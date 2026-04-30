#version 330 core

// Per-instance attributes — one instance per cell.
layout(location = 0) in uvec2 a_cell_xy;
layout(location = 1) in uvec2 a_glyph_origin;
layout(location = 2) in uvec2 a_glyph_size;
layout(location = 3) in ivec2 a_glyph_offset;
layout(location = 4) in vec4 a_fg;
layout(location = 5) in vec4 a_bg;

uniform vec2 u_screen_size;
uniform vec2 u_cell_size;
uniform vec2 u_atlas_size;
uniform vec2 u_grid_offset;

out vec2 v_cell_local;
out vec2 v_glyph_origin_px;
out vec2 v_glyph_size_px;
out vec2 v_glyph_offset_px;
out vec4 v_fg;
out vec4 v_bg;
// `flat` because integer varyings have no defined interpolation in GLSL.
// All six vertices of a cell quad get the same value, which is what we want
// for the cursor-cell match in the fragment shader.
flat out ivec2 v_cell_xy;

const vec2 CORNERS[6] = vec2[6](
    vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
    vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0)
);

void main() {
    vec2 corner = CORNERS[gl_VertexID];
    vec2 cell_origin = u_grid_offset + vec2(a_cell_xy) * u_cell_size;
    vec2 pos_px = cell_origin + corner * u_cell_size;
    vec2 ndc = vec2(
         pos_px.x / u_screen_size.x * 2.0 - 1.0,
        -(pos_px.y / u_screen_size.y * 2.0 - 1.0)
    );
    gl_Position = vec4(ndc, 0.0, 1.0);

    v_cell_local       = corner * u_cell_size;
    v_glyph_origin_px  = vec2(a_glyph_origin);
    v_glyph_size_px    = vec2(a_glyph_size);
    v_glyph_offset_px  = vec2(a_glyph_offset);
    v_fg               = a_fg;
    v_bg               = a_bg;
    v_cell_xy          = ivec2(a_cell_xy);
}
