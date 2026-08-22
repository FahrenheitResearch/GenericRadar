//! The product chooser: grouped, searchable, and driven from the keyboard.
//!
//! What this replaces is a flat `ComboBox` of every product in one
//! undifferentiated column. Nothing on screen said what a product measured,
//! what unit it came back in, or whether the volume on screen could draw it at
//! all, and reaching any of it needed a mouse. During a warning that is the
//! wrong instrument.
//!
//! Three rules hold this file together.
//!
//! * Every product fact it shows is read from `product_engine`'s registry:
//!   group, label, aliases, unit, declared range, citation - and the colour
//!   family from the render path that installs it. This file declares no
//!   second catalog. The last time product facts lived in two places they
//!   drifted, which is the reason the registry exists at all.
//! * It reaches into the application nowhere. Everything arrives in
//!   [`ProductPickerInput`], the way the 3D shell takes `Vol3dPaneInput`, so
//!   the picker can be tested without a `WorkstationApp`.
//! * A product this volume cannot draw stays on screen, greyed, with the
//!   reason beside it. Hiding it makes a correctly working application look
//!   broken: "where did ZDR go" is a support call, "no ZDR in this volume" is
//!   an answer.

use std::collections::BTreeSet;

use color_tables::user::UserTableLibrary;
use color_tables::{ColorTable, ColorTableFamily, ColorTableSet};
use eframe::egui;
use product_engine::registry::DerivedVolumeId;
use product_engine::{
    AvailabilityQualifier, ProductDescriptor, ProductGroup, ProductRegistry, ProductVisibility,
    ValueRange,
};
use render2d::color_family_for_moment;

use crate::product::DisplayProduct;
use crate::product_availability::{ProductAvailabilityIndex, ProductEntry};
use crate::settings_ui::PaletteOfferCache;
use crate::source_fields::{SourceField, SourceFieldCatalog, SourceFieldMetadata};

/// Wide enough for a display name and its range on one line without wrapping.
const PICKER_WIDTH: f32 = 468.0;
/// Roughly ten rows. Past that the list scrolls rather than growing off-screen.
const LIST_MAX_HEIGHT: f32 = 340.0;
const ROW_HEIGHT: f32 = 34.0;
const SOURCE_FIELD_NAME_HEIGHT: f32 = 25.0;
const SOURCE_FIELD_METADATA_HEIGHT: f32 = 17.0;
const GROUP_HEADER_HEIGHT: f32 = 26.0;
const PALETTE_ROW_HEIGHT: f32 = 26.0;
const SWATCH_WIDTH: f32 = 150.0;
/// The edit affordance on a palette row. Two touch targets wide, because the
/// label is a word and mobile is a standing requirement (WCAG 2.2 SC 2.5.8 is
/// 24 points per side and this row is exactly that tall).
const EDIT_BUTTON_WIDTH: f32 = 48.0;
/// Strips per palette preview. Fifty is past the point where the seams show at
/// 150 px and well short of the cost of a per-pixel ramp.
const SWATCH_STRIPS: usize = 50;

/// The toolbar's panel fill, so the picker reads as part of it rather than as
/// a window that landed on top.
const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(10, 13, 17);
const ROW_HOVER: egui::Color32 = egui::Color32::from_rgb(21, 28, 37);
const ROW_SELECTED: egui::Color32 = egui::Color32::from_rgb(25, 39, 53);
const FIELD_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(16, 21, 28);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(78, 180, 244);
const TEXT: egui::Color32 = egui::Color32::from_rgb(226, 236, 246);
const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(166, 184, 196);
const TEXT_FAINT: egui::Color32 = egui::Color32::from_rgb(108, 124, 138);
const WARNING: egui::Color32 = egui::Color32::from_rgb(240, 176, 86);
const SEPARATOR: egui::Color32 = egui::Color32::from_rgb(45, 57, 67);

/// Where the analyst is in the picker, across frames.
///
/// Held by the application beside the picker's open flag. The widget keeps
/// nothing of its own between frames, which is what makes a headless test able
/// to drive it one frame at a time.
#[derive(Debug, Default)]
pub struct ProductPickerState {
    filter: String,
    /// The row the arrow keys are on. Kept as a product rather than an index
    /// so that filtering, which changes every index, cannot silently move it.
    focus: Option<DisplayProduct>,
    /// Groups the analyst has folded shut. Empty by default: an analyst who
    /// has not asked for anything sees everything.
    collapsed: BTreeSet<ProductGroup>,
    /// Put the caret in the filter field on the next frame, so the first
    /// keystroke after opening filters instead of going nowhere.
    focus_filter: bool,
    scroll_to_focus: bool,
    /// The palette rows for the focused product's family, and the set of base
    /// names this build ships. Rebuilding either every frame the picker is
    /// open would be parsing colour tables at the frame rate; the one cache
    /// the toolbar and the settings window also use holds both, keyed on the
    /// family, the installed palette and the colour table folder's scan
    /// generation.
    ///
    /// The shipped-name set is kept beside the offers because the offers list
    /// cannot be asked: a user table an analyst installed is appended to it
    /// and looks exactly like a preset from the outside. It is what decides
    /// whether a row's edit affordance opens the table or duplicates it.
    ///
    /// `a_table_dropped_while_the_picker_is_open_appears_in_the_list` is the
    /// pin for the scan generation; `pressing_a_palette_rows_edit_affordance_
    /// opens_the_editor_without_installing_it` and
    /// `a_palette_this_build_does_not_ship_opens_for_editing_rather_than_
    /// copying` are the pins for the shipped-name set.
    palettes: PaletteOfferCache,
}

impl ProductPickerState {
    /// Call when the picker opens: clear the filter, put the focus on the
    /// product the pane is showing, and claim the keyboard.
    pub fn opened(&mut self, current: DisplayProduct) {
        self.filter.clear();
        self.focus = Some(current);
        self.focus_filter = true;
        self.scroll_to_focus = true;
        // Opening onto a folded group would put the focus ring on a row that
        // is not drawn, which reads as no focus at all.
        self.collapsed.remove(&current.descriptor().group);
    }

    /// The live filter text. Read by the tests, which assert on what the
    /// filter matched rather than on where rows landed on screen.
    #[allow(dead_code)]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The row the keyboard is on. Read by the tests; the widget itself
    /// carries the focus internally.
    #[allow(dead_code)]
    pub fn focused(&self) -> Option<DisplayProduct> {
        self.focus
    }

    fn palettes_for(
        &mut self,
        family: ColorTableFamily,
        installed: &ColorTable,
        user_tables: Option<&UserTableLibrary>,
    ) -> &[ColorTable] {
        self.palettes.offers(family, installed, user_tables)
    }

    /// Whether a palette is one this build ships, and therefore one the editor
    /// must duplicate rather than open. Answered from the same cache entry the
    /// rows came from, so the two can never disagree about one row.
    fn is_builtin(&self, base_name: &str) -> bool {
        self.palettes.is_builtin(base_name)
    }
}

/// Everything the picker needs from the application, and nothing more.
pub struct ProductPickerInput<'a> {
    pub state: &'a mut ProductPickerState,
    /// The product the active pane is drawing now.
    pub current: DisplayProduct,
    /// Exact producer name when the pane is drawing a native source field.
    pub current_source_field: Option<&'a str>,
    /// What the volume on screen can draw. Build it once per volume with
    /// [`ProductAvailabilityIndex::from_optional_capabilities`].
    pub availability: &'a ProductAvailabilityIndex,
    /// Exact producer-native fields preserved by this volume's decoder.
    /// Some may also have a separately selectable canonical interpretation.
    pub source_fields: &'a SourceFieldCatalog,
    /// The colour tables in force, so the palette section can mark the one
    /// already installed for the family.
    pub tables: &'a ColorTableSet,
    /// The analyst's own colour table folder, whose tables are offered after
    /// the built-ins. `None` offers the built-ins alone.
    pub user_tables: Option<&'a UserTableLibrary>,
    /// Whether experimental products are offered. Passed through to the
    /// registry's own visibility rule rather than judged here.
    pub show_experimental: bool,
}

/// A colour table the analyst picked, and the family it belongs to.
///
/// The family, not the product: a `ColorTableSet` holds one table per family,
/// so installing this affects every product that draws from it - all four
/// velocity products share one velocity table.
#[derive(Clone, Debug, PartialEq)]
pub struct PaletteSelection {
    pub family: ColorTableFamily,
    pub table: ColorTable,
}

/// A palette row's edit affordance was pressed.
///
/// `duplicate` is the shipped-preset rule made explicit at the boundary: the
/// picker knows which palettes this build ships, and the editor never has to
/// guess. A preset is copied under a new name and the original is left alone.
#[derive(Clone, Debug, PartialEq)]
pub struct PaletteEditRequest {
    pub family: ColorTableFamily,
    pub table: ColorTable,
    pub duplicate: bool,
}

/// What the analyst did this frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProductPickerOutcome {
    /// A product was chosen. Never a product the volume cannot draw.
    pub product: Option<DisplayProduct>,
    /// A producer-native field was chosen by its exact source key.
    pub source_field: Option<String>,
    /// A palette was chosen for the focused product's family.
    pub palette: Option<PaletteSelection>,
    /// A palette row asked to be opened in the colour table editor.
    pub edit_palette: Option<PaletteEditRequest>,
    /// The picker asked to close: Escape, or a product was chosen. Advisory -
    /// the caller owns the open flag and may keep it open.
    pub dismissed: bool,
}

/// The colour-table family whose table the render path draws this product
/// with, or `None` when it draws on a ramp synthesised over the product's own
/// range and there is nothing in the set to offer.
///
/// Read from the two registry facts `palettes::table_for` reads - the source
/// moment, and whether the product is a volume integration - because that
/// function is what actually picks the table. Deliberately *not* keyed on the
/// registry's `default_palette` handle, which the composition layer is free to
/// resolve differently: keying on it made this file a second opinion about
/// which palette belongs to which product, and it was already wrong, answering
/// "no alternatives" for the four moments drawn from `Generic`.
///
/// `a_palette_offer_changes_exactly_the_product_it_is_offered_for` is the pin:
/// it installs a marker into each family in turn and checks that the only one
/// that changes what the pane draws is the one offered here.
pub fn palette_family(product: DisplayProduct) -> Option<ColorTableFamily> {
    let computation = &product.descriptor().computation;
    match computation.derived_volume() {
        None => Some(color_family_for_moment(&computation.source_moment())),
        // A composite is reflectivity and is drawn with reflectivity's table.
        Some(DerivedVolumeId::CompositeReflectivity) => Some(ColorTableFamily::Reflectivity),
        // Liquid water, hail size and echo height get a ramp built over their
        // own declared range; the set holds no table for them, and offering a
        // dBZ table for a field in kilograms per square metre would be worse
        // than offering nothing.
        Some(_) => None,
    }
}

/// The declared domain in the unit the analyst reads, e.g. "-32.0 to 94.5 dBZ".
///
/// Engine units are what the grid holds; this is the boundary where they are
/// converted, so velocity reads in knots here and metres per second nowhere.
fn range_summary(descriptor: &ProductDescriptor) -> String {
    let domain = &descriptor.domain;
    let range = domain.declared_engine_range;
    // The unit is appended by the domain's own formatter rather than here, so
    // that whether a dimensionless product gets a trailing space is decided in
    // one place. RHO reads "0.200 to 1.050" and REF "-32.0 to 94.5 dBZ".
    format!(
        "{} to {}",
        domain.format_display_value(range.min),
        domain.format_display(range.max)
    )
}

