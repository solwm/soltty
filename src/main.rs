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

use crate::gpu::{Gpu, DEFAULT_FONT_SIZE_PX};
use crate::pty::Pty;
use crate::term::Term;

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    PtyData,
    PtyExited,
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

    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("soltty: {msg}");
            print_usage();
            std::process::exit(2);
        }
    };
    if cli.help {
        print_usage();
        return;
    }

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
        program: cli
            .program
            .unwrap_or_else(crate::pty::default_shell),
        args: cli.args,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Cli {
    program: Option<String>,
    args: Vec<String>,
    help: bool,
}

impl Cli {
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut out = Cli::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "-e" | "--command" => {
                    out.program = Some(
                        it.next()
                            .ok_or_else(|| format!("{arg} requires a command"))?,
                    );
                    // Convention: -e consumes the rest of the command line, so
                    // `soltty -e zsh -i` does what you'd expect.
                    out.args = it.collect();
                    return Ok(out);
                }
                _ => return Err(format!("unrecognized argument: {arg}")),
            }
        }
        Ok(out)
    }
}

fn print_usage() {
    eprintln!(
        "usage: soltty [OPTIONS]

OPTIONS:
  -e, --command <cmd> [args...]   Run cmd with args instead of $SHELL.
                                  Consumes the rest of the command line.
  -h, --help                      Show this help.

ENV:
  SHELL          Default command when -e is not given (Linux/macOS).
  SOLTTY_FONT    Path to a TTF/OTF to use instead of auto-discovery.
  RUST_LOG       e.g. `soltty=debug` for verbose logging."
    );
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    pty: Option<Pty>,
    term: Term,
    modifiers: ModifiersState,
    program: String,
    args: Vec<String>,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("soltty")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        // glutin needs to pick a GL config alongside the window, so it owns
        // the window creation. We get the Arc<Window> back for input handling.
        let (window, gpu) = Gpu::new(event_loop, attrs);

        let inner = window.inner_size();
        let (rows, cols) = grid_dims_for_window(gpu.cell_size(), inner.width, inner.height);
        let term = Term::new(rows as usize, cols as usize);

        let proxy_data = self.proxy.clone();
        let proxy_exit = self.proxy.clone();
        log::info!("spawn: {} {:?}", self.program, self.args);
        let pty = Pty::spawn(
            rows,
            cols,
            &self.program,
            &self.args,
            move || {
                let _ = proxy_data.send_event(UserEvent::PtyData);
            },
            move || {
                let _ = proxy_exit.send_event(UserEvent::PtyExited);
            },
        )
        .expect("spawn pty");

        log::info!("grid: {cols}x{rows}");
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.pty = Some(pty);
        self.term = term;
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // NVIDIA's EGL-Wayland implementation segfaults inside
        // eglDestroySurface if the wl_display has been torn down before
        // the surface — which is exactly what happens once `run_app`
        // returns. Drop the GL state here, while winit's Wayland
        // connection is still alive.
        self.gpu = None;
        self.window = None;
        self.pty = None;
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
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
            UserEvent::PtyExited => {
                // Shell (or whatever was the foreground command) closed its
                // end of the PTY — there's nothing left to interact with.
                event_loop.exit();
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
                gpu.render(&self.term);
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
                // Font zoom: Ctrl+= / Ctrl++ / Ctrl+- / Ctrl+0. Handled
                // locally; never forwarded to the PTY.
                if let Some(target) = font_zoom_target(&logical_key, self.modifiers, gpu.font_size())
                {
                    let inner = window.inner_size();
                    let cell = gpu.set_font_size(target);
                    let (rows, cols) = grid_dims_for_window(cell, inner.width, inner.height);
                    self.term.resize(rows as usize, cols as usize);
                    if let Some(pty) = self.pty.as_ref() {
                        pty.resize(rows, cols);
                    }
                    window.request_redraw();
                    return;
                }

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

/// Returns the new font size in pixels if `key` is a zoom shortcut, else None.
/// Ctrl+= / Ctrl++ → zoom in (×1.1); Ctrl+- → zoom out (÷1.1); Ctrl+0 → reset.
fn font_zoom_target(key: &Key, mods: ModifiersState, current_px: f32) -> Option<f32> {
    if !mods.control_key() || mods.alt_key() {
        return None;
    }
    let Key::Character(s) = key else { return None };
    match s.as_ref() {
        "=" | "+" => Some(current_px * 1.1),
        "-" => Some(current_px / 1.1),
        "0" => Some(DEFAULT_FONT_SIZE_PX),
        _ => None,
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
mod cli_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        Cli::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn empty_args_means_default_shell() {
        let c = parse(&[]).unwrap();
        assert_eq!(c.program, None);
        assert!(c.args.is_empty());
    }

    #[test]
    fn dash_e_takes_command() {
        let c = parse(&["-e", "zsh"]).unwrap();
        assert_eq!(c.program.as_deref(), Some("zsh"));
        assert!(c.args.is_empty());
    }

    #[test]
    fn dash_e_consumes_rest() {
        let c = parse(&["-e", "fish", "-i", "-l"]).unwrap();
        assert_eq!(c.program.as_deref(), Some("fish"));
        assert_eq!(c.args, vec!["-i".to_string(), "-l".to_string()]);
    }

    #[test]
    fn long_form_command() {
        let c = parse(&["--command", "bash"]).unwrap();
        assert_eq!(c.program.as_deref(), Some("bash"));
    }

    #[test]
    fn dash_e_without_arg_errors() {
        assert!(parse(&["-e"]).is_err());
    }

    #[test]
    fn unknown_flag_errors() {
        assert!(parse(&["--bogus"]).is_err());
    }

    #[test]
    fn help_flag() {
        let c = parse(&["--help"]).unwrap();
        assert!(c.help);
    }
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
