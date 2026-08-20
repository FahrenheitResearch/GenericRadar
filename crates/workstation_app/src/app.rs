use std::array;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use analyst_runtime::{
    FrameOrigin, FrameStage, GenerationClock, PaneId, PaneLayout, PlaybackState, RenderStamp,
    TiltSelection, ViewportMetrics, VolumeFrame, VolumeHistory, WorkspaceState,
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
use crate::pane_canvas::{PaneMap, PaneTexture, PlacedSite, draw_pane, pane_rects};
use crate::product::DisplayProduct;

use crate::app_support::{color_image_from_rgba, layout_label, pane_title, viewport_changed};
use crate::product_availability::ProductAvailabilityIndex;
use crate::product_picker::{ProductPickerInput, ProductPickerState, draw_product_picker};
use crate::render_service::{
    RenderRequest, RenderService, RenderUpdate, RenderedPane, SweepBlendRequest,
};
use crate::sites_service::{LocatedSite, SitesService};
use crate::sweep::{SweepAnimator, SweepState, catch_up_factor};
use crate::warnings_service::WarningsService;

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
     GR2Analyst .msg31, ODIM_H5 (.h5/.hdf/.hd5), CfRadial (.nc), or a mobile deployment .zip. \
     Files can also be dropped on the window.";

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
    /// Where the pointer was over this pane last frame, in radar-local
    /// kilometres, and the readout built from it.
    hovered_world_km: Option<(f64, f64)>,
    probe_text: Option<String>,
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
    product: &'static str,
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
    stage: FrameStage,
    cuts: usize,
    radials: usize,
}

