//! Photograph the gate filter being used, on a real volume, in every theme.
//!
//! ```text
//! cargo run --release -p workstation_app --example gate_filter_proof -- <level2-file> <out-dir>
//! ```
//!
//! Nothing here rebuilds the control. The application is constructed exactly
//! as `src/main.rs` constructs it, opened on a real Level II file, pumped
//! through its own `eframe::App::ui` until the volume is on the bar, and then
//! **driven with synthetic pointer events through the real controls**: the
//! toolbar chip is clicked to open its panel, a preset row is clicked to apply
//! it, and the bar's own clear key is clicked to clear it again. Each
//! step is rasterised through the real egui to wgpu pipeline.
//!
//! So the five frames per theme are:
//!
//! 1. `off` - the shipped state. The chip reads "Filter: off", the bar offers
//!    no clear key and no pane says anything about a filter.
//! 2. `panel` - the live control open: the presets, the four thresholds with
//!    "off" written at their left ends, the range-folded toggle, and the way
//!    out.
//! 3. `panel_storm` - Storm mode applied by clicking its row, panel still
//!    down: the thresholds now carry the preset's numbers and the panel names
//!    what it is hiding, in the theme's error ink, immediately above the way
//!    out of it.
//! 4. `storm` - the same state with the panel put away. Every pane's header
//!    carries the whole statement, the legend badge stack carries its one-word
//!    copy, the chip is latched and names the preset, and the clear key sits
//!    beside it.
//! 5. `cleared` - after clicking the clear key. Back to (1), which is the half
//!    of the safety rule that says there must be one obvious way out.
//!
//! There used to be a fifth indicator and it was the loudest: a full-width
//! FILTERED band across the top of every filtered pane, which is what step 5
//! used to click. It is gone: a band that width, in that colour, across a
//! radar image is an alarm, and a filter deliberately switched on is not an
//! alarm. What these photographs are for is the harder question its removal
//! raises - whether the quiet indicators that remain are legible at the
//! sizes and scales an analyst actually runs.
//!
//! It asserts what it photographed rather than only writing the files, so a
//! run that silently stops driving the controls fails instead of producing
//! five identical pictures of an unfiltered pane.
//!
//! # What it writes
//!
//! Every capture in [`CAPTURES`] crossed with every theme in
//! `theme::catalog::THEMES`, each writing the stages its `capture_panels`
//! allows. The run COUNTS and prints that number rather than asserting a
//! remembered one, because the first account of this example claimed sixteen
//! files and two display scales when it wrote fourteen and only ever
//! rasterised at 1x.
//!
//! The theme list is the catalog's, never a list here, so a theme registered
//! tomorrow is photographed by the proof written today. That matters for this
//! feature in particular: the pane's furniture - the header and the legend
//! badge - paints its own ground on purpose and does NOT follow the theme, and
//! "on purpose" is only credible if somebody looked at it on every theme.
//!
//! Nor is the scale sweep decoration. Two different multipliers reach
//! `pixels_per_point`: the DISPLAY (a 2x panel, which egui takes as
//! `native_pixels_per_point`) and the analyst's UI-SCALE setting (which
//! `theme::install` puts in egui's zoom factor). egui lays out afresh under
//! both, so the header's one-row truncation, the chip's width, the clear key's
//! position and the legend badge's column are all different measurements at
//! each. A statement that is unmissable on a 1408-point window at 1x is not
//! evidence about the pane an analyst has at 160 % in the 960-point window
//! this application opens at - which is why `full_dense_160` exists and is the
//! tightest case here.

// The whole application, exactly as `src/main.rs` compiles it - the same
// construction `theme_gallery` and `palette_editor_proof` use, and for the
// same reason: the modules reach `crate::…`, so the example has to present
// the same crate root the binary does.
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
    pub mod playlist_preflight;
    pub mod popup;
    pub mod probe;
    pub mod product;
    pub mod product_availability;
    pub mod product_picker;
    pub mod render_service;
    pub mod research_sites;
    pub mod settings_ui;
    pub mod sites_service;
    pub mod source_field_palettes;
    pub mod source_fields;
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
    north_up, palette_editor, palettes, pane_canvas, playlist_preflight, popup, probe, product,
    product_availability, product_picker, render_service, research_sites, settings_ui,
    sites_service, source_field_palettes, source_fields, sweep, theme, units, user_tables, vol3d,
    vrot, warnings_service, xsection,
};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::wgpu;
use theme::Appearance;

