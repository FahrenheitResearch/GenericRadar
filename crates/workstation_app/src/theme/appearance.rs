//! The customization axes: what an analyst can change about the chrome
//! without a new theme.
//!
//! A theme (see [`super::catalog`]) decides the colours. These four axes
//! decide everything else, and every one of them ships at the value that
//! reproduces the founding look exactly, so a fresh install is byte-for-byte
//! what the application has always drawn:
//!
//! | Axis | Default | What it moves |
//! |------|---------|---------------|
//! | [`UiScale`] | `100 %` | egui's `pixels_per_point`, through the zoom factor |
//! | [`Density`] | `Comfortable` | item spacing, control padding, row heights |
//! | [`Accent`] | `Theme` | the selection / focus / link / latch colour |
//! | [`ChromeEdges`] | `Bevelled` | two-line 3D bevels, or plain borders |
//!
//! All four are one value in [`Appearance`], which is what
//! `super::apply` installs and what `super::chrome` hands back to the bevel
//! primitives. Nothing reads a global; the appearance travels in the egui
//! context, so two contexts (the app and an offscreen contact sheet) can be
//! on different appearances at the same time.
//!
//! Contrast floors follow W3C, "Web Content Accessibility Guidelines (WCAG)
//! 2.2", W3C Recommendation, 2023: SC 1.4.3 (4.5:1 for text), SC 1.4.11
//! (3:1 for user-interface graphics), SC 2.5.8 (24-px minimum target). Every
//! accent below is measured against every registered theme by
//! `tests/theme_catalog.rs`.

use eframe::egui::{Color32, Vec2};

use super::bevel::MIN_TOUCH_POINTS;
use super::catalog::{self, Ground, ThemeSpec};
use super::palette::Palette;

/// The whole appearance of the chrome: a theme plus the four axes.
///
/// `Copy`, and small: it is stored in the egui context and read by every
/// bevel primitive on every frame, so it must cost nothing to hand around.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    /// Which registered theme's colours are in force.
    pub theme: &'static ThemeSpec,
    /// The selection / focus / link colour, or the theme's own.
    pub accent: Accent,
    /// How tightly the chrome is packed.
    pub density: Density,
    /// Bevelled or flat.
    pub edges: ChromeEdges,
    /// The per-user multiplier on egui's `pixels_per_point`.
    pub ui_scale: UiScale,
}

impl Default for Appearance {
    /// Exactly what the application drew before any of this was settable.
    fn default() -> Self {
        Self {
            theme: catalog::DEFAULT,
            accent: Accent::Theme,
            density: Density::Comfortable,
            edges: ChromeEdges::Bevelled,
            ui_scale: UiScale::Normal,
        }
    }
}

impl Appearance {
    /// The default axes on the named theme; an unknown id lands on the
    /// default theme. The constructor the proofs and tests use.
    pub fn by_id(theme_id: &str) -> Self {
        Self {
            theme: catalog::by_id_or_default(Some(theme_id)),
            ..Self::default()
        }
    }

    /// The default axes on the default theme of a ground.
    pub fn on_ground(ground: Ground) -> Self {
        Self {
            theme: catalog::default_for(ground),
            ..Self::default()
        }
    }

    /// The theme's palette with the chosen accent in it.
    ///
    /// [`Accent::Theme`] returns the theme's own colours unchanged, which is
    /// what keeps a fresh install byte-identical to the look this catalog
    /// was carved out of.
    pub fn palette(&self) -> Palette {
        match self.accent.tokens(self.theme.ground) {
            Some(accent) => self.theme.palette.with_accent(accent),
            None => self.theme.palette,
        }
    }

    /// The two appearances egui's two style slots are filled with: the
    /// chosen one on its own ground, and the same axes on the other
    /// ground's default theme.
    ///
    /// Both slots carry the language, so an OS light/dark flip - or an app
    /// that changes `ThemePreference` behind the theme's back - can never
    /// land on stock egui. The axes travel to the other slot because they
    /// are the analyst's choices, not the theme's.
    pub fn slots(&self) -> (Self, Self) {
        let other = Self {
            theme: catalog::default_for(self.theme.ground.opposite()),
            ..*self
        };
        match self.theme.ground {
            Ground::Light => (*self, other),
            Ground::Dark => (other, *self),
        }
    }
}

// ---------------------------------------------------------------------------
// UI scale
// ---------------------------------------------------------------------------

