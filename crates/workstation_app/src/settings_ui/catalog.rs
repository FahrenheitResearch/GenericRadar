//! The workstation's settings catalog: every category and knob the master
//! settings window offers, declared as plain data.
//!
//! This module is the single place the application's knobs are enumerated.
//! The window (`settings_ui`) renders whatever is declared here; the store
//! persists by `(category id, setting id)`; `app.rs` applies changes by
//! matching on the constants in [`keys`]. A new crate that wants a page of
//! its own does not edit this file - it declares its own
//! `settings::SettingsCategory` and the application registers it beside
//! [`registry`]'s output (see `docs/extending.md`).
//!
//! Defaults here are the application's *current shipped behaviour*, read
//! from the owning modules (`render2d::DisplayQuality::default`,
//! `map_scene::MapStylePreset::default`, `vol3d::Vol3d::default`, the
//! navigation constants in `analyst_runtime::view`), so that a fresh
//! settings file changes nothing. Where the owning type is reachable from
//! here, a test pins the equality; the `vol3d` numbers are mirrored by hand
//! from `vol3d.rs` because that module lives behind the binary crate.

use settings::{
    ChoiceOption, SettingKind, SettingSpec, SettingsCategory, SettingsRegistry, SliderFloor,
};

/// Stable identifiers. These strings are the persistence contract: they name
/// values in every settings file already written, so they are never reused
/// for a different meaning (renaming one orphans the stored value, which is
/// safe; reusing one misreads it, which is not).
pub mod keys {
    pub mod appearance {
        /// The Appearance page. The theme, accent, chrome-edge, density and
        /// scale settings on this page are declared by the theme module
        /// itself (`theme::settings`), because their options are derived
        /// from the theme catalog and this file is also compiled where the
        /// catalog does not exist. Two categories that share an id merge, so
        /// the toolbar setting below lands on the same page, after them.
        pub const CATEGORY: &str = "appearance";
        pub const TOOLBAR: &str = "toolbar";
    }
    pub mod map {
        pub const CATEGORY: &str = "map";
        pub const BASEMAP_STYLE: &str = "basemap_style";
        pub const IMAGERY_PROVIDER: &str = "imagery_provider";
        pub const IMAGERY_DIM_AUTO: &str = "imagery_dim_auto";
        pub const IMAGERY_DIM: &str = "imagery_dim";
        pub const SITE_MARKERS: &str = "site_markers";
        pub const SITE_LABELS: &str = "site_labels";
        pub const BOUNDARIES: &str = "boundaries";
        /// RETIRED, and not to be reused: `map/range_rings`.
        ///
        /// It was declared `pending_wiring` for a range-ring layer that had in
        /// fact always been drawn, and it remained after the live ladder became
        /// `annotation/ring_ladder`. The window therefore offered TWO rows
        /// labelled "Range rings", the dead one
        /// reading "Off" above the live one, and a search for "range rings"
        /// found the lie first. The row is gone; the id is recorded here so
        /// nobody gives it a second meaning, because a settings file written
        /// by an older build still carries it and reusing the id would
        /// misread that file. An orphaned value is ignored, which is safe.
        pub const RETIRED_RANGE_RINGS: &str = "range_rings";
    }
    pub mod radar {
        pub const CATEGORY: &str = "radar";
        pub const QUALITY: &str = "quality";
        pub const SWEEP_ANIMATION: &str = "sweep_animation";
        pub const SWEEP_SPEED: &str = "sweep_speed";
        pub const LEGEND: &str = "legend";
        // The gate filter: five criteria, every one of them off in a fresh
        // file. See `crate::gate_filter_ui` for the mapping onto
        // `render2d::GateFilter` and for why "off" is a real number on the
        // slider rather than a companion toggle.
        pub const FILTER_MIN_DBZ: &str = "filter_min_dbz";
        pub const FILTER_VEL_NEEDS_DBZ: &str = "filter_vel_needs_dbz";
        pub const FILTER_MIN_RHO: &str = "filter_min_rho";
        pub const FILTER_HIDE_RF: &str = "filter_hide_rf";
        pub const FILTER_MIN_RANGE_KM: &str = "filter_min_range_km";
    }
    pub mod navigation {
        pub const CATEGORY: &str = "navigation";
        pub const ZOOM_PER_NOTCH: &str = "zoom_per_notch";
        pub const BURST_GAIN_CAP: &str = "burst_gain_cap";
        pub const KEY_PAN_RATE: &str = "key_pan_rate";
        pub const KEY_ZOOM_RATE: &str = "key_zoom_rate";
        pub const DOUBLE_CLICK_RESET: &str = "double_click_reset";
    }
    pub mod vol3d {
        pub const CATEGORY: &str = "vol3d";
        pub const THRESHOLD_DBZ: &str = "threshold_dbz";
        pub const THRESHOLD_MODE: &str = "threshold_mode";
        pub const OPACITY: &str = "opacity";
        pub const DENSITY: &str = "density";
        pub const SHADING: &str = "shading";
        pub const QUALITY: &str = "quality";
        pub const BOX_SIZE_KM: &str = "box_size_km";
        pub const VERTICAL_EXAGGERATION: &str = "vertical_exaggeration";
        pub const FOV_SCALE: &str = "fov_scale";
        pub const FLOOR_MODE: &str = "floor_mode";
        pub const FLOOR_OPACITY: &str = "floor_opacity";
        pub const RAMP_LOW_DBZ: &str = "opacity_ramp_low_dbz";
        pub const RAMP_HIGH_DBZ: &str = "opacity_ramp_high_dbz";
        pub const RAMP_GAMMA: &str = "opacity_ramp_gamma";
        pub const RAMP_FLOOR: &str = "opacity_ramp_floor";
        pub const RAMP_GAIN: &str = "opacity_ramp_gain";
        pub const SHOW_GRID: &str = "show_grid";
        pub const SHOW_BOX: &str = "show_box";
        pub const SHOW_LABELS: &str = "show_labels";
    }
    pub mod analysis {
        pub const CATEGORY: &str = "analysis";
        pub const VROT_UNITS: &str = "vrot_units";
        pub const VROT_MARKER_SIZE: &str = "vrot_marker_size";
        pub const STORM_MOTION_DIR: &str = "storm_motion_dir";
        pub const STORM_MOTION_SPEED: &str = "storm_motion_speed";
    }
    pub mod units {
        pub const CATEGORY: &str = "units";
        pub const DISTANCE: &str = "distance";
        pub const ALTITUDE: &str = "altitude";
        pub const TIME_ZONE: &str = "time_zone";
        pub const CLOCK: &str = "clock";
    }
    pub mod network {
        pub const CATEGORY: &str = "network";
        pub const ARCHIVE_POLL_SECONDS: &str = "archive_poll_seconds";
        pub const ARCHIVE_LEAD_MINUTES: &str = "archive_lead_minutes";
        pub const STALL_AFTER_SECONDS: &str = "stall_after_seconds";
        pub const DOWNLOAD_BATCH: &str = "download_batch";
        pub const DOWNLOAD_ATTEMPTS: &str = "download_attempts";
        pub const RETRY_BACKOFF_MS: &str = "retry_backoff_ms";
    }
    pub mod annotation {
        pub const CATEGORY: &str = "annotation";
        pub const RING_LADDER: &str = "ring_ladder";
        pub const RING_COUNT: &str = "ring_count";
        pub const RING_LABELS: &str = "ring_labels";
        pub const SITE_MARKER_SIZE: &str = "site_marker_size";
        pub const SITE_LABEL_SIZE: &str = "site_label_size";
        pub const SITE_DECLUTTER_MAX: &str = "site_declutter_max";
        pub const SITE_MARKER_MAX: &str = "site_marker_max";
        pub const RANGE_DECIMALS: &str = "range_decimals";
        pub const COORDINATE_DECIMALS: &str = "coordinate_decimals";
        pub const CORNER_READOUT: &str = "corner_readout";
    }
    pub mod profiles {
        /// The same string `settings::profiles::BOOKKEEPING_CATEGORY` names,
        /// because the page and the active-profile pointer stored under it
        /// have to be the same category: the pointer is what makes the
        /// pointer's own page findable.
        pub const CATEGORY: &str = settings::profiles::BOOKKEEPING_CATEGORY;
        pub const SHOW_IN_STATUS: &str = "show_in_status";
    }
    pub mod data {
        pub const CATEGORY: &str = "data";
        pub const STARTUP_SITE: &str = "startup_site";
        pub const RESUME_LAST_SITE: &str = "resume_last_site";
        pub const POLL_SECONDS: &str = "poll_seconds";
        pub const FOLLOW_LOW_TILTS_ENABLED: &str = "follow_low_tilts_enabled";
        pub const FOLLOW_MAX_ELEVATION_DEG: &str = "follow_max_elevation_deg";
        pub const FOLLOW_MIN_SWEEP_INTERVAL_SECONDS: &str = "follow_min_sweep_interval_seconds";
        pub const HISTORY_MAX_FRAMES: &str = "history_max_frames";
        pub const HISTORY_MAX_MB: &str = "history_max_mb";
        pub const LIVE_CACHE_LIMIT_MB: &str = "live_cache_limit_mb";
        pub const TILE_CACHE_LIMIT_MB: &str = "tile_cache_limit_mb";
        pub const LOOP_FRAME_MS: &str = "loop_frame_ms";
        /// The folder `File > Open…` starts in. Written by the browser
        /// itself every time a folder is read successfully, which is what
        /// makes it a memory rather than a preference nobody would set.
        pub const OPEN_FOLDER: &str = "open_folder";
    }
    pub mod xsection {
        pub const CATEGORY: &str = "xsection";
        pub const TOP_KM: &str = "top_km";
    }
    /// NEXRAD Level 1 (time series / I/Q).
    ///
    /// Unlike every other page here, these are not display preferences. A
    /// Level II volume arrives with its moments already estimated and these
    /// choices already made, unrecorded, by the signal processor; a Level 1
    /// record arrives as pulses, and what is on screen is whatever these
    /// choices say it is. Changing one is changing the measurement, not the
    /// picture of it.
    pub mod timeseries {
        pub const CATEGORY: &str = "timeseries";
        pub const DWELL_PULSES: &str = "dwell_pulses";
        pub const WINDOW: &str = "window";
        pub const SNR_MIN_DB: &str = "snr_min_db";
        pub const SPECTRUM_CHANNEL: &str = "spectrum_channel";
    }
}

