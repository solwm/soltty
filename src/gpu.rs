use std::collections::VecDeque;
use std::ffi::CString;
use std::num::NonZeroU32;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use glow::HasContext;
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, Version,
};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
// `Surface` and `PossiblyCurrentContext` above are the dispatch enums;
// for the EGL-only `swap_buffers_with_damage` call we have to pattern-
// match into the concrete variants. The variant names come straight from
// glutin's source — see `api::egl::surface::Surface` for the method.
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

use crate::config::Config;
use crate::font::FontAtlas;
use crate::picker::Picker;
use crate::renderer::{Renderer, SearchOverlay};
use crate::selection::Selection;
use crate::term::Term;
use crate::theme::Theme;

/// Default font size, ~30% larger than the original 16px baseline.
pub const DEFAULT_FONT_SIZE_PX: f32 = 16.0 * 1.3;
const MIN_FONT_SIZE_PX: f32 = 2.0;
const MAX_FONT_SIZE_PX: f32 = 96.0;

/// Resolve startup font size. Precedence: `SOLTTY_FONT_PX` env var >
/// config-file `font_size` > built-in default. Out-of-range values are
/// clamped silently so a misplaced 0 or huge number can't brick startup.
pub fn startup_font_px(config: &Config) -> f32 {
    std::env::var("SOLTTY_FONT_PX")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .or(config.font_size)
        .map(|px| px.clamp(MIN_FONT_SIZE_PX, MAX_FONT_SIZE_PX))
        .unwrap_or(DEFAULT_FONT_SIZE_PX)
}

pub struct Gpu {
    gl: glow::Context,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    renderer: Renderer,
    atlas: FontAtlas,
    font_px: f32,
    /// Cached so `set_font_size` (font zoom on Ctrl+=) can re-resolve
    /// the user's font paths without us threading the config through
    /// every winit event handler.
    config: Config,
    /// Only populated when `SOLTTY_GPU_TIMING=1`. See `FrameTimings`.
    timings: Option<FrameTimings>,
}

impl Gpu {
    /// Build window + GL context together. Returns the Window so the caller
    /// can pass it to winit and keep an `Arc<Window>` for input handling.
    pub fn new(
        event_loop: &ActiveEventLoop,
        window_attrs: WindowAttributes,
        theme: &Theme,
        config: &Config,
    ) -> (Arc<Window>, Self) {
        // Trim the EGL config to the minimum a terminal actually needs:
        //
        //   - alpha_size(0): we composite manually in the fragment
        //     shader, opaque region is set to the full surface — there
        //     is nothing useful the alpha channel can do. With XRGB
        //     instead of ARGB the compositor knows the surface is fully
        //     opaque at the format level (in addition to the explicit
        //     opaque region hint), so it can skip per-pixel blending.
        //   - num_samples manually picked at 1 (no MSAA): the text
        //     atlas is already anti-aliased per-glyph; MSAA on top
        //     multiplies fragment work + back-buffer bandwidth by the
        //     sample count for zero visible benefit.
        //   - depth_size(0) / stencil_size(0): we draw one instanced
        //     pass and overlay tints with explicit blending state;
        //     neither buffer is read or written. Allocating them only
        //     burns memory bandwidth on every swap.
        //
        // The picker then favors hardware-accelerated configs with
        // exactly these properties.
        let template = ConfigTemplateBuilder::new()
            .prefer_hardware_accelerated(Some(true))
            .with_alpha_size(0)
            .with_depth_size(0)
            .with_stencil_size(0);

        let (window, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(window_attrs))
            .build(event_loop, template, |configs| {
                // Score each config: we want samples=1 (no MSAA) AND
                // alpha_size=0 (opaque). Lower score is better.
                let score = |c: &glutin::config::Config| -> u32 {
                    let samples_penalty = c.num_samples() as u32;
                    let alpha_penalty = if c.alpha_size() == 0 { 0 } else { 100 };
                    let depth_penalty = if c.depth_size() == 0 { 0 } else { 1 };
                    let stencil_penalty = if c.stencil_size() == 0 { 0 } else { 1 };
                    samples_penalty + alpha_penalty + depth_penalty + stencil_penalty
                };
                configs
                    .reduce(|acc, c| if score(&c) < score(&acc) { c } else { acc })
                    .expect("no GL configs")
            })
            .expect("build display");
        log::info!(
            "gl config: samples={} alpha={} buf={:?} depth={} stencil={}",
            gl_config.num_samples(),
            gl_config.alpha_size(),
            gl_config.color_buffer_type(),
            gl_config.depth_size(),
            gl_config.stencil_size(),
        );
        let window = Arc::new(window.expect("DisplayBuilder yielded no window"));

        let raw_window_handle = window
            .window_handle()
            .expect("window_handle")
            .as_raw();
        let gl_display = gl_config.display();

        // Try OpenGL 3.3 core. Fall back to OpenGL ES 3.0 if the driver
        // refuses (rare on desktop, common on embedded).
        let context_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(Some(raw_window_handle));
        let fallback_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
            .build(Some(raw_window_handle));

        let not_current_context = unsafe {
            gl_display
                .create_context(&gl_config, &context_attrs)
                .or_else(|_| gl_display.create_context(&gl_config, &fallback_attrs))
                .expect("create context")
        };

        let surface_attrs = window
            .build_surface_attributes(SurfaceAttributesBuilder::default())
            .expect("build surface attributes");
        let surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .expect("create surface")
        };

