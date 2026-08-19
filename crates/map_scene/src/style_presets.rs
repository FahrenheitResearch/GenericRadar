//! Named basemap looks, and the chrome each one implies.
//!
//! The map had exactly one appearance: whatever [`MapStyle::default`] said,
//! with no way to reach [`crate::MapSceneController::set_style`] from the
//! application. This module is the list a picker shows. Every entry is a
//! complete [`MapStyle`], every entry states what it is *for*, and the first
//! one is the map that shipped, unchanged.
//!
//! # Where the colours come from
//!
//! Transcribed from the console-native basemap rather than invented, so the
//! two applications look like the same family of tools:
//! `console_basemap::console_strokes` (cool slate lines meant to read as
//! geography without competing with radar colour),
//! `console_basemap::console_overlay_strokes` (the translucent pass drawn over
//! the radar), and the `console_map::MapStyle` ink ramp (`#f5f5f5`, `#d4d4d4`,
//! `#a3a3a3`, and the `(3, 5, 8, 235)` label halo).
//!
//! # Why chrome is here and not in `MapStyle`
//!
//! [`MapStyle`] is a *geometry* input - width decides how a line becomes
//! triangles, visibility decides whether it is built at all - and it is part of
//! the geometry cache key, so changing it throws away every retained buffer.
//! The pane background, the place-label ink, and every other mark the pane
//! draws straight onto that ground - readouts, range rings, site markers - are
//! paint-time only. Folding them into `MapStyle` would make a background tweak
//! rebuild the entire map mesh for nothing, so they travel alongside the style
//! as [`MapChrome`] instead.

use crate::style::{LayerColor, LayerInk, MapInk, MapStyle, ScaleBands};

/// Paint-time colours that go with a preset: what the pane clears to, and
/// every mark the pane draws straight onto that ground.
///
/// A light basemap on the dark pane background would be invisible, so a look is
/// only complete when the ground it lands on comes with it - and the same is
/// true in reverse of everything else painted on bare canvas. `console_map`'s
/// own `MapStyle` is the precedent: it carries `canvas`, `ring`, `place_dot`,
/// `label`, `label_halo` and the site inks together in one value for exactly
/// this reason. A token bag that stopped at the basemap would have left
/// `Daylight` with a near-white cursor readout on a near-white pane, which is
/// the same defect as the one this type was introduced to fix, one function
/// further down the file.
///
/// What is deliberately *not* here: anything the pane draws on top of its own
/// backing shape. The header bar, the legend panel and the site-marker fills
/// all paint an opaque or heavily translucent ground of their own first, so
/// their ink contrasts with that ground rather than with the canvas and does
/// not change with the look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapChrome {
    /// The pane background, painted before anything else.
    pub canvas: LayerColor,
    /// Place-label text.
    pub label_ink: LayerColor,
    /// The one-pixel outline offset around label text, which is what keeps a
    /// town name readable where it lands on a 70 dBZ core rather than on the
    /// basemap.
    pub label_halo: LayerColor,
    /// The geographic cursor readout, bottom left.
    pub readout_ink: LayerColor,
    /// The probed value readout, stacked above the geographic one. A separate
    /// token from `readout_ink` only because the shipped look gives the two
    /// different values; unifying them would move Slate by six steps of red
    /// for no reason anybody asked for.
    pub probe_ink: LayerColor,
    /// Range-ring stroke. Translucent in every look, because a ring is a
    /// measuring aid drawn straight across the data.
    pub range_ring: LayerColor,
    /// The dot at the radar itself, at the centre of the rings.
    pub origin_dot: LayerColor,
    /// Site-marker outline and identifier for a site that is not tuned.
    pub site_ink: LayerColor,
    /// The site currently being displayed.
    pub site_active_ink: LayerColor,
    /// The site under the pointer.
    pub site_hover_ink: LayerColor,
}

impl Default for MapChrome {
    /// The shipped chrome, which is [`MapStylePreset::Slate`]'s.
    ///
    /// Exists so a pane assembled without a scene - a default-constructed
    /// `PaneMap`, a test - paints today's colours rather than transparent
    /// black. Delegated rather than restated so it cannot drift.
    fn default() -> Self {
        MapStylePreset::Slate.chrome()
    }
}

impl MapChrome {
    /// The chrome that belongs with `style`.
    ///
    /// The scene controller stores a [`MapStyle`], not a preset - style is a
    /// geometry input and chrome is not, so they cannot live in the same value
    /// without a background tweak rebuilding the whole mesh (see the module
    /// note). This is the one place that closes that gap, so the pane painting
    /// the background and the picker choosing the look cannot disagree.
    ///
    /// A style that is not any preset - only reachable by constructing a
    /// `MapStyle` by hand - falls back to the shipped chrome rather than
    /// failing, because the alternative is a pane with no background at all.
    pub fn for_style(style: MapStyle) -> Self {
        MapStylePreset::for_style(style)
            .unwrap_or_default()
            .chrome()
    }
}

/// A selectable basemap look.
///
/// Shaped after `color_tables::ColorTableFamily`: an `ALL` in the order a
/// picker should list them, a `label()` for the picker, and a `style()` for the
/// thing being configured - plus an `id()`, because a saved workspace has to
/// name the choice in a way that survives reordering this enum.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum MapStylePreset {
    /// The map as it shipped: cool slate lines on a near-black pane.
    ///
    /// For ordinary desk work in a dim room. Value for value identical to
    /// [`MapStyle::default`], and pinned that way by test, so choosing it is
    /// always a way back to the map the operator already knows. `#[default]`
    /// for the same reason: an application that reaches for a preset without
    /// being told which one gets today's map, not a new one.
    #[default]
    Slate,
    /// Bright, opaque, thicker lines on pure black.
    ///
    /// For a projector or a screen in a lit room, where the slate look washes
    /// out to nothing: a projector's black is the room's ambient light, so the
    /// only contrast available is in the ink. Ink is the console's neutral
    /// ramp (`#f5f5f5` / `#d4d4d4` / `#a3a3a3`) at full alpha.
    HighContrast,
    /// Dark ink on a light pane.
    ///
    /// For daylight, a glass-walled room, or a screenshot going into a
    /// document that is printed on white. Ink is `console_strokes()` used as
    /// ink rather than as glow: those four slate values are dark enough to be
    /// dim lines on black, which is exactly what makes them legible lines on
    /// light grey.
    Daylight,
    /// The translucent overlay pass, thin and dim, with counties withheld
    /// until twice the zoom.
    ///
    /// For reading the radar itself - a velocity couplet or a debris ball -
    /// where the county mesh reads as texture over the data instead of as
    /// geography. Ink is `console_overlay_strokes()` verbatim.
    Minimal,
}

