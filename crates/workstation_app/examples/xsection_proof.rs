//! Photograph the real cross-section window, with the unit settings turned to
//! a named preset, and look at the axes.
//!
//! ```text
//! cargo run --release -p workstation_app --example xsection_proof -- \
//!     <level2-file> <out.png> [preset]
//! ```
//!
//! `examples/pane_proof.rs` exists because "the setting reaches the picture"
//! is a claim about pixels. This exists because of a narrower and more
//! embarrassing failure: the cross-section's READOUT was converted to the
//! analyst's units and its AXES were not, so a session in feet showed
//! "26903 ft ARL" a few pixels away from a height ladder labelled 0, 2, 4 …
//! 18 under the caption "km ARL". Unit tests on the tick helpers would have
//! caught the numbers; only a photograph shows the readout and the ladder
//! disagreeing in one frame, because that is what an analyst sees.
//!
//! So this drives the SHIPPED `xsection::XSection::window` - not a sample of
//! it - over a real Level II volume, with a line drawn across the storm, and
//! writes a PNG.
//!
//! The presets:
//!
//! * `default` - kilometres and kilometres. The control frame: this must be
//!   the window the application drew before the units work existed.
//! * `imperial` - statute miles and feet. Both axes, both captions and the
//!   readout must agree with each other and with the settings.
//! * `metres` - kilometres and metres, a 12 km top. A shallower slice, to see
//!   the ladder re-choose its own step.

// The application, compiled exactly as `src/main.rs` compiles it. Only the
// modules `xsection` actually reaches: it is a deliberately narrow module -
// everything about its surface arrives through `XSectionInput`, and the one
// `crate::` sibling it names is `units`, which is a leaf.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod units;
    pub mod xsection;
}

#[allow(unused_imports)]
pub(crate) use source::{units, xsection};

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use radar_core::MomentType;

use units::{AltitudeUnit, DistanceUnit, UnitSystem};
use xsection::{SectionLine, XSection, XSectionInput, XsCandidate};

/// Window size in points. Wide, because the thing under test is a distance
/// axis and a height ladder, and a cramped plot hides exactly the crowding a
/// tick spacing is supposed to prevent.
const WINDOW: [f32; 2] = [900.0, 520.0];

/// The section line, radar-local kilometres: a 120 km cut across KDVN's
/// storms, from the south-west quadrant to the north-east. Long enough that
/// the distance axis has to choose a real step.
const LINE: SectionLine = SectionLine {
    a_km: (-60.0, -40.0),
    b_km: (60.0, 40.0),
};

fn main() -> eframe::Result {
    let mut arguments = std::env::args().skip(1);
    let Some(volume_path) = arguments.next().map(PathBuf::from) else {
        eprintln!("usage: xsection_proof <level2-file> <out.png> [default|imperial|metres]");
        std::process::exit(2);
    };
    let shot_path = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("xsection_proof.png"));
    let preset = arguments.next().unwrap_or_else(|| "default".to_owned());
    let (unit_system, top_m) = preset_for(&preset);

    let volume = nexrad_io::decode_volume_from_path(&volume_path)
        .unwrap_or_else(|error| panic!("could not decode {}: {error}", volume_path.display()));
    println!(
        "volume  {} {}  ({} cuts)",
        volume.site.id,
        volume.volume_time.to_rfc3339(),
        volume.cuts.len()
    );
    println!("preset  {preset}");
    println!("top     {top_m} m");
    println!("line    {:.1} km", LINE.length_km());

    let mut section = XSection::default();
    section.open = true;
    section.line = Some(LINE);

    let proof = Proof {
        volume: Arc::new(volume),
        section,
        tables: color_tables::ColorTableSet::default(),
        units: unit_system,
        top_m,
        shot_path,
        frames: 0,
        requested: false,
    };
    eframe::run_native(
        "Cross-section proof",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size(WINDOW),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(proof))),
    )
}

/// The named presets. `default` is `UnitSystem::default()` and the shipped
/// top and nothing else, so the control frame cannot carry a tweak.
fn preset_for(name: &str) -> (UnitSystem, f32) {
    match name {
        "imperial" => (
            UnitSystem {
                distance: DistanceUnit::StatuteMiles,
                altitude: AltitudeUnit::Feet,
                ..UnitSystem::default()
            },
            xsection::DEFAULT_TOP_M,
        ),
        "metres" => (
            UnitSystem {
                altitude: AltitudeUnit::Metres,
                ..UnitSystem::default()
            },
            12_000.0,
        ),
        _ => (UnitSystem::default(), xsection::DEFAULT_TOP_M),
    }
}

struct Proof {
    volume: Arc<radar_core::RadarVolume>,
    section: XSection,
    tables: color_tables::ColorTableSet,
    units: UnitSystem,
    top_m: f32,
    shot_path: PathBuf,
    frames: u32,
    requested: bool,
}

impl eframe::App for Proof {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.frames += 1;

        let domain = product_engine::ProductRegistry::builtin()
            .get("REF")
            .expect("REF is a builtin product")
            .domain;
        let candidates = [XsCandidate {
            volume: &self.volume,
            displayed: true,
        }];
        let input = XSectionInput {
            candidates: &candidates,
            moment: MomentType::Reflectivity,
            product_label: "REF".to_owned(),
            uses_dealiased_velocity: false,
            storm_motion: None,
            color_table: self
                .tables
                .for_family(color_tables::ColorTableFamily::Reflectivity),
            domain,
            units: self.units,
            range_decimals: 1,
            top_m: self.top_m,
        };
        self.section.window(&context, &input);

        // The slice is built on a worker, so the shot waits for the picture
        // rather than for a frame count: an empty plot with correct axes would
        // prove nothing about the axes being drawn over a real slice.
        let ready = self.section.has_built_slice();
        if ready && !self.requested {
            self.requested = true;
            context.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        if self.frames > 600 && !self.requested {
            panic!("the slice never finished building");
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
