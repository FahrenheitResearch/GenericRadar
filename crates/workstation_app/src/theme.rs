//! The unified visual language of the workstation: Windows 95, but modern,
//! with a catalog of colour themes.
//!
//! # The shape of this module
//!
//! * [`catalog`] — the theme registry. One theme is one const in one file;
//!   registering it is one line. This is where a new look goes.
//! * [`palette`] — the token set. Every colour the chrome draws, by role.
//! * [`appearance`] — the four customization axes (scale, density, accent,
//!   chrome edges) and [`Appearance`], the one value that carries a theme
//!   plus all four.
//! * [`bevel`] — the drawing primitives egui's uniform-stroke widgets cannot
//!   express, so other modules compose the language instead of copying
//!   colours.
//! * [`settings`] — the Appearance page, declared as data.
//!
//! One call installs everything: [`apply`] at startup and again whenever an
//! appearance setting changes. The radar pane's own chrome —
//! `map_scene::MapChrome` — stays authoritative for the map itself; this
//! module governs the instrument around it, and deliberately does not reach
//! into the panes. Data is not tinted by the chrome.
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
//! (light face `#D8D5CE`); and there is a dark theme with the same bevel
//! physics, because radar analysts work at night. No skeuomorphic kitsch: no
//! textures, no gradients, and only small crisp shadows under floating
//! windows and menus. Toolbars are flat-until-hover in the Office 97 manner
//! — flatness is visual, never a smaller hit target.
//!
//! `ChromeEdges::Flat` is the one deliberate exit from that grammar, offered
//! because some analysts want the geometry without the 3D language. It
//! changes what is painted inside a rect, never the rect, so nothing moves.
//!
//! # The ground
//!
//! A style is not a background. eframe hands [`eframe::App::ui`] a root
//! `egui::Ui` that "has no margin or background color" (eframe 0.34.3,
//! `epi.rs`), and clears the window to its own near-black default, so an app
//! that only installs a style paints light chrome onto raw near-black and
//! draws its bare labels — a product name, a tilt value, a status line — in
//! `#1C1B19` ink on it, invisible until something hovers a face underneath.
//! Two calls close that hole and both are required: [`paint_root_ground`] at
//! the top of `ui`, and [`clear_color`] from `App::clear_color` so the strip
//! the compositor exposes mid-resize is the same face.
//!
//! # The founding palettes (pinned by `tests/theme_catalog.rs`)
//!
//! | Role            | `light`     | `dark`      |
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
//! face and well, all other live foregrounds ≥ 4.5:1, flat borders ≥ 3:1,
//! bevel edges ≥ 1.3:1 against the face they sculpt (W3C WCAG 2.2, 2023,
//! SC 1.4.3 / 1.4.11). The audit runs over every registered theme crossed
//! with every accent, so new catalog entries receive the full measurement
//! set automatically.
//!
//! # Metrics
//!
//! Text: the fonts egui already bundles — Ubuntu-Light for UI text, Hack for
//! monospace readouts; no font dependency is added. Sizes: Small 10, Body
//! 12.5, Button 12.5, Heading 15.5, Monospace 12 — professional density, one
//! step tighter than egui's defaults, and scaled bodily by [`UiScale`]
//! rather than re-sized per style, so a 125 % analyst gets 125 % of
//! everything and the bevels stay one physical pixel. Spacing comes from
//! [`Density`]. Every interactive element keeps a hit target of at least 24
//! points per side (WCAG 2.2 SC 2.5.8; mobile is a standing requirement) in
//! every density, enforced through `Spacing::interact_size` for egui's
//! widgets and by construction in [`bevel`]'s helpers. Scroll bars are solid
//! and allocated — a visible, grabbable, finger-sized channel — not floating
//! overlays.
//!
//! One stock widget escapes the style: `egui::ProgressBar` defaults to a
//! pill shape that ignores the widget corner radius. Call sites keep the
//! language by passing `.corner_radius(CornerRadius::ZERO)`, as
//! `examples/theme_gallery.rs` demonstrates.

// The explicit `#[path]`s are what let `tests/theme_contract.rs`,
// `tests/theme_catalog.rs` and `examples/theme_gallery.rs` include this
// module by `#[path]`: children of a `#[path]`-included file resolve beside
// the file rather than under `theme/`, and these attributes resolve
// identically (relative to `src/`) from every compilation path. The theme
// files themselves are declared inside `catalog.rs`, whose own children
// resolve beside IT — that is, in `src/theme/` — which is what makes
// registering a theme one line in one file.
#[path = "theme/appearance.rs"]
pub mod appearance;
#[path = "theme/bevel.rs"]
pub mod bevel;
#[path = "theme/catalog.rs"]
pub mod catalog;
#[path = "theme/palette.rs"]
pub mod palette;
#[path = "theme/settings.rs"]
pub mod settings;

use std::collections::BTreeMap;

use eframe::egui::style::{
    HandleShape, ScrollStyle, Selection, TextCursorStyle, WidgetVisuals, Widgets,
};
use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme,
    ThemePreference, Visuals,
};

