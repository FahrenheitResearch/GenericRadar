//! The proof battery demanded of the real store: byte-exact round trips,
//! forward compatibility with files this build did not write, corruption
//! recovery, and atomic-write hygiene - all on real files on the real
//! filesystem, no mocked IO anywhere.

use std::path::PathBuf;

use settings::{LoadStatus, SettingValue, SettingsStore};

/// A fresh directory under the system temp dir, unique per test, removed by
/// the OS's temp cleaning if a failing test leaks it.
fn scratch_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("settings-store-proof")
        .join(format!(
            "{test}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn populated_store(path: &std::path::Path) -> SettingsStore {
    let mut store = SettingsStore::open(path);
    store.set(
        "map",
        "basemap_style",
        SettingValue::Text("daylight".into()),
    );
    store.set("map", "imagery_dim", SettingValue::Float(0.42));
    store.set("radar", "sweep_animation", SettingValue::Bool(false));
    store.set("data", "history_max_frames", SettingValue::Int(45));
    let mut workspace = store.workspace().clone();
    workspace.layout = Some("four".to_owned());
    workspace.last_site = Some("KTLX".to_owned());
    workspace.panes = vec![settings::PaneSnapshot {
        product: Some("DVEL".to_owned()),
        tilt_mode: Some("cut".to_owned()),
        tilt_value: Some(3.0),
        center_east_km: Some(-12.5),
        center_north_km: Some(40.0),
        km_per_point: Some(0.25),
        rotation_rad: Some(0.0),
        camera_linked: Some(true),
        ..Default::default()
    }];
    workspace.palettes.insert(
        "velocity".to_owned(),
        settings::PaletteChoice {
            name: "Analyst Tornado VEL".to_owned(),
            rendering: "smooth".to_owned(),
            ..Default::default()
        },
    );
    store.set_workspace(workspace);
    store
}

#[test]
fn write_reload_rewrite_is_byte_identical() {
    let dir = scratch_dir("round-trip");
    let path = dir.join("settings.json");

    let mut store = populated_store(&path);
    store.save_now().expect("first save");
    let first_bytes = std::fs::read(&path).expect("read first file");

    // Load the file back and write it out again untouched. Byte equality
    // proves load -> save loses nothing and reorders nothing (BTreeMap-backed
    // JSON is deterministically ordered).
    let mut reloaded = SettingsStore::open(&path);
    assert_eq!(reloaded.status(), &LoadStatus::Loaded);
    reloaded.save_now().expect("second save");
    let second_bytes = std::fs::read(&path).expect("read second file");
    assert_eq!(
        first_bytes, second_bytes,
        "a load/save round trip must be byte-identical"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_future_versions_file_survives_this_build_editing_and_saving() {
    let dir = scratch_dir("forward-compat");
    let path = dir.join("settings.json");
    // Written by a hypothetical future build: higher version, unknown
    // top-level section, unknown category, unknown setting id, unknown
    // workspace and pane fields.
    let future = r#"{
        "version": 7,
        "values": {
            "map": { "basemap_style": "daylight", "parallax_layers": 3 },
            "holo_deck": { "enabled": true }
        },
        "workspace": {
            "layout": "four",
            "panes": [ { "product": "REF", "ai_annotations": {"model": "v9"} } ],
            "replay_bookmarks": [ {"t": 12} ]
        },
        "cloud_sync": { "endpoint": "https://example.invalid" }
    }"#;
    std::fs::write(&path, future).expect("write future file");

    let mut store = SettingsStore::open(&path);
    assert_eq!(store.status(), &LoadStatus::Loaded);
    // This build reads what it understands...
    assert_eq!(
        store.value("map", "basemap_style"),
        Some(SettingValue::Text("daylight".into()))
    );
    // ...edits something...
    store.set("map", "basemap_style", SettingValue::Text("slate".into()));
    let mut workspace = store.workspace().clone();
    workspace.last_site = Some("KDMX".to_owned());
    store.set_workspace(workspace);
    store.save_now().expect("save");

    // ...and everything it did NOT understand is still in the file.
    let text = std::fs::read_to_string(&path).expect("re-read");
    let json: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(json["version"], 7, "the higher version is preserved");
    assert_eq!(json["values"]["map"]["parallax_layers"], 3);
    assert_eq!(json["values"]["holo_deck"]["enabled"], true);
    assert_eq!(
        json["workspace"]["panes"][0]["ai_annotations"]["model"],
        "v9"
    );
    assert_eq!(json["workspace"]["replay_bookmarks"][0]["t"], 12);
    assert_eq!(json["cloud_sync"]["endpoint"], "https://example.invalid");
    // And the edits landed.
    assert_eq!(json["values"]["map"]["basemap_style"], "slate");
    assert_eq!(json["workspace"]["last_site"], "KDMX");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_truncated_file_recovers_to_defaults_and_is_replaced_on_next_save() {
    let dir = scratch_dir("corrupt");
    let path = dir.join("settings.json");
    // A crash mid-write by some OTHER program (this store's own writes are
    // atomic): valid prefix, cut mid-token.
    std::fs::write(&path, r#"{"version": 1, "values": {"map": {"basema"#)
        .expect("write truncated file");

    let mut store = SettingsStore::open(&path);
    match store.status() {
        LoadStatus::Recovered { backup } => {
            let backup = backup.as_ref().expect("bad file moved aside");
            assert!(backup.exists(), "backup file exists at {backup:?}");
            assert!(backup.to_string_lossy().ends_with(".corrupt"), "{backup:?}");
        }
        other => panic!("expected Recovered, got {other:?}"),
    }
    // Defaults apply; nothing panicked; the store works.
    assert_eq!(store.value("map", "basemap_style"), None);
    store.set("map", "basemap_style", SettingValue::Text("slate".into()));
    store.save_now().expect("save replaces the file");
    let reloaded = SettingsStore::open(&path);
    assert_eq!(reloaded.status(), &LoadStatus::Loaded);
    assert_eq!(
        reloaded.value("map", "basemap_style"),
        Some(SettingValue::Text("slate".into()))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saving_leaves_no_temp_droppings_and_a_missing_parent_dir_is_created() {
    let dir = scratch_dir("atomic");
    let path = dir.join("deep").join("nested").join("settings.json");
    let mut store = SettingsStore::open(&path);
    store.set("map", "site_markers", SettingValue::Bool(true));
    store
        .save_now()
        .expect("save into a directory that did not exist");
    assert!(path.exists());
    let siblings: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("list dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(
        siblings,
        vec![std::ffi::OsString::from("settings.json")],
        "no temp files left beside the settings file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_path_yields_defaults_and_autosave_stands_down() {
    let dir = scratch_dir("unreadable");
    // The settings "file" is a directory: read_to_string fails with a
    // real IO error that is not NotFound.
    let path = dir.join("settings.json");
    std::fs::create_dir_all(&path).expect("create dir in the way");
    let mut store = SettingsStore::open(&path);
    assert!(
        matches!(store.status(), LoadStatus::Unreadable { .. }),
        "{:?}",
        store.status()
    );
    store.set("map", "site_markers", SettingValue::Bool(false));
    assert!(store.is_dirty());
    // Autosave refuses; the unreadable path is not overwritten.
    assert!(store.autosave_tick().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_file_is_first_run_and_setting_the_same_value_twice_stays_clean() {
    let dir = scratch_dir("first-run");
    let path = dir.join("settings.json");
    let mut store = SettingsStore::open(&path);
    assert_eq!(store.status(), &LoadStatus::Defaults);
    assert!(!store.is_dirty());
    assert!(store.set("map", "imagery_dim", SettingValue::Float(0.4)));
    store.save_now().expect("save");
    assert!(!store.is_dirty());
    // Same value again: not a change, not dirty, no save pressure. This is
    // what lets the application mirror state into the store every frame.
    assert!(!store.set("map", "imagery_dim", SettingValue::Float(0.4)));
    assert!(!store.is_dirty());
    assert!(!store.set_workspace(store.workspace().clone()));
    assert!(!store.is_dirty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_workspace_carries_unknown_fields_and_unknown_palette_families_forward() {
    let dir = scratch_dir("workspace-carry");
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        r#"{
            "version": 1,
            "workspace": {
                "layout": "one",
                "palettes": { "chroma_futures": { "name": "X", "rendering": "smooth" } },
                "future_field": 42
            }
        }"#,
    )
    .expect("write file");
    let mut store = SettingsStore::open(&path);
    // The application rebuilds its snapshot from live state, which cannot
    // know about "future_field" or the "chroma_futures" family.
    let mut rebuilt = settings::WorkspaceSnapshot {
        layout: Some("four".to_owned()),
        ..Default::default()
    };
    rebuilt.palettes.insert(
        "reflectivity".to_owned(),
        settings::PaletteChoice {
            name: "Smooth Classic REF".to_owned(),
            rendering: "smooth".to_owned(),
            ..Default::default()
        },
    );
    assert!(store.set_workspace(rebuilt));
    store.save_now().expect("save");
    let text = std::fs::read_to_string(&path).expect("re-read");
    let json: serde_json::Value = serde_json::from_str(&text).expect("valid");
    assert_eq!(json["workspace"]["future_field"], 42);
    assert_eq!(json["workspace"]["palettes"]["chroma_futures"]["name"], "X");
    assert_eq!(
        json["workspace"]["palettes"]["reflectivity"]["name"],
        "Smooth Classic REF"
    );
    assert_eq!(json["workspace"]["layout"], "four");
    let _ = std::fs::remove_dir_all(&dir);
}
