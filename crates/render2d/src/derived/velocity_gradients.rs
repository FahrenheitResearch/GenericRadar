//! Linear Least Squares Derivative (LLSD) gradients of radial velocity:
//! azimuthal shear (rotation) and radial divergence.
//!
//! A mesocyclone is not visible in any single gate. It is visible as a
//! *difference* between neighbouring gates, and the naive way to measure that
//! difference - subtract two adjacent radials and divide by the arc between
//! them - is dominated by whichever of the two gates happened to be noisiest.
//! LLSD instead fits a plane to a whole neighbourhood of gates and reads the
//! two slopes off the fit, so a single bad gate moves the answer by roughly
//! 1/N rather than by all of it.
//!
//! The model is a weighted local plane in the radar's own polar frame:
//!
//! ```text
//! u = u_0 + u_r * dr + u_theta * dtheta
//! ```
//!
//! with `AzShear = u_theta` and `DivShear = u_r`, both in s^-1.
//!
//! Smith, T. M., and K. L. Elmore, 2004: "The Use of Radial Velocity
//! Derivatives to Diagnose Rotation and Divergence." 11th Conf. on Aviation,
//! Range, and Aerospace Meteorology, AMS, P5.6.
//!
//! Mahalik, M. C., B. R. Smith, K. L. Elmore, D. M. Kingfield, K. L. Ortega,
//! and T. M. Smith, 2019: "Estimates of Gradients in Radar Moments Using a
//! Linear Least Squares Derivative Technique." *Wea. Forecasting*, **34**,
//! 415-434, DOI 10.1175/WAF-D-18-0095.1.
//!
//! Smith, B. R., T. Sandmael, M. C. Mahalik, K. L. Elmore, D. M. Kingfield,
//! K. L. Ortega, and T. M. Smith, 2021: Corrigendum. *Wea. Forecasting*,
//! **36**, 1131-1133, DOI 10.1175/WAF-D-20-0125.1.
//!
//! # Both offsets are distances in metres
//!
//! `dtheta` is *not* an angle. Mahalik et al. (2019) state that "dtheta_ij
//! (dr_ij) is a distance in the azimuthal (radial) direction", and Smith and
//! Elmore (2004) define the azimuthal coordinate as the arc length s = r * phi.
//! Feeding an angle in radians (or worse, in degrees) into the azimuthal slot
//! produces a number that is not s^-1 at all, yet it still brightens over
//! storms and darkens over clear air, so it survives visual inspection
//! indefinitely.
//!
//! # Near range
//!
//! There is no published hard near-range mask, and inventing one - a 20 km
//! blanket cut-off, say - would erase exactly the close-range tornado
//! signatures the field exists to show. The documented operational controls are
//! the [`MAX_KERNEL_RADIALS`] cap and a reflectivity mask. Residual noise
//! inflation is acknowledged rather than removed: NWS/WDTD training notes
//! inflated values "within 5 km", and Mahalik et al. (2019) report "within
//! 5-10 km" for the legacy (pre-LLSD) difference equations. A caller that wants
//! to de-emphasise close range should do it with a confidence field built from
//! [`azimuthal_baseline_m`], not by deleting data.

/// Azimuthal (across-beam) kernel width for AzShear, in metres. The operational
/// MRMS value.
///
/// The kernel holds a constant width in *metres*, not a constant number of
/// gates, so it spans many radials at short range and few at long range. That
/// is the only way range enters the estimate; the weights themselves are
/// uniform and there is no range weighting anywhere in MRMS LLSD.
pub const MRMS_AZIMUTHAL_KERNEL_M: f32 = 2500.0;

/// Radial (along-beam) kernel depth for AzShear, in metres. The operational
/// MRMS value.
pub const MRMS_RADIAL_KERNEL_M: f32 = 750.0;

/// Azimuthal kernel width for DivShear, in metres.
///
/// Note that the divergence kernel is the *transpose* of the shear kernel:
/// rotation is measured across the beam and so wants a wide, shallow box, while
/// divergence is measured along the beam and so wants a narrow, deep one.
/// Swapping the two pairs is an easy edit to make and produces a field that is
/// merely blurred in the wrong direction rather than obviously broken.
pub const MRMS_DIVERGENCE_AZIMUTHAL_KERNEL_M: f32 = 750.0;

/// Radial kernel depth for DivShear, in metres.
pub const MRMS_DIVERGENCE_RADIAL_KERNEL_M: f32 = 1500.0;

// The two kernels are transposes of each other, and swapping a pair is a
// one-line edit that yields a field blurred in the wrong direction rather than
// an obviously broken one. Checked at compile time so the mistake cannot reach
// a test run at all.
const _: () = assert!(
    MRMS_AZIMUTHAL_KERNEL_M > MRMS_RADIAL_KERNEL_M,
    "rotation is measured across the beam, so its kernel must be wide and shallow"
);
const _: () = assert!(
    MRMS_DIVERGENCE_RADIAL_KERNEL_M > MRMS_DIVERGENCE_AZIMUTHAL_KERNEL_M,
    "divergence is measured along the beam, so its kernel must be narrow and deep"
);

/// The operational cap on how many radials one kernel may draw from.
///
/// A fixed-width-in-metres kernel spans `width / (range * step)` radials, which
/// diverges as range goes to zero: without a cap, a 2500 m kernel at 500 m
/// range would wrap most of the way around the radar and average an entire
/// storm into one "gradient". The cap binds for ranges below about 5.7 km on
/// 0.5-degree super-resolution data.
pub const MAX_KERNEL_RADIALS: usize = 51;

/// Three points define a plane. Two define a line, which leaves the third
/// coefficient completely unconstrained, and the solver would happily return
/// a value for it anyway.
const MIN_SAMPLES_FOR_A_PLANE: usize = 3;

/// How far the determinant may fall below its Hadamard bound before the
/// neighbourhood is declared degenerate.
///
/// The test is `det > TOL * ||row0|| * ||row1|| * ||row2||`. Hadamard's
/// inequality says that product is an upper bound on `|det|`, so the ratio
/// always sits in `[0, 1]` and can be compared against a fixed number at all.
/// That normalisation is the whole point: a raw `det > eps` test would reject
/// every real kernel, because entries like `sum(w * dtheta^2)` run to 1e9 when
/// the offsets are in metres, and comparing a determinant built from those
/// against a fixed epsilon compares a pure number against nothing.
///
/// Normalised is not the same as dimensionless. The ratio is invariant to
/// scaling the *rows* of the matrix, but a change of units is not a row scaling:
/// it scales a row and a column together, so the ratio moves. The lopsided
/// four-gate kernel in the tests below scores 39742 with both offsets in metres
/// and 5.0e6 with `dtheta` expressed in kilometres - a factor of 126 for the
/// same four gates. This is a shape measure for offsets in metres, which is what
/// this module requires everywhere; it is not an invariant, and a caller that
/// hands over kilometres gets a different verdict as well as a slope that is
/// wrong by a thousand.
///
/// 1e-10 and not tighter: f64 carries about 1e-16 of relative precision, and
/// summing a few thousand products inflates that to perhaps 1e-13 of the matrix
/// scale, so a truly rank-deficient kernel lands somewhere near 1e-13 rather
/// than at zero. 1e-10 clears that floor by three decades. 1e-10 and not
/// looser: a healthy centred rectangular kernel scores almost exactly 1.0, so
/// there are ten decades of margin before a usable neighbourhood is refused.
const SINGULARITY_RELATIVE_TOLERANCE: f64 = 1e-10;

