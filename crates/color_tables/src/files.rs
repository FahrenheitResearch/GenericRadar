//! Which file in a directory holds which palette.
//!
//! One function, [`palette_named_in`], and it is deliberately here rather than
//! in either of the two places that ask the question. The colour table editor
//! asks it to find the file a save must overwrite; the settings restore path
//! asks it to put an analyst's own palette back on the pane at launch. Those
//! two are compiled into different crates - the settings window is also built
//! by `settings/tests/workstation_settings_ui.rs`, which cannot see the
//! workstation crate at all - so each of them used to carry its own search,
//! and the two searches did not agree:
//!
//! * the editor tried the filename a name reduces to first and only walked
//!   the directory when that file turned out to name something else;
//! * the restore path always walked the sorted directory.
//!
//! Both were internally deterministic, and on a directory holding two files
//! that declare one name they resolved to *different* files: Edit opened one
//! palette and the next launch installed the other. Determinism per function
//! is not the property that matters. The property that matters is that every
//! part of the application resolves a palette name to the same file, which is
//! what one function gives and two cannot.
//!
//! # Identity
//!
//! A palette is identified by the `Name:` row **inside** the file (see
//! [`crate::declared_name`]), falling back to the file stem for a
//! hand-dropped GR `.pal`, which never carries the row. Never by the filename
//! a name would produce: that mapping is lossy and many-to-one, and a search
//! that trusted it would hand back a different palette than the one asked for.
//!
//! A file that does not parse as a colour table answers for nothing. A
//! half-written `.pal` costs the analyst the one palette it holds rather than
//! shadowing a good file that declares the same name.
//!
//! # The name policy
//!
//! The other half of identity is which names an analyst's own palette may go
//! by at all, and it is [`user_palette_name_fault`], here, for the same
//! reason: two features ask it. The colour table editor asks before it writes
//! a file, and refuses the save. The folder scanner asks about a file that is
//! already there, and renames the row it offers with
//! [`crate::user::USER_NAME_SUFFIX`]. One rule, two answers - a refusal where
//! there is still somebody to tell, a rename where there is not - and they
//! have to be the same rule or the editor writes files the scanner then
//! renames out from under the name the settings file stored.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::user::USER_TABLE_EXTENSIONS;
use crate::{ColorTable, ColorTableFamily, declared_name};

/// The extension every colour table file in the directory carries.
pub const PALETTE_EXTENSION: &str = "pal";

/// A palette file that answered to a name: where it is, and what it paints.
///
/// Both halves come back together because both callers need one of them and
/// re-reading the file to get the other is how the two searches drifted apart
/// in the first place.
#[derive(Clone, Debug)]
pub struct PaletteFile {
    pub path: PathBuf,
    pub table: ColorTable,
}

/// The file in `directory` that holds the palette called `name`, if one does.
///
/// The directory is walked in sorted order and the FIRST file that answers to
/// the name wins, so a directory that has somehow ended up with two files
/// declaring one name resolves to the same one on every platform, every run
/// and every caller. (The editor refuses to *create* that situation - see the
/// name check in its store - but a directory an analyst has copied palettes
/// into by hand can already be in it.)
///
/// `name` is compared exactly, after the same trimming a `Name:` row gets when
/// it is read back.
pub fn palette_named_in(directory: &Path, name: &str) -> Option<PaletteFile> {
    let wanted = name.trim();
    if wanted.is_empty() {
        return None;
    }
    for path in palette_paths_in(directory) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if palette_identity(&path, &text).as_deref() != Some(wanted) {
            continue;
        }
        // Parsed with the identity as the table's name, so the table that
        // comes back out of the search is named the thing that was searched
        // for whether the file declared it or the stem supplied it.
        if let Ok(table) = ColorTable::parse(wanted, &text) {
            return Some(PaletteFile { path, table });
        }
    }
    None
}

