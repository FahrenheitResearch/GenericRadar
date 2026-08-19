//! The theme, photographed: a sample panel of every widget class the
//! workstation uses, rendered through the real egui → egui_wgpu pipeline in
//! both variants and written out as PNG for a human to look at.
//!
//! ```text
//! cargo run --release -p workstation_app --example theme_gallery
//! cargo run --release -p workstation_app --example theme_gallery -- --window
//! ```
//!
//! Headless (default): renders the gallery offscreen at 1× and 2× device
//! scale in both variants and writes four PNGs to `THEME_GALLERY_OUT`
//! (default `target/theme_gallery`). The 2× frames are what prove the
//! one-physical-pixel bevel promise: the lines must stay single crisp
//! hairlines, not grey smears. `--window` opens the same gallery live, with
//! variant toggles, for a human on a real display.
//!
//! # The bar, photographed
//!
//! The headless run also photographs the application's OWN menu bar —
//! `app::WorkstationApp::toolbar`, the shipped function, not a sample of it —
//! in both variants, at 1× and 2×, in four states (nothing hovered, a button
//! under the pointer, a button held down, a menu dropped). It is built
//! exactly as `main.rs` builds it and opened on a REAL Level II volume, so
//! the tilt readout carries a measured elevation and the product, palette and
//! live controls carry the state a real volume puts there. Point it at one:
//!
//! ```text
//! cargo run --release -p workstation_app --example theme_gallery -- \
//!     --toolbar <level2-file>
//! ```
//!
//! Each frame is then audited against W3C, "Web Content Accessibility
//! Guidelines (WCAG) 2.2", 2023, SC 1.4.3: every text run the real bar
//! emitted — read out of the frame's own shape list, so nothing can be
//! forgotten — is measured against the ground its pixels actually landed on,
//! read back out of the rendered image. Anything below 4.5:1 fails the run.
//! A picture a human has not looked at is not a sign-off; this is the
//! pre-flight that makes looking worth a reviewer's time.

// The whole application, compiled into this example exactly as `src/main.rs`
// compiles it. The directory `#[path]` is what makes each module's own child
// files resolve (`vol3d/pane.rs`, `pane_canvas/chrome.rs`, ...); the
// re-export below is what makes the `crate::app_support` / `crate::theme`
// paths inside those files resolve here, so photographing the real toolbar
// costs the application not one line of harness-only code.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod app;
    pub mod app_support;
    pub mod hazards;
    pub mod legend;
    pub mod live_service;
    pub mod load_service;
    pub mod nearest_site;
    pub mod palettes;
    pub mod pane_canvas;
    pub mod popup;
    pub mod probe;
    pub mod product;
    pub mod product_availability;
    pub mod product_picker;
    pub mod render_service;
    pub mod settings_ui;
    pub mod sites_service;
    pub mod sweep;
    pub mod theme;
    pub mod vol3d;
    pub mod vrot;
    pub mod warnings_service;
    pub mod xsection;
}

pub(crate) use source::{
    app, app_support, hazards, legend, live_service, load_service, nearest_site, palettes,
    pane_canvas, popup, probe, product, product_availability, product_picker, render_service,
    settings_ui, sites_service, sweep, theme, vol3d, vrot, warnings_service, xsection,
};

use std::path::{Path, PathBuf};

use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::{egui, wgpu};
use theme::palette::Palette;
use theme::{Variant, bevel};

/// 896 × 640 points: wide enough for a real toolbar row, and 896·4·ppp bytes
/// per row is a multiple of wgpu's 256-byte readback alignment at 1× and 2×.
const WIDTH_POINTS: u32 = 896;
const HEIGHT_POINTS: u32 = 640;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.iter().any(|argument| argument == "--window") {
        run_window();
        return;
    }
    let toolbar_volume = arguments
        .iter()
        .position(|argument| argument == "--toolbar")
        .and_then(|index| arguments.get(index + 1))
        .map(PathBuf::from);
    run_headless(toolbar_volume.as_deref());
}

/// The mutable bits of the sample panel, so controls respond in `--window`.
struct GalleryState {
    variant: Variant,
    path_text: String,
    site_text: String,
    frame_index: f32,
    link_cameras: bool,
    show_warnings: bool,
    quality: usize,
    vol3d_open: bool,
}

impl Default for GalleryState {
    fn default() -> Self {
        Self {
            variant: Variant::Dark,
            path_text: String::new(),
            site_text: "KTLX".to_owned(),
            frame_index: 7.0,
            link_cameras: true,
            show_warnings: true,
            quality: 1,
            vol3d_open: true,
        }
    }
}

