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
//! Includes the theme by `#[path]` because the module is delivered ahead of
//! its one-line wiring into `main.rs`; once `mod theme;` lands, this include
//! can become `use` of the crate module.

#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;

use std::path::PathBuf;

use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::{egui, wgpu};
use theme::palette::Palette;
use theme::{Variant, bevel};

/// 896 × 640 points: wide enough for a real toolbar row, and 896·4·ppp bytes
/// per row is a multiple of wgpu's 256-byte readback alignment at 1× and 2×.
const WIDTH_POINTS: u32 = 896;
const HEIGHT_POINTS: u32 = 640;

fn main() {
    let windowed = std::env::args().any(|arg| arg == "--window");
    if windowed {
        run_window();
    } else {
        run_headless();
    }
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
            ui.strong("Radar Workstation");
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

fn run_headless() {
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

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut renderer = Renderer::new(device, format, RendererOptions::PREDICTABLE);
    for (id, delta) in &textures.set {
        renderer.update_texture(device, queue, *id, delta);
    }

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
    let extra = renderer.update_buffers(device, queue, &mut encoder, &clipped, &screen);
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
        renderer.render(&mut pass, &clipped, &screen);
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
    for id in &textures.free {
        renderer.free_texture(id);
    }

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
