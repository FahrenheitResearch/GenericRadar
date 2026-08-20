//! Where an edited colour table lands on disk, and the check it has to pass
//! first.
//!
//! One directory: `<the settings root>/colortables`, beside `settings.json`,
//! named by `settings::user_colortables_dir` so it follows the platform (and
//! follows a shell that injected its own root on iOS or Android) rather than
//! being spelled out again here. That directory is the contract with the rest
//! of the application: `crate::settings_ui::palettes` reads it when it
//! restores a stored palette name the shipped catalogue does not hold, which
//! is what makes a table saved here survive a restart.
//!
//! Nothing here identifies a palette by its filename. [`file_stem_for`] is
//! many-to-one by design, so the search opens what it finds and reads the
//! `Name:` row before believing it - a save that trusted the filename would
//! overwrite whichever palette happened to reduce to the same stem.
//!
//! The search itself is not written here. It is
//! `color_tables::palette_named_in`, the one function both this module and the
//! settings restore path call, because the two used to carry a search each and
//! resolved a directory holding two files of one name to two different files:
//! Edit opened one palette and the next launch installed the other. The other
//! half of that job is here - [`save`] refuses to CREATE a second file
//! declaring a name another file in the directory already declares, so the
//! ambiguity the shared search resolves is one an analyst cannot walk into.
//!
//! Saving is refused unless the file would read back as the table on screen.
//! This is not defensive noise - the writer and the reader are separate code,
//! the dialect has a header that changes what every number means, and a
//! palette that quietly loses its `Scale:` row is a palette that draws 30 m/s
//! where it drew 30 kt. [`save`] writes the text, reads it back, rebuilds the
//! `ColorTable` from both, and only then touches the destination.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::model::{EditorTable, RoundTripError, one_line};
use super::pal;

/// The extension every table in the directory carries.
///
/// The shared search's constant rather than a second spelling of it: the two
/// have to agree or the editor would write files the search does not look at.
pub const EXTENSION: &str = color_tables::PALETTE_EXTENSION;

/// The longest file stem a name is allowed to produce, before the collision
/// suffix. Well inside every filesystem's limit and short enough to stay
/// readable in a file dialog.
const MAX_STEM: usize = 64;

/// How much of [`MAX_STEM`] a truncated name keeps, leaving room for a hyphen
/// and the eight hex digits of [`name_digest`].
const TRUNCATED_STEM: usize = MAX_STEM - 9;

/// The directory user colour tables live in.
///
/// Resolved by `settings`, which owns every path the application uses, so the
/// editor and the palette restore path in `crate::settings_ui::palettes` -
/// which is compiled into a crate that cannot see this module - name the same
/// directory rather than two spellings of it.
pub fn user_colortables_dir() -> PathBuf {
    settings::user_colortables_dir()
}

/// A file stem for a palette name: lower case, ASCII alphanumerics and single
/// hyphens, never empty.
///
/// Deliberately lossy and deliberately not reversible. The name of record is
/// the `Name:` row **inside** the file, and nothing in this module identifies
/// a palette by its filename: [`existing_file_in`] opens what it finds and
/// reads the row before believing it. That is what makes the lossiness safe,
/// because the mapping is many-to-one - `Storm: Detail / v2` and
/// `Storm Detail v2` both reduce to `storm-detail-v2` - and a filename that
/// tried to carry an arbitrary palette name would be a filename with a colon
/// in it on Windows.
///
/// A name long enough to be cut at [`MAX_STEM`] takes a digest of the whole
/// name instead of just its head, so two palettes sharing a sixty-four
/// character prefix do not reduce to the same stem and then have to be told
/// apart by the `-2` suffix search.
pub fn file_stem_for(name: &str) -> String {
    let mut stem = String::with_capacity(name.len());
    let mut pending_separator = false;
    let mut truncated = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !stem.is_empty() {
                stem.push('-');
            }
            pending_separator = false;
            stem.extend(character.to_lowercase());
            if stem.len() >= TRUNCATED_STEM {
                truncated = true;
                break;
            }
        } else {
            pending_separator = true;
        }
    }
    if stem.is_empty() {
        return "palette".to_owned();
    }
    if truncated {
        stem.push('-');
        stem.push_str(&format!("{:08x}", name_digest(name)));
    }
    stem
}

