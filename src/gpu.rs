use std::sync::Arc;

use wgpu::SurfaceError;
use winit::window::Window;

use crate::font::FontAtlas;
use crate::renderer::Renderer;
use crate::term::Term;

/// Default font size, ~30% larger than the original 16px baseline.
pub const DEFAULT_FONT_SIZE_PX: f32 = 16.0 * 1.3;
const MIN_FONT_SIZE_PX: f32 = 6.0;
const MAX_FONT_SIZE_PX: f32 = 96.0;

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    atlas: FontAtlas,
    font_px: f32,
}

impl Gpu {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("request adapter");

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("soltty-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: caps
                .present_modes
                .iter()
                .copied()
                .find(|m| *m == wgpu::PresentMode::Mailbox)
                .unwrap_or(wgpu::PresentMode::Fifo),
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        log::info!(
            "wgpu: adapter={:?} format={:?} present={:?}",
            adapter.get_info().name,
            format,
            config.present_mode
        );

        let atlas = FontAtlas::new(DEFAULT_FONT_SIZE_PX).expect("load font");
        let renderer = Renderer::new(
            &device,
            &queue,
            format,
            (config.width, config.height),
            &atlas,
        );

        Self {
            surface,
            device,
            queue,
            config,
            renderer,
            atlas,
            font_px: DEFAULT_FONT_SIZE_PX,
        }
    }

    pub fn cell_size(&self) -> (u32, u32) {
        self.renderer.cell_size()
    }

    pub fn font_size(&self) -> f32 {
        self.font_px
    }

    /// Reload the font atlas at a new pixel size and rewire it into the
    /// renderer. Returns the new cell size so the caller can resize the
    /// terminal grid + PTY winsize accordingly.
    pub fn set_font_size(&mut self, px: f32) -> (u32, u32) {
        let px = px.clamp(MIN_FONT_SIZE_PX, MAX_FONT_SIZE_PX);
        if (px - self.font_px).abs() < 0.05 {
            return self.renderer.cell_size();
        }
        match FontAtlas::new(px) {
            Ok(atlas) => {
                self.atlas = atlas;
                self.renderer.reload_font(&self.queue, &self.atlas);
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
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.renderer.resize(&self.queue, (width, height));
    }

    pub fn render(&mut self, term: &Term) -> Result<(), SurfaceError> {
        self.renderer
            .prepare(&self.device, &self.queue, term, &mut self.atlas);

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(SurfaceError::Lost | SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                self.surface.get_current_texture()?
            }
            Err(e) => return Err(e),
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("soltty-encoder"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("soltty-grid"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.renderer.clear_color()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.renderer.draw(&mut pass);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}
