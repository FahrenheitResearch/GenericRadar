//! The vertical profile a volume product is computed from.
//!
//! Every volume-derived product - composite reflectivity, echo tops, VIL, and
//! the whole hail family - is a function of one column of reflectivity samples
//! taken above one ground point. Sampling that column is expensive and shared;
//! reducing it to a number is cheap and product-specific. Splitting them here
//! means each algorithm is a pure function over a slice, testable against a
//! hand-written column with no radar volume in sight.
//!
//! Two invariants hold for every profile handed to an algorithm, and the
//! algorithms are entitled to rely on them:
//!
//! 1. Entries are sorted by ascending beam-centre height. **Not** by cut index:
//!    a volume's cuts are not in height order at a given point, because a lower
//!    tilt at long range is higher than a steeper tilt at short range, and on a
//!    SAILS volume the cut list is not even in elevation order.
//! 2. At most one entry per nominal elevation. A split cut's two legs and a
//!    SAILS volume's repeated low tilts are the *same beam sampled twice*.
//!    Integrating both would invent vertical depth that is not there - a
//!    2 km-deep storm scanned three times at 0.5 degrees would report three
//!    separate layers of liquid water.

use product_engine::CellState;

/// Two elevations closer than this are the same tilt. Tighter than the
/// split-cut grouping tolerance, because by the time a column is built the
/// caller has already grouped legs; this only catches an outright repeat.
const NOMINAL_ELEVATION_EPSILON: f32 = 0.01;

/// One tilt's contribution to a column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnSample {
    /// Index of the cut this came from, for provenance in a probe readout.
    pub cut_index: usize,
    /// The cut's nominal elevation, in degrees.
    pub elevation_deg: f32,
    /// Beam-centre height above the antenna, from the 4/3-earth model.
    pub height_arl_m: f32,
    /// True slant range to the gate centre.
    pub slant_range_m: f32,
    /// Reflectivity in dBZ. Meaningful only when `state.has_value()`; for any
    /// other state this holds whatever the sampler last wrote and reading it
    /// is a bug.
    pub reflectivity_dbz: f32,
    pub state: CellState,
}

impl ColumnSample {
    /// The reflectivity, if this sample actually has one.
    pub fn value(&self) -> Option<f32> {
        self.state.has_value().then_some(self.reflectivity_dbz)
    }

    /// Whether a beam reached this point at all, whatever it found.
    ///
    /// The distinction that matters for vertical integration: a gap between two
    /// samples that were both *covered* may be integrated across, because the
    /// radar looked in between and the profile is continuous. A gap where the
    /// radar did not look may not, because anything could be in it.
    pub fn is_covered(&self) -> bool {
        !matches!(self.state, CellState::NoCoverage)
    }
}

/// Sort a column into ascending height order and confirm it holds one entry per
/// nominal elevation.
///
/// Returns the number of duplicate nominal elevations dropped, which the caller
/// records in provenance. Duplicates are dropped keeping the sample from the
/// cut the caller listed first, because the caller has already chosen a
/// representative leg per nominal elevation and any duplicate reaching here is
/// a defect in that selection rather than a choice to make again.
pub fn normalize_column(samples: &mut Vec<ColumnSample>) -> usize {
    samples.sort_by(|left, right| {
        left.height_arl_m
            .total_cmp(&right.height_arl_m)
            .then_with(|| left.cut_index.cmp(&right.cut_index))
    });
    let before = samples.len();
    let mut seen: Vec<f32> = Vec::with_capacity(before);
    samples.retain(|sample| {
        let duplicate = seen
            .iter()
            .any(|elevation| (elevation - sample.elevation_deg).abs() < NOMINAL_ELEVATION_EPSILON);
        if !duplicate {
            seen.push(sample.elevation_deg);
        }
        !duplicate
    });
    before - samples.len()
}

/// Linear reflectivity factor, in mm^6 m^-3, from dBZ.
///
/// `Z = 10^(dBZ/10)`. Vertical integration must happen in linear Z: averaging
/// two reflectivities in dBZ and converting once is not the same as converting
/// both and averaging, and the difference across a 20 dBZ gradient is a factor
/// of several.
pub fn linear_z_from_dbz(dbz: f32) -> f32 {
    10.0_f32.powf(dbz / 10.0)
}

