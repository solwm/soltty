mod clipboard;
mod font;
mod gpu;
mod grid;
mod picker;
mod pty;
mod renderer;
mod selection;
mod term;
mod theme;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
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
        // Noisy on configurations we deliberately handle elsewhere:
        //   wgpu_hal::vulkan::conv  — wgpu warns on compositors that
        //     advertise present-mode extensions it doesn't recognize.
        //   sctk_adwaita::config    — XDG portal not running on minimal/
        //     custom Wayland setups.
        //   arboard::platform::linux — arboard logs a WARN when the
        //     compositor lacks wlr-data-control. We have a wl-copy fallback.
        "warn,soltty=info,wgpu_hal::vulkan::conv=off,\
         sctk_adwaita::config=off,arboard::platform::linux=off",
    ))
    .init();

    let cli = Cli::parse();

    let theme_lib = theme::ThemeLib::load();

    if cli.list_themes {
        for name in theme_lib.names() {
            println!("{name}");
        }
        return;
    }

    let initial_theme_name = match cli.theme.as_deref() {
        Some(n) => match theme_lib.find(n) {
            Some(t) => t.name.clone(),
            None => {
                eprintln!(
                    "soltty: theme {n:?} not found. Use --list-themes to see options."
                );
                std::process::exit(2);
            }
        },
        None => theme_lib.default().name.clone(),
    };

    let (program, args) = match cli.command.as_slice() {
        [] => (crate::pty::default_shell(), Vec::new()),
        [prog, rest @ ..] => (prog.clone(), rest.to_vec()),
    };

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
        program,
        args,
        theme_lib,
        active_theme_name: initial_theme_name,
        picker: None,
        mouse_pos: (0.0, 0.0),
        selection: None,
        last_click: None,
        clipboard: clipboard::Clipboard::new(),
        trace: cli.trace,
        redraw_pending: false,
        dirty: false,
        last_render: None,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

/// soltty: a GPU terminal emulator.
///
/// In-app keybinds:
///   Ctrl+Shift+T   Open the theme picker. Up/Down to navigate (with live
///                  preview), Enter to keep, Esc to revert.
///   Ctrl+= / +     Increase font size.
///   Ctrl+-         Decrease font size.
///   Ctrl+0         Reset font size.
///
/// Drop additional themes into ~/.config/soltty/themes/<name>.toml using
/// the iTerm2-Color-Schemes Alacritty TOML format. They override builtins
/// of the same name.
#[derive(Parser, Debug)]
#[command(name = "soltty", version, about, long_about = None)]
struct Cli {
    /// Color theme. Case-insensitive; substring match works.
    #[arg(short = 't', long = "theme", value_name = "NAME")]
    theme: Option<String>,

    /// Print every available theme and exit.
    #[arg(long = "list-themes")]
    list_themes: bool,

