//! What a single cell of a field means, and what a whole field looks like.
//!
//! The distinction this module exists to protect is between *nothing was
//! there* and *we did not look*. A radar that swept a location and found no
//! echo has told you something; a location no beam reached has told you
//! nothing. Both are blank on screen, and collapsing them into one "no value"
//! makes an echo-top field of zeros indistinguishable from an echo-top field of
//! silence. [`CellState`] keeps them apart all the way to the readout.
//!
//! Statistics are computed once, on a worker, in the same pass as the
//! plausibility check. The update thread never scans a field: at one kilometre
//! spacing over a 460 km radius that is 850 000 cells, and doing it while
//! painting would drop frames for a number nobody asked for.

use crate::domain::{PlausibilityDisposition, PlausibleRange};

/// What one cell of a derived or sampled field is.
///
/// Only [`CellState::Valid`], [`CellState::LowerBound`], and
/// [`CellState::UpperBound`] carry a number worth reading. Everything else has
/// an undefined value slot, and reading it is a bug.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CellState {
    /// A number, known exactly as far as the algorithm can tell.
    Valid,
    /// The true value is at least the stored value. An echo top that was still
    /// above the highest beam is this: the storm is *at least* that tall.
    LowerBound,
    /// The true value is at most the stored value.
    UpperBound,
    /// A beam sampled here and found no meteorological return.
    NoEcho,
    /// A beam sampled here but the data are unusable or absent.
    NoData,
    /// No beam sampled here at all. Beyond the sweep, below the lowest tilt, or
    /// outside the scanned sector.
    NoCoverage,
    /// The radar reported this gate as range folded, so its true range is
    /// ambiguous and its value belongs somewhere else.
    RangeFolded,
    /// The algorithm produced a number and then discarded it, because the
    /// geometry or support here makes it untrustworthy. Azimuthal shear inside
    /// 20 km is the case that motivates this.
    QualityMasked,
    /// The product needs a thermal environment and none is available.
    EnvironmentUnavailable,
}

impl CellState {
    /// Whether the value slot beside this state may be read.
    pub const fn has_value(self) -> bool {
        matches!(self, Self::Valid | Self::LowerBound | Self::UpperBound)
    }

    /// Text for a probe readout. Written so an analyst can tell at a glance
    /// whether the radar looked and found nothing, or never looked.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Valid => "",
            Self::LowerBound => "AT LEAST",
            Self::UpperBound => "AT MOST",
            Self::NoEcho => "NO ECHO - BEAM SAMPLED THIS LOCATION",
            Self::NoData => "NO DATA - SAMPLED BUT UNUSABLE",
            Self::NoCoverage => "OUTSIDE SWEEP - RADAR DID NOT SAMPLE THIS LOCATION",
            Self::RangeFolded => "RANGE FOLDED",
            Self::QualityMasked => "QUALITY MASKED",
            Self::EnvironmentUnavailable => "NO THERMAL ENVIRONMENT",
        }
    }
}

/// A census of one field, computed in a single pass.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FieldStats {
    pub cells_total: usize,
    pub cells_valid: usize,
    pub cells_lower_bound: usize,
    pub cells_upper_bound: usize,
    pub cells_no_echo: usize,
    pub cells_no_data: usize,
    pub cells_no_coverage: usize,
    pub cells_range_folded: usize,
    pub cells_quality_masked: usize,
    pub cells_environment_unavailable: usize,
    /// Cells whose state says they hold a number, but whose number is NaN or
    /// infinite. Always a defect; any nonzero count rejects the field.
    pub non_finite_valued: usize,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub mean: Option<f64>,
}

impl FieldStats {
    /// Cells that carry a readable number.
    pub const fn cells_with_values(&self) -> usize {
        self.cells_valid + self.cells_lower_bound + self.cells_upper_bound
    }

