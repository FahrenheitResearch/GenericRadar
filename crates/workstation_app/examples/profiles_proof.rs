//! Settings profiles, photographed and clicked on a real radar volume.
//!
//! ```text
//! cargo run --release -p workstation_app --example profiles_proof -- <level2-file> [out-dir]
//! ```
//!
//! What this proves that a unit test cannot:
//!
//! * the Profiles page is a page a person can use - it is rendered through the
//!   real egui pipeline on both founding benches and written out as PNG, and the
//!   photographs include the states that only appear when something is wrong
//!   (a profile this build cannot fully read, a file that is not a profile at
//!   all) and the state that matters most (the unsaved-changes question);
//! * a switch reaches LIVE state, not just the settings document. The real
//!   `WorkstationApp` is built on a real Level II volume, and the buttons that
//!   switch profiles are CLICKED - pointer events into the real `App::ui` -
//!   so the picture that comes back is the picture the profile asked for;
//! * switching away and back restores exactly. The frame photographed after
//!   coming back to a profile is compared, pixel for pixel, with the frame
//!   photographed before leaving it.
//!
//! The profiles it switches between are written by hand as plain JSON before
//! the run, which is a second thing worth proving: the file format is one a
//! person can write in a text editor, and a profile that mentions six settings
//! and stays silent about the rest is applied for the six and does not read as
//! "modified" for the rest.
//!
//! Two things here are deliberately NOT clicked: opening the settings window
//! and choosing its page. `WorkstationApp::settings_ui_mut` does that
//! directly, because steering a window title bar with synthetic pointer events
//! would make this proof about egui rather than about profiles. Everything
//! inside the page - Switch, the unsaved-changes answer, the checkbox on the
//! Map page that creates the unsaved change - is a real click at a position
//! read off the frame the application itself drew.
//!
//! The offscreen rasteriser below is a compact second copy of the one in
//! `examples/theme_gallery.rs`. It is not shared: a cargo example is one
//! compilation unit, a file in `examples/` that is not an example is not a
//! thing cargo has, and thirty lines of wgpu boilerplate is a smaller price
//! than a shared harness crate that exists for two callers.

// The whole application, exactly as `src/main.rs` compiles it. See
// `theme_gallery.rs` for why the directory `#[path]` and the re-export are
// both needed.
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

use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::{egui, wgpu};
use theme::Appearance;