/// FNV-1a over the name's bytes, for the truncation suffix.
///
/// Written out rather than taken from `std::hash`: `DefaultHasher`'s output is
/// explicitly not stable across Rust releases, and a filename that moved when
/// the toolchain moved would orphan every long-named palette on disk. FNV-1a
/// (Fowler/Noll/Vo, 1991) is not a cryptographic hash and does not need to be:
/// it only has to separate two names that share a prefix, and a collision costs
/// a `-2` suffix rather than a lost file.
fn name_digest(name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Where a palette of this name would be written in `directory`, ignoring
/// collisions.
pub fn path_in(directory: &Path, name: &str) -> PathBuf {
    directory.join(format!("{}.{EXTENSION}", file_stem_for(name)))
}

/// Where a NEW palette of this name should be written: [`path_in`], or the
/// first free `-2`, `-3` … beside it.
///
/// Used only when the editor has no file of its own yet. Editing a table that
/// came from a file overwrites that file, so a rename does not scatter copies.
pub fn free_path_in(directory: &Path, name: &str) -> PathBuf {
    let stem = file_stem_for(name);
    let first = path_in(directory, name);
    if !first.exists() {
        return first;
    }
    for suffix in 2..1000u32 {
        let candidate = directory.join(format!("{stem}-{suffix}.{EXTENSION}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// The file in `directory` that holds the palette of this name, if one does.
///
/// This is how the editor finds the file behind a table it has been asked to
/// edit in place, and `Some` means "this is the file a save must overwrite",
/// so the answer is the path rather than a boolean.
///
/// One line, because the search is `color_tables::palette_named_in` and this
/// is that function under the editor's own name for it. It has to be one
/// function and not a second one written here: the settings restore path
/// resolves the same question at launch, from a crate that cannot see this
/// module, and two searches that each pick a file out of a directory holding
/// two files of one name pick two different files. Every candidate is opened
/// and its `Name:` row read before it is believed, which is what makes the
/// lossy filename mapping safe.
pub fn existing_file_in(directory: &Path, name: &str) -> Option<PathBuf> {
    color_tables::palette_named_in(directory, &one_line(name)).map(|found| found.path)
}

/// A name for a new palette that nothing in `directory` already answers to:
/// `wanted`, then `wanted 2`, `wanted 3` …
///
/// Copy used to append " copy" and stop there, so pressing Copy twice on one
/// preset wrote two files whose `Name:` rows held the same string. The two
/// then differed only in a filename that nothing identifies a palette by: Edit
/// on the second row opened the first row's file, and saving from there
/// overwrote a palette the analyst could no longer reach from the UI.
///
/// Numbered against the NAMES the directory declares and not against the
/// filenames it holds, because the name is what a palette is found by
/// everywhere else.
pub fn free_name_in(directory: &Path, wanted: &str) -> String {
    let wanted = one_line(wanted);
    let taken = color_tables::palette_names_in(directory);
    if !taken.contains(&wanted) {
        return wanted;
    }
    for suffix in 2..1000u32 {
        let candidate = format!("{wanted} {suffix}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    wanted
}

/// Read one file back into editable state.
pub fn load(path: &Path) -> Result<EditorTable, LoadError> {
    let text = fs::read_to_string(path).map_err(LoadError::Io)?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("palette");
    pal::from_pal_text(stem, &text).ok_or(LoadError::NotATable)
}

/// Write the table to `path`, but only after proving the file reads back as
/// the same table.
///
/// The seven things checked, in the order they can fail:
///
/// 1. the table has a name, because the `Name:` row is what the file is found
///    by afterwards and a nameless palette is one the analyst cannot get back;
/// 2. the name is not a shipped palette's - see [`SaveError::ShippedName`];
/// 3. the name does not end in a rendering suffix - see
///    [`SaveError::RenderingSuffix`];
/// 4. no OTHER file in the same directory declares that name, because a name
///    two files answer to is a name the application resolves to one of them
///    and the analyst reaches the other;
/// 5. the table is a colour table at all (two distinct stops, finite values);
/// 6. the text parses back into editor state whose own text is byte-identical,
///    which is what catches a header the writer emits and the reader ignores;
/// 7. the `ColorTable` built from the re-read state equals the one built from
///    the table on screen - same stops **including ramp-pair end colours**,
///    same sampling, same units, same range-folded colour.
///
/// Checks 2, 3 and 4 are one idea seen three ways: the set of names this build
/// cannot carry end to end is the shipped catalogue's names, plus every name
/// that ends in a rendering suffix, plus whatever the directory already holds.
/// A save that landed on any of them would write a perfect file and lose the
/// palette anyway, so each is a refusal said in words while the table is still
/// on screen and one field edit from a name that works.
///
/// Only then is anything written, and it is written to a temporary file and
/// renamed over the destination, so a failure part-way leaves the previous
/// version of the palette intact.
pub fn save(table: &EditorTable, path: &Path) -> Result<(), SaveError> {
    let name = table.pal_name();
    if name.is_empty() {
        return Err(SaveError::NoName);
    }
    // `color_tables::user_palette_name_fault`, which is the one place the rule
    // is written and is the same function the colour table folder scanner asks
    // about a file that is already there - it renames the row it offers where
    // this refuses the save. Asked with no family, the writer's form of the
    // question: the family a file comes back in is read from its `Product:`
    // header, so a name that is safe in this table's family today is a shipped
    // name the moment the analyst changes what the table measures.
    //
    // Shipped names come back before rendering suffixes, and that order is the
    // function's: taking " (stepped)" off "AWIPS Wilson REF (stepped)" leaves
    // a name that still cannot be saved, and being told about the suffix first
    // is being told the wrong thing first.
    match color_tables::user_palette_name_fault(&name, None) {
        Some(color_tables::UserNameFault::ShippedName { base, family }) => {
            return Err(SaveError::ShippedName { base, family });
        }
        Some(color_tables::UserNameFault::RenderingSuffix(suffix)) => {
            return Err(SaveError::RenderingSuffix(suffix));
        }
        None => {}
    }
    // Against the directory this file is going into, and skipping the file
    // itself so that saving a palette a second time is not a name collision
    // with its own previous version.
    if let Some(directory) = path.parent()
        && let Some(other) = color_tables::palette_named_in(directory, &name)
        && other.path != *path
    {
        return Err(SaveError::NameTaken(other.path));
    }
    let expected = table
        .to_color_table()
        .map_err(|error| SaveError::RoundTrip(RoundTripError::NotATable(error)))?;
    let text = table.pal_text();
    // The canonical name as the fallback, not the raw field: the fallback is
    // only reached when the `Name:` row is empty, and reading back a different
    // name than the one that was written is the whole of the bug this check
    // exists to catch.
    let reread = pal::from_pal_text(&name, &text).ok_or(SaveError::RoundTrip(
        RoundTripError::Mismatch("the saved stops did not read back as a colour table"),
    ))?;
    if reread.pal_text() != text {
        return Err(SaveError::RoundTrip(RoundTripError::Mismatch(
            "a header row did not survive being read back",
        )));
    }
    let actual = reread
        .to_color_table()
        .map_err(|error| SaveError::RoundTrip(RoundTripError::NotATable(error)))?;
    if actual != expected {
        return Err(SaveError::RoundTrip(RoundTripError::Mismatch(
            "the colours the re-read file paints are not the colours on screen",
        )));
    }

    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(SaveError::Io)?;
    }
    let temporary = path.with_extension("pal.writing");
    fs::write(&temporary, text.as_bytes()).map_err(SaveError::Io)?;
    // `rename` replaces the destination on every platform this ships to
    // (Windows included - std uses MOVEFILE_REPLACE_EXISTING), so the previous
    // palette is never a truncated file even for an instant.
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(SaveError::Io(error))
        }
    }
}

#[derive(Debug)]
pub enum SaveError {
    /// The Name field is empty, or holds nothing but spaces. Its own variant
    /// rather than a round-trip mismatch because the two read completely
    /// differently on the footer: one says to look at the name, the other
    /// sends the analyst hunting through the colours.
    NoName,
    /// The name ends in one of the four suffixes this build appends to spell
    /// the two drawings of one palette - " (stepped)", " (continuous)",
    /// " (interpolated)", " (quantized stepped)".
    ///
    /// Refused rather than written, and refused at the door rather than
    /// half-supported, because such a name cannot be carried end to end and
    /// the failure is otherwise silent and total. The file itself would be
    /// perfect; what breaks is everything around it. What the application
    /// stores as the installed palette's identity is `base_name()`, the half
    /// WITHOUT the suffix, so at the next launch it looks for a palette called
    /// "Storm" while the file declares "Storm (stepped)", finds nothing, and
    /// installs the shipped default - the evening's work gone with no message.
    /// Reopening it renames it for the same reason. And even if the lookup
    /// were taught both halves, `ColorTable::rendered` REWRITES the suffix
    /// when the analyst flips the smooth/stepped switch, so the name would
    /// still be destroyed by an unrelated control.
    ///
    /// So the analyst is told, in the footer, while the palette is still on
    /// screen and one field edit away from a name that works.
    RenderingSuffix(&'static str),
    /// The name, or its base form, is one this build already ships a palette
    /// under - in this family or in another.
    ///
    /// The same failure as [`Self::RenderingSuffix`] arriving by a different
    /// road, and refused for the same reason: the file would be written
    /// perfectly and the palette would be gone at the next launch with nothing
    /// said. The restore path searches the shipped catalogue BEFORE the
    /// analyst's own directory - deliberately, so a stray file cannot quietly
    /// replace a palette the rest of the build documents by name - so a file
    /// declaring a shipped name is never what gets installed. And the picker
    /// row for that name offers Edit on the *preset*, because that is what
    /// `color_tables::is_builtin_table` says the name is, so the analyst's own
    /// table cannot be reopened either: it is reachable only through the
    /// filesystem.
    ///
    /// Reachable in two keystrokes, which is why it is worth a refusal rather
    /// than a note in the documentation. Copy on a preset row pre-fills the
    /// preset's name with " copy" on the end, and deleting those five
    /// characters is the shortest edit there is.
    ShippedName {
        /// The name with any rendering suffix taken off - the half that
        /// collided, and the half to print.
        base: String,
        /// Which measurement ships it. Worth naming because the collision is
        /// checked across every family, so the palette being clashed with may
        /// not be one this table's own picker lists.
        family: color_tables::ColorTableFamily,
    },
    /// Another file in the same directory already declares this name.
    ///
    /// A palette is found by its `Name:` row everywhere in this build, so two
    /// files answering to one name means the application resolves it to one of
    /// them and the analyst can reach the other only through the filesystem -
    /// and a save from the row they pressed overwrites the palette they did
    /// not.
    NameTaken(PathBuf),
    RoundTrip(RoundTripError),
    Io(io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoName => write!(
                formatter,
                "not saved: this table has no name. The Name row is what the file is \
                 found by afterwards, so give it one first."
            ),
            Self::RenderingSuffix(suffix) => write!(
                formatter,
                "not saved: a name ending in \"{}\" is how this build spells the stepped \
                 and smooth drawings of a palette, so a palette called that would come \
                 back as the shipped default after a restart. Take the ending off - \
                 \"Storm\" rather than \"Storm{suffix}\" - and save again.",
                suffix.trim_start()
            ),
            Self::ShippedName { base, family } => write!(
                formatter,
                "not saved: \"{base}\" is the name of a palette this build ships under \
                 {}, and the shipped one wins that name everywhere - a table saved under \
                 it would come back as the shipped palette after a restart, and its row \
                 would offer Edit on the preset rather than on this. Give this one a \
                 name of its own - \"{base}, mine\" rather than \"{base}\" - and save \
                 again.",
                family.label()
            ),
            Self::NameTaken(path) => write!(
                formatter,
                "not saved: {} already holds a palette of this name, and a name two files \
                 answer to is one the application resolves to a single file. Give this one \
                 a name of its own.",
                path.display()
            ),
            Self::RoundTrip(error) => write!(formatter, "not saved: {error}"),
            Self::Io(error) => write!(formatter, "not saved: {error}"),
        }
    }
}

impl std::error::Error for SaveError {}

#[derive(Debug)]
pub enum LoadError {
    NotATable,
    Io(io::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotATable => write!(
                formatter,
                "this file does not read as a colour table: it needs at least two Color \
                 rows, each carrying a full RGB or RGBA colour"
            ),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for LoadError {}
