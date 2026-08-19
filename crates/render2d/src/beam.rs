//! Where a radar beam actually is: how high above the antenna, and how far
//! across the ground.
//!
//! A radar reports *slant range* along a tilted beam. Screen distance from the
//! radar is *ground* distance. These are not the same number, and at long range
//! and high tilt they are not close: at 200 km on a 10 degree cut the beam is
//! more than 35 km up, and the ground point beneath it is some 5 km nearer the
//! radar than the slant range says. Computing a beam height from planar screen
//! distance is therefore always wrong, and wrong in a way that looks plausible.
//!
//! Refraction bends the beam back toward the earth, so it travels further
//! before rising above the horizon than a straight ray would. The standard
//! operational treatment replaces the real earth and the real refractive
//! gradient with a fictitious earth of 4/3 the true radius and a straight ray,
//! which reproduces beam height well under an average atmosphere.
//!
//! Doviak, R. J., and D. S. Zrnic, 1993: *Doppler Radar and Weather
//! Observations*, 2nd ed., Academic Press, equations 2.28b and 2.28c.
//!
//! This model is an average-atmosphere approximation. Under a strong
//! temperature inversion the real beam bends further down than 4/3 earth
//! predicts and echoes appear lower than reported; the model does not know
//! that, and neither does anything built on it.

/// Mean earth radius, in metres. The IUGG mean radius.
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// The effective radius of the fictitious earth used to absorb standard
/// atmospheric refraction into a straight-ray geometry.
pub const EFFECTIVE_EARTH_RADIUS_M: f64 = EARTH_RADIUS_M * 4.0 / 3.0;

/// Height of the beam centre above the antenna, in metres.
///
/// `slant_range_m` is the true distance along the beam to the gate centre, not
/// the ground distance and not the gate index times the spacing.
/// `elevation_deg` is the elevation of the sweep the gate came from.
///
/// Doviak and Zrnic (1993) eq. 2.28b:
///
/// ```text
/// h = sqrt(r^2 + R^2 + 2 r R sin(e)) - R
/// ```
pub fn beam_height_arl_m(slant_range_m: f64, elevation_deg: f64) -> f64 {
    let radius = EFFECTIVE_EARTH_RADIUS_M;
    let elevation = elevation_deg.to_radians();
    (slant_range_m * slant_range_m
        + radius * radius
        + 2.0 * slant_range_m * radius * elevation.sin())
    .sqrt()
        - radius
}

/// Distance along the earth's surface from the antenna to the point beneath the
/// gate, in metres.
///
/// Doviak and Zrnic (1993) eq. 2.28c:
///
/// ```text
/// s = R * asin(r cos(e) / (R + h))
/// ```
pub fn ground_arc_m(slant_range_m: f64, elevation_deg: f64) -> f64 {
    let radius = EFFECTIVE_EARTH_RADIUS_M;
    let elevation = elevation_deg.to_radians();
    let height = beam_height_arl_m(slant_range_m, elevation_deg);
    let ratio = slant_range_m * elevation.cos() / (radius + height);
    radius * ratio.clamp(-1.0, 1.0).asin()
}

/// Both quantities at once, which is what every caller actually wants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeamPoint {
    pub height_arl_m: f64,
    pub ground_arc_m: f64,
}

pub fn beam_point(slant_range_m: f64, elevation_deg: f64) -> BeamPoint {
    BeamPoint {
        height_arl_m: beam_height_arl_m(slant_range_m, elevation_deg),
        ground_arc_m: ground_arc_m(slant_range_m, elevation_deg),
    }
}

