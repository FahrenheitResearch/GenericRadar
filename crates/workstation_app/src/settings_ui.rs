//! The master settings window: every knob in the application, one place.
//!
//! Structure is a Windows-properties dialog because that is the product's
//! UI language: categories down the left, the selected page on the right, a
//! search field that cuts across pages, plain egui widgets throughout so the
//! visual theme (owned elsewhere) restyles everything without this module
//! naming a single colour.
//!
//! Division of labour:
//!
//! * [`catalog`] declares WHAT exists - categories, items, ranges, defaults.
//! * The `settings` crate stores values and persists them (debounced,
//!   atomic, forward-compatible).
//! * This file renders the registry generically. It has no per-setting
//!   code: a new item in the catalog - or a whole category contributed by
//!   another crate - appears in the window with zero changes here. That is
//!   the contract `docs/extending.md` documents.
//! * `app.rs` (human-wired; see the integration notes) applies changed
//!   values to the live application and mirrors live state back into the
//!   store.
//!
//! The one non-generic section is the colour-table picker on the Radar page:
//! palettes are structured state (name + rendering per family), not scalar
//! knobs, so they edit the live [`ColorTableSet`] directly and persist
//! through the workspace snapshot - see [`palettes`].
//!
//! Mobile is a standing requirement: every affordance here is visible and
//! tappable (help is inline text, never hover), and interactive rows enforce
//! a minimum 24-point hit height.

// Explicit child paths, not the defaults. This module is compiled in two
// homes - as `workstation_app::settings_ui` once the human-owned `mod`
// wiring lands, and until then via the `#[path]` include in
// `crates/settings/tests/workstation_settings_ui.rs` - and a `#[path]`-loaded
// module resolves DEFAULT child paths beside the loaded file (mod-rs
// semantics), which in the harness is `src/`, where `palettes.rs` names a
// different, pre-existing module of the application. An explicit
// `settings_ui/…` path resolves identically in both homes (verified
// empirically both ways); do not "simplify" these back to bare `pub mod`.
#[path = "settings_ui/catalog.rs"]
pub mod catalog;
#[path = "settings_ui/palettes.rs"]
pub mod palettes;
#[path = "settings_ui/sync.rs"]
pub mod sync;

use std::sync::Arc;

use color_tables::{ColorTableFamily, ColorTableSet};
use eframe::egui;
use settings::{
    LoadStatus, SettingKind, SettingSpec, SettingValue, SettingsRegistry, SettingsStore,
};

/// Minimum hit-target height for interactive rows, in points. 24 pt is the
/// floor the mobile requirement sets for touch.
const MIN_INTERACT_HEIGHT: f32 = 24.0;

/// Window state that survives between frames. Owned by `WorkstationApp`.
#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    selected_category: Option<String>,
    search: String,
}

impl SettingsUi {
    /// Open the window on a given category page - for a control that deep
    /// links into settings (a gear on the 3D window opening the 3D page) and
    /// for the preview example. An unknown id opens the first page.
    pub fn open_category(&mut self, id: &str) {
        self.open = true;
        self.selected_category = Some(id.to_owned());
        self.search.clear();
    }

    /// Open the window with a search already running - the "find a setting"
    /// deep link.
    pub fn open_search(&mut self, term: &str) {
        self.open = true;
        self.search = term.to_owned();
    }
}

/// Everything the window needs for one frame.
pub struct SettingsWindowInput<'a> {
    pub registry: &'a SettingsRegistry,
    pub store: &'a mut SettingsStore,
    /// The live colour tables, edited by the Radar page's palette section.
    /// `None` hides that section (a caller that does not own tables).
    pub color_tables: Option<&'a mut Arc<ColorTableSet>>,
}

/// What changed this frame, for the caller to apply to live state.
#[derive(Default)]
pub struct SettingsOutcome {
    /// `(category id, setting id)` for every value that changed, including
    /// every setting of a category whose defaults were restored. The caller
    /// matches on `catalog::keys` constants and applies each.
    pub changed: Vec<(String, String)>,
    /// The palette section installed a different colour table. The caller
    /// bumps its palette clock and re-renders.
    pub palette_changed: bool,
}

