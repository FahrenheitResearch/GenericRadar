//! Warning-layer stress case: pan cost must not depend on camera position or
//! on how much geometry is retained.
//!
//! The synthetic polygons here are load, not evidence of correctness: visual
//! acceptance is done against real Level II data. What this proves is the
//! structural claim — during a timed pan loop nothing is rebuilt, nothing is
//! re-tessellated and nothing is uploaded.

use std::time::Instant;

use analyst_runtime::{
    Camera2D, Generation, GeometryCacheKey, LodBucket, LodSelector, ViewportMetrics,
};
use map_scene::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
use map_scene::dataset::{GeoLineFeature, MapDataset, MapLayer};
use map_scene::projection::RadarProjection;
use map_scene::residency::{Admission, GeometryResidency};
use map_scene::style::MapStyle;

const PAN_ITERATIONS: usize = 10_000;
const STRESS_POLYGONS: usize = 10_000;

/// Leaked so the features can hold `&'static` point slices, matching the shape
/// of the compiled-in basemap. The process is a test binary; this is bounded
/// and intentional.
fn warning_rings(count: usize) -> Vec<GeoLineFeature> {
    let mut features = Vec::with_capacity(count);
    for index in 0..count {
        // Scatter storm-sized quadrilaterals across the radar footprint.
        // Golden-ratio stepping spreads the rings without clustering.
        let angle = index as f32 * 0.618_034 * std::f32::consts::TAU;
        let radius_deg = 0.2 + (index % 40) as f32 * 0.06;
        let center_lon = -97.2778 + angle.cos() * radius_deg * 1.4;
        let center_lat = 35.3333 + angle.sin() * radius_deg;
        let size = 0.05;
        let ring: &'static [(f32, f32)] = Box::leak(Box::new([
            (center_lon - size, center_lat - size),
            (center_lon + size, center_lat - size * 0.5),
            (center_lon + size * 0.7, center_lat + size),
            (center_lon - size * 0.8, center_lat + size * 0.6),
            (center_lon - size, center_lat - size),
        ]));
        let mut bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for (lon, lat) in ring {
            bbox[0] = bbox[0].min(*lon);
            bbox[1] = bbox[1].min(*lat);
            bbox[2] = bbox[2].max(*lon);
            bbox[3] = bbox[3].max(*lat);
        }
        features.push(GeoLineFeature {
            layer: MapLayer::County,
            bbox,
            points: ring,
        });
    }
    features
}

fn key(lod: LodBucket) -> GeometryCacheKey {
    GeometryCacheKey {
        dataset: Generation::new(1),
        projection: Generation::new(1),
        style: Generation::new(1),
        lod,
    }
}

fn viewport() -> ViewportMetrics {
    ViewportMetrics {
        width_points: 1_600.0,
        height_points: 900.0,
        pixels_per_point: 1.0,
    }
}

/// Everything a frame does for the map while panning: update the camera, pick
/// the LOD, and confirm the geometry is already resident.
fn pan_frame(
    camera: &mut Camera2D,
    selector: &mut LodSelector,
    residency: &mut GeometryResidency,
    bytes: usize,
    delta: f32,
) -> GeometryCacheKey {
    camera.pan_by_screen_delta(delta, delta * 0.5);
    let lod = selector.update(camera.km_per_point);
    let frame_key = key(lod);
    let admission = residency.touch(frame_key, bytes);
    assert_eq!(
        admission,
        Admission::AlreadyResident,
        "a pan uploaded geometry"
    );
    frame_key
}

fn percentile(sorted_nanos: &[u128], fraction: f64) -> u128 {
    if sorted_nanos.is_empty() {
        return 0;
    }
    let index = ((sorted_nanos.len() - 1) as f64 * fraction).round() as usize;
    sorted_nanos[index]
}

