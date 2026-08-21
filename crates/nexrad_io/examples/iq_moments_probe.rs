//! Offline verification tool for the Level 1 moment and spectrum processor.
//!
//! Reads a plain I/Q dump, runs [`nexrad_io::iq_moments::process_sweep`] over
//! it, and prints one CSV row per gate. The point is to make the processor's
//! answer on REAL pulses comparable, number by number, with an independent
//! implementation of the same estimators - which is the only way to tell a
//! correct moment from a plausible one. Synthetic recovery on its own proves
//! only that the code agrees with the code.
//!
//! ```text
//! cargo run --release -p nexrad_io --example iq_moments_probe -- \
//!     <dump.iqd> [--dwell 64] [--stride N] [--taper rectangular|hann|hamming|blackman]
//!     [--censor off|<dB>] [--burst 0] [--dwell-index 0] [--spectrum <gate>]
//! ```
//!
//! Flags may be given in any order. They used to not be: `--dwell` rebuilt the
//! whole plan as a contiguous one, so `--stride 16 --dwell 64` silently threw
//! the stride away and ran non-overlapping dwells while the operator believed
//! they had asked for a sliding window. Every flag is now collected first and
//! the plan built once, from all of them.
//!
//! The dump format is [`nexrad_io::iq_moments::interchange`], which documents
//! it. The example takes a path so that no bulk sample data ever enters the
//! repository.

use std::env;
use std::fs;
use std::process::ExitCode;

use nexrad_io::iq_moments::estimator::SnrCensor;
use nexrad_io::iq_moments::interchange::{DumpVersion, read_dump};
use nexrad_io::iq_moments::taper::Taper;
use nexrad_io::iq_moments::{DwellPlan, MomentConfig, process_sweep, sweep_gate_spectrum};

/// Everything the command line can say, before any of it is turned into a
/// [`MomentConfig`]. Kept as a plain value so that the parse is a pure function
/// of the arguments and can be tested as one - see
/// `crates/nexrad_io/tests/iq_probe_cli.rs`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeOptions {
    pub path: String,
    pub dwell_pulses: Option<usize>,
    pub stride: Option<usize>,
    pub taper: Option<Taper>,
    pub censor: Option<SnrCensor>,
    pub burst_samples: Option<usize>,
    pub dwell_index: usize,
    pub spectrum_gate: Option<usize>,
}

impl ProbeOptions {
    /// The dwell plan the flags add up to, whatever order they arrived in.
    pub fn dwell_plan(&self) -> DwellPlan {
        let default = DwellPlan::default();
        match (self.dwell_pulses, self.stride) {
            (Some(pulses), Some(stride)) => DwellPlan::sliding(pulses, stride),
            (Some(pulses), None) => DwellPlan::contiguous(pulses),
            (None, Some(stride)) => DwellPlan::sliding(default.pulses, stride),
            (None, None) => default,
        }
    }

    pub fn config(&self) -> MomentConfig {
        MomentConfig {
            dwell: self.dwell_plan(),
            taper: self.taper.unwrap_or_default(),
            // The probe exists to see what the operational threshold hides, so
            // it starts with the censor off rather than at the default.
            censor: self.censor.unwrap_or(SnrCensor::Off),
            burst_samples: self.burst_samples.unwrap_or(0),
            ..MomentConfig::default()
        }
    }
}

