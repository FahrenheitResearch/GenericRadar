//! Layer styling and its generation.
//!
//! Style is a geometry input, not a paint-time decision: line width in screen
//! pixels decides how a line is expanded into triangles, and layer visibility
//! decides whether it is built at all. A style change therefore invalidates
//! retained geometry, which is why the style generation is part of the
//! geometry cache key.
//!
//! The named looks a user picks between live in [`crate::style_presets`]; this
//! module holds the shape they are all built out of. Two of those pieces exist
//! so that a preset cannot get the scale bands wrong by accident: [`MapInk`]
//! carries only colour and width, [`ScaleBands`] carries only the two
//! thresholds, and [`MapStyle::from_ink`] is the one place that decides which
//! layer gets which band.

use crate::dataset::MapLayer;

/// RGBA, straight alpha, sRGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl LayerColor {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Build from 8-bit sRGB channels, straight alpha.
    ///
    /// Presets are transcribed from palettes that are written in bytes (the
    /// console's `#0a0a0a` ramp, `console_basemap::console_strokes`), so the
    /// byte is what the source says. Dividing here instead of pasting a
    /// rounded decimal removes the transcription error entirely.
    pub const fn from_rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }

    /// Opaque 8-bit sRGB.
    pub const fn from_rgb8(r: u8, g: u8, b: u8) -> Self {
        Self::from_rgba8(r, g, b, 255)
    }

    /// Back to 8-bit channels, for the egui painter that draws the pane
    /// background and the place labels. Out-of-range components are clamped
    /// rather than wrapped, because a wrapped channel would silently paint the
    /// complement of the intended colour.
    pub fn to_rgba8(self) -> [u8; 4] {
        fn channel(value: f32) -> u8 {
            // Total, including for a NaN component: the bounds are literals so
            // `clamp` cannot panic, NaN propagates through it, and a
            // float-to-integer `as` cast saturates with NaN going to zero (RFC
            // 3013, stabilised in Rust 1.45), so a poisoned channel paints
            // black rather than an arbitrary byte.
            (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        }
        [
            channel(self.r),
            channel(self.g),
            channel(self.b),
            channel(self.a),
        ]
    }

    /// Relative luminance, ITU-R BT.709 coefficients (Rec. ITU-R BT.709-6,
    /// 2015, item 3.2), applied to the sRGB components directly.
    ///
    /// This is the perceptual-weight approximation used to answer one question
    /// only: is this ink lighter or darker than the ground it lands on. It is
    /// deliberately *not* gamma-linearised, because the comparison is between
    /// two colours in the same encoding and the ordering is what matters.
    pub fn luminance(self) -> f32 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }

    /// This colour composited over `ground` with straight alpha, then its
    /// luminance. What the eye actually receives, which is what decides
    /// whether a translucent line is visible at all.
    pub fn composite_luminance_over(self, ground: Self) -> f32 {
        let alpha = self.a.clamp(0.0, 1.0);
        alpha * self.luminance() + (1.0 - alpha) * ground.luminance()
    }

    /// Every component finite and inside `0.0..=1.0`.
    ///
    /// A negative or `NaN` channel reaches the shader as an undefined colour
    /// and an alpha above one saturates the blend, so a style is checked once
    /// here rather than debugged later on the GPU.
    pub fn is_in_range(self) -> bool {
        [self.r, self.g, self.b, self.a]
            .into_iter()
            .all(|component| component.is_finite() && (0.0..=1.0).contains(&component))
    }
}

/// Scale at or below which counties replace state outlines.
pub const COUNTY_DETAIL_KM_PER_POINT: f32 = 1.2;
/// Scale above which only country outlines remain.
pub const COUNTRY_ONLY_KM_PER_POINT: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerStyle {
    pub color: LayerColor,
    /// Stroke width in screen pixels, held constant as the camera zooms.
    pub width_px: f32,
    /// Finest scale, in kilometres per point, at which the layer is drawn.
    /// Coarser US levels switch off once a finer one covers the same ground,
    /// which is what stops one shoreline being drawn twice.
    pub min_km_per_point: f32,
    /// Coarsest scale at which the layer is drawn.
    pub max_km_per_point: f32,
}

