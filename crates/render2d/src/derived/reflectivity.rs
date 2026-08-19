//! Composite reflectivity and echo tops: the two products that are nothing but
//! a reduction of one reflectivity column.
//!
//! The two answer opposite questions about the same column. Composite
//! reflectivity asks *how strong is the strongest return anywhere above this
//! point*, and deliberately throws the height away. Echo top asks *how high
//! does a given reflectivity reach*, and deliberately throws the strength away.
//! Neither can substitute for the other: a 60 dBZ composite says nothing about
//! whether that core sat at 2 km or at 12 km, and a 12 km echo top says nothing
//! about whether the column beneath it held 20 dBZ or 70.
//!
//! Both are pure functions of a `&[ColumnSample]` so they can be pinned against
//! a column written out by hand. See [`super::profile`] for the two invariants
//! every column arrives with: ascending beam-centre height, and at most one
//! entry per nominal elevation.
//!
//! Heights here are **above radar level**, in metres, measured at the **beam
//! centre**. That is the NWS operational convention for the Echo Tops product
//! (NWS Glossary; WSR-88D ROC product description), and it is what
//! `ColumnSample::height_arl_m` already holds. Lakshmanan et al. (2013) note
//! that the traditional NEXRAD echo-top algorithm instead adds half a beamwidth
//! to reach the top edge of the beam; that convention is not used here, and
//! mixing the two would quietly add roughly 0.9 km at 100 km range on the
//! 0.5-degree cut.

use product_engine::CellState;

use crate::derived::profile::ColumnSample;

/// The reflectivity the named echo-tops product promises: ET18 is the height of
/// the highest 18 dBZ echo.
///
/// The legacy RPG echo-tops product actually tests **18.5** dBZ, not 18.0, so a
/// column whose strongest aloft return is 18.2 dBZ gets a top here and none on
/// the RPG product. The product identifier says 18, so 18 is what this default
/// is; a caller that needs to reproduce RPG output must pass 18.5 itself rather
/// than assume this constant matches it.
pub const ECHO_TOP_THRESHOLD_DBZ: f32 = 18.0;

/// The reflectivity whose top height feeds the hail products.
///
/// The 45 dBZ echo top, measured relative to the freezing level, is the
/// discriminator in Waldvogel, A., B. Federer, and P. Grimm, 1979: "Criteria
/// for the Detection of Hail Cells." J. Appl. Meteor., 18, 1521-1525,
/// DOI 10.1175/1520-0450(1979)018<1521:CFTDOH>2.0.CO;2, and it is the input to
/// the probability-of-hail curve that the hail module owns. The threshold lives
/// here, next to the code that applies it, so that the hail module and any
/// stand-alone 45 dBZ echo-top display cannot drift apart.
pub const HAIL_ECHO_TOP_THRESHOLD_DBZ: f32 = 45.0;

/// The reflectivity assumed for a beam that covered a location and reported no
/// echo, so that a bracket exists and the top can be interpolated.
///
/// Lakshmanan, V., K. Hondl, C. K. Potvin, and D. Preignitz, 2013: "An Improved
/// Method for Estimating Radar Echo-Top Height." Wea. Forecasting, 28, 481-488,
/// DOI 10.1175/WAF-D-12-00084.1, section 2, chooses -14 dBZ "because it is the
/// minimum reflectivity value reported by the WSR-88D".
///
/// The failure this fixes: without a value for the empty beam above, the only
/// honest answer is the lower beam's own centre height, so every echo top in
/// the volume snaps to one of a dozen discrete beam heights and the field draws
/// as terraces. With it, a top between two tilts is a number between two tilts.
/// The substitution is only legitimate where the higher beam actually looked -
/// see [`echo_top_m`].
pub const MINIMUM_REPORTABLE_DBZ: f32 = -14.0;

/// What is written into the value slot beside a state that carries no value.
///
/// Zero rather than NaN: the slot must not be read (see [`CellState`]), but a
/// field that is scanned for statistics or handed to a colour lookup by mistake
/// should degenerate into a wrong picture rather than into a NaN that poisons a
/// min/max reduction for the whole pane.
const UNREADABLE_VALUE: f32 = 0.0;

/// The strongest reflectivity anywhere in the column, with no regard to height.
///
/// Ties keep the state of the lowest of the tied samples, which is arbitrary
/// but fixed; nothing downstream may depend on which one won.
///
/// The maximum's own state is carried through rather than forced to
/// [`CellState::Valid`]. If the strongest contributor was itself only a lower
/// bound - a gate clipped at the top of the data scale - then the composite is
/// a lower bound too, and reporting `Valid` would promise an exactness the
/// radar never reported.
pub fn composite_reflectivity(column: &[ColumnSample]) -> (f32, CellState) {
    let mut best: Option<(f32, CellState)> = None;
    for sample in column {
        let Some(dbz) = sample.value() else {
            continue;
        };
        let is_stronger = match best {
            None => true,
            Some((current_dbz, _)) => dbz > current_dbz,
        };
        if is_stronger {
            best = Some((dbz, sample.state));
        }
    }
    match best {
        Some((dbz, state)) => (dbz, state),
        None => (UNREADABLE_VALUE, state_of_a_column_with_no_values(column)),
    }
}

