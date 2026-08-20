//! The editor window: header, gradient strip, stop list, live preview.
//!
//! Deliberately thin. Every decision this file makes is about pixels - where a
//! handle is, which row is selected, when a texture is stale. What a colour
//! table *is*, what a unit change does to it and what its file says all live in
//! [`super::model`], [`super::pal`] and [`super::store`], which have no egui in
//! them and are tested without a window.
//!
//! Two rules the layout follows and one it refuses to break:
//!
//! * every colour on screen comes from [`crate::theme::palette::Palette`] or
//!   out of the table being edited, so the window looks like the rest of the
//!   instrument in both theme variants rather than declaring its own greys;
//! * every interactive element is at least [`bevel::MIN_TOUCH_POINTS`] on a
//!   side - the strip handles included, which is why their hit rects are far
//!   larger than the triangles drawn in them;
//! * the strip and the preview are sampled through the **real**
//!   [`ColorTable`], rebuilt from the editor state whenever it changes. There
//!   is no second sampler here and there must never be one: a preview drawn by
//!   different code from the pane is a preview that can lie.

use std::path::PathBuf;

use color_tables::{ColorTable, ColorTableFamily, Rgba8};
use eframe::egui;
use radar_core::{MomentType, RadarVolume};

use super::model::{
    EditorTable, EditorUnits, Sampling, StopId, family_from_product_token, product_token,
};
use super::store;
use crate::theme::bevel::{self, Bevel};
use crate::theme::palette::Palette;

/// Height of the gradient strip, in points. Tall enough to read a colour
/// against its neighbours rather than as a hairline.
const STRIP_HEIGHT: f32 = 46.0;
/// Height of the handle track under the strip.
const TRACK_HEIGHT: f32 = 26.0;
/// Half the width of a strip handle. The value axis is inset by this at both
/// ends so the first and last handles sit inside the strip instead of half
/// over its edge.
const HANDLE_HALF: f32 = 7.0;
/// Side of one square of the transparency checkerboard, in points.
pub(super) const CHECKER: f32 = 8.0;
/// Most vertical strips the gradient is painted in. One per point up to this,
/// which is finer than any display resolves and bounds the shape count on a
/// very wide window.
const MAX_STRIPS: usize = 1024;
/// Side of the live preview raster, in pixels.
const PREVIEW_PX: u32 = 288;
/// Width of the preview column, in points. Wide enough for the raster at 1:1
/// with room for the caption under it.
const PREVIEW_COLUMN_WIDTH: f32 = 336.0;

/// The editor's cross-frame state. Owned by the application beside the other
/// window states.
pub struct PaletteEditorState {
    pub open: bool,
    /// The table being edited. `None` before anything has been opened, which
    /// is also what the window shows on a fresh install.
    table: Option<EditorTable>,
    /// The state as last saved or loaded, for revert. `None` for a table that
    /// has never been written - a duplicate of a shipped preset, say, which
    /// has nothing to revert *to*.
    saved: Option<EditorTable>,
    /// Where [`Self::save`] writes. `None` until the first save, which picks a
    /// free filename from the name.
    file: Option<PathBuf>,
    /// The directory this editor reads existing palettes from and writes new
    /// ones into. [`store::user_colortables_dir`] in the application; the
    /// window tests point it at a scratch directory so driving the real window
    /// never touches the analyst's own palettes.
    directory: PathBuf,
    /// Which family the application installs this into. Follows the header's
    /// family combo.
    family: ColorTableFamily,
    selected: Option<StopId>,
    drag: Option<HandleDrag>,
    status: Option<Status>,
    /// The built table, cached against the editor state it was built from.
    /// Rebuilt on any change, which is what makes the strip and the preview
    /// agree with the file.
    built: Option<(EditorTable, ColorTable)>,
    build_error: Option<String>,
    preview: Preview,
}

impl Default for PaletteEditorState {
    fn default() -> Self {
        Self {
            open: false,
            table: None,
            saved: None,
            file: None,
            directory: store::user_colortables_dir(),
            // Written over by the first `edit_or_duplicate`; a closed editor
            // with no table has no family, and reflectivity is the one the
            // application opens on.
            family: ColorTableFamily::Reflectivity,
            selected: None,
            drag: None,
            status: None,
            built: None,
            build_error: None,
            preview: Preview::default(),
        }
    }
}

/// A footer line, and whether it reports a failure.
struct Status {
    text: String,
    failed: bool,
}

/// A strip handle mid-drag.
///
/// The value axis is frozen at the moment the drag starts. Recomputing it
/// every frame would move the axis under the pointer as soon as the dragged
/// stop became the new first or last one, and the handle would run away from
/// the finger holding it.
struct HandleDrag {
    stop: StopId,
    span_low: f32,
    span_high: f32,
}

#[derive(Default)]
struct Preview {
    texture: Option<egui::TextureHandle>,
    /// `(table signature, volume identity, cut)`. The signature moves whenever
    /// the table would paint differently, which is exactly when the raster is
    /// stale.
    key: Option<(u64, usize, usize)>,
    /// What the last attempt produced, when it produced no picture.
    note: Option<String>,
}

/// Everything the window needs for one frame.
pub struct PaletteEditorInput<'a> {
    pub state: &'a mut PaletteEditorState,
    /// The volume on screen, for the live preview. `None` is reported in the
    /// panel rather than left blank - an empty box reads as a broken preview,
    /// not as "nothing is loaded".
    pub volume: Option<&'a RadarVolume>,
}

/// What the analyst did this frame.
#[derive(Default)]
pub struct PaletteEditorOutcome {
    /// Install this table into this family in the live set, and repaint.
    pub install: Option<(ColorTableFamily, ColorTable)>,
    /// A file was written here.
    ///
    /// The file is picked up again by
    /// `crate::settings_ui::palettes::resolve_choice`, which looks a stored
    /// palette name up in the user colour tables directory when the shipped
    /// catalogue does not hold it - so a table saved here and applied is the
    /// table that comes back on the next launch.
    pub saved: Option<PathBuf>,
}

