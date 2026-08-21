//! How far round the current sweep a pane is allowed to paint.
//!
//! A live volume does not grow radial by radial. A chunk lands and 240 radials
//! exist at once, then nothing arrives for a second or two. Painting a whole
//! chunk the instant it lands makes the picture jump a quarter turn; painting
//! nothing until the cut closes leaves an empty wedge sitting over the storm.
//! This module turns those bursts into a clockwise reveal that moves at the
//! antenna's own measured rate and never runs ahead of the data.
//!
//! # What the numbers here were measured against
//!
//! KTLX 2026-08-17 07:24:02 UTC, VCP 212, a real chunked live volume decoded by
//! this repository's own tooling:
//!
//! * `time_offset_ms` rises monotonically inside every one of the 19 cuts, and
//!   consecutive radials sit 20-38 ms apart, with zero gaps over 200 ms. Those
//!   are *antenna* times, evenly spaced - a chunk hands over ~240 radials whose
//!   timestamps claim they were collected 30 ms apart, all of which arrived in
//!   the same millisecond. So the timestamps are an excellent clock for the
//!   sweep RATE and useless for ARRIVAL. That split is the whole design: the
//!   reveal is driven forward by wall-clock `elapsed_since_last`, and only its
//!   speed comes out of the radials.
//! * The antenna rate is not a constant. In that single volume it was
//!   16.5 deg/s on the 720-radial Doppler legs, 22.3 deg/s on the 720-radial
//!   surveillance legs, and 27.9-30.2 deg/s on the 360-radial upper cuts. A
//!   hardcoded rate is wrong by nearly a factor of two on half the cuts, which
//!   is why [`SweepState::rate_deg_per_s`] is always measured from the cut in
//!   hand.
//! * A real partial cut held 240 of 360 radials, running from 197.5 deg upward,
//!   through 360/0, ending at 76.5 deg: a 121 deg unswept hole straddling the
//!   seam. Wrapping is the normal case here, not an edge case.
//!
//! # Everything is counted in degrees swept from the start
//!
//! Raw azimuth arithmetic fails on exactly the case above. `76.5 - 197.5` is
//! -121, and a renderer that believes that paints backwards across the storm
//! instead of forwards across the hole. So this module works in *degrees swept
//! from the first radial of the cut*: a monotonic 0..=360 quantity built by
//! summing the forward steps the radials describe. Azimuths exist
//! only at the boundary - read off the cut on the way in, rebuilt as
//! `start + swept` on the way out - and every comparison in between happens on
//! the swept quantity, where there is no seam to straddle.

// This is a binary crate, so `pub` buys nothing from the `dead_code` lint: a
// finished and tested state machine is "never used" until a pane calls it, and
// clippy runs with `-D warnings`. Remove this once a pane drives `observe`;
// anything still reported dead then really is dead.
#![allow(dead_code)]

use std::time::Duration;

use radar_core::ElevationCut;

/// One full turn of the antenna. Named so the reveal arithmetic reads as
/// geometry rather than as a magic 360.
const FULL_TURN_DEG: f64 = 360.0;

/// Antenna rate used when a cut cannot supply one of its own: fewer than two
/// radials, or timestamps that do not advance.
///
/// This number is a GUESS. It cannot be anything else: the measured rates in
/// one real VCP 212 volume ran from 16.5 deg/s to 30.2 deg/s, so no single
/// constant is right for more than a couple of cuts. It sits near the slow end
/// on purpose, because easing slower than the antenna only delays the reveal by
/// a fraction of a second, while easing faster is stopped dead by the frontier
/// clamp in [`SweepAnimator::observe`] and buys nothing.
const FALLBACK_RATE_DEG_PER_S: f32 = 20.0;

/// Slowest rate believed to come from a real antenna, in deg/s.
///
/// A glitched pair of timestamps implying 0.01 deg/s would freeze the reveal
/// for hours with the frontier sitting right there, which looks exactly like a
/// hung feed. Below this, the fallback is the safer lie.
const MIN_PLAUSIBLE_RATE_DEG_PER_S: f64 = 1.0;

/// Fastest rate believed to come from a real antenna, in deg/s. The WSR-88D
/// tops out near 30 deg/s; anything above this came from a truncated or
/// duplicated timestamp, not from an antenna.
const MAX_PLAUSIBLE_RATE_DEG_PER_S: f64 = 120.0;

/// How far the frontier may jump between two observations and still be eased
/// across.
///
/// This has to sit ABOVE a whole chunk. The frontier does not creep: it only
/// moves when a chunk lands, and the measured feed hands over ~240 radials at
/// once, which is 120 deg of new frontier on a 720-radial Doppler leg and
/// 240 deg on a 360-radial upper cut. Easing exactly those bursts is the reason
/// this module exists, so a threshold below them makes every real chunk snap,
/// leaves the ease running only while the frontier is already caught up, and
/// reduces the whole module to the quarter-turn jump it was written to remove.
///
/// It does not also have to be low enough to catch a pane pointed at a cut that
/// was already half full, because that case never reaches this test: an
/// animator with no track, and one whose track fails
/// [`Track::is_same_sweep_as`], snaps before any step is measured. What is left
/// to reject is a frontier that crosses three quarters of a turn in one
/// observation, which no single chunk has ever carried.
const SNAP_THRESHOLD_DEG: f64 = 270.0;

/// How much of a radial's own angular width counts toward closing the sweep.
///
/// Swept degrees are measured centre to centre, so a complete 360-radial cut
/// reports 359.0 deg of travel, not 360.0. Without this slack the pane would
/// hold a one-radial-wide unrevealed wedge open forever on every cut that ever
/// finished. One and a half radial widths also absorbs a single dropped radial
/// at the seam without claiming a sweep that is genuinely 90 deg short.
const COMPLETION_SLACK_RADIALS: f64 = 1.5;

/// Ceiling on the slack [`COMPLETION_SLACK_RADIALS`] is allowed to add.
///
/// That slack is a MEAN forward step, and a mean is only a radial width while
/// the radials are dense. A cut holding three radials 120 deg apart has a mean
/// step of 120 deg, so without this ceiling those three spokes claim 180 deg of
/// slack, report a closed sweep and paint the whole scope from three radials of
/// data - the exact failure the frontier clamp exists to make impossible.
/// Three degrees is one and a half of the widest radial this viewer ingests.
const MAX_COMPLETION_SLACK_DEG: f64 = 3.0;

/// How far an azimuth may read backwards from the previous radial and still be
/// called jitter rather than a gap in the data.
///
/// Within a cut the antenna only turns one way: the measured volume has
/// azimuths rising through every cut and `time_offset_ms` monotonic in all 19
/// of them. So a backwards reading is jitter, and jitter is a fraction of a
/// beam, never five of them.
const MAX_BACKWARD_JITTER_DEG: f64 = 5.0;

/// How far two cuts' start azimuths may differ and still be called the same
/// sweep. The first radial of a cut never moves while the cut grows, so this
/// only has to absorb float noise; a new volume restarting the same elevation
/// lands further away than this and is correctly treated as a new sweep.
const SAME_START_TOLERANCE_DEG: f64 = 0.25;

/// How far two cuts' elevation angles may differ and still be called the same
/// sweep. Matches the tolerance `radar_core::RadarVolume::find_or_insert_cut`
/// uses to decide the same question, so the two cannot disagree.
const SAME_ELEVATION_TOLERANCE_DEG: f32 = 0.05;

/// Below this much apparent backwards travel, a frontier that reads behind the
/// presentation is f32 rounding of two numbers the clamp already pinned
/// together, not a sweep one step short of closing.
const PENDING_ROUNDING_SLACK_DEG: f64 = 0.01;

/// What the renderer needs to know to draw a partially-arrived sweep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepState {
    /// Azimuth of the newest radial that has actually arrived, in degrees.
    pub frontier_deg: f32,
    /// How far the reveal has been eased, in degrees. Never ahead of the
    /// frontier: painting past it would be inventing data.
    pub presentation_deg: f32,
    /// Azimuth the sweep started from, in degrees.
    pub start_deg: f32,
    /// Measured antenna rate for this cut, degrees per second.
    pub rate_deg_per_s: f32,
    /// Degrees revealed so far, 0..=360.
    pub revealed_deg: f32,
    /// True once the sweep has come all the way round.
    pub complete: bool,
}