    /// Fraction of the field that carries a number, 0 to 1.
    pub fn valued_fraction(&self) -> f32 {
        if self.cells_total == 0 {
            return 0.0;
        }
        self.cells_with_values() as f32 / self.cells_total as f32
    }

    /// Whether the radar looked anywhere at all. A field that is entirely
    /// `NoCoverage` is not an algorithm failure and must not be reported as
    /// one; it is a volume that does not reach this grid.
    pub const fn has_any_coverage(&self) -> bool {
        self.cells_total > self.cells_no_coverage
    }
}

/// One reason a field failed its plausibility check.
#[derive(Clone, Debug, PartialEq)]
pub struct PlausibilityViolation {
    pub detail: String,
    pub disposition: PlausibilityDisposition,
}

/// The verdict on a whole field.
#[derive(Clone, Debug, PartialEq)]
pub struct PlausibilityReport {
    pub disposition: PlausibilityDisposition,
    pub violations: Vec<PlausibilityViolation>,
}

impl PlausibilityReport {
    pub fn passed() -> Self {
        Self {
            disposition: PlausibilityDisposition::Pass,
            violations: Vec::new(),
        }
    }

    pub fn is_rejected(&self) -> bool {
        self.disposition == PlausibilityDisposition::Reject
    }

    /// A short line for a badge or a log.
    pub fn summary(&self) -> String {
        match self.disposition {
            PlausibilityDisposition::Pass => "plausible".to_owned(),
            PlausibilityDisposition::Warn | PlausibilityDisposition::Reject => self
                .violations
                .first()
                .map(|violation| violation.detail.clone())
                .unwrap_or_else(|| "implausible".to_owned()),
        }
    }
}

