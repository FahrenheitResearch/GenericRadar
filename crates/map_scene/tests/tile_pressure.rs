//! What a long session does to the tile layer, measured rather than assumed.
//!
//! Everything else in this crate's suite drives one camera for a handful of
//! frames. An operator drives one for hours, and the failure that costs them a
//! session is not a wrong pixel - it is a cache that only ever grows. These
//! tests pan a real pane across a real site's domain, at the zooms the layer
//! actually draws, until the mesh cache is genuinely over its bound, and then
//! assert both that the bound holds AND that the working set on screen
//! survived the eviction that enforced it.
//!
//! Offline with no disk root throughout, so nothing here touches the network
//! or the user's cache directory: the mesh cache is a pure function of the
//! camera and the projection and needs no imagery to exercise.

use std::sync::Arc;

use analyst_runtime::{Camera2D, Generation, LodBucket, LodSelector, ViewportMetrics};
use basemap_tiles::{TileCacheConfig, TileProvider};
use map_scene::build::LOD_REFERENCE_KM_PER_POINT;
use map_scene::projection::RadarProjection;
use map_scene::tiles::{MAX_RESIDENT_MESHES, TileSceneController};

const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);

fn controller() -> TileSceneController {
    TileSceneController::with_config(
        TileCacheConfig {
            disk_root: None,
            max_disk_bytes: 0,
            max_workers: 1,
            user_agent: "radar-workstation-test/0 (+https://example.invalid)".to_owned(),
            offline: true,
        },
        Arc::new(|| {}),
    )
}

fn viewport() -> ViewportMetrics {
    ViewportMetrics {
        width_points: 1500.0,
        height_points: 950.0,
        pixels_per_point: 1.0,
    }
}

fn bucket(km_per_point: f32) -> LodBucket {
    LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current()
}

fn frame_at(controller: &mut TileSceneController, camera: Camera2D) {
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    controller.poll();
    controller.frame_for_pane(
        &projection,
        Generation::new(1),
        bucket(camera.km_per_point),
        camera,
        viewport(),
        [0.05, 0.05, 0.06],
    );
}

/// Drag a pane across `distance_km` of ground in `steps`, the way a hand on a
/// mouse does.
fn sweep(controller: &mut TileSceneController, km_per_point: f32, distance_km: f64, steps: usize) {
    for step in 0..steps {
        let offset = distance_km * (step as f64 / steps as f64);
        frame_at(
            controller,
            Camera2D {
                center_east_km: offset,
                center_north_km: offset * 0.25,
                km_per_point,
                rotation_rad: 0.0,
            },
        );
    }
}

/// THE LEAK TEST: a long pan must not grow the mesh cache without bound, and
/// the eviction that enforces that must actually run.
///
/// A mesh is cached on `(TileId, projection)` and the projection does not
/// change while an operator works one site, so without an eviction policy the
/// cache holds every tile the camera has ever crossed.
#[test]
fn a_long_pan_evicts_rather_than_growing_the_mesh_cache_without_bound() {
    let mut controller = controller();
    controller.set_provider(Some(TileProvider::UsgsImageryTopo));

    // Far enough to put the cache genuinely over its bound: a fine camera
    // crossing 1400 km of ground is an afternoon of following a line east.
    sweep(&mut controller, 0.01, 1_400.0, 700);
    let metrics = controller.metrics();
    println!(
        "after 1400 km at 0.01 km/point: {} meshes resident, {} bytes, {} evicted",
        metrics.meshes_resident, metrics.mesh_bytes, metrics.meshes_evicted
    );
    assert!(
        metrics.meshes_built > MAX_RESIDENT_MESHES as u64,
        "only {} meshes were ever built, so the bound was never approached and \
         this test proves nothing",
        metrics.meshes_built
    );
    assert!(
        metrics.meshes_evicted > 0,
        "the cache never evicted anything, so the bound is not being enforced"
    );
    assert!(
        metrics.meshes_resident <= MAX_RESIDENT_MESHES,
        "the mesh cache holds {} meshes, past its {MAX_RESIDENT_MESHES} bound",
        metrics.meshes_resident
    );
    assert!(
        metrics.mesh_bytes < 16 * 1024 * 1024,
        "the mesh cache holds {} bytes",
        metrics.mesh_bytes
    );

    // And it stays bounded: a second sweep does not ratchet the ceiling up.
    let after_first = controller.metrics().meshes_resident;
    sweep(&mut controller, 0.01, 1_400.0, 700);
    let metrics = controller.metrics();
    assert!(
        metrics.meshes_resident <= MAX_RESIDENT_MESHES,
        "a second pass left {} meshes (first pass left {after_first})",
        metrics.meshes_resident
    );
}

