//! Orthographic globe for the far-zoom view.
//!
//! # Why this module exists
//!
//! [`crate::projection::RadarProjection`] is a geodesic azimuthal-equidistant
//! projection anchored at the antenna. That is the right projection for radar
//! work and it is not negotiable: screen distance from the anchor IS ground
//! distance, which is what makes the vector map agree with the polar gates
//! `render2d` draws. It is, however, only a *local* projection. Its tangential
//! scale factor is `c / sin c` for an angular distance `c`, measured on the
//! real site catalogue as:
//!
//! | range from the anchor | tangential stretch |
//! |----------------------:|-------------------:|
//! |                460 km |           x 1.0009 |
//! |              1 000 km |           x 1.0041 |
//! |              5 000 km |           x 1.1105 |
//! |             11 699 km |           x 1.9029 |
//! |             18 182 km |           x 10.054 |
//!
//! (11 699 km is RODN and 18 182 km is the furthest basemap vertex, both
//! measured from KTLX.) Past a few thousand kilometres the picture stops being
//! a map of the earth and becomes a disc whose rim is the antipode, smeared by
//! an unbounded factor. That is the far-zoom view in the screenshot.
//!
//! # What replaces it, and why orthographic
//!
//! Orthographic (Snyder 1987, section 20, pp. 145-153) is the view of a globe
//! from infinite distance: the earth drawn as a sphere you look AT rather than
//! a plane you look down on. It is chosen over the alternatives because:
//!
//! * It is azimuthal about the same anchor and preserves the same azimuths as
//!   the azimuthal-equidistant projection already in use. Both are therefore
//!   the SAME map up to a radial function `rho(c)` - `R c` for equidistant,
//!   `R sin c` for orthographic. That is the whole reason a continuous handoff
//!   is possible at all (see [`warp_world`]).
//! * The far hemisphere culls itself: a point is drawn exactly when its
//!   surface normal faces the viewer, `cos c >= 0` (Snyder p. 149).
//! * Its inverse is closed form and it has no free parameter to tune, unlike
//!   the vertical (near-side) perspective projection of Snyder section 22,
//!   which would need a viewing altitude chosen out of the air and buys only a
//!   slightly more dramatic limb.
//!
//! Orthographic is neither conformal nor equal-area. Nothing is measured at
//! globe scale - the analyst measures at 50-460 km, where this module is
//! inert - so the property that matters is that it LOOKS like a planet.
//!
//! # The handoff
//!
//! Blending between projections as a function of map scale, so an interactive
//! map is planar when zoomed in and globular when zoomed out, is Jenny's
//! adaptive composite map projection (Jenny 2012). This module follows that
//! design with a two-projection composite: the radar-local equidistant frame
//! and the orthographic globe, blended by [`blend_for_pane`].
//!
//! The blend runs on the radial function only:
//!
//! ```text
//! rho(c, t) = R * [ (1 - t) * c  +  t * sin c ]
//! ```
//!
//! `t = 0` is the shipped projection, bit for bit. `t = 1` is textbook
//! orthographic. Because the blend is on `rho` alone and the azimuth is
//! untouched, every intermediate `t` is itself a legitimate azimuthal
//! projection, and the morph is continuous in `t` and in `c`.
//!
//! # References
//!
//! * Snyder, J.P. (1987). *Map Projections - A Working Manual.* USGS
//!   Professional Paper 1395. Orthographic, section 20 pp. 145-153; azimuthal
//!   equidistant, section 25 pp. 191-202. doi:10.3133/pp1395
//! * Jenny, B. (2012). Adaptive Composite Map Projections. *IEEE Transactions
//!   on Visualization and Computer Graphics* 18(12), 2575-2582.
//!   doi:10.1109/TVCG.2012.192
//! * Moritz, H. (2000). Geodetic Reference System 1980. *Journal of Geodesy*
//!   74(1), 128-133. doi:10.1007/s001900050278 - the mean radius used here.
//! * Perlin, K. (1985). An Image Synthesizer. *SIGGRAPH Computer Graphics*
//!   19(3), 287-296. doi:10.1145/325165.325247 - the smoothstep ramp.

use analyst_runtime::{ViewportMetrics, WorldPoint};

/// Mean radius of the earth, `R1 = (2a + b) / 3` for GRS80/WGS84, in
/// kilometres (Moritz 2000). The globe is drawn on a sphere, which is what
/// Snyder assumes for the orthographic projection (1987, p. 145): the
/// ellipsoidal form has no closed inverse, and the flattening is 0.3% of the
/// radius, well under a screen point at any scale this module is active at.
pub const EARTH_MEAN_RADIUS_KM: f64 = 6_371.008_8;

/// Azimuthal-equidistant radius of the antipode, `pi * R`. No point can
/// project further than this, because no geodesic is longer than half the
/// circumference.
pub const ANTIPODAL_RADIUS_KM: f64 = std::f64::consts::PI * EARTH_MEAN_RADIUS_KM;

/// Azimuthal-equidistant radius of the limb of a full orthographic globe,
/// `pi/2 * R` - the great circle 90 degrees from the anchor.
pub const NEAR_HEMISPHERE_RADIUS_KM: f64 = std::f64::consts::FRAC_PI_2 * EARTH_MEAN_RADIUS_KM;

/// Camera scale at or below which this module is completely inert, whatever
/// the pane looks like.
///
/// This is a FLOOR, not the handoff. The handoff itself depends on the size of
/// the pane (see [`full_globe_scale`]), and on a very large pane it would
/// otherwise creep down into scales an analyst might actually use. Nothing
/// below this line bends, ever, on any pane, at any window size.
///
/// 7 km/point is 20x coarser than [`analyst_runtime::DEFAULT_KM_PER_POINT`]
/// and 7x coarser than the coarsest continental view an analyst works at: the
/// whole of the contiguous United States is 4 500 km, which is 5.6 km/point
/// across a 800-point half pane. At 7 km/point the 460 km Level II footprint
/// is a 131-point thumbnail. No storm is interrogated there, which is what
/// makes it safe to bend the map past this point and nowhere before it.
pub const MIN_BLEND_KM_PER_POINT: f32 = 7.0;

/// How much of the pane's shorter side a fully formed globe fills.
///
/// Not 1.0: a globe that touches the top and bottom of the pane has no
/// horizon around it, and the thing that makes a sphere read as a sphere is
/// the black it sits in.
pub const GLOBE_PANE_FRACTION: f32 = 0.9;

/// Level II's longest unambiguous range, used to bound the error the globe
/// blend can introduce inside the radar footprint. See
/// [`radar_footprint_error_points`].
pub const RADAR_FOOTPRINT_KM: f64 = 460.0;

