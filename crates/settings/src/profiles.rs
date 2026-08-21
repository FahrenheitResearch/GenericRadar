//! Named settings profiles: one install, several ways of working.
//!
//! A profile is a **named snapshot of the whole settings document** - a chase
//! setup, an office setup, a presentation setup - kept in its own file beside
//! `settings.json` so it can be read, hand-edited, copied to another machine
//! or mailed to a colleague.
//!
//! # Switching goes through the document, never through a list of keys
//!
//! The one design decision everything else follows from: switching profiles
//! replaces the settings *document* and then re-runs the application's
//! ordinary apply path over it. It does not carry a list of the settings a
//! profile contains.
//!
//! A hand-written list would be wrong the day after it was written. New
//! releases add knobs - a theme catalogue here, a gate filter there - and
//! each one is added to the catalog and to the apply
//! path because that is what makes it work at all. A profile switch that
//! enumerated keys would silently stop carrying each new setting, and the
//! failure would be invisible: the profile would still switch, still say it
//! had switched, and simply not move the three newest knobs. Because the
//! switch replaces the document instead, a setting this build has never heard
//! of - for example, one written by a future build - is carried by the file,
//! restored into the document, and applied by whatever
//! wiring its own author gave it. **If you are here to add "and also copy
//! X" - stop. X is already copied. What may be missing is X's apply path,
//! which is the same thing that would be missing when a settings file is
//! hand-edited, and it belongs with X.**
//!
//! # What a profile deliberately does not carry
//!
//! Three parts of the document belong to the *install*, not to a way of
//! working, and [`snapshot_for_profile`] strips them:
//!
//! * `profiles/active` - which profile is active. A profile that named itself
//!   the active one would fight the pointer it is read through.
//! * `workspace.window` - the outer window geometry. It is mirrored from the
//!   live viewport every frame, so a profile carrying it would read as
//!   modified the instant the window was nudged, and a profile copied to a
//!   laptop would place the window off the edge of a smaller display.
//! * `workspace.last_site` - the site that was live when the application last
//!   closed. That is a record of where the analyst *was*, not a preference;
//!   the preference is `data/startup_site`, which profiles do carry.
//!
//! # Robustness
//!
//! Nothing here refuses a file wholesale. A profile written by a newer build,
//! or naming settings this build does not know, is applied for the parts that
//! are understood and **reported** for the parts that are not
//! ([`ProfileNote`]) - and the parts that are not understood are still carried
//! through, so switching away and back on the newer build finds them intact.
//! Only a file that does not parse at all is set aside, and then it is set
//! aside by name, in a list the analyst can see, rather than deleted.
//!
//! # The shipped profile
//!
//! [`SHIPPED_NAME`] always exists, has no file, cannot be deleted, renamed or
//! overwritten, and holds whatever the caller declares "as shipped" - the
//! defaults with no stored values at all. It is how an analyst gets back to a
//! known state after an evening of experiments.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as Json};

use crate::document::{FORMAT_VERSION, SettingsDocument, WorkspaceSnapshot};
use crate::registry::SettingsRegistry;
use crate::value::SettingValue;

/// Version of the profile *wrapper* (the fields in [`ProfileFile`]), not of
/// the settings document nested inside it - that carries its own version and
/// its own compatibility mechanisms. A higher number is loaded anyway and
/// reported as [`ProfileNote::NewerProfileFormat`].
pub const PROFILE_FORMAT_VERSION: u32 = 1;

/// The extension every profile file carries. Plain `.json` because the file
/// is plain JSON and a person is expected to open it.
pub const EXTENSION: &str = "json";

/// The name of the profile that always exists and cannot be removed.
pub const SHIPPED_NAME: &str = "As Shipped";

/// The category the active-profile pointer is stored under, in the ordinary
/// settings document. It lives there rather than in a file of its own so it
/// is saved by the same atomic, debounced writer as everything else.
pub const BOOKKEEPING_CATEGORY: &str = "profiles";

/// The setting id of the active-profile pointer within
/// [`BOOKKEEPING_CATEGORY`].
pub const ACTIVE_SETTING: &str = "active";

/// Longest profile name kept; longer names are cut on a character boundary
/// rather than refused, the same way the registry treats over-long text.
const MAX_NAME: usize = 64;

/// Longest file stem a name may produce, before the collision suffix.
const MAX_STEM: usize = 48;

/// How much of [`MAX_STEM`] a truncated name keeps, leaving room for a hyphen
/// and eight hex digits of digest.
const TRUNCATED_STEM: usize = MAX_STEM - 9;

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

/// One profile as it rests on disk.
///
/// The nested `settings` object has exactly the shape of `settings.json`, so
/// a person can copy a block from one to the other and it means the same
/// thing in both places.
///
/// **The name of record is the `name` field inside the file, never the file
/// name.** The file name is a lossy, many-to-one reduction of it
/// ([`file_stem_for`]) which exists only so the directory is browsable:
/// `Chase / night` and `Chase night` both reduce to `chase-night`, and a
/// profile whose name is a Windows-illegal string still has a file. Every
/// candidate is opened and its `name` read before it is believed - the same
/// discipline the colour table store arrived at after a save that trusted a
/// file name overwrote a palette nobody could reach afterwards.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProfileFile {
    /// See [`PROFILE_FORMAT_VERSION`].
    #[serde(default)]
    pub profile_format: u32,
    /// The profile's name, as shown and as switched to.
    #[serde(default)]
    pub name: String,
    /// The settings document this profile applies.
    #[serde(default)]
    pub settings: SettingsDocument,
    /// Wrapper fields a future build added, re-emitted verbatim on save.
    #[serde(flatten)]
    pub unknown: JsonMap<String, Json>,
}

