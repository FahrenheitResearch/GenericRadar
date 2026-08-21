//! Radar-local map projection.
//!
//! `render2d` places a gate at plain polar coordinates: `x = range * sin(az)`,
//! `y = range * cos(az)`, with no earth model. Screen distance from the radar
//! is therefore ground distance, so the map has to answer the same question the
//! radar does — how far, and in what direction, is this point from the antenna
//! — using true geodesic distance on WGS84.
//!
//! That makes this an azimuthal-equidistant projection whose distances and
//! azimuths come from Vincenty's formulae. A spherical approximation was tried
//! first and rejected: no single radius fits both the meridional and
//! prime-vertical curvature, which left a degree of latitude 250 m long and
//! about a kilometre of drift at 460 km range. At analysis zoom that is several
//! pixels of misalignment on a county line.
//!
//! This projection is the NEAR half of a composite. Beyond the scales an
//! analyst works at, it is bent onto an orthographic globe by [`globe`]; see
//! that module for why, and for the proof that the bend is exactly nothing
//! until the camera is 20x coarser than the default. The TRANSFORM below is
//! the shipped one, unchanged, and is not scale dependent. The north-up
//! CAMERA RULE further down is, and the contract it is stated on is next.
//!
//! # THE NORTH-UP DOMAIN, as a contract
//!
//! [`RadarProjection::view_rotation_rad`] derives a camera rotation that puts
//! true north up at the middle of the pane. It is applied ONLY inside the
//! region named here, it is exactly `0.0` - a bit pattern, by early return -
//! everywhere else, and it is ramped to zero at every edge so that crossing
//! one is continuous.
//!
//! A view is inside the domain when ALL FOUR of these hold:
//!
//! 1. THE CAMERA SCALE IS FINER THAN [`NORTH_UP_ZERO_KM_PER_POINT`] (7 km per
//!    point), with full effect at or below [`NORTH_UP_FULL_KM_PER_POINT`]
//!    (5 km per point) and a smoothstep between them. 7 km per point IS
//!    [`globe::MIN_BLEND_KM_PER_POINT`], the scale below which
//!    [`globe::blend_for_pane`] returns an exact `0.0` on EVERY pane at EVERY
//!    window size, so the rule cannot reach a blended globe at all: the globe
//!    band, its limb, and everything past it are outside the domain by
//!    construction rather than by a fade that has to be tuned.
//! 2. THE MIDDLE OF THE PANE IS FURTHER OUT THAN
//!    [`analyst_runtime::NEXRAD_SURVEILLANCE_RANGE_KM`] (460 km), ramping in
//!    over one further surveillance range. Inside it the analyst is working
//!    the storm and nothing may move.
//! 3. THE MIDDLE OF THE PANE IS CLOSER IN THAN [`NORTH_UP_ZERO_RANGE_KM`]
//!    (6500 km), with full effect out to [`NORTH_UP_FULL_RANGE_KM`] (5000 km)
//!    and a smoothstep between them.
//! 4. IT IS NOT PRESSED AGAINST A POLE and north is not drawn nearly straight
//!    down - [`POLAR_HOLD_KM`] and [`NORTH_UP_FULL_DEG`], both of which
//!    predate this contract and both of which are ramps.
//!
//! ## Why a domain and not a wider guard
//!
//! The rate at which panning turns the map is genuinely unbounded in the
//! limit. It grows without bound at a pole (there is no north to put up), at
//! the antipode (the transverse stretch `c / sin c` diverges) and against the
//! globe's limb (the morph's Jacobian goes to zero, so one screen point of
//! pan covers unbounded ground). TWO ROUNDS of trying to bound it by sweeping
//! a grid of view centres failed the same way: each grid missed a region, and
//! the "bound" was only the worst of a sample. No sample can bound a quantity
//! that diverges.
//!
//! So the rule is confined to a region where the bound is an ARGUMENT. The
//! complaint this feature answers is about the CONTINENTAL view - a regional
//! or CONUS-scale look out from a radar - and every pathology either review
//! found lives somewhere that view never goes: 14 800 km downrange, 12 km per
//! point against a half-formed globe's limb, a view centre where the north
//! bearing wraps a whole turn. Outside the domain the map behaves exactly as
//! it did before this feature existed, which costs the analyst nothing they
//! asked for and deletes the entire failure class.
//!
//! ## The domain does the job it was chosen for
//!
//! The whole of the contiguous United States is about 4500 km across and
//! 2500 km tall, so from ANY radar in it every CONUS view centre is inside
//! 5000 km - the furthest CONUS-to-CONUS geodesic is about 4600 km, from the
//! Washington border to Key West - and the whole country fits on a
//! 1600x900-point pane at 2.81 km per point. Both are inside the full-effect
//! region with room to spare. The outlying territories are smaller still:
//! the Hawaiian chain is 700 km, the Marianas about 1000, the Caribbean from
//! a Puerto Rico radar about 2000, and Alaska about 2500 km across.
//!
//! The one shape that lands in the scale ramp rather than under it is the
//! whole of CONUS in a QUARTER pane - 800x450 points needs 5.63 km per point,
//! where the fade is 0.79 - and that is deliberate. The band cannot start
//! higher without becoming narrower than one wheel notch (`ZOOM_PER_NOTCH` is
//! 1.2, and the band is a factor of 1.4), and a band narrower than a notch
//! would unwind the whole rotation in a single detent.

use analyst_runtime::{NEXRAD_SURVEILLANCE_RANGE_KM, WorldPoint};

/// The far-zoom orthographic globe.
///
/// Declared here rather than in `lib.rs` because it is a projection: it is the
/// second half of the composite this module's transform is the near half of,
/// and it is meaningless without [`RadarProjection`] to bend. Reachable as
/// `map_scene::projection::globe`.
#[path = "globe.rs"]
pub mod globe;

/// Bump when the transform itself changes. It is part of projection identity,
/// so a change invalidates retained geometry built by an older algorithm.
pub const PROJECTION_ALGORITHM_VERSION: u16 = 2;

/// WGS84 semi-major axis, metres.
const WGS84_A_M: f64 = 6_378_137.0;
/// WGS84 flattening.
const WGS84_F: f64 = 1.0 / 298.257_223_563;
/// WGS84 semi-minor axis, metres.
const WGS84_B_M: f64 = WGS84_A_M * (1.0 - WGS84_F);

/// Vincenty converges in a handful of steps at radar ranges. The cap guards
/// the near-antipodal case, where the iteration is known not to converge
/// (Vincenty 1975, p. 92).
///
/// This comment used to claim that case "is culled long before it is drawn".
/// That was false once [`crate::build::MAX_BUILD_HALF_EXTENT_KM`] reached
/// 20 000 km: the build region then covered the whole earth, so near-antipodal
/// points really were offered to this function. What saved it was
/// [`Self::try_lon_lat_to_world`] returning `None` rather than a non-converged
/// answer, and the callers honouring it.
///
/// Measured on the shipped basemap from KTLX: 251 771 line vertices, ZERO
/// non-convergent, furthest vertex 18 181.5 km (163.5 degrees). The iteration
/// only gives up inside roughly a tenth of a degree of the antipode - about
/// 19 990 km - which for a radar in the contiguous United States is the middle
/// of the southern Indian Ocean, where the dataset has nothing. So the cap has
/// never fired in practice; it is still load bearing, because a radar anywhere
/// else would reach land there.
const MAX_ITERATIONS: usize = 32;
const CONVERGENCE: f64 = 1e-12;

/// Colatitude, as a meridian arc in kilometres, inside which
/// [`RadarProjection::view_rotation_rad`] holds the map still, and the width of
/// the ramp it fades in over.
///
/// "North up" is a request about a direction, and at a pole there is no such
/// direction: every way is south, and the screen bearing of true north turns
/// through a full circle around any loop enclosing the pole. That is not a
/// numerical difficulty to be smoothed away, it is the statement that the
/// question has no answer there, so the rule answers by not turning the map.
///
/// 500 km of hold is about four and a half degrees of latitude, and the 1500 km
/// ramp puts full correction back at 2000 km. The highest-latitude row in the
/// shipped table - `nearest_site::REAL_STATION_CATALOG`, transcribed from
/// api.weather.gov/radar/stations - is PAPD at 65.0351 N, whose own gap to the
/// pole is 2782 km, so full correction is restored well inside it and that
/// radar's continental view is corrected in full. A synthetic probe at Barrow
/// (71.2854 N, 2080 km to the pole) is swept as well, because it is the
/// harshest case a WSR-88D site could be at even though no such row ships; it
/// too is inside the 2000 km recovery.
const POLAR_HOLD_KM: f64 = 500.0;
const POLAR_RAMP_KM: f64 = 1500.0;

/// Colatitude in degrees converted to kilometres through the WGS84 POLAR
/// radius of curvature, `c = a^2 / b = 6399.594 km` (NIMA TR8350.2, WGS84
/// derived constants; Snyder 1987 p. 24 lists the same quantity).
///
/// Exact at the pole, which is the end of the interval this measures, and 0.3
/// per cent long by 60 degrees of latitude. The gate it feeds is a 1500 km
/// smoothstep, so a third of a per cent is far inside the softness of the
/// thing it is used for: this is a fence, not a measurement.
const POLAR_RADIUS_OF_CURVATURE_KM: f64 = 6399.594;

/// Where the rule stops asking for north up, in degrees of `gamma` - the
/// screen bearing true north is currently drawn at.
///
/// TWO reasons, and the second one is the load-bearing one.
///
/// 1. A view whose centre has north drawn more than half a turn's worth away
///    from screen-up is a view looking past the pole at the far side of the
///    world. Turning it right way up is a defensible thing to want and a
///    strange thing to do to somebody who dragged there.
/// 2. THE RULE IS NOT CONTINUOUS WITHOUT IT. `gamma` is an angle, and its
///    principal value jumps by a full turn across the ray where north is drawn
///    straight down. As a ROTATION that jump is nothing - a turn of `+pi` and
///    a turn of `-pi` are the same picture - but the rule multiplies `gamma`
///    by a fraction (the surveillance ramp, the globe hand-back, the polar
///    hold above), and a fraction of `+pi` is not a fraction of `-pi`. At a
///    half-formed globe the shipped rule therefore flipped the map by up to
///    `2 pi (1 - blend)` - 173 degrees measured at a blend of 0.52 - when the
///    middle of the pane crossed that ray, which for a Fairbanks or Barrow
///    radar is one drag away. Fading the rule out before the ray reaches it is
///    what makes the product continuous, because the jump is then a jump in
///    something that has been multiplied by zero.
///
/// This is topology and not an implementation detail: `gamma` winds once
/// around any loop enclosing a pole, so NO continuous branch of it exists
/// there, and no partial application of it can be continuous unless it is
/// faded to zero across the cut.
const NORTH_UP_FULL_DEG: f64 = 90.0;
const NORTH_UP_ZERO_DEG: f64 = 150.0;

/// The domain's SCALE edge: full effect at or below
/// [`NORTH_UP_FULL_KM_PER_POINT`], exactly zero at or above
/// [`NORTH_UP_ZERO_KM_PER_POINT`], smoothstep between.
///
/// The zero edge IS [`globe::MIN_BLEND_KM_PER_POINT`], and that identity is
/// the whole point rather than a coincidence: `globe::blend_for_pane` opens
/// with `km_per_point <= MIN_BLEND_KM_PER_POINT => return 0.0`, so a nonzero
/// rotation implies an exactly zero blend on every pane at every window size.
/// The globe band, the limb where its Jacobian vanishes, and the formed globe
/// beyond are therefore outside the domain BY CONSTRUCTION - the rule never
/// meets a warped camera centre, never calls `globe::unwarp_world`, and has no
/// limb fade to tune. The static assertion below is what keeps that true if
/// either constant is ever edited.
///
/// The band is a factor of 1.4, which is 1.85 wheel notches - wider than one
/// detent on purpose, so that no single notch can unwind the whole rotation,
/// and no wider, so that the continental scales the feature exists for sit
/// under it rather than in it.
pub const NORTH_UP_FULL_KM_PER_POINT: f32 = 5.0;
pub const NORTH_UP_ZERO_KM_PER_POINT: f32 = globe::MIN_BLEND_KM_PER_POINT;

const _: () = assert!(NORTH_UP_ZERO_KM_PER_POINT <= globe::MIN_BLEND_KM_PER_POINT);
const _: () = assert!(NORTH_UP_FULL_KM_PER_POINT < NORTH_UP_ZERO_KM_PER_POINT);

/// The domain's DOWNRANGE edge: full effect out to
/// [`NORTH_UP_FULL_RANGE_KM`], exactly zero at or beyond
/// [`NORTH_UP_ZERO_RANGE_KM`], smoothstep between.
///
/// 5000 km covers every CONUS view centre from every CONUS radar - the
/// furthest CONUS-to-CONUS geodesic is about 4600 km - and a comparable
/// regional view from every outlying territory, all of which are far smaller.
/// Past it the view centre is on another continent from its own antenna, the
/// projection's transverse stretch `c / sin c` has begun to grow in earnest,
/// and "north up at the middle of the pane" has stopped being a statement
/// about the radar's own picture.
///
/// The ramp is 1500 km wide, matching [`POLAR_RAMP_KM`], so its slope
/// contributes the same `1.5 / 1500` per kilometre to the turn-rate bound
/// argument on [`RadarProjection::view_rotation_rad`].
pub const NORTH_UP_FULL_RANGE_KM: f64 = 5_000.0;
pub const NORTH_UP_ZERO_RANGE_KM: f64 = 6_500.0;

