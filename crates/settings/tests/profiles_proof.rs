//! Named settings profiles, proven against real files on a real disk.
//!
//! The unit tests inside `settings::profiles` pin the pure functions - what a
//! snapshot strips, what a merge keeps, what counts as a difference. This
//! binary pins the behaviour an analyst actually meets: files written and read
//! back, a profile round trip that has to restore *exactly*, a hand-edited
//! file from a newer build, a file that does not parse at all, and the shipped
//! profile refusing to be destroyed.
//!
//! Nothing here touches `settings::app_config_root`: that root is a
//! process-global `OnceLock`, and a test that resolved it would decide it for
//! every other test in the process. Every directory below is a scratch
//! directory of this test's own.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value as Json, json};
use settings::profiles::{
    self, ProfileError, ProfileLibrary, ProfileNote, SHIPPED_NAME, differences, merge_for_switch,
};
use settings::{
    ChoiceOption, SettingKind, SettingSpec, SettingValue, SettingsCategory, SettingsDocument,
    SettingsRegistry, SettingsStore,
};

/// A scratch directory unique to each test, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "radar-profiles-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A registry shaped like the application's: a few pages, a few kinds.
fn registry() -> SettingsRegistry {
    let mut registry = SettingsRegistry::new();
    registry.register(SettingsCategory::new(
        "map",
        "Map",
        vec![
            SettingSpec::new(
                "basemap_style",
                "Basemap style",
                SettingKind::Choice {
                    options: vec![
                        ChoiceOption::new("slate", "Slate Dark"),
                        ChoiceOption::new("daylight", "Daylight"),
                    ],
                    default_id: "slate".to_owned(),
                },
            ),
            SettingSpec::new(
                "site_markers",
                "Site markers",
                SettingKind::Toggle { default: true },
            ),
        ],
    ));
    registry.register(SettingsCategory::new(
        "data",
        "Data",
        vec![SettingSpec::new(
            "poll_seconds",
            "Poll interval",
            SettingKind::Integer {
                min: 5,
                max: 600,
                default: 30,
                unit: "s".to_owned(),
            },
        )],
    ));
    registry
}

fn library(scratch: &Scratch, registry: &SettingsRegistry) -> ProfileLibrary {
    ProfileLibrary::open(
        scratch.join("profiles"),
        SettingsDocument::default(),
        registry,
    )
}

/// Switch the store to `name`, the way the settings window does it: merge the
/// profile into the live document, replace, and hand back what this build
/// could not honour.
fn switch(
    store: &mut SettingsStore,
    library: &ProfileLibrary,
    name: &str,
) -> (bool, Vec<ProfileNote>) {
    let profile = library.find(name).expect("the profile is in the library");
    let merged = merge_for_switch(store.document(), &profile.document, &profile.name);
    let changed = store.replace_document(merged);
    (changed, profile.faults.clone())
}

/// THE round trip: save a profile, change settings, switch away, switch back,
/// and find every value exactly as it was left - including one under a key
/// this build does not declare.
#[test]
fn save_change_switch_away_and_back_restores_the_document_exactly() {
    let scratch = Scratch::new("roundtrip");
    let registry = registry();
    let mut library = library(&scratch, &registry);
    let mut store = SettingsStore::open(scratch.join("settings.json"));

    // The chase setup.
    store.set("map", "basemap_style", SettingValue::Text("slate".into()));
    store.set("map", "site_markers", SettingValue::Bool(false));
    store.set("data", "poll_seconds", SettingValue::Int(15));
    // A setting this build does not recognize: nothing here knows what it
    // means, and the profile still has to carry it.
    store.set(
        "gate_filter",
        "min_dbz",
        SettingValue::Float(-10.5), // not in the registry above
    );
    let mut workspace = store.workspace().clone();
    workspace.layout = Some("four".to_owned());
    workspace.last_site = Some("KDVN".to_owned());
    store.set_workspace(workspace);
    let chase = store.document().clone();

    library
        .save_as("Chase", store.document(), &registry)
        .expect("save the chase setup");

    // The office setup, saved from a deliberately different document.
    store.set(
        "map",
        "basemap_style",
        SettingValue::Text("daylight".into()),
    );
    store.set("map", "site_markers", SettingValue::Bool(true));
    store.set("data", "poll_seconds", SettingValue::Int(120));
    let mut workspace = store.workspace().clone();
    workspace.layout = Some("one".to_owned());
    store.set_workspace(workspace);
    library
        .save_as("Office", store.document(), &registry)
        .expect("save the office setup");

    // Away, then back.
    switch(&mut store, &library, "Office");
    assert_eq!(
        store.effective_text(&registry, "map", "basemap_style"),
        "daylight"
    );
    assert_eq!(store.effective_int(&registry, "data", "poll_seconds"), 120);

    let (changed, notes) = switch(&mut store, &library, "Chase");
    assert!(changed, "switching back must move the document");
    assert!(
        notes
            .iter()
            .any(|note| matches!(note, ProfileNote::UnknownCategory { category, .. } if category == "gate_filter")),
        "the key this build does not declare must be reported, not hidden: {notes:?}"
    );

    // Exactly as it was left - every declared value, the undeclared one, and
    // the workspace.
    assert_eq!(
        store.effective_text(&registry, "map", "basemap_style"),
        "slate"
    );
    assert!(!store.effective_bool(&registry, "map", "site_markers"));
    assert_eq!(store.effective_int(&registry, "data", "poll_seconds"), 15);
    assert_eq!(
        store.value("gate_filter", "min_dbz"),
        Some(SettingValue::Float(-10.5)),
        "a value under a key this build does not know must survive the round trip"
    );
    assert_eq!(store.workspace().layout.as_deref(), Some("four"));
    assert_eq!(
        profiles::active_profile(store.document()),
        Some("Chase"),
        "the switch names the profile that is now active"
    );

    // The only differences from the document that was saved are the
    // install-local parts, which is to say: none that anybody is told about.
    assert_eq!(
        differences(store.document(), &chase, &registry),
        Vec::new(),
        "switching back must land on the saved document, not near it"
    );

    // And it is on disk, not just in memory.
    store.save_now().expect("save the settings file");
    let reopened = SettingsStore::open(scratch.join("settings.json"));
    assert_eq!(
        differences(reopened.document(), &chase, &registry),
        Vec::new()
    );
}

