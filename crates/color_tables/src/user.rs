//! Colour tables an analyst supplies, read out of a directory of files.
//!
//! GR2Analyst and RadarScope both read palettes as plain text: a `Product:`
//! header, optional `Units:` / `Step:` / `RF:` rows, then `Color:` rows of a
//! data value followed by RGB (and, in the ramp-pair dialect, a second RGB
//! for the far end of that row's interval). Analysts trade those `.pal` files
//! the way they trade any other text file. [`ColorTable::parse`] already
//! reads that dialect, so a palette written for GR2Analyst needs no
//! conversion; what was missing was somewhere to put it and something to
//! notice it was there.
//!
//! This module is that reader. It takes the directory as a parameter and
//! never resolves one itself: no crate in this workspace may hardcode a
//! desktop path, because an iOS shell's sandbox path is known only to the
//! shell (see `settings::paths`). The composition root passes
//! `settings::app_config_root().join("colortables")` - the same root the
//! settings file itself resolves against, so every piece of the
//! application's own state sits in one folder.
//!
//! Five rules hold the reader together.
//!
//! * **A file that does not parse is a fault, not a panic and not a silent
//!   omission.** It is listed with its file name, the parser's reason and
//!   the line number, and skipped everywhere else. A folder with one bad
//!   file in it still yields every good one.
//! * **Names are stable and unique.** A table's display name is its file
//!   stem, so the name in the picker is the name in the folder. A stem that
//!   is already a built-in palette's base name in the same family takes
//!   [`USER_NAME_SUFFIX`], and a stem that would collide with another user
//!   file's *in the same family* takes a numbered suffix, so a picker row
//!   and a persisted choice always name exactly one table.
//! * **Scanning is deterministic.** `read_dir` order is whatever the
//!   filesystem feels like; entries are sorted by file name so the picker
//!   list does not reshuffle between launches.
//! * **A scan is bounded, three ways.** This runs on the UI thread every
//!   time the window comes back to the front, so one rescan has to cost
//!   something an analyst can never see. An unchanged folder is recognised
//!   from its listing alone (file name, length and modification time per
//!   entry) and costs zero file reads and zero parses; no single file is
//!   read past [`MAX_TABLE_BYTES`], because `.txt` is an extension a 50 MB
//!   pile of notes can plausibly wear; and one scan reads at most
//!   [`MAX_SCAN_BYTES`] all told, so a folder of files that are each legal
//!   cannot add up to a pause either. A file either cap turns away is a
//!   fault naming the cap, listed exactly like a file that will not parse.
//! * **Importing never overwrites, and never duplicates.** A dropped file
//!   whose name is already taken in the folder is stored under a numbered
//!   name beside it - losing a palette an analyst spent an evening on to a
//!   name collision is not a trade this makes - but a drop whose bytes are
//!   already in the folder files nothing and says which table it already is.
//!
//! # What a listing can and cannot see
//!
//! Recognising an unchanged folder from its listing is what makes the focus
//! rescan free, and the price of not opening a file is that two different
//! files can present the same listing row. This module answers that the way
//! `git` answers it for its index, because it is the same problem: a cheap
//! `stat` per entry, a filesystem clock coarser than the edits being made,
//! and a wrong answer that costs the user their own work.
//!
//! **The racy-timestamp guard.** Filesystem timestamps come from a clock
//! that moves in steps - about 15 ms on NTFS, a few ms on ext4, whole
//! seconds on HFS+ - so a file saved in the same step the scan ran in
//! carries a stamp the scan cannot order against itself. Any entry whose
//! recorded stamp is not [older than the scan by more than that
//! step](SNAPSHOT_TRUST_MARGIN) is therefore treated as unreliable, and the
//! next scan re-reads the folder rather than trusting the listing. That
//! costs one extra read of files that were just written, and it closes the
//! case where an analyst saves a palette twice inside one clock tick, or
//! saves it in the tick this library happened to read it in.
//!
//! **The edge that remains, and is accepted.** A change that keeps the byte
//! count AND carries a deliberately back-dated modification time - what a
//! timestamp-preserving copy does (`robocopy`'s default, `rsync -t`, an
//! archive extraction) - presents a listing row identical to the one already
//! recorded, from a stamp old enough to trust. That is invisible to
//! [`refresh`](UserTableLibrary::refresh) until something else in the folder
//! moves. It is the same contract `git status` lives with, and the way out
//! is the same: an explicit instruction. The *Rescan colour table folder*
//! button in Settings calls [`reread`](UserTableLibrary::reread), which
//! never looks at the listing at all and always re-reads every file.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{ColorTable, ColorTableError, ColorTableFamily, builtin_tables_for_family};

/// File extensions the scanner reads.
///
/// `.pal` is the GR2Analyst/RadarScope convention. `.txt` is admitted
/// because plenty of shared palettes arrive with that extension (a mail
/// client or a forum attachment renames them), and refusing to read a file
/// whose contents are a perfectly good palette because of three letters
/// would be a rule with no purpose behind it.
pub const USER_TABLE_EXTENSIONS: [&str; 2] = ["pal", "txt"];

/// What a user table's display name takes when its file stem is already a
/// built-in palette's base name in the same family.
pub const USER_NAME_SUFFIX: &str = " (user)";

/// How many numbered variants a name collision may generate before the
/// import gives up and says so. Far past any real folder; it exists so a
/// pathological directory cannot spin the import forever.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// The largest file the scanner will read.
///
/// The scan runs on the UI thread, and `.txt` is admitted precisely because
/// shared palettes arrive with that extension - which also means a stray
/// pile of notes can land in the folder wearing one. Reading a 50 MB text
/// file and running the colour-table parser over it costs about 200 ms, and
/// that is 200 ms of frozen window on every alt-tab.
///
/// Two megabytes is roughly eighty thousand `Color:` rows: three orders of
/// magnitude past the largest palette anyone writes, and small enough that
/// the worst case is invisible. A file past it is a fault naming its size,
/// so an analyst who genuinely has one is told why rather than left
/// wondering where their palette went.
const MAX_TABLE_BYTES: u64 = 2 * 1024 * 1024;

/// The most one scan will read in total, across every file in the folder.
///
/// [`MAX_TABLE_BYTES`] bounds one file; without this, twenty files that are
/// each just under it are forty megabytes of reading and parsing on the UI
/// thread, which is the very thing the per-file cap was put there to
/// prevent. Measured on one desktop with the committed reproducer (`cargo
/// run --release -p color_tables --example user_table_scan_cost`), that
/// folder cost **417 ms** to open with no budget and **67 ms** with this
/// one. Parsing real `Color:` rows runs at roughly 8 ms per megabyte, so the
/// budget IS the worst legal scan: raising it raises that number with it.
///
/// It is four times the largest single file, and at the few kilobytes a
/// palette actually weighs it is thousands of files, so no real folder
/// reaches it. A folder that does gets its files in name order until the
/// budget is gone, and the rest become faults naming the budget - the same
/// treatment, and the same visibility, as a file too big on its own.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;

/// How far in the past a modification time has to be before a scan trusts
/// it - the racy-timestamp guard described in the module documentation.
///
/// A filesystem stamps a file from a clock that moves in steps, and the step
/// is the filesystem's business: about 15 ms on NTFS, a few milliseconds on
/// ext4, a whole second on HFS+. A second covers every one of them (FAT's
/// two-second stamps are the exception, and an application config folder
/// does not live on FAT), and it is small enough that the extra reading it
/// asks for lands on files somebody has just saved and on nothing else.
const SNAPSHOT_TRUST_MARGIN: Duration = Duration::from_secs(1);

/// Which colour family a palette's `Product:` header puts it in.
///
/// The spellings are GR2Analyst's (`BR` base reflectivity, `BV` base
/// velocity, `SW`, `ZDR`, `CC`, `KDP`) plus the plain-English ones the same
/// files are written with elsewhere. Matching ignores case, spaces,
/// underscores and hyphens, because a header written `Product: base_velocity`
/// means exactly what `Product: BV` means.
///
/// A header this list does not know - and a file with no header at all -
/// lands in [`ColorTableFamily::Generic`] rather than being refused. A
/// palette for a quantity this build has no family for is still a palette,
/// and the generic family is where a picker can offer it.
pub fn family_for_product_header(product: Option<&str>) -> ColorTableFamily {
    let Some(product) = product else {
        return ColorTableFamily::Generic;
    };
    match normalized_product(product).as_str() {
        "br" | "ref" | "refl" | "reflectivity" | "basereflectivity" | "dbz" | "z" | "cref"
        | "cr" => ColorTableFamily::Reflectivity,
        "bv"
        | "vel"
        | "v"
        | "velocity"
        | "basevelocity"
        | "srv"
        | "srm"
        | "dv"
        | "stormrelativevelocity" => ColorTableFamily::Velocity,
        "sw" | "w" | "width" | "spectrumwidth" => ColorTableFamily::SpectrumWidth,
        "zdr" | "zd" | "differentialreflectivity" => ColorTableFamily::DifferentialReflectivity,
        "cc" | "rho" | "rhohv" | "correlationcoefficient" => {
            ColorTableFamily::CorrelationCoefficient
        }
        "phi" | "phidp" | "differentialphase" => ColorTableFamily::DifferentialPhase,
        "kdp" | "specificdifferentialphase" => ColorTableFamily::SpecificDifferentialPhase,
        _ => ColorTableFamily::Generic,
    }
}

fn normalized_product(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

/// One palette read from the user's folder.
#[derive(Clone, Debug, PartialEq)]
pub struct UserColorTable {
    path: PathBuf,
    file_name: String,
    display_name: String,
    family: ColorTableFamily,
    table: ColorTable,
}

impl UserColorTable {
    /// The file this was read from, for a status line that has to be
    /// actionable: "which of my files is that" is the first question.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The name a picker row shows and a settings file stores. Equal to
    /// `table().base_name()` by construction, which is what lets a stored
    /// choice find its way back to this file.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn family(&self) -> ColorTableFamily {
        self.family
    }

    pub fn table(&self) -> &ColorTable {
        &self.table
    }
}

/// A file in the folder that could not be turned into a palette.
///
/// Kept rather than dropped: a palette an analyst put in the folder and
/// cannot find in the picker is a bug report, and the only way to answer it
/// without a debugger is to show the file, the reason and the line.
#[derive(Clone, Debug, PartialEq)]
pub struct UserTableFault {
    path: PathBuf,
    file_name: String,
    line: Option<usize>,
    reason: String,
}

