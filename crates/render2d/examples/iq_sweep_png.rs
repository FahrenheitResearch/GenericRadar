//! Render a Level 1 I/Q dump to PNG, so a processed sweep can be LOOKED at.
//!
//! The companion to `nexrad_io`'s `iq_moments_probe`, which prints the same
//! sweep as numbers. Numbers agreeing with a second implementation prove the
//! arithmetic; only the picture shows whether the gates landed at the right
//! ranges and azimuths, whether the censor bit where it should, and whether the
//! field looks like weather. Both are needed and neither substitutes for the
//! other.
//!
//! ```text
//! cargo run --release -p render2d --example iq_sweep_png -- \
//!     <dump.iqd> <out-dir> [--dwell 64] [--taper hann] [--censor off|<dB>]
//!     [--burst 2] [--range-fraction 35]
//! ```
//!
//! The dump format is `nexrad_io::iq_moments::interchange`, which documents it
//! and now reads it for both tools rather than each keeping its own copy of the
//! parser. The example takes a path so that no bulk sample data ever enters the
//! repository.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::{TimeZone, Utc};
use nexrad_io::iq_moments::estimator::SnrCensor;
use nexrad_io::iq_moments::interchange::read_dump;
use nexrad_io::iq_moments::taper::Taper;
use nexrad_io::iq_moments::{DwellPlan, MomentConfig, process_sweep};
use radar_core::{MomentType, RadarSite};
use render2d::{RasterOptions, render_moment_png};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: iq_sweep_png <dump.iqd> <out-dir> [--dwell N] [--taper NAME] \
             [--censor off|dB] [--burst N] [--range-fraction N]"
        );
        return ExitCode::FAILURE;
    }
    let dump = args[0].clone();
    let out_dir = PathBuf::from(&args[1]);

    let mut config = MomentConfig::default();
    let mut range_fraction = 94u8;
    let mut index = 2;
    while index + 1 < args.len() {
        let (flag, value) = (args[index].as_str(), args[index + 1].as_str());
        match flag {
            "--dwell" => config.dwell = DwellPlan::contiguous(value.parse().unwrap_or(64)),
            "--taper" => {
                config.taper = match value {
                    "rectangular" | "rect" => Taper::Rectangular,
                    "hann" | "vonhann" => Taper::VonHann,
                    "hamming" => Taper::Hamming,
                    "blackman" => Taper::Blackman,
                    other => {
                        eprintln!("unknown taper {other}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--censor" => {
                config.censor = if value == "off" {
                    SnrCensor::Off
                } else {
                    SnrCensor::MinDb(value.parse().unwrap_or(2.0))
                }
            }
            "--burst" => config.burst_samples = value.parse().unwrap_or(0),
            "--range-fraction" => range_fraction = value.parse().unwrap_or(94),
            other => {
                eprintln!("unknown flag {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 2;
    }

    let bytes = match fs::read(&dump) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {dump}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let sweep = match read_dump(&bytes) {
        Ok(read) => read.sweep,
        Err(error) => {
            eprintln!("{dump}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let processed = match process_sweep(&sweep, &config) {
        Ok(processed) => processed,
        Err(error) => {
            eprintln!("process_sweep: {error}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!(
        "{} pulses -> {} radials of {} ({}), {} gates, {:.0} m spacing from {:.0} m, \
         blank {}/{} ({:.1}%), of which {} had no signal above the noise",
        processed.report.pulses_available,
        processed.report.dwells,
        processed.report.pulses_per_dwell,
        processed.report.taper.label(),
        processed.report.gates,
        processed.gate_range.gate_spacing_m,
        processed.gate_range.first_gate_m,
        processed.report.censored_samples,
        processed.report.total_samples(),
        100.0 * processed.report.censored_fraction(),
        processed.report.below_noise_samples
    );
    eprintln!(
        "azimuth {:.2} to {:.2} deg, elevation {:.2} deg",
        processed
            .cut
            .radials
            .first()
            .map(|radial| radial.azimuth_deg)
            .unwrap_or_default(),
        processed
            .cut
            .radials
            .last()
            .map(|radial| radial.azimuth_deg)
            .unwrap_or_default(),
        processed.cut.elevation_deg
    );

    if let Err(error) = fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {error}", out_dir.display());
        return ExitCode::FAILURE;
    }
    let volume = processed.into_volume(
        RadarSite::new(&sweep.site),
        Utc.timestamp_opt(sweep.time_utc, 0)
            .single()
            .unwrap_or_else(Utc::now),
    );
    let options = RasterOptions {
        width: 900,
        height: 900,
        range_fraction,
    };
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
        MomentType::DifferentialPhase,
    ] {
        let path = out_dir.join(format!("{}.png", moment.short_name().to_lowercase()));
        match render_moment_png(&volume, 0, moment.clone(), &path, options) {
            Ok(()) => eprintln!("wrote {}", path.display()),
            Err(error) => eprintln!("{moment}: {error}"),
        }
    }
    ExitCode::SUCCESS
}
