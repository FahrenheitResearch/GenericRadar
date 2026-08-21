//! The master settings window: every knob in the application, one place.
//!
//! Structure is a Windows-properties dialog because that is the product's
//! UI language: categories down the left, the selected page on the right, a
//! search field that cuts across pages, plain egui widgets throughout so the
//! visual theme (owned elsewhere) restyles everything without this module
//! naming a single colour.
//!
//! This window is where the application's depth lives. The main view is
//! deliberately quiet - the pane and the storm, not a wall of controls - and
//! everything an analyst might want to adjust is behind this one window
//! instead. That only works if the window stays navigable as it fills up, so
//! four things here are load-bearing rather than decoration:
//!
//! * **Search across every page** ([`draw_search_field`],
//!   [`draw_search_results`]). Every typed word has to appear in a setting's
//!   name, its help, its stored id, its subsection or the name of its page,
//!   so adding a word narrows rather than scatters. Results carry the page
//!   name and the subsection they came out of, because a knob found and
//!   changed but not locatable again is a knob that cannot be revisited.
//! * **Subsections** ([`settings::SettingsCategory::sections`]). A page
//!   declares headings on its own items; the window draws runs of them. A
//!   page that declares none is one unheaded run, which is the same widget
//!   stream this file emitted before subsections existed.
//! * **Modified marks** ([`modified_specs`]). A row whose effective value is
//!   not the shipped default carries a mark, the two values in words, and its
//!   own Reset; the page says how many it has; the category list says which
//!   pages have any.
//! * **Resets that state their blast radius first** ([`draw_page_reset`],
//!   [`ResetPlan`]) and a file the whole document can be carried out on and
//!   back in ([`draw_transfer_section`]). Nothing here discards a value
//!   without first naming what it is about to discard - and that includes the
//!   values this build has no row for, which a page reset names, an import
//!   keeps rather than deleting, and the backup page counts. Every
//!   destructive control here takes two presses with the words in between:
//!   the two resets, the export that would rename over a file that is already
//!   there, and the import.
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
//! * `app.rs` applies changed values to the live application and mirrors live
//!   state back into the store.
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
// homes: as `workstation_app::settings_ui` and via the `#[path]` include in
// `crates/settings/tests/workstation_settings_ui.rs`. A `#[path]`-loaded
// module resolves default child paths beside the loaded file (mod-rs
// semantics), which in the harness is `src/`, where `palettes.rs` names a
// different, pre-existing module of the application. An explicit
// `settings_ui/…` path resolves identically in both homes; do not "simplify"
// these back to bare `pub mod`.
#[path = "settings_ui/catalog.rs"]
pub mod catalog;
#[path = "settings_ui/palettes.rs"]
pub mod palettes;
#[path = "settings_ui/profiles.rs"]
pub mod profiles;
#[path = "settings_ui/sync.rs"]
pub mod sync;

use std::path::PathBuf;
use std::sync::Arc;

use color_tables::user::UserTableLibrary;
use color_tables::{ColorTable, ColorTableFamily, ColorTableSet};
use eframe::egui;
use settings::{
    ImportSummary, LoadStatus, SettingKind, SettingSpec, SettingValue, SettingsCategory,
    SettingsRegistry, SettingsStore,
};

/// The registry the application actually runs on: the Appearance page first,
/// then everything [`catalog`] declares.
///
/// The Appearance page arrives as an argument rather than being read from
/// the theme module, because this file is compiled in a second home (the
/// `settings` crate's UI harness) where `crate::theme` does not exist. Both
/// homes pass `theme::settings::settings_category()`; the argument is the
/// seam, not a place to invent a different page.
///
/// Registered first because the category list is in registration order and
/// Appearance belongs at the top of it. `catalog`'s own Appearance page
/// carries the toolbar setting and merges into this one - a registry appends
/// to an existing category id rather than duplicating it - so the page reads
/// theme, accent, edges, density, scale, toolbar.
pub fn full_registry(appearance: SettingsCategory) -> SettingsRegistry {
    let mut registry = SettingsRegistry::new();
    registry.register(appearance);
    catalog::register_into(&mut registry);
    registry
}

/// Minimum hit-target height for interactive rows, in points. 24 pt is the
/// floor the mobile requirement sets for touch.
pub(crate) const MIN_INTERACT_HEIGHT: f32 = 24.0;

/// The mark on a row whose value is not the shipped default, and on the
/// category list entry of a page that holds one. Drawn in the theme's accent
/// (pinned legible on the panel face in both variants) and explained in words
/// on every page it appears on - hover does not exist on glass, so a mark
/// nobody can ask about is a mark nobody can read.
///
/// U+2022 BULLET specifically, and not one of the rounder geometric shapes.
/// The application adds no font: it draws on the ones egui bundles, and
/// U+25CF BLACK CIRCLE, U+25C6, U+25B8 and U+2713 are all missing from them -
/// each one renders as an empty tofu box, which reads as a broken build
/// rather than as a mark. Photographed, not assumed; the alternatives were
/// rendered side by side and looked at before this one was picked. Any change
/// here has to be looked at the same way.
const MODIFIED_MARK: &str = "\u{2022}";

/// The category-list column, in points, and the share of a narrow window it
/// is allowed to take. The fixed width is what the design wants; the share
/// is what stops it eating a phone-width window whole.
const CATEGORY_COLUMN_POINTS: f32 = 176.0;
const CATEGORY_COLUMN_MAX_SHARE: f32 = 0.34;
const CATEGORY_COLUMN_MIN_POINTS: f32 = 92.0;

/// Widths a row's control may take, and the room a row keeps back for the
/// readout and the label beside it.
///
/// The theme's slider track is 140 points and the combo boxes here ask for
/// 210. Both are right on a window of any ordinary size, and both are wider
/// than the whole page column on a phone-shaped one - where the label they
/// belong to was being clipped off the right edge entirely. Photographed at
/// 400 points and 1.6× before this existed: the page read as sliders with no
/// names.
///
/// Written as `full.min(available - reserved)` so a page with room takes the
/// full declared width and renders exactly as it always has; only a page
/// without room gives any of it up.
///
/// [`COMBO_POINTS`] is the declared width of every choice combo, and it is
/// the ONE declaration of that width: a combo's DROPPED list has to be laid
/// out against the button too, because egui gives the menu the button's width
/// as a minimum (`ComboBox::show_ui` passes it to `Area::default_width`), and
/// a second constant for the same 210 is a second thing to forget to change.
/// [`described_option_wrap_width`] therefore takes the width the rule above
/// actually yielded rather than the declared 210 - on a window narrow enough
/// to shrink the button, the list under it is that narrow too, and a wrap
/// width of 210 there would not narrow the descriptions but cut them off.
const ROW_LABEL_RESERVE: f32 = 140.0;
const COMBO_LABEL_RESERVE: f32 = 40.0;
const MIN_SLIDER_POINTS: f32 = 56.0;
const MIN_COMBO_POINTS: f32 = 90.0;
const COMBO_POINTS: f32 = 210.0;
const TEXT_FIELD_POINTS: f32 = 120.0;

/// The two inks one described choice row is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OptionInks {
    /// The label line.
    pub label: egui::Color32,
    /// The description under it.
    pub description: egui::Color32,
}

/// The inks a described option row is drawn in, for the ground egui paints
/// under that row in this state.
///
/// A described option is two lines in ONE selectable, and a selectable does
/// not have one ground. Resting and unselected it paints no frame at all -
/// `Button::selectable` is `frame_when_inactive(selected)` (egui 0.34.3) -
/// and sits on the menu's own face. Hovered it takes the hover face, held
/// it takes the pressed face, selected it takes `selection.bg_fill`. Four
/// grounds, and weak ink was only ever measured against the first.
///
/// So the ink cannot be a constant. The label takes exactly what egui's own
/// `Style::button_style` would give this row, which is what keeps a
/// described row the same colour as a plain one in the same state. The
/// description keeps weak ink only on the ground weak ink was measured
/// against - the one with no frame under it - and on every framed ground
/// rises to the label's ink and stays secondary by SIZE alone. That is not a
/// preference: across the registered themes `text_weak` lands between
/// 2.81:1 and 4.17:1 on their own selection fills and between 3.29:1 and
/// 10.89:1 on their own hover faces, under the 4.5:1 floor (WCAG 2.2 SC
/// 1.4.3) on most of them, while the ink chosen here clears it on every
/// theme and every accent. `tests/theme_catalog.rs` measures this function
/// against [`described_option_ground`] and is what keeps it true.
pub(crate) fn described_option_inks(
    style: &egui::Style,
    state: egui::widget_style::WidgetState,
    selected: bool,
) -> OptionInks {
    let label = style.button_style(state, selected).text_style.color;
    let description = if selected || state != egui::widget_style::WidgetState::Inactive {
        label
    } else {
        style.visuals.weak_text_color()
    };
    OptionInks { label, description }
}

/// The ground egui paints under one described option row.
///
/// Dead code in the application - egui paints the ground itself - and used by
/// the contrast tests in `workstation_app/tests/theme_catalog.rs`, which
/// includes this file by path so that the ink from
/// [`described_option_inks`] is measured against the fill the SAME row is
/// drawn on. It lives here rather than in the test because it is a fact
/// about the widget this row uses, and a copy of it in a test file would go
/// stale the first time this row changed widget.
#[allow(dead_code)]
pub(crate) fn described_option_ground(
    style: &egui::Style,
    state: egui::widget_style::WidgetState,
    selected: bool,
) -> egui::Color32 {
    if !selected && state == egui::widget_style::WidgetState::Inactive {
        // No frame in this state, so the ground is the menu's own.
        egui::Frame::popup(style).fill
    } else {
        style.button_style(state, selected).frame.fill
    }
}

/// Every state a described option row can be drawn in, as
/// `(widget state, selected)`.
///
/// The complete state matrix, kept beside the two functions it crosses so that
/// a state added to egui - or a row that starts sensing drag - is added in one
/// place. Also dead code in the application.
#[allow(dead_code)]
pub(crate) const DESCRIBED_OPTION_STATES: [(egui::widget_style::WidgetState, bool); 8] = [
    (egui::widget_style::WidgetState::Noninteractive, false),
    (egui::widget_style::WidgetState::Inactive, false),
    (egui::widget_style::WidgetState::Hovered, false),
    (egui::widget_style::WidgetState::Active, false),
    (egui::widget_style::WidgetState::Noninteractive, true),
    (egui::widget_style::WidgetState::Inactive, true),
    (egui::widget_style::WidgetState::Hovered, true),
    (egui::widget_style::WidgetState::Active, true),
];

/// The width a described option's two lines have to fit inside.
///
/// Measured, not chosen. The list opens at the combo's left edge and is
/// clipped at the display's right edge, so the distance between those two -
/// less what the menu spends on its own margin, the row's padding and the
/// scroll channel - is the room a description really has. Past it the text
/// is not narrow, it is GONE: egui's menus lay their text out with
/// `TextWrapMode::Extend` and a run that does not fit is simply cut off at
/// the edge.
///
/// Taken here, on the page's own `Ui`, rather than inside the menu closure:
/// inside it, `available_width` is the width the menu MEASURED last frame,
/// and wrapping to that would latch the list at whatever width the first
/// unwrapped pass happened to produce.
fn described_option_wrap_width(ui: &egui::Ui, combo_width: f32) -> f32 {
    let spacing = ui.spacing();
    let chrome =
        spacing.menu_margin.sum().x + 2.0 * spacing.button_padding.x + spacing.scroll.bar_width;
    let room = ui.ctx().content_rect().right() - ui.cursor().left() - chrome;
    // Never narrower than the combo itself, which egui hands the menu as a
    // minimum width, and never zero or negative - a wrap width of nothing
    // puts one character on each line. `combo_width` is the width the button
    // is ACTUALLY being given this frame, not the declared `COMBO_POINTS`: a
    // narrow window shrinks the button, and a floor of 210 there would wrap
    // the descriptions wider than the menu they are drawn in, which does not
    // narrow them - it cuts them off.
    room.max(combo_width - chrome).max(1.0)
}

/// Window state that survives between frames. Owned by `WorkstationApp`.
#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    selected_category: Option<String>,
    /// The window's own page - what differs from the defaults across every
    /// category, reset everything, export and import - selected under the
    /// contributed categories rather than mixed in among them.
    manage_page: bool,
    search: String,
    /// Put the caret in the search field on the next frame. Set by Ctrl/Cmd+F
    /// and by [`Self::open_search`], because a search deep link that leaves
    /// the caret somewhere else cannot be corrected without a mouse.
    focus_search: bool,
    /// Which reset, if any, is armed and waiting to be confirmed.
    confirm: Confirmations,
    /// The export/import path field and the last thing either of them said.
    transfer: TransferUi,
    /// The Radar page's palette rows, held between frames. See
    /// [`PaletteOfferCache`]: the list is rebuilt from parsed text and
    /// cloned tables, and a combo popup asks for it every frame it is open.
    palette_offers: PaletteOfferCache,
    /// The Profiles page: the profile library and the answer it is waiting
    /// for. Public because the application reads the active profile's name
    /// out of it for the File menu - see [`profiles::ProfilesUi::summary`].
    pub profiles: profiles::ProfilesUi,
}

