//! Photograph one real radar pane, with the annotation and unit settings
//! turned to a named preset, and look at what changed.
//!
//! ```text
//! cargo run --release -p workstation_app --example pane_proof -- \
//!     <level2-file> <out.png> [preset] --window
//! ```
//!
//! `--window` is not optional, and it is not decoration. This photographs
//! through a real `eframe` viewport, and `eframe` maps that window onto the
//! display as soon as the first frame is painted - so running this puts a
//! real, focused window on whatever screen it is started on, and nothing in
//! this workspace can prevent that. Without the flag the harness refuses to
//! start rather than taking over a display somebody else is using. See
//! `../src/harness_window.rs` for why the flag is the whole of the remedy.
//!
//! This exists because "the setting reaches the picture" is a claim about
//! pixels. The unit tests read the shapes `draw_pane` emitted and assert on
//! them, which catches a wrong number; only a photograph catches a ring label
//! sitting on top of a county name, or a marker grown until the identifiers
//! collide. So this drives the SHIPPED `pane_canvas::draw_pane` - not a sample
//! of it - over a real Level II volume, with the real basemap under it and
//! real neighbouring WSR-88D sites on it, and writes a PNG.
//!
//! The presets, and what each one is asking:
//!
//! * `default` - every setting at its shipped value. This frame is the
//!   control: it must be the pane the application drew before this settings
//!   work existed.
//! * `miles` - statute miles and feet, with the ring labels on. The rings do
//!   not move; the numbers beside them do.
//! * `rings` - a 50-unit ladder, eight rings, labelled. A different ladder
//!   entirely.
//! * `sparse` - the corner readout off, big markers, big labels. What an
//!   analyst working on a projector asks for.

// The one place a harness decides whether it may take over a display. Held
// apart from the `source` block below because it is policy for the harness,
// not application code the harness is photographing.
#[allow(dead_code)]
#[path = "../src/harness_window.rs"]
mod harness_window;

// The application, compiled exactly as `src/main.rs` compiles it. The
// directory `#[path]` is what makes each module's own child files resolve, and
// the re-export is what makes the `crate::` paths inside them resolve here.
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

use std::path::PathBuf;
use std::sync::Arc;

use analyst_runtime::{Camera2D, Generation, GeometryCacheKey, LodBucket, PaneId, ViewportMetrics};
use eframe::egui;
use radar_core::MomentType;

use annotation::{Annotation, CornerReadout, RingLadder};
use pane_canvas::{PaneMap, PaneOverlay, PaneTexture, PlacedSite, SiteLabelMode, draw_pane};
use units::{AltitudeUnit, DistanceUnit, UnitSystem};

/// Pane size in points. Square, because the range rings are circles and a
/// square frame shows the whole ladder without one axis clipping first.
const PANE_POINTS: f32 = 900.0;

/// Real WSR-88D sites around KDVN (Davenport, Iowa), so the marker and label
/// settings are photographed against a real cluster rather than a made-up one.
/// Latitudes and longitudes are the sites' published positions.
const NEIGHBOURS: &[(&str, f64, f64)] = &[
    ("KDVN", 41.611_667, -90.580_833),
    ("KDMX", 41.731_111, -93.722_778),
    ("KARX", 43.822_778, -91.191_111),
    ("KILX", 40.150_556, -89.336_944),
    ("KLOT", 41.604_444, -88.084_722),
    ("KMPX", 44.848_889, -93.565_556),
    ("KEAX", 38.810_278, -94.264_444),
];

/// The line the refusal tells an operator to type, and the line the usage
/// error prints. One string, so they cannot drift apart.
const USAGE: &str = "pane_proof <level2-file> <out.png> [default|miles|rings|sparse] --window";

