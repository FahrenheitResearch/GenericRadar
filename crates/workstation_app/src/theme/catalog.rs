//! The theme catalog: every look the workstation ships, one value per file.
//!
//! A theme is DATA here, not code. One theme is one [`ThemeSpec`] const in
//! one module file beside this one, and registering it is one line in the
//! [`catalog!`] list below. Nothing else in the application enumerates
//! themes: the settings page derives its options from [`THEMES`], the
//! contact sheet in `examples/theme_gallery.rs` photographs [`THEMES`], and
//! the contrast audit in `tests/theme_catalog.rs` measures [`THEMES`]. Add a
//! theme in two places - its own file, and one line here - and every one of
//! those follows without further edits.
//!
//! # Adding a theme
//!
//! 1. Write `src/theme/<id>.rs`. Copy `light.rs` or `dark.rs`, which are the
//!    two worked examples, and change the values. The module must expose
//!    exactly one item: `pub const THEME: ThemeSpec`.
//! 2. Add `<id>,` to the [`catalog!`] list below, in alphabetical order.
//! 3. Run `cargo test --release -p workstation_app` - the contrast audit
//!    will name any pairing that is not legible - and then the contact sheet
//!    (`examples/theme_gallery.rs`) to look at it on real radar.
//!
//! The module name IS the id with hyphens written as underscores, and the
//! list is sorted by it, which is the same order the ids sort in (`-` and
//! `_` both sort before every letter). A registration is therefore one line
//! that a merge can place without reading the file, which is the point: six
//! themes arriving on six branches must not collide over a shared array.
//!
//! # Why the two founding ids are so terse
//!
//! `light` and `dark` are the strings already written into every settings
//! file in the field. Ids are the persistence contract (see
//! `settings::registry`), so they are kept exactly as they were rather than
//! renamed to match their labels - a rename would silently reset the choice
//! of everyone who picked the night bench. New themes should use a
//! descriptive id (`amber-crt`, `paper-white`), because they carry no such
//! history.

use eframe::egui::Theme;

pub use super::palette::Palette;

/// Whether a theme's chrome sits on a light ground or a dark one.
///
/// This is not decoration: egui keeps two style slots and seeds a handful of
/// mode-conditional behaviours (text-alpha handling, cursor previews) from
/// `Visuals::dark_mode`, so a theme has to say which of the two it is or
/// those details come out wrong for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ground {
    /// Chrome lighter than its data wells.
    #[default]
    Light,
    /// Chrome darker than the room.
    Dark,
}

impl Ground {
    /// The egui theme slot a theme on this ground styles.
    pub const fn egui_theme(self) -> Theme {
        match self {
            Self::Light => Theme::Light,
            Self::Dark => Theme::Dark,
        }
    }

    /// What egui's `Visuals::dark_mode` must say for this ground.
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// The other ground. Used to decide what the *unused* egui style slot is
    /// filled with, so an OS light/dark flip can never land on stock egui.
    pub const fn opposite(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }
}

/// One registered theme: its identity, its words, and every colour the
/// chrome draws.
///
/// The palette is the complete token set - see [`Palette`] for what each
/// role means and where it is painted. A theme declares all of it; there is
/// no inheritance and no partial theme, because a role left to a default is
/// a role nobody measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeSpec {
    /// Stable stored identifier. Written into settings files; never reused
    /// for a different look.
    pub id: &'static str,
    /// What the settings page shows in the list.
    pub label: &'static str,
    /// One line under the label: what this look is for, in the words an
    /// analyst would use.
    pub description: &'static str,
    /// Light or dark ground, so egui's own widget defaults are seeded from
    /// the right mode.
    pub ground: Ground,
    /// Every colour the chrome draws.
    pub palette: Palette,
}

/// Declare the registered themes.
///
/// Each name is both the module file (`src/theme/<name>.rs`) and the
/// registration, so a theme cannot be half-added: a module that exists but
/// is not listed does not compile into the binary, and a name listed without
/// a file fails to build with the missing path.
macro_rules! catalog {
    ($($module:ident),* $(,)?) => {
        $(pub mod $module;)*

        /// Every registered theme, in the order declared below.
        pub const THEMES: &[&ThemeSpec] = &[$(&$module::THEME),*];
    };
}

// ---------------------------------------------------------------------------
// THE REGISTRY. One line per theme, alphabetical by id. Add yours here.
// ---------------------------------------------------------------------------
catalog! {
    amber_ops,
    broadcast,
    dark,
    high_visibility,
    light,
    modern_flat,
    paper,
    storm_night,
}

/// The theme a fresh install opens on: the daylight bench, which is the
/// application's identity.
pub const DEFAULT: &ThemeSpec = DEFAULT_LIGHT;

/// The light-ground theme every other light-ground theme is measured
/// against, and the one that fills the light style slot when a dark theme is
/// in force.
pub const DEFAULT_LIGHT: &ThemeSpec = &light::THEME;

/// The dark-ground counterpart of [`DEFAULT_LIGHT`].
pub const DEFAULT_DARK: &ThemeSpec = &dark::THEME;

/// The registered theme with this id, or `None`.
///
/// `None` is the whole fallback rule in one place: a stored id this build
/// does not know (a theme from a newer build, a hand-edited file) resolves
/// to [`DEFAULT`] and the stored string is left alone, so the theme comes
/// back when the build that has it is next run.
pub fn by_id(id: &str) -> Option<&'static ThemeSpec> {
    THEMES.iter().copied().find(|theme| theme.id == id)
}

/// The registered theme with this id, or [`DEFAULT`].
pub fn by_id_or_default(id: Option<&str>) -> &'static ThemeSpec {
    id.and_then(by_id).unwrap_or(DEFAULT)
}

/// The theme that fills a ground's style slot when the chosen theme is on
/// the other ground.
pub const fn default_for(ground: Ground) -> &'static ThemeSpec {
    match ground {
        Ground::Light => DEFAULT_LIGHT,
        Ground::Dark => DEFAULT_DARK,
    }
}
