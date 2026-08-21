//! NEXRAD Level 1 (time series / I/Q) moment and spectrum processor.
//!
//! Level II - everything this application reads today - is a summary. For each
//! radial the signal processor averages fifty to eighty pulses, writes six or
//! seven numbers per gate, and discards the pulses. Level 1 is the pulses
//! themselves: the complex receiver voltage per pulse per gate, before any
//! estimator has run. From them the moments are computed here, which means the
//! dwell length, the window, and what gets censored stop being decisions
//! somebody else made in 1988 and become parameters - and the Doppler spectrum
//! per gate, which no moment product contains, becomes available at all.
//!
//! # Not a feed
//!
//! Level 1 is an archive and case-study capability. The Radar Operations Center
//! states that Level I data "is not collected regularly or disseminated in real
//! time"; there is no live source and nothing here should be wired to one. The
//! reference dataset this was developed against is the NSSL THREDDS archive of
//! KOUN time series from 20 May 2013.
//!
//! # Where this sits in the crate
//!
//! This module is in `nexrad_io` rather than in `render2d` or a crate of its
//! own, for three reasons:
//!
//! 1. **It is a producer of moments, and every producer lives here.** The
//!    renderer receives moment data as a [`radar_core::ElevationCut`] holding
//!    [`radar_core::MomentGrid`]s; Level II, ODIM, CfRadial, DORADE and the
//!    mobile archive all end at exactly that type, and so does this. The only
//!    thing that makes Level 1 different is that the estimator stage has not
//!    already been run for it, so this module runs it.
//! 2. **`render2d` must not depend on the file reader.** It has `nexrad_io` as a
//!    dev-dependency only. Putting the processor there would invert the
//!    layering so that the rasteriser depends on the decoder.
//! 3. **A new crate could not reach the application.**
//!    `crates/workstation_app/tests/architecture.rs` fixes the workstation's
//!    direct dependencies, and that test is not ours to edit. Reached through
//!    `nexrad_io`, which the workstation already depends on, this needs no
//!    manifest change anywhere and adds no third-party dependency - including
//!    the Fourier transform, which is written out in [`fft`].
//!
//! # What it will not do
//!
//! Contiguous PRT only, this round. Two RVP8 major modes produce garbage under
//! a naive pulse-pair estimator and are refused with a message rather than
//! processed into a plausible wrong field: mode 12 (batch / staggered PRT,
//! where alternating intervals mean `arg R(1)` is not a single velocity) and
//! mode 15 (SZ-2 phase coding, where the second trip has to be separated from
//! the first before any moment means anything). Staggered intervals are also
//! detected directly from the pulse timing, so a file that fails to declare its
//! mode is still caught.
//!
//! A half-present vertical channel is refused for the same reason. A sweep is
//! taken as dual-polarisation when every pulse brought a vertical channel and
//! as single-polarisation when none did; one pulse arriving short is an error,
//! not a reason to quietly drop the differential moments from the whole sweep.
//!
//! # References
//!
//! Doviak and Zrnic, *Doppler Radar and Weather Observations*, 2nd ed. 1993,
//! chapters 4 and 6; Bringi and Chandrasekar, *Polarimetric Doppler Weather
//! Radar*, CUP 2001, chapters 5 and 6; Zrnic 1977, IEEE Trans. AES 13, 344;
//! Melnikov, Zrnic, Doviak et al. 2011, JAMC 50, 859; Ivic, Curtis and Torres
//! 2013, JTECH 30, 2737. The RVP8/RVP900 packed-float and header conventions
//! are those of the Vaisala RVP900 User Guide (section 8 for the time-series
//! record, chapter 7 for the moment chain), which the public NOAA ICD 2620076
//! restates with the packed-float rules transposed. The pulse-pair chain was
//! cross-read against OU RadarKit (github.com/OURadar/RadarKit, MIT licensed).

pub mod estimator;
pub mod fft;
pub mod interchange;
pub mod spectrum;
pub mod taper;

use std::collections::BTreeMap;

use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, RadarSite, RadarVolume, Radial,
};
use rayon::prelude::*;
use thiserror::Error;

use crate::iq::{IqCalibration, IqSweep, PulseLayout};
use estimator::{
    DwellGeometry, DwellWeights, GateEstimate, MomentCalibration, SnrCensor, estimate_gate,
};
use fft::Complex;
use spectrum::{DopplerSpectrum, gate_spectrum};
use taper::Taper;

