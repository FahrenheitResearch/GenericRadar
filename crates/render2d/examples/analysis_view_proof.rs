//! The analysis view, rendered from a REAL Level II volume and looked at.
//!
//! Run it:
//!
//! ```text
//! cargo run --release -p render2d --example analysis_view_proof -- \
//!     <volume-file> <output-dir> [--rotation-deg D] [--bench RUNS]
//! ```
//!
//! It writes, for reflectivity on cut 0 and velocity on cut 1, both a PNG to
//! look at and the raw RGBA bytes to hash. The raw bytes are the point: the
//! promise this branch makes is that an unrotated pane is BYTE-IDENTICAL to
//! what the renderer produced before the camera rotation existed, and a
//! picture that is merely indistinguishable is not that.
//!
//! `--rotation-deg` renders the same volume through a rotated camera, which is
//! how a rotated echo gets drawn at all before anyone claims it registers.
//! `--bench` runs an interleaved A/B/A' - unrotated, rotated, unrotated again -
//! so the third arm measures the noise floor the first two are compared over.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use radar_core::{MomentType, RadarVolume};
use render2d::{DisplayQuality, ViewportMomentCache, ViewportRasterOptions};

/// The pane the workstation opens with on a 1600x900 window.
const WIDTH_PX: u32 = 1600;
const HEIGHT_PX: u32 = 900;
/// `analyst_runtime::DEFAULT_KM_PER_POINT`, restated: this crate does not
/// depend on the runtime, and this probe must not be the reason it starts to.
const DEFAULT_KM_PER_PX: f32 = 0.35;

struct Config {
    input: PathBuf,
    output: PathBuf,
    rotation_deg: f32,
    bench_runs: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = parse_args()?;
    let raw = std::fs::read(&config.input)?;
    let volume = nexrad_io::decode_volume_from_bytes(&raw)?;
    std::fs::create_dir_all(&config.output)?;

    println!(
        "volume {} cuts={} rotation_deg={}",
        config.input.display(),
        volume.cuts.len(),
        config.rotation_deg
    );

    for (label, moment, cut_index) in [
        ("ref-cut0", MomentType::Reflectivity, 0_usize),
        ("vel-cut1", MomentType::Velocity, 1_usize),
    ] {
        let Some(options) = options_at(config.rotation_deg) else {
            continue;
        };
        let (rgba, width, height) = match render(&volume, cut_index, moment.clone(), options) {
            Ok(frame) => frame,
            Err(error) => {
                println!("{label}: SKIPPED ({error})");
                continue;
            }
        };
        let suffix = if config.rotation_deg == 0.0 {
            String::new()
        } else {
            format!("-rot{}", config.rotation_deg)
        };
        let stem = format!("{label}{suffix}");
        write_raw(&config.output, &stem, &rgba)?;
        write_png(&config.output, &stem, &rgba, width, height)?;
        println!(
            "{stem}: {width}x{height} bytes={} fnv1a64={:016x} painted={}",
            rgba.len(),
            fnv1a64(&rgba),
            painted_pixels(&rgba)
        );
    }

    if config.bench_runs > 0 {
        bench(&volume, &config)?;
    }
    Ok(())
}

/// The viewport the pane hands the renderer: the antenna in the middle of a
/// 1600x900 pane at the default scale, which is the analysis view.
fn options_at(rotation_deg: f32) -> Option<ViewportRasterOptions> {
    Some(ViewportRasterOptions {
        width: WIDTH_PX,
        height: HEIGHT_PX,
        radar_x_px: WIDTH_PX as f32 * 0.5,
        radar_y_px: HEIGHT_PX as f32 * 0.5,
        km_per_px_x: DEFAULT_KM_PER_PX,
        km_per_px_y: DEFAULT_KM_PER_PX,
        rotation_rad: rotation_deg.to_radians(),
    })
}

fn render(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
) -> Result<(Vec<u8>, u32, u32), Box<dyn std::error::Error>> {
    let tables = color_tables::ColorTableSet::default();
    let quality = DisplayQuality::default();
    let cache =
        ViewportMomentCache::new_display_quality(volume, cut_index, moment, &tables, quality)?;
    let mut rgba =
        vec![0_u8; render2d::quality::quality_rgba_buffer_len(options, quality.supersample)];
    let (width, height) = render2d::quality::render_moment_viewport_quality_rgba_into(
        &cache,
        volume,
        options,
        quality.supersample,
        &mut rgba,
    )?;
    Ok((rgba, width, height))
}