impl LayerStyle {
    /// Whether this layer can be drawn at all.
    ///
    /// Checks the three ways a hand-written style breaks the builder: a width
    /// that is not a positive finite number of pixels (the line expansion
    /// multiplies by it), a colour component outside `0.0..=1.0`, and an
    /// inverted or negative scale band, which would make the layer either
    /// never visible or visible everywhere.
    pub fn is_well_formed(self) -> bool {
        self.width_px.is_finite()
            && self.width_px > 0.0
            && self.color.is_in_range()
            && self.min_km_per_point.is_finite()
            && self.min_km_per_point >= 0.0
            && self.max_km_per_point.is_finite()
            && self.max_km_per_point > self.min_km_per_point
    }
}

/// Colour and width for one layer, with no opinion about when it is drawn.
///
/// A preset supplies ink; the scale bands come from [`ScaleBands`]. Splitting
/// them is the whole point: restyling the map must not be able to change *when*
/// a layer appears.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerInk {
    pub color: LayerColor,
    /// Stroke width in screen pixels, held constant as the camera zooms.
    pub width_px: f32,
}

impl LayerInk {
    pub const fn new(color: LayerColor, width_px: f32) -> Self {
        Self { color, width_px }
    }
}

/// Ink for all four layers. Named fields, because four arguments of the same
/// type in a row is exactly how a country stroke ends up on the county lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapInk {
    pub country: LayerInk,
    pub foreign_admin: LayerInk,
    pub state: LayerInk,
    pub county: LayerInk,
}

/// The two thresholds that decide which US boundary level is drawn.
///
/// They partition every camera scale into three half-open bands -
/// `(0, county_detail]`, `(county_detail, country_only]`,
/// `(country_only, MAX]` - so exactly one US level is ever built. See
/// [`MapStyle::is_visible`] for the comparison and
/// `only_one_us_level_is_visible_at_any_scale` for the guarantee.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaleBands {
    /// Scale at or below which counties replace state outlines.
    pub county_detail_km_per_point: f32,
    /// Scale above which only country outlines remain.
    pub country_only_km_per_point: f32,
}

impl ScaleBands {
    /// The shipped thresholds. A preset uses these unless it has a stated
    /// reason not to.
    pub const DEFAULT: Self = Self {
        county_detail_km_per_point: COUNTY_DETAIL_KM_PER_POINT,
        country_only_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
    };

    /// Both thresholds finite and strictly increasing from zero, which is what
    /// makes the three bands non-empty and non-overlapping.
    pub fn is_well_formed(self) -> bool {
        self.county_detail_km_per_point.is_finite()
            && self.country_only_km_per_point.is_finite()
            && self.county_detail_km_per_point > 0.0
            && self.country_only_km_per_point > self.county_detail_km_per_point
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapStyle {
    pub country: LayerStyle,
    pub foreign_admin: LayerStyle,
    pub state: LayerStyle,
    pub county: LayerStyle,
}

impl Default for MapStyle {
    fn default() -> Self {
        Self {
            // Country outlines take over only once state lines stop.
            country: LayerStyle {
                color: LayerColor::rgba(0.62, 0.68, 0.78, 0.95),
                width_px: 1.6,
                min_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
                max_km_per_point: f32::MAX,
            },
            // Foreign provinces have no finer level to collide with, so they
            // stay on wherever they would be legible.
            foreign_admin: LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                min_km_per_point: 0.0,
                max_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
            },
            state: LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                min_km_per_point: COUNTY_DETAIL_KM_PER_POINT,
                max_km_per_point: COUNTRY_ONLY_KM_PER_POINT,
            },
            county: LayerStyle {
                color: LayerColor::rgba(0.42, 0.47, 0.55, 0.85),
                width_px: 0.9,
                min_km_per_point: 0.0,
                max_km_per_point: COUNTY_DETAIL_KM_PER_POINT,
            },
        }
    }
}

