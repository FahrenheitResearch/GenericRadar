//! The catalog contract: what every registered theme must be true of, and
//! what the four appearance axes must not break.
//!
//! `tests/theme_contract.rs` pins the LANGUAGE - the bevel arithmetic, the
//! square corners, the founding pair's numbers. This file pins the
//! FRAMEWORK, and it is written for a situation that is about to happen:
//! several themes arriving on several branches at once, each written by
//! somebody who cannot see the others. Nothing here names a theme except the
//! two founding entries; everything else iterates
//! `theme::catalog::THEMES` crossed with `theme::Accent::ALL`, so a theme
//! registered tomorrow is measured by the tests written today.
//!
//! # The floors, and why each one is where it is
//!
//! W3C, "Web Content Accessibility Guidelines (WCAG) 2.2", W3C
//! Recommendation, 2023.
//!
//! * **Body text, 7:1** on `face`, `face_raised` and `well` - SC 1.4.6
//!   (AAA). Higher than the 4.5:1 AA floor on purpose: these three are where
//!   an analyst reads numbers off a screen for hours, often on a bright
//!   bench, and AAA is what the founding pair already clears.
//! * **Every other live foreground, 4.5:1** - SC 1.4.3 (AA). Every text run
//!   in this application is 12.5 pt or smaller, so none of them qualify for
//!   the 3:1 large-text allowance.
//! * **Disabled text, 1.8:1 and strictly weaker than weak text.** SC 1.4.3
//!   explicitly exempts "inactive user interface components", and a disabled
//!   control that reads as crisply as a live one is a worse bug than a faint
//!   one. So the floor here is presence, not legibility - the text must
//!   still be visibly there - with a ceiling that stops a theme from making
//!   disabled look enabled.
//! * **Focus ring and flat borders, 3:1** - SC 1.4.11 (non-text contrast).
//!   In `ChromeEdges::Flat` the border IS the affordance, which is why
//!   `border_strong` is held to the graphics floor while a bevel hairline is
//!   only held to 1.3:1 by `theme_contract.rs`: a bevel is one line of a
//!   five-step ladder, and the ladder is what carries the meaning.
//!
//! Every failure names the theme, the accent, the pairing, the measured
//! ratio and the two colours, because the person who has to fix it is
//! usually not the person who ran the test.

#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;
// The settings window, included the same way, because two of the claims
// below are about what it PAINTS rather than about what the catalog says:
// the inks a described theme row is drawn in, measured against the grounds
// the same module says that row is drawn on, and the width a description
// wraps at. A copy of either rule in this file would pass while the shipped
// window did something else, which is exactly how the hovered row went
// unmeasured.
#[allow(dead_code)]
#[path = "../src/settings_ui.rs"]
mod settings_ui;

use eframe::egui::{self, Color32, Rect, pos2, vec2};
use theme::palette::{DARK, LIGHT, Palette};
use theme::{Accent, Appearance, ChromeEdges, Density, Ground, ThemeSpec, UiScale, bevel, catalog};

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

fn hex(color: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

// ---------------------------------------------------------------------------
// The registry itself
// ---------------------------------------------------------------------------

#[test]
fn the_catalog_is_a_mergeable_alphabetical_list_of_unique_ids() {
    assert!(
        !catalog::THEMES.is_empty(),
        "a catalog with no themes cannot style anything"
    );
    let ids = catalog::THEMES
        .iter()
        .map(|theme| theme.id)
        .collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(
        ids, sorted,
        "the registration list in src/theme/catalog.rs is out of alphabetical order.\n\
         It is sorted so that themes arriving on parallel branches merge without \
         anybody having to read the file: {ids:?}"
    );
    let mut unique = sorted.clone();
    unique.dedup();
    assert_eq!(
        sorted, unique,
        "two themes share an id. Ids are the persistence contract - one of these \
         would silently read the other's stored value"
    );
}

#[test]
fn every_registered_theme_says_what_it_is() {
    for theme in catalog::THEMES {
        let id = theme.id;
        assert!(!id.is_empty(), "a theme with no id cannot be persisted");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{id:?}: theme ids are lowercase ASCII with hyphens, so they survive a \
             settings file and a file name unchanged"
        );
        assert!(
            !theme.label.is_empty(),
            "{id}: no label for the settings list"
        );
        assert!(
            theme.description.len() >= 20,
            "{id}: the description is what an analyst reads to choose - one clause \
             about what this look is FOR, not a restatement of the label"
        );
    }
}

#[test]
fn the_named_defaults_are_registered_and_on_the_ground_they_claim() {
    for (what, theme, ground) in [
        ("DEFAULT_LIGHT", catalog::DEFAULT_LIGHT, Ground::Light),
        ("DEFAULT_DARK", catalog::DEFAULT_DARK, Ground::Dark),
    ] {
        assert!(
            catalog::THEMES.iter().any(|listed| listed.id == theme.id),
            "{what} is not in the catalog"
        );
        assert_eq!(theme.ground, ground, "{what} is on the wrong ground");
    }
    assert_eq!(
        catalog::DEFAULT.id,
        catalog::DEFAULT_LIGHT.id,
        "the daylight bench is the application's identity and its shipped default"
    );
    assert_eq!(
        Appearance::default().theme.id,
        catalog::DEFAULT.id,
        "a fresh install must open on the catalog default"
    );
}

/// A theme that declared the wrong ground gets egui's own mode-conditional
/// details (text-alpha handling, cursor previews) backwards, and nothing
/// about the colours would show it.
#[test]
fn every_theme_declares_the_ground_its_own_colours_are_on() {
    for theme in catalog::THEMES {
        let id = theme.id;
        let palette = theme.palette;
        let ink = relative_luminance(palette.text);
        let face = relative_luminance(palette.face);
        match theme.ground {
            Ground::Light => assert!(
                ink < face,
                "{id}: declared a light ground but its text ({}) is brighter than its \
                 face ({})",
                hex(palette.text),
                hex(palette.face)
            ),
            Ground::Dark => assert!(
                ink > face,
                "{id}: declared a dark ground but its text ({}) is darker than its \
                 face ({})",
                hex(palette.text),
                hex(palette.face)
            ),
        }
        let visuals = theme::style(&Appearance::by_id(id)).visuals;
        assert_eq!(
            visuals.dark_mode,
            theme.ground.is_dark(),
            "{id}: egui's dark_mode disagrees with the declared ground"
        );
    }
}

// ---------------------------------------------------------------------------
// The contrast audit
// ---------------------------------------------------------------------------

