//! Drive a user colour table through the whole chain, on a real volume.
//!
//! ```text
//! cargo run --release -p workstation_app --example user_table_proof -- \
//!     <level2-file> <out-dir> [palette.pal] [cut-index]
//! ```
//!
//! What it exercises, in the order the application does:
//!
//! 1. the drop path - `user_tables::UserTables::import_all`, the same call
//!    `app.rs` makes for a `.pal` dragged onto the window - which copies the
//!    file into the colour table folder and reports what happened;
//! 2. the scan, which is what turns that file into a named table in a family;
//! 3. persistence - the choice is captured, written to a REAL settings file,
//!    the file is reopened, and the table set is resolved back out of it, so
//!    what renders below came out of a settings file and not out of a
//!    variable;
//! 4. the render, twice: once with the shipped defaults and once with the
//!    resolved set, into two PNGs that can be looked at side by side.
//!
//! The numbers it prints are the proof that the pixels came from the
//! analyst's table rather than from the built-in one: how many pixels moved,
//! and what fraction of the painted pixels carry a colour the user table can
//! produce versus one the default table can produce.
//!
//! This is the "real data, looked at" half of the gates. A synthetic sweep
//! would prove the plumbing and nothing about the picture.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use color_tables::{ColorTable, ColorTableFamily, ColorTableSet, Rgba8};
use radar_core::MomentType;
use render2d::{DisplayQuality, ViewportMomentCache, ViewportRasterOptions};

// The application's own modules, compiled into this example exactly as
// `src/main.rs` compiles them - the same trick `theme_gallery` uses, and for
// the same reason: a proof that runs against a copy of the code proves
// nothing about the code.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod settings_ui;
    pub mod user_tables;
}
use source::{settings_ui, user_tables};

const FRAME_PX: u32 = 900;
const KM_PER_PX: f32 = 0.42;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [input, out_dir, rest @ ..] = arguments.as_slice() else {
        eprintln!("usage: user_table_proof <level2-file> <out-dir> [palette.pal] [cut-index]");
        std::process::exit(2);
    };
    let palette = rest
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docs/palettes/Sample Ramp-Pair Velocity.pal"));
    let cut_override = rest.get(1).and_then(|value| value.parse::<usize>().ok());

    if let Err(message) = run(Path::new(input), Path::new(out_dir), &palette, cut_override) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run(
    input: &Path,
    out_dir: &Path,
    palette: &Path,
    cut_override: Option<usize>,
) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|error| format!("{}: {error}", out_dir.display()))?;
    let volume = nexrad_io::decode_volume_from_path(input)
        .map_err(|error| format!("could not decode {}: {error}", input.display()))?;
    let moment = MomentType::Velocity;
    let cut_index = match cut_override {
        Some(index) => index,
        None => volume
            .cuts
            .iter()
            .enumerate()
            .filter(|(_, cut)| cut.moments.contains_key(&moment) && !cut.radials.is_empty())
            .min_by(|left, right| left.1.elevation_deg.total_cmp(&right.1.elevation_deg))
            .map(|(index, _)| index)
            .ok_or_else(|| format!("no velocity anywhere in {}", input.display()))?,
    };
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or_else(|| format!("cut {cut_index} is out of range"))?;
    println!("file    {}", input.display());
    println!(
        "site    {}  {}",
        volume.site.id,
        volume.volume_time.to_rfc3339()
    );
    println!(
        "cut     #{cut_index} at {:.2} deg, {} radials",
        cut.elevation_deg,
        cut.radials.len()
    );

    // 1 - the drop path, into a colour table folder beside the output.
    let folder = out_dir.join("colortables");
    let mut tables = user_tables::UserTables::open(&folder);
    let landed = tables.import_all(&[palette.to_path_buf()]);
    println!("\ndrop    {}", tables.notice_text().unwrap_or("(silent)"));
    if !landed {
        return Err("the palette did not import; nothing to render through".to_owned());
    }

    // 2 - the scan.
    println!("folder  {}", tables.library().directory().display());
    for entry in tables.library().tables() {
        println!(
            "        {} · {} · {}",
            entry.display_name(),
            entry.family().label(),
            entry.file_name()
        );
    }
    for fault in tables.library().faults() {
        println!("        SKIPPED {fault}");
    }
    let user_table = tables
        .library()
        .tables()
        .iter()
        .find(|entry| entry.family() == ColorTableFamily::Velocity)
        .ok_or("the palette did not land in the velocity family")?
        .table()
        .clone();

    // 3 - persistence, through a real settings file on disk.
    let settings_path = out_dir.join("settings.json");
    let _ = std::fs::remove_file(&settings_path);
    {
        let mut installed = ColorTableSet::default();
        installed.set_family(ColorTableFamily::Velocity, user_table.clone());
        let mut store = settings::SettingsStore::open(&settings_path);
        let mut workspace = store.workspace().clone();
        workspace.palettes = settings_ui::palettes::capture_palettes_preserving(
            &installed,
            &workspace.palettes,
            tables.library(),
        );
        store.set_workspace(workspace);
        store.save_now().map_err(|error| error.to_string())?;
    }
    let reopened = settings::SettingsStore::open(&settings_path);
    let stored_name = reopened
        .workspace()
        .palettes
        .get("velocity")
        .map(|choice| choice.name.clone())
        .unwrap_or_default();
    let resolved = settings_ui::palettes::apply_palettes_with_user(
        &reopened.workspace().palettes,
        tables.library(),
    );
    println!(
        "\nstored  velocity = {stored_name:?} in {}",
        settings_path.display()
    );
    println!(
        "resolved velocity = {:?}",
        resolved.for_family(ColorTableFamily::Velocity).name()
    );
    if resolved.for_family(ColorTableFamily::Velocity).base_name() != user_table.base_name() {
        return Err("the settings file did not resolve back to the user table".to_owned());
    }

    // 4 - the render, twice.
    let options = ViewportRasterOptions {
        width: FRAME_PX,
        height: FRAME_PX,
        radar_x_px: FRAME_PX as f32 / 2.0,
        radar_y_px: FRAME_PX as f32 / 2.0,
        km_per_px_x: KM_PER_PX,
        km_per_px_y: KM_PER_PX,
    };
    let defaults = ColorTableSet::default();
    let shown = DisplayQuality::default();
    let with_default = render(&volume, cut_index, &moment, &defaults, options, shown)?;
    let with_user = render(&volume, cut_index, &moment, &resolved, options, shown)?;
    write_png(out_dir.join("velocity-default.png"), &with_default)?;
    write_png(out_dir.join("velocity-user-table.png"), &with_user)?;

    // The proof. Both palettes are velocity palettes over the same m/s
    // domain, so a colour COULD belong to both; what cannot happen by
    // accident is a picture that changed and whose painted colours are all
    // reachable from the user table and none from the built-in one.
    //
    // Measured on the NATIVE raster, one sample per pixel. The shipped
    // quality box-filters a supersampled frame, and the average of two table
    // colours is not itself a table colour, so membership measured on that
    // frame would undercount for a reason that has nothing to do with which
    // table was used. The two PNGs above stay at the shipped quality: they
    // are what an analyst looks at.
    let exact = DisplayQuality::NATIVE;
    let native_default = render(&volume, cut_index, &moment, &defaults, options, exact)?;
    let native_user = render(&volume, cut_index, &moment, &resolved, options, exact)?;
    let user_colors = reachable_colors(resolved.for_family(ColorTableFamily::Velocity));
    let default_colors = reachable_colors(defaults.for_family(ColorTableFamily::Velocity));
    let mut painted = 0_usize;
    let mut moved = 0_usize;
    let mut from_user = 0_usize;
    let mut from_default = 0_usize;
    for (left, right) in native_default
        .chunks_exact(4)
        .zip(native_user.chunks_exact(4))
    {
        if left != right {
            moved += 1;
        }
        if right[3] == 0 {
            continue;
        }
        painted += 1;
        let colour = [right[0], right[1], right[2]];
        if user_colors.contains(&colour) {
            from_user += 1;
        }
        if default_colors.contains(&colour) {
            from_default += 1;
        }
    }
    let total = native_user.len() / 4;
    println!("\npixels  {total} in the native frame, {painted} painted");
    println!(
        "moved   {moved} ({:.1}% of the frame, {:.1}% of the painted pixels) changed when the \
         user table was installed",
        100.0 * moved as f32 / total as f32,
        100.0 * moved as f32 / painted.max(1) as f32
    );
    println!(
        "source  {:.2}% of painted pixels carry a colour the USER table produces",
        100.0 * from_user as f32 / painted.max(1) as f32
    );
    println!(
        "        {:.2}% carry one the DEFAULT table produces",
        100.0 * from_default as f32 / painted.max(1) as f32
    );
    println!("\nwrote {}", out_dir.display());
    Ok(())
}

