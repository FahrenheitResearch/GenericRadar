//! Browsing for a radar file, in the application's own chrome.
//!
//! Until this existed there were two ways to open a file: drop it on the
//! window, or know its path and type it. Both assume the analyst already
//! knows which file they want. Somebody with a folder of volumes and a
//! question - "which of these has the couplet in it?" - had to leave the
//! application to answer it.
//!
//! # Why this is drawn here rather than asked of the operating system
//!
//! A native dialog would be one crate and one call. It is not used, for
//! three reasons, in ascending order of weight:
//!
//! * `crates/workstation_app/tests/architecture.rs` fixes this crate's
//!   direct dependencies, and every entry on that list carries a written
//!   reason for being there. A file dialog is not one of them;
//! * six platforms, six dialogs. The one drawn here behaves identically on
//!   all of them, including the ones with no desktop file manager to borrow;
//! * decisively: **an operating system's dialog cannot be photographed
//!   offscreen.** Every proof in this workspace works by rendering real
//!   chrome through the real egui pipeline into a PNG a human then looks at
//!   (`examples/file_browser_proof.rs` is this window's). A dialog owned by
//!   the window manager is invisible to that, so the one surface an analyst
//!   uses *before* seeing any radar would be the one surface nobody could
//!   check.
//!
//! # Identification is by content, never by extension
//!
//! `nexrad_io` routes on magic bytes, because that is the only thing that
//! works here: NEXRAD Archive II volumes are routinely stored with no
//! extension at all, a `.gz` is opened by whatever is inside it, and a
//! `.raw` from one network is a different format from a `.raw` from another.
//! So this browser reads the first [`HEAD_BYTES`] of each candidate and asks
//! [`nexrad_io::sniff_supported_volume_bytes`] what it is - the same seam the
//! loader routes through, so the column and the load cannot disagree.
//!
//! It reads the HEAD, never the file. A folder of 400 MB volumes costs 8 KiB
//! per file to list, not 400 MB, and the reading happens on a thread of its
//! own so a folder on a share that has stopped answering cannot freeze a
//! frame. Answers are cached per `(path, length, modified)`, so walking back
//! down a tree costs nothing the second time.
//!
//! A file nothing recognises is REPORTED, not hidden and not refused.
//! `sniff_supported_volume_bytes` returning `None` means "no signature
//! matched", which is not quite "not radar data" - an Archive II volume
//! whose tape identifier is neither `AR2V` nor `ARCHIVE2` is still worth
//! handing to the Level II parser, and that parser's own complaint is the
//! useful diagnostic. So the row says what was found, Open stays live, and
//! the decoder gets the last word.
//!
//! # What is remembered
//!
//! The folder, through the settings store, under `data/open_folder` - and
//! written only when a listing SUCCEEDED, so a folder that could not be read
//! is never the one the next session opens on.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use eframe::egui;
use nexrad_io::SupportedVolumeFormat;
use settings::{SettingValue, SettingsRegistry, SettingsStore};

use crate::settings_ui::catalog::keys;
use crate::theme::bevel;
use crate::units::UnitSystem;

/// How much of a file's head identification reads.
///
/// Two things set the floor. A DORADE sweepfile has no file-level magic, so
/// the sniff accepts its leading descriptor name only when the block length
/// beside it is no larger than the buffer it was handed - the real sweeps in
/// `nexrad_io/tests/data` open with a 508-byte `COMM` block, and a buffer
/// smaller than that would report a DORADE file as unrecognised. A gzipped
/// volume has to be inflated far enough to see the inner container's
/// signature, which is 512 bytes of output; radar data does not compress
/// anywhere near 16:1, so 8 KiB in always yields the 512 out.
///
/// The ceiling is what a directory costs: 8 KiB x 400 files is about 3 MiB
/// of reads, which is milliseconds on any disk and happens off the UI thread
/// regardless.
pub const HEAD_BYTES: usize = 8 * 1024;

/// How many identifications one message carries back to the UI.
///
/// Batched so a large folder does not send one channel message per file, and
/// small so the column fills in visibly as the scan walks rather than
/// arriving all at once at the end.
const IDENTIFY_BATCH: usize = 24;

/// How many identifications are remembered across navigations before the
/// cache is emptied. A radar archive is deep rather than wide; this is
/// several large folders' worth and bounds the memory at well under a
/// megabyte.
const CACHE_LIMIT: usize = 8_192;

/// How long a scan may be outstanding before the window says so.
///
/// A local folder answers in microseconds. Anything past this is a mount
/// that is not answering, and saying "still reading" is the difference
/// between a window that is working and a window that looks broken.
const SLOW_SCAN_NOTICE: Duration = Duration::from_millis(750);

/// Column widths, in points, at their widest.
const KIND_WIDTH: f32 = 158.0;
const SIZE_WIDTH: f32 = 78.0;
const MODIFIED_WIDTH: f32 = 186.0;
/// The gap between two columns.
const COLUMN_GAP: f32 = 10.0;
/// How little room the name column may be left with before a fixed column is
/// dropped instead. A radar file name is long and its distinguishing part -
/// the timestamp - is in the middle, so a name squeezed to nothing is a list
/// of identical prefixes.
const NAME_FLOOR: f32 = 190.0;

/// What one row of the listing knows about a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    /// Bytes. Zero for a directory, and zero for a file whose metadata could
    /// not be read - the identification then says why.
    pub len: u64,
    pub modified: Option<DateTime<Utc>>,
    pub identity: FileIdentity,
}

/// What the first bytes of a file said it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileIdentity {
    /// A directory, or a file the scan has not reached yet.
    Unread,
    /// The head named a container this application reads.
    Radar(SupportedVolumeFormat),
    /// The head was read and matched no radar signature.
    Unrecognised,
    /// The head could not be read at all. Carries the reason.
    Unreadable(String),
}

/// The column name for a format.
///
/// Deliberately NOT [`SupportedVolumeFormat::label`], which is written to
/// sit inside a sentence in an error message: "NEXRAD Level 1 time series
/// (RVP8/RVP900)" is right there and far too long for a table column that
/// also has to leave room for a file name. A `match` rather than a
/// truncation, so a format added to `nexrad_io` fails to compile here until
/// somebody chooses its short name, and
/// `every_format_has_a_short_name_that_fits_the_column` pins that they stay
/// distinct and stay short.
pub fn short_format_name(format: SupportedVolumeFormat) -> &'static str {
    match format {
        SupportedVolumeFormat::NexradLevel2 => "NEXRAD Level II",
        SupportedVolumeFormat::NexradLevel1TimeSeries => "NEXRAD Level 1 (I/Q)",
        SupportedVolumeFormat::MatlabIqCube => "MATLAB I/Q",
        SupportedVolumeFormat::OdimH5 => "ODIM_H5",
        SupportedVolumeFormat::Dorade => "DORADE",
        SupportedVolumeFormat::CfRadial1 => "CfRadial 1.x",
        SupportedVolumeFormat::MobileDeploymentZip => "Deployment archive",
    }
}

