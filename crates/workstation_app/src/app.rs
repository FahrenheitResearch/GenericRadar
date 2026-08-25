use std::array;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use analyst_runtime::{
    Camera2D, FrameOrigin, FrameStage, GenerationClock, InstallReport, PaneId, PaneLayout,
    PlaybackState, RenderStamp, TiltSelection, ViewportMetrics, VolumeFrame, VolumeHistory,
    WorkspaceState,
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use color_tables::ColorTableSet;
use eframe::egui;
use map_scene::MapSceneController;
use radar_core::RadarVolume;

use data_source::FeedFreshness;
use data_source::warnings::{WarningRecord, WarningsSource, WarningsState};

use crate::hazards::{PlacedHazard, place_hazards};
use crate::live_service::{LiveService, LiveUpdate, default_live_cache_dir};
use crate::load_service::{LoadRequest, LoadService, LoadUpdate, LoadedVolume};
use crate::north_up::NorthUpFrame;
#[cfg(test)]
use crate::pane_canvas::draw_pane;
use crate::pane_canvas::{
    PaneExternalLayers, PaneMap, PaneTexture, PlacedSite, draw_pane_with_layers, pane_rects,
};
use crate::product::DisplayProduct;

use crate::app_support::{
    color_image_from_rgba, layout_label, pane_title, source_field_pane_title,
    unavailable_source_field_pane_title, viewport_changed,
};
use crate::product_availability::ProductAvailabilityIndex;
use crate::product_picker::{ProductPickerInput, ProductPickerState, draw_product_picker};
use crate::render_service::{
    RenderRequest, RenderService, RenderUpdate, RenderedPane, SweepBlendRequest,
};
use crate::sites_service::{LocatedSite, SitesService};
use crate::sweep::{SweepAnimator, SweepState, catch_up_factor};
use crate::warnings_service::WarningsService;

#[path = "live_follow.rs"]
mod live_follow;
mod online_data;
#[path = "placefiles.rs"]
pub(crate) mod placefiles;
#[path = "surface_observations.rs"]
pub(crate) mod surface_observations;

/// How often placed hazards are rebuilt so expiries take effect.
///
/// Placement filters by "now", so it goes stale on its own even when nothing
/// arrives. A warning ends on a whole minute, so checking twice a minute is
/// enough to never leave an expired polygon on screen for long.
const HAZARD_REPLACEMENT_INTERVAL: Duration = Duration::from_secs(30);

enum LiveAction {
    Start(String),
    Stop,
}

/// The deliberately small local-file browser for analyst-supplied placefiles.
/// Radar's existing browser identifies every entry as a radar volume and
/// routes its selection into the timeline, so sharing that instance would load
/// a placefile as weather data. This browser never touches the volume loader.
struct PlacefileBrowser {
    directory: PathBuf,
    directory_text: String,
    entries: Vec<PlacefileBrowserEntry>,
    error: Option<String>,
}

struct PlacefileBrowserEntry {
    path: PathBuf,
    name: String,
    is_directory: bool,
}

impl PlacefileBrowser {
    fn open() -> Self {
        let downloads = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .map(|root| root.join("Downloads"));
        let directory = downloads
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut browser = Self {
            directory_text: directory.display().to_string(),
            directory,
            entries: Vec::new(),
            error: None,
        };
        browser.reload();
        browser
    }

    fn change_directory(&mut self, directory: PathBuf) {
        self.directory_text = directory.display().to_string();
        self.directory = directory;
        self.reload();
    }

    fn reload(&mut self) {
        self.entries.clear();
        self.error = None;
        let directory = match std::fs::read_dir(&self.directory) {
            Ok(directory) => directory,
            Err(error) => {
                self.error = Some(format!("Cannot open folder: {error}"));
                return;
            }
        };
        for entry in directory.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() && !kind.is_file() {
                continue;
            }
            self.entries.push(PlacefileBrowserEntry {
                path: entry.path(),
                name: entry.file_name().to_string_lossy().into_owned(),
                is_directory: kind.is_dir(),
            });
        }
        self.entries.sort_by(|left, right| {
            right.is_directory.cmp(&left.is_directory).then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
        });
    }
}

/// Recognise a GR placefile by its declared directives rather than by a
/// filename alone. `.pf` and `.placefile` are unambiguous; extensionless and
/// `.txt` files are inspected without consuming more than a small header.
fn dropped_path_is_placefile(path: &Path) -> bool {
    let extension = path.extension().and_then(|value| value.to_str());
    if extension.is_some_and(|value| {
        value.eq_ignore_ascii_case("pf") || value.eq_ignore_ascii_case("placefile")
    }) {
        return true;
    }
    if extension.is_some_and(|value| !value.eq_ignore_ascii_case("txt")) {
        return false;
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = [0_u8; 8 * 1024];
    let Ok(length) = file.read(&mut bytes) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&bytes[..length]) else {
        return false;
    };
    let recognized = text
        .lines()
        .map(|line| line.trim_start_matches('\u{feff}').trim())
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| {
            [
                "title",
                "refresh",
                "threshold",
                "color",
                "font",
                "iconfile",
                "icon",
                "text",
                "line",
                "object",
                "polygon",
            ]
            .iter()
            .any(|directive| name.trim().eq_ignore_ascii_case(directive))
        })
        .take(2)
        .count();
    recognized >= 2
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourcePaletteAction {
    Edit,
    Reset,
}

/// What a tool backed only by modeled [`DisplayProduct`] variants says when
/// the active pane instead carries an exact producer-native field.
///
/// This sentence is part of the scientific boundary: the tool remains
/// unavailable rather than letting `DisplayProduct::from_product_id` turn an
/// unfamiliar id into reflectivity behind the analyst's back.
fn source_field_2d_only_message(tool: &str, producer_name: &str) -> String {
    format!(
        "{tool} is unavailable for exact source field {producer_name}. Exact source fields are \
         currently 2D only; {producer_name} remains selected and no modeled product was \
         substituted."
    )
}

/// Resolve only product ids whose semantics the static product model owns.
///
/// `DisplayProduct::from_product_id` intentionally has a total, migration-safe
/// default for old workspace ids. Producer-native ids are not old workspace
/// ids, though, and crossing that default would silently turn them into REF.
/// Every modeled-moment-only consumer goes through this boundary first.
fn modeled_product_or_source_field(id: &radar_core::ProductId) -> Result<DisplayProduct, &str> {
    match crate::source_fields::producer_name_from_product_id(id) {
        Some(producer_name) => Err(producer_name),
        None => Ok(DisplayProduct::from_product_id(id)),
    }
}

/// The palette control for an exact producer-native field.
///
/// It deliberately does not use `ColorTableFamily::Generic`: that family is
/// shared by every unmodeled field. The action is returned to the caller so
/// the UI closure borrows no application state while the exact-id map moves.
fn source_palette_control(
    ui: &mut egui::Ui,
    producer_name: &str,
    resolved: &crate::source_field_palettes::ResolvedSourceFieldPalette,
) -> Option<SourcePaletteAction> {
    let (minimum, maximum) = resolved.value_range();
    let mode = if resolved.automatic {
        "AUTO"
    } else if resolved.current_is_durable {
        "CUSTOM · SAVED"
    } else {
        "CUSTOM · SESSION"
    };
    let label = format!("{mode} {minimum:.3}…{maximum:.3}");
    let mut action = None;
    egui::ComboBox::from_id_salt("workstation-source-field-palette")
        .selected_text(label)
        .width(210.0)
        .show_ui(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("Exact field: {producer_name}"))
                    .monospace()
                    .weak(),
            );
            ui.label(
                egui::RichText::new(format!("Palette: {}", resolved.table.base_name())).weak(),
            );
            if !resolved.automatic {
                ui.label(
                    egui::RichText::new(if resolved.current_is_durable {
                        "Saved binding · returns after restart"
                    } else {
                        "Session-only preview · Save in the editor to keep it"
                    })
                    .weak(),
                );
            }
            if ui
                .selectable_label(false, "Edit palette and fixed range…")
                .clicked()
            {
                action = Some(SourcePaletteAction::Edit);
                ui.close();
            }
            if !resolved.automatic
                && ui
                    .selectable_label(false, "Reset to observed range")
                    .clicked()
            {
                action = Some(SourcePaletteAction::Reset);
                ui.close();
            }
        })
        .response
        .on_hover_text(if resolved.automatic {
            "Automatic visibility: this exact source field is stretched across its observed finite values. Edit to set reproducible fixed stops and colours."
        } else if resolved.current_is_durable {
            "Saved field-specific palette and fixed raw-value range. It does not affect any other source field; Reset returns this exact id to automatic observed-range display."
        } else {
            "Session-only field-specific preview; it will not return after restart. Save the matching edit to keep it, or choose CUSTOM → Reset to observed range to undo it."
        });
    action
}

/// One ordered set of local files being decoded into separate timeline
/// frames. It is deliberately not a volume-builder: each successful file
/// remains independently identified and failure of one advances to the next.
struct FileSequence {
    paths: Vec<PathBuf>,
    preflight: crate::playlist_preflight::PlaylistRamEstimate,
    next: usize,
    loaded: usize,
    failures: Vec<(PathBuf, String)>,
    /// The history/map architecture is one radar anchor at a time. The first
    /// successful file fixes that anchor; another radar is reported and
    /// skipped instead of being painted on the wrong ground.
    site_id: Option<String>,
    site_position: Option<(Option<f32>, Option<f32>)>,
    level1_files: usize,
    /// A proven one-cut Archive II member waits here until the next decoded
    /// file either extends its internal volume identity or proves a boundary.
    pending_assembly: Option<PendingSweepAssembly>,
    assembled_files: usize,
    assembled_groups: usize,
    assembly_refusals: Vec<(PathBuf, nexrad_io::sweep_assembly::SweepAssemblyRefusal)>,
    /// Explicit limits may remove an installed logical volume. Count every
    /// such removal so completion status never calls an eviction "retained".
    evicted_frames: usize,
}

/// A large selection waiting for an operator decision. This is an egui window,
/// not a blocking native dialog: the rest of the application remains alive and
/// the current session is not cleared until Continue is pressed.
struct PendingPlaylistConfirmation {
    paths: Vec<PathBuf>,
    estimate: crate::playlist_preflight::PlaylistRamEstimate,
}

/// A local selection whose metadata/signature planning is running off the UI
/// thread. Only the matching generation may advance to confirmation or load.
struct PendingPlaylistPreflight {
    generation: analyst_runtime::Generation,
    selected: usize,
}

struct PendingSweepAssembly {
    loaded: LoadedVolume,
    evidence: nexrad_io::sweep_assembly::ProvenSweepMembership,
    first_source_label: String,
}

impl FileSequence {
    fn total(&self) -> usize {
        self.paths.len()
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.next
            .checked_sub(1)
            .and_then(|index| self.paths.get(index))
    }

    /// Successfully decoded files reduced by any one-cut members still
    /// waiting for a boundary, then folded by the proven assembly groups that
    /// have already reached the timeline.
    fn logical_volumes(&self) -> usize {
        let pending_members = self
            .pending_assembly
            .as_ref()
            .map_or(0, |pending| pending.evidence.member_count);
        self.loaded
            .saturating_sub(pending_members)
            .saturating_sub(self.assembled_files)
            .saturating_add(self.assembled_groups)
            .saturating_add(usize::from(pending_members > 0))
    }
}

/// Stable order for a selection assembled by an OS drop or by the browser.
/// Radar archive names normally sort chronologically; the history still uses
/// the decoded volume time as its authoritative display order.
fn ordered_unique_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by(|left, right| {
        let left_name = left
            .file_name()
            .unwrap_or_else(|| left.as_os_str())
            .to_string_lossy()
            .to_lowercase();
        let right_name = right
            .file_name()
            .unwrap_or_else(|| right.as_os_str())
            .to_string_lossy()
            .to_lowercase();
        left_name
            .cmp(&right_name)
            .then_with(|| left.as_os_str().cmp(right.as_os_str()))
    });
    paths.dedup();
    paths
}

fn short_path_label(path: &std::path::Path) -> String {
    let label = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    const MAX_CHARS: usize = 56;
    if label.chars().count() <= MAX_CHARS {
        label
    } else {
        let mut short = label.chars().take(MAX_CHARS - 1).collect::<String>();
        short.push('…');
        short
    }
}

fn history_policy_status(policy: analyst_runtime::HistoryPolicy) -> String {
    if policy.max_frames == 0 && policy.max_estimated_bytes == 0 {
        return "retention Unlimited".to_owned();
    }
    let frames = if policy.max_frames == 0 {
        "Unlimited frames".to_owned()
    } else {
        format!("{} frames", policy.max_frames)
    };
    let bytes = if policy.max_estimated_bytes == 0 {
        "Unlimited RAM".to_owned()
    } else {
        format!(
            "{} RAM",
            crate::playlist_preflight::format_binary_bytes(
                policy.max_estimated_bytes.try_into().unwrap_or(u64::MAX)
            )
        )
    };
    format!("retention limit {frames} / {bytes}")
}

fn same_playlist_position(
    left: (Option<f32>, Option<f32>),
    right: (Option<f32>, Option<f32>),
) -> bool {
    fn same(left: Option<f32>, right: Option<f32>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(left), Some(right)) => (left - right).abs() <= 0.001,
            _ => false,
        }
    }
    same(left.0, right.0) && same(left.1, right.1)
}

/// What the live poll last said about the FEED, as opposed to what is on
/// screen.
///
/// The newest volume TIME is stored rather than an age, because an age stored
/// at poll time freezes: the poll runs every 1.2 s and the paint runs at up to
/// 60 Hz, so the number an analyst watches has to be recomputed against wall
/// clock on the frame it is drawn.
struct LiveFeed {
    site: String,
    /// The newest volume the chunks bucket holds for this site. On a dead
    /// prefix this stops moving and the age computed from it keeps climbing,
    /// which is precisely the signal that was missing.
    newest_volume_time: DateTime<Utc>,
    freshness: FeedFreshness,
}

/// A wall-clock age as an analyst reads it: `42 s`, `6 min`, `3 h`, `3 d`.
///
/// Floored to the unit shown - "3 d" covers everything from three days to just
/// under four - which is the ordinary "3 days ago" convention and keeps the
/// string to one number and one unit. Rounding up would be worse in the
/// direction that matters here: a 61-second-old frame reading "2 min" invites
/// exactly the mistrust this whole readout exists to remove.
///
/// Formatting only, no clock read and no allocation beyond the returned
/// string: this runs once per visible pane per frame plus once for the status
/// line.
pub(crate) fn format_age(age: TimeDelta) -> String {
    // Clamped rather than signed. `data_source::volume_age_at` already clamps,
    // and a caller that forgets must still not print "-3 s".
    let seconds = age.num_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds} s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} h");
    }
    let days = hours / 24;
    // Years exist for archive files, not for live: a 2013 case study reading
    // "4614 d old" is noise where "12 y old" is a fact. 365 flat - a leap day
    // does not change which word an analyst needs.
    if days < 365 {
        return format!("{days} d");
    }
    format!("{} y", days / 365)
}

/// How long the app may sleep before an age readout it has already drawn would
/// read differently.
///
/// Under a minute the string changes every second, so it wakes every second.
/// Above that it changes on the minute, so it sleeps to the next minute
/// boundary - which also caps every wait at 60 s, and that cap is deliberate:
/// whether a live feed has crossed
/// [`data_source::REALTIME_FEED_STALL_AFTER_SECONDS`] is a wall-clock question
/// too, and an app that slept 59 minutes between redraws of an hour-band age
/// would raise the stall banner up to an hour late.
pub(crate) fn age_repaint_interval(age: TimeDelta) -> Duration {
    let seconds = age.num_seconds().max(0);
    if seconds < 60 {
        return Duration::from_secs(1);
    }
    // 1..=60, never zero: a zero-length repaint request is a 60 Hz spin.
    Duration::from_secs((60 - seconds.rem_euclid(60)) as u64)
}

/// Opening overview scale: wide enough to show the country before a radar
/// volume says where to look.
const PLACEHOLDER_KM_PER_POINT: f32 = 4.0;

/// What the Open field and the drop target will take.
///
/// Named formats rather than named extensions, because the decoder routes on
/// magic bytes: an extensionless archive object works, and a `.gz` is opened
/// by what is inside it.
const OPEN_PATH_HINT: &str = "NEXRAD Level II (.ar2v/.gz/.bz2/_V06, or no extension at all), \
                             MATLAB Level 5 I/Q (.mat), \
     GR2Analyst .msg31, ODIM_H5 (.h5/.hdf/.hd5), CfRadial (.nc), or a mobile deployment .zip. \
     Files can also be dropped on the window.";

/// What the SNR threshold readout on the bar means, and why the number is
/// worth a place next to the tilt it describes.
///
/// The point of showing it at all: a weak field that is missing from the
/// picture may never have been written, and this is the number that says how
/// much was thrown away before the file existed.
const SNR_THRESHOLD_HINT: &str = "Signal-to-noise threshold this sweep was censored at, from \
     the Level 2 moment header. Gates weaker than this were discarded by the operational \
     processor before the file was written, so they are missing from the product, not from \
     the atmosphere. The operator sets it per site; the floor is -12.0 dB and 2.0 dB is \
     typical.";

/// What the recombination notice on the bar means. Shown only when the control
/// flags claim a loss, so its presence is the whole message.
const RESOLUTION_REDUCED_HINT: &str = "The control flags in this sweep's moment header say the \
     processor combined gates or radials before writing, so this sweep is coarser on disk \
     than the radar collected it.";

/// Truncation bounds for the two censoring readouts on the menu bar.
///
/// `bevel::sunken_readout` truncates its galley at the width it is given, so
/// these are not cosmetic: a bound below the longest text a readout can carry
/// would silently ellipsize the statement rather than let the row wrap. The
/// wide one holds the longest recombination label there is, and
/// `a_recombined_sweep_states_its_loss_in_full` draws both readouts at their
/// longest and checks neither galley was elided, so shrinking either fails
/// rather than clips.
const SNR_READOUT_WIDTH: f32 = 220.0;
const RESOLUTION_NOTICE_WIDTH: f32 = 560.0;

const MAX_LOAD_RESULTS_PER_FRAME: usize = 4;
const MAX_RENDER_RESULTS_PER_FRAME: usize = 4;
const TIMELINE_HEIGHT: f32 = 34.0;
/// How long the loop holds each frame, by default.
///
/// The DEFAULT now rather than the only answer: `Data > Loop frame time`
/// carries it. A loop speed is the most-used control the application still
/// could not change anywhere, and the argument for leaving it out - that a
/// loop speed belongs on the toolbar - argued against the quiet toolbar this
/// application is built around. It stays 700 ms, so a session with no
/// settings file loops exactly as it always did.
const PLAYBACK_FRAME_TIME: Duration = Duration::from_millis(700);

/// Why a pane has no render in flight and will not be given one for a stamp.
///
/// Without this the app had two failure loops on one worker. A failed render
/// left `pending_stamp = None` and `texture = None`, so `visible_panes_ready`
/// never became true and playback froze at that frame for ever while
/// repainting at ~60 Hz; and nothing recorded that the stamp had failed, so
/// `ensure_render_requested` resubmitted the identical request every frame and
/// the worker failed it again indefinitely - 15-20 ms per derived retry,
/// queueing every other pane behind it. Recording the stamp closes both:
/// `visible_panes_ready` treats a terminal stamp as ready, and a recorded
/// stamp is never resubmitted. Any clock bump - a new chunk, a camera move, a
/// product or palette change - makes a new stamp and naturally retries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderTerminal {
    /// The worker failed this exact stamp.
    Failed(RenderStamp),
    /// No cut in the volume can serve this pane's product for this stamp.
    Unavailable(RenderStamp),
}

impl RenderTerminal {
    fn stamp(self) -> RenderStamp {
        match self {
            Self::Failed(stamp) | Self::Unavailable(stamp) => stamp,
        }
    }
}

#[derive(Default)]
struct PaneRuntime {
    texture: Option<InstalledTexture>,
    pending_stamp: Option<RenderStamp>,
    /// Set when this pane's current stamp ended without a picture - see
    /// [`RenderTerminal`]. Cleared when a render installs or the pane resets.
    terminal: Option<RenderTerminal>,
    viewport: Option<ViewportMetrics>,
    status: String,
    /// The engine's own filter line for the picture on screen, counts and all:
    /// `render2d::GateFilterReport::badge`, straight off the render that
    /// installed here.
    ///
    /// Held apart from [`Self::status`] rather than formatted into it, and
    /// that separation is what lets `pane_header_status` put the filter
    /// statement FIRST on the row. The header truncates from the right, so
    /// whichever part is built last is the part a narrow pane loses - and the
    /// admission that gates are being hidden is not allowed to be that part.
    ///
    /// It is a report about the filter that WAS in force when the worker ran,
    /// so it is only ever shown when it still agrees with the filter that is
    /// in force now; `pane_header_status` checks that by the prefix relation
    /// `gate_filter_ui::pane_status_line` guarantees, rather than by clearing
    /// this on every settings change and hoping no path was missed.
    filter_line: Option<String>,
    /// Where the pointer was over this pane last frame, in radar-local
    /// kilometres, and the readout built from it.
    hovered_world_km: Option<(f64, f64)>,
    probe_text: Option<String>,
    /// The Doppler spectrum of the gate the readout above named, for a
    /// NEXRAD Level 1 file. Built beside `probe_text` and from the same
    /// reading, so the plot and the numbers can never describe two different
    /// gates. `None` for every other format.
    spectrum: Option<crate::iq_spectrum_ui::GateSpectrum>,
    /// Turns bursty radial arrivals into a clockwise wipe. One per pane,
    /// because two panes can be following different tilts of the same volume.
    sweep: SweepAnimator,
    /// The reveal handed to the last render request.
    sweep_state: Option<SweepState>,
    /// What that reveal is a reveal OF. The animator recognises a sweep by its
    /// elevation and start azimuth, which a product change leaves untouched
    /// while replacing every pixel, so the pane tracks that separately.
    sweep_key: Option<SweepKey>,
    /// When the reveal was last stepped, for the wall-clock ease.
    sweep_stepped_at: Option<Instant>,
}

impl PaneRuntime {
    fn reset_sweep(&mut self) {
        self.sweep.reset();
        self.sweep_state = None;
        self.sweep_key = None;
        self.sweep_stepped_at = None;
    }
}

/// What a pane's sweep reveal refers to.
///
/// Compared for equality to decide whether the eased position still means
/// anything. The cut index is in here because switching tilts inside one volume
/// changes everything about the sweep while leaving the frame identity alone.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SweepKey {
    identity: analyst_runtime::FrameIdentity,
    product: String,
    cut_index: usize,
}

struct InstalledTexture {
    handle: egui::TextureHandle,
    stamp: RenderStamp,
    camera: analyst_runtime::Camera2D,
    viewport: ViewportMetrics,
    width: u32,
    height: u32,
}

/// What a measurement of the current volume was taken from.
///
/// Compared for equality to decide whether the measurement is still good.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapabilitiesKey {
    identity: analyst_runtime::FrameIdentity,
    source_label: String,
    stage: FrameStage,
    cuts: usize,
    radials: usize,
}

/// Which of the two supported toolbars draws.
///
/// Both are real, kept, and one setting apart (2026-08-19): the menu bar is
/// the compact row with File / View / Map / Layers / Tools for the occasional
/// controls; Everything is the v0.1.0 row that shows every control at once
/// and wraps on narrower windows. Neither is a legacy mode: both are kept
/// deliberately, and one setting moves between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ToolbarStyle {
    #[default]
    Menus,
    Everything,
}

/// Settings-derived state that is read every frame, recomputed only when the
/// store reports a change. The alternative - a string-keyed store lookup per
/// pane per frame - would spend map walks on values that change a few times a
/// session, and the paint path has no business parsing choice ids.
#[derive(Clone, Copy)]
struct SettingsCache {
    /// Navigation response remaps handed to every pane - see
    /// [`crate::pane_canvas::NavTuning`] for why they are exponents.
    nav: crate::pane_canvas::NavTuning,
    /// Which toolbar the top of the window draws.
    toolbar_style: ToolbarStyle,
    site_labels: crate::pane_canvas::SiteLabelMode,
    /// Gates `refresh_placed_sites`: off hands the panes an empty slice, the
    /// same shape `show_warnings` uses for hazards.
    site_markers: bool,
    /// Surface station models and GR-compatible placefiles are paint-only
    /// geographic overlays; changing either never invalidates a radar raster.
    observations_enabled: bool,
    placefiles_enabled: bool,
    /// Gates each pane's colour bar layout.
    legend: bool,
    /// Off skips the clockwise reveal entirely: arrived radials paint at
    /// once, the way a scrubbed-back frame already does.
    sweep_animation: bool,
    /// Multiplier on the wall clock handed to the sweep animator.
    sweep_speed: f32,
    /// The Analysis page's unit-order choice for the Vrot readouts.
    vrot_mps_first: bool,
    /// Units & time: how every distance, height and volume time is written.
    /// Display only - see [`crate::units`].
    units: crate::units::UnitSystem,
    /// Readout & annotation: the ring ladder, the marker and label sizes, the
    /// readout precision and what the pane corner writes.
    annotation: crate::annotation::Annotation,
    /// Cross-section: how high the slice window is drawn, metres above the
    /// radar. Not display-only - it is what the sampler is asked for.
    xsection_top_m: f32,
    /// Data: how long the timeline holds each frame while looping.
    loop_frame_time: Duration,
    /// NEXRAD Level 1: the dwell, window and censor the moments of an open
    /// time-series file are estimated with. Not a display preference - see
    /// [`crate::iq_session`] - so a change re-runs the estimator rather than
    /// merely repainting.
    iq_controls: crate::iq_session::IqControls,
    /// Which receiver channel the spectrum readout transforms: 0 horizontal,
    /// 1 vertical.
    iq_spectrum_channel: usize,
    /// Which gates are allowed to be painted. `GateFilter::OFF` is the shipped
    /// value; anything else and every pane draws a FILTERED band saying so.
    /// Cached here because the paint path reads it once per pane per frame -
    /// for the band, for the badge and for the render request - and five
    /// string-keyed store lookups per pane per frame is exactly what this
    /// cache exists to avoid.
    gate_filter: render2d::GateFilter,
}

impl Default for SettingsCache {
    fn default() -> Self {
        Self {
            nav: crate::pane_canvas::NavTuning::default(),
            toolbar_style: ToolbarStyle::default(),
            site_labels: crate::pane_canvas::SiteLabelMode::default(),
            site_markers: true,
            observations_enabled: false,
            placefiles_enabled: true,
            legend: true,
            sweep_animation: true,
            sweep_speed: 1.0,
            vrot_mps_first: false,
            units: crate::units::UnitSystem::default(),
            annotation: crate::annotation::Annotation::default(),
            xsection_top_m: crate::xsection::DEFAULT_TOP_M,
            loop_frame_time: PLAYBACK_FRAME_TIME,
            iq_controls: crate::iq_session::IqControls::default(),
            iq_spectrum_channel: 0,
            gate_filter: render2d::GateFilter::OFF,
        }
    }
}

/// The gate-filter mask a readout consults, kept beside the sweep it belongs to.
///
/// The mask the renderer built lives on the render worker and dies with the
/// frame, so the readout path builds its own against the sweep as it sits in
/// the cut - which is the indexing
/// [`render2d::ViewportMomentCache::gate_filter_mask`] documents, and the same
/// grid [`crate::probe::probe_polar`] reads. It is memoised on everything it
/// depends on, because hovering recomputes the readout every frame and a
/// super-resolution sweep is over a million gates.
///
/// This is not a display decision. It exists so a readout can never answer a
/// censored gate with its true value at a pixel the pane deliberately drew
/// empty: the number under the cursor has to be a number that is on the
/// screen.
struct ProbeCensor {
    /// Volume identity by content rather than by pointer, so a freed and
    /// reallocated `Arc` cannot pass for the frame this was computed from.
    site: String,
    volume_time: DateTime<Utc>,
    cut_index: usize,
    moment: radar_core::MomentType,
    filter: render2d::GateFilter,
    /// `None` when the filter ran and hid nothing here - the same meaning
    /// `GateFilterOutcome::mask` gives it.
    mask: Option<render2d::GateFilterMask>,
}

/// A private screenshot tag: the still-image exporter deliberately ignores it.
#[derive(Clone, Debug)]
struct LoopCaptureTag {
    capture_id: u64,
    frame_index: usize,
}

/// Equal timestamps are legitimate in independently selected research files,
/// so an identity alone cannot identify which picture a loop promised to save.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LoopFrameKey {
    identity: analyst_runtime::FrameIdentity,
    source_label: String,
}

impl LoopFrameKey {
    fn from_frame(frame: &VolumeFrame) -> Self {
        Self {
            identity: frame.identity.clone(),
            source_label: frame.source_label.clone(),
        }
    }
}

/// Drives one honest oldest-to-newest loop through the actual displayed panes.
/// Frames are intentionally uncapped: the analyst controls their own history.
struct LoopExportState {
    capture_id: u64,
    frame_keys: Vec<LoopFrameKey>,
    original_frame: Option<LoopFrameKey>,
    original_selected: Option<usize>,
    original_follows_live: bool,
    original_playback: PlaybackState,
    next_index: usize,
    awaiting_screenshot: bool,
    settled_paints_remaining: u8,
    frames: Vec<Arc<egui::ColorImage>>,
    file_base: String,
    delay_ms: u32,
}

pub struct WorkstationApp {
    workspace: WorkspaceState,
    history: VolumeHistory,
    /// What the current volume can do, measured once per frame off the paint
    /// path. Cut selection needs median elevations and per-sweep scan times,
    /// and walking every radial while painting is not affordable.
    /// Whether a pane click takes a Vrot endpoint instead of selecting a pane.
    /// Thermal levels the hail products are computed against. Starts as the
    /// documented fallback, which badges itself ASSUMED so nobody mistakes it
    /// for a sounding.
    hail_environment: product_engine::HailEnvironment,
    /// The open NEXRAD Level 1 (time series) record, when one is open.
    ///
    /// Held on the application rather than in the history because it is not a
    /// frame: it is the PULSES the current frame was estimated from, and it is
    /// what the Level 1 settings re-run and what the spectrum readout
    /// transforms. `None` whenever the current frame came from a file that
    /// arrived with its moments already made, which is every other format.
    iq: Option<Box<crate::iq_session::IqSession>>,
    /// Panes temporarily moved from REF to PWR_REL for a calibration-free I/Q
    /// source. The bit is cleared by an explicit product choice and consumed
    /// when the next calibrated/non-IQ source arrives, so the convenience can
    /// never permanently rewrite the analyst's workspace.
    relative_power_fallback_from_ref: [bool; analyst_runtime::MAX_PANES],
    /// The 3D volume explorer. Its own window, so opening it does not disturb
    /// the pane layout an analyst has set up.
    vol3d: crate::vol3d::Vol3d,
    /// Cross-sections: the analyst's line on the 2D panes and the slice
    /// window that follows it. Its own window, like the 3D explorer.
    xsection: crate::xsection::XSection,
    vrot_active: bool,
    vrot_state: crate::vrot::VrotState,
    vrot_pane: Option<PaneId>,
    capabilities: Option<Arc<product_engine::VolumeCapabilities>>,
    capabilities_for: Option<CapabilitiesKey>,
    /// How hard the raster worker is asked to work. Not part of `RenderStamp`:
    /// a change bumps every pane's view clock instead, which is the existing
    /// way of saying "same data, different picture".
    quality: render2d::DisplayQuality,
    /// Which products the current volume can actually show, rebuilt with the
    /// capabilities.
    product_availability: ProductAvailabilityIndex,
    /// Producer-native fields the research-data decoders preserved. Rebuilt
    /// beside capabilities so opening the picker never walks every cut just
    /// to discover what the file already supplied.
    source_fields: crate::source_fields::SourceFieldCatalog,
    /// Analyst colour/range decisions for exact producer-native fields.
    /// Absence means automatic observed-range visibility; entries never flow
    /// through the shared Generic family.
    source_field_palettes: crate::source_field_palettes::SourceFieldPaletteOverrides,
    product_picker: ProductPickerState,
    product_picker_open: bool,
    palette_editor: crate::palette_editor::PaletteEditorState,
    load_service: LoadService,
    render_service: RenderService,
    session_clock: GenerationClock,
    frame_clock: GenerationClock,
    pane_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    view_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    sweep_clocks: [GenerationClock; analyst_runtime::MAX_PANES],
    palette_clock: GenerationClock,
    panes: [PaneRuntime; analyst_runtime::MAX_PANES],
    color_tables: Arc<ColorTableSet>,
    /// Colour tables the analyst supplied, and the folder they came from.
    /// Read at startup, when the window regains focus, and after a drop; see
    /// `crate::user_tables`.
    user_tables: crate::user_tables::UserTables,
    /// The colour table editor wrote a file and the folder has not been read
    /// since. Set by [`WorkstationApp::palette_editor_window`] and acted on at
    /// the top of the NEXT frame - see the rescan in `update`, which explains
    /// why it cannot be done where it is noticed.
    user_tables_rescan_pending: bool,
    /// The toolbar palette combo's rows, held between frames. Building them
    /// parses every built-in for the family and clones every user table, and
    /// an open combo popup asks for them once a frame; see
    /// `settings_ui::PaletteOfferCache`.
    palette_offers: crate::settings_ui::PaletteOfferCache,
    source_path_text: String,
    /// The `Open…` window. Held here rather than rebuilt per frame because it
    /// owns the folder it is looking at, the identifications it has already
    /// paid for, and the channel its scan thread answers on.
    file_browser: crate::file_browser::FileBrowser,
    /// A multi-file open is a sequential playlist of independent frames.
    file_sequence: Option<FileSequence>,
    playlist_preflight_service: crate::playlist_preflight::PlaylistPreflightService,
    playlist_preflight_clock: GenerationClock,
    pending_playlist_preflight: Option<PendingPlaylistPreflight>,
    pending_playlist_confirmation: Option<PendingPlaylistConfirmation>,
    sequence_status: Option<String>,
    sequence_detail: Option<String>,
    current_view_export: crate::current_view_export::CurrentViewExport,
    /// A full-window screenshot for each exact, settled history frame.
    loop_export: Option<LoopExportState>,
    next_loop_capture_id: u64,
    loop_export_notice: Option<String>,
    /// Public research-radar catalog navigation and its background download
    /// worker. Kept beside the local browser because both return a path into
    /// the same load seam; neither performs decoding itself.
    online_data: online_data::OnlineDataBrowser,
    status: String,
    load_ms: Option<f32>,
    last_playback_step: Instant,
    map_scene: MapSceneController,
    sites_service: SitesService,
    sites: Vec<LocatedSite>,
    placed_sites: Arc<[PlacedSite]>,
    placed_sites_projection: Option<map_scene::ProjectionId>,
    /// Real METAR/mesonet reports, their background worker and station
    /// histories. Shared by every pane rather than fetched once per pane.
    surface_observations: surface_observations::SurfaceObservationService,
    /// Analyst-owned GR/GR2Analyst placefiles, retained across refreshes and
    /// independently persisted without entering the volume timeline.
    placefiles: placefiles::PlacefileManager,
    placefiles_window_open: bool,
    placefile_browser: Option<PlacefileBrowser>,
    live_service: LiveService,
    live_cache_dir: PathBuf,
    site_text: String,
    live_site: Option<String>,
    live_status: String,
    /// The actual acquisition time last accepted by each pane's low-tilt
    /// follower. Per-pane state matters because reflectivity and velocity
    /// often come from different legs of the same split cut.
    live_follow_last_scan: [Option<DateTime<Utc>>; analyst_runtime::MAX_PANES],
    /// A manual tilt choice stays visible until a genuinely newer eligible
    /// sweep arrives; merely repainting the same volume cannot undo it.
    live_follow_manual_hold: [Option<DateTime<Utc>>; analyst_runtime::MAX_PANES],
    /// What the live poll last said about the feed itself. `None` when no live
    /// session is running, so nothing here can outlive the session that
    /// produced it and label a local file "stalled".
    live_feed: Option<LiveFeed>,
    warnings_service: WarningsService,
    warnings: Vec<WarningRecord>,
    warnings_state: WarningsState,
    show_warnings: bool,
    placed_hazards: Arc<[PlacedHazard]>,
    placed_hazards_projection: Option<map_scene::ProjectionId>,
    placed_hazards_at: Option<Instant>,
    /// What settings exist - contributed categories, items, ranges, defaults.
    settings_registry: settings::SettingsRegistry,
    /// The persisted values, loaded once in `main` and saved debounced from
    /// the frame loop. Everything the settings window edits lives here.
    settings_store: settings::SettingsStore,
    /// The settings window's own state: open, selected page, search.
    settings_ui: crate::settings_ui::SettingsUi,
    /// The toolbar's gate-filter chip: whether its panel is down. The filter
    /// itself lives in the settings store, so the chip is stateless beyond
    /// this and a restart reopens on the same criteria.
    gate_filter_ui: crate::gate_filter_ui::GateFilterUi,
    /// The censor the probe and the Vrot sampler read, memoised. Empty
    /// whenever the filter is off, which is the shipped state.
    probe_censor: Option<ProbeCensor>,
    /// Frame-rate mirrors of the handful of settings the paint path reads.
    settings_cache: SettingsCache,
}

impl WorkstationApp {
    pub fn new(
        creation_context: &eframe::CreationContext<'_>,
        input_path: Option<PathBuf>,
        live_site: Option<String>,
        warnings_source: WarningsSource,
        settings_store: settings::SettingsStore,
    ) -> Self {
        Self::with_context(
            creation_context.egui_ctx.clone(),
            input_path,
            live_site,
            warnings_source,
            settings_store,
        )
    }

    /// The whole construction against a bare [`egui::Context`].
    ///
    /// Split from [`Self::new`] because `eframe::CreationContext` cannot be
    /// built outside eframe, and the behavioural tests below need the real
    /// application - real workers, real clocks - without a window.
    fn with_context(
        context: egui::Context,
        input_path: Option<PathBuf>,
        live_site: Option<String>,
        warnings_source: WarningsSource,
        settings_store: settings::SettingsStore,
    ) -> Self {
        let source_path_text = input_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut app = Self {
            workspace: WorkspaceState::default(),
            history: VolumeHistory::default(),
            hail_environment: product_engine::HailEnvironment::climatological_fallback(),
            iq: None,
            relative_power_fallback_from_ref: [false; analyst_runtime::MAX_PANES],
            vol3d: crate::vol3d::Vol3d::default(),
            xsection: crate::xsection::XSection::default(),
            vrot_active: false,
            vrot_state: crate::vrot::VrotState::Idle,
            vrot_pane: None,
            capabilities: None,
            capabilities_for: None,
            quality: render2d::DisplayQuality::default(),
            product_availability: ProductAvailabilityIndex::unrestricted(),
            source_fields: crate::source_fields::SourceFieldCatalog::default(),
            source_field_palettes:
                crate::source_field_palettes::SourceFieldPaletteOverrides::default(),
            product_picker: ProductPickerState::default(),
            product_picker_open: false,
            palette_editor: crate::palette_editor::PaletteEditorState::default(),
            load_service: LoadService::new(context.clone()),
            render_service: RenderService::new(context.clone()),
            live_service: LiveService::new(context.clone()),
            map_scene: {
                let repaint_context = context.clone();
                MapSceneController::new(move || repaint_context.request_repaint())
            },
            session_clock: GenerationClock::default(),
            frame_clock: GenerationClock::default(),
            pane_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            view_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            sweep_clocks: [GenerationClock::default(); analyst_runtime::MAX_PANES],
            palette_clock: GenerationClock::default(),
            panes: array::from_fn(|_| PaneRuntime::default()),
            color_tables: Arc::new(ColorTableSet::default()),
            // The folder is scanned here, so it is already read by the time
            // `apply_settings_on_start` resolves stored palette names
            // against it.
            user_tables: crate::user_tables::UserTables::default(),
            user_tables_rescan_pending: false,
            palette_offers: crate::settings_ui::PaletteOfferCache::default(),
            source_path_text,
            file_browser: crate::file_browser::FileBrowser::new(context.clone()),
            file_sequence: None,
            playlist_preflight_service: crate::playlist_preflight::PlaylistPreflightService::new(
                context.clone(),
            ),
            playlist_preflight_clock: GenerationClock::default(),
            pending_playlist_preflight: None,
            pending_playlist_confirmation: None,
            sequence_status: None,
            sequence_detail: None,
            current_view_export: crate::current_view_export::CurrentViewExport::default(),
            loop_export: None,
            next_loop_capture_id: 0,
            loop_export_notice: None,
            online_data: online_data::OnlineDataBrowser::new(context.clone()),
            status: "Drop a Level II file here or enter a path above".to_owned(),
            load_ms: None,
            last_playback_step: Instant::now(),
            sites_service: SitesService::new(context.clone()),
            sites: Vec::new(),
            placed_sites: Vec::new().into(),
            placed_sites_projection: None,
            surface_observations: surface_observations::SurfaceObservationService::new(
                context.clone(),
            ),
            placefiles: placefiles::PlacefileManager::load(),
            placefiles_window_open: false,
            placefile_browser: None,
            live_cache_dir: default_live_cache_dir(),
            site_text: String::new(),
            live_site: None,
            live_status: String::new(),
            live_follow_last_scan: [None; analyst_runtime::MAX_PANES],
            live_follow_manual_hold: [None; analyst_runtime::MAX_PANES],
            live_feed: None,
            warnings_service: WarningsService::new(context, warnings_source),
            warnings: Vec::new(),
            warnings_state: WarningsState::Unknown,
            show_warnings: true,
            placed_hazards: Vec::new().into(),
            placed_hazards_projection: None,
            placed_hazards_at: None,
            settings_registry: crate::settings_ui::full_registry(
                crate::theme::settings::settings_category(),
            ),
            settings_store,
            settings_ui: crate::settings_ui::SettingsUi::default(),
            gate_filter_ui: crate::gate_filter_ui::GateFilterUi::default(),
            probe_censor: None,
            settings_cache: SettingsCache::default(),
        };
        // Open on a map instead of an empty pane. The placeholder anchor, and
        // this overview scale, are replaced by the first real volume.
        //
        // BEFORE the settings restore, deliberately: `centre_on_anchor`
        // points every camera at the overview, and a camera the settings file
        // restores must land on top of that, not under it. A restored camera
        // then rides through `leave_overview` untouched when the first volume
        // arrives, exactly as a `--zoom`/`--center` camera does.
        app.map_scene.set_default_anchor();
        let panes = app.workspace.centre_on_anchor(PLACEHOLDER_KM_PER_POINT);
        app.invalidate_view_panes(&panes);
        app.apply_settings_on_start();
        let opened_from_cli = input_path.is_some();
        if let Some(path) = input_path {
            app.begin_load(path);
        }
        if let Some(site) = live_site {
            app.site_text = site.trim().to_uppercase();
            app.start_live(site);
        } else if !opened_from_cli {
            // Nothing asked for on the command line, so the settings choose:
            // a stated startup site first, else the site that was live when
            // the application last closed. A command-line file or site always
            // wins - stated intent beats remembered intent.
            app.start_on_saved_site();
        }
        app
    }

    /// The retention policy the operator configured. Zero is literal and
    /// means Unlimited for that dimension in an explicitly selected local
    /// file session.
    fn configured_history_policy(&self) -> analyst_runtime::HistoryPolicy {
        use crate::settings_ui::catalog::keys;

        let frames = self
            .settings_store
            .effective_int(
                &self.settings_registry,
                keys::data::CATEGORY,
                keys::data::HISTORY_MAX_FRAMES,
            )
            .max(0) as usize;
        let megabytes = self
            .settings_store
            .effective_int(
                &self.settings_registry,
                keys::data::CATEGORY,
                keys::data::HISTORY_MAX_MB,
            )
            .max(0) as usize;
        analyst_runtime::HistoryPolicy::new(frames, megabytes.saturating_mul(1024 * 1024))
    }

    /// A live feed can run unattended and has no finite playlist whose cost
    /// the operator accepted in preflight. Preserve every positive configured
    /// ceiling, but replace a zero dimension with the runtime's conservative
    /// live default (30 frames / 1 GiB).
    fn live_history_policy(&self) -> analyst_runtime::HistoryPolicy {
        let configured = self.configured_history_policy();
        let fallback = analyst_runtime::HistoryPolicy::default();
        analyst_runtime::HistoryPolicy::new(
            if configured.max_frames == 0 {
                fallback.max_frames
            } else {
                configured.max_frames
            },
            if configured.max_estimated_bytes == 0 {
                fallback.max_estimated_bytes
            } else {
                configured.max_estimated_bytes
            },
        )
    }

    /// Apply everything the settings file says to a freshly built application.
    ///
    /// Called once from `with_context`, before any load or live start, so the
    /// history policy exists before the first install and a restored pane
    /// never renders once in its default shape first.
    fn apply_settings_on_start(&mut self) {
        self.apply_settings_document();

        // Only on start: `VolumeHistory::new` builds an empty history, so
        // this line belongs to a session that has not loaded anything yet. A
        // profile switch changes the same two settings through
        // `apply_changed_setting`, which calls `set_policy` and evicts down to
        // it rather than throwing away every volume the analyst has.
        self.history = VolumeHistory::new(self.configured_history_policy());

        // The shipped profile is "how this build behaves with nothing
        // stored", and only the application can say what that includes: the
        // default pane layout and the default colour tables are structured
        // snapshot state, not scalar knobs.
        self.settings_ui
            .profiles
            .set_shipped(Self::shipped_settings_document());
    }

    /// The document this build behaves as when nothing is stored - what the
    /// shipped profile installs.
    fn shipped_settings_document() -> settings::SettingsDocument {
        let mut document = settings::SettingsDocument::default();
        // No `values` at all: every declared setting then resolves to the
        // default its own module declared, which is the definition of "as
        // shipped" and stays correct when a setting is added.
        let mut workspace = crate::settings_ui::sync::capture_workspace(
            &analyst_runtime::WorkspaceState::default(),
        );
        workspace.palettes =
            crate::settings_ui::palettes::capture_palettes(&color_tables::ColorTableSet::default());
        workspace.show_warnings = Some(true);
        document.workspace = workspace;
        document
    }

    /// Apply the whole settings document to live state.
    ///
    /// Everything a settings file can say that is not a scalar knob the
    /// per-setting path already handles: the colour tables, the workspace
    /// snapshot, and the handful of values read once rather than watched.
    ///
    /// Run at startup and again on every profile switch - a switch replaces
    /// the document, so it needs exactly what a freshly read file needs. It
    /// deliberately touches nothing that belongs to the session rather than to
    /// the settings (the volume history, the live feed): switching from the
    /// chase profile to the office profile must not throw away the storm.
    fn apply_settings_document(&mut self) {
        use crate::settings_ui::catalog::keys;

        // Palettes before anything renders, resolved against the shipped
        // catalogue AND the analyst's own colour table folder. A stored name
        // neither can supply falls back to its family's default inside
        // `apply_palettes_with_user` - never to a blank.
        self.color_tables = Arc::new(crate::settings_ui::palettes::apply_palettes_with_user(
            &self.settings_store.workspace().palettes,
            self.user_tables.library(),
        ));
        self.source_field_palettes =
            crate::source_field_palettes::SourceFieldPaletteOverrides::from_snapshot(
                &self.settings_store.workspace().source_field_palettes,
                self.user_tables.library(),
            );

        // The workspace: layout, active pane, per-pane product, tilt, camera
        // and camera link. Cameras are sanitized on the way in.
        let snapshot = self.settings_store.workspace().clone();
        crate::settings_ui::sync::apply_workspace_snapshot(&snapshot, &mut self.workspace);
        // Product ids are restored raw; this is where the registry lives, so
        // this is where they resolve. An unknown id resets that pane to the
        // default product with a visible status line, never silently.
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            let id = self.workspace.pane(pane).product.clone();
            if DisplayProduct::try_from_product_id(&id).is_none()
                && crate::source_fields::producer_name_from_product_id(&id).is_none()
            {
                self.workspace.pane_mut(pane).product = DisplayProduct::default().product_id();
                self.status = format!("Unknown saved product '{}' - reset to default", id.0);
            }
        }
        // The restore moved cameras and products under every pane; the same
        // invalidation any camera change gets keeps the clocks honest.
        self.invalidate_view_panes(self.workspace.visible_panes());

        if let Some(quality) =
            crate::settings_ui::sync::quality_from_id(&self.settings_store.effective_text(
                &self.settings_registry,
                keys::radar::CATEGORY,
                keys::radar::QUALITY,
            ))
        {
            self.quality = quality;
        }

        if let Some(preset) =
            map_scene::MapStylePreset::from_id(&self.settings_store.effective_text(
                &self.settings_registry,
                keys::map::CATEGORY,
                keys::map::BASEMAP_STYLE,
            ))
            && preset.style() != self.map_scene.style()
        {
            self.map_scene.set_style(preset.style());
        }
        let provider = self.settings_store.effective_text(
            &self.settings_registry,
            keys::map::CATEGORY,
            keys::map::IMAGERY_PROVIDER,
        );
        self.apply_imagery_provider(&provider);
        self.apply_imagery_dim();

        if let Some(show) = self.settings_store.workspace().show_warnings {
            self.show_warnings = show;
        }

        self.apply_storm_motion_settings();
        self.apply_vol3d_settings();
        self.apply_surface_observation_settings();
        self.recompute_settings_cache();
    }

    /// Start live on the site the settings name, when the command line named
    /// nothing: a stated startup site first, else the last-viewed site when
    /// resuming is on. Case and whitespace are normalised for the toolbar's
    /// site box; the live service normalises again for itself.
    fn start_on_saved_site(&mut self) {
        use crate::settings_ui::catalog::keys;
        let startup = self
            .settings_store
            .effective_text(
                &self.settings_registry,
                keys::data::CATEGORY,
                keys::data::STARTUP_SITE,
            )
            .trim()
            .to_uppercase();
        let site = if !startup.is_empty() {
            Some(startup)
        } else if self.settings_store.effective_bool(
            &self.settings_registry,
            keys::data::CATEGORY,
            keys::data::RESUME_LAST_SITE,
        ) {
            self.settings_store.workspace().last_site.clone()
        } else {
            None
        };
        if let Some(site) = site {
            self.site_text = site.trim().to_uppercase();
            self.start_live(site);
        }
    }

    /// Resolve a stored imagery-provider key onto the scene. `"none"`, an
    /// unknown key and a provider this build's terms cannot satisfy all mean
    /// no imagery - never a guessed substitute.
    fn apply_imagery_provider(&mut self, key: &str) {
        let provider = map_scene::TileProvider::ALL
            .into_iter()
            .find(|candidate| candidate.key() == key)
            .filter(|candidate| self.map_scene.tile_provider_permitted(*candidate));
        if provider != self.map_scene.tile_provider() {
            self.map_scene.set_tile_provider(provider);
        }
    }

    /// A manual dim (auto off) goes through the same setter the toolbar's
    /// slider uses. While auto is on the scrim stays measured from the tiles
    /// that actually arrive, so nothing is written here.
    fn apply_imagery_dim(&mut self) {
        use crate::settings_ui::catalog::keys;
        let auto = self.settings_store.effective_bool(
            &self.settings_registry,
            keys::map::CATEGORY,
            keys::map::IMAGERY_DIM_AUTO,
        );
        if !auto {
            let dim = self.settings_store.effective_float(
                &self.settings_registry,
                keys::map::CATEGORY,
                keys::map::IMAGERY_DIM,
            );
            self.map_scene.set_tile_scrim(dim as f32);
        }
    }

    /// The storm motion the Analysis sliders describe, onto every pane. The
    /// SRV/DSRV products read it per pane, and the sliders are the only
    /// writer, so all panes move together.
    fn apply_storm_motion_settings(&mut self) {
        use crate::settings_ui::catalog::keys;
        let motion = crate::settings_ui::sync::storm_motion_from_settings(
            self.settings_store.effective_float(
                &self.settings_registry,
                keys::analysis::CATEGORY,
                keys::analysis::STORM_MOTION_DIR,
            ),
            self.settings_store.effective_float(
                &self.settings_registry,
                keys::analysis::CATEGORY,
                keys::analysis::STORM_MOTION_SPEED,
            ),
        );
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            self.workspace.pane_mut(pane).storm_motion = motion;
        }
    }

    /// Copy the persisted 3D-explorer values onto the live [`crate::vol3d::Vol3d`].
    /// All of them, not just the one that changed: the settings page and the
    /// window's own controls edit the same fields, and a partial copy would
    /// leave two sources of truth disagreeing.
    fn apply_vol3d_settings(&mut self) {
        use crate::settings_ui::catalog::keys::vol3d as k;
        let store = &self.settings_store;
        let registry = &self.settings_registry;
        let float = |id: &str| store.effective_float(registry, k::CATEGORY, id) as f32;
        let toggle = |id: &str| store.effective_bool(registry, k::CATEGORY, id);
        let text = |id: &str| store.effective_text(registry, k::CATEGORY, id);

        self.vol3d.threshold_dbz = float(k::THRESHOLD_DBZ);
        self.vol3d.threshold_mode = match text(k::THRESHOLD_MODE).as_str() {
            "below" => crate::vol3d::Vol3dThresholdMode::Below,
            _ => crate::vol3d::Vol3dThresholdMode::Above,
        };
        self.vol3d.opacity = float(k::OPACITY);
        self.vol3d.density = float(k::DENSITY);
        self.vol3d.shading = float(k::SHADING);
        self.vol3d.quality = match text(k::QUALITY).as_str() {
            "draft" => crate::vol3d::Vol3dQuality::Draft,
            "high" => crate::vol3d::Vol3dQuality::High,
            _ => crate::vol3d::Vol3dQuality::Balanced,
        };
        // The choice id is the box width in kilometres; the field is its
        // half. An unparsable id keeps the current box rather than guessing.
        if let Ok(size_km) = text(k::BOX_SIZE_KM).parse::<f32>() {
            self.vol3d.box_half_km = size_km * 0.5;
        }
        self.vol3d.vertical_exaggeration = float(k::VERTICAL_EXAGGERATION);
        self.vol3d.fov_scale = float(k::FOV_SCALE);
        self.vol3d.floor_mode = match text(k::FLOOR_MODE).as_str() {
            "off" => crate::vol3d::FloorMode::Off,
            "column-max" => crate::vol3d::FloorMode::ColumnMax,
            _ => crate::vol3d::FloorMode::LowestTilt,
        };
        self.vol3d.floor_opacity = float(k::FLOOR_OPACITY);
        self.vol3d.advanced.opacity_ramp_low_dbz = float(k::RAMP_LOW_DBZ);
        self.vol3d.advanced.opacity_ramp_high_dbz = float(k::RAMP_HIGH_DBZ);
        self.vol3d.advanced.opacity_ramp_gamma = float(k::RAMP_GAMMA);
        self.vol3d.advanced.opacity_ramp_floor = float(k::RAMP_FLOOR);
        self.vol3d.advanced.opacity_ramp_gain = float(k::RAMP_GAIN);
        self.vol3d.show_grid = toggle(k::SHOW_GRID);
        self.vol3d.show_box = toggle(k::SHOW_BOX);
        self.vol3d.show_labels = toggle(k::SHOW_LABELS);
    }

    /// The appearance the store asks for: theme plus the four axes.
    ///
    /// Resolved through the registry, so a stranger id (a theme from a newer
    /// build, a hand-edited file) reads back as the declared default without
    /// the stored string being touched - the analyst's choice comes back the
    /// moment the build that has it runs again. The rule itself lives once,
    /// in `theme::settings::appearance_from_ids`, which `main.rs` also calls
    /// before the first frame.
    fn appearance(&self) -> crate::theme::Appearance {
        use crate::theme::settings::keys;
        let text = |id: &str| {
            self.settings_store
                .effective_text(&self.settings_registry, keys::CATEGORY, id)
        };
        crate::theme::settings::appearance_from_ids(
            Some(&text(keys::THEME)),
            Some(&text(keys::ACCENT)),
            Some(&text(keys::CHROME_EDGES)),
            Some(&text(keys::DENSITY)),
            Some(&text(keys::UI_SCALE)),
        )
    }

    /// Rebuild [`SettingsCache`] from the store. Once at start and again on
    /// any reported change; never per frame.
    fn recompute_settings_cache(&mut self) {
        use crate::settings_ui::catalog::keys;
        let store = &self.settings_store;
        let registry = &self.settings_registry;
        let nav_float =
            |id: &str| store.effective_float(registry, keys::navigation::CATEGORY, id) as f32;
        // Exponent and dt remaps, so the tuned response curves in
        // `analyst_runtime::view` keep their shape with the user's numbers in
        // them - see [`crate::pane_canvas::NavTuning`] for the algebra.
        let nav = crate::pane_canvas::NavTuning {
            zoom_exp: nav_float(keys::navigation::ZOOM_PER_NOTCH).ln()
                / analyst_runtime::ZOOM_PER_NOTCH.ln(),
            pan_scale: nav_float(keys::navigation::KEY_PAN_RATE)
                / analyst_runtime::KEY_PAN_FRACTION_PER_SECOND,
            kzoom_exp: nav_float(keys::navigation::KEY_ZOOM_RATE).ln()
                / analyst_runtime::KEY_ZOOM_RATE_PER_SECOND.ln(),
            double_click_reset: store.effective_bool(
                registry,
                keys::navigation::CATEGORY,
                keys::navigation::DOUBLE_CLICK_RESET,
            ),
        };
        self.settings_cache = SettingsCache {
            nav,
            toolbar_style: match store
                .effective_text(
                    registry,
                    keys::appearance::CATEGORY,
                    keys::appearance::TOOLBAR,
                )
                .as_str()
            {
                "full" => ToolbarStyle::Everything,
                // Anything unrecognized lands on the compact bar, the same
                // stranger-value rule the theme follows: a value written by a
                // future build must not pick the style for it.
                _ => ToolbarStyle::Menus,
            },
            site_labels: match store
                .effective_text(registry, keys::map::CATEGORY, keys::map::SITE_LABELS)
                .as_str()
            {
                "always" => crate::pane_canvas::SiteLabelMode::Always,
                "never" => crate::pane_canvas::SiteLabelMode::Never,
                _ => crate::pane_canvas::SiteLabelMode::Auto,
            },
            site_markers: store.effective_bool(
                registry,
                keys::map::CATEGORY,
                keys::map::SITE_MARKERS,
            ),
            observations_enabled: store.effective_bool(
                registry,
                keys::observations::CATEGORY,
                keys::observations::ENABLED,
            ),
            placefiles_enabled: store.effective_bool(
                registry,
                keys::map::CATEGORY,
                keys::map::PLACEFILES_ENABLED,
            ),
            legend: store.effective_bool(registry, keys::radar::CATEGORY, keys::radar::LEGEND),
            sweep_animation: store.effective_bool(
                registry,
                keys::radar::CATEGORY,
                keys::radar::SWEEP_ANIMATION,
            ),
            sweep_speed: store.effective_float(
                registry,
                keys::radar::CATEGORY,
                keys::radar::SWEEP_SPEED,
            ) as f32,
            vrot_mps_first: store.effective_text(
                registry,
                keys::analysis::CATEGORY,
                keys::analysis::VROT_UNITS,
            ) == "mps",
            units: crate::units::UnitSystem {
                distance: crate::units::DistanceUnit::from_id(&store.effective_text(
                    registry,
                    keys::units::CATEGORY,
                    keys::units::DISTANCE,
                )),
                altitude: crate::units::AltitudeUnit::from_id(&store.effective_text(
                    registry,
                    keys::units::CATEGORY,
                    keys::units::ALTITUDE,
                )),
                zone: crate::units::TimeZoneChoice::from_id(&store.effective_text(
                    registry,
                    keys::units::CATEGORY,
                    keys::units::TIME_ZONE,
                )),
                clock: crate::units::ClockFormat::from_id(&store.effective_text(
                    registry,
                    keys::units::CATEGORY,
                    keys::units::CLOCK,
                )),
            },
            annotation: {
                let annotation_int =
                    |id: &str| store.effective_int(registry, keys::annotation::CATEGORY, id);
                let annotation_float = |id: &str| {
                    store.effective_float(registry, keys::annotation::CATEGORY, id) as f32
                };
                crate::annotation::Annotation {
                    ring_ladder: crate::annotation::RingLadder::from_id(&store.effective_text(
                        registry,
                        keys::annotation::CATEGORY,
                        keys::annotation::RING_LADDER,
                    )),
                    // The store has already clamped these into the ranges the
                    // catalog declares, so the `max(0)` is only about the cast:
                    // `i64 as usize` on a negative number is a very large
                    // positive one, and a ring count of 18 million would be a
                    // frozen frame rather than a wrong picture.
                    ring_count: annotation_int(keys::annotation::RING_COUNT).max(0) as usize,
                    ring_labels: store.effective_bool(
                        registry,
                        keys::annotation::CATEGORY,
                        keys::annotation::RING_LABELS,
                    ),
                    site_marker_points: annotation_float(keys::annotation::SITE_MARKER_SIZE),
                    site_label_points: annotation_float(keys::annotation::SITE_LABEL_SIZE),
                    site_declutter_max: annotation_int(keys::annotation::SITE_DECLUTTER_MAX).max(0)
                        as usize,
                    site_marker_max: annotation_int(keys::annotation::SITE_MARKER_MAX).max(0)
                        as usize,
                    range_decimals: annotation_int(keys::annotation::RANGE_DECIMALS).clamp(0, 9)
                        as u8,
                    coordinate_decimals: annotation_int(keys::annotation::COORDINATE_DECIMALS)
                        .clamp(0, 9) as u8,
                    corner_readout: crate::annotation::CornerReadout::from_id(
                        &store.effective_text(
                            registry,
                            keys::annotation::CATEGORY,
                            keys::annotation::CORNER_READOUT,
                        ),
                    ),
                }
            },
            // Kilometres in the menu, metres to the sampler. Fenced twice, on
            // the same principle as the network tuning: the store has already
            // clamped this into the catalog's declared 4-24 km, and
            // `xsection::sanitized_top_m` is the fence the code itself will
            // accept from a hand-edited settings file.
            xsection_top_m: crate::xsection::sanitized_top_m(
                (store.effective_int(registry, keys::xsection::CATEGORY, keys::xsection::TOP_KM)
                    * 1_000) as f32,
            ),
            // The store has already clamped this into the catalog's 100-3000 ms
            // range; `max(0)` is only about the `i64 as u64` cast.
            loop_frame_time: Duration::from_millis(
                store
                    .effective_int(registry, keys::data::CATEGORY, keys::data::LOOP_FRAME_MS)
                    .max(0) as u64,
            ),
            // The store has already clamped the dwell into the catalog's
            // declared range; `max(1)` is only about the `i64 as usize` cast,
            // which would turn a negative into an enormous positive and ask
            // the estimator for a dwell longer than any record.
            iq_controls: iq_controls_from(registry, store),
            iq_spectrum_channel: usize::from(
                store.effective_text(
                    registry,
                    keys::timeseries::CATEGORY,
                    keys::timeseries::SPECTRUM_CHANNEL,
                ) == crate::settings_ui::catalog::timeseries_limits::CHANNEL_VERTICAL,
            ),
            // Read, never written back. Unlike quality and the basemap, the
            // filter is not mirrored from live state every frame - it has no
            // live state other than the store - so a settings file carrying an
            // out-of-range or hand-edited value resolves to something sane on
            // screen and keeps its own text on disk.
            gate_filter: crate::gate_filter_ui::filter_from_settings(registry, store),
        };
        self.apply_network_settings();
    }

    /// Push the network policy at the two places that own network behaviour:
    /// the live worker's shared handle, and the data layer's transfer
    /// globals. Both clamp again on the way in, so this cannot be the step
    /// that lets a bad value through.
    fn apply_network_settings(&self) {
        use crate::settings_ui::catalog::keys;
        let store = &self.settings_store;
        let registry = &self.settings_registry;
        let seconds = |category: &str, id: &str| {
            Duration::from_secs_f64(store.effective_float(registry, category, id).max(0.0))
        };
        let tuning = crate::net_tuning::NetTuning {
            live_poll: seconds(keys::data::CATEGORY, keys::data::POLL_SECONDS),
            archive_poll: Duration::from_secs(
                store
                    .effective_int(
                        registry,
                        keys::network::CATEGORY,
                        keys::network::ARCHIVE_POLL_SECONDS,
                    )
                    .max(0) as u64,
            ),
            archive_lead_minutes: store.effective_int(
                registry,
                keys::network::CATEGORY,
                keys::network::ARCHIVE_LEAD_MINUTES,
            ),
            stall_after: Duration::from_secs(
                store
                    .effective_int(
                        registry,
                        keys::network::CATEGORY,
                        keys::network::STALL_AFTER_SECONDS,
                    )
                    .max(0) as u64,
            ),
            live_cache_bytes: (store
                .effective_int(
                    registry,
                    keys::data::CATEGORY,
                    keys::data::LIVE_CACHE_LIMIT_MB,
                )
                .max(0) as u64)
                .saturating_mul(1024 * 1024),
            download_batch: store
                .effective_int(
                    registry,
                    keys::network::CATEGORY,
                    keys::network::DOWNLOAD_BATCH,
                )
                .max(0) as usize,
            download_attempts: store
                .effective_int(
                    registry,
                    keys::network::CATEGORY,
                    keys::network::DOWNLOAD_ATTEMPTS,
                )
                .max(0) as usize,
            retry_backoff: Duration::from_millis(
                store
                    .effective_int(
                        registry,
                        keys::network::CATEGORY,
                        keys::network::RETRY_BACKOFF_MS,
                    )
                    .max(0) as u64,
            ),
        }
        .clamped();
        self.live_service.tuning().set(tuning);
        // The data layer's half. Read back out of the clamped policy rather
        // than out of the store, so the two halves cannot disagree about what
        // the analyst asked for.
        data_source::tuning::set_transfer_tuning(
            tuning.download_batch,
            tuning.download_attempts,
            tuning.retry_backoff,
        );
    }

    /// The display-follow policy is independent of how often the live
    /// worker polls for chunks. Keeping those clocks separate lets an analyst
    /// request, for example, a new eligible sweep every 30 seconds while the
    /// feed still notices incoming data immediately.
    fn live_follow_policy(&self) -> live_follow::LiveFollowPolicy {
        use crate::settings_ui::catalog::keys::data as key;

        live_follow::LiveFollowPolicy {
            enabled: self.settings_store.effective_bool(
                &self.settings_registry,
                key::CATEGORY,
                key::FOLLOW_LOW_TILTS_ENABLED,
            ),
            max_elevation_deg: self.settings_store.effective_float(
                &self.settings_registry,
                key::CATEGORY,
                key::FOLLOW_MAX_ELEVATION_DEG,
            ) as f32,
            min_interval: Duration::from_secs(
                self.settings_store
                    .effective_int(
                        &self.settings_registry,
                        key::CATEGORY,
                        key::FOLLOW_MIN_SWEEP_INTERVAL_SECONDS,
                    )
                    .max(0) as u64,
            ),
        }
    }

    fn reset_live_follow_state(&mut self) {
        self.live_follow_last_scan = [None; analyst_runtime::MAX_PANES];
        self.live_follow_manual_hold = [None; analyst_runtime::MAX_PANES];
    }

    fn set_live_follow_enabled(&mut self, enabled: bool) {
        use crate::settings_ui::catalog::keys::data as key;

        if self.settings_store.set(
            key::CATEGORY,
            key::FOLLOW_LOW_TILTS_ENABLED,
            settings::SettingValue::Bool(enabled),
        ) {
            self.apply_changed_setting(key::CATEGORY, key::FOLLOW_LOW_TILTS_ENABLED);
        }
    }

    /// Apply every persisted observation control to the single shared worker.
    /// Display changes only repaint egui shapes; they never discard an already
    /// rendered radar sweep or restart an otherwise healthy acquisition.
    fn apply_surface_observation_settings(&mut self) {
        use crate::settings_ui::catalog::keys::observations as key;

        let store = &self.settings_store;
        let registry = &self.settings_registry;
        let enabled = store.effective_bool(registry, key::CATEGORY, key::ENABLED);
        let toggle = |id| store.effective_bool(registry, key::CATEGORY, id);
        self.surface_observations
            .set_plot_options(surface_observations::ObservationPlotOptions {
                show_temperature: toggle(key::SHOW_TEMPERATURE),
                show_dewpoint: toggle(key::SHOW_DEWPOINT),
                show_wind_barbs: toggle(key::SHOW_WIND_BARBS),
                show_station_id: toggle(key::SHOW_STATION_ID),
                show_sky_cover: toggle(key::SHOW_SKY_COVER),
                show_weather: toggle(key::SHOW_WEATHER),
                show_visibility: toggle(key::SHOW_VISIBILITY),
                show_pressure: toggle(key::SHOW_PRESSURE),
                show_gusts: toggle(key::SHOW_GUSTS),
                declutter_px: store.effective_float(registry, key::CATEGORY, key::DECLUTTER_POINTS)
                    as f32,
                fahrenheit: toggle(key::FAHRENHEIT),
            });
        self.surface_observations
            .set_mesonet_enabled(toggle(key::MESONET_ENABLED));
        self.surface_observations
            .set_refresh_interval(Duration::from_secs(
                store
                    .effective_int(registry, key::CATEGORY, key::REFRESH_SECONDS)
                    .max(1) as u64,
            ));
        self.surface_observations.set_enabled(enabled);
    }

    fn set_surface_observations_enabled(&mut self, enabled: bool) {
        use crate::settings_ui::catalog::keys::observations as key;

        if self.settings_store.set(
            key::CATEGORY,
            key::ENABLED,
            settings::SettingValue::Bool(enabled),
        ) {
            self.apply_changed_setting(key::CATEGORY, key::ENABLED);
            self.recompute_settings_cache();
        }
    }

    fn set_mesonet_observations_enabled(&mut self, enabled: bool) {
        use crate::settings_ui::catalog::keys::observations as key;

        if self.settings_store.set(
            key::CATEGORY,
            key::MESONET_ENABLED,
            settings::SettingValue::Bool(enabled),
        ) {
            self.apply_changed_setting(key::CATEGORY, key::MESONET_ENABLED);
            self.recompute_settings_cache();
        }
    }

    fn set_placefiles_enabled(&mut self, enabled: bool) {
        use crate::settings_ui::catalog::keys::map as key;

        if self.settings_store.set(
            key::CATEGORY,
            key::PLACEFILES_ENABLED,
            settings::SettingValue::Bool(enabled),
        ) {
            self.recompute_settings_cache();
        }
    }

    /// The newest usable, possibly still-arriving sweep that can honestly
    /// serve one pane's own product. Selecting it while it is in progress is
    /// what lets the existing live animator reveal its real incoming radials.
    /// Exact producer-native fields and volume-integrated products have no
    /// equivalent single-sweep following contract.
    fn live_follow_candidate(
        &self,
        pane: PaneId,
        policy: live_follow::LiveFollowPolicy,
        last_followed_scan: Option<DateTime<Utc>>,
    ) -> Option<live_follow::LiveFollowCandidate> {
        let frame = self.history.current()?;
        let capabilities = self.capabilities.as_deref()?;
        let product = modeled_product_or_source_field(&self.workspace.pane(pane).product).ok()?;
        if product.derived_volume().is_some() {
            return None;
        }
        let descriptor = product.descriptor();
        let moment = descriptor.computation.source_moment();
        live_follow::newest_eligible_cut(
            &frame.volume,
            capabilities,
            &moment,
            descriptor.cut_policy,
            policy,
            last_followed_scan,
        )
    }

    /// Apply one independently selected, product-compatible sweep per pane.
    /// This runs only at the live edge: historical scrubbing, playback and
    /// local files retain their explicitly selected tilts.
    fn follow_live_low_tilts(&mut self) {
        if self.live_site.is_none()
            || !self.history.follows_live()
            || self.history.playback() == PlaybackState::Playing
        {
            return;
        }
        let policy = self.live_follow_policy();
        if !policy.enabled {
            return;
        }

        let mut changed = Vec::new();
        for pane in self.workspace.visible_panes().iter().copied() {
            let index = pane.index();
            let Some(candidate) =
                self.live_follow_candidate(pane, policy, self.live_follow_last_scan[index])
            else {
                continue;
            };
            if self.live_follow_manual_hold[index]
                .is_some_and(|held_scan| candidate.scan_time <= held_scan)
            {
                continue;
            }
            let Ok(cut_index) = u16::try_from(candidate.cut_index) else {
                continue;
            };
            let selected = TiltSelection::CutIndex(cut_index);
            if self.workspace.pane(pane).tilt != selected {
                self.workspace.pane_mut(pane).tilt = selected;
                changed.push(pane);
            }
            self.live_follow_last_scan[index] = Some(candidate.scan_time);
            self.live_follow_manual_hold[index] = None;
        }

        if self.vrot_pane.is_some_and(|pane| changed.contains(&pane)) {
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::DifferentCut);
        }
        self.invalidate_semantic_panes(&changed);
    }

    /// Keep a manual tilt choice until a genuinely newer usable low sweep
    /// arrives, even if that next sweep is still in progress.
    /// Using the measured sweep frontier, rather than wall-clock delay, keeps
    /// a quiet feed from repeatedly stealing back the analyst's selection.
    fn hold_live_follow_for_manual_tilts(&mut self, panes: &[PaneId]) {
        let mut policy = self.live_follow_policy();
        if self.live_site.is_none() || !policy.enabled {
            return;
        }
        policy.min_interval = Duration::ZERO;
        for pane in panes {
            let frontier = self
                .live_follow_candidate(*pane, policy, None)
                .map(|candidate| candidate.scan_time)
                .or_else(|| self.history.current().map(|frame| frame.volume.volume_time));
            self.live_follow_manual_hold[pane.index()] = frontier;
        }
    }

    /// Apply one changed setting to live state. The cache rebuild runs once
    /// per change batch in `settings_frame`; these arms cover the values that
    /// live somewhere other than the cache.
    fn apply_changed_setting(&mut self, category: &str, id: &str) {
        use crate::settings_ui::catalog::keys;
        match (category, id) {
            (keys::radar::CATEGORY, keys::radar::QUALITY) => {
                let text = self.settings_store.effective_text(
                    &self.settings_registry,
                    keys::radar::CATEGORY,
                    keys::radar::QUALITY,
                );
                if let Some(quality) = crate::settings_ui::sync::quality_from_id(&text)
                    && quality != self.quality
                {
                    self.quality = quality;
                    // Same data, different picture - the exact invalidation
                    // the toolbar's quality picker runs.
                    self.invalidate_view_panes(self.workspace.visible_panes());
                }
            }
            (keys::map::CATEGORY, keys::map::BASEMAP_STYLE) => {
                let text = self.settings_store.effective_text(
                    &self.settings_registry,
                    keys::map::CATEGORY,
                    keys::map::BASEMAP_STYLE,
                );
                if let Some(preset) = map_scene::MapStylePreset::from_id(&text)
                    && preset.style() != self.map_scene.style()
                {
                    // `set_style` bumps the style clock and drops retained
                    // geometry, so the panes rebuild without more help here.
                    self.map_scene.set_style(preset.style());
                }
            }
            (keys::map::CATEGORY, keys::map::IMAGERY_PROVIDER) => {
                let key = self.settings_store.effective_text(
                    &self.settings_registry,
                    keys::map::CATEGORY,
                    keys::map::IMAGERY_PROVIDER,
                );
                self.apply_imagery_provider(&key);
            }
            (keys::map::CATEGORY, keys::map::IMAGERY_DIM | keys::map::IMAGERY_DIM_AUTO) => {
                self.apply_imagery_dim();
            }
            (keys::observations::CATEGORY, _) => {
                self.apply_surface_observation_settings();
            }
            (
                keys::analysis::CATEGORY,
                keys::analysis::STORM_MOTION_DIR | keys::analysis::STORM_MOTION_SPEED,
            ) => {
                self.apply_storm_motion_settings();
                // Storm-relative products draw the same data differently now.
                self.invalidate_view_panes(self.workspace.visible_panes());
            }
            (keys::data::CATEGORY, keys::data::HISTORY_MAX_FRAMES | keys::data::HISTORY_MAX_MB) => {
                let policy = if self.live_site.is_some() {
                    self.live_history_policy()
                } else {
                    self.configured_history_policy()
                };
                // `set_policy`, not a rebuild: a positive limit can shrink
                // local history, while zero means Unlimited there. A live
                // session substitutes its bounded fallback for a zero
                // dimension. Every resulting eviction is surfaced instead of
                // disappearing behind a settings change.
                let evicted = self.history.set_policy(policy);
                if !evicted.is_empty() {
                    if let Some(sequence) = self.file_sequence.as_mut() {
                        sequence.evicted_frames = sequence
                            .evicted_frames
                            .saturating_add(evicted.len());
                    }
                    self.status = format!(
                        "History limit applied · {} frame(s) evicted · {} retained",
                        evicted.len(),
                        self.history.len()
                    );
                }
            }
            (
                keys::data::CATEGORY,
                keys::data::FOLLOW_LOW_TILTS_ENABLED
                | keys::data::FOLLOW_MAX_ELEVATION_DEG
                | keys::data::FOLLOW_MIN_SWEEP_INTERVAL_SECONDS,
            ) => {
                // A deliberately changed ceiling or cadence is new analyst
                // intent, so reconsider the current completed sweep now.
                self.reset_live_follow_state();
                self.follow_live_low_tilts();
            }
            (
                keys::radar::CATEGORY,
                keys::radar::FILTER_MIN_DBZ
                | keys::radar::FILTER_VEL_NEEDS_DBZ
                | keys::radar::FILTER_MIN_RHO
                | keys::radar::FILTER_HIDE_RF
                | keys::radar::FILTER_MIN_RANGE_KM,
            ) => {
                // Same data, different picture - the same invalidation a
                // quality change gets. The cache rebuild that actually reads
                // the new numbers is the caller's, once per change batch; this
                // arm only has to make the panes ask for a new raster.
                self.invalidate_view_panes(self.workspace.visible_panes());
            }
            (keys::vol3d::CATEGORY, _) => self.apply_vol3d_settings(),
            // The one settings page that changes a MEASUREMENT rather than a
            // picture of one. The dwell, the window and the censor decide what
            // the moments ARE on a Level 1 file, so the estimator is re-run
            // over the pulses already in memory and the frame is replaced.
            // Nothing here reads the file again.
            (keys::timeseries::CATEGORY, _) => self.apply_timeseries_settings(),
            // The network policy lives in the live worker and the data layer
            // rather than in the cache, so it is pushed rather than read.
            // `recompute_settings_cache` already does this for every change;
            // naming the arms keeps the dispatch honest about what each key
            // touches instead of relying on that side effect.
            (keys::network::CATEGORY, _)
            | (keys::data::CATEGORY, keys::data::POLL_SECONDS | keys::data::LIVE_CACHE_LIMIT_MB) => {
                self.apply_network_settings();
            }
            // A unit or an annotation change repaints the chrome, which the
            // pane draws every frame anyway - but the probe readout is cached
            // from the previous frame's sample, so it would keep its old units
            // until the pointer moved.
            (keys::units::CATEGORY, _) | (keys::annotation::CATEGORY, _) => {
                for pane in self.panes.iter_mut() {
                    pane.probe_text = None;
                }
            }
            // The slice top is the one setting on this list that is not a
            // display choice: it changes what the sampler is asked for. It
            // needs nothing here because `xsection` carries the top in its own
            // rebuild key and compares it against the recomputed cache on the
            // next frame - named so the dispatch stays honest about that.
            (keys::xsection::CATEGORY, _)
            // Same for the loop's frame time: `advance_playback` reads it from
            // the cache on the frame after the change.
            | (keys::data::CATEGORY, keys::data::LOOP_FRAME_MS) => {}
            // Everything else is either cache-backed (rebuilt by the caller),
            // read at startup, or declared pending its wiring seam.
            _ => {}
        }
    }

    /// The settings window's own state.
    ///
    /// `pub(crate)` for `examples/profiles_proof.rs`, which opens the window on
    /// the Profiles page and then photographs and CLICKS the real page - the
    /// same reason `toolbar` is `pub(crate)`. Opening a window is not the
    /// behaviour that proof is about, and steering it through simulated clicks
    /// on a title bar would make the proof about egui instead.
    ///
    /// `allow`, not `expect`: dead code is judged per compilation unit, and
    /// this method has a caller in one (the example) and none in the other
    /// (the binary), so an `expect` would be unfulfilled in the example.
    /// The volume currently on the panes.
    ///
    /// `pub(crate)` for `examples/iq_proof.rs`, which counts the radials a
    /// dwell change produced. Reading the radial count off the shipped state is
    /// the difference between proving the slider reached the ESTIMATOR and
    /// proving it reached the settings file, which is the thing that would pass
    /// while the field never changed.
    ///
    /// `allow`, not `expect`: dead code is judged per compilation unit, and
    /// this has a caller in the example and none in the binary.
    #[allow(dead_code)]
    pub(crate) fn current_volume(&self) -> Option<&RadarVolume> {
        self.history.current().map(|frame| frame.volume.as_ref())
    }

    /// Write a settings value and take the application through the same two
    /// steps the settings window's own dispatch runs afterwards.
    ///
    /// `pub(crate)` for `examples/iq_proof.rs`, and deliberately not a
    /// shortcut: a proof that reached into `settings_cache` directly would
    /// photograph a field the shipped path never produced. The ORDER here is
    /// the shipped order - apply, then recompute - which is exactly why
    /// `apply_timeseries_settings` reads the store rather than the cache.
    ///
    /// `allow`, not `expect`: see [`Self::current_volume`].
    #[allow(dead_code)]
    pub(crate) fn apply_setting_for_proof(
        &mut self,
        category: &str,
        id: &str,
        value: settings::SettingValue,
    ) {
        self.settings_store.set(category, id, value);
        self.apply_changed_setting(category, id);
        self.recompute_settings_cache();
    }

    #[allow(dead_code)]
    pub(crate) fn settings_ui_mut(&mut self) -> &mut crate::settings_ui::SettingsUi {
        &mut self.settings_ui
    }

    /// The one line the running application says about profiles: which one is
    /// active and whether the settings have moved away from it since.
    ///
    /// `None` when the analyst has never met a profile and has changed
    /// nothing - a fresh install has no profile worth naming - and when the
    /// Profiles page's own setting says not to show it.
    fn active_profile_line(&mut self) -> Option<String> {
        use crate::settings_ui::catalog::keys;
        if !self.settings_store.effective_bool(
            &self.settings_registry,
            keys::profiles::CATEGORY,
            keys::profiles::SHOW_IN_STATUS,
        ) {
            return None;
        }
        let (name, modified) = self
            .settings_ui
            .profiles
            .summary(&self.settings_registry, &self.settings_store)?;
        Some(if modified {
            format!("Profile: {name} (modified)")
        } else {
            format!("Profile: {name}")
        })
    }

    /// A profile switch: the settings document has already been replaced, and
    /// this is where it reaches live state.
    ///
    /// Two halves, and neither of them is a list of settings:
    ///
    /// * [`Self::apply_settings_document`], which is the same function a
    ///   freshly read settings file goes through at startup - colour tables,
    ///   the workspace snapshot, the values read once;
    /// * every setting the REGISTRY declares, pushed into `outcome.changed` so
    ///   the caller's existing per-setting dispatch runs over all of them.
    ///
    /// The second half is the point. A setting added by any later piece of
    /// work is declared in the catalog and wired into `apply_changed_setting`
    /// (or into the settings cache) because that is what makes it work when
    /// someone drags its slider; enumerating the registry here means a profile
    /// switch inherits that wiring for free, on the day it lands, with no
    /// change to this function. A hand-written list of keys would have to be
    /// remembered instead - and would fail silently when it was not.
    fn apply_switched_profile(&mut self, outcome: &mut crate::settings_ui::SettingsOutcome) {
        self.apply_settings_document();
        for category in self.settings_registry.categories() {
            for spec in &category.settings {
                outcome.changed.push((category.id.clone(), spec.id.clone()));
            }
        }
        // The document brought its own colour tables and its own panes: the
        // same invalidation a palette change and a product change each get,
        // because a switch can be both at once.
        outcome.palette_changed = true;
        self.invalidate_semantic_panes(self.workspace.visible_panes());
        if let Some(name) = settings::profiles::active_profile(self.settings_store.document()) {
            self.status = format!("Profile '{name}' applied");
        }
    }

    /// The settings window, its outcome dispatch, the live-state mirror and
    /// the debounced autosave: the whole persistence pass, once per frame.
    fn settings_frame(&mut self, context: &egui::Context) {
        use crate::settings_ui::catalog::keys;
        let mut outcome = crate::settings_ui::draw_settings_window(
            context,
            &mut self.settings_ui,
            crate::settings_ui::SettingsWindowInput {
                registry: &self.settings_registry,
                store: &mut self.settings_store,
                color_tables: Some(&mut self.color_tables),
                user_tables: Some(self.user_tables.library()),
            },
        );
        if outcome.profile_switched {
            self.apply_switched_profile(&mut outcome);
        }
        if outcome.user_tables_rescan {
            self.rescan_user_tables();
        }
        if outcome.palette_changed {
            // Exactly what the toolbar's palette picker does: new colours,
            // same data.
            self.palette_clock.bump();
            self.invalidate_view_panes(self.workspace.visible_panes());
        }
        if let Some((family, table)) = outcome.palette_edit {
            // The settings page names nothing from this crate, so the
            // shipped-preset question is asked here - of the same function the
            // picker asks, so the two rows cannot answer it differently.
            let duplicate = color_tables::is_builtin_table(family, table.base_name());
            self.palette_editor
                .edit_or_duplicate(family, &table, duplicate);
        }
        for (category, id) in &outcome.changed {
            self.apply_changed_setting(category, id);
            // The appearance axes need the context, which
            // `apply_changed_setting` deliberately does not carry - they are
            // the settings that restyle egui itself rather than the app's own
            // state. All five re-install together: they are one
            // `theme::Appearance`, and installing half of one is how a theme
            // change loses somebody's density.
            if category.as_str() == crate::theme::settings::keys::CATEGORY
                && crate::theme::settings::keys::ALL.contains(&id.as_str())
            {
                crate::theme::apply(context, &self.appearance());
            }
        }
        if !outcome.changed.is_empty() {
            self.recompute_settings_cache();
        }

        // Mirror live state into the store, every frame. Free when nothing
        // moved - `set`/`set_workspace` compare before dirtying - and it is
        // what lets the toolbar's own pickers persist with zero per-widget
        // code.
        let mut workspace = crate::settings_ui::sync::capture_workspace(&self.workspace);
        // Preserving, not unconditional: a stored palette name whose file is
        // missing right now resolves to the family default on screen, and
        // this mirror must not write that default over the analyst's choice.
        workspace.palettes = crate::settings_ui::palettes::capture_palettes_preserving(
            &self.color_tables,
            &self.settings_store.workspace().palettes,
            self.user_tables.library(),
        );
        workspace.source_field_palettes = self.source_field_palettes.capture();
        workspace.last_site = self.live_site.clone();
        workspace.show_warnings = Some(self.show_warnings);
        workspace.window = context.input(|input| {
            let viewport = input.viewport();
            crate::settings_ui::sync::window_snapshot(
                viewport.outer_rect.map(|rect| (rect.min.x, rect.min.y)),
                viewport
                    .inner_rect
                    .map(|rect| (rect.width(), rect.height())),
                viewport.maximized.unwrap_or(false),
            )
        });
        self.settings_store.set_workspace(workspace);
        // `None` keeps the stored id when a custom quality no preset names is
        // live: overwriting with a nearest guess would erase the analyst's
        // last real choice.
        if let Some(id) = crate::settings_ui::sync::quality_id(self.quality) {
            self.settings_store.set(
                keys::radar::CATEGORY,
                keys::radar::QUALITY,
                settings::SettingValue::Text(id.to_owned()),
            );
        }
        self.settings_store.set(
            keys::map::CATEGORY,
            keys::map::BASEMAP_STYLE,
            settings::SettingValue::Text(
                map_scene::MapStylePreset::for_style(self.map_scene.style())
                    .unwrap_or_default()
                    .id()
                    .to_owned(),
            ),
        );
        self.settings_store.set(
            keys::map::CATEGORY,
            keys::map::IMAGERY_PROVIDER,
            settings::SettingValue::Text(
                self.map_scene
                    .tile_provider()
                    .map(map_scene::TileProvider::key)
                    .unwrap_or("none")
                    .to_owned(),
            ),
        );
        if let Some(Err(error)) = self.settings_store.autosave_tick() {
            self.status = format!("Settings save failed: {error}");
        }
    }

    /// The Vrot value pair in the analyst's chosen unit order. The value is
    /// always shown in both units; the setting only chooses which one leads.
    fn vrot_readout(&self, measurement: &crate::vrot::VrotMeasurement) -> String {
        if self.settings_cache.vrot_mps_first {
            format!(
                "Vrot {:.1} m/s ({:.0} kt)",
                measurement.vrot_mps,
                measurement.vrot_knots()
            )
        } else {
            format!(
                "Vrot {:.0} kt ({:.1} m/s)",
                measurement.vrot_knots(),
                measurement.vrot_mps
            )
        }
    }

    /// [`crate::vrot::report`] with the lead pair in the analyst's chosen
    /// order. `vrot::report` itself stays kt-first - its rationale (Thompson
    /// et al. 2017) is about the science, and this is the display layer,
    /// which is exactly what the setting governs.
    fn vrot_report_line(&self, measurement: &crate::vrot::VrotMeasurement) -> String {
        let units = self.settings_cache.units;
        if !self.settings_cache.vrot_mps_first {
            return crate::vrot::report(measurement, units);
        }
        let mut text = format!(
            "{} | delta-V {:.1} m/s | separation {} | height {} ARL | {:.1} deg cut {}",
            self.vrot_readout(measurement),
            measurement.delta_v_mps,
            units.distance(measurement.separation_km, 2),
            units.altitude(measurement.couplet_height_arl_m, 2),
            measurement.first.elevation_deg,
            measurement.first.cut_index,
        );
        for warning in &measurement.warnings {
            text.push_str(" | WARNING: ");
            text.push_str(warning.label());
        }
        text
    }

    /// Apply a camera stated at startup to every pane, so a particular pan or
    /// zoom can be reproduced without driving the window by hand.
    pub fn set_initial_camera(
        &mut self,
        zoom_km_per_point: Option<f32>,
        center_km: Option<(f64, f64)>,
    ) {
        if zoom_km_per_point.is_none() && center_km.is_none() {
            return;
        }
        let mut panes = Vec::with_capacity(analyst_runtime::MAX_PANES);
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            let camera = &mut self.workspace.pane_mut(pane).camera;
            if let Some(km_per_point) = zoom_km_per_point {
                camera.km_per_point = km_per_point;
            }
            if let Some((east_km, north_km)) = center_km {
                camera.center_east_km = east_km;
                camera.center_north_km = north_km;
            }
            *camera = camera.sanitized();
            panes.push(pane);
        }
        self.invalidate_view_panes(&panes);
    }

    /// Open every pane on a product stated at startup.
    ///
    /// This exists for the same reason the camera options do. Windows refuses a
    /// foreground change from a background process, so synthetic clicks land in
    /// whatever window happens to be focused; a product cannot be selected by
    /// hand in a captured session. Without this flag the only product that
    /// could ever be photographed on real data is the default one.
    pub fn set_initial_product(&mut self, product: Option<DisplayProduct>) {
        let Some(product) = product else {
            return;
        };
        let id = product.product_id();
        let mut panes = Vec::with_capacity(analyst_runtime::MAX_PANES);
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            self.workspace.pane_mut(pane).product = id.clone();
            panes.push(pane);
        }
        self.invalidate_semantic_panes(&panes);
    }

    /// Open the 3D explorer at startup, so a particular view can be captured
    /// without driving the window by hand.
    pub fn set_vol3d_open(&mut self, open: bool) {
        self.vol3d.open = open;
    }

    /// Invalidate any metadata planning result that has not yet been acted
    /// upon. The worker may still finish an operating-system read, but its
    /// generation can no longer clear the current session or start a load.
    fn cancel_playlist_preflight(&mut self) {
        self.playlist_preflight_clock.bump();
        self.pending_playlist_preflight = None;
        self.pending_playlist_confirmation = None;
    }

    fn poll_playlist_preflight(&mut self) {
        while let Some(update) = self.playlist_preflight_service.try_recv() {
            let is_current = self
                .pending_playlist_preflight
                .as_ref()
                .is_some_and(|pending| pending.generation == update.generation);
            if !is_current {
                continue;
            }
            self.pending_playlist_preflight = None;
            if update.estimate.requires_confirmation() {
                self.status = format!(
                    "Large playlist awaiting confirmation · {} selected · estimated {} decoded RAM",
                    update.paths.len(),
                    crate::playlist_preflight::format_binary_bytes(
                        update.estimate.estimated_decoded_bytes
                    )
                );
                self.pending_playlist_confirmation = Some(PendingPlaylistConfirmation {
                    paths: update.paths,
                    estimate: update.estimate,
                });
            } else {
                self.start_load_sequence(update.paths, update.estimate);
            }
        }
    }

    /// Retire the previous session before a local file or file playlist.
    fn begin_local_session(&mut self) -> analyst_runtime::Generation {
        let history_policy = self.configured_history_policy();
        self.cancel_playlist_preflight();
        if self.live_site.is_some() {
            self.live_service.stop();
            self.live_site = None;
            self.live_status.clear();
        }
        // A local file is not a feed. Whatever the last session said about
        // KUEX's prefix must not follow the analyst into an archive volume.
        self.live_feed = None;
        self.reset_live_follow_state();
        // The file may be any radar's: the coordinates a Vrot endpoint was
        // clicked at name a different place under the new session, so the
        // measurement is retired before the world changes under it.
        self.vrot_state
            .mark_stale(crate::vrot::StaleReason::DifferentSite);
        let generation = self.session_clock.bump();
        self.frame_clock.bump();
        self.history.clear();
        let evicted = self.history.set_policy(history_policy);
        debug_assert!(evicted.is_empty(), "an empty history cannot evict");
        self.load_ms = None;
        self.clear_all_panes();
        generation
    }

    fn begin_load(&mut self, path: PathBuf) {
        self.file_sequence = None;
        self.sequence_status = None;
        self.sequence_detail = None;
        let generation = self.begin_local_session();
        self.source_path_text = path.display().to_string();
        self.status = format!("Loading {}", path.display());
        let source_label = path.display().to_string();
        if let Err(request) = self.load_service.request(LoadRequest {
            generation,
            path,
            origin: FrameOrigin::Local,
            final_stage: FrameStage::Complete,
            source_label,
            iq_controls: self.settings_cache.iq_controls,
        }) {
            self.status = format!("load worker is closed: {}", request.path.display());
        }
    }

    /// Decode local files one at a time into independent history frames.
    ///
    /// Input order is made deterministic before the first request; history
    /// then applies its existing volume-time ordering. A failed file advances
    /// to the next. Files from another radar are refused because this
    /// workspace has one map anchor and cannot honestly paint two sites on it.
    fn begin_load_sequence(&mut self, paths: Vec<PathBuf>) {
        let paths = ordered_unique_paths(paths);
        if paths.is_empty() {
            return;
        }

        self.cancel_playlist_preflight();
        let selected = paths.len();
        let generation = self.playlist_preflight_clock.bump();
        self.pending_playlist_preflight = Some(PendingPlaylistPreflight {
            generation,
            selected,
        });
        self.status = format!("Estimating playlist memory · {selected} selected");
        if let Err(error) = self.playlist_preflight_service.request(generation, paths) {
            self.pending_playlist_preflight = None;
            self.status = format!(
                "Playlist preflight could not start ({error}) · {selected} selected · 0 decoded · 0 logical volumes · 0 failed"
            );
        }
    }

    /// Begin a selection whose planning estimate has either stayed below the
    /// warning threshold or been explicitly accepted by the operator.
    fn start_load_sequence(
        &mut self,
        paths: Vec<PathBuf>,
        preflight: crate::playlist_preflight::PlaylistRamEstimate,
    ) {
        self.pending_playlist_confirmation = None;

        if paths.len() == 1 {
            // Planning still runs for one file, but it is not a playlist:
            // its raw Level 1 pulses must remain available for reprocessing.
            self.begin_load(paths.into_iter().next().expect("one approved path"));
            return;
        }

        let first = paths[0].display().to_string();
        self.begin_local_session();
        self.source_path_text = first;
        self.file_sequence = Some(FileSequence {
            paths,
            preflight,
            next: 0,
            loaded: 0,
            failures: Vec::new(),
            site_id: None,
            site_position: None,
            level1_files: 0,
            pending_assembly: None,
            assembled_files: 0,
            assembled_groups: 0,
            assembly_refusals: Vec::new(),
            evicted_frames: 0,
        });
        self.update_sequence_status("starting");
        self.request_next_sequence_file();
    }

    fn request_next_sequence_file(&mut self) {
        let next = self.file_sequence.as_mut().and_then(|sequence| {
            let path = sequence.paths.get(sequence.next)?.clone();
            sequence.next += 1;
            Some(path)
        });
        let Some(path) = next else {
            self.flush_pending_sweep_assembly();
            self.finish_file_sequence();
            return;
        };

        let source_label = path.display().to_string();
        self.update_sequence_status("loading");
        self.status = format!("Loading {source_label}");
        let request = LoadRequest {
            generation: self.session_clock.current(),
            path,
            origin: FrameOrigin::Local,
            final_stage: FrameStage::Complete,
            source_label,
            iq_controls: self.settings_cache.iq_controls,
        };
        if let Err(request) = self.load_service.request(request) {
            let message = "load worker is closed".to_owned();
            if let Some(sequence) = self.file_sequence.as_mut() {
                sequence.failures.push((request.path, message.clone()));
                // A closed worker cannot serve any remaining request. Record
                // them explicitly rather than pretending they were attempted.
                for path in sequence.paths.drain(sequence.next..) {
                    sequence.failures.push((path, message.clone()));
                }
            }
            self.finish_file_sequence();
        }
    }

    fn finish_sequence_failure(&mut self, message: String) {
        // A missing or corrupt member is a hard boundary. Even if the next
        // readable file repeats the same three-digit sequence, joining across
        // bytes we could not inspect would claim continuity we did not prove.
        self.flush_pending_sweep_assembly();
        if let Some(sequence) = self.file_sequence.as_mut() {
            let path = sequence
                .current_path()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("unknown file"));
            sequence.failures.push((path, message));
        }
        self.request_next_sequence_file();
    }

    fn finish_sequence_volume(&mut self, mut loaded: LoadedVolume) {
        // Preserve the same sourced site lookup a single Level 1 open gets,
        // before discarding the raw pulses that cannot safely live in history.
        self.locate_loaded_time_series(&mut loaded);
        let site = loaded.volume.site.id.trim().to_owned();
        let position = (
            loaded.volume.site.latitude_deg,
            loaded.volume.site.longitude_deg,
        );
        let expected = self
            .file_sequence
            .as_ref()
            .and_then(|sequence| sequence.site_id.as_deref());
        if let Some(expected) = expected
            && !site.eq_ignore_ascii_case(expected)
        {
            self.finish_sequence_failure(format!(
                "radar {site} does not match playlist radar {expected}; file skipped"
            ));
            return;
        }
        let expected_position = self
            .file_sequence
            .as_ref()
            .and_then(|sequence| sequence.site_position);
        if let Some(expected_position) = expected_position
            && !same_playlist_position(position, expected_position)
        {
            self.finish_sequence_failure(
                "radar position differs from the first playlist frame; file skipped".to_owned(),
            );
            return;
        }

        if let Some(sequence) = self.file_sequence.as_mut() {
            if sequence.site_id.is_none() {
                sequence.site_id = Some(site);
                sequence.site_position = Some(position);
            }
            if loaded.iq.is_some() {
                sequence.level1_files += 1;
            }
            sequence.loaded += 1;
        }
        if let Some(reason) = loaded.assembly_refusal.take()
            && reason.should_report_for_playlist()
            && let Some(sequence) = self.file_sequence.as_mut()
        {
            let path = sequence
                .current_path()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("unknown file"));
            sequence.assembly_refusals.push((path, reason));
        }
        if let Some(evidence) = loaded.assembly.take() {
            self.accept_proven_sweep_member(loaded, evidence);
        } else {
            self.flush_pending_sweep_assembly();
            self.install_sequence_frame(loaded);
        }
        self.request_next_sequence_file();
    }

    /// Buffer or append one member whose identity came from the file itself.
    fn accept_proven_sweep_member(
        &mut self,
        loaded: LoadedVolume,
        evidence: nexrad_io::sweep_assembly::ProvenSweepMembership,
    ) {
        let pending = self
            .file_sequence
            .as_mut()
            .and_then(|sequence| sequence.pending_assembly.take());
        let Some(mut pending) = pending else {
            let first_source_label = loaded.source_label.clone();
            if let Some(sequence) = self.file_sequence.as_mut() {
                sequence.pending_assembly = Some(PendingSweepAssembly {
                    loaded,
                    evidence,
                    first_source_label,
                });
            }
            return;
        };

        let decision =
            nexrad_io::sweep_assembly::decide_adjacent_sweeps(&pending.evidence, &evidence);
        if decision == nexrad_io::sweep_assembly::SweepAssemblyDecision::ProvenSameVolume {
            let incoming =
                Arc::try_unwrap(loaded.volume).unwrap_or_else(|shared| shared.as_ref().clone());
            // The same decision is checked again inside the append function;
            // reaching an error here would mean the evidence changed between
            // two adjacent statements, which cannot happen.
            nexrad_io::sweep_assembly::append_proven_sweep(
                Arc::make_mut(&mut pending.loaded.volume),
                &mut pending.evidence,
                incoming,
                evidence,
            )
            .expect("a proven adjacent sweep remains proven while appending");
            pending.loaded.elapsed_ms += loaded.elapsed_ms;
            let count = pending.evidence.member_count;
            pending.loaded.source_label = format!(
                "{} (+{} sweep files, internal volume {:03})",
                pending.first_source_label,
                count - 1,
                pending.evidence.key.volume_sequence
            );
            Arc::make_mut(&mut pending.loaded.volume)
                .metadata
                .source_path = Some(pending.loaded.source_label.clone());
            if let Some(sequence) = self.file_sequence.as_mut() {
                sequence.pending_assembly = Some(pending);
            }
            return;
        }

        // A typed refusal is a frame boundary, not a load error. Install the
        // completed pending group and let this admitted sweep start its own.
        if let Some(sequence) = self.file_sequence.as_mut() {
            sequence.pending_assembly = Some(pending);
        }
        self.flush_pending_sweep_assembly();
        let first_source_label = loaded.source_label.clone();
        if let Some(sequence) = self.file_sequence.as_mut() {
            sequence.pending_assembly = Some(PendingSweepAssembly {
                loaded,
                evidence,
                first_source_label,
            });
        }
    }

    fn flush_pending_sweep_assembly(&mut self) {
        let pending = self
            .file_sequence
            .as_mut()
            .and_then(|sequence| sequence.pending_assembly.take());
        let Some(pending) = pending else {
            return;
        };
        if pending.evidence.member_count > 1
            && let Some(sequence) = self.file_sequence.as_mut()
        {
            sequence.assembled_files += pending.evidence.member_count;
            sequence.assembled_groups += 1;
        }
        self.install_sequence_frame(pending.loaded);
    }

    /// Install one playlist frame, then retire any raw pulse session it had.
    fn install_sequence_frame(&mut self, loaded: LoadedVolume) {
        // Let the ordinary install path inspect a Level 1 session long enough
        // to select PWR_REL for an uncalibrated cube. Raw pulses are session
        // state, not history state, and must not follow a scrub to another
        // file's estimated moments.
        let report = self.install_distinct_loaded_volume(loaded);
        if let (Some(sequence), Some(report)) = (self.file_sequence.as_mut(), report) {
            sequence.evicted_frames = sequence.evicted_frames.saturating_add(report.evicted.len());
        }
        self.iq = None;
    }

    fn update_sequence_status(&mut self, action: &str) {
        let Some(sequence) = self.file_sequence.as_ref() else {
            return;
        };
        let current = sequence.next.max(1).min(sequence.total());
        self.sequence_status = Some(format!(
            "Playlist {current}/{} · {action} · {} decoded · {} logical · {} retained · {} failed",
            sequence.total(),
            sequence.loaded,
            sequence.logical_volumes(),
            self.history.len(),
            sequence.failures.len()
        ));
        let current = sequence
            .current_path()
            .map(|path| format!(" Current file: {}.", path.display()))
            .unwrap_or_default();
        self.sequence_detail = Some(format!(
            "{} files selected in filename order. Files are independent frames unless matching internal Archive II volume identity proves they are one-cut members of one logical volume.{current} {} safe assembly boundary/boundaries so far. Successful frames are ordered by radar volume time; a different radar or radar position is skipped because this workspace has one map anchor.",
            sequence.total(),
            sequence.assembly_refusals.len()
        ));
    }

    fn finish_file_sequence(&mut self) {
        let Some(sequence) = self.file_sequence.take() else {
            return;
        };
        let failed = sequence.failures.len();
        let logical = sequence.logical_volumes();
        let retained = self.history.len();
        self.sequence_status = Some(format!(
            "Playlist complete · {} selected · {} decoded · {logical} logical volume(s) · {retained} retained · {failed} failed",
            sequence.total(),
            sequence.loaded,
        ));
        let mut detail = format!(
            "Loaded in filename order; the timeline orders retained frames by radar volume time. Preflight saw {} input and estimated {} decoded RAM. Internal Archive II evidence assembled {} file(s) into {} logical volume(s); all weaker or ambiguous cases stayed independent.",
            sequence.preflight.input_size_text(),
            crate::playlist_preflight::format_binary_bytes(
                sequence.preflight.estimated_decoded_bytes
            ),
            sequence.assembled_files,
            sequence.assembled_groups
        );
        if sequence.evicted_frames > 0 {
            detail.push_str(&format!(
                " Configured history limits explicitly evicted {} frame(s) during this playlist.",
                sequence.evicted_frames
            ));
        }
        if sequence.level1_files > 0 {
            detail.push_str(
                " Level 1 frames keep their estimated moments, but raw spectrum and reprocessing controls are disabled for a playlist so pulses from one file cannot be applied to another.",
            );
        }
        if !sequence.assembly_refusals.is_empty() {
            detail.push_str(" Kept separate: ");
            for (index, (path, reason)) in sequence.assembly_refusals.iter().take(5).enumerate() {
                if index > 0 {
                    detail.push_str("; ");
                }
                detail.push_str(&format!("{} ({})", short_path_label(path), reason.label()));
            }
            if sequence.assembly_refusals.len() > 5 {
                detail.push_str(&format!(
                    "; and {} more",
                    sequence.assembly_refusals.len() - 5
                ));
            }
        }
        if failed > 0 {
            detail.push_str(" Failed: ");
            for (index, (path, message)) in sequence.failures.iter().take(5).enumerate() {
                if index > 0 {
                    detail.push_str("; ");
                }
                detail.push_str(&format!("{} ({message})", short_path_label(path)));
            }
            if failed > 5 {
                detail.push_str(&format!("; and {} more", failed - 5));
            }
        }
        self.sequence_detail = Some(detail);
        self.iq = None;
        if self.history.is_empty() {
            self.clear_all_panes();
        }
    }

    /// Start a live session for `site`. The generation bump invalidates every
    /// in-flight local or previous-site result before the new session installs.
    fn start_live(&mut self, site: String) {
        let history_policy = self.live_history_policy();
        self.file_sequence = None;
        self.cancel_playlist_preflight();
        self.sequence_status = None;
        self.sequence_detail = None;
        // Unconditional, like the history clear below: a half-finished pair
        // must not keep its first endpoint - old-anchor coordinates - alive
        // into the new radar's data, and a finished measurement of the old
        // radar must stop reading as current. See `crate::vrot::mark_stale`.
        self.vrot_state
            .mark_stale(crate::vrot::StaleReason::DifferentSite);
        let generation = self.session_clock.bump();
        self.frame_clock.bump();
        self.history.clear();
        self.reset_live_follow_state();
        let evicted = self.history.set_policy(history_policy);
        debug_assert!(evicted.is_empty(), "an empty history cannot evict");
        self.load_ms = None;
        self.clear_all_panes();
        // The old site's feed report says nothing about the new one, and
        // leaving it up would carry a stall banner onto a healthy radar - or,
        // worse, clear one onto a dead radar. The new session's first poll
        // replaces this within about a second.
        self.live_feed = None;
        let label = site.trim().to_uppercase();
        match self
            .live_service
            .start(generation, site, self.live_cache_dir.clone())
        {
            Ok(()) => {
                let site = label;
                self.status = format!("Starting live {site}");
                self.live_status = "connecting".to_owned();
                self.live_site = Some(site);
            }
            Err(message) => {
                self.status = message;
                self.live_status.clear();
                self.live_site = None;
            }
        }
    }

    /// Stop the live session. The generation bump means a download that is
    /// already in flight cannot install after the user has stopped.
    fn stop_live(&mut self) {
        self.live_service.stop();
        self.session_clock.bump();
        self.live_site = None;
        self.live_status.clear();
        self.reset_live_follow_state();
        self.live_feed = None;
        self.status = "Live session stopped".to_owned();
    }

    fn poll_site_directory(&mut self) {
        let mut arrived = false;
        while let Some(sites) = self.sites_service.try_recv() {
            self.sites = sites;
            // Force a reprojection against the current anchor.
            self.placed_sites_projection = None;
            arrived = true;
        }
        if arrived {
            self.locate_time_series_frame();
        }
    }

    /// A site in the directory, by id.
    fn located_site(&self, site_id: &str) -> Option<&LocatedSite> {
        self.sites
            .iter()
            .find(|site| site.id.eq_ignore_ascii_case(site_id))
    }

    /// Where a Level 1 record's radar stood, or nothing.
    ///
    /// Two catalogs, asked in this order and only this order.
    ///
    /// 1. The station directory the application already keeps for its site
    ///    markers, which is the NWS OPERATIONAL feed. If a site is in there,
    ///    that is the position, full stop.
    /// 2. [`crate::research_sites`], a small sourced table of the research and
    ///    testbed radars whose time series are published and which the
    ///    operational feed therefore does not list. KOUN - NSSL's research
    ///    WSR-88D at Norman, and the radar the archived Level 1 records are
    ///    mostly from - is the reason it exists.
    ///
    /// The order is the point. The operational feed is fetched, cached and
    /// re-fetched; the supplementary table is frozen in the binary. Asking the
    /// frozen one second means a position that moves in the published feed
    /// moves here too, and a stale line in this repository can never quietly
    /// override it.
    ///
    /// Still `Option`, and the `None` still reaches the pane as POSITION
    /// UNKNOWN. A name in neither catalog gets no position at all rather than
    /// the nearest plausible one: the sweep's ranges and azimuths are real
    /// without a geography, and a fabricated antenna position under real
    /// weather is the failure this whole path is shaped around.
    ///
    /// Returns an owned site rather than a borrow because the two catalogs
    /// have different lifetimes - one is a `Vec` field, the other is
    /// `'static` - and because both callers immediately copy the fields out
    /// to build a `radar_core::RadarSite` anyway.
    fn time_series_site(&self, site_id: &str) -> Option<LocatedSite> {
        if let Some(published) = self.located_site(site_id) {
            return Some(published.clone());
        }
        crate::research_sites::research_site(site_id).map(|site| LocatedSite {
            id: site.id.to_owned(),
            name: Some(site.name.to_owned()),
            latitude_deg: site.latitude_deg,
            longitude_deg: site.longitude_deg,
        })
    }

    /// Give an already-installed Level 1 frame the position the directory has
    /// just supplied.
    ///
    /// The directory is fetched on its own thread and cached on disk, so on a
    /// cold machine it lands SECONDS after a file dropped at startup has
    /// already been decoded and installed. Without this the record keeps the
    /// position it was installed with - none - for the whole session, and the
    /// sweep never reaches the basemap however long the analyst waits.
    ///
    /// Only a frame that has no position is touched. A volume that stated its
    /// own coordinates is never overwritten by a directory entry: the file is
    /// the better authority about where its own radar was.
    ///
    /// A record whose site is in [`Self::time_series_site`]'s supplementary
    /// research table was already placed when it was installed and so never
    /// reaches the lookup below - the table is in the binary and does not
    /// arrive.
    fn locate_time_series_frame(&mut self) {
        let Some(session) = self.iq.as_ref() else {
            return;
        };
        let Some(frame) = self.history.current() else {
            return;
        };
        if frame.volume.site.latitude_deg.is_some() {
            return;
        }
        let Some(located) = self.time_series_site(session.site_id()) else {
            return;
        };
        let (name, latitude, longitude) = (
            located.name.clone(),
            located.latitude_deg,
            located.longitude_deg,
        );
        let mut volume = (*frame.volume).clone();
        volume.site.name = name;
        volume.site.latitude_deg = Some(latitude as f32);
        volume.site.longitude_deg = Some(longitude as f32);
        let (origin, stage, source_label) = (frame.origin, frame.stage, frame.source_label.clone());
        self.history.install(VolumeFrame::new(
            Arc::new(volume),
            origin,
            stage,
            source_label,
        ));
        // Anchoring moves the ground under every camera, so this goes through
        // the same door a loaded volume does rather than setting the anchor
        // here and leaving the cameras where they were.
        if self.map_scene.set_radar_anchor(latitude, longitude) {
            let changed = self.workspace.leave_overview(
                PLACEHOLDER_KM_PER_POINT,
                analyst_runtime::DEFAULT_KM_PER_POINT,
            );
            self.invalidate_view_panes(&changed);
            self.placed_sites_projection = None;
            self.frame_clock.bump();
            self.reset_all_panes();
        }
    }

    /// Project the site directory into world kilometres.
    ///
    /// Done once per anchor change rather than per frame: the positions are
    /// fixed relative to the projection, so the paint pass only has to apply
    /// the camera transform.
    fn refresh_placed_sites(&mut self) {
        // Markers off hands the panes an empty slice - the same shape the
        // warnings toggle uses - and clearing the projection stamp is what
        // makes turning them back on reproject immediately.
        if !self.settings_cache.site_markers {
            if !self.placed_sites.is_empty() {
                self.placed_sites = Vec::new().into();
                self.placed_sites_projection = None;
            }
            return;
        }
        let Some(projection) = self.map_scene.projection() else {
            return;
        };
        if self.placed_sites_projection == Some(projection.id()) {
            return;
        }
        self.placed_sites = self
            .sites
            .iter()
            .filter_map(|site| {
                let world =
                    projection.try_lon_lat_to_world(site.longitude_deg, site.latitude_deg)?;
                Some(PlacedSite {
                    id: site.id.clone(),
                    world,
                })
            })
            .collect::<Vec<_>>()
            .into();
        self.placed_sites_projection = Some(projection.id());
    }

    /// Hover text for the warnings chip.
    ///
    /// The chip's own number is every alert in force, and most of those are
    /// county-coded products that carry no polygon at all -- 442 active against
    /// 148 with geometry, measured on 2026-08-17. Saying how many are actually
    /// drawn stops the chip reading as a claim about the picture.
    fn poll_warnings(&mut self) {
        while let Some(update) = self.warnings_service.try_recv() {
            self.warnings_state = update.state;
            // A failed poll leaves the previous records alone: blanking the map
            // on one bad round trip would be a worse lie than a stale polygon,
            // and the chip already says the feed is offline.
            if let Some(records) = update.records {
                self.warnings = records;
                self.placed_hazards_at = None;
            }
        }
    }

    /// Project the warnings in force into world kilometres.
    ///
    /// Rebuilt when the anchor changes, when new records arrive, and on a slow
    /// timer so an expiry takes effect without waiting for the next poll.
    fn refresh_placed_hazards(&mut self) {
        if !self.show_warnings {
            if !self.placed_hazards.is_empty() {
                self.placed_hazards = Vec::new().into();
                self.placed_hazards_at = None;
            }
            return;
        }
        let Some(projection) = self.map_scene.projection() else {
            return;
        };
        let stale = self
            .placed_hazards_at
            .is_none_or(|at| at.elapsed() >= HAZARD_REPLACEMENT_INTERVAL);
        if !stale && self.placed_hazards_projection == Some(projection.id()) {
            return;
        }
        let now = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        self.placed_hazards = place_hazards(&self.warnings, &now, &projection).into();
        self.placed_hazards_projection = Some(projection.id());
        self.placed_hazards_at = Some(Instant::now());
    }

    fn poll_live_results(&mut self) {
        for _ in 0..MAX_LOAD_RESULTS_PER_FRAME {
            let Some(update) = self.live_service.try_recv() else {
                break;
            };
            match update {
                LiveUpdate::Started { generation, site } => {
                    if generation == self.session_clock.current() {
                        self.status = format!("Live {site}");
                        self.live_status = "waiting for volume".to_owned();
                    }
                }
                LiveUpdate::FeedStatus {
                    generation,
                    site,
                    newest_volume_time,
                    freshness,
                } => {
                    if generation != self.session_clock.current() {
                        continue;
                    }
                    // This is the ONLY writer of `live_feed`. `VolumeReady`
                    // deliberately does not write it: the once-per-session
                    // backfill delivers the volume BEFORE the live one, and
                    // letting that set the feed's newest time would make a
                    // healthy feed appear to jump backwards every time a
                    // session started.
                    self.live_feed = Some(LiveFeed {
                        site,
                        newest_volume_time,
                        freshness,
                    });
                }
                LiveUpdate::VolumeReady {
                    generation,
                    site,
                    path,
                    stage,
                    volume_time,
                    chunk_count,
                    total_size,
                    cache_hit,
                } => {
                    if generation != self.session_clock.current() {
                        continue;
                    }
                    self.live_status = format!(
                        "{} chunk(s) · {:.1} MiB · {}",
                        chunk_count,
                        total_size as f64 / (1_024.0 * 1_024.0),
                        if cache_hit { "cached" } else { "downloaded" }
                    );
                    let source_label =
                        format!("{site} {}", self.settings_cache.units.time(volume_time));
                    if let Err(request) = self.load_service.request(LoadRequest {
                        generation,
                        path,
                        origin: FrameOrigin::Live,
                        final_stage: stage,
                        source_label,
                        // A live volume is never a time series - Level 1 is
                        // archive material and there is no feed of it - so
                        // this is carried only so the request has one shape.
                        iq_controls: self.settings_cache.iq_controls,
                    }) {
                        self.status = format!("load worker is closed: {}", request.path.display());
                    }
                }
                LiveUpdate::Failed {
                    generation,
                    site,
                    message,
                } => {
                    if generation == self.session_clock.current() {
                        self.status = format!("{site}: {message}");
                        self.live_status = "error".to_owned();
                    }
                }
                LiveUpdate::Stopped => {
                    self.live_status.clear();
                    self.live_feed = None;
                }
            }
        }
    }

    fn poll_load_results(&mut self) {
        for _ in 0..MAX_LOAD_RESULTS_PER_FRAME {
            let Some(update) = self.load_service.try_recv() else {
                break;
            };
            match update {
                LoadUpdate::Started {
                    generation,
                    source_label,
                } => {
                    if generation == self.session_clock.current() {
                        if self.file_sequence.is_some() {
                            self.update_sequence_status("decoding");
                        }
                        self.status = format!("Decoding {source_label}");
                    }
                }
                LoadUpdate::Volume(loaded) => {
                    if loaded.generation != self.session_clock.current() {
                        continue;
                    }
                    if self.file_sequence.is_some() {
                        // A playlist waits for each file's terminal answer.
                        // Installing a progressive preview would leave a
                        // failed file as a half-frame and could fix the map
                        // anchor from a file that never decoded completely.
                        if loaded.stage == FrameStage::Complete {
                            self.finish_sequence_volume(loaded);
                        }
                    } else {
                        let _ = self.install_loaded_volume(loaded);
                    }
                }
                LoadUpdate::Failed {
                    generation,
                    source_label,
                    message,
                } => {
                    if generation == self.session_clock.current() {
                        if self.file_sequence.is_some() {
                            self.finish_sequence_failure(message);
                        } else {
                            self.handle_load_failure(&source_label, &message);
                        }
                    }
                }
            }
        }
    }

    /// One decode failed. Say so - and only when there is nothing on screen,
    /// blank the panes.
    ///
    /// One failed live-chunk round trip used to blank every pane even though
    /// the installed frame was intact and the picture rebuilt 100-300 ms
    /// later; the warnings poller sixty lines up already refuses that trade
    /// ("blanking the map on one bad round trip would be a worse lie").
    /// `LoadUpdate::Failed` carries no `FrameOrigin`, but `begin_load` clears
    /// history before its first request, so an empty history is exactly "the
    /// failure was the load the analyst is waiting on" and a populated one is
    /// exactly "a background decode failed under a frame that is still good".
    fn handle_load_failure(&mut self, source_label: &str, message: &str) {
        self.status = format!("{source_label}: {message}");
        if self.history.is_empty() {
            self.clear_all_panes();
        }
    }

    /// Give a Level 1-derived volume the sourced site position its pulse file
    /// does not contain. Shared by ordinary installs and playlist installs;
    /// the latter discards the raw pulse session after this lookup.
    fn locate_loaded_time_series(&self, loaded: &mut LoadedVolume) {
        let Some(session) = loaded.iq.as_ref() else {
            return;
        };
        let Some(located) = self.time_series_site(session.site_id()) else {
            return;
        };
        let mut volume = (*loaded.volume).clone();
        volume.site.name.clone_from(&located.name);
        volume.site.latitude_deg = Some(located.latitude_deg as f32);
        volume.site.longitude_deg = Some(located.longitude_deg as f32);
        loaded.volume = Arc::new(volume);
    }

    fn install_loaded_volume(&mut self, loaded: LoadedVolume) -> Option<InstallReport> {
        self.install_loaded_volume_with_mode(loaded, false)
    }

    /// A different selected source path is an independently admitted frame.
    /// Equal site/time metadata alone is not permission to discard it; only
    /// the proven sweep-assembly path combines local files. Live updates keep
    /// using `install_loaded_volume` so preview/partial/complete replacement
    /// semantics remain unchanged.
    fn install_distinct_loaded_volume(&mut self, loaded: LoadedVolume) -> Option<InstallReport> {
        self.install_loaded_volume_with_mode(loaded, true)
    }

    fn install_loaded_volume_with_mode(
        &mut self,
        mut loaded: LoadedVolume,
        retain_equal_identity: bool,
    ) -> Option<InstallReport> {
        if loaded.generation != self.session_clock.current() {
            return None;
        }
        // A NEXRAD Level 1 record brings its pulses with it. Give the volume
        // the site position the record could not: the RVP8 header states a processor
        // name and no coordinates, so without this the sweep is drawn in
        // radar-local kilometres over whatever the map was anchored on before.
        //
        // This is a lookup in two catalogs, not a guess: the operational
        // station directory the application already keeps for its site
        // markers, and - only when that has no answer - the sourced table of
        // research radars in `crate::research_sites`, which is where KOUN and
        // the rest of the archived time series live. See
        // `Self::time_series_site`. A record whose site is in NEITHER keeps no
        // position at all rather than borrowing one.
        //
        // Any frame that is not a time series clears the session, so the knobs
        // and the spectrum readout can never act on pulses that belong to a
        // file the analyst has moved on from.
        let relative_iq = loaded
            .iq
            .as_ref()
            .is_some_and(|session| !session.snr_available());
        self.locate_loaded_time_series(&mut loaded);
        self.iq = loaded.iq.take();
        // An uncalibrated I/Q cube intentionally carries no reflectivity grid.
        // A fresh workspace opens on REF, so leaving that selection untouched
        // would present a blank pane even though the source decoded correctly.
        // Replace only REF with the honest relative-power product and remember
        // that it was OUR fallback. On the next ordinary/calibrated source,
        // restore only panes that still show that fallback. A product picked
        // by the analyst clears the marker in `apply_product_selection`.
        for index in 0..analyst_runtime::MAX_PANES {
            let pane = PaneId::new(index as u8).expect("MAX_PANES yields valid pane ids");
            let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
            if relative_iq {
                if product == DisplayProduct::Reflectivity {
                    self.relative_power_fallback_from_ref[index] = true;
                    self.workspace.pane_mut(pane).product =
                        DisplayProduct::RelativePower.product_id();
                    self.pane_clocks[index].bump();
                }
            } else {
                let restore_ref = std::mem::take(&mut self.relative_power_fallback_from_ref[index]);
                if restore_ref && product == DisplayProduct::RelativePower {
                    self.workspace.pane_mut(pane).product =
                        DisplayProduct::Reflectivity.product_id();
                    self.pane_clocks[index].bump();
                }
            }
        }
        // Anchor the map at the radar this volume came from. Re-anchoring is a
        // no-op when the site is unchanged; a genuine site change moves the
        // ground out from under every camera, so each one is re-derived against
        // the new antenna instead of being left on kilometres that now name a
        // different place. See `WorkspaceState::apply_site_change`.
        let opening = self.map_scene.is_default_anchor();
        let previous_anchor = self.map_scene.projection();
        if let (Some(latitude), Some(longitude)) = (
            loaded.volume.site.latitude_deg,
            loaded.volume.site.longitude_deg,
        ) && self
            .map_scene
            .set_radar_anchor(f64::from(latitude), f64::from(longitude))
        {
            let new_anchor = self.map_scene.projection();
            let changed = if opening {
                // Nothing on screen is the analyst's unless they said so, and
                // `--zoom`/`--center` are stated in radar-local kilometres, so
                // the hand-over changes a scale and reprojects nothing.
                self.workspace.leave_overview(
                    PLACEHOLDER_KM_PER_POINT,
                    analyst_runtime::DEFAULT_KM_PER_POINT,
                )
            } else {
                let viewports: [Option<ViewportMetrics>; analyst_runtime::MAX_PANES] =
                    array::from_fn(|index| self.panes[index].viewport);
                // Snapshotted BEFORE the call, because `apply_site_change`
                // takes the workspace mutably and the closure below needs to
                // know each pane's scale to ask the globe how far this pane
                // has been carried.
                let scales: [f32; analyst_runtime::MAX_PANES] = array::from_fn(|index| {
                    PaneId::new(index as u8).map_or(analyst_runtime::DEFAULT_KM_PER_POINT, |pane| {
                        self.workspace.pane(pane).camera.sanitized().km_per_point
                    })
                });
                self.workspace.apply_site_change(
                    &viewports,
                    |world| {
                        let (lon, lat) = previous_anchor?.world_to_lon_lat(world);
                        new_anchor?.try_lon_lat_to_world(lon, lat)
                    },
                    |pane, world| {
                        let (Some(projection), Some(_viewport)) =
                            (new_anchor, viewports[pane.index()])
                        else {
                            return 0.0;
                        };
                        site_change_display_rotation(&projection, world, scales[pane.index()])
                    },
                )
            };
            self.invalidate_view_panes(&changed);
            if !opening {
                // The ground moved: a section line stated in the old radar's
                // kilometres now names a different place on earth.
                self.xsection.clear_line();
            }
        } else if opening && self.iq.is_some() {
            // A Level 1 record whose site is not in the station directory. The
            // RVP8 header carries no coordinates, so there is no anchor to set
            // and nothing to reproject - but the camera still has to leave the
            // continental overview, or an analyst who opened a file is shown a
            // hemisphere with a 125 km speck on it and no way to know the sweep
            // is there. The kilometres are radar-local, which is exactly what
            // this hand-over is documented to keep: see
            // `WorkspaceState::leave_overview`.
            let changed = self.workspace.leave_overview(
                PLACEHOLDER_KM_PER_POINT,
                analyst_runtime::DEFAULT_KM_PER_POINT,
            );
            self.invalidate_view_panes(&changed);
        }

        let before = self.current_frame_signature();
        let before_extent = self.current_frame_extent();
        let stage = loaded.stage;
        let frame = VolumeFrame::new(loaded.volume, loaded.origin, stage, loaded.source_label);
        let report = if retain_equal_identity {
            self.history.install_distinct(frame)
        } else {
            self.history.install(frame)
        };
        self.load_ms = Some(loaded.elapsed_ms);
        let after = self.current_frame_signature();
        let after_extent = self.current_frame_extent();

        if before != after {
            // A genuinely different frame. The clock bump makes every pane's
            // stamp stale, which queues the replacement render; the installed
            // texture is KEPT until that render lands, because dropping it
            // here blanked every pane to bare basemap on each live volume
            // hand-over and on every Preview->Partial->Complete promotion
            // (the signature includes `FrameStage`) - up to ~160 ms of blank
            // across four panes on the single render worker, several times a
            // minute. The grown-volume branch below has kept its texture for
            // exactly this reason all along.
            self.frame_clock.bump();
            self.reset_all_panes();
            // A measurement clicked on the previous volume is history the
            // moment this one installs - see `crate::vrot::mark_stale`.
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::NewVolume);
        } else if before_extent != after_extent {
            // The same frame, grown. Radials were appended under one site,
            // volume time and stage, so the signature above cannot see it and
            // without this the new data never reaches the screen at all.
            //
            // The clock is bumped but the panes are NOT cleared: the installed
            // texture still shows the part of the sweep that had already
            // arrived, and clearing it would blink the pane to empty on every
            // chunk. The texture is replaced when the new render lands.
            self.frame_clock.bump();
        }
        let eviction = if report.evicted.is_empty() {
            String::new()
        } else {
            format!(
                " · {} frame(s) evicted by configured history limit",
                report.evicted.len()
            )
        };
        self.status = match stage {
            FrameStage::Preview => format!(
                "Preview ready in {:.1} ms · {} frame(s){eviction}",
                loaded.elapsed_ms,
                self.history.len()
            ),
            FrameStage::Partial => format!(
                "Partial volume ready in {:.1} ms · {} frame(s){eviction}",
                loaded.elapsed_ms,
                self.history.len()
            ),
            FrameStage::Complete => format!(
                "Complete volume ready in {:.1} ms · {} frame(s) · {:?}{eviction}",
                loaded.elapsed_ms,
                self.history.len(),
                report.disposition
            ),
        };
        Some(report)
    }

    fn poll_render_results(&mut self, context: &egui::Context) {
        for _ in 0..MAX_RENDER_RESULTS_PER_FRAME {
            let Some(update) = self.render_service.try_recv() else {
                break;
            };
            match update {
                RenderUpdate::Completed(rendered) => self.install_render(context, *rendered),
                RenderUpdate::Failed {
                    pane,
                    stamp,
                    message,
                } => self.handle_render_failure(pane, stamp, message),
            }
        }
    }

    /// The worker could not draw this stamp. Record that, terminally.
    ///
    /// Persistent failures are reachable on real data - a velocity-only file
    /// asked for reflectivity (`SamplerError::NoReflectivityCuts`), a live
    /// volume caught before its first surveillance sweep, a derived field
    /// rejected as implausible - so "try again next frame" is a hot loop, not
    /// a recovery. The stamp is recorded and never resubmitted; any clock bump
    /// retries with a new stamp. See [`RenderTerminal`].
    fn handle_render_failure(&mut self, pane: PaneId, stamp: RenderStamp, message: String) {
        if stamp == self.current_stamp(pane) {
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.terminal = Some(RenderTerminal::Failed(stamp));
            runtime.status = message;
        }
    }

    fn install_render(&mut self, context: &egui::Context, rendered: RenderedPane) {
        let current = self.current_stamp(rendered.pane);
        let exact = rendered.stamp == current;
        // A render that differs from the current stamp ONLY in the view
        // generation is installed rather than discarded. Every pointer-move
        // frame bumps the view clock, so during a continuous camera gesture
        // every render the single worker completes is view-stale on arrival;
        // discarding them left the pane showing drag-start pixels for the
        // whole gesture while 100% of gesture-time render CPU was thrown
        // away. The discard was never necessary: `paint_transformed_texture`
        // reprojects the installed texture through `texture.camera`, so a
        // view-stale render draws geometrically correctly - fresher pixels
        // under the same, exact transform. Session, frame, pane, palette and
        // sweep mismatches are still discarded outright: those are different
        // data, not a different look at the same data.
        let view_stale_only = !exact
            && RenderStamp {
                view: current.view,
                ..rendered.stamp
            } == current;
        if !exact && !view_stale_only {
            return;
        }
        if view_stale_only
            && self.panes[rendered.pane.index()]
                .texture
                .as_ref()
                .is_some_and(|texture| texture.stamp == current)
        {
            // Never replace an exactly-current picture with an older view of
            // it. Unreachable in the serialized worker's arrival order, but
            // cheap to make impossible.
            return;
        }
        let image = color_image_from_rgba(rendered.width, rendered.height, &rendered.rgba);
        let runtime = &mut self.panes[rendered.pane.index()];
        let can_update = runtime.texture.as_ref().is_some_and(|texture| {
            texture.width == rendered.width && texture.height == rendered.height
        });
        if can_update {
            if let Some(texture) = &mut runtime.texture {
                texture.handle.set(image, egui::TextureOptions::NEAREST);
                texture.stamp = rendered.stamp;
                texture.camera = rendered.camera;
                texture.viewport = rendered.viewport;
                texture.width = rendered.width;
                texture.height = rendered.height;
            }
        } else {
            let handle = context.load_texture(
                format!(
                    "radar-pane-{}-{}-{}x{}",
                    rendered.pane.get(),
                    rendered.stamp.view.get(),
                    rendered.width,
                    rendered.height
                ),
                image,
                egui::TextureOptions::NEAREST,
            );
            runtime.texture = Some(InstalledTexture {
                handle,
                stamp: rendered.stamp,
                camera: rendered.camera,
                viewport: rendered.viewport,
                width: rendered.width,
                height: rendered.height,
            });
        }
        if exact {
            runtime.pending_stamp = None;
            runtime.terminal = None;
            runtime.status = format!("{:.1} ms", rendered.elapsed_ms);
            // A frame with gates removed says so on the pane header, every
            // time, in the words the filter itself uses - and with the counts
            // only the engine knows. Kept beside the timing rather than
            // formatted into it so `pane_header_status` can put it at the head
            // of the row, where a narrow pane cannot truncate it away. No
            // build of this application may draw a censored sweep and say
            // nothing at all, because the only other evidence an analyst would
            // have is the absence of the echo that was removed.
            runtime.filter_line = rendered.gate_filter.badge();
        }
        // A view-stale install leaves `pending_stamp` alone on purpose: the
        // exact-stamp render is still owed, and `ensure_render_requested`
        // keeps asking for it. `visible_panes_ready` compares stamps exactly,
        // so playback gating is unchanged by the stale pixels.
    }

    /// A drop can contain colour tables, genuine GR placefiles and radar
    /// volumes. Extensionless and text placefiles are recognised by their
    /// directives rather than being accidentally sent through radar decoding.
    ///
    /// One drop can carry several files, and a folder of palettes is exactly
    /// the kind of thing an analyst drags in one go, so every colour table in
    /// the drop is imported rather than only the first. Every plausible radar
    /// path becomes a sequential playlist of independent timeline frames;
    /// obvious decoys such as a screenshot dragged beside them are ignored.
    fn handle_dropped_files(&mut self, context: &egui::Context) {
        let dropped = context.input(|input| input.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let (tables, candidates) =
            crate::user_tables::split_drop(dropped.into_iter().filter_map(|file| file.path));
        if !tables.is_empty() && self.user_tables.import_all(&tables) {
            self.reresolve_palettes_from_user_tables();
        }
        let (placefiles, radar_candidates): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|path| dropped_path_is_placefile(path));
        let mut added_placefiles = 0_usize;
        for path in placefiles {
            if self.placefiles.add_path(&path) {
                added_placefiles += 1;
            }
        }
        if added_placefiles != 0 {
            self.set_placefiles_enabled(true);
            self.placefiles_window_open = true;
            self.status = format!("Added {added_placefiles} placefile overlay(s)");
        }
        let paths = crate::app_support::choose_dropped_radar_files(radar_candidates);
        if !paths.is_empty() {
            self.begin_load_sequence(paths);
        }
    }

    /// Re-read the colour table folder and re-resolve the installed palettes
    /// against it.
    ///
    /// Two callers: the settings window's *Rescan colour table folder* button
    /// (`SettingsOutcome::user_tables_rescan`), and a save made in the colour
    /// table editor, one frame later. Both are explicit instructions rather
    /// than guesses, so both get an actual read of every file rather than the
    /// listing short circuit.
    fn rescan_user_tables(&mut self) {
        self.user_tables.rescan();
        self.reresolve_palettes_from_user_tables();
    }

    /// Put the stored palette choices back through resolution now that the
    /// folder's contents have changed.
    ///
    /// Re-resolving rather than merely refreshing the offer lists is what
    /// makes a returning file bring its palette back with it: the stored
    /// choice survived the file's absence (see
    /// `settings_ui::palettes::capture_palettes_preserving`), so the moment
    /// the file is readable again the pane can be drawing it.
    fn reresolve_palettes_from_user_tables(&mut self) {
        let resolved = crate::settings_ui::palettes::apply_palettes_with_user(
            &self.settings_store.workspace().palettes,
            self.user_tables.library(),
        );
        self.source_field_palettes
            .reresolve(self.user_tables.library());
        if *self.color_tables == resolved {
            // A saved exact-field table may have appeared even when no shared
            // family moved. Its resolver has changed, so source panes still
            // need a fresh render.
            self.palette_clock.bump();
            self.invalidate_view_panes(self.workspace.visible_panes());
            return;
        }
        self.color_tables = Arc::new(resolved);
        self.palette_clock.bump();
        self.invalidate_view_panes(self.workspace.visible_panes());
    }

    fn advance_playback(&mut self, context: &egui::Context) {
        if self.history.playback() != PlaybackState::Playing || self.history.len() < 2 {
            return;
        }
        if !self.visible_panes_ready() {
            context.request_repaint_after(Duration::from_millis(16));
            return;
        }
        let frame_time = self.settings_cache.loop_frame_time;
        let elapsed = self.last_playback_step.elapsed();
        if elapsed < frame_time {
            context.request_repaint_after(frame_time - elapsed);
            return;
        }
        let before = self.current_frame_signature();
        self.history.advance_wrapping();
        self.last_playback_step = Instant::now();
        if self.current_frame_signature() != before {
            self.frame_clock.bump();
            // Keep the outgoing frame's texture until the next one renders:
            // dropping it here blanked every pane to bare basemap on every
            // 700 ms playback step. Pacing is unchanged - the held texture's
            // stamp is stale, so `visible_panes_ready` still says not-ready.
            self.reset_all_panes();
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::NewVolume);
        }
        context.request_repaint();
    }

    /// The toolbar, in whichever of the two styles Settings > Appearance
    /// picks. Both styles are supported and kept on purpose - the menu
    /// bar as the compact default, the v0.1.0 everything-visible row one
    /// setting away - so neither is a fossil the other is waiting to delete.
    ///
    /// `pub(crate)` so `examples/theme_gallery.rs` can photograph THIS
    /// function - the real bar, on real state, through the real egui → wgpu
    /// pipeline - rather than a mock of it that cannot go stale.
    pub(crate) fn toolbar(&mut self, ui: &mut egui::Ui) {
        match self.settings_cache.toolbar_style {
            ToolbarStyle::Menus => self.toolbar_menus(ui),
            ToolbarStyle::Everything => self.toolbar_everything(ui),
        }
    }

    /// One layer menu shared by both supported toolbars. Every toggle writes
    /// through the real settings store immediately, so switching toolbar style
    /// or restarting the application cannot invent a second layer state.
    fn layers_menu(&mut self, ui: &mut egui::Ui) {
        use crate::settings_ui::catalog::keys;

        ui.set_min_width(310.0);
        ui.label(egui::RichText::new("SURFACE OBSERVATIONS").small().strong());
        let enabled = self.settings_cache.observations_enabled;
        if ui
            .selectable_label(enabled, "Show METAR / ASOS / AWOS stations")
            .on_hover_text(
                "Draw measured temperature, dewpoint, wind barbs and station identifiers \
                 above every radar pane. Click or Shift-click a station for its history.",
            )
            .clicked()
        {
            self.set_surface_observations_enabled(!enabled);
        }

        let mesonet = self.settings_store.effective_bool(
            &self.settings_registry,
            keys::observations::CATEGORY,
            keys::observations::MESONET_ENABLED,
        );
        if ui
            .add_enabled(
                self.settings_cache.observations_enabled,
                egui::Button::new("Include supplemental mesonet stations").selected(mesonet),
            )
            .on_hover_text("Add actual reporting road-weather and environmental mesonet stations.")
            .clicked()
        {
            self.set_mesonet_observations_enabled(!mesonet);
        }

        let station_count = self.surface_observations.station_count();
        let status = self.surface_observations.status();
        if self.settings_cache.observations_enabled {
            ui.label(
                egui::RichText::new(format!("{station_count} reporting stations · {status}"))
                    .small()
                    .weak(),
            );
        }
        if ui
            .add_enabled(
                self.settings_cache.observations_enabled,
                egui::Button::new("Refresh observations now"),
            )
            .clicked()
        {
            self.surface_observations.refresh();
        }
        if let Some(station) = self
            .surface_observations
            .selected_station()
            .map(str::to_owned)
        {
            if ui
                .button(format!("Open {station} observation history…"))
                .clicked()
            {
                let frame_time = self.history.current().map(|frame| frame.volume.volume_time);
                self.surface_observations
                    .request_station_history_at(&station, frame_time);
                ui.close();
            }
        } else {
            ui.label(
                egui::RichText::new("Click or Shift-click any station to view its history")
                    .small()
                    .weak(),
            );
        }
        if ui.button("Observation plot settings…").clicked() {
            self.settings_ui.open_category(keys::observations::CATEGORY);
            ui.close();
        }

        ui.separator();
        ui.label(
            egui::RichText::new("GR / GR2ANALYST PLACEFILES")
                .small()
                .strong(),
        );
        let placefiles_enabled = self.settings_cache.placefiles_enabled;
        if ui
            .selectable_label(placefiles_enabled, "Show enabled placefile overlays")
            .on_hover_text(
                "Draw icons, labels, lines and polygons from enabled local or online placefiles.",
            )
            .clicked()
        {
            self.set_placefiles_enabled(!placefiles_enabled);
        }
        ui.label(
            egui::RichText::new(self.placefiles.status_summary())
                .small()
                .weak(),
        );
        if ui.button("Manage placefiles…").clicked() {
            self.placefiles_window_open = true;
            ui.close();
        }
        if ui
            .add_enabled(
                !self.placefiles.layers.is_empty(),
                egui::Button::new("Refresh all placefiles"),
            )
            .clicked()
        {
            self.placefiles.refresh_all();
        }
    }

    /// The menu bar: one compact row at any window width. Storm controls
    /// stay on it; the occasional ones live under File / View / Map / Layers / Tools.
    fn toolbar_menus(&mut self, ui: &mut egui::Ui) {
        use crate::theme::bevel;

        // Before the bar is built, not inside the File menu's closure: the
        // summary reads the profile library and the settings store together,
        // and computing it here keeps the closure borrowing one field.
        let profile_line = self.active_profile_line();
        let active = self.workspace.active_pane;
        let current_product_id = self.workspace.active().product.clone();
        let current_source_field =
            crate::source_fields::producer_name_from_product_id(&current_product_id)
                .map(str::to_owned);
        let current_product = DisplayProduct::from_product_id(&current_product_id);
        let current_source_palette = current_source_field.as_deref().and_then(|producer_name| {
            self.source_palette_for_pane(active, &current_product_id, producer_name)
        });
        let mut source_palette_action = None;
        let mut requested_load = None;
        let mut open_browser = false;
        let mut export_current_view = false;
        let mut export_loop = false;
        let mut open_online_data = false;
        let mut live_action = None;
        let mut selected_layout = self.workspace.layout;
        let mut selected_product = current_product;
        let mut selected_source_field = None;
        let mut quality_changed = false;
        let mut palette_changed = false;
        let mut filter_changed = false;
        let mut tilt_delta = 0_isize;
        let follow_policy = self.live_follow_policy();
        let mut toggle_live_follow = false;
        let mut open_live_follow_settings = false;
        let visible = self.workspace.visible_panes();
        let cameras_linked = visible
            .iter()
            .all(|pane| self.workspace.pane(*pane).links.camera == Some(0));
        let mut toggle_camera_links = false;
        let mut toggle_warnings = false;

        // A menu bar, not a control wall. The bar carries only what an
        // analyst touches mid-storm - product, palette, tilt, site - and the
        // occasional controls live under File / View / Map / Layers / Tools, so the
        // bar is one row at any window width instead of wrapping into a
        // block that eats the screen.
        //
        // Presentation is the theme's, not egui's: a face-coloured band with
        // the chunky raised edge every Win95 toolbar had (`raised_frame`),
        // titles and commands flat until the pointer arrives
        // (`toolbar_menu` / `toolbar_button`, the Office 97 refinement),
        // latching controls sunken and tinted so their state survives a
        // screenshot (`toolbar_toggle`), readouts inset into the chrome
        // (`sunken_readout`), and groove separators rather than hairlines
        // between the groups (`etched_separator`). Nothing on this bar draws
        // a bare label on whatever ground it lands on, which is what made the
        // product name, the tilt and the live status invisible before.
        bevel::raised_frame(ui, |ui| {
            // Full width: a band that stops where its buttons stop is not a
            // band, it is a floating box.
            ui.set_min_width(ui.available_width());
            // A menu bar's own density. The theme's 6-point item gap is right
            // for a form and too airy for a row of menu titles, which in every
            // menu bar since 1995 nearly touch; the grooves do the separating.
            // Hit targets are untouched - the >= 24 points a finger lands on
            // is padding INSIDE each control, not the gap between them.
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.horizontal_wrapped(|ui| {
                bevel::toolbar_menu(ui, "File", |ui| {
                    ui.set_min_width(300.0);
                    ui.label("Open a radar volume");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.source_path_text)
                            .desired_width(260.0)
                            .hint_text("Radar volume file path"),
                    )
                    .on_hover_text(OPEN_PATH_HINT);
                    ui.horizontal(|ui| {
                        if ui.button("Load file").clicked()
                            && !self.source_path_text.trim().is_empty()
                        {
                            requested_load = Some(PathBuf::from(self.source_path_text.trim()));
                            ui.close();
                        }
                        // Beside the box rather than instead of it. Typing a
                        // path stays the fastest way in for somebody who
                        // knows it; browsing is for the folder nobody has
                        // memorised.
                        if ui.button("Browse…").clicked() {
                            open_browser = true;
                            ui.close();
                        }
                    });
                    if ui
                        .add_enabled(
                            !self.current_view_export.in_flight() && self.loop_export.is_none(),
                            egui::Button::new("Export current view"),
                        )
                        .on_hover_text(
                            "Write the rendered application window — panes, legends, chrome and timeline — directly to Downloads as a non-overwriting PNG.",
                        )
                        .clicked()
                    {
                        export_current_view = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.history.is_empty()
                                && !self.current_view_export.in_flight()
                                && self.loop_export.is_none(),
                            egui::Button::new("Export loop…"),
                        )
                        .on_hover_text(
                            "Save every loaded radar frame, including the current map, legends and overlays, as a high-quality animated GIF directly to Downloads.",
                        )
                        .clicked()
                    {
                        export_loop = true;
                        ui.close();
                    }
                    if ui.button("Download Level I data…").clicked() {
                        open_online_data = true;
                        ui.close();
                    }
                    bevel::etched_separator(ui);
                    // The one place a running application names the active
                    // profile: unobtrusive - a menu nobody has to open - and
                    // findable, right above the window it is managed in.
                    if let Some(line) = &profile_line {
                        ui.label(egui::RichText::new(line).small().weak());
                    }
                    if ui.button("Profiles…").clicked() {
                        self.settings_ui
                            .open_category(crate::settings_ui::catalog::keys::profiles::CATEGORY);
                        ui.close();
                    }
                    if ui.button("Settings…").clicked() {
                        self.settings_ui.open = true;
                        ui.close();
                    }
                });
                bevel::toolbar_menu(ui, "View", |ui| {
                    ui.set_min_width(200.0);
                    ui.label("Layout");
                    for layout in [
                        PaneLayout::One,
                        PaneLayout::TwoVertical,
                        PaneLayout::TwoHorizontal,
                        PaneLayout::Four,
                    ] {
                        if ui
                            .selectable_label(selected_layout == layout, layout_label(layout))
                            .clicked()
                        {
                            selected_layout = layout;
                            ui.close();
                        }
                    }
                    bevel::etched_separator(ui);
                    ui.label("Display quality");
                    for (label, preset) in render2d::DisplayQuality::PRESETS {
                        if ui.selectable_label(self.quality == preset, label).clicked()
                            && self.quality != preset
                        {
                            self.quality = preset;
                            quality_changed = true;
                            ui.close();
                        }
                    }
                    bevel::etched_separator(ui);
                    if ui
                        .selectable_label(cameras_linked, "Link cameras")
                        .clicked()
                    {
                        toggle_camera_links = true;
                    }
                });
                bevel::toolbar_menu(ui, "Map", |ui| {
                    crate::app_support::basemap_menu(
                        ui,
                        &mut self.map_scene,
                        &mut self.settings_store,
                    );
                });
                bevel::toolbar_menu(ui, "Layers", |ui| {
                    self.layers_menu(ui);
                });
                bevel::toolbar_menu(ui, "Tools", |ui| {
                    ui.set_min_width(220.0);
                    if ui
                        .selectable_label(self.vol3d.open, "3D volume explorer")
                        .clicked()
                    {
                        self.vol3d.open = !self.vol3d.open;
                        ui.close();
                    }
                    if ui
                        .selectable_label(
                            self.xsection.armed || self.xsection.open,
                            "Cross-section",
                        )
                        .on_hover_text(
                            "Arm, then click two points on a radar pane. A separate window \
                         shows the vertical slice of the current product along that \
                         line; drag the A/B handles to adjust.",
                        )
                        .clicked()
                    {
                        self.xsection.toggle_armed();
                        if self.xsection.armed {
                            // One armed click-mode at a time: a click cannot be both
                            // a Vrot gate and a section endpoint.
                            self.vrot_active = false;
                            self.vrot_state.clear();
                            self.vrot_pane = None;
                        }
                        ui.close();
                    }
                    if ui
                        .selectable_label(self.vrot_active, "Vrot sampling")
                        .on_hover_text(
                            "Click two gates across a velocity couplet. Needs a dealiased \
                         product: measuring folded velocity gives a number wrong by a \
                         multiple of the Nyquist that still looks reasonable.",
                        )
                        .clicked()
                    {
                        self.vrot_active = !self.vrot_active;
                        if self.vrot_active {
                            self.xsection.armed = false;
                        }
                        if !self.vrot_active {
                            self.vrot_state.clear();
                            self.vrot_pane = None;
                        }
                        ui.close();
                    }
                    if (self.vrot_state.measurement().is_some()
                        || self.vrot_state.pending().is_some())
                        && ui.button("Clear Vrot").clicked()
                    {
                        self.vrot_state.clear();
                        self.vrot_pane = None;
                        ui.close();
                    }
                });

                bevel::etched_separator(ui);
                // A latching toggle rather than a plain button: while the picker
                // is down the control stays sunken and tinted, so a screenshot of
                // the bar still says where the popup came from.
                //
                // `⏷` and not `▾`: the fonts egui bundles carry U+23F7 - it is
                // what `egui::containers::menu::SubMenuButton` points its own
                // arrow with - and do NOT carry U+25BE, which renders as a
                // tofu box. Caught by looking at the photograph.
                let picker_button = bevel::toolbar_toggle(
                    ui,
                    self.product_picker_open,
                    format!(
                        "{} ⏷",
                        current_source_field
                            .as_deref()
                            .unwrap_or_else(|| current_product.label())
                    ),
                )
                .on_hover_text("Choose a product and its colour table");
                let mut opened_this_frame = false;
                if picker_button.clicked() {
                    self.product_picker_open = !self.product_picker_open;
                    if self.product_picker_open {
                        self.product_picker.opened(current_product);
                        opened_this_frame = true;
                    }
                }
                if self.product_picker_open {
                    // Only while open: the picker takes the arrow keys, Enter and
                    // Escape off the global event queue every frame it runs, so
                    // drawing it unconditionally would eat them from the toolbar.
                    let outcome = egui::Area::new(egui::Id::new("workstation-product-picker"))
                        .order(egui::Order::Foreground)
                        // Clear of the band, not just of the button: the
                        // bar's frame carries 6 points of margin and a
                        // 2-point bevel below this control, and a popup that
                        // starts 4 points down slices through both.
                        .fixed_pos(picker_button.rect.left_bottom() + egui::vec2(0.0, 10.0))
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                draw_product_picker(
                                    ui,
                                    ProductPickerInput {
                                        state: &mut self.product_picker,
                                        current: current_product,
                                        current_source_field: current_source_field.as_deref(),
                                        availability: &self.product_availability,
                                        source_fields: &self.source_fields,
                                        tables: &self.color_tables,
                                        user_tables: Some(self.user_tables.library()),
                                        show_experimental: false,
                                    },
                                )
                            })
                        });
                    let popup_rect = outcome.response.rect;
                    let outcome = outcome.inner.inner;
                    if let Some(product) = outcome.product {
                        selected_product = product;
                        self.product_picker_open = false;
                    }
                    if let Some(producer_name) = outcome.source_field {
                        selected_source_field = Some(producer_name);
                        self.product_picker_open = false;
                    }
                    if let Some(selection) = outcome.palette {
                        // Family-wide on purpose: installing a velocity table moves
                        // VEL, DVEL, SRV and DSRV together, because they are the
                        // same measurement drawn four ways.
                        Arc::make_mut(&mut self.color_tables)
                            .set_family(selection.family, selection.table);
                        self.palette_clock.bump();
                        palette_changed = true;
                    }
                    if let Some(request) = outcome.edit_palette {
                        // The picker already knows whether this is a shipped
                        // preset, and says so on the request. Passed straight
                        // through: the editor must not re-derive it, because
                        // the only thing it could re-derive it from is a
                        // filename, and filenames are many-to-one.
                        self.palette_editor.edit_or_duplicate(
                            request.family,
                            &request.table,
                            request.duplicate,
                        );
                        self.product_picker_open = false;
                    }
                    // `crate::popup` rather than `clicked_elsewhere()`. That method
                    // answered yes for the click that OPENED this popup - the click
                    // was on the button, which is outside the popup - so the popup
                    // opened and closed inside one frame and the product button was
                    // dead. The rule now knows about that click.
                    let dismissal = crate::popup::dismissal_from_input(
                        ui.ctx(),
                        popup_rect,
                        picker_button.rect,
                        opened_this_frame,
                        outcome.dismissed,
                    );
                    if dismissal.should_close() {
                        self.product_picker_open = false;
                    }
                }

                // The colour table stays on the bar. It is the control that tells
                // an analyst whether a strange-looking field is the data or the
                // palette, so burying it one level down inside a menu was wrong.
                let palette_family = crate::product_picker::palette_family(current_product);
                if let (Some(producer_name), Some(resolved)) =
                    (current_source_field.as_deref(), current_source_palette.as_ref())
                {
                    source_palette_action = source_palette_control(ui, producer_name, resolved);
                } else if current_source_field.is_none()
                    && let Some(family) = palette_family
                {
                    let installed = self.color_tables.for_family(family).clone();
                    // Taken out of the popup rather than installed inside it:
                    // the rows are borrowed from the offer cache for the
                    // length of the loop, and installing writes the set that
                    // cache is keyed on.
                    let mut picked = None;
                    egui::ComboBox::from_id_salt("workstation-palette")
                        .selected_text(installed.name())
                        .width(210.0)
                        .show_ui(ui, |ui| {
                            for table in self.palette_offers.offers(
                                family,
                                &installed,
                                Some(self.user_tables.library()),
                            ) {
                                let chosen = table.name() == installed.name();
                                if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                                    picked = Some(table.clone());
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "Colour table for this product's family. The last row is the \
                         selected palette redrawn the other way: smooth or stepped.",
                        );
                    if let Some(table) = picked {
                        Arc::make_mut(&mut self.color_tables).set_family(family, table);
                        self.palette_clock.bump();
                        palette_changed = true;
                    }
                }

                // The gate filter, beside the product and the palette because
                // it belongs to the same question those two answer: what am I
                // actually being shown. It is on the bar and not filed under a
                // menu for the reason `crate::gate_filter_ui` documents - a
                // control that hides weather has to be visible while it is on,
                // and the chip latches while it is.
                filter_changed |= crate::gate_filter_ui::draw_gate_filter_control(
                    ui,
                    crate::gate_filter_ui::GateFilterControl {
                        state: &mut self.gate_filter_ui,
                        registry: &self.settings_registry,
                        store: &mut self.settings_store,
                    },
                );

                // A stepper: two keys with the measurement inset between them.
                // The well holds a floor width so the whole right-hand half of
                // the bar does not shuffle sideways when 0.48° becomes 19.51°.
                if bevel::toolbar_button(ui, "− Tilt").clicked() {
                    tilt_delta = -1;
                }
                bevel::sunken_readout(ui, 74.0, 150.0, self.active_tilt_label())
                    .on_hover_text(self.active_tilt_hover());
                if bevel::toolbar_button(ui, "+ Tilt").clicked() {
                    tilt_delta = 1;
                }
                let follow_label = if follow_policy.enabled {
                    format!("Auto ≤{:.1}°", follow_policy.max_elevation_deg)
                } else {
                    "Auto tilt".to_owned()
                };
                let follow_response = bevel::toolbar_toggle(
                    ui,
                    follow_policy.enabled,
                    follow_label,
                )
                .on_hover_text(format!(
                    "Follow arriving live sweeps at or below {:.1}° with the \
                     real radial sweep animation; minimum scan-time gap {} s. \
                     Click to toggle; right-click \
                     to adjust the elevation, update interval, or feed polling.",
                    follow_policy.max_elevation_deg,
                    follow_policy.min_interval.as_secs(),
                ));
                if follow_response.clicked() {
                    toggle_live_follow = true;
                }
                if follow_response.secondary_clicked() {
                    open_live_follow_settings = true;
                }
                // Immediately after the stepper, because these describe the
                // sweep the stepper chose. Readout wells rather than bare
                // labels, so they keep the bar's grammar and stay legible on
                // whatever ground the theme paints.
                let (snr_readout, resolution_notice) = self.active_censoring_readouts();
                if let Some(readout) = &snr_readout {
                    bevel::sunken_readout(ui, 0.0, SNR_READOUT_WIDTH, readout.as_str())
                        .on_hover_text(SNR_THRESHOLD_HINT);
                }
                if let Some(notice) = &resolution_notice {
                    bevel::sunken_readout(ui, 0.0, RESOLUTION_NOTICE_WIDTH, notice.as_str())
                        .on_hover_text(RESOLUTION_REDUCED_HINT);
                }

                bevel::etched_separator(ui);
                // Sized, not `desired_width`: a text edit laid out from its font
                // alone is shorter than the 24-point floor the rest of the bar
                // keeps, and a row of controls that disagree about their height
                // is the thing that reads as amateur.
                ui.add_sized(
                    [64.0, bevel::MIN_TOUCH_POINTS],
                    egui::TextEdit::singleline(&mut self.site_text)
                        .char_limit(4)
                        .hint_text("KRTX"),
                );
                if self.live_site.is_some() {
                    if bevel::toolbar_button(ui, "Stop live").clicked() {
                        live_action = Some(LiveAction::Stop);
                    }
                } else if bevel::toolbar_button(ui, "Start live").clicked()
                    && !self.site_text.trim().is_empty()
                {
                    live_action = Some(LiveAction::Start(self.site_text.trim().to_owned()));
                }
                if !self.live_status.is_empty() {
                    // In a well, like every other readout: this line was drawn on
                    // the bare window before, which is exactly where dark ink on
                    // a dark ground disappeared.
                    bevel::sunken_readout(ui, 0.0, 340.0, self.live_status.as_str())
                        .on_hover_text(self.live_status.as_str());
                }
                // The loud one, and the whole reason this row was revisited. On
                // 2026-08-19 the readout above said "82 chunk(s) · 14.3 MiB ·
                // downloaded" over a KUEX volume from the previous Saturday, and
                // every word of it was true - which is how an analyst reads "no
                // storms" off a screen that means "no data". This sits beside it,
                // in the theme's error ink, and contradicts it.
                //
                // A readout well rather than a bare label, so it keeps the bar's
                // grammar; the explicit colour overrides `sunken_readout`'s
                // fallback ink rather than fighting it, and the hover carries the
                // exact Z time the feed stopped at.
                if let Some(notice) = self.live_stall_notice(Utc::now()) {
                    bevel::sunken_readout(
                        ui,
                        0.0,
                        360.0,
                        egui::RichText::new(notice)
                            .color(ui.visuals().error_fg_color)
                            .strong(),
                    )
                    .on_hover_text(self.live_stall_hover());
                }

                bevel::etched_separator(ui);
                // Its own chip, so an analyst can tell "no warnings out" from "we
                // are not receiving warnings".
                let chip = match self.warnings_state.active() {
                    Some(active) => format!("{} · {active}", self.warnings_state.label()),
                    None => self.warnings_state.label().to_owned(),
                };
                let response = bevel::toolbar_toggle(ui, self.show_warnings, chip).on_hover_text(
                    crate::app_support::warnings_hover(
                        &self.warnings_state.detail(),
                        self.show_warnings,
                        self.placed_hazards.len(),
                    ),
                );
                if response.clicked() {
                    toggle_warnings = true;
                }

                // The Vrot readout is a measurement, not a control: it stays on
                // the bar whenever one exists, stale reason and all - there is no
                // hover on glass.
                if let Some(measurement) = self.vrot_state.measurement() {
                    let readout = match self.vrot_state.stale_reason() {
                        Some(reason) => format!(
                            "{} · STALE: {}",
                            self.vrot_readout(measurement),
                            reason.label()
                        ),
                        None => self.vrot_readout(measurement),
                    };
                    bevel::sunken_readout(ui, 0.0, 320.0, readout.as_str())
                        .on_hover_text(readout.as_str());
                }
            });
        });

        if let Some(action) = source_palette_action {
            match action {
                SourcePaletteAction::Edit => {
                    if let Some(resolved) = current_source_palette.as_ref() {
                        self.palette_editor.edit_source_field(
                            current_product_id.clone(),
                            &resolved.table,
                            resolved.automatic,
                            resolved.current_is_durable,
                        );
                    }
                }
                SourcePaletteAction::Reset => {
                    if self.source_field_palettes.reset(&current_product_id) {
                        self.palette_clock.bump();
                        palette_changed = true;
                        self.status = format!(
                            "{} palette reset to automatic observed range",
                            current_source_field.as_deref().unwrap_or("source field")
                        );
                    }
                }
            }
        }

        if filter_changed {
            // The chip wrote straight to the store, so the cache the paint
            // path reads is one frame stale until this runs. Before the
            // invalidation below, so the re-render it asks for is requested
            // under the new filter rather than the old one.
            self.recompute_settings_cache();
            self.note_filter_cleared();
        }
        if quality_changed || palette_changed || filter_changed {
            // Same data, different picture: every pane's view generation moves,
            // which discards the in-flight render and asks for a new one
            // without throwing away the texture that is currently on screen.
            self.invalidate_view_panes(self.workspace.visible_panes());
        }

        if open_browser {
            self.show_file_browser();
        }
        if export_current_view {
            self.request_current_view_export(ui.ctx());
        }
        if export_loop {
            self.begin_loop_export(ui.ctx());
        }
        if open_online_data {
            self.online_data.open();
        }
        if let Some(path) = requested_load {
            self.begin_load(path);
        }
        match live_action {
            Some(LiveAction::Start(site)) => self.start_live(site),
            Some(LiveAction::Stop) => self.stop_live(),
            None => {}
        }
        if selected_layout != self.workspace.layout {
            self.workspace.set_layout(selected_layout);
        }
        if let Some(producer_name) = selected_source_field {
            self.apply_source_field_selection(active, &producer_name);
        } else if selected_product != current_product {
            self.apply_product_selection(active, selected_product);
        }
        if tilt_delta != 0 {
            self.change_active_tilt(tilt_delta);
        }
        if toggle_live_follow {
            self.set_live_follow_enabled(!follow_policy.enabled);
        }
        if open_live_follow_settings {
            self.settings_ui
                .open_category(crate::settings_ui::catalog::keys::data::CATEGORY);
        }
        if toggle_warnings {
            self.show_warnings = !self.show_warnings;
            // Force placement now rather than at the next cadence, so the map
            // answers the click on this frame.
            self.placed_hazards_at = None;
            self.placed_hazards_projection = None;
            self.refresh_placed_hazards();
            if self.show_warnings {
                self.warnings_service.refresh();
            }
        }
        if toggle_camera_links {
            let new_group = (!cameras_linked).then_some(0);
            for pane in self.workspace.visible_panes() {
                self.workspace.pane_mut(*pane).links.camera = new_group;
            }
        }
    }

    /// The everything-visible row, exactly as v0.1.0 shipped it: every
    /// control on the bar at once, wrapping on narrower windows. Selected
    /// by Settings > Appearance > Toolbar style = Everything visible.
    fn toolbar_everything(&mut self, ui: &mut egui::Ui) {
        let active = self.workspace.active_pane;
        let current_product_id = self.workspace.active().product.clone();
        let current_source_field =
            crate::source_fields::producer_name_from_product_id(&current_product_id)
                .map(str::to_owned);
        let current_product = DisplayProduct::from_product_id(&current_product_id);
        let current_source_palette = current_source_field.as_deref().and_then(|producer_name| {
            self.source_palette_for_pane(active, &current_product_id, producer_name)
        });
        let mut source_palette_action = None;
        let mut requested_load = None;
        let mut open_browser = false;
        let mut export_current_view = false;
        let mut export_loop = false;
        let mut open_online_data = false;
        let mut live_action = None;
        let mut selected_layout = self.workspace.layout;
        let mut selected_product = current_product;
        let mut selected_source_field = None;
        let mut quality_changed = false;
        let mut palette_changed = false;
        let mut filter_changed = false;
        let mut tilt_delta = 0_isize;
        let follow_policy = self.live_follow_policy();
        let mut toggle_live_follow = false;
        let mut open_live_follow_settings = false;
        let visible = self.workspace.visible_panes();
        let cameras_linked = visible
            .iter()
            .all(|pane| self.workspace.pane(*pane).links.camera == Some(0));
        let mut toggle_camera_links = false;
        let mut toggle_warnings = false;

        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.source_path_text)
                    .desired_width(260.0)
                    .hint_text("Radar volume file path"),
            )
            .on_hover_text(OPEN_PATH_HINT);
            if ui.button("Load").clicked() && !self.source_path_text.trim().is_empty() {
                requested_load = Some(PathBuf::from(self.source_path_text.trim()));
            }
            if ui
                .button("Browse…")
                .on_hover_text(
                    "Look through a folder. Every file is read rather than judged by its name, \
                     so a volume stored without an extension still says what it is.",
                )
                .clicked()
            {
                open_browser = true;
            }
            if ui
                .add_enabled(
                    !self.current_view_export.in_flight() && self.loop_export.is_none(),
                    egui::Button::new("Export current view"),
                )
                .on_hover_text(
                    "Write the rendered application window directly to Downloads as a non-overwriting PNG.",
                )
                .clicked()
            {
                export_current_view = true;
            }
            if ui
                .add_enabled(
                    !self.history.is_empty()
                        && !self.current_view_export.in_flight()
                        && self.loop_export.is_none(),
                    egui::Button::new("Export loop…"),
                )
                .on_hover_text(
                    "Save every loaded radar frame and its visible overlays as an animated GIF directly to Downloads.",
                )
                .clicked()
            {
                export_loop = true;
            }
            if ui
                .button("Online Level I…")
                .on_hover_text("Browse and download public NOAA/NSSL KOUN time-series records")
                .clicked()
            {
                open_online_data = true;
            }

            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.site_text)
                    .desired_width(56.0)
                    .char_limit(4)
                    .hint_text("KRTX"),
            );
            if self.live_site.is_some() {
                if ui.button("Stop live").clicked() {
                    live_action = Some(LiveAction::Stop);
                }
            } else if ui.button("Start live").clicked() && !self.site_text.trim().is_empty() {
                live_action = Some(LiveAction::Start(self.site_text.trim().to_owned()));
            }
            if !self.live_status.is_empty() {
                ui.label(&self.live_status);
            }
            // Beside the line it contradicts, and never instead of it. On
            // 2026-08-19 the status above said "82 chunk(s) · 14.3 MiB ·
            // downloaded" over a KUEX volume from the previous Saturday, and
            // every word of it was true - which is how an analyst reads "no
            // storms" off a screen that means "no data".
            //
            // A readout well rather than a bare label, because this one has to
            // be found without being looked for: the inset ground separates it
            // from the status line it sits next to, and the theme's error ink
            // (which overrides `sunken_readout`'s fallback rather than fighting
            // it) says which of the two to believe. The hover carries the exact
            // Z time the feed stopped at.
            if let Some(notice) = self.live_stall_notice(Utc::now()) {
                crate::theme::bevel::sunken_readout(
                    ui,
                    0.0,
                    360.0,
                    egui::RichText::new(notice)
                        .color(ui.visuals().error_fg_color)
                        .strong(),
                )
                .on_hover_text(self.live_stall_hover());
            }

            ui.separator();
            // Its own chip, so an analyst can tell "no warnings out" from "we
            // are not receiving warnings".
            let chip = match self.warnings_state.active() {
                Some(active) => format!("{} · {active}", self.warnings_state.label()),
                None => self.warnings_state.label().to_owned(),
            };
            let response = ui.selectable_label(self.show_warnings, chip).on_hover_text(
                crate::app_support::warnings_hover(
                    &self.warnings_state.detail(),
                    self.show_warnings,
                    self.placed_hazards.len(),
                ),
            );
            if response.clicked() {
                toggle_warnings = true;
            }

            ui.separator();
            egui::ComboBox::from_id_salt("workstation-layout")
                .selected_text(layout_label(selected_layout))
                .width(112.0)
                .show_ui(ui, |ui| {
                    for layout in [
                        PaneLayout::One,
                        PaneLayout::TwoVertical,
                        PaneLayout::TwoHorizontal,
                        PaneLayout::Four,
                    ] {
                        ui.selectable_value(&mut selected_layout, layout, layout_label(layout));
                    }
                });

            let picker_button = ui
                .selectable_label(
                    self.product_picker_open,
                    current_source_field
                        .as_deref()
                        .unwrap_or_else(|| current_product.label()),
                )
                .on_hover_text("Choose a product and its colour table");
            let mut opened_this_frame = false;
            if picker_button.clicked() {
                self.product_picker_open = !self.product_picker_open;
                if self.product_picker_open {
                    self.product_picker.opened(current_product);
                    opened_this_frame = true;
                }
            }
            if self.product_picker_open {
                // Only while open: the picker takes the arrow keys, Enter and
                // Escape off the global event queue every frame it runs, so
                // drawing it unconditionally would eat them from the toolbar.
                let outcome = egui::Area::new(egui::Id::new("workstation-product-picker"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(picker_button.rect.left_bottom() + egui::vec2(0.0, 4.0))
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            draw_product_picker(
                                ui,
                                ProductPickerInput {
                                    state: &mut self.product_picker,
                                    current: current_product,
                                    current_source_field: current_source_field.as_deref(),
                                    availability: &self.product_availability,
                                    source_fields: &self.source_fields,
                                    tables: &self.color_tables,
                                    user_tables: Some(self.user_tables.library()),
                                    show_experimental: false,
                                },
                            )
                        })
                    });
                let popup_rect = outcome.response.rect;
                let outcome = outcome.inner.inner;
                if let Some(product) = outcome.product {
                    selected_product = product;
                    self.product_picker_open = false;
                }
                if let Some(producer_name) = outcome.source_field {
                    selected_source_field = Some(producer_name);
                    self.product_picker_open = false;
                }
                if let Some(selection) = outcome.palette {
                    // Family-wide on purpose: installing a velocity table moves
                    // VEL, DVEL, SRV and DSRV together, because they are the
                    // same measurement drawn four ways.
                    Arc::make_mut(&mut self.color_tables)
                        .set_family(selection.family, selection.table);
                    self.palette_clock.bump();
                    palette_changed = true;
                }
                if let Some(request) = outcome.edit_palette {
                    self.palette_editor.edit_or_duplicate(
                        request.family,
                        &request.table,
                        request.duplicate,
                    );
                    self.product_picker_open = false;
                }
                // `crate::popup` rather than `clicked_elsewhere()`. That method
                // answered yes for the click that OPENED this popup - the click
                // was on the button, which is outside the popup - so the popup
                // opened and closed inside one frame and the product button was
                // dead. The rule now knows about that click.
                let dismissal = crate::popup::dismissal_from_input(
                    ui.ctx(),
                    popup_rect,
                    picker_button.rect,
                    opened_this_frame,
                    outcome.dismissed,
                );
                if dismissal.should_close() {
                    self.product_picker_open = false;
                }
            }

            // A colour table has to be reachable without the popup. It is the
            // control that tells an analyst whether a strange-looking field is
            // the data or the palette, so burying it one level down inside
            // another menu was wrong.
            let palette_family = crate::product_picker::palette_family(current_product);
            if let (Some(producer_name), Some(resolved)) =
                (current_source_field.as_deref(), current_source_palette.as_ref())
            {
                source_palette_action = source_palette_control(ui, producer_name, resolved);
            } else if current_source_field.is_none()
                && let Some(family) = palette_family
            {
                let installed = self.color_tables.for_family(family).clone();
                // Taken out of the popup rather than installed inside it, for
                // the reason the wide bar above gives.
                let mut picked = None;
                egui::ComboBox::from_id_salt("workstation-palette")
                    .selected_text(installed.name())
                    .width(210.0)
                    .show_ui(ui, |ui| {
                        for table in self.palette_offers.offers(
                            family,
                            &installed,
                            Some(self.user_tables.library()),
                        ) {
                            let chosen = table.name() == installed.name();
                            if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                                picked = Some(table.clone());
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Colour table for this product's family. The last row is the \
                         selected palette redrawn the other way: smooth or stepped.",
                    );
                if let Some(table) = picked {
                    Arc::make_mut(&mut self.color_tables).set_family(family, table);
                    self.palette_clock.bump();
                    palette_changed = true;
                }
            }

            crate::app_support::basemap_picker(ui, &mut self.map_scene, &mut self.settings_store);
            ui.menu_button("Layers", |ui| self.layers_menu(ui));

            let mut selected_quality = self.quality;
            // `ComboBox::show_ui` builds a non-wrapping horizontal child
            // before the wrapping parent sees the control's minimum width.
            // If the row has less than that width left, the button is laid
            // out in the sliver and its selected text is clipped away instead
            // of the whole control moving to the next row. Reserve the seam
            // explicitly so adding an earlier toolbar action cannot make the
            // active quality (for example, "Smooth") disappear.
            const QUALITY_COMBO_WIDTH: f32 = 92.0;
            if ui.available_size_before_wrap().x < QUALITY_COMBO_WIDTH {
                ui.end_row();
            }
            egui::ComboBox::from_id_salt("workstation-quality")
                .selected_text(selected_quality.preset_label().unwrap_or("Custom"))
                .width(QUALITY_COMBO_WIDTH)
                .show_ui(ui, |ui| {
                    for (label, preset) in render2d::DisplayQuality::PRESETS {
                        ui.selectable_value(&mut selected_quality, preset, label);
                    }
                })
                .response
                .on_hover_text(
                    "Display quality. Smooth adds sub-beams and sub-gates so a gate stops \
                     being a visible block; High and Ultra also supersample, which is what \
                     removes the speckle of a zoomed-out view. Ultra costs about sixteen \
                     times the native raster per frame.",
                );
            if selected_quality != self.quality {
                self.quality = selected_quality;
                quality_changed = true;
            }

            // The same control the menu bar carries, in the same bevelled
            // chip. This row is otherwise stock egui widgets, and that is
            // deliberate for the ordinary ones; a chip that has to stay
            // visibly latched in a screenshot while it is hiding weather is
            // not an ordinary one, which is the same exception the stall
            // notice above already takes.
            filter_changed |= crate::gate_filter_ui::draw_gate_filter_control(
                ui,
                crate::gate_filter_ui::GateFilterControl {
                    state: &mut self.gate_filter_ui,
                    registry: &self.settings_registry,
                    store: &mut self.settings_store,
                },
            );

            if ui.button("− Tilt").clicked() {
                tilt_delta = -1;
            }
            ui.label(self.active_tilt_label())
                .on_hover_text(self.active_tilt_hover());
            if ui.button("+ Tilt").clicked() {
                tilt_delta = 1;
            }
            let follow_label = if follow_policy.enabled {
                format!("Auto ≤{:.1}°", follow_policy.max_elevation_deg)
            } else {
                "Auto tilt".to_owned()
            };
            let follow_response = ui
                .selectable_label(follow_policy.enabled, follow_label)
                .on_hover_text(format!(
                    "Follow arriving live sweeps at or below {:.1}° with the \
                     real radial sweep animation; minimum scan-time gap {} s. \
                     Click to toggle; right-click \
                     to adjust the elevation, update interval, or feed polling.",
                    follow_policy.max_elevation_deg,
                    follow_policy.min_interval.as_secs(),
                ));
            if follow_response.clicked() {
                toggle_live_follow = true;
            }
            if follow_response.secondary_clicked() {
                open_live_follow_settings = true;
            }
            // The same two facts as on the menu bar, in this style's plain
            // widgets: one readout, one definition of what it says.
            let (snr_readout, resolution_notice) = self.active_censoring_readouts();
            if let Some(readout) = &snr_readout {
                ui.label(readout).on_hover_text(SNR_THRESHOLD_HINT);
            }
            if let Some(notice) = &resolution_notice {
                ui.label(notice).on_hover_text(RESOLUTION_REDUCED_HINT);
            }
            if ui
                .selectable_label(cameras_linked, "Link cameras")
                .clicked()
            {
                toggle_camera_links = true;
            }
            if ui
                .selectable_label(self.vol3d.open, "3D")
                .on_hover_text(
                    "Volumetric explorer: every tilt resampled into a box and ray marched",
                )
                .clicked()
            {
                self.vol3d.open = !self.vol3d.open;
            }
            if ui
                .selectable_label(self.xsection.armed || self.xsection.open, "XSec")
                .on_hover_text(
                    "Cross-section: arm, then click two points on a radar pane. A separate \
                     window shows the vertical slice of the current product along that line; \
                     drag the A/B handles to adjust.",
                )
                .clicked()
            {
                self.xsection.toggle_armed();
                if self.xsection.armed {
                    // One armed click-mode at a time: a click cannot be both
                    // a Vrot gate and a section endpoint.
                    self.vrot_active = false;
                    self.vrot_state.clear();
                    self.vrot_pane = None;
                }
            }
            if ui
                .selectable_label(self.vrot_active, "Vrot")
                .on_hover_text(
                    "Click two gates across a velocity couplet. Needs a dealiased product: \
                     measuring folded velocity gives a number wrong by a multiple of the \
                     Nyquist that still looks reasonable.",
                )
                .clicked()
            {
                self.vrot_active = !self.vrot_active;
                if self.vrot_active {
                    self.xsection.armed = false;
                }
                if !self.vrot_active {
                    self.vrot_state.clear();
                    self.vrot_pane = None;
                }
            }
            if self.vrot_state.measurement().is_some() || self.vrot_state.pending().is_some() {
                if ui.button("Clear Vrot").clicked() {
                    self.vrot_state.clear();
                    self.vrot_pane = None;
                }
                if let Some(measurement) = self.vrot_state.measurement() {
                    // A stale measurement stays readable but must not read as
                    // current: the reason is on the label itself, not only in
                    // hover text, because there is no hover on glass.
                    match self.vrot_state.stale_reason() {
                        Some(reason) => {
                            ui.label(format!(
                                "{} · STALE: {}",
                                self.vrot_readout(measurement),
                                reason.label()
                            ));
                        }
                        None => {
                            ui.label(self.vrot_readout(measurement));
                        }
                    }
                }
            }
            if ui.button("Settings").clicked() {
                self.settings_ui.open = true;
            }
            ui.label(format!("Pane {}", active.get() + 1));
        });

        if let Some(action) = source_palette_action {
            match action {
                SourcePaletteAction::Edit => {
                    if let Some(resolved) = current_source_palette.as_ref() {
                        self.palette_editor.edit_source_field(
                            current_product_id.clone(),
                            &resolved.table,
                            resolved.automatic,
                            resolved.current_is_durable,
                        );
                    }
                }
                SourcePaletteAction::Reset => {
                    if self.source_field_palettes.reset(&current_product_id) {
                        self.palette_clock.bump();
                        palette_changed = true;
                        self.status = format!(
                            "{} palette reset to automatic observed range",
                            current_source_field.as_deref().unwrap_or("source field")
                        );
                    }
                }
            }
        }

        if filter_changed {
            // The chip wrote straight to the store, so the cache the paint
            // path reads is one frame stale until this runs. Before the
            // invalidation below, so the re-render it asks for is requested
            // under the new filter rather than the old one.
            self.recompute_settings_cache();
            self.note_filter_cleared();
        }
        if quality_changed || palette_changed || filter_changed {
            // Same data, different picture: every pane's view generation moves,
            // which discards the in-flight render and asks for a new one
            // without throwing away the texture that is currently on screen.
            self.invalidate_view_panes(self.workspace.visible_panes());
        }

        if open_browser {
            self.show_file_browser();
        }
        if export_current_view {
            self.request_current_view_export(ui.ctx());
        }
        if export_loop {
            self.begin_loop_export(ui.ctx());
        }
        if open_online_data {
            self.online_data.open();
        }
        if let Some(path) = requested_load {
            self.begin_load(path);
        }
        match live_action {
            Some(LiveAction::Start(site)) => self.start_live(site),
            Some(LiveAction::Stop) => self.stop_live(),
            None => {}
        }
        if selected_layout != self.workspace.layout {
            self.workspace.set_layout(selected_layout);
        }
        if let Some(producer_name) = selected_source_field {
            self.apply_source_field_selection(active, &producer_name);
        } else if selected_product != current_product {
            self.apply_product_selection(active, selected_product);
        }
        if tilt_delta != 0 {
            self.change_active_tilt(tilt_delta);
        }
        if toggle_live_follow {
            self.set_live_follow_enabled(!follow_policy.enabled);
        }
        if open_live_follow_settings {
            self.settings_ui
                .open_category(crate::settings_ui::catalog::keys::data::CATEGORY);
        }
        if toggle_warnings {
            self.show_warnings = !self.show_warnings;
            // Force placement now rather than at the next cadence, so the map
            // answers the click on this frame.
            self.placed_hazards_at = None;
            self.placed_hazards_projection = None;
            self.refresh_placed_hazards();
            if self.show_warnings {
                self.warnings_service.refresh();
            }
        }
        if toggle_camera_links {
            let new_group = (!cameras_linked).then_some(0);
            for pane in self.workspace.visible_panes() {
                self.workspace.pane_mut(*pane).links.camera = new_group;
            }
        }
    }

    fn source_palette_for_pane(
        &self,
        pane: PaneId,
        product_id: &radar_core::ProductId,
        producer_name: &str,
    ) -> Option<crate::source_field_palettes::ResolvedSourceFieldPalette> {
        let volume = &self.history.current()?.volume;
        let cut_index = self.resolve_cut_index(pane, volume)?;
        let source = self
            .source_fields
            .display_on_cut(producer_name, cut_index)?;
        Some(
            self.source_field_palettes
                .resolve(product_id, &source, &self.color_tables),
        )
    }

    /// Change a pane's product, with everything that has to move with it.
    ///
    /// A product change on the Vrot pane retires the measurement: a DVEL gate
    /// paired with an SRV gate silently mixes ground-relative and
    /// storm-relative frames, and the number still looks reasonable.
    fn apply_product_selection(&mut self, active: PaneId, product: DisplayProduct) {
        let changed = self
            .workspace
            .apply_product_from(active, product.product_id());
        if self.vrot_pane.is_some_and(|pane| changed.contains(&pane)) {
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::DifferentProduct);
        }
        for pane in &changed {
            self.relative_power_fallback_from_ref[pane.index()] = false;
            self.live_follow_last_scan[pane.index()] = None;
            self.live_follow_manual_hold[pane.index()] = None;
        }
        self.invalidate_semantic_panes(&changed);
    }

    fn apply_source_field_selection(&mut self, active: PaneId, producer_name: &str) {
        let changed = self
            .workspace
            .apply_product_from(active, crate::source_fields::product_id(producer_name));
        if self.vrot_pane.is_some_and(|pane| changed.contains(&pane)) {
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::DifferentProduct);
        }
        for pane in &changed {
            self.relative_power_fallback_from_ref[pane.index()] = false;
        }
        self.invalidate_semantic_panes(&changed);
    }

    /// The operator's persistent list of local and HTTP(S) GR placefiles.
    /// Sources are owned by the manager; the global layer toggle remains in
    /// the normal settings document alongside every other persisted map knob.
    fn placefiles_window(&mut self, context: &egui::Context) {
        if !self.placefiles_window_open {
            return;
        }

        let mut open = self.placefiles_window_open;
        let mut browse_local = false;
        let mut layer_enabled = self.settings_cache.placefiles_enabled;
        egui::Window::new("Placefiles — GR / GR2Analyst overlays")
            .open(&mut open)
            .default_width(740.0)
            .default_height(460.0)
            .resizable(true)
            .show(context, |ui| {
                ui.label(
                    "Add a local placefile or HTTPS feed. Enabled icons, labels, lines and \
                     polygons appear above every correctly located radar pane.",
                );
                ui.horizontal(|ui| {
                    ui.checkbox(&mut layer_enabled, "Show enabled placefiles on the map");
                    if ui.button("Browse local file…").clicked() {
                        browse_local = true;
                    }
                    if ui
                        .add_enabled(
                            !self.placefiles.layers.is_empty(),
                            egui::Button::new("Refresh all"),
                        )
                        .clicked()
                    {
                        self.placefiles.refresh_all();
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Tip: drag a .pf, .placefile, .txt or extensionless GR placefile \
                         directly onto the radar window.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();
                if self.placefiles.ui(ui) {
                    context.request_repaint();
                }
            });
        self.placefiles_window_open = open;
        if layer_enabled != self.settings_cache.placefiles_enabled {
            self.set_placefiles_enabled(layer_enabled);
        }
        if browse_local {
            self.placefile_browser = Some(PlacefileBrowser::open());
        }
    }

    /// An egui-native local browser, separate from the radar-volume browser
    /// so selecting an overlay cannot replace the analyst's open storm.
    fn placefile_browser_window(&mut self, context: &egui::Context) {
        let Some(mut browser) = self.placefile_browser.take() else {
            return;
        };

        let mut open = true;
        let mut requested_directory = None;
        let mut selected_file = None;
        let mut refresh = false;
        egui::Window::new("Choose a local placefile")
            .open(&mut open)
            .default_width(660.0)
            .default_height(500.0)
            .resizable(true)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            browser.directory.parent().is_some(),
                            egui::Button::new("Up"),
                        )
                        .clicked()
                    {
                        requested_directory = browser.directory.parent().map(Path::to_path_buf);
                    }
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut browser.directory_text)
                            .desired_width((ui.available_width() - 110.0).max(120.0)),
                    );
                    let enter =
                        edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui.button("Go").clicked() || enter {
                        let path = PathBuf::from(browser.directory_text.trim());
                        if path.is_file() {
                            selected_file = Some(path);
                        } else {
                            requested_directory = Some(path);
                        }
                    }
                    if ui.button("Refresh").clicked() {
                        refresh = true;
                    }
                });
                if let Some(error) = &browser.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.separator();
                let row_height = ui.text_style_height(&egui::TextStyle::Body) + 5.0;
                egui::ScrollArea::vertical()
                    .id_salt("workstation-placefile-browser-entries")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, browser.entries.len(), |ui, visible| {
                        for index in visible {
                            let entry = &browser.entries[index];
                            let label = if entry.is_directory {
                                format!("{} /", entry.name)
                            } else {
                                entry.name.clone()
                            };
                            if ui.selectable_label(false, label).clicked() {
                                if entry.is_directory {
                                    requested_directory = Some(entry.path.clone());
                                } else {
                                    selected_file = Some(entry.path.clone());
                                }
                            }
                        }
                    });
            });

        if let Some(path) = selected_file {
            if self.placefiles.add_path(&path) {
                self.set_placefiles_enabled(true);
                self.placefiles_window_open = true;
                self.status = format!("Added placefile {}", path.display());
            } else {
                self.status = format!("Placefile already present or invalid: {}", path.display());
            }
            open = false;
            context.request_repaint();
        }
        if let Some(directory) = requested_directory {
            browser.change_directory(directory);
        } else if refresh {
            browser.reload();
        }
        if open {
            self.placefile_browser = Some(browser);
        }
    }

    /// The 3D volume explorer, in its own window.
    ///
    /// Follows the active pane's product, so switching that pane to velocity
    /// rebuilds the box from velocity rather than showing a reflectivity body
    /// under a velocity label.
    fn vol3d_window(&mut self, context: &egui::Context) {
        if !self.vol3d.open {
            return;
        }
        let product = match modeled_product_or_source_field(&self.workspace.active().product) {
            Ok(product) => product,
            Err(producer_name) => {
                let message = source_field_2d_only_message("3D Volume", producer_name);
                let mut open = self.vol3d.open;
                egui::Window::new("3D Volume")
                    .open(&mut open)
                    .default_size([460.0, 150.0])
                    .show(context, |ui| {
                        ui.label(egui::RichText::new(producer_name).monospace().strong());
                        ui.label(message);
                    });
                self.vol3d.open = open;
                return;
            }
        };
        let descriptor = product.descriptor();
        // Volume products are already a vertical reduction; there is nothing
        // left to ray march. Fall back to the moment they are built from.
        let moment = descriptor.computation.source_moment();
        let table = crate::palettes::table_for(descriptor, &self.color_tables);
        let range = descriptor.domain.declared_engine_range;
        let candidates = crate::app_support::vol3d_candidates(&self.history);
        let input = crate::vol3d::pane::Vol3dPaneInput {
            candidates: &candidates,
            moment,
            product_label: descriptor.short_name.to_owned(),
            color_table: &table,
            value_range: (range.min, range.max),
        };
        let mut open = self.vol3d.open;
        egui::Window::new("3D Volume")
            .open(&mut open)
            .default_size([900.0, 620.0])
            .show(context, |ui| {
                crate::vol3d::pane::draw_vol3d_pane(&mut self.vol3d, ui, &input);
            });
        self.vol3d.open = open;
    }

    /// The colour table editor, in its own window.
    ///
    /// The volume it previews on is the one the timeline has selected - the
    /// frame on screen - so a palette is judged against the storm the analyst
    /// is looking at rather than against a gradient.
    /// Put the `Open…` window up.
    ///
    /// The folder it lands in is the one the last session read successfully.
    /// Only when nothing has ever been stored does the path in the box get a
    /// say, and then only as the folder it sits in: somebody who reached a
    /// volume by typing its path almost always wants the next one from
    /// beside it.
    fn show_file_browser(&mut self) {
        let typed = self.source_path_text.trim();
        let near = (!typed.is_empty()).then(|| PathBuf::from(typed));
        self.file_browser.show(
            &self.settings_store,
            &self.settings_registry,
            near.as_deref(),
        );
    }

    fn request_current_view_export(&mut self, context: &egui::Context) {
        let frame = self.history.current();
        let site = frame.map(|frame| frame.identity.site_id.as_str());
        let volume_time = frame.map(|frame| frame.identity.volume_time);
        let product_id = self.workspace.active().product.clone();
        let product = DisplayProduct::from_product_id(&product_id);
        let product_name = crate::source_fields::producer_name_from_product_id(&product_id)
            .unwrap_or_else(|| product.id());
        let pane_count = self.workspace.visible_panes().len();
        let view = if pane_count == 1 {
            product_name.to_owned()
        } else {
            format!("{pane_count}panes-{product_name}-active")
        };
        let file_base =
            crate::current_view_export::capture_file_base(site, volume_time, &view, Utc::now());
        self.current_view_export.request(context, file_base);
    }

    /// Capture exactly the retained timeline, without synthesising or skipping
    /// any frame. Selection is restored even when a live eviction interrupts it.
    fn begin_loop_export(&mut self, context: &egui::Context) {
        if self.history.is_empty()
            || self.loop_export.is_some()
            || self.current_view_export.in_flight()
        {
            return;
        }

        let frame = self.history.current();
        let site = frame.map(|frame| frame.identity.site_id.as_str());
        let volume_time = frame.map(|frame| frame.identity.volume_time);
        let product_id = self.workspace.active().product.clone();
        let product = DisplayProduct::from_product_id(&product_id);
        let product_name = crate::source_fields::producer_name_from_product_id(&product_id)
            .unwrap_or_else(|| product.id());
        let pane_count = self.workspace.visible_panes().len();
        let view = if pane_count == 1 {
            product_name.to_owned()
        } else {
            format!("{pane_count}panes-{product_name}-active")
        };
        let file_base =
            crate::current_view_export::loop_file_base(site, volume_time, &view, Utc::now());
        let frame_keys: Vec<_> = self
            .history
            .frames()
            .iter()
            .map(LoopFrameKey::from_frame)
            .collect();
        let frame_count = frame_keys.len();
        self.next_loop_capture_id = self.next_loop_capture_id.wrapping_add(1);
        self.loop_export_notice = None;
        self.loop_export = Some(LoopExportState {
            capture_id: self.next_loop_capture_id,
            frame_keys,
            original_frame: frame.map(LoopFrameKey::from_frame),
            original_selected: self.history.selected_index(),
            original_follows_live: self.history.follows_live(),
            original_playback: self.history.playback(),
            next_index: 0,
            awaiting_screenshot: false,
            settled_paints_remaining: 2,
            frames: Vec::with_capacity(frame_count),
            file_base,
            delay_ms: self
                .settings_cache
                .loop_frame_time
                .as_millis()
                .clamp(1, u128::from(u32::MAX)) as u32,
        });
        self.history.set_playback(PlaybackState::Paused);
        self.select_loop_export_frame(context);
    }

    fn select_loop_export_frame(&mut self, context: &egui::Context) {
        let Some(target) = self
            .loop_export
            .as_ref()
            .and_then(|state| state.frame_keys.get(state.next_index))
            .cloned()
        else {
            return;
        };
        let Some(index) = self
            .history
            .frames()
            .iter()
            .position(|frame| LoopFrameKey::from_frame(frame) == target)
        else {
            self.abort_loop_export(
                context,
                "a frame was removed from the timeline before it could be captured",
            );
            return;
        };

        let before = self.current_frame_signature();
        self.history.select(index);
        self.commit_history_selection(before);
        if let Some(state) = self.loop_export.as_mut() {
            // eframe screenshots observe a presented frame, not a merely
            // installed texture. Two complete paints also clear a File popup.
            state.settled_paints_remaining = 2;
        }
        context.request_repaint();
    }

    fn restore_loop_export_selection(&mut self, state: &LoopExportState) {
        let before = self.current_frame_signature();
        if state.original_follows_live {
            self.history.go_live();
        } else {
            let original_index = state.original_frame.as_ref().and_then(|original| {
                self.history
                    .frames()
                    .iter()
                    .position(|frame| LoopFrameKey::from_frame(frame) == *original)
            });
            if let Some(index) = original_index.or_else(|| {
                state
                    .original_selected
                    .filter(|index| *index < self.history.len())
            }) {
                self.history.select(index);
            }
        }
        self.history.set_playback(state.original_playback);
        self.last_playback_step = Instant::now();
        self.commit_history_selection(before);
    }

    fn abort_loop_export(&mut self, context: &egui::Context, reason: &str) {
        if let Some(state) = self.loop_export.take() {
            self.restore_loop_export_selection(&state);
            let message = format!("Loop export failed: {reason}");
            self.status.clone_from(&message);
            self.loop_export_notice = Some(message);
            context.request_repaint();
        }
    }

    /// Consume only our private tagged screenshot; the PNG exporter continues
    /// to own its own unrelated capture events.
    fn handle_loop_capture_events(&mut self, context: &egui::Context) {
        let captures: Vec<(LoopCaptureTag, Arc<egui::ColorImage>)> = context.input(|input| {
            input
                .raw
                .events
                .iter()
                .filter_map(|event| {
                    let egui::Event::Screenshot {
                        user_data, image, ..
                    } = event
                    else {
                        return None;
                    };
                    let tag = user_data.data.as_ref()?.downcast_ref::<LoopCaptureTag>()?;
                    Some((tag.clone(), Arc::clone(image)))
                })
                .collect()
        });

        for (tag, image) in captures {
            let matches_pending = self.loop_export.as_ref().is_some_and(|state| {
                state.capture_id == tag.capture_id
                    && state.next_index == tag.frame_index
                    && state.awaiting_screenshot
            });
            if !matches_pending {
                continue;
            }

            let target = self
                .loop_export
                .as_ref()
                .and_then(|state| state.frame_keys.get(state.next_index));
            let current = self.history.current().map(LoopFrameKey::from_frame);
            if target != current.as_ref() {
                self.abort_loop_export(context, "the displayed frame changed during capture");
                return;
            }

            let state = self.loop_export.as_mut().expect("pending capture exists");
            state.frames.push(image);
            state.next_index += 1;
            state.awaiting_screenshot = false;
            if state.next_index < state.frame_keys.len() {
                self.select_loop_export_frame(context);
                continue;
            }

            let state = self.loop_export.take().expect("completed capture exists");
            self.restore_loop_export_selection(&state);
            self.current_view_export.request_loop(
                context,
                state.file_base,
                state.frames,
                state.delay_ms,
            );
            context.request_repaint();
        }
    }

    /// Called after the map, overlays, toolbar and timeline were all painted.
    fn drive_loop_export_capture(&mut self, context: &egui::Context) {
        let Some(state) = self.loop_export.as_ref() else {
            return;
        };
        if state.awaiting_screenshot {
            return;
        }
        let target = state.frame_keys.get(state.next_index);
        let current = self.history.current().map(LoopFrameKey::from_frame);
        if target != current.as_ref() {
            self.abort_loop_export(context, "the requested frame is no longer displayed");
            return;
        }
        if !self.visible_panes_ready() {
            context.request_repaint_after(Duration::from_millis(16));
            return;
        }

        let state = self.loop_export.as_mut().expect("active capture exists");
        if state.settled_paints_remaining > 0 {
            state.settled_paints_remaining -= 1;
            context.request_repaint();
            return;
        }
        let tag = LoopCaptureTag {
            capture_id: state.capture_id,
            frame_index: state.next_index,
        };
        state.awaiting_screenshot = true;
        context.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(tag)));
        context.request_repaint();
    }

    /// Draw the `Open…` window and load whatever it was pointed at.
    ///
    /// One file is an ordinary load; a multi-selection takes the same ordered
    /// playlist door as a multi-file drop.
    fn file_browser_window(&mut self, context: &egui::Context) {
        let units = self.settings_cache.units;
        let outcome = crate::file_browser::draw_file_browser(
            context,
            &mut self.file_browser,
            crate::file_browser::FileBrowserInput {
                units,
                store: &mut self.settings_store,
            },
        );
        if !outcome.open.is_empty() {
            self.begin_load_sequence(outcome.open);
        }
    }

    /// Ask before a very large local selection starts consuming RAM.
    ///
    /// This floating egui window does not block the event loop and, until
    /// Continue is pressed, does not clear or replace the session already on
    /// screen. It is strictly a warning: no estimate can disable Continue.
    fn playlist_preflight_window(&mut self, context: &egui::Context) {
        if let Some(pending) = self.pending_playlist_preflight.as_ref() {
            let selected = pending.selected;
            let mut open = true;
            let mut cancel = false;
            egui::Window::new("Estimating playlist memory…")
                .id(egui::Id::new("playlist-memory-estimating"))
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .default_width(520.0)
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(context, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Inspecting {selected} selected files…"));
                    });
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            "File metadata and the first 8 KiB signature are read on a worker. \
                             The current session remains usable while slow or network paths respond.",
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            if cancel || !open {
                self.cancel_playlist_preflight();
                let retained = self.history.len();
                self.status = format!(
                    "Playlist cancelled · {selected} selected · 0 decoded · 0 logical volumes · 0 new retained · existing session unchanged ({retained} frame(s)) · 0 failed"
                );
            }
            return;
        }

        let Some(pending) = self.pending_playlist_confirmation.as_ref() else {
            return;
        };
        let selected = pending.paths.len();
        let input = pending.estimate.input_size_text();
        let estimated = crate::playlist_preflight::format_binary_bytes(
            pending.estimate.estimated_decoded_bytes,
        );
        let method = pending.estimate.method_text();
        let caveat = pending.estimate.caveat_text();
        let mut open = true;
        let mut continue_loading = false;
        let mut cancel = false;

        egui::Window::new("Large playlist memory warning")
            .id(egui::Id::new("playlist-memory-preflight"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(580.0)
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .show(context, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(
                            "This selection may require substantial decoded memory.",
                        )
                        .strong(),
                    )
                    .wrap(),
                );
                ui.add_space(6.0);
                egui::Grid::new("playlist-memory-preflight-facts")
                    .num_columns(2)
                    .spacing([16.0, 5.0])
                    .show(ui, |ui| {
                        ui.label("Selected files");
                        ui.label(selected.to_string());
                        ui.end_row();
                        ui.label("Input size");
                        ui.label(input);
                        ui.end_row();
                        ui.label("Estimated decoded RAM");
                        ui.label(egui::RichText::new(estimated).strong());
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Method").strong());
                ui.add(egui::Label::new(method).wrap());
                ui.add_space(5.0);
                ui.label(egui::RichText::new("Caveat").strong());
                ui.add(egui::Label::new(caveat).wrap());
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        "This is a warning, not a limit. Continue loads the entire selection. \
                         Operator-selected local playlists are Unlimited by default; positive limits \
                         are optional under Settings → Data → Timeline retention. Unattended live \
                         feeds use a bounded fallback for each setting left at 0.",
                    )
                    .wrap(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Continue loading").clicked() {
                        continue_loading = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if continue_loading {
            if let Some(pending) = self.pending_playlist_confirmation.take() {
                self.start_load_sequence(pending.paths, pending.estimate);
            }
        } else if cancel || !open {
            self.cancel_playlist_preflight();
            let retained = self.history.len();
            self.status = format!(
                "Playlist cancelled · {selected} selected · 0 decoded · 0 logical volumes · 0 new retained · existing session unchanged ({retained} frame(s)) · 0 failed"
            );
        }
    }

    fn palette_editor_window(&mut self, context: &egui::Context) {
        if !self.palette_editor.open {
            return;
        }
        let volume = self.history.current().map(|frame| frame.volume.clone());
        let outcome = crate::palette_editor::draw_palette_editor(
            context,
            crate::palette_editor::PaletteEditorInput {
                state: &mut self.palette_editor,
                volume: volume.as_deref(),
            },
        );
        if let Some((family, table)) = outcome.install {
            // Same reach as every other palette install: a family, not a
            // product, because all four velocity products draw from one table.
            Arc::make_mut(&mut self.color_tables).set_family(family, table);
            self.palette_clock.bump();
            self.invalidate_view_panes(self.workspace.visible_panes());
        }
        if let Some(install) = outcome.source_install {
            let label = crate::source_fields::producer_name_from_product_id(&install.id)
                .unwrap_or(install.id.0.as_str())
                .to_owned();
            let changed = if install.durable {
                self.source_field_palettes
                    .apply_saved(install.id, install.table)
            } else {
                self.source_field_palettes
                    .apply_session(install.id, install.table)
            };
            if changed {
                self.palette_clock.bump();
                self.invalidate_view_panes(self.workspace.visible_panes());
            }
            self.status = if install.durable {
                format!(
                    "Saved palette binding applied only to {label}; it will return after restart"
                )
            } else {
                format!(
                    "Session-only preview applied to {label} · undo: CUSTOM → Reset to observed range"
                )
            };
        }
        let promoted_source = outcome.source_saved.and_then(|(id, table)| {
            self.source_field_palettes
                .promote_matching_saved(&id, &table)
                .then(|| {
                    crate::source_fields::producer_name_from_product_id(&id)
                        .unwrap_or(id.0.as_str())
                        .to_owned()
                })
        });
        if let Some(path) = outcome.saved {
            self.status = promoted_source.map_or_else(
                || format!("Colour table saved to {}", path.display()),
                |label| {
                    format!(
                        "Colour table saved to {} · {label} binding saved for restart",
                        path.display()
                    )
                },
            );
            // The folder has a file in it that nothing has read yet, and the
            // focus rescan will never notice: focus was never lost - the save
            // happened inside this window. So the picker, the settings page
            // and the toolbar combo would all keep offering the list they
            // built before the save, and a table an analyst has just written
            // would be missing from every one of them until they alt-tabbed.
            self.user_tables_rescan_pending = true;
        }
    }

    /// The cross-section window, following the active pane's product.
    ///
    /// Same shape as `vol3d_window`: a volume product is already a vertical
    /// reduction, so the slice is built from its source moment, and the
    /// table handed over is the one the pane paints with.
    fn xsection_window(&mut self, context: &egui::Context) {
        if !self.xsection.open {
            return;
        }
        let product = match modeled_product_or_source_field(&self.workspace.active().product) {
            Ok(product) => product,
            Err(producer_name) => {
                let message = source_field_2d_only_message("Cross-section", producer_name);
                self.xsection
                    .source_field_2d_only_window(context, producer_name, &message);
                return;
            }
        };
        let descriptor = product.descriptor();
        let moment = descriptor.computation.source_moment();
        let table = crate::palettes::table_for(descriptor, &self.color_tables);
        let storm_motion = product.is_storm_relative().then(|| {
            let intent = self.workspace.active().storm_motion;
            render2d::StormMotion {
                direction_deg: (intent.direction_from_deg + 180.0).rem_euclid(360.0),
                speed_mps: intent.speed_mps,
            }
        });
        let selected = self.history.selected_index();
        let candidates: Vec<crate::xsection::XsCandidate<'_>> = self
            .history
            .frames()
            .iter()
            .enumerate()
            .map(|(index, frame)| crate::xsection::XsCandidate {
                volume: &frame.volume,
                displayed: Some(index) == selected,
            })
            .collect();
        let input = crate::xsection::XSectionInput {
            candidates: &candidates,
            moment,
            product_label: descriptor.short_name.to_owned(),
            uses_dealiased_velocity: descriptor.computation.uses_dealiased_velocity(),
            storm_motion,
            color_table: &table,
            domain: descriptor.domain,
            units: self.settings_cache.units,
            range_decimals: self.settings_cache.annotation.range_decimals,
            top_m: self.settings_cache.xsection_top_m,
        };
        self.xsection.window(context, &input);
    }

    /// Whether the frame on screen knows where on earth it was measured.
    ///
    /// True when there is no frame at all: an empty instrument showing the
    /// basemap it opened on is not claiming anything about a sweep.
    ///
    /// An RVP8 time-series header carries a signal-processor name and no
    /// coordinates, so a Level 1 record's position is a lookup - see
    /// [`Self::time_series_site`]. The research radars the archives are made of
    /// are placed from a table in the binary and are located the instant they
    /// are opened; anything else waits on the station directory, which is a
    /// network fetch cached on disk and therefore simply absent on a cold
    /// machine or an offline one. Until it lands - or if the site is in neither
    /// catalog, which is the permanent case - the sweep's ranges and azimuths
    /// are real and its geography does not exist.
    fn frame_position_is_known(&self) -> bool {
        self.history
            .current()
            .is_none_or(|frame| frame.volume.site.latitude_deg.is_some())
    }

    /// The map underlay, markers and projection this pane may draw.
    ///
    /// # Why a sweep with no position gets no map
    ///
    /// Everything geographic in here is anchored on a radar position, and a
    /// frame that has none is drawn wherever the map anchor happened to be
    /// left. The pane then makes two statements at once: the header says
    /// POSITION UNKNOWN, and underneath it labelled counties and a
    /// four-decimal cursor readout say precisely where the storm is. One of
    /// those is a fabricated position, and it is the one drawn in the largest
    /// type - a KOUN stare from Norman, Oklahoma was photographed over Smith
    /// and Osborne counties, Kansas, with a confident lat/lon under the
    /// cursor.
    ///
    /// So an unlocated frame gets no basemap, no imagery, no site markers, no
    /// warning polygons and - because the coordinate half of the corner
    /// readout is computed through this projection - no coordinates. What
    /// stays is everything that is true without a position: the sweep, the
    /// range rings, and the range and azimuth from the antenna. The moment the
    /// directory lands, `locate_time_series_frame` gives the frame its
    /// position and all of it comes back.
    fn pane_map(&mut self, pane: PaneId, camera: Camera2D, pane_rect: egui::Rect) -> PaneMap {
        let located = self.frame_position_is_known();
        PaneMap {
            // Ask the scene for this pane's LOD. Once resident this is a cache
            // lookup; it queues a build only when the bucket is new.
            geometry: located
                .then(|| {
                    self.map_scene
                        .geometry_for_pane(pane.index(), camera.sanitized().km_per_point)
                })
                .flatten(),
            tiles: located
                .then(|| {
                    self.map_scene
                        .tiles_for_pane(pane.index(), camera, pane_rect)
                })
                .flatten(),
            projection: located.then(|| self.map_scene.projection()).flatten(),
            // Paint-time colours for the chosen basemap look. Read from the
            // style the controller is holding rather than stored beside it,
            // so the picker has exactly one thing to set. Kept even with no
            // map: it is what the pane clears to, and a pane with no ground
            // colour is not more honest, only darker.
            chrome: map_scene::MapChrome::for_style(self.map_scene.style()),
            sites: if located {
                Arc::clone(&self.placed_sites)
            } else {
                Arc::from([])
            },
            site_labels: self.settings_cache.site_labels,
            annotation: self.settings_cache.annotation,
            units: self.settings_cache.units,
            active_site: self.live_site.clone(),
            hazards: if located {
                Arc::clone(&self.placed_hazards)
            } else {
                Arc::from([])
            },
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        // One clock read for the whole canvas, so four panes cannot disagree
        // about what time it is by a few microseconds and print two different
        // ages for the same volume.
        let now = Utc::now();
        let volume = self
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume));
        // Every pane is filtered by the SAME criteria - there is one filter,
        // not four - but not every pane can obey them: a volume-derived
        // product is integrated out of the whole volume rather than rastered
        // from one sweep, and `render_service` answers that pane with
        // `GateFilterReport::not_applicable`. So the header's filter statement
        // is built per pane, from the pane's own product, and a pane the
        // filter did not run on says that rather than claiming gates are
        // hidden in it. See `pane_header_status`.
        for (pane, pane_rect) in pane_rects(rect, self.workspace.layout) {
            // Everything below draws with the DISPLAY camera - the analyst's
            // stored camera plus the derived north-up rotation - so the
            // basemap, the imagery, the echo, the markers, the warnings, the
            // rings, the labels and the section line are one picture. The
            // stored camera is written back separately, without the
            // derivation; see the `apply_camera_from` call at the foot of this
            // loop.
            // One frame object for the whole pane: it answers both what the map
            // is drawn with and what a gesture is resolved through, so those two
            // can never come apart. See `crate::north_up`.
            let north_up = self.north_up_frame(pane, Self::pane_viewport(ui, pane_rect));
            let camera = north_up.display_camera(self.workspace.pane(pane).camera);
            let product_id = self.workspace.pane(pane).product.clone();
            let source_name =
                crate::source_fields::producer_name_from_product_id(&product_id).map(str::to_owned);
            let product = DisplayProduct::from_product_id(&product_id);
            let cut_index = volume
                .as_deref()
                .and_then(|volume| self.resolve_cut_index(pane, volume));
            let source_display = source_name.as_deref().and_then(|producer_name| {
                self.source_fields.display_on_cut(producer_name, cut_index?)
            });
            let source_palette = source_display.as_ref().map(|source| {
                self.source_field_palettes
                    .resolve(&product_id, source, &self.color_tables)
            });
            let title = match (source_name.as_deref(), source_display.as_ref()) {
                (_, Some(source)) => {
                    source_field_pane_title(volume.as_deref(), pane, source, cut_index)
                }
                (Some(producer_name), None) => unavailable_source_field_pane_title(
                    volume.as_deref(),
                    pane,
                    producer_name,
                    cut_index,
                ),
                (None, None) => pane_title(volume.as_deref(), pane, product, cut_index),
            };
            let mut status = self.pane_header_status(pane, product, now);
            if source_name.is_some() {
                let palette_status =
                    source_palette
                        .as_ref()
                        .map_or("palette unavailable on this cut", |resolved| {
                            if resolved.automatic {
                                "automatic observed-range palette"
                            } else {
                                "custom exact-field palette and fixed range"
                            }
                        });
                status = if status.is_empty() {
                    format!("SOURCE FIELD · native-grid · {palette_status}")
                } else {
                    format!("SOURCE FIELD · native-grid · {palette_status} · {status}")
                };
            }
            let pane_map = self.pane_map(pane, camera, pane_rect);
            let mut badges = self.pane_badges(product, now);
            if source_name.is_some() {
                badges.insert(0, "SOURCE FIELD".to_owned());
            }
            let interaction = {
                let texture =
                    self.panes[pane.index()]
                        .texture
                        .as_ref()
                        .map(|texture| PaneTexture {
                            handle: &texture.handle,
                            camera: texture.camera,
                            viewport: texture.viewport,
                        });
                // The raster is painted with `palettes::table_for`
                // (render_service.rs), so the legend has to read the same table
                // or the bar explains a picture drawn with a different one. For
                // a derived-volume product whose domain is metres or kilograms,
                // a base-moment dBZ ramp does not even intersect the domain, so
                // the legend vanished instead of being wrong visibly: VIL
                // Density had no legend at all.
                let (table, domain, product_name) = match (
                    source_name.as_deref(),
                    source_display.as_ref(),
                    source_palette.as_ref(),
                ) {
                    (_, Some(source), Some(resolved)) => {
                        let (minimum, maximum) = resolved.value_range();
                        let domain = crate::source_fields::numeric_domain(minimum, maximum);
                        (
                            Some(resolved.table.clone()),
                            Some(domain),
                            source.producer_name.as_str(),
                        )
                    }
                    (Some(producer_name), None, _) => (None, None, producer_name),
                    (None, None, _) => (
                        Some(crate::palettes::table_for(
                            product.descriptor(),
                            &self.color_tables,
                        )),
                        Some(product.domain()),
                        product.descriptor().short_name,
                    ),
                    // `source_palette` is resolved directly from a present
                    // source display, so this arm is only a defensive blank:
                    // never explain source pixels with REF colours.
                    (_, Some(source), None) => (None, None, source.producer_name.as_str()),
                };
                // The legend can be turned off in Settings; `None` draws no
                // bar, the same as a product whose domain has no ladder.
                let layout = if self.settings_cache.legend {
                    domain
                        .as_ref()
                        .zip(table.as_ref())
                        .and_then(|(domain, table)| crate::legend::legend_layout(domain, table))
                } else {
                    None
                };
                let overlay = crate::pane_canvas::PaneOverlay {
                    legend: layout.as_ref(),
                    table: table.as_ref(),
                    product_name,
                    badges: &badges,
                    probe: self.panes[pane.index()].probe_text.as_deref(),
                    spectrum: self.panes[pane.index()].spectrum.as_ref(),
                };
                let layers = PaneExternalLayers {
                    observations: self
                        .settings_cache
                        .observations_enabled
                        .then_some(&self.surface_observations),
                    placefiles: self
                        .settings_cache
                        .placefiles_enabled
                        .then_some(&self.placefiles),
                    frame_time: volume.as_ref().map(|volume| volume.volume_time),
                };
                draw_pane_with_layers(
                    ui,
                    pane,
                    pane_rect,
                    pane == self.workspace.active_pane,
                    camera,
                    north_up,
                    self.settings_cache.nav,
                    texture,
                    &pane_map,
                    &title,
                    &status,
                    &overlay,
                    layers,
                )
            };

            // Section line over the pane. Its endpoint widgets register after
            // the pane's interact region, which is what routes an endpoint
            // drag to the endpoint instead of panning the camera
            // (see xsection.rs module docs).
            self.xsection.draw_pane_overlay(
                ui,
                pane.index(),
                pane_rect,
                interaction.camera,
                interaction.viewport,
                self.settings_cache.units,
            );

            if self.vrot_active && interaction.clicked && source_name.is_none() {
                self.take_vrot_sample(pane, volume.as_deref(), cut_index, product);
            } else if self.xsection.wants_pane_clicks()
                && interaction.clicked
                && let Some(world) = interaction.hovered_world_km
                && self.xsection.handle_pane_click(world)
            {
                self.workspace.set_active(pane);
            } else if let Some(station) = interaction.clicked_observation {
                self.workspace.set_active(pane);
                self.surface_observations.request_station_history_at(
                    &station,
                    volume.as_ref().map(|volume| volume.volume_time),
                );
                self.status = format!("Loading observation history for {station}");
            } else if let Some(site) = interaction.clicked_site {
                // Clicking a site marker is the quickest way to change radar.
                self.workspace.set_active(pane);
                self.site_text = site.to_uppercase();
                self.start_live(site);
            } else if let Some((lon, lat)) = interaction.ctrl_clicked_lon_lat {
                // Ctrl+click loads the nearest S-band NEXRAD. A TDWR sits
                // closer to most downtowns than the WSR-88D does and must never
                // win; `nearest_site` is where that is decided. Note the
                // argument swap: the projection returns (lon, lat) and
                // `nearest_s_band_site` takes (lat, lon).
                self.workspace.set_active(pane);
                match crate::nearest_site::nearest_s_band_site(lat, lon, &self.sites) {
                    Some(choice) => {
                        let status = choice.status_line(self.settings_cache.units);
                        self.site_text = choice.id.to_uppercase();
                        self.start_live(choice.id);
                        // AFTER the load kick: `start_live` writes its own
                        // status, so setting this first would be invisible.
                        self.status = status;
                    }
                    None => {
                        self.status =
                            crate::nearest_site::no_site_in_range_status(self.settings_cache.units)
                    }
                }
            } else if interaction.clicked {
                self.workspace.set_active(pane);
            }
            self.panes[pane.index()].hovered_world_km = interaction.hovered_world_km;
            match (source_name.is_some(), source_display.as_ref()) {
                (_, Some(source)) => {
                    self.refresh_source_probe(pane, volume.as_deref(), cut_index, source)
                }
                (true, None) => {
                    self.panes[pane.index()].probe_text = None;
                    self.panes[pane.index()].spectrum = None;
                }
                (false, None) => self.refresh_probe(pane, volume.as_deref(), cut_index, product),
            }
            self.update_viewport(pane, interaction.viewport);
            if interaction.camera_changed {
                // Strip the derived rotation on the way back in. The centre
                // and the scale a gesture produces are WORLD quantities and
                // are valid whatever rotation resolved them -
                // `pan_by_screen_delta` and `zoom_about` both apply the
                // camera's own rotation correctly - but the rotation itself is
                // not the analyst's, and storing it would make the derivation
                // compound with itself on the next frame.
                let stored_rotation_rad = self.workspace.pane(pane).camera.rotation_rad;
                let changed = self.workspace.apply_camera_from(
                    pane,
                    Camera2D {
                        rotation_rad: stored_rotation_rad,
                        ..interaction.camera
                    },
                );
                self.invalidate_view_panes(&changed);
            }
            if let Some(volume) = &volume {
                self.ensure_render_requested(pane, Arc::clone(volume), interaction.viewport);
            }
        }
    }

    fn timeline(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let frame_count = self.history.len();
        let mut selected = self.history.selected_index().unwrap_or(0);
        let export_active = self.loop_export.is_some();
        let mut choose_frame = None;
        let mut go_live = false;
        let mut toggle_playback = false;
        let mut export_loop = false;

        ui.horizontal(|ui| {
            if ui
                .add_enabled(frame_count > 1 && !export_active, egui::Button::new("◀"))
                .clicked()
            {
                choose_frame = selected.checked_sub(1);
            }
            if ui
                .add_enabled(
                    frame_count > 1 && !export_active,
                    egui::Button::new(if self.history.playback() == PlaybackState::Playing {
                        "Pause"
                    } else {
                        "Play"
                    }),
                )
                .clicked()
            {
                toggle_playback = true;
            }
            if ui
                .add_enabled(
                    frame_count > 1 && selected + 1 < frame_count && !export_active,
                    egui::Button::new("▶"),
                )
                .clicked()
            {
                choose_frame = Some(selected + 1);
            }
            if ui
                .add_enabled(frame_count > 0 && !export_active, egui::Button::new("Go live"))
                .clicked()
            {
                go_live = true;
            }

            if ui
                .add_enabled(
                    frame_count > 0 && !export_active && !self.current_view_export.in_flight(),
                    egui::Button::new("Export loop"),
                )
                .on_hover_text("Save this timeline as an animated GIF in Downloads")
                .clicked()
            {
                export_loop = true;
            }

            if frame_count > 1 {
                let response = ui.add_enabled_ui(!export_active, |ui| {
                    ui.add_sized(
                        [220.0, ui.spacing().interact_size.y],
                        egui::Slider::new(&mut selected, 0..=frame_count - 1).show_value(false),
                    )
                });
                if response.inner.changed() {
                    choose_frame = Some(selected);
                }
            }

            ui.separator();
            ui.label(self.timeline_status(Utc::now()));
            if let Some(status) = self.sequence_status.as_deref() {
                ui.label(status).on_hover_text(
                    self.sequence_detail
                        .as_deref()
                        .unwrap_or("File playlist status"),
                );
            }
            if let Some(state) = self.loop_export.as_ref() {
                ui.label(format!(
                    "Exporting loop: frame {} of {}",
                    state.next_index + 1,
                    state.frame_keys.len()
                ))
                .on_hover_text("Capturing the fully rendered radar map, legends and overlays");
            } else if let Some(notice) = self.loop_export_notice.as_deref() {
                ui.label(notice);
            } else if let Some(status) = self.current_view_export.status() {
                ui.label(status).on_hover_text(
                    self.current_view_export
                        .detail()
                        .unwrap_or("Current-view PNG export"),
                );
            }
            if let Some(load_ms) = self.load_ms {
                ui.label(format!("decode {load_ms:.1} ms"));
            }
            ui.label(format!(
                "history {:.1} MiB",
                self.history.estimated_bytes() as f64 / (1024.0 * 1024.0)
            ));
            ui.label(history_policy_status(self.history.policy()))
                .on_hover_text(
                    "For selected local files, 0 means Unlimited. Live feeds substitute the 30-frame / 1 GiB safe fallback for zero dimensions. Positive limits apply to both, and every eviction is reported.",
                );
            let queued = self.render_service.queued_panes();
            if queued > 0 {
                ui.label(format!("{queued} pane(s) queued"));
            }
        });

        if toggle_playback {
            let next = if self.history.playback() == PlaybackState::Playing {
                PlaybackState::Paused
            } else {
                self.last_playback_step = Instant::now();
                PlaybackState::Playing
            };
            self.history.set_playback(next);
            context.request_repaint();
        }
        if go_live {
            let before = self.current_frame_signature();
            self.history.go_live();
            self.history.set_playback(PlaybackState::Paused);
            self.commit_history_selection(before);
        } else if let Some(index) = choose_frame {
            let before = self.current_frame_signature();
            self.history.select(index);
            self.history.set_playback(PlaybackState::Paused);
            self.commit_history_selection(before);
        }
        if export_loop {
            self.begin_loop_export(context);
        }
    }

    fn ensure_render_requested(
        &mut self,
        pane: PaneId,
        volume: Arc<RadarVolume>,
        viewport: ViewportMetrics,
    ) {
        let product_id = self.workspace.pane(pane).product.clone();
        let source_name =
            crate::source_fields::producer_name_from_product_id(&product_id).map(str::to_owned);
        let product = DisplayProduct::from_product_id(&product_id);
        let stamp = self.current_stamp(pane);
        let Some(cut_index) = self.resolve_cut_index(pane, &volume) else {
            // Terminal for this stamp, so playback does not wait for ever on
            // a picture that cannot exist - see [`RenderTerminal`].
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.terminal = Some(RenderTerminal::Unavailable(stamp));
            if source_name.is_some() {
                // There will be no replacement raster for this source stamp;
                // keeping the previous product indefinitely would relabel old
                // pixels as this unavailable native field.
                runtime.texture = None;
            }
            runtime.status = format!(
                "{} unavailable",
                source_name.as_deref().unwrap_or_else(|| product.id())
            );
            return;
        };
        let runtime = &self.panes[pane.index()];
        let already_current = runtime
            .texture
            .as_ref()
            .is_some_and(|texture| texture.stamp == stamp);
        if already_current || runtime.pending_stamp == Some(stamp) {
            return;
        }
        // A stamp the worker has already failed is never resubmitted: the
        // identical request fails the identical way, and re-queueing it every
        // frame hot-looped the single worker (15-20 ms per derived retry,
        // measured) with every other pane serialized behind it. Any clock
        // bump makes a new stamp and retries on its own.
        if runtime.terminal == Some(RenderTerminal::Failed(stamp)) {
            return;
        }

        // A volume product needs the measurement; without it there is nothing
        // to select tilts from, and drawing an empty field would look like a
        // storm-free sky rather than a missing prerequisite.
        let Some(capabilities) = self.capabilities.as_ref().map(Arc::clone) else {
            return;
        };
        let source_field = source_name.as_deref().and_then(|producer_name| {
            let source = self
                .source_fields
                .display_on_cut(producer_name, cut_index)?;
            let resolved =
                self.source_field_palettes
                    .resolve(&product_id, &source, &self.color_tables);
            Some(crate::render_service::SourceFieldRender {
                moment: source.moment,
                table: resolved.table,
            })
        });
        if source_name.is_some() && source_field.is_none() {
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.terminal = Some(RenderTerminal::Unavailable(stamp));
            runtime.texture = None;
            runtime.status = format!(
                "{} has no finite values",
                source_name.as_deref().unwrap_or("source field")
            );
            return;
        }
        // A sweep still filling is drawn over the last complete picture of the
        // same tilt. A complete sweep is not blended at all, so an archive file
        // renders down exactly the path it always did.
        let sweep = if source_field.is_some() {
            None
        } else {
            self.panes[pane.index()]
                .sweep_state
                .filter(|state| !state.complete)
                .and_then(|state| {
                    let moment = product.source_moment();
                    let (previous_volume, previous_cut_index) =
                        crate::app_support::previous_sweep_for(
                            &self.history,
                            &volume,
                            cut_index,
                            &moment,
                        )?;
                    Some(SweepBlendRequest {
                        previous_volume,
                        previous_cut_index,
                        start_deg: state.start_deg,
                        revealed_deg: state.revealed_deg,
                    })
                })
        };

        let request = RenderRequest {
            pane,
            stamp,
            volume,
            capabilities,
            environment: self.hail_environment.clone(),
            cut_index,
            product,
            source_field,
            // The DISPLAY camera, so the echo is rastered under the same
            // rotation the basemap is drawn under. `radar_raster_view` carries
            // it into `ViewportRasterOptions::rotation_rad`, and the raster's
            // azimuth lookup takes it off again.
            camera: self.display_camera(pane, viewport),
            viewport,
            storm_motion: self.workspace.pane(pane).storm_motion,
            color_tables: Arc::clone(&self.color_tables),
            quality: self.quality,
            sweep,
            // The analyst's own criteria, straight off the settings cache and
            // into the worker that reads the gates. `GateFilter::OFF` until
            // they ask for something else, and OFF renders down the path this
            // application always used.
            //
            // Whatever is here, the pane must show
            // `RenderedPane::gate_filter` when it comes back active: a
            // filtered picture may never be distinguishable from a quiet sky
            // only by the absence of echo. The pane's FILTERED band and the
            // legend badge are built from this same cached value, and
            // `install_render` cross-checks them against what the engine
            // reports it actually removed.
            gate_filter: self.settings_cache.gate_filter,
        };
        match self.render_service.request(request) {
            Ok(()) => {
                let runtime = &mut self.panes[pane.index()];
                runtime.pending_stamp = Some(stamp);
                runtime.status = "rendering".to_owned();
            }
            Err(_) => {
                self.panes[pane.index()].status = "render worker closed".to_owned();
            }
        }
    }

    /// Step every pane's sweep reveal on by one frame.
    ///
    /// Only panes with no render in flight are stepped, and that restriction is
    /// load-bearing rather than an optimisation. The reveal is part of the
    /// render stamp, so moving it while a render is running would make that
    /// render stale the instant it landed, `install_render` would drop it, and
    /// the pane would never install anything at all. Tying each step to the
    /// completion of the last one also makes the animation self-pacing: a
    /// slower render takes fewer, larger steps instead of falling behind.
    fn advance_sweeps(&mut self) {
        let Some((identity, volume)) = self
            .history
            .current()
            .map(|frame| (frame.identity.clone(), Arc::clone(&frame.volume)))
        else {
            for runtime in &mut self.panes {
                runtime.reset_sweep();
            }
            return;
        };

        // Only the live edge animates. A frame the analyst has scrubbed back to
        // is finished data, and revealing it a spoke at a time would animate
        // history rather than report on an arriving sweep.
        if self.loop_export.is_some() || !self.history.at_live_edge() {
            for runtime in &mut self.panes {
                runtime.reset_sweep();
            }
            return;
        }

        // Sweep animation off takes the same path: with no reveal in the
        // render request, every radial that has arrived paints at once, which
        // is what the pane did before the animation existed.
        if !self.settings_cache.sweep_animation {
            for runtime in &mut self.panes {
                runtime.reset_sweep();
            }
            return;
        }

        let now = Instant::now();
        for index in 0..analyst_runtime::MAX_PANES {
            let Some(pane) = PaneId::new(index as u8) else {
                continue;
            };
            // Resolved before the mutable borrow below: both read `self`.
            let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
            let product_key = crate::source_fields::producer_name_from_product_id(
                &self.workspace.pane(pane).product,
            )
            .unwrap_or_else(|| product.id())
            .to_owned();
            let cut_index = self.resolve_cut_index(pane, &volume);
            let key = cut_index.map(|cut_index| SweepKey {
                identity: identity.clone(),
                product: product_key,
                cut_index,
            });
            let runtime = &mut self.panes[index];

            let (Some(cut_index), Some(key)) = (cut_index, key) else {
                runtime.reset_sweep();
                continue;
            };
            if runtime.sweep_key.as_ref() != Some(&key) {
                runtime.reset_sweep();
                runtime.sweep_key = Some(key);
            }
            if runtime.pending_stamp.is_some() {
                continue;
            }
            let Some(cut) = volume.cuts.get(cut_index) else {
                runtime.reset_sweep();
                continue;
            };

            let elapsed = runtime
                .sweep_stepped_at
                .map(|stepped_at| now.saturating_duration_since(stepped_at))
                .unwrap_or_default();
            let catch_up = runtime
                .sweep_state
                .map(|state| catch_up_factor(state.pending_deg()))
                .unwrap_or(1.0);
            let before = runtime.sweep_state;
            // The analyst's pace multiplier rides on the same clock as the
            // backlog catch-up; 1x follows the antenna's measured rate.
            let after = runtime.sweep.observe(
                cut,
                elapsed.mul_f32(catch_up * self.settings_cache.sweep_speed),
            );
            runtime.sweep_state = after;
            runtime.sweep_stepped_at = Some(now);

            // Only a reveal that actually moved is worth a render. Without this
            // a settled pane would re-render every frame forever, because the
            // stamp would change on every step whether or not the picture did.
            if before != after {
                self.sweep_clocks[index].bump();
            }
        }
    }

    /// The tilt as it was in the previous frame, for a sweep still arriving.
    ///
    /// `None` is not a failure: the first volume after a site change genuinely
    /// has nothing older to underpaint with, and the blend then draws the
    /// arrived wedge alone, which is what the pane did before any of this
    /// existed.
    fn update_viewport(&mut self, pane: PaneId, viewport: ViewportMetrics) {
        let changed = self.panes[pane.index()]
            .viewport
            .is_none_or(|previous| viewport_changed(previous, viewport));
        if changed {
            self.panes[pane.index()].viewport = Some(viewport);
            self.panes[pane.index()].pending_stamp = None;
            self.view_clocks[pane.index()].bump();
        }
    }

    fn invalidate_view_panes(&mut self, panes: &[PaneId]) {
        for pane in panes {
            self.view_clocks[pane.index()].bump();
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.terminal = None;
        }
    }

    /// Same site, different meaning: a product or tilt change.
    ///
    /// The texture is deliberately KEPT, exactly as `invalidate_view_panes`
    /// keeps it for a camera change. Dropping it blanked the pane to bare
    /// basemap on every product switch and every tilt step - key-repeat tilt
    /// stepping held the pane blank most of the time - for the 1.6-29 ms the
    /// replacement render takes. The old picture is a wrong product for one
    /// render's duration; bare basemap was wrong for the same duration and
    /// looked like lost data. The pane clock bump already marks the texture
    /// stale, so nothing downstream mistakes it for current.
    fn invalidate_semantic_panes(&mut self, panes: &[PaneId]) {
        for pane in panes {
            self.pane_clocks[pane.index()].bump();
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.terminal = None;
            runtime.status.clear();
            runtime.reset_sweep();
        }
    }

    /// Reset every pane's render bookkeeping while KEEPING what is on screen.
    ///
    /// For frame changes inside one session: playback steps, scrub notches,
    /// live volume hand-overs, stage promotions. The caller bumps
    /// `frame_clock` beside this, which makes every held texture stale - so
    /// `visible_panes_ready` still answers not-ready and playback pacing is
    /// untouched - while the analyst keeps a picture until `install_render`
    /// swaps it. Dropping the texture here is what blanked the whole app to
    /// bare basemap on every frame change.
    fn reset_all_panes(&mut self) {
        for runtime in &mut self.panes {
            runtime.pending_stamp = None;
            runtime.terminal = None;
            runtime.status.clear();
            // The reveal describes a position in a sweep that is no longer on
            // screen. Easing on from it would wipe the new picture in from
            // wherever the old one happened to have got to.
            runtime.reset_sweep();
        }
    }

    /// [`Self::reset_all_panes`] plus the texture drop.
    ///
    /// Reserved for `begin_load` and `start_live`: a new session may be a new
    /// radar, and old pixels over new ground would be a lie worth blanking
    /// for. Every within-session frame change goes through
    /// [`Self::reset_all_panes`] instead.
    fn clear_all_panes(&mut self) {
        self.reset_all_panes();
        for runtime in &mut self.panes {
            runtime.texture = None;
        }
    }

    /// The pane's stored camera, plus the rotation that puts north up at the
    /// middle of THIS pane.
    ///
    /// The rotation is DERIVED here on every frame and never written back.
    /// That is the load-bearing choice, not an implementation detail:
    /// [`Camera2D`] keeps its exact current meaning (the analyst's own
    /// intent), the settings file keeps persisting only what the analyst
    /// chose, link groups, history, `apply_site_change` and `centre_on_anchor`
    /// are untouched, and `camera_changed` cannot fire on a frame where only
    /// the derivation moved. Storing it would make every one of those a new
    /// question.
    ///
    /// The rule itself, the 460 km floor and the citations behind it live on
    /// `RadarProjection::view_rotation_rad`; how a GESTURE is resolved through
    /// the same rule, so that it and its inverse still compose to the identity,
    /// lives on [`crate::north_up::NorthUpFrame`].
    fn display_camera(&self, pane: PaneId, viewport: ViewportMetrics) -> Camera2D {
        let stored = self.workspace.pane(pane).camera;
        self.north_up_frame(pane, viewport).display_camera(stored)
    }

    /// The north-up frame this pane is being drawn in.
    ///
    /// One object so that the rotation the map is DRAWN with and the rotation a
    /// GESTURE is resolved through can never be derived from two different
    /// readings of the same rule.
    fn north_up_frame(&self, pane: PaneId, viewport: ViewportMetrics) -> NorthUpFrame {
        NorthUpFrame::new(
            self.map_scene.projection(),
            viewport,
            self.workspace.pane(pane).camera.rotation_rad,
        )
    }

    /// The viewport a pane rectangle implies, the same way `draw_pane`
    /// measures it, so the display rotation is derived from the pane the
    /// analyst is actually looking at.
    fn pane_viewport(ui: &egui::Ui, pane_rect: egui::Rect) -> ViewportMetrics {
        ViewportMetrics {
            width_points: pane_rect.width().max(1.0),
            height_points: pane_rect.height().max(1.0),
            pixels_per_point: ui.ctx().pixels_per_point().max(1.0),
        }
    }

    fn current_stamp(&self, pane: PaneId) -> RenderStamp {
        RenderStamp {
            pane_id: pane.get(),
            session: self.session_clock.current(),
            frame: self.frame_clock.current(),
            pane: self.pane_clocks[pane.index()].current(),
            view: self.view_clocks[pane.index()].current(),
            palette: self.palette_clock.current(),
            sweep: self.sweep_clocks[pane.index()].current(),
        }
    }

    /// Re-measure the current volume when anything about it changes.
    ///
    /// The key includes source label, cut count and radial count, not just the
    /// radar/time identity and stage. Different local research files can
    /// legitimately share site/time metadata, while a live volume grows in
    /// place as chunks append radials and cuts under one stable source label.
    /// Omitting either discriminator can answer questions from a different
    /// retained frame or from the first fragment that arrived.
    fn refresh_capabilities(&mut self) {
        let key = self.history.current().map(|frame| CapabilitiesKey {
            identity: frame.identity.clone(),
            source_label: frame.source_label.clone(),
            stage: frame.stage,
            cuts: frame.volume.cuts.len(),
            radials: frame.volume.cuts.iter().map(|cut| cut.radials.len()).sum(),
        });
        if key == self.capabilities_for && self.capabilities.is_some() {
            return;
        }
        self.capabilities = self
            .history
            .current()
            .map(|frame| Arc::new(product_engine::VolumeCapabilities::analyze(&frame.volume)));
        self.capabilities_for = key;
        // Greying out a product the volume cannot show is a claim about the
        // data, so it is remeasured wherever the measurement is.
        self.product_availability =
            ProductAvailabilityIndex::from_optional_capabilities(self.capabilities.as_deref());
        self.source_fields = self
            .history
            .current()
            .map(|frame| crate::source_fields::SourceFieldCatalog::from_volume(&frame.volume))
            .unwrap_or_default();
    }

    fn current_frame_signature(
        &self,
    ) -> Option<(analyst_runtime::FrameIdentity, FrameStage, String)> {
        self.history.current().map(|frame| {
            (
                frame.identity.clone(),
                frame.stage,
                frame.source_label.clone(),
            )
        })
    }

    /// How much data the current frame holds, as (cuts, radials).
    ///
    /// A live volume grows in place: chunks arrive and radials are appended
    /// under one site, one volume time and the stage `Partial`. Its identity
    /// and stage therefore do not change while it fills, which is why growth
    /// needs its own measure - see `install_loaded_volume`.
    fn current_frame_extent(&self) -> Option<(usize, usize)> {
        self.history.current().map(|frame| {
            (
                frame.volume.cuts.len(),
                frame.volume.cuts.iter().map(|cut| cut.radials.len()).sum(),
            )
        })
    }

    fn commit_history_selection(
        &mut self,
        before: Option<(analyst_runtime::FrameIdentity, FrameStage, String)>,
    ) {
        if self.current_frame_signature() != before {
            self.frame_clock.bump();
            // The slider emits `choose_frame` per notch, so this used to blank
            // every pane on every scrub notch. Hold the outgoing frame until
            // the selected one renders instead.
            self.reset_all_panes();
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::NewVolume);
        }
    }

    /// Which sweep this pane should draw.
    ///
    /// Delegates to `product_engine::cut_selection`, which chooses by measured
    /// elevation and scan time rather than by position in the file. The
    /// difference is not cosmetic: on a VCP 212 SAILSx3 volume the lowest tilt
    /// is scanned four times across the volume period, and taking the first one
    /// listed serves velocity that is over four minutes older than a sweep of
    /// the same tilt sitting in the same file.
    /// Take one endpoint of a Vrot measurement from the point just clicked.
    fn take_vrot_sample(
        &mut self,
        pane: PaneId,
        volume: Option<&RadarVolume>,
        cut_index: Option<usize>,
        product: DisplayProduct,
    ) {
        self.workspace.set_active(pane);
        if self.vrot_pane != Some(pane) {
            // Starting in a different pane abandons the half-finished pair
            // rather than pairing gates from two different pictures.
            self.vrot_state.clear();
            self.vrot_pane = Some(pane);
        }
        let Some((east_km, north_km)) = self.panes[pane.index()].hovered_world_km else {
            return;
        };
        let (Some(volume), Some(cut_index)) = (volume, cut_index) else {
            return;
        };
        let descriptor = product.descriptor();
        let elevation_deg = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(cut_index))
            .map(|cut| cut.nominal_elevation_deg)
            .or_else(|| volume.cuts.get(cut_index).map(|cut| cut.elevation_deg))
            .unwrap_or_default();
        let moment = descriptor.computation.source_moment();
        // The pane's own censor. Without it the readout answers a censored
        // gate with its true value at a pixel the pane drew empty, and the
        // analyst is told a number that is not on the screen. The PRODUCT goes
        // in as well as the moment, because a volume-derived pane is not
        // censored by the renderer either - see `probe_censor`.
        let censor = self.probe_censor(volume, cut_index, &moment, product);
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &moment,
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
            censor,
        );
        let crate::probe::ProbeReading::Value(value) = reading else {
            self.status = "Vrot: that point has no velocity".to_owned();
            return;
        };
        let sample = crate::vrot::VrotSample::from_probe(&value);

        match self.vrot_state.pending().cloned() {
            None => {
                self.vrot_state = crate::vrot::VrotState::AwaitingSecond(sample);
                self.status = "Vrot: click the other side of the couplet".to_owned();
            }
            Some(first) => {
                let dealiased = descriptor.computation.uses_dealiased_velocity();
                match crate::vrot::measure(first, sample, dealiased) {
                    Ok(measurement) => {
                        self.status = self.vrot_report_line(&measurement);
                        self.vrot_state = crate::vrot::VrotState::Complete(measurement);
                    }
                    Err(refusal) => {
                        self.status = format!("Vrot refused: {}", refusal.label());
                        self.vrot_state = crate::vrot::VrotState::Idle;
                    }
                }
            }
        }
    }

    /// Read the value under this pane's cursor from the sweep it is drawing.
    ///
    /// Uses the pointer position captured during the previous paint, so the
    /// volume is never scanned while laying out a frame.
    /// The mask the pane's filter is currently removing gates with, for the
    /// sweep a readout is about to read.
    ///
    /// `None` means nothing is censored there: the filter is off, it hid
    /// nothing on this sweep, or the sweep does not carry the moment. All
    /// three are the same answer to a readout - this gate is not one the pane
    /// hid.
    ///
    /// Recomputed only when the sweep, the moment or the criteria move. The
    /// off path recomputes nothing and drops the memo, so an unfiltered
    /// session pays for none of this.
    ///
    /// One honesty note: for a dealiased-velocity product the picture is
    /// censored after unfolding while this reads the folded sweep, because
    /// that is the sweep [`crate::probe::probe_polar`] itself reads. The two
    /// can only disagree on a gate the radar flagged range-folded, and only
    /// while `hide_range_folded` is on. Matching the grid the readout reads is
    /// the lesser error: the alternative reports a censor against numbers the
    /// readout never quotes.
    ///
    /// # Why the product, and not just the moment
    ///
    /// The readout has to be censored by exactly what the RENDERER censored,
    /// and the renderer does not censor a volume-derived product at all:
    /// `render_request` routes those to `render_derived`, which answers
    /// `GateFilterReport::not_applicable` and paints every gate. All seven of
    /// them - composite reflectivity, echo tops, VIL, VIL density, MESH, POH,
    /// POSH - report `MomentType::Reflectivity` as their source moment, so a
    /// censor keyed on the moment alone finds a reflectivity mask and applies
    /// it to a picture that was never filtered.
    ///
    /// What that shipped as: Storm mode on, a Composite Reflectivity pane,
    /// the cursor over a gate the REF criterion would hide - the readout read
    /// `CREF FILTERED` at a pixel `render_derived` had painted from the whole
    /// volume, on a pane whose own band correctly read `FILTER NOT APPLIED
    /// HERE`. Two indicators on one pane telling opposite stories, which is
    /// the failure this whole integration exists to prevent, arriving from the
    /// other direction: claiming data is hidden where it is shown.
    ///
    /// So the routing fact is `DisplayProduct::derived_volume()`, which is the
    /// same fact `render_service::render_request` routes on and the same one
    /// the pane band is built from (see `central_panel`). One fact, three
    /// readers, and `the_readout_and_the_renderer_censor_the_same_panes` pins
    /// them to each other.
    fn probe_censor(
        &mut self,
        volume: &RadarVolume,
        cut_index: usize,
        moment: &radar_core::MomentType,
        product: DisplayProduct,
    ) -> Option<&render2d::GateFilterMask> {
        let filter = self.settings_cache.gate_filter;
        if !filter.is_active() || product.derived_volume().is_some() {
            self.probe_censor = None;
            return None;
        }
        let fresh = self.probe_censor.as_ref().is_some_and(|censor| {
            censor.site == volume.site.id
                && censor.volume_time == volume.volume_time
                && censor.cut_index == cut_index
                && censor.moment == *moment
                && censor.filter == filter
        });
        if !fresh {
            let mask = volume
                .cuts
                .get(cut_index)
                .and_then(|cut| cut.moments.get(moment))
                .and_then(|grid| {
                    render2d::evaluate_gate_filter(volume, cut_index, grid, &filter).mask
                });
            self.probe_censor = Some(ProbeCensor {
                site: volume.site.id.clone(),
                volume_time: volume.volume_time,
                cut_index,
                moment: moment.clone(),
                filter,
                mask,
            });
        }
        self.probe_censor
            .as_ref()
            .and_then(|censor| censor.mask.as_ref())
    }

    fn refresh_probe(
        &mut self,
        pane: PaneId,
        volume: Option<&RadarVolume>,
        cut_index: Option<usize>,
        product: DisplayProduct,
    ) {
        let Some((east_km, north_km)) = self.panes[pane.index()].hovered_world_km else {
            self.panes[pane.index()].probe_text = None;
            self.panes[pane.index()].spectrum = None;
            return;
        };
        let (Some(volume), Some(cut_index)) = (volume, cut_index) else {
            self.panes[pane.index()].probe_text = None;
            self.panes[pane.index()].spectrum = None;
            return;
        };
        let descriptor = product.descriptor();
        // The measured elevation, so the beam height is computed from the angle
        // the antenna actually flew rather than from the first radial's.
        let elevation_deg = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(cut_index))
            .map(|cut| cut.nominal_elevation_deg)
            .or_else(|| volume.cuts.get(cut_index).map(|cut| cut.elevation_deg))
            .unwrap_or_default();
        let moment = descriptor.computation.source_moment();
        // The pane's own censor. Without it the readout answers a censored
        // gate with its true value at a pixel the pane drew empty, and the
        // analyst is told a number that is not on the screen. The PRODUCT goes
        // in as well as the moment, because a volume-derived pane is not
        // censored by the renderer either - see `probe_censor`.
        let censor = self.probe_censor(volume, cut_index, &moment, product);
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &moment,
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
            censor,
        );
        self.panes[pane.index()].probe_text = Some(crate::probe::format_reading(
            &reading,
            &descriptor.domain,
            descriptor.short_name,
            self.settings_cache.units,
            self.settings_cache.annotation.range_decimals,
        ));
        // Built from the SAME reading, so the plot and the numbers beside it
        // can never be describing two different gates - which is what would
        // happen if the panel re-derived the gate from the cursor position
        // under its own rounding.
        self.panes[pane.index()].spectrum = self.gate_spectrum_for(&reading);
    }

    fn refresh_source_probe(
        &mut self,
        pane: PaneId,
        volume: Option<&RadarVolume>,
        cut_index: Option<usize>,
        source: &crate::source_fields::SourceFieldDisplay,
    ) {
        let Some((east_km, north_km)) = self.panes[pane.index()].hovered_world_km else {
            self.panes[pane.index()].probe_text = None;
            self.panes[pane.index()].spectrum = None;
            return;
        };
        let (Some(volume), Some(cut_index)) = (volume, cut_index) else {
            self.panes[pane.index()].probe_text = None;
            self.panes[pane.index()].spectrum = None;
            return;
        };
        let elevation_deg = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(cut_index))
            .map(|cut| cut.nominal_elevation_deg)
            .or_else(|| volume.cuts.get(cut_index).map(|cut| cut.elevation_deg))
            .unwrap_or_default();
        let censor = self.probe_censor(
            volume,
            cut_index,
            &source.moment,
            DisplayProduct::Reflectivity,
        );
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &source.moment,
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
            censor,
        );
        let product_id = &self.workspace.pane(pane).product;
        let resolved = self
            .source_field_palettes
            .resolve(product_id, source, &self.color_tables);
        let (minimum, maximum) = resolved.value_range();
        let domain = crate::source_fields::numeric_domain(minimum, maximum);
        let mut text = crate::probe::format_reading(
            &reading,
            &domain,
            &source.producer_name,
            self.settings_cache.units,
            self.settings_cache.annotation.range_decimals,
        );
        text.push_str(" | producer unit token: ");
        text.push_str(source.producer_units.as_deref().unwrap_or("not provided"));
        self.panes[pane.index()].probe_text = Some(text);
        // A processed source field has no Level 1 pulse channel attached to
        // it merely because an I/Q record happens to be open in the session.
        self.panes[pane.index()].spectrum = None;
    }

    /// The spectrum panel for a probe reading, when a Level 1 record is open.
    ///
    /// The probe's `row` is the moment grid's row, which for a processed sweep
    /// is the dwell index, and its `gate` is the gate column - the two indices
    /// `sweep_gate_spectrum` takes. That correspondence is what lets a hover
    /// over a rendered pixel become the transform of the pulses that pixel was
    /// made from.
    fn gate_spectrum_for(
        &self,
        reading: &crate::probe::ProbeReading,
    ) -> Option<crate::iq_spectrum_ui::GateSpectrum> {
        let session = self.iq.as_ref()?;
        let channel = self.settings_cache.iq_spectrum_channel;
        let (row, gate, range_m, blank) = match reading {
            crate::probe::ProbeReading::Value(value) => {
                (value.row, value.gate, value.slant_range_m as f32, None)
            }
            // A gate that is there and blank still gets a panel: "this gate is
            // empty, and here is why" is the answer to the question the
            // analyst asked by hovering, and it matters most here - the whole
            // point of the SNR censor knob is that what an analyst moving it
            // is asking is what got removed. Every emitted moment of a
            // censored gate is NaN, so this is the reading such a gate
            // produces; returning `None` for it left the branch below - and
            // the sentence it exists to write - unreachable.
            crate::probe::ProbeReading::Absent {
                row,
                gate,
                slant_range_m,
                state,
                ..
            } => (*row, *gate, *slant_range_m as f32, Some(*state)),
            // Outside the sweep there is no gate to report on and the panel
            // goes away.
            crate::probe::ProbeReading::OutsideSweep(_) => return None,
        };
        let estimate = session
            .processed()
            .dwell(row)
            .and_then(|dwell| dwell.get(gate));
        if let Some(absence) = gate_spectrum_absence(blank, estimate) {
            return Some(crate::iq_spectrum_ui::GateSpectrum {
                spectrum: None,
                estimator_velocity_mps: None,
                range_m,
                channel,
                absence: Some(absence),
            });
        }
        match session.spectrum(row, gate, channel) {
            Ok(spectrum) => Some(crate::iq_spectrum_ui::GateSpectrum {
                range_m: spectrum.range_m,
                spectrum: Some(spectrum),
                estimator_velocity_mps: estimate
                    .map(|estimate| estimate.velocity_mps)
                    .filter(|velocity| velocity.is_finite()),
                channel,
                absence: None,
            }),
            // A single-polarisation record asked for its vertical channel is
            // the case this reaches: say so rather than showing an empty frame.
            Err(error) => Some(crate::iq_spectrum_ui::GateSpectrum {
                spectrum: None,
                estimator_velocity_mps: None,
                range_m,
                channel,
                absence: Some(error),
            }),
        }
    }

    /// Re-estimate the open time-series record with the settings now in force.
    ///
    /// Cheap enough to do on the UI thread and measured before it was left
    /// there: the reference record is 1,830 pulses of 248 gates, and one
    /// pass over it at a 64-pulse dwell is a few milliseconds across the
    /// rayon pool the estimator already uses. It is not a file read and not a
    /// network call, so there is nothing here worth the complexity of a worker
    /// and a generation clock.
    ///
    /// A refusal - a staggered-PRT record, a dwell longer than the record - is
    /// reported and the previous field is LEFT on screen. Blanking the pane
    /// would lose the picture the analyst already had over a slider they can
    /// simply drag back.
    fn apply_timeseries_settings(&mut self) {
        let controls = iq_controls_from(&self.settings_registry, &self.settings_store);
        let Some(session) = self.iq.as_mut() else {
            return;
        };
        match session.set_controls(controls) {
            Ok(false) => return,
            Ok(true) => {}
            Err(error) => {
                self.status = format!("Level 1: {error}");
                return;
            }
        }
        // The site was resolved when the file was opened; re-resolving it here
        // would drop the position on a settings change if the directory had
        // since been cleared.
        let site = self
            .history
            .current()
            .map(|frame| frame.volume.site.clone())
            .unwrap_or_else(|| radar_core::RadarSite::new(session.site_id()));
        let source_label = session.source_label().to_owned();
        let provenance = session.provenance();
        let mut volume = session.volume(site);
        volume.metadata.source_path = Some(source_label.clone());
        self.history.install(VolumeFrame::new(
            Arc::new(volume),
            FrameOrigin::Local,
            FrameStage::Complete,
            source_label,
        ));
        // The frame's identity and stage are unchanged - same site, same
        // volume time - so nothing downstream can notice by comparing
        // signatures. The clock bump is what puts the re-estimated field on
        // screen, and the capabilities are dropped because the cut's radial
        // count changed with the dwell.
        self.capabilities = None;
        self.capabilities_for = None;
        self.frame_clock.bump();
        self.reset_all_panes();
        self.status = format!("Level 1: {provenance}");
    }

    fn resolve_cut_index(&self, pane: PaneId, volume: &RadarVolume) -> Option<usize> {
        let intent = self.workspace.pane(pane);
        if let Some(producer_name) =
            crate::source_fields::producer_name_from_product_id(&intent.product)
        {
            let available = |index: usize| {
                crate::source_fields::grid_in_cut(volume, index, producer_name).is_some()
            };
            return match intent.tilt {
                TiltSelection::LowestAvailable => {
                    (0..volume.cuts.len()).find(|index| available(*index))
                }
                TiltSelection::CutIndex(index) => {
                    let index = usize::from(index);
                    available(index)
                        .then_some(index)
                        .or_else(|| (0..volume.cuts.len()).find(|index| available(*index)))
                }
                TiltSelection::NearestElevationTenths(target) => {
                    let target = f32::from(target) / 10.0;
                    volume
                        .cuts
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| available(*index))
                        .min_by(|(_, left), (_, right)| {
                            (left.elevation_deg - target)
                                .abs()
                                .total_cmp(&(right.elevation_deg - target).abs())
                        })
                        .map(|(index, _)| index)
                }
            };
        }
        let product = DisplayProduct::from_product_id(&intent.product);
        let descriptor = product.descriptor();
        let moment = descriptor.computation.source_moment();
        let policy = descriptor.cut_policy;

        let Some(capabilities) = self.capabilities.as_ref() else {
            // Measurement has not run yet this frame. Draw something rather
            // than nothing; the next frame will have the real answer.
            return product.first_available_cut(volume);
        };

        match intent.tilt {
            TiltSelection::LowestAvailable => {
                product_engine::cut_selection::select_lowest_tilt(capabilities, &moment, policy)
                    .map(|choice| choice.cut_index)
            }
            TiltSelection::CutIndex(index) => {
                let index = usize::from(index);
                product
                    .is_available_in_cut(volume, index)
                    .then_some(index)
                    .or_else(|| {
                        product_engine::cut_selection::select_lowest_tilt(
                            capabilities,
                            &moment,
                            policy,
                        )
                        .map(|choice| choice.cut_index)
                    })
            }
            TiltSelection::NearestElevationTenths(target) => {
                product_engine::cut_selection::select_nearest_elevation(
                    capabilities,
                    f32::from(target) / 10.0,
                    &moment,
                    policy,
                )
                .map(|choice| choice.cut_index)
            }
        }
    }

    fn change_active_tilt(&mut self, delta: isize) {
        let Some(volume) = self
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
        else {
            return;
        };
        let active = self.workspace.active_pane;
        let Some(current) = self.resolve_cut_index(active, &volume) else {
            return;
        };
        if let Some(producer_name) = crate::source_fields::producer_name_from_product_id(
            &self.workspace.pane(active).product,
        ) {
            let next = if delta == 0 {
                Some(current)
            } else {
                let mut index = current as isize;
                loop {
                    index += delta;
                    if index < 0 || index >= volume.cuts.len() as isize {
                        break None;
                    }
                    if crate::source_fields::grid_in_cut(&volume, index as usize, producer_name)
                        .is_some()
                    {
                        break Some(index as usize);
                    }
                }
            };
            let Some(next) = next else {
                return;
            };
            let changed = self
                .workspace
                .apply_tilt_from(active, TiltSelection::CutIndex(next as u16));
            self.hold_live_follow_for_manual_tilts(&changed);
            self.invalidate_semantic_panes(&changed);
            return;
        }
        let product = DisplayProduct::from_product_id(&self.workspace.pane(active).product);
        // Step one commanded tilt, not one cut. On a split-cut volume the next
        // entry in the cut list is the other leg of the same elevation, so
        // stepping by index makes "+ Tilt" stand still.
        let next = match self.capabilities.as_ref() {
            Some(capabilities) => product_engine::cut_selection::step_tilt(
                capabilities,
                current,
                delta,
                &product.descriptor().computation.source_moment(),
                product.descriptor().cut_policy,
            )
            .map(|choice| choice.cut_index),
            None => product.next_available_cut(&volume, current, delta),
        };
        let Some(next) = next else {
            return;
        };
        let changed = self
            .workspace
            .apply_tilt_from(active, TiltSelection::CutIndex(next as u16));
        self.hold_live_follow_for_manual_tilts(&changed);
        // Belt and braces beside `vrot::measure`'s own `DifferentCuts`
        // refusal: the refusal stops a cross-tilt PAIR, this stops a finished
        // measurement reading as current after the pane left its tilt.
        if self.vrot_pane.is_some_and(|pane| changed.contains(&pane)) {
            self.vrot_state
                .mark_stale(crate::vrot::StaleReason::DifferentCut);
        }
        self.invalidate_semantic_panes(&changed);
    }

    /// Why this sweep and not another. Shown on hover over the tilt readout,
    /// because "the pane jumped from 0.48 to 0.44 degrees" is otherwise an
    /// unexplained change rather than a four-minute-fresher picture.
    fn active_tilt_hover(&self) -> String {
        let pane = self.workspace.active_pane;
        let product = match modeled_product_or_source_field(&self.workspace.pane(pane).product) {
            Ok(product) => product,
            Err(producer_name) => return self.source_field_tilt_hover(pane, producer_name),
        };
        let Some(capabilities) = self.capabilities.as_ref() else {
            return "No volume measured yet".to_owned();
        };
        let descriptor = product.descriptor();
        let moment = descriptor.computation.source_moment();
        let Some(choice) = product_engine::cut_selection::select_lowest_tilt(
            capabilities,
            &moment,
            descriptor.cut_policy,
        ) else {
            return format!("No sweep in this volume carries {moment}");
        };
        let Some(cut) = capabilities.cut(choice.cut_index) else {
            return "No sweep selected".to_owned();
        };
        let mut lines = vec![
            format!(
                "cut {} of {} - {} leg at {:.2}° (stored {:.2}°)",
                choice.cut_index,
                capabilities.cuts.len(),
                choice.leg.label(),
                cut.nominal_elevation_deg,
                cut.stored_elevation_deg
            ),
            format!(
                "{} radials, {:.0}° of azimuth{}",
                cut.radial_count,
                cut.azimuth_coverage_deg,
                if cut.complete { "" } else { ", still arriving" }
            ),
        ];
        if let Some(nyquist) = cut.representative_nyquist_mps {
            lines.push(format!("Nyquist {nyquist:.1} m/s"));
        }
        if choice.repeats_passed_over > 0 {
            lines.push(format!(
                "{} other sweep(s) of this tilt in the volume",
                choice.repeats_passed_over
            ));
        }
        if choice.older_alternative_ms > 0 {
            lines.push(format!(
                "{:.1} s fresher than the first sweep listed in the file",
                choice.older_alternative_ms as f32 / 1000.0
            ));
        }
        lines.join(
            "
",
        )
    }

    /// Describe the exact producer field selected by the pane without asking
    /// the static product registry to interpret it.
    fn source_field_tilt_hover(&self, pane: PaneId, producer_name: &str) -> String {
        let limit =
            "Exact source fields are currently 2D only; no modeled product was substituted.";
        let Some(frame) = self.history.current() else {
            return format!("Exact source field {producer_name} is selected.\n{limit}");
        };
        let Some(cut_index) = self.resolve_cut_index(pane, &frame.volume) else {
            return format!(
                "No sweep in this volume carries exact source field {producer_name}.\n{limit}"
            );
        };
        let Some((_, grid)) =
            crate::source_fields::grid_in_cut(&frame.volume, cut_index, producer_name)
        else {
            return format!(
                "No sweep in this volume carries exact source field {producer_name}.\n{limit}"
            );
        };
        let Some(stored_cut) = frame.volume.cuts.get(cut_index) else {
            return format!("Exact source field {producer_name} is selected.\n{limit}");
        };
        let measured_cut = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(cut_index));
        let nominal_elevation_deg = measured_cut
            .map(|cut| cut.nominal_elevation_deg)
            .unwrap_or(stored_cut.elevation_deg);
        let mut lines = vec![
            format!("Exact source field {producer_name} · 2D only"),
            format!(
                "cut {cut_index} of {} at {nominal_elevation_deg:.2}° (stored {:.2}°)",
                frame.volume.cuts.len(),
                stored_cut.elevation_deg
            ),
            format!(
                "{} source rows on {} sweep radials",
                grid.radial_count(),
                stored_cut.radials.len()
            ),
        ];
        if let Some(cut) = measured_cut {
            lines.push(format!(
                "{:.0}° of azimuth{}",
                cut.azimuth_coverage_deg,
                if cut.complete { "" } else { ", still arriving" }
            ));
        }
        if let Some(description) = grid.producer_description.as_deref() {
            lines.push(format!("Producer description: {description}"));
        }
        lines.push(format!(
            "Producer unit token: {}",
            grid.producer_units.as_deref().unwrap_or("not provided")
        ));
        lines.push(limit.to_owned());
        lines.join("\n")
    }

    fn active_tilt_label(&self) -> String {
        let Some(frame) = self.history.current() else {
            return "No tilt".to_owned();
        };
        let Some(index) = self.resolve_cut_index(self.workspace.active_pane, &frame.volume) else {
            return "Unavailable".to_owned();
        };
        // The measured elevation, not the stored one. The stored angle is the
        // first radial's, taken while the antenna is still ramping onto the
        // tilt, so real 0.5-degree sweeps label themselves "0.4" and disagree
        // with every other radar viewer.
        self.capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.cut(index))
            // Two decimals, not one. The commanded tilt is 0.5 degrees but the
            // antenna flies 0.44, and rounding that to "0.4" reads as a wrong
            // 0.5 rather than as a right measurement. There is no VCP
            // elevation table here to recover the commanded angle from, so the
            // honest thing is to show what was measured, precisely enough that
            // nobody mistakes it for a label.
            .map(|cut| format!("{:.2}°", cut.nominal_elevation_deg))
            .or_else(|| {
                frame
                    .volume
                    .cuts
                    .get(index)
                    .map(|cut| format!("{:.2}°", cut.elevation_deg))
            })
            .unwrap_or_else(|| "Unavailable".to_owned())
    }

    /// What the operational processor did to the sweep the active pane is
    /// drawing, before it wrote the file: the signal-to-noise threshold it
    /// censored this moment at, and - only when the control flags claim it -
    /// that what is on disk is coarser than what the radar collected.
    ///
    /// Beside the tilt readout and read off the same cut that readout
    /// resolves, because censoring is a property of the sweep on screen rather
    /// than of the volume: on a split-cut VCP the surveillance leg and the
    /// Doppler leg of one elevation are censored at different thresholds, and
    /// a number that did not move when the tilt did would be describing the
    /// wrong sweep.
    ///
    /// Both come back `None` when the source never stated them. Only the
    /// NEXRAD generic data moment header carries these two fields (ICD
    /// 2620002W, Build 22.0, 05 June 2023, Table XVII-B, bytes 16-17 and byte
    /// 18); a Message 1 volume predates that block, and ODIM, CfRadial and
    /// DORADE have no equivalent, so the bar says nothing rather than
    /// implying a 0.0 dB threshold on an un-recombined sweep.
    fn active_censoring_readouts(&self) -> (Option<String>, Option<String>) {
        let Some(frame) = self.history.current() else {
            return (None, None);
        };
        let pane = self.workspace.active_pane;
        let Some(index) = self.resolve_cut_index(pane, &frame.volume) else {
            return (None, None);
        };
        let moment = match crate::source_fields::producer_name_from_product_id(
            &self.workspace.pane(pane).product,
        ) {
            Some(producer_name) => {
                crate::source_fields::grid_in_cut(&frame.volume, index, producer_name)
                    .map(|(moment, _)| moment.clone())
            }
            None => Some(
                DisplayProduct::from_product_id(&self.workspace.pane(pane).product).source_moment(),
            ),
        };
        let Some(moment) = moment else {
            return (None, None);
        };
        let Some(grid) = frame
            .volume
            .cuts
            .get(index)
            .and_then(|cut| cut.moments.get(&moment))
        else {
            return (None, None);
        };
        // `radar_core` owns both the rounding rule and the words, so this
        // readout and any other reading of the same fields cannot drift into
        // two spellings of one number.
        let threshold = grid.snr_threshold_db.map(|threshold_db| {
            format!(
                "{} SNR threshold {} dB",
                moment.short_name(),
                radar_core::format_snr_threshold_db(threshold_db)
            )
        });
        let resolution_loss = grid
            .recombination
            .filter(radar_core::MomentRecombination::reduces_resolution)
            .map(|recombination| {
                format!(
                    "Resolution reduced on this sweep: {}",
                    recombination.label()
                )
            });
        (threshold, resolution_loss)
    }

    /// How old the volume on screen is at `now`. `None` before the first one
    /// arrives.
    ///
    /// The DISPLAYED frame, not the newest one fetched: an analyst who has
    /// scrubbed the timeline back is looking at an older picture and the number
    /// has to describe that picture.
    fn displayed_frame_age(&self, now: DateTime<Utc>) -> Option<TimeDelta> {
        self.history
            .current()
            .map(|frame| data_source::volume_age_at(frame.identity.volume_time, now))
    }

    /// Whether the live feed has stopped keeping up with wall clock at `now`.
    ///
    /// Judged here as well as in the poll thread, and the redundancy is the
    /// point. `live_service` classifies once per listing and publishes only on
    /// change, so every path that stops the listing - the network drops, the
    /// bucket answers 500, the poll thread dies - freezes the last verdict the
    /// app heard. If that verdict was `Current`, the instrument goes back to
    /// implying a picture is live while it ages, which is the exact silence
    /// this work exists to remove. Nothing has to ARRIVE for time to pass, so
    /// the age is recomputed here from the feed's own newest volume time and
    /// the two verdicts are OR-ed: whichever says stalled, wins.
    ///
    /// `data_source::classify_feed_age` is still the one definition of "too
    /// old" in the workspace. This is that rule read against a fresher clock.
    fn live_feed_stalled(&self, now: DateTime<Utc>) -> bool {
        self.live_feed.as_ref().is_some_and(|feed| {
            feed.freshness.is_stalled()
                || data_source::classify_feed_age(data_source::volume_age_at(
                    feed.newest_volume_time,
                    now,
                ))
                .is_stalled()
        })
    }

    /// Whether the picture is coming from the archive bucket rather than the
    /// chunk feed. `live_feed_stalled` decides that a notice is raised at all;
    /// this decides which words it uses, so the fallback is never announced
    /// with "stalled" over a forty-second-old volume.
    fn live_feed_archive_fallback(&self) -> bool {
        self.live_feed
            .as_ref()
            .is_some_and(|feed| feed.freshness.is_archive_fallback())
    }

    /// The loud line, or `None` when the feed is keeping up.
    ///
    /// Says the site, says the word, and says how old - because "stalled" alone
    /// leaves an analyst to guess whether that means a missed volume or a
    /// missed weekend, and on 2026-08-19 it meant a weekend.
    fn live_stall_notice(&self, now: DateTime<Utc>) -> Option<String> {
        if !self.live_feed_stalled(now) {
            return None;
        }
        let feed = self.live_feed.as_ref()?;
        let age = data_source::volume_age_at(feed.newest_volume_time, now);
        // "archive fallback" is the enum's own label; "feed stalled" is not
        // read from it, because the clock-read OR in `live_feed_stalled` can
        // raise this notice while `freshness` still says Current, and the
        // words must not follow the enum into calling that "live".
        let words = if feed.freshness.is_archive_fallback() {
            feed.freshness.status_label()
        } else {
            "feed stalled"
        };
        Some(format!(
            "{} {words} · newest data {} old",
            feed.site,
            format_age(age)
        ))
    }

    /// The detail behind the stall banner: the exact time the feed stopped at,
    /// what is still true about the picture, and what to do about it.
    fn live_stall_hover(&self) -> String {
        let Some(feed) = self.live_feed.as_ref() else {
            return String::new();
        };
        let when = self.settings_cache.units.time(feed.newest_volume_time);
        // Two different situations, two different sets of advice. On the
        // fallback the picture is being kept current from the other bucket,
        // so "pick another radar" would be telling the analyst to walk away
        // from data that is fine.
        if feed.freshness.is_archive_fallback() {
            return format!(
                "The realtime chunks feed for {} has stopped publishing, so this \
                 session is polling the Level II archive bucket instead. The newest \
                 archive volume starts at {when}, and new ones keep arriving - whole \
                 volumes, roughly one scan behind a healthy chunk feed. The session \
                 returns to the chunk feed by itself the moment it leads again.",
                feed.site
            );
        }
        format!(
            "The newest volume anywhere in the realtime chunks feed for {} starts at {when}. \
             Nothing newer exists to fetch, so the panes are still drawing that volume - \
             real data, old data. Warning polygons are current and will not line up with it. \
             Pick another radar, or load this one from the archive.",
            feed.site
        )
    }

    /// The right-hand side of a pane header.
    ///
    /// Age before render time, because the analyst's question is how old the
    /// picture is and the millisecond figure is a developer's. `STALLED` leads
    /// when it applies: a header that reads "3 d old" on its own can be a
    /// deliberate archive load, and this is the word that says it is not.
    /// What limits this pane's picture, most important first.
    ///
    /// Its own function rather than a block inside `canvas`, because it is the
    /// on-glass half of the stall report and a thing no test could reach while
    /// it lived inside a paint loop. Only what is true right now; an empty list
    /// is the common case and draws nothing.
    fn pane_badges(&self, product: DisplayProduct, now: DateTime<Utc>) -> Vec<String> {
        let mut badges: Vec<String> = Vec::new();
        // Ahead of everything, including the stall badge. Every other badge in
        // this stack qualifies a number the RADAR reported - it is old, it is
        // partial, some of it is hidden. This one says the numbers are not the
        // radar's at all: a Level 1 record carries pulses, and the field on
        // screen is the one this application estimated from them under the
        // settings currently in force. An analyst who cannot tell a computed
        // field from a delivered one cannot tell which of the two they are
        // about to quote.
        if self.iq.is_some() {
            // TWO badges, not one line. The legend stacks badges on their own
            // rows and wraps within a row, so a single string is at the mercy
            // of the column width: "LEVEL 1 · MOMENTS COMPUTED HERE" was cut to
            // "MOMENTS COMPUT…" at 100 %, and "LEVEL 1 · COMPUTED" broke as
            // "COMPUTE / D" at 160 % - a badge about honesty, snapped in half.
            // Two short words each fit their own row at every scale the
            // application offers, which was checked by photographing them.
            //
            // The whole sentence is on the pane header, which has the room. A
            // badge's job is to catch the eye already reading the colour ladder
            // and send it there.
            badges.push("LEVEL 1".to_owned());
            badges.push("COMPUTED".to_owned());
        }
        // First in the stack, ahead of PARTIAL and the hail environment: the
        // badge list is truncated to `legend::MAX_BADGES`, and nothing else a
        // pane can say outranks "what you are looking at is not now". This puts
        // it on the glass beside the legend, where the analyst is already
        // looking - the toolbar banner is the loud copy, this is the one over
        // the storm that is not there.
        if self.live_feed_stalled(now) {
            let words = if self.live_feed_archive_fallback() {
                "ARCHIVE FALLBACK"
            } else {
                "FEED STALLED"
            };
            badges.push(match self.displayed_frame_age(now) {
                Some(age) => format!("{words} · {} OLD", format_age(age)),
                None => words.to_owned(),
            });
        }
        // Second, directly under the stall badge and ahead of PARTIAL: "what
        // you are looking at is not all of it" is the same class of claim as
        // "what you are looking at is not now", and the stack is truncated to
        // `legend::MAX_BADGES`. This is the legend's one-word copy of the
        // statement, for the eye already reading the colour ladder; the whole
        // sentence is on the pane header, which - unlike this stack - cannot
        // be switched off in Settings.
        if let Some(text) = crate::gate_filter_ui::pane_badge_text(&self.settings_cache.gate_filter)
        {
            badges.push(text);
        }
        if let Some(frame) = self.history.current()
            && frame.stage != FrameStage::Complete
        {
            badges.push(format!("{:?}", frame.stage).to_uppercase());
        }
        // A hail product computed from a guessed freezing level and one
        // computed from a sounding are different claims. Without this the two
        // look identical on screen, which is the whole reason the environment
        // carries its provenance around with it.
        if product
            .derived_volume()
            .is_some_and(product_engine::registry::DerivedVolumeId::needs_hail_environment)
        {
            badges.push(self.hail_environment.summary());
        }
        badges
    }

    /// Say so on the status line when the gate filter has just gone off.
    ///
    /// The removed FILTERED band confirmed its own click this way, and the
    /// confirmation is worth more now than it was then: the escape has moved
    /// to a key on the toolbar, several inches from the panes whose pictures
    /// it changes, so an analyst who hits it is not necessarily looking at the
    /// thing that answers. Called after the cache has been recomputed, so it
    /// reads the state the click produced rather than the one before it.
    fn note_filter_cleared(&mut self) {
        if !self.settings_cache.gate_filter.is_active() {
            self.status = "Gate filter cleared - every gate is being drawn".to_owned();
        }
    }

    /// The right-hand end of a pane's header row.
    ///
    /// The filter statement comes FIRST, ahead of the stall word and the frame
    /// age, and that order is the safety rule rather than a preference. This
    /// end of the header truncates from the right with a visible ellipsis
    /// (`pane_canvas::header_galleys`), so on a quarter-pane in the smallest
    /// window whatever is built last is what an analyst does not get to read -
    /// and since the full-width FILTERED band was removed this row is the only
    /// place on the pane the whole statement lives. The stall word and the age
    /// each have a legend badge and the timeline behind them; the filter
    /// statement has one word beside the colour bar.
    ///
    /// Two versions of that statement exist and this picks between them by
    /// asking whether they agree. The engine's
    /// [`PaneRuntime::filter_line`] carries the counts and is the one an
    /// analyst wants, but it describes the filter the worker ran, which is not
    /// the filter in force during the frames between a criterion moving and
    /// the new render landing. `gate_filter_ui::pane_status_line` is built
    /// from the settings this frame is being drawn under and is always current
    /// but has no counts. The engine's line is a strict PREFIX of it when the
    /// two describe the same criteria - pinned in `gate_filter_ui` - so a
    /// prefix test is exactly "does the landed report still describe what is
    /// switched on", and a stale one falls back rather than printing another
    /// filter's numbers.
    fn pane_header_status(
        &self,
        pane: PaneId,
        product: DisplayProduct,
        now: DateTime<Utc>,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();
        // Built from the settings cache, not from the render, so a pane that
        // is still rendering, failed, has no frame yet or carries a product
        // the filter cannot run against still says that gates are being
        // hidden. That coverage was the removed band's, and it is inherited
        // here whole.
        if let Some(current) = crate::gate_filter_ui::pane_status_line(
            &self.settings_cache.gate_filter,
            product
                .derived_volume()
                .is_some()
                .then_some(crate::render_service::DERIVED_PRODUCT_NOT_FILTERED),
        ) {
            parts.push(
                self.panes[pane.index()]
                    .filter_line
                    .as_ref()
                    .filter(|landed| landed.starts_with(&current))
                    .cloned()
                    .unwrap_or(current),
            );
        }
        // The badge stack can be switched off with the legend; this row cannot,
        // which is why the whole sentence goes here and the badge carries only
        // the headline. It names the dwell, the window and the censor because
        // on Level 1 those are part of the measurement: the same pulses under a
        // different dwell are a different field, and two screenshots have no
        // other way of telling an analyst which is which.
        if let Some(session) = self.iq.as_ref() {
            // Before the provenance, because it is the stronger caveat: it
            // says what the picture below it now is.
            //
            // An RVP8 time-series header carries a signal-processor name and no
            // coordinates, so the position comes from a catalog: the sourced
            // research table in the binary, or the station directory, which is
            // fetched over the network and cached and is therefore simply
            // absent on a cold machine that is offline. A record from a radar
            // in neither is never placed at all. The sweep is
            // still drawn, because the ranges and azimuths are real and are
            // what an analyst came for; the geography is not, and `pane_map`
            // withholds it rather than anchoring it wherever the map happened
            // to be left. This row is what tells the analyst that the empty
            // ground under the sweep is an absence and not a basemap that
            // failed to load.
            if !self.frame_position_is_known() {
                // Short on purpose. This row truncates from the RIGHT, and the
                // provenance behind it is not decoration - it is what the field
                // was made with. The first draft named the site and explained
                // itself in a clause, and pushed "gates below 2.0 dB SNR left
                // blank" off the end of the row. The site id is already on the
                // timeline bar; what this has to say is what the pane is now
                // showing, which is range and azimuth from an antenna and
                // nothing about where on earth that antenna stood.
                parts.push("POSITION UNKNOWN - radar-local kilometres only".to_owned());
            }
            parts.push(session.provenance());
        }
        if self.live_feed_stalled(now) {
            parts.push(
                if self.live_feed_archive_fallback() {
                    "ARCHIVE FALLBACK"
                } else {
                    "STALLED"
                }
                .to_owned(),
            );
        }
        // The age stays for a time series too. It is a true statement about the
        // frame and the same one the timeline bar makes, and suppressing it
        // here would leave the two rows disagreeing about the same file. An age
        // is not a claim about a feed; the words that WOULD be - STALLED,
        // ARCHIVE FALLBACK - are above, and a record with no feed behind it
        // never reaches them.
        if let Some(age) = self.displayed_frame_age(now) {
            parts.push(format!("{} old", format_age(age)));
        }
        let render = self.panes[pane.index()].status.as_str();
        if !render.is_empty() {
            parts.push(render.to_owned());
        }
        parts.join(" · ")
    }

    fn timeline_status(&self, now: DateTime<Utc>) -> String {
        let Some(frame) = self.history.current() else {
            // Nothing on screen yet, so there is no frame age to show - but a
            // session pointed at a dead prefix must not sit here reading
            // "Live KUEX" while the first stale volume downloads. The feed
            // report arrives before the transfer starts precisely so that this
            // window is covered.
            return self
                .live_stall_notice(now)
                .unwrap_or_else(|| self.status.clone());
        };
        let index = self.history.selected_index().unwrap_or(0) + 1;
        // The age rides directly behind the Z time it is computed from, so the
        // two can never be read as separate claims about different frames.
        format!(
            "{} · {}/{} · {:?} · {} · {} old",
            frame.identity.site_id,
            index,
            self.history.len(),
            frame.stage,
            self.settings_cache.units.time(frame.identity.volume_time),
            format_age(data_source::volume_age_at(frame.identity.volume_time, now))
        )
    }

    /// Whether every visible pane has said its last word about the current
    /// stamp: an installed exact-stamp texture, or a terminal answer that no
    /// picture is coming. Treating Failed/Unavailable as ready is what lets
    /// playback step past a frame one pane cannot draw instead of freezing on
    /// it for ever at a 60 Hz repaint spin.
    fn visible_panes_ready(&self) -> bool {
        self.workspace.visible_panes().iter().all(|pane| {
            let runtime = &self.panes[pane.index()];
            let current = self.current_stamp(*pane);
            let rendered = runtime
                .texture
                .as_ref()
                .is_some_and(|texture| texture.stamp == current);
            let terminal = runtime
                .terminal
                .is_some_and(|terminal| terminal.stamp() == current);
            runtime.pending_stamp.is_none() && (rendered || terminal)
        })
    }
}

impl eframe::App for WorkstationApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The ground, before anything else allocates. eframe's root `Ui`
        // "has no margin or background color" (eframe 0.34.3, epi.rs) and the
        // window is cleared to eframe's own near-black default, so without
        // this the whole instrument floats on raw black: the light variant's
        // near-black ink - product name, tilt value, live status - is
        // invisible on it until a hover happens to paint a face underneath.
        // `clear_color` below is the other half, for the strip a resize
        // exposes before layout catches up.
        crate::theme::paint_root_ground(ui);
        let context = ui.ctx().clone();
        self.current_view_export.poll();
        self.current_view_export.handle_capture_events(&context);
        self.handle_loop_capture_events(&context);
        self.handle_dropped_files(&context);
        // A save made in the colour table editor, one frame ago.
        //
        // Deferred by a frame rather than done where it is noticed, because
        // the editor's window is drawn BEFORE the store mirror below: an
        // Apply in the same press installs a table that the store has not
        // been told about yet, and re-resolving the stored choices there
        // would put the previous palette straight back on the pane. By the
        // top of the next frame the mirror has run, the stored name is the
        // installed table's, and re-resolving finds the file that was just
        // written.
        if std::mem::take(&mut self.user_tables_rescan_pending) {
            self.rescan_user_tables();
        }
        // The analyst edits a palette by alt-tabbing to a text editor and
        // coming back; the way back is when the folder is worth reading
        // again. No polling, no watcher thread.
        if self.user_tables.poll_focus(&context) {
            self.reresolve_palettes_from_user_tables();
        }
        self.poll_playlist_preflight();
        self.poll_live_results();
        self.poll_load_results();
        self.poll_site_directory();
        self.poll_warnings();
        self.surface_observations.poll();
        if self.settings_cache.placefiles_enabled || self.placefiles_window_open {
            let frame_time = self.history.current().map(|frame| frame.volume.volume_time);
            self.placefiles.set_reference_time(frame_time);
            if self.placefiles.poll(&context) {
                context.request_repaint();
            }
        }
        // Before anything asks which sweep to draw.
        self.refresh_capabilities();
        self.follow_live_low_tilts();
        self.map_scene
            .set_pixels_per_point(context.pixels_per_point());
        self.map_scene.poll();
        self.refresh_placed_sites();
        self.refresh_placed_hazards();
        self.poll_render_results(&context);
        // After the results, before the canvas asks for the next render:
        // the reveal only steps for panes whose previous render has landed.
        self.advance_sweeps();
        self.advance_playback(&context);

        self.vol3d_window(&context);
        self.xsection_window(&context);
        self.palette_editor_window(&context);
        self.surface_observations.history_window(&context);
        self.placefiles_window(&context);
        self.placefile_browser_window(&context);
        self.file_browser_window(&context);
        self.playlist_preflight_window(&context);
        if let Some(path) = self.online_data.draw(&context) {
            self.source_path_text = path.display().to_string();
            self.begin_load(path);
        }
        self.toolbar(ui);
        // No separator under the bar: the band paints its own raised bevel,
        // and a stock hairline immediately below it reads as a second, weaker
        // edge drawn by someone who could not see the first.
        ui.add_space(2.0);

        let available = ui.available_size();
        let canvas_height = (available.y - TIMELINE_HEIGHT).max(120.0);
        // Never wider than the window, whatever the bar above did.
        //
        // `Ui::allocate_space` expands its parent's `max_rect` to contain a
        // child that overflows, and the Everything bar can overflow: it is a
        // wrapped row of stock widgets whose widths follow their contents, and
        // a latched "Filter: Storm mode" chip is wider than "Filter: off". So
        // `available_size().x` can report a width larger than the viewport,
        // and a canvas allocated at it puts every pane's RIGHT-aligned
        // furniture past the window edge.
        //
        // That furniture includes the legend's FILTERED badge. Measured at
        // 1408 points with the full bar in Storm mode, the pane ran about 18
        // points over: the colour ramp was off screen entirely and the badge
        // was clipped to "FIL". An indicator that exists to say data is being
        // hidden must not itself be pushed out of sight, so the canvas is
        // clamped to the distance from here to the window's right edge and
        // the bar is left to be the thing that clips.
        let to_window_edge = (context.content_rect().right() - ui.cursor().left()).max(1.0);
        let canvas_width = available.x.min(to_window_edge).max(1.0);
        let (canvas_rect, _) = ui.allocate_exact_size(
            egui::vec2(canvas_width, canvas_height),
            egui::Sense::hover(),
        );
        self.canvas(ui, canvas_rect);

        ui.separator();
        self.timeline(ui, &context);
        self.settings_frame(&context);
        // Last, over everything: what the analyst's last colour table drop
        // did. It floats rather than joining the timeline's status line
        // because that line is only shown when no volume is loaded, which is
        // never the moment somebody drops a palette.
        self.user_tables.draw_notice(&context);
        // Screenshot commands issued here observe the finished composited
        // window, never the stale texture that preceded this canvas paint.
        self.drive_loop_export_capture(&context);

        // A reveal that has caught up with the data is not animating: it is
        // waiting for a chunk, and the load service wakes the UI when one
        // lands. Repainting anyway would spin at 60 Hz over a picture that
        // cannot change.
        let animating = self.panes.iter().any(|pane| {
            pane.pending_stamp.is_some()
                || pane
                    .sweep_state
                    .is_some_and(|state| !state.complete && state.pending_deg() > 0.0)
        });
        if animating {
            context.request_repaint_after(Duration::from_millis(16));
        }
        // An age is a wall-clock quantity drawn on a screen that only repaints
        // when something wakes it, and between volumes nothing does: the live
        // poll asks for a repaint when a volume lands or the feed report
        // changes, and a stalled feed does neither for days. Without this the
        // readout an analyst is being asked to trust freezes at whatever it
        // said when the last thing happened - "0 s old" sitting on the glass
        // until the warnings poller's 45 s heartbeat happens to wake the
        // frame. So the app wakes itself exactly when the string it just drew
        // would next change, and no more often than that.
        let now = Utc::now();
        if let Some(age) = self.displayed_frame_age(now).or_else(|| {
            self.live_feed
                .as_ref()
                .map(|feed| data_source::volume_age_at(feed.newest_volume_time, now))
        }) {
            context.request_repaint_after(age_repaint_interval(age));
        }
    }

    /// The window's own clear colour, matched to the ground `ui` paints.
    ///
    /// eframe's default is a near-black `rgba(12, 12, 12, 180)` that belongs
    /// to no theme; left in place it is what the compositor shows in the
    /// strip a window drag exposes, and what the very first frame shows
    /// before any layout has happened - a black seam on every resize under
    /// an instrument-grey app.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        crate::theme::clear_color(visuals)
    }

    fn on_exit(&mut self) {
        // The debounce does not get a last word on shutdown; this does. An
        // unreadable file is the one exception: autosave is disabled for it
        // so the evidence is not overwritten, and quitting is not someone
        // deciding to overwrite it either.
        if !matches!(
            self.settings_store.status(),
            settings::LoadStatus::Unreadable { .. }
        ) {
            let _ = self.settings_store.save_now();
        }
    }
}

/// The rotation a pane will be DRAWN with once a site change has moved it,
/// for the pane rectangle `decide_across_site_change` measures.
///
/// A free function with a name because the frame it works in is the whole
/// point. `WorkspaceState::apply_site_change` hands its rotation closure a
/// centre it has already taken out through longitude and latitude on the old
/// anchor and back in on the new one, so what arrives is a GROUND point in the
/// new radar's frame, while a camera centre past the globe's blend start is a
/// WARPED one. An earlier version of the rule unwarped whatever it was given,
/// so handing it a ground point unwarped it twice and answered about somewhere
/// else - worth up to 1.83 degrees at a half-formed globe.
///
/// The two frames are now the same frame wherever the rule answers at all:
/// `RadarProjection::view_rotation_rad` is confined to scales finer than
/// `globe::MIN_BLEND_KM_PER_POINT`, where `globe::blend_for_pane` is an exact
/// zero and `globe::warp_world` is the identity on the bit pattern. So the
/// mistake is not fixed here, it is unrepresentable: the rule has no blend
/// parameter to be given the wrong point for. This function stays because the
/// caller's `world` really is a ground point and a reader should be told so.
/// The Level 1 estimator settings a store holds.
///
/// Read straight from the store rather than from [`SettingsCache`], because the
/// settings window's dispatch runs `apply_changed_setting` BEFORE
/// `recompute_settings_cache`: an apply that read the cache would re-estimate
/// the sweep with the settings from before the analyst moved the slider, and
/// the field would lag one change behind the page describing it. Sharing this
/// function between the cache and the apply is what makes that impossible
/// rather than merely fixed once.
fn iq_controls_from(
    registry: &settings::SettingsRegistry,
    store: &settings::SettingsStore,
) -> crate::iq_session::IqControls {
    use crate::settings_ui::catalog::keys;
    crate::iq_session::IqControls {
        // The store has already clamped this into the catalog's declared range;
        // `max(1)` is only about the `i64 as usize` cast, which would turn a
        // negative into an enormous positive and ask the estimator for a dwell
        // longer than any record.
        dwell_pulses: store
            .effective_int(
                registry,
                keys::timeseries::CATEGORY,
                keys::timeseries::DWELL_PULSES,
            )
            .max(1) as usize,
        taper: iq_taper_from_id(&store.effective_text(
            registry,
            keys::timeseries::CATEGORY,
            keys::timeseries::WINDOW,
        )),
        censor: iq_censor_from_db(store.effective_float(
            registry,
            keys::timeseries::CATEGORY,
            keys::timeseries::SNR_MIN_DB,
        )),
    }
}

/// The window a stored id names.
///
/// Total, like every other settings resolution: the store has already checked
/// the id against the declared options, and a value it could not check resolves
/// to the estimator's own default rather than taking the page down. See
/// `nexrad_io::iq_moments::taper::Taper`.
pub(crate) fn iq_taper_from_id(id: &str) -> nexrad_io::iq_moments::taper::Taper {
    use crate::settings_ui::catalog::timeseries_limits as limit;
    use nexrad_io::iq_moments::taper::Taper;
    match id {
        limit::WINDOW_VON_HANN => Taper::VonHann,
        limit::WINDOW_HAMMING => Taper::Hamming,
        limit::WINDOW_BLACKMAN => Taper::Blackman,
        _ => Taper::Rectangular,
    }
}

/// The censor a stored dB reading means.
///
/// The leftmost stop of the slider means *off* - no threshold at all - and the
/// comparison is against the declared floor rather than within a tolerance, on
/// the same principle the gate filter's four criteria are read with: a number
/// that is NEARLY the floor is still a threshold that is on, and a field
/// reporting "no threshold" while hiding gates is the one failure this whole
/// admission exists to prevent.
pub(crate) fn iq_censor_from_db(db: f64) -> nexrad_io::iq_moments::estimator::SnrCensor {
    use crate::settings_ui::catalog::timeseries_limits as limit;
    use nexrad_io::iq_moments::estimator::SnrCensor;
    if db <= limit::OFF_SNR_DB {
        SnrCensor::Off
    } else {
        SnrCensor::MinDb(db as f32)
    }
}

/// Why a gate has no spectrum to show, or `None` when it has one.
///
/// `blank` is the PANE's answer for that pixel - `None` when the pane drew a
/// number there, otherwise the kind of nothing it drew - and `estimate` is what
/// the pulses produced. Both are consulted, in that order, because they can
/// answer for different reasons and the pane's answer is the one the analyst is
/// looking at:
///
/// * a gate the pane's own gate filter removed is not on screen, so a plot of
///   it would be a picture of something the pane deliberately left empty;
/// * a gate whose `R(0)` never exceeded the receiver noise has no spectrum -
///   transforming it would draw the receiver, which is what this panel exists
///   not to do;
/// * a gate the SNR censor hid is the case the censor knob is FOR, and saying
///   which threshold hid it is the answer to what the analyst is asking.
///
/// A blank pixel the estimator cannot account for still gets a sentence rather
/// than a plot: an empty pixel with a full spectrum drawn beside it is two
/// statements about one gate that disagree.
fn gate_spectrum_absence(
    blank: Option<product_engine::stats::CellState>,
    estimate: Option<&nexrad_io::iq_moments::estimator::GateEstimate>,
) -> Option<String> {
    use product_engine::stats::CellState;
    if blank == Some(CellState::QualityMasked) {
        return Some("hidden by the pane's gate filter".to_owned());
    }
    if let Some(estimate) = estimate {
        if estimate.below_noise {
            return Some("no power above the receiver noise".to_owned());
        }
        if estimate.censored {
            return Some("below the SNR threshold".to_owned());
        }
    }
    blank.map(|state| state.label().to_ascii_lowercase())
}

fn site_change_display_rotation(
    projection: &map_scene::RadarProjection,
    ground_centre: analyst_runtime::WorldPoint,
    km_per_point: f32,
) -> f32 {
    projection.view_rotation_rad(ground_centre, km_per_point)
}

#[cfg(test)]
mod tests {
    /// The catalog declares these choices as plain strings, because it is
    /// also compiled by the `settings` crate's own preview harness and cannot
    /// see this module. This is the seam where the two halves are checked
    /// against each other: every option the menu offers must resolve to an
    /// enum here, every enum must be offered, and the declared default must be
    /// the shipped unit.
    #[test]
    fn every_menu_option_matches_an_enum_and_the_defaults_agree() {
        use crate::settings_ui::catalog::{keys, registry};
        let registry = registry();
        let category = keys::units::CATEGORY;

        let offered = |id: &str| -> (Vec<String>, String) {
            let spec = registry
                .setting(category, id)
                .unwrap_or_else(|| panic!("the catalog declares units/{id}"));
            match &spec.kind {
                settings::SettingKind::Choice {
                    options,
                    default_id,
                } => (
                    options.iter().map(|option| option.id.clone()).collect(),
                    default_id.clone(),
                ),
                other => panic!("units/{id} is {other:?}, not a choice"),
            }
        };

        let (ids, default) = offered(keys::units::DISTANCE);
        assert_eq!(
            ids,
            crate::units::DistanceUnit::ALL
                .iter()
                .map(|unit| unit.id().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            crate::units::DistanceUnit::from_id(&default),
            crate::units::DistanceUnit::default()
        );

        let (ids, default) = offered(keys::units::ALTITUDE);
        assert_eq!(
            ids,
            crate::units::AltitudeUnit::ALL
                .iter()
                .map(|unit| unit.id().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            crate::units::AltitudeUnit::from_id(&default),
            crate::units::AltitudeUnit::default()
        );

        let (ids, default) = offered(keys::units::TIME_ZONE);
        assert_eq!(
            ids,
            crate::units::TimeZoneChoice::ALL
                .iter()
                .map(|zone| zone.id().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            crate::units::TimeZoneChoice::from_id(&default),
            crate::units::TimeZoneChoice::default()
        );

        let (ids, default) = offered(keys::units::CLOCK);
        assert_eq!(
            ids,
            crate::units::ClockFormat::ALL
                .iter()
                .map(|clock| clock.id().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            crate::units::ClockFormat::from_id(&default),
            crate::units::ClockFormat::default()
        );
    }

    /// The two rows this repair added, checked against the code that reads
    /// them: the menu must not offer a slice top the sampler would clamp, and
    /// both defaults must be the constants they replaced.
    ///
    /// Same seam and same reason as the units test above. The catalog is also
    /// compiled by the `settings` crate's preview harness, which cannot see
    /// this module, so the two halves are declared apart and pinned here.
    #[test]
    fn the_new_rows_agree_with_the_code_that_reads_them() {
        use crate::settings_ui::catalog::{keys, registry};
        let registry = registry();
        let integer = |category: &str, id: &str| -> (i64, i64, i64) {
            let spec = registry
                .setting(category, id)
                .unwrap_or_else(|| panic!("the catalog declares {category}/{id}"));
            match &spec.kind {
                settings::SettingKind::Integer {
                    min, max, default, ..
                } => (*min, *max, *default),
                other => panic!("{category}/{id} is {other:?}, not an integer"),
            }
        };

        // Cross-section > Top of the slice. Declared in kilometres, fenced in
        // metres: the menu's own ends must survive the fence untouched, or the
        // window would silently draw something other than what the row says.
        let (min, max, default) = integer(keys::xsection::CATEGORY, keys::xsection::TOP_KM);
        assert_eq!(
            (min * 1_000) as f32,
            crate::xsection::MIN_TOP_M,
            "the menu's floor is the sampler's floor"
        );
        assert_eq!(
            (max * 1_000) as f32,
            crate::xsection::MAX_TOP_M,
            "the menu's ceiling is the sampler's ceiling"
        );
        assert_eq!(
            (default * 1_000) as f32,
            crate::xsection::DEFAULT_TOP_M,
            "the default is the 18 km slice the window always drew"
        );

        // Data > Loop frame time. The default is the constant the loop ran at
        // before it was settable.
        let (min, max, default) = integer(keys::data::CATEGORY, keys::data::LOOP_FRAME_MS);
        assert_eq!(
            Duration::from_millis(default as u64),
            PLAYBACK_FRAME_TIME,
            "the default is the 700 ms step the loop always took"
        );
        assert!(min > 0, "a zero-length frame time is a 60 Hz spin");
        assert!(min <= 700 && max >= 700);

        // And a fresh session - no settings file - resolves to exactly those
        // two constants, so nothing about the shipped behaviour moved.
        let app = test_app();
        assert_eq!(
            app.settings_cache.xsection_top_m,
            crate::xsection::DEFAULT_TOP_M
        );
        assert_eq!(app.settings_cache.loop_frame_time, PLAYBACK_FRAME_TIME);
    }

    /// The Vrot report line follows the analyst's units, on BOTH of the two
    /// paths through it.
    ///
    /// `vrot::report` has its own test; this is the mirrored path in
    /// `vrot_report_line`, which rebuilds the same sentence when
    /// `analysis/vrot_units` puts m/s first. It was the copy that was missed:
    /// a session in miles and feet read its pane corner converted and its Vrot
    /// report in kilometres.
    #[test]
    fn the_vrot_report_line_follows_the_units_on_both_paths() {
        let crate::vrot::VrotState::Complete(measurement) = completed_vrot() else {
            panic!("completed_vrot returns a finished measurement");
        };
        let mut app = test_app();

        // Default units, both orders: character-for-character what shipped.
        assert!(
            app.vrot_report_line(&measurement)
                .contains("separation 0.80 km | height 0.30 km ARL"),
            "{}",
            app.vrot_report_line(&measurement)
        );
        app.settings_cache.vrot_mps_first = true;
        assert!(
            app.vrot_report_line(&measurement)
                .contains("separation 0.80 km | height 0.30 km ARL"),
            "{}",
            app.vrot_report_line(&measurement)
        );

        // Miles and feet. 0.80 km is 0.50 mi; 300 m is 984 ft.
        app.settings_cache.units = crate::units::UnitSystem {
            distance: crate::units::DistanceUnit::StatuteMiles,
            altitude: crate::units::AltitudeUnit::Feet,
            ..crate::units::UnitSystem::default()
        };
        for mps_first in [false, true] {
            app.settings_cache.vrot_mps_first = mps_first;
            let line = app.vrot_report_line(&measurement);
            assert!(
                line.contains("separation 0.50 mi | height 984 ft ARL"),
                "mps_first={mps_first}: {line}"
            );
            assert!(
                !line.contains(" km"),
                "mps_first={mps_first}: a kilometre survived: {line}"
            );
            // The measurement itself is unmoved.
            assert!(line.contains("delta-V 60.0 m/s"), "{line}");
        }
    }

    use super::*;

    #[test]
    fn viewport_change_ignores_subpixel_layout_noise() {
        let original = ViewportMetrics {
            width_points: 800.0,
            height_points: 600.0,
            pixels_per_point: 1.5,
        };
        assert!(!viewport_changed(
            original,
            ViewportMetrics {
                width_points: 800.2,
                height_points: 599.8,
                pixels_per_point: 1.5,
            }
        ));
        assert!(viewport_changed(
            original,
            ViewportMetrics {
                width_points: 801.0,
                ..original
            }
        ));
    }

    fn first_pane() -> PaneId {
        PaneId::new(0).expect("pane 0 always exists")
    }

    #[test]
    fn a_pane_header_names_the_unit_its_readout_will_be_in() {
        assert_eq!(
            pane_title(None, first_pane(), DisplayProduct::Reflectivity, None),
            "1 · REF (dBZ)"
        );
    }

    #[test]
    fn a_pane_header_distinguishes_the_two_velocity_style_units() {
        // Velocity reads in knots and spectrum width in metres per second, and
        // the header is where an analyst finds that out before misreading a
        // threshold quoted in the other one.
        assert_eq!(
            pane_title(None, first_pane(), DisplayProduct::DealiasedVelocity, None),
            "1 · DVEL (kt)"
        );
        assert_eq!(
            pane_title(None, first_pane(), DisplayProduct::SpectrumWidth, None),
            "1 · SW (m/s)"
        );
    }

    #[test]
    fn a_dimensionless_product_header_carries_no_empty_parentheses() {
        assert_eq!(
            pane_title(
                None,
                first_pane(),
                DisplayProduct::CorrelationCoefficient,
                None
            ),
            "1 · RHO"
        );
    }

    // --- behavioural tests: the real application, headless -------------------
    //
    // These construct the real `WorkstationApp` - real load, render and live
    // workers, real clocks - against a bare `egui::Context`, and drive the
    // exact decision points `canvas`/`timeline`/`update` drive. Most of them
    // stop at that decision point rather than driving a whole
    // `eframe::App::ui` pass, because a pinned decision is a sharper failure
    // message than a pinned frame.
    //
    // One of them does drive the full pass - `the_application_paints_its_own_
    // ground_and_clears_to_it` - because the thing it guards is the pass
    // itself. `eframe::Frame::_new_kittest` makes that constructible outside
    // eframe, and a headless pass starts no basemap tile fetches: the tile
    // store's provider is `None` until an operator picks one from the Map
    // menu, and a test settings store has nobody's pick in it.

    const VIEWPORT: ViewportMetrics = ViewportMetrics {
        width_points: 240.0,
        height_points: 160.0,
        pixels_per_point: 1.0,
    };

    /// A SITE CHANGE MEASURES ITS PANE RECTANGLE IN THE FRAME IT WAS HANDED,
    /// AND INSIDE THE DOMAIN THERE IS ONLY THE ONE FRAME.
    ///
    /// `WorkspaceState::apply_site_change` reprojects each pane centre through
    /// longitude and latitude, so the point it gives its rotation closure is a
    /// GROUND point in the new radar's frame. Past the globe's blend start a
    /// camera centre is a WARPED point, and a rule that unwarps what it is
    /// given used to unwarp this one a second time and answer about somewhere
    /// else, up to 1.83 degrees of it.
    ///
    /// Three halves, and each is what makes the next worth something: the
    /// helper is the rule itself with no extra step; wherever the rule
    /// answers, the warped point IS the ground point on the bit pattern, so
    /// there is no second frame to be in the wrong one of; and where the two
    /// frames really do differ - a half-formed globe, where the gap is tens of
    /// kilometres - the rule declines from both of them.
    #[test]
    fn a_site_change_asks_about_the_ground_point_it_was_given() {
        let viewport = ViewportMetrics {
            width_points: 1600.0,
            height_points: 900.0,
            pixels_per_point: 1.0,
        };
        // KRTX, the anchor in the complaint.
        let projection =
            map_scene::RadarProjection::new(45.714_968_872_070_31, -122.965_301_513_671_88);
        let grounds = [
            analyst_runtime::WorldPoint::new(1500.0, 900.0),
            analyst_runtime::WorldPoint::new(3000.0, -400.0),
            analyst_runtime::WorldPoint::new(-2500.0, 2500.0),
        ];
        let mut answered_somewhere = false;
        for km_per_point in [0.35_f32, 2.8, 4.9, 5.5, 6.5, 6.99] {
            let blend = map_scene::projection::globe::blend_for_pane(km_per_point, viewport);
            for ground in grounds {
                let used = super::site_change_display_rotation(&projection, ground, km_per_point);
                assert_eq!(
                    used.to_bits(),
                    projection.view_rotation_rad(ground, km_per_point).to_bits(),
                    "the site change is not asking the rule itself about {ground:?}"
                );
                if used != 0.0 {
                    answered_somewhere = true;
                    let warped = map_scene::projection::globe::warp_world(ground, blend)
                        .expect("inside the limb");
                    assert_eq!(
                        warped.east_km.to_bits(),
                        ground.east_km.to_bits(),
                        "at {km_per_point} km per point the camera frame is not the ground \
                         frame about {ground:?}"
                    );
                    assert_eq!(
                        warped.north_km.to_bits(),
                        ground.north_km.to_bits(),
                        "at {km_per_point} km per point the camera frame is not the ground \
                         frame about {ground:?}"
                    );
                }
            }
        }
        assert!(
            answered_somewhere,
            "the rule answered zero everywhere, so this proof is not measuring what it claims"
        );
        // A half-formed globe, where a ground point and a camera centre really
        // are different places: the rule declines from either one.
        let outside = 11.0_f32;
        let blend = map_scene::projection::globe::blend_for_pane(outside, viewport);
        assert!(blend > 0.0 && blend < 1.0, "pick a scale inside the band");
        let mut worst_frame_gap_km = 0.0f64;
        for ground in grounds {
            let warped =
                map_scene::projection::globe::warp_world(ground, blend).expect("inside the limb");
            worst_frame_gap_km = worst_frame_gap_km
                .max((warped.east_km - ground.east_km).hypot(warped.north_km - ground.north_km));
            for centre in [ground, warped] {
                assert_eq!(
                    super::site_change_display_rotation(&projection, centre, outside).to_bits(),
                    0.0_f32.to_bits(),
                    "a site change turned a pane outside the domain, about {centre:?}"
                );
            }
        }
        assert!(
            worst_frame_gap_km > 50.0,
            "the two frames are only {worst_frame_gap_km:.1} km apart, so this proof is not \
             measuring what it claims"
        );
    }

    /// The derived north-up rotation reaches the pane, and does NOT reach the
    /// stored camera.
    ///
    /// Both halves matter. If it did not reach the pane the map would stay
    /// crooked; if it reached the stored camera it would compound with itself
    /// frame after frame, and it would be persisted as though the analyst had
    /// asked for it.
    #[test]
    fn the_north_up_rotation_is_derived_for_the_pane_and_never_written_back() {
        let mut app = test_app();
        // KRTX, the anchor in the complaint.
        assert!(
            app.map_scene
                .set_radar_anchor(45.714_968_872_070_31, -122.965_301_513_671_88)
        );
        let pane = PaneId::new(0).expect("pane 0");
        let viewport = ViewportMetrics {
            width_points: 1600.0,
            height_points: 900.0,
            pixels_per_point: 1.0,
        };

        // On the antenna: nothing at all, and by a bit pattern.
        app.workspace.pane_mut(pane).camera = Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 0.35,
            rotation_rad: 0.0,
        };
        let displayed = app.display_camera(pane, viewport);
        assert_eq!(displayed, app.workspace.pane(pane).camera);

        // On the eastern seaboard, 3911 km away: about a third of a radian.
        let projection = app.map_scene.projection().expect("the anchor is set");
        let centre = projection
            .try_lon_lat_to_world(-75.0, 40.0)
            .expect("40N 75W projects from KRTX");
        let stored = Camera2D {
            center_east_km: centre.east_km,
            center_north_km: centre.north_km,
            km_per_point: 1.0,
            rotation_rad: 0.0,
        };
        app.workspace.pane_mut(pane).camera = stored;
        let displayed = app.display_camera(pane, viewport);
        assert!(
            (displayed.rotation_rad.to_degrees() - 32.327_957).abs() < 1e-3,
            "the pane was drawn at {} deg",
            displayed.rotation_rad.to_degrees()
        );
        assert_eq!(displayed.center_east_km, stored.center_east_km);
        assert_eq!(displayed.km_per_point, stored.km_per_point);
        // The STORED camera is untouched: deriving it per frame is what keeps
        // `Camera2D` meaning the analyst's own intent.
        assert_eq!(app.workspace.pane(pane).camera, stored);
        assert_eq!(app.workspace.pane(pane).camera.rotation_rad, 0.0);

        // An analyst's own rotation is carried, not replaced.
        app.workspace.pane_mut(pane).camera = Camera2D {
            rotation_rad: 0.25,
            ..stored
        };
        let displayed = app.display_camera(pane, viewport);
        assert!(
            (displayed.rotation_rad - (0.25 + 0.564_226)).abs() < 1e-3,
            "the analyst's own rotation was lost: {}",
            displayed.rotation_rad
        );
    }

    fn test_app() -> WorkstationApp {
        WorkstationApp::with_context(
            egui::Context::default(),
            None,
            None,
            // Daemon-only against the local discard port: the warnings poller
            // fails instantly with connection-refused and never leaves the
            // machine, instead of hitting weather.gov from a unit test.
            WarningsSource::Daemon {
                base_url: "http://127.0.0.1:9".to_owned(),
            },
            test_settings_store(),
        )
    }

    /// The ground, pinned where it can actually go missing: in the
    /// application, not in the theme.
    ///
    /// eframe hands `App::ui` a root `Ui` that "has no margin or background
    /// color" (eframe 0.34.3, `epi.rs`) and clears the window to its own
    /// `rgba(12, 12, 12, 180)`, so the app has to paint its own face and
    /// return its own clear colour. `tests/theme_contract.rs` pins the two
    /// helpers that do it, but includes only `src/theme.rs`, so it cannot prove
    /// the application still calls them. The toolbar proof in
    /// `examples/theme_gallery.rs` paints its own ground as well. This test
    /// therefore drives the real `<WorkstationApp as eframe::App>` and
    /// verifies both application-level calls.
    ///
    /// Both halves are asserted the way eframe asks for them: the ground as
    /// a filled face rect that covers the viewport and is painted before any
    /// text, and the clear colour through `Context::global_style`, which is
    /// the accessor `wgpu_integration.rs` uses.
    #[test]
    fn the_application_paints_its_own_ground_and_clears_to_it() {
        for theme_ground in [crate::theme::Ground::Light, crate::theme::Ground::Dark] {
            let appearance = crate::theme::Appearance::on_ground(theme_ground);
            let palette = appearance.palette();
            let context = egui::Context::default();
            crate::theme::apply(&context, &appearance);
            let mut app = WorkstationApp::with_context(
                context.clone(),
                None,
                None,
                WarningsSource::Daemon {
                    base_url: "http://127.0.0.1:9".to_owned(),
                },
                test_settings_store(),
            );
            let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 800.0));
            let mut frame = eframe::Frame::_new_kittest();
            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ui| <WorkstationApp as eframe::App>::ui(&mut app, ui, &mut frame),
            );

            // Index, not identity: "a face rect covering the viewport exists,
            // and it is painted before the first glyph" is the property that
            // makes the chrome legible, and it survives egui gaining an
            // incidental leading shape in some later version.
            let ground = output.shapes.iter().position(|clipped| {
                matches!(&clipped.shape, egui::Shape::Rect(rect)
                    if rect.fill == palette.face && rect.rect.contains_rect(screen))
            });
            let ground = ground.unwrap_or_else(|| {
                panic!(
                    "{theme_ground:?}: no shape in the frame fills the whole viewport with the panel \
                     face - `WorkstationApp::ui` is not painting its ground, and every bare \
                     label on the bar is back on eframe's near-black"
                )
            });
            let first_text = output
                .shapes
                .iter()
                .position(|clipped| matches!(&clipped.shape, egui::Shape::Text(_)));
            if let Some(first_text) = first_text {
                assert!(
                    ground < first_text,
                    "{theme_ground:?}: the ground is painted at shape {ground}, after the first text \
                     run at {first_text} - it would cover the chrome instead of backing it"
                );
            }

            // Exactly how `eframe::native::wgpu_integration` asks for it.
            let clear =
                <WorkstationApp as eframe::App>::clear_color(&app, &context.global_style().visuals);
            assert_eq!(
                clear,
                palette.face.to_opaque().to_normalized_gamma_f32(),
                "{theme_ground:?}: the window clear colour is not the ground the app paints - every \
                 resize tears a seam of eframe's near-black default"
            );
            assert_eq!(
                clear[3], 1.0,
                "{theme_ground:?}: a see-through clear colour lets the desktop through"
            );
        }
    }

    /// A settings store at a path that never exists: every value a default,
    /// nothing to resume, and nothing here ever saves - so a unit test can
    /// neither read the user's real settings file nor be steered by a
    /// leftover one from an earlier run.
    fn test_settings_store() -> settings::SettingsStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "radar-workstation-test-settings-{}-{}.json",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        settings::SettingsStore::open(std::env::temp_dir().join(unique))
    }

    /// A directory of its own for a test that writes profile files.
    fn scratch_profile_dir(what: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "radar-workstation-test-profiles-{what}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the profile directory");
        dir
    }

    #[test]
    fn file_playlist_input_is_filename_ordered_and_exact_duplicates_are_removed() {
        let ordered = ordered_unique_paths(vec![
            PathBuf::from("C:/case/KTLX_003"),
            PathBuf::from("C:/case/KTLX_001"),
            PathBuf::from("C:/case/KTLX_002"),
            PathBuf::from("C:/case/KTLX_001"),
        ]);
        assert_eq!(
            ordered,
            ["KTLX_001", "KTLX_002", "KTLX_003"]
                .into_iter()
                .map(|name| PathBuf::from("C:/case").join(name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_1075_path_selection_is_preserved_and_large_estimate_waits_for_confirmation() {
        let paths = (0..1_075)
            .rev()
            .map(|index| PathBuf::from(format!("C:/not-present/volume-{index:04}.msg31")))
            .collect::<Vec<_>>();
        let ordered = ordered_unique_paths(paths.clone());
        assert_eq!(ordered.len(), 1_075);
        assert_eq!(
            ordered.first().unwrap(),
            &PathBuf::from("C:/not-present/volume-0000.msg31")
        );
        assert_eq!(
            ordered.last().unwrap(),
            &PathBuf::from("C:/not-present/volume-1074.msg31")
        );

        let mut app = test_app();
        let generation = app.session_clock.current();
        app.begin_load_sequence(paths);
        assert!(
            app.file_sequence.is_none(),
            "a warned selection must not start before Continue"
        );
        assert_eq!(
            app.pending_playlist_preflight
                .as_ref()
                .expect("metadata planning starts on its worker")
                .selected,
            1_075
        );
        for _ in 0..5_000 {
            app.poll_playlist_preflight();
            if app.pending_playlist_preflight.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let pending = app
            .pending_playlist_confirmation
            .as_ref()
            .expect("the >16 GiB estimate opens the warning");
        assert_eq!(pending.paths.len(), 1_075);
        assert!(pending.estimate.requires_confirmation());
        assert_eq!(
            app.session_clock.current(),
            generation,
            "preflight must not clear or replace the current session"
        );
    }

    #[test]
    fn one_user_selected_path_also_enters_worker_preflight() {
        let mut app = test_app();
        let generation = app.session_clock.current();
        app.begin_load_sequence(vec![PathBuf::from(
            "C:/not-present/single-large-candidate.msg31",
        )]);

        let pending = app
            .pending_playlist_preflight
            .as_ref()
            .expect("a single browser/drop selection must not bypass RAM planning");
        assert_eq!(pending.selected, 1);
        assert_eq!(app.session_clock.current(), generation);
        assert!(app.file_sequence.is_none());

        // The pure estimator test proves a one-file estimate above 16 GiB
        // opens the warning. This test proves the app routes that one selected
        // path to the same worker instead of taking the direct-load shortcut.
        app.cancel_playlist_preflight();
    }

    #[test]
    fn an_approved_single_path_uses_direct_load_instead_of_a_playlist() {
        let mut app = test_app();
        let path = PathBuf::from("C:/not-present/single-level-one-candidate.iq");

        app.start_load_sequence(
            vec![path.clone()],
            crate::playlist_preflight::estimate_paths(&[]),
        );

        assert!(
            app.file_sequence.is_none(),
            "one approved file must retain the direct loader's raw I/Q session"
        );
        assert_eq!(app.source_path_text, path.display().to_string());
        assert!(app.status.starts_with("Loading "));
    }

    #[test]
    fn multiple_approved_paths_still_use_a_playlist() {
        let mut app = test_app();

        app.start_load_sequence(
            vec![
                PathBuf::from("C:/not-present/first-level-one-candidate.iq"),
                PathBuf::from("C:/not-present/second-level-one-candidate.iq"),
            ],
            crate::playlist_preflight::estimate_paths(&[]),
        );

        assert_eq!(
            app.file_sequence
                .as_ref()
                .expect("multiple approved files remain a playlist")
                .paths
                .len(),
            2
        );
    }

    #[test]
    fn timeline_status_names_unlimited_and_each_configured_dimension() {
        assert_eq!(
            history_policy_status(analyst_runtime::HistoryPolicy::unlimited()),
            "retention Unlimited"
        );
        assert_eq!(
            history_policy_status(analyst_runtime::HistoryPolicy::new(30, 0)),
            "retention limit 30 frames / Unlimited RAM"
        );
        assert_eq!(
            history_policy_status(analyst_runtime::HistoryPolicy::new(0, 1024 * 1024 * 1024)),
            "retention limit Unlimited frames / 1.0 GiB RAM"
        );
    }

    #[test]
    fn zero_settings_are_local_unlimited_but_live_safe_per_dimension() {
        use crate::settings_ui::catalog::keys;

        let mut app = test_app();
        assert_eq!(
            app.configured_history_policy(),
            analyst_runtime::HistoryPolicy::unlimited()
        );
        assert_eq!(
            app.live_history_policy(),
            analyst_runtime::HistoryPolicy::default()
        );

        // A local session actively replaces the runtime's live-safe default
        // with the operator's literal zero/Unlimited settings.
        let _ = app
            .history
            .set_policy(analyst_runtime::HistoryPolicy::default());
        app.begin_local_session();
        assert_eq!(
            app.history.policy(),
            analyst_runtime::HistoryPolicy::unlimited()
        );

        app.settings_store.set(
            keys::data::CATEGORY,
            keys::data::HISTORY_MAX_FRAMES,
            settings::SettingValue::Int(45),
        );
        assert_eq!(
            app.live_history_policy(),
            analyst_runtime::HistoryPolicy::new(
                45,
                analyst_runtime::HistoryPolicy::default().max_estimated_bytes,
            ),
            "a positive configured dimension is preserved while zero gets only the live fallback"
        );
    }

    #[test]
    fn file_playlist_position_tolerates_rounding_but_not_a_moved_radar() {
        assert!(same_playlist_position(
            (Some(35.3330), Some(-97.2770)),
            (Some(35.3334), Some(-97.2774))
        ));
        assert!(!same_playlist_position(
            (Some(35.3330), Some(-97.2770)),
            (Some(35.3500), Some(-97.2500))
        ));
        assert!(!same_playlist_position(
            (None, None),
            (Some(35.3330), Some(-97.2770))
        ));
    }

    fn proven_playlist_sweep(
        app: &WorkstationApp,
        sequence: u16,
        start_ms: i32,
        elevation_deg: f32,
    ) -> (
        LoadedVolume,
        nexrad_io::sweep_assembly::ProvenSweepMembership,
    ) {
        let mut volume = renderable_volume(1_768_605_600 + i64::from(start_ms / 1_000));
        let volume_mut = Arc::make_mut(&mut volume);
        volume_mut.site.id = "DOW7".to_owned();
        volume_mut.site.latitude_deg = Some(39.7278);
        volume_mut.site.longitude_deg = Some(-101.5425);
        volume_mut.site.elevation_m = Some(1_020.0);
        let cut = &mut volume_mut.cuts[0];
        cut.elevation_deg = elevation_deg;
        for (index, radial) in cut.radials.iter_mut().enumerate() {
            radial.elevation_deg = elevation_deg;
            radial.time_offset_ms = start_ms + index as i32 * 10;
        }
        let last_ms = cut.radials.last().unwrap().time_offset_ms;
        let evidence = nexrad_io::sweep_assembly::ProvenSweepMembership {
            key: nexrad_io::sweep_assembly::ArchiveVolumeKey {
                archive_family: "AR2V0002.".to_owned(),
                volume_sequence: sequence,
                site_id: "DOW7".to_owned(),
                position: nexrad_io::sweep_assembly::RecordedRadarPosition {
                    latitude_bits: 39.7278f32.to_bits(),
                    longitude_bits: (-101.5425f32).to_bits(),
                    elevation_bits: 1_020.0f32.to_bits(),
                },
                utc_date: volume.volume_time.date_naive(),
                vcp: None,
            },
            first_radial_ms: start_ms,
            last_radial_ms: last_ms,
            elevation_number: Some(1),
            elevation_angle_bits: elevation_deg.to_bits(),
            member_count: 1,
        };
        let loaded = LoadedVolume {
            iq: None,
            assembly: Some(evidence.clone()),
            assembly_refusal: None,
            generation: app.session_clock.current(),
            origin: FrameOrigin::Local,
            source_label: format!("sweep-{start_ms}.msg31"),
            stage: FrameStage::Complete,
            volume,
            elapsed_ms: 1.0,
        };
        (loaded, evidence)
    }

    #[test]
    fn playlist_buffers_proven_sweeps_into_one_logical_volume() {
        let mut app = test_app();
        app.file_sequence = Some(FileSequence {
            paths: vec![PathBuf::from("first.msg31"), PathBuf::from("second.msg31")],
            preflight: crate::playlist_preflight::estimate_paths(&[]),
            next: 2,
            loaded: 0,
            failures: Vec::new(),
            site_id: Some("DOW7".to_owned()),
            site_position: Some((Some(39.7278), Some(-101.5425))),
            level1_files: 0,
            pending_assembly: None,
            assembled_files: 0,
            assembled_groups: 0,
            assembly_refusals: Vec::new(),
            evicted_frames: 0,
        });
        let (first, first_evidence) = proven_playlist_sweep(&app, 210, 10_000, 0.9);
        let second_start = first_evidence.last_radial_ms;
        let (second, second_evidence) = proven_playlist_sweep(&app, 210, second_start, 1.3);

        app.file_sequence.as_mut().unwrap().loaded = 1;
        app.accept_proven_sweep_member(first, first_evidence);
        assert_eq!(
            app.file_sequence.as_ref().unwrap().logical_volumes(),
            1,
            "a nonempty pending group is already one logical volume in progress"
        );
        app.file_sequence.as_mut().unwrap().loaded = 2;
        app.accept_proven_sweep_member(second, second_evidence);

        assert!(app.history.is_empty(), "the group waits for a boundary");
        assert_eq!(
            app.file_sequence
                .as_ref()
                .unwrap()
                .pending_assembly
                .as_ref()
                .unwrap()
                .evidence
                .member_count,
            2
        );
        assert_eq!(
            app.file_sequence.as_ref().unwrap().logical_volumes(),
            1,
            "additional cuts in the pending group do not make progress fall to zero"
        );
        app.flush_pending_sweep_assembly();

        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history.current().unwrap().volume.cuts.len(), 2);
        let sequence = app.file_sequence.as_ref().unwrap();
        assert_eq!(sequence.assembled_files, 2);
        assert_eq!(sequence.assembled_groups, 1);
    }

    #[test]
    fn failed_member_is_a_hard_assembly_boundary() {
        let mut app = test_app();
        app.file_sequence = Some(FileSequence {
            paths: vec![PathBuf::from("first.msg31"), PathBuf::from("broken.msg31")],
            preflight: crate::playlist_preflight::estimate_paths(&[]),
            next: 2,
            loaded: 1,
            failures: Vec::new(),
            site_id: Some("DOW7".to_owned()),
            site_position: Some((Some(39.7278), Some(-101.5425))),
            level1_files: 0,
            pending_assembly: None,
            assembled_files: 0,
            assembled_groups: 0,
            assembly_refusals: Vec::new(),
            evicted_frames: 0,
        });
        let (first, evidence) = proven_playlist_sweep(&app, 210, 10_000, 0.9);
        app.accept_proven_sweep_member(first, evidence);

        app.finish_sequence_failure("truncated Message 31 member".to_owned());

        assert!(
            app.file_sequence.is_none(),
            "the exhausted playlist finished"
        );
        assert_eq!(app.history.len(), 1, "the proven prefix was not discarded");
        let status = app.sequence_status.as_deref().unwrap();
        assert!(status.contains("2 selected"), "{status}");
        assert!(status.contains("1 decoded"), "{status}");
        assert!(status.contains("1 logical volume"), "{status}");
        assert!(status.contains("1 retained"), "{status}");
        assert!(status.contains("1 failed"), "{status}");
        assert!(
            app.sequence_detail
                .as_deref()
                .unwrap()
                .contains("broken.msg31"),
            "the corrupt member must remain individually named"
        );
    }

    #[test]
    fn candidate_refusal_is_named_without_becoming_a_load_failure() {
        let mut app = test_app();
        app.file_sequence = Some(FileSequence {
            paths: vec![PathBuf::from("uncertain.msg31")],
            preflight: crate::playlist_preflight::estimate_paths(&[]),
            next: 1,
            loaded: 0,
            failures: Vec::new(),
            site_id: None,
            site_position: None,
            level1_files: 0,
            pending_assembly: None,
            assembled_files: 0,
            assembled_groups: 0,
            assembly_refusals: Vec::new(),
            evicted_frames: 0,
        });
        let (mut loaded, _) = proven_playlist_sweep(&app, 210, 10_000, 0.9);
        loaded.assembly = None;
        loaded.assembly_refusal =
            Some(nexrad_io::sweep_assembly::SweepAssemblyRefusal::MissingPosition);

        app.finish_sequence_volume(loaded);

        assert_eq!(
            app.history.len(),
            1,
            "the sweep remains an independent frame"
        );
        let status = app.sequence_status.as_deref().unwrap();
        assert!(status.contains("1 selected"), "{status}");
        assert!(status.contains("1 decoded"), "{status}");
        assert!(status.contains("1 logical volume"), "{status}");
        assert!(status.contains("1 retained"), "{status}");
        assert!(status.contains("0 failed"), "{status}");
        let detail = app.sequence_detail.as_deref().unwrap();
        assert!(detail.contains("uncertain.msg31"), "{detail}");
        assert!(
            detail.contains("no complete recorded radar position"),
            "{detail}"
        );
    }

    #[test]
    fn file_playlist_continues_after_failure_and_refuses_a_second_radar() {
        let dir = scratch_profile_dir("file-playlist");
        let cfrad = crate::load_service::io_fixture("cfrad.xsapr_sgp_ppi_20110520.classic.nc");
        let dorade =
            crate::load_service::io_fixture("swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3");
        let first = dir.join("001-cfrad.nc");
        let missing = dir.join("002-missing.nc");
        let duplicate = dir.join("003-cfrad-copy.nc");
        let other_radar = dir.join("004-other-radar");
        std::fs::copy(&cfrad, &first).expect("copy first CfRadial");
        std::fs::copy(&cfrad, &duplicate).expect("copy duplicate CfRadial");
        std::fs::copy(&dorade, &other_radar).expect("copy another radar");

        let mut app = test_app();
        // Deliberately reversed: the session owns and reports its order.
        app.begin_load_sequence(vec![other_radar, duplicate, missing, first]);
        for _ in 0..4_000 {
            app.poll_playlist_preflight();
            app.poll_load_results();
            if app.pending_playlist_preflight.is_none()
                && app.file_sequence.is_none()
                && app
                    .sequence_status
                    .as_deref()
                    .is_some_and(|status| status.contains("complete"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            app.file_sequence.is_none(),
            "the playlist worker never finished"
        );
        assert_eq!(
            app.history.len(),
            2,
            "different selected paths remain independent even when site/time metadata matches"
        );
        let retained_sources = app
            .history
            .frames()
            .iter()
            .map(|frame| frame.source_label.as_str())
            .collect::<Vec<_>>();
        assert!(
            retained_sources
                .iter()
                .any(|source| source.ends_with("001-cfrad.nc")),
            "{retained_sources:?}"
        );
        assert!(
            retained_sources
                .iter()
                .any(|source| source.ends_with("003-cfrad-copy.nc")),
            "{retained_sources:?}"
        );
        assert_eq!(app.history.current().unwrap().identity.site_id, "XSAPR-SGP");
        let status = app.sequence_status.as_deref().expect("playlist status");
        assert!(status.contains("4 selected"), "{status}");
        assert!(status.contains("2 decoded"), "{status}");
        assert!(status.contains("2 logical volume"), "{status}");
        assert!(status.contains("2 retained"), "{status}");
        assert!(status.contains("2 failed"), "{status}");
        let detail = app.sequence_detail.as_deref().expect("playlist detail");
        assert!(detail.contains("002-missing.nc"), "{detail}");
        assert!(detail.contains("does not match playlist radar"), "{detail}");
        assert!(
            app.iq.is_none(),
            "a playlist never retains one file's raw I/Q"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stepping_between_equal_identity_sources_refreshes_frame_and_capabilities() {
        let mut app = test_app();
        let volume = renderable_volume(1_768_605_600);
        let first = VolumeFrame::new(
            Arc::clone(&volume),
            FrameOrigin::Local,
            FrameStage::Complete,
            "selected/first.nc",
        );
        let second = VolumeFrame::new(
            volume,
            FrameOrigin::Local,
            FrameStage::Complete,
            "selected/second.nc",
        );
        let _ = app.history.install_distinct(first);
        let _ = app.history.install_distinct(second);
        assert!(app.history.select(0));
        app.refresh_capabilities();
        let before_signature = app.current_frame_signature();
        let before_capabilities = app.capabilities_for.clone();
        let before_clock = app.frame_clock.current();

        assert!(app.history.select(1));
        app.commit_history_selection(before_signature);
        assert_ne!(app.frame_clock.current(), before_clock);
        app.refresh_capabilities();
        assert_ne!(app.capabilities_for, before_capabilities);
        assert_eq!(
            app.history.current().unwrap().source_label,
            "selected/second.nc"
        );
    }

    #[test]
    fn loop_export_keeps_equal_timestamp_frames_and_restores_selection_and_playback() {
        let mut app = test_app();
        let volume = renderable_volume(1_768_605_600);
        for source in ["selected/first.nc", "selected/second.nc"] {
            app.history.install_distinct(VolumeFrame::new(
                Arc::clone(&volume),
                FrameOrigin::Local,
                FrameStage::Complete,
                source,
            ));
        }
        assert!(app.history.select(1));
        app.history.set_playback(PlaybackState::Playing);
        let context = egui::Context::default();

        app.begin_loop_export(&context);

        let state = app.loop_export.as_ref().expect("loop capture started");
        assert_eq!(state.frame_keys.len(), 2);
        assert_eq!(state.frame_keys[0].source_label, "selected/first.nc");
        assert_eq!(state.frame_keys[1].source_label, "selected/second.nc");
        assert_eq!(state.delay_ms, 700);
        assert_eq!(app.history.selected_index(), Some(0));
        assert_eq!(app.history.playback(), PlaybackState::Paused);

        let state = app.loop_export.take().expect("capture remains active");
        app.restore_loop_export_selection(&state);
        assert_eq!(app.history.selected_index(), Some(1));
        assert!(!app.history.follows_live());
        assert_eq!(app.history.playback(), PlaybackState::Playing);
    }

    #[test]
    fn loop_export_restores_live_follow_on_failure() {
        let mut app = test_app();
        for timestamp in [1_768_605_600, 1_768_605_900] {
            app.history.install(VolumeFrame::new(
                renderable_volume(timestamp),
                FrameOrigin::Live,
                FrameStage::Complete,
                "live",
            ));
        }
        app.history.go_live();
        assert!(app.history.follows_live());
        let context = egui::Context::default();

        app.begin_loop_export(&context);
        assert_eq!(app.history.selected_index(), Some(0));
        assert!(!app.history.follows_live());
        app.abort_loop_export(&context, "the test removed a frame");

        assert!(app.loop_export.is_none());
        assert!(app.history.follows_live());
        assert_eq!(app.history.selected_index(), Some(1));
        assert_eq!(
            app.loop_export_notice.as_deref(),
            Some("Loop export failed: the test removed a frame")
        );
    }

    /// Every string one `draw_pane` pass emitted, with the settings cache the
    /// application would be holding driving it.
    ///
    /// This is the whole point of the profile test below: it reads glyphs off
    /// a real egui pass rather than reading the store back. `geometry` and
    /// `tiles` are `None` because a basemap is not what is being measured -
    /// the ring labels are drawn by `draw_range_rings` from `map.units`
    /// whether or not anything is under them.
    fn pane_strings(app: &WorkstationApp) -> Vec<String> {
        fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => {
                    let text = text.galley.text().trim();
                    if !text.is_empty() {
                        found.push(text.to_owned());
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, found);
                    }
                }
                _ => {}
            }
        }

        let map = PaneMap {
            geometry: None,
            projection: None,
            tiles: None,
            chrome: map_scene::MapChrome::default(),
            sites: Arc::from(Vec::new()),
            site_labels: app.settings_cache.site_labels,
            annotation: app.settings_cache.annotation,
            units: app.settings_cache.units,
            active_site: None,
            hazards: Arc::from(Vec::new()),
        };
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, 600.0));
        let context = egui::Context::default();
        let mut found = Vec::new();
        // Twice: the first egui pass builds the font atlas, and a pane is
        // never a session's first frame.
        for _ in 0..2 {
            found.clear();
            let output = context.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    let overlay = crate::pane_canvas::PaneOverlay {
                        spectrum: None,
                        legend: None,
                        table: None,
                        product_name: "REF",
                        badges: &[],
                        probe: None,
                    };
                    draw_pane(
                        ui,
                        PaneId::new(0).expect("pane 0"),
                        rect,
                        true,
                        analyst_runtime::Camera2D::default(),
                        crate::north_up::NorthUpFrame::unrotated(),
                        app.settings_cache.nav,
                        None,
                        &map,
                        "1 - REF (dBZ)",
                        // The pane header, off the same cache the live path
                        // reads and with the same `None` reason a moment pane
                        // passes: REF is not a derived volume, so the
                        // statement this helper sees is the statement the
                        // application would draw over these very pixels.
                        &crate::gate_filter_ui::pane_status_line(
                            &app.settings_cache.gate_filter,
                            None,
                        )
                        .unwrap_or_default(),
                        &overlay,
                    );
                });
            });
            for shape in output.shapes {
                walk(&shape.shape, &mut found);
            }
        }
        found
    }

    /// The ring labels off a pane pass: `"50 km"`, `"31 mi"`. Recognised by
    /// shape - digits, one space, a unit label - so the pane title and the
    /// status line cannot be mistaken for one.
    fn ring_labels(app: &WorkstationApp) -> Vec<String> {
        pane_strings(app)
            .into_iter()
            .filter(|text| {
                text.split_once(' ').is_some_and(|(number, unit)| {
                    !number.is_empty()
                        && number.bytes().all(|byte| byte.is_ascii_digit())
                        && crate::units::DistanceUnit::ALL
                            .iter()
                            .any(|candidate| candidate.label() == unit)
                })
            })
            .collect()
    }

    /// Write a Units & time choice the way the settings window writes it, and
    /// take the application through the recompute the window's own dispatch
    /// runs afterwards.
    fn choose_distance_unit(app: &mut WorkstationApp, id: &str) {
        use crate::settings_ui::catalog::keys;
        app.settings_store.set(
            keys::units::CATEGORY,
            keys::units::DISTANCE,
            settings::SettingValue::Text(id.to_owned()),
        );
        app.apply_changed_setting(keys::units::CATEGORY, keys::units::DISTANCE);
        app.recompute_settings_cache();
    }

    /// A profile switch has to reach the glass, not just the file.
    ///
    /// The cheap version of this test reads the stored value back and calls
    /// it proved. It would pass over a pane still painting the old unit:
    /// between `settings.json` and a glyph there are two more steps -
    /// `recompute_settings_cache` and the paint pass. So this asserts on the
    /// strings `draw_pane` emitted.
    ///
    /// It stands for the whole Units & time page rather than for one row.
    /// [`WorkstationApp::apply_switched_profile`] enumerates the registry
    /// instead of a list of keys, so a page that is registered is a page a
    /// profile carries, including all four operational settings pages.
    #[test]
    fn switching_back_to_a_profile_restores_the_drawn_unit_not_only_the_stored_one() {
        use crate::settings_ui::catalog::keys;

        let dir = scratch_profile_dir("drawn-unit");
        let context = egui::Context::default();
        let mut app = WorkstationApp::with_context(
            context.clone(),
            None,
            None,
            WarningsSource::Daemon {
                base_url: "http://127.0.0.1:9".to_owned(),
            },
            test_settings_store(),
        );
        // Ring labels on, so the pane writes a distance at all: off is the
        // shipped pane, which has never written a number on a ring.
        app.settings_store.set(
            keys::annotation::CATEGORY,
            keys::annotation::RING_LABELS,
            settings::SettingValue::Bool(true),
        );
        app.recompute_settings_cache();

        let mut library = settings::ProfileLibrary::open(
            &dir,
            WorkstationApp::shipped_settings_document(),
            &app.settings_registry,
        );

        // --- miles on the glass, saved under a name --------------------
        choose_distance_unit(&mut app, "mi");
        let in_miles = ring_labels(&app);
        assert!(
            !in_miles.is_empty(),
            "the pane wrote no ring labels at all, so this test measures nothing"
        );
        assert!(
            in_miles.iter().all(|text| text.ends_with(" mi")),
            "every ring label should be in miles: {in_miles:?}"
        );
        library
            .save_as(
                "Field",
                app.settings_store.document(),
                &app.settings_registry,
            )
            .expect("save the profile");

        // --- kilometres, which has to actually move the glass -----------
        choose_distance_unit(&mut app, "km");
        let in_km = ring_labels(&app);
        assert!(
            in_km.iter().all(|text| text.ends_with(" km")),
            "every ring label should be in kilometres: {in_km:?}"
        );
        assert_ne!(
            in_miles, in_km,
            "the two units painted the same strings, so the unit is not \
             reaching the pane and neither half of this test means anything"
        );

        // --- switch back, through the application's own two calls -------
        //
        // Exactly what `settings_ui::profiles::apply_switch` and
        // `settings_frame` do, in that order: merge the profile over the live
        // document, then let the application apply it. The switch is not
        // clicked here - `examples/profiles_proof.rs` clicks it on a real
        // volume - but nothing between those calls and the paint is stubbed.
        let profile = library
            .find("Field")
            .expect("the profile was just saved")
            .clone();
        let merged = settings::profiles::merge_for_switch(
            app.settings_store.document(),
            &profile.document,
            "Field",
        );
        app.settings_store.replace_document(merged);
        let mut outcome = crate::settings_ui::SettingsOutcome::default();
        app.apply_switched_profile(&mut outcome);
        for (category, id) in &outcome.changed {
            app.apply_changed_setting(category, id);
        }
        // Guarded exactly as `settings_frame` guards it, and that guard is
        // what gives this test teeth. `recompute_settings_cache` reads the
        // store, and the switch has already written miles back into the
        // store - so an unconditional recompute here would repaint the right
        // unit even if `apply_switched_profile` reported nothing at all, and
        // this test would pass over a profile switch that reached the file
        // and stopped there. It only recomputes if the switch said something
        // changed.
        if !outcome.changed.is_empty() {
            app.recompute_settings_cache();
        }

        assert_eq!(
            ring_labels(&app),
            in_miles,
            "coming back to the profile has to repaint the pane in the unit \
             it was saved in, character for character"
        );
        assert_eq!(
            settings::profiles::active_profile(app.settings_store.document()),
            Some("Field"),
            "the switch also names the profile it landed on"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every text run one toolbar frame drew, flattened - `Shape::Vec` nests.
    fn toolbar_texts(app: &mut WorkstationApp) -> Vec<String> {
        fn walk_texts(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => {
                    let text = text.galley.text().trim();
                    if !text.is_empty() {
                        found.push(text.to_owned());
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk_texts(shape, found);
                    }
                }
                _ => {}
            }
        }
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::default());
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1600.0, 900.0),
                )),
                ..Default::default()
            },
            |ui| app.toolbar(ui),
        );
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            walk_texts(&clipped.shape, &mut texts);
        }
        texts
    }

    /// Both chromes are supported and ONE SETTING apart; the compact
    /// menu bar is the preferred field default (2026-08-19), so it is
    /// what a fresh install draws. Pinned in both places a
    /// silent flip could come from: the registry default and the cache the
    /// paint path actually reads.
    /// The Appearance page is declared in two files - the theme module owns
    /// the theme/accent/edges/density/scale axes, `settings_ui::catalog`
    /// owns the toolbar setting - and the registry merges them only if both
    /// spell the category id the same way. Nothing else would notice the
    /// day they stopped agreeing: the settings window would simply grow a
    /// second Appearance page.
    #[test]
    fn the_appearance_page_is_one_page_declared_in_two_files() {
        assert_eq!(
            crate::theme::settings::keys::CATEGORY,
            crate::settings_ui::catalog::keys::appearance::CATEGORY
        );
        let registry =
            crate::settings_ui::full_registry(crate::theme::settings::settings_category());
        let appearance = registry
            .categories()
            .iter()
            .filter(|category| category.id == crate::theme::settings::keys::CATEGORY)
            .collect::<Vec<_>>();
        assert_eq!(appearance.len(), 1, "the page split into two");
        let ids = appearance[0]
            .settings
            .iter()
            .map(|spec| spec.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                crate::theme::settings::keys::THEME,
                crate::theme::settings::keys::ACCENT,
                crate::theme::settings::keys::CHROME_EDGES,
                crate::theme::settings::keys::DENSITY,
                crate::theme::settings::keys::UI_SCALE,
                crate::settings_ui::catalog::keys::appearance::TOOLBAR,
            ],
            "the look, then the size, then the layout"
        );
        assert_eq!(
            registry
                .categories()
                .first()
                .map(|category| category.id.as_str()),
            Some(crate::theme::settings::keys::CATEGORY),
            "Appearance is the first page in the category list"
        );
    }

    /// The application's registry is the theme's Appearance page plus
    /// EVERYTHING the catalog declares, in the catalog's own order.
    ///
    /// `settings_ui::full_registry` registers the Appearance page itself and
    /// then hands the registry to `catalog::register_into`, which returns
    /// nothing and registers by side effect. That shape is the seam where a
    /// page goes missing without anyone noticing: `register_into` is a wall
    /// of `registry.register(...)` lines, one page each. Omitting a line takes
    /// its whole page - its rows, its search hits, its reset and its share of
    /// every profile - out of the application while
    /// `catalog::registry()` and every test written against THAT keep
    /// passing, because they go through the same lost line.
    ///
    /// So this pins the complete list, by name and in order, against the only
    /// registry the application actually runs on.
    #[test]
    fn the_application_registry_is_the_appearance_page_and_the_whole_catalog() {
        use crate::settings_ui::catalog::keys;
        let registry =
            crate::settings_ui::full_registry(crate::theme::settings::settings_category());
        let ids = registry
            .categories()
            .iter()
            .map(|category| category.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                // Theme settings and catalog settings share this page.
                crate::theme::settings::keys::CATEGORY,
                keys::map::CATEGORY,
                keys::observations::CATEGORY,
                keys::radar::CATEGORY,
                keys::navigation::CATEGORY,
                keys::vol3d::CATEGORY,
                keys::analysis::CATEGORY,
                keys::data::CATEGORY,
                // The four operational settings pages...
                keys::units::CATEGORY,
                keys::network::CATEGORY,
                keys::annotation::CATEGORY,
                keys::xsection::CATEGORY,
                // The Level 1 page, which arrived with the time-series reader.
                keys::timeseries::CATEGORY,
                // ...and the page about all the other pages, last.
                keys::profiles::CATEGORY,
            ],
            "the category list is the window's left column, in registration order"
        );
        // Every page carries rows. A page registered empty is a page that
        // reads as present and does nothing.
        for category in registry.categories() {
            assert!(
                !category.settings.is_empty(),
                "the {} page is registered with no rows",
                category.id
            );
        }
    }

    /// A fresh store, resolved through the real registry, must ask for
    /// exactly the look the application drew before any of this was
    /// settable - the same claim `theme_catalog.rs` makes about the axes'
    /// defaults, made here against the STORE, which is the thing that can
    /// actually disagree with them.
    #[test]
    fn a_fresh_store_resolves_to_the_shipped_appearance() {
        let app = test_app();
        assert_eq!(app.appearance(), crate::theme::Appearance::default());
        assert_eq!(app.appearance().theme.id, "light");
    }

    /// ...and a stored id this build does not know resolves around, without
    /// a panic and without the analyst's stored string being replaced.
    #[test]
    fn a_stranger_theme_in_the_store_falls_back_without_overwriting_it() {
        let mut app = test_app();
        let stranger = "amber-crt-from-a-newer-build";
        app.settings_store.set(
            crate::theme::settings::keys::CATEGORY,
            crate::theme::settings::keys::THEME,
            settings::SettingValue::Text(stranger.to_owned()),
        );
        assert_eq!(app.appearance(), crate::theme::Appearance::default());
        assert_eq!(
            app.settings_store.value(
                crate::theme::settings::keys::CATEGORY,
                crate::theme::settings::keys::THEME
            ),
            Some(settings::SettingValue::Text(stranger.to_owned())),
            "the stored choice must survive a build that does not have the theme"
        );
    }

    #[test]
    fn the_default_toolbar_style_is_the_menu_bar() {
        assert_eq!(SettingsCache::default().toolbar_style, ToolbarStyle::Menus);
        let registry = crate::settings_ui::catalog::registry();
        let store = test_settings_store();
        assert_eq!(
            store.effective_text(&registry, "appearance", "toolbar"),
            "menus"
        );
        let app = test_app();
        assert_eq!(app.settings_cache.toolbar_style, ToolbarStyle::Menus);
    }

    /// The menu-bar style: one row that carries the mid-storm controls
    /// itself and files the occasional ones under four titles. If a storm
    /// control migrates into a menu, or a menu's contents leak onto the row,
    /// this is the test that says so.
    #[test]
    fn the_menu_bar_keeps_storm_controls_out_and_occasional_ones_filed() {
        let mut app = test_app();
        assert_eq!(app.settings_cache.toolbar_style, ToolbarStyle::Menus);
        let product = DisplayProduct::from_product_id(&app.workspace.active().product);
        let texts = toolbar_texts(&mut app);

        for on_the_row in [
            "File",
            "View",
            "Map",
            "Tools",
            "KRTX",
            "Start live",
            "− Tilt",
            "+ Tilt",
        ] {
            assert!(
                texts.iter().any(|text| text == on_the_row),
                "the menu bar drew no {on_the_row:?}. It drew: {texts:?}"
            );
        }
        assert!(
            texts.iter().any(|text| text.starts_with(product.label())),
            "the menu bar lost the product button. It drew: {texts:?}"
        );
        // Closed menus keep their contents: these live under View / Tools /
        // File and must not be on the row.
        for filed_away in ["Link cameras", "Level II file path", "Settings…"] {
            assert!(
                !texts.iter().any(|text| text == filed_away),
                "{filed_away:?} leaked out of its menu onto the row. \
                 The bar drew: {texts:?}"
            );
        }
    }

    /// The everything-visible style, exactly as v0.1.0 shipped it: every
    /// control on the bar in ONE pass, and no menu title anywhere. The
    /// dynamic labels (product, palette, quality, basemap, tilt, warnings)
    /// are computed from the application's own state rather than spelled
    /// out, so renaming a colour table does not fail this test while hiding
    /// its picker still does.
    #[test]
    fn the_everything_style_puts_every_control_on_the_bar() {
        let mut app = test_app();
        app.settings_cache.toolbar_style = ToolbarStyle::Everything;

        let product = DisplayProduct::from_product_id(&app.workspace.active().product);
        let palette_family = crate::product_picker::palette_family(product);
        let expected_palette =
            palette_family.map(|family| app.color_tables.for_family(family).name().to_owned());
        let expected_quality = app.quality.preset_label().unwrap_or("Custom").to_owned();
        let expected_basemap = map_scene::MapStylePreset::for_style(app.map_scene.style())
            .expect("a fresh scene holds one of the preset styles")
            .label()
            .to_owned();
        let expected_tilt = app.active_tilt_label();
        let expected_warnings = app.warnings_state.label().to_owned();
        let texts = toolbar_texts(&mut app);

        let mut wanted = vec![
            // The field takes every container the routing seam accepts, not
            // only Level II, and says so.
            "Radar volume file path".to_owned(),
            "Load".to_owned(),
            "Export current view".to_owned(),
            "Export loop…".to_owned(),
            "Online Level I…".to_owned(),
            "KRTX".to_owned(),
            "Start live".to_owned(),
            layout_label(app.workspace.layout).to_owned(),
            expected_quality,
            "− Tilt".to_owned(),
            expected_tilt,
            "+ Tilt".to_owned(),
            expected_basemap,
            "No imagery".to_owned(),
            "Link cameras".to_owned(),
            "3D".to_owned(),
            "XSec".to_owned(),
            "Vrot".to_owned(),
            "Settings".to_owned(),
            product.label().to_owned(),
            "Pane 1".to_owned(),
        ];
        wanted.extend(expected_palette);
        for control in &wanted {
            assert!(
                texts.iter().any(|text| text == control),
                "the bar drew no {control:?} control. It drew: {texts:?}"
            );
        }
        // The warnings chip carries its state after a separator, so it is
        // matched by its head rather than whole.
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with(&expected_warnings)),
            "the bar drew no warnings chip starting {expected_warnings:?}. It drew: {texts:?}"
        );
        for title in ["File", "View", "Map", "Tools"] {
            assert!(
                !texts.iter().any(|text| text == title),
                "a {title:?} menu title is on the everything-visible bar; \
                 this style puts every control on the row instead. \
                 The bar drew: {texts:?}"
            );
        }
    }

    // --- what the file says it threw away -----------------------------------
    //
    // Every generic data moment block in a Message 31 radial states the
    // signal-to-noise threshold the operational processor censored that moment
    // at, and whether it recombined the sweep before writing it (NEXRAD ICD
    // 2620002W, Build 22.0, 05 June 2023, Table XVII-B, bytes 16-17 and byte
    // 18). An analyst hunting a weak field that is not on the screen has to be
    // able to tell "the atmosphere was empty" from "the processor threw it
    // away before the file existed", and the bar is where this application
    // says which.

    /// The censoring readouts one toolbar frame drew, in BOTH chromes.
    ///
    /// Both every time, because the two are one setting apart: a readout added
    /// to one and forgotten in the other is invisible to whichever half of the
    /// analysts chose the other. Each style contributes its own entries, so a
    /// statement drawn once per style comes back twice.
    fn censoring_texts(app: &mut WorkstationApp) -> Vec<String> {
        let mut drawn = Vec::new();
        for style in [ToolbarStyle::Menus, ToolbarStyle::Everything] {
            app.settings_cache.toolbar_style = style;
            drawn.extend(toolbar_texts(app).into_iter().filter(|text| {
                text.contains("SNR threshold") || text.starts_with("Resolution reduced")
            }));
        }
        drawn
    }

    /// The text runs one toolbar frame had to clip to fit, flattened.
    ///
    /// A separate walk from `toolbar_texts` because `Galley::text` reports the
    /// whole source string whether or not it was drawn whole (epaint 0.34.3,
    /// `text_layout_types.rs`: "the full, non-elided text of the input job").
    /// Comparing strings therefore cannot catch a readout that was ellipsized
    /// on its way to the screen; `Galley::elided` is the flag that can.
    fn toolbar_elided_texts(app: &mut WorkstationApp) -> Vec<String> {
        fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) if text.galley.elided => {
                    found.push(text.galley.text().trim().to_owned());
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, found);
                    }
                }
                _ => {}
            }
        }
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::default());
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1600.0, 900.0),
                )),
                ..Default::default()
            },
            |ui| app.toolbar(ui),
        );
        let mut elided = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut elided);
        }
        elided
    }

    /// One of the io crate's real fixtures, loaded and measured.
    fn app_showing(fixture: &str) -> WorkstationApp {
        let path = crate::load_service::io_fixture(fixture);
        let volume = nexrad_io::decode_supported_volume_from_path(&path)
            .unwrap_or_else(|error| panic!("{} did not decode: {error}", path.display()));
        let mut app = test_app();
        install(&mut app, Arc::new(volume));
        // What `canvas` does before it resolves a cut. Without it the tilt and
        // the readout beside it answer from the fallback path rather than from
        // the measured volume.
        app.refresh_capabilities();
        app
    }

    /// The number on the bar is the one in the file, and it follows the sweep
    /// on screen rather than the volume.
    ///
    /// Real bytes: KDVN (Davenport, Iowa) 2026-08-19 19:28:02 UTC, VCP 212 -
    /// four LDM records of the operational volume kept verbatim in the io
    /// crate's fixtures. Reflectivity is drawn from the contiguous
    /// surveillance half of the lowest split cut, censored at 2.0 dB;
    /// velocity comes from the Doppler half of the same elevation, censored
    /// harder at 3.5 dB. Across the whole 11 MB volume the field takes only
    /// those two values, so a readout that answered 2.0 dB under velocity
    /// would be describing the wrong half of the cut.
    #[test]
    fn the_bar_states_the_snr_threshold_the_sweep_on_screen_was_censored_at() {
        let mut app = app_showing("KDVN20260819_192802_V06.rec0_1_7_79");
        let active = app.workspace.active_pane;

        app.apply_product_selection(active, DisplayProduct::Reflectivity);
        assert_eq!(
            censoring_texts(&mut app),
            ["REF SNR threshold 2.0 dB"; 2],
            "the surveillance leg of the lowest split cut was censored at 2.0 dB"
        );

        app.apply_product_selection(active, DisplayProduct::Velocity);
        assert_eq!(
            censoring_texts(&mut app),
            ["VEL SNR threshold 3.5 dB"; 2],
            "the Doppler leg of the same elevation was censored at 3.5 dB"
        );

        // The control flags are 0 on all 46,440 moment blocks of this volume,
        // so nothing here may claim a resolution loss. `censoring_texts`
        // collects both statements; only the threshold was drawn.
        assert!(
            !censoring_texts(&mut app)
                .iter()
                .any(|text| text.starts_with("Resolution reduced")),
            "an un-recombined sweep was reported as coarsened"
        );
    }

    /// Hovering the readout explains what the number cost, in both chromes.
    ///
    /// The number alone reads as trivia. The sentence under the pointer is the
    /// part that earns it a place on the bar: the gates below the threshold
    /// are missing from the PRODUCT, not from the atmosphere.
    #[test]
    fn hovering_the_snr_readout_says_what_the_number_costs() {
        for style in [ToolbarStyle::Menus, ToolbarStyle::Everything] {
            let mut app = app_showing("KDVN20260819_192802_V06.rec0_1_7_79");
            app.settings_cache.toolbar_style = style;
            let context = egui::Context::default();
            crate::theme::apply(&context, &crate::theme::Appearance::default());

            let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 900.0));
            let mut over = None;
            let mut drawn = Vec::new();
            // Find the readout, move the pointer onto it, then let the clock
            // run without moving again: egui waits out
            // `interaction.tooltip_delay` and wants the pointer STILL before it
            // paints a tooltip, so a single hovering frame would draw nothing.
            for pass in 0..4 {
                let events = match (pass, over) {
                    (1, Some(position)) => vec![egui::Event::PointerMoved(position)],
                    _ => Vec::new(),
                };
                let output = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        time: Some(f64::from(pass)),
                        events,
                        ..Default::default()
                    },
                    |ui| app.toolbar(ui),
                );
                let shapes: Vec<egui::Shape> = output
                    .shapes
                    .iter()
                    .map(|clipped| clipped.shape.clone())
                    .collect();
                if over.is_none() {
                    over = exact_text_position(&shapes, "REF SNR threshold 2.0 dB");
                    assert!(
                        over.is_some(),
                        "the {style:?} bar drew no readout to hover. It drew: {:?}",
                        shape_texts(&shapes)
                    );
                }
                drawn = shape_texts(&shapes);
            }
            assert!(
                drawn.iter().any(|text| text == SNR_THRESHOLD_HINT),
                "hovering the {style:?} bar's readout explained nothing. It drew: {drawn:?}"
            );
        }
    }

    /// A sweep the processor coarsened before writing says so, whole.
    ///
    /// Hand-set flags, and they have to be: the control flags are 0 on every
    /// one of the 46,440 moment blocks of the real volume above, and on every
    /// other volume this repository has decoded, so no real bytes here can
    /// drive this branch. `radar_core` pins the four ICD codes and the words
    /// each becomes. What is pinned HERE is that both statements reach the bar,
    /// and reach it whole: `bevel::sunken_readout` clips its galley at the
    /// width it is handed, so a bound set below the longest label would quietly
    /// ellipsize the one statement that says the picture is coarser than the
    /// radar made it.
    #[test]
    fn a_recombined_sweep_states_its_loss_in_full() {
        let mut volume = RadarVolume::new(radar_core::RadarSite::new("KTLX"), Utc::now());
        let gates = radar_core::GateRange {
            first_gate_m: 2_125,
            gate_spacing_m: 250,
            gate_count: 4,
        };
        let cut = volume.push_cut(0.5, Some(1));
        cut.radials.push(radar_core::Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: gates.clone(),
            nyquist_velocity_mps: None,
            radial_status: None,
        });
        let mut grid = radar_core::MomentGrid::new_u8(
            radar_core::MomentType::Reflectivity,
            gates,
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        // 2.125 dB is a value the 0.125 dB quantum can hold and one decimal
        // cannot print, so the rounding rule is exercised on the way through
        // as well.
        grid.snr_threshold_db = Some(2.125);
        grid.recombination = Some(radar_core::MomentRecombination::RadialsAndRangeGates);
        grid.push_u8_row_slice(0, &[0_u8; 4])
            .expect("an 8-bit row belongs in an 8-bit grid");
        cut.moments
            .insert(radar_core::MomentType::Reflectivity, grid);

        let mut app = test_app();
        install(&mut app, Arc::new(volume));
        app.refresh_capabilities();
        assert_eq!(
            censoring_texts(&mut app),
            [
                "REF SNR threshold 2.125 dB",
                "Resolution reduced on this sweep: radials and range gates recombined to legacy \
                 resolution",
                "REF SNR threshold 2.125 dB",
                "Resolution reduced on this sweep: radials and range gates recombined to legacy \
                 resolution",
            ],
            "the bar did not state both facts about a recombined sweep"
        );

        for style in [ToolbarStyle::Menus, ToolbarStyle::Everything] {
            app.settings_cache.toolbar_style = style;
            let clipped: Vec<String> = toolbar_elided_texts(&mut app)
                .into_iter()
                .filter(|text| {
                    text.contains("SNR threshold") || text.starts_with("Resolution reduced")
                })
                .collect();
            assert!(
                clipped.is_empty(),
                "the {style:?} bar ellipsized a censoring statement: {clipped:?}"
            );
        }
    }

    /// A format that never stated these fields says nothing at all.
    ///
    /// The failure this forbids is a readout reading "REF SNR threshold 0.0
    /// dB" over an ODIM sweep: a claim the file never made, and an invisible
    /// one, because 0.0 dB is a plausible operational setting. Real files of
    /// every other format the application opens, plus a grid built the way the
    /// Message 1 path builds one - that path predates the generic data moment
    /// block entirely and leaves both fields unset.
    #[test]
    fn a_volume_that_never_stated_a_threshold_draws_no_readout() {
        for fixture in [
            // SMHI Angelholm, 2026-08-20 00:00 UTC (OPERA ORD, CC BY 4.0).
            "seang.scan.20260820.dbzh_th_vradh.h5",
            // ARM X-SAPR at SGP, 2011-05-20, a 40-ray classic-netCDF PPI.
            "cfrad.xsapr_sgp_ppi_20110520.classic.nc",
            // VORTEX-2 NOXP, 2009-05-09 (doi:10.5281/zenodo.14194361).
            "swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3",
        ] {
            let mut app = app_showing(fixture);
            let drawn = censoring_texts(&mut app);
            assert!(
                drawn.is_empty(),
                "{fixture} carries no censoring fields, but the bar drew {drawn:?}"
            );
        }

        let mut app = test_app();
        install(&mut app, renderable_volume(1_760_000_000));
        app.refresh_capabilities();
        assert!(
            censoring_texts(&mut app).is_empty(),
            "a grid built without these fields was reported as if it had them"
        );
    }

    /// A one-cut, 360-radial reflectivity volume the real render worker can
    /// raster. Shape and encoding follow `probe.rs`'s test volume.
    fn renderable_volume(unix_seconds: i64) -> Arc<RadarVolume> {
        let time = chrono::DateTime::from_timestamp(unix_seconds, 0)
            .expect("a fixed epoch second is a valid timestamp");
        let mut volume = RadarVolume::new(radar_core::RadarSite::new("KTLX"), time);
        let gates = || radar_core::GateRange {
            first_gate_m: 2_125,
            gate_spacing_m: 250,
            gate_count: 100,
        };
        let cut = volume.push_cut(0.5, Some(1));
        let mut grid = radar_core::MomentGrid::new_u8(
            radar_core::MomentType::Reflectivity,
            gates(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        for index in 0..360_usize {
            cut.radials.push(radar_core::Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 10,
                gate_range: gates(),
                nyquist_velocity_mps: Some(26.0),
                radial_status: None,
            });
            let mut words = vec![0_u8; 100];
            // A ring of ~51 dBZ so the raster is not empty.
            words[40] = 168;
            grid.push_u8_row_slice(index, &words)
                .expect("an 8-bit row belongs in an 8-bit grid");
        }
        cut.moments
            .insert(radar_core::MomentType::Reflectivity, grid);
        Arc::new(volume)
    }

    fn install(app: &mut WorkstationApp, volume: Arc<RadarVolume>) {
        let generation = app.session_clock.current();
        let _ = app.install_loaded_volume(LoadedVolume {
            iq: None,
            assembly: None,
            assembly_refusal: None,
            generation,
            origin: FrameOrigin::Live,
            source_label: "test".to_owned(),
            stage: FrameStage::Complete,
            volume,
            elapsed_ms: 1.0,
        });
    }

    /// The same, at the stage a live volume still assembling arrives in.
    fn install_partial(app: &mut WorkstationApp, volume: Arc<RadarVolume>) {
        let generation = app.session_clock.current();
        let _ = app.install_loaded_volume(LoadedVolume {
            iq: None,
            assembly: None,
            assembly_refusal: None,
            generation,
            origin: FrameOrigin::Live,
            source_label: "test".to_owned(),
            stage: FrameStage::Partial,
            volume,
            elapsed_ms: 1.0,
        });
    }

    /// Real, complete sweeps at independently controlled elevations and
    /// acquisition times; they share one growing live-volume identity.
    fn live_follow_volume(sweeps: &[(f32, i32)]) -> Arc<RadarVolume> {
        let mut volume = (*renderable_volume(1_700_000_000)).clone();
        let template = volume.cuts[0].clone();
        volume.cuts.clear();
        for (index, (elevation_deg, offset_ms)) in sweeps.iter().copied().enumerate() {
            let mut cut = template.clone();
            cut.elevation_deg = elevation_deg;
            cut.elevation_number = u8::try_from(index + 1).ok();
            for (radial_index, radial) in cut.radials.iter_mut().enumerate() {
                radial.elevation_deg = elevation_deg;
                radial.time_offset_ms = offset_ms + radial_index as i32 * 10;
            }
            volume.cuts.push(cut);
        }
        Arc::new(volume)
    }

    /// Trim the most recent sweep exactly the way a partial live decode does:
    /// both the physical radials and each required moment's gate rows stop at
    /// the same real acquisition frontier.
    fn with_partial_last_sweep(volume: Arc<RadarVolume>, radial_count: usize) -> Arc<RadarVolume> {
        let mut volume = (*volume).clone();
        let cut = volume.cuts.last_mut().expect("at least one fixture sweep");
        cut.radials.truncate(radial_count);
        for grid in cut.moments.values_mut() {
            grid.radial_indices.truncate(radial_count);
            let values = radial_count * grid.gate_range.gate_count;
            match &mut grid.storage {
                radar_core::MomentStorage::U8(stored) => stored.truncate(values),
                radar_core::MomentStorage::U16(stored) => stored.truncate(values),
                radar_core::MomentStorage::F32(stored) => stored.truncate(values),
            }
        }
        Arc::new(volume)
    }

    #[test]
    fn live_follow_animates_an_arriving_sweep_over_the_previous_completed_sweep() {
        let mut app = test_app();
        app.live_site = Some("KTLX".to_owned());
        install_partial(
            &mut app,
            with_partial_last_sweep(live_follow_volume(&[(0.5, 0), (0.5, 60_000)]), 120),
        );
        app.refresh_capabilities();
        app.set_live_follow_enabled(true);

        let pane = first_pane();
        assert_eq!(
            app.workspace.pane(pane).tilt,
            TiltSelection::CutIndex(1),
            "Auto tilt must select the incoming repeat while it is actually sweeping"
        );

        app.advance_sweeps();
        let first = app.panes[pane.index()]
            .sweep_state
            .expect("the selected incoming cut drives the existing live animator");
        assert!(
            !first.complete,
            "an arriving sweep must not be called complete"
        );
        assert!(
            first.revealed_deg > 0.0 && first.revealed_deg < 360.0,
            "the genuine incoming radial frontier is an unfinished clockwise reveal"
        );

        let current = app
            .history
            .current()
            .expect("the partial volume is selected");
        let (underpaint, underpaint_cut) = crate::app_support::previous_sweep_for(
            &app.history,
            &current.volume,
            1,
            &radar_core::MomentType::Reflectivity,
        )
        .expect("the older complete same-volume sweep remains visible under the wipe");
        assert!(Arc::ptr_eq(&underpaint, &current.volume));
        assert_eq!(underpaint_cut, 0);

        install_partial(
            &mut app,
            with_partial_last_sweep(live_follow_volume(&[(0.5, 0), (0.5, 60_000)]), 220),
        );
        app.refresh_capabilities();
        app.follow_live_low_tilts();
        app.advance_sweeps();
        let grown = app.panes[pane.index()]
            .sweep_state
            .expect("the same incoming sweep remains animated as new radials arrive");
        assert!(!grown.complete);
        assert!(
            grown.frontier_deg > first.frontier_deg,
            "the animator tracks the newer, genuinely received radial frontier"
        );
    }

    #[test]
    fn live_follow_selects_newest_completed_sweep_below_the_adjustable_ceiling() {
        use crate::settings_ui::catalog::keys::data as key;

        let mut app = test_app();
        app.live_site = Some("KTLX".to_owned());
        install_partial(
            &mut app,
            live_follow_volume(&[(0.5, 0), (0.9, 30_000), (1.3, 60_000), (1.8, 90_000)]),
        );
        app.refresh_capabilities();

        app.set_live_follow_enabled(true);
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::CutIndex(2),
            "the newest complete 1.3° sweep is allowed; the newer 1.8° sweep is not"
        );

        assert!(app.settings_store.set(
            key::CATEGORY,
            key::FOLLOW_MAX_ELEVATION_DEG,
            settings::SettingValue::Float(0.8),
        ));
        app.apply_changed_setting(key::CATEGORY, key::FOLLOW_MAX_ELEVATION_DEG);
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::CutIndex(0),
            "changing the ceiling immediately honors the analyst's new limit"
        );
    }

    #[test]
    fn live_follow_respects_manual_tilt_until_a_new_completed_sweep_arrives() {
        let mut app = test_app();
        app.live_site = Some("KTLX".to_owned());
        install_partial(&mut app, live_follow_volume(&[(0.5, 0), (0.9, 30_000)]));
        app.refresh_capabilities();
        app.set_live_follow_enabled(true);
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::CutIndex(1)
        );

        app.change_active_tilt(-1);
        app.follow_live_low_tilts();
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::CutIndex(0),
            "repainting the existing volume must not steal back a manually chosen tilt"
        );

        install_partial(
            &mut app,
            live_follow_volume(&[(0.5, 0), (0.9, 30_000), (1.2, 60_000)]),
        );
        app.refresh_capabilities();
        app.follow_live_low_tilts();
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::CutIndex(2),
            "the next genuinely new completed low sweep resumes automatic following"
        );
    }

    #[test]
    fn live_follow_never_changes_historical_playback_or_local_sessions() {
        let mut app = test_app();
        install_partial(&mut app, live_follow_volume(&[(0.5, 0), (0.9, 30_000)]));
        app.refresh_capabilities();
        app.set_live_follow_enabled(true);
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::LowestAvailable,
            "a local or archived session is never an automatic live-follow target"
        );

        app.live_site = Some("KTLX".to_owned());
        app.history.set_playback(PlaybackState::Playing);
        app.follow_live_low_tilts();
        assert_eq!(
            app.workspace.pane(first_pane()).tilt,
            TiltSelection::LowestAvailable,
            "timeline playback owns its selection even while a live site is connected"
        );
    }

    /// What `canvas` does for pane 0, minus the painting: measure, resolve,
    /// request, then poll the real render worker until the pane settles.
    fn pump_render(app: &mut WorkstationApp, context: &egui::Context) {
        let pane = first_pane();
        app.refresh_capabilities();
        app.update_viewport(pane, VIEWPORT);
        let Some(volume) = app.history.current().map(|frame| Arc::clone(&frame.volume)) else {
            return;
        };
        app.ensure_render_requested(pane, volume, VIEWPORT);
        for _ in 0..1_000 {
            app.poll_render_results(context);
            if app.panes[pane.index()].pending_stamp.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the render worker never answered");
    }

    /// §2.1: a frame change holds the outgoing picture - stale, not blank -
    /// until the replacement render lands and swaps it.
    #[test]
    fn a_frame_change_keeps_the_last_picture_until_the_new_render_lands() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        install(&mut app, renderable_volume(1_700_000_360));
        pump_render(&mut app, &context);
        let pane = first_pane();
        let held = app.panes[pane.index()]
            .texture
            .as_ref()
            .expect("the live-edge frame rendered")
            .stamp;

        // Scrub back one frame, exactly as the timeline slider does.
        let before = app.current_frame_signature();
        assert!(app.history.select(0));
        app.history.set_playback(PlaybackState::Paused);
        app.commit_history_selection(before);

        let texture = app.panes[pane.index()]
            .texture
            .as_ref()
            .expect("the outgoing frame is HELD on a scrub, not blanked");
        assert_eq!(texture.stamp, held);
        assert_ne!(
            texture.stamp,
            app.current_stamp(pane),
            "the held texture must read as stale"
        );
        assert!(
            !app.visible_panes_ready(),
            "a held stale texture must not count as ready, or playback pacing breaks"
        );

        // The replacement still lands and swaps it.
        pump_render(&mut app, &context);
        let swapped = app.panes[pane.index()]
            .texture
            .as_ref()
            .expect("the selected frame rendered");
        assert_eq!(swapped.stamp, app.current_stamp(pane));
    }

    /// §2.1: product and tilt changes go through `invalidate_semantic_panes`,
    /// which must keep the texture the way the view path always has.
    #[test]
    fn a_product_or_tilt_change_keeps_the_texture_while_the_replacement_renders() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        pump_render(&mut app, &context);
        let pane = first_pane();
        assert!(app.panes[pane.index()].texture.is_some());

        app.invalidate_semantic_panes(&[pane]);
        let texture = app.panes[pane.index()]
            .texture
            .as_ref()
            .expect("a product/tilt change must not blank the pane to bare basemap");
        assert_ne!(texture.stamp, app.current_stamp(pane));
    }

    /// §2.1: one failed decode never blanks a frame that is on screen; with
    /// nothing on screen the failure is the load itself and the panes clear.
    #[test]
    fn a_failed_decode_never_blanks_a_frame_that_is_on_screen() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        pump_render(&mut app, &context);
        let pane = first_pane();

        app.handle_load_failure("KTLX live", "one bad chunk round trip");
        assert!(
            app.panes[pane.index()].texture.is_some(),
            "a failed live decode blanked an intact picture"
        );
        assert!(app.status.contains("one bad chunk round trip"));

        // Nothing on screen: the failure IS the load the analyst is waiting
        // on, and clearing is the honest answer.
        app.history.clear();
        app.handle_load_failure("C:/gone_V06", "could not read");
        assert!(app.panes[pane.index()].texture.is_none());
    }

    /// §2.2: a failed render is terminal for its stamp - never resubmitted,
    /// never a playback gate - and any clock bump retries naturally.
    #[test]
    fn a_failed_render_is_terminal_not_a_hot_loop() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        pump_render(&mut app, &context);
        let pane = first_pane();
        let volume = app
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
            .expect("a frame");

        // A camera tick, then the worker reports failure for the new stamp.
        app.invalidate_view_panes(&[pane]);
        let stamp = app.current_stamp(pane);
        app.handle_render_failure(pane, stamp, "no reflectivity cuts".to_owned());
        assert_eq!(
            app.panes[pane.index()].terminal,
            Some(RenderTerminal::Failed(stamp))
        );
        assert_eq!(app.panes[pane.index()].status, "no reflectivity cuts");

        // The identical stamp is never resubmitted...
        app.ensure_render_requested(pane, Arc::clone(&volume), VIEWPORT);
        assert_eq!(
            app.panes[pane.index()].pending_stamp,
            None,
            "the doomed stamp was resubmitted - this is the render-worker hot loop"
        );
        // ...the pane does not gate playback...
        assert!(
            app.visible_panes_ready(),
            "a terminal pane froze playback at a 60 Hz spin"
        );
        // ...and any clock bump retries on its own.
        app.invalidate_view_panes(&[pane]);
        app.ensure_render_requested(pane, volume, VIEWPORT);
        assert!(
            app.panes[pane.index()].pending_stamp.is_some(),
            "a new stamp must retry"
        );
    }

    /// §2.2: playback steps past a frame a pane cannot draw instead of
    /// freezing on it for ever. This freeze had no test before.
    #[test]
    fn playback_steps_past_a_pane_that_cannot_render() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        install(&mut app, renderable_volume(1_700_000_360));
        pump_render(&mut app, &context);
        let pane = first_pane();

        app.invalidate_view_panes(&[pane]);
        let stamp = app.current_stamp(pane);
        app.handle_render_failure(pane, stamp, "implausible field".to_owned());

        app.history.set_playback(PlaybackState::Playing);
        app.last_playback_step = Instant::now() - Duration::from_secs(2);
        let before = app.current_frame_signature();
        app.advance_playback(&context);
        assert_ne!(
            app.current_frame_signature(),
            before,
            "playback froze on the failed render"
        );
    }

    /// §2.2: a product no cut can serve is terminal the same way.
    #[test]
    fn an_unavailable_product_is_terminal_and_does_not_gate_playback() {
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        app.refresh_capabilities();
        let pane = first_pane();
        app.update_viewport(pane, VIEWPORT);
        // The volume carries reflectivity only; ask the pane for velocity.
        app.workspace.pane_mut(pane).product = DisplayProduct::DealiasedVelocity.product_id();
        let volume = app
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
            .expect("a frame");
        app.ensure_render_requested(pane, volume, VIEWPORT);
        let stamp = app.current_stamp(pane);
        assert_eq!(
            app.panes[pane.index()].terminal,
            Some(RenderTerminal::Unavailable(stamp))
        );
        assert!(app.panes[pane.index()].status.contains("unavailable"));
        assert!(
            app.visible_panes_ready(),
            "an unavailable product froze playback"
        );
    }

    #[test]
    fn source_field_static_tools_stop_before_the_reflectivity_default() {
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::default());
        let mut app = test_app();
        let pane = first_pane();
        app.apply_source_field_selection(pane, "VL1_CRR");
        let source_id = app.workspace.pane(pane).product.clone();
        assert_eq!(
            modeled_product_or_source_field(&source_id),
            Err("VL1_CRR"),
            "a producer-native id crossed the static resolver's REF default"
        );
        for tool in ["3D Volume", "Cross-section"] {
            let refusal = source_field_2d_only_message(tool, "VL1_CRR");
            assert!(refusal.contains("VL1_CRR"), "{refusal}");
            assert!(refusal.contains("2D only"), "{refusal}");
            assert!(!refusal.contains("REF"), "REF leaked into {refusal}");
            assert!(
                !refusal.contains("Reflectivity"),
                "reflectivity leaked into {refusal}"
            );
        }
        app.vol3d.open = true;

        // A slice and an armed placement from the previously selected modeled
        // product must not remain on glass behind the refusal.
        app.xsection.armed = true;
        assert!(app.xsection.handle_pane_click((1.0, 2.0)));
        assert!(app.xsection.handle_pane_click((3.0, 4.0)));
        assert!(app.xsection.line.is_some());
        app.xsection.armed = true;

        let _ = app_frame(&mut app, &context, Vec::new());
        assert!(!app.xsection.armed);
        assert!(
            app.xsection.line.is_none(),
            "a modeled-product section line survived the source-field refusal"
        );
    }

    #[test]
    fn source_field_tilt_hover_reads_the_exact_grid_and_states_the_2d_limit() {
        let mut volume = Arc::try_unwrap(renderable_volume(1_700_000_000))
            .expect("the test volume has one owner");
        let gate_range = volume.cuts[0].radials[0].gate_range.clone();
        let radial_count = volume.cuts[0].radials.len();
        let moment = radar_core::MomentType::Unknown("VL1_CRR".to_owned());
        let mut grid = radar_core::MomentGrid::new_u8(
            moment.clone(),
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            None,
        );
        grid.producer_name = Some("VL1_CRR".to_owned());
        grid.producer_description = Some("verbatim producer description".to_owned());
        // Deliberately odd for this name: the hover must call this a token,
        // preserve it, and never use it to infer a canonical product.
        grid.producer_units = Some("dBm".to_owned());
        let row = vec![7_u8; gate_range.gate_count];
        for radial_index in 0..radial_count {
            grid.push_u8_row_slice(radial_index, &row)
                .expect("the exact source row fits its grid");
        }
        volume.cuts[0].moments.insert(moment, grid);

        let mut app = test_app();
        install(&mut app, Arc::new(volume));
        app.refresh_capabilities();
        app.apply_source_field_selection(first_pane(), "VL1_CRR");
        let hover = app.active_tilt_hover();

        assert!(
            hover.contains("Exact source field VL1_CRR · 2D only"),
            "{hover}"
        );
        assert!(
            hover.contains("Producer description: verbatim producer description"),
            "{hover}"
        );
        assert!(hover.contains("Producer unit token: dBm"), "{hover}");
        assert!(
            hover.contains("no modeled product was substituted"),
            "{hover}"
        );
        assert!(!hover.contains("REF"), "REF leaked into {hover}");
        assert!(
            !hover.contains("Reflectivity"),
            "reflectivity leaked into {hover}"
        );
    }

    #[test]
    fn an_absent_source_field_clears_the_stale_product_raster() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        pump_render(&mut app, &context);
        let pane = first_pane();
        assert!(app.panes[pane.index()].texture.is_some());

        app.apply_source_field_selection(pane, "ABSENT_NATIVE_FIELD");
        let volume = app
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
            .expect("a frame");
        app.ensure_render_requested(pane, volume, VIEWPORT);

        assert!(app.panes[pane.index()].texture.is_none());
        assert!(
            app.panes[pane.index()]
                .status
                .contains("ABSENT_NATIVE_FIELD unavailable")
        );
        assert_eq!(
            app.panes[pane.index()].terminal,
            Some(RenderTerminal::Unavailable(app.current_stamp(pane)))
        );
    }

    #[test]
    fn an_all_nodata_source_field_clears_the_stale_product_raster() {
        let context = egui::Context::default();
        let mut volume = Arc::try_unwrap(renderable_volume(1_700_000_000))
            .expect("the test volume has one owner");
        let cut = &mut volume.cuts[0];
        let gate_range = cut.radials[0].gate_range.clone();
        let moment = radar_core::MomentType::Unknown("EMPTY_NATIVE".to_owned());
        let mut empty = radar_core::MomentGrid::new_u8(
            moment.clone(),
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            None,
        );
        empty.producer_name = Some("EMPTY_NATIVE".to_owned());
        let row = vec![0_u8; gate_range.gate_count];
        for radial_index in 0..cut.radials.len() {
            empty
                .push_u8_row_slice(radial_index, &row)
                .expect("a nodata row fits the source grid");
        }
        cut.moments.insert(moment, empty);

        let mut app = test_app();
        install(&mut app, Arc::new(volume));
        pump_render(&mut app, &context);
        let pane = first_pane();
        assert!(app.panes[pane.index()].texture.is_some());

        app.apply_source_field_selection(pane, "EMPTY_NATIVE");
        let volume = app
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
            .expect("a frame");
        app.ensure_render_requested(pane, volume, VIEWPORT);

        assert!(app.panes[pane.index()].texture.is_none());
        assert!(
            app.panes[pane.index()]
                .status
                .contains("EMPTY_NATIVE has no finite values")
        );
        assert_eq!(
            app.panes[pane.index()].terminal,
            Some(RenderTerminal::Unavailable(app.current_stamp(pane)))
        );
    }

    /// §2.3: a render completed mid-gesture - stale only in its view
    /// generation - installs under its own camera instead of being discarded,
    /// while the exact-stamp render stays owed.
    #[test]
    fn a_render_completed_mid_gesture_installs_under_its_own_camera() {
        let context = egui::Context::default();
        let mut app = test_app();
        install(&mut app, renderable_volume(1_700_000_000));
        pump_render(&mut app, &context);
        let pane = first_pane();
        let (width, height) = {
            let texture = app.panes[pane.index()].texture.as_ref().expect("rendered");
            (texture.width, texture.height)
        };

        // The gesture: a pointer-move frame bumps the view clock.
        let old = app.current_stamp(pane);
        app.invalidate_view_panes(&[pane]);
        let current = app.current_stamp(pane);
        assert_ne!(old, current);
        app.panes[pane.index()].pending_stamp = Some(current);

        // The worker finishes the render it started BEFORE the bump.
        let camera = analyst_runtime::Camera2D {
            center_east_km: 12.5,
            ..analyst_runtime::Camera2D::default()
        };
        app.install_render(
            &context,
            RenderedPane {
                pane,
                stamp: old,
                camera,
                viewport: VIEWPORT,
                width,
                height,
                rgba: vec![9; width as usize * height as usize * 4],
                elapsed_ms: 2.0,
                gate_filter: render2d::GateFilterReport::INACTIVE,
            },
        );
        let texture = app.panes[pane.index()].texture.as_ref().expect("a texture");
        assert_eq!(
            texture.camera.center_east_km, 12.5,
            "the view-stale render was discarded - drag-start pixels for the whole gesture"
        );
        assert_eq!(texture.stamp, old, "it installs under its OWN stamp");
        assert_eq!(
            app.panes[pane.index()].pending_stamp,
            Some(current),
            "the exact-stamp render is still owed"
        );
        assert!(!app.visible_panes_ready(), "stale pixels are not readiness");

        // A render stale in its FRAME is still a different volume: discarded.
        let wrong_frame = RenderStamp {
            frame: analyst_runtime::Generation::new(9_999),
            ..app.current_stamp(pane)
        };
        let bogus_camera = analyst_runtime::Camera2D {
            center_east_km: -77.0,
            ..analyst_runtime::Camera2D::default()
        };
        app.install_render(
            &context,
            RenderedPane {
                pane,
                stamp: wrong_frame,
                camera: bogus_camera,
                viewport: VIEWPORT,
                width,
                height,
                rgba: vec![1; width as usize * height as usize * 4],
                elapsed_ms: 2.0,
                gate_filter: render2d::GateFilterReport::INACTIVE,
            },
        );
        let texture = app.panes[pane.index()].texture.as_ref().expect("a texture");
        assert_ne!(
            texture.camera.center_east_km, -77.0,
            "a frame-stale render must still be discarded"
        );
    }

    // --- §2.5: Vrot staleness wiring -----------------------------------------

    fn completed_vrot() -> crate::vrot::VrotState {
        let sample = |velocity_mps: f32, east_km: f64| crate::vrot::VrotSample {
            world_east_km: east_km,
            world_north_km: 0.0,
            velocity_mps,
            row: 1,
            gate: 2,
            slant_range_m: 10_000.0,
            beam_height_arl_m: 300.0,
            cut_index: 0,
            elevation_deg: 0.5,
        };
        crate::vrot::VrotState::Complete(
            crate::vrot::measure(sample(-30.0, 0.0), sample(30.0, 0.8), true)
                .expect("a 0.8 km dealiased couplet measures"),
        )
    }

    /// `mark_stale` had zero callers, so its own unit tests proved nothing
    /// about the application. This drives every frame-change site the review
    /// listed and checks the measurement is retired at each one.
    #[test]
    fn every_frame_change_site_retires_the_vrot_measurement() {
        use crate::vrot::StaleReason;
        let context = egui::Context::default();
        let mut app = test_app();
        let pane = first_pane();

        // A new volume installing between the two clicks.
        install(&mut app, renderable_volume(1_700_000_000));
        app.vrot_state = completed_vrot();
        app.vrot_pane = Some(pane);
        install(&mut app, renderable_volume(1_700_000_360));
        assert_eq!(
            app.vrot_state.stale_reason(),
            Some(StaleReason::NewVolume),
            "install_loaded_volume did not retire the measurement"
        );

        // A timeline scrub.
        app.vrot_state = completed_vrot();
        let before = app.current_frame_signature();
        assert!(app.history.select(0));
        app.commit_history_selection(before);
        assert_eq!(app.vrot_state.stale_reason(), Some(StaleReason::NewVolume));

        // A playback step.
        app.vrot_state = completed_vrot();
        pump_render(&mut app, &context);
        app.history.set_playback(PlaybackState::Playing);
        app.last_playback_step = Instant::now() - Duration::from_secs(2);
        app.advance_playback(&context);
        assert_eq!(
            app.vrot_state.stale_reason(),
            Some(StaleReason::NewVolume),
            "advance_playback did not retire the measurement"
        );

        // A product change on the Vrot pane: DVEL gates paired with SRV gates
        // would silently mix reference frames.
        app.vrot_state = completed_vrot();
        app.apply_product_selection(pane, DisplayProduct::DealiasedVelocity);
        assert_eq!(
            app.vrot_state.stale_reason(),
            Some(StaleReason::DifferentProduct)
        );

        // A new archive load may be any radar's file.
        app.vrot_state = completed_vrot();
        app.begin_load(PathBuf::from("Z:/definitely/not/here_V06"));
        assert_eq!(
            app.vrot_state.stale_reason(),
            Some(StaleReason::DifferentSite)
        );

        // A live start. The invalid site keeps the test off the network; the
        // mark happens before the session is even attempted, exactly like the
        // unconditional history clear beside it.
        app.vrot_state = completed_vrot();
        app.start_live("12".to_owned());
        assert_eq!(
            app.vrot_state.stale_reason(),
            Some(StaleReason::DifferentSite)
        );

        // A half-finished pair is discarded outright rather than completed
        // across two volumes.
        install(&mut app, renderable_volume(1_700_000_720));
        let pending = crate::vrot::VrotSample {
            world_east_km: 0.0,
            world_north_km: 0.0,
            velocity_mps: -30.0,
            row: 1,
            gate: 2,
            slant_range_m: 10_000.0,
            beam_height_arl_m: 300.0,
            cut_index: 0,
            elevation_deg: 0.5,
        };
        app.vrot_state = crate::vrot::VrotState::AwaitingSecond(pending);
        install(&mut app, renderable_volume(1_700_001_080));
        assert_eq!(app.vrot_state, crate::vrot::VrotState::Idle);
    }

    /// The same holds on real Level II volumes, real renders included.
    ///
    /// Ignored because it needs radars on disk: point
    /// `RADAR_WORKSTATION_REAL_LEVEL2` at a directory of real Archive II
    /// volumes (the live cache works). Run with:
    ///
    /// ```text
    /// cargo test --release -p workstation_app -- --ignored \
    ///     hold_and_gesture_installs_survive_real_volumes
    /// ```
    #[test]
    #[ignore = "set RADAR_WORKSTATION_REAL_LEVEL2 to a directory of real Archive II volumes"]
    fn hold_and_gesture_installs_survive_real_volumes() {
        let directory = std::env::var("RADAR_WORKSTATION_REAL_LEVEL2")
            .expect("set RADAR_WORKSTATION_REAL_LEVEL2 to a directory of volumes");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
            .expect("the directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| {
                path.metadata()
                    .map(|meta| meta.len() > 1_000_000)
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        // The two newest volumes of any site that has two.
        let site_of = |path: &PathBuf| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.chars().take(4).collect::<String>())
                .unwrap_or_default()
        };
        let mut by_site: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for path in paths {
            by_site.entry(site_of(&path)).or_default().push(path);
        }
        let mut of_site = by_site
            .into_values()
            .rfind(|paths| paths.len() >= 2)
            .expect("a site with two volumes");
        let newest = of_site.pop().expect("a newest volume");
        let previous = of_site.pop().expect("two volumes of one site");

        let context = egui::Context::default();
        let mut app = test_app();
        for path in [&previous, &newest] {
            let volume = nexrad_io::decode_volume_from_path(path)
                .unwrap_or_else(|error| panic!("{} did not decode: {error}", path.display()));
            install(&mut app, Arc::new(volume));
        }
        pump_render(&mut app, &context);
        let pane = first_pane();
        let held = app.panes[pane.index()]
            .texture
            .as_ref()
            .expect("the real volume rendered")
            .stamp;
        println!("rendered {} · held stamp {held:?}", newest.display());

        // Scrub: the picture holds, then swaps.
        let before = app.current_frame_signature();
        assert!(app.history.select(0));
        app.commit_history_selection(before);
        assert_eq!(
            app.panes[pane.index()].texture.as_ref().map(|t| t.stamp),
            Some(held),
            "the real frame was blanked on a scrub"
        );
        assert!(!app.visible_panes_ready());
        pump_render(&mut app, &context);
        assert_eq!(
            app.panes[pane.index()].texture.as_ref().map(|t| t.stamp),
            Some(app.current_stamp(pane)),
            "the replacement render never swapped in"
        );

        // Gesture: ask for a render, move the camera before it lands, and the
        // completed real render must still install.
        let volume = app
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
            .expect("a frame");
        app.invalidate_view_panes(&[pane]);
        app.ensure_render_requested(pane, volume, VIEWPORT);
        let requested = app.panes[pane.index()]
            .pending_stamp
            .expect("a render in flight");
        app.invalidate_view_panes(&[pane]); // the camera moved mid-render
        assert_ne!(requested, app.current_stamp(pane));
        let mut landed = false;
        for _ in 0..1_000 {
            app.poll_render_results(&context);
            if app.panes[pane.index()]
                .texture
                .as_ref()
                .is_some_and(|texture| texture.stamp == requested)
            {
                landed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            landed,
            "the render completed mid-gesture was discarded instead of installed"
        );
    }

    // --- how old is what I am looking at ------------------------------------
    //
    // The field failure, 2026-08-19: a live KUEX session drew a volume from the
    // previous Saturday under the day's warning polygons and said nothing but
    // "82 chunk(s) · 14.3 MiB · downloaded". Every time below is the real one -
    // the last volume KUEX ever published to the chunks bucket
    // (`KUEX/931/20260816-110802-003-I`, LastModified 2026-08-16T11:08:09Z) and
    // the KOAX volume that landed in the same cache at 16:27Z the same day.

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("a fixed instant")
            .with_timezone(&Utc)
    }

    /// The moment both feeds below were observed.
    fn observed_now() -> DateTime<Utc> {
        at("2026-08-19T16:27:00Z")
    }

    fn kuex_last_volume_time() -> DateTime<Utc> {
        at("2026-08-16T11:08:02Z")
    }

    fn koax_live_volume_time() -> DateTime<Utc> {
        at("2026-08-19T16:24:46Z")
    }

    /// A frame with a real site and a real volume time. No cuts: these tests
    /// are about what the instrument SAYS, and nothing here renders.
    fn frame_at(site: &str, volume_time: DateTime<Utc>) -> Arc<RadarVolume> {
        Arc::new(RadarVolume::new(
            radar_core::RadarSite::new(site),
            volume_time,
        ))
    }

    fn stalled_kuex_feed() -> LiveFeed {
        LiveFeed {
            site: "KUEX".to_owned(),
            newest_volume_time: kuex_last_volume_time(),
            freshness: FeedFreshness::Stalled,
        }
    }

    /// The same stalled feed, but aged off the REAL clock rather than off a
    /// fixed instant.
    ///
    /// Every other test here is handed an explicit `now` (see
    /// `observed_now`), so a frozen fixture is exactly right for them. The
    /// one that paints the toolbar cannot be: it drives the shipped
    /// `App::toolbar`, which reads `Utc::now()` itself, so a fixture pinned
    /// to a date gets one day older every day and the banner it asserts
    /// eventually reads a different number. That is not hypothetical - the
    /// "3 d old" assertion was written against a fixture three days old and
    /// began failing the morning the fixture turned four.
    ///
    /// Three days and an hour: comfortably inside the "3 d" bucket, which
    /// `format_age` floors, so the banner reads the same at any hour of any
    /// day this ever runs.
    fn stalled_kuex_feed_three_days_before_now() -> LiveFeed {
        LiveFeed {
            site: "KUEX".to_owned(),
            newest_volume_time: Utc::now() - TimeDelta::days(3) - TimeDelta::hours(1),
            freshness: FeedFreshness::Stalled,
        }
    }

    /// The ladder, one assertion per rung and one per boundary. These strings
    /// are read at a glance off a dark pane, so they are pinned exactly.
    #[test]
    fn an_age_reads_as_one_number_and_one_unit() {
        assert_eq!(format_age(TimeDelta::zero()), "0 s");
        assert_eq!(format_age(TimeDelta::seconds(42)), "42 s");
        assert_eq!(format_age(TimeDelta::seconds(59)), "59 s");
        assert_eq!(format_age(TimeDelta::seconds(60)), "1 min");
        assert_eq!(format_age(TimeDelta::minutes(6)), "6 min");
        assert_eq!(format_age(TimeDelta::minutes(59)), "59 min");
        assert_eq!(format_age(TimeDelta::minutes(60)), "1 h");
        assert_eq!(format_age(TimeDelta::hours(23)), "23 h");
        assert_eq!(format_age(TimeDelta::hours(24)), "1 d");
        assert_eq!(format_age(TimeDelta::days(3)), "3 d");
        // Floored, not rounded: "3 d" covers three days to just under four.
        assert_eq!(format_age(TimeDelta::hours(3 * 24 + 23)), "3 d");
        assert_eq!(format_age(TimeDelta::days(364)), "364 d");
        // Archive loads, so a 2013 case study does not read "4614 d old".
        assert_eq!(format_age(TimeDelta::days(365)), "1 y");
        // A radar clock a few seconds ahead of this machine's must not print a
        // negative age, which reads as a bug in the app rather than as skew.
        assert_eq!(format_age(TimeDelta::seconds(-4)), "0 s");
    }

    /// §1: the age is on the status line beside the Z time it comes from, and
    /// on every pane header, whether or not a live session is running.
    #[test]
    fn the_status_line_and_the_pane_header_both_carry_the_age_of_what_is_on_screen() {
        let mut app = test_app();
        install(&mut app, frame_at("KOAX", koax_live_volume_time()));
        let pane = first_pane();

        let status = app.timeline_status(observed_now());
        assert!(
            status.contains("2026-08-19T16:24:46Z"),
            "the Z time must stay: {status}"
        );
        assert!(
            status.ends_with("· 2 min old"),
            "the age must ride behind the Z time: {status}"
        );

        assert_eq!(
            app.pane_header_status(pane, DisplayProduct::default(), observed_now()),
            "2 min old"
        );

        // Same volume, two hours later: the number moves without anything new
        // arriving, which is what makes it a live quantity rather than a stamp.
        assert_eq!(
            app.pane_header_status(
                pane,
                DisplayProduct::default(),
                observed_now() + TimeDelta::hours(2)
            ),
            "2 h old"
        );
    }

    /// The render time is a developer's number and stays behind the analyst's.
    #[test]
    fn a_pane_header_puts_the_age_ahead_of_the_render_time() {
        let mut app = test_app();
        install(&mut app, frame_at("KOAX", koax_live_volume_time()));
        let pane = first_pane();
        app.panes[pane.index()].status = "12.3 ms".to_owned();

        assert_eq!(
            app.pane_header_status(pane, DisplayProduct::default(), observed_now()),
            "2 min old · 12.3 ms"
        );
    }

    /// §2: the field failure, in the words the app now uses for it.
    #[test]
    fn a_stalled_feed_names_the_site_the_state_and_the_age() {
        let mut app = test_app();
        app.live_site = Some("KUEX".to_owned());
        app.status = "Live KUEX".to_owned();
        app.live_feed = Some(stalled_kuex_feed());

        // The window before the first volume lands: the feed report arrives
        // ahead of the download, so the status line must already have stopped
        // saying "Live KUEX" by the time the transfer starts.
        assert_eq!(
            app.timeline_status(observed_now()),
            "KUEX feed stalled · newest data 3 d old"
        );

        install(&mut app, frame_at("KUEX", kuex_last_volume_time()));

        assert_eq!(
            app.live_stall_notice(observed_now()).as_deref(),
            Some("KUEX feed stalled · newest data 3 d old")
        );
        // The hover carries the exact instant, so "3 d" can be checked against
        // the bucket rather than believed.
        assert!(
            app.live_stall_hover().contains("2026-08-16T11:08:02Z"),
            "hover: {}",
            app.live_stall_hover()
        );
        // The pane header leads with the word, not just the number: "3 d old"
        // alone is also what a deliberate archive load looks like.
        assert_eq!(
            app.pane_header_status(first_pane(), DisplayProduct::default(), observed_now()),
            "STALLED · 3 d old"
        );
        assert!(app.live_feed_stalled(observed_now()));
    }

    /// The healthy feed on the same machine at the same instant. Without this
    /// the banner could be unconditional and every assertion above would still
    /// pass.
    /// The fallback path: the chunk feed is dead, the archive bucket is
    /// current, and every surface names the bucket instead of crying
    /// "stalled" over a forty-second-old volume.
    #[test]
    fn an_archive_fallback_names_the_bucket_not_a_stall() {
        let archive_volume_time = observed_now() - TimeDelta::seconds(40);
        let mut app = test_app();
        app.live_site = Some("KUEX".to_owned());
        app.status = "Live KUEX".to_owned();
        app.live_feed = Some(LiveFeed {
            site: "KUEX".to_owned(),
            newest_volume_time: archive_volume_time,
            freshness: FeedFreshness::ArchiveFallback,
        });
        install(&mut app, frame_at("KUEX", archive_volume_time));

        assert_eq!(
            app.live_stall_notice(observed_now()).as_deref(),
            Some("KUEX archive fallback · newest data 40 s old")
        );
        assert_eq!(
            app.pane_header_status(first_pane(), DisplayProduct::default(), observed_now()),
            "ARCHIVE FALLBACK · 40 s old"
        );
        assert_eq!(
            app.pane_badges(DisplayProduct::default(), observed_now())
                .first()
                .map(String::as_str),
            Some("ARCHIVE FALLBACK · 40 s OLD")
        );
        // The hover explains the bucket and must not hand out the dead-feed
        // advice: on the fallback, "pick another radar" is walking away from
        // data that is fine.
        let hover = app.live_stall_hover();
        assert!(hover.contains("archive bucket"), "hover: {hover}");
        assert!(!hover.contains("Pick another radar"), "hover: {hover}");
        assert!(app.live_feed_stalled(observed_now()));
    }

    #[test]
    fn a_feed_that_is_keeping_up_raises_no_banner_and_no_badge() {
        let mut app = test_app();
        app.live_site = Some("KOAX".to_owned());
        app.live_feed = Some(LiveFeed {
            site: "KOAX".to_owned(),
            newest_volume_time: koax_live_volume_time(),
            freshness: FeedFreshness::Current,
        });
        install(&mut app, frame_at("KOAX", koax_live_volume_time()));

        assert_eq!(app.live_stall_notice(observed_now()), None);
        assert!(!app.live_feed_stalled(observed_now()));
        assert_eq!(
            app.pane_header_status(first_pane(), DisplayProduct::default(), observed_now()),
            "2 min old"
        );
    }

    /// A feed report must not outlive the session that produced it: leaving one
    /// up would put a stall banner over a healthy radar, or - worse - clear one
    /// off a dead radar the analyst has just gone back to.
    #[test]
    fn the_stall_state_does_not_survive_a_site_change_or_a_local_file() {
        let mut app = test_app();
        app.live_site = Some("KUEX".to_owned());
        app.live_feed = Some(stalled_kuex_feed());

        app.stop_live();
        assert!(app.live_feed.is_none(), "stopping the session clears it");

        app.live_feed = Some(stalled_kuex_feed());
        app.begin_load(PathBuf::from("no-such-file_V06"));
        assert!(app.live_feed.is_none(), "a local file is not a feed");

        // And the site change the name promises, which this test did not
        // actually make before. The invalid id keeps it off the network:
        // `start_live` clears the old feed before it ever reaches
        // `live_service`, and that ordering is the point - the banner goes down
        // with the radar it described, not a poll later over the new one.
        app.live_feed = Some(stalled_kuex_feed());
        app.start_live("12".to_owned());
        assert!(
            app.live_feed.is_none(),
            "a new site inherits nothing from the old one's feed"
        );
    }

    /// The failure the poll thread cannot report. `live_service` classifies
    /// once per listing and publishes only on change, so anything that stops
    /// the listing - a dropped network, a bucket answering 500, a poll thread
    /// that died - leaves the app holding a verdict that was true when it was
    /// made. Time passes anyway. The instrument has to reach the conclusion on
    /// its own, or the silence comes back by another road.
    #[test]
    fn a_feed_reported_current_goes_stalled_on_wall_clock_alone() {
        let mut app = test_app();
        app.live_site = Some("KOAX".to_owned());
        // Exactly what a healthy KOAX poll reported at 16:27Z - and then no
        // further report ever arrives.
        app.live_feed = Some(LiveFeed {
            site: "KOAX".to_owned(),
            newest_volume_time: koax_live_volume_time(),
            freshness: FeedFreshness::Current,
        });
        install(&mut app, frame_at("KOAX", koax_live_volume_time()));

        assert!(!app.live_feed_stalled(observed_now()));
        assert_eq!(app.live_stall_notice(observed_now()), None);

        // Fourteen minutes on: still a clear-air VCP plus publication latency,
        // and still not a stall.
        let quiet = koax_live_volume_time() + TimeDelta::minutes(14);
        assert!(!app.live_feed_stalled(quiet), "14 min is not a stall");

        // Past the threshold, with nothing having arrived to say so.
        let stalled_at = koax_live_volume_time()
            + TimeDelta::seconds(data_source::REALTIME_FEED_STALL_AFTER_SECONDS);
        assert!(
            app.live_feed_stalled(stalled_at),
            "the app must reach the verdict itself when no report comes"
        );
        assert_eq!(
            app.live_stall_notice(stalled_at).as_deref(),
            Some("KOAX feed stalled · newest data 15 min old")
        );
        assert_eq!(
            app.pane_badges(DisplayProduct::default(), stalled_at)
                .first()
                .map(String::as_str),
            Some("FEED STALLED · 15 min OLD")
        );
    }

    /// The on-glass half of the report. It lived inside the paint loop, where
    /// no test could reach it and deleting it would have broken nothing. It
    /// leads the stack because `legend::MAX_BADGES` truncates from the end.
    #[test]
    fn the_stall_badge_leads_the_badge_stack_and_names_the_age() {
        let mut app = test_app();
        app.live_site = Some("KUEX".to_owned());
        app.live_feed = Some(stalled_kuex_feed());
        // Partial, so there is a second badge for the stall to be ahead of.
        install_partial(&mut app, frame_at("KUEX", kuex_last_volume_time()));

        let badges = app.pane_badges(DisplayProduct::default(), observed_now());
        assert_eq!(
            badges.first().map(String::as_str),
            Some("FEED STALLED · 3 d OLD"),
            "badges: {badges:?}"
        );
        assert!(
            badges.iter().any(|badge| badge == "PARTIAL"),
            "the partial badge must still be behind it: {badges:?}"
        );

        // The healthy feed on the same machine raises nothing, so the badge is
        // a measurement rather than the only thing this can produce.
        app.live_feed = Some(LiveFeed {
            site: "KOAX".to_owned(),
            newest_volume_time: koax_live_volume_time(),
            freshness: FeedFreshness::Current,
        });
        let badges = app.pane_badges(DisplayProduct::default(), observed_now());
        assert!(
            !badges.iter().any(|badge| badge.contains("STALLED")),
            "badges: {badges:?}"
        );
    }

    /// LOOK AT IT, without a human squinting at a PNG.
    ///
    /// Runs the SHIPPED `toolbar` through egui in both variants and reads the
    /// stall banner back out of the shapes it emitted: the text that was laid
    /// out, the ink each glyph carries, and the fill of the last opaque rect
    /// painted under the place those glyphs land. Dark ink on a dark ground is
    /// a colour pair, and a colour pair is measurable - which is the whole
    /// difference between this and believing a string helper returned
    /// something.
    ///
    /// Both variants, because the failure photographed in the field was
    /// variant-specific: near-black ink that is right on the light palette and
    /// invisible on the dark one.
    #[test]
    fn the_shipped_toolbar_paints_the_stall_banner_legibly_in_both_variants() {
        use crate::theme::palette::{DARK, LIGHT, Palette};
        use crate::theme::{Appearance, apply};

        fn luminance(color: egui::Color32) -> f64 {
            fn channel(byte: u8) -> f64 {
                let u = f64::from(byte) / 255.0;
                if u <= 0.04045 {
                    u / 12.92
                } else {
                    ((u + 0.055) / 1.055).powf(2.4)
                }
            }
            0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
        }
        // WCAG 2.2 SC 1.4.3.
        fn contrast(a: egui::Color32, b: egui::Color32) -> f64 {
            let (la, lb) = (luminance(a), luminance(b));
            let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
            (hi + 0.05) / (lo + 0.05)
        }

        /// Every text run in the frame, flattened - `Shape::Vec` nests, and a
        /// banner hiding inside one would otherwise read as "not painted".
        fn walk(
            shape: &egui::Shape,
            rects: &mut Vec<(egui::Rect, egui::Color32)>,
            found: &mut Vec<(String, egui::Color32, Option<egui::Color32>, bool)>,
        ) {
            match shape {
                egui::Shape::Rect(rect) => rects.push((rect.rect, rect.fill)),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, rects, found);
                    }
                }
                egui::Shape::Text(text) => {
                    let ink = text.override_text_color.unwrap_or_else(|| {
                        let section = text
                            .galley
                            .job
                            .sections
                            .first()
                            .map(|section| section.format.color)
                            .unwrap_or(egui::Color32::PLACEHOLDER);
                        if section == egui::Color32::PLACEHOLDER {
                            text.fallback_color
                        } else {
                            section
                        }
                    });
                    // The centre of the laid-out run, and the last OPAQUE rect
                    // painted under it: that is the ground its pixels landed
                    // on, read out of the frame rather than assumed.
                    let anchor = text.pos + text.galley.rect.center().to_vec2();
                    let ground = rects
                        .iter()
                        .rev()
                        .find(|(rect, fill)| rect.contains(anchor) && fill.a() == 255)
                        .map(|(_, fill)| *fill);
                    // `elided` and not the string: a truncated galley still
                    // reports the job's full text, so a banner cut to
                    // "KUEX feed stalled ..." would read as intact here.
                    found.push((
                        text.galley.text().to_owned(),
                        ink,
                        ground,
                        text.galley.elided,
                    ));
                }
                _ => {}
            }
        }

        for (variant, palette) in [("light", &LIGHT), ("dark", &DARK)] {
            let context = egui::Context::default();
            apply(&context, &Appearance::by_id(variant));
            let mut app = WorkstationApp::with_context(
                context.clone(),
                None,
                None,
                WarningsSource::Daemon {
                    base_url: "http://127.0.0.1:9".to_owned(),
                },
                test_settings_store(),
            );
            app.live_site = Some("KUEX".to_owned());
            // The line the banner has to contradict, in the words the analyst
            // was actually shown on 2026-08-19.
            app.live_status = "82 chunk(s) · 14.3 MiB · downloaded".to_owned();
            // Aged off the real clock: this test paints the real bar, which asks
            // the real clock what time it is. Pinning an instant instead of an age
            // made this test fail on 2026-08-20 for calendar reasons alone - the
            // banner it asserts silently went from "3 d old" to "4 d old".
            app.live_feed = Some(stalled_kuex_feed_three_days_before_now());

            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(1600.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| app.toolbar(ui),
            );

            let mut rects = Vec::new();
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                walk(&clipped.shape, &mut rects, &mut texts);
            }

            let (text, ink, ground, elided) = texts
                .iter()
                .find(|(text, _, _, _)| text.contains("feed stalled"))
                .unwrap_or_else(|| {
                    panic!(
                        "{variant:?}: the toolbar painted no stall banner. It emitted: {:?}",
                        texts.iter().map(|(text, _, _, _)| text).collect::<Vec<_>>()
                    )
                });

            assert_eq!(text, "KUEX feed stalled · newest data 3 d old");
            assert!(
                !elided,
                "{variant:?}: the readout's width cap truncated the banner"
            );
            // The theme's error ink, which is not the ink every other readout
            // on the bar uses. A banner in the same grey as the line it
            // contradicts is not a banner.
            assert_eq!(*ink, palette.error, "{variant:?}: wrong ink");
            assert_ne!(*ink, palette.text, "{variant:?}: the banner is not loud");

            let ground = ground
                .unwrap_or_else(|| panic!("{variant:?}: the banner has no opaque ground under it"));
            assert_eq!(ground, palette.well, "{variant:?}: not in a readout well");
            let ratio = contrast(*ink, ground);
            assert!(
                ratio >= 4.5,
                "{variant:?}: the banner is {ratio:.2}:1 on the ground it landed on \
                 ({ink:?} on {ground:?}), under the 4.5:1 floor"
            );
            // Printed so a human reading the run sees the numbers, not a claim.
            println!("{variant:?}: {text:?} at {ratio:.2}:1 ({ink:?} on {ground:?})");

            // And it sits beside the line it contradicts rather than replacing
            // it: "82 chunk(s) ... downloaded" is still true and still shown.
            assert!(
                texts
                    .iter()
                    .any(|(text, _, _, _)| text.contains("82 chunk(s)")),
                "{variant:?}: the downloaded readout vanished"
            );

            // The negative, on the same bar in the same variant: a feed that is
            // keeping up paints no banner at all.
            app.live_feed = Some(LiveFeed {
                site: "KOAX".to_owned(),
                newest_volume_time: Utc::now(),
                freshness: FeedFreshness::Current,
            });
            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(1600.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| app.toolbar(ui),
            );
            let mut rects = Vec::new();
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                walk(&clipped.shape, &mut rects, &mut texts);
            }
            assert!(
                !texts.iter().any(|(text, _, _, _)| text.contains("stalled")),
                "{variant:?}: a healthy feed raised a banner"
            );

            // The WIDEST banner this format can produce - "59 min" is the
            // longest age string, longer than the "3 d" the field failure
            // happened to make - so the width cap is checked at the case that
            // would actually hit it rather than at a short one.
            app.live_feed = Some(LiveFeed {
                site: "KUEX".to_owned(),
                newest_volume_time: Utc::now() - TimeDelta::seconds(59 * 60 + 30),
                freshness: FeedFreshness::Stalled,
            });
            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(1600.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| app.toolbar(ui),
            );
            let mut rects = Vec::new();
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                walk(&clipped.shape, &mut rects, &mut texts);
            }
            let (text, _ink, _ground, elided) = texts
                .iter()
                .find(|(text, _, _, _)| text.contains("feed stalled"))
                .expect("the widest banner");
            assert_eq!(text, "KUEX feed stalled · newest data 59 min old");
            assert!(!elided, "{variant:?}: the widest banner is truncated");
            println!("{variant:?}: widest banner {text:?} fits");
            let _ = Palette::detect;
        }
    }

    /// An age is a wall-clock quantity drawn on a screen that repaints only
    /// when something wakes it. This is the wake, and without it the number an
    /// analyst is asked to trust sits frozen between volumes.
    #[test]
    fn the_age_readout_wakes_the_app_before_the_string_it_drew_would_change() {
        // Under a minute the string changes every second.
        assert_eq!(
            age_repaint_interval(TimeDelta::zero()),
            Duration::from_secs(1)
        );
        assert_eq!(
            age_repaint_interval(TimeDelta::seconds(59)),
            Duration::from_secs(1)
        );
        // On and above the minute it sleeps to the minute boundary, so "6 min"
        // becomes "7 min" the moment that is true rather than a frame later.
        assert_eq!(
            age_repaint_interval(TimeDelta::seconds(60)),
            Duration::from_secs(60)
        );
        assert_eq!(
            age_repaint_interval(TimeDelta::seconds(61)),
            Duration::from_secs(59)
        );
        assert_eq!(
            age_repaint_interval(TimeDelta::minutes(6) + TimeDelta::seconds(30)),
            Duration::from_secs(30)
        );
        // Never zero and never long, at any band: zero is a 60 Hz spin over a
        // picture that cannot change, and a long sleep in the hour or day band
        // would also delay the moment the stall threshold is crossed.
        for age in [
            TimeDelta::hours(3),
            TimeDelta::days(3),
            TimeDelta::days(3) + TimeDelta::seconds(59),
            TimeDelta::seconds(-30),
        ] {
            let interval = age_repaint_interval(age);
            assert!(
                interval >= Duration::from_secs(1) && interval <= Duration::from_secs(60),
                "{age:?} asked for {interval:?}"
            );
        }
    }

    /// PROVE ON REAL DATA. Reads the two volumes the live cache actually holds:
    /// `KUEX20260816_110802_RT931_V06`, the three-day-old fragment that started
    /// this, and the newest KOAX volume beside it. Decodes both, and prints
    /// every string the instrument would show for each.
    ///
    /// Judged twice per site, and both judgements matter:
    ///
    /// * at the instant the file was FETCHED (its mtime), which is what the
    ///   analyst saw at the time - KUEX already three days behind, KOAX
    ///   minutes behind;
    /// * at wall clock now, where a cache nobody is feeding has itself gone
    ///   stale, which is the correct answer and not a second bug.
    ///
    /// Ignored because it depends on this machine's live cache, which is not
    /// checked in and is pruned as it grows. A test that silently passed when
    /// the files were missing would be worse than no test. Run it with:
    ///
    /// ```text
    /// cargo test --release -p workstation_app --bin GenericRadar -- \
    ///     --ignored --nocapture the_real_cached_volumes
    /// ```
    #[test]
    #[ignore = "reads this machine's real live cache"]
    fn the_real_cached_volumes_produce_the_status_strings_by_hand() {
        let cache = default_live_cache_dir();

        for site in ["KUEX", "KOAX"] {
            let path = newest_cached_volume(&cache, site)
                .unwrap_or_else(|| panic!("no cached {site} volume under {}", cache.display()));
            let fetched_at: DateTime<Utc> = path
                .metadata()
                .and_then(|metadata| metadata.modified())
                .expect("the cached file has an mtime")
                .into();
            let volume = Arc::new(
                nexrad_io::decode_volume_from_path(&path).expect("the cached volume decodes"),
            );
            let volume_time = volume.volume_time;

            println!("--- {site} ---");
            println!("file          {}", path.display());
            println!(
                "volume time   {}",
                volume_time.to_rfc3339_opts(SecondsFormat::Secs, true)
            );
            for (label, now) in [("when fetched", fetched_at), ("now", Utc::now())] {
                let freshness =
                    data_source::classify_feed_age(data_source::volume_age_at(volume_time, now));
                let mut app = test_app();
                app.live_site = Some(site.to_owned());
                app.live_feed = Some(LiveFeed {
                    site: site.to_owned(),
                    newest_volume_time: volume_time,
                    freshness,
                });
                install(&mut app, Arc::clone(&volume));
                app.panes[first_pane().index()].status = "12.3 ms".to_owned();

                println!(
                    "  [{label}] judged at {} · {freshness:?}",
                    now.to_rfc3339_opts(SecondsFormat::Secs, true)
                );
                println!("    status line   {}", app.timeline_status(now));
                println!(
                    "    pane header   {}",
                    app.pane_header_status(first_pane(), DisplayProduct::default(), now)
                );
                println!(
                    "    stall banner  {}",
                    app.live_stall_notice(now)
                        .unwrap_or_else(|| "(none)".to_owned())
                );
            }
            println!();
        }
    }

    // --- the gate filter's safety rule --------------------------------------
    //
    // A filter that hides weather must never be inferable only from the
    // absence of echo. These drive the REAL application - its own
    // `eframe::App::ui` pass and its own `toolbar` - and read the words back
    // out of the shapes it emitted, so they measure what an analyst would
    // actually see rather than what a helper returns.

    use crate::gate_filter_ui::{FILTERED_WORD, FilterValues, PRESETS};

    fn storm_mode() -> FilterValues {
        PRESETS
            .iter()
            .find(|preset| preset.id == "storm")
            .expect("storm mode is declared")
            .values
    }

    /// An application whose settings file already carries these criteria, as
    /// if it had been closed and reopened on them.
    fn app_with_filter(
        context: &egui::Context,
        values: FilterValues,
        toolbar: &str,
    ) -> WorkstationApp {
        let mut store = test_settings_store();
        crate::gate_filter_ui::write_values(&mut store, values);
        store.set(
            crate::settings_ui::catalog::keys::appearance::CATEGORY,
            crate::settings_ui::catalog::keys::appearance::TOOLBAR,
            settings::SettingValue::Text(toolbar.to_owned()),
        );
        WorkstationApp::with_context(
            context.clone(),
            None,
            None,
            WarningsSource::Daemon {
                base_url: "http://127.0.0.1:9".to_owned(),
            },
            store,
        )
    }

    fn shape_texts(shapes: &[egui::Shape]) -> Vec<String> {
        fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => {
                    let text = text.galley.text().trim();
                    if !text.is_empty() {
                        found.push(text.to_owned());
                    }
                }
                egui::Shape::Vec(nested) => {
                    for shape in nested {
                        walk(shape, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        for shape in shapes {
            walk(shape, &mut found);
        }
        found
    }

    /// One whole application frame, through the shipped `eframe::App::ui`.
    fn app_frame(
        app: &mut WorkstationApp,
        context: &egui::Context,
        events: Vec<egui::Event>,
    ) -> Vec<egui::Shape> {
        let mut frame = eframe::Frame::_new_kittest();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1400.0, 900.0),
                )),
                events,
                ..Default::default()
            },
            |ui| <WorkstationApp as eframe::App>::ui(app, ui, &mut frame),
        );
        output
            .shapes
            .into_iter()
            .map(|clipped| clipped.shape)
            .collect()
    }

    fn pointer(position: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    /// Where a text run that IS `wanted` landed, rather than one containing
    /// it.
    ///
    /// The clear key is a single `×`, and several other runs on a full frame
    /// contain that character - a supersampling label, a scale readout - so
    /// the needle form would find whichever came first in the shape list.
    fn exact_text_position(shapes: &[egui::Shape], wanted: &str) -> Option<egui::Pos2> {
        fn walk(shape: &egui::Shape, wanted: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.text().trim() == wanted => {
                    // In a horizontal wrapping layout egui represents a label
                    // with one galley whose rect includes the blank lead-in
                    // occupied by controls earlier on its first row. The
                    // galley rect's center can therefore be nowhere near the
                    // painted glyphs (and outside the label's hover response).
                    // Target the tight painted bounds, which remain true when
                    // earlier controls move the label across a row seam.
                    Some(text.visual_bounding_rect().center())
                }
                egui::Shape::Vec(nested) => nested.iter().find_map(|shape| walk(shape, wanted)),
                _ => None,
            }
        }
        shapes.iter().find_map(|shape| walk(shape, wanted))
    }

    /// THE safety pin. A pane that is hiding gates says so, naming what it is
    /// hiding; a pane that is hiding nothing says nothing.
    ///
    /// Asserted on a full application frame rather than on
    /// `gate_filter_ui::pane_status_line`, because the failure this guards
    /// against is not a wrong string - it is a right string that never reaches
    /// the glass, which is what deleting one line of `canvas` would produce
    /// while every unit test stayed green.
    ///
    /// The loud indicator this used to read - a full-width band under the
    /// header - no longer exists. The pin did not move with it: the same
    /// claim is now made against the pane HEADER, which
    /// is drawn unconditionally and which `pane_header_status` builds from the
    /// settings cache, so it is on the glass from the frame a criterion is set
    /// rather than from the frame the render lands.
    #[test]
    fn a_pane_says_filtered_exactly_when_it_is_hiding_gates() {
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
        let mut quiet = app_with_filter(&context, FilterValues::OFF, "menus");
        // Two passes: the first builds the font atlas, and a pane is never a
        // session's first frame.
        app_frame(&mut quiet, &context, Vec::new());
        let texts = shape_texts(&app_frame(&mut quiet, &context, Vec::new()));
        assert!(
            !texts.iter().any(|text| text.contains(FILTERED_WORD)),
            "an unfiltered application put {FILTERED_WORD:?} on the glass: {texts:?}"
        );

        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
        let mut filtered = app_with_filter(&context, storm_mode(), "menus");
        app_frame(&mut filtered, &context, Vec::new());
        let texts = shape_texts(&app_frame(&mut filtered, &context, Vec::new()));
        // The pane header: the statement no setting can switch off, drawn by
        // `canvas` at the right-hand end of every pane's title row.
        let statement = texts
            .iter()
            .find(|text| text.starts_with(&format!("{FILTERED_WORD}:")))
            .unwrap_or_else(|| {
                panic!(
                    "a filtered pane made no {FILTERED_WORD} statement - the only evidence \
                     left would be the missing echo itself: {texts:?}"
                )
            });
        assert!(
            statement.contains("REF below 20 dBZ"),
            "the header does not name what it hides: {statement:?}"
        );
        // And the legend's own copy, beside the colour bar where the analyst
        // is already reading.
        assert!(
            texts.iter().any(|text| text == FILTERED_WORD),
            "the legend badge stack carries no filter badge: {texts:?}"
        );
    }

    /// The pane keeps saying so with the colour legend switched off.
    ///
    /// This is the hole the band's removal opened, pinned shut on a full
    /// application frame. The band was unconditional precisely because the
    /// legend - and with it the one-word badge - can be turned off in
    /// Settings; the statement had to land somewhere that setting cannot
    /// reach, and it landed on the header.
    ///
    /// The legend badge is asserted absent as well as the statement present,
    /// so this cannot pass by the legend quietly ignoring the setting.
    #[test]
    fn a_filtered_pane_with_no_colour_legend_still_says_so() {
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
        let mut app = app_with_filter(&context, storm_mode(), "menus");
        app.settings_store.set(
            crate::settings_ui::catalog::keys::radar::CATEGORY,
            crate::settings_ui::catalog::keys::radar::LEGEND,
            settings::SettingValue::Bool(false),
        );
        app.recompute_settings_cache();
        assert!(
            !app.settings_cache.legend,
            "the legend is still switched on"
        );

        app_frame(&mut app, &context, Vec::new());
        let texts = shape_texts(&app_frame(&mut app, &context, Vec::new()));
        assert!(
            !texts.iter().any(|text| text == FILTERED_WORD),
            "the legend badge survived the legend being switched off, so this proves \
             nothing about the header: {texts:?}"
        );
        let statement = texts
            .iter()
            .find(|text| text.starts_with(&format!("{FILTERED_WORD}:")))
            .unwrap_or_else(|| {
                panic!(
                    "with the colour legend off, a filtered pane said nothing at all - a \
                     setting about a colour bar switched off the admission that gates are \
                     being hidden: {texts:?}"
                )
            });
        assert!(
            statement.contains("REF below 20 dBZ"),
            "the header does not name what it hides: {statement:?}"
        );
    }

    /// The statement is on the glass BEFORE any render has landed.
    ///
    /// The engine's line - counts and all - only exists once a worker has
    /// answered, and `PaneRuntime::filter_line` is empty until then. The band
    /// covered that window because it was built from the settings; the header
    /// inherits that, and this is what says so. A header built only from the
    /// render result would leave a pane reading nothing at all for as long as
    /// the render takes, and for ever on a pane whose product is unavailable.
    #[test]
    fn a_pane_says_filtered_before_any_render_has_answered() {
        let context = egui::Context::default();
        let app = app_with_filter(&context, storm_mode(), "menus");
        let pane = first_pane();
        assert!(
            app.panes[pane.index()].filter_line.is_none(),
            "this pane has already rendered, so the window under test is not open"
        );
        let status = app.pane_header_status(pane, DisplayProduct::default(), chrono::Utc::now());
        assert!(
            status.starts_with(FILTERED_WORD),
            "a pane with no render behind it said {status:?}"
        );
        assert!(status.contains("REF below 20 dBZ"), "{status:?}");
    }

    /// The header quotes the ENGINE once the engine has answered, and refuses
    /// to quote a report about criteria that are no longer switched on.
    ///
    /// Both halves are the same mechanism seen from two sides. The engine's
    /// line is the one an analyst wants - it carries the counts - but it
    /// describes the filter the worker ran, so a header that printed it
    /// unconditionally would show one filter's numbers under another filter's
    /// picture for the frames between a criterion moving and the new render
    /// landing.
    #[test]
    fn the_header_takes_the_engines_counts_only_while_they_still_describe_the_filter() {
        let context = egui::Context::default();
        let mut app = app_with_filter(&context, storm_mode(), "menus");
        let pane = first_pane();
        let now = chrono::Utc::now();

        // What the worker would have installed for this filter.
        let landed = render2d::GateFilterReport {
            filter: app.settings_cache.gate_filter,
            gates_visible: 298_195,
            gates_hidden: 269_740,
            ..render2d::GateFilterReport::INACTIVE
        }
        .badge()
        .expect("the engine reports an active filter");
        app.panes[pane.index()].filter_line = Some(landed.clone());
        let status = app.pane_header_status(pane, DisplayProduct::default(), now);
        assert!(
            status.starts_with(&landed),
            "the header dropped the engine's own counts: {status:?}"
        );
        assert!(status.contains("269,740 of 298,195"), "{status:?}");

        // Now the analyst moves a criterion. The installed report is about the
        // old one until a new render lands.
        apply_filter_through_settings(&mut app, FilterValues::OFF);
        let status = app.pane_header_status(pane, DisplayProduct::default(), now);
        assert!(
            !status.contains(FILTERED_WORD),
            "the filter is off and the header still quotes a filtered render: {status:?}"
        );

        let mut loosened = storm_mode();
        loosened.min_dbz += 10.0;
        apply_filter_through_settings(&mut app, loosened);
        let status = app.pane_header_status(pane, DisplayProduct::default(), now);
        assert!(
            !status.contains("269,740"),
            "the header printed one filter's counts under another filter's criteria: \
             {status:?}"
        );
        assert!(
            status.contains(&app.settings_cache.gate_filter.hidden_summary()),
            "the header stopped naming what is being hidden: {status:?}"
        );
    }

    /// The filter statement is built ahead of the age and the stall word, so a
    /// narrow pane truncates those and not this.
    ///
    /// The order is the safety rule rather than a preference, and it is only
    /// load-bearing because the band is gone: this row is the pane's whole
    /// account of what is missing, and `pane_canvas::header_galleys`
    /// truncates it from the right.
    #[test]
    fn the_filter_statement_comes_first_on_the_header_row() {
        let context = egui::Context::default();
        let mut app = app_with_filter(&context, storm_mode(), "menus");
        install(&mut app, weak_echo_volume());
        let pane = first_pane();
        app.panes[pane.index()].status = "27.5 ms".to_owned();
        // Old enough that the age and the stall word are both in the row.
        let now = observed_now() + TimeDelta::hours(19);
        let status = app.pane_header_status(pane, DisplayProduct::default(), now);
        assert!(
            status.starts_with(FILTERED_WORD),
            "the header buries its filter statement behind {status:?}"
        );
        assert!(
            status.contains("old"),
            "this row carries no age, so the ordering claim is vacuous: {status:?}"
        );
    }

    /// The same claim for the toolbar chip, in BOTH toolbar styles: neither of
    /// the two supported bars may be the one that stays quiet.
    #[test]
    fn the_toolbar_chip_names_the_filter_in_both_toolbar_styles() {
        for style in ["menus", "full"] {
            let context = egui::Context::default();
            crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
            let mut quiet = app_with_filter(&context, FilterValues::OFF, style);
            let texts = toolbar_texts(&mut quiet);
            assert!(
                texts.iter().any(|text| text.starts_with("Filter: off")),
                "{style}: the bar has no gate-filter control at all: {texts:?}"
            );

            let mut filtered = app_with_filter(&context, storm_mode(), style);
            let texts = toolbar_texts(&mut filtered);
            assert!(
                texts.iter().any(|text| text.contains("Storm mode")),
                "{style}: the bar does not say which filter is on: {texts:?}"
            );
            assert!(
                !texts.iter().any(|text| text.starts_with("Filter: off")),
                "{style}: the bar still reads off while gates are hidden: {texts:?}"
            );
        }
    }

    /// THE one obvious action out, exercised where it now lives: the clear key
    /// on the toolbar, in BOTH toolbar styles.
    ///
    /// It used to be the pane's FILTERED band, which took the click where the
    /// evidence was. With the band gone the escape moved to the bar, beside
    /// the chip that turned the filter on - so this is the same pin with a new
    /// subject, and it is made in both bars because neither of the two
    /// supported toolbars may be the one with no way out of a filtered view.
    ///
    /// Driven through the shipped `eframe::App::ui` with real pointer events,
    /// like the band test before it: the claim is that an analyst can hit it,
    /// not that a function exists.
    #[test]
    fn clicking_the_toolbars_clear_key_shows_everything_again() {
        for style in ["menus", "full"] {
            let context = egui::Context::default();
            crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
            let mut app = app_with_filter(&context, storm_mode(), style);
            app_frame(&mut app, &context, Vec::new());
            let shapes = app_frame(&mut app, &context, Vec::new());
            assert!(app.settings_cache.gate_filter.is_active());
            let key = exact_text_position(&shapes, crate::gate_filter_ui::CLEAR_GLYPH)
                .unwrap_or_else(|| {
                    panic!(
                        "{style}: a filtered bar offers no way out at all: {:?}",
                        shape_texts(&shapes)
                    )
                });

            app_frame(&mut app, &context, pointer(key, true));
            app_frame(&mut app, &context, pointer(key, false));

            assert_eq!(
                app.settings_cache.gate_filter,
                render2d::GateFilter::OFF,
                "{style}: the clear key did not clear the filter"
            );
            let texts = shape_texts(&app_frame(&mut app, &context, Vec::new()));
            assert!(
                !texts.iter().any(|text| text.contains(FILTERED_WORD)),
                "{style}: the statement outlived the filter it was about: {texts:?}"
            );
            assert!(
                !texts
                    .iter()
                    .any(|text| text == crate::gate_filter_ui::CLEAR_GLYPH),
                "{style}: the clear key outlived the filter it cleared: {texts:?}"
            );
            assert!(
                app.status.contains("cleared"),
                "{style}: nothing confirmed the click, and the key is inches from the \
                 panes whose pictures it changed: {:?}",
                app.status
            );
        }
    }

    /// The clear key is offered ONLY while there is something to clear.
    ///
    /// The other half of the claim above, and the one that keeps an unfiltered
    /// session's bar exactly the bar this application has always drawn: a dead
    /// key beside the chip would be furniture, and a live one would be an
    /// action with no subject.
    #[test]
    fn an_unfiltered_toolbar_offers_no_clear_key() {
        for style in ["menus", "full"] {
            let context = egui::Context::default();
            crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
            let mut app = app_with_filter(&context, FilterValues::OFF, style);
            let texts = toolbar_texts(&mut app);
            assert!(
                !texts
                    .iter()
                    .any(|text| text == crate::gate_filter_ui::CLEAR_GLYPH),
                "{style}: an unfiltered bar drew a clear key: {texts:?}"
            );
        }
    }

    /// Persistence, including the preset's identity: the numbers on disk are
    /// what name the preset, so a reopen cannot land on Custom.
    #[test]
    fn a_reopened_application_comes_back_on_the_same_named_preset() {
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
        let storm = PRESETS
            .iter()
            .find(|preset| preset.id == "storm")
            .expect("storm mode is declared");
        let mut app = app_with_filter(&context, storm.values, "menus");
        assert_eq!(app.settings_cache.gate_filter, storm.values.to_filter());
        let texts = toolbar_texts(&mut app);
        assert!(
            texts.iter().any(|text| text.contains(storm.label)),
            "the reopened bar reads {:?} rather than {:?}",
            texts,
            storm.label
        );
        assert!(
            !texts
                .iter()
                .any(|text| text.contains(crate::gate_filter_ui::CUSTOM_LABEL)),
            "the reopened bar calls a shipped preset Custom: {texts:?}"
        );
    }

    /// A fresh install renders what it always rendered.
    #[test]
    fn a_fresh_application_carries_no_filter_at_all() {
        let app = test_app();
        assert_eq!(app.settings_cache.gate_filter, render2d::GateFilter::OFF);
        assert!(!app.settings_cache.gate_filter.is_active());
        assert_eq!(
            SettingsCache::default().gate_filter,
            render2d::GateFilter::OFF
        );
    }

    /// The filter is a citizen of the settings window, not a stowaway.
    ///
    /// The five criteria are reachable from both their toolbar panel and the
    /// settings window, including search, per-setting and per-page reset, and
    /// named profiles. A criterion the window cannot find, cannot reset, or
    /// quietly drops out of a profile is a criterion that hides weather and
    /// then refuses to account for it.
    ///
    /// Every assertion is made against `settings_cache.gate_filter` - the
    /// value `ensure_render_requested` puts on the `RenderRequest` and the
    /// band, the badge and the readout are all built from - rather than
    /// against the stored number. Reading the store back would pass even if
    /// the window and the engine had come apart.
    ///
    /// The profile half goes through the application's own two calls, with
    /// the same guard `settings_frame` uses: the recompute only runs if the
    /// switch reported a change, so a switch that reaches the file and stops
    /// there fails here instead of passing on a recompute this test did for
    /// it.
    #[test]
    fn a_gate_filter_criterion_is_searchable_resettable_and_carried_by_a_profile() {
        use crate::settings_ui::catalog::keys::radar as k;
        const FILTER_IDS: [&str; 5] = [
            k::FILTER_MIN_DBZ,
            k::FILTER_VEL_NEEDS_DBZ,
            k::FILTER_MIN_RHO,
            k::FILTER_HIDE_RF,
            k::FILTER_MIN_RANGE_KM,
        ];
        let dir = std::env::temp_dir().join(format!(
            "radar-workstation-filter-composition-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let context = egui::Context::default();
        let mut app = app_with_filter(&context, FilterValues::OFF, "menus");
        let storm = storm_mode();

        // --- findable, by the words an analyst would type ---------------
        //
        // Through the window's own predicate, and every term has to match, so
        // this is the narrow search rather than a lucky one.
        let category = app
            .settings_registry
            .category(k::CATEGORY)
            .expect("the Radar page is registered");
        for id in FILTER_IDS {
            let spec = app
                .settings_registry
                .setting(k::CATEGORY, id)
                .unwrap_or_else(|| panic!("{id} is declared"));
            assert!(
                crate::settings_ui::search_finds(category, spec, "gate filter"),
                "searching \"gate filter\" does not reach {id}, so the one control                  that hides weather cannot be found in the window that resets it"
            );
        }

        // --- resettable, one row at a time and a page at a time ---------
        apply_filter_through_settings(&mut app, storm);
        assert_eq!(
            app.settings_cache.gate_filter,
            storm.to_filter(),
            "the preset never reached the filter the renderer is handed"
        );

        assert!(app.settings_store.reset(k::CATEGORY, k::FILTER_MIN_DBZ));
        app.recompute_settings_cache();
        assert!(
            app.settings_cache
                .gate_filter
                .min_reflectivity_dbz
                .is_none(),
            "a per-setting reset left the reflectivity threshold censoring"
        );
        assert!(
            app.settings_cache.gate_filter.is_active(),
            "a per-setting reset cleared the whole filter instead of the one row"
        );

        apply_filter_through_settings(&mut app, storm);
        assert!(app.settings_store.reset_category(k::CATEGORY));
        app.recompute_settings_cache();
        assert_eq!(
            app.settings_cache.gate_filter,
            render2d::GateFilter::OFF,
            "a page reset left the filter on with no row on the page saying so"
        );
        assert_eq!(
            crate::gate_filter_ui::pane_status_line(&app.settings_cache.gate_filter, None),
            None,
            "a page reset cleared the pixels and left the pane claiming otherwise"
        );

        // --- saved under a name, switched away from, and switched back --
        apply_filter_through_settings(&mut app, storm);
        let mut library = settings::ProfileLibrary::open(
            &dir,
            WorkstationApp::shipped_settings_document(),
            &app.settings_registry,
        );
        library
            .save_as(
                "Chase",
                app.settings_store.document(),
                &app.settings_registry,
            )
            .expect("save the profile");

        apply_filter_through_settings(&mut app, FilterValues::OFF);
        assert!(
            !app.settings_cache.gate_filter.is_active(),
            "the move away from the profile did not clear the filter, so the              switch back would prove nothing"
        );

        let profile = library
            .find("Chase")
            .expect("the profile was just saved")
            .clone();
        let merged = settings::profiles::merge_for_switch(
            app.settings_store.document(),
            &profile.document,
            "Chase",
        );
        app.settings_store.replace_document(merged);
        let mut outcome = crate::settings_ui::SettingsOutcome::default();
        app.apply_switched_profile(&mut outcome);
        for (category, id) in &outcome.changed {
            app.apply_changed_setting(category, id);
        }
        if !outcome.changed.is_empty() {
            app.recompute_settings_cache();
        }

        assert_eq!(
            app.settings_cache.gate_filter,
            storm.to_filter(),
            "a profile saved with Storm mode came back showing everything, so              the picture and the profile name disagree"
        );
        assert_eq!(
            settings::profiles::active_profile(app.settings_store.document()),
            Some("Chase"),
            "the switch also names the profile it landed on"
        );
        // And the words came back with it.
        assert!(
            crate::gate_filter_ui::pane_status_line(&app.settings_cache.gate_filter, None)
                .is_some_and(|line| line.contains(FILTERED_WORD)),
            "the filter came back and the pane's own statement did not"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- the engine/UI contract ---------------------------------------------
    //
    // `render2d` owns the censoring; `workstation_app` owns the analyst's
    // choice and the indication that it is active. These tests pin the
    // contract between them.

    /// The words on the pane and the gates the engine removed come from the
    /// same sentence.
    ///
    /// `GateFilter::hidden_summary` is the only implementation of that
    /// sentence, and both indicators quote it: the pane header
    /// through `gate_filter_ui::pane_status_line`, and the engine's own line
    /// through `GateFilterReport::badge`. Pinning that they quote it, rather
    /// than pinning two literal strings, is what makes them unable to drift
    /// apart.
    ///
    /// A volume-derived product is integrated out of the whole volume rather
    /// than rastered from one
    /// sweep, so `render_service::render_derived` answers it with
    /// `GateFilterReport::not_applicable` - and an indicator built from the
    /// settings alone would then have that pane reading FILTERED while the
    /// engine's own line read FILTER NOT APPLIED. Two indicators on one pane
    /// disagreeing about whether weather is being hidden is worse than either
    /// alone, which is why `pane_status_line` takes the reason per pane.
    ///
    /// The toolbar's clear key supplies the corresponding "show everything"
    /// action and is pinned by
    /// `clicking_the_toolbars_clear_key_shows_everything_again`.
    #[test]
    fn the_pane_statement_and_the_engine_never_describe_the_same_pane_differently() {
        use crate::render_service::DERIVED_PRODUCT_NOT_FILTERED;

        let filter = storm_mode().to_filter();
        let summary = filter.hidden_summary();
        assert!(!summary.is_empty(), "storm mode names nothing");

        for product in DisplayProduct::ALL {
            // The one fact both sides route on. `render_request` sends a
            // product with a derived volume down `render_derived`;
            // `pane_header_status` asks the identical question of the
            // identical method.
            let applies = product.derived_volume().is_none();
            let reason = (!applies).then_some(DERIVED_PRODUCT_NOT_FILTERED);

            let line = crate::gate_filter_ui::pane_status_line(&filter, reason)
                .unwrap_or_else(|| panic!("{}: a filtered pane said nothing", product.id()));
            let report = if applies {
                // What a sweep-rastered pane comes back with. The counts are
                // not the subject here; the words are.
                render2d::GateFilterReport {
                    filter,
                    gates_visible: 1_000,
                    gates_hidden: 400,
                    ..render2d::GateFilterReport::INACTIVE
                }
            } else {
                render2d::GateFilterReport::not_applicable(filter, DERIVED_PRODUCT_NOT_FILTERED)
            };
            let badge = report
                .badge()
                .unwrap_or_else(|| panic!("{}: the engine reported nothing", product.id()));

            assert!(
                line.contains(&summary),
                "{}: the pane does not name what is hidden: {line:?}",
                product.id()
            );
            assert!(
                badge.contains(&summary),
                "{}: the engine's line does not name what is hidden: {badge:?}",
                product.id()
            );
            assert_eq!(
                line.starts_with(FILTERED_WORD),
                report.is_applicable(),
                "{}: the pane says {line:?} while the engine says {badge:?}",
                product.id()
            );
            if !applies {
                assert!(
                    line.contains(DERIVED_PRODUCT_NOT_FILTERED)
                        && badge.contains(DERIVED_PRODUCT_NOT_FILTERED),
                    "{}: the two sides give different reasons: {line:?} / {badge:?}",
                    product.id()
                );
            }
            // And the pane's own line is what the engine's line is built on
            // top of, so the header does not reword itself when the render
            // lands - it only gains the counts.
            assert!(
                badge.starts_with(&line),
                "{}: the pane reads {line:?} before the render and {badge:?} after it",
                product.id()
            );
        }
    }

    /// A sweep of uniformly weak echo: every gate 10 dBZ, which Storm mode's
    /// 20 dBZ criterion hides wherever the cursor lands past the near-range
    /// cut. Built so a readout test does not have to hit one gate.
    fn weak_echo_volume() -> Arc<RadarVolume> {
        let time = chrono::DateTime::from_timestamp(1_755_000_000, 0).expect("a valid second");
        let mut volume = RadarVolume::new(radar_core::RadarSite::new("KTLX"), time);
        let gates = || radar_core::GateRange {
            first_gate_m: 2_125,
            gate_spacing_m: 250,
            gate_count: 100,
        };
        let cut = volume.push_cut(0.5, Some(1));
        let mut grid = radar_core::MomentGrid::new_u8(
            radar_core::MomentType::Reflectivity,
            gates(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        for index in 0..360_usize {
            cut.radials.push(radar_core::Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: index as i32 * 10,
                gate_range: gates(),
                nyquist_velocity_mps: Some(26.0),
                radial_status: None,
            });
            // 10 dBZ everywhere: (10 * 2) + 66 = 86.
            grid.push_u8_row_slice(index, &[86_u8; 100])
                .expect("an 8-bit row belongs in an 8-bit grid");
        }
        cut.moments
            .insert(radar_core::MomentType::Reflectivity, grid);
        Arc::new(volume)
    }

    /// The cursor readout is censored on exactly the panes the RENDERER
    /// censors, and on no others.
    ///
    /// The failure this pins shipped. `refresh_probe` called `probe_censor`
    /// for every pane, and `probe_censor` keyed on the source MOMENT - but all
    /// seven volume-derived products report `MomentType::Reflectivity` as
    /// their source, and `render_service::render_request` sends every one of
    /// them to `render_derived`, which censors nothing and answers
    /// `GateFilterReport::not_applicable`. So with Storm mode on and a
    /// Composite Reflectivity pane, the cursor over a weak gate read
    /// `CREF FILTERED` at a pixel the pane had painted, under that pane's own
    /// band reading `FILTER NOT APPLIED HERE`. One pane, two indicators,
    /// opposite stories - the same class of contradiction this integration
    /// closed everywhere else, arriving from the other direction.
    ///
    /// Both halves are asserted, because either alone would pass a fix that
    /// broke the other: censor nothing anywhere and the moment pane lies the
    /// original way; censor everything and the derived pane lies this way.
    #[test]
    fn the_readout_and_the_renderer_censor_the_same_panes() {
        let context = egui::Context::default();
        let mut app = app_with_filter(&context, storm_mode(), "menus");
        install(&mut app, weak_echo_volume());
        let pane = first_pane();
        app.refresh_capabilities();
        assert!(app.settings_cache.gate_filter.is_active());

        // A gate 12 km out on the 45 degree radial: past Storm mode's 5 km
        // near-range cut, so the only criterion that can hide it is the 20 dBZ
        // threshold this sweep sits under everywhere.
        let range_km = 12.0_f64;
        let (east_km, north_km) = (
            range_km * 45.0_f64.to_radians().sin(),
            range_km * 45.0_f64.to_radians().cos(),
        );

        for product in DisplayProduct::ALL {
            // Only the products that read reflectivity are comparable here -
            // this fixture carries no velocity, and a pane with no data to
            // probe reads the same either way.
            if product.descriptor().computation.source_moment()
                != radar_core::MomentType::Reflectivity
            {
                continue;
            }
            app.workspace.pane_mut(pane).product = product.product_id();
            app.panes[pane.index()].hovered_world_km = Some((east_km, north_km));
            let volume = app
                .history
                .current()
                .map(|frame| Arc::clone(&frame.volume))
                .expect("a frame is installed");
            let cut_index = app.resolve_cut_index(pane, &volume);
            app.refresh_probe(pane, Some(&volume), cut_index, product);
            let readout = app.panes[pane.index()]
                .probe_text
                .clone()
                .unwrap_or_else(|| panic!("{}: the cursor read nothing", product.id()));

            // The one fact the renderer routes on, asked here the same way.
            let renderer_censors_this_pane = product.derived_volume().is_none();
            assert_eq!(
                readout.contains(FILTERED_WORD),
                renderer_censors_this_pane,
                "{}: the readout says {readout:?} while render_service {} this pane. \
                 The readout and the pixels have to come off the same fact",
                product.id(),
                if renderer_censors_this_pane {
                    "censors"
                } else {
                    "paints every gate of"
                }
            );

            // And the pane's own header agrees with both, for the same
            // reason.
            let line = crate::gate_filter_ui::pane_status_line(
                &app.settings_cache.gate_filter,
                (!renderer_censors_this_pane)
                    .then_some(crate::render_service::DERIVED_PRODUCT_NOT_FILTERED),
            )
            .unwrap_or_else(|| panic!("{}: a filtered pane said nothing", product.id()));
            assert_eq!(
                line.starts_with(FILTERED_WORD),
                readout.contains(FILTERED_WORD),
                "{}: the header says {line:?} and the readout says {readout:?}",
                product.id()
            );
        }
    }

    /// A pane that is showing everything says nothing, whatever its product.
    #[test]
    fn an_unfiltered_pane_says_nothing_even_where_the_filter_could_not_run() {
        for product in DisplayProduct::ALL {
            let reason = product
                .derived_volume()
                .is_some()
                .then_some(crate::render_service::DERIVED_PRODUCT_NOT_FILTERED);
            assert_eq!(
                crate::gate_filter_ui::pane_status_line(&render2d::GateFilter::OFF, reason),
                None,
                "{}: an unfiltered pane put a filter statement on the glass",
                product.id()
            );
        }
    }

    /// One frame through the whole application path, handed back whole.
    ///
    /// The same route `pump_render` takes - measure, resolve, request, wait on
    /// the real worker - except the finished pane is RETURNED rather than
    /// installed, so a test can read the pixels the engine produced and the
    /// report it produced them with. The stamp is matched on the way out
    /// because a frame an earlier pass queued may still be in the channel, and
    /// answering with it would compare two pictures of different settings.
    fn render_one_frame(app: &mut WorkstationApp, pane: PaneId) -> RenderedPane {
        app.refresh_capabilities();
        app.update_viewport(pane, VIEWPORT);
        let volume = app
            .history
            .current()
            .map(|frame| Arc::clone(&frame.volume))
            .expect("a frame to render");
        app.ensure_render_requested(pane, volume, VIEWPORT);
        let wanted = app.panes[pane.index()]
            .pending_stamp
            .expect("the pane queued a render");
        for _ in 0..4_000 {
            match app.render_service.try_recv() {
                Some(RenderUpdate::Completed(rendered)) if rendered.stamp == wanted => {
                    return *rendered;
                }
                Some(RenderUpdate::Failed { stamp, message, .. }) if stamp == wanted => {
                    panic!("the render failed: {message}");
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("the render worker never answered");
    }

    /// Apply these criteria the way the analyst does: write the settings
    /// document, rebuild the cache the paint path reads, invalidate the panes.
    /// Exactly what the chip and the FILTERED band do.
    fn apply_filter_through_settings(app: &mut WorkstationApp, values: FilterValues) {
        crate::gate_filter_ui::write_values(&mut app.settings_store, values);
        app.recompute_settings_cache();
        app.invalidate_view_panes(app.workspace.visible_panes());
    }

    /// Pixels the pane actually inked.
    fn inked(rgba: &[u8]) -> usize {
        rgba.chunks_exact(4).filter(|pixel| pixel[3] > 0).count()
    }

    /// THE end-to-end pin, on real weather.
    ///
    /// A threshold set in the settings document has to change which gates ink
    /// in a rendered frame, the pane has to say so in the engine's own words,
    /// and clearing it has to give the original picture back BYTE FOR BYTE -
    /// not merely something that looks the same. The last of those is the one
    /// that protects every analyst who never touches this feature: OFF is the
    /// shipped state, and OFF has to be the application that existed before
    /// gate filtering did.
    ///
    /// Ignored because it needs real data. Point `NEXRAD_LEVEL2_SAMPLE` at one
    /// Archive II volume - ideally one with a clutter or biological bloom
    /// around the radar, which is what these criteria exist to remove - and
    /// run:
    ///
    /// ```text
    /// cargo test --release -p workstation_app --bin GenericRadar -- \
    ///     --ignored --nocapture storm_mode_reaches_the_gates
    /// ```
    #[test]
    #[ignore = "set NEXRAD_LEVEL2_SAMPLE to one real Archive II volume"]
    fn storm_mode_reaches_the_gates_and_clearing_it_gives_the_frame_back() {
        let path = std::env::var("NEXRAD_LEVEL2_SAMPLE")
            .expect("set NEXRAD_LEVEL2_SAMPLE to one real Archive II volume");
        let volume = Arc::new(
            nexrad_io::decode_volume_from_path(std::path::Path::new(&path))
                .unwrap_or_else(|error| panic!("{path} did not decode: {error}")),
        );
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
        let mut app = app_with_filter(&context, FilterValues::OFF, "menus");
        install(&mut app, Arc::clone(&volume));
        let pane = first_pane();

        // 1. The shipped state.
        let unfiltered = render_one_frame(&mut app, pane);
        assert!(
            unfiltered.gate_filter.is_inactive(),
            "an unfiltered request came back with a filter report: {:?}",
            unfiltered.gate_filter.badge()
        );
        let unfiltered_ink = inked(&unfiltered.rgba);
        assert!(
            unfiltered_ink > 0,
            "{path} rendered an empty pane - nothing here can be proved on it"
        );

        // 2. Storm mode, applied through the settings document.
        apply_filter_through_settings(&mut app, storm_mode());
        assert_eq!(app.settings_cache.gate_filter, storm_mode().to_filter());
        let filtered = render_one_frame(&mut app, pane);

        // The request carries the analyst's criteria, rather than an engine
        // default, so the UI and renderer apply the same filter.
        assert_eq!(
            filtered.gate_filter.filter, app.settings_cache.gate_filter,
            "the engine filtered by something other than what the settings say"
        );
        assert!(
            filtered.gate_filter.gates_hidden > 0,
            "storm mode reached the engine and removed nothing: {:?}",
            filtered.gate_filter
        );
        assert!(
            filtered.gate_filter.gates_hidden < filtered.gate_filter.gates_visible,
            "storm mode emptied the sweep; that is a blank pane, not a filter"
        );
        let filtered_ink = inked(&filtered.rgba);
        assert!(
            filtered_ink < unfiltered_ink,
            "the report says {} gates went but the picture inked {filtered_ink} pixels \
             against {unfiltered_ink} unfiltered - the censor did not reach the raster",
            filtered.gate_filter.gates_hidden
        );

        // 3. The words. The pane header, the legend badge and the engine's
        //    own line all quote the same summary, so the pane and the picture
        //    cannot disagree.
        let summary = app.settings_cache.gate_filter.hidden_summary();
        let line = crate::gate_filter_ui::pane_status_line(&app.settings_cache.gate_filter, None)
            .expect("a filtered pane makes a statement");
        let engine_line = filtered
            .gate_filter
            .badge()
            .expect("the engine says what it removed");
        assert!(line.contains(&summary) && engine_line.contains(&summary));
        assert!(
            engine_line.starts_with(&line),
            "the header reads {line:?} before the render and {engine_line:?} after it"
        );
        assert!(
            crate::gate_filter_ui::pane_badge_text(&app.settings_cache.gate_filter).is_some(),
            "the legend badge went missing while gates were hidden"
        );

        // And on the glass, through the shipped `eframe::App::ui`, with this
        // real volume loaded. Two passes: the first builds the font atlas.
        app_frame(&mut app, &context, Vec::new());
        let texts = shape_texts(&app_frame(&mut app, &context, Vec::new()));
        assert!(
            texts.iter().any(|text| text.starts_with(&engine_line)),
            "a filtered pane did not put the engine's own line on its header over real              data - wanted {engine_line:?} in {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text == FILTERED_WORD),
            "the legend badge is not on the glass over real data: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text == crate::gate_filter_ui::CLEAR_GLYPH),
            "a filtered application over real data offers no way out on its bar: {texts:?}"
        );
        println!("unfiltered ink {unfiltered_ink} px");
        println!("filtered   ink {filtered_ink} px");
        println!("engine     {engine_line}");
        for note in filtered.gate_filter.notes() {
            println!("           {note}");
        }

        // 4. Clear it. The picture has to come back byte for byte: an OFF
        //    filter is not "close enough to" the unfiltered application, it IS
        //    the unfiltered application.
        apply_filter_through_settings(&mut app, FilterValues::OFF);
        let cleared = render_one_frame(&mut app, pane);
        assert!(cleared.gate_filter.is_inactive());
        assert_eq!(
            cleared.rgba.len(),
            unfiltered.rgba.len(),
            "clearing the filter changed the frame's size"
        );
        assert!(
            cleared.rgba == unfiltered.rgba,
            "clearing the filter did not give the original picture back: {} of {} bytes differ",
            cleared
                .rgba
                .iter()
                .zip(&unfiltered.rgba)
                .filter(|(a, b)| a != b)
                .count(),
            unfiltered.rgba.len()
        );
        assert_eq!(
            crate::gate_filter_ui::pane_status_line(&app.settings_cache.gate_filter, None),
            None,
            "the pane's statement outlived the filter"
        );
    }

    // -----------------------------------------------------------------------
    // Level 1: what a pane says about a gate it left blank, and what it may
    // say about where the sweep was.
    // -----------------------------------------------------------------------

    /// A time-series sweep with a signal about 10 dB over the receiver noise.
    ///
    /// Synthetic on purpose, and only about the bookkeeping: the estimators
    /// themselves are proved on real KOUN pulses in `nexrad_io`. What is being
    /// exercised here is the path from a hovered pixel to a sentence, and for
    /// that the sweep has to be (a) a stare at a known azimuth so a cursor can
    /// be put on it, (b) long enough in range to be hovered at the scale a
    /// pane opens on, and (c) at an SNR the censor slider can straddle: above
    /// the shipped 2 dB threshold, below the 20 dB the slider reaches.
    fn stare_sweep(site: &str) -> nexrad_io::iq::IqSweep {
        use nexrad_io::iq::{IqCalibration, IqPulse, IqSweep};
        const PRT_S: f32 = 833.375e-6;
        const NOISE_DBM: f32 = -80.5;
        const SATURATION_DBM: f32 = 6.0;
        const GATES: usize = 96;
        // R(0) ten times the noise power, so S = 9 N and the SNR is 9.5 dB.
        let noise_power = 10f32.powf((NOISE_DBM - SATURATION_DBM) / 10.0);
        let amplitude = (10.0 * noise_power).sqrt();
        let pulses = (0..512)
            .map(|index| {
                let phase = 0.3 * index as f32;
                IqPulse {
                    // A stare, like the reference Ascope records: the antenna
                    // is parked, so every dwell is at one azimuth and a cursor
                    // due east of the radar is on the spoke.
                    azimuth_deg: 90.0,
                    elevation_deg: 4.0,
                    prt_seconds: PRT_S,
                    prt_previous_seconds: PRT_S,
                    h: (0..GATES)
                        .map(|_| (amplitude * phase.cos(), amplitude * phase.sin()))
                        .collect(),
                    v: (0..GATES)
                        .map(|_| (amplitude * phase.cos(), amplitude * phase.sin()))
                        .collect(),
                    ..IqPulse::default()
                }
            })
            .collect();
        IqSweep {
            site: site.to_owned(),
            time_utc: 1_369_079_161,
            wavelength_m: 0.1108,
            pulse_width_s: Some(1.5e-6),
            gate_spacing_m: Some(500.0),
            first_gate_m: 1_000.0,
            range_bins: (0..GATES).map(|bin| 1_000.0 + 500.0 * bin as f32).collect(),
            calibration: IqCalibration::Absolute {
                noise_dbm: [NOISE_DBM, NOISE_DBM],
                dbz_calibration: -35.5,
                saturation_dbm: SATURATION_DBM,
            },
            pulses,
            ..IqSweep::default()
        }
    }

    fn loaded_time_series(app: &WorkstationApp, sweep: nexrad_io::iq::IqSweep) -> LoadedVolume {
        let session = crate::iq_session::IqSession::from_sweep(
            sweep,
            "stare.iqd",
            crate::iq_session::IqControls::default(),
        )
        .expect("the fixture processes");
        let site = radar_core::RadarSite::new(session.site_id());
        let volume = Arc::new(session.volume(site));
        LoadedVolume {
            iq: Some(Box::new(session)),
            assembly: None,
            assembly_refusal: None,
            generation: app.session_clock.current(),
            origin: FrameOrigin::Local,
            source_label: "stare.iqd".to_owned(),
            stage: FrameStage::Complete,
            volume,
            elapsed_ms: 1.0,
        }
    }

    /// Open that sweep in the application, through the shipped install path.
    fn install_time_series(app: &mut WorkstationApp, sweep: nexrad_io::iq::IqSweep) {
        let loaded = loaded_time_series(app, sweep);
        let _ = app.install_loaded_volume(loaded);
    }

    #[test]
    fn an_uncalibrated_iq_cube_opens_on_relative_power_instead_of_a_blank_ref_pane() {
        use nexrad_io::iq::{IqCalibration, PulseLayout, PulseSpan};

        let mut app = test_app();
        let active = app.workspace.active_pane;
        assert_eq!(
            DisplayProduct::from_product_id(&app.workspace.pane(active).product),
            DisplayProduct::Reflectivity,
            "fixture starts on the ordinary workspace default"
        );

        let mut sweep = stare_sweep("OUPRIME");
        sweep.calibration = IqCalibration::RelativeStoredIq;
        sweep.pulse_width_s = None;
        sweep.pulse_layout = PulseLayout::Rays(
            (0..16)
                .map(|ray| PulseSpan {
                    start: ray * 32,
                    len: 32,
                })
                .collect(),
        );
        install_time_series(&mut app, sweep);

        assert_eq!(
            DisplayProduct::from_product_id(&app.workspace.pane(active).product),
            DisplayProduct::RelativePower
        );
        assert!(app.relative_power_fallback_from_ref[active.index()]);
        let session = app.iq.as_ref().expect("I/Q session stays installed");
        assert_eq!(session.native_dwell_pulses(), Some(32));
        let provenance = session.provenance();
        assert!(provenance.contains("power is relative"), "{provenance}");
        assert!(provenance.contains("SNR unavailable"), "{provenance}");
        assert!(!provenance.contains("dBm"), "{provenance}");
        assert!(!provenance.contains("dBZ"), "{provenance}");
    }

    #[test]
    fn relative_power_fallback_restores_ref_for_the_next_calibrated_source() {
        use nexrad_io::iq::IqCalibration;

        let mut app = test_app();
        let active = app.workspace.active_pane;
        let mut relative = stare_sweep("OUPRIME");
        relative.calibration = IqCalibration::RelativeStoredIq;
        relative.pulse_width_s = None;
        install_time_series(&mut app, relative);
        assert_eq!(
            DisplayProduct::from_product_id(&app.workspace.pane(active).product),
            DisplayProduct::RelativePower
        );

        install_time_series(&mut app, stare_sweep("KOUN"));
        assert_eq!(
            DisplayProduct::from_product_id(&app.workspace.pane(active).product),
            DisplayProduct::Reflectivity,
            "the app-owned fallback must not leak into the next source"
        );
        assert!(!app.relative_power_fallback_from_ref[active.index()]);
    }

    #[test]
    fn explicit_product_choice_cancels_relative_power_fallback_restoration() {
        use nexrad_io::iq::IqCalibration;

        let mut app = test_app();
        let active = app.workspace.active_pane;
        let mut relative = stare_sweep("OUPRIME");
        relative.calibration = IqCalibration::RelativeStoredIq;
        relative.pulse_width_s = None;
        install_time_series(&mut app, relative);

        app.apply_product_selection(active, DisplayProduct::Velocity);
        assert!(!app.relative_power_fallback_from_ref[active.index()]);
        install_time_series(&mut app, stare_sweep("KOUN"));
        assert_eq!(
            DisplayProduct::from_product_id(&app.workspace.pane(active).product),
            DisplayProduct::Velocity,
            "a real analyst choice must not be mistaken for the app-owned fallback"
        );
    }

    #[test]
    fn relative_iq_playlist_keeps_relative_power_but_discards_raw_session() {
        use nexrad_io::iq::IqCalibration;

        let mut app = test_app();
        let active = app.workspace.active_pane;
        let mut relative = stare_sweep("OUPRIME");
        relative.calibration = IqCalibration::RelativeStoredIq;
        relative.pulse_width_s = None;
        let loaded = loaded_time_series(&app, relative);
        app.file_sequence = Some(FileSequence {
            paths: vec![PathBuf::from("ouprime-relative.mat")],
            preflight: crate::playlist_preflight::estimate_paths(&[]),
            next: 1,
            loaded: 0,
            failures: Vec::new(),
            site_id: None,
            site_position: None,
            level1_files: 0,
            pending_assembly: None,
            assembled_files: 0,
            assembled_groups: 0,
            assembly_refusals: Vec::new(),
            evicted_frames: 0,
        });

        app.finish_sequence_volume(loaded);

        assert!(app.file_sequence.is_none(), "the one-frame playlist ended");
        assert!(
            app.iq.is_none(),
            "playlist raw pulses must not escape install"
        );
        assert_eq!(app.history.len(), 1);
        assert_eq!(
            DisplayProduct::from_product_id(&app.workspace.pane(active).product),
            DisplayProduct::RelativePower,
            "discarding raw pulses must not also discard the honest display fallback"
        );
        assert!(app.relative_power_fallback_from_ref[active.index()]);
    }

    /// Put the pointer on the gate at `east_km`, `north_km` and return the
    /// frame that reports it.
    ///
    /// The pane's scale is MEASURED rather than assumed: two probe frames give
    /// the affine map from screen points to radar-local kilometres, which is
    /// then inverted. A hard-coded pixel would silently start photographing
    /// empty basemap the day the default camera, the window size or the
    /// toolbar's height changed.
    ///
    /// Two frames at the end because the pane records where the pointer was
    /// and probes it on the NEXT frame - deliberately, so the readout never
    /// reaches into the volume mid-paint.
    fn hover_radar_km(
        app: &mut WorkstationApp,
        context: &egui::Context,
        east_km: f64,
        north_km: f64,
    ) -> Vec<egui::Shape> {
        let pane = first_pane();
        let base = egui::pos2(700.0, 470.0);
        let stepped = base + egui::vec2(40.0, 40.0);
        app_frame(app, context, vec![egui::Event::PointerMoved(base)]);
        app_frame(app, context, vec![egui::Event::PointerMoved(base)]);
        let (base_east, base_north) = app.panes[pane.index()]
            .hovered_world_km
            .expect("the pointer is over the pane");
        app_frame(app, context, vec![egui::Event::PointerMoved(stepped)]);
        app_frame(app, context, vec![egui::Event::PointerMoved(stepped)]);
        let (stepped_east, stepped_north) = app.panes[pane.index()]
            .hovered_world_km
            .expect("the pointer is over the pane");
        let east_per_point = (stepped_east - base_east) / 40.0;
        let north_per_point = (stepped_north - base_north) / 40.0;
        let at = egui::pos2(
            base.x + ((east_km - base_east) / east_per_point) as f32,
            base.y + ((north_km - base_north) / north_per_point) as f32,
        );
        app_frame(app, context, vec![egui::Event::PointerMoved(at)]);
        let shapes = app_frame(app, context, vec![egui::Event::PointerMoved(at)]);
        let (landed_east, landed_north) = app.panes[pane.index()]
            .hovered_world_km
            .expect("the pointer is over the pane");
        assert!(
            (landed_east - east_km).abs() < 0.5 && (landed_north - north_km).abs() < 0.5,
            "aimed at {east_km},{north_km} km and landed on {landed_east},{landed_north}"
        );
        shapes
    }

    /// THE pin on defect 3: a gate that is there and blank says why.
    ///
    /// The pane's own comment promised it - "this gate is empty, and here is
    /// why is the answer to the question the analyst asked by hovering" - and
    /// then returned `None` for `ProbeReading::Absent`, which is exactly the
    /// reading a censored gate produces: every emitted moment of one is NaN.
    /// So hovering the gates the censor removed, which is the whole reason the
    /// censor knob exists, produced no panel at all.
    ///
    /// Asserted on a whole application frame rather than on
    /// `gate_spectrum_absence`, because the failure was never a wrong string -
    /// it was a right string that could not be reached.
    #[test]
    fn hovering_a_censored_gate_gets_a_panel_that_says_why_it_is_empty() {
        use crate::settings_ui::catalog::{keys::timeseries, timeseries_limits as limit};
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("dark"));
        let mut app = test_app();
        install_time_series(&mut app, stare_sweep("ZQZQ_RVP"));

        // First, with the shipped threshold: the gate is 9.5 dB over the
        // receiver noise, so it is on screen and the panel is a spectrum.
        let shown = shape_texts(&hover_radar_km(&mut app, &context, 20.0, 0.0));
        assert!(
            shown.iter().any(|text| text.contains("DOPPLER SPECTRUM")),
            "the fixture gate is not even readable at the shipped threshold: {shown:?}"
        );
        assert!(
            !shown.iter().any(|text| text.contains("below the SNR")),
            "the fixture gate is censored before the test censors it: {shown:?}"
        );

        // Now put the threshold above it. The gate is still THERE - the beam
        // sampled it and the pulses are in memory - and the pane draws nothing
        // at that pixel.
        app.apply_setting_for_proof(
            timeseries::CATEGORY,
            timeseries::SNR_MIN_DB,
            settings::SettingValue::Float(limit::MAX_SNR_DB),
        );
        let hidden = shape_texts(&hover_radar_km(&mut app, &context, 20.0, 0.0));
        assert!(
            hidden
                .iter()
                .any(|text| text.contains("NO DATA - SAMPLED BUT UNUSABLE")),
            "the readout does not report a sampled, blank gate: {hidden:?}"
        );
        assert!(
            hidden.iter().any(|text| text.contains("DOPPLER SPECTRUM")),
            "hovering a censored gate produced no panel at all, so the analyst asking what \
             the censor removed is answered with silence: {hidden:?}"
        );
        assert!(
            hidden
                .iter()
                .any(|text| text.contains("below the SNR threshold")),
            "the panel is up but does not say why the gate is empty: {hidden:?}"
        );
    }

    /// THE pin on defect 4: a pane may not assert a geography it does not
    /// have.
    ///
    /// An RVP8 header carries no coordinates, so an unlocated record's
    /// position is a network fetch that may never arrive. While it has not,
    /// the header said "POSITION UNKNOWN" and the pane simultaneously drew the
    /// sweep over labelled counties with a four-decimal lat/lon under the
    /// cursor - two statements on one pane contradicting each other, one of
    /// them a fabricated position. A KOUN stare from Norman, Oklahoma was
    /// photographed over Smith and Osborne counties, Kansas.
    #[test]
    fn a_sweep_with_no_position_gets_no_geography_to_be_drawn_over() {
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("dark"));
        let mut app = test_app();
        install_time_series(&mut app, stare_sweep("ZQZQ_RVP"));
        assert!(
            !app.frame_position_is_known(),
            "the fixture's site is in the station directory, so this test proves nothing"
        );

        let texts = shape_texts(&hover_radar_km(&mut app, &context, 20.0, 0.0));
        assert!(
            texts.iter().any(|text| text.contains("POSITION UNKNOWN")),
            "the pane does not admit it has no position: {texts:?}"
        );
        // The radar-local half of the corner readout is still there, which is
        // what makes the absence of the other half a suppression rather than a
        // readout that was switched off.
        assert!(
            texts.iter().any(|text| text.contains("090.0°")),
            "the corner readout is not running, so this test cannot see whether the \
             coordinate half was suppressed: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|text| text.contains("°N") || text.contains("°S")),
            "the cursor readout prints a latitude for a sweep with no position: {texts:?}"
        );

        // And nothing geographic reaches the pane to be drawn.
        let pane_rect = egui::Rect::from_min_size(egui::pos2(0.0, 40.0), egui::vec2(1400.0, 820.0));
        let map = app.pane_map(first_pane(), Camera2D::default(), pane_rect);
        assert!(
            map.projection.is_none(),
            "the pane was handed the projection the coordinate readout is computed through"
        );
        assert!(
            map.geometry.is_none(),
            "the pane was handed basemap geometry: county names under an unlocated sweep"
        );
        assert!(map.tiles.is_none(), "the pane was handed imagery");
        assert!(
            map.sites.is_empty(),
            "the pane was handed radar site markers"
        );
        assert!(
            map.hazards.is_empty(),
            "the pane was handed warning polygons"
        );

        // The control: a frame that DOES know where it is gets all of it back,
        // through the same function. Without this the assertions above would
        // pass just as well on a pane that never draws a map at all.
        let mut located = radar_core::RadarSite::new("KTLX");
        located.latitude_deg = Some(35.3333);
        located.longitude_deg = Some(-97.2778);
        install(&mut app, Arc::new(RadarVolume::new(located, Utc::now())));
        // The history keeps the sweep that was already selected, and orders
        // frames by site before time, so the new one is not simply the last:
        // scrub to it by name.
        let ktlx = app
            .history
            .frames()
            .iter()
            .position(|frame| frame.volume.site.id == "KTLX")
            .expect("the located volume is in the history");
        assert!(app.history.select(ktlx));
        assert!(app.frame_position_is_known());
        let map = app.pane_map(first_pane(), Camera2D::default(), pane_rect);
        assert!(
            map.projection.is_some(),
            "a located frame lost its projection too, so the suppression is not conditional"
        );
    }

    /// The other half of the same rule: a record the application CAN source is
    /// placed, offline, the moment it is opened.
    ///
    /// The archived Level 1 records are almost all KOUN, NSSL's research
    /// WSR-88D at Norman. It serves no operational product, so
    /// `api.weather.gov/radar/stations` does not list it, so before
    /// `crate::research_sites` the reference record opened as POSITION UNKNOWN
    /// forever - not until the directory arrived, but forever, because the
    /// directory was never going to have it. The station list here is EMPTY,
    /// which is what makes this a test of the sourced table rather than of the
    /// network.
    #[test]
    fn a_research_radar_is_placed_from_the_sourced_table_with_no_directory() {
        let mut app = test_app();
        assert!(
            app.sites.is_empty(),
            "the station directory arrived, so this test cannot tell which catalog answered"
        );
        install_time_series(&mut app, stare_sweep("KOUN_RVP"));

        assert!(
            app.frame_position_is_known(),
            "the reference record's own site is still unplaced"
        );
        let frame = app.history.current().expect("the record is installed");
        // The processor suffix is off the id, and the position is the one the
        // catalog states - checked against the catalog AND against the literal
        // coordinate, so this fails if the sourced number is ever edited as
        // well as if the plumbing that carries it breaks.
        assert_eq!(frame.volume.site.id, "KOUN");
        let sourced = crate::research_sites::research_site("KOUN").expect("KOUN is sourced");
        assert_eq!(sourced.latitude_deg, 35.236058);
        assert_eq!(sourced.longitude_deg, -97.46235);
        assert_eq!(
            frame.volume.site.latitude_deg,
            Some(sourced.latitude_deg as f32)
        );
        assert_eq!(
            frame.volume.site.longitude_deg,
            Some(sourced.longitude_deg as f32)
        );

        // And the geography the unlocated case withholds is now handed to the
        // pane, through the same function that withheld it.
        let pane_rect = egui::Rect::from_min_size(egui::pos2(0.0, 40.0), egui::vec2(1400.0, 820.0));
        let map = app.pane_map(first_pane(), Camera2D::default(), pane_rect);
        assert!(
            map.projection.is_some(),
            "a placed research radar still gets no projection, so nothing geographic can draw"
        );
    }

    /// Precedence, stated as a behaviour rather than as a comment.
    ///
    /// The published directory is asked first and wins outright. The two
    /// catalogs cannot actually disagree today - no id is in both, and
    /// `research_sites::no_entry_here_shadows_a_published_station` replays the
    /// whole retrieved feed to prove it - so this test manufactures the
    /// collision, because the ORDER is what has to survive somebody adding a
    /// row to the frozen table years from now.
    #[test]
    fn the_published_directory_outranks_the_sourced_table() {
        let mut app = test_app();
        app.sites = vec![LocatedSite {
            id: "KOUN".to_owned(),
            name: Some("published".to_owned()),
            latitude_deg: 41.0,
            longitude_deg: -101.0,
        }];
        let published = app
            .time_series_site("KOUN")
            .expect("both catalogs have KOUN in this test");
        assert_eq!(published.name.as_deref(), Some("published"));
        assert_eq!(published.latitude_deg, 41.0);
        assert_eq!(published.longitude_deg, -101.0);

        // Take the directory away and the sourced table answers instead.
        app.sites.clear();
        let sourced = app
            .time_series_site("KOUN")
            .expect("the sourced table has KOUN");
        assert_eq!(sourced.latitude_deg, 35.236058);
        assert_eq!(sourced.longitude_deg, -97.46235);

        // A site in neither is still refused, which is the behaviour this
        // whole path is built around and the one the fallback must not cost.
        assert_eq!(app.time_series_site("ZQZQ"), None);
        assert_eq!(app.time_series_site("NOXP"), None);
    }

    /// The refusal survives the fallback, end to end through the install path.
    ///
    /// The same fixture site the defect-4 test uses, asserted at the frame
    /// rather than at the pane: a record from a radar neither catalog knows
    /// comes out of `install_loaded_volume` with no coordinates on it at all.
    #[test]
    fn an_unsourced_site_still_installs_with_no_position() {
        let mut app = test_app();
        install_time_series(&mut app, stare_sweep("ZQZQ_RVP"));
        let frame = app.history.current().expect("the record is installed");
        assert_eq!(frame.volume.site.latitude_deg, None);
        assert_eq!(frame.volume.site.longitude_deg, None);
        assert!(!app.frame_position_is_known());
    }

    /// The newest `<SITE>*_V06` in the live cache, by filename - the same
    /// ordering the cache itself uses.
    fn newest_cached_volume(cache: &std::path::Path, site: &str) -> Option<PathBuf> {
        let mut newest: Option<(String, PathBuf)> = None;
        for entry in std::fs::read_dir(cache).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(site) || !name.ends_with("_V06") {
                continue;
            }
            if newest.as_ref().is_none_or(|(best, _)| name > *best) {
                newest = Some((name, entry.path()));
            }
        }
        newest.map(|(_, path)| path)
    }
}
