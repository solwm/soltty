mod clipboard;
mod config;
#[cfg(unix)]
mod clipboard_wayland;
mod font;
mod gpu;
mod grid;
mod picker;
mod pty;
mod renderer;
mod selection;
mod term;
mod theme;
mod vi;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::gpu::{Gpu, DEFAULT_FONT_SIZE_PX};
use crate::grid::CursorShape;
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
    let cfg = config::Config::load();

    let theme_lib = theme::ThemeLib::load();

    if cli.list_themes {
        for name in theme_lib.names() {
            println!("{name}");
        }
        return;
    }

    // Theme precedence: CLI flag > config file > built-in default. CLI
    // misses are fatal (the user explicitly asked for it); config-file
    // misses warn and fall back so a typo in the config doesn't take
    // the terminal down.
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
        None => match cfg.theme.as_deref().and_then(|n| theme_lib.find(n)) {
            Some(t) => t.name.clone(),
            None => {
                if let Some(n) = cfg.theme.as_deref() {
                    log::warn!("config theme {n:?} not found, using default");
                }
                theme_lib.default().name.clone()
            }
        },
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
        redraw_pending: false,
        dirty: false,
        last_render: None,
        // Real value is set in `resumed` once we have a window to ask.
        frame_interval: Duration::from_millis(8),
        blink_epoch: Instant::now(),
        vi: vi::ViMode::new(),
        vi_keybind: vi::ViKeyBind::resolve(cfg.vi_keybind.as_deref()),
        held_buttons: 0,
        last_reported_cell: None,
        burst_holdoff: 0,
        config: cfg,
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
    /// While true, we've called `request_redraw` and the matching
    /// `RedrawRequested` hasn't fired yet. Suppresses extra calls.
    redraw_pending: bool,
    /// Set by PtyData when there's data the user hasn't seen yet. Cleared
    /// by RedrawRequested. `about_to_wait` re-arms a redraw if this is
    /// still set after we've gone quiet (e.g. last byte arrived during
    /// the throttle window).
    dirty: bool,
    /// When the last frame was actually presented. Used to cap redraw
    /// rate so a chatty PTY producer can't make `swap_buffers` (≈200µs
    /// each on macOS Metal/GL even with vsync off) the dominant cost.
    last_render: Option<Instant>,
    /// Minimum interval between paints, derived from the window's
    /// monitor refresh rate at startup (clamped to 60-240 Hz). Set in
    /// `resumed`; the `Default::default()` here only covers the
    /// vanishingly small window before that.
    frame_interval: Duration,
    /// Reference time for the cursor blink phase. Reset whenever the user
    /// types so the cursor flashes on right after activity instead of
    /// continuing whatever stale phase it was in.
    blink_epoch: Instant,
    vi: vi::ViMode,
    /// Activation key for vi mode. Read from `SOLTTY_VI_KEY` at startup,
    /// defaults to Ctrl+Shift+Space.
    vi_keybind: vi::ViKeyBind,
    /// Bit field of currently-held mouse buttons (bit 0 = left, 1 =
    /// middle, 2 = right). Tracked here because winit only tells us
    /// individual press/release events, but mouse-reporting modes
    /// 1002/1003 need to know whether *any* button is held while
    /// motion events fire (to attach the right button code).
    held_buttons: u8,
    /// Last cell we reported a motion event for. Mouse motion is
    /// pixel-granular but the protocol is cell-granular; without dedup
    /// a single drag floods the PTY with redundant events.
    last_reported_cell: Option<(usize, usize)>,
    /// Counts down each render. When > 0, the redraw deadline is
    /// stretched so a continuous PTY burst (gol-c, `cat`-of-large-
    /// file, log dumps) renders the *result* of the burst instead of
    /// every intermediate frame. Set when a single drain returns more
    /// than `BURST_BYTES_THRESHOLD` bytes; decays one step per render
    /// so the moment the burst stops we're back to full refresh rate
    /// for typing/echo. Typing produces tiny drains (one keypress
    /// echo) and never triggers this path.
    burst_holdoff: u32,
    /// Loaded user config. Consulted in `resumed` for font / frame
    /// settings that we don't know how to apply until the window
    /// exists. Other settings (theme, vi keybind) are already resolved
    /// in `main` and stored as concrete values.
    config: config::Config,
}

/// Even with the per-display cap, we always paint immediately if we've
/// been quiet for this long. That makes typing-after-idle feel
/// instant on a 60 Hz panel where the cap would otherwise add up to
/// ~16 ms of echo latency on the first keystroke.
const IDLE_THRESHOLD: Duration = Duration::from_millis(100);

const MIN_FRAME_HZ: u32 = 60;
const MAX_FRAME_HZ: u32 = 240;
/// Used when both the env override and winit's monitor query come up
/// empty. 120 is a safer default than 60 — modern panels are mostly
/// faster than that and the cost of capping high is negligible.
const DEFAULT_FRAME_HZ: u32 = 120;

/// Half a blink period — cursor is on for this long, then off for the same.
/// 500 ms gives a 1 Hz cycle, the xterm/Alacritty default. Block-shape
/// cursors don't blink at all (see `App::cursor_blinks`); only Bar and
/// Underline do, which corresponds to insert/replace mode in vim and the
/// active prompt cursor in zsh-vi-mode.
const BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);

/// A single PTY drain larger than this many bytes is taken as evidence
/// that the child is producing a sustained burst (gol-c, `cat` of a
/// large file, build-tool log dump). Small drains — single keystroke
/// echo, shell prompt repaint — sit well below this threshold and
/// stay on the fast-refresh path.
const BURST_BYTES_THRESHOLD: usize = 4096;
/// During a burst we stretch the redraw deadline by this much per
/// render until the burst ends. 1 ≈ keep current cap; 2 ≈ render at
/// half rate. The bench shows the gol-c gap to alacritty is per-frame
/// cost; halving render frequency during the burst converges on the
/// 60-Hz numbers (where we're at ~0.89x parity) while typing keeps
/// the full refresh rate.
const BURST_FRAME_MULTIPLIER: u32 = 2;
/// How many renders we stay in burst mode for, after the last big
/// drain. Keeps us coasting through the burst without thrashing back
/// and forth on a single momentarily-empty drain.
const BURST_HOLDOFF_FRAMES: u32 = 6;

