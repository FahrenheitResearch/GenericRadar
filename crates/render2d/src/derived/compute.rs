//! Turning a radar volume into one volume-derived field.
//!
//! Every product here is a function of a single column of reflectivity, so one
//! pass over the analysis grid sampling one column per cell produces any of
//! them. Sampling is what costs; reducing a column to a number is arithmetic.
//!
//! The work is spread over grid rows with rayon and must run on a worker. A
//! full-radius kilometre grid is 848 241 cells over as many as fifteen tilts,
//! and doing that on the update thread would stop the application for as long
//! as it took.

use product_engine::registry::DerivedVolumeId;
use product_engine::{CellState, HailEnvironment, PlausibleRange, ProductRegistry};
use radar_core::RadarVolume;
use rayon::prelude::*;

use super::field::ScalarField2D;
use super::grid::{AnalysisGridSpec, DEFAULT_SPACING_KM, GridError, GroundPointKm};
use super::hail;
use super::profile::ColumnSample;
use super::reflectivity::{self, ECHO_TOP_THRESHOLD_DBZ, HAIL_ECHO_TOP_THRESHOLD_DBZ};
use super::sampling::{SamplerError, VolumeSampler};
use super::vil;

/// Why a field could not be computed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComputeError {
    Sampler(SamplerError),
    Grid(GridError),
}

impl std::fmt::Display for ComputeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sampler(error) => write!(formatter, "{error}"),
            Self::Grid(error) => write!(formatter, "analysis grid unavailable: {error:?}"),
        }
    }
}

/// Compute one volume product over the whole analysis grid.
///
/// The grid radius is clipped to how far the selected sweeps actually reach, so
/// a clear-air volume that stops at 100 km does not allocate and then walk a
/// 460 km field of no-coverage to prove nothing is there.
pub fn compute_volume_field(
    volume: &RadarVolume,
    capabilities: &product_engine::VolumeCapabilities,
    product: DerivedVolumeId,
    environment: &HailEnvironment,
    spacing_km: f32,
) -> Result<ScalarField2D, ComputeError> {
    let sampler = VolumeSampler::prepare(volume, capabilities).map_err(ComputeError::Sampler)?;
    let grid = AnalysisGridSpec::new(spacing_km, sampler.max_ground_range_km())
        .map_err(ComputeError::Grid)?;

    let width = grid.width as usize;
    let mut values = vec![0.0_f32; grid.cell_count()];
    let mut states = vec![CellState::NoCoverage; grid.cell_count()];

    values
        .par_chunks_mut(width)
        .zip(states.par_chunks_mut(width))
        .enumerate()
        .for_each(|(row, (value_row, state_row))| {
            // One scratch column per row rather than per cell: the sampler
            // reuses the allocation, and a fresh Vec per cell would dominate.
            let mut column: Vec<ColumnSample> = Vec::with_capacity(sampler.tilt_count());
            for column_index in 0..width {
                let point = grid.cell_center(column_index as u32, row as u32);
                let (value, state) =
                    cell_value(&sampler, volume, point, product, environment, &mut column);
                value_row[column_index] = value;
                state_row[column_index] = state;
            }
        });

    let plausible = plausible_range_for(product);
    Ok(ScalarField2D::new(grid, values, states, plausible))
}

fn plausible_range_for(product: DerivedVolumeId) -> PlausibleRange {
    let id = match product {
        DerivedVolumeId::CompositeReflectivity => "CREF",
        DerivedVolumeId::EchoTop18 => "ET18",
        DerivedVolumeId::Vil => "VIL",
        DerivedVolumeId::VilDensity => "VILD",
        DerivedVolumeId::Mesh => "MESH",
        DerivedVolumeId::ProbabilityOfHail => "POH",
        DerivedVolumeId::ProbabilityOfSevereHail => "POSH",
    };
    ProductRegistry::builtin()
        .get(id)
        .expect("every derived volume product has a registry entry")
        .domain
        .plausible
}