/// Interleaved A / B / A', one process, alternating on every run so a machine
/// that warms up or throttles does it to both arms equally. A and A' are the
/// SAME work, so their difference is the noise floor everything else is read
/// against.
fn bench(volume: &RadarVolume, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let tables = color_tables::ColorTableSet::default();
    let quality = DisplayQuality::default();
    let unrotated = options_at(0.0).expect("unrotated options");
    let rotated = options_at(config.rotation_deg).expect("rotated options");
    for (label, moment, cut_index) in [
        ("REF cut0", MomentType::Reflectivity, 0_usize),
        ("VEL cut1", MomentType::Velocity, 1_usize),
    ] {
        let Ok(cache) = ViewportMomentCache::new_display_quality(
            volume,
            cut_index,
            moment.clone(),
            &tables,
            quality,
        ) else {
            continue;
        };
        let mut rgba =
            vec![0_u8; render2d::quality::quality_rgba_buffer_len(unrotated, quality.supersample)];
        let mut scratch = Vec::new();
        let mut arms = [Vec::new(), Vec::new(), Vec::new()];
        // One warm-up of each arm before anything is recorded.
        for options in [unrotated, rotated, unrotated] {
            render2d::quality::render_moment_viewport_quality_rgba_into_with_scratch(
                &cache,
                volume,
                options,
                quality.supersample,
                &mut scratch,
                &mut rgba,
            )?;
        }
        for _ in 0..config.bench_runs {
            for (arm, options) in [unrotated, rotated, unrotated].into_iter().enumerate() {
                let started = Instant::now();
                render2d::quality::render_moment_viewport_quality_rgba_into_with_scratch(
                    &cache,
                    volume,
                    options,
                    quality.supersample,
                    &mut scratch,
                    &mut rgba,
                )?;
                std::hint::black_box(&rgba);
                arms[arm].push(started.elapsed());
            }
        }
        for (name, samples) in [
            ("A  unrotated ", &arms[0]),
            ("B  rotated   ", &arms[1]),
            ("A' unrotated ", &arms[2]),
        ] {
            let stats = Stats::from(samples);
            println!(
                "{label} {name} n={} best={:.3} ms p50={:.3} ms mean={:.3} ms",
                samples.len(),
                stats.best,
                stats.median,
                stats.mean
            );
        }
    }
    Ok(())
}

struct Stats {
    best: f64,
    median: f64,
    mean: f64,
}

impl Stats {
    fn from(samples: &[Duration]) -> Self {
        let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1_000.0).collect();
        ms.sort_by(f64::total_cmp);
        let mean = ms.iter().sum::<f64>() / ms.len().max(1) as f64;
        Self {
            best: ms.first().copied().unwrap_or(0.0),
            median: ms.get(ms.len() / 2).copied().unwrap_or(0.0),
            mean,
        }
    }
}

/// FNV-1a, 64 bit (Fowler, Noll and Vo, 1991). Not a cryptographic digest and
/// not claimed to be one: it exists so a run prints something a human can
/// compare at a glance. The raw `.rgba` files beside it are what a real hash
/// is taken over.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn painted_pixels(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4).filter(|pixel| pixel[3] != 0).count()
}

fn write_raw(dir: &Path, stem: &str, rgba: &[u8]) -> std::io::Result<()> {
    let path = dir.join(format!("{stem}.rgba"));
    std::fs::write(&path, rgba)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn write_png(
    dir: &Path,
    stem: &str,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.join(format!("{stem}.png"));
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or("RGBA buffer does not match the reported dimensions")?;
    image.save(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn parse_args() -> Result<Config, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args.next().ok_or(USAGE)?;
    let output = args.next().ok_or(USAGE)?;
    let mut config = Config {
        input: PathBuf::from(input),
        output: PathBuf::from(output),
        rotation_deg: 0.0,
        bench_runs: 0,
    };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--rotation-deg" => {
                config.rotation_deg = args.next().ok_or(USAGE)?.parse()?;
            }
            "--bench" => {
                config.bench_runs = args.next().ok_or(USAGE)?.parse()?;
            }
            other => return Err(format!("unknown flag {other}\n{USAGE}").into()),
        }
    }
    Ok(config)
}

const USAGE: &str = "usage: analysis_view_proof <volume-file> <output-dir> \
                     [--rotation-deg D] [--bench RUNS]";
