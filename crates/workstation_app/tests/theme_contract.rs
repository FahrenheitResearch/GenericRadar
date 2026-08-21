//! Behaviour pins for the workstation theme (`src/theme.rs`).
//!
//! The theme sources are included directly (`#[path]`) so this contract tests
//! the visual-language module in isolation from the rest of the application.
//! Keep the shim unless the same contract moves into the crate itself.
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
use theme::{Appearance, ChromeEdges, Ground, bevel};

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
        let floors: [(&str, Color32, Color32, f64); 10] = [
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
            // …and on the well, because `bevel::sunken_readout` invites
            // exactly these two as overrides for a readout that has to
            // shout — the live-stall notice on the toolbar is one.
            ("warn on well", p.warn, p.well, 4.5),
            ("error on well", p.error, p.well, 4.5),
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
    for variant in [Ground::Light, Ground::Dark] {
        let style = theme::style(&Appearance::on_ground(variant));
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
    for (variant, p) in [(Ground::Light, &LIGHT), (Ground::Dark, &DARK)] {
        let v = theme::style(&Appearance::on_ground(variant)).visuals;
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
        assert_eq!(v.dark_mode, matches!(variant, Ground::Dark));
    }
}

#[test]
fn hit_targets_hold_the_mobile_floor() {
    const {
        assert!(bevel::MIN_TOUCH_POINTS >= 24.0);
    }
    for variant in [Ground::Light, Ground::Dark] {
        let style = theme::style(&Appearance::on_ground(variant));
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
    let styles = theme::style(&Appearance::on_ground(Ground::Dark)).text_styles;
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
    theme::apply(&ctx, &Appearance::on_ground(Ground::Dark));
    assert_eq!(ctx.theme(), Theme::Dark);
    // Both slots carry the language, so an OS preference flip can never
    // land on stock egui.
    assert_eq!(ctx.style_of(Theme::Dark).visuals.panel_fill, DARK.face);
    assert_eq!(ctx.style_of(Theme::Light).visuals.panel_fill, LIGHT.face);

    let ctx = egui::Context::default();
    theme::apply(&ctx, &Appearance::on_ground(Ground::Light));
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
    for variant in [Ground::Light, Ground::Dark] {
        let ctx = egui::Context::default();
        theme::apply(&ctx, &Appearance::on_ground(variant));
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
        theme::apply(&ctx, &Appearance::on_ground(Ground::Dark));
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
                    &theme::palette::Palette::of(Ground::Dark),
                    ChromeEdges::Bevelled,
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
    theme::apply(&ctx, &Appearance::on_ground(Ground::Dark));
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

/// The root `Ui` eframe hands an app "has no margin or background color", and
/// the window under it is cleared to eframe's own near-black default. The two
/// halves of the ground must therefore both exist and must agree: a painted
/// face with a black clear colour tears a seam on every resize, and a matched
/// clear colour with an unpainted root leaves light chrome and dark ink
/// floating on raw black — which is what the field failure looked like.
#[test]
fn the_root_ground_is_painted_and_the_clear_colour_matches_it() {
    for (variant, palette) in [(Ground::Light, &LIGHT), (Ground::Dark, &DARK)] {
        let ctx = egui::Context::default();
        theme::apply(&ctx, &Appearance::on_ground(variant));
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| theme::paint_root_ground(ui),
        );
        // The FIRST shape, because anything painted before it would be
        // covered by it.
        let first = output.shapes.first().expect("the ground painted nothing");
        let egui::Shape::Rect(rect_shape) = &first.shape else {
            panic!("{variant:?}: the ground is not a filled rect");
        };
        assert_eq!(rect_shape.fill, palette.face);
        assert!(
            rect_shape.rect.contains_rect(screen),
            "{variant:?}: the ground covers {:?}, not the whole viewport",
            rect_shape.rect
        );

        let clear = theme::clear_color(&theme::style(&Appearance::on_ground(variant)).visuals);
        assert_eq!(
            clear,
            palette.face.to_opaque().to_normalized_gamma_f32(),
            "{variant:?}: the window clear colour is not the painted ground"
        );
        assert_eq!(
            clear[3], 1.0,
            "{variant:?}: the clear colour is see-through"
        );
    }
}

/// A menu title latches while its menu is down, and it can only do that if
/// the id `toolbar_control` allocates is the id `Popup::menu` derives its
/// popup id from. That derivation lives in egui, so it is pinned here rather
/// than assumed: a version bump that changes either end turns the latch off
/// silently, and a menu bar whose open title looks identical to its closed
/// ones is exactly the "which menu is this?" confusion the bevel is for.
#[test]
fn a_toolbar_menu_latches_while_its_menu_is_down() {
    let ctx = egui::Context::default();
    theme::apply(&ctx, &Appearance::on_ground(Ground::Dark));
    let input = || egui::RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0))),
        ..Default::default()
    };

    // The id a bare `toolbar_button` takes is the one `next_auto_id` promised.
    let mut promised = egui::Id::NULL;
    let mut taken = egui::Id::NULL;
    let _ = ctx.run_ui(input(), |ui| {
        ui.horizontal(|ui| {
            promised = ui.next_auto_id();
            taken = bevel::toolbar_button(ui, "File").id;
        });
    });
    assert_eq!(
        promised, taken,
        "toolbar_control no longer allocates the id next_auto_id promised"
    );

    // Opening the popup at `<that id>.with(\"popup\")` — the id `toolbar_menu`
    // probes — really does show the menu.
    let popup_id = promised.with("popup");
    egui::Popup::open_id(&ctx, popup_id);
    let mut menu_ran = false;
    let _ = ctx.run_ui(input(), |ui| {
        ui.horizontal(|ui| {
            bevel::toolbar_menu(ui, "File", |ui| {
                menu_ran = true;
                ui.label("Open a Level II archive");
            });
        });
    });
    assert!(
        menu_ran,
        "the id toolbar_menu probes is not the id Popup::menu uses"
    );
    assert!(egui::Popup::is_id_open(&ctx, popup_id));
}