impl PaletteEditorState {
    /// Open on an installed table: copying it under a new name when
    /// `duplicate`, editing it in place otherwise.
    ///
    /// `duplicate` is **the caller's answer, not a guess made here**. The
    /// picker and the settings page both already know which palettes this
    /// build ships - `color_tables::is_builtin_table` is the one place that
    /// fact lives - and the editor re-deriving it from whether a file of a
    /// similar name happens to exist got it wrong in both directions: a
    /// palette this build does not ship opened as "… copy" because it had
    /// never been saved, and Copy on a shipped preset opened an analyst's
    /// unrelated file because the two names reduced to the same file stem.
    ///
    /// A table being edited in place is re-read **from its file** rather than
    /// from the installed `ColorTable`, because the file carries two things
    /// the installed table has already thrown away: the `Scale:` row, and
    /// which rows were ramp pairs. A table with no file - applied from the
    /// editor but never saved - keeps its own name and waits for a first save
    /// to pick a filename.
    pub fn edit_or_duplicate(
        &mut self,
        family: ColorTableFamily,
        table: &ColorTable,
        duplicate: bool,
    ) {
        let mut editable = EditorTable::from_color_table(family, table);
        // A shipped preset never adopts a file, whatever is in the directory.
        // The stem a name reduces to is many-to-one, so a file sitting at the
        // preset's path may hold something else entirely; opening it would
        // put a different table on screen than the row that was pressed, and
        // saving would overwrite it.
        //
        // The installed table's FULL name is tried before the base one, and
        // that order is the whole of it: a file whose `Name:` row ends in a
        // rendering suffix - which this editor now refuses to write but a hand
        // dropped palette can still carry - is only found under the name it
        // declares. Under the base name alone the file was missed, the editor
        // said "this table has no file yet", and the next Save wrote a second
        // file under the shortened name while the original was orphaned.
        let existing = (!duplicate)
            .then(|| {
                store::existing_file_in(&self.directory, table.name())
                    .or_else(|| store::existing_file_in(&self.directory, &editable.name))
            })
            .flatten();
        match (duplicate, existing) {
            (true, _) => {
                // Numbered against the names the directory already holds, so
                // Copy twice on one preset is two palettes and not two files
                // claiming one name.
                editable.name =
                    store::free_name_in(&self.directory, &format!("{} copy", editable.name));
                self.file = None;
                self.saved = None;
                self.status = Some(Status {
                    text: "Shipped presets are never overwritten, so this is a copy. Saving writes a new file.".to_owned(),
                    failed: false,
                });
            }
            (false, Some(path)) => match store::load(&path) {
                Ok(from_file) => {
                    editable = from_file;
                    // The caller's family as the fallback, exactly as
                    // `from_color_table` uses it on the other arm. A `.pal`
                    // with no `Product:` row - or one spelled with a token
                    // this build does not know - names no measurement, and
                    // reading that as Generic silently retargeted the Apply
                    // button: a table opened from the reflectivity row was
                    // installed into the catch-all family instead.
                    editable.family = editable
                        .product
                        .as_deref()
                        .and_then(family_from_product_token)
                        .unwrap_or(family);
                    self.file = Some(path);
                    self.saved = Some(editable.clone());
                    self.status = None;
                }
                Err(error) => {
                    // The file is there and unreadable. Edit what is installed
                    // and refuse to claim the file as the destination, so a
                    // save cannot silently overwrite something that might
                    // still be recoverable.
                    self.file = None;
                    self.saved = None;
                    self.status = Some(Status {
                        text: format!(
                            "{} could not be read ({error}); editing the installed copy as a new table",
                            path.display()
                        ),
                        failed: true,
                    });
                }
            },
            (false, None) => {
                self.file = None;
                self.saved = None;
                self.status = Some(Status {
                    text: format!(
                        "This table has no file yet. Save writes one into {}.",
                        self.directory.display()
                    ),
                    failed: false,
                });
            }
        }
        self.family = editable.family;
        self.selected = editable.stops().first().map(|stop| stop.id);
        self.drag = None;
        self.built = None;
        self.build_error = None;
        self.preview = Preview::default();
        self.table = Some(editable);
        self.open = true;
    }

    /// The table being edited. Read by the tests, which assert on the state
    /// the window produced rather than on where its widgets landed.
    #[allow(dead_code)]
    pub fn table(&self) -> Option<&EditorTable> {
        self.table.as_ref()
    }

    /// The table being edited, for a caller that is driving the editor rather
    /// than clicking it - the window photographs in
    /// `examples/palette_editor_proof.rs` set up a state to shoot this way.
    #[allow(dead_code)]
    pub fn table_mut(&mut self) -> Option<&mut EditorTable> {
        self.table.as_mut()
    }

    /// The file a save would write to, once one has been chosen. Read by the
    /// tests, for the same reason.
    #[allow(dead_code)]
    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    /// The footer line the analyst is being shown, and whether it reports a
    /// failure. Read by the tests, which assert on what the editor said as
    /// much as on what it did: a refusal that names the wrong cause is a
    /// refusal nobody can act on, and that is a defect whether or not the
    /// state behind it is right.
    #[allow(dead_code)]
    pub fn status(&self) -> Option<(&str, bool)> {
        self.status
            .as_ref()
            .map(|status| (status.text.as_str(), status.failed))
    }

    /// Point the editor at a different directory for reading and writing
    /// palettes.
    ///
    /// Exists so the window tests can drive the real window against a scratch
    /// directory. The application never calls it: the one directory the rest
    /// of the build scans is [`store::user_colortables_dir`], and an editor
    /// writing anywhere else would be writing where nothing looks.
    #[allow(dead_code)]
    pub fn set_directory(&mut self, directory: PathBuf) {
        self.directory = directory;
    }

    /// The live [`ColorTable`], rebuilt if the editor state has moved.
    ///
    /// The single crossing point: the strip, the preview and the Apply button
    /// all read this, so none of them can disagree about what the table paints.
    fn current_table(&mut self) -> Option<&ColorTable> {
        let table = self.table.as_ref()?;
        let stale = self
            .built
            .as_ref()
            .is_none_or(|(source, _)| source != table);
        if stale {
            match table.to_color_table() {
                Ok(built) => {
                    self.built = Some((table.clone(), built));
                    self.build_error = None;
                }
                Err(error) => {
                    self.built = None;
                    self.build_error = Some(error.to_string());
                }
            }
        }
        self.built.as_ref().map(|(_, built)| built)
    }

