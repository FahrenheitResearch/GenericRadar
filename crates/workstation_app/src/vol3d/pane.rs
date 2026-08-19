//! The 3D volume explorer's egui shell: toolbar, canvas, camera, and the
//! background resample that feeds the GPU.
//!
//! Deliberately self-contained. Nothing here reaches into `WorkstationApp`;
//! everything it needs arrives in [`Vol3dPaneInput`], and the renderer itself
//! (`super`) depends on nothing of this application beyond `radar_core`, so the
//! pair lifts into a standalone crate by moving two files rather than by
//! untangling them. The upstream version of this shell lives inside a
//! forty-thousand-line `main.rs` woven through that application's state, which
//! is exactly what makes it unportable.
//!
//! # Which volume the box is built from
//!
//! The pane is handed every volume in history and picks the one that will
//! reconstruct best, which is usually NOT the one the 2D panes are showing:
//! live, the displayed volume is the one still arriving, two tilts of a
//! fourteen-tilt VCP that reconstruct to two disconnected shells. The choice
//! goes on distinct commanded tilts ([`tilt_count`]) among the volumes from the
//! same radar in the quarter hour up to the displayed frame
//! ([`choose_volume`]), and the box is refused below
//! [`MIN_TILTS_FOR_A_BOX`].
//!
//! Choosing an older volume is only defensible if the pane says so, so
//! [`provenance`] states what the box was built from on every frame, at the top
//! of the pane body where the operator cannot miss it and where a pane too
//! small for its own toolbar cannot clip it away.

use std::cell::RefCell;
use std::sync::{Arc, mpsc};
use std::thread;

use chrono::{DateTime, Utc};
use color_tables::ColorTable;
use eframe::egui;
use product_engine::VolumeCapabilities;
use radar_core::{MomentType, RadarVolume};
use render2d::volumetric::InterpPolicy;

use super::advanced::{self, SupportMode, Vol3dRenderMode};
use super::annotations;
use super::camera;
use super::controls;
use super::{
    BOX_N, BOX_NZ, BOX_TOP_M, Vol3d, Vol3dCallback, Vol3dThresholdMode, empty_box,
    lowest_moment_floor, normalize_box_with_range,
};

/// Engine range of the REFLECTIVITY structure box in velocity two-box mode:
/// `product_engine`'s declared reflectivity range, which is what a structure
/// plane is normalised against, so the isosurface can be expressed in the units
/// of the field the shader compares it to. See [`structure_range`].
const VELOCITY_STRUCTURE_RANGE_DBZ: (f32, f32) = (-32.0, 94.5);

/// Colour of every line saying the box is not what the rest of the workstation
/// shows. Amber rather than red: the box is honest, it is simply not current,
/// and a red pane reads as a fault.
const STALE_SOURCE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 176, 46);

/// Everything the pane needs from the application, and nothing more.
pub struct Vol3dPaneInput<'a> {
    /// Every volume the box may be built from, in history order (oldest
    /// first); empty draws the empty box and says so. A list rather than one
    /// volume because opening the 3D window two tilts into a fourteen-tilt VCP
    /// used to reconstruct those two tilts, and two tilts reconstruct to two
    /// disconnected shells an analyst cannot tell apart from a storm with two
    /// layers.
    pub candidates: &'a [Vol3dCandidate<'a>],
    /// Which moment the box is built from.
    pub moment: MomentType,
    /// Short product name, for the status line and the resample key.
    pub product_label: String,
    /// Palette for the 256-entry transfer-function lookup table, and the
    /// engine-value range it and the thresholds are normalised against.
    pub color_table: &'a ColorTable,
    pub value_range: (f32, f32),
}

/// One volume the 3D box may be built from.
pub struct Vol3dCandidate<'a> {
    pub volume: &'a Arc<RadarVolume>,
    /// True for the volume the 2D panes are drawing, which anchors the choice.
    /// Exactly one should carry it; none is tolerated, and the pane then says
    /// what it built from without claiming anything about the panes.
    pub displayed: bool,
}

/// The fewest distinct commanded tilts a volume must carry before a 3D box is
/// worth building from it.
///
/// Measured, not chosen.
/// [`tests::the_box_gains_vertical_structure_at_the_fourth_tilt`] reproduces
/// the measurement over every volume in the workstation's own Level II cache -
/// 48 volumes, 43 WSR-88D and 5 TDWR, reflectivity and velocity, 94 series in
/// all. Each is truncated to its lowest N nominal elevation groups, the shape a
/// live volume arrives in because the antenna climbs, and resampled into the
/// 192 x 192 x 48 cell, 18 km tall, 60 km half-width box this pane builds.
/// `filled` is the fraction of voxels that receive a value; `>=3km` is the
/// fraction of occupied ground columns that receive one on at least 8 of the 48
/// levels, about 3 km - the number that says whether there is an interior to
/// look at or only a shell. Medians over every series deep enough to truncate
/// that far, and how many of those have ANY 3 km column:
///
/// ```text
///  tilts   filled    >=3km    series with a 3 km column
///      1    4.02%    0.00%      0 of 94
///      2    5.63%    0.00%      0 of 79
///      3    6.98%    0.00%     11 of 66    9 of the 11 are TDWR
///      4    8.03%    2.39%     54 of 56
///      5    9.18%    7.24%     52 of 52
///      8   12.65%   20.14%     42 of 42
/// ```
///
/// The `filled` curve has no knee in it - it climbs a point or so per tilt all
/// the way - so the cut-off comes from the depth column, which switches. At two
/// tilts or fewer not one ground column in the box reaches 3 km anywhere, in
/// any of the 94 series: what is there is the half-beamwidth extension around
/// one or two cones and nothing else. At three, 17% of series have a 3 km column
/// and nearly all of those are TDWR, whose VCP climbs faster than an 88D's. At
/// four, 96% do. That is the cut-off: four tilts is where the box stops being a
/// stack of shells, and below it what is drawn is the arrival of the volume
/// rather than the weather.
///
/// It is a floor and not a sufficiency test, and depth itself cannot be the
/// shipped test because it does not travel between box sizes: the same 3 km bar
/// needs eight tilts in this 60 km box, where near-range beams are thin and
/// stacked close together, and is cleared by ONE tilt in a 360 km box, where a
/// single beam is already 3.5 km thick and carries no vertical information at
/// all. Tilt count travels, and the pane states the count it used.
const MIN_TILTS_FOR_A_BOX: usize = 4;

/// One volume, measured.
struct MeasuredVolume {
    site: Box<str>,
    volume_time_ms: i64,
    cut_count: usize,
    address: usize,
    capabilities: VolumeCapabilities,
}

/// How many measurements to keep. History is capped at thirty frames, so this
/// holds the candidate list twice over and a live volume can grow through a
/// dozen snapshots without evicting the volume the box is built from.
const MEASURED_VOLUME_MEMO: usize = 64;

thread_local! {
    /// Measurements of the volumes this pane has been offered.
    ///
    /// `VolumeCapabilities::analyze` takes a median over every radial of every
    /// sweep: 186 to 250 microseconds per volume on the cached volumes the tilt
    /// floor was measured from. The pane is offered the whole history - thirty
    /// frames - and chooses on every frame it paints, so measuring them all
    /// each time would cost 5.6 to 7.5 ms of a 16 ms frame, a bigger bill than
    /// the resample this file exists to keep off the frame thread. The
    /// measurement is a pure function of an immutable snapshot, so it is taken
    /// once and kept; thread-local because the pane runs on the UI thread and
    /// the tests run in parallel, and neither wants a lock.
    static MEASURED: RefCell<Vec<MeasuredVolume>> = const { RefCell::new(Vec::new()) };
}

impl MeasuredVolume {
    fn measure(volume: &Arc<RadarVolume>) -> Self {
        Self {
            site: volume.site.id.as_str().into(),
            volume_time_ms: volume.volume_time.timestamp_millis(),
            cut_count: volume.cuts.len(),
            address: Arc::as_ptr(volume) as usize,
            capabilities: VolumeCapabilities::analyze(volume),
        }
    }

    /// Whether this measurement is of `volume`. The address alone would not be
    /// safe to key on - a dropped `Arc` can be replaced at the same address by
    /// an unrelated volume, and the memo would answer for the wrong radar - so
    /// site, volume time and cut count are checked beside it. A volume agreeing
    /// on all four is the same snapshot as far as counting tilts goes, and the
    /// cut count is what notices a volume that has grown a sweep.
    fn matches(&self, volume: &Arc<RadarVolume>) -> bool {
        self.address == Arc::as_ptr(volume) as usize
            && self.volume_time_ms == volume.volume_time.timestamp_millis()
            && self.cut_count == volume.cuts.len()
            && *self.site == *volume.site.id
    }

    /// Distinct commanded tilts that carry `moment`. Per moment, because a tilt
    /// that does not carry the drawn field contributes nothing to reconstructing
    /// it: on a split cut the surveillance leg has no velocity at all, so a
    /// velocity box is built from a shorter stack than a reflectivity box of the
    /// same volume.
    fn tilts_with(&self, moment: &MomentType) -> usize {
        self.capabilities
            .groups
            .iter()
            .filter(|group| {
                group.members.iter().any(|index| {
                    self.capabilities
                        .cut(*index)
                        .is_some_and(|cut| cut.has_moment(moment))
                })
            })
            .count()
    }
}

/// Distinct commanded tilts in `volume` that carry `moment`, measured once.
/// This, and not `volume.cuts.len()`, is what governs a 3D reconstruction.
/// SAILS scans the lowest tilt up to four times and a split cut scans it twice
/// over, so KTLX 07:24:02Z in the cache carries 19 cuts across 10 tilts with
/// EIGHT of those cuts at one elevation. Those repeats buy azimuthal freshness
/// and nothing at all vertically: the box built from all eight is the box built
/// from one, which is the one-tilt row of the table above. Counting cuts would
/// have called those eight a volume worth reconstructing.
///
/// The grouping is `product_engine`'s, which takes the MEDIAN elevation over
/// each sweep rather than the angle stored on the cut: on that volume the
/// stored angles scatter those eight cuts across two apparent tilts near 0.64
/// and 0.48 degrees, a vertical layer that does not exist.
fn tilt_count(volume: &Arc<RadarVolume>, moment: &MomentType) -> usize {
    MEASURED.with(|measured| {
        let mut measured = measured.borrow_mut();
        let position = match measured.iter().position(|entry| entry.matches(volume)) {
            Some(position) => position,
            None => {
                if measured.len() >= MEASURED_VOLUME_MEMO {
                    measured.remove(0);
                }
                measured.push(MeasuredVolume::measure(volume));
                measured.len() - 1
            }
        };
        measured[position].tilts_with(moment)
    })
}

