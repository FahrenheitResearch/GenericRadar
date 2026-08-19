//! Vertical cross-sections: a volume and a ground line in, a slice grid out.
//!
//! The workstation lets an analyst draw a line on a 2D pane and see the
//! vertical structure of the current product along it — the reconstruction
//! GR2Analyst calls a cross-section and a research radar would fly as an RHI.
//! This module is the pure sampling half: no egui, no state, no product
//! policy. The workstation owns which grids to slice (raw, dealiased,
//! storm-relative) and how the picture is drawn.
//!
//! The reconstruction is the same one `volumetric.rs` uses for the 3D box and
//! its cross-section functions, restated here rather than called because those
//! internals are private to their module and because this module's honesty
//! rule (below) needs the profile BEFORE censored gates are dropped:
//!
//! * Beam geometry is 4/3-effective-earth standard refraction (Doviak, R. J.,
//!   and D. S. Zrnić, 1993: *Doppler Radar and Weather Observations*, 2nd ed.,
//!   Academic Press, eqs. 2.28b/2.28c), via [`crate::beam`].
//! * Vertical interpolation between tilts is linear in ELEVATION ANGLE between
//!   the two bracketing beams, not in height (Zhang, J., K. Howard, and J. J.
//!   Gourley, 2005: Constructing three-dimensional multiple-radar
//!   reflectivity mosaics, *J. Atmos. Oceanic Technol.* 22, 30–42, eqs. 5–7).
//! * Values extend past the bottom/top beam only within half a beamwidth
//!   (range-dependent), never further (Zhang, J., and Coauthors, 2011:
//!   National Mosaic and Multi-Sensor QPE (NMQ) system, *Bull. Amer. Meteor.
//!   Soc.* 92, 1321–1338). Below the lowest beam a 300 m display floor keeps
//!   near-radar sections reaching the ground — the same documented divergence
//!   `volumetric.rs` carries.
//!
//! **The honesty rule this module adds.** The prior art keeps only gates that
//! produced a value, so when a tilt LOOKED at a column and saw nothing —
//! censored weak echo, the clear slot under an overhang — the interpolation
//! brackets straight across it and paints the gap with a blend of the beams
//! above and below. On a supercell that fabrication lands exactly where the
//! weak-echo region is, which is the feature a cross-section exists to show.
//! Here the profile keeps every tilt whose beam actually covered the column,
//! valued or not, and interpolation is only allowed between two CONSECUTIVE
//! covered beams that both saw echo. A bracket with a silent beam on one side
//! extends the valued side half a beamwidth and leaves the rest absent, so
//! beam-gap wedges, the cone of silence and the WER all read as what they
//! are: places the radar looked and saw nothing, or never looked at all.
//! No-data cells are NaN; renderers keep them transparent.

use radar_core::{MomentGrid, MomentType, RadarVolume};
use rayon::prelude::*;

use crate::beam::{beam_height_arl_m, ground_arc_m};
// One interpolation-policy vocabulary for every reconstruction in this crate:
// reflectivity/ZDR blend linearly, CC refuses to blend through a melting-layer
// minimum (Giangrande, S. E., J. M. Krause, and A. V. Ryzhkov, 2008: Automatic
// designation of the melting layer with a polarimetric prototype of the
// WSR-88D radar, *J. Appl. Meteor. Climatol.* 47, 1354–1364), velocity refuses
// to blend across strong shear.
pub use crate::volumetric::InterpPolicy;

/// 4/3-effective-earth radius, m (Doviak & Zrnić 1993 eq. 2.28 model).
const AE_M: f64 = 4.0 / 3.0 * 6_371_000.0;
/// WSR-88D half-power half-beamwidth, rad (0.95° aperture / 2).
const HALF_BEAMWIDTH_RAD: f64 = 0.475 * std::f64::consts::PI / 180.0;
/// Display floor for the surface extension below the lowest beam, m
/// (operational RHI convention; documented divergence from Zhang et al. 2011).
const SURFACE_EXTENSION_FLOOR_M: f64 = 300.0;
/// A column is covered by a tilt only if a radial lies within this many
/// degrees of the column's azimuth. A complete sweep always qualifies; a live
/// sweep that has not reached this azimuth yet must read as absent rather
/// than borrowing a radial from the far side of the arc.
const MAX_AZIMUTH_GAP_DEG: f32 = 2.0;
/// Two cuts within this elevation distance are legs of one split cut (or
/// SAILS revisits of one tilt) and merge into one profile entry, preferring
/// the leg that carries a value. WSR-88D VCPs space distinct tilts >= 0.4°
/// apart, so this can never merge two genuinely different tilts.
const SPLIT_CUT_MERGE_DEG: f64 = 0.2;
/// Velocity difference between bracketing beams above which blending would
/// manufacture intermediate velocities across a shear layer or residual
/// alias; the nearer beam wins instead. Same constant as `volumetric.rs`.
const VELOCITY_GUARD_MPS: f32 = 30.0;
/// Correlation-coefficient floor below which a bracket may span the melting
/// layer; blending would fabricate intermediate rho_hv, so the nearer beam
/// wins (Giangrande, Krause & Ryzhkov 2008). Same constant as `volumetric.rs`.
const CC_GUARD: f32 = 0.97;

/// The geometry of one requested slice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliceRequest {
    /// Line start, radar-local kilometres (east, north).
    pub start_km: (f32, f32),
    /// Line end, radar-local kilometres (east, north).
    pub end_km: (f32, f32),
    /// Columns along the line. At least 2.
    pub width: usize,
    /// Rows in the vertical. At least 2.
    pub height: usize,
    /// Height of row 0 above the radar, metres. Row `height-1` is 0 m ARL.
    pub top_m: f32,
}

