//! The number under the cursor, read out of the sweep the pane is actually
//! drawing.
//!
//! A probe that samples a different sweep, a different gate, or a different
//! geometry from the one on screen is worse than no probe at all: it reads out
//! a number for a pixel that is not there, and nothing about the readout looks
//! wrong. Every choice in this module is therefore made to agree with the
//! renderer, not to be independently correct.
//!
//! Three of those choices are worth stating up front, because each of them is
//! a way a plausible-looking probe goes wrong:
//!
//! 1. **The gate index is `((range_m - first_gate_m) / gate_spacing_m).round()`**,
//!    which is what `render2d` does. Gate `g` is therefore *centred* at
//!    `first_gate_m + g * gate_spacing_m`. The other common idiom, in which a
//!    gate spans `[g * spacing, (g + 1) * spacing)` and the index is a floor,
//!    shifts every readout by half a gate - 125 m on a NEXRAD super-resolution
//!    sweep - and no test that samples a gate centre would ever catch it.
//! 2. **The range used for that lookup is the screen distance from the radar.**
//!    The raster path applies neither `cos(elevation)` nor earth curvature: it
//!    plots slant range as if it were ground range. Agreeing with the pixel and
//!    being physically correct are therefore different things here, and this
//!    module deliberately agrees with the pixel. See [`probe_polar`].
//! 3. **Azimuth is the compass bearing `atan2(east, north)`**, not the
//!    mathematical `atan2(north, east)`. The two agree on the 45 degree
//!    diagonal and nowhere else, so a symmetric storm looks almost right under
//!    the wrong one.

use product_engine::domain::DisplayDomain;
use product_engine::stats::CellState;
use radar_core::{ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, RadarVolume};
use render2d::GateFilterMask;
use render2d::beam;

use crate::units::UnitSystem;

/// How far from a radial a cursor may be and still be reading that radial.
///
/// `render2d` paints each radial's wedge out to half the gap to its neighbour,
/// capped at 3 degrees on each side (`MAX_AZIMUTH_HALF_WIDTH_DEG`). Beyond that
/// the renderer paints nothing, so a probe that kept walking the sweep for a
/// nearest radial would report a value for a pixel that is background. A sector
/// scan with a 60 degree gap is the case this prevents.
const MAX_AZIMUTH_MATCH_DEG: f64 = 3.0;

/// Where the cursor is, in the radar's own frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeLocation {
    pub east_km: f64,
    pub north_km: f64,
    pub azimuth_deg: f64,
    /// Straight-line distance from the radar on the map. The renderer plots
    /// slant range as ground range, so this is what is on screen.
    pub screen_range_km: f64,
}

/// A gate that was found and read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeValue {
    /// The value in engine units - dBZ, m/s, dB - never a display unit. Pass it
    /// through [`DisplayDomain::format_display`] to show it.
    pub engine_value: f32,
    /// Always [`CellState::Valid`] from this module: a base moment either has a
    /// number or it does not. The field exists so a derived field sampled by
    /// the same readout can say `AT LEAST` without a second code path.
    pub state: CellState,
    pub location: ProbeLocation,
    /// Row within the moment grid, not the radial index within the cut. The two
    /// differ whenever a moment is absent from some radials.
    pub row: usize,
    pub gate: usize,
    /// Range to the centre of the gate that was read, in metres, which is the
    /// cursor's screen distance rounded onto the gate ladder. Reported rather
    /// than the cursor's own distance so that two clicks inside one gate give
    /// one height, which is what a couplet measurement needs.
    pub slant_range_m: f64,
    pub beam_height_arl_m: f64,
    /// Height above sea level, present only when the site elevation is known.
    /// A radar at 370 m that reports its echoes from sea level puts the melting
    /// layer 370 m wrong, so this is never defaulted.
    pub beam_height_msl_m: Option<f64>,
    pub elevation_deg: f32,
    pub cut_index: usize,
}

/// What the cursor found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProbeReading {
    Value(ProbeValue),
    /// A gate was found, and it holds no number. `state` says which kind of
    /// nothing: a beam that swept and found no echo, a range-folded gate whose
    /// value belongs at another range, or unusable data.
    ///
    /// The indices ride along for the same reason [`ProbeValue`] carries them:
    /// a gate that is THERE and blank is still a gate, and a reader that wants
    /// to say why it is blank - the Level 1 spectrum panel does - has to be
    /// able to find it. Without them the only honest answer a reader could
    /// give was silence, which is how "this gate is empty, and here is why"
    /// became "no panel at all" for exactly the gates the question is about.
    Absent {
        state: CellState,
        location: ProbeLocation,
        /// Row within the moment grid. See [`ProbeValue::row`].
        row: usize,
        gate: usize,
        /// Range to the centre of the gate, metres, as in
        /// [`ProbeValue::slant_range_m`].
        slant_range_m: f64,
    },
    /// No gate at all: past the last gate, inside the first, in an azimuth gap
    /// the renderer leaves unpainted, or in a cut or moment that is not there.
    OutsideSweep(ProbeLocation),
}

// These three are what a caller needs to turn a reading into something else -
// a couplet sample, a status line, a cursor colour. In a binary crate `pub`
// does not satisfy the `dead_code` lint, and clippy runs with `-D warnings`,
// so the allow stands until the pane uses them.
#[allow(dead_code)]
impl ProbeReading {
    pub fn location(&self) -> &ProbeLocation {
        match self {
            Self::Value(value) => &value.location,
            Self::Absent { location, .. } | Self::OutsideSweep(location) => location,
        }
    }

