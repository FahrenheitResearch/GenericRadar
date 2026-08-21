//! Photograph a real Level 1 record open in the real application.
//!
//! ```text
//! cargo run --release -p workstation_app --example iq_proof -- <iq-file> <out-dir>
//! ```
//!
//! # Why a proof and not a test
//!
//! Everything about this feature that can be asserted is asserted elsewhere:
//! the packed-float decode against a naive restatement of the Vaisala rule, the
//! range ladder against the record's own mask, the estimators against a second
//! implementation on the same pulses. None of that answers the question this
//! file exists for, which is whether an analyst looking at the window can SEE
//! what they are looking at:
//!
//! * is there a storm on the pane, at the right ranges, or a ring of noise?
//! * does the pane admit that the moments were computed here rather than
//!   delivered by the radar, and does it say which dwell and window made them?
//! * is the spectrum readout legible - axes, noise floor, the estimator's own
//!   mean velocity - on a light bench and a dark one?
//! * does moving the dwell slider actually redraw the field?
//!
//! So this rasterises the real application through the same offscreen wgpu path
//! `gate_filter_proof` and `theme_gallery` use, on every registered theme, and
//! writes PNGs for a human to look at. It asserts what it can on the way past -
//! a photograph of an empty pane is not evidence about anything - but the
//! photographs are the output.
//!
//! # The file
//!
//! Takes a path, so no bulk sample data ever enters the repository. The record
//! this was written against is
//! `KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250` from the NSSL
//! THREDDS server (`data.nssl.noaa.gov`, `RRDD/KOUN/2013/KOUN_20130520/IQ/`,
//! catalog rights "Freely available"): 4,434,431 bytes, 1,830 pulses at 4.0
//! degrees elevation, 19:46:01 UTC on 20 May 2013.

// The whole application, exactly as `src/main.rs` compiles it.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod annotation;
    pub mod app;
    pub mod app_support;
    pub mod current_view_export;
    pub mod file_browser;
    pub mod gate_filter_ui;
    pub mod hazards;
    pub mod iq_session;
    pub mod iq_spectrum_ui;
    pub mod legend;
    pub mod live_service;
    pub mod load_service;
    pub mod nearest_site;
    pub mod net_tuning;
    pub mod north_up;
    pub mod palette_editor;
    pub mod palettes;
    pub mod pane_canvas;
    pub mod popup;
    pub mod probe;
    pub mod product;
    pub mod product_availability;
    pub mod product_picker;
    pub mod render_service;
    pub mod research_sites;
    pub mod settings_ui;
    pub mod sites_service;
    pub mod sweep;
    pub mod theme;
    pub mod units;
    pub mod user_tables;
    pub mod vol3d;
    pub mod vrot;
    pub mod warnings_service;
    pub mod xsection;
}

#[allow(unused_imports)]
pub(crate) use source::{
    annotation, app, app_support, current_view_export, file_browser, gate_filter_ui, hazards,
    iq_session, iq_spectrum_ui, legend, live_service, load_service, nearest_site, net_tuning,
    north_up, palette_editor, palettes, pane_canvas, popup, probe, product, product_availability,
    product_picker, render_service, research_sites, settings_ui, sites_service, sweep, theme,
    units, user_tables, vol3d, vrot, warnings_service, xsection,
};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::wgpu;
use theme::Appearance;

/// Window size for the photographs, in points. The width must be a multiple of
/// 64 so the read-back's `bytes_per_row` is a multiple of 256.
const SHOT_POINTS: egui::Vec2 = egui::vec2(1408.0, 896.0);

/// `Rgba8Unorm` rather than `Rgba8UnormSrgb`: egui writes gamma-space bytes, so
/// a read-back triple IS the `Color32` egui asked for.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// How long to wait for the record to decode and estimate before giving up.
const LOAD_BUDGET: Duration = Duration::from_secs(180);

/// The badge the pane puts on a computed field, and the marker this proof
/// waits on: it appears only once a Level 1 session is installed, so it cannot
/// pass on a half-loaded window the way a "has some text" check could.
const COMPUTED_BADGE: &str = "COMPUTED";

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: cargo run --release -p workstation_app --example iq_proof \
             -- <iq-file> <out-dir>"
        );
        std::process::exit(2);
    };
    if let Err(error) = run(&input, &out_dir) {
        eprintln!("iq proof failed: {error}");
        std::process::exit(1);
    }
}