/// Horizontal cleanup of the finished grid.
///
/// Each column samples one nearest radial and gate per tilt, so adjacent
/// columns can disagree gate-to-gate ("barcode") and a column can miss in an
/// azimuth gap. `Smoothed` fills gaps of at most two columns from horizontal
/// neighbours and applies a NaN-aware 3-tap blend — the same cleanup every
/// operational RHI display applies, and the same two passes `volumetric.rs`
/// runs. It never fills across the wide absences the honesty rule creates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SliceSmoothing {
    Native,
    Smoothed,
}

/// A reconstructed vertical slice. `values[y * width + x]`, `y = 0` at
/// `top_m`, `x = 0` at the start point. NaN = the radar saw nothing there.
#[derive(Clone, Debug)]
pub struct Slice {
    pub width: usize,
    pub height: usize,
    pub top_m: f32,
    pub length_m: f32,
    pub start_km: (f32, f32),
    pub end_km: (f32, f32),
    pub values: Vec<f32>,
}

impl Slice {
    pub fn value_at(&self, x: usize, y: usize) -> f32 {
        if x < self.width && y < self.height {
            self.values[y * self.width + x]
        } else {
            f32::NAN
        }
    }

    /// Height of row `y` above the radar, metres.
    pub fn height_m_at_row(&self, y: usize) -> f32 {
        if self.height < 2 {
            return 0.0;
        }
        self.top_m * (1.0 - y as f32 / (self.height - 1) as f32)
    }

    /// Distance of column `x` along the line from the start point, metres.
    pub fn distance_m_at_col(&self, x: usize) -> f32 {
        if self.width < 2 {
            return 0.0;
        }
        self.length_m * x as f32 / (self.width - 1) as f32
    }

    /// Ground position of column `x`, radar-local kilometres (east, north).
    pub fn point_km_at_col(&self, x: usize) -> (f32, f32) {
        if self.width < 2 {
            return self.start_km;
        }
        let f = x as f32 / (self.width - 1) as f32;
        (
            self.start_km.0 + (self.end_km.0 - self.start_km.0) * f,
            self.start_km.1 + (self.end_km.1 - self.start_km.1) * f,
        )
    }

    /// Compass azimuth from the radar to column `x`, degrees. What a
    /// storm-relative transform needs to project the motion vector onto the
    /// radial through this column.
    pub fn azimuth_deg_at_col(&self, x: usize) -> f32 {
        let (east, north) = self.point_km_at_col(x);
        crate::beam::compass_azimuth_deg(f64::from(east), f64::from(north)) as f32
    }
}

/// One tilt prepared for column sampling: an azimuth index over its radials
/// and, per gate, where its beam centre sits over the ground.
struct TiltSampler<'a> {
    elevation_deg: f32,
    grid: &'a MomentGrid,
    /// (azimuth_deg, grid row), sorted by azimuth.
    az_rows: Vec<(f32, usize)>,
    /// Ground distance of each gate's beam centre from the radar, m
    /// (Doviak & Zrnić 1993 eq. 2.28c). Monotonic in gate index.
    ground_range_m: Vec<f64>,
    /// Beam-centre height of each gate above the radar, m (eq. 2.28b).
    height_m: Vec<f64>,
}

impl<'a> TiltSampler<'a> {
    fn new(volume: &'a RadarVolume, cut_index: usize, grid: &'a MomentGrid) -> Option<Self> {
        let cut = volume.cuts.get(cut_index)?;
        if grid.gate_range.gate_count == 0 {
            return None;
        }
        let mut az_rows: Vec<(f32, usize)> = grid
            .radial_indices
            .iter()
            .enumerate()
            .filter_map(|(row, radial_index)| {
                let azimuth = cut
                    .radials
                    .get(*radial_index)?
                    .azimuth_deg
                    .rem_euclid(360.0);
                Some((azimuth, row))
            })
            .collect();
        if az_rows.is_empty() {
            return None;
        }
        az_rows.sort_by(|a, b| a.0.total_cmp(&b.0));

        let elevation_deg = cut.elevation_deg;
        let gate_range = &grid.gate_range;
        let (ground_range_m, height_m) = (0..gate_range.gate_count)
            .map(|gate| {
                let slant = f64::from(gate_range.first_gate_m)
                    + gate as f64 * f64::from(gate_range.gate_spacing_m);
                (
                    ground_arc_m(slant, f64::from(elevation_deg)),
                    beam_height_arl_m(slant, f64::from(elevation_deg)),
                )
            })
            .unzip();

        Some(Self {
            elevation_deg,
            grid,
            az_rows,
            ground_range_m,
            height_m,
        })
    }

    /// The grid row of the radial nearest `azimuth`, if one lies within
    /// [`MAX_AZIMUTH_GAP_DEG`]. `None` means the sweep has no radial pointing
    /// this way — a live sweep still arriving, or a sector scan.
    fn row_near_azimuth(&self, azimuth: f32) -> Option<usize> {
        let (row, distance) = match self.az_rows.binary_search_by(|p| p.0.total_cmp(&azimuth)) {
            Ok(i) => (self.az_rows[i].1, 0.0),
            Err(i) => {
                let lo = if i == 0 {
                    self.az_rows.len() - 1
                } else {
                    i - 1
                };
                let hi = if i >= self.az_rows.len() { 0 } else { i };
                let dl = angular_distance_deg(self.az_rows[lo].0, azimuth);
                let dh = angular_distance_deg(self.az_rows[hi].0, azimuth);
                if dl <= dh {
                    (self.az_rows[lo].1, dl)
                } else {
                    (self.az_rows[hi].1, dh)
                }
            }
        };
        (distance <= MAX_AZIMUTH_GAP_DEG).then_some(row)
    }