/// One measured pairing.
struct Pairing {
    what: &'static str,
    fg: Color32,
    bg: Color32,
    floor: f64,
}

fn pairing(what: &'static str, fg: Color32, bg: Color32, floor: f64) -> Pairing {
    Pairing {
        what,
        fg,
        bg,
        floor,
    }
}

/// Every foreground-on-ground pair the chrome actually paints, for one
/// resolved palette.
///
/// Deliberately NOT every combination of two roles: a link is never drawn on
/// a hovered button face, and asserting pairs the chrome cannot produce
/// would reject good themes for imaginary reasons. Each line below
/// corresponds to something `theme::visuals` or `theme::bevel` really does.
fn pairings(p: &Palette) -> Vec<Pairing> {
    vec![
        // Body text, on the three grounds an analyst reads it off.
        pairing("body text on face", p.text, p.face, 7.0),
        pairing("body text on raised face", p.text, p.face_raised, 7.0),
        pairing("body text on well", p.text, p.well, 7.0),
        // ...and on the two transient face steps, at the AA floor.
        pairing("body text on hovered face", p.text, p.hover, 4.5),
        pairing("body text on pressed face", p.text, p.face_pressed, 4.5),
        // Secondary text: hints, captions, status lines.
        pairing("weak text on face", p.text_weak, p.face, 4.5),
        pairing("weak text on well", p.text_weak, p.well, 4.5),
        // The accent, everywhere the chrome puts it.
        pairing("link on face", p.link, p.face, 4.5),
        pairing("link on well", p.link, p.well, 4.5),
        pairing(
            "selected text on selection",
            p.selection_text,
            p.selection_bg,
            4.5,
        ),
        pairing("body text on selection", p.text, p.selection_bg, 4.5),
        pairing(
            "body text on a latched toggle",
            p.text,
            p.selection_tint,
            4.5,
        ),
        // The focus ring is drawn in `selection_text` on plain chrome
        // (SC 1.4.11 non-text contrast).
        pairing("focus ring on face", p.selection_text, p.face, 3.0),
        // The loud inks, on both grounds `bevel::sunken_readout` invites
        // them onto.
        pairing("warning text on face", p.warn, p.face, 4.5),
        pairing("warning text on well", p.warn, p.well, 4.5),
        pairing("error text on face", p.error, p.face, 4.5),
        pairing("error text on well", p.error, p.well, 4.5),
        // In `ChromeEdges::Flat` this border is the whole affordance.
        pairing("flat border on face", p.border_strong, p.face, 3.0),
        pairing("flat border on well", p.border_strong, p.well, 3.0),
    ]
}

