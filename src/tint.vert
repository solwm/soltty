#version 330 core

// Fullscreen quad for the vi-mode tint pass. Six vertices form a NDC-space
// rectangle covering [-1, 1] x [-1, 1]; the fragment stage paints every
// pixel with a single translucent color, blended onto the existing frame.

const vec2 CORNERS[6] = vec2[6](
    vec2(-1.0, -1.0), vec2( 1.0, -1.0), vec2(-1.0,  1.0),
    vec2(-1.0,  1.0), vec2( 1.0, -1.0), vec2( 1.0,  1.0)
);

void main() {
    gl_Position = vec4(CORNERS[gl_VertexID], 0.0, 1.0);
}
