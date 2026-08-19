//! Inter-gate display interpolation: bilinear upsampling of a moment grid on
//! the polar lattice (azimuth x range). The pass runs ONCE per
//! volume/cut/product on the render worker (cached exactly like the binomial
//! Soften pass in `smooth.rs`) and the finer grid is drawn through the
//! unchanged nearest-gate fast path -- pans stay full speed; only the cached
//! grid is bigger.
//!
//! Technique: standard separable bilinear resampling, applied on the polar
//! grid rather than the screen raster -- the same "smoothed display" approach
//! popularized by GR2Analyst-class viewers (literally "adds grid cells in
//! between the scan lines"). No novel science. The per-moment-family guards
//! mirror `volumetric.rs` (`InterpPolicy`): correlation coefficient never
//! blends through a rho_hv minimum (Giangrande, Krause & Ryzhkov 2008,
//! J. Appl. Meteor. Climatol. 47(5), 1354-1364,
//! doi:10.1175/2007JAMC1634.1 -- the minimum IS the melting-layer
//! signature), and velocity never blends across a spread larger than the
//! volumetric module's 30 m/s guard (interpolating across an aliasing fold
//! or a couplet fabricates intermediate values that never existed; see
//! Doviak & Zrnic, *Doppler Radar and Weather Observations*, 2nd ed., 1993,
//! sec. 3.5 on velocity aliasing).
//!
//! ## Upsample policy (input geometry -> factors)
//!
//! Targets <= 0.25 deg azimuth and <= 250 m gates, capped at 4x per axis and
//! a 64 MB F32 grid budget. Coarse grids are upsampled aggressively, fine
//! grids mildly or not at all:
//!
//! | native cut                        | factors (az x rng) | result           |
//! |-----------------------------------|--------------------|------------------|
//! | 1.0 deg x 1000 m (legacy/intl)    | 4 x 4              | 0.25 deg x 250 m |
//! | 1.0 deg x 500 m (European C-band) | 4 x 2              | 0.25 deg x 250 m |
//! | 1.0 deg x 250 m                   | 4 x 1              | 0.25 deg x 250 m |
//! | 0.5 deg x 250 m (NEXRAD super-res)| 2 x 1              | 0.25 deg x 250 m |
//! | <= 0.25 deg x <= 250 m            | 1 x 1              | native (no pass) |
//!
//! Range factors are additionally constrained to keep the integer-meter gate
//! geometry exact (sub-spacing must divide evenly and the half-cell shift
//! must be a whole meter); failing factors step down -- including the
//! step-downs forced by the packed sample encoding's row/gate limits and by
//! the memory budget, which skip to the next EXACT factor rather than simply
//! decrementing.
//!
//! ## Coverage discipline
//!
//! Interpolation must NOT grow echo coverage: a sub-cell renders only where
//! the native display would. Each sub-cell lies inside exactly one native
//! cell (its nearest parent); if that parent is missing/RF the sub-cell
//! stays empty, and if any of the four bilinear parents is missing the
//! sub-cell takes the nearest parent's value instead of a partial blend.
//! Sub-rows exactly on a native beam boundary (t = 0.5) render only where
//! BOTH bracketing beams carry echo -- otherwise one side's echo would
//! reach half a sub-beam past the midpoint the native display stops at.
//! Azimuth wraps; range clamps. Like the Soften pass, RF gates render
//! transparent (the native display is the place to read the RF purple).
//! Sub-rows are synthesized only between beams whose azimuth gap is small
//! (sector-scan edges keep their native hole).

use crate::volumetric::InterpPolicy;
use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType};
use rayon::prelude::*;

/// Display target: no coarser than 0.25 deg between rendered radials.
pub const INTERP_TARGET_AZIMUTH_DEG: f32 = 0.25;
/// Display target: no coarser than 250 m between rendered gates.
pub const INTERP_TARGET_GATE_SPACING_M: i32 = 250;
/// Hard cap per axis -- beyond 4x the cost outruns the visual return.
pub const INTERP_MAX_FACTOR: usize = 4;
/// Budget for the cached F32 grid (the moment-cache entry that holds it).
pub const INTERP_MAX_GRID_BYTES: usize = 64 << 20;

/// Same threshold as `volumetric.rs`'s `InterpPolicy::VelocityGuard`.
const VELOCITY_GUARD_SPREAD_MPS: f32 = 30.0;
/// Same floor as `volumetric.rs`'s `InterpPolicy::CcGuard`.
const CC_GUARD_FLOOR: f32 = 0.97;
/// Beams closer than this are duplicates -- nothing to synthesize between.
const MIN_SYNTH_DELTA_DEG: f32 = 0.01;

