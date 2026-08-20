//! Render one real volume through a set of gate filters and report what each
//! one hid.
//!
//! This is the proof tool for [`render2d::gate_filter`]. It writes a PNG per
//! case so the pictures can be LOOKED at, and prints the gate counts beside
//! them so the pictures can be checked against a number. It also times the
//! raster build with the filter off and with every criterion on, which is what
//! tells a UI whether a live threshold slider needs debouncing.
//!
//! It also does the accounting that a gate count on its own cannot do: every
//! filtered raster is compared PIXEL BY PIXEL with the unfiltered raster of the
//! same sweep, and the run fails if any pixel was recoloured or appeared rather
//! than removed. That check is here rather than only in the test suite because
//! the failure it catches - a censored gate falling through to the beam beside
//! it - needs a sweep whose radials are half a degree apart to show up at all,
//! and the surest such sweep is a real one.
//!
//! ```text
//! cargo run --release -p render2d --example gate_filter_probe -- <volume> <out-dir>
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use color_tables::ColorTableSet;
use radar_core::MomentType;
use render2d::{
    DisplayQuality, GateFilter, GateFilterReport, RasterOptions, ViewportMomentCache,
    ViewportRasterOptions, render_moment_image_filtered, resolve_companion_sweep,
    viewport_rgba_buffer_len,
};

