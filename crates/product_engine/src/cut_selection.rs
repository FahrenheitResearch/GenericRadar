//! Choosing which sweep a product is drawn from.
//!
//! This replaces "the first cut in the file that carries the moment", which is
//! wrong on every modern volume for two separate reasons.
//!
//! **Split cuts.** NEXRAD scans its lowest tilts twice at one commanded
//! elevation: a long-range low-PRF surveillance sweep carrying reflectivity and
//! dual-pol out to 460 km, and a short-range high-PRF Doppler sweep carrying
//! velocity and spectrum width to about 300 km. Both carry reflectivity. Taking
//! whichever the file lists first is taking a coin flip.
//!
//! **SAILS.** A VCP 212 volume with SAILSx3 scans the lowest tilt four times,
//! spread across the whole volume period, precisely so that low-level data
//! updates faster than the volume does. Taking the first one in file order
//! throws that away and shows the oldest. Measured on KTLX 2026-08-17 07:24:02,
//! a real SAILSx3 volume: the lowest tilt carries eight sweeps, and index-order
//! selection serves velocity that is **254.7 seconds** older than the freshest
//! sweep of the same tilt sitting in the same file. Reproduced on KUDX
//! (198.5 s) and on a second KTLX volume (182.2 s). Four minutes is an eternity
//! in a warning decision, and the fresher data was already decoded.
//!
//! The ordering here is deliberately **total**: leg, then whether the sweep is
//! far enough round to be worth drawing, then reach, then recency, then
//! Nyquist, then cut index. Rust's `min_by`/`max_by` return the first and
//! last of equal elements respectively, so an ordering with ties silently
//! encodes "whichever the file happened to list first" as policy. With the cut
//! index as a final discriminator there are no ties, and the choice is a
//! decision rather than an accident.

use radar_core::MomentType;

use crate::capabilities::{CutCapabilities, CutIdentity, CutLeg, VolumeCapabilities};
use crate::registry::CutSelectionPolicy;

/// A sweep that has covered less azimuth than this is still being written and
/// must not displace a finished one.
///
/// Without this, recency backfires on a live volume: the sweep currently
/// arriving is always the freshest, so a two-radial fragment would outrank the
/// complete sweep of the same tilt from thirty seconds ago and the pane would
/// go nearly blank. 300 of 360 degrees means a sweep takes over once it is
/// most of the way round, which is early enough to keep low-level data fresh
/// and late enough that the picture is worth looking at.
pub const MIN_USABLE_AZIMUTH_COVERAGE_DEG: f32 = 300.0;

/// Whether a sweep has enough of a revolution to be worth drawing.
fn is_usable(cut: &CutCapabilities) -> bool {
    cut.azimuth_coverage_deg >= MIN_USABLE_AZIMUTH_COVERAGE_DEG
}

/// Which sweep was chosen, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct CutChoice {
    pub cut_index: usize,
    pub identity: CutIdentity,
    pub leg: CutLeg,
    /// How many other sweeps of this same commanded tilt were passed over.
    pub repeats_passed_over: usize,
    /// How much older the sweep that index-order selection would have taken is,
    /// in milliseconds. Zero when index order would have made the same choice.
    /// This is the number that makes the SAILS defect visible in a readout.
    pub older_alternative_ms: i32,
}

/// Rank a leg against what a product wants. Higher is better.
///
/// A combined sweep sits between the two specialised legs: it carries the
/// moment, but where a specialised leg exists it is the better source - the
/// surveillance leg reaches 160 km further for reflectivity, and the Doppler
/// leg has three times the Nyquist for velocity.
fn leg_rank(leg: CutLeg, policy: CutSelectionPolicy) -> u8 {
    match (policy, leg) {
        (CutSelectionPolicy::LongestUnfoldedRange, CutLeg::Surveillance) => 2,
        (CutSelectionPolicy::LongestUnfoldedRange, CutLeg::Combined) => 1,
        (CutSelectionPolicy::LongestUnfoldedRange, CutLeg::Doppler) => 0,
        (CutSelectionPolicy::VelocityLeg, CutLeg::Doppler) => 2,
        (CutSelectionPolicy::VelocityLeg, CutLeg::Combined) => 1,
        (CutSelectionPolicy::VelocityLeg, CutLeg::Surveillance) => 0,
    }
}

