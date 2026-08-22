//! Exact producer-field colour-table overrides.
//!
//! A producer-native field is deliberately outside the fixed product
//! registry: its name and unit token came from the file, and may mean
//! something this build has not modeled. That makes a shared `Generic`
//! family override unsafe. Changing `V1` must not recolour `NCP1`, or another
//! file's unrelated unknown field, merely because all three need a generic
//! renderer.
//!
//! Absence from this map is the automatic mode: stretch the installed Generic
//! colour sequence across the selected cut's observed finite values. Presence
//! is an analyst decision scoped to the exact namespaced [`ProductId`], and
//! carries a fixed raw-value table for the running session. Only a binding to
//! a table saved through the editor enters the settings snapshot. That binding
//! remembers the palette's stable name, rendering, and explicit numeric
//! endpoints; if its `.pal` is temporarily absent, restart falls back to the
//! Generic colours over those endpoints rather than silently returning to
//! auto-range.

use std::collections::BTreeMap;

use color_tables::user::UserTableLibrary;
use color_tables::{ColorTable, ColorTableFamily, ColorTableSet};
use radar_core::ProductId;
use settings::SourceFieldPaletteChoice;

use crate::source_fields::{SourceFieldDisplay, producer_name_from_product_id};

#[derive(Clone, Debug, Default)]
pub struct SourceFieldPaletteOverrides {
    entries: BTreeMap<ProductId, Entry>,
    /// Entries this build cannot safely interpret are still written back.
    /// This keeps the settings crate's future-version round-trip contract at
    /// the map-entry level as well as at the struct-field level.
    preserved: BTreeMap<String, SourceFieldPaletteChoice>,
}

#[derive(Clone, Debug)]
struct Entry {
    table: ColorTable,
    /// The binding written to the workspace. `None` means the table above is
    /// a session-only Apply. A session table may sit over an older durable
    /// binding; in that case this remains the restart fallback.
    durable_choice: Option<SourceFieldPaletteChoice>,
    /// Whether `table` itself is the durable choice rather than a later live
    /// edit over it.
    current_is_durable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSourceFieldPalette {
    pub table: ColorTable,
    pub automatic: bool,
    /// The exact table on screen has a saved workspace binding. Automatic
    /// ranges and session-only Apply results are both false.
    pub current_is_durable: bool,
}

impl ResolvedSourceFieldPalette {
    pub fn value_range(&self) -> (f32, f32) {
        table_range(&self.table).expect("a resolved colour table has at least two finite stops")
    }
}

impl SourceFieldPaletteOverrides {
    pub fn from_snapshot(
        snapshot: &BTreeMap<String, SourceFieldPaletteChoice>,
        library: &UserTableLibrary,
    ) -> Self {
        let mut restored = Self::default();
        for (raw_id, choice) in snapshot {
            let id = ProductId(raw_id.clone());
            let valid_id =
                producer_name_from_product_id(&id).is_some() && !choice.name.trim().is_empty();
            let range = choice
                .minimum
                .zip(choice.maximum)
                .filter(|(minimum, maximum)| {
                    minimum.is_finite() && maximum.is_finite() && minimum < maximum
                });
            let Some((minimum, maximum)) = range.filter(|_| valid_id) else {
                restored.preserved.insert(raw_id.clone(), choice.clone());
                continue;
            };
            let producer_name = producer_name_from_product_id(&id)
                .expect("the exact source-field id was validated above");
            let template = find_template(choice, library).unwrap_or_else(|| {
                ColorTableSet::default()
                    .for_family(ColorTableFamily::Generic)
                    .clone()
            });
            let table = if table_range(&template) == Some((minimum, maximum)) {
                template
            } else {
                crate::palettes::source_field_table_from_template(
                    producer_name,
                    minimum,
                    maximum,
                    &template,
                )
            };
            restored.entries.insert(
                id,
                Entry {
                    table,
                    durable_choice: Some(choice.clone()),
                    current_is_durable: true,
                },
            );
        }
        restored
    }