/// How the pulses of a sweep are cut into radials.
///
/// The dwell length is the single most consequential knob in the whole feature:
/// it trades azimuthal resolution and estimate variance against each other, and
/// the operational choice (a few tens of pulses) is a compromise struck for a
/// radar that has to finish a volume in five minutes, not one an analyst is
/// re-examining years later. Long dwells give quiet moments and a finely
/// resolved spectrum over a smeared azimuth; short ones give the opposite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DwellPlan {
    /// Pulses averaged into one radial.
    pub pulses: usize,
    /// Pulses advanced between consecutive radials. Equal to `pulses` for
    /// non-overlapping dwells; smaller for a sliding window, which produces
    /// more radials at the cost of correlating neighbours.
    pub stride: usize,
}

impl DwellPlan {
    /// Non-overlapping dwells of `pulses` pulses.
    pub fn contiguous(pulses: usize) -> Self {
        Self {
            pulses,
            stride: pulses.max(1),
        }
    }

    /// Overlapping dwells advancing by `stride` pulses.
    pub fn sliding(pulses: usize, stride: usize) -> Self {
        Self {
            pulses,
            stride: stride.max(1),
        }
    }
}

impl Default for DwellPlan {
    /// 64 pulses, non-overlapping. Chosen because it sits in the middle of the
    /// WSR-88D's own per-radial sample counts, so a first look at a Level 1 file
    /// is comparable with the Level II product of the same scan before anything
    /// is deliberately changed.
    fn default() -> Self {
        Self::contiguous(64)
    }
}

/// Everything the processor needs beyond the sweep itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MomentConfig {
    pub dwell: DwellPlan,
    pub taper: Taper,
    pub censor: SnrCensor,
    /// Number of leading samples in every pulse that are burst / reference
    /// samples rather than range gates.
    ///
    /// The burst occupies the *first sample*, not gate 0, and a reader that
    /// leaves it in place shifts every gate one bin outward and reports the
    /// transmit sample as an enormous echo at minimum range. On the 20 May 2013
    /// KOUN files there are two such samples per channel block - the raw burst
    /// and the same burst rotated to zero phase - which is why this is a count
    /// rather than a flag. Zero when the reader has already removed them.
    pub burst_samples: usize,
    /// The RVP8 `iMajorMode` the file declared, when the reader knows it.
    /// Modes 12 and 15 are refused. `None` skips the declared-mode check; the
    /// timing check on staggered PRT still runs.
    pub declared_major_mode: Option<u32>,
    /// How far a recorded bin's range may sit from a uniform ladder before the
    /// sweep is refused, metres.
    ///
    /// [`radar_core::GateRange`] models gates as first-gate-plus-spacing, so a
    /// genuinely non-uniform range mask cannot be represented and would be
    /// drawn with every gate at the wrong range. Refusing is the honest
    /// outcome; silently averaging the spacing is not.
    pub max_range_bin_deviation_m: f32,
    pub zdr_offset_db: f32,
    pub phidp_offset_deg: f32,
    /// See [`MomentCalibration::gaseous_attenuation_db_per_km`].
    pub gaseous_attenuation_db_per_km: f32,
    /// Also emit signal-to-noise ratio and normalised coherency as
    /// `MomentType::Unknown("SNR")` and `Unknown("SQI")`.
    ///
    /// Off by default. Not because they are uninteresting - SNR is what the
    /// censor threshold acts on - but because `render2d` has no colour-table
    /// family for either, so both land on the generic 0..100 ramp, on which SQI
    /// (0..1) renders as a flat wash. They are always available on
    /// [`ProcessedSweep::estimates`] regardless of this flag.
    pub emit_diagnostic_moments: bool,
}

impl Default for MomentConfig {
    fn default() -> Self {
        Self {
            dwell: DwellPlan::default(),
            taper: Taper::Rectangular,
            censor: SnrCensor::OPERATIONAL,
            burst_samples: 0,
            declared_major_mode: None,
            max_range_bin_deviation_m: 1.0,
            zdr_offset_db: 0.0,
            phidp_offset_deg: 0.0,
            gaseous_attenuation_db_per_km: 0.0,
            emit_diagnostic_moments: false,
        }
    }
}