impl Default for ProfileFile {
    fn default() -> Self {
        Self {
            profile_format: PROFILE_FORMAT_VERSION,
            name: String::new(),
            settings: SettingsDocument::default(),
            unknown: JsonMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// What this build could not honour
// ---------------------------------------------------------------------------

/// Something in a profile this build cannot act on. Every one of these is a
/// note, never a refusal: the rest of the profile still applies.
#[derive(Clone, Debug, PartialEq)]
pub enum ProfileNote {
    /// The file has no `name` field, so its file stem was used instead.
    NamelessFile { stem: String },
    /// The wrapper was written by a newer build.
    NewerProfileFormat { found: u32, known: u32 },
    /// The settings document inside was written by a newer build.
    NewerSettingsFormat { found: u32, known: u32 },
    /// A whole settings category this build does not declare. Counted rather
    /// than listed one id at a time: a future build's page is one fact about
    /// this build, not thirty.
    UnknownCategory { category: String, settings: usize },
    /// A setting id this build does not declare, inside a category it does.
    UnknownSetting { category: String, id: String },
    /// A setting this build knows, holding a value shape no setting kind can
    /// read (an array, an object, a null). The setting's default applies.
    UnreadableValue { category: String, id: String },
}

impl ProfileNote {
    /// One line, as the settings window prints it.
    pub fn message(&self) -> String {
        match self {
            Self::NamelessFile { stem } => {
                format!("the file does not name the profile; using '{stem}' from its file name")
            }
            Self::NewerProfileFormat { found, known } => format!(
                "saved by a newer build (profile format {found}, this build knows {known}); \
                 applied as far as this build understands it"
            ),
            Self::NewerSettingsFormat { found, known } => format!(
                "its settings were saved by a newer build (format {found}, this build knows \
                 {known}); applied as far as this build understands them"
            ),
            Self::UnknownCategory {
                category,
                settings: count,
            } => format!(
                "'{category}' is not a settings page in this build - {count} value(s) skipped \
                 and kept for the build that knows them"
            ),
            Self::UnknownSetting { category, id } => format!(
                "'{category}/{id}' is not a setting in this build - skipped and kept for the \
                 build that knows it"
            ),
            Self::UnreadableValue { category, id } => format!(
                "'{category}/{id}' holds a value this build cannot read - its default applies"
            ),
        }
    }
}

/// Everything in `document` this build cannot act on.
///
/// Pure, so the notes shown beside a profile and the notes a test asserts on
/// come from the same place.
pub fn inspect(document: &SettingsDocument, registry: &SettingsRegistry) -> Vec<ProfileNote> {
    let mut notes = Vec::new();
    if document.version > FORMAT_VERSION {
        notes.push(ProfileNote::NewerSettingsFormat {
            found: document.version,
            known: FORMAT_VERSION,
        });
    }
    for (category, values) in &document.values {
        if registry.category(category).is_none() {
            notes.push(ProfileNote::UnknownCategory {
                category: category.clone(),
                settings: values.len(),
            });
            continue;
        }
        for (id, json) in values {
            if registry.setting(category, id).is_none() {
                notes.push(ProfileNote::UnknownSetting {
                    category: category.clone(),
                    id: id.clone(),
                });
            } else if SettingValue::from_json(json).is_none() {
                notes.push(ProfileNote::UnreadableValue {
                    category: category.clone(),
                    id: id.clone(),
                });
            }
        }
    }
    notes
}

// ---------------------------------------------------------------------------
// Snapshot, merge, and the active pointer
// ---------------------------------------------------------------------------

fn is_install_local(category: &str, id: &str) -> bool {
    category == BOOKKEEPING_CATEGORY && id == ACTIVE_SETTING
}

/// The document as a profile stores it: everything, minus the three
/// install-local parts the module documentation lists.
pub fn snapshot_for_profile(current: &SettingsDocument) -> SettingsDocument {
    let mut snapshot = current.clone();
    if let Some(values) = snapshot.values.get_mut(BOOKKEEPING_CATEGORY) {
        values.remove(ACTIVE_SETTING);
        if values.is_empty() {
            snapshot.values.remove(BOOKKEEPING_CATEGORY);
        }
    }
    snapshot.workspace.window = None;
    snapshot.workspace.last_site = None;
    snapshot
}

/// The document to install when switching to `profile`, named `name`.
///
/// The profile's values win wholesale - a category the profile does not
/// mention is a category the analyst cleared, and keeping the current values
/// there would make a profile unable to turn anything back off. The three
/// install-local parts come from `current` instead, and the active pointer is
/// stamped with `name` in the same operation, so the switch is one document
/// replacement rather than two writes that could be interrupted between.
///
/// Top-level sections this build cannot see are unioned rather than replaced:
/// a section written by a future build is one this build could not
/// reconstruct if it dropped it, and carrying an extra one costs nothing.
pub fn merge_for_switch(
    current: &SettingsDocument,
    profile: &SettingsDocument,
    name: &str,
) -> SettingsDocument {
    // Through the snapshot, not the raw profile: a hand-edited file that
    // carries a `window` block does not get to move this install's window.
    let mut merged = snapshot_for_profile(profile);
    merged.version = merged.version.max(current.version);
    merged
        .workspace
        .window
        .clone_from(&current.workspace.window);
    merged
        .workspace
        .last_site
        .clone_from(&current.workspace.last_site);
    for (key, value) in &current.unknown {
        merged.unknown.entry(key.clone()).or_insert(value.clone());
    }
    set_active_profile(&mut merged, name);
    merged
}

/// The profile the document says is active, if it says.
pub fn active_profile(document: &SettingsDocument) -> Option<&str> {
    document
        .values
        .get(BOOKKEEPING_CATEGORY)?
        .get(ACTIVE_SETTING)?
        .as_str()
}

/// Point the document at a profile by name.
pub fn set_active_profile(document: &mut SettingsDocument, name: &str) {
    document
        .values
        .entry(BOOKKEEPING_CATEGORY.to_owned())
        .or_default()
        .insert(ACTIVE_SETTING.to_owned(), Json::String(name.to_owned()));
}

// ---------------------------------------------------------------------------
// What differs
// ---------------------------------------------------------------------------

/// One way the live settings differ from the profile they were switched from.
///
/// Named rather than counted, because "you have unsaved changes" with no list
/// is a prompt an analyst has to answer blind.
#[derive(Clone, Debug, PartialEq)]
pub enum Difference {
    /// A registry-shaped value under `(category, id)`.
    Value { category: String, id: String },
    /// Part of the workspace snapshot - the layout, a pane, a colour table.
    Workspace { what: String },
}

impl Difference {
    /// One line for a person, using the registry's own words where this build
    /// declares the setting and the raw ids where it does not.
    pub fn describe(&self, registry: &SettingsRegistry) -> String {
        match self {
            Self::Value { category, id } => {
                match (registry.category(category), registry.setting(category, id)) {
                    (Some(page), Some(spec)) => format!("{} - {}", page.label, spec.label),
                    (Some(page), None) => format!("{} - {id} (not in this build)", page.label),
                    _ => format!("{category}/{id} (not in this build)"),
                }
            }
            Self::Workspace { what } => format!("Workspace - {what}"),
        }
    }
}

/// Every way `current` differs from `profile`.
///
/// The question this answers is exactly **"would switching to this profile
/// change anything?"**, which is what "modified" has to mean for the prompt to
/// be worth reading. Two consequences, both deliberate:
///
/// * scalar values are compared *as the application resolves them*, through
///   the registry: a value the profile does not carry resolves to the
///   setting's default, so a hand-written profile that mentions three settings
///   is compared as though it set the other forty to their defaults - which is
///   what switching to it would do. It also means `30` and `30.0` under an
///   integer setting are the same value, rather than a difference nobody could
///   act on;
/// * the workspace snapshot is compared the other way round, against what the
///   profile *asserts*: a `None` field there means "this file does not say"
///   and applying it changes nothing (`document::WorkspaceSnapshot`), so a
///   silent field is not a difference. A file that says nothing about the
///   colour tables is not a file that disagrees about them.
///
/// The install-local parts are never compared at all.
pub fn differences(
    current: &SettingsDocument,
    profile: &SettingsDocument,
    registry: &SettingsRegistry,
) -> Vec<Difference> {
    let mut found = Vec::new();
    let mut categories: BTreeSet<&str> = BTreeSet::new();
    categories.extend(current.values.keys().map(String::as_str));
    categories.extend(profile.values.keys().map(String::as_str));
    for category in categories {
        let mine = current.values.get(category);
        let theirs = profile.values.get(category);
        let mut ids: BTreeSet<&str> = BTreeSet::new();
        ids.extend(
            mine.into_iter()
                .flat_map(BTreeMap::keys)
                .map(String::as_str),
        );
        ids.extend(
            theirs
                .into_iter()
                .flat_map(BTreeMap::keys)
                .map(String::as_str),
        );
        for id in ids {
            if is_install_local(category, id) {
                continue;
            }
            let mine = mine.and_then(|values| values.get(id));
            let theirs = theirs.and_then(|values| values.get(id));
            let differs = match registry.setting(category, id) {
                Some(spec) => resolve(spec, mine) != resolve(spec, theirs),
                // A key this build does not declare has no default to resolve
                // against, so the raw JSON is all there is to compare - and
                // carrying it across a switch is still a change worth naming.
                None => mine != theirs,
            };
            if differs {
                found.push(Difference::Value {
                    category: category.to_owned(),
                    id: id.to_owned(),
                });
            }
        }
    }
    workspace_differences(&current.workspace, &profile.workspace, &mut found);
    for (key, value) in &profile.unknown {
        if current.unknown.get(key) != Some(value) {
            found.push(Difference::Workspace {
                what: format!("'{key}', from another build"),
            });
        }
    }
    found
}

fn resolve(spec: &crate::registry::SettingSpec, stored: Option<&Json>) -> SettingValue {
    spec.kind
        .sanitize(stored.and_then(SettingValue::from_json).as_ref())
}

/// Whether anything at all differs. Same comparison as [`differences`], asked
/// as a yes or no for the places that only show a marker.
pub fn differs(
    current: &SettingsDocument,
    profile: &SettingsDocument,
    registry: &SettingsRegistry,
) -> bool {
    !differences(current, profile, registry).is_empty()
}

/// What the profile asserts about the workspace that the live state does not
/// match. See [`differences`] for why this direction is the right one.
fn workspace_differences(
    current: &WorkspaceSnapshot,
    profile: &WorkspaceSnapshot,
    found: &mut Vec<Difference>,
) {
    let mut push = |what: String| found.push(Difference::Workspace { what });
    if profile.layout.is_some() && current.layout != profile.layout {
        push("pane layout".to_owned());
    }
    if profile.active_pane.is_some() && current.active_pane != profile.active_pane {
        push("active pane".to_owned());
    }
    for (index, pane) in profile.panes.iter().enumerate() {
        let mine = current.panes.get(index);
        if pane_differs(mine, pane) {
            push(format!("pane {}", index + 1));
        }
    }
    for (family, choice) in &profile.palettes {
        if current.palettes.get(family) != Some(choice) {
            push(format!("colour table for {family}"));
        }
    }
    if profile.show_warnings.is_some() && current.show_warnings != profile.show_warnings {
        push("warnings overlay".to_owned());
    }
    for (key, value) in &profile.unknown {
        if current.unknown.get(key) != Some(value) {
            push(format!("'{key}', from another build"));
        }
    }
    // `window` and `last_site` are deliberately absent: see the module
    // documentation. A profile never carries them, so they can never differ
    // from one in a way worth telling anybody about.
}

/// Whether the live pane fails to match what the profile's pane asserts.
/// Field by field, because a snapshot's `None` means "does not say" there too.
fn pane_differs(
    current: Option<&crate::document::PaneSnapshot>,
    profile: &crate::document::PaneSnapshot,
) -> bool {
    let Some(current) = current else {
        // The profile has a pane this install does not: applying it would put
        // one there.
        return true;
    };
    macro_rules! asserted {
        ($field:ident) => {
            profile.$field.is_some() && current.$field != profile.$field
        };
    }
    asserted!(product)
        || asserted!(tilt_mode)
        || asserted!(tilt_value)
        || asserted!(center_east_km)
        || asserted!(center_north_km)
        || asserted!(km_per_point)
        || asserted!(rotation_rad)
        || asserted!(camera_linked)
        || profile
            .unknown
            .iter()
            .any(|(key, value)| current.unknown.get(key) != Some(value))
}

// ---------------------------------------------------------------------------
// Names and file names
// ---------------------------------------------------------------------------

/// A profile name reduced to one line and to [`MAX_NAME`] characters.
pub fn tidy_name(name: &str) -> String {
    let mut tidy = String::with_capacity(name.len());
    let mut pending_space = false;
    for character in name.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !tidy.is_empty();
            continue;
        }
        if pending_space {
            tidy.push(' ');
            pending_space = false;
        }
        tidy.push(character);
        if tidy.chars().count() >= MAX_NAME {
            break;
        }
    }
    tidy
}

/// A file stem for a profile name: lower case, ASCII alphanumerics and single
/// hyphens, never empty. Deliberately lossy - see [`ProfileFile`].
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
        return "profile".to_owned();
    }
    if truncated {
        stem.push('-');
        stem.push_str(&format!("{:08x}", name_digest(name)));
    }
    stem
}