/// One gate's contribution to a local gradient fit.
///
/// Both offsets are measured from the centre gate of the kernel and both are
/// distances in metres. Signs matter, and they matter asymmetrically:
///
/// - `azimuthal_offset_m` is positive in the direction of *increasing* azimuth,
///   that is clockwise from north. With that convention a positive
///   `azimuthal_shear_per_s` is cyclonic in the northern hemisphere. Flipping
///   the sign turns every mesocyclone anticyclonic while leaving the couplet
///   looking exactly as convincing as before.
/// - `radial_offset_m` is positive *away* from the radar, so a positive
///   `radial_divergence_per_s` is divergence and a negative one convergence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientSample {
    /// Radial velocity at this gate, in m s^-1. It must already be dealiased:
    /// LLSD is a linear fit, and one folded gate tilts the whole plane.
    pub velocity_mps: f32,
    /// Across-beam distance from the centre gate, in metres.
    pub azimuthal_offset_m: f32,
    /// Along-beam distance from the centre gate, in metres.
    pub radial_offset_m: f32,
    /// Relative weight. The operational default is uniform (`1.0`) for every
    /// gate in the kernel. The field exists so a caller can down-weight
    /// low-confidence gates, and so a gate can be excluded with `0.0` without
    /// rebuilding the slice.
    pub weight: f32,
}

/// The fitted plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearGradients {
    /// `u_0`: the fitted velocity at the centre gate, in m s^-1. This is not
    /// the measured centre gate value - it is the plane's value there, which is
    /// the point, since the measured one may be the noisy gate.
    pub intercept_mps: f32,
    /// `u_theta` = AzShear, in s^-1. Positive is cyclonic in the northern
    /// hemisphere.
    ///
    /// For an axisymmetric solid-body vortex sampled through its centre this is
    /// *half* the vertical vorticity, because a radar measures only one of the
    /// two terms that make up vorticity. Labelling it vorticity doubles every
    /// value on the display.
    pub azimuthal_shear_per_s: f32,
    /// `u_r` = DivShear, in s^-1. Positive is divergence, negative convergence.
    pub radial_divergence_per_s: f32,
    /// How many samples actually entered the fit. Not `samples.len()`: gates
    /// with a non-positive weight or a non-finite value are dropped, and a
    /// caller reporting coverage needs the number that survived.
    pub sample_count: usize,
    /// `hadamard_bound / |det|` for the normal-equation matrix, so 1.0 is a
    /// perfectly conditioned design and larger is worse.
    ///
    /// This is *not* the 2-norm condition number. It is the reciprocal of the
    /// Hadamard ratio, which is far cheaper than an eigenvalue decomposition,
    /// moves in the same direction, and is the same quantity the singularity
    /// test thresholds.
    ///
    /// It measures the *shape* of the neighbourhood and nothing else, and a
    /// confidence field built on it alone will wave through exactly the
    /// geometries that produce the worst artefacts:
    ///
    /// - It does not notice how few radials the kernel reached. A balanced
    ///   kernel spanning only two radials - the ordinary case beyond about
    ///   200 km, where one radial is wider than the whole 2500 m kernel - has
    ///   mutually orthogonal design columns and scores exactly 1.0, the best
    ///   score there is, while its shear is a two-point difference with no
    ///   redundancy at all.
    /// - It does not notice how short the azimuthal baseline was. It is blind to
    ///   a uniform scaling of the offsets, so two radials 1745 m apart and two
    ///   radials 1 mm apart both score 1.0, and the second returns thousands of
    ///   s^-1 from a single bad gate.
    ///
    /// Use it to reject *lopsided* neighbourhoods, the sector edges and
    /// half-blocked kernels where the fit leans on one corner of the data. For
    /// the two failures above the caller must count distinct radials itself and
    /// consult [`azimuthal_baseline_m`]; neither number is recoverable from this
    /// one.
    pub condition_estimate: f64,
}

/// Fit the local plane and return its two slopes.
///
/// Returns `None` when the neighbourhood cannot determine a plane: fewer than
/// three usable samples, or a degenerate geometry (every sample at one offset,
/// every sample strung along one line, or every sample on a single radial).
/// `None` is the honest answer there. Returning a zero shear instead would
/// paint a calm, confident, entirely fictional field over exactly the region
/// where the geometry failed.
///
/// The technique is that of Smith and Elmore (2004) and Mahalik et al. (2019),
/// but the arithmetic below is deliberately *not* taken from either paper.
///
/// **DO NOT TRANSCRIBE ANY PUBLISHED EXPANDED FORM, FROM EITHER PAPER.** Solve
/// the standard symmetric 3x3 normal-equation system directly. It is
/// algebraically identical to the corrected result and immune to every
/// transcription error in both papers. Mahalik et al. (2019) published a
/// coefficient matrix (their eq. 8) with all three cross-diagonal pairs
/// transposed, and an adjugate (their eq. 10) that was only partially
/// transposed, so their final AzShear and DivShear equations (eqs. 12a and 12b)
/// and the fully expanded forms in their appendixes A and B are wrong; the
/// corrected determinant is the exact negative of the published one (Smith et
/// al. 2021, corrigendum). Anyone who copies appendix A or appendix B verbatim
/// ships a wrong implementation that still produces plausible pictures.
pub fn fit_linear_gradients(samples: &[GradientSample]) -> Option<LinearGradients> {
    if samples.len() < MIN_SAMPLES_FOR_A_PLANE {
        return None;
    }
    let mut equations = NormalEquations::default();
    for sample in samples {
        equations.accumulate(sample);
    }
    if equations.count < MIN_SAMPLES_FOR_A_PLANE {
        return None;
    }
    equations.solve()
}

/// The approximate tangential baseline across one radial on each side of the
/// centre, in metres: `2 * range * azimuth_step_in_radians`.
///
/// This is the number that collapses near the radar, and it is why close-range
/// LLSD is noisy no matter how carefully the fit is done. At 5 km on 0.5-degree
/// data the baseline is about 87 m, so a single gate in error by 40 m s^-1 -
/// one missed fold - shows up as about 0.46 s^-1 of azimuthal shear, far above
/// any real mesocyclone. At 100 km the same baseline is about 1745 m and the
/// same error is about 0.023 s^-1, twenty times smaller.
///
/// The estimator itself does not use this; it is for a caller building a
/// confidence field or choosing how many radials to gather.
pub fn azimuthal_baseline_m(range_m: f32, azimuth_step_deg: f32) -> f32 {
    2.0 * range_m * azimuth_step_deg.to_radians()
}

