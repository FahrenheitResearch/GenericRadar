//! Amber ops: black data wells, warm dark chrome, amber ink and ember
//! accents — for a darkened operations room where white light costs dark
//! adaptation.
//!
//! # Why a long-wavelength instrument
//!
//! Rods carry vision at scotopic levels and their sensitivity peaks near
//! 507 nm; by 590 nm the CIE scotopic curve V'(λ) is under a hundredth of
//! that peak, so amber light of a given photopic brightness bleaches far
//! less rhodopsin than white light of the same brightness. Hecht and Hsia
//! measured the consequence directly: after pre-adaptation to red rather
//! than white at matched photopic luminance, the dark-adaptation curve
//! recovers in a small fraction of the time (S. Hecht and Y. Hsia, "Dark
//! adaptation following light adaptation to red and white lights", Journal
//! of the Optical Society of America 35(4):261-267, 1945; see also G. Wald,
//! "Human vision and the spectrum", Science 101(2635):653-658, 1945, for the
//! photopic/scotopic pair this rests on). That is the whole reason ops rooms
//! and cockpits go red at night, and it is the only reason this theme
//! exists: an analyst who looks from this screen to a darkened room, a
//! window, or a paper chart should still be able to see.
//!
//! Amber rather than deep red is the working compromise. Below about 620 nm
//! there is still enough spread left in the spectrum to separate a caution
//! ink from an alarm ink, which a pure-red instrument cannot do at all; above
//! it, the rod protection is better but every hue collapses into one. This
//! file sits at the amber end of that trade and says below exactly what the
//! compromise cost.
//!
//! # Where the darkness actually is
//!
//! `well` is `#0C0908` — black with a warm cast — and that is where most of
//! this theme's screen area lives: every readout, list, text field and tilt
//! box. Against the night bench's `#181A1D` it carries 72 % less luminance,
//! which is a real difference an analyst sees.
//!
//! The panel `face` is `#3C2F29`, and it is NOT near-black: 22 % below the
//! night bench's `#36393E`, no more. That is forced rather than chosen. The
//! bevel grammar needs a rung BELOW the face (`sh_outer`) as well as above
//! it, and the founding language holds that outer shade edge to 1.5:1
//! against the face. A face at `#000000` has nowhere to put one — the
//! arithmetic bottoms out — so a black-chrome version of this theme would
//! draw raised buttons and sunken wells that look identical. `dark.rs` says
//! the same thing about graphite; this is the same wall approached from
//! further down, and `#3C2F29` is about the darkest face that still leaves
//! the ladder its bottom two rungs.
//!
//! That is not a loss, because the face is not what costs an analyst their
//! night vision. Dark adaptation is spent by the light a display EMITS, and
//! emission is dominated by the bright pixels — the ink, the lit bevels, the
//! selection. Those are the long-wavelength ones here. A dim brown panel and
//! a black panel emit almost nothing either way; an amber glyph and a white
//! glyph do not.
//!
//! # The ink is a pale amber, and that is measured, not preferred
//!
//! An amber CRT's phosphor sits around `#FFB000`, and that was the first
//! value in this file. It fails: `tests/theme_catalog.rs` crosses every
//! registered theme with every accent, and body ink is painted on the
//! ACCENT's selection ground. `#FFB000` on Instrument blue's `#2E5A96`
//! measures 3.80:1, below the 4.5:1 floor (WCAG 2.2 SC 1.4.3). The floor is
//! the discipline, so the ink moved: `#FFCD91` clears every accent's
//! selection ground, the worst of them — blue again — at 4.77:1, and still
//! reads as amber rather than white (its blue channel is 145 against a red
//! of 255). Anyone tempted to deepen it back toward the phosphor should
//! re-run the audit first; it is the accent crossing, not the theme's own
//! colours, that sets this floor.
//!
//! # The four warm inks
//!
//! Every ink here is long-wavelength on purpose, which means none of them
//! can escape into blue or green to become distinguishable. They are spread
//! across the warm band instead, and separated by saturation and intensity
//! as well as hue:
//!
//! | Role    | Value     | Hue  | What carries it |
//! |---------|-----------|------|-----------------|
//! | `error` | `#FF7C6E` |   6° | red, the only ink in the band |
//! | `link`  | `#FF8F4E` |  22° | ember orange, far more saturated and 1.55:1 deeper than body ink |
//! | `text`  | `#FFCD91` |  33° | amber, the palest and least saturated — the reading ink |
//! | `warn`  | `#FFDD33` |  50° | gold, brighter AND far purer than body ink |
//!
//! `text_weak` and `text_disabled` step down the same amber at low
//! saturation, so secondary text reads as *greyed* while a link reads as
//! *coloured* — that saturation gap is what separates them, since at this
//! luminance they cannot differ much in hue. This is the real cost of the
//! look and it is stated here rather than discovered later.
//!
//! # The accent axis
//!
//! The theme's own accent is the ember red the selection and the latched
//! toggles are painted in. The Appearance page will happily put Instrument
//! blue, Teal or Violet on this theme — every one of them is measured and
//! legible here — but a short-wavelength accent trades away the property the
//! theme exists for. Amber is the accent that agrees with it.
//!
//! Data is untouched either way: the radar panes draw on the map's own
//! colours (`map_scene::MapChrome`), so nothing about this theme changes
//! what a gate means.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "amber-ops",
    label: "Amber ops",
    description: "Amber ink and red accents on black wells and warm dark \
                  chrome. For a darkened operations room, where white light \
                  costs dark adaptation.",
    ground: Ground::Dark,
    palette: Palette {
        // The face steps: a lamp-warmed dark brown, one step up for a button
        // at rest, one down for a pressed one, and the lightest step for the
        // pointer. The ratios between them — 1.16 raised, 1.20 pressed —
        // are the night bench's 1.15 and 1.21, so a control reads as proud
        // or pushed by the amount it always has.
        face: Color32::from_rgb(60, 47, 41),
        face_raised: Color32::from_rgb(71, 57, 49),
        face_pressed: Color32::from_rgb(44, 34, 30),
        hover: Color32::from_rgb(87, 69, 60),
        // Data wells: black with a warm cast, deeper than the chrome so
        // readouts and lists sit visually behind the instrument.
        well: Color32::from_rgb(12, 9, 8),
        // The reading ink. See the module docs for why it is not `#FFB000`.
        text: Color32::from_rgb(255, 205, 145),
        // Secondary and disabled: the same amber walked down in saturation
        // as well as luminance, so they read as greyed rather than coloured.
        // Disabled sits at 2.55:1 on a raised face — present, and barely
        // half of weak text's 4.45:1, which is what stops a dead control
        // from reading as a live one.
        text_weak: Color32::from_rgb(188, 159, 134),
        text_disabled: Color32::from_rgb(140, 117, 101),
        // Control outlines. `border` is the visible edge at rest (1.72:1 on
        // a raised face); `border_strong` is the hovered/pressed edge and,
        // in `ChromeEdges::Flat`, every edge the bevels would have drawn,
        // which is why it is held at 3.81:1 on the face and 5.87:1 on the
        // well (WCAG 2.2 SC 1.4.11).
        border: Color32::from_rgb(112, 90, 76),
        border_strong: Color32::from_rgb(160, 135, 116),
        // The accent roles: ember, one hue step hotter than the ink.
        link: Color32::from_rgb(255, 143, 78),
        selection_bg: Color32::from_rgb(122, 42, 24),
        selection_text: Color32::from_rgb(255, 227, 204),
        // A latched toggle: the face pulled toward the ember and lifted just
        // above it (1.19:1), so a toggle that is ON glows under its sunken
        // bevel instead of merely being outlined.
        selection_tint: Color32::from_rgb(90, 51, 38),
        // The two loud inks. Gold is brighter and far purer than the body
        // amber; red is the one ink at the bottom of the band.
        warn: Color32::from_rgb(255, 221, 51),
        error: Color32::from_rgb(255, 124, 110),
        // The bevel ladder. On chrome this dark the lit side does most of
        // the sculpting — `hi_outer` stands 1.83:1 over the face — while the
        // two shade rungs have only the narrow room between the face and
        // black to work in (1.31:1 and 1.53:1). `sh_outer` is a warm
        // near-black rather than `#000000`: the pure-black outline is the
        // one part of the Win95 grammar this language declines, and here it
        // would also flatten the well's edge into the well.
        hi_outer: Color32::from_rgb(106, 84, 73),
        hi_inner: Color32::from_rgb(82, 64, 56),
        sh_inner: Color32::from_rgb(35, 27, 24),
        sh_outer: Color32::from_rgb(13, 10, 9),
    },
};
