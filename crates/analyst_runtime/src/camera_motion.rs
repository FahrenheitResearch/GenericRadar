//! How the camera GETS to where a gesture asked for, rather than where it ends
//! up.
//!
//! # What this replaces
//!
//! Nothing. Before this module a gesture wrote the camera outright: one wheel
//! notch multiplied `km_per_point` by 1.2 between two frames, a drag moved the
//! centre by the raw pointer delta, and a released drag stopped dead. Every
//! value was correct and every transition was a cut. That is what "navigation
//! is janky" names: not a wrong camera, an instantaneous one.
//!
//! # What it does NOT do
//!
//! It does not move the camera. It owns a small amount of state and answers,
//! once per frame, with a [`MotionStep`] — a zoom factor about a screen anchor
//! and a screen-space pan delta, in exactly the terms
//! [`Camera2D::zoom_about`](crate::Camera2D::zoom_about) and
//! [`Camera2D::pan_by_screen_delta`](crate::Camera2D::pan_by_screen_delta)
//! already take. The caller applies them through whatever it applies gestures
//! through.
//!
//! That indirection is the whole design, and it is deliberate:
//!
//! * `zoom_about`'s factor clamp is the guard against the anchor correction
//!   walking the map off screen when a notch runs into a scale limit — its own
//!   source calls that out, with the measured failure. A spring that wrote
//!   `km_per_point` directly would bypass it. Routing every integration step
//!   back through the same call preserves it verbatim, and preserves the
//!   `MIN_KM_PER_POINT`/`MAX_KM_PER_POINT` clamp with it.
//! * The workstation resolves gestures through a north-up frame that makes a
//!   gesture and its inverse compose to the identity. A motion expressed as a
//!   gesture inherits that too, instead of needing its own version of it.
//!
//! # The two motions, and why they are different shapes
//!
//! **Zoom is a critically damped spring on `ln(km_per_point)`.** Logarithmic
//! because the wheel response is geometric — a notch multiplies the scale — so
//! only in the log is a notch a fixed distance, and only there does a spring
//! feel the same at 40 km/point as at 0.05. Critically damped because a zoom
//! that overshoots and comes back is a zoom that has to be watched rather than
//! read: critical damping is the fastest approach with no overshoot at all, and
//! "no overshoot at all" is not a tuning preference here, it is the difference
//! between framing a hook echo and hunting for it.
//!
//! **Pan is 1:1 while the pointer is down, and an exponential glide after it.**
//! A finger is never sprung: the ground under the pointer is the ground under
//! the pointer, and a map that lags a drag by even a frame feels broken in a
//! way a map that lags a wheel does not. The inertia starts at release, from
//! the speed the hand was actually moving at, measured over a short window so a
//! drag that stopped before the button came up flings nowhere.
//!
//! # Termination is a requirement, not a detail
//!
//! A live motion asks for a repaint every frame. A motion that approaches its
//! target asymptotically and never arrives therefore pins the frame loop
//! forever: a laptop battery, and — because the radar raster is keyed on the
//! exact camera — an image that never settles either. Both motions here stop
//! EXACTLY: inside a threshold the remaining offset is emitted in one step and
//! the state is zeroed, so [`CameraMotion::is_idle`] becomes true and stays
//! true. `a_motion_settles_and_stops_asking_for_frames` pins it.

use crate::{MAX_NAV_STEP_SECONDS, MAX_SCALE_CHANGE_PER_FRAME, ScreenPoint};

/// How long the zoom spring takes to cover nine tenths of a retarget.
///
/// The figure that matters for feel is not the spring's own time constant but
/// how long the picture takes to arrive, so that is what this names. A
/// critically damped spring released from rest is at `1 - (1 + u) e^-u` of its
/// target after `u = omega * t`, so nine tenths is `u = 3.8897`
/// ([`ZOOM_NINE_TENTHS`]) and `omega` follows.
///
/// 130 ms is the top of the 110-130 ms band the design specifies, chosen there
/// because this application's notch is deliberately small (a fifth of the
/// scale) and a *smaller* step wants a *longer* glide to read as motion rather
/// than as a stutter. At a 60 Hz frame it puts 8.4% of the first notch on
/// screen in the frame the notch arrives, 26% by the second and 90% by the
/// eighth — so the picture starts moving in the same frame as the input, which
/// is the latency requirement, and finishes in under a fifth of a second.
pub const ZOOM_RESPONSE_SECONDS: f32 = 0.130;

/// The root of `(1 + u) e^-u = 0.1`: how many `omega * t` a critically damped
/// spring needs to cover nine tenths of its offset.
const ZOOM_NINE_TENTHS: f32 = 3.889_7;

/// Remaining log-scale offset below which the zoom is finished in one step.
///
/// `1e-4` in the natural log is a scale change of one part in ten thousand.
/// Across a 2560-point pane that moves the furthest pixel by a quarter of a
/// pixel, so there is nothing left to animate — and continuing to animate it is
/// precisely the infinite repaint the module docs refuse.
const ZOOM_SETTLE_LN: f32 = 1.0e-4;

/// Remaining log-scale speed below which the zoom is finished in one step.
/// Paired with [`ZOOM_SETTLE_LN`]: an offset can be small while the spring is
/// still travelling through it, and stopping then would cut the motion short.
const ZOOM_SETTLE_LN_PER_SECOND: f32 = 1.0e-3;

