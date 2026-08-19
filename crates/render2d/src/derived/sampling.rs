//! The bridge from a decoded volume to a column of reflectivity samples.
//!
//! Everything in [`super::profile`], [`super::reflectivity`], [`super::vil`]
//! and [`super::hail`] is a pure function of a `&[ColumnSample]`. This module
//! is the only place that knows how such a slice is produced from real radar
//! data, and it answers exactly one question: *for this point on the ground,
//! what did each tilt see above it?*
//!
//! # Why one sweep per commanded tilt
//!
//! A volume's cut list is not a column. A VCP 212 volume with SAILSx3 holds
//! eight sweeps of the lowest tilt, and a split cut holds two sweeps of the
//! same beam at once. Integrating all of them would report eight layers of
//! liquid water where the atmosphere has one. So the sampler asks
//! [`volume_reflectivity_representatives`] for a single reflectivity sweep per
//! commanded tilt, and integrates those.
//!
//! # Ground range here is true ground range, and the sweep raster's is not
//!
//! This module converts a Cartesian ground distance to a slant range through
//! the 4/3-earth model ([`crate::beam`]). The CPU sweep raster in the crate
//! root does no such conversion: it plots slant range directly as if it were
//! ground range, applying neither `cos(elevation)` nor earth curvature. The two
//! therefore disagree about where a gate is, by roughly 6 percent of range on
//! a 19.5-degree cut and by a few tens of metres on the 0.5-degree cut. That is
//! a real difference, not a rounding one, and it is deliberately not papered
//! over here: a field built on this sampler is georeferenced, a sweep raster is
//! not, and the two must not be assumed to be pixel-aligned.
//!
//! # Cost
//!
//! [`VolumeSampler::sample_column`] runs a bisection
//! ([`beam::slant_range_for_ground_arc_m`], 60 iterations) once per tilt per
//! point, because the forward ground-arc relation has no convenient closed
//! inverse. On a full-radius kilometre grid - 848 241 cells across some fifteen
//! tilts - that is tens of seconds on one core. It is meant to be driven with
//! `rayon` from a worker, never from the update thread, and the caller should
//! use [`VolumeSampler::max_ground_range_km`] to skip cells no sweep reaches
//! before paying for them.

use product_engine::capabilities::{CutIdentity, VolumeCapabilities};
use product_engine::cut_selection::{CutChoice, volume_reflectivity_representatives};
use product_engine::registry::CutSelectionPolicy;
use product_engine::stats::CellState;
use radar_core::{MomentGrid, MomentStorage, MomentType, RadarVolume};
use thiserror::Error;

use crate::beam;
use crate::derived::grid::GroundPointKm;
use crate::derived::profile::{self, ColumnSample};

/// How far, in degrees of azimuth, the nearest radial may be from the requested
/// bearing before the sampler declares that nothing looked here.
///
/// The failure this prevents: a sector scan, or a sweep that lost a run of
/// radials, has real holes in it. Without a limit the nearest-radial search
/// always returns *some* radial, so a 90-degree sector sweep would smear its
/// edge radial around the entire circle and a VIL field would show a storm
/// where the antenna never pointed.
///
/// Two degrees is about twice the 0.95-degree WSR-88D beamwidth and twice the
/// 1.0-degree legacy radial spacing, so it bridges a couple of dropped radials
/// without inventing a sector. It is deliberately tighter than the 3.0 degrees
/// the sweep raster allows, because the raster is drawing wedges an analyst can
/// see and this is feeding an integration nobody inspects gate by gate.
pub const MAX_AZIMUTH_OFFSET_DEG: f32 = 2.0;

/// What is written into the value slot of a sample that carries no value.
///
/// Zero rather than NaN, for the reason [`super::reflectivity`] gives: the slot
/// must not be read when the state says so, but a mistaken read should produce
/// a wrong number rather than a NaN that poisons a min/max reduction over the
/// whole field.
const UNREADABLE_VALUE: f32 = 0.0;

/// Why a volume could not be prepared for column sampling.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SamplerError {
    /// No cut in the volume carries reflectivity. A velocity-only file, or a
    /// volume caught mid-decode before its first surveillance sweep landed.
    #[error("the volume has no reflectivity sweep to sample")]
    NoReflectivityCuts,
    /// Reflectivity sweeps exist but none has usable gate geometry: no gates, a
    /// non-positive gate spacing, or no rows in the moment grid. Reported
    /// rather than silently producing an empty column, because an empty column
    /// reduces to `NoCoverage` everywhere and on screen that is
    /// indistinguishable from clear air.
    #[error("the volume's reflectivity sweeps have no usable gate geometry")]
    NoUsableGeometry,
}

