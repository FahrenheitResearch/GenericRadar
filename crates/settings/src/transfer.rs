//! Carrying a settings document out of this machine and back into it.
//!
//! Two operations, and a policy for each.
//!
//! **Export** writes the whole document - every stored value, the workspace
//! snapshot, and every field a future build left in it - through the same
//! atomic writer the store's own saves use. Nothing is filtered on the way
//! out: a file that dropped what it did not understand would be a worse
//! backup than the original.
//!
//! **Import** applies the file's *values* and reports, in words, exactly
//! what moved. It deliberately does not replace the workspace snapshot (pane
//! layout, cameras, window geometry, last site): those are what is on screen
//! right now, and a settings import rearranging the analyst's panes under the
//! cursor is not what "import settings" means to anyone. The colour table
//! choices are the one exception, because they are edited on a settings page
//! and read as settings; the summary says so out loud rather than leaving it
//! to be discovered.
//!
//! Applying is a MERGE, not a wholesale replacement - see [`merge_values`].
//! Every value the file carries lands; every value already stored under a
//! `(category, id)` this build does not declare and the file does not
//! mention stays. Anything else would make importing your own export from a
//! newer build a silent way to destroy that build's settings, which is
//! exactly what [`crate::document`]'s forward-compatibility contract exists
//! to prevent.
//!
//! # Why importing may refuse what opening never does
//!
//! [`crate::store`] never refuses the analyst's own file: a higher
//! [`FORMAT_VERSION`] still loads, best effort, because being locked out of
//! your own settings is worse than reading part of them. Import is the
//! opposite situation. The file is one the analyst pointed at, they still
//! have everything they had, and *nothing is lost by saying no* - so a
//! document written against a wrapper shape this build does not have is
//! refused with the reason, rather than half-applied. The wrapper version is
//! bumped only for a change the document's own forward-compatibility
//! mechanisms cannot express (see [`crate::document`]), which is precisely
//! the case where "best effort" would mean "silently wrong".

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::document::{FORMAT_VERSION, SettingsDocument};
use crate::registry::SettingsRegistry;
use crate::value::SettingValue;

/// Why an import did not happen. Every variant carries what to say to the
/// analyst; [`fmt::Display`] is that sentence.
#[derive(Clone, Debug, PartialEq)]
pub enum ImportRefusal {
    /// The path could not be read at all - missing, a directory, permissions.
    Unreadable { path: PathBuf, error: String },
    /// The bytes are not JSON.
    NotJson { detail: String },
    /// Valid JSON, but not shaped like a settings document (`values` holding
    /// something other than a map of maps, say).
    NotASettingsDocument { detail: String },
    /// Written against a newer wrapper shape. See the module documentation
    /// for why this one is a refusal rather than a best effort.
    TooNew { version: u32, supported: u32 },
}

impl fmt::Display for ImportRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, error } => {
                write!(formatter, "Could not read {}: {error}", path.display())
            }
            Self::NotJson { detail } => write!(
                formatter,
                "That file is not a settings file - it is not valid JSON ({detail})."
            ),
            Self::NotASettingsDocument { detail } => write!(
                formatter,
                "That file is JSON but not a settings document ({detail})."
            ),
            Self::TooNew { version, supported } => write!(
                formatter,
                "That file was written by a newer build (format {version}; this build \
                 reads {supported}). Nothing was changed - importing it would drop or \
                 misread part of it."
            ),
        }
    }
}