/// Ceiling on the log-scale offset the spring will hold.
///
/// `ln(MAX_KM_PER_POINT / MIN_KM_PER_POINT)` is 8.517: the whole legal range,
/// end to end. A target further away than that is a target outside the camera's
/// world, and holding it would make the spring spend frames travelling to a
/// scale that does not exist.
const MAX_PENDING_LN: f32 = 8.517_2;

/// Fastest fling a release may start, in screen points per second.
///
/// MapLibre GL JS's `DragPanOptions::maxSpeed`, kept because it is the number
/// a decade of web maps has been tuned against and because it is a property of
/// hands rather than of a renderer. A hand can flick faster than this; reading
/// it literally would throw the map further than anyone meant.
pub const PAN_FLING_MAX_SPEED_POINTS_PER_SECOND: f32 = 1_400.0;

/// Time constant of the fling's exponential decay.
///
/// A fling travels `speed * this` points in total, so the cap above puts the
/// longest possible fling at 455 points — a bit over a quarter of a
/// 1600-point pane. That is the number this was chosen by: a fling should
/// carry the view to the next thing the analyst can already see, not into
/// ground they have to come back from. MapLibre's own model reaches 1306
/// points from the same cap, which is a browser map's answer and not a
/// warning desk's.
pub const PAN_FLING_DECAY_SECONDS: f32 = 0.325;

/// Speed below which the fling is finished in one step, in points per second.
///
/// At [`PAN_FLING_DECAY_SECONDS`] a fling at this speed has 2.6 points of
/// travel left in it, which is a quarter of the smallest thing on the pane.
const PAN_FLING_STOP_POINTS_PER_SECOND: f32 = 8.0;

/// How much of the recent drag the release velocity is measured over.
///
/// Long enough to average out one dropped frame and the jitter of a real
/// pointer; short enough that a drag which decelerated to a stop before the
/// button came up is measured as stopped, because it was. 90 ms is five frames
/// at 60 Hz and thirteen at 144 Hz.
pub const PAN_VELOCITY_WINDOW_SECONDS: f64 = 0.090;

/// Frames of pointer history kept for the release velocity. Sixteen covers
/// [`PAN_VELOCITY_WINDOW_SECONDS`] at 165 Hz, which is past any display this
/// runs on; a faster one simply measures over a shorter window, which is
/// harmless.
const DRAG_SAMPLES: usize = 16;

/// One frame of pointer travel: how far, and over how long.
///
/// The DURATION is stored rather than inferred from the gap between
/// timestamps, because a sample's delta covers the interval that ENDS at its
/// timestamp. Dividing a sum of deltas by the span between the first and last
/// timestamp leaves out the first sample's own interval and reports a fling
/// about a sixth faster than the hand was moving.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DragSample {
    end_seconds: f64,
    duration_seconds: f32,
    delta_x_points: f32,
    delta_y_points: f32,
}

/// What one frame of motion asks the camera for.
///
/// Both fields are in the units the camera's own methods take, and the identity
/// is a factor of exactly 1 and a delta of exactly zero, so a caller can apply
/// this unconditionally or skip it on [`MotionStep::is_idle`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionStep {
    /// Multiply the camera's scale by this about `zoom_anchor`, exactly as a
    /// wheel notch does. `1.0` is no zoom.
    pub zoom_factor: f32,
    /// Pane-local screen point the zoom is anchored on — where the cursor was
    /// when the gesture that set the target arrived.
    pub zoom_anchor: ScreenPoint,
    /// Screen-space pan, in the same sense a drag delta has: the CONTENT moves
    /// this way.
    pub pan_delta_points: (f32, f32),
}

impl MotionStep {
    pub const NONE: Self = Self {
        zoom_factor: 1.0,
        zoom_anchor: ScreenPoint::new(0.0, 0.0),
        pan_delta_points: (0.0, 0.0),
    };

    #[must_use]
    pub fn is_idle(self) -> bool {
        self.zoom_factor == 1.0 && self.pan_delta_points == (0.0, 0.0)
    }
}

/// One pane's camera motion. Cheap to keep, and inert until a gesture asks it
/// for something.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraMotion {
    /// How much `ln(km_per_point)` still has to change to reach the target.
    ///
    /// Deliberately a REMAINDER and not an absolute target scale. A remainder
    /// is what makes the motion terminate even when the camera cannot honour
    /// it: scrolling into the scale ceiling leaves `zoom_about` a no-op, and a
    /// motion holding an absolute target would push against that wall forever
    /// while a motion holding a remainder simply spends it and stops. It is
    /// also what keeps this type free of the camera: it never reads one.
    zoom_pending_ln: f32,
    zoom_velocity_ln_per_second: f32,
    zoom_anchor: ScreenPoint,
    pan_velocity_points_per_second: (f32, f32),
    samples: [DragSample; DRAG_SAMPLES],
    /// Where the next sample goes; the ring is full once `sample_count` reaches
    /// `DRAG_SAMPLES`.
    sample_next: usize,
    sample_count: usize,
    /// End of the previous recorded sample, so a sample's duration is the
    /// interval it really covers.
    last_sample_seconds: Option<f64>,
}