/// The accumulated weighted sums of the normal equations `A x = b`, with
///
/// ```text
///      | sum_w    sum_wr   sum_wt  |        | sum_wu  |
///  A = | sum_wr   sum_wrr  sum_wrt |    b = | sum_wru |
///      | sum_wt   sum_wrt  sum_wtt |        | sum_wtu |
/// ```
///
/// and `x = (u_0, u_r, u_theta)`. Everything is f64 even though the inputs are
/// f32: `sum(w * dtheta^2)` over a full kernel reaches 1e9 with offsets in
/// metres, and f32 has only about seven significant digits, so accumulating
/// there would lose the small residual differences the slopes are made of.
#[derive(Clone, Copy, Debug, Default)]
struct NormalEquations {
    sum_w: f64,
    sum_wr: f64,
    sum_wt: f64,
    sum_wrr: f64,
    sum_wrt: f64,
    sum_wtt: f64,
    sum_wu: f64,
    sum_wru: f64,
    sum_wtu: f64,
    count: usize,
}

impl NormalEquations {
    /// Add one gate, or silently skip it if it cannot contribute.
    ///
    /// A single NaN velocity - which a failed dealiasing pass can produce -
    /// would otherwise propagate into every sum and make the whole
    /// neighbourhood NaN, and a NaN shear renders as a hole rather than as an
    /// error anyone notices. A negative weight is always a caller bug: it would
    /// let one gate cancel another and quietly halve the effective sample count
    /// with no diagnostic at all.
    fn accumulate(&mut self, sample: &GradientSample) {
        let weight = f64::from(sample.weight);
        let dr = f64::from(sample.radial_offset_m);
        let dtheta = f64::from(sample.azimuthal_offset_m);
        let velocity = f64::from(sample.velocity_mps);

        let usable = weight.is_finite()
            && weight > 0.0
            && dr.is_finite()
            && dtheta.is_finite()
            && velocity.is_finite();
        if !usable {
            return;
        }

        self.sum_w += weight;
        self.sum_wr += weight * dr;
        self.sum_wt += weight * dtheta;
        self.sum_wrr += weight * dr * dr;
        self.sum_wrt += weight * dr * dtheta;
        self.sum_wtt += weight * dtheta * dtheta;
        self.sum_wu += weight * velocity;
        self.sum_wru += weight * dr * velocity;
        self.sum_wtu += weight * dtheta * velocity;
        self.count += 1;
    }