/// Read a settings document from a file, or say why not.
pub fn read_document(path: &Path) -> Result<SettingsDocument, ImportRefusal> {
    let text = std::fs::read_to_string(path).map_err(|error| ImportRefusal::Unreadable {
        path: path.to_path_buf(),
        error: error.to_string(),
    })?;
    // Parsed in two steps so the refusal can tell "not JSON" from "JSON, but
    // not this". They send the analyst to two different places: one to the
    // file they picked, the other to the build that wrote it.
    let json: Json = serde_json::from_str(&text).map_err(|error| ImportRefusal::NotJson {
        detail: error.to_string(),
    })?;
    let Some(object) = json.as_object() else {
        return Err(ImportRefusal::NotASettingsDocument {
            detail: "the top level is not an object".to_owned(),
        });
    };
    // Does the file SAY it is a settings document? Asked before the struct
    // gets a chance to be tolerant, because the very mechanisms that make
    // this format forward-compatible - `#[serde(default)]` on every section
    // plus a flattened catch-all for fields this build never heard of - also
    // make almost any JSON object deserialise cleanly as "a document that
    // happens to hold no values". Without this question a colour table, a
    // GeoJSON feature collection and even `{}` were all accepted, and
    // importing one emptied the store while reporting nothing worse than how
    // many settings had changed. Tried, all three.
    //
    // Ahead of the version check, and that order was also arrived at by
    // trying it: a `package.json` carries `"version": 2`, and with the
    // version read first this build told the analyst their build manifest
    // "was written by a newer build" - a refusal either way, but one that
    // sends them somewhere there is nothing to find. A file that names none
    // of this document's sections is not a settings file at any version.
    if !object.contains_key("values") && !object.contains_key("workspace") {
        return Err(ImportRefusal::NotASettingsDocument {
            detail: "it carries no \"values\" section, and every settings file this \
                     application writes carries one"
                .to_owned(),
        });
    }
    let document: SettingsDocument =
        serde_json::from_value(json).map_err(|error| ImportRefusal::NotASettingsDocument {
            detail: error.to_string(),
        })?;
    if document.version > FORMAT_VERSION {
        return Err(ImportRefusal::TooNew {
            version: document.version,
            supported: FORMAT_VERSION,
        });
    }
    Ok(document)
}

/// The values an import should leave in the store: everything `incoming`
/// carries, plus every value already stored under a `(category, id)` this
/// build does not declare that `incoming` does not mention.
///
/// The retained half is the whole point. The store is deliberately a place
/// where a future build's values survive - [`crate::document`] states that
/// contract, the store's saves honour it, and the settings window prints a
/// line saying those values are carried through every save untouched. An
/// import that simply overwrote the value map would break all three at once,
/// silently: an analyst who ran a newer build, came back to this one and
/// re-imported their own export would lose the newer build's settings with
/// nothing on screen to say so.
///
/// A value under a declared `(category, id)` is NOT retained - the file
/// decides it, and a declared setting the file leaves out goes back to its
/// default, which is what [`summarize`] reports as "(default)".
pub fn merge_values(
    current: &SettingsDocument,
    incoming: &SettingsDocument,
    registry: &SettingsRegistry,
) -> BTreeMap<String, BTreeMap<String, Json>> {
    let mut merged = incoming.values.clone();
    for (category_id, values) in &current.values {
        for (setting_id, value) in values {
            if registry.setting(category_id, setting_id).is_some() {
                continue;
            }
            if incoming
                .values
                .get(category_id)
                .is_some_and(|incoming| incoming.contains_key(setting_id))
            {
                continue;
            }
            merged
                .entry(category_id.clone())
                .or_default()
                .insert(setting_id.clone(), value.clone());
        }
    }
    merged
}

/// Write a document to `path` with the crash safety the store's own saves
/// get: a sibling temp file, flushed, renamed over the target.
pub fn write_document(path: &Path, document: &SettingsDocument) -> io::Result<()> {
    crate::store::write_atomically(path, document)
}

/// One setting an import moves, in the words the window prints.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangedSetting {
    /// The page's label if this build declares it, otherwise the raw id.
    pub category: String,
    /// The setting's label if this build declares it, otherwise the raw id.
    pub setting: String,
    pub before: String,
    pub after: String,
    /// The imported file leaves this setting at its factory default.
    pub to_default: bool,
}

/// What an import would do, computed before it does it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportSummary {
    pub path: PathBuf,
    /// The wrapper version the file declared.
    pub version: u32,
    /// Declared settings whose effective value moves.
    pub changes: Vec<ChangedSetting>,
    /// Values in the file under a category or id this build does not declare.
    /// Carried into the store untouched - a future build reads them - but
    /// counted here so the summary is not quietly short.
    pub unknown_values: usize,
    /// Category ids behind those values, for naming them.
    pub unknown_categories: Vec<String>,
    /// Values ALREADY in the store under a category or id this build does not
    /// declare which the incoming file does not mention. [`merge_values`]
    /// keeps them; counted here so the summary says so rather than letting
    /// the analyst discover it. Nothing is lost when this is non-zero - the
    /// line exists because a silently kept value and a silently dropped one
    /// look identical from outside.
    pub retained_values: usize,
    /// Category ids behind those values, for naming them.
    pub retained_categories: Vec<String>,
    /// Colour table families the file names, e.g. `"reflectivity"`.
    pub palette_families: Vec<String>,
}

