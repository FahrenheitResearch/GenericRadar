//! Hazard polygon colours.
//!
//! These are not invented here. They are BowEcho's operational hazard colour
//! language -- tornado emergency purple, PDS tornado magenta, destructive
//! severe-thunderstorm deep orange -- reproduced exactly, so an analyst who has
//! both applications open sees one colour language rather than two. A key is a
//! hazard family (`"tornado"`) or a damage-threat escalation subkey
//! (`"tornado/considerable"`).
//!
//! The escalation tiers follow NWS impact-based warning practice: the
//! `TORNADO DAMAGE THREAT`, `THUNDERSTORM DAMAGE THREAT` and `FLASH FLOOD
//! DAMAGE THREAT` bulletin tags (NWS Instruction 10-511, WFO Severe Weather
//! Products Specification) are what promote a warning to its louder colour.

use crate::Rgba8;

/// Hazard family ids, in display order.
pub const HAZARD_FAMILIES: &[&str] = &[
    "tornado",
    "severe-thunderstorm",
    "flash-flood",
    "flood",
    "fire-weather",
    "special-marine",
    "snow-squall",
    "watch",
    "special-weather",
    "other",
];

/// Damage-threat escalation subkeys with distinct colours.
pub const HAZARD_ESCALATIONS: &[&str] = &[
    "tornado/catastrophic",
    "tornado/considerable",
    "severe-thunderstorm/considerable",
    "severe-thunderstorm/destructive",
    "flash-flood/considerable",
    "flash-flood/catastrophic",
    "flood/considerable",
    "flood/catastrophic",
    "watch/tornado",
    "watch/severe-thunderstorm",
];

/// Outline width in screen points for an ordinary hazard.
pub const HAZARD_STROKE_WIDTH: f32 = 1.5;

/// Alpha of a hazard polygon's fill.
///
/// Deliberately low. The warning is context for the radar, not a replacement
/// for it -- an opaque polygon over a hook echo hides the thing the analyst is
/// looking at.
pub const HAZARD_FILL_ALPHA: u8 = 24;

/// Any denser and the fill hides the radar it is drawn over. Checked at
/// compile time, so the ceiling cannot be raised by accident.
const _: () = assert!(HAZARD_FILL_ALPHA <= 48);

/// Stroke colour for a hazard key. An unrecognised key gets the generic
/// yellow rather than vanishing.
pub fn hazard_stroke_color(key: &str) -> Rgba8 {
    let [r, g, b] = match key {
        "tornado" => [248, 62, 82],
        "tornado/catastrophic" => [150, 50, 250],
        "tornado/considerable" => [255, 64, 175],
        "severe-thunderstorm" => [246, 183, 57],
        "severe-thunderstorm/considerable" => [255, 152, 42],
        "severe-thunderstorm/destructive" => [252, 122, 28],
        "flash-flood" => [78, 218, 108],
        "flash-flood/considerable" => [42, 224, 154],
        "flash-flood/catastrophic" => [22, 188, 126],
        "flood" => [76, 190, 124],
        "flood/considerable" => [50, 205, 160],
        "flood/catastrophic" => [24, 160, 130],
        "fire-weather" => [255, 126, 46],
        "special-marine" => [70, 190, 238],
        "snow-squall" => [170, 210, 255],
        "watch" => [235, 92, 245],
        "watch/tornado" => [210, 82, 245],
        "watch/severe-thunderstorm" => [246, 183, 57],
        "special-weather" => [245, 220, 72],
        _ => [232, 232, 96],
    };
    Rgba8 { r, g, b, a: 255 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not "is some colour" -- these are the exact operational values, so a
    /// drift away from BowEcho's language shows up here.
    #[test]
    fn the_operational_colours_are_reproduced_exactly() {
        assert_eq!(
            hazard_stroke_color("tornado"),
            Rgba8 {
                r: 248,
                g: 62,
                b: 82,
                a: 255
            }
        );
        assert_eq!(
            hazard_stroke_color("tornado/catastrophic"),
            Rgba8 {
                r: 150,
                g: 50,
                b: 250,
                a: 255
            },
            "a tornado emergency must be the purple tier"
        );
        assert_eq!(
            hazard_stroke_color("severe-thunderstorm"),
            Rgba8 {
                r: 246,
                g: 183,
                b: 57,
                a: 255
            }
        );
    }

    /// A key that falls through to the generic colour is a hazard an analyst
    /// cannot tell apart from any other, so every declared key must be
    /// distinct from it.
    #[test]
    fn every_declared_key_has_its_own_colour() {
        let generic = hazard_stroke_color("definitely-not-a-key");
        for key in HAZARD_FAMILIES.iter().chain(HAZARD_ESCALATIONS.iter()) {
            if *key == "other" {
                continue;
            }
            assert_ne!(
                hazard_stroke_color(key),
                generic,
                "'{key}' falls through to the generic colour"
            );
        }
    }

    /// The families an analyst must never confuse under pressure.
    #[test]
    fn the_families_that_matter_most_are_mutually_distinct() {
        let keys = [
            "tornado",
            "tornado/considerable",
            "tornado/catastrophic",
            "severe-thunderstorm",
            "severe-thunderstorm/destructive",
            "flash-flood",
            "watch",
        ];
        for (index, first) in keys.iter().enumerate() {
            for second in &keys[index + 1..] {
                assert_ne!(
                    hazard_stroke_color(first),
                    hazard_stroke_color(second),
                    "{first} and {second} paint the same colour"
                );
            }
        }
    }
}
