//! The unified visual language of the workstation: Windows 95, but modern.
//!
//! One theme for the whole app, applied once at startup with
//! [`apply`] (or [`install`] to follow the OS light/dark preference), plus
//! the bevel primitives in [`bevel`] that other modules use to compose the
//! same language instead of copying colours. The radar pane's own chrome —
//! `map_scene::MapChrome` — stays authoritative for the map itself; this
//! module governs the instrument around it.
//!
//! # What "Windows 95 but modern" means here
//!
//! The *structural* grammar of Win95, kept: square corners everywhere;
//! chunky raised/sunken bevels with light falling from the top-left, so a
//! button looks pressable and a data well looks inset before you touch
//! either; etched group boxes with captions; visible control borders; dense,
//! professional spacing. The grammar is from "The Windows Interface
//! Guidelines for Software Design", Microsoft Press, 1995, ch. 13.
//!
//! The *execution*, modernised: every bevel line is one physical pixel at
//! any DPI (snapped to the device grid — crisp hairlines at 2×, never grey
//! mush); the pure-black outline is replaced by deep neutrals; the palette is
//! re-tuned so the classic `#C0C0C0` face family no longer looks dingy
//! (light face `#D8D5CE`); and there is a dark variant with the same bevel
//! physics, because radar analysts work at night. No skeuomorphic kitsch: no
//! textures, no gradients, and only small crisp shadows under floating
//! windows and menus. Toolbars are flat-until-hover in the Office 97 manner
//! — flatness is visual, never a smaller hit target.
//!
//! # Palette (both variants, pinned by `tests/theme_contract.rs`)
//!
//! | Role            | Light       | Dark        |
//! |-----------------|-------------|-------------|
//! | face (panels)   | `#D8D5CE`   | `#36393E`   |
//! | raised face     | `#E4E2DC`   | `#3F4248`   |
//! | pressed face    | `#C6C3BC`   | `#2A2C30`   |
//! | hover face      | `#ECEAE4`   | `#464A50`   |
//! | well (data)     | `#FAF9F5`   | `#181A1D`   |
//! | text            | `#1C1B19`   | `#E6E4E1`   |
//! | weak text       | `#585651`   | `#A2A5A9`   |
//! | border          | `#928F89`   | `#5E636A`   |
//! | link / accent   | `#2B5494`   | `#7DA8DE`   |
//! | selection       | `#A7BEDB` on `#102D55` text | `#2E5A96` on `#EBF0F7` text |
//! | bevel lit       | `#FFFFFF` / `#EEECE6` | `#62676E` / `#484C52` |
//! | bevel shade     | `#96938D` / `#5E5C57` | `#232528` / `#0F1012` |
//!
//! Contrast floors are tested, not asserted in prose: primary text ≥ 7:1 on
//! face and well, all other foregrounds ≥ 4.5:1, bevel edges ≥ 1.3:1 against
//! the face they sculpt (W3C WCAG 2.2, 2023, SC 1.4.3 / 1.4.11).
//!
//! # Metrics
//!
//! Text: the fonts egui already bundles — Ubuntu-Light for UI text, Hack for
//! monospace readouts; no font dependency is added. Sizes: Small 10, Body
//! 12.5, Button 12.5, Heading 15.5, Monospace 12 — professional density, one
//! step tighter than egui's defaults. Every interactive element keeps a hit
//! target of at least 24 points per side (WCAG 2.2 SC 2.5.8; mobile is a
//! standing requirement), enforced through `Spacing::interact_size` for
//! egui's widgets and by construction in [`bevel`]'s helpers. Scroll bars
//! are solid and allocated — a visible, grabbable, finger-sized channel —
//! not floating overlays.
//!
//! One stock widget escapes the style: `egui::ProgressBar` defaults to a
//! pill shape that ignores the widget corner radius. Call sites keep the
//! language by passing `.corner_radius(CornerRadius::ZERO)`, as
//! `examples/theme_gallery.rs` demonstrates.

