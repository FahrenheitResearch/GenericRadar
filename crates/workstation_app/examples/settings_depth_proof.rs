//! The settings window, photographed offscreen in every state that matters.
//!
//! ```text
//! cargo run --release -p workstation_app --example settings_depth_proof
//! ```
//!
//! Writes PNGs to `SETTINGS_PROOF_OUT` (default `target/settings_proof`) and
//! exits. Nothing here is interactive and nothing needs a display: the window
//! is drawn through the real `settings_ui::draw_settings_window` into a real
//! egui pass and rasterised through egui's own wgpu renderer, the same path
//! `examples/theme_gallery.rs` photographs the toolbar through.
//!
//! Why offscreen rather than the live preview window: the states worth
//! looking at are combinations - page × theme × device scale × window size -
//! and a human clicking through twenty-two of them will not do it twice. The
//! ones photographed here are the ones this window can plausibly get wrong:
//!
//! * `default` - the window as it opens, on a store with nothing in it.
//! * `ungrouped` - a page that declares no subsections, which must look
//!   exactly like a list, because that is what it always was.
//! * `sections` - the long page, which declares them.
//! * `search` - a term that hits three different pages, so the grouping of
//!   results by page is visible; `search_subsection` hits one page that has
//!   subsections, so the subsection tag under each result is visible.
//! * `modified` - a page carrying several changed values, which is where the
//!   marks, the your-value/default lines and the per-row resets all land at
//!   once.
//! * `manage` - the backup-and-reset page, including a value from a page
//!   this build does not have, which the summary has to count rather than
//!   quietly drop.
//! * `confirm_page` and `imported` both carry a value under an id this build
//!   does not declare, because the words about those are the ones that were
//!   missing: the page reset discarded them without naming them, and the
//!   import deleted them without saying anything at all.
//! * `overwrite` - Export stopped by a file already sitting at the typed
//!   path, which is the only thing standing between a mistyped path and a
//!   rename over someone's colour table.
//! * `import_preview` - the same summary as `imported`, one press earlier,
//!   before anything has been applied. The pair is the thing to look at: the
//!   words are the same, the tense is not, and only one of them has already
//!   happened.
//! * `narrow` and `scaled` - a phone-shaped window and a 1.6× UI scale,
//!   which is where a fixed-width category column, an unwrapped status line
//!   or a slider label too long for the room left beside its track would
//!   clip.

// The two application modules this window needs, compiled into the example
// exactly as `src/main.rs` compiles them. The directory `#[path]` is what
// makes `theme`'s own children (`theme/bevel.rs`, `theme/palette.rs`) and
// `settings_ui`'s (`settings_ui/catalog.rs`, ...) resolve here.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod settings_ui;
    pub mod theme;
}

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::{egui, wgpu};
use settings::{SettingValue, SettingsStore};
use source::{settings_ui, theme};
use theme::{Appearance, catalog};

/// See `theme_gallery`: gamma-space bytes, so what is read back is what egui
/// asked for.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// What the window is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scene {
    /// Fresh store, no page chosen: the window as it opens.
    Default,
    /// A page that declares no subsection headings.
    Ungrouped,
    /// The page that declares them.
    Sections,
    /// A search across pages.
    Search,
    /// A search inside a page that has subsections.
    SearchSubsection,
    /// A search that matches nothing, which has to say so.
    SearchNone,
    /// A page with several values away from their defaults.
    Modified,
    /// The window's own backup-and-reset page.
    Manage,
    /// A page's reset armed: what it would discard, before it discards it.
    ConfirmPage,
    /// The whole-application reset armed.
    ConfirmAll,
    /// An import that happened, with its summary.
    Imported,
    /// An import that was refused, with the reason.
    Refused,
    /// An export stopped by a file already sitting at that path.
    Overwrite,
    /// An import read and summarised, waiting for the second press.
    ImportPreview,
    /// The Profiles surface: the list, the shipped entry, the active one, and
    /// the controls that save and switch. Not a page of knobs, so the only
    /// way to know it reads as a page is to look at it.
    Profiles,
    /// One of the pages the settings audit added, drawn in this window. The
    /// three surfaces met for the first time in this integration and nothing
    /// short of a photograph says whether an audited page looks like a page.
    AuditPage,
    /// A search that lands on rows the audit added. The window branch built
    /// search; the audit added the rows; nothing but a photograph shows a
    /// reader that typing a word reaches them.
    SearchAudit,
    /// The Data page, which the audit and the window branch both edited: the
    /// window gave it headings, the audit added a row to it, and the row
    /// belongs under a heading of its own.
    AuditSections,
}