/// Order two candidate sweeps. The greater one is the better choice.
///
/// Every term is needed and the order of the terms is the policy:
/// 1. the leg the product wants;
/// 2. whether the sweep is far enough round to be worth drawing - this must
///    outrank recency, or the sweep currently arriving always wins;
/// 3. how far the moment is encoded to reach;
/// 4. how recently it was scanned - this is what SAILS is for;
/// 5. Nyquist, which separates two Doppler legs of the same tilt that differ in
///    PRF (seen on real KRLX and KUDX volumes at 33.2 versus 24.0 m/s);
/// 6. the cut index, so the ordering is total and nothing is left to chance.
fn compare_candidates(
    left: &CutCapabilities,
    right: &CutCapabilities,
    moment: &MomentType,
    policy: CutSelectionPolicy,
) -> std::cmp::Ordering {
    leg_rank(left.leg, policy)
        .cmp(&leg_rank(right.leg, policy))
        .then_with(|| is_usable(left).cmp(&is_usable(right)))
        .then_with(|| left.range_km(moment).total_cmp(&right.range_km(moment)))
        .then_with(|| left.median_radial_time_ms.cmp(&right.median_radial_time_ms))
        .then_with(|| {
            left.representative_nyquist_mps
                .unwrap_or(0.0)
                .total_cmp(&right.representative_nyquist_mps.unwrap_or(0.0))
        })
        .then_with(|| left.index.cmp(&right.index))
}

fn choose_among(
    candidates: &[&CutCapabilities],
    moment: &MomentType,
    policy: CutSelectionPolicy,
) -> Option<CutChoice> {
    let best = candidates
        .iter()
        .copied()
        .max_by(|left, right| compare_candidates(left, right, moment, policy))?;
    // What plain index order would have taken, so the choice can be explained.
    let index_order = candidates
        .iter()
        .copied()
        .min_by_key(|candidate| candidate.index)?;
    Some(CutChoice {
        cut_index: best.index,
        identity: best.identity,
        leg: best.leg,
        repeats_passed_over: candidates.len().saturating_sub(1),
        older_alternative_ms: best.median_radial_time_ms - index_order.median_radial_time_ms,
    })
}

/// The best sweep for a moment at one commanded tilt.
pub fn select_in_group(
    capabilities: &VolumeCapabilities,
    group_index: usize,
    moment: &MomentType,
    policy: CutSelectionPolicy,
) -> Option<CutChoice> {
    let group = capabilities.groups.get(group_index)?;
    let candidates: Vec<&CutCapabilities> = group
        .members
        .iter()
        .filter_map(|index| capabilities.cut(*index))
        .filter(|cut| cut.has_moment(moment))
        .collect();
    choose_among(&candidates, moment, policy)
}

/// The best sweep for a moment at the lowest commanded tilt that carries it.
///
/// The replacement for `first_available_cut`. Two differences that matter: the
/// tilt is chosen by measured elevation rather than by position in the file,
/// and among the sweeps of that tilt the freshest wins rather than the first.
pub fn select_lowest_tilt(
    capabilities: &VolumeCapabilities,
    moment: &MomentType,
    policy: CutSelectionPolicy,
) -> Option<CutChoice> {
    // Groups are already in ascending elevation order, so the first group with
    // a candidate is the lowest tilt that carries the moment.
    (0..capabilities.groups.len())
        .find_map(|group_index| select_in_group(capabilities, group_index, moment, policy))
}