/// Every palette name the files in `directory` declare, in sorted order.
///
/// What "taken" means when the editor has to pick a name nothing else in the
/// directory answers to. Unreadable files are skipped, exactly as
/// [`palette_named_in`] skips them, so a name is reported taken only when a
/// file that really would answer to it holds it.
pub fn palette_names_in(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = palette_paths_in(directory)
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let identity = palette_identity(&path, &text)?;
            ColorTable::parse(&identity, &text).ok()?;
            Some(identity)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The colour table files in `directory`, in the folder scanner's order.
/// Empty for a directory that is not there yet, which is the ordinary state
/// before the first save.
///
/// Every extension [`crate::user::USER_TABLE_EXTENSIONS`] admits, not just the
/// one the editor writes: the folder holds ONE set of colour tables, and a
/// `.txt` palette an analyst was mailed is as much a member of it as a `.pal`.
/// A search that skipped it would let a save land a second file on a name that
/// `.txt` already answers to in the picker - which is the ambiguity this
/// module exists to prevent, arriving by way of a file extension.
///
/// The order is [`compare_file_names`], which is the scanner's order too, so
/// "the first file that answers to this name" means the same file to both.
fn palette_paths_in(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_palette_extension(path))
        .collect();
    paths.sort_by(|left, right| {
        compare_file_names(&file_name_of(left), &file_name_of(right)).then_with(|| left.cmp(right))
    });
    paths
}

/// Whether a path's extension is one the colour table folder reads.
fn is_palette_extension(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| USER_TABLE_EXTENSIONS.contains(&extension.as_str()))
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The order the colour table folder is read in, here and in
/// `crate::user`.
///
/// Case-insensitive first, so a folder reads the way a file manager shows it,
/// with the raw bytes as the tie-break so the order is total and does not
/// reshuffle between launches. Shared rather than written twice because the
/// two readings both resolve a name by taking the FIRST file that answers to
/// it, and two orders would take two files.
pub(crate) fn compare_file_names(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

/// The name the palette in this file goes by: its `Name:` row, or its file
/// stem when it has none.
///
/// The one identity rule. `crate::user`'s folder scanner reads it for the row
/// it offers in a picker, and this module reads it for the file a name
/// resolves to; a scanner that named a file by its stem while the search named
/// it by its `Name:` row would offer a row that nothing could then install.
pub(crate) fn palette_identity(path: &Path, text: &str) -> Option<String> {
    declared_name(text).or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
    })
}

/// Why a name cannot be carried as an analyst's own colour table's name, if it
/// cannot.
///
/// `family` is the picker list the name would appear in, and it decides how
/// wide the shipped-name question is asked:
///
/// * `None` - "does this build ship ANY palette under this name" - is what a
///   *writer* asks. The editor asks it, because a file it writes may be read
///   back into a family the writer did not choose (the family comes from the
///   `Product:` header) and because a refusal is cheap while there is still
///   somebody on screen to tell;
/// * `Some(family)` - "does this build ship one under this name HERE" - is
///   what the folder scanner asks about a file that already exists. Its
///   concern is one picker list showing one name twice, and a reflectivity
///   preset's name on a velocity table never puts two rows in one list.
///
/// A rendering suffix is refused for every family alike: `ColorTable::rendered`
/// rewrites that half of a name whenever the smooth/stepped switch is flipped,
/// and `base_name()` - the half stored as the installed palette's identity -
/// is the half without it, so such a name cannot survive a restart in any
/// family. See [`crate::rendering_suffix`].
pub fn user_palette_name_fault(
    name: &str,
    family: Option<ColorTableFamily>,
) -> Option<UserNameFault> {
    // Shipped names first, deliberately: the question is asked of the name's
    // BASE form and is therefore the deeper answer for a name that trips both.
    // Taking " (stepped)" off "AWIPS Wilson REF (stepped)" leaves a name that
    // still cannot be used, and being told about the suffix first is being
    // told the wrong thing first.
    let shipped = match family {
        Some(family) => crate::builtin_family_ships_name(family, name).then_some(family),
        None => crate::builtin_family_for_name(name),
    };
    if let Some(family) = shipped {
        return Some(UserNameFault::ShippedName {
            base: crate::base_name_of(name).to_owned(),
            family,
        });
    }
    crate::rendering_suffix(name).map(UserNameFault::RenderingSuffix)
}