impl Scene {
    fn id(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ungrouped => "ungrouped",
            Self::Sections => "sections",
            Self::Search => "search",
            Self::SearchSubsection => "search_subsection",
            Self::SearchNone => "search_none",
            Self::Modified => "modified",
            Self::Manage => "manage",
            Self::ConfirmPage => "confirm_page",
            Self::ConfirmAll => "confirm_all",
            Self::Imported => "imported",
            Self::Refused => "refused",
            Self::Overwrite => "overwrite",
            Self::ImportPreview => "import_preview",
            Self::Profiles => "profiles",
            Self::AuditPage => "audit_page",
            Self::AuditSections => "audit_sections",
            Self::SearchAudit => "search_audit",
        }
    }
}

/// One photograph: what, in which theme, at which device scale, in a window
/// of which logical size.
struct Shot {
    scene: Scene,
    appearance: Appearance,
    scale: f32,
    width_points: u32,
    height_points: u32,
    suffix: &'static str,
}

/// Passes per photograph: the first applies the device scale, the rest settle
/// the window, its inner panels and any animation the state started.
const PASSES: u32 = 8;

/// A canvas comfortably larger than the window's 760 × 540 default, so the
/// shot is the window rather than the window plus a wall of desktop.
const WIDE: (u32, u32) = (960, 764);
/// Phone-shaped: the window clamps itself to this and has to stay usable.
const NARROW: (u32, u32) = (400, 620);

/// The two founding benches - the daylight bench and the night bench - which
/// the scene matrix below is shot in.
///
/// Two rather than all eight registered themes, deliberately. The matrix is
/// scene x device scale x window size, and what it interrogates is LAYOUT:
/// where the search results group, whether a subsection heading survives, how
/// far a confirmation sentence wraps at 400 points. No theme moves any of
/// that - the geometry comes from the density and UI-scale axes and from the
/// window width, never from the palette - so multiplying fifty-two shots by
/// eight would buy disk and no evidence. The themes are swept separately,
/// once, at the bottom of `shots`.
fn founding() -> [Appearance; 2] {
    [Appearance::by_id("light"), Appearance::by_id("dark")]
}

