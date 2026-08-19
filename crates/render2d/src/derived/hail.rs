//! The hail family: SHI, MESH, POSH, and POH.
//!
//! Four numbers, three of them from one paper. The Severe Hail Index integrates
//! a reflectivity-derived hail kinetic energy flux through the part of the
//! column that is cold enough to grow hail; the Maximum Expected Hail Size and
//! the Probability of Severe Hail are both closed-form functions of that index.
//! The Probability of Hail is unrelated arithmetic on a single height
//! difference, and is kept here because an analyst reads it beside the others.
//!
//! Witt, A., M. D. Eilts, G. J. Stumpf, J. T. Johnson, E. D. Mitchell, and
//! K. W. Thomas, 1998: "An Enhanced Hail Detection Algorithm for the WSR-88D."
//! *Wea. Forecasting*, **13**, 286-303,
//! DOI 10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2.
//! Equations (1) to (6) are implemented below, each beside its function.
//!
//! **This is an adaptation, and the adaptation matters.** Witt's algorithm is
//! *cell* based: it runs on storm cells identified by SCIT, and its SHI is the
//! integral through the cell's vertical profile of maximum reflectivity. Here
//! it runs on one grid column at a time, which is not what the paper validated.
//! It is, however, the same adaptation the Multi-Radar Multi-Sensor system
//! made, and the gridded MESH that came out of it is what operational
//! forecasters have been reading for a decade:
//!
//! - Cintineo, J. L., T. M. Smith, V. Lakshmanan, H. E. Brooks, and K. L.
//!   Ortega, 2012: "An Empirical Model for Assessing the Severe Weather
//!   Potential of Developing Convection." *Wea. Forecasting*, **27**,
//!   1250-1270, DOI 10.1175/WAF-D-11-00786.1. Gridded MESH from Witt's SHI.
//! - Smith, T. M., and Coauthors, 2016: "Multi-Radar Multi-Sensor (MRMS)
//!   Severe Weather and Aviation Products: Initial Operating Capabilities."
//!   *Bull. Amer. Meteor. Soc.*, **97**, 1617-1630,
//!   DOI 10.1175/BAMS-D-14-00173.1. The operational system, and the source of
//!   the gridded convention taught by the NWS Warning Decision Training
//!   Division.
//!
//! The consequence of the adaptation is that a per-column MESH is noisier than
//! a per-cell MESH and its maximum over a storm runs higher, because a single
//! column can be the one unlucky sample rather than a cell-wide profile. It is
//! not the number the 1998 verification statistics describe.
//!
//! Everything here is a pure function. Nothing samples a volume, allocates, or
//! knows what a pixel is, so each equation can be pinned against a
//! hand-computed column.

use crate::derived::profile::ColumnSample;
use product_engine::{CellState, HailEnvironment};

/// Below this reflectivity no return is treated as hail at all.
///
/// Witt et al. (1998) eq. (1), p. 288, calls both thresholds adaptable; 40 and
/// 50 dBZ are the values the paper used and the values MRMS still runs. They
/// are named rather than inlined so a site that retunes them retunes one place.
pub const REFLECTIVITY_WEIGHT_LOWER_DBZ: f32 = 40.0;

/// At and above this reflectivity the return is treated as entirely hail.
pub const REFLECTIVITY_WEIGHT_UPPER_DBZ: f32 = 50.0;

/// The leading coefficient of Witt et al. (1998) eq. (1), in J m^-2 s^-1.
pub const ENERGY_FLUX_COEFFICIENT: f32 = 5.0e-6;

/// The exponent multiplier of Witt et al. (1998) eq. (1), per dBZ.
pub const ENERGY_FLUX_EXPONENT_PER_DBZ: f32 = 0.084;

/// The leading coefficient of Witt et al. (1998) eq. (3).
///
/// It is only correct when the integration step is in metres. See
/// [`severe_hail_index`].
pub const SHI_COEFFICIENT: f32 = 0.1;

/// Slope of the warning threshold, Witt et al. (1998) eq. (4), in
/// J m^-1 s^-1 per kilometre of freezing-level height.
pub const WARNING_THRESHOLD_SLOPE_PER_KM: f32 = 57.5;

/// Intercept of the warning threshold, Witt et al. (1998) eq. (4), in
/// J m^-1 s^-1.
pub const WARNING_THRESHOLD_INTERCEPT: f32 = -121.0;

/// The floor the paper puts under the warning threshold, in J m^-1 s^-1.
///
/// Witt et al. (1998), p. 291: "If WT < 20 J m^-1 s^-1, then WT is set to
/// 20 J m^-1 s^-1." Without it, eq. (4) goes negative below a 2.10 km freezing
/// level and the POSH logarithm takes the ratio of a positive index to a
/// negative threshold, which is not a probability.
pub const WARNING_THRESHOLD_FLOOR: f32 = 20.0;

/// Slope of the POSH regression, Witt et al. (1998) eq. (5), in percent.
pub const POSH_SLOPE_PERCENT: f32 = 29.0;

/// Intercept of the POSH regression, Witt et al. (1998) eq. (5), in percent.
pub const POSH_INTERCEPT_PERCENT: f32 = 50.0;

/// POSH is reported operationally to the nearest 10 percent.
pub const POSH_ROUNDING_STEP_PERCENT: f32 = 10.0;

/// Coefficient of the MEHS relation, Witt et al. (1998) eq. (6), in
/// mm per (J m^-1 s^-1)^(1/2).
pub const MESH_COEFFICIENT_MM: f32 = 2.54;

/// MEHS is reported operationally to the nearest quarter inch, which is
/// exactly 6.35 mm because the inch is exactly 25.4 mm.
pub const MESH_ROUNDING_STEP_MM: f32 = 6.35;

/// The reflectivity weighting function W(Z) of Witt et al. (1998) eq. (1),
/// p. 288, dimensionless and on 0 to 1.
///
/// ```text
/// W(Z) = 0                        Z <= Z_L
///      = (Z - Z_L) / (Z_U - Z_L)  Z_L < Z < Z_U
///      = 1                        Z >= Z_U
/// ```
///
/// The ramp is what keeps rain out of the hail integral. A 45 dBZ return is
/// counted at half weight because at 45 dBZ the radar cannot tell a heavy rain
/// core from a wet hail core, and weighting it fully would give every summer
/// downpour a hail signature.
pub fn reflectivity_weight(dbz: f32) -> f32 {
    if dbz <= REFLECTIVITY_WEIGHT_LOWER_DBZ {
        return 0.0;
    }
    if dbz >= REFLECTIVITY_WEIGHT_UPPER_DBZ {
        return 1.0;
    }
    (dbz - REFLECTIVITY_WEIGHT_LOWER_DBZ)
        / (REFLECTIVITY_WEIGHT_UPPER_DBZ - REFLECTIVITY_WEIGHT_LOWER_DBZ)
}

/// Hail kinetic energy flux Edot, in J m^-2 s^-1.
///
/// Witt et al. (1998) eq. (1), p. 288:
///
/// ```text
/// Edot = 5e-6 * 10^(0.084 * Z) * W(Z)
/// ```
///
/// `Z` is in dBZ. The exponential is steep: 10 dBZ more reflectivity is a
/// factor of nearly 7 in energy flux, so a calibration bias of a couple of dB
/// moves MESH by a size class. That sensitivity is a property of the published
/// equation, not of this implementation, and it is the reason a hail product
/// must never be presented as a measurement of hail size.
pub fn hail_kinetic_energy_flux(dbz: f32) -> f32 {
    let weight = reflectivity_weight(dbz);
    if weight <= 0.0 {
        // Below the lower threshold the answer is exactly zero, and taking the
        // exponential first would spend the work only to multiply it away.
        return 0.0;
    }
    ENERGY_FLUX_COEFFICIENT * 10.0_f32.powf(ENERGY_FLUX_EXPONENT_PER_DBZ * dbz) * weight
}

