use std::ffi::CString;
use std::num::NonZeroU32;
use std::sync::Arc;

use glow::HasContext;
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, Version,
};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

use crate::font::FontAtlas;
use crate::picker::Picker;
use crate::renderer::Renderer;
use crate::term::Term;
use crate::theme::Theme;

/// Default font size, ~30% larger than the original 16px baseline.
pub const DEFAULT_FONT_SIZE_PX: f32 = 16.0 * 1.3;
const MIN_FONT_SIZE_PX: f32 = 6.0;
const MAX_FONT_SIZE_PX: f32 = 96.0;

pub struct Gpu {
    gl: glow::Context,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    renderer: Renderer,
    atlas: FontAtlas,
    font_px: f32,
}

impl Gpu {
    /// Build window + GL context together. Returns the Window so the caller
    /// can pass it to winit and keep an `Arc<Window>` for input handling.
    pub fn new(
        event_loop: &ActiveEventLoop,
        window_attrs: WindowAttributes,
        theme: &Theme,
    ) -> (Arc<Window>, Self) {
        let template = ConfigTemplateBuilder::new()
            .prefer_hardware_accelerated(Some(true))
            .with_alpha_size(8);

        let (window, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(window_attrs))
            .build(event_loop, template, |configs| {
                // Pick a config with the most samples we can get; ties broken
                // by accepting the first.
                configs
                    .reduce(|acc, c| if c.num_samples() > acc.num_samples() { c } else { acc })
                    .expect("no GL configs")
            })
            .expect("build display");
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

        let atlas = FontAtlas::new(DEFAULT_FONT_SIZE_PX).expect("load font");
        let inner = window.inner_size();
        let renderer = Renderer::new(&gl, (inner.width, inner.height), &atlas, theme);

        let gpu = Self {
            gl,
            surface,
            context,
            renderer,
            atlas,
            font_px: DEFAULT_FONT_SIZE_PX,
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
        match FontAtlas::new(px) {
            Ok(atlas) => {
                self.atlas = atlas;
                self.renderer.reload_font(&self.gl, &self.atlas);
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

    pub fn render(&mut self, term: &Term, picker: Option<&mut Picker>) {
        self.renderer
            .prepare(&self.gl, term, &mut self.atlas, picker);
        self.renderer.draw(&self.gl);
        if let Err(e) = self.surface.swap_buffers(&self.context) {
            log::warn!("swap_buffers: {e}");
        }
    }
}
