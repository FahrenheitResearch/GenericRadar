use serde::{Deserialize, Serialize};

use crate::Generation;

pub const DEFAULT_KM_PER_POINT: f32 = 0.35;
pub const MIN_KM_PER_POINT: f32 = 0.01;
pub const MAX_KM_PER_POINT: f32 = 50.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenPoint {
    pub x: f32,
    pub y: f32,
}

impl ScreenPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorldPoint {
    pub east_km: f64,
    pub north_km: f64,
}

impl WorldPoint {
    pub const ORIGIN: Self = Self {
        east_km: 0.0,
        north_km: 0.0,
    };

    pub const fn new(east_km: f64, north_km: f64) -> Self {
        Self { east_km, north_km }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportMetrics {
    pub width_points: f32,
    pub height_points: f32,
    pub pixels_per_point: f32,
}

impl ViewportMetrics {
    pub fn sanitized(self) -> Self {
        Self {
            width_points: finite_positive(self.width_points, 1.0),
            height_points: finite_positive(self.height_points, 1.0),
            pixels_per_point: finite_positive(self.pixels_per_point, 1.0),
        }
    }

    pub fn center(self) -> ScreenPoint {
        let metrics = self.sanitized();
        ScreenPoint::new(metrics.width_points * 0.5, metrics.height_points * 0.5)
    }
}

/// Serializable camera intent in radar-local world kilometres.
///
/// `rotation_rad` is clockwise screen rotation. At zero rotation, east is
/// right and north is up.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera2D {
    pub center_east_km: f64,
    pub center_north_km: f64,
    pub km_per_point: f32,
    pub rotation_rad: f32,
}

impl Default for Camera2D {
    fn default() -> Self {
        Self {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: DEFAULT_KM_PER_POINT,
            rotation_rad: 0.0,
        }
    }
}

impl Camera2D {
    pub fn sanitized(self) -> Self {
        Self {
            center_east_km: finite_f64(self.center_east_km, 0.0),
            center_north_km: finite_f64(self.center_north_km, 0.0),
            km_per_point: finite_positive(self.km_per_point, DEFAULT_KM_PER_POINT)
                .clamp(MIN_KM_PER_POINT, MAX_KM_PER_POINT),
            rotation_rad: if self.rotation_rad.is_finite() {
                normalize_angle(self.rotation_rad)
            } else {
                0.0
            },
        }
    }

    pub fn world_to_screen(self, world: WorldPoint, viewport: ViewportMetrics) -> ScreenPoint {
        let camera = self.sanitized();
        let viewport = viewport.sanitized();
        let center = viewport.center();
        let dx = (world.east_km - camera.center_east_km) as f32;
        let dy = (world.north_km - camera.center_north_km) as f32;
        let (sin, cos) = camera.rotation_rad.sin_cos();
        let screen_x_km = cos * dx + sin * dy;
        let screen_y_km = sin * dx - cos * dy;
        ScreenPoint {
            x: center.x + screen_x_km / camera.km_per_point,
            y: center.y + screen_y_km / camera.km_per_point,
        }
    }

    pub fn screen_to_world(self, screen: ScreenPoint, viewport: ViewportMetrics) -> WorldPoint {
        let camera = self.sanitized();
        let viewport = viewport.sanitized();
        let center = viewport.center();
        let screen_x_km = (screen.x - center.x) * camera.km_per_point;
        let screen_y_km = (screen.y - center.y) * camera.km_per_point;
        let (sin, cos) = camera.rotation_rad.sin_cos();
        // The forward 2x2 matrix is its own inverse.
        let dx = cos * screen_x_km + sin * screen_y_km;
        let dy = sin * screen_x_km - cos * screen_y_km;
        WorldPoint {
            east_km: camera.center_east_km + f64::from(dx),
            north_km: camera.center_north_km + f64::from(dy),
        }
    }

    /// Move map content by a screen-space drag delta.
    ///
    /// The delta comes straight from the pointer, so it is whatever the window
    /// system said it was. A non-finite one is discarded WHOLE -- a drag is one
    /// two-dimensional quantity, and honouring the axis that survived would
    /// slide the map in a direction the analyst did not drag. A merely enormous
    /// delta is finite on the way in but can still overflow
    /// `delta * km_per_point` to infinity, and the rotation then turns
    /// `inf - inf` into NaN, so the result is checked as well as the input. A
    /// drag the arithmetic cannot express is a drag that does not happen: the
    /// camera stays exactly where it was, which is the one outcome the analyst
    /// can undo.
    pub fn pan_by_screen_delta(&mut self, delta_x_points: f32, delta_y_points: f32) {
        let camera = self.sanitized();
        let (delta_x_points, delta_y_points) =
            if delta_x_points.is_finite() && delta_y_points.is_finite() {
                (delta_x_points, delta_y_points)
            } else {
                (0.0, 0.0)
            };
        let (sin, cos) = camera.rotation_rad.sin_cos();
        let screen_x_km = delta_x_points * camera.km_per_point;
        let screen_y_km = delta_y_points * camera.km_per_point;
        let world_dx = cos * screen_x_km + sin * screen_y_km;
        let world_dy = sin * screen_x_km - cos * screen_y_km;
        let east = camera.center_east_km - f64::from(world_dx);
        let north = camera.center_north_km - f64::from(world_dy);
        *self = camera;
        if east.is_finite() && north.is_finite() {
            self.center_east_km = east;
            self.center_north_km = north;
        }
    }

    /// Zoom about a screen point while preserving the world coordinate under
    /// that point. `factor > 1` zooms in.
    ///
    /// The anchor may legitimately sit outside the viewport -- a drag can carry
    /// the pointer off the edge -- so it is not clamped to the pane, only
    /// required to be a number. The scale clamp keeps `km_per_point` in range
    /// on its own; the centre needs its own check because the correction is a
    /// difference of two world points and an anchor far enough out overflows
    /// `(anchor - centre) * km_per_point` to infinity, which the rotation then
    /// turns into NaN. An unrepresentable correction is skipped rather than
    /// written, so the scale still changes and the camera stays finite.
    pub fn zoom_about(&mut self, factor: f32, anchor: ScreenPoint, viewport: ViewportMetrics) {
        let anchor = finite_screen(anchor, viewport.center());
        let before = self.screen_to_world(anchor, viewport);
        let current = self.sanitized();
        let factor = finite_positive(factor, 1.0);
        // Clamp the FACTOR to what the scale limits can honour, not just the
        // result. With only the result clamped, a notch at the ceiling still
        // ran the anchor correction against a scale that had not moved, and
        // scrolling into the wall accumulated visible drift - measured on the
        // real site catalogue as markers walking thousands of screen points
        // off after an over-scrolled round trip. With the factor clamped, a
        // notch the limits cannot honour leaves the camera exactly as it was.
        let factor = factor.clamp(
            current.km_per_point / MAX_KM_PER_POINT,
            current.km_per_point / MIN_KM_PER_POINT,
        );
        self.km_per_point =
            (current.km_per_point / factor).clamp(MIN_KM_PER_POINT, MAX_KM_PER_POINT);
        self.center_east_km = current.center_east_km;
        self.center_north_km = current.center_north_km;
        self.rotation_rad = current.rotation_rad;
        let after = self.screen_to_world(anchor, viewport);
        let east = self.center_east_km + (before.east_km - after.east_km);
        let north = self.center_north_km + (before.north_km - after.north_km);
        if east.is_finite() && north.is_finite() {
            self.center_east_km = east;
            self.center_north_km = north;
        }
    }

    /// Rotate about a screen point while preserving its world coordinate.
    pub fn rotate_about(
        &mut self,
        rotation_rad: f32,
        anchor: ScreenPoint,
        viewport: ViewportMetrics,
    ) {
        // Same anchor and same correction as `zoom_about`, so the same two
        // guards: see there for why the result is checked and not the input.
        let anchor = finite_screen(anchor, viewport.center());
        let before = self.screen_to_world(anchor, viewport);
        let current = self.sanitized();
        self.center_east_km = current.center_east_km;
        self.center_north_km = current.center_north_km;
        self.km_per_point = current.km_per_point;
        self.rotation_rad = if rotation_rad.is_finite() {
            normalize_angle(rotation_rad)
        } else {
            current.rotation_rad
        };
        let after = self.screen_to_world(anchor, viewport);
        let east = self.center_east_km + (before.east_km - after.east_km);
        let north = self.center_north_km + (before.north_km - after.north_km);
        if east.is_finite() && north.is_finite() {
            self.center_east_km = east;
            self.center_north_km = north;
        }
    }

