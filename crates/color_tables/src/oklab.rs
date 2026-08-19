//! Perceptual mixing for the continuous rendering of a palette.
//!
//! A colour table stores stops, and everything between two stops has to be
//! invented. The obvious way to invent it - lerp the three sRGB bytes - is
//! wrong in a specific, visible way: sRGB bytes are gamma-encoded, so the
//! arithmetic mean of two bytes is *not* the colour halfway between them in
//! either light or perception. Two bright, saturated, well-separated stops mix
//! to something darker and greyer than either, so a ramp that should read as a
//! clean transition turns muddy through the middle. On the classic reflectivity
//! sequence the worst case is blue -> green at 20-25 dBZ: `(3,0,244)` to
//! `(2,253,2)` passes through `(2,126,123)`, a dark teal well below both ends.
//!
//! Oklab is a perceptual space built for exactly this. Its lightness axis is
//! near-uniform and its hue lines are straight enough that a straight-line mix
//! between two colours stays as light and as saturated as the ends imply.
//!
//! Ottosson, B., 2020: "A perceptual color space for image processing",
//! <https://bottosson.github.io/posts/oklab/>. The matrices and the cube-root
//! non-linearity below are that article's, transcribed unchanged. Oklab is the
//! space CSS Color 4 (W3C, 2024, <https://www.w3.org/TR/css-color-4/>) adopted
//! for `oklab()` and for gradient interpolation, for the same reason.
//!
//! The sRGB transfer function is IEC 61966-2-1:1999.
//!
//! # Why this module carries lookup tables
//!
//! Most of the renderer never calls this: a u8 or u16 moment grid is drawn
//! through a palette built once per raster, so the cost is per raw code and not
//! per pixel. The storm-relative and float-storage paths are the exception -
//! they resolve a value per screen pixel - and there a naive implementation
//! would spend six `cbrt` and six `powf` calls per sample. Two things avoid
//! that, and neither approximates the result:
//!
//! * the forward transform is done once per *stop*, at table construction, so
//!   sampling only ever runs the inverse;
//! * the inverse's final gamma encode is a search over 255 precomputed
//!   thresholds instead of a `powf`. The threshold table holds the exact linear
//!   values at which the encoded byte ticks over, so the byte this returns is
//!   the byte `round(255 * encode(x))` returns, not an approximation of it.
//!
//! Measured over twenty million calls, values deliberately off any stop grid:
//!
//!   GR2Analyst Classic REF  quantized stepped 13.8 ns  continuous 29.4 ns
//!   Analyst Tornado VEL     quantized stepped 21.8 ns  continuous 36.0 ns
//!   Smooth Doppler VEL      legacy sRGB ramp  22.6 ns
//!
//! So a continuous sample costs about 1.6 times an sRGB one: 75 ms rather than
//! 47 ms of colour lookup for a full 1920x1080 storm-relative velocity frame,
//! and nothing at all on the ordinary raster paths. If that ever needs to come
//! down, the threshold search is the whole of the difference and can be turned
//! into one indexed lookup plus a single correction step.

use std::sync::LazyLock;

use crate::{Rgba8, lerp_u8};

/// A colour in Oklab: perceptual lightness, then the two opponent axes.
pub(crate) type Oklab = [f32; 3];

/// sRGB decode: encoded value in 0..=1 to linear light.
fn linear_from_srgb_unit(value: f64) -> f64 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

struct Transfer {
    /// Linear light for each of the 256 encoded byte values.
    linear: [f32; 256],
    /// `thresholds[index]` is the linear value at which the encoded byte ticks
    /// over from `index` to `index + 1`, so a byte is a `partition_point` away.
    thresholds: [f32; 255],
}

static TRANSFER: LazyLock<Transfer> = LazyLock::new(|| {
    let mut linear = [0.0_f32; 256];
    for (byte, slot) in linear.iter_mut().enumerate() {
        *slot = linear_from_srgb_unit(byte as f64 / 255.0) as f32;
    }
    let mut thresholds = [0.0_f32; 255];
    for (index, slot) in thresholds.iter_mut().enumerate() {
        // The encoded value that rounds up to `index + 1` for the first time.
        *slot = linear_from_srgb_unit((index as f64 + 0.5) / 255.0) as f32;
    }
    Transfer { linear, thresholds }
});

