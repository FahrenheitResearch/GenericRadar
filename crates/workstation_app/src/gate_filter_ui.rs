//! The gate filter as an analyst touches it: the presets, the live control on
//! the toolbar, and the words the pane uses to admit that it is filtering.
//!
//! The filter itself is `render2d::GateFilter` (see that module for the
//! science and the citations). Nothing in this file censors anything - it
//! turns five persisted numbers into a `GateFilter`, hands it to the render
//! path, and makes very sure the analyst can see that it did.
//!
//! # Why the settings are five keys and not nine
//!
//! Four of the five criteria are thresholds, and a threshold has to be able to
//! be *off*. The obvious shape is a toggle beside each slider, which is nine
//! controls for five ideas and one more thing to get out of step. Instead each
//! slider's leftmost position is off, and the number at that position is one
//! that would censor nothing even if a future build applied it literally
//! (-35 dBZ is below the WSR-88D's encoded floor of -32.0 dBZ; RhoHV cannot be
//! negative; nothing is closer than 0 km). The ranges live in
//! `settings_ui::catalog::radar_filter`, beside the specs that declare them.
//!
//! # Why the preset is not persisted
//!
//! "Storm mode" is not a sixth stored fact - it is the *name of a set of
//! numbers*, resolved by [`preset_for`] from the five that are stored. So
//! picking Storm mode, quitting and reopening shows Storm mode, because the
//! numbers on disk are still Storm mode's; and nudging one slider shows
//! Custom, because they are no longer. A stored preset id could disagree with
//! the numbers beside it, and then one of the two would be lying about what is
//! being hidden. This cannot.
//!
//! # The safety rule
//!
//! A filter must never quietly remove weather. Three things say so, and they
//! are deliberately redundant because the pane's furniture is configurable and
//! the toolbar's is not:
//!
//! * the toolbar chip latches - sunken and tinted - whenever any criterion is
//!   on, and names the preset;
//! * every pane draws [`pane_banner_text`] under its header, in the theme's
//!   error ink on its own opaque band, whether or not the colour legend is
//!   switched on. Clicking that band clears every criterion, which is the one
//!   obvious action out;
//! * the legend badge stack carries [`pane_badge_text`] as well, directly
//!   under the stall badge.
//!
//! None of the three infers anything from the absence of echo.

use eframe::egui;
use render2d::GateFilter;
use settings::{SettingValue, SettingsRegistry, SettingsStore};

use crate::settings_ui::catalog::keys::radar as k;
use crate::settings_ui::catalog::radar_filter as bounds;
use crate::theme::bevel;

/// Slider step for a reflectivity threshold, in dBZ. One step is one encoded
/// data value: the WSR-88D stores reflectivity in half-decibel increments
/// (NOAA/NWS ICD 2620002, Level II data format), so a finer step would offer
/// precision the measurement does not have.
const DBZ_STEP: f64 = 0.5;
/// Slider step for RhoHV. Two decimals is how every published threshold for it
/// is written.
const RHO_STEP: f64 = 0.01;
/// Slider step for the near-range cut, in km.
const RANGE_STEP_KM: f64 = 0.5;

/// How close two criteria have to be to count as the same one.
///
/// Not a tolerance on the analyst's intent - 5.0 dBZ and 5.2 dBZ are different
/// filters and are named differently - only on the float noise a 0.01 step
/// leaves behind. Half a step would be far too loose; this is six orders of
/// magnitude below the finest step here.
const MATCH_EPSILON: f64 = 1e-6;

/// What a pane says when it is hiding gates. Also the word the tests look for.
pub const FILTERED_WORD: &str = "FILTERED";

/// The label for a set of numbers no preset names.
pub const CUSTOM_LABEL: &str = "Custom";

/// The five criteria as the numbers the sliders move and the settings file
/// holds, before they become a `GateFilter`'s `Option`s.
///
/// This shape exists so there is exactly one place the "leftmost is off"
/// convention is applied ([`FilterValues::to_filter`]) and exactly one place
/// the file's five keys are named ([`values_from_settings`] /
/// [`write_values`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterValues {
    pub min_dbz: f64,
    pub vel_needs_dbz: f64,
    pub min_rho: f64,
    pub hide_rf: bool,
    pub min_range_km: f64,
}

impl FilterValues {
    /// Every criterion at its off position. Must resolve to
    /// [`GateFilter::OFF`]; a test pins it.
    pub const OFF: Self = Self {
        min_dbz: bounds::OFF_MIN_DBZ,
        vel_needs_dbz: bounds::OFF_MIN_DBZ,
        min_rho: bounds::OFF_MIN_RHO,
        hide_rf: false,
        min_range_km: bounds::OFF_MIN_RANGE_KM,
    };

    /// The filter these numbers describe. A criterion still sitting on its off
    /// position becomes `None`, which is what makes "all the way left" mean
    /// "show everything" rather than "threshold at an absurd number".
    pub fn to_filter(self) -> GateFilter {
        GateFilter {
            min_reflectivity_dbz: above(self.min_dbz, bounds::OFF_MIN_DBZ),
            velocity_requires_reflectivity_dbz: above(self.vel_needs_dbz, bounds::OFF_MIN_DBZ),
            min_correlation: above(self.min_rho, bounds::OFF_MIN_RHO),
            hide_range_folded: self.hide_rf,
            min_range_km: above(self.min_range_km, bounds::OFF_MIN_RANGE_KM),
        }
    }

    fn matches(self, other: Self) -> bool {
        self.hide_rf == other.hide_rf
            && near(self.min_dbz, other.min_dbz)
            && near(self.vel_needs_dbz, other.vel_needs_dbz)
            && near(self.min_rho, other.min_rho)
            && near(self.min_range_km, other.min_range_km)
    }
}