    /// Fly the camera for one frame of keyboard navigation.
    ///
    /// Returns whether the camera moved, so the caller can skip the work a
    /// changed camera implies (re-render, relink, LOD) on an idle frame.
    pub fn apply_nav(&mut self, nav: NavInput, dt_seconds: f32, viewport: ViewportMetrics) -> bool {
        if nav.reset {
            // Reset wins outright: it is the key the analyst reaches for when
            // they are lost, and a half-applied pan on the way out would be one
            // more thing to undo.
            *self = Self::default();
            return true;
        }
        let viewport = viewport.sanitized();
        let dt = if dt_seconds.is_finite() {
            dt_seconds.clamp(0.0, MAX_NAV_STEP_SECONDS)
        } else {
            0.0
        };
        let mut changed = false;

        let pan_right = finite_or_zero(nav.pan_right).clamp(-1.0, 1.0);
        let pan_up = finite_or_zero(nav.pan_up).clamp(-1.0, 1.0);
        if dt > 0.0 && (pan_right != 0.0 || pan_up != 0.0) {
            // Fractions of the SHORTER edge, so one key press covers the same
            // fraction of the picture in a tall pane and a wide one, and a
            // four-pane layout does not pan at four different speeds.
            let span = viewport.width_points.min(viewport.height_points);
            let step = span * KEY_PAN_FRACTION_PER_SECOND * dt;
            // `pan_by_screen_delta` moves CONTENT the way a drag would, so
            // flying the view east means dragging the content west.
            self.pan_by_screen_delta(-pan_right * step, pan_up * step);
            changed = true;
        }

        let hold = finite_or_zero(nav.zoom_hold).clamp(-1.0, 1.0);
        let steps =
            finite_or_zero(nav.zoom_steps).clamp(-MAX_NOTCHES_PER_STEP, MAX_NOTCHES_PER_STEP);
        if hold != 0.0 || steps != 0.0 {
            let held = if dt > 0.0 && hold != 0.0 {
                KEY_ZOOM_RATE_PER_SECOND.powf(hold * dt)
            } else {
                1.0
            };
            // Same per-frame ceiling the wheel gets: a stall can queue key
            // presses exactly as it queues detents.
            let factor = (held * zoom_factor_for_notches(steps))
                .clamp(1.0 / MAX_SCALE_CHANGE_PER_FRAME, MAX_SCALE_CHANGE_PER_FRAME);
            if factor != 1.0 {
                // The keyboard has no pointer, so the pane centre is the
                // anchor. It is the one point the analyst can keep their eye on
                // without moving the mouse, and it makes a keyboard zoom the
                // exact inverse of itself the way a wheel zoom is.
                self.zoom_about(factor, viewport.center(), viewport);
                changed = true;
            }
        }
        changed
    }

    pub fn radar_raster_view(self, viewport: ViewportMetrics) -> RasterView {
        let camera = self.sanitized();
        let viewport = viewport.sanitized();
        let radar = camera.world_to_screen(WorldPoint::ORIGIN, viewport);
        let width_px = (viewport.width_points * viewport.pixels_per_point)
            .round()
            .max(1.0) as u32;
        let height_px = (viewport.height_points * viewport.pixels_per_point)
            .round()
            .max(1.0) as u32;
        let km_per_px = camera.km_per_point / viewport.pixels_per_point;
        RasterView {
            width_px,
            height_px,
            radar_x_px: radar.x * viewport.pixels_per_point,
            radar_y_px: radar.y * viewport.pixels_per_point,
            km_per_px,
            rotation_rad: camera.rotation_rad,
        }
    }
}

/// Renderer-neutral viewport contract for the radar raster worker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterView {
    pub width_px: u32,
    pub height_px: u32,
    pub radar_x_px: f32,
    pub radar_y_px: f32,
    pub km_per_px: f32,
    pub rotation_rad: f32,
}

/// Half-octave geometry LOD bucket. Exact camera scale is intentionally not a
/// geometry cache key.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LodBucket(pub i16);

impl LodBucket {
    pub fn ideal(km_per_point: f32, reference_km_per_point: f32) -> Self {
        let scale = finite_positive(km_per_point, DEFAULT_KM_PER_POINT);
        let reference = finite_positive(reference_km_per_point, DEFAULT_KM_PER_POINT);
        let half_octaves = (scale / reference).log2() * 2.0;
        Self(half_octaves.floor().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
    }

    pub fn center_scale(self, reference_km_per_point: f32) -> f32 {
        let reference = finite_positive(reference_km_per_point, DEFAULT_KM_PER_POINT);
        reference * 2.0_f32.powf(f32::from(self.0) * 0.5)
    }
}

/// Stateful LOD selector with hysteresis so small wheel/trackpad deltas do not
/// repeatedly rebuild scene geometry around a bucket boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodSelector {
    current: LodBucket,
    reference_km_per_point: f32,
    hysteresis_fraction: f32,
}

impl LodSelector {
    pub fn new(km_per_point: f32, reference_km_per_point: f32) -> Self {
        let reference = finite_positive(reference_km_per_point, DEFAULT_KM_PER_POINT);
        Self {
            current: LodBucket::ideal(km_per_point, reference),
            reference_km_per_point: reference,
            hysteresis_fraction: 0.12,
        }
    }

    pub const fn current(self) -> LodBucket {
        self.current
    }

    pub fn update(&mut self, km_per_point: f32) -> LodBucket {
        let scale = finite_positive(km_per_point, DEFAULT_KM_PER_POINT);
        let hysteresis = self.hysteresis_fraction.clamp(0.0, 0.45);
        loop {
            let center = self.current.center_scale(self.reference_km_per_point);
            let upper = center * 2.0_f32.sqrt() * (1.0 + hysteresis);
            let lower = center / 2.0_f32.sqrt() * (1.0 - hysteresis);
            if scale > upper && self.current.0 < i16::MAX {
                self.current.0 += 1;
            } else if scale < lower && self.current.0 > i16::MIN {
                self.current.0 -= 1;
            } else {
                break;
            }
        }
        self.current
    }
}

/// Retained geometry identity. Camera translation and exact camera scale do
/// not appear here by design.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GeometryCacheKey {
    pub dataset: Generation,
    pub projection: Generation,
    pub style: Generation,
    pub lod: LodBucket,
}

fn finite_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

/// Zero is the identity for every navigation quantity -- a pan of zero, a
/// zoom of zero notches -- so it is the right answer for garbage input.
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// A cursor position that is at least a number.
///
/// Both coordinates or neither: half a fallback anchor would zoom about a point
/// the analyst never pointed at, which is harder to explain than zooming about
/// the middle of the pane.
fn finite_screen(point: ScreenPoint, fallback: ScreenPoint) -> ScreenPoint {
    if point.x.is_finite() && point.y.is_finite() {
        point
    } else {
        fallback
    }
}