        let context = not_current_context
            .make_current(&surface)
            .expect("make current");

        // SwapInterval::DontWait — *don't* block the main thread on vsync.
        //
        // Why: with Wait(1), every `swap_buffers` blocks for up to 16 ms.
        // Since we drain PTY data and render on the same thread, a blocked
        // swap stalls byte processing. Measured: ANSI throughput dropped
        // from ~178 MB/s (DontWait) to ~110 MB/s (Wait) on the same load —
        // a 40% hit just from waiting for vsync.
        //
        // Tearing? On Wayland the compositor handles tearing prevention
        // itself regardless of our swap interval, so DontWait is the right
        // call here. On X11 the same setting can produce tearing on full-
        // screen animation; that's a tradeoff future work would address by
        // moving the GL context to a worker thread.
        let _ = surface.set_swap_interval(&context, SwapInterval::DontWait);

        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|s| gl_display.get_proc_address(s) as *const _)
        };

        // Linear-light blending against an sRGB framebuffer — same model as
        // the wgpu version had, just without wgpu's auto-format pick.
        unsafe {
            gl.enable(glow::FRAMEBUFFER_SRGB);
            let size = window.inner_size();
            gl.viewport(0, 0, size.width as i32, size.height as i32);
        }

        log::info!(
            "gl: {} | {} | {}",
            unsafe { gl.get_parameter_string(glow::VERSION) },
            unsafe { gl.get_parameter_string(glow::RENDERER) },
            unsafe { gl.get_parameter_string(glow::VENDOR) },
        );

        let initial_font_px = startup_font_px(config);
        let mut atlas = FontAtlas::new(initial_font_px, config).expect("load font");
        let inner = window.inner_size();
        let renderer = Renderer::new(&gl, (inner.width, inner.height), &mut atlas, theme);

        let timings = if gpu_timing_enabled() {
            // Pre-create the GL timer queries used for GPU draw timing.
            // glQueryCounter results from frame N aren't typically ready
            // until frame N+2/3, so we ring-buffer FRAME_SLOTS frames'
            // worth of query objects and only read the oldest one.
            let queries = unsafe {
                std::array::from_fn(|_| {
                    [
                        gl.create_query().expect("create draw_start query"),
                        gl.create_query().expect("create draw_end query"),
                    ]
                })
            };
            Some(FrameTimings::new(queries))
        } else {
            None
        };

        let gpu = Self {
            gl,
            surface,
            context,
            renderer,
            atlas,
            font_px: initial_font_px,
            config: config.clone(),
            timings,
        };

        // Suppress the "unused" check for fields that aren't read yet.
        let _ = &gpu.context;

        // Ensure CString is in scope so the loader closure compiles cleanly.
        let _ = CString::new("");

        (window, gpu)
    }

    pub fn cell_size(&self) -> (u32, u32) {
        self.renderer.cell_size()
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.renderer.set_theme(theme);
    }

    pub fn font_size(&self) -> f32 {
        self.font_px
    }

    pub fn set_font_size(&mut self, px: f32) -> (u32, u32) {
        let px = px.clamp(MIN_FONT_SIZE_PX, MAX_FONT_SIZE_PX);
        if (px - self.font_px).abs() < 0.05 {
            return self.renderer.cell_size();
        }
        match FontAtlas::new(px, &self.config) {
            Ok(atlas) => {
                self.atlas = atlas;
                self.renderer.reload_font(&self.gl, &mut self.atlas);
                self.font_px = px;
                log::info!(
                    "font: {:.1}px (cell {:?})",
                    self.font_px,
                    self.renderer.cell_size()
                );
            }
            Err(e) => log::warn!("font reload at {px}px failed: {e}"),
        }
        self.renderer.cell_size()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface.resize(
            &self.context,
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
        );
        self.renderer.resize(&self.gl, (width, height));
    }

    pub fn render(
        &mut self,
        term: &Term,
        picker: Option<&mut Picker>,
        selection: Option<&Selection>,
        cursor_visible_now: bool,
        vi_cursor: Option<(usize, usize)>,
        vi_active: bool,
        search: Option<&SearchOverlay<'_>>,
        burst_active: bool,
    ) {
        // Fast path: when timing is off, skip every Instant::now() and
        // every query_counter call — `timings` is None and the matches
        // collapse.
        let t0 = self.timings.as_ref().map(|_| Instant::now());

        self.renderer.prepare(
            &self.gl,
            term,
            &mut self.atlas,
            picker,
            selection,
            cursor_visible_now,
            vi_cursor,
            search,
            burst_active,
        );

        let t1 = self.timings.as_ref().map(|_| Instant::now());

        // Bracket the draw call(s) with GL_TIMESTAMP queries on the
        // current frame's ring slot. We read the *previous* (FRAME_SLOTS
        // ago) slot's results below — by then the GPU has finished those
        // queries so the read is non-blocking.
        if let Some(t) = self.timings.as_mut() {
            let slot = t.next_slot;
            unsafe {
                self.gl
                    .query_counter(t.draw_queries[slot][0], glow::TIMESTAMP);
            }
        }
        self.renderer.draw(&self.gl);
        if vi_active {
            self.renderer.draw_tint(&self.gl, VI_TINT_COLOR);
        }
        if let Some(t) = self.timings.as_mut() {
            let slot = t.next_slot;
            unsafe {
                self.gl
                    .query_counter(t.draw_queries[slot][1], glow::TIMESTAMP);
            }
        }

        let t_swap_start = self.timings.as_ref().map(|_| Instant::now());
        // `swap_buffers_with_damage` lets the Wayland compositor recompose
        // only the damaged rectangles instead of the whole surface. On
        // NVIDIA 595 EGL the EGL_KHR_swap_buffers_with_damage extension is
        // present; glutin silently falls back to plain SwapBuffers if it
        // isn't, so this stays correctness-safe on any driver. Damage
        // rects come from `Renderer::prepare`'s per-row CellInstance diff
        // against last frame.
        //
        // The method only exists on the EGL surface (not GLX/WGL/CGL), so
        // we have to drop down to the concrete variant — exactly the same
        // dance alacritty does. The catch-all arm preserves behavior on
        // any non-EGL backend.
        // EXPERIMENT v6: skip with-damage entirely, always plain swap.
        let _ = burst_active; // unused in this experiment
        let swap_result = self.surface.swap_buffers(&self.context);
        if let Err(e) = swap_result {
            log::warn!("swap_buffers: {e}");
        }

        if let Some(t) = self.timings.as_mut() {
            let t2 = t_swap_start.unwrap();
            let now = Instant::now();
            t.record_cpu(
                (t1.unwrap() - t0.unwrap()).as_secs_f64() * 1e6, // prepare µs
                (t2 - t1.unwrap()).as_secs_f64() * 1e6,           // draw-submit µs
                (now - t2).as_secs_f64() * 1e3,                   // swap ms
            );
            t.poll_and_advance(&self.gl);
            t.maybe_report(now);
        }
    }
}