fn run(input: &Path, out_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    assert!(input.is_file(), "{} is not a file", input.display());
    std::fs::create_dir_all(out_dir)?;

    let instance = wgpu::Instance::default();
    let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .map_err(|_| {
            "no wgpu adapter on this machine: this proof is a set of photographs, so there \
             is nothing honest to do without one"
        })?;
    println!("adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("iq proof"),
        ..Default::default()
    }))?;

    let context = egui::Context::default();
    let mut renderer = Renderer::new(&device, TARGET_FORMAT, RendererOptions::PREDICTABLE);
    let mut shot = Shot {
        device: &device,
        queue: &queue,
        renderer: &mut renderer,
        context: &context,
        out_dir,
        points: SHOT_POINTS,
        pixels_per_point: 1.0,
        display_scale: 1.0,
    };

    let mut written = 0_usize;
    // Every registered theme, not the two this was authored against: the
    // catalog is the list, so a theme added tomorrow is photographed by the
    // proof written today. "Readable at any theme" is a claim about the theme
    // an analyst is running.
    for theme_spec in theme::catalog::THEMES {
        theme::apply(shot.context, &appearance(theme_spec.id, None));
        println!("\n### theme {}", theme_spec.id);
        let mut app = build(&mut shot, input, theme_spec.id);
        assert!(
            pump_until_field(&mut shot, &mut app),
            "the record never reached the pane within {LOAD_BUDGET:?} on theme {}: the \
             photographs would be of an empty pane",
            theme_spec.id
        );
        written += photograph_theme(&mut shot, &mut app, theme_spec.id);
    }

    // The interface-scale case, on the two founding themes. A fixed-size
    // spectrum panel at 160 % in the smallest window the application opens in
    // is where a readout stops fitting, and the panel's own rule is to draw
    // nothing rather than to cover the storm - which is a claim that has to be
    // looked at, not asserted.
    for theme_id in ["light", "dark"] {
        theme::apply(shot.context, &appearance(theme_id, Some("1.60")));
        shot.pixels_per_point = appearance(theme_id, Some("1.60")).ui_scale.factor();
        println!("\n### theme {theme_id} at 160 %");
        let mut app = build(&mut shot, input, &format!("{theme_id}_160"));
        assert!(
            pump_until_field(&mut shot, &mut app),
            "the record never reached the pane at 160 % on theme {theme_id}"
        );
        hover_the_storm(&mut shot, &mut app);
        written += write(&mut shot, &mut app, &format!("{theme_id}_160"), "field");
        shot.pixels_per_point = 1.0;
    }

    println!("\nwrote {written} PNGs");
    println!(
        "The PNGs above are the pre-flight. A human still has to look at them; until one \
         has, nothing here is signed off."
    );
    Ok(())
}