/// THE audit. Every registered theme, crossed with every accent, measured
/// against every pairing the chrome paints.
///
/// This is the test that lets six theme authors work in parallel without a
/// reviewer measuring anything by hand. A failure names the theme, the
/// accent, the pairing, the ratio and the two colours.
#[test]
fn every_theme_and_accent_clears_its_contrast_floor() {
    let mut failures = Vec::new();
    let mut measured = 0usize;
    for theme in catalog::THEMES {
        for accent in Accent::ALL {
            let appearance = Appearance {
                accent,
                ..Appearance::by_id(theme.id)
            };
            let palette = appearance.palette();
            for Pairing {
                what,
                fg,
                bg,
                floor,
            } in pairings(&palette)
            {
                measured += 1;
                let ratio = contrast(fg, bg);
                if ratio < floor {
                    failures.push(format!(
                        "  theme {:<14} accent {:<8} {:<30} {ratio:>6.2}:1 < {floor}:1  \
                         ({} on {})",
                        theme.id,
                        accent.id(),
                        what,
                        hex(fg),
                        hex(bg)
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {measured} theme x accent pairings are below their contrast floor:\n{}\n\
         Fix the theme's own colours, or the accent's on that ground - see the floors \
         and their citations in this file's module docs.",
        failures.len(),
        failures.join("\n")
    );
    println!("{measured} theme x accent pairings measured, all above their floors");
}

/// Disabled text is the one foreground allowed below the AA floor (SC 1.4.3
/// exempts inactive components), so it gets its own rule: present, but
/// plainly weaker than live secondary text on the same ground.
#[test]
fn disabled_text_is_faint_on_purpose_and_never_mistakable_for_live_text() {
    const PRESENCE_FLOOR: f64 = 1.8;
    let mut failures = Vec::new();
    for theme in catalog::THEMES {
        let p = theme.palette;
        for (what, ground) in [
            ("face", p.face),
            ("raised face", p.face_raised),
            ("well", p.well),
        ] {
            let disabled = contrast(p.text_disabled, ground);
            if disabled < PRESENCE_FLOOR {
                failures.push(format!(
                    "  theme {:<14} disabled text on {what} is {disabled:.2}:1 < \
                     {PRESENCE_FLOOR}:1 ({} on {}) - it has faded out of existence",
                    theme.id,
                    hex(p.text_disabled),
                    hex(ground)
                ));
            }
            let weak = contrast(p.text_weak, ground);
            if disabled >= weak {
                failures.push(format!(
                    "  theme {:<14} disabled text on {what} is {disabled:.2}:1, at or above \
                     weak text's {weak:.2}:1 - a disabled control would read as a live one",
                    theme.id
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// The founding pair, pinned
// ---------------------------------------------------------------------------

/// Every token of the two founding themes, stated as numbers.
///
/// These two are what the whole application was drawn, photographed and
/// signed off against. Adding a look is adding a file; retuning one of these
/// is restyling the product, and this test is where that has to be a
/// deliberate act rather than a diff nobody noticed.
#[test]
fn the_two_founding_themes_are_frozen() {
    let expected_light: [(&str, Color32, Color32); 20] = [
        ("face", LIGHT.face, Color32::from_rgb(216, 213, 206)),
        (
            "face_raised",
            LIGHT.face_raised,
            Color32::from_rgb(228, 226, 220),
        ),
        (
            "face_pressed",
            LIGHT.face_pressed,
            Color32::from_rgb(198, 195, 188),
        ),
        ("hover", LIGHT.hover, Color32::from_rgb(236, 234, 228)),
        ("well", LIGHT.well, Color32::from_rgb(250, 249, 245)),
        ("text", LIGHT.text, Color32::from_rgb(28, 27, 25)),
        ("text_weak", LIGHT.text_weak, Color32::from_rgb(88, 86, 81)),
        (
            "text_disabled",
            LIGHT.text_disabled,
            Color32::from_rgb(139, 137, 132),
        ),
        ("border", LIGHT.border, Color32::from_rgb(146, 143, 137)),
        (
            "border_strong",
            LIGHT.border_strong,
            Color32::from_rgb(94, 92, 87),
        ),
        ("link", LIGHT.link, Color32::from_rgb(43, 84, 148)),
        (
            "selection_bg",
            LIGHT.selection_bg,
            Color32::from_rgb(167, 190, 219),
        ),
        (
            "selection_text",
            LIGHT.selection_text,
            Color32::from_rgb(16, 45, 85),
        ),
        (
            "selection_tint",
            LIGHT.selection_tint,
            Color32::from_rgb(181, 187, 194),
        ),
        ("warn", LIGHT.warn, Color32::from_rgb(128, 74, 0)),
        ("error", LIGHT.error, Color32::from_rgb(170, 36, 28)),
        ("hi_outer", LIGHT.hi_outer, Color32::from_rgb(255, 255, 255)),
        ("hi_inner", LIGHT.hi_inner, Color32::from_rgb(238, 236, 230)),
        ("sh_inner", LIGHT.sh_inner, Color32::from_rgb(150, 147, 141)),
        ("sh_outer", LIGHT.sh_outer, Color32::from_rgb(94, 92, 87)),
    ];
    let expected_dark: [(&str, Color32, Color32); 20] = [
        ("face", DARK.face, Color32::from_rgb(54, 57, 62)),
        (
            "face_raised",
            DARK.face_raised,
            Color32::from_rgb(63, 66, 72),
        ),
        (
            "face_pressed",
            DARK.face_pressed,
            Color32::from_rgb(42, 44, 48),
        ),
        ("hover", DARK.hover, Color32::from_rgb(70, 74, 80)),
        ("well", DARK.well, Color32::from_rgb(24, 26, 29)),
        ("text", DARK.text, Color32::from_rgb(230, 228, 225)),
        (
            "text_weak",
            DARK.text_weak,
            Color32::from_rgb(162, 165, 169),
        ),
        (
            "text_disabled",
            DARK.text_disabled,
            Color32::from_rgb(106, 109, 114),
        ),
        ("border", DARK.border, Color32::from_rgb(94, 99, 106)),
        (
            "border_strong",
            DARK.border_strong,
            Color32::from_rgb(130, 136, 144),
        ),
        ("link", DARK.link, Color32::from_rgb(125, 168, 222)),
        (
            "selection_bg",
            DARK.selection_bg,
            Color32::from_rgb(46, 90, 150),
        ),
        (
            "selection_text",
            DARK.selection_text,
            Color32::from_rgb(235, 240, 247),
        ),
        (
            "selection_tint",
            DARK.selection_tint,
            Color32::from_rgb(51, 69, 93),
        ),
        ("warn", DARK.warn, Color32::from_rgb(232, 163, 61)),
        ("error", DARK.error, Color32::from_rgb(244, 130, 120)),
        ("hi_outer", DARK.hi_outer, Color32::from_rgb(98, 103, 110)),
        ("hi_inner", DARK.hi_inner, Color32::from_rgb(72, 76, 82)),
        ("sh_inner", DARK.sh_inner, Color32::from_rgb(35, 37, 40)),
        ("sh_outer", DARK.sh_outer, Color32::from_rgb(15, 16, 18)),
    ];
    for (theme, roles) in [("light", expected_light), ("dark", expected_dark)] {
        for (role, found, expected) in roles {
            assert_eq!(
                found,
                expected,
                "theme {theme}: {role} is {} but the founding value is {}. \
                 Retuning a founding theme restyles the shipped product; a new look \
                 belongs in a new file in src/theme/",
                hex(found),
                hex(expected)
            );
        }
    }
    assert_eq!(catalog::light::THEME.palette, LIGHT);
    assert_eq!(catalog::dark::THEME.palette, DARK);
    assert_eq!(catalog::light::THEME.label, "Daylight bench (Win95)");
    assert_eq!(catalog::dark::THEME.label, "Night bench");
}

/// The two founding ids are the strings already in every settings file in
/// the field. Renaming one resets the choice of everybody who picked it.
#[test]
fn the_founding_ids_are_the_ones_already_on_disk() {
    assert_eq!(catalog::light::THEME.id, "light");
    assert_eq!(catalog::dark::THEME.id, "dark");
    assert_eq!(catalog::by_id("light").map(|theme| theme.id), Some("light"));
    assert_eq!(catalog::by_id("dark").map(|theme| theme.id), Some("dark"));
}

// ---------------------------------------------------------------------------
// The axes
// ---------------------------------------------------------------------------

/// The whole reason the shipped look survived being turned into data: every
/// axis at its default has to leave the theme exactly where it was.
#[test]
fn the_default_axes_reproduce_the_shipped_look_exactly() {
    let default = Appearance::default();
    assert_eq!(default.accent, Accent::Theme);
    assert_eq!(default.density, Density::Comfortable);
    assert_eq!(default.edges, ChromeEdges::Bevelled);
    assert_eq!(default.ui_scale, UiScale::Normal);
    assert_eq!(default.ui_scale.factor(), 1.0);
    for theme in catalog::THEMES {
        let appearance = Appearance::by_id(theme.id);
        assert_eq!(
            appearance.palette(),
            theme.palette,
            "{}: the default accent must leave the theme's own colours alone",
            theme.id
        );
    }
    // The spacing the application shipped with, to the point.
    let spacing = theme::style(&default).spacing;
    assert_eq!(spacing.item_spacing, vec2(6.0, 4.0));
    assert_eq!(spacing.button_padding, vec2(10.0, 4.0));
    assert_eq!(spacing.interact_size, vec2(44.0, 24.0));
    assert_eq!(spacing.slider_width, 140.0);
    assert_eq!(spacing.icon_width, 15.0);
}

/// A named accent moves the four accent roles and NOTHING else. An accent
/// that touched the face steps or the bevel ladder would be a second theme
/// wearing the first one's name.
#[test]
fn an_accent_moves_only_the_four_accent_roles() {
    for theme in catalog::THEMES {
        for accent in Accent::ALL {
            if accent == Accent::Theme {
                continue;
            }
            let base = theme.palette;
            let painted = Appearance {
                accent,
                ..Appearance::by_id(theme.id)
            }
            .palette();
            let expected = accent
                .tokens(theme.ground)
                .expect("every named accent declares tokens for both grounds");
            assert_eq!(painted.link, expected.link);
            assert_eq!(painted.selection_bg, expected.selection_bg);
            assert_eq!(painted.selection_text, expected.selection_text);
            assert_eq!(painted.selection_tint, expected.selection_tint);
            // Everything else, untouched.
            let untouched = Palette {
                link: base.link,
                selection_bg: base.selection_bg,
                selection_text: base.selection_text,
                selection_tint: base.selection_tint,
                ..painted
            };
            assert_eq!(
                untouched,
                base,
                "{}/{}: the accent changed a role it does not own",
                theme.id,
                accent.id()
            );
        }
    }
}

/// `MIN_TOUCH_POINTS` is a floor, not a scaled quantity. Dense may take the
/// gaps and the padding; it may not take the target.
#[test]
fn no_density_takes_a_clickable_control_under_the_touch_floor() {
    const {
        assert!(bevel::MIN_TOUCH_POINTS >= 24.0);
    }
    for density in Density::ALL {
        let id = density.id();
        let metrics = density.metrics();
        assert_eq!(
            metrics.interact_size().y,
            bevel::MIN_TOUCH_POINTS,
            "{id}: egui's interact height left the touch floor"
        );
        let style = theme::style(&Appearance {
            density,
            ..Appearance::default()
        });
        assert!(
            style.spacing.interact_size.y >= bevel::MIN_TOUCH_POINTS,
            "{id}: stock widgets fell below the touch floor"
        );

        // ...and through a real pass, because the helpers compute their own
        // sizes from the density's padding and a label's measured galley.
        let ctx = egui::Context::default();
        theme::apply(
            &ctx,
            &Appearance {
                density,
                ..Appearance::default()
            },
        );
        let mut sizes: Vec<(&str, Rect)> = Vec::new();
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 700.0))),
                ..Default::default()
            },
            |ui| {
                bevel::raised_frame(ui, |ui| {
                    ui.horizontal(|ui| {
                        sizes.push(("toolbar button", bevel::toolbar_button(ui, "Load").rect));
                        // A one-character label is the worst case: nothing
                        // but the floor is holding it up.
                        sizes.push(("narrow button", bevel::toolbar_button(ui, "3").rect));
                        sizes.push(("toolbar toggle", bevel::toolbar_toggle(ui, true, "3D").rect));
                        sizes.push((
                            "readout",
                            bevel::sunken_readout(ui, 0.0, 200.0, "0.48°").rect,
                        ));
                        sizes.push((
                            "menu title",
                            bevel::toolbar_menu(ui, "File", |ui| {
                                ui.label("Open a Level II archive");
                            })
                            .rect,
                        ));
                    });
                });
            },
        );
        assert_eq!(sizes.len(), 5, "{id}: the pass drew nothing to measure");
        for (what, rect) in sizes {
            assert!(
                rect.width() >= bevel::MIN_TOUCH_POINTS && rect.height() >= bevel::MIN_TOUCH_POINTS,
                "{id}: {what} laid out {:.1} x {:.1}, under the {} pt touch floor \
                 (WCAG 2.2 SC 2.5.8). Density may take the gaps; it may not take the \
                 target",
                rect.width(),
                rect.height(),
                bevel::MIN_TOUCH_POINTS
            );
        }
    }
}

/// Density really does tighten, in one direction, across every measurement
/// it owns — otherwise "Dense" is a setting that does nothing.
#[test]
fn density_tightens_monotonically_without_ever_reaching_zero() {
    let ladder = [
        Density::Comfortable.metrics(),
        Density::Compact.metrics(),
        Density::Dense.metrics(),
    ];
    for pair in ladder.windows(2) {
        let (loose, tight) = (pair[0], pair[1]);
        assert!(tight.item_spacing.x < loose.item_spacing.x);
        assert!(tight.item_spacing.y < loose.item_spacing.y);
        assert!(tight.control_padding.x < loose.control_padding.x);
        assert!(tight.frame_margin < loose.frame_margin);
        assert!(tight.group_margin_x < loose.group_margin_x);
        assert!(tight.separator_thickness < loose.separator_thickness);
        assert!(tight.interact_width < loose.interact_width);
    }
    let dense = Density::Dense.metrics();
    assert!(
        dense.item_spacing.y >= 2.0 && dense.control_padding.y >= 2.0,
        "the tightest density still has to leave a gap; zero spacing is a wall of text"
    );
}

/// Flat chrome is a change of paint, not of geometry. If switching it moved
/// a control by a pixel, an analyst who preferred flat edges would be using
/// a differently laid-out application.
#[test]
fn the_chrome_edge_axis_changes_paint_and_never_layout() {
    fn layout(edges: ChromeEdges) -> Vec<Rect> {
        let ctx = egui::Context::default();
        theme::apply(
            &ctx,
            &Appearance {
                edges,
                ..Appearance::default()
            },
        );
        let mut rects = Vec::new();
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0))),
                ..Default::default()
            },
            |ui| {
                let frame = bevel::raised_frame(ui, |ui| {
                    ui.horizontal(|ui| {
                        rects.push(bevel::toolbar_button(ui, "Load").rect);
                        bevel::etched_separator(ui);
                        rects.push(bevel::toolbar_toggle(ui, true, "3D").rect);
                        rects.push(bevel::sunken_readout(ui, 74.0, 150.0, "0.48°").rect);
                    });
                });
                rects.push(frame.response.rect);
                rects.push(
                    bevel::sunken_well(ui, |ui| ui.monospace("KDVN · REF"))
                        .response
                        .rect,
                );
                rects.push(
                    bevel::group_box(ui, "Playback", |ui| ui.label("x"))
                        .response
                        .rect,
                );
            },
        );
        rects
    }
    let bevelled = layout(ChromeEdges::Bevelled);
    let flat = layout(ChromeEdges::Flat);
    assert!(!bevelled.is_empty());
    assert_eq!(
        bevelled, flat,
        "flat chrome moved the layout. It must paint a different edge inside the \
         SAME rect - `paint_bevel` is the only thing the axis is allowed to change"
    );
}

/// ...and it really does paint something different, or the setting is a lie.
/// A `Raised` bevel is two rings of four one-pixel strips; a flat edge is
/// one ring of four.
#[test]
fn flat_chrome_paints_one_border_where_a_bevel_paints_two_rings() {
    fn strips(edges: ChromeEdges, bevel_kind: bevel::Bevel) -> Vec<Color32> {
        let ctx = egui::Context::default();
        let appearance = Appearance {
            edges,
            ..Appearance::default()
        };
        theme::apply(&ctx, &appearance);
        let palette = appearance.palette();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))),
                ..Default::default()
            },
            |ui| {
                bevel::paint_bevel(
                    ui.painter(),
                    Rect::from_min_max(pos2(10.0, 10.0), pos2(110.0, 40.0)),
                    bevel_kind,
                    &palette,
                    edges,
                );
            },
        );
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) => Some(rect.fill),
                _ => None,
            })
            .collect()
    }
    let bevelled = strips(ChromeEdges::Bevelled, bevel::Bevel::Raised);
    assert_eq!(bevelled.len(), 8, "a raised bevel is two rings of four");
    let flat = strips(ChromeEdges::Flat, bevel::Bevel::Raised);
    assert_eq!(flat.len(), 4, "a flat edge is one ring of four");
    for fill in &flat {
        assert_eq!(
            *fill, LIGHT.border_strong,
            "a flat edge is one border colour the whole way round; it is `border_strong` \
             because in this mode the line IS the affordance"
        );
    }
}

