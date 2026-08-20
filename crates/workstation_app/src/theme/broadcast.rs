//! The broadcast desk: deep blue-slate chrome, wide face steps, bright ink.
//!
//! For a bench that something is pointed at - a studio camera, a projector
//! in a briefing room, or just somebody reading the screen from the other
//! side of the desk. The night bench answers "a dark room"; this one answers
//! "a dark room where the screen is also the picture", and every difference
//! between the two files below follows from that one change of question.
//!
//! # The face ladder, which is the whole idea
//!
//! Measured in CIE L\* (CIE 15:2004, §8.2.1.1 - a perceptual scale, where a
//! difference of one unit means roughly the same amount of "lighter" at
//! every brightness) the five face steps run:
//!
//! ```text
//!              well  pressed   face  raised  hover
//! broadcast     5.8     10.2   17.4    29.5   40.0
//! night bench   9.2     18.0   23.9    27.9   31.3
//! ```
//!
//! The night bench's panel, resting control and hovered control are four
//! and three L\* points apart. That is comfortable at arm's length on a good
//! panel, and it is close to nothing once the frame has been through a
//! camera: video encoders spend their bits on motion and edges and quantise
//! exactly that sort of small flat-area difference away, and a projector in
//! a lit room has less usable contrast again. Broadcast opens the same two
//! gaps to twelve and ten points, so a resting button, a hovered button and
//! the panel behind them are three plainly different greys rather than three
//! shades of one. That is the brief's "unambiguous rather than subtle",
//! written as numbers.
//!
//! The ladder cannot simply be slid upward: the catalog holds body text to
//! 7:1 on `face_raised` (SC 1.4.6, AAA), which caps the raised face around
//! L\* 32 on any dark ground however bright the ink. So the room for a wide
//! ladder is bought below instead, by putting `face` deeper than the night
//! bench's rather than putting the controls higher.
//!
//! # The edges, carried from above
//!
//! `border` is held at 3.95:1 on `face`, where a resting outline usually
//! gets about two - every control has an edge that is visible before anyone
//! hovers it - and `border_strong`, which is the entire affordance in
//! [`super::ChromeEdges::Flat`], runs 7.39:1 on `face` and 9.66:1 on `well`.
//!
//! The two-line relief (Win95's raised / etched / sunken grammar - "The
//! Windows Interface Guidelines for Software Design", Microsoft Press, 1995,
//! ch. 13) is deliberately lopsided here. `hi_outer` sits +42 L\* above the
//! face, more than twice the night bench's +19.5 and the strongest highlight
//! in the catalog, while the shade rings are shallower than the night
//! bench's at -9.6 and -12.3. That is not an oversight and it is not
//! laziness about the dark side: `sh_outer` is already on this file's floor,
//! and more to the point a shade edge is the first thing a camera's black
//! level and a projector's black level destroy. Under a lens the lit ring is
//! the only half of the relief that reliably survives, so this theme spends
//! its contrast there.
//!
//! # Nothing at 0 or 255
//!
//! Every channel of every token below lies in 16 ..= 235 - the nominal black
//! and white levels of 8-bit narrow-range video (ITU-R BT.709-6, "Parameter
//! values for the HDTV standards for production and international programme
//! exchange", ITU-R, 2015, Table 3; the tolerance around them is EBU R 103,
//! "Video Signal Tolerance in Digital Television Systems", EBU, 2016). Pure
//! white and pure black are the two values a video path clips, and clipping
//! is what turns crisp small type into a bloomed smear and a bevel into a
//! hole. Holding the range costs a little headroom at both ends and buys a
//! chrome that looks the same on the panel and on the feed.
//!
//! It also decides the inks. A saturated red is illegible as TEXT on a dark
//! ground - it misses SC 1.4.3 outright - and saturated red is separately
//! the worst case for 4:2:0 chroma subsampling, so `error` is a vermilion
//! lifted to 4.93:1 on `face`, and `warn` is a gold far enough away in hue
//! that the two are told apart at a glance rather than by reading them.
//!
//! # What this theme does not do
//!
//! It does not touch the type ramp, despite "bigger type" being the obvious
//! thing to want here. Font sizes belong to the application (pinned by
//! `tests/theme_contract.rs`) and the axis that enlarges them is
//! [`super::UiScale`], which grows the glyphs, the padding, the hit targets
//! and the hairlines together - where a theme growing only its own fonts
//! would leave the chrome around them the size it was. This palette is drawn
//! to hold up at 125 % and 140 %, and an on-air desk should run it there.
//!
//! It does not tint the data. Radar panes draw on the map's own colours
//! (`map_scene::MapChrome`); a theme that made echo more televisual would be
//! a theme that lies about dBZ.
//!
//! # The margins
//!
//! Every value below is measured by `tests/theme_catalog.rs` against every
//! registered accent. The three tightest pairings in the theme are selected
//! text on its own selection (4.86:1), body text on that selection (4.92:1)
//! and `error` on `face` (4.93:1), all against a 4.5:1 floor. Anything that
//! darkens `face` or deepens `selection_bg` spends those three first.

