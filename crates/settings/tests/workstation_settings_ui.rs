//! Compile-and-test harness for the workstation's settings window.
//!
//! `workstation_app/src/settings_ui.rs` is a new module whose `mod` wiring
//! into the binary is human-owned (`main.rs` is outside this workflow's
//! files). An unreferenced source file is not compiled at all, and shipping
//! an unverified UI module would be worthless - so until the wiring lands,
//! this integration test includes the real file by path and compiles it,
//! with all of its `#[cfg(test)]` modules, against the same crates the
//! workstation links. The module deliberately references nothing from the
//! `workstation_app` crate itself (only `settings`, `eframe`,
//! `color_tables`, `map_scene`, `analyst_runtime`, `render2d`,
//! `radar_core`), which is what makes this include valid in both homes.
//!
//! Once `mod settings_ui;` lands in `workstation_app/src/main.rs`, this
//! harness becomes a duplicate compile of the same source and SHOULD BE
//! DELETED together with the dev-dependencies it needs - the integration
//! notes say so too.
//!
//! `dead_code` is allowed on the include because the harness exercises the
//! module's tests, not every public item; in its real home the application
//! calls the rest.

#[allow(dead_code)]
#[path = "../../workstation_app/src/settings_ui.rs"]
mod settings_ui;
// The Appearance page is declared by the theme module, because its options
// are derived from the theme catalog. Included here so this harness covers
// the REAL registry rather than one page short of it.
#[allow(dead_code)]
#[path = "../../workstation_app/src/theme.rs"]
mod theme;

/// The registry the application runs on, assembled the way `app.rs`
/// assembles it.
fn registry() -> settings::SettingsRegistry {
    settings_ui::full_registry(theme::settings::settings_category())
}

use settings::{SettingValue, SettingsStore};

/// Every declared key resolves through the real store: a typo in a catalog
/// id - or an `effective_*` call against a key that was never declared -
/// would return `None`/defaults silently in release, so it is pinned here.
#[test]
fn every_catalog_key_resolves_against_the_real_store() {
    let registry = registry();
    let store =
        SettingsStore::open(std::env::temp_dir().join("settings-ui-harness-never-written.json"));
    let mut checked = 0usize;
    for category in registry.categories() {
        for spec in &category.settings {
            let resolved = store.effective(&registry, &category.id, &spec.id);
            assert_eq!(
                resolved,
                Some(spec.kind.default_value()),
                "{}/{} does not resolve to its default on a fresh store",
                category.id,
                spec.id
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 40,
        "the catalog should be substantial; found only {checked} settings"
    );
}

/// The full write-reload cycle over every catalog setting, on a real file:
/// set every setting to a non-default value, save, reopen, and read every
/// one back identically.
#[test]
fn every_catalog_setting_survives_a_real_disk_round_trip() {
    let dir = std::env::temp_dir().join(format!(
        "settings-ui-harness-roundtrip-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("settings.json");
    let registry = registry();

    let mut expectations: Vec<(String, String, SettingValue)> = Vec::new();
    {
        let mut store = SettingsStore::open(&path);
        for category in registry.categories() {
            for spec in &category.settings {
                let non_default = non_default_value(&spec.kind);
                assert!(
                    store.set(&category.id, &spec.id, non_default.clone()),
                    "{}/{}: setting a non-default value must register as a change",
                    category.id,
                    spec.id
                );
                expectations.push((category.id.clone(), spec.id.clone(), non_default));
            }
        }
        store.save_now().expect("save");
    }

    let reopened = SettingsStore::open(&path);
    for (category, id, expected) in &expectations {
        let effective = reopened
            .effective(&registry, category, id)
            .expect("declared key resolves");
        assert_eq!(
            &effective, expected,
            "{category}/{id} did not survive the disk round trip"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A legal value different from the default, inside the declared range, for
/// every kind - so the round trip test proves storage, not clamping.
fn non_default_value(kind: &settings::SettingKind) -> SettingValue {
    match kind {
        settings::SettingKind::Toggle { default } => SettingValue::Bool(!default),
        settings::SettingKind::Slider {
            min, max, default, ..
        } => {
            let candidate = (min + max) / 2.0;
            let value = if (candidate - default).abs() > 1e-9 {
                candidate
            } else {
                *min
            };
            SettingValue::Float(value)
        }
        settings::SettingKind::Integer {
            min, max, default, ..
        } => {
            let candidate = (min + max) / 2;
            let value = if candidate != *default {
                candidate
            } else {
                *min
            };
            SettingValue::Int(value)
        }
        settings::SettingKind::Choice {
            options,
            default_id,
        } => {
            let other = options
                .iter()
                .find(|option| option.id != *default_id)
                .expect("every choice offers at least two options");
            SettingValue::Text(other.id.clone())
        }
        settings::SettingKind::Text { default, .. } => {
            let value = if default == "KDMX" { "KTLX" } else { "KDMX" };
            SettingValue::Text(value.to_owned())
        }
    }
}

/// The palettes round trip on the REAL color_tables catalog through a REAL
/// file: install non-default tables, snapshot to disk, reload, resolve.
#[test]
fn palette_choices_survive_disk_and_unknown_names_fall_back_not_blank() {
    use color_tables::{ColorTableFamily, ColorTableSet, TableRendering};

    let dir = std::env::temp_dir().join(format!(
        "settings-ui-harness-palettes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("settings.json");

    let mut tables = ColorTableSet::default();
    let stepped_velocity = color_tables::builtin_tables_for_family(ColorTableFamily::Velocity)
        .into_iter()
        .nth(3)
        .expect("velocity catalog depth")
        .rendered(TableRendering::Stepped);
    tables.set_family(ColorTableFamily::Velocity, stepped_velocity.clone());

    {
        let mut store = SettingsStore::open(&path);
        let mut workspace = store.workspace().clone();
        workspace.palettes = settings_ui::palettes::capture_palettes(&tables);
        store.set_workspace(workspace);
        store.save_now().expect("save");
    }

    let reopened = SettingsStore::open(&path);
    let restored = settings_ui::palettes::apply_palettes(&reopened.workspace().palettes);
    assert_eq!(
        restored.for_family(ColorTableFamily::Velocity),
        &stepped_velocity,
        "the stepped non-default velocity palette must come back exactly"
    );

    // Sabotage the file the way a deleted-palette future would: unknown name.
    {
        let mut store = SettingsStore::open(&path);
        let mut workspace = store.workspace().clone();
        if let Some(choice) = workspace.palettes.get_mut("velocity") {
            choice.name = "Removed In A Future Build".to_owned();
        }
        store.set_workspace(workspace);
        store.save_now().expect("save");
    }
    let sabotaged = SettingsStore::open(&path);
    let restored = settings_ui::palettes::apply_palettes(&sabotaged.workspace().palettes);
    let velocity = restored.for_family(ColorTableFamily::Velocity);
    assert!(
        !velocity.stops().is_empty(),
        "an unknown palette name must fall back to a real table, never blank a pane"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
