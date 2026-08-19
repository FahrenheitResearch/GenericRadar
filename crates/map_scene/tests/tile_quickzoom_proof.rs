//! The quick-zoom behaviour of the tile basemap, measured frame by frame on a
//! real GPU against the real USGS provider.
//!
//! This is the diagnosis-and-proof harness for the report that
//! satellite tiles "would be nice if they loaded either faster or more uniform
//! when quick zooming in". It scripts exactly that gesture — three tile-zoom
//! steps in about 700 ms over KTLX — through the shipping pipeline
//! (`TileSceneController` → `TilePaintCallback` → render pass → readback) and
//! measures what a viewer would see in every frame:
//!
//! * the fraction of the pane painted by imagery at all (bare ground showing
//!   through is the non-uniformity complaint made a number),
//! * the largest frame-to-frame *loss* of painted pixels (a blink),
//! * which tiles draw exact, which by ancestor, and the fade state,
//! * the order tiles arrive in, against their distance from the pane centre
//!   (centre-out fetch order is a design claim; this checks it), and
//! * wall clock from the end of the gesture to a uniformly sharp pane,
//!   cold and warm.
//!
//! Network tests are `#[ignore]`d, as in `tile_render_proof.rs`:
//!
//! ```text
//! cargo test --release -p map_scene --test tile_quickzoom_proof -- --ignored \
//!     --nocapture --test-threads=1
//! ```
//!
//! `MAP_SCENE_TILE_PROOF_OUT` chooses where the per-frame PNGs go.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use analyst_runtime::{Camera2D, Generation, LodSelector, ViewportMetrics};
use eframe::egui;
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;
use map_scene::build::LOD_REFERENCE_KM_PER_POINT;
use map_scene::gpu::{MapRenderResources, TileRenderResources};
use map_scene::projection::RadarProjection;
use map_scene::style_presets::MapStylePreset;
use map_scene::tiles::{TileFrame, TileSceneController};
use map_scene::{TileCacheConfig, TileProvider};

/// Square pane; 512*4 bytes rows satisfy the 256-byte copy alignment.
const SIDE: u32 = 512;
/// KTLX, the site every proof in this workspace uses.
const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);
/// The camera the gesture starts from (tile z9 at KTLX) and ends at (z12).
const START_KM_PER_POINT: f32 = 0.35;
const END_KM_PER_POINT: f32 = 0.04;
/// The gesture: three tile-zoom steps in under a second.
const ZOOM_FRAMES: usize = 44;
/// The same three steps as a fast wheel flick: ~320 ms, comfortably inside
/// one 150 ms fade of the level before, which is what exposes a fading
/// underlay standing where an opaque one should.
const FAST_ZOOM_FRAMES: usize = 20;
const FRAME_MS: u64 = 16;
/// How long the pane may take to become uniformly sharp before the test fails.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

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
            label: Some("quickzoom proof device"),
            ..Default::default()
        }))
        .expect("headless device");

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("quickzoom proof target"),
            size: wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
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
            label: Some("quickzoom proof readback"),
            size: u64::from(SIDE) * u64::from(SIDE) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut resources = CallbackResources::default();
        resources.insert(MapRenderResources::new(&device, format));
        resources.insert(TileRenderResources::new(&device, format));
        Some(Self {
            device,
            queue,
            resources,
            target,
            view,
            readback,
        })
    }

    /// One frame exactly as the pane draws it: clear to the ground, imagery on
    /// top, read the pixels back.
    fn frame(
        &mut self,
        tiles: Option<Arc<TileFrame>>,
        camera: Camera2D,
        canvas: [f64; 4],
    ) -> Vec<u8> {
        let viewport = ViewportMetrics {
            width_points: SIDE as f32,
            height_points: SIDE as f32,
            pixels_per_point: 1.0,
        };
        let rect_px = [0.0, 0.0, SIDE as f32, SIDE as f32];
        let screen = ScreenDescriptor {
            size_in_pixels: [SIDE, SIDE],
            pixels_per_point: 1.0,
        };
        let tile_callback = tiles.map(|frame| map_scene::gpu::TilePaintCallback {
            pane_index: 0,
            frame,
            camera,
            viewport,
            rect_px,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("quickzoom proof"),
            });
        if let Some(callback) = tile_callback.as_ref() {
            let extra = callback.prepare(
                &self.device,
                &self.queue,
                &screen,
                &mut encoder,
                &mut self.resources,
            );
            assert!(extra.is_empty(), "the tile callback queued command buffers");
        }
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("quickzoom proof pass"),
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
                egui::vec2(SIDE as f32, SIDE as f32),
            );
            let info = || egui::PaintCallbackInfo {
                viewport: whole,
                clip_rect: whole,
                pixels_per_point: 1.0,
                screen_size_px: [SIDE, SIDE],
            };
            let pixels = info().viewport_in_pixels();
            pass.set_viewport(
                pixels.left_px as f32,
                pixels.top_px as f32,
                pixels.width_px as f32,
                pixels.height_px as f32,
                0.0,
                1.0,
            );
            if let Some(callback) = tile_callback.as_ref() {
                callback.paint(info(), &mut pass, &self.resources);
            }
        }

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
                    bytes_per_row: Some(SIDE * 4),
                    rows_per_image: Some(SIDE),
                },
            },
            wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
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