/// Window size for the photographs, in points.
///
/// 1408 is 22 · 64, and the read-back below copies whole rows: wgpu requires
/// `bytes_per_row` to be a multiple of 256, so the pixel width has to be a
/// multiple of 64. Tall enough that the pane, its header, the legend badge
/// stack and the timeline are all in one frame.
const SHOT_POINTS: egui::Vec2 = egui::vec2(1408.0, 896.0);

/// The smallest window the application will open in, from `main.rs`'s
/// `with_min_inner_size`. Photographed at 2x as well as the roomy window,
/// because the safety rule is about the pane an analyst actually has: a
/// FILTERED statement that is unmissable at 1408 points and truncated to
/// nothing at 960 on a 2x display would satisfy the rule only on the
/// reviewer's monitor.
/// 960 x 2 is 1920, a multiple of 64, so the read-back still copies whole rows.
const MIN_WINDOW_POINTS: egui::Vec2 = egui::vec2(960.0, 620.0);

/// The same smallest window, in the POINTS egui sees at 160 % UI scale.
///
/// The window does not grow when an analyst turns the scale up - the chrome
/// does, and the window keeps the same 960 x 620 physical points. egui measures
/// in points AFTER the zoom factor, so that window is 600 x 387.5 points to lay
/// out in, which is why this is the tightest case the two size axes can make:
/// the least room for the bar and the least height for the pane.
const MIN_WINDOW_POINTS_AT_160: egui::Vec2 = egui::vec2(600.0, 387.5);

/// One window this proof drives the application in.
struct Capture {
    /// The toolbar the settings file is opened with.
    toolbar: &'static str,
    points: egui::Vec2,
    pixels_per_point: f32,
    /// The `appearance.density` id, or `None` for the shipped default.
    density: Option<&'static str>,
    /// The `appearance.ui_scale` id, or `None` for the shipped default.
    ui_scale: Option<&'static str>,
    /// Whether the two panel-open stages are written out as well. The panel is
    /// the same panel whichever bar opened it, so it is photographed once.
    capture_panels: bool,
    /// Goes in the file name, so the sets never overwrite each other.
    tag: &'static str,
}

impl Capture {
    /// This capture's axes on the named theme.
    ///
    /// `Appearance::by_id` alone is not enough: it puts the DEFAULT axes on
    /// the theme, so applying it in the per-theme loop would quietly reset the
    /// density and scale this capture exists to exercise.
    fn appearance(&self, theme_id: &str) -> Appearance {
        theme::settings::appearance_from_ids(
            Some(theme_id),
            None,
            None,
            self.density,
            self.ui_scale,
        )
    }

    /// The zoom the analyst's UI-scale choice puts on top of the display.
    fn ui_scale_factor(&self) -> f32 {
        self.appearance("light").ui_scale.factor()
    }
}