impl Default for CameraMotion {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraMotion {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            zoom_pending_ln: 0.0,
            zoom_velocity_ln_per_second: 0.0,
            zoom_anchor: ScreenPoint::new(0.0, 0.0),
            pan_velocity_points_per_second: (0.0, 0.0),
            samples: [DragSample {
                end_seconds: 0.0,
                duration_seconds: 0.0,
                delta_x_points: 0.0,
                delta_y_points: 0.0,
            }; DRAG_SAMPLES],
            sample_next: 0,
            sample_count: 0,
            last_sample_seconds: None,
        }
    }

    /// Whether anything is still moving. A caller must keep asking for frames
    /// while this is false-negative-free, and may stop the moment it is `true`.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.zoom_pending_ln == 0.0
            && self.zoom_velocity_ln_per_second == 0.0
            && self.pan_velocity_points_per_second == (0.0, 0.0)
    }

    /// The zoom the spring is still carrying, as the factor that would finish
    /// it in one go. Diagnostics and tests; the camera never needs it.
    #[must_use]
    pub fn pending_zoom_factor(&self) -> f32 {
        (-self.zoom_pending_ln).exp()
    }

    /// Point a wheel notch, a pinch, or a keyboard zoom at the spring instead
    /// of at the camera.
    ///
    /// `factor` is exactly what would have been handed to `zoom_about`:
    /// `> 1` zooms in. Retargeting ADDS to whatever is still outstanding and
    /// leaves the spring's velocity alone, which is what turns a fast scroll
    /// into a fast glide rather than a queue of little ones — the burst gain
    /// the wheel response already computes arrives here as a bigger target,
    /// and the spring reads a bigger target as more speed.
    pub fn retarget_zoom(&mut self, factor: f32, anchor: ScreenPoint) {
        if !factor.is_finite() || factor <= 0.0 || factor == 1.0 {
            return;
        }
        // `zoom_about` divides the scale by the factor, so the scale has to
        // travel `-ln(factor)` in the log.
        let wanted = -factor.ln();
        if !wanted.is_finite() {
            return;
        }
        self.zoom_pending_ln =
            (self.zoom_pending_ln + wanted).clamp(-MAX_PENDING_LN, MAX_PENDING_LN);
        self.zoom_anchor = finite_anchor(anchor).unwrap_or(self.zoom_anchor);
    }

    /// One frame of pointer travel while the button is down.
    ///
    /// The caller applies the delta itself, 1:1 — this only remembers it, so
    /// that the release has a speed to fling with.
    pub fn record_drag(&mut self, delta_x_points: f32, delta_y_points: f32, now_seconds: f64) {
        if !delta_x_points.is_finite() || !delta_y_points.is_finite() || !now_seconds.is_finite() {
            return;
        }
        // A drag cancels a fling: the hand has taken the map back.
        self.pan_velocity_points_per_second = (0.0, 0.0);
        let duration = match self.last_sample_seconds {
            // A gap longer than the window is a new gesture, not a slow frame,
            // so it starts a fresh history rather than a single enormous
            // sample.
            Some(previous) if now_seconds > previous => {
                let gap = now_seconds - previous;
                if gap > PAN_VELOCITY_WINDOW_SECONDS {
                    self.clear_samples();
                    0.0
                } else {
                    gap as f32
                }
            }
            _ => 0.0,
        };
        self.last_sample_seconds = Some(now_seconds);
        self.samples[self.sample_next] = DragSample {
            end_seconds: now_seconds,
            duration_seconds: duration,
            delta_x_points,
            delta_y_points,
        };
        self.sample_next = (self.sample_next + 1) % DRAG_SAMPLES;
        self.sample_count = (self.sample_count + 1).min(DRAG_SAMPLES);
    }

    /// The pointer came up. Fling with whatever speed the last
    /// [`PAN_VELOCITY_WINDOW_SECONDS`] of it were moving at.
    pub fn release_drag(&mut self, now_seconds: f64) {
        let (mut vx, mut vy) = self.recent_velocity(now_seconds);
        let speed = vx.hypot(vy);
        if !speed.is_finite() || speed < PAN_FLING_STOP_POINTS_PER_SECOND {
            self.pan_velocity_points_per_second = (0.0, 0.0);
        } else {
            if speed > PAN_FLING_MAX_SPEED_POINTS_PER_SECOND {
                let scale = PAN_FLING_MAX_SPEED_POINTS_PER_SECOND / speed;
                vx *= scale;
                vy *= scale;
            }
            self.pan_velocity_points_per_second = (vx, vy);
        }
        self.clear_samples();
    }

    /// Abandon everything outstanding, without applying it.
    ///
    /// For the gestures that are not a destination but a decision — the reset
    /// key, the double-click home — where finishing a glide the analyst has
    /// already changed their mind about is the wrong answer.
    pub fn stop(&mut self) {
        self.zoom_pending_ln = 0.0;
        self.zoom_velocity_ln_per_second = 0.0;
        self.pan_velocity_points_per_second = (0.0, 0.0);
        self.clear_samples();
    }

    /// Advance by one frame and say what the camera should do.
    ///
    /// `dt_seconds` is clamped to [`MAX_NAV_STEP_SECONDS`] for the same reason
    /// keyboard flight clamps it: a stalled frame must not teleport the camera
    /// across the county because the motion kept running while nothing was
    /// drawn.
    pub fn step(&mut self, dt_seconds: f32) -> MotionStep {
        let dt = if dt_seconds.is_finite() {
            dt_seconds.clamp(0.0, MAX_NAV_STEP_SECONDS)
        } else {
            0.0
        };
        if dt <= 0.0 {
            return MotionStep {
                zoom_anchor: self.zoom_anchor,
                ..MotionStep::NONE
            };
        }
        MotionStep {
            zoom_factor: self.step_zoom(dt),
            zoom_anchor: self.zoom_anchor,
            pan_delta_points: self.step_pan(dt),
        }
    }

    /// One frame of the critically damped spring, integrated in CLOSED FORM.
    ///
    /// `x(t) = (x0 + (v0 + omega x0) t) e^{-omega t}` is the exact solution of
    /// `x'' = -2 omega x' - omega^2 x`, so this is not an approximation that
    /// can go unstable at a long frame — which a semi-implicit Euler step at
    /// `omega * dt = 3` genuinely can, and `MAX_NAV_STEP_SECONDS` allows
    /// exactly that.
    fn step_zoom(&mut self, dt: f32) -> f32 {
        if self.zoom_pending_ln == 0.0 && self.zoom_velocity_ln_per_second == 0.0 {
            return 1.0;
        }
        let omega = ZOOM_NINE_TENTHS / ZOOM_RESPONSE_SECONDS;
        let x = self.zoom_pending_ln;
        let v = self.zoom_velocity_ln_per_second;
        if x.abs() <= ZOOM_SETTLE_LN && v.abs() <= ZOOM_SETTLE_LN_PER_SECOND {
            // Finish exactly, and stop. Not "nearly zero": zero, so `is_idle`
            // is true on the bit pattern and the repaint loop ends.
            self.zoom_pending_ln = 0.0;
            self.zoom_velocity_ln_per_second = 0.0;
            return (-x).exp();
        }
        let decay = (-omega * dt).exp();
        let c = v + omega * x;
        let next_x = (x + c * dt) * decay;
        let next_v = (v - omega * c * dt) * decay;
        if !next_x.is_finite() || !next_v.is_finite() {
            self.stop();
            return 1.0;
        }
        // What the scale has to travel this frame, and therefore the factor:
        // `zoom_about` divides by the factor, so a shrinking remainder is a
        // factor above one.
        let travelled = next_x - x;
        let capped = travelled.clamp(
            -MAX_SCALE_CHANGE_PER_FRAME.ln(),
            MAX_SCALE_CHANGE_PER_FRAME.ln(),
        );
        // Books kept against what was APPLIED, not against what the spring
        // wanted, so a capped frame owes the difference rather than losing it.
        self.zoom_pending_ln = x + capped;
        self.zoom_velocity_ln_per_second = next_v;
        capped.exp()
    }

    /// One frame of the fling, also in closed form: `v(t) = v0 e^{-t/tau}`, so
    /// the distance covered is `v0 tau (1 - e^{-dt/tau})` however long the
    /// frame was.
    fn step_pan(&mut self, dt: f32) -> (f32, f32) {
        let (vx, vy) = self.pan_velocity_points_per_second;
        if vx == 0.0 && vy == 0.0 {
            return (0.0, 0.0);
        }
        let tau = PAN_FLING_DECAY_SECONDS;
        let decay = (-dt / tau).exp();
        if vx.hypot(vy) < PAN_FLING_STOP_POINTS_PER_SECOND {
            // Spend what is left in one step and stop, for the same reason the
            // zoom does.
            self.pan_velocity_points_per_second = (0.0, 0.0);
            return (vx * tau, vy * tau);
        }
        let travel = tau * (1.0 - decay);
        let step = (vx * travel, vy * travel);
        if !step.0.is_finite() || !step.1.is_finite() {
            self.pan_velocity_points_per_second = (0.0, 0.0);
            return (0.0, 0.0);
        }
        self.pan_velocity_points_per_second = (vx * decay, vy * decay);
        step
    }

    /// Mean pointer speed over the last [`PAN_VELOCITY_WINDOW_SECONDS`], in
    /// points per second. Zero when the window is empty, which is what a drag
    /// that stopped before it was released looks like.
    fn recent_velocity(&self, now_seconds: f64) -> (f32, f32) {
        if !now_seconds.is_finite() {
            return (0.0, 0.0);
        }
        let cutoff = now_seconds - PAN_VELOCITY_WINDOW_SECONDS;
        let mut sum = (0.0_f32, 0.0_f32);
        let mut span = 0.0_f32;
        for sample in self.samples.iter().take(self.sample_count) {
            if sample.end_seconds <= cutoff || sample.duration_seconds <= 0.0 {
                continue;
            }
            sum.0 += sample.delta_x_points;
            sum.1 += sample.delta_y_points;
            span += sample.duration_seconds;
        }
        if span <= 0.0 {
            return (0.0, 0.0);
        }
        (sum.0 / span, sum.1 / span)
    }

    fn clear_samples(&mut self) {
        self.samples = [DragSample::default(); DRAG_SAMPLES];
        self.sample_next = 0;
        self.sample_count = 0;
        self.last_sample_seconds = None;
    }
}