impl FileIdentity {
    /// What the "what this is" column says.
    pub fn column_text(&self) -> &'static str {
        match self {
            Self::Unread => "reading…",
            Self::Radar(format) => short_format_name(*format),
            Self::Unrecognised => "not radar data",
            Self::Unreadable(_) => "unreadable",
        }
    }

    /// The whole sentence, for the line under the list that describes
    /// whatever is selected. Here the format's own label is the right one:
    /// this is prose, and prose is what it was written for.
    pub fn sentence(&self) -> String {
        match self {
            Self::Unread => "still reading the first bytes of this file.".to_owned(),
            Self::Radar(format) => format!("{}. Opening it will decode it.", format.label()),
            Self::Unrecognised => format!(
                "nothing in the first {HEAD_BYTES} bytes names a radar container. Opening it \
                 hands the bytes to the Archive II decoder anyway, which will say what it found."
            ),
            Self::Unreadable(reason) => reason.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the disk. Both functions are plain and take a path, so a test
// drives exactly what the scan thread drives.
// ---------------------------------------------------------------------------

/// List one directory: names, kinds, sizes and times, sorted for reading.
///
/// Nothing is filtered out. A radar folder holds index files, notes and
/// screenshots beside the volumes, and hiding them by name would be a claim
/// this function cannot support - the identification column is where a file
/// is judged, and it judges by reading rather than by guessing.
pub fn read_directory(directory: &Path) -> Result<Vec<Entry>, String> {
    let listing = std::fs::read_dir(directory)
        .map_err(|error| format!("could not read {}: {error}", directory.display()))?;
    let mut entries = Vec::new();
    for entry in listing {
        // One unreadable entry does not lose the folder: skip it and keep
        // listing. Failing the whole directory because one name could not be
        // stat'ed would hide every file beside it.
        let Ok(entry) = entry else {
            continue;
        };
        let metadata = entry.metadata().ok();
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            directory: metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
            len: metadata.as_ref().map_or(0, std::fs::Metadata::len),
            modified: metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .map(DateTime::<Utc>::from),
            identity: FileIdentity::Unread,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// Folders first, then files; each in case-insensitive name order, ties
/// broken by the exact name.
///
/// Name order rather than newest-first, and the reason is what radar file
/// names are: every archive convention this application meets puts the
/// timestamp in the name (`KDVN20260819_192802_V06`,
/// `cfrad.20110520_...`), so alphabetical within a site IS chronological,
/// and it stays readable in a folder holding several sites. The exact name
/// is the final key so the order is total - two files differing only in case
/// cannot swap places between two listings of the same folder.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Read the head of one file and ask what it is.
///
/// [`HEAD_BYTES`] at most, and a short file is fine: the sniff judges
/// whatever it is given. An empty file reads as unrecognised, which is
/// exactly what it is.
pub fn identify(path: &Path) -> FileIdentity {
    use std::io::Read;

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return FileIdentity::Unreadable(format!("could not be opened: {error}")),
    };
    let mut head = vec![0_u8; HEAD_BYTES];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) => return FileIdentity::Unreadable(format!("could not be read: {error}")),
        }
    }
    head.truncate(filled);
    match nexrad_io::sniff_supported_volume_bytes(&head) {
        Some(format) => FileIdentity::Radar(format),
        None => FileIdentity::Unrecognised,
    }
}

// ---------------------------------------------------------------------------
// The scan, off the UI thread.
// ---------------------------------------------------------------------------

/// What identification remembers, keyed by the three facts that decide
/// whether a file is the same file: where it is, how long it is, and when it
/// was last written.
type IdentityCache = HashMap<(PathBuf, u64, Option<DateTime<Utc>>), FileIdentity>;

enum ScanUpdate {
    Listed {
        generation: u64,
        directory: PathBuf,
        entries: Vec<Entry>,
    },
    Refused {
        generation: u64,
        message: String,
    },
    Identified {
        generation: u64,
        items: Vec<(usize, FileIdentity)>,
    },
    Finished {
        generation: u64,
    },
}

/// One navigation's work: list the folder, then identify what is in it.
///
/// A thread per navigation rather than one long-lived worker, and that is
/// the deliberate answer to a mount that has stopped answering. `read_dir`
/// on a dead share blocks until the operating system gives up, which can be
/// tens of seconds; a single worker would be stuck for all of them and every
/// later navigation would queue behind it. Here the stuck thread simply
/// finishes late, its generation no longer matches, and its result is
/// dropped on arrival. Navigation stays live throughout, and a human
/// clicking folders cannot make threads faster than the operating system
/// retires them.
fn scan(
    generation: u64,
    directory: PathBuf,
    cache: &Mutex<IdentityCache>,
    updates: &Sender<ScanUpdate>,
    context: &egui::Context,
) {
    let entries = match read_directory(&directory) {
        Ok(entries) => entries,
        Err(message) => {
            let _ = updates.send(ScanUpdate::Refused {
                generation,
                message,
            });
            context.request_repaint();
            return;
        }
    };
    // The list first, the identities after: a folder appears the instant it
    // has been read, and the "what this is" column fills in behind it.
    let files = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.directory)
        .map(|(index, entry)| (index, entry.name.clone(), entry.len, entry.modified))
        .collect::<Vec<_>>();
    let _ = updates.send(ScanUpdate::Listed {
        generation,
        directory: directory.clone(),
        entries,
    });
    context.request_repaint();

    let mut batch = Vec::with_capacity(IDENTIFY_BATCH);
    for (index, name, len, modified) in files {
        let path = directory.join(&name);
        let key = (path.clone(), len, modified);
        let cached = cache.lock().ok().and_then(|cache| cache.get(&key).cloned());
        let identity = match cached {
            Some(identity) => identity,
            None => {
                let identity = identify(&path);
                if let Ok(mut cache) = cache.lock() {
                    if cache.len() >= CACHE_LIMIT {
                        cache.clear();
                    }
                    cache.insert(key, identity.clone());
                }
                identity
            }
        };
        batch.push((index, identity));
        if batch.len() >= IDENTIFY_BATCH {
            let _ = updates.send(ScanUpdate::Identified {
                generation,
                items: std::mem::take(&mut batch),
            });
            context.request_repaint();
        }
    }
    if !batch.is_empty() {
        let _ = updates.send(ScanUpdate::Identified {
            generation,
            items: batch,
        });
    }
    let _ = updates.send(ScanUpdate::Finished { generation });
    context.request_repaint();
}