/// One tilt, prepared for repeated point queries.
///
/// Everything expensive that depends only on the sweep - the gate geometry, the
/// measured elevation, and the azimuth lookup - is computed once here so that
/// [`VolumeSampler::sample_column`] does no allocation and no sweep-wide scan.
#[derive(Clone, Debug)]
struct TiltSampler {
    cut_index: usize,
    identity: CutIdentity,
    /// The **measured** nominal elevation from [`VolumeCapabilities`], which is
    /// the median over the sweep's own radials.
    ///
    /// Not `ElevationCut::elevation_deg`: that is the angle the sweep's first
    /// radial reported, taken while the antenna is still ramping onto the tilt,
    /// and it is wrong by up to half a degree on real volumes. Half a degree at
    /// 100 km is about 900 m of beam height, which is the difference between a
    /// storm top above and below the freezing level.
    nominal_elevation_deg: f64,
    first_gate_m: f64,
    gate_spacing_m: f64,
    gate_count: usize,
    /// Slant range to the centre of the last gate.
    max_slant_range_m: f64,
    /// Ground arc under the centre of the last gate.
    max_ground_range_m: f64,
    /// `(azimuth, moment-grid row)` for every row of this sweep's reflectivity
    /// grid, sorted ascending by azimuth.
    ///
    /// A sorted array with a binary search rather than a fixed-bin table, for
    /// two reasons. It is exact at any radial spacing - super-resolution sweeps
    /// are 0.5 degrees apart and legacy ones 1.0, and a bin width tuned for one
    /// quietly degrades the other - and it needs no policy for an empty bin, so
    /// a sector gap falls out of the nearest-neighbour distance rather than out
    /// of a table-filling rule. The cost is about ten comparisons per tilt per
    /// point against the 720 a linear scan would need, which over 848 241 cells
    /// and fifteen tilts is the difference between ten million comparisons and
    /// nine billion.
    ///
    /// Built over grid **rows**, not over radials: a radial with no row in this
    /// moment's grid is simply never a candidate, so the "this radial has no
    /// row" case is structural rather than a check that can be forgotten - and
    /// a neighbouring radial that *does* have a row is still found, instead of
    /// the point reporting no coverage because the closest radial happened to
    /// carry no reflectivity.
    rows_by_azimuth: Vec<(f32, u32)>,
}

impl TiltSampler {
    fn prepare(
        volume: &RadarVolume,
        capabilities: &VolumeCapabilities,
        choice: &CutChoice,
    ) -> Option<Self> {
        let cut = volume.cuts.get(choice.cut_index)?;
        let grid = cut.moments.get(&MomentType::Reflectivity)?;
        let gate_count = grid.gate_range.gate_count;
        if gate_count == 0 || grid.gate_range.gate_spacing_m <= 0 {
            return None;
        }
        let nominal_elevation_deg =
            f64::from(capabilities.cut(choice.cut_index)?.nominal_elevation_deg);
        if !nominal_elevation_deg.is_finite() {
            return None;
        }

        let first_gate_m = f64::from(grid.gate_range.first_gate_m);
        let gate_spacing_m = f64::from(grid.gate_range.gate_spacing_m);
        // Gate g is CENTRED at first_gate + g * spacing, so the last gate's
        // centre is at (gate_count - 1) spacings, not gate_count. Using
        // gate_count here would let a ground range half a gate beyond the sweep
        // round back onto the last gate and report an echo the radar never
        // measured.
        let max_slant_range_m = first_gate_m + (gate_count - 1) as f64 * gate_spacing_m;
        if max_slant_range_m <= 0.0 {
            return None;
        }
        let max_ground_range_m = beam::ground_arc_m(max_slant_range_m, nominal_elevation_deg);

        let mut rows_by_azimuth: Vec<(f32, u32)> = grid
            .radial_indices
            .iter()
            .enumerate()
            .filter_map(|(row, radial_index)| {
                let radial = cut.radials.get(*radial_index)?;
                let azimuth = radial.azimuth_deg;
                azimuth
                    .is_finite()
                    .then(|| (azimuth.rem_euclid(360.0), row as u32))
            })
            .collect();
        if rows_by_azimuth.is_empty() {
            return None;
        }
        // A total order, including the row index, so two rows recorded at the
        // same azimuth resolve to the earlier row every time rather than to
        // whichever the sort happened to leave first.
        rows_by_azimuth.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });

        Some(Self {
            cut_index: choice.cut_index,
            identity: choice.identity,
            nominal_elevation_deg,
            first_gate_m,
            gate_spacing_m,
            gate_count,
            max_slant_range_m,
            max_ground_range_m,
            rows_by_azimuth,
        })
    }

    /// The moment-grid row nearest a compass bearing, wrapping across 0/360.
    ///
    /// Returns `None` when the nearest radial is further than
    /// [`MAX_AZIMUTH_OFFSET_DEG`] away.
    fn row_for_bearing(&self, bearing_deg: f32) -> Option<u32> {
        let bearing_deg = bearing_deg.rem_euclid(360.0);
        let count = self.rows_by_azimuth.len();
        // The first entry at or above the bearing. When the bearing sits above
        // every entry this is `count`, and wrapping it round to 0 is what makes
        // a bearing of 359.9 find the radial at 0.1 rather than only the one at
        // 359.0 - the case a plain `abs(a - b)` comparison gets wrong by 359
        // degrees.
        let insertion = self
            .rows_by_azimuth
            .partition_point(|(azimuth, _)| *azimuth < bearing_deg);
        let above = insertion % count;
        let below = (insertion + count - 1) % count;

        let (above_azimuth, above_row) = self.rows_by_azimuth[above];
        let (below_azimuth, below_row) = self.rows_by_azimuth[below];
        let above_offset = angular_separation_deg(bearing_deg, above_azimuth);
        let below_offset = angular_separation_deg(bearing_deg, below_azimuth);
        let (offset, row) = if above_offset <= below_offset {
            (above_offset, above_row)
        } else {
            (below_offset, below_row)
        };
        (offset <= MAX_AZIMUTH_OFFSET_DEG).then_some(row)
    }

    /// A sample this tilt could not provide.
    ///
    /// It still carries a height, taken at whatever slant range the sampler got
    /// to before giving up, so the entry sorts into a physically sensible place
    /// in the column. Without one, an uncovered tilt would land at the bottom
    /// or the top of the profile and the vertical algorithms - which use
    /// [`ColumnSample::is_covered`] to decide whether a gap may be integrated
    /// across - would look for the hole in the wrong place.
    fn no_coverage(&self, slant_range_m: f64) -> ColumnSample {
        ColumnSample {
            cut_index: self.cut_index,
            elevation_deg: self.nominal_elevation_deg as f32,
            height_arl_m: beam::beam_height_arl_m(slant_range_m, self.nominal_elevation_deg) as f32,
            slant_range_m: slant_range_m as f32,
            reflectivity_dbz: UNREADABLE_VALUE,
            state: CellState::NoCoverage,
        }
    }

    fn sample(&self, volume: &RadarVolume, bearing_deg: f32, ground_range_m: f64) -> ColumnSample {
        // Cheap rejection before the bisection. Exactly equivalent to the
        // `None` that `slant_range_for_ground_arc_m` would return, because it
        // makes the same comparison against the same maximum; this only avoids
        // paying for that comparison's 4/3-earth arithmetic twice.
        if !ground_range_m.is_finite() || ground_range_m > self.max_ground_range_m {
            return self.no_coverage(self.max_slant_range_m);
        }
        let Some(slant_range_m) = beam::slant_range_for_ground_arc_m(
            ground_range_m,
            self.nominal_elevation_deg,
            self.max_slant_range_m,
        ) else {
            return self.no_coverage(self.max_slant_range_m);
        };

        // Gate g is centred at first_gate + g * spacing. NOT at
        // (g + 0.5) * spacing: the half-gate idiom shifts every readout by half
        // a gate, and on a 250 m sweep with a 2125 m first gate it does not
        // merely shift the answer, it lands on an entirely different gate.
        let gate_offset = (slant_range_m - self.first_gate_m) / self.gate_spacing_m;
        let gate_index = gate_offset.round();
        if !gate_index.is_finite() || gate_index < 0.0 || gate_index >= self.gate_count as f64 {
            return self.no_coverage(slant_range_m);
        }
        let gate_index = gate_index as usize;

        let Some(row) = self.row_for_bearing(bearing_deg) else {
            return self.no_coverage(slant_range_m);
        };
        // A volume other than the one prepared from, or one that grew since,
        // must degrade to "the radar did not sample here" rather than panic.
        let Some(cut) = volume.cuts.get(self.cut_index) else {
            return self.no_coverage(slant_range_m);
        };
        let Some(grid) = cut.moments.get(&MomentType::Reflectivity) else {
            return self.no_coverage(slant_range_m);
        };
        if row as usize >= grid.radial_count() || gate_index >= grid.gate_range.gate_count {
            return self.no_coverage(slant_range_m);
        }

        let (state, reflectivity_dbz) = read_gate(grid, row as usize, gate_index);
        ColumnSample {
            cut_index: self.cut_index,
            elevation_deg: self.nominal_elevation_deg as f32,
            height_arl_m: beam::beam_height_arl_m(slant_range_m, self.nominal_elevation_deg) as f32,
            slant_range_m: slant_range_m as f32,
            reflectivity_dbz,
            state,
        }
    }
}

