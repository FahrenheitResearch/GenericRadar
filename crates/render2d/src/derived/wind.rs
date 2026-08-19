//! Two damaging-wind proxies: mid-altitude radial convergence, and the
//! Stewart pulse-storm gust relation.
//!
//! Both answer the same operational question - "is this storm about to put
//! severe wind on the ground?" - from opposite ends of the volume. MARC reads
//! the mid-level velocity field directly and fires 10 to 20 minutes before the
//! surface gust. The Stewart relation reads only VIL and the echo top, and
//! diagnoses the downdraft a pulse storm can generate right now.
//!
//! # EXPERIMENTAL - MARC
//!
//! The MARC product in this module is EXPERIMENTAL and must be labelled that
//! way anywhere it is drawn. The reason is bibliographic, not numerical. The
//! paper universally credited with MARC,
//!
//!   Schmocker, G. K., R. W. Przybylinski, and Y.-J. Lin, 1996: Forecasting
//!   the initial onset of damaging downburst winds associated with a mesoscale
//!   convective system (MCS) using the mid-altitude radial convergence (MARC)
//!   signature. Preprints, 15th Conf. on Weather Analysis and Forecasting,
//!   Norfolk, VA, Amer. Meteor. Soc., 306-311.
//!
//! could not be obtained. The 1996 AMS preprint volume is not digitised and no
//! DOI exists; only the abstract and secondary restatements are available, and
//! the page range above is taken from those secondary citations rather than
//! from the preprint itself. Nothing here was checked against the primary
//! text, so nobody may treat these numbers as reproducing Schmocker's.
//!
//! What *is* verified is the definition, which the literature attributes to
//! Przybylinski et al. (1995) and which an NWS Louisville conference paper
//! (Funk, DeWald and Lin) quotes verbatim:
//!
//!   "MARC is defined as the 'delta V' or difference between the maximum
//!   inbound and outbound velocity values within 6 km along a radial ... MARC
//!   velocity values of 25 m/s (50 kts) or more at an altitude of 3 to 7 km
//!   preceded the onset of damaging surface winds by up to 20 minutes."
//!
//! The full bibliographic record for that 1995 citation - volume, pages, DOI -
//! could not be confirmed from the secondary source, so it is deliberately not
//! written here rather than guessed.
//!
//! Schmocker et al. (1996) is separately credited with the depth statistics:
//! an average vertical depth of the convergence zone of 6.2 km, a level of
//! maximum MARC near 5-6 km, and 25-30 m/s preceding damaging wind in about
//! 80 percent of a sample of eight warm-season linear MCSs. Eight cases is a
//! small sample, and 25-30 m/s is case-study guidance qualified everywhere it
//! appears by "persistent" and "deep-layered". It is not an operational
//! threshold, and this module exposes it as guidance
//! ([`MARC_CONTEXTUAL_GUIDANCE_MPS`]) rather than as a trigger.
//!
//! # Stewart pulse gust
//!
//! Stewart, S. R., 1991: The prediction of pulse-type thunderstorm gusts using
//! vertically integrated liquid water content (VIL) and the cloud top
//! penetrative downdraft mechanism. NOAA Tech. Memo. NWS SR-136, National
//! Weather Service Southern Region, Fort Worth, TX, 20 pp. plus a bound errata
//! sheet. Built on the similarity theory of Emanuel, K. A., 1981: A similarity
//! theory for unsaturated downdrafts within clouds. J. Atmos. Sci., 38,
//! 1541-1557, `doi:10.1175/1520-0469(1981)038<1541:ASTFUD>2.0.CO;2` - Stewart's
//! eq. (2) is Emanuel's eq. 26.
//!
//! Note that the memo's own reference list prints Emanuel's volume and pages
//! as "36, 2462-2478", which is a different paper; the memo's body text on p.5
//! cites "pp. 1547-1548", which lies inside 1541-1557. The record above is the
//! correct one.

// ---------------------------------------------------------------------------
// Part A: mid-altitude radial convergence (MARC)
// ---------------------------------------------------------------------------

/// The along-radial search window, in metres.
///
/// Przybylinski et al. (1995): the delta V is taken "within 6 km along a
/// radial". Widening it does not find a stronger signature, it finds a
/// different one: two gates 20 km apart on the same radial bracket the whole
/// storm, and their velocity difference is the flow across it rather than a
/// convergence zone inside it.
pub const MARC_WINDOW_M: f32 = 6000.0;

/// Bottom of the mid-altitude band, in metres above the antenna.
///
/// Below 3 km the same arithmetic measures low-level convergence along the
/// gust front, which is present under every mature MCS whether or not it is
/// about to produce damaging wind. Reporting that as MARC would make the
/// product fire on essentially every linear system, which is the same as not
/// firing at all.
pub const MARC_MIN_HEIGHT_ARL_M: f32 = 3000.0;

/// Top of the mid-altitude band, in metres above the antenna.
///
/// Above 7 km the beam is in the upper storm, where the largest along-radial
/// velocity differences belong to the divergent outflow spreading under the
/// anvil. The sign test in [`max_radial_convergence`] already refuses the
/// divergent side of that couplet, but it cannot refuse the convergent side:
/// raise the ceiling and the anvil-level flow meeting the environment ahead of
/// the storm is reported with the same number, and the same colour, as a
/// genuine mid-level convergence zone feeding a downdraft.
pub const MARC_MAX_HEIGHT_ARL_M: f32 = 7000.0;

