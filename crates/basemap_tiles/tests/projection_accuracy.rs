//! The acceptance gate for [`basemap_tiles::build_tile_mesh`].
//!
//! This is the test that stops a future "optimisation" back to plain corner
//! projection from shipping silently. It builds real meshes for real tiles over
//! real radar sites, through the same geodesic azimuthal-equidistant projection
//! the application draws in, and asserts the achieved accuracy.
//!
//! # Why the projection is transcribed here
//!
//! `basemap_tiles` deliberately knows nothing about radars: `build_tile_mesh`
//! takes a closure. The real `RadarProjection` lives in `map_scene`, which
//! depends on *this* crate, so a dev-dependency on it would be a cycle. The
//! Vincenty inverse is therefore transcribed below — and, so that the test is
//! not merely checking our arithmetic against our arithmetic, it is validated
//! against two external references first:
//!
//! * Vincenty's own published test line (Flinders Peak to Buninyong,
//!   54 972.271 m, initial azimuth 306 deg 52' 05.37"), as redistributed by
//!   Geoscience Australia.
//! * The WGS84 meridian arc from 35.3333N to 36.3333N, 110.9559 km.
//!
//! Both are the same references `crates/map_scene/src/projection.rs` asserts
//! against, so passing them means this file agrees with the projection the
//! application actually uses.
//!
//! References: Vincenty, T. (1975), "Direct and inverse solutions of geodesics
//! on the ellipsoid with application of nested equations", *Survey Review*
//! 23(176), 88-93. Snyder, J.P. (1987), *Map Projections — A Working Manual*,
//! USGS Professional Paper 1395, pp. 191-202 (azimuthal equidistant).

use basemap_tiles::{MAX_TILE_ZOOM, MIN_TILE_ZOOM, TileId, build_tile_mesh};

/// Real NEXRAD sites, chosen to span the latitude range the application has to
/// work over: mid-latitude plains, Pacific North-West, high desert, the Alaska
/// Peninsula, and the tropics.
const SITES: &[(&str, f64, f64)] = &[
    ("KTLX Oklahoma City", 35.3333, -97.2778),
    ("KRTX Portland", 45.7150, -122.9650),
    ("KABX Albuquerque", 35.1497, -106.8239),
    ("PAKC King Salmon", 58.6794, -156.6294),
    ("TJUA San Juan", 18.1156, -66.0781),
];

/// The bound the mesh must meet, in the tile's own texels.
///
/// `SUBDIVISION_TOLERANCE_TEXELS` is 0.25 and the cheap midpoint probe
/// under-reports the dense-grid worst case slightly, so 0.30 is the honest
/// acceptance figure. Against the worst-case ~1.77x magnification inside one
/// LOD bucket that is about half a screen pixel.
const MAX_ERROR_TEXELS: f64 = 0.30;

const WGS84_A_M: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const WGS84_B_M: f64 = WGS84_A_M * (1.0 - WGS84_F);
const MAX_ITERATIONS: usize = 32;
const CONVERGENCE: f64 = 1e-12;

/// Geodesic azimuthal-equidistant projection anchored at a radar site, in
/// kilometres east and north.
#[derive(Clone, Copy)]
struct Aeqd {
    anchor_lon_deg: f64,
    sin_u1: f64,
    cos_u1: f64,
}

impl Aeqd {
    fn new(lat_deg: f64, lon_deg: f64) -> Self {
        let u1 = ((1.0 - WGS84_F) * lat_deg.to_radians().tan()).atan();
        Self {
            anchor_lon_deg: normalize_longitude(lon_deg),
            sin_u1: u1.sin(),
            cos_u1: u1.cos(),
        }
    }

    /// `(east_km, north_km)`, or `None` where the geodesic does not converge.
    fn project(&self, lon_deg: f64, lat_deg: f64) -> Option<(f64, f64)> {
        let (distance_m, azimuth_rad) = self.inverse(lon_deg, lat_deg)?;
        let distance_km = distance_m / 1_000.0;
        Some((
            distance_km * azimuth_rad.sin(),
            distance_km * azimuth_rad.cos(),
        ))
    }