// ---------------------------------------------------------------------------
// The browser itself.
// ---------------------------------------------------------------------------

/// How the current folder's listing is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingState {
    /// A scan is out and nothing has come back for this folder yet.
    Reading,
    /// The folder was read. Identification may still be filling in.
    Listed { identifying: bool },
    /// The folder could not be read, and this is what the system said.
    Refused(String),
}

/// The Open… window's whole state.
pub struct FileBrowser {
    /// Whether the window is on screen.
    pub open: bool,
    context: egui::Context,
    directory: PathBuf,
    /// The location strip's editable text. Held apart from `directory` so a
    /// half-typed path never sends the browser anywhere.
    location_text: String,
    entries: Vec<Entry>,
    state: ListingState,
    /// Selected rows, in listing order. Directories are always selected
    /// alone; files may be Ctrl/Command- or Shift-selected into a playlist.
    selected: BTreeSet<usize>,
    /// The row keyboard navigation and Shift-selection extend from.
    selection_focus: Option<usize>,
    selection_anchor: Option<usize>,
    /// Set when the selection moved by keyboard, so the row is scrolled into
    /// view on the next pass. A mouse-driven selection is already visible.
    scroll_to_selection: bool,
    /// Where the list was scrolled to when it was last drawn, so a keyboard
    /// move can be turned into the smallest scroll that brings the row back.
    scroll_offset: f32,
    /// What the browser has to say for itself: a file that vanished, a
    /// folder reader that would not start. Its own line, kept until
    /// something else happens.
    notice: Option<String>,
    generation: u64,
    scan_started: Option<Instant>,
    updates: Receiver<ScanUpdate>,
    sender: Sender<ScanUpdate>,
    cache: Arc<Mutex<IdentityCache>>,
}

impl FileBrowser {
    pub fn new(context: egui::Context) -> Self {
        let (sender, updates) = channel();
        Self {
            open: false,
            context,
            directory: PathBuf::new(),
            location_text: String::new(),
            entries: Vec::new(),
            state: ListingState::Reading,
            selected: BTreeSet::new(),
            selection_focus: None,
            selection_anchor: None,
            scroll_to_selection: false,
            scroll_offset: 0.0,
            notice: None,
            generation: 0,
            scan_started: None,
            updates,
            sender,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The folder the listing is of.
    #[cfg(test)]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// How the current folder's listing is doing.
    #[cfg(test)]
    pub fn state(&self) -> &ListingState {
        &self.state
    }

    /// The rows, as the window draws them.
    #[cfg(test)]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Select the first row whose name matches. Returns whether one did.
    #[cfg(test)]
    pub fn select_named(&mut self, name: &str) -> bool {
        let found = self.entries.iter().position(|entry| entry.name == name);
        self.selected.clear();
        if let Some(index) = found {
            self.selected.insert(index);
        }
        self.selection_focus = found;
        self.selection_anchor = found;
        found.is_some()
    }

    /// What the browser last had to say for itself, if anything.
    #[cfg(test)]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Open the window, on the remembered folder.
    ///
    /// Nothing here touches the disk. The stored folder is taken at its word
    /// and handed to the scan, which is the one place allowed to block: an
    /// `is_dir()` check here would be the UI thread's own call into a mount
    /// that may not answer, and would buy nothing the scan does not report
    /// better.
    ///
    /// `near` is the file the application currently has open, used only when
    /// nothing is stored: somebody who reached a volume by typing its path
    /// almost always wants the next one from beside it.
    pub fn show(
        &mut self,
        store: &SettingsStore,
        registry: &SettingsRegistry,
        near: Option<&Path>,
    ) {
        self.open = true;
        if !self.directory.as_os_str().is_empty() {
            // Already somewhere. Reopening the window does not send an
            // analyst back to the start of the session.
            return;
        }
        self.go_to(opening_directory(store, registry, near));
    }

    /// The folder `Open…` starts in, per the settings file.
    pub fn remembered_directory(store: &SettingsStore, registry: &SettingsRegistry) -> String {
        store.effective_text(registry, keys::data::CATEGORY, keys::data::OPEN_FOLDER)
    }

    /// Point the browser at a folder and start reading it.
    pub fn go_to(&mut self, directory: PathBuf) {
        self.directory = directory;
        self.location_text = self.directory.display().to_string();
        self.entries.clear();
        self.selected.clear();
        self.selection_focus = None;
        self.selection_anchor = None;
        self.scroll_offset = 0.0;
        self.state = ListingState::Reading;
        self.generation += 1;
        self.scan_started = Some(Instant::now());
        let generation = self.generation;
        let path = self.directory.clone();
        let cache = Arc::clone(&self.cache);
        let sender = self.sender.clone();
        let context = self.context.clone();
        let spawned = thread::Builder::new()
            .name("radar-workstation-browse".to_owned())
            .spawn(move || scan(generation, path, &cache, &sender, &context));
        if let Err(error) = spawned {
            // A thread that will not start is not a reason to lose the
            // window. Say it where a folder that will not read is said.
            self.state =
                ListingState::Refused(format!("could not start the folder reader: {error}"));
            self.scan_started = None;
        }
    }

    /// Re-read the current folder. Also what an Open on a file that has gone
    /// falls back to, because a folder that has changed under the listing is
    /// exactly the moment to read it again.
    pub fn refresh(&mut self) {
        let directory = self.directory.clone();
        self.go_to(directory);
    }

    /// Take whatever the scan threads have sent. Cheap and unconditional -
    /// call it every frame, open or not, so a scan that finishes after the
    /// window closed does not sit in the channel.
    pub fn poll(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
            match update {
                ScanUpdate::Listed {
                    generation,
                    directory,
                    entries,
                } => {
                    if generation != self.generation {
                        continue;
                    }
                    self.directory = directory;
                    self.location_text = self.directory.display().to_string();
                    self.entries = entries;
                    self.state = ListingState::Listed { identifying: true };
                }
                ScanUpdate::Refused {
                    generation,
                    message,
                } => {
                    if generation != self.generation {
                        continue;
                    }
                    self.state = ListingState::Refused(message);
                    self.scan_started = None;
                }
                ScanUpdate::Identified { generation, items } => {
                    if generation != self.generation {
                        continue;
                    }
                    for (index, identity) in items {
                        if let Some(entry) = self.entries.get_mut(index) {
                            entry.identity = identity;
                        }
                    }
                }
                ScanUpdate::Finished { generation } => {
                    if generation != self.generation {
                        continue;
                    }
                    if let ListingState::Listed { identifying } = &mut self.state {
                        *identifying = false;
                    }
                    self.scan_started = None;
                }
            }
        }
    }

    /// Select a row by index; out of range clears the selection.
    pub fn select(&mut self, index: usize) {
        self.selected.clear();
        self.selection_focus = (index < self.entries.len()).then_some(index);
        self.selection_anchor = self.selection_focus;
        if self.selection_focus.is_some() {
            self.selected.insert(index);
        }
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        (self.selected.len() == 1)
            .then(|| self.selected.first().copied())
            .flatten()
            .and_then(|index| self.entries.get(index))
    }

    fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Apply conventional file-list selection semantics. A folder cannot be
    /// mixed into a file playlist; choosing one always makes it the sole row.
    fn select_with_modifiers(&mut self, index: usize, modifiers: egui::Modifiers) {
        if index >= self.entries.len() {
            self.selected.clear();
            self.selection_focus = None;
            self.selection_anchor = None;
            return;
        }
        if self.entries[index].directory {
            self.select(index);
            return;
        }

        let toggle = modifiers.command || modifiers.ctrl;
        if modifiers.shift {
            let anchor = self
                .selection_anchor
                .or(self.selection_focus)
                .unwrap_or(index);
            if !toggle {
                self.selected.clear();
            }
            let (start, end) = if anchor <= index {
                (anchor, index)
            } else {
                (index, anchor)
            };
            for candidate in start..=end {
                if !self.entries[candidate].directory {
                    self.selected.insert(candidate);
                }
            }
            self.selection_focus = Some(index);
            return;
        }

        if toggle {
            if !self.selected.remove(&index) {
                self.selected.insert(index);
            }
            self.selection_focus = self
                .selected
                .contains(&index)
                .then_some(index)
                .or_else(|| self.selected.last().copied());
            self.selection_anchor = self.selection_focus;
        } else {
            self.select(index);
        }
    }

    /// Go to the parent folder. Pure path arithmetic - it never asks the
    /// disk anything, so it still works from inside a folder that could not
    /// be read.
    pub fn go_up(&mut self) -> bool {
        let Some(parent) = self.directory.parent().map(Path::to_path_buf) else {
            return false;
        };
        if parent.as_os_str().is_empty() {
            return false;
        }
        self.go_to(parent);
        true
    }

    /// Whether there is anywhere above here to go.
    pub fn can_go_up(&self) -> bool {
        self.directory
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    }

    /// Act on the selected rows: walk into one folder, or hand back the files
    /// to open as a sequence in their visible listing order.
    ///
    /// The file is checked for still being there first, and that check is
    /// not ceremony: a live archive gains and loses files while a listing is
    /// on screen, and the difference between "this is gone" said here, with
    /// the folder re-read underneath it, and a decode failure said in the
    /// status line after the window has closed is the difference between a
    /// browser and a trapdoor.
    pub fn activate_selection(&mut self) -> Option<Vec<PathBuf>> {
        let indices = self.selected.iter().copied().collect::<Vec<_>>();
        if indices.is_empty() {
            return None;
        }
        if indices.len() == 1 {
            let entry = self.entries.get(indices[0])?.clone();
            if entry.directory {
                self.go_to(self.directory.join(entry.name));
                return None;
            }
        }

        let mut paths = Vec::with_capacity(indices.len());
        for index in indices {
            let entry = self.entries.get(index)?.clone();
            let path = self.directory.join(&entry.name);
            if let Err(error) = std::fs::metadata(&path) {
                self.notice = Some(format!(
                    "{} is no longer there ({error}). The folder has been read again.",
                    entry.name
                ));
                self.refresh();
                return None;
            }
            paths.push(path);
        }
        self.notice = None;
        Some(paths)
    }

    /// Move the selection by `delta` rows, clamped to the list.
    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        let next = match self.selection_focus {
            Some(current) => (current as isize + delta).clamp(0, last),
            None if delta >= 0 => 0,
            None => last,
        };
        self.select(next as usize);
        self.scroll_to_selection = true;
    }