/// Every window the proof photographs, and why each one is here.
const CAPTURES: &[Capture] = &[
    // The compact bar in a roomy window: the shipped default, and the set the
    // panel itself is photographed in.
    Capture {
        toolbar: "menus",
        points: SHOT_POINTS,
        pixels_per_point: 1.0,
        density: None,
        ui_scale: None,
        capture_panels: true,
        tag: "menus",
    },
    // The Everything bar is stock egui widgets with the chip's bevelled
    // chrome in the middle of them - a judgement call that has to be looked
    // at rather than asserted.
    Capture {
        toolbar: "full",
        points: SHOT_POINTS,
        pixels_per_point: 1.0,
        density: None,
        ui_scale: None,
        capture_panels: false,
        tag: "full",
    },
    // The hard case for the header: the smallest allowed window on a 2x
    // display, where every pane is a quarter of 960 points wide.
    Capture {
        toolbar: "menus",
        points: MIN_WINDOW_POINTS,
        pixels_per_point: 2.0,
        density: None,
        ui_scale: None,
        capture_panels: false,
        tag: "menus_min_2x",
    },
    // The hard case for the legend BADGE: the narrowest window the
    // application opens in, carrying the widest chrome it offers.
    //
    // This is the case that failed. The Everything bar is a wrapped row of
    // stock widgets, and a latched "Filter: Storm mode" chip is wider than
    // "Filter: off"; the row overflowed, `Ui::allocate_space` expanded the
    // parent's max_rect to contain it, and the canvas allocated at
    // `available_size()` ran past the window. The legend is right-aligned
    // inside the pane, so the colour ramp went off screen and the FILTERED
    // badge was clipped to "FIL" - the indicator that says data is hidden,
    // pushed out of sight by the chip that says the same thing.
    //
    // Photographed at the minimum window rather than the roomy one because
    // the overflow gets worse as the window narrows: if the badge survives
    // here it survives everywhere the application can be opened.
    Capture {
        toolbar: "full",
        points: MIN_WINDOW_POINTS,
        pixels_per_point: 1.0,
        density: None,
        ui_scale: None,
        capture_panels: false,
        tag: "full_min",
    },
    // The worst case the two size axes can make: the tightest spacing at the
    // largest scale, in the smallest window, on both bars.
    //
    // Dense buys its density from the space BETWEEN controls, and 160 % makes
    // every glyph and every control 1.6x while the WINDOW stays 960 x 620 -
    // so the bar has the least room it will ever have and the pane the least
    // height. If the header's sentence, the legend's badge and the bar's
    // clear key survive here they survive every combination the settings
    // offer.
    Capture {
        toolbar: "menus",
        points: MIN_WINDOW_POINTS_AT_160,
        pixels_per_point: 1.0,
        density: Some("dense"),
        ui_scale: Some("1.60"),
        capture_panels: false,
        tag: "menus_dense_160",
    },
    Capture {
        toolbar: "full",
        points: MIN_WINDOW_POINTS_AT_160,
        pixels_per_point: 1.0,
        density: Some("dense"),
        ui_scale: Some("1.60"),
        capture_panels: false,
        tag: "full_dense_160",
    },
];

/// `Rgba8Unorm` rather than `Rgba8UnormSrgb` because egui writes gamma-space
/// bytes into it, so a read-back triple IS the `Color32` egui asked for - the
/// same choice, for the same reason, as `examples/theme_gallery.rs`.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// How long to wait for the volume to decode before giving up and saying so.
const LOAD_BUDGET: Duration = Duration::from_secs(120);

/// The preset this proof applies. Named by its label, because that is what the
/// row an analyst clicks actually says.
const PRESET_ROW: &str = "Storm mode";

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: cargo run --release -p workstation_app --example gate_filter_proof \
             -- <level2-file> <out-dir>"
        );
        std::process::exit(2);
    };
    if let Err(error) = run(&input, &out_dir) {
        eprintln!("gate filter proof failed: {error}");
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
        label: Some("gate filter proof"),
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
    for capture in CAPTURES {
        shot.points = capture.points;
        // The display scale is the context's, not the read-back's: egui lays
        // the whole frame out again at this scale, so the truncation in the
        // photograph is the real one. Setting it only on the raster would
        // photograph a 1x layout through a 2x lens, which proves nothing
        // about legibility.
        //
        // Two multipliers reach `pixels_per_point` and they are NOT the same
        // thing, which is the trap here. `capture.pixels_per_point` is the
        // DISPLAY - a 2x panel - and in the shipped application that arrives
        // as the platform's `native_pixels_per_point`. `appearance.ui_scale`
        // is the ANALYST's choice and `theme::install` puts it in egui's zoom
        // factor, on top. The effective scale is the product.
        //
        // So the display scale is fed in through `RawInput` like the platform
        // feeds it (see `frame`), and the zoom is left to the theme. Calling
        // `set_pixels_per_point` for the display instead would be overwritten
        // by the next `theme::apply`, because `set_pixels_per_point` and
        // `set_zoom_factor` write the same field, which would silently reduce
        // this proof's 2x capture to 1x.
        //
        // Computed rather than read back: `Context::pixels_per_point` answers
        // for the pass that has already run, and nothing has run yet here.
        shot.display_scale = capture.pixels_per_point;
        shot.pixels_per_point = capture.pixels_per_point * capture.ui_scale_factor();
        theme::apply(shot.context, &capture.appearance("light"));
        let (width_px, height_px) = shot.size_in_pixels();
        println!(
            "\n### {} - {} bar, {:.0}x{:.0} points, display {}x, ui scale {} \
             ({width_px}x{height_px} px)",
            capture.tag,
            capture.toolbar,
            capture.points.x,
            capture.points.y,
            capture.pixels_per_point,
            capture.ui_scale.unwrap_or("1.00"),
        );

        let mut app = build(&mut shot, input, capture);
        assert!(
            pump_until_loaded(&mut shot, &mut app),
            "the volume never reached the {} bar within {LOAD_BUDGET:?}: the photographs \
             would be of an empty pane, which is not evidence about a filter",
            capture.tag
        );
        // EVERY registered theme, not the two that existed when this was
        // written. The catalog is the list (`theme::catalog::THEMES`), so a
        // theme added tomorrow is photographed by the proof written today -
        // which is the whole point of the catalog being data. The pane
        // header is the indicator the safety rule names, and "legible" is a
        // claim about the theme an analyst is actually running, not about
        // the two it was authored against.
        for theme_spec in theme::catalog::THEMES {
            theme::apply(shot.context, &capture.appearance(theme_spec.id));
            written += photograph_one_theme(
                &mut shot,
                &mut app,
                &format!("{}_{}", capture.tag, theme_spec.id),
                capture.capture_panels,
            );
        }
    }

    // Counted, not described. The first account of this run said it wrote
    // sixteen PNGs and photographed 1x and 2x; it wrote fourteen and only ever
    // rasterised at 1x. A number in prose is a number nobody rechecks.
    println!("\nwrote {written} PNGs");
    println!(
        "The PNGs above are the pre-flight. A human still has to look at them; until one \
         has, nothing here is signed off."
    );
    Ok(())
}

