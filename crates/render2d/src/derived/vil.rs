//! Vertically integrated liquid, and VIL density.
//!
//! VIL is the mass of liquid water in a column, in kg m^-2, inferred from
//! reflectivity by assuming a Marshall-Palmer drop size distribution. It is the
//! oldest thing in this module and still one of the most used, because a storm
//! that is holding a lot of water aloft is a storm that can drop it.
//!
//! Greene, D. R., and R. A. Clark, 1972: "Vertically Integrated Liquid Water -
//! A New Analysis Tool." *Monthly Weather Review*, **100**, 548-552.
//! DOI 10.1175/1520-0493(1972)100<0548:VILWNA>2.3.CO;2
//!
//! Their eq. (8) is the continuous form:
//!
//! ```text
//! M* = 3.44e-6 * integral of Z^(4/7) dh'
//! ```
//!
//! with `h'` "the height expressed in meters", `Z` the linear reflectivity
//! factor in mm^6 m^-3, and `M*` in kg m^-2. The 3.44e-6 follows from their
//! eq. (6) with a Marshall-Palmer intercept of 8e6 m^-4.
//!
//! The discretised layer sum used here is **not** in Greene and Clark. It comes
//! from:
//!
//! Amburn, S. A., and P. L. Wolf, 1997: "VIL Density as a Hail Indicator."
//! *Weather and Forecasting*, **12**, 473-478.
//! DOI 10.1175/1520-0434(1997)012<0473:VDAAHI>2.0.CO;2
//!
//! whose eq. (1) averages the linear reflectivity across each layer and
//! multiplies by the layer depth in metres. Attributing the discrete form to
//! Greene and Clark would be citing a paper that does not contain it.
//!
//! # The 56 dBZ cap is not in either paper
//!
//! Capping reflectivity before integrating is a later operational convention,
//! documented in NWS/WDTD training material rather than in the literature.
//! Greene and Clark explicitly decline to remove hail inflation: "Hail may also
//! produce fictitious values of liquid water... However, this may be beneficial
//! as an indicator of the severity of a storm." So the cap is a policy of this
//! application, it is named and switchable, and it is applied where MRMS
//! applies it - to the layer-averaged term, not to each reflectivity before
//! averaging. Those two are not the same: for a layer spanning 60 and 40 dBZ,
//! capping each first gives a mean of 204 053 mm^6 m^-3 and capping the mean
//! gives 398 107.

use product_engine::CellState;

use super::profile::{ColumnSample, linear_z_from_dbz};

/// The reflectivity above which liquid water is not believed.
///
/// An operational convention, not a result: see the module documentation. Above
/// about 56 dBZ a return is far more likely to be hail than rain, and the
/// Marshall-Palmer relation behind VIL says nothing about hail.
pub const VIL_REFLECTIVITY_CAP_DBZ: f32 = 56.0;

/// Greene and Clark (1972) eq. (8). Pairs with a depth in **metres**.
pub const VIL_COEFFICIENT: f32 = 3.44e-6;

/// The exponent of eq. (8), from the Marshall-Palmer integration.
pub const VIL_EXPONENT: f32 = 4.0 / 7.0;

/// Vertically integrated liquid over one column, in kg m^-2.
///
/// Integration runs over **layers between adjacent covered samples**, which is
/// Amburn and Wolf's discretisation and differs from the slab sum the hail
/// index uses - the two papers discretise differently and each is implemented
/// as its own author wrote it.
///
/// An uncovered sample breaks the chain. The profile across a layer the radar
/// did not sample is unknown, and a storm's liquid water is exactly the
/// quantity one must not invent: bridging the cone of silence under a supercell
/// would add the deepest, wettest layer in the column.
///
/// The returned state:
///
/// - `NoCoverage` when no beam reached this column at all.
/// - `NoEcho` when beams reached it and found nothing. Deliberately not a
///   numeric zero: "the radar looked and there is no water here" and "there is
///   0.0 kg m^-2 of water here" read the same on a colour bar but not in a
///   probe, and only one of them is a measurement.
/// - `LowerBound` when the column was truncated - only one covered sample, or
///   the topmost covered sample still carried an echo, so there is water above
///   the data that is not in the total.
/// - `Valid` otherwise.
pub fn vertically_integrated_liquid(column: &[ColumnSample]) -> (f32, CellState) {
    let mut total = 0.0_f32;
    let mut any_covered = false;
    let mut any_echo = false;
    let mut layers = 0_usize;
    let mut previous: Option<&ColumnSample> = None;

    for sample in column {
        if !sample.is_covered() {
            // Forgetting the running neighbour is the whole mechanism that
            // stops the integration bridging an unsampled layer.
            previous = None;
            continue;
        }
        any_covered = true;
        if sample.value().is_some_and(|dbz| dbz > 0.0) {
            any_echo = true;
        }
        if let Some(lower) = previous {
            total += layer_liquid_kg_m2(lower, sample);
            layers += 1;
        }
        previous = Some(sample);
    }

    if !any_covered {
        return (0.0, CellState::NoCoverage);
    }
    if !any_echo {
        return (0.0, CellState::NoEcho);
    }
    if layers == 0 {
        // Echo was seen but no two covered samples were adjacent, so there is
        // no layer to integrate. Zero here is a floor, not a measurement.
        return (0.0, CellState::LowerBound);
    }

    let truncated = column
        .iter()
        .rev()
        .find(|sample| sample.is_covered())
        .and_then(ColumnSample::value)
        .is_some_and(|dbz| dbz > 0.0);
    if truncated {
        (total, CellState::LowerBound)
    } else {
        (total, CellState::Valid)
    }
}

