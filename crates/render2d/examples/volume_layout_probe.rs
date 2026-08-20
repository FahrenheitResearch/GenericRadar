//! Print the cut/moment layout of a Level II volume.
//!
//! Written to work out how WSR-88D split cuts land in the decoded model: which
//! cut carries which moment, at which nominal elevation, over which gate
//! layout, and when. That is the input to companion-sweep resolution.

use std::path::PathBuf;

use radar_core::MomentType;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(input) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run -p render2d --example volume_layout_probe -- <level2-file>");
        std::process::exit(2);
    };
    let volume = nexrad_io::decode_volume_from_path(&input)?;
    println!(
        "site={} time={} vcp={:?} cuts={}",
        volume.site.id,
        volume.volume_time,
        volume.vcp.as_ref().map(|vcp| vcp.pattern),
        volume.cuts.len()
    );
    for (index, cut) in volume.cuts.iter().enumerate() {
        let first_offset = cut
            .radials
            .first()
            .map(|radial| radial.time_offset_ms)
            .unwrap_or_default();
        let last_offset = cut
            .radials
            .last()
            .map(|radial| radial.time_offset_ms)
            .unwrap_or_default();
        let nyquist = cut
            .radials
            .first()
            .and_then(|radial| radial.nyquist_velocity_mps);
        println!(
            "cut {index:>2} elev={:>6.2} elev_no={:?} radials={} t=[{first_offset},{last_offset}]ms nyquist={nyquist:?}",
            cut.elevation_deg,
            cut.elevation_number,
            cut.radials.len()
        );
        for (moment, grid) in &cut.moments {
            let valid = count_valid(grid);
            println!(
                "        {:<4} rows={:<5} gates={:<5} first={:>6}m spacing={:>4}m scale={} offset={} nodata={:?} rf={:?} valid={valid}",
                moment.short_name(),
                grid.radial_count(),
                grid.gate_range.gate_count,
                grid.gate_range.first_gate_m,
                grid.gate_range.gate_spacing_m,
                grid.scale,
                grid.offset,
                grid.nodata,
                grid.range_folded,
            );
        }
        let _ = MomentType::Reflectivity;
    }
    Ok(())
}

fn count_valid(grid: &radar_core::MomentGrid) -> usize {
    let gate_count = grid.gate_range.gate_count;
    (0..grid.radial_count())
        .map(|row| {
            (0..gate_count)
                .filter(|gate| grid.scaled_value(row, *gate).is_some())
                .count()
        })
        .sum()
}