impl MapStyle {
    /// Assemble a style from ink plus bands.
    ///
    /// This is the only place that decides which layer gets which threshold,
    /// so every preset built through it inherits the visibility guarantee
    /// instead of re-deriving it. The assignment, and the reasoning, are the
    /// same as [`MapStyle::default`]:
    ///
    /// - country takes over only where state lines stop,
    /// - foreign provinces have no finer level to collide with, so they run
    ///   from the closest zoom up to the country-only threshold,
    /// - state occupies the middle band,
    /// - county occupies the closest band.
    pub const fn from_ink(ink: MapInk, bands: ScaleBands) -> Self {
        Self {
            country: LayerStyle {
                color: ink.country.color,
                width_px: ink.country.width_px,
                min_km_per_point: bands.country_only_km_per_point,
                max_km_per_point: f32::MAX,
            },
            foreign_admin: LayerStyle {
                color: ink.foreign_admin.color,
                width_px: ink.foreign_admin.width_px,
                min_km_per_point: 0.0,
                max_km_per_point: bands.country_only_km_per_point,
            },
            state: LayerStyle {
                color: ink.state.color,
                width_px: ink.state.width_px,
                min_km_per_point: bands.county_detail_km_per_point,
                max_km_per_point: bands.country_only_km_per_point,
            },
            county: LayerStyle {
                color: ink.county.color,
                width_px: ink.county.width_px,
                min_km_per_point: 0.0,
                max_km_per_point: bands.county_detail_km_per_point,
            },
        }
    }

    /// The thresholds this style is actually using, read back off the layers.
    ///
    /// A caller comparing a style against [`ScaleBands::DEFAULT`] is asking
    /// "did this look change *when* things draw, or only how they look", and
    /// that question has to be answerable from a `MapStyle` on its own -
    /// including one that was not built by [`Self::from_ink`].
    pub fn bands(&self) -> ScaleBands {
        ScaleBands {
            county_detail_km_per_point: self.county.max_km_per_point,
            country_only_km_per_point: self.state.max_km_per_point,
        }
    }

    /// Every layer drawable and the US levels partitioning the scale axis.
    ///
    /// The partition is the invariant that stops two generalisations of the
    /// same shoreline being drawn a pixel apart; see `MapLayer`'s own note.
    pub fn is_well_formed(&self) -> bool {
        let layers_ok = MapLayer::ALL
            .into_iter()
            .all(|layer| self.layer(layer).is_well_formed());
        let bands = self.bands();
        // County starts at the closest zoom, state resumes exactly where
        // county stops, country exactly where state stops, and country runs
        // out to the coarsest scale representable.
        let partitioned = self.county.min_km_per_point == 0.0
            && self.state.min_km_per_point == self.county.max_km_per_point
            && self.country.min_km_per_point == self.state.max_km_per_point
            && self.country.max_km_per_point == f32::MAX
            // Foreign admin has no county-level counterpart, so it spans
            // everything below the country-only threshold in one band.
            && self.foreign_admin.min_km_per_point == 0.0
            && self.foreign_admin.max_km_per_point == bands.country_only_km_per_point;
        layers_ok && bands.is_well_formed() && partitioned
    }

    pub fn layer(&self, layer: MapLayer) -> LayerStyle {
        match layer {
            MapLayer::Country => self.country,
            MapLayer::ForeignAdmin => self.foreign_admin,
            MapLayer::State => self.state,
            MapLayer::County => self.county,
        }
    }