    /// Gate whose beam centre is nearest ground distance `s`, or `None` when
    /// this tilt's beam never passes over that ground point. The lower bound
    /// matters for high tilts: clamping short ranges to gate 0 would smear
    /// elevated echo into the cone of silence.
    fn gate_for_ground_range(&self, s: f64) -> Option<usize> {
        let n = self.ground_range_m.len();
        if n == 0 {
            return None;
        }
        let half_gate = if n >= 2 {
            0.5 * (self.ground_range_m[1] - self.ground_range_m[0])
        } else {
            0.0
        };
        if s > self.ground_range_m[n - 1] || s < self.ground_range_m[0] - half_gate {
            return None;
        }
        match self.ground_range_m.binary_search_by(|g| g.total_cmp(&s)) {
            Ok(i) => Some(i),
            Err(i) => {
                if i == 0 {
                    Some(0)
                } else if i >= n {
                    Some(n - 1)
                } else if (self.ground_range_m[i] - s) < (s - self.ground_range_m[i - 1]) {
                    Some(i)
                } else {
                    Some(i - 1)
                }
            }
        }
    }
}

fn angular_distance_deg(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// One tilt's contribution to a column: where its beam centre is, and what it
/// saw. `value: None` is the load-bearing half — the beam covered this ground
/// point and reported nothing, and the interpolation must respect that.
#[derive(Clone, Copy)]
struct ProfileSample {
    height_m: f64,
    elevation_deg: f64,
    slant_m: f64,
    value: Option<f32>,
}

/// Every tilt of one volume prepared for slicing, sorted by elevation.
///
/// Building this walks each tilt's radials and gates once; sampling any number
/// of slices afterwards only does binary searches into it. Rebuild it when the
/// volume grows (new radials or cuts) and the next slice follows the growth.
pub struct SliceVolume<'a> {
    tilts: Vec<TiltSampler<'a>>,
}

impl<'a> SliceVolume<'a> {
    /// All cuts of `volume` that carry `moment`, as stored.
    pub fn from_volume(volume: &'a RadarVolume, moment: &MomentType) -> Self {
        Self::from_indexed_grids(
            volume,
            volume
                .cuts
                .iter()
                .enumerate()
                .filter_map(|(index, cut)| cut.moments.get(moment).map(|grid| (index, grid))),
        )
    }

    /// Cuts paired with caller-supplied grids — the path for dealiased or
    /// otherwise transformed velocity, mirroring
    /// [`crate::volumetric::velocity_cross_section_from_dealiased_with_smoothing`].
    /// Each grid must be radial-aligned with its cut.
    pub fn from_indexed_grids(
        volume: &'a RadarVolume,
        grids: impl IntoIterator<Item = (usize, &'a MomentGrid)>,
    ) -> Self {
        let mut tilts: Vec<TiltSampler<'a>> = grids
            .into_iter()
            .filter_map(|(cut_index, grid)| TiltSampler::new(volume, cut_index, grid))
            .collect();
        tilts.sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
        Self { tilts }
    }

    /// How many tilts will contribute. Distinct legs of a split cut count
    /// separately here; they merge per column during sampling.
    pub fn tilt_count(&self) -> usize {
        self.tilts.len()
    }

    /// The column of samples over ground point (`azimuth`, `s` metres):
    /// every covered tilt, ascending in height, split-cut legs merged.
    fn column_profile(&self, azimuth: f32, s: f64) -> Vec<ProfileSample> {
        let mut profile: Vec<ProfileSample> = self
            .tilts
            .iter()
            .filter_map(|tilt| {
                let gate = tilt.gate_for_ground_range(s)?;
                let row = tilt.row_near_azimuth(azimuth)?;
                let value = tilt.grid.scaled_value(row, gate).filter(|v| v.is_finite());
                let gate_range = &tilt.grid.gate_range;
                let slant_m = f64::from(gate_range.first_gate_m)
                    + gate as f64 * f64::from(gate_range.gate_spacing_m);
                Some(ProfileSample {
                    height_m: tilt.height_m[gate],
                    elevation_deg: f64::from(tilt.elevation_deg),
                    slant_m,
                    value,
                })
            })
            .collect();
        profile.sort_by(|a, b| a.height_m.total_cmp(&b.height_m));
        merge_split_cut_legs(&mut profile);
        profile
    }
}

/// Collapse profile entries whose elevations are within
/// [`SPLIT_CUT_MERGE_DEG`] into one, preferring the leg that carries a value.
///
/// Without this, a censored Doppler leg sitting metres from a valued
/// surveillance leg of the same tilt would count as "the adjacent beam saw
/// nothing" and the honesty rule would cut a false gap between two tilts that
/// both saw the storm.
fn merge_split_cut_legs(profile: &mut Vec<ProfileSample>) {
    let mut merged: Vec<ProfileSample> = Vec::with_capacity(profile.len());
    for sample in profile.drain(..) {
        match merged.last_mut() {
            Some(last)
                if (sample.elevation_deg - last.elevation_deg).abs() < SPLIT_CUT_MERGE_DEG =>
            {
                if last.value.is_none() && sample.value.is_some() {
                    *last = sample;
                }
            }
            _ => merged.push(sample),
        }
    }
    *profile = merged;
}

/// Slant range and elevation angle of the ray through (ground distance `s`,
/// height `z`) — the closed-form inverse of the 4/3-earth height equation
/// (law of cosines on the effective sphere; the same inverse `volumetric.rs`
/// uses, round-tripped against `crate::beam` in the tests below).
fn invert_beam(s: f64, z: f64) -> (f64, f64) {
    let sigma = s / AE_M;
    let r = (AE_M * AE_M + (AE_M + z) * (AE_M + z) - 2.0 * AE_M * (AE_M + z) * sigma.cos())
        .max(0.0)
        .sqrt();
    if r < 1.0 {
        return (0.0, 90.0);
    }
    let sin_theta =
        (((AE_M + z) * (AE_M + z) - AE_M * AE_M - r * r) / (2.0 * AE_M * r)).clamp(-1.0, 1.0);
    (r, sin_theta.asin().to_degrees())
}

/// Half a beamwidth of height at this beam's range — how far a value may
/// honestly extend past the beam centre (Zhang et al. 2011 edge rule).
fn half_beamwidth_extension_m(sample: &ProfileSample) -> f64 {
    sample.slant_m * HALF_BEAMWIDTH_RAD
}

/// The value at height `z` in a covered-beam profile, or `None` where the
/// radar saw nothing. This is where every honesty decision lives.
fn interpolate_profile(
    profile: &[ProfileSample],
    z: f64,
    s: f64,
    policy: InterpPolicy,
) -> Option<f32> {
    let first = profile.first()?;
    let last = profile[profile.len() - 1];

    if z <= first.height_m {
        // Surface extension: the lowest beam stands for the column beneath it,
        // half a beamwidth down with a 300 m display floor.
        let extend = half_beamwidth_extension_m(first).max(SURFACE_EXTENSION_FLOOR_M);
        return (first.height_m - z <= extend)
            .then_some(first.value)
            .flatten();
    }
    if z >= last.height_m {
        // Above the top beam: half a beamwidth and then the cone of silence.
        return (z - last.height_m <= half_beamwidth_extension_m(&last))
            .then_some(last.value)
            .flatten();
    }

    for pair in profile.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        if z < lo.height_m || z > hi.height_m {
            continue;
        }
        return match (lo.value, hi.value) {
            // Two consecutive covered beams that both saw echo: the standard
            // linear-in-elevation-angle blend (Zhang, Howard & Gourley 2005),
            // with the per-moment guards.
            (Some(below), Some(above)) => {
                let nearest = if (z - lo.height_m) <= (hi.height_m - z) {
                    below
                } else {
                    above
                };
                match policy {
                    InterpPolicy::CcGuard if below.min(above) < CC_GUARD => Some(nearest),
                    InterpPolicy::VelocityGuard if (above - below).abs() > VELOCITY_GUARD_MPS => {
                        Some(nearest)
                    }
                    _ => {
                        let span = hi.elevation_deg - lo.elevation_deg;
                        if span.abs() < 1e-6 {
                            return Some(below);
                        }
                        let (_, theta) = invert_beam(s, z);
                        let weight = ((theta - lo.elevation_deg) / span).clamp(0.0, 1.0) as f32;
                        Some(below + (above - below) * weight)
                    }
                }
            }
            // One side of the bracket looked and saw nothing: the valued side
            // reaches half a beamwidth into the gap and the rest is absent —
            // the rule that keeps a WER open and an overhang off the ground.
            (Some(below), None) => {
                (z - lo.height_m <= half_beamwidth_extension_m(&lo)).then_some(below)
            }
            (None, Some(above)) => {
                (hi.height_m - z <= half_beamwidth_extension_m(&hi)).then_some(above)
            }
            (None, None) => None,
        };
    }
    None
}

