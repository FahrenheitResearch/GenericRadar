//! Behaviour pins for the workstation theme (`src/theme.rs`).
//!
//! The theme sources are included directly (`#[path]`) so this contract runs
//! before `mod theme;` is wired into `main.rs` — the module has no other
//! compilation path until a human applies that one-line integration. Once it
//! is wired, this file keeps running unchanged; delete the `#[path]` shim
//! only if the tests move into the crate itself.
//!
//! What is pinned here and why:
//! * the WCAG 2.2 contrast floors (W3C Recommendation, 2023: SC 1.4.3 text
//!   contrast, SC 1.4.11 non-text contrast) for every foreground role, so a
//!   palette tweak that goes illegible fails loudly;
//! * the square-corner, visible-border, raised/pressed grammar, so a stray
//!   `..Default::default()` cannot quietly round the app off;
//! * the ≥ 24-point hit-target floor (WCAG 2.2 SC 2.5.8), because mobile is
//!   a standing requirement;
//! * the bevel arithmetic, so the one-physical-pixel promise holds at any
//!   DPI without anyone rendering a frame to check.
//!
//! No human has to have seen a picture for these to hold — but a picture
//! exists: `examples/theme_gallery.rs` renders both variants offscreen.

#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;

use eframe::egui::emath::GuiRounding as _;
use eframe::egui::{self, Color32, CornerRadius, Rect, Theme, pos2, vec2};
use theme::palette::{DARK, LIGHT, Palette};
use theme::{Variant, bevel};

