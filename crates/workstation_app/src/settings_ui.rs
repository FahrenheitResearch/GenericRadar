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
//! The non-generic sections are both on the Radar page, and both are there
//! because colour tables are not scalar knobs. The picker edits the live
//! [`ColorTableSet`] directly and persists through the workspace snapshot
//! (name + rendering per family - see [`palettes`]); under it,
//! [`draw_user_tables_section`] reports what the analyst's own colour table
//! folder holds and, more importantly, which files in it could not be read
//! and why. A palette that is in the folder and not in the picker has no
//! other way of being diagnosed from inside the application.
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

use color_tables::user::UserTableLibrary;
use color_tables::{ColorTable, ColorTableFamily, ColorTableSet};
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
    /// The Radar page's palette rows, held between frames. See
    /// [`PaletteOfferCache`]: the list is rebuilt from parsed text and
    /// cloned tables, and a combo popup asks for it every frame it is open.
    palette_offers: PaletteOfferCache,
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
    /// What the analyst's own colour table folder currently holds. The
    /// palette lists offer these after the built-ins, and the folder's
    /// parse faults are reported under them. `None` for a caller that keeps
    /// no folder - the palette section then offers the built-ins alone.
    pub user_tables: Option<&'a UserTableLibrary>,
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
    /// The user colour table section asked for the folder to be read again.
    /// The caller owns the library, so it does the scan.
    ///
    /// Also set by the colour table editor's Save and Apply, through the
    /// application: a file written from inside this process is a change the
    /// focus-regain rescan will never see, because focus was never lost.
    pub user_tables_rescan: bool,
    /// The palette section asked for a table to be opened in the colour table
    /// editor. Plain `color_tables` values rather than an editor type: this
    /// module is compiled in a second home that does not have the
    /// application's crate, so it names nothing from it.
    pub palette_edit: Option<(ColorTableFamily, ColorTable)>,
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
        user_tables,
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
                                    PaletteContext {
                                        color_tables,
                                        user_tables,
                                        offers: &mut state.palette_offers,
                                    },
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

/// The colour-table half of a page, which the generic registry rendering
/// knows nothing about: the live set the picker edits, the analyst's own
/// folder, and the rows the combos draw from. Carried as one value because
/// only the Radar page uses any of it, and because three more parameters on
/// a page-drawing function is three more chances to pass them in the wrong
/// order.
struct PaletteContext<'a> {
    color_tables: Option<&'a mut Arc<ColorTableSet>>,
    user_tables: Option<&'a UserTableLibrary>,
    offers: &'a mut PaletteOfferCache,
}