fn shots() -> Vec<Shot> {
    let mut shots = Vec::new();
    // The four states the brief names, in both themes, at 1× and 2×.
    for scene in [
        Scene::Default,
        Scene::Ungrouped,
        Scene::Sections,
        Scene::Search,
        Scene::Modified,
    ] {
        for appearance in founding() {
            for scale in [1.0_f32, 2.0] {
                shots.push(Shot {
                    scene,
                    appearance,
                    scale,
                    width_points: WIDE.0,
                    height_points: WIDE.1,
                    suffix: "",
                });
            }
        }
    }
    // The remaining states at 1× in both themes: what they have to show is
    // wording and layout, and 2× of the same wording proves nothing new.
    for scene in [
        Scene::SearchSubsection,
        Scene::SearchNone,
        Scene::SearchAudit,
        Scene::Profiles,
        Scene::AuditPage,
        Scene::AuditSections,
        Scene::Manage,
        Scene::ConfirmPage,
        Scene::ConfirmAll,
        Scene::Imported,
        Scene::Refused,
        Scene::Overwrite,
        Scene::ImportPreview,
    ] {
        for appearance in founding() {
            shots.push(Shot {
                scene,
                appearance,
                scale: 1.0,
                width_points: WIDE.0,
                height_points: WIDE.1,
                suffix: "",
            });
        }
    }
    // High UI scale, and a phone-shaped window at high UI scale. 1.6× is the
    // top of the range the appearance work is opening up.
    for appearance in founding() {
        shots.push(Shot {
            scene: Scene::Modified,
            appearance,
            scale: 1.6,
            width_points: WIDE.0,
            height_points: WIDE.1,
            suffix: "_scaled",
        });
        shots.push(Shot {
            scene: Scene::Sections,
            appearance,
            scale: 1.6,
            width_points: NARROW.0,
            height_points: NARROW.1,
            suffix: "_narrow",
        });
        // The longest block of words this window prints: a summary of six
        // changed settings plus the sentence saying none of them has been
        // applied yet. If anything wraps badly or scrolls the call to action
        // out of reach, it does it here first.
        shots.push(Shot {
            scene: Scene::ImportPreview,
            appearance,
            scale: 1.6,
            width_points: NARROW.0,
            height_points: NARROW.1,
            suffix: "_narrow",
        });
        // The Data page on the same phone-shaped window. Its slider labels -
        // "Live poll interval", "History frames" - are the longest in the
        // window, and a slider's label is the one label here that cannot
        // wrap: egui draws it inside the widget's own horizontal layout with
        // `TextWrapMode::Extend`, so `horizontal_wrapped` never gets the
        // chance to drop it onto a second line the way it does a combo's.
        // Photographed before `settings_ui::slider_label_reserve` existed,
        // this page read "Live poll interva" and "History frame".
        shots.push(Shot {
            scene: Scene::AuditSections,
            appearance,
            scale: 1.6,
            width_points: NARROW.0,
            height_points: NARROW.1,
            suffix: "_narrow",
        });
    }
    // Every OTHER registered theme, once, on the page with the most ink on
    // it. What a theme CAN get wrong in this window is ink, not geometry: the
    // modified mark is painted in the accent, the your-value/default lines in
    // a dimmed foreground, and each row's own Reset sits on the page face.
    // `Modified` is the one scene that shows all three at once, so the six
    // remaining themes are photographed there and nowhere else - and all
    // eight are then on disk, because `founding()` already shot this scene in
    // the other two. (`examples/theme_gallery.rs` remains the full per-theme
    // contact sheet; this is the settings window's slice of it.)
    for theme in catalog::THEMES {
        if founding().iter().any(|bench| bench.theme.id == theme.id) {
            continue;
        }
        shots.push(Shot {
            scene: Scene::Modified,
            appearance: Appearance::by_id(theme.id),
            scale: 1.0,
            width_points: WIDE.0,
            height_points: WIDE.1,
            suffix: "",
        });
    }
    shots
}

fn main() {
    let out_dir = std::env::var_os("SETTINGS_PROOF_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/settings_proof"));
    std::fs::create_dir_all(&out_dir).expect("create output directory");

    // A config root of this proof's own, set before anything resolves one:
    // the Profiles page reads its library from `settings::profiles_dir()`,
    // and a photograph must never be of a real analyst's profiles - nor be
    // able to write into their folder.
    let config_root = std::env::temp_dir()
        .join("settings-depth-proof")
        .join("config");
    settings::set_app_config_root(&config_root);
    assert_eq!(
        settings::app_config_root(),
        config_root,
        "something resolved the config root before this proof could set it"
    );
    seed_profile_files(&settings::profiles_dir());

    let instance = wgpu::Instance::default();
    let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("a wgpu adapter; this proof needs a GPU");
    println!("adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("settings proof device"),
        ..Default::default()
    }))
    .expect("wgpu device");

    for shot in shots() {
        let width_px = (shot.width_points as f32 * shot.scale) as u32;
        let height_px = (shot.height_points as f32 * shot.scale) as u32;
        assert_eq!(
            (width_px * 4) % 256,
            0,
            "{} at {}×: {width_px} px wide is not a 256-byte readback row",
            shot.scene.id(),
            shot.scale,
        );
        let pixels = render(&device, &queue, &shot, width_px, height_px);
        // An appearance carries its theme's stored id, so the file is named
        // off it rather than off a second mapping that could drift from the
        // catalog: "light" and "dark" still name the two founding benches.
        let theme_id = shot.appearance.theme.id;
        let file = out_dir.join(format!(
            "{}_{theme_id}_{}x{}.png",
            shot.scene.id(),
            shot.scale,
            shot.suffix
        ));
        image::RgbaImage::from_raw(width_px, height_px, pixels)
            .expect("readback size matches the target")
            .save(&file)
            .expect("write PNG");
        println!("wrote {}", file.display());
    }
}