    /// The scroll offset that brings the selected row into a viewport of
    /// `view_height`, moving as little as possible. `None` when there is
    /// nothing to bring into view.
    fn scroll_offset_for_selection(&self, row_height: f32, view_height: f32) -> Option<f32> {
        let selected = self.selection_focus?;
        let top = selected as f32 * row_height;
        let bottom = top + row_height;
        let mut offset = self.scroll_offset;
        if top < offset {
            offset = top;
        }
        if bottom > offset + view_height {
            offset = bottom - view_height;
        }
        Some(offset.max(0.0))
    }

    /// Store the current folder as the one to open on next time. Called only
    /// once a listing has succeeded.
    fn remember(&self, store: &mut SettingsStore) {
        store.set(
            keys::data::CATEGORY,
            keys::data::OPEN_FOLDER,
            SettingValue::Text(self.directory.display().to_string()),
        );
    }
}

/// Where a browser with nothing stored starts.
fn opening_directory(
    store: &SettingsStore,
    registry: &SettingsRegistry,
    near: Option<&Path>,
) -> PathBuf {
    let remembered = FileBrowser::remembered_directory(store, registry);
    if !remembered.trim().is_empty() {
        return PathBuf::from(remembered.trim());
    }
    if let Some(parent) = near.and_then(Path::parent)
        && !parent.as_os_str().is_empty()
    {
        return parent.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Drawing.
// ---------------------------------------------------------------------------

/// What the window needs from the application to draw itself.
pub struct FileBrowserInput<'a> {
    /// The analyst's clock and zone, so a modification time in this window
    /// reads the same way every other time in the application does.
    pub units: UnitSystem,
    /// Written to, not read from: the browser stores the folder it is
    /// looking at every time one reads successfully. The registry is not
    /// needed here - a value that has been set has an effective value
    /// without consulting a default - and `FileBrowser::show` takes it
    /// separately for the one lookup that does need it.
    pub store: &'a mut SettingsStore,
}

/// What the window decided this frame.
#[derive(Default)]
pub struct FileBrowserOutcome {
    /// Files the analyst chose, in visible file-name order. One path is an
    /// ordinary load; several are a sequence of separate timeline frames.
    pub open: Vec<PathBuf>,
}

/// The Open… window. Returns the ordered files to load, if any were chosen.
pub fn draw_file_browser(
    context: &egui::Context,
    browser: &mut FileBrowser,
    input: FileBrowserInput<'_>,
) -> FileBrowserOutcome {
    browser.poll();
    let mut outcome = FileBrowserOutcome::default();
    if !browser.open {
        return outcome;
    }
    // The window never outgrows the display, on either axis - the same rule
    // the settings window keeps, and for the same reason: this ships to a
    // phone-shaped display too.
    let screen = context.content_rect();
    let max_width = (screen.width() - 24.0).clamp(280.0, 1_100.0);
    let max_height = (screen.height() - 48.0).max(240.0);
    let mut open = browser.open;
    egui::Window::new("Open radar files")
        .open(&mut open)
        .default_size([760.0_f32.min(max_width), 520.0])
        .max_size([max_width, max_height])
        .resizable(true)
        .show(context, |ui| {
            ui.spacing_mut().interact_size.y = ui.spacing().interact_size.y.max(MIN_ROW_HEIGHT);
            // Escape shuts it, the way every dialog on every platform this
            // ships to does - and consumed here, inside the window's own
            // body, so the chord exists only while the window is up and the
            // rest of the application never sees the key.
            if ui.input_mut(|state| state.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                browser.open = false;
            }
            draw_body(ui, browser, &input, &mut outcome);
        });
    if !browser.open {
        open = false;
    }
    if !outcome.open.is_empty() {
        open = false;
    }
    // The folder is worth remembering the moment it has been read
    // successfully, not when the window closes: a session that ends in a
    // crash still reopens where it was.
    if matches!(browser.state, ListingState::Listed { .. }) {
        browser.remember(input.store);
    }
    browser.open = open;
    outcome
}

/// The touch floor every interactive row holds itself to, matching the rest
/// of the chrome.
const MIN_ROW_HEIGHT: f32 = bevel::MIN_TOUCH_POINTS;

fn draw_body(
    ui: &mut egui::Ui,
    browser: &mut FileBrowser,
    input: &FileBrowserInput<'_>,
    outcome: &mut FileBrowserOutcome,
) {
    egui::Panel::top("file-browser-location").show_inside(ui, |ui| {
        draw_location_strip(ui, browser);
    });
    egui::Panel::bottom("file-browser-actions").show_inside(ui, |ui| {
        draw_actions(ui, browser, outcome);
    });
    draw_listing(ui, browser, input, outcome);
}

fn draw_location_strip(ui: &mut egui::Ui, browser: &mut FileBrowser) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(browser.can_go_up(), egui::Button::new("Up"))
            .clicked()
        {
            browser.go_up();
        }
        if ui.button("Refresh").clicked() {
            browser.refresh();
        }
        let go_width = 44.0;
        // The field takes what is left. A path is long and the window is
        // resizable, so a fixed width would either waste the room or hide
        // the end of every path.
        let width = (ui.available_width() - go_width - 12.0).max(80.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut browser.location_text)
                .desired_width(width)
                .hint_text("Folder"),
        );
        let entered = field.lost_focus() && ui.input(|state| state.key_pressed(egui::Key::Enter));
        if ui.button("Go").clicked() || entered {
            let typed = PathBuf::from(browser.location_text.trim());
            browser.go_to(typed);
        }
    });
    ui.add_space(2.0);
}

