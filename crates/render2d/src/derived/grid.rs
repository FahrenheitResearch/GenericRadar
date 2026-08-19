//! The Cartesian analysis grid that volume-derived products are computed on.
//!
//! Sweep products live in the radar's native polar geometry, and that is where
//! they should stay: a gate is a real measurement with a real provenance.
//! Volume products cannot, because they combine gates from tilts whose beams
//! cross each other on the way up. They need a common frame, and that frame is
//! a square, radar-centred, ground-referenced grid.
//!
//! The grid is camera-independent on purpose. Panning must never recompute a
//! field, and two panes looking at the same volume through different cameras
//! must share one allocation. So nothing here knows about a viewport, a zoom,
//! or a screen; the only inputs are the radar's own coverage and a spacing.

/// A point on the ground relative to the radar, in kilometres.
///
/// East and north rather than latitude and longitude: the whole derived path
/// works in the radar's local frame, and `render2d` deliberately does not
/// depend on the map or projection crates. The composition layer converts at
/// the boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundPointKm {
    pub east_km: f64,
    pub north_km: f64,
}

impl GroundPointKm {
    pub const ORIGIN: Self = Self {
        east_km: 0.0,
        north_km: 0.0,
    };

    pub const fn new(east_km: f64, north_km: f64) -> Self {
        Self { east_km, north_km }
    }

    /// Ground distance from the radar, in kilometres.
    pub fn range_km(self) -> f64 {
        self.east_km.hypot(self.north_km)
    }
}

/// The largest analysis radius worth building, in kilometres. NEXRAD's longest
/// surveillance sweep reaches about 460 km, so nothing beyond this can hold a
/// measurement.
pub const MAX_ANALYSIS_RADIUS_KM: f32 = 460.0;

/// The default cell size. One kilometre is finer than the beam is wide at any
/// useful range, and coarse enough that a full-radius grid stays under 5 MiB.
pub const DEFAULT_SPACING_KM: f32 = 1.0;

/// Why a requested grid could not be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridError {
    /// A spacing of zero or less would divide by zero; a non-finite one would
    /// produce a grid of unknowable size.
    InvalidSpacing,
    /// A radius of zero or less has no cells in it.
    InvalidRadius,
    /// The requested grid would not fit in memory. Checked rather than trusted:
    /// a 460 km radius at 10 m spacing is 8.5 billion cells, and allocating it
    /// would take the process down rather than report a problem.
    TooLarge { cells: u64 },
}

/// The most cells a single analysis field may hold.
///
/// Four million is comfortably above the 850 000 a full-radius kilometre grid
/// needs, and far below anything that would exhaust memory.
const MAX_CELLS: u64 = 4_000_000;

/// The geometry of one analysis grid.
///
/// Always square, always odd, always centred exactly on the radar. Odd because
/// a centre cell that *is* the radar is easier to reason about than one whose
/// corner is; a field that is symmetric about the radar should look it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnalysisGridSpec {
    pub spacing_km: f32,
    pub radius_km: f32,
    pub width: u32,
    pub height: u32,
}

impl AnalysisGridSpec {
    /// Build a grid covering `radius_km` at `spacing_km`.
    ///
    /// The radius is clamped to the coverage the volume actually has, so a
    /// clear-air volume that reaches 100 km does not allocate a 460 km field
    /// of `NoCoverage` to prove that nothing is there.
    pub fn new(spacing_km: f32, radius_km: f32) -> Result<Self, GridError> {
        if !spacing_km.is_finite() || spacing_km <= 0.0 {
            return Err(GridError::InvalidSpacing);
        }
        if !radius_km.is_finite() || radius_km <= 0.0 {
            return Err(GridError::InvalidRadius);
        }
        let radius_km = radius_km.min(MAX_ANALYSIS_RADIUS_KM);
        let half_cells = (radius_km / spacing_km).ceil() as u64;
        let side = 2 * half_cells + 1;
        let cells = side.saturating_mul(side);
        if cells > MAX_CELLS {
            return Err(GridError::TooLarge { cells });
        }
        let side = side as u32;
        Ok(Self {
            spacing_km,
            radius_km,
            width: side,
            height: side,
        })
    }

