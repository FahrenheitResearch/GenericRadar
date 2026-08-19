//! The tile basemap, proved on a real GPU with real fetched imagery.
//!
//! Everything here renders through the shipping pipeline: the same
//! `TileSceneController` the application drives, the same `TilePaintCallback`
//! and `MapPaintCallback` the pane queues, into one render pass in the pane's
//! own order — ground, imagery, vector boundaries — and then reads the pixels
//! back and looks at them.
//!
//! The network tests are `#[ignore]`d so the ordinary gate never depends on
//! the internet or on a provider being up. Run them deliberately:
//!
//! ```text
//! cargo test --release -p map_scene --test tile_render_proof -- --ignored \
//!     --nocapture --test-threads=1
//! ```
//!
//! `MAP_SCENE_TILE_PROOF_OUT` chooses where the read-back frames are written
//! as PNG, so a human can look at them rather than trust a pixel count.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use analyst_runtime::{
    Camera2D, Generation, GeometryCacheKey, LodBucket, LodSelector, ViewportMetrics,
};
use eframe::egui;
use eframe::egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;
use map_scene::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
use map_scene::dataset::MapDataset;
use map_scene::geometry::MapGeometry;
use map_scene::gpu::{
    MapPaintCallback, MapRenderResources, TilePaintCallback, TileRenderResources,
};
use map_scene::projection::RadarProjection;
use map_scene::style_presets::MapStylePreset;
use map_scene::tiles::{TileFrame, TileSceneController};
use map_scene::{TileCacheConfig, TileProvider};

/// Square pane. 512 * 4 bytes is a multiple of the 256-byte row alignment
/// `copy_texture_to_buffer` requires, so the readback needs no padding.
const SIDE: u32 = 512;
/// KTLX, the same real site every other proof in this workspace uses.
const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);
/// How long to let a cold cache fill before giving up on the network.
const FILL_TIMEOUT: Duration = Duration::from_secs(60);