/// FNV-1a (Fowler/Noll/Vo, 1991) over the name's bytes, for the truncation
/// suffix. Written out rather than taken from `std::hash`, whose output is
/// explicitly not stable across Rust releases: a file name that moved when the
/// toolchain moved would orphan every long-named profile on disk.
fn name_digest(name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn path_in(directory: &Path, name: &str) -> PathBuf {
    directory.join(format!("{}.{EXTENSION}", file_stem_for(name)))
}

/// Where a NEW profile of this name is written: [`path_in`], or the first free
/// `-2`, `-3` ... beside it. Only ever used for a profile that has no file
/// yet; a rename keeps the file it already has.
fn free_path_in(directory: &Path, name: &str) -> PathBuf {
    let first = path_in(directory, name);
    if !first.exists() {
        return first;
    }
    let stem = file_stem_for(name);
    for suffix in 2..1000u32 {
        let candidate = directory.join(format!("{stem}-{suffix}.{EXTENSION}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why an operation was refused. Every variant prints as a sentence the
/// settings window shows verbatim.
#[derive(Clone, Debug, PartialEq)]
pub enum ProfileError {
    /// A name that is blank once tidied.
    EmptyName,
    /// Another profile already answers to that name.
    NameTaken { name: String },
    /// The name the shipped profile answers to.
    Reserved { name: String },
    /// No profile of that name.
    NotFound { name: String },
    /// An operation the shipped profile refuses: it has no file to write,
    /// rename or delete.
    Shipped,
    /// The filesystem said no.
    Io { path: PathBuf, error: String },
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(formatter, "a profile needs a name"),
            Self::NameTaken { name } => {
                write!(formatter, "another profile is already called '{name}'")
            }
            Self::Reserved { name } => write!(
                formatter,
                "'{name}' is the shipped profile's name and is kept for it"
            ),
            Self::NotFound { name } => write!(formatter, "there is no profile called '{name}'"),
            Self::Shipped => write!(
                formatter,
                "the shipped profile has no file to change - it is how this build behaves with \
                 nothing stored, and it is always here to come back to"
            ),
            Self::Io { path, error } => write!(formatter, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for ProfileError {}

// ---------------------------------------------------------------------------
// The library
// ---------------------------------------------------------------------------

/// One profile, ready to switch to.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// The name of record - read out of the file, not off it.
    pub name: String,
    /// `None` for the shipped profile, which has no file.
    pub file: Option<PathBuf>,
    /// What switching to it installs.
    pub document: SettingsDocument,
    /// The wrapper version the file declared, preserved across rewrites.
    pub format: u32,
    /// Wrapper fields this build does not know, preserved across rewrites.
    pub unknown: JsonMap<String, Json>,
    /// What this build could not honour in it. Never a reason to refuse.
    pub faults: Vec<ProfileNote>,
}

impl Profile {
    /// Whether this is the shipped profile, which several operations refuse.
    pub fn is_shipped(&self) -> bool {
        self.file.is_none()
    }
}

/// A file in the profiles directory that could not be read as a profile. Kept
/// and shown rather than deleted or ignored: it is the analyst's file, it may
/// hold a hand edit worth rescuing, and a profile that silently vanished from
/// the list is a bug report nobody can answer.
#[derive(Clone, Debug, PartialEq)]
pub struct BrokenProfile {
    pub file: PathBuf,
    pub reason: String,
}

/// Every profile on this install, rescanned on demand.
pub struct ProfileLibrary {
    directory: PathBuf,
    shipped: SettingsDocument,
    profiles: Vec<Profile>,
    broken: Vec<BrokenProfile>,
    directory_error: Option<String>,
    generation: u64,
}

impl ProfileLibrary {
    /// Open the library at `directory`, declaring `shipped` as the document
    /// the shipped profile installs.
    ///
    /// `shipped` is injected rather than assumed to be
    /// `SettingsDocument::default()` because "as shipped" is a claim about the
    /// application, not about this crate: the workstation's shipped state
    /// includes a default pane layout and the default colour tables, which are
    /// structured snapshot state only the application can build.
    pub fn open(
        directory: impl Into<PathBuf>,
        shipped: SettingsDocument,
        registry: &SettingsRegistry,
    ) -> Self {
        let mut library = Self {
            directory: directory.into(),
            shipped,
            profiles: Vec::new(),
            broken: Vec::new(),
            directory_error: None,
            generation: 0,
        };
        library.rescan(registry);
        library
    }

    /// Read the directory again. Cheap enough to call after every operation
    /// and on opening the settings window, which is exactly when it is called:
    /// a handful of small files, never per frame.
    pub fn rescan(&mut self, registry: &SettingsRegistry) {
        let (profiles, broken, error) = scan(&self.directory, registry);
        self.profiles = profiles;
        self.broken = broken;
        self.directory_error = error;
        self.profiles.insert(0, self.shipped_profile(registry));
        self.generation = self.generation.wrapping_add(1);
    }

    fn shipped_profile(&self, registry: &SettingsRegistry) -> Profile {
        Profile {
            name: SHIPPED_NAME.to_owned(),
            file: None,
            document: snapshot_for_profile(&self.shipped),
            format: PROFILE_FORMAT_VERSION,
            unknown: JsonMap::new(),
            faults: inspect(&self.shipped, registry),
        }
    }

    /// Declare a different "as shipped" document. Takes effect on the next
    /// [`Self::rescan`], which this performs.
    pub fn set_shipped(&mut self, shipped: SettingsDocument, registry: &SettingsRegistry) {
        self.shipped = shipped;
        self.rescan(registry);
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The shipped profile first, then the analyst's own by name.
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn broken(&self) -> &[BrokenProfile] {
        &self.broken
    }

    /// The directory itself could not be read - a permission problem, or a
    /// file sitting where the directory should be.
    pub fn directory_error(&self) -> Option<&str> {
        self.directory_error.as_deref()
    }

    /// Bumped by every rescan, for a caller caching anything derived from the
    /// list.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A profile by name, case-insensitively: an analyst typing `chase` into
    /// the name box means the profile called `Chase`.
    pub fn find(&self, name: &str) -> Option<&Profile> {
        let wanted = tidy_name(name);
        self.profiles
            .iter()
            .find(|profile| profile.name.eq_ignore_ascii_case(&wanted))
    }

    /// A free name near `wanted`: `wanted`, then `wanted 2`, `wanted 3` ...
    /// Numbered against the NAMES the library holds, not against the file
    /// names, because the name is what a profile is found by everywhere else.
    pub fn free_name(&self, wanted: &str) -> String {
        let wanted = tidy_name(wanted);
        if self.find(&wanted).is_none() {
            return wanted;
        }
        for suffix in 2..1000u32 {
            let candidate = format!("{wanted} {suffix}");
            if self.find(&candidate).is_none() {
                return candidate;
            }
        }
        wanted
    }

    /// Save `current` as a NEW profile. Returns the name it was stored under.
    pub fn save_as(
        &mut self,
        name: &str,
        current: &SettingsDocument,
        registry: &SettingsRegistry,
    ) -> Result<String, ProfileError> {
        let name = self.check_new_name(name)?;
        let path = free_path_in(&self.directory, &name);
        self.write(
            &path,
            &ProfileFile {
                profile_format: PROFILE_FORMAT_VERSION,
                name: name.clone(),
                settings: snapshot_for_profile(current),
                unknown: JsonMap::new(),
            },
        )?;
        self.rescan(registry);
        Ok(name)
    }

    /// Write `current` over an existing profile, keeping its name, its file
    /// and any wrapper fields a newer build put in it.
    pub fn overwrite(
        &mut self,
        name: &str,
        current: &SettingsDocument,
        registry: &SettingsRegistry,
    ) -> Result<(), ProfileError> {
        let profile = self.require(name)?;
        let path = profile.file.clone().ok_or(ProfileError::Shipped)?;
        let file = ProfileFile {
            profile_format: profile.format.max(PROFILE_FORMAT_VERSION),
            name: profile.name.clone(),
            settings: snapshot_for_profile(current),
            unknown: profile.unknown.clone(),
        };
        self.write(&path, &file)?;
        self.rescan(registry);
        Ok(())
    }

    /// Rename a profile.
    ///
    /// The file stays where it is and only the `name` inside it changes. That
    /// leaves a file whose stem no longer matches its name, which is by
    /// design - nothing identifies a profile by its file name - and it is far
    /// safer than the alternative: writing a second file and deleting the
    /// first leaves both on disk if the delete fails, and then two files
    /// declare one name.
    pub fn rename(
        &mut self,
        name: &str,
        new_name: &str,
        registry: &SettingsRegistry,
    ) -> Result<String, ProfileError> {
        let profile = self.require(name)?;
        let path = profile.file.clone().ok_or(ProfileError::Shipped)?;
        let previous = profile.name.clone();
        let format = profile.format.max(PROFILE_FORMAT_VERSION);
        let document = profile.document.clone();
        let unknown = profile.unknown.clone();
        let new_name = tidy_name(new_name);
        if new_name.is_empty() {
            return Err(ProfileError::EmptyName);
        }
        if new_name.eq_ignore_ascii_case(SHIPPED_NAME) {
            return Err(ProfileError::Reserved { name: new_name });
        }
        if !new_name.eq_ignore_ascii_case(&previous) && self.find(&new_name).is_some() {
            return Err(ProfileError::NameTaken { name: new_name });
        }
        self.write(
            &path,
            &ProfileFile {
                profile_format: format,
                name: new_name.clone(),
                settings: document,
                unknown,
            },
        )?;
        self.rescan(registry);
        Ok(new_name)
    }

    /// Copy a profile under a free name. The shipped profile may be copied -
    /// that is how an analyst starts from a known state and edits from there.
    pub fn duplicate(
        &mut self,
        name: &str,
        registry: &SettingsRegistry,
    ) -> Result<String, ProfileError> {
        let profile = self.require(name)?;
        let format = profile.format.max(PROFILE_FORMAT_VERSION);
        let document = profile.document.clone();
        let unknown = profile.unknown.clone();
        let copy = self.free_name(&format!("{} copy", profile.name));
        let path = free_path_in(&self.directory, &copy);
        self.write(
            &path,
            &ProfileFile {
                profile_format: format,
                name: copy.clone(),
                settings: document,
                unknown,
            },
        )?;
        self.rescan(registry);
        Ok(copy)
    }

    /// Delete a profile's file. The shipped profile refuses.
    pub fn delete(&mut self, name: &str, registry: &SettingsRegistry) -> Result<(), ProfileError> {
        let profile = self.require(name)?;
        let path = profile.file.clone().ok_or(ProfileError::Shipped)?;
        std::fs::remove_file(&path).map_err(|error| ProfileError::Io {
            path: path.clone(),
            error: error.to_string(),
        })?;
        self.rescan(registry);
        Ok(())
    }

    /// Delete a file the scan could not read - the only way to clear a broken
    /// entry from inside the application.
    pub fn delete_file(
        &mut self,
        path: &Path,
        registry: &SettingsRegistry,
    ) -> Result<(), ProfileError> {
        std::fs::remove_file(path).map_err(|error| ProfileError::Io {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
        self.rescan(registry);
        Ok(())
    }

    fn require(&self, name: &str) -> Result<&Profile, ProfileError> {
        self.find(name).ok_or_else(|| ProfileError::NotFound {
            name: tidy_name(name),
        })
    }

    fn check_new_name(&self, name: &str) -> Result<String, ProfileError> {
        let name = tidy_name(name);
        if name.is_empty() {
            return Err(ProfileError::EmptyName);
        }
        if name.eq_ignore_ascii_case(SHIPPED_NAME) {
            return Err(ProfileError::Reserved { name });
        }
        if self.find(&name).is_some() {
            return Err(ProfileError::NameTaken { name });
        }
        Ok(name)
    }

    fn write(&self, path: &Path, file: &ProfileFile) -> Result<(), ProfileError> {
        // The store's writer: temp file, flush to disk, rename over the
        // target. A crash mid-write leaves the previous profile intact.
        crate::store::write_atomically(path, file).map_err(|error| ProfileError::Io {
            path: path.to_path_buf(),
            error: error.to_string(),
        })
    }
}

type ScanResult = (Vec<Profile>, Vec<BrokenProfile>, Option<String>);

fn scan(directory: &Path, registry: &SettingsRegistry) -> ScanResult {
    let mut profiles: Vec<Profile> = Vec::new();
    let mut broken: Vec<BrokenProfile> = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // No directory yet is first run, not a fault.
            return (profiles, broken, None);
        }
        Err(error) => return (profiles, broken, Some(error.to_string())),
    };
    // Sorted, so which of two files declaring one name wins is decided by the
    // directory's contents and not by the order the filesystem hands them
    // back - the same run twice must produce the same list.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case(EXTENSION))
        })
        .collect();
    paths.sort();

    for path in paths {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                broken.push(BrokenProfile {
                    file: path,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let file: ProfileFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(error) => {
                broken.push(BrokenProfile {
                    file: path,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let mut faults = Vec::new();
        let mut name = tidy_name(&file.name);
        if name.is_empty() {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("profile")
                .to_owned();
            faults.push(ProfileNote::NamelessFile { stem: stem.clone() });
            name = stem;
        }
        if name.eq_ignore_ascii_case(SHIPPED_NAME)
            || profiles
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&name))
        {
            // Two files answering to one name is the ambiguity that makes a
            // library unusable: switch installs one, overwrite writes the
            // other. Said out loud, with the file named, so it can be fixed.
            broken.push(BrokenProfile {
                file: path,
                reason: format!("another profile is already called '{name}'"),
            });
            continue;
        }
        if file.profile_format > PROFILE_FORMAT_VERSION {
            faults.push(ProfileNote::NewerProfileFormat {
                found: file.profile_format,
                known: PROFILE_FORMAT_VERSION,
            });
        }
        faults.extend(inspect(&file.settings, registry));
        profiles.push(Profile {
            name,
            file: Some(path),
            document: file.settings,
            format: file.profile_format,
            unknown: file.unknown,
            faults,
        });
    }
    profiles.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    (profiles, broken, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{PaneSnapshot, WindowSnapshot};
    use crate::registry::{SettingKind, SettingSpec, SettingsCategory};

    fn registry() -> SettingsRegistry {
        let mut registry = SettingsRegistry::new();
        registry.register(SettingsCategory::new(
            "map",
            "Map",
            vec![
                SettingSpec::new(
                    "basemap_style",
                    "Basemap style",
                    SettingKind::Text {
                        default: "slate".to_owned(),
                        placeholder: String::new(),
                        max_len: 32,
                    },
                ),
                SettingSpec::new(
                    "site_markers",
                    "Site markers",
                    SettingKind::Toggle { default: true },
                ),
            ],
        ));
        registry.register(SettingsCategory::new(
            BOOKKEEPING_CATEGORY,
            "Profiles",
            vec![SettingSpec::new(
                "show_in_status",
                "Show the active profile",
                SettingKind::Toggle { default: true },
            )],
        ));
        registry
    }

    fn document_with(category: &str, id: &str, value: Json) -> SettingsDocument {
        let mut document = SettingsDocument::default();
        document
            .values
            .entry(category.to_owned())
            .or_default()
            .insert(id.to_owned(), value);
        document
    }

    fn with_panes(document: &mut SettingsDocument, count: usize) {
        document.workspace.panes = (0..count)
            .map(|index| PaneSnapshot {
                product: Some(format!("P{index}")),
                ..Default::default()
            })
            .collect();
    }

    #[test]
    fn a_profile_snapshot_drops_the_three_install_local_parts_and_keeps_everything_else() {
        let mut document = document_with("map", "basemap_style", Json::String("daylight".into()));
        set_active_profile(&mut document, "Chase");
        document.workspace.last_site = Some("KDVN".to_owned());
        document.workspace.window = Some(WindowSnapshot {
            width: Some(1280.0),
            ..Default::default()
        });
        document.workspace.layout = Some("four".to_owned());

        let snapshot = snapshot_for_profile(&document);
        assert_eq!(active_profile(&snapshot), None);
        assert_eq!(snapshot.workspace.last_site, None);
        assert_eq!(snapshot.workspace.window, None);
        assert_eq!(snapshot.workspace.layout.as_deref(), Some("four"));
        assert_eq!(
            snapshot.values["map"]["basemap_style"],
            Json::String("daylight".into())
        );
    }

    #[test]
    fn merging_takes_the_profiles_values_wholesale_and_keeps_the_installs_own_parts() {
        let mut current = document_with("map", "site_markers", Json::Bool(false));
        current.workspace.last_site = Some("KDVN".to_owned());
        current.workspace.window = Some(WindowSnapshot {
            width: Some(1280.0),
            ..Default::default()
        });
        current
            .unknown
            .insert("biometrics".to_owned(), Json::Bool(true));
        set_active_profile(&mut current, "Chase");

        let profile = document_with("map", "basemap_style", Json::String("daylight".into()));
        let merged = merge_for_switch(&current, &profile, "Office");

        // The profile did not mention site_markers, so it is cleared - a
        // profile has to be able to turn something back off.
        assert!(!merged.values["map"].contains_key("site_markers"));
        assert_eq!(
            merged.values["map"]["basemap_style"],
            Json::String("daylight".into())
        );
        assert_eq!(active_profile(&merged), Some("Office"));
        assert_eq!(merged.workspace.last_site.as_deref(), Some("KDVN"));
        assert_eq!(
            merged.workspace.window.and_then(|window| window.width),
            Some(1280.0)
        );
        assert_eq!(merged.unknown["biometrics"], Json::Bool(true));
    }

    #[test]
    fn differences_name_what_moved_and_ignore_the_install_local_parts() {
        let mut current = document_with("map", "basemap_style", Json::String("daylight".into()));
        current.workspace.layout = Some("four".to_owned());
        current.workspace.last_site = Some("KDVN".to_owned());
        set_active_profile(&mut current, "Chase");
        with_panes(&mut current, 2);

        let mut profile = document_with("map", "basemap_style", Json::String("slate".into()));
        profile.workspace.layout = Some("one".to_owned());
        // One pane, and it wants a different product in it. The profile says
        // nothing about the second pane, which is the point of the assertion
        // below: a file that is silent about something is not a file that
        // disagrees about it.
        profile.workspace.panes = vec![PaneSnapshot {
            product: Some("VEL".to_owned()),
            ..Default::default()
        }];

        let registry = registry();
        let found = differences(&current, &profile, &registry);
        let described: Vec<String> = found
            .iter()
            .map(|difference| difference.describe(&registry))
            .collect();
        assert!(
            described.contains(&"Map - Basemap style".to_owned()),
            "{described:?}"
        );
        assert!(
            described.contains(&"Workspace - pane layout".to_owned()),
            "{described:?}"
        );
        assert!(
            described.contains(&"Workspace - pane 1".to_owned()),
            "{described:?}"
        );
        assert!(
            !described.contains(&"Workspace - pane 2".to_owned()),
            "the profile says nothing about pane 2, so switching to it would not move \
             pane 2, so it is not a difference: {described:?}"
        );
        assert!(
            !described.iter().any(|line| line.contains("active")),
            "the active pointer is install-local and must never read as a change: {described:?}"
        );
        assert!(differs(&current, &profile, &registry));
        assert!(!differs(&current, &current.clone(), &registry));
    }

    #[test]
    fn an_unknown_category_or_id_is_a_note_never_a_refusal() {
        let mut document = document_with("map", "basemap_style", Json::String("daylight".into()));
        document
            .values
            .entry("map".to_owned())
            .or_default()
            .insert("hologram_mode".to_owned(), Json::Bool(true));
        document
            .values
            .entry("quantum_overlay".to_owned())
            .or_default()
            .insert("entanglement".to_owned(), Json::from(0.7));
        document
            .values
            .entry("map".to_owned())
            .or_default()
            .insert("site_markers".to_owned(), Json::Array(vec![]));
        document.version = 9;

        let notes = inspect(&document, &registry());
        assert!(notes.contains(&ProfileNote::NewerSettingsFormat {
            found: 9,
            known: FORMAT_VERSION
        }));
        assert!(notes.contains(&ProfileNote::UnknownCategory {
            category: "quantum_overlay".to_owned(),
            settings: 1
        }));
        assert!(notes.contains(&ProfileNote::UnknownSetting {
            category: "map".to_owned(),
            id: "hologram_mode".to_owned()
        }));
        assert!(notes.contains(&ProfileNote::UnreadableValue {
            category: "map".to_owned(),
            id: "site_markers".to_owned()
        }));
        for note in &notes {
            assert!(!note.message().is_empty());
        }
    }

    #[test]
    fn names_reduce_to_stable_file_stems_and_long_names_stay_distinct() {
        assert_eq!(file_stem_for("Chase / night"), "chase-night");
        assert_eq!(file_stem_for("Chase night"), "chase-night");
        assert_eq!(file_stem_for("???"), "profile");
        let long_a = format!("{}A", "storm ".repeat(20));
        let long_b = format!("{}B", "storm ".repeat(20));
        assert_ne!(file_stem_for(&long_a), file_stem_for(&long_b));
        assert!(file_stem_for(&long_a).len() <= MAX_STEM);
    }

    #[test]
    fn a_name_is_tidied_to_one_line_and_bounded() {
        assert_eq!(tidy_name("  Chase \n setup  "), "Chase setup");
        assert_eq!(tidy_name("\t\n "), "");
        assert_eq!(tidy_name(&"x".repeat(200)).chars().count(), MAX_NAME);
    }
}