    pub fn cell_count(self) -> usize {
        self.width as usize * self.height as usize
    }

    /// Bytes one field on this grid occupies: four for the value, one for the
    /// state. Used by the cache to account for itself honestly rather than
    /// guessing from the cell count.
    pub fn resident_bytes(self) -> usize {
        self.cell_count() * (size_of::<f32>() + size_of::<u8>())
    }

    /// The index of the centre cell along one axis.
    pub fn center_index(self) -> u32 {
        self.width / 2
    }

    /// The ground position of a cell centre.
    ///
    /// North increases upward in the grid, so the row index runs the opposite
    /// way to north. Getting this backwards flips every field vertically, which
    /// on a nearly symmetric storm is almost invisible.
    pub fn cell_center(self, column: u32, row: u32) -> GroundPointKm {
        let center = f64::from(self.center_index());
        let spacing = f64::from(self.spacing_km);
        GroundPointKm {
            east_km: (f64::from(column) - center) * spacing,
            north_km: (center - f64::from(row)) * spacing,
        }
    }

    /// Row-major index of a cell.
    pub fn index_of(self, column: u32, row: u32) -> Option<usize> {
        (column < self.width && row < self.height)
            .then(|| row as usize * self.width as usize + column as usize)
    }

    /// The cell containing a ground point, or `None` when it falls outside.
    pub fn cell_at(self, point: GroundPointKm) -> Option<(u32, u32)> {
        let spacing = f64::from(self.spacing_km);
        let center = f64::from(self.center_index());
        let column = (point.east_km / spacing + center).round();
        let row = (center - point.north_km / spacing).round();
        if column < 0.0 || row < 0.0 {
            return None;
        }
        let (column, row) = (column as u32, row as u32);
        (column < self.width && row < self.height).then_some((column, row))
    }

