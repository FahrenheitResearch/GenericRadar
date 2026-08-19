use std::path::PathBuf;

use eframe::egui;

mod app;
mod app_support;
mod hazards;
mod legend;
mod live_service;
mod load_service;
mod nearest_site;
mod palettes;
mod pane_canvas;
mod popup;
mod probe;
mod product;
mod product_availability;
mod product_picker;
mod render_service;
// `settings_ui` and `theme` are each compiled in a second home as well - the
// `settings` crate's ui harness and the `theme_gallery` example include them
// by `#[path]` - so items this binary does not call (the deep-link openers,
// the gallery's bevel toolkit) are still live API. `dead_code` is judged per
// compilation unit and cannot see those callers.
#[allow(dead_code)]
mod settings_ui;
mod sites_service;
mod sweep;
#[allow(dead_code)]
mod theme;
mod vol3d;
mod vrot;
mod warnings_service;
mod xsection;

/// Overrides where warnings come from, for a daemon that is not on this
/// machine. A base URL selects it; `off` pins the public feed.
const WARNINGS_URL_ENV: &str = "RADAR_WORKSTATION_WARNINGS_URL";

/// Startup intent parsed from the command line: a Level II file to open or a
/// site to go live on, plus an optional starting camera.
///
/// The camera options exist so a given view is reproducible from the command
/// line. Driving this window with synthetic mouse input is unreliable — Windows
/// refuses foreground changes from a background process — so a stated camera is
/// the only honest way to capture a specific pan or zoom.
#[derive(Default)]
struct Startup {
    input_path: Option<PathBuf>,
    live_site: Option<String>,
    zoom_km_per_point: Option<f32>,
    center_km: Option<(f64, f64)>,
    /// Warnings source, as written on the command line. `None` falls back to
    /// [`WARNINGS_URL_ENV`] and then to the default.
    warnings_url: Option<String>,
    /// Product to open on, as a registry id or alias. Same reason as the
    /// camera options: it is the only way to photograph a product on real data.
    product: Option<String>,
    /// Open the 3D volume explorer at startup.
    vol3d: bool,
}

/// `radar-workstation [<level2-file>] [--live <SITE>] [--zoom <km-per-point>]
/// [--center <east_km,north_km>] [--warnings-url <base-url|off>]
/// [--product <REF|VEL|DVEL|SRV|DSRV|SW|ZDR|RHO|PHI|KDP>] [--vol3d]`
fn parse_startup<I: Iterator<Item = String>>(args: I) -> Startup {
    let mut startup = Startup::default();
    let mut pending: Option<String> = None;

    for arg in args {
        if let Some(option) = pending.take() {
            apply_option(&mut startup, &option, &arg);
            continue;
        }
        match arg.split_once('=') {
            Some((option, value)) if option.starts_with("--") => {
                apply_option(&mut startup, option, value);
            }
            // A bare switch takes no value, so it must not swallow the next
            // argument the way an option would.
            _ if arg == "--vol3d" => startup.vol3d = true,
            _ if arg.starts_with("--") => pending = Some(arg),
            _ if startup.input_path.is_none() => startup.input_path = Some(PathBuf::from(arg)),
            _ => {}
        }
    }
    startup
}

fn apply_option(startup: &mut Startup, option: &str, value: &str) {
    match option {
        "--live" => startup.live_site = Some(value.to_owned()),
        "--warnings-url" => startup.warnings_url = Some(value.to_owned()),
        "--product" => startup.product = Some(value.to_owned()),
        "--vol3d" => startup.vol3d = !matches!(value, "off" | "false" | "0"),
        "--zoom" => startup.zoom_km_per_point = value.parse().ok(),
        "--center" => {
            if let Some((east, north)) = value.split_once(',')
                && let (Ok(east), Ok(north)) = (east.trim().parse(), north.trim().parse())
            {
                startup.center_km = Some((east, north));
            }
        }
        _ => {}
    }
}

/// Resolve where warnings come from: the command line first, then the
/// environment, then the default.
fn warnings_source(from_command_line: Option<String>) -> data_source::warnings::WarningsSource {
    from_command_line
        .or_else(|| std::env::var(WARNINGS_URL_ENV).ok())
        .map(|value| data_source::warnings::WarningsSource::parse(&value))
        .unwrap_or_default()
}

/// Resolve a product named on the command line through the registry, so an
/// alias such as `dealiased_velocity` works as well as `DVEL`.
///
/// An unrecognised name says so and leaves the default alone. Guessing would
/// mean a captured receipt is labelled with a product nobody asked for.
fn startup_product(requested: Option<&str>) -> Option<product::DisplayProduct> {
    let requested = requested?;
    let Some(descriptor) = product_engine::ProductRegistry::builtin().get(requested) else {
        eprintln!("unknown product {requested:?}; opening on the default product instead");
        return None;
    };
    let resolved = product::DisplayProduct::try_from_product_id(&descriptor.id);
    if resolved.is_none() {
        eprintln!(
            "product {} is in the registry but no pane can show it yet",
            descriptor.id.0
        );
    }
    resolved
}