/// The volume the box is built from, and the measurement that chose it.
struct Vol3dChoice<'a> {
    volume: &'a Arc<RadarVolume>,
    tilts: usize,
}

/// How far back in time the box may reach for a deeper volume, in seconds.
///
/// "Most tilts wins" on its own has no clock in it: a VCP change out of a
/// precipitation pattern leaves a 14-tilt volume in history against the 7-tilt
/// volumes now arriving, and the deeper one would hold the box until history
/// evicted it - thirty frames at a clear-air volume every ten minutes is five
/// hours of the wrong weather. The window is one volume interval with margin,
/// so the box can always reach the volume BEFORE the one arriving and never
/// past a whole scan strategy: the slowest WSR-88D patterns, VCP 31 and 32,
/// take 10 minutes, VCP 35 about 7, and the precipitation patterns 12, 212 and
/// 215 take 4.5 to 6 (NWS Radar Operations Center, *WSR-88D Volume Coverage
/// Patterns*). After a VCP change the box lags by at most this long, saying so
/// in amber, and then follows the new pattern.
const MAX_LOOKBACK_SECONDS: i64 = 15 * 60;

/// Choose the candidate that will reconstruct best.
///
/// The frame the 2D panes are drawing anchors the search: only volumes from the
/// same radar, at or before that frame's time, within [`MAX_LOOKBACK_SECONDS`]
/// of it, are eligible. Among those the deepest wins, because tilt count is what
/// the vertical walk in `render2d::volumetric` has to work with - a complete
/// volume five minutes old carries more structure than the two tilts of the one
/// arriving now. Recency breaks a tie and list position breaks the last one, so
/// the answer is deterministic and the box cannot flip between two equally deep
/// volumes for ever.
///
/// Each filter is a defect this pane had: a deeper volume from the next radar
/// over won the box and drew a different storm over different ground; pausing
/// playback on an older frame built the box out of the operator's future; an
/// hour-old volume outranked the complete volume on screen.
fn choose_volume<'a>(
    candidates: &'a [Vol3dCandidate<'a>],
    moment: &MomentType,
) -> Option<Vol3dChoice<'a>> {
    // No candidate marked `displayed` is tolerated: the newest volume is then
    // what the operator is most likely looking at.
    let anchor = candidates
        .iter()
        .find(|candidate| candidate.displayed)
        .or_else(|| {
            candidates
                .iter()
                .max_by_key(|candidate| candidate.volume.volume_time)
        })?;
    let mut best: Option<(usize, usize, &Vol3dCandidate<'_>)> = None;
    for (order, candidate) in candidates.iter().enumerate() {
        if !candidate
            .volume
            .site
            .id
            .eq_ignore_ascii_case(&anchor.volume.site.id)
        {
            continue;
        }
        let age = (anchor.volume.volume_time - candidate.volume.volume_time).num_seconds();
        if !(0..=MAX_LOOKBACK_SECONDS).contains(&age) {
            continue;
        }
        let tilts = tilt_count(candidate.volume, moment);
        let better = match best {
            None => true,
            Some((best_order, best_tilts, best_candidate)) => {
                (tilts, candidate.volume.volume_time, order)
                    > (best_tilts, best_candidate.volume.volume_time, best_order)
            }
        };
        if better {
            best = Some((order, tilts, candidate));
        }
    }
    // The anchor itself always clears the three filters, so this is `Some`
    // whenever the list is not empty.
    best.map(|(_, tilts, candidate)| Vol3dChoice {
        volume: candidate.volume,
        tilts,
    })
}

/// Map a threshold in engine units onto the shader's 0..1 domain.
///
/// `Outside` is two-sided and needs both bounds; every other mode uses a single
/// bound and passes -1 for the unused one, which the shader reads as "absent"
/// rather than as a real threshold at the bottom of the scale.
fn shader_threshold_bounds(
    mode: Vol3dThresholdMode,
    threshold: f32,
    value_min: f32,
    value_max: f32,
) -> (f32, f32) {
    match mode {
        Vol3dThresholdMode::Outside => (
            normalized_value(-threshold.abs(), value_min, value_max),
            normalized_value(threshold.abs(), value_min, value_max),
        ),
        _ => (normalized_value(threshold, value_min, value_max), -1.0),
    }
}

fn normalized_value(value: f32, value_min: f32, value_max: f32) -> f32 {
    let span = (value_max - value_min).abs().max(f32::EPSILON);
    ((value - value_min) / span).clamp(0.0, 1.0)
}

/// The interpolation guard a moment needs when it is resampled vertically.
/// Velocity and correlation coefficient are not safely averaged across a fold
/// or across a hail shaft, so each gets its own guard rather than the linear
/// path reflectivity uses.
fn interp_policy(moment: &MomentType) -> InterpPolicy {
    match moment {
        MomentType::Velocity => InterpPolicy::VelocityGuard,
        MomentType::CorrelationCoefficient => InterpPolicy::CcGuard,
        _ => InterpPolicy::LinearAngle,
    }
}

/// Push a 256-entry transfer-function LUT to the GPU when the palette changes,
/// keyed on the table's own signature so an unchanged palette does not queue an
/// upload every frame.
fn update_lut(vol3d: &mut Vol3d, table: &ColorTable, value_min: f32, value_max: f32) {
    let signature = table.signature();
    if signature == vol3d.lut_signature {
        return;
    }
    let mut lut = vec![0_u8; 256 * 4];
    for (index, pixel) in lut.chunks_exact_mut(4).enumerate() {
        let fraction = index as f32 / 255.0;
        let value = value_min + (value_max - value_min) * fraction;
        pixel.copy_from_slice(&table.color_for_value(value));
    }
    if let Ok(mut pending) = vol3d.pending.lock() {
        pending.lut = Some(lut.clone());
        vol3d.lut_signature = signature;
        // The preintegration table integrates this palette over a segment, so
        // it needs the sampled bytes rather than the `ColorTable` they came
        // from. Kept here so the two can never disagree about which palette is
        // on the GPU.
        vol3d.lut_rgba = lut;
    }
}

/// Engine-value range of the box the shader samples as `t_volume` - the
/// STRUCTURE field, which is not always the field the palette paints. In
/// velocity two-box mode `t_volume` carries reflectivity while the palette
/// carries m/s, and normalising a 45 dBZ isosurface against a -100..100 m/s
/// range would place it at 0.725 of the reflectivity ramp, around 58 dBZ. The
/// isosurface slider and both arguments to `shader_uniforms` therefore take
/// this range, never the palette's.
fn structure_range(vol3d: &Vol3d, value_range: (f32, f32)) -> (f32, f32) {
    if vol3d.velocity_color_active {
        VELOCITY_STRUCTURE_RANGE_DBZ
    } else {
        value_range
    }
}

/// Rebuild the segment-preintegration table when the palette, the thresholds or
/// the opacity change - and at no other time. Camera motion is not an input to
/// the signature, so orbiting rebuilds nothing; neither does the box, which is
/// why this cache is separate from the spatial hierarchy's.
fn update_preintegration(vol3d: &mut Vol3d, value_min: f32, value_max: f32) {
    if vol3d.lut_rgba.len() != 256 * 4 {
        return;
    }
    let (low, high) = shader_threshold_bounds(
        vol3d.threshold_mode,
        vol3d.threshold_dbz,
        value_min,
        value_max,
    );
    let mode = vol3d.threshold_mode.shader_value();
    let signature =
        advanced::preintegration_signature(&vol3d.lut_rgba, low, high, mode, vol3d.opacity);
    if signature == vol3d.preintegration_signature {
        return;
    }
    let table = advanced::build_preintegrated_lut(&vol3d.lut_rgba, low, high, mode, vol3d.opacity);
    if let Ok(mut pending) = vol3d.pending.lock() {
        pending.preintegrated = Some(table);
        vol3d.preintegration_signature = signature;
    }
}

/// Identity of the box currently uploaded. The source volume's pointer and its
/// top elevation are both in it, so a live volume that grows another tilt
/// rebuilds rather than showing the box built from the fragment that arrived
/// first. Choosing between candidates adds NOTHING, deliberately: the choice
/// can only express itself as a different volume, and the volume is already
/// three of these fields, so a fresher but thinner frame arriving in history
/// leaves the key untouched and rebuilds nothing - which
/// `a_thinner_new_frame_arriving_is_not_chosen_and_does_not_change_the_key`
/// pins.
fn resample_key(
    volume: &Arc<RadarVolume>,
    product_label: &str,
    top_deg: f32,
    half_km: f32,
    center_east_km: f32,
    center_north_km: f32,
) -> super::Vol3dVolumeKey {
    // Tenths of a km: finer than a voxel at every box size, so any centre move
    // that could change a voxel rebuilds the box, and one that cannot, cannot.
    let (center_east, center_north) = super::box_center_key(center_east_km, center_north_km);
    (
        volume.site.id.clone(),
        product_label.to_owned(),
        volume.volume_time.timestamp_millis(),
        Arc::as_ptr(volume) as usize,
        (top_deg * 10.0) as i32,
        center_east,
        center_north,
        half_km as i32,
    )
}

/// What the box currently on the GPU was built from.
///
/// Read out of the resample key rather than out of a fresh choice, because the
/// key IS the identity of the uploaded box: a choice made this frame may still
/// be resampling on a worker, and a line describing it would name a volume that
/// is not on screen yet.
struct BoxSource {
    site: String,
    address: usize,
    /// `None` only if the stored millisecond stamp is outside the calendar,
    /// which no decoded volume produces; the line then says so rather than
    /// inventing a time.
    volume_time: Option<DateTime<Utc>>,
    /// Tilt count of that volume, measured from the candidate it came from.
    /// `None` once the volume has aged out of history - the pane then names it
    /// without claiming a depth it can no longer measure.
    tilts: Option<usize>,
}