fn output_dir() -> PathBuf {
    let path = std::env::var_os("MAP_SCENE_TILE_PROOF_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("map-scene-tile-proof"));
    std::fs::create_dir_all(&path).expect("create output directory");
    path
}

fn save(pixels: &[u8], name: &str) {
    let path = output_dir().join(name);
    let image =
        image::RgbaImage::from_raw(SIDE, SIDE, pixels.to_vec()).expect("read-back buffer is RGBA");
    image.save(&path).expect("write PNG");
    println!("wrote {}", path.display());
}

fn viewport() -> ViewportMetrics {
    ViewportMetrics {
        width_points: SIDE as f32,
        height_points: SIDE as f32,
        pixels_per_point: 1.0,
    }
}

fn painted_fraction(frame: &[u8], ground: &[u8]) -> f64 {
    let painted = frame
        .chunks_exact(4)
        .zip(ground.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    painted as f64 / (SIDE as f64 * SIDE as f64)
}

/// Distance of a tile's centre from the pane centre (the radar anchor), in
/// tile units at that tile's own zoom.
fn centre_distance(tile: basemap_tiles::TileId) -> f64 {
    let (cx, cy) = basemap_tiles::lon_lat_to_tile_xy(KTLX.1, KTLX.0, tile.z);
    (f64::from(tile.x) + 0.5 - cx).hypot(f64::from(tile.y) + 0.5 - cy)
}

/// One measured frame of the gesture.
struct FrameRecord {
    at_ms: u128,
    zoom: u8,
    draws: usize,
    exact: usize,
    /// Draws served by an ancestor texture, by how many levels up it sits.
    ancestor_levels: HashMap<u8, usize>,
    min_alpha: f32,
    painted: f64,
    /// The worst per-tile GROUND BLEED this frame: over every visible tile,
    /// the product of `1 - alpha` across the draws that cover it. This is
    /// exactly the weight the pane's own ground keeps in the premultiplied
    /// src-over composite the shader uses, so 0.0 is a true crossfade and 1.0
    /// is a tile showing bare ground. `painted_fraction` cannot see this — a
    /// pixel that is 60% ground and 40% imagery still differs from ground —
    /// which is why it is measured from the draw list, not the pixels.
    worst_bleed: f64,
    downloaded: u64,
    served_from_disk: u64,
    arrivals: Vec<basemap_tiles::TileId>,
}

struct GestureResult {
    records: Vec<FrameRecord>,
    settle_from_gesture_end: Duration,
    settle_from_gesture_start: Duration,
}

/// Warm the starting view, run the scripted quick zoom, then keep drawing
/// until the pane is uniformly sharp. Records every frame.
fn run_gesture(
    harness: &mut Harness,
    controller: &mut TileSceneController,
    label: &str,
    save_frames: bool,
    zoom_frames: usize,
) -> GestureResult {
    let chrome = MapStylePreset::Slate.chrome();
    let canvas = chrome.canvas.to_array().map(f64::from);
    let scrim_rgb = [canvas[0] as f32, canvas[1] as f32, canvas[2] as f32];
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let ground = harness.frame(None, Camera2D::default(), canvas);

    let camera_at = |km_per_point: f32| Camera2D {
        km_per_point,
        ..Camera2D::default()
    };

    // Settle the start view completely, as a parked user would have.
    let mut selector = LodSelector::new(START_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT);
    let warm_started = Instant::now();
    loop {
        controller.poll();
        let lod = selector.update(START_KM_PER_POINT);
        let frame = controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                lod,
                camera_at(START_KM_PER_POINT),
                viewport(),
                scrim_rgb,
            )
            .expect("start frame");
        harness.frame(
            Some(Arc::clone(&frame)),
            camera_at(START_KM_PER_POINT),
            canvas,
        );
        if frame.coverage >= 1.0 && frame.draws.iter().all(|draw| draw.alpha >= 1.0) {
            break;
        }
        assert!(
            warm_started.elapsed() < SETTLE_TIMEOUT,
            "the start view never settled: {:?}",
            controller.metrics().store
        );
        std::thread::sleep(Duration::from_millis(FRAME_MS));
    }

    // The gesture, then the wait for sharpness, all measured.
    let mut records: Vec<FrameRecord> = Vec::new();
    let mut seen_arrivals: HashSet<basemap_tiles::TileId> = HashSet::new();
    let mut baseline = controller.metrics().store;
    let started = Instant::now();
    let mut gesture_ended: Option<Instant> = None;
    let mut settled_at: Option<Instant> = None;
    let mut frame_index = 0_usize;
    let mut worst_bleed_seen = (0.0_f64, Vec::new(), 0_usize);
    let log_scale = (END_KM_PER_POINT / START_KM_PER_POINT).ln();
    while settled_at.is_none() {
        let scale = if frame_index < zoom_frames {
            let t = (frame_index + 1) as f32 / zoom_frames as f32;
            START_KM_PER_POINT * (log_scale * t).exp()
        } else {
            if gesture_ended.is_none() {
                gesture_ended = Some(Instant::now());
            }
            END_KM_PER_POINT
        };
        controller.poll();
        let lod = selector.update(scale);
        let frame = controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                lod,
                camera_at(scale),
                viewport(),
                scrim_rgb,
            )
            .expect("gesture frame");
        let arrivals: Vec<basemap_tiles::TileId> = frame
            .uploads
            .iter()
            .filter(|decoded| seen_arrivals.insert(decoded.tile))
            .map(|decoded| decoded.tile)
            .collect();
        let pixels = harness.frame(Some(Arc::clone(&frame)), camera_at(scale), canvas);

        let mut ancestor_levels: HashMap<u8, usize> = HashMap::new();
        let mut exact = 0_usize;
        let mut ground_weight: HashMap<basemap_tiles::TileId, f64> = HashMap::new();
        for draw in frame.draws.iter() {
            if draw.texture == draw.mesh.tile {
                exact += 1;
            } else {
                *ancestor_levels
                    .entry(draw.mesh.tile.z - draw.texture.z)
                    .or_default() += 1;
            }
            // The premultiplied src-over composite: each draw over this
            // tile's ground multiplies the ground's remaining weight by
            // (1 - alpha).
            *ground_weight.entry(draw.mesh.tile).or_insert(1.0) *=
                1.0 - f64::from(draw.alpha.clamp(0.0, 1.0));
        }
        let worst_bleed = ground_weight.values().copied().fold(0.0_f64, f64::max);
        let store = controller.metrics().store;
        let record = FrameRecord {
            at_ms: started.elapsed().as_millis(),
            zoom: frame.key.zoom,
            draws: frame.draws.len(),
            exact,
            ancestor_levels,
            min_alpha: frame
                .draws
                .iter()
                .map(|draw| draw.alpha)
                .fold(1.0_f32, f32::min),
            painted: painted_fraction(&pixels, &ground),
            worst_bleed,
            downloaded: store.downloaded - baseline.downloaded,
            served_from_disk: store.served_from_disk - baseline.served_from_disk,
            arrivals,
        };
        baseline = store;

        if save_frames
            && (frame_index.is_multiple_of(4) || frame_index == zoom_frames)
            && frame_index <= 96
        {
            save(&pixels, &format!("quickzoom-{label}-f{frame_index:03}.png"));
        }
        if worst_bleed > worst_bleed_seen.0 {
            worst_bleed_seen = (worst_bleed, pixels.clone(), frame_index);
        }
        // Uniformly sharp: everything visible draws its own tile at the zoom
        // the parked camera settled on (hysteresis makes that path dependent
        // and z11 is the honest answer for this gesture), every fade has
        // finished, and the whole pane is painted.
        let sharp = frame.coverage >= 1.0
            && frame.draws.iter().all(|draw| draw.alpha >= 1.0)
            && record.painted > 0.999;
        records.push(record);
        if sharp && frame_index >= ZOOM_FRAMES {
            settled_at = Some(Instant::now());
            if save_frames {
                save(&pixels, &format!("quickzoom-{label}-settled.png"));
            }
        }
        assert!(
            started.elapsed() < SETTLE_TIMEOUT,
            "{label}: never uniformly sharp; last painted {:.3}, coverage {:.2}, store {:?}",
            records.last().map_or(0.0, |record| record.painted),
            frame.coverage,
            controller.metrics().store
        );
        frame_index += 1;
        std::thread::sleep(Duration::from_millis(FRAME_MS));
    }

    if save_frames && worst_bleed_seen.0 > 0.0 {
        save(
            &worst_bleed_seen.1,
            &format!(
                "quickzoom-{label}-worstbleed-f{:03}.png",
                worst_bleed_seen.2
            ),
        );
    }
    let settled = settled_at.expect("settled");
    GestureResult {
        records,
        settle_from_gesture_end: settled
            .saturating_duration_since(gesture_ended.unwrap_or(settled)),
        settle_from_gesture_start: settled.saturating_duration_since(started),
    }
}