    /// Write the file, after [`store::save`]'s round-trip check passes.
    fn save(&mut self) -> Option<PathBuf> {
        // The name the file will carry, put back on screen before anything is
        // written. A save that left a trailing space in the field would leave
        // the editor showing a name the file does not hold, and the next open
        // of that file would read as an unexplained rename.
        if let Some(table) = self.table.as_mut() {
            let canonical = table.pal_name();
            table.name = canonical;
        }
        let table = self.table.as_ref()?;
        let path = match &self.file {
            Some(path) => path.clone(),
            None => store::free_path_in(&self.directory, &table.pal_name()),
        };
        match store::save(table, &path) {
            Ok(()) => {
                self.file = Some(path.clone());
                self.saved = Some(table.clone());
                self.status = Some(Status {
                    text: format!("Saved to {}", path.display()),
                    failed: false,
                });
                Some(path)
            }
            Err(error) => {
                self.status = Some(Status {
                    text: error.to_string(),
                    failed: true,
                });
                None
            }
        }
    }

    fn revert(&mut self) {
        let Some(saved) = self.saved.clone() else {
            return;
        };
        self.selected = saved.stops().first().map(|stop| stop.id);
        self.family = saved.family;
        self.table = Some(saved);
        self.drag = None;
        self.status = Some(Status {
            text: "Reverted to the saved file.".to_owned(),
            failed: false,
        });
    }

    /// Whether the table on screen differs from the one on disk.
    fn dirty(&self) -> bool {
        match (&self.table, &self.saved) {
            (Some(table), Some(saved)) => table != saved,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// Draw the window. Call every frame; cheap when closed.
pub fn draw_palette_editor(
    context: &egui::Context,
    input: PaletteEditorInput<'_>,
) -> PaletteEditorOutcome {
    let mut outcome = PaletteEditorOutcome::default();
    let PaletteEditorInput { state, volume } = input;
    if !state.open {
        return outcome;
    }
    let mut open = state.open;
    let screen = context.content_rect();
    let max_width = (screen.width() - 24.0).clamp(320.0, 1180.0);
    let max_height = (screen.height() - 48.0).max(320.0);
    egui::Window::new("Colour Table Editor")
        .open(&mut open)
        .default_size([880.0_f32.min(max_width), 700.0])
        .max_size([max_width, max_height])
        .resizable(true)
        .show(context, |ui| {
            ui.spacing_mut().interact_size.y =
                ui.spacing().interact_size.y.max(bevel::MIN_TOUCH_POINTS);
            if state.table.is_none() {
                ui.label(
                    "No colour table is open. Choose one in the product picker or on the \
                     Radar page of Settings and press Edit.",
                );
                return;
            }
            // The footer reserves its space FIRST, as a panel, so the file
            // path, the save result and the three buttons are on screen
            // whatever the body does. Before this the footer was the last
            // thing in the body's scroll area and a table with more than a
            // dozen stops pushed it off the bottom edge - visible in the
            // window photographs, not hypothesised.
            egui::Panel::bottom("palette-editor-footer").show_inside(ui, |ui| {
                footer(ui, state, &mut outcome);
            });
            egui::ScrollArea::vertical()
                .id_salt("palette-editor-body")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    header(ui, state);
                    ui.add_space(4.0);
                    strip_section(ui, state);
                    ui.add_space(4.0);
                    // Two columns bounded by `set_max_width` rather than by
                    // `allocate_ui`: allocating a zero-height rect to hold a
                    // column starves everything inside it of vertical space,
                    // which silently dropped the add-stop button and the
                    // preview's own caption off the bottom of both columns.
                    ui.horizontal_top(|ui| {
                        let total = ui.available_width();
                        let preview_width = PREVIEW_COLUMN_WIDTH.min(total * 0.45);
                        ui.vertical(|ui| {
                            ui.set_max_width((total - preview_width - 14.0).max(260.0));
                            stop_list(ui, state);
                        });
                        ui.vertical(|ui| {
                            ui.set_max_width(preview_width.max(160.0));
                            preview_section(ui, state, volume);
                        });
                    });
                });
        });
    state.open = open;
    outcome
}

// --- header ----------------------------------------------------------------

fn header(ui: &mut egui::Ui, state: &mut PaletteEditorState) {
    bevel::group_box(ui, "Table", |ui| {
        let Some(table) = state.table.as_mut() else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label("Name");
            let name = ui.add_sized(
                [240.0, bevel::MIN_TOUCH_POINTS],
                egui::TextEdit::singleline(&mut table.name)
                    .id(egui::Id::new("palette-editor-name"))
                    .hint_text("what this palette is called"),
            );
            // Canonicalised when the field is left, never while it is being
            // typed in - trimming under the cursor makes a space impossible to
            // type in the middle of a name. What the file can carry is what
            // stays on screen, so a trailing space cannot sit in the field
            // looking like part of the name it is not part of.
            if name.lost_focus() {
                table.name = table.pal_name();
            }
            ui.label("Measurement");
            let mut family = table.family;
            egui::ComboBox::from_id_salt("palette-editor-family")
                .selected_text(family.label())
                .width(230.0)
                .show_ui(ui, |ui| {
                    for candidate in ColorTableFamily::ALL {
                        ui.selectable_value(&mut family, candidate, candidate.label());
                    }
                });
            if family != table.family {
                table.family = family;
                // The `Product:` row follows the measurement, because the row
                // is how another tool - and this build's own reader - works
                // out which measurement the file is for.
                table.product = product_token(family).map(str::to_owned);
                state.family = family;
            }
        });
        // Said while the name is still on screen and a field edit away from
        // working, rather than only when Save refuses it. Both endings of the
        // same story: a name this build cannot carry writes a perfect file and
        // loses the palette at the next launch, so it is called out at the
        // field it is typed in.
        //
        // Same order as the refusals in `store::save`, so the warning on
        // screen and the footer after a Save press cannot name two different
        // problems with one name. The shipped-name question is asked of the
        // name's BASE form and so is the deeper of the two: taking the suffix
        // off "AWIPS Wilson REF (stepped)" leaves a name that still will not
        // save.
        let typed = table.pal_name();
        if let Some(family) = color_tables::builtin_family_for_name(&typed) {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "\"{}\" is a palette this build ships under {}. The shipped one wins \
                     that name, so this table will not save until it is changed.",
                    color_tables::base_name_of(&typed),
                    family.label()
                ),
            );
        } else if let Some(suffix) = color_tables::rendering_suffix(&typed) {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "A name ending in \"{}\" is reserved for the stepped and smooth \
                     drawings of a palette, and this one will not save until it is \
                     changed.",
                    suffix.trim_start()
                ),
            );
        }
        ui.horizontal(|ui| {
            ui.label("Units");
            let mut units = table.units;
            egui::ComboBox::from_id_salt("palette-editor-units")
                .selected_text(units.label())
                .width(110.0)
                .show_ui(ui, |ui| {
                    for candidate in EditorUnits::ALL {
                        ui.selectable_value(&mut units, candidate, candidate.label());
                    }
                });
            if units != table.units {
                table.set_units(units);
            }

            bevel::etched_separator(ui);
            let mut scaled = table.scale.is_some();
            let scale_response = ui.checkbox(&mut scaled, "Scale");
            if scale_response.changed() {
                table.set_scale(scaled.then_some(table.scale.unwrap_or(1.0)));
            }
            let mut scale = table.scale.unwrap_or(1.0);
            ui.add_enabled_ui(table.scale.is_some(), |ui| {
                if ui
                    .add(
                        egui::DragValue::new(&mut scale)
                            .speed(0.05)
                            .range(0.001..=1000.0),
                    )
                    .changed()
                {
                    table.set_scale(Some(scale));
                }
            });
        });
        ui.label(
            egui::RichText::new(
                "Units CONVERT: switching kt to m/s multiplies every stop by 0.514444, the \
                 factor the palette parser uses, so each colour stays on the wind speed it \
                 was on. Scale REINTERPRETS: the numbers stay where they are typed and the \
                 file declares that each one means 1/scale of an engine unit. A scale also \
                 overrides the unit entirely, which is why setting one freezes the values.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Sampling");
            let mut sampling = table.sampling;
            egui::ComboBox::from_id_salt("palette-editor-sampling")
                .selected_text(sampling.label())
                .width(190.0)
                .show_ui(ui, |ui| {
                    for candidate in Sampling::ALL {
                        ui.selectable_value(&mut sampling, candidate, candidate.label())
                            .on_hover_text(candidate.help());
                    }
                });
            table.sampling = sampling;

            bevel::etched_separator(ui);
            ui.add_enabled_ui(sampling.uses_step(), |ui| {
                let mut stepped = table.step.is_some();
                if ui
                    .checkbox(&mut stepped, "Step")
                    .on_hover_text(
                        "Band edges land on a grid of this size instead of on the stops - \
                         the GR `Step:` row. Only a stepped table has bands to place.",
                    )
                    .changed()
                {
                    table.step = stepped.then(|| table.step.unwrap_or(5.0));
                }
                let mut step = table.step.unwrap_or(5.0);
                ui.add_enabled_ui(table.step.is_some(), |ui| {
                    if ui
                        .add(
                            egui::DragValue::new(&mut step)
                                .speed(0.25)
                                .range(0.01..=1000.0)
                                .suffix(unit_suffix(table.units)),
                        )
                        .changed()
                    {
                        table.step = Some(step);
                    }
                });
            });
        });
        ui.label(egui::RichText::new(table.sampling.help()).small().weak());
    });
}