/// Draw the window. Call every frame; cheap when closed.
pub fn draw_settings_window(
    context: &egui::Context,
    state: &mut SettingsUi,
    input: SettingsWindowInput<'_>,
) -> SettingsOutcome {
    let mut outcome = SettingsOutcome::default();
    if !state.open {
        return outcome;
    }
    let SettingsWindowInput {
        registry,
        store,
        color_tables,
    } = input;
    let mut open = state.open;
    // The window must never outgrow the display - either axis: long pages
    // (3D Volume) scroll inside it instead, the status footer stays on
    // screen, and on a phone-width display (mobile is a standing
    // requirement) the window narrows rather than running off the edge.
    let screen = context.content_rect();
    let max_width = (screen.width() - 24.0).clamp(280.0, 940.0);
    let max_height = (screen.height() - 48.0).max(300.0);
    egui::Window::new("Settings")
        .open(&mut open)
        .default_size([760.0_f32.min(max_width), 540.0])
        .max_size([max_width, max_height])
        .resizable(true)
        .show(context, |ui| {
            ui.spacing_mut().interact_size.y =
                ui.spacing().interact_size.y.max(MIN_INTERACT_HEIGHT);

            // Panels-inside-the-window, so the search strip and the status
            // footer reserve their space FIRST and the page gets the rest.
            // Before this, the page scroll area computed its own height and
            // on the long pages squeezed the footer off the bottom edge -
            // seen in the preview screenshots, not hypothesised.
            egui::Panel::top("settings-search").show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Search");
                    // Sized to the touch floor for the same reason as the Text
                    // rows below: a TextEdit's own height comes from the text
                    // galley, not from `interact_size`.
                    ui.add_sized(
                        [220.0, MIN_INTERACT_HEIGHT],
                        egui::TextEdit::singleline(&mut state.search)
                            .hint_text("setting name or description"),
                    );
                    if !state.search.is_empty() && ui.button("Clear").clicked() {
                        state.search.clear();
                    }
                });
            });
            egui::Panel::bottom("settings-footer").show_inside(ui, |ui| {
                ui.label(egui::RichText::new(store_status_line(store)).small().weak());
            });
            egui::Panel::left("settings-categories")
                .resizable(false)
                .exact_size(170.0)
                .show_inside(ui, |ui| {
                    for category in registry.categories() {
                        let selected = state
                            .selected_category
                            .as_deref()
                            .map(|id| id == category.id)
                            .unwrap_or(false);
                        if ui.selectable_label(selected, &category.label).clicked() {
                            state.selected_category = Some(category.id.clone());
                            state.search.clear();
                        }
                    }
                });
            let search = state.search.trim().to_lowercase();
            egui::CentralPanel::default().show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings-page")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !search.is_empty() {
                            draw_search_results(ui, registry, store, &search, &mut outcome);
                        } else {
                            // Filter, not just or_else: a deep link to a
                            // category that does not exist (or a stale id
                            // from a build that dropped a page) must open the
                            // first page, never a silent blank one.
                            let selected = state
                                .selected_category
                                .clone()
                                .filter(|id| registry.category(id).is_some())
                                .or_else(|| {
                                    registry
                                        .categories()
                                        .first()
                                        .map(|category| category.id.clone())
                                });
                            if let Some(category_id) = selected {
                                state.selected_category = Some(category_id.clone());
                                draw_category_page(
                                    ui,
                                    registry,
                                    store,
                                    &category_id,
                                    color_tables,
                                    &mut outcome,
                                );
                            }
                        }
                    });
            });
        });
    state.open = open;
    outcome
}