impl UserTableFault {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The 1-based line the parser refused, when it named one.
    pub fn line(&self) -> Option<usize> {
        self.line
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for UserTableFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(
                formatter,
                "{} - {} (line {line})",
                self.file_name, self.reason
            ),
            None => write!(formatter, "{} - {}", self.file_name, self.reason),
        }
    }
}

/// Everything the user's colour table folder currently holds.
///
/// Rebuilt wholesale by [`UserTableLibrary::refresh`] rather than patched
/// incrementally: the folder is small, the analyst edits it with a text
/// editor and a file manager behind the application's back, and a scan is
/// the only reading of it that cannot go stale.
#[derive(Clone, Debug)]
pub struct UserTableLibrary {
    directory: PathBuf,
    tables: Vec<UserColorTable>,
    faults: Vec<UserTableFault>,
    generation: u64,
    /// The listing the current tables and faults were read from, or `None`
    /// before the first scan. What makes an unchanged folder free.
    snapshot: Option<DirectoryListing>,
    /// How many files every scan so far has opened. Not part of what the
    /// application does with a library; it is how the tests tell "recognised
    /// the folder from its listing" from "read it again and got the same
    /// answer", which is the whole claim the short circuit makes.
    files_read: u64,
}

impl UserTableLibrary {
    /// A library for a directory, without reading it. For a caller that has
    /// no folder yet and for tests that want a known-empty one.
    pub fn empty(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            tables: Vec::new(),
            faults: Vec::new(),
            generation: 0,
            snapshot: None,
            files_read: 0,
        }
    }

    /// A library for a directory, scanned now. A directory that does not
    /// exist yet is not an error - it is created on the first import.
    pub fn open(directory: impl Into<PathBuf>) -> Self {
        let mut library = Self::empty(directory);
        library.refresh();
        library
    }

    /// Re-read the folder if its listing has moved. Called at startup, when
    /// the window regains focus (the analyst has been in a text editor), and
    /// after an import. Returns whether anything was re-read, which is also
    /// exactly when [`generation`](Self::generation) moved.
    ///
    /// The listing - file name, length and modification time per entry - is
    /// the short circuit. A folder nobody has touched costs one directory
    /// listing and not one file read, which is what keeps this affordable on
    /// the UI thread: an alt-tab out and back must not be a visible pause,
    /// and before this it was one whenever the folder held anything large.
    ///
    /// A listing is only trusted when the clock backs it up. An entry
    /// stamped too recently for the scan that recorded it to order against
    /// itself (see [`SNAPSHOT_TRUST_MARGIN`]) is unreliable, and one
    /// unreliable entry sends the whole folder back through a read - which
    /// is what catches a palette saved twice inside one clock tick. The one
    /// case that survives that - a same-length edit carrying a deliberately
    /// back-dated stamp - is documented at the top of this module and is
    /// what [`reread`](Self::reread) is for.
    pub fn refresh(&mut self) -> bool {
        let listing = scan_directory(&self.directory);
        if let Some(previous) = &self.snapshot
            && previous.entries == listing.entries
            && !previous.holds_a_stamp_the_scan_cannot_trust()
        {
            return false;
        }
        self.read(listing);
        true
    }

    /// Re-read the folder without consulting its listing at all.
    ///
    /// The analyst's escape hatch, and the reason the Rescan button in
    /// Settings exists: an explicit instruction gets an actual read, so the
    /// one change a listing cannot see (a same-length edit under a
    /// back-dated timestamp) is always one click from being picked up.
    /// Every file is opened, whether or not anything appears to have moved,
    /// and the generation always advances.
    pub fn reread(&mut self) {
        let listing = scan_directory(&self.directory);
        self.read(listing);
    }

    /// Read every entry in a listing and install what came back. The single
    /// place tables, faults, the snapshot and the generation move together.
    fn read(&mut self, listing: DirectoryListing) {
        let (tables, faults, files_read) = read_entries(&listing.entries);
        self.tables = tables;
        self.faults = faults;
        self.files_read = self.files_read.wrapping_add(files_read);
        self.snapshot = Some(listing);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Bumped by every [`refresh`](Self::refresh). Callers that cache a
    /// derived list - the product picker caches its palette rows - key on
    /// this so a rescan invalidates them without comparing the tables.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn tables(&self) -> &[UserColorTable] {
        &self.tables
    }

    pub fn faults(&self) -> &[UserTableFault] {
        &self.faults
    }

    /// Nothing to offer and nothing to complain about, so a UI section can
    /// stay off the page entirely.
    pub fn is_quiet(&self) -> bool {
        self.tables.is_empty() && self.faults.is_empty()
    }

    pub fn tables_for_family(
        &self,
        family: ColorTableFamily,
    ) -> impl Iterator<Item = &UserColorTable> {
        self.tables
            .iter()
            .filter(move |entry| entry.family == family)
    }

    /// The table a stored palette choice names, if the folder still holds
    /// it. `None` for a file the analyst deleted - which the caller answers
    /// by falling back to the family default WITHOUT rewriting the stored
    /// name, because the file may come back.
    pub fn table_for_family_named(
        &self,
        family: ColorTableFamily,
        base_name: &str,
    ) -> Option<&ColorTable> {
        self.tables_for_family(family)
            .find(|entry| entry.display_name == base_name)
            .map(UserColorTable::table)
    }

    /// Copy a file into the folder and load it.
    ///
    /// The parse happens FIRST, against the file where it lies. A file that
    /// is not a colour table is reported with the parser's own reason and
    /// line and left exactly where it was dropped from, rather than filling
    /// the folder with files that would fault on every scan for ever.
    ///
    /// Two things the folder is protected from. Nothing in it is ever
    /// overwritten: a *name* that is taken gets a numbered sibling. And
    /// nothing is duplicated: a drop whose *bytes* are already in the folder
    /// files nothing and names the table it already is. The two together are
    /// what make re-dropping a palette - the obvious thing to do after
    /// editing one, and the gesture the focus rescan exists for - safe in
    /// both directions: the edited file lands beside the old one, the
    /// unedited one lands nowhere.
    pub fn import(&mut self, source: &Path) -> ImportOutcome {
        let file_name = file_name_of(source);
        // The size question is asked of the filesystem BEFORE the bytes are
        // asked for. A mis-drag of a half-gigabyte file is one `metadata`
        // call rather than half a gigabyte read into memory on the UI thread
        // and then thrown away - which is exactly what the scanner does with
        // the directory listing, so both paths turn a file away at the same
        // point for the same reason.
        let len = match std::fs::metadata(source) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                return ImportOutcome::Unreadable {
                    file_name,
                    reason: error.to_string(),
                };
            }
        };
        if len > MAX_TABLE_BYTES {
            return ImportOutcome::TooLarge { file_name, len };
        }
        let bytes = match std::fs::read(source) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ImportOutcome::Unreadable {
                    file_name,
                    reason: error.to_string(),
                };
            }
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if let Err(error) = ColorTable::parse(stem_of(source), &text) {
            let (line, reason) = fault_parts(&error);
            return ImportOutcome::Rejected {
                file_name,
                line,
                reason,
            };
        }

        // A drop from the folder itself - dragging a palette out of the file
        // manager window it already lives in - must load it, not make a
        // second copy of it. Asked once: the answer costs two
        // `canonicalize` calls.
        let dropped_from_the_folder = is_in_directory(source, &self.directory);
        if !dropped_from_the_folder && let Some(existing) = self.file_with_bytes(&bytes) {
            // Byte-identical to something already filed. Nothing to copy,
            // and a numbered sibling here would be a second picker row for
            // one palette. (An identical file the rescan does not hold as a
            // table was moved between the comparison and here; that falls
            // through and is filed like any other drop.)
            self.refresh();
            if let Some(entry) = self.tables.iter().find(|entry| entry.path == existing) {
                return ImportOutcome::AlreadyImported {
                    file_name,
                    display_name: entry.display_name.clone(),
                    stored_as: entry.file_name.clone(),
                    family: entry.family,
                };
            }
        }

        let destination = if dropped_from_the_folder {
            source.to_path_buf()
        } else {
            match self.copied_into_the_folder(source, &file_name) {
                Ok(destination) => destination,
                Err(outcome) => return outcome,
            }
        };

        self.refresh();
        match self.tables.iter().find(|entry| entry.path == destination) {
            Some(entry) => ImportOutcome::Loaded {
                display_name: entry.display_name.clone(),
                family: entry.family,
                stored_as: entry.file_name.clone(),
            },
            // The file parsed a moment ago and is on disk now, so the rescan
            // disagreeing means something else moved it. Say that rather
            // than unwrap on it.
            None => ImportOutcome::NotStored {
                file_name,
                reason: "stored, but the rescan no longer found it".to_owned(),
            },
        }
    }

    /// Create the folder, pick a free name in it, and copy the file there.
    /// Every failure on the way is an [`ImportOutcome`] the caller returns.
    fn copied_into_the_folder(
        &self,
        source: &Path,
        file_name: &str,
    ) -> Result<PathBuf, ImportOutcome> {
        if let Err(error) = std::fs::create_dir_all(&self.directory) {
            return Err(ImportOutcome::NotStored {
                file_name: file_name.to_owned(),
                reason: error.to_string(),
            });
        }
        let Some(destination) = self.free_destination(source) else {
            return Err(ImportOutcome::NotStored {
                file_name: file_name.to_owned(),
                reason: format!("{MAX_NAME_ATTEMPTS} files of that name are already there"),
            });
        };
        if let Err(error) = std::fs::copy(source, &destination) {
            return Err(ImportOutcome::NotStored {
                file_name: file_name.to_owned(),
                reason: error.to_string(),
            });
        }
        Ok(destination)
    }

    /// A file already in the folder with exactly these bytes, if there is
    /// one.
    ///
    /// Length first, off the directory listing, so the common case - nothing
    /// in the folder is even the right size - opens no file at all. Only
    /// same-length candidates are read, and every file the scanner will read
    /// is under [`MAX_TABLE_BYTES`], so the comparison is bounded.
    fn file_with_bytes(&self, bytes: &[u8]) -> Option<PathBuf> {
        let len = bytes.len() as u64;
        scan_directory(&self.directory)
            .entries
            .into_iter()
            .filter(|entry| entry.len == len)
            .map(|entry| entry.path)
            .find(|path| std::fs::read(path).is_ok_and(|existing| existing == bytes))
    }

    /// The first free file name in the folder for this source: its own name,
    /// then `stem (2).ext`, `stem (3).ext` and so on.
    fn free_destination(&self, source: &Path) -> Option<PathBuf> {
        let stem = stem_of(source);
        let extension = source
            .extension()
            .map(|extension| extension.to_string_lossy().into_owned())
            .unwrap_or_else(|| USER_TABLE_EXTENSIONS[0].to_owned());
        let first = self.directory.join(format!("{stem}.{extension}"));
        if !first.exists() {
            return Some(first);
        }
        (2..=MAX_NAME_ATTEMPTS)
            .map(|index| self.directory.join(format!("{stem} ({index}).{extension}")))
            .find(|candidate| !candidate.exists())
    }
}