/// Why a sweep could not be turned into moments.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum IqMomentError {
    #[error("sweep carries no pulses")]
    NoPulses,
    #[error("dwell length must be at least 2 pulses, got {requested}")]
    DwellTooShort { requested: usize },
    #[error("dwell of {requested} pulses exceeds the {available} pulses in the sweep")]
    DwellExceedsSweep { requested: usize, available: usize },
    #[error(
        "native ray spans require one {native}-pulse dwell per ray (requested {requested} pulses, \
         stride {stride}); crossing rays would combine antenna positions and subdividing them \
         would create duplicate-azimuth rows"
    )]
    NativeRayDwellRequired {
        requested: usize,
        stride: usize,
        native: usize,
    },
    #[error(
        "native ray span {index} starts at {start} with length {len}, outside the {available} \
         pulses in the sweep"
    )]
    InvalidPulseSpan {
        index: usize,
        start: usize,
        len: usize,
        available: usize,
    },
    #[error(
        "native ray span {index} has {actual} pulses against {expected} in the first ray; \
         variable native ray lengths are not supported"
    )]
    NonUniformPulseSpans {
        index: usize,
        actual: usize,
        expected: usize,
    },
    #[error("native ray span {index} overlaps or precedes the previous span")]
    OverlappingPulseSpans { index: usize },
    #[error(
        "pulse {pulse_index} has {actual} recorded bins but the sweep declares {expected} range bins"
    )]
    RangeBinCountMismatch {
        pulse_index: usize,
        actual: usize,
        expected: usize,
    },
    #[error(
        "pulse {pulse_index} carries {actual} vertical-channel bins against {expected} horizontal \
         ones, so the sweep is neither single-polarisation nor consistently dual: the vertical \
         channel is refused rather than dropped, because dropping it would silently produce a \
         sweep with no ZDR, RhoHV or PhiDP at all"
    )]
    VerticalBinCountMismatch {
        pulse_index: usize,
        actual: usize,
        expected: usize,
    },
    #[error("{burst_samples} burst samples were requested but a pulse holds only {bins} bins")]
    BurstExceedsPulse { burst_samples: usize, bins: usize },
    #[error(
        "recorded bin {index} lies at {actual_m} m but a uniform ladder puts it at {expected_m} m, \
         which is more than the {tolerance_m} m tolerance; a non-uniform range mask cannot be \
         drawn on a first-gate-plus-spacing gate model and would place every gate wrong"
    )]
    NonUniformRangeBins {
        index: usize,
        actual_m: f32,
        expected_m: f32,
        tolerance_m: f32,
    },
    #[error("pulse repetition time must be positive, got {prt_s} s at pulse {pulse_index}")]
    NonPositivePrt { pulse_index: usize, prt_s: f32 },
    #[error(
        "pulse {pulse_index} has a PRT of {prt_s} s against {first_prt_s} s at the start of the \
         sweep: this is a staggered or batch PRT waveform, and a naive pulse-pair estimate of it \
         is not a velocity. Contiguous PRT only."
    )]
    StaggeredPrt {
        pulse_index: usize,
        prt_s: f32,
        first_prt_s: f32,
    },
    #[error("RVP8 major mode {mode} is {description}; refused rather than mis-estimated")]
    UnsupportedMajorMode {
        mode: u32,
        description: &'static str,
    },
    #[error("wavelength must be positive, got {wavelength_m} m")]
    InvalidWavelength { wavelength_m: f32 },
    #[error("dwell {index} is out of range: the sweep produced {dwells} dwells")]
    DwellOutOfRange { index: usize, dwells: usize },
    #[error("gate {index} is out of range: the sweep has {gates} gates")]
    GateOutOfRange { index: usize, gates: usize },
    #[error("the sweep is single-polarisation; there is no vertical channel to read")]
    NoVerticalChannel,
}

pub type Result<T> = std::result::Result<T, IqMomentError>;

/// What the processor did, so that a picture can be labelled with how it was
/// made rather than presented as if it were the only possible one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessingReport {
    pub pulses_available: usize,
    pub pulses_used: usize,
    pub dwells: usize,
    pub pulses_per_dwell: usize,
    pub stride: usize,
    pub taper: Taper,
    pub censor: SnrCensor,
    /// Whether the requested SNR censor could actually be applied.
    pub snr_application: SnrApplication,
    pub gates: usize,
    pub burst_samples_dropped: usize,
    pub dual_pol: bool,
    pub nyquist_velocity_mps: f32,
    pub unambiguous_range_m: f32,
    /// Gate-dwell pairs that reached the renderer blank, out of
    /// `dwells * gates`. Two different things land here: gates the SNR censor
    /// hid, and gates with no power above the receiver noise for any moment to
    /// be formed from. The second happens under [`SnrCensor::Off`] too, and is
    /// counted again on its own in `below_noise_samples`, so
    /// `censored_samples - below_noise_samples` is what the threshold alone
    /// removed.
    pub censored_samples: usize,
    /// Gate-dwell pairs whose `R(0)` did not exceed the receiver noise power,
    /// so no signal existed to estimate. A subset of `censored_samples`,
    /// counted separately because it is a measurement outcome and not a
    /// threshold decision. See [`estimator::GateEstimate::below_noise`].
    pub below_noise_samples: usize,
    /// Worst departure of a recorded bin from the uniform ladder, metres.
    pub worst_range_bin_deviation_m: f32,
}

