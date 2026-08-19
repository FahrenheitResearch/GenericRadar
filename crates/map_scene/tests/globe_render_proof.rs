//! The far-zoom globe, rendered on a real GPU and looked at.
//!
//! Every frame here comes out of the same `MapRenderResources` pipeline the
//! application paints with, over geometry built by the real `build_geometry`
//! from the real shipped basemap, with the real site catalogue drawn on top
//! through the same `Camera2D::world_to_screen` the pane uses.
//!
//! The globe morph is not in `shader.wgsl` yet - it arrives as an integration
//! note - so this file carries the proposed WGSL and its own pipeline. That is
//! only trustworthy if the proposed pipeline is the shipped one plus the
//! morph, so [`the_morph_pipeline_at_zero_blend_is_the_shipped_pipeline`]
//! renders the same camera through both and demands the two frames be equal
//! byte for byte. Everything else in this file rests on that.
//!
//! `MAP_SCENE_GLOBE_PROOF_OUT` chooses where the frames are written. Run it:
//!
//! ```text
//! cargo test --release -p map_scene --test globe_render_proof -- \
//!     --ignored --nocapture --test-threads=1
//! ```

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use analyst_runtime::{
    Camera2D, Generation, GeometryCacheKey, LodBucket, LodSelector, ScreenPoint, ViewportMetrics,
    WorldPoint,
};
use eframe::egui;
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;
use eframe::wgpu::util::DeviceExt;
use map_scene::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
use map_scene::dataset::MapDataset;
use map_scene::geometry::MapGeometry;
use map_scene::gpu::{MapPaintCallback, MapRenderResources, MapUniform};
use map_scene::projection::RadarProjection;
use map_scene::projection::globe;
use map_scene::style_presets::MapStylePreset;

/// A real pane on a 1600x900 window. 1600 * 4 bytes is 25 * 256, so the
/// read-back needs no row padding.
const WIDTH: u32 = 1600;
const HEIGHT: u32 = 900;

/// KTLX, from the live catalogue.
const KTLX: (f64, f64) = (35.333_049_774_169_92, -97.277_748_107_910_16);

/// RODN, Kadena, from the same catalogue. Its antipode is southern Brazil, so
/// this is the anchor where the far hemisphere has geography ON it - the case
/// a CONUS anchor cannot exercise, because the antipode of Oklahoma is empty
/// ocean.
const RODN: (f64, f64) = (26.302_000_045_776_367, 127.909_004_211_425_78);

/// The live site catalogue the application downloads and caches.
const SITE_CATALOGUE: &str = concat!(
    env!("LOCALAPPDATA"),
    "\\FahrenheitResearch\\RadarWorkstation\\cache\\radar-sites.tsv"
);

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

/// The proposed vertex morph, as it would read in `shader.wgsl`.
///
/// The first four items are the shipped shader, transcribed. The rest is the
/// integration note. `LIMB_MODE` is replaced before compilation so the same
/// source can render the shipped map, the smearing version and the fixed one,
/// which is what makes the comparison a comparison.
const MORPH_SHADER: &str = r#"
struct MapUniform {
    world_to_clip: mat4x4<f32>,
    viewport_px: vec2<f32>,
    pixels_per_point: f32,
    globe_blend: f32,
};

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

const EARTH_MEAN_RADIUS_KM: f32 = 6371.0088;
const LIMB_FADE_RAD: f32 = LIMB_FADE_VALUE;

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

// How much of this vertex survives the limb. The angular distance is CLAMPED
// to the horizon before it is interpolated, exactly as the position is, so a
// segment with both ends behind the limb carries fade 0 along its whole length
// and disappears instead of smearing along the rim.
fn limb_fade(position_km: vec2<f32>, blend: f32) -> f32 {
    // A fade width of zero is the CONTROL: the clamp with no fade at all,
    // which is the shape the previous proposal had.
    if (LIMB_FADE_RAD <= 0.0 || blend <= 0.5) {
        return 1.0;
    }
    let horizon = globe_horizon(blend);
    let c = min(length(position_km) / EARTH_MEAN_RADIUS_KM, horizon);
    return clamp((horizon - c) / LIMB_FADE_RAD, 0.0, 1.0);
}

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
"#;

/// Fade width in radians of angular distance. `0.0` reproduces the
/// smear-on-the-rim behaviour of a clamp with no fade at all, because
/// `clamp(x / 0, 0, 1)` is 1 for every positive `x`.
fn shader_source(limb_fade_rad: f32) -> String {
    MORPH_SHADER.replace("LIMB_FADE_VALUE", &format!("{limb_fade_rad:?}"))
}