fn finite_f64(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

fn normalize_angle(angle: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    (angle + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI
}

// ---------------------------------------------------------------------------
// Navigation response: how far one wheel notch, or one held key, moves the
// camera.
// ---------------------------------------------------------------------------

/// Scale change one deliberate wheel notch applies.
///
/// The response is GEOMETRIC -- a notch multiplies the scale rather than adding
/// to it -- for two reasons. It is scale-free, so a notch feels the same at
/// 40 km/point as at 0.05 km/point; and it is exactly reversible, so a notch
/// out undoes a notch in and the camera lands back where it started. That
/// reversibility is what makes a wheel feel like it obeys the analyst.
///
/// The response this replaced was additive: `1.0 + smooth_scroll_delta / 600.0`.
/// With the egui default `line_scroll_speed` of 40 points per notch that made
/// one notch a factor of 1.0667, so a continental 4 km/point down to a
/// street-level 0.1 km/point -- a factor of 40 -- took ln(40)/ln(1.0667) = 58
/// notches. It was not reversible either: 1.0667 * 0.9333 = 0.9956, so every
/// in-and-out pair drifted the scale half a percent.
///
/// The size is set by the SLOW end, not the fast one. A lone notch is what an
/// analyst uses to frame a hook echo, and a step bigger than about a fifth of
/// the scale overshoots the framing every time -- which is the same complaint
/// as a step that is too small, just from the other side. 1.2 is exactly 20%,
/// and the burst gain below is what supplies speed, so the two concerns do not
/// have to be traded against each other in one constant.
///
/// At 1.2, 4 -> 0.1 km/point (a factor of 40) is ln(40)/ln(1.2) = 20.2, so 21
/// notches of deliberate clicking and 13 for 0.5 -> 0.05. Scrolling rather than
/// clicking engages [`MAX_BURST_GAIN`] and cuts those to 10 and 7 at an
/// ordinary scroll, 7 and 5 on a flick.
pub const ZOOM_PER_NOTCH: f32 = 1.2;

/// The most a notch can be worth while the wheel is being spun.
///
/// A detented wheel reports no magnitude -- every notch is the same event -- so
/// RATE is the only signal separating a deliberate click from a flick. The gain
/// is the recent notch rate expressed in notches per [`BURST_MEMORY_SECONDS`],
/// floored at 1 and capped here, which means a lone notch is always exactly
/// `ZOOM_PER_NOTCH` and a hard spin tops out at 1.2^5 = 2.49 per notch.
///
/// The cap is set so that the FAST end does not depend on how fine the slow end
/// is: 1.2^5 = 2.488 is the same spun notch a coarser `ZOOM_PER_NOTCH` of 1.3
/// would reach at a gain of 3.5. Halving the size of a deliberate notch
/// therefore cost nothing on a flick.
pub const MAX_BURST_GAIN: f32 = 5.0;

/// How long a notch keeps counting toward "the wheel is spinning".
///
/// Deliberate clicking tops out near three notches per second, so a 0.3 s
/// window has decayed to nothing by the time the next deliberate notch lands
/// and that notch gets gain 1. A flick is 8 notches per second or more, well
/// inside the window.
pub const BURST_MEMORY_SECONDS: f64 = 0.3;

/// Continuous-scroll points that make up one notch-equivalent.
///
/// A trackpad reports a distance rather than a detent, so its magnitude already
/// encodes how hard the analyst swiped; that is why continuous scrolling gets
/// no burst gain (see [`ZoomResponder::factor`]). 40 points per notch puts a
/// full 4 -> 0.1 km/point descent at about 560 points of swipe, one and a half
/// comfortable gestures.
pub const TRACKPAD_POINTS_PER_NOTCH: f32 = 40.0;

/// Fractions of the shorter viewport edge a held pan key covers per second.
///
/// 1.2 crosses a square pane in a little under a second: a pan the analyst can
/// stop on a feature rather than one that overshoots it.
pub const KEY_PAN_FRACTION_PER_SECOND: f32 = 1.2;

/// Scale change per second while a zoom key is held.
///
/// A held key is a flight, not a step: 6.0 per second puts 4 -> 0.1 km/point at
/// ln(40)/ln(6) = 2.1 seconds of holding. The press that starts the hold is
/// worth one full notch on its own, so a quick tap is still a definite move.
pub const KEY_ZOOM_RATE_PER_SECOND: f32 = 6.0;

/// Ceiling on the frame time a held key is integrated over.
///
/// A stalled frame -- a volume landing, a GPU hitch -- must not teleport the
/// camera across the county because the camera kept flying while nothing was
/// drawn.
pub const MAX_NAV_STEP_SECONDS: f32 = 0.1;

/// The most one frame of input may change the scale, as a multiple.
///
/// The same rule as [`MAX_NAV_STEP_SECONDS`], for the wheel instead of the
/// keyboard, and it exists for the same reason. A stall does not lose the
/// notches the analyst spun during it: the window system queues them and hands
/// the whole backlog to the first frame that runs. The burst gain then reads
/// that backlog as one enormous gesture -- ten detents in a frame earn the
/// capped gain AND multiply by ten, an exponent of fifty, which at
/// [`ZOOM_PER_NOTCH`] is a scale change of 9100x. The legal range is 5000x, so
/// a three-tenths-of-a-second hitch during a spin used to end at
/// [`MIN_KM_PER_POINT`] no matter what the analyst meant.
///
/// A decade is far more than any real gesture -- sustaining it crosses the
/// whole range in four frames -- so this never shapes the feel of the wheel.
/// It only refuses to believe that a backlog was a flick.
pub const MAX_SCALE_CHANGE_PER_FRAME: f32 = 10.0;

/// Ceiling on the notches one step may carry.
///
/// The camera clamps its own scale, but the clamp happens after the anchor
/// correction has already read the new scale, so an unbounded exponent could
/// hand `zoom_about` a non-finite factor before any clamp saw it.
const MAX_NOTCHES_PER_STEP: f32 = 64.0;

/// One frame of wheel input, resolved into notches by the input layer.
///
/// Split by device class because the two carry different information, not
/// because they come from different hardware: a detent is a count, a swipe is a
/// distance.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WheelNotches {
    /// Notches from a detented wheel. Fractional values are normal: a
    /// high-resolution wheel reports a fifth of a notch per event.
    pub detented: f32,
    /// Notch-equivalents from a continuous device -- a trackpad, a touch drag,
    /// a free-spinning wheel that reports pixels.
    pub continuous: f32,
}

impl WheelNotches {
    pub const NONE: Self = Self {
        detented: 0.0,
        continuous: 0.0,
    };

    pub const fn detented(notches: f32) -> Self {
        Self {
            detented: notches,
            continuous: 0.0,
        }
    }

    pub const fn continuous(notches: f32) -> Self {
        Self {
            detented: 0.0,
            continuous: notches,
        }
    }

    pub fn is_idle(self) -> bool {
        finite_or_zero(self.detented) == 0.0 && finite_or_zero(self.continuous) == 0.0
    }
}

/// The scale multiplier for a count of notches. `notches > 0` zooms in.
pub fn zoom_factor_for_notches(notches: f32) -> f32 {
    if !notches.is_finite() {
        return 1.0;
    }
    ZOOM_PER_NOTCH.powf(notches.clamp(-MAX_NOTCHES_PER_STEP, MAX_NOTCHES_PER_STEP))
}

/// Turns wheel notches into zoom factors, remembering how fast they arrived.
///
/// One instance per pane, carried across frames. It holds only a decaying notch
/// count and the time of the last notch, so it costs nothing to keep and
/// nothing to reason about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomResponder {
    /// Notches seen inside the burst window, decayed toward zero between them.
    /// Under steady scrolling this settles at `rate * BURST_MEMORY_SECONDS`, so
    /// it is a notch-rate estimate already in the units the gain wants.
    burst: f32,
    last_notch_time: f64,
    /// Sign of the last detented notch: +1 in, -1 out, 0 before any.
    ///
    /// Carried so that a REVERSAL can throw the window away. Without it the
    /// out-run inherits the speed the in-run built up while the in-run had to
    /// earn it from a standing start, and the two runs no longer cancel: eight
    /// notches in and eight back out at eight notches a second used to land
    /// 2.4x further out than where it began, and at thirteen a second, 6.1x.
    /// That is exactly the "nav fights me" the acceleration was added to cure.
    ///
    /// Resetting is also the right response on its own terms. Reversing means
    /// the analyst overshot and is correcting, and a correction wants the
    /// finest step the wheel has, not the fastest.
    last_direction: f32,
}

impl Default for ZoomResponder {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoomResponder {
    pub const fn new() -> Self {
        Self {
            burst: 0.0,
            // No notch has ever arrived, so the first one must find an empty
            // window however late in the session it lands.
            last_notch_time: f64::NEG_INFINITY,
            last_direction: 0.0,
        }
    }

    /// The gain a detented notch is currently worth.
    pub fn burst_gain(self) -> f32 {
        if self.burst.is_finite() {
            self.burst.clamp(1.0, MAX_BURST_GAIN)
        } else {
            1.0
        }
    }