/// The value the case-study literature associates with damaging surface wind
/// 10 to 20 minutes later.
///
/// Guidance, not a threshold. Schmocker et al. (1996) report 25-30 m/s
/// preceding damaging wind in about 80 percent of eight warm-season linear
/// MCSs, and every restatement qualifies it with "persistent" and
/// "deep-layered". A single sweep that crosses 25 m/s is not a forecast, so no
/// code in this crate may branch on this constant as though it were one.
pub const MARC_CONTEXTUAL_GUIDANCE_MPS: f32 = 25.0;

/// One dealiased velocity gate on one radial.
///
/// The velocity must already be dealiased. A folded gate reads as a jump of
/// twice the Nyquist velocity between neighbouring range bins, which is larger
/// than any real convergence on the radial and would win every time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadialGate {
    /// True distance along the beam to the gate centre, in metres.
    pub slant_range_m: f32,
    /// Beam-centre height above the antenna, in metres.
    pub height_arl_m: f32,
    /// Radial velocity in m/s, positive away from the radar.
    pub velocity_mps: f32,
}

/// The strongest convergent gate pair found on one radial.
///
/// `near_index` and `far_index` index the slice that was passed in, not any
/// filtered subset of it, so a caller can go straight back to its own gate
/// arrays for a probe readout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvergencePair {
    /// Index of the gate closer to the radar.
    pub near_index: usize,
    /// Index of the gate further from the radar. Always greater than
    /// `near_index`.
    pub far_index: usize,
    /// `velocity(near) - velocity(far)`, in m/s. Always strictly positive: a
    /// non-positive difference is divergence and is never reported here.
    pub delta_v_mps: f32,
    /// Along-radial distance between the two gates, in metres.
    pub separation_m: f32,
    /// Mean beam height of the two gates, in metres above the antenna. What an
    /// analyst reads to judge whether the signature is deep-layered or a thin
    /// artefact at one tilt.
    pub mean_height_arl_m: f32,
}

/// Whether a gate may take part in a pair at all.
fn gate_is_usable(gate: &RadialGate, min_height_arl_m: f32, max_height_arl_m: f32) -> bool {
    gate.slant_range_m.is_finite()
        && gate.height_arl_m.is_finite()
        && gate.velocity_mps.is_finite()
        && gate.height_arl_m >= min_height_arl_m
        && gate.height_arl_m <= max_height_arl_m
}

/// The largest convergent delta V on one radial, inside a window and a height
/// band.
///
/// `gates` must be ordered by increasing `slant_range_m` and must already be
/// dealiased. The ordering is load-bearing: the search stops scanning outward
/// as soon as a gate is further than `window_m` from the near gate, so an
/// unsorted slice makes the scan stop early and under-report, quietly, with no
/// error anywhere.
///
/// Only gates whose beam height lies within `[min_height_arl_m,
/// max_height_arl_m]` inclusive are considered, and *both* members of a pair
/// must be inside the band.
///
/// # Sign convention
///
/// Radial velocity is positive away from the radar. Convergence therefore
/// means the nearer gate's velocity *exceeds* the farther gate's, so that
/// dVr/dr < 0 and the two volumes of air are closing on each other. This
/// function maximises `velocity(near) - velocity(far)` and returns the largest
/// strictly positive result.
///
/// The failure this guards against is the one that makes a sign-blind
/// implementation useless: taking `abs(delta V)` reports the divergent
/// signature at the top of a collapsing updraft, and the divergent signature
/// straddling any strong storm top, as though they were MARC. Those are
/// routinely the largest velocity differences on the radial, so a sign-blind
/// product does not merely add false alarms - it reports divergence almost
/// everywhere and MARC almost nowhere.
///
/// Returns `None` when no strictly positive difference exists in the band,
/// which is the ordinary answer over most of a sweep.
pub fn max_radial_convergence(
    gates: &[RadialGate],
    window_m: f32,
    min_height_arl_m: f32,
    max_height_arl_m: f32,
) -> Option<ConvergencePair> {
    let mut best: Option<ConvergencePair> = None;

    for (near_index, near) in gates.iter().enumerate() {
        if !gate_is_usable(near, min_height_arl_m, max_height_arl_m) {
            continue;
        }

        for (far_index, far) in gates.iter().enumerate().skip(near_index + 1) {
            let separation_m = far.slant_range_m - near.slant_range_m;
            if separation_m > window_m {
                // Ranges increase outward, so nothing beyond this gate is
                // inside the window either.
                break;
            }
            // Two gates at the same range are one location sampled twice, and
            // the difference between them is not a gradient.
            if separation_m <= 0.0 {
                continue;
            }
            if !gate_is_usable(far, min_height_arl_m, max_height_arl_m) {
                continue;
            }

            let delta_v_mps = near.velocity_mps - far.velocity_mps;
            if delta_v_mps <= 0.0 {
                continue;
            }

            // Strictly greater, so the first pair found wins a tie and the
            // answer does not depend on where the caller trimmed the radial.
            let is_better = match best {
                None => true,
                Some(current) => delta_v_mps > current.delta_v_mps,
            };
            if is_better {
                best = Some(ConvergencePair {
                    near_index,
                    far_index,
                    delta_v_mps,
                    separation_m,
                    mean_height_arl_m: (near.height_arl_m + far.height_arl_m) / 2.0,
                });
            }
        }
    }

    best
}