/// The sample panel: one of everything the workstation's chrome uses.
fn gallery_body(ui: &mut egui::Ui, state: &mut GalleryState) {
    let palette = Palette::detect(ui);

    // Toolbar strip: the composition helpers.
    bevel::raised_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong("GenericRadar");
            bevel::etched_separator(ui);
            if bevel::toolbar_button(ui, "Load").clicked() {
                state.path_text.clear();
            }
            let _ = bevel::toolbar_button(ui, "Start live");
            if bevel::toolbar_toggle(ui, state.show_warnings, "Warnings · 3").clicked() {
                state.show_warnings = !state.show_warnings;
            }
            if bevel::toolbar_toggle(ui, state.vol3d_open, "3D").clicked() {
                state.vol3d_open = !state.vol3d_open;
            }
            bevel::etched_separator(ui);
            ui.add(
                egui::TextEdit::singleline(&mut state.path_text)
                    .desired_width(220.0)
                    .hint_text("Level II file path"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut state.site_text)
                    .desired_width(56.0)
                    .char_limit(4)
                    .hint_text("KRTX"),
            );
        });
    });
    ui.add_space(6.0);

    // Stock widgets in group boxes.
    ui.horizontal_top(|ui| {
        bevel::group_box(ui, "Playback", |ui| {
            // Group boxes inherit the surrounding layout (here: horizontal),
            // so a stacked interior says so explicitly.
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let _ = ui.button("◀");
                    let _ = ui.button("Play");
                    let _ = ui.button("▶");
                    let _ = ui.add_enabled(false, egui::Button::new("Go live"));
                });
                ui.add(
                    egui::Slider::new(&mut state.frame_index, 0.0..=23.0)
                        .integer()
                        .text("frame"),
                );
                ui.checkbox(&mut state.link_cameras, "Link cameras");
            });
        });
        bevel::group_box(ui, "Display", |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    for (index, label) in ["Draft", "High", "Ultra"].iter().enumerate() {
                        if ui.radio(state.quality == index, *label).clicked() {
                            state.quality = index;
                        }
                    }
                });
                egui::ComboBox::from_id_salt("gallery-palette")
                    .selected_text("NWS Reflectivity")
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        let _ = ui.selectable_label(true, "NWS Reflectivity");
                        let _ = ui.selectable_label(false, "Spectrum HD");
                    });
                ui.horizontal(|ui| {
                    let _ = ui.selectable_label(true, "REF");
                    let _ = ui.selectable_label(false, "DVEL");
                    let _ = ui.hyperlink_to("colour tables", "https://example.invalid/");
                });
            });
        });
    });
    ui.add_space(6.0);

    // A progress bar squared by the call site: `ProgressBar` is the one
    // stock widget whose pill shape ignores the style's corner radius, so
    // the language asks its callers for `.corner_radius(CornerRadius::ZERO)`.
    ui.add(
        egui::ProgressBar::new(0.62)
            .desired_width(420.0)
            .corner_radius(egui::CornerRadius::ZERO)
            .text("volume 15/24 · 62%"),
    );
    ui.add_space(6.0);

    // Data well: monospace readouts on the well ground.
    bevel::sunken_well(ui, |ui| {
        ui.monospace("KTLX · REF (dBZ) · 0.5° · VCP 212 · 2026-06-09 05:51:04Z");
        ui.monospace("gate 0.25 km · az 214.2° · 58.5 dBZ · beam 1.02 km AGL");
        ui.horizontal(|ui| {
            ui.colored_label(ui.visuals().warn_fg_color, "MESO 214/38");
            ui.colored_label(ui.visuals().error_fg_color, "TVS 219/12");
            ui.label(egui::RichText::new("dealiased · region-based").weak());
        });
    });
    ui.add_space(6.0);

    // Status strip.
    bevel::raised_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("KTLX · 8/24 · Complete · 05:51:04Z").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new("35.333°N 97.278°W").weak());
            });
        });
    });

    // Prove the pane surround: the map itself stays MapChrome's business,
    // so here the well just stands in for where a pane would sit.
    let _ = palette;
}

/// The floating window, drawn through the context so both modes share it.
fn gallery_windows(ctx: &egui::Context, state: &mut GalleryState) {
    if !state.vol3d_open {
        return;
    }
    egui::Window::new("3D Volume")
        .default_pos([560.0, 380.0])
        .default_size([300.0, 180.0])
        .collapsible(true)
        .show(ctx, |ui| {
            ui.label("Opacity");
            let mut opacity = 0.62;
            ui.add(egui::Slider::new(&mut opacity, 0.0..=1.0));
            ui.separator();
            ui.horizontal(|ui| {
                let _ = ui.button("Reset view");
                let _ = ui.button("Snapshot");
            });
        });
}

/// Everything, drawn on a root `Ui` — the shared path for the headless
/// capture, and the body of the windowed app.
fn draw_gallery(ui: &mut egui::Ui, state: &mut GalleryState) {
    // A root ui has no background of its own.
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, ui.visuals().panel_fill);
    egui::Frame::NONE
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| gallery_body(ui, state));
    gallery_windows(&ui.ctx().clone(), state);
}

// ---------------------------------------------------------------------------
// Headless: render through egui_wgpu and write PNGs.
// ---------------------------------------------------------------------------

fn run_headless(toolbar_volume: Option<&Path>) {
    let out_dir = std::env::var_os("THEME_GALLERY_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/theme_gallery"));
    std::fs::create_dir_all(&out_dir).expect("create output directory");

    let instance = wgpu::Instance::default();
    let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("a wgpu adapter; this proof needs a GPU");
    println!("adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("theme gallery device"),
        ..Default::default()
    }))
    .expect("wgpu device");

    for (variant, name) in [(Variant::Light, "light"), (Variant::Dark, "dark")] {
        for scale in [1.0_f32, 2.0] {
            let pixels = render_frame(&device, &queue, variant, scale);
            let width = (WIDTH_POINTS as f32 * scale) as u32;
            let height = (HEIGHT_POINTS as f32 * scale) as u32;
            let file = out_dir.join(format!("gallery_{name}_{scale}x.png"));
            let image = image::RgbaImage::from_raw(width, height, pixels)
                .expect("readback size matches the target");
            image.save(&file).expect("write PNG");
            println!("wrote {}", file.display());
        }
    }

    match toolbar_volume {
        Some(volume) => toolbar::photograph(&device, &queue, &out_dir, volume),
        None => println!(
            "\nSKIPPED the real-toolbar proof: pass `--toolbar <level2-file>` to run it.\n\
             It is deliberately not runnable without one - a bar photographed with no \n\
             volume behind it shows placeholder text in every readout, which proves \n\
             nothing about the widths, the wells or the contrast an analyst will see."
        ),
    }
}