/// dBZ from linear reflectivity factor. Zero and negative Z have no logarithm,
/// and answer `None` rather than negative infinity.
pub fn dbz_from_linear_z(linear_z: f32) -> Option<f32> {
    (linear_z > 0.0).then(|| 10.0 * linear_z.log10())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cut_index: usize, elevation_deg: f32, height_arl_m: f32, dbz: f32) -> ColumnSample {
        ColumnSample {
            cut_index,
            elevation_deg,
            height_arl_m,
            slant_range_m: 50_000.0,
            reflectivity_dbz: dbz,
            state: CellState::Valid,
        }
    }

    #[test]
    fn a_column_is_sorted_by_height_not_by_cut_index() {
        // Cut order is not height order: this is the whole reason the helper
        // exists. On a SAILS volume the cut list is not even elevation-ordered.
        let mut column = vec![
            sample(5, 4.0, 9_000.0, 20.0),
            sample(0, 0.5, 1_000.0, 50.0),
            sample(3, 2.0, 5_000.0, 35.0),
        ];
        assert_eq!(normalize_column(&mut column), 0);
        let heights: Vec<f32> = column.iter().map(|s| s.height_arl_m).collect();
        assert_eq!(heights, [1_000.0, 5_000.0, 9_000.0]);
    }

    #[test]
    fn a_repeated_nominal_elevation_is_dropped_so_depth_is_not_invented() {
        // Three 0.5-degree scans of a SAILS volume are one beam sampled three
        // times, not three layers of atmosphere.
        let mut column = vec![
            sample(0, 0.5, 1_000.0, 50.0),
            sample(7, 0.5, 1_000.0, 48.0),
            sample(13, 0.5, 1_000.0, 52.0),
            sample(3, 2.0, 5_000.0, 35.0),
        ];
        assert_eq!(normalize_column(&mut column), 2);
        assert_eq!(column.len(), 2);
        assert_eq!(column[0].cut_index, 0, "the first listed leg is kept");
        assert_eq!(column[1].elevation_deg, 2.0);
    }

    #[test]
    fn only_states_with_values_report_a_reflectivity() {
        let mut covered = sample(0, 0.5, 1_000.0, 50.0);
        assert_eq!(covered.value(), Some(50.0));
        covered.state = CellState::NoEcho;
        assert_eq!(covered.value(), None);
        assert!(covered.is_covered(), "a no-echo gate was still sampled");
        covered.state = CellState::NoCoverage;
        assert!(!covered.is_covered());
    }

    #[test]
    fn fifty_dbz_is_one_hundred_thousand_in_linear_units() {
        let linear = linear_z_from_dbz(50.0);
        assert!((linear - 100_000.0).abs() < 1.0, "50 dBZ was {linear}");
        let back = dbz_from_linear_z(linear).expect("positive Z has a logarithm");
        assert!((back - 50.0).abs() < 1e-3);
    }

    #[test]
    fn zero_dbz_is_unity_linear_reflectivity() {
        assert!((linear_z_from_dbz(0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn zero_linear_reflectivity_has_no_decibel_value() {
        assert_eq!(dbz_from_linear_z(0.0), None);
        assert_eq!(dbz_from_linear_z(-1.0), None);
    }

    #[test]
    fn averaging_in_linear_units_is_not_averaging_in_decibels() {
        // 30 and 50 dBZ average to 40 dBZ in log space but to 47.0 dBZ in
        // linear space. Integrating in the wrong one understates every strong
        // core, which is exactly where VIL matters.
        let linear_mean = (linear_z_from_dbz(30.0) + linear_z_from_dbz(50.0)) / 2.0;
        let in_dbz = dbz_from_linear_z(linear_mean).expect("positive");
        assert!(
            (in_dbz - 47.0).abs() < 0.1,
            "linear mean of 30 and 50 dBZ was {in_dbz} dBZ, not the naive 40"
        );
    }
}