/// Pick the redraw cap. Precedence: `SOLTTY_FRAME_HZ` env var > config
/// `frame_hz` > monitor refresh rate from winit > 120 Hz default.
/// Result is clamped so a misconfigured source can't pin us at 5 Hz or
/// 1 kHz. A config value of 0 means "auto" — explicitly fall through
/// to the monitor query.
fn detect_frame_interval(window: &Window, config_hz: Option<u32>) -> Duration {
    let env_hz = std::env::var("SOLTTY_FRAME_HZ")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    let cfg_hz = config_hz.filter(|&h| h > 0);
    let hz = env_hz
        .or(cfg_hz)
        .or_else(|| {
            window
                .current_monitor()
                .and_then(|m| m.refresh_rate_millihertz())
                .map(|mhz| (mhz + 500) / 1000) // round to nearest Hz
        })
        .unwrap_or(DEFAULT_FRAME_HZ)
        .clamp(MIN_FRAME_HZ, MAX_FRAME_HZ);
    let interval = Duration::from_secs_f64(1.0 / hz as f64);
    log::info!(
        "frame cap: {hz} Hz ({:.2} ms)",
        interval.as_secs_f64() * 1000.0
    );
    interval
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
        let initial_theme = self
            .theme_lib
            .find(&self.active_theme_name)
            .cloned()
            .unwrap_or_else(|| self.theme_lib.default().clone());
        log::info!("theme: {}", initial_theme.name);
        let (window, gpu) = Gpu::new(event_loop, attrs, &initial_theme, &self.config);

        let inner = window.inner_size();
        let (rows, cols) = grid_dims_for_window(gpu.cell_size(), inner.width, inner.height);
        let mut term = Term::new(rows as usize, cols as usize);
        // Seed the OSC-color cache so OSC 10/11/12 queries from a
        // freshly-spawned shell get accurate values on the very first
        // exchange.
        term.set_theme_colors(
            initial_theme.fg,
            initial_theme.bg,
            initial_theme.cursor_bg,
        );

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
        self.frame_interval = detect_frame_interval(&window, self.config.frame_hz);
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.pty = Some(pty);
        self.term = term;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let blinking = self.cursor_blinks();
        // Pre-screen: if there's nothing dirty and we're not blinking,
        // there's no work to schedule. Skip the clock_gettime entirely
        // — `about_to_wait` fires after every winit poll, and during a
        // chatty PTY drain that's tens of thousands of times per
        // bench. Adds up.
        if !self.dirty && !blinking {
            return;
        }
        // Second early-out: dirty data already has a redraw on the
        // way and no blink animation needs scheduling. The body below
        // would only walk through the match arms and bail anyway,
        // but it'd cost a clock_gettime first. During a heavy PTY
        // drain (gol-c, cat'ing a big file) this case dominates.
        if self.redraw_pending && !blinking {
            return;
        }

        let now = Instant::now();

        // If the cursor is in a blinking shape and the phase boundary has
        // passed since our last paint, mark dirty so the throttle path
        // below repaints to flip the cursor on/off.
        if blinking && !self.redraw_pending {
            if let Some(last) = self.last_render {
                if self.cursor_visible_now(last) != self.cursor_visible_now(now) {
                    self.dirty = true;
                }
            }
        }

        // If there's pending data we deferred during the throttle window,
        // either request a redraw now (deadline elapsed) or arm a timer
        // wakeup so we paint exactly when the next slot opens.
        if !self.dirty || self.redraw_pending {
            // Nothing to paint right now, but if the cursor is blinking
            // we still need to wake at the next phase boundary so the
            // toggle isn't delayed indefinitely waiting for input.
            if !self.redraw_pending && blinking {
                event_loop
                    .set_control_flow(ControlFlow::WaitUntil(self.next_blink_toggle(now)));
            }
            return;
        }
        // Same burst-aware deadline as `maybe_request_redraw`. The
        // two paths must agree; otherwise about_to_wait could trip a
        // redraw `maybe_request_redraw` had postponed (or vice
        // versa), and the burst throttle would leak frames.
        let interval = if self.burst_holdoff > 0 {
            self.frame_interval * BURST_FRAME_MULTIPLIER
        } else {
            self.frame_interval
        };
        match self.last_render {
            None => {
                self.do_request_redraw();
            }
            Some(t) => {
                let elapsed = now.duration_since(t);
                if elapsed >= interval || elapsed >= IDLE_THRESHOLD {
                    self.do_request_redraw();
                } else {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(t + interval));
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
                if let Some(pty) = self.pty.as_mut() {
                    let bytes = pty.drain();
                    if !bytes.is_empty() {
                        // A drain larger than the threshold means the
                        // child is in burst mode — stretch the next
                        // few render deadlines so we don't paint every
                        // intermediate state. Typing produces tiny
                        // drains (single-key echo) and never trips
                        // this; gol-c/`cat`-of-large-file always do.
                        if bytes.len() >= BURST_BYTES_THRESHOLD {
                            self.burst_holdoff = BURST_HOLDOFF_FRAMES;
                        }
                        self.term.feed(&bytes);
                        // Parser may have produced reply bytes (DSR
                        // cursor-position reports etc.) — write them
                        // back to the PTY now so the application sees
                        // the round-trip before its next read.
                        let reply = self.term.take_reply();
                        if !reply.is_empty() {
                            pty.write(&reply);
                        }
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
                let now = Instant::now();
                // Inline `cursor_visible_now` here — calling a `&self`
                // method while the destructuring at the top of this fn
                // holds `&mut self.gpu` trips the borrow checker, even
                // though the fields we touch are disjoint from gpu. Going
                // through field access directly lets disjoint-borrow do
                // its job.
                let cursor_visible = match self.term.cursor_shape() {
                    CursorShape::Block | CursorShape::HollowBlock => true,
                    CursorShape::Bar | CursorShape::Underline => {
                        let elapsed = now.duration_since(self.blink_epoch);
                        let half_ms = BLINK_HALF_PERIOD.as_millis();
                        (elapsed.as_millis() / half_ms) % 2 == 0
                    }
                };
                let vi_cursor = if self.vi.active {
                    Some(self.vi.cursor)
                } else {
                    None
                };
                // Build the renderer's search-overlay view from vi state.
                // Only matches inside the current viewport are listed —
                // off-screen ones get skipped, since the renderer matches
                // by viewport row.
                let search_overlay = self.vi.search.as_ref().map(|s| {
                    let g = self.term.grid();
                    let sb_len = g.scrollback.len();
                    let top_abs = sb_len.saturating_sub(self.term.viewport_offset);
                    let view_rows = g.rows;
                    let mut visible = Vec::new();
                    let mut current_visible = None;
                    for (i, m) in s.matches.iter().enumerate() {
                        if m.row >= top_abs && m.row < top_abs + view_rows {
                            if Some(i) == s.current {
                                current_visible = Some(visible.len());
                            }
                            visible.push(renderer::MatchView {
                                vrow: m.row - top_abs,
                                col_start: m.col_start,
                                col_end: m.col_end,
                            });
                        }
                    }
                    let prompt = match s.direction {
                        vi::SearchDirection::Forward => '/',
                        vi::SearchDirection::Backward => '?',
                    };
                    renderer::SearchOverlay {
                        prompt,
                        query: &s.query,
                        typing: s.typing,
                        matches: visible,
                        current: current_visible,
                    }
                });
                gpu.render(
                    &self.term,
                    self.picker.as_mut(),
                    self.selection.as_ref(),
                    cursor_visible,
                    vi_cursor,
                    self.vi.active,
                    search_overlay.as_ref(),
                );
                self.redraw_pending = false;
                self.dirty = false;
                self.last_render = Some(now);
                // Decay burst-mode one step per render. Once it hits
                // 0 we're back to full refresh; another big drain
                // re-arms it.
                self.burst_holdoff = self.burst_holdoff.saturating_sub(1);
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                let mode = self.term.mouse_mode;
                let shift = self.modifiers.shift_key();
                // Mouse-reporting forward path. Shift held = bypass
                // (matches alacritty/xterm: hold Shift to escape an
                // app's mouse grab and select text manually).
                if mode != term::MouseMode::None && !shift {
                    let want_motion = match mode {
                        term::MouseMode::None => false,
                        term::MouseMode::ButtonEvent => false,
                        term::MouseMode::ButtonAndMotion => self.held_buttons != 0,
                        term::MouseMode::AllMotion => true,
                    };
                    if want_motion {
                        let cell = pixel_to_cell(self.mouse_pos, gpu.cell_size(), &self.term);
                        // Drop pixel-granular motion that didn't change
                        // the cell — keeps a fast drag from drowning
                        // the PTY in identical events.
                        if self.last_reported_cell != Some(cell) {
                            self.last_reported_cell = Some(cell);
                            let button = if self.held_buttons & 0b001 != 0 {
                                term::MouseButton::Left
                            } else if self.held_buttons & 0b010 != 0 {
                                term::MouseButton::Middle
                            } else if self.held_buttons & 0b100 != 0 {
                                term::MouseButton::Right
                            } else {
                                term::MouseButton::None
                            };
                            let event = term::MouseEvent {
                                button,
                                action: term::MouseAction::Motion,
                                col: (cell.1 + 1) as u16,
                                row: (cell.0 + 1) as u16,
                                shift,
                                alt: self.modifiers.alt_key(),
                                ctrl: self.modifiers.control_key(),
                            };
                            if let Some(pty) = self.pty.as_mut() {
                                pty.write(&term::encode_mouse(event, self.term.mouse_encoding));
                            }
                        }
                    }
                    return;
                }
                // Local selection path (existing behavior).
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
                let mode = self.term.mouse_mode;
                let shift = self.modifiers.shift_key();
                if mode != term::MouseMode::None && !shift {
                    // Update held mask first so motion handlers know
                    // which button to attach to subsequent drag events.
                    match state {
                        ElementState::Pressed => self.held_buttons |= 0b001,
                        ElementState::Released => self.held_buttons &= !0b001,
                    }
                    let cell = pixel_to_cell(self.mouse_pos, gpu.cell_size(), &self.term);
                    let event = term::MouseEvent {
                        button: term::MouseButton::Left,
                        action: match state {
                            ElementState::Pressed => term::MouseAction::Press,
                            ElementState::Released => term::MouseAction::Release,
                        },
                        col: (cell.1 + 1) as u16,
                        row: (cell.0 + 1) as u16,
                        shift,
                        alt: self.modifiers.alt_key(),
                        ctrl: self.modifiers.control_key(),
                    };
                    if let Some(pty) = self.pty.as_mut() {
                        pty.write(&term::encode_mouse(event, self.term.mouse_encoding));
                    }
                    return;
                }
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
                state,
                button: MouseButton::Middle,
                ..
            } => {
                let mode = self.term.mouse_mode;
                let shift = self.modifiers.shift_key();
                if mode != term::MouseMode::None && !shift {
                    match state {
                        ElementState::Pressed => self.held_buttons |= 0b010,
                        ElementState::Released => self.held_buttons &= !0b010,
                    }
                    let cell = pixel_to_cell(self.mouse_pos, gpu.cell_size(), &self.term);
                    let event = term::MouseEvent {
                        button: term::MouseButton::Middle,
                        action: match state {
                            ElementState::Pressed => term::MouseAction::Press,
                            ElementState::Released => term::MouseAction::Release,
                        },
                        col: (cell.1 + 1) as u16,
                        row: (cell.0 + 1) as u16,
                        shift,
                        alt: self.modifiers.alt_key(),
                        ctrl: self.modifiers.control_key(),
                    };
                    if let Some(pty) = self.pty.as_mut() {
                        pty.write(&term::encode_mouse(event, self.term.mouse_encoding));
                    }
                    return;
                }
                // Only the press triggers paste; ignore release in local
                // mode (matches the original behavior).
                if state != ElementState::Pressed {
                    return;
                }
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
                let mode = self.term.mouse_mode;
                let shift = self.modifiers.shift_key();
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_x, y) => (y * 3.0) as isize,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 16.0) as isize,
                };
                if mode != term::MouseMode::None && !shift && lines != 0 {
                    let cell = pixel_to_cell(self.mouse_pos, gpu.cell_size(), &self.term);
                    let button = if lines > 0 {
                        term::MouseButton::WheelUp
                    } else {
                        term::MouseButton::WheelDown
                    };
                    let count = lines.unsigned_abs();
                    if let Some(pty) = self.pty.as_mut() {
                        // One event per line of scroll. Apps that
                        // expect "tick events" map each one to their
                        // own scroll amount.
                        for _ in 0..count {
                            let event = term::MouseEvent {
                                button,
                                action: term::MouseAction::Press,
                                col: (cell.1 + 1) as u16,
                                row: (cell.0 + 1) as u16,
                                shift,
                                alt: self.modifiers.alt_key(),
                                ctrl: self.modifiers.control_key(),
                            };
                            pty.write(&term::encode_mouse(event, self.term.mouse_encoding));
                        }
                    }
                    return;
                }
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
                            apply_picker_preview(gpu, &mut self.term, picker, &self.theme_lib);
                        }
                        Key::Named(ArrowDown) => {
                            picker.move_down();
                            apply_picker_preview(gpu, &mut self.term, picker, &self.theme_lib);
                        }
                        Key::Named(PageUp) => {
                            picker.page_up(8);
                            apply_picker_preview(gpu, &mut self.term, picker, &self.theme_lib);
                        }
                        Key::Named(PageDown) => {
                            picker.page_down(8);
                            apply_picker_preview(gpu, &mut self.term, picker, &self.theme_lib);
                        }
                        Key::Named(Home) => {
                            picker.home();
                            apply_picker_preview(gpu, &mut self.term, picker, &self.theme_lib);
                        }
                        Key::Named(End) => {
                            picker.end();
                            apply_picker_preview(gpu, &mut self.term, picker, &self.theme_lib);
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
                            self.term.set_theme_colors(
                                original.fg,
                                original.bg,
                                original.cursor_bg,
                            );
                            self.active_theme_name = original.name;
                            self.picker = None;
                        }
                        _ => {}
                    }
                    window.request_redraw();
                    return;
                }

                // Vi mode: activation. Once active, the keypress that
                // triggered entry is consumed locally — never sent to PTY.
                if !self.vi.active && self.vi_keybind.matches(&logical_key, self.modifiers) {
                    let start = self.term.viewport_cursor().unwrap_or((0, 0));
                    self.vi.enter(start);
                    self.selection = None;
                    window.request_redraw();
                    return;
                }

                // Vi mode: while active, all keys go to the vi handler and
                // nothing reaches the PTY. Esc cancels pending count/op
                // first, then exits visual, then exits vi-mode entirely.
                if self.vi.active {
                    // Search-typing has its own input rules: every key
                    // edits the query, navigates it, or commits/cancels.
                    // Vi motions are inert until commit.
                    let typing = self
                        .vi
                        .search
                        .as_ref()
                        .map(|s| s.typing)
                        .unwrap_or(false);
                    if typing {
                        let cols = self.term.grid().cols;
                        match &logical_key {
                            Key::Named(NamedKey::Escape) => {
                                cancel_search(&mut self.vi, &mut self.term);
                            }
                            Key::Named(NamedKey::Enter) => {
                                if let Some(s) = self.vi.search.as_mut() {
                                    s.typing = false;
                                }
                            }
                            Key::Named(NamedKey::Backspace) => {
                                if let Some(s) = self.vi.search.as_mut() {
                                    s.query.pop();
                                }
                                refresh_search(&mut self.vi, &mut self.term);
                            }
                            Key::Character(text) if !self.modifiers.control_key() => {
                                if let Some(c) = text.chars().next() {
                                    if !c.is_control() {
                                        if let Some(s) = self.vi.search.as_mut() {
                                            s.query.push(c);
                                        }
                                        refresh_search(&mut self.vi, &mut self.term);
                                    }
                                }
                            }
                            _ => {}
                        }
                        if self.vi.visual != vi::VisualMode::None {
                            self.selection = self.vi.selection(cols);
                        }
                        window.request_redraw();
                        return;
                    }

                    let cols = self.term.grid().cols;
                    let shift = self.modifiers.shift_key();
                    let ctrl = self.modifiers.control_key();
                    let mut motion: Option<vi::Motion> = None;

                    match &logical_key {
                        Key::Named(NamedKey::Escape) => {
                            if self.vi.pending_count.is_some() || self.vi.pending_op.is_some() {
                                self.vi.cancel_pending();
                            } else if self.vi.search.is_some() {
                                // Dismiss highlighted matches; the next
                                // Esc will exit visual / vi-mode.
                                self.vi.search = None;
                            } else if self.vi.visual != vi::VisualMode::None {
                                self.vi.exit_visual();
                                self.selection = None;
                            } else {
                                self.vi.exit();
                                self.selection = None;
                            }
                        }
                        Key::Character(s) => {
                            let raw = s.chars().next().unwrap_or('\0');
                            let ch = raw.to_ascii_lowercase();

                            // Ctrl-modified letters: page motions and
                            // block-visual. Pulled out before the
                            // digit/letter split so 'd'/'u'/'v' etc.
                            // don't get swallowed by the regular dispatch.
                            let mut ctrl_handled = false;
                            let page_motion = if ctrl {
                                match ch {
                                    'd' => Some(vi::Motion::HalfPageDown),
                                    'u' => Some(vi::Motion::HalfPageUp),
                                    'f' => Some(vi::Motion::FullPageDown),
                                    'b' => Some(vi::Motion::FullPageUp),
                                    'v' => {
                                        self.vi.start_visual_block();
                                        ctrl_handled = true;
                                        None
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };

                            // Operator-pending takes priority over normal
                            // dispatch. After `y`, the next key is
                            // interpreted as a yank target (motion, `yy`,
                            // `yi<obj>`, `ya<obj>`, count digit, or
                            // cancel). Same shape for the text-object
                            // sub-states.
                            let yank_pending = matches!(
                                self.vi.pending_op,
                                Some(vi::PendingOp::Yank { .. })
                            );
                            let inner_pending =
                                self.vi.pending_op == Some(vi::PendingOp::YankInner);
                            let around_pending =
                                self.vi.pending_op == Some(vi::PendingOp::YankAround);

                            if ctrl_handled {
                                // Block-visual already activated; nothing
                                // else to dispatch this keystroke.
                            } else if yank_pending {
                                // Pull start_abs out of the variant; clear
                                // pending_op now and re-set it only for
                                // states that should persist (count digit
                                // or `i`/`a` text-object prefix).
                                let start_abs = match self.vi.pending_op {
                                    Some(vi::PendingOp::Yank { start_abs }) => start_abs,
                                    _ => unreachable!(),
                                };
                                self.vi.pending_op = None;

                                if ch.is_ascii_digit()
                                    && (ch != '0' || self.vi.pending_count.is_some())
                                {
                                    self.vi.push_digit(ch.to_digit(10).unwrap());
                                    self.vi.pending_op =
                                        Some(vi::PendingOp::Yank { start_abs });
                                } else if (ch == 'y' && !shift) || (ch == 'y' && shift) {
                                    // yy or Y: yank N current/next lines.
                                    let count = self.vi.take_count();
                                    let line_start = (start_abs.0, 0);
                                    let line_end = (
                                        start_abs.0.saturating_add(
                                            count.saturating_sub(1) as usize,
                                        ),
                                        cols.saturating_sub(1),
                                    );
                                    yank_range_to_clipboard(
                                        &mut self.clipboard,
                                        &self.term,
                                        line_start,
                                        line_end,
                                    );
                                    self.vi.exit();
                                    self.selection = None;
                                } else if ch == 'i' && !shift {
                                    self.vi.pending_op = Some(vi::PendingOp::YankInner);
                                } else if ch == 'a' && !shift {
                                    self.vi.pending_op = Some(vi::PendingOp::YankAround);
                                } else if let Some(m) = key_to_motion(ch, shift, ctrl) {
                                    let n = self.vi.take_count();
                                    vi::apply_motion(
                                        &mut self.vi,
                                        &mut self.term,
                                        m,
                                        n,
                                    );
                                    let end_abs = vi::cursor_to_absolute(
                                        &self.term,
                                        self.vi.cursor,
                                    );
                                    yank_range_to_clipboard(
                                        &mut self.clipboard,
                                        &self.term,
                                        start_abs,
                                        end_abs,
                                    );
                                    self.vi.exit();
                                    self.selection = None;
                                }
                                // else: invalid follow-up, silently
                                // cancelled by the pending_op = None above.
                            } else if inner_pending || around_pending {
                                self.vi.pending_op = None;
                                if ch == 'w' {
                                    let big = shift;
                                    let bounds = if inner_pending {
                                        vi::inner_word_bounds(
                                            &self.term,
                                            self.vi.cursor,
                                            big,
                                        )
                                    } else {
                                        vi::around_word_bounds(
                                            &self.term,
                                            self.vi.cursor,
                                            big,
                                        )
                                    };
                                    let s_abs = vi::cursor_to_absolute(&self.term, bounds.0);
                                    let e_abs = vi::cursor_to_absolute(&self.term, bounds.1);
                                    yank_range_to_clipboard(
                                        &mut self.clipboard,
                                        &self.term,
                                        s_abs,
                                        e_abs,
                                    );
                                    self.vi.exit();
                                    self.selection = None;
                                }
                                // else: cancel (already cleared above)
                            } else if let Some(m) = page_motion {
                                motion = Some(m);
                            } else if ch.is_ascii_digit()
                                && (ch != '0' || self.vi.pending_count.is_some())
                            {
                                // Digit: append to count. Bare leading
                                // `0` is a motion (LineStart) rather than
                                // a digit, matching vim — falls through
                                // to the `('0', _)` arm below.
                                self.vi.push_digit(ch.to_digit(10).unwrap());
                            } else if self.vi.pending_op == Some(vi::PendingOp::Goto) {
                                // Mid-`gg` sequence. Only `g` completes;
                                // any other key cancels the pending op
                                // (and doesn't do its usual thing — vim
                                // throws away the half-typed sequence).
                                self.vi.pending_op = None;
                                if ch == 'g' {
                                    motion = Some(vi::Motion::DocStart);
                                }
                            } else {
                                match (ch, shift) {
                                    // h/j/k/l — char motion (no shift).
                                    ('h', false) => motion = Some(vi::Motion::Char(0, -1)),
                                    ('j', false) => motion = Some(vi::Motion::Char(1, 0)),
                                    ('k', false) => motion = Some(vi::Motion::Char(-1, 0)),
                                    ('l', false) => motion = Some(vi::Motion::Char(0, 1)),
                                    // H/M/L — screen positions (shift).
                                    ('h', true) => motion = Some(vi::Motion::ScreenTop),
                                    ('m', true) => motion = Some(vi::Motion::ScreenMiddle),
                                    ('l', true) => motion = Some(vi::Motion::ScreenBottom),
                                    // gg / G — document ends.
                                    ('g', false) => {
                                        self.vi.pending_op = Some(vi::PendingOp::Goto)
                                    }
                                    ('g', true) => motion = Some(vi::Motion::DocEnd),
                                    // Word motions: lowercase = word,
                                    // uppercase (shift) = WORD.
                                    ('w', false) => {
                                        motion = Some(vi::Motion::WordNext { big: false })
                                    }
                                    ('w', true) => {
                                        motion = Some(vi::Motion::WordNext { big: true })
                                    }
                                    ('e', false) => {
                                        motion = Some(vi::Motion::WordEnd { big: false })
                                    }
                                    ('e', true) => {
                                        motion = Some(vi::Motion::WordEnd { big: true })
                                    }
                                    ('b', false) => {
                                        motion = Some(vi::Motion::WordPrev { big: false })
                                    }
                                    ('b', true) => {
                                        motion = Some(vi::Motion::WordPrev { big: true })
                                    }
                                    // Line motions. `0`/`^`/`$` arrive
                                    // with whatever shift state the user
                                    // had to type them; we don't care.
                                    ('0', _) => motion = Some(vi::Motion::LineStart),
                                    ('^', _) => motion = Some(vi::Motion::LineFirstNonBlank),
                                    ('$', _) => motion = Some(vi::Motion::LineEnd),
                                    // Visual mode entries.
                                    ('v', false) => self.vi.start_visual_char(),
                                    ('v', true) => self.vi.start_visual_line(),
                                    // Yank. In visual mode: copy the
                                    // selection and exit (alacritty
                                    // semantics). In normal mode: set
                                    // operator-pending; the next key
                                    // (motion / yy / yi<obj> / ya<obj>)
                                    // determines the range.
                                    ('y', _) => {
                                        if self.vi.visual != vi::VisualMode::None {
                                            if let Some(sel) = self.selection.as_ref() {
                                                let text = selection::extract_text(
                                                    &self.term,
                                                    sel,
                                                );
                                                if !text.is_empty() {
                                                    self.clipboard
                                                        .set_primary(text.clone());
                                                    self.clipboard.set_text(text);
                                                }
                                            }
                                            self.vi.exit();
                                            self.selection = None;
                                        } else {
                                            let start_abs = vi::cursor_to_absolute(
                                                &self.term,
                                                self.vi.cursor,
                                            );
                                            self.vi.pending_op =
                                                Some(vi::PendingOp::Yank { start_abs });
                                        }
                                    }
                                    // Search.
                                    ('/', _) => start_search(
                                        &mut self.vi,
                                        &self.term,
                                        vi::SearchDirection::Forward,
                                    ),
                                    ('?', _) => start_search(
                                        &mut self.vi,
                                        &self.term,
                                        vi::SearchDirection::Backward,
                                    ),
                                    ('n', false) => {
                                        search_navigate(&mut self.vi, &mut self.term, false)
                                    }
                                    ('n', true) => {
                                        search_navigate(&mut self.vi, &mut self.term, true)
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }

                    // Apply the chosen motion (if any), consuming the count.
                    if let Some(m) = motion {
                        let n = self.vi.take_count();
                        vi::apply_motion(&mut self.vi, &mut self.term, m, n);
                    }
                    // Refresh selection from the latest vi state if we're
                    // still in a visual mode (covers `v`/`V` plus any
                    // motion that extended the range).
                    if self.vi.active && self.vi.visual != vi::VisualMode::None {
                        self.selection = self.vi.selection(cols);
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
                    if let Some(bytes) = encode_key(
                        &logical_key,
                        text.as_deref(),
                        self.modifiers,
                        self.term.application_cursor_keys,
                    ) {
                        // Any keypress that produces output snaps the viewport
                        // back to the live grid — matches what every other
                        // terminal does and is what users expect.
                        self.term.reset_view();
                        // Typing also clears the selection.
                        self.selection = None;
                        // Reset the blink phase so the cursor is solidly on
                        // for the next half-period after the user typed
                        // something. Without this, fast typing through an
                        // off-phase looks like the cursor is missing.
                        self.blink_epoch = Instant::now();
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
    /// frame-interval throttle window. Inside the window, `about_to_wait`
    /// arms a `WaitUntil` so we paint exactly when the slot opens.
    ///
    /// Idle exit: a burst arriving more than `IDLE_THRESHOLD` after the
    /// last paint paints immediately regardless of the cap. Logically
    /// redundant with `elapsed >= frame_interval` at our normal caps,
    /// but documents the intent and keeps echo latency bounded if the
    /// cap is ever configured high.
    fn maybe_request_redraw(&mut self) {
        if self.redraw_pending {
            return;
        }
        // During a sustained burst (set in user_event PtyData) we
        // stretch the deadline so the burst's intermediate frames
        // don't all hit the GPU. IDLE_THRESHOLD still wins as a
        // ceiling so the user never waits more than 100 ms to see
        // the latest state.
        let interval = if self.burst_holdoff > 0 {
            self.frame_interval * BURST_FRAME_MULTIPLIER
        } else {
            self.frame_interval
        };
        let due = match self.last_render {
            None => true,
            Some(t) => {
                let elapsed = Instant::now().duration_since(t);
                elapsed >= interval || elapsed >= IDLE_THRESHOLD
            }
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

    /// True when the active grid's cursor shape is one we blink. Block
    /// stays steady (matches alacritty's default and avoids flicker
    /// while reading in normal/visual mode); Bar and Underline blink at
    /// `BLINK_HALF_PERIOD * 2`.
    fn cursor_blinks(&self) -> bool {
        match self.term.cursor_shape() {
            // HollowBlock is internal (vi-mode); apps can't request it,
            // and we don't blink it for the same reason as Block — vi
            // mode is "reading" mode, flicker would be distracting.
            CursorShape::Block | CursorShape::HollowBlock => false,
            CursorShape::Bar | CursorShape::Underline => true,
        }
    }

    /// Whether the cursor should be drawn this frame given the current
    /// blink phase. Always true for non-blinking shapes.
    fn cursor_visible_now(&self, now: Instant) -> bool {
        if !self.cursor_blinks() {
            return true;
        }
        let elapsed = now.duration_since(self.blink_epoch);
        let half_ms = BLINK_HALF_PERIOD.as_millis();
        // On for the first half, off for the second half, repeating.
        (elapsed.as_millis() / half_ms) % 2 == 0
    }

    /// Wall-clock time of the next blink phase change after `now`. Used
    /// to schedule a `WaitUntil` so the event loop wakes in time to flip
    /// the cursor.
    fn next_blink_toggle(&self, now: Instant) -> Instant {
        let elapsed = now.duration_since(self.blink_epoch);
        let half_ms = BLINK_HALF_PERIOD.as_millis() as u64;
        let elapsed_ms = elapsed.as_millis() as u64;
        let next_boundary_ms = (elapsed_ms / half_ms + 1) * half_ms;
        self.blink_epoch + Duration::from_millis(next_boundary_ms)
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

/// Map a key (lowered char + shift + ctrl) to a `vi::Motion`, or `None`
/// if it isn't a motion. Shared between the regular vi dispatch (when a
/// motion key is pressed in vi-normal) and the operator-pending dispatch
/// (when a motion key follows `y` to define a yank range).
fn key_to_motion(ch: char, shift: bool, ctrl: bool) -> Option<vi::Motion> {
    if ctrl {
        return match ch {
            'd' => Some(vi::Motion::HalfPageDown),
            'u' => Some(vi::Motion::HalfPageUp),
            'f' => Some(vi::Motion::FullPageDown),
            'b' => Some(vi::Motion::FullPageUp),
            _ => None,
        };
    }
    match (ch, shift) {
        ('h', false) => Some(vi::Motion::Char(0, -1)),
        ('j', false) => Some(vi::Motion::Char(1, 0)),
        ('k', false) => Some(vi::Motion::Char(-1, 0)),
        ('l', false) => Some(vi::Motion::Char(0, 1)),
        ('h', true) => Some(vi::Motion::ScreenTop),
        ('m', true) => Some(vi::Motion::ScreenMiddle),
        ('l', true) => Some(vi::Motion::ScreenBottom),
        ('g', true) => Some(vi::Motion::DocEnd),
        ('w', false) => Some(vi::Motion::WordNext { big: false }),
        ('w', true) => Some(vi::Motion::WordNext { big: true }),
        ('e', false) => Some(vi::Motion::WordEnd { big: false }),
        ('e', true) => Some(vi::Motion::WordEnd { big: true }),
        ('b', false) => Some(vi::Motion::WordPrev { big: false }),
        ('b', true) => Some(vi::Motion::WordPrev { big: true }),
        ('0', _) => Some(vi::Motion::LineStart),
        ('^', _) => Some(vi::Motion::LineFirstNonBlank),
        ('$', _) => Some(vi::Motion::LineEnd),
        // 'g' lowercase starts a Goto pending op rather than being a
        // motion in itself; the caller resolves it via pending_op state.
        _ => None,
    }
}

/// Yank an absolute (scrollback ++ live) range to clipboard + primary.
/// Empty extracts are skipped to avoid wiping existing clipboard contents
/// on a stray operator press with nothing to copy.
fn yank_range_to_clipboard(
    clip: &mut clipboard::Clipboard,
    term: &Term,
    start_abs: (usize, usize),
    end_abs: (usize, usize),
) {
    let text = vi::extract_absolute_range(term, start_abs, end_abs);
    if !text.is_empty() {
        clip.set_primary(text.clone());
        clip.set_text(text);
    }
}

/// Open a vi-mode search at the current cursor / viewport. The user is
/// now in `typing=true` mode; subsequent printable keys edit the query.
fn start_search(vi_state: &mut vi::ViMode, term: &Term, direction: vi::SearchDirection) {
    vi_state.search = Some(vi::Search {
        direction,
        query: String::new(),
        typing: true,
        matches: Vec::new(),
        current: None,
        origin_cursor: vi_state.cursor,
        origin_viewport_offset: term.viewport_offset,
    });
}

/// Recompute matches for the current query, pick the new "current"
/// match relative to the search origin, and live-jump the cursor +
/// viewport to it. Empty query → restore origin (so backspacing back
/// to nothing returns the user to where they came from).
fn refresh_search(vi_state: &mut vi::ViMode, term: &mut Term) {
    if vi_state.search.is_none() {
        return;
    }
    // Phase 1: read-only — compute fresh matches + current index from
    // origin. Drops the immutable vi.search borrow before phase 2.
    let (new_matches, new_current, query_empty) = {
        let s = vi_state.search.as_ref().unwrap();
        let origin_abs =
            vi::viewport_to_absolute(term, s.origin_cursor.0, s.origin_viewport_offset);
        let matches = vi::find_matches(term, &s.query);
        let current =
            vi::pick_current_match(&matches, origin_abs, s.origin_cursor.1, s.direction);
        (matches, current, s.query.is_empty())
    };
    // Phase 2: write back, extract jump target.
    let to_jump = {
        let s = vi_state.search.as_mut().unwrap();
        s.matches = new_matches;
        s.current = new_current;
        s.current.map(|i| s.matches[i])
    };
    if let Some(m) = to_jump {
        let view_rows = term.grid().rows;
        let new_vrow = vi::jump_viewport_to(term, m.row, view_rows);
        vi_state.cursor = (new_vrow, m.col_start);
    } else if query_empty {
        // No query → put the user back where they started so live
        // typing feels reversible.
        let (cursor, offset) = {
            let s = vi_state.search.as_ref().unwrap();
            (s.origin_cursor, s.origin_viewport_offset)
        };
        let cur = term.viewport_offset;
        let delta = offset as isize - cur as isize;
        term.scroll_view(delta);
        vi_state.cursor = cursor;
    }
    // Else: query non-empty but no matches — leave cursor where it is.
}

/// Esc out of search. Restores the cursor + viewport to where they
/// were when search opened.
fn cancel_search(vi_state: &mut vi::ViMode, term: &mut Term) {
    if let Some(s) = vi_state.search.take() {
        let cur = term.viewport_offset;
        let delta = s.origin_viewport_offset as isize - cur as isize;
        term.scroll_view(delta);
        vi_state.cursor = s.origin_cursor;
    }
}

/// `n` / `N` after commit. `reverse` flips relative to the original
/// search direction so `n` always means "continue in original direction"
/// — matching vim.
fn search_navigate(vi_state: &mut vi::ViMode, term: &mut Term, reverse: bool) {
    if vi_state.search.is_none() {
        return;
    }
    let (m, new_idx, view_rows) = {
        let s = vi_state.search.as_ref().unwrap();
        if s.matches.is_empty() {
            return;
        }
        let advance_forward = match (s.direction, reverse) {
            (vi::SearchDirection::Forward, false) => true,
            (vi::SearchDirection::Forward, true) => false,
            (vi::SearchDirection::Backward, false) => false,
            (vi::SearchDirection::Backward, true) => true,
        };
        let cur = s.current.unwrap_or(0);
        let new_idx = if advance_forward {
            (cur + 1) % s.matches.len()
        } else if cur == 0 {
            s.matches.len() - 1
        } else {
            cur - 1
        };
        let m = s.matches[new_idx];
        let view_rows = term.grid().rows;
        (m, new_idx, view_rows)
    };
    vi_state.search.as_mut().unwrap().current = Some(new_idx);
    let new_vrow = vi::jump_viewport_to(term, m.row, view_rows);
    vi_state.cursor = (new_vrow, m.col_start);
}

fn apply_picker_preview(
    gpu: &mut Gpu,
    term: &mut Term,
    picker: &picker::Picker,
    lib: &theme::ThemeLib,
) {
    let theme = picker.current(lib);
    gpu.set_theme(theme);
    term.set_theme_colors(theme.fg, theme.bg, theme.cursor_bg);
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
fn encode_key(
    logical: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
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
        if let Some(seq) = named_key_seq(*named, ctrl, alt, shift, app_cursor) {
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

/// Cursor / Home / End encoding. Default form is `CSI [1;<m>]<letter>`
/// (`ESC [ A` etc.), but in DECCKM application-cursor mode the
/// *unmodified* form switches to the SS3 sequence `ESC O <letter>`.
/// Modifier-encoded forms (`CSI 1;<m><letter>`) keep the CSI form
/// regardless — DECCKM only swaps the no-modifier branch.
fn csi_letter(letter: u8, m: u8, app_cursor: bool) -> Vec<u8> {
    if m == 1 {
        if app_cursor {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
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

fn named_key_seq(
    named: NamedKey,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
) -> Option<Vec<u8>> {
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
        NamedKey::ArrowUp => csi_letter(b'A', m, app_cursor),
        NamedKey::ArrowDown => csi_letter(b'B', m, app_cursor),
        NamedKey::ArrowRight => csi_letter(b'C', m, app_cursor),
        NamedKey::ArrowLeft => csi_letter(b'D', m, app_cursor),
        NamedKey::Home => csi_letter(b'H', m, app_cursor),
        NamedKey::End => csi_letter(b'F', m, app_cursor),
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
        assert_eq!(encode_key(&ch("c"), None, mods, false), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_d_emits_eot() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(encode_key(&ch("d"), None, mods, false), Some(vec![0x04]));
    }

    #[test]
    fn ctrl_open_bracket_is_escape() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(encode_key(&ch("["), None, mods, false), Some(vec![0x1b]));
    }

    #[test]
    fn alt_b_prefixes_with_escape() {
        let mods = ModifiersState::ALT;
        assert_eq!(
            encode_key(&ch("b"), Some("b"), mods, false),
            Some(vec![0x1b, b'b'])
        );
    }

    #[test]
    fn shift_tab_emits_back_tab() {
        let mods = ModifiersState::SHIFT;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Tab), None, mods, false),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn shift_arrow_uses_modifier_param() {
        let mods = ModifiersState::SHIFT;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, mods, false),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn ctrl_arrow_uses_modifier_param() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowRight), None, mods, false),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn plain_char_passthrough() {
        let mods = ModifiersState::empty();
        assert_eq!(
            encode_key(&ch("a"), Some("a"), mods, false),
            Some(vec![b'a'])
        );
    }

    // ---------- DECCKM (application cursor keys) ----------

    #[test]
    fn decckm_off_emits_csi_arrows() {
        // Default mode: arrows are CSI A/B/C/D.
        let mods = ModifiersState::empty();
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, mods, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Home), None, mods, false),
            Some(b"\x1b[H".to_vec())
        );
    }

    #[test]
    fn decckm_on_swaps_unmodified_arrows_to_ss3() {
        // App-cursor mode: unmodified arrows + Home/End become SS3 form
        // (`ESC O <letter>`), which is what vim/tmux ask for.
        let mods = ModifiersState::empty();
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, mods, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowDown), None, mods, true),
            Some(b"\x1bOB".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowRight), None, mods, true),
            Some(b"\x1bOC".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowLeft), None, mods, true),
            Some(b"\x1bOD".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Home), None, mods, true),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::End), None, mods, true),
            Some(b"\x1bOF".to_vec())
        );
    }

    #[test]
    fn decckm_on_keeps_csi_form_with_modifiers() {
        // DECCKM only affects unmodified arrows. Shift/Ctrl/Alt+arrow
        // still uses the CSI 1;<m><letter> form so xterm modifier
        // encoding stays intact.
        let shift = ModifiersState::SHIFT;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, shift, true),
            Some(b"\x1b[1;2A".to_vec())
        );
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowRight), None, ctrl, true),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn decckm_does_not_affect_pageup_or_function_keys() {
        // PageUp/PageDown/Insert/Delete/Fn use the tilde form, which
        // DECCKM doesn't touch — only arrows + Home/End swap to SS3.
        let mods = ModifiersState::empty();
        assert_eq!(
            encode_key(&Key::Named(NamedKey::PageUp), None, mods, true),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::F5), None, mods, true),
            Some(b"\x1b[15~".to_vec())
        );
    }
}