/// The slant range that puts a gate a given distance across the ground.
///
/// The inverse of [`ground_arc_m`], found by bisection because the forward
/// relation has no convenient closed inverse. Needed when sampling a Cartesian
/// analysis grid: the grid knows where a cell is on the ground, and the sweep
/// is indexed by slant range.
///
/// Returns `None` when no slant range within `max_slant_range_m` reaches the
/// requested ground distance, which is the honest answer for a cell beyond the
/// sweep rather than a clamped range that would sample the last gate forever.
pub fn slant_range_for_ground_arc_m(
    ground_distance_m: f64,
    elevation_deg: f64,
    max_slant_range_m: f64,
) -> Option<f64> {
    if !ground_distance_m.is_finite() || ground_distance_m < 0.0 {
        return None;
    }
    if ground_distance_m == 0.0 {
        return Some(0.0);
    }

    // Closed form from the sine rule on the triangle whose vertices are the
    // earth's centre, the antenna and the gate. With `phi = s / R` the
    // earth-central angle, the angle at the antenna between the vertical and
    // the beam is `90 + e`, so the angle at the gate is `90 - e - phi`, and
    //
    //     r / sin(phi) = R / sin(90 - e - phi) = R / cos(e + phi)
    //
    // This replaces a sixty-iteration bisection that cost about forty seconds
    // for one full-radius analysis grid over fifteen tilts. It is not merely
    // faster, it is exact.
    let radius = EFFECTIVE_EARTH_RADIUS_M;
    let elevation = elevation_deg.to_radians();
    let central_angle = ground_distance_m / radius;
    let denominator = (elevation + central_angle).cos();
    if denominator <= 0.0 {
        // The beam has passed the horizon of the effective earth: no slant
        // range reaches this ground distance at this elevation.
        return None;
    }
    let slant_range_m = radius * central_angle.sin() / denominator;
    (slant_range_m.is_finite() && slant_range_m >= 0.0 && slant_range_m <= max_slant_range_m)
        .then_some(slant_range_m)
}

