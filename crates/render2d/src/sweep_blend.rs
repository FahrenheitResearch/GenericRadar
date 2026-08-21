//! Draw a sweep that is still arriving on top of the last complete one.
//!
//! A live volume arrives in chunks. Part way through a tilt the radar has
//! looked at, say, 240 of the 360 azimuths it will eventually deliver, and the
//! remaining wedge holds no data yet. Painting that wedge empty tells the
//! viewer "there is no storm there", when the truth is only "the antenna has
//! not come round to there yet this pass". On a real KTLX 2026-08-17 07:24:02
//! VCP 212 partial cut the hole was 121 degrees wide and it straddled the
//! 0/360 seam (radials ran from 197.5 degrees upward, wrapping through zero and
//! ending at 76.5), so a third of the display would have blinked out once per
//! volume. Wrap handling is the normal case here, not an edge case.
//!
//! The fix is to under-paint the unswept wedge with the previous complete sweep
//! of the same tilt. New data then wipes over old clockwise, like the trace on
//! a PPI scope, and the picture is never blank.
//!
//! # What decides ownership of a pixel
//!
//! Ownership is decided purely by ANGLE, never by "does the incoming sweep
//! happen to have data here". Two failures follow from getting that wrong:
//!
//! * The incoming sweep's azimuth lookup deliberately smears its first and last
//!   radial up to 3 degrees outward to close the gaps between radials. If the
//!   presence of data decided ownership, that smear would paint a 3 degree
//!   tongue of new data over the old sweep at both ends of the arc, so the
//!   leading edge would look ragged and would jitter frame to frame.
//! * Inside the swept arc a gate with no echo means the echo is GONE. Falling
//!   back to the previous sweep there would resurrect a storm that has actually
//!   moved on or dissipated, which is a worse lie than an empty wedge.
//!
//! So: inside the revealed arc the incoming sweep is authoritative even where
//! it is empty; outside it, the previous sweep shows through untouched.
//!
//! # Where `revealed_deg` comes from
//!
//! Not from this module, and never from a hardcoded rate. Radial
//! `time_offset_ms` is ANTENNA time, not arrival time: a chunk hands over about
//! 240 radials at once but their timestamps are evenly spaced (measured p50 gap
//! 20-36 ms, max 38 ms, zero gaps over 200 ms on all 19 cuts of the volume
//! above). Antenna rate therefore has to be measured per cut, because it is not
//! constant across a VCP: 16.5 deg/s on 720-radial Doppler legs, 22.3 deg/s on
//! 720-radial surveillance legs, and 27.9-30.2 deg/s on the 360-radial upper
//! cuts of that same volume. The caller measures it and passes the result in.
//!
//! # Geometry
//!
//! Every pixel-to-gate decision here is made by the crate's own viewport
//! rasteriser types (`ViewportLookupTable`, `AzimuthLookup`, `color_for_raw`),
//! not by a re-derivation, so this raster registers pixel-for-pixel with the
//! normal path and cannot drift away from it. The only thing computed locally
//! is the pixel's compass azimuth for the reveal test, and that calls the same
//! `azimuth_from_xy` on the same `dx`/`dy` the lookup uses.
//!
//! # Velocity
//!
//! Velocity is not painted straight off the grid the way reflectivity is. Two
//! transforms sit between the stored code and the colour, and each of them is a
//! property of a WHOLE sweep rather than of a gate, so each of them has to be
//! decided for the two layers separately and then reconciled:
//!
//! * Dealiasing unfolds a radial against its Nyquist interval by chaining gate
//!   to gate from a seed, so it cannot run on a grid that does not exist yet.
//!   The arriving sweep is unfolded as the partial sweep it is and the previous
//!   sweep as the complete sweep it was, independently. See
//!   [`DealiasedSweepBlend`], which exists so that no caller can unfold one half
//!   of a picture and not the other.
//! * Storm-relative velocity subtracts the projection of a storm motion vector
//!   onto each radial's beam. That subtraction is applied to BOTH layers from
//!   the SAME vector, because a reveal boundary with one frame of reference on
//!   each side of it is a fabricated velocity discontinuity in the shape of a
//!   straight radial line. See [`render_storm_relative_sweep_blend_rgba_into`].
//!
//! Both are shading decisions and neither is a geometry decision: ownership of a
//! pixel is still decided by angle alone, both layers still go through the same
//! `ViewportLookupTable` as the ordinary rasteriser, and a fully revealed blend
//! of either kind is still byte-identical to the ordinary raster of the same
//! sweep.

use std::ops::Range;

use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType};
use rayon::prelude::*;

use crate::{
    AzimuthLookup, ColorTable, ColorTableSet, GateFilterMask, RenderError, Result, StormMotion,
    StormMotionBasis, ViewportLookupRow, ViewportLookupTable, ViewportRasterOptions,
    azimuth_from_xy, build_storm_relative_u8_row_palettes, build_u8_palette, build_u16_palette,
    clockwise_delta_deg, color_family_for_moment, dealias_velocity_grid, ensure_rgba_buffer,
    viewport_dimensions, viewport_geometry,
};

/// Two sweeps of one tilt: the one arriving, and the last complete one.
///
/// Both layers arrive here as ALREADY-RESOLVED grids. Nothing in this module
/// reaches into `cut.moments` to find the moment being drawn, so a caller is
/// free to hand over a grid that no cut holds - a dealiased velocity grid, for
/// instance - as long as it hands over the SAME kind of grid for both layers.
/// The cuts come along only for the radial azimuths their grid rows point at.
///
/// A previous layer whose grid holds a different MOMENT from the incoming one is
/// dropped rather than drawn, because two moments on one display means one half
/// of it is in units the legend does not name - whether or not the two happen to
/// share a colour table.
pub struct SweepBlend<'a> {
    pub incoming: &'a ElevationCut,
    pub incoming_grid: &'a MomentGrid,
    /// The previous complete sweep of the SAME nominal tilt. `None` means
    /// there is nothing to under-paint and the unswept wedge stays empty,
    /// which is correct for the very first sweep after a site change.
    pub previous: Option<(&'a ElevationCut, &'a MomentGrid)>,
    /// Azimuth the incoming sweep started at, degrees.
    pub start_deg: f32,
    /// How far round the reveal has advanced from `start_deg`, degrees, 0..=360.
    pub revealed_deg: f32,
}

/// The gates a [`crate::gate_filter::GateFilter`] removed from each layer of a
/// blend.
///
/// Each mask is indexed against the grid that layer carries, which for a
/// dealiased blend is the UNFOLDED grid the caller handed over and not the one
/// in the cut. Both layers are censored, and by the same rule: the under-paint
/// came from an earlier volume, so filtering only the arriving wedge would
/// leave the analyst looking at one picture with two different rules in it,
/// split along a line that moves with the antenna.
///
/// A censor here does what it does everywhere else in this crate - it stops the
/// candidate walk rather than blanking a value - so a removed gate leaves the
/// pixel empty instead of being replaced by the beam next to it. See
/// `AzimuthLookup::censors`.
#[derive(Clone, Copy, Default)]
pub struct SweepBlendCensor<'a> {
    pub incoming: Option<&'a GateFilterMask>,
    pub previous: Option<&'a GateFilterMask>,
}

/// Rasterise the blend into `rgba`, which must be exactly
/// `viewport_rgba_buffer_len(options)` bytes long. Returns the raster size.
///
/// The buffer is fully cleared first, so a caller may reuse one buffer across
/// frames without stale pixels surviving in the corners that lie beyond radar
/// range.
///
/// An incoming sweep with no rows at all is not an error: it renders as the
/// previous sweep everywhere outside the revealed arc, which is exactly what a
/// viewer should see in the moment before the first chunk of a tilt lands.
pub fn render_sweep_blend_rgba_into(
    blend: &SweepBlend<'_>,
    options: ViewportRasterOptions,
    color_tables: &ColorTableSet,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    render_blend(
        blend,
        SweepBlendCensor::default(),
        Shading::Plain,
        options,
        color_tables,
        rgba,
    )
}

/// The same blend with a pane's gate filter applied to both layers.
///
/// [`SweepBlendCensor::default`] is byte-for-byte
/// [`render_sweep_blend_rgba_into`].
pub fn render_sweep_blend_rgba_into_censored(
    blend: &SweepBlend<'_>,
    censor: SweepBlendCensor<'_>,
    options: ViewportRasterOptions,
    color_tables: &ColorTableSet,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    render_blend(blend, censor, Shading::Plain, options, color_tables, rgba)
}

/// Rasterise the same blend with a storm motion taken out of both layers, for
/// the storm-relative velocity products.
///
/// The subtraction is the one `render_storm_relative_velocity_rgba_into` makes:
/// the component of `storm_motion` along each radial's own beam,
/// `speed * cos(direction - azimuth)`, removed from that radial's velocities
/// before the colour lookup. This calls the crate's own `StormMotionBasis` to
/// get it, so a storm-relative blend and a storm-relative raster of the same
/// sweep agree to the last bit rather than to a tolerance.
///
/// # Why it has to come off BOTH layers
///
/// The two layers meet along a radial line - the reveal boundary. If the motion
/// came off only the arriving sweep, the pixels either side of that line would
/// be measured against different reference frames, and would differ by the
/// projected storm speed even where the radar saw exactly the same wind. Along a
/// beam pointed at the storm that is up to twice the storm speed, arranged as
/// inbound on one side of a straight line and outbound on the other: the
/// signature of a mesocyclone, drawn by the renderer, moving with the antenna.
/// So both layers get the same vector.
///
/// The honest part of that: the previous sweep is a whole volume old, and the
/// storm motion that was true when it was collected is not exactly the one
/// passed in now. What both layers share is the CONVENTION, which is what keeps
/// the two halves comparable. The alternative is not a more truthful picture,
/// it is a false couplet at an angle set by the antenna.
///
/// Refuses anything but velocity. Subtracting metres per second from
/// reflectivity produces a number with no meaning and a colour with every
/// appearance of one.
pub fn render_storm_relative_sweep_blend_rgba_into(
    blend: &SweepBlend<'_>,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
    color_tables: &ColorTableSet,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    render_storm_relative_sweep_blend_rgba_into_censored(
        blend,
        SweepBlendCensor::default(),
        storm_motion,
        options,
        color_tables,
        rgba,
    )
}

/// The same storm-relative blend with a pane's gate filter applied to both
/// layers.
pub fn render_storm_relative_sweep_blend_rgba_into_censored(
    blend: &SweepBlend<'_>,
    censor: SweepBlendCensor<'_>,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
    color_tables: &ColorTableSet,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    // Checking the incoming grid covers the previous one as well: `render_blend`
    // drops any previous layer whose moment is not the incoming one, so past
    // this line either both layers are velocity or there is no second layer.
    if blend.incoming_grid.moment != MomentType::Velocity {
        return Err(RenderError::CacheMomentMismatch {
            expected: MomentType::Velocity,
            actual: blend.incoming_grid.moment.clone(),
        });
    }
    render_blend(
        blend,
        censor,
        Shading::StormRelative(storm_motion),
        options,
        color_tables,
        rgba,
    )
}

fn render_blend(
    blend: &SweepBlend<'_>,
    censor: SweepBlendCensor<'_>,
    shading: Shading,
    options: ViewportRasterOptions,
    color_tables: &ColorTableSet,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    let (width, height) = viewport_dimensions(options);
    ensure_rgba_buffer(rgba, width, height)?;

    // Each layer builds its palette from its own grid, which is right while the
    // two moments agree and a lie the moment they do not: the same reveal would
    // then be showing dBZ on one side of the boundary and metres per second on
    // the other, under one legend, with no seam to give it away. A dropped
    // under-paint is an honest empty wedge; this is not.
    //
    // The test is the MOMENT and not the colour table it maps to. A shared table
    // is not shared units: `color_family_for_moment` sends everything outside
    // reflectivity, velocity and spectrum width to one Generic table, so a
    // family test would happily paint a correlation coefficient of 0.8 into the
    // unswept wedge of a differential reflectivity display, where the same
    // colour means 0.8 dB.
    let plan = BlendPlan {
        incoming: SweepLayer::new(
            blend.incoming,
            blend.incoming_grid,
            censor.incoming,
            shading,
            options,
            color_tables,
        ),
        previous: blend
            .previous
            .filter(|(_, grid)| grid.moment == blend.incoming_grid.moment)
            .map(|(cut, grid)| {
                SweepLayer::new(cut, grid, censor.previous, shading, options, color_tables)
            }),
        options,
        start_deg: blend.start_deg,
        revealed_deg: blend.revealed_deg,
    };

    let row_stride = width as usize * 4;
    rgba.par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| plan.paint_row(y as u32, row_pixels));

    Ok((width, height))
}

/// Both halves of a blend, unfolded, for the dealiased velocity products.
///
/// # The two unfoldings are independent, and that is the correct answer
///
/// Dealiasing is a whole-sweep operation. `dealias_velocity_grid` picks a seed
/// gate per radial, chains outward from it, then reconciles neighbouring radials
/// against each other, so it can only ever be run on the sweep that exists at
/// the time it runs. Here that means the incoming grid is an unfolding of a
/// PARTIAL sweep and the previous grid an unfolding of a COMPLETE one, and
/// neither run has seen the other's answer.
///
/// So the two halves of the picture can disagree, and the disagreement has an
/// exact shape: both runs start from the same folded observations, and every
/// correction either run applies is a whole number of Nyquist intervals, so any
/// gate the two disagree about differs by exactly `k * 2 * nyquist`. Nothing
/// else can reach the reveal boundary from this.
///
/// It is also bounded in WHERE it can appear. `dealias_velocity_grid` couples
/// radials to each other only through a one-radial azimuthal consensus and a
/// three-radial spike test, so the only radials that can unfold differently are
/// the last few of the partial - the ones whose forward neighbour has not
/// arrived. Those sit exactly at the reveal boundary, which is why this is worth
/// measuring rather than assuming.
///
/// Measured, on the most-folded cut of four real cached volumes, unfolding the
/// first 1/3, 1/2, 2/3 and 5/6 of the sweep on its own against unfolding the
/// whole of it: 0 gates out of 1 444 233 differed anywhere.
///
/// | volume | cut | Nyquist | gates the unfolding moves | shared gates | differ |
/// | --- | --- | --- | --- | --- | --- |
/// | KUEX 2026-08-18 06:42:35 | 12, 4.98 deg | 26.3 m/s | 582 | 146 900 | 0 |
/// | KDMX 2026-08-18 08:34:01 | 10, 2.92 deg | 25.5 m/s | 4 357 | 343 500 | 0 |
/// | KEAX 2026-08-18 07:14:59 | 5, 1.22 deg | 24.1 m/s | 507 | 819 739 | 0 |
/// | KABR 2026-08-18 06:43:14 | 10, 2.91 deg | 20.9 m/s | 1 767 | 134 094 | 0 |
///
/// That table is what `two_real_independent_unfoldings_differ_only_by_whole_nyquist_intervals`
/// prints, one line per volume, run with `RADAR_WORKSTATION_L2_CACHE` pointed at
/// a directory holding that volume alone.
///
/// Unfolding one layer only would be a different order of problem: the folded
/// layer would wrap at its own Nyquist while the unfolded one ran past it, so
/// the boundary would jump at every gate where the true velocity exceeded the
/// interval - the 582 to 4 357 gates per cut above, sitting in exactly the
/// couplets a forecaster is looking at, rather than nowhere.
///
/// # Cost
///
/// This unfolds both sweeps, so it does about twice the unfolding work per frame
/// that the ordinary dealiased raster does. Measured in release, unfolding both
/// layers of a real cut took 1.4 ms (360-radial KUEX 4.98 deg) to 5.0 ms
/// (720-radial KEAX 1.22 deg), against 1.8-3.4 ms for the 700x700 raster itself;
/// the first such measurement taken in a process also carries the thread pool's
/// warm-up and read 8.1 ms on a 360-radial cut. So the doubling costs a few
/// milliseconds a frame, not a frame.
///
/// If that ever matters, the previous sweep does not change while a tilt
/// arrives: a caller may keep the grid from [`dealias_cut_velocity`] across the
/// frames of one tilt and assemble a [`SweepBlend`] itself, which is why the
/// entry points take resolved grids. What it must not do is unfold one layer and
/// not the other.
pub struct DealiasedSweepBlend<'a> {
    incoming: &'a ElevationCut,
    incoming_grid: MomentGrid,
    previous: Option<(&'a ElevationCut, MomentGrid)>,
    start_deg: f32,
    revealed_deg: f32,
}