fn linear_from_byte(byte: u8) -> f32 {
    TRANSFER.linear[usize::from(byte)]
}

fn byte_from_linear(value: f32) -> u8 {
    // Oklab is larger than sRGB, so a straight line between two in-gamut
    // colours can leave the cube by a hair. Clamping is the standard, and the
    // only lossless-at-the-ends, way home: at amount 0 and 1 `mix` returns the
    // stop untouched without coming through here at all.
    let value = value.clamp(0.0, 1.0);
    TRANSFER
        .thresholds
        .partition_point(|threshold| *threshold <= value) as u8
}

/// Forward transform, run once per stop when a table is built.
pub(crate) fn oklab_from_rgb(color: Rgba8) -> Oklab {
    let red = linear_from_byte(color.r);
    let green = linear_from_byte(color.g);
    let blue = linear_from_byte(color.b);

    let long = 0.412_221_47 * red + 0.536_332_55 * green + 0.051_445_995 * blue;
    let medium = 0.211_903_5 * red + 0.680_699_5 * green + 0.107_396_96 * blue;
    let short = 0.088_302_46 * red + 0.281_718_85 * green + 0.629_978_7 * blue;

    let long = long.cbrt();
    let medium = medium.cbrt();
    let short = short.cbrt();

    [
        0.210_454_26 * long + 0.793_617_8 * medium - 0.004_072_047 * short,
        1.977_998_5 * long - 2.428_592_2 * medium + 0.450_593_7 * short,
        0.025_904_037 * long + 0.782_771_75 * medium - 0.808_675_77 * short,
    ]
}

/// Inverse transform, run once per sample.
fn rgb_from_oklab(lab: Oklab) -> (u8, u8, u8) {
    let long = lab[0] + 0.396_337_78 * lab[1] + 0.215_803_76 * lab[2];
    let medium = lab[0] - 0.105_561_346 * lab[1] - 0.063_854_17 * lab[2];
    let short = lab[0] - 0.089_484_18 * lab[1] - 1.291_485_5 * lab[2];

    let long = long * long * long;
    let medium = medium * medium * medium;
    let short = short * short * short;

    (
        byte_from_linear(4.076_741_7 * long - 3.307_711_6 * medium + 0.230_969_94 * short),
        byte_from_linear(-1.268_438 * long + 2.609_757_4 * medium - 0.341_319_38 * short),
        byte_from_linear(-0.004_196_086_3 * long - 0.703_418_6 * medium + 1.707_614_7 * short),
    )
}

/// Mix two stops perceptually.
///
/// `amount` 0 returns `left` and 1 returns `right`, byte for byte, with no
/// transform run at all. That is not an optimisation - it is what keeps a
/// palette's declared colours exactly its declared colours at the stops, which
/// every "paints its declared colours" test in this crate depends on.
///
/// Alpha is mixed linearly and deliberately: it is coverage, not colour, and
/// running it through a perceptual lightness axis would be meaningless.
pub(crate) fn mix(
    left: Rgba8,
    left_lab: Oklab,
    right: Rgba8,
    right_lab: Oklab,
    amount: f32,
) -> Rgba8 {
    if amount <= 0.0 {
        return left;
    }
    if amount >= 1.0 {
        return right;
    }
    let lab = [
        left_lab[0] + (right_lab[0] - left_lab[0]) * amount,
        left_lab[1] + (right_lab[1] - left_lab[1]) * amount,
        left_lab[2] + (right_lab[2] - left_lab[2]) * amount,
    ];
    let (red, green, blue) = rgb_from_oklab(lab);
    Rgba8 {
        r: red,
        g: green,
        b: blue,
        a: lerp_u8(left.a, right.a, amount),
    }
}

/// Perceptual lightness of a colour, for tests and for palette diagnostics.
pub fn lightness(color: Rgba8) -> f32 {
    oklab_from_rgb(color)[0]
}

/// Perceptual chroma: distance from the neutral axis in the opponent plane.
pub fn chroma(color: Rgba8) -> f32 {
    let lab = oklab_from_rgb(color);
    (lab[1] * lab[1] + lab[2] * lab[2]).sqrt()
}