/// The per-user multiplier on egui's `pixels_per_point`.
///
/// Fixed steps rather than a free slider: the value is a stored string, and
/// a stored `1.0499999523162842` is a bug waiting for a future rounding
/// change. The range is the accessibility one that matters on a 4K panel -
/// 80 % for an analyst who wants more instrument on screen, 160 % for one
/// who cannot read 12.5-point type at native scale.
///
/// This multiplies whatever the platform already reports, so a 150 % Windows
/// display at 125 % here lands at 187.5 % and the bevels stay one physical
/// pixel at all of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiScale {
    Smallest,
    Smaller,
    #[default]
    Normal,
    Larger,
    Large,
    Largest,
    Huge,
}

impl UiScale {
    pub const ALL: [Self; 7] = [
        Self::Smallest,
        Self::Smaller,
        Self::Normal,
        Self::Larger,
        Self::Large,
        Self::Largest,
        Self::Huge,
    ];

    /// The stored id. The number itself, so a settings file reads plainly.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Smallest => "0.80",
            Self::Smaller => "0.90",
            Self::Normal => "1.00",
            Self::Larger => "1.10",
            Self::Large => "1.25",
            Self::Largest => "1.40",
            Self::Huge => "1.60",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Smallest => "80 %",
            Self::Smaller => "90 %",
            Self::Normal => "100 % (native)",
            Self::Larger => "110 %",
            Self::Large => "125 %",
            Self::Largest => "140 %",
            Self::Huge => "160 %",
        }
    }

    pub const fn factor(self) -> f32 {
        match self {
            Self::Smallest => 0.80,
            Self::Smaller => 0.90,
            Self::Normal => 1.00,
            Self::Larger => 1.10,
            Self::Large => 1.25,
            Self::Largest => 1.40,
            Self::Huge => 1.60,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scale| scale.id() == id)
    }
}

// ---------------------------------------------------------------------------
// Density
// ---------------------------------------------------------------------------

/// How tightly the chrome is packed.
///
/// One axis, moved by one setting: spacing, control padding and row heights
/// travel together, because a toolbar whose buttons shrank while its gaps
/// did not is not denser, it is just wrong.
///
/// [`MIN_TOUCH_POINTS`] is a FLOOR, not a scaled quantity: every interactive
/// control keeps a hit target of at least 24 points per side in all three
/// densities (WCAG 2.2 SC 2.5.8, and this application ships to glass).
/// `Dense` buys its density from the space BETWEEN controls and from the
/// padding inside oversized ones - never from the size of the target itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    /// The shipped spacing: what the application has always drawn.
    #[default]
    Comfortable,
    /// A step tighter. About one extra toolbar control per row.
    Compact,
    /// As tight as the touch floor allows.
    Dense,
}

impl Density {
    pub const ALL: [Self; 3] = [Self::Comfortable, Self::Compact, Self::Dense];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Comfortable => "comfortable",
            Self::Compact => "compact",
            Self::Dense => "dense",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Comfortable => "Comfortable",
            Self::Compact => "Compact",
            Self::Dense => "Dense",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|density| density.id() == id)
    }

    /// Every measurement the chrome lays out with, for this density.
    pub const fn metrics(self) -> DensityMetrics {
        match self {
            Self::Comfortable => DensityMetrics {
                item_spacing: Vec2::new(6.0, 4.0),
                button_padding: Vec2::new(10.0, 4.0),
                window_margin: 8,
                menu_margin: 6,
                interact_width: 44.0,
                slider_width: 140.0,
                text_edit_width: 240.0,
                icon_width: 15.0,
                icon_width_inner: 9.0,
                icon_spacing: 5.0,
                frame_margin: 6,
                group_margin_x: 10,
                group_margin_bottom: 8,
                group_caption_gap: 6.0,
                control_padding: Vec2::new(10.0, 4.0),
                readout_padding: Vec2::new(7.0, 2.0),
                separator_thickness: 7.0,
            },
            Self::Compact => DensityMetrics {
                item_spacing: Vec2::new(5.0, 3.0),
                button_padding: Vec2::new(8.0, 3.0),
                window_margin: 6,
                menu_margin: 5,
                interact_width: 40.0,
                slider_width: 128.0,
                text_edit_width: 224.0,
                icon_width: 14.0,
                icon_width_inner: 8.0,
                icon_spacing: 4.0,
                frame_margin: 5,
                group_margin_x: 8,
                group_margin_bottom: 6,
                group_caption_gap: 5.0,
                control_padding: Vec2::new(8.0, 3.0),
                readout_padding: Vec2::new(6.0, 2.0),
                separator_thickness: 6.0,
            },
            Self::Dense => DensityMetrics {
                item_spacing: Vec2::new(4.0, 2.0),
                button_padding: Vec2::new(6.0, 2.0),
                window_margin: 5,
                menu_margin: 4,
                interact_width: 36.0,
                slider_width: 116.0,
                text_edit_width: 208.0,
                icon_width: 13.0,
                icon_width_inner: 8.0,
                icon_spacing: 4.0,
                frame_margin: 4,
                group_margin_x: 7,
                group_margin_bottom: 5,
                group_caption_gap: 4.0,
                control_padding: Vec2::new(6.0, 2.0),
                readout_padding: Vec2::new(5.0, 2.0),
                separator_thickness: 5.0,
            },
        }
    }
}