fn above(value: f64, off: f64) -> Option<f32> {
    (value > off + MATCH_EPSILON).then_some(value as f32)
}

fn near(left: f64, right: f64) -> bool {
    (left - right).abs() <= MATCH_EPSILON
}

/// One named set of criteria.
///
/// Plain data, and the whole table is [`PRESETS`]: adding one is a row, and
/// nothing else in this file or in `app.rs` has to learn about it.
pub struct FilterPreset {
    /// Stable identifier. Not persisted (see the module doc) - it is here so
    /// tests and future deep links can name a preset without its label.
    pub id: &'static str,
    /// What the control shows.
    pub label: &'static str,
    /// One or two sentences saying what this hides AND what it costs. The
    /// second half is not optional: a preset that only advertises what it
    /// cleans up is how an analyst loses a signature without noticing.
    pub blurb: &'static str,
    pub values: FilterValues,
}

/// The shipped presets, in the order the control lists them.
pub const PRESETS: &[FilterPreset] = &[
    FilterPreset {
        id: "off",
        label: "Off / show everything",
        blurb: "Every gate the radar reported is painted. This is what the application \
                ships with and what it returns to.",
        values: FilterValues::OFF,
    },
    FilterPreset {
        id: "clean-air",
        label: "Clean air",
        blurb: "For a bloom: drops gates under 5 dBZ and gates whose RhoHV says the \
                scatterers are not hydrometeors - birds, insects, chaff, ground clutter. \
                It also drops drizzle, light snow and the thin outer edge of an anvil.",
        values: FilterValues {
            min_dbz: 5.0,
            min_rho: 0.80,
            ..FilterValues::OFF
        },
    },
    FilterPreset {
        id: "storm",
        label: "Storm mode",
        blurb: "Keeps the precipitation cores and the velocity inside them, and clears \
                the near-radar clutter ring. RhoHV is left OFF on purpose: a debris \
                ball, a hail shaft and the melting layer all read low, so censoring on \
                it in a storm removes the signatures you are looking for.",
        values: FilterValues {
            min_dbz: 20.0,
            vel_needs_dbz: 20.0,
            min_range_km: 5.0,
            ..FilterValues::OFF
        },
    },
];

/// The preset these numbers are, if any. `None` is [`CUSTOM_LABEL`].
pub fn preset_for(values: FilterValues) -> Option<&'static FilterPreset> {
    PRESETS.iter().find(|preset| values.matches(preset.values))
}

/// The preset's label, or `"Custom"`.
pub fn selection_label(values: FilterValues) -> &'static str {
    preset_for(values).map_or(CUSTOM_LABEL, |preset| preset.label)
}

// --- persistence ------------------------------------------------------------

/// Read the five criteria out of the store.
///
/// Every read goes through `SettingsStore::effective_*`, so anything missing,
/// malformed or out of range resolves to the declared default rather than
/// panicking - a hand-edited `"filter_min_rho": "yes"` or a `filter_min_dbz`
/// of 900 comes back as *off*.
///
/// The four thresholds are declared `settings::SliderFloor::Off` for exactly
/// that reason. An ordinary slider clamps an out-of-range number to the
/// nearest end, which is right for a dim level and wrong here: 900 would clamp
/// to `MAX_MIN_DBZ`, the strongest censor the control offers, so a file nobody
/// meant to write would start hiding weather. Off is the direction a censoring
/// control has to fail in.
///
/// None of this writes: the stranger value stays in the file until somebody
/// deliberately moves that control.
pub fn values_from_settings(registry: &SettingsRegistry, store: &SettingsStore) -> FilterValues {
    let float = |id: &str| store.effective_float(registry, k::CATEGORY, id);
    FilterValues {
        min_dbz: float(k::FILTER_MIN_DBZ),
        vel_needs_dbz: float(k::FILTER_VEL_NEEDS_DBZ),
        min_rho: float(k::FILTER_MIN_RHO),
        hide_rf: store.effective_bool(registry, k::CATEGORY, k::FILTER_HIDE_RF),
        min_range_km: float(k::FILTER_MIN_RANGE_KM),
    }
}

/// The filter the store currently describes.
pub fn filter_from_settings(registry: &SettingsRegistry, store: &SettingsStore) -> GateFilter {
    values_from_settings(registry, store).to_filter()
}

/// Write all five criteria. Returns whether anything actually changed -
/// `SettingsStore::set` compares before dirtying, so re-applying the preset
/// that is already on does not schedule a save.
pub fn write_values(store: &mut SettingsStore, values: FilterValues) -> bool {
    let mut changed = false;
    changed |= store.set(
        k::CATEGORY,
        k::FILTER_MIN_DBZ,
        SettingValue::Float(values.min_dbz),
    );
    changed |= store.set(
        k::CATEGORY,
        k::FILTER_VEL_NEEDS_DBZ,
        SettingValue::Float(values.vel_needs_dbz),
    );
    changed |= store.set(
        k::CATEGORY,
        k::FILTER_MIN_RHO,
        SettingValue::Float(values.min_rho),
    );
    changed |= store.set(
        k::CATEGORY,
        k::FILTER_HIDE_RF,
        SettingValue::Bool(values.hide_rf),
    );
    changed |= store.set(
        k::CATEGORY,
        k::FILTER_MIN_RANGE_KM,
        SettingValue::Float(values.min_range_km),
    );
    changed
}

// --- the words ---------------------------------------------------------------

/// The toolbar chip's text.
pub fn chip_text(values: FilterValues) -> String {
    if values.to_filter().is_active() {
        format!("Filter: {} ⏷", selection_label(values))
    } else {
        // Not "no filter": the chip has to read as a control that is there and
        // is off, not as a label for something that does not exist.
        "Filter: off ⏷".to_owned()
    }
}

