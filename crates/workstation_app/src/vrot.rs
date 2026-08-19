//! Rotational velocity: a two-click measurement of a velocity couplet.
//!
//! Vrot is the number a warning forecaster reaches for when deciding how hard
//! to word a tornado warning, and the number the damage-rating research is
//! built on. The measurement originates with
//!
//! > Smith, B. T., R. L. Thompson, A. R. Dean, and P. T. Marsh, 2015:
//! > "Diagnosing the Conditional Probability of Tornado Damage Rating Using
//! > Environmental and Radar Attributes." *Wea. Forecasting*, **30**, 914-932,
//! > DOI 10.1175/WAF-D-14-00122.1.
//!
//! and the form computed here is the one used by
//!
//! > Thompson, R. L., and coauthors, 2017: "Tornado Damage Rating
//! > Probabilities Derived from WSR-88D Data." *Wea. Forecasting*, **32**,
//! > 1509-1528, DOI 10.1175/WAF-D-17-0004.1,
//!
//! at the lowest elevation angle:
//!
//! ```text
//! Vrot = (V_max - V_min) / 2
//! ```
//!
//! This is deliberately not Smith et al. (2015)'s `(|V_in| + |V_out|) / 2`.
//! The two agree exactly for a couplet that straddles zero, which is the
//! textbook case and therefore the case that hides the difference. They do not
//! agree when a fast-translating storm carries both gates onto one side of
//! zero: +10 and +70 m/s is a 30 m/s couplet under Thompson's form and a
//! 40 m/s couplet under the magnitude form, and the magnitude form is measuring
//! the storm motion, not the rotation.
//!
//! Vrot is reported in **knots** in both papers, and this module reports knots
//! first for that reason. It also prints m/s, because the radar's own gates are
//! in m/s and an analyst comparing the readout against a probe should not have
//! to convert in their head.
//!
//! # What this module refuses to measure
//!
//! Both papers measured GR2Analyst's *dealiased* velocity. A folded gate is the
//! one failure mode with no visible symptom: it is a perfectly ordinary-looking
//! number that is wrong by a multiple of twice the Nyquist velocity, so a
//! 70 m/s couplet reads as 20 m/s and the warning is downgraded on the strength
//! of it. [`measure`] therefore refuses raw velocity outright rather than
//! attaching a caveat nobody reads.

use std::fmt;

use product_engine::units::METERS_PER_SECOND_TO_KNOTS;

use crate::probe::ProbeValue;

/// The largest couplet diameter either paper accepts: 5 nautical miles, which
/// is exactly 9.26 km (a nautical mile is exactly 1852 m).
///
/// Past this, two gates are not one couplet. Letting a click on the storm's
/// inflow and a click on its rear-flank downdraft become a "measurement" would
/// manufacture a violent-tornado Vrot out of ordinary storm-scale shear.
pub const MAX_COUPLET_SEPARATION_KM: f64 = 9.26;

/// One end of a couplet: a velocity gate an analyst clicked.
///
/// The world kilometres are the pane's own frame, which for a single-radar
/// display is radar-local east and north. Both samples must come from the same
/// frame or the separation is meaningless.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VrotSample {
    pub world_east_km: f64,
    pub world_north_km: f64,
    /// The gate's velocity in m/s, the engine unit. Never knots: the conversion
    /// belongs at the formatting boundary, and a sample stored in knots would
    /// produce a Vrot 1.94 times too large that still looks like a plausible
    /// tornado.
    pub velocity_mps: f32,
    pub row: usize,
    pub gate: usize,
    pub slant_range_m: f64,
    pub beam_height_arl_m: f64,
    pub cut_index: usize,
    pub elevation_deg: f32,
}

impl VrotSample {
    /// The sample a probe reading describes.
    ///
    /// The caller is responsible for having probed a velocity product: a
    /// [`ProbeValue`] carries its number but not its product, so a reflectivity
    /// gate would arrive here as a "velocity" of 52.5 m/s. Read
    /// `ProductComputation::source_moment` before calling this, and pass the
    /// same product's dealiased flag to [`measure`].
    pub fn from_probe(value: &ProbeValue) -> Self {
        Self {
            world_east_km: value.location.east_km,
            world_north_km: value.location.north_km,
            velocity_mps: value.engine_value,
            row: value.row,
            gate: value.gate,
            slant_range_m: value.slant_range_m,
            beam_height_arl_m: value.beam_height_arl_m,
            cut_index: value.cut_index,
            elevation_deg: value.elevation_deg,
        }
    }