// The explicit `#[path]`s are what let `tests/theme_contract.rs` and
// `examples/theme_gallery.rs` include this module by `#[path]` before it is
// wired into `main.rs`: children of a `#[path]`-included file resolve
// beside the file rather than under `theme/`, and these attributes resolve
// identically (relative to `src/`) from both compilation paths.
#[path = "theme/bevel.rs"]
pub mod bevel;
#[path = "theme/palette.rs"]
pub mod palette;

use std::collections::BTreeMap;

use eframe::egui::style::{
    HandleShape, ScrollStyle, Selection, TextCursorStyle, WidgetVisuals, Widgets,
};
use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme,
    ThemePreference, Visuals, vec2,
};

use palette::Palette;

/// Which of the two looks is in force.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Variant {
    /// Graphite chrome, data wells darker than the panels. The default,
    /// because the app's audience works storms at night.
    #[default]
    Dark,
    /// Instrument grey, paper-light data wells.
    Light,
}

impl Variant {
    /// The egui theme slot this variant styles.
    pub const fn egui_theme(self) -> Theme {
        match self {
            Self::Dark => Theme::Dark,
            Self::Light => Theme::Light,
        }
    }
}

/// Install the theme and pin the app to `variant`.
///
/// Both egui theme slots are styled — so anything that later flips the
/// preference still lands on this language, never on stock egui — and the
/// preference is set to `variant`. One call at startup is the entire
/// integration.
pub fn apply(ctx: &egui::Context, variant: Variant) {
    install(ctx);
    ctx.set_theme(match variant {
        Variant::Dark => ThemePreference::Dark,
        Variant::Light => ThemePreference::Light,
    });
}

/// Install both variants and let the OS light/dark preference choose between
/// them. Use [`apply`] to pin one explicitly.
pub fn install(ctx: &egui::Context) {
    ctx.set_style_of(Theme::Dark, style(Variant::Dark));
    ctx.set_style_of(Theme::Light, style(Variant::Light));
}

/// The complete [`egui::Style`] for one variant.
pub fn style(variant: Variant) -> egui::Style {
    let mut style = egui::Style {
        text_styles: text_styles(),
        visuals: visuals(variant),
        // Near-instant state changes: the language is mechanical, not
        // animated. Kept just above zero so egui's fades still resolve.
        animation_time: 0.06,
        ..egui::Style::default()
    };
    spacing(&mut style.spacing);
    style
}

