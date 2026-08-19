//! Independent re-measurement of the tile mesh's projection error, through the
//! REAL `RadarProjection` the application draws with, at cameras far from the
//! anchor.
//!
//! `basemap_tiles/tests/projection_accuracy.rs` is the core crate's own gate,
//! but two things are outside it and are measured here instead:
//!
//! 1. It necessarily *transcribes* the projection - depending on `map_scene`
//!    would be a dependency cycle - so it proves its own arithmetic, not the
//!    closure `crate::tiles::TileSceneController::mesh_for` actually passes to
//!    `build_tile_mesh`.
//! 2. It only probes tiles within four tiles of the radar. Nothing clamps the
//!    camera centre: `Camera2D::pan_by_screen_delta` writes any finite value,
//!    so a pane can legitimately sit thousands of kilometres from the radar,
//!    where the azimuthal-equidistant transverse scale factor
//!    `rho / (R sin(rho/R))` is far from 1 and a tile is at its least
//!    rectangular. `build_tile_mesh` gives up at
//!    `basemap_tiles::MAX_SUBDIVISION` and records whatever error remains, so
//!    the tolerance is a target rather than a guarantee, and the far field is
//!    exactly where it stops being met.
//!
//! MEASURED RESULT, and the reason this file exists: the core crate's 0.30
//! texel acceptance figure IS exceeded once the camera is panned - z5 at
//! 8000 km reaches 0.32 texels with the subdivision saturated at 8x8. What
//! matters is the consequence on screen, and that stays under one pixel
//! because a coarse tile is only ever magnified so far inside the LOD bucket
//! that selected it. That is asserted below in screen pixels rather than
//! argued in a comment.
//!
//! References: Snyder, J.P. (1987), *Map Projections - A Working Manual*, USGS
//! Professional Paper 1395, pp. 191-202 (azimuthal equidistant); Vincenty, T.
//! (1975), "Direct and inverse solutions of geodesics on the ellipsoid with
//! application of nested equations", *Survey Review* 23(176), 88-93.

use std::collections::BTreeMap;

use analyst_runtime::WorldPoint;
use basemap_tiles::{MAX_SUBDIVISION, MAX_TILE_ZOOM, MIN_TILE_ZOOM, TileId, build_tile_mesh};
use map_scene::projection::RadarProjection;

const SITES: &[(&str, f64, f64)] = &[
    ("KTLX Oklahoma City", 35.3333625793457, -97.27776336669922),
    ("KRTX Portland", 45.7150, -122.9650),
    ("PAKC King Salmon", 58.6794, -156.6294),
    ("TJUA San Juan", 18.1156, -66.0781),
];

/// Distances from the anchor a panned pane can sit at. `build_tile_mesh`
/// refuses anything past `MAX_TILE_WORLD_KM` (9000 km), so 8900 is the last
/// band that draws at all.
const DISTANCES_KM: &[f64] = &[0.0, 250.0, 1_000.0, 3_000.0, 6_000.0, 8_000.0, 8_900.0];

const BEARINGS_DEG: &[f64] = &[0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0];

/// Largest magnification a tile can suffer while still inside the LOD bucket
/// that selected its zoom.
///
/// `tile_zoom_for` rounds `log2(texel / m_per_px)` at the bucket's CENTRE
/// scale, so the texel is at most `sqrt(2)` larger than the centre pixel, and
/// `LodSelector` holds a bucket down to `(1/sqrt(2)) * (1 - 0.12) = 0.622` of
/// its centre scale before stepping. One texel therefore covers at most
/// `1.4142 / 0.622 = 2.27` screen pixels, and an error of E texels is at most
/// `2.27 E` pixels on screen.
const MAX_MAGNIFICATION: f64 = std::f64::consts::SQRT_2 / (std::f64::consts::FRAC_1_SQRT_2 * 0.88);