impl SettingsUi {
    /// Open the window on a given category page - for a control that deep
    /// links into settings (a gear on the 3D window opening the 3D page) and
    /// for the preview example. An unknown id opens the first page.
    pub fn open_category(&mut self, id: &str) {
        self.open = true;
        self.selected_category = Some(id.to_owned());
        self.manage_page = false;
        self.search.clear();
    }

    /// Open the window with a search already running - the "find a setting"
    /// deep link.
    pub fn open_search(&mut self, term: &str) {
        self.open = true;
        self.search = term.to_owned();
        self.focus_search = true;
    }

    /// Open the window on the backup-and-reset page.
    pub fn open_manage(&mut self) {
        self.open = true;
        self.manage_page = true;
        self.search.clear();
    }

    /// Put the window into a state a click would have put it into, so the
    /// offscreen proof can photograph it.
    ///
    /// The states this reaches - an armed reset, an import summary, a refused
    /// import - are the ones that carry the most words and the least
    /// rehearsal, which is exactly where text runs off an edge or a
    /// confirmation forgets to say what it is about to throw away. They are
    /// unreachable from outside without synthesising a click at a button's
    /// pixel position, which is a fragile thing to build a proof on.
    ///
    /// Not a product API: nothing in the application calls it, and nothing
    /// should. `allow(dead_code)` for exactly that reason - the module is
    /// compiled in three homes (the binary, the settings crate's test
    /// harness, `examples/settings_depth_proof`) and only the last one uses
    /// this.
    #[allow(dead_code)]
    pub fn stage(&mut self, stage: ProofStage) {
        self.open = true;
        match stage {
            ProofStage::PageResetArmed(id) => {
                self.manage_page = false;
                self.selected_category = Some(id.clone());
                self.confirm.page = Some(id);
                self.confirm.arm_scroll();
            }
            ProofStage::ResetAllArmed => {
                self.manage_page = true;
                self.confirm.all = true;
                self.confirm.arm_scroll();
            }
            ProofStage::Imported(path, summary) => {
                self.manage_page = true;
                self.transfer.path = path;
                self.transfer.report = Some(TransferReport::Imported(Box::new(summary)));
            }
            ProofStage::ImportPreview(path, summary) => {
                self.manage_page = true;
                self.transfer.path = path;
                self.transfer.report = Some(TransferReport::ImportPreview(Box::new(summary)));
            }
            ProofStage::ImportRefused(path, reason) => {
                self.manage_page = true;
                self.transfer.path = path;
                self.transfer.report = Some(TransferReport::Refused(reason));
            }
            ProofStage::ExportWouldOverwrite(path) => {
                self.manage_page = true;
                self.transfer.path = path.clone();
                self.transfer.export_armed = Some(PathBuf::from(&path));
                self.transfer.report =
                    Some(TransferReport::ExportWouldOverwrite(PathBuf::from(path)));
            }
        }
    }
}

/// See [`SettingsUi::stage`]. A state the window reaches by being clicked,
/// named so a photograph can be taken of it.
#[allow(dead_code)]
pub enum ProofStage {
    /// A page's reset armed, showing what it would discard.
    PageResetArmed(String),
    /// The whole-application reset armed, showing the same for every page.
    ResetAllArmed,
    /// An import that happened, with its summary.
    Imported(String, ImportSummary),
    /// An import read and summarised but not yet applied, waiting for the
    /// second press.
    ImportPreview(String, ImportSummary),
    /// An import that was refused, with the reason.
    ImportRefused(String, String),
    /// An export stopped because a file is already at that path, armed so the
    /// next press writes.
    ExportWouldOverwrite(String),
}

/// Which reset is armed, and whether the page still owes it a scroll.
///
/// A confirmation nobody can see is worse than no confirmation: the page it
/// arms on can be nineteen settings long, the button that arms it is at the
/// bottom of that, and the words saying what a reset would throw away land
/// below the fold. So arming asks the page to scroll them into view - once,
/// on the frame it arms, so a later scroll by the analyst is not fought.
#[derive(Default)]
struct Confirmations {
    /// The page whose reset is armed, by category id.
    page: Option<String>,
    /// The whole-application reset is armed.
    all: bool,
    /// Frames of scroll-into-view still owed. A countdown rather than a
    /// single flag because the first frame after arming does not know the
    /// page's full height yet and clamps the scroll to almost nothing -
    /// photographed, with the confirmation still below the fold, before this
    /// was three.
    scroll_frames: u8,
}

impl Confirmations {
    /// Arm the scroll that follows arming a confirmation.
    fn arm_scroll(&mut self) {
        self.scroll_frames = 3;
    }

    /// Bring the armed confirmation into view. `response` is the
    /// confirmation's own heading, so the block that follows it lands under
    /// the top of the viewport rather than wherever the cursor happened to
    /// be.
    fn take_scroll(&mut self, ui: &egui::Ui, response: &egui::Response) {
        if self.scroll_frames == 0 {
            return;
        }
        self.scroll_frames -= 1;
        response.scroll_to_me(Some(egui::Align::Min));
        // The countdown is in FRAMES, and egui only draws a frame when
        // something asks it to. Without this the second and third frames may
        // never happen and the scroll stops where the first one clamped it.
        ui.ctx().request_repaint();
    }
}

/// The export/import strip's own state: the path the analyst typed and the
/// last outcome, held until the next action replaces it.
#[derive(Default)]
struct TransferUi {
    path: String,
    /// The default path has been offered once. Once, and not every frame
    /// while the field is empty: refilling it means the field cannot be
    /// cleared, and a field that cannot be cleared makes both of the
    /// "type a path first" messages below unreachable.
    path_seeded: bool,
    /// The path Export is armed to overwrite. Export refuses to write over an
    /// existing file on the first press and names it; pressing again with the
    /// same path in the field goes through. Same two-press shape as the
    /// resets on this page, for the same reason - the path is typed, one
    /// wrong character names a file the analyst did not mean, and a rename
    /// over it is not undoable.
    export_armed: Option<PathBuf>,
    /// A file that has been read and summarised but not applied. See
    /// [`PendingImport`].
    pending_import: Option<PendingImport>,
    report: Option<TransferReport>,
}

/// An import that has been read, understood and shown, waiting for the press
/// that carries it out.
///
/// Import was the one control in this window that discarded values without
/// asking twice, and `settings::transfer::summarize` exists - and says so in
/// its own documentation - precisely "so the window can show them before
/// deciding". This is the window doing that: the first press reads the file
/// and prints what it would do, the second press does it.
///
/// The parsed document is held rather than re-read on the second press, so
/// what lands is exactly what was shown - a file rewritten underneath the
/// analyst between the two presses cannot turn the summary into a lie.
struct PendingImport {
    path: PathBuf,
    document: settings::SettingsDocument,
    summary: Box<ImportSummary>,
}

/// What to print under the export/import controls. Kept as data rather than
/// a pre-rendered string so the summary can be laid out as lines and pinned
/// by a test that never opens a window.
enum TransferReport {
    Exported(PathBuf),
    ExportFailed(String),
    /// A file is already there. Nothing was written; the next press writes.
    ExportWouldOverwrite(PathBuf),
    /// A file read and understood but not applied. Nothing has changed; the
    /// next press applies exactly this.
    ImportPreview(Box<ImportSummary>),
    Imported(Box<ImportSummary>),
    Refused(String),
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
    /// The Profiles page replaced the whole settings document - a profile was
    /// switched to. The caller re-applies the document to live state through
    /// the same path it uses at startup, and pushes every declared setting
    /// through its own per-setting apply path, so a profile carries settings
    /// this file has never heard of. Deliberately a flag and not a list of
    /// changed keys: see the `profiles` module documentation.
    pub profile_switched: bool,
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

            // Ctrl/Cmd+F puts the caret in the search field. Consumed here,
            // inside the window's own body, so the chord exists only while
            // the window is open and the rest of the application never sees
            // a key this module swallowed.
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
                state.focus_search = true;
            }

            // Panels-inside-the-window, so the search strip and the status
            // footer reserve their space FIRST and the page gets the rest.
            // Before this, the page scroll area computed its own height and
            // on the long pages squeezed the footer off the bottom edge -
            // seen in the preview screenshots, not hypothesised.
            egui::Panel::top("settings-search").show_inside(ui, |ui| {
                draw_search_field(ui, state);
            });
            egui::Panel::bottom("settings-footer").show_inside(ui, |ui| {
                // The active profile rides on the footer that already names
                // the settings file, because that is the line a reader
                // already goes to when asking "what is this window editing?"
                // - findable without adding anything to the main window.
                //
                // Both lines wrap: the status line carries a full file path
                // and a profile can be given any name at all, and an
                // unwrapped label of either would set the window's minimum
                // width to the length of that string.
                if let Some((name, modified)) = state.profiles.summary(registry, store) {
                    let modified = if modified { " (modified)" } else { "" };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("Profile: {name}{modified}"))
                                .small()
                                .weak(),
                        )
                        .wrap(),
                    );
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(store_status_line(store)).small().weak())
                        .wrap(),
                );
            });
            let terms = search_terms(&state.search);
            egui::Panel::left("settings-categories")
                .resizable(false)
                .exact_size(category_column_width(ui.available_width()))
                .show_inside(ui, |ui| {
                    // Scrolled: the category list grows every time a crate
                    // contributes a page, and a list that runs off the bottom
                    // of a short window hides pages with no way to reach them.
                    egui::ScrollArea::vertical()
                        .id_salt("settings-category-list")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            draw_category_list(ui, state, registry, store, &terms);
                        });
                });
            egui::CentralPanel::default().show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings-page")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !terms.is_empty() {
                            draw_search_results(ui, registry, store, &terms, &mut outcome);
                        } else if state.manage_page {
                            draw_manage_page(
                                ui,
                                &mut state.confirm,
                                &mut state.transfer,
                                registry,
                                store,
                                color_tables,
                                user_tables,
                                &mut outcome,
                            );
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
                                    PageContext {
                                        confirm: &mut state.confirm,
                                        profiles: &mut state.profiles,
                                        palettes: PaletteContext {
                                            color_tables,
                                            user_tables,
                                            offers: &mut state.palette_offers,
                                        },
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

/// The search strip: the field, its clear affordance, and the two keys that
/// reach it without a mouse.
///
/// The field takes whatever width is left rather than a fixed 220 points,
/// because at a large UI scale a fixed field is the first thing to squeeze
/// the Clear button off the strip.
fn draw_search_field(ui: &mut egui::Ui, state: &mut SettingsUi) {
    // Escape empties the field, and ONLY when there is something in it and
    // nothing else is claiming the key first.
    //
    // This strip is drawn in the window's top panel, ahead of the page, so a
    // `consume_key` here runs BEFORE any combo popup the page opened gets to
    // look at the input - and combos are drawn in the search results too.
    // Escape then cleared the search and took the whole result list with it
    // instead of closing the popup the analyst was looking at. egui's popups
    // read Escape with `key_pressed`, not `consume_key`, so they cannot
    // defend themselves against a consumer that runs earlier; the check has
    // to be here.
    let popup_open = egui::Popup::is_any_open(ui.ctx());
    if !state.search.is_empty()
        && !popup_open
        && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        state.search.clear();
    }
    ui.horizontal(|ui| {
        ui.label("Search");
        let clear_width = if state.search.is_empty() { 0.0 } else { 72.0 };
        let width = (ui.available_width() - clear_width).max(120.0);
        // `add_sized`, not `add`: a singleline TextEdit's height comes from
        // the text galley, not from `interact_size`, so it lands under the
        // 24 pt touch floor unless it is told otherwise.
        let field = ui.add_sized(
            [width, MIN_INTERACT_HEIGHT],
            egui::TextEdit::singleline(&mut state.search)
                .hint_text("name, description or stored id  (Ctrl+F)"),
        );
        if std::mem::take(&mut state.focus_search) {
            field.request_focus();
        }
        if !state.search.is_empty() && ui.button("Clear").clicked() {
            state.search.clear();
        }
    });
}

/// The typed search, split into the words every match must contain.
///
/// Words rather than one phrase: with several crates contributing pages, the
/// way anyone finds a knob is by remembering two things about it ("map
/// rings", "3d ramp"), and a phrase search finds neither unless those words
/// happen to sit next to each other in the help text.
fn search_terms(search: &str) -> Vec<String> {
    search
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
}

/// The category column: every contributed page, marked where it holds
/// changed values or search matches, then the window's own page under a rule.
fn draw_category_list(
    ui: &mut egui::Ui,
    state: &mut SettingsUi,
    registry: &SettingsRegistry,
    store: &SettingsStore,
    terms: &[String],
) {
    let searching = !terms.is_empty();
    let mut any_modified = false;
    for category in registry.categories() {
        let modified = modified_specs(store, category).len();
        any_modified |= modified > 0;
        let matches = if searching {
            category
                .settings
                .iter()
                .filter(|spec| spec_matches(category, spec, terms))
                .count()
        } else {
            0
        };
        let mut label = String::new();
        if modified > 0 {
            label.push_str(MODIFIED_MARK);
            label.push(' ');
        }
        label.push_str(&category.label);
        if searching {
            label.push_str(&format!("  ({matches})"));
        }
        // Selected shows the page the window would return to, so clearing a
        // search never lands somewhere the analyst did not choose.
        let selected = !state.manage_page
            && state
                .selected_category
                .as_deref()
                .map(|id| id == category.id)
                .unwrap_or(false);
        let mut text = egui::RichText::new(label);
        if modified > 0 && !selected {
            text = text.color(ui.visuals().hyperlink_color);
        }
        if searching && matches == 0 {
            text = text.weak();
        }
        if ui.selectable_label(selected, text).clicked() {
            state.selected_category = Some(category.id.clone());
            state.manage_page = false;
            state.search.clear();
        }
    }
    ui.add_space(6.0);
    ui.separator();
    if ui
        .selectable_label(state.manage_page, "Backup & reset")
        .clicked()
    {
        state.manage_page = true;
        state.search.clear();
    }
    if any_modified {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(format!("{MODIFIED_MARK} changed from the default"))
                .small()
                .weak(),
        );
    }
}

