//! Modern flat: soft neutral greys, white data wells, hairline borders.
//!
//! The look for an analyst who wants current application chrome rather than
//! the bench's 3D edges. Everything the bench does with light and shadow this
//! theme does with a border and one step of tone: panels are a cool neutral
//! grey (`#DFE3E8`), controls stand one flat step proud of them, and data
//! wells are plain white instead of the bench's paper cream. The density, the
//! square corners and the information are untouched - only the surface
//! language changes, which is the whole point of it being a theme and not a
//! second application.
//!
//! # The one thing this theme cannot say for itself
//!
//! `ChromeEdges` is an appearance axis, not a theme field: `ThemeSpec` carries
//! a ground and a `Palette` and nothing else, and `Appearance::by_id` fills
//! the edge axis from `ChromeEdges::default()`, which is `Bevelled`. So
//! choosing this theme still draws the two-line bevel until the analyst also
//! sets **Chrome edges -> Flat**, and the description says so out loud rather
//! than pretending otherwise. (The seam that would close this is the one
//! `Accent::Theme` already uses: a theme-owned default with a "Theme's own"
//! option on the axis.)
//!
//! The palette is therefore tuned to read as modern flat in BOTH edge modes,
//! and the bevel ladder is where that work is:
//!
//! * `hi_inner` (`#E9ECF0`) sits 1.09:1 from the face, so the bevel's INNER
//!   lit ring is below the threshold of noticing. What survives of a raised
//!   bevel is the outer ring alone - a pale rim on the top and left, a grey
//!   line on the bottom and right - which is exactly "a flat surface with a
//!   border and a hint of elevation".
//! * `hi_outer` is `#FAFBFC` and not white. The role calls for the brightest
//!   value in the palette; here the brightest surface is the well, and a
//!   white rim on top of it would be the bench's lit edge rebuilt.
//! * `sh_outer` (`#A9B1BC`) is 1.68:1 on the face where the daylight bench's
//!   is 4.2:1. That single number is what decides whether this reads as a
//!   hairline or as a 3D step, and the role doc's "deep neutral" is exactly
//!   what it must not be.
//! * `sh_inner` (`#C2C9D2`, 1.30:1) is still strong enough that an etched
//!   group-box groove and the inset line of a sunken well are visible rather
//!   than absent, which is what stops "flat" from becoming "no structure".
//!
//! Switching the edge axis to `Flat` swaps every one of those lines for
//! `border_strong` (`#6F7885`, 3.47:1 on the face and 4.47:1 on the well), so
//! the theme tightens rather than changes.
//!
//! # Why the panel is this grey and not a lighter one
//!
//! An earlier draft had the face at `#ECEEF1`, which photographed well until
//! the scroll bars were looked at: egui fills a solid scroll handle with
//! `widgets.inactive.bg_fill`, which this language sets to `well`, so the
//! handle is a white bar on the panel and its whole visibility is the
//! face-to-well distance. At `#ECEEF1` that was 1.16:1 and the handle had
//! effectively vanished. `#DFE3E8` puts it at 1.29:1, level with the daylight
//! bench's own handle, without making the chrome heavy - the panel is still a
//! light cool grey and the wells still read as white content sitting on it.
//!
//! # Why these colours
//!
//! Neutral with a small cool cast - the face is `rgb(223, 227, 232)`, nine
//! points of blue above its red - so the greys stay quiet next to a radar
//! pane instead of competing with it. The accent is the same idea carried
//! into ink: `#35597F` is a blue-grey rather than a saturated system blue,
//! chosen so a link or a selected row registers on the chrome and still
//! disappears politely beside reflectivity. None of this touches the data;
//! the panes keep the map's own ground.
//!
//! Every pairing is measured by `tests/theme_catalog.rs` against every
//! registered accent. The floors are W3C, "Web Content Accessibility
//! Guidelines (WCAG) 2.2", W3C Recommendation, 2023: SC 1.4.6 (7:1) for body
//! text on the three grounds it is read off, SC 1.4.3 (4.5:1) for every other
//! live foreground, SC 1.4.11 (3:1) for the flat border. The tightest live
//! margins here are weak text on the face at 5.18:1 and the flat border on
//! the face at 3.47:1.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "modern-flat",
    label: "Modern flat",
    description: "Neutral grey surfaces, hairline borders and white data \
                  wells - the same density as the bench without the 3D \
                  edges. Set Chrome edges to Flat to drop the last of them.",
    ground: Ground::Light,
    palette: Palette {
        // The panel ground: a light cool grey, dark enough that a white well
        // - and a white scroll handle - reads as a thing sitting on it.
        face: Color32::from_rgb(223, 227, 232),
        // One flat step proud. A modern control at rest is a border with a
        // paler fill, not a lit block, so the step is tone and nothing else.
        face_raised: Color32::from_rgb(237, 240, 243),
        // Pressed drops two steps where raised rose one: with no sunken
        // bevel to help, the fill is carrying the whole cue.
        face_pressed: Color32::from_rgb(202, 207, 216),
        // Hover lifts to just under the well, so a pointer reads as the
        // control coming forward toward the content surface.
        hover: Color32::from_rgb(247, 249, 251),
        // White. The bench's wells are paper; these are a screen.
        well: Color32::from_rgb(255, 255, 255),
        // Near-black with the same cool cast as the greys, never pure black:
        // 12.85:1 on the face, 16.56:1 on the well.
        text: Color32::from_rgb(27, 31, 36),
        // Slate. 5.18:1 on the face and 6.68:1 on the well - a declared
        // secondary ink, not a faded primary.
        text_weak: Color32::from_rgb(84, 93, 104),
        // 2.03:1 on the face: plainly present, and under half of weak text's
        // ratio on every ground, so disabled can never read as live.
        text_disabled: Color32::from_rgb(152, 161, 172),
        // The rest outline of a control. 1.33:1 - a hairline, which is what
        // a flat control at rest is edged with.
        border: Color32::from_rgb(191, 198, 208),
        // The hovered and pressed outline, the window edge, and every line
        // the bevels would have drawn once the edge axis is Flat: 3.47:1 on
        // the face, 4.47:1 on the well (SC 1.4.11).
        border_strong: Color32::from_rgb(111, 120, 133),
        // Calm blue-grey, dark enough to read as ink: 5.64:1 on the face,
        // 7.27:1 on the well.
        link: Color32::from_rgb(53, 89, 127),
        // A pale wash of the same hue, so a selected row is a tinted band
        // rather than a painted block.
        selection_bg: Color32::from_rgb(189, 206, 222),
        // The link taken down to ink strength: 8.41:1 on the selection band,
        // and 10.50:1 on the face, where it is the focus ring.
        selection_text: Color32::from_rgb(20, 48, 74),
        // The face pulled toward the accent - a latched toolbar toggle,
        // which in this theme is a tinted fill under one flat inset line.
        selection_tint: Color32::from_rgb(203, 214, 225),
        // Amber taken down until it is ink rather than a highlighter:
        // 5.55:1 on the face, 7.15:1 on the well.
        warn: Color32::from_rgb(127, 76, 0),
        // Brick, deeper than the bench's red - it has to sit in a lot of
        // cool grey without shouting. 5.60:1 on the face.
        error: Color32::from_rgb(168, 34, 25),
        // The lit rim, and deliberately not white: see the module docs.
        hi_outer: Color32::from_rgb(250, 251, 252),
        // 1.09:1 on the face - the inner lit ring, tuned below the threshold
        // of noticing so a raised bevel collapses to a single line.
        hi_inner: Color32::from_rgb(233, 236, 240),
        // The visible soft grey: the etched groove, and the inset line of a
        // sunken well. 1.30:1.
        sh_inner: Color32::from_rgb(194, 201, 210),
        // The border-weight shade line. Not a deep neutral - a deep neutral
        // here would rebuild the bench's 3D step. 1.68:1.
        sh_outer: Color32::from_rgb(169, 177, 188),
    },
};