struct MorphPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl MorphPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, limb_fade_rad: f32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("globe proof shader"),
            source: wgpu::ShaderSource::Wgsl(shader_source(limb_fade_rad).into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globe proof bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("globe proof pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("globe proof pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 8,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32,
                            offset: 16,
                            shader_location: 2,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Unorm8x4,
                            offset: 20,
                            shader_location: 3,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globe proof uniform"),
            size: std::mem::size_of::<MapUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globe proof bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniform,
            bind_group,
        }
    }
}

/// The shipped `MapPaintCallback::uniform`, transcribed. It is `pub(crate)`,
/// so this is the only way to build the same matrix from outside the crate -
/// and the zero-blend frame comparison is what proves the transcription.
///
/// The result is a raw `[f32; 20]` rather than a `MapUniform`, deliberately.
/// The integration note renames that block's fourth scalar from `_pad` to
/// `globe_blend`, and a test that named either one would stop compiling the
/// moment the note landed - or the moment it was reverted. What the test
/// actually depends on is the LAYOUT, and that is asserted against the shipped
/// type instead.
fn uniform_for(camera: Camera2D, viewport: ViewportMetrics, blend: f32) -> [f32; 20] {
    let camera = camera.sanitized();
    let viewport = viewport.sanitized();
    let width_px = WIDTH as f32;
    let height_px = HEIGHT as f32;
    let scale = 1.0 / camera.km_per_point.max(f32::MIN_POSITIVE);
    let (sin, cos) = camera.rotation_rad.sin_cos();
    let half_width_points = width_px / viewport.pixels_per_point * 0.5;
    let half_height_points = height_px / viewport.pixels_per_point * 0.5;
    let sx = scale / half_width_points;
    let sy = scale / half_height_points;
    let cx = camera.center_east_km as f32;
    let cy = camera.center_north_km as f32;
    let m00 = cos * sx;
    let m10 = sin * sx;
    let m01 = -sin * sy;
    let m11 = cos * sy;
    let tx = -(m00 * cx + m01 * cy);
    let ty = -(m10 * cx + m11 * cy);
    // The shipped block, scalar for scalar: four matrix columns, the viewport,
    // the device pixel ratio, and the slot the note turns into `globe_blend`.
    assert_eq!(
        std::mem::size_of::<MapUniform>(),
        std::mem::size_of::<[f32; 20]>(),
        "the uniform block changed shape, so this transcription is stale"
    );
    [
        m00,
        m10,
        0.0,
        0.0, //
        m01,
        m11,
        0.0,
        0.0, //
        0.0,
        0.0,
        1.0,
        0.0, //
        tx,
        ty,
        0.0,
        1.0, //
        width_px,
        height_px,
        viewport.pixels_per_point,
        blend,
    ]
}

struct Harness {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    resources: CallbackResources,
    target: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
}

