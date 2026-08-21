//! What the north-up camera rotation must not break, and what it must fix.
//!
//! The rule itself lives on `RadarProjection::view_rotation_rad`, with its
//! citations. This file is about the two ends of it:
//!
//! * INSIDE the analysis range nothing may move, and the three properties the
//!   analyst measures with - a range ring is a true circle, an azimuth from the
//!   radar is a straight ray, and a gate is drawn where its geodesic ground
//!   position is - must survive at every rotation, not merely at the ones the
//!   rule happens to produce. A rotation is an isometry, so they do; these
//!   tests are what make that a checked fact rather than an argument.
//!
//! * At regional and continental zoom the map must stop being drawn off axis,
//!   and by a stated number of degrees rather than by eye.
//!
//! References: Snyder, J.P. (1987). *Map Projections - A Working Manual.* USGS
//! Professional Paper 1395, section 25 pp. 191-202 (azimuthal equidistant) and
//! section 15 pp. 104-110 (Lambert conformal conic grid convergence).
//! doi:10.3133/pp1395

use analyst_runtime::{Camera2D, ScreenPoint, ViewportMetrics, WorldPoint};
use map_scene::projection::RadarProjection;
use map_scene::projection::globe;

/// KRTX, Portland, from the live catalogue: the anchor in the complaint.
const KRTX: (f64, f64) = (45.714_968_872_070_31, -122.965_301_513_671_88);
/// KDVN, Davenport, the site the real proof volume comes from.
const KDVN: (f64, f64) = (41.611_667, -90.580_833);
/// AWPA2, Anchorage, where the convergence inside a footprint is largest of
/// the sites the workspace's own tables exercise.
const AWPA2: (f64, f64) = (61.150_001_525_878_906, -149.779_998_779_296_88);

const PANE: ViewportMetrics = ViewportMetrics {
    width_points: 1600.0,
    height_points: 900.0,
    pixels_per_point: 1.0,
};

fn camera(centre: WorldPoint, km_per_point: f32, rotation_rad: f32) -> Camera2D {
    Camera2D {
        center_east_km: centre.east_km,
        center_north_km: centre.north_km,
        km_per_point,
        rotation_rad,
    }
}

/// Every rotation the rule can produce, plus a few it cannot, because these
/// properties are claims about the camera and not about the policy.
const ROTATIONS_RAD: &[f32] = &[
    0.0,
    0.018_3,   // about a degree, the ramp band at fine scale
    0.284_7,   // 16.3 deg, the continental case from a Portland anchor
    0.564_226, // 32.3 deg, the eastern seaboard from a Portland anchor
    -0.564_226,
    std::f32::consts::FRAC_PI_2, // a quarter turn
    3.0,
];

/// A range ring is drawn as a screen circle about the antenna. That is only
/// honest if the locus of constant range really is a screen circle, at every
/// rotation - which is exactly what makes it safe to leave `circle_stroke`
/// alone in the pane.
#[test]
fn a_range_ring_is_a_true_circle_at_every_rotation() {
    let projection = RadarProjection::new(KDVN.0, KDVN.1);
    let mut worst_points: f64 = 0.0;
    for rotation_rad in ROTATIONS_RAD {
        for km_per_point in [0.01_f32, 0.35, 1.0, 4.0] {
            // The pane centre is well away from the antenna as well as on it,
            // because a rotation is applied about the centre and the ring is
            // about the antenna.
            for centre in [WorldPoint::ORIGIN, WorldPoint::new(120.0, -80.0)] {
                let camera = camera(centre, km_per_point, *rotation_rad);
                let antenna = camera.world_to_screen(WorldPoint::ORIGIN, PANE);
                for range_km in [50.0_f64, 120.0, 230.0, 300.0, 460.0] {
                    let expected = range_km / f64::from(km_per_point);
                    for spoke in 0..360 {
                        let azimuth = f64::from(spoke).to_radians();
                        let world =
                            WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                        let screen = camera.world_to_screen(world, PANE);
                        let radius =
                            f64::from(screen.x - antenna.x).hypot(f64::from(screen.y - antenna.y));
                        worst_points = worst_points.max((radius - expected).abs());
                    }
                }
                // Also that the ring's centre is where the pane paints it.
                let _ = projection;
            }
        }
    }
    // A tenth of a screen point, at scales where one point is 10 m of ground.
    // The residual is f32 rounding in `world_to_screen`, not geometry.
    assert!(
        worst_points < 0.1,
        "a range ring went {worst_points} screen points out of round"
    );
    println!("worst ring out-of-round: {worst_points:.6} screen points");
}