/// Changing a setting after a switch has to read as modified relative to the
/// profile, by name, and going back has to clear it.
#[test]
fn a_change_after_a_switch_reads_as_modified_and_names_what_moved() {
    let scratch = Scratch::new("modified");
    let registry = registry();
    let mut library = library(&scratch, &registry);
    let mut store = SettingsStore::open(scratch.join("settings.json"));

    store.set("data", "poll_seconds", SettingValue::Int(15));
    library
        .save_as("Chase", store.document(), &registry)
        .expect("save");
    switch(&mut store, &library, "Chase");

    let chase = library.find("Chase").expect("chase").document.clone();
    assert!(
        !profiles::differs(store.document(), &chase, &registry),
        "straight after a switch nothing differs"
    );

    store.set("data", "poll_seconds", SettingValue::Int(45));
    let moved = differences(store.document(), &chase, &registry);
    let described: Vec<String> = moved
        .iter()
        .map(|difference| difference.describe(&registry))
        .collect();
    assert_eq!(described, vec!["Data - Poll interval".to_owned()]);

    // Saving the change into the profile clears it; so would switching back.
    library
        .overwrite("Chase", store.document(), &registry)
        .expect("overwrite");
    let chase = library.find("Chase").expect("chase").document.clone();
    assert!(!profiles::differs(store.document(), &chase, &registry));
}