fn report(label: &str, result: &GestureResult) {
    println!("\n=== {label} ===");
    println!(
        "{:>6} {:>4} {:>5} {:>5} {:>16} {:>6} {:>7} {:>6} {:>5} {:>5}  arrivals(dist)",
        "ms", "z", "draws", "exact", "ancestors(lvl:n)", "alpha", "painted", "bleed", "dl", "disk"
    );
    for record in &result.records {
        let mut levels: Vec<(u8, usize)> = record
            .ancestor_levels
            .iter()
            .map(|(level, count)| (*level, *count))
            .collect();
        levels.sort_unstable();
        let levels: Vec<String> = levels
            .iter()
            .map(|(level, count)| format!("{level}:{count}"))
            .collect();
        let arrivals: Vec<String> = record
            .arrivals
            .iter()
            .map(|tile| format!("z{}({:.1})", tile.z, centre_distance(*tile)))
            .collect();
        println!(
            "{:>6} {:>4} {:>5} {:>5} {:>16} {:>6.2} {:>7.3} {:>6.3} {:>5} {:>5}  {}",
            record.at_ms,
            record.zoom,
            record.draws,
            record.exact,
            levels.join(" "),
            record.min_alpha,
            record.painted,
            record.worst_bleed,
            record.downloaded,
            record.served_from_disk,
            arrivals.join(" ")
        );
    }
    // The worst frame-to-frame LOSS of painted pane: a blink.
    let mut worst_blink = 0.0_f64;
    for pair in result.records.windows(2) {
        worst_blink = worst_blink.max(pair[0].painted - pair[1].painted);
    }
    let lowest = result
        .records
        .iter()
        .skip(1)
        .map(|record| record.painted)
        .fold(1.0_f64, f64::min);
    println!(
        "worst frame-to-frame loss of painted pane: {:.3}; lowest painted after start: {:.3}",
        worst_blink, lowest
    );
    let worst_bleed = result
        .records
        .iter()
        .map(|record| record.worst_bleed)
        .fold(0.0_f64, f64::max);
    println!("worst per-tile ground bleed across the run: {worst_bleed:.3}");
    println!(
        "uniformly sharp {} ms after the gesture ended ({} ms after it began)",
        result.settle_from_gesture_end.as_millis(),
        result.settle_from_gesture_start.as_millis()
    );
    // Fetch order: were the arrivals centre-out within each zoom?
    let mut by_zoom: HashMap<u8, Vec<f64>> = HashMap::new();
    for record in &result.records {
        for tile in &record.arrivals {
            by_zoom
                .entry(tile.z)
                .or_default()
                .push(centre_distance(*tile));
        }
    }
    let mut zooms: Vec<u8> = by_zoom.keys().copied().collect();
    zooms.sort_unstable();
    for zoom in zooms {
        let distances = &by_zoom[&zoom];
        if distances.len() < 4 {
            continue;
        }
        let half = distances.len() / 2;
        let first: f64 = distances[..half].iter().sum::<f64>() / half as f64;
        let second: f64 = distances[half..].iter().sum::<f64>() / (distances.len() - half) as f64;
        println!(
            "z{zoom}: mean centre-distance of first half of arrivals {first:.2}, second half \
             {second:.2} ({})",
            if first <= second {
                "centre-out"
            } else {
                "EDGE-FIRST: the queue inverted the request order"
            }
        );
    }
}