/// How wide the category column may be. Fixed at the design width when there
/// is room, capped at a share of the window when there is not: on a
/// phone-width display a 176-point column beside a 280-point window leaves a
/// page too narrow to hold a slider.
fn category_column_width(available: f32) -> f32 {
    CATEGORY_COLUMN_POINTS
        .min((available * CATEGORY_COLUMN_MAX_SHARE).max(CATEGORY_COLUMN_MIN_POINTS))
        .min(available.max(1.0))
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

/// Every piece of mutable window state a page may touch, in one value.
///
/// Bundled because the alternative is a positional parameter per surface. The
/// subsection and reset machinery needs `confirm`, and named profiles needs
/// `profiles`; together they push `draw_category_page` past the argument count
/// clippy accepts. The next surface goes in here rather than on the end of the
/// signature.
struct PageContext<'a> {
    confirm: &'a mut Confirmations,
    profiles: &'a mut profiles::ProfilesUi,
    palettes: PaletteContext<'a>,
}

/// One category page: its rows, its palette section if it is the Radar page,
/// and its restore-defaults footer.
fn draw_category_page(
    ui: &mut egui::Ui,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    category_id: &str,
    page: PageContext<'_>,
    outcome: &mut SettingsOutcome,
) {
    let PageContext {
        confirm,
        profiles: profiles_state,
        palettes,
    } = page;
    let Some(category) = registry.category(category_id) else {
        return;
    };
    ui.heading(&category.label);
    draw_modified_banner(ui, store, category);
    ui.add_space(4.0);
    // Sections, not a flat loop. A page that declared no headings is exactly
    // one section with an empty heading, so the widget stream below is the
    // same one this loop emitted before headings existed - pinned by
    // `an_ungrouped_page_draws_the_same_shapes_it_did_before_sections`.
    for section in category.sections() {
        if !section.heading.is_empty() {
            // Generous above, tight below: what makes a heading read as the
            // start of a group rather than as a line of bold text is the gap
            // in FRONT of it. Photographed at 6 points and it did not - the
            // headings sat in the flow like everything else.
            ui.add_space(12.0);
            ui.strong(section.heading);
            ui.add_space(3.0);
        }
        for spec in section.settings {
            draw_setting_row(ui, store, category_id, spec, outcome);
        }
    }
    // The Profiles page is not a page of knobs: it is a list of named
    // snapshots of every other page, so it is drawn by its own module and
    // deliberately has no "restore defaults" footer - what would be restored
    // is a folder of the analyst's files.
    if category_id == settings::profiles::BOOKKEEPING_CATEGORY {
        profiles::draw_profiles_page(ui, profiles_state, registry, store, outcome);
        return;
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
    ui.separator();
    draw_page_reset(ui, store, category, confirm, color_tables, outcome);
}

/// The line that explains the marks on this page, drawn only where there are
/// marks to explain.
fn draw_modified_banner(ui: &mut egui::Ui, store: &SettingsStore, category: &SettingsCategory) {
    let modified = modified_specs(store, category);
    if modified.is_empty() {
        return;
    }
    ui.label(
        egui::RichText::new(format!(
            "{MODIFIED_MARK} {} {} on this page {} not at the shipped default. \
             Each one carries its own Reset.",
            modified.len(),
            if modified.len() == 1 {
                "setting"
            } else {
                "settings"
            },
            if modified.len() == 1 { "is" } else { "are" },
        ))
        .small()
        .color(ui.visuals().hyperlink_color),
    );
}

/// The page's own reset, and the confirmation in front of it.
///
/// The button never resets on the press. It arms, the page lists every value
/// that would go and what it would go back to, and only the second press
/// carries it out - because a page can hold a tuning session's worth of work
/// and "Restore defaults" a pixel away from the last slider is a trap.
fn draw_page_reset(
    ui: &mut egui::Ui,
    store: &mut SettingsStore,
    category: &SettingsCategory,
    confirm: &mut Confirmations,
    color_tables: Option<&mut Arc<ColorTableSet>>,
    outcome: &mut SettingsOutcome,
) {
    let modified = modified_specs(store, category);
    let palettes_modified = category.id == catalog::keys::radar::CATEGORY
        && color_tables
            .as_deref()
            .is_some_and(|tables| **tables != ColorTableSet::default());
    let armed = confirm.page.as_deref() == Some(category.id.as_str());
    if !armed {
        let anything = !modified.is_empty() || palettes_modified;
        ui.add_enabled_ui(anything, |ui| {
            if ui
                .button(format!("Reset {}\u{2026}", category.label))
                .clicked()
            {
                confirm.page = Some(category.id.clone());
                confirm.arm_scroll();
            }
        });
        if !anything {
            ui.label(
                egui::RichText::new("Everything on this page is at its shipped default.")
                    .small()
                    .weak(),
            );
        }
        return;
    }

    ui.add_space(4.0);
    // Nineteen settings can sit above this on the long page, so the words
    // that say what a reset would throw away start below the fold.
    let heading = ui.strong(format!("Reset {} to the shipped defaults?", category.label));
    confirm.take_scroll(ui, &heading);
    for spec in &modified {
        let current = spec
            .kind
            .sanitize(store.value(&category.id, &spec.id).as_ref());
        ui.label(
            egui::RichText::new(format!(
                // No arrow glyph. U+2192 is not in the fonts this application
                // draws on and renders as an empty box - looked at, in this
                // exact list, before it became words.
                "{}: {} \u{00b7} back to {}",
                spec.label,
                spec.kind.display(&current),
                spec.kind.display(&spec.kind.default_value()),
            ))
            .small(),
        );
    }
    if palettes_modified {
        ui.label(
            egui::RichText::new(
                "The installed colour tables go back to the shipped ones as well - they \
                 are on this page.",
            )
            .small(),
        );
    }
    // `reset_category` removes the page's WHOLE map, including rows under ids
    // this build does not declare (a newer build's settings, stored under a
    // page both builds have). The list above is built from what this build
    // declares and cannot show them, so they are named here - a confirmation
    // whose entire job is to say what goes must not leave a class of value
    // out of the list. Same computation `ResetPlan::survey` does for the
    // whole-application reset, which has always named them.
    let undeclared = undeclared_ids(store, category);
    if !undeclared.is_empty() {
        ui.label(
            egui::RichText::new(format!(
                "{} {} under {} this build does not declare {} removed as well: {}.",
                undeclared.len(),
                if undeclared.len() == 1 {
                    "value"
                } else {
                    "values"
                },
                if undeclared.len() == 1 {
                    "an id"
                } else {
                    "ids"
                },
                if undeclared.len() == 1 { "is" } else { "are" },
                undeclared.join(", "),
            ))
            .small(),
        );
    }
    if modified.is_empty() && !palettes_modified && undeclared.is_empty() {
        ui.label(egui::RichText::new("Nothing on this page has changed.").small());
    }
    ui.add_space(4.0);
    let count = modified.len();
    ui.horizontal(|ui| {
        if ui
            .button(format!(
                "Reset {count} {}",
                if count == 1 { "setting" } else { "settings" }
            ))
            .clicked()
        {
            store.reset_category(&category.id);
            for spec in &category.settings {
                outcome.changed.push((category.id.clone(), spec.id.clone()));
            }
            // The colour tables sit on this same page, under this same
            // button. A "Reset Radar" that reset every slider but left a
            // non-default velocity table installed would be quietly lying.
            if category.id == catalog::keys::radar::CATEGORY
                && let Some(tables) = color_tables
                && restore_default_palettes(store, tables)
            {
                outcome.palette_changed = true;
            }
            confirm.page = None;
        }
        if ui.button("Cancel").clicked() {
            confirm.page = None;
        }
    });
}

/// Every setting on a page whose effective value is not its declared default.
///
/// Effective value against default, NOT "is there a row in the file": the
/// application mirrors live state into the store every frame, so a great many
/// settings carry a stored value that is character for character the default.
/// Marking those as changed - which is what a presence test does - would put
/// a mark on almost every row and make the mark mean nothing.
fn modified_specs<'a>(
    store: &SettingsStore,
    category: &'a SettingsCategory,
) -> Vec<&'a SettingSpec> {
    category
        .settings
        .iter()
        .filter(|spec| is_modified(store, &category.id, spec))
        .collect()
}

/// The slider track for a row `row_width` points wide: the theme's, unless
/// there is not enough room left over for the readout and the label.
///
/// `reserve` is what this row's readout and label need - see
/// [`slider_label_reserve`], which measures it rather than assuming it.
fn slider_track(ui: &egui::Ui, row_width: f32, reserve: f32) -> f32 {
    ui.spacing()
        .slider_width
        .min((row_width - reserve).max(MIN_SLIDER_POINTS))
}

/// How much of a slider row its readout and its own label need, in points.
///
/// Measured off the font that will draw it, not assumed, because a slider's
/// label is the one label on this page that CANNOT wrap. egui's `Slider`
/// draws its text inside a nested horizontal layout of its own with
/// `TextWrapMode::Extend`, so the `horizontal_wrapped` the row sits in - which
/// is what drops a combo's label onto a second line instead of losing it -
/// never gets the chance. A label with no room is cut off at the page's right
/// edge instead.
///
/// Photographed at a 448-point window before this existed: the Data page read
/// "Live poll interva" and "History frame". The settings-depth work is what
/// made it visible - it brought both the narrow-window rule and the longest
/// row labels in the window - but nothing about it is particular to the new
/// pages, so this is measured for every slider on every page.
///
/// [`ROW_LABEL_RESERVE`] stays as the floor, so a page with room gives up
/// nothing and renders exactly as it always has: on a wide row the track is
/// the theme's declared width either way, and this only ever bites where the
/// alternative was losing the end of a word.
fn slider_label_reserve(ui: &egui::Ui, label: &str) -> f32 {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let text = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER)
        .size()
        .x;
    // The readout box egui puts between the track and the label, plus the gap
    // on either side of it.
    let readout = ui.spacing().interact_size.x + 2.0 * ui.spacing().item_spacing.x;
    (text + readout).max(ROW_LABEL_RESERVE)
}

/// The same rule for a combo box, which carries its label beside it rather
/// than inside it and so needs less held back.
fn combo_width(row_width: f32, full: f32) -> f32 {
    full.min((row_width - COMBO_LABEL_RESERVE).max(MIN_COMBO_POINTS))
}

/// Every setting id the file holds under a page that the page itself does not
/// declare - a future build's knob, or one this build dropped. Sorted, and
/// named rather than counted, because they are short ids and a confirmation
/// that says "and one other thing" is not a confirmation.
///
/// Deliberately not part of [`modified_specs`]: those are rows on screen with
/// a value and a default to show, and these are neither.
fn undeclared_ids(store: &SettingsStore, category: &SettingsCategory) -> Vec<String> {
    store
        .stored_ids(&category.id)
        .into_iter()
        .filter(|id| !category.settings.iter().any(|spec| spec.id == *id))
        .map(str::to_owned)
        .collect()
}

