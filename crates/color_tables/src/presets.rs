//! The palettes that ship with the application.
//!
//! Split out of `lib.rs` so the machinery that samples a table and the
//! catalogue of tables can be read apart. Everything here is re-exported at the
//! crate root, so `color_tables::builtin_velocity_table()` still resolves.

use crate::{ColorStop, ColorTable, ColorTableFamily, TableRendering, clear_stop, stop};

// ---------------------------------------------------------------------------
// Built-in palettes
//
// Every constructor below is built through one of the three helpers here, so
// two things hold for every built-in without anyone having to remember them.
//
// It is validated at construction and panics carrying its own name if it is
// not, rather than silently painting nothing at the first gate that needs it.
//
// And its name ends with its own sampling mode. That matters because the name
// is the only string a picker row shows. A stepped table paints flat bands and
// an interpolated one paints a gradient; those are wildly different pictures of
// the same field, and a list reading "GR2Analyst Classic REF" beside "Smooth
// Classic REF" gives an analyst no way to tell which is which. Reading
// "GR2Analyst Classic REF (quantized stepped)" beside "Smooth Classic REF
// (interpolated)" answers it with no hover and no click.
// ---------------------------------------------------------------------------

/// Stamp a table's own sampling mode onto the end of its name.
///
/// The wording is `SampleMode::label`'s, the same string `sample_mode_label`
/// reports, so a name and a tooltip can never disagree about one table.
///
/// Read off the built table rather than off whatever mode was asked for:
/// `parse_stepped` only sets a *default*, and a palette carrying `step:` or
/// `mode:` overrides it. Most of the parsed built-ins do exactly that.
fn named_by_sample_mode(table: ColorTable) -> ColorTable {
    let name = format!("{} ({})", table.name(), table.sample_mode_label());
    table.renamed(name)
}

/// A palette written in the GR/RadarScope text format, sampled as its own
/// header asks.
fn parsed_preset(name: &str, text: &str) -> ColorTable {
    named_by_sample_mode(
        ColorTable::parse_stepped(name, text)
            .unwrap_or_else(|error| panic!("built-in palette {name} is invalid: {error}")),
    )
}

/// A palette given as stops, sampled by interpolating between them.
fn smooth_preset(name: &str, stops: Vec<ColorStop>) -> ColorTable {
    named_by_sample_mode(
        ColorTable::new(name, stops)
            .unwrap_or_else(|error| panic!("built-in palette {name} is invalid: {error}")),
    )
}

/// A palette given as stops, sampled as one flat band per stop interval.
fn banded_preset(name: &str, stops: Vec<ColorStop>) -> ColorTable {
    named_by_sample_mode(
        ColorTable::new_stepped(name, stops)
            .unwrap_or_else(|error| panic!("built-in palette {name} is invalid: {error}")),
    )
}

// ---------------------------------------------------------------------------
// The shipped defaults
//
// Both used to be the hard-banded rendering of their palette, and both moved.
// The palettes did not: `builtin_reflectivity_table` is still GR2Analyst
// Classic REF and `builtin_velocity_table` is still Analyst Tornado VEL, stop
// for stop and colour for colour. What changed is that they now open in the
// continuous rendering, which is what those same stops look like when nothing
// rounds the gate's value onto a grid before the colour is looked up.
//
// Measured on real Level II volumes. Two numbers per palette: how many colours
// it maps the volume's own distinct readings onto, and what share of adjacent
// gate pairs that carry *different* readings it paints the same colour anyway.
// The second is taken over a uniform sample of the lowest sweep, with pairs the
// palette leaves clear at both ends dropped, since those are the clear-air mask
// doing its job rather than two readings blended into one.
//
//   KABR 2026-08-18 06:43:14Z
//     reflectivity, 197 distinct values, 139,416 pairs
//       banded      15 colours, 26.0% of differing pairs painted alike
//       continuous 120 colours,  0.0%
//     velocity, 115 distinct values, 200,000 pairs
//       banded      29 colours, 24.3% painted alike
//       continuous 115 colours,  0.0%
//   KDMX 2026-08-18 08:34:01Z
//     reflectivity, 207 distinct values, 105,801 pairs
//       banded      15 colours, 26.4%      continuous 124 colours, 0.0%
//     velocity, 131 distinct values, 200,000 pairs
//       banded      35 colours, 27.1%      continuous 131 colours, 0.0%
//   KARX 2026-08-18 20:35:12Z
//     reflectivity, 190 distinct values, 141,037 pairs
//       banded      13 colours, 36.0%      continuous 115 colours, 0.0%
//     velocity, 139 distinct values, 200,000 pairs
//       banded      35 colours, 40.7%      continuous 139 colours, 0.0%
//   KEAX 2026-08-18 17:56:38Z
//     reflectivity, 175 distinct values, 98,822 pairs
//       banded      11 colours, 28.3%      continuous  92 colours, 0.0%
//     velocity, 115 distinct values, 200,000 pairs
//       banded      30 colours, 33.1%      continuous 115 colours, 0.0%
//
// A quarter to two fifths of every adjacent pair of gates that disagreed about
// the wind was being drawn as if it agreed. That is the whole of "everything
// just blends together".
//
// The banded rendering of either palette is one flip away and paints exactly
// what it painted before: `the_default_reflectivity_table_still_paints_exactly_
// what_it_did` and its velocity twin hold the hand-checked probes, and
// `flipping_a_palettes_rendering_and_flipping_it_back_returns_the_palette`
// holds the round trip. Off-line, every one of the twenty-seven pre-existing
// banded constructors was compared against the code as it stood before this
// change over 28,001 densely spaced values each, and no byte moved.
// ---------------------------------------------------------------------------

pub fn builtin_reflectivity_table() -> ColorTable {
    awips_wilson_reflectivity_table().rendered(TableRendering::Smooth)
}

/// The NCAR/LROSE `dbmlow.colors` table used by the DOW6 and DOW7 DBM
/// products, translated from its X11 colour names to their standard RGB
/// values. The source is an operational display definition, not a palette
/// inferred from the product name:
/// <https://github.com/NCAR/lrose-displays/blob/master/color_scales/dbmlow.colors>.
pub fn builtin_received_power_table() -> ColorTable {
    banded_preset(
        "NCAR DOW Received Power",
        vec![
            stop(-120.0, 77, 77, 77),
            stop(-110.0, 176, 48, 96),
            stop(-105.0, 153, 50, 204),
            stop(-100.0, 0, 0, 255),
            stop(-95.0, 65, 105, 225),
            stop(-90.0, 0, 191, 255),
            stop(-85.0, 0, 250, 154),
            stop(-80.0, 34, 139, 34),
            stop(-75.0, 190, 190, 190),
            stop(-70.0, 238, 154, 73),
            stop(-60.0, 255, 215, 0),
            stop(-50.0, 255, 255, 0),
            stop(-40.0, 255, 140, 105),
            stop(-30.0, 255, 99, 71),
            stop(-20.0, 255, 64, 64),
            stop(-10.0, 255, 0, 0),
            stop(0.0, 178, 34, 34),
            stop(20.0, 178, 34, 34),
        ],
    )
}

pub fn builtin_velocity_table() -> ColorTable {
    generic_radar_velocity_table().rendered(TableRendering::Smooth)
}

/// The default velocity look: the classic WDT/RadarScope velocity ramp (WDT =
/// Weather Decision Technologies, RadarScope's publisher), via the BowEcho
/// preset of the same ramp.
///
/// The source table is a GR `.pal` with two-colour ramp entries - each row's
/// second triple is the colour the segment reaches just before the next row -
/// and the text below is that file, row for row. It used to be a hand
/// expansion of it, one stop per endpoint with a 0.01 kt break between
/// segments, because the parser dropped second colours; the parser reads them
/// now, so the expansion is gone and with it the sliver of wrong colour each
/// break carried. `the_genericradar_velocity_port_matches_the_gr_pal_original`
/// holds the table to an independent sampler of the same text, exactly.
///
/// Values are knots in the text; the `scale:` header converts them to m/s at
/// parse time, exactly as the original did.
pub fn generic_radar_velocity_table() -> ColorTable {
    parsed_preset("GenericRadar VEL", GENERIC_RADAR_VELOCITY_TABLE)
}

/// The default reflectivity look: the AWIPS colour table by Karl Schneider
/// ("Wilson edit"), via the BowEcho preset. Two-colour ramp rows like
/// [`generic_radar_velocity_table`], pinned the same way by
/// `the_wilson_reflectivity_port_matches_the_gr_pal_original`.
pub fn awips_wilson_reflectivity_table() -> ColorTable {
    parsed_preset("AWIPS Wilson REF", AWIPS_WILSON_REFLECTIVITY_TABLE)
}

pub fn tornado_velocity_table() -> ColorTable {
    parsed_preset("Analyst Tornado VEL", TORNADO_VELOCITY_TABLE)
}

pub fn vortex_velocity_table() -> ColorTable {
    parsed_preset("WxTools Vortex Velo", VORTEX_VELO_TABLE)
}

/// The catalogue: every palette this build ships for one family, defaults
/// first, each in the rendering it was authored in.
///
/// The head of each list is that family's default, which
/// `every_family_default_is_the_first_table_the_picker_offers` pins.
///
/// This is a catalogue and not a menu. It says which *palettes* exist, not how
/// they should be drawn - a picker wants [`palette_offers_for_family`], which
/// puts every one of them into the rendering the analyst is actually using.
/// The distinction matters because a palette and its sampling stopped being
/// the same thing: `Smooth Classic REF` and `GR2Analyst Classic REF` are two
/// different colour schemes and both belong here, whereas the banded and
/// continuous drawings of either one are the same entry seen through the
/// switch.
pub fn builtin_tables_for_family(family: ColorTableFamily) -> Vec<ColorTable> {
    match family {
        ColorTableFamily::Reflectivity => vec![
            builtin_reflectivity_table(),
            gr2_reflectivity_table(),
            smooth_classic_reflectivity_table(),
            smooth_sequential_reflectivity_table(),
            smooth_storm_core_reflectivity_table(),
            analyst_classic_reflectivity_table(),
            nws_reflectivity_table(),
            dark_scope_reflectivity_table(),
            hail_core_reflectivity_table(),
            low_precip_reflectivity_table(),
            tornado_debris_reflectivity_table(),
            clean_light_reflectivity_table(),
        ],
        ColorTableFamily::ReceivedPower => vec![builtin_received_power_table()],
        ColorTableFamily::Velocity => vec![
            builtin_velocity_table(),
            tornado_velocity_table(),
            smooth_doppler_velocity_table(),
            smooth_couplet_velocity_table(),
            analyst_velocity_table(),
            radarscope_contrast_velocity_table(),
            sign_check_velocity_table(),
            couplet_pop_velocity_table(),
            gr2_ish_analyst_velocity_table(),
            subtle_srv_velocity_table(),
        ],
        ColorTableFamily::SpectrumWidth => vec![
            builtin_spectrum_width_table(),
            turbulence_spectrum_width_table(),
            clear_air_spectrum_width_table(),
            spectrum_width_class_bands_table(),
        ],
        ColorTableFamily::DifferentialReflectivity => vec![
            builtin_differential_reflectivity_table(),
            storm_interrogation_differential_reflectivity_table(),
            zdr_column_hunter_table(),
            hail_signal_differential_reflectivity_table(),
        ],
        ColorTableFamily::CorrelationCoefficient => vec![
            builtin_correlation_coefficient_table(),
            debris_hunter_correlation_coefficient_table(),
            melting_layer_correlation_coefficient_table(),
            correlation_coefficient_class_bands_table(),
        ],
        ColorTableFamily::DifferentialPhase => vec![
            builtin_differential_phase_table(),
            twilight_cyclic_differential_phase_table(),
            phase_bands_differential_phase_table(),
        ],
        ColorTableFamily::SpecificDifferentialPhase => vec![
            builtin_specific_differential_phase_table(),
            heavy_rain_specific_differential_phase_table(),
            fine_detail_specific_differential_phase_table(),
        ],
        ColorTableFamily::Generic => vec![builtin_generic_table()],
    }
}

