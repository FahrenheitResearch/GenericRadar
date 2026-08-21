//! The pane's north-up frame: the rotation the map is DRAWN with, and the
//! rotation a GESTURE is resolved through.
//!
//! # Why those are two different questions
//!
//! `map_scene::projection::RadarProjection::view_rotation_rad` derives a
//! camera rotation that puts true north up at the middle of the pane. The
//! rotation is a function of the view centre, and a pan or a zoom moves the
//! view centre - so the rule feeds back on the gesture that drives it.
//!
//! That feedback broke an invariant `analyst_runtime::view` names in its own
//! source as the reason the wheel response is geometric at all: "a notch out
//! undoes a notch in and the camera lands back where it started. That
//! reversibility is what makes a wheel feel like it obeys the analyst."
//! `Camera2D::zoom_about` and `Camera2D::pan_by_screen_delta` both send a
//! SCREEN offset to a WORLD offset through the camera's rotation, so resolving
//! the outward gesture through one rotation and the return gesture through
//! another leaves the pair short of the identity. Measured before the stable
//! gesture rotation was introduced: twenty wheel in-and-out cycles about a
//! corner anchor drifted
//! the centre 304.0 km at a 600 km centre and 2.0 km per point, 1888.9 km in
//! the globe band; ten drags out and ten back left the map 74.1 to 1221.8 km
//! from where it started. With the derived rotation forced to zero - the
//! behaviour before the feature - the same loops drift 0.0000 km.
//!
//! # What this module does instead
//!
//! A gesture is resolved through the rotation the rule gives at the MIDDLE of
//! the motion the gesture produces - the implicit midpoint. That single change
//! makes a gesture and its inverse compose to the identity, because they share
//! a midpoint:
//!
//! ```text
//!   forward   c1 = c0 - s M(rot(mid)) d      mid = (c0 + c1) / 2
//!   reverse   c2 = c1 + s M(rot(mid)) d      mid = (c1 + c2) / 2 = the same
//! ```
//!
//! so `c2 = c0` is a solution of the reverse step whenever `c1` solved the
//! forward one. For a zoom the shared midpoint is the same statement with the
//! GEOMETRIC mean of the two scales, which is why [`midpoint`] takes a square
//! root: a zoom of `f` and a zoom of `1/f` about the same screen point meet at
//! `sqrt(s0 s1)`.
//!
//! Symmetric one-step methods are exactly the methods with this property, and
//! the implicit midpoint rule is the canonical one (Hairer, Lubich and Wanner,
//! *Geometric Numerical Integration*, 2nd ed., Springer 2006, section II.1:
//! a method is time-reversible iff it is symmetric, and the implicit midpoint
//! rule is symmetric). Nothing here is novel; it is the standard remedy for
//! exactly this shape of defect.
//!
//! The midpoint is implicit - the rotation decides the motion and the motion
//! decides the rotation - so it is found by fixed-point iteration, which is a
//! contraction as long as one step does not carry the world further than
//! [`MAX_SYMMETRIC_STEP_KM`]. Longer gestures are cut into pieces that do not.
//! A composition of reversible steps is reversible, so cutting costs nothing.
//!
//! None of that machinery runs where there is no rotation to be reversible
//! about. [`NorthUpFrame::rule_is_off_throughout`] hands such a gesture
//! straight to `Camera2D`, which keeps everything outside the north-up domain
//! (the contract is stated in `map_scene::projection`) running the arithmetic
//! it ran before this feature existed, to the bit.
//!
//! That test is an INTERVAL ARGUMENT and not a sample of the path, which is
//! the mistake the previous version of it made: three bit-pattern probes at
//! the start, the end and the halfway state said "off" for a gesture whose
//! middle passed through a region where the rule was on, and one leg of a
//! round trip took the fast path while the other took the solver. The camera
//! makes the argument available: `pan_by_screen_delta` moves the view centre
//! along a straight segment at a fixed scale, `zoom_about` moves it along the
//! straight segment between its two endpoints while the scale changes
//! monotonically, and `apply_nav` zooms about the pane's OWN centre, which
//! does not move the view centre at all. So for every gesture this module can
//! be handed, the centre stays on the segment between the two endpoints and
//! the scale stays between the two endpoint scales - and each of the three
//! ways the rule can be off throughout is then a closed-form test on that
//! segment rather than a probe of it.
//!
//! # The three mechanisms this was chosen over
//!
//! * LATCHING the rotation for the duration of a gesture. It makes a gesture
//!   compose with ITSELF, which is not the property that was broken: a wheel
//!   out-and-back is two gestures with an idle frame between them, and a drag
//!   out and a drag back are two drags. It also needs a gesture lifetime, and
//!   whatever that lifetime is, releasing the latch snaps the map by the whole
//!   turn the gesture accumulated - trading a smooth turn during the drag for
//!   a jump after it.
//! * RESOLVING IN THE UNROTATED FRAME and rotating only at draw time. Exactly
//!   reversible and trivial, and it stops the map following the pointer: the
//!   content would slide off the drag direction by the whole rotation, up to
//!   32 degrees at the anchor this feature was written for.
//! * DROPPING THE SCALE TERM so a zoom cannot change the rotation. It does not
//!   fix a pan at all, and it does not fix a zoom either, because
//!   `zoom_about` moves the centre for any anchor but the pane's own middle -
//!   and this application always anchors a wheel zoom on the POINTER.
//!
//! What the midpoint costs instead: the gesture is resolved through the
//! rotation at the middle of its own motion rather than at its start, so the
//! content is carried half a step's worth of turn away from the pointer. That
//! is half of the turn cost 1 on `view_rotation_rad` already documents, and it
//! is the whole price.

use analyst_runtime::{Camera2D, NEXRAD_SURVEILLANCE_RANGE_KM, NavInput, ScreenPoint};
use analyst_runtime::{ViewportMetrics, WorldPoint};
use map_scene::projection::{NORTH_UP_ZERO_KM_PER_POINT, NORTH_UP_ZERO_RANGE_KM, RadarProjection};

/// The furthest one symmetric step may carry the middle of the pane.
///
/// The fixed-point iteration below contracts by about `L * span / 2`, with `L`
/// the ceiling on the rate at which the rule turns the map -
/// [`map_scene::projection::MAX_TURN_RATE_DEG_PER_KM`], 0.75 degrees per
/// kilometre, which is 1.31e-2
/// radians per kilometre. At 100 km that is 0.65, so the iteration is a strict
/// contraction everywhere INSIDE THE DOMAIN and the midpoint is unique - which
/// is what makes the forward and the reverse gesture find the SAME one. That
/// `L` is an analytic ceiling and not the worst of a sweep, so this is a
/// property of the rule rather than of the grid somebody last measured on.
const MAX_SYMMETRIC_STEP_KM: f64 = 100.0;

/// Ceiling on the pieces one gesture is cut into.
///
/// A frame of input cannot carry the world arbitrarily far - `zoom_about`
/// clamps the scale and `MAX_SCALE_CHANGE_PER_FRAME` clamps a spun wheel - but
/// the ceiling is here so that a pathological delta costs bounded work rather
/// than an unbounded loop. 128 pieces covers 12 800 km; at the domain's own
/// scale ceiling of 7 km per point, a drag across the full width of a
/// 1600-point pane is 11 200 km, so the cap is not reachable by a gesture the
/// rule is on for.
const MAX_SYMMETRIC_STEPS: u32 = 128;

/// Ceiling on the fixed-point iterations per piece.
///
/// At a contraction of 0.65 an initial error of a tenth of a radian is inside
/// `f32` rounding after 24 steps. The loop stops early the moment the rotation
/// repeats, which for an ordinary drag is the second or third pass.
const MAX_MIDPOINT_ITERATIONS: u32 = 24;

/// How far the true path of a gesture may stray from the straight segment
/// between its two endpoints, allowed for in [`NorthUpFrame::rule_is_off_throughout`].
///
/// It is ZERO for a pan and for a wheel zoom, where the centre track IS that
/// segment. `apply_nav` zooms about the pane's own centre, where
/// `screen_to_world` of the anchor before and after the scale change is the
/// view centre both times, so the correction it applies is a difference of two
/// `f32` round-trips - nanometres of ground, not kilometres. A whole kilometre
/// of slack is four orders more than that and costs nothing: it only ever
/// makes the fast path decline a gesture it could have taken.
const SEGMENT_MARGIN_KM: f64 = 1.0;