    /// A key that identifies this geometry for a cache. Two grids that differ
    /// in any dimension must not share a cached field.
    pub fn key(self) -> u64 {
        let mut key = u64::from(self.width) << 32;
        key |= u64::from(self.height);
        key ^= u64::from(self.spacing_km.to_bits()) << 16;
        key ^ u64::from(self.radius_km.to_bits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_radius_kilometre_grid_is_nine_hundred_and_twenty_one_cells_square() {
        let grid = AnalysisGridSpec::new(1.0, 460.0).expect("a standard grid must build");
        assert_eq!(grid.width, 921);
        assert_eq!(grid.height, 921);
        assert_eq!(grid.cell_count(), 848_241);
    }

    #[test]
    fn a_full_radius_kilometre_grid_costs_about_four_megabytes() {
        let grid = AnalysisGridSpec::new(1.0, 460.0).expect("a standard grid must build");
        let mebibytes = grid.resident_bytes() as f64 / (1024.0 * 1024.0);
        assert!(
            (4.0..5.0).contains(&mebibytes),
            "a full grid should cost about 4 MiB, got {mebibytes}"
        );
    }

    #[test]
    fn the_grid_is_always_odd_so_one_cell_is_the_radar() {
        for radius in [10.0, 99.5, 230.0, 460.0] {
            let grid = AnalysisGridSpec::new(1.0, radius).expect("must build");
            assert_eq!(grid.width % 2, 1, "radius {radius} produced an even grid");
        }
    }

    #[test]
    fn the_centre_cell_sits_exactly_on_the_radar() {
        let grid = AnalysisGridSpec::new(1.0, 100.0).expect("must build");
        let center = grid.center_index();
        assert_eq!(grid.cell_center(center, center), GroundPointKm::ORIGIN);
    }

    /// North is up. A row *above* the centre must be north of the radar, not
    /// south of it. Getting this backwards mirrors every volume product.
    #[test]
    fn a_row_above_the_centre_is_north_of_the_radar() {
        let grid = AnalysisGridSpec::new(1.0, 100.0).expect("must build");
        let center = grid.center_index();
        let above = grid.cell_center(center, center - 10);
        assert_eq!(above.north_km, 10.0);
        assert_eq!(above.east_km, 0.0);
    }

    #[test]
    fn a_column_right_of_the_centre_is_east_of_the_radar() {
        let grid = AnalysisGridSpec::new(1.0, 100.0).expect("must build");
        let center = grid.center_index();
        let right = grid.cell_center(center + 10, center);
        assert_eq!(right.east_km, 10.0);
        assert_eq!(right.north_km, 0.0);
    }

    #[test]
    fn a_ground_point_round_trips_through_its_cell() {
        let grid = AnalysisGridSpec::new(2.0, 200.0).expect("must build");
        let point = GroundPointKm::new(-46.0, 88.0);
        let (column, row) = grid.cell_at(point).expect("point is inside the grid");
        assert_eq!(grid.cell_center(column, row), point);
    }

    #[test]
    fn a_point_outside_the_grid_has_no_cell() {
        let grid = AnalysisGridSpec::new(1.0, 50.0).expect("must build");
        assert_eq!(grid.cell_at(GroundPointKm::new(400.0, 0.0)), None);
        assert_eq!(grid.cell_at(GroundPointKm::new(0.0, -400.0)), None);
    }

    #[test]
    fn a_spacing_of_zero_is_refused_rather_than_dividing_by_it() {
        assert_eq!(
            AnalysisGridSpec::new(0.0, 100.0),
            Err(GridError::InvalidSpacing)
        );
        assert_eq!(
            AnalysisGridSpec::new(f32::NAN, 100.0),
            Err(GridError::InvalidSpacing)
        );
    }

    #[test]
    fn an_empty_radius_is_refused() {
        assert_eq!(
            AnalysisGridSpec::new(1.0, 0.0),
            Err(GridError::InvalidRadius)
        );
    }

    #[test]
    fn an_unreasonably_fine_grid_is_refused_rather_than_allocated() {
        // 460 km at 10 m spacing is 8.5 billion cells. Trying it would end the
        // process rather than report a problem.
        let error = AnalysisGridSpec::new(0.01, 460.0).expect_err("must refuse");
        assert!(matches!(error, GridError::TooLarge { .. }));
    }

    #[test]
    fn a_radius_beyond_the_longest_sweep_is_clamped_not_refused() {
        let grid = AnalysisGridSpec::new(1.0, 5_000.0).expect("must clamp and build");
        assert_eq!(grid.radius_km, MAX_ANALYSIS_RADIUS_KM);
    }

    #[test]
    fn grids_that_differ_in_any_dimension_have_different_cache_keys() {
        let base = AnalysisGridSpec::new(1.0, 230.0).expect("must build");
        let finer = AnalysisGridSpec::new(0.5, 230.0).expect("must build");
        let wider = AnalysisGridSpec::new(1.0, 300.0).expect("must build");
        assert_ne!(base.key(), finer.key());
        assert_ne!(base.key(), wider.key());
        assert_eq!(base.key(), AnalysisGridSpec::new(1.0, 230.0).unwrap().key());
    }

    #[test]
    fn row_major_indexing_walks_east_before_north() {
        let grid = AnalysisGridSpec::new(1.0, 2.0).expect("must build");
        assert_eq!(grid.width, 5);
        assert_eq!(grid.index_of(0, 0), Some(0));
        assert_eq!(grid.index_of(4, 0), Some(4));
        assert_eq!(grid.index_of(0, 1), Some(5));
        assert_eq!(grid.index_of(5, 0), None);
        assert_eq!(grid.index_of(0, 5), None);
    }
}