/// One layer of Amburn and Wolf (1997) eq. (1), in kg m^-2.
///
/// A covered sample with no readable value contributes zero linear
/// reflectivity, which is the truth for a no-echo gate and an understatement
/// for an unusable one.
fn layer_liquid_kg_m2(lower: &ColumnSample, upper: &ColumnSample) -> f32 {
    let depth_m = upper.height_arl_m - lower.height_arl_m;
    if !depth_m.is_finite() || depth_m <= 0.0 {
        return 0.0;
    }
    let mean_linear_z = 0.5 * (linear_z_of(lower) + linear_z_of(upper));
    let capped = mean_linear_z.min(linear_z_from_dbz(VIL_REFLECTIVITY_CAP_DBZ));
    if capped <= 0.0 {
        return 0.0;
    }
    VIL_COEFFICIENT * capped.powf(VIL_EXPONENT) * depth_m
}

fn linear_z_of(sample: &ColumnSample) -> f32 {
    match sample.value() {
        Some(dbz) => linear_z_from_dbz(dbz),
        None => 0.0,
    }
}

/// VIL density: liquid water per metre of storm depth, in kg m^-3.
///
/// Amburn, S. A., and P. L. Wolf, 1997, as above. Dividing VIL by echo-top
/// height separates a tall wet storm from a short wet one, and the short one is
/// the hail producer. Their hail-context values are guidance in a paper, not
/// thresholds in this code.
///
/// Stored in kg m^-3 and read in g m^-3, a factor of a thousand. The engine
/// value is around 0.004; the number a forecaster quotes is 4.
///
/// State propagation is the part that is easy to get backwards. When the echo
/// top is a **lower** bound the true denominator is larger, so the true density
/// is **smaller** than the value computed - the result is an `UpperBound`.
pub fn vil_density_kg_m3(
    vil_kg_m2: f32,
    vil_state: CellState,
    echo_top_arl_m: f32,
    echo_top_state: CellState,
) -> (f32, CellState) {
    if !vil_state.has_value() && vil_state != CellState::NoEcho {
        return (0.0, vil_state);
    }
    if !echo_top_state.has_value() {
        return (0.0, echo_top_state);
    }
    if !echo_top_arl_m.is_finite() || echo_top_arl_m <= 0.0 {
        // A storm with no depth has no density. Dividing would give infinity,
        // which would sail through every finite-value check downstream.
        return (0.0, CellState::NoData);
    }
    if vil_state == CellState::NoEcho {
        return (0.0, CellState::NoEcho);
    }

    let density = vil_kg_m2 / echo_top_arl_m;
    let state = match (vil_state, echo_top_state) {
        // A larger true top makes the true density smaller.
        (_, CellState::LowerBound) => CellState::UpperBound,
        (CellState::LowerBound, _) => CellState::LowerBound,
        (CellState::UpperBound, _) => CellState::UpperBound,
        _ => CellState::Valid,
    };
    (density, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(height_arl_m: f32, dbz: f32) -> ColumnSample {
        ColumnSample {
            cut_index: 0,
            elevation_deg: 0.5,
            height_arl_m,
            slant_range_m: 50_000.0,
            reflectivity_dbz: dbz,
            state: CellState::Valid,
        }
    }

    fn sample_with_state(height_arl_m: f32, state: CellState) -> ColumnSample {
        ColumnSample {
            cut_index: 0,
            elevation_deg: 0.5,
            height_arl_m,
            slant_range_m: 50_000.0,
            reflectivity_dbz: 0.0,
            state,
        }
    }

    #[test]
    fn a_uniform_layer_integrates_to_the_hand_computed_liquid_water() {
        // Two samples at 3000 and 6000 m, both 50 dBZ.
        //
        //   Z = 10^(50/10) = 100 000 mm^6 m^-3, and the layer mean is the same
        //   100 000^(4/7) = 10^(5 * 4/7) = 10^2.857142857 = 719.6857
        //   3.44e-6 * 719.6857 * 3000 = 7.42716 kg m^-2
        let column = [sample(3_000.0, 50.0), sample(6_000.0, 50.0)];
        let (vil, state) = vertically_integrated_liquid(&column);
        assert!(
            (vil - 7.427_16).abs() < 1e-3,
            "VIL was {vil}, expected 7.4272"
        );
        assert_eq!(
            state,
            CellState::LowerBound,
            "the top beam still echoes, so there is water above the data"
        );
    }

    #[test]
    fn integration_happens_in_linear_reflectivity_not_in_decibels() {
        // A layer spanning 30 and 50 dBZ. Averaging in dBZ would give 40 dBZ
        // and a much smaller answer; the linear mean is 50 050, equivalent to
        // 47.0 dBZ, because the exponential is dominated by its larger end.
        let linear_column = [sample(3_000.0, 30.0), sample(4_000.0, 50.0)];
        let (linear_vil, _) = vertically_integrated_liquid(&linear_column);

        let naive_column = [sample(3_000.0, 40.0), sample(4_000.0, 40.0)];
        let (naive_vil, _) = vertically_integrated_liquid(&naive_column);

        assert!(
            linear_vil > naive_vil * 1.5,
            "linear integration gave {linear_vil} against a dBZ-averaged {naive_vil}; \
             the difference across a 20 dBZ gradient should be large"
        );
    }

    #[test]
    fn the_cap_is_applied_to_the_layer_mean_and_limits_the_total() {
        // A 70 dBZ core would contribute a hundred times more liquid than a
        // 56 dBZ one if it were believed. It is not believed.
        let capped = [sample(3_000.0, 70.0), sample(6_000.0, 70.0)];
        let at_cap = [
            sample(3_000.0, VIL_REFLECTIVITY_CAP_DBZ),
            sample(6_000.0, VIL_REFLECTIVITY_CAP_DBZ),
        ];
        let (capped_vil, _) = vertically_integrated_liquid(&capped);
        let (at_cap_vil, _) = vertically_integrated_liquid(&at_cap);
        assert!(
            (capped_vil - at_cap_vil).abs() < 1e-4,
            "a 70 dBZ column gave {capped_vil} and a 56 dBZ column {at_cap_vil}; \
             the cap should make them identical"
        );
    }

    #[test]
    fn reflectivity_below_the_cap_is_left_alone() {
        let below = [sample(3_000.0, 45.0), sample(6_000.0, 45.0)];
        let at_cap = [
            sample(3_000.0, VIL_REFLECTIVITY_CAP_DBZ),
            sample(6_000.0, VIL_REFLECTIVITY_CAP_DBZ),
        ];
        let (below_vil, _) = vertically_integrated_liquid(&below);
        let (at_cap_vil, _) = vertically_integrated_liquid(&at_cap);
        assert!(below_vil < at_cap_vil);
    }

    #[test]
    fn the_integration_never_bridges_a_layer_the_radar_did_not_sample() {
        // Under a supercell the unsampled layer is the deepest and wettest one
        // in the column, so bridging it does not add a little water - it adds
        // the most water.
        let gapped = [
            sample(3_000.0, 55.0),
            sample_with_state(6_000.0, CellState::NoCoverage),
            sample(9_000.0, 55.0),
        ];
        let (vil, state) = vertically_integrated_liquid(&gapped);
        assert_eq!(vil, 0.0, "no two covered samples were adjacent");
        assert_eq!(state, CellState::LowerBound, "the zero is a floor");

        let filled = [
            sample(3_000.0, 55.0),
            sample(6_000.0, 55.0),
            sample(9_000.0, 55.0),
        ];
        let (filled_vil, _) = vertically_integrated_liquid(&filled);
        assert!(
            filled_vil > 15.0,
            "the same column, sampled throughout, holds {filled_vil} kg m^-2"
        );
    }

    #[test]
    fn a_sampled_no_echo_layer_contributes_nothing_without_breaking_the_chain() {
        // A beam that looked and found nothing is a measurement of zero water,
        // not a gap, so the layers either side of it still integrate.
        let column = [
            sample(3_000.0, 50.0),
            sample_with_state(6_000.0, CellState::NoEcho),
            sample(9_000.0, 50.0),
        ];
        let (vil, _) = vertically_integrated_liquid(&column);
        assert!(
            vil > 0.0,
            "the two layers either side must still contribute"
        );
    }

    #[test]
    fn a_column_the_radar_never_reached_reports_no_coverage_not_zero() {
        let column = [
            sample_with_state(3_000.0, CellState::NoCoverage),
            sample_with_state(6_000.0, CellState::NoCoverage),
        ];
        let (vil, state) = vertically_integrated_liquid(&column);
        assert_eq!(vil, 0.0);
        assert_eq!(state, CellState::NoCoverage);
    }

    #[test]
    fn a_column_the_radar_swept_and_found_empty_reports_no_echo_not_zero() {
        // The distinction the whole state machine exists for: both paint blank,
        // and only one of them is a measurement.
        let column = [
            sample_with_state(3_000.0, CellState::NoEcho),
            sample_with_state(6_000.0, CellState::NoEcho),
        ];
        let (vil, state) = vertically_integrated_liquid(&column);
        assert_eq!(vil, 0.0);
        assert_eq!(state, CellState::NoEcho);
        assert_ne!(state, CellState::NoCoverage);
    }

    #[test]
    fn an_empty_column_reports_no_coverage() {
        assert_eq!(
            vertically_integrated_liquid(&[]),
            (0.0, CellState::NoCoverage)
        );
    }

    #[test]
    fn a_single_covered_sample_has_no_layer_and_reports_a_lower_bound() {
        let column = [sample(3_000.0, 50.0)];
        let (vil, state) = vertically_integrated_liquid(&column);
        assert_eq!(vil, 0.0);
        assert_eq!(state, CellState::LowerBound);
    }

    #[test]
    fn vil_density_is_stored_in_kilograms_per_cubic_metre() {
        // 40 kg m^-2 spread through a 10 km storm is 0.004 kg m^-3, which a
        // forecaster reads as 4 g m^-3.
        let (density, state) =
            vil_density_kg_m3(40.0, CellState::Valid, 10_000.0, CellState::Valid);
        assert!(
            (density - 0.004).abs() < 1e-9,
            "density was {density}, expected 0.004 kg m^-3"
        );
        assert_eq!(state, CellState::Valid);
        assert!(
            (f64::from(density) * 1000.0 - 4.0).abs() < 1e-6,
            "which is 4 g m^-3 on a legend"
        );
    }

    #[test]
    fn a_topped_echo_makes_the_density_an_upper_bound_not_a_lower_one() {
        // The echo top is a floor, so the true denominator is larger and the
        // true density is smaller. Getting this backwards would label the one
        // case that matters - a storm too tall to measure - as if its density
        // could only be higher.
        let (_, state) = vil_density_kg_m3(40.0, CellState::Valid, 10_000.0, CellState::LowerBound);
        assert_eq!(state, CellState::UpperBound);
    }

    #[test]
    fn a_truncated_vil_keeps_its_lower_bound_through_the_division() {
        let (_, state) = vil_density_kg_m3(40.0, CellState::LowerBound, 10_000.0, CellState::Valid);
        assert_eq!(state, CellState::LowerBound);
    }

    #[test]
    fn a_storm_with_no_depth_has_no_density_rather_than_an_infinite_one() {
        let (density, state) = vil_density_kg_m3(40.0, CellState::Valid, 0.0, CellState::Valid);
        assert_eq!(density, 0.0);
        assert_eq!(state, CellState::NoData);
        assert!(density.is_finite(), "infinity would pass every later check");
    }

    #[test]
    fn absence_propagates_through_the_division_without_dividing() {
        assert_eq!(
            vil_density_kg_m3(0.0, CellState::NoCoverage, 10_000.0, CellState::Valid),
            (0.0, CellState::NoCoverage)
        );
        assert_eq!(
            vil_density_kg_m3(40.0, CellState::Valid, 0.0, CellState::NoCoverage),
            (0.0, CellState::NoCoverage)
        );
        assert_eq!(
            vil_density_kg_m3(0.0, CellState::NoEcho, 10_000.0, CellState::Valid),
            (0.0, CellState::NoEcho)
        );
    }
}