    /// Invert the symmetric 3x3 system by cofactors.
    ///
    /// Cofactors rather than a library solve because the system is three by
    /// three and symmetric: the closed form is a dozen multiplications, it has
    /// no pivoting to get wrong, and it hands over the determinant for the
    /// conditioning test for free. See the warning on [`fit_linear_gradients`]
    /// about published expanded forms - this is derived from the plain normal
    /// equations, not copied from anywhere.
    fn solve(&self) -> Option<LinearGradients> {
        // Cofactors of the symmetric matrix A. Because A is symmetric its
        // cofactor matrix is symmetric too, so the adjugate is the cofactor
        // matrix itself and there is no transpose step left to get wrong. That
        // partially-applied transpose is precisely what the 2019 paper got
        // wrong and the 2021 corrigendum fixed.
        let cofactor_00 = self.sum_wrr * self.sum_wtt - self.sum_wrt * self.sum_wrt;
        let cofactor_01 = self.sum_wt * self.sum_wrt - self.sum_wr * self.sum_wtt;
        let cofactor_02 = self.sum_wr * self.sum_wrt - self.sum_wrr * self.sum_wt;
        let cofactor_11 = self.sum_w * self.sum_wtt - self.sum_wt * self.sum_wt;
        let cofactor_12 = self.sum_wr * self.sum_wt - self.sum_w * self.sum_wrt;
        let cofactor_22 = self.sum_w * self.sum_wrr - self.sum_wr * self.sum_wr;

        let determinant =
            self.sum_w * cofactor_00 + self.sum_wr * cofactor_01 + self.sum_wt * cofactor_02;

        let row_0 =
            (self.sum_w * self.sum_w + self.sum_wr * self.sum_wr + self.sum_wt * self.sum_wt)
                .sqrt();
        let row_1 =
            (self.sum_wr * self.sum_wr + self.sum_wrr * self.sum_wrr + self.sum_wrt * self.sum_wrt)
                .sqrt();
        let row_2 =
            (self.sum_wt * self.sum_wt + self.sum_wrt * self.sum_wrt + self.sum_wtt * self.sum_wtt)
                .sqrt();
        let hadamard_bound = row_0 * row_1 * row_2;

        // Checked before the comparison, because every comparison against a
        // NaN determinant is false and the degenerate case would sail through
        // a bare `<=` test.
        if !determinant.is_finite() || !hadamard_bound.is_finite() {
            return None;
        }
        // The signed determinant, not its magnitude. A is
        // `sum_i w_i * v_i * v_i^T` with `v_i = (1, dr_i, dtheta_i)` and every
        // `w_i > 0`, so A is a Gram matrix: it is positive semi-definite and its
        // determinant cannot be negative, reaching zero exactly when the
        // geometry is rank deficient. A computed determinant below zero is
        // therefore not a geometry at all, it is proof that cancellation ate the
        // answer. Testing `|det|` instead would return that noise as a fit, and
        // `condition_estimate` divides by `|det|` too, so it would come back
        // wearing a respectable number.
        if determinant <= SINGULARITY_RELATIVE_TOLERANCE * hadamard_bound {
            return None;
        }

        // x = adj(A) b / det, written out. The adjugate is symmetric, so the
        // same six cofactors serve all three rows.
        let intercept =
            (cofactor_00 * self.sum_wu + cofactor_01 * self.sum_wru + cofactor_02 * self.sum_wtu)
                / determinant;
        let radial_slope =
            (cofactor_01 * self.sum_wu + cofactor_11 * self.sum_wru + cofactor_12 * self.sum_wtu)
                / determinant;
        let azimuthal_slope =
            (cofactor_02 * self.sum_wu + cofactor_12 * self.sum_wru + cofactor_22 * self.sum_wtu)
                / determinant;

        if !intercept.is_finite() || !radial_slope.is_finite() || !azimuthal_slope.is_finite() {
            return None;
        }

        Some(LinearGradients {
            intercept_mps: intercept as f32,
            azimuthal_shear_per_s: azimuthal_slope as f32,
            radial_divergence_per_s: radial_slope as f32,
            sample_count: self.count,
            // No `abs()`: the guard above already refused everything at or
            // below zero, so this is a positive number over a positive number.
            condition_estimate: hadamard_bound / determinant,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three gates deep by seven radials wide: 250 m gate spacing fills the
    /// 750 m radial kernel, and 350 m radial spacing (0.5 degrees at about
    /// 40 km) fills 2100 m of the 2500 m azimuthal kernel.
    const RADIAL_OFFSETS_M: [f32; 3] = [-250.0, 0.0, 250.0];
    const AZIMUTHAL_OFFSETS_M: [f32; 7] = [-1050.0, -700.0, -350.0, 0.0, 350.0, 700.0, 1050.0];

    /// The two slopes used wherever a test wants an exactly representable
    /// plane: 2^-8 and 2^-7. See
    /// [`an_exactly_planar_velocity_field_is_recovered_to_twelve_decimal_places`]
    /// for why exact binary fractions matter.
    const EXACT_RADIAL_SLOPE: f32 = 0.003_906_25;
    const EXACT_AZIMUTHAL_SLOPE: f32 = 0.007_812_5;

    fn planar_kernel(
        intercept: f32,
        radial_slope: f32,
        azimuthal_slope: f32,
    ) -> Vec<GradientSample> {
        let mut samples = Vec::with_capacity(RADIAL_OFFSETS_M.len() * AZIMUTHAL_OFFSETS_M.len());
        for radial_offset_m in RADIAL_OFFSETS_M {
            for azimuthal_offset_m in AZIMUTHAL_OFFSETS_M {
                samples.push(GradientSample {
                    velocity_mps: intercept
                        + radial_slope * radial_offset_m
                        + azimuthal_slope * azimuthal_offset_m,
                    azimuthal_offset_m,
                    radial_offset_m,
                    weight: 1.0,
                });
            }
        }
        samples
    }

    /// The coefficients are exact binary fractions (2^-8 and 2^-7) and the
    /// offsets are small multiples of 2, so every sample velocity is exactly
    /// representable in f32. That removes storage rounding from the test and
    /// leaves only f64 roundoff in the normal equations, which is why the
    /// tolerance can be 1e-12 rather than the roughly 1e-9 that f32-rounded
    /// inputs would force. A tolerance that loose would still catch a sign
    /// error, but not a one-part-in-a-million bias from a mis-ordered cofactor.
    #[test]
    fn an_exactly_planar_velocity_field_is_recovered_to_twelve_decimal_places() {
        let samples = planar_kernel(12.0, EXACT_RADIAL_SLOPE, EXACT_AZIMUTHAL_SLOPE);
        let fit = fit_linear_gradients(&samples).expect("a full rectangular kernel is solvable");

        // The intercept is an f32 near 12, so it carries about 1e-6 of storage
        // rounding on its own and cannot be pinned as tightly as the slopes.
        assert!(
            (f64::from(fit.intercept_mps) - 12.0).abs() < 1e-6,
            "intercept was {}",
            fit.intercept_mps
        );
        assert!(
            (f64::from(fit.radial_divergence_per_s) - f64::from(EXACT_RADIAL_SLOPE)).abs() < 1e-12,
            "divergence was {}",
            fit.radial_divergence_per_s
        );
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - f64::from(EXACT_AZIMUTHAL_SLOPE)).abs() < 1e-12,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
        assert_eq!(fit.sample_count, 21);
    }

    /// The physical sanity number an operator can check by eye: a couplet of
    /// +20 and -20 m s^-1 separated by the 2500 m operational kernel is
    /// 40 / 2500 = 0.016 s^-1, a violently rotating mesocyclone.
    ///
    /// Tolerance 1e-9 and not tighter because 0.016 is not a binary fraction,
    /// so the sample velocities carry f32 storage rounding of about 2e-6 m s^-1
    /// spread over a 2500 m baseline, which is an irreducible floor near
    /// 1e-9 s^-1.
    #[test]
    fn a_forty_metre_per_second_couplet_across_the_operational_kernel_is_sixteen_thousandths_per_second()
     {
        let half_kernel = MRMS_AZIMUTHAL_KERNEL_M / 2.0;
        let expected = 40.0 / f64::from(MRMS_AZIMUTHAL_KERNEL_M);
        let samples = vec![
            GradientSample {
                velocity_mps: -20.0,
                azimuthal_offset_m: -half_kernel,
                radial_offset_m: 0.0,
                weight: 1.0,
            },
            GradientSample {
                velocity_mps: 0.0,
                azimuthal_offset_m: 0.0,
                radial_offset_m: -250.0,
                weight: 1.0,
            },
            GradientSample {
                velocity_mps: 0.0,
                azimuthal_offset_m: 0.0,
                radial_offset_m: 250.0,
                weight: 1.0,
            },
            GradientSample {
                velocity_mps: 20.0,
                azimuthal_offset_m: half_kernel,
                radial_offset_m: 0.0,
                weight: 1.0,
            },
        ];
        let fit = fit_linear_gradients(&samples).expect("four non-collinear gates fit a plane");
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - expected).abs() < 1e-9,
            "expected {expected} s^-1, got {} s^-1",
            fit.azimuthal_shear_per_s
        );
        assert!(
            (expected - 0.016).abs() < 1e-12,
            "the worked number itself changed: {expected}"
        );
    }

    /// Storm motion, a mis-set Nyquist offset, and a whole-sweep bias all add a
    /// constant to every gate in a small neighbourhood. A derivative estimator
    /// that notices is measuring the wrong thing.
    #[test]
    fn adding_a_constant_to_every_velocity_moves_only_the_intercept() {
        let base = planar_kernel(0.0, EXACT_RADIAL_SLOPE, EXACT_AZIMUTHAL_SLOPE);
        let shifted: Vec<GradientSample> = base
            .iter()
            .map(|sample| GradientSample {
                velocity_mps: sample.velocity_mps + 13.5,
                ..*sample
            })
            .collect();

        let before = fit_linear_gradients(&base).expect("solvable");
        let after = fit_linear_gradients(&shifted).expect("solvable");

        assert!(
            (f64::from(after.intercept_mps) - f64::from(before.intercept_mps) - 13.5).abs() < 1e-6,
            "intercept moved from {} to {}, not by 13.5",
            before.intercept_mps,
            after.intercept_mps
        );
        // 1e-12 rather than exact equality: the sums differ, so the two solves
        // take different roundoff paths even though the answer is analytically
        // identical.
        assert!(
            (f64::from(after.azimuthal_shear_per_s) - f64::from(before.azimuthal_shear_per_s))
                .abs()
                < 1e-12,
            "shear changed from {} to {}",
            before.azimuthal_shear_per_s,
            after.azimuthal_shear_per_s
        );
        assert!(
            (f64::from(after.radial_divergence_per_s) - f64::from(before.radial_divergence_per_s))
                .abs()
                < 1e-12,
            "divergence changed from {} to {}",
            before.radial_divergence_per_s,
            after.radial_divergence_per_s
        );
    }