impl ImportSummary {
    /// Whether the import would change anything at all.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.palette_families.is_empty() && self.unknown_values == 0
    }

    /// One sentence for the top of the summary, for an import that has
    /// happened.
    pub fn headline(&self) -> String {
        self.sentence(true)
    }

    /// The same sentence about an import that has NOT happened yet: what the
    /// window prints when it shows this summary as a preview and waits for a
    /// second press. Tense is the whole difference, and it is the difference
    /// between a report and a question.
    pub fn preview_headline(&self) -> String {
        self.sentence(false)
    }

    fn sentence(&self, done: bool) -> String {
        if self.is_empty() {
            return format!(
                "{} holds the same settings you already have; nothing {}.",
                self.path.display(),
                if done { "changed" } else { "would change" },
            );
        }
        let moved = self.changes.len();
        let to_default = self
            .changes
            .iter()
            .filter(|change| change.to_default)
            .count();
        let mut headline = format!(
            "{} {}: {} {} {}",
            if done { "Imported" } else { "Importing" },
            self.path.display(),
            moved,
            if moved == 1 { "setting" } else { "settings" },
            if done { "changed" } else { "would change" },
        );
        if to_default > 0 {
            headline.push_str(&format!(" ({to_default} back to the default)"));
        }
        headline.push('.');
        headline
    }

    /// The detail lines, in reading order. The window prints these verbatim.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for change in &self.changes {
            // Words, not an arrow. These lines are printed in a window whose
            // fonts do not carry U+2192 - it renders as an empty box, which
            // was photographed in this exact list before it was "becomes".
            lines.push(format!(
                "{} · {}: {} becomes {}{}",
                change.category,
                change.setting,
                change.before,
                change.after,
                if change.to_default { " (default)" } else { "" }
            ));
        }
        if !self.palette_families.is_empty() {
            lines.push(format!(
                "Colour tables installed for: {}.",
                self.palette_families.join(", ")
            ));
        }
        if self.unknown_values > 0 {
            lines.push(format!(
                "{} {} to settings this build does not have ({}), and {} kept in the \
                 file untouched.",
                self.unknown_values,
                if self.unknown_values == 1 {
                    "value belongs"
                } else {
                    "values belong"
                },
                if self.unknown_categories.is_empty() {
                    "unknown ids".to_owned()
                } else {
                    self.unknown_categories.join(", ")
                },
                if self.unknown_values == 1 {
                    "is"
                } else {
                    "are"
                },
            ));
        }
        if self.retained_values > 0 {
            // Present tense, deliberately: these lines are printed both as a
            // preview of an import that has not happened and as a report of
            // one that has, and "was kept" is a lie in the first case.
            let (singular, plural) = if self.retained_values == 1 {
                ("belongs", "is")
            } else {
                ("belong", "are")
            };
            lines.push(format!(
                "{} {} already here {singular} to settings this build does not have ({}), \
                 {plural} not in that file, and {plural} kept.",
                self.retained_values,
                if self.retained_values == 1 {
                    "value"
                } else {
                    "values"
                },
                if self.retained_categories.is_empty() {
                    "unknown ids".to_owned()
                } else {
                    self.retained_categories.join(", ")
                },
            ));
        }
        lines.push(
            "Pane layout, camera positions, window geometry and the last site were not \
             imported - what is on screen stays as it is."
                .to_owned(),
        );
        lines
    }
}

