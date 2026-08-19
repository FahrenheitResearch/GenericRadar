//! Replay a real chunked volume arriving, and photograph what the pane draws.
//!
//! This exists because a sweep animation cannot be verified on synthetic data.
//! Whether the reveal tracks the antenna, and whether the unswept wedge keeps
//! showing the previous tilt instead of going blank, are questions about a
//! specific radar's chunk cadence and azimuth layout - so this replays the
//! actual chunk files the live service cached, in the order they arrived, at
//! the wall-clock spacing they arrived at (their file modification times), and
//! writes one PNG per chunk.
//!
//! ```text
//! cargo run --release -p workstation_app --example sweep_replay -- \
//!     <previous-volume-file> <chunk-dir> <out-dir> [MOMENT]
//! ```
//!
//! `<previous-volume-file>` is the complete volume before this one: the picture
//! the unswept wedge should be showing. `<chunk-dir>` is one of the directories
//! under the live cache's `.chunks`, whose files concatenate, in name order,
//! into the volume as it stood after each chunk - which is exactly how
//! `data_source::append_realtime_chunks` builds it.
//!
//! Two PNGs are written per chunk: `NNN-blend.png`, what the pane now draws,
//! and `NNN-plain.png`, what it drew before this existed. The difference
//! between those two files is the whole feature.

#[path = "../src/sweep.rs"]
mod sweep;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use radar_core::{MomentType, RadarVolume};
use render2d::ViewportRasterOptions;
use render2d::sweep_blend::{SweepBlend, render_sweep_blend_rgba_into};
use render2d::{ViewportMomentCache, viewport_rgba_buffer_len};

use sweep::{SweepAnimator, catch_up_factor, matching_cut_index};

/// Frame size for the photographs. Square, radar centred.
const FRAME_PX: u32 = 700;
/// Kilometres per pixel, chosen so a 230 km sweep fits inside the frame.
const KM_PER_PX: f32 = 0.7;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [previous_path, chunk_dir, out_dir, rest @ ..] = arguments.as_slice() else {
        eprintln!(
            "usage: sweep_replay <previous-volume-file> <chunk-dir> <out-dir> [REF|VEL|SW|ZDR|RHO]"
        );
        std::process::exit(2);
    };
    let moment = rest
        .first()
        .map(|name| MomentType::from_nexrad_name(name))
        .unwrap_or(MomentType::Reflectivity);

    if let Err(message) = run(
        Path::new(previous_path),
        Path::new(chunk_dir),
        Path::new(out_dir),
        moment,
    ) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run(
    previous_path: &Path,
    chunk_dir: &Path,
    out_dir: &Path,
    moment: MomentType,
) -> Result<(), String> {
    let previous = nexrad_io::decode_volume_from_path(previous_path)
        .map_err(|error| format!("could not decode {}: {error}", previous_path.display()))?;
    fs::create_dir_all(out_dir).map_err(|error| format!("{}: {error}", out_dir.display()))?;

    let chunks = chunk_files(chunk_dir)?;
    println!(
        "previous  {}  {}  {} cuts",
        previous.site.id,
        previous.volume_time.to_rfc3339(),
        previous.cuts.len()
    );
    println!(
        "arriving  {} chunks from {}",
        chunks.len(),
        chunk_dir.display()
    );
    println!("moment    {moment:?}");
    println!();
    println!(
        "{:>3}  {:>8}  {:>7}  {:>7}  {:>8}  {:>9}  {:>9}  {:>8}  {:>5}  {:>9}",
        "chk",
        "arrived",
        "cut",
        "radials",
        "start",
        "frontier",
        "revealed",
        "rate",
        "done",
        "underpaint"
    );

    let mut bytes = Vec::new();
    let mut animator = SweepAnimator::new();
    let mut last_arrival: Option<SystemTime> = None;
    let mut last_pending_deg = 0.0_f32;

    for (index, (path, arrived)) in chunks.iter().enumerate() {
        let chunk = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        bytes.extend_from_slice(&chunk);

        // The first chunk is the metadata header alone; it decodes to a volume
        // with no radials in it, which is a real state the pane sees.
        let Ok(volume) = nexrad_io::decode_volume_from_bytes(&bytes) else {
            println!("{:>3}  {:>8}  (not yet decodable)", index + 1, "-");
            continue;
        };

        let elapsed = last_arrival
            .and_then(|last| arrived.duration_since(last).ok())
            .unwrap_or(Duration::ZERO);
        last_arrival = Some(*arrived);

        let Some(cut_index) = lowest_cut_with(&volume, &moment) else {
            println!(
                "{:>3}  {:>8.2}  (no {moment:?} yet)",
                index + 1,
                elapsed.as_secs_f32()
            );
            continue;
        };
        let cut = &volume.cuts[cut_index];

        // Exactly what `app::advance_sweeps` does, including the catch-up.
        let scaled = elapsed.mul_f32(catch_up_factor(last_pending_deg));
        let Some(state) = animator.observe(cut, scaled) else {
            println!(
                "{:>3}  {:>8.2}  (cut has no usable azimuths)",
                index + 1,
                elapsed.as_secs_f32()
            );
            continue;
        };
        last_pending_deg = state.pending_deg();

        let previous_cut_index = matching_cut_index(&previous, cut, &moment);
        let incoming_grid = cut
            .moments
            .get(&moment)
            .expect("checked by lowest_cut_with");
        let underpaint = previous_cut_index
            .and_then(|index| previous.cuts.get(index))
            .and_then(|cut| cut.moments.get(&moment).map(|grid| (cut, grid)));

        println!(
            "{:>3}  {:>7.2}s  {:>7}  {:>7}  {:>7.1}d  {:>8.1}d  {:>8.1}d  {:>6.1}d/s  {:>5}  {:>9}",
            index + 1,
            elapsed.as_secs_f32(),
            cut_index,
            cut.radials.len(),
            state.start_deg,
            state.frontier_deg,
            state.revealed_deg,
            state.rate_deg_per_s,
            if state.complete { "yes" } else { "no" },
            previous_cut_index
                .map(|index| index.to_string())
                .unwrap_or_else(|| "NONE".to_owned()),
        );

        let options = frame_options();
        let mut rgba = vec![0_u8; viewport_rgba_buffer_len(options)];
        render_sweep_blend_rgba_into(
            &SweepBlend {
                incoming: cut,
                incoming_grid,
                previous: underpaint,
                start_deg: state.start_deg,
                revealed_deg: state.revealed_deg,
            },
            options,
            &color_tables::ColorTableSet::default(),
            &mut rgba,
        )
        .map_err(|error| format!("blend render failed: {error}"))?;
        save_png(
            out_dir,
            &format!("{:03}-blend.png", index + 1),
            &rgba,
            options,
        )?;

        // And the same instant drawn the old way, for comparison.
        let mut plain = vec![0_u8; viewport_rgba_buffer_len(options)];
        if let Ok(cache) = ViewportMomentCache::new(&volume, cut_index, moment.clone()) {
            cache
                .render_moment_rgba_into(&volume, options, &mut plain)
                .map_err(|error| format!("plain render failed: {error}"))?;
            save_png(
                out_dir,
                &format!("{:03}-plain.png", index + 1),
                &plain,
                options,
            )?;
        }

        let opaque = |pixels: &[u8]| pixels.chunks_exact(4).filter(|p| p[3] > 0).count();
        let (blended, alone) = (opaque(&rgba), opaque(&plain));
        if alone > 0 {
            println!(
                "     coverage: {blended} px blended vs {alone} px alone ({:+.0}%)",
                (blended as f32 / alone as f32 - 1.0) * 100.0
            );
        }
    }

    println!("\nwrote {} to {}", chunks.len() * 2, out_dir.display());
    Ok(())
}

