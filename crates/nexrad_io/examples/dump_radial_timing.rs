//! Measure how a sweep's radials are laid out in azimuth and in time.
//!
//! A live sweep animation can only be driven by real radial timing if that
//! timing is actually present and sane. This prints, per cut: the azimuth span
//! covered, the elapsed time across the sweep, the implied sweep rate, and the
//! distribution of gaps between consecutive radials - which is where a chunked
//! feed shows its structure, because radials do not arrive one at a time but in
//! bursts as each chunk lands.
//!
//! ```text
//! cargo run --release -p nexrad_io --example dump_radial_timing -- <LEVEL2_FILE>
//! ```

use std::path::PathBuf;

use radar_core::ElevationCut;

fn main() {
    let Some(path) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: dump_radial_timing <LEVEL2_FILE>");
        std::process::exit(2);
    };

    let volume = match nexrad_io::decode_volume_from_path(&path) {
        Ok(volume) => volume,
        Err(error) => {
            eprintln!("could not decode {}: {error}", path.display());
            std::process::exit(1);
        }
    };

    println!("file  {}", path.display());
    println!(
        "site  {}  {}",
        volume.site.id,
        volume.volume_time.to_rfc3339()
    );
    println!();
    println!(
        "{:>3}  {:>7}  {:>8}  {:>9}  {:>10}  {:>9}  {:>9}  {:>9}  {:>8}",
        "idx",
        "radials",
        "az_span",
        "elapsed_s",
        "deg_per_s",
        "gap_p50",
        "gap_p90",
        "gap_max",
        "monotonic"
    );

    for (index, cut) in volume.cuts.iter().enumerate() {
        if cut.radials.len() < 2 {
            println!("{index:>3}  {:>7}  (too few radials)", cut.radials.len());
            continue;
        }
        let first = cut.radials.first().expect("checked above");
        let last = cut.radials.last().expect("checked above");
        let elapsed_ms = last.time_offset_ms - first.time_offset_ms;
        let elapsed_s = elapsed_ms as f32 / 1000.0;

        let az_span = azimuth_span_deg(cut);
        let deg_per_s = if elapsed_s > 0.0 {
            az_span / elapsed_s
        } else {
            f32::NAN
        };

        // Gaps between consecutive radials, in milliseconds. A steady antenna
        // gives a tight cluster; a chunked feed gives many near-zero gaps and a
        // few large ones where the next chunk landed.
        let mut gaps: Vec<i32> = cut
            .radials
            .windows(2)
            .map(|pair| pair[1].time_offset_ms - pair[0].time_offset_ms)
            .collect();
        let monotonic = gaps.iter().all(|gap| *gap >= 0);
        gaps.sort_unstable();
        let percentile = |fraction: f32| -> i32 {
            let position = ((gaps.len() - 1) as f32 * fraction).round() as usize;
            gaps[position]
        };

        println!(
            "{index:>3}  {:>7}  {az_span:>7.1}d  {elapsed_s:>9.2}  {deg_per_s:>10.1}  \
             {:>8}ms  {:>8}ms  {:>8}ms  {:>8}",
            cut.radials.len(),
            percentile(0.5),
            percentile(0.9),
            gaps.last().copied().unwrap_or_default(),
            if monotonic { "yes" } else { "NO" },
        );
    }

    println!();
    print_chunk_structure(&volume);
}

/// Degrees of azimuth the sweep covered: the circle minus its largest hole.
fn azimuth_span_deg(cut: &ElevationCut) -> f32 {
    let mut azimuths: Vec<f32> = cut
        .radials
        .iter()
        .map(|radial| radial.azimuth_deg.rem_euclid(360.0))
        .filter(|azimuth| azimuth.is_finite())
        .collect();
    if azimuths.len() < 2 {
        return 0.0;
    }
    azimuths.sort_by(f32::total_cmp);
    let mut largest_gap = azimuths[0] + 360.0 - azimuths[azimuths.len() - 1];
    for pair in azimuths.windows(2) {
        largest_gap = largest_gap.max(pair[1] - pair[0]);
    }
    (360.0 - largest_gap).max(0.0)
}

/// Look at the last (newest) cut in detail, since that is the one a live
/// animation would be revealing.
fn print_chunk_structure(volume: &radar_core::RadarVolume) {
    let Some((index, cut)) = volume.cuts.iter().enumerate().next_back() else {
        return;
    };
    if cut.radials.len() < 4 {
        return;
    }
    println!("newest cut #{index}: azimuth walk of the first 24 radials");
    for radial in cut.radials.iter().take(24) {
        print!("{:.1} ", radial.azimuth_deg);
    }
    println!();
    println!("last 8 radials");
    for radial in cut
        .radials
        .iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        print!("{:.1}@{}ms ", radial.azimuth_deg, radial.time_offset_ms);
    }
    println!();

    // Where do the large inter-radial gaps sit? Those are chunk boundaries.
    let mut boundaries = Vec::new();
    for (position, pair) in cut.radials.windows(2).enumerate() {
        let gap = pair[1].time_offset_ms - pair[0].time_offset_ms;
        if gap > 200 {
            boundaries.push((position, gap, pair[1].azimuth_deg));
        }
    }
    println!(
        "inter-radial gaps over 200 ms: {} (radial index, gap, azimuth)",
        boundaries.len()
    );
    for (position, gap, azimuth) in boundaries.iter().take(12) {
        println!("  #{position} {gap}ms at {azimuth:.1} deg");
    }
}