/// The chip's hover text. Hover is a bonus here, never the only affordance -
/// the pane banner carries the same facts where there is no pointer.
pub fn chip_hover(values: FilterValues) -> String {
    let filter = values.to_filter();
    if filter.is_active() {
        format!(
            "Gates are being hidden: {}. Every pane says so, and clicking a pane's \
             FILTERED band shows everything again.",
            filter.hidden_summary()
        )
    } else {
        "Hide gates by reflectivity, RhoHV, range folding or range. Nothing is hidden \
         right now."
            .to_owned()
    }
}

/// The band every pane draws under its header while filtering is on.
///
/// It names the criteria rather than summarising them as "filtered", because
/// "filtered" alone does not tell an analyst whether the missing echo was
/// weak, decorrelated, folded or close in.
///
/// The verb here is `hiding` and the phrase after it comes from
/// [`GateFilter::hidden_summary`], which is written from the hidden side for
/// all five criteria. The two have to agree in sense or this sentence states
/// the inverse of what the pane did - which is what it did state, for three
/// of the five criteria, until the summary was rewritten. `the_band_says_what
/// _was_removed_for_every_criterion` reads the built string rather than the
/// pieces, so the pair cannot drift apart again without a test failing.
pub fn pane_banner_text(filter: &GateFilter) -> Option<String> {
    filter.is_active().then(|| {
        format!(
            "{FILTERED_WORD} · hiding {} · click here to show everything",
            filter.hidden_summary()
        )
    })
}

/// What a pane says when a filter is on and this pane's product could not obey
/// it.
///
/// Deliberately NOT a substring of [`FILTERED_WORD`], and deliberately not
/// built out of it: an analyst scanning four panes has to be able to tell the
/// pane that is hiding gates from the pane that is showing everything, and two
/// bands that start with the same word do not let them.
pub const NOT_APPLIED_WORDS: &str = "FILTER NOT APPLIED HERE";

/// The band THIS pane draws, given what its product can do with the filter.
///
/// `not_applied_reason` is `None` for every pane that rasters from a sweep's
/// gates - which is every radar moment, and the only case an ordinary session
/// ever sees. It is `Some` for a product the filter cannot run against, and
/// then the band says so in the engine's own words rather than claiming gates
/// are hidden here when none are.
///
/// The alternative was one string for the whole canvas, which is what this
/// started as. It cannot survive contact with the engine: `render_service`
/// answers a volume-derived product with
/// `GateFilterReport::not_applicable`, so that pane's status line reads
/// `FILTER NOT APPLIED` while a canvas-wide band over the same pane would read
/// `FILTERED`. Two indicators on one pane disagreeing about whether weather is
/// being hidden is worse than either of them alone, and the fix is for both to
/// come from the same fact.
///
/// Silent in neither direction: a pane that cannot filter still carries a
/// band, because an analyst who has switched a censor on and sees no band at
/// all will read the pane as obeying it.
pub fn pane_banner_text_for(
    filter: &GateFilter,
    not_applied_reason: Option<&str>,
) -> Option<String> {
    if !filter.is_active() {
        return None;
    }
    match not_applied_reason {
        None => pane_banner_text(filter),
        Some(reason) => Some(format!(
            "{NOT_APPLIED_WORDS} · {} · {reason} · click here to show everything",
            filter.hidden_summary()
        )),
    }
}

/// The legend's copy of the same statement: one word, and only one word.
///
/// The first version of this carried the summary as well, and the photograph
/// of it settled the question - `crate::legend`'s badge column is 38 to 50
/// points wide, so "FILTERED · REF below 20 dBZ, VEL where REF below 20 dBZ, inside
/// 5 km" wrapped to four rows of two or three characters, spent the badge
/// stack's whole row budget, pushed the colour bar down by sixty points and
/// was unreadable at every step. The full statement belongs on the band, which
/// has the whole width of the pane; the badge's job beside the colour bar is
/// to catch the eye that is already reading the ladder, and one word does that
/// better than four unreadable rows.
///
/// Kept as a `Option<String>` rather than a `&'static str` so the caller's
/// "exists exactly when something is hidden" rule is the same shape for both
/// indicators.
pub fn pane_badge_text(filter: &GateFilter) -> Option<String> {
    filter.is_active().then(|| FILTERED_WORD.to_owned())
}

// --- the control -------------------------------------------------------------

/// The chip's between-frame state. Owned by `WorkstationApp`.
#[derive(Default)]
pub struct GateFilterUi {
    pub open: bool,
}

/// Everything one frame of the control needs.
pub struct GateFilterControl<'a> {
    pub state: &'a mut GateFilterUi,
    pub registry: &'a SettingsRegistry,
    pub store: &'a mut SettingsStore,
}