    /// The gate, when one was read. `None` is the honest answer everywhere
    /// else, and forces a caller to decide what to draw instead of getting a
    /// zero that looks like a measurement.
    pub fn value(&self) -> Option<&ProbeValue> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent { .. } | Self::OutsideSweep(_) => None,
        }
    }

    /// The cell state of any reading, so a caller can label all three cases the
    /// same way.
    pub fn state(&self) -> CellState {
        match self {
            Self::Value(value) => value.state,
            Self::Absent { state, .. } => *state,
            Self::OutsideSweep(_) => CellState::NoCoverage,
        }
    }
}

/// Sample one moment grid of one cut at a radar-local point.
///
/// `east_km` and `north_km` are the cursor in the same radar-local kilometres
/// the renderer draws in. `elevation_deg` is the elevation the pane is showing
/// for this sweep, passed in rather than read from the cut so that the readout
/// and the pane header can never disagree. `site_elevation_m` is `None` when
/// the site record has no elevation; it is not defaulted to sea level.
///
/// # The one place the probe is deliberately not physical
///
/// The gate lookup uses the cursor's **screen** distance from the radar. The
/// raster path plots slant range as ground range, so screen distance *is* slant
/// range as far as the picture is concerned, and using it is what makes the
/// probe read the gate whose colour is under the cursor.
///
/// The beam height is then computed from that same distance treated as a slant
/// range. A physically correct ground range would disagree: at 19.5 degrees
/// elevation the slant range that reaches a given ground distance is about
/// 6 percent longer (`cos(19.5 deg) = 0.943`, plus curvature), which at 43 km
/// is eleven 250 m gates. Agreeing with the pixel was chosen deliberately; the
/// alternative is a probe whose number does not belong to the colour it is
/// pointing at. Callers that need a true ground range must ask
/// [`beam::ground_arc_m`] for it and label it as such.
///
/// # The gate filter
///
/// `censor` is the pane's [`GateFilterMask`], which
/// [`render2d::ViewportMomentCache::gate_filter_mask`] hands back indexed
/// against the grid as it sits in the cut - the same grid this function reads -
/// so a censored gate can be recognised here without the readout having to know
/// anything about how it was censored.
///
/// It is a parameter and not an option because forgetting it is the failure
/// this whole module is written against: without it the readout answers a
/// censored gate with its true value and `CellState::Valid`, at a pixel the
/// pane deliberately drew empty, and the analyst is told a number that is not
/// on the screen. `None` is the honest answer for a pane with no filter on.
// Eight arguments, one over clippy's threshold. Every one of them is a thing a
// caller must decide for itself and none of them has a safe default: bundling
// them into a struct would move that decision into a `Default` impl, where a
// forgotten `censor` becomes an invisible field rather than a compile error.
// The compile error is the point.
#[allow(clippy::too_many_arguments)]
pub fn probe_polar(
    volume: &RadarVolume,
    cut_index: usize,
    moment: &MomentType,
    elevation_deg: f32,
    site_elevation_m: Option<f32>,
    east_km: f64,
    north_km: f64,
    censor: Option<&GateFilterMask>,
) -> ProbeReading {
    let location = ProbeLocation {
        east_km,
        north_km,
        azimuth_deg: beam::compass_azimuth_deg(east_km, north_km),
        screen_range_km: east_km.hypot(north_km),
    };

    let Some(cut) = volume.cuts.get(cut_index) else {
        return ProbeReading::OutsideSweep(location);
    };
    let Some(grid) = cut.moments.get(moment) else {
        return ProbeReading::OutsideSweep(location);
    };
    let Some(gate) = gate_for_range(&grid.gate_range, location.screen_range_km * 1000.0) else {
        return ProbeReading::OutsideSweep(location);
    };
    let Some(row) = nearest_row(cut, grid, location.azimuth_deg) else {
        return ProbeReading::OutsideSweep(location);
    };

    let slant_range_m = gate_centre_range_m(&grid.gate_range, gate);
    let beam_height_arl_m = beam::beam_height_arl_m(slant_range_m, f64::from(elevation_deg));

    // A gate the pane's filter removed is not there as far as the readout is
    // concerned. Reporting its value would put a number under the cursor at a
    // pixel that was deliberately left empty, which is the one thing a censored
    // display must never do.
    if censor.is_some_and(|censor| censor.hides(row, gate)) {
        return ProbeReading::Absent {
            state: CellState::QualityMasked,
            location,
            row,
            gate,
            slant_range_m,
        };
    }

    match grid.scaled_value(row, gate) {
        Some(engine_value) if engine_value.is_finite() => ProbeReading::Value(ProbeValue {
            engine_value,
            state: CellState::Valid,
            location,
            row,
            gate,
            slant_range_m,
            beam_height_arl_m,
            beam_height_msl_m: site_elevation_m
                .map(|elevation| beam_height_arl_m + f64::from(elevation)),
            elevation_deg,
            cut_index,
        }),
        // Only a floating-point grid can hold a non-finite number; the integer
        // encodings spend their sentinel words on nodata and range folding.
        Some(_) => ProbeReading::Absent {
            state: CellState::NoData,
            location,
            row,
            gate,
            slant_range_m,
        },
        None => ProbeReading::Absent {
            state: absent_state(grid, raw_word(grid, row, gate)),
            location,
            row,
            gate,
            slant_range_m,
        },
    }
}

