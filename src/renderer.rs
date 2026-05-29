use glow::HasContext;
use glutin::surface::Rect;

use crate::font::{FontAtlas, FontStyle};
use crate::grid::{CellAttrs, Color, CursorShape};
use crate::picker::Picker;
use crate::selection::Selection;
use crate::term::Term;
use crate::theme::Theme;

/// One match in viewport coords. Built by App from `vi::Search` state
/// before each frame and passed to `Renderer::prepare` so the cell loop
/// can highlight the cells inside the range. Both columns are inclusive.
pub struct MatchView {
    pub vrow: usize,
    pub col_start: usize,
    pub col_end: usize,
}

/// Search-bar + match-highlight bundle. App constructs this from the
/// vi-mode state; renderer doesn't need to know `vi::Search` exists.
pub struct SearchOverlay<'a> {
    pub prompt: char, // '/' for forward, '?' for backward
    pub query: &'a str,
    pub typing: bool,
    pub matches: Vec<MatchView>,
    /// Index into `matches` of the current selection (highlighted more
    /// prominently). `None` if nothing matched.
    pub current: Option<usize>,
}

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

impl PartialEq for CellInstance {
    fn eq(&self, other: &Self) -> bool {
        // Bytewise comparison — repr(C), no padding, deterministic.
        // Sidesteps f32's NaN != NaN rule (we never store NaN colors,
        // but explicit is better here than relying on absence).
        let a = unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        };
        let b = unsafe {
            std::slice::from_raw_parts(
                other as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        };
        a == b
    }
}

/// Convert a `[col_start..=col_end]` slice of viewport row `row` into an
/// EGL-space damage rect and push it onto `out`. EGL surface coordinates
/// place the origin at the bottom-left, so we invert the row index.
/// Caller is responsible for ensuring the row/col indices are in range.
fn push_row_damage(
    out: &mut Vec<Rect>,
    row: i32,
    col_start: i32,
    col_end: i32,
    cw: i32,
    ch: i32,
    screen_h: i32,
) {
    let x = col_start * cw;
    let width = (col_end - col_start + 1) * cw;
    let y = screen_h - (row + 1) * ch;
    out.push(Rect::new(x, y, width, ch));
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
    u_cursor_cell: glow::UniformLocation,
    u_cursor_shape: glow::UniformLocation,
    u_cursor_color: glow::UniformLocation,
    u_cursor_text: glow::UniformLocation,

    // Vi-mode tint: a separate program that draws a fullscreen quad with
    // a translucent color, alpha-blended onto the existing frame. Shares
    // no state with the main pipeline beyond the GL context.
    tint_program: glow::Program,
    tint_vao: glow::VertexArray,
    u_tint_color: glow::UniformLocation,

    palette: [[f32; 4]; 256],
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    cursor_bg: [f32; 4],
    cursor_fg: [f32; 4],

    screen_size: (u32, u32),
    cell_size: (u32, u32),
    atlas_size: (u32, u32),

    /// Cursor cell as (col, row) in viewport coords; (-1, -1) when the
    /// cursor isn't visible. Set in `prepare`, uploaded in `draw`.
    cursor_cell: (i32, i32),
    /// Encoded shape: 0 = none, 1 = block, 2 = underline, 3 = bar,
    /// 4 = hollow block (vi-mode). Mirrors the fragment shader constants.
    cursor_shape_id: i32,
    /// Cursor cell from the previous frame, so the per-row damage diff
    /// can mark the *old* cursor cell dirty even when the underlying
    /// CellInstances haven't changed.
    prev_cursor_cell: (i32, i32),

    // ====== Per-frame damage tracking for eglSwapBuffersWithDamageKHR ======
    //
    // The Wayland compositor's GPU cost during cmatrix-style workloads is
    // dominated by recomposing the whole surface every swap. By hinting the
    // changed regions per-frame (via glutin → EGL_KHR_swap_buffers_with_damage)
    // the compositor only re-blends those rectangles. NVIDIA 595 EGL on
    // Wayland honors the hint; if the driver doesn't, glutin silently falls
    // back to a full-surface swap so this is correctness-safe.
    //
    /// Last frame's grid instances — the *grid* portion only, not the
    /// search-bar or picker overlay. Indexed positionally; entry `i`
    /// corresponds to `instances_scratch[i]` from the previous prepare.
    last_grid_instances: Vec<CellInstance>,
    /// End-of-grid index in `instances_scratch` (exclusive). Overlays
    /// (search bar, picker) append after this point. Lets the damage
    /// diff stop before the overlay range, which we always full-damage.
    grid_instance_end: usize,
    /// Grid dimensions from the last successful damage computation. A
    /// row- or column-count change invalidates positional comparison
    /// against `last_grid_instances`, so we full-damage that frame.
    last_grid_dims: (usize, usize),
    /// Output damage list in EGL surface coordinates (origin bottom-left,
    /// per the EGL spec). Cleared and refilled each `prepare`. `Gpu::render`
    /// reads this and forwards it to `swap_buffers_with_damage`.
    pub damage_rects: Vec<Rect>,
}