/// A profile from a newer build: unknown page, unknown id inside a known page,
/// a value shape no setting kind can read, and a newer format number. Every
/// one of them is a note; the settings this build does know still arrive; and
/// the parts it does not know survive to be switched back to.
#[test]
fn a_profile_from_a_newer_build_is_applied_as_far_as_it_is_understood() {
    let scratch = Scratch::new("newer");
    let registry = registry();
    let directory = scratch.join("profiles");
    std::fs::create_dir_all(&directory).expect("profiles directory");
    std::fs::write(
        directory.join("from-the-future.json"),
        serde_json::to_string_pretty(&json!({
            "profile_format": 7,
            "name": "Presentation",
            "holographic_preview": true,
            "settings": {
                "version": 9,
                "values": {
                    "map": {
                        "basemap_style": "daylight",
                        "hologram_mode": true,
                        "site_markers": { "per_pane": [true, false] }
                    },
                    "quantum_overlay": { "entanglement": 0.7, "spin": 2 }
                },
                "workspace": { "layout": "four" }
            }
        }))
        .expect("serialise"),
    )
    .expect("write the future profile");

    let mut library = ProfileLibrary::open(&directory, SettingsDocument::default(), &registry);
    assert!(
        library.broken().is_empty(),
        "a file from a newer build is not a broken file: {:?}",
        library.broken()
    );
    let profile = library.find("Presentation").expect("it loaded");
    let faults = profile.faults.clone();
    assert!(faults.contains(&ProfileNote::NewerProfileFormat {
        found: 7,
        known: profiles::PROFILE_FORMAT_VERSION
    }));
    assert!(
        faults
            .iter()
            .any(|note| matches!(note, ProfileNote::NewerSettingsFormat { found: 9, .. }))
    );
    assert!(faults.contains(&ProfileNote::UnknownCategory {
        category: "quantum_overlay".to_owned(),
        settings: 2
    }));
    assert!(faults.contains(&ProfileNote::UnknownSetting {
        category: "map".to_owned(),
        id: "hologram_mode".to_owned()
    }));
    assert!(faults.contains(&ProfileNote::UnreadableValue {
        category: "map".to_owned(),
        id: "site_markers".to_owned()
    }));
    for note in &faults {
        assert!(
            !note.message().is_empty(),
            "every note has to say something"
        );
    }

    let mut store = SettingsStore::open(scratch.join("settings.json"));
    switch(&mut store, &library, "Presentation");
    // Understood: applied.
    assert_eq!(
        store.effective_text(&registry, "map", "basemap_style"),
        "daylight"
    );
    assert_eq!(store.workspace().layout.as_deref(), Some("four"));
    // Not understood: skipped, and the setting it collided with fell back to
    // its declared default rather than taking the page down with it.
    assert!(store.effective_bool(&registry, "map", "site_markers"));
    // Not understood: still carried, so the build that wrote it finds it.
    assert_eq!(
        store.document().values["quantum_overlay"]["entanglement"],
        json!(0.7)
    );
    store.save_now().expect("save");
    let text = std::fs::read_to_string(scratch.join("settings.json")).expect("read back");
    assert!(
        text.contains("entanglement"),
        "the settings file must still carry what this build could not read:\n{text}"
    );

    // Rewriting the profile keeps the wrapper field this build never knew.
    library
        .overwrite("Presentation", store.document(), &registry)
        .expect("overwrite");
    let rewritten =
        std::fs::read_to_string(directory.join("from-the-future.json")).expect("read profile");
    assert!(
        rewritten.contains("holographic_preview"),
        "a wrapper field from a newer build must survive this build rewriting the file:\n{rewritten}"
    );
    assert!(
        rewritten.contains("\"profile_format\": 7"),
        "and so must its format number:\n{rewritten}"
    );
}

/// A file that is not a profile at all, and two files claiming one name.
/// Neither may take the library down, and both have to be visible.
#[test]
fn a_corrupt_file_is_reported_by_name_and_the_rest_of_the_library_still_loads() {
    let scratch = Scratch::new("corrupt");
    let registry = registry();
    let directory = scratch.join("profiles");
    std::fs::create_dir_all(&directory).expect("profiles directory");
    std::fs::write(directory.join("torn.json"), "{ \"name\": \"Torn\", ")
        .expect("write a truncated file");
    std::fs::write(
        directory.join("notes.txt"),
        "not a profile and not claimed to be",
    )
    .expect("write a stray file");
    std::fs::write(
        directory.join("zz-a-copy.json"),
        serde_json::to_string(&json!({ "profile_format": 1, "name": "Chase", "settings": {} }))
            .expect("serialise"),
    )
    .expect("write a second file claiming the same name");
    std::fs::write(
        directory.join("chase.json"),
        serde_json::to_string(&json!({
            "profile_format": 1,
            "name": "Chase",
            "settings": { "values": { "data": { "poll_seconds": 15 } } }
        }))
        .expect("serialise"),
    )
    .expect("write the real chase profile");

    let mut library = ProfileLibrary::open(&directory, SettingsDocument::default(), &registry);
    let names: Vec<&str> = library
        .profiles()
        .iter()
        .map(|profile| profile.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![SHIPPED_NAME, "Chase"],
        "the readable profile and the shipped one, and nothing invented"
    );
    let broken: Vec<String> = library
        .broken()
        .iter()
        .map(|broken| {
            format!(
                "{}: {}",
                broken
                    .file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default(),
                broken.reason
            )
        })
        .collect();
    assert_eq!(broken.len(), 2, "{broken:?}");
    assert!(
        broken.iter().any(|line| line.starts_with("torn.json")),
        "{broken:?}"
    );
    assert!(
        broken
            .iter()
            .any(|line| line.starts_with("zz-a-copy.json") && line.contains("already called")),
        "two files claiming one name must be said out loud: {broken:?}"
    );

    // The readable one still works, and the broken one can be cleared from
    // inside the application.
    let mut store = SettingsStore::open(scratch.join("settings.json"));
    switch(&mut store, &library, "Chase");
    assert_eq!(store.effective_int(&registry, "data", "poll_seconds"), 15);
    let torn = directory.join("torn.json");
    library
        .delete_file(&torn, &registry)
        .expect("clear the broken file");
    assert!(!torn.exists());
    assert_eq!(library.broken().len(), 1);
}