/// How the SNR control affected this processing run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SnrApplication {
    Applied {
        threshold_db: f32,
    },
    Off,
    /// The source carries no measured receiver-noise reference, so SNR is not
    /// computable and a threshold cannot honestly be applied.
    UnavailableNoNoiseCalibration,
}

impl ProcessingReport {
    pub fn total_samples(&self) -> usize {
        self.dwells.saturating_mul(self.gates)
    }

    /// Fraction of gate-dwell pairs that came back blank, for any reason. Read
    /// it against `below_noise_samples` before reading it as a statement about
    /// the censor.
    pub fn censored_fraction(&self) -> f32 {
        let total = self.total_samples();
        if total == 0 {
            return 0.0;
        }
        self.censored_samples as f32 / total as f32
    }

    /// Gate-dwell pairs the SNR threshold alone removed: those that had a
    /// signal above the noise and were hidden anyway.
    pub fn threshold_censored_samples(&self) -> usize {
        self.censored_samples
            .saturating_sub(self.below_noise_samples)
    }
}

/// A processed sweep, in the shape the rasteriser already reads.
///
/// `cut` is a [`radar_core::ElevationCut`]: radial geometry plus one
/// [`radar_core::MomentGrid`] per moment, `f32` storage, `NaN` where a gate is
/// censored. That is byte-for-byte the contract `render2d`'s `f32` raster path
/// consumes from a decoded Level II volume, so the integration is
/// [`ProcessedSweep::into_volume`] followed by the existing render call.
#[derive(Clone, Debug)]
pub struct ProcessedSweep {
    pub cut: ElevationCut,
    pub gate_range: GateRange,
    pub nyquist_velocity_mps: f32,
    /// Every estimate, dwell-major, `dwells * gates` long. Carries the
    /// diagnostics that do not become moment grids.
    pub estimates: Vec<GateEstimate>,
    pub report: ProcessingReport,
}

impl ProcessedSweep {
    /// The estimates for one dwell.
    pub fn dwell(&self, index: usize) -> Option<&[GateEstimate]> {
        let gates = self.report.gates;
        if gates == 0 || index >= self.report.dwells {
            return None;
        }
        self.estimates.get(index * gates..(index + 1) * gates)
    }

    /// Wrap the cut in a single-cut volume so it can be handed straight to the
    /// renderer.
    pub fn into_volume(self, site: RadarSite, time: chrono::DateTime<chrono::Utc>) -> RadarVolume {
        let mut volume = RadarVolume::new(site, time);
        volume.cuts.push(self.cut);
        volume
    }
}

/// Turn a dwell's worth of pulses at a time into calibrated moments.
pub fn process_sweep(sweep: &IqSweep, config: &MomentConfig) -> Result<ProcessedSweep> {
    let plan = validate(sweep, config)?;

    let geometry = DwellGeometry {
        wavelength_m: f64::from(sweep.wavelength_m),
        prt_s: f64::from(sweep.pulses[0].prt_seconds),
        doppler_phase_convention: sweep.doppler_phase_convention,
    };
    let calibration = moment_calibration(sweep, config);
    let weights = DwellWeights::new(config.taper, config.dwell.pulses);

    let mut rows: Vec<(Radial, Vec<GateEstimate>)> = (0..plan.dwells)
        .into_par_iter()
        .map(|dwell| {
            let start = plan.dwell_starts[dwell];
            let pulses = &sweep.pulses[start..start + config.dwell.pulses];
            let radial = dwell_radial(pulses, start, &geometry, &plan);
            let estimates = dwell_estimates(
                pulses,
                &plan,
                &weights,
                &geometry,
                &calibration,
                config.censor,
            );
            (radial, estimates)
        })
        .collect();

    let censored_samples = rows
        .iter()
        .map(|(_, estimates)| estimates.iter().filter(|value| value.censored).count())
        .sum();
    let below_noise_samples = rows
        .iter()
        .map(|(_, estimates)| estimates.iter().filter(|value| value.below_noise).count())
        .sum();

    let mut cut = ElevationCut::new(plan.mean_elevation_deg, None);
    cut.radials = rows.iter().map(|(radial, _)| radial.clone()).collect();
    let estimates: Vec<GateEstimate> = rows
        .drain(..)
        .flat_map(|(_, estimates)| estimates)
        .collect();

    cut.moments = build_moment_grids(&estimates, &plan, config, calibration);

    let snr_application = match (calibration, config.censor) {
        (MomentCalibration::RelativeStoredIq, _) => SnrApplication::UnavailableNoNoiseCalibration,
        (_, SnrCensor::Off) => SnrApplication::Off,
        (_, SnrCensor::MinDb(threshold_db)) => SnrApplication::Applied { threshold_db },
    };

    let report = ProcessingReport {
        pulses_available: sweep.pulses.len(),
        pulses_used: plan.pulses_used,
        dwells: plan.dwells,
        pulses_per_dwell: config.dwell.pulses,
        stride: config.dwell.stride,
        taper: config.taper,
        censor: config.censor,
        snr_application,
        gates: plan.gates,
        burst_samples_dropped: config.burst_samples,
        dual_pol: plan.dual_pol,
        nyquist_velocity_mps: geometry.nyquist_velocity_mps() as f32,
        unambiguous_range_m: (SPEED_OF_LIGHT_M_PER_S * geometry.prt_s / 2.0) as f32,
        censored_samples,
        below_noise_samples,
        worst_range_bin_deviation_m: plan.worst_range_deviation_m,
    };

    Ok(ProcessedSweep {
        cut,
        gate_range: plan.gate_range.clone(),
        nyquist_velocity_mps: report.nyquist_velocity_mps,
        estimates,
        report,
    })
}