/// Count a field and judge it in one pass.
///
/// `values` and `states` must be the same length; a mismatch is itself a defect
/// and rejects the field rather than silently truncating to the shorter one,
/// because a truncated field draws as a correct picture of the wrong area.
pub fn summarize(
    values: &[f32],
    states: &[CellState],
    plausible: PlausibleRange,
) -> (FieldStats, PlausibilityReport) {
    let mut stats = FieldStats {
        cells_total: states.len(),
        ..FieldStats::default()
    };
    let mut violations = Vec::new();

    if values.len() != states.len() {
        violations.push(PlausibilityViolation {
            detail: format!(
                "field has {} values but {} states",
                values.len(),
                states.len()
            ),
            disposition: PlausibilityDisposition::Reject,
        });
        return (
            stats,
            PlausibilityReport {
                disposition: PlausibilityDisposition::Reject,
                violations,
            },
        );
    }

    let mut sum = 0.0_f64;
    let mut worst = PlausibilityDisposition::Pass;
    let mut out_of_hard_bounds = 0_usize;
    let mut out_of_soft_bounds = 0_usize;

    for (value, state) in values.iter().zip(states) {
        match state {
            CellState::Valid => stats.cells_valid += 1,
            CellState::LowerBound => stats.cells_lower_bound += 1,
            CellState::UpperBound => stats.cells_upper_bound += 1,
            CellState::NoEcho => stats.cells_no_echo += 1,
            CellState::NoData => stats.cells_no_data += 1,
            CellState::NoCoverage => stats.cells_no_coverage += 1,
            CellState::RangeFolded => stats.cells_range_folded += 1,
            CellState::QualityMasked => stats.cells_quality_masked += 1,
            CellState::EnvironmentUnavailable => stats.cells_environment_unavailable += 1,
        }

        if !state.has_value() {
            continue;
        }
        if !value.is_finite() {
            stats.non_finite_valued += 1;
            worst = PlausibilityDisposition::Reject;
            continue;
        }

        stats.min = Some(stats.min.map_or(*value, |current| current.min(*value)));
        stats.max = Some(stats.max.map_or(*value, |current| current.max(*value)));
        sum += f64::from(*value);

        match plausible.classify(*value) {
            PlausibilityDisposition::Pass => {}
            PlausibilityDisposition::Warn => {
                out_of_soft_bounds += 1;
                if worst == PlausibilityDisposition::Pass {
                    worst = PlausibilityDisposition::Warn;
                }
            }
            PlausibilityDisposition::Reject => {
                out_of_hard_bounds += 1;
                worst = PlausibilityDisposition::Reject;
            }
        }
    }

    let valued = stats.cells_with_values();
    if valued > 0 {
        stats.mean = Some(sum / valued as f64);
    }

    if stats.non_finite_valued > 0 {
        violations.push(PlausibilityViolation {
            detail: format!(
                "{} cells claim a value but hold NaN or infinity",
                stats.non_finite_valued
            ),
            disposition: PlausibilityDisposition::Reject,
        });
    }
    if out_of_hard_bounds > 0 {
        violations.push(PlausibilityViolation {
            detail: format!(
                "{out_of_hard_bounds} cells outside the hard range {}..{} (min {:?}, max {:?})",
                plausible.hard_min, plausible.hard_max, stats.min, stats.max
            ),
            disposition: PlausibilityDisposition::Reject,
        });
    }
    if out_of_soft_bounds > 0 {
        violations.push(PlausibilityViolation {
            detail: format!(
                "{out_of_soft_bounds} cells outside the usual range {}..{}",
                plausible.soft_min, plausible.soft_max
            ),
            disposition: PlausibilityDisposition::Warn,
        });
    }

    (
        stats,
        PlausibilityReport {
            disposition: worst,
            violations,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFLECTIVITY: PlausibleRange = PlausibleRange::new(-35.0, 85.0, -40.0, 100.0);

    #[test]
    fn a_field_of_ordinary_reflectivity_passes_and_is_counted() {
        let values = [10.0, 30.0, 50.0, 0.0];
        let states = [
            CellState::Valid,
            CellState::Valid,
            CellState::Valid,
            CellState::NoCoverage,
        ];
        let (stats, report) = summarize(&values, &states, REFLECTIVITY);
        assert_eq!(stats.cells_total, 4);
        assert_eq!(stats.cells_valid, 3);
        assert_eq!(stats.cells_no_coverage, 1);
        assert_eq!(stats.min, Some(10.0));
        assert_eq!(stats.max, Some(50.0));
        assert_eq!(stats.mean, Some(30.0));
        assert_eq!(report.disposition, PlausibilityDisposition::Pass);
    }

    #[test]
    fn the_value_beside_a_no_coverage_cell_is_never_read() {
        // A cell with no coverage holds whatever the buffer was initialised
        // with. If statistics read it, one uninitialised cell moves the mean.
        let values = [50.0, -9999.0];
        let states = [CellState::Valid, CellState::NoCoverage];
        let (stats, report) = summarize(&values, &states, REFLECTIVITY);
        assert_eq!(stats.min, Some(50.0));
        assert_eq!(stats.max, Some(50.0));
        assert_eq!(stats.mean, Some(50.0));
        assert_eq!(
            report.disposition,
            PlausibilityDisposition::Pass,
            "-9999 beside a no-coverage state is not a plausibility failure"
        );
    }

    #[test]
    fn a_nan_in_a_cell_that_claims_a_value_rejects_the_field() {
        let values = [10.0, f32::NAN];
        let states = [CellState::Valid, CellState::Valid];
        let (stats, report) = summarize(&values, &states, REFLECTIVITY);
        assert_eq!(stats.non_finite_valued, 1);
        assert!(report.is_rejected());
    }

    #[test]
    fn a_value_outside_the_hard_range_rejects_the_field() {
        // 400 dBZ is not weather; it is a decode or unit fault.
        let values = [400.0];
        let states = [CellState::Valid];
        let (_, report) = summarize(&values, &states, REFLECTIVITY);
        assert!(report.is_rejected());
        assert!(report.summary().contains("hard range"));
    }

    #[test]
    fn a_value_outside_the_soft_range_only_warns() {
        // 90 dBZ is extraordinary but a radar can report it, so it must still
        // be drawable. Rejecting it would blank the pane on the biggest hail
        // core of the day.
        let values = [90.0];
        let states = [CellState::Valid];
        let (_, report) = summarize(&values, &states, REFLECTIVITY);
        assert_eq!(report.disposition, PlausibilityDisposition::Warn);
        assert!(!report.is_rejected());
    }

    #[test]
    fn a_length_mismatch_rejects_rather_than_truncating() {
        let values = [10.0, 20.0, 30.0];
        let states = [CellState::Valid, CellState::Valid];
        let (_, report) = summarize(&values, &states, REFLECTIVITY);
        assert!(report.is_rejected());
        assert!(report.summary().contains("3 values but 2 states"));
    }

    #[test]
    fn an_entirely_unsampled_field_is_not_an_algorithm_failure() {
        let values = [0.0; 4];
        let states = [CellState::NoCoverage; 4];
        let (stats, report) = summarize(&values, &states, REFLECTIVITY);
        assert!(!stats.has_any_coverage());
        assert_eq!(stats.mean, None);
        assert_eq!(
            report.disposition,
            PlausibilityDisposition::Pass,
            "a grid the volume does not reach is empty, not broken"
        );
    }

    #[test]
    fn a_sampled_field_that_found_nothing_still_counts_as_coverage() {
        // The difference that matters: the beam looked here and found no echo.
        let values = [0.0; 4];
        let states = [CellState::NoEcho; 4];
        let (stats, _) = summarize(&values, &states, REFLECTIVITY);
        assert!(stats.has_any_coverage());
        assert_eq!(stats.cells_no_echo, 4);
        assert_eq!(stats.cells_with_values(), 0);
    }

    #[test]
    fn bounded_cells_count_as_values_and_enter_the_statistics() {
        // A topped echo still has a height worth colouring and averaging; it is
        // only its interpretation that is one-sided.
        let values = [12_000.0, 8_000.0];
        let states = [CellState::LowerBound, CellState::Valid];
        let (stats, _) = summarize(
            &values,
            &states,
            PlausibleRange::new(0.0, 22_000.0, 0.0, 30_000.0),
        );
        assert_eq!(stats.cells_with_values(), 2);
        assert_eq!(stats.cells_lower_bound, 1);
        assert_eq!(stats.max, Some(12_000.0));
        assert_eq!(stats.mean, Some(10_000.0));
    }

    #[test]
    fn only_states_that_carry_numbers_may_be_read() {
        assert!(CellState::Valid.has_value());
        assert!(CellState::LowerBound.has_value());
        assert!(CellState::UpperBound.has_value());
        for state in [
            CellState::NoEcho,
            CellState::NoData,
            CellState::NoCoverage,
            CellState::RangeFolded,
            CellState::QualityMasked,
            CellState::EnvironmentUnavailable,
        ] {
            assert!(!state.has_value(), "{state:?} must not be read as a number");
        }
    }

    #[test]
    fn no_echo_and_no_coverage_read_differently_to_an_analyst() {
        // These two are blank in the same way on screen, so the words are the
        // only thing that tells them apart.
        assert!(CellState::NoEcho.label().contains("BEAM SAMPLED"));
        assert!(CellState::NoCoverage.label().contains("DID NOT SAMPLE"));
        assert_ne!(CellState::NoEcho.label(), CellState::NoCoverage.label());
    }

    #[test]
    fn an_empty_field_reports_nothing_rather_than_dividing_by_zero() {
        let (stats, report) = summarize(&[], &[], REFLECTIVITY);
        assert_eq!(stats.cells_total, 0);
        assert_eq!(stats.mean, None);
        assert_eq!(stats.valued_fraction(), 0.0);
        assert_eq!(report.disposition, PlausibilityDisposition::Pass);
    }
}