/// Every azimuth from the radar is still a straight line on screen. A rotation
/// is linear, so a ray through the origin maps to a ray through the origin -
/// and the sign of the turn is the one `world_to_screen` documents.
#[test]
fn every_azimuth_from_the_radar_is_still_a_straight_ray() {
    let mut worst_points: f64 = 0.0;
    let mut worst_bearing_deg: f64 = 0.0;
    for rotation_rad in ROTATIONS_RAD {
        let camera = camera(WorldPoint::new(-40.0, 25.0), 0.35, *rotation_rad);
        let antenna = camera.world_to_screen(WorldPoint::ORIGIN, PANE);
        for spoke in 0..720 {
            let azimuth_deg = f64::from(spoke) * 0.5;
            let azimuth = azimuth_deg.to_radians();
            let mut previous: Option<(f64, f64)> = None;
            for range_km in [10.0_f64, 50.0, 120.0, 230.0, 340.0, 460.0] {
                let world = WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                let screen = camera.world_to_screen(world, PANE);
                let dx = f64::from(screen.x - antenna.x);
                let dy = f64::from(screen.y - antenna.y);
                // The screen bearing of this gate must be the compass azimuth
                // plus the camera rotation, at EVERY range along the ray.
                let bearing_deg = dx.atan2(-dy).to_degrees();
                let expected = azimuth_deg + f64::from(*rotation_rad).to_degrees();
                let error = ((bearing_deg - expected + 180.0).rem_euclid(360.0) - 180.0).abs();
                worst_bearing_deg = worst_bearing_deg.max(error);
                if let Some((px, py)) = previous {
                    // Collinear with the previous point and the antenna: the
                    // cross product of the two offsets, normalised, is the
                    // perpendicular distance from the line.
                    let cross = (px * dy - py * dx).abs();
                    let length = dx.hypot(dy).max(1e-9);
                    worst_points = worst_points.max(cross / length);
                }
                previous = Some((dx, dy));
            }
        }
    }
    assert!(
        worst_points < 0.01,
        "an azimuth bowed {worst_points} screen points away from a straight line"
    );
    assert!(
        worst_bearing_deg < 1e-3,
        "a gate's screen bearing was {worst_bearing_deg} deg off azimuth plus rotation"
    );
    println!(
        "worst ray bow {worst_points:.9} points, worst bearing error {worst_bearing_deg:.9} deg"
    );
}

/// A gate is drawn where its ground is drawn.
///
/// The raster places a gate at compass azimuth `A` and ground range `R` at the
/// screen offset `R / km_per_point` along screen bearing `A + rotation` from
/// the antenna's own screen position - that is `render2d`'s rule with the
/// camera rotation subtracted off the azimuth, and `render2d`'s own real-data
/// proof pins it pixel for pixel. The basemap draws the SAME piece of ground
/// by projecting its longitude and latitude and running it through
/// `world_to_screen`. This is the test that the two answers are the same
/// place, which is what "the echo is registered" means.
#[test]
fn a_gate_lands_where_its_geodesic_ground_position_is_drawn() {
    let mut worst_points: f64 = 0.0;
    let mut worst_metres: f64 = 0.0;
    let mut worst_site = "";
    for (name, latitude, longitude) in [("KDVN", KDVN.0, KDVN.1), ("AWPA2", AWPA2.0, AWPA2.1)] {
        let projection = RadarProjection::new(latitude, longitude);
        for rotation_rad in ROTATIONS_RAD {
            for km_per_point in [0.01_f32, 0.35, 1.0, 4.0] {
                let camera = camera(WorldPoint::new(75.0, -140.0), km_per_point, *rotation_rad);
                let antenna = camera.world_to_screen(WorldPoint::ORIGIN, PANE);
                let (sin, cos) = rotation_rad.sin_cos();
                for spoke in 0..180 {
                    let azimuth_deg = f64::from(spoke) * 2.0;
                    let azimuth = azimuth_deg.to_radians();
                    for range_km in [1.0_f64, 50.0, 120.0, 230.0, 340.0, 460.0] {
                        // Where the raster paints the gate.
                        let screen_bearing = azimuth + f64::from(*rotation_rad);
                        let radius = range_km / f64::from(km_per_point);
                        let painted = ScreenPoint::new(
                            antenna.x + (radius * screen_bearing.sin()) as f32,
                            antenna.y - (radius * screen_bearing.cos()) as f32,
                        );
                        let _ = (sin, cos);

                        // Where the gate's GROUND is. Out to longitude and
                        // latitude by the geodesic, and back through the
                        // projection the basemap is built with.
                        let polar =
                            WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                        let (lon, lat) = projection.world_to_lon_lat(polar);
                        let ground = projection
                            .try_lon_lat_to_world(lon, lat)
                            .expect("a gate inside the footprint projects");
                        let drawn = camera.world_to_screen(ground, PANE);

                        let error =
                            f64::from(painted.x - drawn.x).hypot(f64::from(painted.y - drawn.y));
                        if error > worst_points {
                            worst_points = error;
                            worst_site = name;
                        }
                        worst_metres = worst_metres.max(error * f64::from(km_per_point) * 1_000.0);
                    }
                }
            }
        }
    }
    // Two bounds, because a screen point means different ground at different
    // scales and only the pair says anything. The residual is `f32` rounding
    // inside `world_to_screen` on coordinates that reach 46 000 points at the
    // finest scale the camera allows - it is not geometry, and it does not
    // grow with the rotation.
    //
    // For comparison, `globe` holds its own handoff to 0.057 screen points for
    // the same class of error, and a NEXRAD gate is 250 m long.
    assert!(
        worst_points < 0.05,
        "{worst_site}: a gate landed {worst_points} screen points from its own ground"
    );
    assert!(
        worst_metres < 1.0,
        "{worst_site}: a gate landed {worst_metres} m from its own ground"
    );
    println!(
        "worst gate registration error: {worst_points:.6} screen points / {worst_metres:.4} m \
         of ground ({worst_site})"
    );
}