/// The camera scale at which this pane shows a fully formed globe.
///
/// This is the whole reason the handoff is not a fixed number of kilometres
/// per point. A globe is `2R / km_per_point` points across, so the scale at
/// which it fits a pane depends entirely on how big that pane is: 14 km/point
/// for a single 900-point-tall pane, 28 for the same window split four ways.
/// A fixed threshold has to pick one of those and be wrong on the other - at
/// 32 km/point, which is right for a quarter pane, a full-window globe is 398
/// points across in a 900-point pane, a marble in a field of black. That was
/// measured, rendered and looked at, not reasoned about.
///
/// Blending by how much of the world is on screen rather than by raw scale is
/// what Jenny (2012) means by an adaptive composite projection; his own
/// transition is likewise expressed in terms of the map's height against the
/// globe's.
///
/// The result is never finer than `2 * MIN_BLEND_KM_PER_POINT`, so the band
/// below can never reach into the analysis range however large the window is.
#[must_use]
pub fn full_globe_scale(viewport: ViewportMetrics) -> f32 {
    let viewport = viewport.sanitized();
    let shorter_side = viewport.width_points.min(viewport.height_points);
    let fitted = 2.0 * EARTH_MEAN_RADIUS_KM as f32 / (shorter_side * GLOBE_PANE_FRACTION).max(1.0);
    fitted.max(2.0 * MIN_BLEND_KM_PER_POINT)
}

/// How far this pane has been carried from the radar-local projection towards
/// the orthographic globe, in `0..=1`.
///
/// `0.0` and `1.0` are returned EXACTLY, not approached: callers switch on
/// `blend == 0.0` to take the untouched radar-local path, so the boundary has
/// to be a bit pattern and not a tolerance.
///
/// The band is one octave wide and ends at [`full_globe_scale`], so the globe
/// finishes forming exactly as it comes to fit the pane. One octave is about
/// four wheel notches; the ramp across it is Perlin's smoothstep,
/// `3x^2 - 2x^3` (Perlin 1985), whose first derivative vanishes at both ends,
/// so the morph starts and stops without a kink in its rate.
#[must_use]
pub fn blend_for_pane(km_per_point: f32, viewport: ViewportMetrics) -> f32 {
    if !km_per_point.is_finite() || km_per_point <= MIN_BLEND_KM_PER_POINT {
        return 0.0;
    }
    let full = full_globe_scale(viewport);
    let start = (full * 0.5).max(MIN_BLEND_KM_PER_POINT);
    if km_per_point >= full {
        return 1.0;
    }
    if km_per_point <= start {
        return 0.0;
    }
    let x = (km_per_point - start) / (full - start);
    x * x * (3.0 - 2.0 * x)
}

/// Angular distance, in radians, at which the blended radial function stops
/// increasing - the limb.
///
/// `d(rho)/dc = R * [(1 - t) + t cos c]`, which first reaches zero at
/// `cos c = -(1 - t) / t`. Beyond that point the map would fold the far side
/// back over the near side, drawing two different places on the same pixel. So
/// the limb is not a rule bolted on beside the projection: it is the point
/// where the projection stops being one.
///
/// At `t = 1` this is `pi/2` exactly, which is Snyder's orthographic
/// visibility condition `cos c >= 0` (1987, p. 149). At `t <= 0.5` the
/// function never folds and the whole sphere is drawn, as the shipped
/// azimuthal-equidistant view already does.
#[must_use]
pub fn horizon_angle_rad(blend: f32) -> f64 {
    let t = f64::from(blend.clamp(0.0, 1.0));
    if t <= 0.5 {
        return std::f64::consts::PI;
    }
    (-(1.0 - t) / t).acos()
}

/// The limb expressed as a radius in the radar-local world frame, which is
/// where the geometry builder and the marker layer do their culling.
#[must_use]
pub fn horizon_radius_km(blend: f32) -> f64 {
    horizon_angle_rad(blend) * EARTH_MEAN_RADIUS_KM
}

/// Radial multiplier applied to a world position at angular distance `c`.
///
/// `1.0` at `blend == 0`, `sin(c)/c` at `blend == 1`. Both factors are `<= 1`
/// for `c` in `0..=pi`, so the morph is a CONTRACTION of the radar-local
/// plane. That matters to [`crate::build`]: a Douglas-Peucker tolerance
/// measured in the equidistant frame can only over-estimate the error the same
/// simplification produces on the globe, so simplification stays conservative
/// without knowing anything about the blend.
#[must_use]
pub fn radial_factor(c_rad: f64, blend: f32) -> f64 {
    let t = f64::from(blend.clamp(0.0, 1.0));
    if c_rad.abs() < 1e-12 {
        return 1.0;
    }
    (1.0 - t) + t * c_rad.sin() / c_rad
}

/// Carry a radar-local world position onto the blended globe.
///
/// `None` means the point is behind the limb and must not be drawn. Callers
/// break the polyline there rather than substituting a position: any
/// substitute draws a line to somewhere real.
///
/// At `blend == 0.0` the input is returned unchanged - the same `f64` bits, by
/// an early return and not by arithmetic that happens to be the identity. That
/// is what makes the radar-local view at analysis zoom provably untouched.
#[must_use]
pub fn warp_world(world: WorldPoint, blend: f32) -> Option<WorldPoint> {
    if blend == 0.0 {
        return Some(world);
    }
    if !world.east_km.is_finite() || !world.north_km.is_finite() {
        return None;
    }
    let radius_km = world.east_km.hypot(world.north_km);
    if radius_km < 1e-12 {
        return Some(world);
    }
    let c = radius_km / EARTH_MEAN_RADIUS_KM;
    if c > horizon_angle_rad(blend) {
        return None;
    }
    let factor = radial_factor(c, blend);
    Some(WorldPoint {
        east_km: world.east_km * factor,
        north_km: world.north_km * factor,
    })
}

/// Inverse of [`warp_world`], for hit testing and the cursor readout.
///
/// `rho(c)` is strictly increasing on `0..=horizon`, so a bisection on `c` is
/// exact to the tolerance it is run to and cannot pick the wrong branch. Sixty
/// halvings of an interval under `pi` leave far less than a nanometre, so this
/// is exact for every purpose the application has.
#[must_use]
pub fn unwarp_world(warped: WorldPoint, blend: f32) -> Option<WorldPoint> {
    if blend == 0.0 {
        return Some(warped);
    }
    if !warped.east_km.is_finite() || !warped.north_km.is_finite() {
        return None;
    }
    let target_km = warped.east_km.hypot(warped.north_km);
    if target_km < 1e-12 {
        return Some(warped);
    }
    let horizon = horizon_angle_rad(blend);
    let limb_km = horizon * EARTH_MEAN_RADIUS_KM * radial_factor(horizon, blend);
    if target_km > limb_km {
        return None;
    }
    let mut low = 0.0_f64;
    let mut high = horizon;
    for _ in 0..60 {
        let mid = (low + high) * 0.5;
        let rho = mid * EARTH_MEAN_RADIUS_KM * radial_factor(mid, blend);
        if rho < target_km {
            low = mid;
        } else {
            high = mid;
        }
    }
    let c = (low + high) * 0.5;
    let scale = c * EARTH_MEAN_RADIUS_KM / target_km;
    Some(WorldPoint {
        east_km: warped.east_km * scale,
        north_km: warped.north_km * scale,
    })
}