/// A controller on a private disk cache. `wipe` makes the run cold.
fn controller_with_cache(wipe: bool) -> TileSceneController {
    let root = std::env::temp_dir().join("map-scene-quickzoom-cache");
    if wipe {
        let _ = std::fs::remove_dir_all(&root);
    }
    let config = TileCacheConfig {
        disk_root: Some(root),
        ..TileCacheConfig::default()
    };
    let mut controller = TileSceneController::with_config(config, Arc::new(|| {}));
    controller.set_provider(Some(TileProvider::UsgsImageryTopo));
    controller
}

/// THE MEASUREMENT: a scripted quick zoom, KTLX z9 -> z12 in ~0.7 s, against
/// the live USGS service, cold disk then warm disk.
#[test]
#[ignore = "fetches real tiles over the network"]
fn a_quick_zoom_is_measured_frame_by_frame() {
    let Some(mut harness) = Harness::new() else {
        eprintln!("SKIPPED a_quick_zoom_is_measured_frame_by_frame: no wgpu adapter");
        return;
    };

    // COLD: nothing on disk, everything over the wire.
    let mut controller = controller_with_cache(true);
    let cold = run_gesture(&mut harness, &mut controller, "cold", true, ZOOM_FRAMES);
    report("cold quick zoom (empty disk cache)", &cold);
    let cold_prefetched = controller.metrics().tiles_prefetched;

    // WARM: same gesture, fresh controller and fresh GPU residency, tiles on
    // disk. This is the "come back to a site" case.
    let Some(mut harness2) = Harness::new() else {
        eprintln!("no second device; warm half skipped");
        return;
    };
    let mut controller = controller_with_cache(false);
    let warm = run_gesture(&mut harness2, &mut controller, "warm", true, ZOOM_FRAMES);
    report("warm quick zoom (tiles on disk)", &warm);

    // THE PINNED CLAIMS. Measured on 2026-08-19 against the live USGS
    // Imagery+Topo service, before and after the quick-zoom fixes:
    //
    //   before: cold painted fraction dipped to 0.344 mid-gesture and the
    //           warm gesture flashed the ENTIRE pane to bare ground
    //           (painted 0.000) at every zoom step, because an arriving tile
    //           replaced its covering ancestor at fade alpha 0;
    //   after:  painted 1.000 on every frame of both runs — a zoom is a
    //           crossfade, never a blink.
    for (label, result) in [("cold", &cold), ("warm", &warm)] {
        let mut worst_blink = 0.0_f64;
        for pair in result.records.windows(2) {
            worst_blink = worst_blink.max(pair[0].painted - pair[1].painted);
        }
        let lowest = result
            .records
            .iter()
            .skip(1)
            .map(|record| record.painted)
            .fold(1.0_f64, f64::min);
        assert!(
            lowest >= 0.99,
            "{label}: the pane fell to {lowest:.3} painted mid-gesture — imagery blinked \
             back to bare ground"
        );
        assert!(
            worst_blink <= 0.005,
            "{label}: a frame lost {worst_blink:.3} of its painted pane in one step"
        );
        assert!(
            result.records.iter().all(|record| record.draws > 0),
            "{label}: some frame drew nothing at all"
        );
        // The crossfade property itself: every visible tile keeps an OPAQUE
        // composite under its fade, so the pane's own ground never regains
        // weight mid-gesture. `painted_fraction` cannot police this (a 60%
        // ground / 40% imagery blend still counts as painted), so it is
        // asserted from the draw list.
        let worst_bleed = result
            .records
            .iter()
            .map(|record| record.worst_bleed)
            .fold(0.0_f64, f64::max);
        assert!(
            worst_bleed <= 0.05,
            "{label}: a tile's composite let {worst_bleed:.3} of bare ground bleed through — \
             its underlay was not opaque"
        );
        // Uniform sharpness is the last fade finishing, which is bounded by
        // the fetches plus one 150 ms fade; ten seconds is network slack.
        assert!(
            result.settle_from_gesture_end <= Duration::from_secs(10),
            "{label}: {} ms to uniform sharpness after the gesture",
            result.settle_from_gesture_end.as_millis()
        );
    }
    // The zoom boundary was warmed ahead of the camera (this is what put the
    // z10 tiles on screen the same frame the zoom flipped to them).
    assert!(
        cold_prefetched >= PREFETCH_FLOOR,
        "only {cold_prefetched} tiles were prefetched across a three-boundary gesture"
    );
}

