// Retained map line shader.
//
// Vertices are world kilometres plus a world-space perpendicular. The camera
// matrix is applied to the position, while the normal is transformed by the
// matrix's linear part and then renormalised, which strips the zoom and leaves
// a pure screen direction. Offsetting along that direction by a pixel width
// gives strokes that stay the same thickness at every scale without ever
// rewriting a vertex.

struct MapUniform {
    world_to_clip: mat4x4<f32>,
    viewport_px: vec2<f32>,
    pixels_per_point: f32,
    globe_blend: f32,
};

// GRS80/WGS84 mean radius, kilometres (Moritz 2000). Must match
// `globe::EARTH_MEAN_RADIUS_KM`.
const EARTH_MEAN_RADIUS_KM: f32 = 6371.0088;
// Must match `globe::LIMB_FADE_RAD`.
const LIMB_FADE_RAD: f32 = 0.05;

fn globe_horizon(blend: f32) -> f32 {
    if (blend <= 0.5) {
        return 3.14159265;
    }
    return acos(-(1.0 - blend) / blend);
}

fn to_globe(position_km: vec2<f32>, blend: f32) -> vec2<f32> {
    if (blend == 0.0) {
        return position_km;
    }
    let radius = length(position_km);
    if (radius < 1e-4) {
        return position_km;
    }
    let c = radius / EARTH_MEAN_RADIUS_KM;
    let cc = min(c, globe_horizon(blend));
    return position_km * (((1.0 - blend) * cc + blend * sin(cc)) / c);
}

fn to_globe_normal(position_km: vec2<f32>, normal: vec2<f32>, blend: f32) -> vec2<f32> {
    if (blend == 0.0) {
        return normal;
    }
    let radius = length(position_km);
    if (radius < 1e-4) {
        return normal;
    }
    let c = radius / EARTH_MEAN_RADIUS_KM;
    let cc = min(c, globe_horizon(blend));
    let radial = (1.0 - blend) + blend * cos(cc);
    let tangential = ((1.0 - blend) * cc + blend * sin(cc)) / c;
    let u = position_km / radius;
    let v = vec2<f32>(-u.y, u.x);
    let tangent = vec2<f32>(normal.y, -normal.x);
    let moved = u * (dot(tangent, u) * radial) + v * (dot(tangent, v) * tangential);
    let unit = moved / max(length(moved), 1e-8);
    return vec2<f32>(-unit.y, unit.x);
}

fn limb_fade(position_km: vec2<f32>, blend: f32) -> f32 {
    if (blend <= 0.5) {
        return 1.0;
    }
    let horizon = globe_horizon(blend);
    let c = min(length(position_km) / EARTH_MEAN_RADIUS_KM, horizon);
    return clamp((horizon - c) / LIMB_FADE_RAD, 0.0, 1.0);
}

@group(0) @binding(0) var<uniform> uniforms: MapUniform;

struct VertexInput {
    @location(0) position_km: vec2<f32>,
    @location(1) normal: vec2<f32>,
    @location(2) half_width_px: f32,
    @location(3) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) limb_fade: f32,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;

    let world_km = to_globe(input.position_km, uniforms.globe_blend);
    let world_normal = to_globe_normal(input.position_km, input.normal, uniforms.globe_blend);
    let clip = uniforms.world_to_clip * vec4<f32>(world_km, 0.0, 1.0);

    // Linear part of the camera transform applied to the perpendicular.
    let rotated = (uniforms.world_to_clip * vec4<f32>(world_normal, 0.0, 0.0)).xy;
    let length = max(length(rotated), 1e-8);
    let direction = rotated / length;

    // Pixels -> clip units. The clip cube spans 2 units across the viewport.
    let half_width = max(input.half_width_px * uniforms.pixels_per_point, 0.5);
    let offset = direction * half_width * 2.0 / max(uniforms.viewport_px, vec2<f32>(1.0, 1.0));

    output.clip_position = vec4<f32>(clip.xy + offset * clip.w, clip.z, clip.w);
    output.color = input.color;
    output.limb_fade = limb_fade(input.position_km, uniforms.globe_blend);
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // Straight alpha in, premultiplied out, matching the blend state.
    let alpha = input.color.a * input.limb_fade;
    return vec4<f32>(input.color.rgb * alpha, alpha);
}
