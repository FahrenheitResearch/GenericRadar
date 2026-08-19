//! Print the cut structure of a real Level II volume.
//!
//! Split-cut selection cannot be argued about from a screenshot: a header that
//! reads `0.6 deg` is a rounded label, and both legs of a split cut round to
//! nearly the same number. This dumps what is actually in the file so a claim
//! about which leg a product opened on can be checked against the data.
//!
//! It prints two elevations per cut, and the difference between them is the
//! point. `stored` is `ElevationCut::elevation_deg`, which is whatever the
//! radial that created the cut reported - normally the sweep's first radial,
//! taken while the antenna is still ramping onto the commanded tilt. `median`
//! is the median over every radial in the sweep. On real volumes the stored
//! value is biased low by up to half a degree, which is enough to put the two
//! legs of one split cut into different nominal elevation groups.
//!
//! ```text
//! cargo run --release -p nexrad_io --example dump_cuts -- <LEVEL2_FILE>
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use radar_core::{ElevationCut, MomentType, RadarVolume};

/// Two cuts whose nominal elevations differ by less than this are the same
/// commanded tilt, scanned more than once: the two legs of a split cut, or the
/// repeated low-level scans of a SAILS volume.
const NOMINAL_TOLERANCE_DEG: f32 = 0.15;

fn main() {
    let Some(path) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: dump_cuts <LEVEL2_FILE>");
        std::process::exit(2);
    };

    let volume = match nexrad_io::decode_volume_from_path(&path) {
        Ok(volume) => volume,
        Err(error) => {
            eprintln!("could not decode {}: {error}", path.display());
            std::process::exit(1);
        }
    };

    println!("file    {}", path.display());
    println!("site    {}", volume.site.id);
    println!("time    {}", volume.volume_time.to_rfc3339());
    println!(
        "vcp     {}",
        volume
            .vcp
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), |vcp| format!("{:?}", vcp.pattern))
    );
    println!("cuts    {}", volume.cuts.len());
    println!();
    println!(
        "{:>3}  {:>7}  {:>7}  {:>7}  {:>6}  {:>7}  {:>9}  {:>8}  {:>10}  moments",
        "idx", "stored", "median", "spread", "elnum", "radials", "nyquist", "t_ms", "max_range"
    );

    for (index, cut) in volume.cuts.iter().enumerate() {
        let moments = moment_summary(cut.moments.keys());
        let nyquist = median_nyquist(cut)
            .map_or_else(|| "-".to_owned(), |nyquist| format!("{nyquist:.1} m/s"));
        println!(
            "{index:>3}  {:>6.2}d  {:>6.2}d  {:>6.2}d  {:>6}  {:>7}  {nyquist:>9}  {:>8}  {:>7.1} km  {moments}",
            cut.elevation_deg,
            median_elevation_deg(cut).unwrap_or(f32::NAN),
            elevation_spread_deg(cut).unwrap_or(f32::NAN),
            cut.elevation_number
                .map_or_else(|| "-".to_owned(), |number| number.to_string()),
            cut.radials.len(),
            median_radial_time_ms(cut).unwrap_or(0),
            max_range_km(cut),
        );
    }

    println!();
    print_grouping(&volume, "STORED first-radial elevation", |cut| {
        cut.elevation_deg
    });
    println!();
    print_grouping(&volume, "MEDIAN over-sweep elevation", |cut| {
        median_elevation_deg(cut).unwrap_or(cut.elevation_deg)
    });
    println!();
    print_index_order_selection(&volume);
}

fn moment_summary<'a>(moments: impl Iterator<Item = &'a MomentType>) -> String {
    moments
        .map(MomentType::short_name)
        .collect::<Vec<_>>()
        .join(",")
}

/// The median elevation over every radial in the sweep.
///
/// The median rather than the mean: the antenna ramp at the start of a sweep is
/// a run of outliers all on one side, and a mean would be dragged toward them.
fn median_elevation_deg(cut: &ElevationCut) -> Option<f32> {
    median_of(cut.radials.iter().map(|radial| radial.elevation_deg))
}

fn elevation_spread_deg(cut: &ElevationCut) -> Option<f32> {
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for radial in &cut.radials {
        if radial.elevation_deg.is_finite() {
            minimum = minimum.min(radial.elevation_deg);
            maximum = maximum.max(radial.elevation_deg);
        }
    }
    (minimum <= maximum).then_some(maximum - minimum)
}

