//! Beam-stack support metadata and the conservative min/max hierarchy that the
//! second-generation 3D renderer traverses.
//!
//! Two independent pieces live here, both consumed by the 3D volume explorer
//! and neither of which changes a physical field:
//!
//! * **Beam-stack support** — how directly a Cartesian voxel is constrained by
//!   the tilts that were actually flown. A voxel sitting on a beam centre at
//!   short range is well constrained; one interpolated across a five-degree
//!   tilt gap, or extrapolated above the top cut, is not.
//! * **Conservative min/max hierarchy** — a tiny two-level summary of the
//!   uploaded box so the ray traverser can jump over regions that cannot
//!   contribute under the active transfer function.
//!
//! Support is a DISPLAY AID, not radar QC. It says nothing about calibration,
//! attenuation, partial beam blockage, biological or ground contamination,
//! dealiasing confidence, or network quality, and it is not a formal
//! uncertainty. It describes the reconstruction geometry only: which beams
//! were near this point in space, and how far the value had to travel to get
//! here. Everything that presents it must keep that wording.
//!
//! Vertical reconstruction follows the MRMS treatment (Zhang, Howard &
//! Gourley 2005, *J. Atmos. Oceanic Technol.* 22(1), 30-42, Eqs. 5-7; edge
//! rules from Zhang et al. 2011, *Bull. Amer. Meteor. Soc.* 92(10), 1321-1338)
//! and the 4/3-effective-earth beam geometry of Doviak & Zrnic 1993, *Doppler
//! Radar and Weather Observations*, 2nd ed., Eqs. 2.28b/2.28c.

use rayon::prelude::*;

/// WSR-88D half-power HALF-beamwidth, radians: a 0.95 degree aperture halved.
///
/// Mirrors `volumetric::HALF_BW_RAD`, which is private to that module. The
/// numeric value is pinned by [`tests::half_beamwidth_matches_wsr88d_aperture`]
/// so the two cannot drift silently.
pub const HALF_POWER_HALF_BEAMWIDTH_RAD: f64 = 0.475 * std::f64::consts::PI / 180.0;

/// 4/3-effective-earth radius, m (Doviak & Zrnic 1993 Eq. 2.28 model).
const EFFECTIVE_EARTH_RADIUS_M: f64 = 4.0 / 3.0 * 6_371_000.0;

/// One entry of the beam stack over a single Cartesian column: where a tilt's
/// beam centre passed, expressed at the column's ground distance.
///
/// `slant_range_m` is the range at which that tilt reaches the column, not the
/// gate's own range index; it sets the physical beam radius used to decide how
/// far a value may legitimately be carried.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeamStackSample {
    /// Beam-centre height above the radar, m.
    pub height_m: f64,
    /// Tilt elevation, degrees.
    pub elevation_deg: f64,
    /// Slant range from the antenna to this column, m.
    pub slant_range_m: f64,
}

/// Elevation angle of the beam that passes through (ground arc `s`, height
/// `h`), degrees — the closed-form inverse of the 4/3-earth height relation.
///
/// Law of cosines on the effective sphere: with the antenna and the point both
/// referred to the earth's centre, the slant range follows from the included
/// central angle `s / a_e`, and the elevation follows from the same triangle.
/// [`tests::beam_elevation_round_trips_through_forward_geometry`] checks it
/// against this crate's forward [`crate::beam`] model rather than against a
/// second copy of the same algebra.
pub fn beam_elevation_deg(ground_arc_m: f64, height_m: f64) -> f64 {
    beam_inverse(ground_arc_m, height_m).1
}

/// `(slant range m, elevation deg)` at a ground arc and height.
fn beam_inverse(ground_arc_m: f64, height_m: f64) -> (f64, f64) {
    let radius = EFFECTIVE_EARTH_RADIUS_M;
    let central_angle = ground_arc_m / radius;
    let top = radius + height_m;
    let slant_range_m = (radius * radius + top * top - 2.0 * radius * top * central_angle.cos())
        .max(0.0)
        .sqrt();
    if slant_range_m < 1.0 {
        // Directly overhead: the triangle degenerates and the elevation is 90.
        return (0.0, 90.0);
    }
    let sin_elevation = ((top * top - radius * radius - slant_range_m * slant_range_m)
        / (2.0 * radius * slant_range_m))
        .clamp(-1.0, 1.0);
    (slant_range_m, sin_elevation.asin().to_degrees())
}

/// Score how directly the beam stack constrains height `z_m` over a column at
/// ground distance `ground_arc_m`, as `1..=255`.
///
/// **This is a reconstruction-support display aid, never radar QC, confidence,
/// or uncertainty.** 0 is reserved by the caller for "no data": this function
/// never returns it, so the support field stays an unambiguous no-data mask.
///
/// Three regimes, in the order the reconstruction itself uses them:
///
/// * below the lowest cut — extrapolation downward, capped at 0.74;
/// * above the highest cut — extrapolation upward, capped at 0.60, because the
///   storm top is the reconstruction's least constrained region and the one
///   most often mistaken for a real overshooting top;
/// * inside the stack — a weighted blend of how close the nearest beam centre
///   passed (`directness`), how wide the tilt gap being spanned is
///   (`gap_score`), and whether the target height sits at a beam or halfway
///   between two (`endpoint`). The three weights plus the floor sum to 1.
///
/// `stack` must be sorted ascending by height and must be non-empty; an empty
/// stack scores the minimum. The caller is responsible for only WRITING this
/// score where the reconstruction actually produced a value — the MRMS edge
/// rule refuses to extend a value more than half a beamwidth past the end cut,
/// and a score written past that point would claim support for a voxel that
/// carries no data.
pub fn beam_support_score(stack: &[BeamStackSample], z_m: f64, ground_arc_m: f64) -> u8 {
    let Some(first) = stack.first() else {
        return 1;
    };
    let last = stack[stack.len() - 1];
    let score = if z_m <= first.height_m {
        // Downward extrapolation. The reconstruction keeps a 300 m display
        // floor near the radar where half a beamwidth is only metres wide, so
        // the score fades over whichever of the two is larger.
        let extend = (first.slant_range_m * HALF_POWER_HALF_BEAMWIDTH_RAD).max(300.0);
        let distance = (first.height_m - z_m).max(0.0);
        0.18 + 0.56 * (1.0 - distance / extend.max(1.0)).clamp(0.0, 1.0)
    } else if z_m >= last.height_m {
        let extend = (last.slant_range_m * HALF_POWER_HALF_BEAMWIDTH_RAD).max(1.0);
        let distance = (z_m - last.height_m).max(0.0);
        0.12 + 0.48 * (1.0 - distance / extend).clamp(0.0, 1.0)
    } else {
        let elevation = beam_elevation_deg(ground_arc_m, z_m);
        let mut score = 0.45;
        for pair in stack.windows(2) {
            let (lo, hi) = (pair[0], pair[1]);
            if z_m < lo.height_m || z_m > hi.height_m {
                continue;
            }
            let nearest_dz = (z_m - lo.height_m).abs().min((hi.height_m - z_m).abs());
            let beam_radius =
                (0.5 * (lo.slant_range_m + hi.slant_range_m) * HALF_POWER_HALF_BEAMWIDTH_RAD)
                    .max(150.0);
            // Gaussian in units of the beam radius: the two-way power pattern
            // is Gaussian to a good approximation, so a voxel one beam radius
            // off the centre is already substantially less constrained.
            let directness = (-0.5 * (nearest_dz / beam_radius).powi(2)).exp();
            // A 1.5 degree gap halves this term; the VCP 12 lower deck is
            // tighter than that, the upper deck is not.
            let angular_gap = (hi.elevation_deg - lo.elevation_deg).abs();
            let gap_score = 1.0 / (1.0 + (angular_gap / 1.5).powi(2));
            let span = (hi.elevation_deg - lo.elevation_deg).abs().max(1.0e-6);
            let weight = ((elevation - lo.elevation_deg) / span).clamp(0.0, 1.0);
            let endpoint = 1.0 - 2.0 * weight.min(1.0 - weight);
            score = 0.30 + 0.38 * directness + 0.20 * gap_score + 0.12 * endpoint;
            break;
        }
        score
    };
    ((score.clamp(0.0, 1.0) * 255.0).round() as u8).max(1)
}

/// A Cartesian volume plus its beam-stack support field.
///
/// `support` is row-major `[z][y][x]` on the same lattice as `values`: 0 where
/// the reconstruction produced no value, 1..=255 for increasingly direct beam
/// constraint. It is named support rather than confidence on purpose — see the
/// module documentation for what it deliberately does not include.
#[derive(Clone, Debug)]
pub struct VolumeBoxResample {
    pub values: Vec<f32>,
    pub support: Vec<u8>,
}