    /// Program (and its arguments) to run instead of $SHELL.
    /// Everything after -e is forwarded, e.g. `soltty -e fish -i -l`.
    #[arg(
        short = 'e',
        long = "command",
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD",
    )]
    command: Vec<String>,

    /// Show a small FPS counter in the top-right corner. Useful for
    /// eyeballing render-loop performance under load.
    #[arg(long = "trace")]
    trace: bool,
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
    theme_lib: theme::ThemeLib,
    active_theme_name: String,
    picker: Option<picker::Picker>,
    /// Live mouse cursor position in physical pixels. We keep the latest
    /// from CursorMoved so MouseInput (which doesn't carry a position)
    /// can resolve clicks to cell coords.
    mouse_pos: (f64, f64),
    /// Active selection while the user is dragging or after release
    /// (we keep it visible until they click again).
    selection: Option<selection::Selection>,
    /// Last left-press: time, cell, and how many fast clicks in a row
    /// (1 = single, 2 = double, 3 = triple). Used to detect word/line
    /// select on rapid repeats.
    last_click: Option<(Instant, (usize, usize), u32)>,
    clipboard: clipboard::Clipboard,
    /// `--trace`: draw an FPS counter overlay in the top-right corner.
    trace: bool,
    /// While true, we've called `request_redraw` and the matching
    /// `RedrawRequested` hasn't fired yet. Suppresses extra calls.
    redraw_pending: bool,
    /// Set by PtyData when there's data the user hasn't seen yet. Cleared
    /// by RedrawRequested. `about_to_wait` re-arms a redraw if this is
    /// still set after we've gone quiet (e.g. last byte arrived during
    /// the throttle window).
    dirty: bool,
    /// When the last frame was actually presented. Used to cap redraw rate
    /// at ~60 Hz — without this, under a chatty PTY producer we paint 2-3×
    /// per game frame, and `swap_buffers` (≈200µs each on macOS Metal/GL
    /// even with vsync off) becomes the dominant cost.
    last_render: Option<Instant>,
}

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

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
        let initial_theme = self
            .theme_lib
            .find(&self.active_theme_name)
            .cloned()
            .unwrap_or_else(|| self.theme_lib.default().clone());
        log::info!("theme: {}", initial_theme.name);
        let (window, gpu) = Gpu::new(event_loop, attrs, &initial_theme);

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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // If there's pending data we deferred during the throttle window,
        // either request a redraw now (deadline elapsed) or arm a timer
        // wakeup so we paint exactly when the next slot opens.
        if !self.dirty || self.redraw_pending {
            return;
        }
        match self.last_render {
            None => {
                self.do_request_redraw();
            }
            Some(t) => {
                let next = t + FRAME_INTERVAL;
                if Instant::now() >= next {
                    self.do_request_redraw();
                } else {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(next));
                }
            }
        }
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
                        self.dirty = true;
                        self.maybe_request_redraw();
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
                gpu.render(
                    &self.term,
                    self.picker.as_mut(),
                    self.selection.as_ref(),
                    self.trace,
                );
                self.redraw_pending = false;
                self.dirty = false;
                self.last_render = Some(Instant::now());
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                if let Some(sel) = self.selection.as_mut() {
                    if sel.dragging {
                        let (cw, ch) = gpu.cell_size();
                        // Auto-scroll: if the cursor is dragged above the
                        // top or below the bottom of the window, advance
                        // the viewport so the user can extend the
                        // selection past the visible region.
                        let inner = window.inner_size();
                        if position.y < 0.0 {
                            self.term.scroll_view(1);
                        } else if position.y > inner.height as f64 {
                            self.term.scroll_view(-1);
                        }
                        let cell = pixel_to_cell(self.mouse_pos, (cw, ch), &self.term);
                        if cell != sel.end {
                            sel.end = cell;
                            window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let (cw, ch) = gpu.cell_size();
                let cell = pixel_to_cell(self.mouse_pos, (cw, ch), &self.term);
                match state {
                    ElementState::Pressed => {
                        // Shift+click extends the existing selection.
                        if self.modifiers.shift_key() {
                            if let Some(sel) = self.selection.as_mut() {
                                sel.end = cell;
                                sel.dragging = true;
                                window.request_redraw();
                                return;
                            }
                        }

                        // Multi-click detection. Within 400ms of the last
                        // press at the same cell, escalate to word (2)
                        // then line (3) selection.
                        const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);
                        let now = Instant::now();
                        let count = match self.last_click {
                            Some((t, c, n))
                                if c == cell && now.duration_since(t) <= MULTI_CLICK_WINDOW =>
                            {
                                (n % 3) + 1
                            }
                            _ => 1,
                        };
                        self.last_click = Some((now, cell, count));

                        let mut sel = match count {
                            2 => {
                                let (s, e) = selection::word_bounds(&self.term, cell.0, cell.1);
                                let mut sel = selection::Selection::new(s);
                                sel.end = e;
                                sel
                            }
                            3 => {
                                let (s, e) = selection::line_bounds(&self.term, cell.0);
                                let mut sel = selection::Selection::new(s);
                                sel.end = e;
                                sel
                            }
                            _ => selection::Selection::new(cell),
                        };
                        // Word/line selections are committed; don't keep
                        // dragging in cell-precision after a multi-click.
                        if count > 1 {
                            sel.dragging = false;
                            // Auto-fill clipboard so triple-click → middle
                            // click pastes the whole line.
                            let text = selection::extract_text(&self.term, &sel);
                            if !text.is_empty() {
                                self.clipboard.set_primary(text.clone());
                                self.clipboard.set_text(text);
                            }
                        }
                        self.selection = Some(sel);
                        window.request_redraw();
                    }
                    ElementState::Released => {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.dragging = false;
                            // Auto-copy on mouse-up if there's actually a range
                            // (skip stray clicks). Fill both clipboards so
                            // Ctrl+Shift+V and middle-click both paste it.
                            if !sel.is_empty() {
                                let text = selection::extract_text(&self.term, sel);
                                if !text.is_empty() {
                                    self.clipboard.set_primary(text.clone());
                                    self.clipboard.set_text(text);
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                ..
            } => {
                // Middle-click pastes the primary selection — what most
                // Linux apps do. Falls through to clipboard if there's no
                // primary content.
                let mut text = self.clipboard.get_primary();
                if text.is_empty() {
                    text = self.clipboard.get_text();
                }
                if !text.is_empty() {
                    if let Some(pty) = self.pty.as_mut() {
                        write_paste(pty, &text, self.term.bracketed_paste);
                        self.term.reset_view();
                        self.selection = None;
                        window.request_redraw();
                    }
                }
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
                        physical_key,
                        text,
                        state: ElementState::Pressed,
                        repeat: _,
                        ..
                    },
                ..
            } => {
                log::debug!(
                    "key: logical={logical_key:?} physical={physical_key:?} text={text:?} ctrl={} shift={} alt={}",
                    self.modifiers.control_key(),
                    self.modifiers.shift_key(),
                    self.modifiers.alt_key(),
                );

                // Theme picker: Ctrl+Shift+T opens it. While open, all
                // keystrokes are picker-local — never forwarded to the PTY.
                if self.modifiers.control_key() && self.modifiers.shift_key() {
                    // Match the physical T position OR the layout-aware
                    // "t" character. Either works regardless of keyboard
                    // layout; some platforms also deliver Ctrl-modified
                    // keys as raw C0 bytes (\u{14} = Ctrl+T).
                    let is_t = matches!(physical_key, PhysicalKey::Code(KeyCode::KeyT))
                        || matches!(&logical_key, Key::Character(s) if s.eq_ignore_ascii_case("t"))
                        || matches!(text.as_deref(), Some("\u{14}") | Some("t") | Some("T"));
                    if is_t {
                        if self.picker.is_none() {
                            self.picker = Some(picker::Picker::new(
                                &self.theme_lib,
                                &self.active_theme_name,
                            ));
                            window.request_redraw();
                        }
                        return;
                    }
                }
                if let Some(picker) = self.picker.as_mut() {
                    use winit::keyboard::NamedKey::*;
                    match &logical_key {
                        Key::Named(ArrowUp) => {
                            picker.move_up();
                            apply_picker_preview(gpu, picker, &self.theme_lib);
                        }
                        Key::Named(ArrowDown) => {
                            picker.move_down();
                            apply_picker_preview(gpu, picker, &self.theme_lib);
                        }
                        Key::Named(PageUp) => {
                            picker.page_up(8);
                            apply_picker_preview(gpu, picker, &self.theme_lib);
                        }
                        Key::Named(PageDown) => {
                            picker.page_down(8);
                            apply_picker_preview(gpu, picker, &self.theme_lib);
                        }
                        Key::Named(Home) => {
                            picker.home();
                            apply_picker_preview(gpu, picker, &self.theme_lib);
                        }
                        Key::Named(End) => {
                            picker.end();
                            apply_picker_preview(gpu, picker, &self.theme_lib);
                        }
                        Key::Named(Enter) => {
                            // Commit current selection.
                            self.active_theme_name =
                                picker.current(&self.theme_lib).name.clone();
                            self.picker = None;
                        }
                        Key::Named(Escape) => {
                            // Restore the original theme.
                            let original = picker.cancel(&self.theme_lib).clone();
                            gpu.set_theme(&original);
                            self.active_theme_name = original.name;
                            self.picker = None;
                        }
                        _ => {}
                    }
                    window.request_redraw();
                    return;
                }

                // Ctrl+Shift+V → paste from system clipboard.
                if self.modifiers.control_key() && self.modifiers.shift_key() {
                    let is_v = matches!(physical_key, PhysicalKey::Code(KeyCode::KeyV))
                        || matches!(&logical_key, Key::Character(s) if s.eq_ignore_ascii_case("v"))
                        || matches!(text.as_deref(), Some("\u{16}") | Some("v") | Some("V"));
                    if is_v {
                        let pasted = self.clipboard.get_text();
                        if !pasted.is_empty() {
                            if let Some(pty) = self.pty.as_mut() {
                                write_paste(pty, &pasted, self.term.bracketed_paste);
                                self.term.reset_view();
                                self.selection = None;
                                window.request_redraw();
                            }
                        }
                        return;
                    }
                }

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
                        // Typing also clears the selection.
                        self.selection = None;
                        pty.write(&bytes);
                        window.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}

impl App {
    /// Request a redraw if one isn't already pending and we're past the
    /// frame-interval throttle window. If we're inside the window,
    /// `about_to_wait` will arm a `WaitUntil` so we wake up to paint
    /// exactly when the slot opens.
    fn maybe_request_redraw(&mut self) {
        if self.redraw_pending {
            return;
        }
        let due = match self.last_render {
            None => true,
            Some(t) => Instant::now().duration_since(t) >= FRAME_INTERVAL,
        };
        if due {
            self.do_request_redraw();
        }
    }

    fn do_request_redraw(&mut self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
            self.redraw_pending = true;
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

fn apply_picker_preview(gpu: &mut Gpu, picker: &picker::Picker, lib: &theme::ThemeLib) {
    let theme = picker.current(lib);
    gpu.set_theme(theme);
}

/// Write pasted text to the PTY, optionally wrapped in bracketed-paste
/// markers. Bracketed paste lets the receiving program tell pasted bytes
/// apart from typed input — important for editors that interpret newlines
/// in paste differently from Enter.
fn write_paste(pty: &mut Pty, text: &str, bracketed: bool) {
    if bracketed {
        pty.write(b"\x1b[200~");
        pty.write(text.as_bytes());
        pty.write(b"\x1b[201~");
    } else {
        pty.write(text.as_bytes());
    }
}

/// Convert a window-pixel mouse position into viewport (row, col).
/// Clamped so off-window drags pin to grid edges. We pass `term` only to
/// know the grid bounds — selection lives in viewport coords.
fn pixel_to_cell(pos: (f64, f64), cell: (u32, u32), term: &Term) -> (usize, usize) {
    let (cw, ch) = (cell.0.max(1) as f64, cell.1.max(1) as f64);
    let col = (pos.0 / cw).max(0.0) as usize;
    let row = (pos.1 / ch).max(0.0) as usize;
    let g = term.grid();
    (
        row.min(g.rows.saturating_sub(1)),
        col.min(g.cols.saturating_sub(1)),
    )
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