/// The Doppler spectrum of one gate of one dwell.
///
/// `channel` is 0 for horizontal, 1 for vertical. Dwell and gate indices are
/// the same ones [`ProcessedSweep`] uses, so a click on a rendered gate maps
/// straight through.
pub fn sweep_gate_spectrum(
    sweep: &IqSweep,
    config: &MomentConfig,
    dwell_index: usize,
    gate_index: usize,
    channel: usize,
) -> Result<DopplerSpectrum> {
    let plan = validate(sweep, config)?;
    if dwell_index >= plan.dwells {
        return Err(IqMomentError::DwellOutOfRange {
            index: dwell_index,
            dwells: plan.dwells,
        });
    }
    if gate_index >= plan.gates {
        return Err(IqMomentError::GateOutOfRange {
            index: gate_index,
            gates: plan.gates,
        });
    }
    if channel >= 1 && !plan.dual_pol {
        return Err(IqMomentError::NoVerticalChannel);
    }

    let geometry = DwellGeometry {
        wavelength_m: f64::from(sweep.wavelength_m),
        prt_s: f64::from(sweep.pulses[0].prt_seconds),
        doppler_phase_convention: sweep.doppler_phase_convention,
    };
    let calibration = moment_calibration(sweep, config);
    let weights = DwellWeights::new(config.taper, config.dwell.pulses);

    let start = plan.dwell_starts[dwell_index];
    let pulses = &sweep.pulses[start..start + config.dwell.pulses];
    let bin = gate_index + plan.burst_samples;
    let samples: Vec<Complex> = pulses
        .iter()
        .map(|pulse| {
            let source = if channel == 0 { &pulse.h } else { &pulse.v };
            let (i, q) = source[bin];
            Complex::new(f64::from(i), f64::from(q))
        })
        .collect();

    Ok(gate_spectrum(
        &samples,
        &weights,
        &geometry,
        &calibration,
        channel,
        plan.range_of_gate(gate_index),
    ))
}

/// Every gate's spectrum for one dwell - the range/velocity panel an analyst
/// reads a whole radial from.
pub fn sweep_dwell_spectra(
    sweep: &IqSweep,
    config: &MomentConfig,
    dwell_index: usize,
    channel: usize,
) -> Result<Vec<DopplerSpectrum>> {
    let plan = validate(sweep, config)?;
    (0..plan.gates)
        .map(|gate| sweep_gate_spectrum(sweep, config, dwell_index, gate, channel))
        .collect()
}

const SPEED_OF_LIGHT_M_PER_S: f64 = 299_792_458.0;

/// Batch and phase-coded waveforms, refused by declared mode.
const REFUSED_MAJOR_MODES: [(u32, &str); 2] = [
    (
        12,
        "batch / staggered PRT: alternating intervals mean the lag-1 argument is not a single \
         velocity",
    ),
    (
        15,
        "SZ-2 phase coding: the overlaid trips must be separated before any moment is meaningful",
    ),
];

/// Everything derived from the sweep once, shared by the moment and spectrum
/// entry points.
#[derive(Clone, Debug)]
struct SweepPlan {
    dwells: usize,
    dwell_starts: Vec<usize>,
    gates: usize,
    burst_samples: usize,
    pulses_used: usize,
    dual_pol: bool,
    gate_range: GateRange,
    first_gate_m: f32,
    gate_spacing_m: f32,
    mean_elevation_deg: f32,
    worst_range_deviation_m: f32,
}

impl SweepPlan {
    fn range_of_gate(&self, gate: usize) -> f32 {
        self.first_gate_m + self.gate_spacing_m * gate as f32
    }
}