/// The proof frames for one theme.
fn photograph_theme(shot: &mut Shot<'_>, app: &mut app::WorkstationApp, theme_id: &str) -> usize {
    let mut written = 0;

    // 1. The field, with the cursor parked in the storm so the spectrum panel
    //    is up. This is the photograph the feature is about.
    hover_the_storm(shot, app);
    let shapes = settle_hovering(shot, app, 4);
    let seen = texts(&shapes);
    assert!(
        seen.iter().any(|text| text.contains(COMPUTED_BADGE)),
        "{theme_id}: the pane does not admit that the moments were computed here: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|text| text.contains("moments computed here from")),
        "{theme_id}: the pane header does not say which dwell and window made the field: \
         {seen:?}"
    );
    // The provenance has to be ON THE SCREEN, not merely in the shape list: a
    // header row that ran past the window would still read as present to a
    // text assertion while being invisible to the analyst.
    let window = shot.context.content_rect();
    if let Some(badge) = bounds(&shapes, COMPUTED_BADGE) {
        assert!(
            badge.right() <= window.right() && badge.left() >= window.left(),
            "{theme_id}: the computed-moments badge is drawn at {badge:?}, outside the \
             {window:?} window"
        );
    }
    assert!(
        seen.iter().any(|text| text.contains("DOPPLER SPECTRUM")),
        "{theme_id}: no spectrum panel over a gate in the storm: {seen:?}"
    );
    // Nothing may claim this record is live. Level 1 is archive material and
    // the ROC states it is not disseminated in real time.
    //
    // The words checked are the ones that would be a claim ABOUT THIS FRAME.
    // The toolbar's own "Start live" / "Go live" keys are controls for a
    // different session and are not evidence of anything; asserting on the bare
    // word "live" would fail on a button rather than on a lie.
    for forbidden in ["FEED STALLED", "STALLED", "ARCHIVE FALLBACK"] {
        assert!(
            !seen.iter().any(|text| text.contains(forbidden)),
            "{theme_id}: a time series must never read as live, and something says \
             {forbidden:?}: {seen:?}"
        );
    }
    written += write(shot, app, theme_id, "field");

    // Some research cubes preserve already-formed rays and carry neither a
    // receiver-noise measurement nor an absolute calibration. Those absences
    // change which controls are physically meaningful. Photograph the refusal
    // instead of trying to force the RVP continuous-pulse experiment below
    // onto a source that cannot support it.
    let relative_native = seen.iter().any(|text| text.contains("power is relative"));
    if relative_native {
        for required in [
            "native 32-pulse rays",
            "SNR unavailable: source has no receiver-noise calibration",
            "absolute receiver power and calibrated reflectivity are unavailable",
        ] {
            assert!(
                seen.iter().any(|text| text.contains(required)),
                "{theme_id}: relative I/Q field is missing {required:?}: {seen:?}"
            );
        }
        for forbidden in ["dBm", "dBZ"] {
            assert!(
                !seen.iter().any(|text| text.contains(forbidden)),
                "{theme_id}: an uncalibrated I/Q cube makes the fabricated {forbidden} claim: \
                 {seen:?}"
            );
        }

        let before = radial_count(app);
        set_dwell(shot, app, 256);
        let shapes = settle_hovering(shot, app, 6);
        let after = radial_count(app);
        assert_eq!(
            after, before,
            "{theme_id}: a dwell setting crossed measured ray boundaries ({before} -> {after})"
        );
        assert!(
            texts(&shapes)
                .iter()
                .any(|text| text.contains("native 32-pulse rays")),
            "{theme_id}: the pane stopped admitting that the source fixes the dwell"
        );
        written += write(shot, app, theme_id, "native_dwell_locked");

        app.settings_ui_mut().open = true;
        app.settings_ui_mut()
            .open_category(settings_ui::catalog::keys::timeseries::CATEGORY);
        written += write(shot, app, theme_id, "controls");
        app.settings_ui_mut().open = false;
        return written;
    }

    // 2. The same pulses under a longer dwell. This is the whole feature: the
    //    slider redraws the storm. Set through the real settings store, so what
    //    is photographed is the path that ships.
    let before = radial_count(app);
    set_dwell(shot, app, 256);
    let shapes = settle_hovering(shot, app, 6);
    let after = radial_count(app);
    assert!(
        after < before,
        "a longer dwell must produce fewer radials from the same pulses, but the count \
         went {before} -> {after}: the knob did not reach the estimator"
    );
    assert!(
        texts(&shapes)
            .iter()
            .any(|text| text.contains("256-pulse dwells")),
        "{theme_id}: the header still claims the old dwell after the slider moved"
    );
    println!("  dwell 64 -> 256 took the radial count {before} -> {after}");
    written += write(shot, app, theme_id, "dwell256");

    // 3. Back to the shipped dwell, with the censor off, so the photographs
    //    show what the operational threshold was throwing away.
    set_dwell(shot, app, 64);
    set_censor(
        shot,
        app,
        settings_ui::catalog::timeseries_limits::OFF_SNR_DB,
    );
    let shapes = settle_hovering(shot, app, 6);
    assert!(
        texts(&shapes)
            .iter()
            .any(|text| text.contains("no SNR threshold")),
        "{theme_id}: the censor went off and the header did not say so"
    );
    written += write(shot, app, theme_id, "censor_off");

    // 4. The other end of the same knob, and the photograph the knob is FOR.
    //    A threshold above the echo empties the pane; hovering a gate it
    //    emptied is an analyst asking what was taken away, and the panel has
    //    to answer rather than disappear.
    let storm = hover_point();
    set_censor(
        shot,
        app,
        settings_ui::catalog::timeseries_limits::MAX_SNR_DB,
    );
    let shapes = hover_a_blank_gate(shot, app, theme_id);
    let seen = texts(&shapes);
    assert!(
        seen.iter().any(|text| text.contains("DOPPLER SPECTRUM")),
        "{theme_id}: hovering a gate the censor removed produced no panel at all: {seen:?}"
    );
    assert!(
        seen.iter().any(|text| {
            text.contains("below the SNR threshold")
                || text.contains("no power above the receiver noise")
        }),
        "{theme_id}: the panel is up over a blank gate and does not say why it is blank: \
         {seen:?}"
    );
    written += write(shot, app, theme_id, "censored_gate");
    set_censor(
        shot,
        app,
        settings_ui::catalog::timeseries_limits::DEFAULT_SNR_DB,
    );
    set_hover_point(storm);

    // 5. The controls themselves, on their own page in the settings window.
    app.settings_ui_mut().open = true;
    app.settings_ui_mut()
        .open_category(settings_ui::catalog::keys::timeseries::CATEGORY);
    let shapes = settle(shot, app, 6);
    let seen = texts(&shapes);
    for row in ["Preferred pulses per dwell", "Window", "Hide gates below"] {
        assert!(
            seen.iter().any(|text| text.contains(row)),
            "{theme_id}: the Level 1 page is missing the {row:?} control: {seen:?}"
        );
    }
    written += write(shot, app, theme_id, "controls");
    app.settings_ui_mut().open = false;

    written
}