/// Edge length in voxels of one fine hierarchy brick.
pub const FINE_BRICK: usize = 8;
/// Fine bricks aggregated per coarse cell along east-west.
pub const COARSE_GROUP_X: usize = 4;
/// Fine bricks aggregated per coarse cell along north-south.
pub const COARSE_GROUP_Y: usize = 4;
/// Fine bricks aggregated per coarse cell along the vertical.
pub const COARSE_GROUP_Z: usize = 3;

/// Dimensions of one hierarchy level, in cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HierarchyDims {
    pub x: usize,
    pub y: usize,
    pub z: usize,
}

impl HierarchyDims {
    pub fn len(self) -> usize {
        self.x * self.y * self.z
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn index(self, x: usize, y: usize, z: usize) -> usize {
        z * self.x * self.y + y * self.x + x
    }
}

/// Fine-level dimensions for an `n * n * nz` box.
pub fn fine_dims(n: usize, nz: usize) -> HierarchyDims {
    HierarchyDims {
        x: n.div_ceil(FINE_BRICK),
        y: n.div_ceil(FINE_BRICK),
        z: nz.div_ceil(FINE_BRICK),
    }
}

/// Coarse-level dimensions for an `n * n * nz` box.
pub fn coarse_dims(n: usize, nz: usize) -> HierarchyDims {
    let fine = fine_dims(n, nz);
    HierarchyDims {
        x: fine.x.div_ceil(COARSE_GROUP_X),
        y: fine.y.div_ceil(COARSE_GROUP_Y),
        z: fine.z.div_ceil(COARSE_GROUP_Z),
    }
}

/// GPU-resident acceleration structure for one uploaded box.
///
/// `fine_minmax` and `coarse_minmax` are RGBA8 texel data:
/// `r` = minimum normalized field, `g` = maximum, `b` = minimum support,
/// `a` = maximum support. A cell whose `a` is 0 contains no observed data
/// anywhere it can influence and is therefore fully transparent in every
/// threshold mode.
#[derive(Clone, Debug)]
pub struct VolumeAcceleration {
    /// The support field, resized to exactly `n * n * nz`.
    pub support: Vec<u8>,
    pub fine_minmax: Vec<u8>,
    pub coarse_minmax: Vec<u8>,
    pub fine_dims: HierarchyDims,
    pub coarse_dims: HierarchyDims,
    /// Fraction of fine bricks with no observed data. Telemetry only.
    pub empty_fine_fraction: f32,
}

fn clamp_index(value: isize, len: usize) -> usize {
    value.clamp(0, len as isize - 1) as usize
}

/// Inclusive voxel range along one axis that a hierarchy cell can blend.
///
/// Cell `cell` of `cells` owns `uvw` in `[cell/cells, (cell + 1)/cells)`. The
/// GPU turns that into texel coordinate `uvw * len - 0.5` and blends it with
/// its successor, so the reachable indices are
/// `floor(cell * len / cells) - 1 ..= ceil((cell + 1) * len / cells)`,
/// afterwards clamped by `ClampToEdge`.
///
/// For the shipped 192-over-24 and 48-over-6 lattices this is exactly the
/// brick's own eight voxels plus a one-voxel apron. It is derived from the
/// lattice rather than written as a fixed eight-voxel stride because the two
/// only agree when the voxel count is a multiple of the cell count: at, say,
/// 20 voxels over 3 cells the last cell owns voxels 12..=19 while a fixed
/// stride would bound only 15..=19, and the traverser could then skip a cell
/// holding three voxels' worth of echo.
fn axis_span(cell: usize, cells: usize, len: usize) -> (isize, isize) {
    let lo = (cell * len / cells) as isize - 1;
    let hi = ((cell + 1) * len).div_ceil(cells) as isize;
    (lo, hi)
}

/// Conservative bounds over one fine brick INCLUDING its one-voxel apron.
///
/// The apron is the whole point. The GPU samples the box with trilinear
/// filtering, so a ray sample anywhere inside the brick's own spatial extent
/// blends the eight voxels around it — and near a face, four of those eight
/// live in the neighbouring brick. Bounds taken over the brick's own voxels
/// alone are therefore NOT bounds on the reconstructed field over the brick,
/// and a traverser trusting them can skip a cell whose interpolated values
/// would have been visible. Widening to [`axis_span`] makes every trilinear
/// sample inside the brick a convex combination of voxels that were counted,
/// which is exactly the condition the skip test needs.
///
/// No-data voxels are counted in the VALUE bounds even though they carry no
/// observation, because the sampler blends their stored 0 into neighbouring
/// samples. Excluding them would raise the minimum above values the shader can
/// actually produce, and `Below`/`Outside` thresholds would then skip real
/// echo fringes. They are excluded only from the "is this brick observed at
/// all" decision, which is what the support channel answers.
fn fine_brick_bounds(
    values: &[u8],
    support: &[u8],
    n: usize,
    nz: usize,
    dims: HierarchyDims,
    brick: usize,
) -> [u8; 4] {
    let fx = brick % dims.x;
    let fy = (brick / dims.x) % dims.y;
    let fz = brick / (dims.x * dims.y);
    let (x_lo, x_hi) = axis_span(fx, dims.x, n);
    let (y_lo, y_hi) = axis_span(fy, dims.y, n);
    let (z_lo, z_hi) = axis_span(fz, dims.z, nz);
    let mut min_v = u8::MAX;
    let mut max_v = 0u8;
    let mut min_s = u8::MAX;
    let mut max_s = 0u8;
    for lz in z_lo..=z_hi {
        let z = clamp_index(lz, nz);
        for ly in y_lo..=y_hi {
            let y = clamp_index(ly, n);
            let row = z * n * n + y * n;
            for lx in x_lo..=x_hi {
                let x = clamp_index(lx, n);
                let index = row + x;
                let value = values[index];
                let score = support[index];
                min_v = min_v.min(value);
                max_v = max_v.max(value);
                min_s = min_s.min(score);
                max_s = max_s.max(score);
            }
        }
    }
    if max_s == 0 {
        // Nothing observed anywhere this brick can be sampled from. Report an
        // empty range so the traverser skips it without consulting the values.
        return [0, 0, 0, 0];
    }
    [min_v, max_v, min_s, max_s]
}

/// Inclusive fine-cell range one coarse cell covers along one axis.
///
/// Same argument as [`axis_span`], one level up: the coarse cell must aggregate
/// every fine brick whose `uvw` interval it overlaps, which is
/// `floor(cell * children / cells) ..= ceil((cell + 1) * children / cells) - 1`.
/// That reduces to `cell * GROUP ..= cell * GROUP + GROUP - 1` whenever the
/// fine count divides evenly, and is wider — never narrower — when it does not.
fn child_span(cell: usize, cells: usize, children: usize) -> (usize, usize) {
    let first = cell * children / cells;
    let last = ((cell + 1) * children)
        .div_ceil(cells)
        .saturating_sub(1)
        .min(children - 1);
    (first.min(last), last)
}

/// Build the two-level conservative hierarchy for an already-normalized box.
///
/// `normalized` is the STRUCTURE box in 0..=255 texels — the same texture the
/// shader samples for geometry, opacity and thresholding. In the velocity
/// two-box mode that is the reflectivity plane, never the velocity plane, so
/// the hierarchy and the geometry agree by construction.
///
/// This function sees only the box and its support. It cannot depend on the
/// camera, the opacity, or the palette, which is how "camera motion, opacity
/// changes and palette changes must not rebuild the hierarchy" is enforced:
/// there is nothing here for those to change.
pub fn build_acceleration(
    normalized: &[u8],
    support_input: &[u8],
    n: usize,
    nz: usize,
) -> VolumeAcceleration {
    let voxels = n.saturating_mul(n).saturating_mul(nz);
    let fine = fine_dims(n, nz);
    let coarse = coarse_dims(n, nz);
    if voxels == 0 || fine.is_empty() {
        return VolumeAcceleration {
            support: Vec::new(),
            fine_minmax: Vec::new(),
            coarse_minmax: Vec::new(),
            fine_dims: fine,
            coarse_dims: coarse,
            empty_fine_fraction: 1.0,
        };
    }

    // Copy both planes to exactly one box so the inner loop can index without
    // bounds checks. A short input pads with 0, which reads as no data - the
    // safe direction, because a cell with no support is never skipped on the
    // strength of its values.
    let mut support = vec![0u8; voxels];
    let copied = support_input.len().min(voxels);
    support[..copied].copy_from_slice(&support_input[..copied]);
    let mut values = vec![0u8; voxels];
    let copied = normalized.len().min(voxels);
    values[..copied].copy_from_slice(&normalized[..copied]);

    let fine_cells: Vec<[u8; 4]> = (0..fine.len())
        .into_par_iter()
        .map(|brick| fine_brick_bounds(&values, &support, n, nz, fine, brick))
        .collect();

    let mut coarse_cells = vec![[0u8; 4]; coarse.len()];
    for cz in 0..coarse.z {
        let (z_first, z_last) = child_span(cz, coarse.z, fine.z);
        for cy in 0..coarse.y {
            let (y_first, y_last) = child_span(cy, coarse.y, fine.y);
            for cx in 0..coarse.x {
                let (x_first, x_last) = child_span(cx, coarse.x, fine.x);
                let mut min_v = u8::MAX;
                let mut max_v = 0u8;
                let mut min_s = u8::MAX;
                let mut max_s = 0u8;
                let mut observed = false;
                for z in z_first..=z_last {
                    for y in y_first..=y_last {
                        for x in x_first..=x_last {
                            let child = fine_cells[fine.index(x, y, z)];
                            if child[3] == 0 {
                                // An unobserved child is transparent over its
                                // whole extent, so leaving it out only tightens
                                // the parent without ever hiding a sample.
                                continue;
                            }
                            min_v = min_v.min(child[0]);
                            max_v = max_v.max(child[1]);
                            min_s = min_s.min(child[2]);
                            max_s = max_s.max(child[3]);
                            observed = true;
                        }
                    }
                }
                if !observed {
                    min_v = 0;
                    min_s = 0;
                }
                coarse_cells[coarse.index(cx, cy, cz)] = [min_v, max_v, min_s, max_s];
            }
        }
    }

    let empty = fine_cells.iter().filter(|cell| cell[3] == 0).count();
    VolumeAcceleration {
        support,
        fine_minmax: fine_cells.concat(),
        coarse_minmax: coarse_cells.concat(),
        fine_dims: fine,
        coarse_dims: coarse,
        empty_fine_fraction: empty as f32 / fine.len() as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack_at(ground_arc_m: f64, elevations: &[f64]) -> Vec<BeamStackSample> {
        elevations
            .iter()
            .map(|elevation_deg| {
                let slant_range_m =
                    crate::beam::slant_range_for_ground_arc_m(ground_arc_m, *elevation_deg, 2.0e6)
                        .expect("ground arc is reachable at this tilt");
                BeamStackSample {
                    height_m: crate::beam::beam_height_arl_m(slant_range_m, *elevation_deg),
                    elevation_deg: *elevation_deg,
                    slant_range_m,
                }
            })
            .collect()
    }

    #[test]
    fn half_beamwidth_matches_wsr88d_aperture() {
        // 0.95 degree aperture, halved, in radians. Pinned because
        // `volumetric.rs` keeps a private copy of the same number.
        assert!((HALF_POWER_HALF_BEAMWIDTH_RAD - 0.008_290_313_946_973_065).abs() < 1.0e-15);
    }

    #[test]
    fn beam_elevation_round_trips_through_forward_geometry() {
        for elevation_deg in [0.5_f64, 1.5, 4.3, 9.9, 19.5] {
            for slant_range_m in [5_000.0_f64, 60_000.0, 180_000.0] {
                let height_m = crate::beam::beam_height_arl_m(slant_range_m, elevation_deg);
                let ground_arc_m = crate::beam::ground_arc_m(slant_range_m, elevation_deg);
                let (recovered_range, recovered_elevation) = beam_inverse(ground_arc_m, height_m);
                assert!(
                    (recovered_elevation - elevation_deg).abs() < 1.0e-6,
                    "{elevation_deg} deg at {slant_range_m} m recovered {recovered_elevation}"
                );
                assert!((recovered_range - slant_range_m).abs() < 1.0e-3);
            }
        }
    }

    #[test]
    fn support_below_lowest_tilt_fades_over_half_a_beamwidth() {
        // Hand-computed: half a beamwidth at 100 km is 100000 * 0.008290314 =
        // 829.0314 m, which exceeds the 300 m display floor. A voxel 500 m
        // below the lowest beam therefore keeps 1 - 500/829.0314 = 0.3968865
        // of the 0.56 extrapolation band above the 0.18 floor:
        // 0.18 + 0.56 * 0.3968865 = 0.4022564 -> round(102.575) = 103.
        let stack = [BeamStackSample {
            height_m: 500.0,
            elevation_deg: 0.5,
            slant_range_m: 100_000.0,
        }];
        assert_eq!(beam_support_score(&stack, 0.0, 100_000.0), 103);
    }

    #[test]
    fn support_above_top_tilt_bottoms_out_at_the_extrapolation_floor() {
        // Half a beamwidth at 200 km is 1658.0628 m; 2000 m above the top beam
        // is past it, so the fading term clamps to 0 and only the 0.12 floor
        // remains: round(0.12 * 255) = round(30.6) = 31.
        let stack = [BeamStackSample {
            height_m: 10_000.0,
            elevation_deg: 4.0,
            slant_range_m: 200_000.0,
        }];
        assert_eq!(beam_support_score(&stack, 12_000.0, 200_000.0), 31);
        // Upward extrapolation must never outscore downward extrapolation:
        // storm-top overshoot is the reconstruction's weakest claim.
        assert!(beam_support_score(&stack, 12_000.0, 200_000.0) < 103);
    }

    #[test]
    fn support_on_a_beam_centre_scores_the_hand_computed_blend() {
        // Three tilts over one column at 100 km. Evaluate exactly at the 1.0
        // degree beam height, which is bracketed by the (0.5, 1.0) pair:
        //   directness = exp(0)                              = 1.0
        //   gap_score  = 1 / (1 + (0.5/1.5)^2) = 1 / (10/9)  = 0.9
        //   endpoint   = 1 - 2*min(1, 0)                     = 1.0
        //   score = 0.30 + 0.38*1.0 + 0.20*0.9 + 0.12*1.0    = 0.98
        // round(0.98 * 255) = round(249.9) = 250.
        let stack = stack_at(100_000.0, &[0.5, 1.0, 2.0]);
        assert_eq!(
            beam_support_score(&stack, stack[1].height_m, 100_000.0),
            250
        );
    }

    #[test]
    fn support_falls_off_away_from_the_beam_and_across_wide_tilt_gaps() {
        let tight = stack_at(100_000.0, &[0.5, 1.0]);
        let wide = stack_at(100_000.0, &[0.5, 6.0]);
        let midpoint = |stack: &[BeamStackSample]| 0.5 * (stack[0].height_m + stack[1].height_m);

        let on_beam = beam_support_score(&tight, tight[0].height_m + 1.0, 100_000.0);
        let between = beam_support_score(&tight, midpoint(&tight), 100_000.0);
        assert!(
            on_beam > between,
            "on-beam {on_beam} should beat mid-gap {between}"
        );

        let across_wide_gap = beam_support_score(&wide, midpoint(&wide), 100_000.0);
        assert!(
            between > across_wide_gap,
            "a 0.5 deg gap {between} should beat a 5.5 deg gap {across_wide_gap}"
        );
    }

    #[test]
    fn support_never_returns_the_no_data_sentinel() {
        let stack = stack_at(230_000.0, &[0.5, 19.5]);
        for z_m in [0.0_f64, 1.0, 5_000.0, 40_000.0, 120_000.0] {
            assert!(beam_support_score(&stack, z_m, 230_000.0) >= 1);
        }
        assert!(beam_support_score(&[], 1_000.0, 10_000.0) >= 1);
    }

    fn box_index(x: usize, y: usize, z: usize, n: usize) -> usize {
        z * n * n + y * n + x
    }

    #[test]
    fn hierarchy_dimensions_match_the_shader_constants() {
        // The WGSL traverser hard-codes 24x24x6 and 6x6x2 for the 192x192x48
        // box. If either side moves, the other must move with it.
        assert_eq!(fine_dims(192, 48), HierarchyDims { x: 24, y: 24, z: 6 });
        assert_eq!(coarse_dims(192, 48), HierarchyDims { x: 6, y: 6, z: 2 });
    }

    #[test]
    fn fine_bounds_cover_every_observed_voxel_in_the_brick() {
        let (n, nz) = (32usize, 16usize);
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        // One isolated echo well inside brick (2, 1, 1) so the apron cannot
        // reach a neighbouring brick's data and mask a mistake.
        let index = box_index(20, 12, 12, n);
        values[index] = 231;
        support[index] = 207;

        let accel = build_acceleration(&values, &support, n, nz);
        let dims = accel.fine_dims;
        let cell = dims.index(20 / FINE_BRICK, 12 / FINE_BRICK, 12 / FINE_BRICK) * 4;
        assert!(
            accel.fine_minmax[cell] <= 231,
            "minimum must not exceed the sample"
        );
        assert!(
            accel.fine_minmax[cell + 1] >= 231,
            "maximum must cover the sample"
        );
        assert!(accel.fine_minmax[cell + 2] <= 207);
        assert!(accel.fine_minmax[cell + 3] >= 207);
        // The no-data voxels around it drag the minimum to 0, which is what the
        // trilinear sampler will actually produce at the echo's edge.
        assert_eq!(accel.fine_minmax[cell], 0);
    }

    #[test]
    fn fine_bounds_include_the_one_voxel_apron() {
        let (n, nz) = (32usize, 16usize);
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        // x = 8 is the first voxel of brick 1 and the apron of brick 0. Both
        // bricks must report it, because a ray sample just inside brick 0's
        // far face blends this voxel in.
        let index = box_index(8, 4, 4, n);
        values[index] = 200;
        support[index] = 180;

        let accel = build_acceleration(&values, &support, n, nz);
        let dims = accel.fine_dims;
        let owner = dims.index(1, 0, 0) * 4;
        let neighbour = dims.index(0, 0, 0) * 4;
        assert_eq!(accel.fine_minmax[owner + 1], 200);
        assert_eq!(
            accel.fine_minmax[neighbour + 1],
            200,
            "the neighbouring brick must not report itself empty"
        );
        assert!(accel.fine_minmax[neighbour + 3] >= 180);
    }

    #[test]
    fn unobserved_bricks_report_zero_support_and_a_zero_range() {
        let (n, nz) = (32usize, 16usize);
        let values = vec![170u8; n * n * nz];
        let support = vec![0u8; n * n * nz];
        let accel = build_acceleration(&values, &support, n, nz);
        assert_eq!(accel.empty_fine_fraction, 1.0);
        for cell in accel.fine_minmax.chunks_exact(4) {
            assert_eq!(cell, [0, 0, 0, 0], "no support anywhere means no range");
        }
        for cell in accel.coarse_minmax.chunks_exact(4) {
            assert_eq!(cell, [0, 0, 0, 0]);
        }
    }

    /// Every fine brick whose trilinear footprint can reach voxel `(x, y, z)`.
    fn neighbourhood(
        x: usize,
        y: usize,
        z: usize,
        dims: HierarchyDims,
    ) -> Vec<(usize, usize, usize)> {
        let axis = |value: usize, count: usize| {
            let own = value / FINE_BRICK;
            let mut cells = vec![own];
            if value.is_multiple_of(FINE_BRICK) && own > 0 {
                cells.push(own - 1);
            }
            if value % FINE_BRICK == FINE_BRICK - 1 && own + 1 < count {
                cells.push(own + 1);
            }
            cells
        };
        let mut out = Vec::new();
        for bz in axis(z, dims.z) {
            for by in axis(y, dims.y) {
                for bx in axis(x, dims.x) {
                    out.push((bx, by, bz));
                }
            }
        }
        out
    }

    #[test]
    fn hierarchy_is_conservative_over_a_pseudorandom_box() {
        // Deterministic xorshift rather than a dependency: the point is the
        // bound, not the distribution.
        let (n, nz) = (64usize, 32usize);
        let mut state = 0x0B0E_C0DEu32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        for index in 0..values.len() {
            values[index] = (next() & 0xFF) as u8;
            support[index] = if next() % 100 < 28 {
                ((next() % 255) + 1) as u8
            } else {
                0
            };
        }

        let accel = build_acceleration(&values, &support, n, nz);
        let fine = accel.fine_dims;
        let coarse = accel.coarse_dims;
        for z in 0..nz {
            for y in 0..n {
                for x in 0..n {
                    let index = box_index(x, y, z, n);
                    if support[index] == 0 {
                        continue;
                    }
                    // Every brick that can blend this voxel into a sample -
                    // its own and, when it sits on a face, its neighbour -
                    // must bound it.
                    for (bx, by, bz) in neighbourhood(x, y, z, fine) {
                        let cell = fine.index(bx, by, bz) * 4;
                        assert!(
                            accel.fine_minmax[cell] <= values[index]
                                && accel.fine_minmax[cell + 1] >= values[index],
                            "fine value bound missed voxel ({x},{y},{z})"
                        );
                        assert!(
                            accel.fine_minmax[cell + 2] <= support[index]
                                && accel.fine_minmax[cell + 3] >= support[index],
                            "fine support bound missed voxel ({x},{y},{z})"
                        );
                        let parent = coarse.index(
                            bx / COARSE_GROUP_X,
                            by / COARSE_GROUP_Y,
                            bz / COARSE_GROUP_Z,
                        ) * 4;
                        assert!(
                            accel.coarse_minmax[parent] <= values[index]
                                && accel.coarse_minmax[parent + 1] >= values[index],
                            "coarse value bound missed voxel ({x},{y},{z})"
                        );
                        assert!(
                            accel.coarse_minmax[parent + 3] >= support[index],
                            "coarse support bound missed voxel ({x},{y},{z})"
                        );
                    }
                }
            }
        }
    }

    /// The upstream bound: exactly the brick's own 8x8x8 voxels, observed
    /// voxels only. Kept in the tests to demonstrate what the apron buys.
    fn bounds_without_apron(
        values: &[u8],
        support: &[u8],
        n: usize,
        nz: usize,
        dims: HierarchyDims,
        brick: usize,
    ) -> [u8; 4] {
        let fx = brick % dims.x;
        let fy = (brick / dims.x) % dims.y;
        let fz = brick / (dims.x * dims.y);
        let mut bounds = [u8::MAX, 0, u8::MAX, 0];
        let mut observed = false;
        for lz in 0..FINE_BRICK {
            let z = fz * FINE_BRICK + lz;
            if z >= nz {
                break;
            }
            for ly in 0..FINE_BRICK {
                let y = fy * FINE_BRICK + ly;
                if y >= n {
                    break;
                }
                for lx in 0..FINE_BRICK {
                    let x = fx * FINE_BRICK + lx;
                    if x >= n {
                        break;
                    }
                    let index = z * n * n + y * n + x;
                    if support[index] == 0 {
                        continue;
                    }
                    bounds[0] = bounds[0].min(values[index]);
                    bounds[1] = bounds[1].max(values[index]);
                    bounds[2] = bounds[2].min(support[index]);
                    bounds[3] = bounds[3].max(support[index]);
                    observed = true;
                }
            }
        }
        if !observed {
            return [0, 0, 0, 0];
        }
        bounds
    }

    #[test]
    fn dropping_the_apron_would_let_a_brick_disown_an_echo_it_can_sample() {
        // A single observed voxel on the last row of brick 0. Brick 1 has no
        // observed voxel of its own, but a ray sample just inside its near
        // face still blends this one in at up to half weight. Without the
        // apron brick 1 reports itself empty and the traverser skips a sample
        // that would have painted - half a voxel of every echo rim, in every
        // render mode. This test exists so nobody optimises the apron away.
        let (n, nz) = (32usize, 16usize);
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        let index = box_index(FINE_BRICK - 1, 4, 4, n);
        values[index] = 240;
        support[index] = 190;

        let accel = build_acceleration(&values, &support, n, nz);
        let dims = accel.fine_dims;
        let neighbour = dims.index(1, 0, 0);
        let naive = bounds_without_apron(&values, &support, n, nz, dims, neighbour);
        assert_eq!(naive, [0, 0, 0, 0], "the naive bound calls brick 1 empty");
        assert!(
            accel.fine_minmax[neighbour * 4 + 3] >= 190,
            "the apron bound must keep brick 1 alive"
        );
        assert_eq!(accel.fine_minmax[neighbour * 4 + 1], 240);
    }

    /// The shader's `range_can_contribute`, mirrored on the CPU so the skip
    /// decision can be checked against a real volume without a GPU. Mode is
    /// 0 = Above, 1 = Below, 2 = Outside; bounds are in the shader's 0..1
    /// domain, which is the u8 texel divided by 255.
    fn range_can_contribute(cell: [u8; 4], mode: u8, low: f32, high: f32) -> bool {
        if cell[3] == 0 {
            return false;
        }
        let min_v = f32::from(cell[0]) / 255.0;
        let max_v = f32::from(cell[1]) / 255.0;
        match mode {
            1 => min_v < low,
            2 => min_v < low || max_v > high,
            _ => max_v > low,
        }
    }

    /// Full end-to-end check against a decoded Level II volume: real tilt
    /// geometry, real cone of silence, real no-data pattern.
    ///
    /// Ignored by default so the gate stays hermetic. Point `RADAR_L2_SAMPLE`
    /// at any Archive II file, for example one from the workstation's
    /// `cache/level2-live` directory, and run with `--ignored --nocapture`.
    #[ignore = "set RADAR_L2_SAMPLE to a Level II file path to run manually"]
    #[test]
    fn real_volume_support_and_hierarchy_cannot_hide_an_echo() {
        use radar_core::MomentType;

        const N: usize = 192;
        const NZ: usize = 48;
        const HALF_KM: f32 = 60.0;
        const TOP_M: f32 = 18_000.0;
        const VALUE_MIN: f32 = 0.0;
        const VALUE_MAX: f32 = 80.0;

        let path = std::env::var("RADAR_L2_SAMPLE").expect("RADAR_L2_SAMPLE is not set");
        let volume = nexrad_io::decode_volume_from_path(std::path::Path::new(&path))
            .expect("the sample decodes");
        println!(
            "site {} volume {} cuts {}",
            volume.site.id,
            volume.volume_time,
            volume.cuts.len()
        );

        let values = crate::volumetric::volume_box_resample_moment(
            &volume,
            &MomentType::Reflectivity,
            crate::volumetric::InterpPolicy::LinearAngle,
            0.0,
            0.0,
            HALF_KM,
            N,
            NZ,
            TOP_M,
        )
        .expect("the volume resamples into a box");

        // Reconstruct the beam stack the same way the resampler sees it: one
        // entry per reflectivity cut that physically reaches this column.
        let cuts: Vec<(f64, f64)> = volume
            .cuts
            .iter()
            .filter_map(|cut| {
                let grid = cut.moments.get(&MomentType::Reflectivity)?;
                let range = &grid.gate_range;
                let max_slant_m = f64::from(range.first_gate_m)
                    + f64::from(range.gate_spacing_m) * range.gate_count as f64;
                Some((f64::from(cut.elevation_deg), max_slant_m))
            })
            .collect();
        assert!(!cuts.is_empty(), "no reflectivity cuts in this volume");

        let mut support = vec![0u8; N * N * NZ];
        for yi in 0..N {
            let north = -HALF_KM + 2.0 * HALF_KM * yi as f32 / (N - 1) as f32;
            for xi in 0..N {
                let east = -HALF_KM + 2.0 * HALF_KM * xi as f32 / (N - 1) as f32;
                let ground_arc_m = f64::from(east.hypot(north)) * 1000.0;
                let mut stack: Vec<BeamStackSample> = cuts
                    .iter()
                    .filter_map(|(elevation_deg, max_slant_m)| {
                        let slant_range_m = crate::beam::slant_range_for_ground_arc_m(
                            ground_arc_m,
                            *elevation_deg,
                            *max_slant_m,
                        )?;
                        Some(BeamStackSample {
                            height_m: crate::beam::beam_height_arl_m(slant_range_m, *elevation_deg),
                            elevation_deg: *elevation_deg,
                            slant_range_m,
                        })
                    })
                    .collect();
                stack.sort_by(|a, b| a.height_m.total_cmp(&b.height_m));
                if stack.is_empty() {
                    continue;
                }
                for zi in 0..NZ {
                    let index = zi * N * N + yi * N + xi;
                    if !values[index].is_finite() {
                        continue;
                    }
                    let z_m = f64::from(TOP_M) * zi as f64 / (NZ - 1) as f64;
                    support[index] = beam_support_score(&stack, z_m, ground_arc_m);
                }
            }
        }

        let span = VALUE_MAX - VALUE_MIN;
        let normalized: Vec<u8> = values
            .iter()
            .map(|value| {
                if value.is_finite() {
                    (((value - VALUE_MIN) / span).clamp(0.0, 1.0) * 255.0).round() as u8
                } else {
                    0
                }
            })
            .collect();

        let observed = support.iter().filter(|score| **score > 0).count();
        assert!(
            observed > 0,
            "the box is entirely empty; pick a closer scan"
        );
        println!(
            "observed voxels {observed} / {} ({:.1}%)",
            support.len(),
            100.0 * observed as f64 / support.len() as f64
        );
        // The support field is the no-data mask, and nothing else may be.
        for (index, score) in support.iter().enumerate() {
            assert_eq!(
                *score > 0,
                values[index].is_finite(),
                "support disagrees with no-data at voxel {index}"
            );
        }

        let accel = build_acceleration(&normalized, &support, N, NZ);
        println!(
            "empty fine bricks {:.1}%",
            100.0 * f64::from(accel.empty_fine_fraction)
        );
        assert!(
            accel.empty_fine_fraction > 0.0 && accel.empty_fine_fraction < 1.0,
            "a real volume should leave the hierarchy some empty space and some data"
        );

        // How much the one-voxel apron is actually worth on this scan: bricks
        // the upstream bound would call empty even though the sampler can
        // reach observed data inside them.
        let fine = accel.fine_dims;
        let disowned = (0..fine.len())
            .filter(|brick| {
                accel.fine_minmax[brick * 4 + 3] > 0
                    && bounds_without_apron(&normalized, &support, N, NZ, fine, *brick)[3] == 0
            })
            .count();
        println!(
            "bricks the no-apron bound would wrongly skip: {disowned} / {}",
            fine.len()
        );

        // Median support at 2 km and at 14 km. Upper levels are mostly wide
        // tilt gaps and top extrapolation, so they must score lower.
        let median_at = |zi: usize| {
            let mut scores: Vec<u8> = (0..N * N)
                .map(|cell| support[zi * N * N + cell])
                .filter(|score| *score > 0)
                .collect();
            scores.sort_unstable();
            scores.get(scores.len() / 2).copied()
        };
        let low = median_at((2_000.0 / f64::from(TOP_M) * (NZ - 1) as f64).round() as usize);
        let high = median_at((14_000.0 / f64::from(TOP_M) * (NZ - 1) as f64).round() as usize);
        println!("median support: 2 km {low:?}, 14 km {high:?}");
        if let (Some(low), Some(high)) = (low, high) {
            assert!(
                low > high,
                "2 km support {low} should beat 14 km support {high}"
            );
        }

        // The contract that decides whether the optimisation can hide a storm:
        // for every threshold mode, every observed voxel that WOULD paint must
        // live in cells the traverser refuses to skip.
        let coarse = accel.coarse_dims;
        let modes: [(&str, u8, f32, f32); 3] = [
            ("Above 35 dBZ", 0, 35.0 / 80.0, -1.0),
            ("Below 20 dBZ", 1, 20.0 / 80.0, -1.0),
            ("Outside 10..50 dBZ", 2, 10.0 / 80.0, 50.0 / 80.0),
        ];
        let mut checked_total = 0usize;
        for (label, mode, low, high) in modes {
            let mut checked = 0usize;
            for z in 0..NZ {
                for y in 0..N {
                    for x in 0..N {
                        let index = z * N * N + y * N + x;
                        if support[index] == 0 {
                            continue;
                        }
                        let value = f32::from(normalized[index]) / 255.0;
                        let contributes = match mode {
                            1 => value < low,
                            2 => value < low || value > high,
                            _ => value > low,
                        };
                        if !contributes {
                            continue;
                        }
                        checked += 1;
                        for (bx, by, bz) in neighbourhood(x, y, z, fine) {
                            let cell = fine.index(bx, by, bz) * 4;
                            let bounds: [u8; 4] =
                                accel.fine_minmax[cell..cell + 4].try_into().unwrap();
                            assert!(
                                range_can_contribute(bounds, mode, low, high),
                                "{label}: fine brick ({bx},{by},{bz}) would skip voxel ({x},{y},{z})"
                            );
                            let parent = coarse.index(
                                bx / COARSE_GROUP_X,
                                by / COARSE_GROUP_Y,
                                bz / COARSE_GROUP_Z,
                            ) * 4;
                            let bounds: [u8; 4] =
                                accel.coarse_minmax[parent..parent + 4].try_into().unwrap();
                            assert!(
                                range_can_contribute(bounds, mode, low, high),
                                "{label}: coarse cell would skip voxel ({x},{y},{z})"
                            );
                        }
                    }
                }
            }
            println!("{label}: {checked} contributing voxels, none skippable");
            checked_total += checked;
        }
        // A quiet scan can legitimately hold nothing above 35 dBZ inside a
        // 60 km box, and a verification test that refuses to run on one is a
        // test people stop running. Only the total has to be non-empty.
        assert!(
            checked_total > 0,
            "no threshold mode matched anything; pick a livelier scan"
        );

        // The same contract, but against the field the GPU actually fetches:
        // the trilinearly filtered value, sampled everywhere inside every
        // brick rather than only at the source voxels. This is the strict
        // version - a bound can hold at the voxels and still fail between
        // them - and it covers the isosurface and velocity gates as well.
        let tests = adversarial_skip_tests();
        let counts = assert_no_cell_can_hide_a_sample(&normalized, &support, N, NZ, &accel, &tests);
        for ((label, test), count) in tests.iter().zip(counts.iter()) {
            let skippable = (0..fine.len())
                .filter(|brick| {
                    let at = brick * 4;
                    let bounds: [u8; 4] = accel.fine_minmax[at..at + 4].try_into().unwrap();
                    !test.cell_can_contribute(bounds)
                })
                .count();
            println!(
                "{label}: {} painting samples, {} no-data samples the gate stops, \
                 {skippable}/{} fine bricks skippable",
                count.painting,
                count.no_data_would_have_painted,
                fine.len()
            );
        }
        assert!(
            counts.iter().any(|count| count.painting > 0),
            "no mode painted anything; pick a livelier scan"
        );
        // Contract 5 has real work to do on a real volume: the unobserved
        // half of the box is stored as 0, which is trivially "below" a
        // threshold and trivially "outside" a band.
        assert!(
            counts[2].no_data_would_have_painted > 0 && counts[4].no_data_would_have_painted > 0,
            "Below and Outside must be the modes the no-data gate saves"
        );

        // What the default `HonestFade` presentation does to the weakest
        // reconstruction. Reported, not asserted: it is a display choice, but
        // it is the one that decides whether an interpolated storm top is
        // visible at all.
        let floor_value = 0.18_f64;
        let faded = support
            .iter()
            .filter(|score| **score > 0)
            .filter(|score| {
                let value = f64::from(**score) / 255.0;
                let t = ((value - floor_value) / (1.0 - floor_value)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t) < 0.02
            })
            .count();
        println!(
            "observed voxels faded below 2% opacity at the default support floor: {faded} \
             ({:.1}% of observed)",
            100.0 * faded as f64 / observed as f64
        );
    }

    #[test]
    fn support_is_resized_to_the_box_and_never_borrows_stale_texels() {
        let (n, nz) = (16usize, 8usize);
        let accel = build_acceleration(&[], &[9u8; 4], n, nz);
        assert_eq!(accel.support.len(), n * n * nz);
        assert_eq!(&accel.support[..4], &[9, 9, 9, 9]);
        assert!(accel.support[4..].iter().all(|value| *value == 0));
    }

    // ---------------------------------------------------------------------
    // Sample-level conservativeness.
    //
    // The voxel-level tests above prove the stored interval bounds the SOURCE
    // voxels. That is not the quantity the traverser bets on: the shader skips
    // a cell over its whole spatial extent, and inside that extent it fetches
    // the TRILINEARLY FILTERED field. The tests below mirror the GPU fetch
    // exactly and check the bound, and the skip decision, against that.
    // ---------------------------------------------------------------------

    /// One axis of the GPU's trilinear fetch: texel centres at `(i + 0.5)/dim`
    /// with `ClampToEdge` addressing, which is how `vol3d::init_gpu` builds
    /// `s_volume` (`AddressMode::ClampToEdge`, `FilterMode::Linear`).
    fn axis_weights(coord01: f64, dim: usize) -> (usize, usize, f64) {
        let scaled = coord01 * dim as f64 - 0.5;
        let base = scaled.floor();
        let fraction = scaled - base;
        let last = dim as isize - 1;
        let i0 = (base as isize).clamp(0, last) as usize;
        let i1 = (base as isize + 1).clamp(0, last) as usize;
        (i0, i1, fraction)
    }

    /// `textureSampleLevel(field, s_volume, uvw, 0.0).r`, in the 0..=255 texel
    /// domain rather than the shader's 0..1 one.
    fn sample_trilinear(field: &[u8], n: usize, nz: usize, uvw: [f64; 3]) -> f64 {
        let (x0, x1, fx) = axis_weights(uvw[0], n);
        let (y0, y1, fy) = axis_weights(uvw[1], n);
        let (z0, z1, fz) = axis_weights(uvw[2], nz);
        let at = |x: usize, y: usize, z: usize| f64::from(field[z * n * n + y * n + x]);
        let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
        let near = lerp(
            lerp(at(x0, y0, z0), at(x1, y0, z0), fx),
            lerp(at(x0, y1, z0), at(x1, y1, z0), fx),
            fy,
        );
        let far = lerp(
            lerp(at(x0, y0, z1), at(x1, y0, z1), fx),
            lerp(at(x0, y1, z1), at(x1, y1, z1), fx),
            fy,
        );
        lerp(near, far, fz)
    }

    /// Everything `advanced_shader_helpers.wgsl::range_can_contribute` reads,
    /// in the shader's 0..1 domain.
    #[derive(Clone, Copy)]
    struct SkipTest {
        render_mode: f32,
        threshold_mode: f32,
        threshold: f32,
        threshold_high: f32,
        velocity_mode: f32,
        ref_gate: f32,
        iso_value: f32,
        iso_width: f32,
    }

    impl SkipTest {
        fn direct(threshold_mode: f32, threshold: f32, threshold_high: f32) -> Self {
            Self {
                render_mode: 0.0,
                threshold_mode,
                threshold,
                threshold_high,
                velocity_mode: 0.0,
                ref_gate: 0.0,
                iso_value: 0.0,
                iso_width: 0.01,
            }
        }

        fn velocity(ref_gate: f32) -> Self {
            Self {
                velocity_mode: 1.0,
                ref_gate,
                ..Self::direct(0.0, 0.0, -1.0)
            }
        }

        fn isosurface(iso_value: f32, iso_width: f32) -> Self {
            Self {
                render_mode: 2.0,
                iso_value,
                iso_width,
                ..Self::direct(0.0, 0.0, -1.0)
            }
        }

        /// `range_can_contribute`, branch for branch.
        fn cell_can_contribute(self, cell: [u8; 4]) -> bool {
            let range = cell.map(|channel| f32::from(channel) / 255.0);
            if range[3] <= 0.0001 {
                return false;
            }
            let shell = range[1] >= self.iso_value - self.iso_width.max(0.002);
            if self.render_mode > 1.5 && self.render_mode < 2.5 {
                return shell;
            }
            if self.render_mode > 0.5 && self.render_mode < 1.5 && shell {
                return true;
            }
            if self.velocity_mode > 0.5 {
                return range[1] > self.ref_gate;
            }
            if self.threshold_mode > 1.5 {
                return range[0] < self.threshold || range[1] > self.threshold_high;
            }
            if self.threshold_mode > 0.5 {
                return range[0] < self.threshold;
            }
            range[1] > self.threshold
        }

        /// Does `advanced_fs_main.wgsl` put colour on the screen for this
        /// sample? Isosurface asks a different question and is checked against
        /// its own necessary condition at the call site.
        fn sample_paints(self, structure: f32) -> bool {
            if self.velocity_mode > 0.5 {
                return structure > self.ref_gate;
            }
            if self.threshold_mode > 1.5 {
                return structure < self.threshold || structure > self.threshold_high;
            }
            if self.threshold_mode > 0.5 {
                return structure < self.threshold;
            }
            structure > self.threshold
        }
    }

    /// `uvw` probes inside one hierarchy cell along one axis: every texel
    /// centre the cell contains, plus both faces and the midpoint.
    ///
    /// A trilinear reconstruction is multilinear on each sub-cell, so its
    /// extrema over the cell are attained at texel centres or on the cell's
    /// own faces. Probing exactly that set is therefore not a sample of the
    /// cell, it is the cell's extremes — and the faces are where an off-by-one
    /// in the aggregation shows up. Derived from the lattice so the harness
    /// also works when the voxel count is not a multiple of the cell count.
    fn axis_probes(cell: usize, cells: usize, len: usize) -> Vec<f64> {
        let low = cell as f64 / cells as f64;
        let high = (cell + 1) as f64 / cells as f64;
        let mut out = vec![low, 0.5 * (low + high), high - 1.0e-12];
        out.extend(
            (0..len)
                .map(|texel| (texel as f64 + 0.5) / len as f64)
                .filter(|centre| *centre >= low && *centre < high),
        );
        out
    }

    /// Per-mode tally from [`assert_no_cell_can_hide_a_sample`].
    #[derive(Clone, Copy, Default)]
    struct PaintCounts {
        painting: usize,
        no_data_would_have_painted: usize,
    }

    /// The contract that decides whether the optimisation can hide a storm,
    /// checked against the filtered field the GPU actually fetches.
    ///
    /// For every fine brick, walk a lattice of sample points inside that
    /// brick's spatial extent and assert:
    ///
    /// * the brick's stored interval bounds the trilinear value and support;
    /// * the parent coarse cell's interval bounds them too;
    /// * a sample that would paint never sits inside a cell the traverser is
    ///   allowed to skip, at either level, under any threshold mode.
    fn assert_no_cell_can_hide_a_sample(
        values: &[u8],
        support: &[u8],
        n: usize,
        nz: usize,
        accel: &VolumeAcceleration,
        tests: &[(&str, SkipTest)],
    ) -> Vec<PaintCounts> {
        let fine = accel.fine_dims;
        let coarse = accel.coarse_dims;
        let per_brick: Vec<Vec<PaintCounts>> = (0..fine.len())
            .into_par_iter()
            .map(|brick| {
                let bx = brick % fine.x;
                let by = (brick / fine.x) % fine.y;
                let bz = brick / (fine.x * fine.y);
                let at = fine.index(bx, by, bz) * 4;
                let cell: [u8; 4] = accel.fine_minmax[at..at + 4]
                    .try_into()
                    .expect("four channels per fine cell");
                let parent_at = coarse.index(
                    bx / COARSE_GROUP_X,
                    by / COARSE_GROUP_Y,
                    bz / COARSE_GROUP_Z,
                ) * 4;
                let parent: [u8; 4] = accel.coarse_minmax[parent_at..parent_at + 4]
                    .try_into()
                    .expect("four channels per coarse cell");
                let mut counts = vec![PaintCounts::default(); tests.len()];
                let z_probes = axis_probes(bz, fine.z, nz);
                let y_probes = axis_probes(by, fine.y, n);
                let x_probes = axis_probes(bx, fine.x, n);
                for w in &z_probes {
                    for v in &y_probes {
                        for u in &x_probes {
                            let uvw = [*u, *v, *w];
                            // The shader looks the cell up from this same uvw,
                            // so a disagreement here is an indexing bug.
                            assert_eq!(
                                shader_cell_coord(uvw, fine),
                                [bx, by, bz],
                                "hierarchy_coord disagrees with the brick under test"
                            );
                            let sampled_support = sample_trilinear(support, n, nz, uvw);
                            let sampled_value = sample_trilinear(values, n, nz, uvw);
                            if cell[3] == 0 {
                                assert_eq!(
                                    sampled_support, 0.0,
                                    "brick ({bx},{by},{bz}) is declared unobserved but the \
                                     sampler still reaches support inside it"
                                );
                                continue;
                            }
                            assert!(
                                sampled_value >= f64::from(cell[0]) - 1.0e-9
                                    && sampled_value <= f64::from(cell[1]) + 1.0e-9,
                                "fine brick ({bx},{by},{bz}) interval [{},{}] does not bound the \
                                 filtered value {sampled_value}",
                                cell[0],
                                cell[1]
                            );
                            assert!(
                                sampled_support <= f64::from(cell[3]) + 1.0e-9,
                                "fine brick ({bx},{by},{bz}) maximum support {} does not bound \
                                 the filtered support {sampled_support}",
                                cell[3]
                            );
                            assert!(
                                sampled_value >= f64::from(parent[0]) - 1.0e-9
                                    && sampled_value <= f64::from(parent[1]) + 1.0e-9,
                                "the coarse parent of brick ({bx},{by},{bz}) has interval \
                                 [{},{}], which does not bound the filtered value {sampled_value}",
                                parent[0],
                                parent[1]
                            );
                            let structure = (sampled_value / 255.0) as f32;
                            for (index, (label, test)) in tests.iter().enumerate() {
                                let paints = if test.render_mode > 1.5 && test.render_mode < 2.5 {
                                    structure >= test.iso_value
                                } else {
                                    test.sample_paints(structure)
                                };
                                if !paints {
                                    continue;
                                }
                                // No data is transparent in every mode, so a
                                // zero-support sample is not a paint at all.
                                // Counting them shows what the gate is worth.
                                if sampled_support <= 0.0001 * 255.0 {
                                    counts[index].no_data_would_have_painted += 1;
                                    continue;
                                }
                                counts[index].painting += 1;
                                assert!(
                                    test.cell_can_contribute(cell),
                                    "{label}: fine brick ({bx},{by},{bz}) would be skipped, but a \
                                     sample inside it paints (value {sampled_value}, support \
                                     {sampled_support})"
                                );
                                assert!(
                                    test.cell_can_contribute(parent),
                                    "{label}: the coarse parent of brick ({bx},{by},{bz}) would \
                                     be skipped, but a sample inside it paints (value \
                                     {sampled_value}, support {sampled_support})"
                                );
                            }
                        }
                    }
                }
                counts
            })
            .collect();
        let mut merged = vec![PaintCounts::default(); tests.len()];
        for brick in &per_brick {
            for (slot, counts) in merged.iter_mut().zip(brick.iter()) {
                slot.painting += counts.painting;
                slot.no_data_would_have_painted += counts.no_data_would_have_painted;
            }
        }
        merged
    }

    /// `advanced_shader_helpers.wgsl::hierarchy_coord`.
    fn shader_cell_coord(uvw: [f64; 3], dims: HierarchyDims) -> [usize; 3] {
        let axis = |value: f64, count: usize| {
            ((value.clamp(0.0, 0.999_999) * count as f64) as usize).min(count - 1)
        };
        [
            axis(uvw[0], dims.x),
            axis(uvw[1], dims.y),
            axis(uvw[2], dims.z),
        ]
    }

    fn adversarial_skip_tests() -> Vec<(&'static str, SkipTest)> {
        vec![
            ("Above 0.20", SkipTest::direct(0.0, 0.20, -1.0)),
            ("Above 0.55", SkipTest::direct(0.0, 0.55, -1.0)),
            ("Below 0.25", SkipTest::direct(1.0, 0.25, -1.0)),
            ("Below 0.90", SkipTest::direct(1.0, 0.90, -1.0)),
            ("Outside 0.12..0.62", SkipTest::direct(2.0, 0.12, 0.62)),
            ("Velocity gate 0.20", SkipTest::velocity(0.20)),
            ("Isosurface 0.56", SkipTest::isosurface(0.56, 0.025)),
        ]
    }

    #[test]
    fn hierarchy_bounds_every_trilinear_sample_the_gpu_can_take() {
        // Deliberately adversarial: echo placed ON brick faces, edges and
        // corners, which is exactly where a bound taken over a brick's own
        // voxels is not a bound on the reconstructed field.
        let (n, nz) = (32usize, 16usize);
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        let mut state = 0x51ED_5EEDu32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for z in 0..nz {
            for y in 0..n {
                for x in 0..n {
                    let index = box_index(x, y, z, n);
                    let on_face = z == 7 || z == 8 || x == 15 || x == 16 || y.is_multiple_of(8);
                    if on_face {
                        values[index] = ((next() % 200) + 55) as u8;
                        support[index] = ((next() % 250) + 5) as u8;
                    } else if next() % 100 < 12 {
                        values[index] = (next() % 256) as u8;
                        support[index] = ((next() % 250) + 5) as u8;
                    }
                }
            }
        }
        let accel = build_acceleration(&values, &support, n, nz);
        let tests = adversarial_skip_tests();
        let counts = assert_no_cell_can_hide_a_sample(&values, &support, n, nz, &accel, &tests);
        for (index, (label, _)) in tests.iter().enumerate() {
            assert!(
                counts[index].painting > 0,
                "{label} matched nothing; the fixture is not exercising the skip test"
            );
        }
        // Below and Outside are the modes a naive implementation gets wrong:
        // the stored 0 of an unobserved voxel is trivially "below" and
        // "outside", so without the support gate they paint the empty box.
        assert!(counts[2].no_data_would_have_painted > 0);
        assert!(counts[4].no_data_would_have_painted > 0);
    }

    #[test]
    fn a_brick_bounded_only_by_its_own_voxels_would_hide_a_filtered_sample() {
        // The concrete failure the apron prevents, measured on the filtered
        // field rather than argued. One observed voxel on the last row of
        // brick 0; brick 1 owns no observed voxel at all, yet a sample a
        // quarter of a voxel inside brick 1 reads three quarters of it.
        let (n, nz) = (32usize, 16usize);
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        let index = box_index(FINE_BRICK - 1, 4, 4, n);
        values[index] = 240;
        support[index] = 190;

        let accel = build_acceleration(&values, &support, n, nz);
        let dims = accel.fine_dims;
        let brick = dims.index(1, 0, 0);
        // Brick 1 starts at uvw.x = 8/32, which is texel coordinate 7.5:
        // the very first sample inside it already blends half of voxel 7.
        let uvw = [
            FINE_BRICK as f64 / n as f64,
            (4.0 + 0.5) / n as f64,
            (4.0 + 0.5) / nz as f64,
        ];
        assert_eq!(
            shader_cell_coord(uvw, dims),
            [1, 0, 0],
            "the probe must land in brick 1"
        );
        let sampled = sample_trilinear(&values, n, nz, uvw);
        assert!(
            (sampled - 120.0).abs() < 1.0e-9,
            "0.5 * 240 = 120, got {sampled}"
        );
        assert_eq!(
            bounds_without_apron(&values, &support, n, nz, dims, brick),
            [0, 0, 0, 0],
            "the upstream bound calls brick 1 empty, so the traverser skips it"
        );
        assert!(
            f64::from(accel.fine_minmax[brick * 4 + 1]) >= sampled,
            "the shipped bound must cover the filtered sample"
        );
    }

    /// The bound this module shipped before [`axis_span`]: the brick's own
    /// `FINE_BRICK`-voxel stride plus a one-voxel apron. Correct whenever the
    /// voxel count is a multiple of the cell count, and kept here to show
    /// exactly where it stops being correct when it is not.
    fn bounds_with_fixed_stride_apron(
        values: &[u8],
        support: &[u8],
        n: usize,
        nz: usize,
        dims: HierarchyDims,
        brick: usize,
    ) -> [u8; 4] {
        let fx = brick % dims.x;
        let fy = (brick / dims.x) % dims.y;
        let fz = brick / (dims.x * dims.y);
        let edge = FINE_BRICK as isize;
        let mut bounds = [u8::MAX, 0, u8::MAX, 0];
        for lz in -1..=edge {
            let z = clamp_index(fz as isize * edge + lz, nz);
            for ly in -1..=edge {
                let y = clamp_index(fy as isize * edge + ly, n);
                for lx in -1..=edge {
                    let x = clamp_index(fx as isize * edge + lx, n);
                    let index = z * n * n + y * n + x;
                    bounds[0] = bounds[0].min(values[index]);
                    bounds[1] = bounds[1].max(values[index]);
                    bounds[2] = bounds[2].min(support[index]);
                    bounds[3] = bounds[3].max(support[index]);
                }
            }
        }
        if bounds[3] == 0 {
            return [0, 0, 0, 0];
        }
        bounds
    }

    #[test]
    fn a_fixed_brick_stride_would_leave_reachable_voxels_out_of_the_bound() {
        // 20 voxels over 3 bricks. Brick 2 owns uvw in [2/3, 1), which the
        // sampler turns into texel coordinate [13.33, 19.5] and therefore into
        // voxels 12..=19. A fixed eight-voxel stride would bound only 15..=19
        // and declare brick 2 empty, so the traverser would skip an echo the
        // sampler can plainly reach. Nothing in the shipped app uses a
        // non-multiple lattice today, but `build_acceleration` is public and
        // advertises `div_ceil` dimensions, so it has to be right for one.
        let (n, nz) = (20usize, 13usize);
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        let index = box_index(13, 2, 2, n);
        values[index] = 250;
        support[index] = 200;

        let accel = build_acceleration(&values, &support, n, nz);
        let dims = accel.fine_dims;
        assert_eq!(dims, HierarchyDims { x: 3, y: 3, z: 2 });
        let brick = dims.index(2, 0, 0);

        // The sampler really does reach voxel 13 from inside brick 2.
        let uvw = [
            (13.0 + 0.5) / n as f64,
            (2.0 + 0.5) / n as f64,
            (2.0 + 0.5) / nz as f64,
        ];
        assert_eq!(shader_cell_coord(uvw, dims), [2, 0, 0]);
        assert!((sample_trilinear(&values, n, nz, uvw) - 250.0).abs() < 1.0e-9);

        // Neither the upstream bound nor the fixed-stride-plus-apron bound
        // this module shipped before can see voxel 13 from brick 2.
        assert_eq!(
            bounds_without_apron(&values, &support, n, nz, dims, brick),
            [0, 0, 0, 0],
            "the upstream bound calls brick 2 empty"
        );
        assert_eq!(
            bounds_with_fixed_stride_apron(&values, &support, n, nz, dims, brick),
            [0, 0, 0, 0],
            "the fixed-stride apron calls brick 2 empty too"
        );
        assert_eq!(accel.fine_minmax[brick * 4 + 1], 250);
        assert!(accel.fine_minmax[brick * 4 + 3] >= 200);
    }

    #[test]
    fn hierarchy_stays_conservative_on_a_lattice_that_is_not_a_multiple_of_the_brick() {
        // 20 x 20 x 13 gives 3 x 3 x 2 fine cells over 8-voxel bricks and
        // 1 x 1 x 1 coarse cells, so both levels have partial groups and both
        // the voxel span and the child span have to be derived rather than
        // strided.
        let (n, nz) = (20usize, 13usize);
        let mut state = 0x00A7_51DEu32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        for index in 0..values.len() {
            values[index] = (next() & 0xFF) as u8;
            support[index] = if next() % 100 < 35 {
                ((next() % 255) + 1) as u8
            } else {
                0
            };
        }
        let accel = build_acceleration(&values, &support, n, nz);
        assert_eq!(accel.fine_dims, HierarchyDims { x: 3, y: 3, z: 2 });
        assert_eq!(accel.coarse_dims, HierarchyDims { x: 1, y: 1, z: 1 });
        let tests = adversarial_skip_tests();
        let counts = assert_no_cell_can_hide_a_sample(&values, &support, n, nz, &accel, &tests);
        for (index, (label, _)) in tests.iter().enumerate() {
            assert!(
                counts[index].painting > 0,
                "{label} matched nothing on the ragged lattice"
            );
        }
    }

    #[test]
    fn the_axis_and_child_spans_reduce_to_the_shipped_brick_stride() {
        // 192 voxels over 24 cells is the shipped lattice: cell k must come out
        // as voxels 8k-1 ..= 8k+8, the brick plus a one-voxel apron.
        for cell in 0..24usize {
            assert_eq!(
                axis_span(cell, 24, 192),
                (cell as isize * 8 - 1, cell as isize * 8 + 8)
            );
        }
        for cell in 0..6usize {
            assert_eq!(
                axis_span(cell, 6, 48),
                (cell as isize * 8 - 1, cell as isize * 8 + 8)
            );
        }
        // And 24 fine cells over 6 coarse cells is exactly four children each.
        for cell in 0..6usize {
            assert_eq!(child_span(cell, 6, 24), (cell * 4, cell * 4 + 3));
        }
        for cell in 0..2usize {
            assert_eq!(child_span(cell, 2, 6), (cell * 3, cell * 3 + 2));
        }
        // A ragged lattice widens rather than narrows: 7 fine cells over 3
        // coarse cells gives the middle cell children 2..=4, not 3..=5.
        assert_eq!(child_span(0, 3, 7), (0, 2));
        assert_eq!(child_span(1, 3, 7), (2, 4));
        assert_eq!(child_span(2, 3, 7), (4, 6));
    }

    #[test]
    fn the_lattice_derived_bound_matches_the_fixed_stride_one_on_the_shipped_lattice() {
        // 192 x 192 x 48 is a multiple of the brick on every axis, so the fix
        // must be a no-op there: same bytes, cell for cell. Checked on a
        // smaller multiple lattice so the test stays fast.
        let (n, nz) = (64usize, 32usize);
        let mut state = 0x1234_ABCDu32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut values = vec![0u8; n * n * nz];
        let mut support = vec![0u8; n * n * nz];
        for index in 0..values.len() {
            values[index] = (next() & 0xFF) as u8;
            support[index] = if next() % 100 < 22 {
                ((next() % 255) + 1) as u8
            } else {
                0
            };
        }
        let accel = build_acceleration(&values, &support, n, nz);
        let dims = accel.fine_dims;
        for brick in 0..dims.len() {
            let shipped: [u8; 4] = accel.fine_minmax[brick * 4..brick * 4 + 4]
                .try_into()
                .expect("four channels");
            assert_eq!(
                shipped,
                bounds_with_fixed_stride_apron(&values, &support, n, nz, dims, brick),
                "brick {brick} changed on a lattice where nothing should have"
            );
        }
    }
}