impl SweepState {
    /// Degrees of arrived-but-not-yet-revealed sweep: the gap the ease is still
    /// closing, measured forwards from the presentation azimuth to the
    /// frontier. Zero once the reveal has caught up, and zero for a complete
    /// sweep.
    ///
    /// Measured forwards on purpose. A caller that subtracted the two azimuths
    /// would read the measured 197.5 deg to 76.5 deg cut as -121 deg pending
    /// and treat the wrong 121 deg of the display as new.
    pub fn pending_deg(&self) -> f32 {
        if self.complete {
            return 0.0;
        }
        let ahead_deg = wrap_360(f64::from(self.frontier_deg) - f64::from(self.presentation_deg));
        if ahead_deg > FULL_TURN_DEG - PENDING_ROUNDING_SLACK_DEG {
            return 0.0;
        }
        ahead_deg as f32
    }
}

/// Turns bursty radial arrivals into a smooth clockwise reveal.
///
/// One animator follows one pane. Call [`SweepAnimator::reset`] whenever the
/// pane changes what it is looking at: the animator recognises a sweep by its
/// elevation, start azimuth and radial count, and a product or site change
/// leaves all three identical while every pixel underneath them changed.
#[derive(Default)]
pub struct SweepAnimator {
    /// What the animator remembers about the sweep it is following. `None`
    /// before the first observation and after a reset.
    track: Option<Track>,
    /// The state handed out by the last successful observation.
    state: Option<SweepState>,
}

impl SweepAnimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe the current state of a cut. `elapsed_since_last` is wall-clock
    /// time since the previous call, which is what advances the eased
    /// presentation azimuth between chunk arrivals.
    ///
    /// Returns `None` for a cut with no usable radials: there is nothing to
    /// reveal, and the animator forgets the sweep it was following rather than
    /// easing on from a position that no longer refers to anything.
    ///
    /// A `Duration` cannot be negative, so the only degenerate wall clock is a
    /// zero one, which advances the reveal by exactly zero degrees.
    pub fn observe(
        &mut self,
        cut: &ElevationCut,
        elapsed_since_last: Duration,
    ) -> Option<SweepState> {
        let Some(measured) = MeasuredSweep::from_cut(cut) else {
            self.track = None;
            self.state = None;
            return None;
        };

        // Only a track measured on THIS sweep may be eased from. Anything else
        // - a different cut, or the same elevation restarted by a new volume -
        // holds a presentation position that means nothing here.
        let earlier = self
            .track
            .filter(|earlier| earlier.is_same_sweep_as(&measured));

        let eased_swept_deg = match earlier {
            Some(earlier) if earlier.steps_smoothly_to(&measured) => {
                earlier.presentation_swept_deg
                    + f64::from(measured.rate_deg_per_s) * elapsed_since_last.as_secs_f64()
            }
            // Snap: either a sweep we were not following, or a frontier that
            // moved further in one observation than an ease should ever cover.
            _ => measured.frontier_swept_deg,
        };

        // The rule the whole module exists for: the reveal never passes the
        // frontier. Everything above is a proposal; this clamp is what makes
        // painting a spoke the radar has not sent impossible rather than
        // unlikely.
        let presentation_swept_deg = eased_swept_deg.clamp(0.0, measured.frontier_swept_deg);

        let caught_up = presentation_swept_deg >= measured.frontier_swept_deg;
        // Completion is sticky. A closed cut that re-reports its seam radial
        // would otherwise shrink its swept span, un-complete the pane, and
        // flick the unrevealed wedge back on over finished data.
        let complete =
            earlier.is_some_and(|earlier| earlier.complete) || (measured.is_closed && caught_up);

        // The one place the reveal is allowed past the frontier, and only by
        // the last radial's own beam width: once every radial has arrived there
        // is no unarrived data left to invent.
        let revealed_deg = if complete {
            FULL_TURN_DEG
        } else {
            presentation_swept_deg
        };

        let state = SweepState {
            frontier_deg: measured.frontier_deg as f32,
            presentation_deg: wrap_360(measured.start_deg + revealed_deg) as f32,
            start_deg: measured.start_deg as f32,
            rate_deg_per_s: measured.rate_deg_per_s,
            revealed_deg: revealed_deg as f32,
            complete,
        };

        self.track = Some(Track {
            elevation_number: measured.elevation_number,
            elevation_deg: measured.elevation_deg,
            radial_count: measured.radial_count,
            start_deg: measured.start_deg,
            frontier_swept_deg: measured.frontier_swept_deg,
            presentation_swept_deg,
            complete,
        });
        self.state = Some(state);
        Some(state)
    }

    /// Forget everything. Call when the pane changes cut, product or site.
    pub fn reset(&mut self) {
        self.track = None;
        self.state = None;
    }

    pub fn state(&self) -> Option<SweepState> {
        self.state
    }
}

/// What the animator remembers between calls about the sweep it is following.
///
/// Both angles are degrees swept from `start_deg`, never azimuths, so that
/// comparing them cannot trip over the 0/360 seam.
#[derive(Clone, Copy, Debug)]
struct Track {
    elevation_number: Option<u8>,
    elevation_deg: f32,
    radial_count: usize,
    start_deg: f64,
    /// Degrees swept from `start_deg` to the newest radial that had arrived.
    frontier_swept_deg: f64,
    /// Degrees swept from `start_deg` that the reveal had been eased to.
    presentation_swept_deg: f64,
    complete: bool,
}

impl Track {
    /// True when `measured` is the same sweep this track was following, so its
    /// eased position still means something.
    ///
    /// The radial count must not have fallen. A new volume restarting the same
    /// elevation from a similar azimuth is otherwise indistinguishable from the
    /// cut that just closed, and inheriting the closed position would paint a
    /// whole turn of a sweep that holds twenty radials.
    fn is_same_sweep_as(&self, measured: &MeasuredSweep) -> bool {
        self.elevation_number == measured.elevation_number
            && (self.elevation_deg - measured.elevation_deg).abs() <= SAME_ELEVATION_TOLERANCE_DEG
            && (self.start_deg - measured.start_deg).abs() <= SAME_START_TOLERANCE_DEG
            && measured.radial_count >= self.radial_count
    }

    /// True when the frontier crept forward by an amount an ease should follow.
    ///
    /// A backwards step is a sweep that lost radials and a huge forward step is
    /// a cut that was already half full when the pane arrived; both jump
    /// straight to the frontier instead of playing a wipe.
    fn steps_smoothly_to(&self, measured: &MeasuredSweep) -> bool {
        let step_deg = measured.frontier_swept_deg - self.frontier_swept_deg;
        (0.0..=SNAP_THRESHOLD_DEG).contains(&step_deg)
    }
}

/// Everything read straight off one cut, with no memory of earlier calls.
#[derive(Clone, Copy, Debug)]
struct MeasuredSweep {
    elevation_number: Option<u8>,
    elevation_deg: f32,
    radial_count: usize,
    /// Azimuth of the first radial of the cut, in degrees.
    start_deg: f64,
    /// Azimuth of the newest radial that has arrived, in degrees.
    frontier_deg: f64,
    /// Degrees swept from `start_deg` to `frontier_deg`, 0..=360.
    frontier_swept_deg: f64,
    /// Antenna rate measured from this cut's own radials, deg/s.
    rate_deg_per_s: f32,
    /// True once the radials have come all the way round.
    is_closed: bool,
}