// ---------------------------------------------------------------------------
// Part B: Stewart (1991) pulse-type thunderstorm gust
// ---------------------------------------------------------------------------

/// The VIL coefficient of Stewart (1991) eq. (2), p.5, in m s^-2.
///
/// Stewart's eq. (2) is
///
/// ```text
/// W = sqrt(20.628571 * Rbar_c * H - 3.125e-6 * H^2)
/// ```
///
/// with `W` the maximum downward velocity in m/s, `Rbar_c` the storm-averaged
/// rainwater content in g/g (dimensionless), and `H` the height in metres
/// above MSL of the 18 dBZ (VIP 1) echo top.
pub const STEWART_VIL_COEFFICIENT: f64 = 20.628571;

/// The echo-top coefficient of Stewart (1991) eq. (2), p.5, in s^-2.
///
/// The term it multiplies is subtracted, and in the combined form below it is
/// the only term the echo top appears in at all: at a fixed VIL, raising the
/// top spreads the same liquid through a deeper column, so the same water has
/// to drive a parcel down further and the parcel spends that extra depth
/// mixing.
///
/// Where the two terms cancel, the relation predicts no gust at all, which is
/// the "no value" case every caller must handle. That crossover is **not** a
/// fixed height. Setting the radicand to zero gives
///
/// ```text
/// TOP_zero = sqrt(20.628571 * VIL / 3.125e-6) = 2569.27 * sqrt(VIL) metres
/// ```
///
/// which is 16 249 m (53 300 ft) at VIL 40 but 21 496 m (70 500 ft) at VIL 70.
/// Hard-coding a single echo-top cutoff in a caller - "above 65 000 ft there is
/// no gust" - would erase the whole right-hand end of Stewart's Table 1, whose
/// last printed entry is a 9.0 kt gust for VIL 70 under a 70 000 ft top. The
/// VIL dependence is pinned by
/// `the_echo_top_where_the_relation_gives_up_depends_on_vil_not_on_height_alone`.
pub const STEWART_TOP_COEFFICIENT: f64 = 3.125e-6;

/// The VIL at and above which Stewart (1991) refuses to compute a gust, in
/// kg m^-2.
///
/// Stewart (1991), p.16, on Table 1: "potential gust values were not
/// calculated for VILs >= 75 Kgm^-2 due to the strong bias of large
/// reflectivity values produced by large, water coated hailstones which
/// creates unrealistic rainwater liquid water contents." Eq. (3) turns VIL
/// straight into rainwater content, so a VIL made of wet hail is read as
/// liquid that is not there, and the relation then returns a downdraft that
/// cannot happen. Table 1 itself stops at VIL 70.
pub const STEWART_MAX_VIL_KG_M2: f32 = 75.0;

/// Stewart's combined form with no applicability checks at all.
///
/// Separate from [`stewart_gust_potential_mps`] only so the tests can pin the
/// published worked examples, including the one the memo prints for a VIL the
/// memo itself forbids. Nothing outside this module may call it: it will
/// happily return a number for a 200 kg m^-2 hail core.
fn downdraft_mps_unguarded(vil_kg_m2: f32, echo_top_msl_m: f32) -> Option<f32> {
    let vil = f64::from(vil_kg_m2);
    let top = f64::from(echo_top_msl_m);
    let radicand = STEWART_VIL_COEFFICIENT * vil - STEWART_TOP_COEFFICIENT * top * top;
    if radicand < 0.0 {
        // A negative radicand is the relation saying this storm has no
        // downdraft to give, not a small one. Clamping it to zero would paint
        // a tall weak storm the same colour as one sitting exactly at the
        // cutoff, and taking the square root of it would paint NaN.
        return None;
    }
    Some(radicand.sqrt() as f32)
}