    fn inverse(&self, lon_deg: f64, lat_deg: f64) -> Option<(f64, f64)> {
        if !lon_deg.is_finite() || !lat_deg.is_finite() {
            return None;
        }
        let lat2 = lat_deg.clamp(-90.0, 90.0).to_radians();
        let l = normalize_longitude(lon_deg - self.anchor_lon_deg).to_radians();
        let u2 = ((1.0 - WGS84_F) * lat2.tan()).atan();
        let (sin_u2, cos_u2) = u2.sin_cos();
        let (sin_u1, cos_u1) = (self.sin_u1, self.cos_u1);

        let mut lambda = l;
        let (mut sin_sigma, mut cos_sigma, mut sigma) = (0.0, 0.0, 0.0);
        let (mut cos_sq_alpha, mut cos_2sigma_m) = (0.0, 0.0);
        let (mut sin_lambda, mut cos_lambda) = (0.0, 0.0);
        let mut converged = false;

        for _ in 0..MAX_ITERATIONS {
            let sin_cos = lambda.sin_cos();
            sin_lambda = sin_cos.0;
            cos_lambda = sin_cos.1;
            let term_a = cos_u2 * sin_lambda;
            let term_b = cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda;
            sin_sigma = term_a.hypot(term_b);
            if sin_sigma == 0.0 {
                return Some((0.0, 0.0));
            }
            cos_sigma = sin_u1 * sin_u2 + cos_u1 * cos_u2 * cos_lambda;
            sigma = sin_sigma.atan2(cos_sigma);
            let sin_alpha = cos_u1 * cos_u2 * sin_lambda / sin_sigma;
            cos_sq_alpha = 1.0 - sin_alpha * sin_alpha;
            cos_2sigma_m = if cos_sq_alpha.abs() < f64::EPSILON {
                0.0
            } else {
                cos_sigma - 2.0 * sin_u1 * sin_u2 / cos_sq_alpha
            };
            let c = WGS84_F / 16.0 * cos_sq_alpha * (4.0 + WGS84_F * (4.0 - 3.0 * cos_sq_alpha));
            let previous = lambda;
            lambda = l
                + (1.0 - c)
                    * WGS84_F
                    * sin_alpha
                    * (sigma
                        + c * sin_sigma
                            * (cos_2sigma_m
                                + c * cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)));
            if (lambda - previous).abs() < CONVERGENCE {
                converged = true;
                break;
            }
        }
        if !converged {
            return None;
        }

        let u_sq = cos_sq_alpha * (WGS84_A_M * WGS84_A_M - WGS84_B_M * WGS84_B_M)
            / (WGS84_B_M * WGS84_B_M);
        let a_series =
            1.0 + u_sq / 16384.0 * (4096.0 + u_sq * (-768.0 + u_sq * (320.0 - 175.0 * u_sq)));
        let b_series = u_sq / 1024.0 * (256.0 + u_sq * (-128.0 + u_sq * (74.0 - 47.0 * u_sq)));
        let delta_sigma = b_series
            * sin_sigma
            * (cos_2sigma_m
                + b_series / 4.0
                    * (cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)
                        - b_series / 6.0
                            * cos_2sigma_m
                            * (-3.0 + 4.0 * sin_sigma * sin_sigma)
                            * (-3.0 + 4.0 * cos_2sigma_m * cos_2sigma_m)));
        let distance_m = WGS84_B_M * a_series * (sigma - delta_sigma);
        let azimuth = (cos_u2 * sin_lambda).atan2(cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda);
        Some((distance_m, azimuth))
    }
}

fn normalize_longitude(lon_deg: f64) -> f64 {
    if !lon_deg.is_finite() {
        return 0.0;
    }
    let mut lon = (lon_deg + 180.0).rem_euclid(360.0) - 180.0;
    if lon <= -180.0 {
        lon += 360.0;
    }
    lon
}

fn dms(degrees: f64, minutes: f64, seconds: f64) -> f64 {
    degrees + minutes / 60.0 + seconds / 3_600.0
}