/// What one [`UserTableLibrary::import`] did, in enough detail for a status
/// line an analyst can act on.
#[derive(Clone, Debug, PartialEq)]
pub enum ImportOutcome {
    /// Parsed, in the folder, and in the library now.
    Loaded {
        display_name: String,
        family: ColorTableFamily,
        /// The name it is stored under, which differs from the dropped name
        /// when that one was taken.
        stored_as: String,
    },
    /// Byte-identical to a table already in the folder. Nothing was copied
    /// and nothing changed; the analyst already has this palette.
    AlreadyImported {
        file_name: String,
        /// The name the folder's copy is known by in the picker.
        display_name: String,
        /// The file name the folder's copy sits under, which is how the
        /// analyst finds it if it is not the name they just dropped.
        stored_as: String,
        family: ColorTableFamily,
    },
    /// The parser refused it. The file is untouched where it came from.
    Rejected {
        file_name: String,
        /// The 1-based line the parser named, when it named one.
        line: Option<usize>,
        reason: String,
    },
    /// Past [`MAX_TABLE_BYTES`], so it was never read. Its own outcome
    /// rather than a [`Rejected`](Self::Rejected) with a size for a reason:
    /// a 3 MB palette written in a dialect this build reads perfectly well
    /// is not "not a colour table", it is a colour table this build has put
    /// a limit under, and a message that says the first thing sends the
    /// analyst looking for a fault in a file that has none.
    TooLarge {
        file_name: String,
        /// What the filesystem said it was, for a message that names the
        /// size beside the cap.
        len: u64,
    },
    /// A real palette that could not be copied into the folder.
    NotStored { file_name: String, reason: String },
    /// The file could not be read at all.
    Unreadable { file_name: String, reason: String },
}

impl ImportOutcome {
    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded { .. })
    }

    /// Whether this is something an analyst has to do something about.
    ///
    /// Not the same question as [`is_loaded`](Self::is_loaded): a drop that
    /// was already in the folder loaded nothing and is still perfectly fine,
    /// so it is reported in the ordinary voice rather than the warning one.
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Self::Rejected { .. }
                | Self::TooLarge { .. }
                | Self::NotStored { .. }
                | Self::Unreadable { .. }
        )
    }

    /// One line for the toast. Every failure names the file, because a drop
    /// of four files that reports "parse error" names nothing.
    pub fn status_line(&self) -> String {
        match self {
            Self::Loaded {
                display_name,
                family,
                stored_as,
            } => {
                let mut line = format!(
                    "Colour table \"{display_name}\" loaded into {}",
                    family.label()
                );
                if stored_as != display_name {
                    line.push_str(&format!(" (stored as {stored_as})"));
                }
                line
            }
            Self::AlreadyImported {
                file_name,
                display_name,
                stored_as,
                family,
            } => {
                let mut line = format!(
                    "{file_name} is already imported as \"{display_name}\" in {}",
                    family.label()
                );
                if stored_as != file_name {
                    line.push_str(&format!(" (the file {stored_as})"));
                }
                line.push_str(". Nothing was filed.");
                line
            }
            Self::Rejected {
                file_name,
                line,
                reason,
            } => match line {
                Some(line) => format!(
                    "{file_name} is not a colour table this build can read: {reason} (line \
                     {line}). Left where it was."
                ),
                None => format!(
                    "{file_name} is not a colour table this build can read: {reason}. Left where \
                     it was."
                ),
            },
            Self::TooLarge { file_name, len } => format!(
                "{file_name} is too large for this build to read: {} is past the {} a colour \
                 table is read up to. Left where it was.",
                describe_size(*len),
                describe_size(MAX_TABLE_BYTES)
            ),
            Self::NotStored { file_name, reason } => {
                format!("{file_name} parsed, but could not be filed: {reason}")
            }
            Self::Unreadable { file_name, reason } => {
                format!("{file_name} could not be read: {reason}")
            }
        }
    }
}

/// The rows a picker draws for one family, built-ins first and the analyst's
/// own tables after them.
///
/// The same shape as [`crate::palette_offers_for_family`] - every row in the
/// rendering the installed table is being drawn in, and one last row that is
/// the installed table drawn the other way - with the user's tables spliced
/// in between the catalogue and that flip row. After, not before, because a
/// picker whose first rows moved the day an analyst dropped a file into a
/// folder would have lost the muscle memory that makes it usable mid-storm.
///
/// Names stay unique across the returned list: user display names are barred
/// from colliding with a built-in base name in the same family (see
/// [`USER_NAME_SUFFIX`]), and every row carries the installed rendering in
/// its name except the flip row, which carries the other one.
pub fn palette_offers_with_user_tables(
    family: ColorTableFamily,
    installed: &ColorTable,
    library: &UserTableLibrary,
) -> Vec<ColorTable> {
    let rendering = installed.rendering();
    let mut offers: Vec<ColorTable> = builtin_tables_for_family(family)
        .into_iter()
        .map(|table| table.rendered(rendering))
        .collect();
    offers.extend(
        library
            .tables_for_family(family)
            .map(|entry| entry.table().rendered(rendering)),
    );
    if !offers
        .iter()
        .any(|table| table.base_name() == installed.base_name())
    {
        offers.push(installed.clone());
    }
    let flipped = installed.rendered(rendering.flipped());
    if !offers.iter().any(|table| table.name() == flipped.name()) {
        offers.push(flipped);
    }
    offers
}

/// One directory entry as a scan saw it: enough to tell "nothing has moved"
/// from "something has" without opening a single file.
///
/// Length as well as modification time, because a filesystem timestamp is
/// only as fine as the clock that stamped it and a length change catches
/// most of what a coarse clock hides. See [`UserTableLibrary::refresh`] for
/// the case neither catches and the way out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryEntry {
    /// The path the directory read handed over, carried whole rather than
    /// rebuilt from `file_name`: a file name is not always valid Unicode
    /// (any Linux filesystem permits it, and Windows permits unpaired
    /// surrogates), and a path rebuilt from the lossy spelling names a file
    /// that does not exist.
    path: PathBuf,
    /// The same name in the spelling a human is shown, which is the lossy
    /// one. For display and for ordering only - never for opening.
    file_name: String,
    len: u64,
    modified: Option<SystemTime>,
}

impl DirectoryEntry {
    /// Whether this entry's stamp is too close to `taken_at` for the scan
    /// that recorded it to trust - the racy-timestamp guard.
    ///
    /// An entry with no timestamp at all is not distrusted: nothing was
    /// claimed about it, its length still guards it, and a filesystem that
    /// reports no modification time would otherwise force a full read on
    /// every single scan for ever.
    fn stamp_is_too_fresh_to_trust(&self, taken_at: SystemTime) -> bool {
        let Some(modified) = self.modified else {
            return false;
        };
        match taken_at.duration_since(modified) {
            Ok(age) => age <= SNAPSHOT_TRUST_MARGIN,
            // Stamped after the scan started: a clock-skewed network share,
            // or a save that landed while the folder was being listed.
            // Either way it is not a stamp to draw conclusions from.
            Err(_) => true,
        }
    }
}

/// A whole directory listing, and the moment it was taken.
///
/// The moment is half of the racy-timestamp guard: "is this entry's stamp
/// old enough to trust" is only a question you can ask against the instant
/// the reading was made.
#[derive(Clone, Debug)]
struct DirectoryListing {
    taken_at: SystemTime,
    entries: Vec<DirectoryEntry>,
}

impl DirectoryListing {
    /// Whether any entry in this listing was stamped too recently for the
    /// listing to be believed the next time it is compared against.
    fn holds_a_stamp_the_scan_cannot_trust(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.stamp_is_too_fresh_to_trust(self.taken_at))
    }
}

/// List the folder: every file with a colour table extension, with the facts
/// that decide whether it needs reading, in name order.
///
/// `read_dir` order is the filesystem's business. Sorting is what keeps a
/// picker list in the same order twice in a row, and it is also what makes
/// two listings comparable entry by entry.
///
/// One `metadata` call per entry and no file contents: on every platform
/// this workspace targets, the directory read already carried the size and
/// the timestamp, so this is the listing and nothing more.
///
/// A symlink is followed. `DirEntry::metadata` deliberately does not
/// traverse one - it describes the link - and a link is how a palette folder
/// is shared on the platforms this workspace targets (`ln -s
/// ~/Dropbox/palettes/Mine.pal <config>/colortables/`). A linked palette
/// that neither loads nor faults is the exact "my file is in the folder and
/// not in the picker" report the fault list exists to answer, so the one
/// entry in a thousand that is a link pays for a second `metadata` call, of
/// the traversing kind, and everything else keeps the free one.
fn scan_directory(directory: &Path) -> DirectoryListing {
    // Before the listing, not after: an entry stamped at or after this
    // instant is one the next scan must not trust (see
    // `DirectoryEntry::stamp_is_too_fresh_to_trust`), and taking the moment
    // first is the conservative end of that.
    let taken_at = SystemTime::now();
    let mut entries: Vec<DirectoryEntry> = match std::fs::read_dir(directory) {
        Ok(read) => read
            .filter_map(Result::ok)
            .filter(|entry| has_table_extension(Path::new(&entry.file_name())))
            .filter_map(|entry| {
                let path = entry.path();
                let listed = entry.metadata().ok()?;
                let metadata = if listed.file_type().is_symlink() {
                    std::fs::metadata(&path).ok()?
                } else {
                    listed
                };
                metadata.is_file().then(|| DirectoryEntry {
                    file_name: entry.file_name().to_string_lossy().into_owned(),
                    path,
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                })
            })
            .collect(),
        // No folder yet, or one this process cannot list. Neither is worth a
        // fault row: the folder is created on the first import, and a
        // permission problem shows as an empty list beside the path that was
        // tried, which is what the settings footer does for its own file.
        Err(_) => Vec::new(),
    };
    entries.sort_by(|left, right| {
        // `crate::files::compare_file_names`, shared with the search that
        // resolves a stored palette name, because both take the FIRST file
        // that answers to a name and two orders would take two files. Two
        // names that differ only in bytes no spelling can show collapse to one
        // `file_name`; the path is what still tells them apart, and an order
        // that is not total is an order that reshuffles between launches.
        crate::files::compare_file_names(&left.file_name, &right.file_name)
            .then_with(|| left.path.cmp(&right.path))
    });
    DirectoryListing { taken_at, entries }
}

