struct Uniforms {
    screen_size: vec2<f32>,
    cell_size: vec2<f32>,
    atlas_size: vec2<f32>,
    grid_offset: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) cell_local: vec2<f32>,
    @location(1) glyph_origin_px: vec2<f32>,
    @location(2) glyph_size_px: vec2<f32>,
    @location(3) glyph_offset_px: vec2<f32>,
    @location(4) fg: vec4<f32>,
    @location(5) bg: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vid: u32,
    @location(0) cell_xy: vec2<u32>,
    @location(1) glyph_origin: vec2<u32>,
    @location(2) glyph_size: vec2<u32>,
    @location(3) glyph_offset: vec2<i32>,
    @location(4) fg: vec4<f32>,
    @location(5) bg: vec4<f32>,
) -> VsOut {
    // Two triangles forming a rect: (0,0) (1,0) (0,1) | (0,1) (1,0) (1,1)
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let corner = corners[vid];
    let cell_origin = u.grid_offset
        + vec2<f32>(f32(cell_xy.x), f32(cell_xy.y)) * u.cell_size;
    let pos_px = cell_origin + corner * u.cell_size;
    let ndc = vec2<f32>(
         pos_px.x / u.screen_size.x * 2.0 - 1.0,
        -(pos_px.y / u.screen_size.y * 2.0 - 1.0),
    );
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.cell_local = corner * u.cell_size;
    out.glyph_origin_px = vec2<f32>(f32(glyph_origin.x), f32(glyph_origin.y));
    out.glyph_size_px = vec2<f32>(f32(glyph_size.x), f32(glyph_size.y));
    out.glyph_offset_px = vec2<f32>(f32(glyph_offset.x), f32(glyph_offset.y));
    out.fg = fg;
    out.bg = bg;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let glyph_local = in.cell_local - in.glyph_offset_px;
    var alpha: f32 = 0.0;
    if (in.glyph_size_px.x > 0.0
        && glyph_local.x >= 0.0 && glyph_local.x < in.glyph_size_px.x
        && glyph_local.y >= 0.0 && glyph_local.y < in.glyph_size_px.y) {
        let uv = (in.glyph_origin_px + glyph_local) / u.atlas_size;
        alpha = textureSample(atlas_tex, atlas_samp, uv).r;
    }
    // Cells don't overlap, so a manual mix is enough — no alpha blending state.
    return in.bg * (1.0 - alpha) + in.fg * alpha;
}