fn frame_options() -> ViewportRasterOptions {
    ViewportRasterOptions {
        width: FRAME_PX,
        height: FRAME_PX,
        radar_x_px: FRAME_PX as f32 / 2.0,
        radar_y_px: FRAME_PX as f32 / 2.0,
        km_per_px_x: KM_PER_PX,
        km_per_px_y: KM_PER_PX,
    }
}

/// The chunk files in the order the radar sent them, with when they landed.
///
/// Name order is arrival order: the live cache names them with the sequence
/// number the S3 feed assigned, zero padded, so a plain sort is the real order.
fn chunk_files(directory: &Path) -> Result<Vec<(PathBuf, SystemTime)>, String> {
    let mut files = fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            let arrived = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (entry.path(), arrived)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.is_empty() {
        return Err(format!("no chunk files in {}", directory.display()));
    }
    Ok(files)
}

/// The lowest tilt that actually carries the moment, which is the cut a pane on
/// "lowest available" would be showing.
fn lowest_cut_with(volume: &RadarVolume, moment: &MomentType) -> Option<usize> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| cut.moments.contains_key(moment) && !cut.radials.is_empty())
        .min_by(|left, right| left.1.elevation_deg.total_cmp(&right.1.elevation_deg))
        .map(|(index, _)| index)
}

fn save_png(
    directory: &Path,
    name: &str,
    rgba: &[u8],
    options: ViewportRasterOptions,
) -> Result<(), String> {
    let image = image::RgbaImage::from_raw(options.width, options.height, rgba.to_vec())
        .ok_or_else(|| {
            format!(
                "{name}: buffer does not match {}x{}",
                options.width, options.height
            )
        })?;
    image
        .save(directory.join(name))
        .map_err(|error| format!("{name}: {error}"))
}