/// Parse the argument list. Returns the message to print on failure.
pub fn parse_options(args: &[String]) -> Result<ProbeOptions, String> {
    let Some(path) = args.first() else {
        return Err(
            "usage: iq_moments_probe <dump.iqd> [--dwell N] [--stride N] [--taper NAME] \
                    [--censor off|dB] [--burst N] [--dwell-index N] [--spectrum GATE]"
                .to_owned(),
        );
    };
    let mut options = ProbeOptions {
        path: path.clone(),
        ..ProbeOptions::default()
    };

    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} wants a value"))?;
        match flag {
            "--dwell" => options.dwell_pulses = Some(whole(value, flag)?),
            "--stride" => options.stride = Some(whole(value, flag)?),
            "--taper" => {
                options.taper = Some(match value.as_str() {
                    "rectangular" | "rect" => Taper::Rectangular,
                    "hann" | "vonhann" => Taper::VonHann,
                    "hamming" => Taper::Hamming,
                    "blackman" => Taper::Blackman,
                    other => return Err(format!("unknown taper {other}")),
                });
            }
            "--censor" => {
                options.censor = Some(if value == "off" {
                    SnrCensor::Off
                } else {
                    SnrCensor::MinDb(value.parse().map_err(|_| {
                        format!("--censor wants \"off\" or a dB value, got {value}")
                    })?)
                });
            }
            "--burst" => options.burst_samples = Some(whole(value, flag)?),
            "--dwell-index" => options.dwell_index = whole(value, flag)?,
            "--spectrum" => options.spectrum_gate = Some(whole(value, flag)?),
            other => return Err(format!("unknown flag {other}")),
        }
        index += 2;
    }
    Ok(options)
}

fn whole(value: &str, flag: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} wants a whole number, got {value}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let options = match parse_options(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let config = options.config();

    let bytes = match fs::read(&options.path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {}: {error}", options.path);
            return ExitCode::FAILURE;
        }
    };
    let dump = match read_dump(&bytes) {
        Ok(dump) => dump,
        Err(error) => {
            eprintln!("{}: {error}", options.path);
            return ExitCode::FAILURE;
        }
    };
    if dump.version == DumpVersion::V1 {
        eprintln!(
            "# note: IQDUMP01 carries one PRT for the whole sweep, so the timing-based \
             staggered-PRT guard cannot see anything in this file"
        );
    }
    let sweep = dump.sweep;

    if let Some(gate) = options.spectrum_gate {
        match sweep_gate_spectrum(&sweep, &config, options.dwell_index, gate, 0) {
            Ok(spectrum) => {
                println!("velocity_mps,power_dbm");
                for (velocity, power) in spectrum
                    .velocities_mps
                    .iter()
                    .zip(spectrum.power_dbm.iter())
                {
                    println!("{velocity:.6},{power:.6}");
                }
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("spectrum: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    let processed = match process_sweep(&sweep, &config) {
        Ok(processed) => processed,
        Err(error) => {
            eprintln!("process_sweep: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(estimates) = processed.dwell(options.dwell_index) else {
        eprintln!(
            "dwell {} out of range: {} dwells",
            options.dwell_index, processed.report.dwells
        );
        return ExitCode::FAILURE;
    };

    eprintln!(
        "# {} pulses, {} dwells of {} every {} ({}), {} gates, dual_pol={}, \
         nyquist={:.3} m/s, r_max={:.1} km, blank {}/{} of which {} below noise",
        processed.report.pulses_available,
        processed.report.dwells,
        processed.report.pulses_per_dwell,
        processed.report.stride,
        processed.report.taper.label(),
        processed.report.gates,
        processed.report.dual_pol,
        processed.report.nyquist_velocity_mps,
        processed.report.unambiguous_range_m / 1000.0,
        processed.report.censored_samples,
        processed.report.total_samples(),
        processed.report.below_noise_samples
    );

    println!(
        "gate,range_m,power_h_dbm,snr_h_db,snr_v_db,sqi,reflectivity_dbz,velocity_mps,\
         width_mps,zdr_db,rhohv,phidp_deg,censored,below_noise"
    );
    for (gate, estimate) in estimates.iter().enumerate() {
        println!(
            "{gate},{:.4},{},{},{},{},{},{},{},{},{},{},{},{}",
            estimate.range_m,
            csv(estimate.power_h_dbm),
            csv(estimate.snr_h_db),
            csv(estimate.snr_v_db),
            csv(estimate.sqi),
            csv(estimate.reflectivity_dbz),
            csv(estimate.velocity_mps),
            csv(estimate.spectrum_width_mps),
            csv(estimate.differential_reflectivity_db),
            csv(estimate.correlation_coefficient),
            csv(estimate.differential_phase_deg),
            u8::from(estimate.censored),
            u8::from(estimate.below_noise)
        );
    }
    ExitCode::SUCCESS
}

fn csv(value: f32) -> String {
    if value.is_finite() {
        format!("{value:.6}")
    } else {
        "nan".to_owned()
    }
}