fn main() -> eframe::Result {
    // Before anything is decoded: this harness cannot photograph without
    // putting a window on the display, so it does not start unless the
    // operator asked for one.
    harness_window::require_window_or_exit("pane_proof", USAGE);

    let mut arguments = harness_window::positional_arguments().into_iter();
    let Some(volume_path) = arguments.next().map(PathBuf::from) else {
        eprintln!("usage: {USAGE}");
        std::process::exit(2);
    };
    let shot_path = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("pane_proof.png"));
    let preset = arguments.next().unwrap_or_else(|| "default".to_owned());
    let (annotation, unit_system) = preset_for(&preset);

    let volume = nexrad_io::decode_volume_from_path(&volume_path)
        .unwrap_or_else(|error| panic!("could not decode {}: {error}", volume_path.display()));
    println!(
        "volume  {} {}  ({} cuts)",
        volume.site.id,
        volume.volume_time.to_rfc3339(),
        volume.cuts.len()
    );
    println!("preset  {preset}");

    let camera = Camera2D::default();
    let viewport = ViewportMetrics {
        width_points: PANE_POINTS,
        height_points: PANE_POINTS,
        pixels_per_point: 1.0,
    };
    let raster = raster_lowest_reflectivity(&volume, camera, viewport);
    let map = pane_map(&volume, annotation, unit_system);

    let proof = Proof {
        raster,
        texture: None,
        map,
        camera,
        viewport,
        title: format!("1 - REF (dBZ)  ·  {} ", volume.site.id),
        status: unit_system.time(volume.volume_time),
        shot_path,
        frames: 0,
        requested: false,
    };
    eframe::run_native(
        "Pane proof",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([PANE_POINTS, PANE_POINTS]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(proof))),
    )
}

/// The named settings presets. `default` is deliberately
/// `Default::default()` on both structs and nothing else, so the control
/// frame cannot accidentally carry a tweak.
fn preset_for(name: &str) -> (Annotation, UnitSystem) {
    match name {
        "miles" => (
            Annotation {
                ring_labels: true,
                ..Annotation::default()
            },
            UnitSystem {
                distance: DistanceUnit::StatuteMiles,
                altitude: AltitudeUnit::Feet,
                ..UnitSystem::default()
            },
        ),
        "rings" => (
            Annotation {
                ring_ladder: RingLadder::Every50,
                ring_count: 8,
                ring_labels: true,
                ..Annotation::default()
            },
            UnitSystem::default(),
        ),
        "sparse" => (
            Annotation {
                corner_readout: CornerReadout::Off,
                site_marker_points: 18.0,
                site_label_points: 16.0,
                ring_count: 2,
                ..Annotation::default()
            },
            UnitSystem::default(),
        ),
        _ => (Annotation::default(), UnitSystem::default()),
    }
}

/// The lowest reflectivity tilt, rasterised exactly as the render worker does
/// it: same cache, same quality preset, same viewport arithmetic.
fn raster_lowest_reflectivity(
    volume: &radar_core::RadarVolume,
    camera: Camera2D,
    viewport: ViewportMetrics,
) -> egui::ColorImage {
    let moment = MomentType::Reflectivity;
    let cut_index = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| cut.moments.contains_key(&moment) && !cut.radials.is_empty())
        .min_by(|left, right| left.1.elevation_deg.total_cmp(&right.1.elevation_deg))
        .map(|(index, _)| index)
        .expect("the volume holds a reflectivity tilt");
    println!(
        "cut     #{cut_index} at {:.2} deg",
        volume.cuts[cut_index].elevation_deg
    );

    let side = (PANE_POINTS * viewport.pixels_per_point) as u32;
    let options = render2d::ViewportRasterOptions {
        width: side,
        height: side,
        radar_x_px: side as f32 / 2.0,
        radar_y_px: side as f32 / 2.0,
        km_per_px_x: camera.km_per_point / viewport.pixels_per_point,
        km_per_px_y: camera.km_per_point / viewport.pixels_per_point,
        rotation_rad: 0.0,
    };
    let tables = color_tables::ColorTableSet::default();
    let quality = render2d::DisplayQuality::default();
    let cache = render2d::ViewportMomentCache::new_display_quality(
        volume, cut_index, moment, &tables, quality,
    )
    .expect("the reflectivity tilt builds a moment cache");
    let mut rgba =
        vec![0_u8; render2d::quality::quality_rgba_buffer_len(options, quality.supersample)];
    let (width, height) = render2d::quality::render_moment_viewport_quality_rgba_into(
        &cache,
        volume,
        options,
        quality.supersample,
        &mut rgba,
    )
    .expect("the tilt rasterises");
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
}