/// A moment grid upsampled for display plus the per-row azimuths the
/// synthetic rows render at (native rows keep their exact beam azimuth).
pub struct InterpolatedGrid {
    pub grid: MomentGrid,
    pub row_azimuths_deg: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpsampleFactors {
    pub azimuth: usize,
    pub range: usize,
}

impl UpsampleFactors {
    pub fn is_identity(self) -> bool {
        self.azimuth <= 1 && self.range <= 1
    }
}

/// Sub-spacing must stay whole meters and the cell-centered half-shift
/// `(spacing - spacing/factor) / 2` must be a whole meter too, so the
/// upsampled annulus is EXACTLY the native one.
fn range_factor_is_exact(spacing_m: i32, factor: usize) -> bool {
    let factor = factor as i32;
    factor > 0 && spacing_m % factor == 0 && (spacing_m - spacing_m / factor) % 2 == 0
}

/// The largest exact range factor strictly below `factor`. Every step-down
/// path (packed-encoding limits, memory budget) must go through this rather
/// than `factor - 1`: `factor - 1` can land on a factor that does not divide
/// the spacing, and `spacing / factor` then truncates. 1000 m stepped 4 -> 3
/// gives 333 m sub-gates spanning 999 m per native gate -- the annulus ends
/// a metre per gate short of the one the sweep actually painted, and the
/// half-cell shift `(333 - 1000) / 2` truncates to -333 m instead of
/// -333.5 m, sliding every probe readout half a metre inward. Factor 1 is
/// always exact, so this terminates.
fn largest_exact_range_factor_below(spacing_m: i32, factor: usize) -> usize {
    let mut candidate = factor.saturating_sub(1).max(1);
    while candidate > 1 && !range_factor_is_exact(spacing_m, candidate) {
        candidate -= 1;
    }
    candidate
}

/// Pick adaptive upsample factors for a cut's nominal geometry (see the
/// module-level policy table).
pub fn upsample_factors(
    nominal_azimuth_deg: f32,
    gate_spacing_m: i32,
    rows: usize,
    gates: usize,
) -> UpsampleFactors {
    let mut azimuth = 1usize;
    if nominal_azimuth_deg.is_finite() && nominal_azimuth_deg > 0.0 {
        while azimuth < INTERP_MAX_FACTOR
            && nominal_azimuth_deg / azimuth as f32 > INTERP_TARGET_AZIMUTH_DEG + 1e-3
        {
            azimuth += 1;
        }
    }
    let mut range = 1usize;
    if gate_spacing_m > 0 {
        while range < INTERP_MAX_FACTOR
            && gate_spacing_m as f32 / range as f32 > INTERP_TARGET_GATE_SPACING_M as f32
        {
            range += 1;
        }
        while range > 1 && !range_factor_is_exact(gate_spacing_m, range) {
            range -= 1;
        }
    }
    // The fast path packs (row, gate) into 31 bits -- stay inside it.
    while azimuth > 1 && rows.saturating_mul(azimuth) >= crate::CachedSample::ROW_LIMIT {
        azimuth -= 1;
    }
    while range > 1 && gates.saturating_mul(range) > crate::CachedSample::GATE_MASK as usize {
        range = largest_exact_range_factor_below(gate_spacing_m, range);
    }
    // Memory budget for the cached F32 grid. Range steps down first (gates
    // are usually already the finer axis; the azimuth seams are what the
    // interpolated mode exists to fill).
    while rows
        .saturating_mul(azimuth)
        .saturating_mul(gates)
        .saturating_mul(range)
        .saturating_mul(std::mem::size_of::<f32>())
        > INTERP_MAX_GRID_BYTES
    {
        if range > 1 {
            range = largest_exact_range_factor_below(gate_spacing_m, range);
        } else if azimuth > 1 {
            azimuth -= 1;
        } else {
            break;
        }
    }
    UpsampleFactors { azimuth, range }
}

/// Per-moment-family interpolation policy, mirroring the volumetric module's
/// cross-section policies (`volumetric.rs`): velocity guards against
/// blending across folds/couplets, CC against blending through the melting
/// layer; everything else (REF/ZDR/SW/PHI/KDP) blends linearly.
fn interp_policy_for_moment(moment: &MomentType) -> InterpPolicy {
    match moment {
        MomentType::Velocity => InterpPolicy::VelocityGuard,
        MomentType::CorrelationCoefficient => InterpPolicy::CcGuard,
        _ => InterpPolicy::LinearAngle,
    }
}

/// Shortest signed angular step from `from` to `to`, in (-180, 180].
fn signed_delta_deg(from_deg: f32, to_deg: f32) -> f32 {
    let delta = (to_deg - from_deg).rem_euclid(360.0);
    if delta > 180.0 { delta - 360.0 } else { delta }
}

/// One output row's parents: bilinear weight `t` between scan-order rows
/// `lo` and `hi` (t = 0 reproduces the native row `lo` exactly).
struct RowPlan {
    lo: usize,
    hi: usize,
    t: f32,
    azimuth_deg: f32,
}

/// One output gate's parents: weight `u` between native gate centers `lo`
/// and `hi`; `nearest` is the native gate whose cell contains the sub-gate
/// (the coverage authority).
struct GatePlan {
    lo: usize,
    hi: usize,
    u: f32,
    nearest: usize,
}

/// Upsample a moment grid for display. Returns `None` when the grid is
/// already at/finer than the display targets (callers fall back to the
/// native path), when the cut has too few radials, or when the radial
/// linkage is broken.
pub fn upsample_moment_grid(cut: &ElevationCut, grid: &MomentGrid) -> Option<InterpolatedGrid> {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    if rows < 2 || gates == 0 {
        return None;
    }
    let mut azimuths = Vec::with_capacity(rows);
    for radial_index in &grid.radial_indices {
        azimuths.push(
            cut.radials
                .get(*radial_index)?
                .azimuth_deg
                .rem_euclid(360.0),
        );
    }
    // Scan-order azimuth steps (signed shortest; handles CW and CCW sweeps
    // and the wrap pair alike). The median is the nominal beam spacing --
    // robust to a sector scan's single large wrap gap.
    let deltas: Vec<f32> = (0..rows)
        .map(|row| signed_delta_deg(azimuths[row], azimuths[(row + 1) % rows]))
        .collect();
    let mut magnitudes: Vec<f32> = deltas
        .iter()
        .map(|delta| delta.abs())
        .filter(|delta| delta.is_finite())
        .collect();
    if magnitudes.is_empty() {
        return None;
    }
    magnitudes.sort_by(f32::total_cmp);
    let nominal_deg = magnitudes[magnitudes.len() / 2];
    let factors = upsample_factors(nominal_deg, grid.gate_range.gate_spacing_m, rows, gates);
    if factors.is_identity() {
        return None;
    }

    // Sub-rows only between beams whose gap is believably adjacent: wider
    // gaps (sector-scan edges, dropped radials) keep their native hole so
    // azimuth coverage cannot grow. Native bins fill at most
    // MAX_AZIMUTH_HALF_WIDTH_DEG to each side, hence the absolute cap.
    let gap_limit_deg = (nominal_deg * 2.0).min(crate::MAX_AZIMUTH_HALF_WIDTH_DEG * 2.0);
    let mut row_plan = Vec::with_capacity(rows * factors.azimuth);
    for row in 0..rows {
        row_plan.push(RowPlan {
            lo: row,
            hi: row,
            t: 0.0,
            azimuth_deg: azimuths[row],
        });
        let delta = deltas[row];
        if factors.azimuth > 1 && delta.abs() >= MIN_SYNTH_DELTA_DEG && delta.abs() <= gap_limit_deg
        {
            let hi = (row + 1) % rows;
            for step in 1..factors.azimuth {
                let t = step as f32 / factors.azimuth as f32;
                row_plan.push(RowPlan {
                    lo: row,
                    hi,
                    t,
                    azimuth_deg: (azimuths[row] + t * delta).rem_euclid(360.0),
                });
            }
        }
    }

    // Cell-centered range subdivision: native gate g's cell splits into R
    // sub-cells whose centers interpolate between the surrounding native
    // gate CENTERS; ends clamp. first_gate_m is a gate center
    // (gate = round((range - first)/spacing) in the fast path), so
    // new_first = first + (sub - spacing)/2 keeps the rendered annulus
    // [first - spacing/2, first + (count - 0.5)*spacing) EXACTLY.
    let range_factor = factors.range;
    let new_gates = gates * range_factor;
    let mut gate_plan = Vec::with_capacity(new_gates);
    for sub_gate in 0..new_gates {
        let x = (sub_gate as f32 + 0.5) / range_factor as f32 - 0.5;
        let nearest = sub_gate / range_factor;
        let (lo, hi, u) = if x <= 0.0 {
            (0, 0, 0.0)
        } else if x >= (gates - 1) as f32 {
            (gates - 1, gates - 1, 0.0)
        } else {
            let lo = x.floor() as usize;
            (lo, lo + 1, x - lo as f32)
        };
        gate_plan.push(GatePlan { lo, hi, u, nearest });
    }

    // Materialize scaled values once (NaN for missing/RF), as in smooth.rs.
    let mut source = vec![f32::NAN; rows * gates];
    source
        .par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            for (gate, cell) in out_row.iter_mut().enumerate() {
                if let Some(value) = grid
                    .scaled_value(row, gate)
                    .filter(|value| value.is_finite())
                {
                    *cell = value;
                }
            }
        });

    let policy = interp_policy_for_moment(&grid.moment);
    let mut values = vec![f32::NAN; row_plan.len() * new_gates];
    values
        .par_chunks_mut(new_gates)
        .zip(row_plan.par_iter())
        .for_each(|(out_row, plan)| {
            let row_lo = &source[plan.lo * gates..(plan.lo + 1) * gates];
            let row_hi = &source[plan.hi * gates..(plan.hi + 1) * gates];
            let nearest_row = if plan.t <= 0.5 { row_lo } else { row_hi };
            // A sub-row exactly on the native beam boundary (t = 0.5)
            // belongs to NEITHER beam: painting it from one side would
            // push that side's echo half a sub-beam past the midpoint the
            // native display stops at. It renders only where both
            // bracketing beams have echo (strict no-growth; at worst the
            // boundary row goes empty at an echo's azimuth edge).
            let on_beam_boundary = plan.t == 0.5;
            for (cell, gate) in out_row.iter_mut().zip(&gate_plan) {
                let nearest = nearest_row[gate.nearest];
                if !nearest.is_finite() {
                    // The native cell containing this sub-cell is empty --
                    // it stays empty (coverage never grows).
                    continue;
                }
                if on_beam_boundary && !row_hi[gate.nearest].is_finite() {
                    continue;
                }
                let v00 = row_lo[gate.lo];
                let v01 = row_lo[gate.hi];
                let v10 = row_hi[gate.lo];
                let v11 = row_hi[gate.hi];
                if !(v00.is_finite() && v01.is_finite() && v10.is_finite() && v11.is_finite()) {
                    // An echo edge: no partial blends, the nearest parent's
                    // value carries through unchanged.
                    *cell = nearest;
                    continue;
                }
                let min = v00.min(v01).min(v10).min(v11);
                let max = v00.max(v01).max(v10).max(v11);
                let guarded = match policy {
                    InterpPolicy::CcGuard => min < CC_GUARD_FLOOR,
                    InterpPolicy::VelocityGuard => max - min > VELOCITY_GUARD_SPREAD_MPS,
                    InterpPolicy::LinearAngle => false,
                };
                *cell = if guarded {
                    nearest
                } else {
                    let lo = v00 + (v01 - v00) * gate.u;
                    let hi = v10 + (v11 - v10) * gate.u;
                    lo + (hi - lo) * plan.t
                };
            }
        });

    let sub_spacing_m = grid.gate_range.gate_spacing_m / range_factor as i32;
    let gate_range = GateRange {
        first_gate_m: grid.gate_range.first_gate_m
            + (sub_spacing_m - grid.gate_range.gate_spacing_m) / 2,
        gate_spacing_m: sub_spacing_m,
        gate_count: new_gates,
    };
    // Each output row links back to its nearest parent's radial so
    // cut-radial lookups (Nyquist, beam azimuth basis) stay valid.
    let radial_indices = row_plan
        .iter()
        .map(|plan| grid.radial_indices[if plan.t <= 0.5 { plan.lo } else { plan.hi }])
        .collect();
    let row_azimuths_deg = row_plan.iter().map(|plan| plan.azimuth_deg).collect();
    Some(InterpolatedGrid {
        grid: MomentGrid {
            moment: grid.moment.clone(),
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices,
            storage: MomentStorage::F32(values),
        },
        row_azimuths_deg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::Radial;
    use std::collections::BTreeMap;

    fn radial(azimuth_deg: f32, gate_range: GateRange) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range,
            radial_status: None,
            nyquist_velocity_mps: Some(26.0),
        }
    }

    fn cut_and_grid(
        moment: MomentType,
        azimuths: &[f32],
        gates: usize,
        spacing_m: i32,
        data: Vec<f32>,
    ) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: spacing_m,
            gate_spacing_m: spacing_m,
            gate_count: gates,
        };
        let cut = ElevationCut {
            elevation_deg: 0.5,
            elevation_number: Some(1),
            radials: azimuths
                .iter()
                .map(|az| radial(*az, gate_range.clone()))
                .collect(),
            moments: BTreeMap::new(),
        };
        let grid = MomentGrid {
            moment,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..azimuths.len()).collect(),
            storage: MomentStorage::F32(data),
        };
        (cut, grid)
    }

    fn full_sweep_azimuths(count: usize) -> Vec<f32> {
        (0..count)
            .map(|i| i as f32 * 360.0 / count as f32)
            .collect()
    }

    #[test]
    fn factor_policy_table() {
        // (nominal az, spacing) -> (az factor, range factor)
        let cases = [
            ((1.0, 1000), (4, 4)),
            ((1.0, 500), (4, 2)),
            ((1.0, 250), (4, 1)),
            ((0.5, 250), (2, 1)),
            ((0.25, 250), (1, 1)),
            ((0.5, 1000), (2, 4)),
        ];
        for ((az, sp), (fa, fr)) in cases {
            let factors = upsample_factors(az, sp, 360, 1000);
            assert_eq!(
                (factors.azimuth, factors.range),
                (fa, fr),
                "policy for {az} deg x {sp} m"
            );
        }
    }

    #[test]
    fn factor_policy_respects_exact_geometry_and_budget() {
        // 900 m gates: 4x would need a fractional half-shift (225 m sub,
        // 337.5 m shift) -- steps down to 3x (300 m, exact).
        let factors = upsample_factors(1.0, 900, 360, 1000);
        assert_eq!(factors.range, 3);
        // Budget: 720 x 8000 cells at 2x2 would be 92 MB -- range steps
        // down first.
        let factors = upsample_factors(0.5, 500, 720, 8000);
        assert_eq!((factors.azimuth, factors.range), (2, 1));
    }

    #[test]
    fn geometry_subdivides_exactly() {
        // 1.0 deg x 1000 m, 360 rows x 4 gates -> 4x both axes.
        let azimuths = full_sweep_azimuths(360);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 360 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("coarse grid upsamples");
        assert_eq!(up.grid.gate_range.gate_spacing_m, 250);
        assert_eq!(up.grid.gate_range.gate_count, 16);
        // first_gate_m is a gate CENTER: new_first = first + (sub - sp)/2.
        assert_eq!(up.grid.gate_range.first_gate_m, 1000 + (250 - 1000) / 2);
        // The rendered annulus is preserved exactly:
        // first + (count - 0.5)*spacing matches on both grids.
        let native_edge = grid.gate_range.first_gate_m as f32
            + (grid.gate_range.gate_count as f32 - 0.5) * grid.gate_range.gate_spacing_m as f32;
        let up_edge = up.grid.gate_range.first_gate_m as f32
            + (up.grid.gate_range.gate_count as f32 - 0.5)
                * up.grid.gate_range.gate_spacing_m as f32;
        assert_eq!(native_edge, up_edge);
        let native_inner =
            grid.gate_range.first_gate_m as f32 - grid.gate_range.gate_spacing_m as f32 / 2.0;
        let up_inner =
            up.grid.gate_range.first_gate_m as f32 - up.grid.gate_range.gate_spacing_m as f32 / 2.0;
        assert_eq!(native_inner, up_inner);
        // 360 rows x 4 az factor, every native row at its exact azimuth.
        assert_eq!(up.row_azimuths_deg.len(), 1440);
        assert_eq!(up.grid.radial_count(), 1440);
        for (row, az) in azimuths.iter().enumerate() {
            assert_eq!(up.row_azimuths_deg[row * 4], *az, "native row {row}");
        }
        // Synthetic rows fall between (1 deg spacing / 4 = 0.25 deg steps).
        assert!((up.row_azimuths_deg[1] - 0.25).abs() < 1e-3);
        assert!((up.row_azimuths_deg[2] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn azimuth_wraps_between_last_and_first_row() {
        let azimuths = full_sweep_azimuths(360);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 360 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        // Last native row is 359 deg; sub-rows climb toward 360 and wrap.
        let tail: Vec<f32> = up.row_azimuths_deg[1437..].to_vec();
        assert!((tail[0] - 359.25).abs() < 1e-3, "{tail:?}");
        assert!((tail[2] - 359.75).abs() < 1e-3, "{tail:?}");
    }

    #[test]
    fn uniform_field_is_unchanged_and_fine_grids_pass_through() {
        let azimuths = full_sweep_azimuths(360);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            8,
            500,
            vec![35.0; 360 * 8],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 35.0).abs() < 1e-4,
                    "row {row} gate {gate}: {value}"
                );
            }
        }
        // Already at target: no pass.
        let azimuths = full_sweep_azimuths(1440);
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            250,
            vec![35.0; 1440 * 4],
        );
        assert!(upsample_moment_grid(&cut, &grid).is_none());
    }

    #[test]
    fn coverage_does_not_grow() {
        // Rows 0..180 carry echo, rows 180..360 are empty: every sub-cell
        // whose containing native cell is empty must stay empty.
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..180 {
            for gate in 0..4 {
                data[row * 4 + gate] = 20.0;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let mut covered_rows = 0;
        for row in 0..up.grid.radial_count() {
            if (0..up.grid.gate_range.gate_count).any(|gate| {
                up.grid
                    .scaled_value(row, gate)
                    .is_some_and(|v| v.is_finite())
            }) {
                covered_rows += 1;
            }
        }
        // Exactly the rows whose NEAREST parent is an echo row render
        // (and the beam-boundary rows at t = 0.5 only where BOTH parents
        // carry echo): 180 native echo rows, 179 interior segments x 3
        // sub-rows, the trailing edge of row 179 (t = 0.25 toward empty
        // row 180 -- its t = 0.5 boundary row stays empty), and the
        // leading edge of row 0 (t = 0.75 from empty row 359) -- 719 of
        // 1440 rows, never more than the echo's native angular footprint.
        assert_eq!(covered_rows, 180 + 179 * 3 + 1 + 1);
    }

    #[test]
    fn echo_edges_use_nearest_parent_not_partial_blends() {
        // One echo column next to an empty column: sub-cells inside the
        // echo column keep the exact parent value (no fade-out ramp).
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            data[row * 4 + 1] = 40.0;
        }
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for sub in 0..4 {
                let gate = 4 + sub; // native gate 1's sub-cells
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 40.0).abs() < 1e-4,
                    "row {row} sub {sub}: {value} (partial blend leaked)"
                );
            }
            // Native gates 0 and 2 are empty: their sub-cells stay empty.
            for gate in (0..4).chain(8..12) {
                assert!(
                    up.grid
                        .scaled_value(row, gate)
                        .is_none_or(|value| value.is_nan()),
                    "row {row} gate {gate} grew coverage"
                );
            }
        }
    }

    #[test]
    fn velocity_fold_guard_uses_nearest_parent() {
        // Neighboring radials at +20 / -22 m/s (spread 42 > 30): blending
        // would fabricate near-zero gates inside the couplet/fold.
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            let value = if row % 2 == 0 { 20.0 } else { -22.0 };
            for gate in 0..4 {
                data[row * 4 + gate] = value;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Velocity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 20.0).abs() < 1e-4 || (value + 22.0).abs() < 1e-4,
                    "row {row} gate {gate}: fabricated intermediate {value}"
                );
            }
        }
        // Small spreads DO blend (same field, +/-5 m/s).
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            let value = if row % 2 == 0 { 5.0 } else { -5.0 };
            for gate in 0..4 {
                data[row * 4 + gate] = value;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Velocity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let blended = (0..up.grid.radial_count()).any(|row| {
            (0..up.grid.gate_range.gate_count).any(|gate| {
                up.grid
                    .scaled_value(row, gate)
                    .is_some_and(|value| value.abs() < 4.0)
            })
        });
        assert!(blended, "small velocity spreads should interpolate");
    }

    #[test]
    fn cc_guard_never_blends_through_the_melting_layer() {
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..360 {
            for gate in 0..4 {
                // 0.92 / 1.0 alternating along range: the rho_hv minimum
                // must survive (no 0.96 fabrications).
                data[row * 4 + gate] = if gate % 2 == 0 { 0.92 } else { 1.0 };
            }
        }
        let (cut, grid) =
            cut_and_grid(MomentType::CorrelationCoefficient, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 0.92).abs() < 1e-4 || (value - 1.0).abs() < 1e-4,
                    "row {row} gate {gate}: blended through the CC minimum ({value})"
                );
            }
        }
    }

    #[test]
    fn sector_scan_gap_stays_native() {
        // A 91-radial sector (0..90 deg) -- no sub-rows across the 270 deg
        // gap.
        let azimuths: Vec<f32> = (0..91).map(|i| i as f32).collect();
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 91 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for az in &up.row_azimuths_deg {
            assert!(
                *az <= 90.0 + 1e-3,
                "synthetic row at {az} deg bridges the sector gap"
            );
        }
        // ...but inside the sector the rows did refine to 0.25 deg.
        assert_eq!(up.row_azimuths_deg.len(), 91 + 90 * 3);
    }

    #[test]
    fn upsample_cost_smoke() {
        // NEXRAD super-res-shaped cut (720 x 1832 at 0.5 deg x 250 m ->
        // 2x1) and a European-shaped cut (360 x 960 at 1.0 deg x 500 m ->
        // 4x2): one pass each, wall-clock printed for the perf report.
        let azimuths = full_sweep_azimuths(720);
        let data: Vec<f32> = (0..720 * 1832).map(|i| (i % 70) as f32).collect();
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 1832, 250, data);
        let start = std::time::Instant::now();
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let super_res_ms = start.elapsed().as_secs_f32() * 1000.0;
        assert_eq!(up.grid.radial_count(), 1440);
        assert_eq!(up.grid.gate_range.gate_count, 1832);

        let azimuths = full_sweep_azimuths(360);
        let data: Vec<f32> = (0..360 * 960).map(|i| (i % 70) as f32).collect();
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 960, 500, data);
        let start = std::time::Instant::now();
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let euro_ms = start.elapsed().as_secs_f32() * 1000.0;
        assert_eq!(up.grid.radial_count(), 1440);
        assert_eq!(up.grid.gate_range.gate_count, 1920);

        // Worst realistic 4x4 case: a long-range 1.0 deg x 1000 m cut
        // (360 x 2000 -> 1440 x 8000 = 11.5M cells, 46 MB F32).
        let azimuths = full_sweep_azimuths(360);
        let data: Vec<f32> = (0..360 * 2000).map(|i| (i % 70) as f32).collect();
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 2000, 1000, data);
        let start = std::time::Instant::now();
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let long_range_ms = start.elapsed().as_secs_f32() * 1000.0;
        assert_eq!(up.grid.radial_count(), 1440);
        assert_eq!(up.grid.gate_range.gate_count, 8000);
        println!(
            "upsample cost: super-res 720x1832 (2x1) {super_res_ms:.2} ms, \
             euro 360x960 (4x2) {euro_ms:.2} ms, \
             long-range 360x2000 (4x4) {long_range_ms:.2} ms"
        );
        // Generous bound -- this is a smoke test, not a benchmark gate.
        assert!(super_res_ms < 2000.0 && euro_ms < 2000.0 && long_range_ms < 2000.0);
    }

    /// A cut carrying an explicit gate geometry, for the U8/U16 fixtures
    /// that cannot go through `cut_and_grid`'s F32 path.
    fn cut_for(azimuths: &[f32], gate_range: &GateRange) -> ElevationCut {
        ElevationCut {
            elevation_deg: 0.5,
            elevation_number: Some(1),
            radials: azimuths
                .iter()
                .map(|az| radial(*az, gate_range.clone()))
                .collect(),
            moments: BTreeMap::new(),
        }
    }

    /// Regression: EVERY range step-down has to land on a factor that keeps
    /// the integer-metre geometry exact, not just the one the policy loop
    /// picks first. A 360 x 3000 cut at 1.0 deg x 1000 m wants 4x4, blows
    /// the 64 MB budget, and used to step 4 -> 3: 1000 / 3 truncates to
    /// 333 m, so the upsampled sweep spanned 3000 x 999 m = 2 997 000 m
    /// against the native 3 000 000 m -- 3 km of annulus gone -- and the
    /// half-cell shift (333 - 1000) / 2 truncated to -333 m instead of
    /// -333.5 m, sliding every probe readout half a metre inward. 800 m at
    /// the same shape was the same story (266 m sub-gates spanning 798 m).
    #[test]
    fn every_range_step_down_keeps_integer_metre_geometry_exact() {
        for (nominal, spacing, rows, gates) in [
            (1.0f32, 1000i32, 360usize, 3000usize),
            (1.0, 800, 360, 3000),
            (1.0, 1000, 360, 20000),
            (1.0, 1600, 720, 4000),
        ] {
            let factors = upsample_factors(nominal, spacing, rows, gates);
            assert!(
                range_factor_is_exact(spacing, factors.range),
                "{nominal} deg x {spacing} m ({rows}x{gates}) stepped down to a non-exact {}x",
                factors.range
            );
        }
        // Property sweep over every geometry the policy can be handed.
        for spacing in [
            100, 125, 150, 200, 250, 300, 400, 500, 600, 750, 800, 900, 1000, 1200, 1500, 1600,
            2000,
        ] {
            for gates in [1usize, 4, 460, 1000, 1832, 3000, 8000, 20000, 40000] {
                for rows in [180usize, 360, 720, 1440] {
                    for nominal in [0.25f32, 0.5, 0.7, 1.0, 2.0] {
                        let factors = upsample_factors(nominal, spacing, rows, gates);
                        let sub = spacing / factors.range as i32;
                        assert_eq!(
                            sub * factors.range as i32,
                            spacing,
                            "{nominal} deg x {spacing} m ({rows}x{gates}): {}x sub-spacing {sub} \
                             does not tile the native gate",
                            factors.range
                        );
                        assert_eq!(
                            (sub - spacing) % 2,
                            0,
                            "{nominal} deg x {spacing} m ({rows}x{gates}): the half-cell shift is \
                             not a whole metre at {}x",
                            factors.range
                        );
                        let bytes = rows
                            .saturating_mul(factors.azimuth)
                            .saturating_mul(gates)
                            .saturating_mul(factors.range)
                            .saturating_mul(std::mem::size_of::<f32>());
                        assert!(
                            bytes <= INTERP_MAX_GRID_BYTES || factors.is_identity(),
                            "{nominal} deg x {spacing} m ({rows}x{gates}): {bytes} B is over the \
                             {INTERP_MAX_GRID_BYTES} B budget at {}x{}",
                            factors.azimuth,
                            factors.range
                        );
                    }
                }
            }
        }
    }

    /// The 0/360 seam: the sub-rows between the LAST and the FIRST beam must
    /// blend that wrapped pair. Beam 359 holds 10 dBZ and every other beam
    /// 30 dBZ, so the three seam sub-rows are hand-computable -- 10 + 20 t
    /// at t = 1/4, 1/2, 3/4 gives 15, 20 and 25 dBZ at 359.25, 359.50 and
    /// 359.75 deg. Pairing beam 359 with 358 instead, or leaving the seam
    /// unsynthesized, cannot produce that ramp.
    #[test]
    fn seam_sub_rows_blend_the_last_beam_into_the_first() {
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![30.0f32; 360 * 4];
        for gate in 0..4 {
            data[359 * 4 + gate] = 10.0;
        }
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        assert_eq!(up.grid.radial_count(), 1440);
        for (step, expected_az, expected_value) in [
            (1usize, 359.25f32, 15.0f32),
            (2, 359.5, 20.0),
            (3, 359.75, 25.0),
        ] {
            let row = 359 * 4 + step;
            assert!(
                (up.row_azimuths_deg[row] - expected_az).abs() < 1e-3,
                "seam row {row} sits at {} deg, not {expected_az}",
                up.row_azimuths_deg[row]
            );
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - expected_value).abs() < 1e-4,
                    "seam row {row} gate {gate}: {value} != {expected_value}"
                );
            }
        }
        // The native beams either side keep their own measurement.
        assert!((up.grid.scaled_value(359 * 4, 0).unwrap() - 10.0).abs() < 1e-4);
        assert!((up.grid.scaled_value(0, 0).unwrap() - 30.0).abs() < 1e-4);
        // ...and the sub-rows on the far side of beam 359 ramp the other
        // way: 30 -> 10 at t = 1/4 is 25 dBZ.
        assert!((up.grid.scaled_value(358 * 4 + 1, 0).unwrap() - 25.0).abs() < 1e-4);
    }

    /// Coverage must not leak across the seam either. With echo only on
    /// beams 0..=5, the sub-rows between empty beam 359 and echo beam 0 may
    /// paint only the one inside beam 0's own native half-cell (t = 3/4,
    /// i.e. 359.75 deg = -0.25 deg); the t = 1/2 boundary row stays empty
    /// because beam 359 carries nothing.
    #[test]
    fn seam_does_not_leak_coverage_into_the_empty_side() {
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![f32::NAN; 360 * 4];
        for row in 0..=5 {
            for gate in 0..4 {
                data[row * 4 + gate] = 30.0;
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 4, 1000, data);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let covered: Vec<usize> = (0..up.grid.radial_count())
            .filter(|row| {
                (0..up.grid.gate_range.gate_count).any(|gate| {
                    up.grid
                        .scaled_value(*row, gate)
                        .is_some_and(|value| value.is_finite())
                })
            })
            .collect();
        // 6 native beams + 5 fully bracketed segments x 3 sub-rows + the
        // t = 1/4 trailing edge of beam 5 + the t = 3/4 leading edge of
        // beam 0 reached across the seam.
        assert_eq!(covered.len(), 6 + 5 * 3 + 1 + 1, "{covered:?}");
        assert!(
            covered.contains(&1439),
            "the seam sub-row inside beam 0's half-cell went dark"
        );
        assert!(
            !covered.contains(&1438),
            "the t = 1/2 seam boundary row painted from one side only"
        );
        assert!(
            !covered.contains(&1437),
            "coverage leaked a full sub-beam past beam 0 across the seam"
        );
        // Every painted row lies inside [-0.5, 5.5] deg -- the footprint
        // beams 0..=5 already cover natively.
        for row in covered {
            let offset = signed_delta_deg(0.0, up.row_azimuths_deg[row]);
            assert!(
                (-0.5..=5.5).contains(&offset),
                "row {row} at {offset} deg grew the echo's azimuth footprint"
            );
        }
    }

    /// The velocity guard has to fire on the RANGE axis too, not only across
    /// beams: a fold between adjacent gates is exactly the couplet edge an
    /// operator reads off. +26 / -26 m/s either side of gate 4 spans
    /// 52 m/s, past the 30 m/s guard.
    #[test]
    fn velocity_fold_along_range_is_guarded() {
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![0.0f32; 360 * 8];
        for row in 0..360 {
            for gate in 0..8 {
                data[row * 8 + gate] = if gate < 4 { 26.0 } else { -26.0 };
            }
        }
        let (cut, grid) = cut_and_grid(MomentType::Velocity, &azimuths, 8, 1000, data.clone());
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 26.0).abs() < 1e-4 || (value + 26.0).abs() < 1e-4,
                    "row {row} gate {gate}: fabricated {value} inside a range-axis fold"
                );
            }
        }
        // Control: the identical field with the guard switched off (moment
        // relabeled) DOES fabricate, so the assertion above is not vacuous.
        let (cut, mut grid) = cut_and_grid(MomentType::Velocity, &azimuths, 8, 1000, data);
        grid.moment = MomentType::Unknown("TEST".to_owned());
        let plain = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let fabricated = (0..plain.grid.radial_count()).any(|row| {
            (0..plain.grid.gate_range.gate_count).any(|gate| {
                plain
                    .grid
                    .scaled_value(row, gate)
                    .is_some_and(|value| value.abs() < 20.0)
            })
        });
        assert!(fabricated, "the unguarded control never blended the fold");
    }

    /// ...and the CC guard has to fire on the AZIMUTH axis. Alternating
    /// beams at 0.995 / 0.85 put a rho_hv minimum between every pair of
    /// radials; no sub-beam may land on the depression's shoulder
    /// (Giangrande, Krause & Ryzhkov 2008).
    #[test]
    fn cc_minimum_across_beams_is_guarded() {
        let azimuths = full_sweep_azimuths(360);
        let mut data = vec![0.0f32; 360 * 4];
        for row in 0..360 {
            let value = if row % 2 == 0 { 0.995 } else { 0.85 };
            for gate in 0..4 {
                data[row * 4 + gate] = value;
            }
        }
        let (cut, grid) = cut_and_grid(
            MomentType::CorrelationCoefficient,
            &azimuths,
            4,
            1000,
            data.clone(),
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        for row in 0..up.grid.radial_count() {
            for gate in 0..up.grid.gate_range.gate_count {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 0.995).abs() < 1e-4 || (value - 0.85).abs() < 1e-4,
                    "row {row} gate {gate}: blended through the CC minimum ({value})"
                );
            }
        }
        let (cut, mut grid) =
            cut_and_grid(MomentType::CorrelationCoefficient, &azimuths, 4, 1000, data);
        grid.moment = MomentType::Unknown("TEST".to_owned());
        let plain = upsample_moment_grid(&cut, &grid).expect("upsamples");
        let smeared = (0..plain.grid.radial_count()).any(|row| {
            (0..plain.grid.gate_range.gate_count).any(|gate| {
                plain
                    .grid
                    .scaled_value(row, gate)
                    .is_some_and(|value| (0.86..0.99).contains(&value))
            })
        });
        assert!(smeared, "the unguarded control never blended the minimum");
    }

    /// U8 storage with NEXRAD sentinels: the nodata and range-folded codes
    /// must never be scaled into numbers. This grid is REF-scaled
    /// (`value = (raw - 66) / 2`) with gates [nodata, RF, 100, 120] ->
    /// [-, -, 17 dBZ, 27 dBZ]. Hand-computed 4x range subdivision of native
    /// gate 2: sub-cells 8 and 9 bracket the RF gate, so a bilinear parent
    /// is missing and they hold 17.0 exactly; 10..13 blend toward gate 3 at
    /// u = 1/8, 3/8, 5/8, 7/8 -> 18.25, 20.75, 23.25, 25.75 dBZ; 14 and 15
    /// clamp at the last gate centre -> 27.0. Reading the RF code as a
    /// number would drag (1 - 66) / 2 = -32.5 dBZ into that ramp.
    #[test]
    fn u8_sentinels_are_never_averaged_into_values() {
        let azimuths = full_sweep_azimuths(360);
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 1000,
            gate_count: 4,
        };
        let cut = cut_for(&azimuths, &gate_range);
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        for row in 0..360 {
            grid.push_u8_row_slice(row, &[0, 1, 100, 120]).unwrap();
        }
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        assert_eq!(up.grid.gate_range.gate_count, 16);
        assert_eq!(up.grid.gate_range.gate_spacing_m, 250);
        assert_eq!(up.grid.gate_range.first_gate_m, 1000 + (250 - 1000) / 2);
        assert_eq!(up.grid.scale, 1.0);
        assert_eq!(up.grid.offset, 0.0);
        assert_eq!(up.grid.nodata, None);
        assert_eq!(up.grid.range_folded, None);
        for row in 0..up.grid.radial_count() {
            for gate in 0..8 {
                assert!(
                    up.grid
                        .scaled_value(row, gate)
                        .is_none_or(|value| value.is_nan()),
                    "row {row} gate {gate}: a sentinel gate grew coverage"
                );
            }
            for (gate, expected) in [
                (8usize, 17.0f32),
                (9, 17.0),
                (10, 18.25),
                (11, 20.75),
                (12, 23.25),
                (13, 25.75),
                (14, 27.0),
                (15, 27.0),
            ] {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - expected).abs() < 1e-4,
                    "row {row} gate {gate}: {value} != {expected}"
                );
            }
        }
    }

    /// U16 storage round-trips through the same path, ZDR-scaled
    /// (`value = (raw - 128) / 16`): gates [nodata, RF, 160, 192] ->
    /// [-, -, 2.0 dB, 4.0 dB], so sub-cell 10 blends 2.0 + 2.0 / 8 = 2.25 dB
    /// and sub-cells 8/9 hold 2.0 against the missing parent.
    #[test]
    fn u16_storage_round_trips_with_its_sentinels() {
        let azimuths = full_sweep_azimuths(360);
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 1000,
            gate_count: 4,
        };
        let cut = cut_for(&azimuths, &gate_range);
        let mut grid = MomentGrid::new_u16(
            MomentType::DifferentialReflectivity,
            gate_range.clone(),
            16.0,
            128.0,
            Some(0),
            Some(1),
        );
        for row in 0..360 {
            grid.push_row(row, radar_core::MomentRow::U16(vec![0, 1, 160, 192]))
                .unwrap();
        }
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        assert!(matches!(up.grid.storage, MomentStorage::F32(_)));
        for row in 0..up.grid.radial_count() {
            for gate in 0..8 {
                assert!(
                    up.grid
                        .scaled_value(row, gate)
                        .is_none_or(|value| value.is_nan()),
                    "row {row} gate {gate}: a sentinel gate grew coverage"
                );
            }
            for (gate, expected) in [
                (8usize, 2.0f32),
                (9, 2.0),
                (10, 2.25),
                (13, 3.75),
                (14, 4.0),
                (15, 4.0),
            ] {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - expected).abs() < 1e-4,
                    "row {row} gate {gate}: {value} != {expected}"
                );
            }
        }
    }

    #[test]
    fn degenerate_inputs_return_none_or_stay_empty_without_panicking() {
        let azimuths = full_sweep_azimuths(360);
        // No radials, one radial, no gates.
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &[], 4, 1000, Vec::new());
        assert!(upsample_moment_grid(&cut, &grid).is_none());
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &[0.0], 4, 1000, vec![30.0; 4]);
        assert!(upsample_moment_grid(&cut, &grid).is_none());
        let (cut, grid) = cut_and_grid(MomentType::Reflectivity, &azimuths, 0, 1000, Vec::new());
        assert!(upsample_moment_grid(&cut, &grid).is_none());

        // A single gate still subdivides without running off the clamped
        // end: every sub-cell falls back on native gate 0.
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            1,
            1000,
            vec![42.0; 360],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("a one-gate sweep upsamples");
        assert_eq!(up.grid.gate_range.gate_count, 4);
        for row in 0..up.grid.radial_count() {
            for gate in 0..4 {
                let value = up.grid.scaled_value(row, gate).unwrap();
                assert!(
                    (value - 42.0).abs() < 1e-4,
                    "row {row} gate {gate}: {value}"
                );
            }
        }

        // An all-missing sweep upsamples to an all-missing sweep.
        let (cut, grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![f32::NAN; 360 * 4],
        );
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        assert!((0..up.grid.radial_count()).all(|row| {
            (0..up.grid.gate_range.gate_count)
                .all(|gate| up.grid.scaled_value(row, gate).unwrap().is_nan())
        }));

        // An all-range-folded U8 sweep likewise renders nothing (RF is
        // transparent in the interpolated mode, as in the Soften pass).
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 1000,
            gate_count: 4,
        };
        let cut = cut_for(&azimuths, &gate_range);
        let mut folded = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range.clone(),
            2.0,
            129.0,
            Some(0),
            Some(1),
        );
        for row in 0..360 {
            folded.push_u8_row_slice(row, &[1, 1, 1, 1]).unwrap();
        }
        let up = upsample_moment_grid(&cut, &folded).expect("upsamples");
        assert!((0..up.grid.radial_count()).all(|row| {
            (0..up.grid.gate_range.gate_count)
                .all(|gate| up.grid.scaled_value(row, gate).unwrap().is_nan())
        }));

        // radial_indices that do not address the cut are refused, not
        // indexed past the end.
        let (cut, mut grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 360 * 4],
        );
        grid.radial_indices[7] = 100_000;
        assert!(upsample_moment_grid(&cut, &grid).is_none());

        // Storage shorter than radial_indices x gates: the missing tail
        // reads as no data rather than panicking or reading another row.
        let (cut, mut grid) = cut_and_grid(
            MomentType::Reflectivity,
            &azimuths,
            4,
            1000,
            vec![30.0; 360 * 4],
        );
        grid.storage = MomentStorage::F32(vec![30.0; 40]);
        let up = upsample_moment_grid(&cut, &grid).expect("upsamples");
        assert!((up.grid.scaled_value(0, 0).unwrap() - 30.0).abs() < 1e-4);
        // Beams 10..359 have no stored row: their sub-cells stay empty.
        assert!(up.grid.scaled_value(200 * 4, 0).unwrap().is_nan());
        assert!(up.grid.scaled_value(200 * 4 + 2, 0).unwrap().is_nan());
        // The very last sub-row is NOT empty, and must not be: at t = 3/4
        // it sits at 359.75 deg, inside beam 0's own native half-cell, so
        // the containment rule paints it with beam 0's value.
        let last = up.grid.radial_count() - 1;
        assert!((up.grid.scaled_value(last, 0).unwrap() - 30.0).abs() < 1e-4);
    }
}