impl Renderer {
    pub fn new(
        gl: &glow::Context,
        screen_size: (u32, u32),
        atlas: &mut FontAtlas,
        theme: &Theme,
    ) -> Self {
        let palette = build_palette(theme);
        let default_fg = srgb_to_linear_rgba_3(theme.fg);
        let default_bg = srgb_to_linear_rgba_3(theme.bg);
        let cursor_bg = srgb_to_linear_rgba_3(theme.cursor_bg);
        let cursor_fg = srgb_to_linear_rgba_3(theme.cursor_fg);

        let program = unsafe { build_program(gl) };
        let u = |name: &str| unsafe {
            gl.get_uniform_location(program, name)
                .unwrap_or_else(|| panic!("uniform {name} not found"))
        };
        let u_screen_size = u("u_screen_size");
        let u_cell_size = u("u_cell_size");
        let u_atlas_size = u("u_atlas_size");
        let u_grid_offset = u("u_grid_offset");
        let u_atlas = u("u_atlas");
        let u_cursor_cell = u("u_cursor_cell");
        let u_cursor_shape = u("u_cursor_shape");
        let u_cursor_color = u("u_cursor_color");
        let u_cursor_text = u("u_cursor_text");

        let tint_program = unsafe { build_tint_program(gl) };
        let u_tint_color = unsafe {
            gl.get_uniform_location(tint_program, "u_tint_color")
                .expect("tint uniform u_tint_color not found")
        };
        // Vertex-attributeless fullscreen quad — `gl_VertexID` picks each
        // corner — so we just need an empty VAO bound during the draw.
        let tint_vao = unsafe { gl.create_vertex_array().expect("create tint vao") };

        let (vao, vbo) = unsafe { build_vao_vbo(gl, INITIAL_INSTANCE_CAPACITY) };

        let atlas_tex = unsafe { build_atlas_texture(gl, atlas) };
        // The pre-baked ASCII glyphs already shipped via `tex_image_2d`
        // above; clear so the first `prepare()` doesn't re-upload them.
        atlas.dirty_rects.clear();

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
            u_cursor_cell,
            u_cursor_shape,
            u_cursor_color,
            u_cursor_text,
            tint_program,
            tint_vao,
            u_tint_color,
            palette,
            default_fg,
            default_bg,
            cursor_bg,
            cursor_fg,
            screen_size,
            cell_size: (atlas.metrics.cell_w, atlas.metrics.cell_h),
            atlas_size: (atlas.atlas_w, atlas.atlas_h),
            cursor_cell: (-1, -1),
            cursor_shape_id: 0,
            prev_cursor_cell: (-1, -1),
            last_grid_instances: Vec::new(),
            grid_instance_end: 0,
            last_grid_dims: (0, 0),
            damage_rects: Vec::new(),
        };
        unsafe { renderer.upload_uniforms(gl) };
        renderer
    }

    pub fn cell_size(&self) -> (u32, u32) {
        self.cell_size
    }

    /// Swap the active palette/cursor colors. The next frame's instance
    /// pack picks up the new colors automatically — no GPU reset needed.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.palette = build_palette(theme);
        self.default_fg = srgb_to_linear_rgba_3(theme.fg);
        self.default_bg = srgb_to_linear_rgba_3(theme.bg);
        self.cursor_bg = srgb_to_linear_rgba_3(theme.cursor_bg);
        self.cursor_fg = srgb_to_linear_rgba_3(theme.cursor_fg);
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
    /// One full upload is the right call here even though steady-state
    /// uploads are now per-rect: a fresh atlas allocator places glyphs
    /// at brand new positions, so the entire texture needs replacing.
    /// We then clear `dirty_rects` so the next `prepare()` doesn't
    /// redundantly re-upload the rects we just covered.
    pub fn reload_font(&mut self, gl: &glow::Context, atlas: &mut FontAtlas) {
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
        atlas.dirty_rects.clear();
        self.cell_size = (atlas.metrics.cell_w, atlas.metrics.cell_h);
        unsafe { self.upload_uniforms(gl) };
    }

    pub fn prepare(
        &mut self,
        gl: &glow::Context,
        term: &Term,
        atlas: &mut FontAtlas,
        picker: Option<&mut Picker>,
        selection: Option<&Selection>,
        cursor_visible_now: bool,
        vi_cursor: Option<(usize, usize)>,
        search: Option<&SearchOverlay<'_>>,
    ) {
        let rows = term.grid().rows;
        let cols = term.grid().cols;

        // Cursor info goes to the shader via uniforms; the fragment stage
        // overlays the chosen shape on top of the cursor cell. Keeping it
        // out of the per-cell instance pack means selection and SGR-inverse
        // compose normally and the cursor wins because the shader runs last.
        //
        // Vi-mode override: when `vi_cursor` is Some, we draw a hollow-block
        // cursor at that position and suppress the terminal cursor entirely.
        // Otherwise the normal terminal-cursor logic runs, with these
        // suppressions:
        //   - picker open → overlay would bleed through the modal, since
        //     the shader matches by cell_xy regardless of which instance
        //     is being drawn at that position;
        //   - blink off-phase → App passes cursor_visible_now=false for
        //     that frame.
        let cursor: Option<(usize, usize, CursorShape)> = match vi_cursor {
            Some((row, col)) => Some((row, col, CursorShape::HollowBlock)),
            None => term
                .viewport_cursor()
                .filter(|_| picker.is_none())
                .filter(|_| cursor_visible_now)
                .map(|(r, c)| (r, c, term.cursor_shape())),
        };
        match cursor {
            Some((row, col, shape)) => {
                self.cursor_cell = (col as i32, row as i32);
                self.cursor_shape_id = match shape {
                    CursorShape::Block => 1,
                    CursorShape::Underline => 2,
                    CursorShape::Bar => 3,
                    CursorShape::HollowBlock => 4,
                };
            }
            None => {
                self.cursor_cell = (-1, -1);
                self.cursor_shape_id = 0;
            }
        }

        // Single pass: ensure each cell's glyph in the atlas, then pack the
        // instance immediately. Same shape as the wgpu version was.
        self.instances_scratch.clear();
        for vrow in 0..rows {
            let row = term.viewport_row(vrow);
            for (col_idx, cell) in row.cells.iter().take(cols).enumerate() {
                // Spacer cells (the right half of a wide character)
                // don't get their own instance — the leading cell's
                // wide glyph already covers them via a second instance
                // emitted below.
                if cell.width == 0 {
                    continue;
                }
                let style = FontStyle::from_attrs(
                    cell.attrs.has(CellAttrs::BOLD),
                    cell.attrs.has(CellAttrs::ITALIC),
                );
                // Pull the glyph entry once: blank cells (' ' / '\0')
                // have no glyph and don't need a HashMap lookup —
                // gol-c-style workloads where most cells are spaces
                // hit this every frame, so skipping the lookup saves
                // real time. The shader's `glyph_size_px.x > 0` check
                // already handles the empty entry naturally.
                let glyph = if is_blank_glyph(cell.ch) {
                    Default::default()
                } else {
                    atlas.ensure(cell.ch, style);
                    atlas.get(cell.ch, style).unwrap_or_default()
                };
                let mut fg = resolve_color(cell.fg, &self.palette, self.default_fg);
                let mut bg = resolve_color(cell.bg, &self.palette, self.default_bg);
                if cell.attrs.has(CellAttrs::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                // Selection: classic xterm-style inverse highlighting.
                if let Some(sel) = selection {
                    if sel.contains(vrow, col_idx) {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                }
                // Search match highlight wins over both — it's a hard
                // override, not a tint, because we want the user to spot
                // matches at a glance even on a busy background.
                if let Some(ov) = search {
                    let mut hit: Option<bool> = None; // Some(is_current)
                    for (i, m) in ov.matches.iter().enumerate() {
                        if m.vrow == vrow && col_idx >= m.col_start && col_idx <= m.col_end
                        {
                            hit = Some(Some(i) == ov.current);
                            break;
                        }
                    }
                    match hit {
                        Some(true) => {
                            bg = MATCH_BG_CURRENT;
                            fg = MATCH_FG_CURRENT;
                        }
                        Some(false) => {
                            bg = MATCH_BG_OTHER;
                        }
                        None => {}
                    }
                }
                self.instances_scratch.push(CellInstance {
                    cell_xy: [col_idx as u32, vrow as u32],
                    glyph_origin: [glyph.atlas_x as u32, glyph.atlas_y as u32],
                    glyph_size: [glyph.w as u32, glyph.h as u32],
                    glyph_offset: [glyph.offset_x as i32, glyph.offset_y as i32],
                    fg,
                    bg,
                });
                // Wide leading: emit a second instance covering the
                // right half. The shifted glyph_offset makes the shader
                // sample the right half of the atlas glyph, so the two
                // instances together render the wide character across
                // both cells.
                if cell.width == 2 && col_idx + 1 < cols {
                    self.instances_scratch.push(CellInstance {
                        cell_xy: [(col_idx + 1) as u32, vrow as u32],
                        glyph_origin: [glyph.atlas_x as u32, glyph.atlas_y as u32],
                        glyph_size: [glyph.w as u32, glyph.h as u32],
                        glyph_offset: [
                            glyph.offset_x as i32 - self.cell_size.0 as i32,
                            glyph.offset_y as i32,
                        ],
                        fg,
                        bg,
                    });
                }
            }
        }

        // Mark the end of grid instances. Overlays go after this and
        // get full-window damage (their layout isn't a positional match
        // against last frame so per-row diff doesn't apply).
        self.grid_instance_end = self.instances_scratch.len();

        // Compute per-row damage rectangles for this frame's swap.
        // Always do this BEFORE the overlay appends — the diff only
        // considers grid cells, and we want to preserve the diff state
        // (last_grid_instances) across overlay activity.
        self.compute_damage_rects(rows, cols, search.is_some(), picker.is_some());

        // Search bar at the bottom row. After the cell loop so it
        // overwrites whatever's there. Before picker so an open picker
        // (which is centered) can still draw on top.
        if let Some(ov) = search {
            self.append_search_bar(ov, rows, cols, atlas);
        }

        // Picker overlay. Drawn AFTER grid instances, which works because
        // the GPU draws in order and our shader doesn't blend — later
        // instances overwrite earlier ones at the same pixel.
        if let Some(picker) = picker {
            self.append_picker_overlay(picker, rows, cols, atlas);
        }

        if !atlas.dirty_rects.is_empty() {
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
                // The source slice has the full atlas's row stride
                // (atlas_w bytes per row), but we're only uploading a
                // rect.w-wide block. UNPACK_ROW_LENGTH tells GL how
                // far apart consecutive source rows are.
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, atlas.atlas_w as i32);
                for rect in &atlas.dirty_rects {
                    let offset = (rect.y * atlas.atlas_w + rect.x) as usize;
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        rect.x as i32,
                        rect.y as i32,
                        rect.w as i32,
                        rect.h as i32,
                        glow::RED,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(&atlas.atlas_data[offset..]),
                    );
                }
                // Reset to default (0 == "use upload region's width").
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            }
            atlas.dirty_rects.clear();
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

    /// Build `self.damage_rects` for this frame's swap, then snapshot the
    /// current grid instances into `self.last_grid_instances` for next
    /// frame's comparison.
    ///
    /// Output is in EGL surface coordinates: origin bottom-left, X right,
    /// Y up. Caller passes the row/col grid dims so we can detect a
    /// resize-induced positional mismatch.
    fn compute_damage_rects(&mut self, rows: usize, cols: usize, search_active: bool, picker_active: bool) {
        self.damage_rects.clear();

        let cw = self.cell_size.0 as i32;
        let ch = self.cell_size.1 as i32;
        let sw = self.screen_size.0 as i32;
        let sh = self.screen_size.1 as i32;

        let grid = &self.instances_scratch[..self.grid_instance_end];
        let prev = self.last_grid_instances.as_slice();
        let dims_changed = self.last_grid_dims != (rows, cols);
        let layout_changed = grid.len() != prev.len();
        let overlay_active = search_active || picker_active;
        let cursor_moved = self.cursor_cell != self.prev_cursor_cell;

        // Full-window damage for any case we can't diff cleanly. Overlays
        // (picker/search bar) are drawn over arbitrary parts of the
        // surface and we don't track their extent here; vi-tint isn't
        // a parameter to this fn but the App's path force-redraws on
        // every vi state change so the worst case is one full-damaged
        // frame, not visual breakage. First-frame or resize: prev buffer
        // is empty or stale.
        if prev.is_empty() || dims_changed || layout_changed || overlay_active {
            self.damage_rects.push(Rect::new(0, 0, sw, sh));
            self.snapshot_for_next_frame(rows, cols);
            return;
        }

        // Walk the grid in row-major order, accumulating min/max col
        // that differs from the previous frame within each row. The
        // grid is emitted row-by-row so consecutive instances share
        // the same `cell_xy[1]` (row) value — a linear scan suffices.
        let mut i = 0;
        while i < grid.len() {
            let row = grid[i].cell_xy[1];
            let row_start = i;
            // Advance past all instances on this row.
            while i < grid.len() && grid[i].cell_xy[1] == row {
                i += 1;
            }
            let row_end = i;

            let mut min_col: i64 = -1;
            let mut max_col: i64 = -1;
            for k in row_start..row_end {
                if grid[k] != prev[k] {
                    let col = grid[k].cell_xy[0] as i64;
                    if min_col < 0 || col < min_col {
                        min_col = col;
                    }
                    if col > max_col {
                        max_col = col;
                    }
                }
            }
            if min_col >= 0 {
                push_row_damage(
                    &mut self.damage_rects,
                    row as i32,
                    min_col as i32,
                    max_col as i32,
                    cw,
                    ch,
                    sh,
                );
            }
        }

        // Cursor: drawn via uniforms, not instances, so its movement
        // doesn't show up in the diff. Damage the previous cursor cell
        // (which we're un-painting) and the new one (which we're
        // painting). Either can be (-1,-1) for "no cursor", in which
        // case there's nothing to add.
        if cursor_moved {
            for &(col, row) in &[self.prev_cursor_cell, self.cursor_cell] {
                if col >= 0 && row >= 0 && (col as usize) < cols && (row as usize) < rows {
                    push_row_damage(
                        &mut self.damage_rects,
                        row, col, col, cw, ch, sh,
                    );
                }
            }
        }

        // EGL_KHR_swap_buffers_with_damage with n_rects=0 is
        // implementation-defined: some drivers treat it as full damage,
        // some as no damage at all. We're called from the renderer for
        // a reason (App marked dirty), so leaving damage empty would
        // risk a stale window on the "no damage" interpretation. Push
        // a degenerate 1×1 rect at the origin so the swap is always
        // unambiguous — the compositor will recompose ~zero pixels.
        if self.damage_rects.is_empty() {
            self.damage_rects.push(Rect::new(0, 0, 1, 1));
        }

        self.snapshot_for_next_frame(rows, cols);
    }

    fn snapshot_for_next_frame(&mut self, rows: usize, cols: usize) {
        self.last_grid_instances.clear();
        self.last_grid_instances
            .extend_from_slice(&self.instances_scratch[..self.grid_instance_end]);
        self.last_grid_dims = (rows, cols);
        self.prev_cursor_cell = self.cursor_cell;
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
            // Cursor state changes per frame (movement, blink-ish toggles
            // when programs hide/show via DECTCEM, viewport scrolling).
            // Uploaded here rather than in `upload_uniforms` because that
            // path only runs on resize/font reload.
            gl.uniform_2_i32(
                Some(&self.u_cursor_cell),
                self.cursor_cell.0,
                self.cursor_cell.1,
            );
            gl.uniform_1_i32(Some(&self.u_cursor_shape), self.cursor_shape_id);
            gl.uniform_4_f32(
                Some(&self.u_cursor_color),
                self.cursor_bg[0],
                self.cursor_bg[1],
                self.cursor_bg[2],
                self.cursor_bg[3],
            );
            gl.uniform_4_f32(
                Some(&self.u_cursor_text),
                self.cursor_fg[0],
                self.cursor_fg[1],
                self.cursor_fg[2],
                self.cursor_fg[3],
            );
            gl.draw_arrays_instanced(glow::TRIANGLES, 0, 6, self.instance_count);
        }
    }

    /// Paint a translucent fullscreen tint on top of the already-drawn
    /// frame. Called after the main pass when vi-mode is active. The
    /// `color` is in linear-light RGBA — alpha controls the strength of
    /// the wash. Blending is enabled briefly here and disabled before
    /// returning so the cell pipeline sees its usual no-blend state.
    pub fn draw_tint(&self, gl: &glow::Context, color: [f32; 4]) {
        unsafe {
            gl.use_program(Some(self.tint_program));
            gl.bind_vertex_array(Some(self.tint_vao));
            gl.uniform_4_f32(
                Some(&self.u_tint_color),
                color[0],
                color[1],
                color[2],
                color[3],
            );
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            gl.disable(glow::BLEND);
        }
    }

    /// Render the search bar over the bottom row: a `/` or `?` prompt
    /// followed by the live query. Uses theme cursor colors so it reads
    /// against any palette without us picking literal RGB.
    fn append_search_bar(
        &mut self,
        overlay: &SearchOverlay<'_>,
        rows: usize,
        cols: usize,
        atlas: &mut FontAtlas,
    ) {
        if rows == 0 {
            return;
        }
        let bar_row = rows - 1;
        let bar_bg = self.cursor_bg;
        let bar_fg = self.cursor_fg;
        // Fill the row so old grid content underneath is hidden.
        for col in 0..cols {
            self.push_overlay_cell(' ', bar_row, col, bar_fg, bar_bg, atlas);
        }
        let mut col = 0usize;
        if col < cols {
            self.push_overlay_cell(overlay.prompt, bar_row, col, bar_fg, bar_bg, atlas);
            col += 1;
        }
        for ch in overlay.query.chars() {
            if col >= cols {
                break;
            }
            self.push_overlay_cell(ch, bar_row, col, bar_fg, bar_bg, atlas);
            col += 1;
        }
        // Caret: a small under-bar at the typing position so the user
        // sees where their input lands. Skipped post-commit (typing=false)
        // since the bar then just shows the committed query.
        if overlay.typing && col < cols {
            self.push_overlay_cell('_', bar_row, col, bar_fg, bar_bg, atlas);
        }
    }

    fn append_picker_overlay(
        &mut self,
        picker: &mut Picker,
        rows: usize,
        cols: usize,
        atlas: &mut FontAtlas,
    ) {
        let layout = picker.layout(rows, cols);
        let (origin_row, origin_col) = layout.origin;
        let (height, width) = layout.size;
        if width < 8 || height < 5 {
            return;
        }

        // Color scheme: invert page colors so the modal stands out.
        let panel_bg = self.cursor_bg; // bright
        let panel_fg = self.cursor_fg; // dark
        let header_bg = self.default_fg;
        let header_fg = self.default_bg;
        let highlight_bg = self.default_fg;
        let highlight_fg = self.default_bg;

        // Fill background.
        for r in 0..height {
            for c in 0..width {
                self.push_overlay_cell(' ', origin_row + r, origin_col + c, panel_fg, panel_bg, atlas);
            }
        }

        // Header text.
        let header = "Theme picker  ↑↓ navigate  Enter keep  Esc revert";
        let header_row = origin_row;
        write_string_clipped(
            &mut |ch, dr, dc| {
                self.push_overlay_cell(ch, dr, dc, header_fg, header_bg, atlas);
            },
            header,
            header_row,
            origin_col,
            width,
        );

        // List entries.
        let names = picker.themes();
        let cursor_idx = picker.cursor();
        let visible = layout.list_visible;
        let scroll = layout.scroll;
        for vis_row in 0..visible {
            let item_idx = scroll + vis_row;
            if item_idx >= names.len() {
                break;
            }
            let row_y = origin_row + 2 + vis_row;
            let is_selected = item_idx == cursor_idx;
            let (fg, bg) = if is_selected {
                (highlight_fg, highlight_bg)
            } else {
                (panel_fg, panel_bg)
            };
            // Pad with spaces to fill the row so highlight covers full width.
            for c in 0..width {
                self.push_overlay_cell(' ', row_y, origin_col + c, fg, bg, atlas);
            }
            let prefix = if is_selected { " ▶ " } else { "   " };
            let mut col = origin_col;
            write_string_clipped(
                &mut |ch, dr, dc| {
                    self.push_overlay_cell(ch, dr, dc, fg, bg, atlas);
                },
                prefix,
                row_y,
                col,
                width,
            );
            col += prefix.chars().count();
            write_string_clipped(
                &mut |ch, dr, dc| {
                    self.push_overlay_cell(ch, dr, dc, fg, bg, atlas);
                },
                &names[item_idx],
                row_y,
                col,
                origin_col + width - col,
            );
        }
    }

    fn push_overlay_cell(
        &mut self,
        ch: char,
        row: usize,
        col: usize,
        fg: [f32; 4],
        bg: [f32; 4],
        atlas: &mut FontAtlas,
    ) {
        // Picker / search bar / FPS overlays are always rendered in
        // regular weight — the underlying terminal cell's bold/italic
        // attrs don't propagate to overlay text.
        if ch != ' ' && ch != '\0' {
            atlas.ensure(ch, FontStyle::Regular);
        }
        let glyph = atlas.get(ch, FontStyle::Regular).unwrap_or_default();
        self.instances_scratch.push(CellInstance {
            cell_xy: [col as u32, row as u32],
            glyph_origin: [glyph.atlas_x as u32, glyph.atlas_y as u32],
            glyph_size: [glyph.w as u32, glyph.h as u32],
            glyph_offset: [glyph.offset_x as i32, glyph.offset_y as i32],
            fg,
            bg,
        });
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

/// Search-match highlight colors, pre-converted from sRGB to linear-light
/// for the sRGB framebuffer. Yellow/amber palette so matches read at a
/// glance against any theme background.
///
///   Other:  rgb(128, 104, 0)  — dim amber, bg only (keeps original fg)
///   Current bg: rgb(255, 215, 0)  — bright gold, paired with black fg
///                                   so the cell pops where the user is.
const MATCH_BG_OTHER: [f32; 4] = [0.216, 0.139, 0.0, 1.0];
const MATCH_BG_CURRENT: [f32; 4] = [1.0, 0.679, 0.0, 1.0];
const MATCH_FG_CURRENT: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

unsafe fn build_program(gl: &glow::Context) -> glow::Program {
    link_program(
        gl,
        include_str!("shader.vert"),
        include_str!("shader.frag"),
    )
}

unsafe fn build_tint_program(gl: &glow::Context) -> glow::Program {
    link_program(
        gl,
        include_str!("tint.vert"),
        include_str!("tint.frag"),
    )
}

unsafe fn link_program(gl: &glow::Context, vs_src: &str, fs_src: &str) -> glow::Program {
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
    program
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

/// Write `s` cell-by-cell starting at `(row, col)`, calling `emit` for each
/// character. Stops if the column would exceed `col + width`. Counts in chars,
/// not bytes — it's safe with multi-byte UTF-8 but treats each `char` as one
/// cell, which is wrong for full-width CJK but acceptable for picker UI.
fn write_string_clipped(
    emit: &mut dyn FnMut(char, usize, usize),
    s: &str,
    row: usize,
    col_start: usize,
    width: usize,
) {
    for (i, ch) in s.chars().enumerate() {
        if i >= width {
            break;
        }
        emit(ch, row, col_start + i);
    }
}

fn resolve_color(c: Color, palette: &[[f32; 4]; 256], default: [f32; 4]) -> [f32; 4] {
    match c {
        Color::Default => default,
        Color::Indexed(i) => palette[i as usize],
        Color::Rgb(r, g, b) => srgb_to_linear_rgba([r, g, b, 255]),
    }
}

/// sRGB→linear conversion. The math is `if s <= 0.04045 { s/12.92 }
/// else { ((s+0.055)/1.055).powf(2.4) }` over `s = c / 255`. `powf`
/// is ~150 cycles per call, and we hit this four times per truecolor
/// cell in a hot render loop (gol-c full-screen at 100 Hz = 30k+
/// calls per frame). Precompute the 256 outputs once at process
/// startup so the per-cell cost collapses to a bounds-checked array
/// load.
static SRGB_LUT: std::sync::LazyLock<[f32; 256]> = std::sync::LazyLock::new(|| {
    let mut t = [0.0f32; 256];
    for (i, slot) in t.iter_mut().enumerate() {
        let s = i as f32 / 255.0;
        *slot = if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        };
    }
    t
});

fn srgb_to_linear(c: u8) -> f32 {
    SRGB_LUT[c as usize]
}

fn srgb_to_linear_rgba([r, g, b, a]: [u8; 4]) -> [f32; 4] {
    [
        srgb_to_linear(r),
        srgb_to_linear(g),
        srgb_to_linear(b),
        a as f32 / 255.0,
    ]
}

fn srgb_to_linear_rgba_3([r, g, b]: [u8; 3]) -> [f32; 4] {
    srgb_to_linear_rgba([r, g, b, 255])
}

/// Build the full 256-color palette from the theme's 16-color base.
/// Indices 0..=15 come straight from the theme. 16..=231 are the standard
/// xterm 6×6×6 cube, 232..=255 the 24-step grayscale ramp — both
/// derivative formulas, not theme-specific.
fn build_palette(theme: &Theme) -> [[f32; 4]; 256] {
    let mut out = [[0.0; 4]; 256];
    for (i, rgb) in theme.palette.iter().enumerate() {
        out[i] = srgb_to_linear_rgba_3(*rgb);
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

#[cfg(test)]
mod tests {
    #[test]
    fn srgb_lut_matches_formula() {
        // The LUT replaces a per-call powf in the per-cell render
        // hot path. Verify every entry agrees with the original
        // formula to within a tolerance that matches single-precision
        // float roundoff — a regression here would silently shift
        // truecolor rendering.
        for c in 0u8..=255 {
            let s = c as f32 / 255.0;
            let expected = if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            };
            let lut = super::srgb_to_linear(c);
            assert!(
                (lut - expected).abs() < 1e-6,
                "c={c} expected={expected} lut={lut}"
            );
        }
    }
}