/// Width of the fade that hides the far hemisphere, in radians of angular
/// distance from the anchor.
///
/// # Why a fade and not a cull
///
/// The far hemisphere has to be hidden by the thing that draws the map, and
/// for the vector basemap that is a vertex shader. A vertex shader cannot
/// discard a primitive, and the geometry it is fed cannot be culled in
/// advance: one vertex buffer is drawn across a whole LOD bucket, and the limb
/// moves with both the live camera scale and the size of the pane, so any
/// radius the build could pick would delete visible map at one end of the
/// bucket or leave far-side geometry in at the other.
///
/// What is left is to clamp the far side onto the limb and then make it
/// invisible. Clamping alone is NOT enough, and that was rendered rather than
/// reasoned about: with the far side clamped and fully opaque, 20 492 of
/// RODN's vertices land on the limb circle and draw a hard bright ring around
/// the globe - 1 077 pixels of ink that is not a coastline, on a frame that
/// only has 11 216 pixels of real ink. From KTLX, whose antipode is empty
/// ocean, the same bug costs 1 231 pixels. It reads as a drawn circle around
/// the earth, which is exactly what a globe must not have.
///
/// So the shader carries the angular distance CLAMPED TO THE HORIZON as a
/// varying and multiplies alpha by this fade. A segment with both ends behind
/// the limb then carries fade 0 along its whole length and disappears; a
/// segment that crosses the limb fades out as it reaches it. Clamping the
/// varying as well as the position is what makes those two cases agree - an
/// unclamped varying would cut a crossing segment short of the limb and leave
/// a gap.
///
/// # Why this width
///
/// The limb is a stationary point of the radial function - `d(rho)/dc` is zero
/// there for every blend - so a band of angular distance next to it maps to
/// almost no screen distance at all. At a full blend this band is 3 degrees of
/// the earth and 9 km of `rho`, which is 0.28 of a screen point at 32
/// km/point. It is wide enough in `c` that interpolation noise cannot bring a
/// far-side fragment back to life, and narrow enough in `rho` that it hides
/// nothing a viewer could otherwise see.
pub const LIMB_FADE_RAD: f64 = 0.05;

/// How much of a point at angular distance `c_rad` survives the limb, in
/// `0..=1`. The CPU mirror of the shader's `limb_fade`, and the definition
/// both sides must agree on.
///
/// `c_rad` is clamped to the horizon here exactly as the shader clamps its
/// varying, so a position behind the limb returns a hard `0.0` rather than a
/// negative number that some later `clamp` has to catch.
#[must_use]
pub fn limb_fade(c_rad: f64, blend: f32) -> f64 {
    let horizon = horizon_angle_rad(blend);
    if horizon >= std::f64::consts::PI {
        return 1.0;
    }
    ((horizon - c_rad.abs().min(horizon)) / LIMB_FADE_RAD).clamp(0.0, 1.0)
}

/// The width of the limb fade expressed where it matters: screen points.
///
/// This is the measurement that says the fade hides nothing real. It is the
/// distance on screen between the outermost fully opaque point and the limb
/// itself.
#[must_use]
pub fn limb_fade_width_points(blend: f32, km_per_point: f32) -> f64 {
    let horizon = horizon_angle_rad(blend);
    if horizon >= std::f64::consts::PI || !km_per_point.is_finite() || km_per_point <= 0.0 {
        return 0.0;
    }
    let opaque = (horizon - LIMB_FADE_RAD).max(0.0);
    let rho = |c: f64| c * EARTH_MEAN_RADIUS_KM * radial_factor(c, blend);
    (rho(horizon) - rho(opaque)) / f64::from(km_per_point)
}

/// Largest error, in screen points, that the globe blend can introduce
/// anywhere inside the radar footprint at a given camera scale.
///
/// This is the whole argument for leaving `render2d` alone. The radar raster
/// is drawn in radar-local kilometres and cannot be reprojected without
/// destroying it, so the question is how far the echo can disagree with the
/// globe under it. The displacement at range `r` is `r - R sin(r/R)`, which is
/// approximately `r^3 / (6 R^2)` - a fixed number of kilometres; the screen
/// error divides it by the camera scale.
///
/// This takes the WORST CASE over every pane: a full blend, which no pane
/// reaches until well past [`MIN_BLEND_KM_PER_POINT`], divided by the finest
/// scale at which any pane can blend at all. Below that floor it is exactly
/// zero because the blend is exactly zero.
///
/// The test below measures it: 0.057 of a screen point at the very worst,
/// falling from there. The echo therefore needs no special handling at all -
/// it is not hidden, not redrawn as a patch, and not reprojected. It stays
/// exactly where `render2d` put it.
#[must_use]
pub fn radar_footprint_error_points(km_per_point: f32) -> f64 {
    if !km_per_point.is_finite() || km_per_point <= MIN_BLEND_KM_PER_POINT {
        return 0.0;
    }
    let c = RADAR_FOOTPRINT_KM / EARTH_MEAN_RADIUS_KM;
    let displacement_km = RADAR_FOOTPRINT_KM * (1.0 - radial_factor(c, 1.0));
    displacement_km / f64::from(km_per_point)
}

/// Spherical orthographic projection, as published.
///
/// The pipeline does not use this: it uses [`warp_world`], which reaches the
/// same picture by bending the radar-local frame it already has, and so keeps
/// exact continuity with the geodesic distances the radar is drawn in. This
/// type is the independent reference the shortcut is checked against, and the
/// honest place to state what the shortcut costs.
///
/// Snyder (1987), section 20: forward equations 20-3 and 20-4 (p. 149),
/// visibility `cos c >= 0` (p. 149), inverse equations 20-14 to 20-18 (p. 150).
#[derive(Clone, Copy, Debug)]
pub struct GlobeProjection {
    center_lat_rad: f64,
    center_lon_rad: f64,
    sin_center_lat: f64,
    cos_center_lat: f64,
}

impl GlobeProjection {
    #[must_use]
    pub fn new(center_lat_deg: f64, center_lon_deg: f64) -> Self {
        let lat = if center_lat_deg.is_finite() {
            center_lat_deg.clamp(-90.0, 90.0)
        } else {
            0.0
        };
        let lon = if center_lon_deg.is_finite() {
            center_lon_deg
        } else {
            0.0
        };
        let center_lat_rad = lat.to_radians();
        Self {
            center_lat_rad,
            center_lon_rad: lon.to_radians(),
            sin_center_lat: center_lat_rad.sin(),
            cos_center_lat: center_lat_rad.cos(),
        }
    }