/// Build one slice. `None` when the request is degenerate or the volume has
/// no tilt carrying the moment.
///
/// Columns are independent and run in parallel, so an endpoint drag that
/// rebuilds per frame stays fluid; measured on a real 15-tilt KUDX volume the
/// whole build is single-digit milliseconds (see the real-volume test).
pub fn sample_slice(
    volume: &SliceVolume<'_>,
    request: &SliceRequest,
    policy: InterpPolicy,
    smoothing: SliceSmoothing,
) -> Option<Slice> {
    let SliceRequest {
        start_km,
        end_km,
        width,
        height,
        top_m,
    } = *request;
    if width < 2 || height < 2 || !top_m.is_finite() || top_m <= 0.0 || volume.tilts.is_empty() {
        return None;
    }
    if ![start_km.0, start_km.1, end_km.0, end_km.1]
        .iter()
        .all(|v| v.is_finite())
    {
        return None;
    }
    let length_m = ((end_km.0 - start_km.0).hypot(end_km.1 - start_km.1) * 1000.0).max(0.0);

    let columns: Vec<Vec<f32>> = (0..width)
        .into_par_iter()
        .map(|x| {
            let f = x as f32 / (width - 1) as f32;
            let east = start_km.0 + (end_km.0 - start_km.0) * f;
            let north = start_km.1 + (end_km.1 - start_km.1) * f;
            let s = f64::from(east.hypot(north)) * 1000.0;
            let azimuth = east.atan2(north).to_degrees().rem_euclid(360.0);
            let profile = volume.column_profile(azimuth, s);
            let mut column = vec![f32::NAN; height];
            if profile.is_empty() {
                return column;
            }
            for (y, cell) in column.iter_mut().enumerate() {
                let z = f64::from(top_m) * (1.0 - y as f64 / (height - 1) as f64);
                if let Some(value) = interpolate_profile(&profile, z, s, policy) {
                    *cell = value;
                }
            }
            column
        })
        .collect();

    let mut values = vec![f32::NAN; width * height];
    for (x, column) in columns.iter().enumerate() {
        for (y, value) in column.iter().enumerate() {
            values[y * width + x] = *value;
        }
    }
    if smoothing == SliceSmoothing::Smoothed {
        values = smooth_columns(values, width, height);
    }
    Some(Slice {
        width,
        height,
        top_m,
        length_m,
        start_km,
        end_km,
        values,
    })
}