/// The temperature-based weighting function W_T(H) of Witt et al. (1998)
/// eq. (2), p. 288, dimensionless and on 0 to 1.
///
/// ```text
/// W_T(H) = 0                          H <= H_0
///        = (H - H_0) / (H_m20 - H_0)  H_0 < H < H_m20
///        = 1                          H >= H_m20
/// ```
///
/// `H_0` is the freezing level and `H_m20` the -20 C level. Both are **above
/// radar level**, and so is `height_arl_m`: mixing an above-radar beam height
/// with a sea-level sounding height puts the melting layer wrong by the site
/// elevation, which at a mountain radar is over a kilometre.
/// [`HailEnvironment`] only ever holds above-radar heights, which is what makes
/// that mistake impossible to make here.
///
/// The ramp says a hail signature high in the storm counts for more than the
/// same signature just above the melting level, because the latter is already
/// melting on the way down.
pub fn thermal_weight(height_arl_m: f32, environment: &HailEnvironment) -> f32 {
    let freezing_level_m = environment.freezing_level().metres();
    if height_arl_m <= freezing_level_m {
        return 0.0;
    }
    if height_arl_m >= environment.minus_twenty_level().metres() {
        return 1.0;
    }
    // The depth is guaranteed positive by HailEnvironment's constructor, which
    // refuses a -20 C level at or below the freezing level, so this cannot
    // divide by zero however the environment was supplied.
    ((height_arl_m - freezing_level_m) / environment.thermal_depth_m()).clamp(0.0, 1.0)
}

/// The Severe Hail Index of Witt et al. (1998) eq. (3), p. 288, in
/// J m^-1 s^-1.
///
/// ```text
/// SHI = 0.1 * integral from H_0 to H_T of W_T(H) * Edot(Z) dH
/// ```
///
/// **`dH` is in metres while eq. (4)'s `H_0` is in kilometres.** The 0.1
/// coefficient only reproduces the paper's magnitudes under that pairing, and
/// getting it wrong is invisible: integrating in kilometres divides the index
/// by a thousand, which turns a supercell's hail core into a POSH of 0 and a
/// MESH under 2 mm - finite, positive, and completely wrong. See
/// `the_severe_hail_index_integrates_depth_in_metres_not_kilometres`.
///
/// The quadrature is the paper's own, and it is not a trapezoid. Each 2D storm
/// component contributes its reflectivity across a slab centred on its own
/// height: `dH_i = (H_(i+1) - H_(i-1)) / 2` for an interior component, and the
/// full distance to the single neighbour for the top and bottom ones. A
/// trapezoid would give the endpoints half a layer each instead of a whole
/// one, which on a three-sample column understates the index by exactly a
/// factor of 3/2 - 82.24 against 123.35 J m^-1 s^-1 on the column in
/// `the_severe_hail_index_uses_the_papers_slab_quadrature`, which is 23.03 mm
/// of MESH against 28.21 mm and a POSH of 63.6 against 75.3, reported as 60 and
/// 80 percent after the operational rounding. Two bins apart.
///
/// Matching the paper is not pedantry here. The 2.54 of eq. (6) and the 29 and
/// 50 of eq. (5) are empirical regressions fitted to indices produced by this
/// quadrature. Feeding a systematically smaller index through a calibration
/// built for a larger one biases MESH and POSH low, and nothing downstream can
/// detect that it happened.
///
/// Each slab is clipped at `H_0`. That clipping is not an extra rule bolted on:
/// the bottom slab spans `[H_1 - (H_2 - H_1)/2, (H_1 + H_2)/2]`, so clipping it
/// at `H_0` yields a depth of `(H_1 + H_2)/2 - H_0`, which is exactly the
/// special case the paper states for a cell whose base is above the melting
/// level. The two rules are the same rule.
///
/// Slabs are summed only over runs of samples that are **adjacent in the column
/// and all covered**. A sample the radar never reached breaks the run: the
/// profile across an unsampled gap is unknown, and spanning the cone of silence
/// would invent the storm's most important number.
///
/// The returned state:
///
/// - `NoCoverage` when no beam reached this column at or above the freezing
///   level. Zero would be a claim that there is no hail here, and nothing
///   looked.
/// - `LowerBound` when the integral is known to be short: the highest covered
///   beam still carried a hail-weighted echo, so the storm continues above the
///   data; or a covered but unusable sample inside the cold layer was counted
///   as zero energy; or a lone covered sample had no neighbour to give it a
///   slab thickness.
/// - `Valid` only for a clean, fully sampled column.
///
/// This is a **per-column adaptation**. Witt et al. apply these relations to
/// storm cells identified by SCIT, not to grid columns. MRMS makes the same
/// adaptation, so it is well trodden, but it is not the operational HDA.
///
/// The environment is passed by reference and therefore always exists; a
/// caller with no environment reports `EnvironmentUnavailable` itself rather
/// than calling this at all.
pub fn severe_hail_index(
    column: &[ColumnSample],
    environment: &HailEnvironment,
) -> (f32, CellState) {
    let freezing_level_m = environment.freezing_level().metres();
    let mut integral = 0.0_f32;
    let mut sampled_at_or_above_freezing_level = false;
    let mut is_lower_bound = false;
    let mut highest_covered: Option<&ColumnSample> = None;

    for sample in column {
        if !sample.is_covered() {
            continue;
        }
        highest_covered = Some(sample);
        if sample.height_arl_m >= freezing_level_m {
            sampled_at_or_above_freezing_level = true;
            if sample.value().is_none() && sample.state != CellState::NoEcho {
                // The radar looked and came back with nothing usable - range
                // folded, absent, or quality masked. It is integrated as zero
                // energy because inventing energy would manufacture hail, but
                // the resulting index can then only be too small.
                is_lower_bound = true;
            }
        }
    }

    // Integrate each run of adjacent covered samples separately. Splitting at
    // an uncovered sample is the whole mechanism that stops the integral
    // bridging a gap the radar did not sample.
    let mut run_start = 0_usize;
    for index in 0..=column.len() {
        let run_ended = index == column.len() || !column[index].is_covered();
        if !run_ended {
            continue;
        }
        let run = &column[run_start..index];
        if run.len() == 1 && run[0].height_arl_m >= freezing_level_m {
            // One covered sample with no neighbour has no defensible slab
            // thickness, so it contributes nothing - and the index is then
            // known to be short of the truth rather than equal to it.
            if run[0]
                .value()
                .is_some_and(|dbz| reflectivity_weight(dbz) > 0.0)
            {
                is_lower_bound = true;
            }
        }
        integral += slab_sum_over_run(run, freezing_level_m, environment);
        run_start = index + 1;
    }

    if !sampled_at_or_above_freezing_level {
        return (0.0, CellState::NoCoverage);
    }
    if let Some(top) = highest_covered
        && let Some(dbz) = top.value()
        && reflectivity_weight(dbz) > 0.0
    {
        // The storm was still echoing at the highest beam that reached this
        // column, so H_T is above the data and the integral stops early.
        is_lower_bound = true;
    }

    let shi = SHI_COEFFICIENT * integral;
    if is_lower_bound {
        (shi, CellState::LowerBound)
    } else {
        (shi, CellState::Valid)
    }
}