/// One frame's worth of camera input, in the terms `Camera2D` already takes.
///
/// It is an enum rather than a closure because [`NorthUpFrame::resolve`] has to
/// be able to ask for a FRACTION of the gesture, and a closure cannot be cut in
/// half. Each variant carries exactly the arguments of the `Camera2D` method it
/// stands for, so this adds no new camera behaviour - only the ability to name
/// a gesture before applying it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
    /// A drag, as `Camera2D::pan_by_screen_delta` takes it.
    Pan {
        delta_x_points: f32,
        delta_y_points: f32,
    },
    /// A wheel or a pinch about a screen point, as `Camera2D::zoom_about`
    /// takes it.
    Zoom { factor: f32, anchor: ScreenPoint },
    /// One frame of keyboard flight, as `Camera2D::apply_nav` takes it.
    Nav { input: NavInput, dt_seconds: f32 },
}

impl Gesture {
    /// This gesture carried out in `steps` equal pieces: `steps` applications
    /// of the result are the original.
    ///
    /// A pan is linear in the delta and a zoom is geometric in the factor,
    /// which is why one divides and the other takes a root. Keyboard flight is
    /// linear in `dt` for its pan and exponential in `dt` for its hold, so
    /// dividing `dt` splits both exactly - which `apply_nav`'s own two-pass
    /// caller already relies on - and `zoom_steps` divides with it because it
    /// is a notch count rather than a rate.
    fn fraction(self, steps: u32) -> Self {
        let steps = steps.max(1);
        let inverse = 1.0 / steps as f32;
        match self {
            Self::Pan {
                delta_x_points,
                delta_y_points,
            } => Self::Pan {
                delta_x_points: delta_x_points * inverse,
                delta_y_points: delta_y_points * inverse,
            },
            Self::Zoom { factor, anchor } => Self::Zoom {
                factor: factor.powf(inverse),
                anchor,
            },
            Self::Nav { input, dt_seconds } => Self::Nav {
                input: NavInput {
                    zoom_steps: input.zoom_steps * inverse,
                    ..input
                },
                dt_seconds: dt_seconds * inverse,
            },
        }
    }

    /// Apply it to a camera, returning whether the camera moved.
    fn apply(self, camera: &mut Camera2D, viewport: ViewportMetrics) -> bool {
        match self {
            Self::Pan {
                delta_x_points,
                delta_y_points,
            } => {
                camera.pan_by_screen_delta(delta_x_points, delta_y_points);
                true
            }
            Self::Zoom { factor, anchor } => {
                camera.zoom_about(factor, anchor, viewport);
                true
            }
            Self::Nav { input, dt_seconds } => camera.apply_nav(input, dt_seconds, viewport),
        }
    }

    /// A reset is not a gesture with an inverse, and `apply_nav` short-circuits
    /// to `Camera2D::default()` on one. Resolving it symmetrically would iterate
    /// a constant map, so it is handed straight through instead.
    fn is_reset(self) -> bool {
        matches!(self, Self::Nav { input, .. } if input.reset)
    }
}

/// The north-up frame one pane is being drawn in this frame.
///
/// Cheap to build and never stored: the rotation stays DERIVED, exactly as
/// `view_rotation_rad`'s own contract requires, and nothing here writes to the
/// analyst's camera.
#[derive(Clone, Copy, Debug)]
pub struct NorthUpFrame {
    projection: Option<RadarProjection>,
    viewport: ViewportMetrics,
    /// The analyst's OWN rotation, carried through untouched. The derived term
    /// is added on top of it and never folded into it.
    stored_rotation_rad: f32,
}

impl NorthUpFrame {
    pub fn new(
        projection: Option<RadarProjection>,
        viewport: ViewportMetrics,
        stored_rotation_rad: f32,
    ) -> Self {
        Self {
            projection,
            viewport,
            stored_rotation_rad,
        }
    }

    /// A frame with no rule in it: every gesture resolves exactly as the
    /// `Camera2D` method it names, and the display camera is the stored one.
    ///
    /// For the harnesses that drive `draw_pane` with a camera they wrote
    /// themselves and are asking a question about the pane's chrome rather
    /// than about the map's orientation. The application reaches the same
    /// state through `NorthUpFrame::new(None, ..)` before a radar is loaded.
    ///
    /// The viewport is a placeholder, and it does not matter which one:
    /// [`Self::for_viewport`] re-seats every frame on the pane's own measured
    /// viewport before a gesture reaches it.
    #[cfg(test)]
    pub fn unrotated() -> Self {
        Self {
            projection: None,
            viewport: ViewportMetrics {
                width_points: 1.0,
                height_points: 1.0,
                pixels_per_point: 1.0,
            },
            stored_rotation_rad: 0.0,
        }
    }

    /// The same frame, measured against the viewport a pane has just laid
    /// itself out in.
    ///
    /// `draw_pane` calls this before it resolves anything, so a caller cannot
    /// hand a gesture a viewport the pane is not drawn in. Both things the
    /// viewport decides here are load bearing: `zoom_about` anchors a wheel
    /// against the viewport's CENTRE, so a pane-sized error puts the zoom
    /// somewhere else entirely, and the globe blend is a function of the pane's
    /// own diagonal.
    #[must_use]
    pub fn for_viewport(self, viewport: ViewportMetrics) -> Self {
        Self { viewport, ..self }
    }

    /// The rotation the rule gives for a camera, over and above the analyst's.
    pub fn derived_rotation_rad(&self, camera: Camera2D) -> f32 {
        let Some(projection) = self.projection.as_ref() else {
            return 0.0;
        };
        let camera = camera.sanitized();
        projection.view_rotation_rad(
            WorldPoint::new(camera.center_east_km, camera.center_north_km),
            camera.km_per_point,
        )
    }

    /// A stored camera as it will be DRAWN.
    pub fn display_camera(&self, stored: Camera2D) -> Camera2D {
        Camera2D {
            rotation_rad: stored.rotation_rad + self.derived_rotation_rad(stored),
            ..stored
        }
    }

    /// Apply one frame of input to a DISPLAY camera and return the display
    /// camera it becomes, resolved so that this gesture and its inverse
    /// compose to the identity.
    ///
    /// Returns whether the camera moved, which is what `apply_nav` reports and
    /// what the caller needs in order to skip the work a changed camera
    /// implies.
    pub fn resolve(&self, camera: &mut Camera2D, gesture: Gesture) -> bool {
        if self.projection.is_none() || gesture.is_reset() {
            return gesture.apply(camera, self.viewport);
        }
        // WHERE THE RULE IS OFF, THE GESTURE IS THE CAMERA CALL IT ALWAYS WAS,
        // to the bit. Two whole regions are like that and both are promised
        // unchanged elsewhere: everything inside the surveillance range, which
        // is the analysis view, and everything past full globe blend, which is
        // the globe that shipped. Resolving them symmetrically would be
        // arithmetic in place of arithmetic - `n` applications of a `1/n`
        // piece instead of one application - and over a long gesture at globe
        // scale that costs more accuracy than it can possibly buy, since there
        // is no rotation there to be reversible about.
        if self.rule_is_off_throughout(*camera, gesture) {
            return gesture.apply(camera, self.viewport);
        }
        // Work in the STORED frame throughout: each piece derives its own
        // rotation, and carrying a stale one between pieces would put the
        // feedback straight back.
        let mut stored = Camera2D {
            rotation_rad: self.stored_rotation_rad,
            ..*camera
        };
        let steps = self.piece_count(stored, gesture);
        let piece = gesture.fraction(steps);
        let mut moved = false;
        for _ in 0..steps {
            moved |= self.symmetric_step(&mut stored, piece);
        }
        *camera = self.display_camera(stored);
        moved
    }

