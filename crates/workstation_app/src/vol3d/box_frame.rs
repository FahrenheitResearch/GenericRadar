//! How big the 3D box is, and where it sits.
//!
//! Two decisions, together because they are not separable in practice. The box
//! side divided by the fixed `BOX_N` lattice IS the voxel size, so a smaller box
//! is a sharper one - but only if the box is over the storm. A 60 km box nailed
//! to the radar, which is where the box used to be nailed, misses a storm 80 km
//! away completely, so shrinking the default without [`auto_box_center_km`]
//! would have made the pane worse rather than better. Measured on the
//! workstation's own Level II cache, the old radar-centred 120 km box held no
//! 50 dBZ core at all on 25 of 48 volumes: the shrink is only defensible
//! because the box now goes to the storm.
//!
//! Every number here comes from
//! [`tests::the_default_box_frames_the_storm_measured_on_real_volumes`]: point
//! that `#[ignore]`d test at a directory of Level II files and it reprints them.
//!
//! Split out of `vol3d.rs` because that file is at the architecture test's
//! 2000-line module limit, not because the two are independent.

// Same reason as the parent module: this is a binary crate, so `pub` buys
// nothing from the `dead_code` lint, and the pane wires up only part of this
// surface at a time.
#![allow(dead_code)]

use radar_core::{MomentType, RadarVolume};
use rayon::prelude::*;
use std::sync::Arc;

use super::{BOX_N, Vol3d};

/// Box side lengths offered to the operator, kilometres ACROSS.
///
/// The list runs DOWN as well as up because `BOX_N` is fixed at 192: the side
/// length divided by 192 IS the voxel edge, so choosing a box size is also
/// choosing a resolution.
///
/// | side   | voxel  |
/// |--------|--------|
/// | 30 km  | 156 m  |
/// | 60 km  | 312 m  |
/// | 120 km | 625 m  |
/// | 240 km | 1250 m |
/// | 360 km | 1875 m |
pub const BOX_SIZE_CHOICES_KM: [f32; 5] = [30.0, 60.0, 120.0, 240.0, 360.0];

/// Default box half-width, km - a 60 km box, 312 m per voxel.
///
/// Two measurements pick this, not taste.
///
/// RESOLUTION. `BOX_N` is a fixed 192 lattice, so the old 120 km default spent
/// 192 x 192 x 48 cells on 625 m voxels of mostly empty air. A super-resolution
/// NEXRAD reflectivity gate is 250 m long and 0.5 deg wide, and the width grows
/// with range: 524 m of arc at 60 km, 1.3 km at 150 km. A 312 m voxel therefore
/// sits between the two - fine enough never to throw away the along-range detail
/// the radar collected, coarse enough not to pretend to cross-range detail it
/// never had. The 30 km / 156 m option below it is finer than a gate in every
/// direction: interpolation, with nothing new in it.
///
/// FRAMING. A supercell is 20-40 km across and its forward-flank shield and
/// anvil reach past that, so a 60 km box holds one storm plus its immediate
/// inflow - the thing people open the 3D pane to look at - and holds it for
/// several volumes, since at 20 m/s a cell moves about 6 km per 5 minute scan.
///
/// Shrinking the default is only safe BECAUSE of the auto centre. Nailed to the
/// radar as it used to be, a 60 km box would miss a storm 80 km away entirely.
/// [`tests::the_default_box_frames_the_storm_measured_on_real_volumes`]
/// reproduces the measurement over the workstation's own Level II cache; run
/// over 55 volumes from 55 different sites, 2026-06-08/09, at the old default
/// (radar centre, 120 km box) and the new one (auto centre, 60 km box):
///
/// ```text
///  volume                     filled   >=35 dBZ   >=35 dBZ    echo top
///                                       of box     volume
///  KTWX 2026-06-09 01:39Z
///    old  radar, 120 km        22.6%     0.00%        0 km3     11.5 km
///    new  storm,  60 km        77.1%    37.52%    24310 km3     18.0 km
///  KTLH 2026-06-09 00:04Z
///    old  radar, 120 km         3.5%     0.00%        3 km3      3.1 km
///    new  storm,  60 km        29.7%     0.92%      599 km3     11.5 km
///  KRLX 2026-06-09 18:07Z
///    old  radar, 120 km        12.2%     1.64%     4259 km3      8.0 km
///    new  storm,  60 km        39.0%     1.72%     1114 km3      9.2 km
/// ```
///
/// Read all three columns, because two of them disagree. Over the 48 framed
/// volumes the box's >= 35 dBZ VOXEL share goes from 0.381% to 5.590%, a factor
/// of 14.7 - but a 60 km box divides the same 192 lattice into 312 m voxels
/// against the 120 km box's 625 m, so four of that fourteen is the box being
/// finer rather than fuller. The honest figure is the PHYSICAL volume of
/// >= 35 dBZ air in the box: 987 km3 to 3622 km3, a factor of 3.7.
///
/// KTLH is the case the default is for: the old box was not merely emptier, it
/// held nothing - 0% core, a 3.1 km top - because the only convection was 200 km
/// away. KRLX is the case against it, and it is real: the new box is denser but
/// holds a QUARTER of the core volume the old one did, because that day's echo
/// was a 300 km line and no 60 km box can hold a 300 km line. Seven of the 52
/// volumes lose core volume this way, and on a day that is nothing but squall
/// line the aggregate shrinks with them - over 139 consecutive volumes from
/// eight sites the ratio is 1.6, not 3.7.
///
/// What the shrink costs, stated plainly. On those 139 volumes the old box held
/// 17.4% of the volume's >= 35 dBZ field and the new one holds 16.4%: one point
/// less. What it buys is that the box contains the STORM. The old box held no
/// 50 dBZ core at all on 25 of them and the new box on 1, and the strongest echo
/// inside it is 3.7 dB below the volume's peak rather than 8.7 dB below. On the
/// wider 55-site sample, where the storms are not usually on top of the radar,
/// it is 38.7% of the field against 10.8%, and 2.0 dB against 18.9 dB.
pub const BOX_HALF_KM: f32 = 30.0;

/// Voxel edge in metres for a box of this half-width. The lattice is fixed, so
/// this is the whole resolution story: `2 * half_km / BOX_N`.
pub fn box_voxel_m(half_km: f32) -> f32 {
    2_000.0 * half_km / BOX_N as f32
}

/// Horizontal reach of the auto-centre search, km.
///
/// NEXRAD collects reflectivity to 460 km, but the box has to sit where the
/// volume is deep enough to render: past about 230 km even the 0.5 deg beam is
/// above 4 km AGL, so a storm out there has no low levels left to show and
/// centring on it would frame an anvil.
pub const AUTO_CENTER_RANGE_KM: f32 = 230.0;

/// Cell edge of the composite the auto centre is picked from, km. Small against
/// the 20-40 km storm being framed, large enough that one hot gate cannot move
/// the box on its own.
pub const AUTO_CENTER_CELL_KM: f32 = 2.0;

/// Cells per side of that composite. Pinned rather than computed so the raster
/// dimensions are a compile-time constant;
/// [`tests::auto_center_grid_covers_exactly_the_search_range`] keeps it
/// consistent with the two constants above.
pub const AUTO_CENTER_N: usize = 230;

/// Lowest beam height the auto centre will look at, km above the radar.
///
/// Not a nicety: ground clutter and anomalous propagation are 40-70 dBZ, they
/// sit in the lowest tilt within a few tens of km of the radar, and they are
/// therefore exactly what a reflectivity-seeking centre finds first. Measured
/// over 48 framed volumes of the workstation's own cache, turning the gate off
/// moves the centre 24.6 km on average and more than 30 km on 8 of them. The
/// worst is KDDC 2026-06-08 14:04Z: gated, the box sits on the convection at
/// (68.7, -156.9), 172 km south-east; ungated it collapses to (-18.2, -3.9),
/// which is 19 km from the radar and is ground clutter.
pub const AUTO_CENTER_MIN_HEIGHT_KM: f32 = 2.0;

/// Reflectivity that counts as convective core when choosing a centre, dBZ.
///
/// 35 dBZ is the conventional convective/stratiform split (Steiner, Houze and
/// Yuter 1995, *J. Appl. Meteor.* 34(9), 1978-2007), and it is also the volume
/// explorer's own default display threshold, so the box frames what the pane is
/// about to draw rather than something adjacent to it.
pub const AUTO_CENTER_CORE_DBZ: f32 = 35.0;

/// Fallback threshold, dBZ, for volumes with no convection in them at all.
/// Stratiform rain and snow still have structure worth framing; they simply do
/// not reach 35 dBZ.
pub const AUTO_CENTER_ECHO_DBZ: f32 = 20.0;

/// How much one dB above [`ClusterRule::min_dbz`] adds to a cell's weight, as a
/// fraction of the weight the threshold itself is worth.
///
/// Counting cells and ignoring how strong they are picks the BROADEST echo, not
/// the strongest, and it ties constantly: over a squall line hundreds of
/// candidate windows hold the same number of cells, so the winner is decided by
/// raster order, and one cell crossing the threshold moves the box the length of
/// the line. Over 139 CONSECUTIVE volumes from eight sites in the workstation's
/// own cache (KARX, KEAX, KDVN, KTWX, KILX, KLSX, KVWX, 2026-06-08/09) - the
/// `weight/dB` rows the measurement test reprints:
///
/// ```text
///                 mean dB below     volume-to-volume jump
///                 the volume peak   median    p90    over 30 km
///  0.00 (count)      5.53 dB        5.6 km  42.3 km  17 of 132
///  0.10              3.71 dB        5.7 km  15.2 km  10 of 132
///  0.25              3.65 dB        5.3 km  21.5 km  12 of 132
///  0.50              3.63 dB        5.7 km  32.3 km  14 of 132
///  1.00              3.59 dB        5.9 km  33.7 km  14 of 132
/// ```
///
/// A light weight is what breaks the ties: it cuts the number of steps that move
/// the box more than a box half-width from 17 to 10, cuts the 90th-percentile
/// jump from 42 km to 15 km, and cuts the strength the framing misses from
/// 5.5 dB to 3.7 dB, for four tenths of a point of field coverage (16.8% to
/// 16.4%). Heavier weights chase the hottest cell again and the jumps come back.
/// Reflectivity-weighted centroids
/// are how storm-cell trackers have located cells since TITAN (Dixon and Wiener
/// 1993, *J. Atmos. Oceanic Technol.* 10(6), 785-797); this is the same idea
/// with a deliberately shallow weight.
pub const AUTO_CENTER_WEIGHT_PER_DB: f32 = 0.1;

/// Reflectivity above which extra dB buy no extra weight, dBZ.
///
/// Nothing meteorological is brighter than about 75 dBZ, so a cell beyond this
/// is a hail spike, a three-body scatter signature, a test pattern or a decode
/// fault. Saturating rather than trusting it keeps a single absurd value - a
/// corrupt volume can carry 1e30 - from buying unbounded weight and dragging the
/// box onto itself.
pub const AUTO_CENTER_WEIGHT_CEILING_DBZ: f32 = 80.0;