    /// `cos c` for a point - Snyder equation 5-3 (p. 30). Non-negative exactly
    /// when the point faces the viewer.
    #[must_use]
    pub fn cos_angular_distance(&self, lon_deg: f64, lat_deg: f64) -> f64 {
        let lat = lat_deg.to_radians();
        let delta_lon = lon_deg.to_radians() - self.center_lon_rad;
        self.sin_center_lat * lat.sin() + self.cos_center_lat * lat.cos() * delta_lon.cos()
    }

    /// Whether the point is on the hemisphere facing the viewer.
    #[must_use]
    pub fn is_visible(&self, lon_deg: f64, lat_deg: f64) -> bool {
        self.cos_angular_distance(lon_deg, lat_deg) >= 0.0
    }

    /// Snyder 20-3 and 20-4. `None` on the far hemisphere.
    #[must_use]
    pub fn try_lon_lat_to_world(&self, lon_deg: f64, lat_deg: f64) -> Option<WorldPoint> {
        if !lon_deg.is_finite() || !lat_deg.is_finite() {
            return None;
        }
        if self.cos_angular_distance(lon_deg, lat_deg) < 0.0 {
            return None;
        }
        let lat = lat_deg.to_radians();
        let delta_lon = lon_deg.to_radians() - self.center_lon_rad;
        Some(WorldPoint {
            east_km: EARTH_MEAN_RADIUS_KM * lat.cos() * delta_lon.sin(),
            north_km: EARTH_MEAN_RADIUS_KM
                * (self.cos_center_lat * lat.sin()
                    - self.sin_center_lat * lat.cos() * delta_lon.cos()),
        })
    }

