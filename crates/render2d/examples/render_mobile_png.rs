//! Decode a mobile research radar input and render it, so a change can be
//! checked against real bytes rather than against a test's expectations.
//!
//! Accepts any of: one DORADE sweepfile (`swp.*`), a deployment zip, or a
//! deployment folder. Prints a decode summary and writes one PNG per volume
//! for the requested moment.
//!
//! ```text
//! cargo run --release -p render2d --example render_mobile_png -- <input> <out-dir> [moment]
//! ```

use std::path::{Path, PathBuf};

use nexrad_io::mobile_archive::{
    MobileVolume, decode_deployment_zip_from_path, decode_dorade_volume_for_path,
    decode_mobile_dir_from_path, looks_like_zip_path,
};
use radar_core::{MomentType, RadarVolume};
use render2d::{RasterOptions, render_moment_png};

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: cargo run --release -p render2d --example render_mobile_png -- \
             <sweepfile|deployment.zip|folder> <out-dir> [moment]"
        );
        std::process::exit(2);
    };
    let moment = std::env::args()
        .nth(3)
        .map(|value| MomentType::from_nexrad_name(&value.to_ascii_uppercase()))
        .unwrap_or(MomentType::Reflectivity);

    match run(&input, &out_dir, moment) {
        Ok(count) => println!("rendered {count} volume(s) into {}", out_dir.display()),
        Err(err) => {
            eprintln!("failed: {err}");
            std::process::exit(1);
        }
    }
}

fn run(
    input: &Path,
    out_dir: &Path,
    moment: MomentType,
) -> Result<usize, Box<dyn std::error::Error>> {
    let volumes = load(input)?;
    std::fs::create_dir_all(out_dir)?;
    let mut rendered = 0usize;
    for (index, entry) in volumes.iter().enumerate() {
        summarize(&entry.volume, entry.member_count, &entry.member_label);
        for (cut_index, cut) in entry.volume.cuts.iter().enumerate() {
            if !cut.moments.contains_key(&moment) {
                println!("    cut {cut_index}: no {moment}, skipped");
                continue;
            }
            let name = format!(
                "{index:02}_{}_{}_cut{cut_index}_{moment}.png",
                entry.volume.site.id,
                entry.volume.volume_time.format("%Y%m%dT%H%M%SZ")
            );
            let path = out_dir.join(name);
            render_moment_png(
                &entry.volume,
                cut_index,
                moment.clone(),
                &path,
                RasterOptions::default(),
            )?;
            println!("    wrote {}", path.display());
            rendered += 1;
        }
    }
    Ok(rendered)
}

fn load(input: &Path) -> Result<Vec<MobileVolume>, Box<dyn std::error::Error>> {
    if input.is_dir() {
        return Ok(decode_mobile_dir_from_path(input)?);
    }
    if looks_like_zip_path(input) {
        return Ok(decode_deployment_zip_from_path(input)?);
    }
    let volume = decode_dorade_volume_for_path(input)?;
    Ok(vec![MobileVolume {
        member_label: input.display().to_string(),
        member_count: volume.cuts.len(),
        volume,
    }])
}

fn summarize(volume: &RadarVolume, member_count: usize, label: &str) {
    println!(
        "{label}\n  site {} ({:?}) lat {:?} lon {:?} alt {:?}\n  time {} format {:?} compression {:?}\n  {} member(s), {} cut(s), {} radial(s)",
        volume.site.id,
        volume.site.name,
        volume.site.latitude_deg,
        volume.site.longitude_deg,
        volume.site.elevation_m,
        volume.volume_time,
        volume.metadata.archive_version,
        volume.metadata.compression,
        member_count,
        volume.cuts.len(),
        volume.metadata.decoded_radial_count,
    );
    for (index, cut) in volume.cuts.iter().enumerate() {
        let geometry = cut
            .radials
            .first()
            .map(|radial| radial.gate_range.clone())
            .unwrap_or(radar_core::GateRange {
                first_gate_m: 0,
                gate_spacing_m: 0,
                gate_count: 0,
            });
        let moments: Vec<String> = cut
            .moments
            .keys()
            .map(|moment| moment.to_string())
            .collect();
        println!(
            "  cut {index}: fixed {:.4} deg, {} radials, gates {}x{} m from {} m, moments [{}]",
            cut.elevation_deg,
            cut.radials.len(),
            geometry.gate_count,
            geometry.gate_spacing_m,
            geometry.first_gate_m,
            moments.join(", "),
        );
    }
}