    /// Whether this sample carries numbers a measurement can be built from.
    fn is_usable(&self) -> bool {
        self.velocity_mps.is_finite()
            && self.world_east_km.is_finite()
            && self.world_north_km.is_finite()
            && self.beam_height_arl_m.is_finite()
    }
}

/// A finished couplet measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct VrotMeasurement {
    pub first: VrotSample,
    pub second: VrotSample,
    /// `(V_max - V_min) / 2`, in m/s. Thompson et al. (2017).
    pub vrot_mps: f32,
    /// `V_max - V_min`, in m/s. Twice Vrot, kept because the gate-to-gate
    /// velocity difference is what an analyst reads off the screen.
    pub delta_v_mps: f32,
    /// Distance between the two gates on the display, in kilometres. This is
    /// the couplet diameter the 5 nautical mile limit applies to.
    pub separation_km: f64,
    /// The **higher** of the two beam heights, which is the height both papers
    /// associate with a couplet. Taking the mean would put a couplet measured
    /// across a large height difference lower than either paper's convention
    /// and shift it into a different damage-rating bin.
    pub couplet_height_arl_m: f64,
    /// Things an analyst should know that are not grounds to refuse the
    /// measurement. Empty for a textbook couplet.
    pub warnings: Vec<VrotWarning>,
}

impl VrotMeasurement {
    pub fn vrot_knots(&self) -> f64 {
        f64::from(self.vrot_mps) * METERS_PER_SECOND_TO_KNOTS
    }
}

/// Something worth telling the analyst about a measurement that still stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VrotWarning {
    /// Both samples have the same sign, so the couplet does not straddle zero.
    SameSign,
}

impl VrotWarning {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SameSign => {
                "both gates are on the same side of zero - a fast-translating storm; \
                 (V_max - V_min) / 2 still measures the rotation, but check that both \
                 clicks are on the couplet and not on storm motion"
            }
        }
    }
}

impl fmt::Display for VrotWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Why a pair of clicks is not a measurement.
///
/// Every variant carries text an analyst can act on: a refusal that only says
/// "invalid" trains people to click again until something appears.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VrotRefusal {
    RawVelocity,
    DifferentCuts,
    NoValidVelocity,
    SeparationTooLarge,
}

impl VrotRefusal {
    pub const fn label(self) -> &'static str {
        match self {
            Self::RawVelocity => {
                "Vrot needs dealiased velocity - switch this pane to the dealiased velocity \
                 product and measure again. A folded gate reads low by twice the Nyquist \
                 velocity and looks like an ordinary number, so the couplet would be \
                 understated with nothing on screen to show it."
            }
            Self::DifferentCuts => {
                "Both gates must come from the same cut - the tilt changed between the two \
                 clicks. Vrot is measured on one elevation, the lowest that samples the \
                 circulation."
            }
            Self::NoValidVelocity => {
                "One of the two gates has no velocity - click a gate that reports a number, \
                 not a range-folded or below-threshold gate."
            }
            Self::SeparationTooLarge => {
                "Those gates are more than 9.26 km (5 nautical miles) apart, which is wider \
                 than a couplet - click the adjacent inbound and outbound maxima instead."
            }
        }
    }
}

impl fmt::Display for VrotRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Why a measurement no longer describes what is on screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaleReason {
    /// The pane moved to another volume. The couplet has moved and probably
    /// changed strength; the number on screen is history.
    NewVolume,
    /// The pane changed tilt.
    DifferentCut,
    /// The pane changed product.
    DifferentProduct,
    /// The pane changed radar.
    DifferentSite,
}

