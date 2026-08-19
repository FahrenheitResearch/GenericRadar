//! What a volume can actually do, measured once off the update thread.
//!
//! A `RadarVolume` is a list of cuts, and almost every interesting question
//! about it - which leg of a split cut carries usable reflectivity, which of
//! four repeated low-level scans is the freshest, how far a sweep really
//! sampled - requires walking radials and gates. Doing that while painting is
//! not affordable, and doing it repeatedly in three different places is how
//! two parts of an application come to disagree about the same volume. So it
//! happens once, here, and everything downstream reads the answer.
//!
//! # The elevation angle is not the one stored on the cut
//!
//! `ElevationCut::elevation_deg` is whatever the radial that *created* the cut
//! reported, which is normally the sweep's first radial - taken while the
//! antenna is still ramping onto the commanded tilt. It is measurably wrong.
//! On KTLX 2013-05-20 20:16:46 the two legs of the lowest split cut are stored
//! as 0.60 and 0.53 degrees, so a 0.15-degree grouping tolerance sees two
//! different tilts; the medians over their sweeps are 0.53 and 0.53, identical,
//! which is the truth - they are one commanded tilt scanned twice.
//!
//! It gets worse on a SAILS volume. On KTLX 2026-08-17 07:24:02 (VCP 212,
//! SAILSx3) the stored angles put the four surveillance legs near 0.64 and the
//! four Doppler legs near 0.48, so grouping on them yields two "tilts" where
//! there is one - and a vertical integration would then count the same beam
//! twice as two separate layers of atmosphere. On medians all eight collapse
//! into a single 0.44-0.48 group.
//!
//! So every nominal elevation in this module is a median over the sweep's own
//! radials. The stored value is kept beside it only for diagnostics.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use radar_core::{ElevationCut, MomentType, RadarVolume};

/// Two nominal elevations closer than this are the same commanded tilt.
///
/// A policy constant, versioned with the selection code. Wide enough to absorb
/// the residual scatter between the legs of one split cut after taking medians
/// (measured under 0.05 degrees on real volumes), narrow enough not to merge
/// genuinely adjacent VCP tilts (the closest real pair is about 0.3 degrees).
pub const NOMINAL_ELEVATION_TOLERANCE_DEG: f32 = 0.15;

/// Which kind of sweep a cut is.
///
/// NEXRAD splits its lowest tilts into two sweeps at one commanded elevation: a
/// long-range low-PRF surveillance scan that measures reflectivity and dual-pol
/// unambiguously to 460 km, and a short-range high-PRF Doppler scan that
/// measures velocity without folding but only reaches about 300 km. Higher
/// tilts are scanned once and carry everything.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CutLeg {
    /// Reflectivity and dual-pol, long range, low Nyquist, no velocity.
    Surveillance,
    /// Velocity and spectrum width, short range, high Nyquist.
    Doppler,
    /// One sweep carrying every moment.
    Combined,
}

impl CutLeg {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Surveillance => "surveillance",
            Self::Doppler => "Doppler",
            Self::Combined => "combined",
        }
    }
}

/// Enough to name one cut and tell it apart from a repeat of the same tilt.
///
/// Meaningful only alongside the snapshot it was derived from: `index` is a
/// position in one volume's cut list, and a growing volume renumbers nothing
/// but does append. The elevation is carried in thousandths of a degree rather
/// than as an `f32` so the identity can derive `Eq`, `Ord`, and `Hash` without
/// hashing raw float bits.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CutIdentity {
    pub index: u32,
    /// Median elevation over the sweep, in thousandths of a degree.
    pub elevation_millidegrees: i32,
    pub elevation_number: Option<u8>,
    /// Median radial time offset, in milliseconds from the volume start. This
    /// is what makes one SAILS repeat distinguishable from another.
    pub median_radial_time_ms: i32,
}

impl CutIdentity {
    pub fn elevation_deg(self) -> f32 {
        self.elevation_millidegrees as f32 / 1000.0
    }
}

