//! The two colour palettes of the workstation theme, stated as numbers.
//!
//! Every colour the theme uses is declared here once, as a named role, so the
//! widget styling in [`super`] and the bevel primitives in [`super::bevel`]
//! cannot drift apart: both read the same [`Palette`]. The values themselves
//! are design decisions, recorded in the table in the [`super`] module doc and
//! pinned by the contrast tests in `tests/theme_contract.rs` — a later edit
//! that quietly drops a foreground below its WCAG floor fails a test rather
//! than shipping.
//!
//! Contrast floors follow W3C, "Web Content Accessibility Guidelines (WCAG)
//! 2.2", W3C Recommendation, 2023: SC 1.4.3 (contrast minimum, 4.5:1 for
//! text) and SC 1.4.11 (non-text contrast, 3:1 for UI graphics). The bevel
//! grammar the four `hi_*`/`sh_*` roles serve is from "The Windows Interface
//! Guidelines for Software Design", Microsoft Press, 1995, ch. 13 — the
//! light-from-top-left convention this whole theme is built on.

use eframe::egui::Color32;

use super::Variant;

/// Every colour role the theme paints with, for one variant.
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
    /// The fill of inset content — text edits, lists, data readouts. In the
    /// light variant this is near-paper; in the dark variant it is *darker*
    /// than the chrome, so data areas sit visually behind the instrument.
    pub well: Color32,
    /// Primary text. Pinned at ≥ 7:1 against both `face` and `well`.
    pub text: Color32,
    /// Secondary text: hints, captions, status lines. Pinned at ≥ 4.5:1.
    pub text_weak: Color32,
    /// Text of a disabled control. Deliberately *below* the WCAG floor —
    /// illegibility-by-degree is the disabled affordance — but still present.
    pub text_disabled: Color32,
    /// The 1-px outline of controls at rest. Visible affordance: a button has
    /// an edge you can see before you hover it.
    pub border: Color32,
    /// The outline of a control that is hovered, pressed, or a window edge.
    pub border_strong: Color32,
    /// Hyperlinks and the open-combo accent edge. Pinned ≥ 4.5:1 on `face`.
    pub link: Color32,
    /// Fill behind selected text and selected list rows.
    pub selection_bg: Color32,
    /// Text on `selection_bg`; also egui's focus-ring colour, so it must be
    /// visible against `face` as well as against `selection_bg`.
    pub selection_text: Color32,
    /// The fill of a latched (toggled-on) toolbar button: `face` pulled
    /// toward the accent, sitting under a sunken bevel.
    pub selection_tint: Color32,
    /// Warning text on `face`. Amber, tuned per variant to hold ≥ 4.5:1.
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

/// The light ("daylight bench") palette.
///
/// The classic `#C0C0C0` face family lifted well out of dinge — `face` is
/// `#D8D5CE`, about nine percent brighter and a hair warm, so on a modern
/// panel it reads as brushed instrument grey rather than as an old dialog.
pub const LIGHT: Palette = Palette {
    face: Color32::from_rgb(216, 213, 206),
    face_raised: Color32::from_rgb(228, 226, 220),
    face_pressed: Color32::from_rgb(198, 195, 188),
    hover: Color32::from_rgb(236, 234, 228),
    well: Color32::from_rgb(250, 249, 245),
    text: Color32::from_rgb(28, 27, 25),
    text_weak: Color32::from_rgb(88, 86, 81),
    text_disabled: Color32::from_rgb(139, 137, 132),
    border: Color32::from_rgb(146, 143, 137),
    border_strong: Color32::from_rgb(94, 92, 87),
    link: Color32::from_rgb(43, 84, 148),
    selection_bg: Color32::from_rgb(167, 190, 219),
    selection_text: Color32::from_rgb(16, 45, 85),
    selection_tint: Color32::from_rgb(181, 187, 194),
    warn: Color32::from_rgb(128, 74, 0),
    error: Color32::from_rgb(170, 36, 28),
    hi_outer: Color32::from_rgb(255, 255, 255),
    hi_inner: Color32::from_rgb(238, 236, 230),
    sh_inner: Color32::from_rgb(150, 147, 141),
    sh_outer: Color32::from_rgb(94, 92, 87),
};

/// The dark ("night bench") palette, because radar analysts work at night.
///
/// Graphite, not black: `face` is `#36393E`, bright enough that the bevel
/// grammar still has room on both sides — a lit edge above it and two shade
/// steps below it — where a near-black chrome would leave the language
/// nowhere to go. Data wells are darker than the chrome so imagery pops.
pub const DARK: Palette = Palette {
    face: Color32::from_rgb(54, 57, 62),
    face_raised: Color32::from_rgb(63, 66, 72),
    face_pressed: Color32::from_rgb(42, 44, 48),
    hover: Color32::from_rgb(70, 74, 80),
    well: Color32::from_rgb(24, 26, 29),
    text: Color32::from_rgb(230, 228, 225),
    text_weak: Color32::from_rgb(162, 165, 169),
    text_disabled: Color32::from_rgb(106, 109, 114),
    border: Color32::from_rgb(94, 99, 106),
    border_strong: Color32::from_rgb(130, 136, 144),
    link: Color32::from_rgb(125, 168, 222),
    selection_bg: Color32::from_rgb(46, 90, 150),
    selection_text: Color32::from_rgb(235, 240, 247),
    selection_tint: Color32::from_rgb(51, 69, 93),
    warn: Color32::from_rgb(232, 163, 61),
    error: Color32::from_rgb(244, 130, 120),
    hi_outer: Color32::from_rgb(98, 103, 110),
    hi_inner: Color32::from_rgb(72, 76, 82),
    sh_inner: Color32::from_rgb(35, 37, 40),
    sh_outer: Color32::from_rgb(15, 16, 18),
};

impl Palette {
    /// The palette of a variant.
    pub const fn of(variant: Variant) -> &'static Self {
        match variant {
            Variant::Light => &LIGHT,
            Variant::Dark => &DARK,
        }
    }

    /// The palette in force for a `Ui`, read from the style the theme
    /// installed (`Visuals::dark_mode`). This is how the bevel helpers find
    /// their colours without every caller threading a [`Variant`] around.
    pub fn detect(ui: &eframe::egui::Ui) -> &'static Self {
        if ui.visuals().dark_mode {
            &DARK
        } else {
            &LIGHT
        }
    }
}