/// The sweep nearest a requested elevation that carries a moment.
pub fn select_nearest_elevation(
    capabilities: &VolumeCapabilities,
    target_elevation_deg: f32,
    moment: &MomentType,
    policy: CutSelectionPolicy,
) -> Option<CutChoice> {
    // Choose the tilt first, then the best sweep of that tilt. Choosing a sweep
    // directly would let a stale repeat of a nearer tilt lose to a fresh sweep
    // of a further one, which is not what "nearest" means.
    let mut best_group: Option<(usize, f32)> = None;
    for (group_index, group) in capabilities.groups.iter().enumerate() {
        if select_in_group(capabilities, group_index, moment, policy).is_none() {
            continue;
        }
        let distance = (group.elevation_deg - target_elevation_deg).abs();
        let better = match best_group {
            // Strictly less, so an exact tie keeps the lower tilt. Ties are
            // possible: a target halfway between two VCP elevations.
            Some((_, best_distance)) => distance < best_distance,
            None => true,
        };
        if better {
            best_group = Some((group_index, distance));
        }
    }
    let (group_index, _) = best_group?;
    select_in_group(capabilities, group_index, moment, policy)
}

/// Step up or down one commanded tilt from the sweep currently shown.
///
/// Stepping by cut index instead is the other half of the SAILS defect: on a
/// VCP 212 volume the cut list runs 0.5, 0.5, 0.9, 0.9, 1.3, 1.3, 0.5 ..., so
/// "next tilt" from the first sweep lands on the *other leg of the same tilt*,
/// and four presses are needed to leave 0.5 degrees. Stepping by group moves
/// one real elevation at a time and lands on that tilt's best sweep.
pub fn step_tilt(
    capabilities: &VolumeCapabilities,
    from_cut_index: usize,
    delta: isize,
    moment: &MomentType,
    policy: CutSelectionPolicy,
) -> Option<CutChoice> {
    let current_group = capabilities
        .groups
        .iter()
        .position(|group| group.members.contains(&from_cut_index))?;
    let mut group_index = current_group as isize;
    loop {
        group_index += delta.signum();
        if group_index < 0 || group_index >= capabilities.groups.len() as isize {
            return None;
        }
        // Skip tilts that do not carry this moment rather than stopping at
        // them, so stepping through velocity does not stall on a surveillance
        // tilt that has no velocity at all.
        if let Some(choice) = select_in_group(capabilities, group_index as usize, moment, policy) {
            return Some(choice);
        }
    }
}