/// The scale axis lands where egui already has a home for it, so it
/// multiplies the platform's own scaling rather than replacing it.
#[test]
fn the_interface_scale_axis_drives_egui_zoom_factor() {
    for scale in UiScale::ALL {
        let ctx = egui::Context::default();
        theme::apply(
            &ctx,
            &Appearance {
                ui_scale: scale,
                ..Appearance::default()
            },
        );
        // egui applies a zoom change at the start of the next pass, so this
        // is measured where it is actually observable: on the frame that
        // comes out.
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                ..Default::default()
            },
            |ui| {
                ui.label("scale");
            },
        );
        assert_eq!(
            ctx.zoom_factor(),
            scale.factor(),
            "{}: the appearance did not reach egui's zoom factor",
            scale.id()
        );
        assert_eq!(
            output.pixels_per_point,
            scale.factor(),
            "{}: the frame did not come out at the scale the appearance asked for              (native scale is 1.0 in a headless context, so pixels_per_point IS the              zoom factor here)",
            scale.id()
        );
        assert!(
            (0.8..=1.6).contains(&scale.factor()),
            "{}: outside the offered range",
            scale.id()
        );
        assert_eq!(
            scale.id().parse::<f32>().ok(),
            Some(scale.factor()),
            "{}: the stored id must BE the number, so a settings file reads plainly",
            scale.id()
        );
    }
}