/// One full egui pass rendered offscreen; returns tightly packed RGBA.
fn render_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    variant: Variant,
    scale: f32,
) -> Vec<u8> {
    let width_px = (WIDTH_POINTS as f32 * scale) as u32;
    let height_px = (HEIGHT_POINTS as f32 * scale) as u32;

    let ctx = egui::Context::default();
    theme::apply(&ctx, variant);
    ctx.set_pixels_per_point(scale);
    let mut state = GalleryState {
        variant,
        ..GalleryState::default()
    };
    // Three passes: the first applies the scale, the rest settle sizes. The
    // font-atlas delta arrives with the FIRST pass's output, so texture
    // deltas must be accumulated across all of them — dropping the early
    // outputs would leave every mesh sampling a texture that was never
    // uploaded, and the frame would come back black.
    let mut textures = eframe::epaint::textures::TexturesDelta::default();
    let mut full_output = None;
    for _ in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(WIDTH_POINTS as f32, HEIGHT_POINTS as f32),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| draw_gallery(ui, &mut state));
        textures.append(std::mem::take(&mut output.textures_delta));
        full_output = Some(output);
    }
    let full_output = full_output.expect("at least one pass ran");
    assert_eq!(
        full_output.pixels_per_point, scale,
        "the requested device scale must be in force"
    );
    let clipped = ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
    assert!(
        !clipped.is_empty(),
        "the gallery tessellated nothing; the capture would be a lie"
    );

    let mut renderer = Renderer::new(device, TARGET_FORMAT, RendererOptions::PREDICTABLE);
    for (id, delta) in &textures.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    let pixels = render_clipped(
        device,
        queue,
        &mut renderer,
        &clipped,
        width_px,
        height_px,
        scale,
    );
    for id in &textures.free {
        renderer.free_texture(id);
    }
    pixels
}

/// The pixel format the offscreen target uses. Deliberately `Rgba8Unorm`
/// rather than `Rgba8UnormSrgb`: egui writes gamma-space bytes into it, so a
/// read-back byte triple IS the `Color32` egui asked for, which is what lets
/// the contrast audit compare measured pixels against declared colours
/// without inverting a transfer function first. `RendererOptions::PREDICTABLE`
/// turns dithering off for the same reason.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Rasterise an already-tessellated frame offscreen and read it back as
/// tightly packed RGBA. Split out of [`render_frame`] so the toolbar proof
/// renders through exactly this path rather than a second copy of it.
fn render_clipped(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    clipped: &[egui::ClippedPrimitive],
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> Vec<u8> {
    let format = TARGET_FORMAT;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("theme gallery target"),
        size: wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("theme gallery readback"),
        size: u64::from(width_px) * u64::from(height_px) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let screen = ScreenDescriptor {
        size_in_pixels: [width_px, height_px],
        pixels_per_point: scale,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("theme gallery"),
    });
    let extra = renderer.update_buffers(device, queue, &mut encoder, clipped, &screen);
    assert!(extra.is_empty(), "no paint callbacks in the gallery");
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("theme gallery pass"),
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
        renderer.render(&mut pass, clipped, &screen);
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

/// Drive a future to completion on this thread; wgpu's native backends
/// resolve adapter/device requests without needing a waker (same pattern as
/// `map_scene/tests/tile_render_proof.rs`).
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

// ---------------------------------------------------------------------------
// Windowed: the same gallery on a real display, with variant toggles.
// ---------------------------------------------------------------------------

struct GalleryApp {
    state: GalleryState,
}

impl eframe::App for GalleryApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, ui.visuals().panel_fill);
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (variant, label) in [
                        (Variant::Dark, "Night bench"),
                        (Variant::Light, "Daylight bench"),
                    ] {
                        if bevel::toolbar_toggle(ui, self.state.variant == variant, label).clicked()
                        {
                            self.state.variant = variant;
                            theme::apply(ui.ctx(), variant);
                        }
                    }
                });
                ui.add_space(4.0);
                gallery_body(ui, &mut self.state);
            });
        gallery_windows(&ui.ctx().clone(), &mut self.state);
    }
}

fn run_window() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WIDTH_POINTS as f32, HEIGHT_POINTS as f32 + 40.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Theme Gallery",
        options,
        Box::new(|creation_context| {
            let state = GalleryState::default();
            theme::apply(&creation_context.egui_ctx, state.variant);
            Ok(Box::new(GalleryApp { state }))
        }),
    )
    .expect("gallery window");
}

// ---------------------------------------------------------------------------
// The real toolbar: photographed on a real volume, and audited for contrast.
// ---------------------------------------------------------------------------