impl MeasuredSweep {
    /// Measure a cut. `None` when it holds no radial with a usable azimuth,
    /// because a sweep with no start has no reveal.
    fn from_cut(cut: &ElevationCut) -> Option<Self> {
        let first = cut.radials.first()?;
        let last = cut.radials.last()?;

        // A NaN azimuth would poison the sum, and every later comparison
        // against a NaN is false - including the clamp that holds the reveal
        // behind the frontier. Dropping those radials costs a beam width each.
        // Refusing the whole cut because the FIRST azimuth is the unusable one
        // would instead blank a pane with 719 good radials sitting behind it,
        // so the sweep starts at the first radial that has an azimuth at all.
        let mut azimuths_deg = cut
            .radials
            .iter()
            .map(|radial| f64::from(radial.azimuth_deg))
            .filter(|azimuth_deg| azimuth_deg.is_finite());

        let start_deg = azimuths_deg.next()?;

        // Sum wrap-corrected steps rather than subtracting the end azimuths.
        // The measured 197.5 deg to 76.5 deg cut is 239 deg of sweep, and
        // `76.5 - 197.5` is -121: the subtraction is not a shortcut, it is the
        // wrong answer with the wrong sign.
        let mut previous_deg = start_deg;
        let mut swept_deg = 0.0;
        let mut forward_steps = 0_usize;
        for azimuth_deg in azimuths_deg {
            let step_deg = forward_step_deg(previous_deg, azimuth_deg);
            // A backwards step is antenna jitter, not sweep. Counting it would
            // let revealed_deg shrink and erase painted radials, so it is
            // dropped - and the cursor stays where it was. Measuring the next
            // step from the wobble instead charges the sum for that jitter
            // twice, once going back and once coming forward again, so a
            // 4 deg wobble on a 0.5 deg leg would add 3.5 deg of sweep that
            // never happened. Left in, that inflation reaches 360 early and
            // closes a cut that is still short.
            if step_deg > 0.0 {
                swept_deg += step_deg;
                forward_steps += 1;
                previous_deg = azimuth_deg;
            }
        }
        let frontier_swept_deg = swept_deg.min(FULL_TURN_DEG);

        let span_ms = i64::from(last.time_offset_ms) - i64::from(first.time_offset_ms);
        let rate_deg_per_s = measure_rate_deg_per_s(swept_deg, span_ms);

        let mean_step_deg = if forward_steps == 0 {
            0.0
        } else {
            swept_deg / forward_steps as f64
        };
        let completion_slack_deg =
            (mean_step_deg * COMPLETION_SLACK_RADIALS).min(MAX_COMPLETION_SLACK_DEG);
        let is_closed = swept_deg + completion_slack_deg >= FULL_TURN_DEG;

        Some(Self {
            elevation_number: cut.elevation_number,
            elevation_deg: cut.elevation_deg,
            radial_count: cut.radials.len(),
            start_deg,
            // The furthest the antenna has been seen to reach, NOT the last
            // radial's own azimuth. A radial that lands a fraction of a degree
            // behind the one before it is jitter that the swept sum already
            // ignored; reading the frontier straight off it would put the
            // frontier behind the presentation, and `pending_deg` would answer
            // "359.7 deg of new sweep to fade in" where the honest answer is
            // none.
            frontier_deg: wrap_360(start_deg + frontier_swept_deg),
            frontier_swept_deg,
            rate_deg_per_s,
            is_closed,
        })
    }
}

/// Antenna rate for one cut: total azimuth swept divided by the time its own
/// radials say that took.
///
/// Every degenerate input degrades to [`FALLBACK_RATE_DEG_PER_S`] rather than
/// propagating. A cut of one radial has no span and would divide by zero; a
/// non-monotonic pair of timestamps would produce a negative rate, which eases
/// the reveal backwards over radials already painted; and either can surface as
/// a NaN that turns the presentation azimuth into a NaN and every comparison
/// guarding it into false.
fn measure_rate_deg_per_s(swept_deg: f64, span_ms: i64) -> f32 {
    if span_ms <= 0 || swept_deg <= 0.0 {
        return FALLBACK_RATE_DEG_PER_S;
    }
    let rate_deg_per_s = swept_deg / (span_ms as f64 / 1000.0);
    if !rate_deg_per_s.is_finite()
        || !(MIN_PLAUSIBLE_RATE_DEG_PER_S..=MAX_PLAUSIBLE_RATE_DEG_PER_S).contains(&rate_deg_per_s)
    {
        return FALLBACK_RATE_DEG_PER_S;
    }
    rate_deg_per_s as f32
}

/// Fold an angle into 0..360.
fn wrap_360(degrees: f64) -> f64 {
    let wrapped = degrees % FULL_TURN_DEG;
    if wrapped < 0.0 {
        wrapped + FULL_TURN_DEG
    } else {
        wrapped
    }
}

/// Step from one azimuth to the next, read as forward travel unless it is a
/// backwards jitter smaller than [`MAX_BACKWARD_JITTER_DEG`]. Positive is
/// clockwise.
///
/// Two things have to come out right here, and they pull in opposite
/// directions. 359.8 deg to 0.2 deg has to be a 0.4 deg step FORWARD, not a
/// 359.6 deg step backward, which is the difference between revealing one
/// radial and erasing the whole sweep. But folding into (-180, 180] to get that
/// puts the coin toss in the wrong place: a 200 deg gap - two chunks of a
/// 360-radial cut missing between one radial and the next - then reads as a
/// 160 deg step BACKWARD, is dropped as jitter, and every radial past the gap
/// is left outside the painted arc even though it arrived. Splitting at one
/// beam width instead of at half a turn answers both: forward is the only
/// direction the antenna turns, so anything but a sliver of backwards travel is
/// a gap.
fn forward_step_deg(from_deg: f64, to_deg: f64) -> f64 {
    let forward_deg = wrap_360(to_deg - from_deg);
    if forward_deg > FULL_TURN_DEG - MAX_BACKWARD_JITTER_DEG {
        forward_deg - FULL_TURN_DEG
    } else {
        forward_deg
    }
}

/// How far behind the arrived data the reveal may sit before it is allowed to
/// run faster than the antenna did, in degrees.
///
/// At the antenna's own measured rate the reveal tracks the feed on average,
/// because on average the chunks arrive as fast as the radar turned. What it
/// cannot do at that rate is RECOVER: a burst that lands while the pane was
/// busy leaves the display a chunk behind and it stays there for the rest of
/// the sweep. So the reveal speeds up in proportion to how far behind it is,
/// which makes the backlog decay with a time constant instead of persisting,
/// and keeps a big burst a visible wipe rather than a jump.
const CATCHUP_REFERENCE_DEG: f32 = 45.0;

/// Ceiling on that speed-up. Past roughly six times the antenna rate a whole
/// turn goes by in under two seconds and the eye reads it as a jump, which is
/// the thing being avoided; there is nothing to gain above it.
const MAX_CATCHUP: f32 = 6.0;

/// How far two volumes' records of the same tilt may differ, in degrees.
const MATCHING_TILT_DEG: f32 = 0.5;

/// How much faster than the antenna the reveal may run, given how far behind it
/// is. One when caught up, rising with the backlog - see
/// [`CATCHUP_REFERENCE_DEG`].
///
/// Multiply the wall-clock delta handed to [`SweepAnimator::observe`] by this.
/// Scaling the caller's clock rather than the measured rate keeps the rate in
/// [`SweepState`] honest: it still reports what the antenna did, not how fast
/// the pane chose to draw it.
pub fn catch_up_factor(pending_deg: f32) -> f32 {
    if !pending_deg.is_finite() || pending_deg <= 0.0 {
        return 1.0;
    }
    (1.0 + pending_deg / CATCHUP_REFERENCE_DEG).min(MAX_CATCHUP)
}

