//! Paper: warm off-white grounds, dark ink, and rules instead of relief.
//!
//! The look of a plotted chart lying on a desk. `face` is a cream sheet
//! (`#EAE4D7`), the data wells are a whiter sheet laid on top of it
//! (`#FDFCF7`), and the ink is a warm near-black (`#201C15`) — browner than
//! the bench themes' neutral ink, because it is sitting on a warm sheet.
//! What separates one region from another is a line, not a light source.
//!
//! It exists for the rooms and the outputs the two bench themes are wrong
//! for: a workstation under a window at midday, a briefing throw where a
//! grey instrument face turns to mud on the wall, and the screenshot that
//! ends up in a printed case study, where an instrument-grey chrome costs
//! toner and prints as a smudge.
//!
//! # The one departure from the founding grammar
//!
//! Every other part of the language is kept: square corners, one physical
//! pixel per line, visible control edges, data untinted by the chrome. The
//! bevel ladder is where this theme parts company with the Windows
//! convention it inherits ("The Windows Interface Guidelines for Software
//! Design", Microsoft Press, 1995, ch. 13), and it is worth being explicit
//! about, because the four `hi_*`/`sh_*` roles are named for a light source
//! this theme does not have.
//!
//! On the bench themes those four are a ladder of *brightness*: a lit edge
//! above the face, two shade steps below it, and a block looks raised
//! because light appears to fall on it from the top-left. Paper has no such
//! light. A printed box is defined by the weight of the line around it, so
//! here the ladder is a ladder of **ink weight** instead, and the four roles
//! are filled with two of them:
//!
//! | Role       | Value     | On `face` | What it draws |
//! |------------|-----------|-----------|---------------|
//! | `hi_outer` | `#C8C0B0` | 1.43 : 1  | the quiet hairline, light side |
//! | `hi_inner` | `#F0EBE0` | 1.07 : 1  | one step above the sheet; deliberately all but invisible, so a raised block gets ONE line on its lit side and not two |
//! | `sh_inner` | `#B8B09E` | 1.70 : 1  | the quiet hairline, shade side — the one an etched groove and a well's top edge are made of, so it is the heavier of the two hairlines |
//! | `sh_outer` | `#6E6657` | 4.48 : 1  | the rule |
//!
//! The consequence is what the theme is for. Because `hi_outer` and
//! `sh_inner` are both real lines, every bevelled box closes on all four
//! sides — a raised strip is not a lit corner and a dark corner, and a well
//! is not open at the bottom right. And because `sh_outer` is heavier than
//! either hairline, the box still says which way is "out": the rule falls on
//! the side the shadow used to, which is a plotted drop rule rather than a
//! bezel. `hi_inner` sits just above the sheet so that the second lit line
//! of a two-ring raised bevel disappears into the paper and the edge stays a
//! hairline.
//!
//! `sh_outer` and `border_strong` are the same ink on purpose: the rule is
//! one weight in this theme, so `ChromeEdges::Flat` — which paints
//! `border_strong` on all four sides — is not a different look here but the
//! same drawing with the drop rule taken off. That is the exception to
//! "every value chosen separately", and it is the choice, not a copy.
//!
//! # White is data
//!
//! The second rule, and the one the photographs changed. `well` (`#FDFCF7`)
//! is the only near-white value in the palette: every chrome surface —
//! panel, button, hovered button, pressed button — stays on the cream side
//! of it. The first draft let `hover` go to `#FAF8F2`, and on the real
//! toolbar a hovered button then read as one more white readout in a row of
//! them. Pulling it back to `#F8F5EC` keeps the lift (1.16 : 1 over the
//! sheet, plus the thin bevel the hover paints) and gives the whitest thing
//! on screen back to the data.
//!
//! The minimal-ink reasoning is Tufte's, "The Visual Display of Quantitative
//! Information", Graphics Press, 1983, ch. 4: non-data ink earns its place
//! or comes off. That argument is about charts, and the radar pane is not
//! styled from here — it is the chrome AROUND the data that this theme takes
//! the ink out of.
//!
//! # Measured
//!
//! Against the floors in `tests/theme_catalog.rs` (W3C, "Web Content
//! Accessibility Guidelines (WCAG) 2.2", W3C Recommendation, 2023 — SC 1.4.6
//! for body text, SC 1.4.3 for the rest, SC 1.4.11 for the flat border), on
//! the theme's own accent:
//!
//! * body text 13.4 : 1 on `face`, 14.8 : 1 on a raised face, 16.5 : 1 on a
//!   well (floor 7);
//! * weak text 5.6 : 1 on `face`, 6.9 : 1 on a well (floor 4.5);
//! * the plotter-blue link 6.6 : 1 on `face`, 8.2 : 1 on a well (floor 4.5);
//! * the flat border 4.5 : 1 on `face`, 5.5 : 1 on a well (floor 3);
//! * disabled ink 2.2 : 1 on `face` — present, and under half the strength
//!   of weak text, which is the affordance.
//!
//! The whole cross-product against the five named accents is measured by the
//! catalog audit; nothing here is exempted from it.
//!
//! # Beside the pane
//!
//! A radar pane draws on the map's own near-black ground and does not take
//! colour from the chrome, so this theme meets a dark rectangle on every
//! screen it is used on. The pane's sunken edge is painted INSIDE that
//! rectangle, which is why both hairlines are chosen light: `#C8C0B0` and
//! `#B8B09E` are 10.6 : 1 and 8.9 : 1 against the map ground, so the frame
//! reads as a drawn border on all four sides rather than dissolving into the
//! echo. The rule (`#6E6657`) still holds 3.4 : 1 there, so the pane is
//! outlined whichever edge language is in force.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "paper",
    label: "Paper",
    description: "Ink on off-white paper, ruled instead of bevelled. For a bright \
                  room, a projector, or a screenshot that has to print.",
    ground: Ground::Light,
    palette: Palette {
        // The sheet the instrument is drawn on: warm, and clearly not white,
        // so the whiter wells laid on it read as separate paper.
        face: Color32::from_rgb(234, 228, 215),
        // A control stands proud by a step of paper, not by a highlight.
        face_raised: Color32::from_rgb(243, 239, 230),
        // Pressed: the sheet in shadow. The one face step that goes down.
        face_pressed: Color32::from_rgb(218, 211, 194),
        // Under the pointer, the sheet catches the light — but it stops
        // short of white. White belongs to the data well alone here (see the
        // module docs): a hovered control that went white read, in the
        // photographs, as one more readout on the bar.
        hover: Color32::from_rgb(248, 245, 236),
        // The plot field: the whitest thing in the theme, and the only white
        // thing in it, so a data area is legible as a fresh sheet on the desk.
        well: Color32::from_rgb(253, 252, 247),
        // Printer's ink, not screen black: warm, to sit on a warm sheet.
        text: Color32::from_rgb(32, 28, 21),
        // A second pass of the same warm ink, thinned.
        text_weak: Color32::from_rgb(95, 88, 73),
        // Pencil, not ink. Present at 2.2 : 1, and far below weak text, so a
        // disabled control cannot be mistaken for a live one.
        text_disabled: Color32::from_rgb(162, 154, 134),
        // The hairline a control is boxed with at rest.
        border: Color32::from_rgb(169, 160, 141),
        // THE RULE. The flat-chrome edge, the window edge, and the edge of
        // anything hovered or held. Same ink as `sh_outer`.
        border_strong: Color32::from_rgb(110, 102, 87),
        // Plotter blue: a pen colour, saturated enough not to go muddy on
        // cream the way a greyed instrument blue would.
        link: Color32::from_rgb(27, 77, 143),
        // A wash over the sheet rather than a filled bar; cool, so it
        // separates from the warm ground by hue as well as by value.
        selection_bg: Color32::from_rgb(179, 201, 230),
        // The wash's own ink, and the focus ring on plain chrome.
        selection_text: Color32::from_rgb(18, 54, 95),
        // A latched control: the sheet washed cool. A wider step off the
        // ground than the founding light theme's latch (1.41 : 1 against
        // `face`, where that one is 1.32 : 1 against its own), because the
        // quiet rules around it carry less of the "this one is on" than a
        // two-line bevel does.
        selection_tint: Color32::from_rgb(184, 195, 211),
        // Ochre. Amber goes invisible on paper this bright.
        warn: Color32::from_rgb(138, 80, 0),
        // Correction red, the second pen in the plotter.
        error: Color32::from_rgb(171, 36, 25),
        // The lit side's hairline (see the module docs: weight, not light).
        hi_outer: Color32::from_rgb(200, 192, 176),
        // One step above the sheet, and no more: the inner lit line of a
        // raised bevel is meant to vanish so the edge stays a single line.
        hi_inner: Color32::from_rgb(240, 235, 224),
        // The shade side's hairline: an etched groove and a well's top edge
        // are made of this, so it is the heavier of the two hairlines.
        sh_inner: Color32::from_rgb(184, 176, 158),
        // The rule again. One weight, whichever edge language is in force.
        sh_outer: Color32::from_rgb(110, 102, 87),
    },
};