/// Draw the toolbar chip and, while it is down, the filter panel under it.
/// Returns whether any criterion changed this frame - the caller re-reads the
/// filter and re-renders.
///
/// **Live, and deliberately not timer-debounced.** A drag writes the store
/// every frame it moves, and `app.rs` answers by bumping each visible pane's
/// view clock. The render lane is newest-wins per pane
/// (`analyst_runtime::latest_lane_channel`), so a request that is superseded
/// while the worker is busy is dropped rather than queued: the coalescing a
/// debounce timer would add is already in the transport, and adding a second
/// one would only make the pane lag the slider. The *file* is debounced
/// separately by the settings store (two seconds of quiet), so a drag is one
/// write, not sixty.
pub fn draw_gate_filter_control(ui: &mut egui::Ui, control: GateFilterControl<'_>) -> bool {
    let values = values_from_settings(control.registry, control.store);
    let filter = values.to_filter();

    // Latched on the FILTER, not on the popup. A control that is only tinted
    // while its own popup is open says nothing in a screenshot, and "is this
    // pane hiding gates" is exactly the question a screenshot gets asked.
    let button = bevel::toolbar_toggle(ui, filter.is_active(), chip_text(values))
        .on_hover_text(chip_hover(values));
    let mut opened_this_frame = false;
    if button.clicked() {
        control.state.open = !control.state.open;
        opened_this_frame = control.state.open;
    }

    let mut changed = false;
    if control.state.open {
        let area = egui::Area::new(egui::Id::new("workstation-gate-filter"))
            .order(egui::Order::Foreground)
            // Ten points clear of the button, like the product picker: the
            // band carries six points of margin and a two-point bevel below
            // this control, and a popup that starts four points down slices
            // through both.
            .fixed_pos(button.rect.left_bottom() + egui::vec2(0.0, 10.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| draw_panel(ui, control.store, values))
            });
        let popup_rect = area.response.rect;
        changed = area.inner.inner;
        // The same dismissal rule the product picker uses, and for the same
        // reason - see `crate::popup`. Nothing inside the panel dismisses it:
        // this is a panel you use by dragging, and a picker that closed on
        // every choice would close on the first pixel of a drag.
        let dismissal = crate::popup::dismissal_from_input(
            ui.ctx(),
            popup_rect,
            button.rect,
            opened_this_frame,
            false,
        );
        if dismissal.should_close() {
            control.state.open = false;
        }
    }
    changed
}

/// Panel width in points. Wide enough that a slider keeps a usable travel with
/// its label beside it, narrow enough to sit under a toolbar chip.
const PANEL_WIDTH: f32 = 380.0;

/// Returns whether any criterion changed this frame.
fn draw_panel(ui: &mut egui::Ui, store: &mut SettingsStore, values: FilterValues) -> bool {
    ui.set_min_width(PANEL_WIDTH);
    ui.set_max_width(PANEL_WIDTH);
    let mut changed = false;

    ui.label(egui::RichText::new("Gate filter").strong());
    // What this panel does and what it does NOT do, in one line, because a
    // control that removes weather owes the analyst a plain account of its
    // reach. The readout is named explicitly: it FOLLOWS the filter, and it has
    // to be said here because the alternative reading - that the cursor still
    // quotes the measurement - is the natural one and is wrong. A number under
    // the cursor at a pixel the pane drew empty is the same lie as a censored
    // sweep with no band on it, arriving from the other side.
    ui.label(
        egui::RichText::new(
            "A hidden gate is not drawn at all, and reads as \
             \u{201c}FILTERED\u{201d} under the cursor. Nothing here changes the data, or \
             what the 3D and cross-section windows compute.",
        )
        .small()
        .weak(),
    );
    bevel::etched_separator(ui);

    // Live within the frame, not the snapshot the frame started with. A
    // preset row and the four sliders below it are drawn in one pass, so
    // reading `values` throughout meant the frame a preset was clicked on
    // still drew the *previous* preset's numbers and the previous FILTERED
    // line - a panel that disagrees with itself for one frame about what is
    // being hidden. `write_values` has already put these numbers in the store;
    // this is the same numbers, in hand, without waiting for the read back.
    let mut values = values;
    for preset in PRESETS {
        // Recomputed per row rather than hoisted, so a row drawn after the one
        // just clicked highlights against the new numbers.
        let chosen = preset_for(values).is_some_and(|selected| selected.id == preset.id);
        if ui
            .selectable_label(chosen, preset.label)
            .on_hover_text(preset.blurb)
            .clicked()
            && !chosen
        {
            changed |= write_values(store, preset.values);
            values = preset.values;
        }
    }
    let current = preset_for(values);
    // Custom is a state, not a choice: it appears when the numbers are the
    // analyst's own, shows as the selected row, and clicking it does nothing
    // because there is nothing to select it back to.
    if current.is_none() {
        let _ = ui.selectable_label(true, CUSTOM_LABEL);
    }
    ui.label(
        egui::RichText::new(current.map_or(
            "Your own thresholds. Any preset above replaces them.",
            |preset| preset.blurb,
        ))
        .small()
        .weak(),
    );

    bevel::etched_separator(ui);

    changed |= threshold_row(
        ui,
        store,
        ThresholdRow {
            id: k::FILTER_MIN_DBZ,
            label: "Hide REF below",
            value: &mut values.min_dbz,
            off: bounds::OFF_MIN_DBZ,
            max: bounds::MAX_MIN_DBZ,
            step: DBZ_STEP,
            decimals: 1,
            unit: " dBZ",
        },
    );
    changed |= threshold_row(
        ui,
        store,
        ThresholdRow {
            id: k::FILTER_VEL_NEEDS_DBZ,
            label: "Hide VEL where REF below",
            value: &mut values.vel_needs_dbz,
            off: bounds::OFF_MIN_DBZ,
            max: bounds::MAX_MIN_DBZ,
            step: DBZ_STEP,
            decimals: 1,
            unit: " dBZ",
        },
    );
    changed |= threshold_row(
        ui,
        store,
        ThresholdRow {
            id: k::FILTER_MIN_RHO,
            label: "Hide below RhoHV",
            value: &mut values.min_rho,
            off: bounds::OFF_MIN_RHO,
            max: bounds::MAX_MIN_RHO,
            step: RHO_STEP,
            decimals: 2,
            unit: "",
        },
    );
    let mut hide_rf = values.hide_rf;
    if ui
        .checkbox(&mut hide_rf, "Hide range-folded gates")
        .on_hover_text(
            "The RF colour says the Doppler ambiguity could not be resolved at that gate. \
             Hiding it makes an unresolved gate look like clear air.",
        )
        .changed()
    {
        changed |= store.set(k::CATEGORY, k::FILTER_HIDE_RF, SettingValue::Bool(hide_rf));
    }
    changed |= threshold_row(
        ui,
        store,
        ThresholdRow {
            id: k::FILTER_MIN_RANGE_KM,
            label: "Hide inside",
            value: &mut values.min_range_km,
            off: bounds::OFF_MIN_RANGE_KM,
            max: bounds::MAX_MIN_RANGE_KM,
            step: RANGE_STEP_KM,
            decimals: 1,
            unit: " km",
        },
    );

    bevel::etched_separator(ui);

    // The state, said out loud, immediately above the way out of it.
    let filter = values.to_filter();
    if filter.is_active() {
        ui.label(
            egui::RichText::new(format!("{FILTERED_WORD} · {}", filter.hidden_summary()))
                .color(ui.visuals().error_fg_color)
                .strong(),
        );
    } else {
        ui.label(egui::RichText::new("Nothing is hidden.").small().weak());
    }
    if ui
        .add_sized(
            [PANEL_WIDTH, bevel::MIN_TOUCH_POINTS],
            egui::Button::new("Show everything"),
        )
        .on_hover_text(
            "Turn every criterion off. Also reachable by clicking a pane's FILTERED band.",
        )
        .clicked()
    {
        changed |= write_values(store, FilterValues::OFF);
    }

    if changed {
        // The way out is the last widget on the panel, so its own effect
        // cannot reach the lines drawn above it until the frame after the
        // click. Ask for that frame now rather than waiting for the analyst
        // to move the mouse: an "everything is hidden" line that outlives the
        // click that cleared it is exactly the wrong thing to leave on screen.
        ui.ctx().request_repaint();
    }
    changed
}