/// Underscores and hyphens read as spaces, so "storm relative" finds both the
/// alias `storm_relative_velocity` and the name "Storm-Relative Velocity".
fn normalized(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '_' | '-' => ' ',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// Whether a product answers to what has been typed.
///
/// Matches the canonical id, the short name, the display name and every alias
/// the registry accepts, because an analyst types what they know: `RHO`, `cc`
/// and `rhohv` are the same product to three different people.
fn matches_filter(descriptor: &ProductDescriptor, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hit = |text: &str| normalized(text).contains(needle);
    hit(&descriptor.id.0)
        || hit(descriptor.short_name)
        || hit(descriptor.display_name)
        || descriptor.aliases.iter().any(|alias| hit(alias))
}

/// A source field is searchable by every producer-supplied string we show.
/// The match is forgiving; the stored and displayed strings remain exact.
fn source_field_matches_filter(field: &SourceField, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hit = |text: &str| normalized(text).contains(needle);
    hit(&field.producer_name)
        || field.metadata.iter().any(|metadata| {
            metadata.producer_description.as_deref().is_some_and(hit)
                || metadata.producer_units.as_deref().is_some_and(hit)
        })
}

fn visible_source_fields<'a>(
    filter: &str,
    catalog: &'a SourceFieldCatalog,
) -> Vec<&'a SourceField> {
    let needle = normalized(filter.trim());
    catalog
        .fields()
        .iter()
        .filter(|field| source_field_matches_filter(field, &needle))
        .collect()
}

/// The rows to draw, in registry group order.
///
/// Products the pane handles cannot name are skipped, because the picker
/// cannot return one. `product::DisplayProduct`'s own test pins the handles
/// against the registry's selectable list, so that skip is a tripwire rather
/// than a silent omission.
fn visible_entries<'a>(
    filter: &str,
    availability: &'a ProductAvailabilityIndex,
    show_experimental: bool,
) -> Vec<ProductEntry<'a>> {
    let registry = ProductRegistry::builtin();
    let needle = normalized(filter.trim());
    let mut entries = Vec::new();
    for group in ProductGroup::ALL {
        for descriptor in registry.group_products(group, show_experimental) {
            let Some(product) = DisplayProduct::try_from_product_id(&descriptor.id) else {
                continue;
            };
            if !matches_filter(descriptor, &needle) {
                continue;
            }
            entries.push(ProductEntry {
                product,
                descriptor,
                availability: availability.get(product),
            });
        }
    }
    entries
}

fn focus_index(focus: Option<DisplayProduct>, entries: &[ProductEntry<'_>]) -> Option<usize> {
    focus.and_then(|focus| entries.iter().position(|entry| entry.product == focus))
}

/// A filter hides the rows it does not match; a folded group hides the rows
/// inside it. Both leave the focus naming a product that is not on screen.
fn is_drawn(state: &ProductPickerState, entry: &ProductEntry<'_>) -> bool {
    // While filtering, groups are drawn open: a filter whose matches stayed
    // folded away looks like a filter that found nothing.
    !state.filter.trim().is_empty() || !state.collapsed.contains(&entry.descriptor.group)
}

/// Where the focus ring is, but only while the row under it is drawn.
fn focused_row(state: &ProductPickerState, entries: &[ProductEntry<'_>]) -> Option<usize> {
    let index = focus_index(state.focus, entries)?;
    is_drawn(state, &entries[index]).then_some(index)
}

/// Somewhere for the focus to go when the row it was on stopped being drawn.
///
/// `None` when every group is folded shut: no focus at all is the honest
/// answer there, and an arrow key unfolds a group and lands in it.
fn first_drawn(state: &ProductPickerState, entries: &[ProductEntry<'_>]) -> Option<DisplayProduct> {
    entries
        .iter()
        .find(|entry| is_drawn(state, entry))
        .map(|entry| entry.product)
}

/// What the keyboard asked for this frame.
struct KeyIntent {
    /// Rows to move, positive downwards.
    step: isize,
    choose: bool,
    dismiss: bool,
}

/// Read and *consume* the picker's keys.
///
/// Consuming matters: the filter field holds the caret, and a focused
/// `TextEdit` would otherwise swallow Enter and Escape and move its own caret
/// on the arrows. Taking the events before the field is drawn leaves the
/// analyst one set of keys that means one thing.
fn read_keys(ui: &egui::Ui) -> KeyIntent {
    ui.input_mut(|input| {
        let modifiers = egui::Modifiers::NONE;
        // Counted, not tested: a held arrow key delivers several presses in a
        // frame and all of them should move.
        let down = input.count_and_consume_key(modifiers, egui::Key::ArrowDown) as isize;
        let up = input.count_and_consume_key(modifiers, egui::Key::ArrowUp) as isize;
        KeyIntent {
            step: down - up,
            choose: input.consume_key(modifiers, egui::Key::Enter),
            dismiss: input.consume_key(modifiers, egui::Key::Escape),
        }
    })
}

/// Draw the picker into `ui` and report what the analyst chose.
pub fn draw_product_picker(
    ui: &mut egui::Ui,
    input: ProductPickerInput<'_>,
) -> ProductPickerOutcome {
    let ProductPickerInput {
        state,
        current,
        current_source_field,
        availability,
        source_fields,
        tables,
        user_tables,
        show_experimental,
    } = input;

    // Before the filter field is drawn, so a focused `TextEdit` cannot swallow
    // Enter, Escape or the arrows.
    let keys = read_keys(ui);
    let mut outcome = ProductPickerOutcome {
        dismissed: keys.dismiss,
        ..ProductPickerOutcome::default()
    };

    egui::Frame::new()
        .fill(BACKGROUND)
        .inner_margin(egui::Margin::same(8))
        .corner_radius(6.0)
        .show(ui, |ui| {
            ui.set_width(PICKER_WIDTH);
            dark_visuals(ui);
            filter_field(ui, state);

            // After the field, so a keystroke and the Enter that follows it in
            // the same frame agree about what is on the list. Filtering first
            // and reading the field afterwards leaves Enter choosing whatever
            // the previous frame's filter had focused.
            let entries = visible_entries(&state.filter, availability, show_experimental);
            let source_fields = visible_source_fields(&state.filter, source_fields);
            // A filter that hides the focused row, or a group folded shut over
            // it, must not leave Enter pointing at something the analyst
            // cannot see.
            if focused_row(state, &entries).is_none() {
                state.focus = first_drawn(state, &entries);
                state.scroll_to_focus = true;
            }
            if keys.step != 0 && !entries.is_empty() {
                let last = entries.len() as isize - 1;
                let next = match focus_index(state.focus, &entries) {
                    // Nothing is focused, because every group is folded shut.
                    // The first step lands on the first row rather than
                    // stepping off a row that was never there.
                    None => 0,
                    // Clamped, not wrapped: an analyst holding the arrow key
                    // must not sail past the product they wanted and back to
                    // the top.
                    Some(at) => (at as isize + keys.step).clamp(0, last) as usize,
                };
                state.focus = Some(entries[next].product);
                // Arrowing into a folded group unfolds it, rather than moving
                // the focus somewhere the analyst cannot see it.
                state.collapsed.remove(&entries[next].descriptor.group);
                state.scroll_to_focus = true;
            }
            if keys.choose
                && let Some(index) = focused_row(state, &entries)
                && entries[index].is_available()
            {
                outcome.product = Some(entries[index].product);
                outcome.dismissed = true;
            }

            ui.add_space(6.0);
            list(
                ui,
                state,
                &entries,
                &source_fields,
                current,
                current_source_field,
                &mut outcome,
            );
            ui.add_space(6.0);
            separator(ui);
            ui.add_space(6.0);
            // The palette section follows the focused row rather than the
            // pane's current product: a colour table belongs to a family, and
            // an analyst arrowing onto velocity is asking about velocity's
            // palettes.
            let palette_product = state.focus.unwrap_or(current);
            if let Some(palette) = palette_section(
                ui,
                state,
                palette_product,
                tables,
                user_tables,
                &mut outcome,
            ) {
                outcome.palette = Some(palette);
            }
        });

    outcome
}

fn dark_visuals(ui: &mut egui::Ui) {
    let visuals = ui.visuals_mut();
    visuals.panel_fill = BACKGROUND;
    visuals.extreme_bg_color = FIELD_BACKGROUND;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    visuals.widgets.inactive.bg_fill = FIELD_BACKGROUND;
    visuals.widgets.hovered.bg_fill = ROW_HOVER;
}

fn separator(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, 1.0));
    ui.painter().rect_filled(rect, 0.0, SEPARATOR);
}

fn filter_field(ui: &mut egui::Ui, state: &mut ProductPickerState) {
    // Focus is claimed before the field is drawn, not after: `request_focus`
    // on the returned response only takes effect next frame, and the first
    // keystroke after the picker opens is the one an analyst is most likely to
    // have already typed.
    let id = filter_id();
    if state.focus_filter || ui.memory(|memory| memory.focused()).is_none() {
        ui.memory_mut(|memory| memory.request_focus(id));
        state.focus_filter = false;
    }
    ui.add(
        egui::TextEdit::singleline(&mut state.filter)
            .id(id)
            .desired_width(f32::INFINITY)
            .hint_text("filter: id, name, alias or source field")
            .margin(egui::Margin::symmetric(8, 5)),
    );
}

fn list(
    ui: &mut egui::Ui,
    state: &mut ProductPickerState,
    entries: &[ProductEntry<'_>],
    source_fields: &[&SourceField],
    current: DisplayProduct,
    current_source_field: Option<&str>,
    outcome: &mut ProductPickerOutcome,
) {
    if entries.is_empty() && source_fields.is_empty() {
        // Cleared here too: a request to scroll that outlives the row it was
        // made for would jump the list the next time anything matches.
        state.scroll_to_focus = false;
        let width = ui.available_width();
        let (_, rect) = ui.allocate_space(egui::vec2(width, ROW_HEIGHT));
        ui.painter().text(
            rect.left_center() + egui::vec2(10.0, 0.0),
            egui::Align2::LEFT_CENTER,
            format!("no product matches \"{}\"", state.filter.trim()),
            egui::FontId::proportional(12.0),
            TEXT_DIM,
        );
        return;
    }

    let filtering = !state.filter.trim().is_empty();
    let focus = state.focus;
    let mut toggled_group = None;
    let mut chosen = None;
    let mut chosen_source = None;
    let mut scroll_to = None;

    egui::ScrollArea::vertical()
        .max_height(LIST_MAX_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for group in ProductGroup::ALL {
                let members: Vec<&ProductEntry<'_>> = entries
                    .iter()
                    .filter(|entry| entry.descriptor.group == group)
                    .collect();
                if members.is_empty() {
                    continue;
                }
                // A filter that leaves its matches folded inside a collapsed
                // group looks like a filter that found nothing.
                let collapsed = !filtering && state.collapsed.contains(&group);
                if group_header(ui, group, collapsed, &members).clicked() {
                    toggled_group = Some(group);
                }
                if collapsed {
                    continue;
                }
                for entry in members {
                    // A namespaced source-field id falls back to REF when it
                    // passes through the fixed DisplayProduct enum. That is a
                    // routing fallback, not a second selection: highlighting
                    // REF beside the selected native row claims two active
                    // products at once.
                    let selected = current_source_field.is_none() && entry.product == current;
                    let focused = Some(entry.product) == focus;
                    let response = product_row(ui, entry, selected, focused);
                    if focused && state.scroll_to_focus {
                        scroll_to = Some(response.rect);
                    }
                    if response.clicked() && entry.is_available() {
                        chosen = Some(entry.product);
                    }
                }
            }
            if !source_fields.is_empty() {
                source_field_header(ui, source_fields.len());
                for field in source_fields {
                    let selected = current_source_field == Some(field.producer_name.as_str());
                    if source_field_row(ui, field, selected).clicked() {
                        chosen_source = Some(field.producer_name.clone());
                    }
                }
            }
            if let Some(rect) = scroll_to {
                ui.scroll_to_rect(rect, Some(egui::Align::Center));
            }
        });

    state.scroll_to_focus = false;
    // `remove` reports whether the group was folded, so one call both unfolds
    // and answers which way the click went.
    if let Some(group) = toggled_group
        && !state.collapsed.remove(&group)
    {
        state.collapsed.insert(group);
    }
    if let Some(product) = chosen {
        state.focus = Some(product);
        outcome.product = Some(product);
        outcome.dismissed = true;
    }
    if let Some(producer_name) = chosen_source {
        outcome.source_field = Some(producer_name);
        outcome.dismissed = true;
    }
}

/// The producer-native catalog is deliberately separate from the fixed DOW
/// dual-frequency group. These fields came from this file; they are not a
/// claim that every research radar implements one universal product suite.
fn source_field_header(ui: &mut egui::Ui, count: usize) -> egui::Response {
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, GROUP_HEADER_HEIGHT));
    let response = ui.interact(
        rect,
        egui::Id::new("radar-product-picker-source-fields"),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    painter.text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "SOURCE FIELDS FROM THIS FILE",
        egui::FontId::proportional(11.0),
        TEXT_DIM,
    );
    painter.text(
        rect.right_center() - egui::vec2(10.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{count} preserved · selectable"),
        egui::FontId::proportional(10.5),
        WARNING,
    );
    response.on_hover_text(
        "Exact fields preserved by this file's decoder. Names, descriptions and unit tokens are \
         producer metadata, not validated units, aliases or inferences. A selected field uses a generic source \
         palette stretched only across its observed finite values. The modeled DOW DBMH*/DBZH* \
         suite remains a separate group above.",
    )
}

