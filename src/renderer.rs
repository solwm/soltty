use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::font::FontAtlas;
use crate::grid::{CellAttrs, Color};
use crate::term::Term;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
struct CellInstance {
    cell_xy: [u32; 2],
    glyph_origin: [u32; 2],
    glyph_size: [u32; 2],
    glyph_offset: [i32; 2],
    fg: [f32; 4],
    bg: [f32; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Uniforms {
    screen_size: [f32; 2],
    cell_size: [f32; 2],
    atlas_size: [f32; 2],
    grid_offset: [f32; 2],
}

pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    atlas_texture: wgpu::Texture,
    atlas_size: (u32, u32),
    uniform_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    instance_count: u32,
    instances_scratch: Vec<CellInstance>,

    palette: [[f32; 4]; 256],
    default_fg: [f32; 4],
    default_bg: [f32; 4],

    screen_size: (u32, u32),
    cell_size: (u32, u32),
}

impl Renderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        screen_size: (u32, u32),
        atlas: &FontAtlas,
    ) -> Self {
        let palette = build_palette();
        let default_fg = srgb_to_linear_rgba([214, 214, 214, 255]);
        let default_bg = srgb_to_linear_rgba([11, 12, 17, 255]);

        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("soltty-atlas"),
            size: wgpu::Extent3d {
                width: atlas.atlas_w,
                height: atlas.atlas_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        upload_atlas_full(queue, &atlas_texture, atlas);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("soltty-atlas-samp"),
            // Nearest sampling: glyphs are placed at integer pixel offsets.
            // Linear would soften them and was the source of the 1px gutter
            // we put around each entry in the atlas.
            min_filter: wgpu::FilterMode::Nearest,
            mag_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let uniforms = Uniforms {
            screen_size: [screen_size.0 as f32, screen_size.1 as f32],
            cell_size: [atlas.metrics.cell_w as f32, atlas.metrics.cell_h as f32],
            atlas_size: [atlas.atlas_w as f32, atlas.atlas_h as f32],
            grid_offset: [0.0, 0.0],
        };
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("soltty-uniforms"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("soltty-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("soltty-bg"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("soltty-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("soltty-pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_attrs = [
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Uint32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: wgpu::VertexFormat::Uint32x2,
            },
            wgpu::VertexAttribute {
                offset: 16,
                shader_location: 2,
                format: wgpu::VertexFormat::Uint32x2,
            },
            wgpu::VertexAttribute {
                offset: 24,
                shader_location: 3,
                format: wgpu::VertexFormat::Sint32x2,
            },
            wgpu::VertexAttribute {
                offset: 32,
                shader_location: 4,
                format: wgpu::VertexFormat::Float32x4,
            },
            wgpu::VertexAttribute {
                offset: 48,
                shader_location: 5,
                format: wgpu::VertexFormat::Float32x4,
            },
        ];

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("soltty-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<CellInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &instance_attrs,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let instance_capacity = 4096;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("soltty-instances"),
            size: (instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group,
            atlas_texture,
            atlas_size: (atlas.atlas_w, atlas.atlas_h),
            uniform_buffer,
            instance_buffer,
            instance_capacity,
            instance_count: 0,
            instances_scratch: Vec::with_capacity(instance_capacity),
            palette,
            default_fg,
            default_bg,
            screen_size,
            cell_size: (atlas.metrics.cell_w, atlas.metrics.cell_h),
        }
    }

    #[allow(dead_code)] // surfaced through Gpu in milestone 5
    pub fn cell_size(&self) -> (u32, u32) {
        self.cell_size
    }

    pub fn clear_color(&self) -> wgpu::Color {
        wgpu::Color {
            r: self.default_bg[0] as f64,
            g: self.default_bg[1] as f64,
            b: self.default_bg[2] as f64,
            a: 1.0,
        }
    }

    pub fn resize(&mut self, queue: &wgpu::Queue, screen_size: (u32, u32)) {
        self.screen_size = screen_size;
        self.write_uniforms(queue);
    }

    fn write_uniforms(&self, queue: &wgpu::Queue) {
        let uniforms = Uniforms {
            screen_size: [self.screen_size.0 as f32, self.screen_size.1 as f32],
            cell_size: [self.cell_size.0 as f32, self.cell_size.1 as f32],
            atlas_size: [self.atlas_size.0 as f32, self.atlas_size.1 as f32],
            grid_offset: [0.0, 0.0],
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        term: &Term,
        atlas: &mut FontAtlas,
    ) {
        let rows = term.grid().rows;
        let cols = term.grid().cols;

        // Step 1: ensure every visible glyph is rasterized into the atlas.
        for vrow in 0..rows {
            for cell in &term.viewport_row(vrow).cells {
                if !is_blank_glyph(cell.ch) {
                    atlas.ensure(cell.ch);
                }
            }
        }
        if atlas.atlas_dirty {
            upload_atlas_full(queue, &self.atlas_texture, atlas);
            atlas.atlas_dirty = false;
        }

        // Step 2: pack instances over the viewport, not the live grid.
        self.instances_scratch.clear();
        let cursor = term.viewport_cursor();
        for vrow in 0..rows {
            let row = term.viewport_row(vrow);
            for (col_idx, cell) in row.cells.iter().take(cols).enumerate() {
                let mut fg = resolve_color(cell.fg, &self.palette, self.default_fg);
                let mut bg = resolve_color(cell.bg, &self.palette, self.default_bg);
                if cell.attrs.has(CellAttrs::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cursor == Some((vrow, col_idx)) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let glyph = atlas.get(cell.ch).unwrap_or_default();
                self.instances_scratch.push(CellInstance {
                    cell_xy: [col_idx as u32, vrow as u32],
                    glyph_origin: [glyph.atlas_x as u32, glyph.atlas_y as u32],
                    glyph_size: [glyph.w as u32, glyph.h as u32],
                    glyph_offset: [glyph.offset_x as i32, glyph.offset_y as i32],
                    fg,
                    bg,
                });
            }
        }

        // Step 3: ensure GPU buffer is large enough; upload.
        if self.instances_scratch.len() > self.instance_capacity {
            self.instance_capacity = self.instances_scratch.len().next_power_of_two();
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("soltty-instances"),
                size: (self.instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&self.instances_scratch),
        );
        self.instance_count = self.instances_scratch.len() as u32;

        self.write_uniforms(queue);
    }

    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if self.instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..6, 0..self.instance_count);
    }
}

fn is_blank_glyph(c: char) -> bool {
    c == ' ' || c == '\0'
}

fn upload_atlas_full(queue: &wgpu::Queue, texture: &wgpu::Texture, atlas: &FontAtlas) {
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &atlas.atlas_data,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(atlas.atlas_w),
            rows_per_image: Some(atlas.atlas_h),
        },
        wgpu::Extent3d {
            width: atlas.atlas_w,
            height: atlas.atlas_h,
            depth_or_array_layers: 1,
        },
    );
}