/// Distinct elevation angles that must see echo in a cell before that cell may
/// help choose the centre.
///
/// A convective core has DEPTH: it is seen at several heights, which is the
/// whole reason for looking at it in 3D. The artifacts that most look like a
/// storm to a reflectivity-seeking centre do not. A solar spike - the sun in the
/// sidelobe at sunrise and sunset - is a line of gates along ONE azimuth on the
/// ONE sweep whose elevation matches the sun, and the range-square correction
/// makes it BRIGHTER with range, so at 200 km it reads as 40 dBZ.
/// [`tests::a_solar_spike_cannot_take_the_box_off_a_real_storm`] plants one and
/// it beat a real 50 dBZ storm outright before this rule existed.
///
/// Two is the weakest requirement that means "seen at more than one height", and
/// it is what a volume already refused a 3D box below four tilts can always
/// supply. Repeated cuts of one elevation - SAILS and MRLE cut the 0.5 deg sweep
/// two or three times a volume - count once, which is why [`echo_composite`]
/// groups the cuts by elevation before rasterising them.
///
/// On 139 consecutive WSR-88D volumes the rule changes NOTHING: every volume is
/// still framed, at the same centre. It earns its place on TDWR, where it turns
/// out to describe the instrument - the hazardous-weather scan reaches about
/// 90 km and only the lowest monitor sweep goes further, so past that there is
/// one elevation of data, a shell rather than a volume. Over 87 cached TDWR
/// volumes it pulls every chosen centre inside 85 km, the dual-tilt coverage,
/// and the number whose box holds no 50 dBZ core falls from 11 to 1. It declines
/// to move the box on 27 of them, all cases where the only echo over the
/// threshold was in that single distant sweep, and the box stays on the radar -
/// which for a TDWR is where the weather is.
pub const AUTO_CENTER_MIN_SWEEPS: u8 = 2;

/// Fewest composite cells a cluster must hold before the box is moved onto it.
///
/// 12 cells is 48 km2, roughly 7 km across - smaller than any storm and larger
/// than a speck. The floor is not knife-edge: over 55 cached volumes the number
/// framed goes 53, 50, 49, 48, 46, 46 as the floor goes 1, 4, 8, 12, 24, 48, so
/// the five volumes between a floor of one cell and a floor of twelve are the
/// clear-air scans (KGGW and KLNX 2026-06-08/09 carry 0 and 1 core cells in the
/// whole volume), and nothing with a storm in it is near the edge.
///
/// Below the floor the box does not move at all, which leaves it wherever the
/// operator had it - by default on the radar, showing the whole coverage.
pub const AUTO_CENTER_MIN_CELLS: usize = 12;

/// Footprint the auto centre optimises for, km half-width: the default box.
///
/// Fixed rather than following the currently selected box size, so the centre is
/// one number per volume rather than one per volume and size, and changing box
/// size costs no rescan.
///
/// The obvious objection is that a 360 km box centred 200 km from the radar
/// wastes a third of itself on ground the radar never reached, and that a big
/// box therefore wants the radar. Measured over 48 framed volumes it is not so:
/// the storm centre wins at every offered size, because a radar-centred big box
/// spends its middle on the cone of silence and its edges on nothing. At 60,
/// 120, 240 and 360 km across it holds 237 / 912 / 3903 / 8322 km3 of >= 35 dBZ
/// air centred on the radar, against 3372 / 5931 / 8589 / 10388 km3 centred on
/// the storm. An operator who wants the whole coverage picks
/// [`Vol3dBoxCenter::Radar`], which is what that mode is for.
pub const AUTO_CENTER_WINDOW_HALF_KM: f32 = BOX_HALF_KM;

/// 4/3 effective earth radius, km, for beam height (Doviak and Zrnic 1993,
/// *Doppler Radar and Weather Observations*, 2nd ed., eq. 2.28b).
const EFFECTIVE_EARTH_RADIUS_KM: f32 = 8_494.67;

/// Beam centre height above the radar, km.
fn beam_height_km(slant_km: f32, elevation_deg: f32) -> f32 {
    let sin_elevation = elevation_deg.to_radians().sin();
    (slant_km * slant_km
        + EFFECTIVE_EARTH_RADIUS_KM * EFFECTIVE_EARTH_RADIUS_KM
        + 2.0 * slant_km * EFFECTIVE_EARTH_RADIUS_KM * sin_elevation)
        .sqrt()
        - EFFECTIVE_EARTH_RADIUS_KM
}

/// Where the 3D box sits horizontally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dBoxCenter {
    /// On the radar, as the box always used to be. Right for a whole-coverage
    /// look; useless for framing one storm at 80 km.
    Radar,
    /// On the volume's own convective cluster, re-measured once per volume so
    /// the box follows the storm as it moves. The default, and the coin flip it
    /// replaces is literal: the old radar-centred box held no 50 dBZ core on 25
    /// of 48 cached volumes, and this one on 4.
    Storm,
    /// Pinned to a point the operator chose. Kept across volumes, so a box put
    /// on an outflow boundary stays there while the cores move through it.
    Fixed,
}

impl Vol3dBoxCenter {
    pub const ALL: [Self; 3] = [Self::Storm, Self::Radar, Self::Fixed];

    pub fn label(self) -> &'static str {
        match self {
            Self::Radar => "Radar",
            Self::Storm => "Follow storm",
            Self::Fixed => "Pinned",
        }
    }
}

/// Identity of the volume an auto centre was measured from: site, volume time
/// and the source pointer, matching the first three fields of
/// [`super::Vol3dVolumeKey`]. A live volume that grows another tilt is a new
/// pointer, so the storm centre is re-measured as the scan fills in.
pub type Vol3dCenterKey = (String, i64, usize);

/// Box centre quantised for the resample key, in tenths of a kilometre.
///
/// 100 m is finer than a voxel at every offered box size - the smallest is
/// 156 m - so any centre move that could change a single voxel changes the key
/// and rebuilds the box, and a centre that has not moved cannot.
pub fn box_center_key(east_km: f32, north_km: f32) -> (i32, i32) {
    (
        (east_km * 10.0).round() as i32,
        (north_km * 10.0).round() as i32,
    )
}

impl Vol3d {
    /// Box centre for `volume`, km east and north of the radar.
    ///
    /// Cheap on every frame but the first of a new volume: the gate scan behind
    /// [`auto_box_center_km`] walks every gate of every tilt, tens of millions
    /// of them, so it is cached against the volume identity and never runs twice
    /// for the same volume. Timed in a release build over 55 cached volumes
    /// (`tests::explore_scan_cost` during the verification pass) it is 8.1 ms on
    /// average and 15.3 ms at worst, which is one dropped frame when a volume
    /// arrives and nothing at all afterwards. It is NOT cheap enough to run per
    /// frame, which is what the cache is for.
    pub fn resolve_box_center(&mut self, volume: &Arc<RadarVolume>) -> (f32, f32) {
        match self.box_center_mode {
            Vol3dBoxCenter::Radar => {
                self.box_center_east_km = 0.0;
                self.box_center_north_km = 0.0;
                // Dropping the cache here is what makes switching BACK to
                // Storm work on the SAME volume. The cache is keyed on the
                // volume, not on the mode, so without this the box would still
                // be sitting on the radar with the key saying "already
                // measured", and Follow Storm would do nothing until the next
                // volume arrived - five minutes of a mode that looks broken.
                self.auto_center_key = None;
            }
            // The operator placed it. Nothing here may move it.
            Vol3dBoxCenter::Fixed => {}
            Vol3dBoxCenter::Storm => {
                let key: Vol3dCenterKey = (
                    volume.site.id.clone(),
                    volume.volume_time.timestamp_millis(),
                    Arc::as_ptr(volume) as usize,
                );
                if self.auto_center_key.as_ref() != Some(&key) {
                    // A volume with nothing worth framing leaves the box where
                    // it was rather than chasing a speck: one weak scan in a
                    // loop must not throw away the framing an operator is in
                    // the middle of reading.
                    if let Some((east_km, north_km)) = auto_box_center_km(volume) {
                        self.box_center_east_km = east_km;
                        self.box_center_north_km = north_km;
                    }
                    self.auto_center_key = Some(key);
                }
            }
        }
        (self.box_center_east_km, self.box_center_north_km)
    }

    /// Move the box to a point the operator picked and pin it there.
    ///
    /// A non-finite point is refused rather than stored: it would travel into
    /// the resample key, the box origin and the floor raster, none of which fail
    /// loudly on a NaN - they just build an empty box - and the projection that
    /// produces a click position can hand back a NaN for a degenerate camera.
    pub fn pin_box_center(&mut self, east_km: f32, north_km: f32) {
        if !east_km.is_finite() || !north_km.is_finite() {
            return;
        }
        self.box_center_mode = Vol3dBoxCenter::Fixed;
        self.box_center_east_km = east_km;
        self.box_center_north_km = north_km;
        // Same reason as the Radar arm of `resolve_box_center`: this overwrote
        // the measured centre, so the measurement no longer describes where the
        // box is and must not be reused if the operator hands the box back.
        self.auto_center_key = None;
    }

    /// Hand the box back to the storm tracker, re-measuring on the next volume.
    pub fn follow_storm(&mut self) {
        self.box_center_mode = Vol3dBoxCenter::Storm;
        self.auto_center_key = None;
    }
}

/// Column-maximum reflectivity on an equal-area horizontal raster centred on
/// the radar, `AUTO_CENTER_N` cells of `AUTO_CENTER_CELL_KM` per side.
///
/// Rasterising BEFORE choosing a centre is the whole trick. A polar gate covers
/// `r * dAz * dr` of ground, so gates at 20 km sit about ten times more densely
/// per unit area than gates at 200 km, and any peak or centroid taken over gates
/// directly is dragged toward the radar by that geometry alone. One value per
/// equal-area cell removes the bias without any weighting fudge.
///
/// `max_dbz` is `f32::NEG_INFINITY` in cells no gate reached.
pub struct EchoComposite {
    pub cell_km: f32,
    pub n: usize,
    pub max_dbz: Vec<f32>,
    /// How many DISTINCT elevation angles put at least [`AUTO_CENTER_ECHO_DBZ`]
    /// into this cell: the cell's echo depth, in sweeps. One means the echo was
    /// seen at exactly one height, which is what a solar spike, a sidelobe and a
    /// single-tilt artifact look like and what a storm does not. See
    /// [`AUTO_CENTER_MIN_SWEEPS`].
    pub sweeps: Vec<u8>,
}

impl EchoComposite {
    /// Centre of cell `index`, km east and north of the radar.
    pub fn cell_center_km(&self, index: usize) -> (f32, f32) {
        let half_span = 0.5 * self.n as f32 * self.cell_km;
        let x = index % self.n;
        let y = index / self.n;
        (
            (x as f32 + 0.5) * self.cell_km - half_span,
            (y as f32 + 0.5) * self.cell_km - half_span,
        )
    }