/// Whether a palette base name is one this build ships for a family.
///
/// The single fact "edit this table" turns on, answered in the crate that owns
/// the catalogue so that the picker, the settings page and the colour table
/// editor cannot hold three opinions about it. Keyed on
/// [`ColorTable::base_name`] rather than on the full name, because the
/// rendering suffix is not part of which palette a table is.
pub fn is_builtin_table(family: ColorTableFamily, base_name: &str) -> bool {
    builtin_tables_for_family(family)
        .iter()
        .any(|table| table.base_name() == base_name)
}

/// The family that ships a palette going by this name, if any family does.
///
/// [`is_builtin_table`] asked of every family at once, and asked of a name an
/// analyst has typed rather than of a base name a table already carries - so
/// the name is reduced with [`crate::base_name_of`] first. That reduction is
/// the whole reason this is not just a loop at the call site: a picker row is
/// labelled `"AWIPS Wilson REF (interpolated)"`, a stored choice is written as
/// `"AWIPS Wilson REF"`, and both of those are the same shipped palette. Both
/// forms answer here.
///
/// Asked by a *writer*, which is why it exists. The shipped catalogue is
/// searched before the analyst's own directory when a palette choice is
/// restored at launch (see the settings restore path), so a file that declares
/// a shipped palette's name is a file the application will never install and
/// the picker will never offer: the row for that name draws Edit on the
/// preset, and the analyst's own table is reachable only through the
/// filesystem. The colour table editor asks this before it writes and refuses
/// the name, for the same reason it refuses a rendering suffix - the file
/// would be perfect and the palette would be gone at the next launch with
/// nothing said.
///
/// Every family, not just the one being saved into, because the editor's
/// Measurement control moves a table between families after the fact: a name
/// that is safe in one family today is a shipped name the moment the analyst
/// changes what the table measures.
///
/// The catalogue is built once. Every constructor in this module parses or
/// validates its palette, and this is asked once per frame while the editor's
/// window is open.
pub fn builtin_family_for_name(name: &str) -> Option<ColorTableFamily> {
    let wanted = crate::base_name_of(name);
    builtin_name_index()
        .iter()
        .find(|(shipped, _)| shipped == wanted)
        .map(|(_, family)| *family)
}

/// [`builtin_family_for_name`] narrowed to one family: whether *this* family
/// ships a palette going by this name.
///
/// [`is_builtin_table`] answers the same question, and is what a caller
/// holding a table's own base name should use. This one takes a name an
/// analyst has typed, reduces it with [`crate::base_name_of`] the way
/// `builtin_family_for_name` does, and reads the cached index rather than
/// re-parsing a family's catalogue - which matters because the colour table
/// folder scanner asks it once per file in the folder.
pub fn builtin_family_ships_name(family: ColorTableFamily, name: &str) -> bool {
    let wanted = crate::base_name_of(name);
    builtin_name_index()
        .iter()
        .any(|(shipped, shipping)| shipped == wanted && *shipping == family)
}

/// Every shipped palette's base name with the family that ships it, built
/// once.
///
/// Every constructor in this module parses or validates its palette, and these
/// questions are asked once per frame while the editor's window is open and
/// once per file while the colour table folder is being scanned.
fn builtin_name_index() -> &'static [(String, ColorTableFamily)] {
    static NAMES: std::sync::OnceLock<Vec<(String, ColorTableFamily)>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = Vec::new();
        for family in ColorTableFamily::ALL {
            for table in builtin_tables_for_family(family) {
                names.push((table.base_name().to_owned(), family));
            }
        }
        names
    })
}

/// The rows a picker should draw for one family, given what is installed in it.
///
/// This is [`builtin_tables_for_family`] with the analyst's sampling choice
/// applied, plus one extra row at the end: the installed palette drawn the
/// other way. That last row *is* the smooth/stepped switch. It is a row and not
/// a separate widget for one reason - it costs the caller nothing. A picker
/// that already draws a list of palettes and installs whichever one is clicked
/// keeps doing exactly that, and the switch arrives with no new state to
/// persist, no new keyboard handling, and no second control to keep in sync
/// with the first.
///
/// The rendering comes off the installed table rather than out of a setting,
/// which is what makes the choice survive a palette change: pick a different
/// palette while on smooth and the new one arrives smooth.
///
/// Names stay unique across the returned list, because the flipped row carries
/// its own mode in its name and every other row carries the installed one. A
/// caller that identifies a row by [`ColorTable::name`] - which is what the two
/// existing pickers do - therefore needs no change at all beyond calling this.
///
/// The installed palette is included even when it is not in the catalogue, so
/// a table an analyst loaded from a file does not vanish from its own list.
pub fn palette_offers_for_family(
    family: ColorTableFamily,
    installed: &ColorTable,
) -> Vec<ColorTable> {
    let rendering = installed.rendering();
    let mut offers: Vec<ColorTable> = builtin_tables_for_family(family)
        .into_iter()
        .map(|table| table.rendered(rendering))
        .collect();
    if !offers
        .iter()
        .any(|table| table.base_name() == installed.base_name())
    {
        offers.push(installed.clone());
    }
    let flipped = installed.rendered(rendering.flipped());
    if !offers.iter().any(|table| table.name() == flipped.name()) {
        offers.push(flipped);
    }
    offers
}

pub fn analyst_reflectivity_table() -> ColorTable {
    banded_preset(
        "Analyst High Contrast REF",
        vec![
            stop(-10.0, 5, 8, 18),
            stop(0.0, 18, 36, 76),
            stop(7.5, 23, 92, 157),
            stop(15.0, 26, 158, 191),
            stop(22.5, 17, 146, 62),
            stop(30.0, 84, 188, 54),
            stop(37.5, 242, 216, 47),
            stop(45.0, 239, 120, 34),
            stop(52.5, 221, 42, 38),
            stop(60.0, 174, 32, 112),
            stop(67.5, 214, 76, 218),
            stop(75.0, 245, 245, 245),
        ],
    )
}

pub fn nws_reflectivity_table() -> ColorTable {
    parsed_preset("NWS Classic REF", NWS_CLASSIC_REFLECTIVITY_TABLE)
}

pub fn analyst_classic_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Classic REF", ANALYST_CLASSIC_REFLECTIVITY_TABLE)
}

pub fn gr2_reflectivity_table() -> ColorTable {
    parsed_preset("GR2Analyst Classic REF", GR2_REFLECTIVITY_TABLE)
}

pub fn storm_detail_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Storm Detail REF", STORM_DETAIL_REFLECTIVITY_TABLE)
}

pub fn hail_core_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Hail Core REF", HAIL_CORE_REFLECTIVITY_TABLE)
}

pub fn low_precip_reflectivity_table() -> ColorTable {
    parsed_preset("Analyst Low Precip REF", LOW_PRECIP_REFLECTIVITY_TABLE)
}

pub fn dark_scope_reflectivity_table() -> ColorTable {
    parsed_preset("Dark Scope REF", DARK_SCOPE_REFLECTIVITY_TABLE)
}

pub fn tornado_debris_reflectivity_table() -> ColorTable {
    parsed_preset("Tornado Debris REF", TORNADO_DEBRIS_REFLECTIVITY_TABLE)
}

pub fn clean_light_reflectivity_table() -> ColorTable {
    parsed_preset("Clean Light REF", CLEAN_LIGHT_REFLECTIVITY_TABLE)
}

// ---------------------------------------------------------------------------
// Continuously interpolated reflectivity
//
// Every reflectivity preset above this comment is stepped: each carries a
// `step:` row, so `sample` quantises the gate's dBZ onto a 2.5 or 5 dBZ grid
// before it looks up a colour. Inside a bin every gate paints the identical
// colour, which is why the display draws flat plateaus with hard edges between
// them. That is a deliberate reading aid - the edges are contours of constant
// reflectivity, and an analyst can count them - but it is also indistinguishable
// from a renderer that cannot interpolate. With nothing but stepped tables in
// the picker there was no way to tell the two apart from the scope.
//
// So these three are the same field seen the other way: no quantisation, and
// enough stops that the ramp reads as a gradient. Between them and the stepped
// presets, banding that survives a switch to an interpolated table is the
// renderer's, and banding that does not is the palette's.
//
// All three keep the four break points people actually read reflectivity by.
// Under the Marshall-Palmer relation Z = 200 R^1.6 (Marshall, J. S., and
// W. M. Palmer, 1948: "The distribution of raindrops with size", J. Meteor., 5,
// 165-166, doi:10.1175/1520-0469(1948)005<0165:TDORWS>2.0.CO;2) those dBZ
// values are rain rates that mean something different from each other:
//
// * 20 dBZ -> 0.65 mm/h. Precipitation onset; below it is drizzle, cloud, and
//   the clear-air return of insects and dust (Fabry, F., 2015: "Radar
//   Meteorology: Principles and Practice", Cambridge Univ. Press,
//   doi:10.1017/CBO9781107707405, ch. 8).
// * 35 dBZ -> 5.6 mm/h. Moderate-to-heavy rain; in convection this is the edge
//   of the core.
// * 50 dBZ -> 49 mm/h. Torrential rain or hail. Near the 45-50 dBZ thresholds
//   the operational hail algorithms are built on (Waldvogel, A., B. Federer,
//   and P. Grimm, 1979: "Criteria for the detection of hail cells", J. Appl.
//   Meteor., 18, 1521-1525,
//   doi:10.1175/1520-0450(1979)018<1521:CFTDOH>2.0.CO;2; Witt, A., and
//   coauthors, 1998: "An enhanced hail detection algorithm for the WSR-88D",
//   Wea. Forecasting, 13, 286-303,
//   doi:10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2).
// * 65 dBZ -> 420 mm/h, which no rain shaft produces. At 65 dBZ the target is
//   large hail, and saying so is the whole point of the last band.
//
// A gradient that smears those four away is prettier than a stepped table and
// worse at the job, so all three place a half-dBZ turn on each one - one data
// step wide, since Level II reflectivity is quantised to 0.5 dBZ - and run a
// true gradient in between. The result is four thin contours where an analyst
// wants contours, and continuous tone everywhere else. Smooth Classic and
// Smooth Storm Core turn hue; Smooth Sequential steps luminance up, which is
// what lets it keep a monotone lightness ramp and still show the thresholds.
//
// All three also ink from 10 dBZ, exactly like the stepped presets, so
// switching between them changes how the echo is coloured. The half-dBZ alpha
// ramp below 10 dBZ is one Level II step wide; it exists because an
// interpolated table cannot have a discontinuity, not to fade anything in.
//
// On raw Level II values nothing lands inside that ramp: the 8-bit reflectivity
// word decodes as (raw - 66) / 2, so every value a radar can send sits exactly
// on the 0.5 dBZ grid, and 9.5 and 10.0 are both grid points. That is NOT true
// once the display passes run. `render2d::smooth` (a 3x3 binomial on the polar
// lattice) and `render2d::interpolate` (bilinear inter-gate upsampling) both
// produce physical values off the 0.5 grid, and a NaN-aware [1 2 1] pass along
// range over KABR 2026-08-18 06:43:14Z put 74,932 of 3,799,008 gates strictly
// inside 9.5 < dBZ < 10.0, and over KDMX 2026-08-18 08:34:01Z 59,827 of
// 5,025,389 - one to two percent, all of them at the outer fringe of the echo.
// Those gates draw at partial alpha on these three tables and fully clear on
// every quantised preset, whose `sample` returns transparent below its first
// opaque stop outright. So in Soften or Interpolate display modes an echo edge
// is one half-dBZ softer here than on a stepped table; the interior, which is
// what the palette is being judged on, is unaffected.
// `the_interpolated_reflectivity_presets_ink_the_same_gates_as_the_stepped_ones`
// pins both halves of that: identical on the 0.5 dBZ grid, half-alpha off it.
//
// At the top they run to 95 rather than the stepped presets' 92.5, because
// that same encoding tops out at (255 - 66) / 2 = 94.5 dBZ. The last stop is
// past the last value the field can hold, so nothing is ever clamped onto it
// and the legend's upper bound is a number the data can actually reach.
// ---------------------------------------------------------------------------

