//! High visibility: the accessibility bench. Black ink, neutral chrome, and
//! nothing on the panel that an analyst has to lean in to read.
//!
//! This is the look to switch to for low vision, for a bench under glare, or
//! for anyone who has turned the interface scale up — it is drawn to be read
//! at 1.25× and 1.5× as much as at 1×, because that is the setting it will
//! usually be wearing.
//!
//! # The standard it holds
//!
//! W3C, "Web Content Accessibility Guidelines (WCAG) 2.2", W3C
//! Recommendation, 2023.
//!
//! * **Every LIVE text role clears 7:1** — SC 1.4.6 (AAA) — on every ground
//!   the chrome actually paints it on, the transient hover and pressed faces
//!   included. Not only body text (14.5:1 on the face, 21:1 on the well):
//!   secondary text is 8.7:1, the link 7.5:1, the warning ink 7.3:1, the
//!   error ink 10.5:1. The catalog audit demands AAA of `text` alone and AA
//!   of the rest; here there is no second tier of writing that is allowed to
//!   be harder to read, because in this theme nothing on the panel is
//!   decorative. Measured end to end rather than asserted: the real toolbar
//!   proof in `examples/theme_gallery.rs` reads every text run the shipped
//!   bar emits back out of the rendered frame, and this theme's *weakest*
//!   run there is 7.13:1.
//! * **Every control border clears 3:1 at rest** — SC 1.4.11 — not just the
//!   hovered one. `border` is 3.5:1 on the face and 5.1:1 on the well, where
//!   the daylight bench's resting edge is 2.2:1 and the night bench's 1.9:1:
//!   both save the graphics floor for `border_strong`, which only appears
//!   once a control is hovered or pressed. An affordance you can only find
//!   by hovering is an affordance a keyboard or touch analyst never finds.
//! * **Disabled text stops just under the AA text floor**, at 2.95:1 on the
//!   face and 4.29:1 at its strongest (on the well) — the lightest ink that
//!   still fails SC 1.4.3 on every ground, and three times weaker than
//!   secondary text on each. SC 1.4.3 exempts inactive components from the
//!   floor; it does not excuse making them vanish, and an analyst who cannot
//!   read a greyed-out label cannot tell which control is unavailable.
//!
//! # Colour-blind safety
//!
//! Okabe, M. and Ito, K., "Color Universal Design (CUD): How to make figures
//! and presentations that are friendly to Colorblind people", J*FLY, 2008 —
//! the eight-colour universal set, popularised in English by Wong, B.,
//! "Points of view: Color blindness", *Nature Methods* 8(6):441, 2011.
//!
//! Two rules from that work are what this palette is built on.
//!
//! 1. **No hue carries meaning on its own.** The neutral ladder here is
//!    exactly neutral — `#D6D6D6`, `#E6E6E6`, `#BEBEBE`, R = G = B all the
//!    way down, where the daylight bench carries a warm cast. That is not
//!    austerity for its own sake: with no chroma in the chrome, every hue an
//!    analyst does see is one this palette put there deliberately, and a
//!    dichromat is never asked to separate a tinted face from a plain one.
//! 2. **Where two inks must be told apart, separate them on lightness as
//!    well as hue**, because lightness is the one channel every common
//!    colour-vision deficiency keeps. `warn` and `error` are the only pair
//!    in this palette that an analyst reads side by side — the status line
//!    puts a mesocyclone callout next to a TVS callout — so `warn` is an
//!    umber at relative luminance 0.050 and `error` a plum-crimson at
//!    0.019, a 1.45:1 step apart, their blue channels differing (00 to 22)
//!    so the residual hue difference survives protanopia and deuteranopia
//!    too. Vermillion-on-orange, the obvious pairing, is the exact confusion
//!    Okabe and Ito warn about; it is not used. Neither is green, anywhere.
//!
//! `selection_bg` is Okabe–Ito sky blue `#56B4E9` **verbatim** — the one
//! token taken from the published set unmodified, because at relative
//! luminance 0.41 it carries black text at 9.1:1 unaltered. The link and
//! selection inks are the set's blue `#0072B2` taken down in luminance (to
//! `#004067` and `#002136`) until they clear AAA on a light face; hue and
//! the blue/orange separation that makes the set universal are preserved,
//! only the lightness moves. That is the standard CUD adjustment for ink on
//! a light ground, and it is why these are not the published hex values.
//!
//! # Why a light ground, and where the contrast could not go
//!
//! Both extremes were measured and both were rejected, because this
//! application's language is a bevel and a bevel needs headroom on *both*
//! sides of the face.
//!
//! * A near-white face (`#F2F1EE`) leaves `#FFFFFF` a 1.13:1 lit edge — the
//!   top-left of every raised control disappears.
//! * A near-black face (`#101215`) leaves the outer shade a 1.12:1 drop to
//!   black — the bottom-right goes with it. `dark.rs` says the same thing
//!   about graphite.
//!
//! So the face sits at the *same luminance the daylight bench uses*, which
//! is the value the bevel ladder was tuned around, with the warm cast taken
//! out. The visibility is then bought where it is free: pure black ink
//! (14.5:1 on the face, 21:1 on the well), a pure white well, and a bevel
//! whose outer ring runs white to `#2B2B2B` — 14.2:1 across a single
//! control edge, against 6.7:1 on the daylight bench. This theme restores
//! the near-black outline that the shipped pair deliberately declines (see
//! `palette.rs` on the Win95 grammar): here the edge has to carry, so it is
//! drawn to be seen rather than to be tasteful.
//!
//! A dark ground would have kept more chroma in the inks — saturated colour
//! holds up better at high luminance than at low — but it costs the bevel
//! (best achievable outer ring, 9.7:1), it flattens the gap between primary
//! and secondary text, and it removes the figure/ground step between light
//! chrome and a dark radar pane. Light won on all three.
//!
//! The honest cost of that choice is written into the inks: at AAA on a
//! light face, amber darkens to umber and red to plum. They are quieter
//! hues than a night bench could carry. They are also the loudest either
//! can be while staying legible, which is the trade this theme exists to
//! make.
//!
//! # What a theme cannot do here
//!
//! Focus-ring *width* is not a palette token. Both rings — egui's, drawn in
//! `selection_text`, and the toolbar's, drawn in `link` by
//! `bevel::toolbar_button` — are one physical pixel, fixed in code this
//! theme does not own. What is available is contrast, so both are taken far
//! past the 3:1 graphics floor: 11.4:1 for the egui ring on the face, and
//! 7.5:1 for the toolbar ring on a raised face, 3.7:1 at its worst (inside a
//! latched toggle). Anyone widening the ring should widen it here first and
//! this paragraph should shrink.
//!
//! The other one, found by photographing the panel and looking at it: egui
//! fills a **slider rail and a scrollbar handle with `well` and strokes
//! neither**, so both are white shapes lying on the face at 1.45:1 with no
//! outline — the faintest thing on the panel by some way. No palette can fix
//! it. `well` is already pure white, which is the value that maximises the
//! well-against-face step on a light ground, and the face cannot be darkened
//! to widen it because the amber accent's link needs a face at or above this
//! luminance to hold its own 4.5:1 (it lands at 4.68:1 as things are). A
//! stroked rail has to come from the chrome, not from here.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "high-visibility",
    label: "High visibility",
    description: "For low vision or a glare-lit bench: 7:1 text (WCAG AAA), \
                  colour-blind-safe inks, and a visible edge on every \
                  control. Pairs with a raised interface scale.",
    ground: Ground::Light,
    palette: Palette {
        // The neutral ladder. Same luminance as the daylight bench's face,
        // no warm cast, so hue anywhere in this theme means something.
        face: Color32::from_rgb(214, 214, 214),
        face_raised: Color32::from_rgb(230, 230, 230),
        face_pressed: Color32::from_rgb(190, 190, 190),
        hover: Color32::from_rgb(238, 238, 238),
        // Pure paper. The well is where numbers are read, so it is the one
        // surface taken all the way to 21:1 under black ink.
        well: Color32::from_rgb(255, 255, 255),
        // Pure black, chosen knowing the cost: at 21:1 on white it can
        // halate for readers with astigmatism. That reader has the daylight
        // bench, which is softer by design; this theme is the maximum, and a
        // maximum that stops short of black is not one.
        text: Color32::from_rgb(0, 0, 0),
        // Secondary text at 8.7:1 — above AAA, not merely above AA. Close
        // to `text` on purpose: the hierarchy here is carried by weight and
        // placement, not by making half the panel harder to read.
        text_weak: Color32::from_rgb(51, 51, 51),
        // The one token set by a rule rather than a taste: the lightest
        // disabled ink that still fails the AA text floor on every ground it
        // lands on (2.95:1 on the face, 3.44:1 raised, 4.29:1 on the well —
        // the well is what stops it going further). Deliberately stronger
        // than the founding themes' disabled ink, because a low-vision
        // analyst still has to be able to READ which control is unavailable;
        // the affordance is the three-fold drop from secondary text and
        // five-fold from body text, not illegibility. It also lands where
        // egui's own automatic fade lands for stock widgets (#6F6F6F on this
        // face), so the two disabled paths in the application look alike
        // instead of like two different states.
        text_disabled: Color32::from_rgb(122, 122, 122),
        // The resting edge, already at the SC 1.4.11 graphics floor (3.5:1
        // on the face) rather than a hairline waiting to be hovered.
        border: Color32::from_rgb(110, 110, 110),
        // Near-black: this is the whole affordance in `ChromeEdges::Flat`,
        // and the hovered/pressed edge everywhere else. 9.7:1 on the face.
        border_strong: Color32::from_rgb(43, 43, 43),
        // Okabe–Ito blue #0072B2, luminance-lowered until it clears AAA on
        // the face (7.5:1) as well as the well (10.9:1). Also the toolbar
        // focus ring, which is why it is taken this far.
        link: Color32::from_rgb(0, 64, 103),
        // Okabe–Ito sky blue, verbatim. Black text on it is 9.1:1.
        selection_bg: Color32::from_rgb(86, 180, 233),
        // Text on the selection at 7.2:1, and egui's focus ring at 11.4:1 on
        // the face.
        selection_text: Color32::from_rgb(0, 33, 54),
        // A latched toolbar button, pulled most of the way to the selection
        // blue instead of a hair off the face: 2.0:1 against `face`, so the
        // latch is a block of colour you cannot miss, while still carrying
        // black text at 7.1:1. The sunken bevel is the second, non-colour
        // cue on the same control.
        selection_tint: Color32::from_rgb(91, 157, 203),
        // Umber. The lighter half of the warn/error pair — see the
        // colour-blind note above. 7.3:1 on the face.
        warn: Color32::from_rgb(87, 57, 0),
        // Plum-crimson, 1.45:1 darker than `warn` and carrying blue where
        // `warn` has none. 10.5:1 on the face: the loudest ink is also the
        // most legible one.
        error: Color32::from_rgb(78, 7, 34),
        // The bevel, drawn to be seen. The outer ring runs white to
        // near-black — 14.2:1 across one control edge.
        hi_outer: Color32::from_rgb(255, 255, 255),
        hi_inner: Color32::from_rgb(242, 242, 242),
        sh_inner: Color32::from_rgb(154, 154, 154),
        sh_outer: Color32::from_rgb(43, 43, 43),
    },
};