    /// Whether the layer is drawn at this camera scale.
    pub fn is_visible(&self, layer: MapLayer, km_per_point: f32) -> bool {
        let style = self.layer(layer);
        km_per_point <= style.max_km_per_point && km_per_point > style.min_km_per_point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly one US boundary level may be visible at a time. Two of them
    /// together draw the same shoreline twice, a pixel apart, which is what
    /// made the map look like several overlapping basemaps.
    #[test]
    fn only_one_us_level_is_visible_at_any_scale() {
        let style = MapStyle::default();
        for km_per_point in [
            0.05_f32, 0.2, 0.35, 1.0, 1.19, 1.21, 3.0, 7.9, 8.1, 40.0, 500.0,
        ] {
            let us_levels = [MapLayer::County, MapLayer::State, MapLayer::Country]
                .into_iter()
                .filter(|layer| style.is_visible(*layer, km_per_point))
                .count();
            assert_eq!(
                us_levels, 1,
                "{us_levels} US boundary levels drawn at {km_per_point} km/point"
            );
        }
    }

    #[test]
    fn detail_decreases_as_the_camera_pulls_back() {
        let style = MapStyle::default();
        assert!(
            style.is_visible(MapLayer::County, 0.2),
            "counties when close"
        );
        assert!(
            style.is_visible(MapLayer::State, 4.0),
            "states at mid scale"
        );
        assert!(
            style.is_visible(MapLayer::Country, 40.0),
            "countries when far out"
        );
        assert!(!style.is_visible(MapLayer::County, 4.0));
        assert!(!style.is_visible(MapLayer::State, 40.0));
    }

    #[test]
    fn foreign_admin_stays_on_where_counties_cannot_replace_it() {
        // Canada and Mexico have no county-level table, so their provinces
        // must remain visible at close zoom or a border radar loses context.
        let style = MapStyle::default();
        assert!(style.is_visible(MapLayer::ForeignAdmin, 0.2));
        assert!(style.is_visible(MapLayer::ForeignAdmin, 4.0));
        assert!(!style.is_visible(MapLayer::ForeignAdmin, 40.0));
    }

    #[test]
    fn widths_are_positive_for_every_layer() {
        let style = MapStyle::default();
        for layer in MapLayer::ALL {
            assert!(style.layer(layer).width_px > 0.0, "{}", layer.label());
        }
    }

    /// The shipped look, pinned value for value.
    ///
    /// Every number below is hard-coded on purpose - including the two
    /// thresholds, which are written as literals rather than as the constants,
    /// so that editing a constant is also caught. Presets exist now, and the
    /// point of the default one is that nobody who liked the current map wakes
    /// up to a different map. A deliberate restyle of the default has to come
    /// through this test.
    #[test]
    fn default_style_is_pinned_value_for_value() {
        let style = MapStyle::default();

        assert_eq!(COUNTY_DETAIL_KM_PER_POINT, 1.2);
        assert_eq!(COUNTRY_ONLY_KM_PER_POINT, 8.0);

        assert_eq!(
            style.country,
            LayerStyle {
                color: LayerColor::rgba(0.62, 0.68, 0.78, 0.95),
                width_px: 1.6,
                min_km_per_point: 8.0,
                max_km_per_point: f32::MAX,
            }
        );
        assert_eq!(
            style.foreign_admin,
            LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                min_km_per_point: 0.0,
                max_km_per_point: 8.0,
            }
        );
        assert_eq!(
            style.state,
            LayerStyle {
                color: LayerColor::rgba(0.52, 0.58, 0.68, 0.85),
                width_px: 1.2,
                min_km_per_point: 1.2,
                max_km_per_point: 8.0,
            }
        );
        assert_eq!(
            style.county,
            LayerStyle {
                color: LayerColor::rgba(0.42, 0.47, 0.55, 0.85),
                width_px: 0.9,
                min_km_per_point: 0.0,
                max_km_per_point: 1.2,
            }
        );
    }