const _: () = assert!(NORTH_UP_FULL_RANGE_KM < NORTH_UP_ZERO_RANGE_KM);

/// CEILING on the rate at which panning turns the map, in degrees per
/// kilometre the middle of the pane moves.
///
/// **This is an argument, not a sample.** The analytic bound derived in the
/// "How fast the map can turn" section of
/// [`RadarProjection::view_rotation_rad`] is 0.61 degrees per kilometre
/// everywhere in the domain; this constant is that ceiling with margin over
/// it. Sampling CORROBORATES it - `workstation_app::north_up::tests::
/// the_pan_turn_rate_stays_under_its_documented_bound` sweeps the whole
/// shipped station table and prints its own measured worst, which is a third
/// of this - but the sampling is not what makes it true. Two earlier rounds
/// pinned the worst of a grid and were falsified by the next grid.
///
/// At the domain's own scale ceiling of 7 km per point this is 5.25 degrees
/// per screen point of drag.
pub const MAX_TURN_RATE_DEG_PER_KM: f64 = 0.75;

/// CEILING on the rotation itself, in degrees, anywhere at all.
///
/// The rule applies `gamma * half_turn_fade(gamma)` and never more, so the
/// largest rotation it can produce is the maximum of that product over
/// `gamma`, which sits at 100.6 degrees of convergence and is 93.352 degrees.
/// Everything else the rule multiplies in lies in `0..=1`.
///
/// This is what makes cost 5 on [`RadarProjection::view_rotation_rad`] - what
/// a spun wheel can do in one frame - a bounded claim rather than a measured
/// one: no event of any kind can turn the map by more than twice this, since
/// the rotation is inside `+/- 93.36` degrees before it and after it.
pub const MAX_ROTATION_DEG: f64 = 93.36;

/// Identity of a projection, for the projection half of a `GeometryCacheKey`.
/// The anchor is quantised to 1e-7 degrees (about 11 mm) so the identity is
/// hashable and exact; it is provenance, never a camera cache key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProjectionId {
    pub anchor_lat_e7: i32,
    pub anchor_lon_e7: i32,
    pub algorithm_version: u16,
}

/// Geodesic azimuthal-equidistant projection anchored at a radar site.
#[derive(Clone, Copy, Debug)]
pub struct RadarProjection {
    radar_lat_deg: f64,
    radar_lon_deg: f64,
    lat1_rad: f64,
    /// Reduced latitude of the anchor, cached because every forward call needs
    /// it.
    sin_u1: f64,
    cos_u1: f64,
}

impl RadarProjection {
    pub fn new(radar_lat_deg: f64, radar_lon_deg: f64) -> Self {
        let lat = if radar_lat_deg.is_finite() {
            radar_lat_deg.clamp(-90.0, 90.0)
        } else {
            0.0
        };
        let lon = normalize_longitude(radar_lon_deg);
        let lat_rad = lat.to_radians();
        let u1 = ((1.0 - WGS84_F) * lat_rad.tan()).atan();
        Self {
            radar_lat_deg: lat,
            radar_lon_deg: lon,
            lat1_rad: lat_rad,
            sin_u1: u1.sin(),
            cos_u1: u1.cos(),
        }
    }

    pub fn radar_lat_deg(&self) -> f64 {
        self.radar_lat_deg
    }

    pub fn radar_lon_deg(&self) -> f64 {
        self.radar_lon_deg
    }

    pub fn id(&self) -> ProjectionId {
        ProjectionId {
            anchor_lat_e7: quantize_e7(self.radar_lat_deg),
            anchor_lon_e7: quantize_e7(self.radar_lon_deg),
            algorithm_version: PROJECTION_ALGORITHM_VERSION,
        }
    }

    /// Project geographic coordinates into radar-local world kilometres,
    /// or `None` where the geodesic does not converge (effectively antipodal
    /// points). Geometry builders must drop those rather than substitute a
    /// position, since any substitute draws a line to somewhere real.
    pub fn try_lon_lat_to_world(&self, lon_deg: f64, lat_deg: f64) -> Option<WorldPoint> {
        let (distance_m, azimuth_rad) = self.inverse_geodesic(lon_deg, lat_deg)?;
        let distance_km = distance_m / 1_000.0;
        Some(WorldPoint {
            east_km: distance_km * azimuth_rad.sin(),
            north_km: distance_km * azimuth_rad.cos(),
        })
    }

    /// Project geographic coordinates into radar-local world kilometres.
    ///
    /// Non-convergent points collapse to the origin. Use
    /// [`Self::try_lon_lat_to_world`] anywhere that distinction matters.
    pub fn lon_lat_to_world(&self, lon_deg: f64, lat_deg: f64) -> WorldPoint {
        self.try_lon_lat_to_world(lon_deg, lat_deg)
            .unwrap_or(WorldPoint::ORIGIN)
    }

    /// Project geographic coordinates onto the blended globe.
    ///
    /// `blend` is [`globe::blend_for_scale`] of the camera scale the result
    /// will be drawn at. At `blend == 0.0` this IS
    /// [`Self::try_lon_lat_to_world`] - the same call, not an equivalent one -
    /// so the radar-local view cannot be changed by anything the globe does.
    ///
    /// `None` means either that the geodesic did not converge or that the
    /// point is behind the limb. Both are "do not draw this", and callers must
    /// break the feature rather than substitute a position.
    pub fn try_lon_lat_to_globe(
        &self,
        lon_deg: f64,
        lat_deg: f64,
        blend: f32,
    ) -> Option<WorldPoint> {
        let world = self.try_lon_lat_to_world(lon_deg, lat_deg)?;
        if blend == 0.0 {
            return Some(world);
        }
        globe::warp_world(world, blend)
    }

    /// Inverse of [`Self::try_lon_lat_to_globe`], for the cursor readout when
    /// the pane is showing the globe.
    pub fn globe_to_lon_lat(&self, world: WorldPoint, blend: f32) -> Option<(f64, f64)> {
        let local = globe::unwarp_world(world, blend)?;
        Some(self.world_to_lon_lat(local))
    }

    /// Inverse of [`Self::lon_lat_to_world`], returning `(lon_deg, lat_deg)`.
    pub fn world_to_lon_lat(&self, world: WorldPoint) -> (f64, f64) {
        let distance_km = world.east_km.hypot(world.north_km);
        if distance_km < 1e-9 {
            return (self.radar_lon_deg, self.radar_lat_deg);
        }
        let azimuth_rad = world.east_km.atan2(world.north_km);
        self.direct_geodesic(distance_km * 1_000.0, azimuth_rad)
    }