impl Harness {
    fn new() -> Option<Self> {
        let instance = wgpu::Instance::default();
        let adapter =
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        println!("adapter: {:?}", adapter.get_info());
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("globe proof device"),
            ..Default::default()
        }))
        .expect("headless device");
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("globe proof target"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globe proof readback"),
            size: u64::from(WIDTH) * u64::from(HEIGHT) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut resources = CallbackResources::default();
        resources.insert(MapRenderResources::new(&device, format));
        Some(Self {
            device,
            queue,
            format,
            resources,
            target,
            view,
            readback,
        })
    }

    fn read_back(&self, mut encoder: wgpu::CommandEncoder) -> Vec<u8> {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(WIDTH * 4),
                    rows_per_image: Some(HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = self.readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll");
        receiver.recv().expect("map callback").expect("map read");
        let pixels = slice.get_mapped_range().to_vec();
        self.readback.unmap();
        pixels
    }

    /// One frame through the SHIPPED `MapPaintCallback`, exactly as the pane
    /// paints it today.
    fn shipped_frame(
        &mut self,
        geometry: Arc<MapGeometry>,
        camera: Camera2D,
        canvas: [f64; 4],
    ) -> Vec<u8> {
        let viewport = viewport();
        let rect_px = [0.0, 0.0, WIDTH as f32, HEIGHT as f32];
        let screen = ScreenDescriptor {
            size_in_pixels: [WIDTH, HEIGHT],
            pixels_per_point: 1.0,
        };
        let callback = MapPaintCallback {
            pane_index: 0,
            geometry,
            camera,
            viewport,
            rect_px,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("globe proof shipped"),
            });
        let extra = callback.prepare(
            &self.device,
            &self.queue,
            &screen,
            &mut encoder,
            &mut self.resources,
        );
        assert!(extra.is_empty());
        {
            let mut pass = begin(&mut encoder, &self.view, canvas);
            let whole = egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(WIDTH as f32, HEIGHT as f32),
            );
            let info = egui::PaintCallbackInfo {
                viewport: whole,
                clip_rect: whole,
                pixels_per_point: 1.0,
                screen_size_px: [WIDTH, HEIGHT],
            };
            let pixels = info.viewport_in_pixels();
            pass.set_viewport(
                pixels.left_px as f32,
                pixels.top_px as f32,
                pixels.width_px as f32,
                pixels.height_px as f32,
                0.0,
                1.0,
            );
            callback.paint(info, &mut pass, &self.resources);
        }
        self.read_back(encoder)
    }

    /// One frame through the proposed morph pipeline.
    fn morph_frame(
        &mut self,
        pipeline: &MorphPipeline,
        geometry: &MapGeometry,
        camera: Camera2D,
        blend: f32,
        canvas: [f64; 4],
    ) -> Vec<u8> {
        self.morph_frame_indexed(pipeline, geometry, &geometry.indices, camera, blend, canvas)
    }

    /// The same frame with an explicit index list, so a caller can leave
    /// triangles out and compare.
    #[allow(clippy::too_many_arguments)]
    fn morph_frame_indexed(
        &mut self,
        pipeline: &MorphPipeline,
        geometry: &MapGeometry,
        indices: &[u32],
        camera: Camera2D,
        blend: f32,
        canvas: [f64; 4],
    ) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(geometry.vertices.len() * 24);
        for vertex in geometry.vertices.iter() {
            bytes.extend_from_slice(&vertex.position_km[0].to_le_bytes());
            bytes.extend_from_slice(&vertex.position_km[1].to_le_bytes());
            bytes.extend_from_slice(&vertex.normal[0].to_le_bytes());
            bytes.extend_from_slice(&vertex.normal[1].to_le_bytes());
            bytes.extend_from_slice(&vertex.half_width_px.to_le_bytes());
            bytes.extend_from_slice(&vertex.color);
        }
        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("globe proof vertices"),
                contents: &bytes,
                usage: wgpu::BufferUsages::VERTEX,
            });
        let mut index_bytes = Vec::with_capacity(indices.len() * 4);
        for index in indices {
            index_bytes.extend_from_slice(&index.to_le_bytes());
        }
        let index_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("globe proof indices"),
                contents: &index_bytes,
                usage: wgpu::BufferUsages::INDEX,
            });
        let uniform = uniform_for(camera, viewport(), blend);
        self.queue
            .write_buffer(&pipeline.uniform, 0, bytemuck::cast_slice(&uniform));
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("globe proof morph"),
            });
        {
            let mut pass = begin(&mut encoder, &self.view, canvas);
            pass.set_viewport(0.0, 0.0, WIDTH as f32, HEIGHT as f32, 0.0, 1.0);
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &pipeline.bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
        }
        self.read_back(encoder)
    }
}

fn begin<'a>(
    encoder: &'a mut wgpu::CommandEncoder,
    view: &'a wgpu::TextureView,
    canvas: [f64; 4],
) -> wgpu::RenderPass<'static> {
    encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("globe proof pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: canvas[0],
                        g: canvas[1],
                        b: canvas[2],
                        a: canvas[3],
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        })
        .forget_lifetime()
}

fn viewport() -> ViewportMetrics {
    ViewportMetrics {
        width_points: WIDTH as f32,
        height_points: HEIGHT as f32,
        pixels_per_point: 1.0,
    }
}

fn camera_at(km_per_point: f32) -> Camera2D {
    Camera2D {
        center_east_km: 0.0,
        center_north_km: 0.0,
        km_per_point,
        rotation_rad: 0.0,
    }
}

fn geometry_at(km_per_point: f32) -> Arc<MapGeometry> {
    let lod = LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
    geometry_for_bucket(lod, KTLX)
}

fn geometry_for_bucket(lod: LodBucket, anchor: (f64, f64)) -> Arc<MapGeometry> {
    Arc::new(build_geometry(&MapBuildRequest {
        key: GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod,
        },
        dataset: MapDataset::from_generated(Generation::new(1)),
        projection: RadarProjection::new(anchor.0, anchor.1),
        style: MapStylePreset::Slate.style(),
    }))
}