/// `SOLTTY_GPU_TIMING=1` — enable per-frame draw/prepare/swap timing
/// printed to stderr each second. Off by default; intended for the
/// perf-investigation loop.
fn gpu_timing_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("SOLTTY_GPU_TIMING")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Ring buffer of GL timer queries plus rolling CPU-side stats for
/// `prepare` and `swap_buffers`. Lives on `Gpu` only when
/// `SOLTTY_GPU_TIMING=1` was set at process start.
///
/// GL timer-query results from frame N typically aren't ready until
/// frame N+2 — the GPU pipeline is deep. We use a ring of FRAME_SLOTS
/// query pairs so the slot we read each frame is the one the GPU
/// finished with several frames ago. `glGetQueryObjectuiv(...,
/// QUERY_RESULT_AVAILABLE, …)` is the guard.
struct FrameTimings {
    draw_queries: [[glow::NativeQuery; 2]; FRAME_SLOTS],
    /// The slot we fill on this frame. Reads happen on the *next* slot
    /// over (the oldest one) — see `poll_and_advance`.
    next_slot: usize,
    /// True once `next_slot` has cycled all the way around; before that
    /// the slot we're about to read was never written and would return
    /// garbage.
    primed: bool,
    /// Latest sampled GPU draw time (microseconds), rolling. We keep a
    /// short window so the printed mean tracks the last ~1 s of frames.
    draw_gpu_us: VecDeque<f64>,
    prepare_cpu_us: VecDeque<f64>,
    draw_cpu_us: VecDeque<f64>,
    swap_cpu_ms: VecDeque<f64>,
    last_report: Instant,
}