/// A one-line readout, e.g.
/// `REF 52.5 dBZ | 41.1 km 247.4 deg | row 712 gate 164 | beam 0.73 km ARL`
///
/// The distance is the screen distance from the radar, which is what the range
/// rings measure. The MSL height is appended only when the site elevation is
/// known.
///
/// `units` decides only how the distance and the two heights are WRITTEN - the
/// sampled gate, the slant range and the beam height are all unchanged, which
/// is why they are passed in as the metres and kilometres the geometry
/// produced. Under [`UnitSystem::default`] every character is what this
/// function has always written; the annotation's `range_decimals` is likewise
/// the `1` the format string carried.
pub fn format_reading(
    reading: &ProbeReading,
    domain: &DisplayDomain,
    short_name: &str,
    units: UnitSystem,
    range_decimals: u8,
) -> String {
    match reading {
        ProbeReading::Value(value) => {
            let qualifier = match value.state.label() {
                "" => String::new(),
                label => format!("{label} "),
            };
            let mut text = format!(
                "{short_name} {qualifier}{} | {} {:.1} deg | row {} gate {} | beam {} ARL",
                domain.format_display(value.engine_value),
                units.distance(value.location.screen_range_km, range_decimals),
                value.location.azimuth_deg,
                value.row,
                value.gate,
                units.altitude(value.beam_height_arl_m, 2),
            );
            if let Some(msl_m) = value.beam_height_msl_m {
                text.push_str(&format!(" / {} MSL", units.altitude(msl_m, 2)));
            }
            text
        }
        ProbeReading::Absent {
            state, location, ..
        } => format_absent(short_name, *state, location, units, range_decimals),
        ProbeReading::OutsideSweep(location) => format_absent(
            short_name,
            CellState::NoCoverage,
            location,
            units,
            range_decimals,
        ),
    }
}

fn format_absent(
    short_name: &str,
    state: CellState,
    location: &ProbeLocation,
    units: UnitSystem,
    range_decimals: u8,
) -> String {
    format!(
        "{short_name} {} | {} {:.1} deg",
        absent_label(state),
        units.distance(location.screen_range_km, range_decimals),
        location.azimuth_deg,
    )
}

/// What an absent gate is called on the glass.
///
/// [`CellState::label`] everywhere except one: `QualityMasked` reads "QUALITY
/// MASKED", which names a judgement the RADAR made about its own data, and
/// inside this module it means the opposite - the only thing that produces it
/// in [`probe_polar`] is the pane's own gate filter, because `absent_state`
/// spends its words on range folding, no-echo and no-data and never returns
/// it.
///
/// So it is named after the thing that caused it, in the same word the pane's
/// band and the legend badge use. An analyst who hovers a gap they created and
/// reads "QUALITY MASKED" has been told the radar threw the gate away; the
/// readout, the band and the badge now agree, and they agree in one vocabulary.
fn absent_label(state: CellState) -> &'static str {
    match state {
        CellState::QualityMasked => crate::gate_filter_ui::FILTERED_WORD,
        other => other.label(),
    }
}

/// The gate the renderer would paint at this range, or `None` when the range
/// falls off either end of the gate ladder.
///
/// `((range_m - first_gate_m) / gate_spacing_m).round()`, exactly as in
/// `render2d`. The renderer does this in `f32` and this does it in `f64`; the
/// difference is far below a gate at every range a radar reaches, and `f64`
/// keeps the arithmetic here consistent with the rest of the geometry.
fn gate_for_range(gate_range: &GateRange, range_m: f64) -> Option<usize> {
    if !range_m.is_finite() {
        return None;
    }
    // `max(1)` mirrors the renderer, which refuses to divide by a zero spacing
    // rather than producing an infinite gate index.
    let spacing_m = f64::from(gate_range.gate_spacing_m.max(1));
    let gate = ((range_m - f64::from(gate_range.first_gate_m)) / spacing_m).round();
    if gate < 0.0 {
        return None;
    }
    // A saturating cast: an absurd range becomes `usize::MAX` and fails the
    // gate-count test below rather than wrapping into a valid-looking index.
    let gate = gate as usize;
    (gate < gate_range.gate_count).then_some(gate)
}

/// The range to the centre of a gate.
///
/// `first_gate_m + gate * gate_spacing_m`, not `(gate + 0.5) * gate_spacing_m`.
/// This is the inverse of [`gate_for_range`] and the two must stay that way.
fn gate_centre_range_m(gate_range: &GateRange, gate: usize) -> f64 {
    f64::from(gate_range.first_gate_m) + gate as f64 * f64::from(gate_range.gate_spacing_m.max(1))
}