    pub fn resolve(
        &self,
        id: &ProductId,
        source: &SourceFieldDisplay,
        tables: &ColorTableSet,
    ) -> ResolvedSourceFieldPalette {
        match self.entries.get(id) {
            Some(entry) => ResolvedSourceFieldPalette {
                table: entry.table.clone(),
                automatic: false,
                current_is_durable: entry.current_is_durable,
            },
            None => ResolvedSourceFieldPalette {
                table: crate::palettes::source_field_table(
                    &source.producer_name,
                    source.finite_min,
                    source.finite_max,
                    tables,
                ),
                automatic: true,
                current_is_durable: false,
            },
        }
    }

    /// Install a table only for this exact source id and this running session.
    ///
    /// A prior saved binding is retained as the restart fallback, but the live
    /// table is not silently promoted into the workspace snapshot.
    pub fn apply_session(&mut self, id: ProductId, table: ColorTable) -> bool {
        self.apply(id, table, false)
    }

    /// Install a clean, file-backed table and bind it across restart.
    pub fn apply_saved(&mut self, id: ProductId, table: ColorTable) -> bool {
        self.apply(id, table, true)
    }

    fn apply(&mut self, id: ProductId, table: ColorTable, durable: bool) -> bool {
        if producer_name_from_product_id(&id).is_none() {
            return false;
        }
        let Some(choice) = choice_from_table(&table) else {
            return false;
        };
        let durable_choice = if durable {
            Some(choice)
        } else {
            self.entries
                .get(&id)
                .and_then(|entry| entry.durable_choice.clone())
        };
        let changed = self.entries.get(&id).is_none_or(|entry| {
            entry.table != table
                || entry.durable_choice != durable_choice
                || entry.current_is_durable != durable
        });
        if durable {
            self.preserved.remove(&id.0);
        }
        self.entries.insert(
            id,
            Entry {
                table,
                durable_choice,
                current_is_durable: durable,
            },
        );
        changed
    }

    /// Promote a matching active session-only Apply after its editor table is
    /// saved. Saving a reusable file without first applying it must not change
    /// the exact-field binding.
    pub fn promote_matching_saved(&mut self, id: &ProductId, table: &ColorTable) -> bool {
        let Some(choice) = choice_from_table(table) else {
            return false;
        };
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.table != *table || entry.current_is_durable {
            return false;
        }
        entry.durable_choice = Some(choice);
        entry.current_is_durable = true;
        self.preserved.remove(&id.0);
        true
    }

    pub fn reset(&mut self, id: &ProductId) -> bool {
        let removed = self.entries.remove(id).is_some();
        let preserved = self.preserved.remove(&id.0).is_some();
        removed || preserved
    }

    pub fn capture(&self) -> BTreeMap<String, SourceFieldPaletteChoice> {
        let mut snapshot = self.preserved.clone();
        snapshot.extend(self.entries.iter().filter_map(|(id, entry)| {
            entry
                .durable_choice
                .clone()
                .map(|choice| (id.0.clone(), choice))
        }));
        snapshot
    }