/// Find the cut in `volume` that shows the same tilt as `target`, carrying
/// `moment`.
///
/// This is how a pane finds the last complete picture of the tilt now arriving,
/// so that the part of the sweep the antenna has not reached yet can keep
/// showing it instead of going blank.
///
/// Elevation number first, because it is the VCP's own name for the sweep: it
/// tells a SAILS repeat apart from the surveillance leg sitting at the same
/// angle, which no comparison of angles can. The fallback compares angles with
/// a deliberately wide tolerance - `ElevationCut::elevation_deg` is the FIRST
/// radial's angle, recorded while the antenna is still settling onto the new
/// tilt, so one sweep can be recorded a few tenths apart in two consecutive
/// volumes. Measured on KTLX 2026-08-17: the split legs of the lowest tilt
/// store 0.64 and 0.48 for beams whose medians are both 0.53.
pub fn matching_cut_index(
    volume: &radar_core::RadarVolume,
    target: &ElevationCut,
    moment: &radar_core::MomentType,
) -> Option<usize> {
    let carries_moment =
        |cut: &ElevationCut| cut.moments.contains_key(moment) && !cut.radials.is_empty();

    if let Some(number) = target.elevation_number
        && let Some(index) = volume
            .cuts
            .iter()
            .position(|cut| cut.elevation_number == Some(number) && carries_moment(cut))
    {
        return Some(index);
    }

    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| carries_moment(cut))
        .filter(|(_, cut)| (cut.elevation_deg - target.elevation_deg).abs() <= MATCHING_TILT_DEG)
        // Ties go to the fullest sweep: with SAILS several cuts sit at the same
        // angle, and the one that finished is the one worth underpainting with.
        .max_by_key(|(_, cut)| cut.radials.len())
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    use radar_core::{GateRange, Radial};

    fn radial(azimuth_deg: f32, time_offset_ms: i32) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms,
            gate_range: GateRange {
                first_gate_m: 2125,
                gate_spacing_m: 250,
                gate_count: 1832,
            },
            nyquist_velocity_mps: Some(26.0),
            radial_status: None,
        }
    }

    /// A cut shaped like every cut in the measured KTLX volume: radials
    /// stepping clockwise by a fixed width, a fixed number of milliseconds
    /// apart.
    fn cut(start_deg: f32, step_deg: f32, count: usize, ms_per_radial: i32) -> ElevationCut {
        cut_at_elevation(0.5, 1, start_deg, step_deg, count, ms_per_radial)
    }

    fn cut_at_elevation(
        elevation_deg: f32,
        elevation_number: u8,
        start_deg: f32,
        step_deg: f32,
        count: usize,
        ms_per_radial: i32,
    ) -> ElevationCut {
        let mut cut = ElevationCut::new(elevation_deg, Some(elevation_number));
        for index in 0..count {
            let azimuth_deg =
                wrap_360(f64::from(start_deg) + f64::from(step_deg) * index as f64) as f32;
            cut.radials
                .push(radial(azimuth_deg, ms_per_radial * index as i32));
        }
        cut
    }

    fn observed(animator: &mut SweepAnimator, cut: &ElevationCut, elapsed: Duration) -> SweepState {
        animator
            .observe(cut, elapsed)
            .expect("a cut with radials must produce a sweep state")
    }

    /// 360 radials 1.00 deg apart, 40 ms apart, is 359 deg in 14.360 s, which
    /// is 25.000 deg/s exactly. Hand-computed, not read back out of the code.
    #[test]
    fn the_rate_is_measured_from_the_cuts_own_timestamps() {
        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &cut(0.0, 1.0, 360, 40), Duration::ZERO);
        assert!(
            (state.rate_deg_per_s - 25.0).abs() < 1e-3,
            "measured rate was {} deg/s, expected 25.0",
            state.rate_deg_per_s
        );
    }

    /// Two cuts of one volume, shaped like the real ones: a 720-radial Doppler
    /// leg at 0.5 deg per 30 ms is 16.667 deg/s, and a 360-radial upper cut at
    /// 1.0 deg per 33 ms is 30.303 deg/s. No single constant is both, which is
    /// the entire reason the rate is measured.
    #[test]
    fn two_cuts_swept_at_different_rates_each_measure_their_own() {
        let mut doppler_leg = SweepAnimator::new();
        let doppler = observed(&mut doppler_leg, &cut(0.0, 0.5, 720, 30), Duration::ZERO);
        assert!(
            (doppler.rate_deg_per_s - 16.666_666).abs() < 1e-3,
            "the 720-radial leg measured {} deg/s, expected 16.667",
            doppler.rate_deg_per_s
        );

        let mut upper_cut = SweepAnimator::new();
        let upper = observed(&mut upper_cut, &cut(0.0, 1.0, 360, 33), Duration::ZERO);
        assert!(
            (upper.rate_deg_per_s - 30.303_03).abs() < 1e-3,
            "the 360-radial upper cut measured {} deg/s, expected 30.303",
            upper.rate_deg_per_s
        );

        assert!(
            (upper.rate_deg_per_s - doppler.rate_deg_per_s).abs() > 13.0,
            "the two cuts must not report the same rate"
        );
    }

    /// The measured partial cut: 240 of 360 radials from 197.5 deg upward,
    /// wrapping through 360/0, ending at 76.5 deg. 239 deg swept, with the
    /// 121 deg hole sitting on the seam.
    #[test]
    fn a_hole_that_straddles_the_zero_degree_seam_is_measured_the_long_way_round() {
        let mut animator = SweepAnimator::new();
        let partial = cut(197.5, 1.0, 240, 40);
        assert_eq!(
            partial.radials.last().expect("240 radials").azimuth_deg,
            76.5,
            "the fixture must reproduce the measured wrap"
        );

        let state = observed(&mut animator, &partial, Duration::ZERO);
        assert_eq!(state.start_deg, 197.5);
        assert_eq!(state.frontier_deg, 76.5);
        assert!(
            (state.revealed_deg - 239.0).abs() < 1e-3,
            "revealed {} deg, expected 239 deg swept the long way round",
            state.revealed_deg
        );
        assert!(!state.complete, "121 deg of the sweep has not arrived");
        // The reveal has caught up, so the presentation sits on the frontier -
        // not 121 deg the other side of the seam.
        assert_eq!(state.presentation_deg, 76.5);
        assert_eq!(state.pending_deg(), 0.0);
    }

    /// `presentation_deg` is always `start_deg` plus `revealed_deg`, folded
    /// into 0..360. A renderer painting the arc from the start through the
    /// revealed degrees must land exactly on the presentation azimuth, or the
    /// painted wedge and the reported frontier disagree across the seam.
    #[test]
    fn the_presentation_azimuth_is_always_the_start_plus_the_degrees_revealed() {
        let mut animator = SweepAnimator::new();
        let opening = cut(310.0, 0.5, 200, 30);
        let first = observed(&mut animator, &opening, Duration::ZERO);
        assert!(
            (first.presentation_deg - wrap_360(310.0 + f64::from(first.revealed_deg)) as f32).abs()
                < 1e-3,
            "presentation {} deg does not match start 310 plus revealed {} deg",
            first.presentation_deg,
            first.revealed_deg
        );

        let grown = cut(310.0, 0.5, 320, 30);
        for _ in 0..40 {
            let state = observed(&mut animator, &grown, Duration::from_millis(16));
            let expected_deg = wrap_360(310.0 + f64::from(state.revealed_deg)) as f32;
            assert!(
                (state.presentation_deg - expected_deg).abs() < 1e-3,
                "presentation {} deg does not match start 310 plus revealed {} deg",
                state.presentation_deg,
                state.revealed_deg
            );
        }
    }

    /// 25 deg/s across 16 ms frames is 0.4 deg a frame, so the 60 deg the chunk
    /// added takes 150 frames. Over 600 frames the reveal must approach the
    /// frontier and stop dead on it - never one degree past.
    #[test]
    fn the_presentation_azimuth_never_passes_the_frontier_over_a_long_run_of_small_steps() {
        let mut animator = SweepAnimator::new();
        let opening = cut(0.0, 1.0, 120, 40);
        let first = observed(&mut animator, &opening, Duration::ZERO);
        assert_eq!(first.revealed_deg, 119.0);

        let grown = cut(0.0, 1.0, 180, 40);
        for frame in 0..600 {
            let state = observed(&mut animator, &grown, Duration::from_millis(16));
            assert!(
                state.revealed_deg <= 179.0 + 1e-4,
                "frame {frame} revealed {} deg, past the 179 deg frontier",
                state.revealed_deg
            );
            assert!(
                state.pending_deg() <= 60.0 + 1e-4,
                "frame {frame} reported {} deg pending, more than the chunk added",
                state.pending_deg()
            );
        }

        let settled = animator.state().expect("600 observations produced a state");
        assert!(
            (settled.revealed_deg - 179.0).abs() < 1e-3,
            "the reveal settled at {} deg instead of pinning to the 179 deg frontier",
            settled.revealed_deg
        );
        assert_eq!(settled.pending_deg(), 0.0);
        assert!(!settled.complete, "half a sweep is not a complete sweep");
    }

    /// 300 deg of new frontier in one observation is more than any chunk has
    /// ever carried, so it is a cut that filled itself while nobody was
    /// watching. It appears whole rather than playing an eighteen second wipe.
    #[test]
    fn a_frontier_that_jumps_past_the_snap_threshold_snaps_instead_of_easing() {
        let mut animator = SweepAnimator::new();
        observed(&mut animator, &cut(0.0, 1.0, 20, 40), Duration::ZERO);

        let jumped = cut(0.0, 1.0, 320, 40);
        let state = observed(&mut animator, &jumped, Duration::from_millis(16));
        assert_eq!(
            state.revealed_deg, 319.0,
            "a jump past the snap threshold must reveal the whole cut at once"
        );
    }

    /// The failure this module was written to remove. A chunk hands over ~240
    /// radials at once, which is 120 deg of frontier on a 720-radial Doppler leg
    /// and 240 deg on a 360-radial upper cut, and the frontier moves at no other
    /// time. If a chunk-sized step counts as too big to ease, every real arrival
    /// snaps, the ease only ever runs while it is already caught up, and the
    /// picture jumps a third of a turn every few seconds.
    #[test]
    fn a_chunk_sized_frontier_jump_eases_at_the_measured_rate_instead_of_snapping() {
        // 720 radials 0.5 deg apart, 30 ms apart, is 16.667 deg/s, so one 16 ms
        // frame is 0.267 deg: 119.5 + 0.267 = 119.767.
        let mut doppler_leg = SweepAnimator::new();
        let first_chunk = observed(&mut doppler_leg, &cut(0.0, 0.5, 240, 30), Duration::ZERO);
        assert_eq!(first_chunk.revealed_deg, 119.5);

        let second_chunk = cut(0.0, 0.5, 480, 30);
        let state = observed(&mut doppler_leg, &second_chunk, Duration::from_millis(16));
        assert!(
            (state.revealed_deg - 119.766_67).abs() < 1e-3,
            "a 120 deg chunk arrival revealed {} deg in one frame, expected 119.767",
            state.revealed_deg
        );

        // 360 radials 1 deg apart, 33 ms apart, is 30.303 deg/s, so one 16 ms
        // frame is 0.485 deg: 59 + 0.485 = 59.485.
        let mut upper_cut = SweepAnimator::new();
        observed(&mut upper_cut, &cut(0.0, 1.0, 60, 33), Duration::ZERO);
        let state = observed(
            &mut upper_cut,
            &cut(0.0, 1.0, 300, 33),
            Duration::from_millis(16),
        );
        assert!(
            (state.revealed_deg - 59.484_85).abs() < 1e-3,
            "a 240 deg chunk arrival revealed {} deg in one frame, expected 59.485",
            state.revealed_deg
        );
    }

    /// The contrast that makes the snap meaningful: a 60 deg step is inside the
    /// threshold, so one 16 ms frame moves the reveal 0.4 deg and no further.
    #[test]
    fn a_frontier_step_inside_the_snap_threshold_eases_instead_of_snapping() {
        let mut animator = SweepAnimator::new();
        observed(&mut animator, &cut(0.0, 1.0, 120, 40), Duration::ZERO);

        let grown = cut(0.0, 1.0, 180, 40);
        let state = observed(&mut animator, &grown, Duration::from_millis(16));
        assert!(
            (state.revealed_deg - 119.4).abs() < 1e-3,
            "one 16 ms frame at 25 deg/s revealed {} deg, expected 119.4",
            state.revealed_deg
        );
        assert!(
            (state.pending_deg() - 59.6).abs() < 1e-3,
            "{} deg pending, expected 59.6",
            state.pending_deg()
        );
    }

    /// A closed sweep reports a full turn and holds it. Wrapping round would
    /// send `revealed_deg` back to zero and erase a finished cut a radial at a
    /// time.
    #[test]
    fn a_completed_sweep_stays_complete_and_never_wraps_round_to_erase() {
        let mut animator = SweepAnimator::new();
        let full = cut(197.5, 1.0, 360, 40);

        let first = observed(&mut animator, &full, Duration::ZERO);
        assert!(
            first.complete,
            "360 radials of a 360-radial cut is complete"
        );
        assert_eq!(first.revealed_deg, 360.0);
        assert_eq!(
            first.presentation_deg, 197.5,
            "a full turn ends where it started"
        );

        for second in 0..10 {
            let state = observed(&mut animator, &full, Duration::from_millis(1000));
            assert!(state.complete, "second {second} un-completed the sweep");
            assert_eq!(
                state.revealed_deg, 360.0,
                "second {second} erased the sweep"
            );
            assert_eq!(state.presentation_deg, 197.5);
            assert_eq!(state.pending_deg(), 0.0);
        }
    }

    /// A 720-radial cut sweeps 359.5 deg centre to centre. Without the
    /// completion slack it would never be called complete and the pane would
    /// hold a half-degree wedge open over a finished sweep.
    #[test]
    fn a_full_seven_hundred_and_twenty_radial_cut_counts_as_complete() {
        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &cut(88.0, 0.5, 720, 30), Duration::ZERO);
        assert!(state.complete, "719 steps of 0.5 deg closes the sweep");
        assert_eq!(state.revealed_deg, 360.0);
    }

    /// Three spokes are not a sweep. The completion slack is a mean step, and
    /// the mean step of three radials 120 deg apart is 120 deg, so an uncapped
    /// slack lets them claim a closed turn and paint the whole scope.
    #[test]
    fn three_radials_a_third_of_a_turn_apart_are_not_a_complete_sweep() {
        let mut sparse = ElevationCut::new(0.5, Some(1));
        sparse.radials.push(radial(0.0, 0));
        sparse.radials.push(radial(120.0, 4_000));
        sparse.radials.push(radial(240.0, 8_000));

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &sparse, Duration::ZERO);
        assert!(!state.complete, "three spokes are not a complete turn");
        assert_eq!(state.revealed_deg, 240.0);
    }

    /// The other side of that cap: capping it too hard would hold a wedge open
    /// forever on a coarse cut. 180 radials 2 deg apart sweep 358 deg centre to
    /// centre and must still close.
    #[test]
    fn a_full_turn_of_coarse_two_degree_radials_still_closes() {
        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &cut(31.0, 2.0, 180, 60), Duration::ZERO);
        assert!(state.complete, "179 steps of 2 deg closes the sweep");
        assert_eq!(state.revealed_deg, 360.0);
    }

    /// A gap wider than half a turn is still forward travel. Read as a step
    /// backwards it would be dropped as jitter, and then every radial past the
    /// gap sits outside the painted arc despite having arrived.
    #[test]
    fn a_gap_wider_than_half_a_turn_is_still_swept_rather_than_dropped() {
        let mut gapped = ElevationCut::new(0.5, Some(1));
        gapped.radials.push(radial(10.0, 0));
        gapped.radials.push(radial(210.0, 8_000));

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &gapped, Duration::ZERO);
        assert_eq!(
            state.revealed_deg, 200.0,
            "a 200 deg gap must read as 200 deg forward, not -160 deg dropped"
        );
        assert_eq!(state.frontier_deg, 210.0);
        assert_eq!(state.presentation_deg, 210.0);
        assert!(!state.complete, "two radials are not a complete turn");
    }

    /// A UI thread that stalls hands over one enormous frame. Rate times elapsed
    /// is 50 deg the first time and another 50 deg the second, which would put
    /// the reveal 40 deg past the frontier and paint spokes the radar never
    /// sent.
    #[test]
    fn a_two_second_stall_advances_the_reveal_but_still_stops_on_the_frontier() {
        let mut animator = SweepAnimator::new();
        observed(&mut animator, &cut(0.0, 1.0, 120, 40), Duration::ZERO);

        let grown = cut(0.0, 1.0, 180, 40);
        let stalled = observed(&mut animator, &grown, Duration::from_secs(2));
        assert!(
            (stalled.revealed_deg - 169.0).abs() < 1e-3,
            "two stalled seconds at 25 deg/s revealed {} deg, expected 169",
            stalled.revealed_deg
        );

        for stall in 0..20 {
            let state = observed(&mut animator, &grown, Duration::from_secs(2));
            assert_eq!(
                state.revealed_deg, 179.0,
                "stall {stall} pushed the reveal past the 179 deg frontier"
            );
        }
    }

    /// A pane resumed after being parked, or a clock that jumped, hands over a
    /// frame no antenna could have swept. Rate times elapsed then dwarfs a full
    /// turn, and the clamp is the only thing between that and a presentation
    /// azimuth of several thousand degrees.
    #[test]
    fn an_absurd_elapsed_time_clamps_to_the_frontier_instead_of_running_away() {
        for elapsed in [
            Duration::from_secs(3_600),
            Duration::from_secs(86_400),
            Duration::MAX,
        ] {
            let mut animator = SweepAnimator::new();
            observed(&mut animator, &cut(0.0, 1.0, 120, 40), Duration::ZERO);
            let state = observed(&mut animator, &cut(0.0, 1.0, 180, 40), elapsed);
            assert_eq!(
                state.revealed_deg, 179.0,
                "{elapsed:?} revealed {} deg instead of stopping on the frontier",
                state.revealed_deg
            );
            assert_eq!(state.presentation_deg, 179.0);
        }
    }

    /// One radial has no time span at all: the rate is a division by zero
    /// waiting to happen.
    #[test]
    fn a_single_radial_cut_falls_back_to_the_named_rate_instead_of_dividing_by_zero() {
        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &cut(12.0, 1.0, 1, 40), Duration::ZERO);
        assert_eq!(state.rate_deg_per_s, FALLBACK_RATE_DEG_PER_S);
        assert!(state.rate_deg_per_s.is_finite() && state.rate_deg_per_s > 0.0);
        assert_eq!(state.revealed_deg, 0.0);
        assert_eq!(state.start_deg, 12.0);
        assert_eq!(state.frontier_deg, 12.0);
        assert!(!state.complete);
    }

    /// Timestamps that run backwards would produce a negative rate, which eases
    /// the reveal backwards and erases radials that arrived.
    #[test]
    fn timestamps_that_run_backwards_fall_back_rather_than_measuring_a_negative_rate() {
        let mut backwards = ElevationCut::new(0.5, Some(1));
        backwards.radials.push(radial(0.0, 5_000));
        backwards.radials.push(radial(90.0, 1_000));

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &backwards, Duration::ZERO);
        assert_eq!(state.rate_deg_per_s, FALLBACK_RATE_DEG_PER_S);
        assert!(
            state.rate_deg_per_s > 0.0,
            "a negative rate must never escape"
        );
    }

    /// Two radials that claim the same collection time would divide by zero.
    #[test]
    fn timestamps_that_do_not_advance_fall_back_rather_than_dividing_by_zero() {
        let mut frozen = ElevationCut::new(0.5, Some(1));
        frozen.radials.push(radial(0.0, 7_000));
        frozen.radials.push(radial(1.0, 7_000));

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &frozen, Duration::ZERO);
        assert_eq!(state.rate_deg_per_s, FALLBACK_RATE_DEG_PER_S);
        assert!(state.rate_deg_per_s.is_finite());
    }

    /// 90 deg in 1 ms is 90 000 deg/s. No antenna does that, so it is a broken
    /// timestamp, and easing at it would whip the reveal round in a frame.
    #[test]
    fn an_impossible_measured_rate_falls_back_to_the_named_rate() {
        let mut impossible = ElevationCut::new(0.5, Some(1));
        impossible.radials.push(radial(0.0, 0));
        impossible.radials.push(radial(90.0, 1));

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &impossible, Duration::ZERO);
        assert_eq!(state.rate_deg_per_s, FALLBACK_RATE_DEG_PER_S);
    }

    /// A frame that took no measurable wall-clock time reveals nothing new. The
    /// data may have moved on; the presentation has not.
    #[test]
    fn an_elapsed_time_of_zero_does_not_move_the_presentation_azimuth() {
        let mut animator = SweepAnimator::new();
        let opening = cut(0.0, 1.0, 120, 40);
        let first = observed(&mut animator, &opening, Duration::ZERO);
        assert_eq!(first.revealed_deg, 119.0);

        let grown = cut(0.0, 1.0, 180, 40);
        let state = observed(&mut animator, &grown, Duration::ZERO);
        assert_eq!(
            state.revealed_deg, 119.0,
            "zero elapsed time moved the reveal"
        );
        assert_eq!(state.presentation_deg, first.presentation_deg);
        assert_eq!(
            state.frontier_deg, 179.0,
            "the frontier still reports the newest radial that arrived"
        );
        assert!((state.pending_deg() - 60.0).abs() < 1e-3);
    }

    /// A cut with no radials has no start, no frontier and nothing to reveal.
    #[test]
    fn a_cut_with_no_radials_reveals_nothing() {
        let mut animator = SweepAnimator::new();
        let empty = ElevationCut::new(0.5, Some(1));
        assert_eq!(animator.observe(&empty, Duration::ZERO), None);
        assert_eq!(animator.state(), None);
    }

    /// An empty cut also drops the sweep being followed, so the next cut snaps
    /// rather than easing from a position measured on data that is gone.
    #[test]
    fn an_empty_cut_forgets_the_sweep_that_was_being_followed() {
        let mut animator = SweepAnimator::new();
        observed(&mut animator, &cut(0.0, 1.0, 120, 40), Duration::ZERO);
        assert_eq!(
            animator.observe(&ElevationCut::new(0.5, Some(1)), Duration::ZERO),
            None
        );

        let state = observed(&mut animator, &cut(0.0, 1.0, 180, 40), Duration::ZERO);
        assert_eq!(
            state.revealed_deg, 179.0,
            "the next cut must snap, not ease from a forgotten position"
        );
    }

    /// Switching a pane to another elevation must not wipe slowly across a cut
    /// that was collected minutes ago.
    #[test]
    fn switching_to_a_different_cut_snaps_rather_than_easing_across_it() {
        let mut animator = SweepAnimator::new();
        observed(
            &mut animator,
            &cut_at_elevation(0.5, 1, 0.0, 1.0, 300, 40),
            Duration::ZERO,
        );

        let other = cut_at_elevation(4.3, 7, 0.0, 1.0, 60, 33);
        let state = observed(&mut animator, &other, Duration::from_millis(16));
        assert_eq!(
            state.revealed_deg, 59.0,
            "the new cut must show its own 59 deg, not inherit 299 deg"
        );
        assert!(
            (state.rate_deg_per_s - 30.303_03).abs() < 1e-3,
            "the new cut must be measured at its own rate, got {} deg/s",
            state.rate_deg_per_s
        );
    }

    /// A new volume restarting the same elevation holds fewer radials than the
    /// cut that just finished. Inheriting the finished position would paint a
    /// whole turn of a sweep that has forty radials.
    #[test]
    fn a_new_volume_restarting_the_same_elevation_starts_its_reveal_over() {
        let mut animator = SweepAnimator::new();
        let finished = observed(&mut animator, &cut(0.0, 1.0, 360, 40), Duration::ZERO);
        assert!(finished.complete);

        let restarted = cut(0.0, 1.0, 40, 40);
        let state = observed(&mut animator, &restarted, Duration::from_millis(16));
        assert_eq!(state.revealed_deg, 39.0);
        assert!(!state.complete, "a 40-radial cut is not a complete sweep");
    }

    /// Reset is what the pane calls when it changes product or site, where the
    /// cut can be identical and the pixels are not.
    #[test]
    fn reset_forgets_the_tracked_sweep_so_the_next_observation_snaps() {
        let mut animator = SweepAnimator::new();
        observed(&mut animator, &cut(0.0, 1.0, 120, 40), Duration::ZERO);
        animator.reset();
        assert_eq!(animator.state(), None);

        let state = observed(&mut animator, &cut(0.0, 1.0, 180, 40), Duration::ZERO);
        assert_eq!(
            state.revealed_deg, 179.0,
            "after a reset the next cut appears whole"
        );
    }

    /// A cut that grows from 29 deg short of the seam to 29 deg past it gains
    /// exactly 60 deg of sweep.
    #[test]
    fn a_reveal_that_crosses_the_zero_degree_seam_gains_sweep_rather_than_losing_it() {
        let mut animator = SweepAnimator::new();
        let before_seam = cut(300.0, 1.0, 30, 40);
        let first = observed(&mut animator, &before_seam, Duration::ZERO);
        assert_eq!(first.frontier_deg, 329.0);
        assert_eq!(first.revealed_deg, 29.0);

        let across_seam = cut(300.0, 1.0, 90, 40);
        assert_eq!(
            across_seam.radials.last().expect("90 radials").azimuth_deg,
            29.0,
            "the fixture must cross the seam"
        );
        let state = observed(&mut animator, &across_seam, Duration::from_secs(10));
        assert_eq!(state.frontier_deg, 29.0);
        assert_eq!(
            state.revealed_deg, 89.0,
            "crossing the seam must add 60 deg, not subtract 300"
        );
        assert_eq!(state.presentation_deg, 29.0);
    }

    /// One unusable azimuth in the first radial must not blank the pane. The
    /// sweep opens at the first radial that has an azimuth at all, and the 89
    /// good radials behind it are still revealed.
    #[test]
    fn a_cut_whose_first_azimuth_is_unusable_still_reveals_the_radials_behind_it() {
        for bad_azimuth_deg in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut poisoned = cut(0.0, 1.0, 90, 40);
            poisoned.radials[0].azimuth_deg = bad_azimuth_deg;

            let mut animator = SweepAnimator::new();
            let state = observed(&mut animator, &poisoned, Duration::ZERO);
            assert_eq!(
                state.start_deg, 1.0,
                "the sweep must open at the first usable azimuth"
            );
            assert_eq!(
                state.revealed_deg, 88.0,
                "the 89 radials behind the bad one describe 88 deg of sweep"
            );
            assert_eq!(state.frontier_deg, 89.0);
        }
    }

    /// A cut where no radial has a usable azimuth has no start, so it has no
    /// reveal either.
    #[test]
    fn a_cut_with_no_usable_azimuth_at_all_reveals_nothing() {
        let mut hopeless = cut(0.0, 1.0, 40, 40);
        for radial in &mut hopeless.radials {
            radial.azimuth_deg = f32::NAN;
        }

        let mut animator = SweepAnimator::new();
        assert_eq!(animator.observe(&hopeless, Duration::ZERO), None);
        assert_eq!(animator.state(), None);
    }

    /// A non-finite azimuth must not turn the reveal into a NaN, because every
    /// comparison against a NaN is false and the clamp that keeps the reveal
    /// behind the frontier would silently stop working.
    #[test]
    fn a_radial_with_a_non_finite_azimuth_is_skipped_rather_than_poisoning_the_reveal() {
        let mut poisoned = cut(0.0, 1.0, 90, 40);
        poisoned.radials[45].azimuth_deg = f32::NAN;

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &poisoned, Duration::ZERO);
        assert!(state.revealed_deg.is_finite());
        assert!(state.presentation_deg.is_finite());
        assert!(state.rate_deg_per_s.is_finite());
        assert_eq!(
            state.revealed_deg, 89.0,
            "the surviving radials still describe 89 deg of sweep"
        );
    }

    /// The risk that comes with reading a backwards step as forward travel:
    /// real sub-beam jitter must still be dropped. Counted as travel instead, a
    /// 0.3 deg wobble becomes 359.7 deg of sweep and closes a cut that holds a
    /// quarter of one.
    #[test]
    fn sub_beam_backwards_jitter_is_dropped_rather_than_read_as_a_turn_forward() {
        for jitter_deg in [0.05_f64, 0.3, 1.0, 4.0] {
            let mut wobbling = ElevationCut::new(0.5, Some(1));
            let mut azimuth_deg = 0.0_f64;
            for index in 0..90_i32 {
                wobbling
                    .radials
                    .push(radial(azimuth_deg as f32, index * 40));
                azimuth_deg = wrap_360(azimuth_deg + 1.0);
                if index == 44 {
                    azimuth_deg = wrap_360(azimuth_deg - jitter_deg);
                }
            }

            let mut animator = SweepAnimator::new();
            let state = observed(&mut animator, &wobbling, Duration::ZERO);
            assert!(
                !state.complete,
                "a {jitter_deg} deg wobble closed a 90 radial sweep"
            );
            // The wobble costs the radials it swallowed and nothing more: the
            // walk resumes from 44 deg, the furthest point actually reached, so
            // the sweep is 85 deg rather than the 89 deg of a clean cut. An
            // inflated answer here means the cursor followed the wobble
            // backwards and charged the return trip as sweep.
            let expected_deg = 89.0 - jitter_deg as f32;
            assert!(
                (state.revealed_deg - expected_deg).abs() < 1e-3,
                "a {jitter_deg} deg wobble revealed {} deg, expected {expected_deg}",
                state.revealed_deg
            );
        }
    }

    /// A radial that lands behind the one before it must not drag the reported
    /// frontier back with it. Read straight off the last radial, a 0.3 deg
    /// wobble at the tail leaves the frontier BEHIND the presentation, and
    /// `pending_deg` - the width of the fade wedge a renderer paints over the
    /// newest data - then reads 359.7 deg instead of nothing at all.
    #[test]
    fn a_wobble_at_the_tail_does_not_drag_the_frontier_behind_the_reveal() {
        let mut wobbling = cut(0.0, 1.0, 46, 40);
        wobbling.radials.push(radial(44.7, 46 * 40));

        let mut animator = SweepAnimator::new();
        let state = observed(&mut animator, &wobbling, Duration::ZERO);
        assert_eq!(state.revealed_deg, 45.0);
        assert_eq!(
            state.frontier_deg, 45.0,
            "the frontier fell back onto the wobble"
        );
        assert_eq!(
            state.pending_deg(),
            0.0,
            "a caught-up reveal reported a fade wedge"
        );
    }

    /// `RadarVolume::find_or_insert_cut` matches on elevation ANGLE as well as
    /// number, so both legs of a VCP 212 split cut land in one `ElevationCut`
    /// and its radials come round twice. The reveal has to saturate, not wrap
    /// past 360 and start erasing what it painted on the first turn.
    #[test]
    fn a_cut_that_sweeps_more_than_one_turn_saturates_instead_of_wrapping_back() {
        let mut animator = SweepAnimator::new();
        let mut previous_deg = 0.0_f32;
        for count in (60..=1440).step_by(60) {
            let state = observed(
                &mut animator,
                &cut(88.0, 0.5, count, 30),
                Duration::from_secs(30),
            );
            assert!(
                state.revealed_deg >= previous_deg,
                "{count} radials sent the reveal backwards: {previous_deg} -> {}",
                state.revealed_deg
            );
            assert!(
                state.revealed_deg <= 360.0,
                "{count} radials revealed {} deg",
                state.revealed_deg
            );
            previous_deg = state.revealed_deg;
        }
        assert_eq!(previous_deg, 360.0);
    }

    /// Every invariant at once, against arrival patterns nobody thought to
    /// write down: random chunk sizes, random frame times, both radial widths,
    /// random start azimuths. The seed is fixed, so a failure here is a
    /// reproducible failure and not a flake.
    #[test]
    fn no_random_sequence_of_arrivals_and_frame_times_breaks_an_invariant() {
        let mut seed = 0x2026_0817_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for trial in 0..300 {
            let start_deg = f64::from(next() as u32 % 3600) / 10.0;
            let sparse = trial % 2 == 0;
            let step_deg = if sparse { 1.0_f64 } else { 0.5 };
            let ms_per_radial = if sparse { 40 } else { 30 };
            let radial_limit = (FULL_TURN_DEG / step_deg) as usize;

            let mut animator = SweepAnimator::new();
            let mut count = 0_usize;
            let mut previous_deg = 0.0_f32;
            let mut was_complete = false;

            for observation in 0..80 {
                if next() % 3 == 0 {
                    count = (count + 1 + next() as usize % 300).min(radial_limit);
                }
                if count == 0 {
                    continue;
                }
                let grown = cut(start_deg as f32, step_deg as f32, count, ms_per_radial);
                let elapsed = Duration::from_millis(u64::from(next() as u32 % 3000));
                let state = observed(&mut animator, &grown, elapsed);

                let at = format!("trial {trial} observation {observation}, {count} radials");
                assert!(
                    state.revealed_deg.is_finite() && (0.0..=360.0).contains(&state.revealed_deg),
                    "{at}: revealed {} deg",
                    state.revealed_deg
                );
                assert!(
                    state.presentation_deg.is_finite()
                        && (0.0..360.0).contains(&state.presentation_deg),
                    "{at}: presentation {} deg",
                    state.presentation_deg
                );
                assert!(
                    state.rate_deg_per_s.is_finite() && state.rate_deg_per_s > 0.0,
                    "{at}: rate {} deg/s",
                    state.rate_deg_per_s
                );
                let frontier_swept_deg = ((count - 1) as f64 * step_deg).min(FULL_TURN_DEG);
                assert!(
                    f64::from(state.revealed_deg) <= frontier_swept_deg + 1e-3 || state.complete,
                    "{at}: revealed {} deg, past the {frontier_swept_deg} deg frontier",
                    state.revealed_deg
                );
                assert!(
                    state.revealed_deg >= previous_deg - 1e-3,
                    "{at}: the reveal went backwards, {previous_deg} -> {}",
                    state.revealed_deg
                );
                assert!(
                    !was_complete || state.complete,
                    "{at}: a completed sweep was un-completed"
                );
                let expected_deg = wrap_360(start_deg + f64::from(state.revealed_deg)) as f32;
                assert!(
                    (state.presentation_deg - expected_deg).abs() < 1e-2,
                    "{at}: presentation {} deg is not start plus revealed {expected_deg} deg",
                    state.presentation_deg
                );
                previous_deg = state.revealed_deg;
                was_complete = state.complete;
            }
        }
    }

    #[test]
    fn a_step_across_the_seam_is_forwards_and_only_a_sliver_backwards_is_jitter() {
        assert!((forward_step_deg(359.8, 0.2) - 0.4).abs() < 1e-9);
        assert!((forward_step_deg(0.2, 359.8) + 0.4).abs() < 1e-9);
        // Past one beam of backwards travel the antenna cannot have reversed, so
        // the reading is a gap in the data and counts forward.
        assert!((forward_step_deg(10.0, 210.0) - 200.0).abs() < 1e-9);
        assert!((forward_step_deg(0.0, 350.0) - 350.0).abs() < 1e-9);
        assert!((wrap_360(-1.0) - 359.0).abs() < 1e-9);
        assert!((wrap_360(436.5) - 76.5).abs() < 1e-9);
    }

    // ---- catch-up and previous-tilt matching -----------------------------
    //
    // These two are what turns the reveal above into something a pane can
    // actually draw: how fast it is allowed to close a backlog, and where the
    // picture under the unswept wedge comes from.

    fn tilt(
        elevation_number: Option<u8>,
        elevation_deg: f32,
        radial_count: usize,
        moment: radar_core::MomentType,
    ) -> ElevationCut {
        let mut cut = ElevationCut::new(elevation_deg, elevation_number);
        for index in 0..radial_count {
            cut.radials.push(radial(
                index as f32 * (360.0 / radial_count as f32),
                index as i32 * 30,
            ));
        }
        let gate_range = radar_core::GateRange {
            first_gate_m: 2_000,
            gate_spacing_m: 250,
            gate_count: 4,
        };
        cut.moments.insert(
            moment.clone(),
            radar_core::MomentGrid {
                moment,
                producer_description: None,
                producer_units: None,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                snr_threshold_db: None,
                recombination: None,
                radial_indices: (0..radial_count).collect(),
                storage: radar_core::MomentStorage::U8(vec![64; radial_count * 4]),
            },
        );
        cut
    }

    fn volume_of(cuts: Vec<ElevationCut>) -> radar_core::RadarVolume {
        let mut volume = radar_core::RadarVolume::new(
            radar_core::RadarSite::new("KTLX"),
            chrono::DateTime::UNIX_EPOCH,
        );
        volume.cuts = cuts;
        volume
    }

    #[test]
    fn a_caught_up_reveal_runs_at_the_antennas_own_rate() {
        // The whole design rests on the measured rate being the honest speed,
        // so with nothing to catch up the multiplier has to be exactly one.
        assert_eq!(catch_up_factor(0.0), 1.0);
        // A reveal reporting a negative or non-finite backlog is a caller bug,
        // not licence to run backwards or to multiply a duration by NaN.
        assert_eq!(catch_up_factor(-30.0), 1.0);
        assert_eq!(catch_up_factor(f32::NAN), 1.0);
        assert_eq!(catch_up_factor(f32::INFINITY), 1.0);
    }

    #[test]
    fn a_backlog_speeds_the_reveal_up_but_never_past_the_ceiling() {
        // One chunk behind on a 720-radial leg is about 120 degrees. At the
        // antenna's 16.5 deg/s that would take over seven seconds to clear and
        // the pane would sit a whole chunk behind for the rest of the sweep.
        let one_chunk = catch_up_factor(120.0);
        assert!(
            (one_chunk - 3.6667).abs() < 1e-3,
            "120 deg behind gave {one_chunk}"
        );
        assert!(catch_up_factor(45.0) > catch_up_factor(10.0));
        // A whole turn behind must not turn the wipe into a jump.
        assert_eq!(catch_up_factor(359.0), MAX_CATCHUP);
        assert_eq!(catch_up_factor(1e9), MAX_CATCHUP);
    }

    #[test]
    fn the_previous_tilt_is_found_by_elevation_number_not_by_angle() {
        // A SAILS VCP holds several cuts at the same angle. Matching on the
        // angle alone would underpaint a 0.5 degree SAILS repeat with the
        // surveillance leg, which is a different beam taken at a different
        // time; the elevation number is the VCP's own name for the sweep.
        let previous = volume_of(vec![
            tilt(Some(1), 0.53, 720, radar_core::MomentType::Velocity),
            tilt(Some(2), 0.48, 720, radar_core::MomentType::Velocity),
            tilt(Some(15), 0.51, 720, radar_core::MomentType::Velocity),
        ]);
        let arriving = tilt(Some(15), 0.62, 90, radar_core::MomentType::Velocity);

        assert_eq!(
            matching_cut_index(&previous, &arriving, &radar_core::MomentType::Velocity),
            Some(2),
            "matched by angle instead of by elevation number"
        );
    }

    #[test]
    fn a_tilt_without_the_moment_on_screen_is_not_offered_as_the_previous_one() {
        // The lowest tilt of a split cut carries reflectivity on the
        // surveillance leg and velocity on the Doppler leg. Underpainting a
        // velocity pane with the leg that has no velocity in it would render an
        // empty frame and report it as the previous picture.
        let previous = volume_of(vec![
            tilt(Some(1), 0.53, 720, radar_core::MomentType::Reflectivity),
            tilt(Some(2), 0.53, 720, radar_core::MomentType::Velocity),
        ]);
        let arriving = tilt(Some(1), 0.53, 40, radar_core::MomentType::Velocity);

        assert_eq!(
            matching_cut_index(&previous, &arriving, &radar_core::MomentType::Velocity),
            Some(1),
            "elevation number 1 matched even though that leg has no velocity"
        );
        assert_eq!(
            matching_cut_index(&previous, &arriving, &radar_core::MomentType::Reflectivity),
            Some(0)
        );
        assert_eq!(
            matching_cut_index(&previous, &arriving, &radar_core::MomentType::SpectrumWidth),
            None,
            "a moment nothing carries has no previous picture"
        );
    }

    #[test]
    fn without_an_elevation_number_the_fullest_sweep_at_the_angle_wins() {
        // `elevation_deg` is the FIRST radial's angle, read while the antenna
        // is still settling, so the fallback has to tolerate a few tenths. When
        // several cuts qualify, the finished one is the one worth showing.
        let previous = volume_of(vec![
            tilt(None, 0.50, 120, radar_core::MomentType::Reflectivity),
            tilt(None, 0.55, 720, radar_core::MomentType::Reflectivity),
            tilt(None, 2.40, 720, radar_core::MomentType::Reflectivity),
        ]);
        let arriving = tilt(None, 0.64, 30, radar_core::MomentType::Reflectivity);

        assert_eq!(
            matching_cut_index(&previous, &arriving, &radar_core::MomentType::Reflectivity),
            Some(1)
        );

        // Nothing within half a degree means nothing to underpaint with, which
        // the caller must read as "draw the arrived wedge alone" rather than as
        // licence to use the nearest tilt at any angle.
        let elsewhere = volume_of(vec![tilt(
            None,
            4.30,
            720,
            radar_core::MomentType::Reflectivity,
        )]);
        assert_eq!(
            matching_cut_index(&elsewhere, &arriving, &radar_core::MomentType::Reflectivity),
            None
        );
    }

    #[test]
    fn an_empty_cut_is_never_offered_as_the_previous_tilt() {
        // A cut that exists but holds no radials renders as nothing at all.
        // Offering it would replace the previous picture with a blank one,
        // which is the failure this whole path exists to prevent.
        let mut empty = tilt(Some(1), 0.53, 720, radar_core::MomentType::Reflectivity);
        empty.radials.clear();
        let previous = volume_of(vec![
            empty,
            tilt(Some(2), 0.53, 720, radar_core::MomentType::Reflectivity),
        ]);
        let arriving = tilt(Some(1), 0.53, 20, radar_core::MomentType::Reflectivity);

        assert_eq!(
            matching_cut_index(&previous, &arriving, &radar_core::MomentType::Reflectivity),
            Some(1)
        );
    }
}