/// This proof's axes on the named theme.
fn appearance(theme_id: &str, ui_scale: Option<&'static str>) -> Appearance {
    theme::settings::appearance_from_ids(Some(theme_id), None, None, None, ui_scale)
}

/// Where the pointer is parked, once a gate with weather in it has been found.
///
/// Searched rather than hardcoded. An `Ascope` task is a STARE - the antenna is
/// parked, so every dwell is at nearly one azimuth - and the field is a narrow
/// spoke rather than a sector. Which pixel that spoke lands on depends on the
/// camera the application chose, the window size and the interface scale, so a
/// hardcoded fraction of the pane is a guess that silently photographs empty
/// basemap the day any of the three changes.
static HOVER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn hover_point() -> egui::Pos2 {
    let packed = HOVER.load(std::sync::atomic::Ordering::Relaxed);
    egui::pos2(
        f32::from_bits((packed >> 32) as u32),
        f32::from_bits(packed as u32),
    )
}

fn set_hover_point(at: egui::Pos2) {
    let packed = (u64::from(at.x.to_bits()) << 32) | u64::from(at.y.to_bits());
    HOVER.store(packed, std::sync::atomic::Ordering::Relaxed);
}

/// Find a pixel whose readout names a real gate, and park the pointer on it.
///
/// The spectrum panel is the application's own answer to "there is data under
/// the cursor", so using it as the search oracle means the photograph is taken
/// wherever the shipped code says there is data - not where this file guessed
/// there would be.
fn hover_the_storm(shot: &mut Shot<'_>, app: &mut app::WorkstationApp) {
    // The radar sits at the camera centre and the spoke runs out from it, so
    // the search walks outward along rays rather than over a rectangle: a grid
    // fine enough to hit a three-degree wedge would be thousands of passes.
    let pane = shot.context.content_rect();
    let centre = pane.center();
    let reach = (pane.width().min(pane.height()) * 0.5) - 8.0;
    // Start a little way out rather than at the antenna. The first gates of a
    // stare are transmit-recovery and ground clutter - a real measurement, and
    // the least interesting one in the record: a flat clutter spectrum at 3 km
    // says nothing about what this readout is for. Walking outward from a fifth
    // of the reach lands the cursor in the body of the echo instead. If nothing
    // is found out there the loop still covers everything beyond it, and the
    // second pass below covers what was skipped.
    for step in 10..=48 {
        let radius = reach * step as f32 / 48.0;
        for spoke in 0..72 {
            let angle = std::f32::consts::TAU * spoke as f32 / 72.0;
            let at = centre + egui::vec2(radius * angle.sin(), -radius * angle.cos());
            if !pane.contains(at) {
                continue;
            }
            // TWO passes, and the second is the one that answers. The pane
            // records where the pointer was and probes it on the NEXT frame -
            // deliberately, so the readout never reaches into the volume
            // mid-paint - so a single pass reports the panel belonging to the
            // PREVIOUS candidate. Reading that one lands the search on a pixel
            // one step behind the one it thinks it found, which is how the
            // first run of this proof parked the cursor outside the sweep and
            // then failed on its own photograph.
            frame(shot, app, vec![egui::Event::PointerMoved(at)]);
            let shapes = frame(shot, app, vec![egui::Event::PointerMoved(at)]);
            if texts(&shapes)
                .iter()
                .any(|text| text.contains("DOPPLER SPECTRUM"))
            {
                set_hover_point(at);
                println!(
                    "  found a gate at {:.0},{:.0} ({:.0} points from the radar)",
                    at.x, at.y, radius
                );
                for _ in 0..3 {
                    frame(shot, app, vec![egui::Event::PointerMoved(at)]);
                }
                return;
            }
        }
    }
    // Nothing in the body of the echo: fall back to everything, including the
    // near gates the walk above skipped. A clutter spectrum is a worse
    // photograph than a storm one, and a far better one than no photograph.
    for step in 1..10 {
        let radius = reach * step as f32 / 48.0;
        for spoke in 0..72 {
            let angle = std::f32::consts::TAU * spoke as f32 / 72.0;
            let at = centre + egui::vec2(radius * angle.sin(), -radius * angle.cos());
            if !pane.contains(at) {
                continue;
            }
            frame(shot, app, vec![egui::Event::PointerMoved(at)]);
            let shapes = frame(shot, app, vec![egui::Event::PointerMoved(at)]);
            if texts(&shapes)
                .iter()
                .any(|text| text.contains("DOPPLER SPECTRUM"))
            {
                set_hover_point(at);
                println!("  found a near gate at {:.0},{:.0}", at.x, at.y);
                for _ in 0..3 {
                    frame(shot, app, vec![egui::Event::PointerMoved(at)]);
                }
                return;
            }
        }
    }
    panic!(
        "no pixel anywhere in the pane produced a spectrum readout: either nothing was \
         drawn, or the readout never reaches the glass"
    );
}