/// The operational hue sequence, continuously interpolated.
///
/// Blue for light echo, green for rain, yellow through orange for heavy rain,
/// red for a core, magenta for hail, white for the top of the scale - the same
/// order every stepped preset above uses, so nothing an analyst has learned to
/// read moves. What changes is that the 15 dBZ of green between 20 and 35 are
/// now 15 dBZ of *varying* green, and a gradient inside a storm is visible
/// instead of quantised into three plateaus.
///
/// This is the table to switch to first when the question is whether the
/// banding on the scope belongs to the palette or to the renderer.
pub fn smooth_classic_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Smooth Classic REF",
        vec![
            clear_stop(-10.0),
            clear_stop(9.5),
            stop(10.0, 16, 88, 140),
            stop(12.5, 18, 118, 176),
            stop(15.0, 20, 148, 208),
            stop(17.5, 24, 178, 230),
            stop(19.5, 40, 208, 244),
            // 20 dBZ: precipitation onset. Blue gives way to green.
            stop(20.0, 14, 148, 60),
            stop(22.5, 16, 168, 58),
            stop(25.0, 20, 190, 58),
            stop(27.5, 40, 206, 56),
            stop(30.0, 64, 214, 55),
            stop(32.5, 92, 220, 54),
            stop(34.5, 120, 224, 52),
            // 35 dBZ: moderate-to-heavy rain. Green gives way to yellow.
            stop(35.0, 250, 228, 36),
            stop(37.5, 251, 212, 32),
            stop(40.0, 252, 196, 28),
            stop(42.5, 252, 178, 26),
            stop(45.0, 251, 160, 24),
            stop(47.5, 250, 140, 22),
            stop(49.5, 249, 124, 20),
            // 50 dBZ: the core. Amber gives way to red.
            stop(50.0, 230, 18, 26),
            stop(52.5, 216, 14, 26),
            stop(55.0, 198, 10, 26),
            stop(57.5, 180, 8, 28),
            stop(60.0, 160, 8, 30),
            stop(62.5, 142, 8, 34),
            stop(64.5, 126, 8, 38),
            // 65 dBZ: not rain. Red gives way to magenta.
            stop(65.0, 214, 40, 200),
            stop(67.5, 226, 86, 220),
            stop(70.0, 232, 130, 234),
            stop(72.5, 222, 168, 240),
            stop(75.0, 226, 200, 246),
            stop(80.0, 238, 226, 250),
            stop(85.0, 246, 240, 252),
            stop(95.0, 255, 255, 255),
        ],
    )
}

/// Reflectivity on a ramp whose lightness only ever increases.
///
/// The classic radar hue order is not monotone in lightness: yellow at 40 dBZ
/// is far lighter than the dark red at 60, so a storm's strongest gates are
/// *darker* than its moderate ones and the eye has to be told the order rather
/// than seeing it. Worse for the job in hand, a ramp that goes light-dark-light
/// hides small gradients wherever it doubles back.
///
/// This one is built the way the colour-map literature says a sequential scale
/// should be: lightness rising monotonically end to end, hue carrying the rest
/// of the information (Kovesi, P., 2015: "Good colour maps: how to design
/// them", arXiv:1509.03700; Crameri, F., G. E. Shephard, and P. J. Heron, 2020:
/// "The misuse of colour in science communication", Nat. Commun., 11, 5444,
/// doi:10.1038/s41467-020-19160-x). Lightness is the BT.709 relative luminance
/// 0.2126 R + 0.7152 G + 0.0722 B, and a test checks it rises at every one of
/// the 32 inked stops.
///
/// Two consequences worth knowing before reaching for it. Strongest is always
/// brightest, so a core reads as a peak in a relief map rather than as a colour
/// to be looked up. And because lightness never doubles back, the palette
/// cannot stall: on a stepped table a plateau might be the palette's bin or the
/// renderer's, and here there is nowhere for the palette to plateau.
///
/// The four break points are turns here, exactly as on Smooth Classic REF, and
/// this was got wrong the first time. The original stops moved 3-8 units of
/// colour across each break window - *less* than the 8 units the widest
/// ordinary half-dBZ window moved, and at 20 dBZ less than the window
/// immediately before it - so 49.5 dBZ and 50.5 dBZ were the same orange and
/// the core boundary was invisible. Measured on KABR 2026-08-18 06:43:14Z and
/// KDMX 2026-08-18 08:34:01Z, whose reflectivity fields between them hold 119
/// and 123 distinct dBZ values above 10 dBZ.
///
/// Monotone lightness does not require a smooth ramp, only a rising one, so
/// each break is now a step *up*: luminance jumps 15 to 40 units in the half
/// dBZ below 20, 35, 50 and 65, against 1 to 5 units for an ordinary half-dBZ
/// window. Every break window moves at least twelve times as far as the worst
/// non-break window, and `the_contour_tables_turn_at_the_four_breaks_and_glide_between_them`
/// holds all three interpolated presets to the same bar.
///
/// The hue story is unchanged: indigo at the bottom, violet from 20, red from
/// 35, amber from 50, near-white from 65.
pub fn smooth_sequential_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Smooth Sequential REF",
        vec![
            clear_stop(-10.0),
            clear_stop(9.5),
            stop(10.0, 20, 10, 40),
            stop(12.5, 28, 13, 60),
            stop(15.0, 36, 16, 80),
            stop(17.5, 44, 19, 100),
            stop(19.5, 50, 21, 116),
            // 20 dBZ: precipitation onset. Indigo steps up into violet.
            stop(20.0, 98, 26, 144),
            stop(22.5, 110, 31, 148),
            stop(25.0, 122, 36, 150),
            stop(27.5, 134, 41, 151),
            stop(30.0, 146, 46, 150),
            stop(32.5, 158, 51, 148),
            stop(34.5, 168, 55, 145),
            // 35 dBZ: moderate-to-heavy rain. Violet steps up into red.
            stop(35.0, 226, 66, 66),
            stop(37.5, 233, 78, 58),
            stop(40.0, 238, 89, 50),
            stop(42.5, 243, 99, 44),
            stop(45.0, 247, 108, 39),
            stop(47.5, 250, 116, 35),
            stop(49.5, 252, 122, 32),
            // 50 dBZ: the core. Red steps up into amber.
            stop(50.0, 255, 178, 26),
            stop(52.5, 255, 184, 34),
            stop(55.0, 255, 190, 44),
            stop(57.5, 255, 196, 54),
            stop(60.0, 255, 201, 64),
            stop(62.5, 254, 206, 74),
            stop(64.5, 253, 210, 82),
            // 65 dBZ: not rain. Amber steps up into near-white.
            stop(65.0, 248, 248, 176),
            stop(67.5, 250, 250, 196),
            stop(70.0, 252, 251, 214),
            stop(75.0, 253, 253, 230),
            stop(80.0, 254, 254, 243),
            stop(95.0, 255, 255, 255),
        ],
    )
}

/// Reflectivity with the colour spent between 35 and 65 dBZ.
///
/// Interrogating a convective core on a full-range palette means most of the
/// scope's colour is being used on stratiform rain that is not the question.
/// This table holds everything below 35 dBZ in low-saturation slate and blue -
/// present, locatable, never competing - and spends the rest of its travel on
/// the 30 dBZ where a core, a hail shaft and a debris ball live.
///
/// The same idea the ZDR Column Hunter and Heavy Rain KDP presets apply to
/// their own moments: a preset earns its place when it is stretched over the
/// band one specific question lives in.
///
/// Interpolated, and with the same four break turns as Smooth Classic REF, so
/// the 50 dBZ contour is still a contour.
pub fn smooth_storm_core_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Smooth Storm Core REF",
        vec![
            clear_stop(-10.0),
            clear_stop(9.5),
            stop(10.0, 40, 44, 50),
            stop(12.5, 46, 50, 56),
            stop(15.0, 52, 56, 62),
            stop(17.5, 57, 61, 67),
            stop(19.5, 60, 64, 70),
            // 20 dBZ: precipitation onset, marked but kept muted. The turn is
            // into blue rather than up in brightness, so it is findable without
            // the sub-convective band ever competing with the core above.
            stop(20.0, 50, 74, 110),
            stop(22.5, 54, 79, 117),
            stop(25.0, 58, 84, 124),
            stop(27.5, 62, 89, 131),
            stop(30.0, 66, 94, 138),
            stop(32.5, 70, 99, 145),
            stop(34.5, 74, 104, 152),
            // 35 dBZ: the palette switches on.
            stop(35.0, 24, 152, 78),
            stop(37.5, 46, 182, 66),
            stop(40.0, 104, 204, 58),
            stop(42.5, 168, 218, 54),
            stop(45.0, 226, 224, 50),
            stop(47.5, 246, 190, 42),
            stop(49.5, 250, 178, 40),
            // 50 dBZ: the core.
            stop(50.0, 248, 80, 30),
            stop(52.5, 244, 54, 34),
            stop(55.0, 236, 24, 38),
            stop(57.5, 220, 16, 46),
            stop(60.0, 202, 12, 56),
            stop(62.5, 184, 10, 66),
            stop(64.5, 168, 10, 76),
            // 65 dBZ: hail.
            stop(65.0, 226, 60, 214),
            stop(67.5, 236, 118, 230),
            stop(70.0, 242, 166, 240),
            stop(72.5, 246, 204, 246),
            stop(75.0, 250, 232, 250),
            stop(80.0, 252, 244, 252),
            stop(95.0, 255, 255, 255),
        ],
    )
}

pub fn analyst_velocity_table() -> ColorTable {
    parsed_preset("Analyst Pro VEL", ANALYST_PRO_VELOCITY_TABLE)
}

pub fn nws_velocity_table() -> ColorTable {
    parsed_preset("NWS Classic VEL", NWS_VELOCITY_TABLE)
}

pub fn gr2_velocity_table() -> ColorTable {
    parsed_preset("GR2Analyst Classic VEL", GR2_VELOCITY_TABLE)
}

pub fn tight_couplet_velocity_table() -> ColorTable {
    parsed_preset("Analyst Tight Couplet VEL", TIGHT_COUPLET_VELOCITY_TABLE)
}

pub fn radarscope_contrast_velocity_table() -> ColorTable {
    parsed_preset(
        "RadarScope Contrast VEL",
        RADARSCOPE_CONTRAST_VELOCITY_TABLE,
    )
}

pub fn sign_check_velocity_table() -> ColorTable {
    parsed_preset("Sign Check VEL", SIGN_CHECK_VELOCITY_TABLE)
}

pub fn couplet_pop_velocity_table() -> ColorTable {
    parsed_preset("Couplet Pop VEL", COUPLET_POP_VELOCITY_TABLE)
}

pub fn gr2_ish_analyst_velocity_table() -> ColorTable {
    parsed_preset("GR2-ish Analyst VEL", GR2_ISH_ANALYST_VELOCITY_TABLE)
}

pub fn subtle_srv_velocity_table() -> ColorTable {
    parsed_preset("Subtle SRV VEL", SUBTLE_SRV_VELOCITY_TABLE)
}

pub fn nws_split_velocity_table() -> ColorTable {
    parsed_preset("NWS Split VEL", NWS_SPLIT_VELOCITY_TABLE)
}