/// What importing `incoming` over `current` would do. Pure: computes the
/// words, changes nothing, so the window can show them before deciding and a
/// test can read them without a store.
pub fn summarize(
    path: &Path,
    current: &SettingsDocument,
    incoming: &SettingsDocument,
    registry: &SettingsRegistry,
) -> ImportSummary {
    let mut summary = ImportSummary {
        path: path.to_path_buf(),
        version: incoming.version,
        ..ImportSummary::default()
    };

    // Every key either side mentions, so a value the import REMOVES (sending
    // a setting back to its default) is reported as loudly as one it adds.
    let mut keys: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (category, values) in current.values.iter().chain(incoming.values.iter()) {
        for id in values.keys() {
            keys.insert((category.as_str(), id.as_str()));
        }
    }

    // A key this build cannot show is either coming IN from the file (it
    // lands in the store and a future build reads it) or ALREADY HERE and
    // unmentioned by the file (it stays - see `merge_values`). Both are
    // counted; the second one used to hit `continue` and be reported by
    // nothing at all, which is how a wholesale replace managed to delete a
    // newer build's settings under a headline that said one setting changed.
    let mut unknown_categories = BTreeSet::new();
    let mut retained_categories = BTreeSet::new();
    let mut unknown_values = 0usize;
    let mut retained_values = 0usize;
    let mut count_unshowable = |category_id: &str, after_present: bool| {
        if after_present {
            unknown_values += 1;
            unknown_categories.insert(category_id.to_owned());
        } else {
            retained_values += 1;
            retained_categories.insert(category_id.to_owned());
        }
    };
    for (category_id, setting_id) in keys {
        let before_raw = raw(&current.values, category_id, setting_id);
        let after_raw = raw(&incoming.values, category_id, setting_id);
        let Some(category) = registry.category(category_id) else {
            count_unshowable(category_id, after_raw.is_some());
            continue;
        };
        let Some(spec) = category.settings.iter().find(|spec| spec.id == setting_id) else {
            count_unshowable(category_id, after_raw.is_some());
            continue;
        };
        // Compared as EFFECTIVE values, not as raw JSON: a stored `0.35` and
        // a stored `0.350000001` both resolve to the same slider, and a
        // summary that called that a change would be noise. Equally, a value
        // that is out of range in the file resolves to the same clamp both
        // sides and correctly reports no change.
        let before = spec
            .kind
            .sanitize(before_raw.and_then(SettingValue::from_json).as_ref());
        let after = spec
            .kind
            .sanitize(after_raw.and_then(SettingValue::from_json).as_ref());
        if before == after {
            continue;
        }
        summary.changes.push(ChangedSetting {
            category: category.label.clone(),
            setting: spec.label.clone(),
            before: spec.kind.display(&before),
            after: spec.kind.display(&after),
            to_default: after == spec.kind.default_value(),
        });
    }
    summary.unknown_values = unknown_values;
    summary.unknown_categories = unknown_categories.into_iter().collect();
    summary.retained_values = retained_values;
    summary.retained_categories = retained_categories.into_iter().collect();
    summary.palette_families = incoming.workspace.palettes.keys().cloned().collect();
    summary
}