/// Reduce one column to one product's value.
fn cell_value(
    sampler: &VolumeSampler,
    volume: &RadarVolume,
    point: GroundPointKm,
    product: DerivedVolumeId,
    environment: &HailEnvironment,
    column: &mut Vec<ColumnSample>,
) -> (f32, CellState) {
    sampler.sample_column(volume, point, column);
    if column.is_empty() {
        return (0.0, CellState::NoCoverage);
    }

    match product {
        DerivedVolumeId::CompositeReflectivity => reflectivity::composite_reflectivity(column),
        DerivedVolumeId::EchoTop18 => reflectivity::echo_top_m(column, ECHO_TOP_THRESHOLD_DBZ),
        DerivedVolumeId::Vil => vil::vertically_integrated_liquid(column),
        DerivedVolumeId::VilDensity => {
            let (liquid, liquid_state) = vil::vertically_integrated_liquid(column);
            let (top, top_state) = reflectivity::echo_top_m(column, ECHO_TOP_THRESHOLD_DBZ);
            vil::vil_density_kg_m3(liquid, liquid_state, top, top_state)
        }
        DerivedVolumeId::Mesh => {
            let (index, state) = hail::severe_hail_index(column, environment);
            (hail::mesh_mm(index), state)
        }
        DerivedVolumeId::ProbabilityOfHail => {
            // POH compares the 45 dBZ top against the melting level, so it
            // needs its own echo top and not the 18 dBZ one.
            let (top, state) = reflectivity::echo_top_m(column, HAIL_ECHO_TOP_THRESHOLD_DBZ);
            match state {
                CellState::NoEcho => (0.0, CellState::Valid),
                state if state.has_value() => {
                    (hail::probability_of_hail_percent(top, environment), state)
                }
                other => (0.0, other),
            }
        }
        DerivedVolumeId::ProbabilityOfSevereHail => {
            let (index, state) = hail::severe_hail_index(column, environment);
            (hail::posh_percent(index, environment), state)
        }
    }
}

/// The default cell size for an interactive field.
pub const INTERACTIVE_SPACING_KM: f32 = DEFAULT_SPACING_KM;

#[cfg(test)]
mod tests {
    use super::*;
    use product_engine::VolumeCapabilities;
    use radar_core::{
        ElevationCut, GateRange, MomentGrid, MomentRow, MomentType, RadarSite, Radial,
    };

    /// A volume with three tilts, each a full sweep, carrying a uniform
    /// reflectivity out to 100 km.
    fn uniform_volume(dbz: f32) -> RadarVolume {
        // The u8 encoding is value = (raw - offset) / scale with scale 2 and
        // offset 66, so raw = dbz * 2 + 66.
        let raw = (dbz * 2.0 + 66.0).round() as u8;
        let mut volume = RadarVolume::new(RadarSite::new("KTST"), chrono::Utc::now());
        for (number, elevation) in [(1_u8, 0.5_f32), (2, 2.4), (3, 6.0)] {
            let mut cut = ElevationCut::new(elevation, Some(number));
            let gate_range = GateRange {
                first_gate_m: 0,
                gate_spacing_m: 1000,
                gate_count: 100,
            };
            let mut grid = MomentGrid::new_u8(
                MomentType::Reflectivity,
                gate_range.clone(),
                2.0,
                66.0,
                Some(0),
                Some(1),
            );
            for index in 0..360 {
                cut.radials.push(Radial {
                    azimuth_deg: index as f32,
                    elevation_deg: elevation,
                    time_offset_ms: index * 10,
                    gate_range: gate_range.clone(),
                    nyquist_velocity_mps: Some(26.0),
                    radial_status: None,
                });
                grid.push_row(index as usize, MomentRow::U8(vec![raw; 100]))
                    .expect("row fits");
            }
            cut.moments.insert(MomentType::Reflectivity, grid);
            volume.cuts.push(cut);
        }
        volume
    }