/// Compass bearing of a point given in radar-local east/north kilometres.
///
/// `atan2(east, north)`, not the mathematical `atan2(north, east)`. Compass
/// bearings run clockwise from north; mathematical angles run anticlockwise
/// from east. Using the wrong one mirrors the sweep about the 45 degree line,
/// which on a symmetric storm looks almost right.
pub fn compass_azimuth_deg(east_km: f64, north_km: f64) -> f64 {
    east_km.atan2(north_km).to_degrees().rem_euclid(360.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straight up, the beam height is exactly the slant range and the ground
    /// arc is exactly zero. An exact identity, so it pins the formula rather
    /// than merely agreeing with it approximately.
    #[test]
    fn a_vertical_beam_rises_by_its_own_slant_range_and_travels_no_ground_distance() {
        let point = beam_point(10_000.0, 90.0);
        assert!(
            (point.height_arl_m - 10_000.0).abs() < 1e-6,
            "vertical beam height was {}",
            point.height_arl_m
        );
        assert!(
            point.ground_arc_m.abs() < 1e-6,
            "vertical beam ground arc was {}",
            point.ground_arc_m
        );
    }

    #[test]
    fn a_zero_range_gate_sits_at_the_antenna() {
        let point = beam_point(0.0, 0.5);
        assert_eq!(point.height_arl_m, 0.0);
        assert_eq!(point.ground_arc_m, 0.0);
    }

    /// Along a level beam the earth curves away underneath at r^2 / 2R. At
    /// 10 km that is about 5.9 m over the 4/3 earth.
    #[test]
    fn a_level_beam_rises_only_by_the_curvature_of_the_effective_earth() {
        let height = beam_height_arl_m(10_000.0, 0.0);
        let expected = 10_000.0_f64.powi(2) / (2.0 * EFFECTIVE_EARTH_RADIUS_M);
        assert!(
            (height - expected).abs() < 0.01,
            "level beam rose {height} m, small-angle form gives {expected} m"
        );
    }

    /// The operational rule of thumb every forecaster knows: the 0.5 degree
    /// beam is roughly 1.5 km up at 100 km. Pinning it here means a change to
    /// the earth model has to argue with a number people recognise.
    #[test]
    fn the_lowest_tilt_is_about_one_and_a_half_kilometres_up_at_one_hundred_kilometres() {
        let height = beam_height_arl_m(100_000.0, 0.5);
        assert!(
            (height - 1_464.0).abs() < 5.0,
            "0.5 deg at 100 km should be near 1464 m, got {height}"
        );
    }

    #[test]
    fn a_higher_tilt_is_higher_at_the_same_range() {
        let low = beam_height_arl_m(100_000.0, 0.5);
        let high = beam_height_arl_m(100_000.0, 10.0);
        assert!(high > low);
        // 10 degrees at 100 km is well above 17 km, which is why hail
        // algorithms must never treat a tilt index as a height.
        assert!(high > 17_000.0, "10 deg at 100 km was only {high} m");
    }

    /// The distinction the whole module exists for: on a steep tilt the ground
    /// point is far nearer the radar than the slant range.
    #[test]
    fn ground_distance_falls_short_of_slant_range_on_a_steep_tilt() {
        let slant = 100_000.0;
        let ground = ground_arc_m(slant, 10.0);
        assert!(
            ground < slant - 1_000.0,
            "ground arc {ground} should be well short of slant {slant}"
        );
    }

    /// On a nearly level beam at short range the two are almost the same, which
    /// is exactly why the error is easy to miss until it matters.
    #[test]
    fn ground_distance_nearly_equals_slant_range_on_a_flat_short_beam() {
        let ground = ground_arc_m(20_000.0, 0.5);
        assert!((ground - 20_000.0).abs() < 20.0, "ground arc was {ground}");
    }

    #[test]
    fn the_ground_arc_inverse_returns_the_slant_range_it_came_from() {
        for (slant, elevation) in [
            (30_000.0, 0.5),
            (100_000.0, 0.5),
            (230_000.0, 0.5),
            (60_000.0, 4.0),
            (100_000.0, 10.0),
        ] {
            let ground = ground_arc_m(slant, elevation);
            let recovered = slant_range_for_ground_arc_m(ground, elevation, 500_000.0)
                .expect("a ground arc produced from a real slant range must invert");
            assert!(
                (recovered - slant).abs() < 1.0,
                "{elevation} deg: {slant} m round-tripped to {recovered} m"
            );
        }
    }

    /// The closed form replaced a bisection. Bisection is slow but obviously
    /// correct, so it stays here as the oracle the fast path is checked
    /// against - if the algebra were wrong this is what would catch it.
    #[test]
    fn the_closed_form_inverse_agrees_with_a_bisection_search() {
        fn by_bisection(ground_distance_m: f64, elevation_deg: f64) -> f64 {
            let mut low = 0.0_f64;
            let mut high = 1_000_000.0_f64;
            for _ in 0..80 {
                let middle = 0.5 * (low + high);
                if ground_arc_m(middle, elevation_deg) < ground_distance_m {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            0.5 * (low + high)
        }

        for elevation in [0.0, 0.5, 2.4, 10.0, 19.5] {
            for ground_km in [1.0, 25.0, 100.0, 230.0, 400.0] {
                let ground_m = ground_km * 1000.0;
                let Some(closed) = slant_range_for_ground_arc_m(ground_m, elevation, 1_000_000.0)
                else {
                    continue;
                };
                let searched = by_bisection(ground_m, elevation);
                assert!(
                    (closed - searched).abs() < 0.5,
                    "{elevation} deg at {ground_km} km: closed form {closed} m,                      bisection {searched} m"
                );
            }
        }
    }

    #[test]
    fn a_ground_point_beyond_the_sweep_has_no_slant_range() {
        // 400 km across the ground cannot be reached by a 230 km sweep.
        assert_eq!(
            slant_range_for_ground_arc_m(400_000.0, 0.5, 230_000.0),
            None,
            "a cell beyond the sweep must report no coverage, not the last gate"
        );
    }

    #[test]
    fn compass_bearings_run_clockwise_from_north() {
        assert!((compass_azimuth_deg(0.0, 10.0) - 0.0).abs() < 1e-9, "north");
        assert!((compass_azimuth_deg(10.0, 0.0) - 90.0).abs() < 1e-9, "east");
        assert!(
            (compass_azimuth_deg(0.0, -10.0) - 180.0).abs() < 1e-9,
            "south"
        );
        assert!(
            (compass_azimuth_deg(-10.0, 0.0) - 270.0).abs() < 1e-9,
            "west"
        );
    }

    #[test]
    fn a_north_east_bearing_is_forty_five_degrees_not_its_mirror() {
        // The mathematical convention would answer 45 here too, so the
        // distinguishing case is one that is not on the diagonal.
        assert!((compass_azimuth_deg(10.0, 10.0) - 45.0).abs() < 1e-9);
        // 10 east, 1 north is nearly due east: 84 degrees on a compass, but
        // only 6 degrees under the mathematical convention.
        let bearing = compass_azimuth_deg(10.0, 1.0);
        assert!(
            (bearing - 84.289_406_9).abs() < 1e-6,
            "expected a near-easterly compass bearing, got {bearing}"
        );
    }
}