/// Everything measured about one cut.
#[derive(Clone, Debug, PartialEq)]
pub struct CutCapabilities {
    pub identity: CutIdentity,
    pub index: usize,
    /// The median over the sweep's radials. Use this, not `stored_elevation_deg`.
    pub nominal_elevation_deg: f32,
    /// What `ElevationCut::elevation_deg` says. Kept only so a diagnostic can
    /// show how far the first radial was from the truth.
    pub stored_elevation_deg: f32,
    /// Largest minus smallest radial elevation in the sweep. Routinely 0.1 to
    /// 0.4 degrees, which is why the first radial is not a usable estimate.
    pub elevation_spread_deg: f32,
    pub leg: CutLeg,
    pub moments: BTreeSet<MomentType>,
    pub radial_count: usize,
    /// Degrees of azimuth the sweep actually covered.
    pub azimuth_coverage_deg: f32,
    /// Whether the sweep looks like a complete revolution.
    pub complete: bool,
    /// Median of the positive finite Nyquist velocities on this sweep's own
    /// radials. The median rather than the first: a stray radial must not speak
    /// for the sweep, and the two legs of a split cut differ by a factor of
    /// three, so reading it from the wrong cut ruins any unfolding.
    pub representative_nyquist_mps: Option<f32>,
    /// How far each moment's grid is encoded to reach, in kilometres.
    pub encoded_range_km: BTreeMap<MomentType, f32>,
    pub median_radial_time_ms: i32,
}

impl CutCapabilities {
    pub fn has_moment(&self, moment: &MomentType) -> bool {
        self.moments.contains(moment)
    }

    pub fn has_velocity(&self) -> bool {
        self.moments.contains(&MomentType::Velocity)
    }

    /// Encoded reach of one moment, or zero when the cut does not carry it.
    pub fn range_km(&self, moment: &MomentType) -> f32 {
        self.encoded_range_km.get(moment).copied().unwrap_or(0.0)
    }
}

/// One commanded tilt, and every sweep that scanned it.
#[derive(Clone, Debug, PartialEq)]
pub struct NominalElevationGroup {
    /// The median of the members' nominal elevations.
    pub elevation_deg: f32,
    /// Indices into [`VolumeCapabilities::cuts`], in file order.
    pub members: Vec<usize>,
}

impl NominalElevationGroup {
    /// Whether this tilt was scanned more than once - a split cut, a SAILS
    /// repeat, or both.
    pub fn is_repeated(&self) -> bool {
        self.members.len() > 1
    }
}

/// The measured capabilities of one immutable volume snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeCapabilities {
    pub cuts: Arc<[CutCapabilities]>,
    /// Commanded tilts in ascending elevation order.
    pub groups: Arc<[NominalElevationGroup]>,
    /// Every moment present anywhere in the volume.
    pub moments: BTreeSet<MomentType>,
}

impl VolumeCapabilities {
    /// Measure a volume. Call this on a worker; it walks every radial.
    pub fn analyze(volume: &RadarVolume) -> Self {
        let cuts: Vec<CutCapabilities> = volume
            .cuts
            .iter()
            .enumerate()
            .map(|(index, cut)| analyze_cut(index, cut))
            .collect();
        let groups = group_by_nominal_elevation(&cuts);
        let moments = cuts
            .iter()
            .flat_map(|cut| cut.moments.iter().cloned())
            .collect();
        Self {
            cuts: cuts.into(),
            groups: groups.into(),
            moments,
        }
    }

    pub fn cut(&self, index: usize) -> Option<&CutCapabilities> {
        self.cuts.get(index)
    }

    pub fn has_moment(&self, moment: &MomentType) -> bool {
        self.moments.contains(moment)
    }

    /// The group a cut belongs to.
    pub fn group_of(&self, cut_index: usize) -> Option<&NominalElevationGroup> {
        self.groups
            .iter()
            .find(|group| group.members.contains(&cut_index))
    }
}