/// WCAG 2.2 relative luminance of an sRGB colour (the SC 1.4.3 definition,
/// which follows IEC 61966-2-1 for the transfer function).
fn relative_luminance(color: Color32) -> f64 {
    fn channel(byte: u8) -> f64 {
        let u = f64::from(byte) / 255.0;
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// WCAG 2.2 contrast ratio, 1.0 ..= 21.0.
fn contrast(a: Color32, b: Color32) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn both_palettes() -> [(&'static str, &'static Palette); 2] {
    [("light", &LIGHT), ("dark", &DARK)]
}

#[test]
fn every_text_role_clears_its_wcag_floor_in_both_variants() {
    for (name, p) in both_palettes() {
        let floors: [(&str, Color32, Color32, f64); 8] = [
            ("text on face", p.text, p.face, 7.0),
            ("text on well", p.text, p.well, 7.0),
            ("text on raised face", p.text, p.face_raised, 7.0),
            ("weak text on face", p.text_weak, p.face, 4.5),
            ("link on face", p.link, p.face, 4.5),
            (
                "selection text on selection",
                p.selection_text,
                p.selection_bg,
                4.5,
            ),
            ("warn on face", p.warn, p.face, 4.5),
            ("error on face", p.error, p.face, 4.5),
        ];
        for (what, fg, bg, floor) in floors {
            let ratio = contrast(fg, bg);
            assert!(
                ratio >= floor,
                "{name}: {what} is {ratio:.2}, below the {floor}:1 floor"
            );
        }
        // The focus ring egui draws in `selection.stroke` must also be
        // visible against plain chrome (SC 1.4.11 non-text contrast).
        assert!(
            contrast(p.selection_text, p.face) >= 3.0,
            "{name}: focus ring would vanish on the panel face"
        );
    }
}

#[test]
fn the_bevel_language_reads_in_both_variants() {
    for (name, p) in both_palettes() {
        // A raised edge needs a lit side above the face and a shade side
        // below it; 1.3:1 per line is the point where the line survives on
        // a desk in daylight, and the outer shade edge carries more.
        assert!(
            contrast(p.hi_outer, p.face) >= 1.3,
            "{name}: lit edge too weak"
        );
        assert!(
            contrast(p.face, p.sh_inner) >= 1.3,
            "{name}: inner shade too weak"
        );
        assert!(
            contrast(p.face, p.sh_outer) >= 1.5,
            "{name}: outer shade too weak"
        );
        // The five-step luminance ladder the whole grammar sits on.
        let ladder = [p.hi_outer, p.hi_inner, p.face, p.sh_inner, p.sh_outer];
        for pair in ladder.windows(2) {
            assert!(
                relative_luminance(pair[0]) > relative_luminance(pair[1]),
                "{name}: bevel ladder out of order"
            );
        }
    }
}

#[test]
fn the_light_face_is_not_the_dingy_classic_and_the_dark_face_is_not_black() {
    // #C0C0C0 is the face this theme deliberately climbs out of.
    for channel in [LIGHT.face.r(), LIGHT.face.g(), LIGHT.face.b()] {
        assert!(channel > 0xC0, "light face has sunk back to Win95 grey");
    }
    // Night chrome is graphite: dark enough for a dark room, never so black
    // that the bevel ladder loses its bottom rungs.
    for channel in [DARK.face.r(), DARK.face.g(), DARK.face.b()] {
        assert!(
            (40..=90).contains(&channel),
            "dark face left the graphite band"
        );
    }
    // Wells sit on opposite sides of the face in the two variants: paper in
    // daylight, deeper-than-chrome at night so imagery pops.
    assert!(relative_luminance(LIGHT.well) > relative_luminance(LIGHT.face));
    assert!(relative_luminance(DARK.well) < relative_luminance(DARK.face));
}

#[test]
fn every_widget_state_is_square_with_a_visible_border() {
    for variant in [Variant::Light, Variant::Dark] {
        let style = theme::style(variant);
        let v = &style.visuals;
        for w in [
            &v.widgets.noninteractive,
            &v.widgets.inactive,
            &v.widgets.hovered,
            &v.widgets.active,
            &v.widgets.open,
        ] {
            assert_eq!(w.corner_radius, CornerRadius::ZERO);
            assert!(w.bg_stroke.width >= 1.0, "border must be visible at rest");
            assert_eq!(w.expansion, 0.0, "widgets stay inside their own bounds");
        }
        assert_eq!(v.window_corner_radius, CornerRadius::ZERO);
        assert_eq!(v.menu_corner_radius, CornerRadius::ZERO);
        // Crisp seat, not a soft material halo.
        assert!(v.window_shadow.blur <= 4);
        assert!(v.popup_shadow.blur <= 3);
    }
}

#[test]
fn the_raised_pressed_grammar_is_wired_into_stock_widgets() {
    for (variant, p) in [(Variant::Light, &LIGHT), (Variant::Dark, &DARK)] {
        let v = theme::style(variant).visuals;
        assert_eq!(v.panel_fill, p.face);
        assert_eq!(v.window_fill, p.face);
        // A button at rest stands one step proud of the panel; pressed, one
        // step below it. That ordering IS the affordance.
        let face = relative_luminance(p.face);
        assert!(relative_luminance(v.widgets.inactive.weak_bg_fill) > face);
        assert!(relative_luminance(v.widgets.hovered.weak_bg_fill) > face);
        let pressed = relative_luminance(v.widgets.active.weak_bg_fill);
        assert!(pressed < face);
        assert_eq!(v.text_edit_bg_color, Some(p.well));
        assert_eq!(v.extreme_bg_color, p.well);
        assert_eq!(v.selection.bg_fill, p.selection_bg);
        assert_eq!(v.selection.stroke.color, p.selection_text);
        assert_eq!(v.hyperlink_color, p.link);
        assert_eq!(v.dark_mode, matches!(variant, Variant::Dark));
    }
}

#[test]
fn hit_targets_hold_the_mobile_floor() {
    const {
        assert!(bevel::MIN_TOUCH_POINTS >= 24.0);
    }
    for variant in [Variant::Light, Variant::Dark] {
        let style = theme::style(variant);
        assert!(
            style.spacing.interact_size.y >= bevel::MIN_TOUCH_POINTS,
            "stock widgets fell below the touch floor"
        );
        assert!(
            style.spacing.scroll.bar_width >= 12.0 && !style.spacing.scroll.floating,
            "scroll bars must be solid and grabbable"
        );
    }
}

#[test]
fn the_type_ramp_is_the_bundled_fonts_at_professional_density() {
    let styles = theme::style(Variant::Dark).text_styles;
    let size = |ts: &egui::TextStyle| styles[ts].size;
    assert_eq!(size(&egui::TextStyle::Small), 10.0);
    assert_eq!(size(&egui::TextStyle::Body), 12.5);
    assert_eq!(size(&egui::TextStyle::Button), 12.5);
    assert_eq!(size(&egui::TextStyle::Heading), 15.5);
    assert_eq!(size(&egui::TextStyle::Monospace), 12.0);
    assert_eq!(
        styles[&egui::TextStyle::Monospace].family,
        egui::FontFamily::Monospace
    );
}

#[test]
fn apply_styles_both_theme_slots_and_pins_the_preference() {
    let ctx = egui::Context::default();
    theme::apply(&ctx, Variant::Dark);
    assert_eq!(ctx.theme(), Theme::Dark);
    // Both slots carry the language, so an OS preference flip can never
    // land on stock egui.
    assert_eq!(ctx.style_of(Theme::Dark).visuals.panel_fill, DARK.face);
    assert_eq!(ctx.style_of(Theme::Light).visuals.panel_fill, LIGHT.face);

    let ctx = egui::Context::default();
    theme::apply(&ctx, Variant::Light);
    assert_eq!(ctx.theme(), Theme::Light);
    assert_eq!(ctx.style_of(Theme::Light).visuals.panel_fill, LIGHT.face);
}

#[test]
fn ring_rects_are_one_pixel_strips_that_tile_the_ring() {
    for ppp in [1.0_f32, 1.25, 1.5, 2.0, 3.0] {
        let px = 1.0 / ppp;
        let rect = Rect::from_min_max(pos2(3.3, 7.9), pos2(103.7, 41.2)).round_to_pixels(ppp);
        let [top, left, bottom, right] = bevel::ring_rects(rect, px);
        // Each strip is exactly one physical pixel across.
        assert!((top.height() - px).abs() < 1e-4);
        assert!((bottom.height() - px).abs() < 1e-4);
        assert!((left.width() - px).abs() < 1e-4);
        assert!((right.width() - px).abs() < 1e-4);
        // The strips lie on the ring's own edges.
        assert_eq!(top.min, rect.min);
        assert_eq!(right.max, rect.max);
        assert_eq!(bottom.max, rect.max);
        assert_eq!(left.min, rect.min);
        // And the snapped rect really sits on the physical pixel grid, which
        // is what makes the strips whole pixels rather than grey smears.
        for value in [rect.min.x, rect.min.y, rect.max.x, rect.max.y] {
            let device = value * ppp;
            assert!(
                (device - device.round()).abs() < 1e-3,
                "edge at {value} points is off the pixel grid at {ppp}x"
            );
        }
    }
}

/// Drives one real (headless, CPU-only) egui pass through every composition
/// helper, so their layout arithmetic runs against the actual font metrics.
#[test]
fn composition_helpers_lay_out_touch_sized_controls_in_a_real_pass() {
    for variant in [Variant::Light, Variant::Dark] {
        let ctx = egui::Context::default();
        theme::apply(&ctx, variant);
        let mut button_rect = Rect::NOTHING;
        let mut toggle_rect = Rect::NOTHING;
        let mut well_rect = Rect::NOTHING;
        let mut group_rect = Rect::NOTHING;
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0))),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            bevel::raised_frame(ui, |ui| {
                ui.horizontal(|ui| {
                    button_rect = bevel::toolbar_button(ui, "Load").rect;
                    bevel::etched_separator(ui);
                    toggle_rect = bevel::toolbar_toggle(ui, true, "3D").rect;
                });
            });
            well_rect = bevel::sunken_well(ui, |ui| {
                ui.monospace("KTLX · REF (dBZ) · 0.5°");
            })
            .response
            .rect;
            group_rect = bevel::group_box(ui, "Playback", |ui| {
                ui.label("x");
            })
            .response
            .rect;
        });
        for (what, rect) in [
            ("toolbar button", button_rect),
            ("toolbar toggle", toggle_rect),
        ] {
            assert!(
                rect.width() >= bevel::MIN_TOUCH_POINTS && rect.height() >= bevel::MIN_TOUCH_POINTS,
                "{variant:?}: {what} is {}x{}, below the touch floor",
                rect.width(),
                rect.height()
            );
        }
        assert!(
            well_rect.height() > 0.0,
            "{variant:?}: well laid out nothing"
        );
        // The group box must be at least as wide as its caption plus the
        // margins that keep the caption clear of the groove.
        assert!(
            group_rect.width() > 60.0,
            "{variant:?}: group box narrower than its own caption"
        );
    }
}