fn box_source(
    vol3d: &Vol3d,
    candidates: &[Vol3dCandidate<'_>],
    moment: &MomentType,
) -> Option<BoxSource> {
    let key = vol3d.volume_key.as_ref()?;
    let address = key.3;
    let volume_time_ms = key.2;
    Some(BoxSource {
        site: key.0.clone(),
        address,
        volume_time: DateTime::<Utc>::from_timestamp_millis(volume_time_ms),
        // Address AND volume time: an address alone can outlive the `Arc` it
        // came from and be handed out again for another volume, and the
        // disclosure would quote the depth of a volume nobody is looking at.
        tilts: candidates
            .iter()
            .find(|candidate| {
                Arc::as_ptr(candidate.volume) as usize == address
                    && candidate.volume.volume_time.timestamp_millis() == volume_time_ms
            })
            .map(|candidate| tilt_count(candidate.volume, moment)),
    })
}

/// The volume the 2D panes are drawing, as far as the disclosure needs it.
struct DisplayedVolume {
    address: usize,
    volume_time: DateTime<Utc>,
    tilts: usize,
}

fn displayed_volume(
    candidates: &[Vol3dCandidate<'_>],
    moment: &MomentType,
) -> Option<DisplayedVolume> {
    let candidate = candidates.iter().find(|candidate| candidate.displayed)?;
    Some(DisplayedVolume {
        address: Arc::as_ptr(candidate.volume) as usize,
        volume_time: candidate.volume.volume_time,
        tilts: tilt_count(candidate.volume, moment),
    })
}

/// A gap in time, written the way an operator reads a clock.
fn age_label(seconds: i64) -> String {
    let seconds = seconds.abs();
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min {} s", seconds / 60, seconds % 60)
    } else {
        format!("{} h {} min", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// One line of the provenance block, and whether it is a warning.
#[derive(Debug)]
struct ProvenanceLine {
    text: String,
    warn: bool,
}

/// What the pane says about where its box came from. Pure, so the words an
/// operator reads are testable without a GPU. Two lines at most: what the box
/// on screen is, and - only when the pane is refusing to build a better one -
/// why it is not.
fn provenance_lines(
    source: Option<&BoxSource>,
    displayed: Option<&DisplayedVolume>,
    best_tilts: Option<usize>,
    moment: &MomentType,
) -> Vec<ProvenanceLine> {
    let line = |text: String, warn: bool| ProvenanceLine { text, warn };
    let mut lines = Vec::new();
    match source {
        None if displayed.is_none() => lines.push(line(
            "No volume loaded: the 3D box is empty.".to_owned(),
            false,
        )),
        None => lines.push(line("No 3D box built yet.".to_owned(), false)),
        Some(source) => {
            let when = source.volume_time.map_or_else(
                || "unknown time".to_owned(),
                |time| time.format("%H:%M:%SZ").to_string(),
            );
            let depth = source.tilts.map_or_else(
                || "tilt count no longer measurable".to_owned(),
                |tilts| format!("{tilts} tilts"),
            );
            let head = format!("3D box: {} {when}, {depth}", source.site);
            match displayed {
                // Pointer equality, not volume time: a partial volume that was
                // later replaced by the completed one shares its time, and the
                // box built from the partial is genuinely not the volume the 2D
                // panes are drawing.
                Some(displayed) if displayed.address == source.address => lines.push(line(
                    format!("{head} - the volume the 2D panes are showing."),
                    false,
                )),
                Some(displayed) => {
                    let gap = match source
                        .volume_time
                        .map(|time| (displayed.volume_time - time).num_seconds())
                    {
                        Some(seconds) if seconds > 0 => format!(", {} older", age_label(seconds)),
                        Some(seconds) if seconds < 0 => format!(", {} NEWER", age_label(seconds)),
                        _ => ", a different snapshot of the same volume time".to_owned(),
                    };
                    lines.push(line(
                        format!(
                            "{head} - NOT the volume the 2D panes are showing ({}, {} tilts){gap}.",
                            displayed.volume_time.format("%H:%M:%SZ"),
                            displayed.tilts
                        ),
                        true,
                    ));
                }
                None => lines.push(line(format!("{head}."), false)),
            }
        }
    }
    if let Some(tilts) = best_tilts
        && tilts < MIN_TILTS_FOR_A_BOX
    {
        lines.push(line(
            format!(
                concat!(
                    "Not building a box: the fullest volume available carries {} {} of {}, ",
                    "and below {} tilts no ground column in the box reaches 3 km of depth."
                ),
                tilts,
                if tilts == 1 { "tilt" } else { "tilts" },
                moment,
                MIN_TILTS_FOR_A_BOX
            ),
            true,
        ));
    }
    lines
}

/// Say what the box was built from, on screen, on every frame.
///
/// Not a tooltip, not inside one of the collapsible control groups, and not
/// below anything that can grow: a 3D structure five minutes older than the 2D
/// panes is a different storm, and an operator who has to go looking will not
/// go looking.
fn provenance(
    vol3d: &Vol3d,
    ui: &mut egui::Ui,
    input: &Vol3dPaneInput<'_>,
    choice: Option<&Vol3dChoice<'_>>,
) {
    for line in provenance_lines(
        box_source(vol3d, input.candidates, &input.moment).as_ref(),
        displayed_volume(input.candidates, &input.moment).as_ref(),
        choice.map(|choice| choice.tilts),
        &input.moment,
    ) {
        let text = egui::RichText::new(line.text);
        ui.label(if line.warn {
            text.strong().color(STALE_SOURCE_COLOR)
        } else {
            text.weak().small()
        });
    }
}

/// Start a background resample when the chosen volume or the box geometry
/// changes.
fn drive_resample(
    vol3d: &mut Vol3d,
    input: &Vol3dPaneInput<'_>,
    choice: Option<&Vol3dChoice<'_>>,
    ctx: &egui::Context,
) {
    // Drain a finished resample first, so a completed box is uploaded on the
    // frame it arrives rather than one frame later.
    if let Some(receiver) = &vol3d.resample_rx
        && let Ok(result) = receiver.try_recv()
    {
        vol3d.resample_rx = None;
        match result {
            Some(volume_box) => {
                // Telemetry, not a claim: the fraction of fine bricks the
                // traverser may skip depends on the scene, so it is reported per
                // volume rather than asserted once.
                let empty = volume_box
                    .acceleration
                    .as_ref()
                    .map_or(0.0, |acceleration| acceleration.empty_fine_fraction);
                vol3d.status = format!(
                    "{} volume ready ({:.0}% empty bricks)",
                    input.product_label,
                    empty * 100.0
                );
                if let Ok(mut pending) = vol3d.pending.lock() {
                    pending.volume = Some(volume_box);
                }
                ctx.request_repaint();
            }
            None => {
                vol3d.status = "no data in this box".to_owned();
                if let Ok(mut pending) = vol3d.pending.lock() {
                    pending.volume = Some(empty_box());
                }
            }
        }
    }

    let Some(choice) = choice else {
        vol3d.status = "no volume loaded".to_owned();
        return;
    };
    let volume = choice.volume;
    if vol3d.resample_rx.is_some() {
        return;
    }

    let top_deg = volume
        .cuts
        .iter()
        .filter(|cut| cut.moments.contains_key(&input.moment))
        .map(|cut| cut.elevation_deg)
        .fold(0.0_f32, f32::max);
    if top_deg <= 0.0 {
        vol3d.status = format!("no {} in this volume", input.moment);
        return;
    }

    // Refuse before keying: a box this thin is not a reconstruction, and drawing
    // it would put the arrival of the volume on screen in the shape of weather.
    // Whatever box is already uploaded stays up, and the provenance line says
    // how old it is. The measurement is in [`MIN_TILTS_FOR_A_BOX`].
    if choice.tilts < MIN_TILTS_FOR_A_BOX {
        vol3d.status = format!(
            "{} of {MIN_TILTS_FOR_A_BOX} tilts needed for a {} box",
            choice.tilts, input.product_label
        );
        return;
    }

    // Where the box sits, resolved before the key because the centre IS part of
    // the key. Cached against the volume identity inside `resolve_box_center`,
    // so this is a lookup on every frame but the first of a new volume; the
    // scan behind it walks every gate of every tilt (5 to 7 ms measured).
    // Placed after the tilt floor on purpose: a two-tilt fragment must not get
    // to pick where the box goes.
    let (center_east_km, center_north_km) = vol3d.resolve_box_center(volume);
    let key = resample_key(
        volume,
        &input.product_label,
        top_deg,
        vol3d.box_half_km,
        center_east_km,
        center_north_km,
    );
    if vol3d.volume_key.as_ref() == Some(&key) {
        return;
    }

    // The wait below is about ONE volume growing, so its high-water mark belongs
    // to that volume and product alone; moving to a different volume time - a
    // scrub back, a VCP change to a shallower pattern, the deep volume ageing
    // out of history - retires it. Without this the pane latches on the tallest
    // volume it ever saw, refuses every shallower one for ever, and reports
    // "volume building" at a volume that is already complete.
    if vol3d
        .volume_key
        .as_ref()
        .is_none_or(|previous| (&previous.0, &previous.1, previous.2) != (&key.0, &key.1, key.2))
    {
        vol3d.last_top_deg = 0.0;
    }

    // A live volume that has not yet reached the height of the last complete
    // box would replace a full storm with a shallow slice of one. Wait instead.
    if top_deg + 0.3 < vol3d.last_top_deg {
        vol3d.status = format!(
            "volume building (top {top_deg:.1} deg / {:.1} deg)...",
            vol3d.last_top_deg
        );
        return;
    }

    vol3d.volume_key = Some(key);
    vol3d.last_top_deg = top_deg.max(vol3d.last_top_deg.min(20.0));
    vol3d.status = format!("resampling {}...", input.product_label);

    let (sender, receiver) = mpsc::channel();
    vol3d.resample_rx = Some(receiver);

    let volume = Arc::clone(volume);
    let moment = input.moment.clone();
    let policy = interp_policy(&moment);
    let half_km = vol3d.box_half_km;
    let (value_min, value_max) = input.value_range;
    let ctx = ctx.clone();

    // On a worker: a full box is 192 x 192 x 48 cells resampled from every tilt,
    // far too much to do between two frames. The support plane comes out of the
    // same walk as the values, so support 0 is exactly where the MRMS edge rule
    // declined to produce one - the authoritative no-data mask.
    thread::spawn(move || {
        let resampled = render2d::volumetric::volume_box_resample_moment_with_support(
            &volume,
            &moment,
            policy,
            center_east_km,
            center_north_km,
            half_km,
            BOX_N,
            BOX_NZ,
            BOX_TOP_M,
        );
        let result = resampled.map(|resampled| {
            let mut volume_box =
                normalize_box_with_range(&resampled.values, BOX_N, BOX_NZ, value_min, value_max);
            // Same worker, because the hierarchy is a pure function of this box
            // and the frame thread must never build it. `volume_box.data` IS the
            // structure plane - reflectivity in velocity two-box mode, never the
            // velocity - so geometry, opacity, hierarchy and support agree.
            volume_box.acceleration = Some(advanced::build_box_acceleration(
                &volume_box.data,
                &resampled.support,
            ));
            if let Some((floor_data, elevation_deg)) = lowest_moment_floor(
                &volume,
                &moment,
                center_east_km,
                center_north_km,
                half_km,
                value_min,
                value_max,
            ) {
                volume_box.floor_data = Some(floor_data);
                volume_box.floor_elevation_deg = Some(elevation_deg);
            }
            volume_box
        });
        let _ = sender.send(result);
        ctx.request_repaint();
    });
}

/// Draw the whole 3D pane into `ui`.
pub fn draw_vol3d_pane(vol3d: &mut Vol3d, ui: &mut egui::Ui, input: &Vol3dPaneInput<'_>) {
    let (value_min, value_max) = input.value_range;
    let context = ui.ctx().clone();
    update_lut(vol3d, input.color_table, value_min, value_max);
    update_preintegration(vol3d, value_min, value_max);
    // Chosen once and handed to both, so the line the operator reads and the
    // box the worker builds can never come from two different scans of the
    // candidate list.
    let choice = choose_volume(input.candidates, &input.moment);
    drive_resample(vol3d, input, choice.as_ref(), &context);

    // ABOVE the toolbar. An egui window can be dragged smaller than its own
    // contents and what does not fit is clipped away, not scrolled to: at
    // 330 x 120 the wrapped toolbar fills the pane on its own, and drawn under
    // it the line saying the box is not the volume on screen was the first
    // thing to vanish. Measured in
    // `the_disclosure_survives_a_pane_too_small_for_its_own_toolbar`.
    provenance(vol3d, ui, input, choice.as_ref());
    toolbar(vol3d, ui, value_min, value_max);
    ui.separator();
    canvas(vol3d, ui, value_min, value_max);
}

fn toolbar(vol3d: &mut Vol3d, ui: &mut egui::Ui, value_min: f32, value_max: f32) {
    ui.horizontal_wrapped(|ui| {
        ui.menu_button("Presets", |ui| {
            ui.set_min_width(170.0);
            for (label, apply) in [
                ("Balanced", Vol3d::apply_balanced_preset as fn(&mut Vol3d)),
                ("Storm structure", Vol3d::apply_structure_preset),
                ("Core isolation", Vol3d::apply_core_preset),
            ] {
                if ui.button(label).clicked() {
                    apply(vol3d);
                    ui.close();
                }
            }

            // The four analysis presets each pair a render mode with the
            // support presentation that mode needs. Beam-support inspection is
            // the one that MUST be paired: choosing that render mode on its own
            // leaves "Fade weak support" selected, and at the default 0.18 floor
            // the fade erases the cone of silence, the wide tilt gaps and the
            // top extrapolation - exactly the anatomy VERIFY.md's
            // meteorological-honesty gate asks an operator to go and look at.
            // The Analysis panel opens with them so the operator can see which
            // controls the preset moved, not only that the picture changed.
            ui.separator();
            type Preset = fn(&mut advanced::AdvancedParams);
            for (label, apply) in [
                (
                    "Direct volume",
                    advanced::AdvancedParams::apply_volume_preset as Preset,
                ),
                (
                    "Hybrid shell",
                    advanced::AdvancedParams::apply_hybrid_preset,
                ),
                ("Isosurface", advanced::AdvancedParams::apply_surface_preset),
                (
                    "Beam-support inspection",
                    advanced::AdvancedParams::apply_support_preset,
                ),
            ] {
                if ui.button(label).clicked() {
                    apply(&mut vol3d.advanced);
                    vol3d.show_analysis_controls = true;
                    ui.close();
                }
            }
        });

        if ui
            .selectable_label(vol3d.show_volume_controls, "Volume")
            .clicked()
        {
            vol3d.show_volume_controls = !vol3d.show_volume_controls;
        }
        if ui
            .selectable_label(vol3d.show_lighting_controls, "Lighting")
            .clicked()
        {
            vol3d.show_lighting_controls = !vol3d.show_lighting_controls;
        }
        if ui
            .selectable_label(vol3d.show_analysis_controls, "Analysis")
            .clicked()
        {
            vol3d.show_analysis_controls = !vol3d.show_analysis_controls;
        }

        ui.menu_button("View", |ui| {
            ui.set_min_width(260.0);
            if ui.button("Reset camera").clicked() {
                vol3d.reset_camera();
                ui.close();
            }
            if ui.button("Top-down").clicked() {
                vol3d.top_view();
                ui.close();
            }
            if ui.button("Low angle").clicked() {
                vol3d.low_view();
                ui.close();
            }
            ui.separator();
            ui.add(
                egui::Slider::new(&mut vol3d.vertical_exaggeration, 0.5..=6.0)
                    .suffix("x")
                    .text("vertical exaggeration"),
            );
            ui.add(egui::Slider::new(&mut vol3d.fov_scale, 0.42..=1.1).text("field of view"));
            ui.checkbox(&mut vol3d.show_grid, "Floor grid");
            ui.checkbox(&mut vol3d.show_box, "Bounding box");
            ui.checkbox(&mut vol3d.show_labels, "Height scale and compass");
        });

        // Which camera flies the box: Orbit/Fly toggle, fly speed, Recenter.
        // This is the entry point `camera.rs` shipped without - the module
        // doc calls Fly "the reason a second one was asked for", and until
        // this call it was unreachable from any control.
        camera::camera_controls(ui, vol3d);

        controls::box_geometry(ui, vol3d);

        ui.separator();
        ui.weak(&vol3d.status);
    });

    if vol3d.show_volume_controls {
        ui.horizontal_wrapped(|ui| {
            let modes = [
                (Vol3dThresholdMode::Above, "Above"),
                (Vol3dThresholdMode::Below, "Below"),
                (Vol3dThresholdMode::Outside, "Outside"),
            ];
            let selected = modes
                .iter()
                .find(|(mode, _)| *mode == vol3d.threshold_mode)
                .map_or("Above", |(_, label)| *label);
            egui::ComboBox::from_id_salt("vol3d-threshold-mode")
                .selected_text(selected)
                .width(96.0)
                .show_ui(ui, |ui| {
                    for (mode, label) in modes {
                        ui.selectable_value(&mut vol3d.threshold_mode, mode, label);
                    }
                });
            ui.add(
                egui::Slider::new(&mut vol3d.threshold_dbz, value_min..=value_max)
                    .text("threshold"),
            );
            ui.add(egui::Slider::new(&mut vol3d.opacity, 0.02..=1.0).text("opacity"));
            ui.add(egui::Slider::new(&mut vol3d.density, 0.2..=4.0).text("density"));
        });
    }

    if vol3d.show_lighting_controls {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Slider::new(&mut vol3d.shading, 0.0..=1.0).text("shading"));
            ui.add(
                egui::Slider::new(&mut vol3d.lighting.light_azimuth_deg, 0.0..=360.0)
                    .suffix(" deg")
                    .text("key azimuth"),
            );
            ui.add(
                egui::Slider::new(&mut vol3d.lighting.light_elevation_deg, 0.0..=89.0)
                    .suffix(" deg")
                    .text("key elevation"),
            );
            ui.add(
                egui::Slider::new(&mut vol3d.lighting.ambient_strength, 0.0..=2.0).text("ambient"),
            );
            ui.add(egui::Slider::new(&mut vol3d.lighting.key_strength, 0.0..=2.0).text("key"));
            ui.add(
                egui::Slider::new(&mut vol3d.lighting.shadow_strength, 0.0..=1.0).text("shadow"),
            );
        });
    }

    if vol3d.show_floor_controls {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Slider::new(&mut vol3d.floor_opacity, 0.0..=1.0).text("floor opacity"));
        });
    }

    if vol3d.show_analysis_controls {
        analysis_controls(vol3d, ui, value_min, value_max);
    }
}