/// The grid row whose radial is nearest this bearing, wrap-aware.
///
/// Rows are indexed within the moment grid, and the azimuth of a row comes from
/// the radial the grid points at: a moment absent from some radials makes row
/// and radial index diverge, and using one for the other silently rotates the
/// sweep.
fn nearest_row(cut: &ElevationCut, grid: &MomentGrid, azimuth_deg: f64) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for (row, radial_index) in grid.radial_indices.iter().enumerate() {
        let Some(radial) = cut.radials.get(*radial_index) else {
            continue;
        };
        let separation = angular_separation_deg(azimuth_deg, f64::from(radial.azimuth_deg));
        if best.is_none_or(|(best_separation, _)| separation < best_separation) {
            best = Some((separation, row));
        }
    }
    best.filter(|(separation, _)| *separation <= MAX_AZIMUTH_MATCH_DEG)
        .map(|(_, row)| row)
}

/// The smaller of the two ways round the circle, in degrees.
///
/// Without the wrap a cursor at 359.8 degrees would be judged 359.8 degrees
/// away from the radial at 0.0 and would read the wrong side of the storm.
fn angular_separation_deg(left_deg: f64, right_deg: f64) -> f64 {
    let delta = (left_deg - right_deg).rem_euclid(360.0);
    delta.min(360.0 - delta)
}

/// The stored word behind a gate, before scaling.
///
/// `MomentGrid::scaled_value` returns `None` for a range-folded gate and for a
/// nodata gate alike, so the sentinel has to be compared by hand or a gate the
/// renderer paints purple reads out as "no data". Floating-point grids have no
/// sentinel words, hence `None`.
fn raw_word(grid: &MomentGrid, row: usize, gate: usize) -> Option<u16> {
    if gate >= grid.gate_range.gate_count {
        return None;
    }
    let index = row
        .checked_mul(grid.gate_range.gate_count)?
        .checked_add(gate)?;
    match &grid.storage {
        MomentStorage::U8(values) => values.get(index).map(|word| u16::from(*word)),
        MomentStorage::U16(values) => values.get(index).copied(),
        MomentStorage::F32(_) => None,
    }
}