/// The Level 1 page's numbers, mirrored by hand from `crate::iq_session` and
/// `nexrad_io::iq_moments`.
///
/// Mirrored rather than imported for the reason the `vol3d` numbers are: this
/// file is also compiled by the `settings` crate's UI harness, which has
/// neither the binary crate nor `nexrad_io` on its dependency list. The
/// equality is pinned by a test in `workstation_app` that CAN see both, so the
/// mirror cannot drift without something going red.
pub mod timeseries_limits {
    /// Pulses per dwell: `iq_session::MIN_DWELL_PULSES` and `MAX_DWELL_PULSES`,
    /// with `nexrad_io::iq_moments::DwellPlan::default`'s count as the default.
    pub const MIN_DWELL: i64 = 8;
    pub const MAX_DWELL: i64 = 512;
    pub const DEFAULT_DWELL: i64 = 64;
    /// The SNR censor, in dB. The floor means *off* - no threshold at all - on
    /// the same principle as the gate filter's four criteria, and the default
    /// is the operational WSR-88D threshold
    /// (`nexrad_io::iq_moments::estimator::SnrCensor::OPERATIONAL`) so that a
    /// first look at a Level 1 file shows the population of gates the Level II
    /// product of the same scan would.
    pub const OFF_SNR_DB: f64 = -10.0;
    pub const MAX_SNR_DB: f64 = 20.0;
    pub const DEFAULT_SNR_DB: f64 = 2.0;
    /// Window ids, matching `nexrad_io::iq_moments::taper::Taper`.
    pub const WINDOW_RECTANGULAR: &str = "rectangular";
    pub const WINDOW_VON_HANN: &str = "von_hann";
    pub const WINDOW_HAMMING: &str = "hamming";
    pub const WINDOW_BLACKMAN: &str = "blackman";
    pub const CHANNEL_HORIZONTAL: &str = "h";
    pub const CHANNEL_VERTICAL: &str = "v";
}

/// Ranges for the gate filter's four numeric criteria, and the value each one
/// reads as "off".
///
/// Two things are true of every `OFF_*` constant here, and both matter:
///
/// * it is the **bottom of its own slider**, so "all the way left" is off and
///   an analyst never has to hunt for a separate enable toggle - which is why
///   this is five keys and not nine;
/// * it is also a number that **would hide nothing if it were applied
///   literally**. Reflectivity on a WSR-88D is encoded from -32.0 dBZ upward
///   (NOAA/NWS ICD 2620002, the Level II data format), so -35 dBZ is below
///   every gate that can exist; RhoHV is a correlation and cannot be negative,
///   so 0.00 is below every gate; nothing is closer to the radar than 0 km.
///   So a future build that read the number and skipped the sentinel check
///   would still censor nothing, which is the failure direction a filter must
///   have.
///
/// The conversion between these numbers and `render2d::GateFilter`'s
/// `Option<f32>` fields lives in `crate::gate_filter_ui`; the constants are
/// here because a range is a declaration and this file is where declarations
/// live.
pub mod radar_filter {
    pub const OFF_MIN_DBZ: f64 = -35.0;
    pub const MAX_MIN_DBZ: f64 = 40.0;
    pub const OFF_MIN_RHO: f64 = 0.0;
    pub const MAX_MIN_RHO: f64 = 1.0;
    pub const OFF_MIN_RANGE_KM: f64 = 0.0;
    pub const MAX_MIN_RANGE_KM: f64 = 40.0;
}

fn toggle(default: bool) -> SettingKind {
    SettingKind::Toggle { default }
}

fn slider(min: f64, max: f64, default: f64, decimals: u8, unit: &str) -> SettingKind {
    SettingKind::Slider {
        min,
        max,
        default,
        decimals,
        unit: unit.to_owned(),
        floor: SliderFloor::Number,
    }
}

/// A slider whose leftmost stop means *off* rather than "the smallest number".
///
/// Everything the gate filter's four thresholds are declared with. Two things
/// follow from the declaration and neither is cosmetic: the window's readout
/// says "off" at that stop instead of printing a threshold nobody chose, and a
/// stored number outside the range falls back to the default (off) instead of
/// clamping to the strongest censor the control offers. See
/// `settings::SliderFloor`.
fn off_at_left(min: f64, max: f64, decimals: u8, unit: &str) -> SettingKind {
    SettingKind::Slider {
        min,
        max,
        // The off position IS the default: a fresh file hides nothing.
        default: min,
        decimals,
        unit: unit.to_owned(),
        floor: SliderFloor::Off,
    }
}

fn integer(min: i64, max: i64, default: i64, unit: &str) -> SettingKind {
    SettingKind::Integer {
        min,
        max,
        default,
        unit: unit.to_owned(),
    }
}

fn choice(options: Vec<ChoiceOption>, default_id: &str) -> SettingKind {
    SettingKind::Choice {
        options,
        default_id: default_id.to_owned(),
    }
}

/// The whole catalog. Order is the order the window lists categories.
///
/// The Appearance page is NOT here: `theme::settings::settings_category`
/// declares it, and `settings_ui::full_registry` puts it first. This
/// function is what a caller with no theme module gets - the harness in the
/// `settings` crate - and it still registers the toolbar setting on an
/// Appearance page of its own, which merges with the theme's when both are
/// registered.
pub fn registry() -> SettingsRegistry {
    let mut registry = SettingsRegistry::new();
    register_into(&mut registry);
    registry
}

/// Add every category this file declares to an existing registry, in order.
///
/// Split out of [`registry`] so the application can register the theme's
/// Appearance page FIRST and still get the rest in the order below - the
/// registry appends, and the order of registration is the order of the
/// category list in the window.
pub fn register_into(registry: &mut SettingsRegistry) {
    registry.register(appearance_category());
    registry.register(map_category());
    registry.register(radar_category());
    registry.register(navigation_category());
    registry.register(vol3d_category());
    registry.register(analysis_category());
    registry.register(data_category());
    registry.register(units_category());
    registry.register(network_category());
    registry.register(annotation_category());
    registry.register(xsection_category());
    registry.register(timeseries_category());
    // Last, because it is the page about the other pages: a named snapshot of
    // everything above it, including Units, Data & network, Readout &
    // annotation, and Cross-sections.
    registry.register(profiles_category());
}

fn appearance_category() -> SettingsCategory {
    use keys::appearance as k;
    let toolbar_options = vec![
        ChoiceOption::new("menus", "Menu bar (compact)"),
        ChoiceOption::new("full", "Everything visible"),
    ];
    SettingsCategory::new(
        k::CATEGORY,
        "Appearance",
        vec![
            SettingSpec::new(
                k::TOOLBAR,
                "Toolbar style",
                choice(toolbar_options, "menus"),
            )
            .help(
                "Menu bar keeps one compact row - storm controls stay on it, the \
                 occasional ones live under File / View / Map / Tools. Everything \
                 visible puts every control on the row itself, which wraps on \
                 narrower windows.",
            ),
        ],
    )
}

fn map_category() -> SettingsCategory {
    use keys::map as k;
    let style_options = map_scene::MapStylePreset::ALL
        .into_iter()
        .map(|preset| ChoiceOption::new(preset.id(), preset.label()))
        .collect();
    let mut provider_options = vec![ChoiceOption::new("none", "No imagery")];
    provider_options.extend(
        map_scene::TileProvider::ALL
            .into_iter()
            .map(|provider| ChoiceOption::new(provider.key(), provider.label())),
    );
    SettingsCategory::new(
        k::CATEGORY,
        "Map",
        vec![
            SettingSpec::new(
                k::BASEMAP_STYLE,
                "Basemap look",
                choice(style_options, map_scene::MapStylePreset::default().id()),
            )
            .help(
                "Slate Dark is the shipped map. High Contrast is for a lit room or a \
                 projector; Daylight is dark ink on a light pane; Minimal thins the lines \
                 and holds counties back until twice the zoom.",
            ),
            SettingSpec::new(
                k::IMAGERY_PROVIDER,
                "Ground imagery",
                choice(provider_options, "none"),
            )
            .help(
                "Raster imagery drawn under the radar, boundaries still on top. USGS \
                 layers are U.S. Government works; OpenStreetMap is community-run and \
                 fetches only what is on screen. Attribution is drawn bottom right and \
                 is a condition of use - there is no switch for it.",
            ),
            SettingSpec::new(
                k::IMAGERY_DIM_AUTO,
                "Dim imagery automatically",
                toggle(true),
            )
            .help(
                "Measure how much to dim the imagery from the tiles that actually arrive \
                 - a white topo map needs far more than an aerial photo. Turn off to set \
                 the dim by hand below.",
            ),
            SettingSpec::new(k::IMAGERY_DIM, "Imagery dim", slider(0.0, 0.9, 0.35, 2, "")).help(
                "How far the imagery is dimmed towards the pane's own ground, so weak \
                 reflectivity and near-zero velocity stay readable on top of it. Applies \
                 when automatic dimming is off.",
            ),
            SettingSpec::new(k::SITE_MARKERS, "Radar site markers", toggle(true)).help(
                "Draw every NEXRAD site as a clickable marker. Clicking one is the \
                 quickest way to change radar.",
            ),
            SettingSpec::new(
                k::SITE_LABELS,
                "Site labels",
                choice(
                    vec![
                        ChoiceOption::new("auto", "When uncluttered"),
                        ChoiceOption::new("always", "Always"),
                        ChoiceOption::new("never", "Never"),
                    ],
                    "auto",
                ),
            )
            .help(
                "When to write the four-letter id beside a site marker. 'When \
                 uncluttered' labels every site while no more than Readout & \
                 annotation > 'Label every site up to' are on screen, and the hovered \
                 or active site always; 'Never' writes no ids at all.",
            ),
            // `map/range_rings` used to be declared here, greyed out and
            // defaulting to "Off", for a range-ring layer that had always
            // existed. The live rings are `annotation/ring_ladder` on the
            // Readout & annotation page. See `keys::map::RETIRED_RANGE_RINGS`.
            SettingSpec::new(
                k::BOUNDARIES,
                "Boundary detail",
                choice(
                    vec![
                        ChoiceOption::new("all", "Countries, states, counties"),
                        ChoiceOption::new("no-counties", "Countries and states"),
                        ChoiceOption::new("states-only", "States only"),
                    ],
                    "all",
                ),
            )
            .help(
                "Which boundary classes are drawn at all, independent of the look \
                 preset. Declared ahead of per-class visibility in the map style.",
            )
            .pending_wiring(),
        ],
    )
}