/// Sum eq. (3)'s slabs over one run of adjacent covered samples, clipped at
/// the freezing level, in J m^-1 s^-1 before the 0.1 coefficient is applied.
///
/// Every sample owns a slab centred on its own height, reaching half way to
/// each neighbour. The top and bottom samples have only one neighbour, so they
/// reach the same distance on both sides and end up owning a full spacing -
/// which is the paper's rule for the endpoints, arrived at without a special
/// case. A run shorter than two samples has no spacing to work with and
/// contributes nothing.
fn slab_sum_over_run(
    run: &[ColumnSample],
    freezing_level_m: f32,
    environment: &HailEnvironment,
) -> f32 {
    if run.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0_f32;
    for (index, sample) in run.iter().enumerate() {
        let height_m = sample.height_arl_m;
        // Reach half way to each neighbour; where there is no neighbour, mirror
        // the one that exists.
        let below_m = if index == 0 {
            (run[1].height_arl_m - height_m) / 2.0
        } else {
            (height_m - run[index - 1].height_arl_m) / 2.0
        };
        let above_m = if index + 1 == run.len() {
            (height_m - run[index - 1].height_arl_m) / 2.0
        } else {
            (run[index + 1].height_arl_m - height_m) / 2.0
        };

        let base_m = (height_m - below_m).max(freezing_level_m);
        let top_m = height_m + above_m;
        let depth_m = top_m - base_m;
        if !depth_m.is_finite() || depth_m <= 0.0 {
            // Entirely below the freezing level, or a degenerate pair.
            continue;
        }
        // The weights are evaluated at the sample's own height, not averaged
        // across the slab: eq. (3) treats each component as one measurement
        // spread over its layer.
        total += thermal_weight(height_m, environment) * energy_flux_of(sample) * depth_m;
    }
    total
}

/// The energy flux a covered sample contributes.
///
/// A covered sample with no readable value contributes zero. For `NoEcho` that
/// is the truth: the radar looked and there was nothing there. For the unusable
/// states it is a deliberate understatement, and [`severe_hail_index`] flags
/// the result as a lower bound rather than letting the zero pass for a
/// measurement.
fn energy_flux_of(sample: &ColumnSample) -> f32 {
    match sample.value() {
        Some(dbz) => hail_kinetic_energy_flux(dbz),
        None => 0.0,
    }
}

/// Maximum Expected Hail Size, in **millimetres**.
///
/// Witt et al. (1998) eq. (6), p. 293:
///
/// ```text
/// MEHS = 2.54 * sqrt(SHI)
/// ```
///
/// The paper writes it in millimetres, and this returns millimetres. The
/// coefficient 2.54 is not the inch conversion appearing by coincidence; it is
/// the fitted coefficient of a size regression, and treating this number as
/// inches because 2.54 looks familiar would report a 25 mm stone as 25 in.
///
/// A negative index has no square root and cannot arise from
/// [`severe_hail_index`], which sums non-negative terms; it is floored at zero
/// here so a caller passing a stored NaN or a hand-built negative gets 0 mm
/// rather than a NaN painted as a hail size.
pub fn mesh_mm(shi: f32) -> f32 {
    MESH_COEFFICIENT_MM * shi.max(0.0).sqrt()
}

/// MEHS rounded to the nearest quarter inch, in millimetres.
///
/// The operational product is reported in quarter-inch steps because that is
/// the resolution of a public hail report ("quarter", "golf ball"). The
/// unrounded value stays available in [`mesh_mm`] so that a colour scale and a
/// maximum-over-storm readout do not both quantise, which would turn a smooth
/// gradient into six visible plateaus.
pub fn mesh_rounded_mm(shi: f32) -> f32 {
    (mesh_mm(shi) / MESH_ROUNDING_STEP_MM).round() * MESH_ROUNDING_STEP_MM
}

/// The warning threshold WT, in J m^-1 s^-1.
///
/// Witt et al. (1998) eq. (4), p. 291:
///
/// ```text
/// WT = 57.5 * H_0 - 121
/// ```
///
/// **`H_0` is in kilometres here**, unlike the metres of eq. (3). A higher
/// freezing level demands a larger index for the same probability of severe
/// hail, because a deeper warm layer melts more of what falls through it.
///
/// The floor at [`WARNING_THRESHOLD_FLOOR`] is in the paper, not an invention
/// of this implementation.
pub fn warning_threshold(environment: &HailEnvironment) -> f32 {
    let freezing_level_km = environment.freezing_level().metres() / 1000.0;
    let threshold =
        WARNING_THRESHOLD_SLOPE_PER_KM * freezing_level_km + WARNING_THRESHOLD_INTERCEPT;
    threshold.max(WARNING_THRESHOLD_FLOOR)
}

/// Probability of Severe Hail, in percent on 0 to 100.
///
/// Witt et al. (1998) eq. (5), p. 292:
///
/// ```text
/// POSH = 29 * ln(SHI / WT) + 50
/// ```
///
/// "Severe" here means hail of at least 0.75 in (19 mm) diameter, the severe
/// criterion in force when the paper was written; it is not the 1 in criterion
/// the National Weather Service adopted in 2010, and the regression was never
/// refitted. A POSH of 70 is a 70 percent chance of *three-quarter inch* hail.
///
/// An index of exactly zero has no logarithm. No special case is written for
/// it: `ln(0)` is negative infinity, and the clamp turns that into 0 percent,
/// which is the right answer.
pub fn posh_percent(shi: f32, environment: &HailEnvironment) -> f32 {
    let ratio = shi / warning_threshold(environment);
    (POSH_SLOPE_PERCENT * ratio.ln() + POSH_INTERCEPT_PERCENT).clamp(0.0, 100.0)
}

/// POSH rounded to the nearest 10 percent, as it is reported operationally.
///
/// Kept separate from [`posh_percent`] for the same reason as
/// [`mesh_rounded_mm`]: a rounded field makes a terrible colour scale, and a
/// probability regression fitted to a few hundred storms does not support the
/// precision an unrounded label implies.
pub fn posh_rounded_percent(shi: f32, environment: &HailEnvironment) -> f32 {
    (posh_percent(shi, environment) / POSH_ROUNDING_STEP_PERCENT).round()
        * POSH_ROUNDING_STEP_PERCENT
}