// ---------------------------------------------------------------------------
// The settings surface
// ---------------------------------------------------------------------------

/// The Appearance page's theme options are the catalog, derived rather than
/// listed — the single edit that adds a theme has to reach the settings
/// window without anybody remembering to update it.
#[test]
fn the_settings_page_offers_exactly_the_registered_themes() {
    let category = theme::settings::settings_category();
    assert_eq!(category.id, theme::settings::keys::CATEGORY);
    let spec = category
        .settings
        .iter()
        .find(|spec| spec.id == theme::settings::keys::THEME)
        .expect("the page declares a theme setting");
    let settings::SettingKind::Choice {
        options,
        default_id,
    } = &spec.kind
    else {
        panic!("the theme setting must be a choice over the catalog");
    };
    let offered = options
        .iter()
        .map(|option| {
            (
                option.id.as_str(),
                option.label.as_str(),
                option.description.as_str(),
            )
        })
        .collect::<Vec<_>>();
    let registered = catalog::THEMES
        .iter()
        .map(|theme| (theme.id, theme.label, theme.description))
        .collect::<Vec<_>>();
    assert_eq!(
        offered, registered,
        "the settings page and the catalog disagree about what themes exist"
    );
    assert_eq!(default_id, catalog::DEFAULT.id);
}