/// The REAL site catalogue, as the application cached it.
fn real_sites() -> Vec<(String, f64, f64)> {
    let text = std::fs::read_to_string(SITE_CATALOGUE)
        .unwrap_or_else(|error| panic!("the real site catalogue at {SITE_CATALOGUE}: {error}"));
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.to_owned();
            let lat = fields.next()?.parse().ok()?;
            let lon = fields.next()?.parse().ok()?;
            Some((id, lat, lon))
        })
        .collect()
}

/// Draw the site markers into the read-back frame the way `draw_radar_sites`
/// draws them: `world_to_screen` on the placed world position, a small box,
/// and nothing else. `blend` is the globe blend the pane is drawn under.
fn draw_sites(pixels: &mut [u8], camera: Camera2D, blend: f32, color: [u8; 3]) -> usize {
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let viewport = viewport().sanitized();
    let mut drawn = 0;
    for (_, lat, lon) in real_sites() {
        let Some(world) = projection.try_lon_lat_to_world(lon, lat) else {
            continue;
        };
        let Some(world) = globe::warp_world(world, blend) else {
            continue;
        };
        let screen = camera.world_to_screen(world, viewport);
        let x = screen.x.round() as i64;
        let y = screen.y.round() as i64;
        if x < 2 || y < 2 || x >= i64::from(WIDTH) - 2 || y >= i64::from(HEIGHT) - 2 {
            continue;
        }
        drawn += 1;
        for dy in -2..=2_i64 {
            for dx in -2..=2_i64 {
                if dx.abs() != 2 && dy.abs() != 2 {
                    continue;
                }
                let offset = (((y + dy) * i64::from(WIDTH)) + x + dx) as usize * 4;
                pixels[offset] = color[0];
                pixels[offset + 1] = color[1];
                pixels[offset + 2] = color[2];
                pixels[offset + 3] = 255;
            }
        }
    }
    drawn
}

fn output_dir() -> PathBuf {
    let path = std::env::var_os("MAP_SCENE_GLOBE_PROOF_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("map-scene-globe-proof"));
    std::fs::create_dir_all(&path).expect("create output directory");
    path
}

fn save(pixels: &[u8], name: &str) -> PathBuf {
    let path = output_dir().join(name);
    let image = image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels.to_vec()).expect("RGBA read-back");
    image.save(&path).expect("write PNG");
    println!("wrote {}", path.display());
    path
}

fn canvas() -> [f64; 4] {
    MapStylePreset::Slate
        .chrome()
        .canvas
        .to_array()
        .map(f64::from)
}