/// 1408 points wide so the read-back needs no row padding (1408 · 4 = 22 · 256),
/// and tall enough for four panes plus the settings window over them.
const WIDTH_POINTS: f32 = 1408.0;
const HEIGHT_POINTS: f32 = 896.0;
/// The two benches this proof lights its stage with.
///
/// Two rather than all eight registered themes, deliberately. This proof is
/// about PROFILES - that a switch reaches live state, that coming back
/// restores the picture pixel for pixel, that a modified profile asks before
/// it is left. The theme is the lamp, not the subject: a third lamp would
/// double a slow, stateful run on a real volume and tell us nothing new about
/// switching. The eight-theme sweep of the settings window itself lives in
/// `examples/settings_depth_proof.rs`, and the full per-theme contact sheet in
/// `examples/theme_gallery.rs`.
const LIGHT: &str = "light";
const DARK: &str = "dark";
/// How long to wait for a real volume before giving up and saying so.
const LOAD_BUDGET: Duration = Duration::from_secs(120);
/// How long to let a switch settle before photographing it: a profile that
/// changes the display quality asks every visible pane for a new picture, and
/// those arrive from worker threads.
const SETTLE_BUDGET: Duration = Duration::from_secs(8);
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let volume = PathBuf::from(arguments.next().unwrap_or_else(|| {
        panic!(
            "this proof needs a real Level II volume: \n  cargo run --release -p workstation_app \
             --example profiles_proof -- <level2-file> [out-dir]\nIt is deliberately not \
             runnable without one - a profile switch photographed over an empty pane proves \
             nothing about whether the switch reached the picture."
        )
    }));
    assert!(volume.is_file(), "{} is not a file", volume.display());
    let out_dir = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/profiles_proof"));
    std::fs::create_dir_all(&out_dir).expect("create the output directory");

    let instance = wgpu::Instance::default();
    let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("a wgpu adapter; this proof needs a GPU");
    println!("adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("profiles proof device"),
        ..Default::default()
    }))
    .expect("wgpu device");

    let config_root = out_dir.join("config");
    let _ = std::fs::remove_dir_all(&config_root);
    std::fs::create_dir_all(&config_root).expect("create the capture's config root");
    settings::set_app_config_root(&config_root);
    assert_eq!(
        settings::app_config_root(),
        config_root,
        "the capture must not touch a real analyst's settings, profiles or colour tables"
    );
    seed_profiles(&settings::profiles_dir());

    let ctx = egui::Context::default();
    theme::apply(&ctx, &Appearance::by_id(LIGHT));
    let renderer = Renderer::new(&device, TARGET_FORMAT, RendererOptions::PREDICTABLE);
    let store = settings::SettingsStore::open(config_root.join("settings.json"));
    let creation = eframe::CreationContext::_new_kittest(ctx.clone());
    let app = app::WorkstationApp::new(
        &creation,
        Some(volume.clone()),
        None,
        data_source::warnings::WarningsSource::default(),
        store,
    );
    let mut harness = Harness {
        ctx,
        device,
        queue,
        renderer,
        app,
        out_dir: out_dir.clone(),
        frame: eframe::Frame::_new_kittest(),
    };

    println!("\n=== loading {} ===", volume.display());
    assert!(
        harness.pump_until_a_real_volume(),
        "no volume reached the panes within {LOAD_BUDGET:?}: every photograph below would be of \
         an empty window, which is not evidence about anything"
    );
    harness.settle_until_quiet();

    // --- the page itself, on both benches ----------------------------------
    harness.open_profiles_page();
    for bench in [LIGHT, DARK] {
        harness.set_theme(bench);
        harness.capture(&format!("01-profiles-page-{bench}.png"));
    }
    harness.set_theme(LIGHT);
    let page = harness.whole_page();
    for wanted in [
        "Profiles",
        "As Shipped",
        "Chase",
        "Presentation",
        "Field Test",
        "Switch to it",
    ] {
        assert!(
            page.iter().any(|text| text.starts_with(wanted)),
            "the Profiles page drew no {wanted:?}; it drew: {page:?}"
        );
    }
    // The profile from a newer build is listed with what this build cannot use
    // rather than being refused, and the file that is not a profile at all is
    // named rather than swallowed.
    assert!(
        page.iter()
            .any(|text| text.starts_with("this build cannot use:")),
        "the page must say what it could not read in the newer build's profile"
    );
    assert!(
        page.iter().any(|text| text.contains("torn.json")),
        "the page must name the file that is not a usable profile"
    );
    println!("the Profiles page lists three profiles, the shipped one, and one broken file");

    // --- switch to Chase, and photograph the application it makes ----------
    harness.switch_to("Chase");
    harness.assert_active("Chase");
    let chase_before = harness.capture_app("02-app-on-chase-light.png");
    harness.set_theme(DARK);
    harness.capture_app("02-app-on-chase-dark.png");
    harness.set_theme(LIGHT);

    // The one line a running application says about profiles.
    harness.photograph_file_menu("03-file-menu-on-chase-light.png", "Profile: Chase");

    // --- switch to Presentation: four panes, four products, ultra quality --
    harness.switch_to("Presentation");
    harness.assert_active("Presentation");
    let presentation = harness.capture_app("04-app-on-presentation-light.png");
    harness.set_theme(DARK);
    harness.capture_app("04-app-on-presentation-dark.png");
    harness.set_theme(LIGHT);
    let moved = difference_fraction(&chase_before, &presentation);
    assert!(
        moved > 0.15,
        "switching from a one-pane profile to a four-pane one changed only {:.2}% of the \
         picture - the switch did not reach live state",
        moved * 100.0
    );
    println!(
        "switching Chase -> Presentation redrew {:.1}% of the window",
        moved * 100.0
    );

    // --- change a setting: the profile must read as modified, by name ------
    // Through the settings window's own search, so the click lands on one
    // unambiguous row rather than on whatever the Map page's layout puts at a
    // guessed position.
    harness.app.settings_ui_mut().open_search("site markers");
    harness.settle(4);
    harness.click("Radar site markers");
    harness.open_profiles_page();
    let page = harness.whole_page();
    let modified = page
        .iter()
        .find(|text| text.starts_with("Active profile: Presentation - modified"))
        .cloned();
    assert!(
        modified.is_some(),
        "one setting changed after a switch and the page did not say so: {page:?}"
    );
    assert!(
        page.iter().any(|text| text.contains("Radar site markers")),
        "the page must NAME what differs, not just count it: {page:?}"
    );
    println!(
        "after one change the page says: {}",
        modified.expect("checked")
    );
    harness.photograph_file_menu(
        "05-file-menu-modified-light.png",
        "Profile: Presentation (modified)",
    );

    // --- switching away with unsaved changes asks first --------------------
    harness.open_profiles_page();
    let prompted = harness.request_switch("Chase");
    assert!(
        prompted,
        "a switch away from a modified profile must ask before it happens"
    );
    harness.assert_active("Presentation");
    harness.capture("06-unsaved-changes-light.png");
    harness.set_theme(DARK);
    harness.capture("06-unsaved-changes-dark.png");
    harness.set_theme(LIGHT);
    println!("the unsaved-changes question is on screen and the switch has not happened");

    // --- answer it, and land back on Chase exactly where we left it --------
    harness.click("Discard them and switch");
    harness.settle_until_quiet();
    harness.assert_active("Chase");
    let chase_after = harness.capture_app("07-app-back-on-chase-light.png");
    let drift = difference_fraction(&chase_before, &chase_after);
    println!(
        "back on Chase: {:.3}% of the window differs from the frame before leaving it",
        drift * 100.0
    );
    assert!(
        drift < 0.02,
        "coming back to a profile must restore the picture it was left in; {:.2}% of the \
         window differs (see 02-app-on-chase-light.png beside 07-app-back-on-chase-light.png)",
        drift * 100.0
    );

    println!(
        "\nPNGs in {}\nA picture nobody has looked at is not a sign-off. Look at them.",
        out_dir.display()
    );
}