use eframe::egui::Color32;

use super::{Ground, Palette, ThemeSpec};

pub const THEME: ThemeSpec = ThemeSpec {
    id: "broadcast",
    label: "Broadcast desk",
    description: "Deep chrome, wide gaps between the control states, heavy \
                  edges. For a bench that is filmed, projected, or read from \
                  across the room.",
    ground: Ground::Dark,
    palette: Palette {
        // The ladder: L* 17.4 for the panel, +12.1 to a resting control,
        // +10.5 again to a hovered one, -7.2 to a pressed one. Blue-slate
        // rather than the night bench's neutral graphite, so the two are
        // not mistaken for each other in a settings list.
        face: Color32::from_rgb(36, 43, 55),
        face_raised: Color32::from_rgb(61, 70, 85),
        face_pressed: Color32::from_rgb(24, 28, 36),
        hover: Color32::from_rgb(84, 95, 112),
        // Data areas sit behind the instrument, as they must on a dark
        // ground - and one step above the video-black floor, not on it.
        well: Color32::from_rgb(16, 19, 25),

        // Ink. Studio white, not paper white: 235 is the top of the range
        // this file works in. 11.62 / 7.78 / 15.19:1 on face, raised face
        // and well.
        text: Color32::from_rgb(228, 233, 235),
        text_weak: Color32::from_rgb(176, 186, 198),
        // Disabled is faint on purpose (2.02:1 on the raised face) and a
        // long way under live secondary text on the same ground (4.84:1),
        // because a greyed control must not read as a live one from the
        // back of the room either.
        text_disabled: Color32::from_rgb(108, 116, 128),

        // Edges, loud on purpose: 3.95:1 at rest, 7.39:1 when the border
        // IS the affordance.
        border: Color32::from_rgb(124, 136, 152),
        border_strong: Color32::from_rgb(176, 188, 204),

        // One saturated azure, and it is the only chroma in the chrome, so
        // it never has to shout to be found: 5.52:1 on face, 7.21:1 on well.
        link: Color32::from_rgb(60, 170, 235),
        selection_bg: Color32::from_rgb(16, 100, 178),
        selection_text: Color32::from_rgb(222, 233, 235),
        // A latched toolbar toggle is +12.4 L* over the face AND carries a
        // hue nothing else in the chrome has: on at a glance, across a room,
        // through a lens.
        selection_tint: Color32::from_rgb(24, 72, 118),

        // Gold and vermilion - far enough apart in hue to be told apart
        // rather than read.
        warn: Color32::from_rgb(235, 186, 66),
        error: Color32::from_rgb(235, 118, 90),

        // The relief. The lit rings are the strongest in the catalog
        // (+42.0 and +19.8 L*); the shade rings are -9.6 and -12.3, the
        // outer one already sitting on this file's floor of 16.
        hi_outer: Color32::from_rgb(132, 144, 162),
        hi_inner: Color32::from_rgb(78, 88, 104),
        sh_inner: Color32::from_rgb(20, 23, 30),
        sh_outer: Color32::from_rgb(16, 17, 20),
    },
};
