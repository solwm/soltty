use glow::HasContext;

use crate::font::FontAtlas;
use crate::grid::{CellAttrs, Color};
use crate::term::Term;

/// One instance per cell. Layout has to match the vertex shader's attribute
/// declarations (locations 0..=5) and the `vertex_attrib_pointer_*` calls in
/// `Renderer::new`.
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct CellInstance {
    cell_xy: [u32; 2],         // 0..8
    glyph_origin: [u32; 2],    // 8..16
    glyph_size: [u32; 2],      // 16..24
    glyph_offset: [i32; 2],    // 24..32
    fg: [f32; 4],              // 32..48
    bg: [f32; 4],              // 48..64
}

const INSTANCE_STRIDE: i32 = std::mem::size_of::<CellInstance>() as i32;

pub struct Renderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    atlas_tex: glow::Texture,
    instance_capacity: usize,
    instance_count: i32,
    instances_scratch: Vec<CellInstance>,

    // Uniform locations cached at link time.
    u_screen_size: glow::UniformLocation,
    u_cell_size: glow::UniformLocation,
    u_atlas_size: glow::UniformLocation,
    u_grid_offset: glow::UniformLocation,
    u_atlas: glow::UniformLocation,

    palette: [[f32; 4]; 256],
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    cursor_bg: [f32; 4],
    cursor_fg: [f32; 4],

    screen_size: (u32, u32),
    cell_size: (u32, u32),
    atlas_size: (u32, u32),
}

impl Renderer {
    pub fn new(gl: &glow::Context, screen_size: (u32, u32), atlas: &FontAtlas) -> Self {
        let palette = build_palette();
        let default_fg = srgb_to_linear_rgba([214, 214, 214, 255]);
        let default_bg = srgb_to_linear_rgba([11, 12, 17, 255]);
        let cursor_bg = srgb_to_linear_rgba([235, 235, 235, 255]);
        let cursor_fg = default_bg;

        let (program, u_screen_size, u_cell_size, u_atlas_size, u_grid_offset, u_atlas) =
            unsafe { build_program(gl) };

        let (vao, vbo) = unsafe { build_vao_vbo(gl, INITIAL_INSTANCE_CAPACITY) };

        let atlas_tex = unsafe { build_atlas_texture(gl, atlas) };

        let renderer = Self {
            program,
            vao,
            vbo,
            atlas_tex,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            instance_count: 0,
            instances_scratch: Vec::new(),
            u_screen_size,
            u_cell_size,
            u_atlas_size,
            u_grid_offset,
            u_atlas,
            palette,
            default_fg,
            default_bg,
            cursor_bg,
            cursor_fg,
            screen_size,
            cell_size: (atlas.metrics.cell_w, atlas.metrics.cell_h),
            atlas_size: (atlas.atlas_w, atlas.atlas_h),
        };
        unsafe { renderer.upload_uniforms(gl) };
        renderer
    }

    pub fn cell_size(&self) -> (u32, u32) {
        self.cell_size
    }


    pub fn resize(&mut self, gl: &glow::Context, screen_size: (u32, u32)) {
        self.screen_size = screen_size;
        unsafe {
            gl.viewport(0, 0, screen_size.0 as i32, screen_size.1 as i32);
            self.upload_uniforms(gl);
        }
    }

