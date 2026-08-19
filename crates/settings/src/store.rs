//! The persisted settings store: load once, edit in memory, save atomically
//! and debounced.
//!
//! Failure policy, stated once and honoured everywhere:
//!
//! * **Opening never fails.** A missing file is first run; a corrupt file is
//!   moved aside (`settings.json.corrupt`) and defaults apply; an unreadable
//!   file (permissions, a directory in the way) applies defaults and
//!   *disables autosave*, because writing over a file we could not read would
//!   destroy whatever it held. [`SettingsStore::status`] reports which of
//!   these happened so the application can say so in its status line.
//! * **Saving is atomic.** The document is written to a sibling temp file and
//!   renamed over the target (`std::fs::rename` replaces on every platform
//!   this ships to), so a crash mid-save leaves the previous file intact.
//! * **Saving is debounced.** Callers mark changes, and
//!   [`SettingsStore::autosave_tick`] (called once per UI frame) writes only
//!   after [`SAVE_DEBOUNCE`] of quiet, or [`SAVE_MAX_LATENCY`] after the
//!   oldest unsaved change while a slider is still moving. Nothing here ever
//!   writes per frame.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use crate::document::{FORMAT_VERSION, SettingsDocument, WorkspaceSnapshot};
use crate::registry::SettingsRegistry;
use crate::value::SettingValue;