    /// Re-resolve saved table names after the user colour-table directory is
    /// rescanned. The stored choice stays intact when its file is missing.
    pub fn reresolve(&mut self, library: &UserTableLibrary) {
        for (id, entry) in &mut self.entries {
            // Folder rescans must not erase a session-only live judgment. A
            // durable binding underneath it remains available to restart.
            if !entry.current_is_durable {
                continue;
            }
            let Some(choice) = entry.durable_choice.as_ref() else {
                continue;
            };
            let (Some(minimum), Some(maximum)) = (choice.minimum, choice.maximum) else {
                continue;
            };
            let Some(template) = find_template(choice, library) else {
                continue;
            };
            let producer_name = producer_name_from_product_id(id)
                .expect("only validated source ids enter the override map");
            entry.table = if table_range(&template) == Some((minimum, maximum)) {
                template
            } else {
                crate::palettes::source_field_table_from_template(
                    producer_name,
                    minimum,
                    maximum,
                    &template,
                )
            };
        }
    }
}

fn choice_from_table(table: &ColorTable) -> Option<SourceFieldPaletteChoice> {
    let (minimum, maximum) = table_range(table)?;
    Some(SourceFieldPaletteChoice {
        name: table.base_name().to_owned(),
        rendering: crate::settings_ui::palettes::rendering_id(table.rendering()).to_owned(),
        minimum: Some(minimum),
        maximum: Some(maximum),
        ..Default::default()
    })
}

fn find_template(
    choice: &SourceFieldPaletteChoice,
    library: &UserTableLibrary,
) -> Option<ColorTable> {
    let family = ColorTableFamily::Generic;
    let rendering = crate::settings_ui::palettes::rendering_from_id(&choice.rendering);
    color_tables::builtin_tables_for_family(family)
        .into_iter()
        .find(|table| table.base_name() == choice.name)
        .or_else(|| {
            library
                .table_for_family_named(family, &choice.name)
                .cloned()
        })
        .map(|table| table.rendered(rendering))
}

fn table_range(table: &ColorTable) -> Option<(f32, f32)> {
    let minimum = table.stops().first()?.value;
    let maximum = table.stops().last()?.value;
    (minimum.is_finite() && maximum.is_finite() && minimum < maximum).then_some((minimum, maximum))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_fields;

    fn source(name: &str, minimum: f32, maximum: f32) -> SourceFieldDisplay {
        SourceFieldDisplay {
            producer_name: name.to_owned(),
            producer_description: None,
            producer_units: None,
            moment: radar_core::MomentType::Unknown(name.to_owned()),
            finite_count: 2,
            finite_min: minimum,
            finite_max: maximum,
        }
    }

    fn empty_library() -> UserTableLibrary {
        UserTableLibrary::empty(std::env::temp_dir().join("generic-radar-source-palette-tests"))
    }

    #[test]
    fn absence_is_an_automatic_table_over_this_cuts_observed_values() {
        let overrides = SourceFieldPaletteOverrides::default();
        let id = source_fields::product_id("NCP1");
        let resolved =
            overrides.resolve(&id, &source("NCP1", 0.12, 0.98), &ColorTableSet::default());
        assert!(resolved.automatic);
        assert!(!resolved.current_is_durable);
        assert_eq!(resolved.value_range(), (0.12, 0.98));
    }

    #[test]
    fn an_override_is_scoped_to_the_exact_case_sensitive_product_id() {
        let tables = ColorTableSet::default();
        let upper_id = source_fields::product_id("V1");
        let lower_id = source_fields::product_id("v1");
        let custom = crate::palettes::source_field_table("V1", -30.0, 30.0, &tables);
        let mut overrides = SourceFieldPaletteOverrides::default();
        assert!(overrides.apply_session(upper_id.clone(), custom.clone()));

        let upper = overrides.resolve(&upper_id, &source("V1", -2.0, 2.0), &tables);
        let lower = overrides.resolve(&lower_id, &source("v1", -2.0, 2.0), &tables);
        assert!(!upper.automatic);
        assert!(!upper.current_is_durable);
        assert_eq!(upper.table, custom);
        assert!(lower.automatic);
        assert_eq!(lower.value_range(), (-2.0, 2.0));
    }

    #[test]
    fn an_unsaved_apply_does_not_enter_a_workspace_round_trip_or_restart() {
        let tables = ColorTableSet::default();
        let id = source_fields::product_id("V1");
        let custom = crate::palettes::source_field_table("V1", -30.0, 30.0, &tables);
        let mut live = SourceFieldPaletteOverrides::default();
        assert!(live.apply_session(id.clone(), custom));
        assert!(live.capture().is_empty(), "autosave must see no binding");

        let restarted =
            SourceFieldPaletteOverrides::from_snapshot(&live.capture(), &empty_library());
        let resolved = restarted.resolve(&id, &source("V1", -2.0, 2.0), &tables);
        assert!(resolved.automatic);
        assert_eq!(resolved.value_range(), (-2.0, 2.0));
    }

    #[test]
    fn a_saved_binding_round_trips_with_its_explicit_range_and_reset_removes_it() {
        let tables = ColorTableSet::default();
        let id = source_fields::product_id("ZH1C");
        let custom = crate::palettes::source_field_table("ZH1C", -18.0, 72.0, &tables);
        let mut overrides = SourceFieldPaletteOverrides::default();
        assert!(overrides.apply_saved(id.clone(), custom));

        let snapshot = overrides.capture();
        let mut restored = SourceFieldPaletteOverrides::from_snapshot(&snapshot, &empty_library());
        let resolved = restored.resolve(&id, &source("ZH1C", -4.0, 42.0), &tables);
        assert!(!resolved.automatic);
        assert!(resolved.current_is_durable);
        assert_eq!(resolved.value_range(), (-18.0, 72.0));
        assert!(restored.reset(&id));
        assert!(
            restored
                .resolve(&id, &source("ZH1C", -4.0, 42.0), &tables)
                .automatic
        );
        assert!(restored.capture().is_empty());
    }

    #[test]
    fn saving_promotes_only_the_matching_active_session_apply() {
        let tables = ColorTableSet::default();
        let id = source_fields::product_id("V1");
        let custom = crate::palettes::source_field_table("V1", -30.0, 30.0, &tables);
        let other = crate::palettes::source_field_table("V1", -20.0, 20.0, &tables);
        let mut overrides = SourceFieldPaletteOverrides::default();

        assert!(
            !overrides.promote_matching_saved(&id, &custom),
            "Save alone must not create a binding"
        );
        assert!(overrides.apply_session(id.clone(), custom.clone()));
        assert!(
            !overrides.promote_matching_saved(&id, &other),
            "saving a different edit must not promote the active preview"
        );
        assert!(overrides.capture().is_empty());
        assert!(overrides.promote_matching_saved(&id, &custom));
        assert!(
            overrides
                .resolve(&id, &source("V1", -2.0, 2.0), &tables)
                .current_is_durable
        );
        assert!(overrides.capture().contains_key(&id.0));
    }

    #[test]
    fn a_later_session_edit_keeps_the_older_saved_restart_fallback() {
        let tables = ColorTableSet::default();
        let id = source_fields::product_id("V1");
        let saved = crate::palettes::source_field_table("V1", -30.0, 30.0, &tables);
        let session = crate::palettes::source_field_table("V1", -12.0, 12.0, &tables);
        let mut overrides = SourceFieldPaletteOverrides::default();
        assert!(overrides.apply_saved(id.clone(), saved));
        let saved_snapshot = overrides.capture();
        assert!(overrides.apply_session(id.clone(), session.clone()));

        let live = overrides.resolve(&id, &source("V1", -2.0, 2.0), &tables);
        assert_eq!(live.table, session);
        assert!(!live.current_is_durable);
        assert_eq!(overrides.capture(), saved_snapshot);

        let restarted =
            SourceFieldPaletteOverrides::from_snapshot(&overrides.capture(), &empty_library());
        assert_eq!(
            restarted
                .resolve(&id, &source("V1", -2.0, 2.0), &tables)
                .value_range(),
            (-30.0, 30.0)
        );
    }

    #[test]
    fn malformed_future_entries_survive_capture_unchanged() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            "FUTURE_FIELD:Q".to_owned(),
            SourceFieldPaletteChoice {
                name: "Tomorrow".to_owned(),
                minimum: Some(5.0),
                maximum: Some(4.0),
                ..Default::default()
            },
        );
        let restored = SourceFieldPaletteOverrides::from_snapshot(&snapshot, &empty_library());
        assert_eq!(restored.capture(), snapshot);
    }

    #[test]
    fn an_unrelated_folder_rescan_does_not_discard_an_unsaved_session_table() {
        let id = source_fields::product_id("V1");
        let custom = ColorTable::new(
            "Unsaved V1",
            vec![
                color_tables::ColorStop {
                    value: -9.0,
                    color: color_tables::Rgba8::opaque(250, 10, 20),
                    end_color: None,
                },
                color_tables::ColorStop {
                    value: 11.0,
                    color: color_tables::Rgba8::opaque(20, 30, 240),
                    end_color: None,
                },
            ],
        )
        .expect("custom table");
        let mut overrides = SourceFieldPaletteOverrides::default();
        assert!(overrides.apply_session(id.clone(), custom.clone()));

        overrides.reresolve(&empty_library());

        let resolved = overrides.resolve(&id, &source("V1", -2.0, 2.0), &ColorTableSet::default());
        assert_eq!(resolved.table, custom);
    }
}