/// The bound that matters: the mesh must never land the imagery more than one
/// screen pixel from where the true projection puts it, at any camera the
/// application permits.
const MAX_ERROR_SCREEN_PX: f64 = 1.0;

/// And the texel figure, pinned at the measured worst with headroom, so a
/// regression that doubled the error would fail here even though it would
/// still be sub-pixel.
const MAX_ERROR_TEXELS: f64 = 0.40;

fn offset_lon_lat(projection: &RadarProjection, distance_km: f64, bearing_deg: f64) -> (f64, f64) {
    let bearing = bearing_deg.to_radians();
    projection.world_to_lon_lat(WorldPoint {
        east_km: distance_km * bearing.sin(),
        north_km: distance_km * bearing.cos(),
    })
}

/// THE RE-MEASUREMENT.
#[test]
fn the_mesh_error_stays_sub_pixel_however_far_the_camera_is_panned() {
    let mut per_zoom: BTreeMap<u8, (f64, String)> = BTreeMap::new();
    let mut worst = (0.0_f64, String::new());
    let mut measured = 0_u32;
    let mut dropped = 0_u32;
    let mut saturated = 0_u32;

    for (name, lat, lon) in SITES {
        let projection = RadarProjection::new(*lat, *lon);
        let project = |lon_deg: f64, lat_deg: f64| {
            projection
                .try_lon_lat_to_world(lon_deg, lat_deg)
                .map(|world| (world.east_km, world.north_km))
        };

        for z in MIN_TILE_ZOOM..=MAX_TILE_ZOOM {
            for distance_km in DISTANCES_KM {
                for bearing_deg in BEARINGS_DEG {
                    let (far_lon, far_lat) =
                        offset_lon_lat(&projection, *distance_km, *bearing_deg);
                    let Some(tile) = TileId::containing(far_lon, far_lat, z) else {
                        continue; // Past the Web Mercator latitude limit.
                    };
                    let Some(mesh) = build_tile_mesh(tile, project) else {
                        dropped += 1;
                        continue; // Beyond the world radius: deliberately not drawn.
                    };
                    measured += 1;
                    if mesh.subdivision >= MAX_SUBDIVISION {
                        saturated += 1;
                    }
                    let texels = mesh.max_error_texels();
                    assert!(texels.is_finite(), "{name} z{z}: non-finite error");
                    let label = format!(
                        "{name} z{z} {distance_km:.0} km bearing {bearing_deg:.0} tile {}/{} \
                         N={} ({:.6} km)",
                        tile.x, tile.y, mesh.subdivision, mesh.max_error_km
                    );
                    let entry = per_zoom.entry(z).or_insert((0.0, String::new()));
                    if texels > entry.0 {
                        *entry = (texels, label.clone());
                    }
                    if texels > worst.0 {
                        worst = (texels, label);
                    }
                }
            }
        }
    }

    for (z, (texels, where_it_was)) in &per_zoom {
        println!(
            "z{z:>2}: worst {texels:.4} texels = {:.3} screen px magnified  [{where_it_was}]",
            texels * MAX_MAGNIFICATION
        );
    }
    println!(
        "measured {measured} meshes, {saturated} with the subdivision saturated, {dropped} \
         dropped as out of range; worst {:.4} texels = {:.3} px at {}",
        worst.0,
        worst.0 * MAX_MAGNIFICATION,
        worst.1
    );

    assert!(measured > 1_000, "only {measured} meshes were exercised");
    assert!(
        worst.0 * MAX_MAGNIFICATION <= MAX_ERROR_SCREEN_PX,
        "the imagery lands {:.3} screen pixels from the truth at {}",
        worst.0 * MAX_MAGNIFICATION,
        worst.1
    );
    assert!(
        worst.0 <= MAX_ERROR_TEXELS,
        "worst {:.4} texels at {}",
        worst.0,
        worst.1
    );
}