fn draw_actions(ui: &mut egui::Ui, browser: &mut FileBrowser, outcome: &mut FileBrowserOutcome) {
    ui.add_space(2.0);
    // What the selection is, in words. Above the buttons rather than beside
    // them: it is a sentence, and a sentence beside a button sets the
    // window's minimum width to its own length.
    let selected = browser.selected_entry().cloned();
    let selected_count = browser.selected_count();
    let line = match (&selected, selected_count) {
        (_, count) if count > 1 => format!(
            "{count} files selected. They load in file-name order as separate timeline frames."
        ),
        (Some(entry), _) if entry.directory => format!("{} - a folder.", entry.name),
        (Some(entry), _) => format!("{} - {}", entry.name, entry.identity.sentence()),
        _ => "Nothing selected. Ctrl/Command-click or Shift-click selects a file playlist."
            .to_owned(),
    };
    ui.add(egui::Label::new(egui::RichText::new(line).small().weak()).wrap());
    if let Some(notice) = browser.notice.clone() {
        ui.add(
            egui::Label::new(
                egui::RichText::new(notice)
                    .small()
                    .color(ui.visuals().error_fg_color),
            )
            .wrap(),
        );
    }
    ui.horizontal(|ui| {
        let label = match (&selected, selected_count) {
            (Some(entry), 1) if entry.directory => "Open folder".to_owned(),
            (_, count) if count > 1 => format!("Open {count} files as playlist"),
            _ => "Open".to_owned(),
        };
        if ui
            .add_enabled(selected_count > 0, egui::Button::new(label))
            .clicked()
            && let Some(paths) = browser.activate_selection()
        {
            outcome.open = paths;
        }
        if ui.button("Cancel").clicked() {
            browser.open = false;
        }
    });
    ui.add_space(2.0);
}