fn unit_suffix(units: EditorUnits) -> String {
    match units.pal_token() {
        Some(token) => format!(" {token}"),
        None => String::new(),
    }
}

// --- gradient strip --------------------------------------------------------

fn strip_section(ui: &mut egui::Ui, state: &mut PaletteEditorState) {
    bevel::group_box(ui, "Ramp", |ui| {
        // A table that will not build still gets its handle track. This used
        // to return here, and a table whose stops had all been typed onto one
        // value - which does not build - lost the handles at the same moment
        // it needed them: the only pointer-led way out of that state is to
        // drag two stops apart, and there was nothing left to drag.
        let built = state.current_table().cloned();
        let message = built.is_none().then(|| {
            state
                .build_error
                .clone()
                .unwrap_or_else(|| "this table cannot be sampled".to_owned())
        });
        // `strip_span`, not `display_span`: never zero-width, so the axis maps
        // handles to distinct pixels and pixels back to distinct values even
        // when every stop sits on the same number.
        let (low, high) = state
            .table
            .as_ref()
            .map(EditorTable::strip_span)
            .unwrap_or((0.0, 1.0));
        let factor = state
            .table
            .as_ref()
            .map(EditorTable::engine_factor)
            .unwrap_or(1.0);

        ui.horizontal_top(|ui| {
            let width = (ui.available_width() - 96.0).max(160.0);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(width, STRIP_HEIGHT + TRACK_HEIGHT),
                egui::Sense::hover(),
            );
            let strip = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), STRIP_HEIGHT));
            let track = egui::Rect::from_min_max(
                egui::pos2(rect.left(), strip.bottom()),
                egui::pos2(rect.right(), rect.bottom()),
            );
            match &built {
                Some(built) => paint_strip(ui, strip, built, low, high, factor),
                None => paint_empty_strip(ui, strip),
            }
            handles(ui, state, strip, track, low, high);

            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Range folded").small().weak());
                let Some(table) = state.table.as_mut() else {
                    return;
                };
                let mut color = to_color32(table.range_folded);
                if ui
                    .color_edit_button_srgba(&mut color)
                    .on_hover_text(
                        "The colour a gate gets when the radar reports it range folded - \
                         a category, not a value, so it sits beside the ramp rather than on it.",
                    )
                    .changed()
                {
                    table.range_folded = from_color32(color);
                }
            });
        });
        if let Some(message) = message {
            ui.colored_label(ui.visuals().error_fg_color, message);
        }
    });
}

/// The strip's well with no ramp in it, for a table that will not build.
///
/// Drawn rather than skipped so the handle track keeps the same geometry it
/// has when the table is valid: the handles are laid out against this rect,
/// and a strip that collapsed to nothing would move them.
fn paint_empty_strip(ui: &egui::Ui, strip: egui::Rect) {
    if !ui.is_rect_visible(strip) {
        return;
    }
    let palette = Palette::detect(ui);
    let painter = ui.painter();
    painter.rect_filled(strip, 0.0, palette.well);
    bevel::paint_bevel(painter, strip, Bevel::Sunken, palette);
}