/// Which of the two supported toolbars draws.
///
/// Both are real, kept, and one setting apart (2026-08-19): the menu bar is
/// the compact row with File / View / Map / Tools for the occasional
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
            legend: true,
            sweep_animation: true,
            sweep_speed: 1.0,
            vrot_mps_first: false,
            units: crate::units::UnitSystem::default(),
            annotation: crate::annotation::Annotation::default(),
            xsection_top_m: crate::xsection::DEFAULT_TOP_M,
            loop_frame_time: PLAYBACK_FRAME_TIME,
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
    status: String,
    load_ms: Option<f32>,
    last_playback_step: Instant,
    map_scene: MapSceneController,
    sites_service: SitesService,
    sites: Vec<LocatedSite>,
    placed_sites: Arc<[PlacedSite]>,
    placed_sites_projection: Option<map_scene::ProjectionId>,
    live_service: LiveService,
    live_cache_dir: PathBuf,
    site_text: String,
    live_site: Option<String>,
    live_status: String,
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
            vol3d: crate::vol3d::Vol3d::default(),
            xsection: crate::xsection::XSection::default(),
            vrot_active: false,
            vrot_state: crate::vrot::VrotState::Idle,
            vrot_pane: None,
            capabilities: None,
            capabilities_for: None,
            quality: render2d::DisplayQuality::default(),
            product_availability: ProductAvailabilityIndex::unrestricted(),
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
            status: "Drop a Level II file here or enter a path above".to_owned(),
            load_ms: None,
            last_playback_step: Instant::now(),
            sites_service: SitesService::new(context.clone()),
            sites: Vec::new(),
            placed_sites: Vec::new().into(),
            placed_sites_projection: None,
            live_cache_dir: default_live_cache_dir(),
            site_text: String::new(),
            live_site: None,
            live_status: String::new(),
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

    /// Apply everything the settings file says to a freshly built application.
    ///
    /// Called once from `with_context`, before any load or live start, so the
    /// history policy exists before the first install and a restored pane
    /// never renders once in its default shape first.
    fn apply_settings_on_start(&mut self) {
        use crate::settings_ui::catalog::keys;

        self.apply_settings_document();

        // Only on start: `VolumeHistory::new` builds an empty history, so
        // this line belongs to a session that has not loaded anything yet. A
        // profile switch changes the same two settings through
        // `apply_changed_setting`, which calls `set_policy` and evicts down to
        // it rather than throwing away every volume the analyst has.
        let frames = self.settings_store.effective_int(
            &self.settings_registry,
            keys::data::CATEGORY,
            keys::data::HISTORY_MAX_FRAMES,
        ) as usize;
        let megabytes = self.settings_store.effective_int(
            &self.settings_registry,
            keys::data::CATEGORY,
            keys::data::HISTORY_MAX_MB,
        ) as usize;
        self.history = VolumeHistory::new(analyst_runtime::HistoryPolicy::new(
            frames,
            megabytes * 1024 * 1024,
        ));

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
            if DisplayProduct::try_from_product_id(&id).is_none() {
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
            (
                keys::analysis::CATEGORY,
                keys::analysis::STORM_MOTION_DIR | keys::analysis::STORM_MOTION_SPEED,
            ) => {
                self.apply_storm_motion_settings();
                // Storm-relative products draw the same data differently now.
                self.invalidate_view_panes(self.workspace.visible_panes());
            }
            (keys::data::CATEGORY, keys::data::HISTORY_MAX_FRAMES | keys::data::HISTORY_MAX_MB) => {
                let frames = self.settings_store.effective_int(
                    &self.settings_registry,
                    keys::data::CATEGORY,
                    keys::data::HISTORY_MAX_FRAMES,
                ) as usize;
                let megabytes = self.settings_store.effective_int(
                    &self.settings_registry,
                    keys::data::CATEGORY,
                    keys::data::HISTORY_MAX_MB,
                ) as usize;
                // `set_policy`, not a rebuild: shrinking evicts, and the
                // frames that survive stay on the timeline.
                let _evicted = self.history.set_policy(analyst_runtime::HistoryPolicy::new(
                    frames,
                    megabytes * 1024 * 1024,
                ));
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

    fn begin_load(&mut self, path: PathBuf) {
        if self.live_site.is_some() {
            self.live_service.stop();
            self.live_site = None;
            self.live_status.clear();
        }
        // A local file is not a feed. Whatever the last session said about
        // KUEX's prefix must not follow the analyst into an archive volume.
        self.live_feed = None;
        // The file may be any radar's: the coordinates a Vrot endpoint was
        // clicked at name a different place under the new session, so the
        // measurement is retired before the world changes under it.
        self.vrot_state
            .mark_stale(crate::vrot::StaleReason::DifferentSite);
        let generation = self.session_clock.bump();
        self.frame_clock.bump();
        self.history.clear();
        self.source_path_text = path.display().to_string();
        self.status = format!("Loading {}", path.display());
        self.load_ms = None;
        self.clear_all_panes();
        let source_label = path.display().to_string();
        if let Err(request) = self.load_service.request(LoadRequest {
            generation,
            path,
            origin: FrameOrigin::Local,
            final_stage: FrameStage::Complete,
            source_label,
        }) {
            self.status = format!("load worker is closed: {}", request.path.display());
        }
    }

    /// Start a live session for `site`. The generation bump invalidates every
    /// in-flight local or previous-site result before the new session installs.
    fn start_live(&mut self, site: String) {
        // Unconditional, like the history clear below: a half-finished pair
        // must not keep its first endpoint - old-anchor coordinates - alive
        // into the new radar's data, and a finished measurement of the old
        // radar must stop reading as current. See `crate::vrot::mark_stale`.
        self.vrot_state
            .mark_stale(crate::vrot::StaleReason::DifferentSite);
        let generation = self.session_clock.bump();
        self.frame_clock.bump();
        self.history.clear();
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
        self.live_feed = None;
        self.status = "Live session stopped".to_owned();
    }

    fn poll_site_directory(&mut self) {
        while let Some(sites) = self.sites_service.try_recv() {
            self.sites = sites;
            // Force a reprojection against the current anchor.
            self.placed_sites_projection = None;
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
                        self.status = format!("Decoding {source_label}");
                    }
                }
                LoadUpdate::Volume(loaded) => self.install_loaded_volume(loaded),
                LoadUpdate::Failed {
                    generation,
                    source_label,
                    message,
                } => {
                    if generation == self.session_clock.current() {
                        self.handle_load_failure(&source_label, &message);
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

    fn install_loaded_volume(&mut self, loaded: LoadedVolume) {
        if loaded.generation != self.session_clock.current() {
            return;
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
                let viewports = array::from_fn(|index| self.panes[index].viewport);
                self.workspace.apply_site_change(&viewports, |world| {
                    let (lon, lat) = previous_anchor?.world_to_lon_lat(world);
                    new_anchor?.try_lon_lat_to_world(lon, lat)
                })
            };
            self.invalidate_view_panes(&changed);
            if !opening {
                // The ground moved: a section line stated in the old radar's
                // kilometres now names a different place on earth.
                self.xsection.clear_line();
            }
        }

        let before = self.current_frame_signature();
        let before_extent = self.current_frame_extent();
        let stage = loaded.stage;
        let report = self.history.install(VolumeFrame::new(
            loaded.volume,
            loaded.origin,
            stage,
            loaded.source_label,
        ));
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
        self.status = match stage {
            FrameStage::Preview => format!(
                "Preview ready in {:.1} ms · {} frame(s)",
                loaded.elapsed_ms,
                self.history.len()
            ),
            FrameStage::Partial => format!(
                "Partial volume ready in {:.1} ms · {} frame(s)",
                loaded.elapsed_ms,
                self.history.len()
            ),
            FrameStage::Complete => format!(
                "Complete volume ready in {:.1} ms · {} frame(s) · {:?}",
                loaded.elapsed_ms,
                self.history.len(),
                report.disposition
            ),
        };
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
            // A frame with gates removed says so here, every time, in the words
            // the filter itself uses. This is the floor, not the finished
            // treatment: a persistent badge on the pane belongs on the pane.
            // But no build of this application may ever draw a censored sweep
            // and say nothing at all, because the only other evidence an
            // analyst would have is the absence of the echo that was removed.
            runtime.status = match rendered.gate_filter.badge() {
                Some(badge) => format!("{:.1} ms | {badge}", rendered.elapsed_ms),
                None => format!("{:.1} ms", rendered.elapsed_ms),
            };
        }
        // A view-stale install leaves `pending_stamp` alone on purpose: the
        // exact-stamp render is still owed, and `ensure_render_requested`
        // keeps asking for it. `visible_panes_ready` compares stamps exactly,
        // so playback gating is unchanged by the stale pixels.
    }

    /// A drop is either colour tables or a radar volume, decided per path by
    /// its extension.
    ///
    /// One drop can carry several files, and a folder of palettes is exactly
    /// the kind of thing an analyst drags in one go, so every colour table in
    /// the drop is imported rather than only the first. A volume is still one
    /// at a time - a pane draws one - so the first non-palette path that
    /// looks like radar data wins, which keeps a screenshot dragged alongside
    /// the volume from clearing the session.
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
        if let Some(path) = crate::app_support::choose_dropped_radar_file(candidates) {
            self.begin_load(path);
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
        if *self.color_tables == resolved {
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

    /// The menu bar: one compact row at any window width. Storm controls
    /// stay on it; the occasional ones live under File / View / Map / Tools.
    fn toolbar_menus(&mut self, ui: &mut egui::Ui) {
        use crate::theme::bevel;

        // Before the bar is built, not inside the File menu's closure: the
        // summary reads the profile library and the settings store together,
        // and computing it here keeps the closure borrowing one field.
        let profile_line = self.active_profile_line();
        let active = self.workspace.active_pane;
        let current_product = DisplayProduct::from_product_id(&self.workspace.active().product);
        let mut requested_load = None;
        let mut live_action = None;
        let mut selected_layout = self.workspace.layout;
        let mut selected_product = current_product;
        let mut quality_changed = false;
        let mut palette_changed = false;
        let mut filter_changed = false;
        let mut tilt_delta = 0_isize;
        let visible = self.workspace.visible_panes();
        let cameras_linked = visible
            .iter()
            .all(|pane| self.workspace.pane(*pane).links.camera == Some(0));
        let mut toggle_camera_links = false;
        let mut toggle_warnings = false;

        // A menu bar, not a control wall. The bar carries only what an
        // analyst touches mid-storm - product, palette, tilt, site - and the
        // occasional controls live under File / View / Map / Tools, so the
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
                    if ui.button("Load file").clicked() && !self.source_path_text.trim().is_empty()
                    {
                        requested_load = Some(PathBuf::from(self.source_path_text.trim()));
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
                    format!("{} ⏷", current_product.label()),
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
                                        availability: &self.product_availability,
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
                if let Some(family) = palette_family {
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

        if filter_changed {
            // The chip wrote straight to the store, so the cache the paint
            // path reads is one frame stale until this runs. Before the
            // invalidation below, so the re-render it asks for is requested
            // under the new filter rather than the old one.
            self.recompute_settings_cache();
        }
        if quality_changed || palette_changed || filter_changed {
            // Same data, different picture: every pane's view generation moves,
            // which discards the in-flight render and asks for a new one
            // without throwing away the texture that is currently on screen.
            self.invalidate_view_panes(self.workspace.visible_panes());
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
        if selected_product != current_product {
            self.apply_product_selection(active, selected_product);
        }
        if tilt_delta != 0 {
            self.change_active_tilt(tilt_delta);
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
        let current_product = DisplayProduct::from_product_id(&self.workspace.active().product);
        let mut requested_load = None;
        let mut live_action = None;
        let mut selected_layout = self.workspace.layout;
        let mut selected_product = current_product;
        let mut quality_changed = false;
        let mut palette_changed = false;
        let mut filter_changed = false;
        let mut tilt_delta = 0_isize;
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
                .selectable_label(self.product_picker_open, current_product.label())
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
                                    availability: &self.product_availability,
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
            if let Some(family) = palette_family {
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

            let mut selected_quality = self.quality;
            egui::ComboBox::from_id_salt("workstation-quality")
                .selected_text(selected_quality.preset_label().unwrap_or("Custom"))
                .width(92.0)
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

        if filter_changed {
            // The chip wrote straight to the store, so the cache the paint
            // path reads is one frame stale until this runs. Before the
            // invalidation below, so the re-render it asks for is requested
            // under the new filter rather than the old one.
            self.recompute_settings_cache();
        }
        if quality_changed || palette_changed || filter_changed {
            // Same data, different picture: every pane's view generation moves,
            // which discards the in-flight render and asks for a new one
            // without throwing away the texture that is currently on screen.
            self.invalidate_view_panes(self.workspace.visible_panes());
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
        if selected_product != current_product {
            self.apply_product_selection(active, selected_product);
        }
        if tilt_delta != 0 {
            self.change_active_tilt(tilt_delta);
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
        self.invalidate_semantic_panes(&changed);
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
        let product = DisplayProduct::from_product_id(&self.workspace.active().product);
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
        if let Some(path) = outcome.saved {
            self.status = format!("Colour table saved to {}", path.display());
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
        let product = DisplayProduct::from_product_id(&self.workspace.active().product);
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
        // `GateFilterReport::not_applicable`. So the band is built per pane,
        // from the pane's own product, and a pane the filter did not run on
        // says that rather than claiming gates are hidden in it.
        //
        // Whether it ran is known here, synchronously, from the product alone -
        // it is not read back off the render, which would put the honest
        // version of this band a frame or two behind the picture it describes.
        // `render_service::render_request` routes on exactly the same
        // `derived_volume()`, and a test pins the two to each other.
        let mut clear_filter = false;
        for (pane, pane_rect) in pane_rects(rect, self.workspace.layout) {
            let camera = self.workspace.pane(pane).camera;
            let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
            let cut_index = volume
                .as_deref()
                .and_then(|volume| self.resolve_cut_index(pane, volume));
            let title = pane_title(volume.as_deref(), pane, product, cut_index);
            let status = self.pane_header_status(pane, now);
            // Ask the scene for this pane's LOD. Once resident this is a cache
            // lookup; it queues a build only when the bucket is new.
            let pane_map = PaneMap {
                geometry: self
                    .map_scene
                    .geometry_for_pane(pane.index(), camera.sanitized().km_per_point),
                tiles: self
                    .map_scene
                    .tiles_for_pane(pane.index(), camera, pane_rect),
                projection: self.map_scene.projection(),
                // Paint-time colours for the chosen basemap look. Read from the
                // style the controller is holding rather than stored beside it,
                // so the picker has exactly one thing to set.
                chrome: map_scene::MapChrome::for_style(self.map_scene.style()),
                sites: Arc::clone(&self.placed_sites),
                site_labels: self.settings_cache.site_labels,
                annotation: self.settings_cache.annotation,
                units: self.settings_cache.units,
                active_site: self.live_site.clone(),
                hazards: Arc::clone(&self.placed_hazards),
            };
            let badges = self.pane_badges(product, now);
            let filter_notice = crate::gate_filter_ui::pane_banner_text_for(
                &self.settings_cache.gate_filter,
                product
                    .derived_volume()
                    .is_some()
                    .then_some(crate::render_service::DERIVED_PRODUCT_NOT_FILTERED),
            );
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
                let table = crate::palettes::table_for(product.descriptor(), &self.color_tables);
                // The legend can be turned off in Settings; `None` draws no
                // bar, the same as a product whose domain has no ladder.
                let layout = if self.settings_cache.legend {
                    crate::legend::legend_layout(&product.domain(), &table)
                } else {
                    None
                };
                let overlay = crate::pane_canvas::PaneOverlay {
                    legend: layout.as_ref(),
                    table: Some(&table),
                    product_name: product.descriptor().short_name,
                    badges: &badges,
                    probe: self.panes[pane.index()].probe_text.as_deref(),
                    filter_notice: filter_notice.as_deref(),
                };
                draw_pane(
                    ui,
                    pane,
                    pane_rect,
                    pane == self.workspace.active_pane,
                    camera,
                    self.settings_cache.nav,
                    texture,
                    &pane_map,
                    &title,
                    &status,
                    &overlay,
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

            // The one obvious way out, taken where the evidence is. `draw_pane`
            // has already withheld this click from `interaction.clicked`, from
            // `clicked_site` and from `ctrl_clicked_lon_lat`, so clearing the
            // filter cannot also drop a Vrot endpoint, drop a section handle,
            // or change which radar this pane is showing.
            if interaction.clear_filter_clicked {
                clear_filter = true;
                self.workspace.set_active(pane);
            }
            if self.vrot_active && interaction.clicked {
                self.take_vrot_sample(pane, volume.as_deref(), cut_index, product);
            } else if self.xsection.wants_pane_clicks()
                && interaction.clicked
                && let Some(world) = interaction.hovered_world_km
                && self.xsection.handle_pane_click(world)
            {
                self.workspace.set_active(pane);
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
            self.refresh_probe(pane, volume.as_deref(), cut_index, product);
            self.update_viewport(pane, interaction.viewport);
            if interaction.camera_changed {
                let changed = self.workspace.apply_camera_from(pane, interaction.camera);
                self.invalidate_view_panes(&changed);
            }
            if let Some(volume) = &volume {
                self.ensure_render_requested(pane, Arc::clone(volume), interaction.viewport);
            }
        }
        if clear_filter {
            // After the loop, not inside it: every visible pane has already
            // asked for a render under the old filter this frame, and the
            // invalidation below has to be the last word so the request that
            // actually reaches the worker is the unfiltered one.
            crate::gate_filter_ui::write_values(
                &mut self.settings_store,
                crate::gate_filter_ui::FilterValues::OFF,
            );
            self.recompute_settings_cache();
            self.invalidate_view_panes(self.workspace.visible_panes());
            self.status = "Gate filter cleared - every gate is being drawn".to_owned();
        }
    }

    fn timeline(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let frame_count = self.history.len();
        let mut selected = self.history.selected_index().unwrap_or(0);
        let mut choose_frame = None;
        let mut go_live = false;
        let mut toggle_playback = false;

        ui.horizontal(|ui| {
            if ui
                .add_enabled(frame_count > 1, egui::Button::new("◀"))
                .clicked()
            {
                choose_frame = selected.checked_sub(1);
            }
            if ui
                .add_enabled(
                    frame_count > 1,
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
                    frame_count > 1 && selected + 1 < frame_count,
                    egui::Button::new("▶"),
                )
                .clicked()
            {
                choose_frame = Some(selected + 1);
            }
            if ui
                .add_enabled(frame_count > 0, egui::Button::new("Go live"))
                .clicked()
            {
                go_live = true;
            }

            if frame_count > 1 {
                let response = ui.add_sized(
                    [220.0, ui.spacing().interact_size.y],
                    egui::Slider::new(&mut selected, 0..=frame_count - 1).show_value(false),
                );
                if response.changed() {
                    choose_frame = Some(selected);
                }
            }

            ui.separator();
            ui.label(self.timeline_status(Utc::now()));
            if let Some(load_ms) = self.load_ms {
                ui.label(format!("decode {load_ms:.1} ms"));
            }
            ui.label(format!(
                "history {:.1} MiB",
                self.history.estimated_bytes() as f64 / (1024.0 * 1024.0)
            ));
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
    }

    fn ensure_render_requested(
        &mut self,
        pane: PaneId,
        volume: Arc<RadarVolume>,
        viewport: ViewportMetrics,
    ) {
        let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
        let stamp = self.current_stamp(pane);
        let Some(cut_index) = self.resolve_cut_index(pane, &volume) else {
            // Terminal for this stamp, so playback does not wait for ever on
            // a picture that cannot exist - see [`RenderTerminal`].
            let runtime = &mut self.panes[pane.index()];
            runtime.pending_stamp = None;
            runtime.terminal = Some(RenderTerminal::Unavailable(stamp));
            runtime.status = format!("{} unavailable", product.id());
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
        // A sweep still filling is drawn over the last complete picture of the
        // same tilt. A complete sweep is not blended at all, so an archive file
        // renders down exactly the path it always did.
        let sweep = self.panes[pane.index()]
            .sweep_state
            .filter(|state| !state.complete)
            .and_then(|state| {
                let moment = product.source_moment();
                let (previous_volume, previous_cut_index) = crate::app_support::previous_sweep_for(
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
            });

        let request = RenderRequest {
            pane,
            stamp,
            volume,
            capabilities,
            environment: self.hail_environment.clone(),
            cut_index,
            product,
            camera: self.workspace.pane(pane).camera,
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
        if !self.history.at_live_edge() {
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
            let cut_index = self.resolve_cut_index(pane, &volume);
            let key = cut_index.map(|cut_index| SweepKey {
                identity: identity.clone(),
                product: product.id(),
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
    /// The key includes the cut and radial counts, not just the frame identity
    /// and stage. A live volume grows in place: chunks arrive, radials are
    /// appended and whole cuts are added, all under one site and volume time at
    /// stage `Partial`. Keying on identity alone would measure the first
    /// fragment that arrived and then answer every later question from it, so
    /// the pane would keep drawing the tilt that existed a minute ago.
    fn refresh_capabilities(&mut self) {
        let key = self.history.current().map(|frame| CapabilitiesKey {
            identity: frame.identity.clone(),
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
    }

    fn current_frame_signature(&self) -> Option<(analyst_runtime::FrameIdentity, FrameStage)> {
        self.history
            .current()
            .map(|frame| (frame.identity.clone(), frame.stage))
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
        before: Option<(analyst_runtime::FrameIdentity, FrameStage)>,
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
            return;
        };
        let (Some(volume), Some(cut_index)) = (volume, cut_index) else {
            self.panes[pane.index()].probe_text = None;
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
    }

    fn resolve_cut_index(&self, pane: PaneId, volume: &RadarVolume) -> Option<usize> {
        let intent = self.workspace.pane(pane);
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
        let product = DisplayProduct::from_product_id(&self.workspace.pane(active).product);
        let Some(current) = self.resolve_cut_index(active, &volume) else {
            return;
        };
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
        let Some(capabilities) = self.capabilities.as_ref() else {
            return "No volume measured yet".to_owned();
        };
        let pane = self.workspace.active_pane;
        let product = DisplayProduct::from_product_id(&self.workspace.pane(pane).product);
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
        // `legend::MAX_BADGES`. This is the legend's copy of the statement -
        // the loud one is the FILTERED band `canvas` draws under the header,
        // which is not affected by the colour legend being switched off.
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

    fn pane_header_status(&self, pane: PaneId, now: DateTime<Utc>) -> String {
        let mut parts: Vec<String> = Vec::new();
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
        self.poll_live_results();
        self.poll_load_results();
        self.poll_site_directory();
        self.poll_warnings();
        // Before anything asks which sweep to draw.
        self.refresh_capabilities();
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
    /// helpers that do it — but that file `#[path]`-includes `src/theme.rs`
    /// and nothing else, so it cannot see whether anyone still CALLS them,
    /// and neither can `examples/theme_gallery.rs`'s toolbar proof, which
    /// paints the ground itself before photographing `toolbar`. Deleting
    /// `paint_root_ground(ui)` from `ui` and the `clear_color` override from
    /// this `impl` left all sixteen contract tests green and all sixteen
    /// proof PNGs byte-identical — and that deletion IS the field failure:
    /// the per-frame `panel_fill` override that used to hide it was removed
    /// exactly this way when the theme landed. So this test drives the real
    /// `<WorkstationApp as eframe::App>` and nothing else.
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
                    // Off the same cache the live path reads, and with the same
                    // `None` reason a moment pane passes: REF is not a derived
                    // volume, so the band this helper sees is the band the
                    // application would draw over these very pixels.
                    let filter_notice = crate::gate_filter_ui::pane_banner_text_for(
                        &app.settings_cache.gate_filter,
                        None,
                    );
                    let overlay = crate::pane_canvas::PaneOverlay {
                        legend: None,
                        table: None,
                        product_name: "REF",
                        badges: &[],
                        probe: None,
                        filter_notice: filter_notice.as_deref(),
                    };
                    draw_pane(
                        ui,
                        PaneId::new(0).expect("pane 0"),
                        rect,
                        true,
                        analyst_runtime::Camera2D::default(),
                        app.settings_cache.nav,
                        None,
                        &map,
                        "1 - REF (dBZ)",
                        "",
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
    /// `recompute_settings_cache` and the paint pass - and this wave built
    /// the unit setting, the window that resets it and the profile that
    /// restores it on three separate branches that never compiled together.
    /// So this asserts on the strings `draw_pane` emitted.
    ///
    /// It stands for the whole Units & time page rather than for one row.
    /// [`WorkstationApp::apply_switched_profile`] enumerates the registry
    /// instead of a list of keys, so a page that is registered is a page a
    /// profile carries - and the audit's four pages are registered.
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
    /// of `registry.register(...)` lines, one page each, and a line lost to a
    /// bad merge takes its whole page - its rows, its search hits, its reset
    /// and its share of every profile - out of the application while
    /// `catalog::registry()` and every test written against THAT keep
    /// passing, because they go through the same lost line.
    ///
    /// So this pins the merged list, by name and in order, against the only
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
                // The theme's page and the catalog's merge into this one.
                crate::theme::settings::keys::CATEGORY,
                keys::map::CATEGORY,
                keys::radar::CATEGORY,
                keys::navigation::CATEGORY,
                keys::vol3d::CATEGORY,
                keys::analysis::CATEGORY,
                keys::data::CATEGORY,
                // The four pages the settings audit added...
                keys::units::CATEGORY,
                keys::network::CATEGORY,
                keys::annotation::CATEGORY,
                keys::xsection::CATEGORY,
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
        app.install_loaded_volume(LoadedVolume {
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
        app.install_loaded_volume(LoadedVolume {
            generation,
            origin: FrameOrigin::Live,
            source_label: "test".to_owned(),
            stage: FrameStage::Partial,
            volume,
            elapsed_ms: 1.0,
        });
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

        assert_eq!(app.pane_header_status(pane, observed_now()), "2 min old");

        // Same volume, two hours later: the number moves without anything new
        // arriving, which is what makes it a live quantity rather than a stamp.
        assert_eq!(
            app.pane_header_status(pane, observed_now() + TimeDelta::hours(2)),
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
            app.pane_header_status(pane, observed_now()),
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
            app.pane_header_status(first_pane(), observed_now()),
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
            app.pane_header_status(first_pane(), observed_now()),
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
            app.pane_header_status(first_pane(), observed_now()),
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
                    app.pane_header_status(first_pane(), now)
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

    /// Where a text run containing `needle` landed. The needle rather than a
    /// prefix because the pane draws TWO runs beginning `FILTERED` - the band
    /// and the legend badge - and only one of them is clickable.
    fn text_position(shapes: &[egui::Shape], needle: &str) -> Option<egui::Pos2> {
        fn walk(shape: &egui::Shape, needle: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.text().contains(needle) => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                }
                egui::Shape::Vec(nested) => nested.iter().find_map(|shape| walk(shape, needle)),
                _ => None,
            }
        }
        shapes.iter().find_map(|shape| walk(shape, needle))
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

    /// THE safety pin. A pane that is hiding gates says so, naming what it is
    /// hiding; a pane that is hiding nothing says nothing.
    ///
    /// Asserted on a full application frame rather than on
    /// `gate_filter_ui::pane_banner_text`, because the failure this guards
    /// against is not a wrong string - it is a right string that never reaches
    /// the glass, which is what deleting one line of `canvas` would produce
    /// while every unit test stayed green.
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
        // The band: the loud one, drawn by `canvas` under every pane's header
        // whatever the legend setting says.
        let band = texts
            .iter()
            .find(|text| text.contains("show everything"))
            .unwrap_or_else(|| {
                panic!(
                    "a filtered pane drew no {FILTERED_WORD} band - the only evidence left \
                     would be the missing echo itself: {texts:?}"
                )
            });
        assert!(
            band.starts_with(FILTERED_WORD),
            "the band buries the word that matters: {band:?}"
        );
        assert!(
            band.contains("REF below 20 dBZ"),
            "the band does not name what it hides: {band:?}"
        );
        // And the legend's own copy, beside the colour bar where the analyst
        // is already reading.
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with(FILTERED_WORD) && !text.contains("show everything")),
            "the legend badge stack carries no filter badge: {texts:?}"
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

    /// The one obvious action, exercised where it lives: clicking the band
    /// clears every criterion.
    #[test]
    fn clicking_the_filtered_band_shows_everything_again() {
        let context = egui::Context::default();
        crate::theme::apply(&context, &crate::theme::Appearance::by_id("light"));
        let mut app = app_with_filter(&context, storm_mode(), "menus");
        app_frame(&mut app, &context, Vec::new());
        let shapes = app_frame(&mut app, &context, Vec::new());
        let band = text_position(&shapes, "show everything")
            .expect("a filtered pane draws its band before it can be clicked");
        assert!(app.settings_cache.gate_filter.is_active());

        app_frame(&mut app, &context, pointer(band, true));
        app_frame(&mut app, &context, pointer(band, false));

        assert_eq!(
            app.settings_cache.gate_filter,
            render2d::GateFilter::OFF,
            "clicking the band did not clear the filter"
        );
        let texts = shape_texts(&app_frame(&mut app, &context, Vec::new()));
        assert!(
            !texts.iter().any(|text| text.contains(FILTERED_WORD)),
            "the band outlived the filter it was warning about: {texts:?}"
        );
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
    /// The five criteria arrived on their own branch, reachable from their own
    /// toolbar panel, while the window beside them grew search, per-setting
    /// and per-page reset, and named profiles. Landing both is only half the
    /// job: a criterion the window cannot find, cannot reset, or quietly drops
    /// out of a profile is a criterion that hides weather and then refuses to
    /// account for it.
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
            crate::gate_filter_ui::pane_banner_text_for(&app.settings_cache.gate_filter, None),
            None,
            "a page reset cleared the pixels and left the band claiming otherwise"
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
            crate::gate_filter_ui::pane_banner_text_for(&app.settings_cache.gate_filter, None)
                .is_some_and(|band| band.contains(FILTERED_WORD)),
            "the filter came back and the band did not"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- the integration seams ----------------------------------------------
    //
    // Two branches built this feature against a written contract: `render2d`
    // owns the censoring, `workstation_app` owns the analyst's choice and the
    // admission that it is on. These pin the joints between them - the places
    // where a merge that compiles can still be wrong.

    /// The words on the pane and the gates the engine removed come from the
    /// same sentence.
    ///
    /// `GateFilter::hidden_summary` is the only implementation of that
    /// sentence after the merge - the UI branch's copy was a declared
    /// placeholder and is gone - and both indicators quote it: the pane's band
    /// through `gate_filter_ui::pane_banner_text_for`, and the engine's own
    /// line through `GateFilterReport::badge`. Pinning that they quote it,
    /// rather than pinning two literal strings, is what makes them unable to
    /// drift apart.
    ///
    /// The second half is the case the merge created. A volume-derived product
    /// is integrated out of the whole volume rather than rastered from one
    /// sweep, so `render_service::render_derived` answers it with
    /// `GateFilterReport::not_applicable` - and a canvas-wide band would then
    /// have that pane reading FILTERED while its own status line read FILTER
    /// NOT APPLIED. Two indicators on one pane disagreeing about whether
    /// weather is being hidden is worse than either alone.
    #[test]
    fn the_band_and_the_engine_never_describe_the_same_pane_differently() {
        use crate::render_service::DERIVED_PRODUCT_NOT_FILTERED;

        let filter = storm_mode().to_filter();
        let summary = filter.hidden_summary();
        assert!(!summary.is_empty(), "storm mode names nothing");

        for product in DisplayProduct::ALL {
            // The one fact both sides route on. `render_request` sends a
            // product with a derived volume down `render_derived`; `canvas`
            // asks the identical question of the identical method.
            let applies = product.derived_volume().is_none();
            let reason = (!applies).then_some(DERIVED_PRODUCT_NOT_FILTERED);

            let band = crate::gate_filter_ui::pane_banner_text_for(&filter, reason)
                .unwrap_or_else(|| panic!("{}: a filtered pane drew no band", product.id()));
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
                band.contains(&summary),
                "{}: the band does not name what is hidden: {band:?}",
                product.id()
            );
            assert!(
                badge.contains(&summary),
                "{}: the engine's line does not name what is hidden: {badge:?}",
                product.id()
            );
            assert_eq!(
                band.starts_with(FILTERED_WORD),
                report.is_applicable(),
                "{}: the band says {band:?} while the engine says {badge:?}",
                product.id()
            );
            if !applies {
                assert!(
                    band.contains(DERIVED_PRODUCT_NOT_FILTERED)
                        && badge.contains(DERIVED_PRODUCT_NOT_FILTERED),
                    "{}: the two sides give different reasons: {band:?} / {badge:?}",
                    product.id()
                );
            }
            // Whatever it says, the way out is on it.
            assert!(
                band.contains("click here to show everything"),
                "{}: the band offers no way out: {band:?}",
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

            // And the pane's own band agrees with both, for the same reason.
            let band = crate::gate_filter_ui::pane_banner_text_for(
                &app.settings_cache.gate_filter,
                (!renderer_censors_this_pane)
                    .then_some(crate::render_service::DERIVED_PRODUCT_NOT_FILTERED),
            )
            .unwrap_or_else(|| panic!("{}: a filtered pane drew no band", product.id()));
            assert_eq!(
                band.starts_with(FILTERED_WORD),
                readout.contains(FILTERED_WORD),
                "{}: the band says {band:?} and the readout says {readout:?}",
                product.id()
            );
        }
    }

    /// A pane that is showing everything says nothing, whatever its product.
    #[test]
    fn an_unfiltered_pane_draws_no_band_even_where_the_filter_could_not_run() {
        for product in DisplayProduct::ALL {
            let reason = product
                .derived_volume()
                .is_some()
                .then_some(crate::render_service::DERIVED_PRODUCT_NOT_FILTERED);
            assert_eq!(
                crate::gate_filter_ui::pane_banner_text_for(&render2d::GateFilter::OFF, reason),
                None,
                "{}: an unfiltered pane put a band on the glass",
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

        // The request carried the analyst's criteria, not the engine's
        // placeholder. This is the seam the merge had to close: both branches
        // wrote this field, and only one of them read the settings.
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

        // 3. The words. Band, legend badge and the engine's own line all quote
        //    the same summary, so the pane and the picture cannot disagree.
        let summary = app.settings_cache.gate_filter.hidden_summary();
        let band =
            crate::gate_filter_ui::pane_banner_text_for(&app.settings_cache.gate_filter, None)
                .expect("a filtered pane draws a band");
        let engine_line = filtered
            .gate_filter
            .badge()
            .expect("the engine says what it removed");
        assert!(band.contains(&summary) && engine_line.contains(&summary));
        assert!(
            crate::gate_filter_ui::pane_badge_text(&app.settings_cache.gate_filter).is_some(),
            "the legend badge went missing while gates were hidden"
        );

        // And on the glass, through the shipped `eframe::App::ui`, with this
        // real volume loaded. Two passes: the first builds the font atlas.
        app_frame(&mut app, &context, Vec::new());
        let texts = shape_texts(&app_frame(&mut app, &context, Vec::new()));
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with(FILTERED_WORD) && text.contains("show everything")),
            "a filtered pane drew no {FILTERED_WORD} band over real data: {texts:?}"
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
            crate::gate_filter_ui::pane_banner_text_for(&app.settings_cache.gate_filter, None),
            None,
            "the band outlived the filter"
        );
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