/// One reflectivity sweep per commanded tilt, in ascending elevation order.
///
/// This is what a vertical product integrates over. Choosing one representative
/// per tilt is not an optimisation; it is the difference between a column and a
/// fiction. A SAILSx3 volume holds eight sweeps of the lowest tilt, and
/// integrating all eight would report eight layers of liquid water where the
/// atmosphere has one.
pub fn volume_reflectivity_representatives(
    capabilities: &VolumeCapabilities,
    policy: CutSelectionPolicy,
) -> Vec<CutChoice> {
    (0..capabilities.groups.len())
        .filter_map(|group_index| {
            select_in_group(capabilities, group_index, &MomentType::Reflectivity, policy)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::VolumeCapabilities;
    use chrono::{TimeZone, Utc};
    use radar_core::{ElevationCut, GateRange, MomentGrid, RadarSite, RadarVolume, Radial};

    fn grid(moment: MomentType, gate_count: usize) -> MomentGrid {
        MomentGrid::new_u8(
            moment,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        )
    }

    /// A sweep at a commanded tilt, scanned at a stated time.
    ///
    /// `reach_gates` differs between the legs on purpose: 1840 gates is the
    /// 460 km surveillance reach, 1200 is the 300 km Doppler reach.
    fn sweep(
        elevation_number: u8,
        commanded_deg: f32,
        time_ms: i32,
        nyquist: f32,
        moments: &[(MomentType, usize)],
    ) -> ElevationCut {
        let mut cut = ElevationCut::new(commanded_deg - 0.12, Some(elevation_number));
        for index in 0..360 {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                // The first few radials ramp, as a real antenna does.
                elevation_deg: if index < 6 {
                    commanded_deg - 0.12
                } else {
                    commanded_deg
                },
                time_offset_ms: time_ms + index * 10,
                gate_range: GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 250,
                    gate_count: 100,
                },
                nyquist_velocity_mps: Some(nyquist),
                radial_status: None,
            });
        }
        for (moment, gates) in moments {
            cut.moments
                .insert(moment.clone(), grid(moment.clone(), *gates));
        }
        cut
    }

    fn surveillance(time_ms: i32, elevation: f32) -> ElevationCut {
        sweep(
            1,
            elevation,
            time_ms,
            8.3,
            &[
                (MomentType::Reflectivity, 1840),
                (MomentType::DifferentialReflectivity, 1840),
            ],
        )
    }

    fn doppler(time_ms: i32, elevation: f32, nyquist: f32) -> ElevationCut {
        sweep(
            2,
            elevation,
            time_ms,
            nyquist,
            &[
                (MomentType::Reflectivity, 1200),
                (MomentType::Velocity, 1200),
                (MomentType::SpectrumWidth, 1200),
            ],
        )
    }

    fn analyze(cuts: Vec<ElevationCut>) -> VolumeCapabilities {
        let mut volume = RadarVolume::new(
            RadarSite::new("KTLX"),
            Utc.with_ymd_and_hms(2026, 8, 17, 7, 24, 2).unwrap(),
        );
        volume.cuts = cuts;
        VolumeCapabilities::analyze(&volume)
    }

    /// A VCP 212 SAILSx3 volume: the lowest tilt scanned four times, each time
    /// as a surveillance leg followed by a Doppler leg, spread over the volume.
    fn sails_x3_volume() -> VolumeCapabilities {
        analyze(vec![
            surveillance(0, 0.48),
            doppler(20_000, 0.48, 24.1),
            surveillance(90_000, 0.88),
            doppler(110_000, 0.88, 24.1),
            surveillance(180_000, 0.48),
            doppler(200_000, 0.48, 24.1),
            surveillance(255_000, 0.48),
            doppler(275_000, 0.48, 24.1),
        ])
    }

    #[test]
    fn reflectivity_takes_the_surveillance_leg_of_a_split_cut() {
        let capabilities = analyze(vec![surveillance(0, 0.48), doppler(20_000, 0.48, 24.1)]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
        )
        .expect("reflectivity is present");
        assert_eq!(choice.cut_index, 0);
        assert_eq!(choice.leg, CutLeg::Surveillance);
    }

    #[test]
    fn velocity_takes_the_doppler_leg_of_a_split_cut() {
        let capabilities = analyze(vec![surveillance(0, 0.48), doppler(20_000, 0.48, 24.1)]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("velocity is present");
        assert_eq!(choice.cut_index, 1);
        assert_eq!(choice.leg, CutLeg::Doppler);
    }

    /// The defect this module exists to fix, measured on a real volume.
    #[test]
    fn velocity_takes_the_freshest_scan_of_a_repeated_low_tilt_not_the_first() {
        let capabilities = sails_x3_volume();
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("velocity is present");
        assert_eq!(
            choice.cut_index, 7,
            "the last Doppler sweep of the lowest tilt is the freshest"
        );
        // Three Doppler sweeps scanned this tilt (cuts 1, 5 and 7); two were
        // passed over. The surveillance sweeps of the same tilt carry no
        // velocity and are never candidates.
        assert_eq!(choice.repeats_passed_over, 2);
        // Index order would have taken cut 1, scanned 255 seconds earlier.
        assert_eq!(choice.older_alternative_ms, 255_000);
    }

    #[test]
    fn reflectivity_also_takes_the_freshest_surveillance_scan() {
        let capabilities = sails_x3_volume();
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
        )
        .expect("reflectivity is present");
        assert_eq!(choice.cut_index, 6);
        assert_eq!(choice.leg, CutLeg::Surveillance);
        assert_eq!(choice.older_alternative_ms, 255_000);
    }

    #[test]
    fn a_fresher_doppler_leg_never_beats_the_surveillance_leg_for_reflectivity() {
        // The Doppler sweep is 20 seconds newer but reaches only 300 km. For
        // reflectivity, reach wins: a fresher picture that stops 160 km short
        // is not an improvement.
        let capabilities = analyze(vec![surveillance(0, 0.48), doppler(20_000, 0.48, 24.1)]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Reflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
        )
        .expect("present");
        assert_eq!(choice.leg, CutLeg::Surveillance);
    }

    #[test]
    fn two_doppler_legs_of_one_tilt_are_separated_by_recency_then_nyquist() {
        // Seen on real KRLX and KUDX volumes: two Doppler sweeps of one tilt
        // with 33.2 and 24.0 m/s Nyquist. Recency decides first; Nyquist only
        // breaks a tie between sweeps scanned at the same moment.
        let capabilities = analyze(vec![doppler(0, 0.48, 33.2), doppler(120_000, 0.48, 24.0)]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(choice.cut_index, 1, "the fresher sweep wins on recency");

        let simultaneous = analyze(vec![doppler(0, 0.48, 24.0), doppler(0, 0.48, 33.2)]);
        let choice = select_lowest_tilt(
            &simultaneous,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(
            choice.cut_index, 1,
            "with recency tied, the higher Nyquist wins"
        );
    }

    #[test]
    fn a_moment_the_volume_does_not_carry_selects_nothing() {
        let capabilities = analyze(vec![surveillance(0, 0.48)]);
        assert_eq!(
            select_lowest_tilt(
                &capabilities,
                &MomentType::Velocity,
                CutSelectionPolicy::VelocityLeg
            ),
            None
        );
    }

    #[test]
    fn dual_pol_follows_reflectivity_onto_the_surveillance_leg() {
        let capabilities = analyze(vec![surveillance(0, 0.48), doppler(20_000, 0.48, 24.1)]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::DifferentialReflectivity,
            CutSelectionPolicy::LongestUnfoldedRange,
        )
        .expect("present");
        assert_eq!(choice.cut_index, 0);
    }

    #[test]
    fn the_lowest_tilt_is_the_lowest_measured_elevation_not_the_first_in_the_file() {
        // A SAILS volume lists a higher tilt before a repeat of the lowest one.
        let capabilities = analyze(vec![
            doppler(0, 2.40, 26.0),
            doppler(30_000, 0.48, 26.0),
            doppler(60_000, 1.30, 26.0),
        ]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(choice.cut_index, 1);
    }

    #[test]
    fn the_nearest_elevation_search_picks_a_tilt_then_its_freshest_sweep() {
        let capabilities = sails_x3_volume();
        let choice = select_nearest_elevation(
            &capabilities,
            0.90,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(choice.cut_index, 3, "the 0.88 tilt's only Doppler sweep");
    }

    #[test]
    fn a_target_between_two_tilts_keeps_the_lower_one_rather_than_flipping() {
        // Exact ties happen whenever a target sits halfway between VCP tilts.
        // Resolving them by "strictly nearer" keeps the answer stable instead
        // of depending on which tilt the file listed first.
        let capabilities = analyze(vec![doppler(0, 1.00, 26.0), doppler(30_000, 2.00, 26.0)]);
        let choice = select_nearest_elevation(
            &capabilities,
            1.50,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(choice.cut_index, 0);
    }

    #[test]
    fn a_volume_integration_gets_one_reflectivity_sweep_per_commanded_tilt() {
        // Eight sweeps, two commanded tilts. Integrating all eight would invent
        // six layers of atmosphere that are not there.
        let capabilities = sails_x3_volume();
        let representatives = volume_reflectivity_representatives(
            &capabilities,
            CutSelectionPolicy::LongestUnfoldedRange,
        );
        assert_eq!(representatives.len(), 2);
        let indices: Vec<usize> = representatives
            .iter()
            .map(|choice| choice.cut_index)
            .collect();
        assert_eq!(indices, vec![6, 2], "freshest surveillance sweep per tilt");
    }

    #[test]
    fn representatives_come_back_in_ascending_elevation_order() {
        let capabilities = analyze(vec![
            surveillance(0, 3.00),
            surveillance(30_000, 0.48),
            surveillance(60_000, 1.30),
        ]);
        let representatives = volume_reflectivity_representatives(
            &capabilities,
            CutSelectionPolicy::LongestUnfoldedRange,
        );
        let indices: Vec<usize> = representatives
            .iter()
            .map(|choice| choice.cut_index)
            .collect();
        assert_eq!(indices, vec![1, 2, 0]);
    }

    #[test]
    fn the_ordering_is_total_so_no_choice_depends_on_file_order() {
        // Two sweeps identical in every ranked term except their index. If the
        // comparison left them equal, `max_by` would silently encode "last in
        // the file" as policy. It must not be able to.
        let capabilities = analyze(vec![doppler(0, 0.48, 26.0), doppler(0, 0.48, 26.0)]);
        let left = &capabilities.cuts[0];
        let right = &capabilities.cuts[1];
        assert_ne!(
            compare_candidates(
                left,
                right,
                &MomentType::Velocity,
                CutSelectionPolicy::VelocityLeg
            ),
            std::cmp::Ordering::Equal
        );
    }

    /// A growing sweep must not displace a finished one just by being newer.
    #[test]
    fn a_sweep_still_being_written_does_not_outrank_the_complete_one_before_it() {
        let complete = doppler(0, 0.48, 24.1);
        let mut arriving = doppler(120_000, 0.48, 24.1);
        // Thirty degrees of a new sweep: the freshest data in the file, and
        // not yet a picture.
        arriving.radials.truncate(30);

        let capabilities = analyze(vec![complete, arriving]);
        assert!(
            capabilities.cuts[1].median_radial_time_ms > capabilities.cuts[0].median_radial_time_ms,
            "the fragment really is the newer sweep"
        );
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(
            choice.cut_index, 0,
            "a 30-degree fragment must not replace a complete sweep"
        );
    }

    #[test]
    fn a_sweep_takes_over_once_it_is_most_of_the_way_round() {
        let complete = doppler(0, 0.48, 24.1);
        let mut arriving = doppler(120_000, 0.48, 24.1);
        arriving.radials.truncate(330);

        let capabilities = analyze(vec![complete, arriving]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("present");
        assert_eq!(
            choice.cut_index, 1,
            "330 degrees is a picture worth having, and it is two minutes fresher"
        );
    }

    #[test]
    fn every_sweep_being_partial_still_yields_the_best_available_one() {
        // Early in a volume nothing is complete. Drawing nothing would be worse
        // than drawing the most complete fragment there is.
        let mut first = doppler(0, 0.48, 24.1);
        first.radials.truncate(40);
        let mut second = doppler(30_000, 0.48, 24.1);
        second.radials.truncate(120);

        let capabilities = analyze(vec![first, second]);
        let choice = select_lowest_tilt(
            &capabilities,
            &MomentType::Velocity,
            CutSelectionPolicy::VelocityLeg,
        )
        .expect("a partial sweep is still something to draw");
        assert_eq!(choice.cut_index, 1);
    }

    #[test]
    fn an_empty_volume_selects_nothing_rather_than_panicking() {
        let capabilities = analyze(Vec::new());
        assert_eq!(
            select_lowest_tilt(
                &capabilities,
                &MomentType::Reflectivity,
                CutSelectionPolicy::LongestUnfoldedRange
            ),
            None
        );
        assert!(
            volume_reflectivity_representatives(
                &capabilities,
                CutSelectionPolicy::LongestUnfoldedRange
            )
            .is_empty()
        );
    }
}