fn draw_listing(
    ui: &mut egui::Ui,
    browser: &mut FileBrowser,
    input: &FileBrowserInput<'_>,
    outcome: &mut FileBrowserOutcome,
) {
    // Keys only when nothing is being typed into: the location field owns
    // the arrows and Enter while it has the caret.
    let typing = ui.memory(|memory| memory.focused().is_some());
    let (up, down, enter) = ui.input(|state| {
        (
            state.key_pressed(egui::Key::ArrowUp),
            state.key_pressed(egui::Key::ArrowDown),
            state.key_pressed(egui::Key::Enter),
        )
    });
    if !typing && up {
        browser.move_selection(-1);
    }
    if !typing && down {
        browser.move_selection(1);
    }
    let mut activate = !typing && enter;

    match browser.state.clone() {
        ListingState::Refused(message) => {
            bevel::sunken_well(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(message).color(ui.visuals().error_fg_color),
                    )
                    .wrap(),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(
                            "Up still works from here, and a path can be typed into the folder \
                             field above.",
                        )
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
            });
            return;
        }
        ListingState::Reading => {
            let slow = browser
                .scan_started
                .is_some_and(|started| started.elapsed() >= SLOW_SCAN_NOTICE);
            bevel::sunken_well(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add(
                        egui::Label::new(if slow {
                            "Still reading this folder. A network folder that has stopped \
                             answering holds it until the operating system gives up; this \
                             window stays live throughout."
                        } else {
                            "Reading this folder…"
                        })
                        .wrap(),
                    );
                });
            });
            return;
        }
        ListingState::Listed { .. } => {}
    }

    if browser.entries.is_empty() {
        bevel::sunken_well(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label("Nothing in this folder.");
        });
        return;
    }

    let row_height = ui.spacing().interact_size.y.max(MIN_ROW_HEIGHT);
    let scroll_to = std::mem::take(&mut browser.scroll_to_selection);
    let mut clicked = None;
    let mut double_clicked = None;
    let selected = browser.selected.clone();
    // Taken out of the browser for the pass so the rows can be drawn while
    // the browser itself stays borrowable; put straight back below.
    let entries = std::mem::take(&mut browser.entries);
    let forced_offset = scroll_to
        .then(|| browser.scroll_offset_for_selection(row_height, ui.available_height()))
        .flatten();
    bevel::sunken_well(ui, |ui| {
        ui.set_min_width(ui.available_width());
        let columns = columns_for(ui.available_width());
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(offset) = forced_offset {
            area = area.vertical_scroll_offset(offset);
        }
        let output = area.show_rows(ui, row_height, entries.len(), |ui, range| {
            for index in range {
                let response = draw_row(
                    ui,
                    &entries[index],
                    selected.contains(&index),
                    &columns,
                    row_height,
                    input.units,
                );
                if response.clicked() {
                    clicked = Some((index, ui.input(|input| input.modifiers)));
                }
                if response.double_clicked() {
                    double_clicked = Some(index);
                }
            }
        });
        browser.scroll_offset = output.state.offset.y;
    });
    browser.entries = entries;
    if let Some((index, modifiers)) = clicked {
        browser.select_with_modifiers(index, modifiers);
    }
    if let Some(index) = double_clicked {
        browser.select(index);
        activate = true;
    }
    if activate && let Some(paths) = browser.activate_selection() {
        outcome.open = paths;
    }
}

/// The fixed columns a row has room for. A width of zero is a column this
/// window is too narrow to draw.
struct Columns {
    kind: f32,
    size: f32,
    modified: f32,
}

impl Columns {
    fn fixed_width(&self) -> f32 {
        let mut total = 0.0;
        for width in [self.kind, self.size, self.modified] {
            if width > 0.0 {
                total += width + COLUMN_GAP;
            }
        }
        total
    }
}

/// Which columns fit, widest first.
///
/// The kind column is never dropped. It is the answer this browser exists to
/// give, and a listing that has run out of room for it is a listing of names,
/// which is what the analyst already had in the operating system's own file
/// manager.
fn columns_for(available: f32) -> Columns {
    let mut columns = Columns {
        kind: KIND_WIDTH,
        size: SIZE_WIDTH,
        modified: MODIFIED_WIDTH,
    };
    // Modified goes first: a radar file name almost always carries its own
    // timestamp, so it is the column that repeats what is already on screen.
    if available - columns.fixed_width() < NAME_FLOOR {
        columns.modified = 0.0;
    }
    if available - columns.fixed_width() < NAME_FLOOR {
        columns.size = 0.0;
    }
    columns
}

/// One row: a full-width hit target with the name truncated and the fixed
/// columns to the right of it.
///
/// Painted rather than composed out of stock widgets because a table has
/// widths: a row built from labels sizes itself to its longest cell, and one
/// ninety-character deployment archive name would then set the width of the
/// whole window.
fn draw_row(
    ui: &mut egui::Ui,
    entry: &Entry,
    selected: bool,
    columns: &Columns,
    row_height: f32,
    units: UnitSystem,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let palette = crate::theme::chrome(ui).palette;
    if selected {
        ui.painter().rect_filled(rect, 0.0, palette.selection_bg);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 0.0, palette.hover);
    }
    let ink = if selected {
        palette.selection_text
    } else {
        palette.text
    };
    // Size and time are supporting facts, so they are drawn in the weaker
    // ink - except on the selected row, where the selection fill is the
    // ground and only `selection_text` is pinned legible on it.
    let weak_ink = if selected {
        palette.selection_text
    } else {
        palette.text_weak
    };

    let name_width = (width - columns.fixed_width() - COLUMN_GAP).max(40.0);
    let mut x = rect.min.x + COLUMN_GAP * 0.5;
    // A trailing separator is how a folder has been marked in a listing
    // since long before any of this, and it survives a screenshot in a way a
    // tinted row does not.
    let name = if entry.directory {
        format!("{}{}", entry.name, std::path::MAIN_SEPARATOR)
    } else {
        entry.name.clone()
    };
    x = paint_cell(ui, &name, x, name_width, rect, ink, false);
    if columns.kind > 0.0 {
        let text = if entry.directory {
            "Folder"
        } else {
            entry.identity.column_text()
        };
        // A recognised format, and a folder, are statements; "not radar
        // data" and "unreadable" are absences, and are drawn in the weaker
        // ink so a folder of volumes reads as a folder of volumes.
        let kind_ink = match &entry.identity {
            _ if entry.directory => ink,
            FileIdentity::Radar(_) => ink,
            _ => weak_ink,
        };
        x = paint_cell(ui, text, x, columns.kind, rect, kind_ink, false);
    }
    if columns.size > 0.0 {
        let text = if entry.directory {
            String::new()
        } else {
            format_size(entry.len)
        };
        x = paint_cell(ui, &text, x, columns.size, rect, weak_ink, true);
    }
    if columns.modified > 0.0 {
        let text = entry
            .modified
            .map(|time| units.time(time))
            .unwrap_or_default();
        let _ = paint_cell(ui, &text, x, columns.modified, rect, weak_ink, false);
    }
    response
}

/// Paint one cell and return where the next one starts.
fn paint_cell(
    ui: &egui::Ui,
    text: &str,
    x: f32,
    width: f32,
    row: egui::Rect,
    ink: egui::Color32,
    right_aligned: bool,
) -> f32 {
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        width,
        egui::TextStyle::Body,
    );
    let left = if right_aligned {
        x + (width - galley.size().x).max(0.0)
    } else {
        x
    };
    let position = egui::pos2(left, row.center().y - galley.size().y * 0.5);
    ui.painter().galley(position, galley, ink);
    x + width + COLUMN_GAP
}

