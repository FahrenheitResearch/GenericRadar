//! The token set: every colour the chrome draws, named by role.
//!
//! One [`Palette`] is the whole colour vocabulary of one theme. The widget
//! styling in [`super`] and the bevel primitives in [`super::bevel`] read the
//! same struct, which is what keeps them from drifting apart, and a
//! registered theme in [`super::catalog`] is exactly one of these plus its
//! identity. There are no per-widget colours anywhere in the chrome: if a
//! control needs a colour that is not a role here, the role is missing.
//!
//! The values themselves live in the theme files (`src/theme/light.rs`,
//! `src/theme/dark.rs`, and one file per theme after them). This module owns
//! only the vocabulary and the one operation performed on it - swapping in a
//! chosen accent, [`Palette::with_accent`].
//!
//! Contrast floors follow W3C, "Web Content Accessibility Guidelines (WCAG)
//! 2.2", W3C Recommendation, 2023: SC 1.4.3 (contrast minimum, 4.5:1 for
//! text) and SC 1.4.11 (non-text contrast, 3:1 for UI graphics). They are
//! tested, not asserted in prose - `tests/theme_catalog.rs` measures every
//! registered theme against every accent and names the pairing it failed on.
//! The bevel grammar the four `hi_*`/`sh_*` roles serve is from "The Windows
//! Interface Guidelines for Software Design", Microsoft Press, 1995, ch. 13 -
//! the light-from-top-left convention this whole theme is built on.

use eframe::egui::Color32;

use super::appearance::AccentTokens;
use super::catalog::{self, Ground};

/// Every colour role the chrome paints with, for one theme.
///
/// Roles, not widgets: a button and a combo box share `face_raised` rather
/// than each declaring a colour, which is what keeps the app looking like one
/// instrument instead of a parts bin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// The ground everything sits on: panels, window bodies, toolbar strips.
    pub face: Color32,
    /// The face of something that stands proud of the panel — a button at
    /// rest. One step lighter than `face` so controls read as *on* the panel.
    pub face_raised: Color32,
    /// The face of a control while it is pressed. One step darker than
    /// `face`, which together with the sunken bevel is the "pushed in" cue.
    pub face_pressed: Color32,
    /// The face of a control under the pointer. The lightest face step.
    pub hover: Color32,
    /// The fill of inset content — text edits, lists, data readouts. On a
    /// light ground this is near-paper; on a dark one it is *darker* than the
    /// chrome, so data areas sit visually behind the instrument.
    pub well: Color32,
    /// Primary text. Pinned at ≥ 7:1 against `face`, `face_raised` and
    /// `well`.
    pub text: Color32,
    /// Secondary text: hints, captions, status lines. Pinned at ≥ 4.5:1 on
    /// `face` and `well`.
    pub text_weak: Color32,
    /// Text of a disabled control. Deliberately *below* the WCAG text floor —
    /// illegibility-by-degree is the disabled affordance, and SC 1.4.3
    /// exempts inactive components — but pinned above a presence floor so it
    /// never disappears, and pinned weaker than `text_weak` so a disabled
    /// control can never read as a live one.
    pub text_disabled: Color32,
    /// The 1-px outline of controls at rest. Visible affordance: a button has
    /// an edge you can see before you hover it.
    pub border: Color32,
    /// The outline of a control that is hovered or pressed, a window edge,
    /// and — in `ChromeEdges::Flat` — every edge the bevels would have drawn.
    /// Pinned at ≥ 3:1 on `face` and `well` for that reason (SC 1.4.11).
    pub border_strong: Color32,
    /// Hyperlinks and the open-combo accent edge. Pinned ≥ 4.5:1 on `face`
    /// and `well`. Owned by the accent axis.
    pub link: Color32,
    /// Fill behind selected text and selected list rows. Owned by the accent
    /// axis.
    pub selection_bg: Color32,
    /// Text on `selection_bg`; also egui's focus-ring colour, so it must be
    /// visible against `face` as well as against `selection_bg`. Owned by the
    /// accent axis.
    pub selection_text: Color32,
    /// The fill of a latched (toggled-on) toolbar button: `face` pulled
    /// toward the accent, sitting under a sunken bevel. Owned by the accent
    /// axis.
    pub selection_tint: Color32,
    /// Warning text on `face`. Amber, tuned per theme to hold ≥ 4.5:1.
    pub warn: Color32,
    /// Error text on `face`.
    pub error: Color32,
    /// Bevel: the outer lit edge (top/left of a raised block). The brightest
    /// value in the palette.
    pub hi_outer: Color32,
    /// Bevel: the inner lit edge, one step above `face`.
    pub hi_inner: Color32,
    /// Bevel: the inner shade edge, one step below `face`.
    pub sh_inner: Color32,
    /// Bevel: the outer shade edge (bottom/right of a raised block). Deep
    /// neutral, never pure black — the Win95 black outline is the one part of
    /// the original grammar this theme declines.
    pub sh_outer: Color32,
}

/// The daylight bench's palette. A convenience alias for the founding light
/// theme's tokens, kept because the contract tests and the toolbar audit name
/// it directly.
pub const LIGHT: Palette = catalog::light::THEME.palette;

/// The night bench's palette. See [`LIGHT`].
pub const DARK: Palette = catalog::dark::THEME.palette;

impl Palette {
    /// The default theme of a ground, as a palette.
    pub const fn of(ground: Ground) -> Self {
        catalog::default_for(ground).palette
    }

    /// This palette with a chosen accent's four roles swapped in.
    ///
    /// Only the four accent roles move. A theme's face steps, inks, borders
    /// and bevel ladder are what make it that theme, and an accent that
    /// touched them would be a second theme wearing the first one's name.
    pub const fn with_accent(mut self, accent: AccentTokens) -> Self {
        self.link = accent.link;
        self.selection_bg = accent.selection_bg;
        self.selection_text = accent.selection_text;
        self.selection_tint = accent.selection_tint;
        self
    }

    /// The palette in force for a `Ui`.
    ///
    /// Reads the appearance the theme installed into the egui context (see
    /// `super::chrome`), so the bevel helpers find their colours - including
    /// the analyst's chosen accent - without every caller threading an
    /// [`super::Appearance`] around.
    pub fn detect(ui: &eframe::egui::Ui) -> Self {
        super::chrome(ui).palette
    }
}
