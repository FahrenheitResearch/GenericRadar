//! Storm night: a true-dark console for a dark room and an OLED panel.
//!
//! The night bench in `dark.rs` is graphite — `#36393E`, bright enough that
//! the Win95 bevel ladder has two shade rungs below it and still measures.
//! This theme takes the other side of that trade on purpose. `face` is
//! `#171B22`: near-black with a cold slate cast, about a quarter of the
//! graphite bench's luminance, so at 3 a.m. in an unlit room the instrument
//! stops being a lit rectangle and the echo is the only bright thing left on
//! the screen. On an OLED panel the wells at `#070A0D` are very nearly
//! pixels-off, which is where that class of display is at its best.
//!
//! # What near-black costs, and where the geometry went instead
//!
//! WCAG contrast is a ratio with a `+0.05` viewing-flare term in it (W3C,
//! "Web Content Accessibility Guidelines (WCAG) 2.2", W3C Recommendation,
//! 2023, SC 1.4.3), and down here that term dominates: `face` is `0.0108`
//! relative luminance, so *everything* darker than it — the well, both
//! shade rungs — is crushed into a band 1.10:1 to 1.18:1 wide no matter what
//! numbers are chosen. The founding pair's bevel floors (1.3:1 for a lit
//! edge and an inner shade, 1.5:1 for the outer shade, pinned for `light`
//! and `dark` in `tests/theme_contract.rs`) are unreachable *below* a
//! near-black face; they are not a rule this theme is dodging, they are a
//! statement about faces 3.8× and 62× brighter than this one.
//!
//! So the sculpting is moved to the side that still has room — upward:
//!
//! * `hi_outer` `#4B5766` is 8.6× the face's luminance, 2.35:1 — the lit
//!   top/left of a raised block and the lit bottom/right of a sunken one is
//!   what actually draws the geometry here, and it is a stronger edge than
//!   either founding theme carries, not a weaker one. It is also kept above
//!   `border`, so that wherever a real bevel is painted the lit line is the
//!   brightest thing on the control rather than its resting outline; the
//!   first cut of this theme had that inverted and the photographs showed
//!   flat outlined boxes where sculpted ones were intended.
//! * The shade rungs stay in the ladder's declared order and stay *real* in
//!   the only terms that mean anything at these levels — ratios of emitted
//!   light, not WCAG ratios: `face` 0.0108, `sh_inner` 0.0043 (2.5×),
//!   `sh_outer` 0.0018 (6.1×). On a panel in a dark room those are plain
//!   steps; they photograph as plain steps; and the ladder is monotone, so
//!   `bevel::paint_bevel` composes the same five-step grammar it always did.
//! * `border` `#414A57` (1.93:1 on face) carries more weight than it does on
//!   the lighter themes: when the face steps are this close together, the
//!   resting outline is what tells an analyst where one control ends and the
//!   next begins. `border_strong` at 3.86:1 keeps `ChromeEdges::Flat`
//!   legible above the SC 1.4.11 graphics floor.
//!
//! `sh_outer` is `#04060A`, not `#000000`. The one piece of the 1995
//! grammar this application declines is the pure-black outline ("The Windows
//! Interface Guidelines for Software Design", Microsoft Press, 1995, ch. 13),
//! and a theme that reached black for its darkest rung would also have
//! nothing left to separate a sunken lip from the well beside it.
//!
//! # The ink, and why it is not white
//!
//! `text` is `#D5DBE3`, a cool grey — 12.4:1 on the face, comfortably past
//! the AAA floor, and deliberately short of `#FFFFFF`, whose 17.3:1 on this
//! ground is the glare an analyst is turning the lights off to avoid. It is
//! not free to go softer either: the accent axis crosses every theme with
//! every named accent, and `Accent::Blue`'s `selection_bg` `#2E5A96` is the
//! brightest ground any body text lands on here. `#D5DBE3` clears it at
//! 4.99:1; two steps darker does not.
//!
//! # The accent
//!
//! One cold cyan family, used sparingly: `link` `#5AB4DC`, selection on
//! `#17506F`, and a latched toolbar toggle tinted `#14313F` — 1.27:1 above
//! the face, so a latched control reads by hue and by its sunken bevel
//! rather than by brightness. Cyan rather than the bench's blue because at
//! this ground level blue and grey converge, and because it is the one
//! saturated thing in the chrome, which is the whole budget.
//!
//! Data is never tinted by any of this: the radar pane draws on the map's
//! own colours, and the only thing this file changes about a sweep is how
//! dark the frame around it is.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "storm-night",
    label: "Storm night",
    description: "Near-black chrome for an unlit room or an OLED panel, dimmed \
                  so the echo is the brightest thing on the screen.",
    ground: Ground::Dark,
    palette: Palette {
        // The ground: cold slate, near-black. Everything else is measured
        // off this one number.
        face: Color32::from_rgb(23, 27, 34),
        // A button at rest, 2.4× the face in emitted light — small in WCAG
        // terms, plain on a panel, and carried the rest of the way by the
        // lit bevel edge above it.
        face_raised: Color32::from_rgb(38, 45, 55),
        // Pressed sinks toward the well rather than merely darkening, which
        // is the only direction left below a near-black face.
        face_pressed: Color32::from_rgb(14, 17, 22),
        // The lightest face step, 4× the face: hover is the one state that
        // has to be unmistakable from across a dark room.
        hover: Color32::from_rgb(51, 59, 71),
        // Data wells, all but pixels-off on an OLED. Deeper than the chrome,
        // so a text edit or a readout sits visually behind the instrument.
        well: Color32::from_rgb(7, 10, 13),
        // Cool grey, not white. See the module docs: this is the softest ink
        // the accent axis leaves room for.
        text: Color32::from_rgb(213, 219, 227),
        // Hints, captions, status lines: 5.9:1 on the face, plainly ink and
        // plainly secondary.
        text_weak: Color32::from_rgb(143, 152, 165),
        // Disabled: present at 2.6:1 on the face, half the weight of
        // `text_weak` on every ground, so an unavailable control can never
        // be mistaken for a live one (SC 1.4.3 exempts inactive components).
        text_disabled: Color32::from_rgb(85, 93, 104),
        // The resting outline. Stronger here relative to its face than on
        // the lighter themes, because with face steps this close it is the
        // border that separates one control from the next.
        border: Color32::from_rgb(65, 74, 87),
        // Hover and window edges, and every edge in `ChromeEdges::Flat`:
        // 3.86:1 on the face and 4.44:1 on the well (SC 1.4.11 wants 3:1).
        border_strong: Color32::from_rgb(110, 120, 135),
        // The accent, cold cyan: 7.4:1 on the face.
        link: Color32::from_rgb(90, 180, 220),
        selection_bg: Color32::from_rgb(23, 80, 111),
        selection_text: Color32::from_rgb(234, 244, 250),
        // A latched toggle: the face pulled toward the accent, 1.27:1 above
        // it — hue and the sunken bevel do the latching, not brightness.
        selection_tint: Color32::from_rgb(20, 49, 63),
        // Amber and a low-saturation red. Fully saturated inks bloom on a
        // near-black ground; these are pulled back until they read as text.
        warn: Color32::from_rgb(232, 169, 60),
        error: Color32::from_rgb(240, 133, 122),
        // The bevel ladder, weighted upward: the lit edge is the strongest
        // rung (2.35:1, and brighter than `border`), while the two shade
        // rungs are 2.5× and 6.1× drops in emitted light below the face.
        hi_outer: Color32::from_rgb(75, 87, 102),
        hi_inner: Color32::from_rgb(43, 51, 62),
        sh_inner: Color32::from_rgb(11, 14, 18),
        sh_outer: Color32::from_rgb(4, 6, 10),
    },
};