/// One category page: its rows, its palette section if it is the Radar page,
/// and its restore-defaults footer.
fn draw_category_page(
    ui: &mut egui::Ui,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    category_id: &str,
    palettes: PaletteContext<'_>,
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
    let PaletteContext {
        color_tables,
        user_tables,
        offers,
    } = palettes;
    let mut color_tables = color_tables;
    if category_id == catalog::keys::radar::CATEGORY
        && let Some(tables) = color_tables.as_deref_mut()
    {
        draw_palette_section(ui, store, tables, user_tables, offers, outcome);
        if let Some(library) = user_tables {
            draw_user_tables_section(ui, library, outcome);
        }
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
/// same list the toolbar picker offers - the shipped catalogue, then the
/// analyst's own tables for that family, then the smooth/stepped flip as the
/// last row. Changes install into the live set immediately and persist
/// through the workspace snapshot.
fn draw_palette_section(
    ui: &mut egui::Ui,
    store: &mut SettingsStore,
    tables: &mut Arc<ColorTableSet>,
    user_tables: Option<&UserTableLibrary>,
    palette_offers: &mut PaletteOfferCache,
    outcome: &mut SettingsOutcome,
) {
    ui.add_space(6.0);
    ui.strong("Colour tables");
    ui.label(
        egui::RichText::new(
            "Per measurement family; installing a velocity table moves VEL, DVEL, SRV \
             and DSRV together. Tables from your own colour table folder follow the \
             built-in ones. The last row of each list is the selected palette \
             redrawn the other way: smooth or stepped.",
        )
        .small()
        .weak(),
    );
    ui.add_space(4.0);
    let mut changed = false;
    for family in ColorTableFamily::ALL {
        let installed = tables.for_family(family).clone();
        // Taken out of the popup rather than installed inside it: the rows
        // are borrowed from the cache for the length of the loop, and
        // installing writes the set the cache's key is read from.
        let mut picked = None;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("settings-palette", palettes::family_id(family)))
                .selected_text(installed.name().to_owned())
                .width(230.0)
                .show_ui(ui, |ui| {
                    for table in palette_offers.offers(family, &installed, user_tables) {
                        let chosen = table.name() == installed.name();
                        if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                            picked = Some(table.clone());
                        }
                    }
                });
            ui.label(family.label());
            // The editor opens on whatever is installed for the family. It
            // decides for itself whether that is a table it may write over: a
            // shipped preset is duplicated, so this button is never a way to
            // lose one.
            if ui
                .button("Edit…")
                .on_hover_text(
                    "Open this table in the colour table editor. Shipped presets open as a \
                     copy - they are never overwritten.",
                )
                .clicked()
            {
                outcome.palette_edit = Some((family, installed.clone()));
            }
        });
        if let Some(table) = picked {
            Arc::make_mut(tables).set_family(family, table);
            changed = true;
        }
    }
    if changed {
        let mut workspace = store.workspace().clone();
        workspace.palettes = match user_tables {
            // Preserving, not unconditional: a stored name whose file is
            // temporarily missing must survive an unrelated palette change
            // in another family.
            Some(library) => {
                palettes::capture_palettes_preserving(tables, &workspace.palettes, library)
            }
            None => palettes::capture_palettes(tables),
        };
        store.set_workspace(workspace);
        outcome.palette_changed = true;
    }
}

/// The rows one family's picker offers, with or without a user folder behind
/// it. One function so the settings window and the toolbar cannot drift into
/// offering different lists.
pub fn palette_offers(
    family: ColorTableFamily,
    installed: &color_tables::ColorTable,
    user_tables: Option<&UserTableLibrary>,
) -> Vec<color_tables::ColorTable> {
    match user_tables {
        Some(library) => {
            color_tables::user::palette_offers_with_user_tables(family, installed, library)
        }
        None => color_tables::palette_offers_for_family(family, installed),
    }
}

/// One family's picker rows, held until something they are built from moves.
///
/// [`palette_offers`] is not cheap and every caller is inside a combo box's
/// popup, which means it runs once per frame for as long as that popup is
/// open: each built-in for the family is parsed out of its text, and each
/// table the analyst supplied is cloned whole - its stops and the per-stop
/// Oklab beside them. For a list whose contents change only when the analyst
/// installs a different palette or the colour table folder is rescanned,
/// paying that at the frame rate is pure waste, and with a large user table
/// in the folder it was a measurable slice of a 60 fps budget.
///
/// The key is exactly what the list is built from:
///
/// * the family, which decides which built-ins and which user tables;
/// * the installed table's full `name()`, which carries both the palette and
///   the rendering the whole list is drawn in, and which the flip row at the
///   bottom is derived from;
/// * the folder's scan generation, so a table dropped, edited or rescanned
///   while the popup is open appears on the next frame instead of being
///   served stale.
///
/// One cache holds one family's list, which is all any caller needs: a combo
/// popup is open one at a time.
#[derive(Debug, Default)]
pub struct PaletteOfferCache {
    held: Option<HeldOffers>,
}

#[derive(Debug)]
struct HeldOffers {
    family: ColorTableFamily,
    installed: String,
    user_generation: u64,
    tables: Vec<color_tables::ColorTable>,
    /// The base names this build *ships* in that family. Held beside the
    /// offers because the offers list cannot be asked: a table an analyst
    /// loaded from a file is appended to it and looks exactly like a preset
    /// from the outside. It is what decides whether a picker row's edit
    /// affordance opens the table or duplicates it, and recomputing it per
    /// frame would re-parse the whole catalogue at the frame rate.
    builtin: std::collections::BTreeSet<String>,
}