/// Verification against cached Level II volumes. These run only when a real
/// archive file is present (the live cache the app writes, or a directory
/// named by `RADAR_WORKSTATION_L2_CACHE`); synthetic fixtures alone never
/// establish that the display stack behaves on radar data.
#[cfg(test)]
mod real_data_tests {
    use super::*;
    use crate::smooth::smooth_moment_grid;
    use radar_core::RadarVolume;
    use std::path::PathBuf;

    fn level2_cache_dir() -> PathBuf {
        if let Some(path) = std::env::var_os("RADAR_WORKSTATION_L2_CACHE") {
            return PathBuf::from(path);
        }
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path)
                .join("FahrenheitResearch")
                .join("RadarWorkstation")
                .join("cache")
                .join("level2-live");
        }
        PathBuf::from("level2-live")
    }

    /// Every cached archive file, sorted so runs are reproducible.
    fn cached_volumes() -> Vec<PathBuf> {
        let dir = level2_cache_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with("_V06"))
            })
            .collect();
        paths.sort();
        paths
    }

    fn decode(path: &std::path::Path) -> RadarVolume {
        nexrad_io::decode_volume_from_path(path)
            .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()))
    }

    /// Number of cells that carry a finite physical value.
    fn covered_cells(grid: &MomentGrid) -> usize {
        let gates = grid.gate_range.gate_count;
        (0..grid.radial_count())
            .map(|row| {
                (0..gates)
                    .filter(|gate| {
                        grid.scaled_value(row, *gate)
                            .is_some_and(|value| value.is_finite())
                    })
                    .count()
            })
            .sum()
    }

    /// The native cell that CONTAINS an upsampled cell, for a sweep whose
    /// rows all refined (`radial_count == rows * azimuth factor`). This is
    /// the documented containment rule, not the bilinear parent plan:
    /// sub-row `s` of native row `r` sits at t = s/f, and belongs to `r`
    /// while t <= 0.5, to the next beam above it.
    fn containing_native_cell(
        out_row: usize,
        out_gate: usize,
        rows: usize,
        factors: UpsampleFactors,
    ) -> (usize, usize) {
        let native_row = out_row / factors.azimuth;
        let sub = out_row % factors.azimuth;
        let t = sub as f32 / factors.azimuth as f32;
        let row = if t <= 0.5 {
            native_row
        } else {
            (native_row + 1) % rows
        };
        (row, out_gate / factors.range)
    }

    fn nominal_azimuth_deg(cut: &ElevationCut, grid: &MomentGrid) -> f32 {
        let rows = grid.radial_count();
        let azimuths: Vec<f32> = grid
            .radial_indices
            .iter()
            .map(|index| cut.radials[*index].azimuth_deg.rem_euclid(360.0))
            .collect();
        let mut magnitudes: Vec<f32> = (0..rows)
            .map(|row| signed_delta_deg(azimuths[row], azimuths[(row + 1) % rows]).abs())
            .collect();
        magnitudes.sort_by(f32::total_cmp);
        magnitudes[magnitudes.len() / 2]
    }

    /// Softening a real sweep must leave the echo footprint bit-for-bit the
    /// same size: exactly the gates that render natively render softened.
    #[test]
    fn real_volume_soften_preserves_coverage_exactly() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut checked = 0;
        for cut in &volume.cuts {
            for (moment, grid) in &cut.moments {
                if grid.radial_count() == 0 {
                    continue;
                }
                let native = covered_cells(grid);
                let softened = smooth_moment_grid(grid);
                assert_eq!(softened.radial_count(), grid.radial_count());
                assert_eq!(softened.gate_range, grid.gate_range);
                assert_eq!(
                    covered_cells(&softened),
                    native,
                    "{} {:.2} deg {moment}: soften changed coverage",
                    volume.site.id,
                    cut.elevation_deg
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "cached volume carried no moment grids");
    }

    /// Upsampling a real sweep: geometry is exact, and echo coverage never
    /// grows once normalized by the cell-count ratio.
    #[test]
    fn real_volume_upsample_geometry_and_coverage() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut upsampled_any = false;
        for cut in &volume.cuts {
            for (moment, grid) in &cut.moments {
                if grid.radial_count() < 2 || grid.gate_range.gate_count == 0 {
                    continue;
                }
                let Some(up) = upsample_moment_grid(cut, grid) else {
                    continue;
                };
                upsampled_any = true;
                let factors = upsample_factors(
                    nominal_azimuth_deg(cut, grid),
                    grid.gate_range.gate_spacing_m,
                    grid.radial_count(),
                    grid.gate_range.gate_count,
                );
                // Gate geometry subdivides exactly and the annulus the
                // sweep paints is unchanged.
                assert_eq!(
                    up.grid.gate_range.gate_spacing_m * factors.range as i32,
                    grid.gate_range.gate_spacing_m
                );
                assert_eq!(
                    up.grid.gate_range.gate_count,
                    grid.gate_range.gate_count * factors.range
                );
                let native_inner = grid.gate_range.first_gate_m as f64
                    - grid.gate_range.gate_spacing_m as f64 / 2.0;
                let up_inner = up.grid.gate_range.first_gate_m as f64
                    - up.grid.gate_range.gate_spacing_m as f64 / 2.0;
                assert_eq!(native_inner, up_inner);
                assert_eq!(up.row_azimuths_deg.len(), up.grid.radial_count());

                let native_cells = grid.radial_count() * grid.gate_range.gate_count;
                let up_cells = up.grid.radial_count() * up.grid.gate_range.gate_count;
                let native_covered = covered_cells(grid);
                let up_covered = covered_cells(&up.grid);
                let ratio = up_cells as f64 / native_cells as f64;
                let normalized = up_covered as f64 / (native_covered as f64 * ratio);
                println!(
                    "{} {:.2} deg {moment}: native {}x{} @ {:.3} deg x {} m -> factors {}x{} -> {}x{} @ {} m; \
                     covered {native_covered} -> {up_covered} (normalized {normalized:.4})",
                    volume.site.id,
                    cut.elevation_deg,
                    grid.radial_count(),
                    grid.gate_range.gate_count,
                    nominal_azimuth_deg(cut, grid),
                    grid.gate_range.gate_spacing_m,
                    factors.azimuth,
                    factors.range,
                    up.grid.radial_count(),
                    up.grid.gate_range.gate_count,
                    up.grid.gate_range.gate_spacing_m,
                );
                assert!(
                    normalized <= 1.0 + 1e-9,
                    "{} {:.2} deg {moment}: coverage grew ({normalized})",
                    volume.site.id,
                    cut.elevation_deg
                );
            }
        }
        assert!(
            upsampled_any,
            "no cut in the cached volume was coarser than the display target"
        );
    }

    /// Every NEXRAD cut in the live cache is already 250 m in range, so the
    /// range-subdivision path only runs on legacy/international geometry.
    /// Decimating a real super-res sweep 2x in azimuth and 4x in range
    /// produces exactly that: a genuine 1.0 deg x 1000 m rendition of the
    /// same measured field. Upsampling it must land back on the original
    /// 250 m gate geometry, gate for gate.
    #[test]
    fn real_sweep_decimated_to_legacy_geometry_upsamples_back_exactly() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut examined = 0;
        for cut in &volume.cuts {
            let Some(grid) = cut.moments.get(&MomentType::Reflectivity) else {
                continue;
            };
            let rows = grid.radial_count();
            let gates = grid.gate_range.gate_count;
            if grid.gate_range.gate_spacing_m != 250 || rows < 700 || gates < 400 {
                continue;
            }
            // Legacy rendition: every 2nd beam, every 4th gate. The coarse
            // cell's centre sits 1.5 native spacings beyond the first
            // native gate centre.
            let coarse_rows = rows / 2;
            let coarse_gates = gates / 4;
            let mut data = vec![f32::NAN; coarse_rows * coarse_gates];
            for row in 0..coarse_rows {
                for gate in 0..coarse_gates {
                    if let Some(value) = grid.scaled_value(row * 2, gate * 4)
                        && value.is_finite()
                    {
                        data[row * coarse_gates + gate] = value;
                    }
                }
            }
            let coarse = MomentGrid {
                moment: MomentType::Reflectivity,
                gate_range: GateRange {
                    first_gate_m: grid.gate_range.first_gate_m + 3 * 250 / 2,
                    gate_spacing_m: 1000,
                    gate_count: coarse_gates,
                },
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..coarse_rows)
                    .map(|row| grid.radial_indices[row * 2])
                    .collect(),
                storage: MomentStorage::F32(data),
            };
            let up = upsample_moment_grid(cut, &coarse).expect("legacy geometry upsamples");
            let factors = upsample_factors(
                nominal_azimuth_deg(cut, &coarse),
                coarse.gate_range.gate_spacing_m,
                coarse_rows,
                coarse_gates,
            );
            assert_eq!((factors.azimuth, factors.range), (4, 4));
            // Back to the sweep's own 250 m lattice, gate centre included.
            assert_eq!(up.grid.gate_range.gate_spacing_m, 250);
            assert_eq!(
                up.grid.gate_range.first_gate_m,
                grid.gate_range.first_gate_m
            );
            assert_eq!(up.grid.gate_range.gate_count, coarse_gates * 4);

            let coarse_covered = covered_cells(&coarse);
            let up_covered = covered_cells(&up.grid);
            let cell_ratio = (up.grid.radial_count() * up.grid.gate_range.gate_count) as f64
                / (coarse_rows * coarse_gates) as f64;
            let normalized = up_covered as f64 / (coarse_covered as f64 * cell_ratio);
            println!(
                "{} {:.2} deg REF decimated to legacy: {coarse_rows}x{coarse_gates} @ \
                 1.000 deg x 1000 m -> factors 4x4 -> {}x{} @ {} m; covered {coarse_covered} \
                 -> {up_covered} (normalized {normalized:.4})",
                volume.site.id,
                cut.elevation_deg,
                up.grid.radial_count(),
                up.grid.gate_range.gate_count,
                up.grid.gate_range.gate_spacing_m,
            );
            assert!(normalized <= 1.0 + 1e-9, "coverage grew ({normalized})");
            examined += 1;
            break;
        }
        assert!(
            examined > 0,
            "no 250 m super-res reflectivity sweep to decimate"
        );
    }

    /// A real velocity sweep carries aliasing folds and couplets. Where the
    /// four bilinear parents span more than the 30 m/s guard the sub-cell
    /// must keep its containing native cell's value -- an unguarded blend
    /// would fabricate intermediate speeds inside the fold.
    #[test]
    fn real_velocity_fold_is_never_blended_through() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let mut examined = 0;
        for path in volumes.iter().take(4) {
            let volume = decode(path);
            for cut in &volume.cuts {
                let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
                    continue;
                };
                if grid.radial_count() < 2 {
                    continue;
                }
                let Some(up) = upsample_moment_grid(cut, grid) else {
                    continue;
                };
                let rows = grid.radial_count();
                let factors = upsample_factors(
                    nominal_azimuth_deg(cut, grid),
                    grid.gate_range.gate_spacing_m,
                    rows,
                    grid.gate_range.gate_count,
                );
                if up.grid.radial_count() != rows * factors.azimuth {
                    // Dropped/duplicate radials: the simple containment
                    // arithmetic below does not apply to this sweep.
                    continue;
                }
                // Count real fold-scale jumps between adjacent native
                // gates along range -- proof the file contains what the
                // guard exists for.
                let gates = grid.gate_range.gate_count;
                let mut fold_pairs = 0usize;
                for row in 0..rows {
                    for gate in 1..gates {
                        let a = grid.scaled_value(row, gate - 1);
                        let b = grid.scaled_value(row, gate);
                        if let (Some(a), Some(b)) = (a, b)
                            && a.is_finite()
                            && b.is_finite()
                            && (a - b).abs() > VELOCITY_GUARD_SPREAD_MPS
                        {
                            fold_pairs += 1;
                        }
                    }
                }
                if fold_pairs == 0 {
                    continue;
                }
                // Compare against the same field interpolated with no
                // guard (moment relabeled): the guard must actually change
                // the picture, and every changed cell must land exactly on
                // its containing native cell's value.
                let mut unguarded = grid.clone();
                unguarded.moment = MomentType::Unknown("TEST".to_owned());
                let plain = upsample_moment_grid(cut, &unguarded).expect("same geometry upsamples");
                let mut guarded_cells = 0usize;
                for out_row in 0..up.grid.radial_count() {
                    for out_gate in 0..up.grid.gate_range.gate_count {
                        let guarded = up.grid.scaled_value(out_row, out_gate).unwrap();
                        let linear = plain.grid.scaled_value(out_row, out_gate).unwrap();
                        if !guarded.is_finite() {
                            assert!(!linear.is_finite(), "guard changed coverage");
                            continue;
                        }
                        if (guarded - linear).abs() <= 1e-4 {
                            continue;
                        }
                        guarded_cells += 1;
                        let (native_row, native_gate) =
                            containing_native_cell(out_row, out_gate, rows, factors);
                        let native = grid.scaled_value(native_row, native_gate).unwrap();
                        assert!(
                            (guarded - native).abs() <= 1e-4,
                            "{} {:.2} deg VEL row {out_row} gate {out_gate}: guarded {guarded} \
                             is neither the blend nor the containing gate {native}",
                            volume.site.id,
                            cut.elevation_deg
                        );
                    }
                }
                println!(
                    "{} {:.2} deg VEL: {fold_pairs} native fold-scale gate pairs, \
                     {guarded_cells} sub-cells held at their native value by the 30 m/s guard",
                    volume.site.id, cut.elevation_deg
                );
                assert!(
                    guarded_cells > 0,
                    "{} {:.2} deg VEL: folds present but the guard never fired",
                    volume.site.id,
                    cut.elevation_deg
                );
                examined += 1;
            }
            if examined > 0 {
                break;
            }
        }
        assert!(
            examined > 0,
            "no cached volume offered a foldable velocity sweep to check"
        );
    }

    /// A real rho_hv sweep through a melting layer (or non-meteorological
    /// scatter) carries a population of gates below the 0.97 floor, and that
    /// depression IS the signature (Giangrande, Krause & Ryzhkov 2008). Any
    /// sub-cell whose parents straddle the floor must therefore keep its
    /// containing native cell's value, and the sweep's rho_hv distribution
    /// must stay closer to native than an unguarded blend leaves it.
    #[test]
    fn real_correlation_coefficient_minimum_survives_upsampling() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        /// Share of covered gates under the guard floor, plus the sweep
        /// minimum -- the two numbers a melting-layer read depends on.
        fn low_cc_stats(grid: &MomentGrid) -> (usize, usize, f32) {
            let gates = grid.gate_range.gate_count;
            let mut covered = 0usize;
            let mut low = 0usize;
            let mut minimum = f32::INFINITY;
            for row in 0..grid.radial_count() {
                for gate in 0..gates {
                    if let Some(value) = grid.scaled_value(row, gate)
                        && value.is_finite()
                    {
                        covered += 1;
                        minimum = minimum.min(value);
                        if value < CC_GUARD_FLOOR {
                            low += 1;
                        }
                    }
                }
            }
            (low, covered, minimum)
        }

        let mut examined = 0;
        for path in volumes.iter().take(4) {
            let volume = decode(path);
            for cut in &volume.cuts {
                let Some(grid) = cut.moments.get(&MomentType::CorrelationCoefficient) else {
                    continue;
                };
                if grid.radial_count() < 2 {
                    continue;
                }
                let Some(up) = upsample_moment_grid(cut, grid) else {
                    continue;
                };
                let rows = grid.radial_count();
                let factors = upsample_factors(
                    nominal_azimuth_deg(cut, grid),
                    grid.gate_range.gate_spacing_m,
                    rows,
                    grid.gate_range.gate_count,
                );
                if up.grid.radial_count() != rows * factors.azimuth {
                    continue;
                }
                let (native_low, native_covered, native_min) = low_cc_stats(grid);
                if native_low * 200 < native_covered {
                    // Fewer than 0.5 % sub-floor gates: no rho_hv
                    // depression on this sweep to preserve.
                    continue;
                }
                let (guarded_low, guarded_covered, guarded_min) = low_cc_stats(&up.grid);

                // The same field with the guard switched off (moment
                // relabeled) is the control.
                let mut unguarded = grid.clone();
                unguarded.moment = MomentType::Unknown("TEST".to_owned());
                let plain = upsample_moment_grid(cut, &unguarded).expect("same geometry upsamples");
                let (linear_low, linear_covered, _) = low_cc_stats(&plain.grid);
                assert_eq!(
                    guarded_covered, linear_covered,
                    "the guard must not change coverage"
                );

                // Blending is convex, so it can never reach below the
                // native minimum; the guard must not lose it either.
                assert!(
                    (guarded_min - native_min).abs() < 1e-4,
                    "{} {:.2} deg RHO: sweep minimum moved {native_min} -> {guarded_min}",
                    volume.site.id,
                    cut.elevation_deg
                );

                let mut guarded_cells = 0usize;
                for out_row in 0..up.grid.radial_count() {
                    for out_gate in 0..up.grid.gate_range.gate_count {
                        let guarded = up.grid.scaled_value(out_row, out_gate).unwrap();
                        let linear = plain.grid.scaled_value(out_row, out_gate).unwrap();
                        if !guarded.is_finite() {
                            assert!(!linear.is_finite(), "guard changed coverage");
                            continue;
                        }
                        if (guarded - linear).abs() <= 1e-6 {
                            continue;
                        }
                        guarded_cells += 1;
                        let (native_row, native_gate) =
                            containing_native_cell(out_row, out_gate, rows, factors);
                        let native = grid.scaled_value(native_row, native_gate).unwrap();
                        assert!(
                            (guarded - native).abs() <= 1e-6,
                            "{} {:.2} deg RHO row {out_row} gate {out_gate}: guarded {guarded} \
                             is neither the blend nor the containing gate {native}",
                            volume.site.id,
                            cut.elevation_deg
                        );
                    }
                }
                assert!(
                    guarded_cells > 0,
                    "{} {:.2} deg RHO: a sub-floor population is present but the guard never fired",
                    volume.site.id,
                    cut.elevation_deg
                );

                let native_fraction = native_low as f64 / native_covered as f64;
                let guarded_fraction = guarded_low as f64 / guarded_covered as f64;
                let linear_fraction = linear_low as f64 / linear_covered as f64;
                println!(
                    "{} {:.2} deg RHO: sub-0.97 share native {native_fraction:.4} \
                     ({native_low}/{native_covered}), guarded {guarded_fraction:.4}, \
                     unguarded {linear_fraction:.4}; sweep min {native_min:.3}; \
                     {guarded_cells} sub-cells held at their native value by the guard",
                    volume.site.id, cut.elevation_deg
                );
                // The guarded sweep tracks the native rho_hv distribution;
                // the unguarded blend smears across the floor and drifts.
                assert!(
                    (guarded_fraction - native_fraction).abs()
                        < (linear_fraction - native_fraction).abs(),
                    "{} {:.2} deg RHO: the guard did not keep the sub-floor share closer to native",
                    volume.site.id,
                    cut.elevation_deg
                );
                examined += 1;
            }
            if examined > 0 {
                break;
            }
        }
        assert!(
            examined > 0,
            "no cached volume offered a rho_hv sweep with a sub-floor population"
        );
    }

    /// The no-growth contract checked CELL FOR CELL on real sweeps, not in
    /// aggregate. A normalized ratio averages over ~2.6 M cells and would
    /// still read <= 1.0 with a leak at the 0/360 seam, which is one beam
    /// pair in 720. Here every painted upsampled cell must sit inside a
    /// native cell that also paints, and every beam-boundary sub-row
    /// (t = 1/2) must have echo on BOTH bracketing beams.
    #[test]
    fn real_volume_upsample_coverage_is_contained_cell_for_cell() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut checked = 0usize;
        let mut painted_total = 0usize;
        for cut in &volume.cuts {
            for (moment, grid) in &cut.moments {
                if grid.radial_count() < 2 || grid.gate_range.gate_count == 0 {
                    continue;
                }
                let Some(up) = upsample_moment_grid(cut, grid) else {
                    continue;
                };
                let rows = grid.radial_count();
                let factors = upsample_factors(
                    nominal_azimuth_deg(cut, grid),
                    grid.gate_range.gate_spacing_m,
                    rows,
                    grid.gate_range.gate_count,
                );
                if up.grid.radial_count() != rows * factors.azimuth {
                    // Dropped/duplicate radials: the containment arithmetic
                    // below assumes every native beam refined.
                    continue;
                }
                let mut leaked = 0usize;
                let mut boundary_leaked = 0usize;
                let mut painted = 0usize;
                for out_row in 0..up.grid.radial_count() {
                    let sub = out_row % factors.azimuth;
                    let t = sub as f32 / factors.azimuth as f32;
                    let native_row = out_row / factors.azimuth;
                    let (owner, other) = if t <= 0.5 {
                        (native_row, (native_row + 1) % rows)
                    } else {
                        ((native_row + 1) % rows, native_row)
                    };
                    for out_gate in 0..up.grid.gate_range.gate_count {
                        if !up
                            .grid
                            .scaled_value(out_row, out_gate)
                            .is_some_and(|value| value.is_finite())
                        {
                            continue;
                        }
                        painted += 1;
                        let native_gate = out_gate / factors.range;
                        if !grid
                            .scaled_value(owner, native_gate)
                            .is_some_and(|value| value.is_finite())
                        {
                            leaked += 1;
                        }
                        if t == 0.5
                            && !grid
                                .scaled_value(other, native_gate)
                                .is_some_and(|value| value.is_finite())
                        {
                            boundary_leaked += 1;
                        }
                    }
                }
                assert_eq!(
                    leaked, 0,
                    "{} {:.2} deg {moment}: {leaked} upsampled cells paint outside the native echo",
                    volume.site.id, cut.elevation_deg
                );
                assert_eq!(
                    boundary_leaked, 0,
                    "{} {:.2} deg {moment}: {boundary_leaked} beam-boundary cells painted from \
                     one side only",
                    volume.site.id, cut.elevation_deg
                );
                painted_total += painted;
                checked += 1;
            }
        }
        assert!(checked > 0, "no real sweep was coarse enough to upsample");
        println!(
            "{}: cell-for-cell containment holds on {checked} sweeps, {painted_total} painted \
             sub-cells, none outside its native cell",
            volume.site.id
        );
    }

    /// The 0/360 seam on real data. For every consecutive native pair --
    /// the wrap pair included -- each synthesized sub-row must sit on the
    /// SHORT arc between the two beams, at exactly `az_lo + t * delta`.
    /// A wrap bug shows up here and nowhere else: it moves one beam pair
    /// out of 720, which no aggregate coverage number can see.
    #[test]
    fn real_volume_sub_rows_stay_on_the_short_arc_across_0_360() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut checked = 0usize;
        let mut seam_pairs_total = 0usize;
        for cut in &volume.cuts {
            for (moment, grid) in &cut.moments {
                if grid.radial_count() < 2 || grid.gate_range.gate_count == 0 {
                    continue;
                }
                let Some(up) = upsample_moment_grid(cut, grid) else {
                    continue;
                };
                let rows = grid.radial_count();
                let factors = upsample_factors(
                    nominal_azimuth_deg(cut, grid),
                    grid.gate_range.gate_spacing_m,
                    rows,
                    grid.gate_range.gate_count,
                );
                if up.grid.radial_count() != rows * factors.azimuth || factors.azimuth < 2 {
                    continue;
                }
                let azimuths: Vec<f32> = grid
                    .radial_indices
                    .iter()
                    .map(|index| cut.radials[*index].azimuth_deg.rem_euclid(360.0))
                    .collect();
                let mut seam_pairs = 0usize;
                for native_row in 0..rows {
                    let lo = azimuths[native_row];
                    let hi = azimuths[(native_row + 1) % rows];
                    let delta = signed_delta_deg(lo, hi);
                    if (delta > 0.0 && hi < lo) || (delta < 0.0 && hi > lo) {
                        seam_pairs += 1;
                    }
                    let base = native_row * factors.azimuth;
                    assert!(
                        signed_delta_deg(lo, up.row_azimuths_deg[base]).abs() < 1e-3,
                        "{} {:.2} deg {moment}: native row {native_row} moved off its beam",
                        volume.site.id,
                        cut.elevation_deg
                    );
                    for step in 1..factors.azimuth {
                        let t = step as f32 / factors.azimuth as f32;
                        let actual = up.row_azimuths_deg[base + step];
                        let expected = (lo + t * delta).rem_euclid(360.0);
                        assert!(
                            signed_delta_deg(expected, actual).abs() < 1e-3,
                            "{} {:.2} deg {moment}: sub-row {step} of beam {native_row} \
                             ({lo} -> {hi}) sits at {actual}, not {expected}",
                            volume.site.id,
                            cut.elevation_deg
                        );
                        let offset = signed_delta_deg(lo, actual);
                        assert!(
                            offset.abs() <= delta.abs() + 1e-3 && offset * delta >= -1e-4,
                            "{} {:.2} deg {moment}: sub-row {step} of beam {native_row} left the \
                             short arc ({lo} -> {hi}, offset {offset})",
                            volume.site.id,
                            cut.elevation_deg
                        );
                    }
                }
                assert!(
                    seam_pairs >= 1,
                    "{} {:.2} deg {moment}: a {rows}-beam sweep never crossed 0/360",
                    volume.site.id,
                    cut.elevation_deg
                );
                seam_pairs_total += seam_pairs;
                checked += 1;
            }
        }
        assert!(checked > 0, "no real sweep synthesized sub-rows");
        println!(
            "{}: {checked} sweeps, {seam_pairs_total} beam pairs straddling 0/360, every \
             sub-row on the short arc",
            volume.site.id
        );
    }

    /// Quantifies the echo-edge rule on a real reflectivity sweep. Every
    /// NEXRAD cut in the cache is already 250 m in range, so the range
    /// factor is 1 and `gate.u` is 0 -- but the sub-cell's range neighbour
    /// still has to be finite for the azimuth blend to run, so along every
    /// range edge of an echo the sub-beam falls back on its containing
    /// native gate rather than a partial blend. That fallback must be
    /// EXACT (bit-for-bit the native value, no fade), and it must not
    /// swallow the whole sweep -- if nothing blended, the interpolated mode
    /// would be paying 2x memory to redraw the native display.
    #[test]
    fn real_volume_synthetic_beams_blend_except_at_echo_edges() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut examined = 0usize;
        for cut in &volume.cuts {
            let Some(grid) = cut.moments.get(&MomentType::Reflectivity) else {
                continue;
            };
            let rows = grid.radial_count();
            if rows < 2 || grid.gate_range.gate_count == 0 {
                continue;
            }
            let Some(up) = upsample_moment_grid(cut, grid) else {
                continue;
            };
            let factors = upsample_factors(
                nominal_azimuth_deg(cut, grid),
                grid.gate_range.gate_spacing_m,
                rows,
                grid.gate_range.gate_count,
            );
            if up.grid.radial_count() != rows * factors.azimuth || factors.azimuth < 2 {
                continue;
            }
            let mut held = 0usize;
            let mut blended = 0usize;
            for out_row in 0..up.grid.radial_count() {
                if out_row.is_multiple_of(factors.azimuth) {
                    // Native beams reproduce themselves by construction.
                    continue;
                }
                for out_gate in 0..up.grid.gate_range.gate_count {
                    let Some(value) = up
                        .grid
                        .scaled_value(out_row, out_gate)
                        .filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let (native_row, native_gate) =
                        containing_native_cell(out_row, out_gate, rows, factors);
                    let native = grid.scaled_value(native_row, native_gate).unwrap();
                    if value == native {
                        held += 1;
                    } else {
                        blended += 1;
                        // A blend is convex between the bracketing beams,
                        // so it can never leave the sweep's own range.
                        assert!(
                            value.is_finite(),
                            "{} {:.2} deg REF row {out_row} gate {out_gate}: non-finite blend",
                            volume.site.id,
                            cut.elevation_deg
                        );
                    }
                }
            }
            let total = held + blended;
            println!(
                "{} {:.2} deg REF: {total} painted synthetic sub-cells -- {blended} blended \
                 ({:.1} %), {held} held exactly at their native gate by the echo-edge rule",
                volume.site.id,
                cut.elevation_deg,
                100.0 * blended as f64 / total as f64
            );
            assert!(
                blended > 0 && held > 0,
                "{} {:.2} deg REF: expected both blended and edge-held sub-cells, got \
                 {blended}/{held}",
                volume.site.id,
                cut.elevation_deg
            );
            examined += 1;
            break;
        }
        assert!(
            examined > 0,
            "no real reflectivity sweep synthesized sub-beams"
        );
    }

    /// Measures, on real velocity data, the limitation pinned synthetically
    /// by `smooth::tests::soften_blends_across_velocity_folds_unguarded`:
    /// the Soften pass has no `InterpPolicy`, so it averages straight
    /// through aliasing folds and couplet cores that the interpolated
    /// display refuses to blend. A cell counts as smeared when its native
    /// 3x3 neighbourhood spans more than the 30 m/s guard AND softening
    /// moved it more than 10 m/s off the value actually measured there.
    ///
    /// If a future change gives `smooth.rs` the same guard as
    /// `interpolate.rs`, this assertion goes red -- that is the intended
    /// signal, not a regression; update it then.
    #[test]
    fn real_volume_soften_smears_velocity_folds_documented_limitation() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let mut examined = 0usize;
        for path in volumes.iter().take(4) {
            let volume = decode(path);
            for cut in &volume.cuts {
                let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
                    continue;
                };
                let rows = grid.radial_count();
                let gates = grid.gate_range.gate_count;
                if rows < 2 || gates == 0 {
                    continue;
                }
                let softened = smooth_moment_grid(grid);
                let mut fold_pairs = 0usize;
                let mut across_fold = 0usize;
                let mut smeared = 0usize;
                let mut worst = 0.0f32;
                for row in 0..rows {
                    for gate in 0..gates {
                        let Some(native) = grid
                            .scaled_value(row, gate)
                            .filter(|value| value.is_finite())
                        else {
                            continue;
                        };
                        if gate + 1 < gates
                            && let Some(next) = grid
                                .scaled_value(row, gate + 1)
                                .filter(|value| value.is_finite())
                            && (native - next).abs() > VELOCITY_GUARD_SPREAD_MPS
                        {
                            fold_pairs += 1;
                        }
                        let mut min = f32::INFINITY;
                        let mut max = f32::NEG_INFINITY;
                        for di in -1i64..=1 {
                            let r = (row as i64 + di).rem_euclid(rows as i64) as usize;
                            for dj in -1i64..=1 {
                                let g = gate as i64 + dj;
                                if g < 0 || g >= gates as i64 {
                                    continue;
                                }
                                if let Some(neighbour) = grid
                                    .scaled_value(r, g as usize)
                                    .filter(|value| value.is_finite())
                                {
                                    min = min.min(neighbour);
                                    max = max.max(neighbour);
                                }
                            }
                        }
                        if max - min <= VELOCITY_GUARD_SPREAD_MPS {
                            continue;
                        }
                        across_fold += 1;
                        let shift = (softened.scaled_value(row, gate).unwrap() - native).abs();
                        worst = worst.max(shift);
                        if shift > 10.0 {
                            smeared += 1;
                        }
                    }
                }
                if fold_pairs == 0 {
                    continue;
                }
                println!(
                    "{} {:.2} deg VEL soften: {fold_pairs} native fold-scale gate pairs, \
                     {across_fold} gates whose 3x3 window spans > {VELOCITY_GUARD_SPREAD_MPS} m/s, \
                     {smeared} moved more than 10 m/s off the measured value (worst {worst:.1} m/s)",
                    volume.site.id, cut.elevation_deg
                );
                assert!(
                    smeared > 0,
                    "{} {:.2} deg VEL: the Soften pass appears to have gained a fold guard -- \
                     update this test and the note in smooth.rs",
                    volume.site.id,
                    cut.elevation_deg
                );
                examined += 1;
            }
            if examined > 0 {
                break;
            }
        }
        assert!(
            examined > 0,
            "no cached volume offered a foldable velocity sweep to measure"
        );
    }

    /// Softening on real data must stay inside the measurements: every
    /// softened value has to lie within the min/max of the FINITE native
    /// gates in its 3x3 azimuth x range neighbourhood. A nodata or
    /// range-folded code leaking into the average as a number (raw 0 scales
    /// to -33 dBZ on a NEXRAD REF grid) would break that bound at the first
    /// echo edge it touched.
    #[test]
    fn real_volume_soften_stays_inside_the_measured_neighbourhood() {
        let volumes = cached_volumes();
        if volumes.is_empty() {
            eprintln!("no cached Level II volumes; skipping");
            return;
        }
        let volume = decode(&volumes[0]);
        let mut checked = 0usize;
        let mut edge_cells = 0usize;
        for cut in &volume.cuts {
            for (moment, grid) in &cut.moments {
                let rows = grid.radial_count();
                let gates = grid.gate_range.gate_count;
                if rows < 2 || gates == 0 {
                    continue;
                }
                let softened = smooth_moment_grid(grid);
                for row in 0..rows {
                    for gate in 0..gates {
                        let Some(value) = softened
                            .scaled_value(row, gate)
                            .filter(|value| value.is_finite())
                        else {
                            continue;
                        };
                        let mut min = f32::INFINITY;
                        let mut max = f32::NEG_INFINITY;
                        let mut missing_neighbours = 0usize;
                        for di in -1i64..=1 {
                            let r = (row as i64 + di).rem_euclid(rows as i64) as usize;
                            for dj in -1i64..=1 {
                                let g = gate as i64 + dj;
                                if g < 0 || g >= gates as i64 {
                                    continue;
                                }
                                match grid
                                    .scaled_value(r, g as usize)
                                    .filter(|value| value.is_finite())
                                {
                                    Some(neighbour) => {
                                        min = min.min(neighbour);
                                        max = max.max(neighbour);
                                    }
                                    None => missing_neighbours += 1,
                                }
                            }
                        }
                        if missing_neighbours > 0 {
                            edge_cells += 1;
                        }
                        assert!(
                            value >= min - 1e-3 && value <= max + 1e-3,
                            "{} {:.2} deg {moment} row {row} gate {gate}: softened {value} \
                             outside the measured neighbourhood [{min}, {max}]",
                            volume.site.id,
                            cut.elevation_deg
                        );
                    }
                }
                checked += 1;
                if checked >= 6 {
                    break;
                }
            }
            if checked >= 6 {
                break;
            }
        }
        assert!(checked > 0, "cached volume carried no moment grids");
        println!(
            "{}: soften stayed inside the measured 3x3 neighbourhood on {checked} sweeps \
             ({edge_cells} cells had at least one missing/RF neighbour)",
            volume.site.id
        );
    }
}