    /// `from_ink` must wire the bands onto the same layers the hand-written
    /// default does, or a preset built through it would draw at the wrong
    /// zooms while looking correct in a screenshot.
    #[test]
    fn from_ink_reproduces_the_default_style() {
        let default = MapStyle::default();
        let rebuilt = MapStyle::from_ink(
            MapInk {
                country: LayerInk::new(LayerColor::rgba(0.62, 0.68, 0.78, 0.95), 1.6),
                foreign_admin: LayerInk::new(LayerColor::rgba(0.52, 0.58, 0.68, 0.85), 1.2),
                state: LayerInk::new(LayerColor::rgba(0.52, 0.58, 0.68, 0.85), 1.2),
                county: LayerInk::new(LayerColor::rgba(0.42, 0.47, 0.55, 0.85), 0.9),
            },
            ScaleBands::DEFAULT,
        );
        assert_eq!(rebuilt, default);
    }

    #[test]
    fn default_style_is_well_formed() {
        assert!(MapStyle::default().is_well_formed());
        assert_eq!(MapStyle::default().bands(), ScaleBands::DEFAULT);
    }

    #[test]
    fn is_well_formed_rejects_the_ways_a_style_breaks() {
        let mut style = MapStyle::default();
        style.county.width_px = 0.0;
        assert!(!style.is_well_formed(), "zero width builds no triangles");

        let mut style = MapStyle::default();
        style.state.width_px = f32::NAN;
        assert!(!style.is_well_formed(), "NaN width");

        let mut style = MapStyle::default();
        style.country.color.a = 1.5;
        assert!(!style.is_well_formed(), "alpha above one");

        let mut style = MapStyle::default();
        style.county.color.r = -0.1;
        assert!(!style.is_well_formed(), "negative channel");

        // Inverted band: min above max, so the layer is never visible.
        let mut style = MapStyle::default();
        style.state.min_km_per_point = 9.0;
        assert!(!style.is_well_formed(), "inverted scale band");

        // A gap between county and state: at 1.5 km/point nothing US draws.
        let mut style = MapStyle::default();
        style.state.min_km_per_point = 2.0;
        assert!(!style.is_well_formed(), "gap between county and state");
        assert_eq!(
            [MapLayer::County, MapLayer::State, MapLayer::Country]
                .into_iter()
                .filter(|layer| style.is_visible(*layer, 1.5))
                .count(),
            0,
            "the gap is real, not just rejected on paper"
        );

        // An overlap: county and state both draw at 1.0 km/point.
        let mut style = MapStyle::default();
        style.state.min_km_per_point = 0.5;
        assert!(!style.is_well_formed(), "overlap between county and state");
        assert_eq!(
            [MapLayer::County, MapLayer::State, MapLayer::Country]
                .into_iter()
                .filter(|layer| style.is_visible(*layer, 1.0))
                .count(),
            2,
            "the overlap is real: one shoreline drawn twice"
        );
    }

    /// Byte to float and back, on values whose quotients are exact in binary
    /// (51/255 = 0.2, 102/255 = 0.4, 153/255 = 0.6, 204/255 = 0.8) plus the
    /// two endpoints. IEEE-754 division is correctly rounded, so each quotient
    /// is the same `f32` as the decimal literal.
    #[test]
    fn rgba8_conversion_is_exact_where_it_can_be() {
        assert_eq!(
            LayerColor::from_rgba8(51, 102, 153, 204),
            LayerColor::rgba(0.2, 0.4, 0.6, 0.8)
        );
        assert_eq!(
            LayerColor::from_rgba8(255, 255, 255, 255),
            LayerColor::rgba(1.0, 1.0, 1.0, 1.0)
        );
        assert_eq!(
            LayerColor::from_rgba8(0, 0, 0, 0),
            LayerColor::rgba(0.0, 0.0, 0.0, 0.0)
        );
        assert_eq!(LayerColor::from_rgb8(6, 9, 13).a, 1.0);

        // Round trip: the label ink and canvas the pane paints today.
        assert_eq!(
            LayerColor::from_rgb8(214, 222, 232).to_rgba8(),
            [214, 222, 232, 255]
        );
        assert_eq!(LayerColor::from_rgb8(6, 9, 13).to_rgba8(), [6, 9, 13, 255]);
        assert_eq!(
            LayerColor::from_rgba8(0, 0, 0, 190).to_rgba8(),
            [0, 0, 0, 190]
        );
    }