/// Find a gate the pane left BLANK - sampled, and with nothing drawn at it -
/// and park the pointer on it.
///
/// The same outward walk `hover_the_storm` uses, with the probe readout as the
/// oracle instead of the spectrum panel: a gate the renderer drew empty reads
/// "SAMPLED BUT UNUSABLE" under the cursor, and that is the pane's own
/// statement that there IS a gate there and it is blank. Searched rather than
/// hardcoded for the reason the storm gate is - which gate is blank depends on
/// the threshold, the dwell and the record.
fn hover_a_blank_gate(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    theme_id: &str,
) -> Vec<egui::Shape> {
    let pane = shot.context.content_rect();
    let centre = pane.center();
    let reach = (pane.width().min(pane.height()) * 0.5) - 8.0;
    for step in 1..=48 {
        let radius = reach * step as f32 / 48.0;
        for spoke in 0..72 {
            let angle = std::f32::consts::TAU * spoke as f32 / 72.0;
            let at = centre + egui::vec2(radius * angle.sin(), -radius * angle.cos());
            if !pane.contains(at) {
                continue;
            }
            // Two passes, for the reason `hover_the_storm` takes two: the pane
            // probes the PREVIOUS frame's pointer position.
            frame(shot, app, vec![egui::Event::PointerMoved(at)]);
            let shapes = frame(shot, app, vec![egui::Event::PointerMoved(at)]);
            if texts(&shapes)
                .iter()
                .any(|text| text.contains("SAMPLED BUT UNUSABLE"))
            {
                set_hover_point(at);
                println!("  found a blank gate at {:.0},{:.0}", at.x, at.y);
                return settle_hovering(shot, app, 4);
            }
        }
    }
    panic!(
        "{theme_id}: no pixel in the pane reads as a sampled, blank gate, so there is \
         nothing here to ask why it is empty"
    );
}