// ---------------------------------------------------------------------------
// The profiles this run switches between, written the way a person would
// ---------------------------------------------------------------------------

/// Three profiles and one file that is not one, written as plain JSON.
///
/// Each mentions only what it cares about. That is the interesting case: a
/// profile that is silent about the colour tables, the cameras and forty other
/// settings must apply the ones it names and must NOT read as modified for the
/// ones it does not.
fn seed_profiles(directory: &Path) {
    std::fs::create_dir_all(directory).expect("create the profiles directory");
    write(
        directory.join("chase.json"),
        r#"{
  "profile_format": 1,
  "name": "Chase",
  "settings": {
    "version": 1,
    "values": {
      "radar": { "quality": "smooth", "legend": true },
      "map": { "site_markers": true, "range_rings": true, "basemap_style": "slate" }
    },
    "workspace": {
      "layout": "one",
      "active_pane": 0,
      "panes": [ { "product": "REF", "tilt_mode": "lowest" } ],
      "show_warnings": true
    }
  }
}
"#,
    );
    write(
        directory.join("presentation.json"),
        r#"{
  "profile_format": 1,
  "name": "Presentation",
  "settings": {
    "version": 1,
    "values": {
      "radar": { "quality": "ultra", "legend": true },
      "map": { "site_markers": false, "range_rings": false, "basemap_style": "slate" }
    },
    "workspace": {
      "layout": "four",
      "active_pane": 0,
      "panes": [
        { "product": "REF", "tilt_mode": "lowest" },
        { "product": "VEL", "tilt_mode": "lowest" },
        { "product": "ZDR", "tilt_mode": "lowest" },
        { "product": "RHO", "tilt_mode": "lowest" }
      ],
      "show_warnings": true
    }
  }
}
"#,
    );
    // A profile from a build that knows more than this one: a newer wrapper
    // format, a newer document format, a page this build does not have and a
    // setting id it does not declare inside a page it does.
    write(
        directory.join("field-test.json"),
        r#"{
  "profile_format": 7,
  "name": "Field Test",
  "holographic_preview": true,
  "settings": {
    "version": 9,
    "values": {
      "radar": { "quality": "high", "hologram_mode": true },
      "quantum_overlay": { "entanglement": 0.7, "spin": 2 }
    },
    "workspace": { "layout": "two-vertical" }
  }
}
"#,
    );
    write(
        directory.join("torn.json"),
        "{ \"name\": \"Interrupted mid-save\", \"settings\": {",
    );
}

fn write(path: PathBuf, text: &str) {
    std::fs::write(&path, text).unwrap_or_else(|error| {
        panic!("write {}: {error}", path.display());
    });
}

// ---------------------------------------------------------------------------
// Driving and photographing the real application
// ---------------------------------------------------------------------------