impl<'a> DealiasedSweepBlend<'a> {
    /// Unfold both cuts' velocity.
    ///
    /// `None` when the ARRIVING cut carries no velocity moment at all, which is
    /// the one case where there is no picture to draw. A previous cut that
    /// carries none costs only the under-paint: the blend then behaves exactly
    /// as it does for the first sweep after a site change, painting the arrived
    /// wedge and leaving the rest transparent.
    ///
    /// A velocity moment that is present but still EMPTY is not that case. It is
    /// the state of every tilt between the moment its header lands and the
    /// moment its first chunk does, and [`render_sweep_blend_rgba_into`] draws
    /// it as the previous sweep everywhere outside the revealed arc. Returning
    /// `None` here would take the under-paint away from the dealiased products
    /// at exactly the instant the display has nothing else to show.
    pub fn new(
        incoming: &'a ElevationCut,
        previous: Option<&'a ElevationCut>,
        start_deg: f32,
        revealed_deg: f32,
    ) -> Option<Self> {
        let incoming_grid = dealias_cut_velocity(incoming)?;
        let previous = previous.and_then(|cut| dealias_cut_velocity(cut).map(|grid| (cut, grid)));
        Some(Self {
            incoming,
            incoming_grid,
            previous,
            start_deg,
            revealed_deg,
        })
    }

    /// Take a pair of grids that have already been unfolded, and possibly
    /// altered afterwards.
    ///
    /// [`DealiasedSweepBlend::new`] unfolds and keeps the result, which is what
    /// a caller wants when nothing else happens to the velocity. A caller that
    /// must censor gates after unfolding - a pane with an active
    /// [`crate::gate_filter::GateFilter`] - cannot use it, because the grids it
    /// needs to hand over do not exist until after the unfold. This is the same
    /// structure, built from grids the caller owns.
    pub fn from_unfolded_grids(
        incoming: &'a ElevationCut,
        incoming_grid: MomentGrid,
        previous: Option<(&'a ElevationCut, MomentGrid)>,
        start_deg: f32,
        revealed_deg: f32,
    ) -> Self {
        Self {
            incoming,
            incoming_grid,
            previous,
            start_deg,
            revealed_deg,
        }
    }

    /// Borrow the unfolded pair as an ordinary blend, for either entry point.
    pub fn blend(&self) -> SweepBlend<'_> {
        SweepBlend {
            incoming: self.incoming,
            incoming_grid: &self.incoming_grid,
            previous: self.previous.as_ref().map(|(cut, grid)| (*cut, grid)),
            start_deg: self.start_deg,
            revealed_deg: self.revealed_deg,
        }
    }
}

/// Unfold one cut's velocity, or `None` if the cut carries no velocity moment.
///
/// A velocity moment with no rows in it yet unfolds to an empty grid rather than
/// to `None`: "the sweep has not started" and "this product does not exist here"
/// are different answers, and a blend can draw the first one - the previous
/// sweep, everywhere - while there is nothing at all it can do with the second.
///
/// Exposed so a caller can unfold the previous sweep once and keep the grid
/// across the frames of one tilt, instead of paying for it every frame.
pub fn dealias_cut_velocity(cut: &ElevationCut) -> Option<MomentGrid> {
    let source = cut.moments.get(&MomentType::Velocity)?;
    Some(dealias_velocity_grid(cut, source))
}

/// True when the antenna has already swept past `azimuth_deg` this pass.
///
/// The arc is half open: `[start_deg, start_deg + revealed_deg)`. The azimuth
/// the reveal has just reached is therefore NOT yet revealed, which is the
/// conservative choice - the leading radial is the one still being written.
pub fn azimuth_is_revealed(start_deg: f32, revealed_deg: f32, azimuth_deg: f32) -> bool {
    let revealed = revealed_arc_deg(revealed_deg);
    // A full revolution is checked before the offset comparison because the
    // offset itself can round to exactly 360.0 (see `clockwise_offset_deg`),
    // and `360.0 < 360.0` is false. Without this line a completed sweep would
    // keep one hairline of previous data at the seam.
    if revealed >= 360.0 {
        return true;
    }
    clockwise_offset_deg(start_deg, azimuth_deg) < revealed
}

/// Degrees clockwise from `start_deg` round to `azimuth_deg`, in 0..360.
///
/// Two answers are folded to zero.
///
/// A NON-FINITE offset means `start_deg` was itself NaN or infinite: the caller
/// could not place the arc at all, which is what reading a start azimuth off a
/// cut with no radials gives you. Left as NaN, every comparison in
/// `azimuth_is_revealed` would be false and the WHOLE display would be painted
/// from the previous sweep, presenting a stale frame as live radar. That is the
/// same worst-case failure `revealed_arc_deg` guards `revealed_deg` against, and
/// guarding only one of the two inputs leaves the failure reachable through the
/// other. Folding to zero puts every pixel inside any non-empty arc, so a broken
/// start degrades to the incoming sweep alone with an honest empty wedge, while
/// a caller that has genuinely received nothing yet still gets the previous
/// sweep, because a `revealed_deg` of zero makes `0.0 < 0.0` false.
///
/// An offset of exactly 360.0 is the f32 rounding of an azimuth a hair
/// counter-clockwise of `start_deg`: f32 steps by about 3.05e-5 near 360, so
/// `rem_euclid` lands the true answer on the endpoint. Folding it to zero keeps
/// the returned range half open at 360, so the value `azimuth_is_revealed`
/// compares against `revealed` is never its own excluded upper bound.
fn clockwise_offset_deg(start_deg: f32, azimuth_deg: f32) -> f32 {
    let offset = clockwise_delta_deg(start_deg, azimuth_deg);
    if !offset.is_finite() || offset >= 360.0 {
        0.0
    } else {
        offset
    }
}

/// Clamp the reveal to a sane arc.
///
/// A NON-FINITE reveal means the caller's rate estimate broke. Both shapes it
/// comes in are reachable from the same arithmetic: a measured antenna rate is
/// degrees divided by a time span, so a span that rounds to zero gives an
/// infinite rate, and multiplying that by a zero elapsed time gives NaN. Neither
/// is a measurement, and taken at face value BOTH land on the worst failure
/// available here. NaN makes every `<` comparison false; negative infinity
/// clamps to zero and makes them all false too; either way the WHOLE display is
/// painted from the previous sweep, presenting a stale frame as live radar. So
/// any non-finite reveal is turned into a full one, which degrades to the
/// un-blended picture: the incoming sweep alone, with an honest empty wedge.
///
/// A FINITE negative reveal is a different case and does clamp to zero, because
/// "nothing of this tilt has arrived yet" is a real state in which the previous
/// sweep is exactly what should be on screen, and a caller whose arc comes out a
/// little below zero across a clock correction means that state rather than a
/// broken estimate.
fn revealed_arc_deg(revealed_deg: f32) -> f32 {
    if revealed_deg.is_finite() {
        revealed_deg.clamp(0.0, 360.0)
    } else {
        360.0
    }
}

/// Compass azimuth of a pixel centre, in this crate's screen convention: pixel
/// centres sit at (x + 0.5, y + 0.5) and +y is SOUTH.
///
/// The kilometres-per-pixel clamp matches `viewport_geometry` exactly. Without
/// it a zero scale would divide the reveal test and the gate lookup by
/// different numbers, and the two would then disagree about where a pixel is.
fn pixel_azimuth_deg(options: ViewportRasterOptions, x: u32, y: u32) -> f32 {
    let km_per_px_x = options.km_per_px_x.max(f32::EPSILON);
    let km_per_px_y = options.km_per_px_y.max(f32::EPSILON);
    let dx_km = (x as f32 + 0.5 - options.radar_x_px) * km_per_px_x;
    let dy_km = (options.radar_y_px - (y as f32 + 0.5)) * km_per_px_y;
    // The camera rotation comes off, exactly as it does in the gate lookup:
    // the reveal test compares this against TRUE sweep azimuths reported by
    // the radar, and a rotated pane would otherwise reveal the wrong wedge.
    let rotation_deg = if options.rotation_rad.is_finite() {
        options.rotation_rad.to_degrees()
    } else {
        0.0
    };
    azimuth_from_xy(dx_km, dy_km) - rotation_deg
}

/// What a layer does to a stored value on the way to a colour.
///
/// This is the ONLY thing the velocity products change. Both variants share the
/// azimuth lookup, the gate lookup and the ownership test, so a storm-relative
/// blend cannot drift geometrically away from a plain one, and the shading is
/// applied to every layer of one blend or to none of them.
#[derive(Clone, Copy)]
enum Shading {
    /// Paint the value the grid holds.
    Plain,
    /// Paint the value the grid holds minus this storm motion's projection on
    /// the beam of the radial that value came from.
    StormRelative(StormMotion),
}

/// One sweep, prepared for rasterising: its azimuth-to-row index, its
/// pixel-to-gate table, and its palette.
struct SweepLayer<'a> {
    grid: &'a MomentGrid,
    azimuths: AzimuthLookup,
    lookup: ViewportLookupTable,
    colors: LayerColors,
}

impl<'a> SweepLayer<'a> {
    fn new(
        cut: &ElevationCut,
        grid: &'a MomentGrid,
        censor: Option<&GateFilterMask>,
        shading: Shading,
        options: ViewportRasterOptions,
        color_tables: &ColorTableSet,
    ) -> Self {
        Self {
            grid,
            azimuths: AzimuthLookup::new(cut, grid).with_censor(censor.cloned()),
            lookup: ViewportLookupTable::new(grid, viewport_geometry(grid, options)),
            colors: LayerColors::new(cut, grid, shading, color_tables),
        }
    }
}

struct BlendPlan<'a> {
    incoming: SweepLayer<'a>,
    previous: Option<SweepLayer<'a>>,
    options: ViewportRasterOptions,
    start_deg: f32,
    revealed_deg: f32,
}

impl BlendPlan<'_> {
    fn paint_row(&self, y: u32, row_pixels: &mut [u8]) {
        row_pixels.fill(0);

        let incoming_row = self.incoming.lookup.row(y);
        let previous_row = self.previous.as_ref().and_then(|layer| layer.lookup.row(y));
        let Some(x_range) = union_x_range(incoming_row.as_ref(), previous_row.as_ref()) else {
            return;
        };

        for x in x_range {
            let azimuth_deg = pixel_azimuth_deg(self.options, x, y);
            let color = if azimuth_is_revealed(self.start_deg, self.revealed_deg, azimuth_deg) {
                incoming_row
                    .as_ref()
                    .and_then(|row| sample_layer_color(&self.incoming, row, x))
            } else {
                match (self.previous.as_ref(), previous_row.as_ref()) {
                    (Some(layer), Some(row)) => sample_layer_color(layer, row, x),
                    _ => None,
                }
            };
            let Some(color) = color else {
                continue;
            };
            let pixel = x as usize * 4;
            row_pixels[pixel..pixel + 4].copy_from_slice(&color);
        }
    }
}

/// The pixels this scanline could touch from either sweep.
///
/// The two sweeps can have different gate counts and so different on-screen
/// radii. Scanning only the incoming sweep's span would clip the previous
/// sweep's outer ring away; each row's own lookup still rejects pixels beyond
/// its own range, so widening the scan cannot paint anything out of range.
fn union_x_range(
    incoming: Option<&ViewportLookupRow>,
    previous: Option<&ViewportLookupRow>,
) -> Option<Range<u32>> {
    match (incoming, previous) {
        (Some(incoming), Some(previous)) => Some(
            incoming.x_range.start.min(previous.x_range.start)
                ..incoming.x_range.end.max(previous.x_range.end),
        ),
        (Some(only), None) | (None, Some(only)) => Some(only.x_range.clone()),
        (None, None) => None,
    }
}

/// Colour one pixel from one sweep, or `None` if that sweep has nothing to say
/// there.
///
/// Candidate rows are tried in the azimuth lookup's own ranked order and the
/// first one with a non-transparent colour wins, which is what the normal
/// viewport rasteriser does. Anything else here would make a blended frame
/// differ from an unblended one inside the arc where the two must be identical.
fn sample_layer_color(layer: &SweepLayer<'_>, row: &ViewportLookupRow, x: u32) -> Option<[u8; 4]> {
    let sample = row.lookup(x, &layer.azimuths)?;
    for candidate in layer.azimuths.candidates_for_bin(sample.azimuth_bin) {
        // A gate this layer's filter removed ends the walk. Falling through
        // would paint the beam beside it, which is how a censored blend would
        // come to show a value from an azimuth the analyst never asked about.
        if layer.azimuths.censors(candidate.row, sample.gate) {
            return None;
        }
        if let Some(color) = layer
            .colors
            .color_at(layer.grid, candidate.row, sample.gate)
        {
            return Some(color);
        }
    }
    None
}

/// Per-sweep colour lookup, built once instead of per pixel.
///
/// The plain palettes come from `color_for_raw`, so a raw code equal to the
/// grid's `range_folded` gets the table's range-folded colour and a code equal
/// to `nodata` gets a transparent entry, which this module then skips.
///
/// The storm-relative arms subtract a per-ROW quantity, so there is no single
/// palette that covers the sweep: the u8 arm holds one palette per radial, built
/// by the crate's own `build_storm_relative_u8_row_palettes`, and the wider
/// storages carry the motion vector and resolve per gate the way
/// `render_storm_relative_viewport_storage` does. Both mirror the ordinary
/// storm-relative raster arm for arm, which is what makes a fully revealed blend
/// byte-identical to it.
enum LayerColors {
    U8(Box<[[u8; 4]; 256]>),
    U16(Vec<[u8; 4]>),
    F32(ColorTable),
    StormRelativeU8(Vec<[[u8; 4]; 256]>),
    StormRelativeValue {
        row_motion: Vec<f32>,
        color_table: ColorTable,
    },
}

impl LayerColors {
    fn new(
        cut: &ElevationCut,
        grid: &MomentGrid,
        shading: Shading,
        color_tables: &ColorTableSet,
    ) -> Self {
        let color_table = color_tables
            .for_family(color_family_for_moment(&grid.moment))
            .clone();
        match shading {
            Shading::Plain => match &grid.storage {
                MomentStorage::U8(_) => Self::U8(Box::new(build_u8_palette(grid, &color_table))),
                MomentStorage::U16(_) => Self::U16(build_u16_palette(grid, &color_table)),
                MomentStorage::F32(_) => Self::F32(color_table),
            },
            Shading::StormRelative(storm_motion) => {
                // `StormMotionBasis` and not a local cosine: the ordinary
                // storm-relative raster projects the motion through this exact
                // decomposition for every velocity cut, and two spellings of one
                // cosine agree to within an ulp, not to the bit. An ulp is
                // enough to land two gates either side of a colour stop.
                let row_motion =
                    StormMotionBasis::new(cut, grid).row_motion_components(storm_motion);
                match &grid.storage {
                    MomentStorage::U8(_) => Self::StormRelativeU8(
                        build_storm_relative_u8_row_palettes(grid, &row_motion, &color_table),
                    ),
                    MomentStorage::U16(_) | MomentStorage::F32(_) => Self::StormRelativeValue {
                        row_motion,
                        color_table,
                    },
                }
            }
        }
    }

    fn color_at(&self, grid: &MomentGrid, row: usize, gate: usize) -> Option<[u8; 4]> {
        let index = row
            .checked_mul(grid.gate_range.gate_count)?
            .checked_add(gate)?;
        let color = match (&grid.storage, self) {
            (MomentStorage::U8(values), Self::U8(palette)) => {
                palette[usize::from(*values.get(index)?)]
            }
            (MomentStorage::U16(values), Self::U16(palette)) => {
                *palette.get(usize::from(*values.get(index)?))?
            }
            (MomentStorage::F32(values), Self::F32(color_table)) => {
                let value = values
                    .get(index)
                    .copied()
                    .filter(|value| value.is_finite())?;
                color_table.color_for_value(value)
            }
            (MomentStorage::U8(values), Self::StormRelativeU8(row_palettes)) => {
                row_palettes.get(row)?[usize::from(*values.get(index)?)]
            }
            (
                MomentStorage::U16(values),
                Self::StormRelativeValue {
                    row_motion,
                    color_table,
                },
            ) => {
                let raw = *values.get(index)?;
                if grid.nodata == Some(raw) {
                    return None;
                }
                if grid.range_folded == Some(raw) {
                    color_table.range_folded_color()
                } else {
                    let velocity = (raw as f32 - grid.offset) / grid.scale;
                    color_table.color_for_value(velocity - row_motion_at(row_motion, row))
                }
            }
            (
                MomentStorage::F32(values),
                Self::StormRelativeValue {
                    row_motion,
                    color_table,
                },
            ) => {
                let velocity = values
                    .get(index)
                    .copied()
                    .filter(|value| value.is_finite())?;
                color_table.color_for_value(velocity - row_motion_at(row_motion, row))
            }
            _ => return None,
        };
        (color[3] != 0).then_some(color)
    }
}

