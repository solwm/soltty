#version 330 core

in vec2 v_cell_local;
in vec2 v_glyph_origin_px;
in vec2 v_glyph_size_px;
in vec2 v_glyph_offset_px;
in vec4 v_fg;
in vec4 v_bg;

uniform sampler2D u_atlas;
uniform vec2 u_atlas_size;

out vec4 frag_color;

void main() {
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
    frag_color = v_bg * (1.0 - alpha) + v_fg * alpha;
}
