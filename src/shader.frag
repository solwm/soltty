#version 330 core

in vec2 v_cell_local;
in vec2 v_glyph_origin_px;
in vec2 v_glyph_size_px;
in vec2 v_glyph_offset_px;
in vec4 v_fg;
in vec4 v_bg;
flat in ivec2 v_cell_xy;

uniform sampler2D u_atlas;
uniform vec2 u_atlas_size;
// Shared with the vertex stage — same uniform, same location, no extra
// CPU upload needed.
uniform vec2 u_cell_size;

// Cursor overlay. Cell is (col, row). Negative values mean "no cursor"
// (DECTCEM-hidden, scrolled out of view, or window unfocused — set by
// the renderer).
uniform ivec2 u_cursor_cell;
// 0 = none, 1 = block, 2 = underline, 3 = bar, 4 = hollow block.
// Matches CursorShape on the CPU side.
uniform int u_cursor_shape;
uniform vec4 u_cursor_color; // body color (theme cursor_bg)
uniform vec4 u_cursor_text;  // glyph color when block-overlaid (theme cursor_fg)

out vec4 frag_color;

void main() {
    bool on_cursor = u_cursor_shape != 0
        && v_cell_xy.x == u_cursor_cell.x
        && v_cell_xy.y == u_cursor_cell.y;

    // For block, override the cell's bg/fg so the glyph appears "punched out"
    // in the cursor color. Underline and bar leave the cell's normal colors
    // alone and overlay a band on top.
    vec4 bg = v_bg;
    vec4 fg = v_fg;
    if (on_cursor && u_cursor_shape == 1) {
        bg = u_cursor_color;
        fg = u_cursor_text;
    }

    vec2 glyph_local = v_cell_local - v_glyph_offset_px;
    float alpha = 0.0;
    if (v_glyph_size_px.x > 0.0
        && glyph_local.x >= 0.0 && glyph_local.x < v_glyph_size_px.x
        && glyph_local.y >= 0.0 && glyph_local.y < v_glyph_size_px.y) {
        vec2 uv = (v_glyph_origin_px + glyph_local) / u_atlas_size;
        alpha = texture(u_atlas, uv).r;
    }
    // Cells don't overlap, so we composite manually instead of using GL blending.
    // GL_FRAMEBUFFER_SRGB is enabled at the GL level so the framebuffer
    // gamma-encodes our linear-light output.
    vec4 color = bg * (1.0 - alpha) + fg * alpha;

    if (on_cursor) {
        if (u_cursor_shape == 2) {
            // Underline: ~10% of cell height, minimum 2 px so it stays
            // visible at small font sizes.
            float thickness = max(2.0, u_cell_size.y * 0.10);
            if (v_cell_local.y >= u_cell_size.y - thickness) {
                color = u_cursor_color;
            }
        } else if (u_cursor_shape == 3) {
            // Bar: ~15% of cell width on the left edge, minimum 2 px.
            float thickness = max(2.0, u_cell_size.x * 0.15);
            if (v_cell_local.x < thickness) {
                color = u_cursor_color;
            }
        } else if (u_cursor_shape == 4) {
            // Hollow block: 1-2 px outline along all four cell edges. Used
            // for the vi-mode cursor so it reads as "the other cursor"
            // without obscuring the glyph it's parked on.
            float thickness = max(1.0, min(u_cell_size.x, u_cell_size.y) * 0.06);
            vec2 d = min(v_cell_local, u_cell_size - v_cell_local);
            if (d.x < thickness || d.y < thickness) {
                color = u_cursor_color;
            }
        }
    }

    frag_color = color;
}