fn validate(sweep: &IqSweep, config: &MomentConfig) -> Result<SweepPlan> {
    if sweep.pulses.is_empty() {
        return Err(IqMomentError::NoPulses);
    }
    if config.dwell.pulses < 2 {
        return Err(IqMomentError::DwellTooShort {
            requested: config.dwell.pulses,
        });
    }
    if config.dwell.pulses > sweep.pulses.len() {
        return Err(IqMomentError::DwellExceedsSweep {
            requested: config.dwell.pulses,
            available: sweep.pulses.len(),
        });
    }
    if sweep.wavelength_m <= 0.0 || sweep.wavelength_m.is_nan() {
        return Err(IqMomentError::InvalidWavelength {
            wavelength_m: sweep.wavelength_m,
        });
    }
    if let Some(mode) = config.declared_major_mode {
        for (refused, description) in REFUSED_MAJOR_MODES {
            if mode == refused {
                return Err(IqMomentError::UnsupportedMajorMode { mode, description });
            }
        }
    }

    let first_prt = sweep.pulses[0].prt_seconds;
    if first_prt <= 0.0 || first_prt.is_nan() {
        return Err(IqMomentError::NonPositivePrt {
            pulse_index: 0,
            prt_s: first_prt,
        });
    }
    // A staggered or batch waveform declares itself in the timing whether or not
    // the header was passed in. The tolerance is a part in a thousand, which is
    // far wider than the clock jitter a contiguous-PRT file shows and far
    // narrower than any real stagger ratio (2/3 and 3/4 are the usual ones).
    let prt_tolerance = first_prt * 1e-3;
    for (index, pulse) in sweep.pulses.iter().enumerate() {
        if pulse.prt_seconds <= 0.0 || pulse.prt_seconds.is_nan() {
            return Err(IqMomentError::NonPositivePrt {
                pulse_index: index,
                prt_s: pulse.prt_seconds,
            });
        }
        if (pulse.prt_seconds - first_prt).abs() > prt_tolerance {
            return Err(IqMomentError::StaggeredPrt {
                pulse_index: index,
                prt_s: pulse.prt_seconds,
                first_prt_s: first_prt,
            });
        }
    }

    let declared_bins = sweep.range_bins.len();
    for (index, pulse) in sweep.pulses.iter().enumerate() {
        if pulse.h.len() != declared_bins {
            return Err(IqMomentError::RangeBinCountMismatch {
                pulse_index: index,
                actual: pulse.h.len(),
                expected: declared_bins,
            });
        }
    }
    if config.burst_samples >= declared_bins {
        return Err(IqMomentError::BurstExceedsPulse {
            burst_samples: config.burst_samples,
            bins: declared_bins,
        });
    }

    let bins = &sweep.range_bins[config.burst_samples..];
    let gates = bins.len();
    let first_gate_m = bins[0];
    let spacing = if gates > 1 {
        (bins[gates - 1] - bins[0]) / (gates - 1) as f32
    } else {
        // A single-gate sweep states no spacing of its own, so fall back to the
        // reader's advisory one; `None` there means the mask was irregular,
        // which one gate cannot contradict.
        sweep.gate_spacing_m.unwrap_or_default()
    };
    let mut worst = 0.0f32;
    for (index, range_m) in bins.iter().enumerate() {
        let expected = first_gate_m + spacing * index as f32;
        let deviation = (range_m - expected).abs();
        if deviation > worst {
            worst = deviation;
        }
        // `>` is false for every NaN, so a ladder of NaN ranges - which is
        // what a NaN `fRangeMaskRes` produces, one multiply per bin - would
        // sail through a check written to catch precisely a range mask that
        // cannot be drawn, and every gate would then be placed at NaN metres.
        // The reader refuses such a header by name; this is the second door,
        // because `process_sweep` takes a sweep from anywhere.
        if !deviation.is_finite() || deviation > config.max_range_bin_deviation_m {
            return Err(IqMomentError::NonUniformRangeBins {
                index: index + config.burst_samples,
                actual_m: *range_m,
                expected_m: expected,
                tolerance_m: config.max_range_bin_deviation_m,
            });
        }
    }

    // A sweep is dual-pol if EVERY pulse brought a full vertical channel, and
    // single-pol if NO pulse brought one. Anything in between is refused. It
    // would be easy to write this as `all(|p| p.v.len() == p.h.len())` and let
    // one short vector demote the sweep, but the demotion is silent and total:
    // no ZDR, no RhoHV, no PhiDP anywhere, visible only as a `false` in the
    // report - while the identical inconsistency in the horizontal channel is
    // refused loudly a few lines above. Refusing rather than quietly producing
    // a lesser answer is the rule this module is built on.
    let vertical_present = sweep
        .pulses
        .iter()
        .filter(|pulse| !pulse.v.is_empty())
        .count();
    let dual_pol = vertical_present == sweep.pulses.len();
    if vertical_present > 0 {
        for (index, pulse) in sweep.pulses.iter().enumerate() {
            if pulse.v.len() != pulse.h.len() {
                return Err(IqMomentError::VerticalBinCountMismatch {
                    pulse_index: index,
                    actual: pulse.v.len(),
                    expected: pulse.h.len(),
                });
            }
        }
    }

    let stride = config.dwell.stride.max(1);
    let (dwell_starts, pulses_used) = match &sweep.pulse_layout {
        PulseLayout::Continuous => {
            let dwells = (sweep.pulses.len() - config.dwell.pulses) / stride + 1;
            let starts: Vec<usize> = (0..dwells).map(|dwell| dwell * stride).collect();
            let used = (dwells - 1) * stride + config.dwell.pulses;
            (starts, used)
        }
        PulseLayout::Rays(spans) => {
            let Some(first) = spans.first().copied() else {
                return Err(IqMomentError::NoPulses);
            };
            if first.len == 0 {
                return Err(IqMomentError::InvalidPulseSpan {
                    index: 0,
                    start: first.start,
                    len: first.len,
                    available: sweep.pulses.len(),
                });
            }
            if config.dwell.pulses != first.len || stride != first.len {
                return Err(IqMomentError::NativeRayDwellRequired {
                    requested: config.dwell.pulses,
                    stride,
                    native: first.len,
                });
            }
            let mut previous_end = None;
            let mut starts = Vec::with_capacity(spans.len());
            for (index, span) in spans.iter().copied().enumerate() {
                let Some(end) = span.end() else {
                    return Err(IqMomentError::InvalidPulseSpan {
                        index,
                        start: span.start,
                        len: span.len,
                        available: sweep.pulses.len(),
                    });
                };
                if end > sweep.pulses.len() || span.len == 0 {
                    return Err(IqMomentError::InvalidPulseSpan {
                        index,
                        start: span.start,
                        len: span.len,
                        available: sweep.pulses.len(),
                    });
                }
                if span.len != first.len {
                    return Err(IqMomentError::NonUniformPulseSpans {
                        index,
                        actual: span.len,
                        expected: first.len,
                    });
                }
                if previous_end.is_some_and(|previous| span.start < previous) {
                    return Err(IqMomentError::OverlappingPulseSpans { index });
                }
                previous_end = Some(end);
                starts.push(span.start);
            }
            let used = spans.len().saturating_mul(first.len);
            (starts, used)
        }
    };
    let dwells = dwell_starts.len();

    let mean_elevation_deg = sweep
        .pulses
        .iter()
        .map(|pulse| f64::from(pulse.elevation_deg))
        .sum::<f64>() as f32
        / sweep.pulses.len() as f32;

    Ok(SweepPlan {
        dwells,
        dwell_starts,
        gates,
        burst_samples: config.burst_samples,
        pulses_used,
        dual_pol,
        gate_range: GateRange {
            first_gate_m: first_gate_m.round() as i32,
            gate_spacing_m: spacing.round() as i32,
            gate_count: gates,
        },
        first_gate_m,
        gate_spacing_m: spacing,
        mean_elevation_deg,
        worst_range_deviation_m: worst,
    })
}

