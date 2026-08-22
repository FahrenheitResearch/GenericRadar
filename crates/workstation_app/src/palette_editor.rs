//! The colour table editor: a full editor for the GR `.pal` dialect, in a
//! window of its own.
//!
//! An analyst who cannot change a colour table is stuck with whatever twelve
//! reflectivity ramps this build happens to ship. The picker's list is a
//! menu; this is the kitchen. What it edits is the same `.pal` dialect
//! GR2Analyst and RadarScope read, written into
//! `settings::user_colortables_dir` - the directory
//! [`crate::settings_ui::palettes`] searches when it restores a stored palette
//! name the shipped catalogue does not hold - so a table made here is a file
//! an analyst owns rather than a row in a settings blob, and one that comes
//! back on the next launch.
//!
//! Four modules, split by what they know about:
//!
//! * [`model`] - what a table *is*: display-unit stops with stable ids, the
//!   header rows that decide what those numbers mean, and the two functions
//!   that cross to and from [`color_tables::ColorTable`]. No egui, no
//!   filesystem.
//! * [`pal`] - reading a `.pal` file back into that model, including the one
//!   thing a `ColorTable` cannot carry: the `Scale:` row, which the palette
//!   parser applies to every stop and then forgets.
//! * [`store`] - where files live, and the round-trip check a save has to pass
//!   before it is allowed to touch one.
//! * [`ui`] - the window. Thin by design: it decides where a handle is, never
//!   what a colour table means.
//!
//! # Shipped presets are never overwritten
//!
//! Shared-family rows enter through
//! [`ui::PaletteEditorState::edit_or_duplicate`], and it is **told** which
//! case it is in rather than working it out: the caller asks
//! `color_tables::is_builtin_table`, which is where the catalogue lives. A
//! built-in is duplicated under a new name and claims no file at all; anything
//! else is edited in place, in the file whose `Name:` row matches it. There is
//! no code path here that writes over a preset, which is why the picker can
//! offer the button on every row.
//!
//! The editor used to re-derive the answer from whether a file of a similar
//! name existed, and a filename is not an identity: a palette that had never
//! been saved opened as a copy of itself, and Copy on a preset opened an
//! unrelated file whose name reduced to the same stem.
//!
//! Producer-native fields enter through
//! [`ui::PaletteEditorState::edit_source_field`]. That path locks measurement
//! and unit conversion and returns a source-specific Apply outcome, so an
//! exact field can use this editor without installing its table into the
//! shared Generic family.

pub mod model;
pub mod pal;
pub mod store;
pub mod ui;

pub use ui::{PaletteEditorInput, PaletteEditorState, draw_palette_editor};

#[cfg(test)]
mod tests;