/// The smaller of the two ways round the circle between two bearings.
fn angular_separation_deg(left_deg: f32, right_deg: f32) -> f32 {
    let difference = (left_deg - right_deg).abs().rem_euclid(360.0);
    difference.min(360.0 - difference)
}

/// The stored code at one gate, before scaling.
///
/// `MomentGrid::scaled_value` answers `None` for a range-folded gate and for a
/// nodata gate alike, so the raw code is the only way to tell a gate the sweep
/// raster paints purple from a gate that holds nothing.
///
/// `F32` storage holds physical values rather than encoded codes, so it has no
/// sentinel to compare against and answers `None`.
fn raw_code(grid: &MomentGrid, row_index: usize, gate_index: usize) -> Option<u16> {
    if gate_index >= grid.gate_range.gate_count {
        return None;
    }
    let index = row_index
        .checked_mul(grid.gate_range.gate_count)?
        .checked_add(gate_index)?;
    match &grid.storage {
        MomentStorage::U8(values) => values.get(index).map(|value| u16::from(*value)),
        MomentStorage::U16(values) => values.get(index).copied(),
        MomentStorage::F32(_) => None,
    }
}

/// Classify one gate.
///
/// Range folded is tested first, and against the raw code, because
/// `scaled_value` collapses folded and nodata into the same `None`. Reading it
/// alone would report a gate the sweep raster paints purple as an absence, and
/// a VIL integration would then treat a column of ambiguous range as clear air.
///
/// The absence case answers [`CellState::NoEcho`] and not [`CellState::NoData`]
/// because the two cannot currently be told apart: `MomentGrid` pads short rows
/// with the nodata code and keeps no per-row original length, so a gate the
/// radar sampled and found empty is byte-identical to a gate past the end of a
/// short radial. `NoEcho` is the conservative choice - it keeps `is_covered`
/// true, so the vertical algorithms may integrate across the gate instead of
/// declaring a hole in the profile and refusing to integrate at all.
fn read_gate(grid: &MomentGrid, row_index: usize, gate_index: usize) -> (CellState, f32) {
    if let Some(folded) = grid.range_folded
        && raw_code(grid, row_index, gate_index) == Some(folded)
    {
        return (CellState::RangeFolded, UNREADABLE_VALUE);
    }
    match grid.scaled_value(row_index, gate_index) {
        Some(value) if value.is_finite() => (CellState::Valid, value),
        _ => (CellState::NoEcho, UNREADABLE_VALUE),
    }
}