/// The pane's map: the real basemap at this LOD, the real projection about the
/// volume's own site, and real neighbouring radars as markers.
fn pane_map(
    volume: &radar_core::RadarVolume,
    annotation: Annotation,
    unit_system: UnitSystem,
) -> PaneMap {
    // The volume's own reported position, which is what `install_loaded_volume`
    // hands to `set_radar_anchor`. A volume with no RVOL block cannot be
    // photographed honestly, so this refuses rather than guessing a centre.
    let (latitude, longitude) = volume
        .site
        .latitude_deg
        .zip(volume.site.longitude_deg)
        .map(|(latitude, longitude)| (f64::from(latitude), f64::from(longitude)))
        .expect("the volume reports its own site position");
    let projection = map_scene::RadarProjection::new(latitude, longitude);
    let scale = Camera2D::default().km_per_point;
    let geometry = map_scene::build_geometry(&map_scene::MapBuildRequest {
        key: GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod: LodBucket::ideal(scale, map_scene::LOD_REFERENCE_KM_PER_POINT),
        },
        dataset: map_scene::MapDataset::from_generated(Generation::new(1)),
        projection,
        style: map_scene::MapStylePreset::default().style(),
    });
    let sites = NEIGHBOURS
        .iter()
        .map(|(id, latitude, longitude)| PlacedSite {
            id: (*id).to_owned(),
            world: projection.lon_lat_to_world(*longitude, *latitude),
        })
        .collect::<Vec<_>>();
    PaneMap {
        geometry: Some(Arc::new(geometry)),
        projection: Some(projection),
        tiles: None,
        chrome: map_scene::MapStylePreset::default().chrome(),
        sites: Arc::from(sites),
        site_labels: SiteLabelMode::default(),
        annotation,
        units: unit_system,
        active_site: Some(volume.site.id.clone()),
        hazards: Arc::from(Vec::new()),
    }
}

struct Proof {
    raster: egui::ColorImage,
    texture: Option<egui::TextureHandle>,
    map: PaneMap,
    camera: Camera2D,
    viewport: ViewportMetrics,
    title: String,
    status: String,
    shot_path: PathBuf,
    frames: u32,
    requested: bool,
}

impl eframe::App for Proof {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.frames += 1;
        let texture = self.texture.get_or_insert_with(|| {
            context.load_texture("pane-proof-raster", self.raster.clone(), Default::default())
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| {
                let rect = egui::Rect::from_min_size(
                    ui.max_rect().min,
                    egui::vec2(PANE_POINTS, PANE_POINTS),
                );
                let overlay = PaneOverlay {
                    spectrum: None,
                    legend: None,
                    table: None,
                    product_name: "REF",
                    badges: &[],
                    probe: None,
                    // This proof photographs the pane chrome with nothing
                    // hidden, so there is no band to draw over it.
                };
                draw_pane(
                    ui,
                    PaneId::new(0).expect("pane 0"),
                    rect,
                    true,
                    self.camera,
                    // No projection: this proof photographs the pane chrome
                    // with a camera it wrote itself, so the map's north-up
                    // rule has nothing to say about it.
                    north_up::NorthUpFrame::new(None, self.viewport, 0.0),
                    pane_canvas::NavTuning::default(),
                    Some(PaneTexture {
                        handle: texture,
                        camera: self.camera,
                        viewport: self.viewport,
                    }),
                    &self.map,
                    &self.title,
                    &self.status,
                    &overlay,
                );
            });

        // A few passes so the font atlas rasterises and the map settles.
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
            println!("wrote {}", self.shot_path.display());
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        context.request_repaint();
    }
}