impl StaleReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NewVolume => "measured on an earlier volume - measure again on this one",
            Self::DifferentCut => "measured on a different tilt",
            Self::DifferentProduct => "measured on a different product",
            Self::DifferentSite => "measured at a different radar",
        }
    }
}

impl fmt::Display for StaleReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Where a two-click measurement has got to.
#[derive(Clone, Debug, PartialEq)]
pub enum VrotState {
    Idle,
    AwaitingSecond(VrotSample),
    Complete(VrotMeasurement),
    /// A finished measurement kept on screen after the thing it measured went
    /// away. Kept rather than deleted so an analyst can still read the number
    /// they just took, and labelled so nobody reads it as current.
    Stale {
        measurement: VrotMeasurement,
        reason: StaleReason,
    },
}

impl VrotState {
    /// The measurement, current or stale.
    pub fn measurement(&self) -> Option<&VrotMeasurement> {
        match self {
            Self::Complete(measurement) | Self::Stale { measurement, .. } => Some(measurement),
            Self::Idle | Self::AwaitingSecond(_) => None,
        }
    }

    /// The first click of a measurement still waiting for its second.
    pub fn pending(&self) -> Option<&VrotSample> {
        match self {
            Self::AwaitingSecond(sample) => Some(sample),
            _ => None,
        }
    }