/// Quiet time after the last change before an autosave fires. Long enough
/// that a slider drag coalesces to one write, short enough that pulling the
/// power cord thirty seconds after a change loses nothing.
pub const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Ceiling on how stale the file may go while changes keep arriving (a held
/// slider never goes quiet). After this long with unsaved changes, save
/// anyway.
pub const SAVE_MAX_LATENCY: Duration = Duration::from_secs(20);

/// How the file on disk was read at open.
#[derive(Clone, Debug, PartialEq)]
pub enum LoadStatus {
    /// No file existed; every value is a default. First run.
    Defaults,
    /// The file loaded.
    Loaded,
    /// The file existed but did not parse. It was moved to `backup` (or left
    /// in place if the move itself failed) and defaults apply. The next save
    /// writes a fresh file.
    Recovered { backup: Option<PathBuf> },
    /// The file could not be read at all - not "did not parse", could not be
    /// *read*. Defaults apply and autosave is disabled so the unreadable file
    /// is not overwritten; an explicit [`SettingsStore::save_now`] still
    /// works, because that is someone deciding.
    Unreadable { error: String },
}

/// See the module documentation.
pub struct SettingsStore {
    path: PathBuf,
    document: SettingsDocument,
    status: LoadStatus,
    dirty: bool,
    last_change_at: Option<Instant>,
    oldest_unsaved_at: Option<Instant>,
    /// The last save failure, kept for the status line. Cleared by a
    /// successful save.
    last_save_error: Option<String>,
}

impl SettingsStore {
    /// Open the store at `path`, loading the file if there is one. Never
    /// fails; see [`LoadStatus`] for what happened instead.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (document, status) = load_document(&path);
        Self {
            path,
            document,
            status,
            dirty: false,
            last_change_at: None,
            oldest_unsaved_at: None,
            last_save_error: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn status(&self) -> &LoadStatus {
        &self.status
    }

    pub fn last_save_error(&self) -> Option<&str> {
        self.last_save_error.as_deref()
    }

    /// The raw stored value, if the file holds one this build can read.
    /// Most callers want [`Self::effective`] instead.
    pub fn value(&self, category: &str, id: &str) -> Option<SettingValue> {
        let json = self.document.values.get(category)?.get(id)?;
        SettingValue::from_json(json)
    }

    /// The value the application should act on: stored if valid, clamped to
    /// the declared range, otherwise the declared default. Returns `None`
    /// only for a `(category, id)` the registry does not declare, which is a
    /// caller typo - a test over the real catalog enumerates every key.
    pub fn effective(
        &self,
        registry: &SettingsRegistry,
        category: &str,
        id: &str,
    ) -> Option<SettingValue> {
        let spec = registry.setting(category, id)?;
        Some(spec.kind.sanitize(self.value(category, id).as_ref()))
    }

    pub fn effective_bool(&self, registry: &SettingsRegistry, category: &str, id: &str) -> bool {
        self.effective(registry, category, id)
            .and_then(|value| value.as_bool())
            .unwrap_or_default()
    }

    pub fn effective_float(&self, registry: &SettingsRegistry, category: &str, id: &str) -> f64 {
        self.effective(registry, category, id)
            .and_then(|value| value.as_float())
            .unwrap_or_default()
    }

    pub fn effective_int(&self, registry: &SettingsRegistry, category: &str, id: &str) -> i64 {
        self.effective(registry, category, id)
            .and_then(|value| value.as_int())
            .unwrap_or_default()
    }

    /// For `Choice` and `Text` kinds: the resolved string.
    pub fn effective_text(&self, registry: &SettingsRegistry, category: &str, id: &str) -> String {
        self.effective(registry, category, id)
            .and_then(|value| value.as_text().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Store a value. Returns whether anything actually changed; an
    /// unchanged write does not dirty the store, so calling this every frame
    /// with the same value never causes a save.
    pub fn set(&mut self, category: &str, id: &str, value: SettingValue) -> bool {
        let json = value.to_json();
        let slot = self
            .document
            .values
            .entry(category.to_owned())
            .or_default()
            .entry(id.to_owned());
        let changed = match &slot {
            std::collections::btree_map::Entry::Occupied(existing) => *existing.get() != json,
            std::collections::btree_map::Entry::Vacant(_) => true,
        };
        if changed {
            *slot.or_insert(Json::Null) = json;
            self.mark_changed();
        }
        changed
    }

    /// Remove a stored value so the default applies again.
    pub fn reset(&mut self, category: &str, id: &str) -> bool {
        let removed = self
            .document
            .values
            .get_mut(category)
            .is_some_and(|values| values.remove(id).is_some());
        if removed {
            self.mark_changed();
        }
        removed
    }

    /// Remove every stored value in a category. Values under ids the registry
    /// does not know (a future build's) are removed too - "restore defaults"
    /// means the page, not the subset this build understands.
    pub fn reset_category(&mut self, category: &str) -> bool {
        let removed = self
            .document
            .values
            .remove(category)
            .is_some_and(|values| !values.is_empty());
        if removed {
            self.mark_changed();
        }
        removed
    }

    pub fn workspace(&self) -> &WorkspaceSnapshot {
        &self.document.workspace
    }

    /// Replace the workspace snapshot. Compares first, so calling this every
    /// frame with an unchanged snapshot never causes a save.
    pub fn set_workspace(&mut self, workspace: WorkspaceSnapshot) -> bool {
        // A rebuilt snapshot cannot know about fields a future build stored,
        // so they are carried across the replacement rather than silently
        // dropped with the old snapshot. Same for unknown palette entries.
        let mut workspace = workspace;
        if workspace.unknown.is_empty() {
            workspace.unknown = self.document.workspace.unknown.clone();
        }
        for (family, previous) in &self.document.workspace.palettes {
            if !workspace.palettes.contains_key(family) {
                workspace.palettes.insert(family.clone(), previous.clone());
            }
        }
        let changed = self.document.workspace != workspace;
        if changed {
            self.document.workspace = workspace;
            self.mark_changed();
        }
        changed
    }

    fn mark_changed(&mut self) {
        let now = Instant::now();
        self.dirty = true;
        self.last_change_at = Some(now);
        self.oldest_unsaved_at.get_or_insert(now);
    }

    /// Whether there are unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Call once per UI frame. Saves when the debounce policy says to;
    /// returns what happened so the caller can surface a failure. `None`
    /// means "nothing was due", the overwhelmingly common case, and costs
    /// two comparisons.
    pub fn autosave_tick(&mut self) -> Option<io::Result<()>> {
        if !self.dirty {
            return None;
        }
        if matches!(self.status, LoadStatus::Unreadable { .. }) {
            // See LoadStatus::Unreadable: do not overwrite a file that could
            // not be read.
            return None;
        }
        let now = Instant::now();
        let since_change = self
            .last_change_at
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or(Duration::MAX);
        let since_oldest = self
            .oldest_unsaved_at
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or(Duration::MAX);
        if !save_due(since_change, since_oldest) {
            return None;
        }
        Some(self.save_now())
    }

    /// Save immediately and atomically. For shutdown paths and for tests;
    /// frame-loop callers use [`Self::autosave_tick`].
    pub fn save_now(&mut self) -> io::Result<()> {
        // Preserve a higher version from a future build; see the document.
        self.document.version = self.document.version.max(FORMAT_VERSION);
        let result = write_atomically(&self.path, &self.document);
        match &result {
            Ok(()) => {
                self.dirty = false;
                self.last_change_at = None;
                self.oldest_unsaved_at = None;
                self.last_save_error = None;
                // Whatever went wrong at open, the file on disk is now this
                // store's own writing.
                self.status = LoadStatus::Loaded;
            }
            Err(error) => {
                self.last_save_error = Some(error.to_string());
            }
        }
        result
    }
}

/// The debounce policy, pure so it is testable without sleeping:
/// save after [`SAVE_DEBOUNCE`] of quiet, or once changes have been waiting
/// [`SAVE_MAX_LATENCY`] even if they are still coming.
pub fn save_due(since_last_change: Duration, since_oldest_unsaved: Duration) -> bool {
    since_last_change >= SAVE_DEBOUNCE || since_oldest_unsaved >= SAVE_MAX_LATENCY
}

fn load_document(path: &Path) -> (SettingsDocument, LoadStatus) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (SettingsDocument::default(), LoadStatus::Defaults);
        }
        Err(error) => {
            return (
                SettingsDocument::default(),
                LoadStatus::Unreadable {
                    error: error.to_string(),
                },
            );
        }
    };
    match serde_json::from_str::<SettingsDocument>(&text) {
        Ok(document) => (document, LoadStatus::Loaded),
        Err(_) => {
            // Move the bad file aside rather than deleting it: it may hold a
            // recoverable hand edit, and the analyst can look at it. Best
            // effort - if the move fails the file simply stays until the next
            // successful save replaces it.
            let backup_path = corrupt_backup_path(path);
            let backup = std::fs::rename(path, &backup_path)
                .ok()
                .map(|_| backup_path);
            (
                SettingsDocument::default(),
                LoadStatus::Recovered { backup },
            )
        }
    }
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".corrupt");
    path.with_file_name(name)
}

