// Raster tile basemap shader.
//
// A tile is an axis-aligned rectangle in Web Mercator (EPSG:3857) and a curved
// quadrilateral in the radar-local azimuthal-equidistant frame, so the CPU
// hands this an adaptively subdivided grid whose vertices are already in world
// kilometres. Nothing here reprojects: the vertex stage applies the same
// world-to-clip matrix the vector map uses, so imagery and boundaries move as
// one picture under a pan.
//
// Colour space matches `shader.wgsl` and egui deliberately. egui uploads its
// textures as `Rgba8Unorm` and works in gamma (code value) space, converting
// only at the framebuffer; tile textures are uploaded the same way and sampled
// the same way, so the imagery, the vector ink and the radar raster all
// composite in one space. An `Rgba8UnormSrgb` texture here would make the
// imagery the only thing in the pane whose brightness depended on the surface
// format.

struct TilePaneUniform {
    world_to_clip: mat4x4<f32>,
    // rgb + straight alpha. Mixed into the sampled texel rather than drawn as
    // a separate full-pane quad, so it dims the imagery and nothing else.
    scrim: vec4<f32>,
};

struct TileDrawUniform {
    // [u_offset, v_offset, u_scale, v_scale]: the window of the sampled
    // texture this tile occupies. Identity when the tile has its own texture,
    // a quarter/sixteenth/... when an ancestor is standing in for it.
    uv_offset_scale: vec4<f32>,
    // rgb multiplier, a = per-tile fade.
    tint: vec4<f32>,
};

@group(0) @binding(0) var<uniform> pane: TilePaneUniform;
@group(1) @binding(0) var<uniform> tile: TileDrawUniform;
@group(2) @binding(0) var tile_texture: texture_2d<f32>;
@group(2) @binding(1) var tile_sampler: sampler;

struct VertexInput {
    @location(0) position_km: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = pane.world_to_clip * vec4<f32>(input.position_km, 0.0, 1.0);
    output.uv = tile.uv_offset_scale.xy + input.uv * tile.uv_offset_scale.zw;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(tile_texture, tile_sampler, input.uv);
    // The imagery, dimmed towards the pane's own ground so weak reflectivity
    // stays readable on top of an aerial photograph.
    let ground = mix(texel.rgb * tile.tint.rgb, pane.scrim.rgb, pane.scrim.a);
    // Straight alpha in, premultiplied out, matching the blend state and the
    // vector map's fragment stage. The ArcGIS-style caches these providers use
    // are `format: MIXED`, so PNG tiles with transparent no-data really do
    // occur and would fringe without this.
    let alpha = texel.a * tile.tint.a;
    return vec4<f32>(ground * alpha, alpha);
}