fn radar_category() -> SettingsCategory {
    use keys::radar as k;
    let quality_options = vec![
        ChoiceOption::new("native", "Native"),
        ChoiceOption::new("smooth", "Smooth"),
        ChoiceOption::new("high", "High"),
        ChoiceOption::new("ultra", "Ultra"),
    ];
    SettingsCategory::new(
        k::CATEGORY,
        "Radar",
        vec![
            SettingSpec::new(
                k::QUALITY,
                "Display quality",
                choice(quality_options, "smooth"),
            )
            .help(
                "Smooth adds sub-beams and sub-gates so a gate stops being a visible \
                     block; High and Ultra also supersample, which removes the speckle of \
                     a zoomed-out view. Ultra costs about sixteen times the native raster \
                     per frame - worth it for a still, heavy on a fast loop.",
            ),
            SettingSpec::new(k::SWEEP_ANIMATION, "Sweep animation", toggle(true)).help(
                "Reveal incoming live radials as a clockwise sweep at the antenna's own \
                 measured rate, instead of repainting the entire tilt at once. This is \
                 enabled by default when automatically following arriving low tilts.",
            ),
            SettingSpec::new(
                k::SWEEP_SPEED,
                "Sweep catch-up",
                slider(0.25, 4.0, 1.0, 2, "×"),
            )
            .help(
                "Multiplier on the wipe's pace. 1× follows the antenna; higher \
                     values close a backlog faster after a stall, lower values draw the \
                     wipe out for demonstration.",
            ),
            SettingSpec::new(k::LEGEND, "Colour legend", toggle(true)).help(
                "Draw each pane's colour bar. The bar always shows exactly the table \
                 the pixels were painted with.",
            ),
            SettingSpec::new(
                k::FILTER_MIN_DBZ,
                "Gate filter: hide reflectivity below",
                off_at_left(
                    radar_filter::OFF_MIN_DBZ,
                    radar_filter::MAX_MIN_DBZ,
                    1,
                    "dBZ",
                ),
            )
            .help(
                "Leave gates weaker than this unpainted. All the way left is off and \
                 nothing is hidden. Around 5 dBZ clears most of a summer bloom; it also \
                 clears light snow, drizzle, the far edge of an anvil and the weak-echo \
                 side of a hook. Whenever this is on, every pane says so.",
            ),
            SettingSpec::new(
                k::FILTER_VEL_NEEDS_DBZ,
                "Gate filter: hide velocity with no echo",
                off_at_left(
                    radar_filter::OFF_MIN_DBZ,
                    radar_filter::MAX_MIN_DBZ,
                    1,
                    "dBZ",
                ),
            )
            .help(
                "Leave a velocity gate unpainted when the reflectivity at the same place \
                 is weaker than this. On a split-cut VCP the reflectivity comes from the \
                 surveillance sweep at the same elevation, matched by azimuth and range - \
                 when the volume carries no such sweep this does nothing rather than \
                 emptying the pane. All the way left is off.",
            ),
            SettingSpec::new(
                k::FILTER_MIN_RHO,
                "Gate filter: hide below RhoHV",
                off_at_left(radar_filter::OFF_MIN_RHO, radar_filter::MAX_MIN_RHO, 2, ""),
            )
            .help(
                "Leave gates whose correlation coefficient is below this unpainted. \
                 Birds, insects, chaff and ground clutter scatter incoherently and read \
                 low; rain reads above about 0.97. 0.00 is off. This is the one to be \
                 most careful with: a debris ball, the melting layer and a hail shaft \
                 also read low, so a cut that removes a bloom removes them too.",
            ),
            SettingSpec::new(
                k::FILTER_HIDE_RF,
                "Gate filter: hide range folded",
                toggle(false),
            )
            .help(
                "Leave range-folded gates unpainted instead of drawing them in the \
                     table's RF colour. The purple tells you the Doppler ambiguity could \
                     not be resolved there; hiding it makes the pane cleaner and makes \
                     that ambiguity look like clear air.",
            ),
            SettingSpec::new(
                k::FILTER_MIN_RANGE_KM,
                "Gate filter: hide inside",
                off_at_left(
                    radar_filter::OFF_MIN_RANGE_KM,
                    radar_filter::MAX_MIN_RANGE_KM,
                    1,
                    "km",
                ),
            )
            .help(
                "Leave everything closer to the radar than this unpainted - the near-field \
                 ring of ground clutter and the roosting bloom that sits on top of the \
                 site. 0 km is off. A storm over the radar is inside this circle too.",
            ),
        ],
    )
}

fn navigation_category() -> SettingsCategory {
    use keys::navigation as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Navigation",
        vec![
            SettingSpec::new(
                k::ZOOM_PER_NOTCH,
                "Zoom per notch",
                slider(1.05, 1.5, 1.2, 2, "×"),
            )
            .help(
                "Scale change of one deliberate wheel click. 1.2 is 20% per click; \
                     smaller is finer. Spinning the wheel still accelerates on top of \
                     this - see the burst cap below.",
            ),
            SettingSpec::new(
                k::BURST_GAIN_CAP,
                "Scroll burst cap",
                slider(1.0, 8.0, 5.0, 1, "×"),
            )
            .help(
                "The most a notch can be worth while the wheel is being spun hard. \
                     1 disables the acceleration entirely; the default tops a flick out \
                     at about 2.5× per notch. Declared ahead of a tunable cap in the \
                     scroll responder, so the choice is stored when that lands.",
            )
            // Unlike zoom-per-notch and the keyboard rates - which the
            // composition root can wire through exponent/dt multipliers at
            // the existing call sites - the burst state lives inside
            // `analyst_runtime::view`'s scroll responder (`MAX_BURST_GAIN`
            // clamps `self.burst` internally) and no call-site remap is
            // exact. Declared so the choice is stored; flip to enabled in
            // the same change that gives the responder a tunable cap.
            .pending_wiring(),
            SettingSpec::new(
                k::KEY_PAN_RATE,
                "Keyboard pan speed",
                slider(0.3, 3.0, 1.2, 2, "panes/s"),
            )
            .help(
                "Fractions of the pane a held arrow key crosses per second. The default \
                 crosses a pane in a little under a second.",
            ),
            SettingSpec::new(
                k::KEY_ZOOM_RATE,
                "Keyboard zoom speed",
                slider(2.0, 12.0, 6.0, 1, "×/s"),
            )
            .help("Scale change per second while a zoom key is held."),
            SettingSpec::new(
                k::DOUBLE_CLICK_RESET,
                "Double-click resets the view",
                toggle(true),
            )
            .help(
                "Double-clicking a pane returns its camera to the home view. Turn \
                     off if you double-click while measuring.",
            ),
        ],
    )
}

fn vol3d_category() -> SettingsCategory {
    use keys::vol3d as k;
    SettingsCategory::new(
        k::CATEGORY,
        "3D Volume",
        vec![
            SettingSpec::new(
                k::THRESHOLD_DBZ,
                "Threshold",
                slider(-30.0, 70.0, 12.0, 0, "dBZ"),
            )
            .help(
                "Echo weaker than this is not drawn. 12 dBZ lets the weak echo read \
                     as the cloud body with the cores solid inside it; raise it to strip \
                     the volume back to the cores alone.",
            )
            .group("Echo threshold"),
            SettingSpec::new(
                k::THRESHOLD_MODE,
                "Threshold side",
                choice(
                    vec![
                        ChoiceOption::new("above", "Keep above"),
                        ChoiceOption::new("below", "Keep below"),
                    ],
                    "above",
                ),
            )
            .help("Keep echo above the threshold (normal) or below it (weak-echo work).")
            .group("Echo threshold"),
            SettingSpec::new(k::OPACITY, "Opacity", slider(0.02, 1.0, 0.28, 2, ""))
                .help(
                    "How much each sample absorbs. Low values see through the storm; high \
                 values read the surface only.",
                )
                .group("How the volume is drawn"),
            SettingSpec::new(k::DENSITY, "Density", slider(0.2, 4.0, 0.78, 2, ""))
                .help(
                    "How quickly repeated samples accumulate into a solid body. Opacity \
                 controls each sample; density controls the pile-up.",
                )
                .group("How the volume is drawn"),
            SettingSpec::new(k::SHADING, "Shading", slider(0.0, 1.0, 0.9, 2, ""))
                .help("Blend from untouched palette colour (0) to lit cloud shading (1).")
                .group("How the volume is drawn"),
            SettingSpec::new(
                k::QUALITY,
                "Ray-march quality",
                choice(
                    vec![
                        ChoiceOption::new("draft", "Draft"),
                        ChoiceOption::new("balanced", "Balanced"),
                        ChoiceOption::new("high", "High"),
                    ],
                    "balanced",
                ),
            )
            .help("Steps per ray: 96, 160 or 240. Higher is smoother and costs GPU time.")
            .group("How the volume is drawn"),
            SettingSpec::new(
                k::BOX_SIZE_KM,
                "Box size",
                choice(
                    vec![
                        ChoiceOption::new("30", "30 km"),
                        ChoiceOption::new("60", "60 km"),
                        ChoiceOption::new("120", "120 km"),
                        ChoiceOption::new("240", "240 km"),
                        ChoiceOption::new("360", "360 km"),
                    ],
                    "60",
                ),
            )
            .help(
                "Width of the resampled box. 60 km frames one supercell; 240 km \
                 frames a line. The same choices the 3D pane itself offers.",
            )
            .group("Box and camera"),
            SettingSpec::new(
                k::VERTICAL_EXAGGERATION,
                "Vertical exaggeration",
                slider(0.5, 6.0, 1.5, 1, "×"),
            )
            .help(
                "Purely visual stretch of height. 1× preserves physical proportions; \
                 the 1.5× default keeps a storm reading broader than deep, which is \
                 the truth.",
            )
            .group("Box and camera"),
            SettingSpec::new(k::FOV_SCALE, "Field of view", slider(0.42, 1.1, 0.7, 2, ""))
                .help("Perspective strength of the 3D camera.")
                .group("Box and camera"),
            SettingSpec::new(
                k::FLOOR_MODE,
                "Floor",
                choice(
                    vec![
                        ChoiceOption::new("off", "Off"),
                        ChoiceOption::new("lowest-tilt", "Lowest tilt"),
                        ChoiceOption::new("column-max", "Column max"),
                    ],
                    "lowest-tilt",
                ),
            )
            .help("The reference raster drawn under the volume.")
            .group("Ground plane"),
            SettingSpec::new(
                k::FLOOR_OPACITY,
                "Floor opacity",
                slider(0.0, 1.0, 0.82, 2, ""),
            )
            .help("How solid the floor raster is drawn.")
            .group("Ground plane"),
            SettingSpec::new(
                k::RAMP_LOW_DBZ,
                "Opacity ramp: lift-off",
                slider(-30.0, 40.0, 5.0, 0, "dBZ"),
            )
            .help(
                "Where the opacity ramp lifts off: about where a reflectivity field \
                 stops being receiver noise and starts being cloud.",
            )
            .group("Opacity ramp"),
            SettingSpec::new(
                k::RAMP_HIGH_DBZ,
                "Opacity ramp: saturation",
                slider(40.0, 80.0, 60.0, 0, "dBZ"),
            )
            .help(
                "Where the ramp saturates: a hail-bearing core, which has to read as a \
                 solid body and not a brighter patch of the same haze.",
            )
            .group("Opacity ramp"),
            SettingSpec::new(
                k::RAMP_GAMMA,
                "Opacity ramp: focus",
                slider(1.0, 6.0, 4.2, 1, ""),
            )
            .help(
                "Exponent between the knees; higher concentrates opacity into the \
                     cores. The default follows Marshall & Palmer 1948 / Atlas 1953 \
                     extinction physics - see the derivation in the 3D module.",
            )
            .group("Opacity ramp"),
            SettingSpec::new(
                k::RAMP_FLOOR,
                "Opacity ramp: haze",
                slider(0.0, 1.0, 0.07, 2, ""),
            )
            .help(
                "Extinction at and below lift-off. Not zero, so a deep body of weak \
                     echo still reads as cloud.",
            )
            .group("Opacity ramp"),
            SettingSpec::new(
                k::RAMP_GAIN,
                "Opacity ramp: body",
                slider(1.0, 12.0, 3.5, 1, ""),
            )
            .help("Extinction at and above saturation.")
            .group("Opacity ramp"),
            SettingSpec::new(k::SHOW_GRID, "Height grid", toggle(true))
                .help("Draw the kilometre height grid on the box walls.")
                .group("Annotations"),
            SettingSpec::new(k::SHOW_BOX, "Box frame", toggle(true))
                .help("Draw the box outline.")
                .group("Annotations"),
            SettingSpec::new(k::SHOW_LABELS, "Axis labels", toggle(true))
                .help("Draw the distance and height labels.")
                .group("Annotations"),
        ],
    )
}