/// One category page: its rows, its palette section if it is the Radar page,
/// and its restore-defaults footer.
fn draw_category_page(
    ui: &mut egui::Ui,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    category_id: &str,
    color_tables: Option<&mut Arc<ColorTableSet>>,
    outcome: &mut SettingsOutcome,
) {
    let Some(category) = registry.category(category_id) else {
        return;
    };
    ui.heading(&category.label);
    ui.add_space(4.0);
    for spec in &category.settings {
        draw_setting_row(ui, store, category_id, spec, outcome);
    }
    let mut color_tables = color_tables;
    if category_id == catalog::keys::radar::CATEGORY
        && let Some(tables) = color_tables.as_deref_mut()
    {
        draw_palette_section(ui, store, tables, outcome);
    }
    ui.add_space(8.0);
    if ui
        .button(format!("Restore {} defaults", category.label))
        .clicked()
    {
        store.reset_category(category_id);
        for spec in &category.settings {
            outcome
                .changed
                .push((category_id.to_owned(), spec.id.clone()));
        }
        // The colour tables sit on this same page, under this same button. A
        // "Restore Radar defaults" that reset every slider but left a
        // non-default velocity table installed would be quietly lying.
        if category_id == catalog::keys::radar::CATEGORY
            && let Some(tables) = color_tables
            && restore_default_palettes(store, tables)
        {
            outcome.palette_changed = true;
        }
    }
}

/// Reset the live colour tables to the shipped defaults and persist that
/// through the workspace snapshot. Returns whether the live set actually
/// changed - the caller re-renders only when it did. Free of UI types so the
/// behaviour is pinned by a plain test.
fn restore_default_palettes(store: &mut SettingsStore, tables: &mut Arc<ColorTableSet>) -> bool {
    let defaults = ColorTableSet::default();
    let live_changed = **tables != defaults;
    if live_changed {
        *tables = Arc::new(defaults);
    }
    let mut workspace = store.workspace().clone();
    workspace.palettes = palettes::capture_palettes(tables);
    store.set_workspace(workspace);
    live_changed
}

/// Search results: matching rows from every category, grouped under their
/// category's name, fully editable in place.
fn draw_search_results(
    ui: &mut egui::Ui,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    needle: &str,
    outcome: &mut SettingsOutcome,
) {
    let mut any = false;
    for category in registry.categories() {
        let matching: Vec<&SettingSpec> = category
            .settings
            .iter()
            .filter(|spec| spec_matches(spec, needle))
            .collect();
        if matching.is_empty() {
            continue;
        }
        any = true;
        ui.heading(&category.label);
        ui.add_space(4.0);
        for spec in matching {
            draw_setting_row(ui, store, &category.id, spec, outcome);
        }
    }
    if !any {
        ui.label("No settings match.");
    }
}

/// Case-insensitive match on the words a person would search by.
fn spec_matches(spec: &SettingSpec, needle: &str) -> bool {
    spec.label.to_lowercase().contains(needle)
        || spec.help.to_lowercase().contains(needle)
        || spec.id.contains(needle)
}