impl MapStylePreset {
    /// Every preset, in the order a picker should list them.
    ///
    /// The shipped look first because it is the one an operator returns to,
    /// then the two that change the room the map is read in, then the one that
    /// gets the map out of the way.
    ///
    /// A new variant must be added here *and* to [`Self::ordinal`]; the
    /// `ordinal` match is exhaustive, so the compiler stops at that one.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Slate,
        Self::HighContrast,
        Self::Daylight,
        Self::Minimal,
    ];

    /// How many presets there are. Declared next to `ALL` so its length and
    /// [`Self::ordinal`] are checked against each other by test.
    pub const COUNT: usize = 4;

    /// The stable identifier a saved workspace holds.
    ///
    /// Never reuse or repurpose one: a settings file written last week names a
    /// look by this string, and the enum's declaration order is not a contract.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Slate => "slate",
            Self::HighContrast => "high-contrast",
            Self::Daylight => "daylight",
            Self::Minimal => "minimal",
        }
    }

    /// The name a picker shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Slate => "Slate Dark",
            Self::HighContrast => "High Contrast",
            Self::Daylight => "Daylight",
            Self::Minimal => "Minimal",
        }
    }

    /// Position in [`Self::ALL`].
    ///
    /// Exhaustive on purpose - see `ALL` - and `pub` on purpose: a private
    /// method whose only caller is a `#[cfg(test)]` module is dead code in the
    /// library build, which fails `clippy -D warnings` as soon as this module
    /// is declared. It is also the index a picker needs for keyboard paging.
    pub const fn ordinal(self) -> usize {
        match self {
            Self::Slate => 0,
            Self::HighContrast => 1,
            Self::Daylight => 2,
            Self::Minimal => 3,
        }
    }

    /// Resolve a saved identifier. Unknown ids return `None` so the caller can
    /// fall back to the default rather than fail to start.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == id)
    }

    /// Which preset a style came from, if any.
    ///
    /// A picker needs to show the current selection, and the controller stores
    /// a `MapStyle`, not a preset.
    pub fn for_style(style: MapStyle) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.style() == style)
    }

    /// The scale thresholds this preset draws at.
    ///
    /// Only [`Self::Minimal`] departs from [`ScaleBands::DEFAULT`], and it says
    /// why on its own arm. Every other preset changes ink alone; a test pins
    /// that, because a look that quietly changes *when* a layer appears is a
    /// defect, not a style.
    pub const fn bands(self) -> ScaleBands {
        match self {
            Self::Slate | Self::HighContrast | Self::Daylight => ScaleBands::DEFAULT,
            // Counties are the clutter this preset exists to remove, so they
            // are withheld until the camera is twice as close. This mirrors
            // console-native, where `console_basemap::Layer::Overlay` raises
            // the county gate from map scale 38 to 76 - a factor of two - for
            // exactly this reason: over the radar, the dense county mesh reads
            // as texture rather than as geography. Halving a kilometres-per-
            // point threshold is that same doubling of zoom. The three US
            // bands still tile the scale axis without gap or overlap, so the
            // one-level-at-a-time guarantee is untouched.
            Self::Minimal => ScaleBands {
                county_detail_km_per_point: ScaleBands::DEFAULT.county_detail_km_per_point / 2.0,
                country_only_km_per_point: ScaleBands::DEFAULT.country_only_km_per_point,
            },
        }
    }

    /// The complete style. Hand this to
    /// [`crate::MapSceneController::set_style`].
    pub fn style(self) -> MapStyle {
        match self {
            // Deliberately delegated rather than restated: the shipped look has
            // exactly one definition, so it cannot drift from the default.
            Self::Slate => MapStyle::default(),
            _ => MapStyle::from_ink(self.ink(), self.bands()),
        }
    }

    /// Colour and width per layer.
    fn ink(self) -> MapInk {
        match self {
            // Reachable only through `MapStyle::default`, which `style()`
            // returns directly; restated here so `ink()` is total, and the two
            // are pinned equal by test.
            Self::Slate => MapInk {
                country: LayerInk::new(LayerColor::rgba(0.62, 0.68, 0.78, 0.95), 1.6),
                foreign_admin: LayerInk::new(LayerColor::rgba(0.52, 0.58, 0.68, 0.85), 1.2),
                state: LayerInk::new(LayerColor::rgba(0.52, 0.58, 0.68, 0.85), 1.2),
                county: LayerInk::new(LayerColor::rgba(0.42, 0.47, 0.55, 0.85), 0.9),
            },
            // `console_map::MapStyle`'s neutral ink ramp, opaque, and roughly a
            // third wider than the slate look: a projector loses thin lines
            // before it loses dim ones.
            Self::HighContrast => MapInk {
                // #f5f5f5, the console scalebar ink.
                country: LayerInk::new(LayerColor::from_rgb8(245, 245, 245), 2.2),
                // #d4d4d4, the console label ink.
                foreign_admin: LayerInk::new(LayerColor::from_rgb8(212, 212, 212), 1.7),
                state: LayerInk::new(LayerColor::from_rgb8(212, 212, 212), 1.7),
                // #a3a3a3, the console site-label ink: the county mesh is
                // dense, so it stays a step below the state lines even here.
                county: LayerInk::new(LayerColor::from_rgb8(163, 163, 163), 1.2),
            },
            // `console_basemap::console_strokes()`, re-ranked for a light
            // ground: on black the brightest line reads as the strongest, on
            // light grey the *darkest* does, so the ordering inverts while the
            // four values stay exactly what the console uses. Widths and the
            // alpha hierarchy are the shipped ones, so ink is the only thing
            // this preset changes.
            Self::Daylight => MapInk {
                // console county stroke #171f27, the darkest of the four.
                country: LayerInk::new(LayerColor::from_rgba8(23, 31, 39, 242), 1.6),
                // console regional stroke #2a3946.
                foreign_admin: LayerInk::new(LayerColor::from_rgba8(42, 57, 70, 217), 1.2),
                // console world stroke #263440.
                state: LayerInk::new(LayerColor::from_rgba8(38, 52, 64, 217), 1.2),
                // console state stroke #344655, the lightest of the four.
                county: LayerInk::new(LayerColor::from_rgba8(52, 70, 85, 191), 0.9),
            },
            // `console_basemap::console_overlay_strokes()` verbatim, layer for
            // layer, widths included.
            //
            // Three of these widths are below one point, which `shader.wgsl`
            // floors at a half-pixel half-width. On a 1x display that collapses
            // them to a single physical pixel each and the width hierarchy is
            // carried by alpha alone; at 2x it comes back. Kept as transcribed
            // anyway, because the alpha ordering is the part that does the work
            // in this preset and inventing wider strokes would make it a
            // different look with a borrowed provenance.
            Self::Minimal => MapInk {
                country: LayerInk::new(LayerColor::from_rgba8(102, 126, 145, 84), 0.85),
                foreign_admin: LayerInk::new(LayerColor::from_rgba8(112, 136, 154, 96), 0.75),
                state: LayerInk::new(LayerColor::from_rgba8(126, 150, 170, 116), 1.0),
                county: LayerInk::new(LayerColor::from_rgba8(92, 112, 128, 76), 0.55),
            },
        }
    }

    /// Pane background, label ink, and every other mark the pane paints
    /// straight onto the ground.
    ///
    /// Slate's ten values are, one for one, the constants `pane_canvas.rs`
    /// hard-coded before there was a picker, so choosing it is byte for byte
    /// the map that shipped. `slate_chrome_is_every_constant_the_pane_replaced`
    /// pins that against the literals.
    pub const fn chrome(self) -> MapChrome {
        match self {
            // Byte for byte what the pane canvas paints today, so wiring the
            // chrome up changes nothing for the default look.
            Self::Slate => MapChrome {
                canvas: LayerColor::from_rgb8(6, 9, 13),
                label_ink: LayerColor::from_rgb8(214, 222, 232),
                label_halo: LayerColor::from_rgba8(0, 0, 0, 190),
                readout_ink: LayerColor::from_rgb8(220, 228, 234),
                probe_ink: LayerColor::from_rgb8(226, 236, 246),
                range_ring: LayerColor::from_rgba8(170, 190, 205, 88),
                origin_dot: LayerColor::from_rgb8(230, 236, 240),
                site_ink: LayerColor::from_rgb8(170, 190, 210),
                site_active_ink: LayerColor::from_rgb8(120, 220, 255),
                site_hover_ink: LayerColor::from_rgb8(255, 236, 150),
            },
            Self::HighContrast => MapChrome {
                // Pure black, not the console's #0a0a0a: on a projector the
                // canvas is already lifted by room light, so every step of
                // headroom belongs to the ink.
                canvas: LayerColor::from_rgb8(0, 0, 0),
                label_ink: LayerColor::from_rgb8(245, 245, 245),
                // console_map's label halo, (3, 5, 8, 235).
                label_halo: LayerColor::from_rgba8(3, 5, 8, 235),
                // The neutral ramp again: a readout is text, so it takes the
                // same top of the ramp the place labels do.
                readout_ink: LayerColor::from_rgb8(245, 245, 245),
                probe_ink: LayerColor::from_rgb8(245, 245, 245),
                // #d4d4d4, and at a heavier alpha than the slate ring: a
                // projector loses a 34%-alpha hairline entirely.
                range_ring: LayerColor::from_rgba8(212, 212, 212, 120),
                origin_dot: LayerColor::from_rgb8(245, 245, 245),
                site_ink: LayerColor::from_rgb8(212, 212, 212),
                // The two states that mean "this one" keep their hue in every
                // look. They are found by colour rather than by brightness,
                // and a neutral ramp has no way to say "selected".
                site_active_ink: LayerColor::from_rgb8(120, 220, 255),
                site_hover_ink: LayerColor::from_rgb8(255, 236, 150),
            },
            Self::Daylight => MapChrome {
                // Light neutral grey, not paper white: reflectivity ramps end
                // in white before they turn magenta (the WSR-88D operational
                // scale and the GR2Analyst-style tables in `color_tables` both
                // do), so a white canvas would swallow the strongest echo on
                // the screen.
                canvas: LayerColor::from_rgb8(232, 236, 239),
                // The console county stroke again, as text.
                label_ink: LayerColor::from_rgb8(23, 31, 39),
                // The halo inverts with the ground: a dark outline on a light
                // pane would read as a smudge.
                label_halo: LayerColor::from_rgba8(255, 255, 255, 204),
                // Every mark below inverts too. Left at the shipped values
                // they would be near-white on a near-white pane: the
                // geographic readout would carry 0.04 of BT.709 luminance
                // separation from this canvas and the probed value 0.002,
                // which is no readout at all.
                readout_ink: LayerColor::from_rgb8(23, 31, 39),
                probe_ink: LayerColor::from_rgb8(23, 31, 39),
                // console stroke #344655, at the alpha that lands on the same
                // composited separation the slate ring has against its own
                // ground (0.24 of luminance), so the rings are as present here
                // as there and no more.
                range_ring: LayerColor::from_rgba8(52, 70, 85, 92),
                origin_dot: LayerColor::from_rgb8(23, 31, 39),
                // console regional stroke #2a3946.
                site_ink: LayerColor::from_rgb8(42, 57, 70),
                // The selection blue and the hover amber, taken down to where
                // they are the darker of the pair against a light pane. The
                // shipped rgb(120, 220, 255) and rgb(255, 236, 150) are both
                // lighter than this canvas - the hover state by 0.008 of
                // luminance, which is nothing.
                site_active_ink: LayerColor::from_rgb8(0, 95, 150),
                site_hover_ink: LayerColor::from_rgb8(150, 95, 0),
            },
            Self::Minimal => MapChrome {
                canvas: LayerColor::from_rgb8(6, 9, 13),
                // #a3a3a3: labels step back with the lines, or they become the
                // clutter the lines stopped being.
                label_ink: LayerColor::from_rgb8(163, 163, 163),
                label_halo: LayerColor::from_rgba8(0, 0, 0, 190),
                // Furniture is the shipped furniture, unchanged, because this
                // preset shares Slate's ground exactly. What it thins is the
                // basemap - the mesh that competes with the data. A range ring
                // or a site marker is a measuring aid the operator asked for,
                // so dimming those as well would be a different preset from
                // the one this one describes.
                readout_ink: LayerColor::from_rgb8(220, 228, 234),
                probe_ink: LayerColor::from_rgb8(226, 236, 246),
                range_ring: LayerColor::from_rgba8(170, 190, 205, 88),
                origin_dot: LayerColor::from_rgb8(230, 236, 240),
                site_ink: LayerColor::from_rgb8(170, 190, 210),
                site_active_ink: LayerColor::from_rgb8(120, 220, 255),
                site_hover_ink: LayerColor::from_rgb8(255, 236, 150),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LOD_REFERENCE_KM_PER_POINT;
    use crate::dataset::MapLayer;
    use crate::style::{COUNTRY_ONLY_KM_PER_POINT, COUNTY_DETAIL_KM_PER_POINT};
    use analyst_runtime::{Camera2D, LodBucket, MAX_KM_PER_POINT, MIN_KM_PER_POINT};

    /// How many US boundary levels this style draws at one camera scale.
    /// Anything but one is a defect: zero is a hole in the basemap, two is the
    /// same generalised shoreline drawn twice, a pixel apart.
    fn us_levels_at(style: MapStyle, km_per_point: f32) -> usize {
        [MapLayer::County, MapLayer::State, MapLayer::Country]
            .into_iter()
            .filter(|layer| style.is_visible(*layer, km_per_point))
            .count()
    }

    /// Every colour in a chrome, named. Exhaustive by construction: it
    /// destructures `MapChrome`, so adding a token to that struct without
    /// adding it here stops the compiler rather than quietly leaving the new
    /// colour out of the range and contrast checks below.
    fn chrome_tokens(chrome: MapChrome) -> [(&'static str, LayerColor); 10] {
        let MapChrome {
            canvas,
            label_ink,
            label_halo,
            readout_ink,
            probe_ink,
            range_ring,
            origin_dot,
            site_ink,
            site_active_ink,
            site_hover_ink,
        } = chrome;
        [
            ("canvas", canvas),
            ("label_ink", label_ink),
            ("label_halo", label_halo),
            ("readout_ink", readout_ink),
            ("probe_ink", probe_ink),
            ("range_ring", range_ring),
            ("origin_dot", origin_dot),
            ("site_ink", site_ink),
            ("site_active_ink", site_active_ink),
            ("site_hover_ink", site_hover_ink),
        ]
    }

    /// The tokens the pane paints straight onto its own canvas with nothing
    /// behind them, so each one has to separate from that canvas by itself.
    ///
    /// `canvas` is excluded because it *is* the ground, and `label_halo`
    /// because its whole job is to sit against the label ink rather than
    /// against the canvas - both are checked elsewhere.
    fn on_bare_canvas(chrome: MapChrome) -> Vec<(&'static str, LayerColor)> {
        chrome_tokens(chrome)
            .into_iter()
            .filter(|(name, _)| !matches!(*name, "canvas" | "label_halo"))
            .collect()
    }

    /// The whole point of the default preset: an operator who liked the map
    /// gets the same map.
    #[test]
    fn default_preset_is_the_shipped_style_unchanged() {
        assert_eq!(MapStylePreset::default(), MapStylePreset::Slate);
        assert_eq!(MapStylePreset::default().style(), MapStyle::default());
        assert_eq!(MapStylePreset::ALL[0], MapStylePreset::Slate);
        // Also equal the long way round, so the restated Slate ink cannot
        // drift from `MapStyle::default` unnoticed.
        assert_eq!(
            MapStyle::from_ink(MapStylePreset::Slate.ink(), MapStylePreset::Slate.bands()),
            MapStyle::default()
        );
    }

    /// Byte for byte what the pane canvas hard-coded before there was a
    /// picker. Wiring chrome through must be a no-op for Slate, and it must
    /// stay a no-op as the token bag grows: every literal below was read out
    /// of `pane_canvas.rs` at the line that used to carry it, and the pane has
    /// a matching test that the shapes it emits are these same values.
    ///
    /// The full list, and where each one came from:
    ///
    /// - `canvas` - the pane background rect, and the hazard-tag halo,
    /// - `label_ink` / `label_halo` - `draw_map_labels`,
    /// - `readout_ink` - `draw_cursor_readout`,
    /// - `probe_ink` - `draw_probe_readout`,
    /// - `range_ring` / `origin_dot` - `draw_range_rings`,
    /// - `site_ink` / `site_active_ink` / `site_hover_ink` -
    ///   `draw_radar_sites`, which uses each for both a marker outline and
    ///   the identifier drawn above it.
    #[test]
    fn slate_chrome_is_every_constant_the_pane_replaced() {
        let chrome = MapStylePreset::Slate.chrome();
        assert_eq!(chrome.canvas.to_rgba8(), [6, 9, 13, 255]);
        assert_eq!(chrome.label_ink.to_rgba8(), [214, 222, 232, 255]);
        assert_eq!(chrome.label_halo.to_rgba8(), [0, 0, 0, 190]);
        assert_eq!(chrome.readout_ink.to_rgba8(), [220, 228, 234, 255]);
        assert_eq!(chrome.probe_ink.to_rgba8(), [226, 236, 246, 255]);
        assert_eq!(chrome.range_ring.to_rgba8(), [170, 190, 205, 88]);
        assert_eq!(chrome.origin_dot.to_rgba8(), [230, 236, 240, 255]);
        assert_eq!(chrome.site_ink.to_rgba8(), [170, 190, 210, 255]);
        assert_eq!(chrome.site_active_ink.to_rgba8(), [120, 220, 255, 255]);
        assert_eq!(chrome.site_hover_ink.to_rgba8(), [255, 236, 150, 255]);

        // Minimal shares Slate's ground, so it shares Slate's furniture too.
        // Stated here rather than left implicit, because the two differing
        // only in `label_ink` is a deliberate choice and not an oversight.
        let minimal = MapStylePreset::Minimal.chrome();
        assert_eq!(
            MapChrome {
                label_ink: chrome.label_ink,
                ..minimal
            },
            chrome,
            "Minimal changed more than its label ink"
        );
    }

    #[test]
    fn every_preset_is_a_valid_map_style() {
        for preset in MapStylePreset::ALL {
            let style = preset.style();
            assert!(
                style.is_well_formed(),
                "{} is not a valid style: {style:?}",
                preset.id()
            );
            for layer in MapLayer::ALL {
                let layer_style = style.layer(layer);
                assert!(
                    layer_style.width_px.is_finite() && layer_style.width_px > 0.0,
                    "{} {} width {}",
                    preset.id(),
                    layer.label(),
                    layer_style.width_px
                );
                assert!(
                    (0.0..=1.0).contains(&layer_style.color.a),
                    "{} {} alpha {}",
                    preset.id(),
                    layer.label(),
                    layer_style.color.a
                );
                assert!(
                    layer_style.color.is_in_range(),
                    "{} {} colour {:?}",
                    preset.id(),
                    layer.label(),
                    layer_style.color
                );
                assert!(
                    layer_style.min_km_per_point < layer_style.max_km_per_point,
                    "{} {} band is inverted",
                    preset.id(),
                    layer.label()
                );
            }
        }
    }

    /// The guarantee that survives every restyle: two US levels together draw
    /// the same generalised shoreline twice, a pixel apart, and zero levels
    /// leaves a hole in the map.
    ///
    /// Swept densely rather than at a handful of hand-picked scales, because a
    /// hand-picked list only proves the points on the list. The range is the
    /// whole interval `Camera2D::sanitized` can produce - `MIN_KM_PER_POINT`
    /// to `MAX_KM_PER_POINT` - walked in log space so the close zooms, where
    /// the band edges sit, get as many samples as the far ones. Each preset's
    /// own two edges and their immediate `f32` neighbours are added on top,
    /// since a half-open band `(min, max]` fails one step either side of an
    /// edge or nowhere at all.
    #[test]
    fn every_preset_draws_exactly_one_us_level_at_every_scale() {
        const STEPS: u32 = 4000;
        let span = MAX_KM_PER_POINT / MIN_KM_PER_POINT;
        for preset in MapStylePreset::ALL {
            let style = preset.style();
            let mut scales: Vec<f32> = (0..=STEPS)
                .map(|step| MIN_KM_PER_POINT * span.powf(step as f32 / STEPS as f32))
                .collect();
            for edge in [
                preset.bands().county_detail_km_per_point,
                preset.bands().country_only_km_per_point,
            ] {
                scales.push(edge);
                // The representable neighbours of the edge: `to_bits` is
                // monotonic over positive finite floats, so +/-1 is one step.
                scales.push(f32::from_bits(edge.to_bits() - 1));
                scales.push(f32::from_bits(edge.to_bits() + 1));
            }
            // Well outside the camera's range, so `is_visible` is sound as a
            // public function and not only as the app happens to call it.
            scales.extend([1.0e-6_f32, 0.001, 500.0, 1.0e6, f32::MAX]);

            for km_per_point in scales {
                let levels = us_levels_at(style, km_per_point);
                assert_eq!(
                    levels,
                    1,
                    "{} drew {levels} US levels at {km_per_point} km/point",
                    preset.id()
                );
            }
        }
    }

    /// The scales the renderer actually asks about are not camera scales.
    ///
    /// `build_geometry` tests visibility against the *bucket centre*
    /// (`LodBucket::center_scale`), because exact camera scale is deliberately
    /// not part of the geometry cache key. So the question that decides whether
    /// the basemap can go blank is whether every bucket the app can reach draws
    /// exactly one level. The range swept here is far wider than the one
    /// `MIN_KM_PER_POINT..=MAX_KM_PER_POINT` produces, which covers `LodSelector`
    /// holding a bucket a step or two off ideal for hysteresis.
    #[test]
    fn every_lod_bucket_the_app_can_reach_draws_exactly_one_us_level() {
        let ideal_min = LodBucket::ideal(MIN_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT).0;
        let ideal_max = LodBucket::ideal(MAX_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT).0;
        assert!(ideal_min < ideal_max, "bucket ladder runs the wrong way");

        for preset in MapStylePreset::ALL {
            let style = preset.style();
            for bucket in (ideal_min - 20)..=(ideal_max + 20) {
                let km = LodBucket(bucket).center_scale(LOD_REFERENCE_KM_PER_POINT);
                let levels = us_levels_at(style, km);
                assert_eq!(
                    levels,
                    1,
                    "{} drew {levels} US levels in LOD bucket {bucket} ({km} km/point)",
                    preset.id()
                );
            }
        }
    }

    /// No camera an operator can produce - including one restored from a
    /// corrupt saved workspace - reaches a scale where the basemap disappears.
    ///
    /// `is_visible` uses a half-open band `(min, max]` and county's `min` is
    /// zero, so a scale of exactly zero, a negative one, or a non-finite one
    /// draws nothing at all. That case is unreachable rather than handled, and
    /// this test pins the two guards that make it unreachable:
    /// `Camera2D::sanitized` replaces a non-finite or non-positive scale with
    /// the default and clamps into `MIN_KM_PER_POINT..=MAX_KM_PER_POINT`, and
    /// `LodBucket` applies the same guard before raising two to a bounded
    /// power. Teaching `is_visible` to cope with a zero scale instead would
    /// mean moving a band edge, which is the one thing a restyle must not do.
    #[test]
    fn a_corrupt_camera_cannot_blank_the_basemap() {
        let poisoned = [
            0.0_f32,
            -0.0,
            -1.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            1.0e30,
            MIN_KM_PER_POINT,
            MAX_KM_PER_POINT,
        ];
        for km_per_point in poisoned {
            let camera = Camera2D {
                km_per_point,
                ..Camera2D::default()
            }
            .sanitized();
            assert!(
                camera.km_per_point.is_finite() && camera.km_per_point > 0.0,
                "sanitized camera kept {km_per_point}"
            );
            assert!((MIN_KM_PER_POINT..=MAX_KM_PER_POINT).contains(&camera.km_per_point));

            let km = LodBucket::ideal(camera.km_per_point, LOD_REFERENCE_KM_PER_POINT)
                .center_scale(LOD_REFERENCE_KM_PER_POINT);
            assert!(km.is_finite() && km > 0.0, "bucket centre {km}");
            for preset in MapStylePreset::ALL {
                let levels = us_levels_at(preset.style(), km);
                assert_eq!(
                    levels,
                    1,
                    "{} drew {levels} US levels for camera scale {km_per_point}",
                    preset.id()
                );
            }
        }
    }

    /// Canada and Mexico have no county table, so their provinces must stay on
    /// at close zoom in every look or a border radar loses its context.
    #[test]
    fn foreign_admin_survives_every_preset() {
        for preset in MapStylePreset::ALL {
            let style = preset.style();
            assert!(
                style.is_visible(MapLayer::ForeignAdmin, 0.2),
                "{} hid foreign provinces when close",
                preset.id()
            );
            assert!(
                style.is_visible(MapLayer::ForeignAdmin, 4.0),
                "{} hid foreign provinces at mid scale",
                preset.id()
            );
            assert!(
                !style.is_visible(MapLayer::ForeignAdmin, 40.0),
                "{} drew foreign provinces where only countries belong",
                preset.id()
            );
        }
    }

    /// Only Minimal moves a threshold, and only the county one. Anything else
    /// changing a band is a preset changing behaviour while pretending to
    /// change appearance.
    #[test]
    fn only_minimal_departs_from_the_shipped_scale_bands() {
        for preset in MapStylePreset::ALL {
            if preset == MapStylePreset::Minimal {
                continue;
            }
            let bands = preset.style().bands();
            assert_eq!(
                bands,
                ScaleBands::DEFAULT,
                "{} changed the scale bands",
                preset.id()
            );
            assert_eq!(bands.county_detail_km_per_point, 1.2);
            assert_eq!(bands.country_only_km_per_point, 8.0);
        }
    }

    /// Minimal's one behavioural difference, stated exactly: counties appear at
    /// half the kilometres per point, which is twice the zoom, and the coarse
    /// threshold is untouched.
    #[test]
    fn minimal_withholds_counties_until_twice_the_zoom() {
        let minimal = MapStylePreset::Minimal.style();
        let slate = MapStylePreset::Slate.style();

        assert_eq!(minimal.bands().county_detail_km_per_point, 0.6);
        assert_eq!(
            minimal.bands().county_detail_km_per_point * 2.0,
            COUNTY_DETAIL_KM_PER_POINT
        );
        assert_eq!(
            minimal.bands().country_only_km_per_point,
            COUNTRY_ONLY_KM_PER_POINT
        );

        // At 1.0 km/point Slate is already drawing counties; Minimal is still
        // on state outlines.
        assert!(slate.is_visible(MapLayer::County, 1.0));
        assert!(!slate.is_visible(MapLayer::State, 1.0));
        assert!(!minimal.is_visible(MapLayer::County, 1.0));
        assert!(minimal.is_visible(MapLayer::State, 1.0));

        // Twice as close, both draw counties.
        assert!(minimal.is_visible(MapLayer::County, 0.5));
        assert!(slate.is_visible(MapLayer::County, 0.5));
    }

    #[test]
    fn all_lists_every_preset_exactly_once() {
        assert_eq!(MapStylePreset::ALL.len(), MapStylePreset::COUNT);
        for (index, preset) in MapStylePreset::ALL.into_iter().enumerate() {
            assert_eq!(
                preset.ordinal(),
                index,
                "{} is listed out of order",
                preset.id()
            );
            let occurrences = MapStylePreset::ALL
                .into_iter()
                .filter(|other| *other == preset)
                .count();
            assert_eq!(occurrences, 1, "{} appears twice in ALL", preset.id());
        }
    }

    #[test]
    fn ids_and_labels_are_unique_and_non_empty() {
        for (index, preset) in MapStylePreset::ALL.into_iter().enumerate() {
            assert!(!preset.id().is_empty(), "empty id at {index}");
            assert!(!preset.label().is_empty(), "empty label at {index}");
            // An id goes into a settings file: no spaces, no case surprises.
            assert!(
                preset
                    .id()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{} is not a stable identifier",
                preset.id()
            );
            for other in MapStylePreset::ALL.into_iter().skip(index + 1) {
                assert_ne!(preset.id(), other.id(), "duplicate id");
                assert_ne!(preset.label(), other.label(), "duplicate label");
            }
        }
    }

    /// The one US boundary level this style draws at this scale, with the
    /// colour the GPU receives and its stroke width.
    ///
    /// Colours are quantised to eight bits per channel first, because
    /// `build_geometry` packs them to `[u8; 4]` before they leave the CPU - a
    /// difference finer than one of those steps does not exist on screen.
    fn us_level_at(style: MapStyle, km_per_point: f32) -> (MapLayer, [u8; 4], f32) {
        let mut drawn = [MapLayer::County, MapLayer::State, MapLayer::Country]
            .into_iter()
            .filter(|layer| style.is_visible(*layer, km_per_point));
        let layer = drawn.next().expect("no US level drawn");
        assert!(drawn.next().is_none(), "two US levels drawn");
        let level = style.layer(layer);
        (layer, level.color.to_rgba8(), level.width_px)
    }

    /// Four presets that are only nominally different would be no better than
    /// the one look the map had - and `!=` on a struct of `f32` does not rule
    /// that out, because two styles one bit apart compare unequal and look
    /// identical.
    ///
    /// So the difference is measured, and measured under the two conditions
    /// that make it honest:
    ///
    /// *At every rung of the LOD ladder separately*, not across the style as a
    /// whole. The largest difference over all four layers would let a preset
    /// that clones another everywhere except the country line pass, and country
    /// lines only draw past 8 km/point - the pair would be indistinguishable at
    /// every zoom an operator actually works at. A look is different only if it
    /// is different where you are looking.
    ///
    /// *On the US boundary level alone*, not on whatever else happens to be on
    /// screen. Foreign provinces are the only other line layer, and for a radar
    /// in the interior - Oklahoma, Kansas, most of the network - that layer has
    /// no geometry in range at all. A pair of presets whose difference is
    /// carried by the foreign layer is one look for most of the country, so it
    /// does not count here.
    ///
    /// At each rung, any *one* of these separations is enough:
    ///
    /// - a different US level drawn altogether,
    /// - 24/255 in some channel of that level (just under 10% of the range),
    /// - 0.35 px of its stroke width,
    /// - 0.05 of BT.709 luminance between the two canvases.
    ///
    /// The tightest real margin belongs to Slate against High Contrast, which
    /// share a near-black canvas (0.034 apart, below the bar) and are carried
    /// entirely by ink: 56/255 on the county line at the closest rungs,
    /// widening to 87/255 on the country line at the coarsest. That is more
    /// than twice the ink bar, so these thresholds reject a cosmetic clone
    /// without having been tuned to pass the present four.
    #[test]
    fn presets_are_genuinely_different_from_each_other() {
        const MIN_INK_STEPS: i32 = 24;
        const MIN_CANVAS_LUMINANCE: f32 = 0.05;
        const MIN_WIDTH_PX: f32 = 0.35;

        let ideal_min = LodBucket::ideal(MIN_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT).0;
        let ideal_max = LodBucket::ideal(MAX_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT).0;

        for (index, preset) in MapStylePreset::ALL.into_iter().enumerate() {
            for other in MapStylePreset::ALL.into_iter().skip(index + 1) {
                let (mine, theirs) = (preset.style(), other.style());
                let canvas_delta =
                    (preset.chrome().canvas.luminance() - other.chrome().canvas.luminance()).abs();

                for bucket in ideal_min..=ideal_max {
                    let km = LodBucket(bucket).center_scale(LOD_REFERENCE_KM_PER_POINT);
                    let (layer_a, ink_a, width_a) = us_level_at(mine, km);
                    let (layer_b, ink_b, width_b) = us_level_at(theirs, km);

                    let mut ink_steps = 0;
                    if layer_a == layer_b {
                        for channel in 0..4 {
                            ink_steps = ink_steps
                                .max((i32::from(ink_a[channel]) - i32::from(ink_b[channel])).abs());
                        }
                    }

                    assert!(
                        layer_a != layer_b
                            || ink_steps >= MIN_INK_STEPS
                            || (width_a - width_b).abs() >= MIN_WIDTH_PX
                            || canvas_delta >= MIN_CANVAS_LUMINANCE,
                        "{} and {} draw the same {} line at {km} km/point (LOD bucket \
                         {bucket}): {ink_steps}/255 of ink, {} px of width, {canvas_delta} \
                         of canvas luminance",
                        preset.id(),
                        other.id(),
                        layer_a.label(),
                        (width_a - width_b).abs()
                    );
                }

                // Stated separately because it is a different property: two
                // presets that resolved to one style would make `for_style`
                // ambiguous and a picker would show the wrong selection.
                assert_ne!(
                    mine,
                    theirs,
                    "{} and {} are the same style",
                    preset.id(),
                    other.id()
                );
                assert_ne!(
                    preset.chrome(),
                    other.chrome(),
                    "{} and {} have the same chrome",
                    preset.id(),
                    other.id()
                );
            }
        }
    }

    /// Minimal's smaller county threshold has to change what is on screen at a
    /// zoom the app can actually reach, or it is a number with no effect.
    ///
    /// Walking the LOD ladder rather than arbitrary scales, because the ladder
    /// is what the renderer steps through. Exactly two of the reachable buckets
    /// differ: the two whose centres fall between Minimal's county threshold
    /// (0.6 km/point) and the shipped one (1.2), where Slate has already
    /// switched to the dense county mesh and Minimal is still on state
    /// outlines. Two rungs out of twenty-six is small, and it is the whole
    /// behavioural difference between the presets - everything else is ink.
    #[test]
    fn minimal_differs_from_slate_on_exactly_two_rungs_of_the_lod_ladder() {
        let slate = MapStylePreset::Slate.style();
        let minimal = MapStylePreset::Minimal.style();
        let ideal_min = LodBucket::ideal(MIN_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT).0;
        let ideal_max = LodBucket::ideal(MAX_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT).0;

        let mut differing = Vec::new();
        for bucket in ideal_min..=ideal_max {
            let km = LodBucket(bucket).center_scale(LOD_REFERENCE_KM_PER_POINT);
            for layer in [MapLayer::County, MapLayer::State, MapLayer::Country] {
                if slate.is_visible(layer, km) != minimal.is_visible(layer, km) {
                    differing.push(bucket);
                    break;
                }
            }
        }
        assert_eq!(
            differing.len(),
            2,
            "Minimal differs from Slate on buckets {differing:?}"
        );
        for bucket in differing {
            let km = LodBucket(bucket).center_scale(LOD_REFERENCE_KM_PER_POINT);
            assert!(
                km > minimal.bands().county_detail_km_per_point
                    && km <= slate.bands().county_detail_km_per_point,
                "bucket {bucket} at {km} km/point is outside the withheld range"
            );
            assert!(slate.is_visible(MapLayer::County, km));
            assert!(minimal.is_visible(MapLayer::State, km));
        }
    }

    #[test]
    fn ids_round_trip_and_unknown_ids_do_not_resolve() {
        for preset in MapStylePreset::ALL {
            assert_eq!(MapStylePreset::from_id(preset.id()), Some(preset));
            assert_eq!(MapStylePreset::for_style(preset.style()), Some(preset));
        }
        assert_eq!(MapStylePreset::from_id("slate "), None);
        assert_eq!(MapStylePreset::from_id("Slate"), None);
        assert_eq!(MapStylePreset::from_id(""), None);
    }

    /// Every line must be visible against the ground its own preset paints.
    ///
    /// Composited luminance, BT.709: `alpha * ink + (1 - alpha) * canvas`. The
    /// tightest case by hand is Minimal's county line - ink `rgb(92, 112, 128)`
    /// is luminance 0.427 against a 0.034 canvas, at alpha 76/255 = 0.298, so
    /// 0.298 * (0.427 - 0.034) = 0.117 of separation. The 0.08 floor sits below
    /// that and far above zero, so it catches ink that has been made invisible
    /// without failing the dimmest look, which is meant to be dim.
    #[test]
    fn every_preset_separates_its_ink_from_its_own_canvas() {
        for preset in MapStylePreset::ALL {
            let canvas = preset.chrome().canvas;
            let ground = canvas.luminance();
            for layer in MapLayer::ALL {
                let ink = preset.style().layer(layer).color;
                let separation = ink.composite_luminance_over(canvas) - ground;
                assert!(
                    separation.abs() >= 0.08,
                    "{} {} is invisible on its own canvas: separation {separation}",
                    preset.id(),
                    layer.label()
                );
            }
            let label = preset.chrome().label_ink;
            let label_separation = label.composite_luminance_over(canvas) - ground;
            assert!(
                label_separation.abs() >= 0.4,
                "{} label ink is unreadable on its own canvas: separation {label_separation}",
                preset.id()
            );
            // The halo exists to separate text from whatever is under it, so
            // it has to contrast with the text, not with the canvas.
            let halo_separation = preset.chrome().label_halo.luminance() - label.luminance();
            assert!(
                halo_separation.abs() >= 0.4,
                "{} label halo does not separate from its own text",
                preset.id()
            );
        }
    }

    /// Daylight is the only light-ground look, and it must be dark ink on
    /// light ground throughout - one bright line on a light canvas is the one
    /// that disappears.
    #[test]
    fn daylight_is_dark_ink_on_light_ground_and_the_rest_are_the_reverse() {
        for preset in MapStylePreset::ALL {
            let canvas_luminance = preset.chrome().canvas.luminance();
            let light_ground = preset == MapStylePreset::Daylight;
            assert_eq!(
                canvas_luminance > 0.5,
                light_ground,
                "{} canvas luminance {canvas_luminance} contradicts its kind",
                preset.id()
            );
            for layer in MapLayer::ALL {
                let ink = preset.style().layer(layer).color.luminance();
                assert_eq!(
                    ink < canvas_luminance,
                    light_ground,
                    "{} {} runs the wrong way against its canvas",
                    preset.id(),
                    layer.label()
                );
            }
        }
    }

    /// Every mark the pane paints straight onto its own ground has to be
    /// visible against that ground, in every look.
    ///
    /// This is the test that would have caught the second half of the reported
    /// bug. Wiring only the basemap ink and the place labels through the
    /// chrome left `Daylight` painting the shipped near-white cursor readout
    /// (`rgb(220, 228, 234)`, BT.709 luminance 0.889) onto its near-white pane
    /// (0.923): 0.034 of separation, which is not a readout. The probed value
    /// was worse at 0.003, the site-marker hover highlight worse still at
    /// 0.006, and the dot marking the radar itself worst of all at 0.001 -
    /// four marks that had gone from readable to absent while every test in
    /// the crate stayed green. See
    /// `the_shipped_furniture_would_be_invisible_on_the_daylight_pane`, which
    /// pins those four numbers so the reason for this test cannot be
    /// forgotten.
    ///
    /// The floor is 0.2, and it is the *composited* separation, so a
    /// translucent mark is judged on what the eye actually receives rather
    /// than on the colour it was written as. The two range rings are the
    /// tightest cases by design - a ring is drawn across the data and must not
    /// compete with it - at 0.241 for Slate and 0.238 for Daylight, so the
    /// floor sits just under the dimmest mark that is deliberately dim and far
    /// above the ones that had disappeared.
    #[test]
    fn every_mark_on_bare_canvas_separates_from_it() {
        const MIN_SEPARATION: f32 = 0.2;
        for preset in MapStylePreset::ALL {
            let chrome = preset.chrome();
            let ground = chrome.canvas.luminance();
            for (name, color) in on_bare_canvas(chrome) {
                let separation = color.composite_luminance_over(chrome.canvas) - ground;
                // Run with --nocapture to read the table these numbers came
                // from; the doc comment above quotes it.
                println!(
                    "{:<14} {name:<16} {:?} on {:?}: {separation:+.3}",
                    preset.id(),
                    color.to_rgba8(),
                    chrome.canvas.to_rgba8(),
                );
                assert!(
                    separation.abs() >= MIN_SEPARATION,
                    "{} {name} is invisible on its own canvas: {separation} of luminance",
                    preset.id()
                );
            }
        }
    }

    /// Why `Daylight` needs its own furniture, stated as the measurement that
    /// forced it rather than as an opinion.
    ///
    /// Every value below is Slate's - the shipped constant the pane used to
    /// hard-code - composited over `Daylight`'s canvas. Four of the eight land
    /// under a twentieth of the luminance range, which is the definition of
    /// not being on the screen. Had the chrome carried only the basemap ink
    /// and the place labels, this is exactly what an operator who chose
    /// `Daylight` would have got: a readable map with no cursor readout, no
    /// probed value, no radar-centre dot, and a hover highlight that did not
    /// highlight.
    #[test]
    fn the_shipped_furniture_would_be_invisible_on_the_daylight_pane() {
        let ground = MapStylePreset::Daylight.chrome().canvas;
        let shipped = MapStylePreset::Slate.chrome();
        let mut worst: f32 = 1.0;
        for (name, color) in on_bare_canvas(shipped) {
            let separation = color.composite_luminance_over(ground) - ground.luminance();
            println!("slate {name:<16} on the daylight pane: {separation:+.4}");
            worst = worst.min(separation.abs());
        }
        assert!(
            worst < 0.005,
            "the shipped furniture is no longer invisible on a light pane              ({worst} at its faintest); if that is deliberate, this test and              Daylight's furniture both need revisiting"
        );
    }

    /// Two marks that mean different things must not read as the same mark.
    ///
    /// The site under the pointer and the site being displayed are told apart
    /// by colour alone - the box is the same size and the same shape - so a
    /// look that made them similar would take away the only cue there is.
    #[test]
    fn the_three_site_states_stay_distinguishable_in_every_look() {
        for preset in MapStylePreset::ALL {
            let chrome = preset.chrome();
            let states = [
                ("idle", chrome.site_ink),
                ("active", chrome.site_active_ink),
                ("hover", chrome.site_hover_ink),
            ];
            for (index, (name, color)) in states.into_iter().enumerate() {
                for (other_name, other) in states.into_iter().skip(index + 1) {
                    let steps = color
                        .to_rgba8()
                        .into_iter()
                        .zip(other.to_rgba8())
                        .map(|(a, b)| i32::from(a).abs_diff(i32::from(b)))
                        .max()
                        .unwrap_or(0);
                    assert!(
                        steps >= 24,
                        "{} draws {name} and {other_name} sites {steps}/255 apart",
                        preset.id()
                    );
                }
            }
        }
    }

    #[test]
    fn every_chrome_colour_is_in_range() {
        for preset in MapStylePreset::ALL {
            let chrome = preset.chrome();
            for (name, color) in chrome_tokens(chrome) {
                assert!(
                    color.is_in_range(),
                    "{} {name} is out of range: {color:?}",
                    preset.id()
                );
            }
            // A transparent canvas would leave the previous frame on screen.
            assert_eq!(chrome.canvas.a, 1.0, "{} canvas is not opaque", preset.id());
            assert_eq!(
                chrome.label_ink.a,
                1.0,
                "{} label ink is not opaque",
                preset.id()
            );
        }
    }
}

/// What a preset actually delivers to the two places that paint it, measured
/// by running the real build rather than by reading the style struct.
///
/// These tests exist because the picker looked correct and the map did not
/// change. Every claim below is the output of `build_geometry` over the
/// compiled-in basemap tables, projected at a real radar's real coordinates.
#[cfg(test)]
mod delivered_to_the_screen {
    use super::*;
    use crate::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
    use crate::dataset::{MapDataset, MapLayer};
    use crate::geometry::MapGeometry;
    use crate::projection::RadarProjection;
    use crate::residency::{Admission, GeometryResidency};
    use crate::scene::MapSceneController;
    use analyst_runtime::{Generation, GeometryCacheKey, LodBucket};
    use std::sync::Arc;

    /// KTLX (Twin Lakes, Oklahoma) as the radar itself reports it.
    ///
    /// Read out of the `RVOL` volume data block of a message 31 in the real
    /// archive file `KTLX20260817_165447_RT346_V06`, cached under
    /// `%LOCALAPPDATA%/FahrenheitResearch/RadarWorkstation/cache/level2-live`.
    /// That is the same field `install_loaded_volume` hands to
    /// `set_radar_anchor`, and it is `f32` in the message, so these are the
    /// exact bits the application anchors on. (ICD 2620002, Interface Control
    /// Document for the RDA/RPG, Table XVII-E: Volume Data Constant Type.)
    const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);

    /// The LOD buckets an operator actually works in: the default camera scale
    /// (0.35 km/point, the county band), the middle of the state band, and the
    /// country band.
    fn working_buckets() -> [LodBucket; 3] {
        [
            LodBucket::ideal(0.35, LOD_REFERENCE_KM_PER_POINT),
            LodBucket::ideal(3.0, LOD_REFERENCE_KM_PER_POINT),
            LodBucket::ideal(20.0, LOD_REFERENCE_KM_PER_POINT),
        ]
    }

    fn build(style: MapStyle, lod: LodBucket) -> MapGeometry {
        build_geometry(&MapBuildRequest {
            key: GeometryCacheKey {
                dataset: Generation::new(1),
                projection: Generation::new(1),
                style: Generation::new(1),
                lod,
            },
            dataset: MapDataset::from_generated(Generation::new(1)),
            projection: RadarProjection::new(KTLX.0, KTLX.1),
            style,
        })
    }

    /// The bytes the vertex buffer carries, per drawn layer.
    ///
    /// Taken from the vertices the draw actually indexes, which is the exact
    /// `Unorm8x4` the shader reads - not from the style struct, because the
    /// whole question is whether the style survives the trip.
    /// One drawn layer as the GPU receives it: which layer, the exact
    /// `Unorm8x4` bytes, and the stroke width in screen pixels.
    type EmittedLine = (MapLayer, [u8; 4], f32);
    /// Every line one build emitted, in paint order.
    type EmittedFrame = Vec<EmittedLine>;

    fn emitted_colors(geometry: &MapGeometry) -> EmittedFrame {
        geometry
            .draws
            .iter()
            .map(|draw| {
                let first = geometry.indices[draw.index_start as usize] as usize;
                let color = geometry.vertices[first].color;
                // Every vertex in a draw carries the layer's colour; check it
                // rather than trust the first one.
                for offset in 0..draw.index_count as usize {
                    let index = geometry.indices[draw.index_start as usize + offset] as usize;
                    assert_eq!(
                        geometry.vertices[index].color, color,
                        "a draw range mixed two colours"
                    );
                }
                (
                    draw.layer,
                    color,
                    geometry.vertices[first].half_width_px * 2.0,
                )
            })
            .collect()
    }

    /// The measurement the bug report needed: what each preset puts in the
    /// vertex buffer and in the chrome, at the zooms an operator uses.
    ///
    /// Run with `--nocapture` to read the table.
    #[test]
    fn every_preset_reaches_the_gpu_as_different_bytes() {
        let mut per_preset: Vec<(MapStylePreset, Vec<EmittedFrame>)> = Vec::new();
        for preset in MapStylePreset::ALL {
            let chrome = preset.chrome();
            println!(
                "{:<14} canvas {:?}  label_ink {:?}  label_halo {:?}",
                preset.id(),
                chrome.canvas.to_rgba8(),
                chrome.label_ink.to_rgba8(),
                chrome.label_halo.to_rgba8(),
            );
            let mut rungs = Vec::new();
            for lod in working_buckets() {
                let geometry = build(preset.style(), lod);
                let colors = emitted_colors(&geometry);
                assert!(
                    !colors.is_empty(),
                    "{} built nothing at LOD {lod:?} over Oklahoma",
                    preset.id()
                );
                for (layer, color, width) in &colors {
                    println!(
                        "  lod {:>3} {:>7}: rgba{:?} width {width} px  ({} vertices)",
                        lod.0,
                        layer.label(),
                        color,
                        geometry.vertex_count(),
                    );
                }
                rungs.push(colors);
            }
            per_preset.push((preset, rungs));
        }

        // Slate is the shipped look and must be unmoved: these are the bytes
        // `MapStyle::default` produces through `pack_color`, which rounds
        // rather than truncates.
        let slate = &per_preset[0];
        assert_eq!(slate.0, MapStylePreset::Slate);
        assert_eq!(
            slate.1[0],
            vec![
                // Mexico's states are inside the 1 000 km build region from
                // Norman, so the foreign layer is drawn here too.
                (MapLayer::ForeignAdmin, [133, 148, 173, 217], 1.2),
                (MapLayer::County, [107, 120, 140, 217], 0.9),
            ],
            "the shipped close-zoom lines moved"
        );
        assert_eq!(
            slate.1[1],
            vec![
                (MapLayer::ForeignAdmin, [133, 148, 173, 217], 1.2),
                (MapLayer::State, [133, 148, 173, 217], 1.2),
            ],
            "the shipped state line moved"
        );
        assert_eq!(
            slate.1[2],
            vec![(MapLayer::Country, [158, 173, 199, 242], 1.6)],
            "the shipped country line moved"
        );

        // And every preset differs from every other, in the buffer, at every
        // rung - not merely in the struct.
        for (index, (preset, mine)) in per_preset.iter().enumerate() {
            for (other, theirs) in per_preset.iter().skip(index + 1) {
                for (rung, (a, b)) in mine.iter().zip(theirs.iter()).enumerate() {
                    assert_ne!(
                        a,
                        b,
                        "{} and {} put identical bytes in the vertex buffer at rung {rung}",
                        preset.id(),
                        other.id()
                    );
                }
            }
        }
    }

    /// The whole chain the picker drives, run for real: pick a preset, and the
    /// controller must hand the pane a *new* key whose geometry carries the new
    /// colours, and the GPU residency must call that an upload rather than a
    /// hit.
    ///
    /// `GeometryResidency::touch` is the exact decision
    /// `MapRenderResources::ensure_resident` acts on: `Admitted` is the branch
    /// that writes new vertex buffers, `AlreadyResident` is the branch that
    /// draws the buffers already there. A style that reached the CPU but not
    /// the GPU would show up here as `AlreadyResident`.
    #[test]
    fn choosing_a_preset_rebuilds_the_geometry_and_reuploads_it() {
        let mut controller = MapSceneController::new(|| {});
        assert!(controller.set_radar_anchor(KTLX.0, KTLX.1));
        let mut residency = GeometryResidency::default();
        let mut seen_keys: Vec<GeometryCacheKey> = Vec::new();
        let mut seen_colors: Vec<EmittedFrame> = Vec::new();

        for preset in MapStylePreset::ALL {
            controller.set_style(preset.style());
            assert_eq!(
                controller.style(),
                preset.style(),
                "{} did not stick",
                preset.id()
            );
            assert_eq!(
                MapStylePreset::for_style(controller.style()),
                Some(preset),
                "the picker would show the wrong selection for {}",
                preset.id()
            );

            let key = controller.key_for_pane(0, 0.35).expect("anchored");
            assert!(
                !seen_keys.contains(&key),
                "{} reused a key the pane already has geometry for",
                preset.id()
            );
            seen_keys.push(key);

            // Drive the real build worker rather than calling build_geometry
            // directly, so the request/poll path is what is under test.
            assert!(controller.request(key), "{} queued nothing", preset.id());
            let geometry = settle(&mut controller, key);
            assert_eq!(geometry.key, key);

            // What the GPU would do with it.
            let admission = residency.touch(key, geometry.estimated_bytes);
            assert!(
                matches!(admission, Admission::Admitted { .. }),
                "{} would not have been uploaded: {admission:?}",
                preset.id()
            );
            // ...and a repeat frame at the same style must not re-upload, which
            // is the property a pan depends on.
            assert_eq!(
                residency.touch(key, geometry.estimated_bytes),
                Admission::AlreadyResident
            );

            let colors = emitted_colors(&geometry);
            assert!(
                !seen_colors.contains(&colors),
                "{} produced vertex colours identical to an earlier preset",
                preset.id()
            );
            println!("{:<14} {key:?} -> {colors:?}", preset.id());
            seen_colors.push(colors);
        }

        assert_eq!(residency.metrics().uploads, MapStylePreset::COUNT as u64);
        assert_eq!(residency.metrics().hits, MapStylePreset::COUNT as u64);
    }

    /// Re-picking the look already in force must cost nothing: `set_style`
    /// returns early on an equal style, so the resident buffers survive and the
    /// pane keeps drawing without a rebuild.
    #[test]
    fn re_picking_the_current_look_does_not_throw_the_map_away() {
        let mut controller = MapSceneController::new(|| {});
        controller.set_radar_anchor(KTLX.0, KTLX.1);
        controller.set_style(MapStylePreset::HighContrast.style());
        let key = controller.key_for_pane(0, 0.35).expect("anchored");
        controller.request(key);
        settle(&mut controller, key);

        for _ in 0..100 {
            controller.set_style(MapStylePreset::HighContrast.style());
            assert_eq!(
                controller.key_for_pane(0, 0.35),
                Some(key),
                "an identical style changed the key"
            );
            assert!(controller.geometry(&key).is_some(), "geometry was dropped");
        }
        assert_eq!(controller.metrics().build_requests, 1);
    }

    /// The chrome a pane paints is the chrome the picker chose, for every
    /// preset - and an unrecognised style still gets the shipped ground rather
    /// than nothing.
    #[test]
    fn chrome_follows_the_style_the_controller_is_holding() {
        let mut controller = MapSceneController::new(|| {});
        for preset in MapStylePreset::ALL {
            controller.set_style(preset.style());
            assert_eq!(
                MapChrome::for_style(controller.style()),
                preset.chrome(),
                "{} would paint another look's chrome",
                preset.id()
            );
        }

        let mut hand_written = MapStyle::default();
        hand_written.county.width_px += 0.01;
        assert_eq!(MapStylePreset::for_style(hand_written), None);
        assert_eq!(MapChrome::for_style(hand_written), MapChrome::default());
        assert_eq!(MapChrome::default(), MapStylePreset::Slate.chrome());
    }

    fn settle(controller: &mut MapSceneController, key: GeometryCacheKey) -> Arc<MapGeometry> {
        for _ in 0..600 {
            controller.poll();
            if let Some(geometry) = controller.geometry(&key) {
                return geometry;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("geometry for {key:?} never arrived");
    }
}

/// The last link: a real GPU, drawing the real basemap, with the pixels read
/// back.
///
/// Everything above this point proves the bytes are correct in memory. This
/// runs `MapPaintCallback` - the same `prepare` that decides whether to upload
/// and the same `paint` that binds the buffers - against a headless wgpu
/// device, into an offscreen target, once per preset through ONE
/// `MapRenderResources`, exactly as the application does across frames. A style
/// that changed on the CPU but left a stale vertex buffer bound would come back
/// here as two identical images.
///
/// Skipped, loudly, when the machine has no adapter, so a headless build node
/// still passes without pretending it measured anything.
#[cfg(test)]
mod painted_by_the_gpu {
    use super::*;
    use crate::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
    use crate::dataset::MapDataset;
    use crate::geometry::MapGeometry;
    use crate::gpu::{MapPaintCallback, MapRenderResources};
    use crate::projection::RadarProjection;
    use analyst_runtime::{Camera2D, Generation, GeometryCacheKey, LodBucket, ViewportMetrics};
    use eframe::egui;
    use eframe::egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
    use eframe::wgpu;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    /// Square pane. 512 * 4 bytes is a multiple of the 256-byte row alignment
    /// `copy_texture_to_buffer` requires, so the readback needs no padding
    /// arithmetic.
    const SIDE: u32 = 512;
    /// The same real KTLX position the CPU-side proof uses.
    const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);

    /// Drive a future to completion on this thread.
    ///
    /// wgpu's native backends resolve `request_adapter` and `request_device`
    /// without ever needing to be woken, so a no-op waker is sufficient and
    /// the crate does not have to take an executor dependency to run one test.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
            std::thread::yield_now();
        }
    }

    /// One preset's frame: the pane ground, and the pixels the map drew on it.
    struct Frame {
        pixels: Vec<u8>,
        canvas: [u8; 4],
        line_pixels: usize,
    }

    impl Frame {
        /// Mean colour of everything that is not the untouched ground, which is
        /// what the basemap contributed to the picture.
        fn mean_line_color(&self) -> [u8; 4] {
            let mut sums = [0_u64; 4];
            let mut count = 0_u64;
            for pixel in self.pixels.chunks_exact(4) {
                if pixel == self.canvas {
                    continue;
                }
                for (sum, channel) in sums.iter_mut().zip(pixel) {
                    *sum += u64::from(*channel);
                }
                count += 1;
            }
            if count == 0 {
                return self.canvas;
            }
            sums.map(|sum| (sum / count) as u8)
        }
    }

    fn geometry_for(style: MapStyle, lod: LodBucket, style_generation: u64) -> Arc<MapGeometry> {
        Arc::new(build_geometry(&MapBuildRequest {
            key: GeometryCacheKey {
                dataset: Generation::new(1),
                projection: Generation::new(1),
                style: Generation::new(style_generation),
                lod,
            },
            dataset: MapDataset::from_generated(Generation::new(1)),
            projection: RadarProjection::new(KTLX.0, KTLX.1),
            style,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resources: &mut CallbackResources,
        target: &wgpu::Texture,
        view: &wgpu::TextureView,
        readback: &wgpu::Buffer,
        geometry: Arc<MapGeometry>,
        canvas: LayerColor,
    ) -> Frame {
        let viewport = ViewportMetrics {
            width_points: SIDE as f32,
            height_points: SIDE as f32,
            pixels_per_point: 1.0,
        };
        let callback = MapPaintCallback {
            pane_index: 0,
            geometry,
            camera: Camera2D::default(),
            viewport,
            rect_px: [0.0, 0.0, SIDE as f32, SIDE as f32],
        };
        let screen = ScreenDescriptor {
            size_in_pixels: [SIDE, SIDE],
            pixels_per_point: 1.0,
        };

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("map style proof"),
        });
        // The upload decision, run for real.
        let extra = callback.prepare(device, queue, &screen, &mut encoder, resources);
        assert!(extra.is_empty(), "the map callback queued command buffers");

        {
            // The pane's own `rect_filled(rect, chrome.canvas)` becomes the
            // clear here: same ground, one draw earlier.
            let [red, green, blue, alpha] = canvas.to_array().map(f64::from);
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("map style proof pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: red,
                                g: green,
                                b: blue,
                                a: alpha,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            let whole = egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(SIDE as f32, SIDE as f32),
            );
            let info = egui::PaintCallbackInfo {
                viewport: whole,
                clip_rect: whole,
                pixels_per_point: 1.0,
                screen_size_px: [SIDE, SIDE],
            };
            // Copied from `egui_wgpu::Renderer::render`, which sets the
            // viewport from the callback rect before handing over the pass.
            let pixels = info.viewport_in_pixels();
            pass.set_viewport(
                pixels.left_px as f32,
                pixels.top_px as f32,
                pixels.width_px as f32,
                pixels.height_px as f32,
                0.0,
                1.0,
            );
            callback.paint(info, &mut pass, resources);
        }

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIDE * 4),
                    rows_per_image: Some(SIDE),
                },
            },
            wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll");
        receiver.recv().expect("map callback").expect("map read");
        let pixels = slice.get_mapped_range().to_vec();
        readback.unmap();

        // The top-left pixel is outside every line at this camera, so it is the
        // untouched ground and the honest definition of "not drawn on".
        let canvas: [u8; 4] = pixels[..4].try_into().expect("rgba");
        let line_pixels = pixels
            .chunks_exact(4)
            .filter(|pixel| *pixel != canvas)
            .count();
        Frame {
            pixels,
            canvas,
            line_pixels,
        }
    }

    /// Four presets, four frames, one device, one `MapRenderResources`: the
    /// pictures must differ.
    #[test]
    fn every_preset_renders_a_different_picture() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) =
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            eprintln!(
                "SKIPPED every_preset_renders_a_different_picture: no wgpu adapter on this \
                 machine, so the GPU half of the basemap style chain is UNPROVEN here"
            );
            return;
        };
        println!("adapter: {:?}", adapter.get_info());
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("map style proof device"),
            ..Default::default()
        }))
        .expect("headless device");

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("map style proof target"),
            size: wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("map style proof readback"),
            size: u64::from(SIDE) * u64::from(SIDE) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut resources = CallbackResources::default();
        resources.insert(MapRenderResources::new(&device, format));

        // The default camera: 0.35 km/point, which is the county band.
        let lod = LodBucket::ideal(Camera2D::default().km_per_point, LOD_REFERENCE_KM_PER_POINT);

        let mut frames = Vec::new();
        for (index, preset) in MapStylePreset::ALL.into_iter().enumerate() {
            // A fresh style generation per preset, exactly as
            // `MapSceneController::set_style` produces.
            let geometry = geometry_for(preset.style(), lod, index as u64 + 1);
            let frame = render(
                &device,
                &queue,
                &mut resources,
                &target,
                &view,
                &readback,
                geometry,
                preset.chrome().canvas,
            );
            println!(
                "{:<14} ground {:?}  {} line pixels of {}  mean line colour {:?}",
                preset.id(),
                frame.canvas,
                frame.line_pixels,
                SIDE * SIDE,
                frame.mean_line_color(),
            );
            assert_eq!(
                frame.canvas,
                preset.chrome().canvas.to_rgba8(),
                "{} did not clear to its own ground",
                preset.id()
            );
            assert!(
                frame.line_pixels > 1_000,
                "{} drew almost nothing over Oklahoma: {} pixels",
                preset.id(),
                frame.line_pixels
            );
            frames.push((preset, frame));
        }

        for (index, (preset, mine)) in frames.iter().enumerate() {
            for (other, theirs) in frames.iter().skip(index + 1) {
                assert_ne!(
                    mine.pixels,
                    theirs.pixels,
                    "{} and {} rendered identical images",
                    preset.id(),
                    other.id()
                );
                // Not merely one pixel apart: the ink itself has to differ, or
                // the two looks are the same look drawn twice.
                let (a, b) = (mine.mean_line_color(), theirs.mean_line_color());
                let separation = (0..3)
                    .map(|channel| (i32::from(a[channel]) - i32::from(b[channel])).abs())
                    .max()
                    .unwrap_or(0);
                assert!(
                    separation >= 8,
                    "{} and {} drew the same ink: {a:?} against {b:?}",
                    preset.id(),
                    other.id()
                );
            }
        }
    }
}
