//! A computed volume field, and how it becomes pixels.
//!
//! Sweep products are drawn straight from their native polar grid by the
//! existing raster path. Volume products cannot be: they are a function of
//! several tilts at once and live on the Cartesian analysis grid instead. This
//! module holds that field and rasterises it.
//!
//! The field itself is camera-independent, so a pan, a zoom, or a palette
//! change repaints from the same allocation and never recomputes.

use color_tables::ColorTable;
use product_engine::{CellState, FieldStats, PlausibilityReport, PlausibleRange, summarize};

use super::grid::{AnalysisGridSpec, GroundPointKm};

/// One computed volume product over one analysis grid.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarField2D {
    pub grid: AnalysisGridSpec,
    /// Row-major, one per cell. Meaningful only where the state says so.
    pub values: Vec<f32>,
    pub states: Vec<CellState>,
    pub stats: FieldStats,
    pub plausibility: PlausibilityReport,
}

impl ScalarField2D {
    /// Build a field from raw cells, counting and judging it in one pass.
    pub fn new(
        grid: AnalysisGridSpec,
        values: Vec<f32>,
        states: Vec<CellState>,
        plausible: PlausibleRange,
    ) -> Self {
        let (stats, plausibility) = summarize(&values, &states, plausible);
        Self {
            grid,
            values,
            states,
            stats,
            plausibility,
        }
    }

    /// The value and state at a ground point, or `None` outside the grid.
    ///
    /// Nearest cell rather than bilinear interpolation. Interpolating between a
    /// valid cell and a no-coverage cell would invent a value where nothing was
    /// sampled, and interpolating across the states honestly is a lot of
    /// machinery for a field whose cells are already a kilometre wide.
    pub fn sample(&self, point: GroundPointKm) -> Option<(f32, CellState)> {
        let (column, row) = self.grid.cell_at(point)?;
        let index = self.grid.index_of(column, row)?;
        Some((self.values[index], self.states[index]))
    }

    pub fn resident_bytes(&self) -> usize {
        // Capacity rather than length, because that is what was actually taken
        // from the allocator and what the history budget measures.
        self.values.capacity() * size_of::<f32>() + self.states.capacity() * size_of::<CellState>()
    }
}

/// Where the viewport is looking, in radar-local kilometres per pixel.
///
/// Deliberately the same shape as the polar path's `ViewportRasterOptions` so a
/// caller can drive both from one camera without a second conversion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldRasterOptions {
    pub width: u32,
    pub height: u32,
    pub radar_x_px: f32,
    pub radar_y_px: f32,
    pub km_per_px_x: f32,
    pub km_per_px_y: f32,
    /// Clockwise screen rotation of the camera, radians, exactly as on
    /// [`crate::ViewportRasterOptions`].
    ///
    /// The polar path's azimuth shortcut does NOT apply here: a derived field
    /// is sampled on a Cartesian analysis grid, not on a polar lattice, so the
    /// sample coordinate takes the full two-by-two rotation.
    pub rotation_rad: f32,
}

/// Paint a field into an RGBA buffer.
///
/// Returns the dimensions written. Cells without a value are left fully
/// transparent, which is the same convention the polar path uses - and, as
/// there, means the pixels cannot afterwards be read back to decide whether
/// data existed. The field's `states` are the authority on that, not the image.
pub fn render_field_rgba_into(
    field: &ScalarField2D,
    table: &ColorTable,
    options: FieldRasterOptions,
    rgba: &mut [u8],
) -> (u32, u32) {
    let expected = field_rgba_buffer_len(options);
    assert_eq!(
        rgba.len(),
        expected,
        "field raster buffer is {} bytes, expected {expected}",
        rgba.len()
    );
    rgba.fill(0);

    let folded = table.range_folded_rgba();
    // The inverse of the camera's screen rotation, applied once per raster
    // rather than derived per pixel. `Camera2D::screen_to_world` uses the
    // reflection `(cos, sin; sin, -cos)` on a screen offset whose y grows
    // DOWNWARD; the offsets below are stated with north positive, which flips
    // the sign of the second column and leaves the plain rotation written
    // here. At zero rotation `sin` is 0 and `cos` is 1, so both expressions
    // collapse to the two the shipped code wrote.
    let rotation_rad = if options.rotation_rad.is_finite() {
        options.rotation_rad
    } else {
        0.0
    };
    let (sin, cos) = rotation_rad.sin_cos();
    for y in 0..options.height {
        for x in 0..options.width {
            // Pixel centres, matching the polar viewport path. +y is south.
            let screen_east_km = (x as f32 + 0.5 - options.radar_x_px) * options.km_per_px_x;
            let screen_north_km = (options.radar_y_px - (y as f32 + 0.5)) * options.km_per_px_y;
            let east_km = f64::from(cos * screen_east_km - sin * screen_north_km);
            let north_km = f64::from(sin * screen_east_km + cos * screen_north_km);
            let Some((value, state)) = field.sample(GroundPointKm::new(east_km, north_km)) else {
                continue;
            };
            let colour = match state {
                CellState::RangeFolded => folded,
                state if state.has_value() => table.sample(value),
                _ => continue,
            };
            let offset = ((y as usize * options.width as usize) + x as usize) * 4;
            rgba[offset] = colour.r;
            rgba[offset + 1] = colour.g;
            rgba[offset + 2] = colour.b;
            rgba[offset + 3] = colour.a;
        }
    }
    (options.width, options.height)
}