/// Which kind of nothing a gate holds.
///
/// Range folding is checked first: a gate whose word is both sentinels at once
/// is a broken encoding, and calling it range folded at least tells an analyst
/// its value belongs somewhere else.
///
/// The nodata word is also what `MomentGrid` pads a short row with, and the
/// grid keeps no per-row original length, so a padded gate and a genuinely
/// below-threshold gate are the same word and cannot be told apart here. Both
/// read as `NoEcho`. Nothing downstream may claim to distinguish them.
fn absent_state(grid: &MomentGrid, raw: Option<u16>) -> CellState {
    match raw {
        Some(word) if grid.range_folded == Some(word) => CellState::RangeFolded,
        Some(word) if grid.nodata == Some(word) => CellState::NoEcho,
        _ => CellState::NoData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use product_engine::registry::ProductRegistry;
    use radar_core::{RadarSite, Radial};

    const FIRST_GATE_M: i32 = 2_125;
    const GATE_SPACING_M: i32 = 250;
    const GATE_COUNT: usize = 200;

    /// NEXRAD 8-bit reflectivity: `dBZ = (word - 66) / 2`, so 171 is 52.5 dBZ.
    const WORD_52_5_DBZ: u8 = 171;
    /// The same encoding: 0 is below threshold, 1 is range folded.
    const WORD_BELOW_THRESHOLD: u8 = 0;
    const WORD_RANGE_FOLDED: u8 = 1;
    /// `(96 - 66) / 2`, i.e. 15.0 dBZ. Used where a test must prove it did not
    /// read the radial it was aiming at.
    const WORD_15_DBZ: u8 = 96;

    /// The gate whose centre is 43 125 m out: `(43125 - 2125) / 250`.
    const HOT_GATE: usize = 164;
    const HOT_GATE_RANGE_KM: f64 = 43.125;
    /// KTLX's antenna, so the MSL arithmetic has a real number in it.
    const SITE_ELEVATION_M: f32 = 370.0;

    fn gate_range() -> GateRange {
        GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        }
    }

    fn radial(azimuth_deg: f32, elevation_deg: f32) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg,
            time_offset_ms: 0,
            gate_range: gate_range(),
            nyquist_velocity_mps: Some(26.0),
            radial_status: None,
        }
    }

    /// One cut, one radial per listed azimuth, one reflectivity grid.
    ///
    /// Row `n` is the radial at `azimuths[n]`. The interesting gates sit on the
    /// 247 degree radial:
    ///
    /// - gate 164 (43.125 km): 52.5 dBZ
    /// - gate 165 (43.375 km): range folded
    /// - gate 166 (43.625 km): below threshold
    ///
    /// The radials at 0 and 84 degrees also hold 52.5 dBZ at gate 164, for the
    /// compass and wrap tests; those at 6 and 359 hold 15.0 dBZ there, so that
    /// picking the wrong radial produces a different number rather than a
    /// coincidence.
    fn volume_with_cut_elevation(elevation_deg: f32, azimuths: &[f32]) -> RadarVolume {
        let time = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("a fixed epoch second is a valid timestamp");
        let mut volume = RadarVolume::new(RadarSite::new("KTLX"), time);
        let cut = volume.push_cut(elevation_deg, Some(1));
        for azimuth in azimuths {
            cut.radials.push(radial(*azimuth, elevation_deg));
        }

        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range(),
            2.0,
            66.0,
            Some(WORD_BELOW_THRESHOLD),
            Some(WORD_RANGE_FOLDED),
        );
        for (row, azimuth) in azimuths.iter().enumerate() {
            let mut words = vec![WORD_BELOW_THRESHOLD; GATE_COUNT];
            match *azimuth as i32 {
                247 => {
                    words[HOT_GATE] = WORD_52_5_DBZ;
                    words[HOT_GATE + 1] = WORD_RANGE_FOLDED;
                    words[HOT_GATE + 2] = WORD_BELOW_THRESHOLD;
                }
                0 | 84 => words[HOT_GATE] = WORD_52_5_DBZ,
                6 | 359 => words[HOT_GATE] = WORD_15_DBZ,
                _ => {}
            }
            grid.push_u8_row_slice(row, &words)
                .expect("an 8-bit row belongs in an 8-bit grid");
        }
        cut.moments.insert(MomentType::Reflectivity, grid);
        volume
    }

    fn whole_degree_azimuths() -> Vec<f32> {
        (0..360).map(|degree| degree as f32).collect()
    }

    fn test_volume() -> RadarVolume {
        volume_with_cut_elevation(0.5, &whole_degree_azimuths())
    }

    /// East and north kilometres of a point on a compass bearing.
    fn point_at(azimuth_deg: f64, range_km: f64) -> (f64, f64) {
        let radians = azimuth_deg.to_radians();
        (range_km * radians.sin(), range_km * radians.cos())
    }

    fn probe_reflectivity(
        volume: &RadarVolume,
        elevation_deg: f32,
        site_elevation_m: Option<f32>,
        east_km: f64,
        north_km: f64,
    ) -> ProbeReading {
        probe_polar(
            volume,
            0,
            &MomentType::Reflectivity,
            elevation_deg,
            site_elevation_m,
            east_km,
            north_km,
            None,
        )
    }

    fn expect_value(reading: ProbeReading) -> ProbeValue {
        match reading {
            ProbeReading::Value(value) => value,
            other => panic!("expected a gate value, got {other:?}"),
        }
    }

    /// A gate the pane's filter removed reads as absent, not as its true value.
    ///
    /// This is the same gate `a_cursor_on_a_gate_centre_reads_that_gate` reads
    /// 52.5 dBZ out of, so the only thing that changes here is the censor. The
    /// pane draws that pixel empty; a readout that answered 52.5 dBZ over it
    /// would be telling the analyst a number that is not on the screen, and
    /// would be doing it at the exact moment they are asking what is there.
    #[test]
    fn a_gate_the_filter_removed_reads_as_absent_rather_than_as_its_value() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let source = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity");

        // A filter that hides everything, so the mask certainly covers the gate
        // the cursor is over. What is under test is the readout, not the
        // criterion.
        let outcome = render2d::evaluate_gate_filter(
            &volume,
            0,
            source,
            &render2d::GateFilter {
                min_reflectivity_dbz: Some(1_000.0),
                ..render2d::GateFilter::OFF
            },
        );
        let censor = outcome.mask.expect("every gate is below 1000 dBZ");
        assert!(censor.hides(247, HOT_GATE));

        let uncensored = expect_value(probe_reflectivity(&volume, 0.5, None, east_km, north_km));
        assert_eq!(uncensored.engine_value, 52.5);

        let reading = probe_polar(
            &volume,
            0,
            &MomentType::Reflectivity,
            0.5,
            None,
            east_km,
            north_km,
            Some(&censor),
        );
        assert_eq!(
            reading,
            ProbeReading::Absent {
                state: CellState::QualityMasked,
                location: *uncensored_location(&uncensored),
                row: uncensored.row,
                gate: uncensored.gate,
                slant_range_m: uncensored.slant_range_m,
            }
        );
        assert!(reading.value().is_none());
    }

    fn uncensored_location(value: &ProbeValue) -> &ProbeLocation {
        &value.location
    }

    /// And it says so in the pane's own word.
    ///
    /// The band over the pane, the legend badge and this readout are three
    /// different pieces of furniture describing one decision, so they use one
    /// word for it. "QUALITY MASKED" - `CellState`'s own label - would name a
    /// judgement the radar made about its data, at a gate the radar reported
    /// perfectly well and the ANALYST removed.
    #[test]
    fn a_filtered_gate_reads_under_the_cursor_in_the_word_the_pane_uses() {
        let reading = ProbeReading::Absent {
            state: CellState::QualityMasked,
            location: ProbeLocation {
                east_km: 10.0,
                north_km: 0.0,
                azimuth_deg: 90.0,
                screen_range_km: 10.0,
            },
            row: 0,
            gate: 40,
            slant_range_m: 10_000.0,
        };
        let text = format_reading(
            &reading,
            &reflectivity_domain(),
            "REF",
            UnitSystem::default(),
            1,
        );
        assert!(
            text.starts_with(&format!("REF {}", crate::gate_filter_ui::FILTERED_WORD)),
            "the readout does not use the pane's word: {text:?}"
        );
        assert!(
            !text.contains("QUALITY"),
            "the readout blames the radar for the analyst's filter: {text:?}"
        );
        // Every other absence keeps its own name.
        for state in [
            CellState::RangeFolded,
            CellState::NoEcho,
            CellState::NoData,
            CellState::NoCoverage,
        ] {
            let text = format_reading(
                &ProbeReading::Absent {
                    state,
                    location: ProbeLocation {
                        east_km: 10.0,
                        north_km: 0.0,
                        azimuth_deg: 90.0,
                        screen_range_km: 10.0,
                    },
                    row: 0,
                    gate: 40,
                    slant_range_m: 10_000.0,
                },
                &reflectivity_domain(),
                "REF",
                UnitSystem::default(),
                1,
            );
            assert!(
                text.contains(state.label()),
                "{state:?} lost its own name: {text:?}"
            );
            assert!(
                !text.contains(crate::gate_filter_ui::FILTERED_WORD),
                "{state:?} is being reported as the analyst's filter: {text:?}"
            );
        }
    }

    #[test]
    fn a_cursor_on_a_gate_centre_reads_that_gate() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let value = expect_value(probe_reflectivity(&volume, 0.5, None, east_km, north_km));
        assert_eq!(value.gate, HOT_GATE);
        assert_eq!(value.row, 247);
        assert_eq!(value.engine_value, 52.5);
        assert_eq!(value.cut_index, 0);
        assert_eq!(value.state, CellState::Valid);
        assert_eq!(value.slant_range_m, 43_125.0);
    }

    /// The half-gate idiom, in which gate `g` spans
    /// `[g * spacing, (g + 1) * spacing)`, agrees with the renderer at a gate
    /// centre and disagrees six tenths of a gate later: it would still be
    /// reporting 52.5 dBZ from gate 164 while the pixel under the cursor is the
    /// purple of range-folded gate 165.
    #[test]
    fn a_cursor_past_the_midpoint_moves_to_the_next_gate_as_the_renderer_does() {
        let volume = test_volume();
        let range_km = HOT_GATE_RANGE_KM + 0.6 * f64::from(GATE_SPACING_M) / 1000.0;
        let (east_km, north_km) = point_at(247.4, range_km);
        let reading = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(
            reading.state(),
            CellState::RangeFolded,
            "0.6 gates past the centre of gate 164 is gate 165, which is folded"
        );

        // Four tenths of a gate is still gate 164 - the rounding is symmetric.
        let range_km = HOT_GATE_RANGE_KM + 0.4 * f64::from(GATE_SPACING_M) / 1000.0;
        let (east_km, north_km) = point_at(247.4, range_km);
        let value = expect_value(probe_reflectivity(&volume, 0.5, None, east_km, north_km));
        assert_eq!(value.gate, HOT_GATE);
    }

    /// 42.9 km east and 4.29 km north is 84.29 degrees on a compass and 5.71
    /// degrees under the mathematical convention. The compass answer lands on
    /// the radial holding 52.5 dBZ; the mathematical one lands on the radial
    /// holding 15.0 dBZ, so a mirrored azimuth cannot pass this test.
    #[test]
    fn the_azimuth_is_a_compass_bearing_so_ten_east_and_one_north_reads_eighty_four_degrees() {
        let volume = test_volume();
        let value = expect_value(probe_reflectivity(&volume, 0.5, None, 42.9, 4.29));
        assert!(
            (value.location.azimuth_deg - 84.289_406_9).abs() < 1e-6,
            "bearing was {}",
            value.location.azimuth_deg
        );
        assert_eq!(value.row, 84);
        assert_eq!(value.gate, HOT_GATE);
        assert_eq!(value.engine_value, 52.5);
    }

    #[test]
    fn the_nearest_radial_is_found_across_the_north_wrap() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(359.8, HOT_GATE_RANGE_KM);
        let value = expect_value(probe_reflectivity(&volume, 0.5, None, east_km, north_km));
        assert_eq!(
            value.row, 0,
            "359.8 degrees is 0.2 degrees from the radial at 0 and 0.8 from the radial at 359"
        );
        assert_eq!(value.engine_value, 52.5);
    }

    /// `scaled_value` returns `None` for a folded gate and for a nodata gate
    /// alike. Reporting the folded gate as absent data would hide the one thing
    /// an analyst needs to know about it: its value belongs at another range.
    #[test]
    fn a_range_folded_gate_reads_as_range_folded_and_not_as_missing_data() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, 43.375);
        let reading = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(reading.state(), CellState::RangeFolded);
        assert_eq!(reading.value(), None);
    }

    /// The below-threshold word is also the word `MomentGrid` pads short rows
    /// with, and the grid keeps no per-row original length, so this test pins
    /// what the probe can honestly say: a beam swept here and returned nothing
    /// above threshold, or the row ended early. It cannot tell which.
    #[test]
    fn a_below_threshold_gate_reads_as_no_echo() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, 43.625);
        let reading = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(reading.state(), CellState::NoEcho);
    }

    #[test]
    fn a_cursor_beyond_the_last_gate_is_outside_the_sweep() {
        let volume = test_volume();
        // The ladder ends at 2125 + 200 * 250 = 52 125 m.
        let (east_km, north_km) = point_at(247.4, 120.0);
        let reading = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert!(matches!(reading, ProbeReading::OutsideSweep(_)));
        assert_eq!(reading.state(), CellState::NoCoverage);
    }

    #[test]
    fn a_cursor_inside_the_first_gate_is_outside_the_sweep() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, 0.5);
        assert!(matches!(
            probe_reflectivity(&volume, 0.5, None, east_km, north_km),
            ProbeReading::OutsideSweep(_)
        ));
    }

    /// A sector scan leaves a wedge of the screen unpainted. Walking the whole
    /// sweep for a nearest radial would put a number on that background.
    #[test]
    fn a_cursor_in_an_azimuth_gap_the_renderer_leaves_unpainted_is_outside_the_sweep() {
        let azimuths: Vec<f32> = (0..=300).map(|degree| degree as f32).collect();
        let volume = volume_with_cut_elevation(0.5, &azimuths);
        let (east_km, north_km) = point_at(330.0, HOT_GATE_RANGE_KM);
        assert!(
            matches!(
                probe_reflectivity(&volume, 0.5, None, east_km, north_km),
                ProbeReading::OutsideSweep(_)
            ),
            "330 degrees is 30 degrees from the nearest radial, far past the 3 degree wedge"
        );

        // Two degrees off the last radial is inside the wedge and still reads.
        let (east_km, north_km) = point_at(302.0, HOT_GATE_RANGE_KM);
        assert!(matches!(
            probe_reflectivity(&volume, 0.5, None, east_km, north_km),
            ProbeReading::Absent { .. }
        ));
    }

    #[test]
    fn a_missing_moment_or_a_missing_cut_is_outside_the_sweep_rather_than_a_zero() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        assert!(matches!(
            probe_polar(
                &volume,
                0,
                &MomentType::Velocity,
                0.5,
                None,
                east_km,
                north_km,
                None
            ),
            ProbeReading::OutsideSweep(_)
        ));
        assert!(matches!(
            probe_polar(
                &volume,
                7,
                &MomentType::Reflectivity,
                0.5,
                None,
                east_km,
                north_km,
                None
            ),
            ProbeReading::OutsideSweep(_)
        ));
    }

    /// Doviak and Zrnic (1993) eq. 2.28b over a 4/3 earth puts the 0.5 degree
    /// beam 485.78 m up at 43.125 km.
    #[test]
    fn the_beam_height_comes_from_the_gate_centre_range_and_the_shown_elevation() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let value = expect_value(probe_reflectivity(&volume, 0.5, None, east_km, north_km));
        assert_eq!(value.slant_range_m, 43_125.0);
        assert!(
            (value.beam_height_arl_m - 485.784_6).abs() < 0.01,
            "beam height was {}",
            value.beam_height_arl_m
        );
        assert_eq!(value.elevation_deg, 0.5);
    }

    /// A radar 370 m up that reports its echoes from sea level puts every
    /// height 370 m wrong, which is enough to move a melting layer. The MSL
    /// height is therefore absent rather than assumed.
    #[test]
    fn the_msl_height_appears_only_when_the_site_elevation_is_known() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);

        let unknown = expect_value(probe_reflectivity(&volume, 0.5, None, east_km, north_km));
        assert_eq!(unknown.beam_height_msl_m, None);

        let known = expect_value(probe_reflectivity(
            &volume,
            0.5,
            Some(SITE_ELEVATION_M),
            east_km,
            north_km,
        ));
        let msl_m = known
            .beam_height_msl_m
            .expect("a known site elevation must produce an MSL height");
        assert!(
            (msl_m - 855.784_6).abs() < 0.01,
            "MSL height was {msl_m}, expected 485.78 m above a 370 m antenna"
        );
    }

    /// The deliberate disagreement, pinned so that nobody "fixes" it by
    /// accident. On a 19.5 degree cut the cursor 43.125 km from the radar on
    /// screen is reading gate 164, because that is the gate whose colour is
    /// under it. The gate that is really 43.125 km across the ground is gate
    /// 175 - eleven gates and 2.75 km further out, about 6 percent.
    #[test]
    fn the_gate_comes_from_the_screen_distance_not_from_a_physical_ground_range() {
        let volume = volume_with_cut_elevation(19.5, &whole_degree_azimuths());
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let value = expect_value(probe_reflectivity(&volume, 19.5, None, east_km, north_km));
        assert_eq!(value.gate, HOT_GATE);

        let physical_slant_m = beam::slant_range_for_ground_arc_m(43_125.0, 19.5, 250_000.0)
            .expect("43 km of ground arc is well inside a 250 km sweep");
        let physical_gate =
            ((physical_slant_m - f64::from(FIRST_GATE_M)) / f64::from(GATE_SPACING_M)).round();
        assert_eq!(
            physical_gate, 175.0,
            "the physically correct gate is 175, and the probe deliberately reads 164"
        );
        assert!(
            (physical_slant_m / 43_125.0 - 1.063).abs() < 0.001,
            "the two conventions differ by about 6 percent at 19.5 degrees, got {}",
            physical_slant_m / 43_125.0
        );
    }

    fn reflectivity_domain() -> DisplayDomain {
        ProductRegistry::builtin()
            .get("REF")
            .expect("REF is a builtin product")
            .domain
    }

    #[test]
    fn the_readout_names_the_product_the_value_the_position_and_the_beam_height() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let reading = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(
            format_reading(
                &reading,
                &reflectivity_domain(),
                "REF",
                UnitSystem::default(),
                1
            ),
            "REF 52.5 dBZ | 43.1 km 247.4 deg | row 247 gate 164 | beam 0.49 km ARL"
        );
    }

    #[test]
    fn the_readout_adds_the_msl_height_when_the_site_elevation_is_known() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let reading = probe_reflectivity(&volume, 0.5, Some(SITE_ELEVATION_M), east_km, north_km);
        assert_eq!(
            format_reading(
                &reading,
                &reflectivity_domain(),
                "REF",
                UnitSystem::default(),
                1
            ),
            "REF 52.5 dBZ | 43.1 km 247.4 deg | row 247 gate 164 | beam 0.49 km ARL / 0.86 km MSL"
        );
    }

    /// The same gate, read in another set of units.
    ///
    /// This is the property the whole units feature rests on: 43.1 km is 26.8
    /// statute miles and 485.9 m ARL is 1594 ft, and NOTHING else about the
    /// line changes - same row, same gate, same 52.5 dBZ, same azimuth. The
    /// conversion happens at the character boundary and nowhere earlier.
    #[test]
    fn the_same_gate_reads_in_miles_and_feet_without_moving() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let reading = probe_reflectivity(&volume, 0.5, Some(SITE_ELEVATION_M), east_km, north_km);
        let imperial = UnitSystem {
            distance: crate::units::DistanceUnit::StatuteMiles,
            altitude: crate::units::AltitudeUnit::Feet,
            ..UnitSystem::default()
        };
        assert_eq!(
            format_reading(&reading, &reflectivity_domain(), "REF", imperial, 1),
            "REF 52.5 dBZ | 26.8 mi 247.4 deg | row 247 gate 164 | beam 1594 ft ARL / 2808 ft MSL"
        );
        // The reading itself never learned about miles.
        let ProbeReading::Value(value) = &reading else {
            panic!("the hot gate holds a value");
        };
        assert!((value.location.screen_range_km - 43.125).abs() < 0.01);
    }

    /// The precision setting reaches the same line.
    #[test]
    fn the_range_decimal_setting_changes_the_readout_and_nothing_else() {
        let volume = test_volume();
        let (east_km, north_km) = point_at(247.4, HOT_GATE_RANGE_KM);
        let reading = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(
            format_reading(
                &reading,
                &reflectivity_domain(),
                "REF",
                UnitSystem::default(),
                0
            ),
            "REF 52.5 dBZ | 43 km 247.4 deg | row 247 gate 164 | beam 0.49 km ARL"
        );
        assert_eq!(
            format_reading(
                &reading,
                &reflectivity_domain(),
                "REF",
                UnitSystem::default(),
                3
            ),
            "REF 52.5 dBZ | 43.125 km 247.4 deg | row 247 gate 164 | beam 0.49 km ARL"
        );
    }

    #[test]
    fn the_readout_says_which_kind_of_nothing_a_gate_holds() {
        let volume = test_volume();
        let domain = reflectivity_domain();

        let (east_km, north_km) = point_at(247.4, 43.625);
        let no_echo = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(
            format_reading(&no_echo, &domain, "REF", UnitSystem::default(), 1),
            "REF NO ECHO - BEAM SAMPLED THIS LOCATION | 43.6 km 247.4 deg"
        );

        let (east_km, north_km) = point_at(247.4, 43.375);
        let folded = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(
            format_reading(&folded, &domain, "REF", UnitSystem::default(), 1),
            "REF RANGE FOLDED | 43.4 km 247.4 deg"
        );

        let (east_km, north_km) = point_at(247.4, 120.0);
        let outside = probe_reflectivity(&volume, 0.5, None, east_km, north_km);
        assert_eq!(
            format_reading(&outside, &domain, "REF", UnitSystem::default(), 1),
            "REF OUTSIDE SWEEP - RADAR DID NOT SAMPLE THIS LOCATION | 120.0 km 247.4 deg"
        );
    }

    #[test]
    fn the_angular_separation_takes_the_short_way_round() {
        // 0.2 degrees is not exactly representable, so these are pinned to a
        // ten-thousandth of a degree - four orders of magnitude finer than the
        // 0.5 degree spacing of a super-resolution sweep.
        for (left, right) in [(359.8, 0.0), (0.0, 359.8), (0.1, 359.9)] {
            let separation = angular_separation_deg(left, right);
            assert!(
                (separation - 0.2).abs() < 1e-4,
                "{left} to {right} measured {separation} degrees, expected 0.2"
            );
        }
        assert_eq!(angular_separation_deg(10.0, 190.0), 180.0);
        assert_eq!(angular_separation_deg(45.0, 45.0), 0.0);
        assert_eq!(angular_separation_deg(0.0, 720.0), 0.0);
    }

    #[test]
    fn a_gate_index_round_trips_through_its_centre_range() {
        let range = gate_range();
        for gate in [0_usize, 1, 164, 199] {
            let centre_m = gate_centre_range_m(&range, gate);
            assert_eq!(
                gate_for_range(&range, centre_m),
                Some(gate),
                "gate {gate} centre is {centre_m} m"
            );
        }
        assert_eq!(
            gate_centre_range_m(&range, 0),
            f64::from(FIRST_GATE_M),
            "gate 0 is centred on the first gate, not half a gate beyond it"
        );
    }

    #[test]
    fn a_zero_gate_spacing_cannot_produce_an_infinite_gate_index() {
        let range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 0,
            gate_count: 10,
        };
        assert_eq!(gate_for_range(&range, 5.0), Some(5));
        assert_eq!(gate_for_range(&range, 1_000.0), None);
        assert_eq!(gate_for_range(&range, f64::NAN), None);
    }
}