fn dwell_radial(
    pulses: &[crate::iq::IqPulse],
    start_pulse: usize,
    geometry: &DwellGeometry,
    plan: &SweepPlan,
) -> Radial {
    // Azimuth is an angle, so the average is the argument of the mean unit
    // vector. An arithmetic mean puts a dwell that straddles north at 180.
    let mut vector = Complex::ZERO;
    let mut elevation = 0.0f64;
    for pulse in pulses {
        vector += Complex::from_polar(1.0, f64::from(pulse.azimuth_deg).to_radians());
        elevation += f64::from(pulse.elevation_deg);
    }
    let azimuth = vector.arg().to_degrees().rem_euclid(360.0);
    let elevation = elevation / pulses.len() as f64;
    let time_offset_ms = (start_pulse as f64 * geometry.prt_s * 1000.0).round();

    Radial {
        azimuth_deg: azimuth as f32,
        elevation_deg: elevation as f32,
        time_offset_ms: time_offset_ms as i32,
        gate_range: plan.gate_range.clone(),
        nyquist_velocity_mps: Some(geometry.nyquist_velocity_mps() as f32),
        radial_status: None,
    }
}

fn dwell_estimates(
    pulses: &[crate::iq::IqPulse],
    plan: &SweepPlan,
    weights: &DwellWeights,
    geometry: &DwellGeometry,
    calibration: &MomentCalibration,
    censor: SnrCensor,
) -> Vec<GateEstimate> {
    let mut h = vec![Complex::ZERO; pulses.len()];
    let mut v = if plan.dual_pol {
        vec![Complex::ZERO; pulses.len()]
    } else {
        Vec::new()
    };
    (0..plan.gates)
        .map(|gate| {
            let bin = gate + plan.burst_samples;
            for (slot, pulse) in h.iter_mut().zip(pulses.iter()) {
                let (i, q) = pulse.h[bin];
                *slot = Complex::new(f64::from(i), f64::from(q));
            }
            if plan.dual_pol {
                for (slot, pulse) in v.iter_mut().zip(pulses.iter()) {
                    let (i, q) = pulse.v[bin];
                    *slot = Complex::new(f64::from(i), f64::from(q));
                }
            }
            estimate_gate(
                &h,
                &v,
                weights,
                geometry,
                calibration,
                censor,
                plan.range_of_gate(gate),
            )
        })
        .collect()
}