/// A described choice row is legible on EVERY ground the menu paints it on.
///
/// A described option is two lines in one selectable, and a selectable is
/// painted on more than one ground. Resting and unselected it has no frame
/// at all and sits on the menu's own face. Hovered it takes the hover face.
/// Held down it takes the pressed face. Selected - hovered or not - it takes
/// the selection fill. Four grounds, and until this test existed the audit
/// knew about two of them: an analyst reading down the list has the pointer
/// on a row that was never measured for the whole time they are choosing.
///
/// Nothing here decides what the row is drawn in. The inks come from
/// `settings_ui::described_option_inks` and the grounds from
/// `settings_ui::described_option_ground` - the same two functions the
/// shipped window calls - so this measures the window rather than a
/// description of it, and reverting the window's rule fails this test.
///
/// Crossed with every accent, because three of the four grounds move with
/// the accent.
#[test]
fn a_described_choice_is_legible_on_every_ground_the_menu_paints_it_on() {
    let mut failures = Vec::new();
    let mut measured = 0usize;
    for theme in catalog::THEMES {
        for accent in Accent::ALL {
            let appearance = Appearance {
                accent,
                ..Appearance::by_id(theme.id)
            };
            let style = theme::style(&appearance);
            for (state, selected) in settings_ui::DESCRIBED_OPTION_STATES {
                let ground = settings_ui::described_option_ground(&style, state, selected);
                let inks = settings_ui::described_option_inks(&style, state, selected);
                for (what, ink) in [("label", inks.label), ("description", inks.description)] {
                    measured += 1;
                    let ratio = contrast(ink, ground);
                    if ratio < 4.5 {
                        failures.push(format!(
                            "  theme {:<16} accent {:<8} {state:?}/{}selected {what:<11} \
                             {ratio:>6.2}:1 < 4.5:1  ({} on {})",
                            theme.id,
                            accent.id(),
                            if selected { "" } else { "un" },
                            hex(ink),
                            hex(ground)
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {measured} described-option inks are below the 4.5:1 floor:\n{}\n\
         Fix the theme's own colours, or the ink rule in \
         `settings_ui::described_option_inks` - but not by exempting a ground: \
         every one of them is a ground an analyst reads this list on.",
        failures.len(),
        failures.join("\n")
    );
    println!("{measured} described-option inks measured, all above the 4.5:1 floor");
}

/// And the reason the row does not simply keep weak ink everywhere.
///
/// Weak ink belongs to the menu's own face and is audited there. On the two
/// framed grounds it does not survive: across the registered themes
/// `text_weak` lands between 2.81:1 and 4.17:1 on their own selection fills
/// and between 3.29:1 and 10.89:1 on their own hover faces - under the
/// 4.5:1 floor on most of them either way, and on the founding pair's own
/// numbers, which are frozen. So the row rises to that state's own ink and
/// stays secondary by SIZE alone.
///
/// If a theme's weak ink ever does clear a floor, this test does not stop
/// it. It says so, and asks for the rule to be re-read rather than silently
/// keeping a switch that nothing needs.
#[test]
fn weak_ink_is_switched_out_because_the_framed_grounds_do_not_carry_it() {
    for theme in catalog::THEMES {
        let id = theme.id;
        let palette = theme.palette;
        let style = theme::style(&Appearance::by_id(theme.id));
        for (what, ground, state, selected) in [
            (
                "hover face",
                palette.hover,
                egui::widget_style::WidgetState::Hovered,
                false,
            ),
            (
                "selection fill",
                palette.selection_bg,
                egui::widget_style::WidgetState::Inactive,
                true,
            ),
        ] {
            let painted = settings_ui::described_option_inks(&style, state, selected).description;
            assert_eq!(
                settings_ui::described_option_ground(&style, state, selected),
                ground,
                "{id}: the {what} is not the ground the menu paints for this state"
            );
            let weak = contrast(palette.text_weak, ground);
            let now = contrast(painted, ground);
            assert!(
                weak < 4.5 || now >= weak,
                "{id}: weak ink now clears the floor on the {what} ({weak:.2}:1). That \
                 is fine, but the row still switches to {} ({now:.2}:1) - re-read \
                 `described_option_inks` before relaxing this.",
                hex(painted)
            );
        }
    }
}

/// Every theme's description reaches the list an analyst actually picks
/// from.
///
/// This is separate from the equality above on purpose. A description that
/// is written, measured and photographed but never rendered is worth
/// nothing, and that is exactly what happened here: `ThemeSpec` carried a
/// description from the first day, the settings page built its options from
/// `id` and `label` alone, and it went unnoticed while there were two themes
/// whose labels said enough. With eight of them the label is not enough -
/// "Paper" and "Broadcast desk" do not tell anybody which to pick - so the
/// wiring is pinned rather than left to be re-lost.
#[test]
fn the_theme_list_tells_an_analyst_what_each_theme_is_for() {
    let category = theme::settings::settings_category();
    let spec = category
        .settings
        .iter()
        .find(|spec| spec.id == theme::settings::keys::THEME)
        .expect("the page declares a theme setting");
    let settings::SettingKind::Choice { options, .. } = &spec.kind else {
        panic!("the theme setting must be a choice over the catalog");
    };
    for option in options {
        assert!(
            !option.description.is_empty(),
            "{}: the theme list shows no description, so the list reads as eight \
             bare names",
            option.id
        );
    }
    assert!(
        !spec.help.contains("Daylight bench") && !spec.help.contains("Night bench"),
        "the theme setting's help text names individual themes. It goes stale the \
         next time one is registered - what a theme is for belongs to that theme's \
         own description, which the list now shows."
    );
}

/// Every text run one pass of the window emitted: its text, the rect it
/// really occupies, and how many rows it was laid out on.
fn text_runs(shapes: &[eframe::epaint::ClippedShape]) -> Vec<(String, Rect, usize)> {
    fn walk(shape: &egui::Shape, found: &mut Vec<(String, Rect, usize)>) {
        match shape {
            egui::Shape::Text(run) => found.push((
                run.galley.text().to_owned(),
                run.galley.rect.translate(run.pos.to_vec2()),
                run.galley.rows.len(),
            )),
            egui::Shape::Vec(inner) => {
                for shape in inner {
                    walk(shape, found);
                }
            }
            _ => {}
        }
    }
    let mut runs = Vec::new();
    for clipped in shapes {
        walk(&clipped.shape, &mut runs);
    }
    runs
}

/// The shipped settings window, run headless on a display of the given size
/// until the theme list is dropped open. Returns the display egui settled on
/// and every text run the open list emitted.
///
/// The real `draw_settings_window` on the real registry, opened by a real
/// click on the combo at the position the frame itself reports - a harness
/// that rebuilt the list would measure the harness.
fn open_theme_list(
    appearance: &Appearance,
    screen: egui::Vec2,
) -> (Rect, Vec<(String, Rect, usize)>) {
    let ctx = egui::Context::default();
    theme::apply(&ctx, appearance);
    // Never written: the page reads its defaults and nothing here clicks a
    // value, so no file is created.
    let mut store =
        settings::SettingsStore::open(std::env::temp_dir().join("theme-list-never-written.json"));
    let registry = settings_ui::full_registry(theme::settings::settings_category());
    let mut state = settings_ui::SettingsUi::default();
    state.open_category(theme::settings::keys::CATEGORY);

    let mut pass = |events: Vec<egui::Event>| {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), screen)),
            // egui drives a popup's fade off `predicted_dt`; at the default
            // sixtieth of a second a menu read two passes after it opened is
            // still part-way through appearing.
            predicted_dt: 0.25,
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            let _ = settings_ui::draw_settings_window(
                ui.ctx(),
                &mut state,
                settings_ui::SettingsWindowInput {
                    registry: &registry,
                    store: &mut store,
                    color_tables: None,
                    user_tables: None,
                },
            );
        })
    };

    // Four passes to settle the widths the layout reads back from the
    // previous one, then the click, then four more for the menu to appear.
    let mut output = None;
    for _ in 0..4 {
        output = Some(pass(Vec::new()));
    }
    let settled = output.as_ref().expect("at least one pass ran");
    let shown = catalog::DEFAULT.label;
    let at = text_runs(&settled.shapes)
        .into_iter()
        .find(|(text, _, _)| text.trim() == shown)
        .map(|(_, rect, _)| rect.center())
        .unwrap_or_else(|| panic!("the page never drew the theme combo's selected text {shown:?}"));
    let click = vec![
        egui::Event::PointerMoved(at),
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ];
    for index in 0..4 {
        let events = if index == 0 {
            click.clone()
        } else {
            Vec::new()
        };
        output = Some(pass(events));
    }
    let output = output.expect("at least one pass ran");
    (ctx.content_rect(), text_runs(&output.shapes))
}

/// A theme's description stays ON the display, however little of it there
/// is.
///
/// egui lays a menu's text out with `TextWrapMode::Extend` - deliberately,
/// so ordinary one-word menu entries do not wrap after two letters - and a
/// run that does not fit is not narrowed, it is CUT OFF at the edge. A theme
/// description is a whole sentence, which is why this list is the one place
/// that has to lay its own text out.
///
/// The three cases below are the ones that ran out of room: the interface
/// scale at its top step (the axis buys bigger type by leaving fewer points
/// on the same panel, so a 1024-pixel display is 640 points at 160 %), the
/// narrowest display the page supports, and the tightest density. The bench
/// is here as the control: there is room for a whole description on one line
/// and the fix must not take it away.
#[test]
fn the_theme_list_wraps_its_descriptions_instead_of_running_off_the_display() {
    let longest = catalog::THEMES
        .iter()
        .copied()
        .max_by_key(|theme| theme.description.len())
        .expect("the catalog registers at least one theme");
    for (what, screen, appearance, out_of_room) in [
        (
            "the bench",
            vec2(1024.0, 768.0),
            Appearance::default(),
            false,
        ),
        (
            "a 160 % interface scale",
            vec2(640.0, 480.0),
            Appearance {
                ui_scale: UiScale::Huge,
                ..Appearance::default()
            },
            true,
        ),
        (
            "the narrowest display the page supports",
            vec2(304.0, 720.0),
            Appearance::default(),
            true,
        ),
        (
            "the tightest density",
            vec2(1024.0, 768.0),
            Appearance {
                density: Density::Dense,
                ..Appearance::default()
            },
            false,
        ),
    ] {
        let (display, runs) = open_theme_list(&appearance, screen);
        let (text, rect, rows) = runs
            .iter()
            .find(|(text, _, _)| text.starts_with(longest.label) && text.contains('\n'))
            .unwrap_or_else(|| {
                panic!(
                    "on {what} the theme list never drew a described row for {:?}",
                    longest.label
                )
            });
        assert!(
            rect.left() >= display.left() - 0.5 && rect.right() <= display.right() + 0.5,
            "on {what} the longest theme description is laid out from {:.1} to {:.1} on \
             a display that runs {:.1} to {:.1}. The part past the edge is not narrow, \
             it is gone - an analyst choosing a theme reads half a sentence.\n  {text:?}",
            rect.left(),
            rect.right(),
            display.left(),
            display.right()
        );
        if out_of_room {
            assert!(
                *rows > 1,
                "on {what} the longest theme description still fits on {rows} row(s) \
                 ({:.1} points wide on a {:.1}-point display). Either the wrap width is \
                 not being applied or this case has stopped being the tight one it was \
                 chosen for.",
                rect.width(),
                display.width()
            );
        }
    }
}

/// Every axis is on the page, every option is derived, and every setting
/// carries help text — hover does not exist on glass and this application
/// ships to glass.
#[test]
fn every_appearance_axis_is_on_the_page_in_a_sensible_order() {
    let category = theme::settings::settings_category();
    let ids = category
        .settings
        .iter()
        .map(|spec| spec.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            theme::settings::keys::THEME,
            theme::settings::keys::ACCENT,
            theme::settings::keys::CHROME_EDGES,
            theme::settings::keys::DENSITY,
            theme::settings::keys::UI_SCALE,
        ],
        "colour first, then shape, then size - the order somebody actually changes them in"
    );
    for spec in &category.settings {
        assert!(
            !spec.help.is_empty(),
            "{}: no help text, and hover does not exist on glass",
            spec.id
        );
        assert!(spec.enabled, "{}: declared but not wired", spec.id);
        let settings::SettingKind::Choice { options, .. } = &spec.kind else {
            panic!("{}: every appearance axis is a choice", spec.id);
        };
        assert!(
            options.len() >= 2,
            "{}: a choice of one is not a choice",
            spec.id
        );
    }
    let counts = [
        (theme::settings::keys::ACCENT, Accent::ALL.len()),
        (theme::settings::keys::CHROME_EDGES, ChromeEdges::ALL.len()),
        (theme::settings::keys::DENSITY, Density::ALL.len()),
        (theme::settings::keys::UI_SCALE, UiScale::ALL.len()),
    ];
    for (id, expected) in counts {
        let spec = category
            .settings
            .iter()
            .find(|spec| spec.id == id)
            .expect("declared above");
        let settings::SettingKind::Choice { options, .. } = &spec.kind else {
            unreachable!()
        };
        assert_eq!(
            options.len(),
            expected,
            "{id}: the page lists {} options for an axis with {expected} values - \
             it is hand-listing instead of deriving",
            options.len()
        );
    }
}