    /// Cell holding the strongest column in the composite.
    ///
    /// One of the two centres [`auto_box_center_km`] rejects, kept as the
    /// executable form of that rejection: it frames whichever single 2 km cell
    /// happens to be hottest, which on KARX 2026-06-08 17:28Z was 164 km from
    /// the storm an analyst would have chosen.
    pub fn strongest_column_km(&self) -> Option<(f32, f32)> {
        let mut best: Option<(usize, f32)> = None;
        for (index, value) in self.max_dbz.iter().enumerate() {
            if !value.is_finite() {
                continue;
            }
            if best.is_none_or(|(_, best_value)| *value > best_value) {
                best = Some((index, *value));
            }
        }
        best.map(|(index, _)| self.cell_center_km(index))
    }

    /// Equal-area centroid of every cell at or above `min_dbz`.
    ///
    /// The other centre [`auto_box_center_km`] rejects, kept for the same
    /// reason: over a scattered field it lands BETWEEN the storms rather than on
    /// one, and on a field with clutter in it, on the clutter.
    pub fn core_centroid_km(&self, min_dbz: f32) -> Option<(f32, f32)> {
        let mut east_sum = 0.0f64;
        let mut north_sum = 0.0f64;
        let mut count = 0u32;
        for (index, value) in self.max_dbz.iter().enumerate() {
            if !value.is_finite() || *value < min_dbz {
                continue;
            }
            let (east_km, north_km) = self.cell_center_km(index);
            east_sum += f64::from(east_km);
            north_sum += f64::from(north_km);
            count += 1;
        }
        (count > 0).then(|| {
            (
                (east_sum / f64::from(count)) as f32,
                (north_sum / f64::from(count)) as f32,
            )
        })
    }

    /// Number of cells at or above `min_dbz`.
    pub fn core_cells(&self, min_dbz: f32) -> usize {
        self.max_dbz
            .iter()
            .filter(|value| value.is_finite() && **value >= min_dbz)
            .count()
    }

    /// Per-cell weight under `rule`: zero for a cell that does not qualify.
    fn weights(&self, rule: ClusterRule) -> Vec<u32> {
        self.max_dbz
            .iter()
            .zip(&self.sweeps)
            .map(|(value, sweeps)| {
                if *sweeps < rule.min_sweeps.max(1) {
                    return 0;
                }
                rule.weight(*value)
            })
            .collect()
    }

    /// Centre of the strongest cluster: find the `rule.half_km` window with the
    /// most echo weight in it, then return the weighted centroid of the
    /// qualifying cells inside it. `None` if the best window holds fewer than
    /// `rule.min_cells` of them.
    ///
    /// The window is what makes this frame a storm rather than a field - it
    /// picks the concentration a box can actually contain - and the centroid is
    /// what stops the answer depending on which of several equally dense windows
    /// happened to be visited first.
    ///
    /// The weight is integer, not floating point, so that two windows holding
    /// the same echo tie EXACTLY and the tie is broken by raster order rather
    /// than by rounding noise. That is what makes the answer reproducible for a
    /// given volume, which the resample key depends on: an unstable centre would
    /// rebuild the box on every frame.
    pub fn strongest_cluster_km(&self, rule: ClusterRule) -> Option<(f32, f32)> {
        let n = self.n;
        let weights = self.weights(rule);
        // Summed-area table (Crow 1984, *SIGGRAPH* 18(3), 207-212): every
        // window sum is then four lookups, so scanning all 52 900 candidate
        // centres costs one pass rather than 52 900 window sums. u64 because a
        // saturated 52 900-cell raster is about 2.9 million weight units and the
        // table accumulates all of them.
        let mut area = vec![0u64; (n + 1) * (n + 1)];
        for y in 0..n {
            let mut row_sum = 0u64;
            for x in 0..n {
                row_sum += u64::from(weights[y * n + x]);
                area[(y + 1) * (n + 1) + x + 1] = area[y * (n + 1) + x + 1] + row_sum;
            }
        }
        let radius = (rule.half_km / self.cell_km).round() as usize;
        let mut best: Option<(usize, usize, usize, usize, u64)> = None;
        for y in 0..n {
            for x in 0..n {
                let (east_km, north_km) = self.cell_center_km(y * n + x);
                if east_km.hypot(north_km) > rule.max_center_km {
                    continue;
                }
                let x0 = x.saturating_sub(radius);
                let y0 = y.saturating_sub(radius);
                let x1 = (x + radius + 1).min(n);
                let y1 = (y + radius + 1).min(n);
                let weight = area[y1 * (n + 1) + x1] + area[y0 * (n + 1) + x0]
                    - area[y0 * (n + 1) + x1]
                    - area[y1 * (n + 1) + x0];
                if best.is_none_or(|(.., best_weight)| weight > best_weight) {
                    best = Some((x0, y0, x1, y1, weight));
                }
            }
        }
        let (x0, y0, x1, y1, _) = best?;
        let mut east_sum = 0.0f64;
        let mut north_sum = 0.0f64;
        let mut weight_sum = 0.0f64;
        let mut cells = 0usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let weight = f64::from(weights[y * n + x]);
                if weight <= 0.0 {
                    continue;
                }
                let (east_km, north_km) = self.cell_center_km(y * n + x);
                east_sum += f64::from(east_km) * weight;
                north_sum += f64::from(north_km) * weight;
                weight_sum += weight;
                cells += 1;
            }
        }
        if cells < rule.min_cells.max(1) || weight_sum <= 0.0 {
            return None;
        }
        let east_km = (east_sum / weight_sum) as f32;
        let north_km = (north_sum / weight_sum) as f32;
        // The window CENTRE was inside the footprint; its centroid need not be,
        // because the window reaches `half_km` past its own centre. Without this
        // the box really does get hung off the edge of the coverage: on KVNX
        // 2026-06-09 20:42Z the centroid came out 227 km from the radar, where
        // the lowest tilt is already 4 km up and the box has no low levels in it
        // at all.
        let range_km = east_km.hypot(north_km);
        if range_km > rule.max_center_km && range_km > 0.0 {
            let scale = rule.max_center_km / range_km;
            return Some((east_km * scale, north_km * scale));
        }
        Some((east_km, north_km))
    }
}

/// One pass of the centre search: which cells count, and how much each is worth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClusterRule {
    /// Reflectivity a cell must reach to count at all, dBZ.
    pub min_dbz: f32,
    /// Extra weight per dB above `min_dbz`, as a fraction of the weight a cell
    /// at exactly `min_dbz` carries. See [`AUTO_CENTER_WEIGHT_PER_DB`].
    pub weight_per_db: f32,
    /// Distinct elevation angles the cell's echo must appear on. See
    /// [`AUTO_CENTER_MIN_SWEEPS`].
    pub min_sweeps: u8,
    /// Half-width of the window the search maximises over, km.
    pub half_km: f32,
    /// How far from the radar the ANSWER may be, km.
    pub max_center_km: f32,
    /// Fewest qualifying cells in the winning window. See
    /// [`AUTO_CENTER_MIN_CELLS`].
    pub min_cells: usize,
}

impl ClusterRule {
    /// The convective pass: the one that frames a storm.
    pub fn core() -> Self {
        Self {
            min_dbz: AUTO_CENTER_CORE_DBZ,
            weight_per_db: AUTO_CENTER_WEIGHT_PER_DB,
            min_sweeps: AUTO_CENTER_MIN_SWEEPS,
            half_km: AUTO_CENTER_WINDOW_HALF_KM,
            max_center_km: AUTO_CENTER_RANGE_KM - AUTO_CENTER_WINDOW_HALF_KM,
            min_cells: AUTO_CENTER_MIN_CELLS,
        }
    }

    /// The fallback pass, for volumes with no convection in them.
    pub fn echo() -> Self {
        Self {
            min_dbz: AUTO_CENTER_ECHO_DBZ,
            ..Self::core()
        }
    }

    /// Weight of one cell, in tenths of a threshold cell.
    ///
    /// Integer on purpose - see [`EchoComposite::strongest_cluster_km`] - and
    /// tenths because [`AUTO_CENTER_WEIGHT_PER_DB`] is a tenth per dB, so one dB
    /// above the threshold is exactly one unit and nothing is rounded away.
    fn weight(&self, dbz: f32) -> u32 {
        if !dbz.is_finite() || dbz < self.min_dbz {
            return 0;
        }
        let above_db = dbz.min(AUTO_CENTER_WEIGHT_CEILING_DBZ) - self.min_dbz;
        // `above_db` is finite and non-negative here, and the ceiling bounds it
        // at 60 dB, so the cast cannot saturate or wrap.
        10 + (above_db * self.weight_per_db * 10.0).round() as u32
    }
}