/// Eviction must not cost the pan invariant the mesh cache exists for: the
/// tiles on screen right now are the most recently used, so they survive every
/// sweep and a parked or nudged camera still rebuilds nothing.
///
/// This is the assertion that would fail if the eviction policy were anything
/// other than least-recently-used - a random or oldest-first sweep would drop
/// tiles that are still on screen and rebuild them the very next frame.
#[test]
fn eviction_never_touches_the_tiles_that_are_on_screen() {
    let mut controller = controller();
    controller.set_provider(Some(TileProvider::UsgsImageryTopo));
    sweep(&mut controller, 0.01, 1_400.0, 700);
    assert!(
        controller.metrics().meshes_evicted > 0,
        "the cache never filled, so this proves nothing about eviction"
    );

    // Park the camera where the sweep left it and let the pane settle.
    let camera = Camera2D {
        center_east_km: 1_400.0,
        center_north_km: 350.0,
        km_per_point: 0.01,
        rotation_rad: 0.0,
    };
    for _ in 0..8 {
        frame_at(&mut controller, camera);
    }
    let settled = controller.metrics().meshes_built;

    frame_at(&mut controller, camera);
    assert_eq!(
        controller.metrics().meshes_built,
        settled,
        "a parked camera rebuilt meshes it had already built"
    );

    // A nudge of a fraction of a tile is the same tile set, so it too must
    // rebuild nothing.
    frame_at(
        &mut controller,
        Camera2D {
            center_east_km: 1_400.05,
            ..camera
        },
    );
    assert_eq!(
        controller.metrics().meshes_built,
        settled,
        "a sub-tile pan rebuilt meshes"
    );
}

/// Switching sites must not leave the previous site's meshes behind: they are
/// geometry in the old anchor's frame and are worthless in the new one.
#[test]
fn changing_the_anchor_releases_the_previous_sites_meshes() {
    let mut controller = controller();
    controller.set_provider(Some(TileProvider::UsgsImageryTopo));
    sweep(&mut controller, 0.01, 400.0, 200);
    let before = controller.metrics().meshes_resident;
    assert!(before > 100, "only {before} meshes were built");

    // A different anchor, one frame.
    let krtx = RadarProjection::new(45.7150, -122.9650);
    controller.poll();
    controller.frame_for_pane(
        &krtx,
        Generation::new(2),
        bucket(0.35),
        Camera2D::default(),
        viewport(),
        [0.05, 0.05, 0.06],
    );
    let after = controller.metrics().meshes_resident;
    println!("meshes resident: {before} at KTLX, {after} after moving to KRTX");
    assert!(
        after < before,
        "moving the anchor kept {after} meshes of the {before} built for the old one"
    );
}

/// The tile layer runs on the UI thread, so what it costs there is a frame
/// budget question, not a curiosity.
///
/// Nothing here may block on the network: `frame_for_pane` is hash lookups
/// plus at most `MAX_MESH_BUILDS_PER_FRAME` geodesic meshes, and the fetching
/// and decoding happen on the store's worker threads. The bounds below are
/// deliberately loose - this is a guard against a hang or an unbounded build
/// loop, not a benchmark - and the measured values are printed so a regression
/// is visible even when the assertion still passes.
#[test]
fn a_cold_pane_never_blocks_the_frame_it_is_asked_on() {
    let mut controller = controller();
    controller.set_provider(Some(TileProvider::UsgsImageryTopo));
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let camera = Camera2D::default();

    let started = std::time::Instant::now();
    controller.poll();
    controller.frame_for_pane(
        &projection,
        Generation::new(1),
        bucket(camera.km_per_point),
        camera,
        viewport(),
        [0.05, 0.05, 0.06],
    );
    let cold = started.elapsed();

    // Let the pane settle, then measure a steady frame.
    for _ in 0..8 {
        frame_at(&mut controller, camera);
    }
    let started = std::time::Instant::now();
    frame_at(&mut controller, camera);
    let warm = started.elapsed();

    println!("cold frame {cold:?}, warm frame {warm:?}");
    assert!(
        cold < std::time::Duration::from_millis(250),
        "the first frame of a cold pane took {cold:?} on the UI thread"
    );
    assert!(
        warm < std::time::Duration::from_millis(25),
        "a settled pane still costs {warm:?} on the UI thread"
    );
}