/// Paint the checkerboard, the ramp and the sunken frame around them.
///
/// The ramp is sampled through the real table across the strip's whole width,
/// so the margins outside the value axis show the clamped end colours - which
/// is what the renderer paints for a gate outside the table's range, and
/// therefore what the strip should show there too.
fn paint_strip(
    ui: &egui::Ui,
    strip: egui::Rect,
    table: &ColorTable,
    low: f32,
    high: f32,
    factor: f32,
) {
    if !ui.is_rect_visible(strip) {
        return;
    }
    let palette = Palette::detect(ui);
    let painter = ui.painter();
    // Transparency has to be visible: a stop at alpha 0 over a solid ground
    // looks like a stop the colour of the ground. The SAME ground the preview
    // stands on - see `preview_ground` - because the strip and the preview are
    // two pictures of one table, and one alpha-0 stop reading two ways inside
    // one window is the two pictures disagreeing.
    let (dark, light) = preview_ground();
    paint_checkerboard(painter, strip, dark, light);

    let strips = (strip.width().ceil() as usize).clamp(2, MAX_STRIPS);
    let step = strip.width() / strips as f32;
    for index in 0..strips {
        let x = strip.left() + index as f32 * step;
        let value = value_at(x + step * 0.5, strip, low, high);
        let [red, green, blue, alpha] = table.color_for_value(value * factor);
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x, strip.top()),
                // Half a point of overlap so the columns do not show seams
                // when the width is not a whole number of them.
                egui::vec2(step + 0.5, strip.height()),
            ),
            0.0,
            egui::Color32::from_rgba_unmultiplied(red, green, blue, alpha),
        );
    }
    bevel::paint_bevel(painter, strip, Bevel::Sunken, palette);
}

/// The two-tone ground that makes transparency visible, over `rect`.
///
/// Shared by the gradient strip and the live preview, because both are
/// pictures of the same table and an alpha-0 stop has to read as absence in
/// both of them.
fn paint_checkerboard(
    painter: &egui::Painter,
    rect: egui::Rect,
    dark: egui::Color32,
    light: egui::Color32,
) {
    let columns = (rect.width() / CHECKER).ceil() as i32;
    let rows = (rect.height() / CHECKER).ceil() as i32;
    for row in 0..rows {
        for column in 0..columns {
            let square = egui::Rect::from_min_size(
                egui::pos2(
                    rect.left() + column as f32 * CHECKER,
                    rect.top() + row as f32 * CHECKER,
                ),
                egui::vec2(CHECKER, CHECKER),
            )
            .intersect(rect);
            let fill = if (row + column) % 2 == 0 { dark } else { light };
            painter.rect_filled(square, 0.0, fill);
        }
    }
}

/// The two greys transparency is shown against, in BOTH theme variants.
///
/// The night bench's own well and pressed face, and not the current variant's,
/// which is the point: in the light variant `palette.well` is near-paper
/// (250, 249, 245), and a palette's whitest bands - AWIPS Wilson's 60 dBZ stop
/// is pure white - were a hole in the echo rather than a core. An analyst
/// judging a colour table reads it against the dark sky the pane draws on, so
/// that is what the panel that claims to be "this palette, on the volume on
/// screen" has to put behind it.
///
/// One function for the gradient strip and the live preview both, because the
/// argument above is not about the preview: it is about a colour table, and
/// the strip is the control an analyst DRAGS a stop on. When only the preview
/// took it, the light variant showed one alpha-0 stop three ways in one
/// window: near-white checks under the strip, dark checks in the row's own
/// colour swatch, dark checks in the preview. Two pictures of one table
/// disagreeing about which of its stops were transparent.
pub(super) fn preview_ground() -> (egui::Color32, egui::Color32) {
    (
        crate::theme::palette::DARK.well,
        crate::theme::palette::DARK.face_pressed,
    )
}

/// The display value the value axis puts at `x`.
fn value_at(x: f32, strip: egui::Rect, low: f32, high: f32) -> f32 {
    let track_left = strip.left() + HANDLE_HALF;
    let track_width = (strip.width() - 2.0 * HANDLE_HALF).max(1.0);
    low + (x - track_left) / track_width * (high - low)
}

/// Where the value axis puts a display value.
fn x_at(value: f32, strip: egui::Rect, low: f32, high: f32) -> f32 {
    let track_left = strip.left() + HANDLE_HALF;
    let track_width = (strip.width() - 2.0 * HANDLE_HALF).max(1.0);
    let span = high - low;
    let fraction = if span.abs() <= f32::EPSILON {
        0.5
    } else {
        (value - low) / span
    };
    track_left + fraction.clamp(0.0, 1.0) * track_width
}

/// The draggable stop markers, and the readout that follows the one being
/// dragged.
fn handles(
    ui: &mut egui::Ui,
    state: &mut PaletteEditorState,
    strip: egui::Rect,
    track: egui::Rect,
    low: f32,
    high: f32,
) {
    let palette = Palette::detect(ui);
    let stops: Vec<(StopId, f32, Rgba8)> = state
        .table
        .as_ref()
        .map(|table| {
            table
                .stops()
                .iter()
                .map(|stop| (stop.id, stop.value, stop.color))
                .collect()
        })
        .unwrap_or_default();

    let mut readout: Option<(f32, f32)> = None;
    for (id, value, color) in stops {
        // The axis a drag reads is the one frozen when it started, so the
        // handle stays under the pointer even once this stop has become the
        // new end of the table and moved the axis.
        let (axis_low, axis_high) = match &state.drag {
            Some(drag) if drag.stop == id => (drag.span_low, drag.span_high),
            _ => (low, high),
        };
        let x = x_at(value, strip, axis_low, axis_high);
        let hit = egui::Rect::from_center_size(
            egui::pos2(x, track.center().y),
            egui::vec2(bevel::MIN_TOUCH_POINTS, bevel::MIN_TOUCH_POINTS),
        );
        let response = ui.interact(hit, handle_id(id), egui::Sense::click_and_drag());
        if response.drag_started() {
            state.selected = Some(id);
            state.drag = Some(HandleDrag {
                stop: id,
                span_low: low,
                span_high: high,
            });
        }
        if response.clicked() {
            state.selected = Some(id);
        }
        if response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some(table) = state.table.as_mut()
        {
            let moved = value_at(pointer.x, strip, axis_low, axis_high);
            table.set_value(id, moved);
            readout = Some((x_at(moved, strip, axis_low, axis_high), moved));
        }
        if response.drag_stopped() {
            state.drag = None;
        }

        if ui.is_rect_visible(hit) {
            let selected = state.selected == Some(id);
            let painter = ui.painter();
            let top = track.top() + 2.0;
            let points = vec![
                egui::pos2(x, top),
                egui::pos2(x - HANDLE_HALF, top + 11.0),
                egui::pos2(x + HANDLE_HALF, top + 11.0),
            ];
            let edge = if selected {
                palette.link
            } else {
                palette.border_strong
            };
            painter.add(egui::Shape::convex_polygon(
                points,
                to_color32(color),
                egui::Stroke::new(if selected { 2.0 } else { 1.0 }, edge),
            ));
        }
    }

    // The axis ends, so the strip is readable without dragging anything, and
    // the live value while a handle is moving.
    if ui.is_rect_visible(track) {
        let painter = ui.painter();
        let small = egui::FontId::proportional(10.0);
        painter.text(
            egui::pos2(strip.left() + HANDLE_HALF, track.bottom() - 1.0),
            egui::Align2::LEFT_BOTTOM,
            format_value(low),
            small.clone(),
            palette.text_weak,
        );
        painter.text(
            egui::pos2(strip.right() - HANDLE_HALF, track.bottom() - 1.0),
            egui::Align2::RIGHT_BOTTOM,
            format_value(high),
            small.clone(),
            palette.text_weak,
        );
        if let Some((x, value)) = readout {
            painter.text(
                egui::pos2(x, track.bottom() - 1.0),
                egui::Align2::CENTER_BOTTOM,
                format_value(value),
                small,
                palette.text,
            );
        }
    }
}

