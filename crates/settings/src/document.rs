//! The on-disk shape of the settings file, built for two-way compatibility.
//!
//! The contract: **a file written by a future version of this application
//! must survive a round trip through this one, and vice versa.** Two
//! mechanisms deliver it:
//!
//! * every map is keyed by strings, so values under categories and ids this
//!   build has never heard of are carried verbatim;
//! * every struct carries `#[serde(flatten)] unknown`, which catches fields a
//!   future build added and re-emits them on save. Without the flatten, serde
//!   would *accept* the unknown field and then silently drop it on the next
//!   write - tolerant reading is not the same thing as preservation.
//!
//! Everything else is `#[serde(default)]`, so a file from an older build that
//! lacks a section deserialises to that section's default instead of failing
//! the whole document.
//!
//! Snapshot fields use plain strings and numbers (`"layout": "four"`,
//! `"tilt_mode": "cut"`) rather than serde's enum encodings, so the file is
//! hand-readable, diffs cleanly, and never breaks because a Rust enum was
//! renamed. The string vocabularies are defined by the application's sync
//! layer and resolved defensively there: an unknown string falls back to a
//! default, never to a blank pane.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as Json};

/// Version of the file's *wrapper* shape (the field names in this module),
/// not of its contents: content evolves compatibly through the mechanisms
/// above without a version bump. Bump this only for a change the mechanisms
/// cannot express, together with an explicit migration in the store.
pub const FORMAT_VERSION: u32 = 1;

/// The whole settings file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SettingsDocument {
    /// See [`FORMAT_VERSION`]. A file with a *higher* version still loads -
    /// best effort, nothing understood is refused - and saving preserves the
    /// higher number so the future build that wrote it does not mistake the
    /// file for an old one.
    #[serde(default)]
    pub version: u32,
    /// Registry-backed values: `values[category id][setting id] = scalar`.
    /// Kept as raw JSON so shapes this build cannot read are still carried.
    #[serde(default)]
    pub values: BTreeMap<String, BTreeMap<String, Json>>,
    /// Structured session state that is not a scalar knob.
    #[serde(default)]
    pub workspace: WorkspaceSnapshot,
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

impl Default for SettingsDocument {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            values: BTreeMap::new(),
            workspace: WorkspaceSnapshot::default(),
            unknown: JsonMap::new(),
        }
    }
}

/// What the analyst had on screen: layout, panes, palettes, site, window.
///
/// Every field is optional or defaulted. `None` always means "this file does
/// not say", and the application keeps whatever it would have done anyway.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    /// Pane layout id: `"one"`, `"two-vertical"`, `"two-horizontal"`, `"four"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Index of the active pane, 0-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_pane: Option<u8>,
    /// One entry per pane, in pane order. Shorter than the pane count is
    /// fine - the remaining panes keep their defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneSnapshot>,
    /// Colour table choice per family: family id (the application defines
    /// the vocabulary, e.g. `"reflectivity"`) to palette name and rendering.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub palettes: BTreeMap<String, PaletteChoice>,
    /// Per-producer-field colour choices. Keys are the exact namespaced
    /// product ids the application stores in panes (for example
    /// `"SOURCE_FIELD:ZH1C"`), never a shared colour-table family.
    ///
    /// An absent key means the source field follows its observed finite range
    /// automatically. A present key records an explicit palette and range,
    /// so adjusting one research-radar channel cannot recolour another.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_field_palettes: BTreeMap<String, SourceFieldPaletteChoice>,
    /// The live site last viewed, e.g. `"KTLX"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_site: Option<String>,
    /// Whether the warnings layer was shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_warnings: Option<bool>,
    /// Outer window geometry, for reopening where the analyst left off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowSnapshot>,
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

/// One pane's saved intent.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PaneSnapshot {
    /// Product registry id, e.g. `"REF"`. Resolved through the product
    /// registry on load; an unknown id resets to the default product *with a
    /// visible status line*, never silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    /// `"lowest"`, `"cut"` or `"nearest"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tilt_mode: Option<String>,
    /// Meaning depends on `tilt_mode`: the cut index for `"cut"`, the target
    /// elevation in degrees for `"nearest"`, unused for `"lowest"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tilt_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_east_km: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_north_km: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_per_point: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_rad: Option<f64>,
    /// Whether this pane's camera is in the shared link group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_linked: Option<bool>,
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

