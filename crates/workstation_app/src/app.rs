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

const MAX_LOAD_RESULTS_PER_FRAME: usize = 4;
const MAX_RENDER_RESULTS_PER_FRAME: usize = 4;
const TIMELINE_HEIGHT: f32 = 34.0;
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
        }
    }
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
            settings_registry: crate::settings_ui::catalog::registry(),
            settings_store,
            settings_ui: crate::settings_ui::SettingsUi::default(),
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

        // Palettes before anything renders. An unknown stored name falls back
        // to its family's default inside `apply_palettes` - never a blank.
        self.color_tables = Arc::new(crate::settings_ui::palettes::apply_palettes(
            &self.settings_store.workspace().palettes,
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

        // Before any volume installs, so the first frame is admitted under
        // the policy the analyst chose rather than evicted into it.
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
        };
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
            (keys::vol3d::CATEGORY, _) => self.apply_vol3d_settings(),
            // Everything else is either cache-backed (rebuilt by the caller),
            // read at startup, or declared pending its wiring seam.
            _ => {}
        }
    }

    /// The settings window, its outcome dispatch, the live-state mirror and
    /// the debounced autosave: the whole persistence pass, once per frame.
    fn settings_frame(&mut self, context: &egui::Context) {
        use crate::settings_ui::catalog::keys;
        let outcome = crate::settings_ui::draw_settings_window(
            context,
            &mut self.settings_ui,
            crate::settings_ui::SettingsWindowInput {
                registry: &self.settings_registry,
                store: &mut self.settings_store,
                color_tables: Some(&mut self.color_tables),
            },
        );
        if outcome.palette_changed {
            // Exactly what the toolbar's palette picker does: new colours,
            // same data.
            self.palette_clock.bump();
            self.invalidate_view_panes(self.workspace.visible_panes());
        }
        for (category, id) in &outcome.changed {
            self.apply_changed_setting(category, id);
            // The theme needs the context, which `apply_changed_setting`
            // deliberately does not carry - it is the one setting that
            // restyles egui itself rather than the app's own state.
            if (category.as_str(), id.as_str())
                == (keys::appearance::CATEGORY, keys::appearance::THEME)
            {
                let variant = if self.settings_store.effective_text(
                    &self.settings_registry,
                    keys::appearance::CATEGORY,
                    keys::appearance::THEME,
                ) == "dark"
                {
                    crate::theme::Variant::Dark
                } else {
                    crate::theme::Variant::Light
                };
                crate::theme::apply(context, variant);
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
        workspace.palettes = crate::settings_ui::palettes::capture_palettes(&self.color_tables);
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
        if !self.settings_cache.vrot_mps_first {
            return crate::vrot::report(measurement);
        }
        let mut text = format!(
            "{} | delta-V {:.1} m/s | separation {:.2} km | height {:.2} km ARL | {:.1} deg cut {}",
            self.vrot_readout(measurement),
            measurement.delta_v_mps,
            measurement.separation_km,
            measurement.couplet_height_arl_m / 1000.0,
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
                    let source_label = format!(
                        "{site} {}",
                        volume_time.to_rfc3339_opts(SecondsFormat::Secs, true)
                    );
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
                RenderUpdate::Completed(rendered) => self.install_render(context, rendered),
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
        }
        // A view-stale install leaves `pending_stamp` alone on purpose: the
        // exact-stamp render is still owed, and `ensure_render_requested`
        // keeps asking for it. `visible_panes_ready` compares stamps exactly,
        // so playback gating is unchanged by the stale pixels.
    }

    fn handle_dropped_files(&mut self, context: &egui::Context) {
        let dropped = context.input(|input| input.raw.dropped_files.clone());
        if let Some(path) = dropped.into_iter().find_map(|file| file.path) {
            self.begin_load(path);
        }
    }

    fn advance_playback(&mut self, context: &egui::Context) {
        if self.history.playback() != PlaybackState::Playing || self.history.len() < 2 {
            return;
        }
        if !self.visible_panes_ready() {
            context.request_repaint_after(Duration::from_millis(16));
            return;
        }
        let elapsed = self.last_playback_step.elapsed();
        if elapsed < PLAYBACK_FRAME_TIME {
            context.request_repaint_after(PLAYBACK_FRAME_TIME - elapsed);
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

        let active = self.workspace.active_pane;
        let current_product = DisplayProduct::from_product_id(&self.workspace.active().product);
        let mut requested_load = None;
        let mut live_action = None;
        let mut selected_layout = self.workspace.layout;
        let mut selected_product = current_product;
        let mut quality_changed = false;
        let mut palette_changed = false;
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
                    ui.label("Open a Level II archive");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.source_path_text)
                            .desired_width(260.0)
                            .hint_text("Level II file path"),
                    );
                    if ui.button("Load file").clicked() && !self.source_path_text.trim().is_empty()
                    {
                        requested_load = Some(PathBuf::from(self.source_path_text.trim()));
                        ui.close();
                    }
                    bevel::etched_separator(ui);
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
                    egui::ComboBox::from_id_salt("workstation-palette")
                        .selected_text(installed.name())
                        .width(210.0)
                        .show_ui(ui, |ui| {
                            for table in color_tables::palette_offers_for_family(family, &installed)
                            {
                                let chosen = table.name() == installed.name();
                                if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                                    Arc::make_mut(&mut self.color_tables).set_family(family, table);
                                    self.palette_clock.bump();
                                    palette_changed = true;
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "Colour table for this product's family. The last row is the \
                         selected palette redrawn the other way: smooth or stepped.",
                        );
                }

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

        if quality_changed || palette_changed {
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
                    .hint_text("Level II file path"),
            );
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
                egui::ComboBox::from_id_salt("workstation-palette")
                    .selected_text(installed.name())
                    .width(210.0)
                    .show_ui(ui, |ui| {
                        for table in color_tables::palette_offers_for_family(family, &installed) {
                            let chosen = table.name() == installed.name();
                            if ui.selectable_label(chosen, table.name()).clicked() && !chosen {
                                Arc::make_mut(&mut self.color_tables).set_family(family, table);
                                self.palette_clock.bump();
                                palette_changed = true;
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Colour table for this product's family. The last row is the \
                         selected palette redrawn the other way: smooth or stepped.",
                    );
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

        if quality_changed || palette_changed {
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
                active_site: self.live_site.clone(),
                hazards: Arc::clone(&self.placed_hazards),
            };
            let badges = self.pane_badges(product, now);
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
            );

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
                        let status = choice.status_line();
                        self.site_text = choice.id.to_uppercase();
                        self.start_live(choice.id);
                        // AFTER the load kick: `start_live` writes its own
                        // status, so setting this first would be invisible.
                        self.status = status;
                    }
                    None => self.status = crate::nearest_site::no_site_in_range_status(),
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
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &descriptor.computation.source_moment(),
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
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
        let reading = crate::probe::probe_polar(
            volume,
            cut_index,
            &descriptor.computation.source_moment(),
            elevation_deg,
            volume.site.elevation_m,
            east_km,
            north_km,
        );
        self.panes[pane.index()].probe_text = Some(crate::probe::format_reading(
            &reading,
            &descriptor.domain,
            descriptor.short_name,
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
        let when = feed
            .newest_volume_time
            .to_rfc3339_opts(SecondsFormat::Secs, true);
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
            frame
                .identity
                .volume_time
                .to_rfc3339_opts(SecondsFormat::Secs, true),
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
        self.toolbar(ui);
        // No separator under the bar: the band paints its own raised bevel,
        // and a stock hairline immediately below it reads as a second, weaker
        // edge drawn by someone who could not see the first.
        ui.add_space(2.0);

        let available = ui.available_size();
        let canvas_height = (available.y - TIMELINE_HEIGHT).max(120.0);
        let (canvas_rect, _) = ui.allocate_exact_size(
            egui::vec2(available.x.max(1.0), canvas_height),
            egui::Sense::hover(),
        );
        self.canvas(ui, canvas_rect);

        ui.separator();
        self.timeline(ui, &context);
        self.settings_frame(&context);

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
        for variant in [crate::theme::Variant::Light, crate::theme::Variant::Dark] {
            let palette = crate::theme::palette::Palette::of(variant);
            let context = egui::Context::default();
            crate::theme::apply(&context, variant);
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
                    "{variant:?}: no shape in the frame fills the whole viewport with the panel \
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
                    "{variant:?}: the ground is painted at shape {ground}, after the first text \
                     run at {first_text} - it would cover the chrome instead of backing it"
                );
            }

            // Exactly how `eframe::native::wgpu_integration` asks for it.
            let clear =
                <WorkstationApp as eframe::App>::clear_color(&app, &context.global_style().visuals);
            assert_eq!(
                clear,
                palette.face.to_opaque().to_normalized_gamma_f32(),
                "{variant:?}: the window clear colour is not the ground the app paints - every \
                 resize tears a seam of eframe's near-black default"
            );
            assert_eq!(
                clear[3], 1.0,
                "{variant:?}: a see-through clear colour lets the desktop through"
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
        crate::theme::apply(&context, crate::theme::Variant::Light);
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
            "Level II file path".to_owned(),
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
        use crate::theme::{Variant, apply};

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

        for (variant, palette) in [(Variant::Light, &LIGHT), (Variant::Dark, &DARK)] {
            let context = egui::Context::default();
            apply(&context, variant);
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
            app.live_feed = Some(stalled_kuex_feed());

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