fn source_field_row(ui: &mut egui::Ui, field: &SourceField, selected: bool) -> egui::Response {
    let detail_rows = field.metadata.len().max(1) as f32 + 1.0;
    let height = SOURCE_FIELD_NAME_HEIGHT + detail_rows * SOURCE_FIELD_METADATA_HEIGHT;
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, height));
    let response = ui.interact(
        rect,
        source_field_row_id(&field.producer_name),
        egui::Sense::CLICK,
    );
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        4.0,
        if selected {
            ROW_SELECTED
        } else if response.hovered() {
            ROW_HOVER
        } else {
            BACKGROUND
        },
    );
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height())),
            2.0,
            ACCENT,
        );
    }
    painter.text(
        rect.left_top() + egui::vec2(12.0, 6.0),
        egui::Align2::LEFT_TOP,
        &field.producer_name,
        egui::FontId::monospace(11.0),
        ACCENT,
    );
    painter.text(
        rect.right_top() + egui::vec2(-10.0, 6.0),
        egui::Align2::RIGHT_TOP,
        "SOURCE FIELD · GENERIC DISPLAY",
        egui::FontId::proportional(10.0),
        WARNING,
    );
    for (index, metadata) in field.metadata.iter().enumerate() {
        painter.text(
            rect.left_top()
                + egui::vec2(
                    24.0,
                    SOURCE_FIELD_NAME_HEIGHT + index as f32 * SOURCE_FIELD_METADATA_HEIGHT,
                ),
            egui::Align2::LEFT_TOP,
            source_field_metadata_line(metadata),
            egui::FontId::proportional(10.5),
            TEXT_DIM,
        );
    }
    painter.text(
        rect.left_top()
            + egui::vec2(
                24.0,
                SOURCE_FIELD_NAME_HEIGHT
                    + field.metadata.len().max(1) as f32 * SOURCE_FIELD_METADATA_HEIGHT,
            ),
        egui::Align2::LEFT_TOP,
        source_field_value_line(field),
        egui::FontId::proportional(10.5),
        WARNING.gamma_multiply(0.9),
    );
    response.on_hover_text(source_field_hover(field))
}

fn source_field_metadata_line(metadata: &SourceFieldMetadata) -> String {
    format!(
        "description: {} · producer unit token: {} · {}",
        metadata
            .producer_description
            .as_deref()
            .unwrap_or("not provided"),
        metadata.producer_units.as_deref().unwrap_or("not provided"),
        cut_summary(&metadata.cut_indices)
    )
}

fn cut_summary(cut_indices: &[usize]) -> String {
    let cuts = cut_indices
        .iter()
        .map(|index| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if cut_indices.len() == 1 {
        format!("cut {cuts}")
    } else {
        format!("cuts {cuts}")
    }
}

fn source_field_hover(field: &SourceField) -> String {
    let mut lines = vec![
        field.producer_name.clone(),
        "producer-native field · exact container identity".to_owned(),
    ];
    lines.extend(field.metadata.iter().map(source_field_metadata_line));
    lines.extend(field.occurrences.iter().map(|occurrence| {
        format!(
            "cut {} · {} finite gates · observed {}",
            occurrence.cut_index + 1,
            occurrence.finite_count,
            observed_range(occurrence.finite_min, occurrence.finite_max)
        )
    }));
    lines.push("generic source display · no meaning or units inferred".to_owned());
    lines.join("\n")
}

fn source_field_value_line(field: &SourceField) -> String {
    let finite_count: usize = field
        .occurrences
        .iter()
        .map(|occurrence| occurrence.finite_count)
        .sum();
    let minimum = field
        .occurrences
        .iter()
        .filter_map(|occurrence| occurrence.finite_min)
        .min_by(f32::total_cmp);
    let maximum = field
        .occurrences
        .iter()
        .filter_map(|occurrence| occurrence.finite_max)
        .max_by(f32::total_cmp);
    format!(
        "observed: {} · {finite_count} finite gates · generic palette",
        observed_range(minimum, maximum)
    )
}

fn observed_range(minimum: Option<f32>, maximum: Option<f32>) -> String {
    match (minimum, maximum) {
        (Some(minimum), Some(maximum)) => format!("{minimum:.3} to {maximum:.3}"),
        _ => "no finite values".to_owned(),
    }
}

fn group_header(
    ui: &mut egui::Ui,
    group: ProductGroup,
    collapsed: bool,
    members: &[&ProductEntry<'_>],
) -> egui::Response {
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, GROUP_HEADER_HEIGHT));
    let response = ui.interact(rect, group_id(group), egui::Sense::CLICK);
    let painter = ui.painter();

    // Drawn rather than typed: a triangle glyph is not in every font the host
    // might substitute, and a header that renders as a box is worse than one
    // with no marker at all.
    let marker = rect.left_center() + egui::vec2(10.0, 0.0);
    let points = if collapsed {
        vec![
            marker + egui::vec2(-2.0, -4.5),
            marker + egui::vec2(-2.0, 4.5),
            marker + egui::vec2(4.0, 0.0),
        ]
    } else {
        vec![
            marker + egui::vec2(-4.5, -2.0),
            marker + egui::vec2(4.5, -2.0),
            marker + egui::vec2(0.0, 3.5),
        ]
    };
    painter.add(egui::Shape::convex_polygon(
        points,
        TEXT_DIM,
        egui::Stroke::NONE,
    ));
    painter.text(
        rect.left_center() + egui::vec2(24.0, 0.0),
        egui::Align2::LEFT_CENTER,
        group.label().to_uppercase(),
        egui::FontId::proportional(11.0),
        TEXT_DIM,
    );

    let unavailable = members.iter().filter(|entry| !entry.is_available()).count();
    if unavailable > 0 {
        painter.text(
            rect.right_center() - egui::vec2(10.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{unavailable} of {} unavailable", members.len()),
            egui::FontId::proportional(10.5),
            WARNING.gamma_multiply(0.8),
        );
    }
    if group == ProductGroup::DowDualFrequency {
        response.on_hover_text(
            "Raw DOW6/7 dual-frequency receiver-chain products. These rows require the exact \
             DBMH1/DBMH2/DBMHM, DBMV1/DBMV2/DBMVM, DBZH1/DBZH2/DBZHM, or \
             DBZV1/DBZV2/DBZVM fields; ordinary DOW Message 31 REF/VEL/ZDR/RHO/PHI moments \
             remain in their standard product groups.",
        )
    } else {
        response
    }
}

fn product_row(
    ui: &mut egui::Ui,
    entry: &ProductEntry<'_>,
    selected: bool,
    focused: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, ROW_HEIGHT));
    // `Sense::CLICK` and not `Sense::click()`: the latter also makes the row
    // focusable, which would move the caret out of the filter field on the
    // first click and silently break type-to-filter.
    let response = ui.interact(rect, row_id(entry.product), egui::Sense::CLICK);
    let available = entry.is_available();
    let hovered = response.hovered() && available;

    let painter = ui.painter();
    let fill = if selected {
        ROW_SELECTED
    } else if hovered {
        ROW_HOVER
    } else {
        BACKGROUND
    };
    painter.rect_filled(rect, 4.0, fill);
    if focused {
        painter.rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height())),
            2.0,
            ACCENT,
        );
    }

    let name_color = if available { TEXT } else { TEXT_FAINT };
    let id_color = if available { ACCENT } else { TEXT_FAINT };
    painter.text(
        rect.left_top() + egui::vec2(12.0, 9.0),
        egui::Align2::LEFT_TOP,
        entry.descriptor.short_name,
        egui::FontId::monospace(11.0),
        id_color,
    );
    painter.text(
        rect.left_top() + egui::vec2(66.0, 5.0),
        egui::Align2::LEFT_TOP,
        entry.descriptor.display_name,
        egui::FontId::proportional(13.0),
        name_color,
    );
    // The unit and range are the point of the row: a picker that only names
    // products leaves an analyst to remember that VIL is kilograms per square
    // metre and echo tops are kilofeet.
    painter.text(
        rect.left_top() + egui::vec2(66.0, 20.0),
        egui::Align2::LEFT_TOP,
        range_summary(entry.descriptor),
        egui::FontId::proportional(10.5),
        if available { TEXT_DIM } else { TEXT_FAINT },
    );

    let right = rect.right_center() - egui::vec2(10.0, 0.0);
    if let Some(reason) = entry.unavailable_label() {
        painter.text(
            right,
            egui::Align2::RIGHT_CENTER,
            reason,
            egui::FontId::proportional(11.0),
            WARNING,
        );
    } else {
        let badges = row_badges(entry);
        if !badges.is_empty() {
            painter.text(
                right,
                egui::Align2::RIGHT_CENTER,
                badges.join(" "),
                egui::FontId::proportional(10.0),
                WARNING,
            );
        }
    }

    response.on_hover_text(hover_text(entry))
}