/// `app::WorkstationApp::toolbar`, rendered through the real pipeline and
/// measured.
///
/// Nothing here re-creates the bar. The application is constructed the way
/// `main.rs` constructs it, pumped through its own `eframe::App::ui` until a
/// real Level II volume has landed, and then asked to draw its toolbar onto a
/// root `Ui` grounded by `theme::paint_root_ground` — the same function the
/// application calls. A harness that rebuilt the bar out of theme primitives
/// would photograph the harness, and would keep passing after the real bar
/// broke.
mod toolbar {
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use eframe::egui::{self, Color32};
    use eframe::egui_wgpu::{Renderer, RendererOptions};
    use eframe::wgpu;

    use super::theme::palette::Palette;
    use super::theme::{self, Variant};
    use super::{TARGET_FORMAT, app, render_clipped};

    /// 1408 points wide: the shipped window opens at 1500 and the bar spans
    /// it. 1408 · 4 bytes is 22 · 256, so the read-back needs no row padding
    /// at 1× or 2×.
    const WIDTH_POINTS: f32 = 1408.0;
    /// The band, plus a strip of bare ground beneath it — the ground being
    /// half of what this proves.
    const HEIGHT_POINTS: f32 = 96.0;
    /// The window the application is pumped in while its volume decodes.
    const PUMP_POINTS: (f32, f32) = (1408.0, 880.0);
    /// How long to wait for a real volume before giving up and saying so.
    const LOAD_BUDGET: Duration = Duration::from_secs(90);
    /// WCAG 2.2 SC 1.4.3: 4.5:1 for text below 18 pt / 14 pt bold. Every run
    /// on this bar is body or button text at 12.5 pt, so every run is held to
    /// this floor — none of them qualify for the 3:1 large-text allowance.
    const TEXT_FLOOR: f64 = 4.5;