    /// Whether the rule turns the map nowhere this gesture goes - decided by
    /// an argument about the whole path, not by probing points on it.
    ///
    /// The previous version probed the start, the end and the halfway state on
    /// the bit pattern, and that is a SAMPLE of a continuous path: a gesture
    /// whose middle passed through a region where the rule was on could read
    /// zero at all three, so one leg of a round trip took this fast path while
    /// the other took the solver, and the pair no longer composed to the
    /// identity. Measured at the time: 105.15 screen points on a single
    /// three-notch zoom out and back.
    ///
    /// What replaces it uses two facts about `Camera2D`, both pinned by tests
    /// in this module. The VIEW CENTRE stays on the straight segment between
    /// the two endpoint centres - a pan translates it, a wheel zoom slides it
    /// along the line through the anchor's world point, and a keyboard zoom is
    /// anchored on the pane's own centre and does not move it at all. The
    /// SCALE stays between the two endpoint scales, because each gesture
    /// changes it at most once and monotonically. On that segment, each of the
    /// three ways the rule can be off everywhere is a closed form:
    ///
    /// * THE SCALE IS PAST THE DOMAIN'S SCALE EDGE THROUGHOUT, which is the
    ///   smaller of the two endpoint scales being at or past it.
    /// * THE SEGMENT LIES INSIDE THE SURVEILLANCE FLOOR. The distance to the
    ///   origin is convex along a segment, so its maximum is at an endpoint.
    /// * THE SEGMENT LIES BEYOND THE DOMAIN'S DOWNRANGE EDGE. The minimum is
    ///   the perpendicular distance from the origin to the segment, in closed
    ///   form.
    ///
    /// Each is sufficient on its own, and a gesture that satisfies none of
    /// them simply takes the solver, which is correct everywhere - so the only
    /// cost of this test being conservative is arithmetic.
    fn rule_is_off_throughout(&self, start: Camera2D, gesture: Gesture) -> bool {
        if self.projection.is_none() {
            return true;
        }
        let start = start.sanitized();
        let mut end = Camera2D {
            rotation_rad: 0.0,
            ..start
        };
        gesture.apply(&mut end, self.viewport);
        let end = end.sanitized();
        // The scale edge. Monotone between the endpoints, so the whole path is
        // past it exactly when the nearer endpoint is.
        if start.km_per_point.min(end.km_per_point) >= NORTH_UP_ZERO_KM_PER_POINT {
            return true;
        }
        let from = WorldPoint::new(start.center_east_km, start.center_north_km);
        let to = WorldPoint::new(end.center_east_km, end.center_north_km);
        let furthest = from
            .east_km
            .hypot(from.north_km)
            .max(to.east_km.hypot(to.north_km));
        if !furthest.is_finite() {
            return false;
        }
        if furthest + SEGMENT_MARGIN_KM <= NEXRAD_SURVEILLANCE_RANGE_KM {
            return true;
        }
        let nearest = distance_from_origin_to_segment(from, to);
        nearest.is_finite() && nearest - SEGMENT_MARGIN_KM >= NORTH_UP_ZERO_RANGE_KM
    }

    /// How many pieces this gesture has to be cut into.
    ///
    /// Measured by applying it once at rotation ZERO. A rotation is an
    /// isometry, so the DISTANCE the middle of the pane travels does not depend
    /// on which rotation resolves the gesture - which is what makes the count
    /// the same for a gesture and for its inverse, and therefore what makes the
    /// two cut into matching pieces. For a pan the two measurements are
    /// bit-identical (the same scale, the negated delta, the same `hypot`); for
    /// a zoom they agree to `f32` rounding of the reciprocal factor.
    fn piece_count(&self, stored: Camera2D, gesture: Gesture) -> u32 {
        let mut probe = Camera2D {
            rotation_rad: 0.0,
            ..stored
        };
        gesture.apply(&mut probe, self.viewport);
        let span_km = (probe.center_east_km - stored.center_east_km)
            .hypot(probe.center_north_km - stored.center_north_km);
        if !span_km.is_finite() || span_km <= MAX_SYMMETRIC_STEP_KM {
            return 1;
        }
        let wanted = (span_km / MAX_SYMMETRIC_STEP_KM).ceil();
        if wanted >= f64::from(MAX_SYMMETRIC_STEPS) {
            MAX_SYMMETRIC_STEPS
        } else {
            (wanted as u32).max(1)
        }
    }

    /// One piece, resolved through the rotation at the middle of its own
    /// motion.
    fn symmetric_step(&self, stored: &mut Camera2D, piece: Gesture) -> bool {
        let start = *stored;
        // Seed with the rotation the pane is drawn with. It is one iteration
        // closer than zero and it costs the same.
        let mut derived = self.derived_rotation_rad(start);
        for _ in 0..MAX_MIDPOINT_ITERATIONS {
            let mut trial = Camera2D {
                rotation_rad: self.stored_rotation_rad + derived,
                ..start
            };
            piece.apply(&mut trial, self.viewport);
            let next = self.derived_rotation_rad(midpoint(start, trial));
            if next == derived {
                break;
            }
            derived = next;
        }
        let mut settled = Camera2D {
            rotation_rad: self.stored_rotation_rad + derived,
            ..start
        };
        let moved = piece.apply(&mut settled, self.viewport);
        *stored = Camera2D {
            rotation_rad: self.stored_rotation_rad,
            ..settled
        };
        moved
    }
}

/// The state halfway through a gesture: the arithmetic mean of the two centres
/// and the GEOMETRIC mean of the two scales.
///
/// The scale mean is geometric because the wheel is: `zoom_about` divides the
/// scale by a factor, so the state halfway between `s` and `s / f` in the only
/// sense the gesture knows is `s / sqrt(f)`. That is what makes a zoom of `f`
/// and a zoom of `1 / f` about the same screen point meet at the same place,
/// which is the whole reason this function exists.
/// The closest the segment `from..=to` comes to the anchor, in kilometres.
///
/// The infimum of a convex function on a segment, in closed form: project the
/// origin onto the line, clamp the parameter to the segment, measure. Used to
/// decide whether a whole gesture stays beyond the domain's downrange edge, so
/// it has to be the true minimum and not a sample of one.
fn distance_from_origin_to_segment(from: WorldPoint, to: WorldPoint) -> f64 {
    let dx = to.east_km - from.east_km;
    let dy = to.north_km - from.north_km;
    let length_squared = dx * dx + dy * dy;
    if !length_squared.is_finite() {
        return f64::NAN;
    }
    if length_squared <= 0.0 {
        return from.east_km.hypot(from.north_km);
    }
    let t = (-(from.east_km * dx + from.north_km * dy) / length_squared).clamp(0.0, 1.0);
    (from.east_km + t * dx).hypot(from.north_km + t * dy)
}

