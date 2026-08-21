//! Photograph the crooked map, before and after.
//!
//! ```text
//! cargo run --release -p workstation_app --example projection_anchor_proof -- \
//!     <level2-file> <out-dir>
//! ```
//!
//! Headless: a wgpu device with no window, the SHIPPED `MapPaintCallback` over
//! geometry the SHIPPED `build_geometry` made from the shipped basemap, with
//! the SHIPPED `render2d` raster of a REAL Level II volume composited over it
//! through the same `Camera2D` the pane uses. A ten-degree graticule and the
//! range rings are drawn on the read-back frame through
//! `Camera2D::world_to_screen`, so they carry the camera's rotation exactly as
//! the pane's own overlays do.
//!
//! The graticule is the point of the exercise. "The map looks crooked" is a
//! statement about where the meridians run, and no amount of arithmetic in a
//! test settles it - so every frame here has the meridians drawn on it and the
//! answer is read off the picture.

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use analyst_runtime::{
    Camera2D, Generation, GeometryCacheKey, LodSelector, ScreenPoint, ViewportMetrics, WorldPoint,
};
use eframe::egui;
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;
use map_scene::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
use map_scene::dataset::MapDataset;
use map_scene::geometry::MapGeometry;
use map_scene::gpu::{MapPaintCallback, MapRenderResources};
use map_scene::projection::{RadarProjection, globe};
use map_scene::style_presets::MapStylePreset;
use radar_core::MomentType;
use render2d::{DisplayQuality, ViewportMomentCache, ViewportRasterOptions};

/// 1600 * 4 bytes is 25 * 256, so the read-back needs no row padding.
const WIDTH: u32 = 1600;
const HEIGHT: u32 = 900;

/// KRTX, Portland, from the live catalogue. The anchor in the complaint.
const KRTX: (f64, f64) = (45.714_968_872_070_31, -122.965_301_513_671_88);

/// PABR, Barrow: a SYNTHETIC anchor and not a shipped one - the station table
/// this repository ships has 208 rows and the highest latitude in it is PAPD
/// at 65.0351 N. Barrow is photographed because 71.2854 N is the harshest
/// latitude a WSR-88D could stand at, so it exercises the rule's polar hold
/// and its fade across the ray where the screen bearing of true north wraps
/// harder than any real row can.
const PABR: (f64, f64) = (71.2854, -156.7889);

/// One number per anchor photographed here. See `geometry_for` for why two
/// anchors must never share one.
const KRTX_ANCHOR: u64 = 1;
const SITE_ANCHOR: u64 = 2;
const PABR_ANCHOR: u64 = 3;

/// WGS84 polar radius of curvature, for turning a colatitude into kilometres.
/// The same number and the same reason as `POLAR_RADIUS_OF_CURVATURE_KM` in
/// `map_scene::projection`; restated here because an example must not reach
/// into a crate's private constants to photograph it.
const POLAR_RADIUS_OF_CURVATURE_KM: f64 = 6_399.594;

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