fn median_radial_time_ms(cut: &ElevationCut) -> Option<i32> {
    let mut values: Vec<i32> = cut
        .radials
        .iter()
        .map(|radial| radial.time_offset_ms)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

fn median_nyquist(cut: &ElevationCut) -> Option<f32> {
    median_of(
        cut.radials
            .iter()
            .filter_map(|radial| radial.nyquist_velocity_mps),
    )
}

fn median_of(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut values: Vec<f32> = values
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    Some(values[values.len() / 2])
}

fn max_range_km(cut: &ElevationCut) -> f32 {
    cut.moments
        .values()
        .map(|grid| {
            let gates = grid.gate_range.gate_count as f32;
            (grid.gate_range.first_gate_m as f32 + gates * grid.gate_range.gate_spacing_m as f32)
                / 1000.0
        })
        .fold(0.0_f32, f32::max)
}

/// Group cuts by nominal elevation under a chosen angle, and report which
/// groups hold more than one leg.
fn print_grouping(volume: &RadarVolume, label: &str, angle: impl Fn(&ElevationCut) -> f32) {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, cut) in volume.cuts.iter().enumerate() {
        let elevation = angle(cut);
        let existing = groups.iter_mut().find(|group| {
            group.first().is_some_and(|first| {
                (angle(&volume.cuts[*first]) - elevation).abs() <= NOMINAL_TOLERANCE_DEG
            })
        });
        match existing {
            Some(group) => group.push(index),
            None => groups.push(vec![index]),
        }
    }

    let multi = groups.iter().filter(|group| group.len() > 1).count();
    println!("grouping by {label} (within {NOMINAL_TOLERANCE_DEG} deg): {multi} multi-leg groups");
    for group in &groups {
        let members: Vec<String> = group
            .iter()
            .map(|index| {
                let cut = &volume.cuts[*index];
                format!(
                    "#{index} {:.2}d [{}]",
                    angle(cut),
                    moment_summary(cut.moments.keys())
                )
            })
            .collect();
        let marker = if group.len() > 1 { "MULTI" } else { "     " };
        println!("  {marker} {}", members.join("  |  "));
    }
}

/// What selecting the first cut carrying a moment would pick, and what the
/// freshest cut carrying it would have been instead.
fn print_index_order_selection(volume: &RadarVolume) {
    println!("index-order selection versus freshest available:");
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
    ] {
        let carrying: Vec<usize> = volume
            .cuts
            .iter()
            .enumerate()
            .filter(|(_, cut)| cut.moments.contains_key(&moment))
            .map(|(index, _)| index)
            .collect();
        let Some(&first) = carrying.first() else {
            println!("  {:<4} -> not present", moment.short_name());
            continue;
        };

        // The lowest nominal tilt that carries this moment, and every cut that
        // scanned it. On a SAILS volume that is three or four separate sweeps
        // minutes apart, and index order always takes the oldest.
        let lowest = carrying
            .iter()
            .map(|index| median_elevation_deg(&volume.cuts[*index]).unwrap_or(f32::MAX))
            .fold(f32::MAX, f32::min);
        let same_tilt: Vec<usize> = carrying
            .iter()
            .copied()
            .filter(|index| {
                (median_elevation_deg(&volume.cuts[*index]).unwrap_or(f32::MAX) - lowest).abs()
                    <= NOMINAL_TOLERANCE_DEG
            })
            .collect();
        let freshest = same_tilt
            .iter()
            .copied()
            .max_by_key(|index| median_radial_time_ms(&volume.cuts[*index]).unwrap_or(i32::MIN))
            .unwrap_or(first);
        let stale_ms = median_radial_time_ms(&volume.cuts[freshest]).unwrap_or(0)
            - median_radial_time_ms(&volume.cuts[first]).unwrap_or(0);
        let verdict = if freshest == first {
            "ok".to_owned()
        } else {
            format!(
                "STALE by {:.1} s (freshest is #{freshest})",
                stale_ms as f32 / 1000.0
            )
        };
        println!(
            "  {:<4} -> cut #{first} at {:.2}d   [{} cuts at this tilt]  {verdict}",
            moment.short_name(),
            median_elevation_deg(&volume.cuts[first]).unwrap_or(f32::NAN),
            same_tilt.len(),
        );
    }

    let counts: BTreeMap<&str, usize> = volume.cuts.iter().flat_map(|cut| cut.moments.keys()).fold(
        BTreeMap::new(),
        |mut counts, moment| {
            *counts.entry(moment.short_name()).or_default() += 1;
            counts
        },
    );
    println!();
    println!("cuts carrying each moment: {counts:?}");
}