fn analysis_category() -> SettingsCategory {
    use keys::analysis as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Analysis",
        vec![
            SettingSpec::new(
                k::VROT_UNITS,
                "Vrot readout units",
                choice(
                    vec![
                        ChoiceOption::new("kt", "Knots first"),
                        ChoiceOption::new("mps", "m/s first"),
                    ],
                    "kt",
                ),
            )
            .help(
                "Which unit leads the Vrot readout. Knots first matches the papers the \
                 warning criteria are written in (Thompson et al. 2017); the other unit \
                 is always printed beside it, because the radar's own gates are m/s.",
            ),
            SettingSpec::new(
                k::VROT_MARKER_SIZE,
                "Vrot marker size",
                slider(4.0, 24.0, 10.0, 0, "pt"),
            )
            .help(
                "Drawn size of the two gate markers of a Vrot measurement. Declared \
                 ahead of the marker overlay itself.",
            )
            .pending_wiring(),
            SettingSpec::new(
                k::STORM_MOTION_DIR,
                "Storm motion: from",
                slider(0.0, 360.0, 240.0, 0, "°"),
            )
            .help(
                "Meteorological direction the storm moves from, used by the \
                 storm-relative velocity products. 240° is the climatological \
                 southwest-flow default.",
            ),
            SettingSpec::new(
                k::STORM_MOTION_SPEED,
                "Storm motion: speed",
                slider(0.0, 50.0, 15.0, 0, "m/s"),
            )
            .help("Storm speed for the storm-relative velocity products."),
        ],
    )
}

fn data_category() -> SettingsCategory {
    use keys::data as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Data",
        vec![
            SettingSpec::new(
                k::STARTUP_SITE,
                "Startup site",
                SettingKind::Text {
                    default: String::new(),
                    placeholder: "KTLX".to_owned(),
                    // 5, not 4: the shipped site catalog (radar-sites.tsv)
                    // carries five-character identifiers - ROCO2, AWPA2,
                    // HWPA2, TLKA2 - beside the four-character WSR-88Ds, and
                    // a 4-character limit would refuse their fifth letter.
                    max_len: 5,
                },
            )
            .help(
                "Go live on this radar at launch. Leave empty to use the last-viewed \
                 site (below), or neither to open on the map.",
            )
            .group("Startup"),
            SettingSpec::new(k::RESUME_LAST_SITE, "Resume last site", toggle(true))
                .help(
                    "When no startup site is set, reopen on the radar that was live when \
                 the application last closed.",
                )
                .group("Startup"),
            SettingSpec::new(
                k::POLL_SECONDS,
                "Live poll interval",
                // The floor is 1 s, not the 0.5 s this row was declared with
                // before it was wired. Nothing ever read the old range, so
                // nothing can have stored a value inside it that this refuses;
                // and a live setting that let one session list a public bucket
                // twice a second is a way for this application to become a
                // nuisance. See `net_tuning::MIN_LIVE_POLL`.
                slider(1.0, 30.0, 1.2, 1, "s"),
            )
            .help(
                "How often the live feed asks the chunks bucket what is new. \
                 1.2 s follows a volume as it arrives - a chunk lands every few \
                 seconds, so this already asks two to four times per chunk. \
                 Raise it on a metered connection. It will not go below 1 s: \
                 the buckets are a public good and nothing faster buys a \
                 picture.",
            )
            .group("Live polling"),
            SettingSpec::new(
                k::FOLLOW_LOW_TILTS_ENABLED,
                "Automatically follow arriving low tilts",
                toggle(false),
            )
            .help(
                "Automatically select newly arriving, usable sweeps at or below the \
                 elevation ceiling, including supplemental low-level scans within the \
                 same volume. Following begins while a sweep is still in progress, with \
                 incoming data revealed radial by radial. Disable this setting to keep \
                 tilt selection under manual control.",
            )
            .group("Live tilt following"),
            SettingSpec::new(
                k::FOLLOW_MAX_ELEVATION_DEG,
                "Maximum followed elevation",
                slider(0.1, 20.0, 1.4, 1, "°"),
            )
            .help(
                "The highest measured elevation a live sweep may have before automatic \
                 following ignores it. For example, 1.4° admits arriving sweeps around \
                 0.5°, 0.9° and 1.3° without following the radar to higher tilts.",
            )
            .group("Live tilt following"),
            SettingSpec::new(
                k::FOLLOW_MIN_SWEEP_INTERVAL_SECONDS,
                "Minimum sweep update interval",
                integer(1, 600, 30, "s"),
            )
            .help(
                "The minimum difference between the measured scan times of consecutive \
                 automatically followed sweeps. This is a display-selection interval, not \
                 the live acquisition poll interval above: the feed continues checking \
                 for new chunks while the selected sweep updates radial by radial.",
            )
            .group("Live tilt following"),
            SettingSpec::new(
                k::HISTORY_MAX_FRAMES,
                "History frame limit (0 = Unlimited)",
                integer(0, 100_000, 0, "frames"),
            )
            .help(
                "For operator-selected local files, 0 is Unlimited and is the default: \
                 every selected or assembled logical volume remains on the timeline. A \
                 live feed can run unattended, so a 0 frame limit uses the live-safe \
                 fallback of 30 frames. A positive limit applies to both local and live \
                 sessions. Every resulting eviction is reported.",
            )
            .group("Timeline retention (Unlimited by default)"),
            SettingSpec::new(
                k::HISTORY_MAX_MB,
                "History RAM limit (0 = Unlimited)",
                integer(0, 1_048_576, 0, "MiB"),
            )
            .help(
                "For operator-selected local files, 0 is Unlimited and is the default. \
                 A live feed can run unattended, so 0 uses the live-safe 1 GiB fallback. \
                 A positive value applies to both local and live sessions and caps the \
                 timeline's conservative estimate of decoded volume allocations; it is \
                 not an input-file-size limit. Every resulting eviction is reported.",
            )
            .group("Timeline retention (Unlimited by default)"),
            // The loop's speed sits beside the loop's depth: the two rows
            // above decide how much there is to play, this decides how fast it
            // plays. The toolbar is the surface this application deliberately
            // keeps quiet, so the settings page is the durable home for this
            // control.
            //
            // Its own heading rather than the history one, and declared here
            // rather than at the end of the page, because sections are runs
            // of consecutive rows: a heading-less row at the end of a page
            // that has headings would float above nothing, which is the shape
            // `a_page_that_groups_anything_groups_everything_exactly_once`
            // refuses.
            SettingSpec::new(
                k::LOOP_FRAME_MS,
                "Loop frame time",
                integer(100, 3_000, 700, "ms"),
            )
            .help(
                "How long the timeline holds each frame while looping. 700 ms is \
                 about a frame and a half a second - fast enough to see a couplet \
                 turn, slow enough to read the hook. A frame that has not finished \
                 rendering is always waited for, so this is a floor rather than a \
                 promise.",
            )
            .group("Playing the loop"),
            SettingSpec::new(
                k::LIVE_CACHE_LIMIT_MB,
                "Live cache on disk",
                integer(256, 16384, 2048, "MiB"),
            )
            .help(
                "Disk ceiling for downloaded Level II volumes. The sweep runs every \
                 five minutes and never touches anything under fifteen minutes old, \
                 so the volume still assembling and any transfer in flight are safe. \
                 Measured growth is about 0.5 GB a day at single-site use.",
            )
            .group("Caches on disk"),
            SettingSpec::new(
                k::TILE_CACHE_LIMIT_MB,
                "Basemap tiles on disk",
                integer(64, 4096, 512, "MiB"),
            )
            .help(
                "Disk ceiling for cached basemap imagery tiles. Declared ahead of a \
                 seam for it: today the ceiling is fixed at 512 MiB.",
            )
            // `basemap_tiles::TileCacheConfig::max_disk_bytes` IS enforced (the
            // 512 MiB default matches this declaration), but the scene builds
            // its tile controller with `TileCacheConfig::default()` and
            // `map_scene::MapScene` exposes no constructor or setter that
            // accepts a config - there is nothing `app.rs` can push this value
            // through. Flip to enabled in the same change that gives the scene
            // a tile-config seam.
            .pending_wiring()
            .group("Caches on disk"),
            // Not a preference in the ordinary sense: the file browser
            // writes this every time it reads a folder, so what is stored is
            // wherever the last session was looking. It is on the page
            // anyway - and last, where a memory belongs rather than a knob -
            // because an analyst who keeps one archive can type it once and
            // stop navigating, and because a setting nobody can see is a
            // setting nobody can clear.
            //
            // `max_len` is generous on purpose. Windows extended paths reach
            // 32 767 characters and a deep archive tree on a mapped share
            // gets long; the truncation in `SettingKind::sanitize` cuts on a
            // character boundary, so an over-long stored path degrades to a
            // shorter path (which simply fails to read, and says so) rather
            // than to a panic.
            SettingSpec::new(
                k::OPEN_FOLDER,
                "Open folder",
                SettingKind::Text {
                    default: String::new(),
                    placeholder: "D:/radar/archive".to_owned(),
                    max_len: 4096,
                },
            )
            .help(
                "Where File > Open… starts looking. It follows the browser: walk into another \
                 folder and this becomes that folder, so the next session opens where the last \
                 one left off. A folder that could not be read is never stored. Empty means \
                 the folder the application was started from.",
            )
            .group("Opening files"),
        ],
    )
}