/// Render mode, support presentation, the horizontal crop box, and the support
/// disclosure.
///
/// Everything here is uniform-only state: none of it can reach the hierarchy
/// builder, so none of it rebuilds the hierarchy.
fn analysis_controls(vol3d: &mut Vol3d, ui: &mut egui::Ui, value_min: f32, value_max: f32) {
    // The isosurface slider runs over the STRUCTURE field's units -
    // reflectivity in velocity two-box mode - and not over the palette's.
    let (structure_min, structure_max) = structure_range(vol3d, (value_min, value_max));

    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("vol3d-render-mode")
            .selected_text(vol3d.advanced.render_mode.label())
            .width(190.0)
            .show_ui(ui, |ui| {
                for mode in Vol3dRenderMode::ALL {
                    ui.selectable_value(&mut vol3d.advanced.render_mode, mode, mode.label());
                }
            });
        egui::ComboBox::from_id_salt("vol3d-support-mode")
            .selected_text(vol3d.advanced.support_mode.label())
            .width(190.0)
            .show_ui(ui, |ui| {
                for mode in SupportMode::ALL {
                    ui.selectable_value(&mut vol3d.advanced.support_mode, mode, mode.label());
                }
            });
        ui.checkbox(&mut vol3d.advanced.preintegration, "Preintegrate segments");
        // Contract 8: the fixed-step, no-hierarchy path stays reachable until
        // GPU captures prove the accelerated traversal image-equivalent.
        ui.checkbox(&mut vol3d.advanced.reference_path, "Reference path (A/B)");
    });

    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::Slider::new(&mut vol3d.advanced.iso_value, structure_min..=structure_max)
                .text("isosurface"),
        );
        ui.add(
            egui::Slider::new(
                &mut vol3d.advanced.iso_width,
                0.1..=((structure_max - structure_min).abs() * 0.2).max(0.2),
            )
            .text("shell width"),
        );
        ui.add(egui::Slider::new(&mut vol3d.advanced.support_floor, 0.0..=0.95).text("fade below"));
        ui.add(egui::Slider::new(&mut vol3d.advanced.support_fade, 0.2..=3.0).text("fade power"));
    });

    // The reflectivity opacity ramp: opacity as a function of the VALUE, so a
    // core reads as a solid body and weak echo as cloud. The knees are in the
    // STRUCTURE field's dBZ, and the ramp is inert unless that field IS
    // reflectivity - see `advanced::AdvancedParams::packed_ramp_scale`.
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::Slider::new(&mut vol3d.advanced.opacity_ramp_low_dbz, -30.0..=40.0)
                .text("cloud edge dBZ"),
        );
        ui.add(
            egui::Slider::new(&mut vol3d.advanced.opacity_ramp_high_dbz, 40.0..=80.0)
                .text("solid core dBZ"),
        );
        ui.add(egui::Slider::new(&mut vol3d.advanced.opacity_ramp_gain, 1.0..=12.0).text("body"));
        ui.add(egui::Slider::new(&mut vol3d.advanced.opacity_ramp_gamma, 1.0..=6.0).text("focus"));
        ui.add(egui::Slider::new(&mut vol3d.advanced.opacity_ramp_floor, 0.0..=1.0).text("haze"));
    });

    ui.horizontal_wrapped(|ui| {
        ui.label("crop");
        ui.add(egui::Slider::new(&mut vol3d.advanced.crop_x_min, 0.0..=0.99).text("west"));
        ui.add(egui::Slider::new(&mut vol3d.advanced.crop_x_max, 0.01..=1.0).text("east"));
        ui.add(egui::Slider::new(&mut vol3d.advanced.crop_y_min, 0.0..=0.99).text("south"));
        ui.add(egui::Slider::new(&mut vol3d.advanced.crop_y_max, 0.01..=1.0).text("north"));
        if ui.button("Whole box").clicked() {
            vol3d.advanced.crop_x_min = 0.0;
            vol3d.advanced.crop_x_max = 1.0;
            vol3d.advanced.crop_y_min = 0.0;
            vol3d.advanced.crop_y_max = 1.0;
        }
    });

    // Contract 4, verbatim: support says which beams passed near a voxel and
    // nothing about calibration, blockage, contamination or dealiasing, and
    // paraphrasing it is how a display aid turns into a claim.
    ui.label(
        egui::RichText::new(advanced::SUPPORT_DISCLOSURE)
            .weak()
            .small(),
    );
}