/// Readouts are the controls that used to be bare labels on whatever ground
/// they landed on. They must be a well, they must be legible in both
/// variants, and they must be the same height as the buttons beside them —
/// a row of controls that disagree about their height is the thing that
/// reads as amateur.
#[test]
fn a_sunken_readout_is_a_legible_well_the_height_of_a_toolbar_button() {
    for (variant, palette) in [(Ground::Light, &LIGHT), (Ground::Dark, &DARK)] {
        let ctx = egui::Context::default();
        theme::apply(&ctx, &Appearance::on_ground(variant));
        let mut button = Rect::NOTHING;
        let mut short = Rect::NOTHING;
        let mut long = Rect::NOTHING;
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 200.0))),
                ..Default::default()
            },
            |ui| {
                ui.horizontal(|ui| {
                    button = bevel::toolbar_button(ui, "− Tilt").rect;
                    short = bevel::sunken_readout(ui, 74.0, 150.0, "0.48°").rect;
                    long = bevel::sunken_readout(
                        ui,
                        0.0,
                        120.0,
                        "KOAX · live · chunk 14/17 · 42 s ago · VCP 212",
                    )
                    .rect;
                });
            },
        );
        assert_eq!(
            short.height(),
            button.height(),
            "{variant:?}: a readout and a button next to it are different heights"
        );
        assert!(short.height() >= bevel::MIN_TOUCH_POINTS);
        assert!(
            short.width() >= 74.0,
            "{variant:?}: the floor width is what stops the bar re-flowing as the data changes"
        );
        assert!(
            long.width() <= 120.0,
            "{variant:?}: a long readout ran past its ceiling instead of truncating"
        );
        assert!(
            long.height() <= short.height(),
            "{variant:?}: a long readout wrapped, which changes the height of the whole bar"
        );
        // The pair the readout always draws, whatever the data says.
        assert!(
            contrast(palette.text, palette.well) >= 4.5,
            "{variant:?}: readout ink is not legible on the well it sits in"
        );
    }
}

/// Secondary text is a declared colour, not a faded primary. egui's fallback
/// (`text_color().gamma_multiply(weak_text_alpha)`) fades toward transparency
/// without knowing what is behind it, and on the dark variant's well it
/// landed at 2.64:1 — every text-edit hint on the toolbar was illegible.
#[test]
fn weak_text_is_the_palette_role_and_clears_the_floor_on_both_grounds() {
    for (variant, palette) in [(Ground::Light, &LIGHT), (Ground::Dark, &DARK)] {
        let visuals = theme::style(&Appearance::on_ground(variant)).visuals;
        assert_eq!(
            visuals.weak_text_color(),
            palette.text_weak,
            "{variant:?}: weak text fell back to egui's alpha fade"
        );
        for (what, ground) in [("face", palette.face), ("well", palette.well)] {
            let ratio = contrast(palette.text_weak, ground);
            assert!(
                ratio >= 4.5,
                "{variant:?}: weak text on {what} is {ratio:.2}, below 4.5:1"
            );
        }
    }
}
