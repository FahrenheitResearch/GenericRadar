//! Render one ODIM_H5 polar volume (EUMETNET OPERA information model) to PNGs.
//!
//! The routing seam will hand an ODIM file to the right decoder now, and
//! `render_reflectivity_png` draws whatever it returns. This example calls
//! `nexrad_io::odim` directly on purpose: it exercises the ODIM decoder
//! with the seam out of the way, and it writes several moments of one
//! sweep in a single run — real bytes to a picture you can look at.
//!
//! usage: cargo run --release -p render2d --example render_odim_png -- \
//!            <file.h5> <out-dir> [cut-index] [moment...]

use std::path::{Path, PathBuf};

use radar_core::MomentType;
use render2d::{RasterOptions, render_moment_png};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(input), Some(out_dir)) = (args.first(), args.get(1)) else {
        eprintln!(
            "usage: cargo run --release -p render2d --example render_odim_png -- \
             <file.h5> <out-dir> [cut-index] [moment...]"
        );
        std::process::exit(2);
    };
    let cut_index = args
        .get(2)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let moments: Vec<MomentType> = if args.len() > 3 {
        args[3..]
            .iter()
            .map(|value| MomentType::from_nexrad_name(&value.to_ascii_uppercase()))
            .collect()
    } else {
        vec![MomentType::Reflectivity, MomentType::Velocity]
    };

    match run(Path::new(input), Path::new(out_dir), cut_index, &moments) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("render failed: {err}");
            std::process::exit(1);
        }
    }
}

fn run(
    input: &Path,
    out_dir: &Path,
    cut_index: usize,
    moments: &[MomentType],
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(input)?;
    let volume = nexrad_io::odim::decode_odim_h5_volume(&bytes)?;
    std::fs::create_dir_all(out_dir)?;

    println!(
        "{} {} lat {:?} lon {:?} time {} cuts {}",
        input.file_name().unwrap_or_default().to_string_lossy(),
        volume.site.id,
        volume.site.latitude_deg,
        volume.site.longitude_deg,
        volume.volume_time.to_rfc3339(),
        volume.cuts.len(),
    );
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or_else(|| format!("cut {cut_index} of {}", volume.cuts.len()))?;
    println!(
        "  cut {cut_index}: elev {:.2} deg, {} rays, moments {:?}",
        cut.elevation_deg,
        cut.radials.len(),
        cut.moments_available()
            .iter()
            .map(|moment| moment.short_name().to_owned())
            .collect::<Vec<_>>(),
    );

    let stem = input
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    for moment in moments {
        if !cut.moments.contains_key(moment) {
            println!("  skip {moment}: not in this cut");
            continue;
        }
        let mut finite = 0usize;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        if let Some(grid) = cut.moments.get(moment) {
            for row in 0..grid.radial_indices.len() {
                for gate in 0..grid.gate_range.gate_count {
                    if let Some(value) = grid.scaled_value(row, gate)
                        && value.is_finite()
                    {
                        finite += 1;
                        min = min.min(value);
                        max = max.max(value);
                    }
                }
            }
        }
        let out: PathBuf = out_dir.join(format!("{stem}_cut{cut_index}_{moment}.png"));
        render_moment_png(
            &volume,
            cut_index,
            moment.clone(),
            &out,
            RasterOptions::default(),
        )?;
        if finite > 0 {
            println!(
                "  {moment}: {finite} valid gates, {min:.2}..{max:.2} -> {}",
                out.display()
            );
        } else {
            println!("  {moment}: NO valid gates -> {}", out.display());
        }
    }
    Ok(())
}