/// `paint_bevel` itself must do the device-grid snapping — not just the
/// callers — or an off-grid rect at a fractional scale factor smears every
/// hairline. Painted through a real pass and read back from the shape list,
/// so reverting the `round_to_pixels` inside `paint_bevel` fails here.
#[test]
fn paint_bevel_snaps_off_grid_rects_to_device_pixels() {
    use eframe::egui::Shape;
    for ppp in [1.25_f32, 1.5, 2.0] {
        let ctx = egui::Context::default();
        theme::apply(&ctx, Variant::Dark);
        ctx.set_pixels_per_point(ppp);
        let mut shapes = Vec::new();
        // Two passes: the scale change lands on the second.
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))),
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                let off_grid = Rect::from_min_max(pos2(3.3, 7.9), pos2(103.7, 41.2));
                bevel::paint_bevel(
                    ui.painter(),
                    off_grid,
                    bevel::Bevel::Raised,
                    theme::palette::Palette::of(Variant::Dark),
                );
            });
            assert_eq!(output.pixels_per_point, ppp);
            shapes = output.shapes;
        }
        let px = 1.0 / ppp;
        let mut strips = 0;
        for clipped in &shapes {
            if let Shape::Rect(rect_shape) = &clipped.shape {
                let r = rect_shape.rect;
                for value in [r.min.x, r.min.y, r.max.x, r.max.y] {
                    let device = value * ppp;
                    assert!(
                        (device - device.round()).abs() < 1e-3,
                        "bevel edge at {value} points is off the device grid at {ppp}x"
                    );
                }
                let thickness = r.width().min(r.height());
                assert!(
                    (thickness - px).abs() < 1e-3,
                    "bevel strip is {thickness} points thick at {ppp}x, wanted {px}"
                );
                strips += 1;
            }
        }
        assert_eq!(strips, 8, "a Raised bevel is two rings of four strips");
    }
}