fn main() -> eframe::Result {
    let Startup {
        input_path,
        live_site,
        zoom_km_per_point,
        center_km,
        warnings_url,
        product,
        vol3d: open_vol3d,
    } = parse_startup(std::env::args().skip(1));
    let warnings_source = warnings_source(warnings_url);
    let initial_product = startup_product(product.as_deref());
    // The settings file is parsed once, here, because the window geometry it
    // holds has to exist before the window does. The store then moves into
    // the app, so live state persists through the same handle rather than a
    // second parse racing this one.
    //
    // A mobile shell porting this must call `settings::set_app_config_root` /
    // `set_app_cache_root` with its sandbox paths BEFORE this line; the
    // desktop defaults are the conventions the rest of the workspace uses.
    let store = settings::SettingsStore::open(settings::default_settings_file());
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1500.0, 950.0])
        .with_min_inner_size([960.0, 620.0]);
    if let Some(window) = &store.workspace().window {
        // `window_snapshot` refused degenerate sizes at capture, so a size
        // that is here at all is one worth reopening at.
        if let (Some(width), Some(height)) = (window.width, window.height) {
            viewport = viewport.with_inner_size([width, height]);
        }
        if let (Some(x), Some(y)) = (window.x, window.y) {
            viewport = viewport.with_position([x, y]);
        }
        viewport = viewport.with_maximized(window.maximized);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "GenericRadar",
        native_options,
        Box::new(move |creation_context| {
            // The visual theme, before anything draws: every widget of the
            // first frame styles itself from the context this call fills in.
            // The Win95-grey daylight bench is the app's identity and the
            // default; the stored choice - set in Settings > Appearance -
            // wins when present. Applied before the first frame so the app
            // never flashes the wrong chrome.
            let variant = match store.value(
                crate::settings_ui::catalog::keys::appearance::CATEGORY,
                crate::settings_ui::catalog::keys::appearance::THEME,
            ) {
                Some(settings::SettingValue::Text(text)) if text == "dark" => theme::Variant::Dark,
                _ => theme::Variant::Light,
            };
            theme::apply(&creation_context.egui_ctx, variant);
            // Register the map's persistent GPU resources once, before any
            // pane paints. Without a wgpu render state the map cannot draw at
            // all, so say so rather than silently falling back to per-frame
            // CPU geometry.
            match creation_context.wgpu_render_state.as_ref() {
                Some(render_state) => {
                    let resources = map_scene::gpu::MapRenderResources::new(
                        &render_state.device,
                        render_state.target_format,
                    );
                    render_state
                        .renderer
                        .write()
                        .callback_resources
                        .insert(resources);
                    // The raster tile underlay owns its own pipeline and its
                    // own texture residency, registered the same way and once.
                    // Inert until a provider is picked: with no imagery
                    // selected the pane never queues a tile callback.
                    let tiles = map_scene::gpu::TileRenderResources::new(
                        &render_state.device,
                        render_state.target_format,
                    );
                    render_state
                        .renderer
                        .write()
                        .callback_resources
                        .insert(tiles);
                    // The 3D explorer owns its own pipelines and textures and
                    // registers them the same way, once, before any pane paints.
                    vol3d::init_gpu(render_state);
                }
                None => eprintln!(
                    "wgpu map unavailable: no wgpu render state; the basemap will not draw"
                ),
            }

            let mut app = app::WorkstationApp::new(
                creation_context,
                input_path,
                live_site,
                warnings_source,
                store,
            );
            app.set_initial_camera(zoom_km_per_point, center_km);
            app.set_initial_product(initial_product);
            app.set_vol3d_open(open_vol3d);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup(args: &[&str]) -> Startup {
        parse_startup(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn parses_a_bare_file_path() {
        let parsed = startup(&["C:/data/KTLX_V06"]);
        assert_eq!(parsed.input_path, Some(PathBuf::from("C:/data/KTLX_V06")));
        assert_eq!(parsed.live_site, None);
    }

    #[test]
    fn parses_both_live_flag_spellings() {
        assert_eq!(
            startup(&["--live", "KTLX"]).live_site.as_deref(),
            Some("KTLX")
        );
        assert_eq!(startup(&["--live=KTLX"]).live_site.as_deref(), Some("KTLX"));
    }

    #[test]
    fn does_not_treat_a_live_site_as_a_file_path() {
        let parsed = startup(&["--live", "KTLX"]);
        assert_eq!(parsed.input_path, None);
    }

    #[test]
    fn the_volume_explorer_switch_takes_no_value_and_does_not_eat_the_next_argument() {
        // Written as a bare switch, so `--vol3d C:/data/file` must still see
        // the path as a path rather than as the switch's value.
        let parsed = startup(&["--vol3d", "C:/data/KTLX_V06"]);
        assert!(parsed.vol3d);
        assert_eq!(parsed.input_path, Some(PathBuf::from("C:/data/KTLX_V06")));
        assert!(!startup(&["C:/data/KTLX_V06"]).vol3d);
        assert!(!startup(&["--vol3d=off"]).vol3d);
    }

    #[test]
    fn a_stated_product_resolves_through_the_registry() {
        let parsed = startup(&["--product", "DVEL"]);
        assert_eq!(
            startup_product(parsed.product.as_deref()),
            Some(product::DisplayProduct::DealiasedVelocity)
        );
    }

    #[test]
    fn a_stated_product_accepts_a_registry_alias() {
        assert_eq!(
            startup_product(Some("correlation_coefficient")),
            Some(product::DisplayProduct::CorrelationCoefficient)
        );
    }

    #[test]
    fn an_unknown_product_name_keeps_the_default_rather_than_guessing() {
        assert_eq!(startup_product(Some("AZSHR")), None);
        assert_eq!(startup_product(None), None);
    }
}