fn viewport() -> ViewportMetrics {
    ViewportMetrics {
        width_points: WIDTH as f32,
        height_points: HEIGHT as f32,
        pixels_per_point: 1.0,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let volume_path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("usage: projection_anchor_proof <level2-file> <out-dir>")?;
    let out_dir = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("usage: projection_anchor_proof <level2-file> <out-dir>")?;
    std::fs::create_dir_all(&out_dir)?;

    let volume = nexrad_io::decode_volume_from_path(&volume_path)?;
    let site_lat = f64::from(
        volume
            .site
            .latitude_deg
            .ok_or("the volume has no site latitude")?,
    );
    let site_lon = f64::from(
        volume
            .site
            .longitude_deg
            .ok_or("the volume has no site longitude")?,
    );
    println!(
        "volume {} at {site_lat:.4} {site_lon:.4}, {} cuts",
        volume.site.id,
        volume.cuts.len()
    );

    let mut harness = Harness::new().ok_or("no wgpu adapter: nothing can be photographed here")?;

    // (a) THE REPORTED FAULT. A west-coast anchor, looking at the eastern United
    // States, with no radar data on the pane - this frame is entirely about
    // where the meridians run.
    let krtx = RadarProjection::new(KRTX.0, KRTX.1);
    for (label, lon, lat, km_per_point) in [
        ("seaboard", -75.0_f64, 40.0_f64, 1.0_f32),
        ("overview", -98.58, 39.83, 4.0),
        ("continental", -98.58, 39.83, 2.8),
    ] {
        let centre = krtx
            .try_lon_lat_to_world(lon, lat)
            .ok_or("the view centre does not project")?;
        let stored = Camera2D {
            center_east_km: centre.east_km,
            center_north_km: centre.north_km,
            km_per_point,
            rotation_rad: 0.0,
        };
        let blend = globe::blend_for_pane(km_per_point, viewport());
        let derived = krtx.view_rotation_rad(centre, km_per_point);
        let display = Camera2D {
            rotation_rad: derived,
            ..stored
        };
        println!("{label}: derived rotation {:.4} deg", derived.to_degrees());
        for (suffix, camera) in [("before", stored), ("after", display)] {
            let mut pixels = harness.map_frame(&krtx, KRTX_ANCHOR, camera);
            draw_graticule(&mut pixels, &krtx, camera, blend);
            save(&out_dir, &format!("{label}-{suffix}"), &pixels)?;
        }
    }

    // (b) THE ANALYSIS VIEW, on the real storm. The antenna in the middle at
    // the default scale: the rule must return an exact zero here, so the two
    // frames have to be identical byte for byte.
    let site = RadarProjection::new(site_lat, site_lon);
    let echo = raster(&volume, MomentType::Reflectivity, 0)?;
    for (label, east_km, km_per_point) in [
        ("analysis", 0.0_f64, 0.35_f32),
        // Dragged 600 km east: inside the 460-920 km ramp band, where a sliver
        // of the outer footprint is still on screen while the map is turning.
        ("ramp-band", 600.0, 0.35),
        // Far enough downrange for the ramp to be all but finished, and coarse
        // enough that the WHOLE footprint is on the pane with a lot of
        // basemap around it. This is the frame that says whether a rotated
        // echo is still on its county lines.
        ("downrange", 900.0, 2.0),
    ] {
        let stored = Camera2D {
            center_east_km: east_km,
            center_north_km: 0.0,
            km_per_point,
            rotation_rad: 0.0,
        };
        let blend = globe::blend_for_pane(km_per_point, viewport());
        let derived = site.view_rotation_rad(
            WorldPoint::new(stored.center_east_km, stored.center_north_km),
            km_per_point,
        );
        println!("{label}: derived rotation {:.6} deg", derived.to_degrees());
        for (suffix, camera) in [
            ("before", stored),
            (
                "after",
                Camera2D {
                    rotation_rad: derived,
                    ..stored
                },
            ),
        ] {
            let mut pixels = harness.map_frame(&site, SITE_ANCHOR, camera);
            paint_echo(&mut pixels, &volume, &echo, camera);
            draw_graticule(&mut pixels, &site, camera, blend);
            draw_range_rings(&mut pixels, camera);
            save(&out_dir, &format!("{label}-{suffix}"), &pixels)?;
        }
    }

    // (c) A SYNTHETIC HIGH-LATITUDE ANCHOR, harsher than any row the station
    // table ships. Barrow is 2080 km from the pole, which is where the rule's
    // two guards live: the polar hold, and the fade across the ray where the
    // screen bearing of true north wraps. The first frame is an ordinary
    // continental view from Barrow - the case the rule is FOR - and the second
    // is centred on the pole itself, where it must do nothing at all rather
    // than something confident.
    let pabr = RadarProjection::new(PABR.0, PABR.1);
    let pole_gap_km = (90.0 - PABR.0).to_radians() * POLAR_RADIUS_OF_CURVATURE_KM;
    for (label, centre, km_per_point) in [
        (
            "barrow-continental",
            WorldPoint::new(1600.0, -1600.0),
            2.8_f32,
        ),
        ("barrow-pole", WorldPoint::new(0.0, pole_gap_km), 2.8),
        (
            "barrow-past-the-pole",
            WorldPoint::new(0.0, pole_gap_km + 2600.0),
            2.8,
        ),
    ] {
        let stored = Camera2D {
            center_east_km: centre.east_km,
            center_north_km: centre.north_km,
            km_per_point,
            rotation_rad: 0.0,
        };
        let blend = globe::blend_for_pane(km_per_point, viewport());
        let derived = pabr.view_rotation_rad(centre, km_per_point);
        let (lon, lat) = pabr.world_to_lon_lat(centre);
        println!(
            "{label}: centre {lat:.3} {lon:.3}, derived rotation {:.4} deg",
            derived.to_degrees()
        );
        for (suffix, camera) in [
            ("before", stored),
            (
                "after",
                Camera2D {
                    rotation_rad: derived,
                    ..stored
                },
            ),
        ] {
            let mut pixels = harness.map_frame(&pabr, PABR_ANCHOR, camera);
            draw_graticule(&mut pixels, &pabr, camera, blend);
            save(&out_dir, &format!("{label}-{suffix}"), &pixels)?;
        }
    }

    // (d) THE EDGES OF THE DOMAIN, which is the set of frames that matters
    // most: if the restriction snapped anywhere, it would snap here. Two
    // ladders from the same west-coast anchor, each stepping from well inside
    // the domain, through the middle of a fade, to outside it - one in SCALE
    // and one in DOWNRANGE DISTANCE. The rotation printed beside each frame
    // and the meridians drawn on it should ease off together, not jump.
    for (label, lon, lat, km_per_point) in [
        ("scale-edge-inside", -98.58_f64, 39.83_f64, 4.9_f32),
        ("scale-edge-band", -98.58, 39.83, 6.0),
        ("scale-edge-outside", -98.58, 39.83, 7.2),
    ] {
        let centre = krtx
            .try_lon_lat_to_world(lon, lat)
            .ok_or("the view centre does not project")?;
        edge_frame(
            &mut harness,
            &out_dir,
            &krtx,
            KRTX_ANCHOR,
            label,
            centre,
            km_per_point,
        )?;
    }
    for (label, range_km) in [
        ("range-edge-inside", 4_900.0_f64),
        ("range-edge-band", 5_750.0),
        // A kilometre either side of the zero edge itself. The smoothstep's
        // slope vanishes there, so these two frames must be the same picture -
        // if the edge snapped, this is the pair it would snap between.
        ("range-edge-just-inside", 6_499.0),
        ("range-edge-just-outside", 6_501.0),
        ("range-edge-outside", 6_600.0),
    ] {
        // Due east from Portland, which at these distances is the north
        // Atlantic and beyond - the meridians there are what a snap would show.
        let centre = WorldPoint::new(range_km, 0.0);
        edge_frame(
            &mut harness,
            &out_dir,
            &krtx,
            KRTX_ANCHOR,
            label,
            centre,
            2.8,
        )?;
    }
    Ok(())
}

/// One before/after pair at a stated view centre and scale, for the domain
/// edge ladders.
fn edge_frame(
    harness: &mut Harness,
    out_dir: &std::path::Path,
    projection: &RadarProjection,
    anchor: u64,
    label: &str,
    centre: WorldPoint,
    km_per_point: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let stored = Camera2D {
        center_east_km: centre.east_km,
        center_north_km: centre.north_km,
        km_per_point,
        rotation_rad: 0.0,
    };
    let blend = globe::blend_for_pane(km_per_point, viewport());
    let derived = projection.view_rotation_rad(centre, km_per_point);
    println!(
        "{label}: {:.0} km downrange at {km_per_point} km/point, blend {blend:.3}, \
         derived rotation {:.4} deg",
        centre.east_km.hypot(centre.north_km),
        derived.to_degrees()
    );
    for (suffix, camera) in [
        ("before", stored),
        (
            "after",
            Camera2D {
                rotation_rad: derived,
                ..stored
            },
        ),
    ] {
        let mut pixels = harness.map_frame(projection, anchor, camera);
        draw_graticule(&mut pixels, projection, camera, blend);
        save(out_dir, &format!("{label}-{suffix}"), &pixels)?;
    }
    Ok(())
}

/// The lowest reflectivity tilt, rasterised the way the render worker does it.
fn raster(
    volume: &radar_core::RadarVolume,
    moment: MomentType,
    cut_index: usize,
) -> Result<ViewportMomentCache, Box<dyn std::error::Error>> {
    let tables = color_tables::ColorTableSet::default();
    Ok(ViewportMomentCache::new_display_quality(
        volume,
        cut_index,
        moment,
        &tables,
        DisplayQuality::default(),
    )?)
}

/// Composite the radar raster over the read-back basemap, straight alpha.
///
/// The raster is produced from the SAME camera the basemap was drawn with, via
/// `radar_raster_view`, so the two are one picture by construction rather than
/// by being lined up here.
fn paint_echo(
    pixels: &mut [u8],
    volume: &radar_core::RadarVolume,
    cache: &ViewportMomentCache,
    camera: Camera2D,
) {
    let quality = DisplayQuality::default();
    let view = camera.radar_raster_view(viewport());
    let options = ViewportRasterOptions {
        width: view.width_px,
        height: view.height_px,
        radar_x_px: view.radar_x_px,
        radar_y_px: view.radar_y_px,
        km_per_px_x: view.km_per_px,
        km_per_px_y: view.km_per_px,
        rotation_rad: view.rotation_rad,
    };
    let mut rgba =
        vec![0_u8; render2d::quality::quality_rgba_buffer_len(options, quality.supersample)];
    let Ok((width, height)) = render2d::quality::render_moment_viewport_quality_rgba_into(
        cache,
        volume,
        options,
        quality.supersample,
        &mut rgba,
    ) else {
        return;
    };
    assert_eq!((width, height), (WIDTH, HEIGHT), "the raster changed size");
    for (target, source) in pixels.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        let alpha = f32::from(source[3]) / 255.0;
        for channel in 0..3 {
            let over =
                f32::from(source[channel]) * alpha + f32::from(target[channel]) * (1.0 - alpha);
            target[channel] = over.round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// A ten-degree graticule, drawn through the same `world_to_screen` the pane's
/// own overlays use.
///
/// Meridians in one colour and parallels in another, because the whole
/// question is whether the meridians run up the screen.
fn draw_graticule(pixels: &mut [u8], projection: &RadarProjection, camera: Camera2D, blend: f32) {
    let meridian = [90_u8, 190, 255];
    let parallel = [255, 170, 90];
    let mut plot = |lon: f64, lat: f64, colour: [u8; 3]| {
        let Some(world) = projection.try_lon_lat_to_world(lon, lat) else {
            return;
        };
        let Some(world) = globe::warp_world(world, blend) else {
            return;
        };
        let screen = camera.world_to_screen(world, viewport());
        put(pixels, screen, colour);
    };
    let mut longitude = -180.0_f64;
    while longitude < 180.0 {
        let mut latitude = -80.0_f64;
        while latitude <= 80.0 {
            plot(longitude, latitude, meridian);
            latitude += 0.05;
        }
        longitude += 10.0;
    }
    let mut latitude = -80.0_f64;
    while latitude <= 80.0 {
        let mut longitude = -180.0_f64;
        while longitude < 180.0 {
            plot(longitude, latitude, parallel);
            longitude += 0.05;
        }
        latitude += 10.0;
    }
}

/// Range rings at 100 km intervals, plus a tick on world north.
///
/// A ring is a circle under any camera rotation - that is the near-field
/// promise - so drawing it as a screen circle is still right. The north tick
/// is what makes the rotation visible in the picture.
fn draw_range_rings(pixels: &mut [u8], camera: Camera2D) {
    let centre = camera.world_to_screen(WorldPoint::ORIGIN, viewport());
    let (sin, cos) = camera.sanitized().rotation_rad.sin_cos();
    for range_km in [100.0_f32, 200.0, 300.0, 400.0] {
        let radius = range_km / camera.sanitized().km_per_point;
        for step in 0..4_000 {
            let angle = std::f32::consts::TAU * step as f32 / 4_000.0;
            put(
                pixels,
                ScreenPoint::new(
                    centre.x + radius * angle.sin(),
                    centre.y - radius * angle.cos(),
                ),
                [220, 220, 220],
            );
        }
        // Where the ring crosses world NORTH, which is where the pane writes
        // the ring's distance.
        for step in 0..40 {
            let along = radius - step as f32 * 0.5;
            put(
                pixels,
                ScreenPoint::new(centre.x + sin * along, centre.y - cos * along),
                [255, 80, 80],
            );
        }
    }
}

fn put(pixels: &mut [u8], at: ScreenPoint, colour: [u8; 3]) {
    let x = at.x.round();
    let y = at.y.round();
    if !(0.0..WIDTH as f32).contains(&x) || !(0.0..HEIGHT as f32).contains(&y) {
        return;
    }
    let offset = ((y as usize) * WIDTH as usize + x as usize) * 4;
    pixels[offset] = colour[0];
    pixels[offset + 1] = colour[1];
    pixels[offset + 2] = colour[2];
    pixels[offset + 3] = 255;
}

fn save(
    dir: &std::path::Path,
    stem: &str,
    pixels: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.join(format!("{stem}.png"));
    let image = image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels.to_vec())
        .ok_or("the read-back is not a whole frame")?;
    image.save(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

struct Harness {
    device: wgpu::Device,
    queue: wgpu::Queue,
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
            label: Some("projection anchor proof device"),
            ..Default::default()
        }))
        .ok()?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("projection anchor proof target"),
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
            label: Some("projection anchor proof readback"),
            size: u64::from(WIDTH) * u64::from(HEIGHT) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut resources = CallbackResources::default();
        resources.insert(MapRenderResources::new(&device, format));
        Some(Self {
            device,
            queue,
            resources,
            target,
            view,
            readback,
        })
    }

    /// One basemap frame through the shipped paint callback.
    fn map_frame(
        &mut self,
        projection: &RadarProjection,
        anchor: u64,
        camera: Camera2D,
    ) -> Vec<u8> {
        let geometry = geometry_for(projection, anchor, camera.sanitized().km_per_point);
        let rect_px = [0.0, 0.0, WIDTH as f32, HEIGHT as f32];
        let screen = ScreenDescriptor {
            size_in_pixels: [WIDTH, HEIGHT],
            pixels_per_point: 1.0,
        };
        let callback = MapPaintCallback {
            pane_index: 0,
            geometry,
            camera,
            viewport: viewport(),
            rect_px,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("projection anchor proof"),
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
            let canvas = MapStylePreset::Slate
                .chrome()
                .canvas
                .to_array()
                .map(f64::from);
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("projection anchor proof pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.view,
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
                .forget_lifetime();
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
            let in_pixels = info.viewport_in_pixels();
            pass.set_viewport(
                in_pixels.left_px as f32,
                in_pixels.top_px as f32,
                in_pixels.width_px as f32,
                in_pixels.height_px as f32,
                0.0,
                1.0,
            );
            callback.paint(info, &mut pass, &self.resources);
        }
        self.read_back(encoder)
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
}

/// The geometry for one anchor at one scale.
///
/// `anchor` numbers the PROJECTION, and it is load bearing rather than
/// bookkeeping: `MapRenderResources` keeps uploaded geometry resident by
/// `GeometryCacheKey`, so two anchors sharing a key at the same LOD would have
/// the second frame drawn with the FIRST one's vertices. That is exactly what
/// happened the first time this file photographed a second anchor at a scale a
/// previous one had already used - a Barrow frame came back showing Mexico,
/// because Mexico is what sits 2263 km southeast of Portland. The application
/// never meets this: `map_scene::residency` bumps the projection generation on
/// every site change.
fn geometry_for(projection: &RadarProjection, anchor: u64, km_per_point: f32) -> Arc<MapGeometry> {
    let lod = LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
    Arc::new(build_geometry(&MapBuildRequest {
        key: GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(anchor),
            style: Generation::new(1),
            lod,
        },
        dataset: MapDataset::from_generated(Generation::new(1)),
        projection: *projection,
        style: MapStylePreset::Slate.style(),
    }))
}