fn format_value(value: f32) -> String {
    if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else if value.abs() >= 1.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.3}")
    }
}

// --- stop list -------------------------------------------------------------

/// What each column of the stop list holds, left to right. One array, read by
/// the caption row and by every stop row.
const STOP_CAPTIONS: [&str; 7] = ["#", "Value", "Colour", "Pair", "Ramp to", "Add", "Cut"];

/// The width of each column, in points.
///
/// The floor is what the control in that column needs - 30 for the row number,
/// 92 for the value drag, `interact_size.x` for a colour swatch, one touch
/// target for the pair checkbox and the two buttons - widened where the
/// caption above it is wider than that, which is what keeps a caption on top
/// of the column it names instead of clipped or spilling into the next one.
///
/// Measured rather than guessed: the caption font is the theme's, and a build
/// with a larger UI scale or a different font would otherwise reopen exactly
/// the misalignment this replaces.
fn stop_columns(ui: &egui::Ui) -> [f32; 7] {
    let swatch = ui.spacing().interact_size.x.max(bevel::MIN_TOUCH_POINTS);
    let floors = [
        30.0,
        92.0,
        swatch,
        bevel::MIN_TOUCH_POINTS,
        swatch,
        bevel::MIN_TOUCH_POINTS,
        bevel::MIN_TOUCH_POINTS,
    ];
    let font = egui::TextStyle::Small.resolve(ui.style());
    floors
        .into_iter()
        .zip(STOP_CAPTIONS)
        .map(|(floor, caption)| {
            let caption_width = ui
                .painter()
                .layout_no_wrap(caption.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x;
            floor.max(caption_width + 2.0)
        })
        .collect::<Vec<f32>>()
        .try_into()
        .expect("one width per caption")
}

/// One cell of the stop list, exactly `width` wide.
///
/// `allocate_ui` alone states a MAXIMUM and lets the cursor advance by
/// whatever the content turned out to be, which is how the captions ended up
/// packed against the left edge while the rows under them were laid out from
/// fixed widths. `set_min_size` makes the cell take the width whether the
/// thing in it does or not, so the columns are columns.
fn column<R>(ui: &mut egui::Ui, width: f32, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let size = egui::vec2(width, bevel::MIN_TOUCH_POINTS);
    ui.allocate_ui_with_layout(
        size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(size);
            contents(ui)
        },
    )
    .inner
}