/// A store seeded for the scene, in a directory of this scene's own, so the
/// photograph does not depend on whatever is in a developer's real settings
/// file.
fn seeded_store(scene: Scene) -> SettingsStore {
    use settings_ui::catalog::keys;

    let dir = std::env::temp_dir()
        .join("settings-depth-proof")
        // No process id in the path. The window prints its settings file in
        // the status footer, so a pid here would put a different string into
        // every photograph and make two runs of this proof undiffable - which
        // is most of what a photograph is for. One run at a time, then.
        .join(scene.id());
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("settings.json");
    // Removed rather than reused: a store left over from a previous run would
    // put marks on a photograph that is supposed to have none.
    let _ = std::fs::remove_file(&path);
    let mut store = SettingsStore::open(&path);
    match scene {
        Scene::Modified | Scene::ConfirmPage => {
            store.set(
                keys::vol3d::CATEGORY,
                keys::vol3d::THRESHOLD_DBZ,
                SettingValue::Float(28.0),
            );
            store.set(
                keys::vol3d::CATEGORY,
                keys::vol3d::OPACITY,
                SettingValue::Float(0.62),
            );
            store.set(
                keys::vol3d::CATEGORY,
                keys::vol3d::QUALITY,
                SettingValue::Text("high".to_owned()),
            );
            store.set(
                keys::vol3d::CATEGORY,
                keys::vol3d::VERTICAL_EXAGGERATION,
                SettingValue::Float(3.0),
            );
            store.set(
                keys::vol3d::CATEGORY,
                keys::vol3d::SHOW_LABELS,
                SettingValue::Bool(false),
            );
            // And one on another page, so the category list's mark is visible
            // too rather than only the current page's.
            store.set(
                keys::map::CATEGORY,
                keys::map::IMAGERY_DIM,
                SettingValue::Float(0.72),
            );
        }
        Scene::Manage
        | Scene::ConfirmAll
        | Scene::Imported
        | Scene::Refused
        | Scene::Overwrite
        | Scene::ImportPreview => {
            store.set(
                keys::vol3d::CATEGORY,
                keys::vol3d::OPACITY,
                SettingValue::Float(0.62),
            );
            store.set(
                keys::map::CATEGORY,
                keys::map::IMAGERY_DIM,
                SettingValue::Float(0.72),
            );
            store.set(
                keys::map::CATEGORY,
                keys::map::SITE_MARKERS,
                SettingValue::Bool(false),
            );
            store.set(
                keys::navigation::CATEGORY,
                keys::navigation::ZOOM_PER_NOTCH,
                SettingValue::Float(1.35),
            );
            // A value from a build that has a page this one does not. The
            // manage page has to count it rather than pretend the file holds
            // only what this build declares.
            store.set("quantum_overlay", "entanglement", SettingValue::Float(0.7));
            // And one under a page this build DOES have, from a build that
            // added a knob to it. The incoming document never mentions this
            // one, so it is what the import summary has to report as kept -
            // an import used to delete it under a headline that said one
            // setting changed.
            store.set(
                keys::map::CATEGORY,
                "hologram_mode",
                SettingValue::Bool(true),
            );
        }
        Scene::Profiles => {
            // Switched to "Chase" and then changed something: the page has to
            // show both the active profile and that it has drifted from it,
            // which is the state the whole surface exists to report.
            store.set(
                settings::profiles::BOOKKEEPING_CATEGORY,
                settings::profiles::ACTIVE_SETTING,
                SettingValue::Text("Chase".to_owned()),
            );
            store.set(
                keys::units::CATEGORY,
                keys::units::DISTANCE,
                SettingValue::Text("mi".to_owned()),
            );
            store.set(
                keys::units::CATEGORY,
                keys::units::TIME_ZONE,
                SettingValue::Text("local".to_owned()),
            );
            store.set(
                keys::annotation::CATEGORY,
                keys::annotation::RING_LADDER,
                SettingValue::Text("every-50".to_owned()),
            );
            store.set(
                keys::annotation::CATEGORY,
                keys::annotation::RING_LABELS,
                SettingValue::Bool(true),
            );
            // The drift: "Chase" does not mention the sweep speed.
            store.set(
                keys::radar::CATEGORY,
                keys::radar::SWEEP_SPEED,
                SettingValue::Float(1.6),
            );
        }
        // The audit's pages with real values on them, so the modified marks
        // are on rows the audit added rather than only on older pages.
        Scene::AuditPage => {
            store.set(
                keys::annotation::CATEGORY,
                keys::annotation::RING_LADDER,
                SettingValue::Text("every-50".to_owned()),
            );
            store.set(
                keys::annotation::CATEGORY,
                keys::annotation::RING_LABELS,
                SettingValue::Bool(true),
            );
            store.set(
                keys::annotation::CATEGORY,
                keys::annotation::RANGE_DECIMALS,
                SettingValue::Int(2),
            );
        }
        Scene::AuditSections => {
            store.set(
                keys::data::CATEGORY,
                keys::data::LOOP_FRAME_MS,
                SettingValue::Int(450),
            );
            store.set(
                keys::data::CATEGORY,
                keys::data::HISTORY_MAX_FRAMES,
                SettingValue::Int(60),
            );
        }
        _ => {}
    }
    // A knob a newer build added to a page this build also has. The page
    // reset removes the page's whole stored map, so its confirmation has to
    // name this one too - it cannot appear in the list of rows, because this
    // build has no row for it.
    if scene == Scene::ConfirmPage {
        store.set(
            keys::vol3d::CATEGORY,
            "neural_isosurface",
            SettingValue::Bool(true),
        );
    }
    // The imported scene photographs the window AFTER the import landed, so
    // that the values on the page and the summary under it describe the same
    // import. A picture where the two disagreed would teach a reader the
    // wrong thing about what importing does.
    if scene == Scene::Imported {
        // Through the real merge, not a wholesale replace: the photograph has
        // to show the store an import actually leaves behind, including the
        // values under ids this build does not declare that the file never
        // mentioned.
        let registry = registry();
        let merged =
            settings::transfer::merge_values(store.document(), &incoming_document(), &registry);
        store.replace_values(merged);
    }
    store
}