const TIMED_RUNS: usize = 9;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: cargo run --release -p render2d --example gate_filter_probe -- <level2-file> <out-dir>"
        );
        std::process::exit(2);
    };
    std::fs::create_dir_all(&out_dir)?;

    let volume = nexrad_io::decode_volume_from_path(&input)?;
    println!(
        "site={} time={} vcp={:?} cuts={}",
        volume.site.id,
        volume.volume_time,
        volume.vcp.as_ref().map(|vcp| vcp.pattern),
        volume.cuts.len()
    );

    println!("\n-- companion sweep resolution --");
    for cut_index in 0..volume.cuts.len() {
        let cut = &volume.cuts[cut_index];
        let reflectivity = resolve_companion_sweep(&volume, cut_index, &MomentType::Reflectivity);
        let correlation =
            resolve_companion_sweep(&volume, cut_index, &MomentType::CorrelationCoefficient);
        println!(
            "cut {cut_index:>2} elev={:>5.2} REF<-{reflectivity:?} RHO<-{correlation:?}",
            cut.elevation_deg
        );
    }

    println!("\n-- rho_HV distribution inside the bloom (cut 0, 5-35 km) --");
    print_correlation_histogram(&volume, 0, 5.0, 35.0);
    println!("-- rho_HV distribution in the southern shield (cut 0, 60-120 km) --");
    print_correlation_histogram(&volume, 0, 60.0, 120.0);

    let cases: Vec<(&str, usize, MomentType, GateFilter)> = vec![
        ("ref_cut0_off", 0, MomentType::Reflectivity, GateFilter::OFF),
        (
            "ref_cut0_min5",
            0,
            MomentType::Reflectivity,
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                ..GateFilter::OFF
            },
        ),
        (
            "ref_cut0_min20",
            0,
            MomentType::Reflectivity,
            GateFilter {
                min_reflectivity_dbz: Some(20.0),
                ..GateFilter::OFF
            },
        ),
        ("vel_cut1_off", 1, MomentType::Velocity, GateFilter::OFF),
        (
            "vel_cut1_needs_ref5",
            1,
            MomentType::Velocity,
            GateFilter {
                velocity_requires_reflectivity_dbz: Some(5.0),
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_needs_ref20",
            1,
            MomentType::Velocity,
            GateFilter {
                velocity_requires_reflectivity_dbz: Some(20.0),
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_rho080",
            1,
            MomentType::Velocity,
            GateFilter {
                min_correlation: Some(0.80),
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_rho095",
            1,
            MomentType::Velocity,
            GateFilter {
                min_correlation: Some(0.95),
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_no_rangefolded",
            1,
            MomentType::Velocity,
            GateFilter {
                hide_range_folded: true,
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_beyond40km",
            1,
            MomentType::Velocity,
            GateFilter {
                min_range_km: Some(40.0),
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_rho080_and_beyond40km",
            1,
            MomentType::Velocity,
            GateFilter {
                min_correlation: Some(0.80),
                min_range_km: Some(40.0),
                ..GateFilter::OFF
            },
        ),
        (
            "vel_cut1_all",
            1,
            MomentType::Velocity,
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                velocity_requires_reflectivity_dbz: Some(5.0),
                min_correlation: Some(0.80),
                hide_range_folded: true,
                min_range_km: Some(40.0),
            },
        ),
        (
            "ref_cut0_rho095",
            0,
            MomentType::Reflectivity,
            GateFilter {
                min_correlation: Some(0.95),
                ..GateFilter::OFF
            },
        ),
    ];

    println!("\n-- cases --");
    let mut unfiltered: Vec<(usize, MomentType, Vec<u8>)> = Vec::new();
    for (name, cut_index, moment, filter) in &cases {
        let out = Path::new(&out_dir).join(format!("{name}.png"));
        let rendered = render_moment_image_filtered(
            &volume,
            *cut_index,
            moment.clone(),
            RasterOptions::default(),
            None,
            filter,
        )?;
        let pixels = rendered.image.as_raw().clone();
        rendered.image.save(&out)?;
        print_case(name, filter, &rendered.report);
        print_zone_breakdown(&volume, *cut_index, moment, filter);

        // Pixel accounting against the unfiltered raster of the same sweep.
        //
        // The gate counts above say what the FILTER selected; this says what
        // the PICTURE did with it, and the two are not the same question. A
        // 0.1 degree raster bin can hold two radials, so a censor that merely
        // blanks a gate lets the pixel fall through to the beam beside it and
        // come back a different colour - which is echo from an azimuth the
        // analyst did not ask about, sitting under a badge that says the gate
        // was removed. Only `removed` may be non-zero.
        match unfiltered
            .iter()
            .find(|(cut, held, _)| cut == cut_index && held == moment)
        {
            None => unfiltered.push((*cut_index, moment.clone(), pixels)),
            Some((_, _, before)) => {
                let (removed, recoloured, appeared) = pixel_accounting(before, &pixels);
                println!(
                    "{:<32}   pixels removed={removed} recoloured={recoloured} appeared={appeared}",
                    ""
                );
                assert_eq!(
                    (recoloured, appeared),
                    (0, 0),
                    "{name}: a censor may only remove pixels"
                );
            }
        }
    }

    println!("\n-- raster build timings (median of {TIMED_RUNS}) --");
    for (label, cut_index, moment, filter) in [
        (
            "cut0 REF filter=OFF",
            0,
            MomentType::Reflectivity,
            GateFilter::OFF,
        ),
        (
            "cut0 REF filter=ALL",
            0,
            MomentType::Reflectivity,
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                velocity_requires_reflectivity_dbz: Some(5.0),
                min_correlation: Some(0.80),
                hide_range_folded: true,
                min_range_km: Some(40.0),
            },
        ),
        (
            "cut1 VEL filter=OFF",
            1,
            MomentType::Velocity,
            GateFilter::OFF,
        ),
        (
            "cut1 VEL filter=ALL",
            1,
            MomentType::Velocity,
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                velocity_requires_reflectivity_dbz: Some(5.0),
                min_correlation: Some(0.80),
                hide_range_folded: true,
                min_range_km: Some(40.0),
            },
        ),
        (
            "cut1 VEL filter=RHO0.80",
            1,
            MomentType::Velocity,
            GateFilter {
                min_correlation: Some(0.80),
                ..GateFilter::OFF
            },
        ),
    ] {
        let mut timings = Vec::with_capacity(TIMED_RUNS);
        for _ in 0..TIMED_RUNS {
            let start = Instant::now();
            let rendered = render_moment_image_filtered(
                &volume,
                cut_index,
                moment.clone(),
                RasterOptions::default(),
                None,
                &filter,
            )?;
            timings.push(start.elapsed());
            std::hint::black_box(rendered.image.as_raw().len());
        }
        timings.sort();
        println!(
            "{label:<26} median_ms={:.3} best_ms={:.3} worst_ms={:.3}",
            elapsed_ms(timings[timings.len() / 2]),
            elapsed_ms(timings[0]),
            elapsed_ms(timings[timings.len() - 1])
        );
    }

    // What the workstation actually pays. A pane rebuilds its
    // `ViewportMomentCache` when the volume, cut, product, quality or FILTER
    // changes, and reuses it for every pan and zoom in between - so this, not
    // the per-frame raster, is the cost of moving a threshold slider.
    println!("\n-- viewport cache rebuild (median of {TIMED_RUNS}) --");
    let tables = ColorTableSet::default();
    for (label, cut_index, moment, quality, filter) in [
        (
            "cut1 VEL smooth OFF",
            1,
            MomentType::Velocity,
            DisplayQuality::SMOOTH,
            GateFilter::OFF,
        ),
        (
            "cut1 VEL smooth RHO0.80",
            1,
            MomentType::Velocity,
            DisplayQuality::SMOOTH,
            GateFilter {
                min_correlation: Some(0.80),
                ..GateFilter::OFF
            },
        ),
        (
            "cut1 VEL smooth ALL",
            1,
            MomentType::Velocity,
            DisplayQuality::SMOOTH,
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                velocity_requires_reflectivity_dbz: Some(5.0),
                min_correlation: Some(0.80),
                hide_range_folded: true,
                min_range_km: Some(40.0),
            },
        ),
        (
            "cut1 VEL native OFF",
            1,
            MomentType::Velocity,
            DisplayQuality::NATIVE,
            GateFilter::OFF,
        ),
        (
            "cut1 VEL native ALL",
            1,
            MomentType::Velocity,
            DisplayQuality::NATIVE,
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                velocity_requires_reflectivity_dbz: Some(5.0),
                min_correlation: Some(0.80),
                hide_range_folded: true,
                min_range_km: Some(40.0),
            },
        ),
    ] {
        let mut timings = Vec::with_capacity(TIMED_RUNS);
        for _ in 0..TIMED_RUNS {
            let start = Instant::now();
            let cache = ViewportMomentCache::new_display_quality_filtered(
                &volume,
                cut_index,
                moment.clone(),
                &tables,
                quality,
                &filter,
            )?;
            timings.push(start.elapsed());
            std::hint::black_box(cache.gate_filter_report().gates_hidden);
        }
        timings.sort();
        println!(
            "{label:<26} median_ms={:.3} best_ms={:.3}",
            elapsed_ms(timings[timings.len() / 2]),
            elapsed_ms(timings[0])
        );
    }

    // The path the workstation actually draws through, on the same real sweep.
    //
    // The plain raster above censors a grid whose lattice is the one the filter
    // was evaluated against. The display-quality path does not: it censors
    // first and then softens and upsamples, which is the right order - a bloom
    // gate must not contribute its value to the neighbours that survive it -
    // and which leaves the mask indexing a grid that no longer exists. So the
    // censor is carried across by running the quality passes over the clean
    // sweep as well and taking the gates that went absent between the two.
    //
    // What that has to produce, and what is checked here: nothing appears, and
    // a pixel that changes COLOUR is next to something that was removed. Values
    // next to a removed gate legitimately change - that is the halo being taken
    // out - but a pixel far from any removal changing colour means the censor
    // fell through to another beam, or the candidate ranking moved.
    println!("\n-- viewport display-quality path --");
    let options = ViewportRasterOptions {
        width: 800,
        height: 800,
        radar_x_px: 400.0,
        radar_y_px: 400.0,
        km_per_px_x: 0.25,
        km_per_px_y: 0.25,
    };
    for (label, quality) in [
        ("native", DisplayQuality::NATIVE),
        ("smooth", DisplayQuality::SMOOTH),
        ("high", DisplayQuality::HIGH),
    ] {
        let mut frames = Vec::new();
        for (name, filter) in [
            ("off", GateFilter::OFF),
            (
                "rho080",
                GateFilter {
                    min_correlation: Some(0.80),
                    ..GateFilter::OFF
                },
            ),
        ] {
            let cache = ViewportMomentCache::new_display_quality_filtered(
                &volume,
                1,
                MomentType::Velocity,
                &tables,
                quality,
                &filter,
            )?;
            let mut pixels = vec![0; viewport_rgba_buffer_len(options)];
            cache.render_moment_rgba_into(&volume, options, &mut pixels)?;
            let out = Path::new(&out_dir).join(format!("viewport_vel_cut1_{label}_{name}.png"));
            image::RgbaImage::from_raw(options.width, options.height, pixels.clone())
                .expect("RGBA buffer matches the viewport")
                .save(&out)?;
            frames.push(pixels);
        }

        let (removed, recoloured, appeared) = pixel_accounting(&frames[0], &frames[1]);
        let stray = stray_recolour(&frames[0], &frames[1], options.width as usize, 4);
        println!(
            "{label:<8} pixels removed={removed} recoloured={recoloured} appeared={appeared} \
             recoloured_far_from_any_removal={stray}"
        );
        assert_eq!(appeared, 0, "{label}: a censor conjured a pixel");
        assert_eq!(
            stray, 0,
            "{label}: a pixel changed colour nowhere near anything the filter removed"
        );
        if !quality.soften && !quality.interpolate {
            assert_eq!(
                recoloured, 0,
                "{label}: nothing resamples here, so nothing may change colour"
            );
        }
    }

    Ok(())
}