/// The all-caps badges on the right of an available row.
///
/// The volume's own qualifiers, plus the registry's visibility: a product
/// declared `Experimental` is "offered only when experimental products are
/// switched on, and badged wherever it appears", and the picker is one of the
/// places it appears. Reading the badge off the descriptor rather than
/// expecting the caller to add a qualifier means a product cannot arrive here
/// experimental and unmarked.
fn row_badges(entry: &ProductEntry<'_>) -> Vec<&'static str> {
    let mut badges: Vec<&'static str> = entry
        .availability
        .qualifiers()
        .iter()
        .map(|qualifier| qualifier.badge())
        .collect();
    let experimental = AvailabilityQualifier::Experimental.badge();
    if entry.descriptor.visibility == ProductVisibility::Experimental
        && !badges.contains(&experimental)
    {
        badges.insert(0, experimental);
    }
    badges
}

/// The long form: what the product is, what it needs, and where it comes from.
fn hover_text(entry: &ProductEntry<'_>) -> String {
    let descriptor = entry.descriptor;
    let mut lines = vec![
        format!("{} ({})", descriptor.display_name, descriptor.id.0),
        format!(
            "{} · {}",
            descriptor.group.label(),
            range_summary(descriptor)
        ),
    ];
    if let Some(reason) = entry.unavailable_label() {
        lines.push(format!("unavailable: {reason}"));
    }
    // Named here rather than only in a paper nobody opens: a MESH is a claim
    // about Witt et al., and an analyst is entitled to see whose.
    for citation in descriptor.algorithm.citations {
        lines.push(citation.to_line());
    }
    lines.join("\n")
}

fn palette_section(
    ui: &mut egui::Ui,
    state: &mut ProductPickerState,
    product: DisplayProduct,
    tables: &ColorTableSet,
    user_tables: Option<&UserTableLibrary>,
    outcome: &mut ProductPickerOutcome,
) -> Option<PaletteSelection> {
    let descriptor = product.descriptor();
    let Some(family) = palette_family(product) else {
        label_row(
            ui,
            format!(
                "PALETTE · {} is drawn on a ramp built over its own range",
                descriptor.short_name
            ),
        );
        return None;
    };

    let installed = tables.for_family(family);
    let in_use = installed.name().to_owned();
    let mut chosen = None;
    // Cloned out of the cache: the rows borrow `state` mutably to draw, and a
    // colour table is a few dozen stops.
    let palettes: Vec<ColorTable> = state.palettes_for(family, installed, user_tables).to_vec();
    // Said on the heading rather than left to be inferred from the list: the
    // last row is never another palette, it is this one drawn the other way,
    // and that row is the whole of the smooth/stepped control.
    let heading = if palettes.len() < 3 {
        format!(
            "PALETTE · {} · no alternatives in this build",
            family.label().to_uppercase()
        )
    } else {
        format!(
            "PALETTE · {} · last row redraws the selected palette {}",
            family.label().to_uppercase(),
            installed.rendering().flipped().label().to_lowercase()
        )
    };
    label_row(ui, heading);
    for table in palettes {
        let duplicate = state.is_builtin(table.base_name());
        let row = palette_row(ui, &table, table.name() == in_use, descriptor, duplicate);
        if row.edit.clicked() {
            outcome.edit_palette = Some(PaletteEditRequest {
                family,
                table: table.clone(),
                duplicate,
            });
        } else if row.row.clicked() {
            chosen = Some(PaletteSelection {
                family,
                table: table.clone(),
            });
        }
    }
    chosen
}

fn label_row(ui: &mut egui::Ui, text: String) {
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, 18.0));
    ui.painter().text(
        rect.left_center() + egui::vec2(2.0, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(10.5),
        TEXT_DIM,
    );
}

/// The two things a palette row can be clicked on.
struct PaletteRow {
    row: egui::Response,
    edit: egui::Response,
}

fn palette_row(
    ui: &mut egui::Ui,
    table: &ColorTable,
    in_use: bool,
    descriptor: &ProductDescriptor,
    duplicate: bool,
) -> PaletteRow {
    let width = ui.available_width();
    let (_, rect) = ui.allocate_space(egui::vec2(width, PALETTE_ROW_HEIGHT));
    let response = ui.interact(rect, palette_row_id(table.name()), egui::Sense::CLICK);
    // Registered AFTER the row, so it sits above it: a click on the edit
    // affordance must open the editor, not also install the palette.
    let edit_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - SWATCH_WIDTH - 18.0 - EDIT_BUTTON_WIDTH,
            rect.top(),
        ),
        egui::vec2(EDIT_BUTTON_WIDTH, rect.height()),
    );
    let edit = ui.interact(edit_rect, palette_edit_id(table.name()), egui::Sense::CLICK);
    let painter = ui.painter();
    let fill = if in_use {
        ROW_SELECTED
    } else if response.hovered() {
        ROW_HOVER
    } else {
        BACKGROUND
    };
    painter.rect_filled(rect, 4.0, fill);
    painter.text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        table.name(),
        egui::FontId::proportional(12.0),
        if in_use { TEXT } else { TEXT_DIM },
    );
    let swatch = egui::Rect::from_min_size(
        egui::pos2(rect.right() - SWATCH_WIDTH - 10.0, rect.center().y - 6.0),
        egui::vec2(SWATCH_WIDTH, 12.0),
    );
    draw_swatch(
        painter,
        swatch,
        table,
        descriptor.domain.declared_engine_range,
    );
    if in_use {
        painter.rect_stroke(
            swatch,
            2.0,
            egui::Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Outside,
        );
    }
    // "Copy" and not "Edit" on a shipped preset, because that is what pressing
    // it does: the catalogue is never written over, so a preset opens as a
    // duplicate under a new name. The full sentence is on the hover.
    let edit_label = if duplicate { "Copy" } else { "Edit" };
    painter.rect_filled(
        edit_rect.shrink2(egui::vec2(0.0, 3.0)),
        3.0,
        if edit.hovered() {
            ROW_SELECTED
        } else {
            FIELD_BACKGROUND
        },
    );
    painter.text(
        edit_rect.center(),
        egui::Align2::CENTER_CENTER,
        edit_label,
        egui::FontId::proportional(11.0),
        if edit.hovered() { TEXT } else { TEXT_DIM },
    );
    let edit = edit.on_hover_text(if duplicate {
        "Duplicate and edit. Shipped presets are never overwritten, so this opens a copy."
    } else {
        "Edit this colour table."
    });
    let row = response.on_hover_text(format!(
        "{} · previewed over {}",
        table.name(),
        range_summary(descriptor)
    ));
    PaletteRow { row, edit }
}

/// Paint a palette across the span of the product it would draw.
///
/// Over the product's own range rather than the table's: a preview that
/// stretched every table to the same width would show a reflectivity table
/// that stops at 75 dBZ as if it ran to the top of the domain.
fn draw_swatch(
    painter: &egui::Painter,
    rect: egui::Rect,
    table: &ColorTable,
    declared: ValueRange,
) {
    let span = table
        .inked_value_span()
        .and_then(|(low, high)| ValueRange::new(low, high).intersect(declared))
        .unwrap_or(declared);
    let step = rect.width() / SWATCH_STRIPS as f32;
    for index in 0..SWATCH_STRIPS {
        let fraction = (index as f32 + 0.5) / SWATCH_STRIPS as f32;
        let value = span.min + span.span() * fraction;
        let [red, green, blue, alpha] = table.color_for_value(value);
        let strip = egui::Rect::from_min_size(
            egui::pos2(rect.left() + index as f32 * step, rect.top()),
            // Half a pixel of overlap, so the strips do not show seams when
            // the swatch width is not a multiple of the strip count.
            egui::vec2(step + 0.5, rect.height()),
        );
        painter.rect_filled(
            strip,
            0.0,
            egui::Color32::from_rgba_unmultiplied(red, green, blue, alpha),
        );
    }
}

/// Stable ids, so a test can find a row by product rather than by pixel.
///
/// One picker is open at a time - it is the toolbar's product menu - so a
/// fixed namespace is enough and is what makes the rows addressable.
fn row_id(product: DisplayProduct) -> egui::Id {
    egui::Id::new(("radar-product-picker-row", product.id()))
}

fn source_field_row_id(producer_name: &str) -> egui::Id {
    egui::Id::new(("radar-product-picker-source-field", producer_name))
}

fn group_id(group: ProductGroup) -> egui::Id {
    egui::Id::new(("radar-product-picker-group", group.label()))
}

fn palette_row_id(name: &str) -> egui::Id {
    egui::Id::new(("radar-product-picker-palette", name))
}

fn palette_edit_id(name: &str) -> egui::Id {
    egui::Id::new(("radar-product-picker-palette-edit", name))
}

fn filter_id() -> egui::Id {
    egui::Id::new("radar-product-picker-filter")
}

#[cfg(test)]
mod tests {
    use color_tables::palette_offers_for_family;

    use super::*;
    // Test-only: production code offers palettes through
    // `palette_offers_for_family`; the tests compare that list against the
    // bare family list to pin the "+1 switch row" relationship.
    use crate::product_availability::availability_in;
    use color_tables::builtin_tables_for_family;
    use product_engine::{
        AlgorithmStatus, AvailabilityQualifier, CutIdentity, CutLeg, NominalElevationGroup,
        ProductRegistry,
    };
    use product_engine::{
        CutCapabilities, ProductAvailability, UnavailableReason, VolumeCapabilities,
    };
    use radar_core::{GateRange, MomentGrid, MomentType, RadarSite, RadarVolume};
    use std::collections::BTreeMap;