    /// `to_rgba8` has to quantise exactly the way `build::pack_color` does.
    ///
    /// The two are the only paths a style colour takes to the screen -
    /// `pack_color` writes the `Unorm8x4` in the vertex buffer, `to_rgba8`
    /// feeds the egui painter that draws the ground under it and the labels
    /// over it - and `pane_canvas::chrome_color` states in its own comment
    /// that they agree. Nothing enforced that. They are written differently
    /// (`(v * 255.0 + 0.5) as u8` here, `(v * 255.0).round() as u8` there),
    /// and while the two are the same function on `0.0..=1.0`, a later edit to
    /// either could make a line and the ground beneath it round a step apart -
    /// which on the `Minimal` preset, whose whole hierarchy is carried by
    /// alpha, is the difference between two layers and one.
    ///
    /// Swept rather than spot-checked, at four times the number of
    /// representable output steps, so every rounding boundary is crossed.
    #[test]
    fn to_rgba8_quantises_the_way_the_geometry_builder_packs() {
        // Copied from `build::pack_color`, which is private to that module.
        fn as_build_packs(channel: f32) -> u8 {
            (channel.clamp(0.0, 1.0) * 255.0).round() as u8
        }
        for step in 0..=1020_u32 {
            let channel = step as f32 / 1020.0;
            let packed = LayerColor::rgba(channel, channel, channel, channel).to_rgba8();
            let expected = as_build_packs(channel);
            assert_eq!(
                packed, [expected; 4],
                "channel {channel} packs as {packed:?} here and {expected} in the builder"
            );
        }
        // The two boundaries either side of the domain agree as well.
        assert_eq!(
            LayerColor::rgba(-1.0, 2.0, 0.0, 1.0).to_rgba8(),
            [0, 255, 0, 255]
        );
    }

    #[test]
    fn to_rgba8_clamps_instead_of_wrapping() {
        assert_eq!(
            LayerColor::rgba(2.0, -1.0, 0.5, f32::NAN).to_rgba8(),
            [255, 0, 128, 0]
        );
    }

    /// Hand-computed from ITU-R BT.709: pure green is the heaviest channel at
    /// 0.7152, pure blue the lightest at 0.0722.
    #[test]
    fn luminance_uses_bt709_weights() {
        assert_eq!(LayerColor::rgba(0.0, 1.0, 0.0, 1.0).luminance(), 0.7152);
        assert_eq!(LayerColor::rgba(0.0, 0.0, 1.0, 1.0).luminance(), 0.0722);
        assert_eq!(LayerColor::rgba(1.0, 0.0, 0.0, 1.0).luminance(), 0.2126);

        // Half-transparent white over black: 0.5 * 1.0 + 0.5 * 0.0. The three
        // weights sum to one only to within a rounding step in `f32`, so this
        // one is compared with a tolerance rather than for equality.
        let white_half = LayerColor::rgba(1.0, 1.0, 1.0, 0.5);
        let black = LayerColor::rgba(0.0, 0.0, 0.0, 1.0);
        assert!((white_half.composite_luminance_over(black) - 0.5).abs() < 1e-6);
        // Fully transparent ink is exactly its ground.
        let invisible = LayerColor::rgba(1.0, 1.0, 1.0, 0.0);
        assert_eq!(invisible.composite_luminance_over(black), 0.0);
    }

    #[test]
    fn scale_bands_reject_inverted_and_zero_thresholds() {
        assert!(ScaleBands::DEFAULT.is_well_formed());
        assert!(
            !ScaleBands {
                county_detail_km_per_point: 8.0,
                country_only_km_per_point: 1.2,
            }
            .is_well_formed()
        );
        assert!(
            !ScaleBands {
                county_detail_km_per_point: 0.0,
                country_only_km_per_point: 8.0,
            }
            .is_well_formed()
        );
    }
}