/// Units and time.
///
/// The single most-requested axis in radar software, and the one this
/// application had no answer to: every distance was kilometres, every height
/// was kilometres, every time was UTC, because that is what the decoder hands
/// over and nothing converted it on the way to the glass.
///
/// Every default here is the unit the application already wrote, so a fresh
/// settings file changes not one character. The conversion is DISPLAY only -
/// the stored volume, the sampled gate and the camera all stay in the units
/// they were decoded in - which is what lets an analyst switch to statute
/// miles, read a range, switch back, and be exactly where they started. The
/// numbers themselves are mirrored by hand from `crate::units`, which lives
/// behind the binary crate; a test there pins each one against its enum.
fn units_category() -> SettingsCategory {
    use keys::units as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Units & time",
        vec![
            SettingSpec::new(
                k::DISTANCE,
                "Distance",
                choice(
                    vec![
                        ChoiceOption::new("km", "Kilometres (km)"),
                        ChoiceOption::new("mi", "Statute miles (mi)"),
                        ChoiceOption::new("nm", "Nautical miles (nm)"),
                    ],
                    "km",
                ),
            )
            .help(
                "The unit every ground distance is written in: the range in both \
                 corner readouts, the probe and cross-section readouts, the \
                 cross-section's distance axis, the Vrot and site-pick reports, and \
                 the range-ring labels. The numbered ring spacings move with it - \
                 'every 50' in miles is a ring every 50 miles - except the standard \
                 ladder, which keeps its kilometre spacing and is relabelled in your \
                 unit.",
            ),
            SettingSpec::new(
                k::ALTITUDE,
                "Altitude",
                choice(
                    vec![
                        ChoiceOption::new("km", "Kilometres (km)"),
                        ChoiceOption::new("ft", "Feet (ft)"),
                        ChoiceOption::new("m", "Metres (m)"),
                    ],
                    "km",
                ),
            )
            .help(
                "The unit beam heights are written in - the probe readout's ARL and \
                 MSL figures, the cross-section's readout and its height axis, and \
                 the couplet height in a Vrot report. Feet and metres are rounded to \
                 whole units: a beam at 0.73 km is 2392 ft, and a hundredth of a foot \
                 would claim precision no beam has.",
            ),
            SettingSpec::new(
                k::TIME_ZONE,
                "Times shown in",
                choice(
                    vec![
                        ChoiceOption::new("utc", "UTC"),
                        ChoiceOption::new("local", "This machine's local time"),
                    ],
                    "utc",
                ),
            )
            .help(
                "Which clock volume times are written against. UTC is what the \
                 volume header holds and what every product discussion is written \
                 in. Local time always carries its offset, so a screenshot can \
                 never be read against the wrong clock.",
            ),
            SettingSpec::new(
                k::CLOCK,
                "Clock",
                choice(
                    vec![
                        ChoiceOption::new("24h", "24-hour"),
                        ChoiceOption::new("12h", "12-hour (AM/PM)"),
                    ],
                    "24h",
                ),
            )
            .help(
                "Whether an hour is written 00-23 or 12-hour with AM/PM. Seconds \
                 are always shown either way: a volume time is an instant a radar \
                 recorded, not a wall clock.",
            ),
        ],
    )
}

/// Data and network.
///
/// How hard this session works the two public NEXRAD buckets. Every one of
/// these was a constant argued for at its declaration - most of them against a
/// measured site - and every default here is that constant, so a fresh
/// settings file makes exactly the requests the application always made.
///
/// Each range carries a floor that is not advisory: the buckets are a public
/// good paid for by somebody else, the fence is enforced again in
/// `crate::net_tuning::NetTuning::clamped` and `data_source::tuning`, and the
/// help text on every row names the floor rather than hiding it.
fn network_category() -> SettingsCategory {
    use keys::network as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Data & network",
        vec![
            SettingSpec::new(
                k::ARCHIVE_POLL_SECONDS,
                "Archive fallback poll",
                integer(15, 600, 30, "s"),
            )
            .help(
                "How often a session that has lost its chunk feed asks the Level II \
                 archive bucket what it is holding. A healthy site never asks at \
                 all. The archive receives one finished object per volume - about \
                 one every four minutes - so 30 s is already twenty questions per \
                 answer, and 15 s is the floor.",
            )
            .group("When the chunk feed dies"),
            SettingSpec::new(
                k::ARCHIVE_LEAD_MINUTES,
                "Archive must lead by",
                integer(1, 60, 5, "min"),
            )
            .help(
                "How far ahead of the dead chunk feed the archive has to be before \
                 this session switches to it. The guard is against a radar that is \
                 genuinely off the air: then both feeds stop together, the archive \
                 has nothing better, and switching would only add a second source \
                 to explain. Five minutes is about one volume.",
            )
            .group("When the chunk feed dies"),
            SettingSpec::new(
                k::STALL_AFTER_SECONDS,
                "Listing failures count as a stall after",
                integer(15, 900, 60, "s"),
            )
            .help(
                "How long the chunk listing has to keep failing before the failure \
                 is treated as a dead feed rather than a dropped connection. A \
                 503, a lost link and a closed laptop lid all clear on the next \
                 poll; only a failure that persists says anything about the radar.",
            )
            .group("When the chunk feed dies"),
            SettingSpec::new(
                k::DOWNLOAD_BATCH,
                "Chunks downloaded at once",
                integer(1, 16, 8, ""),
            )
            .help(
                "How many chunk objects are fetched in parallel. One at a time is \
                 the honest setting on a metered or shared link. Sixteen is the \
                 ceiling: more parallel requests against one bucket prefix is a \
                 way to get throttled, not a way to go faster.",
            )
            .group("Fetching an object"),
            SettingSpec::new(
                k::DOWNLOAD_ATTEMPTS,
                "Attempts per object",
                integer(1, 6, 3, ""),
            )
            .help(
                "Total tries for one object, the first included, when the failure \
                 is the retriable kind - a dropped connection, a truncated body, a \
                 5xx. A 404 or a full disk is never retried whatever this says.",
            )
            .group("Fetching an object"),
            SettingSpec::new(
                k::RETRY_BACKOFF_MS,
                "Pause before a retry",
                integer(100, 5_000, 150, "ms"),
            )
            .help(
                "How long to wait between attempts at the same object. The 100 ms \
                 floor is what keeps a failing object from becoming a tight retry \
                 loop against a public bucket.",
            )
            .group("Fetching an object"),
        ],
    )
}