/// Drive a future to completion on this thread. wgpu's native backends resolve
/// `request_adapter` and `request_device` without ever needing to be woken.
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
    /// `None` when the machine has no wgpu adapter, so a headless build node
    /// skips loudly instead of pretending it measured something.
    fn new() -> Option<Self> {
        Self::with_tile_budget(Some(TileRenderResources::DEFAULT_BUDGET_BYTES))
    }

    /// A harness whose tile texture cache is deliberately far too small, so
    /// eviction runs every frame. This is the memory-pressure path, which is
    /// otherwise only reachable on a four-pane HiDPI layout at four different
    /// zooms.
    fn with_tile_budget_bytes(budget: usize) -> Option<Self> {
        Self::with_tile_budget(Some(budget))
    }

    /// A harness whose callback-resource store does NOT hold
    /// `TileRenderResources` - the application exactly as it shipped before
    /// this feature. Comparing a frame from one against a frame from the other
    /// is what makes the no-regression claim a measurement rather than a
    /// tautology.
    fn without_tile_layer() -> Option<Self> {
        Self::with_tile_budget(None)
    }

    fn with_tile_budget(tile_budget: Option<usize>) -> Option<Self> {
        let instance = wgpu::Instance::default();
        let adapter =
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        println!("adapter: {:?}", adapter.get_info());
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("tile proof device"),
            ..Default::default()
        }))
        .expect("headless device");

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tile proof target"),
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
            label: Some("tile proof readback"),
            size: u64::from(SIDE) * u64::from(SIDE) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut resources = CallbackResources::default();
        resources.insert(MapRenderResources::new(&device, format));
        if let Some(budget) = tile_budget {
            resources.insert(TileRenderResources::with_budget(&device, format, budget));
        }
        Some(Self {
            device,
            queue,
            resources,
            target,
            view,
            readback,
        })
    }

    /// One honest frame in the pane's own order: clear to the chrome ground,
    /// draw the imagery, then draw the vector boundaries over it.
    fn frame(
        &mut self,
        tiles: Option<Arc<TileFrame>>,
        geometry: Option<Arc<MapGeometry>>,
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
        let tile_callback = tiles.map(|frame| TilePaintCallback {
            pane_index: 0,
            frame,
            camera,
            viewport,
            rect_px,
        });
        let map_callback = geometry.map(|geometry| MapPaintCallback {
            pane_index: 0,
            geometry,
            camera,
            viewport,
            rect_px,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tile proof"),
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
        if let Some(callback) = map_callback.as_ref() {
            let extra = callback.prepare(
                &self.device,
                &self.queue,
                &screen,
                &mut encoder,
                &mut self.resources,
            );
            assert!(extra.is_empty(), "the map callback queued command buffers");
        }

        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tile proof pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // The pane's own `rect_filled(rect, chrome.canvas)`
                            // becomes the clear here: same ground, one draw
                            // earlier.
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
            // `PaintCallbackInfo` is neither `Copy` nor `Clone`, and both
            // callbacks need one, so it is built per callback exactly as
            // `egui_wgpu::Renderer::render` builds it per primitive.
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
            // Imagery is the ground, so it goes under the boundaries, which
            // are the part that has to stay legible on top of a photograph.
            if let Some(callback) = tile_callback.as_ref() {
                callback.paint(info(), &mut pass, &self.resources);
            }
            if let Some(callback) = map_callback.as_ref() {
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

fn geometry_for(preset: MapStylePreset, camera: Camera2D) -> Arc<MapGeometry> {
    let lod = LodSelector::new(camera.km_per_point, LOD_REFERENCE_KM_PER_POINT).current();
    Arc::new(build_geometry(&MapBuildRequest {
        key: GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod,
        },
        dataset: MapDataset::from_generated(Generation::new(1)),
        projection: RadarProjection::new(KTLX.0, KTLX.1),
        style: preset.style(),
    }))
}

fn bucket(km_per_point: f32) -> LodBucket {
    LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current()
}

fn viewport() -> ViewportMetrics {
    ViewportMetrics {
        width_points: SIDE as f32,
        height_points: SIDE as f32,
        pixels_per_point: 1.0,
    }
}

fn output_dir() -> PathBuf {
    let path = std::env::var_os("MAP_SCENE_TILE_PROOF_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("map-scene-tile-proof"));
    std::fs::create_dir_all(&path).expect("create output directory");
    path
}

fn save(pixels: &[u8], name: &str) -> PathBuf {
    let directory = output_dir();
    let path = directory.join(name);
    let image =
        image::RgbaImage::from_raw(SIDE, SIDE, pixels.to_vec()).expect("read-back buffer is RGBA");
    image.save(&path).expect("write PNG");
    println!("wrote {}", path.display());
    path
}

/// Distinct RGB values in the frame. Imagery has thousands; a flat fill or a
/// blank pane has a handful.
fn distinct_colors(pixels: &[u8]) -> usize {
    let mut seen: HashMap<[u8; 3], u32> = HashMap::new();
    for pixel in pixels.chunks_exact(4) {
        *seen.entry([pixel[0], pixel[1], pixel[2]]).or_default() += 1;
    }
    seen.len()
}

fn differing(a: &[u8], b: &[u8]) -> usize {
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(left, right)| left != right)
        .count()
}

/// A controller with its own throwaway disk cache, so a test never writes into
/// the user's cache directory and a rerun is warm.
fn controller(provider: TileProvider) -> TileSceneController {
    let root = std::env::temp_dir().join("map-scene-tile-proof-cache");
    let config = TileCacheConfig {
        disk_root: Some(root),
        ..TileCacheConfig::default()
    };
    let mut controller = TileSceneController::with_config(config, Arc::new(|| {}));
    controller.set_provider(Some(provider));
    controller
}

thread_local! {
    /// Every decoded tile `fill` has seen, kept so a test can compare the
    /// rendered frame against the provider's own pixels.
    static DECODED: std::cell::RefCell<HashMap<basemap_tiles::TileId, Arc<basemap_tiles::DecodedTile>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Drive real frames until the imagery has arrived, exactly as the application
/// would: poll, build the pane's frame, render it, repeat.
fn fill(
    harness: &mut Harness,
    controller: &mut TileSceneController,
    camera: Camera2D,
    canvas: [f64; 4],
    want_exact_coverage: bool,
) -> (Arc<TileFrame>, usize) {
    // The scrim is the pane's own ground over the imagery, exactly as
    // `MapSceneController::tiles_for_pane` supplies it.
    let scrim_rgb = [canvas[0] as f32, canvas[1] as f32, canvas[2] as f32];
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let started = Instant::now();
    let mut frames = 0;
    loop {
        controller.poll();
        let frame = controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(camera.km_per_point),
                camera,
                viewport(),
                scrim_rgb,
            )
            .expect("the controller must produce a frame at this camera");
        // Keep every decoded tile that passes through, so the registration
        // check below can compare the frame against the pixels the provider
        // actually sent. They are gone from the store once the GPU has them.
        for decoded in frame.uploads.iter() {
            DECODED.with_borrow_mut(|seen| {
                seen.insert(decoded.tile, Arc::clone(decoded));
            });
        }
        harness.frame(Some(Arc::clone(&frame)), None, camera, canvas);
        frames += 1;
        // Settled means the picture has STOPPED CHANGING, which is not the
        // same as "something drew". A tile fades in over `FADE_SECONDS`, so a
        // frame captured while any draw is still below full opacity is a whole
        // tile darker than the finished picture - measured at 13.6 code values
        // over one ancestor tile's footprint, which is visible as a block in
        // the saved PNG and differs from run to run. Waiting for every alpha
        // makes what a human looks at the picture the application settles on.
        let opaque = frame.draws.iter().all(|draw| draw.alpha >= 1.0);
        let settled = opaque
            && if want_exact_coverage {
                frame.coverage >= 1.0
            } else {
                !frame.draws.is_empty()
            };
        // One more frame after everything lands, so the fade reaches 1.0.
        if settled && started.elapsed() > Duration::from_millis(400) {
            return (frame, frames);
        }
        if started.elapsed() > FILL_TIMEOUT {
            let metrics = controller.metrics();
            panic!(
                "imagery never arrived in {FILL_TIMEOUT:?}: {:?} — is the network reachable?",
                metrics.store
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// THE PROOF: real tiles, fetched over the network for a real radar site,
/// uploaded to a real GPU, drawn under the real vector basemap, read back.
#[test]
#[ignore = "fetches real tiles over the network"]
fn real_imagery_draws_under_the_real_vector_basemap() {
    let Some(mut harness) = Harness::new() else {
        eprintln!(
            "SKIPPED real_imagery_draws_under_the_real_vector_basemap: no wgpu adapter on this \
             machine, so the GPU half of the tile basemap is UNPROVEN here"
        );
        return;
    };

    let preset = MapStylePreset::Slate;
    let chrome = preset.chrome();
    let canvas = chrome.canvas.to_array().map(f64::from);
    let camera = Camera2D::default();
    let geometry = geometry_for(preset, camera);

    let mut controller = controller(TileProvider::UsgsImageryTopo);
    let (frame, frames) = fill(&mut harness, &mut controller, camera, canvas, true);
    let metrics = controller.metrics();
    let residency = harness
        .resources
        .get::<TileRenderResources>()
        .expect("tile resources")
        .metrics();
    println!(
        "frames {frames}  draws {}  coverage {:.2}  zoom {}  imagery luminance {:?}  scrim {:.2}\
         \nstore {:?}\nresidency {:?}",
        frame.draws.len(),
        frame.coverage,
        frame.key.zoom,
        controller.imagery_luminance(TileProvider::UsgsImageryTopo),
        frame.scrim[3],
        metrics.store,
        residency,
    );

    // The imagery is REAL: it came off the wire, not out of a fixture.
    assert!(
        metrics.store.downloaded > 0 || metrics.store.served_from_disk > 0,
        "no tile was ever obtained: {:?}",
        metrics.store
    );
    assert!(residency.uploads > 0, "nothing reached the GPU");
    assert_eq!(frame.coverage, 1.0, "every tile drew at its own zoom");

    // Three frames of the same camera: the ground alone, the imagery on it,
    // and the finished pane.
    let ground = harness.frame(None, None, camera, canvas);
    let tiles_only = harness.frame(Some(Arc::clone(&frame)), None, camera, canvas);
    let vector_only = harness.frame(None, Some(Arc::clone(&geometry)), camera, canvas);
    let composite = harness.frame(Some(Arc::clone(&frame)), Some(geometry), camera, canvas);
    save(&tiles_only, "ktlx-tiles-only.png");
    save(&vector_only, "ktlx-vector-only.png");
    save(&composite, "ktlx-composite.png");

    let total = (SIDE * SIDE) as usize;
    let painted = differing(&tiles_only, &ground);
    println!(
        "painted by imagery {painted}/{total}  distinct colours: ground {}, imagery {}, \
         composite {}",
        distinct_colors(&ground),
        distinct_colors(&tiles_only),
        distinct_colors(&composite),
    );
    assert!(
        painted > total * 9 / 10,
        "imagery covered only {painted} of {total} pixels"
    );
    // A photograph, not a fill: a flat or blank layer would have a handful of
    // colours, and the untouched ground has exactly one.
    assert_eq!(distinct_colors(&ground), 1);
    assert!(
        distinct_colors(&tiles_only) > 5_000,
        "the imagery has only {} distinct colours, which is not a photograph",
        distinct_colors(&tiles_only)
    );

    // LAYERING, stated exactly. Where the vector map drew ink, the composite
    // must differ from the imagery alone: the boundaries are ON TOP. Where it
    // did not, the composite must be the imagery, unchanged, byte for byte.
    let ground_pixel: [u8; 4] = ground[..4].try_into().expect("rgba");
    let mut ink = 0_usize;
    let mut ink_over_imagery = 0_usize;
    let mut disturbed = 0_usize;
    for index in 0..total {
        let range = index * 4..index * 4 + 4;
        let is_ink = vector_only[range.clone()] != ground_pixel;
        let changed = composite[range.clone()] != tiles_only[range.clone()];
        if is_ink {
            ink += 1;
            if changed {
                ink_over_imagery += 1;
            }
        } else if changed {
            // A stroke so faint it left the near-black ground unchanged can
            // still show over imagery, so this is counted rather than
            // forbidden - but it has to stay negligible, or the vector layer
            // is quietly repainting ground it never drew on.
            disturbed += 1;
        }
    }
    println!(
        "vector ink {ink} px, of which {ink_over_imagery} survive over imagery;          {disturbed} px changed where no ink was drawn"
    );
    assert!(ink > 1_000, "the vector basemap drew almost nothing: {ink}");
    assert!(
        ink_over_imagery * 100 >= ink * 95,
        "only {ink_over_imagery} of {ink} ink pixels survived over the imagery"
    );
    assert!(
        disturbed * 1_000 <= total,
        "the vector layer changed {disturbed} pixels it never drew on"
    );
}

/// The ancestor fallback, on a provider that really does answer 404 here.
///
/// USGS shaded relief has no z9 tile over Oklahoma City — every tile in this
/// pane is a 404 — so the only thing that can draw is the z8 parent. This is
/// the case that turns a checkerboard of holes into a coarser picture.
#[test]
#[ignore = "fetches real tiles over the network"]
fn a_provider_with_a_real_hole_falls_back_to_its_ancestors() {
    let Some(mut harness) = Harness::new() else {
        eprintln!(
            "SKIPPED a_provider_with_a_real_hole_falls_back_to_its_ancestors: no wgpu adapter"
        );
        return;
    };
    let chrome = MapStylePreset::Slate.chrome();
    let canvas = chrome.canvas.to_array().map(f64::from);
    let camera = Camera2D::default();

    let mut controller = controller(TileProvider::UsgsShadedRelief);
    let (frame, frames) = fill(&mut harness, &mut controller, camera, canvas, false);
    let metrics = controller.metrics();
    println!(
        "frames {frames}  draws {}  coverage {:.2}  zoom {}  absent {}  ancestors {}  \
         luminance {:?}  scrim {:.2}",
        frame.draws.len(),
        frame.coverage,
        frame.key.zoom,
        metrics.store.absent,
        metrics.ancestor_substitutions,
        controller.imagery_luminance(TileProvider::UsgsShadedRelief),
        frame.scrim[3],
    );

    assert!(metrics.store.absent > 0, "expected real 404s at z9 here");
    assert!(!frame.draws.is_empty(), "the hole was not filled at all");
    assert!(
        frame.draws.iter().all(|draw| draw.alpha >= 1.0),
        "a tile was still fading in, so the frame below is not the settled picture"
    );
    for draw in frame.draws.iter() {
        assert!(
            draw.texture.z < draw.mesh.tile.z,
            "expected an ancestor texture, got the tile itself"
        );
        // The UV window has to be the child's share of the parent, or the
        // fallback would draw the wrong ground.
        let expected = draw
            .mesh
            .tile
            .uv_offset_scale_within(draw.texture)
            .expect("ancestor");
        assert_eq!(draw.uv_offset_scale, expected);
    }
    let pixels = harness.frame(Some(Arc::clone(&frame)), None, camera, canvas);
    save(&pixels, "ktlx-shaded-relief-ancestor.png");
    let ground = harness.frame(None, None, camera, canvas);
    let painted = differing(&pixels, &ground);
    println!("painted by the ancestor texture {painted} px");
    assert!(painted > (SIDE * SIDE) as usize / 2);
}

/// The regression guard: with no provider, the pane is what it has always
/// been. No tile callback is queued, and the pixels are the vector map's own.
#[test]
fn slate_is_byte_identical_when_no_imagery_is_selected() {
    let Some(mut harness) = Harness::new() else {
        eprintln!(
            "SKIPPED slate_is_byte_identical_when_no_imagery_is_selected: no wgpu adapter, so \
             the no-regression claim is UNPROVEN here"
        );
        return;
    };
    let preset = MapStylePreset::Slate;
    let chrome = preset.chrome();
    let canvas = chrome.canvas.to_array().map(f64::from);
    let camera = Camera2D::default();
    let geometry = geometry_for(preset, camera);

    // A controller with no provider must produce no frame at all, offline or
    // not, so nothing of the tile layer can reach the pass.
    let mut controller = TileSceneController::with_config(
        TileCacheConfig {
            disk_root: None,
            max_disk_bytes: 0,
            max_workers: 1,
            user_agent: "radar-workstation-test/0 (+https://example.invalid)".to_owned(),
            offline: true,
        },
        Arc::new(|| {}),
    );
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    assert!(
        controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(camera.km_per_point),
                camera,
                viewport(),
                [0.0, 0.0, 0.0],
            )
            .is_none(),
        "no provider must mean no tile frame"
    );

    // The comparison that matters: the SAME pane rendered by a harness that
    // has the tile layer registered and by one that does not. Rendering it
    // twice through one harness would only prove the renderer is
    // deterministic, which was never in doubt.
    let with_tile_layer = harness.frame(None, Some(Arc::clone(&geometry)), camera, canvas);
    let mut shipped = Harness::without_tile_layer().expect("a second device on the same adapter");
    let without = shipped.frame(None, Some(geometry), camera, canvas);
    assert_eq!(
        with_tile_layer, without,
        "registering the tile layer changed the vector-only pane"
    );
    let ground: [u8; 4] = with_tile_layer[..4].try_into().expect("rgba");
    assert_eq!(
        ground,
        chrome.canvas.to_rgba8(),
        "Slate no longer clears to its own ground"
    );
    let drawn = with_tile_layer
        .chunks_exact(4)
        .filter(|pixel| *pixel != ground)
        .count();
    println!("Slate, vector only: {drawn} line pixels of {}", SIDE * SIDE);
    assert!(drawn > 1_000, "Slate drew almost nothing: {drawn}");
    save(&with_tile_layer, "ktlx-slate-vector-only.png");
}

/// MEMORY PRESSURE, on real hardware with real imagery.
///
/// The shipped budget is 160 MiB, about 480 tiles, and the layout that exceeds
/// it is four panes at four different zooms on a HiDPI display - which is a
/// real layout and which nothing else here exercises. This forces the same
/// condition with a budget of eight tiles and asserts the three things that
/// have to hold when the cache cannot hold the pane:
///
/// * the pane keeps drawing rather than going blank or panicking,
/// * the eviction backchannel is answered, so an evicted tile is refetched
///   rather than becoming a permanent hole (the failure mode that appears only
///   under pressure and never in a demo), and
/// * the refetch comes off the DISK, so thrashing a texture cache can never
///   turn into hammering a provider. That last one is a policy obligation, not
///   a performance nicety.
#[test]
#[ignore = "fetches real tiles over the network"]
fn a_texture_budget_too_small_for_the_pane_thrashes_without_hammering_the_provider() {
    // Eight tiles' worth: a 256x256 RGBA tile with its mip chain is about
    // 349 KiB, and the pane below needs sixteen.
    const BUDGET: usize = 8 * 349 * 1024;

    let Some(mut harness) = Harness::with_tile_budget_bytes(BUDGET) else {
        eprintln!(
            "SKIPPED a_texture_budget_too_small_for_the_pane_thrashes_without_hammering_the_provider: \
             no wgpu adapter, so the eviction path is UNPROVEN here"
        );
        return;
    };
    let chrome = MapStylePreset::Slate.chrome();
    let canvas = chrome.canvas.to_array().map(f64::from);
    let camera = Camera2D::default();

    let mut controller = controller(TileProvider::UsgsImageryTopo);
    // Fill once so the disk cache is warm, then take the download count: from
    // here on, nothing may reach the network again.
    let _ = fill(&mut harness, &mut controller, camera, canvas, false);
    let downloaded_after_fill = controller.metrics().store.downloaded;

    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let mut drew_something = 0;
    // A tile that is evicted and re-uploaded must NOT fade in again: under
    // sustained pressure that is a pane that pulses forever instead of
    // settling.
    let mut still_opaque = 0;
    for _ in 0..40 {
        controller.poll();
        let frame = controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(camera.km_per_point),
                camera,
                viewport(),
                [canvas[0] as f32, canvas[1] as f32, canvas[2] as f32],
            )
            .expect("the controller must keep producing a frame under pressure");
        harness.frame(Some(frame.clone()), None, camera, canvas);
        if !frame.draws.is_empty() {
            drew_something += 1;
            if frame.draws.iter().all(|draw| draw.alpha >= 1.0) {
                still_opaque += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let residency = harness
        .resources
        .get::<TileRenderResources>()
        .expect("tile resources")
        .metrics();
    let metrics = controller.metrics();
    println!(
        "under an {BUDGET} byte budget: {residency:?}\nstore {:?}\nforgotten {} \
         frames-with-draws {drew_something}/40",
        metrics.store, metrics.textures_forgotten
    );

    assert!(
        residency.evictions > 0,
        "the budget was never exceeded, so this test proves nothing: {residency:?}"
    );
    assert!(
        residency.resident_bytes <= BUDGET,
        "the texture cache is {} bytes over an {BUDGET} byte budget",
        residency.resident_bytes
    );
    assert!(
        metrics.textures_forgotten > 0,
        "the scene never heard about an eviction, so those tiles are permanent holes"
    );
    assert!(
        drew_something >= 30,
        "the pane went blank under pressure: only {drew_something} of 40 frames drew"
    );
    assert!(
        still_opaque >= 30,
        "the pane pulsed under pressure: only {still_opaque} of 40 frames were fully          opaque, so an evicted tile is fading in again every time it returns"
    );
    assert_eq!(
        metrics.store.downloaded,
        downloaded_after_fill,
        "thrashing the texture cache went back to the provider {} times",
        metrics.store.downloaded - downloaded_after_fill
    );
    assert_eq!(
        metrics.store.failed, 0,
        "a refetch failed: {:?}",
        metrics.store
    );
}

/// REGISTRATION: the imagery at a pane pixel must be the imagery OF the ground
/// at that pane pixel.
///
/// Nothing else in this suite asserts this, and it is the property the whole
/// feature rests on. Every existing check - pixels painted, distinct colours,
/// ink surviving, coverage 1.0 - passes just as happily if the basemap is
/// mirrored, offset by half a tile, or drawing the wrong tile entirely: a
/// wrong picture is still a picture. A mirrored map is also exactly what a
/// flipped `v` in the mesh or in the shader produces, and it looks plausible.
///
/// So this compares the READ-BACK frame against the provider's OWN decoded
/// pixels, through the camera and the projection, at hundreds of sample
/// points, and then does the same for three deliberately wrong hypotheses:
/// mirrored, shifted half a tile east, shifted half a tile south. The truth
/// has to beat all three by a wide margin, which is a claim no amount of
/// filtering or mip selection can fake.
#[test]
#[ignore = "fetches real tiles over the network"]
fn the_imagery_is_registered_to_the_ground_it_is_drawn_over() {
    let Some(mut harness) = Harness::new() else {
        eprintln!(
            "SKIPPED the_imagery_is_registered_to_the_ground_it_is_drawn_over: no wgpu adapter, \
             so the registration of the imagery is UNPROVEN here"
        );
        return;
    };
    let chrome = MapStylePreset::Slate.chrome();
    let canvas = chrome.canvas.to_array().map(f64::from);
    let camera = Camera2D::default();
    let mut controller = controller(TileProvider::UsgsImageryTopo);
    DECODED.with_borrow_mut(HashMap::clear);
    let (frame, _) = fill(&mut harness, &mut controller, camera, canvas, true);
    let pixels = harness.frame(Some(Arc::clone(&frame)), None, camera, canvas);

    let decoded = DECODED.with_borrow(|seen| seen.clone());
    assert!(
        decoded.len() >= 16,
        "only {} decoded tiles were captured",
        decoded.len()
    );
    let zoom = frame.key.zoom;
    let scrim = frame.scrim;
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let view = viewport();

    // (label, shift in fractional tile x, shift in fractional tile y, mirror v)
    let hypotheses: [(&str, f64, f64, bool); 4] = [
        ("as drawn", 0.0, 0.0, false),
        ("mirrored north-south", 0.0, 0.0, true),
        ("half a tile east", 0.5, 0.0, false),
        ("half a tile south", 0.0, 0.5, false),
    ];
    let mut errors: Vec<Vec<f64>> = vec![Vec::new(); hypotheses.len()];

    // A margin, because a pane-edge pixel can sample a tile that was never
    // captured, and the sample step is prime-ish so the grid cannot land on a
    // tile lattice.
    for row in (24..SIDE - 24).step_by(7) {
        for column in (24..SIDE - 24).step_by(7) {
            let screen = analyst_runtime::ScreenPoint::new(column as f32 + 0.5, row as f32 + 0.5);
            let world = camera.screen_to_world(screen, view);
            let (lon, lat) = projection.world_to_lon_lat(world);
            if !lon.is_finite() || !lat.is_finite() {
                continue;
            }
            let index = (row as usize * SIDE as usize + column as usize) * 4;
            let drawn = [
                f64::from(pixels[index]),
                f64::from(pixels[index + 1]),
                f64::from(pixels[index + 2]),
            ];
            let (fx, fy) = basemap_tiles::lon_lat_to_tile_xy(lon, lat, zoom);

            for (slot, (_, shift_x, shift_y, mirror)) in hypotheses.iter().enumerate() {
                let fx = fx + shift_x;
                let fy = fy + shift_y;
                let Some(tile) = basemap_tiles::TileId::new(zoom, fx as u32, fy as u32) else {
                    continue;
                };
                let Some(decoded) = decoded.get(&tile) else {
                    continue;
                };
                let u = fx.fract();
                let v = if *mirror {
                    1.0 - fy.fract()
                } else {
                    fy.fract()
                };
                let Some(expected) = expected_texel(decoded, u, v, scrim) else {
                    continue;
                };
                let error = (0..3)
                    .map(|channel| (drawn[channel] - expected[channel]).abs())
                    .sum::<f64>()
                    / 3.0;
                errors[slot].push(error);
            }
        }
    }

    let mut medians = Vec::new();
    for (slot, (label, ..)) in hypotheses.iter().enumerate() {
        let mut sample = std::mem::take(&mut errors[slot]);
        assert!(
            sample.len() > 1_000,
            "{label}: only {} samples landed on a captured tile",
            sample.len()
        );
        sample.sort_by(|left, right| left.partial_cmp(right).expect("finite"));
        let median = sample[sample.len() / 2];
        println!(
            "{label}: median error {median:.2} of 255 over {} samples",
            sample.len()
        );
        medians.push(median);
    }

    // The truth has to be close in absolute terms - the residual is bilinear
    // filtering and mip selection against a point-sampled expectation, not
    // misregistration.
    assert!(
        medians[0] < 12.0,
        "the imagery does not match the ground it covers: median error {:.2}",
        medians[0]
    );
    // And decisively better than every wrong placement. A basemap that is
    // mirrored or a half tile out cannot pass this.
    for (slot, (label, ..)) in hypotheses.iter().enumerate().skip(1) {
        assert!(
            medians[slot] > medians[0] * 2.0,
            "'{label}' fits the frame nearly as well ({:.2}) as the true placement ({:.2}), \
             so this test cannot tell a misregistered basemap from a correct one",
            medians[slot],
            medians[0]
        );
    }
}

/// The colour the tile shader should produce for the texel at `(u, v)`: a 2x2
/// box of level-0 texels, mixed towards the pane's ground by the scrim exactly
/// as `tile_shader.wgsl` does. `None` where the tile has no level 0.
fn expected_texel(
    decoded: &basemap_tiles::DecodedTile,
    u: f64,
    v: f64,
    scrim: [f32; 4],
) -> Option<[f64; 3]> {
    let (texels, side) = decoded.level(0)?;
    let side = side as i64;
    let x = (u * side as f64 - 0.5).floor() as i64;
    let y = (v * side as f64 - 0.5).floor() as i64;
    let mut total = [0.0_f64; 3];
    let mut counted = 0.0_f64;
    for dy in 0..2 {
        for dx in 0..2 {
            let sx = (x + dx).clamp(0, side - 1);
            let sy = (y + dy).clamp(0, side - 1);
            let index = ((sy * side + sx) * 4) as usize;
            for channel in 0..3 {
                total[channel] += f64::from(texels[index + channel]);
            }
            counted += 1.0;
        }
    }
    let alpha = f64::from(scrim[3]);
    let mut out = [0.0_f64; 3];
    for channel in 0..3 {
        let texel = total[channel] / counted / 255.0;
        let ground = f64::from(scrim[channel]);
        out[channel] = (texel * (1.0 - alpha) + ground * alpha) * 255.0;
    }
    Some(out)
}