/// The test's own ground truth, checked against outside authorities before it
/// is used to judge anything else.
#[test]
fn the_reference_projection_matches_published_geodesy() {
    // Flinders Peak 37 57 03.72030 S, 144 25 29.52440 E to
    // Buninyong 37 39 10.15610 S, 143 55 35.38390 E.
    let projection = Aeqd::new(-dms(37.0, 57.0, 3.720_30), dms(144.0, 25.0, 29.524_40));
    let (east_km, north_km) = projection
        .project(dms(143.0, 55.0, 35.383_90), -dms(37.0, 39.0, 10.156_10))
        .expect("converges");
    let distance_m = east_km.hypot(north_km) * 1_000.0;
    assert!(
        (distance_m - 54_972.271).abs() < 0.001,
        "Vincenty test line measured {distance_m} m, published 54972.271 m"
    );
    let azimuth_deg = east_km.atan2(north_km).to_degrees().rem_euclid(360.0);
    let expected = dms(306.0, 52.0, 5.37);
    assert!(
        (azimuth_deg - expected).abs() < 1e-6,
        "azimuth {azimuth_deg} deg, published {expected} deg"
    );

    // The WGS84 meridian arc from KTLX, 110.9559 km, due north.
    let ktlx = Aeqd::new(35.3333, -97.2778);
    let (east_km, north_km) = ktlx.project(-97.2778, 36.3333).expect("converges");
    assert!(
        (north_km - 110.9559).abs() < 0.001,
        "meridian arc measured {north_km} km, expected 110.9559 km"
    );
    assert!(east_km.abs() < 1e-6, "due north had easting {east_km} km");

    // The anchor is the origin, at every site.
    for (name, lat, lon) in SITES {
        let projection = Aeqd::new(*lat, *lon);
        let origin = projection.project(*lon, *lat).expect("converges");
        assert!(
            origin.0.abs() < 1e-9 && origin.1.abs() < 1e-9,
            "{name}: anchor projected to {origin:?}"
        );
    }
}

/// THE ACCEPTANCE GATE.
///
/// Every tile a pane can reasonably see, at every zoom the layer draws, over
/// five real radar sites, must come out within [`MAX_ERROR_TEXELS`] of the true
/// projection.
#[test]
fn every_mesh_is_sub_texel_at_every_drawn_zoom_and_site() {
    let mut worst_overall: (f64, String) = (0.0, String::new());
    let mut meshes = 0_u32;

    for (name, lat, lon) in SITES {
        let projection = Aeqd::new(*lat, *lon);
        let project = |lon_deg: f64, lat_deg: f64| projection.project(lon_deg, lat_deg);

        for z in MIN_TILE_ZOOM..=MAX_TILE_ZOOM {
            let center = TileId::containing(*lon, *lat, z).expect("a radar site is on the map");
            // At coarse zooms a pane is thousands of kilometres wide, so a
            // narrower spread of tiles is the realistic one; at fine zooms the
            // pane holds many more tiles than this in each direction.
            let offsets: &[i64] = if z <= 6 {
                &[-2, -1, 0, 1, 2]
            } else {
                &[-4, -2, 0, 2, 4]
            };
            let span = i64::from(center.span());

            let mut built_here = 0_u32;
            for dy in offsets {
                for dx in offsets {
                    let x = i64::from(center.x) + dx;
                    let y = i64::from(center.y) + dy;
                    if y < 0 || y >= span {
                        continue;
                    }
                    let x = x.rem_euclid(span) as u32;
                    let Some(tile) = TileId::new(z, x, y as u32) else {
                        continue;
                    };
                    // A tile the projection cannot express is dropped by
                    // design, and dropping it is the correct behaviour.
                    let Some(mesh) = build_tile_mesh(tile, project) else {
                        continue;
                    };
                    built_here += 1;
                    meshes += 1;

                    let error_texels = mesh.max_error_texels();
                    assert!(
                        error_texels <= MAX_ERROR_TEXELS,
                        "{name} z{z} tile {}/{}: {error_texels:.4} texels \
                         ({:.6} km) at subdivision {}",
                        tile.x,
                        tile.y,
                        mesh.max_error_km,
                        mesh.subdivision
                    );
                    assert!(mesh.max_error_km.is_finite());
                    assert!(!mesh.vertices.is_empty() && !mesh.indices.is_empty());
                    if error_texels > worst_overall.0 {
                        worst_overall = (
                            error_texels,
                            format!("{name} z{z} {}/{} N={}", tile.x, tile.y, mesh.subdivision),
                        );
                    }
                }
            }
            assert!(
                built_here > 0,
                "{name} z{z}: not one tile built, so nothing was actually tested"
            );
        }
    }

    assert!(meshes > 250, "only {meshes} meshes were exercised");
    println!(
        "built {meshes} meshes; worst {:.4} texels at {}",
        worst_overall.0, worst_overall.1
    );
}