/// The fix, as a number rather than as a look.
///
/// `north_bearing_rad` is the screen bearing of true north at a point, so the
/// RMS of it over a grid on the pane is exactly "how far off axis is this map
/// drawn". Measured on the panes the application really draws, from the anchor
/// in the complaint.
#[test]
fn the_eastern_united_states_stops_being_drawn_off_axis_from_a_portland_anchor() {
    let krtx = RadarProjection::new(KRTX.0, KRTX.1);
    // (label, view centre, scale, worst RMS allowed after, minimum improvement)
    let cases = [
        (
            "eastern seaboard",
            -75.0_f64,
            40.0_f64,
            1.0_f32,
            4.5_f64,
            8.0_f64,
        ),
        ("Chicago mesoscale", -87.63, 41.88, 0.7, 3.5, 8.0),
        ("eastern half", -85.0, 39.0, 1.6, 7.0, 4.0),
        ("whole CONUS", -98.58, 39.83, 2.8, 11.5, 1.5),
    ];
    for (label, lon, lat, km_per_point, allowed_rms, minimum_gain) in cases {
        let centre = krtx.try_lon_lat_to_world(lon, lat).expect("a view centre");
        let blend = globe::blend_for_pane(km_per_point, PANE);
        assert_eq!(blend, 0.0, "{label} should be well below the globe floor");
        let rotation = krtx.view_rotation_rad(centre, km_per_point);
        let (before_rms, before_max) = off_axis(&krtx, camera(centre, km_per_point, 0.0), blend);
        let (after_rms, after_max) = off_axis(&krtx, camera(centre, km_per_point, rotation), blend);
        println!(
            "{label} at {km_per_point} km/point: applied {:.4} deg, RMS {before_rms:.2} -> \
             {after_rms:.2} deg, max {before_max:.2} -> {after_max:.2} deg",
            rotation.to_degrees()
        );
        assert!(
            after_rms < allowed_rms,
            "{label}: still {after_rms} deg off axis"
        );
        assert!(
            before_rms - after_rms > minimum_gain,
            "{label}: only improved by {} deg",
            before_rms - after_rms
        );
        // And at the middle of the pane, which is what the rotation is
        // anchored on, north really is up.
        let residual = f64::from(rotation).to_degrees()
            + krtx
                .north_bearing_rad(centre)
                .expect("the centre has a north")
                .to_degrees();
        assert!(
            residual.abs() < 0.01,
            "{label}: north is {residual} deg off screen-up at the view centre"
        );
    }
}