/// The POH lookup table: depth of the 45 dBZ echo above the freezing level, in
/// kilometres, against probability of hail in percent.
///
/// Published verbatim as Table 1, "Probability of Hail at the surface according
/// to height of the 45 dBZ contour above freezing", of Foote, G. B., T. W.
/// Krauss, and V. Makitov, 2005: "Hail Metrics Using Conventional Radar."
/// *16th Conf. on Planned and Inadvertent Weather Modification*, 85th AMS
/// Annual Meeting, San Diego, CA, paper 1.5 (AMS confex 86773,
/// <https://ams.confex.com/ams/pdfpapers/86773.pdf>). The preprint carries no
/// printed page numbers, so none are quoted: a page range invented to round the
/// citation out reads exactly like a checked one. That paper tabulates the
/// probabilities as fractions 0.0 to 1.0 under a column head that says percent;
/// they are held here in percent because percent is the engine unit of every
/// probability product.
///
/// The attribution matters and is routinely got wrong. The 45 dBZ criterion
/// comes from Waldvogel, A., B. Federer, and P. Grimm, 1979: "Criteria for the
/// Detection of Hail Cells." *J. Appl. Meteor.*, **18**, 1521-1525,
/// DOI 10.1175/1520-0450(1979)018<1521:CFTDOH>2.0.CO;2, whose Grossversuch IV
/// hailpad data (Switzerland, 1977) underlie these numbers - but that paper
/// gives a binary hail / no-hail *detection* criterion, not a probability
/// curve, and the table must not be cited to it. Note also that Foote et al.
/// (2005) print that volume as 25 in their own reference list; 18 is the
/// correct one, and 25 is 1986. Witt et al. (1998) Fig. 2, p. 287, first drew
/// the Waldvogel data as a probability curve; Foote et al. (2005) published the
/// eleven knots.
pub const POH_TABLE: [(f32, f32); 11] = [
    (1.65, 0.0),
    (1.80, 10.0),
    (1.97, 20.0),
    (2.17, 30.0),
    (2.40, 40.0),
    (2.70, 50.0),
    (3.07, 60.0),
    (3.55, 70.0),
    (4.20, 80.0),
    (5.00, 90.0),
    (5.80, 100.0),
];

