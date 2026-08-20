//! The daylight bench: instrument grey, paper-light data wells.
//!
//! The founding look and the application's identity. The classic `#C0C0C0`
//! face family lifted well out of dinge - `face` is `#D8D5CE`, about nine
//! percent brighter and a hair warm, so on a modern panel it reads as
//! brushed instrument grey rather than as an old dialog.
//!
//! Every value below is pinned by `tests/theme_catalog.rs`. That test is not
//! ceremony: this file and `dark.rs` are the two looks the whole application
//! was drawn and photographed against, so a well-meant tweak here restyles
//! the product rather than adding to it. A new look belongs in a new file.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "light",
    label: "Daylight bench (Win95)",
    description: "Instrument grey with paper-light wells. The shipped look, \
                  for a lit room or a desk by a window.",
    ground: Ground::Light,
    palette: Palette {
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
    },
};