/// Readout and annotation.
///
/// What the pane writes on top of the radar. These were eight hard-coded
/// numbers in `pane_canvas.rs` - a ring ladder, a marker size, a label size, a
/// declutter count, a marker ceiling, and the decimal places on a range and a
/// latitude - each of them defensible and none of them a law.
///
/// Every default is the number the pane already used, so a fresh settings file
/// paints the pane it always painted. The values are mirrored by hand from
/// `crate::annotation`, which lives behind the binary crate; a test there pins
/// each one against its field.
fn annotation_category() -> SettingsCategory {
    use keys::annotation as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Readout & annotation",
        vec![
            SettingSpec::new(
                k::CORNER_READOUT,
                "Pane corner readout",
                choice(
                    vec![
                        ChoiceOption::new(
                            "range-azimuth-coords",
                            "Range, azimuth, latitude/longitude",
                        ),
                        ChoiceOption::new("range-azimuth", "Range and azimuth"),
                        ChoiceOption::new("coords", "Latitude/longitude"),
                        ChoiceOption::new("off", "Nothing"),
                    ],
                    "range-azimuth-coords",
                ),
            )
            .help(
                "What the bottom-left corner writes while the pointer is over a \
                 pane. This is the geographic line only - the value under the \
                 cursor is drawn above it and stays either way.",
            )
            .group("The corner readout"),
            SettingSpec::new(k::RANGE_DECIMALS, "Range decimals", integer(0, 3, 1, ""))
                .help(
                    "Decimal places on a distance in the corner and probe readouts. \
                     One place is a tenth of a kilometre, which is under half a gate.",
                )
                .group("The corner readout"),
            SettingSpec::new(
                k::COORDINATE_DECIMALS,
                "Latitude/longitude decimals",
                integer(2, 6, 4, ""),
            )
            .help(
                "Decimal places on a latitude or longitude. Four places is about \
                 11 m of latitude - finer than any radar gate, and the shipped \
                 choice because it is what a position gets read aloud in.",
            )
            .group("The corner readout"),
            SettingSpec::new(
                k::RING_LADDER,
                "Range rings",
                choice(
                    vec![
                        ChoiceOption::new("shipped", "Standard (50, 100, 150, 200, 300, 400 km)"),
                        ChoiceOption::new("every-25", "Every 25"),
                        ChoiceOption::new("every-50", "Every 50"),
                        ChoiceOption::new("every-100", "Every 100"),
                        ChoiceOption::new("every-200", "Every 200"),
                    ],
                    "shipped",
                ),
            )
            .help(
                "Which distance rings are drawn about the radar. The standard \
                 ladder steps by 50 km out to 200 and by 100 to 400 - close in is \
                 where a distance gets read, far out only has to say 'a long way' - \
                 and keeps that kilometre spacing whatever the distance unit is, so \
                 switching to miles does not move a ring. The even spacings do move: \
                 they step in the unit chosen under Units & time. Either way the \
                 labels are written in the chosen distance unit.",
            )
            // Not "Range rings": one row already carries that name, and this
            // page has been through one round of two things sharing it.
            .group("Rings about the radar"),
            SettingSpec::new(k::RING_COUNT, "Rings drawn", integer(0, 12, 6, ""))
                .help(
                    "How many rings of the chosen ladder to draw, counting outwards. \
                 The standard ladder has six and stops there; an even spacing draws \
                 exactly this many. Zero draws none - the origin dot stays.",
                )
                .group("Rings about the radar"),
            SettingSpec::new(k::RING_LABELS, "Label the rings", toggle(false))
                .help(
                    "Write each ring's distance where it crosses due north, in the \
                 chosen distance unit. Off is the shipped pane, which has never \
                 written a number on a ring.",
                )
                .group("Rings about the radar"),
            SettingSpec::new(
                k::SITE_MARKER_SIZE,
                "Site marker size",
                slider(4.0, 24.0, 10.0, 0, "pt"),
            )
            .help(
                "Drawn size of a radar site's clickable box. The click target \
                 follows it, with four points of slack around the edge.",
            )
            .group("Site markers and their labels"),
            SettingSpec::new(
                k::SITE_LABEL_SIZE,
                "Site label size",
                slider(7.0, 20.0, 11.0, 0, "pt"),
            )
            .help("Point size of the identifier written beside a site marker.")
            .group("Site markers and their labels"),
            SettingSpec::new(
                k::SITE_DECLUTTER_MAX,
                "Label every site up to",
                integer(0, 400, 40, "sites"),
            )
            .help(
                "How many sites may be on screen before the automatic label rule \
                 stops writing all of them. Above this only the active and hovered \
                 sites are named, which is what keeps a continental view from \
                 disappearing under two hundred identifiers. Only applies when Map \
                 > Site labels is 'When uncluttered'.",
            )
            .group("Site markers and their labels"),
            SettingSpec::new(
                k::SITE_MARKER_MAX,
                "Most markers per pane",
                integer(10, 1_000, 250, "sites"),
            )
            .help(
                "Ceiling on markers drawn in one pane. When more sites than this are \
                 in view, the ones nearest the middle of the pane are the ones kept - \
                 so lowering it thins a continental view towards what is under the \
                 pointer rather than dropping sites at random.",
            )
            .group("Site markers and their labels"),
        ],
    )
}

/// The cross-section window.
///
/// A page of its own rather than a row bolted onto Radar, because the slice
/// is a second picture with its own geometry and this is where the rest of
/// its depth will land. Today it carries one row - and one honest row on its
/// own page beats the same row filed somewhere it does not belong.
fn xsection_category() -> SettingsCategory {
    use keys::xsection as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Cross-section",
        vec![
            SettingSpec::new(k::TOP_KM, "Top of the slice", integer(4, 24, 18, "km")).help(
                "How high the cross-section window is drawn, above the radar. 18 km \
                 clears any storm the radar can see over; drop it to 12 for a warm- \
                 season line and the same picture is drawn at half again the vertical \
                 detail. Stated in kilometres whatever Units & time says - it is a \
                 fixed height, and a stored number that changed meaning when the unit \
                 changed would silently redraw the slice. The height axis is labelled \
                 in the chosen unit.",
            ),
        ],
    )
}

/// The Level 1 (time series / I/Q) page.
///
/// The one page in this window whose knobs are part of the measurement rather
/// than part of its presentation, and the page is written that way: every help
/// line says what the choice DOES to the numbers, not what it looks like.
///
/// The page is offered whether or not a time-series file is open, like every
/// other page here - a settings window whose contents changed depending on what
/// was loaded would be a window an analyst could not learn.
fn timeseries_category() -> SettingsCategory {
    use keys::timeseries as k;
    use timeseries_limits as limit;
    let windows = vec![
        ChoiceOption::new(limit::WINDOW_RECTANGULAR, "Rectangular (none)").describe(
            "No window. The narrowest main lobe, so the least width bias and the most \
             independent samples - and -13 dB sidelobes, which strong ground clutter \
             smears across the whole spectrum. This is the estimator the published \
             pulse-pair formulas describe.",
        ),
        ChoiceOption::new(limit::WINDOW_VON_HANN, "Von Hann").describe(
            "-31 dB sidelobes falling 18 dB per octave. The usual choice for reading a \
             Doppler spectrum, and what to reach for first when clutter is in the way.",
        ),
        ChoiceOption::new(limit::WINDOW_HAMMING, "Hamming").describe(
            "-43 dB first sidelobe but only 6 dB per octave beyond it: better than von \
             Hann close in to the signal, worse far out from it.",
        ),
        ChoiceOption::new(limit::WINDOW_BLACKMAN, "Blackman").describe(
            "-58 dB sidelobes, at the cost of the widest main lobe. For weather beside \
             clutter 50 dB stronger than it.",
        ),
    ];
    let channels = vec![
        ChoiceOption::new(limit::CHANNEL_HORIZONTAL, "Horizontal"),
        ChoiceOption::new(limit::CHANNEL_VERTICAL, "Vertical")
            .describe("Single-polarisation records have no vertical channel to show."),
    ];
    SettingsCategory::new(
        k::CATEGORY,
        "Level 1 (I/Q)",
        vec![
            SettingSpec::new(
                k::DWELL_PULSES,
                "Preferred pulses per dwell",
                integer(
                    limit::MIN_DWELL,
                    limit::MAX_DWELL,
                    limit::DEFAULT_DWELL,
                    "pulses",
                ),
            )
            .help(
                "For a continuous pulse stream, how many transmitted pulses are averaged \
                 into one radial. This is the trade the radar made once, at scan time, \
                 and never wrote down. A long dwell averages more pulses, so the moments \
                 are steadier and the spectrum is finer in velocity, over a wider smear \
                 of azimuth; a short one resolves the storm's own changes and gives more \
                 radials, from noisier estimates. Dwells do not overlap, so the radial \
                 count is the pulse count divided by this. A source that preserves \
                 measured ray boundaries ignores this preference and keeps one native \
                 ray per dwell; the pane header names the dwell actually in use.",
            ),
            SettingSpec::new(
                k::WINDOW,
                "Window",
                choice(windows, limit::WINDOW_RECTANGULAR),
            )
            .help(
                "The taper applied across each dwell before the transform. It decides how \
                 far a strong echo leaks into the velocity bins around it, which is what \
                 makes weak weather next to ground clutter readable or not. Every window \
                 trades sidelobe suppression for a wider main lobe, so a spectrum width \
                 read through one is broadened by it.",
            ),
            SettingSpec::new(
                k::SNR_MIN_DB,
                "Hide gates below",
                SettingKind::Slider {
                    min: limit::OFF_SNR_DB,
                    max: limit::MAX_SNR_DB,
                    default: limit::DEFAULT_SNR_DB,
                    decimals: 1,
                    unit: "dB SNR".to_owned(),
                    // Leftmost means no threshold at all, and a stored number
                    // this build cannot read falls back to the operational 2 dB
                    // rather than to either end. See `settings::SliderFloor`.
                    floor: SliderFloor::Off,
                },
            )
            .help(
                "Signal-to-noise floor. Gates below it are left blank instead of being \
                 drawn from noise. 2 dB is the operational WSR-88D threshold, so a file \
                 opened without touching this shows the same population of gates the \
                 Level II product of the same scan would - which is what makes the two \
                 comparable. All the way left applies no threshold, which is the only way \
                 to see what the operational one was throwing away. Gates with no power \
                 above the receiver noise stay blank either way: that is a measurement, \
                 not a threshold. A source with no measured receiver-noise reference \
                 cannot produce a signal-to-noise ratio and ignores this preference; the \
                 pane header states that receiver noise is unavailable.",
            ),
            SettingSpec::new(
                k::SPECTRUM_CHANNEL,
                "Spectrum channel",
                choice(channels, limit::CHANNEL_HORIZONTAL),
            )
            .help(
                "Which receiver channel the spectrum readout under the cursor is taken \
                 from. The moments use both channels whatever this says.",
            ),
        ],
    )
}