/// Settle without letting the pointer leave: the spectrum panel is a hover
/// readout, so a `PointerGone` between passes would photograph it gone.
fn settle_hovering(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    passes: usize,
) -> Vec<egui::Shape> {
    let at = hover_point();
    let mut last = Vec::new();
    for _ in 0..passes.max(1) {
        last = frame(shot, app, vec![egui::Event::PointerMoved(at)]);
        std::thread::sleep(Duration::from_millis(20));
    }
    last
}

/// How many radials the field on screen is made of.
fn radial_count(app: &app::WorkstationApp) -> usize {
    app.current_volume()
        .map(|volume| volume.cuts.iter().map(|cut| cut.radials.len()).sum())
        .unwrap_or_default()
}

/// Move the dwell through the real settings store and the real apply path.
fn set_dwell(shot: &mut Shot<'_>, app: &mut app::WorkstationApp, pulses: i64) {
    app.apply_setting_for_proof(
        settings_ui::catalog::keys::timeseries::CATEGORY,
        settings_ui::catalog::keys::timeseries::DWELL_PULSES,
        settings::SettingValue::Int(pulses),
    );
    settle_hovering(shot, app, 3);
}

fn set_censor(shot: &mut Shot<'_>, app: &mut app::WorkstationApp, db: f64) {
    app.apply_setting_for_proof(
        settings_ui::catalog::keys::timeseries::CATEGORY,
        settings_ui::catalog::keys::timeseries::SNR_MIN_DB,
        settings::SettingValue::Float(db),
    );
    settle_hovering(shot, app, 3);
}

// ---------------------------------------------------------------------------
// Driving and photographing the real application.
// ---------------------------------------------------------------------------

struct Shot<'a> {
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    renderer: &'a mut Renderer,
    context: &'a egui::Context,
    out_dir: &'a Path,
    points: egui::Vec2,
    pixels_per_point: f32,
    display_scale: f32,
}

impl Shot<'_> {
    fn size_in_pixels(&self) -> (u32, u32) {
        let width = (self.points.x * self.pixels_per_point).round() as u32;
        let height = (self.points.y * self.pixels_per_point).round() as u32;
        (width - width % 64, height)
    }
}

/// The application, built as `main.rs` builds it, on its own settings file and
/// config root so a real one on this machine cannot change what is
/// photographed.
fn build(shot: &mut Shot<'_>, input: &Path, tag: &str) -> app::WorkstationApp {
    let settings_file = shot.out_dir.join(format!("iq-proof-{tag}.json"));
    let _ = std::fs::remove_file(&settings_file);
    let store = settings::SettingsStore::open(settings_file);
    let config_root = shot.out_dir.join("iq-proof-config");
    std::fs::create_dir_all(&config_root).expect("create the capture's config root");
    settings::set_app_config_root(&config_root);
    let creation = eframe::CreationContext::_new_kittest(shot.context.clone());
    app::WorkstationApp::new(
        &creation,
        Some(input.to_path_buf()),
        None,
        data_source::warnings::WarningsSource::default(),
        store,
    )
}

fn frame(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    events: Vec<egui::Event>,
) -> Vec<egui::Shape> {
    let mut eframe_frame = eframe::Frame::_new_kittest();
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, shot.points)),
        events,
        predicted_dt: 0.25,
        ..Default::default()
    };
    raw.viewports
        .entry(raw.viewport_id)
        .or_default()
        .native_pixels_per_point = Some(shot.display_scale);
    let mut output = shot.context.run_ui(raw, |ui| {
        <app::WorkstationApp as eframe::App>::ui(app, ui, &mut eframe_frame)
    });
    upload(shot, &mut output.textures_delta);
    output
        .shapes
        .into_iter()
        .map(|clipped| clipped.shape)
        .collect()
}

fn settle(shot: &mut Shot<'_>, app: &mut app::WorkstationApp, passes: usize) -> Vec<egui::Shape> {
    let mut last = Vec::new();
    for _ in 0..passes.max(1) {
        last = frame(shot, app, Vec::new());
        std::thread::sleep(Duration::from_millis(20));
    }
    last
}