/// The five frames, in the order an analyst would produce them. Every one of
/// them is asserted; `capture_panels` only decides which are also written out,
/// because the panel is the same panel whichever bar opened it.
///
/// Returns how many PNGs it wrote, so the run can count its own output rather
/// than claim a number.
fn photograph_one_theme(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    theme_name: &str,
    capture_panels: bool,
) -> usize {
    println!("\n=== {theme_name} ===");
    let mut written = 0_usize;

    // 1. The shipped state.
    let shapes = settle(shot, app, 4);
    assert!(
        texts(&shapes)
            .iter()
            .any(|text| text.starts_with("Filter: off")),
        "{theme_name}: the bar carries no gate-filter chip at all"
    );
    assert!(
        !texts(&shapes)
            .iter()
            .any(|text| text.contains(gate_filter_ui::FILTERED_WORD)),
        "{theme_name}: an unfiltered application is already claiming to filter"
    );
    written += write(shot, app, theme_name, "off");

    // 2. The live control, opened by clicking the chip on the real bar.
    let chip = position(&shapes, "Filter: off")
        .expect("the chip drew its own label, so it has a position");
    click(shot, app, chip);
    let shapes = settle(shot, app, 3);
    let panel_rows = texts(&shapes);
    for wanted in [
        "Gate filter",
        PRESET_ROW,
        "Clean air",
        "Hide REF below",
        "Hide below RhoHV",
        "Show everything",
    ] {
        assert!(
            panel_rows.iter().any(|text| text.contains(wanted)),
            "{theme_name}: the open panel drew no {wanted:?}: {panel_rows:?}"
        );
    }
    // "off" written at the left end of every threshold, rather than the
    // number that happens to sit there.
    assert!(
        panel_rows
            .iter()
            .filter(|text| text.trim() == "off")
            .count()
            >= 4,
        "{theme_name}: the thresholds are not reading 'off' at their off positions: \
         {panel_rows:?}"
    );
    if capture_panels {
        written += write(shot, app, theme_name, "panel");
    }

    // 3. Apply a preset by clicking its row. Photographed with the panel still
    //    down, because this is the frame that shows the thresholds carrying
    //    the preset's numbers and the panel saying what it is now hiding.
    let row = position(&shapes, PRESET_ROW).expect("the panel drew the preset row");
    click(shot, app, row);
    let shapes = settle(shot, app, 3);
    let applied = texts(&shapes);
    assert!(
        applied.iter().any(|text| text.trim() == "20.0 dBZ"),
        "the thresholds do not carry the preset's numbers: {applied:?}"
    );
    assert!(
        applied
            .iter()
            .any(|text| text.starts_with(gate_filter_ui::FILTERED_WORD)),
        "the panel does not say what it is now hiding: {applied:?}"
    );
    if capture_panels {
        written += write(shot, app, theme_name, "panel_storm");
    }

    // 4. Put the panel away, so the pane and its header are what the frame
    //    is about.
    //
    // Put away with Escape rather than by clicking the chip again, because
    // the chip is not always reachable. In the smallest window at 160 % UI
    // scale the panel is 380 points wide in a 600-point window and taller than
    // the room below the bar, so egui clamps the Area to the screen and it
    // comes to rest OVER its own chip: a second click on the chip's position
    // lands inside the panel, which is not a dismissal, and the panel stays up
    // for ever. Escape is the analyst's way out of it in that state too, which
    // is the point of driving the shipped dismissal rule rather than a click.
    press_escape(shot, app);
    let shapes = settle(shot, app, 4);
    assert!(
        !texts(&shapes).iter().any(|text| text == "Gate filter"),
        "{theme_name}: the filter panel would not close, so the frame below is a \
         photograph of the panel rather than of the pane"
    );
    let after = texts(&shapes);
    // The pane header's statement: the one a setting cannot switch off. It
    // starts with the word and a colon, which is what tells it apart from the
    // legend's one-word badge in a list of runs.
    let statement = after
        .iter()
        .find(|text| text.starts_with(&format!("{}:", gate_filter_ui::FILTERED_WORD)))
        .unwrap_or_else(|| {
            panic!("{theme_name}: a filtered pane made no statement at all: {after:?}")
        });
    println!("  header: {statement}");
    assert!(
        after.iter().any(|text| text.contains(PRESET_ROW)),
        "{theme_name}: the chip does not name the preset that is on: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|text| text.trim() == gate_filter_ui::FILTERED_WORD),
        "{theme_name}: the legend badge stack carries no filter badge: {after:?}"
    );
    // And the way out is on the bar, beside the chip that is latched.
    assert!(
        after
            .iter()
            .any(|text| text.trim() == gate_filter_ui::CLEAR_GLYPH),
        "{theme_name}: a filtered bar offers no way out: {after:?}"
    );
    // And they landed ON THE SCREEN. Existing in the shape list is not the
    // same as being visible: a pane allocated wider than the window draws its
    // right-aligned legend past the edge, where the badge is clipped to "FIL"
    // while still reading as FILTERED to the assertion above. That is exactly
    // what shipped, and the assertion above is exactly what missed it.
    let window = shot.context.content_rect();
    let badge = exact_bounds(&shapes, gate_filter_ui::FILTERED_WORD)
        .expect("the badge is in the shape list, so it has a rect");
    assert!(
        badge.right() <= window.right() && badge.left() >= window.left(),
        "{theme_name}: the FILTERED badge is drawn at {badge:?}, outside the {window:?} \
         window - the one indicator that says data is hidden has been pushed off screen"
    );
    // The clear key too. It is the only way out of a filtered view, and a bar
    // that overflows its window - which the Everything bar does, and does more
    // once the chip latches - would push it past the edge. This assertion is
    // the reason the key is a single glyph rather than a labelled button.
    let key = exact_bounds(&shapes, gate_filter_ui::CLEAR_GLYPH).expect("the key has a rect");
    assert!(
        key.right() <= window.right() && key.left() >= window.left(),
        "{theme_name}: the clear key is drawn at {key:?}, outside the {window:?} window - \
         the one obvious action out has been pushed off screen"
    );
    written += write(shot, app, theme_name, "storm");

    // 5. The one obvious way out: click the clear key on the bar.
    let key_at =
        exact_position(&shapes, gate_filter_ui::CLEAR_GLYPH).expect("the clear key has a position");
    click(shot, app, key_at);
    let shapes = settle(shot, app, 4);
    let cleared = texts(&shapes);
    assert!(
        !cleared
            .iter()
            .any(|text| text.contains(gate_filter_ui::FILTERED_WORD)),
        "{theme_name}: clearing left the pane still claiming to filter: {cleared:?}"
    );
    assert!(
        cleared.iter().any(|text| text.starts_with("Filter: off")),
        "{theme_name}: the chip did not return to off: {cleared:?}"
    );
    if capture_panels {
        written += write(shot, app, theme_name, "cleared");
    }
    written
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
    /// The window the application is being driven in, in points.
    points: egui::Vec2,
    /// The display scale it is being driven at. Not a zoom on the finished
    /// picture: egui lays every galley, rounded rectangle and truncation out
    /// again at this scale, so a line that fits at 1x is not evidence that it
    /// fits at 2x. The DISPLAY scale times the analyst's UI scale.
    pixels_per_point: f32,
    /// The display's own scale, without the analyst's UI scale on it. Fed to
    /// egui as `native_pixels_per_point`, which is where the platform puts it.
    display_scale: f32,
}