fn stop_list(ui: &mut egui::Ui, state: &mut PaletteEditorState) {
    bevel::group_box(ui, "Stops", |ui| {
        let (units, speed) = match state.table.as_ref() {
            Some(table) => (table.units, table.drag_speed()),
            None => return,
        };
        let suffix = unit_suffix(units);

        let mut insert_after: Option<StopId> = None;
        let mut remove: Option<StopId> = None;
        // A header, because seven controls in a row are unreadable without
        // one - and built from the SAME widths as the rows under it, through
        // `stop_columns`, so a caption cannot drift off the column it names.
        //
        // It used to be five `allocate_ui` calls with widths written out by
        // hand. `allocate_ui` states a MAXIMUM, so each caption shrank to its
        // own text width and they closed up to the left: in the window
        // photographs "Colour" sat over the value field, "Ramp to" over the
        // colour swatch, "Add / cut" over the ramp checkbox, and the + and x
        // buttons had no caption at all. A caption over the wrong control is
        // an instruction to press the wrong thing.
        let columns = stop_columns(ui);
        ui.horizontal(|ui| {
            for (width, caption) in columns.into_iter().zip(STOP_CAPTIONS) {
                column(ui, width, |ui| {
                    ui.label(egui::RichText::new(caption).small().weak());
                });
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("palette-editor-stops")
            .max_height(320.0)
            // Shrinks vertically so a four-stop table does not reserve the
            // full height, but never horizontally, so the rows keep one set
            // of column positions however wide the values happen to print.
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let Some(table) = state.table.as_mut() else {
                    return;
                };
                let removable = table.stops().len() > 2;
                let ids: Vec<StopId> = table.stops().iter().map(|stop| stop.id).collect();
                for (index, id) in ids.into_iter().enumerate() {
                    let Some(stop) = table.stop(id).copied() else {
                        continue;
                    };
                    let next_color = table
                        .stops()
                        .get(index + 1)
                        .map(|next| next.color)
                        .unwrap_or(stop.color);
                    ui.horizontal(|ui| {
                        // Every control sits in a cell of the width its
                        // caption was laid out against - `columns` is the same
                        // array the header row read - so the two cannot drift
                        // apart however wide a value happens to print.
                        //
                        // A fixed-width row number, inside that: `min_size`
                        // for the floor and the button's own padding cut right
                        // back so the text cannot push past it. `add_sized`
                        // was not enough - it gives a widget a box and lets it
                        // come out narrower OR wider, so a one-digit number
                        // produced a narrower cell than a two-digit one and
                        // every control to its right stepped sideways at row
                        // 10, which is visible in the window photographs.
                        let number = column(ui, columns[0], |ui| {
                            ui.spacing_mut().button_padding = egui::vec2(2.0, 2.0);
                            ui.add(
                                egui::Button::selectable(
                                    state.selected == Some(id),
                                    format!("{}", index + 1),
                                )
                                .min_size(egui::vec2(columns[0], bevel::MIN_TOUCH_POINTS)),
                            )
                        });
                        if number.clicked() {
                            state.selected = Some(id);
                        }
                        let mut value = stop.value;
                        let value_changed = column(ui, columns[1], |ui| {
                            ui.add_sized(
                                [columns[1], bevel::MIN_TOUCH_POINTS],
                                egui::DragValue::new(&mut value)
                                    .speed(speed)
                                    .suffix(suffix.clone()),
                            )
                            .changed()
                        });
                        if value_changed {
                            table.set_value(id, value);
                            state.selected = Some(id);
                        }
                        let mut color = to_color32(stop.color);
                        let color_changed = column(ui, columns[2], |ui| {
                            ui.color_edit_button_srgba(&mut color)
                                .on_hover_text("This stop's colour, with alpha")
                                .changed()
                        });
                        if color_changed && let Some(stop) = table.stop_mut(id) {
                            stop.color = from_color32(color);
                        }
                        let mut ramped = stop.ramp_end.is_some();
                        let ramp_toggled = column(ui, columns[3], |ui| {
                            // No glyph on the box: the column header names it,
                            // and an arrow here has to be a character the
                            // bundled fonts actually carry. U+2192 is not one
                            // of them - it photographed as a tofu square.
                            ui.checkbox(&mut ramped, "")
                                .on_hover_text(
                                    "Two-colour row: this stop ramps from its own colour to a \
                                     second one just before the next stop. The GR .pal ramp pair.",
                                )
                                .changed()
                        });
                        if ramp_toggled && let Some(target) = table.stop_mut(id) {
                            // Defaults to the next stop's colour, so switching
                            // a ramp on changes nothing a smooth table paints
                            // and turns a stepped band into the ramp it is
                            // being asked for.
                            target.ramp_end = ramped.then_some(next_color);
                        }
                        let end_changed = column(ui, columns[4], |ui| match stop.ramp_end {
                            Some(end_color) => {
                                let mut end = to_color32(end_color);
                                ui.color_edit_button_srgba(&mut end)
                                    .on_hover_text("The colour this row ramps to")
                                    .changed()
                                    .then_some(from_color32(end))
                            }
                            // A placeholder rather than a disabled swatch of
                            // the next stop's colour. A greyed-out control
                            // still shows a colour, and a row that is NOT a
                            // ramp pair showing a second colour reads as one
                            // - the exact thing an analyst must be able to
                            // tell apart at a glance down the column.
                            None => {
                                let (rect, _) = ui.allocate_exact_size(
                                    ui.spacing().interact_size,
                                    egui::Sense::hover(),
                                );
                                if ui.is_rect_visible(rect) {
                                    let palette = Palette::detect(ui);
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "-",
                                        egui::FontId::proportional(12.0),
                                        palette.text_weak,
                                    );
                                }
                                None
                            }
                        });
                        if let Some(end) = end_changed
                            && let Some(target) = table.stop_mut(id)
                        {
                            target.ramp_end = Some(end);
                        }
                        let add = column(ui, columns[5], |ui| {
                            ui.add_sized(
                                [bevel::MIN_TOUCH_POINTS, bevel::MIN_TOUCH_POINTS],
                                egui::Button::new("+"),
                            )
                            .on_hover_text("Insert a stop midway to the next one")
                            .clicked()
                        });
                        if add {
                            insert_after = Some(id);
                        }
                        let cut = column(ui, columns[6], |ui| {
                            ui.add_enabled_ui(removable, |ui| {
                                ui.add_sized(
                                    [bevel::MIN_TOUCH_POINTS, bevel::MIN_TOUCH_POINTS],
                                    egui::Button::new("×"),
                                )
                                .on_hover_text(if removable {
                                    "Remove this stop"
                                } else {
                                    "A colour table needs at least two stops"
                                })
                                .clicked()
                            })
                            .inner
                        });
                        if cut {
                            remove = Some(id);
                        }
                    });
                }
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .button("Add stop")
                .on_hover_text("Inserts midway between the selected stop and the next one")
                .clicked()
            {
                insert_after = state
                    .selected
                    .or_else(|| state.table.as_ref()?.stops().first().map(|stop| stop.id));
            }
            let Some(table) = state.table.as_ref() else {
                return;
            };
            let count = table.stops().len();
            // The engine span, whenever the numbers on screen are not already
            // engine values. A knots table is stored in metres per second and
            // an analyst checking a palette against a Nyquist velocity needs
            // to see which of the two they are reading.
            let line = if (table.engine_factor() - 1.0).abs() < f32::EPSILON {
                format!("{count} stops")
            } else {
                let (low, high) = table.display_span();
                format!(
                    "{count} stops · {} to {} in engine units",
                    format_value(table.to_engine(low)),
                    format_value(table.to_engine(high))
                )
            };
            ui.label(egui::RichText::new(line).small().weak());
        });

        if let Some(after) = insert_after
            && let Some(table) = state.table.as_mut()
            && let Some(new_id) = table.insert_after(after)
        {
            state.selected = Some(new_id);
        }
        if let Some(id) = remove
            && let Some(table) = state.table.as_mut()
            && table.remove(id)
            && state.selected == Some(id)
        {
            state.selected = table.stops().first().map(|stop| stop.id);
        }
    });
}

// --- live preview ----------------------------------------------------------