/// An auto-sized window must never be inflated by a separator that
/// helpfully fills "available" space — on ANY frame, not just the first
/// (sizing) pass. Stock `egui::Separator` fails this: it guards only the
/// sizing pass, and on the next frame it fills the screen-sized opening bid
/// `Resize::auto_sized` makes (`default_size = INFINITY`, clamped to the
/// screen, never shrunk), ratcheting the window out to the screen edge for
/// good. The etched separator bounds its fill by the containing `Ui`'s
/// previous-pass extent instead, so only real content can widen a window.
#[test]
fn an_etched_separator_does_not_inflate_an_auto_sized_window() {
    let ctx = egui::Context::default();
    theme::apply(&ctx, Variant::Dark);
    let mut window_rect = Rect::NOTHING;
    for _ in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0))),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let ctx = ui.ctx().clone();
            if let Some(response) =
                egui::Window::new("context-menu")
                    .auto_sized()
                    .show(&ctx, |ui| {
                        ui.label("Open volume");
                        bevel::etched_separator(ui);
                        ui.label("Close pane");
                        ui.horizontal(|ui| {
                            let _ = bevel::toolbar_button(ui, "A");
                            bevel::etched_separator(ui);
                            let _ = bevel::toolbar_button(ui, "B");
                        });
                    })
            {
                window_rect = response.response.rect;
            }
        });
    }
    assert!(
        window_rect.width() < 400.0,
        "a menu of short rows measured {} points wide: the separator inflated \
         the sizing pass",
        window_rect.width()
    );
    assert!(
        window_rect.height() < 300.0,
        "a menu of short rows measured {} points tall: the separator inflated \
         the sizing pass",
        window_rect.height()
    );
}
