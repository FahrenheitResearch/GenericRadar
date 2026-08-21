//! Photograph one real sweep at every display-quality preset.
//!
//! The rasteriser takes one sample per screen pixel off the native polar
//! lattice, which aliases into speckle when a pixel covers several gates and
//! goes blocky when a gate covers several pixels. Three passes address that -
//! softening, polar upsampling and supersampling - and this renders the same
//! real sweep through each combination so the difference can be looked at
//! rather than argued about.
//!
//! ```text
//! cargo run --release -p workstation_app --example quality_compare -- \
//!     <level2-file> <out-dir> [MOMENT] [km-per-px]
//! ```
//!
//! It also reports two numbers per preset: the render time, and a speckle
//! count - pixels whose colour matches none of their four neighbours. Speckle
//! is what aliasing looks like when it is counted instead of squinted at.
//!
//! Read that count with care: once the picture has continuous tone, an
//! antialiased edge pixel legitimately matches none of its neighbours, so the
//! metric RISES with quality even as the picture improves. It measures how
//! many distinct colours meet, not how bad the aliasing is. The frames are the
//! evidence; this is only a hint about where to look in them.

use std::path::Path;
use std::time::Instant;

use radar_core::MomentType;
use render2d::{DisplayQuality, ViewportMomentCache, ViewportRasterOptions};

const FRAME_PX: u32 = 900;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [input, out_dir, rest @ ..] = arguments.as_slice() else {
        eprintln!("usage: quality_compare <level2-file> <out-dir> [MOMENT] [km-per-px]");
        std::process::exit(2);
    };
    let moment = rest
        .first()
        .map(|name| MomentType::from_nexrad_name(name))
        .unwrap_or(MomentType::Reflectivity);
    let km_per_px = rest
        .get(1)
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(0.55);

    if let Err(message) = run(Path::new(input), Path::new(out_dir), moment, km_per_px) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run(input: &Path, out_dir: &Path, moment: MomentType, km_per_px: f32) -> Result<(), String> {
    let volume = nexrad_io::decode_volume_from_path(input)
        .map_err(|error| format!("could not decode {}: {error}", input.display()))?;
    std::fs::create_dir_all(out_dir).map_err(|error| format!("{}: {error}", out_dir.display()))?;

    // The tilt a pane on "lowest available" would be showing.
    let cut_index = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| cut.moments.contains_key(&moment) && !cut.radials.is_empty())
        .min_by(|left, right| left.1.elevation_deg.total_cmp(&right.1.elevation_deg))
        .map(|(index, _)| index)
        .ok_or_else(|| format!("no {moment:?} anywhere in {}", input.display()))?;
    let cut = &volume.cuts[cut_index];
    let grid = &cut.moments[&moment];

    println!("file    {}", input.display());
    println!(
        "site    {}  {}",
        volume.site.id,
        volume.volume_time.to_rfc3339()
    );
    println!(
        "cut     #{cut_index} at {:.2} deg, {} radials x {} gates @ {} m",
        cut.elevation_deg,
        cut.radials.len(),
        grid.gate_range.gate_count,
        grid.gate_range.gate_spacing_m
    );
    println!("moment  {moment:?}   frame {FRAME_PX} px at {km_per_px} km/px");
    println!();
    println!(
        "{:>8}  {:>7}  {:>7}  {:>5}  {:>9}  {:>9}  {:>9}",
        "preset", "soften", "interp", "ss", "grid", "render", "speckle"
    );

    let options = ViewportRasterOptions {
        width: FRAME_PX,
        height: FRAME_PX,
        radar_x_px: FRAME_PX as f32 / 2.0,
        radar_y_px: FRAME_PX as f32 / 2.0,
        km_per_px_x: km_per_px,
        km_per_px_y: km_per_px,
        rotation_rad: 0.0,
    };
    let tables = color_tables::ColorTableSet::default();

    for (label, quality) in DisplayQuality::PRESETS {
        let built = Instant::now();
        let cache = ViewportMomentCache::new_display_quality(
            &volume,
            cut_index,
            moment.clone(),
            &tables,
            quality,
        )
        .map_err(|error| format!("{label}: {error}"))?;
        let grid_ms = built.elapsed().as_secs_f32() * 1_000.0;

        let mut rgba =
            vec![0_u8; render2d::quality::quality_rgba_buffer_len(options, quality.supersample)];
        let drawn = Instant::now();
        let (width, height) = render2d::quality::render_moment_viewport_quality_rgba_into(
            &cache,
            &volume,
            options,
            quality.supersample,
            &mut rgba,
        )
        .map_err(|error| format!("{label}: {error}"))?;
        let render_ms = drawn.elapsed().as_secs_f32() * 1_000.0;

        println!(
            "{label:>8}  {:>7}  {:>7}  {:>5}  {grid_ms:>7.1}ms  {render_ms:>7.1}ms  {:>9}",
            quality.soften,
            quality.interpolate,
            quality.supersample,
            speckle_count(&rgba, width, height),
        );

        let image = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| format!("{label}: buffer does not match {width}x{height}"))?;
        image
            .save(out_dir.join(format!("{}.png", label.to_lowercase())))
            .map_err(|error| format!("{label}: {error}"))?;
    }

    println!(
        "\nwrote {} PNGs to {}",
        DisplayQuality::PRESETS.len(),
        out_dir.display()
    );
    Ok(())
}

/// Opaque pixels whose colour matches none of their four neighbours.
///
/// A gate that is genuinely one pixel across is indistinguishable from an
/// aliasing artefact in a single frame, so this is a relative measure: it is
/// only meaningful compared between presets rendering the SAME sweep at the
/// SAME scale, where a fall means neighbouring pixels agree more than they did.
fn speckle_count(rgba: &[u8], width: u32, height: u32) -> usize {
    let pixel = |x: u32, y: u32| -> [u8; 4] {
        let at = (y as usize * width as usize + x as usize) * 4;
        [rgba[at], rgba[at + 1], rgba[at + 2], rgba[at + 3]]
    };
    let mut isolated = 0;
    for y in 1..height.saturating_sub(1) {
        for x in 1..width.saturating_sub(1) {
            let here = pixel(x, y);
            if here[3] == 0 {
                continue;
            }
            let neighbours = [
                pixel(x - 1, y),
                pixel(x + 1, y),
                pixel(x, y - 1),
                pixel(x, y + 1),
            ];
            if !neighbours.contains(&here) {
                isolated += 1;
            }
        }
    }
    isolated
}