/// One volume, prepared for repeated column queries.
#[derive(Clone, Debug)]
pub struct VolumeSampler {
    tilts: Vec<TiltSampler>,
    selected_cuts: Vec<CutIdentity>,
    max_ground_range_km: f32,
}

impl VolumeSampler {
    /// Prepare once per volume, on a worker.
    ///
    /// Selects one reflectivity sweep per commanded tilt via
    /// [`volume_reflectivity_representatives`], so a SAILS volume's repeated
    /// low tilts are not integrated more than once.
    ///
    /// The policy is [`CutSelectionPolicy::LongestUnfoldedRange`]: for
    /// reflectivity the surveillance leg of a split cut reaches about 160 km
    /// further than the Doppler leg, and a vertical product wants every
    /// kilometre of that.
    pub fn prepare(
        volume: &RadarVolume,
        capabilities: &VolumeCapabilities,
    ) -> Result<Self, SamplerError> {
        let representatives = volume_reflectivity_representatives(
            capabilities,
            CutSelectionPolicy::LongestUnfoldedRange,
        );
        if representatives.is_empty() {
            return Err(SamplerError::NoReflectivityCuts);
        }

        let tilts: Vec<TiltSampler> = representatives
            .iter()
            .filter_map(|choice| TiltSampler::prepare(volume, capabilities, choice))
            .collect();
        if tilts.is_empty() {
            return Err(SamplerError::NoUsableGeometry);
        }

        let selected_cuts = tilts.iter().map(|tilt| tilt.identity).collect();
        let max_ground_range_km = tilts
            .iter()
            .map(|tilt| (tilt.max_ground_range_m / 1000.0) as f32)
            .fold(0.0_f32, f32::max);
        Ok(Self {
            tilts,
            selected_cuts,
            max_ground_range_km,
        })
    }

    /// Ground range, in kilometres, beyond which no selected sweep has gates.
    ///
    /// True ground range under the 4/3-earth model, not slant range. A caller
    /// building an analysis grid should size it from this rather than from the
    /// encoded gate reach, and should skip cells beyond it rather than paying
    /// for a bisection that can only answer `NoCoverage`.
    pub fn max_ground_range_km(&self) -> f32 {
        self.max_ground_range_km
    }

    /// Number of tilts contributing.
    pub fn tilt_count(&self) -> usize {
        self.tilts.len()
    }

    /// The cuts chosen, for provenance in a readout.
    pub fn selected_cuts(&self) -> &[CutIdentity] {
        &self.selected_cuts
    }