fn midpoint(start: Camera2D, end: Camera2D) -> Camera2D {
    let start = start.sanitized();
    let end = end.sanitized();
    Camera2D {
        center_east_km: 0.5 * (start.center_east_km + end.center_east_km),
        center_north_km: 0.5 * (start.center_north_km + end.center_north_km),
        km_per_point: (start.km_per_point * end.km_per_point).sqrt(),
        rotation_rad: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{
        MAX_KM_PER_POINT, MAX_SCALE_CHANGE_PER_FRAME, MIN_KM_PER_POINT, WheelNotches,
        ZoomResponder, zoom_factor_for_notches,
    };
    use map_scene::projection::{
        MAX_ROTATION_DEG, MAX_TURN_RATE_DEG_PER_KM, NORTH_UP_FULL_RANGE_KM,
    };

    /// A 1600x900 pane at one device pixel per point: the window used by the
    /// proofs and measurements, so the numbers in the doc
    /// comments and the numbers here describe the same picture.
    const PANE: ViewportMetrics = ViewportMetrics {
        width_points: 1600.0,
        height_points: 900.0,
        pixels_per_point: 1.0,
    };

    /// A wheel anchored well off the middle of the pane. The centre is the one
    /// anchor for which `zoom_about` does not move the view centre at all, so
    /// zooming there would pass this proof without proving anything; the
    /// defect was measured at a corner and so is this proof.
    const CORNER: ScreenPoint = ScreenPoint {
        x: 1500.0,
        y: 800.0,
    };

    /// How far the centre may be from where it started AFTER A WHOLE ROUND
    /// TRIP, in SCREEN POINTS.
    ///
    /// Screen points and not kilometres, because the defect was reported in
    /// screen points and because a kilometre means something different at 0.35
    /// and at 12 km per point. The old numbers in the same unit: 152.0 points
    /// for twenty wheel cycles at a 600 km centre, 94.0 at 1500 km, 236.1 in
    /// the globe band, and 37.1 to 152.7 for ten drags out and back. The
    /// control arm - the same loops with the derived rotation forced to zero,
    /// which is the behaviour before the feature - drifts 0.0000.
    ///
    /// The worst actually measured over the whole table is 0.0339 points -
    /// twenty wheel cycles about a pane corner at 6.99 km per point with the
    /// view centre 6425 km downrange, which is the CORNER OF THE DOMAIN, a
    /// hundredth of a per cent inside both its scale edge and its downrange
    /// edge at once. That is the case worth quoting: if the restriction were
    /// going to cost reversibility anywhere it would cost it there. A tenth of
    /// a point is asserted, so the margin is three times over. Either way
    /// nothing on the pane moves by a pixel over a round trip, against 236.1
    /// points before. The drag is three orders better again: 0.0000487 points,
    /// worst over the same table.
    ///
    /// What sets the residual is `f32`: `view_rotation_rad` answers in `f32`,
    /// so the rotation the forward gesture settles on and the one the reverse
    /// settles on can be an ulp or two apart, and an ulp of angle times the
    /// gesture's own span is what is left. It accumulates linearly with the
    /// number of cycles and not with anything geometric.
    const CENTRE_TOLERANCE_POINTS: f64 = 0.1;

    /// Ceiling on the turn ONE spun wheel event may produce, in degrees.
    ///
    /// ARGUED, not measured: the rotation is inside `+/- MAX_ROTATION_DEG`
    /// before the event and inside it after, so the difference cannot exceed
    /// twice that whatever the wheel does. A frame that swallows a queued
    /// backlog crosses the whole scale band, so nothing smaller is true. See
    /// `a_spun_wheel_turns_the_map_by_no_more_than_this`, which drives both
    /// the flick and the backlog case through the real `ZoomResponder`.
    const SPUN_WHEEL_TURN_CEILING_DEG: f64 = 2.0 * MAX_ROTATION_DEG;

    /// How far the scale may be from where it started, as a relative error.
    /// `zoom_factor_for_notches(-n)` is not the exact reciprocal of
    /// `zoom_factor_for_notches(n)` in `f32`, so a few ulps per cycle is the
    /// floor for any implementation, including the one before this feature.
    const SCALE_TOLERANCE: f64 = 1.0e-4;

    /// Every row of the shipped station table, as (id, latitude, longitude).
    fn shipped_sites() -> Vec<(&'static str, f64, f64)> {
        let rows: Vec<_> = crate::nearest_site::REAL_STATION_CATALOG
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut fields = line.split('\t');
                let id = fields.next().expect("id column");
                let lat: f64 = fields.next().expect("lat column").parse().expect("lat");
                let lon: f64 = fields.next().expect("lon column").parse().expect("lon");
                (id, lat, lon)
            })
            .collect();
        assert!(
            rows.len() > 200,
            "the shipped table shrank to {} rows",
            rows.len()
        );
        rows
    }

    /// Distance from a site to its own pole, as a meridian arc in kilometres.
    /// Used to aim a view centre AT the pole, which is where the rule's two
    /// new guards live.
    fn pole_gap_km(lat_deg: f64) -> f64 {
        (90.0 - lat_deg.abs()).to_radians() * 6_399.594
    }

    /// View centres worth testing from a given anchor, in radar-local
    /// kilometres: inside the surveillance floor, inside the ramp band, past
    /// it, at both edges of the domain's downrange fade and in the middle of
    /// it, well outside, and three straddling the polar hold and the ray where
    /// the north bearing wraps.
    fn probe_centres(lat_deg: f64) -> Vec<WorldPoint> {
        let pole = pole_gap_km(lat_deg);
        vec![
            WorldPoint::new(0.0, 300.0),
            WorldPoint::new(500.0, 500.0),
            WorldPoint::new(-900.0, 800.0),
            WorldPoint::new(2000.0, -1500.0),
            WorldPoint::new(0.0, NORTH_UP_FULL_RANGE_KM),
            WorldPoint::new(0.0, 0.5 * (NORTH_UP_FULL_RANGE_KM + NORTH_UP_ZERO_RANGE_KM)),
            WorldPoint::new(0.0, NORTH_UP_ZERO_RANGE_KM),
            WorldPoint::new(-4000.0, -9000.0),
            WorldPoint::new(0.0, pole - 400.0),
            WorldPoint::new(0.0, pole),
            WorldPoint::new(0.0, pole + 900.0),
        ]
    }

    /// Scales either side of every threshold this feature has: the analysis
    /// default, regional, continental, both edges of the domain's scale fade
    /// and the middle of it, the globe blend start on this pane (7.865 km per
    /// point) and past full blend.
    const PROBE_SCALES: &[f32] = &[0.35, 2.8, 4.9, 6.0, 6.99, 8.5, 12.0, 20.0];

    fn frame_at(lat: f64, lon: f64) -> NorthUpFrame {
        NorthUpFrame::new(Some(RadarProjection::new(lat, lon)), PANE, 0.0)
    }

    fn camera_at(centre: WorldPoint, km_per_point: f32) -> Camera2D {
        Camera2D {
            center_east_km: centre.east_km,
            center_north_km: centre.north_km,
            km_per_point,
            rotation_rad: 0.0,
        }
    }

    fn drift_km(start: Camera2D, end: Camera2D) -> f64 {
        (end.center_east_km - start.center_east_km)
            .hypot(end.center_north_km - start.center_north_km)
    }

    /// THE INVARIANT THIS MODULE EXISTS FOR, on the wheel.
    ///
    /// `analyst_runtime::view` names exact reversibility, on `ZOOM_PER_NOTCH`,
    /// as the reason the zoom response is geometric at all. Before the
    /// midpoint resolution twenty in-and-out cycles about a corner anchor
    /// drifted the centre 304.0 km at a 600 km centre and 2.0 km per point,
    /// and 1888.9 km in the globe band.
    #[test]
    fn a_wheel_out_and_back_lands_the_camera_where_it_started() {
        const CYCLES: usize = 20;
        let out = zoom_factor_for_notches(-3.0);
        let back = zoom_factor_for_notches(3.0);
        let mut worst_centre = 0.0f64;
        let mut worst_scale = 0.0f64;
        let mut worst_where = String::new();
        for (id, lat, lon) in shipped_sites() {
            let frame = frame_at(lat, lon);
            for centre in probe_centres(lat) {
                for &scale in PROBE_SCALES {
                    let start = camera_at(centre, scale);
                    let mut camera = frame.display_camera(start);
                    for _ in 0..CYCLES {
                        frame.resolve(
                            &mut camera,
                            Gesture::Zoom {
                                factor: out,
                                anchor: CORNER,
                            },
                        );
                        frame.resolve(
                            &mut camera,
                            Gesture::Zoom {
                                factor: back,
                                anchor: CORNER,
                            },
                        );
                    }
                    let moved = drift_km(start, camera);
                    let points = moved / f64::from(scale);
                    let scaled = (f64::from(camera.km_per_point) / f64::from(scale) - 1.0).abs();
                    if points > worst_centre {
                        worst_centre = points;
                        worst_where =
                            format!("{id} centre={centre:?} scale={scale} ({moved:.6} km)");
                    }
                    worst_scale = worst_scale.max(scaled);
                    assert!(
                        points <= CENTRE_TOLERANCE_POINTS,
                        "{id} at {centre:?}, {scale} km/point: {CYCLES} in-and-out cycles \
                         moved the centre {points:.6} screen points ({moved:.6} km)"
                    );
                    assert!(
                        scaled <= SCALE_TOLERANCE,
                        "{id} at {centre:?}, {scale} km/point: {CYCLES} in-and-out cycles \
                         changed the scale by {scaled:e}"
                    );
                }
            }
        }
        println!(
            "wheel round trip, whole station table: worst centre drift \
             {worst_centre:.9} screen points ({worst_where}), worst scale error {worst_scale:e}"
        );
    }

    /// The same invariant on the drag. Before the midpoint resolution ten
    /// drags out and ten back left the map 74.1 km from where it started at a
    /// 600 km centre and 1221.8 km in the globe band.
    #[test]
    fn a_drag_out_and_back_lands_the_camera_where_it_started() {
        const CYCLES: usize = 10;
        let mut worst_centre = 0.0f64;
        let mut worst_where = String::new();
        for (id, lat, lon) in shipped_sites() {
            let frame = frame_at(lat, lon);
            for centre in probe_centres(lat) {
                for &scale in PROBE_SCALES {
                    let start = camera_at(centre, scale);
                    let mut camera = frame.display_camera(start);
                    for _ in 0..CYCLES {
                        frame.resolve(
                            &mut camera,
                            Gesture::Pan {
                                delta_x_points: 137.0,
                                delta_y_points: -83.0,
                            },
                        );
                        frame.resolve(
                            &mut camera,
                            Gesture::Pan {
                                delta_x_points: -137.0,
                                delta_y_points: 83.0,
                            },
                        );
                    }
                    let moved = drift_km(start, camera);
                    let points = moved / f64::from(scale);
                    if points > worst_centre {
                        worst_centre = points;
                        worst_where =
                            format!("{id} centre={centre:?} scale={scale} ({moved:.6} km)");
                    }
                    assert!(
                        points <= CENTRE_TOLERANCE_POINTS,
                        "{id} at {centre:?}, {scale} km/point: {CYCLES} drags out and back \
                         moved the centre {points:.6} screen points ({moved:.6} km)"
                    );
                    assert!(
                        (f64::from(camera.km_per_point) - f64::from(scale)).abs() < 1e-9,
                        "{id}: a pan changed the scale"
                    );
                }
            }
        }
        println!(
            "drag round trip, whole station table: worst centre drift \
             {worst_centre:.9} screen points ({worst_where})"
        );
    }

    /// Keyboard flight is a gesture too: the arrow keys have to undo each
    /// other for the same reason the wheel does.
    #[test]
    fn arrow_keys_out_and_back_land_the_camera_where_they_started() {
        const CYCLES: usize = 10;
        let east = NavInput {
            pan_right: 1.0,
            ..NavInput::default()
        };
        let west = NavInput {
            pan_right: -1.0,
            ..NavInput::default()
        };
        for (id, lat, lon) in shipped_sites().into_iter().step_by(7) {
            let frame = frame_at(lat, lon);
            for centre in probe_centres(lat) {
                for &scale in PROBE_SCALES {
                    let start = camera_at(centre, scale);
                    let mut camera = frame.display_camera(start);
                    for _ in 0..CYCLES {
                        frame.resolve(
                            &mut camera,
                            Gesture::Nav {
                                input: east,
                                dt_seconds: 0.05,
                            },
                        );
                        frame.resolve(
                            &mut camera,
                            Gesture::Nav {
                                input: west,
                                dt_seconds: 0.05,
                            },
                        );
                    }
                    let points = drift_km(start, camera) / f64::from(scale);
                    assert!(
                        points <= CENTRE_TOLERANCE_POINTS,
                        "{id} at {centre:?}, {scale} km/point: {CYCLES} flights out and back \
                         moved the centre {points:.6} screen points"
                    );
                }
            }
        }
    }

    /// THE NEAR FIELD IS NOT TOUCHED, to the bit.
    ///
    /// Inside the surveillance range the rule returns an exact zero, so the
    /// midpoint resolution has to reduce to the plain camera call it wraps -
    /// not agree with it to a tolerance, BE it. `rule_is_off_throughout` is
    /// what delivers that: a gesture with no rotation anywhere along it is
    /// handed straight to `Camera2D`, so the analysis view runs exactly the
    /// arithmetic it ran before this feature existed, whatever the gesture's
    /// length.
    ///
    /// The second arm is here so the proof still says something if that fast
    /// path is ever removed: a cut-up gesture is `n` applications of a `1/n`
    /// piece and cannot be bit-identical to one whole application of anything,
    /// but it must still land in the same place.
    #[test]
    fn a_gesture_in_the_analysis_view_is_the_camera_call_it_wraps() {
        for (id, lat, lon) in shipped_sites() {
            let frame = frame_at(lat, lon);
            for centre in [
                WorldPoint::ORIGIN,
                WorldPoint::new(120.0, -80.0),
                WorldPoint::new(-250.0, 250.0),
            ] {
                for &scale in &[MIN_KM_PER_POINT, 0.35, 1.0] {
                    let start = camera_at(centre, scale);
                    for gesture in [
                        Gesture::Pan {
                            delta_x_points: 60.0,
                            delta_y_points: -40.0,
                        },
                        Gesture::Zoom {
                            factor: zoom_factor_for_notches(2.0),
                            anchor: CORNER,
                        },
                    ] {
                        let mut plain = frame.display_camera(start);
                        gesture.apply(&mut plain, PANE);
                        let mut resolved = frame.display_camera(start);
                        frame.resolve(&mut resolved, gesture);
                        assert_eq!(
                            frame.derived_rotation_rad(resolved).to_bits(),
                            0.0_f32.to_bits(),
                            "{id} at {centre:?}, {scale} km/point: the analysis view turned"
                        );
                        if frame.rule_is_off_throughout(frame.display_camera(start), gesture)
                            || frame.piece_count(start, gesture) == 1
                        {
                            assert_eq!(
                                resolved.center_east_km.to_bits(),
                                plain.center_east_km.to_bits(),
                                "{id} at {centre:?}, {scale} km/point, {gesture:?}"
                            );
                            assert_eq!(
                                resolved.center_north_km.to_bits(),
                                plain.center_north_km.to_bits(),
                                "{id} at {centre:?}, {scale} km/point, {gesture:?}"
                            );
                            assert_eq!(
                                resolved.km_per_point.to_bits(),
                                plain.km_per_point.to_bits(),
                                "{id} at {centre:?}, {scale} km/point, {gesture:?}"
                            );
                        } else {
                            let points = drift_km(plain, resolved) / f64::from(scale);
                            assert!(
                                points < 1.0e-3,
                                "{id} at {centre:?}, {scale} km/point, {gesture:?}: cut \
                                 into {} pieces it landed {points} screen points from \
                                 the whole gesture",
                                frame.piece_count(start, gesture)
                            );
                        }
                    }
                }
            }
        }
    }

    /// THE THREE STATES A REVIEW MEASURED THE OLD DEFECT AT ARE OUTSIDE THE
    /// DOMAIN, AND THERE THE GESTURE IS THE CAMERA CALL IT ALWAYS WAS.
    ///
    /// Each of these was reported as a single gesture and its inverse landing
    /// tens or hundreds of screen points from where they started, on the
    /// version of this module that resolved every gesture symmetrically and
    /// decided the fast path by probing three points on the path:
    ///
    /// * TMEM at 8.04 km per point, centre (-515.9, -14802.9): 105.15 points
    ///   on one three-notch zoom out and back. OUTSIDE ON BOTH COUNTS - the
    ///   scale is past the domain's 7 km per point edge and the centre is
    ///   14 812 km downrange, past its 6500 km one.
    /// * KEWX at 10 km per point, 6035 km due north: 102.75 points on the same
    ///   gesture. OUTSIDE ON SCALE. The centre is inside the downrange fade,
    ///   which is why the scale edge is what excludes it.
    /// * KBYX at 12.95 km per point, centre (-858.7, 6978.6): 98.74 points on
    ///   one drag out and back. OUTSIDE ON BOTH.
    ///
    /// So all three are the untouched `Camera2D` calls, and a call and its
    /// inverse compose exactly as they did before this feature existed. That
    /// is asserted on the BIT PATTERN and not to a tolerance.
    #[test]
    fn the_states_a_review_measured_the_drift_at_are_outside_the_domain() {
        let cases: [(&str, f64, f64, WorldPoint, f32); 3] = [
            (
                "TMEM",
                35.135_0,
                -89.976_1,
                WorldPoint::new(-515.9, -14_802.9),
                8.04,
            ),
            (
                "KEWX",
                29.703_9,
                -98.028_3,
                WorldPoint::new(0.0, 6_035.0),
                10.0,
            ),
            (
                "KBYX",
                24.597_5,
                -81.703_2,
                WorldPoint::new(-858.7, 6_978.6),
                12.95,
            ),
        ];
        let gestures: [(Gesture, Gesture); 2] = [
            (
                Gesture::Zoom {
                    factor: zoom_factor_for_notches(-3.0),
                    anchor: CORNER,
                },
                Gesture::Zoom {
                    factor: zoom_factor_for_notches(3.0),
                    anchor: CORNER,
                },
            ),
            (
                Gesture::Pan {
                    delta_x_points: 250.0,
                    delta_y_points: -140.0,
                },
                Gesture::Pan {
                    delta_x_points: -250.0,
                    delta_y_points: 140.0,
                },
            ),
        ];
        for (id, lat, lon, centre, scale) in cases {
            let frame = frame_at(lat, lon);
            let start = camera_at(centre, scale);
            assert_eq!(
                frame.derived_rotation_rad(start).to_bits(),
                0.0_f32.to_bits(),
                "{id} is inside the domain after all"
            );
            for (out, back) in gestures {
                // The rule is off along the whole of both legs, by the
                // interval argument and not by a probe.
                let mut plain = start;
                assert!(
                    frame.rule_is_off_throughout(plain, out),
                    "{id}: the outward leg is not wholly outside the domain"
                );
                let mut resolved = frame.display_camera(start);
                frame.resolve(&mut resolved, out);
                out.apply(&mut plain, PANE);
                assert_eq!(
                    resolved.center_east_km.to_bits(),
                    plain.center_east_km.to_bits(),
                    "{id}: the outward leg is not the camera call it wraps"
                );
                assert_eq!(
                    resolved.km_per_point.to_bits(),
                    plain.km_per_point.to_bits(),
                    "{id}: the outward leg changed the scale differently"
                );
                assert!(
                    frame.rule_is_off_throughout(plain, back),
                    "{id}: the return leg is not wholly outside the domain"
                );
                frame.resolve(&mut resolved, back);
                back.apply(&mut plain, PANE);
                assert_eq!(
                    resolved.center_east_km.to_bits(),
                    plain.center_east_km.to_bits(),
                    "{id}: the return leg is not the camera call it wraps"
                );
                let drift = drift_km(start, resolved) / f64::from(scale);
                assert!(
                    drift <= CENTRE_TOLERANCE_POINTS,
                    "{id} at {centre:?}, {scale} km/point: one gesture out and back moved \
                     the centre {drift:.6} screen points"
                );
            }
        }
    }

    /// THE FAST PATH MUST REFUSE A GESTURE THAT DIPS INTO THE DOMAIN, EVEN
    /// WHEN EVERY POINT A PROBE WOULD LOOK AT IS OUTSIDE IT.
    ///
    /// This is the shape of gesture the three-probe test got wrong, built
    /// deliberately rather than found: a drag along a line 6400 km from the
    /// anchor, which is INSIDE the domain, between two ends that are outside
    /// it - and placed so the halfway state is outside it too. The old test
    /// asked three points and would have handed the whole gesture to
    /// `Camera2D` unrotated while the rule turns the map over the middle third
    /// of it. The interval test asks the segment and refuses.
    ///
    /// Both halves are asserted, because the second is what makes the first
    /// worth something: the three probes really do all read zero here, so this
    /// is a case the old test could not have got right rather than one it
    /// happened to miss.
    #[test]
    fn the_fast_path_refuses_a_gesture_that_dips_into_the_domain() {
        const SCALE: f32 = 5.0;
        // The closest the drag's line comes to the anchor, well inside the
        // domain's outer edge, and aimed away from the anchor's own pole so
        // the polar hold is not what makes the rotation small.
        const DIP_KM: f64 = 6_000.0;
        // Asymmetric on purpose: the halfway state has to be outside the
        // domain too, and the halfway state of a symmetric pair is the dip
        // itself.
        const BEFORE_KM: f64 = 2_600.0;
        const AFTER_KM: f64 = 7_700.0;
        let frame = frame_at(41.611_66, -90.580_83);
        let azimuth = 135.0_f64.to_radians();
        let dip = WorldPoint::new(DIP_KM * azimuth.sin(), DIP_KM * azimuth.cos());
        // A unit vector along the line, perpendicular to the dip's own radius.
        let along = (azimuth.cos(), -azimuth.sin());
        let from = WorldPoint::new(
            dip.east_km - BEFORE_KM * along.0,
            dip.north_km - BEFORE_KM * along.1,
        );
        let to = WorldPoint::new(
            dip.east_km + AFTER_KM * along.0,
            dip.north_km + AFTER_KM * along.1,
        );
        let start = camera_at(from, SCALE);
        let gesture = Gesture::Pan {
            delta_x_points: ((from.east_km - to.east_km) / f64::from(SCALE)) as f32,
            delta_y_points: ((to.north_km - from.north_km) / f64::from(SCALE)) as f32,
        };
        let mut end = start;
        gesture.apply(&mut end, PANE);
        let landed = WorldPoint::new(end.center_east_km, end.center_north_km);
        assert!(
            (landed.east_km - to.east_km).hypot(landed.north_km - to.north_km) < 1.0,
            "the drag did not land where this proof needs it: {landed:?} against {to:?}"
        );

        // The three states a probe would look at are all outside the domain.
        for (name, state) in [
            ("start", start),
            ("end", end),
            ("midpoint", midpoint(start, end)),
        ] {
            assert_eq!(
                frame.derived_rotation_rad(state).to_bits(),
                0.0_f32.to_bits(),
                "the {name} probe is inside the domain, so this proof is not measuring \
                 what it claims"
            );
        }
        // And the rule really does turn the map in between.
        let turned = f64::from(frame.derived_rotation_rad(camera_at(dip, SCALE))).to_degrees();
        assert!(
            turned.abs() > 1.0,
            "the middle of the segment is only turned {turned:.4} degrees, so this proof \
             is not measuring what it claims"
        );
        assert!(
            !frame.rule_is_off_throughout(start, gesture),
            "the fast path took a gesture that passes through {turned:.4} degrees of \
             rotation"
        );
    }

    /// THE TWO FACTS THE INTERVAL ARGUMENT RESTS ON.
    ///
    /// `rule_is_off_throughout` reasons about the SEGMENT between a gesture's
    /// endpoints rather than probing points on the path, and that is only
    /// sound because the path IS that segment. Both halves are properties of
    /// `Camera2D` and neither is obvious, so both are pinned here: a keyboard
    /// zoom is anchored on the pane's own centre and must not move the view
    /// centre at all, and a wheel zoom about any anchor must move it along the
    /// straight line between where it started and where it finished.
    #[test]
    fn a_gesture_keeps_its_view_centre_on_the_segment_between_its_ends() {
        let start = camera_at(WorldPoint::new(1_500.0, -900.0), 3.0);

        // A keyboard zoom does not move the view centre.
        let mut flown = start;
        Gesture::Nav {
            input: NavInput {
                zoom_steps: 2.0,
                ..NavInput::default()
            },
            dt_seconds: 0.05,
        }
        .apply(&mut flown, PANE);
        assert_ne!(
            flown.km_per_point.to_bits(),
            start.km_per_point.to_bits(),
            "the keyboard zoom did nothing, so this proves nothing"
        );
        let moved_km = drift_km(start, flown);
        assert!(
            moved_km < 1.0e-6,
            "a keyboard zoom moved the view centre {moved_km} km"
        );

        // A wheel zoom about a corner moves it along a straight segment: every
        // fraction of the gesture lands on the line between the two ends.
        for factor in [zoom_factor_for_notches(-3.0), zoom_factor_for_notches(4.0)] {
            let whole = Gesture::Zoom {
                factor,
                anchor: CORNER,
            };
            let mut end = start;
            whole.apply(&mut end, PANE);
            assert!(
                drift_km(start, end) > 100.0,
                "the corner-anchored zoom barely moved the centre, so this proves nothing"
            );
            let mut walked = start;
            let piece = whole.fraction(16);
            let mut worst_off_line = 0.0f64;
            for _ in 0..16 {
                piece.apply(&mut walked, PANE);
                let dx = end.center_east_km - start.center_east_km;
                let dy = end.center_north_km - start.center_north_km;
                let px = walked.center_east_km - start.center_east_km;
                let py = walked.center_north_km - start.center_north_km;
                let length = dx.hypot(dy).max(1e-9);
                worst_off_line = worst_off_line.max((px * dy - py * dx).abs() / length);
            }
            assert!(
                worst_off_line < 1.0e-6,
                "a wheel zoom's centre track bowed {worst_off_line} km off the segment \
                 between its two ends"
            );
        }
    }

    /// THE PAN-TURN CEILING, CORROBORATED OVER THE WHOLE SHIPPED TABLE.
    ///
    /// The ceiling itself is an ARGUMENT, derived in "How fast the map can
    /// turn" on `RadarProjection::view_rotation_rad` from the convergence
    /// gradient, the smoothstep slopes and the transverse stretch, all of
    /// which the domain holds away from their singularities. This test cannot
    /// establish it and does not try to; what it does is fail loudly if the
    /// argument is wrong, and print the worst it found so the margin is
    /// visible.
    ///
    /// That distinction is the whole history of this number. It was once
    /// 0.0269 deg/km, a sample from one mid-latitude anchor, and the truth on
    /// the same rule was 107.9 degrees per 280 km near a pole. It was then
    /// 0.3180, the worst of 1 585 656 centres, and the truth was 0.9132
    /// against the globe's limb, which that grid never reached. A third grid
    /// would have been the same mistake again.
    ///
    /// The sweep is still the widest one that can be run: every site in the
    /// table, a ladder of downrange distances AND radii placed relative to
    /// each site's OWN pole - the polar hold and its ramp live at a fixed
    /// COLATITUDE, so they sit at a different downrange distance from every
    /// site and a fixed ladder walks straight past them - crossed with scales
    /// either side of the domain's scale fade.
    #[test]
    fn the_pan_turn_rate_stays_under_its_documented_bound() {
        const SCALES: &[f32] = &[0.35, 1.6, 2.8, 4.9, 5.5, 6.5, 6.99, 7.0, 8.5, 12.0, 20.0];
        const RADII_KM: &[f64] = &[
            461.0, 470.0, 600.0, 900.0, 1200.0, 2000.0, 3000.0, 4000.0, 4900.0, 5000.0, 5100.0,
            5750.0, 6400.0, 6499.0, 6500.0, 6501.0, 8000.0, 12000.0, 18000.0,
        ];
        const POLE_RELATIVE_KM: &[f64] = &[
            -2600.0, -2000.0, -1500.0, -1000.0, -700.0, -400.0, -150.0, 0.0, 150.0, 400.0, 700.0,
            1000.0, 1500.0, 2000.0, 2600.0, 3500.0,
        ];
        const STEP_KM: f64 = 1.0;
        let mut worst_rate = 0.0f64;
        let mut worst_screen = 0.0f64;
        let mut worst_where = String::new();
        let mut sampled = 0u64;
        let mut reached_the_domain = false;
        for (id, lat, lon) in shipped_sites() {
            let projection = RadarProjection::new(lat, lon);
            for &scale in SCALES {
                let pole = pole_gap_km(lat);
                let radii: Vec<f64> = RADII_KM
                    .iter()
                    .copied()
                    .chain(
                        POLE_RELATIVE_KM
                            .iter()
                            .map(|offset| pole + offset)
                            .filter(|radius| *radius > 460.0),
                    )
                    .collect();
                for &radius in &radii {
                    for step in 0..36 {
                        let azimuth = f64::from(step) * 10.0f64.to_radians();
                        let centre =
                            WorldPoint::new(radius * azimuth.sin(), radius * azimuth.cos());
                        sampled += 1;
                        if projection.view_rotation_rad(centre, scale) != 0.0 {
                            reached_the_domain = true;
                        }
                        let mut gradient = [0.0f64; 2];
                        for (axis, slot) in gradient.iter_mut().enumerate() {
                            let mut low = centre;
                            let mut high = centre;
                            if axis == 0 {
                                low.east_km -= STEP_KM;
                                high.east_km += STEP_KM;
                            } else {
                                low.north_km -= STEP_KM;
                                high.north_km += STEP_KM;
                            }
                            let a = f64::from(projection.view_rotation_rad(low, scale));
                            let b = f64::from(projection.view_rotation_rad(high, scale));
                            *slot = shortest_turn(b - a) / (2.0 * STEP_KM);
                        }
                        let rate = gradient[0].hypot(gradient[1]).to_degrees();
                        let per_point = rate * f64::from(scale);
                        if rate > worst_rate {
                            worst_rate = rate;
                            worst_screen = per_point;
                            worst_where =
                                format!("{id} radius={radius} azimuth={} scale={scale}", step * 10);
                        }
                        assert!(
                            rate <= MAX_TURN_RATE_DEG_PER_KM,
                            "{id} at radius {radius} km, azimuth {}, {scale} km/point: \
                             the map turns {rate:.4} degrees per kilometre of pan, over the \
                             argued ceiling of {MAX_TURN_RATE_DEG_PER_KM}",
                            step * 10
                        );
                    }
                }
            }
        }
        assert!(
            reached_the_domain,
            "the sweep never entered the domain, so it corroborates nothing"
        );
        println!(
            "pan-turn rate over {sampled} view centres on the whole station table: \
             worst {worst_rate:.4} deg/km, {worst_screen:.4} deg per screen point, at \
             {worst_where}; the argued ceiling is {MAX_TURN_RATE_DEG_PER_KM}"
        );
    }

    /// The rule has no jump anywhere on the ray where the north bearing wraps.
    ///
    /// That ray - the one where true north is drawn straight down, which for a
    /// northern anchor is everything past its own pole - is where the shipped
    /// rule flipped the map by a whole turn times whatever fraction was being
    /// applied. It is still INSIDE the domain for the northern anchors, which
    /// is why the half-turn fade is still needed: a view centre 4000 km due
    /// north of a 65N radar is 1200 km past its own pole at a scale a
    /// continental view is drawn at.
    #[test]
    fn crossing_the_ray_where_the_north_bearing_wraps_is_not_a_jump() {
        /// One step of the walk. `MAX_TURN_RATE_DEG_PER_KM` says the map can
        /// turn at most 0.75 degrees per kilometre, so a step this long can
        /// carry 3.0 degrees; a wrap would carry a hundred or more.
        const STEP_KM: f64 = 4.0;
        const HALF_WIDTH_KM: f64 = 1_800.0;
        let mut worst = 0.0f64;
        let mut worst_where = String::new();
        let mut worst_rotation_seen = 0.0f64;
        let mut closest_to_the_ray_deg = f64::INFINITY;
        for (id, lat, lon) in shipped_sites().into_iter().step_by(5) {
            let projection = RadarProjection::new(lat, lon);
            let pole = pole_gap_km(lat);
            for &scale in &[2.8f32, 6.5] {
                for beyond in [1_500.0f64, 2_500.0, 3_500.0] {
                    let radius = pole + beyond;
                    let mut previous: Option<f64> = None;
                    let mut east = -HALF_WIDTH_KM;
                    while east <= HALF_WIDTH_KM {
                        let centre = WorldPoint::new(east, radius);
                        let here = f64::from(projection.view_rotation_rad(centre, scale));
                        worst_rotation_seen = worst_rotation_seen.max(here.abs().to_degrees());
                        if let Some(gamma) = projection.north_bearing_rad(centre) {
                            closest_to_the_ray_deg =
                                closest_to_the_ray_deg.min(180.0 - gamma.to_degrees().abs());
                        }
                        if let Some(before) = previous {
                            let jump = shortest_turn(here - before).abs().to_degrees();
                            if jump > worst {
                                worst = jump;
                                worst_where =
                                    format!("{id} beyond={beyond} east={east} scale={scale}");
                            }
                            assert!(
                                jump < STEP_KM * MAX_TURN_RATE_DEG_PER_KM,
                                "{id}, {beyond} km past its pole at {scale} km/point, east \
                                 {east}: {STEP_KM} km of pan turned the map {jump:.4} degrees"
                            );
                        }
                        previous = Some(here);
                        east += STEP_KM;
                    }
                }
            }
        }
        // The walk has to pass through ground the rule really acts on, or the
        // continuity it proves is the continuity of a constant zero. Past the
        // pole and off the anchor's meridian is exactly where the wrap ray is
        // and where the rotation is deepest.
        assert!(
            worst_rotation_seen > 30.0,
            "the deepest rotation on the walk was {worst_rotation_seen:.4} degrees, so this \
             never crossed ground the rule acts on"
        );
        assert!(
            closest_to_the_ray_deg < 2.0,
            "the walk never came closer than {closest_to_the_ray_deg:.4} degrees to the ray \
             where the bearing wraps, so it did not cross it"
        );
        println!(
            "worst turn per {STEP_KM} km of pan across the wrap ray inside the domain: \
             {worst:.6} degrees ({worst_where}); deepest rotation on the walk \
             {worst_rotation_seen:.2} degrees, closest approach to the ray \
             {closest_to_the_ray_deg:.4} degrees"
        );
    }

    /// WHAT A SPUN WHEEL DOES TO THE SCALE-BAND UNWIND, INCLUDING THE CASE
    /// THAT HAS NO SMALLER BOUND.
    ///
    /// One deliberate notch is `ZOOM_PER_NOTCH`; a flick earns up to
    /// `MAX_BURST_GAIN`, so ONE input event is worth `1.2^5 = 2.49`; and a
    /// frame that swallows a QUEUED BACKLOG is capped by
    /// `MAX_SCALE_CHANGE_PER_FRAME` at a decade of scale, which is wider than
    /// the domain's whole 1.4 scale band. That last case is the reason no
    /// bound of the form "a notch unwinds part of it" exists, and an earlier
    /// version of this test named it in exactly those words while only ever
    /// producing five single detents - so the ceiling it asserted did not
    /// cover the case its own prose said it was there for. Driving the real
    /// responder at the backlog rate turned the map 93.35 degrees, over that
    /// ceiling.
    ///
    /// Both rates are driven here, in both directions, through the real
    /// `ZoomResponder`, with the wheel anchored on a pane CORNER because that
    /// is where the view centre moves furthest and the turn is largest. The
    /// ceiling asserted is the argued one: the rotation is inside
    /// `+/- MAX_ROTATION_DEG` before the event and after it, so no event can
    /// turn the map by more than twice that.
    #[test]
    fn a_spun_wheel_turns_the_map_by_no_more_than_this() {
        let mut worst_turn = 0.0f64;
        let mut worst_where = String::new();
        let mut worst_backlog = 0.0f64;
        let mut saturated_the_frame_cap = false;
        for (id, lat, lon) in shipped_sites() {
            let frame = frame_at(lat, lon);
            for centre in probe_centres(lat) {
                for &scale in &[2.8f32, 4.0, 4.9, 5.5, 6.0, 6.99, 7.9, 8.5, 11.0, 15.0] {
                    for &direction in &[-1.0f32, 1.0] {
                        let start = camera_at(centre, scale);
                        let before = frame.derived_rotation_rad(start);
                        // (label, the wheel that produced it)
                        for (label, factor) in [
                            ("flick", burst_factor(WheelNotches::detented(direction), 5)),
                            ("backlog", backlog_factor(direction)),
                        ] {
                            if label == "backlog" {
                                let unclamped = (f64::from(factor)).abs();
                                if (unclamped - f64::from(MAX_SCALE_CHANGE_PER_FRAME)).abs() < 1e-3
                                    || (unclamped - 1.0 / f64::from(MAX_SCALE_CHANGE_PER_FRAME))
                                        .abs()
                                        < 1e-6
                                {
                                    saturated_the_frame_cap = true;
                                }
                            }
                            let mut camera = frame.display_camera(start);
                            frame.resolve(
                                &mut camera,
                                Gesture::Zoom {
                                    factor,
                                    anchor: CORNER,
                                },
                            );
                            let after = frame.derived_rotation_rad(Camera2D {
                                rotation_rad: 0.0,
                                ..camera
                            });
                            let turn = f64::from(after - before).abs().to_degrees();
                            if label == "backlog" {
                                worst_backlog = worst_backlog.max(turn);
                            }
                            if turn > worst_turn {
                                worst_turn = turn;
                                worst_where = format!(
                                    "{id} {label} centre={centre:?} scale={scale} -> {} \
                                     km/point, {:.2} to {:.2} deg",
                                    camera.km_per_point,
                                    f64::from(before).to_degrees(),
                                    f64::from(after).to_degrees()
                                );
                            }
                            assert!(
                                turn <= SPUN_WHEEL_TURN_CEILING_DEG,
                                "{id} at {centre:?}, {scale} km/point: one {label} wheel \
                                 event turned the map {turn:.4} degrees, over the argued \
                                 ceiling of {SPUN_WHEEL_TURN_CEILING_DEG}"
                            );
                        }
                    }
                }
            }
        }
        assert!(
            saturated_the_frame_cap,
            "the backlog arm never reached MAX_SCALE_CHANGE_PER_FRAME, so it is not the \
             case this test says it covers"
        );
        assert!(
            worst_backlog > 0.0,
            "the backlog arm never turned the map at all"
        );
        println!(
            "worst turn from ONE spun wheel event: {worst_turn:.4} degrees ({worst_where}); \
             the backlog arm alone reached {worst_backlog:.4}, against an argued ceiling of \
             {SPUN_WHEEL_TURN_CEILING_DEG}"
        );
    }

    /// A flick: `detents` wheel events inside `BURST_MEMORY_SECONDS`, which is
    /// what the responder's burst gain is there to notice.
    fn burst_factor(notches: WheelNotches, detents: u32) -> f32 {
        let mut responder = ZoomResponder::new();
        let mut factor = 1.0f32;
        for tick in 0..detents {
            factor = responder.factor(notches, f64::from(tick) * 0.02);
        }
        factor
    }

    /// A FRAME THAT SWALLOWED A QUEUED BACKLOG: three detents' worth of wheel
    /// arriving at a time, repeatedly, at the rate `analyst_runtime::view`
    /// documents as the one that saturates `MAX_SCALE_CHANGE_PER_FRAME`.
    fn backlog_factor(direction: f32) -> f32 {
        let mut responder = ZoomResponder::new();
        let mut factor = 1.0f32;
        for tick in 0..6 {
            factor = responder.factor(
                WheelNotches::detented(direction * 3.0),
                f64::from(tick) * 0.04,
            );
        }
        factor
    }

    /// The scale rails are the one place a gesture is not its own inverse, and
    /// that predates this module: `zoom_about` clamps the FACTOR to what the
    /// limits can honour, so a notch into the wall does nothing and the notch
    /// back out is a real notch. Pinned so nobody reads the round-trip proofs
    /// above as a claim about the rails.
    #[test]
    fn the_scale_rails_are_still_a_wall_and_not_a_hinge() {
        let frame = frame_at(41.611_66, -90.580_83);
        let start = camera_at(WorldPoint::new(0.0, 1500.0), MAX_KM_PER_POINT);
        let mut camera = frame.display_camera(start);
        frame.resolve(
            &mut camera,
            Gesture::Zoom {
                factor: zoom_factor_for_notches(-3.0),
                anchor: CORNER,
            },
        );
        assert_eq!(
            camera.km_per_point, MAX_KM_PER_POINT,
            "a notch into the ceiling changed the scale"
        );
    }

    /// A gesture too long for one contraction is cut up, and the pieces still
    /// compose to the original: `MAX_SYMMETRIC_STEP_KM` is a numerical device
    /// and must not become a behaviour.
    #[test]
    fn cutting_a_long_gesture_into_pieces_does_not_change_where_it_lands() {
        let frame = NorthUpFrame::new(None, PANE, 0.0);
        let start = camera_at(WorldPoint::new(0.0, 2000.0), 8.0);
        for delta in [10.0f32, 137.0, 400.0, 900.0] {
            let mut whole = frame.display_camera(start);
            Gesture::Pan {
                delta_x_points: delta,
                delta_y_points: -delta,
            }
            .apply(&mut whole, PANE);
            let mut pieces = frame.display_camera(start);
            let cut = Gesture::Pan {
                delta_x_points: delta,
                delta_y_points: -delta,
            }
            .fraction(8);
            for _ in 0..8 {
                cut.apply(&mut pieces, PANE);
            }
            assert!(
                drift_km(whole, pieces) < 1.0e-6,
                "{delta} points: eight pieces landed {} km from one whole",
                drift_km(whole, pieces)
            );
        }
    }

    /// THE PIN THE CEILING'S DOC NAMES HAS TO KEEP ITS NAME.
    ///
    /// `map_scene::projection::MAX_TURN_RATE_DEG_PER_KM` sends a reader here
    /// by name, and its doc used to send them to an integration-test file that
    /// has never existed. A doc string cannot be type-checked, so this is the
    /// other half of that pin: naming the function as an ITEM means the build
    /// breaks if it is ever renamed, and `map_scene`'s own
    /// `every_path_and_site_claim_in_this_file_is_true` checks that the doc
    /// still spells it the same way.
    #[test]
    fn the_bounds_documentation_names_a_test_that_exists() {
        let named: fn() = the_pan_turn_rate_stays_under_its_documented_bound;
        assert!(
            std::mem::size_of_val(&named) > 0,
            "a function pointer is not zero sized"
        );
    }

    /// Wrap a difference of two angles into (-pi, pi]. A rotation of `+pi` and
    /// one of `-pi` are the same picture, so a raw subtraction would report a
    /// full turn where nothing moved.
    fn shortest_turn(mut difference: f64) -> f64 {
        while difference > std::f64::consts::PI {
            difference -= 2.0 * std::f64::consts::PI;
        }
        while difference <= -std::f64::consts::PI {
            difference += 2.0 * std::f64::consts::PI;
        }
        difference
    }
}