/// The height above radar level, in metres, at which the column last falls
/// through `threshold_dbz` on the way up.
///
/// The highest *qualifying* sample is used, not the first one found from the
/// bottom: an elevated core above a gap in coverage is still an echo top, and
/// stopping at the first sub-threshold sample would report the top of the
/// lowest layer as the top of the storm.
///
/// The interpolation, derived here rather than transcribed. Take reflectivity
/// as linear in height between two adjacent beam centres - the lower one
/// `(h_b, z_b)` with `z_b >= t`, the upper one `(h_a, z_a)` with `z_a < t`:
///
/// ```text
/// z(h) = z_b + (z_a - z_b) * (h - h_b) / (h_a - h_b)
/// ```
///
/// Setting `z(h_top) = t` and solving for `h_top`:
///
/// ```text
/// h_top = h_b + (h_a - h_b) * (z_b - t) / (z_b - z_a)
/// ```
///
/// The fraction `(z_b - t) / (z_b - z_a)` lies in `[0, 1]` by construction,
/// because `z_b - t >= 0` and `z_b - z_a` exceeds it by `t - z_a > 0`. The
/// returned height is therefore between the two beam centres *structurally*,
/// with no clamp to hide a sign error.
///
/// The upper end of that interval is closed, not open. In exact arithmetic
/// `t - z_a > 0` holds the fraction under 1, but in `f32` a `t - z_a` smaller
/// than half an ulp of `z_b - t` makes both subtractions round to the same
/// float, the fraction is exactly 1.0, and the top lands on the upper beam
/// centre. It never goes past it. Read as half-open, a maintainer adding
/// `assert!(top_m < h_a)` would get a failure that is not a defect;
/// `a_fraction_that_rounds_to_one_lands_on_the_upper_beam_and_never_above_it`
/// pins the endpoint so the interval cannot be misread.
///
/// That property is the whole point of deriving it. Lakshmanan, V., K. Hondl,
/// C. K. Potvin, and D. Preignitz, 2013: "An Improved Method for Estimating
/// Radar Echo-Top Height." Wea. Forecasting, 28, 481-488,
/// DOI 10.1175/WAF-D-12-00084.1, print this step as their Eq. (1):
///
/// ```text
/// theta_T = (Z_T - Z_a) * (theta_b - theta_a) / (Z_b - Z_a) + theta_b
/// ```
///
/// where `theta_b` is the highest elevation whose reflectivity `Z_b` reaches
/// the threshold `Z_T`, and `theta_a` is the next elevation up, whose `Z_a`
/// does not. The trailing term is the typo: it must anchor on `theta_a`, not
/// `theta_b`. As printed, `(Z_T - Z_a) / (Z_b - Z_a)` is positive while
/// `(theta_b - theta_a)` is negative, so the equation subtracts a share of the
/// gap from the lower beam instead of adding it to the upper one, and the top
/// lands below the beam that detected the echo. Worked: `theta_b` = 2 degrees
/// at 30 dBZ, `theta_a` = 4 degrees at 10 dBZ, `Z_T` = 18 dBZ gives 1.2
/// degrees as printed, beneath the 2-degree beam; anchoring the identical
/// expression on `theta_a` gives 3.2 degrees, which is what the form above
/// reduces to. Both `vlouf/eth_radar` and the Project Pythia radar cookbook
/// transcribe Eq. (1) as printed. The tests
/// `an_interpolated_top_never_falls_outside_its_bracketing_beams` and
/// `a_beam_barely_over_the_threshold_puts_the_top_just_above_that_beam` pin
/// exactly what that transcription loses.
///
/// One further difference from the paper, deliberate and harmless: Eq. (1)
/// interpolates in *elevation angle*, and this interpolates in *height*.
/// Height is not linear in elevation angle, so the two do not agree exactly
/// even with the anchor corrected - but over realistic VCP geometry they agree
/// to within about 5 m (the worst case across ground ranges of 30 to 150 km
/// and tilt gaps of 1 to 2 degrees is 4.7 m, at 150 km across the 4-to-6
/// degree gap), because `sin(theta)` is very nearly linear over one tilt gap.
/// That is two orders of magnitude below the roughly 0.9 km that the
/// half-beamwidth convention in the module header is worth. Height is the
/// variable the column already carries per tilt and the variable the product
/// reports, so interpolating in it avoids a conversion that could go wrong;
/// there is nothing to gain by restoring the angle form to match the paper.
///
/// The answer is [`CellState::LowerBound`], meaning "the storm is at least
/// this tall", whenever the volume cannot see the top: no beam above the
/// qualifying one, a beam above that did not cover this point, or a beam above
/// whose data are unusable. Never extrapolate above the highest beam; a 70 dBZ
/// return in the top tilt means the storm continues, not that it ends there.
pub fn echo_top_m(column: &[ColumnSample], threshold_dbz: f32) -> (f32, CellState) {
    let Some(top_index) = highest_index_reaching(column, threshold_dbz) else {
        return (UNREADABLE_VALUE, state_of_a_column_with_no_top(column));
    };

    // The search only accepts samples whose state carries a value, so reading
    // the reflectivity slot of this one is sound.
    let height_below_m = column[top_index].height_arl_m;
    let dbz_below = column[top_index].reflectivity_dbz;

    let Some(above) = column.get(top_index + 1) else {
        return (height_below_m, CellState::LowerBound);
    };
    let Some(dbz_above) = reflectivity_to_interpolate_towards(above, threshold_dbz) else {
        return (height_below_m, CellState::LowerBound);
    };

    let span_dbz = dbz_below - dbz_above;
    if span_dbz <= 0.0 || !span_dbz.is_finite() {
        // Not a bracket, so there is nothing to interpolate across. Dividing
        // anyway is exactly how a top ends up below the beam that found it.
        return (height_below_m, CellState::LowerBound);
    }
    let fraction = (dbz_below - threshold_dbz) / span_dbz;
    let top_m = height_below_m + fraction * (above.height_arl_m - height_below_m);
    (top_m, CellState::Valid)
}