/// Ink pixels: anything that is not the pane's own ground.
fn drawn_pixels(pixels: &[u8], ground: &[u8]) -> usize {
    pixels
        .chunks_exact(4)
        .zip(ground.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count()
}

/// EVERYTHING in this file rests on this: the proposed pipeline, driven at
/// zero blend, must be the shipped pipeline. Not close - equal, byte for byte,
/// on a real GPU, at the scales an analyst works at and at the coarsest scale
/// where the blend is still exactly zero.
#[test]
fn the_morph_pipeline_at_zero_blend_is_the_shipped_pipeline() {
    let Some(mut harness) = Harness::new() else {
        eprintln!(
            "SKIPPED the_morph_pipeline_at_zero_blend_is_the_shipped_pipeline: no wgpu adapter, \
             so the globe is UNPROVEN on this machine"
        );
        return;
    };
    let morph = MorphPipeline::new(&harness.device, harness.format, globe::LIMB_FADE_RAD as f32);
    for km_per_point in [
        analyst_runtime::DEFAULT_KM_PER_POINT,
        1.0,
        5.0,
        globe::MIN_BLEND_KM_PER_POINT,
    ] {
        let camera = camera_at(km_per_point);
        let geometry = geometry_at(km_per_point);
        let blend = globe::blend_for_pane(km_per_point, viewport());
        assert_eq!(
            blend, 0.0,
            "{km_per_point} km/point is supposed to be inside the untouched range"
        );
        let shipped = harness.shipped_frame(Arc::clone(&geometry), camera, canvas());
        let morphed = harness.morph_frame(&morph, &geometry, camera, blend, canvas());
        let differing = drawn_pixels(&shipped, &morphed);
        assert_eq!(
            differing, 0,
            "the morph pipeline changed {differing} pixels at {km_per_point} km/point"
        );
    }
}

/// THE regression test for the defect this file was written to find.
///
/// Half the vertex buffer at globe scales is behind the limb - 51% of it from
/// KTLX at 32 km/point. Clamping those vertices onto the limb and drawing them
/// opaque paints a hard bright ring around the earth: 1 252 pixels of ink from
/// KTLX and 3 262 from RODN, on frames that only carry ten to eighteen
/// thousand pixels of real coastline. A globe with a drawn circle around it is
/// not a globe.
///
/// So the test is not "the ring is smaller". It is that the far side leaves NO
/// MARK AT ALL: a frame drawn with every wholly-far-side triangle removed on
/// the CPU must be byte-identical to the frame drawn with them left in. Only
/// triangles with all three vertices behind the limb are removed - the ones
/// that straddle it are exactly the ones that must still draw, up to the limb.
#[test]
fn the_far_hemisphere_leaves_no_mark_on_the_globe() {
    let Some(mut harness) = Harness::new() else {
        eprintln!(
            "SKIPPED the_far_hemisphere_leaves_no_mark_on_the_globe: no wgpu adapter, so the \
             limb is UNPROVEN on this machine"
        );
        return;
    };
    let faded = MorphPipeline::new(&harness.device, harness.format, globe::LIMB_FADE_RAD as f32);
    for (anchor_name, anchor) in [("KTLX", KTLX), ("RODN", RODN)] {
        for km_per_point in [16.0_f32, 22.4, 32.0, 50.0] {
            let camera = camera_at(km_per_point);
            let lod = LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
            let geometry = geometry_for_bucket(lod, anchor);
            let blend = globe::blend_for_pane(km_per_point, viewport());
            assert_eq!(blend, 1.0, "{km_per_point} km/point should be a full globe");
            let horizon_km = globe::horizon_radius_km(blend);
            let behind = |index: u32| {
                let vertex = geometry.vertices[index as usize];
                f64::from(vertex.position_km[0]).hypot(f64::from(vertex.position_km[1]))
                    > horizon_km
            };
            let kept: Vec<u32> = geometry
                .indices
                .chunks_exact(3)
                .filter(|triangle| !triangle.iter().all(|index| behind(*index)))
                .flatten()
                .copied()
                .collect();
            let dropped = (geometry.indices.len() - kept.len()) / 3;
            assert!(
                dropped > 100,
                "{anchor_name} at {km_per_point} km/point has only {dropped} far-side \
                 triangles, so this proves nothing"
            );

            let all = harness.morph_frame(&faded, &geometry, camera, blend, canvas());
            let near_only =
                harness.morph_frame_indexed(&faded, &geometry, &kept, camera, blend, canvas());
            let differing = drawn_pixels(&all, &near_only);
            assert_eq!(
                differing, 0,
                "{anchor_name} at {km_per_point} km/point: {dropped} far-side triangles left \
                 {differing} pixels of ink on the globe"
            );
        }
    }
}

/// What the far-zoom view looks like TODAY, what the clamp-only morph makes of
/// it, and what the faded limb makes of it. This is the picture, not a metric.
#[test]
#[ignore = "writes PNGs for a human to look at"]
fn look_at_the_globe() {
    let Some(mut harness) = Harness::new() else {
        eprintln!("SKIPPED look_at_the_globe: no wgpu adapter");
        return;
    };
    // Fade width 0 reproduces a clamp with no fade: the previous proposal.
    let clamped = MorphPipeline::new(&harness.device, harness.format, 0.0);
    let faded = MorphPipeline::new(&harness.device, harness.format, globe::LIMB_FADE_RAD as f32);
    let ground = {
        let mut encoder = harness
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let _pass = begin(&mut encoder, &harness.view, canvas());
        }
        harness.read_back(encoder)
    };

    for km_per_point in [0.35_f32, 5.0, 8.0, 10.0, 12.0, 14.0, 16.0, 22.4, 32.0, 50.0] {
        let camera = camera_at(km_per_point);
        let geometry = geometry_at(km_per_point);
        let lod = LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
        let blend = globe::blend_for_pane(km_per_point, viewport());

        let mut shipped = harness.shipped_frame(Arc::clone(&geometry), camera, canvas());
        let shipped_ink = drawn_pixels(&shipped, &ground);
        let sites = draw_sites(&mut shipped, camera, 0.0, [255, 90, 40]);
        save(&shipped, &format!("shipped-{km_per_point}.png"));

        let mut clamp = harness.morph_frame(&clamped, &geometry, camera, blend, canvas());
        let clamp_ink = drawn_pixels(&clamp, &ground);
        draw_sites(&mut clamp, camera, blend, [255, 90, 40]);
        save(&clamp, &format!("clamped-{km_per_point}.png"));

        let mut fade = harness.morph_frame(&faded, &geometry, camera, blend, canvas());
        let fade_ink = drawn_pixels(&fade, &ground);
        draw_sites(&mut fade, camera, blend, [255, 90, 40]);
        save(&fade, &format!("faded-{km_per_point}.png"));

        println!(
            "{km_per_point:>6} km/point  bucket {:>3}  blend {blend:.3}  \
             ink shipped {shipped_ink:>7} clamped {clamp_ink:>7} faded {fade_ink:>7}  \
             smear {:>6}  sites {sites}",
            lod.0,
            clamp_ink as i64 - fade_ink as i64,
        );
    }
}

