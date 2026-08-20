//! The Appearance page, declared as data, and the one place a stored
//! appearance is turned back into an [`Appearance`].
//!
//! The page lives here rather than in `settings_ui::catalog` for a reason
//! that is easy to miss: the theme options are DERIVED from
//! [`super::catalog::THEMES`], so the page can only be declared somewhere
//! that can see the catalog. `settings_ui/catalog.rs` is compiled in a second
//! home (the `settings` crate's UI harness) where `crate::theme` does not
//! exist, so a theme list written there would have to be hand-maintained —
//! exactly the drift this framework is meant to remove. The settings
//! registry merges two categories that share an id, so the toolbar setting
//! declared over in `settings_ui::catalog` still lands on this same page,
//! after these.
//!
//! # The fallback rule
//!
//! Every axis resolves the same way and it is the rule the whole settings
//! system already uses: an unrecognised stored id falls back to the default
//! and the stored string is LEFT ALONE. A theme from a newer build, or one
//! an analyst is between versions of, comes back the moment the build that
//! has it runs again. Nothing here panics on a stranger value, and nothing
//! here writes over one.

use ::settings::{ChoiceOption, SettingKind, SettingSpec, SettingsCategory};

use super::appearance::{Accent, Appearance, ChromeEdges, Density, UiScale};
use super::catalog;

/// Stable identifiers. These strings are the persistence contract: they name
/// values in settings files already written, so they are never reused for a
/// different meaning.
pub mod keys {
    /// Shared with `settings_ui::catalog::keys::appearance::CATEGORY`, which
    /// declares the toolbar setting on the same page. `app.rs` pins the two
    /// spellings equal so they cannot drift.
    pub const CATEGORY: &str = "appearance";
    pub const THEME: &str = "theme";
    pub const ACCENT: &str = "accent";
    pub const CHROME_EDGES: &str = "chrome_edges";
    pub const DENSITY: &str = "density";
    pub const UI_SCALE: &str = "ui_scale";

    /// Every key this page owns, for the callers that have to notice when
    /// any of them changed and re-install the theme.
    pub const ALL: [&str; 5] = [THEME, ACCENT, CHROME_EDGES, DENSITY, UI_SCALE];
}

fn choice(options: Vec<ChoiceOption>, default_id: &str) -> SettingKind {
    SettingKind::Choice {
        options,
        default_id: default_id.to_owned(),
    }
}

/// The Appearance page: the theme, then the three colour/shape axes, then
/// the size axis.
///
/// The theme options are read out of the catalog, never listed here, so a
/// theme registered in `catalog.rs` appears on this page with no edit to
/// this file. Same for the accent, density, edge and scale options, which
/// are read off their own `ALL` arrays.
pub fn settings_category() -> SettingsCategory {
    let theme_options = catalog::THEMES
        .iter()
        .map(|theme| ChoiceOption::new(theme.id, theme.label).describe(theme.description))
        .collect::<Vec<_>>();
    let accent_options = Accent::ALL
        .into_iter()
        .map(|accent| ChoiceOption::new(accent.id(), accent.label()))
        .collect::<Vec<_>>();
    let edge_options = ChromeEdges::ALL
        .into_iter()
        .map(|edges| ChoiceOption::new(edges.id(), edges.label()))
        .collect::<Vec<_>>();
    let density_options = Density::ALL
        .into_iter()
        .map(|density| ChoiceOption::new(density.id(), density.label()))
        .collect::<Vec<_>>();
    let scale_options = UiScale::ALL
        .into_iter()
        .map(|scale| ChoiceOption::new(scale.id(), scale.label()))
        .collect::<Vec<_>>();

    SettingsCategory::new(
        keys::CATEGORY,
        "Appearance",
        vec![
            SettingSpec::new(
                keys::THEME,
                "Theme",
                choice(theme_options, catalog::DEFAULT.id),
            )
            // Deliberately says nothing about which themes exist. This
            // sentence used to name the two that shipped, and the moment six
            // more were registered it was wrong on the page every analyst
            // reads. What each theme is FOR belongs to that theme, which is
            // what `ThemeSpec::description` is, and the list above now shows
            // it under each entry.
            .help(
                "The colours of the whole application's chrome - the panels, the \
                 buttons, the group boxes and the wells. Each one below says what \
                 it is for; they all draw the same instrument, in different \
                 light. The radar panes keep their own ground whichever you pick: \
                 data is drawn on the map's colours, not the theme's.",
            ),
            SettingSpec::new(
                keys::ACCENT,
                "Accent",
                choice(accent_options, Accent::default().id()),
            )
            .help(
                "The colour used for selection, keyboard focus, links and a latched \
                 toolbar button. Theme's own keeps whatever the theme declared. The \
                 others are checked for legibility against every theme, which is why \
                 this is a list and not a colour picker. The active-pane border is \
                 not on this list - it belongs to the map.",
            ),
            SettingSpec::new(
                keys::CHROME_EDGES,
                "Chrome edges",
                choice(edge_options, ChromeEdges::default().id()),
            )
            .help(
                "Bevelled draws the two-line 3D edge: lit on the top and left, \
                 shaded on the bottom and right, so a button looks pressable and a \
                 well looks inset. Flat draws one plain border in the same place. \
                 Nothing moves either way - only what is painted inside the edge \
                 changes.",
            ),
            SettingSpec::new(
                keys::DENSITY,
                "Density",
                choice(density_options, Density::default().id()),
            )
            .help(
                "How much space sits between and inside controls. Comfortable is \
                 the shipped spacing. Compact and Dense tighten the gaps and the \
                 padding - about one extra toolbar control per row at Dense - but \
                 nothing you can click ever gets smaller than 24 points on a side, \
                 so it stays usable with a finger.",
            ),
            SettingSpec::new(
                keys::UI_SCALE,
                "Interface scale",
                choice(scale_options, UiScale::default().id()),
            )
            .help(
                "Scales the entire interface - type, spacing, borders and hit \
                 targets together - on top of whatever scaling the display \
                 already applies. 100 % is the native size. Raise it if 12.5-point \
                 type is small on a 4K panel; lower it to fit more instrument on \
                 screen. The radar imagery is not affected.",
            ),
        ],
    )
}

/// Rebuild an [`Appearance`] from stored ids.
///
/// Every argument is the raw stored string if there is one. `None`, or an id
/// this build does not recognise, takes the default for that axis and leaves
/// the stored value untouched — see the module docs.
///
/// One function, called from both places that need it: `main.rs` before the
/// first frame (from the raw store, because the registry does not exist yet)
/// and `app.rs` on every appearance change (from the resolved store). Two
/// copies of this rule would be two chances for the startup look and the
/// live look to disagree.
pub fn appearance_from_ids(
    theme: Option<&str>,
    accent: Option<&str>,
    edges: Option<&str>,
    density: Option<&str>,
    ui_scale: Option<&str>,
) -> Appearance {
    Appearance {
        theme: catalog::by_id_or_default(theme),
        accent: accent.and_then(Accent::from_id).unwrap_or_default(),
        edges: edges.and_then(ChromeEdges::from_id).unwrap_or_default(),
        density: density.and_then(Density::from_id).unwrap_or_default(),
        ui_scale: ui_scale.and_then(UiScale::from_id).unwrap_or_default(),
    }
}