/// The storm motion projected on one row's beam.
///
/// A row past the end of the vector reads as zero rather than as no data, which
/// is what the ordinary storm-relative raster does: a row the basis could not
/// place still holds real echo, and ground-relative echo is a smaller error than
/// a hole in the display.
fn row_motion_at(row_motion: &[f32], row: usize) -> f32 {
    row_motion.get(row).copied().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use super::*;
    use crate::beam::compass_azimuth_deg;
    use crate::{
        ColorTableFamily, RenderError, ViewportMomentCache, color_for_raw,
        render_moment_viewport_rgba_into, row_nyquist_mps, viewport_rgba_buffer_len,
    };
    use radar_core::{GateRange, MomentRow, MomentType, RadarSite, RadarVolume, Radial};

    const FIRST_GATE_M: i32 = 2_000;
    const GATE_SPACING_M: i32 = 1_000;
    const GATE_COUNT: usize = 60;
    const SCALE: f32 = 2.0;
    const OFFSET: f32 = 66.0;
    /// Raw 86 is exactly 10 dBZ under `SCALE`/`OFFSET`.
    const INCOMING_RAW: u8 = 86;
    /// Raw 156 is exactly 45 dBZ.
    const PREVIOUS_RAW: u8 = 156;
    /// Raw 200 is exactly 67 dBZ.
    const MARKER_RAW: u8 = 200;
    const WIDTH: u32 = 201;
    const HEIGHT: u32 = 201;

    /// A 201x201 viewport with the radar exactly on the centre pixel's centre
    /// and 0.5 km per pixel, so 18 pixels north of the radar is exactly 9.0 km
    /// and the pixel arithmetic in these tests is exact rather than nearly so.
    fn options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: WIDTH,
            height: HEIGHT,
            radar_x_px: 100.5,
            radar_y_px: 100.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        }
    }

    fn sweep(start_deg: f32, radial_count: usize, raw: u8) -> (ElevationCut, MomentGrid) {
        sweep_with_marker_gate(start_deg, radial_count, raw, None)
    }

    /// A sweep of `radial_count` radials one degree apart, clockwise from
    /// `start_deg`, every gate holding `raw` except an optional marker gate.
    fn sweep_with_marker_gate(
        start_deg: f32,
        radial_count: usize,
        raw: u8,
        marker: Option<(usize, u8)>,
    ) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            SCALE,
            OFFSET,
            Some(0),
            Some(1),
        );
        let mut row = vec![raw; GATE_COUNT];
        if let Some((gate, marker_raw)) = marker {
            row[gate] = marker_raw;
        }
        for index in 0..radial_count {
            cut.radials.push(Radial {
                azimuth_deg: (start_deg + index as f32).rem_euclid(360.0),
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            grid.push_u8_row_slice(index, &row)
                .expect("row fits the grid");
        }
        (cut, grid)
    }

    fn reflectivity_color(grid: &MomentGrid, raw: u8) -> [u8; 4] {
        let tables = ColorTableSet::default();
        color_for_raw(
            grid,
            tables.for_family(ColorTableFamily::Reflectivity),
            u16::from(raw),
        )
    }

    /// Render into a buffer prefilled with a non-zero byte, so a test that
    /// expects transparency is really seeing the renderer clear the buffer.
    fn render(blend: &SweepBlend<'_>) -> Vec<u8> {
        let mut rgba = vec![7; viewport_rgba_buffer_len(options())];
        let dimensions =
            render_sweep_blend_rgba_into(blend, options(), &ColorTableSet::default(), &mut rgba)
                .expect("blend renders");
        assert_eq!(dimensions, (WIDTH, HEIGHT));
        rgba
    }

    fn pixel_for(azimuth_deg: f32, range_km: f32) -> (u32, u32) {
        let options = options();
        let east_km = range_km * azimuth_deg.to_radians().sin();
        let north_km = range_km * azimuth_deg.to_radians().cos();
        let x = (options.radar_x_px + east_km / options.km_per_px_x - 0.5).round();
        let y = (options.radar_y_px - north_km / options.km_per_px_y - 0.5).round();
        (x as u32, y as u32)
    }

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let index = (y as usize * WIDTH as usize + x as usize) * 4;
        [
            rgba[index],
            rgba[index + 1],
            rgba[index + 2],
            rgba[index + 3],
        ]
    }

    fn pixel_at(rgba: &[u8], azimuth_deg: f32, range_km: f32) -> [u8; 4] {
        let (x, y) = pixel_for(azimuth_deg, range_km);
        pixel(rgba, x, y)
    }

    fn count_color(rgba: &[u8], color: [u8; 4]) -> usize {
        rgba.chunks_exact(4).filter(|chunk| *chunk == color).count()
    }

    #[test]
    fn a_pixel_inside_the_revealed_arc_shows_the_incoming_sweep_and_one_outside_shows_the_previous()
    {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        assert_eq!(incoming_grid.scaled_value(0, 0), Some(10.0));
        assert_eq!(previous_grid.scaled_value(0, 0), Some(45.0));
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);
        assert_ne!(
            incoming_color, previous_color,
            "10 dBZ and 45 dBZ must be different colours or this test proves nothing"
        );

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });

        assert_eq!(pixel_at(&rgba, 90.0, 20.0), incoming_color, "due east");
        assert_eq!(pixel_at(&rgba, 270.0, 20.0), previous_color, "due west");
    }

    #[test]
    fn the_pixel_at_exactly_revealed_deg_still_belongs_to_the_previous_sweep() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let (x, y) = pixel_for(90.0, 20.0);
        assert_eq!((x, y), (140, 100));
        let boundary_deg = pixel_azimuth_deg(options(), x, y);
        assert!(
            (boundary_deg - 90.0).abs() < 1e-4,
            "pixel (140, 100) sits due east of the radar, got {boundary_deg}"
        );

        assert!(
            !azimuth_is_revealed(0.0, boundary_deg, boundary_deg),
            "the arc is half open, so its own end azimuth is not yet swept"
        );
        assert!(azimuth_is_revealed(0.0, boundary_deg + 0.05, boundary_deg));

        let at_boundary = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: boundary_deg,
        });
        assert_eq!(pixel(&at_boundary, x, y), previous_color);

        let just_past = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: boundary_deg + 0.05,
        });
        assert_eq!(pixel(&just_past, x, y), incoming_color);
    }

    #[test]
    fn a_reveal_of_three_hundred_and_sixty_degrees_paints_every_pixel_from_the_incoming_sweep() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 360.0,
        });

        for step in 0..8 {
            let azimuth_deg = step as f32 * 45.0;
            assert_eq!(
                pixel_at(&rgba, azimuth_deg, 20.0),
                incoming_color,
                "azimuth {azimuth_deg}"
            );
        }
        assert_eq!(
            count_color(&rgba, previous_color),
            0,
            "a completed sweep must leave no trace of the previous one, not even at the seam"
        );
        for (index, chunk) in rgba.chunks_exact(4).enumerate() {
            assert!(
                chunk == incoming_color || chunk == [0, 0, 0, 0],
                "pixel {index} is neither incoming nor empty: {chunk:?}"
            );
        }
        assert!(count_color(&rgba, incoming_color) > 30_000);
    }

    #[test]
    fn a_reveal_of_zero_degrees_paints_every_pixel_from_the_previous_sweep() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 0.0,
        });

        for step in 0..8 {
            let azimuth_deg = step as f32 * 45.0;
            assert_eq!(
                pixel_at(&rgba, azimuth_deg, 20.0),
                previous_color,
                "azimuth {azimuth_deg}"
            );
        }
        assert_eq!(
            count_color(&rgba, incoming_color),
            0,
            "nothing of the new tilt has arrived, so none of it may be on screen"
        );
    }

    #[test]
    fn a_revealed_arc_that_straddles_the_zero_seam_reveals_on_both_sides_of_it() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        // The arc runs 300 -> 360/0 -> 60.
        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 300.0,
            revealed_deg: 120.0,
        });

        assert_eq!(pixel_at(&rgba, 330.0, 20.0), incoming_color, "before 360");
        assert_eq!(pixel_at(&rgba, 0.0, 20.0), incoming_color, "on the seam");
        assert_eq!(pixel_at(&rgba, 30.0, 20.0), incoming_color, "after 360");
        assert_eq!(pixel_at(&rgba, 90.0, 20.0), previous_color, "past the end");
        assert_eq!(pixel_at(&rgba, 250.0, 20.0), previous_color, "before start");

        assert!(azimuth_is_revealed(300.0, 120.0, 359.9));
        assert!(azimuth_is_revealed(300.0, 120.0, 0.0));
        assert!(
            !azimuth_is_revealed(300.0, 120.0, 60.0),
            "300 + 120 = 60 is the open end of the arc"
        );
    }

    #[test]
    fn the_unswept_wedge_stays_fully_transparent_when_there_is_no_previous_sweep() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 180.0,
        });

        assert_eq!(pixel_at(&rgba, 90.0, 20.0), incoming_color);
        for azimuth_deg in [181.0, 225.0, 270.0, 315.0, 359.0] {
            assert_eq!(
                pixel_at(&rgba, azimuth_deg, 20.0),
                [0, 0, 0, 0],
                "azimuth {azimuth_deg} has no previous sweep to fall back to"
            );
        }
    }

    #[test]
    fn a_partially_arrived_sweep_underpaints_its_unswept_wedge_with_the_previous_sweep() {
        // The shape of a real KTLX 2026-08-17 07:24:02 VCP 212 partial cut:
        // 240 of 360 radials, running from 197.5 degrees up through 360/0 and
        // ending at 76.5, leaving a 121 degree hole across the seam.
        let (incoming_cut, incoming_grid) = sweep(197.5, 240, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.5, 360, PREVIOUS_RAW);
        assert_eq!(incoming_cut.radials.len(), 240);
        assert_eq!(incoming_cut.radials[0].azimuth_deg, 197.5);
        assert_eq!(incoming_cut.radials[239].azimuth_deg, 76.5);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 197.5,
            revealed_deg: 240.0,
        });

        assert_eq!(pixel_at(&rgba, 250.0, 20.0), incoming_color, "swept");
        assert_eq!(
            pixel_at(&rgba, 10.0, 20.0),
            incoming_color,
            "swept, wrapped"
        );
        assert_eq!(pixel_at(&rgba, 100.0, 20.0), previous_color, "in the hole");
        assert_eq!(pixel_at(&rgba, 190.0, 20.0), previous_color, "in the hole");

        for step in 0..72 {
            let azimuth_deg = step as f32 * 5.0;
            assert_ne!(
                pixel_at(&rgba, azimuth_deg, 20.0)[3],
                0,
                "azimuth {azimuth_deg} went blank, so the storm appeared to vanish there"
            );
        }
    }

    #[test]
    fn the_incoming_azimuth_smear_never_paints_into_the_unswept_wedge() {
        let (incoming_cut, incoming_grid) = sweep(197.5, 240, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.5, 360, PREVIOUS_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        // The azimuth lookup widens the first radial of the incoming sweep by
        // up to 3 degrees, so it does claim 195.5 - two degrees inside the
        // hole. Without this assertion the test below would be vacuous.
        let incoming_azimuths = AzimuthLookup::new(&incoming_cut, &incoming_grid);
        assert!(
            incoming_azimuths.row_for_azimuth(195.5).is_some(),
            "the incoming lookup really does smear back past its first radial"
        );
        assert!(
            incoming_azimuths.row_for_azimuth(150.0).is_none(),
            "deep in the hole the incoming lookup has nothing at all"
        );

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 197.5,
            revealed_deg: 240.0,
        });

        assert_eq!(
            pixel_at(&rgba, 195.5, 20.0),
            previous_color,
            "angle decides ownership, not whether the incoming sweep has data"
        );
    }

    #[test]
    fn a_gate_is_centred_at_first_gate_plus_index_times_spacing() {
        // Gate 7 therefore sits at 2000 + 7 * 1000 = 9000 m, not at 9500 m.
        let (incoming_cut, incoming_grid) =
            sweep_with_marker_gate(0.0, 360, INCOMING_RAW, Some((7, MARKER_RAW)));
        assert_eq!(incoming_grid.scaled_value(0, 7), Some(67.0));
        let marker_color = reflectivity_color(&incoming_grid, MARKER_RAW);
        let plain_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        assert_ne!(marker_color, plain_color);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        });

        // Column 100 is exactly due north of the radar; the radar sits at
        // y = 100.5, so pixel y is (100 - dy_px) with 0.5 km per pixel.
        assert_eq!(pixel(&rgba, 100, 82), marker_color, "9.0 km is gate 7");
        assert_eq!(pixel(&rgba, 100, 84), plain_color, "8.0 km is gate 6");
        assert_eq!(pixel(&rgba, 100, 80), plain_color, "10.0 km is gate 8");
        assert_eq!(
            pixel(&rgba, 100, 81),
            plain_color,
            "9.5 km is the gate 7/8 boundary; the (g + 0.5) idiom would have put gate 7 there"
        );
    }

    #[test]
    fn a_broken_reveal_estimate_shows_the_incoming_sweep_alone_rather_than_a_stale_frame() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        assert!(azimuth_is_revealed(0.0, f32::NAN, 123.0));

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: f32::NAN,
        });

        assert_eq!(pixel_at(&rgba, 90.0, 20.0), incoming_color);
        assert_eq!(count_color(&rgba, previous_color), 0);
    }

    #[test]
    fn a_fully_revealed_blend_is_byte_identical_to_the_normal_viewport_render() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let mut volume = RadarVolume::new(RadarSite::new("TST"), chrono::Utc::now());
        let mut volume_cut = incoming_cut.clone();
        volume_cut
            .moments
            .insert(MomentType::Reflectivity, incoming_grid.clone());
        volume.cuts.push(volume_cut);

        let mut expected = vec![0; viewport_rgba_buffer_len(options())];
        render_moment_viewport_rgba_into(
            &volume,
            0,
            MomentType::Reflectivity,
            options(),
            &mut expected,
        )
        .expect("normal viewport render");

        let blended = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        });

        assert_eq!(
            blended, expected,
            "a complete sweep with nothing under it must be the ordinary raster, pixel for pixel"
        );
    }

    #[test]
    fn a_pixel_azimuth_is_the_compass_bearing_of_its_east_north_offset() {
        let options = options();
        for (x, y) in [(140, 100), (100, 60), (60, 100), (100, 140), (130, 70)] {
            let east_km = (f64::from(x) + 0.5 - f64::from(options.radar_x_px))
                * f64::from(options.km_per_px_x);
            let north_km = (f64::from(options.radar_y_px) - (f64::from(y) + 0.5))
                * f64::from(options.km_per_px_y);
            let expected = compass_azimuth_deg(east_km, north_km);
            let actual = f64::from(pixel_azimuth_deg(options, x, y));
            assert!(
                (actual - expected).abs() < 1e-3,
                "pixel ({x}, {y}): got {actual}, expected {expected}"
            );
        }
    }

    #[test]
    fn a_wrong_sized_buffer_is_rejected_instead_of_painting_a_skewed_frame() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let blend = SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        };
        let mut rgba = vec![0; viewport_rgba_buffer_len(options()) - 4];

        let error =
            render_sweep_blend_rgba_into(&blend, options(), &ColorTableSet::default(), &mut rgba)
                .expect_err("a short buffer must be an error");

        match error {
            RenderError::BufferSizeMismatch {
                actual,
                expected,
                width,
                height,
            } => {
                assert_eq!(actual, 201 * 201 * 4 - 4);
                assert_eq!(expected, 201 * 201 * 4);
                assert_eq!((width, height), (201, 201));
            }
            other => panic!("expected a buffer size mismatch, got {other}"),
        }
    }

    /// A sweep of `radial_count` radials one degree apart with a gate range the
    /// caller chooses, so a test can give the two sweeps different footprints.
    fn sized_sweep(
        start_deg: f32,
        radial_count: usize,
        raw: u8,
        first_gate_m: i32,
        gate_spacing_m: i32,
        gate_count: usize,
    ) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m,
            gate_spacing_m,
            gate_count,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            SCALE,
            OFFSET,
            Some(0),
            Some(1),
        );
        let row = vec![raw; gate_count];
        for index in 0..radial_count {
            cut.radials.push(Radial {
                azimuth_deg: (start_deg + index as f32).rem_euclid(360.0),
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            grid.push_u8_row_slice(index, &row)
                .expect("row fits the grid");
        }
        (cut, grid)
    }

    /// The same sweep shape, stored as u16 words with a spread of raw codes so
    /// the u16 palette arm is exercised on more than one entry.
    fn u16_sweep() -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u16(
            MomentType::Reflectivity,
            gate_range.clone(),
            SCALE,
            OFFSET,
            Some(0),
            Some(1),
        );
        for index in 0..360usize {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            let row = (0..GATE_COUNT)
                .map(|gate| ((index * 7 + gate * 13) % 900) as u16)
                .collect();
            grid.push_row(index, MomentRow::U16(row))
                .expect("row fits the grid");
        }
        (cut, grid)
    }

    /// The same sweep shape, stored as f32 values with NaN holes so the f32 arm
    /// is exercised on both a paintable value and a skipped one.
    fn f32_sweep() -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u16(
            MomentType::Reflectivity,
            gate_range.clone(),
            SCALE,
            OFFSET,
            None,
            None,
        );
        // radar_core has no f32 constructor; swap the storage before any row is
        // pushed so the grid is a genuine f32 grid from the first row on.
        grid.storage = MomentStorage::F32(Vec::new());
        for index in 0..360usize {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            let row = (0..GATE_COUNT)
                .map(|gate| {
                    if (index + gate) % 17 == 0 {
                        f32::NAN
                    } else {
                        (index % 13) as f32 * 4.0 + gate as f32 * 0.25
                    }
                })
                .collect();
            grid.push_row(index, MomentRow::F32(row))
                .expect("row fits the grid");
        }
        (cut, grid)
    }

    /// A sweep whose rows really contain the nodata code 0 and the range-folded
    /// code 1, so those two palette entries are compared and not just built.
    fn coded_sweep() -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            SCALE,
            OFFSET,
            Some(0),
            Some(1),
        );
        for index in 0..360usize {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            let row: Vec<u8> = (0..GATE_COUNT)
                .map(|gate| match (index + gate) % 5 {
                    0 => 0,
                    1 => 1,
                    other => (60 + other * 20 + gate) as u8,
                })
                .collect();
            grid.push_u8_row_slice(index, &row)
                .expect("row fits the grid");
        }
        (cut, grid)
    }

    /// Render one sweep through the crate's ordinary viewport rasteriser, which
    /// is the reference every registration test below compares against.
    fn normal_render_with(
        cut: &ElevationCut,
        grid: &MomentGrid,
        raster_options: ViewportRasterOptions,
    ) -> Vec<u8> {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), chrono::Utc::now());
        let mut volume_cut = cut.clone();
        volume_cut
            .moments
            .insert(MomentType::Reflectivity, grid.clone());
        volume.cuts.push(volume_cut);
        let mut pixels = vec![0; viewport_rgba_buffer_len(raster_options)];
        render_moment_viewport_rgba_into(
            &volume,
            0,
            MomentType::Reflectivity,
            raster_options,
            &mut pixels,
        )
        .expect("normal viewport render");
        pixels
    }

    fn normal_render(cut: &ElevationCut, grid: &MomentGrid) -> Vec<u8> {
        normal_render_with(cut, grid, options())
    }

    /// The first pixel where two rasters differ, as (index, x, y), so a failure
    /// names the pixel instead of dumping 161 604 bytes.
    fn first_difference(left: &[u8], right: &[u8]) -> Option<(usize, u32, u32)> {
        left.chunks_exact(4)
            .zip(right.chunks_exact(4))
            .position(|(left_pixel, right_pixel)| left_pixel != right_pixel)
            .map(|index| (index, index as u32 % WIDTH, index as u32 / WIDTH))
    }

    #[test]
    fn a_start_azimuth_that_is_not_a_number_never_presents_the_previous_sweep_as_live_radar() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        for start_deg in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let rgba = render(&SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: Some((&previous_cut, &previous_grid)),
                start_deg,
                revealed_deg: 180.0,
            });
            assert_eq!(
                count_color(&rgba, previous_color),
                0,
                "start {start_deg} put stale data on screen as if it were this pass"
            );
            assert!(count_color(&rgba, incoming_color) > 30_000);
        }
    }

    #[test]
    fn a_start_azimuth_that_is_not_a_number_still_shows_the_previous_sweep_before_anything_arrives()
    {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        // No radials have arrived, so the caller has no start azimuth to give.
        // A zero reveal still has to mean "none of this tilt is on screen yet".
        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: f32::NAN,
            revealed_deg: 0.0,
        });

        assert_eq!(pixel_at(&rgba, 90.0, 20.0), previous_color);
        assert_eq!(count_color(&rgba, incoming_color), 0);
    }

    #[test]
    fn a_straddling_arc_is_incoming_at_310_350_10_and_50_and_previous_at_100_200_and_290() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 300.0,
            revealed_deg: 120.0,
        });

        // Subtracting without wrapping gets 10 and 50 exactly backwards: 10 -
        // 300 is -290, which is not in 0..120 the way 70 is.
        for azimuth_deg in [310.0f32, 350.0, 10.0, 50.0] {
            assert!(azimuth_is_revealed(300.0, 120.0, azimuth_deg));
            assert_eq!(
                pixel_at(&rgba, azimuth_deg, 20.0),
                incoming_color,
                "azimuth {azimuth_deg} is inside the arc 300 -> 60"
            );
        }
        for azimuth_deg in [100.0f32, 200.0, 290.0] {
            assert!(!azimuth_is_revealed(300.0, 120.0, azimuth_deg));
            assert_eq!(
                pixel_at(&rgba, azimuth_deg, 20.0),
                previous_color,
                "azimuth {azimuth_deg} is in the 240 degree hole"
            );
        }
    }

    #[test]
    fn a_full_reveal_matches_the_normal_raster_even_with_a_previous_sweep_underneath() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);

        let blended = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 360.0,
        });

        assert_eq!(
            first_difference(&blended, &normal_render(&incoming_cut, &incoming_grid)),
            None,
            "a finished sweep must not shift by a pixel just because something is under it"
        );
    }

    #[test]
    fn a_zero_reveal_matches_the_normal_raster_of_the_previous_sweep() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);

        let blended = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 0.0,
        });

        assert_eq!(
            first_difference(&blended, &normal_render(&previous_cut, &previous_grid)),
            None,
            "the under-painted sweep has to land where its own raster would"
        );
    }

    #[test]
    fn a_u16_grid_blends_to_the_same_bytes_as_the_normal_raster() {
        let (cut, grid) = u16_sweep();
        let blended = render(&SweepBlend {
            incoming: &cut,
            incoming_grid: &grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        });
        assert_eq!(
            first_difference(&blended, &normal_render(&cut, &grid)),
            None
        );
        assert!(blended.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn an_f32_grid_blends_to_the_same_bytes_as_the_normal_raster() {
        let (cut, grid) = f32_sweep();
        let blended = render(&SweepBlend {
            incoming: &cut,
            incoming_grid: &grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        });
        assert_eq!(
            first_difference(&blended, &normal_render(&cut, &grid)),
            None
        );
        assert!(blended.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn range_folded_and_nodata_gates_blend_to_the_same_bytes_as_the_normal_raster() {
        let (cut, grid) = coded_sweep();
        let tables = ColorTableSet::default();
        let folded_color = tables
            .for_family(ColorTableFamily::Reflectivity)
            .range_folded_color();
        assert_eq!(
            reflectivity_color(&grid, 0),
            [0, 0, 0, 0],
            "raw 0 is nodata"
        );
        assert_eq!(
            reflectivity_color(&grid, 1),
            folded_color,
            "raw 1 is range folded"
        );

        let blended = render(&SweepBlend {
            incoming: &cut,
            incoming_grid: &grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        });

        assert_eq!(
            first_difference(&blended, &normal_render(&cut, &grid)),
            None
        );
        assert!(count_color(&blended, folded_color) > 0);
    }

    #[test]
    fn a_previous_sweep_that_reaches_further_than_the_incoming_one_is_not_clipped_to_it() {
        // Incoming reaches 2000 + 1000 * 20 = 22 km, previous 122 km.
        let (incoming_cut, incoming_grid) = sized_sweep(0.0, 360, INCOMING_RAW, 2_000, 1_000, 20);
        let (previous_cut, previous_grid) = sized_sweep(0.0, 360, PREVIOUS_RAW, 2_000, 1_000, 120);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let blended = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 0.0,
        });

        assert_eq!(
            pixel_at(&blended, 90.0, 40.0),
            previous_color,
            "40 km is outside the incoming sweep's 22 km footprint, not outside the frame"
        );
        assert_eq!(
            first_difference(&blended, &normal_render(&previous_cut, &previous_grid)),
            None
        );
    }

    #[test]
    fn a_previous_sweep_with_a_different_gate_spacing_keeps_its_own_outer_edge() {
        // Incoming: 1000 m gates to 122 km. Previous: 250 m gates to 16 km.
        let (incoming_cut, incoming_grid) = sized_sweep(0.0, 360, INCOMING_RAW, 2_000, 1_000, 120);
        let (previous_cut, previous_grid) = sized_sweep(0.0, 360, PREVIOUS_RAW, 1_000, 250, 60);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        let blended = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });

        assert_eq!(
            pixel_at(&blended, 90.0, 10.0),
            incoming_color,
            "swept, near"
        );
        assert_eq!(pixel_at(&blended, 90.0, 40.0), incoming_color, "swept, far");
        assert_eq!(
            pixel_at(&blended, 270.0, 10.0),
            previous_color,
            "unswept and inside the previous sweep's 16 km reach"
        );
        assert_eq!(
            pixel_at(&blended, 270.0, 40.0),
            [0, 0, 0, 0],
            "unswept and past 16 km, so the previous sweep has nothing to say"
        );
    }

    #[test]
    fn a_gate_also_claims_the_half_gate_below_its_nominal_range() {
        let (incoming_cut, incoming_grid) =
            sweep_with_marker_gate(0.0, 360, INCOMING_RAW, Some((7, MARKER_RAW)));
        let marker_color = reflectivity_color(&incoming_grid, MARKER_RAW);
        let plain_color = reflectivity_color(&incoming_grid, INCOMING_RAW);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: None,
            start_deg: 0.0,
            revealed_deg: 360.0,
        });

        // round((8500 - 2000) / 1000) = round(6.5) = 7. floor() would have made
        // 8.5 km gate 6 and painted it plain, so this pins the low half of the
        // gate that the 9.5 km assertion above cannot reach.
        assert_eq!(pixel(&rgba, 100, 83), marker_color, "8.5 km is gate 7");
        assert_eq!(pixel(&rgba, 100, 85), plain_color, "7.5 km is gate 6");
    }

    #[test]
    fn an_off_centre_radar_with_unequal_pixel_scales_still_registers_with_the_normal_raster() {
        // Nothing here divides evenly: the radar sits off centre on a quarter
        // pixel, the frame is not square and neither are the pixels.
        let raster_options = ViewportRasterOptions {
            width: 137,
            height: 91,
            radar_x_px: 33.25,
            radar_y_px: 71.75,
            km_per_px_x: 0.37,
            km_per_px_y: 0.61,
            rotation_rad: 0.0,
        };
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let expected = normal_render_with(&incoming_cut, &incoming_grid, raster_options);

        let mut blended = vec![9; viewport_rgba_buffer_len(raster_options)];
        render_sweep_blend_rgba_into(
            &SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: None,
                start_deg: 0.0,
                revealed_deg: 360.0,
            },
            raster_options,
            &ColorTableSet::default(),
            &mut blended,
        )
        .expect("blend renders");

        let difference = blended
            .chunks_exact(4)
            .zip(expected.chunks_exact(4))
            .position(|(left, right)| left != right);
        assert_eq!(
            difference, None,
            "the reveal test must use the same pixel centre the gate lookup uses"
        );
    }

    #[test]
    fn a_frame_with_no_area_and_a_flipped_scale_are_rendered_instead_of_panicking() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);

        for raster_options in [
            ViewportRasterOptions {
                width: 0,
                height: 0,
                radar_x_px: 0.0,
                radar_y_px: 0.0,
                km_per_px_x: 0.0,
                km_per_px_y: 0.0,
                rotation_rad: 0.0,
            },
            ViewportRasterOptions {
                width: 5,
                height: 5,
                radar_x_px: 2.5,
                radar_y_px: 2.5,
                km_per_px_x: -1.0,
                km_per_px_y: -1.0,
                rotation_rad: 0.0,
            },
            ViewportRasterOptions {
                width: 64,
                height: 48,
                radar_x_px: -400.0,
                radar_y_px: 900.0,
                km_per_px_x: 0.25,
                km_per_px_y: 0.25,
                rotation_rad: 0.0,
            },
        ] {
            let mut rgba = vec![0; viewport_rgba_buffer_len(raster_options)];
            let dimensions = render_sweep_blend_rgba_into(
                &SweepBlend {
                    incoming: &incoming_cut,
                    incoming_grid: &incoming_grid,
                    previous: Some((&previous_cut, &previous_grid)),
                    start_deg: 30.0,
                    revealed_deg: 200.0,
                },
                raster_options,
                &ColorTableSet::default(),
                &mut rgba,
            )
            .expect("blend renders");
            assert_eq!(
                dimensions,
                (raster_options.width.max(1), raster_options.height.max(1))
            );
        }
    }

    // -- velocity ----------------------------------------------------------
    //
    // NEXRAD scales velocity as `(raw - offset) / scale` with raw 0 below
    // threshold and raw 1 range folded, so these constants put 0 m/s on raw 129
    // and step half a metre per second per code, the way a 0.5 m/s resolution
    // Doppler cut really does.

    const VELOCITY_SCALE: f32 = 2.0;
    const VELOCITY_OFFSET: f32 = 129.0;
    /// A Nyquist a real VCP 212 Doppler leg could have. Anything faster than
    /// this folds, which is what gives the dealias tests something to undo.
    const NYQUIST_MPS: f32 = 25.0;

    fn velocity_raw(value_mps: f32) -> u8 {
        (value_mps * VELOCITY_SCALE + VELOCITY_OFFSET).round() as u8
    }

    /// Wrap a true velocity into the Nyquist interval, which is what the radar
    /// itself reports: the phase measurement cannot tell 35 m/s from -15.
    fn folded_mps(value_mps: f32) -> f32 {
        let interval = 2.0 * NYQUIST_MPS;
        ((value_mps + NYQUIST_MPS).rem_euclid(interval)) - NYQUIST_MPS
    }

    fn velocity_color(value_mps: f32) -> [u8; 4] {
        ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .color_for_value(value_mps)
    }

    /// A velocity sweep of `radial_count` radials one degree apart clockwise
    /// from `start_deg`, each gate holding whatever `value` says, folded into
    /// the Nyquist interval on the way in.
    fn velocity_sweep_with(
        start_deg: f32,
        radial_count: usize,
        value: impl Fn(usize, usize) -> f32,
    ) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count: GATE_COUNT,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range.clone(),
            VELOCITY_SCALE,
            VELOCITY_OFFSET,
            Some(0),
            Some(1),
        );
        for index in 0..radial_count {
            cut.radials.push(Radial {
                azimuth_deg: (start_deg + index as f32).rem_euclid(360.0),
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(NYQUIST_MPS),
                radial_status: None,
            });
            let row = (0..GATE_COUNT)
                .map(|gate| velocity_raw(folded_mps(value(index, gate))))
                .collect::<Vec<_>>();
            grid.push_u8_row_slice(index, &row)
                .expect("row fits the grid");
        }
        // A decoded cut carries its own moments, and `DealiasedSweepBlend`
        // reads the velocity back out of the cut the way a real one would.
        cut.moments.insert(MomentType::Velocity, grid.clone());
        (cut, grid)
    }

    fn velocity_sweep(
        start_deg: f32,
        radial_count: usize,
        value_mps: f32,
    ) -> (ElevationCut, MomentGrid) {
        velocity_sweep_with(start_deg, radial_count, |_, _| value_mps)
    }

    /// A sweep whose velocity ramps out along every radial and folds twice, so
    /// an unfolded picture and a folded one cannot be confused.
    fn folding_velocity_sweep(start_deg: f32, radial_count: usize) -> (ElevationCut, MomentGrid) {
        velocity_sweep_with(start_deg, radial_count, |_, gate| gate as f32 * 0.75)
    }

    fn render_storm_relative(blend: &SweepBlend<'_>, storm_motion: StormMotion) -> Vec<u8> {
        let mut rgba = vec![7; viewport_rgba_buffer_len(options())];
        let dimensions = render_storm_relative_sweep_blend_rgba_into(
            blend,
            storm_motion,
            options(),
            &ColorTableSet::default(),
            &mut rgba,
        )
        .expect("blend renders");
        assert_eq!(dimensions, (WIDTH, HEIGHT));
        rgba
    }

    fn one_cut_volume(cut: &ElevationCut, grid: &MomentGrid) -> RadarVolume {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), chrono::Utc::now());
        let mut volume_cut = cut.clone();
        volume_cut.moments.insert(grid.moment.clone(), grid.clone());
        volume.cuts.push(volume_cut);
        volume
    }

    /// The crate's ordinary storm-relative raster of one sweep: the reference a
    /// fully revealed storm-relative blend has to reproduce byte for byte.
    fn normal_storm_relative_render(
        cut: &ElevationCut,
        grid: &MomentGrid,
        storm_motion: StormMotion,
        raster_options: ViewportRasterOptions,
    ) -> Vec<u8> {
        let volume = one_cut_volume(cut, grid);
        let cache = ViewportMomentCache::new_with_color_tables(
            &volume,
            0,
            MomentType::Velocity,
            &ColorTableSet::default(),
        )
        .expect("velocity cache");
        let mut pixels = vec![0; viewport_rgba_buffer_len(raster_options)];
        cache
            .render_storm_relative_velocity_rgba_into(
                &volume,
                storm_motion,
                raster_options,
                &mut pixels,
            )
            .expect("storm relative render");
        pixels
    }

    #[test]
    fn a_storm_relative_blend_takes_the_motion_out_of_the_under_painted_sweep_too() {
        // Both sweeps read a flat +10 m/s. A storm moving toward 090 at 10 m/s
        // is receding at exactly 10 m/s along the beam pointed due east and
        // approaching at exactly 10 m/s along the beam pointed due west, so the
        // storm-relative field is 0 m/s due east and +20 m/s due west - and that
        // second number is the one the under-painted sweep has to be showing.
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep(0.0, 360, 10.0);
        assert_eq!(incoming_grid.scaled_value(0, 0), Some(10.0));

        let ground_relative = velocity_color(10.0);
        let east = velocity_color(0.0);
        let west = velocity_color(20.0);
        for color in [ground_relative, east, west] {
            assert_ne!(color[3], 0, "the three test values must all be paintable");
        }
        assert_ne!(east, ground_relative);
        assert_ne!(west, ground_relative);

        let rgba = render_storm_relative(
            &SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: Some((&previous_cut, &previous_grid)),
                start_deg: 0.0,
                revealed_deg: 180.0,
            },
            StormMotion {
                direction_deg: 90.0,
                speed_mps: 10.0,
            },
        );

        assert_eq!(
            pixel_at(&rgba, 90.0, 20.0),
            east,
            "arriving sweep, due east"
        );
        assert_eq!(
            pixel_at(&rgba, 270.0, 20.0),
            west,
            "under-painted sweep, due west: the same motion has to come out of it"
        );
    }

    #[test]
    fn a_partly_revealed_storm_relative_blend_of_one_sweep_over_itself_is_the_ordinary_raster() {
        // With the same sweep on both layers the only thing that can differ
        // either side of the reveal boundary is the shading, so byte identity
        // here is a direct proof that the two halves share a reference frame.
        // Subtracting the motion from one layer only fails this at every pixel
        // of the unrevealed 222.7 degrees.
        let (cut, grid) = velocity_sweep_with(0.0, 360, |row, gate| {
            (row as f32 * 0.3).sin() * 20.0 + gate as f32 * 0.4
        });
        let storm_motion = StormMotion {
            direction_deg: 235.0,
            speed_mps: 24.0,
        };

        let blended = render_storm_relative(
            &SweepBlend {
                incoming: &cut,
                incoming_grid: &grid,
                previous: Some((&cut, &grid)),
                start_deg: 0.0,
                revealed_deg: 137.3,
            },
            storm_motion,
        );
        let expected = normal_storm_relative_render(&cut, &grid, storm_motion, options());

        assert_eq!(first_difference(&blended, &expected), None);

        let ground_relative = render(&SweepBlend {
            incoming: &cut,
            incoming_grid: &grid,
            previous: Some((&cut, &grid)),
            start_deg: 0.0,
            revealed_deg: 137.3,
        });
        assert_ne!(
            blended, ground_relative,
            "a storm motion that changes no pixel would make this test vacuous"
        );
    }

    #[test]
    fn a_fully_revealed_storm_relative_blend_is_byte_identical_to_the_ordinary_raster() {
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep(0.0, 360, -30.0);
        let storm_motion = StormMotion {
            direction_deg: 305.0,
            speed_mps: 18.0,
        };

        let blended = render_storm_relative(
            &SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: Some((&previous_cut, &previous_grid)),
                start_deg: 0.0,
                revealed_deg: 360.0,
            },
            storm_motion,
        );

        assert_eq!(
            first_difference(
                &blended,
                &normal_storm_relative_render(
                    &incoming_cut,
                    &incoming_grid,
                    storm_motion,
                    options()
                )
            ),
            None,
            "a finished storm-relative sweep must not shift because something is under it"
        );
    }

    #[test]
    fn a_storm_relative_blend_still_decides_ownership_by_angle_across_the_seam() {
        // The real KTLX partial shape, in velocity: 240 radials from 197.5 deg
        // through 360/0 to 76.5, and a 121 degree hole across the seam.
        let (incoming_cut, incoming_grid) = velocity_sweep(197.5, 240, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep(0.5, 360, 10.0);
        let storm_motion = StormMotion {
            direction_deg: 90.0,
            speed_mps: 10.0,
        };

        let rgba = render_storm_relative(
            &SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: Some((&previous_cut, &previous_grid)),
                start_deg: 197.5,
                revealed_deg: 240.0,
            },
            storm_motion,
        );

        // Due east is inside the hole and due west is inside the swept arc, and
        // the storm-relative value at each is the same whichever layer supplies
        // it - which is the point. What must not happen is a blank.
        assert_eq!(pixel_at(&rgba, 90.0, 20.0), velocity_color(0.0), "hole");
        assert_eq!(pixel_at(&rgba, 270.0, 20.0), velocity_color(20.0), "swept");
        for step in 0..72 {
            let azimuth_deg = step as f32 * 5.0;
            assert_ne!(
                pixel_at(&rgba, azimuth_deg, 20.0)[3],
                0,
                "azimuth {azimuth_deg} went blank in a storm-relative blend"
            );
        }
    }

    #[test]
    fn a_broken_reveal_estimate_in_a_storm_relative_blend_shows_the_incoming_sweep_alone() {
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep(0.0, 360, -30.0);
        let storm_motion = StormMotion {
            direction_deg: 0.0,
            speed_mps: 0.0,
        };

        for (start_deg, revealed_deg) in [(0.0, f32::NAN), (f32::NAN, 180.0)] {
            let rgba = render_storm_relative(
                &SweepBlend {
                    incoming: &incoming_cut,
                    incoming_grid: &incoming_grid,
                    previous: Some((&previous_cut, &previous_grid)),
                    start_deg,
                    revealed_deg,
                },
                storm_motion,
            );
            assert_eq!(
                count_color(&rgba, velocity_color(-30.0)),
                0,
                "start {start_deg} reveal {revealed_deg} put a stale sweep on screen as live radar"
            );
            assert!(count_color(&rgba, velocity_color(10.0)) > 30_000);
        }
    }

    #[test]
    fn a_storm_relative_blend_of_a_moment_that_is_not_velocity_is_refused() {
        // Subtracting metres per second from decibels is arithmetic with no
        // meaning, and it would come out as a picture with every appearance of
        // one.
        let (cut, grid) = sweep(0.0, 360, INCOMING_RAW);
        let mut rgba = vec![0; viewport_rgba_buffer_len(options())];

        let error = render_storm_relative_sweep_blend_rgba_into(
            &SweepBlend {
                incoming: &cut,
                incoming_grid: &grid,
                previous: None,
                start_deg: 0.0,
                revealed_deg: 180.0,
            },
            StormMotion {
                direction_deg: 90.0,
                speed_mps: 10.0,
            },
            options(),
            &ColorTableSet::default(),
            &mut rgba,
        )
        .expect_err("reflectivity has no storm-relative form");

        match error {
            RenderError::CacheMomentMismatch { expected, actual } => {
                assert_eq!(expected, MomentType::Velocity);
                assert_eq!(actual, MomentType::Reflectivity);
            }
            other => panic!("expected a moment mismatch, got {other}"),
        }
    }

    #[test]
    fn a_previous_sweep_from_another_moment_is_dropped_rather_than_painted_in_the_wrong_units() {
        // Reflectivity under velocity would put dBZ colours in the unswept
        // wedge with the velocity legend beside them and no seam to give it
        // away. An empty wedge is the honest answer.
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);
        assert_ne!(previous_color[3], 0);

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });

        assert_eq!(pixel_at(&rgba, 90.0, 20.0), velocity_color(10.0), "swept");
        assert_eq!(
            pixel_at(&rgba, 270.0, 20.0),
            [0, 0, 0, 0],
            "the unswept wedge must not show a different moment's colours"
        );
        assert_eq!(count_color(&rgba, previous_color), 0);
    }

    #[test]
    fn a_dealiased_blend_unfolds_the_under_painted_sweep_as_well_as_the_arriving_one() {
        // The ramp reaches 44.25 m/s against a 25 m/s Nyquist, so the folded
        // picture and the unfolded one disagree over the outer two thirds of
        // every radial. If only the arriving sweep were unfolded, the unswept
        // wedge would still be showing the folded colours out there.
        let (cut, grid) = folding_velocity_sweep(0.0, 360);
        assert_eq!(grid.scaled_value(0, 40), Some(folded_mps(30.0)));
        assert_eq!(folded_mps(30.0), -20.0);

        let dealiased = DealiasedSweepBlend::new(&cut, Some(&cut), 0.0, 180.0)
            .expect("the arriving cut has velocity");
        let blended = render(&dealiased.blend());

        let unfolded_grid = dealias_velocity_grid(&cut, &grid);
        let expected = normal_render_with(&cut, &unfolded_grid, options());
        assert_eq!(
            first_difference(&blended, &expected),
            None,
            "both layers hold the same sweep, so every pixel must be the unfolded raster"
        );

        let folded = render(&SweepBlend {
            incoming: &cut,
            incoming_grid: &grid,
            previous: Some((&cut, &grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });
        assert_ne!(
            blended, folded,
            "if unfolding changed no pixel this test would prove nothing"
        );
    }

    #[test]
    fn a_dealiased_storm_relative_blend_is_the_ordinary_one_when_both_layers_hold_one_sweep() {
        let (cut, grid) = folding_velocity_sweep(0.0, 360);
        let storm_motion = StormMotion {
            direction_deg: 200.0,
            speed_mps: 21.0,
        };

        let dealiased = DealiasedSweepBlend::new(&cut, Some(&cut), 0.0, 214.5)
            .expect("the arriving cut has velocity");
        let blended = render_storm_relative(&dealiased.blend(), storm_motion);

        let unfolded_grid = dealias_velocity_grid(&cut, &grid);
        assert_eq!(
            first_difference(
                &blended,
                &normal_storm_relative_render(&cut, &unfolded_grid, storm_motion, options())
            ),
            None,
            "the unfolded pair and the storm motion have to compose to the ordinary raster"
        );
    }

    #[test]
    fn a_previous_cut_with_no_velocity_costs_the_under_paint_and_not_the_frame() {
        let (cut, _) = folding_velocity_sweep(0.0, 360);
        let (reflectivity_cut, reflectivity_grid) = sweep(0.0, 360, PREVIOUS_RAW);

        assert!(
            dealias_cut_velocity(&reflectivity_cut).is_none(),
            "a cut with no velocity has nothing to unfold"
        );
        assert!(
            DealiasedSweepBlend::new(&reflectivity_cut, Some(&cut), 0.0, 180.0).is_none(),
            "an arriving cut with no velocity has no picture to draw at all"
        );

        let dealiased = DealiasedSweepBlend::new(&cut, Some(&reflectivity_cut), 0.0, 180.0)
            .expect("the arriving cut has velocity");
        assert!(dealiased.blend().previous.is_none());

        let rgba = render(&dealiased.blend());
        assert_ne!(pixel_at(&rgba, 90.0, 20.0)[3], 0, "swept");
        assert_eq!(
            pixel_at(&rgba, 270.0, 20.0),
            [0, 0, 0, 0],
            "unswept, with nothing behind it"
        );
        assert_eq!(
            count_color(&rgba, reflectivity_color(&reflectivity_grid, PREVIOUS_RAW)),
            0
        );
    }

    // -- adversarial review, second pass -----------------------------------
    //
    // Everything below this line was written against the finished module rather
    // than alongside it, to break it rather than to describe it. Each test names
    // the input that failed before the fix beside it.

    /// A sweep of one chosen moment, scale and offset, so a test can hand the
    /// two layers grids that a caller could really get them confused over.
    fn moment_sweep(
        moment: MomentType,
        radial_count: usize,
        raw: u8,
        scale: f32,
        offset: f32,
        gate_count: usize,
    ) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: FIRST_GATE_M,
            gate_spacing_m: GATE_SPACING_M,
            gate_count,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid =
            MomentGrid::new_u8(moment, gate_range.clone(), scale, offset, Some(0), Some(1));
        let row = vec![raw; gate_count];
        for index in 0..radial_count {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 30,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            grid.push_u8_row_slice(index, &row)
                .expect("row fits the grid");
        }
        (cut, grid)
    }

    fn pixel_in(rgba: &[u8], raster_options: ViewportRasterOptions, x: u32, y: u32) -> [u8; 4] {
        let index = (y as usize * raster_options.width as usize + x as usize) * 4;
        [
            rgba[index],
            rgba[index + 1],
            rgba[index + 2],
            rgba[index + 3],
        ]
    }

    #[test]
    fn a_reveal_estimate_that_is_not_finite_never_presents_the_previous_sweep_as_live_radar() {
        // `revealed_deg: f32::NEG_INFINITY` painted 40 401 pixels of 40 401 from
        // the previous sweep before this was fixed: `is_nan()` let it through and
        // `clamp(0.0, 360.0)` turned it into a zero reveal, which is the one
        // value that means "show the previous sweep everywhere". A measured
        // antenna rate is degrees over a time span, so a span that rounds to zero
        // makes the rate infinite and the arc infinite with it - the same
        // arithmetic that produces the NaN this function already guarded.
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);
        assert_ne!(incoming_color, previous_color);

        for (start_deg, revealed_deg) in [
            (0.0_f32, f32::NEG_INFINITY),
            (0.0, f32::INFINITY),
            (0.0, f32::NAN),
            (f32::INFINITY, 180.0),
            (f32::NEG_INFINITY, 180.0),
            (f32::NEG_INFINITY, f32::NEG_INFINITY),
            (f32::NAN, f32::NEG_INFINITY),
        ] {
            let rgba = render(&SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: Some((&previous_cut, &previous_grid)),
                start_deg,
                revealed_deg,
            });
            assert_eq!(
                count_color(&rgba, previous_color),
                0,
                "start {start_deg} reveal {revealed_deg} put a stale sweep on screen as live radar"
            );
            assert!(
                count_color(&rgba, incoming_color) > 30_000,
                "start {start_deg} reveal {revealed_deg} should degrade to the incoming sweep alone"
            );
        }

        // The one negative reveal that is a measurement and not a broken
        // estimate still means "nothing of this tilt has arrived".
        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: -0.25,
        });
        assert_eq!(count_color(&rgba, incoming_color), 0);
        assert!(count_color(&rgba, previous_color) > 30_000);
    }

    #[test]
    fn a_previous_sweep_of_another_moment_from_the_same_colour_table_is_dropped_too() {
        // Two moments the render path cannot tell apart still describe different
        // quantities, and painting one into the unswept wedge of the other
        // leaves no seam and no legend to give it away. The test is the moment
        // itself, never the colour table.
        //
        // It used to read ZDR against correlation coefficient, which both fell
        // to the Generic ramp. They have their own families now, so the case is
        // written against what still shares Generic: the unclassified moments.
        // Every cached WSR-88D volume carries `Unknown("CFP")`, clutter filter
        // power, so this is a pairing the app can really be handed.
        let (incoming_cut, incoming_grid) = moment_sweep(
            MomentType::Unknown("CFP".to_owned()),
            360,
            120,
            10.0,
            100.0,
            GATE_COUNT,
        );
        let (previous_cut, previous_grid) = moment_sweep(
            MomentType::Unknown("SNR".to_owned()),
            360,
            200,
            250.0,
            0.0,
            GATE_COUNT,
        );
        assert_eq!(incoming_grid.scaled_value(0, 0), Some(2.0));
        assert_eq!(previous_grid.scaled_value(0, 0), Some(0.8));
        assert_eq!(
            color_family_for_moment(&incoming_grid.moment),
            color_family_for_moment(&previous_grid.moment),
            "this test is only worth anything while the two share a table"
        );

        let tables = ColorTableSet::default();
        let generic = tables.for_family(ColorTableFamily::Generic);
        let previous_color = color_for_raw(&previous_grid, generic, 200);
        let incoming_color = color_for_raw(&incoming_grid, generic, 120);
        assert_ne!(previous_color[3], 0, "the under-paint must be paintable");

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });

        assert_eq!(pixel_at(&rgba, 90.0, 20.0), incoming_color, "swept");
        assert_eq!(
            pixel_at(&rgba, 270.0, 20.0),
            [0, 0, 0, 0],
            "a different moment in the same table is still a different quantity"
        );
        assert_eq!(count_color(&rgba, previous_color), 0);
    }

    #[test]
    fn a_previous_sweep_is_decoded_with_its_own_scale_and_offset() {
        // The layers are free to disagree about how a code becomes a number -
        // they are two different messages off the wire - and each has to be
        // decoded by its own header. 45 dBZ is raw 156 on the incoming sweep's
        // scale and raw 75 on the previous sweep's; both must come out 45 dBZ.
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let (previous_cut, previous_grid) =
            moment_sweep(MomentType::Reflectivity, 360, 75, 1.0, 30.0, GATE_COUNT);
        assert_eq!(incoming_grid.scaled_value(0, 0), Some(45.0));
        assert_eq!(previous_grid.scaled_value(0, 0), Some(45.0));

        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });

        let forty_five = reflectivity_color(&incoming_grid, PREVIOUS_RAW);
        assert_ne!(forty_five[3], 0);
        assert_eq!(pixel_at(&rgba, 90.0, 20.0), forty_five, "swept");
        assert_eq!(
            pixel_at(&rgba, 270.0, 20.0),
            forty_five,
            "the under-paint was decoded with the wrong header"
        );
        // Raw 75 read through the INCOMING sweep's scale is 4.5 dBZ, which the
        // default table paints a visibly different colour from 45 dBZ (the
        // old default left it blank outright; AWIPS Wilson shows low dBZ on
        // purpose) - either way the wedge test above catches a wrong-header
        // decode.
        assert_ne!(reflectivity_color(&incoming_grid, 75), forty_five);
    }

    #[test]
    fn a_dealiased_blend_of_a_tilt_whose_first_radial_has_not_landed_still_under_paints() {
        // A velocity moment that exists with no rows in it is every tilt for the
        // moment between its header and its first chunk. The plain entry point
        // draws that as the previous sweep everywhere outside the arc; the
        // dealiased one used to refuse the frame outright, which took the
        // under-paint away at exactly the instant nothing else was on screen.
        let (arriving, _) = velocity_sweep(0.0, 0, 10.0);
        assert!(arriving.moments.contains_key(&MomentType::Velocity));
        let unfolded_nothing =
            dealias_cut_velocity(&arriving).expect("an empty sweep still unfolds");
        assert_eq!(unfolded_nothing.radial_count(), 0);

        let (previous_cut, _) = folding_velocity_sweep(0.0, 360);
        let previous_unfolded = dealias_cut_velocity(&previous_cut).expect("velocity unfolds");
        let reference = normal_render_of(&previous_cut, &previous_unfolded, options());

        let dealiased = DealiasedSweepBlend::new(&arriving, Some(&previous_cut), 0.0, 45.0)
            .expect("a tilt that has announced itself is still a picture");
        let rgba = render(&dealiased.blend());

        let (inside_x, inside_y) = pixel_for(20.0, 20.0);
        let (outside_x, outside_y) = pixel_for(200.0, 20.0);
        assert_ne!(
            pixel(&reference, outside_x, outside_y)[3],
            0,
            "the reference must paint the pixel this test reads"
        );
        assert_eq!(
            pixel(&rgba, outside_x, outside_y),
            pixel(&reference, outside_x, outside_y),
            "outside the arc the previous sweep must show through, unfolded"
        );
        assert_eq!(
            pixel(&rgba, inside_x, inside_y),
            [0, 0, 0, 0],
            "inside the arc the arriving sweep is authoritative even with no rows"
        );

        // A cut with no velocity moment at all is still the one case with no
        // picture to draw, and is still told apart from this one.
        let (reflectivity_cut, _) = sweep(0.0, 360, PREVIOUS_RAW);
        assert!(dealias_cut_velocity(&reflectivity_cut).is_none());
        assert!(
            DealiasedSweepBlend::new(&reflectivity_cut, Some(&previous_cut), 0.0, 45.0).is_none()
        );
    }

    #[test]
    fn every_pixel_of_a_storm_relative_blend_is_the_ordinary_raster_of_the_layer_that_owns_it() {
        // The two layers are rotated half a turn against each other: the
        // previous sweep's row 90 points due WEST while the incoming sweep's
        // row 90 points due EAST, and one radial of the previous sweep - the one
        // pointing west - carries 4 m/s where every other gate in either sweep
        // carries 10. So the two cuts can be told apart by their pictures, which
        // the usual one-sweep-over-itself test cannot do because there the two
        // layers are the same object.
        //
        // The claim is then the whole of it, pixel by pixel rather than at a
        // handful of sampled azimuths: inside the revealed arc the frame is the
        // ordinary storm-relative raster of the arriving sweep, outside it the
        // ordinary storm-relative raster of the previous sweep, and nothing else
        // anywhere. That catches a motion left off the under-paint, a motion
        // applied with the wrong sign, a basis built from the wrong cut, and an
        // azimuth lookup built from the wrong cut, each of which moves pixels
        // this comparison names.
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep_with(180.0, 360, |row, _| {
            if (180 + row) % 360 == 270 { 4.0 } else { 10.0 }
        });
        assert_eq!(previous_cut.radials[90].azimuth_deg, 270.0);
        assert_eq!(incoming_cut.radials[90].azimuth_deg, 90.0);
        assert_eq!(previous_grid.scaled_value(90, 18), Some(4.0));
        assert_eq!(incoming_grid.scaled_value(90, 18), Some(10.0));

        let storm_motion = StormMotion {
            direction_deg: 90.0,
            speed_mps: 10.0,
        };
        let start_deg = 0.0;
        let revealed_deg = 180.0;
        let blended = render_storm_relative(
            &SweepBlend {
                incoming: &incoming_cut,
                incoming_grid: &incoming_grid,
                previous: Some((&previous_cut, &previous_grid)),
                start_deg,
                revealed_deg,
            },
            storm_motion,
        );
        let incoming_alone =
            normal_storm_relative_render(&incoming_cut, &incoming_grid, storm_motion, options());
        let previous_alone =
            normal_storm_relative_render(&previous_cut, &previous_grid, storm_motion, options());

        let mut checked = (0_usize, 0_usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let azimuth_deg = pixel_azimuth_deg(options(), x, y);
                let owner = if azimuth_is_revealed(start_deg, revealed_deg, azimuth_deg) {
                    checked.0 += 1;
                    &incoming_alone
                } else {
                    checked.1 += 1;
                    &previous_alone
                };
                assert_eq!(
                    pixel(&blended, x, y),
                    pixel(owner, x, y),
                    "pixel ({x}, {y}) at {azimuth_deg} deg"
                );
            }
        }
        assert_eq!(checked.0 + checked.1, (WIDTH * HEIGHT) as usize);

        // Non-vacuity: the west-facing radial reads differently on the two
        // sweeps, and the storm motion moves it again.
        let west = pixel_for(270.0, 20.0);
        assert_ne!(
            pixel(&previous_alone, west.0, west.1),
            pixel(&incoming_alone, west.0, west.1),
            "the two layers must be distinguishable due west"
        );
        assert_ne!(
            pixel(&previous_alone, west.0, west.1),
            pixel(
                &normal_render_of(&previous_cut, &previous_grid, options()),
                west.0,
                west.1
            ),
            "the storm motion must change the under-paint due west"
        );
        assert_ne!(pixel(&blended, west.0, west.1)[3], 0);
    }

    #[test]
    fn a_storm_relative_reveal_boundary_is_continuous_where_it_crosses_the_zero_seam() {
        // The two layers hold the same flat +10 m/s on separate objects, and the
        // reveal boundary is put exactly on 0/360. A storm moving due NORTH at
        // 20 m/s projects its whole speed onto the beam that points due north,
        // so this is the azimuth where a reference-frame mistake is largest:
        // storm-relative 10 - 20 = -10 m/s against a ground-relative +10.
        //
        // Sampling is by RADIAL rather than by angle. Radial 0 owns the half
        // degree either side of due north, so it holds pixels the reveal test
        // hands to the incoming sweep AND pixels it hands to the previous one,
        // and with a flat field every pixel of one radial is one value. Their
        // colours therefore have to be the same byte for byte, whatever the
        // colour table does between stops.
        let seam_options = ViewportRasterOptions {
            width: 401,
            height: 401,
            radar_x_px: 200.5,
            radar_y_px: 200.5,
            km_per_px_x: 0.1,
            km_per_px_y: 0.1,
            rotation_rad: 0.0,
        };
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep(0.0, 360, 10.0);
        let storm_motion = StormMotion {
            direction_deg: 0.0,
            speed_mps: 20.0,
        };
        let blend = SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        };
        let blended = render_storm_relative_with(&blend, storm_motion, seam_options);
        let ground_relative = render_blend_with(&blend, seam_options);

        let lookup = AzimuthLookup::new(&incoming_cut, &incoming_grid);
        // (revealed, unrevealed) colours seen on the seam radial and on the
        // radial the other boundary of the same 180 degree arc falls on.
        let mut seam = (Vec::new(), Vec::new());
        let mut opposite = (Vec::new(), Vec::new());
        let mut seam_ground_relative = Vec::new();
        for y in 0..seam_options.height {
            for x in 0..seam_options.width {
                let dx_km = (x as f32 + 0.5 - seam_options.radar_x_px) * seam_options.km_per_px_x;
                let dy_km = (seam_options.radar_y_px - (y as f32 + 0.5)) * seam_options.km_per_px_y;
                let range_km = dx_km.hypot(dy_km);
                if !(5.0..19.0).contains(&range_km) {
                    continue;
                }
                let azimuth_deg = pixel_azimuth_deg(seam_options, x, y);
                let Some(row) = lookup.row_for_azimuth(azimuth_deg) else {
                    continue;
                };
                let revealed = azimuth_is_revealed(0.0, 180.0, azimuth_deg);
                let color = pixel_in(&blended, seam_options, x, y);
                let bucket = match row {
                    0 => &mut seam,
                    180 => &mut opposite,
                    _ => continue,
                };
                if revealed {
                    bucket.0.push(color);
                } else {
                    bucket.1.push(color);
                }
                if row == 0 {
                    seam_ground_relative.push(pixel_in(&ground_relative, seam_options, x, y));
                }
            }
        }

        println!(
            "seam radial: {} revealed px, {} unrevealed px; opposite radial: {} / {}",
            seam.0.len(),
            seam.1.len(),
            opposite.0.len(),
            opposite.1.len()
        );
        for (label, side) in [
            ("seam revealed", &seam.0),
            ("seam unrevealed", &seam.1),
            ("opposite revealed", &opposite.0),
            ("opposite unrevealed", &opposite.1),
        ] {
            assert!(
                !side.is_empty(),
                "{label} sampled no pixels, so this test proves nothing"
            );
        }

        // Due north the projection is the whole storm speed, so both sides of
        // the 0/360 boundary must read 10 - 20 = -10 m/s.
        let inbound_ten = velocity_color(-10.0);
        assert_ne!(inbound_ten, velocity_color(10.0));
        assert_ne!(inbound_ten[3], 0);
        for (label, side) in [("revealed", &seam.0), ("unrevealed", &seam.1)] {
            for color in side.iter() {
                assert_eq!(
                    *color, inbound_ten,
                    "the {label} side of the 0/360 seam is not in the storm's frame"
                );
            }
        }
        assert!(
            seam_ground_relative
                .iter()
                .all(|color| *color == velocity_color(10.0)),
            "the ground-relative blend must differ, or the motion changed nothing"
        );

        // Due south the projection reverses. Its exact value is left to the
        // arithmetic - what matters is that both sides agree on it and that
        // neither of them is the ground-relative answer.
        let outbound = opposite.0[0];
        assert_ne!(outbound, velocity_color(10.0));
        assert_ne!(outbound[3], 0);
        for (label, side) in [("revealed", &opposite.0), ("unrevealed", &opposite.1)] {
            for color in side.iter() {
                assert_eq!(
                    *color, outbound,
                    "the {label} side of the 180 degree boundary moved reference frame"
                );
            }
        }

        assert_eq!(
            differing_pixels(
                &blended,
                &normal_storm_relative_render(
                    &incoming_cut,
                    &incoming_grid,
                    storm_motion,
                    seam_options
                )
            )
            .len(),
            0,
            "a partly revealed storm-relative blend of two identical sweeps is the ordinary raster"
        );
    }

    #[test]
    fn a_storm_motion_that_is_not_finite_does_what_the_ordinary_raster_does() {
        // A broken motion vector cannot reach the ownership test - it is a
        // shading input - so the thing to prove is that it neither panics nor
        // diverges from the raster the rest of the app draws. Every colour table
        // sends a non-finite value to transparent, so the honest answer is an
        // empty frame, and that is what both paths give.
        let (incoming_cut, incoming_grid) = velocity_sweep(0.0, 360, 10.0);
        let (previous_cut, previous_grid) = velocity_sweep(0.0, 360, -30.0);
        for (direction_deg, speed_mps) in [
            (90.0_f32, f32::NAN),
            (90.0, f32::INFINITY),
            (90.0, f32::NEG_INFINITY),
            (f32::NAN, 10.0),
            (f32::INFINITY, 10.0),
        ] {
            let storm_motion = StormMotion {
                direction_deg,
                speed_mps,
            };
            let rgba = render_storm_relative(
                &SweepBlend {
                    incoming: &incoming_cut,
                    incoming_grid: &incoming_grid,
                    previous: Some((&previous_cut, &previous_grid)),
                    start_deg: 0.0,
                    revealed_deg: 180.0,
                },
                storm_motion,
            );
            assert_eq!(
                count_color(&rgba, velocity_color(-30.0)),
                0,
                "direction {direction_deg} speed {speed_mps} showed the previous sweep"
            );
            assert_eq!(
                first_difference(
                    &rgba,
                    &normal_storm_relative_render(
                        &incoming_cut,
                        &incoming_grid,
                        storm_motion,
                        options()
                    )
                ),
                None,
                "direction {direction_deg} speed {speed_mps} diverged from the ordinary raster"
            );
        }
    }

    #[test]
    fn a_layer_with_no_radials_or_no_gates_renders_instead_of_panicking() {
        let (incoming_cut, incoming_grid) = sweep(0.0, 360, INCOMING_RAW);
        let (previous_cut, previous_grid) = sweep(0.0, 360, PREVIOUS_RAW);
        let (empty_cut, empty_grid) = sweep(0.0, 0, INCOMING_RAW);
        let (gateless_cut, gateless_grid) = moment_sweep(
            MomentType::Reflectivity,
            360,
            INCOMING_RAW,
            SCALE,
            OFFSET,
            0,
        );
        let incoming_color = reflectivity_color(&incoming_grid, INCOMING_RAW);
        let previous_color = reflectivity_color(&previous_grid, PREVIOUS_RAW);

        // Nothing has arrived yet: the whole previous sweep, and no incoming
        // pixel anywhere inside the arc the caller says has been swept.
        let rgba = render(&SweepBlend {
            incoming: &empty_cut,
            incoming_grid: &empty_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });
        assert_eq!(
            pixel_at(&rgba, 90.0, 20.0),
            [0, 0, 0, 0],
            "swept, but empty"
        );
        assert_eq!(pixel_at(&rgba, 270.0, 20.0), previous_color, "unswept");

        // A previous sweep with no gates has no footprint and simply does not
        // paint; the arriving sweep is untouched by it.
        let rgba = render(&SweepBlend {
            incoming: &incoming_cut,
            incoming_grid: &incoming_grid,
            previous: Some((&gateless_cut, &gateless_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });
        assert_eq!(pixel_at(&rgba, 90.0, 20.0), incoming_color, "swept");
        assert_eq!(pixel_at(&rgba, 270.0, 20.0), [0, 0, 0, 0], "unswept");

        // And the same grid arriving rather than under-painted.
        let rgba = render(&SweepBlend {
            incoming: &gateless_cut,
            incoming_grid: &gateless_grid,
            previous: Some((&previous_cut, &previous_grid)),
            start_deg: 0.0,
            revealed_deg: 180.0,
        });
        assert_eq!(pixel_at(&rgba, 90.0, 20.0), [0, 0, 0, 0], "swept");
        assert_eq!(pixel_at(&rgba, 270.0, 20.0), previous_color, "unswept");

        // Two empty layers and a viewport with no area, together.
        let mut rgba = vec![7; viewport_rgba_buffer_len(options())];
        render_sweep_blend_rgba_into(
            &SweepBlend {
                incoming: &empty_cut,
                incoming_grid: &empty_grid,
                previous: Some((&gateless_cut, &gateless_grid)),
                start_deg: 0.0,
                revealed_deg: 180.0,
            },
            options(),
            &ColorTableSet::default(),
            &mut rgba,
        )
        .expect("two empty layers still render");
        assert!(rgba.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_full_reveal_over_a_wider_previous_sweep_is_still_the_ordinary_raster() {
        // The scanline span is the UNION of the two sweeps' spans, so a previous
        // sweep that reaches further makes the blend visit pixels the ordinary
        // raster never looks at. None of them may be painted at a full reveal,
        // in any of the three storages. One kilometre per pixel, so the wider
        // sweep's extra 30 km of range is inside the frame and really is extra
        // scanline rather than clipped away at the edge.
        let wide_options = ViewportRasterOptions {
            width: WIDTH,
            height: HEIGHT,
            radar_x_px: 100.5,
            radar_y_px: 100.5,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
            rotation_rad: 0.0,
        };
        let (wide_cut, wide_grid) =
            sized_sweep(0.0, 360, PREVIOUS_RAW, FIRST_GATE_M, GATE_SPACING_M, 90);
        for (label, (cut, grid)) in [
            ("u8", sweep(0.0, 360, INCOMING_RAW)),
            ("u16", u16_sweep()),
            ("f32", f32_sweep()),
        ] {
            let expected = normal_render_with(&cut, &grid, wide_options);
            let under_paint = normal_render_with(&wide_cut, &wide_grid, wide_options);
            assert!(
                blanked_pixels(&under_paint, &expected).len() > 1_000,
                "{label}: the under-paint has to reach past the arriving sweep"
            );

            let blended = render_blend_with(
                &SweepBlend {
                    incoming: &cut,
                    incoming_grid: &grid,
                    previous: Some((&wide_cut, &wide_grid)),
                    start_deg: 0.0,
                    revealed_deg: 360.0,
                },
                wide_options,
            );
            assert_eq!(first_difference(&blended, &expected), None, "{label}");
            assert!(
                blended.chunks_exact(4).any(|pixel| pixel[3] != 0),
                "{label}"
            );
        }
    }

    #[test]
    fn a_half_unfolded_blend_paints_each_layer_in_its_own_units_and_disagrees_across_the_seam() {
        // The layers are free to differ in STORAGE as well as in scale: an
        // unfolded velocity grid is u16 words on a 0.1 m/s scale while the sweep
        // it came from is u8 codes on a 0.5 m/s one. Handing one of each to the
        // renderer is what a caller does when it unfolds the arriving sweep and
        // forgets the previous one, so this pins two things at once - that
        // nothing reads the u16 grid through the u8 palette, and that the
        // renderer cannot save such a caller.
        let (cut, folded_grid) = folding_velocity_sweep(0.0, 360);
        let unfolded_grid = dealias_velocity_grid(&cut, &folded_grid);
        assert!(matches!(unfolded_grid.storage, MomentStorage::U16(_)));
        assert!(matches!(folded_grid.storage, MomentStorage::U8(_)));
        assert_eq!(unfolded_grid.moment, folded_grid.moment);

        let start_deg = 0.0;
        let revealed_deg = 180.0;
        let blended = render(&SweepBlend {
            incoming: &cut,
            incoming_grid: &unfolded_grid,
            previous: Some((&cut, &folded_grid)),
            start_deg,
            revealed_deg,
        });
        let unfolded_alone = normal_render_of(&cut, &unfolded_grid, options());
        let folded_alone = normal_render_of(&cut, &folded_grid, options());

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let azimuth_deg = pixel_azimuth_deg(options(), x, y);
                let owner = if azimuth_is_revealed(start_deg, revealed_deg, azimuth_deg) {
                    &unfolded_alone
                } else {
                    &folded_alone
                };
                assert_eq!(
                    pixel(&blended, x, y),
                    pixel(owner, x, y),
                    "pixel ({x}, {y}) at {azimuth_deg} deg"
                );
            }
        }

        // Gate 40 is 30 m/s of true velocity against a 25 m/s Nyquist, so the
        // radar reported -20 there. The unfolded half of the picture says 30 and
        // the folded half still says -20: this is the jump `DealiasedSweepBlend`
        // exists to prevent, drawn here on purpose so the module's claim that it
        // lands "in exactly the couplets a forecaster is looking at" is a
        // measurement rather than an assertion.
        assert_eq!(folded_grid.scaled_value(0, 40), Some(-20.0));
        let east = pixel_at(&blended, 90.0, 42.0);
        let west = pixel_at(&blended, 270.0, 42.0);
        assert_ne!(east[3], 0, "the unfolded half must paint gate 40");
        assert_ne!(west[3], 0, "the folded half must paint gate 40");
        assert_ne!(
            east, west,
            "unfolding one layer only has to show at the reveal boundary, or this test is not \
             measuring the thing it names"
        );
        assert_eq!(west, velocity_color(-20.0), "the folded half reads -20 m/s");
    }

    // -- real radar --------------------------------------------------------
    //
    // Every sweep above this line is one somebody made up, and a made-up sweep
    // cannot answer the questions this module is actually exposed to: what a
    // real antenna's azimuth spacing does to the reveal boundary, what two
    // independent unfoldings of one real Nyquist interval disagree about, and
    // whether a real storm's velocity field stays continuous across the seam.
    // So the tests below run on whatever the live cache holds, discovered the
    // way `interpolate::real_data_tests` in this crate discovers it, and they
    // skip out loud rather than failing on a machine with an empty cache. Run
    // them with `--nocapture` to read the measurements they print.

    /// Where the live service leaves decoded volumes. Mirrors
    /// `interpolate::real_data_tests::level2_cache_dir`, which cannot be shared
    /// because it lives in another module's test scope.
    fn level2_cache_dir() -> PathBuf {
        if let Some(path) = std::env::var_os("RADAR_WORKSTATION_L2_CACHE") {
            return PathBuf::from(path);
        }
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path)
                .join("FahrenheitResearch")
                .join("RadarWorkstation")
                .join("cache")
                .join("level2-live");
        }
        PathBuf::from("level2-live")
    }

    /// One Archive II file named by `NEXRAD_LEVEL2_SAMPLE` -- the workspace's
    /// existing convention for pointing a test at real data.
    fn pinned_sample() -> Option<PathBuf> {
        let path = PathBuf::from(std::env::var_os("NEXRAD_LEVEL2_SAMPLE")?);
        assert!(
            path.is_file(),
            "NEXRAD_LEVEL2_SAMPLE names {}, which is not a file",
            path.display()
        );
        Some(path)
    }

    /// The volumes these tests run on, most deterministic source first: the
    /// pinned `NEXRAD_LEVEL2_SAMPLE`, else every archive file in the cache
    /// directory, sorted.
    ///
    /// The default source is the LIVE cache the running app fills, so which
    /// volumes are there records which radars were last looked at and changes
    /// while the app runs. No test below may FAIL because of what it found:
    /// they state their preconditions and skip out loud. Pin
    /// `NEXRAD_LEVEL2_SAMPLE` at one volume to make a run repeatable, which is
    /// what a gate needs.
    fn cached_volumes() -> Vec<PathBuf> {
        if let Some(path) = pinned_sample() {
            return vec![path];
        }
        let Ok(entries) = std::fs::read_dir(level2_cache_dir()) else {
            return Vec::new();
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with("_V06"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    /// How many cached volumes a search may decode before giving up. Decoding is
    /// seconds per file and these tests run in the ordinary suite.
    const MAX_VOLUMES_SEARCHED: usize = 8;

    /// 700 px square at 0.7 km per pixel, so a 245 km radius fits inside the
    /// frame - the shape `workstation_app`'s `sweep_replay` photographs.
    fn real_options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 700,
            height: 700,
            radar_x_px: 350.0,
            radar_y_px: 350.0,
            km_per_px_x: 0.7,
            km_per_px_y: 0.7,
            rotation_rad: 0.0,
        }
    }

    /// A storm motion large enough that a layer accidentally left in the
    /// ground-relative frame could not hide: 20 m/s toward 060 is 39 kt, an
    /// unremarkable supercell.
    fn real_storm_motion() -> StormMotion {
        StormMotion {
            direction_deg: 60.0,
            speed_mps: 20.0,
        }
    }

    fn decode(path: &Path) -> RadarVolume {
        nexrad_io::decode_volume_from_path(path)
            .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()))
    }

    /// The first cached volume that carries a full Doppler sweep.
    fn real_velocity_volume() -> Option<&'static RadarVolume> {
        static VOLUME: OnceLock<Option<RadarVolume>> = OnceLock::new();
        VOLUME
            .get_or_init(|| {
                cached_volumes()
                    .into_iter()
                    .take(MAX_VOLUMES_SEARCHED)
                    .map(|path| decode(&path))
                    .find(|volume| velocity_cut_index(volume).is_some())
            })
            .as_ref()
    }

    /// The lowest cut carrying a full Doppler sweep.
    fn velocity_cut_index(volume: &RadarVolume) -> Option<usize> {
        volume.cuts.iter().position(|cut| {
            cut.moments
                .get(&MomentType::Velocity)
                .is_some_and(|grid| grid.radial_count() >= 360)
        })
    }

    fn real_velocity_cut_index(volume: &RadarVolume) -> usize {
        velocity_cut_index(volume).expect("the sample volume has a velocity cut")
    }

    /// How many gates a cut's unfolding actually moves.
    ///
    /// A gate the unfolding leaves alone cannot tell a folded picture from an
    /// unfolded one, so this is what decides whether a dealias test is testing
    /// anything. It is not a formality: on KTLX 2026-08-17 07:24:02 the fastest
    /// gate in the whole volume was 26.0 m/s against a 26.11 m/s Nyquist, so
    /// nothing folded anywhere and every dealias assertion made against that
    /// file passes without exercising a single correction.
    fn corrected_gate_count(cut: &ElevationCut) -> usize {
        let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
            return 0;
        };
        let Some(unfolded) = dealias_cut_velocity(cut) else {
            return 0;
        };
        (0..grid.radial_count())
            .flat_map(|row| (0..grid.gate_range.gate_count).map(move |gate| (row, gate)))
            .filter(|(row, gate)| {
                match (
                    grid.scaled_value(*row, *gate),
                    unfolded.scaled_value(*row, *gate),
                ) {
                    (Some(observed), Some(value)) => (value - observed).abs() > 1.0,
                    _ => false,
                }
            })
            .count()
    }

    /// The cached volume and cut whose velocity folds the most, so the dealias
    /// tests run on data that has something to unfold.
    ///
    /// `None` when no cached volume has any: a calm night is a real state of the
    /// cache, and the tests that need folding say so and stop rather than
    /// passing on a cut where unfolding is the identity.
    fn real_folded_velocity_cut() -> Option<(&'static RadarVolume, usize)> {
        static FOUND: OnceLock<Option<(RadarVolume, usize)>> = OnceLock::new();
        FOUND
            .get_or_init(|| {
                for path in cached_volumes().into_iter().take(MAX_VOLUMES_SEARCHED) {
                    let volume = decode(&path);
                    let Some((index, corrected)) = volume
                        .cuts
                        .iter()
                        .enumerate()
                        .map(|(index, cut)| (index, corrected_gate_count(cut)))
                        .max_by_key(|(_, corrected)| *corrected)
                    else {
                        continue;
                    };
                    if corrected > 100 {
                        println!(
                            "{} cut {index}: {corrected} gates corrected by unfolding",
                            volume.site.id
                        );
                        return Some((volume, index));
                    }
                }
                None
            })
            .as_ref()
            .map(|(volume, index)| (volume, *index))
    }

    fn velocity_grid_of(cut: &ElevationCut) -> &MomentGrid {
        cut.moments
            .get(&MomentType::Velocity)
            .expect("cut carries velocity")
    }

    /// A real cut cropped to its first `rows` velocity radials: what a live cut
    /// looks like while the rest of the chunk is still on the wire.
    fn truncated_velocity_cut(cut: &ElevationCut, rows: usize) -> ElevationCut {
        let grid = velocity_grid_of(cut);
        let rows = rows.min(grid.radial_count());
        let gate_count = grid.gate_range.gate_count;

        let mut partial_grid = grid.clone();
        partial_grid.radial_indices.truncate(rows);
        match &mut partial_grid.storage {
            MomentStorage::U8(values) => values.truncate(rows * gate_count),
            MomentStorage::U16(values) => values.truncate(rows * gate_count),
            MomentStorage::F32(values) => values.truncate(rows * gate_count),
        }

        let kept = partial_grid
            .radial_indices
            .iter()
            .copied()
            .max()
            .map_or(0, |index| index + 1);
        let mut partial = ElevationCut::new(cut.elevation_deg, cut.elevation_number);
        partial.radials = cut.radials[..kept].to_vec();
        partial.moments.insert(MomentType::Velocity, partial_grid);
        partial
    }

    /// A partial cut of `arc_deg` degrees whose first radial is the one nearest
    /// `from_deg`, in the shape of the real KTLX 2026-08-17 07:24:02 partial the
    /// module documents: 240 of 360 radials running from 197.5 degrees up
    /// through 360/0 and ending at 76.5, with the hole straddling the seam.
    ///
    /// A window rather than a truncation, because every cut in an archived
    /// volume starts near the same azimuth - 26 degrees in the KUEX file - so a
    /// truncation of one of them only reaches the seam when it is 334 degrees
    /// long and all but complete. The radials, gates, Nyquist and folding inside
    /// the window are the file's own; only where the sweep is declared to have
    /// begun is chosen, which is the one thing that varies between volumes
    /// anyway.
    fn seam_crossing_partial(cut: &ElevationCut, from_deg: f32, arc_deg: f32) -> ElevationCut {
        let grid = velocity_grid_of(cut);
        let azimuth = |row: usize| cut.radials[grid.radial_indices[row]].azimuth_deg;
        let first_row = (0..grid.radial_count())
            .min_by(|left, right| {
                let distance = |row: usize| {
                    let delta = clockwise_delta_deg(from_deg, azimuth(row));
                    delta.min(360.0 - delta)
                };
                distance(*left).total_cmp(&distance(*right))
            })
            .expect("the cut has rows");
        let rows = (first_row..grid.radial_count())
            .take_while(|row| clockwise_delta_deg(azimuth(first_row), azimuth(*row)) <= arc_deg)
            .collect::<Vec<_>>();

        fn window<T: Copy>(values: &[T], rows: &[usize], gate_count: usize) -> Vec<T> {
            rows.iter()
                .flat_map(|row| {
                    values[row * gate_count..(row + 1) * gate_count]
                        .iter()
                        .copied()
                })
                .collect()
        }

        let gate_count = grid.gate_range.gate_count;
        let mut partial_grid = grid.clone();
        partial_grid.radial_indices = rows.iter().map(|row| grid.radial_indices[*row]).collect();
        partial_grid.storage = match &grid.storage {
            MomentStorage::U8(values) => MomentStorage::U8(window(values, &rows, gate_count)),
            MomentStorage::U16(values) => MomentStorage::U16(window(values, &rows, gate_count)),
            MomentStorage::F32(values) => MomentStorage::F32(window(values, &rows, gate_count)),
        };

        // The radials come across whole: nothing reads them except through the
        // grid's own `radial_indices`, and keeping them keeps every azimuth the
        // file recorded exactly where the file recorded it.
        let mut partial = ElevationCut::new(cut.elevation_deg, cut.elevation_number);
        partial.radials = cut.radials.clone();
        partial.moments.insert(MomentType::Velocity, partial_grid);
        partial
    }

    /// The arc a partial cut has swept: its first radial's azimuth, and the
    /// degrees clockwise from there to its newest. Wrap-aware, because a real
    /// cut is under no obligation to start where the arithmetic is easy.
    fn swept_arc_deg(partial: &ElevationCut) -> (f32, f32) {
        let grid = velocity_grid_of(partial);
        let azimuth = |row: usize| partial.radials[grid.radial_indices[row]].azimuth_deg;
        let start_deg = azimuth(0);
        (
            start_deg,
            clockwise_delta_deg(start_deg, azimuth(grid.radial_count() - 1)),
        )
    }

    fn render_blend_with(blend: &SweepBlend<'_>, raster_options: ViewportRasterOptions) -> Vec<u8> {
        let mut rgba = vec![0; viewport_rgba_buffer_len(raster_options)];
        render_sweep_blend_rgba_into(blend, raster_options, &ColorTableSet::default(), &mut rgba)
            .expect("blend renders");
        rgba
    }

    fn render_storm_relative_with(
        blend: &SweepBlend<'_>,
        storm_motion: StormMotion,
        raster_options: ViewportRasterOptions,
    ) -> Vec<u8> {
        let mut rgba = vec![0; viewport_rgba_buffer_len(raster_options)];
        render_storm_relative_sweep_blend_rgba_into(
            blend,
            storm_motion,
            raster_options,
            &ColorTableSet::default(),
            &mut rgba,
        )
        .expect("blend renders");
        rgba
    }

    /// The ordinary raster of one grid, under the grid's own moment.
    fn normal_render_of(
        cut: &ElevationCut,
        grid: &MomentGrid,
        raster_options: ViewportRasterOptions,
    ) -> Vec<u8> {
        let volume = one_cut_volume(cut, grid);
        let mut pixels = vec![0; viewport_rgba_buffer_len(raster_options)];
        render_moment_viewport_rgba_into(
            &volume,
            0,
            grid.moment.clone(),
            raster_options,
            &mut pixels,
        )
        .expect("normal viewport render");
        pixels
    }

    /// Pixel indices the reference paints and the candidate leaves transparent:
    /// the blank wedge, counted rather than described.
    fn blanked_pixels(reference: &[u8], candidate: &[u8]) -> Vec<usize> {
        reference
            .chunks_exact(4)
            .zip(candidate.chunks_exact(4))
            .enumerate()
            .filter(|(_, (reference, candidate))| reference[3] != 0 && candidate[3] == 0)
            .map(|(index, _)| index)
            .collect()
    }

    /// How many pixels a raster actually paints. How much echo a sweep puts on
    /// screen is the cache's business -- a 15 deg tilt reaches 60 km where a
    /// 0.5 deg tilt reaches 460 km -- so shares of THIS are what the tests
    /// below assert on, never absolute pixel counts.
    fn painted_pixels(pixels: &[u8]) -> usize {
        pixels.chunks_exact(4).filter(|pixel| pixel[3] != 0).count()
    }

    fn differing_pixels(left: &[u8], right: &[u8]) -> Vec<usize> {
        left.chunks_exact(4)
            .zip(right.chunks_exact(4))
            .enumerate()
            .filter(|(_, (left, right))| left != right)
            .map(|(index, _)| index)
            .collect()
    }

    /// How far inside the revealed arc a pixel sits, in degrees, measured to the
    /// nearer edge. Negative outside it. Wrap-aware throughout.
    fn degrees_inside_arc(
        raster_options: ViewportRasterOptions,
        index: usize,
        start_deg: f32,
        revealed_deg: f32,
    ) -> f32 {
        let x = index as u32 % raster_options.width;
        let y = index as u32 / raster_options.width;
        let offset = clockwise_delta_deg(start_deg, pixel_azimuth_deg(raster_options, x, y));
        if offset < revealed_deg {
            offset.min(revealed_deg - offset)
        } else {
            -(offset - revealed_deg).min(360.0 - offset)
        }
    }

    #[test]
    fn a_real_partial_velocity_sweep_blends_without_blanking_the_storm() {
        let Some(volume) = real_velocity_volume() else {
            eprintln!("no cached Level II volume carries velocity; skipping");
            return;
        };
        let cut_index = real_velocity_cut_index(volume);
        let cut = &volume.cuts[cut_index];
        let grid = velocity_grid_of(cut);
        let cut_start_deg = swept_arc_deg(cut).0;
        println!(
            "{} cut {cut_index} {:.2} deg: {} radials from {cut_start_deg:.1} deg",
            volume.site.id,
            cut.elevation_deg,
            grid.radial_count(),
        );

        // Two shapes of partial: one caught mid sweep, and one that has just
        // carried the reveal past the 0/360 seam - the case where subtracting
        // azimuths without wrapping paints backwards across the storm.
        let partials = [
            (
                "mid sweep",
                truncated_velocity_cut(cut, grid.radial_count() * 2 / 3),
            ),
            ("across the seam", seam_crossing_partial(cut, 197.5, 239.0)),
        ];

        let complete = normal_render_of(cut, grid, real_options());
        for (label, partial) in &partials {
            let partial_grid = velocity_grid_of(partial);
            let (start_deg, revealed_deg) = swept_arc_deg(partial);
            let wraps = start_deg + revealed_deg > 360.0;
            println!(
                "{label}: {} radials, arc {start_deg:.1} -> {:.1} ({revealed_deg:.1} deg swept, wraps {wraps})",
                partial_grid.radial_count(),
                (start_deg + revealed_deg).rem_euclid(360.0),
            );

            let alone = normal_render_of(partial, partial_grid, real_options());
            let blended = render_blend_with(
                &SweepBlend {
                    incoming: partial,
                    incoming_grid: partial_grid,
                    previous: Some((cut, grid)),
                    start_deg,
                    revealed_deg,
                },
                real_options(),
            );

            // A cut that starts at 26 degrees only reaches the seam 334 degrees
            // in, so the seam-crossing partial is necessarily nearly complete
            // and its hole is a narrow wedge rather than a third of the scope.
            // The ratio test below is what carries the weight either way.
            let hole = blanked_pixels(&complete, &alone);
            assert!(
                hole.len() > 1_000,
                "{label}: the truncation left only {} blank pixels, which proves nothing",
                hole.len()
            );

            let blanks = blanked_pixels(&complete, &blended);
            let deepest = blanks
                .iter()
                .map(|index| degrees_inside_arc(real_options(), *index, start_deg, revealed_deg))
                .fold(f32::NEG_INFINITY, f32::max);
            println!(
                "{label}: unblended hole {} px, blended blanks {} px, deepest {deepest:.3} deg",
                hole.len(),
                blanks.len()
            );
            assert!(
                blanks.len() * 200 < hole.len(),
                "{label}: {} of the {} blanked pixels survived the blend",
                blanks.len(),
                hole.len()
            );
            assert!(
                !deepest.is_finite() || deepest < 1.0,
                "{label}: a blank {deepest:.3} deg inside the revealed arc is a hole, not a seam artefact"
            );
        }

        assert!(
            partials.iter().any(|(_, partial)| {
                swept_arc_deg(partial).0 + swept_arc_deg(partial).1 > 360.0
            }),
            "neither partial crossed the seam, so the wrap case went untested"
        );
    }

    #[test]
    fn two_real_independent_unfoldings_differ_only_by_whole_nyquist_intervals() {
        // This is the whole honest claim behind blending dealiased velocity.
        // Both runs start from the same folded observations and every correction
        // either of them applies is a whole number of Nyquist intervals, so the
        // most the reveal boundary can show is a jump of exactly 2 * nyquist.
        // Anything else appearing there would be an unfolding that had invented
        // a velocity.
        let Some((volume, cut_index)) = real_folded_velocity_cut() else {
            eprintln!("no cached Level II volume carries folded velocity; skipping");
            return;
        };
        let cut = &volume.cuts[cut_index];
        let grid = velocity_grid_of(cut);
        let gate_count = grid.gate_range.gate_count;
        let complete_unfolded = dealias_cut_velocity(cut).expect("velocity unfolds");

        let mut total_compared = 0_usize;
        let mut total_differing = 0_usize;
        let mut worst_residual = 0.0_f32;
        for (numerator, denominator) in [(1, 3), (1, 2), (2, 3), (5, 6)] {
            let partial =
                truncated_velocity_cut(cut, grid.radial_count() * numerator / denominator);
            let partial_unfolded = dealias_cut_velocity(&partial).expect("velocity unfolds");
            let rows = partial_unfolded.radial_count();

            let mut compared = 0_usize;
            let mut differing = 0_usize;
            let mut last_sixth = 0_usize;
            let mut interval = 0.0_f32;
            for row in 0..rows {
                let Some(nyquist) = row_nyquist_mps(cut, grid, row).filter(|value| *value > 0.0)
                else {
                    continue;
                };
                interval = 2.0 * nyquist;
                for gate in 0..gate_count {
                    let (Some(from_partial), Some(from_complete)) = (
                        partial_unfolded.scaled_value(row, gate),
                        complete_unfolded.scaled_value(row, gate),
                    ) else {
                        continue;
                    };
                    compared += 1;
                    let folds = (from_partial - from_complete) / interval;
                    worst_residual = worst_residual.max((folds - folds.round()).abs());
                    if folds.round() != 0.0 {
                        differing += 1;
                        if row * 6 >= rows * 5 {
                            last_sixth += 1;
                        }
                    }
                }
            }

            println!(
                "{numerator}/{denominator} of the sweep ({rows} radials): compared {compared} gates, {differing} differ ({:.3}%), {last_sixth} of those in its last sixth, interval {interval:.1} m/s",
                100.0 * differing as f32 / compared.max(1) as f32,
            );
            total_compared += compared;
            total_differing += differing;
        }

        println!(
            "all truncations: {total_differing} of {total_compared} gates differ ({:.3}%), worst residual {worst_residual:.4} of an interval",
            100.0 * total_differing as f32 / total_compared.max(1) as f32,
        );
        assert!(
            total_compared > 20_000,
            "only {total_compared} real gates compared, which is not enough to conclude from"
        );
        assert!(
            worst_residual < 0.01,
            "two unfoldings differ by {worst_residual:.4} of a Nyquist interval somewhere, which neither of them can produce: one of them is not unfolding the observations it was given"
        );
    }

    #[test]
    fn a_real_dealiased_partial_sweep_blends_without_blanking_the_storm() {
        let Some((volume, cut_index)) = real_folded_velocity_cut() else {
            eprintln!("no cached Level II volume carries folded velocity; skipping");
            return;
        };
        let cut = &volume.cuts[cut_index];
        let grid = velocity_grid_of(cut);
        let partial = truncated_velocity_cut(cut, grid.radial_count() * 2 / 3);
        let (start_deg, revealed_deg) = swept_arc_deg(&partial);

        // What unfolding both layers instead of one actually costs, so the
        // caller can decide whether to keep the previous sweep's grid across the
        // frames of a tilt rather than guessing.
        let unfold_started = std::time::Instant::now();
        let dealiased = DealiasedSweepBlend::new(&partial, Some(cut), start_deg, revealed_deg)
            .expect("the arriving cut has velocity");
        let unfold_ms = unfold_started.elapsed().as_secs_f32() * 1_000.0;
        let render_started = std::time::Instant::now();
        let blended = render_blend_with(&dealiased.blend(), real_options());
        let raster_ms = render_started.elapsed().as_secs_f32() * 1_000.0;
        // The storm-relative arm is timed separately because it costs more than
        // the plain one and there is no palette cache behind this entry point:
        // it builds one 1 KiB palette per radial per LAYER, every frame, where
        // `ViewportMomentCache` keeps a `StormRelativePaletteCache` across them.
        let storm_relative_started = std::time::Instant::now();
        let storm_relative =
            render_storm_relative_with(&dealiased.blend(), real_storm_motion(), real_options());
        println!(
            "cut {cut_index}: unfolding both layers {unfold_ms:.1} ms, blend raster {raster_ms:.1} ms, storm-relative blend raster {:.1} ms",
            storm_relative_started.elapsed().as_secs_f32() * 1_000.0,
        );

        let complete_unfolded = dealias_cut_velocity(cut).expect("velocity unfolds");
        let reference = normal_render_of(cut, &complete_unfolded, real_options());
        let storm_relative_reference = normal_storm_relative_render(
            cut,
            &complete_unfolded,
            real_storm_motion(),
            real_options(),
        );

        for (label, candidate, reference) in [
            ("dealiased", &blended, &reference),
            (
                "dealiased storm relative",
                &storm_relative,
                &storm_relative_reference,
            ),
        ] {
            let blanks = blanked_pixels(reference, candidate);
            let deepest = blanks
                .iter()
                .map(|index| degrees_inside_arc(real_options(), *index, start_deg, revealed_deg))
                .fold(f32::NEG_INFINITY, f32::max);
            let differing = differing_pixels(reference, candidate);
            println!(
                "{label}: {} blanks (deepest {deepest:.3} deg), {} px differ from the complete unfolding ({:.2}%)",
                blanks.len(),
                differing.len(),
                100.0 * differing.len() as f32
                    / (real_options().width * real_options().height) as f32,
            );
            assert!(
                blanks.len() < 500,
                "{label} left {} pixels blank that the complete sweep painted",
                blanks.len()
            );
            assert!(
                !deepest.is_finite() || deepest < 1.0,
                "{label} left a blank {deepest:.3} deg inside the revealed arc"
            );
        }
    }

    #[test]
    fn a_real_storm_relative_blend_keeps_both_halves_in_one_reference_frame() {
        // One real sweep on both layers: the lookups are then identical, so the
        // only thing that can differ across the reveal boundary is the shading.
        // Byte identity with the ordinary storm-relative raster is therefore a
        // direct measurement that both halves are in the same reference frame,
        // on real beam azimuths rather than on a ring of tidy integers.
        let Some(volume) = real_velocity_volume() else {
            eprintln!("no cached Level II volume carries velocity; skipping");
            return;
        };
        let lowest = &volume.cuts[real_velocity_cut_index(volume)];
        let folded = real_folded_velocity_cut().map(|(volume, index)| {
            let cut = &volume.cuts[index];
            (cut, dealias_cut_velocity(cut).expect("velocity unfolds"))
        });

        let mut layers = vec![("velocity", lowest, velocity_grid_of(lowest))];
        match &folded {
            Some((cut, unfolded)) => layers.push(("dealiased velocity", cut, unfolded)),
            None => eprintln!("no cached volume carries folded velocity; skipping that half"),
        }

        for (label, cut, grid) in layers {
            let start_deg = swept_arc_deg(cut).0;
            let blend = SweepBlend {
                incoming: cut,
                incoming_grid: grid,
                previous: Some((cut, grid)),
                start_deg,
                revealed_deg: 137.3,
            };
            let blended = render_storm_relative_with(&blend, real_storm_motion(), real_options());
            let reference =
                normal_storm_relative_render(cut, grid, real_storm_motion(), real_options());
            assert_eq!(
                first_difference(&blended, &reference),
                None,
                "{label}: the reveal boundary moved the reference frame"
            );

            let ground_relative = render_blend_with(&blend, real_options());
            let painted = painted_pixels(&ground_relative);
            let moved = differing_pixels(&blended, &ground_relative);
            let share = moved.len() as f32 / painted.max(1) as f32;
            println!(
                "{label}: {painted} px of echo, storm motion changed {} of them ({:.1}% of the \
                 echo, {:.1}% of the frame)",
                moved.len(),
                100.0 * share,
                100.0 * moved.len() as f32 / (real_options().width * real_options().height) as f32
            );
            // Measured as a share of the echo, not as a count of pixels: the
            // dealiased layer is whichever cut folds hardest, which on a day
            // of high-tilt folding is a 15 deg sweep painting a few thousand
            // pixels where the 0.5 deg sweep paints forty thousand. A count
            // would only be measuring which tilt the cache happened to offer;
            // the share is a property of the shading, and it is near total
            // (measured 93.5 % and 100.0 % on KAKQ 2026-08-20 18:22 UTC).
            if painted < 1_000 {
                eprintln!(
                    "{label}: only {painted} px of echo on screen, too little to conclude from; \
                     skipping the storm-motion half of this layer"
                );
                continue;
            }
            assert!(
                share > 0.5,
                "{label}: a 20 m/s storm motion changed {} of the {painted} pixels this sweep \
                 paints ({:.1}%) -- a layer still in the ground-relative frame looks like this",
                moved.len(),
                100.0 * share
            );
        }
    }

    #[test]
    fn real_fully_revealed_blends_are_byte_identical_to_the_ordinary_rasters() {
        let Some((volume, cut_index)) = real_folded_velocity_cut() else {
            eprintln!("no cached Level II volume carries folded velocity; skipping");
            return;
        };
        let cut = &volume.cuts[cut_index];
        let grid = velocity_grid_of(cut);
        let (start_deg, _) = swept_arc_deg(cut);
        let unfolded = dealias_cut_velocity(cut).expect("velocity unfolds");
        let previous = truncated_velocity_cut(cut, grid.radial_count() / 2);
        let previous_unfolded = dealias_cut_velocity(&previous).expect("velocity unfolds");

        // Something different underneath, so a full reveal that let any of it
        // through would show up rather than coincide.
        let folded = SweepBlend {
            incoming: cut,
            incoming_grid: grid,
            previous: Some((&previous, velocity_grid_of(&previous))),
            start_deg,
            revealed_deg: 360.0,
        };
        let unfolded_blend = SweepBlend {
            incoming: cut,
            incoming_grid: &unfolded,
            previous: Some((&previous, &previous_unfolded)),
            start_deg,
            revealed_deg: 360.0,
        };

        assert_eq!(
            first_difference(
                &render_blend_with(&folded, real_options()),
                &normal_render_of(cut, grid, real_options())
            ),
            None,
            "velocity"
        );
        assert_eq!(
            first_difference(
                &render_blend_with(&unfolded_blend, real_options()),
                &normal_render_of(cut, &unfolded, real_options())
            ),
            None,
            "dealiased velocity"
        );
        assert_eq!(
            first_difference(
                &render_storm_relative_with(&folded, real_storm_motion(), real_options()),
                &normal_storm_relative_render(cut, grid, real_storm_motion(), real_options())
            ),
            None,
            "storm relative velocity"
        );
        assert_eq!(
            first_difference(
                &render_storm_relative_with(&unfolded_blend, real_storm_motion(), real_options()),
                &normal_storm_relative_render(cut, &unfolded, real_storm_motion(), real_options())
            ),
            None,
            "dealiased storm relative velocity"
        );
    }
}