/// The gesture crosses the z9/z10 and z10/z11 boundaries and parks in the
/// fine half of the final bucket, so at least one full boundary's worth of
/// centre tiles must have been warmed.
const PREFETCH_FLOOR: u64 = 8;

/// The same three zoom steps as a FAST wheel flick — ~320 ms end to end, so
/// each level's arrival lands inside the previous level's 150 ms fade — on a
/// warm disk. This is the gesture that catches a fading underlay: if the
/// texture drawn beneath a fading tile is itself mid-fade (or worse, had its
/// fade clock started the moment it was first used as an underlay), the
/// composite lets the pane's bare ground bleed through at every zoom flip
/// even though an OPAQUE grandparent is resident on the GPU. Measured on
/// 2026-08-19 against the live USGS service, before the underlay preferred a
/// settled ancestor: worst per-tile ground bleed 0.275 at the z10→z11 flip;
/// after: 0.000 on every frame — the settled ancestor carries the pane until
/// every fade above it finishes.
#[test]
#[ignore = "fetches real tiles over the network"]
fn a_fast_warm_flick_is_a_crossfade_not_a_ground_bleed() {
    let Some(mut harness) = Harness::new() else {
        eprintln!("SKIPPED a_fast_warm_flick_is_a_crossfade_not_a_ground_bleed: no wgpu adapter");
        return;
    };
    // Warm the whole gesture path on disk first, at the slow pace.
    let mut controller = controller_with_cache(true);
    let _ = run_gesture(&mut harness, &mut controller, "prewarm", false, ZOOM_FRAMES);

    // The flick, on a fresh controller and fresh GPU residency.
    let Some(mut harness2) = Harness::new() else {
        eprintln!("no second device; flick skipped");
        return;
    };
    let mut controller = controller_with_cache(false);
    let flick = run_gesture(
        &mut harness2,
        &mut controller,
        "flick",
        true,
        FAST_ZOOM_FRAMES,
    );
    report("fast warm flick (three levels in ~320 ms)", &flick);

    let worst_bleed = flick
        .records
        .iter()
        .map(|record| record.worst_bleed)
        .fold(0.0_f64, f64::max);
    assert!(
        worst_bleed <= 0.05,
        "the flick let {worst_bleed:.3} of bare ground bleed through a tile's composite — \
         the underlay beneath a fading tile was not opaque"
    );
    let lowest = flick
        .records
        .iter()
        .skip(1)
        .map(|record| record.painted)
        .fold(1.0_f64, f64::min);
    assert!(
        lowest >= 0.99,
        "the flick fell to {lowest:.3} painted — imagery blinked back to bare ground"
    );
}
