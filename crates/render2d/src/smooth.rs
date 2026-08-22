//! Polar-domain smoothing for display: a NaN-aware 3x3 binomial kernel
//! ([1 2 1] outer [1 2 1]) over azimuth x range on the moment's physical
//! values. Smoothing the GRID once (cached per volume/cut/product by the
//! render worker) and rendering it through the existing nearest-gate fast
//! path keeps pans at full speed -- the smoothed look costs one ~5-10 ms
//! pass per product instead of per-pixel work every frame.
//!
//! The kernel is the separable binomial (Pascal row 2) filter, the standard
//! discrete Gaussian approximation (Gonzalez & Woods, *Digital Image
//! Processing*, 4th ed., Pearson 2018, ch. 3). Applying it on the polar
//! lattice rather than the screen raster is what keeps the softened look
//! zoom-invariant.
//!
//! Range-folded and missing gates contribute nothing (weights renormalize);
//! a gate with no finite neighbors stays empty. Note: RF gates therefore
//! render transparent in smoothed mode -- analysts who need the RF purple
//! should use the native (unsmoothed) display.

use radar_core::{MomentGrid, MomentStorage};
use rayon::prelude::*;

/// Smooth a moment grid's values into a new F32 grid with identical
/// geometry. Azimuth wraps; range is clamped at the ends.
pub fn smooth_moment_grid(grid: &MomentGrid) -> MomentGrid {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let mut values = vec![f32::NAN; rows * gates];
    if rows > 0 && gates > 0 {
        // Materialize scaled values once (NaN for missing/RF).
        let mut source = vec![f32::NAN; rows * gates];
        source
            .par_chunks_mut(gates)
            .enumerate()
            .for_each(|(row, out_row)| {
                for (gate, cell) in out_row.iter_mut().enumerate() {
                    if let Some(v) = grid.scaled_value(row, gate).filter(|v| v.is_finite()) {
                        *cell = v;
                    }
                }
            });
        const KERNEL: [f32; 3] = [1.0, 2.0, 1.0];
        values
            .par_chunks_mut(gates)
            .enumerate()
            .for_each(|(row, out_row)| {
                for (gate, cell) in out_row.iter_mut().enumerate() {
                    // A gate only renders where the native display would --
                    // smoothing must not grow coverage.
                    if !source[row * gates + gate].is_finite() {
                        continue;
                    }
                    let mut sum = 0.0f32;
                    let mut weight = 0.0f32;
                    for (di, &kr) in KERNEL.iter().enumerate() {
                        let r = ((row as i64 + di as i64 - 1).rem_euclid(rows as i64)) as usize;
                        for (dj, &kg) in KERNEL.iter().enumerate() {
                            let g = gate as i64 + dj as i64 - 1;
                            if g < 0 || g >= gates as i64 {
                                continue;
                            }
                            let v = source[r * gates + g as usize];
                            if v.is_finite() {
                                let k = kr * kg;
                                sum += v * k;
                                weight += k;
                            }
                        }
                    }
                    if weight > 0.0 {
                        *cell = sum / weight;
                    }
                }
            });
    }
    MomentGrid {
        moment: grid.moment.clone(),
        producer_name: grid.producer_name.clone(),
        producer_description: grid.producer_description.clone(),
        producer_units: grid.producer_units.clone(),
        gate_range: grid.gate_range.clone(),
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        // Smoothing one sweep does not un-censor it.
        snr_threshold_db: grid.snr_threshold_db,
        recombination: grid.recombination,
        radial_indices: grid.radial_indices.clone(),
        storage: MomentStorage::F32(values),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentType};

    fn grid(rows: usize, gates: usize, data: Vec<f32>) -> MomentGrid {
        MomentGrid {
            moment: MomentType::Reflectivity,
            producer_name: None,
            producer_description: None,
            producer_units: None,
            gate_range: GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: gates,
            },
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            snr_threshold_db: None,
            recombination: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(data),
        }
    }

    #[test]
    fn uniform_field_is_unchanged() {
        let g = grid(8, 8, vec![35.0; 64]);
        let s = smooth_moment_grid(&g);
        for row in 0..8 {
            for gate in 0..8 {
                let v = s.scaled_value(row, gate).unwrap();
                assert!((v - 35.0).abs() < 1e-4, "{v}");
            }
        }
    }

    #[test]
    fn steps_soften_and_coverage_does_not_grow() {
        // Left half 20 dBZ, right half NaN.
        let mut data = vec![f32::NAN; 64];
        for row in 0..8 {
            for gate in 0..4 {
                data[row * 8 + gate] = 20.0;
            }
        }
        let s = smooth_moment_grid(&grid(8, 8, data));
        // Edge gate keeps its value (NaN neighbors renormalize)...
        assert!((s.scaled_value(0, 3).unwrap() - 20.0).abs() < 1e-4);
        // ...and empty gates STAY empty (no coverage bleed).
        assert!(s.scaled_value(0, 4).is_none_or(|v| v.is_nan()));
    }

    #[test]
    fn interior_step_blends() {
        // Gate column 4 jumps 0 -> 40: smoothed neighbors blend toward each
        // other across the step.
        let mut data = vec![0.0f32; 64];
        for row in 0..8 {
            for gate in 4..8 {
                data[row * 8 + gate] = 40.0;
            }
        }
        let s = smooth_moment_grid(&grid(8, 8, data));
        let low_side = s.scaled_value(3, 3).unwrap();
        let high_side = s.scaled_value(3, 4).unwrap();
        assert!(low_side > 0.0 && low_side < 20.0, "{low_side}");
        assert!(high_side > 20.0 && high_side < 40.0, "{high_side}");
    }

    /// Hand-computed kernel arithmetic: a 3x3 field of 1.0 with a single
    /// 10.0 at the centre. All nine taps are finite (azimuth wraps, gate 1
    /// is interior), the centre tap weighs 4 of the total 16, so
    /// (4*10 + 12*1) / 16 = 3.25.
    #[test]
    fn kernel_weights_are_the_binomial_1_2_1_outer_product() {
        let mut data = vec![1.0f32; 9];
        data[3 + 1] = 10.0;
        let s = smooth_moment_grid(&grid(3, 3, data));
        let v = s.scaled_value(1, 1).unwrap();
        assert!((v - 3.25).abs() < 1e-5, "{v}");
    }

    /// Range clamps instead of wrapping: at gate 0 the three dj = -1 taps
    /// fall outside the grid and drop out, renormalizing over the remaining
    /// 3x2 block (total weight 12, gate-0 column 8). Gate 0 holds 0.0 and
    /// gates 1..3 hold 4.0, so (0*8 + 4*4) / 12 = 4/3.
    #[test]
    fn range_clamps_at_the_first_gate() {
        let mut data = vec![4.0f32; 9];
        for row in 0..3 {
            data[row * 3] = 0.0;
        }
        let s = smooth_moment_grid(&grid(3, 3, data));
        let v = s.scaled_value(1, 0).unwrap();
        assert!((v - 4.0 / 3.0).abs() < 1e-5, "{v}");
    }

    /// Azimuth wraps: row 0's di = -1 neighbor is the last row. With only
    /// row 2 carrying 8.0 and the rest 0.0, row 0 smooths to (8*4) / 16 =
    /// 2.0 -- non-zero only because the sweep closes on itself.
    #[test]
    fn azimuth_wraps_across_the_sweep_seam() {
        let mut data = vec![0.0f32; 9];
        for gate in 0..3 {
            data[2 * 3 + gate] = 8.0;
        }
        let s = smooth_moment_grid(&grid(3, 3, data));
        let v = s.scaled_value(0, 1).unwrap();
        assert!((v - 2.0).abs() < 1e-5, "{v}");
    }

    /// The output is always a plain F32 grid with unity scaling and no
    /// nodata/RF sentinels, and keeps the native geometry and radial
    /// linkage so the nearest-gate fast path reads it unchanged.
    #[test]
    fn output_is_unscaled_f32_with_native_geometry() {
        let mut g = grid(4, 5, vec![12.0; 20]);
        g.scale = 2.0;
        g.offset = -3.0;
        g.nodata = Some(0);
        g.range_folded = Some(1);
        let s = smooth_moment_grid(&g);
        assert_eq!(s.scale, 1.0);
        assert_eq!(s.offset, 0.0);
        assert_eq!(s.nodata, None);
        assert_eq!(s.range_folded, None);
        assert_eq!(s.gate_range, g.gate_range);
        assert_eq!(s.radial_indices, g.radial_indices);
        assert_eq!(s.radial_count(), 4);
        assert!(matches!(s.storage, MomentStorage::F32(ref v) if v.len() == 20));
    }

    /// Range-folded and nodata gates are dropped from the source field and,
    /// having no finite value of their own, stay empty. Exercises the
    /// sentinel path on a U8 grid carrying NEXRAD REF scaling
    /// (value = (raw - offset) / scale): raw 100 is (100 - 66) / 2 = 17 dBZ,
    /// and a uniform block of it stays 17 dBZ.
    #[test]
    fn range_folded_and_nodata_gates_stay_empty() {
        let mut g = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        for row in 0..4 {
            g.push_u8_row_slice(row, &[0, 1, 100, 100]).unwrap();
        }
        let s = smooth_moment_grid(&g);
        assert!(s.scaled_value(0, 0).unwrap().is_nan());
        assert!(s.scaled_value(0, 1).unwrap().is_nan());
        assert!((s.scaled_value(0, 2).unwrap() - 17.0).abs() < 1e-4);
        assert!((s.scaled_value(0, 3).unwrap() - 17.0).abs() < 1e-4);
    }

    /// The mirror of `azimuth_wraps_across_the_sweep_seam`: the LAST row's
    /// di = +1 neighbour is row 0. Only row 0 carries 8.0, so row 2 smooths
    /// to (8 * 1 * 4) / 16 = 2.0 -- non-zero only because the sweep closes
    /// on itself in the forward direction too.
    #[test]
    fn azimuth_wrap_is_symmetric_at_the_last_row() {
        let mut data = vec![0.0f32; 9];
        data[..3].fill(8.0);
        let s = smooth_moment_grid(&grid(3, 3, data));
        let v = s.scaled_value(2, 1).unwrap();
        assert!((v - 2.0).abs() < 1e-5, "{v}");
    }

    /// A gate with no finite neighbours keeps its exact value: the centre
    /// tap alone carries weight 4 and renormalizes to 1. An isolated echo
    /// must not dim toward the empty space around it.
    #[test]
    fn isolated_gate_keeps_its_exact_value() {
        let mut data = vec![f32::NAN; 9];
        data[3 + 1] = 55.0;
        let s = smooth_moment_grid(&grid(3, 3, data));
        assert!((s.scaled_value(1, 1).unwrap() - 55.0).abs() < 1e-6);
        for row in 0..3 {
            for gate in 0..3 {
                if (row, gate) == (1, 1) {
                    continue;
                }
                assert!(
                    s.scaled_value(row, gate).unwrap().is_nan(),
                    "row {row} gate {gate} grew coverage"
                );
            }
        }
    }

    /// U16 storage with sentinels, ZDR-scaled (`value = (raw - 128) / 16`):
    /// row [nodata, RF, 160, 192] -> [-, -, 2.0 dB, 4.0 dB] on every row.
    /// Hand-computed: gate 2's window drops the RF gate, leaving the gate-2
    /// column (weight 2) and the gate-3 column (weight 1) over three
    /// identical rows -- (2.0 * 2 + 4.0 * 1) / 3 = 8/3 dB. Gate 3 loses the
    /// dj = +1 column off the end -- (2.0 * 1 + 4.0 * 2) / 3 = 10/3 dB.
    /// Averaging the nodata code as a number would inject
    /// (0 - 128) / 16 = -8.0 dB and drag both far negative.
    #[test]
    fn u16_sentinels_are_never_averaged_into_values() {
        let mut g = MomentGrid::new_u16(
            MomentType::DifferentialReflectivity,
            GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            16.0,
            128.0,
            Some(0),
            Some(1),
        );
        for row in 0..3 {
            g.push_row(row, radar_core::MomentRow::U16(vec![0, 1, 160, 192]))
                .unwrap();
        }
        let s = smooth_moment_grid(&g);
        for row in 0..3 {
            assert!(s.scaled_value(row, 0).unwrap().is_nan());
            assert!(s.scaled_value(row, 1).unwrap().is_nan());
            let two = s.scaled_value(row, 2).unwrap();
            let three = s.scaled_value(row, 3).unwrap();
            assert!((two - 8.0 / 3.0).abs() < 1e-5, "row {row} gate 2: {two}");
            assert!(
                (three - 10.0 / 3.0).abs() < 1e-5,
                "row {row} gate 3: {three}"
            );
        }
    }

    /// One radial and one gate are legal shapes. With a single radial the
    /// azimuth taps all fold onto row 0 (weights 1 + 2 + 1 = 4), so
    /// [1, 2, 3] smooths at gate 1 to (1 + 2*2 + 3) / 4 = 2.0; with a
    /// single gate the range taps off both ends drop and the same 2.0
    /// falls out along azimuth.
    #[test]
    fn single_radial_and_single_gate_shapes_are_exact() {
        let one_row = smooth_moment_grid(&grid(1, 3, vec![1.0, 2.0, 3.0]));
        assert!((one_row.scaled_value(0, 1).unwrap() - 2.0).abs() < 1e-5);
        let one_gate = smooth_moment_grid(&grid(3, 1, vec![1.0, 2.0, 3.0]));
        assert!((one_gate.scaled_value(1, 0).unwrap() - 2.0).abs() < 1e-5);
    }

    /// Storage shorter than `radial_indices.len() * gate_count` reads as no
    /// data for the missing tail rather than panicking or wrapping into the
    /// next row.
    #[test]
    fn short_storage_reads_as_missing() {
        let mut g = grid(4, 4, vec![20.0; 8]);
        g.radial_indices = (0..4).collect();
        let s = smooth_moment_grid(&g);
        assert_eq!(s.radial_count(), 4);
        assert!(s.scaled_value(0, 1).unwrap().is_finite());
        assert!(s.scaled_value(2, 1).unwrap().is_nan());
        assert!(s.scaled_value(3, 1).unwrap().is_nan());
    }

    /// DOCUMENTED LIMITATION, pinned so it is a decision and not a
    /// surprise: unlike `interpolate.rs`, the Soften pass carries NO
    /// per-moment guard. Beams alternating +26 / -26 m/s (a 52 m/s
    /// fold-scale jump) smooth to exactly (-26 + 2*26 - 26) / 4 = 0.0 m/s
    /// -- a velocity that was never measured, sitting where the fold is.
    /// The interpolated display refuses this blend (30 m/s guard, matching
    /// `volumetric.rs`); softened velocity does not. Analysts reading
    /// couplets or aliasing should use the native or interpolated display,
    /// or this pass needs the same `InterpPolicy` treatment.
    #[test]
    fn soften_blends_across_velocity_folds_unguarded() {
        let mut data = vec![0.0f32; 12];
        for row in 0..4 {
            let value = if row % 2 == 0 { 26.0 } else { -26.0 };
            for gate in 0..3 {
                data[row * 3 + gate] = value;
            }
        }
        let mut g = grid(4, 3, data);
        g.moment = MomentType::Velocity;
        let s = smooth_moment_grid(&g);
        let v = s.scaled_value(0, 1).unwrap();
        assert!(
            v.abs() < 1e-5,
            "expected the unguarded 0.0 m/s blend, got {v}"
        );
    }

    #[test]
    fn degenerate_grids_do_not_panic() {
        let empty = grid(0, 8, Vec::new());
        assert_eq!(smooth_moment_grid(&empty).radial_count(), 0);
        let no_gates = grid(4, 0, Vec::new());
        let s = smooth_moment_grid(&no_gates);
        assert_eq!(s.radial_count(), 4);
        assert_eq!(s.gate_range.gate_count, 0);
    }
}