/// Build the column-max composite for `moment` from gates whose beam centre is
/// at or above `min_height_km` above the radar.
///
/// Ground range is `slant * cos(elevation)`. The 4/3-earth correction to that
/// arc is under 100 m at 230 km on a low tilt, two orders below the 2 km cell,
/// so the flat-earth HORIZONTAL projection is exact enough - which is not true
/// of the vertical one, and [`beam_height_km`] does not take it.
pub fn echo_composite(
    volume: &RadarVolume,
    moment: &MomentType,
    min_height_km: f32,
) -> Option<EchoComposite> {
    let half_span = 0.5 * AUTO_CENTER_N as f32 * AUTO_CENTER_CELL_KM;
    // Group the cuts by nominal elevation BEFORE rasterising them, because
    // `sweeps` counts heights and repeated cuts of one elevation are one height.
    // SAILS and MRLE cut the 0.5 deg sweep two or three times a volume; without
    // the grouping those alone would satisfy [`AUTO_CENTER_MIN_SWEEPS`] and the
    // depth test would pass on a single low tilt, which is exactly the case it
    // exists to fail.
    let mut by_elevation: std::collections::BTreeMap<i32, Vec<&radar_core::ElevationCut>> =
        std::collections::BTreeMap::new();
    for cut in &volume.cuts {
        // A cut with no usable nominal elevation still has per-radial
        // elevations, so it is rasterised; it simply cannot be told apart from
        // any other such cut, and they share one bucket.
        let key = if cut.elevation_deg.is_finite() {
            (cut.elevation_deg * 10.0).round() as i32
        } else {
            i32::MIN
        };
        by_elevation.entry(key).or_default().push(cut);
    }
    let sweeps_of_elevation: Vec<Vec<&radar_core::ElevationCut>> =
        by_elevation.into_values().collect();
    let empty = || {
        (
            vec![f32::NEG_INFINITY; AUTO_CENTER_N * AUTO_CENTER_N],
            vec![0u8; AUTO_CENTER_N * AUTO_CENTER_N],
        )
    };
    let (max_dbz, sweeps) = sweeps_of_elevation
        .par_iter()
        .map(|cuts| {
            let mut cells = vec![f32::NEG_INFINITY; AUTO_CENTER_N * AUTO_CENTER_N];
            for cut in cuts {
                let Some(grid) = cut.moments.get(moment) else {
                    continue;
                };
                let first_gate_km = grid.gate_range.first_gate_m as f32 / 1000.0;
                let gate_spacing_km = grid.gate_range.gate_spacing_m.max(1) as f32 / 1000.0;
                for (row, radial_index) in grid.radial_indices.iter().enumerate() {
                    let Some(radial) = cut.radials.get(*radial_index) else {
                        continue;
                    };
                    if !radial.azimuth_deg.is_finite() || !radial.elevation_deg.is_finite() {
                        continue;
                    }
                    let (sin_az, cos_az) = radial.azimuth_deg.to_radians().sin_cos();
                    let ground_scale = radial.elevation_deg.to_radians().cos();
                    for gate in 0..grid.gate_range.gate_count {
                        let slant_km = first_gate_km + gate_spacing_km * gate as f32;
                        let ground_km = slant_km * ground_scale;
                        // Range ascends with the gate index, so the first gate
                        // past the search radius ends this radial.
                        if ground_km >= half_span {
                            break;
                        }
                        if beam_height_km(slant_km, radial.elevation_deg) < min_height_km {
                            continue;
                        }
                        let Some(value) = grid.scaled_value(row, gate).filter(|v| v.is_finite())
                        else {
                            continue;
                        };
                        let east_km = ground_km * sin_az;
                        let north_km = ground_km * cos_az;
                        let x = ((east_km + half_span) / AUTO_CENTER_CELL_KM) as usize;
                        let y = ((north_km + half_span) / AUTO_CENTER_CELL_KM) as usize;
                        if x >= AUTO_CENTER_N || y >= AUTO_CENTER_N {
                            continue;
                        }
                        let cell = &mut cells[y * AUTO_CENTER_N + x];
                        if value > *cell {
                            *cell = value;
                        }
                    }
                }
            }
            // This elevation's contribution to the depth count: one, or none.
            let seen = cells
                .iter()
                .map(|value| u8::from(value.is_finite() && *value >= AUTO_CENTER_ECHO_DBZ))
                .collect();
            (cells, seen)
        })
        .reduce(empty, |mut left, right| {
            for (target, value) in left.0.iter_mut().zip(right.0) {
                if value > *target {
                    *target = value;
                }
            }
            for (target, value) in left.1.iter_mut().zip(right.1) {
                *target = target.saturating_add(value);
            }
            left
        });
    max_dbz
        .iter()
        .any(|value| value.is_finite())
        .then_some(EchoComposite {
            cell_km: AUTO_CENTER_CELL_KM,
            n: AUTO_CENTER_N,
            max_dbz,
            sweeps,
        })
}