/// A pane centred on its own antenna is a SYMMETRIC fan, not a tilt, and a
/// rigid rotation cannot improve it. The rule has to leave it alone - which it
/// does for free, because the convergence at the view centre is nil there.
#[test]
fn a_pane_on_its_own_antenna_is_left_alone_because_rotating_it_would_not_help() {
    for (name, latitude, longitude) in [("KRTX", KRTX.0, KRTX.1), ("AWPA2", AWPA2.0, AWPA2.1)] {
        let projection = RadarProjection::new(latitude, longitude);
        let km_per_point = 2.8_f32;
        let blend = globe::blend_for_pane(km_per_point, PANE);
        let rotation = projection.view_rotation_rad(WorldPoint::ORIGIN, km_per_point);
        assert_eq!(rotation.to_bits(), 0.0_f32.to_bits(), "{name}");
        let (rms, _) = off_axis(
            &projection,
            camera(WorldPoint::ORIGIN, km_per_point, 0.0),
            blend,
        );
        // Sweep every rotation and confirm none of them is materially better,
        // so "the rule does nothing here" is a good decision and not a miss.
        let mut best = rms;
        for step in -180..180 {
            let candidate = (f64::from(step) * 0.5).to_radians() as f32;
            let (candidate_rms, _) = off_axis(
                &projection,
                camera(WorldPoint::ORIGIN, km_per_point, candidate),
                blend,
            );
            best = best.min(candidate_rms);
        }
        println!("{name} on its own antenna: RMS {rms:.2} deg, best possible {best:.2} deg");
        assert!(
            rms - best < 0.1,
            "{name}: a rotation would have improved this pane by {} deg and the rule refused it",
            rms - best
        );
    }
}