    fn compute(product: DerivedVolumeId, dbz: f32) -> ScalarField2D {
        let volume = uniform_volume(dbz);
        let capabilities = VolumeCapabilities::analyze(&volume);
        compute_volume_field(
            &volume,
            &capabilities,
            product,
            &HailEnvironment::climatological_fallback(),
            4.0,
        )
        .expect("a three-tilt volume computes")
    }

    #[test]
    fn a_composite_of_a_uniform_volume_is_that_uniform_value() {
        let field = compute(DerivedVolumeId::CompositeReflectivity, 45.0);
        assert!(
            field.stats.cells_with_values() > 0,
            "a 100 km uniform volume must produce values"
        );
        let max = field.stats.max.expect("values exist");
        assert!(
            (max - 45.0).abs() < 0.6,
            "composite of a uniform 45 dBZ volume was {max}"
        );
    }

    #[test]
    fn a_field_reaches_no_further_than_the_sweeps_that_built_it() {
        // The sweeps stop at 100 km, so the grid must not be a 460 km field of
        // no-coverage cells.
        let field = compute(DerivedVolumeId::CompositeReflectivity, 45.0);
        assert!(
            field.grid.radius_km <= 130.0,
            "grid radius was {} km for a 100 km volume",
            field.grid.radius_km
        );
    }

    #[test]
    fn an_echo_top_field_is_in_metres_and_within_the_volume() {
        let field = compute(DerivedVolumeId::EchoTop18, 45.0);
        let max = field.stats.max.expect("values exist");
        assert!(
            (100.0..30_000.0).contains(&max),
            "echo top max was {max}; a value near 40 would mean kilofeet"
        );
        assert!(!field.plausibility.is_rejected());
    }

    #[test]
    fn a_vil_field_is_kilograms_per_square_metre_and_survives_its_plausibility_gate() {
        let field = compute(DerivedVolumeId::Vil, 45.0);
        assert!(!field.plausibility.is_rejected());
        let max = field.stats.max.expect("values exist");
        assert!(max > 0.0 && max < 250.0, "VIL max was {max}");
    }

    #[test]
    fn a_probability_field_never_leaves_nought_to_one_hundred() {
        for product in [
            DerivedVolumeId::ProbabilityOfHail,
            DerivedVolumeId::ProbabilityOfSevereHail,
        ] {
            let field = compute(product, 60.0);
            assert!(
                !field.plausibility.is_rejected(),
                "{product:?} was rejected"
            );
            if let (Some(min), Some(max)) = (field.stats.min, field.stats.max) {
                assert!(
                    (0.0..=100.0).contains(&min) && (0.0..=100.0).contains(&max),
                    "{product:?} spanned {min} to {max}"
                );
            }
        }
    }

    #[test]
    fn a_volume_with_no_reflectivity_reports_an_error_rather_than_an_empty_field() {
        let mut volume = uniform_volume(45.0);
        for cut in &mut volume.cuts {
            cut.moments.clear();
        }
        let capabilities = VolumeCapabilities::analyze(&volume);
        let error = compute_volume_field(
            &volume,
            &capabilities,
            DerivedVolumeId::CompositeReflectivity,
            &HailEnvironment::climatological_fallback(),
            4.0,
        )
        .expect_err("a volume with no reflectivity cannot make a composite");
        assert!(matches!(error, ComputeError::Sampler(_)));
    }

    #[test]
    fn every_derived_product_has_a_plausible_range_from_the_registry() {
        for product in [
            DerivedVolumeId::CompositeReflectivity,
            DerivedVolumeId::EchoTop18,
            DerivedVolumeId::Vil,
            DerivedVolumeId::VilDensity,
            DerivedVolumeId::Mesh,
            DerivedVolumeId::ProbabilityOfHail,
            DerivedVolumeId::ProbabilityOfSevereHail,
        ] {
            let range = plausible_range_for(product);
            assert!(
                range.is_well_ordered(),
                "{product:?} has an unusable plausible range"
            );
        }
    }
}