/// Index of the highest sample whose value reaches the threshold.
fn highest_index_reaching(column: &[ColumnSample], threshold_dbz: f32) -> Option<usize> {
    column
        .iter()
        .rposition(|sample| sample.value().is_some_and(|dbz| dbz >= threshold_dbz))
}

/// The reflectivity to interpolate towards for the sample above the top, or
/// `None` when that sample cannot bracket the top at all.
///
/// Three cases, and the difference between them is the difference between a
/// measured top and an invented one:
///
/// * the beam reported a value - use it;
/// * the beam covered this point and reported no echo - use
///   [`MINIMUM_REPORTABLE_DBZ`], which is what makes interpolation possible
///   instead of snapping to a beam centre;
/// * the beam did not cover this point, or covered it and produced unusable
///   data (range folded, quality masked, no data) - `None`. An unusable gate is
///   not evidence of absence. Treating one as -14 dBZ would cap a storm at the
///   last good beam and draw a hole in the echo-top field wherever the higher
///   tilt was range folded, which is precisely over the strong cores.
fn reflectivity_to_interpolate_towards(above: &ColumnSample, threshold_dbz: f32) -> Option<f32> {
    if let Some(dbz) = above.value() {
        return Some(dbz);
    }
    if above.state == CellState::NoEcho && MINIMUM_REPORTABLE_DBZ < threshold_dbz {
        // A caller asking for a threshold at or below the minimum reportable
        // value gets no bracket from the substitution, because the assumed
        // value would sit above the threshold it is meant to be below.
        return Some(MINIMUM_REPORTABLE_DBZ);
    }
    None
}

/// The state of a column in which nothing carried a number.
///
/// Precedence, worst provenance first: a folded gate belongs at some other
/// range and its absence here means nothing; unusable data was looked at but
/// cannot be believed; no-echo means the radar looked and the sky was empty;
/// no-coverage means the radar never looked. Collapsing the last two is the
/// failure this whole state machine exists to prevent - an empty field and an
/// unscanned field are both blank on screen and mean opposite things.
///
/// Quality-masked and environment-unavailable gates are reported as
/// [`CellState::NoData`]: they were sampled, and their numbers are unusable,
/// which is exactly what a downstream reader takes `NoData` to mean.
fn state_of_a_column_with_no_values(column: &[ColumnSample]) -> CellState {
    let mut any_folded = false;
    let mut any_unusable = false;
    let mut any_no_echo = false;
    for sample in column {
        match sample.state {
            CellState::RangeFolded => any_folded = true,
            CellState::NoData | CellState::QualityMasked | CellState::EnvironmentUnavailable => {
                any_unusable = true;
            }
            CellState::NoEcho => any_no_echo = true,
            // No beam here, so nothing to report either way.
            CellState::NoCoverage => {}
            // Unreachable in this helper: the caller only calls it when no
            // sample carried a value. Matched rather than wildcarded so that a
            // new state added to the enum fails to compile here.
            CellState::Valid | CellState::LowerBound | CellState::UpperBound => {}
        }
    }
    if any_folded {
        CellState::RangeFolded
    } else if any_unusable {
        CellState::NoData
    } else if any_no_echo {
        CellState::NoEcho
    } else {
        CellState::NoCoverage
    }
}