impl Shot<'_> {
    /// The read-back size. wgpu copies whole rows and wants `bytes_per_row` a
    /// multiple of 256, so the pixel width has to be a multiple of 64 - which
    /// is a constraint on `points.x * pixels_per_point`, not on either alone.
    fn size_in_pixels(&self) -> (u32, u32) {
        let width = (self.points.x * self.pixels_per_point).round() as u32;
        let height = (self.points.y * self.pixels_per_point).round() as u32;
        assert_eq!(
            width % 64,
            0,
            "{width} px is not a multiple of 64: the read-back would shear"
        );
        (width, height)
    }
}

/// The application, built exactly as `main.rs` builds it, on its own settings
/// file and its own config root so a real one on this machine cannot change
/// what is photographed.
///
/// `toolbar` is the only value written into that file before the application
/// reads it: the gate filter itself must start from the shipped default, or
/// the first frame would not be evidence that the shipped default is off.
/// `tag` names the file, so two captures on the same bar cannot inherit each
/// other's state.
fn build(shot: &mut Shot<'_>, volume: &Path, capture: &Capture) -> app::WorkstationApp {
    let settings_file = shot
        .out_dir
        .join(format!("gate-filter-proof-{}.json", capture.tag));
    let _ = std::fs::remove_file(&settings_file);
    let mut store = settings::SettingsStore::open(settings_file);
    store.set(
        settings_ui::catalog::keys::appearance::CATEGORY,
        settings_ui::catalog::keys::appearance::TOOLBAR,
        settings::SettingValue::Text(capture.toolbar.to_owned()),
    );
    // The two size axes, written into the settings file rather than pushed at
    // the context, because that is how an analyst sets them: the app reads
    // them back through `WorkstationApp::appearance` and installs them itself,
    // so what is photographed is the path that ships.
    for (key, value) in [
        (theme::settings::keys::DENSITY, capture.density),
        (theme::settings::keys::UI_SCALE, capture.ui_scale),
    ] {
        if let Some(value) = value {
            store.set(
                theme::settings::keys::CATEGORY,
                key,
                settings::SettingValue::Text(value.to_owned()),
            );
        }
    }
    // The config root is process-global and set-once, so only the first call
    // installs it; the assertion is what makes that explicit rather than a
    // silent no-op on the second pass.
    let config_root = shot.out_dir.join("gate-filter-proof-config");
    std::fs::create_dir_all(&config_root).expect("create the capture's config root");
    settings::set_app_config_root(&config_root);
    assert_eq!(
        settings::app_config_root(),
        config_root,
        "the capture must not read a real colour table folder"
    );
    let creation = eframe::CreationContext::_new_kittest(shot.context.clone());
    app::WorkstationApp::new(
        &creation,
        Some(volume.to_path_buf()),
        None,
        data_source::warnings::WarningsSource::default(),
        store,
    )
}