const FRAME_SLOTS: usize = 4;
const HISTORY: usize = 256;
const REPORT_INTERVAL_MS: u128 = 1_000;

impl FrameTimings {
    fn new(draw_queries: [[glow::NativeQuery; 2]; FRAME_SLOTS]) -> Self {
        Self {
            draw_queries,
            next_slot: 0,
            primed: false,
            draw_gpu_us: VecDeque::with_capacity(HISTORY),
            prepare_cpu_us: VecDeque::with_capacity(HISTORY),
            draw_cpu_us: VecDeque::with_capacity(HISTORY),
            swap_cpu_ms: VecDeque::with_capacity(HISTORY),
            last_report: Instant::now(),
        }
    }

    fn record_cpu(&mut self, prepare_us: f64, draw_submit_us: f64, swap_ms: f64) {
        push_bounded(&mut self.prepare_cpu_us, prepare_us);
        push_bounded(&mut self.draw_cpu_us, draw_submit_us);
        push_bounded(&mut self.swap_cpu_ms, swap_ms);
    }

    /// After issuing this frame's queries: if the ring has cycled at
    /// least once, read the slot we're about to overwrite next frame.
    /// Its queries are stale enough that `QUERY_RESULT_AVAILABLE` is
    /// almost certainly true; if not (driver hiccup) we skip rather
    /// than block.
    fn poll_and_advance(&mut self, gl: &glow::Context) {
        // Move next_slot forward; this is the slot we'll write next
        // frame, so reading it now retrieves the *oldest* outstanding
        // query pair (written FRAME_SLOTS frames ago).
        self.next_slot = (self.next_slot + 1) % FRAME_SLOTS;
        if !self.primed {
            if self.next_slot == 0 {
                self.primed = true;
            }
            return;
        }
        let slot = self.next_slot;
        let [start_q, end_q] = self.draw_queries[slot];
        unsafe {
            let avail = gl.get_query_parameter_u32(end_q, glow::QUERY_RESULT_AVAILABLE);
            if avail == 0 {
                return; // Driver still working on it; skip this sample.
            }
            // GL_TIMESTAMP results are u64 nanoseconds since some
            // implementation-defined reference. Modern NVIDIA values
            // sit in the trillions, so reading with the u32 getter
            // returns GL_MAX_UINT for *both* timestamps and the delta
            // becomes 0 — silent and useless.
            //
            // glow 0.14's `get_query_parameter_u64_with_offset` has a
            // confusingly named `offset: usize` which is actually used
            // as the destination pointer for the u64 result. Pass the
            // address of a stack u64 cast to usize. Verified against
            // glow native.rs at `gl.GetQueryObjectui64v(..., offset
            // as *mut _)`.
            let mut start_ns: u64 = 0;
            let mut end_ns: u64 = 0;
            gl.get_query_parameter_u64_with_offset(
                start_q,
                glow::QUERY_RESULT,
                &mut start_ns as *mut u64 as usize,
            );
            gl.get_query_parameter_u64_with_offset(
                end_q,
                glow::QUERY_RESULT,
                &mut end_ns as *mut u64 as usize,
            );
            let delta_ns = end_ns.saturating_sub(start_ns) as f64;
            push_bounded(&mut self.draw_gpu_us, delta_ns / 1000.0);
        }
    }