/// The fallback rule, in the one function that implements it: a stored id
/// this build does not know resolves to the default, and nothing panics.
#[test]
fn an_unknown_stored_id_falls_back_to_the_default_on_every_axis() {
    let stranger = theme::settings::appearance_from_ids(
        Some("amber-crt-from-a-newer-build"),
        Some("chartreuse"),
        Some("hand-carved"),
        Some("extremely-dense"),
        Some("3.5"),
    );
    assert_eq!(stranger, Appearance::default());

    let missing = theme::settings::appearance_from_ids(None, None, None, None, None);
    assert_eq!(missing, Appearance::default());

    // Empty strings and blanks are strangers too, not a reason to panic.
    let blank =
        theme::settings::appearance_from_ids(Some(""), Some(" "), Some(""), Some("\n"), Some(""));
    assert_eq!(blank, Appearance::default());

    // ...and a value this build DOES know is honoured on every axis.
    let honoured = theme::settings::appearance_from_ids(
        Some("dark"),
        Some(Accent::Amber.id()),
        Some(ChromeEdges::Flat.id()),
        Some(Density::Dense.id()),
        Some(UiScale::Large.id()),
    );
    assert_eq!(honoured.theme.id, "dark");
    assert_eq!(honoured.accent, Accent::Amber);
    assert_eq!(honoured.edges, ChromeEdges::Flat);
    assert_eq!(honoured.density, Density::Dense);
    assert_eq!(honoured.ui_scale, UiScale::Large);
}

/// The stored id of a theme this build does not have must survive in the
/// settings document: an analyst who moves between builds gets their theme
/// back rather than being silently reset to the default.
#[test]
fn a_stranger_theme_id_is_resolved_around_and_not_written_over() {
    let category = theme::settings::settings_category();
    let spec = category
        .settings
        .iter()
        .find(|spec| spec.id == theme::settings::keys::THEME)
        .expect("declared");
    let stored = settings::SettingValue::Text("amber-crt-from-a-newer-build".to_owned());
    // The registry resolves it to the default...
    assert_eq!(
        spec.kind.sanitize(Some(&stored)),
        settings::SettingValue::Text(catalog::DEFAULT.id.to_owned())
    );
    // ...and `sanitize` is a read: the caller's own value is untouched, which
    // is what the store persists.
    assert_eq!(
        stored,
        settings::SettingValue::Text("amber-crt-from-a-newer-build".to_owned())
    );
}

// ---------------------------------------------------------------------------
// The chrome resolution seam
// ---------------------------------------------------------------------------