/// The far hemisphere from an anchor that HAS a far hemisphere worth drawing.
///
/// KTLX cannot show this: the antipode of Oklahoma is empty southern Indian
/// Ocean, so a clamp that piles the far side onto the limb piles almost
/// nothing there. RODN's antipode is southern Brazil. This is the frame that
/// decides whether the limb needs a fade or only a clamp.
#[test]
#[ignore = "writes PNGs for a human to look at"]
fn look_at_the_far_hemisphere_from_kadena() {
    let Some(mut harness) = Harness::new() else {
        eprintln!("SKIPPED look_at_the_far_hemisphere_from_kadena: no wgpu adapter");
        return;
    };
    let clamped = MorphPipeline::new(&harness.device, harness.format, 0.0);
    let faded = MorphPipeline::new(&harness.device, harness.format, globe::LIMB_FADE_RAD as f32);
    let ground = {
        let mut encoder = harness
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let _pass = begin(&mut encoder, &harness.view, canvas());
        }
        harness.read_back(encoder)
    };
    for km_per_point in [22.4_f32, 32.0] {
        let camera = camera_at(km_per_point);
        let lod = LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
        let geometry = geometry_for_bucket(lod, RODN);
        let blend = globe::blend_for_pane(km_per_point, viewport());
        let horizon_km = globe::horizon_radius_km(blend);
        let behind = geometry
            .vertices
            .iter()
            .filter(|vertex| {
                f64::from(vertex.position_km[0]).hypot(f64::from(vertex.position_km[1]))
                    > horizon_km
            })
            .count();

        let shipped = harness.shipped_frame(Arc::clone(&geometry), camera, canvas());
        save(&shipped, &format!("rodn-shipped-{km_per_point}.png"));
        let clamp = harness.morph_frame(&clamped, &geometry, camera, blend, canvas());
        save(&clamp, &format!("rodn-clamped-{km_per_point}.png"));
        let fade = harness.morph_frame(&faded, &geometry, camera, blend, canvas());
        save(&fade, &format!("rodn-faded-{km_per_point}.png"));
        println!(
            "RODN {km_per_point:>6} km/point  blend {blend:.3}  behind the limb {behind:>7}  \
             ink shipped {:>7} clamped {:>7} faded {:>7}",
            drawn_pixels(&shipped, &ground),
            drawn_pixels(&clamp, &ground),
            drawn_pixels(&fade, &ground),
        );
    }
}

/// How much of the retained geometry is BEHIND the limb at the scale it is
/// drawn at - the geometry the shader has to do something with, and the
/// measurement that decides whether "the far hemisphere is hidden" is a claim
/// about the pipeline or only about `warp_world`.
#[test]
fn far_side_geometry_reaches_the_shader_at_every_globe_scale() {
    let mut any = false;
    for km_per_point in [12.0_f32, 19.7, 22.4, 25.0, 32.0, 40.0, 50.0] {
        let lod = LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
        let geometry = geometry_for_bucket(lod, KTLX);
        let blend = globe::blend_for_pane(km_per_point, viewport());
        let horizon_km = globe::horizon_radius_km(blend);
        let behind = geometry
            .vertices
            .iter()
            .filter(|vertex| {
                f64::from(vertex.position_km[0]).hypot(f64::from(vertex.position_km[1]))
                    > horizon_km
            })
            .count();
        println!(
            "{km_per_point:>5} km/point  bucket {:>3}  blend {blend:.3}  limb {horizon_km:>8.0} km  \
             vertices {:>7}  behind the limb {behind:>7} ({:.1}%)",
            lod.0,
            geometry.vertices.len(),
            100.0 * behind as f64 / geometry.vertices.len().max(1) as f64
        );
        if behind > 0 {
            any = true;
        }
    }
    assert!(
        any,
        "if no vertex ever reaches the shader from behind the limb, the shader needs no limb \
         handling at all and this file can be deleted"
    );
}

