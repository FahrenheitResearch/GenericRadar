//! Persisting colour table choices, and restoring them defensively.
//!
//! A palette is stored as its **base name** plus its **rendering** - the two
//! halves `color_tables` split a table into when rendering became a property
//! (commit 18af957): `base_name()` is stable across the smooth/stepped
//! switch, and the rendering is one word. Restoring resolves the name
//! through the shipped catalog for that family; an unknown name - a palette
//! from a build that no longer ships it, a hand-edited file - falls back to
//! the family default. **Never** to nothing: a stale settings file must not
//! blank a pane.

use std::collections::BTreeMap;

use color_tables::{ColorTable, ColorTableFamily, ColorTableSet, TableRendering};
use settings::PaletteChoice;

/// Stable stored identifier per family. Same contract as setting ids: these
/// name choices in files already written, never reuse one.
pub fn family_id(family: ColorTableFamily) -> &'static str {
    match family {
        ColorTableFamily::Reflectivity => "reflectivity",
        ColorTableFamily::Velocity => "velocity",
        ColorTableFamily::SpectrumWidth => "spectrum_width",
        ColorTableFamily::DifferentialReflectivity => "differential_reflectivity",
        ColorTableFamily::CorrelationCoefficient => "correlation_coefficient",
        ColorTableFamily::DifferentialPhase => "differential_phase",
        ColorTableFamily::SpecificDifferentialPhase => "specific_differential_phase",
        ColorTableFamily::Generic => "generic",
    }
}

pub fn family_from_id(id: &str) -> Option<ColorTableFamily> {
    ColorTableFamily::ALL
        .into_iter()
        .find(|family| family_id(*family) == id)
}

pub fn rendering_id(rendering: TableRendering) -> &'static str {
    match rendering {
        TableRendering::Smooth => "smooth",
        TableRendering::Stepped => "stepped",
    }
}

/// Unknown strings fall back to Smooth, the shipped default for every family.
pub fn rendering_from_id(id: &str) -> TableRendering {
    match id {
        "stepped" => TableRendering::Stepped,
        _ => TableRendering::Smooth,
    }
}

/// What the analyst has installed, as the snapshot the store persists.
pub fn capture_palettes(tables: &ColorTableSet) -> BTreeMap<String, PaletteChoice> {
    let mut choices = BTreeMap::new();
    for family in ColorTableFamily::ALL {
        let table = tables.for_family(family);
        choices.insert(
            family_id(family).to_owned(),
            PaletteChoice {
                name: table.base_name().to_owned(),
                rendering: rendering_id(table.rendering()).to_owned(),
                ..Default::default()
            },
        );
    }
    choices
}

/// Resolve one family's stored choice against the shipped catalog.
///
/// The name is matched by `base_name` so a file written while stepped finds
/// the same palette as one written while smooth; the stored rendering is
/// then applied to whatever was found. An unknown name keeps the family's
/// default palette (in the stored rendering, which was understood even if
/// the name was not).
fn resolve_choice(family: ColorTableFamily, choice: &PaletteChoice) -> ColorTable {
    let rendering = rendering_from_id(&choice.rendering);
    let catalog = color_tables::builtin_tables_for_family(family);
    let default = ColorTableSet::default();
    let base = catalog
        .into_iter()
        .find(|table| table.base_name() == choice.name)
        .unwrap_or_else(|| default.for_family(family).clone());
    base.rendered(rendering)
}

/// Rebuild a full table set from the snapshot. Families the snapshot does
/// not mention keep their defaults.
pub fn apply_palettes(choices: &BTreeMap<String, PaletteChoice>) -> ColorTableSet {
    let mut tables = ColorTableSet::default();
    for (id, choice) in choices {
        let Some(family) = family_from_id(id) else {
            // A family this build does not know - a future build's. The store
            // carries it forward; there is nothing to install it into.
            continue;
        };
        tables.set_family(family, resolve_choice(family, choice));
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_id_round_trips_and_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for family in ColorTableFamily::ALL {
            let id = family_id(family);
            assert!(seen.insert(id), "duplicate family id {id:?}");
            assert_eq!(family_from_id(id), Some(family));
        }
    }

    #[test]
    fn a_real_installed_set_survives_capture_and_apply_exactly() {
        // The real catalog, not fixtures: install a non-default palette in a
        // non-default rendering for two families and round-trip the whole set.
        let mut tables = ColorTableSet::default();
        let velocity_pick = color_tables::builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .nth(2)
            .expect("the velocity catalog ships more than two palettes")
            .rendered(TableRendering::Stepped);
        tables.set_family(ColorTableFamily::Velocity, velocity_pick.clone());
        let reflectivity_pick =
            color_tables::builtin_tables_for_family(ColorTableFamily::Reflectivity)
                .into_iter()
                .nth(1)
                .expect("the reflectivity catalog ships more than one palette");
        tables.set_family(ColorTableFamily::Reflectivity, reflectivity_pick.clone());

        let restored = apply_palettes(&capture_palettes(&tables));
        for family in ColorTableFamily::ALL {
            assert_eq!(
                restored.for_family(family),
                tables.for_family(family),
                "family {family:?} did not survive the round trip"
            );
        }
    }

    #[test]
    fn an_unknown_palette_name_falls_back_to_the_family_default_never_to_nothing() {
        let mut choices = BTreeMap::new();
        choices.insert(
            "velocity".to_owned(),
            PaletteChoice {
                name: "A Palette This Build Never Shipped".to_owned(),
                rendering: "stepped".to_owned(),
                ..Default::default()
            },
        );
        let restored = apply_palettes(&choices);
        let restored_velocity = restored.for_family(ColorTableFamily::Velocity);
        // The default palette, in the rendering that WAS understood.
        assert_eq!(
            restored_velocity.base_name(),
            ColorTableSet::default()
                .for_family(ColorTableFamily::Velocity)
                .base_name()
        );
        assert_eq!(restored_velocity.rendering(), TableRendering::Stepped);
        // And every colour stop is real: the table samples, it does not blank.
        assert!(!restored_velocity.stops().is_empty());
    }

    #[test]
    fn an_unknown_family_id_is_skipped_and_an_unknown_rendering_reads_smooth() {
        let mut choices = BTreeMap::new();
        choices.insert(
            "chroma_futures".to_owned(),
            PaletteChoice {
                name: "X".to_owned(),
                rendering: "holographic".to_owned(),
                ..Default::default()
            },
        );
        choices.insert(
            "reflectivity".to_owned(),
            PaletteChoice {
                name: "GR2Analyst Classic REF".to_owned(),
                rendering: "holographic".to_owned(),
                ..Default::default()
            },
        );
        let restored = apply_palettes(&choices);
        assert_eq!(
            restored
                .for_family(ColorTableFamily::Reflectivity)
                .rendering(),
            TableRendering::Smooth
        );
    }
}