fn render(
    volume: &radar_core::RadarVolume,
    cut_index: usize,
    moment: &MomentType,
    tables: &ColorTableSet,
    options: ViewportRasterOptions,
    quality: DisplayQuality,
) -> Result<Vec<u8>, String> {
    let cache = ViewportMomentCache::new_display_quality(
        volume,
        cut_index,
        moment.clone(),
        tables,
        quality,
    )
    .map_err(|error| error.to_string())?;
    let mut rgba =
        vec![0_u8; render2d::quality::quality_rgba_buffer_len(options, quality.supersample)];
    let (width, height) = render2d::quality::render_moment_viewport_quality_rgba_into(
        &cache,
        volume,
        options,
        quality.supersample,
        &mut rgba,
    )
    .map_err(|error| error.to_string())?;
    if (width, height) != (FRAME_PX, FRAME_PX) {
        return Err(format!(
            "expected a {FRAME_PX} px frame, got {width}x{height}"
        ));
    }
    Ok(rgba)
}

fn write_png(path: PathBuf, rgba: &[u8]) -> Result<(), String> {
    let image = image::RgbaImage::from_raw(FRAME_PX, FRAME_PX, rgba.to_vec())
        .ok_or("buffer does not match the frame")?;
    image
        .save(&path)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Every RGB a table can paint, swept finer than the radar's own value
/// quantisation.
///
/// NEXRAD velocity arrives on a 0.5 m/s (or 1.0 m/s) code grid, so a sweep at
/// roughly 0.002 m/s cannot miss a colour the renderer's palette lookup can
/// produce; the set is therefore a superset of what any render through this
/// table can contain, and membership is a real test rather than a coincidence.
fn reachable_colors(table: &ColorTable) -> BTreeSet<[u8; 3]> {
    let stops = table.stops();
    let (low, high) = (
        stops.first().map(|stop| stop.value).unwrap_or(-100.0),
        stops.last().map(|stop| stop.value).unwrap_or(100.0),
    );
    let steps = 100_000;
    let mut colors: BTreeSet<[u8; 3]> = (0..=steps)
        .map(|index| low + (high - low) * index as f32 / steps as f32)
        .map(|value| table.sample(value))
        .filter(|colour: &Rgba8| colour.a != 0)
        .map(|colour| [colour.r, colour.g, colour.b])
        .collect();
    // The range-folded flag is not a value on the ramp, it is the palette's
    // `RF:` row, and a 0.44 degree velocity sweep is full of it.
    let folded = table.range_folded_rgba();
    colors.insert([folded.r, folded.g, folded.b]);
    colors
}