/// Read and parse a listing, in the order it is given. Returns the tables,
/// the faults, and how many files were opened.
///
/// The order is load-bearing three times over: it is the order the picker
/// offers the tables in, it is the order the numbering rule hands out names
/// in - so which file a stored choice resolves to depends on it - and it is
/// the order [`MAX_SCAN_BYTES`] is spent in, so a folder past the budget
/// keeps the same files from one scan to the next rather than a different
/// arbitrary handful each time.
fn read_entries(entries: &[DirectoryEntry]) -> (Vec<UserColorTable>, Vec<UserTableFault>, u64) {
    let mut tables = Vec::new();
    let mut faults = Vec::new();
    let mut files_read = 0u64;
    let mut budget_left = MAX_SCAN_BYTES;
    // Numbering is per family, not global: two files that can never appear
    // in one picker list must not push each other's numbers, or a folder
    // with a reflectivity `Mine.pal` beside a velocity `Mine.txt` offers
    // "Mine" in one list and "Mine (2)" in the other for no reason an
    // analyst can see.
    let mut taken: Vec<BTreeSet<String>> = vec![BTreeSet::new(); ColorTableFamily::ALL.len()];
    for entry in entries {
        let path = entry.path.clone();
        let file_name = entry.file_name.clone();
        if entry.len > MAX_TABLE_BYTES {
            // Not silence and not a 200 ms parse: a fault row that says how
            // big the file is, so an analyst can see it is the size that is
            // the problem rather than the contents.
            faults.push(UserTableFault {
                path,
                file_name,
                line: None,
                reason: format!(
                    "{} is past the {} a colour table is read up to",
                    describe_size(entry.len),
                    describe_size(MAX_TABLE_BYTES)
                ),
            });
            continue;
        }
        // The per-file cap bounds one file; this bounds the scan. Twenty
        // files that are each just legal are forty megabytes of parsing on
        // the UI thread, and the analyst who is owed an answer about them is
        // owed it in the fault list rather than in a frozen window.
        let Some(remaining) = budget_left.checked_sub(entry.len) else {
            faults.push(UserTableFault {
                path,
                file_name,
                line: None,
                reason: format!(
                    "the {} one scan reads was already spent on the files before this one",
                    describe_size(MAX_SCAN_BYTES)
                ),
            });
            continue;
        };
        budget_left = remaining;
        files_read += 1;
        let text = match std::fs::read(&path) {
            // Lossy on purpose: a palette is ASCII, and one stray byte in a
            // comment must not cost the analyst the whole table.
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                faults.push(UserTableFault {
                    path,
                    file_name,
                    line: None,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        // The `Name:` row inside the file, or its stem when it has none -
        // `crate::files::palette_identity`, the same rule the shared search
        // resolves a stored palette name with. A scanner that named a file by
        // its stem while the search named it by its `Name:` row would offer a
        // row in the picker that the next launch could not install.
        let identity =
            crate::files::palette_identity(&path, &text).unwrap_or_else(|| stem_of(&path));
        match ColorTable::parse(identity.clone(), &text) {
            Ok(table) => {
                let family = family_for_product_header(table.product());
                let slot = family_slot(family);
                let display_name = display_name_for(&identity, family, &taken[slot]);
                taken[slot].insert(display_name.clone());
                let table = table.renamed(display_name.clone());
                tables.push(UserColorTable {
                    path,
                    file_name,
                    display_name,
                    family,
                    table,
                });
            }
            Err(error) => {
                let (line, reason) = fault_parts(&error);
                faults.push(UserTableFault {
                    path,
                    file_name,
                    line,
                    reason,
                });
            }
        }
    }
    (tables, faults, files_read)
}

/// Which per-family numbering set a family uses.
///
/// `ColorTableFamily::ALL` holds every variant, so the fallback is
/// unreachable; it is a `0` rather than a panic because sharing
/// reflectivity's numbering would still produce unique, resolvable names,
/// and a scanner that panics on a folder is worse than one that numbers
/// oddly.
fn family_slot(family: ColorTableFamily) -> usize {
    ColorTableFamily::ALL
        .iter()
        .position(|known| *known == family)
        .unwrap_or(0)
}

/// A file size an analyst can read at a glance, for the oversize fault row.
fn describe_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{bytes:.0} bytes")
    }
}

/// The name a user table is known by everywhere: the picker row, the
/// settings file, and `ColorTable::base_name`.
///
/// Two rules, in order:
///
/// * a name this build cannot carry as an analyst's own takes
///   [`USER_NAME_SUFFIX`]. Which names those are is not decided here: it is
///   [`crate::user_palette_name_fault`], the one place the rule is written,
///   asked of this table's own family. The colour table editor asks the same
///   function before it writes a file and refuses the name outright - a
///   refusal where there is still somebody on screen to tell, a rename where
///   there is not. The two have to be one rule, or the editor writes files
///   this scanner then renames out from under the name the settings file
///   stored;
/// * a name another file *in the same family* already took gets ` (2)`,
///   ` (3)`, and so on - `a.pal` and `a.txt` are two files and must be two
///   rows. `taken` is that family's set and no other's: two files that can
///   never appear in one picker list must not push each other's numbers.
fn display_name_for(identity: &str, family: ColorTableFamily, taken: &BTreeSet<String>) -> String {
    let trimmed = identity.trim();
    let mut candidate = if trimmed.is_empty() {
        "Untitled".to_owned()
    } else {
        trimmed.to_owned()
    };
    if crate::user_palette_name_fault(&candidate, Some(family)).is_some() {
        candidate.push_str(USER_NAME_SUFFIX);
    }
    if !taken.contains(&candidate) {
        return candidate;
    }
    // `taken` grows by one per file, so a free name is always within reach;
    // the bound is here so a caller cannot be hung by a strange folder.
    (2..=MAX_NAME_ATTEMPTS)
        .map(|index| format!("{candidate} ({index})"))
        .find(|name| !taken.contains(name))
        .unwrap_or(candidate)
}

fn fault_parts(error: &ColorTableError) -> (Option<usize>, String) {
    match error {
        ColorTableError::InvalidColor { line, reason } => (Some(*line), (*reason).to_owned()),
        ColorTableError::NotEnoughStops => (None, error.to_string()),
    }
}

fn has_table_extension(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| USER_TABLE_EXTENSIONS.contains(&extension.as_str()))
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether a path already sits in the folder. Compared through
/// `canonicalize` so a drop that arrives as a shortened or differently-cased
/// path is still recognised; a path that cannot be canonicalised (it was
/// deleted between the drop and here) is treated as outside, which costs a
/// copy attempt and nothing else.
fn is_in_directory(path: &Path, directory: &Path) -> bool {
    let (Some(parent), Ok(directory)) = (path.parent(), directory.canonicalize()) else {
        return false;
    };
    parent
        .canonicalize()
        .is_ok_and(|parent| parent == directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorTableSet, TableRendering};

    /// A RadarScope/GR2Analyst-shaped velocity palette with two-colour ramp
    /// rows: each `Color:` row names the colour at its own value and the
    /// colour at the far end of the interval. `.pal` files traded between
    /// analysts are written this way.
    const RAMP_PAIR_VELOCITY: &str = "\
; two-colour ramp rows, GR2Analyst dialect
Product: BV
Units: KTS
Color: -120 130   0 130   200   0 200
Color:  -60 200   0 200    60 220 220
Color:  -20  60 220 220     8  60  70
Color:   -1   8  60  70     8  60  70
Color:    1  70  20  20   220  60  60
Color:   20 220  60  60   255 230  60
Color:   60 255 230  60   255 255 255
";

    const SIMPLE_REFLECTIVITY: &str = "\
Product: BR
Color: 0 0 0 0
Color: 40 200 200 0
Color: 75 255 255 255
";

    /// The same palette with its last row a different colour, and byte for
    /// byte the same length - the edit a listing cannot see by length alone,
    /// and an entirely ordinary one: nudging a colour is what editing a
    /// palette mostly is.
    const SIMPLE_REFLECTIVITY_EDITED: &str = "\
Product: BR
Color: 0 0 0 0
Color: 40 200 200 0
Color: 75 254 254 254
";

    fn scratch_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("color-tables-user-scan")
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

    fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, text).expect("write fixture");
        path
    }

    /// The modification time the filesystem reports for a file right now.
    fn stamp_of(path: &Path) -> SystemTime {
        std::fs::metadata(path)
            .expect("stat fixture")
            .modified()
            .expect("this platform records modification times")
    }

    /// Put a modification time back exactly as it was - what every
    /// timestamp-preserving copy does (`robocopy` by default, `rsync -t`, an
    /// archive extraction), and the half of the listing key that then says
    /// nothing happened.
    fn set_stamp(path: &Path, when: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open fixture")
            .set_modified(when)
            .expect("this platform sets modification times");
    }

    /// Stamp a file with a modification time the next scan cannot vouch for,
    /// and hand it back so an edit can put it straight back.
    ///
    /// A moment the scan has not reached yet is what a save landing inside
    /// the scan's own clock tick looks like from the listing's side: a stamp
    /// the scan has no way to order against itself. Producing it this way
    /// rather than by racing the machine's clock is what makes the test that
    /// uses it deterministic on a loaded box - the bug it pins was reported
    /// as "fires on some runs and not others".
    fn stamp_the_scan_cannot_vouch_for(path: &Path) -> SystemTime {
        let when = SystemTime::now() + Duration::from_secs(30);
        set_stamp(path, when);
        when
    }

    /// Stamp a file as saved a while back - an ordinary file in an ordinary
    /// folder, old enough that a scan trusts its listing row.
    ///
    /// The alternative is to sit and wait out [`SNAPSHOT_TRUST_MARGIN`] on
    /// the real clock, which is slower and no more truthful: a folder an
    /// analyst edited last week is the case these tests are about.
    fn stamped_a_while_ago(path: &Path) {
        set_stamp(path, SystemTime::now() - Duration::from_secs(60));
    }

    /// Wait until every stamp in a folder is old enough for a scan to trust,
    /// so what follows exercises the listing comparison rather than the
    /// racy-timestamp guard.
    fn settle_past_the_trust_margin(directory: &Path) {
        let mut newest = SystemTime::UNIX_EPOCH;
        for entry in std::fs::read_dir(directory).expect("list scratch dir") {
            let entry = entry.expect("read scratch entry");
            if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) {
                newest = newest.max(modified);
            }
        }
        let trusted_from = newest + SNAPSHOT_TRUST_MARGIN + Duration::from_millis(20);
        while let Ok(remaining) = trusted_from.duration_since(SystemTime::now()) {
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }

    /// The colour of a reflectivity table's last stop: what an edit to the
    /// last `Color:` row moves, and so the cheapest proof that a file was
    /// actually re-read rather than remembered.
    fn last_stop_colour(library: &UserTableLibrary, name: &str) -> (u8, u8, u8) {
        let colour = library
            .table_for_family_named(ColorTableFamily::Reflectivity, name)
            .unwrap_or_else(|| panic!("the folder holds a table called {name}"))
            .stops()
            .last()
            .expect("stops exist")
            .color;
        (colour.r, colour.g, colour.b)
    }

    /// A palette that parses and is exactly `len` bytes long: the real rows,
    /// then one comment line padded out to size. The caps are about size, so
    /// what tests them has to be a legal palette that happens to be big.
    fn padded_palette(len: usize) -> String {
        let mut text = String::from(SIMPLE_REFLECTIVITY);
        text.push(';');
        let padding = len - text.len() - 1;
        text.push_str(&"x".repeat(padding));
        text.push('\n');
        assert_eq!(text.len(), len, "the padding arithmetic must be exact");
        text
    }

    /// Make `link` a symbolic link to `target`, or say why this machine
    /// would not.
    #[cfg(unix)]
    fn link_file(target: &Path, link: &Path) -> Result<(), String> {
        std::os::unix::fs::symlink(target, link).map_err(|error| error.to_string())
    }

    #[cfg(windows)]
    fn link_file(target: &Path, link: &Path) -> Result<(), String> {
        std::os::windows::fs::symlink_file(target, link).map_err(|error| {
            format!(
                "this machine will not create a file symlink ({error}). Windows wants either \
                 elevation or Developer Mode for one, and a junction - the link Windows does \
                 hand out freely - links a directory, so it cannot stand in for a linked .pal"
            )
        })
    }

    #[cfg(not(any(unix, windows)))]
    fn link_file(_target: &Path, _link: &Path) -> Result<(), String> {
        Err("no symlink API on this platform".to_owned())
    }

    /// A file name this platform accepts and no string can spell: an
    /// unpaired surrogate on Windows, a stray byte on Unix. `None` when the
    /// filesystem in front of us refuses one.
    #[cfg(windows)]
    fn a_name_no_spelling_can_show() -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        // "Bad\u{D800}.pal" - a lone high surrogate, which NTFS stores and
        // `to_string_lossy` turns into a replacement character.
        std::ffi::OsString::from_wide(&[
            0x0042, 0x0061, 0x0064, 0xD800, 0x002E, 0x0070, 0x0061, 0x006C,
        ])
    }

    #[cfg(unix)]
    fn a_name_no_spelling_can_show() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"Bad\xFF.pal".to_vec())
    }

    #[cfg(not(any(unix, windows)))]
    fn a_name_no_spelling_can_show() -> std::ffi::OsString {
        std::ffi::OsString::from("Bad.pal")
    }

    /// Hold a file's bytes back while its size stays visible, if this
    /// platform can arrange that. The guard restores it when dropped;
    /// `None` means the environment would not play along, and the caller
    /// falls back to the assertions that need no such arrangement.
    #[cfg(windows)]
    fn deny_reads(path: &Path) -> Option<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        let exclusive = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .ok()?;
        let size_still_visible = std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0);
        let bytes_denied = std::fs::read(path).is_err();
        (size_still_visible && bytes_denied).then_some(exclusive)
    }

    #[cfg(unix)]
    struct ReadDenial {
        path: PathBuf,
        original: std::fs::Permissions,
    }

    #[cfg(unix)]
    impl Drop for ReadDenial {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.path, self.original.clone());
        }
    }

    #[cfg(unix)]
    fn deny_reads(path: &Path) -> Option<ReadDenial> {
        use std::os::unix::fs::PermissionsExt;
        let original = std::fs::metadata(path).ok()?.permissions();
        let mut denied = original.clone();
        denied.set_mode(0o000);
        std::fs::set_permissions(path, denied).ok()?;
        let denial = ReadDenial {
            path: path.to_path_buf(),
            original,
        };
        // Running as root ignores the mode bits, which is ordinary inside a
        // container; then there is nothing to arrange and the guard puts the
        // file back on its way out.
        let size_still_visible = std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0);
        let bytes_denied = std::fs::read(path).is_err();
        (size_still_visible && bytes_denied).then_some(denial)
    }

    #[cfg(not(any(unix, windows)))]
    fn deny_reads(_path: &Path) -> Option<()> {
        None
    }

    #[test]
    fn product_headers_route_case_insensitively_and_unknown_lands_in_generic() {
        for (header, family) in [
            ("BR", ColorTableFamily::Reflectivity),
            ("br", ColorTableFamily::Reflectivity),
            ("REF", ColorTableFamily::Reflectivity),
            ("BV", ColorTableFamily::Velocity),
            ("vel", ColorTableFamily::Velocity),
            ("Base_Velocity", ColorTableFamily::Velocity),
            ("SW", ColorTableFamily::SpectrumWidth),
            ("ZDR", ColorTableFamily::DifferentialReflectivity),
            ("zdr", ColorTableFamily::DifferentialReflectivity),
            ("RHO", ColorTableFamily::CorrelationCoefficient),
            ("CC", ColorTableFamily::CorrelationCoefficient),
            ("PHI", ColorTableFamily::DifferentialPhase),
            ("KDP", ColorTableFamily::SpecificDifferentialPhase),
            ("kdp ", ColorTableFamily::SpecificDifferentialPhase),
            ("VIL", ColorTableFamily::Generic),
            ("", ColorTableFamily::Generic),
        ] {
            assert_eq!(
                family_for_product_header(Some(header)),
                family,
                "header {header:?}"
            );
        }
        assert_eq!(
            family_for_product_header(None),
            ColorTableFamily::Generic,
            "a palette with no Product row still belongs somewhere"
        );
    }

    #[test]
    fn a_folder_that_does_not_exist_yet_scans_empty_rather_than_failing() {
        let missing = scratch_dir("absent").join("not-created-yet");
        let library = UserTableLibrary::open(&missing);
        assert!(library.tables().is_empty());
        assert!(library.faults().is_empty());
        assert!(library.is_quiet());
        assert_eq!(library.directory(), missing);
    }

    #[test]
    fn one_good_file_becomes_one_table_in_the_family_its_header_names() {
        let dir = scratch_dir("one-good");
        write(&dir, "Ramp Velocity.pal", RAMP_PAIR_VELOCITY);
        let library = UserTableLibrary::open(&dir);
        assert_eq!(library.tables().len(), 1);
        assert!(library.faults().is_empty());
        let entry = &library.tables()[0];
        assert_eq!(entry.display_name(), "Ramp Velocity");
        assert_eq!(entry.family(), ColorTableFamily::Velocity);
        assert_eq!(entry.file_name(), "Ramp Velocity.pal");
        // The stored name is what a settings file will hold, so it has to be
        // the name `base_name` returns.
        assert_eq!(entry.table().base_name(), "Ramp Velocity");
        // `Units: KTS` scaled the stops into m/s, which is the engine unit.
        assert!(
            entry
                .table()
                .stops()
                .last()
                .expect("stops exist")
                .value
                .abs()
                < 40.0,
            "knots must have been scaled to m/s"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that declares a `Name:` row is offered under THAT name, not
    /// under its filename, and the shared search finds the same file under the
    /// same name.
    ///
    /// The two used to disagree, and the disagreement was the seam between
    /// this scanner and the colour table editor: the editor writes a `Name:`
    /// row and finds files by it (`crate::palette_named_in`), while this scan
    /// named every table after its file stem. A palette called
    /// "Storm: Detail / v2" therefore appeared in the picker as
    /// "storm-detail-v2" - the lossy filename the editor had reduced it to -
    /// and installing that row stored a name nothing could resolve at the next
    /// launch, because no file declares it.
    #[test]
    fn a_file_is_offered_under_the_name_row_inside_it_and_the_shared_search_agrees() {
        let dir = scratch_dir("declared-name");
        write(
            &dir,
            "storm-detail-v2.pal",
            "Name: Storm: Detail / v2\nProduct: BV\nColor: -30 0 200 0\nColor: 30 200 0 0\n",
        );
        let library = UserTableLibrary::open(&dir);
        assert_eq!(library.tables().len(), 1);
        let entry = &library.tables()[0];
        assert_eq!(entry.display_name(), "Storm: Detail / v2");
        assert_eq!(entry.table().base_name(), "Storm: Detail / v2");
        assert_eq!(entry.file_name(), "storm-detail-v2.pal");
        assert!(
            library
                .table_for_family_named(ColorTableFamily::Velocity, "Storm: Detail / v2")
                .is_some(),
            "the name the picker offers has to be the name that resolves"
        );
        assert!(
            library
                .table_for_family_named(ColorTableFamily::Velocity, "storm-detail-v2")
                .is_none(),
            "the filename is a handle, not a name"
        );
        // And the search the editor and the launch-time restore both call
        // lands on the same file under the same name.
        let found = crate::palette_named_in(&dir, "Storm: Detail / v2")
            .expect("the shared search finds it");
        assert_eq!(found.path, entry.path());
        assert_eq!(found.table.stops(), entry.table().stops());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file with no `Name:` row is still offered under its stem, which is
    /// the only name a hand-dropped GR palette has.
    #[test]
    fn a_file_with_no_name_row_keeps_being_offered_under_its_stem() {
        let dir = scratch_dir("stem-name");
        write(&dir, "Field VEL.pal", RAMP_PAIR_VELOCITY);
        let library = UserTableLibrary::open(&dir);
        assert_eq!(library.tables()[0].display_name(), "Field VEL");
        assert_eq!(
            crate::palette_named_in(&dir, "Field VEL")
                .expect("the shared search finds it")
                .path,
            library.tables()[0].path()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_is_a_fault_with_its_line_and_costs_the_good_files_nothing() {
        let dir = scratch_dir("mixed");
        write(&dir, "good.pal", SIMPLE_REFLECTIVITY);
        // Line 3 asks for a colour component of 900.
        write(
            &dir,
            "broken.pal",
            "Product: BV\nColor: 0 0 0 0\nColor: 10 900 0 0\n",
        );
        // Two stops are the floor; one is not a table.
        write(&dir, "thin.pal", "Product: BR\nColor: 0 1 2 3\n");
        let library = UserTableLibrary::open(&dir);

        assert_eq!(library.tables().len(), 1);
        assert_eq!(library.tables()[0].display_name(), "good");

        let broken = library
            .faults()
            .iter()
            .find(|fault| fault.file_name() == "broken.pal")
            .expect("the broken file is reported");
        assert_eq!(broken.line(), Some(3));
        assert!(broken.reason().contains("0-255"), "{}", broken.reason());
        assert!(broken.to_string().contains("line 3"), "{broken}");

        let thin = library
            .faults()
            .iter()
            .find(|fault| fault.file_name() == "thin.pal")
            .expect("the too-short file is reported");
        assert_eq!(thin.line(), None, "no single line is to blame for that one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stem_that_is_already_a_built_in_name_is_marked_as_the_users() {
        let dir = scratch_dir("collision");
        let builtin = ColorTableSet::default()
            .for_family(ColorTableFamily::Reflectivity)
            .base_name()
            .to_owned();
        write(&dir, &format!("{builtin}.pal"), SIMPLE_REFLECTIVITY);
        let library = UserTableLibrary::open(&dir);
        assert_eq!(
            library.tables()[0].display_name(),
            format!("{builtin}{USER_NAME_SUFFIX}")
        );
        // And the marked name is still what `base_name` gives back, so a
        // stored choice resolves.
        assert_eq!(
            library.tables()[0].table().base_name(),
            library.tables()[0].display_name()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stem_that_mimics_a_rendering_marker_is_marked_too() {
        // `base_name` strips " (stepped)", so without the mark this table's
        // stored name and its display name would disagree for ever.
        let dir = scratch_dir("marker");
        write(&dir, "Mine (stepped).pal", SIMPLE_REFLECTIVITY);
        let library = UserTableLibrary::open(&dir);
        let entry = &library.tables()[0];
        assert_eq!(entry.display_name(), "Mine (stepped) (user)");
        assert_eq!(entry.table().base_name(), entry.display_name());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_files_with_one_stem_get_two_distinct_names_in_a_stable_order() {
        let dir = scratch_dir("two-stems");
        write(&dir, "mine.pal", SIMPLE_REFLECTIVITY);
        write(&dir, "mine.txt", SIMPLE_REFLECTIVITY);
        let names: Vec<String> = UserTableLibrary::open(&dir)
            .tables()
            .iter()
            .map(|entry| entry.display_name().to_owned())
            .collect();
        assert_eq!(names, ["mine", "mine (2)"]);
        // Scanned twice, the same order: the sort is what makes that true.
        let again: Vec<String> = UserTableLibrary::open(&dir)
            .tables()
            .iter()
            .map(|entry| entry.display_name().to_owned())
            .collect();
        assert_eq!(again, ["mine", "mine (2)"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_copies_into_the_folder_and_never_overwrites_what_is_there() {
        let dir = scratch_dir("import");
        let elsewhere = scratch_dir("import-source");
        let first = write(&elsewhere, "velocity.pal", RAMP_PAIR_VELOCITY);

        let mut library = UserTableLibrary::open(&dir);
        let outcome = library.import(&first);
        assert!(outcome.is_loaded(), "{outcome:?}");
        assert_eq!(library.tables().len(), 1);
        assert!(dir.join("velocity.pal").is_file());

        // A different file with the same name lands beside the first one,
        // and the first one's bytes are untouched.
        let second = write(&elsewhere, "velocity.pal", SIMPLE_REFLECTIVITY);
        let outcome = library.import(&second);
        match &outcome {
            ImportOutcome::Loaded { stored_as, .. } => assert_eq!(stored_as, "velocity (2).pal"),
            other => panic!("expected a numbered sibling, got {other:?}"),
        }
        assert_eq!(library.tables().len(), 2);
        assert!(
            std::fs::read_to_string(dir.join("velocity.pal"))
                .expect("first import survives")
                .contains("Units: KTS"),
            "the second import overwrote the first"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn importing_a_file_that_is_already_in_the_folder_does_not_duplicate_it() {
        let dir = scratch_dir("import-self");
        let inside = write(&dir, "mine.pal", SIMPLE_REFLECTIVITY);
        let mut library = UserTableLibrary::open(&dir);
        assert!(library.import(&inside).is_loaded());
        assert_eq!(library.tables().len(), 1);
        assert_eq!(
            std::fs::read_dir(&dir)
                .expect("folder listing")
                .filter_map(Result::ok)
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn importing_a_file_that_is_not_a_colour_table_says_so_and_files_nothing() {
        let dir = scratch_dir("import-broken");
        let elsewhere = scratch_dir("import-broken-source");
        let broken = write(
            &elsewhere,
            "notes.pal",
            "Product: BR\nColor: 0 0 0 0\nColor: 10 0 -4 0\n",
        );
        let mut library = UserTableLibrary::open(&dir);
        let outcome = library.import(&broken);

        match &outcome {
            ImportOutcome::Rejected {
                file_name,
                line,
                reason,
            } => {
                assert_eq!(file_name, "notes.pal");
                assert_eq!(*line, Some(3));
                assert!(reason.contains("0-255"), "{reason}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(outcome.status_line().contains("notes.pal"), "{outcome:?}");
        assert!(outcome.status_line().contains("line 3"), "{outcome:?}");
        assert!(library.tables().is_empty());
        assert!(!dir.join("notes.pal").exists(), "a refused file was filed");
        // And the analyst still has their file where they left it.
        assert!(broken.is_file());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn importing_a_file_that_cannot_be_read_says_so_rather_than_panicking() {
        let dir = scratch_dir("import-missing");
        let mut library = UserTableLibrary::open(&dir);
        let outcome = library.import(&dir.join("nothing-here.pal"));
        assert!(
            matches!(outcome, ImportOutcome::Unreadable { .. }),
            "{outcome:?}"
        );
        assert!(outcome.status_line().contains("nothing-here.pal"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rescan_bumps_the_generation_so_cached_picker_rows_are_dropped() {
        let dir = scratch_dir("generation");
        let mut library = UserTableLibrary::open(&dir);
        let before = library.generation();
        write(&dir, "later.pal", SIMPLE_REFLECTIVITY);
        library.refresh();
        assert_ne!(library.generation(), before);
        assert_eq!(library.tables().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_tables_are_offered_after_the_built_ins_in_the_installed_rendering() {
        let dir = scratch_dir("offers");
        write(&dir, "Ramp Velocity.pal", RAMP_PAIR_VELOCITY);
        let library = UserTableLibrary::open(&dir);
        let installed = ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .clone();
        let builtin_count = builtin_tables_for_family(ColorTableFamily::Velocity).len();
        let offers =
            palette_offers_with_user_tables(ColorTableFamily::Velocity, &installed, &library);

        assert_eq!(
            offers[builtin_count].base_name(),
            "Ramp Velocity",
            "the user table belongs after the catalogue, not inside it"
        );
        // Every row in the installed rendering, and one flip row at the end.
        assert_eq!(
            offers.last().expect("a flip row").rendering(),
            installed.rendering().flipped()
        );
        // Names stay unique - a picker identifies a row by its name.
        let mut names: Vec<&str> = offers.iter().map(ColorTable::name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two rows share a name");

        // And the flip works on the user table: install it, ask again.
        let stepped = library
            .table_for_family_named(ColorTableFamily::Velocity, "Ramp Velocity")
            .expect("the user table resolves by its stored name")
            .rendered(TableRendering::Stepped);
        assert_eq!(stepped.base_name(), "Ramp Velocity");
        assert_eq!(stepped.rendering(), TableRendering::Stepped);
        let flipped_offers =
            palette_offers_with_user_tables(ColorTableFamily::Velocity, &stepped, &library);
        assert!(
            flipped_offers
                .iter()
                .all(|table| table.base_name() != "Ramp Velocity"
                    || table.rendering() == TableRendering::Stepped
                    || table.rendering() == TableRendering::Smooth),
            "the user row must appear in one of the two renderings"
        );
        assert_eq!(
            flipped_offers
                .iter()
                .filter(|table| table.base_name() == "Ramp Velocity")
                .count(),
            2,
            "the installed user table plus its flip row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A ramp-pair `.pal`, read through the folder, keeps one stop per
    /// `Color:` row at that row's own colour.
    ///
    /// Dialect-invariant on purpose. In the GR2Analyst/RadarScope form a row
    /// carries TWO colours - its own and the far end of its interval - and
    /// what a reader does with the second one is a property of the parser,
    /// not of this scanner. What must hold either way is that the row's
    /// value keeps the row's first colour and that no row is dropped;
    /// asserting anything about the second colour here would pin one
    /// parser's answer inside the file-reading tests, where it does not
    /// belong.
    #[test]
    fn a_ramp_pair_palette_keeps_one_stop_per_row_at_the_rows_own_colour() {
        let dir = scratch_dir("ramp-pairs");
        write(&dir, "Pairs.pal", RAMP_PAIR_VELOCITY);
        let library = UserTableLibrary::open(&dir);
        let table = library.tables()[0].table();
        assert_eq!(
            table.stops().len(),
            7,
            "one stop per Color: row, whatever is done with the second colour"
        );
        // `Units: KTS` puts the values in m/s, the engine unit; the colour is
        // the row's first triple.
        let first = table.stops().first().expect("stops exist");
        assert!((first.value - -120.0 * 0.514_444).abs() < 0.01, "{first:?}");
        assert_eq!(first.color, crate::Rgba8::opaque(130, 0, 130));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_entry_that_is_not_a_palette_extension_is_ignored_entirely() {
        let dir = scratch_dir("extensions");
        write(&dir, "readme.md", "not a palette");
        write(&dir, "KDVN20260819_V06", "not a palette either");
        write(&dir, "good.pal", SIMPLE_REFLECTIVITY);
        let library = UserTableLibrary::open(&dir);
        assert_eq!(library.tables().len(), 1);
        assert!(
            library.faults().is_empty(),
            "a file that was never a palette is not a fault"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_uppercase_extension_is_read_like_any_other() {
        // Saving as `.PAL` is routine on Windows and macOS, and a palette
        // that is in the folder and not in the picker is precisely the
        // failure the fault list exists to make diagnosable. Nothing else in
        // this module exercises the scanner's case fold - the drop path's
        // `is_colour_table_drop` is a different function - so drop the
        // `to_ascii_lowercase` out of `has_table_extension` and, without
        // this, the whole suite stays green.
        let dir = scratch_dir("shouty");
        write(&dir, "Shouty.PAL", SIMPLE_REFLECTIVITY);
        write(&dir, "Mixed.Txt", SIMPLE_REFLECTIVITY);
        write(&dir, "quiet.pal", SIMPLE_REFLECTIVITY);
        let names: Vec<String> = UserTableLibrary::open(&dir)
            .tables()
            .iter()
            .map(|entry| entry.display_name().to_owned())
            .collect();
        assert_eq!(names, ["Mixed", "quiet", "Shouty"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_file_keeps_the_display_name_its_own_position_earned() {
        // The pin for the file -> display-name BINDING, which
        // `two_files_with_one_stem_get_two_distinct_names_in_a_stable_order`
        // cannot see: its two files are identical, so the pair of names it
        // asserts is the same pair whichever way the scan is sorted. Give
        // the two files different colours and the binding shows - reverse
        // the comparator and a settings file that stores "mine" starts
        // resolving to the other palette, silently.
        let dir = scratch_dir("binding");
        write(
            &dir,
            "mine.pal",
            "Product: BR\nColor: 0 0 0 0\nColor: 40 200 200 0\n",
        );
        write(
            &dir,
            "mine.txt",
            "Product: BR\nColor: 0 0 0 0\nColor: 40 9 9 9\n",
        );
        let library = UserTableLibrary::open(&dir);
        let bound: Vec<(&str, &str)> = library
            .tables()
            .iter()
            .map(|entry| (entry.file_name(), entry.display_name()))
            .collect();
        assert_eq!(bound, [("mine.pal", "mine"), ("mine.txt", "mine (2)")]);

        // And each name reaches its own file's colours, which is the half a
        // stored palette choice actually depends on.
        let top_colour = |name: &str| {
            library
                .table_for_family_named(ColorTableFamily::Reflectivity, name)
                .unwrap_or_else(|| panic!("{name} resolves"))
                .stops()
                .last()
                .expect("stops exist")
                .color
        };
        assert_eq!(top_colour("mine"), crate::Rgba8::opaque(200, 200, 0));
        assert_eq!(top_colour("mine (2)"), crate::Rgba8::opaque(9, 9, 9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinct_stems_are_offered_in_alphabetical_order() {
        // Written in an order no sort would produce by accident. A picker
        // list that reshuffles between launches is the thing the module doc
        // promises against, and nothing else here would notice a reversed
        // comparator: with distinct stems the numbering never fires, so the
        // names come out the same set either way.
        let dir = scratch_dir("alphabetical");
        write(&dir, "zulu.pal", SIMPLE_REFLECTIVITY);
        write(&dir, "alpha.pal", SIMPLE_REFLECTIVITY);
        write(&dir, "mike.pal", SIMPLE_REFLECTIVITY);
        let names: Vec<String> = UserTableLibrary::open(&dir)
            .tables()
            .iter()
            .map(|entry| entry.display_name().to_owned())
            .collect();
        assert_eq!(names, ["alpha", "mike", "zulu"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbering_is_per_family_so_two_picker_lists_do_not_push_each_others_numbers() {
        let dir = scratch_dir("per-family");
        write(&dir, "Mine.pal", SIMPLE_REFLECTIVITY);
        write(&dir, "Mine.txt", RAMP_PAIR_VELOCITY);
        let library = UserTableLibrary::open(&dir);
        let bound: Vec<(&str, &str, ColorTableFamily)> = library
            .tables()
            .iter()
            .map(|entry| (entry.file_name(), entry.display_name(), entry.family()))
            .collect();
        assert_eq!(
            bound,
            [
                ("Mine.pal", "Mine", ColorTableFamily::Reflectivity),
                ("Mine.txt", "Mine", ColorTableFamily::Velocity),
            ],
            "two files that never share a picker list must not renumber each other"
        );
        // Each list still names exactly one table, which is the property the
        // numbering exists for in the first place.
        assert!(
            library
                .table_for_family_named(ColorTableFamily::Reflectivity, "Mine")
                .is_some()
        );
        assert!(
            library
                .table_for_family_named(ColorTableFamily::Velocity, "Mine")
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_folder_is_not_read_again() {
        // The whole point of the listing short circuit: this runs on the UI
        // thread on every window focus regain, so a folder nobody touched
        // must cost one directory listing and no parses. The observable
        // proof is the generation, which is what every cached picker list
        // keys on - re-reading would drop those caches for nothing.
        let dir = scratch_dir("unchanged");
        write(&dir, "one.pal", SIMPLE_REFLECTIVITY);
        // A file written a moment ago carries a stamp the scan cannot vouch
        // for, and is deliberately re-read (see the racy-timestamp guard).
        // Sat through on the real clock rather than stamped away, because
        // the property being pinned here is that the guard lets go by
        // itself: a folder goes quiet again shortly after it is edited, and
        // the wait it asks for is one a test can sit through.
        assert!(
            SNAPSHOT_TRUST_MARGIN <= Duration::from_secs(2),
            "a long margin would make every alt-tab after an edit a re-read"
        );
        settle_past_the_trust_margin(&dir);
        let mut library = UserTableLibrary::open(&dir);
        let after_open = library.generation();
        let reads_after_open = library.files_read;
        assert_eq!(library.tables().len(), 1);

        assert!(
            !library.refresh(),
            "an untouched folder must not be read again"
        );
        assert_eq!(library.generation(), after_open);
        assert_eq!(
            library.files_read, reads_after_open,
            "and not one file was opened to work that out"
        );

        // And it is a listing comparison, not a scan-once flag: the very
        // next refresh after the folder moves reads all of it.
        write(&dir, "two.pal", SIMPLE_REFLECTIVITY);
        assert!(library.refresh(), "a changed folder must be read");
        assert_eq!(library.tables().len(), 2);
        assert_ne!(library.generation(), after_open);

        // A file whose contents changed under a new length is a change too.
        let after_two = library.generation();
        write(
            &dir,
            "one.pal",
            "Product: BR\nColor: 0 0 0 0\nColor: 40 1 2 3\nColor: 75 4 5 6\n",
        );
        assert!(library.refresh(), "an edited file must be read");
        assert_ne!(library.generation(), after_two);

        // The explicit gesture re-reads whether or not the listing moved,
        // because "Rescan" is an instruction, not a question.
        let before_reread = library.generation();
        library.reread();
        assert_ne!(library.generation(), before_reread);
        assert_eq!(library.tables().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_stamped_in_the_scans_own_moment_is_read_again_rather_than_trusted() {
        // The racy-timestamp guard, and `git`'s answer to the same problem:
        // an index entry whose stamp is not strictly older than the moment
        // the index was written is "racily clean" and gets looked at again.
        // Without it, an analyst who saves a palette twice inside one clock
        // tick - which on Windows is a 15 ms window, and on any platform is
        // what a script that writes a file twice does - keeps the colours
        // from the first save on screen until they find the Rescan button,
        // with nothing said.
        let dir = scratch_dir("racy-stamp");
        let path = write(&dir, "Zulu.pal", SIMPLE_REFLECTIVITY);
        let stamp = stamp_the_scan_cannot_vouch_for(&path);
        let mut library = UserTableLibrary::open(&dir);
        assert_eq!(last_stop_colour(&library, "Zulu"), (255, 255, 255));

        // The second save: same byte count, different bytes, and a stamp
        // that has not moved because the clock has not either.
        assert_eq!(SIMPLE_REFLECTIVITY.len(), SIMPLE_REFLECTIVITY_EDITED.len());
        std::fs::write(&path, SIMPLE_REFLECTIVITY_EDITED).expect("edit fixture");
        set_stamp(&path, stamp);

        assert!(
            library.refresh(),
            "a listing row the scan could not vouch for must be read again, not believed"
        );
        assert_eq!(
            last_stop_colour(&library, "Zulu"),
            (254, 254, 254),
            "the analyst's second save has to be the one on screen"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rescan_button_re_reads_a_folder_whose_listing_never_moved() {
        // The escape hatch, pinned as one. The listing cannot see a change
        // that keeps the byte count AND carries a back-dated stamp - this
        // module says so in as many words rather than pretending otherwise -
        // so the way out has to exist, has to read every file, and must not
        // consult the listing on the way past.
        let dir = scratch_dir("rescan-escape");
        let path = write(&dir, "Zulu.pal", SIMPLE_REFLECTIVITY);
        stamped_a_while_ago(&path);
        let mut library = UserTableLibrary::open(&dir);
        let reads_after_open = library.files_read;
        let after_open = library.generation();

        // Produced exactly the way a timestamp-preserving copy produces it.
        let stamp = stamp_of(&path);
        std::fs::write(&path, SIMPLE_REFLECTIVITY_EDITED).expect("edit fixture");
        set_stamp(&path, stamp);

        assert!(
            !library.refresh(),
            "the documented edge: this is invisible to a listing, by contract"
        );
        assert_eq!(library.files_read, reads_after_open, "nothing was opened");
        assert_eq!(last_stop_colour(&library, "Zulu"), (255, 255, 255));
        assert_eq!(library.generation(), after_open);

        library.reread();
        assert!(
            library.files_read > reads_after_open,
            "Rescan must actually open the folder's files, not compare listings"
        );
        assert_eq!(
            last_stop_colour(&library, "Zulu"),
            (254, 254, 254),
            "one click has to be enough to get the analyst's own edit on screen"
        );
        assert_ne!(
            library.generation(),
            after_open,
            "and every cached picker list has to be dropped with it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_that_keeps_the_length_but_moves_the_stamp_is_read_again() {
        // The modification time is half the listing key and the half that
        // was carrying no assertions: set `modified: None` in
        // `scan_directory` and everything else here stayed green, which
        // would have left every same-length edit invisible.
        let dir = scratch_dir("stamp-half-of-the-key");
        let path = write(&dir, "Zulu.pal", SIMPLE_REFLECTIVITY);
        stamped_a_while_ago(&path);
        let mut library = UserTableLibrary::open(&dir);
        assert_eq!(last_stop_colour(&library, "Zulu"), (255, 255, 255));

        // An ordinary save of an edited palette: the bytes change, the byte
        // COUNT does not, and the stamp moves. Length can see none of this.
        assert_eq!(SIMPLE_REFLECTIVITY.len(), SIMPLE_REFLECTIVITY_EDITED.len());
        std::fs::write(&path, SIMPLE_REFLECTIVITY_EDITED).expect("edit fixture");

        assert!(
            library.refresh(),
            "a moved modification time is a changed folder"
        );
        assert_eq!(last_stop_colour(&library, "Zulu"), (254, 254, 254));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_table_is_followed_like_any_other_file() {
        // `ln -s ~/Dropbox/palettes/Mine.pal <config>/colortables/` is the
        // ordinary way to keep palettes in sync on the Unix platforms this
        // workspace targets. `DirEntry::metadata` describes the LINK and not
        // the file it points at, so deciding "is this a file" with it makes
        // a linked palette produce neither a table nor a fault row - the
        // exact "it is in the folder and not in the picker" report the fault
        // list exists to answer.
        let dir = scratch_dir("symlink-folder");
        let elsewhere = scratch_dir("symlink-target");
        let target = write(&elsewhere, "Shared.pal", SIMPLE_REFLECTIVITY);
        stamped_a_while_ago(&target);
        let link = dir.join("Linked.pal");
        if let Err(reason) = link_file(&target, &link) {
            eprintln!("SKIPPED a_symlinked_table_is_followed_like_any_other_file: {reason}");
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&elsewhere);
            return;
        }
        let mut library = UserTableLibrary::open(&dir);
        assert_eq!(
            library.tables().len(),
            1,
            "a linked palette is a palette; faults: {:?}",
            library.faults()
        );
        assert!(library.faults().is_empty(), "{:?}", library.faults());
        assert_eq!(library.tables()[0].display_name(), "Linked");
        assert_eq!(last_stop_colour(&library, "Linked"), (255, 255, 255));

        // And the listing describes the file at the far end of the link, so
        // an edit made over there is an edit this folder can see.
        std::fs::write(&target, SIMPLE_REFLECTIVITY_EDITED).expect("edit the linked file");
        assert!(library.refresh(), "the linked file moved");
        assert_eq!(last_stop_colour(&library, "Linked"), (254, 254, 254));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_file_name_that_is_not_valid_unicode_is_read_from_its_own_path() {
        // Rebuilding the path from `to_string_lossy` names a file that does
        // not exist, so a palette that loaded perfectly well becomes a fault
        // row reading "the system cannot find the file specified" - and two
        // names differing only in bytes no spelling can show would collapse
        // into one entry. The listing carries the real path instead.
        let dir = scratch_dir("lossy-name");
        let path = dir.join(a_name_no_spelling_can_show());
        if std::fs::write(&path, SIMPLE_REFLECTIVITY).is_err() {
            eprintln!(
                "SKIPPED a_file_name_that_is_not_valid_unicode_is_read_from_its_own_path: this \
                 filesystem will not take such a name"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let library = UserTableLibrary::open(&dir);
        assert_eq!(
            library.tables().len(),
            1,
            "the file is a perfectly good palette; faults: {:?}",
            library.faults()
        );
        assert!(library.faults().is_empty(), "{:?}", library.faults());
        let entry = &library.tables()[0];
        assert_eq!(
            entry.path(),
            path,
            "the entry must carry the path the directory handed over"
        );
        assert_eq!(
            entry.table().stops().last().expect("stops exist").color.r,
            255
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_scan_reads_up_to_its_budget_and_the_rest_are_faults_naming_it() {
        // The per-file cap bounds one file at 2 MB and nothing more: twenty
        // files that are each just legal are forty megabytes of reading and
        // parsing on the UI thread. This is the other half of the bound, and
        // the files it turns away are named rather than dropped in silence.
        let dir = scratch_dir("scan-budget");
        let each = MAX_TABLE_BYTES as usize;
        let fits = (MAX_SCAN_BYTES / MAX_TABLE_BYTES) as usize;
        let padded = padded_palette(each);
        for index in 0..=fits {
            write(&dir, &format!("Pad {index}.pal"), &padded);
        }

        let library = UserTableLibrary::open(&dir);
        assert_eq!(
            library.tables().len(),
            fits,
            "the scan reads its budget and stops"
        );
        assert_eq!(library.faults().len(), 1, "{:?}", library.faults());
        let fault = &library.faults()[0];
        assert_eq!(
            fault.file_name(),
            format!("Pad {fits}.pal"),
            "the budget is spent in name order, so the same files load every time"
        );
        assert!(
            fault.reason().contains(&describe_size(MAX_SCAN_BYTES)),
            "the fault has to name the budget it ran into: {fault}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drop_past_the_cap_is_turned_away_by_its_size_before_its_bytes_are_asked_for() {
        // Two things at once, both about the same half-second. The size is
        // asked of the filesystem before the bytes are asked for, so a
        // mis-dragged half-gigabyte file costs one `metadata` call rather
        // than half a gigabyte read onto the UI thread and thrown away. And
        // the answer says what is actually wrong: a 3 MB palette in a
        // dialect this build reads fluently is not "not a colour table".
        let dir = scratch_dir("big-drop-folder");
        let elsewhere = scratch_dir("big-drop-source");
        let source = write(
            &elsewhere,
            "Big.pal",
            &padded_palette(MAX_TABLE_BYTES as usize + 1),
        );
        let mut library = UserTableLibrary::open(&dir);

        // Where the platform allows it: the size stays visible and the bytes
        // do not come. Checking the cap first still answers "too large";
        // reading first could only answer "could not be read".
        let denial = deny_reads(&source);
        let denied = denial.is_some();
        let outcome = library.import(&source);
        drop(denial);
        if !denied {
            eprintln!(
                "NOTE a_drop_past_the_cap_is_turned_away_by_its_size_before_its_bytes_are_asked_\
                 for: this environment would not hold the file's bytes back, so the ordering is \
                 asserted through the outcome alone"
            );
        }

        match &outcome {
            ImportOutcome::TooLarge { file_name, len } => {
                assert_eq!(file_name, "Big.pal");
                assert_eq!(*len, MAX_TABLE_BYTES + 1);
            }
            other => panic!("expected the size to turn it away first, got {other:?}"),
        }
        let line = outcome.status_line();
        assert!(line.contains("too large for this build"), "{line}");
        assert!(line.contains(&describe_size(MAX_TABLE_BYTES)), "{line}");
        assert!(
            !line.contains("is not a colour table"),
            "it IS a colour table; it is only too big: {line}"
        );
        assert!(outcome.is_problem(), "an analyst has to act on this one");
        assert!(!dir.join("Big.pal").exists(), "nothing oversize is filed");
        assert!(library.tables().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_file_past_the_size_cap_is_a_fault_naming_its_size_and_costs_the_others_nothing() {
        // `.txt` is admitted because shared palettes arrive with it, which
        // is exactly why a pile of notes can turn up wearing one. Reading
        // and parsing a 50 MB one froze the window on every alt-tab; now it
        // is one line in the fault list, beside its size.
        let dir = scratch_dir("oversize");
        write(&dir, "good.pal", SIMPLE_REFLECTIVITY);
        std::fs::write(dir.join("notes.txt"), vec![b'x'; 3 * 1024 * 1024]).expect("write notes");
        let library = UserTableLibrary::open(&dir);

        assert_eq!(library.tables().len(), 1, "the good file still loads");
        let fault = library
            .faults()
            .iter()
            .find(|fault| fault.file_name() == "notes.txt")
            .expect("the oversize file is reported, not silently skipped");
        assert!(fault.reason().contains("3.0 MB"), "{fault}");
        assert!(fault.reason().contains("2.0 MB"), "{fault}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drop_of_bytes_already_in_the_folder_files_nothing_and_says_which_table_it_is() {
        let dir = scratch_dir("duplicate");
        let elsewhere = scratch_dir("duplicate-source");
        let source = write(&elsewhere, "Ramp.pal", RAMP_PAIR_VELOCITY);

        let mut library = UserTableLibrary::open(&dir);
        assert!(library.import(&source).is_loaded());

        // The same file again - the obvious thing to do when you are not
        // sure the first drop landed, and what re-dropping an unedited
        // palette after a look in a text editor is.
        let outcome = library.import(&source);
        match &outcome {
            ImportOutcome::AlreadyImported {
                display_name,
                stored_as,
                family,
                ..
            } => {
                assert_eq!(display_name, "Ramp");
                assert_eq!(stored_as, "Ramp.pal");
                assert_eq!(*family, ColorTableFamily::Velocity);
            }
            other => panic!("expected the duplicate to be recognised, got {other:?}"),
        }
        let line = outcome.status_line();
        assert!(line.contains("already imported as"), "{line}");
        assert!(
            !outcome.is_problem(),
            "a palette the analyst already has is not a problem to report"
        );
        assert_eq!(library.tables().len(), 1, "nothing new was filed");
        assert!(!dir.join("Ramp (2).pal").exists());

        // The other half of the rule is untouched: DIFFERENT bytes under a
        // name that is taken still land beside what is already there.
        let different = write(&elsewhere, "Ramp.pal", SIMPLE_REFLECTIVITY);
        match library.import(&different) {
            ImportOutcome::Loaded { stored_as, .. } => assert_eq!(stored_as, "Ramp (2).pal"),
            other => panic!("expected a numbered sibling, got {other:?}"),
        }
        assert_eq!(library.tables().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }
}