fn preview_section(
    ui: &mut egui::Ui,
    state: &mut PaletteEditorState,
    volume: Option<&RadarVolume>,
) {
    bevel::group_box(ui, "Preview", |ui| {
        let Some(built) = state.current_table().cloned() else {
            ui.label("Nothing to preview until the table has two stops.");
            return;
        };
        let moment = preview_moment(state.family);
        match (volume, moment) {
            (Some(volume), Some(moment)) => {
                update_preview(ui.ctx(), &mut state.preview, volume, &moment, &built);
            }
            (None, _) => {
                state.preview.texture = None;
                state.preview.key = None;
                state.preview.note = Some(
                    "No volume is loaded, so there is no echo to draw this on. The ramp above \
                     is sampled through the same table the pane would use."
                        .to_owned(),
                );
            }
            (Some(_), None) => {
                state.preview.texture = None;
                state.preview.key = None;
                state.preview.note = Some(
                    "This measurement has no radar moment of its own, so there is nothing to \
                     render. The ramp above is the whole preview."
                        .to_owned(),
                );
            }
        }

        if let Some(texture) = &state.preview.texture {
            let side = ui.available_width().clamp(160.0, 320.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
            let palette = Palette::detect(ui);
            let painter = ui.painter();
            // The same two-tone ground the gradient strip uses, and the same
            // one in both variants - see `preview_ground`. The raster is
            // mostly transparent (every gate below the table's first stop, and
            // everything off the sweep), so what is behind it is what the
            // echo's whitest bands are read against.
            let (dark, light) = preview_ground();
            paint_checkerboard(painter, rect, dark, light);
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            bevel::paint_bevel(painter, rect, Bevel::Sunken, palette);
        }
        if let Some(note) = &state.preview.note {
            ui.label(egui::RichText::new(note.as_str()).small().weak());
        }
    });
}

/// Which radar moment a family is read on. `None` for the catch-all, which
/// covers derived fields that have no moment of their own.
fn preview_moment(family: ColorTableFamily) -> Option<MomentType> {
    match family {
        ColorTableFamily::Reflectivity => Some(MomentType::Reflectivity),
        ColorTableFamily::Velocity => Some(MomentType::Velocity),
        ColorTableFamily::SpectrumWidth => Some(MomentType::SpectrumWidth),
        ColorTableFamily::DifferentialReflectivity => Some(MomentType::DifferentialReflectivity),
        ColorTableFamily::CorrelationCoefficient => Some(MomentType::CorrelationCoefficient),
        ColorTableFamily::DifferentialPhase => Some(MomentType::DifferentialPhase),
        ColorTableFamily::SpecificDifferentialPhase => Some(MomentType::SpecificDifferentialPhase),
        ColorTableFamily::Generic => None,
    }
}

/// The lowest cut that actually carries this moment.
///
/// Not cut zero: on a WSR-88D split cut the surveillance sweep carries
/// reflectivity and no velocity at all, so a preview pinned to the first cut
/// would show an empty box for every velocity palette.
fn first_cut_with(volume: &RadarVolume, moment: &MomentType) -> Option<usize> {
    volume
        .cuts
        .iter()
        .position(|cut| cut.moments.contains_key(moment))
}

fn update_preview(
    context: &egui::Context,
    preview: &mut Preview,
    volume: &RadarVolume,
    moment: &MomentType,
    table: &ColorTable,
) {
    let Some(cut_index) = first_cut_with(volume, moment) else {
        preview.texture = None;
        preview.key = None;
        preview.note = Some(format!(
            "The loaded volume carries no {} sweep, so there is nothing to draw this \
             palette on. The ramp above is sampled through the same table.",
            moment.short_name()
        ));
        return;
    };
    let key = (
        table.signature(),
        std::ptr::from_ref(volume) as usize,
        cut_index,
    );
    if preview.key == Some(key) && preview.texture.is_some() {
        return;
    }
    let options = render2d::RasterOptions {
        width: PREVIEW_PX,
        height: PREVIEW_PX,
        range_fraction: 94,
    };
    match render2d::render_moment_image_with_table(
        volume,
        cut_index,
        moment.clone(),
        options,
        Some(table),
    ) {
        Ok(image) => {
            let color_image = crate::app_support::color_image_from_rgba(
                image.width(),
                image.height(),
                image.as_raw(),
            );
            match &mut preview.texture {
                Some(texture) => texture.set(color_image, egui::TextureOptions::LINEAR),
                None => {
                    preview.texture = Some(context.load_texture(
                        "palette-editor-preview",
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            preview.key = Some(key);
            preview.note = Some(format!(
                "{} · cut {} · this palette, on the volume on screen",
                moment.short_name(),
                cut_index
            ));
        }
        Err(error) => {
            preview.texture = None;
            preview.key = None;
            preview.note = Some(format!("This volume cannot be drawn here: {error}"));
        }
    }
}

// --- footer ----------------------------------------------------------------

fn footer(ui: &mut egui::Ui, state: &mut PaletteEditorState, outcome: &mut PaletteEditorOutcome) {
    bevel::etched_separator(ui);
    ui.horizontal(|ui| {
        let can_build = state.current_table().is_some();
        ui.add_enabled_ui(can_build, |ui| {
            if ui
                .add_sized([96.0, bevel::MIN_TOUCH_POINTS], egui::Button::new("Save"))
                .on_hover_text(
                    "Writes the .pal into the user colour tables directory, but only after \
                     reading it back and checking it paints exactly what is on screen.",
                )
                .clicked()
                && let Some(path) = state.save()
            {
                outcome.saved = Some(path);
            }
        });
        ui.add_enabled_ui(state.saved.is_some() && state.dirty(), |ui| {
            if ui
                .add_sized(
                    [140.0, bevel::MIN_TOUCH_POINTS],
                    egui::Button::new("Revert to saved"),
                )
                .clicked()
            {
                state.revert();
            }
        });
        let family = state.family;
        ui.add_enabled_ui(can_build, |ui| {
            if ui
                .add_sized(
                    [190.0, bevel::MIN_TOUCH_POINTS],
                    egui::Button::new(format!("Apply to {}", family.label())),
                )
                .on_hover_text(
                    "Installs this table for the measurement family without saving it, so it \
                     can be judged on the live pane first.",
                )
                .clicked()
                && let Some(built) = state.current_table().cloned()
            {
                outcome.install = Some((family, built));
            }
        });
    });
    let dirty = state.dirty();
    let state_directory = state.directory.display().to_string();
    let line = match (&state.status, &state.file) {
        (Some(status), _) => status.text.clone(),
        (None, Some(path)) => format!("File: {}", path.display()),
        (None, None) => format!("Not saved yet; Save will write into {}", state_directory),
    };
    let failed = state.status.as_ref().is_some_and(|status| status.failed);
    let mut text = egui::RichText::new(if dirty {
        format!("{line} · unsaved changes")
    } else {
        line
    })
    .small();
    if failed {
        text = text.color(ui.visuals().error_fg_color);
    } else {
        text = text.weak();
    }
    ui.label(text);
    if let Some(error) = &state.build_error {
        ui.colored_label(ui.visuals().error_fg_color, error.as_str());
    }
}

// --- helpers ---------------------------------------------------------------

fn handle_id(stop: StopId) -> egui::Id {
    egui::Id::new(("palette-editor-handle", stop))
}

fn to_color32(color: Rgba8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

fn from_color32(color: egui::Color32) -> Rgba8 {
    let [red, green, blue, alpha] = color.to_srgba_unmultiplied();
    Rgba8::new(red, green, blue, alpha)
}