/// Probability of Hail, in percent on 0 to 100, of hail of any size at the
/// ground.
///
/// The predictor is a **difference**: the height of the 45 dBZ echo top minus
/// the freezing level. Because it is a difference, the vertical datum cancels,
/// provided both heights use the same one. Both are above radar level here, so
/// they do. Passing a sea-level echo top against an above-radar freezing level
/// would shift the answer by the site elevation - at 370 m that is most of a
/// probability step, and at a mountain radar it is four of them.
///
/// Linear interpolation between the knots of [`POH_TABLE`]; zero below the
/// first knot, 100 at and above the last. The published table is eleven points
/// and the operational HDA reads it as a step function - Witt et al. (1998)
/// Fig. 2, p. 287, is drawn as a staircase - so the interpolation is this
/// implementation's choice, made because a step function would draw contours at
/// the table's spacing rather than at the storm's edges. It is a deviation from
/// the operational product, and it shows: smooth POH gradients here where the
/// HDA has ten flat terraces.
///
/// Two other curves answer to "POH" and neither is this one. Substituting
/// either is pinned against by
/// `the_probability_of_hail_curve_is_the_table_and_not_a_curve_fitted_to_it`.
///
/// - The third-order fit `-1.20231 + 1.00184 dH - 0.17018 dH^2 + 0.01086 dH^3`
///   (`dH` in km, answer a fraction), which is **also Foote et al. (2005)**,
///   printed above their own Fig. 1 in the same preprint as Table 1, and which
///   MeteoSwiss has run operationally since 2008. It is a least-squares fit to
///   these very knots, so it is not a rival climatology - it is a smoothed
///   version of this table that misses the points it was fitted to: at
///   `dH = 2.70` km the table says 50 percent and the cubic says 47.58. Do not
///   credit that polynomial to Kopp, J., A. Hering, U. Germann, and O. Martius,
///   2024: "Verification of Weather-Radar-Based Hail Metrics with Crowdsourced
///   Observations from Switzerland." *Atmos. Meas. Tech.*, **17**, 4529-4552,
///   DOI 10.5194/amt-17-4529-2024. Their eq. (1), Sect. 2.1, quotes it and
///   attributes it to Foote; what is theirs is a *recalibration* of POH against
///   crowdsourced reports, which is a third curve again.
/// - The KNMI linear fit `0.319 + 0.133 dH` of Holleman, I., 2001: "Hail
///   Detection Using Single-Polarization Radar." *KNMI Scientific Report*
///   WR-2001-01, 72 pp. That one is a genuinely different calibration, refitted
///   to Dutch C-band data, and it disagrees violently at the bottom of the
///   range: at `dH = 1.65` km, where this table reads 0 percent, it reads 53.8.
///
/// MESHS is not a POH at all - it is a hail *size* estimate - and swapping it
/// in would put centimetres on a probability legend.
///
/// A column with no 45 dBZ echo at all has no echo top; the caller reports that
/// as `NoEcho` rather than passing a sentinel height here.
pub fn probability_of_hail_percent(echo_top_45_arl_m: f32, environment: &HailEnvironment) -> f32 {
    let depth_km = (echo_top_45_arl_m - environment.freezing_level().metres()) / 1000.0;

    let first_knot = POH_TABLE[0];
    if depth_km <= first_knot.0 {
        return first_knot.1;
    }
    let last_knot = POH_TABLE[POH_TABLE.len() - 1];
    if depth_km >= last_knot.0 {
        return last_knot.1;
    }

    for knots in POH_TABLE.windows(2) {
        let (lower_km, lower_percent) = knots[0];
        let (upper_km, upper_percent) = knots[1];
        if depth_km <= upper_km {
            let fraction = (depth_km - lower_km) / (upper_km - lower_km);
            return lower_percent + fraction * (upper_percent - lower_percent);
        }
    }
    // Unreachable: the bound checks above cover everything at or beyond the
    // last knot, and the loop covers everything between the first and the last.
    last_knot.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use product_engine::{HailEnvironmentProvenance, HeightArlM};

    /// The fallback environment: a 3 km freezing level and a 6 km -20 C level,
    /// so the thermal ramp is 3000 m deep and its midpoint is 4500 m.
    fn standard_environment() -> HailEnvironment {
        HailEnvironment::climatological_fallback()
    }

    fn environment(freezing_m: f32, minus_twenty_m: f32) -> HailEnvironment {
        HailEnvironment::new(
            HeightArlM(freezing_m),
            HeightArlM(minus_twenty_m),
            HailEnvironmentProvenance::UserEnteredArl,
        )
        .expect("the test environments are ordered and plausible")
    }

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
            // Deliberately a large number: a state that carries no value must
            // never let this reach the arithmetic.
            reflectivity_dbz: 75.0,
            state,
        }
    }

    // ---- Witt eq. (1): the reflectivity weight ------------------------------

    #[test]
    fn the_reflectivity_weight_is_zero_at_forty_half_at_forty_five_and_one_at_fifty_dbz() {
        assert_eq!(reflectivity_weight(40.0), 0.0);
        assert_eq!(reflectivity_weight(45.0), 0.5);
        assert_eq!(reflectivity_weight(50.0), 1.0);
    }

    #[test]
    fn the_reflectivity_weight_is_flat_outside_the_ramp() {
        // Rain contributes nothing however heavy, and a 70 dBZ core is not
        // weighted more than fully hail.
        assert_eq!(reflectivity_weight(0.0), 0.0);
        assert_eq!(reflectivity_weight(39.9), 0.0);
        assert_eq!(reflectivity_weight(70.0), 1.0);
    }

    // ---- Witt eq. (1): the energy flux --------------------------------------

    #[test]
    fn the_energy_flux_is_exactly_zero_where_the_weight_is_zero() {
        // Not "small": exactly zero, so a column of 35 dBZ rain integrates to
        // an SHI of exactly zero rather than to a small positive number that
        // colours the whole stratiform shield.
        assert_eq!(hail_kinetic_energy_flux(35.0), 0.0);
        assert_eq!(hail_kinetic_energy_flux(40.0), 0.0);
    }

    #[test]
    fn the_energy_flux_at_fifty_dbz_is_the_published_expression() {
        // 5e-6 * 10^(0.084 * 50) * 1 = 5e-6 * 10^4.2
        //                            = 5e-6 * 15848.931924611136
        //                            = 0.0792446596230557
        let flux = hail_kinetic_energy_flux(50.0);
        // 1e-7 is about 13 ulp of 0.0792 in f32, which covers the rounding of
        // powf; a tighter bound would pin the libm implementation rather than
        // the equation. A wrong coefficient or exponent misses by orders of
        // magnitude, not by ulps. The literal carries only the digits an f32
        // holds; the full-precision value is in the arithmetic above.
        assert!(
            (flux - 0.079_244_66).abs() < 1e-7,
            "Edot(50 dBZ) was {flux}, expected 0.0792446596"
        );
    }

    #[test]
    fn the_energy_flux_is_halved_by_the_weight_at_forty_five_dbz() {
        // 5e-6 * 10^(0.084 * 45) * 0.5 = 5e-6 * 6025.595860743575 * 0.5
        //                              = 0.0150639896518589
        let flux = hail_kinetic_energy_flux(45.0);
        assert!(
            (flux - 0.015_063_99).abs() < 1e-8,
            "Edot(45 dBZ) was {flux}, expected 0.0150639897"
        );
    }

    // ---- Witt eq. (2): the thermal weight -----------------------------------

    #[test]
    fn the_thermal_weight_is_zero_below_and_at_the_freezing_level() {
        let environment = standard_environment();
        assert_eq!(thermal_weight(0.0, &environment), 0.0);
        assert_eq!(thermal_weight(2_999.0, &environment), 0.0);
        assert_eq!(thermal_weight(3_000.0, &environment), 0.0);
    }

    #[test]
    fn the_thermal_weight_is_one_half_midway_between_the_thermal_levels() {
        assert_eq!(thermal_weight(4_500.0, &standard_environment()), 0.5);
    }

    #[test]
    fn the_thermal_weight_is_one_at_and_above_the_minus_twenty_level() {
        let environment = standard_environment();
        assert_eq!(thermal_weight(6_000.0, &environment), 1.0);
        assert_eq!(thermal_weight(15_000.0, &environment), 1.0);
    }

    // ---- Witt eq. (3): the severe hail index --------------------------------

    #[test]
    fn a_three_sample_column_integrates_to_the_hand_computed_severe_hail_index() {
        // H_0 = 3000 m, H_-20 = 6000 m.
        //
        //   height   Z       W_T   Edot                 W_T * Edot
        //   3000 m   50 dBZ  0.0   0.0792446596230557   0.0
        //   4500 m   60 dBZ  0.5   0.5482390980715926   0.2741195490357963
        //   6000 m   30 dBZ  1.0   0.0 (W_Z(30) = 0)    0.0
        //
        //   slab at 3000 m: clipped to [3000, 3750], W_T = 0, so 0.0
        //   slab at 4500 m: [3750, 5250], depth 1500
        //                   0.2741195490357963 * 1500 = 411.17932355369444
        //   slab at 6000 m: [5250, 6750], depth 1500, but Edot(30 dBZ) = 0
        //   SHI = 0.1 * 411.17932355369444 = 41.117932355369444
        //
        // Note the two ends contribute nothing for opposite reasons: the bottom
        // sample is at the melting level where the thermal weight is zero, and
        // the top sample is at 30 dBZ where the reflectivity weight is zero.
        let column = [
            sample(3_000.0, 50.0),
            sample(4_500.0, 60.0),
            sample(6_000.0, 30.0),
        ];
        let (shi, _) = severe_hail_index(&column, &standard_environment());
        assert!(
            (shi - 41.117_93).abs() < 1e-3,
            "SHI was {shi}, expected 41.1179"
        );
    }

    #[test]
    fn the_severe_hail_index_integrates_depth_in_metres_not_kilometres() {
        // The trap eq. (3) sets: the 0.1 coefficient pairs with a depth in
        // METRES, while eq. (4)'s freezing level is in KILOMETRES. Integrating
        // in kilometres divides the index by a thousand, and the result is
        // still finite and positive - it just reports no hail in a 60 dBZ core
        // reaching the -20 C level.
        let environment = standard_environment();
        let column = [
            sample(3_000.0, 60.0),
            sample(4_500.0, 60.0),
            sample(6_000.0, 60.0),
        ];
        let (shi, _) = severe_hail_index(&column, &environment);
        assert!(
            (shi - 123.353_8).abs() < 1e-3,
            "the metre answer was {shi}, expected 123.3538"
        );

        let shi_if_the_step_were_kilometres = shi / 1_000.0;
        assert_eq!(
            posh_percent(shi_if_the_step_were_kilometres, &environment),
            0.0,
            "the kilometre answer would report no chance of severe hail at all"
        );
        assert!(
            mesh_mm(shi_if_the_step_were_kilometres) < 1.0,
            "the kilometre answer would report sub-millimetre hail in a 60 dBZ core"
        );
        assert!(
            mesh_mm(shi) > 25.0,
            "the metre answer should report severe hail, got {} mm",
            mesh_mm(shi)
        );
    }

    #[test]
    fn a_layer_straddling_the_freezing_level_is_clipped_at_it() {
        // Samples at 1000 m and 4500 m with H_0 = 3000 m. Each is an endpoint
        // of a two-sample run, so each owns a slab a full spacing wide -
        // 3500 m - centred on itself, and each slab is then clipped at H_0.
        //
        //   1000 m: slab [-750, 2750]; the clipped base of 3000 is above its
        //           top, so the slab lies wholly below the melting level and
        //           contributes nothing
        //   4500 m: slab [2750, 6250], clipped to [3000, 6250], depth 3250
        //           flux = 0.5 * 0.5482390980715926 = 0.2741195490357963
        //           0.2741195490357963 * 3250 = 890.8885343663379
        //   SHI = 89.08885343663379
        let column = [sample(1_000.0, 60.0), sample(4_500.0, 60.0)];
        let (shi, _) = severe_hail_index(&column, &standard_environment());
        assert!(
            (shi - 89.088_85).abs() < 1e-3,
            "SHI was {shi}, expected 89.0889"
        );
    }

    #[test]
    fn the_severe_hail_index_never_bridges_a_gap_the_radar_did_not_sample() {
        // The 3000 m and 6000 m beams both found a 60 dBZ core, but nothing
        // sampled between them. That leaves two runs of one sample each, and a
        // lone sample has no neighbour to give it a slab thickness, so the
        // column integrates to nothing rather than to an invented hail core.
        let environment = standard_environment();
        let column = [
            sample(3_000.0, 60.0),
            sample_with_state(4_500.0, CellState::NoCoverage),
            sample(6_000.0, 60.0),
        ];
        let (shi, state) = severe_hail_index(&column, &environment);
        assert_eq!(shi, 0.0, "an unsampled gap must not be interpolated across");
        assert_eq!(
            state,
            CellState::LowerBound,
            "the zero is a floor and not a measurement"
        );

        // The same column with the middle beam present integrates normally, so
        // the zero above is the gap and not a broken loop.
        let without_gap = [
            sample(3_000.0, 60.0),
            sample(4_500.0, 60.0),
            sample(6_000.0, 60.0),
        ];
        let (filled_shi, _) = severe_hail_index(&without_gap, &environment);
        assert!(
            (filled_shi - 123.353_8).abs() < 1e-3,
            "the sampled column integrated to {filled_shi}, expected 123.3538"
        );
    }

    #[test]
    fn a_bridged_gap_would_not_look_wrong_it_would_look_like_a_bigger_storm() {
        // The test above lands on zero when the gap is respected, and a zero is
        // conspicuous. This is the dangerous shape instead: the sampled part of
        // the column integrates to a perfectly ordinary index, so a bridging
        // bug would not announce itself - it would just grow the storm.
        //
        // Covered at 3000, 6000 and 7500 m at 60 dBZ; nothing sampled 4500 m.
        // That leaves two runs: a lone sample at 3000 m, which has no
        // neighbour to give it a slab thickness and so contributes nothing,
        // and the 6000-7500 m pair, where both samples are endpoints and each
        // owns a full 1500 m spacing at W_T = 1:
        //
        //   slab at 6000 m: 0.5482390980715925 * 1500 = 822.3586471073888
        //   slab at 7500 m: 0.5482390980715925 * 1500 = 822.3586471073888
        //   SHI = 164.47172942147776, MESH 32.5745 mm
        //
        // Ignoring coverage would make one run of the three valued samples,
        // giving the 6000 m sample a 2250 m slab reaching down across the
        // unsampled layer:
        //
        //   slab at 6000 m: 0.5482390980715925 * 2250 = 1233.5379706610831
        //   slab at 7500 m: 0.5482390980715925 * 1500 = 822.3586471073888
        //   SHI = 205.5896617768472, MESH 36.4196 mm
        //
        // Neither number looks suspicious on a legend; one of them is a storm
        // the radar never saw.
        let environment = standard_environment();
        let column = [
            sample(3_000.0, 60.0),
            sample_with_state(4_500.0, CellState::NoCoverage),
            sample(6_000.0, 60.0),
            sample(7_500.0, 60.0),
        ];
        let (shi, state) = severe_hail_index(&column, &environment);
        assert!(
            (shi - 164.471_73).abs() < 1e-3,
            "SHI was {shi}, expected 164.4717 from the one adjacent pair alone"
        );

        // The same three valued samples with nothing missing between them.
        let bridged = [
            sample(3_000.0, 60.0),
            sample(6_000.0, 60.0),
            sample(7_500.0, 60.0),
        ];
        let (bridged_shi, _) = severe_hail_index(&bridged, &environment);
        assert!(
            (bridged_shi - 205.589_66).abs() < 1e-3,
            "the bridged column gave {bridged_shi}, expected 205.5897"
        );
        assert!(
            bridged_shi > shi,
            "bridging an unsampled layer can only add energy that was never measured"
        );
        assert_eq!(
            state,
            CellState::LowerBound,
            "the 7500 m beam still echoes at 60 dBZ, so the storm continues above it"
        );
        // The same claim in the unit on the legend. A hundredth of a millimetre
        // is 3e-4 relative here: far inside any structural error and far
        // outside f32 rounding.
        //
        //   2.54 * sqrt(164.4717) = 32.5746 mm, which rounds to 31.75 mm (1.25 in)
        //   2.54 * sqrt(205.5897) = 36.4196 mm, which rounds to 38.10 mm (1.50 in)
        //
        // One reported hail size apart, from a layer nothing measured.
        let honest_mesh = mesh_mm(shi);
        let bridged_mesh = mesh_mm(bridged_shi);
        assert!(
            (honest_mesh - 32.574_62).abs() < 1e-2,
            "MESH was {honest_mesh} mm, expected 32.5746"
        );
        assert!(
            (bridged_mesh - 36.419_63).abs() < 1e-2,
            "a bridged MESH would have been {bridged_mesh} mm, expected 36.4196"
        );
        assert_ne!(
            mesh_rounded_mm(shi),
            mesh_rounded_mm(bridged_shi),
            "the two land in different quarter-inch bins"
        );
    }

    #[test]
    fn the_severe_hail_index_uses_the_papers_slab_quadrature() {
        // Witt et al. (1998), p. 288, sum slabs, not trapezoids: an interior
        // component spans (H_(i+1) - H_(i-1))/2 and the top and bottom
        // components span the full distance to their single neighbour. The
        // endpoints therefore carry a whole layer each where a trapezoid
        // carries half of one.
        //
        // Column at 3000 / 4500 / 6000 m, all 60 dBZ, H_0 = 3000, H_-20 = 6000:
        //
        //   f(3000) = 0.0 * 0.5482390980715925 = 0.0
        //   f(4500) = 0.5 * 0.5482390980715925 = 0.27411954903579626
        //   f(6000) = 1.0 * 0.5482390980715925 = 0.5482390980715925
        //
        //   slabs: 0.0 * 750                   (bottom, clipped at H_0)
        //        + 0.27411954903579626 * 1500  (interior, (H_3 - H_1) / 2)
        //        + 0.5482390980715925 * 1500   (top, mirrored to a full layer)
        //        = 1233.5379706610834, SHI = 123.35379706610834
        //
        // A trapezoid would have given 822.36 and SHI 82.24 - a factor of
        // exactly 3/2 smaller, because f(6000) = 2 f(4500) and the endpoints
        // lose half a layer each. That is 23.03 mm of MESH instead of 28.21,
        // and POSH 63.6 instead of 75.3: 60 against 80 percent once the
        // operational rounding is applied. MESH and POSH are regressions fitted
        // to the paper's own index, so the smaller number is not merely a
        // different convention, it is the wrong input to a fixed calibration.
        let environment = standard_environment();
        let column = [
            sample(3_000.0, 60.0),
            sample(4_500.0, 60.0),
            sample(6_000.0, 60.0),
        ];
        let (shi, _) = severe_hail_index(&column, &environment);
        assert!(
            (shi - 123.353_8).abs() < 1e-3,
            "the slab sum gave {shi}, expected the paper's 123.3538"
        );

        let flux_middle = thermal_weight(4_500.0, &environment) * hail_kinetic_energy_flux(60.0);
        let flux_top = thermal_weight(6_000.0, &environment) * hail_kinetic_energy_flux(60.0);
        let trapezoid = SHI_COEFFICIENT
            * (0.5 * flux_middle * 1_500.0 + 0.5 * (flux_middle + flux_top) * 1_500.0);
        assert!(
            (shi / trapezoid - 1.5).abs() < 1e-4,
            "the slab sum should exceed a trapezoid by exactly 3/2 here, got {}",
            shi / trapezoid
        );
    }

    #[test]
    fn a_covered_no_echo_sample_contributes_zero_energy_without_breaking_the_integration() {
        // A beam that looked and found nothing is a measurement of zero, not a
        // gap: the layers either side of it are integrated, with zero flux at
        // the no-echo height.
        //
        //   slab at 3000 m: W_T = 0, so 0.0
        //   slab at 4500 m: flux 0.0 (the beam looked and found nothing)
        //   slab at 6000 m: [5250, 6750], depth 1500 (mirrored endpoint)
        //                   0.5482390980715926 * 1500 = 822.3586471073889
        //   SHI = 82.23586471073889
        //
        // The no-echo sample contributes nothing but does NOT split the run,
        // which is the difference between a measurement of zero and a gap.
        let column = [
            sample(3_000.0, 60.0),
            sample_with_state(4_500.0, CellState::NoEcho),
            sample(6_000.0, 60.0),
        ];
        let (shi, state) = severe_hail_index(&column, &standard_environment());
        assert!(
            (shi - 82.235_86).abs() < 1e-3,
            "SHI was {shi}, expected 82.2359"
        );
        // The top beam still echoes at 60 dBZ, so the storm continues above it.
        assert_eq!(state, CellState::LowerBound);
    }

    #[test]
    fn an_unusable_sample_above_the_freezing_level_makes_the_index_a_lower_bound() {
        // Range-folded data are integrated as zero energy so that hail is never
        // invented, which means the answer can only be too small - and it must
        // say so rather than pass for a measurement.
        let column = [
            sample(3_000.0, 30.0),
            sample_with_state(4_500.0, CellState::RangeFolded),
            sample(6_000.0, 30.0),
        ];
        let (shi, state) = severe_hail_index(&column, &standard_environment());
        assert_eq!(shi, 0.0);
        assert_eq!(state, CellState::LowerBound);
    }

    #[test]
    fn a_column_that_never_reaches_the_freezing_level_reports_no_coverage_not_zero() {
        // Close to the radar even the highest tilt can sit below the melting
        // level. An SHI of zero there would be a claim that there is no hail
        // above this point, and nothing looked.
        let column = [sample(500.0, 65.0), sample(1_500.0, 65.0)];
        let (shi, state) = severe_hail_index(&column, &standard_environment());
        assert_eq!(shi, 0.0);
        assert_eq!(state, CellState::NoCoverage);
    }

    #[test]
    fn an_empty_column_reports_no_coverage() {
        let (shi, state) = severe_hail_index(&[], &standard_environment());
        assert_eq!(shi, 0.0);
        assert_eq!(state, CellState::NoCoverage);
    }

    #[test]
    fn a_column_whose_highest_beam_still_echoes_reports_a_lower_bound() {
        let column = [
            sample(3_000.0, 50.0),
            sample(4_500.0, 60.0),
            sample(6_000.0, 55.0),
        ];
        let (shi, state) = severe_hail_index(&column, &standard_environment());
        assert!(shi > 0.0);
        assert_eq!(state, CellState::LowerBound);
    }

    // ---- Witt eq. (6): MESH -------------------------------------------------

    #[test]
    fn mesh_is_two_point_five_four_times_the_square_root_of_the_index() {
        // SHI = 100 J/m/s: 2.54 * sqrt(100) = 2.54 * 10 = 25.4 mm, which is an
        // inch of hail and a severe report.
        assert_eq!(mesh_mm(100.0), 25.4);
        // SHI = 144 J/m/s: 2.54 * 12 = 30.48 mm.
        let mesh = mesh_mm(144.0);
        assert!((mesh - 30.48).abs() < 1e-4, "MESH(144) was {mesh} mm");
    }

    #[test]
    fn mesh_of_a_zero_index_is_zero_millimetres() {
        assert_eq!(mesh_mm(0.0), 0.0);
    }

    #[test]
    fn mesh_of_a_realistic_index_is_a_hail_size_a_forecaster_would_recognise() {
        // 200 J/m/s is an ordinary strong-storm index: Witt et al. (1998)
        // Table 6, p. 293, reports an average SHI of 325 across 99 reports of
        // 19-33 mm hail and 1465 across 11 reports above 60 mm. So this is the
        // magnitude the whole chain has to be right at, not 1 and not 10000.
        //
        //   2.54 * sqrt(200) = 2.54 * 14.142135623730951 = 35.921024484276614
        //
        // which is 1.4142 in, because 2.54 / 25.4 is exactly a tenth and
        // sqrt(200) / 10 is exactly sqrt(2).
        let mesh = mesh_mm(200.0);
        // 1e-4 mm is 3e-6 relative, about 25 ulp of 35.92 in f32, which covers
        // the rounding of sqrt. Every failure mode that matters here is a
        // factor: reading eq. (6) as inches gives 1.41, a kilometre-stepped
        // SHI gives 1.14, and a slab quadrature gives about 44.
        assert!(
            (mesh - 35.921_024).abs() < 1e-4,
            "MESH(200) was {mesh} mm, expected 35.9210"
        );
        let inches = mesh / 25.4;
        assert!(
            (inches - std::f32::consts::SQRT_2).abs() < 1e-5,
            "MESH(200) was {inches} in, expected sqrt(2) = 1.4142"
        );
        // 35.9210 / 6.35 = 5.6569 steps, which rounds to 6: 38.1 mm, 1.50 in.
        assert_eq!(mesh_rounded_mm(200.0), 6.0 * MESH_ROUNDING_STEP_MM);
        let rounded = mesh_rounded_mm(200.0);
        assert!(
            (rounded - 38.1).abs() < 1e-4,
            "rounded MESH was {rounded} mm"
        );
    }

    #[test]
    fn mesh_rounds_to_the_nearest_quarter_inch_step() {
        // 30.48 mm is 4.8 steps of 6.35 mm, which rounds to 5 steps: 31.75 mm,
        // or 1.25 in.
        assert_eq!(mesh_rounded_mm(144.0), 5.0 * MESH_ROUNDING_STEP_MM);
        // The same value against the decimal an analyst reads. 6.35 is not
        // representable in binary, so five steps land about 5e-7 mm below
        // 31.75; the tolerance is that representation error and nothing more.
        let rounded = mesh_rounded_mm(144.0);
        assert!(
            (rounded - 31.75).abs() < 1e-4,
            "rounded MESH was {rounded} mm"
        );
        // 25.4 mm is exactly four steps and must not move.
        assert_eq!(mesh_rounded_mm(100.0), 25.4);
    }

    // ---- Witt eq. (4): the warning threshold --------------------------------

    #[test]
    fn the_warning_threshold_follows_the_freezing_level_linearly() {
        // 57.5 * 3.0 - 121 = 172.5 - 121 = 51.5
        let standard = warning_threshold(&standard_environment());
        assert!(
            (standard - 51.5).abs() < 1e-4,
            "WT for a 3 km freezing level was {standard}"
        );
        // 57.5 * 4.5 - 121 = 258.75 - 121 = 137.75
        let tropical = warning_threshold(&environment(4_500.0, 7_500.0));
        assert!(
            (tropical - 137.75).abs() < 1e-3,
            "WT for a 4.5 km freezing level was {tropical}"
        );
    }

    #[test]
    fn the_warning_threshold_never_falls_below_twenty_joules_per_metre_per_second() {
        // A 1.5 km freezing level gives 57.5 * 1.5 - 121 = -34.75, and a
        // negative threshold would make the POSH logarithm take the ratio of a
        // positive index to a negative number. The paper floors it at 20.
        assert_eq!(warning_threshold(&environment(1_500.0, 4_000.0)), 20.0);
        // 57.5 * 2.4 - 121 = 17.0, still under the floor.
        assert_eq!(warning_threshold(&environment(2_400.0, 5_000.0)), 20.0);
        // 57.5 * 2.5 - 121 = 22.75, above the floor and therefore unchanged.
        let mild = warning_threshold(&environment(2_500.0, 5_000.0));
        assert!(
            (mild - 22.75).abs() < 1e-4,
            "WT for a 2.5 km freezing level was {mild}"
        );
    }

    // ---- Witt eq. (5): POSH -------------------------------------------------

    #[test]
    fn posh_is_fifty_percent_when_the_index_equals_the_warning_threshold() {
        // ln(1) = 0, so POSH = 50 exactly. This is the anchor of the whole
        // regression: the threshold is by definition the coin flip.
        let environment = standard_environment();
        let threshold = warning_threshold(&environment);
        assert_eq!(posh_percent(threshold, &environment), 50.0);
    }

    #[test]
    fn posh_rises_by_twenty_nine_points_for_each_factor_of_e_above_the_threshold() {
        // 29 * ln(e) + 50 = 79.
        let environment = standard_environment();
        let threshold = warning_threshold(&environment);
        let posh = posh_percent(threshold * std::f32::consts::E, &environment);
        assert!(
            (posh - 79.0).abs() < 1e-3,
            "POSH one e-fold above the threshold was {posh}, expected 79"
        );
    }

    #[test]
    fn posh_clamps_at_zero_and_at_one_hundred_percent() {
        let environment = standard_environment();
        // 29 * ln(0.001 / 51.5) + 50 = -264, which is not a probability.
        assert_eq!(posh_percent(0.001, &environment), 0.0);
        // ln(0) is negative infinity; the clamp is what turns it into 0.
        assert_eq!(posh_percent(0.0, &environment), 0.0);
        // 29 * ln(100000 / 51.5) + 50 = 269.6.
        assert_eq!(posh_percent(100_000.0, &environment), 100.0);
    }

    #[test]
    fn posh_rounds_to_the_nearest_ten_percent() {
        let environment = standard_environment();
        let threshold = warning_threshold(&environment);
        // Exactly at the threshold POSH is 50 and stays 50.
        assert_eq!(posh_rounded_percent(threshold, &environment), 50.0);
        // One e-fold up is 79, which rounds to 80.
        assert_eq!(
            posh_rounded_percent(threshold * std::f32::consts::E, &environment),
            80.0
        );
        // 29 * ln(41.117932 / 51.5) + 50 = 43.49, which rounds to 40.
        let unrounded = posh_percent(41.117_93, &environment);
        assert!(
            (unrounded - 43.49).abs() < 0.05,
            "POSH was {unrounded}, expected about 43.49"
        );
        assert_eq!(posh_rounded_percent(41.117_93, &environment), 40.0);
    }

    // ---- POH ----------------------------------------------------------------

    #[test]
    fn every_probability_of_hail_table_knot_is_reproduced_exactly() {
        // Foote et al. (2005), Table 1. Each knot is checked at its own depth
        // above the freezing level, so a transcription slip in any one row
        // fails here rather than showing up as a plausible-looking field.
        let environment = standard_environment();
        let freezing_level_m = environment.freezing_level().metres();
        for (depth_km, percent) in POH_TABLE {
            let echo_top_m = freezing_level_m + depth_km * 1000.0;
            assert_eq!(
                probability_of_hail_percent(echo_top_m, &environment),
                percent,
                "the knot at {depth_km} km above the freezing level"
            );
        }
    }

    #[test]
    fn probability_of_hail_interpolates_linearly_between_knots() {
        // Halfway between the 2.40 km / 40 percent and 2.70 km / 50 percent
        // knots is 2.55 km, which must give 45 percent.
        let environment = standard_environment();
        let poh = probability_of_hail_percent(3_000.0 + 2_550.0, &environment);
        // The knots are decimal fractions that f32 cannot hold exactly, so the
        // interpolation fraction lands a few 1e-7 from one half; 1e-3 percent
        // is that error and nothing more.
        assert!(
            (poh - 45.0).abs() < 1e-3,
            "POH 2.55 km above the freezing level was {poh}, expected 45"
        );
    }

    #[test]
    fn the_probability_of_hail_curve_is_the_table_and_not_a_curve_fitted_to_it() {
        // Two published relations answer to "POH" and both were fitted to these
        // knots or to the same hailpad data, so either would render as a
        // plausible field. At 2.70 km above the freezing level the table reads
        // exactly 50 percent and they do not:
        //
        //   Foote et al. (2005), the cubic printed above their own Fig. 1:
        //     -1.20231 + 1.00184*2.70 - 0.17018*2.70^2 + 0.01086*2.70^3
        //     = -1.20231 + 2.704968 - 1.2406122 + 0.21375738
        //     = 0.47580318 = 47.580 percent
        //   Holleman (2001), the KNMI linear fit:
        //     0.319 + 0.133*2.70 = 0.6781 = 67.810 percent
        let environment = standard_environment();
        let poh = probability_of_hail_percent(3_000.0 + 2_700.0, &environment);
        assert_eq!(poh, 50.0);

        let depth_km = 2.70_f32;
        let foote_cubic_percent = 100.0
            * (-1.202_31 + 1.001_84 * depth_km - 0.170_18 * depth_km * depth_km
                + 0.010_86 * depth_km * depth_km * depth_km);
        let holleman_linear_percent = 100.0 * (0.319 + 0.133 * depth_km);
        // 1e-2 percent: these two are quoted to five decimal places in their
        // sources, so the only error here is f32 accumulation over four terms.
        assert!(
            (foote_cubic_percent - 47.580_3).abs() < 1e-2,
            "the Foote cubic gave {foote_cubic_percent} percent"
        );
        assert!(
            (holleman_linear_percent - 67.81).abs() < 1e-2,
            "the Holleman linear fit gave {holleman_linear_percent} percent"
        );
        // The cubic is off by 2.42 points, which is inside one reported 10
        // percent step and would therefore be nearly invisible; the linear fit
        // is off by 17.81, which is not. Neither is this curve.
        assert!((poh - foote_cubic_percent).abs() > 2.0);
        assert!((poh - holleman_linear_percent).abs() > 17.0);

        // Where the linear fit gives itself away: at the bottom knot, which is
        // the whole point of the table, it claims better than a coin flip.
        let holleman_at_the_first_knot = 100.0 * (0.319 + 0.133 * 1.65_f32);
        assert!(
            (holleman_at_the_first_knot - 53.845).abs() < 1e-2,
            "the linear fit gave {holleman_at_the_first_knot} percent at 1.65 km"
        );
        assert_eq!(
            probability_of_hail_percent(3_000.0 + 1_650.0, &environment),
            0.0
        );
    }

    #[test]
    fn probability_of_hail_is_zero_at_and_below_the_first_knot() {
        let environment = standard_environment();
        // Exactly 1.65 km above the freezing level.
        assert_eq!(probability_of_hail_percent(4_650.0, &environment), 0.0);
        // Well below it.
        assert_eq!(probability_of_hail_percent(3_500.0, &environment), 0.0);
        // A 45 dBZ echo top beneath the freezing level entirely: all warm rain.
        assert_eq!(probability_of_hail_percent(2_000.0, &environment), 0.0);
    }

    #[test]
    fn probability_of_hail_is_one_hundred_at_and_above_the_last_knot() {
        let environment = standard_environment();
        // Exactly 5.80 km above the freezing level.
        assert_eq!(probability_of_hail_percent(8_800.0, &environment), 100.0);
        // Higher still: the table does not run out, it saturates.
        assert_eq!(probability_of_hail_percent(15_000.0, &environment), 100.0);
    }

    #[test]
    fn probability_of_hail_depends_only_on_the_difference_so_the_datum_cancels() {
        // Raise both the echo top and the freezing level by the same 700 m and
        // the answer must not move. This is why an above-radar echo top may be
        // compared with an above-radar freezing level without either being
        // converted, and why mixing the two frames would be silently wrong.
        let low = environment(3_000.0, 6_000.0);
        let high = environment(3_700.0, 6_700.0);
        assert_eq!(
            probability_of_hail_percent(3_000.0 + 2_550.0, &low),
            probability_of_hail_percent(3_700.0 + 2_550.0, &high)
        );
    }
}