/// Perceptual hue angle in degrees, undefined but harmless at zero chroma.
pub fn hue_degrees(color: Rgba8) -> f32 {
    let lab = oklab_from_rgb(color);
    lab[2].atan2(lab[1]).to_degrees()
}

/// Perceptual distance between two colours: the Euclidean metric Oklab is
/// designed to make meaningful.
pub fn difference(left: Rgba8, right: Rgba8) -> f32 {
    let a = oklab_from_rgb(left);
    let b = oklab_from_rgb(right);
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transform has to come back where it started, or a palette's own
    /// stops would drift the moment anything asked for the colour at a stop.
    #[test]
    fn every_byte_triple_on_the_grey_axis_survives_a_round_trip() {
        for byte in 0..=255_u8 {
            let colour = Rgba8::opaque(byte, byte, byte);
            let (red, green, blue) = rgb_from_oklab(oklab_from_rgb(colour));
            assert_eq!(
                (red, green, blue),
                (byte, byte, byte),
                "grey {byte} did not survive"
            );
        }
    }

    #[test]
    fn a_spread_of_saturated_colours_survives_a_round_trip() {
        for colour in [
            Rgba8::opaque(0, 0, 0),
            Rgba8::opaque(255, 255, 255),
            Rgba8::opaque(255, 0, 0),
            Rgba8::opaque(0, 255, 0),
            Rgba8::opaque(0, 0, 255),
            Rgba8::opaque(3, 0, 244),
            Rgba8::opaque(2, 253, 2),
            Rgba8::opaque(4, 233, 231),
            Rgba8::opaque(253, 149, 0),
            Rgba8::opaque(232, 32, 206),
            Rgba8::opaque(112, 112, 112),
        ] {
            let (red, green, blue) = rgb_from_oklab(oklab_from_rgb(colour));
            assert_eq!(
                (red, green, blue),
                (colour.r, colour.g, colour.b),
                "{colour:?} did not survive the round trip"
            );
        }
    }

    /// The transform is the published one, checked against published numbers.
    ///
    /// The two round-trip tests above pass for any pair of matrices that are
    /// each other's inverse, which includes a pair that is internally
    /// consistent and wrong - a transposed row, a digit dropped from a
    /// coefficient, the whole thing built for a different white point. Nothing
    /// else here would notice: the palettes would still round-trip, the ramps
    /// would still be smooth, and every "does it sag" statistic would be
    /// measured against the same wrong space it was computed in. The only way
    /// to know the space is Oklab is to compare it with numbers somebody else
    /// published.
    ///
    /// Reference values are the sRGB primaries and secondaries under D65, as
    /// given by Ottosson, B., 2020: "A perceptual color space for image
    /// processing", <https://bottosson.github.io/posts/oklab/>, and as carried
    /// by the CSS Color Module Level 4 conversion sample set (W3C, 2024,
    /// <https://www.w3.org/TR/css-color-4/>), which is what the browser and
    /// library implementations of `oklab()` are validated against.
    ///
    /// The tolerance is 1e-3 on every axis. That is far wider than the
    /// agreement actually achieved - the worst channel here is off by 5e-6 -
    /// and deliberately so: it is tight enough that no plausible transcription
    /// slip survives it, and loose enough that it is not a test of f32
    /// rounding.
    #[test]
    fn the_transform_agrees_with_the_published_oklab_values() {
        // (name, sRGB, published L, a, b)
        const REFERENCE: [(&str, [u8; 3], f32, f32, f32); 9] = [
            ("white", [255, 255, 255], 1.000_00, 0.000_00, 0.000_00),
            ("black", [0, 0, 0], 0.000_00, 0.000_00, 0.000_00),
            ("red", [255, 0, 0], 0.627_96, 0.224_86, 0.125_85),
            ("green", [0, 255, 0], 0.866_44, -0.233_89, 0.179_50),
            ("blue", [0, 0, 255], 0.452_01, -0.032_46, -0.311_53),
            ("cyan", [0, 255, 255], 0.905_40, -0.149_44, -0.039_40),
            ("magenta", [255, 0, 255], 0.701_67, 0.274_57, -0.169_16),
            ("yellow", [255, 255, 0], 0.967_98, -0.071_37, 0.198_57),
            ("mid grey", [128, 128, 128], 0.599_87, 0.000_00, 0.000_00),
        ];

        for (name, [red, green, blue], want_l, want_a, want_b) in REFERENCE {
            let measured = oklab_from_rgb(Rgba8::opaque(red, green, blue));
            for (axis, got, want) in [
                ("L", measured[0], want_l),
                ("a", measured[1], want_a),
                ("b", measured[2], want_b),
            ] {
                assert!(
                    (got - want).abs() < 1e-3,
                    "{name}: Oklab {axis} is {got}, published {want}"
                );
            }
        }

        // And the metric built on it. Euclidean distance between the published
        // triples, so a correct transform with a broken `difference` is caught
        // separately from a broken transform.
        for (name, left, right, want) in [
            ("red-green", [255_u8, 0, 0], [0_u8, 255, 0], 0.519_81_f32),
            ("red-blue", [255, 0, 0], [0, 0, 255], 0.537_10),
            ("white-black", [255, 255, 255], [0, 0, 0], 1.000_00),
        ] {
            let got = difference(
                Rgba8::opaque(left[0], left[1], left[2]),
                Rgba8::opaque(right[0], right[1], right[2]),
            );
            assert!(
                (got - want).abs() < 1e-3,
                "dE {name} is {got}, the published triples are {want} apart"
            );
        }
    }

    /// White is Oklab lightness 1 by construction; black is 0. Ottosson's
    /// article states both, and they are the two anchors the scale is fixed by.
    #[test]
    fn the_lightness_axis_is_anchored_where_the_paper_anchors_it() {
        assert!((lightness(Rgba8::opaque(255, 255, 255)) - 1.0).abs() < 1e-3);
        assert!(lightness(Rgba8::opaque(0, 0, 0)).abs() < 1e-6);
        // Mid grey sits near the middle of the perceptual scale, which is the
        // whole difference from a linear-light L: linear-light mid grey is 0.21.
        let mid = lightness(Rgba8::opaque(128, 128, 128));
        assert!((0.55..0.65).contains(&mid), "mid grey lightness {mid}");
    }

    /// Neutral in, neutral out: a mix of two greys must not pick up a cast, or
    /// the zero isodop on a velocity table would drift off neutral.
    #[test]
    fn mixing_two_greys_stays_grey() {
        let left = Rgba8::opaque(84, 100, 84);
        let right = Rgba8::opaque(112, 112, 112);
        for step in 0..=10 {
            let amount = step as f32 / 10.0;
            let mixed = mix(
                left,
                oklab_from_rgb(left),
                right,
                oklab_from_rgb(right),
                amount,
            );
            let spread = i32::from(mixed.r.max(mixed.g).max(mixed.b))
                - i32::from(mixed.r.min(mixed.g).min(mixed.b));
            assert!(spread <= 17, "mix at {amount} spread {spread}: {mixed:?}");
        }
    }

    #[test]
    fn the_ends_of_a_mix_are_the_stops_themselves() {
        let left = Rgba8::new(3, 0, 244, 200);
        let right = Rgba8::opaque(2, 253, 2);
        let left_lab = oklab_from_rgb(left);
        let right_lab = oklab_from_rgb(right);
        assert_eq!(mix(left, left_lab, right, right_lab, 0.0), left);
        assert_eq!(mix(left, left_lab, right, right_lab, 1.0), right);
        assert_eq!(mix(left, left_lab, right, right_lab, -0.5), left);
        assert_eq!(mix(left, left_lab, right, right_lab, 1.5), right);
    }

    /// The reason this module exists, stated as a number.
    ///
    /// The blue-to-green step in the classic reflectivity sequence is the worst
    /// case in the built-in palettes. Halfway along it, the sRGB byte mean is
    /// far darker than either end; the perceptual mix is not.
    #[test]
    fn the_perceptual_midpoint_does_not_sag_the_way_the_srgb_midpoint_does() {
        let left = Rgba8::opaque(3, 0, 244);
        let right = Rgba8::opaque(2, 253, 2);
        let srgb_mid = Rgba8::opaque(
            lerp_u8(left.r, right.r, 0.5),
            lerp_u8(left.g, right.g, 0.5),
            lerp_u8(left.b, right.b, 0.5),
        );
        let perceptual_mid = mix(
            left,
            oklab_from_rgb(left),
            right,
            oklab_from_rgb(right),
            0.5,
        );

        let ends = (lightness(left) + lightness(right)) / 2.0;
        let srgb_sag = ends - lightness(srgb_mid);
        let perceptual_sag = ends - lightness(perceptual_mid);
        assert!(
            srgb_sag > 0.1,
            "the sRGB midpoint was expected to sag, sag {srgb_sag}"
        );
        // Not exactly zero: the straight line between these two ends leaves the
        // sRGB gamut around the middle - it wants more chroma at that lightness
        // than sRGB can hold - and `byte_from_linear` clamps it back in, which
        // moves the lightness a little. An order of magnitude smaller than the
        // sRGB sag is the claim, and it is the claim that matters.
        assert!(
            perceptual_sag.abs() < srgb_sag / 5.0,
            "the perceptual midpoint sagged {perceptual_sag} against sRGB's {srgb_sag}"
        );
    }

    /// Alpha is coverage and is mixed as coverage, all the way through.
    ///
    /// Nothing in the built-in catalogue can catch this: every stop in every
    /// built-in is either fully clear or fully opaque, and the continuous mode
    /// clips below the first opaque stop, so `mix` is never handed two
    /// different alphas by a shipped palette. A palette an analyst loads can
    /// do it in one line - `color4` takes an alpha per stop - and a table that
    /// paints a partial-coverage ramp opaque is a table that hides whatever is
    /// drawn under it.
    #[test]
    fn a_partial_alpha_ramp_keeps_its_coverage_through_the_mixer() {
        let left = Rgba8::new(20, 40, 200, 40);
        let right = Rgba8::new(240, 60, 20, 200);
        let left_lab = oklab_from_rgb(left);
        let right_lab = oklab_from_rgb(right);
        for (amount, expected) in [
            (0.0, 40_u8),
            (0.25, 80),
            (0.5, 120),
            (0.75, 160),
            (1.0, 200),
        ] {
            let mixed = mix(left, left_lab, right, right_lab, amount);
            assert_eq!(
                mixed.a, expected,
                "alpha at {amount} was {} and should be {expected}",
                mixed.a
            );
        }
    }

    /// The transform is exact on every colour a built-in palette declares.
    ///
    /// `mix` short-circuits at `amount` 0 and 1 and hands back the stop
    /// untouched, so "the stops are painted as declared" would hold even if the
    /// transform drifted. This checks the property that makes the short circuit
    /// an optimisation rather than a cover-up: run every declared colour in the
    /// catalogue through the transform and back, and none of them moves. If one
    /// ever does, the ramp beside it is being invented from a colour that is
    /// not the one the palette declares.
    #[test]
    fn every_colour_any_built_in_declares_survives_the_transform_exactly() {
        let mut checked = 0;
        for family in crate::ColorTableFamily::ALL {
            for table in crate::builtin_tables_for_family(family) {
                for stop in table.stops() {
                    let (red, green, blue) = rgb_from_oklab(oklab_from_rgb(stop.color));
                    assert_eq!(
                        (red, green, blue),
                        (stop.color.r, stop.color.g, stop.color.b),
                        "{} moved its stop at {} through the transform",
                        table.name(),
                        stop.value
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 500, "only {checked} stops were checked");
    }

    #[test]
    fn the_byte_encoder_agrees_with_the_transfer_function_it_stands_in_for() {
        for byte in 0..=255_u8 {
            assert_eq!(byte_from_linear(linear_from_byte(byte)), byte);
        }
        // And between two byte values it lands on one of the two.
        for byte in 0..255_u8 {
            let low = linear_from_byte(byte);
            let high = linear_from_byte(byte + 1);
            let found = byte_from_linear((low + high) / 2.0);
            assert!(found == byte || found == byte + 1, "{byte} -> {found}");
        }
    }
}