    pub fn stale_reason(&self) -> Option<StaleReason> {
        match self {
            Self::Stale { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// Mark what is on screen as no longer describing what is on screen.
    ///
    /// A half-finished measurement is discarded outright: its first click
    /// belongs to a frame that is gone, and completing it with a second click
    /// on the new frame would silently measure a couplet across two volumes.
    /// An already-stale measurement keeps the reason it first went stale, which
    /// is the earliest and therefore the most honest one.
    pub fn mark_stale(&mut self, reason: StaleReason) {
        match self {
            Self::Complete(measurement) => {
                *self = Self::Stale {
                    measurement: measurement.clone(),
                    reason,
                };
            }
            Self::AwaitingSecond(_) => *self = Self::Idle,
            Self::Idle | Self::Stale { .. } => {}
        }
    }

    pub fn clear(&mut self) {
        *self = Self::Idle;
    }
}

/// Measure a couplet from two clicked gates.
///
/// `product_is_dealiased` is the displayed product's own flag - see
/// `ProductComputation::uses_dealiased_velocity` - not a guess from the data.
///
/// Thompson, R. L., and coauthors, 2017, *Wea. Forecasting*, **32**, 1509-1528,
/// DOI 10.1175/WAF-D-17-0004.1:
///
/// ```text
/// Vrot = (V_max - V_min) / 2
/// ```
pub fn measure(
    first: VrotSample,
    second: VrotSample,
    product_is_dealiased: bool,
) -> Result<VrotMeasurement, VrotRefusal> {
    if !product_is_dealiased {
        return Err(VrotRefusal::RawVelocity);
    }
    if first.cut_index != second.cut_index {
        return Err(VrotRefusal::DifferentCuts);
    }
    if !first.is_usable() || !second.is_usable() {
        return Err(VrotRefusal::NoValidVelocity);
    }

    let separation_km = (first.world_east_km - second.world_east_km)
        .hypot(first.world_north_km - second.world_north_km);
    if separation_km > MAX_COUPLET_SEPARATION_KM {
        return Err(VrotRefusal::SeparationTooLarge);
    }

    // Thompson et al. (2017): the half difference of the two velocities, which
    // is signed-subtraction and not the mean of two magnitudes. Both samples
    // are finite by the check above, so `max` and `min` have no NaN case.
    let maximum_mps = first.velocity_mps.max(second.velocity_mps);
    let minimum_mps = first.velocity_mps.min(second.velocity_mps);
    let delta_v_mps = maximum_mps - minimum_mps;

    let mut warnings = Vec::new();
    if share_a_sign(first.velocity_mps, second.velocity_mps) {
        warnings.push(VrotWarning::SameSign);
    }

    Ok(VrotMeasurement {
        first,
        second,
        vrot_mps: delta_v_mps / 2.0,
        delta_v_mps,
        separation_km,
        couplet_height_arl_m: first.beam_height_arl_m.max(second.beam_height_arl_m),
        warnings,
    })
}

/// A one-line readout of a finished measurement.
///
/// Knots first because that is the unit both papers and every warning are
/// written in; m/s beside it because that is what the gates hold.
pub fn report(measurement: &VrotMeasurement) -> String {
    let mut text = format!(
        "Vrot {:.0} kt ({:.1} m/s) | delta-V {:.1} m/s | separation {:.2} km | height {:.2} km ARL | {:.1} deg cut {}",
        measurement.vrot_knots(),
        measurement.vrot_mps,
        measurement.delta_v_mps,
        measurement.separation_km,
        measurement.couplet_height_arl_m / 1000.0,
        measurement.first.elevation_deg,
        measurement.first.cut_index,
    );
    for warning in &measurement.warnings {
        text.push_str(" | WARNING: ");
        text.push_str(warning.label());
    }
    text
}

/// Whether both velocities are on the same side of zero.
///
/// A gate of exactly zero is on neither side, so it does not raise the warning:
/// a couplet with one gate at zero is an ordinary weak couplet, not a
/// translating one.
fn share_a_sign(left_mps: f32, right_mps: f32) -> bool {
    (left_mps > 0.0 && right_mps > 0.0) || (left_mps < 0.0 && right_mps < 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gate 1 km east-north-east of the origin, so that a pair built with
    /// [`pair`] is exactly 1.00 km apart.
    fn sample(velocity_mps: f32, east_km: f64, north_km: f64) -> VrotSample {
        VrotSample {
            world_east_km: east_km,
            world_north_km: north_km,
            velocity_mps,
            row: 247,
            gate: 164,
            slant_range_m: 43_125.0,
            beam_height_arl_m: 485.8,
            cut_index: 0,
            elevation_deg: 0.5,
        }
    }

    /// Two gates exactly 1.00 km apart: `hypot(0.8, 0.6)`.
    fn pair(first_mps: f32, second_mps: f32) -> (VrotSample, VrotSample) {
        (
            sample(first_mps, 10.0, 20.0),
            sample(second_mps, 10.8, 20.6),
        )
    }

    fn measured(first_mps: f32, second_mps: f32) -> VrotMeasurement {
        let (first, second) = pair(first_mps, second_mps);
        measure(first, second, true).expect("a 1 km couplet on one dealiased cut must measure")
    }

    /// 30 m/s is 58.3 kt: 30 * 3600 / 1852. The knot conversion is exact by
    /// definition, so this pins both the formula and the unit.
    #[test]
    fn a_symmetric_couplet_of_plus_and_minus_thirty_measures_thirty_metres_per_second() {
        let measurement = measured(-30.0, 30.0);
        assert_eq!(measurement.vrot_mps, 30.0);
        assert_eq!(measurement.delta_v_mps, 60.0);
        assert!(
            (measurement.vrot_knots() - 58.315_334_8).abs() < 1e-6,
            "Vrot in knots was {}",
            measurement.vrot_knots()
        );
        assert!(
            measurement.warnings.is_empty(),
            "a couplet straddling zero has nothing to warn about"
        );
    }

    /// An asymmetric couplet that still straddles zero. Thompson's difference
    /// form gives (40 - (-20)) / 2 = 30; Smith et al. (2015)'s magnitude form
    /// gives (20 + 40) / 2 = 30 as well. The two only diverge once both gates
    /// share a sign, which the next test covers - this one pins that an
    /// asymmetric couplet is not accidentally averaged or maximised.
    #[test]
    fn an_asymmetric_couplet_measures_half_the_difference_not_the_larger_gate() {
        let measurement = measured(-20.0, 40.0);
        assert_eq!(measurement.vrot_mps, 30.0);
        assert_eq!(measurement.delta_v_mps, 60.0);
        assert!(measurement.warnings.is_empty());
    }

    /// The case that separates the two published forms. A storm translating at
    /// 40 m/s carries both gates onto the inbound side: Thompson's
    /// (70 - 10) / 2 = 30 measures the rotation, while the magnitude form
    /// (|10| + |70|) / 2 = 40 measures a third of the storm motion as well and
    /// would push this couplet a damage-rating bin higher.
    #[test]
    fn a_same_sign_pair_still_measures_and_carries_a_warning() {
        let measurement = measured(10.0, 70.0);
        assert_eq!(measurement.vrot_mps, 30.0);
        assert_ne!(
            measurement.vrot_mps, 40.0,
            "40 m/s is the Smith et al. (2015) magnitude form, which this module does not use"
        );
        assert_eq!(measurement.warnings, vec![VrotWarning::SameSign]);
        assert!(
            report(&measurement).contains("WARNING"),
            "a same-sign couplet must say so in the readout"
        );
    }

    #[test]
    fn a_gate_at_exactly_zero_is_not_a_same_sign_pair() {
        let measurement = measured(0.0, 60.0);
        assert_eq!(measurement.vrot_mps, 30.0);
        assert!(measurement.warnings.is_empty());
    }

    /// The refusal that matters most: a folded gate produces a number that
    /// looks entirely reasonable and is wrong by twice the Nyquist velocity.
    #[test]
    fn raw_velocity_is_refused_because_a_folded_gate_reads_as_a_plausible_number() {
        let (first, second) = pair(-30.0, 30.0);
        assert_eq!(measure(first, second, false), Err(VrotRefusal::RawVelocity));
        assert!(
            VrotRefusal::RawVelocity.label().contains("dealiased"),
            "the refusal must tell the analyst what to switch to"
        );
    }

    #[test]
    fn two_gates_from_different_cuts_are_refused() {
        let (first, mut second) = pair(-30.0, 30.0);
        second.cut_index = 1;
        second.elevation_deg = 1.5;
        assert_eq!(
            measure(first, second, true),
            Err(VrotRefusal::DifferentCuts)
        );
    }

    #[test]
    fn a_gate_without_a_velocity_is_refused() {
        let (first, mut second) = pair(-30.0, 30.0);
        second.velocity_mps = f32::NAN;
        assert_eq!(
            measure(first, second, true),
            Err(VrotRefusal::NoValidVelocity)
        );

        let (mut first, second) = pair(-30.0, 30.0);
        first.velocity_mps = f32::INFINITY;
        assert_eq!(
            measure(first, second, true),
            Err(VrotRefusal::NoValidVelocity)
        );
    }

    /// 12 km apart is not one couplet; it is two different parts of a storm.
    #[test]
    fn gates_twelve_kilometres_apart_are_refused_as_a_couplet() {
        let first = sample(-30.0, 0.0, 0.0);
        let second = sample(30.0, 0.0, 12.0);
        assert_eq!(
            measure(first, second, true),
            Err(VrotRefusal::SeparationTooLarge)
        );
    }

    /// 5 nautical miles is exactly 9.26 km, and the limit is inclusive: a
    /// couplet measured at exactly the published maximum is a couplet.
    #[test]
    fn the_couplet_limit_is_five_nautical_miles_and_includes_its_endpoint() {
        let first = sample(-30.0, 0.0, 0.0);
        let at_limit = sample(30.0, 0.0, MAX_COUPLET_SEPARATION_KM);
        let measurement =
            measure(first, at_limit, true).expect("9.26 km is the largest couplet allowed");
        assert!((measurement.separation_km - 9.26).abs() < 1e-9);

        let past_limit = sample(30.0, 0.0, MAX_COUPLET_SEPARATION_KM + 0.04);
        assert_eq!(
            measure(first, past_limit, true),
            Err(VrotRefusal::SeparationTooLarge)
        );
    }

    /// Both papers associate a couplet with the height of the beam it was
    /// measured in; taking the mean of two beam heights would put the couplet
    /// lower than either paper's convention.
    #[test]
    fn the_couplet_height_is_the_higher_of_the_two_beams() {
        let (mut first, mut second) = pair(-30.0, 30.0);
        first.beam_height_arl_m = 500.0;
        second.beam_height_arl_m = 900.0;
        let measurement = measure(first, second, true).expect("a 1 km couplet must measure");
        assert_eq!(measurement.couplet_height_arl_m, 900.0);

        // And the order of the clicks does not change it.
        let measurement = measure(second, first, true).expect("a 1 km couplet must measure");
        assert_eq!(measurement.couplet_height_arl_m, 900.0);
    }

    #[test]
    fn the_report_gives_knots_and_metres_per_second_and_the_geometry() {
        let (mut first, mut second) = pair(-30.0, 30.0);
        first.beam_height_arl_m = 500.0;
        second.beam_height_arl_m = 620.0;
        let measurement = measure(first, second, true).expect("a 1 km couplet must measure");
        assert_eq!(
            report(&measurement),
            "Vrot 58 kt (30.0 m/s) | delta-V 60.0 m/s | separation 1.00 km | height 0.62 km ARL | 0.5 deg cut 0"
        );
    }

    #[test]
    fn a_probe_reading_becomes_a_sample_without_losing_a_field() {
        use crate::probe::{ProbeLocation, ProbeValue};
        use product_engine::stats::CellState;

        let value = ProbeValue {
            engine_value: -31.5,
            state: CellState::Valid,
            location: ProbeLocation {
                east_km: -39.813_440_6,
                north_km: -16.572_735_8,
                azimuth_deg: 247.4,
                screen_range_km: 43.125,
            },
            row: 247,
            gate: 164,
            slant_range_m: 43_125.0,
            beam_height_arl_m: 485.784_6,
            beam_height_msl_m: Some(855.784_6),
            elevation_deg: 0.5,
            cut_index: 3,
        };
        let sample = VrotSample::from_probe(&value);
        assert_eq!(sample.world_east_km, value.location.east_km);
        assert_eq!(sample.world_north_km, value.location.north_km);
        assert_eq!(sample.velocity_mps, -31.5);
        assert_eq!(sample.row, 247);
        assert_eq!(sample.gate, 164);
        assert_eq!(sample.slant_range_m, 43_125.0);
        assert_eq!(sample.beam_height_arl_m, 485.784_6);
        assert_eq!(sample.cut_index, 3);
        assert_eq!(sample.elevation_deg, 0.5);
    }

    #[test]
    fn a_completed_measurement_survives_a_frame_change_but_is_labelled_stale() {
        let mut state = VrotState::Complete(measured(-30.0, 30.0));
        state.mark_stale(StaleReason::NewVolume);
        assert_eq!(state.stale_reason(), Some(StaleReason::NewVolume));
        assert_eq!(
            state
                .measurement()
                .expect("a stale measurement is still readable")
                .vrot_mps,
            30.0
        );

        // The first reason stands: it is the moment the number stopped being
        // current, and a later one would understate how old it is.
        state.mark_stale(StaleReason::DifferentCut);
        assert_eq!(state.stale_reason(), Some(StaleReason::NewVolume));
    }

    /// Completing a half-finished measurement after the frame changed would
    /// measure one gate on one volume against another gate on the next.
    #[test]
    fn a_half_finished_measurement_is_discarded_when_the_frame_changes() {
        let (first, _) = pair(-30.0, 30.0);
        let mut state = VrotState::AwaitingSecond(first);
        state.mark_stale(StaleReason::NewVolume);
        assert_eq!(state, VrotState::Idle);
        assert_eq!(state.pending(), None);
        assert_eq!(state.measurement(), None);
    }

    #[test]
    fn every_refusal_and_stale_reason_says_something_an_analyst_can_act_on() {
        for refusal in [
            VrotRefusal::RawVelocity,
            VrotRefusal::DifferentCuts,
            VrotRefusal::NoValidVelocity,
            VrotRefusal::SeparationTooLarge,
        ] {
            let message = refusal.label();
            assert!(message.len() > 40, "{refusal:?} says only {message:?}");
            assert_eq!(message, refusal.to_string());
        }
        assert!(!VrotWarning::SameSign.label().is_empty());
        for reason in [
            StaleReason::NewVolume,
            StaleReason::DifferentCut,
            StaleReason::DifferentProduct,
            StaleReason::DifferentSite,
        ] {
            assert!(!reason.label().is_empty());
            assert_eq!(reason.label(), reason.to_string());
        }
    }
}