    /// The zoom factor for one frame of wheel input.
    ///
    /// Call once per frame with that frame's summed notches. Calling it per
    /// event would make the burst window depend on how the platform happened to
    /// split a spin into events, and a high-resolution wheel splits one notch
    /// into five.
    pub fn factor(&mut self, notches: WheelNotches, now_seconds: f64) -> f32 {
        let detented = finite_or_zero(notches.detented);
        let continuous = finite_or_zero(notches.continuous);
        if detented == 0.0 && continuous == 0.0 {
            // Deliberately no state change on an idle frame. Ageing the window
            // once per frame rather than once per notch would make the gain
            // depend on the frame rate, so the same flick would zoom further on
            // a slower machine.
            return 1.0;
        }

        if detented != 0.0 {
            let direction = if detented > 0.0 { 1.0 } else { -1.0 };
            let elapsed = now_seconds - self.last_notch_time;
            // Non-finite covers the first notch of the session; negative covers
            // a clock that stepped backwards; a change of direction covers an
            // overshoot being corrected. All three start a fresh burst, so the
            // notch that begins a run is always worth exactly one notch.
            let retained =
                if direction == self.last_direction && elapsed.is_finite() && elapsed >= 0.0 {
                    (1.0 - elapsed / BURST_MEMORY_SECONDS).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
            self.burst = self.burst.mul_add(retained, detented.abs());
            self.last_direction = direction;
            if now_seconds.is_finite() {
                self.last_notch_time = now_seconds;
            }
        }

        // Continuous input is deliberately NOT accelerated, and deliberately
        // does not feed the window: its magnitude already says how hard the
        // analyst swiped, so multiplying by a rate would count that twice and a
        // trackpad would become impossible to land on a scale.
        let exponent = detented * self.burst_gain() + continuous;
        // Capped per frame, not per notch: see MAX_SCALE_CHANGE_PER_FRAME. The
        // burst still records every notch, so the cap is symmetric and a spin
        // that was capped on the way in is capped the same on the way out.
        zoom_factor_for_notches(exponent)
            .clamp(1.0 / MAX_SCALE_CHANGE_PER_FRAME, MAX_SCALE_CHANGE_PER_FRAME)
    }
}

/// Keyboard navigation asked for this frame, in device-independent terms.
///
/// The input layer resolves keys to these; the camera never sees a key. That
/// keeps the binding decision in one place and lets the response be tested
/// without an event loop.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NavInput {
    /// +1 flies the view right (east at zero rotation), -1 left.
    pub pan_right: f32,
    /// +1 flies the view up (north at zero rotation), -1 down.
    pub pan_up: f32,
    /// +1 while a zoom-in key is held, -1 while a zoom-out key is held.
    pub zoom_hold: f32,
    /// Whole notches from zoom keys pressed this frame, on top of the hold.
    pub zoom_steps: f32,
    /// Put the camera back on the radar at the default scale.
    pub reset: bool,
}

impl NavInput {
    pub fn is_idle(self) -> bool {
        !self.reset
            && finite_or_zero(self.pan_right) == 0.0
            && finite_or_zero(self.pan_up) == 0.0
            && finite_or_zero(self.zoom_hold) == 0.0
            && finite_or_zero(self.zoom_steps) == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: ViewportMetrics = ViewportMetrics {
        width_points: 1000.0,
        height_points: 800.0,
        pixels_per_point: 2.0,
    };

    fn close(left: f64, right: f64) {
        assert!((left - right).abs() < 1.0e-5, "{left} != {right}");
    }

    #[test]
    fn world_screen_round_trip_survives_rotation() {
        let camera = Camera2D {
            center_east_km: 12.0,
            center_north_km: -8.0,
            km_per_point: 0.2,
            rotation_rad: 0.73,
        };
        let world = WorldPoint::new(45.0, 19.0);
        let restored = camera.screen_to_world(camera.world_to_screen(world, VIEW), VIEW);
        close(restored.east_km, world.east_km);
        close(restored.north_km, world.north_km);
    }

    #[test]
    fn zoom_keeps_anchor_world_coordinate_fixed() {
        let mut camera = Camera2D::default();
        let anchor = ScreenPoint::new(810.0, 190.0);
        let before = camera.screen_to_world(anchor, VIEW);
        camera.zoom_about(2.5, anchor, VIEW);
        let after = camera.screen_to_world(anchor, VIEW);
        close(before.east_km, after.east_km);
        close(before.north_km, after.north_km);
    }

    /// The bug this pins: with only the RESULT clamped, a zoom-out notch at
    /// the scale ceiling still ran the anchor correction against a scale that
    /// had not moved, and scrolling into the wall walked the whole map -
    /// radar sites included - off the screen over a round trip. A notch the
    /// limits cannot honour must leave the camera bit-for-bit untouched.
    #[test]
    fn a_notch_the_scale_limits_cannot_honour_is_an_exact_no_op() {
        let mut camera = Camera2D {
            km_per_point: MAX_KM_PER_POINT,
            center_east_km: 120.0,
            center_north_km: -45.0,
            ..Camera2D::default()
        };
        let held = camera;
        // Off-centre anchor, so a drifting anchor correction cannot hide.
        let anchor = ScreenPoint::new(1200.0, 300.0);
        for _ in 0..40 {
            camera.zoom_about(0.72, anchor, VIEW);
        }
        assert_eq!(camera.km_per_point.to_bits(), held.km_per_point.to_bits());
        assert_eq!(
            camera.center_east_km.to_bits(),
            held.center_east_km.to_bits()
        );
        assert_eq!(
            camera.center_north_km.to_bits(),
            held.center_north_km.to_bits()
        );

        // And at the floor, the same in the other direction.
        let mut camera = Camera2D {
            km_per_point: MIN_KM_PER_POINT,
            ..Camera2D::default()
        };
        let held = camera;
        camera.zoom_about(1.4, anchor, VIEW);
        assert_eq!(camera.km_per_point.to_bits(), held.km_per_point.to_bits());
        assert_eq!(
            camera.center_east_km.to_bits(),
            held.center_east_km.to_bits()
        );
    }

    /// A notch the limits can only PARTLY honour applies exactly the
    /// achievable part: the scale lands on the limit, and the anchor holds.
    #[test]
    fn a_partly_honourable_notch_applies_its_achievable_part() {
        let mut camera = Camera2D {
            km_per_point: MAX_KM_PER_POINT * 0.9,
            ..Camera2D::default()
        };
        let anchor = ScreenPoint::new(810.0, 190.0);
        let before = camera.screen_to_world(anchor, VIEW);
        camera.zoom_about(0.5, anchor, VIEW);
        assert_eq!(camera.km_per_point, MAX_KM_PER_POINT);
        let after = camera.screen_to_world(anchor, VIEW);
        close(before.east_km, after.east_km);
        close(before.north_km, after.north_km);
    }

    #[test]
    fn pan_moves_world_content_with_pointer() {
        let mut camera = Camera2D::default();
        let world = WorldPoint::new(30.0, 20.0);
        let before = camera.world_to_screen(world, VIEW);
        camera.pan_by_screen_delta(50.0, -25.0);
        let after = camera.world_to_screen(world, VIEW);
        assert!((after.x - before.x - 50.0).abs() < 1.0e-4);
        assert!((after.y - before.y + 25.0).abs() < 1.0e-4);
    }

    #[test]
    fn raster_view_places_radar_from_camera_transform() {
        let camera = Camera2D {
            center_east_km: 35.0,
            center_north_km: -14.0,
            ..Camera2D::default()
        };
        let view = camera.radar_raster_view(VIEW);
        assert_eq!(view.width_px, 2000);
        assert_eq!(view.height_px, 1600);
        let radar_points = camera.world_to_screen(WorldPoint::ORIGIN, VIEW);
        assert!((view.radar_x_px - radar_points.x * 2.0).abs() < 1.0e-4);
        assert!((view.radar_y_px - radar_points.y * 2.0).abs() < 1.0e-4);
    }

    #[test]
    fn geometry_identity_is_camera_independent() {
        let key = GeometryCacheKey {
            dataset: Generation::new(4),
            projection: Generation::new(2),
            style: Generation::new(7),
            lod: LodBucket(3),
        };
        let mut camera = Camera2D::default();
        camera.pan_by_screen_delta(900.0, -400.0);
        camera.zoom_about(1.4, VIEW.center(), VIEW);
        assert_eq!(
            key,
            GeometryCacheKey {
                dataset: Generation::new(4),
                projection: Generation::new(2),
                style: Generation::new(7),
                lod: LodBucket(3),
            }
        );
    }

    #[test]
    fn lod_hysteresis_holds_near_boundary() {
        let mut selector = LodSelector::new(1.0, 1.0);
        let original = selector.current();
        assert_eq!(selector.update(1.05), original);
        assert_eq!(selector.update(0.96), original);
    }

    /// Count the notches a scale change costs, driving the responder exactly
    /// the way one frame of input drives it.
    ///
    /// `notches_per_second` is what separates a deliberate click from a spin,
    /// so it is the only knob: the same helper measures both.
    fn notches_between(from_km: f32, to_km: f32, notches_per_second: f64) -> usize {
        let mut camera = Camera2D {
            km_per_point: from_km,
            ..Camera2D::default()
        };
        let mut responder = ZoomResponder::new();
        let zoom_in = to_km < from_km;
        let gap = 1.0 / notches_per_second;
        let mut now = 0.0_f64;
        let mut count = 0_usize;
        while count < 500 {
            if zoom_in && camera.km_per_point <= to_km {
                break;
            }
            if !zoom_in && camera.km_per_point >= to_km {
                break;
            }
            now += gap;
            let notch = if zoom_in { 1.0 } else { -1.0 };
            let factor = responder.factor(WheelNotches::detented(notch), now);
            camera.zoom_about(factor, VIEW.center(), VIEW);
            count += 1;
        }
        count
    }

    /// The rate the owner was complaining about. Deliberate clicking is the
    /// SLOW end of the new response and it still beats the old one nearly
    /// fourfold; anything faster than clicking beats it by an order.
    #[test]
    fn deliberate_notches_cross_the_working_range_without_fifty_of_them() {
        // Hand-computed: ln(40)/ln(1.2) = 3.68888/0.18232 = 20.23, so the
        // twenty-first notch is the one that arrives at 0.1 km/point.
        assert_eq!(notches_between(4.0, 0.1, 3.0), 21);
        // ln(10)/ln(1.2) = 2.30259/0.18232 = 12.63 -> the thirteenth arrives.
        assert_eq!(notches_between(0.5, 0.05, 3.0), 13);
        // And back out, at the same cost: the response is symmetric.
        assert_eq!(notches_between(0.1, 4.0, 3.0), 21);
        assert_eq!(notches_between(0.05, 0.5, 3.0), 13);
        // Deliberate clicking is the SLOW end and it is deliberately fine: one
        // notch is a fifth of the scale, small enough to frame a hook echo
        // rather than jump over it. The old additive rule was 6.7% per notch
        // and 58 notches across this range; the speed is bought back by the
        // burst gain, not by making the fine step coarse.
        const {
            assert!(
                ZOOM_PER_NOTCH <= 1.2,
                "a notch bigger than a fifth of the scale overshoots the framing"
            );
        }
    }

    /// Spinning the wheel rather than clicking it is where the burst gain
    /// earns its keep. These are ceilings, not the exact counts, because the
    /// point is that a spin is cheap -- but they are tight enough that losing
    /// the acceleration fails the test.
    #[test]
    fn spinning_the_wheel_crosses_the_range_in_a_handful_of_notches() {
        // An ordinary scroll, about eight notches per second.
        assert_eq!(notches_between(4.0, 0.1, 8.0), 10);
        assert_eq!(notches_between(0.5, 0.05, 8.0), 7);
        // A brisk flick.
        assert_eq!(notches_between(4.0, 0.1, 13.3), 8);
        assert_eq!(notches_between(0.5, 0.05, 13.3), 6);
        // A hard spin cannot run away: the gain is capped, so this never falls
        // below the brisk count by more than the cap allows.
        assert_eq!(notches_between(4.0, 0.1, 40.0), 7);
        assert_eq!(notches_between(0.5, 0.05, 40.0), 5);
    }

    /// The half of the brief acceleration must not break: one notch on its own
    /// is always exactly one notch, whenever in the session it arrives.
    #[test]
    fn a_lone_notch_is_always_exactly_one_notch() {
        let mut responder = ZoomResponder::new();
        assert!((responder.factor(WheelNotches::detented(1.0), 0.0) - ZOOM_PER_NOTCH).abs() < 1e-6);
        // Far enough apart that the window has fully decayed.
        assert!(
            (responder.factor(WheelNotches::detented(1.0), 10.0) - ZOOM_PER_NOTCH).abs() < 1e-6
        );
        assert!((responder.burst_gain() - 1.0).abs() < 1e-6);
        // Exactly at the window length the memory is spent, not merely small.
        let mut edge = ZoomResponder::new();
        edge.factor(WheelNotches::detented(1.0), 100.0);
        let factor = edge.factor(WheelNotches::detented(1.0), 100.0 + BURST_MEMORY_SECONDS);
        assert!((factor - ZOOM_PER_NOTCH).abs() < 1e-6, "{factor}");
    }

    /// A stall queues the notches spun during it and hands the whole backlog
    /// to the next frame. The backlog must not be read as one enormous flick.
    ///
    /// Ten detents in one frame is a three-tenths-of-a-second hitch at an
    /// ordinary spin rate -- a volume landing, a shader compiling -- and the
    /// gain used to multiply it into 1.2^50 = 9100x, which is nearly twice the
    /// whole legal range, so the camera arrived at MIN_KM_PER_POINT whatever
    /// the analyst meant. It is the wheel's version of the stalled frame that
    /// MAX_NAV_STEP_SECONDS already guards the keyboard against.
    #[test]
    fn a_backlog_of_notches_after_a_stall_cannot_teleport_the_camera() {
        for backlog in [1.0_f32, 3.0, 5.0, 10.0, 40.0, 1.0e6] {
            for direction in [1.0_f32, -1.0] {
                let mut responder = ZoomResponder::new();
                let mut camera = Camera2D {
                    km_per_point: (MIN_KM_PER_POINT * MAX_KM_PER_POINT).sqrt(),
                    ..Camera2D::default()
                };
                let before = camera.km_per_point;
                let factor = responder.factor(WheelNotches::detented(backlog * direction), 1.0);
                camera.zoom_about(factor, VIEW.center(), VIEW);
                let moved = (camera.km_per_point / before).max(before / camera.km_per_point);
                assert!(
                    moved <= MAX_SCALE_CHANGE_PER_FRAME * 1.0001,
                    "a frame carrying {backlog} detents moved the scale {moved}x"
                );
            }
        }
        // The cap is a rail, not the response: everything a hand can do in one
        // frame is well under it, so the ordinary feel is untouched.
        let mut ordinary = ZoomResponder::new();
        let factor = ordinary.factor(WheelNotches::detented(3.0), 1.0);
        assert!(
            factor < MAX_SCALE_CHANGE_PER_FRAME,
            "three detents in a frame already hit the rail: {factor}"
        );
        // A held zoom key gets the same rail, from the same kind of backlog.
        let mut keyed = Camera2D {
            km_per_point: (MIN_KM_PER_POINT * MAX_KM_PER_POINT).sqrt(),
            ..Camera2D::default()
        };
        let before = keyed.km_per_point;
        keyed.apply_nav(
            NavInput {
                zoom_steps: 60.0,
                zoom_hold: 1.0,
                ..NavInput::default()
            },
            MAX_NAV_STEP_SECONDS,
            VIEW,
        );
        assert!(
            before / keyed.km_per_point <= MAX_SCALE_CHANGE_PER_FRAME * 1.0001,
            "sixty queued key presses moved the scale {}x",
            before / keyed.km_per_point
        );
    }

    #[test]
    fn burst_gain_is_capped_however_hard_the_wheel_is_spun() {
        let mut responder = ZoomResponder::new();
        for step in 0..400 {
            responder.factor(WheelNotches::detented(1.0), f64::from(step) * 0.001);
        }
        assert!((responder.burst_gain() - MAX_BURST_GAIN).abs() < 1e-6);
        let factor = responder.factor(WheelNotches::detented(1.0), 0.4);
        assert!(
            factor <= ZOOM_PER_NOTCH.powf(MAX_BURST_GAIN) + 1e-4,
            "one notch became {factor}"
        );
    }

    /// A trackpad reports distance, not detents, so it must not be accelerated
    /// and must not leave a burst behind for the next wheel notch to inherit.
    #[test]
    fn trackpad_scrolling_is_not_accelerated() {
        let mut responder = ZoomResponder::new();
        let mut swiped = 1.0_f32;
        // A brisk swipe: forty frames of one notch-equivalent each, back to
        // back. A detented wheel at this rate would be at the gain cap.
        for step in 0..40 {
            swiped *= responder.factor(WheelNotches::continuous(1.0), f64::from(step) * 0.016);
        }
        let unaccelerated = ZOOM_PER_NOTCH.powi(40);
        assert!(
            (swiped / unaccelerated - 1.0).abs() < 1e-3,
            "{swiped} != {unaccelerated}"
        );
        // And the wheel notch that follows is still worth exactly one notch.
        let factor = responder.factor(WheelNotches::detented(1.0), 0.64);
        assert!((factor - ZOOM_PER_NOTCH).abs() < 1e-6, "{factor}");
    }

    /// Scroll in a while, then scroll straight back out the way you came, and
    /// land on the scale you started from.
    ///
    /// This is the whole of "the wheel obeys me", and it is a REAL round trip:
    /// the out-notches are driven through the responder exactly as the
    /// in-notches were, at the same rate, in the same order. Inverting the
    /// factors the in-run happened to produce would prove nothing, because
    /// `1/f * f` is 1 for any response at all -- including the additive one
    /// this replaced, which drifted, and including an accelerating one that
    /// does not cancel.
    ///
    /// Without the direction reset in [`ZoomResponder`] the out-run inherits
    /// the speed the in-run built up while the in-run had to earn it from
    /// rest, and eight notches each way at eight per second ended 2.4x further
    /// out than it began (6.1x at thirteen per second). Started at 4 km/point
    /// so the descent stays clear of MIN_KM_PER_POINT: a clamp is not a
    /// rounding error, and a run that hits one is genuinely not reversible.
    #[test]
    fn an_overshoot_scrolled_straight_back_lands_where_it_started() {
        // Geometric centre of the legal range, so there is the same headroom
        // either way and the run is measuring the response, not the clamp.
        let start = (MIN_KM_PER_POINT * MAX_KM_PER_POINT).sqrt();
        // A quarter of the way to the limit. A run that reaches the clamp is
        // genuinely not reversible -- the scale it would have had no longer
        // exists -- so the descent stops before it gets there and the ascent
        // undoes exactly the notches the descent spent.
        let floor = MIN_KM_PER_POINT * 4.0;
        for rate in [1.0_f64, 3.0, 5.0, 8.0, 13.3, 25.0, 40.0] {
            for wanted in [1_usize, 4, 8, 12, 30] {
                let mut camera = Camera2D {
                    km_per_point: start,
                    ..Camera2D::default()
                };
                let anchor = ScreenPoint::new(640.0, 210.0);
                let before = camera.screen_to_world(anchor, VIEW);
                let mut responder = ZoomResponder::new();
                let gap = 1.0 / rate;
                let mut now = 0.0_f64;
                let mut spent = 0_usize;
                while spent < wanted && camera.km_per_point > floor {
                    now += gap;
                    let factor = responder.factor(WheelNotches::detented(1.0), now);
                    camera.zoom_about(factor, anchor, VIEW);
                    spent += 1;
                }
                assert!(
                    camera.km_per_point < start * 0.999,
                    "{rate}/s x{wanted} never moved"
                );
                for _ in 0..spent {
                    now += gap;
                    let factor = responder.factor(WheelNotches::detented(-1.0), now);
                    camera.zoom_about(factor, anchor, VIEW);
                }
                assert!(
                    (camera.km_per_point / start - 1.0).abs() < 1.0e-4,
                    "{rate}/s x{spent} notches each way ended at {} instead of {start}",
                    camera.km_per_point
                );
                // The anchor is where it was too, so an overshoot-and-correct
                // does not leave the map shifted sideways either.
                let after = camera.screen_to_world(anchor, VIEW);
                let drift =
                    (before.east_km - after.east_km).hypot(before.north_km - after.north_km);
                assert!(drift < 0.01, "{rate}/s x{spent} shifted the map {drift} km");
            }
        }
    }

    /// A reversal is a correction, so the notch that reverses is the FINEST
    /// step the wheel has, not the fastest one the previous run earned.
    #[test]
    fn reversing_direction_gives_back_the_fine_step() {
        let mut responder = ZoomResponder::new();
        // Spin in hard enough to sit on the gain cap.
        let mut now = 0.0_f64;
        for _ in 0..30 {
            now += 0.02;
            responder.factor(WheelNotches::detented(1.0), now);
        }
        assert!((responder.burst_gain() - MAX_BURST_GAIN).abs() < 1.0e-6);
        // The very next notch the other way, one frame later, is one notch and
        // nothing more.
        now += 0.02;
        let factor = responder.factor(WheelNotches::detented(-1.0), now);
        assert!(
            (factor - 1.0 / ZOOM_PER_NOTCH).abs() < 1.0e-6,
            "the reversing notch was worth {factor}, not one notch out"
        );
        // The run that follows accelerates again from rest, so the reversal
        // costs speed exactly once.
        now += 0.02;
        let second = responder.factor(WheelNotches::detented(-1.0), now);
        assert!(
            second < factor,
            "{second} should be a bigger step out than {factor}"
        );
    }

    #[test]
    fn nonsense_wheel_input_never_moves_the_camera() {
        let mut responder = ZoomResponder::new();
        assert_eq!(responder.factor(WheelNotches::NONE, 0.0), 1.0);
        assert_eq!(responder.factor(WheelNotches::detented(f32::NAN), 0.5), 1.0);
        assert_eq!(
            responder.factor(WheelNotches::continuous(f32::INFINITY), 0.5),
            1.0
        );
        // A finite but absurd burst -- a wheel driver gone mad -- is bounded
        // rather than turned into a non-finite factor.
        let factor = responder.factor(WheelNotches::detented(1.0e30), 1.0);
        assert!(factor.is_finite(), "{factor}");
        let mut camera = Camera2D::default();
        camera.zoom_about(factor, VIEW.center(), VIEW);
        assert_eq!(camera.sanitized(), camera);
    }

    /// Every viewport, cursor and camera the anchor sweep is run over.
    ///
    /// Shared by the single-step sweep and the hundred-step walk so the two
    /// cannot drift apart: a fix that holds for one step and not for a hundred
    /// is not a fix.
    fn anchor_sweep_viewports() -> Vec<ViewportMetrics> {
        let mut out = Vec::new();
        // Square, landscape, portrait and the two degenerate strips a
        // four-pane layout can produce on an ultrawide or a laptop.
        for (width_points, height_points) in [
            (640.0_f32, 640.0_f32),
            (1000.0, 800.0),
            (1920.0, 1080.0),
            (1080.0, 1920.0),
            (2560.0, 80.0),
            (80.0, 2560.0),
        ] {
            // `pixels_per_point` does not enter the camera transform at all --
            // it is applied once in `radar_raster_view` -- so every one of
            // these must give the same answer. Sweeping it is how that stays
            // true rather than merely being true today.
            for pixels_per_point in [1.0_f32, 1.25, 1.5, 2.0] {
                out.push(ViewportMetrics {
                    width_points,
                    height_points,
                    pixels_per_point,
                });
            }
        }
        out
    }

    /// All four corners, the exact centre, and a spread of interior points.
    ///
    /// The corners and the centre are the cases a sign error hides in: at the
    /// centre the anchor correction is identically zero, so a broken one still
    /// passes there.
    fn anchor_sweep_cursors(viewport: ViewportMetrics) -> Vec<ScreenPoint> {
        let (w, h) = (viewport.width_points, viewport.height_points);
        let mut out = vec![
            ScreenPoint::new(0.0, 0.0),
            ScreenPoint::new(w, 0.0),
            ScreenPoint::new(0.0, h),
            ScreenPoint::new(w, h),
            ScreenPoint::new(w * 0.5, h * 0.5),
        ];
        for fx in [0.125_f32, 0.63, 0.97] {
            for fy in [0.03_f32, 0.44, 0.99] {
                out.push(ScreenPoint::new(w * fx, h * fy));
            }
        }
        out
    }

    fn anchor_sweep_cameras() -> [Camera2D; 6] {
        [
            Camera2D::default(),
            Camera2D {
                center_east_km: 240.0,
                center_north_km: -180.0,
                km_per_point: 4.0,
                rotation_rad: 0.0,
            },
            Camera2D {
                center_east_km: -55.5,
                center_north_km: 31.25,
                km_per_point: 0.02,
                rotation_rad: 0.91,
            },
            Camera2D {
                center_east_km: 12.0,
                center_north_km: 9.0,
                km_per_point: MAX_KM_PER_POINT,
                rotation_rad: -2.4,
            },
            Camera2D {
                center_east_km: 0.0,
                center_north_km: 0.0,
                km_per_point: MIN_KM_PER_POINT,
                rotation_rad: 3.0,
            },
            // Far from the radar and zoomed in: the case where the world
            // coordinate is large and the interesting difference is small, so
            // f32 cancellation would show up here first.
            Camera2D {
                center_east_km: 460.0,
                center_north_km: 460.0,
                km_per_point: 0.05,
                rotation_rad: 1.7,
            },
        ]
    }

    /// The property that makes zooming feel like it obeys you: whatever is
    /// under the cursor stays under the cursor.
    ///
    /// Measured with the FORWARD transform, on purpose. `zoom_about` corrects
    /// the centre by the difference of two `screen_to_world` readings, so
    /// asking `screen_to_world` again afterwards returns the corrected value by
    /// construction and agrees to the last bit whatever the camera does -- it
    /// measures the subtraction, not the picture. `world_to_screen` is the
    /// independent direction and it is also the one the renderer and every
    /// overlay actually use, so a slip that shows up here is a slip the analyst
    /// can see.
    ///
    /// The tolerance is in screen POINTS because that is what "stayed under the
    /// cursor" means and because it is the one figure that is honest at every
    /// scale: a hundredth of a kilometre is a quarter of a pixel at
    /// MIN_KM_PER_POINT and a five-thousandth of a pixel at MAX_KM_PER_POINT.
    /// The ground distance is checked too, across the whole working range.
    #[test]
    fn zoom_holds_the_world_point_under_the_cursor() {
        // One notch each way, a whole spin each way, factors that slam into
        // both scale clamps, and the identity.
        let factors = [
            ZOOM_PER_NOTCH,
            1.0 / ZOOM_PER_NOTCH,
            zoom_factor_for_notches(6.0),
            zoom_factor_for_notches(-6.0),
            1.0001,
            0.9999,
            10_000.0,
            0.0001,
            1.0,
        ];
        let mut checked = 0_usize;
        for viewport in anchor_sweep_viewports() {
            for cursor in anchor_sweep_cursors(viewport) {
                for camera in anchor_sweep_cameras() {
                    for factor in factors {
                        let mut moved = camera;
                        let world = moved.screen_to_world(cursor, viewport);
                        moved.zoom_about(factor, cursor, viewport);
                        let back = moved.world_to_screen(world, viewport);
                        let slip_points = (back.x - cursor.x).hypot(back.y - cursor.y);
                        assert!(
                            slip_points < 0.01,
                            "the picture slid {slip_points} points under the \
                             cursor: factor {factor}, cursor ({}, {}), \
                             viewport {}x{}@{}, camera {camera:?}",
                            cursor.x,
                            cursor.y,
                            viewport.width_points,
                            viewport.height_points,
                            viewport.pixels_per_point,
                        );
                        // The same slip as ground distance. Only asserted over
                        // the working range: at 50 km/point one point IS 50 km,
                        // so a metric of ten metres there is a statement about
                        // f32 and not about the camera.
                        if moved.km_per_point <= 4.0 {
                            let slip_km = f64::from(slip_points) * f64::from(moved.km_per_point);
                            assert!(slip_km < 0.01, "{slip_km} km at {camera:?}");
                        }
                        // Whatever the factor asked for, the camera is still a
                        // camera afterwards.
                        assert_eq!(moved.sanitized(), moved, "zoom left {moved:?} unrepaired");
                        checked += 1;
                    }
                }
            }
        }
        // A sweep that silently stopped covering anything is worse than none.
        assert!(checked > 5_000, "only {checked} cases");
    }

    /// A hundred consecutive zoom steps at one cursor position, then a hundred
    /// back.
    ///
    /// One step's worth of anchor error is invisible; the question is whether
    /// it ACCUMULATES, because the correction is applied per step and a
    /// systematic 1e-4 km bias is a kilometre out after a hundred notches. A
    /// fifth of a notch per step is what a high-resolution wheel reports, and
    /// it is small enough that a hundred of them stay inside the scale limits
    /// (1.2^20 = 38x), so this measures drift rather than the clamp.
    #[test]
    fn a_hundred_zoom_steps_do_not_walk_the_anchor() {
        let step = zoom_factor_for_notches(0.2);
        for viewport in anchor_sweep_viewports() {
            for cursor in [
                ScreenPoint::new(0.0, 0.0),
                ScreenPoint::new(viewport.width_points, viewport.height_points),
                ScreenPoint::new(viewport.width_points * 0.93, viewport.height_points * 0.07),
            ] {
                for rotation_rad in [0.0_f32, 0.42, 2.9] {
                    let mut camera = Camera2D {
                        center_east_km: 118.0,
                        center_north_km: -62.0,
                        km_per_point: 4.0,
                        rotation_rad,
                    };
                    let world = camera.screen_to_world(cursor, viewport);
                    let mut worst = 0.0_f32;
                    let mut deepest = camera.km_per_point;
                    for direction in [step, 1.0 / step] {
                        for _ in 0..100 {
                            camera.zoom_about(direction, cursor, viewport);
                            deepest = deepest.min(camera.km_per_point);
                            let back = camera.world_to_screen(world, viewport);
                            worst = worst.max((back.x - cursor.x).hypot(back.y - cursor.y));
                        }
                    }
                    assert!(
                        deepest > MIN_KM_PER_POINT * 1.001,
                        "the walk clamped at {deepest}, so it measures the clamp"
                    );
                    assert!(
                        worst < 0.01,
                        "the anchor walked {worst} points over 200 steps: cursor \
                         ({}, {}), viewport {}x{}@{}, rotation {rotation_rad}",
                        cursor.x,
                        cursor.y,
                        viewport.width_points,
                        viewport.height_points,
                        viewport.pixels_per_point,
                    );
                    // And it came back to the scale it left, so the walk is a
                    // round trip rather than a slow leak.
                    assert!(
                        (camera.km_per_point / 4.0 - 1.0).abs() < 1.0e-4,
                        "{}",
                        camera.km_per_point
                    );
                }
            }
        }
    }

    /// The exact input the fuzz below found, named so the defect is readable.
    ///
    /// A pointer delta or a cursor position large enough to overflow
    /// `points * km_per_point` to infinity used to poison the camera CENTRE
    /// while leaving the scale perfectly in range, because the rotation turns
    /// `inf - inf` into NaN and the centre had no check of its own. The pane
    /// then drew nothing, and the next `sanitized()` snapped the view back to
    /// the radar as if the analyst had asked for it.
    ///
    /// Reachable: `pan_by_screen_delta` is fed `pointer.delta()` straight from
    /// the window system, and `zoom_about` is fed `hover_pos()`.
    #[test]
    fn an_overflowing_pointer_leaves_the_camera_exactly_where_it_was() {
        // Rotated so `cos` and `sin` have opposite signs: that is what turns
        // two infinities into a NaN rather than another infinity.
        let spun = Camera2D {
            center_east_km: 25.0,
            center_north_km: -13.0,
            km_per_point: MAX_KM_PER_POINT,
            rotation_rad: 3.0,
        };

        // Compared against the SANITISED camera rather than the literal one:
        // `normalize_angle` rounds 3.0 to 3.0000002 on its first pass through
        // f32 and is a fixed point from then on, so that one bit is the
        // repair working, not the drag leaking.
        let settled = spun.sanitized();
        let mut panned = spun;
        panned.pan_by_screen_delta(f32::MAX, f32::MAX);
        assert_eq!(panned, settled, "an unrepresentable drag moved the camera");

        let mut nudged = spun;
        nudged.pan_by_screen_delta(f32::NAN, 4.0);
        assert_eq!(nudged, settled, "a NaN drag moved the camera");

        let mut zoomed = spun;
        zoomed.zoom_about(ZOOM_PER_NOTCH, ScreenPoint::new(f32::MAX, f32::MAX), VIEW);
        assert!(
            zoomed.center_east_km.is_finite() && zoomed.center_north_km.is_finite(),
            "{zoomed:?}"
        );
        // The scale still changed: only the correction was unrepresentable, and
        // refusing the whole zoom would be a worse answer than refusing the
        // part that could not be computed.
        assert!(
            (zoomed.km_per_point - MAX_KM_PER_POINT / ZOOM_PER_NOTCH).abs() < 1.0e-4,
            "{zoomed:?}"
        );

        // A cursor that is not a number at all falls back to the pane centre,
        // which is the anchor that needs no correction.
        let mut nowhere = spun;
        nowhere.zoom_about(ZOOM_PER_NOTCH, ScreenPoint::new(f32::NAN, 12.0), VIEW);
        let mut middle = spun;
        middle.zoom_about(ZOOM_PER_NOTCH, VIEW.center(), VIEW);
        assert_eq!(nowhere, middle);
    }

    /// Nothing any input path can do leaves the scale outside its limits or
    /// the camera non-finite.
    ///
    /// The limits are load-bearing twice over: they are the depth the analyst
    /// can reach, and they are the only thing standing between a garbage event
    /// and a camera full of NaN. Driven with a deterministic xorshift so a
    /// failure names the exact seed that produced it.
    #[test]
    fn no_input_path_can_take_the_camera_outside_its_limits() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // The values that are not numbers, plus a spread of ordinary ones.
        let nasty = [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.0,
            -0.0,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
            1.0e30,
            -1.0e30,
        ];
        let mut camera = Camera2D::default();
        for round in 0..20_000_u64 {
            let roll = next();
            let pick = |bits: u64| {
                if bits.is_multiple_of(4) {
                    nasty[(bits >> 8) as usize % nasty.len()]
                } else {
                    // Roughly -1e4..1e4, dense near zero.
                    let unit = ((bits >> 16) % 20_001) as f32 / 10_000.0 - 1.0;
                    unit * 10_000.0
                }
            };
            let viewport = ViewportMetrics {
                width_points: pick(next()),
                height_points: pick(next()),
                pixels_per_point: pick(next()),
            };
            let cursor = ScreenPoint::new(pick(next()), pick(next()));
            match roll % 4 {
                0 => camera.zoom_about(pick(next()), cursor, viewport),
                1 => camera.pan_by_screen_delta(pick(next()), pick(next())),
                2 => {
                    let _ = camera.apply_nav(
                        NavInput {
                            pan_right: pick(next()),
                            pan_up: pick(next()),
                            zoom_hold: pick(next()),
                            zoom_steps: pick(next()),
                            reset: next().is_multiple_of(64),
                        },
                        pick(next()),
                        viewport,
                    );
                }
                _ => {
                    let mut responder = ZoomResponder::new();
                    let notches = WheelNotches {
                        detented: pick(next()),
                        continuous: pick(next()),
                    };
                    let factor = responder.factor(notches, f64::from(pick(next())));
                    assert!(factor.is_finite() && factor > 0.0, "factor {factor}");
                    camera.zoom_about(factor, cursor, viewport);
                }
            }
            assert!(
                camera.km_per_point.is_finite()
                    && camera.km_per_point >= MIN_KM_PER_POINT
                    && camera.km_per_point <= MAX_KM_PER_POINT,
                "round {round}: scale {}",
                camera.km_per_point
            );
            assert!(
                camera.center_east_km.is_finite() && camera.center_north_km.is_finite(),
                "round {round}: centre {camera:?}"
            );
            assert!(camera.rotation_rad.is_finite(), "round {round}: {camera:?}");
            // A camera that needs repair is one the next frame draws wrong.
            assert_eq!(camera.sanitized(), camera, "round {round}");
        }
    }

