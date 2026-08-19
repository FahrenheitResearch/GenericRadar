use std::array;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use analyst_runtime::{
    FrameOrigin, FrameStage, GenerationClock, PaneId, PaneLayout, PlaybackState, RenderStamp,
    TiltSelection, ViewportMetrics, VolumeFrame, VolumeHistory, WorkspaceState,
};
use chrono::SecondsFormat;
use color_tables::ColorTableSet;
use eframe::egui;
use map_scene::MapSceneController;
use radar_core::RadarVolume;

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

/// Settings-derived state that is read every frame, recomputed only when the
/// store reports a change. The alternative - a string-keyed store lookup per
/// pane per frame - would spend map walks on values that change a few times a
/// session, and the paint path has no business parsing choice ids.
#[derive(Clone, Copy)]
struct SettingsCache {
    /// Navigation response remaps handed to every pane - see
    /// [`crate::pane_canvas::NavTuning`] for why they are exponents.
    nav: crate::pane_canvas::NavTuning,
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

    fn toolbar(&mut self, ui: &mut egui::Ui) {
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
            ui.strong("GenericRadar");
            ui.separator();
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

            ui.separator();
            // Its own chip, so an analyst can tell "no warnings out" from "we
            // are not receiving warnings".
            let chip = match self.warnings_state.active() {
                Some(active) => format!("{} · {active}", self.warnings_state.label()),
                None => self.warnings_state.label().to_owned(),
            };
            let response = ui
                .selectable_label(self.show_warnings, chip)
                .on_hover_text(crate::app_support::warnings_hover(
                    &self.warnings_state.detail(),
                    self.show_warnings,
                    self.placed_hazards.len(),
                ));
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
                .on_hover_text("Volumetric explorer: every tilt resampled into a box and ray marched")
                .clicked()
            {
                self.vol3d.open = !self.vol3d.open;
            }
            if ui
                .selectable_label(self.xsection.armed || self.xsection.open, "XSec")
                .on_hover_text(
                    "Cross-section: arm, then click two points on a radar pane.                      A separate window shows the vertical slice of the current                      product along that line; drag the A/B handles to adjust.",
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
                    "Click two gates across a velocity couplet.
                     Needs a dealiased product: measuring folded velocity gives                      a number wrong by a multiple of the Nyquist that still                      looks reasonable.",
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
            let status = self.panes[pane.index()].status.clone();
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
            // Badges describe what limits the picture. Only what is true right
            // now; an empty list is the common case and draws nothing.
            let mut badges: Vec<String> = Vec::new();
            if let Some(frame) = self.history.current()
                && frame.stage != FrameStage::Complete
            {
                badges.push(format!("{:?}", frame.stage).to_uppercase());
            }
            // A hail product computed from a guessed freezing level and one
            // computed from a sounding are different claims. Without this the
            // two look identical on screen, which is the whole reason the
            // environment carries its provenance around with it.
            if product
                .derived_volume()
                .is_some_and(product_engine::registry::DerivedVolumeId::needs_hail_environment)
            {
                badges.push(self.hail_environment.summary());
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
            ui.label(self.timeline_status());
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

    fn timeline_status(&self) -> String {
        let Some(frame) = self.history.current() else {
            return self.status.clone();
        };
        let index = self.history.selected_index().unwrap_or(0) + 1;
        format!(
            "{} · {}/{} · {:?} · {}",
            frame.identity.site_id,
            index,
            self.history.len(),
            frame.stage,
            frame
                .identity
                .volume_time
                .to_rfc3339_opts(SecondsFormat::Secs, true)
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
        ui.separator();

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
    // exact decision points `canvas`/`timeline`/`update` drive. The full
    // `eframe::App::ui` pass is NOT driven here because `eframe::Frame` cannot
    // be constructed outside eframe and a painted pass would start basemap
    // tile fetches; every assertion below is therefore pinned at the decision
    // point the review named, with the real render worker doing real renders.

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
}