fn finite_anchor(anchor: ScreenPoint) -> Option<ScreenPoint> {
    (anchor.x.is_finite() && anchor.y.is_finite()).then_some(anchor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Camera2D, MAX_KM_PER_POINT, MIN_KM_PER_POINT, ViewportMetrics, WorldPoint, ZOOM_PER_NOTCH,
    };

    const VIEW: ViewportMetrics = ViewportMetrics {
        width_points: 1600.0,
        height_points: 900.0,
        pixels_per_point: 1.0,
    };

    const FRAME: f32 = 1.0 / 60.0;

    /// Run a motion to a standstill, applying every step to a camera exactly
    /// as `draw_pane` does. Returns the frames it took.
    fn settle(motion: &mut CameraMotion, camera: &mut Camera2D) -> usize {
        for frame in 1..=600_usize {
            let step = motion.step(FRAME);
            camera.zoom_about(step.zoom_factor, step.zoom_anchor, VIEW);
            camera.pan_by_screen_delta(step.pan_delta_points.0, step.pan_delta_points.1);
            if motion.is_idle() {
                return frame;
            }
        }
        panic!("the motion never settled");
    }

    #[test]
    fn a_fresh_motion_is_idle_and_asks_for_nothing() {
        let mut motion = CameraMotion::new();
        assert!(motion.is_idle());
        let step = motion.step(FRAME);
        assert!(step.is_idle());
        assert_eq!(step.zoom_factor, 1.0);
        assert_eq!(step.pan_delta_points, (0.0, 0.0));
        assert!(motion.is_idle());
        assert_eq!(motion, CameraMotion::default());
    }

    /// THE PROPERTY THE MODULE EXISTS FOR: a notch handed to the spring lands
    /// the camera on exactly the scale the same notch applied instantly would
    /// have, having taken a fifth of a second to get there instead of nothing.
    #[test]
    fn an_eased_notch_lands_on_the_scale_an_instant_notch_would_have() {
        for factor in [
            ZOOM_PER_NOTCH,
            1.0 / ZOOM_PER_NOTCH,
            ZOOM_PER_NOTCH.powi(5),
            ZOOM_PER_NOTCH.powi(-5),
            1.0001,
        ] {
            let start = Camera2D {
                center_east_km: 120.0,
                center_north_km: -64.0,
                km_per_point: 0.35,
                rotation_rad: 0.4,
            };
            let anchor = ScreenPoint::new(1_310.0, 240.0);

            let mut instant = start;
            instant.zoom_about(factor, anchor, VIEW);

            let mut motion = CameraMotion::new();
            motion.retarget_zoom(factor, anchor);
            let mut eased = start;
            let frames = settle(&mut motion, &mut eased);

            assert!(
                (eased.km_per_point / instant.km_per_point - 1.0).abs() < 1.0e-4,
                "factor {factor}: eased to {} against {}",
                eased.km_per_point,
                instant.km_per_point
            );
            // And the centre agrees too, so the anchor correction composed the
            // same way over many small steps as it did in one big one.
            let drift = (eased.center_east_km - instant.center_east_km)
                .hypot(eased.center_north_km - instant.center_north_km);
            let points = drift / f64::from(eased.km_per_point);
            assert!(
                points < 0.01,
                "factor {factor}: {points} screen points apart"
            );
            assert!(frames < 60, "factor {factor} took {frames} frames");
        }
    }

    /// THE ANCHOR SURVIVES THE EASING: whatever is under the cursor is still
    /// under the cursor, at every frame of the glide and not merely at the end.
    ///
    /// Measured with the FORWARD transform, for the reason
    /// `view::zoom_holds_the_world_point_under_the_cursor` gives: asking
    /// `screen_to_world` again measures the subtraction that produced the
    /// correction, not the picture.
    #[test]
    fn the_world_under_the_cursor_stays_under_it_for_the_whole_glide() {
        for factor in [ZOOM_PER_NOTCH.powi(3), ZOOM_PER_NOTCH.powi(-3)] {
            for anchor in [
                ScreenPoint::new(0.0, 0.0),
                ScreenPoint::new(1_600.0, 900.0),
                ScreenPoint::new(800.0, 450.0),
                ScreenPoint::new(1_480.0, 90.0),
            ] {
                let mut camera = Camera2D {
                    center_east_km: -240.0,
                    center_north_km: 310.0,
                    km_per_point: 1.2,
                    rotation_rad: -0.9,
                };
                let world = camera.screen_to_world(anchor, VIEW);
                let mut motion = CameraMotion::new();
                motion.retarget_zoom(factor, anchor);
                let mut worst = 0.0_f32;
                for _ in 0..600 {
                    let step = motion.step(FRAME);
                    camera.zoom_about(step.zoom_factor, step.zoom_anchor, VIEW);
                    let back = camera.world_to_screen(world, VIEW);
                    worst = worst.max((back.x - anchor.x).hypot(back.y - anchor.y));
                    if motion.is_idle() {
                        break;
                    }
                }
                assert!(
                    worst < 0.01,
                    "factor {factor} at ({}, {}): the picture slid {worst} points",
                    anchor.x,
                    anchor.y
                );
            }
        }
    }

    /// CRITICAL DAMPING: the glide approaches its target and does not pass it.
    /// A zoom that overshoots and comes back has to be watched instead of read.
    #[test]
    fn the_zoom_never_overshoots_its_target() {
        for factor in [ZOOM_PER_NOTCH, ZOOM_PER_NOTCH.powi(8), 1.0 / ZOOM_PER_NOTCH] {
            let mut motion = CameraMotion::new();
            motion.retarget_zoom(factor, VIEW.center());
            let start_pending = motion.zoom_pending_ln;
            let mut previous = start_pending.abs();
            for _ in 0..600 {
                motion.step(FRAME);
                let pending = motion.zoom_pending_ln;
                // Never past zero: the sign cannot flip.
                assert!(
                    pending * start_pending >= 0.0,
                    "factor {factor}: the spring overshot to {pending}"
                );
                // And monotone toward it.
                assert!(
                    pending.abs() <= previous + 1.0e-7,
                    "factor {factor}: the remainder grew from {previous} to {}",
                    pending.abs()
                );
                previous = pending.abs();
                if motion.is_idle() {
                    break;
                }
            }
            assert!(motion.is_idle());
        }
    }

    /// THE RESPONSE, in the units the constant is written in. Nine tenths of a
    /// retarget on screen after `ZOOM_RESPONSE_SECONDS`, and something on
    /// screen in the very first frame — which is the whole of the latency
    /// requirement.
    #[test]
    fn the_spring_covers_nine_tenths_in_its_stated_response_time() {
        let mut motion = CameraMotion::new();
        motion.retarget_zoom(ZOOM_PER_NOTCH, VIEW.center());
        let whole = motion.zoom_pending_ln;

        let first = motion.step(FRAME);
        assert!(
            first.zoom_factor > 1.0,
            "the first frame did not move at all: {}",
            first.zoom_factor
        );
        let covered = 1.0 - motion.zoom_pending_ln / whole;
        assert!(
            (0.05..0.15).contains(&covered),
            "one frame covered {covered} of the notch"
        );

        // Step to the stated response time and read the remainder.
        let mut elapsed = FRAME;
        while elapsed < ZOOM_RESPONSE_SECONDS {
            motion.step(FRAME);
            elapsed += FRAME;
        }
        let covered = 1.0 - motion.zoom_pending_ln / whole;
        assert!(
            (0.85..0.95).contains(&covered),
            "{ZOOM_RESPONSE_SECONDS} s covered {covered}, not nine tenths"
        );
    }

    /// A SPIN IS A FAST GLIDE, NOT A QUEUE OF SLOW ONES.
    ///
    /// Retargeting keeps the spring's velocity, so notches that arrive while
    /// the last one is still travelling add to the target and the glide speeds
    /// up. Without that a spin would feel like a series of little steps
    /// starting from rest, which is the syrup this module was written against.
    #[test]
    fn notches_arriving_during_a_glide_make_it_faster_not_longer() {
        let mut single = CameraMotion::new();
        single.retarget_zoom(ZOOM_PER_NOTCH, VIEW.center());
        let mut spun = CameraMotion::new();

        let mut single_camera = Camera2D::default();
        let mut spun_camera = Camera2D::default();
        // Five notches, one every other frame: an ordinary scroll.
        for frame in 0..600 {
            if frame < 10 && frame % 2 == 0 {
                spun.retarget_zoom(ZOOM_PER_NOTCH, VIEW.center());
            }
            let step = single.step(FRAME);
            single_camera.zoom_about(step.zoom_factor, step.zoom_anchor, VIEW);
            let step = spun.step(FRAME);
            spun_camera.zoom_about(step.zoom_factor, step.zoom_anchor, VIEW);
            if frame == 30 {
                break;
            }
        }
        // Half a second in, the spin has covered far more ground than one
        // notch, and it is not simply five sequential notches' worth of delay.
        let single_travel = (0.35_f32 / single_camera.km_per_point).ln();
        let spun_travel = (0.35_f32 / spun_camera.km_per_point).ln();
        assert!(
            spun_travel > single_travel * 4.0,
            "the spin covered {spun_travel} against one notch's {single_travel}"
        );
        // And both still land where their notches said, eventually.
        settle(&mut spun, &mut spun_camera);
        let wanted = 0.35_f32 / ZOOM_PER_NOTCH.powi(5);
        assert!(
            (spun_camera.km_per_point / wanted - 1.0).abs() < 1.0e-3,
            "five notches landed at {} instead of {wanted}",
            spun_camera.km_per_point
        );
    }

    /// TERMINATION. A live motion asks for a frame every frame, so one that
    /// never finishes pins the frame loop for good — the battery, and the
    /// drape, both depend on this stopping.
    #[test]
    fn a_motion_settles_and_stops_asking_for_frames() {
        for factor in [ZOOM_PER_NOTCH.powi(20), 1.0e-6, 1.0e6, ZOOM_PER_NOTCH] {
            let mut motion = CameraMotion::new();
            motion.retarget_zoom(factor, VIEW.center());
            let mut camera = Camera2D::default();
            let frames = settle(&mut motion, &mut camera);
            assert!(frames <= 60, "factor {factor} took {frames} frames");
            // Idle means idle: zero on the bit pattern, and the next step is
            // the exact identity so a caller that keeps stepping cannot be
            // nudged by it.
            assert!(motion.is_idle());
            let step = motion.step(FRAME);
            assert_eq!(step.zoom_factor.to_bits(), 1.0_f32.to_bits());
            assert_eq!(step.pan_delta_points, (0.0, 0.0));
            assert!(motion.is_idle());
        }

        // A fling too, from the fastest release the cap allows.
        let mut motion = CameraMotion::new();
        let mut now = 0.0_f64;
        for _ in 0..8 {
            now += f64::from(FRAME);
            motion.record_drag(90.0, -60.0, now);
        }
        motion.release_drag(now);
        assert!(!motion.is_idle(), "a hard flick did not fling");
        let mut camera = Camera2D::default();
        let frames = settle(&mut motion, &mut camera);
        assert!(frames <= 400, "the fling ran for {frames} frames");
    }

    /// A DRAG THAT STOPPED IS NOT A FLING. The commonest way to misread a
    /// pointer: the analyst drags to a feature, holds still on it, and lets go
    /// — and the map slides off it.
    #[test]
    fn a_drag_that_stopped_before_release_flings_nowhere() {
        let mut motion = CameraMotion::new();
        let mut now = 0.0_f64;
        for _ in 0..6 {
            now += f64::from(FRAME);
            motion.record_drag(40.0, 0.0, now);
        }
        // Two hundred milliseconds of holding still, reported as zero-delta
        // frames.
        for _ in 0..12 {
            now += f64::from(FRAME);
            motion.record_drag(0.0, 0.0, now);
        }
        motion.release_drag(now);
        assert!(motion.is_idle(), "a held pointer flung the map");

        // And a release with no drag at all — a click — flings nothing.
        let mut clicked = CameraMotion::new();
        clicked.release_drag(1.0);
        assert!(clicked.is_idle());
    }

    /// The fling goes the way the hand was going, at the speed the hand was
    /// going, and travels the distance the decay constant promises.
    #[test]
    fn a_fling_carries_the_hands_own_direction_and_speed() {
        let mut motion = CameraMotion::new();
        let mut now = 0.0_f64;
        // 12 points per frame at 60 Hz is 720 points per second.
        for _ in 0..8 {
            now += f64::from(FRAME);
            motion.record_drag(12.0, -6.0, now);
        }
        motion.release_drag(now);
        let (vx, vy) = motion.pan_velocity_points_per_second;
        assert!((vx - 720.0).abs() < 10.0, "vx {vx}");
        assert!((vy + 360.0).abs() < 10.0, "vy {vy}");

        let mut travel = (0.0_f32, 0.0_f32);
        for _ in 0..600 {
            let step = motion.step(FRAME);
            travel.0 += step.pan_delta_points.0;
            travel.1 += step.pan_delta_points.1;
            if motion.is_idle() {
                break;
            }
        }
        // `v0 * tau`, to the accuracy the stop threshold allows.
        let expected = 720.0 * PAN_FLING_DECAY_SECONDS;
        assert!(
            (travel.0 / expected - 1.0).abs() < 0.02,
            "travelled {} against {expected}",
            travel.0
        );
        // Direction preserved: the ratio of the two axes is the hand's.
        assert!((travel.1 / travel.0 + 0.5).abs() < 0.01, "{travel:?}");
    }

    /// The cap is on the SPEED, not on the distance, so a flick faster than a
    /// hand can be believed is read as the fastest believable one rather than
    /// thrown across the world.
    #[test]
    fn an_impossible_flick_is_capped_at_the_believable_speed() {
        let mut motion = CameraMotion::new();
        let mut now = 0.0_f64;
        for _ in 0..4 {
            now += f64::from(FRAME);
            motion.record_drag(4_000.0, 3_000.0, now);
        }
        motion.release_drag(now);
        let (vx, vy) = motion.pan_velocity_points_per_second;
        let speed = vx.hypot(vy);
        assert!(
            (speed - PAN_FLING_MAX_SPEED_POINTS_PER_SECOND).abs() < 1.0,
            "capped at {speed}"
        );
        // Still pointing where the hand pointed.
        assert!((vy / vx - 0.75).abs() < 1.0e-3, "{vx} {vy}");
    }

    /// A drag takes the map back from a fling in progress. Without this,
    /// grabbing a sliding map fights it for a third of a second.
    #[test]
    fn taking_hold_of_a_sliding_map_stops_it() {
        let mut motion = CameraMotion::new();
        let mut now = 0.0_f64;
        for _ in 0..8 {
            now += f64::from(FRAME);
            motion.record_drag(60.0, 0.0, now);
        }
        motion.release_drag(now);
        assert!(!motion.is_idle());
        now += f64::from(FRAME);
        motion.record_drag(0.0, 0.0, now);
        assert_eq!(motion.pan_velocity_points_per_second, (0.0, 0.0));
    }

    /// `stop` is for the gestures that are a decision rather than a
    /// destination: a reset must not be followed by the glide it interrupted.
    #[test]
    fn stopping_abandons_everything_outstanding() {
        let mut motion = CameraMotion::new();
        motion.retarget_zoom(ZOOM_PER_NOTCH.powi(4), ScreenPoint::new(10.0, 20.0));
        let mut now = 0.0_f64;
        for _ in 0..8 {
            now += f64::from(FRAME);
            motion.record_drag(50.0, 50.0, now);
        }
        motion.release_drag(now);
        assert!(!motion.is_idle());
        motion.stop();
        assert!(motion.is_idle());
        let mut camera = Camera2D::default();
        let held = camera;
        let step = motion.step(FRAME);
        camera.zoom_about(step.zoom_factor, step.zoom_anchor, VIEW);
        camera.pan_by_screen_delta(step.pan_delta_points.0, step.pan_delta_points.1);
        assert_eq!(camera.km_per_point.to_bits(), held.km_per_point.to_bits());
        assert_eq!(
            camera.center_east_km.to_bits(),
            held.center_east_km.to_bits()
        );
    }

    /// A STALLED FRAME MUST NOT TELEPORT THE CAMERA, for the same reason
    /// `MAX_NAV_STEP_SECONDS` exists for the keyboard: a volume landing or a
    /// shader compiling stops the loop, and the motion must not integrate the
    /// whole hitch in one go.
    #[test]
    fn a_stalled_frame_is_clamped_like_every_other_flight() {
        let mut stalled = CameraMotion::new();
        stalled.retarget_zoom(ZOOM_PER_NOTCH.powi(6), VIEW.center());
        let mut clamped = stalled;
        let long = stalled.step(5.0);
        let capped = clamped.step(MAX_NAV_STEP_SECONDS);
        assert_eq!(long, capped);
        assert_eq!(stalled, clamped);
    }

    /// Nothing any input path can hand this may make it produce a factor the
    /// camera cannot use, a delta that is not a number, or a motion that never
    /// ends.
    #[test]
    fn nonsense_input_never_produces_a_nonsense_step() {
        let nasty = [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.0,
            -0.0,
            -1.0,
            f32::MAX,
            f32::MIN,
            f32::MIN_POSITIVE,
            1.0e30,
            1.0,
        ];
        for factor in nasty {
            for anchor in [
                ScreenPoint::new(f32::NAN, 0.0),
                ScreenPoint::new(0.0, f32::INFINITY),
                ScreenPoint::new(700.0, 400.0),
            ] {
                let mut motion = CameraMotion::new();
                motion.retarget_zoom(factor, anchor);
                for delta in nasty {
                    motion.record_drag(delta, delta, f64::from(delta));
                }
                motion.release_drag(f64::NAN);
                let mut camera = Camera2D {
                    km_per_point: 3.0,
                    center_east_km: 500.0,
                    center_north_km: -220.0,
                    rotation_rad: 2.1,
                };
                for _ in 0..600 {
                    let step = motion.step(FRAME);
                    assert!(
                        step.zoom_factor.is_finite() && step.zoom_factor > 0.0,
                        "factor {factor} produced {}",
                        step.zoom_factor
                    );
                    assert!(
                        step.pan_delta_points.0.is_finite() && step.pan_delta_points.1.is_finite(),
                        "factor {factor} produced {:?}",
                        step.pan_delta_points
                    );
                    assert!(step.zoom_anchor.x.is_finite() && step.zoom_anchor.y.is_finite());
                    camera.zoom_about(step.zoom_factor, step.zoom_anchor, VIEW);
                    camera.pan_by_screen_delta(step.pan_delta_points.0, step.pan_delta_points.1);
                    assert_eq!(camera.sanitized(), camera, "factor {factor}");
                    if motion.is_idle() {
                        break;
                    }
                }
                assert!(motion.is_idle(), "factor {factor} never settled");
            }
        }
    }

    /// SCROLLING INTO THE WALL STILL STOPS. The camera clamps its own scale, so
    /// a target past the limit is a target `zoom_about` refuses; the motion
    /// holds a REMAINDER rather than an absolute target precisely so that it
    /// spends itself against the wall instead of pushing on it forever.
    #[test]
    fn a_target_past_the_scale_limit_settles_at_the_limit_and_stops() {
        for (start, factor, limit) in [
            (MAX_KM_PER_POINT, 1.0e-4_f32, MAX_KM_PER_POINT),
            (MIN_KM_PER_POINT, 1.0e4, MIN_KM_PER_POINT),
        ] {
            let mut motion = CameraMotion::new();
            motion.retarget_zoom(factor, ScreenPoint::new(1_500.0, 800.0));
            let mut camera = Camera2D {
                km_per_point: start,
                center_east_km: 88.0,
                center_north_km: -12.0,
                ..Camera2D::default()
            };
            let frames = settle(&mut motion, &mut camera);
            assert!(frames <= 120, "took {frames} frames");
            assert_eq!(camera.km_per_point, limit);
            assert!(camera.center_east_km.is_finite());
        }
    }

    /// The centre a fling produces is the centre the same drag would have
    /// produced, so inertia adds distance and nothing else — it does not
    /// introduce a second way for the camera to move.
    #[test]
    fn a_fling_pans_the_world_the_way_a_drag_does() {
        let mut motion = CameraMotion::new();
        let mut now = 0.0_f64;
        for _ in 0..8 {
            now += f64::from(FRAME);
            motion.record_drag(30.0, -15.0, now);
        }
        motion.release_drag(now);
        let mut camera = Camera2D::default();
        let world = WorldPoint::new(20.0, 10.0);
        let before = camera.world_to_screen(world, VIEW);
        let mut travel = (0.0_f32, 0.0_f32);
        for _ in 0..600 {
            let step = motion.step(FRAME);
            camera.pan_by_screen_delta(step.pan_delta_points.0, step.pan_delta_points.1);
            travel.0 += step.pan_delta_points.0;
            travel.1 += step.pan_delta_points.1;
            if motion.is_idle() {
                break;
            }
        }
        let after = camera.world_to_screen(world, VIEW);
        // The content moved with the fling, exactly as it moves with a drag.
        assert!((after.x - before.x - travel.0).abs() < 0.01, "{after:?}");
        assert!((after.y - before.y - travel.1).abs() < 0.01, "{after:?}");
        assert!(travel.0 > 100.0, "the fling barely moved: {travel:?}");
    }
}