    #[test]
    fn arrow_pan_moves_the_camera_the_way_the_key_points() {
        let mut camera = Camera2D::default();
        let east = NavInput {
            pan_right: 1.0,
            ..NavInput::default()
        };
        camera.apply_nav(east, 0.1, VIEW);
        // The shorter edge is 800 points, so 0.1 s at 1.2 edges per second is
        // 800 * 1.2 * 0.1 = 96 points, and at the default 0.35 km/point that is
        // 33.6 km east.
        assert!((camera.center_east_km - 33.6).abs() < 1.0e-3, "{camera:?}");
        assert!(camera.center_north_km.abs() < 1.0e-6);

        let mut camera = Camera2D::default();
        camera.apply_nav(
            NavInput {
                pan_up: 1.0,
                ..NavInput::default()
            },
            0.1,
            VIEW,
        );
        assert!((camera.center_north_km - 33.6).abs() < 1.0e-3, "{camera:?}");
        assert!(camera.center_east_km.abs() < 1.0e-6);
    }

    /// Pan is expressed in screen directions, so a rotated camera has to fly
    /// toward the top of the SCREEN, not toward north.
    #[test]
    fn pan_follows_the_screen_not_the_compass_when_rotated() {
        let mut camera = Camera2D {
            rotation_rad: std::f32::consts::FRAC_PI_2,
            ..Camera2D::default()
        };
        let world = WorldPoint::new(40.0, -15.0);
        let before = camera.world_to_screen(world, VIEW);
        camera.apply_nav(
            NavInput {
                pan_up: 1.0,
                ..NavInput::default()
            },
            0.1,
            VIEW,
        );
        let after = camera.world_to_screen(world, VIEW);
        // The view flew up the screen, so the content slid down it by the same
        // 96 points, and did not move sideways at all.
        assert!((after.y - before.y - 96.0).abs() < 1.0e-2, "{after:?}");
        assert!((after.x - before.x).abs() < 1.0e-2, "{after:?}");
    }