    /// Cross-talk between the two slopes is the failure that makes a strong
    /// mesocyclone paint a matching divergence signature on top of itself,
    /// which reads as a rotating downburst that is not there.
    #[test]
    fn a_pure_azimuthal_ramp_has_no_radial_divergence() {
        let samples = planar_kernel(3.0, 0.0, EXACT_AZIMUTHAL_SLOPE);
        let fit = fit_linear_gradients(&samples).expect("solvable");
        assert!(
            f64::from(fit.radial_divergence_per_s).abs() < 1e-12,
            "a pure rotation leaked {} s^-1 of divergence",
            fit.radial_divergence_per_s
        );
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - f64::from(EXACT_AZIMUTHAL_SLOPE)).abs() < 1e-12,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
    }

    #[test]
    fn a_pure_radial_ramp_has_no_azimuthal_shear() {
        let samples = planar_kernel(3.0, EXACT_RADIAL_SLOPE, 0.0);
        let fit = fit_linear_gradients(&samples).expect("solvable");
        assert!(
            f64::from(fit.azimuthal_shear_per_s).abs() < 1e-12,
            "a pure divergence leaked {} s^-1 of shear",
            fit.azimuthal_shear_per_s
        );
        assert!(
            (f64::from(fit.radial_divergence_per_s) - f64::from(EXACT_RADIAL_SLOPE)).abs() < 1e-12,
            "divergence was {}",
            fit.radial_divergence_per_s
        );
    }

    /// One gate on the exactly planar field, for the small hand-built kernels
    /// that need specific offsets rather than the full rectangle.
    fn planar_gate(radial_offset_m: f32, azimuthal_offset_m: f32) -> GradientSample {
        GradientSample {
            velocity_mps: EXACT_RADIAL_SLOPE * radial_offset_m
                + EXACT_AZIMUTHAL_SLOPE * azimuthal_offset_m,
            azimuthal_offset_m,
            radial_offset_m,
            weight: 1.0,
        }
    }

    /// A table of `(velocity m s^-1, azimuthal offset m, radial offset m,
    /// weight)` rows, so a hand-built kernel reads as the grid of numbers it is
    /// rather than as four screens of struct literals.
    fn gates(rows: &[(f32, f32, f32, f32)]) -> Vec<GradientSample> {
        rows.iter()
            .map(
                |&(velocity_mps, azimuthal_offset_m, radial_offset_m, weight)| GradientSample {
                    velocity_mps,
                    azimuthal_offset_m,
                    radial_offset_m,
                    weight,
                },
            )
            .collect()
    }

    #[test]
    fn fewer_than_three_samples_cannot_define_a_plane() {
        // A triangle rather than a prefix of the rectangular kernel: the first
        // three gates of that kernel all share one radial offset and are
        // legitimately singular, so slicing it would test the wrong rule.
        let triangle = [
            planar_gate(0.0, 0.0),
            planar_gate(250.0, 0.0),
            planar_gate(0.0, 350.0),
        ];
        assert_eq!(fit_linear_gradients(&[]), None);
        assert_eq!(fit_linear_gradients(&triangle[..1]), None);
        assert_eq!(fit_linear_gradients(&triangle[..2]), None);
        assert!(
            fit_linear_gradients(&triangle).is_some(),
            "three non-collinear samples are exactly enough"
        );
    }

    /// The complement of the single-radial case: a kernel that reached only one
    /// range ring. Every gate shares a radial offset, so the second column of
    /// the design is a multiple of the first and the along-beam slope is
    /// unconstrained. This is the case that a naive "take the first three
    /// gates" fixture walks straight into.
    #[test]
    fn a_kernel_confined_to_one_range_ring_returns_none_rather_than_a_zero_divergence() {
        let samples: Vec<GradientSample> = AZIMUTHAL_OFFSETS_M
            .into_iter()
            .map(|azimuthal_offset_m| planar_gate(250.0, azimuthal_offset_m))
            .collect();
        assert_eq!(samples.len(), 7);
        assert_eq!(fit_linear_gradients(&samples), None);
    }

    #[test]
    fn samples_that_all_share_one_offset_are_singular_and_return_none() {
        // Twenty gates stacked at the same place say nothing about any
        // gradient, however many of them there are.
        let samples: Vec<GradientSample> = [10.0_f32, 11.0, 12.0, 13.0, 14.0]
            .into_iter()
            .map(|velocity_mps| GradientSample {
                velocity_mps,
                azimuthal_offset_m: 0.0,
                radial_offset_m: 0.0,
                weight: 1.0,
            })
            .collect();
        assert_eq!(fit_linear_gradients(&samples), None);
    }

    #[test]
    fn samples_strung_along_one_line_are_singular_and_return_none() {
        // dtheta = 2 * dr for every sample, so the third column of the design
        // is exactly twice the second and the plane's tilt across that line is
        // unconstrained. Without the determinant test the solver would return
        // whichever of infinitely many planes roundoff happened to pick, and it
        // would look like a perfectly ordinary shear value.
        let samples: Vec<GradientSample> = [-500.0_f32, -250.0, 0.0, 250.0, 500.0]
            .into_iter()
            .map(|radial_offset_m| GradientSample {
                velocity_mps: 0.01 * radial_offset_m,
                azimuthal_offset_m: 2.0 * radial_offset_m,
                radial_offset_m,
                weight: 1.0,
            })
            .collect();
        assert_eq!(fit_linear_gradients(&samples), None);
    }

    /// At long range a fixed-width kernel spans very few radials - at 250 km a
    /// 0.5-degree radial is about 2182 m across, so a 2500 m kernel can catch
    /// as little as one. One radial carries no azimuthal information at all,
    /// and answering 0.0 there would paint a calm band across the far edge of
    /// every sweep.
    #[test]
    fn a_kernel_that_caught_only_one_radial_returns_none_rather_than_a_zero_shear() {
        let samples: Vec<GradientSample> = [-500.0_f32, -250.0, 0.0, 250.0, 500.0]
            .into_iter()
            .map(|radial_offset_m| GradientSample {
                velocity_mps: 20.0 + 0.004 * radial_offset_m,
                azimuthal_offset_m: 0.0,
                radial_offset_m,
                weight: 1.0,
            })
            .collect();
        assert_eq!(fit_linear_gradients(&samples), None);
    }

    #[test]
    fn a_full_rectangular_kernel_is_almost_perfectly_conditioned() {
        // A centred rectangular kernel makes the three columns of the design
        // mutually orthogonal, so the determinant meets its Hadamard bound
        // exactly and the estimate is 1.0.
        let fit = fit_linear_gradients(&planar_kernel(
            0.0,
            EXACT_RADIAL_SLOPE,
            EXACT_AZIMUTHAL_SLOPE,
        ))
        .expect("solvable");
        assert!(
            (fit.condition_estimate - 1.0).abs() < 1e-9,
            "condition estimate was {}",
            fit.condition_estimate
        );
    }

    #[test]
    fn a_lopsided_kernel_is_still_solvable_but_reports_worse_conditioning() {
        // Every sample on one side of the centre gate, as happens at the edge
        // of a sector of valid data. Solvable, but the analyst should be able
        // to tell it apart from a clean fit, which is what the condition
        // estimate is for.
        let samples = [
            planar_gate(0.0, 0.0),
            planar_gate(250.0, 350.0),
            planar_gate(500.0, 700.0),
            planar_gate(250.0, 700.0),
        ];
        let fit = fit_linear_gradients(&samples).expect("four non-collinear gates fit a plane");
        assert!(
            fit.condition_estimate > 10.0,
            "an all-on-one-side kernel should score poorly, got {}",
            fit.condition_estimate
        );
        // It is still the right plane, which is why this must not be rejected.
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - f64::from(EXACT_AZIMUTHAL_SLOPE)).abs() < 1e-9,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
    }

    #[test]
    fn a_non_finite_velocity_is_dropped_rather_than_poisoning_the_whole_fit() {
        let mut samples = planar_kernel(12.0, EXACT_RADIAL_SLOPE, EXACT_AZIMUTHAL_SLOPE);
        samples[5].velocity_mps = f32::NAN;
        let fit = fit_linear_gradients(&samples).expect("twenty good gates still fit a plane");
        assert_eq!(fit.sample_count, 20, "the NaN gate must not be counted");
        // Dropping one gate makes the kernel asymmetric, so the fit is no
        // longer exact to f64 roundoff; 1e-9 is the level at which a genuine
        // bias would show while asymmetry alone does not.
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - f64::from(EXACT_AZIMUTHAL_SLOPE)).abs() < 1e-9,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
    }

    #[test]
    fn a_zero_weight_gate_neither_enters_the_fit_nor_the_sample_count() {
        let mut samples = planar_kernel(12.0, EXACT_RADIAL_SLOPE, EXACT_AZIMUTHAL_SLOPE);
        // A wildly wrong velocity that would drag the plane if it counted.
        samples[9].velocity_mps = 900.0;
        samples[9].weight = 0.0;
        let fit = fit_linear_gradients(&samples).expect("solvable");
        assert_eq!(fit.sample_count, 20);
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - f64::from(EXACT_AZIMUTHAL_SLOPE)).abs() < 1e-9,
            "a zero-weight gate moved the shear to {}",
            fit.azimuthal_shear_per_s
        );
    }

    #[test]
    fn a_negative_weight_is_refused_rather_than_subtracting_a_gate() {
        let mut samples = planar_kernel(12.0, EXACT_RADIAL_SLOPE, EXACT_AZIMUTHAL_SLOPE);
        samples[9].weight = -1.0;
        let fit = fit_linear_gradients(&samples).expect("solvable");
        assert_eq!(
            fit.sample_count, 20,
            "a negative weight must drop the gate, not subtract it"
        );
    }

    /// The worked numbers from the doc comment, pinned so that a change to the
    /// formula has to argue with them.
    #[test]
    fn the_azimuthal_baseline_is_about_eighty_seven_metres_at_five_kilometres() {
        let baseline = azimuthal_baseline_m(5_000.0, 0.5);
        assert!(
            (baseline - 87.266_46).abs() < 0.001,
            "baseline at 5 km was {baseline} m"
        );
        // One missed fold, 40 m s^-1, spread over that baseline.
        let spurious_shear = 40.0 / baseline;
        assert!(
            (spurious_shear - 0.458_366).abs() < 1e-5,
            "a 40 m/s error at 5 km looked like {spurious_shear} s^-1"
        );
    }

    #[test]
    fn the_azimuthal_baseline_at_one_hundred_kilometres_is_twenty_times_the_five_kilometre_one() {
        let near = azimuthal_baseline_m(5_000.0, 0.5);
        let far = azimuthal_baseline_m(100_000.0, 0.5);
        assert!(
            (far - 1_745.329_3).abs() < 0.01,
            "baseline at 100 km was {far} m"
        );
        // Exactly linear in range, so the ratio is exactly the range ratio.
        assert!(
            (far / near - 20.0).abs() < 1e-4,
            "the near-range collapse factor was {}",
            far / near
        );
        // The same 40 m/s error is twenty times smaller out here, which is why
        // close range needs a confidence field rather than a blanket mask.
        assert!(
            (40.0 / far - 0.022_918).abs() < 1e-5,
            "a 40 m/s error at 100 km looked like {} s^-1",
            40.0 / far
        );
    }

    #[test]
    fn the_baseline_vanishes_at_the_radar_itself() {
        assert_eq!(azimuthal_baseline_m(0.0, 0.5), 0.0);
    }

    /// The operational MRMS kernel sizes, pinned to the exact metre values. The
    /// wide-versus-deep ordering is enforced at compile time beside the
    /// constants themselves; this pins the numbers so that a change to any one
    /// of them has to be deliberate.
    #[test]
    fn the_operational_kernel_sizes_are_the_mrms_values_in_metres() {
        assert_eq!(MRMS_AZIMUTHAL_KERNEL_M, 2500.0);
        assert_eq!(MRMS_RADIAL_KERNEL_M, 750.0);
        assert_eq!(MRMS_DIVERGENCE_AZIMUTHAL_KERNEL_M, 750.0);
        assert_eq!(MRMS_DIVERGENCE_RADIAL_KERNEL_M, 1500.0);
    }

    #[test]
    fn the_fifty_one_radial_cap_binds_inside_about_six_kilometres() {
        // A 2500 m kernel spans 2500 / (range * step) radial spacings. The cap
        // of 51 radials allows 50 spacings, which is reached near 5.7 km.
        let step_rad = 0.5_f32.to_radians();
        let spacings_at = |range_m: f32| MRMS_AZIMUTHAL_KERNEL_M / (range_m * step_rad);
        assert_eq!(MAX_KERNEL_RADIALS, 51);
        assert!(
            spacings_at(5_000.0) > 50.0,
            "at 5 km the kernel wants {} spacings, so the cap must bind",
            spacings_at(5_000.0)
        );
        assert!(
            spacings_at(6_000.0) < 50.0,
            "at 6 km the kernel wants {} spacings, so the cap is slack",
            spacings_at(6_000.0)
        );
    }

    /// The one test here whose expected answer was computed outside this file,
    /// by a different method, in exact arithmetic.
    ///
    /// Every other recovery test plants a plane, and a planted plane is a weak
    /// witness. The fit is then exact, so it is reproduced by any rearrangement
    /// of the cofactors that still inverts the matrix, and a centred rectangular
    /// kernel drives `sum_wr`, `sum_wt` and `sum_wrt` to zero, which deletes
    /// most of the terms a wrong rearrangement would get wrong. This kernel
    /// zeroes nothing: it is lopsided, unevenly spaced in both directions,
    /// unevenly weighted, and its velocities are deliberately *not* planar, so
    /// the answer is a real projection that only the true normal-equation
    /// solution reaches.
    ///
    /// The reference was obtained by rebuilding the same 3x3 system with Python
    /// `fractions.Fraction` and solving it by Cramer's rule - exact rational
    /// arithmetic, no rounding at any step:
    ///
    /// ```text
    /// u_0     = -3.137540368030649
    /// u_r     =  0.0016911274275294493
    /// u_theta =  0.0009535846266455497
    /// ```
    ///
    /// Every velocity, offset and weight below is an integer or a multiple of
    /// 0.25, so all are exactly representable in f32 and the reference solved
    /// precisely the numbers the fit sees.
    ///
    /// This is the check that catches a transcription of Mahalik et al. (2019)
    /// appendix A or B, corrected by Smith et al. (2021): their determinant is
    /// the exact negative of the correct one and their adjugate is only
    /// partially transposed, and neither error is visible on a symmetric kernel
    /// carrying a planted plane.
    #[test]
    fn a_lopsided_unevenly_weighted_nonplanar_kernel_matches_an_exact_rational_solution() {
        let samples = gates(&[
            // velocity, dtheta, dr, weight
            (-18.50, -1000.0, -750.0, 1.00),
            (-7.25, -500.0, -250.0, 0.50),
            (3.00, 250.0, -250.0, 2.00),
            (1.50, 0.0, 0.0, 1.00),
            (11.75, 875.0, 125.0, 0.25),
            (-4.50, -250.0, 500.0, 4.00),
            (9.25, 625.0, 500.0, 1.00),
            (21.00, 125.0, 1000.0, 0.50),
            (-13.50, 1500.0, 1000.0, 1.50),
        ]);
        let fit = fit_linear_gradients(&samples).expect("nine scattered gates determine a plane");
        assert_eq!(fit.sample_count, 9);

        // The tolerances are one f32 unit in the last place of each answer,
        // rounded up: about 2.4e-7 near 3.14, 1.2e-10 near 0.0017, and 5.8e-11
        // near 0.00095. Nothing tighter is testable, because the results are
        // stored as f32. Nothing looser is needed: the smallest structural
        // error either paper's appendix would introduce changes these numbers
        // in their first digit, not their eighth.
        assert!(
            (f64::from(fit.intercept_mps) + 3.137_540_368_030_649).abs() < 5e-7,
            "intercept was {}",
            fit.intercept_mps
        );
        assert!(
            (f64::from(fit.radial_divergence_per_s) - 0.001_691_127_427_529_449_3).abs() < 3e-10,
            "divergence was {}",
            fit.radial_divergence_per_s
        );
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - 0.000_953_584_626_645_549_7).abs() < 2e-10,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
    }

    /// Four gates on the plane `u = 0.02 * dtheta - 0.004 * dr`: a cyclonic,
    /// convergent couplet, the classic tornadic signature. Every velocity is a
    /// multiple of 0.5, so the table is exact in f32.
    fn cyclonic_convergent_couplet() -> Vec<GradientSample> {
        gates(&[
            // velocity, dtheta, dr, weight
            (-21.0, -1050.0, 0.0, 1.0),
            (-8.0, -350.0, 250.0, 1.0),
            (4.5, 175.0, -250.0, 1.0),
            (13.5, 700.0, 125.0, 1.0),
        ])
    }

    /// The handedness of the result, derived rather than asserted.
    ///
    /// Put a cyclonic - counterclockwise, northern hemisphere - vortex in the
    /// kernel. Take the kernel centre due north of the radar, so the direction
    /// of increasing azimuth there points east. Counterclockwise flow around
    /// that centre runs northward on its east side and southward on its west
    /// side, that is away from the radar at positive `azimuthal_offset_m` and
    /// toward the radar at negative. Radial velocity is positive away from the
    /// radar, so `u` rises with `dtheta` and the fitted slope is positive.
    ///
    /// This needs its own test because the failure is invisible on a display: a
    /// sign flip renders every mesocyclone anticyclonic, and the couplet still
    /// looks exactly as convincing as before. The offsets here are deliberately
    /// asymmetric, so the result cannot come from a fixture that is symmetric
    /// about zero and would give the same answer either way.
    #[test]
    fn a_cyclonic_couplet_reports_positive_azimuthal_shear() {
        let fit = fit_linear_gradients(&cyclonic_convergent_couplet()).expect("solvable");
        // 1e-8 and not tighter: 0.02 is not a binary fraction, so the f32
        // result is 0.019_999_999_552_965_164, about 4.5e-10 low, and the f32
        // velocities carry their own rounding on top of that.
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - 0.02).abs() < 1e-8,
            "a cyclonic couplet reported {} s^-1",
            fit.azimuthal_shear_per_s
        );
        assert!(
            fit.azimuthal_shear_per_s > 0.0,
            "the sign itself is the claim: got {}",
            fit.azimuthal_shear_per_s
        );
    }

    #[test]
    fn mirroring_a_couplet_reverses_the_shear_and_leaves_its_magnitude_alone() {
        let couplet = cyclonic_convergent_couplet();
        let mirrored: Vec<GradientSample> = couplet
            .iter()
            .map(|sample| GradientSample {
                velocity_mps: -sample.velocity_mps,
                ..*sample
            })
            .collect();
        let cyclonic = fit_linear_gradients(&couplet).expect("solvable");
        let anticyclonic = fit_linear_gradients(&mirrored).expect("solvable");
        // Exact equality: negating every velocity negates every right-hand side
        // exactly, and the matrix is untouched, so the solve is the same
        // arithmetic with one sign changed.
        assert_eq!(
            anticyclonic.azimuthal_shear_per_s,
            -cyclonic.azimuthal_shear_per_s
        );
        assert!(
            anticyclonic.azimuthal_shear_per_s < 0.0,
            "the mirror of a cyclonic couplet must be anticyclonic, got {}",
            anticyclonic.azimuthal_shear_per_s
        );
    }

    /// Convergence is the negative sign, and it is the one that matters for
    /// mid-altitude radial convergence and for the descending reflectivity core
    /// of a downburst. Reporting inflow as positive would swap the two
    /// signatures on every display that colours divergence.
    #[test]
    fn velocity_falling_away_from_the_radar_reports_negative_divergence() {
        // Inbound 15 m s^-1 on the near side of the kernel, outbound -15 on the
        // far side: 30 m s^-1 of closing over 750 m, so -0.04 s^-1.
        let samples = gates(&[
            // velocity, dtheta, dr, weight
            (15.0, -1250.0, -375.0, 1.0),
            (-15.0, -1250.0, 375.0, 1.0),
            (15.0, 1250.0, -375.0, 1.0),
            (-15.0, 1250.0, 375.0, 1.0),
        ]);
        let fit = fit_linear_gradients(&samples).expect("solvable");
        // 1e-8 because -0.04 is not a binary fraction; the f32 result is
        // -0.039_999_999_105_930_33.
        assert!(
            (f64::from(fit.radial_divergence_per_s) + 0.04).abs() < 1e-8,
            "convergence was reported as {} s^-1",
            fit.radial_divergence_per_s
        );
        assert_eq!(
            fit.azimuthal_shear_per_s, 0.0,
            "pure convergence must leak no rotation"
        );
    }

    /// Three gates deep by two radials wide, the ordinary kernel beyond about
    /// 200 km where one 0.5-degree radial is already 1745 m across and the
    /// 2500 m kernel cannot reach a third.
    ///
    /// Its shear is a two-point difference with no redundancy: one bad gate
    /// moves it by half the error rather than by 1/N of it, which is the whole
    /// reason LLSD exists. `condition_estimate` reports 1.0 for it anyway,
    /// because a balanced kernel has orthogonal design columns however few
    /// radials it reached. This is pinned because the field is exactly what a
    /// downstream confidence layer would be tempted to threshold on, and here it
    /// hands the noisiest real geometry the best score available.
    #[test]
    fn a_balanced_two_radial_kernel_scores_a_perfect_one_despite_a_two_point_shear() {
        let mut samples = Vec::with_capacity(6);
        for radial_offset_m in RADIAL_OFFSETS_M {
            for azimuthal_offset_m in [-872.664_f32, 872.664] {
                samples.push(GradientSample {
                    velocity_mps: 0.008 * azimuthal_offset_m,
                    azimuthal_offset_m,
                    radial_offset_m,
                    weight: 1.0,
                });
            }
        }
        let fit = fit_linear_gradients(&samples).expect("two radials still determine a plane");
        assert_eq!(fit.sample_count, 6);
        // 1e-12 rather than exact equality: the determinant forms `a * (d * f)`
        // while the Hadamard bound forms `(a * d) * f`, two orderings of the
        // same product that may differ in the last bit.
        assert!(
            (fit.condition_estimate - 1.0).abs() < 1e-12,
            "a two-radial kernel scored {}, so this number cannot detect one",
            fit.condition_estimate
        );
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - 0.008).abs() < 1e-8,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
    }

    /// The companion failure: the estimate is blind to a *uniform* shrinking of
    /// the kernel, so a collapsed azimuthal baseline also scores 1.0.
    ///
    /// Half a millimetre between radials is not a geometry any caller should
    /// hand over, but nothing in the signature stops it, and the number it
    /// produces - 13333 s^-1, roughly a million times a violent mesocyclone -
    /// arrives wearing a perfect condition estimate. The guard cannot catch this
    /// and is not meant to: the fit is perfectly well posed, it is the baseline
    /// that is worthless. Only [`azimuthal_baseline_m`] can tell the caller so.
    #[test]
    fn a_collapsed_azimuthal_baseline_returns_thousands_of_inverse_seconds_and_still_scores_one() {
        let mut samples = Vec::with_capacity(6);
        for radial_offset_m in RADIAL_OFFSETS_M {
            for azimuthal_offset_m in [-0.000_5_f32, 0.000_5] {
                samples.push(GradientSample {
                    // One gate off by 40 m s^-1, one missed Nyquist fold.
                    velocity_mps: if radial_offset_m == -250.0 && azimuthal_offset_m > 0.0 {
                        40.0
                    } else {
                        0.0
                    },
                    azimuthal_offset_m,
                    radial_offset_m,
                    weight: 1.0,
                });
            }
        }
        let fit = fit_linear_gradients(&samples).expect("well posed, merely useless");
        // 40 m s^-1 over a 1 mm baseline: 0.0005 * 40 / (6 * 0.0005^2).
        assert!(
            (f64::from(fit.azimuthal_shear_per_s) - 13_333.333).abs() < 0.01,
            "shear was {}",
            fit.azimuthal_shear_per_s
        );
        assert!(
            (fit.condition_estimate - 1.0).abs() < 1e-12,
            "the collapsed kernel scored {}",
            fit.condition_estimate
        );
    }

    /// `condition_estimate` is normalised to `[1, inf)` but it is not
    /// dimensionless, so the offsets have to stay in metres.
    ///
    /// The Hadamard ratio is invariant under scaling the rows of the matrix, and
    /// a change of units is not a row scaling - it scales a row and a column
    /// together. The same four gates therefore score 39742 in metres and 5.0e6
    /// with `dtheta` in kilometres, a factor of 126, which also moves the
    /// kernel 126 times closer to the rejection threshold. Pinned because the
    /// tolerance's justification rests on this ratio being comparable against a
    /// fixed number, and that only holds for one choice of units.
    #[test]
    fn the_condition_estimate_depends_on_the_units_of_the_offsets() {
        let in_metres = [
            planar_gate(0.0, 0.0),
            planar_gate(250.0, 350.0),
            planar_gate(500.0, 700.0),
            planar_gate(250.0, 700.0),
        ];
        let in_kilometres: Vec<GradientSample> = in_metres
            .iter()
            .map(|sample| GradientSample {
                azimuthal_offset_m: sample.azimuthal_offset_m / 1000.0,
                ..*sample
            })
            .collect();
        let metres = fit_linear_gradients(&in_metres).expect("solvable");
        let kilometres = fit_linear_gradients(&in_kilometres).expect("solvable");
        // 1e-3 absolute on numbers near 4e4 and 5e6: they are pure f64
        // arithmetic, so only the last few bits are in doubt, but pinning all
        // sixteen digits would fail on any future reordering of the sums.
        assert!(
            (metres.condition_estimate - 39_742.283_895_694_9).abs() < 1e-3,
            "metres scored {}",
            metres.condition_estimate
        );
        assert!(
            (kilometres.condition_estimate - 5_000_100.697_061_906).abs() < 1e-3,
            "kilometres scored {}",
            kilometres.condition_estimate
        );
    }

    /// Unreachable through [`fit_linear_gradients`], which is the point.
    ///
    /// `A = sum_i w_i * v_i * v_i^T` with `v_i = (1, dr_i, dtheta_i)` and every
    /// `w_i > 0` is a Gram matrix, so no slice of samples can drive its
    /// determinant below zero; only cancellation can. The sums below are not a
    /// Gram matrix - `sum(w * dtheta^2)` is negative, which no real kernel can
    /// produce - and they stand in for that arithmetic breakdown. Note that
    /// `|det|` here equals the Hadamard bound exactly, so a guard written on the
    /// magnitude would have returned this as a fit with a condition estimate of
    /// 1.0: the most confident possible report of pure noise.
    #[test]
    fn a_negative_determinant_is_cancellation_noise_and_is_refused() {
        let impossible = NormalEquations {
            sum_w: 6.0,
            sum_wr: 0.0,
            sum_wt: 0.0,
            sum_wrr: 250_000.0,
            sum_wrt: 0.0,
            sum_wtt: -1_500.0,
            sum_wu: 40.0,
            sum_wru: -10_000.0,
            sum_wtu: 0.02,
            count: 6,
        };
        assert_eq!(impossible.solve(), None);
    }
}