    fn ids(entries: &[ProductEntry<'_>]) -> Vec<&'static str> {
        entries
            .iter()
            .map(|entry| entry.descriptor.id.0.as_str())
            .collect()
    }

    fn all_entries(availability: &ProductAvailabilityIndex) -> Vec<ProductEntry<'_>> {
        visible_entries("", availability, false)
    }

    fn filtered<'a>(
        filter: &str,
        availability: &'a ProductAvailabilityIndex,
    ) -> Vec<ProductEntry<'a>> {
        visible_entries(filter, availability, false)
    }

    fn one_source_field() -> SourceFieldCatalog {
        let mut volume = RadarVolume::new(RadarSite::new("DOW7"), chrono::Utc::now());
        let cut = volume.push_cut(0.5, Some(1));
        let moment = MomentType::Unknown("NVM".to_owned());
        let mut grid = MomentGrid::new_u16(
            moment.clone(),
            GateRange {
                first_gate_m: 100,
                gate_spacing_m: 75,
                gate_count: 800,
            },
            100.0,
            32768.0,
            Some(0),
            None,
        );
        grid.producer_description = Some("Normalized velocity metric".to_owned());
        grid.producer_units = Some("arb".to_owned());
        grid.producer_name = Some("NVM".to_owned());
        cut.moments.insert(moment, grid);
        SourceFieldCatalog::from_volume(&volume)
    }

    // --- the catalog ------------------------------------------------------

    #[test]
    fn producer_native_fields_are_visible_with_exact_metadata() {
        let mut picker = Harness::open(DisplayProduct::Reflectivity).inspecting(one_source_field());
        let painted = picker.painted();
        assert!(
            painted
                .iter()
                .any(|text| text == "SOURCE FIELDS FROM THIS FILE"),
            "the native-field catalog has no visible home: {painted:?}"
        );
        assert!(painted.iter().any(|text| text == "NVM"));
        assert!(painted.iter().any(|text| {
            text == "description: Normalized velocity metric · producer unit token: arb · cut 1"
        }));
        assert!(picker.drawn(source_field_row_id("NVM")));
    }

    #[test]
    fn source_field_metadata_is_searchable_but_not_mistaken_for_a_product_alias() {
        let mut picker = Harness::open(DisplayProduct::Reflectivity)
            .inspecting(one_source_field())
            .filtered("normalized velocity metric");
        let painted = picker.painted();
        assert!(painted.iter().any(|text| text == "NVM"));

        let outcome = picker.click(source_field_row_id("NVM"));
        assert_eq!(outcome.product, None);
        assert_eq!(outcome.source_field.as_deref(), Some("NVM"));
        assert!(outcome.dismissed);
        assert_eq!(picker.state.focused(), None);
    }

    #[test]
    fn every_registry_product_appears_exactly_once_across_the_groups() {
        // The defect this picker exists to avoid is a product that is in the
        // registry and nowhere in the UI. Grouping must partition, not filter,
        // and `visible_entries` skips a descriptor no `DisplayProduct` can
        // name - silently, because the row simply is not drawn. Comparing
        // against the registry rather than against `DisplayProduct::ALL` is
        // what makes this a tripwire rather than a restatement.
        let availability = ProductAvailabilityIndex::unrestricted();
        let registry = ProductRegistry::builtin();
        for show_experimental in [false, true] {
            let listed: Vec<&str> = visible_entries("", &availability, show_experimental)
                .iter()
                .map(|entry| entry.descriptor.id.0.as_str())
                .collect();
            let selectable: Vec<&str> = registry
                .selectable_products(show_experimental)
                .map(|descriptor| descriptor.id.0.as_str())
                .collect();
            assert_eq!(
                listed, selectable,
                "show_experimental={show_experimental}: the picker and the registry disagree"
            );
            let unique: BTreeSet<&str> = listed.iter().copied().collect();
            assert_eq!(unique.len(), listed.len(), "a product is listed twice");
            assert_eq!(listed.len(), DisplayProduct::ALL.len());
        }
    }

    #[test]
    fn the_rows_follow_the_registry_group_order() {
        // Hand-written from the registry's declaration order: base moment,
        // then velocity, dual-pol, research-radar, volume and hail products.
        // If the registry reorders, this is the test that says so.
        let availability = ProductAvailabilityIndex::unrestricted();
        assert_eq!(
            ids(&all_entries(&availability)),
            [
                "REF", "PWR_REL", "VEL", "DVEL", "SRV", "DSRV", "SW", "ZDR", "RHO", "PHI", "KDP",
                "DBMH1", "DBMH2", "DBMHM", "DBMV1", "DBMV2", "DBMVM", "DBZH1", "DBZH2", "DBZHM",
                "DBZV1", "DBZV2", "DBZVM", "CREF", "ET18", "VIL", "VILD", "MESH", "POH", "POSH"
            ]
        );
    }

    #[test]
    fn every_row_paints_the_name_and_the_range_the_registry_declares() {
        // Read back off the frame rather than off the functions that build the
        // strings: this is what fails if a label, a unit or a range is ever
        // typed into this file instead of read from the descriptor. A legend
        // and a colour table in this workspace once disagreed about the same
        // product for exactly that reason.
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        let painted = picker.painted();
        let count = |needle: &str| painted.iter().filter(|text| *text == needle).count();

        let mut ranges: BTreeMap<String, usize> = BTreeMap::new();
        for product in DisplayProduct::ALL {
            let descriptor = product.descriptor();
            let id = &descriptor.id.0;
            assert_eq!(
                count(descriptor.short_name),
                1,
                "{id} is not on screen once"
            );
            assert_eq!(
                count(descriptor.display_name),
                1,
                "{id} is drawn under a name the registry does not declare"
            );
            *ranges.entry(range_summary(descriptor)).or_default() += 1;
        }
        // VEL and SRV share a domain, as do DVEL and DSRV, so the range lines
        // are counted rather than required to be unique.
        for (range, times) in ranges {
            assert_eq!(count(&range), times, "the range line {range:?} is missing");
        }
        // Hand-computed from the registry: 64 m/s is 124.4 kt, and the
        // velocity domain asks for whole knots.
        assert_eq!(count("-124 to 124 kt"), 2, "VEL and SRV read in knots");
        for group in ProductGroup::ALL {
            let header = group.label().to_uppercase();
            let expected = usize::from(
                ProductRegistry::builtin()
                    .group_products(group, false)
                    .next()
                    .is_some(),
            );
            assert_eq!(count(&header), expected, "group header {header:?}");
        }
    }

    #[test]
    fn a_range_is_summarised_in_the_unit_an_analyst_reads() {
        let registry = ProductRegistry::builtin();
        let summary = |id: &str| range_summary(registry.get(id).expect("product exists"));
        assert_eq!(summary("REF"), "-32.0 to 94.5 dBZ");
        assert_eq!(summary("PWR_REL"), "-5.0 to 95.0 dB re stored I/Q unit²");
        // Velocity is stored in m/s and read in knots: 64 m/s is 124.4 kt,
        // and the domain asks for whole knots.
        assert_eq!(summary("VEL"), "-124 to 124 kt");
        // Correlation coefficient is dimensionless, so no unit is appended.
        // The bounds are the field's own decoded endpoints, `(raw + 60.5)/300`
        // over raw 2..255, rather than round numbers - see the registry entry
        // for RHO. Rounding them here would make the picker disagree with the
        // legend and the probe about where the field stops.
        assert_eq!(summary("RHO"), "0.208 to 1.052");
        // Echo tops are stored in metres and read in kilofeet:
        // 21 000 m * 0.00328084 = 68.9 kft.
        assert_eq!(summary("ET18"), "0.0 to 68.9 kft");
    }

    // --- the filter -------------------------------------------------------

    #[test]
    fn the_filter_matches_an_alias_that_never_appears_on_screen() {
        // `rhohv` and `cc` are aliases of RHO; neither is its id or its label,
        // so a filter that only searched what is drawn would find nothing.
        let availability = ProductAvailabilityIndex::unrestricted();
        assert_eq!(ids(&filtered("rhohv", &availability)), ["RHO"]);
        assert_eq!(ids(&filtered("cc", &availability)), ["RHO"]);
        assert_eq!(ids(&filtered("unfolded", &availability)), ["DVEL"]);
    }

    #[test]
    fn the_filter_matches_a_display_name_and_reads_punctuation_as_a_space() {
        let availability = ProductAvailabilityIndex::unrestricted();
        // "Max Estimated Hail Size", "Probability of Hail", "Probability of
        // Severe Hail" - counted by hand off the registry.
        assert_eq!(
            ids(&filtered("hail", &availability)),
            ["MESH", "POH", "POSH"]
        );
        // "Storm-Relative Velocity" and the alias
        // `dealiased_storm_relative_velocity` are the same phrase to a human.
        assert_eq!(
            ids(&filtered("storm relative", &availability)),
            ["SRV", "DSRV"]
        );
    }

    #[test]
    fn a_blank_filter_is_no_filter_and_case_does_not_matter() {
        let availability = ProductAvailabilityIndex::unrestricted();
        for blank in ["", " ", "   ", "\t", "\n "] {
            assert_eq!(
                filtered(blank, &availability).len(),
                DisplayProduct::ALL.len(),
                "{blank:?} hid a row"
            );
        }
        for spelling in ["kdp", "KDP", "KdP", " Kdp "] {
            assert_eq!(
                ids(&filtered(spelling, &availability)),
                ["KDP"],
                "{spelling:?}"
            );
        }
    }

    #[test]
    fn a_filter_that_hides_the_focused_row_moves_the_focus_to_one_that_is_drawn() {
        // The classic crash in a filtered list is a selection index left
        // pointing past the end of it. The focus is a product rather than an
        // index, so this checks the other half: that the product it names is
        // still on the list the analyst can see, and that Enter in the same
        // frame as the keystroke chooses from the new list rather than the old.
        let mut picker = Harness::open(DisplayProduct::ProbabilityOfSevereHail);
        picker.idle();
        assert_eq!(
            picker.state.focused(),
            Some(DisplayProduct::ProbabilityOfSevereHail)
        );
        let outcome = picker.frame(vec![
            egui::Event::Text("ref".to_owned()),
            key_event(egui::Key::Enter),
        ]);
        assert_eq!(picker.state.filter(), "ref");
        // "ref" matches REF and CREF; POSH is gone, so the focus moved to the
        // first row that is still drawn.
        assert_eq!(picker.state.focused(), Some(DisplayProduct::Reflectivity));
        assert_eq!(outcome.product, Some(DisplayProduct::Reflectivity));
    }

    #[test]
    fn a_filter_that_matches_nothing_survives_the_arrows_and_enter() {
        // Opened on VIL, so the frame also has a pane product that is not on
        // the list and a palette section with no family behind it.
        let mut picker = Harness::open(DisplayProduct::Vil).filtered("azimuthal shear");
        let outcome = picker.keys(&[egui::Key::ArrowDown, egui::Key::ArrowUp, egui::Key::Enter]);
        assert_eq!(outcome.product, None);
        assert_eq!(picker.state.focused(), None, "nothing is drawn to focus");
    }

    #[test]
    fn typing_goes_to_the_filter_rather_than_nowhere() {
        // The first frame claims the caret; the second is the analyst typing.
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        picker.idle();
        picker.frame(vec![egui::Event::Text("kdp".to_owned())]);
        assert_eq!(picker.state.filter(), "kdp");
        assert_eq!(
            picker.state.focused(),
            Some(DisplayProduct::SpecificDifferentialPhase)
        );
    }

    // --- the keyboard and the pointer --------------------------------------

    #[test]
    fn arrows_move_the_focus_enter_chooses_and_the_ends_clamp() {
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        // Navigate to the product by identity: adding another base moment must
        // not turn this into a test of VEL's absolute row number.
        picker.arrow_down_to(DisplayProduct::Velocity);
        let outcome = picker.keys(&[egui::Key::Enter]);
        assert_eq!(outcome.product, Some(DisplayProduct::Velocity));
        assert!(outcome.dismissed, "choosing a product asks to close");

        // More steps than there are products. Wrapping would put an analyst
        // holding the arrow key back near reflectivity without them noticing
        // they had passed the product they wanted.
        picker.state.opened(DisplayProduct::Reflectivity);
        let last = all_entries(&picker.availability)
            .last()
            .expect("the picker has products")
            .product;
        let mut events: Vec<egui::Event> = (0..DisplayProduct::ALL.len() + 10)
            .map(|_| key_event(egui::Key::ArrowDown))
            .collect();
        events.push(key_event(egui::Key::Enter));
        assert_eq!(picker.frame(events).product, Some(last));
    }

    #[test]
    fn arrowing_into_a_folded_group_unfolds_it() {
        // Otherwise the focus ring lands on a row that is not drawn, and the
        // keyboard appears to have stopped working.
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        picker
            .state
            .collapsed
            .insert(ProductGroup::VelocityAnalysis);
        let outcome = picker.arrow_down_to(DisplayProduct::Velocity);
        assert_eq!(outcome.product, None);
        assert_eq!(picker.state.focused(), Some(DisplayProduct::Velocity));
        assert!(
            !picker
                .state
                .collapsed
                .contains(&ProductGroup::VelocityAnalysis)
        );
    }

    #[test]
    fn escape_closes_without_choosing_anything() {
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        let outcome = picker.keys(&[egui::Key::Escape]);
        assert!(outcome.dismissed);
        assert_eq!(outcome.product, None);
    }

    #[test]
    fn a_group_folds_shut_when_its_header_is_clicked() {
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        picker.click(group_id(ProductGroup::Base));
        assert!(picker.state.collapsed.contains(&ProductGroup::Base));

        // Two more frames: the fold is recorded after the rows are drawn, and
        // a response survives one frame in egui's previous-pass table.
        picker.idle();
        picker.idle();
        assert!(
            !picker.drawn(row_id(DisplayProduct::Reflectivity)),
            "a folded group must stop drawing its rows"
        );
        // REF was the focused row and is now inside the fold, so the focus
        // moved to the first row still on screen rather than staying on one
        // the analyst cannot see - and Enter follows it there.
        assert_eq!(picker.state.focused(), Some(DisplayProduct::Velocity));
        assert_eq!(
            picker.keys(&[egui::Key::Enter]).product,
            Some(DisplayProduct::Velocity)
        );
    }

    #[test]
    fn with_every_group_folded_there_is_no_focus_and_an_arrow_opens_the_first() {
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        for group in ProductGroup::ALL {
            picker.state.collapsed.insert(group);
        }
        // Nothing is drawn, so nothing is focused and Enter has nothing to
        // choose - rather than choosing a row that is inside a fold.
        assert_eq!(picker.keys(&[egui::Key::Enter]).product, None);
        assert_eq!(picker.state.focused(), None);
        // One arrow lands on the first row and opens the group it is in.
        picker.keys(&[egui::Key::ArrowDown]);
        assert_eq!(picker.state.focused(), Some(DisplayProduct::Reflectivity));
        assert!(!picker.state.collapsed.contains(&ProductGroup::Base));
    }

    #[test]
    fn a_ui_with_no_room_in_it_draws_nothing_rather_than_panicking() {
        // The picker is drawn in an `Area` anchored under a toolbar button. A
        // window dragged to nothing, or a first frame before the host knows
        // its size, hands it a rectangle with no room in it.
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        for size in [
            egui::vec2(0.0, 0.0),
            egui::vec2(1.0, 1.0),
            egui::vec2(20.0, 8.0),
        ] {
            picker.frame_in(
                size,
                vec![key_event(egui::Key::ArrowDown), key_event(egui::Key::Enter)],
            );
        }
    }

    // --- availability ------------------------------------------------------

    #[test]
    fn an_unavailable_product_is_listed_with_its_reason_rather_than_hidden() {
        let mut availability = ProductAvailabilityIndex::unrestricted();
        availability.set(
            DisplayProduct::DifferentialReflectivity,
            ProductAvailability::Unavailable(UnavailableReason::MissingMoment(
                MomentType::DifferentialReflectivity,
            )),
        );
        let entries = all_entries(&availability);
        assert_eq!(
            entries.len(),
            DisplayProduct::ALL.len(),
            "an unavailable product must stay listed"
        );
        let zdr = entries
            .iter()
            .find(|entry| entry.product == DisplayProduct::DifferentialReflectivity)
            .expect("ZDR is listed");
        assert!(!zdr.is_available());
        assert_eq!(
            zdr.unavailable_label().as_deref(),
            Some("no ZDR in this volume")
        );
    }

    #[test]
    fn no_unavailable_product_can_be_chosen_by_the_keyboard_or_the_pointer() {
        // Every product in turn, both input paths. A greyed row that can still
        // be chosen is worse than no greying at all: it says the pane refused
        // and then changes the pane anyway.
        for product in DisplayProduct::ALL {
            // The only match, so the focus lands on it and Enter has nowhere
            // else to go.
            let mut picker = Harness::open(DisplayProduct::Reflectivity)
                .filtered(product.id())
                .greying(product, UnavailableReason::NoUsableCut);
            let by_key = picker.keys(&[egui::Key::Enter]);
            assert_eq!(by_key.product, None, "{} chosen by Enter", product.id());
            assert!(
                !by_key.dismissed,
                "{} closed the picker without choosing anything",
                product.id()
            );
            let by_click = picker.click(row_id(product));
            assert_eq!(by_click.product, None, "{} chosen by click", product.id());
        }
    }

    #[test]
    fn a_row_is_clickable_only_while_the_volume_can_draw_it() {
        // The control for the test above: both halves click the same row at
        // the same place, so a click that missed the row would fail the second
        // half rather than quietly pass the first.
        let mut picker = Harness::open(DisplayProduct::Reflectivity)
            .filtered("zdr")
            .greying(
                DisplayProduct::DifferentialReflectivity,
                UnavailableReason::MissingMoment(MomentType::DifferentialReflectivity),
            );
        let refused = picker.click(row_id(DisplayProduct::DifferentialReflectivity));
        assert_eq!(refused.product, None);

        picker.availability = ProductAvailabilityIndex::unrestricted();
        let chosen = picker.click(row_id(DisplayProduct::DifferentialReflectivity));
        assert_eq!(
            chosen.product,
            Some(DisplayProduct::DifferentialReflectivity)
        );
        assert!(chosen.dismissed);
    }

    #[test]
    fn a_product_whose_algorithm_may_not_produce_a_number_is_never_offered() {
        // `AlgorithmStatus::PendingPrimaryVerification` means the primary
        // source has not been read and no number may be produced. Nothing in
        // this build carries it, so it is applied to a copy of a descriptor
        // that does not: the point is that such a product cannot arrive
        // through the registry and reach an analyst as a selectable field.
        let mut descriptor = mesh_descriptor();
        descriptor.algorithm.status = AlgorithmStatus::PendingPrimaryVerification;
        let capabilities = reflectivity_only(2);
        assert_eq!(
            availability_in(&descriptor, &capabilities).unavailable_reason(),
            Some(&UnavailableReason::AlgorithmPendingPrimaryVerification),
            "the volume can feed it, but the algorithm may not answer"
        );
        // The same descriptor without the status is available, so the refusal
        // above is the status and not the capabilities.
        descriptor.algorithm.status = AlgorithmStatus::LiteratureAdaptation;
        assert!(availability_in(&descriptor, &capabilities).is_available());
    }

    #[test]
    fn an_experimental_product_is_badged_wherever_it_appears() {
        // The registry's own contract for `ProductVisibility::Experimental`.
        // Applied to a copy of a real descriptor because no built-in product
        // is experimental yet; the badge must be there the day one is.
        let mut descriptor = mesh_descriptor();
        descriptor.visibility = ProductVisibility::Experimental;
        let leaked: &'static ProductDescriptor = Box::leak(Box::new(descriptor));
        let entry = |availability| ProductEntry {
            product: DisplayProduct::Mesh,
            descriptor: leaked,
            availability,
        };
        let plain = ProductAvailability::available();
        assert_eq!(row_badges(&entry(&plain)), ["EXPERIMENTAL"]);

        // Beside a qualifier the volume reported, not instead of it.
        let assumed =
            ProductAvailability::available_with(vec![AvailabilityQualifier::AssumedEnvironment]);
        let with_qualifier = entry(&assumed);
        assert!(with_qualifier.is_available());
        assert_eq!(row_badges(&with_qualifier), ["EXPERIMENTAL", "ASSUMED ENV"]);
    }

    #[test]
    fn a_volume_with_no_dual_pol_greys_the_dual_pol_products_with_the_reason() {
        // A hand-built capability set, which is enough to pin the mapping from
        // rule to reason. The real-volume test below is what proves it holds
        // for a radar.
        let availability = ProductAvailabilityIndex::from_capabilities(&reflectivity_only(1));
        assert_eq!(
            availability
                .get(DisplayProduct::DifferentialReflectivity)
                .unavailable_reason(),
            Some(&UnavailableReason::MissingMoment(
                MomentType::DifferentialReflectivity
            ))
        );
        assert_eq!(
            availability
                .get(DisplayProduct::Velocity)
                .unavailable_reason(),
            Some(&UnavailableReason::MissingMoment(MomentType::Velocity))
        );
        assert!(
            availability
                .get(DisplayProduct::Reflectivity)
                .is_available()
        );
    }

    #[test]
    fn relative_power_alone_does_not_unlock_reflectivity_products() {
        let capabilities = capabilities_of(vec![
            measured_cut(0, 0.5, &[MomentType::RelativePower], None),
            measured_cut(1, 1.5, &[MomentType::RelativePower], None),
        ]);
        let availability = ProductAvailabilityIndex::from_capabilities(&capabilities);
        assert!(
            availability
                .get(DisplayProduct::RelativePower)
                .is_available()
        );
        for product in [
            DisplayProduct::Reflectivity,
            DisplayProduct::CompositeReflectivity,
            DisplayProduct::EchoTop18,
            DisplayProduct::Vil,
            DisplayProduct::VilDensity,
            DisplayProduct::Mesh,
            DisplayProduct::ProbabilityOfHail,
            DisplayProduct::ProbabilityOfSevereHail,
        ] {
            assert_eq!(
                availability.get(product).unavailable_reason(),
                Some(&UnavailableReason::MissingMoment(MomentType::Reflectivity)),
                "{} became available from uncalibrated power",
                product.id()
            );
        }
    }

    #[test]
    fn a_single_tilt_cannot_integrate_a_column() {
        let availability = ProductAvailabilityIndex::from_capabilities(&reflectivity_only(1));
        assert_eq!(
            availability.get(DisplayProduct::Vil).unavailable_reason(),
            Some(&UnavailableReason::InsufficientUniqueElevations)
        );
        // The second commanded tilt is what makes a column a column.
        let availability = ProductAvailabilityIndex::from_capabilities(&reflectivity_only(2));
        assert!(availability.get(DisplayProduct::Vil).is_available());
    }

    #[test]
    fn velocity_without_a_nyquist_cannot_be_presented_as_unfolded() {
        // The failure this guards is silent: a "dealiased" field that was
        // never unfolded looks entirely reasonable and is wrong by multiples
        // of the Nyquist velocity.
        let capabilities = capabilities_of(vec![
            measured_cut(0, 0.5, &[MomentType::Velocity], None),
            measured_cut(1, 1.5, &[MomentType::Velocity], None),
        ]);
        let availability = ProductAvailabilityIndex::from_capabilities(&capabilities);
        assert!(availability.get(DisplayProduct::Velocity).is_available());
        assert_eq!(
            availability
                .get(DisplayProduct::DealiasedVelocity)
                .unavailable_reason(),
            Some(&UnavailableReason::NoDealiasedVelocity)
        );
    }

    #[test]
    fn before_a_volume_loads_nothing_is_greyed_out() {
        let availability = ProductAvailabilityIndex::from_optional_capabilities(None);
        for product in DisplayProduct::ALL {
            assert!(
                availability.get(product).is_available(),
                "{} is greyed out before any volume said so",
                product.id()
            );
        }
    }

    // --- palettes ----------------------------------------------------------

    #[test]
    fn a_palette_offer_changes_exactly_the_product_it_is_offered_for() {
        // The pin against a second palette catalog, and the test that caught
        // the first one: keying the offer on `default_palette` told an analyst
        // that ZDR, RHO, PHI and KDP had no alternative table, while the
        // render path drew all four from the Generic family. Installing a
        // marker into one family at a time asks the render path itself which
        // family owns each product rather than asking this file to agree.
        for product in DisplayProduct::ALL {
            let offered = palette_family(product);
            for family in ColorTableFamily::ALL {
                let mut tables = ColorTableSet::default();
                let before = crate::palettes::table_for(product.descriptor(), &tables);
                tables.set_family(family, marker_table());
                let after = crate::palettes::table_for(product.descriptor(), &tables);
                let changed = after.name() != before.name();
                assert_eq!(
                    changed,
                    offered == Some(family),
                    "{}: installing into {family:?} changed what it draws = {changed}, \
                     but the picker offers {offered:?}",
                    product.id()
                );
            }
        }
    }

    #[test]
    fn the_families_a_reader_would_question_are_the_ones_the_pane_draws_with() {
        // A composite is reflectivity and must offer reflectivity's tables.
        assert_eq!(
            palette_family(DisplayProduct::CompositeReflectivity),
            Some(ColorTableFamily::Reflectivity)
        );
        // Relative stored-I/Q power must never borrow reflectivity's visual
        // language: it has no radar constant and is not dBZ.
        assert_eq!(
            palette_family(DisplayProduct::RelativePower),
            Some(ColorTableFamily::Generic)
        );
        // Each dual-polarimetric moment now draws from its own family. It used
        // to draw from Generic, whose ramp spans 0..100 - so ZDR, which lives
        // on -13..20 dB, rendered as one flat colour. This assertion is what
        // stops that being reintroduced by a wildcard arm in
        // `render2d::color_family_for_moment`.
        assert_eq!(
            palette_family(DisplayProduct::DifferentialReflectivity),
            Some(ColorTableFamily::DifferentialReflectivity)
        );
        assert_eq!(
            palette_family(DisplayProduct::CorrelationCoefficient),
            Some(ColorTableFamily::CorrelationCoefficient)
        );
        assert_eq!(
            palette_family(DisplayProduct::DifferentialPhase),
            Some(ColorTableFamily::DifferentialPhase)
        );
        assert_eq!(
            palette_family(DisplayProduct::SpecificDifferentialPhase),
            Some(ColorTableFamily::SpecificDifferentialPhase)
        );
        assert_eq!(
            palette_family(DisplayProduct::DowReceivedPowerH1),
            Some(ColorTableFamily::ReceivedPower)
        );
        assert_eq!(
            palette_family(DisplayProduct::DowReflectivityVMerged),
            Some(ColorTableFamily::Reflectivity)
        );
        // VIL draws on a ramp built for kg/m2; a dBZ table is not an option.
        assert_eq!(palette_family(DisplayProduct::Vil), None);
    }

    #[test]
    fn the_palette_list_is_the_family_list_and_is_parsed_once_per_family() {
        // The tables are parsed from text, and the picker asks for them every
        // frame it is open. Same allocation twice means it parsed once.
        let mut state = ProductPickerState::default();
        let defaults = ColorTableSet::default();
        let vel_installed = defaults.for_family(ColorTableFamily::Velocity);
        let first = state.palettes_for(ColorTableFamily::Velocity, vel_installed, None);
        // Counted against the family list rather than a literal. A literal here
        // says "there are seven velocity tables", which is not what this test
        // is about and which fails every time a palette is added - so the
        // failure would arrive on the wrong test with the wrong message. The
        // +1 is the switch row: the installed palette redrawn the other way.
        assert_eq!(
            first.len(),
            builtin_tables_for_family(ColorTableFamily::Velocity).len() + 1,
            "the picker dropped or invented a velocity table"
        );
        let address = first.as_ptr();
        assert_eq!(
            state
                .palettes_for(ColorTableFamily::Velocity, vel_installed, None)
                .as_ptr(),
            address
        );
        assert_eq!(
            state
                .palettes_for(
                    ColorTableFamily::Reflectivity,
                    defaults.for_family(ColorTableFamily::Reflectivity),
                    None
                )
                .len(),
            builtin_tables_for_family(ColorTableFamily::Reflectivity).len() + 1
        );
    }

    #[test]
    fn the_palette_rows_on_screen_are_the_focused_products_own_family() {
        // A velocity table offered while reflectivity is focused would install
        // silently and change nothing the analyst is looking at.
        let mut picker = Harness::open(DisplayProduct::Reflectivity);
        picker.idle();
        assert_eq!(
            picker.palette_rows(ColorTableFamily::Reflectivity),
            builtin_tables_for_family(ColorTableFamily::Reflectivity).len() + 1
        );
        assert_eq!(
            picker.palette_rows(ColorTableFamily::Velocity),
            0,
            "velocity tables offered for reflectivity"
        );

        // Arrow onto velocity by identity, then give egui one frame to retain
        // the newly drawn palette-row responses.
        picker.arrow_down_to(DisplayProduct::Velocity);
        picker.idle();
        assert_eq!(picker.state.focused(), Some(DisplayProduct::Velocity));
        assert_eq!(
            picker.palette_rows(ColorTableFamily::Velocity),
            builtin_tables_for_family(ColorTableFamily::Velocity).len() + 1
        );
        assert_eq!(
            picker.palette_rows(ColorTableFamily::Reflectivity),
            0,
            "reflectivity tables still offered after moving onto velocity"
        );
    }

    #[test]
    fn clicking_a_palette_returns_the_table_and_its_family() {
        // An interpolated-authored table, so the picker's offer carries the
        // same name the catalog does regardless of the analyst's rendering.
        let alternative = builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .find(|table| table.name() == "Smooth Doppler VEL (interpolated)")
            .expect("the velocity family ships Smooth Doppler");
        let mut picker = Harness::open(DisplayProduct::Velocity);
        let outcome = picker.click(palette_row_id(alternative.name()));
        let selection = outcome.palette.expect("a palette was chosen");
        assert_eq!(selection.family, ColorTableFamily::Velocity);
        assert_eq!(selection.table.name(), alternative.name());
        assert_eq!(
            outcome.product, None,
            "a palette click must not also change the product"
        );
    }

    /// A velocity palette in the GR2Analyst/RadarScope dialect, for the
    /// tests that put a file in a folder and expect a picker row.
    const USER_VELOCITY_PAL: &str = "Product: BV