/// The type ramp, on the fonts egui already bundles.
fn text_styles() -> BTreeMap<TextStyle, FontId> {
    [
        (
            TextStyle::Small,
            FontId::new(10.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(12.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(15.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
    ]
    .into()
}

/// Dense professional spacing that never shrinks a hit target below 24 pt.
fn spacing(spacing: &mut egui::style::Spacing) {
    spacing.item_spacing = vec2(6.0, 4.0);
    spacing.window_margin = Margin::same(8);
    spacing.menu_margin = Margin::same(6);
    spacing.button_padding = vec2(10.0, 4.0);
    // The floor for every interactive widget's height, and the touch rule.
    spacing.interact_size = vec2(44.0, bevel::MIN_TOUCH_POINTS);
    spacing.slider_width = 140.0;
    spacing.slider_rail_height = 6.0;
    spacing.text_edit_width = 240.0;
    spacing.icon_width = 15.0;
    spacing.icon_width_inner = 9.0;
    spacing.icon_spacing = 5.0;
    spacing.tooltip_width = 420.0;
    spacing.combo_height = 260.0;
    // A solid, allocated scroll channel — visible and grabbable, like the
    // instrument it belongs to — sized for fingers.
    spacing.scroll = ScrollStyle {
        bar_width: 12.0,
        handle_min_length: 24.0,
        ..ScrollStyle::solid()
    };
}

/// The colours and shapes for one variant.
fn visuals(variant: Variant) -> Visuals {
    let palette = Palette::of(variant);
    let base = match variant {
        // Start from egui's own mode defaults so mode-conditional details
        // this function does not name (text-alpha handling, cursor
        // previews) stay correct for the mode.
        Variant::Dark => Visuals::dark(),
        Variant::Light => Visuals::light(),
    };
    let shadow_alpha = match variant {
        Variant::Dark => 130,
        Variant::Light => 60,
    };
    Visuals {
        widgets: widgets(palette),
        selection: Selection {
            bg_fill: palette.selection_bg,
            stroke: Stroke::new(1.0, palette.selection_text),
        },
        hyperlink_color: palette.link,
        faint_bg_color: match variant {
            Variant::Dark => Color32::from_white_alpha(4),
            Variant::Light => Color32::from_black_alpha(7),
        },
        extreme_bg_color: palette.well,
        text_edit_bg_color: Some(palette.well),
        code_bg_color: match variant {
            Variant::Dark => Color32::from_rgb(30, 32, 36),
            Variant::Light => Color32::from_rgb(233, 231, 225),
        },
        warn_fg_color: palette.warn,
        error_fg_color: palette.error,
        // Square. Everywhere. This is the single loudest sentence of the
        // language.
        window_corner_radius: CornerRadius::ZERO,
        menu_corner_radius: CornerRadius::ZERO,
        // Small, crisp, close: enough to seat a floating window on the
        // panel, nothing like a soft material shadow.
        window_shadow: Shadow {
            offset: [2, 3],
            blur: 4,
            spread: 0,
            color: Color32::from_black_alpha(shadow_alpha),
        },
        popup_shadow: Shadow {
            offset: [2, 2],
            blur: 3,
            spread: 0,
            color: Color32::from_black_alpha(shadow_alpha),
        },
        window_fill: palette.face,
        window_stroke: Stroke::new(1.0, palette.border_strong),
        panel_fill: palette.face,
        text_cursor: TextCursorStyle {
            stroke: Stroke::new(2.0, palette.text),
            ..base.text_cursor
        },
        striped: true,
        slider_trailing_fill: false,
        // A rectangular slider thumb: the Win95 pointer, not a modern pill.
        handle_shape: HandleShape::Rect { aspect_ratio: 0.6 },
        image_loading_spinners: false,
        ..base
    }
}

/// The five interaction states, mapped onto the raised/pressed face steps.
///
/// egui widgets draw one uniform stroke, so this is the language's
/// approximation for stock widgets: face steps do the raising and pressing,
/// a visible border does the affordance. Chrome that wants the true
/// two-line bevel composes it from [`bevel`].
fn widgets(palette: &Palette) -> Widgets {
    let corner_radius = CornerRadius::ZERO;
    Widgets {
        noninteractive: WidgetVisuals {
            weak_bg_fill: palette.face,
            bg_fill: palette.face,
            bg_stroke: Stroke::new(1.0, palette.border),
            fg_stroke: Stroke::new(1.0, palette.text),
            corner_radius,
            expansion: 0.0,
        },
        inactive: WidgetVisuals {
            // Buttons stand one step proud of the panel...
            weak_bg_fill: palette.face_raised,
            // ...while checkbox and slider-rail grounds read as small wells.
            bg_fill: palette.well,
            bg_stroke: Stroke::new(1.0, palette.border),
            fg_stroke: Stroke::new(1.0, palette.text),
            corner_radius,
            expansion: 0.0,
        },
        hovered: WidgetVisuals {
            weak_bg_fill: palette.hover,
            bg_fill: palette.well,
            bg_stroke: Stroke::new(1.0, palette.border_strong),
            fg_stroke: Stroke::new(1.0, palette.text),
            corner_radius,
            expansion: 0.0,
        },
        active: WidgetVisuals {
            weak_bg_fill: palette.face_pressed,
            bg_fill: palette.face_pressed,
            bg_stroke: Stroke::new(1.0, palette.border_strong),
            fg_stroke: Stroke::new(1.0, palette.text),
            corner_radius,
            expansion: 0.0,
        },
        open: WidgetVisuals {
            weak_bg_fill: palette.face_pressed,
            bg_fill: palette.well,
            bg_stroke: Stroke::new(1.0, palette.link),
            fg_stroke: Stroke::new(1.0, palette.text),
            corner_radius,
            expansion: 0.0,
        },
    }
}