/// Corner projection - the two-triangle quad most viewers draw - must actually
/// be worse than what ships, measured through the REAL projection. If it were
/// not, the adaptive subdivision would be dead weight and someone would
/// eventually delete it.
#[test]
fn corner_projection_alone_would_be_visibly_wrong_at_the_coarse_zooms() {
    let projection = RadarProjection::new(35.3333625793457, -97.27776336669922);
    let project = |lon_deg: f64, lat_deg: f64| {
        projection
            .try_lon_lat_to_world(lon_deg, lat_deg)
            .map(|world| (world.east_km, world.north_km))
    };
    for z in [MIN_TILE_ZOOM, MIN_TILE_ZOOM + 1, MIN_TILE_ZOOM + 2] {
        let center = TileId::containing(-97.27776336669922, 35.3333625793457, z).expect("on map");
        let tile = TileId::new(z, center.x + 2, center.y).expect("valid");
        let at = |u: f64, v: f64| {
            let (lon, lat) = tile.lon_lat_at(u, v);
            project(lon, lat).expect("converges")
        };

        let north_west = at(0.0, 0.0);
        let south_east = at(1.0, 1.0);
        let truth = at(0.5, 0.5);
        // The tile centre lies on the NW-SE diagonal, where a two-triangle
        // quad interpolates the midpoint of those two corners.
        let estimate = (
            (north_west.0 + south_east.0) * 0.5,
            (north_west.1 + south_east.1) * 0.5,
        );
        let corner_texels = (truth.0 - estimate.0).hypot(truth.1 - estimate.1) * 1_000.0
            / tile.ground_resolution_m_per_texel();
        let mesh = build_tile_mesh(tile, project).expect("builds");
        println!(
            "z{z}: corner-only {corner_texels:.2} texels ({:.2} px), subdivided {:.4} texels \
             ({:.3} px)",
            corner_texels * MAX_MAGNIFICATION,
            mesh.max_error_texels(),
            mesh.max_error_texels() * MAX_MAGNIFICATION
        );
        assert!(
            corner_texels * MAX_MAGNIFICATION > 3.5,
            "z{z}: corner projection was already within {corner_texels} texels, so the \
             subdivision is unjustified"
        );
        // The subdivided mesh has to be an order of magnitude better AND land
        // inside the tolerance near the radar, which is where the core crate's
        // 0.30 figure does hold.
        assert!(mesh.max_error_texels() * 10.0 < corner_texels);
        assert!(
            mesh.max_error_texels() <= 0.30,
            "z{z}: {:.4} texels near the anchor",
            mesh.max_error_texels()
        );
    }
}

/// The transcribed projection in the core crate's gate has to agree with the
/// one the application actually draws with, or that gate measures a different
/// map from the one that ships. Both references below are the ones
/// `map_scene::projection` and that gate each assert against.
#[test]
fn the_real_projection_agrees_with_the_published_geodesic_references() {
    // Vincenty's own published line: Flinders Peak to Buninyong, 54 972.271 m,
    // quoted to the millimetre, so a millimetre is the tolerance.
    let flinders = RadarProjection::new(-37.951_033_333, 144.424_868_055);
    let world = flinders
        .try_lon_lat_to_world(143.926_495_694, -37.652_821_111)
        .expect("converges");
    let distance_km = world.east_km.hypot(world.north_km);
    assert!(
        (distance_km - 54.972_271).abs() < 1e-5,
        "Vincenty test line measured {distance_km} km"
    );

    // The WGS84 meridian arc from 35.3333N to 36.3333N, 110.9559 km.
    let ktlx = RadarProjection::new(35.3333, -97.2778);
    let north = ktlx
        .try_lon_lat_to_world(-97.2778, 36.3333)
        .expect("converges");
    assert!(
        (north.north_km - 110.9559).abs() < 0.001,
        "meridian arc measured {} km",
        north.north_km
    );
    assert!(north.east_km.abs() < 1e-6);
}