/// One setting: its control, its inline help, and a reset affordance when a
/// stored value overrides the default. Everything visible - nothing here is
/// hover-only, because hover does not exist on glass.
fn draw_setting_row(
    ui: &mut egui::Ui,
    store: &mut SettingsStore,
    category_id: &str,
    spec: &SettingSpec,
    outcome: &mut SettingsOutcome,
) {
    let salt = egui::Id::new(("settings-item", category_id, spec.id.as_str()));
    let effective = spec
        .kind
        .sanitize(store.value(category_id, &spec.id).as_ref());
    let mut set_value: Option<SettingValue> = None;
    let mut reset = false;

    ui.add_enabled_ui(spec.enabled, |ui| {
        ui.horizontal(|ui| {
            match &spec.kind {
                SettingKind::Toggle { .. } => {
                    let mut value = effective.as_bool().unwrap_or_default();
                    if ui.checkbox(&mut value, &spec.label).changed() {
                        set_value = Some(SettingValue::Bool(value));
                    }
                }
                SettingKind::Slider {
                    min,
                    max,
                    decimals,
                    unit,
                    ..
                } => {
                    let mut value = effective.as_float().unwrap_or_default();
                    let mut slider = egui::Slider::new(&mut value, *min..=*max)
                        .text(&spec.label)
                        .fixed_decimals(usize::from(*decimals));
                    if !unit.is_empty() {
                        slider = slider.suffix(format!(" {unit}"));
                    }
                    if ui.add(slider).changed() {
                        set_value = Some(SettingValue::Float(value));
                    }
                }
                SettingKind::Integer { min, max, unit, .. } => {
                    let mut value = effective.as_int().unwrap_or_default();
                    let mut slider = egui::Slider::new(&mut value, *min..=*max).text(&spec.label);
                    if !unit.is_empty() {
                        slider = slider.suffix(format!(" {unit}"));
                    }
                    if ui.add(slider).changed() {
                        set_value = Some(SettingValue::Int(value));
                    }
                }
                SettingKind::Choice { options, .. } => {
                    let current_id = effective.as_text().unwrap_or_default().to_owned();
                    let current_label = options
                        .iter()
                        .find(|option| option.id == current_id)
                        .map(|option| option.label.as_str())
                        .unwrap_or(current_id.as_str());
                    egui::ComboBox::from_id_salt(salt)
                        .selected_text(current_label)
                        .width(210.0)
                        .show_ui(ui, |ui| {
                            for option in options {
                                let chosen = option.id == current_id;
                                if ui.selectable_label(chosen, &option.label).clicked() && !chosen {
                                    set_value = Some(SettingValue::Text(option.id.clone()));
                                }
                            }
                        });
                    ui.label(&spec.label);
                }
                SettingKind::Text {
                    placeholder,
                    max_len,
                    ..
                } => {
                    let mut value = effective.as_text().unwrap_or_default().to_owned();
                    let edit = egui::TextEdit::singleline(&mut value)
                        .hint_text(placeholder.as_str())
                        .char_limit(*max_len);
                    // `add_sized`, not `add`: a singleline TextEdit sizes its
                    // height from the text galley (under the 24 pt touch
                    // floor), not from `interact_size`.
                    if ui.add_sized([120.0, MIN_INTERACT_HEIGHT], edit).changed() {
                        set_value = Some(SettingValue::Text(value));
                    }
                    ui.label(&spec.label);
                }
            }
            // Visible only when a stored value exists, i.e. the row differs
            // from factory in the file. A full-height button, not a small one
            // and not an icon on hover: `small_button` sizes to the text line
            // (~18 pt) and would undercut the 24 pt touch floor this module
            // promises.
            if store.value(category_id, &spec.id).is_some() && ui.button("Reset").clicked() {
                reset = true;
            }
        });
        if !spec.help.is_empty() {
            ui.label(egui::RichText::new(spec.help.as_str()).small().weak());
        }
        if !spec.enabled {
            ui.label(
                egui::RichText::new(
                    "Declared ahead of its feature; the stored choice takes effect when \
                     the wiring lands.",
                )
                .small()
                .weak(),
            );
        }
    });
    ui.add_space(6.0);

    if reset {
        if store.reset(category_id, &spec.id) {
            outcome
                .changed
                .push((category_id.to_owned(), spec.id.clone()));
        }
    } else if let Some(value) = set_value
        && store.set(category_id, &spec.id, value)
    {
        outcome
            .changed
            .push((category_id.to_owned(), spec.id.clone()));
    }
}

/// The Radar page's colour-table pickers: one row per family, offering the
/// same list the toolbar picker offers (`palette_offers_for_family`, which
/// includes the smooth/stepped flip as its last row). Changes install into
/// the live set immediately and persist through the workspace snapshot.
fn draw_palette_section(
    ui: &mut egui::Ui,
    store: &mut SettingsStore,
    tables: &mut Arc<ColorTableSet>,
    outcome: &mut SettingsOutcome,
) {
    ui.add_space(6.0);
    ui.strong("Colour tables");
    ui.label(
        egui::RichText::new(
            "Per measurement family; installing a velocity table moves VEL, DVEL, SRV \
             and DSRV together. The last row of each list is the selected palette \
             redrawn the other way: smooth or stepped.",
        )
        .small()
        .weak(),
    );
    ui.add_space(4.0);
    let mut changed = false;
    for family in ColorTableFamily::ALL {
        let installed = tables.for_family(family).clone();
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("settings-palette", palettes::family_id(family)))
                .selected_text(installed.name().to_owned())
                .width(230.0)
                .show_ui(ui, |ui| {
                    for table in color_tables::palette_offers_for_family(family, &installed) {
                        let chosen = table.name() == installed.name();
                        if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                            Arc::make_mut(tables).set_family(family, table);
                            changed = true;
                        }
                    }
                });
            ui.label(family.label());
        });
    }
    if changed {
        let mut workspace = store.workspace().clone();
        workspace.palettes = palettes::capture_palettes(tables);
        store.set_workspace(workspace);
        outcome.palette_changed = true;
    }
}