fn raw<'a>(
    values: &'a BTreeMap<String, BTreeMap<String, Json>>,
    category: &str,
    id: &str,
) -> Option<&'a Json> {
    values.get(category)?.get(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ChoiceOption, SettingKind, SettingSpec, SettingsCategory, SliderFloor};

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("settings-transfer").join(format!(
            "{test}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn registry() -> SettingsRegistry {
        let mut registry = SettingsRegistry::new();
        registry.register(SettingsCategory::new(
            "map",
            "Map",
            vec![
                SettingSpec::new(
                    "imagery_dim",
                    "Imagery dim",
                    SettingKind::Slider {
                        min: 0.0,
                        max: 0.9,
                        default: 0.35,
                        decimals: 2,
                        unit: String::new(),
                        floor: SliderFloor::Number,
                    },
                ),
                SettingSpec::new(
                    "basemap_style",
                    "Basemap",
                    SettingKind::Choice {
                        options: vec![
                            ChoiceOption::new("slate", "Slate Dark"),
                            ChoiceOption::new("daylight", "Daylight"),
                        ],
                        default_id: "slate".to_owned(),
                    },
                ),
            ],
        ));
        registry
    }

    fn document(values: Json) -> SettingsDocument {
        serde_json::from_value(serde_json::json!({ "version": 1, "values": values }))
            .expect("fixture document")
    }

    #[test]
    fn a_file_that_is_not_json_is_refused_by_name_and_nothing_is_applied() {
        let dir = scratch("not-json");
        let path = dir.join("junk.json");
        std::fs::write(&path, "this is not json at all").expect("write");
        let refusal = read_document(&path).expect_err("must refuse");
        assert!(matches!(refusal, ImportRefusal::NotJson { .. }));
        assert!(refusal.to_string().contains("not valid JSON"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_that_is_not_a_settings_document_is_refused_with_the_reason() {
        let dir = scratch("not-a-document");
        let path = dir.join("array.json");
        std::fs::write(&path, "[1, 2, 3]").expect("write");
        assert!(matches!(
            read_document(&path),
            Err(ImportRefusal::NotASettingsDocument { .. })
        ));

        let path = dir.join("wrong-shape.json");
        std::fs::write(&path, r#"{"version": 1, "values": ["map"]}"#).expect("write");
        let refusal = read_document(&path).expect_err("must refuse");
        assert!(matches!(
            refusal,
            ImportRefusal::NotASettingsDocument { .. }
        ));
        assert!(refusal.to_string().contains("not a settings document"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The likeliest wrong file is not junk - it is another perfectly valid
    /// JSON file that happens to be lying in the same folder. Each of these
    /// deserialised cleanly as "a settings document with no values" before
    /// `read_document` asked whether the file names a section, and importing
    /// one emptied the store.
    #[test]
    fn ordinary_json_that_is_not_a_settings_file_is_refused_rather_than_read_as_an_empty_one() {
        let dir = scratch("not-a-settings-file");
        for (name, text) in [
            (
                "Ramp Velocity.json",
                r#"{"stops":[{"value":-60,"rgb":[0,0,0]}],"name":"Ramp Velocity","family":"velocity"}"#,
            ),
            (
                "track.json",
                r#"{"type":"FeatureCollection","features":[]}"#,
            ),
            ("empty.json", "{}"),
            (
                "package.json",
                r#"{"name":"tools","version":2,"scripts":{"build":"tsc"}}"#,
            ),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("write");
            let refusal = read_document(&path)
                .err()
                .unwrap_or_else(|| panic!("{name} must be refused, not read as an empty document"));
            assert!(
                matches!(refusal, ImportRefusal::NotASettingsDocument { .. }),
                "{name}: {refusal:?}"
            );
            assert!(
                refusal.to_string().contains("not a settings document"),
                "{name}: {refusal}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A settings file with nothing in it yet is still a settings file, and
    /// the guard above must not start refusing those.
    #[test]
    fn an_empty_but_genuine_settings_document_still_reads() {
        let dir = scratch("empty-settings-file");
        let path = dir.join("settings.json");
        write_document(&path, &SettingsDocument::default()).expect("write");
        let read_back = read_document(&path).expect("a document this application wrote");
        assert_eq!(read_back, SettingsDocument::default());

        // And one hand-written with only the section that carries values.
        let path = dir.join("hand-written.json");
        std::fs::write(&path, r#"{"values": {"map": {"imagery_dim": 0.5}}}"#).expect("write");
        let read_back = read_document(&path).expect("a hand-written settings file");
        assert_eq!(
            read_back.values["map"]["imagery_dim"],
            serde_json::json!(0.5)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The forward-compatibility contract, stated in `document.rs`, applied
    /// to import: a value this build cannot show that the file does not
    /// mention is still here afterwards.
    #[test]
    fn importing_keeps_the_values_this_build_cannot_show_that_the_file_does_not_mention() {
        let registry = registry();
        let current = document(serde_json::json!({
            "map": { "imagery_dim": 0.6, "hologram_mode": true },
            "quantum_overlay": { "entanglement": 0.7 }
        }));
        // An ordinary document from this build: it simply has never heard of
        // either of those two keys.
        let incoming = document(serde_json::json!({ "map": { "imagery_dim": 0.1 } }));

        let merged = merge_values(&current, &incoming, &registry);
        assert_eq!(merged["map"]["imagery_dim"], serde_json::json!(0.1));
        assert_eq!(
            merged["map"]["hologram_mode"],
            serde_json::json!(true),
            "an id this build does not declare must survive the import"
        );
        assert_eq!(
            merged["quantum_overlay"]["entanglement"],
            serde_json::json!(0.7),
            "a whole page this build does not have must survive the import"
        );
        assert!(
            !merged["map"].contains_key("basemap_style"),
            "a DECLARED setting the file leaves out still goes back to its default"
        );

        let summary = summarize(Path::new("in.json"), &current, &incoming, &registry);
        assert_eq!(summary.retained_values, 2);
        assert_eq!(summary.retained_categories, ["map", "quantum_overlay"]);
        let joined = summary.lines().join(" | ");
        assert!(
            joined.contains("quantum_overlay") && joined.contains("kept"),
            "the summary must say the values it kept: {joined}"
        );
    }

    /// The file's own unknown value wins over the stored one, and is counted
    /// as arriving rather than as being kept.
    #[test]
    fn a_value_the_file_carries_replaces_the_stored_one_even_where_this_build_cannot_show_it() {
        let registry = registry();
        let current = document(serde_json::json!({ "quantum_overlay": { "entanglement": 0.7 } }));
        let incoming = document(serde_json::json!({ "quantum_overlay": { "entanglement": 0.2 } }));
        let merged = merge_values(&current, &incoming, &registry);
        assert_eq!(
            merged["quantum_overlay"]["entanglement"],
            serde_json::json!(0.2)
        );
        let summary = summarize(Path::new("in.json"), &current, &incoming, &registry);
        assert_eq!(summary.unknown_values, 1);
        assert_eq!(summary.retained_values, 0);
    }

    #[test]
    fn a_document_from_a_newer_wrapper_version_is_refused_and_says_both_versions() {
        let dir = scratch("too-new");
        let path = dir.join("future.json");
        std::fs::write(
            &path,
            format!(r#"{{"version": {}, "values": {{}}}}"#, FORMAT_VERSION + 3),
        )
        .expect("write");
        let refusal = read_document(&path).expect_err("must refuse");
        assert_eq!(
            refusal,
            ImportRefusal::TooNew {
                version: FORMAT_VERSION + 3,
                supported: FORMAT_VERSION,
            }
        );
        let words = refusal.to_string();
        assert!(words.contains("newer build"), "{words}");
        assert!(words.contains("Nothing was changed"), "{words}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_refused_with_its_path() {
        let path = scratch("missing").join("nope.json");
        let refusal = read_document(&path).expect_err("must refuse");
        assert!(matches!(refusal, ImportRefusal::Unreadable { .. }));
        assert!(refusal.to_string().contains("nope.json"), "{refusal}");
    }

    #[test]
    fn export_then_import_round_trips_a_document_byte_for_byte() {
        let dir = scratch("round-trip");
        let path = dir.join("export.json");
        let original = document(serde_json::json!({
            "map": { "imagery_dim": 0.6, "basemap_style": "daylight" },
            "future_page": { "unknown_knob": 3 }
        }));
        write_document(&path, &original).expect("export");
        let read_back = read_document(&path).expect("import");
        assert_eq!(read_back, original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_names_every_setting_that_moves_in_both_directions() {
        let registry = registry();
        let current = document(serde_json::json!({
            "map": { "imagery_dim": 0.6, "basemap_style": "daylight" }
        }));
        // The incoming file changes one, sends one back to its default by
        // omitting it, and carries a value for a page this build lacks.
        let incoming = document(serde_json::json!({
            "map": { "imagery_dim": 0.1 },
            "quantum": { "entanglement": 0.7 }
        }));
        let summary = summarize(Path::new("in.json"), &current, &incoming, &registry);

        assert!(!summary.is_empty());
        assert_eq!(summary.changes.len(), 2);
        let joined = summary.lines().join(" | ");
        assert!(joined.contains("Imagery dim"), "{joined}");
        assert!(joined.contains("Basemap"), "{joined}");
        assert!(
            joined.contains("Slate Dark") && joined.contains("(default)"),
            "an omitted value must be reported as going back to its default: {joined}"
        );
        assert!(
            joined.contains("quantum"),
            "values this build cannot show must still be counted: {joined}"
        );
        assert!(
            joined.contains("Pane layout"),
            "the summary must say what it did NOT import: {joined}"
        );
        assert_eq!(summary.unknown_values, 1);
    }

    #[test]
    fn an_identical_document_reports_no_changes_rather_than_a_list_of_none() {
        let registry = registry();
        let same = document(serde_json::json!({ "map": { "imagery_dim": 0.6 } }));
        let summary = summarize(Path::new("in.json"), &same, &same, &registry);
        assert!(summary.is_empty());
        assert!(summary.headline().contains("nothing changed"));
    }

    /// A stored value that only differs in a way the setting cannot express
    /// is not a change, and saying it is would make every import summary
    /// untrustworthy.
    #[test]
    fn a_value_that_resolves_the_same_way_is_not_reported_as_a_change() {
        let registry = registry();
        // 9.0 is far outside the 0..0.9 slider: both sides clamp to 0.9.
        let current = document(serde_json::json!({ "map": { "imagery_dim": 9.0 } }));
        let incoming = document(serde_json::json!({ "map": { "imagery_dim": 4.0 } }));
        let summary = summarize(Path::new("in.json"), &current, &incoming, &registry);
        assert!(summary.changes.is_empty(), "{:?}", summary.changes);

        // An unknown choice id resolves to the default on both sides too.
        let current = document(serde_json::json!({ "map": { "basemap_style": "vaporwave" } }));
        let incoming = document(serde_json::json!({ "map": { "basemap_style": "neon" } }));
        let summary = summarize(Path::new("in.json"), &current, &incoming, &registry);
        assert!(summary.changes.is_empty(), "{:?}", summary.changes);
    }
}