/// The marker layer and the vector layer have to agree about where the limb
/// is. `warp_world` culls a site the moment it passes the horizon; the shader
/// fades a line out at the same horizon. This checks they use the same number.
#[test]
fn the_marker_cull_and_the_line_fade_share_one_horizon() {
    for blend in [0.6_f32, 0.75, 0.9, 1.0] {
        let horizon = globe::horizon_radius_km(blend);
        let inside = WorldPoint::new(horizon - 1.0, 0.0);
        let outside = WorldPoint::new(horizon + 1.0, 0.0);
        assert!(globe::warp_world(inside, blend).is_some());
        assert!(globe::warp_world(outside, blend).is_none());
    }
}

/// Switching the anchor to another REAL radar - one 22 km away and one
/// 1 903 km away - must not disturb the near view, and must leave the limb
/// honest from the new anchor.
///
/// Both distances are measured from the live catalogue, not invented: TOKC is
/// the Oklahoma City terminal radar, 22 km from KTLX, and KMSX is Missoula.
/// A 22 km hop is the case where nothing may perceptibly change; a 1 903 km
/// hop is the case where the whole globe turns under the analyst.
#[test]
fn switching_to_a_radar_22_km_or_1903_km_away_keeps_the_near_view_and_the_limb_honest() {
    let sites = real_sites();
    let find = |id: &str| {
        sites
            .iter()
            .find(|(name, _, _)| name == id)
            .unwrap_or_else(|| panic!("{id} is in the live catalogue"))
            .clone()
    };
    let here = find("KTLX");
    for id in ["TOKC", "KMSX"] {
        let (_, lat, lon) = find(id);
        let projection = RadarProjection::new(lat, lon);
        let sphere = globe::GlobeProjection::new(lat, lon);

        // 1. The near view is untouched: at every analysis scale the blend is
        //    a hard zero, so the new anchor's map is the shipped projection.
        for scale in [0.35_f32, 1.0, 2.0, 5.0, globe::MIN_BLEND_KM_PER_POINT] {
            assert_eq!(
                globe::blend_for_pane(scale, viewport()),
                0.0,
                "{id} at {scale} km/point"
            );
        }

        // 2. The projection is still an equidistant one about the NEW anchor:
        //    the drawn distance to the old anchor is its geodesic distance.
        let world = projection
            .try_lon_lat_to_world(here.2, here.1)
            .expect("KTLX projects from the new anchor");
        let drawn_km = world.east_km.hypot(world.north_km);
        let (back_lon, back_lat) = projection.world_to_lon_lat(world);
        assert!(
            (back_lon - here.2).abs() < 1e-6 && (back_lat - here.1).abs() < 1e-6,
            "{id}: the inverse did not return KTLX"
        );
        println!("{id}: KTLX is {drawn_km:.1} km away in the new frame");

        // 3. At a full globe the limb partition from the new anchor still
        //    agrees with Snyder, over the whole real catalogue.
        let mut hidden = 0;
        for (name, site_lat, site_lon) in &sites {
            let Some(world) = projection.try_lon_lat_to_world(*site_lon, *site_lat) else {
                continue;
            };
            let drawn = globe::warp_world(world, 1.0).is_some();
            let facing = sphere.is_visible(*site_lon, *site_lat);
            assert_eq!(drawn, facing, "{id}: {name} disagreed about the limb");
            if !drawn {
                hidden += 1;
            }
        }
        println!(
            "{id}: {hidden} of {} real sites are behind the limb",
            sites.len()
        );
    }
}