/// One `eframe::App::ui` pass with the given events. Texture deltas are
/// uploaded on every pass, including the ones that are never rasterised: the
/// font atlas arrives with the first of them, and a capture that skipped it
/// would sample a texture nobody uploaded and come back black.
fn frame(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    events: Vec<egui::Event>,
) -> Vec<egui::Shape> {
    let mut eframe_frame = eframe::Frame::_new_kittest();
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, shot.points)),
        events,
        // A quarter second per pass: egui drives every fade off
        // `predicted_dt`, and at the default sixtieth of a second a panel
        // photographed three passes after it opened is still part-way
        // through its fade.
        predicted_dt: 0.25,
        ..Default::default()
    };
    // The DISPLAY's scale, delivered the way winit delivers it, so the
    // analyst's UI-scale setting can keep egui's zoom factor to itself and the
    // two multiply instead of overwriting each other. See the capture loop.
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
    }
    last
}

/// Press and release on a point, then let the frame after it settle.
fn click(shot: &mut Shot<'_>, app: &mut app::WorkstationApp, at: egui::Pos2) {
    frame(shot, app, pointer(at, true));
    frame(shot, app, pointer(at, false));
    // Park the pointer off the controls, so a photograph shows a control's
    // resting state rather than its hover state.
    frame(shot, app, vec![egui::Event::PointerGone]);
}