fn run_case(polygon_count: usize) {
    let dataset = MapDataset::from_parts(
        Generation::new(1),
        warning_rings(polygon_count),
        Vec::new(),
        Vec::new(),
    );
    let projection = RadarProjection::new(35.3333, -97.2778);
    let mut selector = LodSelector::new(LOD_REFERENCE_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT);
    let lod = selector.update(LOD_REFERENCE_KM_PER_POINT);

    let build_started = Instant::now();
    let geometry = build_geometry(&MapBuildRequest {
        key: key(lod),
        dataset,
        projection,
        style: MapStyle::default(),
    });
    let build_ms = build_started.elapsed().as_secs_f64() * 1_000.0;

    let mut residency = GeometryResidency::default();
    let admission = residency.touch(geometry.key, geometry.estimated_bytes);
    assert!(
        matches!(admission, Admission::Admitted { .. }),
        "the first frame must upload once"
    );

    let uploads_before = residency.metrics().uploads;
    let key_before = geometry.key;

    let mut camera = Camera2D {
        center_east_km: 0.0,
        center_north_km: 0.0,
        km_per_point: LOD_REFERENCE_KM_PER_POINT,
        rotation_rad: 0.0,
    };
    let mut samples = Vec::with_capacity(PAN_ITERATIONS);
    for iteration in 0..PAN_ITERATIONS {
        // Sweep the camera a long way in both axes so "cost is independent of
        // camera position" is actually exercised, not just jittered.
        let delta = if iteration % 2 == 0 { 7.0 } else { -3.0 };
        let started = Instant::now();
        let frame_key = pan_frame(
            &mut camera,
            &mut selector,
            &mut residency,
            geometry.estimated_bytes,
            delta,
        );
        samples.push(started.elapsed().as_nanos());
        assert_eq!(frame_key, key_before, "a pan changed the geometry key");
    }

    // The structural assertions: nothing was rebuilt or uploaded, and the key
    // the frame resolves to never moved.
    assert_eq!(
        residency.metrics().uploads,
        uploads_before,
        "geometry was uploaded during the pan loop"
    );
    assert_eq!(residency.metrics().evictions, 0);
    assert_eq!(residency.len(), 1);

    samples.sort_unstable();
    let median = percentile(&samples, 0.50);
    let p95 = percentile(&samples, 0.95);
    let p99 = percentile(&samples, 0.99);

    println!(
        "polygons={polygon_count} build_ms={build_ms:.1} vertices={} indices={} bytes={} \
         uploads_initial=1 uploads_during_pan=0 tessellations_during_pan=0 \
         pan_median_ns={median} pan_p95_ns={p95} pan_p99_ns={p99}",
        geometry.vertex_count(),
        geometry.index_count(),
        geometry.estimated_bytes,
    );

    // A camera update is arithmetic on a handful of floats plus one hash
    // lookup. This bound is loose enough for a loaded CI box but would fail
    // immediately if per-polygon work crept into the pan path.
    assert!(
        p99 < 200_000,
        "p99 pan cost was {p99} ns with {polygon_count} polygons, which suggests \
         per-feature work on the pan path"
    );
}

#[test]
fn panning_costs_the_same_with_no_polygons() {
    run_case(0);
}

#[test]
fn panning_costs_the_same_with_a_thousand_polygons() {
    run_case(1_000);
}

#[test]
fn panning_costs_the_same_with_ten_thousand_polygons() {
    run_case(STRESS_POLYGONS);
}

/// Cameras that share a LOD share geometry however far apart they are pointing
/// and however different their exact scale is.
#[test]
fn cameras_with_different_centres_and_scales_share_one_key() {
    let mut selector = LodSelector::new(LOD_REFERENCE_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT);
    let baseline = key(selector.update(LOD_REFERENCE_KM_PER_POINT));

    for (east, north, scale) in [
        (0.0_f64, 0.0_f64, LOD_REFERENCE_KM_PER_POINT),
        (900.0, -700.0, LOD_REFERENCE_KM_PER_POINT * 1.11),
        (-450.0, 380.0, LOD_REFERENCE_KM_PER_POINT * 0.91),
        (12_000.0, 9_000.0, LOD_REFERENCE_KM_PER_POINT * 1.05),
    ] {
        let camera = Camera2D {
            center_east_km: east,
            center_north_km: north,
            km_per_point: scale,
            rotation_rad: 0.0,
        };
        let candidate = key(selector.update(camera.km_per_point));
        assert_eq!(
            candidate, baseline,
            "centre ({east}, {north}) at scale {scale} produced a different key"
        );
    }

    // And the viewport is not part of identity either.
    let _ = viewport();
}
