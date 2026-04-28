mod font;
mod gpu;
mod grid;
mod pty;
mod renderer;
mod term;

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::gpu::Gpu;
use crate::pty::Pty;
use crate::term::Term;

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    PtyData,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        // wgpu_hal::vulkan::conv: noisy when compositors advertise present-mode
        // extensions wgpu doesn't recognize yet (e.g. FIFO_LATEST_READY_EXT).
        // sctk_adwaita::config: noisy when the XDG Settings Portal isn't running
        // (custom WMs, minimal Wayland setups).
        "warn,soltty=info,wgpu_hal::vulkan::conv=off,sctk_adwaita::config=off",
    ))
    .init();

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // Term is built lazily in `resumed` once we know the window size.
    let mut app = App {
        proxy,
        window: None,
        gpu: None,
        pty: None,
        term: Term::new(24, 80),
        modifiers: ModifiersState::empty(),
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    pty: Option<Pty>,
    term: Term,
    modifiers: ModifiersState,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("soltty")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = pollster::block_on(Gpu::new(window.clone()));

        let inner = window.inner_size();
        let (rows, cols) = grid_dims_for_window(gpu.cell_size(), inner.width, inner.height);
        let term = Term::new(rows as usize, cols as usize);

        let proxy = self.proxy.clone();
        let pty = Pty::spawn(rows, cols, move || {
            let _ = proxy.send_event(UserEvent::PtyData);
        })
        .expect("spawn pty");

        log::info!("grid: {cols}x{rows}");
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.pty = Some(pty);
        self.term = term;
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData => {
                if let Some(pty) = self.pty.as_ref() {
                    let bytes = pty.drain();
                    if !bytes.is_empty() {
                        self.term.feed(&bytes);
                        if let Some(window) = self.window.as_ref() {
                            window.request_redraw();
                        }
                    }
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let (Some(window), Some(gpu)) = (self.window.as_ref(), self.gpu.as_mut()) else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                gpu.resize(size.width, size.height);
                let (rows, cols) = grid_dims_for_window(gpu.cell_size(), size.width, size.height);
                self.term.resize(rows as usize, cols as usize);
                if let Some(pty) = self.pty.as_ref() {
                    pty.resize(rows, cols);
                }
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
                let (rows, cols) = grid_dims_for_window(gpu.cell_size(), size.width, size.height);
                self.term.resize(rows as usize, cols as usize);
                if let Some(pty) = self.pty.as_ref() {
                    pty.resize(rows, cols);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(err) = gpu.render(&self.term) {
                    log::warn!("render error: {err:?}");
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_x, y) => (y * 3.0) as isize,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 16.0) as isize,
                };
                if lines != 0 {
                    self.term.scroll_view(lines);
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        text,
                        state: ElementState::Pressed,
                        repeat: _,
                        ..
                    },
                ..
            } => {
                if let Some(pty) = self.pty.as_mut() {
                    if let Some(bytes) = encode_key(&logical_key, text.as_deref(), self.modifiers) {
                        // Any keypress that produces output snaps the viewport
                        // back to the live grid — matches what every other
                        // terminal does and is what users expect.
                        self.term.reset_view();
                        pty.write(&bytes);
                        window.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}

fn grid_dims_for_window(cell_size: (u32, u32), w: u32, h: u32) -> (u16, u16) {
    let (cw, ch) = cell_size;
    let cols = (w / cw.max(1)).max(1) as u16;
    let rows = (h / ch.max(1)).max(1) as u16;
    (rows, cols)
}

/// Map a winit key event into the byte sequence a terminal expects.
/// Returns None for keys with no associated output (modifier presses, dead keys).
fn encode_key(logical: &Key, text: Option<&str>, mods: ModifiersState) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    // Ctrl+letter → C0 control byte. Done first because winit (correctly)
    // gives us no `text` for ctrl-modified keys on most platforms.
    if ctrl && !alt {
        if let Some(b) = ctrl_byte(logical) {
            return Some(vec![b]);
        }
    }

    // Named keys with potentially modifier-encoded sequences.
    if let Key::Named(named) = logical {
        if let Some(seq) = named_key_seq(*named, ctrl, alt, shift) {
            return Some(seq);
        }
    }

    // Alt + character → ESC prefix + character (readline meta-* convention).
    if alt {
        if let Some(t) = text {
            let mut v = Vec::with_capacity(1 + t.len());
            v.push(0x1b);
            v.extend_from_slice(t.as_bytes());
            return Some(v);
        }
    }

    // Plain character keys, including shift-modified ones — winit already
    // applied the layout so `text` reflects what should be sent.
    text.map(|t| t.as_bytes().to_vec())
}

/// Encode a Ctrl-modified key into its C0 control byte.
fn ctrl_byte(logical: &Key) -> Option<u8> {
    match logical {
        Key::Character(s) => {
            let c = s.chars().next()?;
            match c.to_ascii_lowercase() {
                'a'..='z' => Some(c.to_ascii_lowercase() as u8 - b'a' + 1),
                ' ' | '@' => Some(0x00),
                '[' => Some(0x1b), // Ctrl+[ ≡ ESC
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' => Some(0x1e),
                '_' | '/' => Some(0x1f),
                '?' => Some(0x7f),
                _ => None,
            }
        }
        Key::Named(NamedKey::Space) => Some(0x00),
        _ => None,
    }
}

/// xterm modifier parameter (1=none, 2=shift, 3=alt, 5=ctrl, 6=ctrl+shift, 7=alt+ctrl, 8=all, …).
fn mod_param(ctrl: bool, alt: bool, shift: bool) -> u8 {
    1 + (shift as u8) + ((alt as u8) << 1) + ((ctrl as u8) << 2)
}

/// `CSI [1;<m>]<letter>` form (arrows, Home, End).
fn csi_letter(letter: u8, m: u8) -> Vec<u8> {
    if m == 1 {
        vec![0x1b, b'[', letter]
    } else {
        format!("\x1b[1;{m}{}", letter as char).into_bytes()
    }
}

/// `CSI <n>[;<m>]~` form (PageUp, PageDown, Insert, Delete, F-keys).
fn csi_tilde(n: u16, m: u8) -> Vec<u8> {
    if m == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{m}~").into_bytes()
    }
}

fn named_key_seq(named: NamedKey, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
    let m = mod_param(ctrl, alt, shift);
    Some(match named {
        NamedKey::Enter => {
            if alt {
                vec![0x1b, b'\r']
            } else {
                vec![b'\r']
            }
        }
        NamedKey::Backspace => {
            // Bash/readline treat 0x7F as erase-prev-char and 0x08 as ctrl-h.
            // Alt+Backspace → ESC + DEL is the readline convention.
            let base = if ctrl { 0x08 } else { 0x7f };
            if alt {
                vec![0x1b, base]
            } else {
                vec![base]
            }
        }
        NamedKey::Tab => {
            if shift {
                b"\x1b[Z".to_vec() // CSI Z = back-tab
            } else if alt {
                vec![0x1b, b'\t']
            } else {
                vec![b'\t']
            }
        }
        NamedKey::Escape => vec![0x1b],
        NamedKey::ArrowUp => csi_letter(b'A', m),
        NamedKey::ArrowDown => csi_letter(b'B', m),
        NamedKey::ArrowRight => csi_letter(b'C', m),
        NamedKey::ArrowLeft => csi_letter(b'D', m),
        NamedKey::Home => csi_letter(b'H', m),
        NamedKey::End => csi_letter(b'F', m),
        NamedKey::PageUp => csi_tilde(5, m),
        NamedKey::PageDown => csi_tilde(6, m),
        NamedKey::Insert => csi_tilde(2, m),
        NamedKey::Delete => csi_tilde(3, m),
        NamedKey::F1 => csi_tilde(11, m),
        NamedKey::F2 => csi_tilde(12, m),
        NamedKey::F3 => csi_tilde(13, m),
        NamedKey::F4 => csi_tilde(14, m),
        NamedKey::F5 => csi_tilde(15, m),
        NamedKey::F6 => csi_tilde(17, m),
        NamedKey::F7 => csi_tilde(18, m),
        NamedKey::F8 => csi_tilde(19, m),
        NamedKey::F9 => csi_tilde(20, m),
        NamedKey::F10 => csi_tilde(21, m),
        NamedKey::F11 => csi_tilde(23, m),
        NamedKey::F12 => csi_tilde(24, m),
        _ => return None,
    })
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn ch(c: &str) -> Key {
        Key::Character(SmolStr::new(c))
    }

    #[test]
    fn ctrl_c_emits_etx() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(encode_key(&ch("c"), None, mods), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_d_emits_eot() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(encode_key(&ch("d"), None, mods), Some(vec![0x04]));
    }

    #[test]
    fn ctrl_open_bracket_is_escape() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(encode_key(&ch("["), None, mods), Some(vec![0x1b]));
    }

    #[test]
    fn alt_b_prefixes_with_escape() {
        let mods = ModifiersState::ALT;
        assert_eq!(encode_key(&ch("b"), Some("b"), mods), Some(vec![0x1b, b'b']));
    }

    #[test]
    fn shift_tab_emits_back_tab() {
        let mods = ModifiersState::SHIFT;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Tab), None, mods),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn shift_arrow_uses_modifier_param() {
        let mods = ModifiersState::SHIFT;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, mods),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn ctrl_arrow_uses_modifier_param() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowRight), None, mods),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn plain_char_passthrough() {
        let mods = ModifiersState::empty();
        assert_eq!(encode_key(&ch("a"), Some("a"), mods), Some(vec![b'a']));
    }
}