/// THE ACCEPTANCE GATE, FAR FROM THE RADAR.
///
/// The gate above samples tiles a few steps either side of the site. That is
/// the realistic pane, but it is not the whole domain `build_tile_mesh` will
/// accept: the camera keeps the projection anchored at the radar while the
/// user pans, and `MAX_TILE_WORLD_KM` lets a tile 9000 km away be drawn. The
/// subdivision saturates at `MAX_SUBDIVISION`, and when it does the mesh is
/// emitted anyway with whatever error it reached — there is no flag on
/// `TileMesh` that says "I did not converge". So the far field has to be
/// measured, not assumed.
///
/// MEASURED over five sites and every drawn zoom, sampling by distance rather
/// than by tile index:
///
/// * within 6000 km of the anchor, every mesh meets the 0.30-texel bound;
/// * beyond it, z5 saturates at N=8 and the worst case rises to 0.3452 texels
///   (0.74 km) — sub-pixel on screen at the zoom that produced it, but over
///   the tolerance the crate documents.
///
/// Both numbers are asserted. If someone lowers `MAX_SUBDIVISION`, widens
/// `SUBDIVISION_TOLERANCE_TEXELS`, or lets the layer draw a coarser zoom, this
/// is where it shows up.
#[test]
fn the_far_field_is_measured_rather_than_assumed() {
    /// Where the bound genuinely holds. Any pane wider than this at a radar's
    /// own anchor is no longer a picture of anything.
    const NEAR_FIELD_KM: f64 = 6_000.0;
    /// What saturation actually costs past that, measured.
    const FAR_FIELD_LIMIT_TEXELS: f64 = 0.40;

    let mut worst_near: (f64, String) = (0.0, String::new());
    let mut worst_far: (f64, String) = (0.0, String::new());
    let mut built = 0_u32;

    for (name, lat, lon) in SITES {
        let projection = Aeqd::new(*lat, *lon);
        let project = |lon_deg: f64, lat_deg: f64| projection.project(lon_deg, lat_deg);

        for z in MIN_TILE_ZOOM..=MAX_TILE_ZOOM {
            let center = TileId::containing(*lon, *lat, z).expect("a radar site is on the map");
            let span = i64::from(center.span());
            // Tile width in kilometres at this latitude, used to turn a
            // distance in kilometres into an offset in tiles.
            let tile_km = 40_075.0 * lat.to_radians().cos() / span as f64;
            let mut offsets = vec![0_i64];
            for kilometres in [250.0, 1_000.0, 3_000.0, 6_000.0, 8_500.0] {
                let offset = (kilometres / tile_km).round() as i64;
                if offset > 0 && offset < span / 2 {
                    offsets.push(offset);
                }
            }

            for dy in &offsets {
                for dx in &offsets {
                    for (sx, sy) in [(1_i64, 1_i64), (-1, 1), (1, -1), (-1, -1)] {
                        let y = i64::from(center.y) + dy * sy;
                        if y < 0 || y >= span {
                            continue;
                        }
                        let x = (i64::from(center.x) + dx * sx).rem_euclid(span) as u32;
                        let Some(tile) = TileId::new(z, x, y as u32) else {
                            continue;
                        };
                        // A tile the projection refuses is not drawn at all,
                        // which is the correct answer and not a failure.
                        let Some(mesh) = build_tile_mesh(tile, project) else {
                            continue;
                        };
                        built += 1;

                        let (center_lon, center_lat) = tile.center_lon_lat();
                        let distance_km = projection
                            .project(center_lon, center_lat)
                            .map_or(f64::INFINITY, |(east, north)| east.hypot(north));
                        let error_texels = mesh.max_error_texels();
                        let record = format!(
                            "{name} z{z} {}/{} N={} {error_texels:.4} texels \
                             ({:.3} km) at {distance_km:.0} km",
                            tile.x, tile.y, mesh.subdivision, mesh.max_error_km
                        );
                        if distance_km <= NEAR_FIELD_KM {
                            assert!(
                                error_texels <= MAX_ERROR_TEXELS,
                                "inside {NEAR_FIELD_KM} km the bound must hold: {record}"
                            );
                            if error_texels > worst_near.0 {
                                worst_near = (error_texels, record);
                            }
                        } else {
                            assert!(
                                error_texels <= FAR_FIELD_LIMIT_TEXELS,
                                "the far field got worse than it was measured at: {record}"
                            );
                            if error_texels > worst_far.0 {
                                worst_far = (error_texels, record);
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(built > 1_000, "only {built} meshes were exercised");
    assert!(
        worst_far.0 > MAX_ERROR_TEXELS,
        "the far field no longer saturates, so this test is now measuring \
         nothing; re-derive the bounds rather than deleting it"
    );
    println!("built {built} meshes");
    println!("worst inside {NEAR_FIELD_KM} km: {}", worst_near.1);
    println!("worst beyond it: {}", worst_far.1);
}

/// The subdivision must be doing real work.
///
/// If corner projection alone were good enough, the adaptive machinery would
/// be dead weight and someone would eventually delete it. This measures what
/// plain corner projection costs at the coarse zooms and asserts it is over
/// the tolerance — i.e. that the feature earns its keep.
#[test]
fn corner_projection_alone_would_miss_the_tolerance_at_coarse_zooms() {
    for (name, lat, lon) in SITES {
        let projection = Aeqd::new(*lat, *lon);
        for z in [MIN_TILE_ZOOM, MIN_TILE_ZOOM + 1] {
            let center = TileId::containing(*lon, *lat, z).expect("on the map");
            // A tile two along, i.e. a tile a real pane at this zoom contains.
            let tile = TileId::new(z, center.x + 2, center.y).expect("valid");

            let north_west = projection.project_at(tile, 0.0, 0.0);
            let south_east = projection.project_at(tile, 1.0, 1.0);
            let truth = projection.project_at(tile, 0.5, 0.5);
            // The centre of the tile lies on the NW-SE diagonal, where a
            // two-triangle quad interpolates the midpoint of those corners.
            let estimate = (
                (north_west.0 + south_east.0) * 0.5,
                (north_west.1 + south_east.1) * 0.5,
            );
            let error_km = (truth.0 - estimate.0).hypot(truth.1 - estimate.1);
            let error_texels = error_km * 1_000.0 / tile.ground_resolution_m_per_texel();

            println!("{name} z{z}: corner-only error {error_texels:.2} texels ({error_km:.3} km)");
            assert!(
                error_texels > MAX_ERROR_TEXELS,
                "{name} z{z}: corner projection was already within tolerance \
                 ({error_texels:.4} texels), so the subdivision is unjustified here"
            );

            // And the adaptive mesh fixes it.
            let mesh = build_tile_mesh(tile, |lon_deg, lat_deg| {
                projection.project(lon_deg, lat_deg)
            })
            .expect("builds");
            assert!(mesh.subdivision > 1);
            assert!(mesh.max_error_texels() <= MAX_ERROR_TEXELS);
        }
    }
}

/// Cost, measured rather than asserted: the number of geodesic evaluations a
/// mesh needs, and the fact that it collapses to a single quad at analysis
/// zooms where the tile is small enough to be flat.
#[test]
fn fine_zooms_need_no_subdivision_at_all() {
    for (name, lat, lon) in SITES {
        let projection = Aeqd::new(*lat, *lon);
        for z in 11..=MAX_TILE_ZOOM {
            let tile = TileId::containing(*lon, *lat, z).expect("on the map");
            let mesh = build_tile_mesh(tile, |lon_deg, lat_deg| {
                projection.project(lon_deg, lat_deg)
            })
            .expect("builds");
            assert_eq!(
                mesh.subdivision, 1,
                "{name} z{z} subdivided to {} where a quad should do",
                mesh.subdivision
            );
            assert_eq!(mesh.vertices.len(), 4);
            assert_eq!(mesh.indices.len(), 6);
        }
    }
}

/// A mesh must depend on the tile and the projection and on nothing else. This
/// is what makes `(TileId, projection identity)` a sound cache key, which is in
/// turn what makes a pan free.
#[test]
fn a_mesh_is_a_pure_function_of_the_tile_and_the_projection() {
    let projection = Aeqd::new(35.3333, -97.2778);
    let project = |lon_deg: f64, lat_deg: f64| projection.project(lon_deg, lat_deg);
    for z in MIN_TILE_ZOOM..=12 {
        let tile = TileId::containing(-97.2778, 35.3333, z).expect("on the map");
        let first = build_tile_mesh(tile, project).expect("builds");
        let second = build_tile_mesh(tile, project).expect("builds");
        assert_eq!(first.subdivision, second.subdivision);
        assert_eq!(first.vertices, second.vertices);
        assert_eq!(first.indices, second.indices);
        assert_eq!(
            first.max_error_km.to_bits(),
            second.max_error_km.to_bits(),
            "z{z}: the recorded error was not reproducible"
        );
    }
}

impl Aeqd {
    fn project_at(&self, tile: TileId, u: f64, v: f64) -> (f64, f64) {
        let (lon_deg, lat_deg) = tile.lon_lat_at(u, v);
        self.project(lon_deg, lat_deg).expect("converges")
    }
}