/// The field complaint, reproduced on the REAL catalogue with the REAL
/// camera: "the radar sites move every time i zoom".
///
/// A marker's world position cannot move with the camera - `app.rs` projects
/// the catalogue once per ANCHOR and the paint pass only applies the camera
/// transform - so if a marker walks, the camera walked. This drives the real
/// `Camera2D::zoom_about` with the real `zoom_factor_for_notches` about an
/// off-centre anchor, exactly as a wheel over a pane does, and measures where
/// every real site ends up.
///
/// Inside the scale limits the round trip is exact and this test asserts it.
/// PAST the limits it is not, and that is a defect in
/// `analyst_runtime/src/view.rs` which this crate cannot fix: `zoom_about`
/// clamps the RESULT of the division, so a notch that cannot be honoured is
/// still consumed, and the anchor compensation for it is not. The numbers are
/// printed rather than asserted, so that fixing view.rs does not fail a test
/// that was pinning the bug.
#[test]
fn real_sites_return_to_their_own_pixel_after_a_zoom_round_trip() {
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let sites = real_sites();
    assert!(
        sites.len() > 100,
        "the real catalogue is {} rows",
        sites.len()
    );
    let viewport = viewport().sanitized();
    let placed: Vec<(String, WorldPoint)> = sites
        .iter()
        .filter_map(|(id, lat, lon)| {
            projection
                .try_lon_lat_to_world(*lon, *lat)
                .map(|world| (id.clone(), world))
        })
        .collect();
    // The pointer sits off centre, which is what makes a zoom move the centre
    // at all.
    let anchor = ScreenPoint::new(1_200.0, 300.0);
    let notch = analyst_runtime::zoom_factor_for_notches(1.0);

    for notches in [5_u32, 14, 22, 27, 29, 40] {
        let start = Camera2D::default();
        let before: Vec<ScreenPoint> = placed
            .iter()
            .map(|(_, world)| start.world_to_screen(*world, viewport))
            .collect();
        let mut camera = start;
        for _ in 0..notches {
            camera.zoom_about(1.0 / notch, anchor, viewport);
        }
        let coarsest = camera.km_per_point;
        for _ in 0..notches {
            camera.zoom_about(notch, anchor, viewport);
        }
        let mut worst = 0.0_f32;
        let mut worst_site = String::new();
        for ((id, world), was) in placed.iter().zip(&before) {
            let now = camera.world_to_screen(*world, viewport);
            let moved = (now.x - was.x).hypot(now.y - was.y);
            if moved > worst {
                worst = moved;
                worst_site = id.clone();
            }
        }
        println!(
            "{notches:>2} notches out and back: reached {coarsest:>7.3} km/point, returned to \
             {:.6} km/point, centre ({:.1}, {:.1}) km, worst marker {worst:.4} points ({worst_site})",
            camera.km_per_point, camera.center_east_km, camera.center_north_km
        );
        if coarsest < analyst_runtime::MAX_KM_PER_POINT {
            // The wheel never hit the ceiling, so the round trip is exact and
            // every marker is back on its own pixel.
            // A fiftieth of a screen point. That is 54 f32 zoom operations of
            // accumulated rounding on a site 10 881 km away, not drift the eye
            // could find: the measured worst is 0.017 points at 27 notches.
            assert!(
                worst < 0.05,
                "{notches} notches inside the limits moved {worst_site} by {worst} points"
            );
            assert!(
                (camera.km_per_point - analyst_runtime::DEFAULT_KM_PER_POINT).abs() < 1e-6,
                "{notches} notches inside the limits did not return the scale"
            );
        }
    }

    // And the ceiling itself: the scale the wheel stops responding at.
    let mut camera = Camera2D::default();
    let mut dead_at = None;
    for notch_index in 1..60 {
        let before = camera.km_per_point;
        camera.zoom_about(1.0 / notch, anchor, viewport);
        if camera.km_per_point == before && dead_at.is_none() {
            dead_at = Some((notch_index, before));
        }
    }
    let (index, scale) = dead_at.expect("the wheel does stop somewhere");
    println!("the wheel stops changing the scale at notch {index}, {scale} km/point");
    assert_eq!(
        scale,
        analyst_runtime::MAX_KM_PER_POINT,
        "the wheel died somewhere other than the documented ceiling"
    );
}

/// Where the raster underlay actually stops, measured rather than inherited.
///
/// The integration note that switches the tile layer off past
/// `MIN_BLEND_KM_PER_POINT` is only honest if it knows what it is taking away.
/// `tile_zoom_for` already refuses to answer once the camera is coarser than
/// the provider's coarsest zoom, and this reports that cutoff per device pixel
/// ratio - which is what decides how much imagery the globe costs.
#[test]
fn the_raster_layer_already_stands_down_near_the_globe_floor() {
    for pixels_per_point in [1.0_f32, 1.25, 1.5, 2.0, 3.0, 4.0] {
        let mut coarsest = None;
        for bucket in -4_i16..=24 {
            let lod = LodBucket(bucket);
            if map_scene::tiles::tile_zoom_for(lod, KTLX.0, pixels_per_point).is_some() {
                coarsest = Some(lod.center_scale(LOD_REFERENCE_KM_PER_POINT));
            }
        }
        let coarsest = coarsest.expect("some bucket draws imagery");
        println!(
            "at {pixels_per_point}x the raster layer stops after {coarsest:.2} km/point \
             (globe floor {:.2})",
            globe::MIN_BLEND_KM_PER_POINT
        );
        // What the note gives up is the band between the globe floor and this
        // cutoff. On a 1x display there is none.
        if pixels_per_point <= 1.25 {
            assert!(
                coarsest <= globe::MIN_BLEND_KM_PER_POINT,
                "at {pixels_per_point}x the imagery outlives the globe floor by \
                 {coarsest} km/point, so the note gives up more than it says"
            );
        }
    }
}