fn resolve_color(c: Color, palette: &[[f32; 4]; 256], default: [f32; 4]) -> [f32; 4] {
    match c {
        Color::Default => default,
        Color::Indexed(i) => palette[i as usize],
        Color::Rgb(r, g, b) => srgb_to_linear_rgba([r, g, b, 255]),
    }
}

fn srgb_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_to_linear_rgba([r, g, b, a]: [u8; 4]) -> [f32; 4] {
    [
        srgb_to_linear(r),
        srgb_to_linear(g),
        srgb_to_linear(b),
        a as f32 / 255.0,
    ]
}

fn build_palette() -> [[f32; 4]; 256] {
    let mut out = [[0.0; 4]; 256];
    // Standard xterm 16-color palette.
    let basic: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00], // 0  black
        [0xcd, 0x00, 0x00], // 1  red
        [0x00, 0xcd, 0x00], // 2  green
        [0xcd, 0xcd, 0x00], // 3  yellow
        [0x1e, 0x6f, 0xd0], // 4  blue (less retina-burning than #0000ee)
        [0xcd, 0x00, 0xcd], // 5  magenta
        [0x00, 0xcd, 0xcd], // 6  cyan
        [0xe5, 0xe5, 0xe5], // 7  white
        [0x7f, 0x7f, 0x7f], // 8  bright black
        [0xff, 0x40, 0x40], // 9  bright red
        [0x40, 0xff, 0x40], // 10 bright green
        [0xff, 0xff, 0x40], // 11 bright yellow
        [0x60, 0xa0, 0xff], // 12 bright blue
        [0xff, 0x40, 0xff], // 13 bright magenta
        [0x40, 0xff, 0xff], // 14 bright cyan
        [0xff, 0xff, 0xff], // 15 bright white
    ];
    for (i, rgb) in basic.iter().enumerate() {
        out[i] = srgb_to_linear_rgba([rgb[0], rgb[1], rgb[2], 255]);
    }
    // 6×6×6 color cube (indices 16..=231).
    let levels: [u8; 6] = [0, 95, 135, 175, 215, 255];
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                let idx = 16 + r * 36 + g * 6 + b;
                out[idx] = srgb_to_linear_rgba([levels[r], levels[g], levels[b], 255]);
            }
        }
    }
    // Grayscale ramp (indices 232..=255).
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        out[232 + i] = srgb_to_linear_rgba([v, v, v, 255]);
    }
    out
}