/// The footer: where the file is, how it loaded, whether it is saved. This
/// is the line that makes persistence auditable instead of assumed.
fn store_status_line(store: &SettingsStore) -> String {
    let mut line = format!("Settings file: {}", store.path().display());
    match store.status() {
        LoadStatus::Defaults => line.push_str(" · first run, defaults"),
        LoadStatus::Loaded => {}
        LoadStatus::Recovered { backup } => match backup {
            Some(backup) => {
                line.push_str(&format!(
                    " · previous file was corrupt, moved to {}",
                    backup.display()
                ));
            }
            None => line.push_str(" · previous file was corrupt and could not be moved aside"),
        },
        LoadStatus::Unreadable { error } => {
            line.push_str(&format!(
                " · UNREADABLE ({error}); changes will not be saved automatically"
            ));
        }
    }
    if let Some(error) = store.last_save_error() {
        line.push_str(&format!(" · last save FAILED: {error}"));
    } else if store.is_dirty() {
        line.push_str(" · unsaved changes (autosaves shortly)");
    } else if !matches!(store.status(), LoadStatus::Defaults) {
        line.push_str(" · saved");
    }
    if settings::is_fallback_root(store.path()) {
        line.push_str(" · WARNING: stored under the system temp directory");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_labels_help_and_ids_case_insensitively() {
        let registry = catalog::registry();
        let spec = registry
            .setting(
                catalog::keys::navigation::CATEGORY,
                catalog::keys::navigation::ZOOM_PER_NOTCH,
            )
            .expect("zoom_per_notch is declared");
        assert!(spec_matches(spec, "zoom"));
        assert!(spec_matches(spec, "notch"));
        assert!(spec_matches(spec, "wheel click"), "matches help text");
        assert!(spec_matches(spec, "zoom_per_notch"), "matches the raw id");
        assert!(!spec_matches(spec, "differential phase"));
    }

    #[test]
    fn restore_radar_defaults_also_restores_the_colour_tables() {
        let dir = std::env::temp_dir().join(format!(
            "settings-ui-palette-restore-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let mut store = SettingsStore::open(dir.join("settings.json"));

        // A non-default velocity table, in the non-default rendering.
        let mut tables = Arc::new(ColorTableSet::default());
        let pick = color_tables::builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .nth(2)
            .expect("velocity catalog depth")
            .rendered(color_tables::TableRendering::Stepped);
        Arc::make_mut(&mut tables).set_family(ColorTableFamily::Velocity, pick);

        assert!(
            restore_default_palettes(&mut store, &mut tables),
            "a non-default set must report a change"
        );
        assert_eq!(*tables, ColorTableSet::default());
        // The stored snapshot agrees with the live set, so the next launch
        // restores the same defaults.
        let restored = palettes::apply_palettes(&store.workspace().palettes);
        assert_eq!(restored, ColorTableSet::default());
        // Already default: no spurious re-render.
        assert!(!restore_default_palettes(&mut store, &mut tables));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_status_line_reports_recovery_and_save_failures_in_words() {
        let dir = std::env::temp_dir().join(format!(
            "settings-ui-status-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ definitely not json").expect("write corrupt file");
        let store = SettingsStore::open(&path);
        let line = store_status_line(&store);
        assert!(line.contains("corrupt"), "{line}");
        assert!(line.contains("settings.json.corrupt"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