/// See [`modified_specs`].
fn is_modified(store: &SettingsStore, category_id: &str, spec: &SettingSpec) -> bool {
    spec.kind
        .sanitize(store.value(category_id, &spec.id).as_ref())
        != spec.kind.default_value()
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

/// The window's own page: what differs from the defaults across the whole
/// application, the one reset that reaches all of it, and the file the
/// analyst can carry their settings out to and back in on.
///
/// A page of its own rather than a strip along the bottom of every other
/// page, for two reasons. It costs no vertical space on the pages an analyst
/// actually works in - the point of this window is depth behind a quiet main
/// view, not chrome in front of every knob - and the destructive control is
/// somewhere that has to be navigated to rather than a pixel below the last
/// slider that was being dragged.
#[allow(clippy::too_many_arguments)]
fn draw_manage_page(
    ui: &mut egui::Ui,
    confirm: &mut Confirmations,
    transfer: &mut TransferUi,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    color_tables: Option<&mut Arc<ColorTableSet>>,
    user_tables: Option<&UserTableLibrary>,
    outcome: &mut SettingsOutcome,
) {
    let mut color_tables = color_tables;
    ui.heading("Backup & reset");
    ui.add_space(4.0);

    let plan = ResetPlan::survey(registry, store, color_tables.as_deref());
    ui.strong("What differs from the shipped defaults");
    ui.add_space(2.0);
    if plan.changed.is_empty() && !plan.palettes {
        ui.label(
            egui::RichText::new("Nothing. Every setting is at the value this build ships.").small(),
        );
    } else {
        for (label, count) in &plan.changed {
            ui.label(
                egui::RichText::new(format!("{MODIFIED_MARK} {label}: {count}"))
                    .small()
                    .color(ui.visuals().hyperlink_color),
            );
        }
        if plan.palettes {
            ui.label(
                egui::RichText::new(format!(
                    "{MODIFIED_MARK} Colour tables: not the shipped set"
                ))
                .small()
                .color(ui.visuals().hyperlink_color),
            );
        }
    }
    if plan.unknown_values > 0 {
        ui.label(
            egui::RichText::new(format!(
                "The file also holds {} {} under settings this build does not have ({}). \
                 {} carried through every save, and through an import, untouched.",
                plan.unknown_values,
                if plan.unknown_values == 1 {
                    "value"
                } else {
                    "values"
                },
                plan.unknown_categories.join(", "),
                if plan.unknown_values == 1 {
                    "It is"
                } else {
                    "They are"
                },
            ))
            .small()
            .weak(),
        );
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(4.0);
    if confirm.all {
        let heading = ui.strong("Reset every setting in the application?");
        confirm.take_scroll(ui, &heading);
        ui.label(
            egui::RichText::new(
                "This removes your values, not just the ones on this list, and it \
                 cannot be undone. Export first if you might want them back.",
            )
            .small(),
        );
        ui.add_space(2.0);
        for line in plan.confirmation_lines() {
            ui.label(egui::RichText::new(line).small());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .button(format!(
                    "Reset {} stored {}",
                    plan.stored_values,
                    if plan.stored_values == 1 {
                        "value"
                    } else {
                        "values"
                    }
                ))
                .clicked()
            {
                store.reset_all_values();
                for category in registry.categories() {
                    for spec in &category.settings {
                        outcome.changed.push((category.id.clone(), spec.id.clone()));
                    }
                }
                if let Some(tables) = color_tables.as_deref_mut()
                    && restore_default_palettes(store, tables)
                {
                    outcome.palette_changed = true;
                }
                confirm.all = false;
            }
            if ui.button("Cancel").clicked() {
                confirm.all = false;
            }
        });
    } else {
        let anything = plan.stored_values > 0 || plan.palettes;
        ui.add_enabled_ui(anything, |ui| {
            if ui.button("Reset everything\u{2026}").clicked() {
                confirm.all = true;
                confirm.arm_scroll();
            }
        });
        if !anything {
            ui.label(
                egui::RichText::new("There is nothing stored to reset.")
                    .small()
                    .weak(),
            );
        }
    }

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(4.0);
    draw_transfer_section(
        ui,
        transfer,
        registry,
        store,
        color_tables,
        user_tables,
        outcome,
    );
}

/// What a reset would remove, surveyed before anything is offered.
struct ResetPlan {
    /// `(page label, settings on it that differ from their default)`, pages
    /// with none omitted.
    changed: Vec<(String, usize)>,
    /// Every value the file holds, including ones that match a default and
    /// ones under ids this build does not declare - the number of rows a
    /// reset actually deletes.
    stored_values: usize,
    unknown_values: usize,
    unknown_categories: Vec<String>,
    /// The live colour tables are not the shipped set.
    palettes: bool,
}

impl ResetPlan {
    fn survey(
        registry: &SettingsRegistry,
        store: &SettingsStore,
        color_tables: Option<&Arc<ColorTableSet>>,
    ) -> Self {
        let changed = registry
            .categories()
            .iter()
            .filter_map(|category| {
                let count = modified_specs(store, category).len();
                (count > 0).then(|| (category.label.clone(), count))
            })
            .collect();
        let mut stored_values = 0usize;
        let mut unknown_values = 0usize;
        let mut unknown_categories = Vec::new();
        for category_id in store.stored_categories() {
            let ids = store.stored_ids(category_id);
            stored_values += ids.len();
            let here = match registry.category(category_id) {
                // A page this build HAS can still hold an id it does not
                // declare - a knob a newer build added to it. Same class of
                // value as a whole unknown page, so it is named the same way:
                // counting it while leaving its page out of the list is how
                // this panel came to say nothing at all about a value the
                // reset underneath it removes.
                Some(category) => ids
                    .iter()
                    .filter(|id| !category.settings.iter().any(|spec| spec.id == **id))
                    .count(),
                None => ids.len(),
            };
            unknown_values += here;
            if here > 0 {
                // `stored_categories` is sorted, so this stays sorted.
                unknown_categories.push(category_id.to_owned());
            }
        }
        Self {
            changed,
            stored_values,
            unknown_values,
            unknown_categories,
            palettes: color_tables.is_some_and(|tables| **tables != ColorTableSet::default()),
        }
    }

    /// What the confirmation states before the second press. Split out so a
    /// test can read the words without opening a window.
    fn confirmation_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (label, count) in &self.changed {
            lines.push(format!("{label}: {count} back to the default"));
        }
        if self.palettes {
            lines.push("Colour tables: back to the shipped set".to_owned());
        }
        if self.unknown_values > 0 {
            lines.push(format!(
                "{} {} under ids this build does not declare {} removed as well",
                self.unknown_values,
                if self.unknown_values == 1 {
                    "value"
                } else {
                    "values"
                },
                if self.unknown_values == 1 {
                    "is"
                } else {
                    "are"
                },
            ));
        }
        lines.push(
            "The pane layout, camera positions and window geometry are not touched.".to_owned(),
        );
        lines
    }
}