/// Pixels that changed colour with no removed pixel within `radius` of them.
///
/// Softening and interpolation reach one gate; a fall-through to another beam,
/// or a re-ranked azimuth bin, reaches anywhere. This tells the two apart.
fn stray_recolour(before: &[u8], after: &[u8], width: usize, radius: i32) -> usize {
    let height = before.len() / 4 / width;
    let mut removed = vec![false; width * height];
    let mut recoloured = Vec::new();
    for (index, (before, after)) in before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .enumerate()
    {
        if before == after {
            continue;
        }
        if before[3] != 0 && after[3] == 0 {
            removed[index] = true;
        } else {
            recoloured.push((index % width, index / width));
        }
    }

    recoloured
        .into_iter()
        .filter(|(x, y)| {
            !(-radius..=radius).any(|dy| {
                (-radius..=radius).any(|dx| {
                    let nx = *x as i32 + dx;
                    let ny = *y as i32 + dy;
                    nx >= 0
                        && ny >= 0
                        && (nx as usize) < width
                        && (ny as usize) < height
                        && removed[ny as usize * width + nx as usize]
                })
            })
        })
        .count()
}

/// `(removed, recoloured, appeared)` between two rasters of the same sweep.
///
/// Removed: was opaque, is now transparent - the only thing a censor may do.
/// Recoloured: was one opaque colour, is now a different one. Appeared: was
/// transparent, is now opaque.
fn pixel_accounting(before: &[u8], after: &[u8]) -> (usize, usize, usize) {
    let mut removed = 0;
    let mut recoloured = 0;
    let mut appeared = 0;
    for (before, after) in before.chunks_exact(4).zip(after.chunks_exact(4)) {
        if before == after {
            continue;
        }
        match (before[3] == 0, after[3] == 0) {
            (false, true) => removed += 1,
            (true, false) => appeared += 1,
            _ => recoloured += 1,
        }
    }
    (removed, recoloured, appeared)
}