    #[test]
    fn a_stalled_frame_cannot_teleport_a_held_key() {
        let mut stalled = Camera2D::default();
        stalled.apply_nav(
            NavInput {
                pan_right: 1.0,
                ..NavInput::default()
            },
            5.0,
            VIEW,
        );
        let mut clamped = Camera2D::default();
        clamped.apply_nav(
            NavInput {
                pan_right: 1.0,
                ..NavInput::default()
            },
            MAX_NAV_STEP_SECONDS,
            VIEW,
        );
        assert_eq!(stalled, clamped);
    }

    #[test]
    fn zoom_keys_step_on_press_and_fly_while_held() {
        // A tap is one notch: definite, and the same size as a wheel notch, so
        // the two controls agree about what "one step" means.
        let mut tapped = Camera2D::default();
        assert!(tapped.apply_nav(
            NavInput {
                zoom_steps: 1.0,
                ..NavInput::default()
            },
            0.0,
            VIEW,
        ));
        assert!(
            (tapped.km_per_point - DEFAULT_KM_PER_POINT / ZOOM_PER_NOTCH).abs() < 1.0e-6,
            "{tapped:?}"
        );

        // Holding for a second multiplies the scale by the hold rate.
        let mut held = Camera2D::default();
        for _ in 0..100 {
            held.apply_nav(
                NavInput {
                    zoom_hold: 1.0,
                    ..NavInput::default()
                },
                0.01,
                VIEW,
            );
        }
        let expected = DEFAULT_KM_PER_POINT / KEY_ZOOM_RATE_PER_SECOND;
        assert!(
            (held.km_per_point / expected - 1.0).abs() < 1.0e-4,
            "{held:?} wanted {expected}"
        );

        // Out is the exact inverse of in.
        let mut out = Camera2D::default();
        out.apply_nav(
            NavInput {
                zoom_steps: -1.0,
                ..NavInput::default()
            },
            0.0,
            VIEW,
        );
        assert!(
            (out.km_per_point - DEFAULT_KM_PER_POINT * ZOOM_PER_NOTCH).abs() < 1.0e-6,
            "{out:?}"
        );
    }

