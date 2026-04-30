#version 330 core

// Vi-mode "you are here" indicator. A solid color with non-zero alpha,
// alpha-blended onto the already-drawn terminal frame. Caller enables
// GL_BLEND with src_alpha / one_minus_src_alpha around the draw call.

uniform vec4 u_tint_color;

out vec4 frag_color;

void main() {
    frag_color = u_tint_color;
}