    /// Snyder 20-14 to 20-18. `None` outside the drawn disc.
    #[must_use]
    pub fn world_to_lon_lat(&self, world: WorldPoint) -> Option<(f64, f64)> {
        let rho = world.east_km.hypot(world.north_km);
        if rho > EARTH_MEAN_RADIUS_KM {
            return None;
        }
        if rho < 1e-12 {
            return Some((
                self.center_lon_rad.to_degrees(),
                self.center_lat_rad.to_degrees(),
            ));
        }
        let c = (rho / EARTH_MEAN_RADIUS_KM).asin();
        let (sin_c, cos_c) = c.sin_cos();
        let lat = (cos_c * self.sin_center_lat
            + world.north_km * sin_c * self.cos_center_lat / rho)
            .clamp(-1.0, 1.0)
            .asin();
        let lon = self.center_lon_rad
            + (world.east_km * sin_c).atan2(
                rho * cos_c * self.cos_center_lat - world.north_km * sin_c * self.sin_center_lat,
            );
        Some((lon.to_degrees(), lat.to_degrees()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::RadarProjection;

    /// Real NEXRAD sites, transcribed from the live catalogue at
    /// `%LOCALAPPDATA%/FahrenheitResearch/RadarWorkstation/cache/radar-sites.tsv`
    /// so the tests run without it. Every one of these is a real antenna.
    const SITES: &[(&str, f64, f64)] = &[
        ("KTLX", 35.333_049_774_169_92, -97.277_748_107_910_16),
        ("KICT", 37.654_499_053_955_08, -97.442_802_429_199_22),
        ("KAKQ", 36.983_879_089_355_47, -77.007_499_694_824_22),
        ("KRTX", 45.714_968_872_070_31, -122.965_301_513_671_88),
        ("AWPA2", 61.150_001_525_878_906, -149.779_998_779_296_88),
        ("PHKI", 21.894_000_244_140_625, -159.552_001_953_125),
        ("RODN", 26.302_000_045_776_367, 127.909_004_211_425_78),
        ("TJUA", 18.115_600_585_937_5, -66.077_903_747_558_6),
    ];

    /// Panes the application can really produce, from an eighth of a laptop
    /// window to a whole 4K display. The blend is pane dependent, so every
    /// claim about it has to hold on all of them.
    const PANES: &[(&str, f32, f32)] = &[
        ("1600x900 single", 1_600.0, 900.0),
        ("800x450 quarter", 800.0, 450.0),
        ("1280x800 laptop", 1_280.0, 800.0),
        ("3840x2160 4K", 3_840.0, 2_160.0),
        ("640x360 eighth", 640.0, 360.0),
        ("1600x200 letterbox", 1_600.0, 200.0),
    ];

    fn pane(width: f32, height: f32) -> ViewportMetrics {
        ViewportMetrics {
            width_points: width,
            height_points: height,
            pixels_per_point: 1.0,
        }
    }

    #[test]
    fn the_blend_is_exactly_off_across_the_whole_analysis_range_on_every_pane() {
        // analyst_runtime clamps the camera to 0.01..=50 km/point. Everything
        // an analyst works at is at the fine end of that.
        for (name, width, height) in PANES {
            for scale in [
                analyst_runtime::MIN_KM_PER_POINT,
                0.05,
                analyst_runtime::DEFAULT_KM_PER_POINT,
                0.5,
                1.0,
                2.0,
                4.0,
                6.999,
                MIN_BLEND_KM_PER_POINT,
            ] {
                let blend = blend_for_pane(scale, pane(*width, *height));
                assert_eq!(
                    blend.to_bits(),
                    0.0_f32.to_bits(),
                    "blend at {scale} km/point on {name} must be a hard zero, was {blend}"
                );
            }
        }
    }

    /// The globe finishes forming exactly as it comes to fit the pane. This is
    /// the claim the pane-dependent handoff exists to make, so it is asserted
    /// as a measured screen size rather than as a threshold.
    #[test]
    fn a_finished_globe_fills_the_pane_it_was_finished_for() {
        for (name, width, height) in PANES {
            let viewport = pane(*width, *height);
            let full = full_globe_scale(viewport);
            assert_eq!(blend_for_pane(full, viewport).to_bits(), 1.0_f32.to_bits());
            let diameter_points = 2.0 * EARTH_MEAN_RADIUS_KM / f64::from(full);
            let shorter = f64::from(width.min(*height));
            // Either the globe fills its share of the shorter side, or the
            // pane is so small that the floor took over and the globe is
            // smaller than that - never larger.
            assert!(
                diameter_points <= shorter * f64::from(GLOBE_PANE_FRACTION) + 1.0,
                "{name}: a finished globe is {diameter_points:.0} points across a {shorter:.0} point pane"
            );
            if full > 2.0 * MIN_BLEND_KM_PER_POINT {
                assert!(
                    diameter_points >= shorter * f64::from(GLOBE_PANE_FRACTION) - 1.0,
                    "{name}: a finished globe is only {diameter_points:.0} points across"
                );
            }
        }
    }

    /// A bigger pane reaches the globe at a finer scale, and never finer than
    /// the floor.
    #[test]
    fn a_larger_pane_forms_its_globe_earlier_but_never_inside_the_analysis_range() {
        let big = full_globe_scale(pane(1_600.0, 900.0));
        let small = full_globe_scale(pane(800.0, 450.0));
        assert!(big < small, "{big} should be finer than {small}");
        for (name, width, height) in PANES {
            let full = full_globe_scale(pane(*width, *height));
            assert!(
                full >= 2.0 * MIN_BLEND_KM_PER_POINT,
                "{name} would start bending at {} km/point",
                full * 0.5
            );
        }
        // A pane the size of a whole 8K display still does not bend the map
        // anywhere an analyst works.
        assert!(full_globe_scale(pane(7_680.0, 4_320.0)) >= 2.0 * MIN_BLEND_KM_PER_POINT);
    }

    #[test]
    fn a_zero_blend_returns_the_input_bit_for_bit() {
        let projection = RadarProjection::new(SITES[0].1, SITES[0].2);
        for (name, lat, lon) in SITES {
            let world = projection
                .try_lon_lat_to_world(*lon, *lat)
                .expect("a real site projects");
            let warped = warp_world(world, 0.0).expect("zero blend never culls");
            assert_eq!(
                warped.east_km.to_bits(),
                world.east_km.to_bits(),
                "{name} easting moved at zero blend"
            );
            assert_eq!(
                warped.north_km.to_bits(),
                world.north_km.to_bits(),
                "{name} northing moved at zero blend"
            );
        }
    }

    /// The early return in [`warp_world`] is load bearing, and a mutation test
    /// proved it: removing it and letting the arithmetic produce the identity
    /// passed every other test in this file.
    ///
    /// It is not the identity. Without it the horizon test runs at blend zero
    /// as well, and the horizon at blend zero is `pi R` = 20 015 km - while a
    /// GEODESIC on the ellipsoid can be 20 037 km long, because the equator is
    /// longer than a meridian. Everything in that 22 km shell would stop being
    /// drawn on a flat map that has never heard of a globe.
    #[test]
    fn a_flat_map_draws_points_further_away_than_the_spherical_antipode() {
        // Half the equatorial circumference on WGS84: the longest geodesic
        // there is, and 22 km beyond `pi R`.
        const LONGEST_GEODESIC_KM: f64 = 20_037.5;
        const { assert!(LONGEST_GEODESIC_KM > ANTIPODAL_RADIUS_KM) };
        for radius_km in [
            ANTIPODAL_RADIUS_KM - 1.0,
            ANTIPODAL_RADIUS_KM,
            ANTIPODAL_RADIUS_KM + 1.0,
            LONGEST_GEODESIC_KM,
        ] {
            for azimuth_deg in [0.0_f64, 37.0, 180.0, 271.0] {
                let azimuth = azimuth_deg.to_radians();
                let world = WorldPoint::new(radius_km * azimuth.sin(), radius_km * azimuth.cos());
                let drawn = warp_world(world, 0.0).expect("a flat map draws everything");
                assert_eq!(drawn.east_km.to_bits(), world.east_km.to_bits());
                assert_eq!(drawn.north_km.to_bits(), world.north_km.to_bits());
                // And the inverse agrees, so a cursor readout out there still
                // resolves.
                let back = unwarp_world(world, 0.0).expect("a flat map inverts everything");
                assert_eq!(back.east_km.to_bits(), world.east_km.to_bits());
            }
        }
    }

    #[test]
    fn the_blend_reaches_a_hard_one_and_stays_there() {
        for (name, width, height) in PANES {
            let viewport = pane(*width, *height);
            let full = full_globe_scale(viewport);
            for scale in [full, full * 1.5, analyst_runtime::MAX_KM_PER_POINT * 4.0] {
                assert_eq!(
                    blend_for_pane(scale, viewport).to_bits(),
                    1.0_f32.to_bits(),
                    "{name} at {scale} km/point"
                );
            }
        }
    }

    /// Monotone, and no single wheel notch can move the blend by enough to
    /// read as a jump. The wheel is 1.2x per notch, so the step between
    /// neighbouring samples 1.2x apart IS what one notch does.
    #[test]
    fn the_blend_is_monotone_and_no_notch_steps_it() {
        for (name, width, height) in PANES {
            let viewport = pane(*width, *height);
            let mut previous = 0.0_f32;
            let mut scale = analyst_runtime::MIN_KM_PER_POINT;
            let mut worst_notch = 0.0_f32;
            while scale <= analyst_runtime::MAX_KM_PER_POINT * 2.0 {
                let blend = blend_for_pane(scale, viewport);
                assert!(
                    blend >= previous,
                    "{name}: blend went backwards at {scale} km/point"
                );
                worst_notch = worst_notch.max(blend - previous);
                previous = blend;
                scale *= analyst_runtime::zoom_factor_for_notches(1.0);
            }
            assert_eq!(previous, 1.0, "{name} never finished");
            // MEASURED, not chosen: one octave is 3.8 wheel notches and
            // smoothstep peaks at 1.5x its mean rate, so the steepest notch
            // moves the blend by about 0.4. That number on its own says
            // nothing about whether the eye sees a jump, which is what
            // `no_notch_moves_the_map_far_more_than_the_zoom_itself_does`
            // measures. This only pins that the band has not silently
            // narrowed.
            assert!(
                worst_notch < 0.45,
                "{name}: one notch moved the blend by {worst_notch}"
            );
        }
    }

    /// The handoff has to feel continuous, and "continuous" is not a property
    /// of the blend number - it is a property of how far the map moves on
    /// screen when the analyst rolls the wheel one notch.
    ///
    /// So this measures exactly that: for a point at a given angular distance,
    /// the screen distance it travels across one notch WITH the morph, against
    /// the distance the same notch would move it with no morph at all. A ratio
    /// of 1.0 is an ordinary zoom. The morph can only ever add to it, and what
    /// matters is that it stays the same order - a notch that moved the map
    /// several times further than a zoom notch normally does is what reads as
    /// a jump cut.
    ///
    /// The worst case is inherent and cannot be tuned away: the antipodal rim
    /// of the equidistant disc is at `pi R` and the limb of the globe is at
    /// `R`, so the outermost ring of the map MUST contract by a factor of pi
    /// somewhere. Spreading that over more notches would mean starting the
    /// band below `MIN_BLEND_KM_PER_POINT`, which would bend the continental
    /// view an analyst still uses. This test is where that trade is recorded.
    #[test]
    fn no_notch_moves_the_map_far_more_than_the_zoom_itself_does() {
        let notch = analyst_runtime::zoom_factor_for_notches(1.0);
        let mut worst_ratio = 0.0_f64;
        let mut worst_where = (String::new(), 0.0_f32, 0.0_f64);
        for (name, width, height) in PANES {
            let viewport = pane(*width, *height);
            // 1 rad is 6 371 km - the far side of a continent. 3 rad is deep
            // in the antipodal smear, the part that has to collapse.
            for c in [0.1_f64, 0.5, 1.0, 2.0, 3.0] {
                let radius_km = c * EARTH_MEAN_RADIUS_KM;
                let mut scale = MIN_BLEND_KM_PER_POINT;
                while scale <= analyst_runtime::MAX_KM_PER_POINT {
                    let next = scale * notch;
                    let here = radius_km * radial_factor(c, blend_for_pane(scale, viewport))
                        / f64::from(scale);
                    let there = radius_km * radial_factor(c, blend_for_pane(next, viewport))
                        / f64::from(next);
                    let flat_there = radius_km / f64::from(next);
                    let flat_here = radius_km / f64::from(scale);
                    let moved = (here - there).abs();
                    let flat_moved = (flat_here - flat_there).abs();
                    if flat_moved > 1.0 {
                        let ratio = moved / flat_moved;
                        if ratio > worst_ratio {
                            worst_ratio = ratio;
                            worst_where = ((*name).to_owned(), scale, c);
                        }
                    }
                    scale = next;
                }
            }
        }
        assert!(
            worst_ratio < 3.0,
            "one notch moved the map {worst_ratio:.2}x further than the zoom alone would, \
             on {} at {} km/point, {} rad from the anchor",
            worst_where.0,
            worst_where.1,
            worst_where.2
        );
        // And it is never LESS than an ordinary zoom either, which would mean
        // the map had stalled under the analyst's hand.
        assert!(worst_ratio > 1.0, "the morph moved nothing at all");
    }

    #[test]
    fn a_full_blend_hides_exactly_the_far_hemisphere() {
        assert!((horizon_angle_rad(1.0) - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert!((horizon_radius_km(1.0) - NEAR_HEMISPHERE_RADIUS_KM).abs() < 1e-9);

        let just_inside = WorldPoint::new(NEAR_HEMISPHERE_RADIUS_KM - 1.0, 0.0);
        let just_outside = WorldPoint::new(NEAR_HEMISPHERE_RADIUS_KM + 1.0, 0.0);
        assert!(warp_world(just_inside, 1.0).is_some());
        assert!(
            warp_world(just_outside, 1.0).is_none(),
            "the far side must not be drawn"
        );
    }

    #[test]
    fn nothing_is_hidden_until_the_projection_would_fold() {
        // Up to t = 0.5 the radial function is still increasing everywhere, so
        // the whole disc is legitimate and culling any of it would delete map
        // the analyst can still see.
        for blend in [0.0_f32, 0.1, 0.25, 0.5] {
            assert_eq!(horizon_angle_rad(blend), std::f64::consts::PI);
            let antipode = WorldPoint::new(0.0, ANTIPODAL_RADIUS_KM - 1.0);
            assert!(warp_world(antipode, blend).is_some(), "blend {blend}");
        }
    }

    #[test]
    fn the_radial_function_never_folds_inside_the_horizon() {
        for step in 0..=100 {
            let blend = step as f32 / 100.0;
            let horizon = horizon_angle_rad(blend);
            let mut previous = 0.0_f64;
            for index in 1..=2_000 {
                let c = horizon * (f64::from(index) / 2_000.0);
                let rho = c * EARTH_MEAN_RADIUS_KM * radial_factor(c, blend);
                assert!(
                    rho >= previous - 1e-9,
                    "blend {blend} folded at c = {c}: {rho} after {previous}"
                );
                previous = rho;
            }
        }
    }

    #[test]
    fn the_morph_is_a_contraction_so_simplification_stays_conservative() {
        for step in 0..=20 {
            let blend = step as f32 / 20.0;
            let horizon = horizon_angle_rad(blend);
            for index in 0..=500 {
                let c = horizon * (f64::from(index) / 500.0);
                let factor = radial_factor(c, blend);
                assert!(
                    (0.0..=1.0 + 1e-12).contains(&factor),
                    "blend {blend} at c = {c} scaled by {factor}"
                );
            }
        }
    }

    #[test]
    fn the_warp_round_trips_through_its_inverse() {
        for step in 1..=20 {
            let blend = step as f32 / 20.0;
            let horizon_km = horizon_radius_km(blend);
            for radius_fraction in [0.0_f64, 0.01, 0.1, 0.5, 0.9, 0.999] {
                for azimuth_deg in (0..360).step_by(30) {
                    let azimuth = f64::from(azimuth_deg).to_radians();
                    let radius = horizon_km * radius_fraction;
                    let world = WorldPoint::new(radius * azimuth.sin(), radius * azimuth.cos());
                    let warped = warp_world(world, blend).expect("inside the horizon");
                    let back = unwarp_world(warped, blend).expect("inside the limb");
                    let error =
                        (back.east_km - world.east_km).hypot(back.north_km - world.north_km);
                    assert!(
                        error < 1e-6,
                        "blend {blend} radius {radius} drifted {error} km"
                    );
                }
            }
        }
    }

    /// The shortcut this module takes - bending the ellipsoidal, geodesic
    /// radar-local frame rather than projecting lon/lat orthographically on a
    /// sphere - costs the difference between the two earth models. This test
    /// MEASURES that cost against Snyder's published equations rather than
    /// asserting it is small.
    #[test]
    fn the_radial_shortcut_agrees_with_snyders_orthographic_to_a_fraction_of_a_point() {
        let (_, anchor_lat, anchor_lon) = SITES[0];
        let radar = RadarProjection::new(anchor_lat, anchor_lon);
        let globe = GlobeProjection::new(anchor_lat, anchor_lon);
        let mut worst_km = 0.0_f64;
        let mut worst_at = (0.0_f64, 0.0_f64);
        for lat_step in -8..=8 {
            for lon_step in 0..36 {
                let lat = f64::from(lat_step) * 10.0;
                let lon = f64::from(lon_step) * 10.0 - 180.0;
                let Some(reference) = globe.try_lon_lat_to_world(lon, lat) else {
                    continue;
                };
                let Some(local) = radar.try_lon_lat_to_world(lon, lat) else {
                    continue;
                };
                let Some(warped) = warp_world(local, 1.0) else {
                    continue;
                };
                let error = (warped.east_km - reference.east_km)
                    .hypot(warped.north_km - reference.north_km);
                if error > worst_km {
                    worst_km = error;
                    worst_at = (lon, lat);
                }
            }
        }
        // The whole globe is on screen only at 25 km/point or coarser, where a
        // kilometre is 0.04 of a screen point.
        let worst_points = worst_km / 25.0;
        assert!(
            worst_points < 1.0,
            "sphere-vs-ellipsoid disagreement peaked at {worst_km:.2} km \
             ({worst_points:.3} screen points) near lon {:.0} lat {:.0}",
            worst_at.0,
            worst_at.1
        );
    }

    #[test]
    fn snyders_orthographic_round_trips_on_the_near_hemisphere() {
        let globe = GlobeProjection::new(35.3330, -97.2777);
        for lat_step in -8..=8 {
            for lon_step in 0..36 {
                let lat = f64::from(lat_step) * 10.0;
                let lon = f64::from(lon_step) * 10.0 - 180.0;
                let Some(world) = globe.try_lon_lat_to_world(lon, lat) else {
                    continue;
                };
                let (back_lon, back_lat) = globe.world_to_lon_lat(world).expect("inside the disc");
                // Compare on the sphere, so a longitude near the pole is not
                // judged by its own degrees.
                let there = GlobeProjection::new(lat, lon);
                assert!(
                    there.cos_angular_distance(back_lon, back_lat) > 1.0 - 1e-9,
                    "lon {lon} lat {lat} came back as {back_lon} {back_lat}"
                );
            }
        }
    }

    #[test]
    fn the_visibility_test_is_snyders_facing_condition() {
        let globe = GlobeProjection::new(0.0, 0.0);
        assert!(globe.is_visible(0.0, 0.0), "the centre faces the viewer");
        assert!(globe.is_visible(89.9, 0.0), "just inside the limb");
        assert!(!globe.is_visible(90.1, 0.0), "just past the limb");
        assert!(!globe.is_visible(180.0, 0.0), "the antipode is behind");
    }

    /// The load-bearing claim about the radar raster: `render2d` is not
    /// changed, and the echo it draws cannot disagree with the globe under it
    /// by as much as a tenth of a screen point, on any pane.
    #[test]
    fn the_radar_footprint_error_stays_under_a_tenth_of_a_screen_point() {
        let mut worst = 0.0_f64;
        let mut worst_scale = 0.0_f32;
        for step in 0..=1_000 {
            let scale = MIN_BLEND_KM_PER_POINT
                + (analyst_runtime::MAX_KM_PER_POINT - MIN_BLEND_KM_PER_POINT)
                    * (step as f32 / 1_000.0);
            let error = radar_footprint_error_points(scale);
            if error > worst {
                worst = error;
                worst_scale = scale;
            }
            // The bound has to hold against the blend every pane in the table
            // actually produces, not only against the worst case the bound is
            // computed from.
            for (name, width, height) in PANES {
                let blend = blend_for_pane(scale, pane(*width, *height));
                let c = RADAR_FOOTPRINT_KM / EARTH_MEAN_RADIUS_KM;
                let actual =
                    RADAR_FOOTPRINT_KM * (1.0 - radial_factor(c, blend)) / f64::from(scale);
                assert!(
                    actual <= error + 1e-12,
                    "{name} at {scale} km/point moves the echo {actual} points, past the \
                     {error} point bound"
                );
            }
        }
        assert!(
            worst < 0.1,
            "worst footprint error was {worst:.4} points at {worst_scale} km/point"
        );
        assert_eq!(
            radar_footprint_error_points(analyst_runtime::DEFAULT_KM_PER_POINT),
            0.0,
            "the blend is inert at analysis zoom"
        );
    }

    /// The limb fade hides the far hemisphere and nothing else.
    ///
    /// Two halves. Behind the limb it is a hard zero, which is what stops the
    /// clamped far side drawing a ring around the globe. In front of it the
    /// band is thinner than a screen point at every scale a pane can reach it
    /// at, which is what says no visible map was traded away to get that.
    #[test]
    fn the_limb_fade_hides_the_far_side_without_eating_the_near_side() {
        for blend in [0.55_f32, 0.7, 0.9, 1.0] {
            let horizon = horizon_angle_rad(blend);
            assert_eq!(
                limb_fade(horizon, blend),
                0.0,
                "the limb itself must be out"
            );
            assert_eq!(
                limb_fade(horizon + 1.0, blend),
                0.0,
                "behind the limb must be out"
            );
            assert!(
                (limb_fade(horizon - LIMB_FADE_RAD, blend) - 1.0).abs() < 1e-9,
                "the band must be exactly this wide"
            );
            assert_eq!(limb_fade(0.0, blend), 1.0, "the anchor is never faded");
            // Monotone across the band, so the edge cannot band or reverse.
            let mut previous = 1.0;
            for step in 0..=200 {
                let c = horizon * f64::from(step) / 200.0;
                let fade = limb_fade(c, blend);
                assert!(fade <= previous + 1e-12, "fade rose again at c = {c}");
                previous = fade;
            }
        }

        // And it costs less than a screen point of map at every state a pane
        // can actually be in. Sweeping blend against scale independently would
        // be sweeping states that cannot happen - a blend of 0.9 only exists
        // at 13 km/point and coarser, and asking what it would cost at 7 is
        // asking about a pane that does not exist.
        let mut worst = (0.0_f64, String::new(), 0.0_f32);
        for (name, width, height) in PANES {
            let viewport = pane(*width, *height);
            for step in 0..=500 {
                let scale = MIN_BLEND_KM_PER_POINT
                    + (analyst_runtime::MAX_KM_PER_POINT - MIN_BLEND_KM_PER_POINT)
                        * (step as f32 / 500.0);
                let points = limb_fade_width_points(blend_for_pane(scale, viewport), scale);
                if points > worst.0 {
                    worst = (points, (*name).to_owned(), scale);
                }
            }
        }
        assert!(
            worst.0 < 1.0,
            "the fade eats {:.3} screen points of map on {} at {} km/point",
            worst.0,
            worst.1,
            worst.2
        );
        // Below the fold there is no limb at all, so nothing is faded.
        for blend in [0.0_f32, 0.25, 0.5] {
            assert_eq!(
                limb_fade(ANTIPODAL_RADIUS_KM / EARTH_MEAN_RADIUS_KM, blend),
                1.0
            );
            assert_eq!(limb_fade_width_points(blend, 20.0), 0.0);
        }
    }

    /// The fade and the point cull have to agree, or a site marker sits on a
    /// coastline that has faded out from under it.
    #[test]
    fn the_fade_reaches_zero_exactly_where_warp_world_starts_culling() {
        // From just past 0.5, where a limb first exists at all. At exactly
        // 0.5 the horizon is pi and there is nothing to hide.
        for step in 1..=20 {
            let blend = 0.5 + step as f32 / 40.0;
            let horizon_km = horizon_radius_km(blend);
            let inside = WorldPoint::new(horizon_km - 1.0, 0.0);
            let outside = WorldPoint::new(horizon_km + 1.0, 0.0);
            assert!(warp_world(inside, blend).is_some(), "blend {blend}");
            assert!(warp_world(outside, blend).is_none(), "blend {blend}");
            // Not `== 0.0`: `horizon_km / R` is the angle back through a
            // multiply and a divide, so it lands an attosecond of arc inside
            // the horizon. The shader has no such round trip - it clamps the
            // angle it already has - so its far side is a hard zero.
            assert!(
                limb_fade(horizon_km / EARTH_MEAN_RADIUS_KM, blend) < 1e-9,
                "blend {blend}"
            );
            assert_eq!(
                limb_fade(horizon_angle_rad(blend), blend),
                0.0,
                "blend {blend}"
            );
        }
    }

    #[test]
    fn non_finite_input_cannot_poison_a_vertex_buffer() {
        for bad in [
            WorldPoint::new(f64::NAN, 0.0),
            WorldPoint::new(0.0, f64::INFINITY),
        ] {
            assert!(warp_world(bad, 1.0).is_none());
            assert!(unwarp_world(bad, 1.0).is_none());
        }
        let viewport = pane(1_600.0, 900.0);
        assert_eq!(blend_for_pane(f32::NAN, viewport), 0.0);
        assert_eq!(blend_for_pane(-1.0, viewport), 0.0);
        assert_eq!(blend_for_pane(f32::INFINITY, viewport), 0.0);
        // A pane that has not been laid out yet must not decide the map is a
        // globe. `ViewportMetrics::sanitized` is what stops it.
        let broken = ViewportMetrics {
            width_points: f32::NAN,
            height_points: 0.0,
            pixels_per_point: 0.0,
        };
        assert!(full_globe_scale(broken).is_finite());
        assert!((0.0..=1.0).contains(&blend_for_pane(20.0, broken)));
    }

    /// The poles, the dateline and the antipode, which are where an azimuthal
    /// projection is usually caught out.
    ///
    /// The limb this module draws is a distance test in the ELLIPSOIDAL
    /// geodesic frame; Snyder's is a facing test on a SPHERE. They cannot
    /// agree exactly, and the honest thing is to measure how wide the band of
    /// disagreement is rather than to assert it away. Outside that band they
    /// must agree exactly, everywhere, including at the poles and across the
    /// seam at 180 degrees.
    #[test]
    fn the_limb_agrees_with_snyder_at_the_poles_and_across_the_dateline() {
        let anchors = [
            ("north pole", 89.999_f64, 0.0_f64),
            ("south pole", -89.999, 0.0),
            ("on the dateline", 0.0, 180.0),
            ("just west of the dateline", 12.5, -179.999),
            ("equator", 0.0, 0.0),
            ("KTLX", SITES[0].1, SITES[0].2),
        ];
        let mut worst_band_deg = 0.0_f64;
        for (anchor_name, anchor_lat, anchor_lon) in anchors {
            let radar = RadarProjection::new(anchor_lat, anchor_lon);
            let sphere = GlobeProjection::new(anchor_lat, anchor_lon);
            for lat_step in -90..=90 {
                for lon_step in -180..=180 {
                    let lat = f64::from(lat_step);
                    let lon = f64::from(lon_step);
                    let visible_on_sphere = sphere.is_visible(lon, lat);
                    let Some(world) = radar.try_lon_lat_to_world(lon, lat) else {
                        // Vincenty gave up: that only happens within a
                        // whisker of the antipode, which is behind the limb
                        // on any reading.
                        assert!(
                            !visible_on_sphere,
                            "{anchor_name}: the geodesic failed at a point Snyder calls visible"
                        );
                        continue;
                    };
                    let drawn = warp_world(world, 1.0).is_some();
                    if drawn == visible_on_sphere {
                        continue;
                    }
                    // Disagreement is only tolerable within the sphere-vs-
                    // ellipsoid band around the terminator. Measure how far
                    // from 90 degrees this point actually is.
                    let angle_deg = sphere
                        .cos_angular_distance(lon, lat)
                        .clamp(-1.0, 1.0)
                        .acos()
                        .to_degrees();
                    let band = (angle_deg - 90.0).abs();
                    assert!(
                        band < 0.5,
                        "{anchor_name}: lon {lon} lat {lat} is {band:.3} degrees from the \
                         terminator and the two tests still disagreed"
                    );
                    worst_band_deg = worst_band_deg.max(band);
                }
            }
        }
        // The band is the price of bending the geodesic frame rather than
        // reprojecting. 0.2 degrees of arc is 22 km, and it only ever moves
        // the moment a feature vanishes at the limb - never where it is
        // drawn.
        assert!(
            worst_band_deg < 0.5,
            "the two limb tests disagreed {worst_band_deg:.3} degrees from the terminator"
        );
    }

    /// The exact handoff scales, where a hard 0 and a hard 1 have to be bit
    /// patterns rather than nearly-right floats.
    #[test]
    fn the_handoff_endpoints_are_exact() {
        for (name, width, height) in PANES {
            let viewport = pane(*width, *height);
            let full = full_globe_scale(viewport);
            let start = (full * 0.5).max(MIN_BLEND_KM_PER_POINT);
            assert_eq!(
                blend_for_pane(start, viewport).to_bits(),
                0.0_f32.to_bits(),
                "{name}: the first scale of the band is not exactly flat"
            );
            // No dead zone: every wheel notch strictly inside the band moves
            // the blend. (The last few ULPs at each end are flat, because
            // smoothstep's derivative vanishes there and f32 cannot hold the
            // difference - which is invisible by construction.)
            let notch = analyst_runtime::zoom_factor_for_notches(1.0);
            let mut scale = start * notch;
            while scale * notch < full {
                let before = blend_for_pane(scale, viewport);
                let after = blend_for_pane(scale * notch, viewport);
                assert!(
                    after > before,
                    "{name}: a notch at {scale} km/point did not move the blend"
                );
                scale *= notch;
            }
            assert!(blend_for_pane(full * 0.99, viewport) < 1.0, "{name}");
            assert_eq!(blend_for_pane(full, viewport).to_bits(), 1.0_f32.to_bits());
            // At the exact handoff the map is still continuous: the last
            // partial blend and the full one put a point in nearly the same
            // place.
            let world = WorldPoint::new(0.0, 5_000.0);
            let before =
                warp_world(world, blend_for_pane(full * 0.99, viewport)).expect("inside the limb");
            let after = warp_world(world, 1.0).expect("inside the limb");
            let moved_points = (before.north_km - after.north_km).abs() / f64::from(full);
            // Measured at 0.037 of a screen point on a 1600x900 pane: the
            // moment the blend saturates is not a step the eye can find.
            assert!(
                moved_points < 0.1,
                "{name}: the last step of the handoff moved a point {moved_points} points"
            );
        }
    }

    #[test]
    fn every_real_site_lands_on_the_visible_globe_from_its_own_anchor() {
        for (anchor_name, anchor_lat, anchor_lon) in SITES {
            let radar = RadarProjection::new(*anchor_lat, *anchor_lon);
            let globe = GlobeProjection::new(*anchor_lat, *anchor_lon);
            for (name, lat, lon) in SITES {
                let world = radar
                    .try_lon_lat_to_world(*lon, *lat)
                    .expect("a real site projects");
                let warped = warp_world(world, 1.0);
                assert_eq!(
                    warped.is_some(),
                    globe.is_visible(*lon, *lat),
                    "{name} from {anchor_name}: the radial cull and Snyder's \
                     facing test disagreed"
                );
            }
        }
    }
}
