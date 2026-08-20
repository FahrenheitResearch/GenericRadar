//! The night bench: graphite chrome, data wells darker than the panels.
//!
//! The founding dark look, because radar analysts work storms at night.
//! Graphite, not black: `face` is `#36393E`, bright enough that the bevel
//! grammar still has room on both sides - a lit edge above it and two shade
//! steps below it - where a near-black chrome would leave the language
//! nowhere to go. Data wells are darker than the chrome so imagery pops.
//!
//! Every value below is pinned by `tests/theme_catalog.rs`; see `light.rs`
//! for why the two founding files are frozen rather than tuned.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "dark",
    label: "Night bench",
    description: "Graphite chrome with wells deeper than the panels. For a \
                  dark room, and the reason the bevels are graphite too.",
    ground: Ground::Dark,
    palette: Palette {
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
    },
};