/// The numbers one [`Density`] lays the chrome out with.
///
/// Stated as data so the density axis is a table a reader can check rather
/// than arithmetic scattered through six widget helpers. Every field is in
/// points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DensityMetrics {
    /// egui's gap between consecutive widgets.
    pub item_spacing: Vec2,
    /// egui's padding inside a stock button.
    pub button_padding: Vec2,
    /// Margin inside a floating window.
    pub window_margin: i8,
    /// Margin inside a dropped menu.
    pub menu_margin: i8,
    /// The WIDTH half of egui's `interact_size`. The height half is
    /// [`MIN_TOUCH_POINTS`] in every density and is not listed here,
    /// because it is a floor rather than a choice.
    pub interact_width: f32,
    pub slider_width: f32,
    pub text_edit_width: f32,
    pub icon_width: f32,
    pub icon_width_inner: f32,
    pub icon_spacing: f32,
    /// Inner margin of `bevel::raised_frame` and `bevel::sunken_well`.
    pub frame_margin: i8,
    /// Left/right margin of `bevel::group_box`.
    pub group_margin_x: i8,
    /// Bottom margin of `bevel::group_box`.
    pub group_margin_bottom: i8,
    /// Space between a group box caption and its contents.
    pub group_caption_gap: f32,
    /// Padding inside `bevel::toolbar_button` / `toolbar_toggle` /
    /// `toolbar_menu`. The control is still at least
    /// [`MIN_TOUCH_POINTS`] on each side.
    pub control_padding: Vec2,
    /// Padding inside `bevel::sunken_readout`.
    pub readout_padding: Vec2,
    /// Cross-axis thickness of `bevel::etched_separator`. Not a hit target:
    /// a separator is not interactive.
    pub separator_thickness: f32,
}

impl DensityMetrics {
    /// egui's `interact_size`, with the touch floor already applied.
    pub const fn interact_size(&self) -> Vec2 {
        Vec2::new(self.interact_width, MIN_TOUCH_POINTS)
    }
}

// ---------------------------------------------------------------------------
// Chrome edges
// ---------------------------------------------------------------------------

/// How the chrome draws its edges.
///
/// Both modes lay out identically - this changes what
/// `bevel::paint_bevel` paints INSIDE a rect, never the rect - so switching
/// cannot move a control by a pixel. `Flat` is one border line where
/// `Bevelled` draws the two-line 3D edge; the raised/sunken distinction is
/// then carried by the fill, which the language already sets (a button is
/// `face_raised`, a well is `well`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChromeEdges {
    /// The Win95 raised / etched / sunken language. The shipped look.
    #[default]
    Bevelled,
    /// Plain one-pixel borders, same geometry.
    Flat,
}

impl ChromeEdges {
    pub const ALL: [Self; 2] = [Self::Bevelled, Self::Flat];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Bevelled => "bevelled",
            Self::Flat => "flat",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Bevelled => "Bevelled",
            Self::Flat => "Flat",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|edges| edges.id() == id)
    }
}

// ---------------------------------------------------------------------------
// Accent
// ---------------------------------------------------------------------------

/// The four colour roles an accent owns.
///
/// These are exactly the places the chrome paints something because it is
/// selected, focused, active or a link - `super::visuals` wires all four
/// into egui, and `super::bevel` paints the latch and the focus ring from
/// them. Nothing else in the chrome carries an accent.
///
/// The radar panes are deliberately NOT on this list. A pane draws on the
/// map's own colours (`map_scene::MapChrome`) and its active-pane border is
/// part of that language, because data must not change colour when somebody
/// changes the chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccentTokens {
    /// Hyperlinks and the open-combo edge.
    pub link: Color32,
    /// Fill behind selected text and selected rows.
    pub selection_bg: Color32,
    /// Text on `selection_bg`, and egui's focus-ring colour.
    pub selection_text: Color32,
    /// Fill of a latched toolbar toggle.
    pub selection_tint: Color32,
}

/// The named accent colours.
///
/// A small named set rather than a colour picker: every entry is measured
/// against every registered theme by `tests/theme_catalog.rs`, and a picker
/// would hand an analyst a way to make their own instrument illegible with
/// no test able to stop them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Accent {
    /// Whatever the theme declared. The default, and the reason a fresh
    /// install is byte-identical to the look before this axis existed.
    #[default]
    Theme,
    Blue,
    Teal,
    Green,
    Amber,
    Violet,
}