    /// Re-upload glyph atlas pixels and update cell-size uniform after a
    /// font reload (e.g. zoom). Atlas dimensions are fixed in `FontAtlas::new`,
    /// so the texture object stays the same — we just call `tex_sub_image_2d`.
    pub fn reload_font(&mut self, gl: &glow::Context, atlas: &FontAtlas) {
        debug_assert_eq!(self.atlas_size, (atlas.atlas_w, atlas.atlas_h));
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                atlas.atlas_w as i32,
                atlas.atlas_h as i32,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(&atlas.atlas_data),
            );
        }
        self.cell_size = (atlas.metrics.cell_w, atlas.metrics.cell_h);
        unsafe { self.upload_uniforms(gl) };
    }

    pub fn prepare(&mut self, gl: &glow::Context, term: &Term, atlas: &mut FontAtlas) {
        let rows = term.grid().rows;
        let cols = term.grid().cols;
        let cursor = term.viewport_cursor();

        // Single pass: ensure each cell's glyph in the atlas, then pack the
        // instance immediately. Same shape as the wgpu version was.
        self.instances_scratch.clear();
        for vrow in 0..rows {
            let row = term.viewport_row(vrow);
            for (col_idx, cell) in row.cells.iter().take(cols).enumerate() {
                if !is_blank_glyph(cell.ch) {
                    atlas.ensure(cell.ch);
                }
                let mut fg = resolve_color(cell.fg, &self.palette, self.default_fg);
                let mut bg = resolve_color(cell.bg, &self.palette, self.default_bg);
                if cell.attrs.has(CellAttrs::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cursor == Some((vrow, col_idx)) {
                    bg = self.cursor_bg;
                    fg = self.cursor_fg;
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

        if atlas.atlas_dirty {
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    atlas.atlas_w as i32,
                    atlas.atlas_h as i32,
                    glow::RED,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&atlas.atlas_data),
                );
            }
            atlas.atlas_dirty = false;
        }

        let needed = self.instances_scratch.len();
        unsafe {
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            if needed > self.instance_capacity {
                self.instance_capacity = needed.next_power_of_two();
                gl.buffer_data_size(
                    glow::ARRAY_BUFFER,
                    (self.instance_capacity * std::mem::size_of::<CellInstance>()) as i32,
                    glow::DYNAMIC_DRAW,
                );
            }
            let bytes = std::slice::from_raw_parts(
                self.instances_scratch.as_ptr() as *const u8,
                needed * std::mem::size_of::<CellInstance>(),
            );
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);
        }
        self.instance_count = needed as i32;
    }

    pub fn draw(&self, gl: &glow::Context) {
        unsafe {
            let bg = self.default_bg;
            gl.clear_color(bg[0], bg[1], bg[2], 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            if self.instance_count == 0 {
                return;
            }
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
            gl.uniform_1_i32(Some(&self.u_atlas), 0);
            gl.draw_arrays_instanced(glow::TRIANGLES, 0, 6, self.instance_count);
        }
    }

    unsafe fn upload_uniforms(&self, gl: &glow::Context) {
        gl.use_program(Some(self.program));
        gl.uniform_2_f32(
            Some(&self.u_screen_size),
            self.screen_size.0 as f32,
            self.screen_size.1 as f32,
        );
        gl.uniform_2_f32(
            Some(&self.u_cell_size),
            self.cell_size.0 as f32,
            self.cell_size.1 as f32,
        );
        gl.uniform_2_f32(
            Some(&self.u_atlas_size),
            self.atlas_size.0 as f32,
            self.atlas_size.1 as f32,
        );
        gl.uniform_2_f32(Some(&self.u_grid_offset), 0.0, 0.0);
    }
}

const INITIAL_INSTANCE_CAPACITY: usize = 4096;

unsafe fn build_program(
    gl: &glow::Context,
) -> (
    glow::Program,
    glow::UniformLocation,
    glow::UniformLocation,
    glow::UniformLocation,
    glow::UniformLocation,
    glow::UniformLocation,
) {
    let vs_src = include_str!("shader.vert");
    let fs_src = include_str!("shader.frag");

    let program = gl.create_program().expect("create program");

    let vs = compile_shader(gl, glow::VERTEX_SHADER, vs_src);
    let fs = compile_shader(gl, glow::FRAGMENT_SHADER, fs_src);
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        panic!("link: {}", gl.get_program_info_log(program));
    }
    gl.detach_shader(program, vs);
    gl.detach_shader(program, fs);
    gl.delete_shader(vs);
    gl.delete_shader(fs);

    let u = |name: &str| {
        gl.get_uniform_location(program, name)
            .unwrap_or_else(|| panic!("uniform {name} not found"))
    };
    (
        program,
        u("u_screen_size"),
        u("u_cell_size"),
        u("u_atlas_size"),
        u("u_grid_offset"),
        u("u_atlas"),
    )
}