    #[test]
    fn reset_puts_the_camera_back_on_the_radar() {
        let mut camera = Camera2D {
            center_east_km: -410.0,
            center_north_km: 233.0,
            km_per_point: 0.011,
            rotation_rad: 1.9,
        };
        assert!(camera.apply_nav(
            NavInput {
                reset: true,
                // Reset wins even when a pan key is held in the same frame.
                pan_right: 1.0,
                zoom_hold: -1.0,
                ..NavInput::default()
            },
            0.016,
            VIEW,
        ));
        assert_eq!(camera, Camera2D::default());
        assert_eq!(
            camera.world_to_screen(WorldPoint::ORIGIN, VIEW),
            VIEW.center()
        );
    }

    #[test]
    fn an_idle_frame_reports_no_camera_change() {
        let mut camera = Camera2D::default();
        assert!(NavInput::default().is_idle());
        assert!(!camera.apply_nav(NavInput::default(), 0.016, VIEW));
        assert_eq!(camera, Camera2D::default());
        // A held key with no elapsed time is idle too, rather than a divide by
        // zero dressed up as a move.
        assert!(!camera.apply_nav(
            NavInput {
                pan_right: 1.0,
                ..NavInput::default()
            },
            0.0,
            VIEW,
        ));
        assert_eq!(camera, Camera2D::default());
    }