/// The registry the APPLICATION runs on, not the harness one.
///
/// `catalog::registry()` is the no-theme-module fallback: its Appearance page
/// carries the toolbar style and nothing else. This example compiles `theme`,
/// so it can and must build the real thing - theme, accent, density, chrome
/// edges and UI scale on the Appearance page, above the toolbar row - or every
/// photograph below would be of a window the application never shows.
fn registry() -> settings::SettingsRegistry {
    settings_ui::full_registry(theme::settings::settings_category())
}

fn ui_state(scene: Scene) -> settings_ui::SettingsUi {
    use settings_ui::catalog::keys;

    let mut state = settings_ui::SettingsUi::default();
    match scene {
        Scene::Default => state.open = true,
        Scene::Ungrouped => state.open_category(keys::map::CATEGORY),
        Scene::Sections | Scene::Modified => state.open_category(keys::vol3d::CATEGORY),
        // Three pages carry a "speed": the radar sweep, the keyboard rates
        // and the storm motion. That is the cross-page case.
        Scene::Search => state.open_search("speed"),
        // One page, several subsections: the tag under each row is the point.
        Scene::SearchSubsection => state.open_search("opacity"),
        Scene::SearchNone => state.open_search("hodograph"),
        Scene::Manage => state.open_manage(),
        Scene::ConfirmPage => state.stage(settings_ui::ProofStage::PageResetArmed(
            keys::vol3d::CATEGORY.to_owned(),
        )),
        Scene::ConfirmAll => state.stage(settings_ui::ProofStage::ResetAllArmed),
        Scene::Imported | Scene::ImportPreview => {
            // A real summary, computed by the real code against a real
            // document: a photograph of hand-written sample text would prove
            // nothing about what an import actually says.
            let registry = registry();
            let current = seeded_store(Scene::Manage);
            let incoming = incoming_document();
            let summary = settings::transfer::summarize(
                std::path::Path::new("D:/field-kit/bench-settings.json"),
                current.document(),
                &incoming,
                &registry,
            );
            let path = "D:/field-kit/bench-settings.json".to_owned();
            // The same summary in both tenses: the first press asks, the
            // second reports. Photographed side by side so the two are read
            // together - the whole point of the preview is that the words are
            // the same and the outcome is not.
            state.stage(if scene == Scene::Imported {
                settings_ui::ProofStage::Imported(path, summary)
            } else {
                settings_ui::ProofStage::ImportPreview(path, summary)
            });
        }
        Scene::Refused => {
            let refusal = settings::ImportRefusal::TooNew {
                version: 4,
                supported: 1,
            };
            state.stage(settings_ui::ProofStage::ImportRefused(
                "D:/field-kit/bench-settings.json".to_owned(),
                refusal.to_string(),
            ));
        }
        Scene::Overwrite => state.stage(settings_ui::ProofStage::ExportWouldOverwrite(
            "D:/field-kit/bench-settings.json".to_owned(),
        )),
        Scene::Profiles => {
            // The library reads the directory the first time the page draws,
            // so the files have to be on disk before this returns. Seeded by
            // `seed_profile_files`, called once from `main`.
            state.open_category(keys::profiles::CATEGORY);
        }
        // The audit's biggest new page: ten rows that used to be constants in
        // `pane_canvas.rs`.
        Scene::AuditPage => state.open_category(keys::annotation::CATEGORY),
        // "ring" is on the audit's own page and nowhere else.
        Scene::SearchAudit => state.open_search("ring"),
        Scene::AuditSections => state.open_category(keys::data::CATEGORY),
    }
    state
}