/// A file size as an analyst reads it: `512 B`, `18.4 KiB`, `74.2 MiB`,
/// `1.4 GiB`.
///
/// Binary units, one decimal above a kilobyte. Radar volumes are quoted in
/// MiB everywhere else in this application - the history ceiling, the live
/// cache, the moment sizes `nexrad_io`'s inspector prints - so a browser
/// quoting MB would be the one surface disagreeing.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    for (ceiling, unit) in [
        (KIB * KIB, "KiB"),
        (KIB * KIB * KIB, "MiB"),
        (KIB * KIB * KIB * KIB, "GiB"),
    ] {
        if value < ceiling {
            return format!("{:.1} {unit}", value / (ceiling / KIB));
        }
    }
    format!("{:.1} TiB", value / (KIB * KIB * KIB * KIB))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own, removed on the way in so a previous
    /// run cannot seed it. Never a real folder of anybody's: this test suite
    /// has been bitten before by a test that read the machine it was written
    /// on.
    fn scratch(what: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "radar-workstation-browser-{what}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// The short names are what the column can actually print. A name that
    /// does not fit is a name that gets truncated in every screenshot.
    #[test]
    fn every_format_has_a_short_name_that_fits_the_column() {
        let formats = [
            SupportedVolumeFormat::NexradLevel2,
            SupportedVolumeFormat::NexradLevel1TimeSeries,
            SupportedVolumeFormat::MatlabIqCube,
            SupportedVolumeFormat::OdimH5,
            SupportedVolumeFormat::Dorade,
            SupportedVolumeFormat::CfRadial1,
            SupportedVolumeFormat::MobileDeploymentZip,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for format in formats {
            let short = short_format_name(format);
            assert!(!short.is_empty(), "{format:?} has no short name");
            assert!(
                short.len() <= 22,
                "{format:?}: {short:?} is {} characters, which will not fit the kind column",
                short.len()
            );
            assert!(
                seen.insert(short),
                "{format:?}: {short:?} is already another format's name"
            );
            // And the long label is still the one the sentence uses, so the
            // two vocabularies stay separate on purpose rather than by
            // accident.
            assert!(!format.label().is_empty());
        }
    }

    #[test]
    fn sizes_read_the_way_the_rest_of_the_application_writes_them() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(11_195_445), "10.7 MiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn folders_sort_above_files_and_the_order_is_total() {
        let entry = |name: &str, directory: bool| Entry {
            name: name.to_owned(),
            directory,
            len: 0,
            modified: None,
            identity: FileIdentity::Unread,
        };
        let mut entries = vec![
            entry("zulu", false),
            entry("Alpha", false),
            entry("alpha", false),
            entry("mike", true),
            entry("Bravo", true),
        ];
        sort_entries(&mut entries);
        let names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Bravo", "mike", "Alpha", "alpha", "zulu"]);
    }

    /// The whole point of the browser: the column tells the truth about a
    /// file with no extension, and about a decoy with a radar-looking name.
    #[test]
    fn identification_reads_the_bytes_and_not_the_name() {
        let dir = scratch("identify");
        // A real Archive II tape identifier, extensionless, which is exactly
        // how NEXRAD volumes arrive.
        std::fs::write(
            dir.join("KDVN20260819_192802_V06"),
            b"AR2V0006.473 whatever",
        )
        .expect("write the volume");
        // The decoy: the same shape of name, and a text file inside.
        std::fs::write(
            dir.join("KDVN20260819_192802_V06.txt"),
            b"notes about the couplet, taken during the event\n",
        )
        .expect("write the decoy");
        std::fs::write(dir.join("empty"), b"").expect("write the empty file");
        std::fs::write(
            dir.join("research-cube.without-mat-extension"),
            b"MATLAB 5.0 MAT-file synthetic header",
        )
        .expect("write MATLAB signature");

        assert_eq!(
            identify(&dir.join("KDVN20260819_192802_V06")),
            FileIdentity::Radar(SupportedVolumeFormat::NexradLevel2)
        );
        assert_eq!(
            identify(&dir.join("KDVN20260819_192802_V06.txt")),
            FileIdentity::Unrecognised
        );
        assert_eq!(identify(&dir.join("empty")), FileIdentity::Unrecognised);
        assert_eq!(
            identify(&dir.join("research-cube.without-mat-extension")),
            FileIdentity::Radar(SupportedVolumeFormat::MatlabIqCube),
            "MATLAB identity comes from the Level 5 header, not the suffix"
        );
        let missing = identify(&dir.join("never-written"));
        assert!(
            matches!(missing, FileIdentity::Unreadable(_)),
            "a file that is not there must say so, not read as unrecognised: {missing:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A DORADE sweep has no file-level magic; the sniff leans on the
    /// leading block length fitting in the buffer it was given, which is a
    /// direct claim about [`HEAD_BYTES`].
    #[test]
    fn the_head_is_long_enough_for_a_dorade_leading_block() {
        let dir = scratch("dorade");
        let mut sweep = Vec::new();
        sweep.extend_from_slice(b"COMM");
        sweep.extend_from_slice(&508_i32.to_le_bytes());
        sweep.resize(64 * 1024, 0);
        let path = dir.join("swp.1090509143923.NOXPRVP.0.0.5_PPI_v1");
        std::fs::write(&path, &sweep).expect("write the sweep");
        assert_eq!(
            identify(&path),
            FileIdentity::Radar(SupportedVolumeFormat::Dorade),
            "HEAD_BYTES = {HEAD_BYTES} is too small for a DORADE leading block"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_read_says_so_rather_than_returning_nothing() {
        let dir = scratch("refused");
        let missing = dir.join("no-such-folder");
        let refusal = read_directory(&missing).expect_err("a missing folder cannot be listed");
        assert!(
            refusal.contains("could not read") && refusal.contains("no-such-folder"),
            "the refusal has to name the folder and the reason: {refusal}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_listing_carries_names_kinds_sizes_and_times() {
        let dir = scratch("listing");
        std::fs::create_dir(dir.join("subfolder")).expect("create the subfolder");
        std::fs::write(dir.join("volume"), vec![0_u8; 4096]).expect("write the file");
        let entries = read_directory(&dir).expect("the scratch folder lists");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].directory && entries[0].name == "subfolder");
        assert!(!entries[1].directory && entries[1].name == "volume");
        assert_eq!(entries[1].len, 4096);
        assert!(
            entries[1].modified.is_some(),
            "a file just written has a modification time"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The kind column never goes, however narrow the window gets; the
    /// others do, in the stated order.
    #[test]
    fn narrow_windows_drop_columns_but_never_the_one_that_answers_the_question() {
        let wide = columns_for(900.0);
        assert!(wide.kind > 0.0 && wide.size > 0.0 && wide.modified > 0.0);

        let medium = columns_for(560.0);
        assert!(medium.kind > 0.0 && medium.size > 0.0);
        assert_eq!(medium.modified, 0.0, "the time goes first");

        let narrow = columns_for(340.0);
        assert!(narrow.kind > 0.0, "the kind column is never dropped");
        assert_eq!(narrow.size, 0.0);
        assert_eq!(narrow.modified, 0.0);

        // And whatever is dropped, the name is never squeezed below the
        // floor while a droppable column is still being drawn.
        for width in [280.0_f32, 340.0, 420.0, 560.0, 720.0, 900.0, 1_100.0] {
            let columns = columns_for(width);
            let name = width - columns.fixed_width();
            assert!(
                name >= NAME_FLOOR || (columns.size == 0.0 && columns.modified == 0.0),
                "at {width} points the name gets {name} and columns are still being drawn"
            );
        }
    }

    /// Walking up is path arithmetic, so it works from inside a folder that
    /// does not exist - which is exactly the folder somebody needs to get
    /// out of.
    #[test]
    fn going_up_works_from_a_folder_that_could_not_be_read() {
        let mut browser = FileBrowser::new(egui::Context::default());
        let dir = scratch("go-up");
        browser.go_to(dir.join("no-such-folder"));
        assert!(browser.can_go_up());
        assert!(browser.go_up());
        assert_eq!(browser.directory(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Choosing a file that has gone in the meantime keeps the window up,
    /// says what happened, and re-reads the folder.
    #[test]
    fn a_file_that_disappeared_is_reported_rather_than_silently_failed() {
        let dir = scratch("vanished");
        let path = dir.join("volume");
        std::fs::write(&path, b"AR2V0006.473").expect("write the volume");
        let mut browser = FileBrowser::new(egui::Context::default());
        browser.go_to(dir.clone());
        pump(&mut browser);
        assert!(browser.select_named("volume"), "the file was listed");

        std::fs::remove_file(&path).expect("take the file away");
        assert_eq!(
            browser.activate_selection(),
            None,
            "a file that is gone must not be handed to the loader"
        );
        let notice = browser.notice().expect("the browser says what happened");
        assert!(
            notice.contains("volume") && notice.contains("no longer there"),
            "{notice}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn control_selection_returns_a_filename_ordered_file_playlist() {
        let dir = scratch("playlist");
        for name in ["KTLX_003", "KTLX_001", "KTLX_002"] {
            std::fs::write(dir.join(name), b"AR2V0006.473").expect("write a listed volume");
        }
        let mut browser = FileBrowser::new(egui::Context::default());
        browser.go_to(dir.clone());
        pump(&mut browser);

        assert!(browser.select_named("KTLX_003"));
        let first = browser
            .entries()
            .iter()
            .position(|entry| entry.name == "KTLX_001")
            .expect("first file is listed");
        let middle = browser
            .entries()
            .iter()
            .position(|entry| entry.name == "KTLX_002")
            .expect("middle file is listed");
        browser.select_with_modifiers(
            first,
            egui::Modifiers {
                ctrl: true,
                ..Default::default()
            },
        );
        browser.select_with_modifiers(
            middle,
            egui::Modifiers {
                ctrl: true,
                ..Default::default()
            },
        );

        let chosen = browser
            .activate_selection()
            .expect("three selected files become a playlist");
        assert_eq!(
            chosen,
            ["KTLX_001", "KTLX_002", "KTLX_003"]
                .into_iter()
                .map(|name| dir.join(name))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The scan runs off the UI thread and streams back; this is the loop a
    /// test uses in place of frames.
    fn pump(browser: &mut FileBrowser) {
        for _ in 0..2_000 {
            browser.poll();
            if matches!(
                browser.state(),
                ListingState::Listed { identifying: false } | ListingState::Refused(_)
            ) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the folder scan never finished");
    }

    /// End to end, through the real thread: a folder with a radar file and a
    /// decoy in it comes back identified, and the identities are the ones
    /// the bytes support.
    #[test]
    fn a_scan_identifies_a_whole_folder_without_blocking_the_caller() {
        let dir = scratch("scan");
        std::fs::create_dir(dir.join("older")).expect("create the subfolder");
        std::fs::write(dir.join("KDVN20260819_192802_V06"), b"AR2V0006.473 x")
            .expect("write the volume");
        std::fs::write(dir.join("field-notes.txt"), b"nothing radar about this")
            .expect("write the decoy");
        let mut browser = FileBrowser::new(egui::Context::default());
        browser.go_to(dir.clone());
        pump(&mut browser);

        let named = |name: &str| {
            browser
                .entries()
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("{name} should be in the listing"))
                .clone()
        };
        assert!(named("older").directory);
        assert_eq!(
            named("KDVN20260819_192802_V06").identity,
            FileIdentity::Radar(SupportedVolumeFormat::NexradLevel2)
        );
        assert_eq!(
            named("field-notes.txt").identity,
            FileIdentity::Unrecognised
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second visit to the same folder is answered from the cache, so
    /// walking down a tree and back does not re-read every head.
    #[test]
    fn identification_is_cached_per_path_length_and_time() {
        let dir = scratch("cache");
        for index in 0..8 {
            std::fs::write(dir.join(format!("volume{index}")), b"AR2V0006.473 x")
                .expect("write a volume");
        }
        let mut browser = FileBrowser::new(egui::Context::default());
        browser.go_to(dir.clone());
        pump(&mut browser);
        let cached = browser
            .cache
            .lock()
            .expect("the cache is not poisoned")
            .len();
        assert_eq!(cached, 8, "every file should have been remembered");

        // Same folder again: the cache answers, and the entries still come
        // back identified.
        browser.refresh();
        pump(&mut browser);
        assert!(browser.entries().iter().all(
            |entry| entry.identity == FileIdentity::Radar(SupportedVolumeFormat::NexradLevel2)
        ));
        assert_eq!(
            browser
                .cache
                .lock()
                .expect("the cache is not poisoned")
                .len(),
            8,
            "a second listing of the same files must not add cache entries"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stale scan's answer is dropped rather than painted over the folder
    /// the analyst has since moved to.
    #[test]
    fn an_answer_from_an_abandoned_scan_is_discarded() {
        let first = scratch("stale-first");
        let second = scratch("stale-second");
        std::fs::write(first.join("one"), b"AR2V0006.473").expect("write one");
        std::fs::write(second.join("two"), b"AR2V0006.473").expect("write two");

        let mut browser = FileBrowser::new(egui::Context::default());
        browser.go_to(first.clone());
        // Move on before polling: the first scan's messages are in the
        // channel and belong to a generation that no longer exists.
        browser.go_to(second.clone());
        pump(&mut browser);
        assert_eq!(browser.directory(), second);
        let names = browser
            .entries()
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["two".to_owned()]);
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }
}