pub fn dark_analyst_velocity_table() -> ColorTable {
    parsed_preset("Dark Analyst VEL", DARK_ANALYST_VELOCITY_TABLE)
}

// ---------------------------------------------------------------------------
// Continuously interpolated velocity
//
// Same argument as the interpolated reflectivity block above, and it bites
// harder here. Every velocity preset above quantises onto a 1 or 2 m/s grid,
// and the thing an analyst is looking for in a velocity field is a *gradient*:
// a couplet is two adjacent gates of opposite sign, and its strength is the
// difference between them. A quantised palette rounds that difference to the
// bin size before it is ever drawn, so a 3 m/s shear and a 4 m/s shear can
// paint the same two colours.
//
// Both tables here keep the conventions that make a velocity display readable:
// negative is inbound and runs green through cyan, positive is outbound and
// runs red through amber, and zero is a neutral grey so the zero isodop is a
// line rather than a colour. Sign is not a magnitude - an analyst reads the
// two halves as different things - so the two run to different hues rather
// than to two ends of one ramp.
// ---------------------------------------------------------------------------

/// Doppler velocity, continuously interpolated over the full +/-70 m/s.
///
/// The everyday table: the familiar green-inbound, red-outbound scheme with no
/// quantisation, ramping out to near-white at both extremes so a strong core of
/// either sign is unmistakable. Colour is spread over the whole domain rather
/// than concentrated, which is what you want when the question is the flow
/// field - a rear-inflow jet, a low-level jet, the breadth of an outflow - and
/// not one small rotation.
pub fn smooth_doppler_velocity_table() -> ColorTable {
    smooth_preset(
        "Smooth Doppler VEL",
        vec![
            stop(-70.0, 236, 255, 255),
            stop(-62.0, 198, 248, 255),
            stop(-55.0, 150, 238, 252),
            stop(-48.0, 100, 224, 246),
            stop(-42.0, 56, 206, 236),
            stop(-36.0, 24, 186, 216),
            stop(-32.0, 16, 196, 172),
            stop(-28.0, 16, 208, 136),
            stop(-24.0, 18, 218, 96),
            stop(-20.0, 22, 228, 62),
            stop(-16.0, 20, 202, 58),
            stop(-12.0, 18, 174, 54),
            stop(-8.0, 16, 146, 50),
            stop(-5.0, 34, 128, 60),
            stop(-3.0, 60, 116, 74),
            stop(-1.5, 92, 110, 96),
            stop(0.0, 112, 112, 112),
            stop(1.5, 130, 100, 98),
            stop(3.0, 148, 84, 78),
            stop(5.0, 168, 62, 60),
            stop(8.0, 190, 40, 42),
            stop(12.0, 212, 28, 32),
            stop(16.0, 232, 24, 28),
            stop(20.0, 250, 26, 26),
            stop(24.0, 252, 66, 22),
            stop(28.0, 253, 100, 20),
            stop(32.0, 254, 132, 20),
            stop(36.0, 255, 162, 26),
            stop(42.0, 255, 196, 48),
            stop(48.0, 255, 220, 96),
            stop(55.0, 255, 236, 150),
            stop(62.0, 255, 246, 200),
            stop(70.0, 255, 252, 240),
        ],
    )
}

/// Doppler velocity with the colour spent inside +/-25 m/s.
///
/// A tornadic or mesocyclonic couplet is defined by its rotational velocity,
/// half the inbound-to-outbound difference across the circulation, and the
/// operational thresholds sit low: the WSR-88D mesocyclone detection algorithm
/// works from shear and momentum over circulations whose rotational velocities
/// are typically 15-25 m/s (Stumpf, G. J., and coauthors, 1998: "The National
/// Severe Storms Laboratory mesocyclone detection algorithm for the WSR-88D",
/// Wea. Forecasting, 13, 304-326,
/// doi:10.1175/1520-0434(1998)013<0304:TNSSLM>2.0.CO;2). Most base-velocity
/// data is inside the Nyquist interval anyway, which for the common precipitation
/// VCPs is nearer 25-32 m/s than 70.
///
/// So this table gives 36% of its domain - the +/-25 m/s that couplets live in -
/// about 63% of its colour travel, with hue anchors on 15 and 25 m/s of both
/// signs. Beyond 25 m/s it keeps changing, just more slowly, so a dealiasing
/// failure or a genuine 60 m/s gate is still visibly extreme.
///
/// Interpolated, which is the point: the gate-to-gate difference across a
/// couplet is drawn at the resolution the data has rather than rounded to a
/// 1 m/s bin first.
pub fn smooth_couplet_velocity_table() -> ColorTable {
    smooth_preset(
        "Smooth Couplet VEL",
        vec![
            stop(-70.0, 214, 250, 250),
            stop(-55.0, 170, 244, 250),
            stop(-45.0, 120, 236, 248),
            stop(-38.0, 70, 226, 244),
            stop(-32.0, 24, 214, 238),
            // -25 m/s: strong inbound.
            stop(-25.0, 0, 206, 214),
            stop(-22.0, 8, 214, 170),
            stop(-19.0, 12, 222, 124),
            // -15 m/s: mesocyclone-strength inbound.
            stop(-15.0, 18, 232, 70),
            stop(-12.0, 16, 210, 62),
            stop(-9.0, 14, 186, 56),
            stop(-6.0, 26, 158, 56),
            stop(-4.0, 44, 134, 62),
            stop(-2.0, 70, 112, 82),
            stop(-1.0, 90, 102, 96),
            stop(0.0, 104, 104, 104),
            stop(1.0, 118, 96, 94),
            stop(2.0, 138, 86, 82),
            stop(4.0, 166, 62, 60),
            stop(6.0, 194, 42, 42),
            stop(9.0, 218, 28, 30),
            stop(12.0, 240, 22, 26),
            // +15 m/s: mesocyclone-strength outbound.
            stop(15.0, 255, 44, 28),
            stop(19.0, 255, 96, 24),
            stop(22.0, 255, 142, 22),
            // +25 m/s: strong outbound.
            stop(25.0, 255, 188, 28),
            stop(32.0, 255, 214, 88),
            stop(38.0, 255, 228, 136),
            stop(45.0, 255, 240, 184),
            stop(55.0, 255, 248, 218),
            stop(70.0, 255, 253, 244),
        ],
    )
}

pub fn builtin_spectrum_width_table() -> ColorTable {
    smooth_preset(
        "Analyst Spectrum Width",
        vec![
            stop(0.0, 9, 20, 32),
            stop(1.0, 24, 52, 100),
            stop(2.0, 22, 102, 172),
            stop(3.0, 18, 152, 180),
            stop(4.0, 36, 174, 98),
            stop(5.5, 160, 188, 58),
            stop(7.0, 232, 190, 54),
            stop(9.0, 238, 112, 42),
            stop(12.0, 216, 44, 50),
            stop(16.0, 160, 36, 136),
            stop(24.0, 235, 235, 235),
        ],
    )
}

/// Turbulence-hunting spectrum width, stretched across 4-12 m/s.
///
/// Spectrum width is the spread of the Doppler spectrum in one resolution
/// volume, so it rises with shear and turbulence inside the beam. The default
/// preset above spends most of its ramp on the 0-8 m/s bulk of a scan; this one
/// pushes the ramp into the band where a mesocyclone, a gust front, or a
/// three-body scatter spike separates from ordinary precipitation.
pub fn turbulence_spectrum_width_table() -> ColorTable {
    smooth_preset(
        "Turbulence SW",
        vec![
            stop(0.0, 12, 14, 22),
            stop(2.0, 18, 34, 62),
            stop(4.0, 26, 78, 132),
            stop(5.0, 28, 130, 176),
            stop(6.0, 34, 176, 156),
            stop(7.0, 96, 202, 96),
            stop(8.0, 188, 216, 60),
            stop(9.0, 240, 200, 50),
            stop(10.0, 246, 150, 42),
            stop(11.0, 240, 92, 40),
            stop(12.0, 228, 40, 48),
            stop(16.0, 198, 46, 156),
            stop(24.0, 246, 246, 250),
        ],
    )
}

/// Clear-air spectrum width, stretched across 0-6 m/s.
///
/// Boundaries, fine lines, and bird/insect returns in a clear-air VCP sit under
/// about 6 m/s, where the default preset has barely left its first two stops.
pub fn clear_air_spectrum_width_table() -> ColorTable {
    smooth_preset(
        "Clear Air SW",
        vec![
            stop(0.0, 10, 26, 46),
            stop(0.5, 18, 56, 96),
            stop(1.0, 24, 92, 148),
            stop(1.5, 28, 132, 182),
            stop(2.0, 32, 172, 176),
            stop(2.5, 48, 196, 124),
            stop(3.0, 116, 208, 78),
            stop(3.5, 188, 216, 62),
            stop(4.0, 238, 210, 54),
            stop(5.0, 246, 156, 44),
            stop(6.0, 240, 88, 40),
            stop(10.0, 200, 40, 80),
            stop(16.0, 150, 44, 150),
            stop(24.0, 240, 240, 246),
        ],
    )
}

/// Spectrum width as flat categories rather than a ramp.
///
/// Stepped, so every gate inside a band paints one colour and the band edges
/// read as contours. Useful when the question is "where does the field cross
/// 8 m/s", not "how does it vary".
pub fn spectrum_width_class_bands_table() -> ColorTable {
    banded_preset(
        "SW Class Bands",
        vec![
            stop(0.0, 20, 32, 56),
            stop(2.0, 32, 104, 168),
            stop(4.0, 40, 168, 140),
            stop(6.0, 150, 206, 66),
            stop(8.0, 240, 206, 52),
            stop(11.0, 240, 122, 42),
            stop(14.0, 226, 44, 52),
            stop(18.0, 176, 48, 168),
            stop(24.0, 240, 240, 248),
        ],
    )
}

// ---------------------------------------------------------------------------
// Dual-polarimetric palettes
//
// Interpretation breaks below are taken from Kumjian, M. R. (2013):
// "Principles and applications of dual-polarization weather radar",
// J. Operational Meteor., Part I 1(19), 226-242, doi:10.15191/nwajom.2013.0119;
// Part II 1(20), 243-264, doi:10.15191/nwajom.2013.0120;
// Part III 1(21), 265-274, doi:10.15191/nwajom.2013.0121.
// Physical ranges follow Ryzhkov, A. V., and D. S. Zrnic (2019):
// "Radar Polarimetry for Weather Observations", Springer,
// doi:10.1007/978-3-030-05093-1.
//
// Two design rules are shared by all of them.
//
// First: put colour travel where the physical discrimination is, not where the
// numeric range is. A linear ramp over the declared domain is the failure mode
// this module exists to fix.
//
// Second, and learned the hard way from the cached Level II volumes: a palette
// must span the whole range its field can encode, and it must not spend its
// brightest colour on the part of that range which is instrument noise. Every
// value past the last stop is painted in the last stop's colour, so a domain
// that stops short of the field's own saturation code turns that code's
// pile-up into a flat wash - the very defect the dual-pol families were added
// to fix. Both ZDR and RHOHV pile up hard on their top code in real data, so
// both palettes end muted rather than white.
// ---------------------------------------------------------------------------

/// Declared ZDR domain, taken from the field's own encoding rather than from a
/// round number.
///
/// The Level II 16-bit ZDR word carries scale 32 and offset 418, and codes 0
/// and 1 are reserved for "below threshold" and "range folded", so the field
/// runs from (2 - 418)/32 = -13.0 dB to (1058 - 418)/32 = +20.0 dB. Decoding
/// KUEX, KABR, KTLX, KLTX and KDMX confirms exactly that: every volume reports
/// scale 32, offset 418, raw minimum 2 and raw maximum 1058.
///
/// The palette earlier stopped at +8 dB on the theory that the field saturates
/// near +/-7.9 dB, which is the *legacy 8-bit* product's range, not this one's.
/// On real scans 3.4% (KABR) to 25.0% (KLTX) of all gates sit at or above
/// +8 dB - biological scatterers, sea clutter and low-SNR noise - and every one
/// of them was clamped onto the palette's brightest colour.
pub(crate) const ZDR_MIN_DB: f32 = -13.0;
pub(crate) const ZDR_MAX_DB: f32 = 20.0;