fn canvas(vol3d: &mut Vol3d, ui: &mut egui::Ui, value_min: f32, value_max: f32) {
    let available = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_rgb(5, 7, 11));

    // The one camera authority: `super::camera` turns this frame's pointer,
    // wheel and keyboard input into camera state for both modes. It replaces
    // the inline drag/scroll handler this canvas carried, whose wheel clamped
    // `dist` to [0.35, 6.0] while the renderer floors the radius at
    // `orbit_distance()` - 1.655 on the default box - so about fifteen wheel
    // notches changed the number and moved nothing, and Fly mode had no entry
    // point at all. The repaint on movement is what makes a HELD key fly: a
    // held key is not an event, so without it the camera would take one step
    // per keystroke.
    if camera::drive_camera(camera::FlyInput {
        vol3d,
        response: &response,
        dt: camera::frame_dt(ui.ctx()),
    }) {
        ui.ctx().request_repaint();
    }
    response.context_menu(|ui| {
        if ui.button("Reset view").clicked() {
            vol3d.reset_camera();
            ui.close();
        }
        if ui.button("Top view").clicked() {
            vol3d.top_view();
            ui.close();
        }
        if ui.button("Low-angle view").clicked() {
            vol3d.low_view();
            ui.close();
        }
        ui.separator();
        ui.checkbox(&mut vol3d.show_grid, "Floor grid");
        ui.checkbox(&mut vol3d.show_box, "Bounding box");
        ui.checkbox(&mut vol3d.show_labels, "Height scale and compass");
    });

    let (clip_low, clip_high) = vol3d.normalized_clip();
    let (threshold01, threshold_high01) = shader_threshold_bounds(
        vol3d.threshold_mode,
        vol3d.threshold_dbz,
        value_min,
        value_max,
    );
    let (floor_threshold01, floor_threshold_high01) = shader_threshold_bounds(
        vol3d.floor_threshold_mode,
        vol3d.floor_threshold_dbz,
        value_min,
        value_max,
    );
    let (structure_min, structure_max) = structure_range(vol3d, (value_min, value_max));

    ui.painter()
        .add(eframe::egui_wgpu::Callback::new_paint_callback(
            rect,
            Vol3dCallback {
                yaw: vol3d.yaw,
                pitch: vol3d.pitch,
                dist: vol3d.orbit_distance(),
                camera_mode: vol3d.camera_mode,
                fly_x: vol3d.fly_x,
                fly_y: vol3d.fly_y,
                fly_z: vol3d.fly_z,
                threshold01,
                threshold_high01,
                threshold_mode: vol3d.threshold_mode,
                opacity: vol3d.opacity,
                aspect: (rect.width() / rect.height().max(1.0)).max(0.1),
                floor_opacity: vol3d.floor_opacity,
                floor_mode: vol3d.floor_mode,
                zspan: vol3d.zspan(),
                fov_scale: vol3d.fov_scale,
                quality: vol3d.quality,
                density: vol3d.density,
                shading: vol3d.shading,
                lighting: vol3d.lighting,
                clip_low,
                clip_high,
                floor_threshold01,
                floor_threshold_high01,
                floor_threshold_mode: vol3d.floor_threshold_mode,
                focus_height: vol3d.focus_height_fraction(),
                // Single-box only: the workstation does not yet resample the
                // reflectivity structure alongside a velocity colour box.
                velocity_mode: 0.0,
                ref_gate: 0.0,
                couplet_emphasis: 0.0,
                // STRUCTURE range on both arguments: the shader compares the
                // isosurface against a `t_volume` sample, which is reflectivity
                // in velocity two-box mode even though the palette is m/s.
                advanced: vol3d.advanced.shader_uniforms(
                    structure_min,
                    structure_max,
                    vol3d.velocity_color_active,
                ),
                pending: Arc::clone(&vol3d.pending),
            },
        ));

    // The rect is both the shader callback's rect and the painter's clip rect:
    // the first makes `project_point` agree with the WGSL camera, the second is
    // what keeps a zoomed-in height ladder off the panel beside the pane.
    annotations::draw(vol3d, &ui.painter_at(rect), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_sided_threshold_leaves_its_upper_bound_absent() {
        // -1 is the shader's "no upper bound" sentinel. Passing 0.0 instead
        // would read as a real threshold at the bottom of the scale and hide the
        // whole volume. `Outside` is the mode that really has two bounds.
        let (low, high) = shader_threshold_bounds(Vol3dThresholdMode::Above, 35.0, -32.0, 94.5);
        assert!((low - 0.5296).abs() < 1e-3, "low bound was {low}");
        assert_eq!(high, -1.0);
        let (low, high) = shader_threshold_bounds(Vol3dThresholdMode::Outside, 20.0, -100.0, 100.0);
        assert!((low - 0.4).abs() < 1e-6, "low was {low}");
        assert!((high - 0.6).abs() < 1e-6, "high was {high}");
    }

    #[test]
    fn a_value_outside_the_range_is_clamped_and_a_degenerate_range_is_finite() {
        assert_eq!(normalized_value(-50.0, -32.0, 94.5), 0.0);
        assert_eq!(normalized_value(200.0, -32.0, 94.5), 1.0);
        let normalized = normalized_value(10.0, 10.0, 10.0);
        assert!(normalized.is_finite(), "got {normalized}");
    }

    #[test]
    fn the_isosurface_is_normalised_against_the_structure_field_not_the_palette() {
        // The shader compares `iso_value` against a `t_volume` sample, which in
        // velocity two-box mode is REFLECTIVITY while the palette is m/s: the
        // palette range would put a 45 dBZ shell at (45 + 100) / 200 = 0.725 of
        // the reflectivity ramp, roughly 58 dBZ, and the surface an operator
        // asked for would not be the surface drawn.
        let mut vol3d = Vol3d {
            advanced: advanced::AdvancedParams {
                iso_value: 45.0,
                iso_width: 2.0,
                ..advanced::AdvancedParams::default()
            },
            ..Vol3d::default()
        };
        // Single box: the structure field is the displayed moment itself.
        assert_eq!(structure_range(&vol3d, (0.0, 80.0)), (0.0, 80.0));

        // Two box: the palette range is velocity, the structure range is not.
        vol3d.velocity_color_active = true;
        let (low, high) = structure_range(&vol3d, (-100.0, 100.0));
        assert_eq!((low, high), VELOCITY_STRUCTURE_RANGE_DBZ);
        let uniforms = vol3d
            .advanced
            .shader_uniforms(low, high, vol3d.velocity_color_active);
        let slot = advanced::ADVANCED_UNIFORM_FIELDS
            .iter()
            .position(|field| *field == "iso_value")
            .expect("the field exists");
        let expected = (45.0 - low) / (high - low);
        assert!(
            (uniforms[slot] - expected).abs() < 1e-6,
            "{}",
            uniforms[slot]
        );
        // And it is nowhere near where the velocity range would have put it.
        assert!((uniforms[slot] - 0.725).abs() > 0.05);
    }

    #[test]
    fn the_support_disclosure_reaches_the_operator_verbatim() {
        // Contract 4. The one sentence pair is a single const so it cannot drift
        // between call sites; this fails if the pane starts paraphrasing it.
        let source = include_str!("pane.rs");
        assert!(source.contains("advanced::SUPPORT_DISCLOSURE"));
        assert!(
            advanced::SUPPORT_DISCLOSURE.contains("not official radar QC")
                && advanced::SUPPORT_DISCLOSURE.contains("not a formal uncertainty")
        );
    }

    #[test]
    fn velocity_and_correlation_get_guarded_interpolation_but_reflectivity_does_not() {
        // Averaging across a velocity fold, or across the correlation drop at a
        // hail shaft, invents values that never existed in the sweep. `assert!`
        // rather than `assert_eq!` because `InterpPolicy` is a verbatim BowEcho
        // type and does not derive Debug.
        assert!(interp_policy(&MomentType::Velocity) == InterpPolicy::VelocityGuard);
        assert!(interp_policy(&MomentType::CorrelationCoefficient) == InterpPolicy::CcGuard);
        assert!(interp_policy(&MomentType::Reflectivity) == InterpPolicy::LinearAngle);
    }

    #[test]
    fn choosing_beam_support_inspection_also_turns_the_fade_off() {
        // `apply_support_preset` pairs `SupportInspection` with `Inspect`, and
        // the pairing matters: at the default 0.18 floor `HonestFade` erases the
        // cone of silence, the tilt gaps and the top extrapolation - the exact
        // anatomy VERIFY.md's honesty gate sends an operator to inspect.
        // `advanced.rs` allows dead code, so an unwired preset warns nobody.
        let mut params = advanced::AdvancedParams::default();
        assert_eq!(params.support_mode, SupportMode::HonestFade);
        params.apply_support_preset();
        assert_eq!(params.render_mode, Vol3dRenderMode::SupportInspection);
        assert_eq!(params.support_mode, SupportMode::Inspect);

        let source = include_str!("pane.rs");
        for preset in [
            "apply_volume_preset",
            "apply_hybrid_preset",
            "apply_surface_preset",
            "apply_support_preset",
        ] {
            assert!(source.contains(preset), "{preset} is unreachable");
        }
    }

    // --- choosing which volume to reconstruct -------------------------------

    /// A sweep at a commanded tilt, with the antenna ramp real volumes have:
    /// the stored angle is the first radial's, 0.30 degrees below the commanded
    /// tilt, while the median over the sweep is the commanded tilt. Grouping on
    /// the stored angle would split sweeps that grouping on the median keeps
    /// together, which is why the tilt count comes from `product_engine`.
    fn sweep(
        commanded_deg: f32,
        elevation_number: u8,
        start_ms: i32,
        moments: &[MomentType],
    ) -> radar_core::ElevationCut {
        let gates = || radar_core::GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count: 100,
        };
        let first = commanded_deg - 0.30;
        let mut cut = radar_core::ElevationCut::new(first, Some(elevation_number));
        for index in 0..36 {
            // The antenna is still climbing for the first four radials.
            let elevation = first + (commanded_deg - first) * (index.min(4) as f32 / 4.0);
            cut.radials.push(radar_core::Radial {
                azimuth_deg: index as f32 * 10.0,
                elevation_deg: elevation,
                time_offset_ms: start_ms + index * 10,
                gate_range: gates(),
                nyquist_velocity_mps: Some(26.0),
                radial_status: None,
            });
        }
        for moment in moments {
            let grid = radar_core::MomentGrid::new_u8(
                moment.clone(),
                gates(),
                2.0,
                66.0,
                Some(0),
                Some(1),
            );
            cut.moments.insert(moment.clone(), grid);
        }
        cut
    }

    /// One sweep per commanded tilt, every one carrying `moments`.
    fn stack(tilts: &[f32], moments: &[MomentType]) -> Vec<radar_core::ElevationCut> {
        tilts
            .iter()
            .enumerate()
            .map(|(index, tilt)| sweep(*tilt, index as u8 + 1, index as i32 * 20_000, moments))
            .collect()
    }

    /// 08:34:00Z plus `second`, which may run past a minute so volumes an hour
    /// apart are as easy to write as volumes thirty seconds apart.
    fn at(second: i64) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 18, 8, 34, 0).unwrap()
            + chrono::TimeDelta::seconds(second)
    }

    fn volume(site: &str, second: i64, cuts: Vec<radar_core::ElevationCut>) -> Arc<RadarVolume> {
        let mut volume = RadarVolume::new(radar_core::RadarSite::new(site), at(second));
        volume.cuts = cuts;
        Arc::new(volume)
    }

    /// One sweep per commanded tilt in `tilts`, all carrying reflectivity.
    fn scan(site: &str, second: i64, tilts: &[f32]) -> Arc<RadarVolume> {
        volume(site, second, stack(tilts, &[MomentType::Reflectivity]))
    }

    fn as_candidates(volumes: &[Arc<RadarVolume>], displayed: usize) -> Vec<Vol3dCandidate<'_>> {
        volumes
            .iter()
            .enumerate()
            .map(|(index, volume)| Vol3dCandidate {
                volume,
                displayed: index == displayed,
            })
            .collect()
    }

    /// What the pane would build the box from, with `displayed` marking the
    /// frame the 2D panes are drawing.
    fn chosen(volumes: &[Arc<RadarVolume>], displayed: usize) -> Arc<RadarVolume> {
        let candidates = as_candidates(volumes, displayed);
        let choice = choose_volume(&candidates, &MomentType::Reflectivity).expect("a choice");
        Arc::clone(choice.volume)
    }

    #[test]
    fn sails_repeats_do_not_make_a_volume_look_deeper_than_it_is() {
        // Eight cuts at one commanded tilt - a SAILSx3 volume's four low-level
        // scans, each split into a surveillance and a Doppler leg - against
        // three cuts at three tilts. Counting cuts takes the eight; counting
        // commanded tilts takes the three, because eight scans of one cone
        // reconstruct what one scan of it reconstructs. KTLX 07:24:02Z in the
        // cache is this volume: 19 cuts, 10 tilts, 8 cuts at 0.5 degrees.
        let mut repeated = Vec::new();
        for repeat in 0..4 {
            repeated.push(sweep(0.48, 1, repeat * 60_000, &[MomentType::Reflectivity]));
            repeated.push(sweep(
                0.48,
                2,
                repeat * 60_000 + 20_000,
                &[MomentType::Reflectivity, MomentType::Velocity],
            ));
        }
        let flat = volume("KTLX", 1, repeated);
        let layered = scan("KTLX", 31, &[0.5, 1.5, 2.4]);
        assert_eq!(flat.cuts.len(), 8);
        assert_eq!(layered.cuts.len(), 3);
        assert_eq!(tilt_count(&flat, &MomentType::Reflectivity), 1);
        assert_eq!(tilt_count(&layered, &MomentType::Reflectivity), 3);
        let volumes = [flat, Arc::clone(&layered)];
        assert!(
            Arc::ptr_eq(&chosen(&volumes, 1), &layered),
            "eight cuts of one tilt beat three tilts"
        );
    }

    #[test]
    fn recency_breaks_a_tie_between_two_equally_deep_volumes() {
        let tilts = [0.5, 1.5, 2.4, 3.4, 4.3];
        let older = scan("KABR", 1, &tilts);
        let newer = scan("KABR", 31, &tilts);
        let forwards = [Arc::clone(&older), Arc::clone(&newer)];
        assert!(Arc::ptr_eq(&chosen(&forwards, 1), &newer));
        // And the answer is the volume time, not the position in the list: the
        // same two handed over the other way round, the same one displayed,
        // gives the same answer.
        let backwards = [Arc::clone(&newer), older];
        assert!(Arc::ptr_eq(&chosen(&backwards, 0), &newer));
    }

    #[test]
    fn a_tilt_that_does_not_carry_the_drawn_moment_is_not_counted() {
        // A split volume: the two lowest tilts are scanned twice, once for
        // reflectivity and once for velocity, and everything above them is a
        // single sweep carrying reflectivity only. Reflectivity has five tilts
        // to reconstruct from; velocity has two, and a velocity box built as if
        // it had five would claim a stack that was never measured.
        let mut cuts = vec![
            sweep(0.5, 1, 0, &[MomentType::Reflectivity]),
            sweep(0.5, 2, 20_000, &[MomentType::Velocity]),
            sweep(0.9, 3, 40_000, &[MomentType::Reflectivity]),
            sweep(0.9, 4, 60_000, &[MomentType::Velocity]),
        ];
        cuts.extend(stack(&[1.5, 2.4, 3.4], &[MomentType::Reflectivity]));
        let split = volume("KDMX", 1, cuts);
        assert_eq!(tilt_count(&split, &MomentType::Reflectivity), 5);
        assert_eq!(tilt_count(&split, &MomentType::Velocity), 2);
    }

    #[test]
    fn one_radar_at_or_before_this_frame_and_not_from_an_hour_ago() {
        const DEEP: [f32; 8] = [0.5, 0.9, 1.3, 1.8, 2.4, 3.1, 4.0, 5.1];
        const THIN: [f32; 4] = [0.5, 0.9, 1.3, 1.8];

        // The radar next door is a different storm over different ground, and
        // the box is drawn around the site of whatever volume built it.
        let here = scan("KEAX", 31, &THIN);
        let volumes = [scan("KDMX", 1, &DEEP), Arc::clone(&here)];
        assert!(Arc::ptr_eq(&chosen(&volumes, 1), &here));

        // Paused on an older frame: a deeper volume that arrived AFTER it is
        // the operator's future, and the box must not be built from there.
        let paused = scan("KEAX", 1, &THIN);
        let volumes = [Arc::clone(&paused), scan("KEAX", 59, &DEEP)];
        assert!(Arc::ptr_eq(&chosen(&volumes, 0), &paused));

        // An hour-old volume cannot hold the box against the complete volume on
        // screen however deep it is - a VCP change to a shallower pattern is
        // this shape, and it used to freeze the box for hours.
        let current = scan("KEAX", 3_601, &THIN);
        let volumes = [scan("KEAX", 1, &DEEP), Arc::clone(&current)];
        assert!(Arc::ptr_eq(&chosen(&volumes, 1), &current));

        // Ten minutes back is inside the window: that is the slowest WSR-88D
        // volume interval, and the box must always reach the volume before the
        // one still arriving.
        let previous = scan("KEAX", 3_001, &DEEP);
        let volumes = [Arc::clone(&previous), current];
        assert!(Arc::ptr_eq(&chosen(&volumes, 1), &previous));
    }

    #[test]
    fn a_candidate_list_with_nothing_in_it_builds_nothing() {
        assert!(choose_volume(&[], &MomentType::Reflectivity).is_none());
        // Volumes that decoded to no cuts at all: a choice is still made, it
        // carries no tilts, and the pane refuses out loud rather than drawing an
        // empty box as though it were weather.
        let volumes = [
            volume("KOAX", 1, Vec::new()),
            volume("KOAX", 31, Vec::new()),
        ];
        let candidates = as_candidates(&volumes, 1);
        let choice = choose_volume(&candidates, &MomentType::Reflectivity).expect("a choice");
        assert_eq!(choice.tilts, 0);
        let refusal = lines_for(None, None, Some(0));
        let last = &refusal.last().expect("a line").text;
        assert!(last.contains("Not building"), "{last:?}");
    }

    #[test]
    fn a_thinner_new_frame_arriving_is_not_chosen_and_does_not_change_the_key() {
        // The live case this mechanism exists for, and the contract that keeps
        // it cheap: the newest frame is the volume still arriving, six commanded
        // tilts against two, so the older volume is what gets reconstructed -
        // and because the choice can only express itself as a volume, and the
        // volume is already three fields of the key, the thinner frame landing
        // in history leaves the key byte for byte identical.
        let deep = scan("KDMX", 1, &[0.5, 0.9, 1.5, 2.4, 3.4, 4.3]);
        let arriving = scan("KDMX", 31, &[0.5, 0.9]);
        assert_eq!(tilt_count(&deep, &MomentType::Reflectivity), 6);
        assert_eq!(tilt_count(&arriving, &MomentType::Reflectivity), 2);
        let one_frame = [Arc::clone(&deep)];
        let before = resample_key(&chosen(&one_frame, 0), "REF", 4.3, 60.0, 0.0, 0.0);
        let two_frames = [Arc::clone(&deep), arriving];
        assert!(Arc::ptr_eq(&chosen(&two_frames, 1), &deep));
        let after = resample_key(&chosen(&two_frames, 1), "REF", 4.3, 60.0, 0.0, 0.0);
        assert_eq!(before, after);
    }

    #[test]
    fn the_memo_notices_a_volume_that_grew_even_if_the_allocator_reuses_the_address() {
        // The memo is keyed on the `Arc` address, which the allocator may hand out
        // again once the first volume is dropped; site, volume time and cut
        // count are checked beside it so a re-used address cannot serve the old
        // answer. Dropping the small volume first invites exactly that.
        let small = scan("KOAX", 1, &[0.5, 0.9]);
        assert_eq!(tilt_count(&small, &MomentType::Reflectivity), 2);
        drop(small);
        let grown = scan("KOAX", 1, &[0.5, 0.9, 1.5, 2.4, 3.4]);
        assert_eq!(tilt_count(&grown, &MomentType::Reflectivity), 5);
        // Twice, because the second answer comes from the memo.
        assert_eq!(tilt_count(&grown, &MomentType::Reflectivity), 5);
    }

    // --- saying what it chose ----------------------------------------------

    fn box_from(site: &str, address: usize, second: i64, tilts: Option<usize>) -> BoxSource {
        BoxSource {
            site: site.to_owned(),
            address,
            volume_time: Some(at(second)),
            tilts,
        }
    }

    fn panes_show(address: usize, second: i64, tilts: usize) -> DisplayedVolume {
        DisplayedVolume {
            address,
            volume_time: at(second),
            tilts,
        }
    }

    fn lines_for(
        source: Option<&BoxSource>,
        displayed: Option<&DisplayedVolume>,
        best_tilts: Option<usize>,
    ) -> Vec<ProvenanceLine> {
        provenance_lines(source, displayed, best_tilts, &MomentType::Reflectivity)
    }

    #[test]
    fn the_disclosure_names_the_older_volume_its_time_and_its_tilt_count() {
        // 08:40:00 minus 08:34:01 is 359 seconds: 5 minutes 59 seconds.
        let source = box_from("KDMX", 1, 1, Some(12));
        let displayed = panes_show(2, 360, 3);
        let lines = lines_for(Some(&source), Some(&displayed), Some(12));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].warn, "a stale 3D box is a warning, not a footnote");
        let fragments = ["KDMX", "08:34:01Z", "12 tilts", "08:40:00Z", "3 tilts"];
        for fragment in fragments.into_iter().chain([
            "NOT the volume the 2D panes are showing",
            "5 min 59 s older",
        ]) {
            assert!(
                lines[0].text.contains(fragment),
                "{fragment:?} missing from {:?}",
                lines[0].text
            );
        }
    }

    #[test]
    fn the_disclosure_is_quiet_when_the_box_and_the_panes_agree() {
        let source = box_from("KABR", 7, 1, Some(11));
        let displayed = panes_show(7, 1, 11);
        let lines = lines_for(Some(&source), Some(&displayed), Some(11));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!lines[0].warn);
        assert!(
            lines[0]
                .text
                .contains("the volume the 2D panes are showing")
        );
        assert!(lines[0].text.contains("11 tilts"));
    }

    #[test]
    fn a_replaced_snapshot_of_the_same_volume_time_is_still_declared() {
        // A partial volume that history later replaced with the completed one
        // shares its volume time. The box built from the partial is genuinely
        // not what the 2D panes are drawing, and matching on time rather than on
        // the pointer would have called them the same.
        let source = box_from("KTLX", 1, 1, Some(4));
        let displayed = panes_show(2, 1, 11);
        let lines = lines_for(Some(&source), Some(&displayed), Some(11));
        assert!(lines[0].warn);
        assert!(
            lines[0]
                .text
                .contains("a different snapshot of the same volume time"),
            "{:?}",
            lines[0].text
        );
    }

    #[test]
    fn a_volume_too_thin_to_reconstruct_is_refused_out_loud() {
        // Three tilts is the top of the measured dead zone: no ground column
        // anywhere in the box reaches 3 km of depth on any 88D volume tested.
        let lines = lines_for(None, None, Some(3));
        let refusal = lines.last().expect("a refusal line");
        assert!(refusal.warn);
        for fragment in ["3 tilts", "3 km of depth", "Not building a box"] {
            assert!(
                refusal.text.contains(fragment),
                "{fragment:?} missing from {:?}",
                refusal.text
            );
        }
        // Singular reads as English, because an operator reads it.
        let one = lines_for(None, None, Some(1));
        assert!(one.last().expect("a line").text.contains("1 tilt of"));
        // And at the floor nothing is refused.
        let allowed = lines_for(None, None, Some(MIN_TILTS_FOR_A_BOX));
        assert_eq!(allowed.len(), 1, "{allowed:?}");
        assert!(!allowed[0].warn);
    }

    #[test]
    fn a_box_whose_volume_has_aged_out_of_history_does_not_claim_a_depth() {
        let source = box_from("KEAX", 99, 1, None);
        let lines = lines_for(Some(&source), None, Some(9));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].text.contains("tilt count no longer measurable"),
            "{:?}",
            lines[0].text
        );
    }

    #[test]
    fn a_clock_gap_reads_the_way_an_operator_reads_a_clock() {
        assert_eq!(age_label(47), "47 s");
        assert_eq!(age_label(359), "5 min 59 s");
        assert_eq!(age_label(3725), "1 h 2 min");
        assert_eq!(age_label(-359), "5 min 59 s");
    }

    #[test]
    fn the_box_source_is_read_back_out_of_the_key_of_the_box_on_screen() {
        let deep = scan("KDMX", 1, &[0.5, 0.9, 1.5, 2.4, 3.4, 4.3]);
        let volumes = [Arc::clone(&deep)];
        let candidates = as_candidates(&volumes, 0);
        let vol3d = Vol3d {
            volume_key: Some(resample_key(&deep, "REF", 4.3, 60.0, 0.0, 0.0)),
            ..Vol3d::default()
        };
        let source = box_source(&vol3d, &candidates, &MomentType::Reflectivity).expect("a source");
        assert_eq!(source.site, "KDMX");
        assert_eq!(source.tilts, Some(6));
        let when = source.volume_time.expect("a time").format("%H:%M:%SZ");
        assert_eq!(when.to_string(), "08:34:01Z");
        // With no box uploaded there is nothing to describe.
        assert!(box_source(&Vol3d::default(), &candidates, &MomentType::Reflectivity).is_none());
    }

    /// One headless pane pass at a given size, returning every text run painted
    /// wholly inside its clip rectangle - what an operator can actually read.
    /// Twice round, because egui measures on the first pass and paints what it
    /// measured on the second.
    fn pane_pass(
        vol3d: &mut Vol3d,
        candidates: &[Vol3dCandidate<'_>],
        table: &ColorTable,
        size: (f32, f32),
    ) -> Vec<(String, bool)> {
        let context = egui::Context::default();
        let mut texts = Vec::new();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(size.0, size.1),
                )),
                ..Default::default()
            };
            let output = context.run_ui(raw, |ui| {
                draw_vol3d_pane(
                    vol3d,
                    ui,
                    &Vol3dPaneInput {
                        candidates,
                        moment: MomentType::Reflectivity,
                        product_label: "REF".to_owned(),
                        color_table: table,
                        value_range: (-32.0, 94.5),
                    },
                );
            });
            texts.clear();
            for clipped in &output.shapes {
                collect_text(&clipped.shape, clipped.clip_rect, &mut texts);
            }
        }
        texts
    }

    fn collect_text(shape: &egui::Shape, clip: egui::Rect, out: &mut Vec<(String, bool)>) {
        match shape {
            egui::Shape::Text(text) => {
                let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                if clip.contains_rect(rect) {
                    out.push((text.galley.text().to_owned(), text.galley.elided));
                }
            }
            egui::Shape::Vec(shapes) => shapes
                .iter()
                .for_each(|shape| collect_text(shape, clip, out)),
            _ => {}
        }
    }

    #[test]
    fn the_disclosure_survives_a_pane_too_small_for_its_own_toolbar() {
        // An egui window can be dragged smaller than its contents, and what does
        // not fit is clipped away rather than scrolled to: drawn under the
        // toolbar, this line was measurably gone at 330 x 120 while every
        // toolbar button was still on screen.
        let volumes = [
            scan("KDMX", 1, &[0.5, 0.9, 1.3, 1.8, 2.4, 3.1]),
            scan("KDMX", 31, &[0.5, 0.9]),
        ];
        let candidates = as_candidates(&volumes, 1);
        let table = color_tables::builtin_reflectivity_table();
        for size in [(900.0, 620.0), (420.0, 260.0), (330.0, 120.0)] {
            let mut vol3d = Vol3d::default();
            let texts = pane_pass(&mut vol3d, &candidates, &table, size);
            let shown = "NOT the volume the 2D panes are showing";
            let line = texts.iter().find(|(text, _)| text.contains(shown));
            let (text, elided) =
                line.unwrap_or_else(|| panic!("the disclosure was clipped at {size:?}: {texts:?}"));
            assert!(!elided, "the disclosure was cut short at {size:?}: {text}");
        }
    }

    #[test]
    fn nothing_the_operator_touches_rebuilds_the_box() {
        let deep = scan("KDMX", 1, &[0.5, 0.9, 1.3, 1.8, 2.4, 3.1]);
        let volumes = [Arc::clone(&deep), scan("KDMX", 31, &[0.5, 0.9])];
        let candidates = as_candidates(&volumes, 1);
        let reflectivity = color_tables::builtin_reflectivity_table();
        let velocity = color_tables::builtin_velocity_table();
        let mut vol3d = Vol3d::default();

        // The first pass builds the box, from the deep volume rather than the two
        // tilts the 2D panes show. Then wait for the worker, so what follows
        // cannot be quiet merely because one was already in flight.
        let _ = pane_pass(&mut vol3d, &candidates, &reflectivity, (900.0, 620.0));
        let key = vol3d.volume_key.clone().expect("a box was built");
        assert_eq!(key.3, Arc::as_ptr(&deep) as usize);
        for _ in 0..50 {
            if vol3d.resample_rx.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = pane_pass(&mut vol3d, &candidates, &reflectivity, (900.0, 620.0));
        }
        assert!(vol3d.resample_rx.is_none(), "the resample never landed");

        // Everything an operator moves between frames: camera, opacity,
        // threshold, isosurface, and the palette itself. None of it is an input
        // to the reconstruction, so none of it may start one.
        for step in 0..4 {
            vol3d.yaw += 0.4;
            vol3d.pitch += 0.05;
            vol3d.dist *= 0.97;
            vol3d.opacity += 0.03;
            vol3d.threshold_dbz += 2.0;
            vol3d.advanced.iso_value += 1.0;
            let table = [&velocity, &reflectivity][step % 2];
            let _ = pane_pass(&mut vol3d, &candidates, table, (900.0, 620.0));
            assert_eq!(
                vol3d.volume_key.as_ref(),
                Some(&key),
                "the box was rebuilt on step {step}"
            );
            assert!(
                vol3d.resample_rx.is_none(),
                "a resample was started on step {step}"
            );
        }

        // But when the deep volume leaves history the box DOES rebuild from the
        // shallower one left behind. The top-elevation wait used to latch on the
        // tallest volume ever seen and refuse every shallower one for ever.
        let volumes = [scan("KDMX", 601, &[0.5, 0.9, 1.3, 1.8])];
        let candidates = as_candidates(&volumes, 0);
        let _ = pane_pass(&mut vol3d, &candidates, &reflectivity, (900.0, 620.0));
        let rebuilt = vol3d.volume_key.clone().expect("a key");
        assert_eq!(
            rebuilt.3,
            Arc::as_ptr(&volumes[0]) as usize,
            "never rebuilt from the only volume left: {:?}",
            vol3d.status
        );
    }

    // --- the camera is wired, not just written ------------------------------

    /// One headless pass of the real `canvas`, with this frame's input events.
    fn canvas_frame(ctx: &egui::Context, vol3d: &mut Vol3d, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let _ = ctx.run_ui(raw, |ui| canvas(vol3d, ui, -32.0, 94.5));
    }

    /// §2.8: the canvas feeds its wheel to `camera::drive_camera`, so zooming
    /// in stops AT the radius the renderer will actually use instead of at the
    /// old 0.35 clamp ~15 dead notches below it.
    #[test]
    fn the_canvas_wheel_zooms_to_the_renderers_floor_with_no_dead_notches() {
        let ctx = egui::Context::default();
        let mut vol3d = Vol3d::default();
        // First pass lays the canvas out; hover resolves on the second.
        canvas_frame(&ctx, &mut vol3d, Vec::new());
        canvas_frame(
            &ctx,
            &mut vol3d,
            vec![egui::Event::PointerMoved(egui::pos2(200.0, 150.0))],
        );
        let floor = (vol3d.zspan() * 0.45 + 1.25).clamp(0.35, 6.0);
        assert!(
            vol3d.dist > floor,
            "the default orbit must start above the floor for this test to bite"
        );

        let wheel = || egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 50.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        };
        let mut previous = vol3d.dist;
        for _ in 0..40 {
            canvas_frame(&ctx, &mut vol3d, vec![wheel()]);
            assert!(
                vol3d.dist >= floor - 1e-4,
                "the wheel zoomed below the radius the renderer uses: {} < {floor}",
                vol3d.dist
            );
            assert!(
                vol3d.dist <= previous + 1e-6,
                "zooming in moved the eye out"
            );
            previous = vol3d.dist;
        }
        // Flush egui's scroll smoothing, then: forty notches in lands ON the
        // floor, and the renderer's radius agrees with the stored one - no
        // band of notches that changes the number but not the eye.
        for _ in 0..10 {
            canvas_frame(&ctx, &mut vol3d, Vec::new());
        }
        assert!(
            (vol3d.dist - floor).abs() < 1e-3,
            "forty notches must land on the floor, not the old 0.35 clamp: {}",
            vol3d.dist
        );
        assert!((vol3d.orbit_distance() - vol3d.dist).abs() < 1e-3);
    }

    /// §2.8: Fly mode has an on-screen entry point. `camera_controls` is
    /// tested in `camera.rs`; this pins that the pane actually SURFACES it,
    /// which is the half that shipped missing.
    #[test]
    fn the_fly_toggle_is_on_the_pane_toolbar() {
        let volumes = [scan("KDMX", 1, &[0.5, 0.9, 1.3, 1.8, 2.4, 3.1])];
        let candidates = as_candidates(&volumes, 0);
        let table = color_tables::builtin_reflectivity_table();
        let mut vol3d = Vol3d::default();
        let texts = pane_pass(&mut vol3d, &candidates, &table, (900.0, 620.0));
        for label in ["Orbit", "Fly"] {
            assert!(
                texts.iter().any(|(text, _)| text == label),
                "no {label:?} toggle on the toolbar: {texts:?}"
            );
        }
    }

    // --- the measurement behind the tilt floor ------------------------------

    /// Filled levels that count as a column with vertical extent. The box is 48
    /// levels over 18 km, so 8 levels is about 3 km.
    const DEEP_LEVELS: u32 = 8;

    /// Box half-width the tilt floor was measured at, km. Pinned rather than
    /// read from `BOX_HALF_KM`: the threshold is a statement about one geometry
    /// and would stop reproducing that measurement if the pane's default box
    /// size moved underneath it.
    const MEASURED_HALF_KM: f32 = 60.0;

    /// Whether this commanded tilt carries `moment` at all.
    fn group_has(capabilities: &VolumeCapabilities, group: usize, moment: &MomentType) -> bool {
        capabilities.groups[group].members.iter().any(|index| {
            capabilities
                .cut(*index)
                .is_some_and(|cut| cut.has_moment(moment))
        })
    }

    /// Resample `volume` truncated to its lowest `tilts` commanded tilts that
    /// carry `moment` - the shape a live volume arrives in, because the antenna
    /// climbs - and report the fraction of box voxels that received a value and
    /// the fraction of occupied ground columns that received one on
    /// `DEEP_LEVELS` levels or more.
    fn box_fill(
        volume: &RadarVolume,
        capabilities: &VolumeCapabilities,
        moment: &MomentType,
        tilts: usize,
    ) -> (f64, f64) {
        // `groups` is in ascending elevation order, so the lowest `tilts` of
        // them that carry the moment are the volume as it would have arrived.
        let keep: std::collections::BTreeSet<usize> = (0..capabilities.groups.len())
            .filter(|group| group_has(capabilities, *group, moment))
            .take(tilts)
            .flat_map(|group| capabilities.groups[group].members.iter().copied())
            .collect();
        let mut truncated = volume.clone();
        truncated.cuts = volume
            .cuts
            .iter()
            .enumerate()
            .filter(|(index, _)| keep.contains(index))
            .map(|(_, cut)| cut.clone())
            .collect();
        let Some(resampled) = render2d::volumetric::volume_box_resample_moment_with_support(
            &truncated,
            moment,
            interp_policy(moment),
            0.0,
            0.0,
            MEASURED_HALF_KM,
            BOX_N,
            BOX_NZ,
            BOX_TOP_M,
        ) else {
            return (0.0, 0.0);
        };
        // Layout, from `volume_box_resample_moment_with_support`:
        // `values[z * n * n + y * n + x]`, so `index % (n * n)` is the ground
        // column a voxel stands in whatever level it is on.
        let mut levels = vec![0_u32; BOX_N * BOX_N];
        let mut filled = 0_usize;
        for (index, value) in resampled.values.iter().enumerate() {
            if value.is_finite() {
                filled += 1;
                levels[index % (BOX_N * BOX_N)] += 1;
            }
        }
        let occupied = levels.iter().filter(|count| **count > 0).count().max(1) as f64;
        let deep = levels.iter().filter(|count| **count >= DEEP_LEVELS).count() as f64;
        (
            filled as f64 / resampled.values.len() as f64,
            deep / occupied,
        )
    }

    /// The measurement [`MIN_TILTS_FOR_A_BOX`] is taken from, on real data.
    ///
    /// Ignored by default because it needs radars on disk: point
    /// `NEXRAD_LEVEL2_CACHE` at a directory of Archive II files. It prints the
    /// table tabulated on the constant and checks the claims under it - at two
    /// tilts or fewer NO ground column reaches 3 km, both measures rise with
    /// tilt count so a deeper volume is never a worse reconstruction, and at the
    /// floor almost every series does have a 3 km column (54 of 56, the switch
    /// the threshold sits on). That last is aggregate because it is not
    /// universal: KRAX 07:17:27Z has none even at four tilts.
    #[ignore = "set NEXRAD_LEVEL2_CACHE to a directory of real Archive II volumes"]
    #[test]
    fn the_box_gains_vertical_structure_at_the_fourth_tilt() {
        const MAX_TILTS: usize = 8;
        let directory =
            std::env::var("NEXRAD_LEVEL2_CACHE").expect("set NEXRAD_LEVEL2_CACHE to a directory");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
            .expect("the cache directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "{directory} holds no volumes");

        let mut fills = vec![Vec::<f64>::new(); MAX_TILTS + 1];
        let mut deeps = vec![Vec::<f64>::new(); MAX_TILTS + 1];
        let mut any_deep = [0_usize; MAX_TILTS + 1];
        for path in paths {
            let volume = nexrad_io::decode_volume_from_path(&path)
                .unwrap_or_else(|error| panic!("{} did not decode: {error}", path.display()));
            let capabilities = VolumeCapabilities::analyze(&volume);
            for moment in [MomentType::Reflectivity, MomentType::Velocity] {
                let available = (0..capabilities.groups.len())
                    .filter(|group| group_has(&capabilities, *group, &moment))
                    .count();
                let mut previous = (0.0_f64, 0.0_f64);
                for tilts in 1..=MAX_TILTS.min(available) {
                    let (filled, deep) = box_fill(&volume, &capabilities, &moment, tilts);
                    assert!(
                        filled >= previous.0 && deep >= previous.1,
                        "{} {moment} went backwards at {tilts} tilts: {previous:?} then {:?}",
                        path.display(),
                        (filled, deep)
                    );
                    let where_ = path.display();
                    assert!(
                        tilts > 2 || deep == 0.0,
                        "{where_} {moment}: 3 km at {tilts}"
                    );
                    previous = (filled, deep);
                    fills[tilts].push(filled);
                    deeps[tilts].push(deep);
                    any_deep[tilts] += usize::from(deep > 0.0);
                }
            }
        }
        println!(" tilts   filled    >=3km    series with a 3 km column");
        for tilts in 1..=MAX_TILTS {
            let (mut filled, mut deep) = (fills[tilts].clone(), deeps[tilts].clone());
            if filled.is_empty() {
                continue;
            }
            filled.sort_by(f64::total_cmp);
            deep.sort_by(f64::total_cmp);
            println!(
                "  {tilts:>4}  {:>6.2}%  {:>6.2}%   {:>4} of {}",
                filled[filled.len() / 2] * 100.0,
                deep[deep.len() / 2] * 100.0,
                any_deep[tilts],
                filled.len()
            );
        }
        // Only worth asserting over a population; one file may be the KRAX case.
        let at_floor = fills[MIN_TILTS_FOR_A_BOX].len();
        let deep_at_floor = any_deep[MIN_TILTS_FOR_A_BOX];
        assert!(
            at_floor < 10 || deep_at_floor * 4 >= at_floor * 3,
            "only {deep_at_floor} of {at_floor} series gain depth at the floor"
        );
    }
}
