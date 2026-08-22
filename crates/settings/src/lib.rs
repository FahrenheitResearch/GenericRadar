//! The workstation's persisted settings: a typed, contributed registry of
//! knobs and a crash-safe, forward-compatible store for their values.
//!
//! Three layers, bottom to top, and the reason each exists:
//!
//! * [`registry`] - the *shape* of a setting. A module that owns a capability
//!   declares a [`SettingsCategory`] - plain data: an id, a label and a list
//!   of typed items with ranges and defaults - and the master settings window
//!   renders whatever was declared. This is the white-label plumbing: a crate
//!   ported from BowEcho tomorrow declares its settings the same way and they
//!   appear in the menu with zero changes to the menu code. No trait objects,
//!   no macros; declarations are values.
//! * [`store`] - the persisted *values*, keyed by `(category id, setting id)`,
//!   plus a structured [`WorkspaceSnapshot`] for state that is not a scalar
//!   knob (pane products, cameras, palettes, the last site). The store is
//!   versioned, keeps every field it does not understand so a file written by
//!   a future build survives a round trip through this one, writes atomically
//!   so a crash mid-save cannot corrupt the file, and debounces saves so no
//!   caller ever saves per frame.
//! * [`transfer`] - carrying a whole document out to a file the analyst
//!   names and back in again, with a plain summary of what an import moved
//!   and a stated reason when one is refused. Above the store because it is
//!   a deliberate operation with its own policy, not part of persistence.
//! * [`paths`] - where the file lives. The base directory is injectable
//!   because an iOS or Android shell has to hand the sandbox path in; the
//!   desktop defaults follow the conventions the rest of this workspace
//!   already uses (`LOCALAPPDATA/FahrenheitResearch/RadarWorkstation` on
//!   Windows, XDG on Linux).
//!
//! Above those three, [`profiles`] adds named snapshots of the whole
//! document - a chase setup, an office setup, a presentation setup - stored
//! one file per profile beside `settings.json`. It sits on top of the
//! document rather than beside the registry on purpose: a profile carries
//! whatever the document holds, including settings this build has never heard
//! of, so it never needs to be taught about a new knob.
//!
//! This crate deliberately depends on nothing but serde. It sits at the
//! bottom of the workspace so that any crate - including ones that do not
//! exist yet - can depend on it to declare settings without creating a cycle.
//! See `docs/extending.md` for the full contribution contract.

pub mod document;
pub mod paths;
pub mod profiles;
pub mod registry;
pub mod store;
pub mod transfer;
pub mod value;

pub use document::{
    PaletteChoice, PaneSnapshot, SettingsDocument, SourceFieldPaletteChoice, WindowSnapshot,
    WorkspaceSnapshot,
};
pub use paths::{
    app_cache_root, app_config_root, default_settings_file, is_fallback_root, profiles_dir,
    set_app_cache_root, set_app_config_root, user_colortables_dir,
};
pub use profiles::{Profile, ProfileError, ProfileLibrary, ProfileNote};
pub use registry::{
    ChoiceOption, SettingKind, SettingSpec, SettingsCategory, SettingsRegistry, SettingsSection,
    SliderFloor,
};
pub use store::{LoadStatus, SettingsStore};
pub use transfer::{ChangedSetting, ImportRefusal, ImportSummary};
pub use value::SettingValue;