    /// Fill `scratch` with one [`ColumnSample`] per contributing tilt, sorted
    /// ascending by beam height and with at most one entry per nominal
    /// elevation.
    ///
    /// Clears `scratch` first. It takes the caller's allocation because it is
    /// called once per grid cell - 848 241 times for a full-radius kilometre
    /// grid - and a fresh `Vec` per call would put a heap allocation and a free
    /// in the innermost loop of the run.
    ///
    /// `volume` must be the volume [`VolumeSampler::prepare`] was given. A
    /// different one degrades to `NoCoverage` rather than panicking, but the
    /// answer is meaningless.
    pub fn sample_column(
        &self,
        volume: &RadarVolume,
        point: GroundPointKm,
        scratch: &mut Vec<ColumnSample>,
    ) {
        scratch.clear();
        // Compass bearing: atan2(east, north). The mathematical atan2(north,
        // east) would mirror the whole field about the 45-degree line, which on
        // a nearly symmetric storm looks almost right.
        let bearing_deg = beam::compass_azimuth_deg(point.east_km, point.north_km) as f32;
        let ground_range_m = point.range_km() * 1000.0;
        for tilt in &self.tilts {
            scratch.push(tilt.sample(volume, bearing_deg, ground_range_m));
        }
        profile::normalize_column(scratch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use radar_core::{ElevationCut, GateRange, RadarSite, Radial};

    /// The nodata code of the synthetic grids.
    const NODATA_CODE: u8 = 0;
    /// The range-folded code of the synthetic grids.
    const RANGE_FOLDED_CODE: u8 = 1;
    const GATE_COUNT: usize = 40;
    /// Deliberately not zero. With a first gate at the origin the correct
    /// centring rule `first + g * spacing` and the wrong half-gate idiom
    /// `(g + 0.5) * spacing` differ by only half a gate; with a real NEXRAD
    /// first-gate offset they land on completely different gates, so a test
    /// built on this constant cannot pass under the wrong rule.
    const FIRST_GATE_M: i32 = 2_125;
    const GATE_SPACING_M: i32 = 250;
    /// Centre of the last gate: 2125 + 39 * 250.
    const MAX_SLANT_RANGE_M: f64 = 11_875.0;

    /// The stored code that decodes to exactly `gate` dBZ under scale 2,
    /// offset 66. Every gate therefore reads back as its own index, so a
    /// readout that is off by one gate is off by exactly one dBZ and the
    /// assertion names the defect.
    fn code_for_gate(gate: usize) -> u8 {
        66 + 2 * gate as u8
    }

    /// Radial 0 (due north) is offset by +20 dBZ and radial 90 (due east) by
    /// +40 dBZ. A sample that took the wrong radial, or read a compass bearing
    /// as a mathematical angle, therefore reports a value wrong by a
    /// recognisable amount rather than by an amount that could be noise.
    fn marked_row(radial_index: usize) -> Vec<u8> {
        let bonus: u8 = match radial_index {
            0 => 40,
            90 => 80,
            _ => 0,
        };
        (0..GATE_COUNT)
            .map(|gate| code_for_gate(gate) + bonus)
            .collect()
    }

    fn plain_row(_radial_index: usize) -> Vec<u8> {
        (0..GATE_COUNT).map(code_for_gate).collect()
    }

    fn gate_range() -> GateRange {
        GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        }
    }

    /// A reflectivity-only sweep of 360 radials at whole degrees.
    ///
    /// The first six radials are stored 0.4 degrees below the commanded tilt,
    /// as a real antenna reports while it is still ramping on, and the cut's
    /// own `elevation_deg` carries that same ramping value. Anything that reads
    /// the stored angle instead of the measured median is therefore wrong by
    /// 0.4 degrees, and the height tests catch it.
    fn sweep(
        commanded_deg: f32,
        elevation_number: u8,
        time_ms: i32,
        row_for: impl Fn(usize) -> Vec<u8>,
    ) -> ElevationCut {
        let ramping_deg = commanded_deg - 0.4;
        let mut cut = ElevationCut::new(ramping_deg, Some(elevation_number));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range(),
            2.0,
            66.0,
            Some(NODATA_CODE),
            Some(RANGE_FOLDED_CODE),
        );
        for index in 0..360usize {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: if index < 6 {
                    ramping_deg
                } else {
                    commanded_deg
                },
                time_offset_ms: time_ms + index as i32 * 10,
                gate_range: gate_range(),
                nyquist_velocity_mps: Some(26.0),
                radial_status: None,
            });
            grid.push_u8_row_slice(index, &row_for(index))
                .expect("a u8 row belongs in a u8 grid");
        }
        cut.moments.insert(MomentType::Reflectivity, grid);
        cut
    }

    fn volume_of(cuts: Vec<ElevationCut>) -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("TST"),
            Utc.with_ymd_and_hms(2026, 8, 17, 7, 24, 2)
                .single()
                .expect("a real instant"),
        );
        volume.cuts = cuts;
        volume
    }

    fn sampler_for(volume: &RadarVolume) -> VolumeSampler {
        let capabilities = VolumeCapabilities::analyze(volume);
        VolumeSampler::prepare(volume, &capabilities).expect("a reflectivity volume must prepare")
    }

    /// The ground point that puts the beam of `elevation_deg` at the centre of
    /// `gate`, on a given compass bearing.
    fn point_at_gate(gate: usize, elevation_deg: f64, bearing_deg: f64) -> GroundPointKm {
        let slant_range_m = f64::from(FIRST_GATE_M) + gate as f64 * f64::from(GATE_SPACING_M);
        point_at_slant(slant_range_m, elevation_deg, bearing_deg)
    }

    fn point_at_slant(slant_range_m: f64, elevation_deg: f64, bearing_deg: f64) -> GroundPointKm {
        let ground_km = beam::ground_arc_m(slant_range_m, elevation_deg) / 1000.0;
        let bearing = bearing_deg.to_radians();
        GroundPointKm::new(ground_km * bearing.sin(), ground_km * bearing.cos())
    }

    fn column(
        sampler: &VolumeSampler,
        volume: &RadarVolume,
        point: GroundPointKm,
    ) -> Vec<ColumnSample> {
        let mut scratch = Vec::new();
        sampler.sample_column(volume, point, &mut scratch);
        scratch
    }

    #[test]
    fn a_point_due_north_is_sampled_from_the_radial_at_bearing_zero() {
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, point_at_gate(3, 0.5, 0.0));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].state, CellState::Valid);
        // Gate 3 reads 3 dBZ, and the +20 dBZ marker says it came from radial 0.
        assert_eq!(samples[0].reflectivity_dbz, 23.0);
    }

    #[test]
    fn a_point_due_east_is_sampled_from_the_radial_at_ninety_degrees_not_at_zero() {
        // Under the mathematical atan2(north, east) convention due east is
        // bearing 0, so a mirrored sampler reads the north marker and answers
        // 23.0 here.
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, point_at_gate(3, 0.5, 90.0));
        assert_eq!(samples[0].state, CellState::Valid);
        assert_eq!(
            samples[0].reflectivity_dbz, 43.0,
            "due east must read the +40 dBZ marker on radial 90"
        );
    }

    #[test]
    fn a_gate_is_centred_on_first_gate_plus_index_times_spacing() {
        // Gate 7's centre is 2125 + 7 * 250 = 3875 m. Under the half-gate idiom
        // (g + 0.5) * spacing, a slant range of 3875 m would be read as gate
        // round(3875 / 250 - 0.5) = 15, which decodes to 15 dBZ, not 7.
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, point_at_gate(7, 0.5, 0.0));
        assert_eq!(samples[0].state, CellState::Valid);
        assert_eq!(samples[0].reflectivity_dbz, 27.0, "gate 7 plus the marker");
    }

    #[test]
    fn a_point_within_half_a_gate_of_a_centre_rounds_onto_that_gate() {
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        let centre_m = f64::from(FIRST_GATE_M) + 7.0 * f64::from(GATE_SPACING_M);

        for offset_m in [-120.0_f64, -1.0, 1.0, 120.0] {
            let point = point_at_slant(centre_m + offset_m, 0.5, 0.0);
            let samples = column(&sampler, &volume, point);
            assert_eq!(
                samples[0].reflectivity_dbz, 27.0,
                "{offset_m} m from the centre of gate 7 must still read gate 7"
            );
        }
        // 0.52 of a gate away is nearer the next centre, and must move.
        let samples = column(
            &sampler,
            &volume,
            point_at_slant(centre_m + 130.0, 0.5, 0.0),
        );
        assert_eq!(
            samples[0].reflectivity_dbz, 28.0,
            "130 m past the centre of gate 7 is inside gate 8"
        );
    }

    #[test]
    fn a_point_beyond_every_sweep_reports_no_coverage_on_every_tilt() {
        // These sweeps stop at 11.875 km of slant range; 50 km is far outside.
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(4.5, 2, 60_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, 50.0));
        assert_eq!(samples.len(), 2);
        for sample in &samples {
            assert_eq!(sample.state, CellState::NoCoverage);
            assert!(!sample.is_covered());
            assert_eq!(sample.value(), None);
        }
    }

    #[test]
    fn a_higher_tilt_samples_a_higher_beam_over_the_same_ground_point() {
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(4.5, 2, 60_000, plain_row),
            sweep(9.9, 3, 120_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        assert_eq!(sampler.tilt_count(), 3);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, 8.0));
        assert_eq!(samples.len(), 3);
        for sample in &samples {
            assert_eq!(sample.state, CellState::Valid);
        }
        assert_eq!(
            samples.iter().map(|s| s.elevation_deg).collect::<Vec<_>>(),
            [0.5, 4.5, 9.9],
            "the column must arrive in ascending height order, which here is \
             ascending tilt order"
        );
        // 8 km of ground range at 0.5, 4.5 and 9.9 degrees over the 4/3 earth.
        let heights: Vec<f32> = samples.iter().map(|s| s.height_arl_m).collect();
        assert!(
            (heights[0] - 73.6).abs() < 1.5,
            "0.5 deg at 8 km was {} m",
            heights[0]
        );
        assert!(
            (heights[1] - 633.5).abs() < 3.0,
            "4.5 deg at 8 km was {} m",
            heights[1]
        );
        assert!(
            (heights[2] - 1_398.8).abs() < 10.0,
            "9.9 deg at 8 km was {} m",
            heights[2]
        );
        assert!(heights[0] < heights[1] && heights[1] < heights[2]);
    }

    #[test]
    fn the_measured_tilt_is_used_for_height_not_the_ramping_first_radial() {
        // The cut stores 4.1 degrees - its first radial, taken while the
        // antenna was still ramping - and the median over the sweep is 4.5. At
        // 8 km that is over 50 m of beam height, and at 100 km it would be 700.
        let volume = volume_of(vec![sweep(4.5, 1, 0, plain_row)]);
        assert_eq!(volume.cuts[0].elevation_deg, 4.1);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, 8.0));
        assert_eq!(samples[0].elevation_deg, 4.5);
        let from_stored = beam::beam_height_arl_m(f64::from(samples[0].slant_range_m), 4.1) as f32;
        assert!(
            (samples[0].height_arl_m - from_stored).abs() > 40.0,
            "the stored angle would have given {from_stored} m, the measured \
             one gave {}",
            samples[0].height_arl_m
        );
    }

    #[test]
    fn a_range_folded_gate_reports_folded_and_not_no_echo() {
        // scaled_value answers None for a folded gate and for a nodata gate
        // alike, so a sampler that consulted only it would call a gate the
        // sweep raster paints purple an absence.
        let folded = |radial_index: usize| {
            let mut row = marked_row(radial_index);
            if radial_index == 0 {
                row[5] = RANGE_FOLDED_CODE;
            }
            row
        };
        let volume = volume_of(vec![sweep(0.5, 1, 0, folded)]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, point_at_gate(5, 0.5, 0.0));
        assert_eq!(samples[0].state, CellState::RangeFolded);
        assert_eq!(samples[0].value(), None);
        assert!(
            samples[0].is_covered(),
            "a folded gate was sampled; it is not a hole in the sweep"
        );
    }

    #[test]
    fn a_nodata_gate_reports_no_echo_because_padding_cannot_be_told_from_silence() {
        let empty = |radial_index: usize| {
            let mut row = marked_row(radial_index);
            if radial_index == 0 {
                row[5] = NODATA_CODE;
            }
            row
        };
        let volume = volume_of(vec![sweep(0.5, 1, 0, empty)]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, point_at_gate(5, 0.5, 0.0));
        assert_eq!(samples[0].state, CellState::NoEcho);
        assert!(samples[0].is_covered());
    }

    #[test]
    fn a_bearing_just_west_of_north_wraps_round_to_the_radial_at_zero() {
        // Bearing 359.8. The nearest radial is 0, at 0.2 degrees away; the
        // nearest one *below* is 359, at 0.8 degrees. A search that compared
        // raw azimuths without wrapping would answer 359.
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        let point = point_at_gate(3, 0.5, 359.8);
        assert!(
            (beam::compass_azimuth_deg(point.east_km, point.north_km) - 359.8).abs() < 1e-6,
            "the test point must actually be at bearing 359.8"
        );
        let samples = column(&sampler, &volume, point);
        assert_eq!(
            samples[0].reflectivity_dbz, 23.0,
            "gate 3 on the +20 dBZ marked radial at bearing 0"
        );
    }

    #[test]
    fn a_bearing_just_east_of_north_stays_on_the_radial_at_zero() {
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, point_at_gate(3, 0.5, 0.2));
        assert_eq!(samples[0].reflectivity_dbz, 23.0);
    }

    #[test]
    fn a_bearing_inside_a_sector_gap_reports_no_coverage_rather_than_the_edge_radial() {
        // A 91-radial sector sweep from 0 to 90 degrees. Bearing 180 is 90
        // degrees from the nearest radial; without the azimuth tolerance the
        // nearest-neighbour search would hand back radial 90 and smear the
        // sector's edge right round the circle.
        let mut cut = sweep(0.5, 1, 0, marked_row);
        cut.radials.truncate(91);
        let grid = cut
            .moments
            .get_mut(&MomentType::Reflectivity)
            .expect("the sweep carries reflectivity");
        grid.radial_indices.truncate(91);
        let MomentStorage::U8(values) = &mut grid.storage else {
            panic!("the synthetic sweep is u8");
        };
        values.truncate(91 * GATE_COUNT);

        let volume = volume_of(vec![cut]);
        let sampler = sampler_for(&volume);

        let inside = column(&sampler, &volume, point_at_gate(3, 0.5, 45.0));
        assert_eq!(inside[0].state, CellState::Valid);
        assert_eq!(inside[0].reflectivity_dbz, 3.0);

        let just_past_the_edge = column(&sampler, &volume, point_at_gate(3, 0.5, 91.5));
        assert_eq!(
            just_past_the_edge[0].state,
            CellState::Valid,
            "1.5 degrees past the last radial is inside the 2.0 degree tolerance"
        );

        let in_the_gap = column(&sampler, &volume, point_at_gate(3, 0.5, 180.0));
        assert_eq!(in_the_gap[0].state, CellState::NoCoverage);
    }

    #[test]
    fn repeated_scans_of_one_tilt_contribute_a_single_sample() {
        // Three 0.5-degree sweeps, as a SAILS volume has, and one 4.5.
        // Integrating all four would report four layers of atmosphere where
        // there are two.
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(4.5, 2, 60_000, plain_row),
            sweep(0.5, 1, 120_000, plain_row),
            sweep(0.5, 1, 240_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        assert_eq!(sampler.tilt_count(), 2, "four sweeps, two commanded tilts");
        assert_eq!(sampler.selected_cuts().len(), 2);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, 8.0));
        assert_eq!(samples.len(), 2);
        assert_eq!(
            samples.iter().map(|s| s.elevation_deg).collect::<Vec<_>>(),
            [0.5, 4.5]
        );
    }

    #[test]
    fn the_freshest_repeat_of_a_tilt_is_the_one_selected() {
        // The whole point of SAILS: cut 2 is the last 0.5-degree sweep in the
        // file and 240 seconds fresher than cut 0.
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(0.5, 1, 120_000, plain_row),
            sweep(0.5, 1, 240_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        assert_eq!(sampler.tilt_count(), 1);
        assert_eq!(sampler.selected_cuts()[0].index, 2);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, 8.0));
        assert_eq!(samples[0].cut_index, 2);
    }

    #[test]
    fn the_column_is_sorted_by_height_even_when_the_cut_list_is_not() {
        // File order 9.9, 0.5, 4.5 - which is what a SAILS volume's cut list
        // looks like once the repeats are stripped out.
        let volume = volume_of(vec![
            sweep(9.9, 3, 0, plain_row),
            sweep(0.5, 1, 60_000, plain_row),
            sweep(4.5, 2, 120_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, 8.0));
        assert_eq!(
            samples.iter().map(|s| s.cut_index).collect::<Vec<_>>(),
            [1, 2, 0]
        );
        for pair in samples.windows(2) {
            assert!(
                pair[0].height_arl_m <= pair[1].height_arl_m,
                "column out of height order: {samples:?}"
            );
        }
    }

    #[test]
    fn the_ground_reach_is_the_arc_under_the_last_gate_not_its_slant_range() {
        let volume = volume_of(vec![sweep(0.5, 1, 0, plain_row)]);
        let sampler = sampler_for(&volume);
        let expected_km = beam::ground_arc_m(MAX_SLANT_RANGE_M, 0.5) / 1000.0;
        assert!(
            (f64::from(sampler.max_ground_range_km()) - expected_km).abs() < 1e-3,
            "reach was {} km, the arc under the last gate is {expected_km} km",
            sampler.max_ground_range_km()
        );
        assert!(
            sampler.max_ground_range_km() < 11.875,
            "the ground arc must fall short of the 11.875 km slant range"
        );
    }

    #[test]
    fn the_reach_is_taken_from_the_tilt_that_gets_furthest_across_the_ground() {
        // The steep tilt runs out of ground range first even though both sweeps
        // have the same number of gates.
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(19.5, 2, 60_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        let shallow_km = beam::ground_arc_m(MAX_SLANT_RANGE_M, 0.5) / 1000.0;
        let steep_km = beam::ground_arc_m(MAX_SLANT_RANGE_M, 19.5) / 1000.0;
        assert!(
            steep_km < shallow_km - 0.6,
            "19.5 degrees should fall about 6 percent short: {steep_km} against {shallow_km}"
        );
        assert!(
            (f64::from(sampler.max_ground_range_km()) - shallow_km).abs() < 1e-3,
            "the reach must come from the shallow tilt"
        );
    }

    #[test]
    fn a_point_between_the_two_sweep_edges_is_covered_by_one_tilt_and_not_the_other() {
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(19.5, 2, 60_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        let shallow_km = beam::ground_arc_m(MAX_SLANT_RANGE_M, 0.5) / 1000.0;
        let steep_km = beam::ground_arc_m(MAX_SLANT_RANGE_M, 19.5) / 1000.0;
        let between = 0.5 * (steep_km + shallow_km);
        let samples = column(&sampler, &volume, GroundPointKm::new(0.0, between));
        let shallow = samples
            .iter()
            .find(|sample| sample.elevation_deg == 0.5)
            .expect("the shallow tilt is in the column");
        let steep = samples
            .iter()
            .find(|sample| sample.elevation_deg == 19.5)
            .expect("the steep tilt is in the column");
        assert_eq!(shallow.state, CellState::Valid);
        assert_eq!(steep.state, CellState::NoCoverage);
    }

    #[test]
    fn the_scratch_vector_is_cleared_and_reused_rather_than_appended_to() {
        let volume = volume_of(vec![
            sweep(0.5, 1, 0, plain_row),
            sweep(4.5, 2, 60_000, plain_row),
        ]);
        let sampler = sampler_for(&volume);
        let mut scratch = Vec::new();
        for _ in 0..5 {
            sampler.sample_column(&volume, GroundPointKm::new(0.0, 8.0), &mut scratch);
            assert_eq!(scratch.len(), 2, "each call must replace, not append");
        }
        // The point of taking the caller's allocation: after the first call the
        // vector already has room and nothing further is allocated.
        let capacity = scratch.capacity();
        sampler.sample_column(&volume, GroundPointKm::new(3.0, 5.0), &mut scratch);
        assert_eq!(scratch.capacity(), capacity);
    }

    #[test]
    fn the_cone_of_silence_inside_the_first_gate_is_not_a_measurement() {
        let volume = volume_of(vec![sweep(0.5, 1, 0, marked_row)]);
        let sampler = sampler_for(&volume);
        // Zero slant range rounds to gate round(-2125 / 250) = -9, outside the
        // sweep. Clamping it to gate 0 would paint the radar's own position
        // with whatever the first gate happened to hold.
        let samples = column(&sampler, &volume, GroundPointKm::ORIGIN);
        assert_eq!(samples[0].state, CellState::NoCoverage);
        // Half a gate outside the first gate's centre is still gate 0.
        let samples = column(&sampler, &volume, point_at_slant(2_240.0, 0.5, 0.0));
        assert_eq!(samples[0].state, CellState::Valid);
        assert_eq!(samples[0].reflectivity_dbz, 20.0, "gate 0 plus the marker");
    }

    #[test]
    fn a_volume_with_no_reflectivity_cannot_be_prepared() {
        let mut cut = ElevationCut::new(0.5, Some(1));
        for index in 0..360usize {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 10,
                gate_range: gate_range(),
                nyquist_velocity_mps: Some(26.0),
                radial_status: None,
            });
        }
        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range(),
            2.0,
            129.0,
            Some(NODATA_CODE),
            Some(RANGE_FOLDED_CODE),
        );
        for index in 0..360usize {
            grid.push_u8_row_slice(index, &[129_u8; GATE_COUNT])
                .expect("a u8 row belongs in a u8 grid");
        }
        cut.moments.insert(MomentType::Velocity, grid);

        let volume = volume_of(vec![cut]);
        let capabilities = VolumeCapabilities::analyze(&volume);
        assert_eq!(
            VolumeSampler::prepare(&volume, &capabilities).unwrap_err(),
            SamplerError::NoReflectivityCuts
        );
    }

    #[test]
    fn a_reflectivity_sweep_with_no_gates_is_refused_rather_than_sampled_as_empty() {
        // An empty column reduces to NoCoverage everywhere, which on screen is
        // indistinguishable from clear air. Saying so is the only honest
        // option.
        let mut cut = ElevationCut::new(0.5, Some(1));
        for index in 0..360usize {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 10,
                gate_range: gate_range(),
                nyquist_velocity_mps: Some(26.0),
                radial_status: None,
            });
        }
        cut.moments.insert(
            MomentType::Reflectivity,
            MomentGrid::new_u8(
                MomentType::Reflectivity,
                GateRange {
                    first_gate_m: FIRST_GATE_M,
                    gate_spacing_m: GATE_SPACING_M,
                    gate_count: 0,
                },
                2.0,
                66.0,
                Some(NODATA_CODE),
                Some(RANGE_FOLDED_CODE),
            ),
        );

        let volume = volume_of(vec![cut]);
        let capabilities = VolumeCapabilities::analyze(&volume);
        assert_eq!(
            VolumeSampler::prepare(&volume, &capabilities).unwrap_err(),
            SamplerError::NoUsableGeometry
        );
    }

    #[test]
    fn an_azimuth_separation_takes_the_short_way_round_the_circle() {
        // 359.8 and 0.0 are 0.2 degrees apart, not 359.8. Subtracting f32
        // degrees leaves about 1e-5 of representation error, which is four
        // orders of magnitude below the 2.0 degree tolerance it feeds.
        assert!((angular_separation_deg(359.8, 0.0) - 0.2).abs() < 1e-4);
        assert!((angular_separation_deg(0.0, 359.8) - 0.2).abs() < 1e-4);
        assert_eq!(angular_separation_deg(1.0, 359.0), 2.0);
        assert_eq!(angular_separation_deg(90.0, 270.0), 180.0);
        assert_eq!(angular_separation_deg(10.0, 10.0), 0.0);
    }
}