impl PaletteOfferCache {
    /// The rows for this family, rebuilt only if the key moved.
    pub fn offers(
        &mut self,
        family: ColorTableFamily,
        installed: &color_tables::ColorTable,
        user_tables: Option<&UserTableLibrary>,
    ) -> &[color_tables::ColorTable] {
        self.refresh(family, installed, user_tables);
        self.held
            .as_ref()
            .map_or(&[][..], |held| held.tables.as_slice())
    }

    /// Whether `base_name` is a palette this build ships in the family the
    /// held rows belong to - the question the colour table editor's Edit and
    /// Copy affordances turn on.
    ///
    /// Answered from the same cache entry [`Self::offers`] handed out, so a
    /// row and its affordance can never disagree about what that row is. A
    /// cache that has not been filled yet answers `false`: nothing has been
    /// offered, so nothing can be pressed.
    pub fn is_builtin(&self, base_name: &str) -> bool {
        self.held
            .as_ref()
            .is_some_and(|held| held.builtin.contains(base_name))
    }

    fn refresh(
        &mut self,
        family: ColorTableFamily,
        installed: &color_tables::ColorTable,
        user_tables: Option<&UserTableLibrary>,
    ) {
        let user_generation = user_tables.map_or(0, UserTableLibrary::generation);
        if self.held.as_ref().is_none_or(|held| {
            held.family != family
                || held.installed != installed.name()
                || held.user_generation != user_generation
        }) {
            self.held = Some(HeldOffers {
                family,
                installed: installed.name().to_owned(),
                user_generation,
                tables: palette_offers(family, installed, user_tables),
                builtin: color_tables::builtin_tables_for_family(family)
                    .iter()
                    .map(|table| table.base_name().to_owned())
                    .collect(),
            });
        }
    }
}