/// Where the *meteorological* ZDR scale ends, which is not where the field
/// ends. Rain tops out near 5 dB, the melting layer and the largest oblate
/// drops near 7-8 dB (Kumjian 2013 Part I); past that, ZDR is biota, clutter or
/// noise. Colour is spent between these two bounds; outside them the palettes
/// run to a muted off-scale band so junk cannot outshine weather.
pub(crate) const ZDR_MET_MIN_DB: f32 = -7.0;
pub(crate) const ZDR_MET_MAX_DB: f32 = 8.0;

/// Declared RHOHV domain, taken from the field's own encoding.
///
/// The WSR-88D 8-bit RHOHV word carries scale 300 and offset -60.5 with codes 0
/// and 1 reserved, so the smallest and largest values it can hold are
/// (2 + 60.5)/300 = 0.2083 and (255 + 60.5)/300 = 1.0517. Written as those
/// quotients so the constants are exactly the decoded endpoints - `MomentGrid`
/// decodes as `(raw - offset) / scale` over the same f32 values - and no real
/// gate can fall outside the palette.
///
/// Code 255 is a saturation code, not a measurement: across the five cached
/// volumes it holds 4.1% to 10.3% of *all* gates but only 0.012% to 0.037% of
/// gates with reflectivity above 20 dBZ, and the median reflectivity of the
/// gates carrying it is -2.5 to +7.5 dBZ. RHOHV above unity is not physical for
/// a single hydrometeor population; it is the low-SNR bias of the estimator
/// (Ryzhkov and Zrnic 2019, ch. 3). So the palettes peak at 1.00 and darken
/// into the ceiling instead of ending white.
pub(crate) const CC_MIN: f32 = 62.5 / 300.0;
pub(crate) const CC_MAX: f32 = 315.5 / 300.0;

/// Declared PHIDP domain. The field wraps: 359 deg and 1 deg are two degrees
/// apart, not 358.
pub(crate) const PHI_MIN_DEG: f32 = 0.0;
pub(crate) const PHI_MAX_DEG: f32 = 360.0;

/// Declared KDP domain, in deg/km.
pub(crate) const KDP_MIN_DEG_PER_KM: f32 = -2.0;
pub(crate) const KDP_MAX_DEG_PER_KM: f32 = 7.0;

pub fn builtin_differential_reflectivity_table() -> ColorTable {
    analyst_differential_reflectivity_table()
}

/// Differential reflectivity with the three bands a forecaster reads.
///
/// ZDR is the log ratio of horizontal to vertical reflectivity, so it measures
/// how oblate the scatterers are (Kumjian 2013 Part I, section 3). The breaks:
///
/// * Near zero, -0.5 to +0.3 dB: spherical or tumbling scatterers - dry hail,
///   large hail that is falling chaotically, dry snow aggregates. Held on one
///   neutral hue across the whole band - it brightens from (124,124,128) to
///   (176,176,180) but never leaves grey - so the band reads as one category
///   while still resolving where inside it a gate sits. This is the band that
///   pairs with high reflectivity to say "hail", and the band that pairs with
///   low CC to say "debris".
/// * 1 to 3 dB: rain. Drops flatten as they grow, so ZDR climbs with drop size
///   through this range. Green through yellow.
/// * Above 4 dB: large oblate drops - the melting band, drop-size sorting on
///   the storm's forward flank, and biological scatterers. Orange through red
///   into magenta so it separates hard from the rain band below it.
///
/// Negative ZDR runs violet. It is uncommon and worth noticing: vertically
/// aligned ice in a strong electric field, or a bad calibration.
///
/// Outside -7 to +8 dB the palette leaves the meteorological scale and runs to
/// a dark teal that appears nowhere else in the table, so off-scale echo stays
/// legible without competing with weather for attention.
pub fn analyst_differential_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Analyst ZDR",
        vec![
            stop(ZDR_MIN_DB, 34, 0, 64),
            stop(ZDR_MET_MIN_DB, 58, 10, 92),
            stop(-4.0, 92, 26, 148),
            stop(-2.0, 120, 66, 196),
            stop(-1.0, 96, 122, 208),
            // Grey plateau: both ends of the near-zero band are neutral, so the
            // band changes only in lightness and no hue creeps into it.
            stop(-0.5, 124, 124, 128),
            stop(0.3, 176, 176, 180),
            stop(0.4, 24, 96, 62),
            stop(0.7, 26, 140, 74),
            stop(1.0, 44, 188, 86),
            stop(1.5, 132, 210, 70),
            stop(2.0, 198, 224, 64),
            stop(2.5, 240, 216, 58),
            stop(3.0, 248, 170, 44),
            stop(3.5, 246, 124, 36),
            stop(4.0, 238, 62, 44),
            stop(5.0, 208, 30, 98),
            stop(6.0, 198, 46, 172),
            stop(7.0, 228, 152, 228),
            stop(ZDR_MET_MAX_DB, 246, 246, 250),
            stop(9.0, 88, 168, 176),
            stop(14.0, 34, 96, 104),
            stop(ZDR_MAX_DB, 16, 40, 46),
        ],
    )
}

/// The same ZDR breaks as flat categories.
///
/// Stepped, so each interpretation band from Kumjian (2013, Part I) paints one
/// colour and the band edges become contours. Reach for this when the question
/// is which category a core falls in rather than how ZDR varies inside it.
///
/// "At or above 8 dB" is itself one of those categories - non-meteorological -
/// so it gets a band of its own rather than the top of the ramp, and the
/// encoding ceiling at 20 dB gets one more so a saturated field is visible as
/// saturated.
pub fn storm_interrogation_differential_reflectivity_table() -> ColorTable {
    banded_preset(
        "Storm Interrogation ZDR",
        vec![
            stop(ZDR_MIN_DB, 30, 6, 54),
            stop(ZDR_MET_MIN_DB, 72, 20, 110),
            stop(-1.0, 86, 106, 178),
            stop(-0.5, 112, 112, 116),
            stop(0.3, 30, 120, 70),
            stop(1.0, 46, 190, 88),
            stop(2.0, 206, 222, 62),
            stop(3.0, 250, 176, 44),
            stop(4.0, 238, 66, 46),
            stop(5.0, 206, 34, 120),
            stop(6.0, 200, 48, 176),
            stop(ZDR_MET_MAX_DB, 56, 124, 130),
            stop(ZDR_MAX_DB, 26, 62, 68),
        ],
    )
}

/// ZDR stretched onto 0.5-4 dB to find ZDR columns.
///
/// A ZDR column is a plume of ZDR above 1 dB extending above the environmental
/// 0 C level, where supercooled raindrops are being lofted; it marks the updraft
/// and it leads hail and tornadogenesis (Kumjian, M. R., A. P. Khain,
/// N. BenMoshe, E. Ilotoviz, A. V. Ryzhkov, and V. T. J. Phillips, 2014: "The
/// anatomy and physics of ZDR columns", J. Appl. Meteor. Climatol., 53,
/// 1820-1843, doi:10.1175/JAMC-D-13-0354.1). The bright end of this palette
/// therefore sits on 2.5-5 dB; below 0.5 dB it is near-black and past 5 dB it
/// darkens again, so a column is what the eye lands on. That was not true
/// before: the palette ran to white at its top stop, handing the brightest
/// colour on the scope to the biological scatterers and sea clutter that hold
/// 3-25% of the gates in a real volume.
pub fn zdr_column_hunter_table() -> ColorTable {
    smooth_preset(
        "ZDR Column Hunter",
        vec![
            stop(ZDR_MIN_DB, 8, 8, 12),
            stop(ZDR_MET_MIN_DB, 10, 10, 16),
            stop(0.5, 14, 18, 30),
            stop(1.0, 26, 70, 120),
            stop(1.5, 32, 132, 168),
            stop(2.0, 44, 186, 150),
            stop(2.5, 130, 214, 92),
            stop(3.0, 232, 216, 62),
            stop(3.5, 246, 146, 44),
            stop(4.0, 236, 52, 48),
            stop(5.0, 232, 108, 200),
            stop(ZDR_MET_MAX_DB, 96, 60, 110),
            stop(ZDR_MAX_DB, 20, 14, 28),
        ],
    )
}

/// ZDR stretched onto -1 to +1 dB, so the near-zero band is the bright one.
///
/// Large hail depolarises so little that ZDR collapses to zero regardless of
/// how big the stones are, which is why a hail core reads as high reflectivity
/// with ZDR near 0 (Kumjian 2013 Part II, section 3). Every other ZDR value is
/// pushed dark here so the hail signal, and the near-zero ZDR of a tornadic
/// debris signature, is what the eye lands on.
pub fn hail_signal_differential_reflectivity_table() -> ColorTable {
    smooth_preset(
        "Hail Signal ZDR",
        vec![
            stop(ZDR_MIN_DB, 16, 4, 28),
            stop(ZDR_MET_MIN_DB, 28, 6, 48),
            stop(-2.0, 54, 18, 96),
            stop(-1.0, 92, 60, 176),
            stop(-0.6, 60, 128, 210),
            stop(-0.3, 40, 190, 200),
            stop(-0.1, 250, 250, 250),
            stop(0.1, 250, 250, 250),
            stop(0.3, 240, 196, 60),
            stop(0.6, 230, 128, 46),
            stop(1.0, 206, 58, 48),
            stop(2.0, 120, 36, 60),
            stop(4.0, 56, 60, 96),
            stop(ZDR_MET_MAX_DB, 24, 30, 52),
            stop(ZDR_MAX_DB, 14, 18, 32),
        ],
    )
}

pub fn builtin_correlation_coefficient_table() -> ColorTable {
    analyst_correlation_coefficient_table()
}

/// Correlation coefficient on a deliberately non-linear scale.
///
/// This is the single most important design decision in this module. RHOHV is
/// bounded above by 1 and essentially all meteorological echo sits between 0.95
/// and 1.00 - a seventeenth of the 0.2083-1.0517 declared domain. A linear ramp
/// gives that seventeenth about 6% of its colour range, so rain, wet snow, and
/// the melting layer all come out the same colour and the field is decorative
/// rather than diagnostic.
///
/// So the stops are packed towards unity: five of the fifteen stops fall in
/// 0.95-1.00, which turns 6% of the domain into roughly 40% of the colour path.
/// The breaks follow Kumjian (2013, Part I, section 5):
///
/// * Above 0.97: meteorological, a single hydrometeor type filling the beam.
/// * 0.90-0.97: mixed-phase - the melting layer's bright-band dip, wet
///   aggregates, hail large enough to resonate.
/// * 0.80-0.90: mixed hydrometeors, big wet hail, the edges of non-met echo.
/// * Below 0.80: non-meteorological. Ground clutter, chaff, birds, insects, and
///   tornadic debris (Ryzhkov, A. V., T. J. Schuur, D. W. Burgess, and
///   D. S. Zrnic, 2005: "Polarimetric tornado detection", J. Appl. Meteor., 44,
///   557-570, doi:10.1175/JAM2235.1, which sets the debris threshold at 0.80
///   and notes most debris falls below 0.70).
///
/// The brightest colour is at 1.00, not at the top of the domain. Everything
/// above unity is the estimator's low-SNR bias rather than a measurement (see
/// `CC_MAX`), and the field's saturation code alone carries 4-10% of the gates
/// in a real volume, so the ramp dims into the ceiling.
pub fn analyst_correlation_coefficient_table() -> ColorTable {
    smooth_preset(
        "Analyst CC",
        vec![
            stop(CC_MIN, 36, 12, 60),
            stop(0.45, 78, 20, 96),
            stop(0.65, 128, 32, 96),
            stop(0.75, 186, 46, 74),
            stop(0.80, 226, 86, 48),
            stop(0.85, 240, 140, 40),
            stop(0.90, 246, 196, 52),
            stop(0.93, 206, 220, 62),
            stop(0.95, 120, 206, 84),
            stop(0.96, 56, 190, 120),
            stop(0.97, 30, 168, 168),
            stop(0.98, 34, 128, 200),
            stop(0.99, 56, 84, 216),
            stop(1.00, 150, 150, 235),
            stop(CC_MAX, 96, 100, 120),
        ],
    )
}