/// Switching themes leaves NOTHING of the previous one behind.
///
/// Every axis on the Appearance page applies live - an analyst changes the
/// theme and the window redraws in it, without a restart - so a context in
/// this application is re-themed over and over across a session. That is
/// only safe if installing a theme REPLACES the look rather than editing
/// it: one role that a later theme happens not to write would keep the
/// earlier theme's colour, and the result is a window that is mostly one
/// theme with a stale button or a stale border from another. Colour residue
/// is the classic failure of a themable UI and it is invisible in the theme
/// that caused it - you only see it in the theme you switched TO.
///
/// The check is exact and needs no eye: drag one context through every
/// registered theme and every axis value, then demand it be
/// indistinguishable from a context that was only ever given the final
/// appearance. Both egui style slots are compared, not just the one on
/// screen, because an OS light/dark flip can put the other one up at any
/// moment.
#[test]
fn switching_themes_repeatedly_leaves_no_trace_of_the_previous_one() {
    let churned = egui::Context::default();

    // Every theme, every accent, both edge modes, every density and every
    // scale - applied in that order and then again in reverse, so no axis
    // is only ever set on top of its own default.
    let mut visited = 0;
    for theme in catalog::THEMES {
        for accent in Accent::ALL {
            for edges in ChromeEdges::ALL {
                theme::apply(
                    &churned,
                    &Appearance {
                        accent,
                        edges,
                        ..Appearance::by_id(theme.id)
                    },
                );
                visited += 1;
            }
        }
    }
    for theme in catalog::THEMES.iter().rev() {
        for density in Density::ALL {
            for ui_scale in UiScale::ALL {
                theme::apply(
                    &churned,
                    &Appearance {
                        density,
                        ui_scale,
                        ..Appearance::by_id(theme.id)
                    },
                );
                visited += 1;
            }
        }
    }
    assert!(
        visited > 100,
        "the churn has to be long enough to actually accumulate residue; it made \
         only {visited} switches"
    );

    // Land every axis somewhere that is NOT its default, so a role restored
    // by luck to the shipped value cannot pass this.
    let landing = Appearance {
        accent: Accent::Amber,
        edges: ChromeEdges::Flat,
        density: Density::Dense,
        ui_scale: UiScale::Large,
        ..Appearance::by_id("dark")
    };
    theme::apply(&churned, &landing);

    let fresh = egui::Context::default();
    theme::apply(&fresh, &landing);

    assert_eq!(
        theme::active(&churned),
        theme::active(&fresh),
        "the appearance published in the context is not the one just installed"
    );
    for dark_mode in [false, true] {
        assert_eq!(
            theme::chrome_of(&churned, dark_mode),
            theme::chrome_of(&fresh, dark_mode),
            "after switching themes {visited} times the chrome on the \
             {} ground still differs from a clean install of the same appearance",
            if dark_mode { "dark" } else { "light" }
        );
    }
    // Field by field, not `Style == Style`. `Style` holds a
    // `number_formatter: NumberFormatter` whose `PartialEq` is
    // `Arc::ptr_eq` over a boxed closure, so two independently built styles
    // are NEVER equal however identical their colours - comparing whole
    // styles here fails on every theme and proves nothing. These four are
    // what `theme::style` writes, which is the whole of what a theme can
    // leave behind.
    for slot in [egui::Theme::Light, egui::Theme::Dark] {
        let (a, b) = (churned.style_of(slot), fresh.style_of(slot));
        assert_eq!(
            a.visuals, b.visuals,
            "the {slot:?} slot's COLOURS still carry an earlier theme: a role this \
             theme does not write is holding the previous one's value"
        );
        assert_eq!(
            a.spacing, b.spacing,
            "the {slot:?} slot's spacing did not follow the last density switch"
        );
        assert_eq!(
            a.text_styles, b.text_styles,
            "the {slot:?} slot's type ramp drifted across the switches"
        );
        assert_eq!(
            a.animation_time, b.animation_time,
            "the {slot:?} slot's animation time drifted across the switches"
        );
    }
    assert_eq!(
        churned.zoom_factor(),
        fresh.zoom_factor(),
        "the interface scale did not follow the last switch"
    );
}

/// The bevel primitives read the appearance out of the egui context, which
/// is what lets a contact sheet photograph several themes at once and what
/// makes `Palette::detect` return the analyst's accent rather than the
/// theme's.
#[test]
fn the_installed_appearance_travels_in_the_context_and_not_in_a_global() {
    let dark_amber = Appearance {
        accent: Accent::Amber,
        density: Density::Dense,
        edges: ChromeEdges::Flat,
        ..Appearance::by_id("dark")
    };
    let a = egui::Context::default();
    let b = egui::Context::default();
    theme::apply(&a, &dark_amber);
    theme::apply(&b, &Appearance::default());

    for (ctx, expected) in [(&a, dark_amber), (&b, Appearance::default())] {
        assert_eq!(theme::active(ctx), expected);
        let chrome = theme::chrome_of(ctx, expected.theme.ground.is_dark());
        assert_eq!(chrome.palette, expected.palette());
        assert_eq!(chrome.density, expected.density);
        assert_eq!(chrome.edges, expected.edges);
    }

    // A context nobody styled still hands back the shipped look rather than
    // stock egui's colours - a primitive drawn in somebody else's test must
    // not panic and must not come out grey.
    let bare = egui::Context::default();
    assert_eq!(theme::active(&bare), Appearance::default());
    assert_eq!(theme::chrome_of(&bare, false).palette, LIGHT);
    assert_eq!(theme::chrome_of(&bare, true).palette, DARK);
}

/// Both egui style slots carry the language whichever theme is chosen, so a
/// light/dark flip from the OS can never land on stock egui.
#[test]
fn both_style_slots_carry_the_language_for_every_theme() {
    for theme_spec in catalog::THEMES {
        let appearance = Appearance::by_id(theme_spec.id);
        let (light, dark) = appearance.slots();
        assert_eq!(light.theme.ground, Ground::Light);
        assert_eq!(dark.theme.ground, Ground::Dark);
        let chosen: &ThemeSpec = match theme_spec.ground {
            Ground::Light => light.theme,
            Ground::Dark => dark.theme,
        };
        assert_eq!(
            chosen.id, theme_spec.id,
            "the chosen theme must fill its own ground's slot"
        );
        // The axes travel with it: they are the analyst's choices, not the
        // theme's.
        assert_eq!(light.density, appearance.density);
        assert_eq!(dark.edges, appearance.edges);

        let ctx = egui::Context::default();
        theme::apply(&ctx, &appearance);
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.panel_fill,
            light.palette().face
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.panel_fill,
            dark.palette().face
        );
    }
}