// The vocabulary, re-exported so a caller writes `theme::Density` rather
// than `theme::appearance::Density`. `unused_imports` is judged per
// compilation unit and the binary happens not to name all of them; the
// contract tests, the catalog audit and the contact sheet do.
#[allow(unused_imports)]
pub use appearance::{Accent, Appearance, ChromeEdges, Density, UiScale};
#[allow(unused_imports)]
pub use catalog::{Ground, ThemeSpec};
use palette::Palette;

/// Everything a chrome primitive needs to draw one control: the resolved
/// colours, how tight the layout is, and which edge language is in force.
///
/// Handed back by [`chrome`], which reads it out of the egui context rather
/// than a global, so an offscreen contact sheet can photograph six themes at
/// once without any of them seeing another's state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chrome {
    pub palette: Palette,
    pub density: Density,
    pub edges: ChromeEdges,
}

/// What [`install`] leaves in the egui context for [`chrome`] to find.
///
/// Both grounds' palettes are stored, not just the chosen one, because
/// [`install`] styles both of egui's theme slots: if something flips the
/// preference (the OS, a stray `set_theme`), the widgets change ground and
/// the bevel primitives have to change with them or the chrome tears in half.
#[derive(Clone, Copy)]
struct InstalledTheme {
    appearance: Appearance,
    light: Palette,
    dark: Palette,
}

fn state_id() -> egui::Id {
    egui::Id::new("radar-workstation::theme::installed")
}

/// Install `appearance` and pin the app to its theme's ground.
///
/// Both egui theme slots are styled — so anything that later flips the
/// preference still lands on this language, never on stock egui — and the
/// preference is set to the chosen theme's ground. Call this at startup and
/// after each appearance change.
pub fn apply(ctx: &egui::Context, appearance: &Appearance) {
    install(ctx, appearance);
    ctx.set_theme(match appearance.theme.ground {
        Ground::Light => ThemePreference::Light,
        Ground::Dark => ThemePreference::Dark,
    });
}

/// Style both slots and publish the appearance, without touching the
/// light/dark preference. Use [`apply`] to pin the ground explicitly.
pub fn install(ctx: &egui::Context, appearance: &Appearance) {
    let (light, dark) = appearance.slots();
    ctx.set_style_of(Theme::Light, style(&light));
    ctx.set_style_of(Theme::Dark, style(&dark));
    // The scale axis, applied where egui already has a home for it: the zoom
    // factor multiplies whatever `pixels_per_point` the platform reports, so
    // a 150 % Windows display at 125 % here lands at 187.5 % and every bevel
    // is still snapped to one physical pixel.
    ctx.set_zoom_factor(appearance.ui_scale.factor());
    let installed = InstalledTheme {
        appearance: *appearance,
        light: light.palette(),
        dark: dark.palette(),
    };
    ctx.data_mut(|data| data.insert_temp(state_id(), installed));
}

/// The chrome in force for a `Ui`.
///
/// Falls back to [`Appearance::default`] when nothing has been installed —
/// which is what a bare `egui::Context` in somebody else's test looks like —
/// so a primitive drawn without the theme still draws the shipped look
/// rather than panicking or reading stock egui's colours.
pub fn chrome(ui: &egui::Ui) -> Chrome {
    chrome_of(ui.ctx(), ui.visuals().dark_mode)
}

/// The chrome in force for a context on a given ground. `dark_mode` is
/// egui's own answer (`Visuals::dark_mode`), because a `Ui` can carry
/// overridden visuals and the primitives must follow the ground they are
/// actually painting on.
pub fn chrome_of(ctx: &egui::Context, dark_mode: bool) -> Chrome {
    let installed = ctx.data(|data| data.get_temp::<InstalledTheme>(state_id()));
    match installed {
        Some(installed) => Chrome {
            palette: if dark_mode {
                installed.dark
            } else {
                installed.light
            },
            density: installed.appearance.density,
            edges: installed.appearance.edges,
        },
        None => {
            let ground = if dark_mode {
                Ground::Dark
            } else {
                Ground::Light
            };
            let appearance = Appearance::on_ground(ground);
            Chrome {
                palette: appearance.palette(),
                density: appearance.density,
                edges: appearance.edges,
            }
        }
    }
}

/// The appearance installed in a context, or the default if none is.
pub fn active(ctx: &egui::Context) -> Appearance {
    ctx.data(|data| data.get_temp::<InstalledTheme>(state_id()))
        .map(|installed| installed.appearance)
        .unwrap_or_default()
}

/// The colour an [`eframe::App`] must clear its window to, so the ground the
/// OS sees on a resize agrees with the ground [`paint_root_ground`] paints.
///
/// eframe's default is `rgba(12, 12, 12, 180)` — a near-black stand-in
/// (`eframe` 0.34.3, `epi.rs`) that has nothing to do with any theme. When
/// the window is dragged bigger, the compositor shows that clear colour in
/// the newly exposed strip for the frame or two before egui lays out over
/// it; if it is not the panel face, the app tears a black seam on every
/// resize. Forced opaque: the default's alpha of 180 lets the desktop show
/// through, which on a dark wallpaper is the same near-black again.
pub fn clear_color(visuals: &Visuals) -> [f32; 4] {
    visuals.panel_fill.to_opaque().to_normalized_gamma_f32()
}