/// What [`user_palette_name_fault`] found wrong with a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserNameFault {
    /// The name, or its base form, is one this build already ships a palette
    /// under. The family named is the one that ships it, which for a writer's
    /// question may not be the family the table itself is for.
    ShippedName {
        base: String,
        family: ColorTableFamily,
    },
    /// The name ends in one of the suffixes this build appends to spell the
    /// two drawings of one palette.
    RenderingSuffix(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "color-tables-files-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn pal(name: Option<&str>, red: u8) -> String {
        let mut text = String::new();
        if let Some(name) = name {
            text.push_str(&format!("Name: {name}\n"));
        }
        text.push_str(&format!(
            "Product: BR\nMode: smooth\nColor4: 0 {red} 0 0 255\nColor4: 60 200 210 220 255\n"
        ));
        text
    }

    #[test]
    fn a_file_answers_to_the_name_row_inside_it_and_not_to_its_filename() {
        let dir = scratch_dir("identity");
        std::fs::write(
            dir.join("storm-detail-v2.pal"),
            pal(Some("Storm: Detail / v2"), 7),
        )
        .expect("write");
        assert!(palette_named_in(&dir, "Storm Detail v2").is_none());
        let found = palette_named_in(&dir, "Storm: Detail / v2").expect("found by its Name row");
        assert_eq!(found.path, dir.join("storm-detail-v2.pal"));
        assert_eq!(found.table.name(), "Storm: Detail / v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand-dropped GR palette carries no `Name:` row, and its file stem is
    /// the only name it has.
    #[test]
    fn a_file_with_no_name_row_answers_to_its_stem() {
        let dir = scratch_dir("stem");
        std::fs::write(dir.join("field-ref.pal"), pal(None, 9)).expect("write");
        let found = palette_named_in(&dir, "field-ref").expect("found by its stem");
        assert_eq!(found.path, dir.join("field-ref.pal"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason this function exists: two files declaring one name
    /// resolve to one file, and every caller gets that same file.
    #[test]
    fn two_files_with_one_name_resolve_to_the_same_file_every_time() {
        let dir = scratch_dir("duplicate");
        std::fs::write(dir.join("alpha.pal"), pal(Some("Bravo"), 1)).expect("write");
        std::fs::write(dir.join("bravo.pal"), pal(Some("Bravo"), 2)).expect("write");
        let first = palette_named_in(&dir, "Bravo").expect("one of them answers");
        assert_eq!(first.path, dir.join("alpha.pal"), "sorted order decides");
        for _ in 0..5 {
            let again = palette_named_in(&dir, "Bravo").expect("still answers");
            assert_eq!(again.path, first.path);
            assert_eq!(again.table.stops()[0].color, first.table.stops()[0].color);
        }
        assert_eq!(palette_names_in(&dir), vec!["Bravo".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A half-written file does not answer for a name a good file holds, and
    /// does not stop the search either. This runs at launch, where a directory
    /// that gave up at the first bad file would blank a pane.
    #[test]
    fn an_unreadable_file_answers_for_nothing_and_stops_nothing() {
        let dir = scratch_dir("unreadable");
        // Sorts first, so a search that stopped at it would never reach the
        // real one.
        std::fs::write(dir.join("aa-broken.pal"), "Name: Field VEL\nColor4: 5 1\n").expect("write");
        std::fs::write(dir.join("zz-field.pal"), pal(Some("Field VEL"), 3)).expect("write");
        let found = palette_named_in(&dir, "Field VEL").expect("the good file answers");
        assert_eq!(found.path, dir.join("zz-field.pal"));
        assert_eq!(palette_names_in(&dir), vec!["Field VEL".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_does_not_exist_is_empty_rather_than_an_error() {
        let dir = std::env::temp_dir().join("color-tables-files-nowhere-at-all");
        assert!(palette_named_in(&dir, "Anything").is_none());
        assert!(palette_names_in(&dir).is_empty());
    }
}