/// A colour table choice: the palette's base name (stable across the
/// smooth/stepped switch - `color_tables::ColorTable::base_name`) plus the
/// rendering it was being drawn in (`"smooth"` or `"stepped"`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PaletteChoice {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rendering: String,
    /// Which era of shipped defaults wrote this choice. The every-frame
    /// mirror stores whatever palette is installed, so when a build changes
    /// its default the store is full of names nobody picked; the generation
    /// lets the reader migrate those exactly once (a file from before the
    /// field deserializes to 0) while respecting the same name written by a
    /// deliberate pick under the current generation.
    #[serde(default)]
    pub generation: u32,
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

/// One exact producer-native field's explicit colour-table choice.
///
/// The palette itself remains an ordinary `.pal` file when the analyst saves
/// it; this snapshot keeps the stable name, current rendering, and the raw
/// producer-value span that the field was edited over. Optional bounds make
/// older and hand-edited files tolerant: an incomplete entry is ignored by
/// the application rather than blanking a pane.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SourceFieldPaletteChoice {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rendering: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f32>,
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

/// Outer window geometry in logical points.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct WindowSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub maximized: bool,
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_future_files_extra_fields_survive_a_round_trip_at_every_level() {
        // A file as a future build might write it: unknown fields at the top
        // level, inside the workspace, inside a pane, inside a palette choice,
        // plus a whole unknown category and an unknown setting id.
        let future = json!({
            "version": 9,
            "values": {
                "map": {
                    "basemap_style": "daylight",
                    "hologram_mode": true
                },
                "quantum_overlay": { "entanglement": 0.7 }
            },
            "workspace": {
                "layout": "four",
                "panes": [ { "product": "REF", "neural_annotations": [1, 2] } ],
                "palettes": { "reflectivity": { "name": "X", "rendering": "smooth", "gamut": "p3" } },
                "source_field_palettes": {
                    "SOURCE_FIELD:V1": {
                        "name": "DOW velocity",
                        "rendering": "smooth",
                        "minimum": -48.0,
                        "maximum": 48.0,
                        "future_sampler": "cyclic"
                    }
                },
                "teleporter": { "target": "KTLX" }
            },
            "biometrics": { "retina_lock": false }
        });
        let text = serde_json::to_string_pretty(&future).expect("serialise fixture");
        let document: SettingsDocument = serde_json::from_str(&text).expect("future file loads");
        let rewritten = serde_json::to_value(&document).expect("document serialises");

        assert_eq!(rewritten["version"], json!(9));
        assert_eq!(rewritten["values"]["map"]["hologram_mode"], json!(true));
        assert_eq!(
            rewritten["values"]["quantum_overlay"]["entanglement"],
            json!(0.7)
        );
        assert_eq!(
            rewritten["workspace"]["panes"][0]["neural_annotations"],
            json!([1, 2])
        );
        assert_eq!(
            rewritten["workspace"]["palettes"]["reflectivity"]["gamut"],
            json!("p3")
        );
        assert_eq!(
            rewritten["workspace"]["source_field_palettes"]["SOURCE_FIELD:V1"]["future_sampler"],
            json!("cyclic")
        );
        assert_eq!(
            rewritten["workspace"]["teleporter"]["target"],
            json!("KTLX")
        );
        assert_eq!(rewritten["biometrics"]["retina_lock"], json!(false));
    }

    #[test]
    fn a_file_from_an_older_build_with_missing_sections_loads_as_defaults() {
        let document: SettingsDocument = serde_json::from_str("{}").expect("empty object loads");
        assert_eq!(document.version, 0);
        assert!(document.values.is_empty());
        assert_eq!(document.workspace, WorkspaceSnapshot::default());
    }

    #[test]
    fn a_default_document_serialises_without_noise() {
        let text = serde_json::to_string(&SettingsDocument::default()).expect("serialises");
        // Empty optionals are skipped, so a fresh file is small and readable.
        assert_eq!(text, "{\"version\":1,\"values\":{},\"workspace\":{}}");
    }
}