    #[test]
    fn nonsense_nav_input_never_moves_the_camera() {
        let mut camera = Camera2D::default();
        assert!(!camera.apply_nav(
            NavInput {
                pan_right: f32::NAN,
                pan_up: f32::INFINITY,
                zoom_hold: f32::NAN,
                zoom_steps: f32::NEG_INFINITY,
                reset: false,
            },
            f32::NAN,
            VIEW,
        ));
        assert_eq!(camera, Camera2D::default());
    }

    /// The scale limits are also the guard against a non-finite camera, so a
    /// flight that runs into them has to stop AT them.
    #[test]
    fn flying_into_the_scale_limits_stops_at_them() {
        let mut inward = Camera2D::default();
        for _ in 0..200 {
            inward.apply_nav(
                NavInput {
                    zoom_steps: 1.0,
                    ..NavInput::default()
                },
                0.0,
                VIEW,
            );
        }
        assert!((inward.km_per_point - MIN_KM_PER_POINT).abs() < 1.0e-9);
        let mut outward = Camera2D::default();
        for _ in 0..200 {
            outward.apply_nav(
                NavInput {
                    zoom_steps: -1.0,
                    ..NavInput::default()
                },
                0.0,
                VIEW,
            );
        }
        assert!((outward.km_per_point - MAX_KM_PER_POINT).abs() < 1.0e-9);
    }
}
