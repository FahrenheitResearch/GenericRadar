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

use settings::{ChoiceOption, SettingKind, SettingSpec, SettingsCategory, SettingsRegistry};

/// Stable identifiers. These strings are the persistence contract: they name
/// values in every settings file already written, so they are never reused
/// for a different meaning (renaming one orphans the stored value, which is
/// safe; reusing one misreads it, which is not).
pub mod keys {
    pub mod appearance {
        pub const CATEGORY: &str = "appearance";
        pub const THEME: &str = "theme";
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
        pub const RANGE_RINGS: &str = "range_rings";
        pub const BOUNDARIES: &str = "boundaries";
    }
    pub mod radar {
        pub const CATEGORY: &str = "radar";
        pub const QUALITY: &str = "quality";
        pub const SWEEP_ANIMATION: &str = "sweep_animation";
        pub const SWEEP_SPEED: &str = "sweep_speed";
        pub const LEGEND: &str = "legend";
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
    pub mod data {
        pub const CATEGORY: &str = "data";
        pub const STARTUP_SITE: &str = "startup_site";
        pub const RESUME_LAST_SITE: &str = "resume_last_site";
        pub const POLL_SECONDS: &str = "poll_seconds";
        pub const HISTORY_MAX_FRAMES: &str = "history_max_frames";
        pub const HISTORY_MAX_MB: &str = "history_max_mb";
        pub const LIVE_CACHE_LIMIT_MB: &str = "live_cache_limit_mb";
        pub const TILE_CACHE_LIMIT_MB: &str = "tile_cache_limit_mb";
    }
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
pub fn registry() -> SettingsRegistry {
    let mut registry = SettingsRegistry::new();
    registry.register(appearance_category());
    registry.register(map_category());
    registry.register(radar_category());
    registry.register(navigation_category());
    registry.register(vol3d_category());
    registry.register(analysis_category());
    registry.register(data_category());
    registry
}

fn appearance_category() -> SettingsCategory {
    use keys::appearance as k;
    let theme_options = vec![
        ChoiceOption::new("light", "Daylight bench (Win95)"),
        ChoiceOption::new("dark", "Night bench"),
    ];
    let toolbar_options = vec![
        ChoiceOption::new("menus", "Menu bar (compact)"),
        ChoiceOption::new("full", "Everything visible"),
    ];
    SettingsCategory::new(
        k::CATEGORY,
        "Appearance",
        vec![
            SettingSpec::new(k::THEME, "Theme", choice(theme_options, "light")).help(
                "The whole application's chrome. Daylight bench is the classic              grey - raised buttons, etched group boxes, sunken wells - and is              the app's identity; Night bench is the same language cut in              graphite for a dark room. The radar panes keep their own ground              either way: data is drawn on the map's colours, not the theme's.",
            ),
            SettingSpec::new(
                k::TOOLBAR,
                "Toolbar style",
                choice(toolbar_options, "menus"),
            )
            .help(
                "Menu bar keeps one compact row - storm controls stay on it,              the occasional ones live under File / View / Map / Tools.              Everything visible puts every control on the row itself,              which wraps on narrower windows.",
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
                 uncluttered' labels every site once fewer than about forty are on \
                 screen, and the hovered or active site always; 'Never' writes no \
                 ids at all.",
            ),
            SettingSpec::new(
                k::RANGE_RINGS,
                "Range rings",
                choice(
                    vec![
                        ChoiceOption::new("off", "Off"),
                        ChoiceOption::new("adaptive", "Adaptive"),
                        ChoiceOption::new("25", "Every 25 km"),
                        ChoiceOption::new("50", "Every 50 km"),
                        ChoiceOption::new("100", "Every 100 km"),
                    ],
                    "off",
                ),
            )
            .help(
                "Concentric distance rings about the radar. Declared ahead of the \
                 range-ring layer itself, so the choice is already stored when that \
                 layer lands.",
            )
            .pending_wiring(),
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
                "Reveal an arriving live sweep as a clockwise wipe at the antenna's own \
                 measured rate, instead of repainting the whole tilt at once.",
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
                     at about 2.5× per notch.",
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
            ),
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
            .help("Keep echo above the threshold (normal) or below it (weak-echo work)."),
            SettingSpec::new(k::OPACITY, "Opacity", slider(0.02, 1.0, 0.28, 2, "")).help(
                "How much each sample absorbs. Low values see through the storm; high \
                 values read the surface only.",
            ),
            SettingSpec::new(k::DENSITY, "Density", slider(0.2, 4.0, 0.78, 2, "")).help(
                "How quickly repeated samples accumulate into a solid body. Opacity \
                 controls each sample; density controls the pile-up.",
            ),
            SettingSpec::new(k::SHADING, "Shading", slider(0.0, 1.0, 0.9, 2, ""))
                .help("Blend from untouched palette colour (0) to lit cloud shading (1)."),
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
            .help("Steps per ray: 96, 160 or 240. Higher is smoother and costs GPU time."),
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
            ),
            SettingSpec::new(
                k::VERTICAL_EXAGGERATION,
                "Vertical exaggeration",
                slider(0.5, 6.0, 1.5, 1, "×"),
            )
            .help(
                "Purely visual stretch of height. 1× preserves physical proportions; \
                 the 1.5× default keeps a storm reading broader than deep, which is \
                 the truth.",
            ),
            SettingSpec::new(k::FOV_SCALE, "Field of view", slider(0.42, 1.1, 0.7, 2, ""))
                .help("Perspective strength of the 3D camera."),
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
            .help("The reference raster drawn under the volume."),
            SettingSpec::new(
                k::FLOOR_OPACITY,
                "Floor opacity",
                slider(0.0, 1.0, 0.82, 2, ""),
            )
            .help("How solid the floor raster is drawn."),
            SettingSpec::new(
                k::RAMP_LOW_DBZ,
                "Opacity ramp: lift-off",
                slider(-30.0, 40.0, 5.0, 0, "dBZ"),
            )
            .help(
                "Where the opacity ramp lifts off: about where a reflectivity field \
                 stops being receiver noise and starts being cloud.",
            ),
            SettingSpec::new(
                k::RAMP_HIGH_DBZ,
                "Opacity ramp: saturation",
                slider(40.0, 80.0, 60.0, 0, "dBZ"),
            )
            .help(
                "Where the ramp saturates: a hail-bearing core, which has to read as a \
                 solid body and not a brighter patch of the same haze.",
            ),
            SettingSpec::new(
                k::RAMP_GAMMA,
                "Opacity ramp: focus",
                slider(1.0, 6.0, 4.2, 1, ""),
            )
            .help(
                "Exponent between the knees; higher concentrates opacity into the \
                     cores. The default follows Marshall & Palmer 1948 / Atlas 1953 \
                     extinction physics - see the derivation in the 3D module.",
            ),
            SettingSpec::new(
                k::RAMP_FLOOR,
                "Opacity ramp: haze",
                slider(0.0, 1.0, 0.07, 2, ""),
            )
            .help(
                "Extinction at and below lift-off. Not zero, so a deep body of weak \
                     echo still reads as cloud.",
            ),
            SettingSpec::new(
                k::RAMP_GAIN,
                "Opacity ramp: body",
                slider(1.0, 12.0, 3.5, 1, ""),
            )
            .help("Extinction at and above saturation."),
            SettingSpec::new(k::SHOW_GRID, "Height grid", toggle(true))
                .help("Draw the kilometre height grid on the box walls."),
            SettingSpec::new(k::SHOW_BOX, "Box frame", toggle(true)).help("Draw the box outline."),
            SettingSpec::new(k::SHOW_LABELS, "Axis labels", toggle(true))
                .help("Draw the distance and height labels."),
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
                "Meteorological direction the storm moves FROM, used by the \
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
            ),
            SettingSpec::new(k::RESUME_LAST_SITE, "Resume last site", toggle(true)).help(
                "When no startup site is set, reopen on the radar that was live when \
                 the application last closed.",
            ),
            SettingSpec::new(
                k::POLL_SECONDS,
                "Live poll interval",
                slider(0.5, 30.0, 1.2, 1, "s"),
            )
            .help(
                "How often the live feed asks for new chunks. 1.2 s follows a \
                     volume as it arrives; raise it on a metered connection.",
            )
            // `live_service::POLL_INTERVAL` is a const consumed inside the
            // polling thread; there is no seam `app.rs` can push a value
            // through today. Declared so the choice is stored; flip this to
            // enabled in the same change that threads the interval through
            // `LiveService::new`.
            .pending_wiring(),
            SettingSpec::new(
                k::HISTORY_MAX_FRAMES,
                "History frames",
                integer(5, 200, 30, ""),
            )
            .help(
                "How many volumes the timeline keeps in memory. A real VCP-212 \
                     volume measures ~74 MiB, so the memory ceiling below usually \
                     binds first.",
            ),
            SettingSpec::new(
                k::HISTORY_MAX_MB,
                "History memory",
                integer(128, 8192, 1024, "MiB"),
            )
            .help("Memory ceiling for the volume timeline."),
            SettingSpec::new(
                k::LIVE_CACHE_LIMIT_MB,
                "Live cache on disk",
                integer(256, 16384, 2048, "MiB"),
            )
            .help(
                "Disk ceiling for downloaded Level II volumes. Declared ahead of the \
                 live-cache eviction sweep; until that lands the cache is unbounded.",
            )
            .pending_wiring(),
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
            .pending_wiring(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::SettingValue;

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