impl Accent {
    pub const ALL: [Self; 6] = [
        Self::Theme,
        Self::Blue,
        Self::Teal,
        Self::Green,
        Self::Amber,
        Self::Violet,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Blue => "blue",
            Self::Teal => "teal",
            Self::Green => "green",
            Self::Amber => "amber",
            Self::Violet => "violet",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Theme => "Theme's own",
            Self::Blue => "Instrument blue",
            Self::Teal => "Teal",
            Self::Green => "Green",
            Self::Amber => "Amber",
            Self::Violet => "Violet",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|accent| accent.id() == id)
    }

    /// The four roles this accent paints on a given ground, or `None` for
    /// [`Accent::Theme`], which paints nothing and leaves the theme's own
    /// colours where they are.
    ///
    /// Keyed on the ground because an accent is ink as well as fill: the
    /// same hue has to be dark enough to read on a light face and light
    /// enough to read on a dark one, and one value cannot be both.
    pub const fn tokens(self, ground: Ground) -> Option<AccentTokens> {
        match (self, ground) {
            (Self::Theme, _) => None,
            (Self::Blue, Ground::Light) => Some(AccentTokens {
                link: Color32::from_rgb(43, 84, 148),
                selection_bg: Color32::from_rgb(167, 190, 219),
                selection_text: Color32::from_rgb(16, 45, 85),
                selection_tint: Color32::from_rgb(181, 187, 194),
            }),
            (Self::Blue, Ground::Dark) => Some(AccentTokens {
                link: Color32::from_rgb(125, 168, 222),
                selection_bg: Color32::from_rgb(46, 90, 150),
                selection_text: Color32::from_rgb(235, 240, 247),
                selection_tint: Color32::from_rgb(51, 69, 93),
            }),
            (Self::Teal, Ground::Light) => Some(AccentTokens {
                link: Color32::from_rgb(15, 90, 94),
                selection_bg: Color32::from_rgb(168, 208, 210),
                selection_text: Color32::from_rgb(5, 57, 59),
                selection_tint: Color32::from_rgb(176, 196, 196),
            }),
            (Self::Teal, Ground::Dark) => Some(AccentTokens {
                link: Color32::from_rgb(111, 195, 199),
                selection_bg: Color32::from_rgb(20, 85, 90),
                selection_text: Color32::from_rgb(228, 245, 246),
                selection_tint: Color32::from_rgb(44, 71, 73),
            }),
            (Self::Green, Ground::Light) => Some(AccentTokens {
                link: Color32::from_rgb(42, 93, 46),
                selection_bg: Color32::from_rgb(180, 213, 182),
                selection_text: Color32::from_rgb(20, 58, 23),
                selection_tint: Color32::from_rgb(184, 198, 182),
            }),
            (Self::Green, Ground::Dark) => Some(AccentTokens {
                link: Color32::from_rgb(130, 200, 136),
                selection_bg: Color32::from_rgb(42, 94, 49),
                selection_text: Color32::from_rgb(232, 246, 233),
                selection_tint: Color32::from_rgb(53, 73, 47),
            }),
            (Self::Amber, Ground::Light) => Some(AccentTokens {
                link: Color32::from_rgb(138, 75, 0),
                selection_bg: Color32::from_rgb(232, 207, 160),
                selection_text: Color32::from_rgb(74, 42, 0),
                selection_tint: Color32::from_rgb(205, 195, 169),
            }),
            (Self::Amber, Ground::Dark) => Some(AccentTokens {
                link: Color32::from_rgb(224, 168, 96),
                selection_bg: Color32::from_rgb(110, 74, 18),
                selection_text: Color32::from_rgb(251, 238, 220),
                selection_tint: Color32::from_rgb(74, 64, 52),
            }),
            (Self::Violet, Ground::Light) => Some(AccentTokens {
                link: Color32::from_rgb(91, 62, 155),
                selection_bg: Color32::from_rgb(201, 188, 230),
                selection_text: Color32::from_rgb(46, 26, 87),
                selection_tint: Color32::from_rgb(192, 183, 206),
            }),
            (Self::Violet, Ground::Dark) => Some(AccentTokens {
                link: Color32::from_rgb(180, 154, 232),
                selection_bg: Color32::from_rgb(74, 56, 128),
                selection_text: Color32::from_rgb(240, 234, 251),
                selection_tint: Color32::from_rgb(62, 58, 85),
            }),
        }
    }
}