Units: KTS
Color: -60 200   0 200    60 220 220
Color:  60 220  60  60   255 255 255
";

    /// A colour table folder of this test's own, removed at the end.
    fn scratch_dir(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("product-picker-user-tables")
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

    #[test]
    fn a_table_dropped_while_the_picker_is_open_appears_in_the_list() {
        // The pin for the `user_generation` third of the palette cache's key
        // (`settings_ui::PaletteOfferCache`). Take that clause out and an
        // open picker keeps serving the list it built on the frame it
        // opened - which is exactly the window in which an analyst drops a
        // palette onto the application. Nothing else in this file renders a
        // user-table row at all, so without this the whole suite stays green
        // through that deletion.
        let folder = scratch_dir("dropped-while-open");
        let mut picker = Harness::open(DisplayProduct::Velocity).reading_tables_from(&folder);
        assert!(
            !picker
                .painted()
                .iter()
                .any(|text| text.contains("Dropped Velocity")),
            "the folder starts empty"
        );

        std::fs::write(folder.join("Dropped Velocity.pal"), USER_VELOCITY_PAL)
            .expect("write palette");
        picker.rescan_user_tables();
        assert!(
            picker
                .painted()
                .iter()
                .any(|text| text.contains("Dropped Velocity")),
            "a table that arrived while the picker was open must appear in it"
        );

        // And it is a real row, not just painted text: it can be clicked and
        // it installs that table for the family.
        let installed = ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .clone();
        let row = color_tables::user::palette_offers_with_user_tables(
            ColorTableFamily::Velocity,
            &installed,
            picker.user_tables.as_ref().expect("a folder was given"),
        )
        .into_iter()
        .find(|table| table.base_name() == "Dropped Velocity")
        .expect("the dropped table is offered");
        let selection = picker
            .click(palette_row_id(row.name()))
            .palette
            .expect("the dropped table's row is clickable");
        assert_eq!(selection.family, ColorTableFamily::Velocity);
        assert_eq!(selection.table.base_name(), "Dropped Velocity");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The edit affordance sits on top of the row it is drawn in, so pressing
    /// it opens the editor and does NOT also install the palette. Pinned
    /// because the two hit rects overlap and the answer depends on the order
    /// they are registered in.
    #[test]
    fn pressing_a_palette_rows_edit_affordance_opens_the_editor_without_installing_it() {
        let alternative = builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .find(|table| table.name() == "Smooth Doppler VEL (interpolated)")
            .expect("the velocity family ships Smooth Doppler");
        let mut picker = Harness::open(DisplayProduct::Velocity);
        let outcome = picker.click(palette_edit_id(alternative.name()));
        let request = outcome.edit_palette.expect("the editor was asked for");
        assert_eq!(request.family, ColorTableFamily::Velocity);
        assert_eq!(request.table.name(), alternative.name());
        assert!(
            request.duplicate,
            "a shipped preset must open as a duplicate; presets are never overwritten"
        );
        assert_eq!(
            outcome.palette, None,
            "pressing edit must not also install the palette"
        );
        assert_eq!(outcome.product, None);
    }

    /// A palette the catalogue does not hold - one an analyst loaded - is
    /// opened rather than copied, because there is a file of theirs to write.
    #[test]
    fn a_palette_this_build_does_not_ship_opens_for_editing_rather_than_copying() {
        let mut tables = ColorTableSet::default();
        let loaded = marker_table();
        tables.set_family(ColorTableFamily::Velocity, loaded.clone());
        let mut picker = Harness::open(DisplayProduct::Velocity);
        picker.tables = tables;
        let outcome = picker.click(palette_edit_id(loaded.name()));
        let request = outcome.edit_palette.expect("the editor was asked for");
        assert!(
            !request.duplicate,
            "a table that is not in the catalogue is the analyst's own"
        );
        assert_eq!(request.table.name(), loaded.name());
    }

    /// The two affordances, on the two kinds of row, decided by the same
    /// cached set - with a real file in a real folder rather than a table
    /// handed in by a test.
    ///
    /// This is where the folder feature and the editor feature meet: the
    /// picker offers the analyst's own tables beside the shipped ones, and the
    /// row's affordance has to tell them apart. A shipped preset opens as a
    /// COPY, because presets are never overwritten. An imported file opens for
    /// EDITING, because there is a file of the analyst's to write back to. Get
    /// that backwards and either the catalogue is editable or an analyst's own
    /// table can only ever be duplicated.
    #[test]
    fn an_imported_table_offers_edit_while_a_shipped_preset_offers_copy() {
        let folder = scratch_dir("edit-vs-copy");
        std::fs::write(folder.join("Field Velocity.pal"), USER_VELOCITY_PAL)
            .expect("write palette");
        let mut picker = Harness::open(DisplayProduct::Velocity).reading_tables_from(&folder);

        let installed = ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .clone();
        let offers = color_tables::user::palette_offers_with_user_tables(
            ColorTableFamily::Velocity,
            &installed,
            picker.user_tables.as_ref().expect("a folder was given"),
        );
        let imported = offers
            .iter()
            .find(|table| table.base_name() == "Field Velocity")
            .expect("the imported file is offered")
            .clone();
        let shipped = offers
            .iter()
            .find(|table| {
                color_tables::is_builtin_table(ColorTableFamily::Velocity, table.base_name())
            })
            .expect("the shipped catalogue is offered too")
            .clone();

        let request = picker
            .click(palette_edit_id(imported.name()))
            .edit_palette
            .expect("the imported row asks for the editor");
        assert!(
            !request.duplicate,
            "an analyst's own file has a file to write back to, so it is edited"
        );
        assert_eq!(request.table.base_name(), "Field Velocity");
        assert_eq!(request.family, ColorTableFamily::Velocity);

        let request = picker
            .click(palette_edit_id(shipped.name()))
            .edit_palette
            .expect("the shipped row asks for the editor");
        assert!(
            request.duplicate,
            "a shipped preset is copied; presets are never overwritten"
        );

        // And the table the editor is handed is the FILE's, not a preset of
        // the same shape: its first stop is the one the file declares.
        assert_eq!(
            imported.stops()[0].color,
            color_tables::Rgba8::opaque(200, 0, 200),
            "the row carried the imported file's own colours"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A table no built-in family holds, so "did this change?" has one answer.
    fn marker_table() -> ColorTable {
        use color_tables::{ColorStop, Rgba8};
        ColorTable::new(
            "PICKER MARKER",
            vec![
                ColorStop {
                    value: -1000.0,
                    color: Rgba8::opaque(1, 2, 3),
                    end_color: None,
                },
                ColorStop {
                    value: 1000.0,
                    color: Rgba8::opaque(4, 5, 6),
                    end_color: None,
                },
            ],
        )
        .expect("two ascending stops")
    }

    // --- the headless harness ----------------------------------------------

    /// One picker, driven a frame at a time: the egui context (which holds
    /// last frame's layout, and is what `read_response` reads), the picker
    /// state, the colour tables in force, and what the volume can draw.
    struct Harness {
        context: egui::Context,
        state: ProductPickerState,
        tables: ColorTableSet,
        /// The analyst's colour table folder, when a test supplies one.
        /// `None` is the shipped catalogue alone, which is what almost every
        /// test here is about.
        user_tables: Option<UserTableLibrary>,
        availability: ProductAvailabilityIndex,
        source_fields: SourceFieldCatalog,
        current: DisplayProduct,
        current_source_field: Option<String>,
    }

    impl Harness {
        /// A picker just opened on `current`, with nothing greyed out.
        fn open(current: DisplayProduct) -> Self {
            let mut state = ProductPickerState::default();
            state.opened(current);
            Self {
                context: egui::Context::default(),
                state,
                tables: ColorTableSet::default(),
                user_tables: None,
                availability: ProductAvailabilityIndex::unrestricted(),
                source_fields: SourceFieldCatalog::default(),
                current,
                current_source_field: None,
            }
        }

        fn inspecting(mut self, source_fields: SourceFieldCatalog) -> Self {
            self.source_fields = source_fields;
            self
        }

        /// Give this picker an analyst's colour table folder, scanned now.
        /// Without this every test here runs on the shipped catalogue alone,
        /// which is what left the folder's half of the palette cache key
        /// with no coverage at all.
        fn reading_tables_from(mut self, directory: &std::path::Path) -> Self {
            self.user_tables = Some(UserTableLibrary::open(directory));
            self
        }

        /// Re-read that folder, the way the application does when its window
        /// comes back to the front.
        fn rescan_user_tables(&mut self) {
            let library = self
                .user_tables
                .as_mut()
                .expect("this harness was given a folder");
            assert!(
                library.refresh(),
                "the folder moved, so the rescan must read it"
            );
        }

        fn filtered(mut self, filter: &str) -> Self {
            self.state.filter = filter.to_owned();
            self
        }

        fn greying(mut self, product: DisplayProduct, reason: UnavailableReason) -> Self {
            self.availability
                .set(product, ProductAvailability::Unavailable(reason));
            self
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> ProductPickerOutcome {
            self.frame_in(egui::vec2(900.0, 1600.0), events)
        }

        fn frame_in(&mut self, size: egui::Vec2, events: Vec<egui::Event>) -> ProductPickerOutcome {
            self.run(size, events).0
        }

        fn run(
            &mut self,
            size: egui::Vec2,
            events: Vec<egui::Event>,
        ) -> (ProductPickerOutcome, egui::FullOutput) {
            let mut outcome = ProductPickerOutcome::default();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            };
            let output = self.context.run_ui(raw, |ui| {
                outcome = draw_product_picker(
                    ui,
                    ProductPickerInput {
                        state: &mut self.state,
                        current: self.current,
                        current_source_field: self.current_source_field.as_deref(),
                        availability: &self.availability,
                        source_fields: &self.source_fields,
                        tables: &self.tables,
                        user_tables: self.user_tables.as_ref(),
                        show_experimental: false,
                    },
                );
            });
            (outcome, output)
        }

        fn idle(&mut self) -> ProductPickerOutcome {
            self.frame(Vec::new())
        }

        fn keys(&mut self, keys: &[egui::Key]) -> ProductPickerOutcome {
            self.frame(keys.iter().copied().map(key_event).collect())
        }

        /// Walk down until the keyboard focus reaches `target`, without
        /// baking the registry position of that product into a test.
        fn arrow_down_to(&mut self, target: DisplayProduct) -> ProductPickerOutcome {
            let mut outcome = ProductPickerOutcome::default();
            for _ in 0..=DisplayProduct::ALL.len() {
                if self.state.focused() == Some(target) {
                    return outcome;
                }
                outcome = self.keys(&[egui::Key::ArrowDown]);
            }
            panic!("could not reach {} by arrowing down", target.id());
        }

        /// Lay the picker out, then press and release on one widget's own rect.
        fn click(&mut self, id: egui::Id) -> ProductPickerOutcome {
            self.idle();
            let position = self
                .context
                .read_response(id)
                .expect("the widget was drawn this frame")
                .rect
                .center();
            self.frame(click_events(position, true));
            self.frame(click_events(position, false))
        }

        fn drawn(&self, id: egui::Id) -> bool {
            self.context.read_response(id).is_some()
        }

        /// How many of a family's offered tables were drawn last frame,
        /// including the switch row (the installed palette flipped).
        fn palette_rows(&self, family: ColorTableFamily) -> usize {
            palette_offers_for_family(family, ColorTableSet::default().for_family(family))
                .iter()
                .filter(|table| self.drawn(palette_row_id(table.name())))
                .count()
        }

        /// Every string the picker painted this frame, in draw order.
        ///
        /// `FullOutput::shapes` is the shape list before tessellation, so a
        /// row scrolled below the viewport is still in it: this reads what the
        /// widget drew, not what happens to be on screen.
        fn painted(&mut self) -> Vec<String> {
            let (_, output) = self.run(egui::vec2(900.0, 1600.0), Vec::new());
            let mut painted = Vec::new();
            for clipped in &output.shapes {
                collect_text(&clipped.shape, &mut painted);
            }
            painted
        }
    }

    fn collect_text(shape: &egui::Shape, into: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => into.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, into);
                }
            }
            _ => {}
        }
    }

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn click_events(position: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn mesh_descriptor() -> ProductDescriptor {
        ProductRegistry::builtin()
            .get("MESH")
            .expect("MESH is in the registry")
            .clone()
    }

    /// One measured sweep, as `VolumeCapabilities::analyze` would report it.
    ///
    /// Built directly rather than by analysing a fabricated `RadarVolume`:
    /// what is under test is the mapping from a measurement to a reason, and
    /// hand-rolling radials to reach it only adds a second thing that can be
    /// wrong.
    fn measured_cut(
        index: usize,
        elevation_deg: f32,
        moments: &[MomentType],
        nyquist_mps: Option<f32>,
    ) -> CutCapabilities {
        CutCapabilities {
            identity: CutIdentity {
                index: index as u32,
                elevation_millidegrees: (elevation_deg * 1000.0).round() as i32,
                elevation_number: Some(index as u8 + 1),
                median_radial_time_ms: 0,
            },
            index,
            nominal_elevation_deg: elevation_deg,
            stored_elevation_deg: elevation_deg,
            elevation_spread_deg: 0.1,
            leg: CutLeg::Combined,
            moments: moments.iter().cloned().collect(),
            radial_count: 720,
            azimuth_coverage_deg: 360.0,
            complete: true,
            representative_nyquist_mps: nyquist_mps,
            encoded_range_km: BTreeMap::new(),
            median_radial_time_ms: 0,
        }
    }

    /// A volume of `tilts` commanded elevations carrying reflectivity alone.
    fn reflectivity_only(tilts: usize) -> VolumeCapabilities {
        capabilities_of(
            (0..tilts)
                .map(|index| {
                    let elevation = 0.5 + index as f32;
                    measured_cut(index, elevation, &[MomentType::Reflectivity], Some(26.0))
                })
                .collect(),
        )
    }

    fn capabilities_of(cuts: Vec<CutCapabilities>) -> VolumeCapabilities {
        let moments = cuts
            .iter()
            .flat_map(|cut| cut.moments.iter().cloned())
            .collect();
        let mut groups: Vec<NominalElevationGroup> = Vec::new();
        for cut in &cuts {
            match groups
                .iter_mut()
                .find(|group| (group.elevation_deg - cut.nominal_elevation_deg).abs() < 0.15)
            {
                Some(group) => group.members.push(cut.index),
                None => groups.push(NominalElevationGroup {
                    elevation_deg: cut.nominal_elevation_deg,
                    members: vec![cut.index],
                }),
            }
        }
        VolumeCapabilities {
            cuts: cuts.into(),
            groups: groups.into(),
            moments,
        }
    }

    // --- a real radar -------------------------------------------------------

    /// Every Level II volume in a directory, driven through the real widget.
    ///
    /// Ignored by default because it needs a radar on disk. Run it with
    /// `NEXRAD_LEVEL2_CACHE` pointing at the live cache, or
    /// `NEXRAD_LEVEL2_SAMPLE` at one file: it decodes each volume, builds the
    /// index from what the volume actually measured, and then tries to choose
    /// every product from the keyboard - expecting exactly the ones the volume
    /// says it can draw.
    #[ignore = "set NEXRAD_LEVEL2_CACHE (or NEXRAD_LEVEL2_SAMPLE) to real Archive II data"]
    #[test]
    fn a_real_volume_answers_for_every_product_and_refuses_what_it_cannot_draw() {
        for path in sample_volumes() {
            let volume = nexrad_io::decode_volume_from_path(&path)
                .unwrap_or_else(|error| panic!("{} did not decode: {error}", path.display()));
            let capabilities = VolumeCapabilities::analyze(&volume);
            let availability = ProductAvailabilityIndex::from_capabilities(&capabilities);
            let entries = all_entries(&availability);
            assert_eq!(
                entries.len(),
                DisplayProduct::ALL.len(),
                "{}",
                path.display()
            );
            let greyed: Vec<String> = entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .unavailable_label()
                        .map(|reason| format!("{}={reason}", entry.descriptor.short_name))
                })
                .collect();
            println!(
                "{} {} cuts={} tilts={}\n    {}",
                volume.site.id,
                volume.volume_time,
                capabilities.cuts.len(),
                capabilities.groups.len(),
                if greyed.is_empty() {
                    "everything available".to_owned()
                } else {
                    greyed.join(", ")
                }
            );
            // A WSR-88D volume always carries reflectivity; if it does not,
            // the picker is not what is broken.
            assert!(
                availability
                    .get(DisplayProduct::Reflectivity)
                    .is_available()
            );
            for entry in &entries {
                let mut picker = Harness::open(DisplayProduct::Reflectivity);
                picker.availability = availability.clone();
                picker.state.filter = entry.product.id().to_owned();
                let outcome = picker.keys(&[egui::Key::Enter]);
                assert!(
                    picker.drawn(row_id(entry.product)),
                    "{} was not drawn for {}",
                    entry.product.id(),
                    volume.site.id
                );
                assert_eq!(
                    outcome.product,
                    entry.is_available().then_some(entry.product),
                    "{} on {}: {:?}",
                    entry.product.id(),
                    volume.site.id,
                    entry.unavailable_label()
                );
            }
        }
    }

    /// The volumes to run over: a whole cache directory, or one file.
    fn sample_volumes() -> Vec<std::path::PathBuf> {
        if let Ok(directory) = std::env::var("NEXRAD_LEVEL2_CACHE") {
            let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
                .expect("the cache directory is readable")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            paths.sort();
            assert!(!paths.is_empty(), "{directory} holds no volumes");
            return paths;
        }
        let one = std::env::var("NEXRAD_LEVEL2_SAMPLE")
            .expect("set NEXRAD_LEVEL2_CACHE or NEXRAD_LEVEL2_SAMPLE");
        vec![std::path::PathBuf::from(one)]
    }
}