/// Press Escape, then let the frame after it settle.
fn press_escape(shot: &mut Shot<'_>, app: &mut app::WorkstationApp) {
    frame(
        shot,
        app,
        vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    frame(shot, app, vec![egui::Event::PointerGone]);
}

fn pointer(at: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(at),
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

/// Drive the application until the tilt readout carries a measured elevation.
fn pump_until_loaded(shot: &mut Shot<'_>, app: &mut app::WorkstationApp) -> bool {
    let start = Instant::now();
    loop {
        let shapes = frame(shot, app, Vec::new());
        if texts(&shapes).iter().any(|text| is_an_elevation(text)) {
            println!("volume on the bar after {:?}", start.elapsed());
            // A few more passes so the first render lands in the pane.
            for _ in 0..24 {
                frame(shot, app, Vec::new());
                std::thread::sleep(Duration::from_millis(20));
            }
            return true;
        }
        if start.elapsed() > LOAD_BUDGET {
            return false;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// `active_tilt_label` renders a measured elevation as `0.48°` and its
/// placeholders (`No tilt`, `Unavailable`) as words.
fn is_an_elevation(text: &str) -> bool {
    text.strip_suffix('°')
        .is_some_and(|number| number.parse::<f32>().is_ok())
}

/// Rasterise one more pass and write it out.
///
/// The wait first is not padding. The render worker is a thread, and a pane
/// whose raster is still in flight prints "rendering" in its header - a
/// photograph taken there would show the application mid-thought rather than
/// at rest, which is not what a reviewer is being asked to look at.
fn write(
    shot: &mut Shot<'_>,
    app: &mut app::WorkstationApp,
    theme_name: &str,
    stage: &str,
) -> usize {
    for _ in 0..24 {
        frame(shot, app, Vec::new());
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut eframe_frame = eframe::Frame::_new_kittest();
    let mut output = shot.context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, shot.points)),
            predicted_dt: 0.25,
            ..Default::default()
        },
        |ui| <app::WorkstationApp as eframe::App>::ui(app, ui, &mut eframe_frame),
    );
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
    let file = shot
        .out_dir
        .join(format!("filter_{theme_name}_{stage}.png"));
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

/// Where the first text run containing `needle` landed, in points.
fn position(shapes: &[egui::Shape], needle: &str) -> Option<egui::Pos2> {
    bounds(shapes, needle).map(|rect| rect.center())
}

/// Where a text shape actually landed, in points.
///
/// The rect rather than the centre, because "is this indicator on the screen"
/// is a question about its EDGES. A badge clipped to "FIL" by a pane that ran
/// past the window still contributes its whole galley to the shape list and
/// still reads as `FILTERED` to a text assertion - which is how the clipped
/// badge shipped past a proof that already asserted the badge existed.
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

/// Where a text run that IS `wanted` landed, rather than one containing it.
///
/// The clear key is a single multiplication sign, and other runs on a full
/// frame contain that character - a supersampling label, a scale readout - so
/// the substring form would answer for whichever came first in the shape list.
fn exact_bounds(shapes: &[egui::Shape], wanted: &str) -> Option<egui::Rect> {
    fn walk(shape: &egui::Shape, wanted: &str) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Text(text) if text.galley.text().trim() == wanted => {
                Some(text.galley.rect.translate(text.pos.to_vec2()))
            }
            egui::Shape::Vec(nested) => nested.iter().find_map(|shape| walk(shape, wanted)),
            _ => None,
        }
    }
    shapes.iter().find_map(|shape| walk(shape, wanted))
}

fn exact_position(shapes: &[egui::Shape], wanted: &str) -> Option<egui::Pos2> {
    exact_bounds(shapes, wanted).map(|rect| rect.center())
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
        label: Some("gate filter target"),
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
        label: Some("gate filter readback"),
        size: u64::from(width_px) * u64::from(height_px) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let screen = ScreenDescriptor {
        size_in_pixels: [width_px, height_px],
        pixels_per_point: scale,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gate filter"),
    });
    let _extra = shot
        .renderer
        .update_buffers(device, queue, &mut encoder, clipped, &screen);
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gate filter pass"),
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

/// Drive a future to completion on this thread; wgpu's native backends resolve
/// adapter and device requests without needing a waker.
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