/// What the analyst's own colour table folder holds, and what it could not
/// read.
///
/// A palette that is in the folder and not in the picker is otherwise
/// undiagnosable from inside the application: the file is there, the name is
/// right, and nothing anywhere says which line the parser stopped on. This
/// is that answer, in the one window an analyst already opens to look for
/// colour tables.
fn draw_user_tables_section(
    ui: &mut egui::Ui,
    library: &UserTableLibrary,
    outcome: &mut SettingsOutcome,
) {
    ui.add_space(10.0);
    ui.strong("Your colour tables");
    ui.label(
        egui::RichText::new(format!(
            "Folder: {}. Drop a .pal file on the window to add one; \
             GR2Analyst and RadarScope palettes are read as they are.",
            library.directory().display()
        ))
        .small()
        .weak(),
    );
    ui.add_space(2.0);
    if library.tables().is_empty() {
        ui.label(egui::RichText::new("No tables loaded from that folder.").small());
    } else {
        for entry in library.tables() {
            ui.label(
                egui::RichText::new(format!(
                    "{} · {} · {}",
                    entry.display_name(),
                    entry.family().label(),
                    entry.file_name()
                ))
                .small(),
            );
        }
    }
    for fault in library.faults() {
        // The one place in this window that is allowed to shout. A file the
        // analyst put in the folder deliberately, that is being skipped
        // everywhere else, has to be visible or the feature looks broken.
        ui.label(
            egui::RichText::new(match fault.line() {
                Some(line) => format!(
                    "Skipped {} - {} (line {line})",
                    fault.file_name(),
                    fault.reason()
                ),
                None => format!("Skipped {} - {}", fault.file_name(), fault.reason()),
            })
            .small()
            .color(ui.visuals().warn_fg_color),
        );
    }
    ui.add_space(4.0);
    // The folder is normally re-read when the window regains focus, which
    // covers editing a palette in another application. This is for the case
    // that does not move focus at all - a file synced in, or an editor
    // inside this same window saving one.
    if ui.button("Rescan colour table folder").clicked() {
        outcome.user_tables_rescan = true;
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

    /// A scratch directory, unique per test, removed at the end.
    fn scratch_dir(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("settings-ui-scratch")
            .join(format!(
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

    /// Every text run a section drew, flattened - `Shape::Vec` nests.
    ///
    /// The section is drawn into a bare `Ui` rather than inside the settings
    /// window, because the window's page scrolls: what an assertion about
    /// scrolled-away rows would measure is the window's default height, not
    /// whether the section says what it must.
    fn section_texts(draw: impl FnOnce(&mut egui::Ui)) -> Vec<String> {
        fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => {
                    let text = text.galley.text().trim();
                    if !text.is_empty() {
                        found.push(text.to_owned());
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, found);
                    }
                }
                _ => {}
            }
        }
        let context = egui::Context::default();
        let mut draw = Some(draw);
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1400.0, 1400.0),
                )),
                ..Default::default()
            },
            |ui| {
                if let Some(draw) = draw.take() {
                    draw(ui);
                }
            },
        );
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut texts);
        }
        texts
    }

    /// The diagnosis a palette that will not load has to produce. Without
    /// this the analyst sees a file in their folder, no row in the picker,
    /// and nothing anywhere saying why - which is indistinguishable from the
    /// feature being broken.
    #[test]
    fn a_folder_fault_is_reported_with_its_file_and_line() {
        let dir = scratch_dir("fault-on-screen");
        std::fs::write(
            dir.join("Ramp Velocity.pal"),
            "Product: BV\nColor: -30 0 200 0\nColor: 30 200 0 0\n",
        )
        .expect("write palette");
        // Line 3 asks for a colour component of 900.
        std::fs::write(
            dir.join("wrong.pal"),
            "Product: BR\nColor: 0 0 0 0\nColor: 10 900 0 0\n",
        )
        .expect("write palette");
        let library = UserTableLibrary::open(&dir);

        let mut outcome = SettingsOutcome::default();
        let texts = section_texts(|ui| draw_user_tables_section(ui, &library, &mut outcome));
        let joined = texts.join(" | ");

        assert!(joined.contains("Your colour tables"), "{joined}");
        assert!(
            joined.contains("Ramp Velocity") && joined.contains("Velocity / SRV"),
            "the loaded table and its family must be on the page: {joined}"
        );
        assert!(
            joined.contains("wrong.pal") && joined.contains("line 3"),
            "the skipped file and its line must be on the page: {joined}"
        );
        assert!(
            joined.contains(&dir.display().to_string()),
            "the folder path must be on the page: {joined}"
        );
        assert!(
            !outcome.user_tables_rescan,
            "nothing was clicked, so nothing may have been asked for"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// The picker contract, at the one function both pickers call: the
    /// analyst's own tables are offered, after the built-ins, in the family
    /// their header put them in.
    #[test]
    fn a_user_table_is_offered_in_its_family_after_the_built_ins() {
        let dir = std::env::temp_dir().join(format!(
            "settings-ui-user-offers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(
            dir.join("Ramp Velocity.pal"),
            "Product: BV\nUnits: KTS\nColor: -60 200 0 200 60 220 220\nColor: 60 220 60 60 \
             255 255 255\n",
        )
        .expect("write palette");
        let library = UserTableLibrary::open(&dir);
        let tables = ColorTableSet::default();

        let velocity = palette_offers(
            ColorTableFamily::Velocity,
            tables.for_family(ColorTableFamily::Velocity),
            Some(&library),
        );
        let builtin_count =
            color_tables::builtin_tables_for_family(ColorTableFamily::Velocity).len();
        assert_eq!(velocity[builtin_count].base_name(), "Ramp Velocity");

        // And nowhere else: a velocity palette offered under reflectivity
        // would install silently and change nothing on screen.
        assert!(
            palette_offers(
                ColorTableFamily::Reflectivity,
                tables.for_family(ColorTableFamily::Reflectivity),
                Some(&library),
            )
            .iter()
            .all(|table| table.base_name() != "Ramp Velocity")
        );

        // With no folder at all the list is exactly what it always was.
        assert_eq!(
            palette_offers(
                ColorTableFamily::Velocity,
                tables.for_family(ColorTableFamily::Velocity),
                None,
            )
            .len(),
            builtin_count + 1,
            "the built-in list plus its flip row"
        );
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