    /// Vincenty inverse: geodesic distance in metres and initial azimuth in
    /// radians, measured clockwise from north at the anchor.
    fn inverse_geodesic(&self, lon_deg: f64, lat_deg: f64) -> Option<(f64, f64)> {
        if !lon_deg.is_finite() || !lat_deg.is_finite() {
            return None;
        }
        let lat2 = lat_deg.clamp(-90.0, 90.0).to_radians();
        let l = shortest_longitude_delta(lon_deg, self.radar_lon_deg).to_radians();

        let u2_reduced = ((1.0 - WGS84_F) * lat2.tan()).atan();
        let (sin_u2, cos_u2) = u2_reduced.sin_cos();
        let (sin_u1, cos_u1) = (self.sin_u1, self.cos_u1);

        let mut lambda = l;
        let mut sin_sigma = 0.0;
        let mut cos_sigma = 0.0;
        let mut sigma = 0.0;
        let mut cos_sq_alpha = 0.0;
        let mut cos_2sigma_m = 0.0;
        let mut sin_lambda = 0.0;
        let mut cos_lambda = 0.0;
        let mut converged = false;

        for _ in 0..MAX_ITERATIONS {
            let sin_cos = lambda.sin_cos();
            sin_lambda = sin_cos.0;
            cos_lambda = sin_cos.1;

            let term_a = cos_u2 * sin_lambda;
            let term_b = cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda;
            sin_sigma = term_a.hypot(term_b);
            if sin_sigma == 0.0 {
                // Coincident points.
                return Some((0.0, 0.0));
            }
            cos_sigma = sin_u1 * sin_u2 + cos_u1 * cos_u2 * cos_lambda;
            sigma = sin_sigma.atan2(cos_sigma);
            let sin_alpha = cos_u1 * cos_u2 * sin_lambda / sin_sigma;
            cos_sq_alpha = 1.0 - sin_alpha * sin_alpha;
            cos_2sigma_m = if cos_sq_alpha.abs() < f64::EPSILON {
                // Equatorial line: cos(2 sigma_m) is undefined, and zero is the
                // value that makes the series below reduce correctly.
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

    /// Vincenty direct: the point `distance_m` along `azimuth_rad` from the
    /// anchor, returned as `(lon_deg, lat_deg)`.
    fn direct_geodesic(&self, distance_m: f64, azimuth_rad: f64) -> (f64, f64) {
        let (sin_alpha1, cos_alpha1) = azimuth_rad.sin_cos();
        let tan_u1 = self.sin_u1 / self.cos_u1.max(f64::MIN_POSITIVE);
        let sigma1 = tan_u1.atan2(cos_alpha1);
        let sin_alpha = self.cos_u1 * sin_alpha1;
        let cos_sq_alpha = 1.0 - sin_alpha * sin_alpha;
        let u_sq = cos_sq_alpha * (WGS84_A_M * WGS84_A_M - WGS84_B_M * WGS84_B_M)
            / (WGS84_B_M * WGS84_B_M);
        let a_series =
            1.0 + u_sq / 16384.0 * (4096.0 + u_sq * (-768.0 + u_sq * (320.0 - 175.0 * u_sq)));
        let b_series = u_sq / 1024.0 * (256.0 + u_sq * (-128.0 + u_sq * (74.0 - 47.0 * u_sq)));

        let sigma_initial = distance_m / (WGS84_B_M * a_series);
        let mut sigma = sigma_initial;
        let mut cos_2sigma_m = 0.0;
        for _ in 0..MAX_ITERATIONS {
            cos_2sigma_m = (2.0 * sigma1 + sigma).cos();
            let (sin_sigma, cos_sigma) = sigma.sin_cos();
            let delta_sigma = b_series
                * sin_sigma
                * (cos_2sigma_m
                    + b_series / 4.0
                        * (cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)
                            - b_series / 6.0
                                * cos_2sigma_m
                                * (-3.0 + 4.0 * sin_sigma * sin_sigma)
                                * (-3.0 + 4.0 * cos_2sigma_m * cos_2sigma_m)));
            let previous = sigma;
            sigma = sigma_initial + delta_sigma;
            if (sigma - previous).abs() < CONVERGENCE {
                break;
            }
        }

        let (sin_sigma, cos_sigma) = sigma.sin_cos();
        let temp = self.sin_u1 * sin_sigma - self.cos_u1 * cos_sigma * cos_alpha1;
        let lat2 = (self.sin_u1 * cos_sigma + self.cos_u1 * sin_sigma * cos_alpha1)
            .atan2((1.0 - WGS84_F) * (sin_alpha * sin_alpha + temp * temp).sqrt());
        let lambda = (sin_sigma * sin_alpha1)
            .atan2(self.cos_u1 * cos_sigma - self.sin_u1 * sin_sigma * cos_alpha1);
        let c = WGS84_F / 16.0 * cos_sq_alpha * (4.0 + WGS84_F * (4.0 - 3.0 * cos_sq_alpha));
        let l = lambda
            - (1.0 - c)
                * WGS84_F
                * sin_alpha
                * (sigma
                    + c * sin_sigma
                        * (cos_2sigma_m
                            + c * cos_sigma * (-1.0 + 2.0 * cos_2sigma_m * cos_2sigma_m)));

        (
            normalize_longitude(self.radar_lon_deg + l.to_degrees()),
            lat2.to_degrees().clamp(-90.0, 90.0),
        )
    }

    /// Anchor latitude in radians, for callers needing the raw value.
    pub fn anchor_lat_rad(&self) -> f64 {
        self.lat1_rad
    }

    /// Screen bearing of TRUE NORTH at `world`, radians clockwise from
    /// screen-up, on an unrotated camera.
    ///
    /// Measured by finite difference through THIS projection's own forward and
    /// inverse pair, so it cannot drift away from the map it describes: it
    /// literally asks the map where north is rather than predicting where it
    /// ought to be. `None` where the geodesic does not converge; callers apply
    /// no rotation there, which is this file's drop-it-never-substitute rule.
    ///
    /// The closed form would be `az1 + atan2(-k sin az2, cos az2)` with
    /// `k = c / sin c` the transverse scale factor of the azimuthal
    /// equidistant projection (Snyder 1987, section 25, pp. 191-202: radial
    /// `h = 1`, transverse `k = c / sin c`). The two agree: anchored at KRTX
    /// and asked about 40N 75W, the finite difference gives -32.3280 degrees
    /// and the closed form -32.3249, an eleven-arcsecond disagreement that is
    /// the second-order term the difference quotient carries. The pure
    /// azimuth convergence `az1 - az2`, which ignores the stretch entirely,
    /// gives -33.7136 - and it is the finite-difference number that the eye
    /// sees, because the eye is looking at the drawn meridian.
    pub fn north_bearing_rad(&self, world: WorldPoint) -> Option<f64> {
        let (lon, lat) = self.world_to_lon_lat(world);
        // About 1.1 m of ground. At 1e-4 degrees the answer is the same to
        // 1e-3 degrees, so the choice is not delicate - but it is pinned here
        // rather than passed in, because two callers using two steps would be
        // two different maps.
        const STEP_DEG: f64 = 1e-5;
        let (near, far) = if lat + STEP_DEG > 90.0 {
            (lat - STEP_DEG, lat)
        } else {
            (lat, lat + STEP_DEG)
        };
        // BOTH endpoints go through the forward transform. Using the incoming
        // `world` as one of them would mix this projection's rounding with
        // whatever arithmetic produced the caller's point.
        let a = self.try_lon_lat_to_world(lon, near)?;
        let b = self.try_lon_lat_to_world(lon, far)?;
        let east = b.east_km - a.east_km;
        let north = b.north_km - a.north_km;
        if east == 0.0 && north == 0.0 {
            return None;
        }
        Some(east.atan2(north))
    }

    /// The camera rotation that puts north up at the middle of the pane, in
    /// radians clockwise, or exactly `0.0` where the map must not move.
    ///
    /// # The problem this answers
    ///
    /// This is an AZIMUTHAL projection about the antenna, and in any azimuthal
    /// projection grid north coincides with screen-up only along the meridian
    /// through the anchor. Everywhere else the meridians converge. Anchored at
    /// a Portland radar (about 122.7W) and looking at the eastern United
    /// States (about 75W), true north is drawn 32.3 degrees off screen-up and
    /// the whole east coast reads as swung off its axis. Nothing has rotated -
    /// the map is simply the map - and re-centring only helps by returning to
    /// the anchor.
    ///
    /// A rotation cancels a uniform tilt exactly and cannot unbend a fan, so
    /// it is anchored on the VIEW CENTRE, which gets both cases right for
    /// free: a pane centred on the antenna sees a symmetric fan whose centre
    /// convergence is nil, and the rule correctly does nothing; a pane that
    /// has left the anchor sees a near-uniform tilt, and the rule cancels it.
    ///
    /// # Why the residual at continental zoom is not a defect
    ///
    /// An azimuthal-equidistant projection re-centred on the view centre
    /// reproduces the grid convergence of a Lambert conformal conic on the
    /// same central meridian to within 2.13 degrees across the whole
    /// contiguous United States (Snyder 1987, section 15, pp. 104-110;
    /// convergence `theta = n (lambda - lambda0)`, Snyder eq. 14-4). Cancelling
    /// the convergence AT THE CENTRE, which is what this rotation does, is the
    /// same first-order operation. So the residual fan of meridians that
    /// survives at whole-continent scale is what a correctly parameterised
    /// continental map looks like, and it is not a number to chase to zero.
    ///
    /// A conic on FIXED continental parameters was measured and rejected: with
    /// the standard 33/45N, 96W cone (`n = 0.630478`) a Portland radar sits
    /// 17.0 degrees off north on its own antenna and the frame swings 34
    /// degrees across the country - the crookedness redistributed, not
    /// removed - and it has no parameters to offer Puerto Rico, Hawaii,
    /// Okinawa or Alaska, which the site tables here treat as first-class.
    ///
    /// # The domain
    ///
    /// The contract is stated in full in this module's own documentation, and
    /// this method is where it is enforced. Outside the domain the answer is
    /// exactly `0.0`, by an early return on a bit pattern and never by
    /// arithmetic that happens to evaluate to the identity - the same
    /// discipline [`globe::warp_world`] uses at `blend == 0.0`. Every edge of
    /// it is a smoothstep, so crossing one is continuous and there is nothing
    /// to snap.
    ///
    /// `km_per_point` is the camera scale the pane is being drawn at, and it
    /// is the whole of what the rule needs to know about the globe: below
    /// [`NORTH_UP_ZERO_KM_PER_POINT`] the blend is an exact zero on every
    /// pane, so the centre handed in is a GROUND point and there is no warped
    /// frame to be in the wrong one of.
    ///
    /// # The floor
    ///
    /// Exactly `0.0` whenever the middle of the pane is inside
    /// [`analyst_runtime::NEXRAD_SURVEILLANCE_RANGE_KM`]. That is the same 460
    /// km, with the same Crum and Alberty (1993) citation, that
    /// `WorkspaceState` already uses to decide whether a camera is still about
    /// this radar. Inside it the analyst is working the storm, and nothing may
    /// move.
    ///
    /// The floor is a DISTANCE and not a convergence angle on purpose: a gate
    /// on `|gamma|` has no floor at high latitude, because convergence is
    /// `delta-lambda * sin(phi)` to first order and reaches 7.5 degrees inside
    /// an Anchorage radar's own footprint and 12.3 inside Barrow's. It is also
    /// not a camera-SCALE gate, because a rotation is an isometry and is
    /// therefore exact at every scale - so there is no reason to refuse it at
    /// the regional scales where the complaint actually lives.
    ///
    /// Beyond the floor it ramps in over one further surveillance range, 460
    /// to 920 km, on Perlin's smoothstep `3x^2 - 2x^3` (Perlin 1985), whose
    /// first derivative vanishes at both ends so the turn starts and stops
    /// without a kink. Widening that band was measured and rejected: at three
    /// times the width the worst turn-per-drag falls only from 6.60 to 4.99
    /// degrees per 100 points while the correction at a 1200 km centre falls
    /// from 100 to 57 per cent, because the residual rate is intrinsic to the
    /// convergence and not to the ramp.
    ///
    /// The scale fade hands the map back, un-rotated, to [`globe`] BEFORE the
    /// globe starts forming, so every blended and fully formed globe is byte
    /// for byte the globe that shipped.
    ///
    /// # The other places it holds still, and why they are not the floor
    ///
    /// The 460 km floor is about the analyst. These are about the map, and
    /// each of them is one sentence of geometry: "put north up" is a question
    /// that stops having an answer, and where a question has no answer the
    /// honest reply is to leave the map alone.
    ///
    /// * NEAR A POLE, where there is no north to put up and the screen bearing
    ///   of north winds through a full turn around any enclosing loop. Held
    ///   still inside [`POLAR_HOLD_KM`] of one and ramped back over
    ///   [`POLAR_RAMP_KM`]. This is also what BOUNDS the pan-turn rate near a
    ///   pole: without it that rate grows like the reciprocal of the distance
    ///   to the pole and there is no true bound to state.
    /// * WHERE NORTH IS DRAWN NEARLY STRAIGHT DOWN, past
    ///   [`NORTH_UP_FULL_DEG`]. That ray is where the principal value of the
    ///   bearing wraps, and any FRACTION of it - which is what the ramps and
    ///   the polar hold all produce - jumps by a whole turn's worth across it.
    ///   The shipped rule flipped the map by 173 degrees there. See
    ///   [`NORTH_UP_FULL_DEG`].
    /// * FAR DOWNRANGE, past [`NORTH_UP_FULL_RANGE_KM`], where the view centre
    ///   is on another continent from its own antenna. Faded out by
    ///   [`NORTH_UP_ZERO_RANGE_KM`], which also keeps the transverse stretch
    ///   `c / sin c` under 1.20 and the antipodal singularity out of reach.
    /// * AT GLOBE SCALE, past [`NORTH_UP_FULL_KM_PER_POINT`]. Faded out by
    ///   [`NORTH_UP_ZERO_KM_PER_POINT`], which is exactly where the globe
    ///   blend starts, so the limb - where the morph's Jacobian goes to zero
    ///   and one screen point of pan covers unbounded ground - is somewhere
    ///   this rule can no longer be asked about.
    ///
    /// All of them are ramps and not gates, so none of them adds a threshold
    /// to the ones this rule already had; and each of them multiplies in, so
    /// the rule is one product and not a ladder of special cases.
    ///
    /// # How fast the map can turn, by argument
    ///
    /// Write the rule as `rho = -g(gamma) * N(r) * P(theta) * F(r) * S(s)`,
    /// with `g(gamma) = gamma * half_turn_fade(gamma)`, `N` the 460-920 km
    /// ramp in, `P` the polar hold, `F` the downrange fade and `S` the scale
    /// fade. A pan does not change `s`, so `S` is a constant `<= 1` under one,
    /// and every other factor is in `0..=1`. By the product rule,
    ///
    /// ```text
    ///   |grad rho| <= |g'(gamma)| |grad gamma| P + |g| (|N'| + |P'| + |F'|)
    /// ```
    ///
    /// and each piece is bounded on the domain and only on the domain.
    ///
    /// GAMMA'S GRADIENT. On a sphere of radius `R`, in geodesic polar
    /// coordinates `(c, alpha)` about the anchor, the projection sends a
    /// ground direction at angle `beta` from the outward radial to map angle
    /// `beta' = atan2(k sin beta, cos beta)` with `k = c / sin c`, because the
    /// radial direction is preserved and the transverse one is stretched by
    /// `k`. Grid north is therefore drawn at `gamma = alpha + beta'`, and with
    /// `dbeta = sin(phi) dlambda - cos(c) dalpha` (the connection forms of the
    /// geographic and the geodesic-polar frames, do Carmo 1976 section 4-4),
    ///
    /// ```text
    ///   dgamma = (1 - p cos c) dalpha + p sin(phi) dlambda + q k'(c) dc
    ///   p = dbeta'/dbeta = k / (cos^2 b + k^2 sin^2 b)  in [1/k, k]
    ///   q = dbeta'/dk    <= 1 / (2k)                    by AM-GM
    /// ```
    ///
    /// In arc length `|grad alpha| = 1/(R sin c)`, `|grad lambda| = 1/(R cos
    /// phi)` and `|grad c| = 1/R`. All three of those diverge somewhere, and
    /// the domain is exactly what holds them off:
    ///
    /// * `|1 - p cos c| / sin c <= 1.0` for `c` in `[460 km, 6500 km]` - this
    ///   is the term that would blow up at the anchor, and it does not,
    ///   because `p -> 1` and `cos c -> 1` together there. At the far edge
    ///   `k = 1.197` and the term is 0.66.
    /// * `p / cos(phi)` is the polar term, and it is multiplied by `P`, which
    ///   is zero inside 500 km of colatitude. `sup P(theta) cot(theta)` over
    ///   `theta >= 500 km` is 3.30, at about 1600 km, so `p P / cos phi <=
    ///   1.197 * 3.30 = 3.95`.
    /// * `q k'(c) <= 0.5 * 0.44 = 0.22` at the far edge, where `k'` is
    ///   largest. At the antipode `k'` diverges; 6500 km is a third of the way
    ///   there.
    ///
    /// So `|grad gamma| P <= (1.0 + 3.95 + 0.22) / 6371 km = 8.12e-4 rad/km`.
    ///
    /// THE SMOOTHSTEP DERIVATIVES. Perlin's `3x^2 - 2x^3` has slope at most
    /// `1.5` across its band, so `|N'| <= 1.5/460`, `|P'| <= 1.5/1500` and
    /// `|F'| <= 1.5/1500` per kilometre, together `5.26e-3` per km.
    ///
    /// THE TWO GAMMA FACTORS. `|g| = |gamma| half_turn_fade(gamma)` peaks at
    /// 1.6293 rad ([`MAX_ROTATION_DEG`]) and `|g'| = |H + gamma H'|` peaks at
    /// 2.50, both at interior points of the 90-150 degree fade.
    ///
    /// Putting them together:
    ///
    /// ```text
    ///   |grad rho| <= 2.50 * 8.12e-4 + 1.6293 * 5.26e-3
    ///              =  2.03e-3      +  8.57e-3   = 1.06e-2 rad/km
    ///              =  0.61 degrees per kilometre
    /// ```
    ///
    /// The argument is spherical and the transform is WGS84; the flattening
    /// is `1/298`, so the ellipsoidal correction is a few tenths of a per
    /// cent, far inside the margin between 0.61 and the
    /// [`MAX_TURN_RATE_DEG_PER_KM`] this is pinned at. Sampling the whole
    /// shipped catalogue corroborates the argument and does not replace it.
    ///
    /// # References
    ///
    /// * Snyder, J.P. (1987). *Map Projections - A Working Manual.* USGS
    ///   Professional Paper 1395. Azimuthal equidistant scale factors, section
    ///   25 pp. 191-202; Lambert conformal conic and its grid convergence,
    ///   section 15 pp. 104-110 and eq. 14-4. doi:10.3133/pp1395
    /// * Vincenty, T. (1975). Direct and inverse solutions of geodesics on the
    ///   ellipsoid with application of nested equations. *Survey Review*
    ///   23(176), 88-93. doi:10.1179/sre.1975.23.176.88
    /// * Karney, C.F.F. (2013). Algorithms for geodesics. *Journal of Geodesy*
    ///   87(1), 43-55. doi:10.1007/s00190-012-0578-0
    /// * Perlin, K. (1985). An Image Synthesizer. *SIGGRAPH Computer Graphics*
    ///   19(3), 287-296. doi:10.1145/325165.325247
    /// * do Carmo, M.P. (1976). *Differential Geometry of Curves and
    ///   Surfaces.* Prentice-Hall. Section 4-4, the connection forms used for
    ///   `dgamma` above; geodesic polar coordinates section 4-6.
    /// * Jenny, B. (2012). Adaptive Composite Map Projections. *IEEE TVCG*
    ///   18(12), 2575-2582. doi:10.1109/TVCG.2012.192
    /// * Crum, T.D. and R.L. Alberty (1993). The WSR-88D and the WSR-88D
    ///   Operational Support Facility. *BAMS* 74, 1669-1687.
    ///   doi:10.1175/1520-0477(1993)074<1669:TWATWO>2.0.CO;2
    ///
    /// # What this costs, measured and accepted
    ///
    /// 1. THE MAP TURNS WHILE YOU PAN. The rotation is a function of the view
    ///    centre, so dragging changes it.
    ///
    ///    THE ORDINARY CASE, which is what an analyst meets: 0.0269 degrees
    ///    per kilometre of pan in the 460-920 km ramp band at mid latitude,
    ///    which at the default 0.35 km per point is 0.94 degrees per 100
    ///    points of drag and 6.6 degrees per 100 points at 2.8 km per point.
    ///
    ///    THE BOUND, which is a different claim: **0.61 degrees per
    ///    kilometre**, derived in "How fast the map can turn, by argument"
    ///    above and pinned with margin at [`MAX_TURN_RATE_DEG_PER_KM`]. At the
    ///    domain's own scale ceiling of 7 km per point that is 5.25 degrees
    ///    per screen point of drag, and at the 2.8 km per point a CONUS view
    ///    is drawn at it is 2.1.
    ///
    ///    THAT NUMBER IS AN ARGUMENT AND NOT A SAMPLE, and the distinction is
    ///    the reason the domain exists. Two earlier rounds stated the worst of
    ///    a grid of view centres as though it were a bound. The first swept a
    ///    ladder of downrange distances and missed the POLES, which sit at a
    ///    fixed colatitude and therefore at a different distance from every
    ///    site: it reported 0.0269 where the truth on the same rule was 107.9
    ///    degrees per 280 km of pan. The second added pole-relative radii,
    ///    swept 1 585 656 centres, reported 0.3180 - and missed the GLOBE'S
    ///    LIMB, where the morph's Jacobian vanishes and the true rate was
    ///    0.9132 deg/km, 10.96 degrees per screen point. A third and larger
    ///    grid would have been the same mistake a third time, because the
    ///    quantity is unbounded in the limit and no sample can bound it. The
    ///    domain removes the limit points instead.
    ///
    ///    `workstation_app::north_up::tests::the_pan_turn_rate_stays_under_its_documented_bound`
    ///    sweeps the whole shipped station table, the whole scale band and
    ///    radii placed relative to each site's own pole, asserts the ceiling
    ///    at every one of them and prints its own measured worst. It is
    ///    corroboration: if the argument above is right the sweep cannot fail,
    ///    and if the sweep fails the argument is wrong.
    ///
    ///    Widening the surveillance ramp was measured and rejected: see the
    ///    smoothstep note above.
    /// 2. A SITE CHANGE SNAPS THE ROTATION TO ZERO in one frame when the
    ///    analyst picks a radar under a distant cursor, because the new
    ///    centre is then on the new antenna. The end state is right - north-up
    ///    at the radar you just chose - and the analyst asked for it. Shipped
    ///    unsmoothed; if it is ever eased, ease it in the application's
    ///    display state and never in the stored camera.
    /// 3. A ROTATED PANE ASKS FOR MORE RASTER TILES than it needs, because
    ///    tile visibility takes an axis-aligned box around the pane boundary.
    ///    Bounded and self-correcting; see the note in [`crate::tiles`].
    /// 4. IN THE RAMP BAND AT FINE SCALE a sliver of the outer footprint can
    ///    be on screen while the map has begun to turn - a pane at 0.35 km
    ///    per point centred 600 km out shows ground from 320 to 880 km and is
    ///    rotated by 1.05 degrees. Registration there is EXACT: rings are
    ///    circles, azimuths are rays, range is untouched. Only the appearance
    ///    differs. The floor is deliberately on the MIDDLE of the pane, which
    ///    is where the analyst's eye is, and not on the nearest visible gate.
    /// 5. THE SCALE BAND UNWINDS AS YOU ZOOM OUT, AND A SPUN WHEEL CAN UNWIND
    ///    IT WHOLE. Over the 5-7 km per point band the scale fade hands the
    ///    rotation back, so a wheel notch turns the map as well as rescaling
    ///    it. One deliberate notch is `ZOOM_PER_NOTCH = 1.2` against a band a
    ///    factor of 1.4 wide, so a notch spans `ln 1.2 / ln 1.4 = 0.542` of
    ///    it and can carry at most 0.73 of the unwind - the largest change a
    ///    smoothstep makes across that fraction of its own width - while the
    ///    vanishing slope at both ends makes the first and last notch much
    ///    less. At the deepest rotation the domain admits that is 68 degrees
    ///    in one notch; at the 32 degrees a Portland radar looking at the
    ///    eastern seaboard is turned by, it is 23.
    ///
    ///    THE BURST CASE, which that does not cover. A flick earns
    ///    `MAX_BURST_GAIN`, so ONE input event is worth `1.2^5 = 2.49`; a
    ///    frame that swallows a queued backlog is capped by
    ///    `MAX_SCALE_CHANGE_PER_FRAME` at a DECADE of scale, which is wider
    ///    than the band several times over. So there is no bound of the form
    ///    "a notch unwinds part of it": one frame of wheel input can cross the
    ///    band outright and take the whole rotation with it.
    ///
    ///    What CAN be said, and it is an argument rather than a sample: the
    ///    rotation is inside `+/-`[`MAX_ROTATION_DEG`] before the event and
    ///    inside it after, so no event of any kind - deliberate notch, flick,
    ///    or a frame swallowing a whole queued backlog - can turn the map by
    ///    more than twice that, 186.72 degrees. It is not purely an unwind
    ///    either: a corner-anchored wheel moves the view centre as well as the
    ///    scale, so the rotation can change sign rather than only shrink.
    ///    `north_up::tests::a_spun_wheel_turns_the_map_by_no_more_than_this`
    ///    drives the real `ZoomResponder` at BOTH rates - a five-detent flick
    ///    and the queued backlog that saturates `MAX_SCALE_CHANGE_PER_FRAME` -
    ///    in both directions, over the whole shipped table, and asserts that
    ///    ceiling.
    /// 6. A GESTURE IS RESOLVED THROUGH THE ROTATION AT THE MIDDLE OF ITS OWN
    ///    MOTION, not the one the pane is drawn with. That is what keeps a pan
    ///    or a zoom and its reverse composing to the identity while this rule
    ///    turns the map under them - see `workstation_app::north_up`, which
    ///    owns the mechanism, the arithmetic and the proof. The price is half
    ///    of cost 1: the content is carried half a step's worth of turn away
    ///    from the pointer over the course of the gesture.
    #[must_use]
    pub fn view_rotation_rad(&self, centre: WorldPoint, km_per_point: f32) -> f32 {
        // THE DOMAIN, CHECKED BEFORE ANY GEODESY RUNS. Each of these is an
        // early return on a bit pattern, so "outside the domain" is exactly
        // the zero rotation and never a small number, and none of the
        // singular arithmetic the rule would otherwise meet is even reached.
        let scale_fade = north_up_scale_fade(km_per_point);
        if scale_fade == 0.0 {
            return 0.0;
        }
        let distance_km = centre.east_km.hypot(centre.north_km);
        if !distance_km.is_finite()
            || distance_km <= NEXRAD_SURVEILLANCE_RANGE_KM
            || distance_km >= NORTH_UP_ZERO_RANGE_KM
        {
            return 0.0;
        }
        // Inside the domain the geodesic always converges - the far edge is a
        // third of the way to the antipode - so this cannot fire; it is here
        // because a projection answering `None` must never be guessed at.
        let Some(gamma_rad) = self.north_bearing_rad(centre) else {
            return 0.0;
        };
        // `Camera2D::world_to_screen` sends a world bearing `b` to screen
        // bearing `b + rotation_rad`, so the rotation that puts north up is
        // the NEGATIVE of the bearing north is currently drawn at.
        //
        // Every other factor is a ramp in `0..=1`, multiplied in rather than
        // gated on, so that every edge this rule has is continuous: see the
        // domain contract in this module's own documentation.
        let rotation = -(gamma_rad as f32)
            * near_range_ramp(distance_km)
            * far_range_fade(distance_km)
            * scale_fade
            * self.polar_hold(centre)
            * half_turn_fade(gamma_rad);
        // Normalise the sign of zero. A view centre sitting on the anchor's
        // own meridian has `gamma == 0.0`, and negating that gives `-0.0`;
        // so does a product that underflows a hair past the floor. Both are
        // the same rotation as `+0.0` and neither turns a pixel, but the
        // near-field guarantee is asserted on the BIT PATTERN, so it has to be
        // the one bit pattern.
        if rotation == 0.0 { 0.0 } else { rotation }
    }

    /// 1 well away from a pole, 0 at one, on a smoothstep in between.
    ///
    /// See [`POLAR_HOLD_KM`] for why the map must not be turned near a pole at
    /// all, rather than turned by some carefully conditioned amount.
    fn polar_hold(&self, ground: WorldPoint) -> f32 {
        let (_lon, lat_deg) = self.world_to_lon_lat(ground);
        if !lat_deg.is_finite() {
            return 0.0;
        }
        let colatitude_km = (90.0 - lat_deg.abs()).to_radians() * POLAR_RADIUS_OF_CURVATURE_KM;
        smoothstep((((colatitude_km - POLAR_HOLD_KM) / POLAR_RAMP_KM) as f32).clamp(0.0, 1.0))
    }
}

/// Perlin's `3x^2 - 2x^3` on a clamped parameter (Perlin 1985).
fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// 0 inside the surveillance range, 1 by twice it, on a smoothstep in
/// between. The domain's INNER edge, which is about the analyst rather than
/// about the geometry.
fn near_range_ramp(distance_km: f64) -> f32 {
    let x = ((distance_km - NEXRAD_SURVEILLANCE_RANGE_KM) / NEXRAD_SURVEILLANCE_RANGE_KM)
        .clamp(0.0, 1.0) as f32;
    smoothstep(x)
}

/// 1 out to [`NORTH_UP_FULL_RANGE_KM`], 0 by [`NORTH_UP_ZERO_RANGE_KM`], on a
/// smoothstep in between. The domain's OUTER edge.
fn far_range_fade(distance_km: f64) -> f32 {
    let x = ((distance_km - NORTH_UP_FULL_RANGE_KM)
        / (NORTH_UP_ZERO_RANGE_KM - NORTH_UP_FULL_RANGE_KM))
        .clamp(0.0, 1.0) as f32;
    1.0 - smoothstep(x)
}

/// 1 at or below [`NORTH_UP_FULL_KM_PER_POINT`], 0 at or above
/// [`NORTH_UP_ZERO_KM_PER_POINT`], on a smoothstep in between. The domain's
/// SCALE edge, and the reason the rule can never meet a blended globe.
///
/// A scale that is not a number is not a scale: answer zero, which is this
/// file's reply to every question it cannot answer.
#[must_use]
pub fn north_up_scale_fade(km_per_point: f32) -> f32 {
    if !km_per_point.is_finite() {
        return 0.0;
    }
    if km_per_point <= NORTH_UP_FULL_KM_PER_POINT {
        return 1.0;
    }
    if km_per_point >= NORTH_UP_ZERO_KM_PER_POINT {
        return 0.0;
    }
    let x = (km_per_point - NORTH_UP_FULL_KM_PER_POINT)
        / (NORTH_UP_ZERO_KM_PER_POINT - NORTH_UP_FULL_KM_PER_POINT);
    1.0 - smoothstep(x)
}

/// 1 while true north is drawn within [`NORTH_UP_FULL_DEG`] of screen-up, 0 by
/// [`NORTH_UP_ZERO_DEG`], on a smoothstep in between.
///
/// This is what makes the rule continuous across the ray where `gamma` wraps.
/// See [`NORTH_UP_FULL_DEG`] for why a partial rotation of an angle near half
/// a turn is not a well-defined thing to ask for.
fn half_turn_fade(gamma_rad: f64) -> f32 {
    let offset_deg = gamma_rad.abs().to_degrees();
    let x = ((offset_deg - NORTH_UP_FULL_DEG) / (NORTH_UP_ZERO_DEG - NORTH_UP_FULL_DEG))
        .clamp(0.0, 1.0) as f32;
    1.0 - smoothstep(x)
}

/// Wrap to (-180, 180].
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

/// Signed shortest angular distance from `from` to `to`, so a site beside the
/// antimeridian does not project its neighbours a full turn away.
fn shortest_longitude_delta(to_deg: f64, from_deg: f64) -> f64 {
    normalize_longitude(to_deg - from_deg)
}

fn quantize_e7(degrees: f64) -> i32 {
    let scaled = (degrees * 1e7).round();
    scaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sites the app is actually exercised against, plus antimeridian cases.
    const SITES: &[(&str, f64, f64)] = &[
        ("KTLX", 35.3333, -97.2778),
        ("KRTX", 45.7150, -122.9650),
        ("KAKQ", 36.9840, -77.0074),
        ("KAPX", 44.9072, -84.7198),
        ("PAKC", 58.6794, -156.6294),
        ("synthetic antimeridian", 51.8800, 179.9500),
    ];

    #[test]
    fn round_trips_within_a_millimetre_across_the_radar_footprint() {
        for (name, lat, lon) in SITES {
            let projection = RadarProjection::new(*lat, *lon);
            for range_km in [0.0_f64, 1.0, 50.0, 230.0, 460.0, 1_000.0] {
                for azimuth_deg in (0..360).step_by(15) {
                    let azimuth = f64::from(azimuth_deg).to_radians();
                    let world = WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                    let (lon_deg, lat_deg) = projection.world_to_lon_lat(world);
                    let back = projection.lon_lat_to_world(lon_deg, lat_deg);
                    let error_km =
                        (back.east_km - world.east_km).hypot(back.north_km - world.north_km);
                    assert!(
                        error_km < 1e-6,
                        "{name}: round trip drifted {error_km} km at {range_km} km / {azimuth_deg}deg"
                    );
                }
            }
        }
    }

    #[test]
    fn the_anchor_projects_to_the_world_origin() {
        for (name, lat, lon) in SITES {
            let projection = RadarProjection::new(*lat, *lon);
            let origin = projection.lon_lat_to_world(*lon, *lat);
            assert!(
                origin.east_km.abs() < 1e-9 && origin.north_km.abs() < 1e-9,
                "{name}: anchor projected to {origin:?}"
            );
        }
    }

    /// Vincenty's own published test line, as redistributed by Geoscience
    /// Australia: Flinders Peak to Buninyong, 54 972.271 m, initial azimuth
    /// 306 deg 52' 05.37". This checks the geodesic engine against an external
    /// authority rather than against arithmetic of our own.
    /// Degrees, minutes, seconds to decimal degrees. The published case is
    /// specified in DMS; converting here rather than transcribing decimals
    /// keeps the reference exact to the millimetre.
    fn dms(degrees: f64, minutes: f64, seconds: f64) -> f64 {
        degrees + minutes / 60.0 + seconds / 3_600.0
    }

    #[test]
    fn matches_the_published_vincenty_test_line() {
        // Flinders Peak 37 57 03.72030 S, 144 25 29.52440 E.
        let projection =
            RadarProjection::new(-dms(37.0, 57.0, 3.72030), dms(144.0, 25.0, 29.52440));
        // Buninyong 37 39 10.15610 S, 143 55 35.38390 E.
        let world =
            projection.lon_lat_to_world(dms(143.0, 55.0, 35.38390), -dms(37.0, 39.0, 10.15610));

        let distance_m = world.east_km.hypot(world.north_km) * 1_000.0;
        assert!(
            (distance_m - 54_972.271).abs() < 0.001,
            "distance was {distance_m} m, expected 54972.271 m"
        );

        let azimuth_deg = world
            .east_km
            .atan2(world.north_km)
            .to_degrees()
            .rem_euclid(360.0);
        let expected_deg = 306.0 + 52.0 / 60.0 + 5.37 / 3_600.0;
        assert!(
            (azimuth_deg - expected_deg).abs() < 1e-6,
            "azimuth was {azimuth_deg} deg, expected {expected_deg} deg"
        );
    }

    /// The WGS84 meridian arc from 35.3333N to 36.3333N, evaluated from the
    /// standard series expansion, is 110.9559 km. A fixed-radius sphere gives
    /// 111.195 km here, which is the error this projection exists to avoid.
    #[test]
    fn a_degree_of_latitude_matches_the_wgs84_meridian_arc() {
        let projection = RadarProjection::new(35.3333, -97.2778);
        let world = projection.lon_lat_to_world(-97.2778, 36.3333);
        assert!(
            (world.north_km - 110.9559).abs() < 0.001,
            "meridian distance was {} km, expected 110.9559 km",
            world.north_km
        );
        assert!(
            world.east_km.abs() < 1e-6,
            "due north should have no easting"
        );
    }

    #[test]
    fn due_east_and_due_north_land_on_the_expected_axes() {
        let projection = RadarProjection::new(35.3333, -97.2778);
        let north = projection.world_to_lon_lat(WorldPoint::new(0.0, 100.0));
        assert!(
            (north.0 - -97.2778).abs() < 1e-9,
            "due north kept longitude"
        );
        assert!(north.1 > 35.3333, "due north increased latitude");

        let east = projection.world_to_lon_lat(WorldPoint::new(100.0, 0.0));
        assert!(east.0 > -97.2778, "due east increased longitude");
    }

    #[test]
    fn crossing_the_antimeridian_stays_local() {
        let projection = RadarProjection::new(51.88, 179.95);
        // A tenth of a degree east is across the antimeridian at -179.95.
        let world = projection.lon_lat_to_world(-179.95, 51.88);
        assert!(
            world.east_km > 0.0 && world.east_km < 20.0,
            "expected a short eastward hop, got {world:?}"
        );
        // And the inverse must come back across rather than wrapping the globe.
        let (lon, _lat) = projection.world_to_lon_lat(world);
        assert!((lon - -179.95).abs() < 1e-6, "inverse returned {lon}");
    }

    #[test]
    fn non_finite_input_cannot_poison_a_vertex_buffer() {
        let projection = RadarProjection::new(35.3333, -97.2778);
        for (lon, lat) in [(f64::NAN, 35.0), (-97.0, f64::INFINITY)] {
            let world = projection.lon_lat_to_world(lon, lat);
            assert!(world.east_km.is_finite() && world.north_km.is_finite());
        }
    }

    #[test]
    fn identity_tracks_the_anchor_and_the_algorithm_version() {
        let a = RadarProjection::new(35.3333, -97.2778);
        let b = RadarProjection::new(35.3333, -97.2778);
        let c = RadarProjection::new(45.7150, -122.9650);
        assert_eq!(a.id(), b.id());
        assert_ne!(a.id(), c.id());
        assert_eq!(a.id().algorithm_version, PROJECTION_ALGORITHM_VERSION);
    }

    /// The scale a whole-CONUS view is drawn at on a 1600x900-point pane -
    /// 4500 km across 1600 points - and the scale every proof in this file
    /// that is not about the scale edge itself is stated at. Well inside
    /// [`NORTH_UP_FULL_KM_PER_POINT`], which is the point.
    const CONTINENTAL_KM_PER_POINT: f32 = 2.8;

    /// Real sites, transcribed from the live catalogue at
    /// `%LOCALAPPDATA%/FahrenheitResearch/RadarWorkstation/cache/radar-sites.tsv`.
    const REAL_SITES: &[(&str, f64, f64)] = &[
        ("KTLX", 35.333_049_774_169_92, -97.277_748_107_910_16),
        ("KICT", 37.654_499_053_955_08, -97.442_802_429_199_22),
        ("KAKQ", 36.983_879_089_355_47, -77.007_499_694_824_22),
        ("KRTX", 45.714_968_872_070_31, -122.965_301_513_671_88),
        ("AWPA2", 61.150_001_525_878_906, -149.779_998_779_296_88),
        ("PHKI", 21.894_000_244_140_625, -159.552_001_953_125),
        ("RODN", 26.302_000_045_776_367, 127.909_004_211_425_78),
        ("TJUA", 18.115_600_585_937_5, -66.077_903_747_558_6),
    ];

    /// `(site, east_km bits, north_km bits)` from KTLX, captured from the
    /// shipped transform on 2026-08-18.
    ///
    /// This is the pin behind the promise that the radar-local projection did
    /// not move by a pixel when the globe was added. It is bit patterns, not
    /// an epsilon: a change of one unit in the last place of a `f64` fails it.
    const FROZEN_FROM_KTLX: &[(&str, u64, u64)] = &[
        ("KTLX", 0x0000000000000000, 0x0000000000000000),
        ("KICT", 0xc02d233ed3ccc32c, 0x407019e95e9065ca),
        ("KAKQ", 0x409bfeb29aabfbf3, 0x40772ed8f6397b19),
        ("KRTX", 0xc09ef79ba638a36b, 0x4096727082a20cdd),
        ("AWPA2", 0xc0a4ecb51103a3cd, 0x40adb173d57c69ed),
        ("PHKI", 0xc0b80c02558ae074, 0x407a0c790e148520),
        ("RODN", 0xc0be0b44e4c7d594, 0x40c137743aeca2aa),
        ("TJUA", 0x40a9e8cff4108ce6, 0xc096a3a6e0fa6fac),
    ];

    #[test]
    fn the_radar_local_transform_is_bit_for_bit_what_it_shipped_as() {
        let projection = RadarProjection::new(REAL_SITES[0].1, REAL_SITES[0].2);
        for ((name, lat, lon), (frozen_name, east_bits, north_bits)) in
            REAL_SITES.iter().zip(FROZEN_FROM_KTLX)
        {
            assert_eq!(name, frozen_name, "the two tables must stay aligned");
            let world = projection
                .try_lon_lat_to_world(*lon, *lat)
                .expect("a real site projects");
            assert_eq!(
                world.east_km.to_bits(),
                *east_bits,
                "{name} easting moved: {} vs frozen {}",
                world.east_km,
                f64::from_bits(*east_bits)
            );
            assert_eq!(
                world.north_km.to_bits(),
                *north_bits,
                "{name} northing moved: {} vs frozen {}",
                world.north_km,
                f64::from_bits(*north_bits)
            );
        }
    }

    /// The globe entry point at zero blend must be the SAME CALL, not an
    /// equivalent one. Every camera scale an analyst uses produces zero blend
    /// (proved in `globe`), so this is what keeps 50-460 km work untouched.
    #[test]
    fn the_globe_entry_point_is_the_shipped_transform_at_zero_blend() {
        for (anchor_name, anchor_lat, anchor_lon) in REAL_SITES {
            let projection = RadarProjection::new(*anchor_lat, *anchor_lon);
            for (name, lat, lon) in REAL_SITES {
                let shipped = projection.try_lon_lat_to_world(*lon, *lat);
                let globe = projection.try_lon_lat_to_globe(*lon, *lat, 0.0);
                match (shipped, globe) {
                    (Some(shipped), Some(globe)) => {
                        assert_eq!(
                            shipped.east_km.to_bits(),
                            globe.east_km.to_bits(),
                            "{name} from {anchor_name}"
                        );
                        assert_eq!(
                            shipped.north_km.to_bits(),
                            globe.north_km.to_bits(),
                            "{name} from {anchor_name}"
                        );
                    }
                    (None, None) => {}
                    _ => {
                        panic!("{name} from {anchor_name}: the two paths disagreed on drawability")
                    }
                }
            }
        }
    }

    /// Also pin the near field itself, at the ranges the analyst interrogates,
    /// rather than only at whole-catalogue distances.
    #[test]
    fn analysis_ranges_survive_the_globe_entry_point_untouched() {
        let projection = RadarProjection::new(REAL_SITES[0].1, REAL_SITES[0].2);
        for range_km in [1.0_f64, 50.0, 120.0, 230.0, 300.0, 460.0] {
            for azimuth_deg in (0..360).step_by(5) {
                let azimuth = f64::from(azimuth_deg).to_radians();
                let seed = WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                let (lon, lat) = projection.world_to_lon_lat(seed);
                let shipped = projection
                    .try_lon_lat_to_world(lon, lat)
                    .expect("inside the footprint");
                let globe = projection
                    .try_lon_lat_to_globe(lon, lat, 0.0)
                    .expect("inside the footprint");
                assert_eq!(shipped.east_km.to_bits(), globe.east_km.to_bits());
                assert_eq!(shipped.north_km.to_bits(), globe.north_km.to_bits());
            }
        }
    }

    #[test]
    fn the_globe_inverse_returns_the_point_the_forward_pass_started_from() {
        let projection = RadarProjection::new(REAL_SITES[0].1, REAL_SITES[0].2);
        for blend in [0.0_f32, 0.25, 0.6, 1.0] {
            for (name, lat, lon) in REAL_SITES {
                let Some(world) = projection.try_lon_lat_to_globe(*lon, *lat, blend) else {
                    continue;
                };
                let (back_lon, back_lat) = projection
                    .globe_to_lon_lat(world, blend)
                    .expect("a drawn point inverts");
                // Compare as a ground distance, so longitude near a pole is
                // not judged by its own degrees.
                let straight = projection
                    .try_lon_lat_to_world(*lon, *lat)
                    .expect("a real site projects");
                let returned = projection
                    .try_lon_lat_to_world(back_lon, back_lat)
                    .expect("the inverse lands somewhere real");
                let error_km = (returned.east_km - straight.east_km)
                    .hypot(returned.north_km - straight.north_km);
                assert!(
                    error_km < 1e-3,
                    "{name} at blend {blend} came back {error_km} km away"
                );
            }
        }
    }

    /// The complaint, as a number.
    ///
    /// Anchored at a Portland radar and looking at 40N 75W - the middle of the
    /// eastern seaboard, 3911.35 km away - the shipped projection draws true
    /// north 32.328 degrees anticlockwise of screen-up, which is the whole of
    /// "the east half of the country is crooked". The rule turns the camera by
    /// the same amount the other way.
    ///
    /// Cross-checked against the closed form
    /// `az1 + atan2(-k sin az2, cos az2)` with `k = c / sin c` the transverse
    /// scale factor (Snyder 1987, section 25): forward azimuth 81.803 degrees,
    /// reverse 295.517, arc 35.1754 degrees, `k = 1.065694`, giving -32.3249.
    /// The pure azimuth convergence, which ignores the stretch, is -33.7136.
    #[test]
    fn the_eastern_seaboard_is_drawn_thirty_two_degrees_off_north_from_portland() {
        let krtx = RadarProjection::new(REAL_SITES[3].1, REAL_SITES[3].2);
        assert_eq!(REAL_SITES[3].0, "KRTX", "the site table moved");
        let target = krtx
            .try_lon_lat_to_world(-75.0, 40.0)
            .expect("40N 75W projects from KRTX");
        let range_km = target.east_km.hypot(target.north_km);
        assert!(
            (range_km - 3_911.354).abs() < 0.01,
            "the geodesic is {range_km} km"
        );
        let gamma_deg = krtx
            .north_bearing_rad(target)
            .expect("a drawn point has a north")
            .to_degrees();
        assert!(
            (gamma_deg - -32.327_957).abs() < 1e-4,
            "north is drawn at {gamma_deg} deg, expected -32.327957"
        );
        // The closed form, from the same two azimuths the geodesic gives.
        let stretch = {
            let c = range_km / globe::EARTH_MEAN_RADIUS_KM;
            c / c.sin()
        };
        assert!(
            (stretch - 1.065_694).abs() < 1e-5,
            "transverse stretch is {stretch}"
        );

        let applied_deg =
            f64::from(krtx.view_rotation_rad(target, CONTINENTAL_KM_PER_POINT)).to_degrees();
        assert!(
            (applied_deg - 32.327_957).abs() < 1e-4,
            "the rule applies {applied_deg} deg, expected +32.327957"
        );
        // Applied to the centre, north comes out UP, to a hundredth of a
        // degree. This is the fix, stated as the thing the eye is looking at.
        let residual_deg = gamma_deg + applied_deg;
        assert!(
            residual_deg.abs() < 0.01,
            "north is still {residual_deg} deg off screen-up at the view centre"
        );
    }

    /// The floor, as a bit pattern rather than as a small number.
    ///
    /// With the middle of the pane anywhere inside the radar's own
    /// surveillance range, at any azimuth and at every camera scale the
    /// application can reach, the rotation applied is EXACTLY zero. The
    /// convergence genuinely present inside that disc is not zero - it reaches
    /// 12.27 degrees inside a Barrow radar's footprint - and that gap is the
    /// point of the test: the analysis view is protected by an early return,
    /// not by the angle happening to be small.
    #[test]
    fn nothing_inside_the_surveillance_range_is_rotated_at_any_scale() {
        let scales = [
            analyst_runtime::MIN_KM_PER_POINT,
            0.1,
            analyst_runtime::DEFAULT_KM_PER_POINT,
            1.0,
            4.0,
            globe::MIN_BLEND_KM_PER_POINT,
            20.0,
            analyst_runtime::MAX_KM_PER_POINT,
        ];
        let panes = [
            (1600.0_f32, 900.0_f32),
            (800.0, 450.0),
            (1280.0, 800.0),
            (3840.0, 2160.0),
            (640.0, 360.0),
            (1920.0, 480.0),
        ];
        for (name, lat, lon) in HIGH_LATITUDE_SITES {
            let projection = RadarProjection::new(*lat, *lon);
            let mut worst_gamma_deg: f64 = 0.0;
            for spoke in 0..72 {
                let azimuth = f64::from(spoke) * 5.0_f64.to_radians();
                for step in 0..=46 {
                    let range_km = f64::from(step) * 10.0;
                    let centre =
                        WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                    // The guarantee is about the centre's ACTUAL distance, and
                    // `range * sin`, `range * cos`, `hypot` can land a hair
                    // over a round 460: a point 60 picometres outside the disc
                    // is outside it. Those are covered by the boundary test
                    // below instead of being quietly asserted about here.
                    if centre.east_km.hypot(centre.north_km) > NEXRAD_SURVEILLANCE_RANGE_KM {
                        continue;
                    }
                    if let Some(gamma) = projection.north_bearing_rad(centre) {
                        worst_gamma_deg = worst_gamma_deg.max(gamma.to_degrees().abs());
                    }
                    for km_per_point in scales {
                        for (width_points, height_points) in panes {
                            let viewport = analyst_runtime::ViewportMetrics {
                                width_points,
                                height_points,
                                pixels_per_point: 1.0,
                            };
                            let blend = globe::blend_for_pane(km_per_point, viewport);
                            // A camera centre is stated in the frame the pane
                            // is DRAWN in, which past the globe floor is the
                            // warped one - that is what `screen_to_world`
                            // hands back and what the cursor readout unwarps.
                            // Inside the domain that frame IS the ground one,
                            // which the pane sweep here is what proves: the
                            // blend is an exact zero at every scale the rule
                            // answers at, on every window size.
                            let camera_centre = globe::warp_world(centre, blend).unwrap_or(centre);
                            let rotation =
                                projection.view_rotation_rad(camera_centre, km_per_point);
                            let context = format!(
                                "{name} applied {rotation} rad at {range_km} km / \
                                 {km_per_point} km per point on a \
                                 {width_points}x{height_points} pane"
                            );
                            // A BIT PATTERN AT EVERY SCALE, not a small
                            // number at some of them. Inside the floor the
                            // rule declines by early return; past the
                            // domain's scale edge it declines before it ever
                            // looks at the centre.
                            assert_eq!(rotation.to_bits(), 0.0_f32.to_bits(), "{context}");
                            if north_up_scale_fade(km_per_point) > 0.0 {
                                assert_eq!(
                                    blend, 0.0,
                                    "{context}: the rule answered at a nonzero blend"
                                );
                                assert_eq!(
                                    camera_centre.east_km.to_bits(),
                                    centre.east_km.to_bits(),
                                    "{context}: the camera frame is not the ground frame"
                                );
                            }
                        }
                    }
                }
            }
            assert!(
                worst_gamma_deg > 1.0,
                "{name}: the disc carries only {worst_gamma_deg} deg of convergence, so this \
                 test is not proving anything"
            );
        }
    }

    /// The ramp is continuous where it starts and where it finishes, and it
    /// starts at a hard zero.
    #[test]
    fn the_ramp_leaves_and_reaches_its_ends_without_a_step() {
        let krtx = RadarProjection::new(REAL_SITES[3].1, REAL_SITES[3].2);
        let at = |range_km: f64| {
            f64::from(
                krtx.view_rotation_rad(WorldPoint::new(range_km, 0.0), CONTINENTAL_KM_PER_POINT),
            )
            .to_degrees()
        };
        // Below and at the floor, on all four axes so the boundary is tested
        // where `hypot` is exact: a bit-pattern zero.
        for range_km in [0.0_f64, 100.0, 459.999, 460.0] {
            for centre in [
                WorldPoint::new(range_km, 0.0),
                WorldPoint::new(-range_km, 0.0),
                WorldPoint::new(0.0, range_km),
                WorldPoint::new(0.0, -range_km),
            ] {
                let rotation = krtx.view_rotation_rad(centre, CONTINENTAL_KM_PER_POINT);
                assert_eq!(
                    rotation.to_bits(),
                    0.0_f32.to_bits(),
                    "at {range_km} km, {centre:?}"
                );
            }
        }
        // Just past it, the smoothstep leaves zero with zero slope, so the
        // first tenth of a kilometre buys a millionth of a degree - and a
        // centre a picometre outside the disc buys something like 1e-34
        // radians, which no pixel can express.
        assert!(at(460.1).abs() < 1e-4, "the ramp stepped: {}", at(460.1));
        assert!(
            at(460.000_000_000_001).abs() < 1e-20,
            "a picometre outside the disc turned the map by {} deg",
            at(460.000_000_000_001)
        );
        // And it arrives at the top of the band without a kink either.
        let below = at(919.0);
        let at_top = at(920.0);
        let above = at(921.0);
        assert!(
            (at_top - below).abs() < 0.02 && (above - at_top).abs() < 0.02,
            "the band's top stepped: {below} -> {at_top} -> {above}"
        );
        assert!(
            above > 8.0,
            "beyond the band the full convergence should be cancelled, got {above}"
        );
    }

    /// THE GLOBE NEVER MEETS THIS RULE AT ALL, on every pane at every window
    /// size.
    ///
    /// The scale fade reaches zero at `globe::MIN_BLEND_KM_PER_POINT`, and
    /// `globe::blend_for_pane` returns an exact zero at or below that scale by
    /// its own early return - so a nonzero rotation implies a zero blend, and
    /// a nonzero blend implies a zero rotation. Neither half is a tolerance.
    /// That is the whole of the globe exclusion in the domain contract, and it
    /// is what removes the limb, the unwarp and the hand-back factor from this
    /// rule's arithmetic rather than fading them out.
    #[test]
    fn the_globe_band_is_outside_the_domain_on_every_pane() {
        let krtx = RadarProjection::new(REAL_SITES[3].1, REAL_SITES[3].2);
        let far = krtx
            .try_lon_lat_to_world(-75.0, 40.0)
            .expect("40N 75W projects");
        for (width_points, height_points) in [
            (1600.0_f32, 900.0_f32),
            (800.0, 450.0),
            (1280.0, 800.0),
            (3840.0, 2160.0),
            (640.0, 360.0),
            (1920.0, 480.0),
        ] {
            let viewport = analyst_runtime::ViewportMetrics {
                width_points,
                height_points,
                pixels_per_point: 1.0,
            };
            let full = globe::full_globe_scale(viewport);
            let ceiling = analyst_runtime::MAX_KM_PER_POINT.max(full * 2.0);
            // Every scale from the finest the camera allows up past a formed
            // globe, in two-per-cent steps, so the partial band is walked and
            // not stepped over.
            let mut km_per_point = analyst_runtime::MIN_KM_PER_POINT;
            while km_per_point < ceiling {
                let blend = globe::blend_for_pane(km_per_point, viewport);
                let warped = globe::warp_world(far, blend).unwrap_or(far);
                let rotation = krtx.view_rotation_rad(warped, km_per_point);
                let context = format!(
                    "{width_points}x{height_points} at {km_per_point} km per point, \
                     blend {blend}, rotation {rotation}"
                );
                if blend > 0.0 {
                    assert_eq!(rotation.to_bits(), 0.0_f32.to_bits(), "{context}");
                }
                if rotation != 0.0 {
                    assert_eq!(blend, 0.0, "{context}");
                }
                km_per_point *= 1.02;
            }
        }
    }

    /// Rubbish in cannot turn the map.
    #[test]
    fn a_centre_or_a_scale_that_is_not_a_number_rotates_nothing() {
        let krtx = RadarProjection::new(REAL_SITES[3].1, REAL_SITES[3].2);
        let far = WorldPoint::new(3_871.4, 557.7);
        for km_per_point in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            let rotation = krtx.view_rotation_rad(far, km_per_point);
            assert!(
                rotation.is_finite(),
                "{km_per_point} km per point produced {rotation} rad"
            );
        }
        assert_eq!(
            krtx.view_rotation_rad(far, f32::NAN).to_bits(),
            0.0_f32.to_bits(),
            "a scale that is not a number turned the map"
        );
        for centre in [
            WorldPoint::new(f64::NAN, 0.0),
            WorldPoint::new(0.0, f64::INFINITY),
        ] {
            assert_eq!(
                krtx.view_rotation_rad(centre, CONTINENTAL_KM_PER_POINT)
                    .to_bits(),
                0.0_f32.to_bits(),
                "{centre:?} turned the map"
            );
        }
    }

    /// Sites chosen for latitude, because the convergence inside a footprint
    /// is `delta-lambda * sin(phi)` to first order and therefore grows with
    /// it.
    ///
    /// The last row, PABR at Barrow, is a SYNTHETIC PROBE and not a shipped
    /// site: `nearest_site::REAL_STATION_CATALOG` has 208 rows and PABR is not
    /// one of them, the highest latitude there being PAPD at 65.0351 N. It is
    /// swept anyway because it is the harshest latitude a WSR-88D could stand
    /// at, and a rule that survives it survives the catalogue.
    const HIGH_LATITUDE_SITES: &[(&str, f64, f64)] = &[
        ("TJUA", 18.115_600_585_937_5, -66.077_903_747_558_6),
        ("KTLX", 35.333_049_774_169_92, -97.277_748_107_910_16),
        ("KRTX", 45.714_968_872_070_31, -122.965_301_513_671_88),
        ("AWPA2", 61.150_001_525_878_906, -149.779_998_779_296_88),
        ("PABR", 71.2854, -156.7889),
    ];

    /// THE RULE HOLDS STILL AT A POLE, and reaches it without a step.
    ///
    /// "North up" is a request about a direction, and there is no such
    /// direction at a pole: the screen bearing of true north winds through a
    /// whole turn around any loop enclosing one. Before the polar hold the
    /// rule answered anyway, and a sweep across the pole reported a 180 degree
    /// step at the crossing.
    #[test]
    fn the_rule_holds_still_at_a_pole_and_gets_there_without_a_step() {
        for (id, lat, lon) in HIGH_LATITUDE_SITES {
            let projection = RadarProjection::new(*lat, *lon);
            let pole_gap_km = (90.0 - lat.abs()).to_radians() * POLAR_RADIUS_OF_CURVATURE_KM;
            // Straight over the pole, a kilometre at a time, all the way
            // through and out the other side.
            let mut previous: Option<f32> = None;
            let mut worst_step = 0.0f64;
            for step in -600_i32..=600 {
                let centre = WorldPoint::new(0.0, pole_gap_km + f64::from(step));
                let here = projection.view_rotation_rad(centre, CONTINENTAL_KM_PER_POINT);
                assert!(here.is_finite(), "{id}: {centre:?} gave {here}");
                if step.abs() <= 20 {
                    assert_eq!(
                        here.to_bits(),
                        0.0_f32.to_bits(),
                        "{id}: a view centre {step} km from the pole was turned by {here}"
                    );
                }
                if let Some(before) = previous {
                    let jump = f64::from(here - before).abs().to_degrees();
                    worst_step = worst_step.max(jump);
                }
                previous = Some(here);
            }
            assert!(
                worst_step < 0.2,
                "{id}: crossing its own pole stepped the rotation {worst_step:.4} degrees"
            );
            println!("{id}: worst step per km across the pole {worst_step:.6} degrees");
        }
    }

    /// A PARTLY APPLIED ROTATION HAS NO JUMP WHERE THE BEARING WRAPS.
    ///
    /// `gamma`'s principal value jumps by a whole turn across the ray where
    /// north is drawn straight down. As a rotation that is nothing; as a
    /// FRACTION of a rotation - which the surveillance ramp, the downrange
    /// fade, the scale fade and the polar hold all produce - it is a flip of
    /// a whole turn times whatever that fraction is. Measured on the shipped
    /// rule before the half-turn fade: 173 degrees across two kilometres of
    /// pan.
    ///
    /// The ray is still reachable INSIDE the domain and that is why the fade
    /// stays: a view centre 4000 km due north of a 65N radar is 1200 km past
    /// its own pole, which is 4000 km downrange at a continental scale.
    #[test]
    fn a_partly_applied_rotation_does_not_flip_across_the_wrap_ray() {
        for (id, lat, lon) in HIGH_LATITUDE_SITES {
            let projection = RadarProjection::new(*lat, *lon);
            let pole_gap_km = (90.0 - lat.abs()).to_radians() * POLAR_RADIUS_OF_CURVATURE_KM;
            // Scales either side of the domain's scale ramp, so the fraction
            // being applied is a partial one at some of them.
            for km_per_point in [2.8_f32, 5.5, 6.4] {
                for beyond in [800.0_f64, 1200.0, 2500.0] {
                    let mut previous: Option<f32> = None;
                    for step in -30..=30 {
                        let centre = WorldPoint::new(f64::from(step) * 2.0, pole_gap_km + beyond);
                        let here = projection.view_rotation_rad(centre, km_per_point);
                        if let Some(before) = previous {
                            let jump = f64::from(here - before).abs().to_degrees();
                            assert!(
                                jump < 1.0,
                                "{id} at {km_per_point} km/point, {beyond} km past its pole: \
                                 2 km of pan flipped the map {jump:.4} degrees"
                            );
                        }
                        previous = Some(here);
                    }
                }
            }
        }
    }

    /// THE GUARDS DO NOT TOUCH THE CASE THE FEATURE WAS WRITTEN FOR.
    ///
    /// A Portland radar looking at 40N 75W: the whole complaint, and the
    /// number every other proof on this branch is anchored to. Both guards are
    /// at 1 there - the centre is 5560 km from the pole and north is drawn 32
    /// degrees off screen-up, well inside `NORTH_UP_FULL_DEG` - so the
    /// correction has to be the one it always was, to the bit.
    #[test]
    fn the_guards_leave_the_regional_correction_exactly_where_it_was() {
        let krtx = RadarProjection::new(45.714_968_872_070_31, -122.965_301_513_671_88);
        let target = krtx
            .try_lon_lat_to_world(-75.0, 40.0)
            .expect("40N 75W from KRTX");
        assert_eq!(
            krtx.polar_hold(target).to_bits(),
            1.0_f32.to_bits(),
            "the polar hold is not one at 40N"
        );
        let gamma = krtx.north_bearing_rad(target).expect("north bearing");
        assert_eq!(
            half_turn_fade(gamma).to_bits(),
            1.0_f32.to_bits(),
            "the half-turn fade is not one at 32 degrees of convergence"
        );
        let applied =
            f64::from(krtx.view_rotation_rad(target, CONTINENTAL_KM_PER_POINT)).to_degrees();
        assert!(
            (applied - 32.327_957).abs() < 1.0e-4,
            "the correction moved to {applied}"
        );
    }

    /// THERE IS ONLY ONE FRAME INSIDE THE DOMAIN, so there are no longer two
    /// entry points to confuse.
    ///
    /// A camera centre past the globe's blend start is a WARPED point, while a
    /// point round-tripped through longitude and latitude - which is what
    /// `WorkspaceState::apply_site_change` hands its rotation closure - is a
    /// ground point. Feeding the second to a rule that unwarps what it is
    /// given answers about somewhere else, and it was worth up to 1.83 degrees
    /// at a half-formed globe.
    ///
    /// The domain removes the distinction rather than documenting it: at every
    /// scale the rule answers at, `globe::warp_world` is the identity on the
    /// BIT PATTERN, so the warped centre and the ground centre are the same
    /// `f64`s and there is nothing to unwarp. The old mistake is now
    /// unrepresentable, because the parameter it needed does not exist.
    ///
    /// The second half is what keeps the first from being vacuous: at the
    /// scale where the two frames really do differ, the rule answers an exact
    /// zero from either one.
    #[test]
    fn inside_the_domain_the_camera_frame_and_the_ground_frame_are_one_frame() {
        let viewport = analyst_runtime::ViewportMetrics {
            width_points: 1600.0,
            height_points: 900.0,
            pixels_per_point: 1.0,
        };
        let krtx = RadarProjection::new(45.714_968_872_070_31, -122.965_301_513_671_88);
        let grounds = [
            WorldPoint::new(1500.0, 900.0),
            WorldPoint::new(3000.0, -400.0),
            WorldPoint::new(-2500.0, 2500.0),
            WorldPoint::new(4000.0, 4000.0),
        ];
        let mut km_per_point = analyst_runtime::MIN_KM_PER_POINT;
        while km_per_point < NORTH_UP_ZERO_KM_PER_POINT {
            let blend = globe::blend_for_pane(km_per_point, viewport);
            for ground in grounds {
                let warped = globe::warp_world(ground, blend).expect("inside the limb");
                assert_eq!(
                    warped.east_km.to_bits(),
                    ground.east_km.to_bits(),
                    "at {km_per_point} km per point the two frames differ about {ground:?}"
                );
                assert_eq!(
                    warped.north_km.to_bits(),
                    ground.north_km.to_bits(),
                    "at {km_per_point} km per point the two frames differ about {ground:?}"
                );
            }
            km_per_point *= 1.05;
        }
        // Where the frames DO differ, the rule declines from both of them.
        let outside = 11.0_f32;
        let blend = globe::blend_for_pane(outside, viewport);
        assert!(blend > 0.0 && blend < 1.0, "pick a scale inside the band");
        let mut worst_frame_gap_km = 0.0f64;
        for ground in grounds {
            let warped = globe::warp_world(ground, blend).expect("inside the limb");
            worst_frame_gap_km = worst_frame_gap_km
                .max((warped.east_km - ground.east_km).hypot(warped.north_km - ground.north_km));
            for centre in [ground, warped] {
                assert_eq!(
                    krtx.view_rotation_rad(centre, outside).to_bits(),
                    0.0_f32.to_bits(),
                    "the rule answered outside its domain about {centre:?}"
                );
            }
        }
        assert!(
            worst_frame_gap_km > 100.0,
            "the two frames are only {worst_frame_gap_km:.1} km apart at {outside} km per \
             point, so this proof is not measuring what it claims"
        );
        println!(
            "the two frames are up to {worst_frame_gap_km:.1} km apart at {outside} km per \
             point, and the rule answers zero from both"
        );
    }

    /// THE DOWNRANGE EDGE DOES NOT SNAP, and it reaches a bit-pattern zero.
    ///
    /// Walked outward a kilometre at a time from well inside the full-effect
    /// region to well past the edge, at every high-latitude anchor and at
    /// three scales. A jump here would be a new defect traded for the old one:
    /// the analyst would see the map flick as they dragged across 6500 km.
    #[test]
    fn the_downrange_edge_of_the_domain_is_crossed_without_a_step() {
        let mut worst_step = 0.0f64;
        let mut worst_where = String::new();
        for (id, lat, lon) in HIGH_LATITUDE_SITES.iter().chain(REAL_SITES.iter()) {
            let projection = RadarProjection::new(*lat, *lon);
            for km_per_point in [CONTINENTAL_KM_PER_POINT, 5.5, 6.5] {
                for spoke in 0..8 {
                    let azimuth = f64::from(spoke) * 45.0_f64.to_radians();
                    let mut previous: Option<f32> = None;
                    let mut range_km = NORTH_UP_FULL_RANGE_KM - 400.0;
                    while range_km < NORTH_UP_ZERO_RANGE_KM + 400.0 {
                        let centre =
                            WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                        let here = projection.view_rotation_rad(centre, km_per_point);
                        assert!(here.is_finite(), "{id}: {centre:?} gave {here}");
                        if range_km >= NORTH_UP_ZERO_RANGE_KM {
                            assert_eq!(
                                here.to_bits(),
                                0.0_f32.to_bits(),
                                "{id} at {range_km} km, past the domain's outer edge, \
                                 was turned by {here}"
                            );
                        }
                        if let Some(before) = previous {
                            let jump = f64::from(here - before).abs().to_degrees();
                            if jump > worst_step {
                                worst_step = jump;
                                worst_where =
                                    format!("{id} at {range_km} km, {km_per_point} km/point");
                            }
                            assert!(
                                jump < 0.1,
                                "{id} at {range_km} km, {km_per_point} km/point: one km of \
                                 pan across the outer edge turned the map {jump:.4} degrees"
                            );
                        }
                        previous = Some(here);
                        range_km += 1.0;
                    }
                }
            }
        }
        println!(
            "worst turn per km of pan across the downrange edge: {worst_step:.6} degrees \
             ({worst_where})"
        );
    }

    /// THE SCALE EDGE DOES NOT SNAP EITHER, and it reaches a bit-pattern zero
    /// exactly where the globe blend starts.
    ///
    /// Walked in per-mille steps of scale, which is a two-hundredth of a wheel
    /// notch, at view centres spread across the whole downrange band.
    #[test]
    fn the_scale_edge_of_the_domain_is_crossed_without_a_step() {
        let mut worst_step = 0.0f64;
        let mut worst_where = String::new();
        for (id, lat, lon) in HIGH_LATITUDE_SITES.iter().chain(REAL_SITES.iter()) {
            let projection = RadarProjection::new(*lat, *lon);
            for range_km in [600.0_f64, 1500.0, 3000.0, 4800.0, 5600.0] {
                for spoke in 0..4 {
                    let azimuth = f64::from(spoke) * 90.0_f64.to_radians();
                    let centre =
                        WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                    let mut previous: Option<f32> = None;
                    let mut km_per_point = NORTH_UP_FULL_KM_PER_POINT * 0.9;
                    while km_per_point < NORTH_UP_ZERO_KM_PER_POINT * 1.1 {
                        let here = projection.view_rotation_rad(centre, km_per_point);
                        if km_per_point >= NORTH_UP_ZERO_KM_PER_POINT {
                            assert_eq!(
                                here.to_bits(),
                                0.0_f32.to_bits(),
                                "{id} at {km_per_point} km/point, past the domain's scale \
                                 edge, was turned by {here}"
                            );
                        }
                        if let Some(before) = previous {
                            let jump = f64::from(here - before).abs().to_degrees();
                            if jump > worst_step {
                                worst_step = jump;
                                worst_where =
                                    format!("{id} at {range_km} km, {km_per_point} km/point");
                            }
                            // The band is a factor of 1.4 wide in scale and
                            // the smoothstep's slope peaks at 1.5, so a
                            // per-mille of scale can carry at most
                            // `MAX_ROTATION_DEG * 1.5 * 0.001 / ln(1.4)` =
                            // 0.416 degrees of unwind. This is the ceiling
                            // that argument gives, with margin, not a number
                            // read off the sweep.
                            assert!(
                                jump < 0.45,
                                "{id} at {range_km} km: a per-mille of scale across the \
                                 scale edge turned the map {jump:.4} degrees"
                            );
                        }
                        previous = Some(here);
                        km_per_point *= 1.001;
                    }
                }
            }
        }
        println!(
            "worst turn per per-mille of scale across the scale edge: {worst_step:.6} \
             degrees ({worst_where})"
        );
    }

    /// THE ROTATION ITSELF IS BOUNDED, which is what makes cost 5 an argument.
    ///
    /// `|gamma * half_turn_fade(gamma)|` peaks at 1.6293 rad, and every other
    /// factor is in `0..=1`, so no view can be turned by more than
    /// [`MAX_ROTATION_DEG`]. Checked against the closed form on a fine sweep of
    /// `gamma`, and against the real rule over the high-latitude anchors and
    /// the whole downrange band, which is where the largest convergences the
    /// domain still admits actually live.
    #[test]
    fn no_view_is_ever_turned_further_than_the_stated_ceiling() {
        let mut worst_closed_form = 0.0f64;
        for step in 0..=180_000 {
            let gamma = f64::from(step) * 0.001_f64.to_radians();
            let product = gamma * f64::from(half_turn_fade(gamma));
            worst_closed_form = worst_closed_form.max(product.to_degrees());
        }
        assert!(
            worst_closed_form <= MAX_ROTATION_DEG,
            "the closed form reaches {worst_closed_form:.4} degrees, over the stated \
             ceiling of {MAX_ROTATION_DEG}"
        );
        assert!(
            worst_closed_form > MAX_ROTATION_DEG - 0.5,
            "the ceiling is {} degrees looser than the closed form, so it is not the \
             ceiling it claims to be",
            MAX_ROTATION_DEG - worst_closed_form
        );

        let mut worst_applied = 0.0f64;
        let mut worst_where = String::new();
        for (id, lat, lon) in HIGH_LATITUDE_SITES.iter().chain(REAL_SITES.iter()) {
            let projection = RadarProjection::new(*lat, *lon);
            for spoke in 0..72 {
                let azimuth = f64::from(spoke) * 5.0_f64.to_radians();
                let mut range_km = 470.0_f64;
                while range_km < NORTH_UP_ZERO_RANGE_KM {
                    let centre =
                        WorldPoint::new(range_km * azimuth.sin(), range_km * azimuth.cos());
                    let applied = f64::from(projection.view_rotation_rad(centre, 1.0)).to_degrees();
                    if applied.abs() > worst_applied {
                        worst_applied = applied.abs();
                        worst_where = format!("{id} at {range_km} km, azimuth {}", spoke * 5);
                    }
                    assert!(
                        applied.abs() <= MAX_ROTATION_DEG,
                        "{id} at {range_km} km, azimuth {}: turned {applied:.4} degrees, \
                         over the ceiling of {MAX_ROTATION_DEG}",
                        spoke * 5
                    );
                    range_km += 25.0;
                }
            }
        }
        println!(
            "closed form peaks at {worst_closed_form:.4} deg; worst applied over the \
             anchors {worst_applied:.4} deg ({worst_where})"
        );
    }

    /// THE DOMAIN DOES THE JOB IT WAS CHOSEN FOR: every CONUS view centre,
    /// from every CONUS radar in the table, at the scale the whole country is
    /// drawn at, is inside the FULL-EFFECT region and not merely inside the
    /// ramp.
    ///
    /// This is the claim that decides the numbers, so it is checked and not
    /// asserted in prose: the corners of a box around the contiguous United
    /// States, projected from the westernmost and easternmost anchors the
    /// table carries.
    #[test]
    fn every_conus_view_centre_from_a_conus_radar_gets_the_whole_correction() {
        // (lon, lat) corners and edge midpoints of a box around CONUS.
        const CONUS: &[(f64, f64)] = &[
            (-124.7, 48.4),
            (-124.4, 32.5),
            (-66.9, 44.8),
            (-80.1, 25.1),
            (-98.6, 39.8),
            (-124.7, 39.8),
            (-66.9, 39.8),
            (-95.0, 49.4),
            (-95.0, 25.1),
        ];
        let mut worst_km = 0.0f64;
        let mut worst_where = String::new();
        for (id, lat, lon) in REAL_SITES {
            // The table also carries Hawaii, Okinawa and Puerto Rico; this
            // claim is about CONUS radars looking at CONUS.
            if !(24.0..50.0).contains(lat) || !(-125.0..-66.0).contains(lon) {
                continue;
            }
            let projection = RadarProjection::new(*lat, *lon);
            for (target_lon, target_lat) in CONUS {
                let centre = projection
                    .try_lon_lat_to_world(*target_lon, *target_lat)
                    .expect("a CONUS point projects from a CONUS radar");
                let range_km = centre.east_km.hypot(centre.north_km);
                if range_km > worst_km {
                    worst_km = range_km;
                    worst_where = format!("{id} to {target_lat}N {target_lon}E");
                }
                assert!(
                    range_km < NORTH_UP_FULL_RANGE_KM,
                    "{id} to {target_lat}N {target_lon}E is {range_km:.1} km, outside the \
                     full-effect range of {NORTH_UP_FULL_RANGE_KM}"
                );
                assert_eq!(
                    far_range_fade(range_km).to_bits(),
                    1.0_f32.to_bits(),
                    "{id} to {target_lat}N {target_lon}E is in the downrange ramp"
                );
            }
        }
        // And the scale the whole country is drawn at on the pane the proofs
        // use: 4500 km across 1600 points.
        assert_eq!(
            north_up_scale_fade(CONTINENTAL_KM_PER_POINT).to_bits(),
            1.0_f32.to_bits(),
            "a whole-CONUS pane is not at full effect"
        );
        println!(
            "furthest CONUS view centre from a CONUS radar: {worst_km:.1} km ({worst_where}), \
             against a full-effect range of {NORTH_UP_FULL_RANGE_KM} km"
        );
    }

    /// THE STATES THE LAST TWO ROUNDS OF THIS BOUND WERE FALSIFIED AT ARE
    /// OUTSIDE THE DOMAIN.
    ///
    /// Round one pinned 0.0269 deg/km and was falsified NEAR A POLE, where the
    /// turn rate grows like the reciprocal of the distance to it. Round two
    /// added pole-relative sampling, pinned 0.3180 as the worst of 1 585 656
    /// centres, and was falsified AGAINST THE GLOBE'S LIMB: with the view
    /// centre 9413.5 km out at 12 km per point, a half-formed globe whose
    /// horizon is 16 568 km, the true rate was 0.9132 deg/km and 10.96 degrees
    /// per SCREEN POINT, at eight different anchors. A third and larger grid
    /// would have been the same mistake again, because the quantity is
    /// unbounded in the limit.
    ///
    /// The domain is what answers that, so the counterexamples are pinned
    /// here rather than left in a review: at the state round two was falsified
    /// at, the rule answers an exact zero, and it answers it for a reason
    /// stated in the contract rather than by luck. The polar case is pinned
    /// separately by `the_rule_holds_still_at_a_pole_and_gets_there_without_a_step`.
    #[test]
    fn the_states_this_bound_was_falsified_at_are_outside_the_domain() {
        /// The anchors the second round's counterexample was found at, all of
        /// which gave between 0.9008 and 0.9132 degrees per kilometre.
        const ANCHORS: &[(&str, f64, f64)] = &[
            ("TLKA2", 62.316_7, -150.1),
            ("PAHG", 60.725_9, -151.351_4),
            ("AWPA2", 61.150_001_525_878_906, -149.779_998_779_296_88),
            ("KRTX", 45.714_968_872_070_31, -122.965_301_513_671_88),
            ("KPOE", 31.155_3, -92.975_8),
            ("KTLX", 35.333_049_774_169_92, -97.277_748_107_910_16),
            ("PGUA", 13.455_8, 144.808_3),
            ("TJUA", 18.115_600_585_937_5, -66.077_903_747_558_6),
        ];
        /// The camera radius and scale the rate was measured at.
        const RADIUS_KM: f64 = 9_413.5;
        const KM_PER_POINT: f32 = 12.0;
        // Both gates refuse it, and either one alone would.
        assert_eq!(
            north_up_scale_fade(KM_PER_POINT).to_bits(),
            0.0_f32.to_bits(),
            "{KM_PER_POINT} km per point is inside the domain's scale edge"
        );
        const {
            assert!(
                RADIUS_KM > NORTH_UP_ZERO_RANGE_KM,
                "the radius this bound was falsified at is inside the domain's downrange edge"
            )
        };
        for (id, lat, lon) in ANCHORS {
            let projection = RadarProjection::new(*lat, *lon);
            for spoke in 0..36 {
                let azimuth = f64::from(spoke) * 10.0_f64.to_radians();
                let centre = WorldPoint::new(RADIUS_KM * azimuth.sin(), RADIUS_KM * azimuth.cos());
                assert_eq!(
                    projection.view_rotation_rad(centre, KM_PER_POINT).to_bits(),
                    0.0_f32.to_bits(),
                    "{id} at azimuth {}: the rule answered at the state its own bound was \
                     falsified at",
                    spoke * 10
                );
            }
        }
        // And the one-screen-point case from the same review: PACG at 12 km
        // per point, 0.957 of its horizon, where one point of drag turned the
        // drawn map 8.4692 degrees.
        let pacg = RadarProjection::new(56.852_8, -135.529_2);
        let horizon_km = globe::horizon_radius_km(globe::blend_for_pane(
            KM_PER_POINT,
            analyst_runtime::ViewportMetrics {
                width_points: 1600.0,
                height_points: 900.0,
                pixels_per_point: 1.0,
            },
        ));
        let centre = WorldPoint::new(0.0, horizon_km * 0.957);
        assert_eq!(
            pacg.view_rotation_rad(centre, KM_PER_POINT).to_bits(),
            0.0_f32.to_bits(),
            "the rule answered {horizon_km} km out against a half-formed globe's limb"
        );
    }

    /// WHAT THIS FILE SAYS ABOUT ITSELF HAS TO BE TRUE.
    ///
    /// Two claims in the doc comments above were not, and both sent a reader
    /// chasing something that does not exist: the turn-rate bound was said to
    /// be pinned by a `north_up_bound.rs` integration test that has never been
    /// a file in this repository, and the polar hold's recovery
    /// distance was justified by calling PABR "the highest-latitude radar in
    /// the shipped catalogue" when PABR is not in that catalogue at all.
    ///
    /// So this reads the file back and checks both kinds of claim: every
    /// repository path it names must exist, and the PABR sentence must be the
    /// one that is true.
    #[test]
    fn every_path_and_site_claim_in_this_file_is_true() {
        const SOURCE: &str = include_str!("projection.rs");
        // `CARGO_MANIFEST_DIR` is `crates/map_scene`, so two levels up is the
        // workspace root the paths in these comments are relative to.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the manifest directory has a workspace root above it")
            .to_path_buf();
        let repository_paths = |text: &str| -> Vec<String> {
            text.split_whitespace()
                .map(|token| {
                    token
                        .trim_matches(|c: char| !c.is_ascii_graphic() || c == '`' || c == ',')
                        .to_string()
                })
                .filter(|token| token.starts_with("crates/") && token.ends_with(".rs"))
                .collect()
        };
        // The control arm: the sentence this file used to carry really would
        // be caught, so a clean run means the claim is absent and not that the
        // check is blind.
        // Assembled rather than written out, so this sentence does not put the
        // very token it is checking for back into the file.
        let control = repository_paths(&format!(
            "pinned by `{}/workstation_app/tests/{}.rs`, which sweeps",
            "crates", "north_up_bound"
        ));
        assert_eq!(control.len(), 1, "the path scanner stopped finding paths");
        assert!(
            !root.join(&control[0]).exists(),
            "{} exists after all, so the control arm proves nothing",
            control[0]
        );
        let named = repository_paths(SOURCE);
        for path in &named {
            assert!(
                root.join(path).exists(),
                "this file points a reader at {path}, which does not exist"
            );
        }
        // And the bound now names the pin that really holds it.
        assert!(
            SOURCE.contains("the_pan_turn_rate_stays_under_its_documented_bound"),
            "the turn-rate ceiling no longer says what pins it"
        );
        // Assembled for the same reason the control path above is.
        let false_claim = format!("highest-latitude radar in the {} catalogue", "shipped");
        assert!(
            !SOURCE.contains(&false_claim),
            "PABR is not a row of the station table and this file says it is"
        );
        assert!(
            SOURCE.contains("PAPD at 65.0351 N"),
            "the polar hold's justification no longer names the highest row the \
             catalogue really ships"
        );
        println!(
            "checked {} repository paths named in this file",
            named.len()
        );
    }

    #[test]
    fn longitude_normalizes_into_the_expected_half_open_range() {
        assert_eq!(normalize_longitude(180.0), 180.0);
        assert_eq!(normalize_longitude(-180.0), 180.0);
        assert_eq!(normalize_longitude(190.0), -170.0);
        assert_eq!(normalize_longitude(-190.0), 170.0);
    }
}