pub fn field_rgba_buffer_len(options: FieldRasterOptions) -> usize {
    options.width as usize * options.height as usize * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_tables::builtin_reflectivity_table;

    fn tiny_grid() -> AnalysisGridSpec {
        AnalysisGridSpec::new(1.0, 2.0).expect("a 5x5 grid must build")
    }

    fn uniform_field(value: f32, state: CellState) -> ScalarField2D {
        let grid = tiny_grid();
        let cells = grid.cell_count();
        ScalarField2D::new(
            grid,
            vec![value; cells],
            vec![state; cells],
            PlausibleRange::new(-35.0, 85.0, -40.0, 100.0),
        )
    }

    #[test]
    fn a_field_counts_and_judges_itself_when_it_is_built() {
        let field = uniform_field(45.0, CellState::Valid);
        assert_eq!(field.stats.cells_total, 25);
        assert_eq!(field.stats.cells_valid, 25);
        assert_eq!(field.stats.max, Some(45.0));
        assert!(!field.plausibility.is_rejected());
    }

    #[test]
    fn a_field_holding_an_impossible_value_is_rejected_at_construction() {
        // 400 dBZ is a decode or unit fault, and the field must be refused
        // before it can be cached or uploaded, not after someone looks at it.
        let field = uniform_field(400.0, CellState::Valid);
        assert!(field.plausibility.is_rejected());
    }

    #[test]
    fn sampling_the_centre_cell_reads_the_radar_position() {
        let field = uniform_field(30.0, CellState::Valid);
        assert_eq!(
            field.sample(GroundPointKm::ORIGIN),
            Some((30.0, CellState::Valid))
        );
    }

    #[test]
    fn sampling_outside_the_grid_reads_nothing_rather_than_the_edge() {
        let field = uniform_field(30.0, CellState::Valid);
        assert_eq!(field.sample(GroundPointKm::new(500.0, 0.0)), None);
    }

    #[test]
    fn a_cell_with_no_coverage_paints_nothing() {
        let field = uniform_field(0.0, CellState::NoCoverage);
        let options = FieldRasterOptions {
            width: 8,
            height: 8,
            radar_x_px: 4.0,
            radar_y_px: 4.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        };
        let mut rgba = vec![255_u8; field_rgba_buffer_len(options)];
        render_field_rgba_into(&field, &builtin_reflectivity_table(), options, &mut rgba);
        assert!(
            rgba.chunks_exact(4).all(|pixel| pixel[3] == 0),
            "an unsampled field must paint nothing at all"
        );
    }

    #[test]
    fn a_valid_cell_paints_its_palette_colour() {
        let field = uniform_field(50.0, CellState::Valid);
        let table = builtin_reflectivity_table();
        let options = FieldRasterOptions {
            width: 4,
            height: 4,
            radar_x_px: 2.0,
            radar_y_px: 2.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        };
        let mut rgba = vec![0_u8; field_rgba_buffer_len(options)];
        render_field_rgba_into(&field, &table, options, &mut rgba);
        let expected = table.sample(50.0);
        assert_eq!(
            &rgba[0..4],
            &[expected.r, expected.g, expected.b, expected.a]
        );
    }

    #[test]
    fn a_range_folded_cell_paints_the_folded_colour_and_not_a_value() {
        let field = uniform_field(50.0, CellState::RangeFolded);
        let table = builtin_reflectivity_table();
        let options = FieldRasterOptions {
            width: 2,
            height: 2,
            radar_x_px: 1.0,
            radar_y_px: 1.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        };
        let mut rgba = vec![0_u8; field_rgba_buffer_len(options)];
        render_field_rgba_into(&field, &table, options, &mut rgba);
        let folded = table.range_folded_rgba();
        assert_eq!(&rgba[0..4], &[folded.r, folded.g, folded.b, folded.a]);
    }

    /// North must be up. Painting a field upside down is invisible on a
    /// symmetric storm and obvious only once someone compares it with the map.
    #[test]
    fn the_north_half_of_the_image_reads_the_north_half_of_the_field() {
        let grid = tiny_grid();
        let cells = grid.cell_count();
        let mut values = vec![0.0_f32; cells];
        let mut states = vec![CellState::NoCoverage; cells];
        // One cell, 2 km north of the radar.
        let (column, row) = grid
            .cell_at(GroundPointKm::new(0.0, 2.0))
            .expect("inside the grid");
        let index = grid.index_of(column, row).expect("inside the grid");
        values[index] = 60.0;
        states[index] = CellState::Valid;
        let field = ScalarField2D::new(
            grid,
            values,
            states,
            PlausibleRange::new(-35.0, 85.0, -40.0, 100.0),
        );

        let options = FieldRasterOptions {
            width: 5,
            height: 5,
            radar_x_px: 2.5,
            radar_y_px: 2.5,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
            rotation_rad: 0.0,
        };
        let mut rgba = vec![0_u8; field_rgba_buffer_len(options)];
        render_field_rgba_into(&field, &builtin_reflectivity_table(), options, &mut rgba);

        let opaque_rows: Vec<u32> = (0..options.height)
            .filter(|y| {
                (0..options.width).any(|x| {
                    rgba[((*y as usize * options.width as usize) + x as usize) * 4 + 3] > 0
                })
            })
            .collect();
        assert_eq!(
            opaque_rows,
            vec![0],
            "an echo 2 km north must paint in the top row of the image"
        );
    }

    #[test]
    fn a_field_reports_the_bytes_it_actually_took_from_the_allocator() {
        let field = uniform_field(30.0, CellState::Valid);
        assert_eq!(field.resident_bytes(), 25 * 4 + 25);
    }
}