/// The two horizontal cleanup passes described on [`SliceSmoothing`].
fn smooth_columns(values: Vec<f32>, width: usize, height: usize) -> Vec<f32> {
    // Pass 1: fill gaps of at most two columns from horizontal neighbours.
    let mut filled = values.clone();
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            if values[row + x].is_finite() {
                continue;
            }
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for dx in [-2isize, -1, 1, 2] {
                let xi = x as isize + dx;
                if xi < 0 || xi >= width as isize {
                    continue;
                }
                let v = values[row + xi as usize];
                if v.is_finite() {
                    sum += v;
                    n += 1.0;
                }
            }
            if n >= 2.0 {
                filled[row + x] = sum / n;
            }
        }
    }
    // Pass 2: NaN-aware 3-tap blend. Never invents a value where pass 1 left
    // absence, so the honesty gaps survive.
    let mut smoothed = filled.clone();
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            if !filled[row + x].is_finite() {
                continue;
            }
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for dx in [-1isize, 0, 1] {
                let xi = x as isize + dx;
                if xi < 0 || xi >= width as isize {
                    continue;
                }
                let v = filled[row + xi as usize];
                if v.is_finite() {
                    sum += v;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                smoothed[row + x] = sum / n;
            }
        }
    }
    smoothed
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{ElevationCut, GateRange, MomentStorage, RadarSite, Radial};

    /// A cut with `az_count` radials and a uniform value, except where
    /// `censor` says a gate reports nothing.
    fn cut_with_value(
        elevation_deg: f32,
        az_count: usize,
        gates: usize,
        value: f32,
        censor: impl Fn(usize, usize) -> bool,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation_deg, None);
        for k in 0..az_count {
            cut.radials.push(Radial {
                azimuth_deg: k as f32 * (360.0 / az_count as f32),
                elevation_deg,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        let mut values = vec![value; az_count * gates];
        for row in 0..az_count {
            for gate in 0..gates {
                if censor(row, gate) {
                    values[row * gates + gate] = f32::NAN;
                }
            }
        }
        let grid = MomentGrid {
            moment: MomentType::Reflectivity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..az_count).collect(),
            storage: MomentStorage::F32(values),
        };
        cut.moments.insert(MomentType::Reflectivity, grid);
        cut
    }

    fn volume_with_cuts(cuts: Vec<ElevationCut>) -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("TEST"),
            chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp"),
        );
        volume.cuts = cuts;
        volume
    }

    fn slice_of(volume: &RadarVolume, request: &SliceRequest) -> Slice {
        let prepared = SliceVolume::from_volume(volume, &MomentType::Reflectivity);
        sample_slice(
            &prepared,
            request,
            InterpPolicy::LinearAngle,
            SliceSmoothing::Native,
        )
        .expect("slice builds")
    }

    fn east_line() -> SliceRequest {
        SliceRequest {
            start_km: (10.0, 0.0),
            end_km: (60.0, 0.0),
            width: 100,
            height: 80,
            top_m: 15_000.0,
        }
    }

    #[test]
    fn the_beam_inverse_round_trips_the_forward_geometry() {
        for (slant, elevation) in [
            (30_000.0, 0.5),
            (120_000.0, 0.5),
            (60_000.0, 4.0),
            (100_000.0, 10.0),
        ] {
            let s = ground_arc_m(slant, elevation);
            let z = beam_height_arl_m(slant, elevation);
            let (r, theta) = invert_beam(s, z);
            assert!(
                (r - slant).abs() < 1.0,
                "{elevation}°: slant {slant} inverted to {r}"
            );
            assert!(
                (theta - elevation).abs() < 1e-3,
                "{elevation}° inverted to {theta}°"
            );
        }
    }

    #[test]
    fn a_uniform_two_tilt_volume_fills_between_the_beams_and_not_above_them() {
        let volume = volume_with_cuts(vec![
            cut_with_value(0.5, 360, 120, 40.0, |_, _| false),
            cut_with_value(4.0, 360, 120, 40.0, |_, _| false),
        ]);
        let slice = slice_of(&volume, &east_line());
        // At 35 km east: 0.5° beam is ~370 m up, 4° beam is ~2.5 km up.
        // Everything between them is bracketed by two valued beams.
        let x = 50; // 35 km along a 10..60 km line
        let mid = slice
            .values
            .iter()
            .skip(x)
            .step_by(slice.width)
            .filter(|v| v.is_finite())
            .count();
        assert!(mid > 0, "the bracketed span carries values");
        // The top rows sit far above the 4° beam plus half a beamwidth and
        // must be absent — that is the cone-of-silence wedge.
        assert!(
            slice.values[..slice.width].iter().all(|v| v.is_nan()),
            "the top of the slice is above every beam and stays empty"
        );
        // Values that do exist are the uniform field, not a blend artefact.
        for v in slice.values.iter().filter(|v| v.is_finite()) {
            assert!((v - 40.0).abs() < 1e-3, "uniform in, uniform out: {v}");
        }
    }

    /// The honesty rule. Three tilts; the middle one covered the column and
    /// saw NOTHING. The prior art bracketed tilt 1 against tilt 3 and painted
    /// the whole wedge; here the gap stays open beyond half a beamwidth.
    #[test]
    fn a_silent_middle_tilt_cuts_the_bracket_instead_of_being_painted_over() {
        let volume = volume_with_cuts(vec![
            cut_with_value(0.5, 360, 120, 40.0, |_, _| false),
            // 4°: censored everywhere — looked, saw nothing.
            cut_with_value(4.0, 360, 120, 40.0, |_, _| true),
            cut_with_value(8.0, 360, 120, 40.0, |_, _| false),
        ]);
        let slice = slice_of(&volume, &east_line());
        // At 35 km: 0.5° ≈ 0.38 km, 4° ≈ 2.5 km, 8° ≈ 5 km. Half a beamwidth
        // at 35 km is ~290 m. Heights well inside the 0.5°–4° gap and the
        // 4°–8° gap must be absent.
        let x = 50;
        let column: Vec<f32> = (0..slice.height).map(|y| slice.value_at(x, y)).collect();
        let value_at_height = |z_m: f32| {
            let y = ((1.0 - z_m / slice.top_m) * (slice.height - 1) as f32).round() as usize;
            column[y]
        };
        assert!(
            value_at_height(1_500.0).is_nan(),
            "midway between the low beam and the silent beam is absent"
        );
        assert!(
            value_at_height(3_700.0).is_nan(),
            "midway between the silent beam and the high beam is absent"
        );
        // The valued beams still paint their own neighbourhoods.
        assert!(
            value_at_height(400.0).is_finite(),
            "the low beam's own height still carries its value"
        );
        assert!(
            value_at_height(5_000.0).is_finite(),
            "the high beam's own height still carries its value"
        );
    }

    /// Without the middle tilt flown at all the geometry is identical, and the
    /// two valued beams are CONSECUTIVE covered beams — so the wedge fills.
    /// Together with the test above this pins the distinction: interpolation
    /// is between adjacent flown beams, not across a beam that saw nothing.
    #[test]
    fn adjacent_valued_tilts_still_interpolate_across_their_gap() {
        let volume = volume_with_cuts(vec![
            cut_with_value(0.5, 360, 120, 40.0, |_, _| false),
            cut_with_value(8.0, 360, 120, 40.0, |_, _| false),
        ]);
        let slice = slice_of(&volume, &east_line());
        let x = 50;
        let y_mid = ((1.0 - 2_500.0 / slice.top_m) * (slice.height - 1) as f32).round() as usize;
        assert!(
            slice.value_at(x, y_mid).is_finite(),
            "two adjacent valued beams bracket the space between them"
        );
    }

    /// A censored Doppler leg metres from a valued surveillance leg of the
    /// SAME tilt must not read as "the adjacent beam saw nothing".
    #[test]
    fn a_censored_split_cut_leg_does_not_cut_a_false_gap() {
        let volume = volume_with_cuts(vec![
            cut_with_value(0.48, 360, 120, 40.0, |_, _| false),
            // The other leg of the same tilt, fully censored.
            cut_with_value(0.53, 360, 120, 40.0, |_, _| true),
            cut_with_value(4.0, 360, 120, 40.0, |_, _| false),
        ]);
        let slice = slice_of(&volume, &east_line());
        let x = 50;
        let y_mid = ((1.0 - 1_500.0 / slice.top_m) * (slice.height - 1) as f32).round() as usize;
        assert!(
            slice.value_at(x, y_mid).is_finite(),
            "the merged split cut keeps the 0.5°–4° bracket intact"
        );
    }

    /// A sweep that has only reached one sector must read as absent elsewhere
    /// rather than borrowing a radial from the far side of the arc.
    #[test]
    fn a_partial_sweep_is_absent_where_it_has_not_scanned() {
        let mut cut = cut_with_value(0.5, 360, 120, 40.0, |_, _| false);
        // Keep only radials 0..90 (azimuths 0..90°): a quarter of the sweep.
        cut.radials.truncate(90);
        let grid = cut
            .moments
            .get_mut(&MomentType::Reflectivity)
            .expect("reflectivity exists");
        grid.radial_indices.truncate(90);
        match &mut grid.storage {
            MomentStorage::F32(values) => values.truncate(90 * 120),
            _ => unreachable!("test grid is F32"),
        }
        let volume = volume_with_cuts(vec![cut]);

        // Due west (azimuth 270°) — far outside the scanned sector.
        let west = SliceRequest {
            start_km: (-10.0, 0.0),
            end_km: (-60.0, 0.0),
            width: 60,
            height: 40,
            top_m: 15_000.0,
        };
        let prepared = SliceVolume::from_volume(&volume, &MomentType::Reflectivity);
        let slice = sample_slice(
            &prepared,
            &west,
            InterpPolicy::LinearAngle,
            SliceSmoothing::Native,
        )
        .expect("slice builds");
        assert!(
            slice.values.iter().all(|v| v.is_nan()),
            "an unscanned azimuth carries no data"
        );

        // Due north-east (azimuth 45°) — inside the sector — still works.
        let inside = SliceRequest {
            start_km: (7.0, 7.0),
            end_km: (40.0, 40.0),
            width: 60,
            height: 40,
            top_m: 15_000.0,
        };
        let slice = sample_slice(
            &prepared,
            &inside,
            InterpPolicy::LinearAngle,
            SliceSmoothing::Native,
        )
        .expect("slice builds");
        assert!(
            slice.values.iter().any(|v| v.is_finite()),
            "the scanned sector carries data"
        );
    }

    #[test]
    fn the_velocity_guard_refuses_to_blend_across_strong_shear() {
        let profile = [
            ProfileSample {
                height_m: 1_000.0,
                elevation_deg: 0.5,
                slant_m: 50_000.0,
                value: Some(-20.0),
            },
            ProfileSample {
                height_m: 3_000.0,
                elevation_deg: 3.0,
                slant_m: 50_000.0,
                value: Some(25.0),
            },
        ];
        let blended = interpolate_profile(&profile, 2_000.0, 50_000.0, InterpPolicy::VelocityGuard)
            .expect("bracketed");
        assert!(
            blended == -20.0 || blended == 25.0,
            "45 m/s across the bracket must snap to a beam, got {blended}"
        );
        let linear = interpolate_profile(&profile, 2_000.0, 50_000.0, InterpPolicy::LinearAngle)
            .expect("bracketed");
        assert!(
            linear > -20.0 && linear < 25.0,
            "the linear policy does blend, got {linear}"
        );
    }

    #[test]
    fn degenerate_requests_and_empty_volumes_build_nothing() {
        let volume = volume_with_cuts(vec![cut_with_value(0.5, 360, 120, 40.0, |_, _| false)]);
        let prepared = SliceVolume::from_volume(&volume, &MomentType::Reflectivity);
        let good = east_line();
        for bad in [
            SliceRequest { width: 1, ..good },
            SliceRequest { height: 1, ..good },
            SliceRequest { top_m: 0.0, ..good },
            SliceRequest {
                top_m: f32::NAN,
                ..good
            },
            SliceRequest {
                start_km: (f32::NAN, 0.0),
                ..good
            },
        ] {
            assert!(
                sample_slice(
                    &prepared,
                    &bad,
                    InterpPolicy::LinearAngle,
                    SliceSmoothing::Native
                )
                .is_none(),
                "{bad:?} must not build"
            );
        }
        let empty = SliceVolume::from_volume(&volume, &MomentType::Velocity);
        assert!(
            sample_slice(
                &empty,
                &good,
                InterpPolicy::LinearAngle,
                SliceSmoothing::Native
            )
            .is_none(),
            "a volume with no tilt of the moment builds nothing"
        );
    }

    #[test]
    fn slice_axis_helpers_report_the_geometry_the_grid_was_built_on() {
        let volume = volume_with_cuts(vec![
            cut_with_value(0.5, 360, 120, 40.0, |_, _| false),
            cut_with_value(4.0, 360, 120, 40.0, |_, _| false),
        ]);
        let slice = slice_of(&volume, &east_line());
        assert!((slice.length_m - 50_000.0).abs() < 1.0);
        assert_eq!(slice.height_m_at_row(0), slice.top_m);
        assert_eq!(slice.height_m_at_row(slice.height - 1), 0.0);
        assert_eq!(slice.distance_m_at_col(0), 0.0);
        assert!((slice.distance_m_at_col(slice.width - 1) - slice.length_m).abs() < 0.5);
        // A due-east line: every column bears 090.
        assert!((slice.azimuth_deg_at_col(slice.width / 2) - 90.0).abs() < 1e-3);
    }

    // ------------------------------------------------------------------
    // Real-volume tests. They use the workstation's own Level II cache and
    // skip silently on a machine that has never run the app, exactly like the
    // real-data tests in `quality.rs`.
    // ------------------------------------------------------------------

    fn level2_cache_dir() -> Option<std::path::PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")?;
        let path = std::path::PathBuf::from(local)
            .join("FahrenheitResearch")
            .join("RadarWorkstation")
            .join("cache")
            .join("level2-live");
        path.is_dir().then_some(path)
    }

    fn decode_cached(name_contains: &str) -> Option<RadarVolume> {
        let dir = level2_cache_dir()?;
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.contains(name_contains))
            })
            .collect();
        paths.sort();
        paths
            .into_iter()
            .find_map(|path| nexrad_io::decode_volume_from_path(&path).ok())
    }

    fn any_cached_volume() -> Option<RadarVolume> {
        let dir = level2_cache_dir()?;
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        paths.sort();
        paths
            .into_iter()
            .find_map(|path| nexrad_io::decode_volume_from_path(&path).ok())
    }

    /// The strongest low-tilt reflectivity cell: (east_km, north_km).
    fn strongest_cell_km(volume: &RadarVolume) -> Option<(f32, f32)> {
        let (cut, grid) = volume.cuts.iter().find_map(|cut| {
            cut.moments
                .get(&MomentType::Reflectivity)
                .map(|grid| (cut, grid))
        })?;
        let gates = grid.gate_range.gate_count;
        let mut best: Option<(f32, usize, usize)> = None;
        for row in 0..grid.radial_indices.len() {
            for gate in 0..gates {
                let Some(value) = grid.scaled_value(row, gate) else {
                    continue;
                };
                if value.is_finite() && best.as_ref().is_none_or(|(b, _, _)| value > *b) {
                    best = Some((value, row, gate));
                }
            }
        }
        let (_, row, gate) = best?;
        let radial = cut.radials.get(grid.radial_indices[row])?;
        let slant = f64::from(grid.gate_range.first_gate_m)
            + gate as f64 * f64::from(grid.gate_range.gate_spacing_m);
        let ground = ground_arc_m(slant, f64::from(cut.elevation_deg)) / 1000.0;
        let azimuth = f64::from(radial.azimuth_deg).to_radians();
        Some((
            (ground * azimuth.sin()) as f32,
            (ground * azimuth.cos()) as f32,
        ))
    }

    /// A slice request through the strongest cell, oriented along the radial
    /// through it: `half_km` each way.
    fn line_through_cell(cell: (f32, f32), half_km: f32) -> SliceRequest {
        let range = cell.0.hypot(cell.1).max(1.0);
        let (ux, uy) = (cell.0 / range, cell.1 / range);
        SliceRequest {
            start_km: (cell.0 - ux * half_km, cell.1 - uy * half_km),
            end_km: (cell.0 + ux * half_km, cell.1 + uy * half_km),
            width: 640,
            height: 320,
            top_m: 18_000.0,
        }
    }

    /// On a real volume: the slice builds, carries echo, keeps the cone of
    /// silence empty, and the build cost is small enough to follow a live
    /// volume. The timing prints so the number lands in the test log.
    #[test]
    fn real_volume_slice_carries_echo_and_reports_its_build_cost() {
        let Some(volume) = any_cached_volume() else {
            eprintln!("no cached Level II volume; skipping");
            return;
        };
        let Some(cell) = strongest_cell_km(&volume) else {
            eprintln!("no reflectivity in the cached volume; skipping");
            return;
        };
        let request = line_through_cell(cell, 40.0);

        let prepare_started = std::time::Instant::now();
        let prepared = SliceVolume::from_volume(&volume, &MomentType::Reflectivity);
        let prepare_ms = prepare_started.elapsed().as_secs_f32() * 1_000.0;

        let sample_started = std::time::Instant::now();
        let slice = sample_slice(
            &prepared,
            &request,
            InterpPolicy::LinearAngle,
            SliceSmoothing::Smoothed,
        )
        .expect("a real volume slices");
        let sample_ms = sample_started.elapsed().as_secs_f32() * 1_000.0;

        let finite = slice.values.iter().filter(|v| v.is_finite()).count();
        eprintln!(
            "site {} · {} tilts · prepare {prepare_ms:.1} ms · sample {sample_ms:.1} ms · {}x{} · {:.1}% observed",
            volume.site.id,
            prepared.tilt_count(),
            slice.width,
            slice.height,
            finite as f32 / slice.values.len() as f32 * 100.0,
        );
        assert!(finite > 0, "a slice through the strongest cell sees echo");
        // The very top of an 18 km slice at <= 100 km range is above every
        // WSR-88D tilt plus half a beamwidth: the cone-of-silence wedge.
        assert!(
            slice.values[..slice.width].iter().all(|v| v.is_nan()),
            "row 0 (18 km ARL) must be empty on a real volume"
        );
    }

    /// The PROOF picture: render real slices to PNG for eyes, when
    /// `XSECTION_PNG_DIR` says where. `--ignored` only; the CI gate stays
    /// hermetic. Draws reflectivity and velocity through the strongest cell
    /// of the named volumes with the default colour tables.
    #[ignore = "set XSECTION_PNG_DIR and run with --ignored to write proof PNGs"]
    #[test]
    fn real_volume_slice_png_proof() {
        let out_dir = std::env::var("XSECTION_PNG_DIR").expect("XSECTION_PNG_DIR is set");
        let out_dir = std::path::PathBuf::from(out_dir);
        std::fs::create_dir_all(&out_dir).expect("output directory exists");
        let tables = color_tables::ColorTableSet::default();

        for name in [
            "KUDX20260819_0437",
            "KMKX20260818_2031",
            "KARX20260818_2045",
        ] {
            let Some(volume) = decode_cached(name) else {
                eprintln!("{name}: not in cache, skipped");
                continue;
            };
            let Some(cell) = strongest_cell_km(&volume) else {
                continue;
            };
            let request = line_through_cell(cell, 40.0);
            for (moment, family) in [
                (
                    MomentType::Reflectivity,
                    color_tables::ColorTableFamily::Reflectivity,
                ),
                (
                    MomentType::Velocity,
                    color_tables::ColorTableFamily::Velocity,
                ),
            ] {
                let prepared = SliceVolume::from_volume(&volume, &moment);
                if prepared.tilt_count() == 0 {
                    continue;
                }
                let policy = if moment == MomentType::Velocity {
                    InterpPolicy::VelocityGuard
                } else {
                    InterpPolicy::LinearAngle
                };
                let Some(slice) =
                    sample_slice(&prepared, &request, policy, SliceSmoothing::Smoothed)
                else {
                    continue;
                };
                let path = out_dir.join(format!(
                    "{}_{}_xsection.png",
                    volume.site.id,
                    moment.short_name()
                ));
                write_slice_png(&slice, tables.for_family(family), &path);
                eprintln!("wrote {}", path.display());
            }

            // Dealiased velocity through the same line — the exact
            // `from_indexed_grids` path the workstation's DVEL product
            // slices, on real folds rather than synthetic ones.
            let dealiased: Vec<Option<MomentGrid>> = volume
                .cuts
                .iter()
                .map(|cut| {
                    cut.moments
                        .get(&MomentType::Velocity)
                        .map(|grid| crate::dealias_velocity_grid(cut, grid))
                })
                .collect();
            let prepared = SliceVolume::from_indexed_grids(
                &volume,
                dealiased
                    .iter()
                    .enumerate()
                    .filter_map(|(index, grid)| grid.as_ref().map(|grid| (index, grid))),
            );
            if prepared.tilt_count() > 0
                && let Some(slice) = sample_slice(
                    &prepared,
                    &request,
                    InterpPolicy::VelocityGuard,
                    SliceSmoothing::Smoothed,
                )
            {
                let path = out_dir.join(format!("{}_DVEL_xsection.png", volume.site.id));
                write_slice_png(
                    &slice,
                    tables.for_family(color_tables::ColorTableFamily::Velocity),
                    &path,
                );
                eprintln!("wrote {}", path.display());
            }
        }
    }

    /// Minimal proof renderer: the slice through the colour table, absent
    /// cells left as dark background, 2 km height lines and 10 km distance
    /// ticks so the geometry is readable.
    fn write_slice_png(slice: &Slice, table: &crate::ColorTable, path: &std::path::Path) {
        let width = slice.width as u32;
        let height = slice.height as u32;
        let mut image = image::RgbaImage::from_pixel(width, height, image::Rgba([13, 16, 21, 255]));
        for y in 0..slice.height {
            for x in 0..slice.width {
                let value = slice.value_at(x, y);
                if !value.is_finite() {
                    continue;
                }
                let color = table.sample(value);
                if color.a == 0 {
                    continue;
                }
                image.put_pixel(
                    x as u32,
                    y as u32,
                    image::Rgba([color.r, color.g, color.b, 255]),
                );
            }
        }
        // 2 km height lines.
        let mut z = 2_000.0f32;
        while z < slice.top_m {
            let y = ((1.0 - z / slice.top_m) * (slice.height - 1) as f32).round() as u32;
            for x in 0..width {
                let pixel = image.get_pixel_mut(x, y.min(height - 1));
                pixel.0 = [
                    pixel.0[0].saturating_add(24),
                    pixel.0[1].saturating_add(24),
                    pixel.0[2].saturating_add(24),
                    255,
                ];
            }
            z += 2_000.0;
        }
        // 10 km distance ticks along the bottom edge.
        let mut d = 10_000.0f32;
        while d < slice.length_m {
            let x = (d / slice.length_m * (slice.width - 1) as f32).round() as u32;
            for y in (height.saturating_sub(6))..height {
                image.put_pixel(x.min(width - 1), y, image::Rgba([200, 205, 210, 255]));
            }
            d += 10_000.0;
        }
        image.save(path).expect("png writes");
    }
}