fn print_case(name: &str, filter: &GateFilter, report: &GateFilterReport) {
    let summary = filter.hidden_summary();
    let summary = if summary.is_empty() {
        "<off>".to_owned()
    } else {
        summary
    };
    println!(
        "{name:<32} [{summary}] visible={} hidden={} ({:.2}%) ref={} vel_ref={} rho={} rf={} range={} unknown_ref={} unknown_rho={}",
        report.gates_visible,
        report.gates_hidden,
        report.hidden_fraction() * 100.0,
        report.hidden_by_min_reflectivity,
        report.hidden_by_velocity_reflectivity,
        report.hidden_by_min_correlation,
        report.hidden_by_range_folded,
        report.hidden_by_min_range,
        report.kept_unknown_reflectivity,
        report.kept_unknown_correlation,
    );
    for note in report.notes() {
        println!("{:<32}   {note}", "");
    }
}

/// How much of the near-radar bloom, and how much of the far field, one filter
/// removed.
///
/// The picture says "the bloom cleared"; this says by how much, and says the
/// same thing about the ranges where the real storms are, so "it cleared the
/// bloom and left the weather" is a claim with two numbers behind it rather
/// than an impression.
fn print_zone_breakdown(
    volume: &radar_core::RadarVolume,
    cut_index: usize,
    moment: &MomentType,
    filter: &GateFilter,
) {
    if !filter.is_active() {
        return;
    }
    let Some(cut) = volume.cuts.get(cut_index) else {
        return;
    };
    let Some(grid) = cut.moments.get(moment) else {
        return;
    };
    let outcome = render2d::evaluate_gate_filter(volume, cut_index, grid, filter);
    let Some(mask) = outcome.mask.as_ref() else {
        return;
    };

    for (label, near_km, far_km) in [
        ("bloom 5-35km", 5.0, 35.0),
        ("storms 60-160km", 60.0, 160.0),
    ] {
        let mut visible = 0_usize;
        let mut hidden = 0_usize;
        for row in 0..mask.rows() {
            for gate in 0..mask.gate_count() {
                let range_km = (grid.gate_range.first_gate_m as f32
                    + gate as f32 * grid.gate_range.gate_spacing_m as f32)
                    / 1_000.0;
                if range_km < near_km || range_km > far_km {
                    continue;
                }
                if !is_visible(grid, row, gate) {
                    continue;
                }
                visible += 1;
                if mask.hides(row, gate) {
                    hidden += 1;
                }
            }
        }
        let percent = if visible == 0 {
            0.0
        } else {
            hidden as f32 / visible as f32 * 100.0
        };
        println!(
            "{:<32}   {label}: visible={visible} hidden={hidden} ({percent:.1}%)",
            ""
        );
    }
}