/// Export and import: the whole settings document out to a file the analyst
/// names, and back in again.
///
/// The path is typed rather than picked out of a system dialog, because this
/// workspace ships no dialog crate and adding one for two buttons is not a
/// trade worth making - the colour table editor names its files the same way.
/// The resolved path and the result of the last action are both printed, so
/// nothing about where the file went is left to be guessed.
#[allow(clippy::too_many_arguments)]
fn draw_transfer_section(
    ui: &mut egui::Ui,
    transfer: &mut TransferUi,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    color_tables: Option<&mut Arc<ColorTableSet>>,
    user_tables: Option<&UserTableLibrary>,
    outcome: &mut SettingsOutcome,
) {
    ui.strong("Export & import");
    ui.label(
        egui::RichText::new(
            "Export writes the whole settings file, including values this build does not \
             understand. Import applies a file's settings and colour tables; the pane \
             layout, the cameras and the window position stay exactly as they are.",
        )
        .small()
        .weak(),
    );
    ui.add_space(4.0);
    if !std::mem::replace(&mut transfer.path_seeded, true) && transfer.path.is_empty() {
        transfer.path = default_export_path(store).display().to_string();
    }
    let field_width = ui.available_width();
    ui.add_sized(
        [field_width, MIN_INTERACT_HEIGHT],
        egui::TextEdit::singleline(&mut transfer.path).hint_text("path to a .json file"),
    );
    ui.add_space(4.0);
    // The presses are read inside the row and acted on outside it, so the
    // live colour tables are handed to `import` once, by value, instead of
    // being reborrowed inside a closure that holds both buttons.
    let pressed = ui
        .horizontal_wrapped(|ui| {
            (
                ui.button("Export to this file").clicked(),
                ui.button("Import from this file").clicked(),
            )
        })
        .inner;
    if pressed.0 {
        let report = export(
            store,
            transfer.path.trim(),
            transfer.export_armed.as_deref(),
        );
        // Armed only by the refusal that names the file, and disarmed by
        // anything else - including a press against a different path, which
        // has to name that file before it goes over it too.
        transfer.export_armed = match &report {
            TransferReport::ExportWouldOverwrite(path) => Some(path.clone()),
            _ => None,
        };
        transfer.pending_import = None;
        transfer.report = Some(report);
    }
    if pressed.1 {
        transfer.export_armed = None;
        transfer.report = Some(import(
            store,
            registry,
            color_tables,
            user_tables,
            transfer.path.trim(),
            &mut transfer.pending_import,
            outcome,
        ));
    }
    let Some(report) = &transfer.report else {
        return;
    };
    ui.add_space(6.0);
    match report {
        TransferReport::Exported(path) => {
            ui.label(egui::RichText::new(format!("Wrote {}", path.display())).small());
        }
        TransferReport::ExportFailed(reason) => {
            ui.label(
                egui::RichText::new(format!("Nothing was written. {reason}"))
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
        TransferReport::ExportWouldOverwrite(path) => {
            ui.label(
                egui::RichText::new(format!(
                    "Nothing was written. A file already exists at {}. Press \"Export to \
                     this file\" again to write over it, or change the path first.",
                    path.display()
                ))
                .small()
                .color(ui.visuals().warn_fg_color),
            );
        }
        TransferReport::Refused(reason) => {
            // The one thing an import refusal must never do is fail quietly:
            // a button that appears to do nothing is indistinguishable from a
            // broken build.
            ui.label(
                egui::RichText::new(reason)
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
        TransferReport::ImportPreview(summary) => {
            ui.label(
                egui::RichText::new(summary.preview_headline())
                    .small()
                    .strong(),
            );
            // Above the list, not under it. The list is as long as the import
            // is big - photographed at six changed settings, where it pushed
            // this sentence off the bottom of the window and left a summary
            // that looked exactly like one describing an import that had
            // already happened.
            ui.label(
                egui::RichText::new(
                    "Nothing has changed yet. Press \"Import from this file\" again to \
                     apply exactly this, or change the path to read a different file.",
                )
                .small()
                .color(ui.visuals().warn_fg_color),
            );
            ui.add_space(2.0);
            for line in summary.lines() {
                ui.label(egui::RichText::new(line).small());
            }
        }
        TransferReport::Imported(summary) => {
            ui.label(egui::RichText::new(summary.headline()).small().strong());
            for line in summary.lines() {
                ui.label(egui::RichText::new(line).small());
            }
        }
    }
}

/// Where the export field points before anything is typed: beside the live
/// settings file, which is a directory the analyst can certainly write to.
fn default_export_path(store: &SettingsStore) -> PathBuf {
    store
        .path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
        .join("radar-settings-export.json")
}

/// Write the document to the typed path. `armed_for` is the path a previous
/// press already refused to overwrite: an existing file is written over only
/// on the second press against that same path.
fn export(
    store: &SettingsStore,
    path: &str,
    armed_for: Option<&std::path::Path>,
) -> TransferReport {
    if path.is_empty() {
        return TransferReport::ExportFailed("Type a file name to write to first.".to_owned());
    }
    let path = PathBuf::from(path);
    if path == store.path() {
        // Writing the live file onto itself would "succeed" and leave the
        // analyst with a backup that is the original.
        return TransferReport::ExportFailed(
            "That is the live settings file itself. Choose another name so the export is \
             a separate copy."
                .to_owned(),
        );
    }
    // There is no file dialog in this workspace, so the path is whatever was
    // typed - and the writer renames over the target. One press against a
    // path that happens to name a colour table, a note or last week's export
    // would destroy it with nothing asked. Arm first, the way the resets on
    // this page do.
    if path.exists() && armed_for != Some(path.as_path()) {
        return TransferReport::ExportWouldOverwrite(path);
    }
    match settings::transfer::write_document(&path, store.document()) {
        Ok(()) => TransferReport::Exported(path),
        Err(error) => TransferReport::ExportFailed(format!("{path:?}: {error}")),
    }
}

/// The Import button, both presses of it.
///
/// First press: read the file, say what importing it would do, change
/// nothing. Second press against the same path: do exactly that. `pending`
/// carries the read document between the two, and is cleared by anything
/// else - a different path, a refusal, an export.
#[allow(clippy::too_many_arguments)]
fn import(
    store: &mut SettingsStore,
    registry: &SettingsRegistry,
    color_tables: Option<&mut Arc<ColorTableSet>>,
    user_tables: Option<&UserTableLibrary>,
    path: &str,
    pending: &mut Option<PendingImport>,
    outcome: &mut SettingsOutcome,
) -> TransferReport {
    if path.is_empty() {
        *pending = None;
        return TransferReport::Refused("Type the path of a file to read first.".to_owned());
    }
    let path = PathBuf::from(path);
    let ready = match pending.take() {
        // The second press, against the file the first press named.
        Some(ready) if ready.path == path => ready,
        // Anything else reads and asks again. A path edited between the two
        // presses must never apply the file that was shown under the old one.
        _ => {
            let incoming = match settings::transfer::read_document(&path) {
                Ok(document) => document,
                Err(refusal) => return TransferReport::Refused(refusal.to_string()),
            };
            let summary =
                settings::transfer::summarize(&path, store.document(), &incoming, registry);
            *pending = Some(PendingImport {
                path,
                document: incoming,
                summary: Box::new(summary.clone()),
            });
            return TransferReport::ImportPreview(Box::new(summary));
        }
    };
    let PendingImport {
        path: _,
        document: incoming,
        summary,
    } = ready;
    // Merged, never `incoming.values` wholesale. A plain replace deletes
    // every stored value under a category or id this build does not declare
    // - a newer build's settings, the exact thing the file format and the
    // line one panel above this one both promise are carried through
    // untouched. See `settings::transfer::merge_values`.
    let merged = settings::transfer::merge_values(store.document(), &incoming, registry);
    store.replace_values(merged);
    // Every declared key, not only the ones the summary listed: the
    // application applies changes by key, and a key whose stored row moved
    // without its resolved value moving still has to be re-read so that a
    // later reset resolves against the file that is actually there.
    for category in registry.categories() {
        for spec in &category.settings {
            outcome.changed.push((category.id.clone(), spec.id.clone()));
        }
    }
    if !incoming.workspace.palettes.is_empty() {
        let resolved = match user_tables {
            Some(library) => {
                palettes::apply_palettes_with_user(&incoming.workspace.palettes, library)
            }
            None => palettes::apply_palettes(&incoming.workspace.palettes),
        };
        if let Some(tables) = color_tables
            && **tables != resolved
        {
            *tables = Arc::new(resolved);
            outcome.palette_changed = true;
        }
        let mut workspace = store.workspace().clone();
        workspace.palettes = incoming.workspace.palettes;
        store.set_workspace(workspace);
    }
    TransferReport::Imported(summary)
}

/// Search results: matching rows from every category, under their category's
/// name and their own subsection heading, fully editable in place.
///
/// The category name is drawn as a page heading over each block and repeated
/// on nothing else, because the one thing a cross-page result list has to
/// answer is "where does this knob live" - a row that can be changed but not
/// found again is a row an analyst cannot come back to.
fn draw_search_results(
    ui: &mut egui::Ui,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    terms: &[String],
    outcome: &mut SettingsOutcome,
) {
    let mut categories = 0usize;
    let mut rows = 0usize;
    for category in registry.categories() {
        let matching: Vec<&SettingSpec> = category
            .settings
            .iter()
            .filter(|spec| spec_matches(category, spec, terms))
            .collect();
        if matching.is_empty() {
            continue;
        }
        categories += 1;
        rows += matching.len();
        ui.heading(&category.label);
        ui.add_space(4.0);
        let mut heading: Option<&str> = None;
        for spec in matching {
            // The subsection a match came out of, carried into the result
            // list: on a page that groups its knobs, "Opacity" alone does not
            // say whether this is the volume's opacity or the ground plane's.
            if !spec.group.is_empty() && heading != Some(spec.group.as_str()) {
                ui.label(egui::RichText::new(&spec.group).small().weak());
            }
            heading = (!spec.group.is_empty()).then_some(spec.group.as_str());
            draw_setting_row(ui, store, &category.id, spec, outcome);
        }
        ui.add_space(6.0);
    }
    if categories == 0 {
        ui.label(format!(
            // Straight quotes: the curly pair is another glyph the bundled
            // fonts do not carry.
            "Nothing matches \"{}\".",
            terms.join(" ")
        ));
        ui.label(
            egui::RichText::new(
                "Search covers each setting's name, its description and its stored id, \
                 plus the name of the page it is on. Every word you type has to appear \
                 somewhere in one of those.",
            )
            .small()
            .weak(),
        );
        return;
    }
    ui.label(
        egui::RichText::new(format!(
            "{rows} {} on {categories} {}.",
            if rows == 1 { "match" } else { "matches" },
            if categories == 1 { "page" } else { "pages" },
        ))
        .small()
        .weak(),
    );
}

/// Case-insensitive match on the words a person would search by: the
/// setting's label, its inline help, its stored id, and the label and id of
/// the page it lives on.
///
/// Every term must appear somewhere - an AND, not an OR - so adding a word
/// narrows the list, which is the only behaviour that stays useful as the
/// catalog grows. The page's own name is in the haystack so that typing a
/// page name lists that page's knobs from anywhere.
fn spec_matches(category: &SettingsCategory, spec: &SettingSpec, terms: &[String]) -> bool {
    if terms.is_empty() {
        return false;
    }
    let haystack = format!(
        "{} {} {} {} {} {}",
        spec.label.to_lowercase(),
        spec.help.to_lowercase(),
        spec.id.to_lowercase(),
        spec.group.to_lowercase(),
        category.label.to_lowercase(),
        category.id.to_lowercase(),
    );
    terms.iter().all(|term| haystack.contains(term.as_str()))
}

/// Does typing `query` into the window's search field reach this row?
///
/// For callers outside this module: `app.rs` asks it of the gate filter's five
/// criteria so every control remains findable by an analyst who needs to turn
/// it off.
///
/// Deliberately the real predicate rather than a second implementation of it:
/// a helper that matched on its own terms could agree with a search that had
/// stopped working.
#[cfg(test)]
pub fn search_finds(category: &SettingsCategory, spec: &SettingSpec, query: &str) -> bool {
    spec_matches(category, spec, &search_terms(query))
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
    let default = spec.kind.default_value();
    let modified = effective != default;
    let mut set_value: Option<SettingValue> = None;
    let mut reset = false;
    // Measured before the row is entered: inside a wrapped layout
    // `available_width` is what is left on the CURRENT line, which for the
    // second item is not the width of the page.
    let row_width = ui.available_width();

    ui.add_enabled_ui(spec.enabled, |ui| {
        // Wrapped, so a page too narrow to hold control-plus-label on one
        // line puts the label underneath instead of clipping it off the
        // right edge. On any page wide enough for the row this is the same
        // single line it always was.
        ui.horizontal_wrapped(|ui| {
            if modified {
                // Ahead of the control, so the marks form a column an eye can
                // run down a long page instead of hunting for them at the
                // ragged right edge where the controls end.
                ui.label(egui::RichText::new(MODIFIED_MARK).color(ui.visuals().hyperlink_color));
            }
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
                    floor,
                    ..
                } => {
                    let mut value = effective.as_float().unwrap_or_default();
                    ui.spacing_mut().slider_width =
                        slider_track(ui, row_width, slider_label_reserve(ui, &spec.label));
                    let mut slider = egui::Slider::new(&mut value, *min..=*max).text(&spec.label);
                    match floor {
                        // The leftmost stop means *off*, so it has to say so.
                        // Without this the gate filter's four thresholds read
                        // "-35.0 dBZ", "0.00" and "0.0 km" on this page while
                        // the toolbar's own panel - which has always written
                        // "off" - reads off, and the same five settings tell
                        // two different stories in two places. The one an
                        // analyst reaches through Settings would be the one
                        // that looks like a threshold somebody chose.
                        settings::SliderFloor::Off => {
                            // Exact, not within a tolerance: a number that is
                            // *nearly* the floor is still a criterion that is
                            // on, and a row reading "off" over a pane that is
                            // hiding gates would be the one failure this whole
                            // feature exists to prevent. Printing a number for
                            // a control that is off is only untidy; printing
                            // "off" for one that is not is a lie.
                            let (floor_value, decimals, unit) =
                                (*min, usize::from(*decimals), unit.clone());
                            slider = slider.custom_formatter(move |shown, _| {
                                if shown <= floor_value {
                                    "off".to_owned()
                                } else if unit.is_empty() {
                                    format!("{shown:.decimals$}")
                                } else {
                                    format!("{shown:.decimals$} {unit}")
                                }
                            });
                        }
                        settings::SliderFloor::Number => {
                            slider = slider.fixed_decimals(usize::from(*decimals));
                            if !unit.is_empty() {
                                slider = slider.suffix(format!(" {unit}"));
                            }
                        }
                    }
                    if ui.add(slider).changed() {
                        set_value = Some(SettingValue::Float(value));
                    }
                }
                SettingKind::Integer { min, max, unit, .. } => {
                    let mut value = effective.as_int().unwrap_or_default();
                    ui.spacing_mut().slider_width =
                        slider_track(ui, row_width, slider_label_reserve(ui, &spec.label));
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
                    // egui's default popup height fits about eight ONE-line
                    // entries. A described option is two lines, so the theme
                    // list overflowed it and cut the last theme in half
                    // behind a scrollbar - seen in the settings photographs,
                    // not guessed. Tall enough for eight two-line rows, and
                    // still a maximum: a short list does not stretch to it,
                    // and a longer one still scrolls rather than running off
                    // a phone-height display.
                    let described = options.iter().any(|o| !o.description.is_empty());
                    let popup_height = if described {
                        380.0
                    } else {
                        ui.spacing().combo_height
                    };
                    // Measured before the menu exists, because inside it the
                    // only widths on offer are the ones the menu itself
                    // produced last frame. See `described_option_wrap_width`.
                    let width = combo_width(row_width, COMBO_POINTS);
                    let wrap_width = described_option_wrap_width(ui, width);
                    egui::ComboBox::from_id_salt(salt)
                        .selected_text(current_label)
                        .width(width)
                        .height(popup_height)
                        .show_ui(ui, |ui| {
                            for option in options {
                                let chosen = option.id == current_id;
                                // An option that carries a description is
                                // drawn as two stacked lines inside ONE
                                // selectable, not as a label with a caption
                                // beside it: the whole block has to be the
                                // click target, or the description becomes a
                                // dead strip an analyst can aim at and miss.
                                let clicked = if option.description.is_empty() {
                                    ui.selectable_label(chosen, &option.label).clicked()
                                } else {
                                    // This row's own interaction state, read
                                    // the way egui's own button reads it:
                                    // from LAST frame's response, because the
                                    // ink has to be chosen before the widget
                                    // that produces THIS frame's response is
                                    // added. `Button::atom_ui` peeks at
                                    // `ui.next_auto_id()` for exactly this and
                                    // then takes that id, so this is the same
                                    // row (egui 0.34.3, `widgets/button.rs`).
                                    //
                                    // Derived rather than special-cased, and
                                    // that matters: `draw_setting_row` is
                                    // wrapped in `add_enabled_ui(spec.enabled,
                                    // ..)`, and this branch is the only place
                                    // in it that picks a colour outright. Ink
                                    // and ground come from the same state
                                    // through the same egui call, so a
                                    // disabled row (which egui reports as
                                    // `Inactive`, since it suppresses hover on
                                    // disabled widgets, and then fades by
                                    // multiplying the painter's opacity)
                                    // cannot end up with an ink measured
                                    // against a ground it is not on.
                                    let state = ui
                                        .ctx()
                                        .read_response(ui.next_auto_id())
                                        .map(|response| response.widget_state())
                                        .unwrap_or_default();
                                    let inks = described_option_inks(ui.style(), state, chosen);
                                    let style = ui.style();
                                    let mut job = egui::text::LayoutJob::default();
                                    // The description wraps under the label
                                    // rather than running off the display.
                                    job.wrap.max_width = wrap_width;
                                    job.append(
                                        &option.label,
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::TextStyle::Button.resolve(style),
                                            color: inks.label,
                                            ..Default::default()
                                        },
                                    );
                                    job.append(
                                        &format!("\n{}", option.description),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::TextStyle::Small.resolve(style),
                                            color: inks.description,
                                            ..Default::default()
                                        },
                                    );
                                    // Laid out HERE, and handed to the widget
                                    // as a finished galley. A `LayoutJob`
                                    // handed to a widget has its `wrap`
                                    // REPLACED wholesale -
                                    // `WidgetText::into_galley_impl` assigns
                                    // `job.wrap = TextWrapping::
                                    // from_wrap_mode_and_width(wrap_mode,
                                    // available_width)` (egui 0.34.3,
                                    // `widget_text.rs`) - and inside a combo
                                    // menu that wrap mode is `Extend`, which
                                    // is `max_width: INFINITY`. So a wrap
                                    // width set on the job never survived the
                                    // journey and the longest description was
                                    // laid on one unbreakable line and cut off
                                    // at the display edge. A galley that is
                                    // already laid out is passed through
                                    // untouched, which is the one way to keep
                                    // a wrap width of our own.
                                    let galley = ui.ctx().fonts_mut(|fonts| fonts.layout_job(job));
                                    ui.selectable_label(chosen, galley).clicked()
                                };
                                if clicked && !chosen {
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
                    let width = TEXT_FIELD_POINTS.min((row_width - COMBO_LABEL_RESERVE).max(72.0));
                    if ui.add_sized([width, MIN_INTERACT_HEIGHT], edit).changed() {
                        set_value = Some(SettingValue::Text(value));
                    }
                    ui.label(&spec.label);
                }
            }
            // Visible only when the row's EFFECTIVE value differs from the
            // declared default - see `modified_specs` for why a presence test
            // on the stored file is the wrong question. A full-height button,
            // not a small one and not an icon on hover: `small_button` sizes
            // to the text line (~18 pt) and would undercut the 24 pt touch
            // floor this module promises.
            if modified && ui.button("Reset").clicked() {
                reset = true;
            }
        });
        if !spec.help.is_empty() {
            ui.label(egui::RichText::new(spec.help.as_str()).small().weak());
        }
        if modified {
            // The two numbers the mark stands for, spelled out. A dot that
            // says "this is not the default" without saying what the default
            // was leaves the analyst nothing to decide with.
            ui.label(
                egui::RichText::new(format!(
                    "Yours: {} \u{00b7} default: {}",
                    spec.kind.display(&effective),
                    spec.kind.display(&default),
                ))
                .small()
                .color(ui.visuals().hyperlink_color),
            );
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
    let row_width = ui.available_width();
    for family in ColorTableFamily::ALL {
        let installed = tables.for_family(family).clone();
        // Taken out of the popup rather than installed inside it: the rows
        // are borrowed from the cache for the length of the loop, and
        // installing writes the set the cache's key is read from.
        let mut picked = None;
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt(("settings-palette", palettes::family_id(family)))
                .selected_text(installed.name().to_owned())
                .width(combo_width(row_width, 230.0))
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

    /// The frame's whole shape list, as text, for comparing two renderings
    /// of the same thing.
    ///
    /// `Shape` is not `PartialEq` all the way down, so the debug rendering is
    /// the comparison: it carries every position, size, colour and glyph the
    /// tessellator was handed, which is exactly the "byte-identical
    /// photograph" question asked without a GPU in the loop. Two renderings
    /// that differ anywhere a camera could see differ here.
    fn section_shapes(draw: impl FnOnce(&mut egui::Ui)) -> String {
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
        format!("{:?}", output.shapes)
    }

    /// The Import button pressed twice against the same path: the first press
    /// previews, the second applies. Import deliberately takes two presses -
    /// see `PendingImport` - so a test that pressed once and then asserted on
    /// the store would be asserting the wrong thing.
    fn import_twice(
        store: &mut SettingsStore,
        registry: &SettingsRegistry,
        path: &str,
        outcome: &mut SettingsOutcome,
    ) -> (TransferReport, TransferReport) {
        let mut pending = None;
        let first = import(store, registry, None, None, path, &mut pending, outcome);
        let second = import(store, registry, None, None, path, &mut pending, outcome);
        (first, second)
    }

    /// The gate filter's rows say "off" on this page too, not a number.
    ///
    /// The same five settings are reachable from two places - the toolbar's
    /// own panel and this window - and they used to tell two different
    /// stories: `gate_filter_ui::threshold_row` has always written "off" at
    /// the leftmost stop, while the generic row here printed the number that
    /// happens to sit there, so the Radar page read "-35.0 dBZ", "-35.0 dBZ",
    /// "0.00" and "0.0 km" for a filter that was doing nothing at all. An
    /// analyst who reaches the filter through Settings rather than the chip
    /// could not tell the shipped state was off, and "-35.0 dBZ" reads as a
    /// threshold somebody chose.
    ///
    /// Driven through the real specs from the real catalog, and paired with an
    /// ordinary slider on the same page, which must still print its number.
    #[test]
    fn a_gate_filter_row_reads_off_at_its_off_position() {
        use crate::settings_ui::catalog::keys;
        let registry = catalog::registry();
        let mut store = SettingsStore::open(
            std::env::temp_dir().join("settings-ui-filter-rows-never-written.json"),
        );
        let mut outcome = SettingsOutcome::default();

        for id in [
            keys::radar::FILTER_MIN_DBZ,
            keys::radar::FILTER_VEL_NEEDS_DBZ,
            keys::radar::FILTER_MIN_RHO,
            keys::radar::FILTER_MIN_RANGE_KM,
        ] {
            let spec = registry
                .setting(keys::radar::CATEGORY, id)
                .unwrap_or_else(|| panic!("{id} is declared"))
                .clone();
            let texts = section_texts(|ui| {
                draw_setting_row(ui, &mut store, keys::radar::CATEGORY, &spec, &mut outcome);
            });
            assert!(
                texts.iter().any(|text| text == "off"),
                "{id} does not read 'off' at its off position: {texts:?}"
            );
            // Checked against the runs egui actually emits: a slider's
            // suffix is its own galley, so the off position used to arrive as
            // "-35.0" and "dBZ" side by side rather than as one string.
            for number in ["-35.0", "0.00", "0.0"] {
                assert!(
                    !texts.iter().any(|text| text == number),
                    "{id} prints {number:?}, which reads as a threshold somebody chose: \
                     {texts:?}"
                );
            }
        }

        // The pairing: an ordinary slider is untouched by this, because its
        // minimum is a number and not an off switch.
        let spec = registry
            .setting(keys::vol3d::CATEGORY, keys::vol3d::OPACITY)
            .expect("the 3D opacity slider is declared")
            .clone();
        let texts = section_texts(|ui| {
            draw_setting_row(ui, &mut store, keys::vol3d::CATEGORY, &spec, &mut outcome);
        });
        assert!(
            !texts.iter().any(|text| text == "off"),
            "an ordinary slider started calling its minimum 'off': {texts:?}"
        );
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

    /// Four cross-cutting settings pages, put through every piece of the
    /// window's generic page machinery.
    ///
    /// Deliberately driven off this explicit list rather than off the whole
    /// registry: a test over every page would keep passing if one of these four
    /// silently stopped being registered, which is the failure this catches.
    const AUDIT_PAGES: [&str; 4] = [
        catalog::keys::units::CATEGORY,
        catalog::keys::network::CATEGORY,
        catalog::keys::annotation::CATEGORY,
        catalog::keys::xsection::CATEGORY,
    ];

    #[test]
    fn every_page_the_audit_added_is_searchable() {
        let registry = catalog::registry();
        for id in AUDIT_PAGES {
            let category = registry
                .category(id)
                .unwrap_or_else(|| panic!("the {id} page is registered"));
            assert!(
                !category.settings.is_empty(),
                "{id} declares no rows, so there is nothing to find"
            );
            // Typing the page's own id lists every row on it: the id is in
            // the haystack precisely so a page name reaches its knobs from
            // anywhere in the window.
            for spec in &category.settings {
                assert!(
                    spec_matches(category, spec, &search_terms(id)),
                    "searching {id:?} does not reach {}/{}",
                    category.id,
                    spec.id
                );
            }
        }
    }

    /// Subsections keep a page from becoming a wall of controls. These pages
    /// have to go through the same split, and the split has to be lossless or a
    /// row is declared and never drawn.
    #[test]
    fn every_page_the_audit_added_survives_the_subsection_split() {
        let registry = catalog::registry();
        for id in AUDIT_PAGES {
            let category = registry
                .category(id)
                .unwrap_or_else(|| panic!("the {id} page is registered"));
            let through_sections: Vec<&str> = category
                .sections()
                .iter()
                .flat_map(|section| section.settings.iter())
                .map(|spec| spec.id.as_str())
                .collect();
            let declared: Vec<&str> = category
                .settings
                .iter()
                .map(|spec| spec.id.as_str())
                .collect();
            assert_eq!(
                through_sections, declared,
                "{id}: the section split must emit every declared row exactly \
                 once, in order"
            );
        }
    }

    /// Every listed page must participate in per-page reset. A page whose
    /// values a reset cannot reach has no way back to the shipped behaviour.
    #[test]
    fn every_page_the_audit_added_can_be_reset_through_the_window() {
        let registry = catalog::registry();
        let dir = scratch_dir("audit-pages-reset");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        for id in AUDIT_PAGES {
            let category = registry
                .category(id)
                .unwrap_or_else(|| panic!("the {id} page is registered"));
            for spec in &category.settings {
                store.set(id, &spec.id, spec.kind.default_value());
            }
            assert!(
                store.stored_categories().contains(&id),
                "{id}: the page's values did not reach the file"
            );
            assert!(
                store.reset_category(id),
                "{id}: a page reset reported it removed nothing"
            );
            assert!(
                store.stored_ids(id).is_empty(),
                "{id}: values survived the page reset"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A named snapshot has to carry every listed page, or switching profiles
    /// silently reverts those settings.
    ///
    /// Dropping one of these categories inside `snapshot_for_profile` fails
    /// this test and the drawn-unit round trip in `app.rs`.
    #[test]
    fn a_profile_snapshot_carries_every_page_the_audit_added() {
        let registry = catalog::registry();
        let dir = scratch_dir("audit-pages-snapshot");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        for id in AUDIT_PAGES {
            let category = registry
                .category(id)
                .unwrap_or_else(|| panic!("the {id} page is registered"));
            for spec in &category.settings {
                store.set(id, &spec.id, spec.kind.default_value());
            }
        }
        let snapshot = settings::profiles::snapshot_for_profile(store.document());
        for id in AUDIT_PAGES {
            let category = registry
                .category(id)
                .unwrap_or_else(|| panic!("the {id} page is registered"));
            let kept = snapshot
                .values
                .get(id)
                .unwrap_or_else(|| panic!("a profile snapshot dropped the whole {id} page"));
            for spec in &category.settings {
                assert!(
                    kept.contains_key(&spec.id),
                    "a profile snapshot dropped {id}/{}",
                    spec.id
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_matches_labels_help_and_ids_case_insensitively() {
        let registry = catalog::registry();
        let category = registry
            .category(catalog::keys::navigation::CATEGORY)
            .expect("the navigation page is declared");
        let spec = registry
            .setting(
                catalog::keys::navigation::CATEGORY,
                catalog::keys::navigation::ZOOM_PER_NOTCH,
            )
            .expect("zoom_per_notch is declared");
        let hit = |needle: &str| spec_matches(category, spec, &search_terms(needle));
        assert!(hit("zoom"));
        assert!(hit("NOTCH"), "case-insensitive");
        assert!(hit("wheel click"), "matches help text");
        assert!(hit("zoom_per_notch"), "matches the raw id");
        assert!(hit("navigation"), "matches the page it lives on");
        assert!(!hit("differential phase"));
    }

    /// Every typed word has to land, so a second word narrows the list
    /// instead of widening it. With a dozen contributed pages, an OR search
    /// returns a page of results for any two common words and is useless.
    #[test]
    fn every_search_word_must_match_so_adding_one_narrows_the_result() {
        let registry = catalog::registry();
        let category = registry
            .category(catalog::keys::navigation::CATEGORY)
            .expect("the navigation page is declared");
        let spec = registry
            .setting(
                catalog::keys::navigation::CATEGORY,
                catalog::keys::navigation::ZOOM_PER_NOTCH,
            )
            .expect("zoom_per_notch is declared");
        assert!(spec_matches(category, spec, &search_terms("zoom")));
        assert!(spec_matches(category, spec, &search_terms("zoom notch")));
        assert!(
            !spec_matches(category, spec, &search_terms("zoom hurricane")),
            "a word that matches nothing must sink the whole match"
        );
        // Words in either order, and out of different fields: "click" is in
        // the help, "zoom" is in the label.
        assert!(spec_matches(category, spec, &search_terms("click zoom")));
        assert!(
            !spec_matches(category, spec, &[]),
            "an empty search is not a match-everything"
        );
    }

    /// Deliverable of the subsection work, stated as the thing that could go
    /// wrong: rendering a page through the section split must emit exactly
    /// the widget stream the flat loop emitted, shape for shape, for every
    /// page that declares no headings.
    #[test]
    fn an_ungrouped_page_draws_the_same_shapes_it_did_before_sections() {
        let registry = catalog::registry();
        let dir = scratch_dir("section-identity");
        for category in registry.categories() {
            if category.has_sections() {
                continue;
            }
            let mut store = SettingsStore::open(dir.join("settings.json"));
            let mut outcome = SettingsOutcome::default();
            let flat = section_shapes(|ui| {
                for spec in &category.settings {
                    draw_setting_row(ui, &mut store, &category.id, spec, &mut outcome);
                }
            });
            let mut store = SettingsStore::open(dir.join("settings.json"));
            let mut outcome = SettingsOutcome::default();
            let sectioned = section_shapes(|ui| {
                for section in category.sections() {
                    if !section.heading.is_empty() {
                        ui.add_space(12.0);
                        ui.strong(section.heading);
                        ui.add_space(3.0);
                    }
                    for spec in section.settings {
                        draw_setting_row(ui, &mut store, &category.id, spec, &mut outcome);
                    }
                }
            });
            assert_eq!(
                flat, sectioned,
                "the {} page renders differently through the section split",
                category.id
            );
            assert!(!flat.is_empty(), "the {} page drew nothing", category.id);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And the other half of that promise: a page that DOES declare headings
    /// must actually show them, or the feature is a no-op that passes its own
    /// identity test.
    #[test]
    fn a_grouped_page_puts_its_headings_on_screen_above_its_rows() {
        let registry = catalog::registry();
        let category = registry
            .category(catalog::keys::vol3d::CATEGORY)
            .expect("the 3D page is declared");
        assert!(
            category.has_sections(),
            "the 3D page is the long one; it is grouped on purpose"
        );
        let dir = scratch_dir("grouped-headings");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        let mut outcome = SettingsOutcome::default();
        let mut confirm = Confirmations::default();
        let mut tables = Arc::new(ColorTableSet::default());
        let mut profiles_state = profiles::ProfilesUi::default();
        let texts = section_texts(|ui| {
            draw_category_page(
                ui,
                &registry,
                &mut store,
                catalog::keys::vol3d::CATEGORY,
                PageContext {
                    confirm: &mut confirm,
                    profiles: &mut profiles_state,
                    palettes: PaletteContext {
                        color_tables: Some(&mut tables),
                        user_tables: None,
                        offers: &mut PaletteOfferCache::default(),
                    },
                },
                &mut outcome,
            );
        });
        for section in category.sections() {
            if section.heading.is_empty() {
                continue;
            }
            assert!(
                texts.iter().any(|text| text == section.heading),
                "the heading {:?} is not on the page: {texts:?}",
                section.heading
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mark has to mean something. A stored value that is character for
    /// character the default is NOT a modification - and the application
    /// stores exactly those, every frame, for every knob the toolbar mirrors.
    #[test]
    fn only_a_value_that_differs_from_the_default_counts_as_modified() {
        let registry = catalog::registry();
        let dir = scratch_dir("modified-marks");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        let category = registry
            .category(catalog::keys::map::CATEGORY)
            .expect("the map page is declared");
        assert!(
            modified_specs(&store, category).is_empty(),
            "a fresh store has nothing modified"
        );

        // Stored, but stored AS the default: not a modification.
        let spec = registry
            .setting(
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM,
            )
            .expect("imagery_dim is declared");
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            spec.kind.default_value(),
        );
        assert!(
            store
                .value(
                    catalog::keys::map::CATEGORY,
                    catalog::keys::map::IMAGERY_DIM
                )
                .is_some(),
            "the value really is in the file"
        );
        assert!(
            modified_specs(&store, category).is_empty(),
            "a stored default is not a modification"
        );

        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        let modified = modified_specs(&store, category);
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].id, catalog::keys::map::IMAGERY_DIM);

        // Out of range in the file resolves to the clamp, and the clamp is
        // not the default, so it is still a modification - the mark follows
        // what the pane is actually drawing.
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(99.0),
        );
        assert_eq!(modified_specs(&store, category).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A row that is not at its default has to SAY so, in words, on the page
    /// - the mark, both values, and its own Reset.
    #[test]
    fn a_modified_row_shows_the_mark_its_value_and_the_default_it_left() {
        let registry = catalog::registry();
        let dir = scratch_dir("modified-row-words");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        let spec = registry
            .setting(
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM,
            )
            .expect("imagery_dim is declared")
            .clone();
        let mut outcome = SettingsOutcome::default();
        let texts = section_texts(|ui| {
            draw_setting_row(
                ui,
                &mut store,
                catalog::keys::map::CATEGORY,
                &spec,
                &mut outcome,
            );
        });
        let joined = texts.join(" | ");
        assert!(joined.contains(MODIFIED_MARK), "{joined}");
        assert!(joined.contains("0.72"), "the analyst's value: {joined}");
        assert!(joined.contains("0.35"), "the default it left: {joined}");
        assert!(joined.contains("Reset"), "{joined}");

        // And at the default: no mark, no Reset, no extra line.
        store.reset(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
        );
        let texts = section_texts(|ui| {
            draw_setting_row(
                ui,
                &mut store,
                catalog::keys::map::CATEGORY,
                &spec,
                &mut outcome,
            );
        });
        let joined = texts.join(" | ");
        assert!(!joined.contains(MODIFIED_MARK), "{joined}");
        assert!(!joined.contains("Reset"), "{joined}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resetting a page must state what it is about to throw away, by name,
    /// before it throws anything away. The first press arms; only the second
    /// resets.
    #[test]
    fn resetting_a_page_names_every_value_it_would_discard_before_discarding_it() {
        let registry = catalog::registry();
        let dir = scratch_dir("page-reset-confirm");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::SITE_LABELS,
            SettingValue::Bool(false),
        );
        let category = registry
            .category(catalog::keys::map::CATEGORY)
            .expect("map page")
            .clone();

        // Armed: the words are on screen and NOTHING has been reset.
        let mut confirm = Confirmations {
            page: Some(category.id.clone()),
            ..Confirmations::default()
        };
        let mut outcome = SettingsOutcome::default();
        let texts = section_texts(|ui| {
            draw_page_reset(ui, &mut store, &category, &mut confirm, None, &mut outcome);
        });
        let joined = texts.join(" | ");
        assert!(joined.contains("Imagery dim"), "{joined}");
        assert!(joined.contains("0.72"), "the value that would go: {joined}");
        assert!(joined.contains("Cancel"), "{joined}");
        assert!(
            outcome.changed.is_empty(),
            "arming a reset must not reset anything"
        );
        assert_eq!(
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM
            ),
            0.72,
            "the value is still there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole-application reset states its blast radius in the same way,
    /// and counts the rows a build cannot even show.
    #[test]
    fn the_reset_everything_plan_counts_what_it_would_remove_including_unknown_ids() {
        let registry = catalog::registry();
        let dir = scratch_dir("reset-plan");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        assert_eq!(ResetPlan::survey(&registry, &store, None).stored_values, 0);

        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        // A value from a build that has a page this one does not.
        store.set("quantum_overlay", "entanglement", SettingValue::Float(0.7));
        let plan = ResetPlan::survey(&registry, &store, None);
        assert_eq!(plan.stored_values, 2);
        assert_eq!(plan.unknown_values, 1);
        assert_eq!(plan.unknown_categories, ["quantum_overlay"]);
        assert_eq!(plan.changed, vec![("Map".to_owned(), 1)]);
        let lines = plan.confirmation_lines().join(" | ");
        assert!(lines.contains("Map: 1 back to the default"), "{lines}");
        assert!(lines.contains("does not declare"), "{lines}");
        assert!(
            lines.contains("pane layout") || lines.contains("pane layout"),
            "the confirmation must say what it will NOT touch: {lines}"
        );

        // An id a newer build added to a page this one HAS is the same class
        // of value, and the reset removes it just the same. Its page has to be
        // named too, or the panel says nothing at all about a value that is
        // about to go.
        store.set(
            catalog::keys::map::CATEGORY,
            "hologram_mode",
            SettingValue::Bool(true),
        );
        let plan = ResetPlan::survey(&registry, &store, None);
        assert_eq!(plan.unknown_values, 2);
        assert_eq!(plan.unknown_categories, ["map", "quantum_overlay"]);
        assert!(
            plan.confirmation_lines()
                .join(" | ")
                .contains("2 values under ids this build does not declare")
        );
        store.reset(catalog::keys::map::CATEGORY, "hologram_mode");

        // A non-default colour table is part of the blast radius too.
        let mut tables = Arc::new(ColorTableSet::default());
        assert!(!ResetPlan::survey(&registry, &store, Some(&tables)).palettes);
        let pick = color_tables::builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .nth(2)
            .expect("velocity catalog depth");
        Arc::make_mut(&mut tables).set_family(ColorTableFamily::Velocity, pick);
        assert!(ResetPlan::survey(&registry, &store, Some(&tables)).palettes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Export then import, through the real window's own entry points, on a
    /// real file: the values come back and the summary says what moved.
    #[test]
    fn a_document_exported_from_this_window_imports_back_into_it_with_a_summary() {
        let registry = catalog::registry();
        let dir = scratch_dir("transfer-round-trip");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        let export_path = dir.join("carried.json");
        let report = export(&store, &export_path.display().to_string(), None);
        assert!(
            matches!(report, TransferReport::Exported(_)),
            "export failed"
        );
        assert!(export_path.exists());

        // Move on, then bring the file back.
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.10),
        );
        let mut outcome = SettingsOutcome::default();
        let mut pending = None;
        let typed = export_path.display().to_string();
        let preview = import(
            &mut store,
            &registry,
            None,
            None,
            &typed,
            &mut pending,
            &mut outcome,
        );
        assert!(
            matches!(preview, TransferReport::ImportPreview(_)),
            "the first press must show what it would do, not do it"
        );
        assert_eq!(
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM
            ),
            0.10,
            "nothing may move on the press that only reads the file"
        );
        let report = import(
            &mut store,
            &registry,
            None,
            None,
            &typed,
            &mut pending,
            &mut outcome,
        );
        let TransferReport::Imported(summary) = report else {
            panic!("the second press must apply the import");
        };
        assert_eq!(
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM
            ),
            0.72,
            "the exported value must be what is in force again"
        );
        let words = format!("{} {}", summary.headline(), summary.lines().join(" | "));
        assert!(words.contains("Imagery dim"), "{words}");
        assert!(words.contains("0.72"), "{words}");
        assert!(
            words.contains("Pane layout"),
            "the summary must say what it did not import: {words}"
        );
        assert!(
            !outcome.changed.is_empty(),
            "the application has to be told to re-read the settings"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The refusal path, end to end through the window's own function: a file
    /// this build cannot read changes nothing and says why.
    #[test]
    fn an_unreadable_document_is_refused_in_words_and_changes_nothing() {
        let registry = catalog::registry();
        let dir = scratch_dir("transfer-refusal");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        let junk = dir.join("junk.json");
        std::fs::write(&junk, "{ not json at all").expect("write junk");

        let mut outcome = SettingsOutcome::default();
        let report = import(
            &mut store,
            &registry,
            None,
            None,
            &junk.display().to_string(),
            &mut None,
            &mut outcome,
        );
        let TransferReport::Refused(reason) = report else {
            panic!("junk must be refused");
        };
        assert!(reason.contains("not valid JSON"), "{reason}");
        assert!(
            outcome.changed.is_empty(),
            "a refused import must not tell the application anything changed"
        );
        assert_eq!(
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM
            ),
            0.72,
            "the analyst's value must be untouched"
        );

        // And a file from a build with a newer wrapper shape.
        let future = dir.join("future.json");
        std::fs::write(&future, r#"{"version": 99, "values": {}}"#).expect("write future");
        let report = import(
            &mut store,
            &registry,
            None,
            None,
            &future.display().to_string(),
            &mut None,
            &mut outcome,
        );
        let TransferReport::Refused(reason) = report else {
            panic!("a newer format must be refused");
        };
        assert!(reason.contains("newer build"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason the file format carries what it does not understand:
    /// an analyst who ran a newer build and came back to this one must be
    /// able to re-import their own export without losing the newer build's
    /// settings. Through the window's own `import`, on a real file.
    #[test]
    fn importing_keeps_the_values_this_build_cannot_show_and_says_it_kept_them() {
        let registry = catalog::registry();
        let dir = scratch_dir("import-keeps-unknown");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.10),
        );
        // What a newer build left behind: a whole page this one does not
        // have, and a knob it added to a page this one does have.
        store.set("quantum_overlay", "entanglement", SettingValue::Float(0.7));
        store.set(
            catalog::keys::map::CATEGORY,
            "hologram_mode",
            SettingValue::Bool(true),
        );

        // An ordinary document written by THIS build: it simply has never
        // heard of either of them.
        let incoming = dir.join("bench.json");
        std::fs::write(
            &incoming,
            r#"{"version": 1, "values": {"map": {"imagery_dim": 0.72}}, "workspace": {}}"#,
        )
        .expect("write incoming");

        let mut outcome = SettingsOutcome::default();
        let (_, report) = import_twice(
            &mut store,
            &registry,
            &incoming.display().to_string(),
            &mut outcome,
        );
        let TransferReport::Imported(summary) = report else {
            panic!("the import was refused");
        };

        assert_eq!(
            store.value("quantum_overlay", "entanglement"),
            Some(SettingValue::Float(0.7)),
            "an import must not delete a page this build does not declare"
        );
        assert_eq!(
            store.value(catalog::keys::map::CATEGORY, "hologram_mode"),
            Some(SettingValue::Bool(true)),
            "an import must not delete an id this build does not declare"
        );
        assert_eq!(
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM
            ),
            0.72,
            "the file still decides every setting this build declares"
        );
        let words = format!("{} {}", summary.headline(), summary.lines().join(" | "));
        assert!(
            words.contains("quantum_overlay") && words.contains("map"),
            "the summary has to name what it kept: {words}"
        );
        assert!(words.contains("kept"), "{words}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Import is destructive and has no undo, so it takes two presses with the
    /// words in between - the same shape as every reset in this window, and
    /// what `settings::transfer::summarize` says in its own documentation it
    /// exists for.
    #[test]
    fn an_import_says_what_it_would_do_before_it_does_it_and_a_new_path_re_asks() {
        let registry = catalog::registry();
        let dir = scratch_dir("import-preview");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.10),
        );
        let one = dir.join("one.json");
        std::fs::write(
            &one,
            r#"{"version": 1, "values": {"map": {"imagery_dim": 0.72}}}"#,
        )
        .expect("write one");
        let two = dir.join("two.json");
        std::fs::write(
            &two,
            r#"{"version": 1, "values": {"map": {"imagery_dim": 0.31}}}"#,
        )
        .expect("write two");

        let mut outcome = SettingsOutcome::default();
        let mut pending = None;
        let dim = |store: &SettingsStore| {
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM,
            )
        };

        // First press: the words, in the tense of something that has not
        // happened, and a store that has not moved.
        let preview = import(
            &mut store,
            &registry,
            None,
            None,
            &one.display().to_string(),
            &mut pending,
            &mut outcome,
        );
        let TransferReport::ImportPreview(summary) = preview else {
            panic!("the first press must preview, not apply");
        };
        assert!(
            summary.preview_headline().contains("would change"),
            "{}",
            summary.preview_headline()
        );
        assert_eq!(dim(&store), 0.10, "the first press must change nothing");
        assert!(
            outcome.changed.is_empty(),
            "and must not tell the application anything moved"
        );

        // The path is edited before the second press: the file that was shown
        // must NOT be the one applied.
        let re_asked = import(
            &mut store,
            &registry,
            None,
            None,
            &two.display().to_string(),
            &mut pending,
            &mut outcome,
        );
        assert!(
            matches!(re_asked, TransferReport::ImportPreview(_)),
            "a changed path has to be read and shown, not applied on the spot"
        );
        assert_eq!(dim(&store), 0.10);

        // Pressing again on the path that was last shown applies that one.
        let applied = import(
            &mut store,
            &registry,
            None,
            None,
            &two.display().to_string(),
            &mut pending,
            &mut outcome,
        );
        assert!(matches!(applied, TransferReport::Imported(_)));
        assert_eq!(
            dim(&store),
            0.31,
            "the file that was shown is the one that lands"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The likeliest wrong path is not junk - it is another valid JSON file
    /// in the same folder. Importing one used to empty the store and report
    /// only how many settings had changed.
    #[test]
    fn importing_a_file_that_is_not_a_settings_file_refuses_and_leaves_the_store_alone() {
        let registry = catalog::registry();
        let dir = scratch_dir("import-wrong-file");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        let table = dir.join("Ramp Velocity.json");
        std::fs::write(
            &table,
            r#"{"stops":[{"value":-60,"rgb":[0,0,0]}],"name":"Ramp Velocity","family":"velocity"}"#,
        )
        .expect("write colour table");

        let mut outcome = SettingsOutcome::default();
        let report = import(
            &mut store,
            &registry,
            None,
            None,
            &table.display().to_string(),
            &mut None,
            &mut outcome,
        );
        let TransferReport::Refused(reason) = report else {
            panic!("a colour table is not a settings document");
        };
        assert!(reason.contains("not a settings document"), "{reason}");
        assert!(
            outcome.changed.is_empty(),
            "a refused import must not tell the application anything changed"
        );
        assert_eq!(
            store.stored_categories(),
            [catalog::keys::map::CATEGORY],
            "the store must be exactly as it was"
        );
        assert_eq!(
            store.effective_float(
                &registry,
                catalog::keys::map::CATEGORY,
                catalog::keys::map::IMAGERY_DIM
            ),
            0.72,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page reset removes the page's whole stored map. The list on the
    /// confirmation is built from what this build declares, so the rest has
    /// to be named separately or the confirmation is not one.
    #[test]
    fn a_page_reset_names_the_ids_it_discards_that_this_build_does_not_declare() {
        let registry = catalog::registry();
        let dir = scratch_dir("page-reset-unknown");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        store.set(
            catalog::keys::map::CATEGORY,
            catalog::keys::map::IMAGERY_DIM,
            SettingValue::Float(0.72),
        );
        store.set(
            catalog::keys::map::CATEGORY,
            "hologram_mode",
            SettingValue::Bool(true),
        );
        let category = registry
            .category(catalog::keys::map::CATEGORY)
            .expect("map page")
            .clone();

        let mut confirm = Confirmations {
            page: Some(category.id.clone()),
            ..Confirmations::default()
        };
        let mut outcome = SettingsOutcome::default();
        let texts = section_texts(|ui| {
            draw_page_reset(ui, &mut store, &category, &mut confirm, None, &mut outcome);
        });
        let joined = texts.join(" | ");
        assert!(
            joined.contains("hologram_mode"),
            "the confirmation must name the value it is about to discard: {joined}"
        );
        assert!(
            joined.contains("does not declare"),
            "and say why it has no row: {joined}"
        );
        // And the press really does remove it, which is what makes the line
        // above true rather than decorative.
        assert_eq!(store.stored_ids(&category.id).len(), 2);
        store.reset_category(&category.id);
        assert!(store.stored_ids(&category.id).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Export renames over its target. With no file dialog in the workspace
    /// the path is whatever was typed, so an existing file gets named once
    /// before it is written over.
    #[test]
    fn exporting_over_an_existing_file_names_it_first_and_writes_on_the_second_press() {
        let dir = scratch_dir("export-overwrite");
        let store = SettingsStore::open(dir.join("settings.json"));
        let target = dir.join("Ramp Velocity.json");
        let precious = "{\"stops\":[]}";
        std::fs::write(&target, precious).expect("write the file already there");

        let typed = target.display().to_string();
        let report = export(&store, &typed, None);
        let TransferReport::ExportWouldOverwrite(named) = report else {
            panic!("an existing file must not be written over unasked");
        };
        assert_eq!(named, target);
        assert_eq!(
            std::fs::read_to_string(&target).expect("still there"),
            precious,
            "nothing may be written by the press that refuses"
        );

        // Armed for a DIFFERENT path: still refused, because the file this
        // press would destroy has not been named yet.
        let elsewhere = dir.join("other.json");
        let report = export(&store, &typed, Some(&elsewhere));
        assert!(
            matches!(report, TransferReport::ExportWouldOverwrite(_)),
            "arming one path must not arm another"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("still there"),
            precious
        );

        // Armed for this one: it goes through.
        let report = export(&store, &typed, Some(&target));
        assert!(
            matches!(report, TransferReport::Exported(_)),
            "second press"
        );
        assert_ne!(
            std::fs::read_to_string(&target).expect("written"),
            precious,
            "the second press has to actually write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The path field offers a default once. Refilling it every frame made
    /// the field impossible to clear and both "type a path first" messages
    /// unreachable.
    #[test]
    fn the_transfer_path_can_be_cleared_and_the_empty_path_messages_are_reachable() {
        let registry = catalog::registry();
        let dir = scratch_dir("transfer-path-clearable");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        let mut transfer = TransferUi::default();
        let mut outcome = SettingsOutcome::default();

        let mut draw = |transfer: &mut TransferUi, store: &mut SettingsStore| {
            let _ = section_texts(|ui| {
                draw_transfer_section(ui, transfer, &registry, store, None, None, &mut outcome);
            });
        };
        draw(&mut transfer, &mut store);
        assert!(
            !transfer.path.is_empty(),
            "the first frame offers a default path"
        );

        transfer.path.clear();
        draw(&mut transfer, &mut store);
        assert!(
            transfer.path.is_empty(),
            "a cleared path must stay cleared, not be refilled next frame"
        );

        // Which is what makes these two reachable at all.
        let TransferReport::ExportFailed(reason) = export(&store, "", None) else {
            panic!("an empty path cannot be exported to");
        };
        assert!(reason.contains("Type a file name"), "{reason}");
        let TransferReport::Refused(reason) = import(
            &mut store,
            &registry,
            None,
            None,
            "",
            &mut None,
            &mut outcome,
        ) else {
            panic!("an empty path cannot be imported from");
        };
        assert!(reason.contains("Type the path"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Escape belongs to whatever is in front. The search strip is drawn in
    /// the top panel, ahead of the page, so without a guard it took the key
    /// off an open combo popup and cleared the search - which takes the whole
    /// result list, and the popup, with it.
    #[test]
    fn escape_leaves_the_search_alone_while_a_popup_is_open() {
        fn escape_once(state: &mut SettingsUi, popup_open: bool) {
            let context = egui::Context::default();
            if popup_open {
                egui::Popup::open_id(&context, egui::Id::new("a-combo-in-the-results"));
            }
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(600.0, 400.0),
                )),
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            };
            let mut state = Some(state);
            let _ = context.run_ui(input, |ui| {
                if let Some(state) = state.take() {
                    draw_search_field(ui, state);
                }
            });
        }

        let mut state = SettingsUi {
            search: "opacity".to_owned(),
            ..SettingsUi::default()
        };
        escape_once(&mut state, true);
        assert_eq!(
            state.search, "opacity",
            "Escape with a popup open must close the popup, not empty the search"
        );

        escape_once(&mut state, false);
        assert!(
            state.search.is_empty(),
            "with nothing else claiming it, Escape still clears the search"
        );
    }

    /// Exporting the live file onto itself is the one path that looks like it
    /// worked and leaves the analyst with no backup at all.
    #[test]
    fn exporting_onto_the_live_settings_file_is_refused_with_the_reason() {
        let dir = scratch_dir("transfer-self");
        let store = SettingsStore::open(dir.join("settings.json"));
        let report = export(&store, &store.path().display().to_string(), None);
        let TransferReport::ExportFailed(reason) = report else {
            panic!("writing the live file onto itself must be refused");
        };
        assert!(reason.contains("live settings file"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The category column has to leave a page worth reading beside it, at
    /// any window width the window itself allows.
    #[test]
    fn the_category_column_never_takes_more_than_its_share_of_a_narrow_window() {
        // The window clamps its own width to at least 280 points.
        for available in [280.0_f32, 320.0, 500.0, 760.0, 940.0] {
            let width = category_column_width(available);
            assert!(width <= CATEGORY_COLUMN_POINTS, "{available}: {width}");
            assert!(
                width <= available * 0.4,
                "at {available} points wide the column took {width}"
            );
            assert!(
                width >= CATEGORY_COLUMN_MIN_POINTS,
                "{available}: {width} is below the readable floor"
            );
            // The page beside it has to hold a slider, its readout and its
            // label: `slider_track` gives up the track before the label, but
            // only down to a floor, and below about 180 points nothing fits
            // however the row is cut.
            assert!(
                available - width >= 180.0,
                "at {available} points wide the page got only {}",
                available - width
            );
        }
        // Full width where there is room for it.
        assert_eq!(category_column_width(940.0), CATEGORY_COLUMN_POINTS);
    }

    /// A control on a page with room takes exactly the width the theme
    /// declares - so an ordinary window renders exactly as it always has -
    /// and gives width up only on a page that has none.
    #[test]
    fn a_control_keeps_its_declared_width_until_the_page_cannot_hold_it() {
        let context = egui::Context::default();
        let mut probe = None;
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1400.0, 1400.0),
                )),
                ..Default::default()
            },
            |ui| {
                let declared = ui.spacing().slider_width;
                // The reserve a short label asks for is the floor, so these
                // three read exactly as they did before the reserve was
                // measured per row.
                let floor = slider_label_reserve(ui, "Opacity");
                probe = Some((
                    declared,
                    // A page the size the window opens at: unchanged.
                    slider_track(ui, 584.0, floor),
                    // A phone-shaped page: shortened, not clipped.
                    slider_track(ui, 205.0, floor),
                    // Absurdly narrow: still a track, never zero or negative.
                    slider_track(ui, 40.0, floor),
                    // A long label on that same phone-shaped page asks for
                    // more than the floor, and the track - not the label -
                    // gives it up. This is the row that used to read
                    // "Live poll interva".
                    slider_track(ui, 205.0, slider_label_reserve(ui, "Live poll interval")),
                ));
            },
        );
        let (declared, wide, narrow, absurd, long_label) = probe.expect("the probe ran");
        assert_eq!(wide, declared, "a page with room changes nothing");
        assert!(
            narrow < declared && narrow >= MIN_SLIDER_POINTS,
            "a narrow page gives up track, down to a floor: {narrow}"
        );
        assert_eq!(absurd, MIN_SLIDER_POINTS);
        assert!(
            long_label < narrow,
            "a long label on a narrow page must take its room out of the track, or it is cut              off the right edge: {long_label} vs {narrow}"
        );

        assert_eq!(combo_width(584.0, COMBO_POINTS), COMBO_POINTS);
        assert!(combo_width(205.0, COMBO_POINTS) < COMBO_POINTS);
        assert_eq!(combo_width(40.0, COMBO_POINTS), MIN_COMBO_POINTS);
    }

    /// A search that finds nothing has to say so, and say what it looked in -
    /// silence is indistinguishable from a broken filter.
    #[test]
    fn a_search_with_no_matches_says_so_and_says_where_it_looked() {
        let registry = catalog::registry();
        let dir = scratch_dir("search-empty");
        let mut store = SettingsStore::open(dir.join("settings.json"));
        let mut outcome = SettingsOutcome::default();
        let texts = section_texts(|ui| {
            draw_search_results(
                ui,
                &registry,
                &mut store,
                &search_terms("zzzz nothing here"),
                &mut outcome,
            );
        });
        let joined = texts.join(" | ");
        assert!(joined.contains("Nothing matches"), "{joined}");
        assert!(joined.contains("zzzz nothing here"), "{joined}");
        assert!(joined.contains("stored id"), "{joined}");

        // And a search that DOES match names the page each row came from.
        let texts = section_texts(|ui| {
            draw_search_results(
                ui,
                &registry,
                &mut store,
                &search_terms("opacity"),
                &mut outcome,
            );
        });
        let joined = texts.join(" | ");
        assert!(
            joined.contains("3D Volume"),
            "a result must carry the page it lives on: {joined}"
        );
        assert!(
            joined.contains("Opacity ramp"),
            "and the subsection it came out of: {joined}"
        );
        assert!(joined.contains("matches on"), "{joined}");
        let _ = std::fs::remove_dir_all(&dir);
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
