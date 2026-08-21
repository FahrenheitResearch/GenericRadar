//! Open the real master settings window and photograph it.
//!
//! `cargo run --release -p settings --example settings_preview -- <out.png> [category]`
//!
//! This exists so the settings window can be LOOKED at without launching the
//! whole workstation: it opens the same source file the workstation compiles,
//! drawing the real catalog over a real store, rather than trusting that
//! compiling code draws a usable dialog. The
//! screenshot is taken through eframe's own viewport command a few frames
//! after startup (so layout has settled) and the process exits by itself.
//!
//! With a second argument it opens that category page (`vol3d`, `data`, ...)
//! by simulating what a click on the category list stores, so every page can
//! be photographed.

#[allow(dead_code)]
#[path = "../../workstation_app/src/settings_ui.rs"]
mod settings_ui;
// The Appearance page is declared by the theme module, because its options
// are derived from the theme catalog. Included here so this harness renders
// and checks the REAL settings window rather than one page short of it.
#[allow(dead_code)]
#[path = "../../workstation_app/src/theme.rs"]
mod theme;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

struct Preview {
    ui_state: settings_ui::SettingsUi,
    registry: settings::SettingsRegistry,
    store: settings::SettingsStore,
    color_tables: Arc<color_tables::ColorTableSet>,
    shot_path: PathBuf,
    frames: u32,
    requested: bool,
}

impl eframe::App for Preview {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        let context = &context;
        self.frames += 1;
        self.ui_state.open = true;
        let outcome = settings_ui::draw_settings_window(
            context,
            &mut self.ui_state,
            settings_ui::SettingsWindowInput {
                registry: &self.registry,
                store: &mut self.store,
                color_tables: Some(&mut self.color_tables),
                // The preview owns no colour table folder: the window is
                // photographed with the shipped palettes alone, so the shot
                // does not change with whatever is in a developer's folder.
                user_tables: None,
            },
        );
        if !outcome.changed.is_empty() || outcome.palette_changed {
            eprintln!(
                "changed: {:?} palette_changed: {}",
                outcome.changed, outcome.palette_changed
            );
        }
        // A few frames so fonts rasterise and the window settles.
        if self.frames == 8 && !self.requested {
            self.requested = true;
            context.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let image = context.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [width, height] = image.size;
            let mut rgba = Vec::with_capacity(width * height * 4);
            for pixel in &image.pixels {
                rgba.extend_from_slice(&pixel.to_srgba_unmultiplied());
            }
            image::save_buffer(
                &self.shot_path,
                &rgba,
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            )
            .expect("write screenshot");
            eprintln!("screenshot written to {}", self.shot_path.display());
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        context.request_repaint();
    }
}

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let shot_path = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "settings_preview.png".to_owned()),
    );
    let category = args.next();
    let store_dir = std::env::temp_dir().join(format!("settings-preview-{}", std::process::id()));
    let mut ui_state = settings_ui::SettingsUi::default();
    if let Some(category) = category {
        if let Some(term) = category.strip_prefix("search:") {
            ui_state.open_search(term);
        } else {
            ui_state.open_category(&category);
        }
    }
    let preview = Preview {
        ui_state,
        registry: settings_ui::full_registry(theme::settings::settings_category()),
        store: settings::SettingsStore::open(store_dir.join("settings.json")),
        color_tables: Arc::new(color_tables::ColorTableSet::default()),
        shot_path,
        frames: 0,
        requested: false,
    };
    eframe::run_native(
        "Settings preview",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 780.0]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(preview))),
    )
}