/// A gate the raster would have drawn something for: a value, or the
/// range-folded colour.
fn is_visible(grid: &radar_core::MomentGrid, row: usize, gate: usize) -> bool {
    if grid.scaled_value(row, gate).is_some() {
        return true;
    }
    let Some(folded) = grid.range_folded else {
        return false;
    };
    let index = row * grid.gate_range.gate_count + gate;
    match &grid.storage {
        radar_core::MomentStorage::U8(values) => {
            values.get(index).map(|value| u16::from(*value)) == Some(folded)
        }
        radar_core::MomentStorage::U16(values) => values.get(index).copied() == Some(folded),
        radar_core::MomentStorage::F32(_) => false,
    }
}

fn print_correlation_histogram(
    volume: &radar_core::RadarVolume,
    cut_index: usize,
    near_km: f32,
    far_km: f32,
) {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return;
    };
    let Some(grid) = cut.moments.get(&MomentType::CorrelationCoefficient) else {
        println!("  (cut {cut_index} carries no rho_HV)");
        return;
    };
    let edges = [0.0_f32, 0.5, 0.7, 0.8, 0.9, 0.95, 0.97, 1.01, f32::INFINITY];
    let mut bins = vec![0_usize; edges.len()];
    let mut total = 0_usize;
    for row in 0..grid.radial_count() {
        for gate in 0..grid.gate_range.gate_count {
            let range_km = (grid.gate_range.first_gate_m as f32
                + gate as f32 * grid.gate_range.gate_spacing_m as f32)
                / 1_000.0;
            if range_km < near_km || range_km > far_km {
                continue;
            }
            let Some(rho) = grid.scaled_value(row, gate) else {
                continue;
            };
            total += 1;
            let bin = edges.iter().position(|edge| rho < *edge).unwrap_or(0);
            bins[bin] += 1;
        }
    }
    if total == 0 {
        println!("  (no rho_HV gates in {near_km}-{far_km} km)");
        return;
    }
    let mut text = format!("  n={total}");
    let mut previous = 0.0_f32;
    for (index, edge) in edges.iter().enumerate() {
        if index == 0 {
            continue;
        }
        let count = bins[index];
        if count > 0 {
            text.push_str(&format!(
                "  [{previous:.2},{:.2})={:.1}%",
                edge.min(1.1),
                count as f32 / total as f32 * 100.0
            ));
        }
        previous = edge.min(1.1);
    }
    println!("{text}");
}

fn elapsed_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