/// Paint the ground of a root [`egui::Ui`] — the one eframe hands to
/// [`eframe::App::ui`], which "has no margin or background color" (eframe
/// 0.34.3, `epi.rs`).
///
/// Without this the app's widgets float on the raw window clear colour, and
/// every text run that is not inside a well or a button is drawn straight
/// onto it: on the light ground that is `#1C1B19` ink on near-black, which
/// is invisible until a hover happens to paint a face under it. Call this
/// FIRST in `ui`, before anything else allocates, so the fill lands behind
/// the frame's own shapes.
///
/// The rect covers the root `Ui`'s extent unioned with the viewport's content
/// area, because on the frame a resize lands the layout is still the previous
/// frame's and the `Ui` alone would leave the new strip bare. The painter's
/// clip keeps that union honest.
pub fn paint_root_ground(ui: &egui::Ui) {
    let ground = ui.visuals().panel_fill;
    let rect = ui.max_rect().union(ui.ctx().content_rect());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, ground);
}

/// The complete [`egui::Style`] for one appearance.
pub fn style(appearance: &Appearance) -> egui::Style {
    let mut style = egui::Style {
        text_styles: text_styles(),
        visuals: visuals(appearance),
        // Near-instant state changes: the language is mechanical, not
        // animated. Kept just above zero so egui's fades still resolve.
        animation_time: 0.06,
        ..egui::Style::default()
    };
    spacing(&mut style.spacing, appearance.density);
    style
}

/// The type ramp, on the fonts egui already bundles.
///
/// Not scaled by [`UiScale`]: the scale axis multiplies `pixels_per_point`,
/// which enlarges the type, the spacing, the bevels and the hit targets
/// together and keeps every hairline one physical pixel. Growing the font
/// sizes here instead would leave the chrome around them the same size.
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

/// Spacing for one density that never shrinks a hit target below 24 pt.
fn spacing(spacing: &mut egui::style::Spacing, density: Density) {
    let metrics = density.metrics();
    spacing.item_spacing = metrics.item_spacing;
    spacing.window_margin = Margin::same(metrics.window_margin);
    spacing.menu_margin = Margin::same(metrics.menu_margin);
    spacing.button_padding = metrics.button_padding;
    // The floor for every interactive widget's height, and the touch rule.
    // The height half is `MIN_TOUCH_POINTS` in every density: `Dense` buys
    // its density from the gaps, never from the targets.
    spacing.interact_size = metrics.interact_size();
    spacing.slider_width = metrics.slider_width;
    spacing.slider_rail_height = 6.0;
    spacing.text_edit_width = metrics.text_edit_width;
    spacing.icon_width = metrics.icon_width;
    spacing.icon_width_inner = metrics.icon_width_inner;
    spacing.icon_spacing = metrics.icon_spacing;
    spacing.tooltip_width = 420.0;
    spacing.combo_height = 260.0;
    // A solid, allocated scroll channel — visible and grabbable, like the
    // instrument it belongs to — sized for fingers, in every density.
    spacing.scroll = ScrollStyle {
        bar_width: 12.0,
        handle_min_length: 24.0,
        ..ScrollStyle::solid()
    };
}

/// The colours and shapes for one appearance.
fn visuals(appearance: &Appearance) -> Visuals {
    let palette = appearance.palette();
    let ground = appearance.theme.ground;
    let base = match ground {
        // Start from egui's own mode defaults so mode-conditional details
        // this function does not name (text-alpha handling, cursor
        // previews) stay correct for the ground.
        Ground::Dark => Visuals::dark(),
        Ground::Light => Visuals::light(),
    };
    let shadow_alpha = match ground {
        Ground::Dark => 130,
        Ground::Light => 60,
    };
    Visuals {
        widgets: widgets(&palette),
        selection: Selection {
            bg_fill: palette.selection_bg,
            stroke: Stroke::new(1.0, palette.selection_text),
        },
        hyperlink_color: palette.link,
        // Secondary text is a declared colour, not a faded primary. egui's
        // default is `text_color().gamma_multiply(weak_text_alpha)`, which
        // fades the ink toward transparency without knowing what is behind
        // it: on the dark theme's well that lands at `#5C5D5D` on
        // `#181A1D`, a measured 2.64:1, and every text-edit hint on the bar
        // was illegible because of it (caught by the toolbar audit in
        // `examples/theme_gallery.rs`, not by eye). `text_weak` is the
        // palette's own answer, pinned at ≥ 4.5:1 on both face and well.
        weak_text_color: Some(palette.text_weak),
        faint_bg_color: match ground {
            Ground::Dark => Color32::from_white_alpha(4),
            Ground::Light => Color32::from_black_alpha(7),
        },
        extreme_bg_color: palette.well,
        text_edit_bg_color: Some(palette.well),
        code_bg_color: match ground {
            Ground::Dark => Color32::from_rgb(30, 32, 36),
            Ground::Light => Color32::from_rgb(233, 231, 225),
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