/// The state of a column that no sample took past the threshold.
///
/// Only one distinction matters here, and it is not the same one composite
/// reflectivity draws: did any beam look at this point. If one did, there is
/// genuinely no echo of the requested strength above it. If none did, this
/// location has no echo top because it has no observation.
fn state_of_a_column_with_no_top(column: &[ColumnSample]) -> CellState {
    if column.iter().any(ColumnSample::is_covered) {
        CellState::NoEcho
    } else {
        CellState::NoCoverage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample that carries a reflectivity.
    fn valued(height_arl_m: f32, dbz: f32) -> ColumnSample {
        ColumnSample {
            cut_index: 0,
            elevation_deg: 0.5,
            height_arl_m,
            slant_range_m: 60_000.0,
            reflectivity_dbz: dbz,
            state: CellState::Valid,
        }
    }

    /// A sample that carries no reflectivity. The number in the value slot is
    /// deliberately absurd: any test that starts passing because that slot was
    /// read has found a real bug.
    fn stated(height_arl_m: f32, state: CellState) -> ColumnSample {
        ColumnSample {
            cut_index: 0,
            elevation_deg: 0.5,
            height_arl_m,
            slant_range_m: 60_000.0,
            reflectivity_dbz: -9999.0,
            state,
        }
    }

    #[test]
    fn the_composite_is_the_strongest_gate_whether_it_sat_low_or_high() {
        // Same three numbers, once with the core at the bottom of the column
        // and once at the top. Composite reflectivity is defined to be blind to
        // the difference, which is why it cannot tell an elevated core from a
        // surface one and why echo tops exist.
        let core_low = [
            valued(1_000.0, 62.0),
            valued(5_000.0, 30.0),
            valued(9_000.0, 12.0),
        ];
        let core_high = [
            valued(1_000.0, 12.0),
            valued(5_000.0, 30.0),
            valued(9_000.0, 62.0),
        ];
        assert_eq!(composite_reflectivity(&core_low), (62.0, CellState::Valid));
        assert_eq!(composite_reflectivity(&core_high), (62.0, CellState::Valid));
    }

    #[test]
    fn one_valid_gate_outranks_every_valueless_state_beside_it() {
        // The state precedence is only a fallback for a column with nothing in
        // it. A single good gate makes the answer a measurement.
        let column = [
            stated(1_000.0, CellState::RangeFolded),
            valued(5_000.0, 41.5),
            stated(9_000.0, CellState::NoData),
        ];
        assert_eq!(composite_reflectivity(&column), (41.5, CellState::Valid));
    }

    #[test]
    fn a_composite_whose_maximum_is_only_a_lower_bound_stays_a_lower_bound() {
        let mut clipped = valued(5_000.0, 80.0);
        clipped.state = CellState::LowerBound;
        let column = [valued(1_000.0, 50.0), clipped];
        assert_eq!(
            composite_reflectivity(&column),
            (80.0, CellState::LowerBound),
            "a gate clipped at the top of the scale does not become exact by being the maximum"
        );
    }

    #[test]
    fn a_folded_contributor_outranks_every_other_reason_for_having_no_value() {
        let column = [
            stated(1_000.0, CellState::NoCoverage),
            stated(5_000.0, CellState::NoEcho),
            stated(7_000.0, CellState::NoData),
            stated(9_000.0, CellState::RangeFolded),
        ];
        assert_eq!(
            composite_reflectivity(&column),
            (0.0, CellState::RangeFolded)
        );
    }

    #[test]
    fn unusable_data_outranks_no_echo_in_a_valueless_composite() {
        let column = [
            stated(1_000.0, CellState::NoEcho),
            stated(5_000.0, CellState::NoData),
            stated(9_000.0, CellState::NoCoverage),
        ];
        assert_eq!(composite_reflectivity(&column), (0.0, CellState::NoData));
    }

    #[test]
    fn a_quality_masked_gate_reads_as_unusable_rather_than_unsampled() {
        // It was looked at, and the number was thrown away. Reporting no
        // coverage would claim the radar never pointed here.
        let column = [
            stated(1_000.0, CellState::QualityMasked),
            stated(5_000.0, CellState::NoCoverage),
        ];
        assert_eq!(composite_reflectivity(&column), (0.0, CellState::NoData));
    }

    #[test]
    fn a_composite_of_nothing_but_no_echo_is_not_a_composite_of_nothing_at_all() {
        let sampled = [
            stated(1_000.0, CellState::NoEcho),
            stated(5_000.0, CellState::NoCoverage),
        ];
        let unsampled = [
            stated(1_000.0, CellState::NoCoverage),
            stated(5_000.0, CellState::NoCoverage),
        ];
        assert_eq!(composite_reflectivity(&sampled), (0.0, CellState::NoEcho));
        assert_eq!(
            composite_reflectivity(&unsampled),
            (0.0, CellState::NoCoverage)
        );
    }

    #[test]
    fn an_empty_column_composites_to_no_coverage_rather_than_to_zero_dbz() {
        assert_eq!(composite_reflectivity(&[]), (0.0, CellState::NoCoverage));
    }

    #[test]
    fn an_echo_top_interpolates_exactly_between_the_two_bracketing_beams() {
        // Beam centres 6000 m at 30.0 dBZ and 8000 m at 10.0 dBZ, threshold
        // 18.0 dBZ. Reflectivity taken linear in height between the centres:
        //   fraction = (30 - 18) / (30 - 10) = 12 / 20 = 0.6
        //   top      = 6000 + 0.6 * (8000 - 6000) = 6000 + 1200 = 7200 m
        let column = [valued(6_000.0, 30.0), valued(8_000.0, 10.0)];
        let (top_m, state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(state, CellState::Valid);
        // 1e-3 m is a millimetre, far below anything physical, but not exact
        // equality: 0.6 is not representable in binary, so the two f32
        // roundings in the divide and the multiply are real. Asserting bit
        // equality would pin an accident of rounding, not the arithmetic.
        assert!(
            (top_m - 7_200.0).abs() < 1e-3,
            "top was {top_m} m, expected 7200 m"
        );
    }

    #[test]
    fn an_interpolated_top_never_falls_outside_its_bracketing_beams() {
        // The property that a verbatim transcription of the published equation
        // loses: a top below the beam that detected the echo. Every
        // combination here brackets 18 dBZ, so every answer must sit inside
        // its own bracket.
        let brackets = [
            (500.0_f32, 1_200.0_f32),
            (3_000.0, 3_050.0),
            (6_000.0, 14_000.0),
        ];
        let profiles = [
            (18.0_f32, 17.9_f32),
            (75.0, -30.0),
            (20.0, -14.0),
            (60.0, 17.999),
            (18.5, 0.0),
            (44.0, -13.5),
        ];
        for (lower_m, upper_m) in brackets {
            for (dbz_below, dbz_above) in profiles {
                let column = [valued(lower_m, dbz_below), valued(upper_m, dbz_above)];
                let (top_m, state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
                assert_eq!(
                    state,
                    CellState::Valid,
                    "{dbz_below} over {dbz_above} dBZ brackets the threshold"
                );
                assert!(top_m.is_finite(), "top was {top_m} for a bracketed column");
                assert!(
                    top_m >= lower_m && top_m <= upper_m,
                    "top {top_m} m escaped its bracket {lower_m}..{upper_m} m \
                     for {dbz_below} over {dbz_above} dBZ"
                );
            }
        }
    }

    #[test]
    fn a_gate_exactly_at_the_threshold_puts_the_top_at_its_own_beam_centre() {
        // fraction = (18 - 18) / (18 - 5) = 0, so the crossing is the beam
        // itself and no part of the gap above it belongs to the echo.
        let column = [valued(4_000.0, 18.0), valued(9_000.0, 5.0)];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (4_000.0, CellState::Valid)
        );
    }

    #[test]
    fn a_shallow_echo_never_reports_a_top_below_the_beam_that_found_it() {
        // The lowest beam is the only qualifying one, and the beam above it is
        // barely under the threshold, which is where a sign error in the
        // interpolation shows up as a top beneath the radar horizon.
        let column = [valued(700.0, 55.0), valued(2_500.0, 17.99)];
        let (top_m, state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(state, CellState::Valid);
        assert!(top_m >= 700.0, "top was {top_m} m, below the 700 m beam");
        assert!(top_m <= 2_500.0, "top was {top_m} m, above the 2500 m beam");
    }

    #[test]
    fn a_storm_still_above_the_highest_beam_is_a_lower_bound() {
        // 52 dBZ in the top tilt means the storm continues above the volume.
        // Extrapolating would invent a top; reporting the beam centre as exact
        // would state that the storm ends at the last thing the radar saw.
        let column = [valued(3_000.0, 60.0), valued(12_000.0, 52.0)];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (12_000.0, CellState::LowerBound)
        );
    }

    #[test]
    fn a_single_qualifying_beam_with_nothing_above_it_is_a_lower_bound() {
        let column = [valued(2_400.0, 44.0)];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (2_400.0, CellState::LowerBound)
        );
    }

    #[test]
    fn the_top_is_the_beam_centre_height_with_no_half_beamwidth_added() {
        // The NWS Echo Tops convention. The traditional NEXRAD algorithm adds
        // half a beamwidth to reach the top edge of the beam; at 100 km on the
        // 0.5-degree cut that is close to another kilometre, so the two
        // conventions disagree by more than most forecast decisions can absorb.
        let column = [valued(9_137.0, 33.0)];
        let (top_m, _) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(top_m, 9_137.0);
    }

    #[test]
    fn a_covered_beam_that_found_no_echo_supplies_minus_fourteen_dbz() {
        // 40 dBZ at 9000 m; the beam at 11000 m swept this point and reported
        // no echo, so Lakshmanan et al. (2013) stand -14 dBZ in for it:
        //   fraction = (40 - 18) / (40 - (-14)) = 22 / 54 = 0.4074074...
        //   top      = 9000 + 0.4074074... * 2000 = 9000 + 814.8148... m
        //            = 9814.8148... m
        let column = [valued(9_000.0, 40.0), stated(11_000.0, CellState::NoEcho)];
        let (top_m, state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(
            state,
            CellState::Valid,
            "an empty beam above is a measured top, not a bound"
        );
        // 1e-2 m, a centimetre: the literal below is the exact quotient cut to
        // two decimals for legibility, so the tolerance has to cover that
        // rounding as well as the f32 arithmetic. It is still four orders of
        // magnitude tighter than the smallest wrong answer this could give,
        // which would be the 9000 m beam centre itself.
        assert!(
            (top_m - 9814.81).abs() < 1e-2,
            "top was {top_m} m, expected 9814.8148 m"
        );
    }

    #[test]
    fn minus_fourteen_dbz_is_used_only_where_the_higher_beam_actually_looked() {
        // Identical geometry, identical echo. The only difference is whether
        // the beam above swept this point. Where it did not, nothing is known
        // about the air up there and the top can only be a lower bound.
        let looked = [valued(9_000.0, 40.0), stated(11_000.0, CellState::NoEcho)];
        let did_not_look = [
            valued(9_000.0, 40.0),
            stated(11_000.0, CellState::NoCoverage),
        ];
        let (interpolated_m, interpolated_state) = echo_top_m(&looked, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(interpolated_state, CellState::Valid);
        assert!(interpolated_m > 9_000.0 && interpolated_m < 11_000.0);
        assert_eq!(
            echo_top_m(&did_not_look, ECHO_TOP_THRESHOLD_DBZ),
            (9_000.0, CellState::LowerBound),
            "an unswept beam must not be read as an empty one"
        );
    }

    #[test]
    fn an_unusable_beam_above_the_echo_is_not_evidence_of_absence() {
        // Range folding sits over strong cores, which is exactly where a top
        // capped at the last good beam would be most wrong.
        for state in [
            CellState::RangeFolded,
            CellState::NoData,
            CellState::QualityMasked,
        ] {
            let column = [valued(9_000.0, 40.0), stated(11_000.0, state)];
            assert_eq!(
                echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
                (9_000.0, CellState::LowerBound),
                "{state:?} above the echo must not stand in for an empty beam"
            );
        }
    }

    #[test]
    fn the_highest_qualifying_beam_wins_even_across_a_gap_in_coverage() {
        // An elevated core above a hole in the volume is still an echo top.
        // Stopping at the first sub-threshold sample from the bottom would
        // report 2000 m and lose 5 km of storm.
        //   fraction = (25 - 18) / (25 - 10) = 7 / 15 = 0.4666666...
        //   top      = 6000 + 0.4666666... * 2000 = 6933.3333... m
        let column = [
            valued(2_000.0, 30.0),
            stated(4_000.0, CellState::NoCoverage),
            valued(6_000.0, 25.0),
            valued(8_000.0, 10.0),
        ];
        let (top_m, state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(state, CellState::Valid);
        // 1e-2 m for the same reason as the -14 dBZ case: 6933.33 is the exact
        // quotient cut to two decimals for printing.
        assert!(
            (top_m - 6933.33).abs() < 1e-2,
            "top was {top_m} m, expected 6933.3333 m"
        );
    }

    #[test]
    fn a_threshold_no_higher_than_the_minimum_reportable_value_yields_a_lower_bound() {
        // The -14 dBZ stand-in only brackets a threshold above it. Using it
        // anyway would divide by a non-positive span, which is how a top ends
        // up below the beam it came from.
        let column = [valued(2_000.0, 10.0), stated(4_000.0, CellState::NoEcho)];
        assert_eq!(
            echo_top_m(&column, MINIMUM_REPORTABLE_DBZ),
            (2_000.0, CellState::LowerBound)
        );
        assert_eq!(echo_top_m(&column, -20.0), (2_000.0, CellState::LowerBound));
    }

    #[test]
    fn a_column_that_was_swept_but_never_reached_the_threshold_reports_no_echo() {
        let column = [
            valued(1_000.0, 12.0),
            valued(5_000.0, 4.0),
            stated(9_000.0, CellState::NoEcho),
        ];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (0.0, CellState::NoEcho)
        );
    }

    #[test]
    fn a_column_no_beam_reached_reports_no_coverage_rather_than_no_echo() {
        // Both draw blank. Only the state tells an analyst whether the sky was
        // empty or the radar was looking somewhere else.
        let column = [
            stated(1_000.0, CellState::NoCoverage),
            stated(5_000.0, CellState::NoCoverage),
        ];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (0.0, CellState::NoCoverage)
        );
    }

    #[test]
    fn an_empty_column_has_no_top_and_no_coverage() {
        assert_eq!(
            echo_top_m(&[], ECHO_TOP_THRESHOLD_DBZ),
            (0.0, CellState::NoCoverage)
        );
    }

    #[test]
    fn the_hail_threshold_tops_out_lower_than_the_named_product_threshold() {
        // One column, two thresholds: the 45 dBZ top is inside the core and the
        // 18 dBZ top is up at the anvil. A hail product fed the 18 dBZ top
        // would read every anvil as a hail column.
        assert_eq!(ECHO_TOP_THRESHOLD_DBZ, 18.0);
        assert_eq!(HAIL_ECHO_TOP_THRESHOLD_DBZ, 45.0);
        assert_eq!(MINIMUM_REPORTABLE_DBZ, -14.0);
        let column = [
            valued(2_000.0, 58.0),
            valued(6_000.0, 50.0),
            valued(10_000.0, 30.0),
            valued(14_000.0, 8.0),
        ];
        let (hail_top_m, hail_state) = echo_top_m(&column, HAIL_ECHO_TOP_THRESHOLD_DBZ);
        let (echo_top_18_m, echo_18_state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
        assert_eq!(hail_state, CellState::Valid);
        assert_eq!(echo_18_state, CellState::Valid);
        // 45 dBZ crossing: fraction = (50 - 45) / (50 - 30) = 5 / 20 = 0.25,
        //   top = 6000 + 0.25 * (10000 - 6000) = 7000 m.
        assert!(
            (hail_top_m - 7_000.0).abs() < 1e-3,
            "45 dBZ top was {hail_top_m} m, expected 7000 m"
        );
        // 18 dBZ crossing: fraction = (30 - 18) / (30 - 8) = 12 / 22
        //   = 0.5454545..., top = 10000 + 0.5454545... * 4000 = 12181.8181... m
        // 1e-2 m again because 12181.82 is the exact quotient rounded for
        // printing.
        assert!(
            (echo_top_18_m - 12181.82).abs() < 1e-2,
            "18 dBZ top was {echo_top_18_m} m, expected 12181.8181 m"
        );
        assert!(hail_top_m < echo_top_18_m);
    }

    #[test]
    fn a_rising_threshold_lowers_the_top_within_one_fixed_bracket() {
        // Direction, which membership of a bracket does not constrain at all.
        // Swapping the numerator for `(t - z_a)` - the other way to mis-copy
        // the rearrangement - leaves every answer inside its bracket and
        // merely runs the field backwards, so a storm's 45 dBZ top would draw
        // higher than its 18 dBZ top and the hail products would read every
        // anvil as a deep core. One bracket, 55 dBZ at 1000 m over -5 dBZ at
        // 9000 m, swept by threshold.
        let column = [valued(1_000.0, 55.0), valued(9_000.0, -5.0)];
        let mut previous_m = f32::INFINITY;
        for threshold_dbz in [0.0_f32, 5.0, 10.0, 18.0, 30.0, 45.0, 54.9, 55.0] {
            let (top_m, state) = echo_top_m(&column, threshold_dbz);
            assert_eq!(state, CellState::Valid, "{threshold_dbz} dBZ is bracketed");
            assert!(
                top_m < previous_m,
                "the {threshold_dbz} dBZ top of {top_m} m is not below the top of \
                 the threshold beneath it, {previous_m} m"
            );
            previous_m = top_m;
        }
        // Two of the sweep pinned exactly, so the test constrains the values
        // and not only their order. Both are exact in binary: at 10 dBZ the
        // fraction is (55 - 10) / 60 = 0.75 and the top is
        // 1000 + 0.75 * 8000 = 7000 m; at 55 dBZ the fraction is 0 and the top
        // is the qualifying beam centre itself.
        assert_eq!(echo_top_m(&column, 10.0), (7_000.0, CellState::Valid));
        assert_eq!(echo_top_m(&column, 55.0), (1_000.0, CellState::Valid));
    }

    #[test]
    fn a_beam_barely_over_the_threshold_puts_the_top_just_above_that_beam() {
        // 18.25 dBZ at 4000 m over -13.75 dBZ at 10000 m, threshold 18. The
        // echo only just qualifies, so almost none of the 6000 m gap above it
        // belongs to the echo:
        //   fraction = (18.25 - 18) / (18.25 - (-13.75)) = 0.25 / 32
        //            = 0.0078125
        //   top      = 4000 + 0.0078125 * 6000 = 4046.875 m
        // Every quantity here is a dyadic rational and exact in f32 - 18.25 is
        // 73/4, -13.75 is -55/4, the span is 32 - so this is asserted bit
        // exactly, with no tolerance for a drift to hide in. The published
        // equation's sign error puts the answer at 3953.125 m, 46.875 m
        // *below* the beam that detected the echo.
        let column = [valued(4_000.0, 18.25), valued(10_000.0, -13.75)];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (4_046.875, CellState::Valid)
        );
    }

    #[test]
    fn a_beam_above_that_barely_misses_puts_the_top_just_below_that_beam() {
        // The mirror case: 49.75 dBZ at 3000 m over 17.75 dBZ at 11000 m. The
        // beam above misses the threshold by a quarter of a dBZ, so nearly the
        // whole 8000 m gap belongs to the echo:
        //   fraction = (49.75 - 18) / (49.75 - 17.75) = 31.75 / 32 = 0.9921875
        //   top      = 3000 + 0.9921875 * 8000 = 10937.5 m
        // Dyadic again - 49.75 is 199/4, 17.75 is 71/4, the span is 32 - so
        // exact. The answer approaches the upper beam from below and stops
        // 62.5 m short of it, which is the half of the interpolation that a
        // bracket-membership sweep cannot tell from its mirror image.
        let column = [valued(3_000.0, 49.75), valued(11_000.0, 17.75)];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (10_937.5, CellState::Valid)
        );
    }

    #[test]
    fn the_top_puts_the_threshold_back_on_the_straight_line_through_both_beams() {
        // Checks the algebra by substituting into the definition rather than
        // by restating the rearrangement. Evaluate the assumed profile
        //   z(h) = z_b + (z_a - z_b) * (h - h_b) / (h_a - h_b)
        // at the height the function returned; it must come back as the
        // threshold. A rearrangement that dropped, flipped or transposed a
        // term cannot survive this even in the cases where its answer happens
        // to stay inside the bracket.
        let brackets = [
            (500.0_f32, 1_200.0_f32),
            (3_000.0, 3_050.0),
            (6_000.0, 14_000.0),
            (9_000.0, 11_000.0),
            (1_000.0, 19_000.0),
        ];
        let profiles = [
            (18.0_f32, 17.9_f32),
            (75.0, -30.0),
            (20.0, -14.0),
            (60.0, 17.999),
            (18.5, 0.0),
            (44.0, -13.5),
            (94.5, -32.0),
        ];
        for (lower_m, upper_m) in brackets {
            for (dbz_below, dbz_above) in profiles {
                let column = [valued(lower_m, dbz_below), valued(upper_m, dbz_above)];
                let (top_m, state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
                assert_eq!(
                    state,
                    CellState::Valid,
                    "{dbz_below} over {dbz_above} dBZ brackets the threshold"
                );
                let on_the_line =
                    dbz_below + (dbz_above - dbz_below) * (top_m - lower_m) / (upper_m - lower_m);
                // 1e-3 dBZ. The worst residual over these 35 combinations is
                // 2.4e-4 dBZ, from the 126.5 dBZ span of 94.5 over -32 across
                // the 50 m bracket, where the height quantum is coarse next to
                // the reflectivity gradient and a round trip through two
                // divisions cannot do better. A tighter bound would pin that
                // one rounding rather than the algebra, and 1e-3 dBZ is still
                // three orders below the 0.5 dBZ the RPG reports reflectivity
                // to.
                assert!(
                    (on_the_line - ECHO_TOP_THRESHOLD_DBZ).abs() < 1e-3,
                    "the top of {top_m} m for {dbz_below} over {dbz_above} dBZ in \
                     {lower_m}..{upper_m} m sits at {on_the_line} dBZ on the profile, \
                     not at {ECHO_TOP_THRESHOLD_DBZ} dBZ"
                );
            }
        }
    }

    #[test]
    fn a_fraction_that_rounds_to_one_lands_on_the_upper_beam_and_never_above_it() {
        // The endpoint the interpolation doc comment claims, made concrete so
        // the interval cannot be misread as half-open. `f32::EPSILON * 16.0`
        // is exactly one ulp at 18 dBZ, because 18 lies in [16, 32), so
        // `just_under` is the largest f32 below the threshold. The two
        // subtractions (60 - 18) and (60 - just_under) then differ by half an
        // ulp of 42 and round to the same float, the fraction is exactly 1.0,
        // and the top is the upper beam centre - never a hair above it.
        let just_under = ECHO_TOP_THRESHOLD_DBZ - f32::EPSILON * 16.0;
        assert!(
            just_under < ECHO_TOP_THRESHOLD_DBZ,
            "one ulp below 18 dBZ must not round back up to 18 dBZ"
        );
        let column = [valued(6_000.0, 60.0), valued(14_000.0, just_under)];
        assert_eq!(
            echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ),
            (14_000.0, CellState::Valid)
        );
    }

    #[test]
    fn only_a_no_echo_beam_above_supplies_the_stand_in_reflectivity() {
        // Exhaustive over `CellState`. The match has no wildcard arm, so a
        // state added to the enum stops this test compiling instead of
        // silently inheriting whichever branch it happens to land in.
        //
        // The failure being pinned: a beam above that was never swept, or
        // whose number was thrown away, being read as an empty sky. That caps
        // the storm at the last good beam and punches a hole in the echo-top
        // field exactly over the range-folded cores, which is where the field
        // is being read. The mirror failure is as bad - refusing the stand-in
        // for a genuinely empty beam terraces every top in the volume onto a
        // dozen discrete beam heights.
        //
        // Note what the height assertion rules out in the bound cases: the
        // answer is the *qualifying* beam at 9000 m, never the 11000 m beam
        // that failed to bracket it. Returning an unswept beam's height would
        // invent 2 km of storm out of a gap in the volume.
        let states = [
            CellState::Valid,
            CellState::LowerBound,
            CellState::UpperBound,
            CellState::NoEcho,
            CellState::NoData,
            CellState::NoCoverage,
            CellState::RangeFolded,
            CellState::QualityMasked,
            CellState::EnvironmentUnavailable,
        ];
        for state in states {
            let (expected_m, expected_state) = match state {
                // These three carry a number of their own, so the stand-in
                // never arises; the interpolation tests cover them.
                CellState::Valid | CellState::LowerBound | CellState::UpperBound => continue,
                // 9000 + (40 - 18) / (40 - (-14)) * 2000
                //   = 9000 + (22 / 54) * 2000 = 9814.8148... m
                CellState::NoEcho => (9_814.815_f32, CellState::Valid),
                CellState::NoData
                | CellState::NoCoverage
                | CellState::RangeFolded
                | CellState::QualityMasked
                | CellState::EnvironmentUnavailable => (9_000.0, CellState::LowerBound),
            };
            let column = [valued(9_000.0, 40.0), stated(11_000.0, state)];
            let (top_m, got_state) = echo_top_m(&column, ECHO_TOP_THRESHOLD_DBZ);
            assert_eq!(got_state, expected_state, "beam above was {state:?}");
            // 1e-2 m, a centimetre: the 9814.815 literal is the exact quotient
            // cut to three decimals for legibility, so the tolerance has to
            // cover that rounding as well as the f32 arithmetic. The bound
            // cases are exact and would pass at any tolerance.
            assert!(
                (top_m - expected_m).abs() < 1e-2,
                "beam above {state:?} gave a top of {top_m} m, expected {expected_m} m"
            );
            assert!(
                top_m <= 11_000.0,
                "beam above {state:?} gave {top_m} m, above the beam that bounds it"
            );
        }
    }
}