/// Two profile files and the active pointer, so the Profiles photograph is of
/// a populated page rather than of an empty one.
///
/// Written as plain JSON through the same format the library reads, for the
/// same reason `incoming_document` writes a file: a profile assembled in
/// memory would skip the step that can refuse.
fn seed_profile_files(directory: &std::path::Path) {
    let _ = std::fs::remove_dir_all(directory);
    std::fs::create_dir_all(directory).expect("create the profiles directory");
    let write = |name: &str, body: &str| {
        std::fs::write(directory.join(name), body).expect("write a profile");
    };
    write(
        "chase.json",
        r#"{
  "profile_format": 1,
  "name": "Chase",
  "settings": {
    "version": 1,
    "values": {
      "radar": { "quality": "smooth" },
      "units": { "distance": "mi", "time_zone": "local" },
      "annotation": { "ring_ladder": "every-50", "ring_labels": true }
    }
  }
}
"#,
    );
    write(
        "office.json",
        r#"{
  "profile_format": 1,
  "name": "Office bench",
  "settings": {
    "version": 1,
    "values": {
      "radar": { "quality": "ultra" },
      "units": { "distance": "km", "clock": "24h" },
      "network": { "download_batch": 4 }
    }
  }
}
"#,
    );
}

/// A document as another bench would have written it: two values moved, one
/// sent back to its default by being left out, one under a page this build
/// does not have, and a colour table choice.
///
/// Written as text and read back through the real
/// `settings::transfer::read_document`, not assembled in memory: the point of
/// the photograph is what an imported FILE says, and a document built by hand
/// would skip the one step that can refuse.
fn incoming_document() -> settings::SettingsDocument {
    let dir = std::env::temp_dir()
        .join("settings-depth-proof")
        .join("incoming");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("bench-settings.json");
    std::fs::write(
        &path,
        concat!(
            "{\n",
            "  \"version\": 1,\n",
            "  \"values\": {\n",
            "    \"map\": { \"imagery_dim\": 0.15, \"basemap_style\": \"high-contrast\" },\n",
            "    \"vol3d\": { \"opacity\": 0.44, \"threshold_dbz\": 20.0 },\n",
            "    \"quantum_overlay\": { \"entanglement\": 0.7 }\n",
            "  },\n",
            "  \"workspace\": {\n",
            "    \"palettes\": {\n",
            "      \"reflectivity\": { \"name\": \"NWS Reflectivity\", \"rendering\": \"stepped\" }\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    )
    .expect("write the incoming document");
    settings::transfer::read_document(&path).expect("the fixture document must be readable")
}

/// One full egui pass with the settings window in it, rasterised offscreen.
fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    shot: &Shot,
    width_px: u32,
    height_px: u32,
) -> Vec<u8> {
    let ctx = egui::Context::default();
    theme::apply(&ctx, &shot.appearance);
    ctx.set_pixels_per_point(shot.scale);

    let registry = registry();
    let mut store = seeded_store(shot.scene);
    let mut state = ui_state(shot.scene);
    let mut tables = Arc::new(color_tables::ColorTableSet::default());

    // Several passes: the first applies the scale, the rest settle the
    // window's size, its inner panels and its scroll animations. The
    // font-atlas delta arrives with the FIRST pass, so texture deltas are
    // accumulated across all of them - dropping the early outputs leaves
    // every mesh sampling a texture that was never uploaded and the frame
    // comes back black.
    let mut textures = eframe::epaint::textures::TexturesDelta::default();
    let mut full_output = None;
    for pass in 0..PASSES {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(shot.width_points as f32, shot.height_points as f32),
            )),
            // A CLOCK. egui animates a scroll-into-view over wall time, and a
            // headless run whose time never advances converges on the target
            // about fourteen points per pass - which photographed as a
            // confirmation still sitting below the fold, and looked exactly
            // like the scroll not working at all. Quarter-second steps let
            // every animation this window starts finish before the shutter.
            time: Some(f64::from(pass) * 0.25),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let context = ui.ctx().clone();
            settings_ui::draw_settings_window(
                &context,
                &mut state,
                settings_ui::SettingsWindowInput {
                    registry: &registry,
                    store: &mut store,
                    color_tables: Some(&mut tables),
                    // No colour table folder: the shot must not change with
                    // whatever is in a developer's palette directory.
                    user_tables: None,
                },
            );
        });
        textures.append(std::mem::take(&mut output.textures_delta));
        full_output = Some(output);
    }
    let full_output = full_output.expect("at least one pass ran");
    assert_eq!(
        full_output.pixels_per_point, shot.scale,
        "the requested device scale must be in force"
    );
    let clipped = ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
    assert!(
        !clipped.is_empty(),
        "{} tessellated nothing; the capture would be a lie",
        shot.scene.id()
    );

    let mut renderer = Renderer::new(device, TARGET_FORMAT, RendererOptions::PREDICTABLE);
    for (id, delta) in &textures.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    let pixels = render_clipped(
        device,
        queue,
        &mut renderer,
        &clipped,
        width_px,
        height_px,
        shot.scale,
    );
    for id in &textures.free {
        renderer.free_texture(id);
    }
    pixels
}

/// Rasterise an already-tessellated frame offscreen and read it back as
/// tightly packed RGBA. Same shape as `theme_gallery`'s.
fn render_clipped(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    clipped: &[egui::ClippedPrimitive],
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> Vec<u8> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("settings proof target"),
        size: wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("settings proof readback"),
        size: u64::from(width_px) * u64::from(height_px) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let screen = ScreenDescriptor {
        size_in_pixels: [width_px, height_px],
        pixels_per_point: scale,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("settings proof"),
    });
    let extra = renderer.update_buffers(device, queue, &mut encoder, clipped, &screen);
    assert!(
        extra.is_empty(),
        "no paint callbacks in the settings window"
    );
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("settings proof pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        renderer.render(&mut pass, clipped, &screen);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width_px * 4),
                rows_per_image: Some(height_px),
            },
        },
        wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    receiver.recv().expect("map callback").expect("map read");
    let pixels = slice.get_mapped_range().to_vec();
    readback.unmap();
    pixels
}

/// Drive a future to completion on this thread; wgpu's native backends
/// resolve adapter/device requests without needing a waker.
fn pollster_block<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}