/// CC with the whole ramp below 0.90, for tornadic debris.
///
/// A tornadic debris signature is co-located high reflectivity, near-zero ZDR,
/// and RHOHV below 0.80 - usually below 0.70 (Ryzhkov et al. 2005,
/// doi:10.1175/JAM2235.1). This table burns its brightest colours there and
/// mutes everything meteorological, which inverts the usual reading: the debris
/// ball is the only lit thing on the scope.
pub fn debris_hunter_correlation_coefficient_table() -> ColorTable {
    smooth_preset(
        "Debris Hunter CC",
        vec![
            stop(CC_MIN, 255, 240, 120),
            stop(0.45, 250, 170, 40),
            stop(0.60, 240, 80, 40),
            stop(0.70, 226, 30, 90),
            stop(0.75, 198, 26, 150),
            stop(0.80, 140, 40, 190),
            stop(0.85, 76, 66, 190),
            stop(0.90, 40, 92, 150),
            stop(0.95, 26, 92, 110),
            stop(0.97, 30, 60, 78),
            stop(0.99, 46, 46, 52),
            stop(1.00, 60, 60, 66),
            stop(CC_MAX, 84, 84, 90),
        ],
    )
}

/// CC with the whole ramp inside 0.85-1.00, for the melting layer.
///
/// The bright band is a local RHOHV minimum, typically 0.90-0.97, sandwiched
/// between snow above and rain below that both sit above 0.98 (Giangrande,
/// S. E., J. M. Krause, and A. V. Ryzhkov, 2008: "Automatic designation of the
/// melting layer with a polarimetric prototype of the WSR-88D radar",
/// J. Appl. Meteor. Climatol., 47, 1354-1364, doi:10.1175/2007JAMC1634.1, which
/// designates the layer on a 0.90-0.97 RHOHV window). Spending eleven stops
/// above 0.85 makes that dip a band of its own colour instead of a shade; those
/// stops carry 82% of the table's colour travel across 24% of its domain.
///
/// Below 0.85 the table stays dark so the bright-band ramp is what the eye
/// lands on, but it still changes hue - plum, dark red, dark amber, dark slate -
/// rather than fading through one near-black gradient. A muted region is meant
/// to be de-emphasised, not made unreadable: a forecaster who glances at
/// non-meteorological echo on this table must still be able to tell 0.55 from
/// 0.75, and equal-luminance hue changes buy that without stealing attention.
pub fn melting_layer_correlation_coefficient_table() -> ColorTable {
    smooth_preset(
        "Melting Layer CC",
        vec![
            stop(CC_MIN, 34, 22, 40),
            stop(0.45, 110, 34, 56),
            stop(0.65, 96, 60, 24),
            stop(0.80, 26, 54, 78),
            stop(0.85, 40, 50, 110),
            stop(0.88, 34, 106, 176),
            stop(0.90, 30, 160, 176),
            stop(0.92, 54, 196, 118),
            stop(0.94, 150, 214, 66),
            stop(0.95, 216, 216, 54),
            stop(0.96, 246, 176, 44),
            stop(0.97, 244, 110, 40),
            stop(0.98, 226, 44, 56),
            stop(0.99, 176, 40, 130),
            stop(1.00, 150, 150, 220),
            stop(CC_MAX, 96, 100, 118),
        ],
    )
}

/// CC as flat hydrometeor-classification bands.
///
/// Stepped on the Kumjian (2013, Part I) breaks, so each gate paints the colour
/// of the category it falls in and the category edges become contours. This is
/// the quality-control view: everything below 0.80 is one colour, and it is the
/// colour you scan for before trusting a rainfall estimate.
///
/// The last band is the field's saturation code on its own, in slate rather
/// than the near-white it used to take, so a display full of low-SNR gates
/// looks like a display full of low-SNR gates.
pub fn correlation_coefficient_class_bands_table() -> ColorTable {
    banded_preset(
        "CC Class Bands",
        vec![
            stop(CC_MIN, 206, 44, 168),
            stop(0.70, 232, 96, 44),
            stop(0.80, 240, 200, 56),
            stop(0.90, 92, 196, 96),
            stop(0.95, 36, 158, 168),
            stop(0.97, 46, 96, 200),
            stop(1.00, 200, 210, 240),
            stop(CC_MAX, 104, 108, 124),
        ],
    )
}

pub fn builtin_differential_phase_table() -> ColorTable {
    analyst_differential_phase_table()
}

/// Differential phase on a closed hue wheel.
///
/// PHIDP is the accumulated phase difference between the H and V returns along
/// the beam. It only increases with range through precipitation, and it wraps:
/// past 360 deg the field folds back to 0 (Ryzhkov and Zrnic 2019, chapter 4).
/// A ramp with different colours at each end therefore draws a hard edge across
/// every wrapping ray, and an analyst reads that edge as a real gradient when
/// it is an artefact of the number line.
///
/// The fix is a cyclic map - one whose first and last colours are identical, so
/// there is no privileged point on the scale (Kovesi, P., 2015: "Good colour
/// maps: how to design them", arXiv:1509.03700, section 4). This is a constant
/// saturation, constant value hue wheel sampled every 30 deg, so 360 deg
/// returns exactly the colour of 0 deg and the wrap is invisible.
pub fn analyst_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Analyst Cyclic PHI",
        vec![
            stop(PHI_MIN_DEG, 242, 36, 36),
            stop(30.0, 242, 139, 36),
            stop(60.0, 242, 242, 36),
            stop(90.0, 139, 242, 36),
            stop(120.0, 36, 242, 36),
            stop(150.0, 36, 242, 139),
            stop(180.0, 36, 242, 242),
            stop(210.0, 36, 139, 242),
            stop(240.0, 36, 36, 242),
            stop(270.0, 139, 36, 242),
            stop(300.0, 242, 36, 242),
            stop(330.0, 242, 36, 139),
            stop(PHI_MAX_DEG, 242, 36, 36),
        ],
    )
}

/// Differential phase on a closed dark-to-light-to-dark cycle.
///
/// The hue wheel above is cyclic but is not monotone in lightness, so a mid-grey
/// display or a colour-vision deficiency can flatten parts of it. This one
/// cycles lightness instead of hue - pale lavender through blue to near-black
/// and back through red - which keeps the wrap closed while giving the eye a
/// brightness gradient to follow. Same construction as matplotlib's `twilight`,
/// designed to the Kovesi (2015, arXiv:1509.03700) cyclic criteria.
pub fn twilight_cyclic_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Twilight Cyclic PHI",
        vec![
            stop(PHI_MIN_DEG, 226, 217, 226),
            stop(36.0, 150, 180, 225),
            stop(72.0, 72, 132, 205),
            stop(108.0, 36, 84, 158),
            stop(144.0, 30, 44, 96),
            stop(180.0, 34, 26, 46),
            stop(216.0, 96, 34, 58),
            stop(252.0, 160, 50, 60),
            stop(288.0, 206, 96, 78),
            stop(324.0, 222, 160, 150),
            stop(PHI_MAX_DEG, 226, 217, 226),
        ],
    )
}

/// Differential phase as 15 deg isophase bands.
///
/// Stepped, so the display draws contours of constant PHIDP. What matters
/// operationally is the range derivative - KDP is half of it - and band spacing
/// reads a derivative far better than a smooth ramp does: bands crowd together
/// where phase accumulates fast. Still cyclic: the band at 345-360 deg abuts the
/// band at 0-15 deg exactly one band-step away in colour, the same step as
/// every other boundary, so the wrap is not a special edge.
pub fn phase_bands_differential_phase_table() -> ColorTable {
    banded_preset(
        "Phase Bands PHI",
        vec![
            stop(PHI_MIN_DEG, 242, 36, 36),
            stop(15.0, 242, 88, 36),
            stop(30.0, 242, 139, 36),
            stop(45.0, 242, 191, 36),
            stop(60.0, 242, 242, 36),
            stop(75.0, 191, 242, 36),
            stop(90.0, 139, 242, 36),
            stop(105.0, 88, 242, 36),
            stop(120.0, 36, 242, 36),
            stop(135.0, 36, 242, 88),
            stop(150.0, 36, 242, 139),
            stop(165.0, 36, 242, 191),
            stop(180.0, 36, 242, 242),
            stop(195.0, 36, 191, 242),
            stop(210.0, 36, 139, 242),
            stop(225.0, 36, 88, 242),
            stop(240.0, 36, 36, 242),
            stop(255.0, 88, 36, 242),
            stop(270.0, 139, 36, 242),
            stop(285.0, 191, 36, 242),
            stop(300.0, 242, 36, 242),
            stop(315.0, 242, 36, 191),
            stop(330.0, 242, 36, 139),
            stop(345.0, 242, 36, 88),
            stop(PHI_MAX_DEG, 242, 36, 36),
        ],
    )
}

pub fn builtin_specific_differential_phase_table() -> ColorTable {
    analyst_specific_differential_phase_table()
}

/// Specific differential phase, diverging about zero.
///
/// KDP is half the range derivative of PHIDP, so it is a local measure of how
/// much liquid the beam is passing through: immune to attenuation, partial beam
/// blockage, and absolute calibration, which is why it carries rainfall
/// estimation (Ryzhkov and Zrnic 2019, chapter 6). Sign is meaningful - negative
/// KDP means vertically aligned scatterers or non-uniform beam filling, not
/// "less rain" - so zero is the neutral grey pivot and the two signs run to
/// different hues rather than to two ends of one ramp.
///
/// Breaks follow Kumjian (2013, Part I, section 4): below about 0.5 deg/km is
/// light rain or ice, 0.5-2 is moderate to heavy rain, and above 2 is very heavy
/// rain or a rain/hail mix.
pub fn analyst_specific_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Analyst KDP",
        vec![
            stop(KDP_MIN_DEG_PER_KM, 40, 0, 80),
            stop(-1.0, 78, 30, 150),
            stop(-0.5, 60, 90, 180),
            stop(-0.25, 86, 110, 140),
            stop(0.0, 112, 112, 112),
            stop(0.25, 60, 120, 90),
            stop(0.5, 30, 160, 80),
            stop(1.0, 90, 200, 60),
            stop(1.5, 180, 220, 56),
            stop(2.0, 240, 220, 50),
            stop(3.0, 246, 168, 40),
            stop(4.0, 240, 96, 36),
            stop(5.0, 226, 40, 44),
            stop(6.0, 208, 44, 150),
            stop(KDP_MAX_DEG_PER_KM, 245, 240, 250),
        ],
    )
}