fn write_atomically(path: &Path, document: &SettingsDocument) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(document).map_err(io::Error::other)?;
    text.push('\n');
    // Process-id suffix so two instances racing do not write through each
    // other's temp file; the final rename is last-writer-wins either way,
    // which for a settings file is the correct resolution.
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    let write_result = (|| -> io::Result<()> {
        // One writable handle for write and sync both: on Windows,
        // `sync_all` (FlushFileBuffers) is refused on a read-only handle, so
        // re-opening the file to sync it would fail with Access Denied.
        // Flushing to disk before the rename is what makes "the previous file
        // stays intact" true across a power cut, not just across a crash.
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if write_result.is_err() {
        // Do not leave temp droppings next to the settings file.
        let _ = std::fs::remove_file(&temp);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debounce_policy_saves_on_quiet_or_on_staleness_never_instantly() {
        // A change 10 ms ago with nothing older: too fresh, no save.
        assert!(!save_due(
            Duration::from_millis(10),
            Duration::from_millis(10)
        ));
        // Quiet for the debounce interval: save.
        assert!(save_due(SAVE_DEBOUNCE, SAVE_DEBOUNCE));
        // A slider still moving (recent change) but the oldest unsaved change
        // has waited out the latency ceiling: save anyway.
        assert!(save_due(Duration::from_millis(10), SAVE_MAX_LATENCY));
    }

    #[test]
    fn the_corrupt_backup_sits_beside_the_file_with_a_corrupt_suffix() {
        let path = Path::new("some/dir/settings.json");
        assert_eq!(
            corrupt_backup_path(path),
            Path::new("some/dir/settings.json.corrupt")
        );
    }
}