unsafe fn compile_shader(gl: &glow::Context, kind: u32, src: &str) -> glow::Shader {
    let shader = gl.create_shader(kind).expect("create shader");
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        panic!(
            "compile {} shader: {}",
            if kind == glow::VERTEX_SHADER { "vertex" } else { "fragment" },
            gl.get_shader_info_log(shader)
        );
    }
    shader
}

unsafe fn build_vao_vbo(gl: &glow::Context, capacity: usize) -> (glow::VertexArray, glow::Buffer) {
    let vao = gl.create_vertex_array().expect("create vao");
    let vbo = gl.create_buffer().expect("create vbo");

    gl.bind_vertex_array(Some(vao));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
    gl.buffer_data_size(
        glow::ARRAY_BUFFER,
        (capacity * std::mem::size_of::<CellInstance>()) as i32,
        glow::DYNAMIC_DRAW,
    );

    // Integer attribute pointers for uvec2/ivec2 use glVertexAttribIPointer.
    // Float pointers use glVertexAttribPointer.
    let attrs: &[(u32, i32, u32, bool, i32)] = &[
        // (location, components, gl_type, is_int, byte_offset)
        (0, 2, glow::UNSIGNED_INT, true, 0),  // cell_xy
        (1, 2, glow::UNSIGNED_INT, true, 8),  // glyph_origin
        (2, 2, glow::UNSIGNED_INT, true, 16), // glyph_size
        (3, 2, glow::INT, true, 24),          // glyph_offset
        (4, 4, glow::FLOAT, false, 32),       // fg
        (5, 4, glow::FLOAT, false, 48),       // bg
    ];
    for &(loc, count, ty, is_int, offset) in attrs {
        gl.enable_vertex_attrib_array(loc);
        if is_int {
            gl.vertex_attrib_pointer_i32(loc as u32, count, ty, INSTANCE_STRIDE, offset);
        } else {
            gl.vertex_attrib_pointer_f32(loc as u32, count, ty, false, INSTANCE_STRIDE, offset);
        }
        gl.vertex_attrib_divisor(loc as u32, 1);
    }

    gl.bind_buffer(glow::ARRAY_BUFFER, None);
    gl.bind_vertex_array(None);
    (vao, vbo)
}

unsafe fn build_atlas_texture(gl: &glow::Context, atlas: &FontAtlas) -> glow::Texture {
    let tex = gl.create_texture().expect("create texture");
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::R8 as i32,
        atlas.atlas_w as i32,
        atlas.atlas_h as i32,
        0,
        glow::RED,
        glow::UNSIGNED_BYTE,
        Some(&atlas.atlas_data),
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::NEAREST as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::NEAREST as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    tex
}

fn is_blank_glyph(c: char) -> bool {
    c == ' ' || c == '\0'
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
    let basic: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xcd, 0x00, 0x00],
        [0x00, 0xcd, 0x00],
        [0xcd, 0xcd, 0x00],
        [0x1e, 0x6f, 0xd0],
        [0xcd, 0x00, 0xcd],
        [0x00, 0xcd, 0xcd],
        [0xe5, 0xe5, 0xe5],
        [0x7f, 0x7f, 0x7f],
        [0xff, 0x40, 0x40],
        [0x40, 0xff, 0x40],
        [0xff, 0xff, 0x40],
        [0x60, 0xa0, 0xff],
        [0xff, 0x40, 0xff],
        [0x40, 0xff, 0xff],
        [0xff, 0xff, 0xff],
    ];
    for (i, rgb) in basic.iter().enumerate() {
        out[i] = srgb_to_linear_rgba([rgb[0], rgb[1], rgb[2], 255]);
    }
    let levels: [u8; 6] = [0, 95, 135, 175, 215, 255];
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                let idx = 16 + r * 36 + g * 6 + b;
                out[idx] = srgb_to_linear_rgba([levels[r], levels[g], levels[b], 255]);
            }
        }
    }
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        out[232 + i] = srgb_to_linear_rgba([v, v, v, 255]);
    }
    out
}