/// The shipped profile: always there, never destroyable, and a way back to a
/// known state.
#[test]
fn the_shipped_profile_always_exists_and_refuses_to_be_destroyed() {
    let scratch = Scratch::new("shipped");
    let registry = registry();
    let mut library = library(&scratch, &registry);
    assert_eq!(library.profiles().len(), 1);
    assert_eq!(library.profiles()[0].name, SHIPPED_NAME);
    assert!(library.profiles()[0].is_shipped());

    assert_eq!(
        library.delete(SHIPPED_NAME, &registry),
        Err(ProfileError::Shipped)
    );
    assert_eq!(
        library.rename(SHIPPED_NAME, "Mine", &registry),
        Err(ProfileError::Shipped)
    );
    assert_eq!(
        library.overwrite(SHIPPED_NAME, &SettingsDocument::default(), &registry),
        Err(ProfileError::Shipped)
    );
    assert!(matches!(
        library.save_as("as shipped", &SettingsDocument::default(), &registry),
        Err(ProfileError::Reserved { .. })
    ));

    // It is how an analyst gets back: a document full of stored values
    // switches to one with none, and every setting resolves to its default.
    let mut store = SettingsStore::open(scratch.join("settings.json"));
    store.set("data", "poll_seconds", SettingValue::Int(600));
    store.set(
        "map",
        "basemap_style",
        SettingValue::Text("daylight".into()),
    );
    switch(&mut store, &library, SHIPPED_NAME);
    assert_eq!(store.effective_int(&registry, "data", "poll_seconds"), 30);
    assert_eq!(
        store.effective_text(&registry, "map", "basemap_style"),
        "slate"
    );
    assert_eq!(store.value("data", "poll_seconds"), None);

    // And it can be copied, which is how someone starts from it.
    let copy = library
        .duplicate(SHIPPED_NAME, &registry)
        .expect("copy the shipped profile");
    assert_eq!(copy, "As Shipped copy");
    assert!(
        library
            .find(&copy)
            .is_some_and(|profile| !profile.is_shipped())
    );
}

/// Rename, duplicate, delete and the name rules, on real files.
#[test]
fn the_file_operations_behave_and_the_name_is_read_out_of_the_file_not_off_it() {
    let scratch = Scratch::new("ops");
    let registry = registry();
    let mut library = library(&scratch, &registry);
    let mut document = SettingsDocument::default();
    document.values.insert(
        "data".to_owned(),
        BTreeMap::from([("poll_seconds".to_owned(), json!(15))]),
    );

    let saved = library
        .save_as("  Chase   setup ", &document, &registry)
        .expect("save");
    assert_eq!(saved, "Chase setup", "the name is tidied to one line");
    assert!(matches!(
        library.save_as("chase SETUP", &document, &registry),
        Err(ProfileError::NameTaken { .. })
    ));
    assert!(matches!(
        library.save_as("   ", &document, &registry),
        Err(ProfileError::EmptyName)
    ));

    let file = library
        .find("Chase setup")
        .and_then(|profile| profile.file.clone())
        .expect("it has a file");
    assert_eq!(
        file.file_name().and_then(|name| name.to_str()),
        Some("chase-setup.json")
    );

    // A rename keeps the file and moves the name INSIDE it. The stem no
    // longer matches, and that is the point: nothing identifies a profile by
    // its file name.
    let renamed = library
        .rename("Chase setup", "Night chase", &registry)
        .expect("rename");
    assert_eq!(renamed, "Night chase");
    assert!(file.exists(), "the file must not move");
    assert!(library.find("Chase setup").is_none());
    let profile = library.find("night CHASE").expect("found by its new name");
    assert_eq!(profile.file.as_deref(), Some(file.as_path()));

    // The file is plain, readable, hand-editable JSON in the documented shape.
    let text = std::fs::read_to_string(&file).expect("read");
    let parsed: Json = serde_json::from_str(&text).expect("plain JSON");
    assert_eq!(parsed["name"], json!("Night chase"));
    assert_eq!(parsed["profile_format"], json!(1));
    assert_eq!(
        parsed["settings"]["values"]["data"]["poll_seconds"],
        json!(15)
    );
    assert!(
        text.contains('\n'),
        "a file a person is expected to edit is pretty-printed"
    );

    let copy = library
        .duplicate("Night chase", &registry)
        .expect("duplicate");
    assert_eq!(copy, "Night chase copy");
    let copy_again = library
        .duplicate("Night chase", &registry)
        .expect("duplicate");
    assert_eq!(copy_again, "Night chase copy 2");
    assert_eq!(library.profiles().len(), 4);

    library.delete(&copy, &registry).expect("delete");
    library.delete(&copy_again, &registry).expect("delete");
    assert_eq!(library.profiles().len(), 2);
    assert!(matches!(
        library.delete("Nothing", &registry),
        Err(ProfileError::NotFound { .. })
    ));
}