/// Where to put the 3D box for `volume`: km east and north of the radar, or
/// `None` when there is nothing in the volume worth framing, which leaves the
/// box wherever it already was.
///
/// The centre is the reflectivity-weighted centroid of the box-sized window
/// holding the most >= 35 dBZ echo above 2 km, seen at two or more elevations,
/// falling back to the same rule at 20 dBZ for volumes with no convection in
/// them.
///
/// Every clause of that sentence is load-bearing, and each was measured against
/// dropping it. Over 139 CONSECUTIVE volumes from eight sites (KARX, KEAX, KDVN,
/// KTWX, KILX, KLSX, KVWX, 2026-06-08/09) - consecutive because a centre is
/// judged in a loop, not in a still - against the rule this replaced, which
/// counted cells and took their unweighted centroid:
///
/// ```text
///                      core framed   peak missed   jump median / p90 / >30 km
///  counted cells          16.8%        5.55 dB      5.6 / 42.3 km / 18 of 132
///  weighted, clamped      16.4%        3.71 dB      5.7 / 15.2 km / 10 of 132
/// ```
///
/// The single obvious alternative, "put the box on the strongest gate", frames
/// one 2 km cell and is not stable enough to be worth a column here: on KARX
/// 2026-06-08 17:28Z the volume's peak was 164 km from the cluster an analyst
/// would have framed. The other, "take the centroid of every core cell in the
/// volume", lands BETWEEN the storms whenever there is more than one.
///
/// The residual, stated because it is real: 10 of 132 volume-to-volume steps
/// still move the box more than a box half-width. Some are the storm moving,
/// some are a second storm overtaking the first on weight. There is no
/// hysteresis here - the centre is a pure function of one volume - so a genuine
/// near-tie between two cells will alternate.
pub fn auto_box_center_km(volume: &RadarVolume) -> Option<(f32, f32)> {
    let composite = echo_composite(volume, &MomentType::Reflectivity, AUTO_CENTER_MIN_HEIGHT_KM)?;
    for rule in [ClusterRule::core(), ClusterRule::echo()] {
        if let Some((east_km, north_km)) = composite.strongest_cluster_km(rule) {
            // Belt and braces: everything upstream is finite by construction,
            // and a non-finite centre would reach the resample key, the box
            // origin and the floor raster, where it would not fail loudly but
            // would silently produce an empty box.
            if east_km.is_finite() && north_km.is_finite() {
                return Some((east_km, north_km));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentGrid, MomentStorage, RadarSite, Radial};

    const TEST_GATE_COUNT: usize = 300;
    const TEST_GATE_SPACING_M: i32 = 1_000;

    /// One gate to plant: azimuth in whole degrees, slant range in km, dBZ.
    type TestGate = (usize, usize, f32);
    /// One cut to build: its elevation and the gates on it.
    type TestCut<'a> = (f32, &'a [TestGate]);

    /// Cuts of 360 radials with 1 km gates starting at the radar, so a gate
    /// index is a slant range in km and every expectation below is arithmetic.
    ///
    /// Each cut is `(elevation_deg, &[(azimuth_deg, range_km, dbz)])`. The 2 and
    /// 3 deg elevations [`storm_volume`] uses put an 80 km gate 3.2 and 4.6 km
    /// above the radar, clear of [`AUTO_CENTER_MIN_HEIGHT_KM`], and shorten
    /// ground range by 0.06% and 0.14% - far inside a 2 km cell, so both land in
    /// the same cell and the cell arithmetic is unchanged. Everything not listed
    /// is NaN, which f32 storage hands back unchanged and the composite drops.
    fn test_volume(cuts: &[TestCut<'_>]) -> RadarVolume {
        let time = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("a fixed epoch second is a valid timestamp");
        let mut volume = RadarVolume::new(RadarSite::new("KTLX"), time);
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: TEST_GATE_SPACING_M,
            gate_count: TEST_GATE_COUNT,
        };
        for (elevation_deg, gates) in cuts {
            let cut = volume.push_cut(*elevation_deg, Some(1));
            for azimuth in 0..360 {
                cut.radials.push(Radial {
                    azimuth_deg: azimuth as f32,
                    elevation_deg: *elevation_deg,
                    time_offset_ms: 0,
                    gate_range: gate_range.clone(),
                    nyquist_velocity_mps: None,
                    radial_status: None,
                });
            }
            let mut values = vec![f32::NAN; 360 * TEST_GATE_COUNT];
            for (azimuth_deg, range_km, dbz) in *gates {
                values[azimuth_deg * TEST_GATE_COUNT + range_km] = *dbz;
            }
            cut.moments.insert(
                MomentType::Reflectivity,
                MomentGrid {
                    moment: MomentType::Reflectivity,
                    gate_range: gate_range.clone(),
                    scale: 1.0,
                    offset: 0.0,
                    nodata: None,
                    range_folded: None,
                    radial_indices: (0..360).collect(),
                    storage: MomentStorage::F32(values),
                },
            );
        }
        volume
    }

    /// The same gates on two elevations, which is the least a real storm does
    /// and the least [`AUTO_CENTER_MIN_SWEEPS`] accepts. A fixture on one
    /// elevation is a spike, not a storm, and is used deliberately below.
    fn storm_volume(gates: &[TestGate]) -> RadarVolume {
        test_volume(&[(2.0, gates), (3.0, gates)])
    }

    fn composite_of(volume: &RadarVolume) -> EchoComposite {
        echo_composite(volume, &MomentType::Reflectivity, AUTO_CENTER_MIN_HEIGHT_KM)
            .expect("the volume has reflectivity aloft")
    }

    /// The shipping rule with one constant relaxed, for tests that need a
    /// smaller cluster than an operator would ever be shown.
    fn rule(min_dbz: f32, min_cells: usize) -> ClusterRule {
        ClusterRule {
            min_dbz,
            min_cells,
            ..ClusterRule::core()
        }
    }

    fn cluster(composite: &EchoComposite, min_dbz: f32, min_cells: usize) -> Option<(f32, f32)> {
        composite.strongest_cluster_km(rule(min_dbz, min_cells))
    }

    #[test]
    fn box_sizes_are_the_voxel_sizes_the_doc_comment_claims() {
        // side / BOX_N, by hand: 30000/192 = 156.25, 60000/192 = 312.5,
        // 120000/192 = 625, 240000/192 = 1250, 360000/192 = 1875.
        for (side_km, voxel_m) in [
            (30.0, 156.25),
            (60.0, 312.5),
            (120.0, 625.0),
            (240.0, 1250.0),
            (360.0, 1875.0),
        ] {
            let measured = box_voxel_m(side_km * 0.5);
            assert!(
                (measured - voxel_m).abs() < 1.0e-3,
                "{side_km} km box: {measured} m per voxel, expected {voxel_m}"
            );
        }
        assert_eq!(BOX_SIZE_CHOICES_KM, [30.0, 60.0, 120.0, 240.0, 360.0]);
    }

    #[test]
    fn the_default_box_is_offered_and_resolves_the_gate_rather_than_interpolating_it() {
        assert!(
            BOX_SIZE_CHOICES_KM.contains(&(BOX_HALF_KM * 2.0)),
            "the default box size must be one the picker offers"
        );

        // The claim in BOX_HALF_KM's doc comment, made checkable. A
        // super-resolution reflectivity gate is 250 m long; the 0.5 deg beam is
        // 60_000 * 0.5 * pi / 180 = 523.6 m wide at the 60 km box edge and wider
        // beyond. The default voxel has to land between: no coarser than the
        // beam (or it discards resolution the radar collected) and no finer than
        // the gate (or it is interpolating).
        let voxel_m = box_voxel_m(BOX_HALF_KM);
        let beam_arc_m = 60_000.0 * 0.5_f32.to_radians();
        assert!((beam_arc_m - 523.6).abs() < 0.1, "{beam_arc_m}");
        assert!(voxel_m < beam_arc_m, "{voxel_m} m is coarser than the beam");
        assert!(voxel_m >= 250.0, "{voxel_m} m is finer than a gate");

        // And the next size down is not: 156 m is finer than a gate in every
        // direction, so it can only interpolate.
        assert!(box_voxel_m(BOX_SIZE_CHOICES_KM[0] * 0.5) < 250.0);
    }

    #[test]
    fn the_default_box_renders_a_supercell_broader_than_it_is_deep() {
        let vol3d = Vol3d::default();
        // Display units: one horizontal unit is `box_half_km`, and `zspan`
        // normalises the vertical by top_km / half_km, so a height in km renders
        // as km * exaggeration / half_km.
        let width = 30.0 / vol3d.box_half_km;
        let height = 15.0 * vol3d.vertical_exaggeration / vol3d.box_half_km;
        assert!((width - 1.0).abs() < 1.0e-6, "{width}");
        assert!((height - 0.75).abs() < 1.0e-6, "{height}");
        assert!(
            height < width,
            "a 30 km wide, 15 km deep supercell must not render taller than it is wide"
        );
        // 18 km of box over a 30 km half-width at 1.5x.
        assert!((vol3d.zspan() - 0.9).abs() < 1.0e-6, "{}", vol3d.zspan());
    }

    #[test]
    fn the_exaggeration_slider_means_the_same_thing_at_every_offered_box_size() {
        // `vol3d::tests::vertical_exaggeration_is_independent_of_box_size` swept
        // a sample of sizes at one multiplier. This sweeps what the UI can
        // actually produce: every box size THIS module offers, against the full
        // range of the exaggeration slider in `vol3d/pane.rs` (0.5 to 6.0). The
        // corner that used to fail is the widest box at the lowest exaggeration,
        // 18 / 180 * 0.5 = 0.05, which the old 0.06 floor in `Vol3d::zspan`
        // clamped - so the box was drawn 20% taller than the operator asked for.
        let mut explorer = Vol3d::default();
        for side_km in BOX_SIZE_CHOICES_KM {
            for step in 0..=55 {
                let exaggeration = 0.5 + 0.1 * step as f32;
                explorer.box_half_km = side_km * 0.5;
                explorer.vertical_exaggeration = exaggeration;
                let recovered = explorer.zspan() * explorer.box_half_km / explorer.top_km();
                assert!(
                    (recovered - exaggeration).abs() < 1.0e-4,
                    "{side_km} km box at {exaggeration}x recovered {recovered}"
                );
                assert!(explorer.zspan() > 0.0 && explorer.zspan().is_finite());
            }
        }
    }

    #[test]
    fn auto_center_grid_covers_exactly_the_search_range() {
        assert!(
            (AUTO_CENTER_N as f32 * AUTO_CENTER_CELL_KM - 2.0 * AUTO_CENTER_RANGE_KM).abs() < 1e-6
        );
    }

    #[test]
    fn cell_centres_are_hand_computable() {
        let composite = EchoComposite {
            cell_km: AUTO_CENTER_CELL_KM,
            n: AUTO_CENTER_N,
            max_dbz: vec![f32::NEG_INFINITY; AUTO_CENTER_N * AUTO_CENTER_N],
            sweeps: vec![0; AUTO_CENTER_N * AUTO_CENTER_N],
        };
        // Cell 0 spans [-230, -228) east and north, so its centre is -229.
        assert_eq!(composite.cell_center_km(0), (-229.0, -229.0));
        // Cell (115, 115) spans [0, 2): the first cell past the radar.
        assert_eq!(
            composite.cell_center_km(115 * AUTO_CENTER_N + 115),
            (1.0, 1.0)
        );
        assert_eq!(
            composite.cell_center_km(AUTO_CENTER_N * AUTO_CENTER_N - 1),
            (229.0, 229.0)
        );
    }

    #[test]
    fn beam_height_matches_the_four_thirds_earth_formula() {
        // Hand-computed from h = sqrt(r^2 + ae^2 + 2 r ae sin(theta)) - ae:
        // 80 km at 2 deg is 80 sin 2 + 80^2 / (2 ae) to first order
        // = 2.7926 + 0.3767 = 3.169 km.
        assert!((beam_height_km(80.0, 2.0) - 3.169).abs() < 0.01);
        // A clutter return: 10 km on the 0.5 deg tilt is 93 m up.
        assert!((beam_height_km(10.0, 0.5) - 0.093).abs() < 0.005);
        // And the standard 0.5 deg beam is above 4 km beyond 230 km, which is
        // why AUTO_CENTER_RANGE_KM stops there.
        assert!(beam_height_km(230.0, 0.5) > 4.0);
    }

    #[test]
    fn cell_weight_is_hand_computable() {
        let core = ClusterRule::core();
        // Ten units at the threshold, one more per dB above it: 0.1 of a
        // threshold cell per dB, in tenths.
        assert_eq!(core.weight(AUTO_CENTER_CORE_DBZ), 10);
        assert_eq!(core.weight(45.0), 20);
        assert_eq!(core.weight(55.0), 30);
        // Saturating at the ceiling: 80 - 35 = 45 dB above, so 10 + 45 = 55.
        assert_eq!(core.weight(AUTO_CENTER_WEIGHT_CEILING_DBZ), 55);
        assert_eq!(core.weight(200.0), 55);
        assert_eq!(core.weight(1.0e30), 55);
        // Below the threshold, and the values a corrupt volume carries, are
        // worth nothing at all rather than something enormous.
        assert_eq!(core.weight(34.9), 0);
        assert_eq!(core.weight(f32::NAN), 0);
        assert_eq!(core.weight(f32::INFINITY), 0);
        assert_eq!(core.weight(f32::NEG_INFINITY), 0);
        // The fallback pass measures its dB above its own lower threshold.
        assert_eq!(ClusterRule::echo().weight(AUTO_CENTER_ECHO_DBZ), 10);
        assert_eq!(ClusterRule::echo().weight(35.0), 25);
    }

    #[test]
    fn a_single_core_lands_in_the_cell_that_contains_it() {
        // 80 km slant on the 2 deg tilt is 79.95 km of ground range; on the
        // 45 deg radial that is 56.53 km east and the same north. That is cell
        // floor((56.53 + 230) / 2) = 143 on both axes, whose centre is
        // 143.5 * 2 - 230 = 57.0.
        let volume = storm_volume(&[(45, 80, 60.0)]);
        let composite = composite_of(&volume);
        assert_eq!(composite.strongest_column_km(), Some((57.0, 57.0)));
        assert_eq!(composite.core_cells(AUTO_CENTER_CORE_DBZ), 1);
        // Within half a cell of the truth on each axis.
        assert!((57.0 - 79.95 * 45.0_f32.to_radians().sin()).abs() <= 0.5 * AUTO_CENTER_CELL_KM);
        // One cell is a speck, so the shipping rule declines to move the box.
        assert_eq!(auto_box_center_km(&volume), None);
        // With the floor lowered to one cell it is the answer.
        assert_eq!(
            cluster(&composite, AUTO_CENTER_CORE_DBZ, 1),
            Some((57.0, 57.0))
        );
    }

    #[test]
    fn ground_clutter_below_the_height_gate_cannot_move_the_box() {
        // 65 dBZ of clutter due east at 10, 11 and 12 km on the two lowest
        // tilts, 93 to 130 m above the radar, against real 45 dBZ convection
        // 80 km out and 3.2 km up.
        //
        // The clutter gates land in cells centred 9 and 11 km east - due east is
        // the y row centred 1 km north - and the convection in the two cells
        // (57, 57) and (57, 55), the second because 80 km on the 46 deg radial
        // is 55.5 km north where 80 km on the 45 deg radial is 56.5 km.
        let clutter: &[TestGate] = &[(90, 10, 65.0), (90, 11, 65.0), (90, 12, 65.0)];
        let convection: &[TestGate] = &[(45, 80, 45.0), (46, 80, 45.0)];
        let volume = test_volume(&[
            (0.5, clutter),
            (0.9, clutter),
            (2.0, convection),
            (3.0, convection),
        ]);

        // Ungated, the clutter IS the strongest column and it is 9 km east.
        // Equal maxima keep the first cell in raster order, so this is the
        // nearer of the two clutter cells rather than an arbitrary one.
        let whole_column = echo_composite(&volume, &MomentType::Reflectivity, 0.0)
            .expect("the volume has reflectivity");
        assert_eq!(whole_column.strongest_column_km(), Some((9.0, 1.0)));

        // Gated at 2 km it is gone, and only the convection is left.
        let aloft = composite_of(&volume);
        assert_eq!(aloft.strongest_column_km(), Some((57.0, 55.0)));
        assert_eq!(aloft.core_cells(AUTO_CENTER_CORE_DBZ), 2);
        assert_eq!(cluster(&aloft, AUTO_CENTER_CORE_DBZ, 1), Some((57.0, 56.0)));
    }

    #[test]
    fn the_window_decides_whether_two_cores_are_one_cluster_or_two() {
        // 100 km slant is 99.94 km of ground range, 70.67 km each way: cell
        // floor((70.67 + 230) / 2) = 150, centre 71.0. The two cores are 14 km
        // apart on each axis, and equally bright, so the weighting cannot
        // decide between them and the geometry has to.
        let volume = storm_volume(&[(45, 80, 50.0), (45, 100, 50.0)]);
        let composite = composite_of(&volume);
        assert_eq!(composite.core_cells(AUTO_CENTER_CORE_DBZ), 2);

        // A 60 km window holds both, so the answer is their centroid,
        // (57 + 71) / 2 = 64.
        assert_eq!(
            composite.strongest_cluster_km(ClusterRule {
                min_cells: 1,
                ..ClusterRule::core()
            }),
            Some((64.0, 64.0))
        );
        // A 4 km window holds only one. Windows that hold one are all tied, so
        // the tie is what the centroid resolves: whichever tied window wins, it
        // contains exactly the nearer core, and the answer is that core.
        assert_eq!(
            composite.strongest_cluster_km(ClusterRule {
                half_km: 2.0,
                min_cells: 1,
                ..ClusterRule::core()
            }),
            Some((57.0, 57.0))
        );
    }

    #[test]
    fn the_brighter_of_two_equal_clusters_is_the_one_framed() {
        // Two clusters of the same size and shape, mirrored about north so
        // their cell counts are exactly equal: a dim one west, a bright one
        // east. 114 km apart, so no window holds both.
        let mut gates: Vec<TestGate> = Vec::new();
        for azimuth in 43..49 {
            for range in 78..84 {
                gates.push((azimuth, range, 55.0));
                gates.push((360 - azimuth, range, 40.0));
            }
        }
        let volume = storm_volume(&gates);
        let composite = composite_of(&volume);

        // Counting cells cannot tell them apart, so raster order decides, and
        // raster order runs west to east: the DIM cluster wins.
        let counted = composite
            .strongest_cluster_km(ClusterRule {
                weight_per_db: 0.0,
                ..ClusterRule::core()
            })
            .expect("both clusters clear the floor");
        assert!(counted.0 < 0.0, "counting picked {counted:?}, not the west");

        // Weighting the cells by how far above the threshold they are is what
        // breaks the tie, and it breaks it toward the storm an analyst opened
        // the pane for.
        let framed = auto_box_center_km(&volume).expect("the volume has a cluster");
        assert!(
            framed.0 > 0.0,
            "the bright east cluster was not framed: {framed:?}"
        );
        assert!((framed.0 + counted.0).abs() < 1.0, "{framed:?} {counted:?}");
    }

    #[test]
    fn a_cluster_smaller_than_the_floor_leaves_the_box_alone() {
        let volume = storm_volume(&[(45, 80, 60.0), (45, 100, 50.0)]);
        let composite = composite_of(&volume);
        assert!(cluster(&composite, AUTO_CENTER_CORE_DBZ, 2).is_some());
        assert_eq!(cluster(&composite, AUTO_CENTER_CORE_DBZ, 3), None);
        // Which is what the shipping floor does with a two-cell speck.
        const { assert!(AUTO_CENTER_MIN_CELLS > 2) };
        assert_eq!(auto_box_center_km(&volume), None);
    }

    #[test]
    fn the_answer_may_not_be_hung_off_the_edge_of_the_coverage() {
        // A storm at 225 km, which is inside the searched range but further out
        // than a box may be centred: past 200 km the box would reach beyond the
        // 230 km search and the lowest tilt out there is already 4 km up, so
        // there are no low levels left to render.
        let mut gates: Vec<TestGate> = Vec::new();
        for azimuth in 88..94 {
            for range in 222..228 {
                gates.push((azimuth, range, 55.0));
            }
        }
        let volume = storm_volume(&gates);
        let center = auto_box_center_km(&volume).expect("the storm clears the floor");
        let range_km = center.0.hypot(center.1);
        let limit = AUTO_CENTER_RANGE_KM - AUTO_CENTER_WINDOW_HALF_KM;
        assert!(
            (range_km - limit).abs() < 1.0e-3,
            "{center:?} is {range_km} km out"
        );
        // Pulled straight back along its own bearing, so the box still points
        // at the storm; it is only closer to the radar than the storm is.
        assert!(center.0 > 0.0 && center.1.abs() < 20.0, "{center:?}");

        // The window centre alone does not enforce this: the window reaches
        // `half_km` past its own centre, so its centroid can be 230 km out.
        let composite = composite_of(&volume);
        let unclamped = composite
            .strongest_cluster_km(ClusterRule {
                max_center_km: AUTO_CENTER_RANGE_KM,
                ..ClusterRule::core()
            })
            .expect("the storm clears the floor");
        assert!(
            unclamped.0.hypot(unclamped.1) > limit,
            "{unclamped:?} would not have needed clamping"
        );
    }

    #[test]
    fn stratiform_echo_is_framed_only_when_there_is_no_convection() {
        // Fourteen cells of 25 dBZ, which is the fallback threshold's business,
        // plus two cells of 60 dBZ, which is under the cluster floor. The
        // stratiform gates step 3 km down the 200 deg radial so that each lands
        // in its own 2 km cell; stepping 1 km would put two or three of them in
        // the same cell and the cluster would never reach the floor. Fourteen
        // rather than twelve because one gate of a sparse fixture like this can
        // straddle a cell boundary BETWEEN the two elevations - 0.08 km of
        // ground range separates them at 110 km - and then neither of the two
        // cells it lands in is seen at two heights. Real gates are packed
        // hundreds to a cell and cannot do that.
        let mut gates: Vec<TestGate> = (0..14).map(|i| (200, 80 + 3 * i, 25.0)).collect();
        gates.push((45, 80, 60.0));
        gates.push((46, 80, 60.0));
        let volume = storm_volume(&gates);
        let composite = composite_of(&volume);
        assert_eq!(composite.core_cells(AUTO_CENTER_CORE_DBZ), 2);
        assert!(composite.core_cells(AUTO_CENTER_ECHO_DBZ) >= AUTO_CENTER_MIN_CELLS);

        // The 35 dBZ pass finds only the two-cell speck and declines; the
        // 20 dBZ pass frames the stratiform band, which is on the 200 deg
        // radial, so south-west of the radar.
        let center = auto_box_center_km(&volume).expect("the fallback frames the band");
        assert!(center.0 < 0.0 && center.1 < 0.0, "{center:?}");
    }

    #[test]
    fn a_solar_spike_cannot_take_the_box_off_a_real_storm() {
        // The sun in a sidelobe at sunrise: one azimuth, ONE elevation, and the
        // range-square correction makes it brighter with range, so it reads as
        // 25 dBZ at 100 km and 44 dBZ at 230 km. Sixty-five of its cells clear
        // 35 dBZ - well past the cluster floor - and every one of them is 2 km
        // long by one beam wide.
        let spike: Vec<TestGate> = (100..230)
            .map(|range| (270, range, 25.0 + 0.15 * (range as f32 - 100.0)))
            .collect();
        let mut storm: Vec<TestGate> = Vec::new();
        for azimuth in 43..49 {
            for range in 78..84 {
                storm.push((azimuth, range, 50.0));
            }
        }

        // The spike alone: enough cells, bright enough, and it is refused.
        let sun = test_volume(&[(2.0, &spike)]);
        let sun_composite = composite_of(&sun);
        assert!(
            sun_composite.core_cells(AUTO_CENTER_CORE_DBZ) > 2 * AUTO_CENTER_MIN_CELLS,
            "{}",
            sun_composite.core_cells(AUTO_CENTER_CORE_DBZ)
        );
        assert_eq!(auto_box_center_km(&sun), None);

        // Depth is what refuses it: relax that one rule and the spike is a
        // storm, 197 km due west of the radar.
        let one_sweep = sun_composite
            .strongest_cluster_km(ClusterRule {
                min_sweeps: 1,
                ..ClusterRule::core()
            })
            .expect("the spike clears every other rule");
        assert!(one_sweep.0 < -150.0, "{one_sweep:?}");

        // And with a real storm in the same volume, the spike does not merely
        // fail to win - it must not be able to drag the box off the storm.
        let mut both = spike.clone();
        both.extend_from_slice(&storm);
        let sun_and_storm = test_volume(&[(2.0, &both), (3.0, &storm)]);
        let framed = auto_box_center_km(&sun_and_storm).expect("the storm is framed");
        assert!(framed.0 > 45.0 && framed.1 > 45.0, "{framed:?}");
    }

    #[test]
    fn repeated_cuts_of_one_elevation_are_one_height() {
        // SAILS and MRLE cut the 0.5 deg sweep two or three times a volume. If
        // `sweeps` counted cuts, three of them would satisfy the depth rule on
        // their own and a single low tilt would look like a storm.
        let storm: &[TestGate] = &[(45, 80, 50.0)];
        let repeated = test_volume(&[(2.0, storm), (2.0, storm), (2.0, storm)]);
        let composite = composite_of(&repeated);
        let cell = composite
            .max_dbz
            .iter()
            .position(|value| value.is_finite())
            .expect("the storm is in the composite");
        assert_eq!(
            composite.sweeps[cell], 1,
            "three cuts of 2.0 deg are one height"
        );
        assert_eq!(cluster(&composite, AUTO_CENTER_CORE_DBZ, 1), None);

        // Two DIFFERENT elevations are two heights, and then the same cell is
        // allowed to choose the centre.
        let deep = test_volume(&[(2.0, storm), (3.0, storm)]);
        let deep_composite = composite_of(&deep);
        assert_eq!(deep_composite.sweeps[cell], 2);
        assert_eq!(
            cluster(&deep_composite, AUTO_CENTER_CORE_DBZ, 1),
            Some((57.0, 57.0))
        );
    }

    #[test]
    fn a_volume_with_no_reflectivity_at_all_has_no_opinion() {
        let volume = storm_volume(&[]);
        assert!(echo_composite(&volume, &MomentType::Reflectivity, 0.0).is_none());
        assert_eq!(auto_box_center_km(&volume), None);
    }

    #[test]
    fn degenerate_volumes_are_declined_rather_than_survived() {
        let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("a valid timestamp");

        // No cuts at all: a volume header and nothing behind it.
        let bare = RadarVolume::new(RadarSite::new("KTLX"), time);
        assert!(echo_composite(&bare, &MomentType::Reflectivity, 0.0).is_none());
        assert_eq!(auto_box_center_km(&bare), None);

        // One radial, which is what the first milliseconds of a live volume
        // look like.
        let mut lone = RadarVolume::new(RadarSite::new("KTLX"), time);
        let gate_range = GateRange {
            first_gate_m: 80_000,
            gate_spacing_m: TEST_GATE_SPACING_M,
            gate_count: 4,
        };
        let cut = lone.push_cut(2.0, Some(1));
        cut.radials.push(Radial {
            azimuth_deg: 90.0,
            elevation_deg: 2.0,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: None,
            radial_status: None,
        });
        cut.moments.insert(
            MomentType::Reflectivity,
            MomentGrid {
                moment: MomentType::Reflectivity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: vec![0],
                storage: MomentStorage::F32(vec![60.0; 4]),
            },
        );
        assert_eq!(auto_box_center_km(&lone), None);

        // A field of NaN, of infinities, and of values no radar can produce.
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0e30, -1.0e30] {
            let mut gates: Vec<TestGate> = Vec::new();
            for azimuth in 43..49 {
                for range in 78..84 {
                    gates.push((azimuth, range, value));
                }
            }
            let volume = storm_volume(&gates);
            let center = auto_box_center_km(&volume);
            assert!(
                center.is_none_or(|(east, north)| east.is_finite() && north.is_finite()),
                "{value} produced {center:?}"
            );
        }

        // And a real storm with one absurd cell in the middle of it still comes
        // back finite and still frames the storm.
        let mut gates: Vec<TestGate> = Vec::new();
        for azimuth in 43..49 {
            for range in 78..84 {
                gates.push((azimuth, range, 50.0));
            }
        }
        gates.push((46, 81, 1.0e30));
        let volume = storm_volume(&gates);
        let framed = auto_box_center_km(&volume).expect("the storm is still framed");
        assert!(framed.0.is_finite() && framed.1.is_finite());
        assert!(framed.0 > 45.0 && framed.1 > 45.0, "{framed:?}");
    }

    #[test]
    fn gates_beyond_the_search_range_are_dropped_rather_than_wrapped() {
        // 250 km is outside the 230 km raster. Indexing it would land in the
        // wrong cell rather than nowhere, so this guard is what keeps a distant
        // second-trip echo from moving the box.
        let volume = storm_volume(&[(90, 250, 60.0), (45, 80, 40.0)]);
        let composite = composite_of(&volume);
        assert_eq!(composite.core_cells(AUTO_CENTER_CORE_DBZ), 1);
        assert_eq!(
            cluster(&composite, AUTO_CENTER_CORE_DBZ, 1),
            Some((57.0, 57.0))
        );
    }

    /// Enough core cells in one place to clear [`AUTO_CENTER_MIN_CELLS`],
    /// centred on the 45 deg radial at 80 km.
    fn framed_storm() -> RadarVolume {
        let mut gates: Vec<TestGate> = Vec::new();
        for azimuth in 43..49 {
            for range in 78..84 {
                gates.push((azimuth, range, 50.0));
            }
        }
        storm_volume(&gates)
    }

    #[test]
    fn the_same_volume_gives_the_same_centre_every_time() {
        // The resample key carries the centre, so a centre that wobbled - by a
        // tie broken differently, by a parallel reduction landing in a different
        // order - would rebuild the 192 x 192 x 48 box on every frame.
        let volume = framed_storm();
        let first = auto_box_center_km(&volume).expect("the storm is framed");
        for _ in 0..8 {
            assert_eq!(auto_box_center_km(&volume), Some(first));
        }
        assert_eq!(
            box_center_key(first.0, first.1),
            box_center_key(first.0, first.1)
        );
    }

    #[test]
    fn the_storm_centre_is_measured_once_per_volume() {
        let volume = Arc::new(framed_storm());
        let mut vol3d = Vol3d::default();
        assert_eq!(vol3d.box_center_mode, Vol3dBoxCenter::Storm);
        let first = vol3d.resolve_box_center(&volume);
        assert!(first.0 > 45.0 && first.1 > 45.0, "{first:?}");

        // The cache is what keeps a 20-million-gate scan off the frame thread,
        // and this is how it is observable: move the centre by hand, and the
        // same volume does not measure again to undo it.
        vol3d.box_center_east_km = 12.0;
        assert_eq!(vol3d.resolve_box_center(&volume), (12.0, first.1));

        // A different volume does measure again.
        let next = Arc::new(framed_storm());
        assert_eq!(vol3d.resolve_box_center(&next), first);
    }

    #[test]
    fn a_lull_leaves_the_box_where_the_operator_is_looking() {
        let storm = Arc::new(framed_storm());
        let empty = Arc::new(storm_volume(&[]));
        let mut vol3d = Vol3d::default();
        let framed = vol3d.resolve_box_center(&storm);
        assert!(framed.0 > 45.0);
        assert_eq!(
            vol3d.resolve_box_center(&empty),
            framed,
            "a scan with no echo must not snap the box back to the radar"
        );
    }

    #[test]
    fn the_radar_and_pinned_modes_ignore_the_storm() {
        let volume = Arc::new(framed_storm());
        let mut vol3d = Vol3d {
            box_center_mode: Vol3dBoxCenter::Radar,
            ..Vol3d::default()
        };
        assert_eq!(vol3d.resolve_box_center(&volume), (0.0, 0.0));

        vol3d.pin_box_center(-15.0, 4.0);
        assert_eq!(vol3d.box_center_mode, Vol3dBoxCenter::Fixed);
        assert_eq!(vol3d.resolve_box_center(&volume), (-15.0, 4.0));
        let next = Arc::new(framed_storm());
        assert_eq!(vol3d.resolve_box_center(&next), (-15.0, 4.0));

        // A pin that is not a place is refused outright rather than stored: a
        // NaN centre builds an empty box without failing anywhere.
        vol3d.pin_box_center(f32::NAN, 0.0);
        assert_eq!(vol3d.resolve_box_center(&next), (-15.0, 4.0));

        // And handing it back re-measures at once, rather than waiting for the
        // next volume to arrive.
        vol3d.follow_storm();
        let framed = vol3d.resolve_box_center(&next);
        assert!(framed.0 > 45.0 && framed.1 > 45.0, "{framed:?}");
    }

    #[test]
    fn switching_away_from_follow_storm_and_back_re_measures_the_same_volume() {
        // The cache is keyed on the VOLUME, so a mode that moved the box has to
        // drop it or Follow Storm does nothing until the next volume arrives.
        let volume = Arc::new(framed_storm());
        let mut vol3d = Vol3d::default();
        let framed = vol3d.resolve_box_center(&volume);
        assert!(framed.0 > 45.0, "{framed:?}");

        vol3d.box_center_mode = Vol3dBoxCenter::Radar;
        assert_eq!(vol3d.resolve_box_center(&volume), (0.0, 0.0));
        vol3d.box_center_mode = Vol3dBoxCenter::Storm;
        assert_eq!(
            vol3d.resolve_box_center(&volume),
            framed,
            "Follow Storm did not re-measure the volume it was already showing"
        );

        vol3d.pin_box_center(-15.0, 4.0);
        assert_eq!(vol3d.resolve_box_center(&volume), (-15.0, 4.0));
        vol3d.box_center_mode = Vol3dBoxCenter::Storm;
        assert_eq!(vol3d.resolve_box_center(&volume), framed);
    }

    #[test]
    fn nothing_a_camera_or_a_palette_does_moves_the_box() {
        // The resample key carries the centre, so anything that moves the centre
        // rebuilds the 192 x 192 x 48 box. Camera motion, opacity, palette and
        // threshold changes happen every frame of an interaction and must not.
        let volume = Arc::new(framed_storm());
        let mut vol3d = Vol3d::default();
        let framed = vol3d.resolve_box_center(&volume);

        vol3d.yaw += 1.3;
        vol3d.pitch = 0.9;
        vol3d.dist = 5.0;
        vol3d.opacity = 0.9;
        vol3d.density = 2.0;
        vol3d.shading = 0.1;
        vol3d.threshold_dbz = 55.0;
        vol3d.floor_threshold_dbz = 20.0;
        vol3d.vertical_exaggeration = 4.0;
        vol3d.lut_signature = 42;
        vol3d.quality = super::super::Vol3dQuality::High;
        for _ in 0..16 {
            assert_eq!(vol3d.resolve_box_center(&volume), framed);
            assert_eq!(
                box_center_key(vol3d.box_center_east_km, vol3d.box_center_north_km),
                box_center_key(framed.0, framed.1)
            );
        }

        // Box SIZE is the one thing on that list that does move the box, and it
        // does so by changing the footprint rather than the centre: the centre
        // is measured for one window size and reused at every box size.
        vol3d.box_half_km = 180.0;
        assert_eq!(vol3d.resolve_box_center(&volume), framed);
    }

    #[test]
    fn the_resample_key_resolves_a_centre_move_finer_than_one_voxel() {
        assert_eq!(box_center_key(0.0, 0.0), (0, 0));
        // 10 m is far inside a voxel and must not rebuild the box.
        assert_eq!(box_center_key(0.01, -0.01), (0, 0));
        // 100 m is the quantum, and it is smaller than the smallest voxel any
        // box size can have, so nothing that could change a voxel is missed.
        assert_eq!(box_center_key(0.1, -0.1), (1, -1));
        assert!(box_voxel_m(BOX_SIZE_CHOICES_KM[0] * 0.5) > 100.0);
        assert_eq!(box_center_key(-57.04, 12.36), (-570, 124));
    }

    /// The measurement behind [`BOX_HALF_KM`], [`auto_box_center_km`] and every
    /// number quoted in this module. Run it over the workstation's own Level II
    /// cache:
    ///
    /// ```text
    /// RADAR_L2_SAMPLES=<dir or ;-separated files> \
    ///   cargo test --release -p workstation_app -- --ignored --nocapture \
    ///   the_default_box_frames_the_storm_measured_on_real_volumes
    /// ```
    ///
    /// A cache directory holds partial downloads and volumes with no
    /// reflectivity in them, so the harness SKIPS what it cannot read and says
    /// how many rather than failing: a test that cannot be pointed at the
    /// local cache is a test nobody reruns.
    ///
    /// It resamples the real 192 x 192 x 48 box at the old default (radar
    /// centre, 120 km) and the new one (auto centre, 60 km) and reports, for
    /// each: the share of voxels carrying data, the share carrying >= 35 dBZ,
    /// the PHYSICAL volume of >= 35 dBZ air (which is the honest comparison,
    /// because the two boxes have different voxel sizes), the height of the
    /// highest 18 dBZ voxel, the share of the volume's whole >= 35 dBZ field
    /// that the box contains, and the strongest echo inside it.
    #[ignore = "set RADAR_L2_SAMPLES to Level II files or a directory to run"]
    #[test]
    fn the_default_box_frames_the_storm_measured_on_real_volumes() {
        use super::super::{BOX_NZ, BOX_TOP_M};
        use render2d::volumetric::{InterpPolicy, volume_box_resample_moment_with_support};

        /// What one box holds.
        struct Held {
            filled_share: f32,
            core_share: f32,
            core_km3: f32,
            top_km: f32,
            /// Share of the volume's own >= 35 dBZ field inside this box.
            field_share: f32,
            /// Strongest composite cell inside it, or -inf when it holds none.
            peak_dbz: f32,
        }

        fn hold(
            volume: &RadarVolume,
            composite: &EchoComposite,
            center: (f32, f32),
            half_km: f32,
        ) -> Held {
            let resampled = volume_box_resample_moment_with_support(
                volume,
                &MomentType::Reflectivity,
                InterpPolicy::LinearAngle,
                center.0,
                center.1,
                half_km,
                BOX_N,
                BOX_NZ,
                BOX_TOP_M,
            )
            .expect("the box resamples");
            let voxels = resampled.values.len() as f32;
            let filled = resampled.support.iter().filter(|s| **s > 0).count() as f32;
            let core = resampled
                .values
                .iter()
                .filter(|v| v.is_finite() && **v >= AUTO_CENTER_CORE_DBZ)
                .count() as f32;
            let dx_km = 2.0 * half_km / BOX_N as f32;
            let dz_km = BOX_TOP_M / 1000.0 / BOX_NZ as f32;
            let level = BOX_N * BOX_N;
            let mut top_level = 0usize;
            for zi in 0..BOX_NZ {
                let slab = &resampled.values[zi * level..(zi + 1) * level];
                if slab.iter().any(|v| v.is_finite() && *v >= 18.0) {
                    top_level = zi;
                }
            }
            let mut peak_dbz = f32::NEG_INFINITY;
            let mut inside = 0usize;
            let mut total = 0usize;
            for (index, value) in composite.max_dbz.iter().enumerate() {
                if !value.is_finite() {
                    continue;
                }
                let (east_km, north_km) = composite.cell_center_km(index);
                let within =
                    (east_km - center.0).abs() <= half_km && (north_km - center.1).abs() <= half_km;
                if within && *value > peak_dbz {
                    peak_dbz = *value;
                }
                if *value >= AUTO_CENTER_CORE_DBZ {
                    total += 1;
                    inside += usize::from(within);
                }
            }
            Held {
                filled_share: 100.0 * filled / voxels,
                core_share: 100.0 * core / voxels,
                core_km3: core * dx_km * dx_km * dz_km,
                top_km: BOX_TOP_M / 1000.0 * top_level as f32 / (BOX_NZ - 1) as f32,
                field_share: 100.0 * inside as f32 / total.max(1) as f32,
                peak_dbz,
            }
        }

        let raw = std::env::var("RADAR_L2_SAMPLES").expect("RADAR_L2_SAMPLES is not set");
        let mut paths: Vec<std::path::PathBuf> = if std::path::Path::new(&raw).is_dir() {
            let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&raw)
                .expect("the sample directory is readable")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            found.sort();
            found
        } else {
            raw.split(';')
                .filter(|part| !part.is_empty())
                .map(std::path::PathBuf::from)
                .collect()
        };
        paths.retain(|path| path.is_file());
        assert!(!paths.is_empty(), "no sample volumes in {raw}");

        /// What one candidate centring rule did over the whole sample, so that
        /// the constants above can be re-derived rather than believed.
        #[derive(Default)]
        struct Tally {
            label: String,
            framed: usize,
            deficit: f32,
            missed: usize,
            jumps: Vec<f32>,
            previous: Option<(String, (f32, f32))>,
        }

        impl Tally {
            fn record(
                &mut self,
                site: &str,
                composite: &EchoComposite,
                volume_peak: f32,
                center: Option<(f32, f32)>,
            ) {
                let Some(center) = center else { return };
                self.framed += 1;
                let mut peak = f32::NEG_INFINITY;
                for (index, value) in composite.max_dbz.iter().enumerate() {
                    let (east_km, north_km) = composite.cell_center_km(index);
                    if value.is_finite()
                        && *value > peak
                        && (east_km - center.0).abs() <= BOX_HALF_KM
                        && (north_km - center.1).abs() <= BOX_HALF_KM
                    {
                        peak = *value;
                    }
                }
                if peak.is_finite() {
                    self.deficit += volume_peak - peak;
                }
                if volume_peak >= 50.0 && peak < 50.0 {
                    self.missed += 1;
                }
                // Consecutive samples from one site only: a jump between two
                // different radars is not a jump.
                if let Some((previous_site, previous)) = &self.previous
                    && previous_site == site
                {
                    self.jumps
                        .push((center.0 - previous.0).hypot(center.1 - previous.1));
                }
                self.previous = Some((site.to_owned(), center));
            }

            fn report(&self) {
                let mut jumps = self.jumps.clone();
                jumps.sort_by(f32::total_cmp);
                println!(
                    "  {:22} framed {:3}  {:5.2} dB below the peak  no 50 dBZ core {:3}  \
                     jump median {:5.1} p90 {:6.1} over 30 km {:3} of {}",
                    self.label,
                    self.framed,
                    self.deficit / self.framed.max(1) as f32,
                    self.missed,
                    jumps.get(jumps.len() / 2).copied().unwrap_or(f32::NAN),
                    jumps.get(jumps.len() * 9 / 10).copied().unwrap_or(f32::NAN),
                    jumps.iter().filter(|jump| **jump > 30.0).count(),
                    jumps.len(),
                );
            }
        }

        /// The constants this module has to justify, each as a rule that
        /// differs from the shipping one in exactly one place.
        fn variants() -> Vec<(Tally, ClusterRule)> {
            let core = ClusterRule::core();
            let named = |label: String, rule: ClusterRule| {
                (
                    Tally {
                        label,
                        ..Tally::default()
                    },
                    rule,
                )
            };
            let weights = [0.0f32, 0.1, 0.25, 0.5, 1.0].map(|weight_per_db| {
                let rule = ClusterRule {
                    weight_per_db,
                    ..core
                };
                named(format!("weight/dB {weight_per_db:4.2}"), rule)
            });
            let floors = [1usize, 4, 8, 12, 24, 48].map(|min_cells| {
                let rule = ClusterRule { min_cells, ..core };
                named(format!("cluster floor {min_cells:3}"), rule)
            });
            let depths = [1u8, 2, 3].map(|min_sweeps| {
                let rule = ClusterRule { min_sweeps, ..core };
                named(format!("elevations {min_sweeps}"), rule)
            });
            weights.into_iter().chain(floors).chain(depths).collect()
        }

        let mut skipped = 0usize;
        let mut declined = 0usize;
        let mut framed = 0usize;
        let mut sweeps = variants();
        let mut ungated_shift = 0.0f32;
        let mut ungated_shifts = 0usize;
        let (mut old_km3, mut new_km3) = (0.0f32, 0.0f32);
        let (mut old_voxels, mut new_voxels) = (0.0f32, 0.0f32);
        let (mut old_share, mut new_share) = (0.0f32, 0.0f32);
        let (mut old_deficit, mut new_deficit) = (0.0f32, 0.0f32);
        let (mut old_deficits, mut new_deficits) = (0usize, 0usize);
        let (mut old_missed, mut new_missed) = (0usize, 0usize);
        let max_center_km = ClusterRule::core().max_center_km;

        for path in &paths {
            let Ok(volume) = nexrad_io::decode_volume_from_path(path) else {
                skipped += 1;
                continue;
            };
            let Some(composite) = echo_composite(
                &volume,
                &MomentType::Reflectivity,
                AUTO_CENTER_MIN_HEIGHT_KM,
            ) else {
                skipped += 1;
                continue;
            };
            let name = format!(
                "{} {}Z",
                volume.site.id,
                volume.volume_time.format("%Y-%m-%d %H:%M")
            );
            let volume_peak = composite
                .max_dbz
                .iter()
                .copied()
                .filter(|value| value.is_finite())
                .fold(f32::NEG_INFINITY, f32::max);
            for (tally, rule) in &mut sweeps {
                let center = composite.strongest_cluster_km(*rule).or_else(|| {
                    composite.strongest_cluster_km(ClusterRule {
                        min_dbz: AUTO_CENTER_ECHO_DBZ,
                        ..*rule
                    })
                });
                tally.record(&volume.site.id, &composite, volume_peak, center);
            }

            let Some(center) = auto_box_center_km(&volume) else {
                declined += 1;
                println!("{name}  nothing worth framing; the box does not move");
                continue;
            };

            // What the height gate is worth on this volume: where the same rule
            // lands when it is allowed to see the lowest tilt.
            if let Some(ground) = echo_composite(&volume, &MomentType::Reflectivity, 0.0)
                && let Some(ungated) = ground.strongest_cluster_km(ClusterRule::core())
            {
                ungated_shift += (center.0 - ungated.0).hypot(center.1 - ungated.1);
                ungated_shifts += 1;
            }

            // The two invariants the rest of the pane relies on, checked on
            // every real volume rather than argued for.
            assert!(
                center.0.is_finite() && center.1.is_finite(),
                "{name}: non-finite centre {center:?}"
            );
            let range_km = center.0.hypot(center.1);
            assert!(
                range_km <= max_center_km + 1.0e-3,
                "{name}: centre {center:?} is {range_km} km out, past the {max_center_km} km limit"
            );
            // And the centre must not depend on being asked twice.
            assert_eq!(auto_box_center_km(&volume), Some(center), "{name}");

            let old = hold(&volume, &composite, (0.0, 0.0), 60.0);
            let new = hold(&volume, &composite, center, BOX_HALF_KM);
            println!(
                "{name}  peak {volume_peak:5.1} dBZ  centre ({:6.1},{:6.1}) {range_km:5.1} km out\n  \
                 old radar 120 km  filled {:6.2}%  >=35 {:5.2}%  {:8.1} km3  top {:5.1} km  \
                 field {:5.1}%  peak {:5.1}\n  \
                 new storm  60 km  filled {:6.2}%  >=35 {:5.2}%  {:8.1} km3  top {:5.1} km  \
                 field {:5.1}%  peak {:5.1}",
                center.0,
                center.1,
                old.filled_share,
                old.core_share,
                old.core_km3,
                old.top_km,
                old.field_share,
                old.peak_dbz,
                new.filled_share,
                new.core_share,
                new.core_km3,
                new.top_km,
                new.field_share,
                new.peak_dbz,
            );

            framed += 1;
            old_km3 += old.core_km3;
            new_km3 += new.core_km3;
            old_voxels += old.core_share;
            new_voxels += new.core_share;
            old_share += old.field_share;
            new_share += new.field_share;
            if old.peak_dbz.is_finite() {
                old_deficit += volume_peak - old.peak_dbz;
                old_deficits += 1;
            }
            if new.peak_dbz.is_finite() {
                new_deficit += volume_peak - new.peak_dbz;
                new_deficits += 1;
            }
            if volume_peak >= 50.0 {
                // `peak_dbz` is a finite cell value or NEG_INFINITY for a box
                // holding no echo at all, and NEG_INFINITY compares less than
                // everything, so the empty box counts as a miss without a
                // separate arm.
                old_missed += usize::from(old.peak_dbz < 50.0);
                new_missed += usize::from(new.peak_dbz < 50.0);
            }
        }

        println!(
            "\n=== {framed} framed, {declined} declined, {skipped} unreadable, of {} ===",
            paths.len()
        );
        assert!(framed > 0, "no volume had anything to frame");
        let per = framed as f32;
        println!(
            "mean >=35 dBZ volume in the box   old {:8.1} km3   new {:8.1} km3   ratio {:5.2}",
            old_km3 / per,
            new_km3 / per,
            new_km3 / old_km3.max(1.0e-6),
        );
        // Printed beside the physical volume above on purpose: the voxel share
        // is the flattering number and it is flattering by a factor of four,
        // because the 60 km box divides the same 192 lattice into 312 m voxels
        // where the 120 km box uses 625 m ones.
        println!(
            "mean >=35 dBZ VOXEL share         old {:8.3}%      new {:8.3}%      ratio {:5.2}",
            old_voxels / per,
            new_voxels / per,
            new_voxels / old_voxels.max(1.0e-6),
        );
        println!(
            "mean share of the >=35 dBZ field  old {:6.1}%       new {:6.1}%",
            old_share / per,
            new_share / per,
        );
        println!(
            "mean dB below the volume's peak   old {:6.2}        new {:6.2}",
            old_deficit / old_deficits.max(1) as f32,
            new_deficit / new_deficits.max(1) as f32,
        );
        println!("volumes whose box holds no 50 dBZ core  old {old_missed}   new {new_missed}",);
        println!(
            "turning the {AUTO_CENTER_MIN_HEIGHT_KM} km height gate off moves the centre \
             {:.1} km on average over {ungated_shifts} volumes",
            ungated_shift / ungated_shifts.max(1) as f32,
        );

        // Where the constants at the top of this module come from. Each row is
        // the shipping rule with one constant changed, so a maintainer can see
        // what the choice bought rather than take the doc comment's word.
        println!("\n=== the constants, re-derived ===");
        for (tally, _) in &sweeps {
            tally.report();
        }

        // The three claims the default rests on. Not the share of the field -
        // the smaller box holds about a point less of it on a widespread event,
        // and BOX_HALF_KM says so.
        // Not "much more": "never less". The ratio is 3.7 over a mixed sample of
        // 55 sites, 1.6 over 139 volumes of one squall-line day, and 1.04 over
        // 87 TDWR volumes, whose whole domain fits inside the old box already.
        // The argument for the new default is the two assertions after this one,
        // not this one - this one only says the shrink costs nothing in
        // aggregate.
        assert!(
            new_km3 >= old_km3,
            "the auto-centred box holds {:.1} km3 of core against the old default's {:.1}",
            new_km3 / per,
            old_km3 / per,
        );
        assert!(
            new_deficit / new_deficits.max(1) as f32 <= old_deficit / old_deficits.max(1) as f32,
            "the auto-centred box frames weaker echo than the old default did",
        );
        assert!(
            new_missed * 3 <= old_missed,
            "the auto-centred box misses the 50 dBZ core on {new_missed} volumes \
             against the old default's {old_missed}",
        );
    }
}