/// KDP stretched onto 0.5-4 deg/km, the heavy rain band.
///
/// R(KDP) relations are the operational rainfall estimator inside convection
/// because KDP does not care about hail contamination or attenuation
/// (Ryzhkov and Zrnic 2019, chapter 6). This table darkens everything under
/// 0.5 deg/km so the heavy-rain core is what stands out, which is the view for
/// flash-flood interrogation rather than for storm structure.
pub fn heavy_rain_specific_differential_phase_table() -> ColorTable {
    smooth_preset(
        "Heavy Rain KDP",
        vec![
            stop(KDP_MIN_DEG_PER_KM, 14, 16, 24),
            stop(0.0, 20, 24, 36),
            stop(0.5, 26, 62, 110),
            stop(1.0, 28, 120, 170),
            stop(1.5, 36, 176, 150),
            stop(2.0, 110, 208, 92),
            stop(2.5, 214, 218, 62),
            stop(3.0, 248, 170, 44),
            stop(3.5, 244, 108, 38),
            stop(4.0, 230, 42, 48),
            stop(5.0, 206, 44, 152),
            stop(KDP_MAX_DEG_PER_KM, 250, 250, 252),
        ],
    )
}

/// KDP stretched onto -0.5 to +1.5 deg/km.
///
/// Outside a convective core KDP is small and noisy, and the interesting
/// structure - ice crystal alignment, the KDP foot below the melting layer,
/// weak stratiform rain - lives in a range where the default table has moved
/// three stops. Diverging about zero like the default so sign still reads.
pub fn fine_detail_specific_differential_phase_table() -> ColorTable {
    smooth_preset(
        "KDP Fine Detail",
        vec![
            stop(KDP_MIN_DEG_PER_KM, 48, 8, 70),
            stop(-1.0, 86, 26, 140),
            stop(-0.5, 92, 92, 200),
            stop(-0.25, 70, 150, 214),
            stop(-0.1, 96, 150, 160),
            stop(0.0, 118, 118, 118),
            stop(0.1, 104, 154, 96),
            stop(0.25, 48, 176, 76),
            stop(0.5, 128, 208, 60),
            stop(0.75, 206, 222, 56),
            stop(1.0, 246, 202, 48),
            stop(1.25, 248, 146, 40),
            stop(1.5, 240, 74, 44),
            stop(3.0, 188, 40, 118),
            stop(KDP_MAX_DEG_PER_KM, 244, 236, 248),
        ],
    )
}

pub fn builtin_generic_table() -> ColorTable {
    smooth_preset(
        "Analyst Generic",
        vec![
            stop(0.0, 34, 40, 64),
            stop(10.0, 34, 82, 130),
            stop(25.0, 34, 132, 172),
            stop(40.0, 58, 166, 140),
            stop(55.0, 116, 180, 92),
            stop(70.0, 218, 188, 74),
            stop(85.0, 224, 114, 56),
            stop(100.0, 210, 64, 68),
        ],
    )
}

const GR2_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 4 233 231
color: 15 1 159 244
color: 20 3 0 244
color: 25 2 253 2
color: 30 1 197 1
color: 35 0 142 0
color: 40 253 248 2
color: 45 229 188 0
color: 50 253 149 0
color: 55 253 0 0
color: 62.5 212 0 0
color: 67.5 188 0 0
color: 72.5 232 32 206
color: 80 156 70 206
color: 92.5 255 255 255
"#;

const NWS_CLASSIC_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 4 233 231
color: 15 1 159 244
color: 20 3 0 244
color: 25 2 253 2
color: 30 1 197 1
color: 35 0 142 0
color: 40 253 248 2
color: 45 229 188 0
color: 50 253 149 0
color: 55 253 0 0
color: 62.5 212 0 0
color: 67.5 188 0 0
color: 72.5 232 32 206
color: 80 156 70 206
color: 92.5 255 255 255
"#;

const ANALYST_CLASSIC_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 0 204 220
color: 15 0 132 232
color: 20 12 58 226
color: 25 0 222 44
color: 30 0 174 24
color: 35 0 124 12
color: 40 235 226 34
color: 45 238 174 28
color: 50 242 112 22
color: 55 238 28 30
color: 62.5 190 0 18
color: 67.5 150 0 18
color: 72.5 214 42 180
color: 80 150 82 198
color: 92.5 246 246 246
"#;

const STORM_DETAIL_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 2.5
color4: -10 0 0 0 0
color4: 0 0 0 0 0
color: 5 18 42 86
color: 10 25 92 154
color: 15 31 164 206
color: 20 28 184 114
color: 25 21 132 44
color: 30 88 178 42
color: 35 218 226 45
color: 40 251 180 32
color: 45 254 101 22
color: 50 238 32 28
color: 55 174 0 22
color: 60 214 52 168
color: 65 142 34 214
color: 70 228 228 236
color: 80 255 255 255
"#;

const HAIL_CORE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 35 98 164
color: 15 33 168 210
color: 20 16 172 78
color: 25 0 120 36
color: 30 82 170 40
color: 35 234 232 36
color: 40 252 168 22
color: 45 252 88 18
color: 50 246 26 28
color: 57.5 176 0 16
color: 65 154 0 28
color: 70 206 32 174
color: 77.5 152 74 204
color: 80 255 255 255
color: 87.5 112 228 255
color: 95 255 255 255
"#;

const LOW_PRECIP_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 2.5
color4: -15 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 38 116 174
color: 15 42 184 214
color: 20 58 204 132
color: 25 44 154 66
color: 30 84 188 50
color: 35 224 226 64
color: 40 250 178 50
color: 45 244 96 42
color: 50 218 44 52
color: 57.5 160 26 78
color: 65 170 28 128
color: 72.5 202 68 196
color: 80 154 84 204
color: 90 238 238 244
"#;

const DARK_SCOPE_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 38 86 128
color: 15 52 136 170
color: 20 30 158 86
color: 25 18 118 48
color: 30 78 164 44
color: 35 196 206 54
color: 40 232 156 42
color: 45 234 88 34
color: 50 218 38 40
color: 57.5 156 24 30
color: 65 168 30 130
color: 72.5 196 70 204
color: 80 154 82 210
color: 87.5 226 226 232
color: 95 255 255 255
"#;

const TORNADO_DEBRIS_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 5
color4: -10 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 30 96 152
color: 15 34 152 196
color: 20 26 190 112
color: 25 0 146 52
color: 30 72 176 42
color: 35 214 220 48
color: 40 246 174 32
color: 45 250 102 26
color: 50 238 32 30
color: 57.5 178 0 24
color: 65 164 0 40
color: 70 206 36 168
color: 77.5 224 94 210
color: 87.5 176 230 255
color: 95 255 255 255
"#;

const CLEAN_LIGHT_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
step: 2.5
color4: -15 0 0 0 0
color4: 7.5 0 0 0 0
color: 10 30 114 160
color: 17.5 38 164 190
color: 22.5 42 186 110
color: 27.5 22 132 52
color: 32.5 94 176 48
color: 37.5 220 218 58
color: 42.5 242 160 42
color: 47.5 236 90 38
color: 52.5 218 38 44
color: 60 156 22 34
color: 67.5 174 34 132
color: 75 206 72 198
color: 82.5 156 84 206
color: 92.5 238 238 242
"#;

const VORTEX_VELO_TABLE: &str = r#"
units: MPH
step: 20
scale: 2.237
product: BV
color: 0 115 115 115
color: .1 134 113 116
color: 5 130 3 3
color: 30 238 0 0
color: 40 255 87 1
color: 55 255 143 1
color: 70 255 239 2
color: 90 255 252 81
color: 120 255 255 255
color: 130 128 128 128
color: -4.99 70 129 68
color: -5 2 139 2
color: -30 4 239 16
color: -40 4 169 86
color: -55 4 92 162
color: -70 4 5 254
color: -90 4 87 254
color: -110 5 177 255
color: -130 0 255 255
"#;

const TORNADO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 236 255 255
color: -58 126 220 255
color: -48 166 236 255
color: -38 210 250 255
color: -30 246 255 255
color: -24 232 255 250
color: -18 0 156 54
color: -13 18 232 54
color: -9 82 244 104
color: -5 36 136 54
color: -2 84 100 84
color: 0 112 112 112
color: 2 120 86 84
color: 5 154 46 44
color: 9 216 28 28
color: 14 255 34 40
color: 20 242 0 0
color: 24 255 238 218
color: 28 255 255 238
color: 34 255 224 168
color: 42 255 248 220
color: 50 255 255 240
color: 58 255 230 190
color: 64 255 202 130
color: 70 255 240 204
"#;

const GR2_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 255 255
color: -55 0 170 255
color: -42 0 80 255
color: -32 0 180 80
color: -24 0 220 0
color: -16 0 148 0
color: -8 74 132 74
color: -2 96 108 96
color: 0 128 128 128
color: 2 126 94 94
color: 8 156 44 44
color: 16 198 0 0
color: 24 244 0 0
color: 32 255 116 0
color: 42 255 220 0
color: 55 255 255 255
color: 70 172 172 172
"#;

const TIGHT_COUPLET_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 230 255 255
color: -50 54 236 214
color: -36 0 188 122
color: -26 0 114 48
color: -18 0 176 34
color: -12 32 252 46
color: -7 0 176 34
color: -3 36 112 50
color: -1 78 94 78
color: 0 112 112 112
color: 1 112 78 78
color: 3 152 36 36
color: 7 246 22 22
color: 12 255 42 42
color: 18 202 0 0
color: 26 142 0 0
color: 36 110 0 0
color: 50 238 124 132
color: 70 255 255 255
"#;

const RADARSCOPE_CONTRAST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 216 255 255
color: -58 126 220 255
color: -48 166 236 255
color: -38 210 250 255
color: -30 246 255 255
color: -24 232 255 250
color: -22 210 248 226
color: -16 0 224 54
color: -11 42 255 66
color: -7 106 240 116
color: -4 46 134 54
color: -1 98 104 96
color: 0 122 122 122
color: 1 128 96 96
color: 4 156 64 62
color: 7 198 42 42
color: 11 246 28 28
color: 16 255 40 46
color: 22 244 0 24
color: 24 255 238 218
color: 28 255 255 238
color: 36 255 220 172
color: 44 255 250 224
color: 50 255 255 238
color: 56 255 232 190
color: 62 255 204 134
color: 70 255 242 202
"#;

const SIGN_CHECK_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
rf: 180 80 255 255
color: -100 0 0 255
color: -0.01 0 0 255
color: 0 120 120 120
color: 0.01 255 0 0
color: 100 255 0 0
"#;

const COUPLET_POP_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 238 255 255
color: -58 92 238 216
color: -46 20 206 152
color: -36 0 150 82
color: -28 0 92 42
color: -21 0 172 58
color: -15 0 236 44
color: -10 34 186 48
color: -6 36 122 50
color: -2 78 98 76
color: 0 92 92 92
color: 2 104 72 70
color: 6 132 34 34
color: 10 214 24 24
color: 15 255 34 34
color: 21 236 16 38
color: 28 180 8 34
color: 36 122 6 34
color: 46 196 78 96
color: 58 240 184 190
color: 70 255 255 255
"#;

const GR2_ISH_ANALYST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 252 252
color: -55 0 174 244
color: -42 20 90 238
color: -32 0 176 82
color: -24 0 214 0
color: -16 0 150 0
color: -8 74 132 74
color: -2 96 108 96
color: 0 124 124 124
color: 2 126 94 94
color: 8 160 42 42
color: 16 204 0 0
color: 24 246 0 0
color: 32 255 92 38
color: 42 246 156 128
color: 55 255 222 222
color: 70 172 172 172
"#;

const SUBTLE_SRV_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 1
color: -70 184 236 230
color: -55 90 206 190
color: -42 32 168 132
color: -32 12 122 76
color: -24 18 88 52
color: -16 36 140 64
color: -10 62 196 82
color: -5 58 132 70
color: -1 82 98 84
color: 0 94 94 94
color: 1 104 86 84
color: 5 128 58 54
color: 10 188 52 48
color: 16 222 64 58
color: 24 184 42 54
color: 32 138 34 54
color: 42 190 96 114
color: 55 224 184 190
color: 70 242 242 242
"#;