/// The Profiles page.
///
/// Almost all of this page is not a knob and is not declared here: the list of
/// profiles, the save/switch/rename controls and the unsaved-changes question
/// are drawn by `settings_ui::profiles`, because a named snapshot of every
/// other page is not a value with a range and a default. What IS declared is
/// the page itself - so it appears in the category list and in the search
/// results like every other page - and the one genuine preference about it.
///
/// The active profile's name is stored under this category too, as
/// `settings::profiles::ACTIVE_SETTING`, and is deliberately NOT declared: it
/// is a pointer the application maintains, not something to type into a text
/// box, and a declared row would offer exactly that.
fn profiles_category() -> SettingsCategory {
    use keys::profiles as k;
    SettingsCategory::new(
        k::CATEGORY,
        "Profiles",
        vec![
            SettingSpec::new(k::SHOW_IN_STATUS, "Name the active profile", toggle(true)).help(
                "Show the active profile's name in the File menu. The main window stays \
                     otherwise unchanged - profiles are managed here.",
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::SettingValue;

    /// One row in the whole window is named "Range rings", and it is the live
    /// one.
    ///
    /// The defect this closes: `map/range_rings` was declared for a range-ring
    /// layer that had in fact always been drawn, and it remained after the real
    /// ladder became `annotation/ring_ladder`. A search for "range rings"
    /// returned two rows - a greyed Map one reading
    /// "Off" above a live Readout & annotation one - and the dead one came
    /// first, so the honest reading of the window was that the control did
    /// nothing.
    #[test]
    fn only_one_row_in_the_whole_window_is_named_range_rings() {
        let registry = registry();
        let named: Vec<(String, String)> = registry
            .categories()
            .iter()
            .flat_map(|category| {
                category
                    .settings
                    .iter()
                    .filter(|setting| setting.label == "Range rings")
                    .map(|setting| (category.id.clone(), setting.id.clone()))
            })
            .collect();
        assert_eq!(
            named,
            vec![(
                keys::annotation::CATEGORY.to_owned(),
                keys::annotation::RING_LADDER.to_owned()
            )],
            "the live ring ladder must be the only row with that name"
        );
        // And the retired id is declared nowhere at all, in any category, so
        // an old settings file's value is simply orphaned.
        for category in registry.categories() {
            assert!(
                !category
                    .settings
                    .iter()
                    .any(|setting| setting.id == keys::map::RETIRED_RANGE_RINGS),
                "{} still declares the retired id",
                category.id
            );
        }
    }

    /// The three rows that talk about range rings and units have to tell one
    /// story, because an analyst reads whichever one they land on first.
    ///
    /// The defect this closes: the ladder row said the standard ladder "keeps
    /// those kilometre numbers whatever the distance unit is" while the
    /// labels row said labels are written "in the chosen distance unit". Both
    /// were describing the same pane. What is actually true is narrower than
    /// either sentence - the *spacing* stays metric, the *label* does not -
    /// and `chrome_tests::the_distance_unit_relabels_the_rings_without_
    /// moving_them` is the behaviour these words now have to match.
    #[test]
    fn the_ring_rows_and_the_units_row_tell_the_same_story_about_labels() {
        let registry = registry();
        let help = |category: &str, id: &str| -> String {
            registry
                .setting(category, id)
                .unwrap_or_else(|| panic!("{category}/{id} is declared"))
                .help
                .clone()
        };
        let ladder = help(keys::annotation::CATEGORY, keys::annotation::RING_LADDER);
        let labels = help(keys::annotation::CATEGORY, keys::annotation::RING_LABELS);
        let distance = help(keys::units::CATEGORY, keys::units::DISTANCE);

        // What stays metric on the standard ladder is the spacing. Saying
        // "numbers" was the contradiction: the numbers are exactly the part
        // that follows the unit.
        assert!(
            ladder.contains("kilometre spacing"),
            "the ladder row must name the spacing as the metric part: {ladder}"
        );
        assert!(
            !ladder.contains("kilometre numbers"),
            "the ladder row is claiming the labels stay metric again: {ladder}"
        );
        // And all three agree that a label follows the chosen unit.
        for (which, text) in [
            ("the ladder row", &ladder),
            ("the labels row", &labels),
            ("the distance row", &distance),
        ] {
            assert!(
                text.contains("chosen distance unit") || text.contains("ring labels"),
                "{which} says nothing about what unit a ring label is written \
                 in, so a reader has to guess: {text}"
            );
        }
    }

    /// The window's voice is plain sentences. A word in capitals reads as
    /// shouting on the glass, and three of them shipped into the settings
    /// window in this wave: "the cross-section's readout AND its height
    /// axis", "keeps that kilometre SPACING", "The height AXIS is labelled".
    ///
    /// A fourth predates the wave - "the storm moves FROM" - and is fixed
    /// with them rather than added to the list below, because an invariant
    /// that has to whitelist the very thing it forbids is not one.
    ///
    /// Acronyms are not shouting, so they are listed rather than guessed at -
    /// a new one is a one-line change here and a deliberate decision, which
    /// is the point.
    #[test]
    fn no_help_text_shouts_a_word_in_capitals() {
        // Acronyms, unit symbols and the Roman numeral in "Level II".
        // "RF" arrived with the gate filter: it is the name a colour table
        // paints on a range-folded gate, so the help text that offers to hide
        // those gates has to be able to say which colour it means.
        // "WSR" arrived with the Level 1 page: the threshold that page
        // defaults to is the operational WSR-88D one, and naming the radar it
        // came from is what makes the number checkable rather than arbitrary.
        const ACRONYMS: [&str; 14] = [
            "UTC", "AM", "PM", "MSL", "ARL", "USGS", "GPU", "MiB", "GB", "VCP", "NEXRAD", "II",
            "RF", "WSR",
        ];
        for category in registry().categories() {
            for setting in &category.settings {
                for word in setting.help.split(|c: char| !c.is_ascii_alphabetic()) {
                    if word.len() < 2 || ACRONYMS.contains(&word) {
                        continue;
                    }
                    assert!(
                        !word.chars().all(|c| c.is_ascii_uppercase()),
                        "{}/{} shouts {word:?}: {}",
                        category.id,
                        setting.id,
                        setting.help
                    );
                }
            }
        }
    }

    #[test]
    fn every_setting_id_is_unique_within_its_category() {
        for category in registry().categories() {
            let mut seen = std::collections::BTreeSet::new();
            for setting in &category.settings {
                assert!(
                    seen.insert(setting.id.clone()),
                    "duplicate id {:?} in category {:?}",
                    setting.id,
                    category.id
                );
            }
        }
    }

    #[test]
    fn every_declared_default_survives_its_own_sanitizer() {
        // If a default fell outside its own declared range, a freshly reset
        // setting would immediately read back as something else.
        for category in registry().categories() {
            for setting in &category.settings {
                let default = setting.kind.default_value();
                assert_eq!(
                    setting.kind.sanitize(Some(&default)),
                    default,
                    "{}/{}",
                    category.id,
                    setting.id
                );
            }
        }
    }

    #[test]
    fn live_tilt_following_is_opt_in_and_distinct_from_acquisition_polling() {
        let registry = registry();
        let category = registry
            .category(keys::data::CATEGORY)
            .expect("the Data settings page is declared");
        let section = category
            .sections()
            .into_iter()
            .find(|section| section.heading == "Live tilt following")
            .expect("live tilt following has its own settings section");
        let ids: Vec<&str> = section
            .settings
            .iter()
            .map(|setting| setting.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                keys::data::FOLLOW_LOW_TILTS_ENABLED,
                keys::data::FOLLOW_MAX_ELEVATION_DEG,
                keys::data::FOLLOW_MIN_SWEEP_INTERVAL_SECONDS,
            ],
            "display-selection controls must not be mixed into network polling"
        );

        let enable = registry
            .setting(keys::data::CATEGORY, keys::data::FOLLOW_LOW_TILTS_ENABLED)
            .expect("the opt-in toggle is declared");
        assert_eq!(enable.kind.default_value(), SettingValue::Bool(false));
        assert!(
            enable.help.contains("still in progress") && enable.help.contains("radial by radial"),
            "automatic following must describe in-progress sweeps and incoming radials"
        );

        let animation = registry
            .setting(keys::radar::CATEGORY, keys::radar::SWEEP_ANIMATION)
            .expect("the existing live sweep animation is declared");
        assert_eq!(
            animation.kind.default_value(),
            SettingValue::Bool(true),
            "automatic following should reveal incoming sweeps without another toggle"
        );

        let elevation = registry
            .setting(keys::data::CATEGORY, keys::data::FOLLOW_MAX_ELEVATION_DEG)
            .expect("the elevation ceiling is declared");
        let SettingKind::Slider {
            min,
            max,
            default,
            decimals,
            unit,
            ..
        } = &elevation.kind
        else {
            panic!("the elevation ceiling must be a degree slider");
        };
        assert_eq!((*min, *max, *default, *decimals), (0.1, 20.0, 1.4, 1));
        assert_eq!(unit, "°");

        let interval = registry
            .setting(
                keys::data::CATEGORY,
                keys::data::FOLLOW_MIN_SWEEP_INTERVAL_SECONDS,
            )
            .expect("the minimum followed-sweep interval is declared");
        let SettingKind::Integer {
            min,
            max,
            default,
            unit,
        } = &interval.kind
        else {
            panic!("the minimum followed-sweep interval must use whole seconds");
        };
        assert_eq!((*min, *max, *default), (1, 600, 30));
        assert_eq!(unit, "s");
        assert!(
            interval
                .help
                .contains("not the live acquisition poll interval"),
            "the display interval must not be mistaken for the network poll cadence"
        );

        let acquisition = registry
            .setting(keys::data::CATEGORY, keys::data::POLL_SECONDS)
            .expect("the pre-existing acquisition poll remains declared");
        assert_eq!(acquisition.group, "Live polling");
        assert_eq!(acquisition.kind.default_value(), SettingValue::Float(1.2));
    }

    /// The pages that had stopped being readable as a list carry headings.
    /// The short ones deliberately do not: a heading over three toggles is
    /// noise, and a page that declares none renders exactly as it always has.
    ///
    /// Named rather than derived from a length threshold, because "long
    /// enough to need structure" is a judgement about what the settings are,
    /// not an arithmetic fact about how many of them there are.
    #[test]
    fn the_pages_that_had_grown_into_a_wall_carry_headings() {
        let registry = registry();
        // Readout & annotation has ten rows and Data & network has six; both
        // need the same grouping already used by the other long pages. Without
        // it, the settings appear as the wall of controls this machinery exists
        // to prevent.
        //
        // The minimum is per page rather than one number for all of them:
        // the six rows of Data & network divide honestly into two, and
        // demanding a third would be asking for a heading invented to satisfy
        // a test. What every one of them has to clear is two - one section is
        // the wall with a heading on top of it.
        for (id, minimum) in [
            (keys::vol3d::CATEGORY, 3),
            (keys::data::CATEGORY, 3),
            (keys::annotation::CATEGORY, 3),
            (keys::network::CATEGORY, 2),
        ] {
            let category = registry.category(id).expect("declared page");
            assert!(
                category.has_sections(),
                "{id} is grouped on purpose; a build where the headings vanished \
                 would silently be the wall again"
            );
            assert!(
                category.sections().len() >= minimum,
                "{id} groups into {} section(s), which is not structure",
                category.sections().len()
            );
        }
    }

    /// Whichever pages are grouped, the grouping has to be well formed: no
    /// settings floating above the first heading with nothing naming them,
    /// and no heading drawn twice because its items were declared in two
    /// separate runs.
    #[test]
    fn a_page_that_groups_anything_groups_everything_exactly_once() {
        for category in registry().categories() {
            if !category.has_sections() {
                continue;
            }
            let mut seen = std::collections::BTreeSet::new();
            for section in category.sections() {
                assert!(
                    !section.heading.is_empty(),
                    "{}: {} setting(s) sit above the first heading with nothing \
                     naming them",
                    category.id,
                    section.settings.len(),
                );
                assert!(
                    seen.insert(section.heading.to_owned()),
                    "{}: the heading {:?} appears in two separate runs",
                    category.id,
                    section.heading,
                );
            }
        }
    }

    #[test]
    fn every_setting_has_help_text_because_hover_does_not_exist_on_glass() {
        for category in registry().categories() {
            for setting in &category.settings {
                assert!(
                    !setting.help.is_empty(),
                    "{}/{} has no help",
                    category.id,
                    setting.id
                );
            }
        }
    }

    #[test]
    fn the_basemap_default_is_the_shipped_slate_look() {
        let registry = registry();
        let store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-proof-never-written.json"),
        );
        assert_eq!(
            store.effective_text(&registry, keys::map::CATEGORY, keys::map::BASEMAP_STYLE),
            map_scene::MapStylePreset::default().id()
        );
        assert_eq!(
            store.effective_text(&registry, keys::map::CATEGORY, keys::map::IMAGERY_PROVIDER),
            "none",
            "no imagery is the shipped behaviour - an offline machine is never worse off"
        );
    }

    #[test]
    fn the_quality_default_matches_the_renderers_shipped_default() {
        // "smooth" must be the id of render2d's Default, or a fresh settings
        // file would change the picture.
        assert_eq!(
            render2d::DisplayQuality::default(),
            render2d::DisplayQuality::SMOOTH
        );
        let registry = registry();
        let store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-proof-never-written.json"),
        );
        assert_eq!(
            store.effective_text(&registry, keys::radar::CATEGORY, keys::radar::QUALITY),
            "smooth"
        );
    }

    #[test]
    fn timeline_retention_is_unlimited_by_default_and_preserves_explicit_old_limits() {
        let registry = registry();
        let mut store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-history-never-written.json"),
        );
        assert_eq!(
            store.effective_int(
                &registry,
                keys::data::CATEGORY,
                keys::data::HISTORY_MAX_FRAMES
            ),
            0
        );
        assert_eq!(
            store.effective_int(&registry, keys::data::CATEGORY, keys::data::HISTORY_MAX_MB),
            0
        );

        // The ids did not change. A constrained-system profile from an older
        // build therefore keeps the positive limits its operator chose while
        // an absent value adopts the new Unlimited default.
        store.set(
            keys::data::CATEGORY,
            keys::data::HISTORY_MAX_FRAMES,
            SettingValue::Int(45),
        );
        store.set(
            keys::data::CATEGORY,
            keys::data::HISTORY_MAX_MB,
            SettingValue::Int(1_536),
        );
        assert_eq!(
            store.effective_int(
                &registry,
                keys::data::CATEGORY,
                keys::data::HISTORY_MAX_FRAMES
            ),
            45
        );
        assert_eq!(
            store.effective_int(&registry, keys::data::CATEGORY, keys::data::HISTORY_MAX_MB),
            1_536
        );
    }

    #[test]
    fn the_navigation_defaults_are_the_tuned_constants_from_analyst_runtime() {
        let registry = registry();
        let store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-proof-never-written.json"),
        );
        let f = |id| store.effective_float(&registry, keys::navigation::CATEGORY, id);
        assert_eq!(
            f(keys::navigation::ZOOM_PER_NOTCH) as f32,
            analyst_runtime::ZOOM_PER_NOTCH
        );
        assert_eq!(
            f(keys::navigation::BURST_GAIN_CAP) as f32,
            analyst_runtime::MAX_BURST_GAIN
        );
        assert_eq!(
            f(keys::navigation::KEY_PAN_RATE) as f32,
            analyst_runtime::KEY_PAN_FRACTION_PER_SECOND
        );
        assert_eq!(
            f(keys::navigation::KEY_ZOOM_RATE) as f32,
            analyst_runtime::KEY_ZOOM_RATE_PER_SECOND
        );
    }

    #[test]
    fn the_storm_motion_defaults_match_the_runtime_intent_default() {
        let default = analyst_runtime::StormMotionIntent::default();
        let registry = registry();
        let store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-proof-never-written.json"),
        );
        assert_eq!(
            store.effective_float(
                &registry,
                keys::analysis::CATEGORY,
                keys::analysis::STORM_MOTION_DIR
            ) as f32,
            default.direction_from_deg
        );
        assert_eq!(
            store.effective_float(
                &registry,
                keys::analysis::CATEGORY,
                keys::analysis::STORM_MOTION_SPEED
            ) as f32,
            default.speed_mps
        );
    }

    /// A fresh settings file must censor nothing. This is the declaration
    /// half of that promise - `gate_filter_ui` pins the other half, that these
    /// numbers resolve to `render2d::GateFilter::OFF`.
    #[test]
    fn every_gate_filter_criterion_is_off_in_a_fresh_file() {
        let registry = registry();
        let store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-proof-never-written.json"),
        );
        let f = |id| store.effective_float(&registry, keys::radar::CATEGORY, id);
        assert_eq!(f(keys::radar::FILTER_MIN_DBZ), radar_filter::OFF_MIN_DBZ);
        assert_eq!(
            f(keys::radar::FILTER_VEL_NEEDS_DBZ),
            radar_filter::OFF_MIN_DBZ
        );
        assert_eq!(f(keys::radar::FILTER_MIN_RHO), radar_filter::OFF_MIN_RHO);
        assert_eq!(
            f(keys::radar::FILTER_MIN_RANGE_KM),
            radar_filter::OFF_MIN_RANGE_KM
        );
        assert!(!store.effective_bool(
            &registry,
            keys::radar::CATEGORY,
            keys::radar::FILTER_HIDE_RF
        ));
    }

    /// Each "off" value is also the bottom of its own slider, so all-the-way
    /// left is off and there is no second enable control to find.
    #[test]
    fn every_gate_filter_off_value_is_the_bottom_of_its_own_range() {
        let registry = registry();
        for id in [
            keys::radar::FILTER_MIN_DBZ,
            keys::radar::FILTER_VEL_NEEDS_DBZ,
            keys::radar::FILTER_MIN_RHO,
            keys::radar::FILTER_MIN_RANGE_KM,
        ] {
            let spec = registry
                .setting(keys::radar::CATEGORY, id)
                .unwrap_or_else(|| panic!("{id} is declared"));
            let SettingKind::Slider {
                min,
                default,
                floor,
                ..
            } = &spec.kind
            else {
                panic!("{id} should be a slider");
            };
            assert_eq!(min, default, "{id}: off is not the left end of the travel");
            // And the range says so, which is what makes the window print
            // "off" at that stop and what keeps a stranger value from
            // resolving to the far end. See `settings::SliderFloor`.
            assert_eq!(
                *floor,
                SliderFloor::Off,
                "{id} is declared as an ordinary slider, so a stored value this build \
                 cannot account for would clamp to the strongest censor it offers"
            );
        }
    }

    /// A stored threshold this build cannot account for turns nothing on.
    ///
    /// Driven through the real catalog and the real store, because the
    /// declaration is only half of it: the other half is that
    /// `SettingsStore::effective_float` reads the declaration. 900 is what a
    /// hand edit or a build with a wider range would leave behind; before the
    /// floor was declared it resolved to 40 dBZ and 40 km - an active censor
    /// on a scene nobody asked to have censored.
    #[test]
    fn a_stranger_threshold_resolves_to_off_rather_than_to_the_strongest_censor() {
        let registry = registry();
        let mut store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-stranger-never-written.json"),
        );
        for (id, off) in [
            (keys::radar::FILTER_MIN_DBZ, radar_filter::OFF_MIN_DBZ),
            (keys::radar::FILTER_VEL_NEEDS_DBZ, radar_filter::OFF_MIN_DBZ),
            (keys::radar::FILTER_MIN_RHO, radar_filter::OFF_MIN_RHO),
            (
                keys::radar::FILTER_MIN_RANGE_KM,
                radar_filter::OFF_MIN_RANGE_KM,
            ),
        ] {
            for stranger in [900.0_f64, -900.0] {
                store.set(keys::radar::CATEGORY, id, SettingValue::Float(stranger));
                assert_eq!(
                    store.effective_float(&registry, keys::radar::CATEGORY, id),
                    off,
                    "{id} stored as {stranger} resolved to something that hides gates"
                );
            }
            // And the file is left as it was found: resolution is a read.
            assert_eq!(
                store.value(keys::radar::CATEGORY, id),
                Some(SettingValue::Float(-900.0)),
                "{id}: reading the stored value rewrote it"
            );
        }
    }

    #[test]
    fn an_out_of_range_stored_value_resolves_clamped_not_blank() {
        let registry = registry();
        let mut store = settings::SettingsStore::open(
            std::env::temp_dir().join("settings-catalog-proof-never-written.json"),
        );
        store.set(
            keys::vol3d::CATEGORY,
            keys::vol3d::OPACITY,
            SettingValue::Float(99.0),
        );
        assert_eq!(
            store.effective_float(&registry, keys::vol3d::CATEGORY, keys::vol3d::OPACITY),
            1.0
        );
        store.set(
            keys::map::CATEGORY,
            keys::map::BASEMAP_STYLE,
            SettingValue::Text("vaporwave".to_owned()),
        );
        assert_eq!(
            store.effective_text(&registry, keys::map::CATEGORY, keys::map::BASEMAP_STYLE),
            map_scene::MapStylePreset::default().id()
        );
    }
}