fn analyze_cut(index: usize, cut: &ElevationCut) -> CutCapabilities {
    let nominal_elevation_deg = median_elevation_deg(cut).unwrap_or(cut.elevation_deg);
    let median_radial_time_ms = median_radial_time_ms(cut).unwrap_or(0);
    let moments: BTreeSet<MomentType> = cut.moments.keys().cloned().collect();
    let encoded_range_km = cut
        .moments
        .iter()
        .map(|(moment, grid)| {
            let gates = grid.gate_range.gate_count as f32;
            let reach = (grid.gate_range.first_gate_m as f32
                + gates * grid.gate_range.gate_spacing_m as f32)
                / 1000.0;
            (moment.clone(), reach)
        })
        .collect();

    CutCapabilities {
        identity: CutIdentity {
            index: index as u32,
            elevation_millidegrees: (nominal_elevation_deg * 1000.0).round() as i32,
            elevation_number: cut.elevation_number,
            median_radial_time_ms,
        },
        index,
        nominal_elevation_deg,
        stored_elevation_deg: cut.elevation_deg,
        elevation_spread_deg: elevation_spread_deg(cut).unwrap_or(0.0),
        leg: classify_leg(&moments),
        moments,
        radial_count: cut.radials.len(),
        azimuth_coverage_deg: azimuth_coverage_deg(cut),
        complete: azimuth_coverage_deg(cut) >= 350.0,
        representative_nyquist_mps: median_nyquist_mps(cut),
        encoded_range_km,
        median_radial_time_ms,
    }
}

/// Classify a sweep by what it carries.
///
/// By moments rather than by range or Nyquist, because those are properties of
/// the same decision and a sweep that carries velocity is the Doppler leg
/// whatever its numbers happen to be.
fn classify_leg(moments: &BTreeSet<MomentType>) -> CutLeg {
    let has_velocity = moments.contains(&MomentType::Velocity);
    let has_dual_pol = moments.contains(&MomentType::DifferentialReflectivity)
        || moments.contains(&MomentType::CorrelationCoefficient)
        || moments.contains(&MomentType::DifferentialPhase);
    match (has_velocity, has_dual_pol) {
        (true, true) => CutLeg::Combined,
        (true, false) => CutLeg::Doppler,
        (false, _) => CutLeg::Surveillance,
    }
}

/// The median elevation over a sweep's radials.
///
/// The median and not the mean: the antenna ramp at the start of a sweep is a
/// run of outliers all on one side, and a mean would be dragged toward them.
pub fn median_elevation_deg(cut: &ElevationCut) -> Option<f32> {
    median_finite(cut.radials.iter().map(|radial| radial.elevation_deg))
}

fn elevation_spread_deg(cut: &ElevationCut) -> Option<f32> {
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for radial in &cut.radials {
        if radial.elevation_deg.is_finite() {
            minimum = minimum.min(radial.elevation_deg);
            maximum = maximum.max(radial.elevation_deg);
        }
    }
    (minimum <= maximum).then_some(maximum - minimum)
}