const NWS_SPLIT_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 0 240 240
color: -55 0 150 240
color: -42 0 62 220
color: -32 0 150 60
color: -24 0 210 0
color: -16 0 136 0
color: -8 76 140 76
color: -2 104 118 104
color: 0 130 130 130
color: 2 142 104 104
color: 8 168 54 54
color: 16 210 0 0
color: 24 248 0 0
color: 32 255 118 0
color: 42 255 226 0
color: 55 255 255 255
color: 70 170 170 170
"#;

const DARK_ANALYST_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
step: 2
color: -70 210 246 240
color: -55 82 210 196
color: -42 0 164 126
color: -32 0 114 68
color: -24 0 80 44
color: -16 0 142 50
color: -10 20 206 42
color: -5 34 126 46
color: -1 72 88 74
color: 0 94 94 94
color: 1 102 72 72
color: 5 132 34 34
color: 10 208 24 24
color: 16 238 42 42
color: 24 188 18 36
color: 32 128 16 36
color: 42 198 92 112
color: 55 232 202 206
color: 70 250 250 250
"#;

const ANALYST_PRO_VELOCITY_TABLE: &str = r#"
product: BV
units: m/s
mode: stepped
color: -70 222 255 255
color: -58 126 220 255
color: -46 170 238 255
color: -36 214 250 255
color: -28 246 255 255
color: -24 232 255 250
color: -21 210 248 226
color: -15 0 226 58
color: -10 42 214 70
color: -6 42 132 54
color: -2 82 98 80
color: 0 110 110 110
color: 2 116 84 84
color: 6 148 42 42
color: 10 204 30 30
color: 15 248 36 42
color: 21 255 78 86
color: 24 255 238 218
color: 28 255 255 238
color: 36 255 222 174
color: 46 255 250 226
color: 58 255 255 238
color: 66 255 210 146
color: 70 255 240 220
"#;

const NWS_VELOCITY_TABLE: &str = r#"
product: BV
units: kt
color: -120 0 255 255
color: -100 0 160 255
color: -80 0 64 255
color: -60 0 160 80
color: -40 0 220 0
color: -20 0 128 0
color: -5 85 145 85
color: 0 128 128 128
color: 5 150 90 90
color: 20 160 0 0
color: 40 230 0 0
color: 60 255 130 0
color: 80 255 230 0
color: 100 255 255 255
color: 120 170 170 170
"#;

// The classic WDT/RadarScope velocity ramp (WDT = Weather Decision
// Technologies, RadarScope's publisher), as its GR `.pal` text stands, via the
// BowEcho preset. Two-colour ramp rows throughout: each row's second triple is
// the colour the segment reaches just before the next row, which is what makes
// the band edges hard and the insides of the bands a gradient. Values are
// knots; `scale:` converts to m/s at parse time.
const GENERIC_RADAR_VELOCITY_TABLE: &str = r#"
product: BV
units: KTS
scale: 1.9426
mode: smooth
color: 200 45 0 0
color: 140 60 0 0
color: 120 97 6 2
color: 80 254 137 80
color: 60 255 230 169 255 151 86
color: 55 255 157 206 255 221 176
color: 40 249 58 84 255 142 212
color: 10 105 0 0 242 1 6
color: 0 130 106 120 122 48 57
color: -10 72 112 71 106 125 105
color: -40 10 248 35 15 99 20
color: -50 180 240 243 33 253 50
color: -70 55 226 229 172 239 242
color: -90 25 1 142 47 215 225
color: -100 105 2 142 32 1 141
color: -120 250 4 130 114 3 141
color: -140 255 20 180
color: -200 255 220 220
RF: 123 0 200
"#;

// The AWIPS colour table by Karl Schneider ("Wilson edit"), as its GR `.pal`
// text stands, via the BowEcho preset. Ramp pairs throughout, same as the
// velocity table above. The bottom row is transparent and declares an opaque
// second colour, so everything below -30 dBZ stays off the scope and [-30,
// -20) fades in.
const AWIPS_WILSON_REFLECTIVITY_TABLE: &str = r#"
product: BR
units: dBZ
mode: smooth
color4: -30 116 78 173 0 147 141 117 255
color: -20 150 145 83 210 212 180
color: -10 204 207 180 65 91 158
color: 10 67 97 162 106 208 228
color: 18 111 214 232 53 213 91
color: 22 17 213 24 9 94 9
color: 35 29 104 9 234 210 4
color: 40 255 226 0 255 128 0
color: 50 255 0 0 113 0 0
color: 60 255 255 255 255 146 255
color: 65 255 117 255 225 11 227
color: 70 178 0 255 99 0 214
color: 75 5 236 240 1 32 32
color: 85 1 32 32
color: 95 1 32 32
"#;

#[cfg(test)]
mod port_fidelity {
    use super::*;

    /// The original BowEcho/GR texts, ramp pairs and all, kept here as a
    /// second copy that this crate's parser never touches.
    ///
    /// The shipped constants above are now these same texts, but the sampler
    /// below is a from-scratch reading of the `.pal` semantics rather than a
    /// call into the production one, so the sweep still answers the question
    /// it always answered: does the shipped table paint what the file says,
    /// colour for colour. A parser change that quietly loses a second colour,
    /// or a sampling change that ramps to the wrong target, disagrees here.
    const WDT_ORIGINAL: &str = "
color: 200 45 0 0
color: 140 60 0 0
color: 120 97 6 2
color: 80 254 137 80
color: 60 255 230 169 255 151 86
color: 55 255 157 206 255 221 176
color: 40 249 58 84 255 142 212
color: 10 105 0 0 242 1 6
color: 0 130 106 120 122 48 57
color: -10 72 112 71 106 125 105
color: -40 10 248 35 15 99 20
color: -50 180 240 243 33 253 50
color: -70 55 226 229 172 239 242
color: -90 25 1 142 47 215 225
color: -100 105 2 142 32 1 141
color: -120 250 4 130 114 3 141
color: -140 255 20 180
color: -200 255 220 220
";

    const WILSON_ORIGINAL: &str = "
color4: -30 116 78 173 0 147 141 117 255
color: -20 150 145 83 210 212 180
color: -10 204 207 180 65 91 158
color: 10 67 97 162 106 208 228
color: 18 111 214 232 53 213 91
color: 22 17 213 24 9 94 9
color: 35 29 104 9 234 210 4
color: 40 255 226 0 255 128 0
color: 50 255 0 0 113 0 0
color: 60 255 255 255 255 146 255
color: 65 255 117 255 225 11 227
color: 70 178 0 255 99 0 214
color: 75 5 236 240 1 32 32
color: 85 1 32 32
color: 95 1 32 32
";

    struct GrStop {
        value: f32,
        color: [u8; 4],
        end: Option<[u8; 4]>,
    }

    /// GR `.pal` two-colour rows, scaled into display units.
    fn parse_gr(text: &str, scale: f32) -> Vec<GrStop> {
        let mut stops: Vec<GrStop> = text
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let (key, rest) = line.split_once(':')?;
                let with_alpha = match key.trim().to_ascii_lowercase().as_str() {
                    "color" => false,
                    "color4" => true,
                    _ => return None,
                };
                let numbers: Vec<f32> = rest
                    .split_whitespace()
                    .map(|token| token.parse::<f32>().expect("test table is numeric"))
                    .collect();
                let head = if with_alpha { 5 } else { 4 };
                let color = |slice: &[f32]| {
                    let mut rgba = [0_u8, 0, 0, 255];
                    for (target, source) in rgba.iter_mut().zip(slice) {
                        *target = *source as u8;
                    }
                    if !with_alpha {
                        rgba[3] = 255;
                    }
                    rgba
                };
                let end = (numbers.len() > head).then(|| color(&numbers[head..]));
                Some(GrStop {
                    value: numbers[0] * scale,
                    color: color(&numbers[1..head]),
                    end,
                })
            })
            .collect();
        stops.sort_by(|a, b| a.value.total_cmp(&b.value));
        stops
    }

    fn lerp_u8(left: u8, right: u8, amount: f32) -> u8 {
        ((left as f32 + (right as f32 - left as f32) * amount).round()).clamp(0.0, 255.0) as u8
    }

    /// BowEcho's GrPal sampler, verbatim semantics: each segment ramps from
    /// its own colour to its declared end colour, or to the next stop's
    /// colour when none is declared, or stays put when it is transparent.
    fn sample_gr(stops: &[GrStop], value: f32) -> [u8; 4] {
        let first = &stops[0];
        if value <= first.value {
            return first.color;
        }
        let index = stops.partition_point(|stop| stop.value <= value);
        let stop = &stops[index - 1];
        let Some(next) = stops.get(index) else {
            return stop.color;
        };
        let end = stop.end.unwrap_or(if stop.color[3] == 0 {
            stop.color
        } else {
            next.color
        });
        let span = (next.value - stop.value).max(f32::EPSILON);
        let t = ((value - stop.value) / span).clamp(0.0, 1.0);
        [
            lerp_u8(stop.color[0], end[0], t),
            lerp_u8(stop.color[1], end[1], t),
            lerp_u8(stop.color[2], end[2], t),
            lerp_u8(stop.color[3], end[3], t),
        ]
    }

    /// Sweep a shipped port against the reference, densely, and demand exact
    /// agreement everywhere.
    ///
    /// No skipped window and no per-channel tolerance. Both existed to cover
    /// the hand expansion's 0.01-unit break, which was the one place the port
    /// and the original could not agree; the port is the original's own rows
    /// now, so a single byte out anywhere is a real disagreement.
    fn assert_port_matches(shipped: &ColorTable, stops: &[GrStop], from: f32, to: f32) {
        let mut value = from;
        let mut compared = 0_u32;
        while value <= to {
            let got = shipped.sample(value).to_array();
            let want = sample_gr(stops, value);
            assert_eq!(
                got,
                want,
                "{} at {value}: shipped {got:?} vs original {want:?}",
                shipped.name()
            );
            compared += 1;
            value += 0.05;
        }
        assert!(compared > 2_000, "the sweep barely sampled: {compared}");
        // At every declared row the port must agree exactly, not within one.
        for stop in stops {
            assert_eq!(
                shipped.sample(stop.value).to_array(),
                stop.color,
                "{} at declared row {}",
                shipped.name(),
                stop.value
            );
        }
    }

    #[test]
    fn the_genericradar_velocity_port_matches_the_gr_pal_original() {
        let shipped = generic_radar_velocity_table();
        let stops = parse_gr(WDT_ORIGINAL, 1.0 / 1.9426);
        assert_port_matches(&shipped, &stops, -104.0, 104.0);
        // The declared purple; the alpha is this crate's own range-folded
        // convention, not the table's to override.
        let range_folded = shipped.range_folded_rgba().to_array();
        assert_eq!(&range_folded[..3], &[123, 0, 200]);
    }

    #[test]
    fn the_wilson_reflectivity_port_matches_the_gr_pal_original() {
        let shipped = awips_wilson_reflectivity_table();
        let stops = parse_gr(WILSON_ORIGINAL, 1.0);
        assert_port_matches(&shipped, &stops, -33.0, 97.0);
        // The bottom of the table is transparent: the noise floor stays off
        // the scope, and the first band fades in rather than popping.
        assert_eq!(shipped.sample(-31.0).to_array()[3], 0);
        assert_eq!(shipped.sample(-30.0).to_array()[3], 0);
        assert!(shipped.sample(-25.0).to_array()[3] > 90);
    }

    /// Fresh installs draw these two, exactly as shipped.
    #[test]
    fn the_two_base_moment_defaults_are_the_ported_looks() {
        assert_eq!(
            builtin_velocity_table().name(),
            "GenericRadar VEL (interpolated)"
        );
        assert_eq!(
            builtin_reflectivity_table().name(),
            "AWIPS Wilson REF (interpolated)"
        );
    }
}