    fn maybe_report(&mut self, now: Instant) {
        if (now - self.last_report).as_millis() < REPORT_INTERVAL_MS {
            return;
        }
        self.last_report = now;
        eprintln!(
            "gpu_timing: prepare {} draw_cpu {} draw_gpu {} swap {}",
            fmt_us(&self.prepare_cpu_us),
            fmt_us(&self.draw_cpu_us),
            fmt_us(&self.draw_gpu_us),
            fmt_ms(&self.swap_cpu_ms),
        );
        // Don't clear — let the next interval recompute over a rolling
        // window. `push_bounded` already caps history at HISTORY frames.
    }
}

fn push_bounded(buf: &mut VecDeque<f64>, v: f64) {
    if buf.len() == HISTORY {
        buf.pop_front();
    }
    buf.push_back(v);
}

fn fmt_us(buf: &VecDeque<f64>) -> String {
    if buf.is_empty() {
        return "n=0".into();
    }
    let n = buf.len();
    let sum: f64 = buf.iter().sum();
    let mean = sum / n as f64;
    let mut sorted: Vec<f64> = buf.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = sorted[((n as f64) * 0.95).floor() as usize];
    format!("mean={:.0}µs p95={:.0}µs n={}", mean, p95, n)
}

fn fmt_ms(buf: &VecDeque<f64>) -> String {
    if buf.is_empty() {
        return "n=0".into();
    }
    let n = buf.len();
    let sum: f64 = buf.iter().sum();
    let mean = sum / n as f64;
    let mut sorted: Vec<f64> = buf.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = sorted[((n as f64) * 0.95).floor() as usize];
    format!("mean={:.2}ms p95={:.2}ms n={}", mean, p95, n)
}

/// Light-green wash painted over the whole window when vi-mode is active.
/// CSS `lightgreen` (#90EE90 = rgb(144, 238, 144)), pre-converted to
/// linear-light because the framebuffer is sRGB-encoded — values here go
/// through `GL_FRAMEBUFFER_SRGB` and come out the right green. Alpha is
/// the wash strength; 0.20 is enough to read at a glance without burying
/// the underlying terminal content.
const VI_TINT_COLOR: [f32; 4] = [0.279, 0.855, 0.279, 0.015];