/// The downdraft the cloud-top penetrative mechanism can generate, in m/s.
///
/// `vil_kg_m2` is vertically integrated liquid in kg m^-2 and `echo_top_msl_m`
/// is the height of the 18 dBZ (VIP 1) echo top in metres **above MSL**, not
/// above the antenna. Stewart (1991), p.5, is explicit that `H` is "the height
/// (meters) above mean sea-level of the 18 dBz (VIP 1) echo". Every other
/// height in this crate is above radar level, so this one parameter flips the
/// convention, and nothing in the type system enforces it. Feeding it a height
/// above radar level understates the subtracted term and overstates the gust
/// everywhere the radar is not at sea level - by 2.28 m/s, 4.4 kt, about 9
/// percent, for a 60 kg m^-2 storm topping 45 000 ft MSL over a 1500 m site,
/// all of it in the unsafe direction. That error is pinned by
/// `feeding_a_height_above_radar_level_instead_of_msl_overstates_the_gust`.
///
/// This is Stewart (1991) eq. (2) with eq. (3), p.6, substituted into it. Eq.
/// (3) is `Rbar_c = VIL / TOP` under the memo's assumption that one cubic
/// metre of dry air has a mass of one kilogram (about right at 700 mb), which
/// is what makes `Rbar_c` dimensionless. The memo's bound errata sheet gives
/// the combined form directly:
///
/// ```text
/// W = sqrt(20.628571 * VIL - 3.125e-6 * TOP^2)
/// ```
///
/// # This is a lower bound on the memo's forecast gust
///
/// Stewart's "final potential gust" adds, vectorially, one third of the mean
/// wind speed in the lowest 5000 ft, following Miller, R. C., 1967: Notes on
/// analysis and severe storm forecasting procedures of the Military Weather
/// Warning Center. AWS Tech. Rep. 200, USAF Air Weather Service. In the memo's
/// own case studies that term is 2 to 7 kt. We have no wind profile here, so
/// we do not add it and we do not pretend to. The number returned is the
/// downdraft term alone and is therefore at or below the memo's final gust; a
/// readout that calls it "forecast gust" is wrong by 2 to 7 kt in the unsafe
/// direction.
///
/// # When this returns `None`
///
/// - `vil_kg_m2 >= STEWART_MAX_VIL_KG_M2`, which the author excludes outright.
/// - A non-positive or non-finite VIL or echo top. A zero echo top matters
///   because the combined form hides a division: eq. (3) is `VIL / TOP`, so at
///   `TOP = 0` the rainwater content is infinite, and the finite answer the
///   combined form produces there is an artefact of the algebra.
/// - A negative radicand, meaning no downdraft is defined for this pair.
///
/// # Applicability
///
/// Pulse-type, weakly sheared, air-mass storms only. Stewart (1991), p.16:
/// "this technique is not intended for strongly sheared storms". Emanuel
/// (1981), quoted on p.3 of the memo, is the reason - a supercell's updraft is
/// intense enough to preclude the penetration of downdrafts from aloft, so the
/// mechanism this relation describes does not operate there. Nothing in the
/// two input numbers can detect that, so the caller must.
pub fn stewart_gust_potential_mps(vil_kg_m2: f32, echo_top_msl_m: f32) -> Option<f32> {
    if !vil_kg_m2.is_finite() || !echo_top_msl_m.is_finite() {
        return None;
    }
    if vil_kg_m2 <= 0.0 || echo_top_msl_m <= 0.0 {
        return None;
    }
    if vil_kg_m2 >= STEWART_MAX_VIL_KG_M2 {
        return None;
    }
    downdraft_mps_unguarded(vil_kg_m2, echo_top_msl_m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One knot in m/s, exactly: 1852 m per nautical mile over 3600 s.
    const MPS_PER_KNOT: f32 = 1852.0 / 3600.0;

    /// One foot in metres, exactly.
    const M_PER_FT: f32 = 0.3048;

    fn gate(slant_range_m: f32, height_arl_m: f32, velocity_mps: f32) -> RadialGate {
        RadialGate {
            slant_range_m,
            height_arl_m,
            velocity_mps,
        }
    }

    // -----------------------------------------------------------------------
    // MARC
    // -----------------------------------------------------------------------

    #[test]
    fn the_marc_constants_are_the_published_window_band_and_guidance_value() {
        assert_eq!(MARC_WINDOW_M, 6000.0, "6 km along a radial");
        assert_eq!(MARC_MIN_HEIGHT_ARL_M, 3000.0, "3 km");
        assert_eq!(MARC_MAX_HEIGHT_ARL_M, 7000.0, "7 km");
        assert_eq!(MARC_CONTEXTUAL_GUIDANCE_MPS, 25.0, "25 m/s, 50 kt");
    }

    #[test]
    fn the_strongest_convergent_pair_in_the_band_is_reported_with_its_delta_v() {
        // Outbound 30 m/s at 40 km closing on inbound 25 m/s at 46 km: exactly
        // the 6 km window, delta V 55 m/s, well over the guidance value.
        let gates = [
            gate(40_000.0, 5_000.0, 30.0),
            gate(42_000.0, 5_000.0, 20.0),
            gate(44_000.0, 5_000.0, -20.0),
            gate(46_000.0, 5_000.0, -25.0),
        ];
        let pair = max_radial_convergence(
            &gates,
            MARC_WINDOW_M,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("a convergent pair exists");
        assert_eq!(pair.near_index, 0);
        assert_eq!(pair.far_index, 3);
        // 30.0 - (-25.0) is exact in binary floating point, so no tolerance.
        assert_eq!(pair.delta_v_mps, 55.0);
        assert_eq!(pair.separation_m, 6000.0);
        assert_eq!(pair.mean_height_arl_m, 5000.0);
        assert!(pair.delta_v_mps > MARC_CONTEXTUAL_GUIDANCE_MPS);
    }

    #[test]
    fn an_entirely_divergent_radial_reports_no_convergence_at_all() {
        // Inbound nearest the radar, outbound furthest: air pulling apart
        // along the beam, so dVr/dr > 0 for every pair.
        let gates = [
            gate(40_000.0, 5_000.0, -25.0),
            gate(42_000.0, 5_000.0, -10.0),
            gate(44_000.0, 5_000.0, 10.0),
            gate(46_000.0, 5_000.0, 25.0),
        ];
        assert_eq!(
            max_radial_convergence(
                &gates,
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M
            ),
            None
        );
    }

    #[test]
    fn a_strong_divergent_pair_loses_to_a_weak_convergent_one_because_the_sign_is_checked() {
        // Gates 0 and 1 differ by 60 m/s and gates 2 and 3 by 10 m/s. The 60
        // is divergence at a storm top; an implementation that maximised
        // abs(delta V) would report it, and be wrong by 180 degrees. The two
        // couplets sit 20 km apart so they never pair with each other.
        let gates = [
            gate(20_000.0, 5_000.0, -30.0),
            gate(21_000.0, 5_000.0, 30.0),
            gate(40_000.0, 5_000.0, 5.0),
            gate(41_000.0, 5_000.0, -5.0),
        ];
        let pair = max_radial_convergence(
            &gates,
            MARC_WINDOW_M,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("the weak convergent pair is still convergence");
        assert_eq!(pair.near_index, 2);
        assert_eq!(pair.far_index, 3);
        assert_eq!(pair.delta_v_mps, 10.0);
    }

    #[test]
    fn a_couplet_wider_than_the_window_is_outside_the_definition_and_is_not_reported() {
        // 7 km apart. The same two gates are found the moment the window is
        // widened to 8 km, which shows the window and nothing else rejected
        // them.
        let gates = [
            gate(40_000.0, 5_000.0, 30.0),
            gate(47_000.0, 5_000.0, -25.0),
        ];
        assert_eq!(
            max_radial_convergence(
                &gates,
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M
            ),
            None
        );
        let widened = max_radial_convergence(
            &gates,
            8_000.0,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("an 8 km window reaches");
        assert_eq!(widened.delta_v_mps, 55.0);
        assert_eq!(widened.separation_m, 7000.0);
    }

    #[test]
    fn a_stronger_couplet_below_the_band_is_ignored_so_marc_stays_mid_altitude() {
        // 80 m/s of convergence at 1.5 km is a gust front, which sits under
        // every mature MCS. 12 m/s at 5 km is the mid-altitude signature, and
        // only that one is MARC. Widening the band down to the ground finds
        // the low couplet, which shows height and nothing else rejected it.
        let gates = [
            gate(10_000.0, 1_500.0, 40.0),
            gate(11_000.0, 1_500.0, -40.0),
            gate(40_000.0, 5_000.0, 6.0),
            gate(41_000.0, 5_000.0, -6.0),
        ];
        let pair = max_radial_convergence(
            &gates,
            MARC_WINDOW_M,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("the mid-altitude couplet is in band");
        assert_eq!(pair.near_index, 2);
        assert_eq!(pair.delta_v_mps, 12.0);
        assert_eq!(pair.mean_height_arl_m, 5000.0);

        let unbanded = max_radial_convergence(&gates, MARC_WINDOW_M, 0.0, 8_000.0)
            .expect("the gust front is convergence too, just not MARC");
        assert_eq!(unbanded.near_index, 0);
        assert_eq!(unbanded.delta_v_mps, 80.0);
    }

    #[test]
    fn the_reported_indices_point_into_the_caller_slice_not_into_the_banded_subset() {
        // Two out-of-band gates come first. If the search indexed a filtered
        // copy, the answer would be (0, 1) and the caller would fetch the
        // wrong gates for its readout - plausibly, and 35 km off.
        let gates = [
            gate(5_000.0, 1_000.0, 0.0),
            gate(6_000.0, 1_000.0, 0.0),
            gate(40_000.0, 5_000.0, 20.0),
            gate(41_000.0, 5_000.0, -20.0),
        ];
        let pair = max_radial_convergence(
            &gates,
            MARC_WINDOW_M,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("the in-band couplet is found");
        assert_eq!(pair.near_index, 2);
        assert_eq!(pair.far_index, 3);
        assert_eq!(pair.delta_v_mps, 40.0);
    }

    #[test]
    fn the_first_of_two_equally_strong_couplets_is_reported_so_the_answer_is_deterministic() {
        // Both couplets are 20 m/s. A non-strict comparison would return the
        // last one, and the reported range would then depend on how far out
        // the caller happened to trim the radial.
        let gates = [
            gate(40_000.0, 5_000.0, 10.0),
            gate(41_000.0, 5_000.0, -10.0),
            gate(50_000.0, 5_000.0, 10.0),
            gate(51_000.0, 5_000.0, -10.0),
        ];
        let pair = max_radial_convergence(
            &gates,
            MARC_WINDOW_M,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("a convergent pair exists");
        assert_eq!(pair.near_index, 0);
        assert_eq!(pair.far_index, 1);
        assert_eq!(pair.delta_v_mps, 20.0);
    }

    #[test]
    fn a_gate_holding_a_non_finite_velocity_can_never_win_the_maximum() {
        // NaN loses every comparison, so against an empty running maximum it
        // would be adopted as the best pair and then never displaced.
        let gates = [
            gate(40_000.0, 5_000.0, f32::NAN),
            gate(41_000.0, 5_000.0, -50.0),
            gate(42_000.0, 5_000.0, 8.0),
            gate(43_000.0, 5_000.0, -2.0),
        ];
        let pair = max_radial_convergence(
            &gates,
            MARC_WINDOW_M,
            MARC_MIN_HEIGHT_ARL_M,
            MARC_MAX_HEIGHT_ARL_M,
        )
        .expect("the finite couplet is found");
        assert_eq!(pair.near_index, 2);
        assert_eq!(pair.far_index, 3);
        assert_eq!(pair.delta_v_mps, 10.0);
    }

    #[test]
    fn two_gates_at_the_same_range_are_one_location_and_yield_no_gradient() {
        let gates = [
            gate(40_000.0, 5_000.0, 30.0),
            gate(40_000.0, 5_000.0, -30.0),
        ];
        assert_eq!(
            max_radial_convergence(
                &gates,
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M
            ),
            None
        );
    }

    #[test]
    fn an_empty_or_single_gate_radial_reports_nothing_rather_than_panicking() {
        assert_eq!(
            max_radial_convergence(
                &[],
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M
            ),
            None
        );
        let one = [gate(40_000.0, 5_000.0, 30.0)];
        assert_eq!(
            max_radial_convergence(
                &one,
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M
            ),
            None
        );
    }

    #[test]
    fn a_pair_with_only_one_member_in_the_band_is_not_marc_because_both_ends_must_be_mid_level() {
        // The near gate is at 6.9 km, inside the band; the far gate is at
        // 7.6 km, 600 m above it. Their 40 m/s difference is mid-level flow
        // meeting the storm-top outflow, not a convergence zone inside the
        // mid-altitude layer, and painting it would put a MARC colour on the
        // anvil edge. Raising the ceiling to 8 km finds the same pair, which
        // shows the band and nothing else rejected it.
        let gates = [
            gate(40_000.0, 6_900.0, 20.0),
            gate(41_000.0, 7_600.0, -20.0),
        ];
        assert_eq!(
            max_radial_convergence(
                &gates,
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M
            ),
            None
        );
        let raised = max_radial_convergence(&gates, MARC_WINDOW_M, MARC_MIN_HEIGHT_ARL_M, 8_000.0)
            .expect("an 8 km ceiling admits both gates");
        assert_eq!(raised.delta_v_mps, 40.0);
        assert_eq!(raised.mean_height_arl_m, 7250.0);
    }

    #[test]
    fn the_band_bounds_are_inclusive_so_a_gate_sitting_exactly_on_an_edge_still_counts() {
        // The definition says "an altitude of 3 to 7 km", which includes the
        // ends. A half-open comparison would silently drop every gate a
        // sampler snapped onto a round height, and 3 km and 7 km are exactly
        // the heights such a sampler lands on.
        let on_the_floor = [
            gate(40_000.0, MARC_MIN_HEIGHT_ARL_M, 15.0),
            gate(41_000.0, MARC_MIN_HEIGHT_ARL_M, -15.0),
        ];
        let on_the_ceiling = [
            gate(40_000.0, MARC_MAX_HEIGHT_ARL_M, 15.0),
            gate(41_000.0, MARC_MAX_HEIGHT_ARL_M, -15.0),
        ];
        for gates in [&on_the_floor, &on_the_ceiling] {
            let pair = max_radial_convergence(
                gates,
                MARC_WINDOW_M,
                MARC_MIN_HEIGHT_ARL_M,
                MARC_MAX_HEIGHT_ARL_M,
            )
            .expect("a gate exactly on a band edge is inside the band");
            assert_eq!(pair.delta_v_mps, 30.0);
        }
    }

    // -----------------------------------------------------------------------
    // Stewart (1991)
    // -----------------------------------------------------------------------

    #[test]
    fn stewart_table_one_vil_forty_top_twenty_five_kft_is_forty_nine_point_three_knots() {
        // Stewart (1991) Table 1, p.16: row VIL 40, column TOP 250 (hundreds
        // of feet) prints 49.3 kt. Recomputed by hand:
        //   H        = 25 000 ft * 0.3048 m/ft      = 7620 m
        //   20.628571 * 40                          = 825.14284
        //   7620^2                                  = 58 064 400
        //   3.125e-6 * 58 064 400                   = 181.45125
        //   radicand = 825.14284 - 181.45125        = 643.69159
        //   W        = sqrt(643.69159)              = 25.3711 m/s
        //   in knots = 25.3711 / 0.5144444          = 49.32 kt
        let top_m = 25_000.0 * M_PER_FT;
        assert_eq!(top_m, 7620.0);
        let w = stewart_gust_potential_mps(40.0, top_m).expect("the radicand is positive");
        // 0.001 m/s: the hand value above is carried to six significant
        // figures, and f32 itself resolves about 2e-6 m/s at this magnitude,
        // so the hand rounding is the only slack needed.
        assert!((w - 25.3711).abs() < 0.001, "W was {w} m/s, not 25.3711");
        let knots = w / MPS_PER_KNOT;
        // 0.05 kt: Table 1 is printed to a tenth of a knot, so a printed entry
        // can differ from the exact value by half a tenth by rounding alone.
        // The exact value here is 49.32 kt.
        assert!((knots - 49.3).abs() < 0.05, "W was {knots} kt, not 49.3");
    }

    #[test]
    fn stewart_table_one_vil_seventy_top_seventy_kft_is_nine_knots() {
        // Stewart (1991) Table 1, p.16: row VIL 70, column TOP 700 prints 9.0
        // kt, the last entry before the height term wins outright. Recomputed:
        //   H        = 70 000 ft * 0.3048 m/ft      = 21 336 m
        //   20.628571 * 70                          = 1443.99997
        //   21 336^2                                = 455 224 896
        //   3.125e-6 * 455 224 896                  = 1422.57780
        //   radicand = 1443.99997 - 1422.57780      = 21.42217
        //   W        = sqrt(21.42217)               = 4.62841 m/s
        //   in knots = 4.62841 / 0.5144444          = 9.00 kt
        let top_m = 70_000.0 * M_PER_FT;
        assert_eq!(top_m, 21336.0);
        let w = stewart_gust_potential_mps(70.0, top_m).expect("the radicand is barely positive");
        // 0.001 m/s for the same reason as the previous test: hand rounding,
        // not float error, is what is being allowed for.
        assert!((w - 4.6284).abs() < 0.001, "W was {w} m/s, not 4.6284");
        let knots = w / MPS_PER_KNOT;
        assert!((knots - 9.0).abs() < 0.05, "W was {knots} kt, not 9.0");
    }

    #[test]
    fn the_memos_page_six_worked_example_reproduces_as_25_48_metres_per_second() {
        // Stewart (1991) p.6: VIL 60 kg m^-2, TOP 45 000 ft (13 720 m MSL).
        //   20.628571 * 60                          = 1237.71426
        //   13 720^2                                = 188 238 400
        //   3.125e-6 * 188 238 400                  = 588.24500
        //   radicand = 1237.71426 - 588.24500       = 649.46926
        //   W        = sqrt(649.46926)              = 25.4847 m/s
        // The memo prints 25.48 m/s, which we reproduce.
        let w = stewart_gust_potential_mps(60.0, 13_720.0).expect("the radicand is positive");
        assert!((w - 25.4847).abs() < 0.001, "W was {w} m/s, not 25.4847");
        // 0.005 m/s against the memo's own printed figure, because that figure
        // is printed to two decimals and can be half a hundredth off by
        // rounding.
        assert!((w - 25.48).abs() < 0.005, "the memo prints 25.48 m/s");
    }

    #[test]
    fn the_thirteen_two_seventy_printed_in_the_memos_radicand_is_a_transposition() {
        // Stewart (1991) p.6 substitutes "(3.125x10^-6)(13,270)^2" while every
        // other appearance of the same storm says 13,720 m. The printed answer
        // settles which is meant: 13 270 gives
        //   3.125e-6 * 176 092 900                  = 550.29031
        //   radicand = 1237.71426 - 550.29031       = 687.42395
        //   W        = sqrt(687.42395)              = 26.2188 m/s
        // and the memo then prints 25.48 m/s. Do not copy the typo: the digits
        // are transposed and the answer beside them is right.
        let typo = stewart_gust_potential_mps(60.0, 13_270.0).expect("the radicand is positive");
        assert!((typo - 26.2188).abs() < 0.001, "W was {typo} m/s");
        assert!(
            (typo - 25.48).abs() > 0.5,
            "the transposed height cannot produce the memo's own printed answer"
        );
    }

    #[test]
    fn the_memos_second_page_six_example_is_22_39_mps_not_the_printed_23_34() {
        // Stewart (1991) p.6, second example: "a storm containing a VIL of 75
        // Kgm^-2 and a TOP of 60,000 ft (18,293m), a maximum gust of 23.34
        // ms^-1 (45.9 kt) could occur." That answer does not follow from the
        // memo's own eq. (2). Recomputed with the memo's own metre figure:
        //   20.628571 * 75                          = 1547.14283
        //   18 293^2                                = 334 633 849
        //   3.125e-6 * 334 633 849                  = 1045.73078
        //   radicand = 1547.14283 - 1045.73078      = 501.41205
        //   W        = sqrt(501.41205)              = 22.3922 m/s, i.e. 43.5 kt
        // We pin 22.39, the arithmetic, and not 23.34, the misprint. This goes
        // through the unguarded core deliberately: VIL 75 is exactly the value
        // the memo elsewhere refuses to compute, which is the next test.
        let w = downdraft_mps_unguarded(75.0, 18_293.0).expect("the radicand is positive");
        // 0.005 m/s: the reference is quoted to two decimals, and the exact
        // value 22.3922 sits 0.0022 away from it.
        assert!((w - 22.39).abs() < 0.005, "W was {w} m/s, not 22.39");
        assert!(
            (w - 23.34).abs() > 0.9,
            "the memo's printed 23.34 m/s does not follow from its own equation"
        );
    }

    #[test]
    fn a_vil_at_or_above_the_authors_own_limit_is_refused_rather_than_extrapolated() {
        // Stewart (1991), p.16: no gust was calculated for VIL >= 75 kg m^-2,
        // because that much VIL is wet hail being read as liquid water. This
        // is the very pair the memo itself prints on p.6, which is why the
        // limit has to live in the code and not only in a comment.
        assert_eq!(stewart_gust_potential_mps(75.0, 18_293.0), None);
        assert_eq!(stewart_gust_potential_mps(80.0, 13_720.0), None);
        // Just under the limit still answers, so the limit and nothing else is
        // what rejected the two above.
        assert!(stewart_gust_potential_mps(74.9, 13_720.0).is_some());
    }

    #[test]
    fn a_negative_radicand_is_no_gust_at_all_rather_than_a_silently_clamped_zero() {
        // A 40 kg m^-2 VIL under a 60 000 ft top:
        //   20.628571 * 40                          = 825.14284
        //   18 288^2                                = 334 450 944
        //   3.125e-6 * 334 450 944                  = 1045.15920
        //   radicand = 825.14284 - 1045.15920       = -220.01636
        // Clamping that to 0.0 would paint this storm identically to one
        // sitting exactly at the cutoff, and would put a number in a cell the
        // relation has nothing to say about.
        let top_m = 60_000.0 * M_PER_FT;
        assert_eq!(top_m, 18288.0);
        assert_eq!(stewart_gust_potential_mps(40.0, top_m), None);
    }

    #[test]
    fn a_zero_echo_top_is_refused_because_the_rainwater_content_behind_it_is_undefined() {
        // The combined form hides the division in eq. (3), Rbar_c = VIL / TOP.
        // At TOP = 0 the storm-averaged rainwater content is infinite, so the
        // finite 35.2 m/s the combined form would return for VIL 60 is an
        // artefact of the algebra rather than a downdraft.
        assert_eq!(stewart_gust_potential_mps(60.0, 0.0), None);
        assert_eq!(stewart_gust_potential_mps(60.0, -100.0), None);
    }

    #[test]
    fn a_non_positive_or_non_finite_input_is_refused() {
        assert_eq!(stewart_gust_potential_mps(0.0, 13_720.0), None);
        assert_eq!(stewart_gust_potential_mps(-10.0, 13_720.0), None);
        assert_eq!(stewart_gust_potential_mps(f32::NAN, 13_720.0), None);
        assert_eq!(stewart_gust_potential_mps(60.0, f32::NAN), None);
        assert_eq!(stewart_gust_potential_mps(f32::INFINITY, 13_720.0), None);
        assert_eq!(stewart_gust_potential_mps(60.0, f32::INFINITY), None);
    }

    #[test]
    fn the_echo_top_where_the_relation_gives_up_depends_on_vil_not_on_height_alone() {
        // The radicand vanishes at TOP = sqrt(20.628571 * VIL / 3.125e-6),
        // i.e. 2569.27 * sqrt(VIL) metres, so no single echo top separates
        // "gust" from "no gust". At one and the same 65 000 ft top:
        //   H        = 65 000 ft * 0.3048 m/ft      = 19 812 m
        //   19 812^2                                = 392 515 344
        //   3.125e-6 * 392 515 344                  = 1226.61045
        //   VIL 60: 1237.71426 - 1226.61045         =   11.10381 -> 3.3322 m/s
        //   VIL 55: 1134.57141 - 1226.61045         =  -92.03904 -> no gust
        // Stewart (1991) Table 1, p.16, prints 6.5 kt for VIL 60 at TOP 650,
        // and 3.3322 m/s is 6.48 kt. A caller that hard-coded "no gust above
        // 65 000 ft" would erase that entry and everything to the right of it.
        let top_m = 65_000.0 * M_PER_FT;
        assert_eq!(top_m, 19812.0);
        let sixty = stewart_gust_potential_mps(60.0, top_m).expect("VIL 60 still has a gust");
        // 0.001 m/s, as in the Table 1 tests above: the hand value is carried
        // to six significant figures and that rounding, not f32, is the slack.
        assert!(
            (sixty - 3.3322).abs() < 0.001,
            "W was {sixty} m/s, not 3.3322"
        );
        let knots = sixty / MPS_PER_KNOT;
        // 0.05 kt because Table 1 is printed to a tenth of a knot; the exact
        // value is 6.48 kt.
        assert!((knots - 6.5).abs() < 0.05, "W was {knots} kt, not 6.5");
        assert_eq!(
            stewart_gust_potential_mps(55.0, top_m),
            None,
            "VIL 55 under the same top is past its own crossover"
        );
        // Higher still than 65 000 ft, VIL 70 is answered rather than refused.
        assert!(stewart_gust_potential_mps(70.0, 70_000.0 * M_PER_FT).is_some());
    }

    #[test]
    fn feeding_a_height_above_radar_level_instead_of_msl_overstates_the_gust() {
        // The same 45 000 ft storm and the same VIL, passed once as the MSL
        // height the memo asks for and once as a height above a 1500 m
        // antenna:
        //   MSL 13 716 m: 1237.71426 - 587.90205 = 649.81221 -> 25.4914 m/s
        //   ARL 12 216 m: 1237.71426 - 466.34580 = 771.36846 -> 27.7735 m/s
        // 2.28 m/s, 4.4 kt, about 9 percent, and all of it in the unsafe
        // direction. Two f32 arguments cannot tell the two apart, so the
        // parameter name and the doc comment are the only defence there is,
        // and this test is what keeps the number in that doc comment honest.
        let top_msl_m = 45_000.0 * M_PER_FT;
        assert_eq!(top_msl_m, 13716.0);
        let correct =
            stewart_gust_potential_mps(60.0, top_msl_m).expect("the radicand is positive");
        let mistaken = stewart_gust_potential_mps(60.0, top_msl_m - 1500.0).expect("also positive");
        assert!((correct - 25.4914).abs() < 0.001, "MSL gave {correct} m/s");
        assert!(
            (mistaken - 27.7735).abs() < 0.001,
            "ARL gave {mistaken} m/s"
        );
        // 0.01 m/s rather than 0.001: this is a difference of two values each
        // pinned to six significant figures, so the hand rounding accumulates.
        let overstatement = mistaken - correct;
        assert!(
            (overstatement - 2.2821).abs() < 0.01,
            "the ARL mistake was worth {overstatement} m/s, not 2.2821"
        );
    }

    #[test]
    fn the_stewart_coefficients_are_the_published_ones() {
        assert_eq!(STEWART_VIL_COEFFICIENT, 20.628571);
        assert_eq!(STEWART_TOP_COEFFICIENT, 3.125e-6);
        assert_eq!(STEWART_MAX_VIL_KG_M2, 75.0);
    }
}