    /// Which pointer state a frame is photographed in. Flat-until-hover is a
    /// claim about three renderings of the same control, so all three are
    /// photographed; "no state may render as dark-on-dark" is a claim about
    /// all of them, so all three are audited.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Pointer {
        /// Pointer off the bar: every command flat on the band.
        Rest,
        /// Pointer on a command: it raises.
        Hover,
        /// Pointer held down on a command: it sinks.
        Press,
        /// A menu title clicked open: the title latches and its menu drops.
        Menu,
    }

    impl Pointer {
        const ALL: [Self; 4] = [Self::Rest, Self::Hover, Self::Press, Self::Menu];

        const fn name(self) -> &'static str {
            match self {
                Self::Rest => "rest",
                Self::Hover => "hover",
                Self::Press => "press",
                Self::Menu => "menu",
            }
        }

        /// Frame height in points. A dropped menu needs room to drop into.
        const fn height_points(self) -> f32 {
            match self {
                Self::Menu => 384.0,
                _ => HEIGHT_POINTS,
            }
        }
    }

    /// One text run the bar emitted, read out of the frame's own shape list.
    struct TextRun {
        text: String,
        /// Where the glyphs landed, in points.
        rect: egui::Rect,
        /// The colour the shape asked for, after any layer opacity.
        ink: Color32,
    }

    /// The whole proof: build, load, photograph, audit.
    pub fn photograph(device: &wgpu::Device, queue: &wgpu::Queue, out_dir: &Path, volume: &Path) {
        assert!(
            volume.is_file(),
            "--toolbar wants a real Level II file; {} is not one",
            volume.display()
        );
        println!("\n=== the real toolbar, on {} ===", volume.display());

        let ctx = egui::Context::default();
        theme::apply(&ctx, Variant::Light);
        let mut renderer = Renderer::new(device, TARGET_FORMAT, RendererOptions::PREDICTABLE);
        let mut app = build(&ctx, out_dir, volume);

        let loaded = pump_until_a_real_volume(&ctx, device, queue, &mut renderer, &mut app);
        assert!(
            loaded,
            "no measured elevation reached the bar within {LOAD_BUDGET:?}: the proof would be \
             photographing placeholder text, which is not evidence about anything"
        );

        let mut failures = Vec::new();
        for (variant, name) in [(Variant::Light, "light"), (Variant::Dark, "dark")] {
            theme::apply(&ctx, variant);
            let palette = Palette::of(variant);
            check_the_app_grounds_itself(
                &ctx,
                device,
                queue,
                &mut renderer,
                &mut app,
                variant,
                palette,
            );
            for scale in [1.0_f32, 2.0] {
                ctx.set_pixels_per_point(scale);
                // Targets are read off the bar itself rather than guessed:
                // whatever the layout does, the pointer lands on a real
                // command.
                let mut targets = Targets::default();
                for pointer in Pointer::ALL {
                    let (runs, pixels) = frame(
                        &ctx,
                        device,
                        queue,
                        &mut renderer,
                        &mut app,
                        scale,
                        pointer,
                        targets,
                    );
                    if pointer == Pointer::Rest {
                        targets = Targets {
                            hover: centre_of(&runs, "+ Tilt"),
                            press: centre_of(&runs, "− Tilt"),
                            menu: centre_of(&runs, "File"),
                        };
                    }
                    let width_px = (WIDTH_POINTS * scale) as u32;
                    let height_px = (pointer.height_points() * scale) as u32;
                    let file =
                        out_dir.join(format!("toolbar_{name}_{}_{scale}x.png", pointer.name()));
                    image::RgbaImage::from_raw(width_px, height_px, pixels.clone())
                        .expect("readback size matches the target")
                        .save(&file)
                        .expect("write PNG");
                    println!("\nwrote {}", file.display());

                    // The bare-ground check reads the bottom third of the
                    // frame, which is bare only while nothing is dropped into
                    // it; the three closed states prove the ground already.
                    if pointer != Pointer::Menu {
                        check_the_ground_is_painted(&pixels, width_px, height_px, palette);
                    }
                    failures.extend(audit(&runs, &pixels, width_px, height_px, scale, palette));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "text on the bar below the {TEXT_FLOOR}:1 floor:\n{}",
            failures.join("\n")
        );
        println!(
            "\nEvery text run on the real bar clears WCAG 2.2 SC 1.4.3 ({TEXT_FLOOR}:1) against \
             the ground its pixels actually landed on, in both variants, at 1x and 2x, at rest, \
             hovered and pressed.\nThe PNGs above are the pre-flight. A human has still not \
             looked at them; until one has, nothing here is signed off."
        );
    }

    /// The application, built exactly as `main.rs` builds it.
    fn build(ctx: &egui::Context, out_dir: &Path, volume: &Path) -> app::WorkstationApp {
        // A settings file of this run's own, removed first, so a leftover
        // from an earlier run cannot quietly change what the bar shows.
        let settings_file = out_dir.join("toolbar-proof-settings.json");
        let _ = std::fs::remove_file(&settings_file);
        let store = settings::SettingsStore::open(settings_file);
        let creation = eframe::CreationContext::_new_kittest(ctx.clone());
        app::WorkstationApp::new(
            &creation,
            Some(volume.to_path_buf()),
            None,
            data_source::warnings::WarningsSource::default(),
            store,
        )
    }

    /// Drive the real `eframe::App::ui` until the tilt readout carries a
    /// measured elevation, then a few passes more to let the rest settle.
    ///
    /// These passes are never rendered — their shapes are read for text and
    /// dropped — but their texture deltas are not: the font atlas arrives
    /// with the first of them, and a capture that skipped it would sample a
    /// texture nobody uploaded and come back black.
    fn pump_until_a_real_volume(
        ctx: &egui::Context,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        app: &mut app::WorkstationApp,
    ) -> bool {
        let mut eframe_frame = eframe::Frame::_new_kittest();
        let start = Instant::now();
        let mut settle_left = None;
        loop {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(PUMP_POINTS.0, PUMP_POINTS.1),
                    )),
                    ..Default::default()
                },
                |ui| <app::WorkstationApp as eframe::App>::ui(app, ui, &mut eframe_frame),
            );
            upload(device, queue, renderer, &mut output.textures_delta);
            match settle_left {
                Some(0) => return true,
                Some(remaining) => settle_left = Some(remaining - 1),
                None => {
                    if text_runs(&output.shapes)
                        .iter()
                        .any(|run| is_a_measured_elevation(&run.text))
                    {
                        println!("volume on the bar after {:?}", start.elapsed());
                        settle_left = Some(8);
                    } else if start.elapsed() > LOAD_BUDGET {
                        return false;
                    } else {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                }
            }
        }
    }

    /// `active_tilt_label` renders a measured elevation as `0.48°`, and the
    /// placeholders it renders instead (`No tilt`, `Unavailable`) as words.
    fn is_a_measured_elevation(text: &str) -> bool {
        text.strip_suffix('°')
            .is_some_and(|number| number.parse::<f32>().is_ok())
    }

    /// Where on the real bar each pointer state aims, read off the bar's own
    /// first frame rather than guessed from a layout that may have moved.
    #[derive(Clone, Copy, Default)]
    struct Targets {
        hover: egui::Pos2,
        press: egui::Pos2,
        menu: egui::Pos2,
    }

    /// One photographed frame of the real bar: settle, capture, rasterise,
    /// then put the pointer and the menu back the way they were found.
    #[expect(clippy::too_many_arguments, reason = "a capture is many knobs")]
    fn frame(
        ctx: &egui::Context,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        app: &mut app::WorkstationApp,
        scale: f32,
        pointer: Pointer,
        targets: Targets,
    ) -> (Vec<TextRun>, Vec<u8>) {
        let height_points = pointer.height_points();
        let screen_rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH_POINTS, height_points),
        );
        let bar = |ctx: &egui::Context, app: &mut app::WorkstationApp, events: Vec<egui::Event>| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen_rect),
                    events,
                    // A quarter-second per pass. egui drives every fade off
                    // `predicted_dt`, and at the default sixtieth of a second
                    // a menu photographed three passes after it opened is
                    // still part-way through its fade — the frame would be a
                    // picture of an animation, not of the bar.
                    predicted_dt: 0.25,
                    ..Default::default()
                },
                |ui| {
                    // NOTE: this ground is THIS HARNESS's, not the
                    // application's - it is here so the bar is photographed
                    // on the face an analyst will see it on, and it is not
                    // evidence that `WorkstationApp::ui` paints one. That
                    // claim is `check_the_app_grounds_itself`'s, which drives
                    // the real `App::ui`; delete the call site in `app.rs` and
                    // every PNG below is byte-identical.
                    theme::paint_root_ground(ui);
                    app.toolbar(ui);
                },
            )
        };
        // Three passes: the first carries the scale or theme change and the
        // pointer events, the rest settle the widths the layout reads from
        // the previous pass — and, for a menu, let the popup appear.
        let mut output = None;
        for pass in 0..3 {
            let events = if pass == 0 {
                pointer_events(pointer, targets)
            } else {
                Vec::new()
            };
            let mut this = bar(ctx, app, events);
            upload(device, queue, renderer, &mut this.textures_delta);
            output = Some(this);
        }
        let output = output.expect("at least one pass ran");
        assert_eq!(
            output.pixels_per_point, scale,
            "the requested device scale must be in force"
        );
        let runs = text_runs(&output.shapes);
        assert!(
            !runs.is_empty(),
            "the real toolbar drew no text at all; the capture would be a lie"
        );
        let clipped = ctx.tessellate(output.shapes, output.pixels_per_point);
        let pixels = render_clipped(
            device,
            queue,
            renderer,
            &clipped,
            (WIDTH_POINTS * scale) as u32,
            (height_points * scale) as u32,
            scale,
        );
        for events in unwind(pointer, targets, height_points) {
            let mut this = bar(ctx, app, events);
            upload(device, queue, renderer, &mut this.textures_delta);
        }
        (runs, pixels)
    }

    fn pointer_events(pointer: Pointer, targets: Targets) -> Vec<egui::Event> {
        match pointer {
            Pointer::Rest => vec![egui::Event::PointerGone],
            Pointer::Hover => vec![egui::Event::PointerMoved(targets.hover)],
            Pointer::Press => vec![
                egui::Event::PointerMoved(targets.press),
                button(targets.press, true),
            ],
            Pointer::Menu => vec![
                egui::Event::PointerMoved(targets.menu),
                button(targets.menu, true),
                button(targets.menu, false),
            ],
        }
    }

    /// Put the application back where this capture found it, so the next one
    /// photographs the bar and not this one's leftovers.
    fn unwind(pointer: Pointer, targets: Targets, height_points: f32) -> Vec<Vec<egui::Event>> {
        match pointer {
            // Release well off the bar. A release ON the held button would
            // count as a click and step the tilt that the next photograph is
            // supposed to show unchanged.
            Pointer::Press => {
                let away = egui::pos2(4.0, height_points - 4.0);
                vec![vec![
                    egui::Event::PointerMoved(away),
                    button(away, false),
                    egui::Event::PointerGone,
                ]]
            }
            // Click the title again to put the menu away, then a pass for the
            // popup to actually close.
            Pointer::Menu => vec![
                vec![button(targets.menu, true), button(targets.menu, false)],
                vec![egui::Event::PointerGone],
            ],
            _ => Vec::new(),
        }
    }

    fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        delta: &mut eframe::epaint::textures::TexturesDelta,
    ) {
        for (id, image) in &delta.set {
            renderer.update_texture(device, queue, *id, image);
        }
        for id in &delta.free {
            renderer.free_texture(id);
        }
        *delta = eframe::epaint::textures::TexturesDelta::default();
    }

    /// The centre of the run whose text is `wanted`, for parking a pointer.
    fn centre_of(runs: &[TextRun], wanted: &str) -> egui::Pos2 {
        runs.iter()
            .find(|run| run.text == wanted)
            .unwrap_or_else(|| panic!("the real bar drew no {wanted:?} control to point at"))
            .rect
            .center()
    }

    // -- reading the frame ---------------------------------------------------

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
                // A galley of nothing but whitespace has no ink to judge -
                // an empty text edit still emits one for its cursor row.
                if text.galley.text().trim().is_empty() {
                    return;
                }
                // `Painter::galley` leaves the section colours as
                // `PLACEHOLDER` and carries the real colour in
                // `fallback_color`; a `RichText` that named a colour carries
                // it in the section instead. `override_text_color` beats both.
                let declared = text.override_text_color.unwrap_or_else(|| {
                    text.galley
                        .job
                        .sections
                        .first()
                        .map(|section| section.format.color)
                        .filter(|color| *color != Color32::PLACEHOLDER)
                        .unwrap_or(text.fallback_color)
                });
                // Clipped, not raw: a text edit narrower than its text emits
                // a galley the full width of the string, and measuring the
                // ground over that rect would read the panel the overflow is
                // scissored against instead of the well the glyphs sit in.
                let rect = text
                    .galley
                    .rect
                    .translate(text.pos.to_vec2())
                    .intersect(clip);
                runs.push(TextRun {
                    text: text.galley.text().to_owned(),
                    rect,
                    ink: declared.gamma_multiply(text.opacity_factor),
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

    // -- the audit -----------------------------------------------------------

    /// WCAG 2.2 relative luminance of an sRGB colour (SC 1.4.3's definition,
    /// which follows IEC 61966-2-1 for the transfer function).
    fn relative_luminance(color: Color32) -> f64 {
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

    /// WCAG 2.2 contrast ratio, 1.0 ..= 21.0.
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Composite a possibly-translucent ink onto its ground, because what an
    /// eye judges is the pixel that results, not the colour that was asked
    /// for.
    fn over(ink: Color32, ground: Color32) -> Color32 {
        let alpha = f32::from(ink.a()) / 255.0;
        let mix = |ink: u8, ground: u8| {
            (f32::from(ink) * alpha + f32::from(ground) * (1.0 - alpha)).round() as u8
        };
        Color32::from_rgb(
            mix(ink.r(), ground.r()),
            mix(ink.g(), ground.g()),
            mix(ink.b(), ground.b()),
        )
    }

    /// Every colour the theme paints as a *ground*: the fills a text run can
    /// legitimately land on. Bevel and border roles are deliberately absent —
    /// they are one-pixel structure, held to SC 1.4.11's 3:1 by
    /// `tests/theme_contract.rs`, and a hairline crossing a glyph's bounding
    /// box is not the glyph's background.
    fn grounds(palette: &Palette) -> [Color32; 7] {
        [
            palette.face,
            palette.face_raised,
            palette.face_pressed,
            palette.hover,
            palette.well,
            palette.selection_tint,
            palette.selection_bg,
        ]
    }

    struct Measured {
        /// The modal theme ground under this run's glyphs, from the pixels.
        ground: Color32,
        /// The rendered pixel furthest from that ground: at any scale where
        /// a stem covers a whole pixel this IS the ink, so it is printed as
        /// corroboration that the declared colour is the painted one.
        extreme: Color32,
        pixels: usize,
    }

    /// Audit one frame. Returns a line per text run that misses the floor.
    fn audit(
        runs: &[TextRun],
        pixels: &[u8],
        width_px: u32,
        height_px: u32,
        scale: f32,
        palette: &Palette,
    ) -> Vec<String> {
        let grounds = grounds(palette);
        let mut rows = Vec::new();
        let mut unread = Vec::new();
        for run in runs {
            match measure(run, pixels, width_px, height_px, scale, &grounds) {
                Some(measured) => {
                    let ink = over(run.ink, measured.ground);
                    rows.push((contrast(ink, measured.ground), ink, measured, run));
                }
                // A run whose ground is no colour this theme paints is a run
                // this audit did not judge, and silence about it would be the
                // whole point of the audit thrown away.
                None => unread.push(run.text.clone()),
            }
        }
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        println!(
            "  {:<34} {:>7}  {:<9} {:<9} {:<9} {:>6}",
            "text run", "ratio", "ink", "ground", "brightest", "px"
        );
        let mut failures = Vec::new();
        for (ratio, ink, measured, run) in &rows {
            let text = if run.text.chars().count() > 32 {
                format!("{}…", run.text.chars().take(31).collect::<String>())
            } else {
                run.text.clone()
            };
            let mark = if *ratio >= TEXT_FLOOR { ' ' } else { '!' };
            println!(
                "{mark} {text:<34} {ratio:>7.2}  {:<9} {:<9} {:<9} {:>6}",
                hex(*ink),
                hex(measured.ground),
                hex(measured.extreme),
                measured.pixels
            );
            if *ratio < TEXT_FLOOR {
                failures.push(format!(
                    "  {ratio:.2}:1 - {:?} in {} on {}",
                    run.text,
                    hex(*ink),
                    hex(measured.ground)
                ));
            }
        }
        for text in unread {
            println!(
                "? {text:<34} {:>7}  landed on no colour this theme paints",
                "-"
            );
            failures.push(format!("  unaudited - {text:?} sits on an unknown ground"));
        }
        failures
    }

    /// Read a run's actual ground and its brightest/darkest rendered pixel
    /// out of the frame.
    fn measure(
        run: &TextRun,
        pixels: &[u8],
        width_px: u32,
        height_px: u32,
        scale: f32,
        grounds: &[Color32],
    ) -> Option<Measured> {
        let clamp = |value: f32, limit: u32| (value.max(0.0) as u32).min(limit);
        let min_x = clamp((run.rect.min.x * scale).floor(), width_px);
        let min_y = clamp((run.rect.min.y * scale).floor(), height_px);
        let max_x = clamp((run.rect.max.x * scale).ceil(), width_px);
        let max_y = clamp((run.rect.max.y * scale).ceil(), height_px);
        if min_x >= max_x || min_y >= max_y {
            return None;
        }
        let mut counts: HashMap<[u8; 3], usize> = HashMap::new();
        for y in min_y..max_y {
            for x in min_x..max_x {
                let index = ((y * width_px + x) * 4) as usize;
                let rgb = [pixels[index], pixels[index + 1], pixels[index + 2]];
                *counts.entry(rgb).or_default() += 1;
            }
        }
        let total = counts.values().sum::<usize>();
        // The ground is the theme ground that covers most of the run's box.
        // Text is a minority of the pixels in its own bounding box - that is
        // what makes this a measurement rather than an assumption.
        let ground = counts
            .iter()
            .filter(|(rgb, _)| grounds.contains(&Color32::from_rgb(rgb[0], rgb[1], rgb[2])))
            .max_by_key(|(_, count)| **count)
            .map(|(rgb, _)| Color32::from_rgb(rgb[0], rgb[1], rgb[2]))?;
        let ground_luminance = relative_luminance(ground);
        // Ignore lone pixels: a single stray sample is noise, a glyph stem is
        // not.
        let extreme = counts
            .iter()
            .filter(|(_, count)| **count >= 2)
            .map(|(rgb, _)| Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
            .max_by(|a, b| {
                (relative_luminance(*a) - ground_luminance)
                    .abs()
                    .total_cmp(&(relative_luminance(*b) - ground_luminance).abs())
            })
            .unwrap_or(ground);
        Some(Measured {
            ground,
            extreme,
            pixels: total,
        })
    }

    fn hex(color: Color32) -> String {
        format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
    }

    // -- the ground ----------------------------------------------------------

    /// What `paint_root_ground` actually covers: the part of the frame the
    /// bar does NOT reach must be the panel face, edge to edge, not eframe's
    /// near-black default.
    ///
    /// This pins the FILL, not the caller — the ground in these frames is
    /// painted by `frame` above, so a `paint_root_ground` that covered only
    /// the widgets' own rects would fail here, while deleting the call in
    /// `WorkstationApp::ui` would not. `check_the_app_grounds_itself` is the
    /// one that watches the caller.
    ///
    /// Measured over the bottom third — well clear of the band, which is
    /// about 40 points tall — so this reads the ground itself rather than the
    /// face the band paints inside its own frame. Every pixel, not a corner
    /// sample: a ground painted only where a widget happens to sit is the bug.
    fn check_the_ground_is_painted(
        pixels: &[u8],
        width_px: u32,
        height_px: u32,
        palette: &Palette,
    ) {
        let first_row = height_px * 2 / 3;
        for y in first_row..height_px {
            for x in 0..width_px {
                let index = ((y * width_px + x) * 4) as usize;
                let found = Color32::from_rgb(pixels[index], pixels[index + 1], pixels[index + 2]);
                assert_eq!(
                    found,
                    palette.face,
                    "the ground at ({x}, {y}) is {} - the root Ui is unpainted again",
                    hex(found)
                );
            }
        }
    }

    /// The half of the ground the photographs below CANNOT carry, driven
    /// through the application itself.
    ///
    /// `frame` calls `theme::paint_root_ground` and then `app.toolbar`, so
    /// every PNG in this directory is a picture of the bar on a ground this
    /// harness painted. That is the right frame to judge the bar's contrast
    /// on, and it is worthless as evidence about whether the APPLICATION
    /// still paints a ground: deleting `paint_root_ground(ui)` from
    /// `WorkstationApp::ui` and the `clear_color` override from its
    /// `impl eframe::App` left all sixteen PNGs byte-identical, the bare-
    /// ground assertion passing, and all sixteen theme contract tests green.
    /// That deletion is the field failure itself — it is how the ground was
    /// lost the first time, when the per-frame `panel_fill` override was
    /// removed as the theme landed.
    ///
    /// So this drives the real `<WorkstationApp as eframe::App>::ui` over a
    /// full window and reads its own shape list, and asks the real
    /// `App::clear_color` the way `eframe::native::wgpu_integration` asks:
    /// through `Context::global_style`.
    ///
    /// (`tests/theme_contract.rs` cannot do this either — it
    /// `#[path]`-includes `src/theme.rs` and nothing else, so it can only see
    /// the helpers, never their callers. The gate-level twin of this check
    /// lives in `app.rs`'s own test module.)
    fn check_the_app_grounds_itself(
        ctx: &egui::Context,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        app: &mut app::WorkstationApp,
        variant: Variant,
        palette: &Palette,
    ) {
        let screen = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(PUMP_POINTS.0, PUMP_POINTS.1),
        );
        let mut eframe_frame = eframe::Frame::_new_kittest();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| <app::WorkstationApp as eframe::App>::ui(app, ui, &mut eframe_frame),
        );
        // Whatever this pass grew the atlas by belongs to the renderer the
        // photographs use; dropping it would leave them sampling a texture
        // nobody uploaded.
        upload(device, queue, renderer, &mut output.textures_delta);

        // Index rather than identity: the property that makes the chrome
        // legible is "a face rect covers the viewport AND is painted before
        // the first glyph", and stating it that way survives egui gaining an
        // incidental leading shape in a later version.
        let ground = output
            .shapes
            .iter()
            .position(|clipped| {
                matches!(&clipped.shape, egui::Shape::Rect(rect)
                    if rect.fill == palette.face && rect.rect.contains_rect(screen))
            })
            .unwrap_or_else(|| {
                panic!(
                    "{variant:?}: the real `App::ui` painted no {} rect over the whole viewport \
                     - the application is back on eframe's near-black ground and every bare \
                     label on the bar is invisible again",
                    hex(palette.face)
                )
            });
        if let Some(first_text) = output
            .shapes
            .iter()
            .position(|clipped| matches!(&clipped.shape, egui::Shape::Text(_)))
        {
            assert!(
                ground < first_text,
                "{variant:?}: the ground is painted at shape {ground}, after the first text run \
                 at {first_text} - it would cover the chrome instead of backing it"
            );
        }

        let clear =
            <app::WorkstationApp as eframe::App>::clear_color(app, &ctx.global_style().visuals);
        assert_eq!(
            clear,
            palette.face.to_opaque().to_normalized_gamma_f32(),
            "{variant:?}: App::clear_color would tear a seam against the painted ground"
        );
        assert!(
            (clear[3] - 1.0).abs() < 1e-6,
            "{variant:?}: a translucent clear colour lets the desktop through"
        );
        println!(
            "\nthe real App::ui paints {} over the whole viewport (shape {ground}) and \
             App::clear_color returns it - {variant:?}",
            hex(palette.face)
        );
    }
}