/// One threshold slider's declaration, so the four calls read as data.
struct ThresholdRow<'a> {
    id: &'static str,
    label: &'static str,
    /// Borrowed, not copied: a drag writes the new number back through it, so
    /// the FILTERED line drawn below this row on the same frame states what
    /// the slider says now rather than what it said last frame.
    value: &'a mut f64,
    off: f64,
    max: f64,
    step: f64,
    decimals: usize,
    unit: &'static str,
}

fn threshold_row(ui: &mut egui::Ui, store: &mut SettingsStore, row: ThresholdRow<'_>) -> bool {
    let mut value = *row.value;
    let (off, decimals, unit) = (row.off, row.decimals, row.unit);
    let slider = egui::Slider::new(&mut value, row.off..=row.max)
        .text(row.label)
        .step_by(row.step)
        // The readout says "off" at the leftmost position rather than the
        // number that happens to sit there. Without this the control reads
        // "-35.0 dBZ", which is a threshold nobody chose and looks like one
        // somebody did.
        .custom_formatter(move |shown, _| {
            if shown <= off + MATCH_EPSILON {
                "off".to_owned()
            } else {
                format!("{shown:.decimals$}{unit}")
            }
        });
    if ui.add(slider).changed() {
        *row.value = value;
        return store.set(k::CATEGORY, row.id, SettingValue::Float(value));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_store() -> SettingsStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        SettingsStore::open(std::env::temp_dir().join(format!(
            "gate-filter-ui-test-{}-{}.json",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }

    /// The half of the shared contract the written contract forgot to state.
    ///
    /// `SettingsCache` is `Copy` and holds a `GateFilter` by value; `app.rs`
    /// compares one to `GateFilter::OFF` with `assert_eq!`; several tests
    /// print one in a failure message. So the engine branch's `GateFilter`
    /// owes this branch `Copy + Clone + Debug + PartialEq + Default`, and the
    /// contract text named none of them. This is a compile-time assertion on
    /// purpose: a merge that drops a derive fails here, with a message that
    /// says which one, instead of somewhere in `app.rs`.
    #[test]
    fn the_engine_contract_carries_the_derives_this_branch_depends_on() {
        fn requires_the_contracts_derives<T>(_: &T)
        where
            T: Copy + Clone + std::fmt::Debug + PartialEq + Default,
        {
        }
        requires_the_contracts_derives(&GateFilter::OFF);
        // Default is not just present, it is OFF: a `GateFilter::default()`
        // that censored anything would arrive through every `..Default()`
        // struct update in the app.
        assert_eq!(GateFilter::default(), GateFilter::OFF);
    }

    /// The promise the whole feature rests on: a fresh install censors
    /// nothing, so its pictures are the pictures it drew before this existed.
    #[test]
    fn a_fresh_settings_file_resolves_to_the_off_filter() {
        let registry = crate::settings_ui::catalog::registry();
        let store = scratch_store();
        let values = values_from_settings(&registry, &store);
        assert_eq!(values, FilterValues::OFF);
        assert_eq!(values.to_filter(), GateFilter::OFF);
        assert!(!values.to_filter().is_active());
        assert_eq!(selection_label(values), "Off / show everything");
    }

    #[test]
    fn the_off_values_and_the_off_filter_are_the_same_thing() {
        assert_eq!(FilterValues::OFF.to_filter(), GateFilter::OFF);
        assert_eq!(preset_for(FilterValues::OFF).map(|p| p.id), Some("off"));
    }

    /// Every preset must be reachable BY ITS NUMBERS, or the control would
    /// apply one and then immediately report Custom.
    #[test]
    fn every_preset_names_itself_after_a_round_trip_through_the_store() {
        let registry = crate::settings_ui::catalog::registry();
        for preset in PRESETS {
            let mut store = scratch_store();
            write_values(&mut store, preset.values);
            let read_back = values_from_settings(&registry, &store);
            assert_eq!(
                read_back, preset.values,
                "{} did not survive the store",
                preset.id
            );
            assert_eq!(
                preset_for(read_back).map(|found| found.id),
                Some(preset.id),
                "{} came back as {}",
                preset.id,
                selection_label(read_back)
            );
        }
    }

    /// The restart requirement, on a real file: pick Storm mode, save, reopen
    /// the store from disk, and it is still Storm mode - not Custom with equal
    /// numbers.
    #[test]
    fn a_preset_survives_a_real_save_and_reopen_as_itself() {
        let registry = crate::settings_ui::catalog::registry();
        let dir = std::env::temp_dir().join(format!(
            "gate-filter-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("settings.json");
        let storm = PRESETS
            .iter()
            .find(|preset| preset.id == "storm")
            .expect("storm mode is declared");
        {
            let mut store = SettingsStore::open(&path);
            assert!(write_values(&mut store, storm.values));
            store.save_now().expect("save");
        }
        let reopened = SettingsStore::open(&path);
        let values = values_from_settings(&registry, &reopened);
        assert_eq!(values, storm.values);
        assert_eq!(selection_label(values), storm.label);
        assert_eq!(
            values.to_filter().hidden_summary(),
            "REF below 20 dBZ, VEL where REF below 20 dBZ, everything inside 5 km"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nudging one slider off a preset is Custom, and Custom still says what
    /// it is hiding.
    #[test]
    fn moving_one_threshold_off_a_preset_reads_as_custom() {
        let storm = PRESETS
            .iter()
            .find(|preset| preset.id == "storm")
            .expect("storm mode is declared");
        let nudged = FilterValues {
            min_dbz: storm.values.min_dbz + DBZ_STEP,
            ..storm.values
        };
        assert_eq!(selection_label(nudged), CUSTOM_LABEL);
        assert!(nudged.to_filter().is_active());
        assert!(
            pane_banner_text(&nudged.to_filter())
                .expect("an active filter has a banner")
                .contains("20.5 dBZ")
        );
    }

    /// No two presets may be the same numbers, or the first would shadow the
    /// second and one row of the control could never be selected.
    #[test]
    fn no_two_presets_share_their_numbers_or_their_ids() {
        for (index, preset) in PRESETS.iter().enumerate() {
            for other in &PRESETS[index + 1..] {
                assert_ne!(preset.id, other.id, "duplicate preset id {}", preset.id);
                assert!(
                    !preset.values.matches(other.values),
                    "{} and {} are the same filter",
                    preset.id,
                    other.id
                );
            }
        }
    }

    /// A preset that hides something has to say what it costs, not only what
    /// it cleans up.
    #[test]
    fn every_preset_has_a_blurb_and_a_label() {
        for preset in PRESETS {
            assert!(!preset.label.is_empty(), "{} has no label", preset.id);
            assert!(!preset.blurb.is_empty(), "{} has no blurb", preset.id);
        }
    }

    /// The safety rule at the level this module owns: active means the words
    /// exist, off means they do not.
    #[test]
    fn the_pane_words_exist_exactly_when_something_is_hidden() {
        assert_eq!(pane_banner_text(&GateFilter::OFF), None);
        assert_eq!(pane_badge_text(&GateFilter::OFF), None);
        for preset in PRESETS {
            let filter = preset.values.to_filter();
            let banner = pane_banner_text(&filter);
            let badge = pane_badge_text(&filter);
            assert_eq!(
                filter.is_active(),
                banner.is_some(),
                "{} disagrees about whether to warn",
                preset.id
            );
            assert_eq!(filter.is_active(), badge.is_some());
            if let Some(banner) = banner {
                assert!(banner.starts_with(FILTERED_WORD));
                assert!(
                    banner.contains(&filter.hidden_summary()),
                    "{} does not name what it hides",
                    preset.id
                );
                assert!(
                    banner.contains("show everything"),
                    "{} offers no way out",
                    preset.id
                );
                assert_eq!(badge.expect("badge"), FILTERED_WORD);
            }
        }
    }

    /// The band's WHOLE SENTENCE, for every criterion alone and for all five
    /// together, pinned as an analyst would read it aloud.
    ///
    /// This reads the built string rather than the pieces, because the defect
    /// this guards lived in neither piece. `pane_banner_text` supplies the
    /// verb - "hiding" - and `GateFilter::hidden_summary` supplied a phrase
    /// written from the SURVIVING side for three of the five criteria, so the
    /// sentence they made together announced the opposite of what the pane had
    /// done. What shipped, on a real KDVN volume in Storm mode, was:
    ///
    /// ```text
    /// FILTERED · hiding REF > 20 dBZ, VEL needs REF > 20 dBZ, beyond 5 km · …
    /// ```
    ///
    /// over a picture in which everything above 20 dBZ is precisely what
    /// SURVIVED and everything inside 5 km is what went. Both halves were
    /// individually defensible and the sentence was false, which is why the
    /// pin is on the sentence.
    ///
    /// The expected strings below are written the way an analyst would say
    /// them out loud - "it's hiding REF below 20 dBZ" - so re-inverting one
    /// cannot be done quietly: the diff would have to replace plain English
    /// with its opposite in this file, in words, where a reviewer reads it.
    #[test]
    fn the_band_says_what_was_removed_for_every_criterion() {
        let cases: [(GateFilter, &str); 6] = [
            (
                GateFilter {
                    min_reflectivity_dbz: Some(20.0),
                    ..GateFilter::OFF
                },
                "FILTERED · hiding REF below 20 dBZ · click here to show everything",
            ),
            (
                GateFilter {
                    velocity_requires_reflectivity_dbz: Some(20.0),
                    ..GateFilter::OFF
                },
                "FILTERED · hiding VEL where REF below 20 dBZ · click here to show everything",
            ),
            (
                GateFilter {
                    min_correlation: Some(0.80),
                    ..GateFilter::OFF
                },
                "FILTERED · hiding RhoHV below 0.80 · click here to show everything",
            ),
            (
                GateFilter {
                    hide_range_folded: true,
                    ..GateFilter::OFF
                },
                "FILTERED · hiding range-folded gates · click here to show everything",
            ),
            (
                GateFilter {
                    min_range_km: Some(5.0),
                    ..GateFilter::OFF
                },
                "FILTERED · hiding everything inside 5 km · click here to show everything",
            ),
            // All five at once, which is also the widest the band ever gets.
            (
                GateFilter {
                    min_reflectivity_dbz: Some(20.0),
                    velocity_requires_reflectivity_dbz: Some(20.0),
                    min_correlation: Some(0.95),
                    hide_range_folded: true,
                    min_range_km: Some(5.0),
                },
                "FILTERED · hiding REF below 20 dBZ, VEL where REF below 20 dBZ, \
                 RhoHV below 0.95, range-folded gates, everything inside 5 km · \
                 click here to show everything",
            ),
        ];
        for (filter, expected) in cases {
            assert_eq!(
                pane_banner_text(&filter).as_deref(),
                Some(expected),
                "{filter:?}: the band is the one place an analyst goes to find out \
                 what is missing. Every word after \"hiding\" has to name what WENT"
            );
        }
    }

    /// Storm mode is the preset the defect was photographed under, so its
    /// exact shipped sentence is pinned on its own.
    ///
    /// The panel that sets these numbers prints "Hide REF below 20.0 dBZ" and
    /// "Hide inside 5.0 km" on its own sliders, inches from this band. The two
    /// used to disagree on the same screen; this asserts the agreement in the
    /// only way that survives an edit to either - by quoting both.
    #[test]
    fn storm_mode_band_and_the_panel_that_set_it_say_the_same_thing() {
        let storm = PRESETS
            .iter()
            .find(|preset| preset.id == "storm")
            .expect("storm mode is declared");
        let band = pane_banner_text(&storm.values.to_filter()).expect("storm mode filters");
        assert_eq!(
            band,
            "FILTERED · hiding REF below 20 dBZ, VEL where REF below 20 dBZ, \
             everything inside 5 km · click here to show everything"
        );
        // The panel's own slider labels, verbatim from `draw_panel`. A band
        // that says "below" while the control says "below" is one sentence; a
        // band that says "REF > 20 dBZ" over a slider reading "Hide REF below
        // 20.0 dBZ" is two indicators contradicting each other in one window.
        for label in ["Hide REF below", "Hide VEL where REF below", "Hide inside"] {
            let stem = label.trim_start_matches("Hide ");
            assert!(
                band.contains(stem),
                "the band does not use the panel's own words for {label:?}: {band:?}"
            );
        }
    }

    /// A hand-edited or future-build file must degrade **to off** rather than
    /// panic, and reading it must not rewrite it.
    ///
    /// The direction is the whole point. This used to assert that a stored
    /// `filter_min_dbz` of 900 resolved to `MAX_MIN_DBZ` - "clamped, not
    /// blank" - which is what an ordinary slider does and is the wrong answer
    /// for a censor: 40 dBZ on the KDVN scene this was built against removes
    /// the bloom, the whole precipitation shield and most of the convective
    /// line, from a number no analyst ever chose. The four thresholds are now
    /// declared `SliderFloor::Off`, so an unaccountable number is read as the
    /// control being off.
    #[test]
    fn a_stranger_value_falls_back_without_touching_the_document() {
        let registry = crate::settings_ui::catalog::registry();
        let mut store = scratch_store();
        // Out of range high, out of range low, and the wrong type entirely.
        store.set(k::CATEGORY, k::FILTER_MIN_DBZ, SettingValue::Float(900.0));
        // Only just outside the range - what a build with a slightly wider
        // slider would leave behind, and the case a clamp gets most wrong,
        // because the number looks entirely plausible.
        store.set(
            k::CATEGORY,
            k::FILTER_VEL_NEEDS_DBZ,
            SettingValue::Float(bounds::MAX_MIN_DBZ + 1.0),
        );
        store.set(k::CATEGORY, k::FILTER_MIN_RHO, SettingValue::Float(-4.0));
        store.set(
            k::CATEGORY,
            k::FILTER_MIN_RANGE_KM,
            SettingValue::Text("as close as possible".to_owned()),
        );
        store.set(k::CATEGORY, k::FILTER_HIDE_RF, SettingValue::Int(7));

        let values = values_from_settings(&registry, &store);
        assert_eq!(
            values.min_dbz,
            bounds::OFF_MIN_DBZ,
            "an unaccountable threshold resolved to a censor"
        );
        assert_eq!(values.vel_needs_dbz, bounds::OFF_MIN_DBZ);
        assert_eq!(values.min_rho, bounds::OFF_MIN_RHO);
        assert_eq!(
            values.min_range_km,
            bounds::OFF_MIN_RANGE_KM,
            "a non-number falls back to the default"
        );
        assert!(!values.hide_rf, "a non-bool falls back to the default");
        // The claim that matters, said once more as a whole: a file this
        // build cannot read hides nothing at all.
        assert_eq!(values, FilterValues::OFF);
        assert_eq!(values.to_filter(), GateFilter::OFF);
        assert!(
            !values.to_filter().is_active(),
            "a corrupt settings file switched the filter on"
        );
        // And the file still says what it said: resolution is a read.
        assert_eq!(
            store.value(k::CATEGORY, k::FILTER_MIN_DBZ),
            Some(SettingValue::Float(900.0))
        );
        assert_eq!(
            store.value(k::CATEGORY, k::FILTER_MIN_RANGE_KM),
            Some(SettingValue::Text("as close as possible".to_owned()))
        );
    }

    /// The panel does not spend a frame disagreeing with itself.
    ///
    /// Driven through real egui, one pass at a time, because the claim is
    /// about a single frame. The preset rows are drawn before the four
    /// thresholds and before the panel's own FILTERED line, so a panel that
    /// read its numbers from a snapshot taken at the top of the frame drew
    /// the *previous* preset's numbers underneath a row that had just been
    /// clicked - a control that says one thing at the top and another at the
    /// bottom about what is being hidden. It corrected itself on the next
    /// frame, which is exactly why nothing that settles first can catch it:
    /// the proof example's photographs settle three passes and look right
    /// either way.
    #[test]
    fn the_frame_a_preset_is_clicked_on_already_shows_that_presets_numbers() {
        fn texts(shapes: &[egui::Shape]) -> Vec<String> {
            fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(text) => found.push(text.galley.text().trim().to_owned()),
                    egui::Shape::Vec(nested) => nested.iter().for_each(|s| walk(s, found)),
                    _ => {}
                }
            }
            let mut found = Vec::new();
            shapes.iter().for_each(|shape| walk(shape, &mut found));
            found
        }
        fn position(shapes: &[egui::Shape], wanted: &str) -> Option<egui::Pos2> {
            fn walk(shape: &egui::Shape, wanted: &str) -> Option<egui::Pos2> {
                match shape {
                    egui::Shape::Text(text) if text.galley.text().trim() == wanted => {
                        Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                    }
                    egui::Shape::Vec(nested) => nested.iter().find_map(|s| walk(s, wanted)),
                    _ => None,
                }
            }
            shapes.iter().find_map(|shape| walk(shape, wanted))
        }

        let registry = crate::settings_ui::catalog::registry();
        let mut store = scratch_store();
        let mut state = GateFilterUi::default();
        let context = egui::Context::default();
        let frame =
            |store: &mut SettingsStore, state: &mut GateFilterUi, events: Vec<egui::Event>| {
                let input = egui::RawInput {
                    events,
                    ..Default::default()
                };
                let output = context.run_ui(input, |ui| {
                    draw_gate_filter_control(
                        ui,
                        GateFilterControl {
                            state,
                            registry: &registry,
                            store,
                        },
                    );
                });
                output
                    .shapes
                    .into_iter()
                    .map(|clipped| clipped.shape)
                    .collect::<Vec<_>>()
            };
        let press = |at: egui::Pos2, pressed: bool| {
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };

        // Open the panel by clicking the real chip.
        let closed = frame(&mut store, &mut state, Vec::new());
        let chip = position(&closed, "Filter: off ⏷").expect("the chip drew its own label");
        frame(&mut store, &mut state, press(chip, true));
        frame(&mut store, &mut state, press(chip, false));

        // Find the Storm mode row, and click it. egui fires `clicked` on the
        // release, so the release pass is the frame under examination.
        let open = frame(&mut store, &mut state, Vec::new());
        let row = position(&open, "Storm mode")
            .unwrap_or_else(|| panic!("the panel drew no Storm mode row: {:?}", texts(&open)));
        assert!(
            !texts(&open).iter().any(|text| text == "20.0 dBZ"),
            "the panel already carried Storm mode's numbers before it was clicked"
        );
        frame(&mut store, &mut state, press(row, true));
        let clicked_frame = texts(&frame(&mut store, &mut state, press(row, false)));

        assert_eq!(
            clicked_frame
                .iter()
                .filter(|text| *text == "20.0 dBZ")
                .count(),
            2,
            "the two dBZ thresholds still read last frame's numbers: {clicked_frame:?}"
        );
        assert!(
            clicked_frame.iter().any(|text| text == "5.0 km"),
            "the near-range threshold still reads last frame's number: {clicked_frame:?}"
        );
        let storm = PRESETS
            .iter()
            .find(|preset| preset.id == "storm")
            .expect("the storm preset is declared");
        let expected_line = format!(
            "{FILTERED_WORD} · {}",
            storm.values.to_filter().hidden_summary()
        );
        assert!(
            clicked_frame.contains(&expected_line),
            "the panel's own FILTERED line is a frame behind the row above it: wanted \
             {expected_line:?} in {clicked_frame:?}"
        );
    }

    /// A criterion sitting exactly on its off position is `None`, and one step
    /// above it is `Some`. This is the boundary the whole "leftmost is off"
    /// convention turns on.
    #[test]
    fn the_off_position_is_none_and_one_step_up_is_some() {
        let off = FilterValues::OFF.to_filter();
        assert_eq!(off.min_reflectivity_dbz, None);
        assert_eq!(off.min_correlation, None);
        assert_eq!(off.min_range_km, None);

        let nudged = FilterValues {
            min_dbz: bounds::OFF_MIN_DBZ + DBZ_STEP,
            min_rho: bounds::OFF_MIN_RHO + RHO_STEP,
            min_range_km: bounds::OFF_MIN_RANGE_KM + RANGE_STEP_KM,
            hide_rf: true,
            ..FilterValues::OFF
        }
        .to_filter();
        assert_eq!(nudged.min_reflectivity_dbz, Some(-34.5));
        assert_eq!(nudged.min_correlation, Some(0.01));
        assert_eq!(nudged.min_range_km, Some(0.5));
        assert!(nudged.hide_range_folded);
    }
}