/// One text run the application drew, and where it landed.
struct TextRun {
    text: String,
    rect: egui::Rect,
}

struct Harness {
    ctx: egui::Context,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    app: app::WorkstationApp,
    out_dir: PathBuf,
    frame: eframe::Frame,
}

impl Harness {
    /// One pass of the real `App::ui`, with the texture deltas uploaded so the
    /// next capture samples a font atlas that exists.
    fn pass(&mut self, events: Vec<egui::Event>) -> egui::FullOutput {
        let screen = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH_POINTS, HEIGHT_POINTS),
        );
        let app = &mut self.app;
        let frame = &mut self.frame;
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                // A quarter-second a pass: egui drives its fades off
                // `predicted_dt`, and a menu photographed three passes after it
                // opened would otherwise still be part-way through one.
                predicted_dt: 0.25,
                ..Default::default()
            },
            |ui| <app::WorkstationApp as eframe::App>::ui(app, ui, frame),
        );
        for (id, image) in &output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, image);
        }
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        output.textures_delta = Default::default();
        output
    }

    fn settle(&mut self, passes: u32) {
        for _ in 0..passes {
            self.pass(Vec::new());
        }
    }

    /// Let worker threads deliver: a profile switch can change the display
    /// quality, the products in four panes and the colour tables at once, and
    /// every one of those is a render that arrives later.
    fn settle_until_quiet(&mut self) {
        let start = Instant::now();
        while start.elapsed() < SETTLE_BUDGET {
            self.settle(2);
            std::thread::sleep(Duration::from_millis(40));
        }
        self.settle(4);
    }

    /// Light the stage with a registered theme, by its stored id.
    ///
    /// The default axes on that theme: this changes the lamp only, so the
    /// density, accent, edges and UI scale every photograph below is taken at
    /// stay the shipped ones and two frames differ by colour alone.
    fn set_theme(&mut self, theme_id: &str) {
        let appearance = Appearance::by_id(theme_id);
        assert_eq!(
            appearance.theme.id, theme_id,
            "no registered theme is called {theme_id:?}; `Appearance::by_id` fell back to the              default and every photograph below would be mislabelled"
        );
        theme::apply(&self.ctx, &appearance);
        self.settle(3);
        println!(
            "  theme {theme_id}: ctx theme {:?}, panel fill {:?}",
            self.ctx.theme(),
            self.ctx.global_style().visuals.panel_fill
        );
    }

    fn runs(&mut self) -> Vec<TextRun> {
        let output = self.pass(Vec::new());
        text_runs(&output.shapes)
    }

    /// Open the settings window on the Profiles page, scrolled to the top.
    fn open_profiles_page(&mut self) {
        self.app.settings_ui_mut().open = true;
        self.app.settings_ui_mut().open_category("profiles");
        self.settle(4);
        self.scroll_to_top();
    }

    /// A point inside the settings page's scroll area, taken off the window
    /// the application actually drew rather than guessed at.
    fn page_point(&mut self) -> egui::Pos2 {
        let runs = self.runs();
        let search = runs
            .iter()
            .find(|run| run.text == "Search")
            .expect("the settings window draws a Search label")
            .rect;
        search.center() + egui::vec2(320.0, 220.0)
    }

    fn scroll(&mut self, lines: f32) {
        let position = self.page_point();
        self.pass(vec![
            egui::Event::PointerMoved(position),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                // egui's convention: a positive y moves the CONTENT down and
                // reveals what is above it, so a negative y is "scroll down
                // the page".
                delta: egui::vec2(0.0, lines),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            },
        ]);
        self.settle(2);
    }

    fn scroll_to_top(&mut self) {
        for _ in 0..12 {
            self.scroll(6.0);
        }
    }

    /// Every line of text the current page holds, top to bottom, gathered by
    /// scrolling through it. A settings page longer than its window is normal;
    /// a proof that could only see the first screenful would be testing the
    /// window height.
    fn whole_page(&mut self) -> Vec<String> {
        self.scroll_to_top();
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..14 {
            for run in self.runs() {
                if !seen.contains(&run.text) {
                    seen.push(run.text);
                }
            }
            self.scroll(-4.0);
        }
        self.scroll_to_top();
        seen
    }

    /// The centre of the first text run that is exactly `label`, scrolling the
    /// page to find it if it is not on screen.
    fn find(&mut self, label: &str) -> egui::Pos2 {
        for attempt in 0..14 {
            if let Some(run) = self.runs().into_iter().find(|run| run.text == label) {
                return run.rect.center();
            }
            if attempt == 0 {
                self.scroll_to_top();
            } else {
                self.scroll(-4.0);
            }
        }
        let drew = self
            .runs()
            .into_iter()
            .map(|run| run.text)
            .collect::<Vec<_>>();
        panic!(
            "the application drew no {label:?} to click, even after scrolling. It drew: {drew:?}"
        )
    }

    /// A real click: hover, press, release, settle - four passes, because a
    /// widget egui has not seen under the pointer before is not a widget it
    /// will report a click on.
    fn click_at(&mut self, position: egui::Pos2) {
        self.pass(vec![egui::Event::PointerMoved(position)]);
        self.pass(vec![
            egui::Event::PointerMoved(position),
            button(position, true),
        ]);
        self.pass(vec![button(position, false)]);
        self.pass(vec![egui::Event::PointerGone]);
        self.settle(2);
    }

    fn click(&mut self, label: &str) {
        let position = self.find(label);
        self.click_at(position);
    }

    /// Click a button on a named profile's row: the button of that label whose
    /// baseline is the row's. Rows are found by the name the page drew, which
    /// carries an "(active)" marker for the active one, and the page is
    /// scrolled until the row is on screen.
    fn click_row(&mut self, profile: &str, button_label: &str) {
        for attempt in 0..14 {
            let runs = self.runs();
            let row = runs
                .iter()
                .find(|run| run.text == profile || run.text.starts_with(&format!("{profile}  (")))
                .map(|run| run.rect);
            if let Some(row) = row
                && let Some(target) = runs
                    .iter()
                    .filter(|run| run.text == button_label)
                    .filter(|run| (run.rect.center().y - row.center().y).abs() < 14.0)
                    .filter(|run| run.rect.center().x > row.center().x)
                    .map(|run| run.rect.center())
                    .next()
            {
                self.click_at(target);
                return;
            }
            if attempt == 0 {
                self.scroll_to_top();
            } else {
                self.scroll(-4.0);
            }
        }
        panic!("no {button_label:?} button on a {profile:?} row, even after scrolling the page");
    }

    /// Ask the real page to switch, and report whether it asked a question
    /// instead of switching. Both answers are correct behaviour: a profile
    /// with unsaved changes must not go quietly.
    fn request_switch(&mut self, profile: &str) -> bool {
        self.click_row(profile, "Switch to it");
        self.settle(2);
        self.runs()
            .iter()
            .any(|run| run.text.starts_with("Switching to") || run.text.starts_with("Reapplying"))
    }

    /// Switch to a profile through the real page, answering the
    /// unsaved-changes question if it is asked, and wait for the picture the
    /// switch asked for.
    fn switch_to(&mut self, profile: &str) {
        self.open_profiles_page();
        if self.request_switch(profile) {
            self.click("Discard them and switch");
        }
        self.settle_until_quiet();
    }

    fn assert_active(&mut self, profile: &str) {
        self.scroll_to_top();
        let runs = self.runs();
        let line = format!("Active profile: {profile}");
        assert!(
            runs.iter().any(|run| run.text.starts_with(&line)),
            "the page does not say {line:?}; it says: {:?}",
            runs.iter()
                .filter(|run| run.text.starts_with("Active profile"))
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Photograph the application itself, with the settings window put away,
    /// and hand back the pixels so two of them can be compared. This is the
    /// picture the profile is supposed to have produced.
    fn capture_app(&mut self, file: &str) -> Vec<u8> {
        self.app.settings_ui_mut().open = false;
        self.settle(4);
        let pixels = self.capture(file);
        self.app.settings_ui_mut().open = true;
        self.settle(3);
        pixels
    }

    /// Open the File menu, photograph it, and check the line it carries about
    /// the active profile. The settings window is closed first so the menu is
    /// photographed over the application rather than over a dialog.
    fn photograph_file_menu(&mut self, file: &str, expected: &str) {
        self.app.settings_ui_mut().open = false;
        self.settle(3);
        self.click("File");
        self.settle(2);
        let runs = self.runs();
        assert!(
            runs.iter().any(|run| run.text == expected),
            "the File menu does not carry {expected:?}; it carries: {:?}",
            runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>()
        );
        assert!(
            runs.iter().any(|run| run.text == "Profiles…"),
            "the File menu has no way into the Profiles page"
        );
        self.capture(file);
        // Put the menu away and the window back.
        self.click("File");
        self.app.settings_ui_mut().open = true;
        self.settle(3);
    }

    /// Rasterise the current frame offscreen and write it out. Returns the
    /// pixels, so two captures can be compared.
    fn capture(&mut self, file: &str) -> Vec<u8> {
        let output = self.pass(Vec::new());
        let scale = output.pixels_per_point;
        let clipped = self.ctx.tessellate(output.shapes, scale);
        assert!(
            !clipped.is_empty(),
            "the application tessellated nothing; {file} would be a lie"
        );
        let width = (WIDTH_POINTS * scale) as u32;
        let height = (HEIGHT_POINTS * scale) as u32;
        let pixels = self.render(&clipped, width, height, scale);
        let path = self.out_dir.join(file);
        image::RgbaImage::from_raw(width, height, pixels.clone())
            .expect("readback size matches the target")
            .save(&path)
            .expect("write PNG");
        println!("wrote {}", path.display());
        pixels
    }

    fn render(
        &mut self,
        clipped: &[egui::ClippedPrimitive],
        width: u32,
        height: u32,
        scale: f32,
    ) -> Vec<u8> {
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("profiles proof target"),
            size: wgpu::Extent3d {
                width,
                height,
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
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("profiles proof readback"),
            size: u64::from(width) * u64::from(height) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let screen = ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point: scale,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("profiles proof"),
            });
        let extra =
            self.renderer
                .update_buffers(&self.device, &self.queue, &mut encoder, clipped, &screen);
        assert!(extra.is_empty(), "no paint callbacks are expected here");
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("profiles proof pass"),
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
            self.renderer.render(&mut pass, clipped, &screen);
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
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll");
        receiver.recv().expect("map callback").expect("map read");
        let pixels = slice.get_mapped_range().to_vec();
        readback.unmap();
        pixels
    }

    /// Drive the real application until a volume is on the panes.
    fn pump_until_a_real_volume(&mut self) -> bool {
        let start = Instant::now();
        let mut settle_left: Option<u32> = None;
        loop {
            let output = self.pass(Vec::new());
            match settle_left {
                Some(0) => return true,
                Some(remaining) => settle_left = Some(remaining - 1),
                None => {
                    if text_runs(&output.shapes)
                        .iter()
                        .any(|run| is_a_measured_elevation(&run.text))
                    {
                        println!("volume on the panes after {:?}", start.elapsed());
                        settle_left = Some(24);
                    } else if start.elapsed() > LOAD_BUDGET {
                        return false;
                    } else {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                }
            }
        }
    }
}

/// The tilt readout renders a measured elevation as `0.48°`, and its
/// placeholders (`No tilt`, `Unavailable`) as words.
fn is_a_measured_elevation(text: &str) -> bool {
    text.strip_suffix('°')
        .is_some_and(|number| number.parse::<f32>().is_ok())
}

fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

fn text_runs(shapes: &[eframe::epaint::ClippedShape]) -> Vec<TextRun> {
    let mut runs = Vec::new();
    for clipped in shapes {
        collect(&clipped.shape, clipped.clip_rect, &mut runs);
    }
    runs
}

fn collect(shape: &egui::Shape, clip: egui::Rect, runs: &mut Vec<TextRun>) {
    match shape {
        egui::Shape::Text(text) => {
            if text.galley.text().trim().is_empty() {
                return;
            }
            runs.push(TextRun {
                text: text.galley.text().to_owned(),
                rect: text
                    .galley
                    .rect
                    .translate(text.pos.to_vec2())
                    .intersect(clip),
            });
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect(shape, clip, runs);
            }
        }
        _ => {}
    }
}

/// The fraction of pixels that differ between two frames of the same size.
///
/// Not a byte comparison: the age readout ticks over in wall-clock time and a
/// pane can be one anti-aliased pixel different after a re-render. What the
/// caller asserts on is the SIZE of the change - a four-pane layout against a
/// one-pane layout is a fifth of the window or more, and a profile that came
/// back to where it was is a fraction of a percent.
fn difference_fraction(before: &[u8], after: &[u8]) -> f64 {
    assert_eq!(before.len(), after.len(), "frames of different sizes");
    let differing = before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    differing as f64 / (before.len() / 4) as f64
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