/// No edge of the domain steps the map.
///
/// Four boundaries: the 460 km floor where the ramp starts, the 920 km top
/// where it finishes, the 5000-6500 km downrange fade, and the 5-7 km per
/// point scale fade that hands the map back before the globe starts forming.
/// Sampled either side of each, in the units the analyst moves in - a
/// kilometre of pan, a wheel notch of zoom.
///
/// The globe's own band is no longer one of them, because the rule is off
/// throughout it: the scale fade reaches zero exactly where
/// `globe::blend_for_pane` leaves zero.
#[test]
fn no_edge_of_the_domain_steps_the_map() {
    let krtx = RadarProjection::new(KRTX.0, KRTX.1);
    let analysis_scale = 2.8_f32;

    // Panning outward through both ends of the near ramp band.
    let mut previous: Option<(f64, f64)> = None;
    let mut worst_step_deg_per_km: f64 = 0.0;
    let mut range_km = 400.0_f64;
    while range_km < 1_100.0 {
        let rotation =
            f64::from(krtx.view_rotation_rad(WorldPoint::new(range_km, 0.0), analysis_scale))
                .to_degrees();
        if let Some((previous_range, previous_rotation)) = previous {
            let step = (rotation - previous_rotation).abs() / (range_km - previous_range);
            worst_step_deg_per_km = worst_step_deg_per_km.max(step);
        }
        previous = Some((range_km, rotation));
        range_km += 1.0;
    }
    println!("worst turn while panning: {worst_step_deg_per_km:.6} deg per km of pan");
    // At the analysis scale a kilometre of pan is 2.9 screen points, so this
    // is the honest statement of B's one real cost: the map turns as you drag.
    assert!(
        worst_step_deg_per_km < 0.05,
        "the map turned {worst_step_deg_per_km} deg in one kilometre of pan"
    );

    // Either side of every threshold, in the smallest steps the arithmetic can
    // express: nothing may STEP. A jump is a discontinuity, and a
    // discontinuity is what the analyst sees as the map flicking.
    let far = krtx
        .try_lon_lat_to_world(-75.0, 40.0)
        .expect("40N 75W projects");
    let at_range = |range_km: f64| {
        f64::from(krtx.view_rotation_rad(WorldPoint::new(range_km, 0.0), analysis_scale))
    };
    let at_scale = |km_per_point: f32| {
        let blend = globe::blend_for_pane(km_per_point, PANE);
        let warped = globe::warp_world(far, blend).unwrap_or(far);
        f64::from(krtx.view_rotation_rad(warped, km_per_point))
    };
    let full_globe = globe::full_globe_scale(PANE);
    let cases: [(&str, f64, f64); 6] = [
        (
            "the surveillance floor",
            at_range(analyst_runtime::NEXRAD_SURVEILLANCE_RANGE_KM * 0.999_999),
            at_range(analyst_runtime::NEXRAD_SURVEILLANCE_RANGE_KM * 1.000_001),
        ),
        (
            "the downrange full-effect edge",
            at_range(map_scene::projection::NORTH_UP_FULL_RANGE_KM * 0.999_999),
            at_range(map_scene::projection::NORTH_UP_FULL_RANGE_KM * 1.000_001),
        ),
        (
            "the downrange zero edge",
            at_range(map_scene::projection::NORTH_UP_ZERO_RANGE_KM * 0.999_999),
            at_range(map_scene::projection::NORTH_UP_ZERO_RANGE_KM * 1.000_001),
        ),
        (
            "the scale full-effect edge",
            at_scale(map_scene::projection::NORTH_UP_FULL_KM_PER_POINT * 0.999_9),
            at_scale(map_scene::projection::NORTH_UP_FULL_KM_PER_POINT * 1.000_1),
        ),
        (
            "the scale zero edge",
            at_scale(map_scene::projection::NORTH_UP_ZERO_KM_PER_POINT * 0.999_9),
            at_scale(map_scene::projection::NORTH_UP_ZERO_KM_PER_POINT * 1.000_1),
        ),
        (
            "the globe's blend start",
            at_scale(full_globe * 0.5 * 0.999_9),
            at_scale(full_globe * 0.5 * 1.000_1),
        ),
    ];
    for (name, below, above) in cases {
        let step_deg = (above - below).to_degrees().abs();
        println!("{name}: {step_deg:.9} deg across the threshold");
        assert!(step_deg < 0.02, "{name} stepped the map by {step_deg} deg");
    }

    // ONE WHEEL NOTCH ANYWHERE IN THE SCALE FADE. The band is a factor of 1.4
    // and a notch is 1.2, so a notch cannot unwind all of it - but the claim
    // that matters to the eye is how far a feature at the edge of the pane
    // moves, and that is what this measures. It is stated rather than bounded
    // against something louder, because inside the domain there is no globe
    // morph riding underneath it any more.
    let mut worst_notch_deg: f64 = 0.0;
    let mut worst_feature_points: f64 = 0.0;
    let mut km_per_point = map_scene::projection::NORTH_UP_FULL_KM_PER_POINT * 0.8;
    while km_per_point < map_scene::projection::NORTH_UP_ZERO_KM_PER_POINT * 1.2 {
        let next = km_per_point * analyst_runtime::ZOOM_PER_NOTCH;
        let rotation_here = krtx.view_rotation_rad(far, km_per_point);
        let rotation_there = krtx.view_rotation_rad(far, next);
        worst_notch_deg =
            worst_notch_deg.max(f64::from(rotation_there - rotation_here).to_degrees().abs());
        // A feature at the far corner of the pane, turned by the difference.
        let corner = 800.0_f64.hypot(450.0);
        worst_feature_points = worst_feature_points
            .max(corner * f64::from(rotation_there - rotation_here).abs().min(2.0));
        km_per_point = next;
    }
    println!(
        "one wheel notch through the scale fade: turns at most {worst_notch_deg:.2} deg, \
         which moves a corner feature {worst_feature_points:.0} points"
    );
    assert!(
        worst_notch_deg < map_scene::projection::MAX_ROTATION_DEG,
        "one notch turned the map {worst_notch_deg} deg"
    );
}

/// RMS and worst |screen bearing of true north| over a grid on the pane, in
/// degrees. Zero is a map drawn north-up.
fn off_axis(projection: &RadarProjection, camera: Camera2D, blend: f32) -> (f64, f64) {
    let mut sum = 0.0;
    let mut worst: f64 = 0.0;
    let mut count = 0.0_f64;
    for row in 0..17 {
        for column in 0..17 {
            let screen = ScreenPoint::new(
                PANE.width_points * column as f32 / 16.0,
                PANE.height_points * row as f32 / 16.0,
            );
            let Some(world) = globe::unwarp_world(camera.screen_to_world(screen, PANE), blend)
            else {
                continue;
            };
            let Some(gamma) = projection.north_bearing_rad(world) else {
                continue;
            };
            // On screen, true north is drawn at `gamma + rotation`.
            let on_screen = (gamma + f64::from(camera.sanitized().rotation_rad)).to_degrees();
            let wrapped = (on_screen + 180.0).rem_euclid(360.0) - 180.0;
            sum += wrapped * wrapped;
            worst = worst.max(wrapped.abs());
            count += 1.0;
        }
    }
    ((sum / count.max(1.0)).sqrt(), worst)
}