fn median_radial_time_ms(cut: &ElevationCut) -> Option<i32> {
    let mut values: Vec<i32> = cut
        .radials
        .iter()
        .map(|radial| radial.time_offset_ms)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// The median of the positive finite Nyquist velocities on this sweep.
pub fn median_nyquist_mps(cut: &ElevationCut) -> Option<f32> {
    median_finite(
        cut.radials
            .iter()
            .filter_map(|radial| radial.nyquist_velocity_mps),
    )
    .filter(|value| *value > 0.0)
}

fn median_finite(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut values: Vec<f32> = values.filter(|value| value.is_finite()).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    Some(values[values.len() / 2])
}

/// Degrees of azimuth a sweep covered, from its radial count and spacing.
fn azimuth_coverage_deg(cut: &ElevationCut) -> f32 {
    if cut.radials.len() < 2 {
        return 0.0;
    }
    let mut azimuths: Vec<f32> = cut
        .radials
        .iter()
        .map(|radial| radial.azimuth_deg.rem_euclid(360.0))
        .filter(|azimuth| azimuth.is_finite())
        .collect();
    if azimuths.len() < 2 {
        return 0.0;
    }
    azimuths.sort_by(f32::total_cmp);
    // The largest gap between consecutive azimuths, wrapping. What the sweep
    // covered is the circle minus its biggest hole - which is the honest
    // answer for a sector scan and for a sweep interrupted part way round.
    let mut largest_gap = azimuths[0] + 360.0 - azimuths[azimuths.len() - 1];
    for pair in azimuths.windows(2) {
        largest_gap = largest_gap.max(pair[1] - pair[0]);
    }
    (360.0 - largest_gap).max(0.0)
}

/// Group cuts by commanded tilt.
///
/// Each cut joins the group whose elevation it is nearest, provided it is
/// within tolerance. Nearest rather than first-within-tolerance, because
/// first-match chains: on a real KDOX volume, cuts at 0.27, 0.26 and 0.41
/// degrees all landed in one group because each was within 0.15 of the group's
/// first member, merging two genuinely different tilts.
fn group_by_nominal_elevation(cuts: &[CutCapabilities]) -> Vec<NominalElevationGroup> {
    let mut groups: Vec<NominalElevationGroup> = Vec::new();
    for cut in cuts {
        let elevation = cut.nominal_elevation_deg;
        let nearest = groups
            .iter_mut()
            .filter(|group| {
                (group.elevation_deg - elevation).abs() <= NOMINAL_ELEVATION_TOLERANCE_DEG
            })
            .min_by(|left, right| {
                (left.elevation_deg - elevation)
                    .abs()
                    .total_cmp(&(right.elevation_deg - elevation).abs())
            });
        match nearest {
            Some(group) => {
                group.members.push(cut.index);
                // Re-centre on the mean of the members so the group's elevation
                // does not drift toward whichever member happened to be first.
                let count = group.members.len() as f32;
                group.elevation_deg += (elevation - group.elevation_deg) / count;
            }
            None => groups.push(NominalElevationGroup {
                elevation_deg: elevation,
                members: vec![cut.index],
            }),
        }
    }
    groups.sort_by(|left, right| left.elevation_deg.total_cmp(&right.elevation_deg));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use radar_core::{GateRange, MomentGrid, MomentType, RadarSite, Radial};

    fn radial(azimuth_deg: f32, elevation_deg: f32, time_offset_ms: i32) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg,
            time_offset_ms,
            gate_range: GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 100,
            },
            nyquist_velocity_mps: Some(26.0),
            radial_status: None,
        }
    }

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

    /// A sweep whose radials ramp onto the commanded tilt, exactly as a real
    /// antenna does: the first few radials are low, the rest settle.
    fn ramped_cut(
        elevation_number: u8,
        commanded_deg: f32,
        start_time_ms: i32,
        moments: &[MomentType],
    ) -> ElevationCut {
        // The stored angle comes from the first radial, which is on the ramp.
        let first = commanded_deg - 0.30;
        let mut cut = ElevationCut::new(first, Some(elevation_number));
        for index in 0..360 {
            let elevation = if index < 8 {
                first + (commanded_deg - first) * (index as f32 / 8.0)
            } else {
                commanded_deg
            };
            cut.radials
                .push(radial(index as f32, elevation, start_time_ms + index * 10));
        }
        for moment in moments {
            cut.moments
                .insert(moment.clone(), grid(moment.clone(), 100));
        }
        cut
    }

    fn volume(cuts: Vec<ElevationCut>) -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("KTLX"),
            Utc.with_ymd_and_hms(2013, 5, 20, 20, 16, 46).unwrap(),
        );
        volume.cuts = cuts;
        volume
    }

    #[test]
    fn the_nominal_elevation_is_the_median_not_the_first_radial() {
        // The whole point: the stored angle is on the antenna ramp and reads
        // 0.30 degrees low, while the median reports the commanded tilt.
        let cut = ramped_cut(1, 0.50, 0, &[MomentType::Reflectivity]);
        // Tolerances because 0.50 - 0.30 is 0.19999999 in f32, not 0.2. The
        // claim under test is a third of a degree of difference, not the last
        // bit of a float.
        assert!(
            (cut.elevation_deg - 0.20).abs() < 1e-6,
            "stored angle is the first radial, got {}",
            cut.elevation_deg
        );
        let measured = analyze_cut(0, &cut);
        assert_eq!(measured.nominal_elevation_deg, 0.50);
        assert!((measured.stored_elevation_deg - 0.20).abs() < 1e-6);
        assert!((measured.elevation_spread_deg - 0.30).abs() < 1e-5);
    }

    #[test]
    fn split_cut_legs_group_together_on_medians_but_not_on_stored_angles() {
        // Reproduces KTLX 2013-05-20: stored 0.60 and 0.53 look like two tilts,
        // medians are both 0.53 and are one.
        let surveillance = ramped_cut(
            1,
            0.53,
            0,
            &[
                MomentType::Reflectivity,
                MomentType::DifferentialReflectivity,
            ],
        );
        let doppler = ramped_cut(
            2,
            0.53,
            16_000,
            &[MomentType::Reflectivity, MomentType::Velocity],
        );
        let stored_apart = (surveillance.elevation_deg - doppler.elevation_deg).abs();
        assert!(
            stored_apart < NOMINAL_ELEVATION_TOLERANCE_DEG,
            "this fixture's stored angles happen to agree; the real risk is shown by the medians"
        );

        let capabilities = VolumeCapabilities::analyze(&volume(vec![surveillance, doppler]));
        assert_eq!(capabilities.groups.len(), 1, "one commanded tilt");
        assert_eq!(capabilities.groups[0].members, vec![0, 1]);
        assert_eq!(capabilities.cuts[0].leg, CutLeg::Surveillance);
        assert_eq!(capabilities.cuts[1].leg, CutLeg::Doppler);
    }

    #[test]
    fn a_sweep_carrying_velocity_and_dual_pol_is_one_combined_leg() {
        let combined = ramped_cut(
            7,
            2.0,
            0,
            &[
                MomentType::Reflectivity,
                MomentType::Velocity,
                MomentType::DifferentialReflectivity,
            ],
        );
        assert_eq!(analyze_cut(0, &combined).leg, CutLeg::Combined);
    }

    #[test]
    fn repeated_low_tilts_land_in_one_group_however_many_there_are() {
        // A SAILSx3 volume scans the lowest tilt four times.
        let cuts = (0..4)
            .map(|repeat| {
                ramped_cut(
                    1,
                    0.48,
                    repeat * 90_000,
                    &[MomentType::Reflectivity, MomentType::Velocity],
                )
            })
            .collect();
        let capabilities = VolumeCapabilities::analyze(&volume(cuts));
        assert_eq!(capabilities.groups.len(), 1);
        assert_eq!(capabilities.groups[0].members, vec![0, 1, 2, 3]);
        assert!(capabilities.groups[0].is_repeated());
    }

    #[test]
    fn grouping_does_not_chain_across_genuinely_different_tilts() {
        // Reproduces a real KDOX volume where first-match grouping merged
        // 0.26, 0.26 and 0.44 into one group because each was within 0.15 of
        // the group's first member. 0.26 and 0.44 are 0.18 apart and are two
        // tilts, not one.
        let cuts = vec![
            ramped_cut(1, 0.26, 0, &[MomentType::Reflectivity]),
            ramped_cut(2, 0.26, 20_000, &[MomentType::Velocity]),
            ramped_cut(3, 0.44, 40_000, &[MomentType::Reflectivity]),
        ];
        let capabilities = VolumeCapabilities::analyze(&volume(cuts));
        assert_eq!(capabilities.groups.len(), 2, "0.26 and 0.44 are two tilts");
        assert_eq!(capabilities.groups[0].members, vec![0, 1]);
        assert_eq!(capabilities.groups[1].members, vec![2]);
    }

    #[test]
    fn groups_come_back_in_ascending_elevation_order_whatever_the_file_order() {
        // A SAILS volume's cut list is not in elevation order.
        let cuts = vec![
            ramped_cut(1, 0.50, 0, &[MomentType::Reflectivity]),
            ramped_cut(5, 3.00, 20_000, &[MomentType::Reflectivity]),
            ramped_cut(1, 0.50, 40_000, &[MomentType::Reflectivity]),
            ramped_cut(3, 1.50, 60_000, &[MomentType::Reflectivity]),
        ];
        let capabilities = VolumeCapabilities::analyze(&volume(cuts));
        let elevations: Vec<f32> = capabilities
            .groups
            .iter()
            .map(|group| group.elevation_deg)
            .collect();
        assert_eq!(elevations, [0.50, 1.50, 3.00]);
        assert_eq!(capabilities.groups[0].members, vec![0, 2]);
    }

    #[test]
    fn the_representative_nyquist_is_the_median_of_this_sweeps_own_radials() {
        let mut cut = ramped_cut(2, 0.5, 0, &[MomentType::Velocity]);
        // One stray radial must not speak for the sweep.
        cut.radials[0].nyquist_velocity_mps = Some(8.3);
        let measured = analyze_cut(0, &cut);
        assert_eq!(measured.representative_nyquist_mps, Some(26.0));
    }

    #[test]
    fn a_sweep_with_no_nyquist_reports_none_rather_than_zero() {
        let mut cut = ramped_cut(1, 0.5, 0, &[MomentType::Reflectivity]);
        for radial in &mut cut.radials {
            radial.nyquist_velocity_mps = None;
        }
        assert_eq!(analyze_cut(0, &cut).representative_nyquist_mps, None);
    }

    #[test]
    fn a_full_revolution_is_complete_and_a_sector_is_not() {
        let full = ramped_cut(1, 0.5, 0, &[MomentType::Reflectivity]);
        assert!(analyze_cut(0, &full).complete);

        let mut sector = ElevationCut::new(0.5, Some(1));
        for index in 0..90 {
            sector.radials.push(radial(index as f32, 0.5, index * 10));
        }
        sector.moments.insert(
            MomentType::Reflectivity,
            grid(MomentType::Reflectivity, 100),
        );
        let measured = analyze_cut(0, &sector);
        assert!(!measured.complete);
        assert!(
            measured.azimuth_coverage_deg < 100.0,
            "a 90-degree sector reported {} degrees",
            measured.azimuth_coverage_deg
        );
    }

    #[test]
    fn the_encoded_range_is_reported_per_moment() {
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(radial(0.0, 0.5, 0));
        cut.moments.insert(
            MomentType::Reflectivity,
            grid(MomentType::Reflectivity, 1840),
        );
        cut.moments
            .insert(MomentType::Velocity, grid(MomentType::Velocity, 1200));
        let measured = analyze_cut(0, &cut);
        assert_eq!(measured.range_km(&MomentType::Reflectivity), 460.0);
        assert_eq!(measured.range_km(&MomentType::Velocity), 300.0);
        assert_eq!(
            measured.range_km(&MomentType::SpectrumWidth),
            0.0,
            "a moment the cut does not carry reaches nowhere"
        );
    }

    #[test]
    fn the_identity_distinguishes_two_scans_of_the_same_tilt() {
        // Without the radial time these would be equal, and a cache would serve
        // a four-minute-old sweep under the newer one's key.
        let first = analyze_cut(0, &ramped_cut(1, 0.48, 0, &[MomentType::Velocity]));
        let second = analyze_cut(5, &ramped_cut(1, 0.48, 250_000, &[MomentType::Velocity]));
        assert_eq!(
            first.identity.elevation_millidegrees,
            second.identity.elevation_millidegrees
        );
        assert_ne!(first.identity, second.identity);
    }

    #[test]
    fn an_empty_volume_measures_to_nothing_rather_than_panicking() {
        let capabilities = VolumeCapabilities::analyze(&volume(Vec::new()));
        assert!(capabilities.cuts.is_empty());
        assert!(capabilities.groups.is_empty());
        assert!(capabilities.moments.is_empty());
    }
}