/// Pulls one moment's value out of a gate estimate. One per grid the cut ends
/// up carrying.
type MomentExtractor = fn(&GateEstimate) -> f32;

fn build_moment_grids(
    estimates: &[GateEstimate],
    plan: &SweepPlan,
    config: &MomentConfig,
    calibration: MomentCalibration,
) -> BTreeMap<MomentType, MomentGrid> {
    let mut moments: Vec<(MomentType, MomentExtractor)> =
        if matches!(calibration, MomentCalibration::RelativeStoredIq) {
            vec![
                (MomentType::RelativePower, |value| value.power_h_db),
                (MomentType::Velocity, |value| value.velocity_mps),
            ]
        } else {
            vec![
                (MomentType::Reflectivity, |value| value.reflectivity_dbz),
                (MomentType::Velocity, |value| value.velocity_mps),
                (MomentType::SpectrumWidth, |value| value.spectrum_width_mps),
            ]
        };
    if plan.dual_pol && !matches!(calibration, MomentCalibration::RelativeStoredIq) {
        moments.push((MomentType::DifferentialReflectivity, |value| {
            value.differential_reflectivity_db
        }));
        moments.push((MomentType::CorrelationCoefficient, |value| {
            value.correlation_coefficient
        }));
        moments.push((MomentType::DifferentialPhase, |value| {
            value.differential_phase_deg
        }));
    }
    if config.emit_diagnostic_moments {
        if !matches!(calibration, MomentCalibration::RelativeStoredIq) {
            moments.push((MomentType::Unknown("SNR".to_owned()), |value| {
                value.snr_h_db
            }));
        }
        moments.push((MomentType::Unknown("SQI".to_owned()), |value| value.sqi));
    }

    moments
        .into_iter()
        .map(|(moment, extract)| {
            let values: Vec<f32> = estimates.iter().map(extract).collect();
            let grid = MomentGrid {
                moment: moment.clone(),
                producer_description: None,
                producer_units: None,
                gate_range: plan.gate_range.clone(),
                // `MomentGrid::scaled_value` returns an `f32` cell verbatim, so
                // scale and offset are inert for this storage; they are set to
                // the identity rather than to anything that would look
                // meaningful.
                scale: 1.0,
                offset: 0.0,
                // `NaN` is the censoring signal for `f32` storage - the
                // rasteriser skips any sample that is not finite - so there is
                // no sentinel value to declare.
                nodata: None,
                range_folded: None,
                // Both facts belong to the operational processor: the
                // threshold it censored at, and whether it recombined the
                // radials it recorded. Nothing here came through that
                // processor - these moments were estimated from the pulses in
                // this process, under the analyst's own censor - so claiming
                // either would attribute this build's choices to the radar.
                snr_threshold_db: None,
                recombination: None,
                radial_indices: (0..plan.dwells).collect(),
                storage: MomentStorage::F32(values),
            };
            (moment, grid)
        })
        .collect()
}

fn moment_calibration(sweep: &IqSweep, config: &MomentConfig) -> MomentCalibration {
    match sweep.calibration {
        IqCalibration::Absolute {
            noise_dbm,
            dbz_calibration,
            saturation_dbm,
        } => MomentCalibration::absolute(
            dbz_calibration,
            noise_dbm,
            saturation_dbm,
            config.zdr_offset_db,
            config.phidp_offset_deg,
            config.gaseous_attenuation_db_per_km,
        ),
        IqCalibration::RelativeStoredIq => MomentCalibration::RelativeStoredIq,
    }
}

#[cfg(test)]
mod tests;