/// Drive the application until a field is on the pane.
///
/// The marker is the pane's own admission that the moments were computed here,
/// which only appears once a Level 1 session is installed - so this cannot pass
/// on a half-loaded window the way a "has some text" check could.
fn pump_until_field(shot: &mut Shot<'_>, app: &mut app::WorkstationApp) -> bool {
    let start = Instant::now();
    loop {
        let shapes = frame(shot, app, Vec::new());
        if texts(&shapes)
            .iter()
            .any(|text| text.contains(COMPUTED_BADGE))
        {
            println!("  field on the pane after {:?}", start.elapsed());
            for _ in 0..24 {
                frame(shot, app, Vec::new());
                std::thread::sleep(Duration::from_millis(20));
            }
            return true;
        }
        if start.elapsed() > LOAD_BUDGET {
            let seen = texts(&shapes);
            eprintln!("  gave up; the window said: {seen:?}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// Rasterise one more pass and write it out.
fn write(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    theme_name: &str,
    stage: &str,
) -> usize {
    let at = hover_point();
    for _ in 0..16 {
        frame(shot, app, vec![egui::Event::PointerMoved(at)]);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut eframe_frame = eframe::Frame::_new_kittest();
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, shot.points)),
        events: vec![egui::Event::PointerMoved(at)],
        predicted_dt: 0.25,
        ..Default::default()
    };
    raw.viewports
        .entry(raw.viewport_id)
        .or_default()
        .native_pixels_per_point = Some(shot.display_scale);
    let mut output = shot.context.run_ui(raw, |ui| {
        <app::WorkstationApp as eframe::App>::ui(app, ui, &mut eframe_frame)
    });
    upload(shot, &mut output.textures_delta);
    let clipped = shot
        .context
        .tessellate(output.shapes, output.pixels_per_point);
    assert!(
        !clipped.is_empty(),
        "the application tessellated nothing; the photograph would be a lie"
    );
    let (width_px, height_px) = shot.size_in_pixels();
    let pixels = rasterise(shot, &clipped, width_px, height_px, shot.pixels_per_point);
    let file = shot.out_dir.join(format!("iq_{theme_name}_{stage}.png"));
    image::RgbaImage::from_raw(width_px, height_px, pixels)
        .expect("readback size matches the target")
        .save(&file)
        .expect("write PNG");
    println!("  wrote {} ({width_px}x{height_px})", file.display());
    1
}

fn upload(shot: &mut Shot<'_>, delta: &mut eframe::epaint::textures::TexturesDelta) {
    for (id, image) in &delta.set {
        shot.renderer
            .update_texture(shot.device, shot.queue, *id, image);
    }
    for id in &delta.free {
        shot.renderer.free_texture(id);
    }
    *delta = eframe::epaint::textures::TexturesDelta::default();
}

fn texts(shapes: &[egui::Shape]) -> Vec<String> {
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

/// Where a text shape actually landed, in points.
fn bounds(shapes: &[egui::Shape], needle: &str) -> Option<egui::Rect> {
    fn walk(shape: &egui::Shape, needle: &str) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Text(text) if text.galley.text().contains(needle) => {
                Some(text.galley.rect.translate(text.pos.to_vec2()))
            }
            egui::Shape::Vec(nested) => nested.iter().find_map(|shape| walk(shape, needle)),
            _ => None,
        }
    }
    shapes.iter().find_map(|shape| walk(shape, needle))
}

fn rasterise(
    shot: &mut Shot<'_>,
    clipped: &[egui::ClippedPrimitive],
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> Vec<u8> {
    let device = shot.device;
    let queue = shot.queue;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("iq proof target"),
        size: wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("iq proof readback"),
        size: u64::from(width_px) * u64::from(height_px) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let screen = ScreenDescriptor {
        size_in_pixels: [width_px, height_px],
        pixels_per_point: scale,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("iq proof"),
    });
    let _extra = shot
        .renderer
        .update_buffers(device, queue, &mut encoder, clipped, &screen);
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("iq proof pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        shot.renderer.render(&mut pass, clipped, &screen);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width_px * 4),
                rows_per_image: Some(height_px),
            },
        },
        wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    receiver.recv().expect("map callback").expect("map read");
    let pixels = slice.get_mapped_range().to_vec();
    readback.unmap();
    pixels
}

/// Drive a future to completion on this thread.
fn pollster_block<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}
