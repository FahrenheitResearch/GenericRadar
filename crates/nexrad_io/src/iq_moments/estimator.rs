//! Pulse-pair moment estimation for one dwell of one gate.
//!
//! Everything here works on plain slices of complex samples so that it can be
//! read, and tested, without a file anywhere near it. The file-shaped
//! [`super::process_sweep`] is a thin loop over this.
//!
//! # References
//!
//! * Doviak and Zrnic, *Doppler Radar and Weather Observations*, 2nd ed.,
//!   Academic Press 1993, chapters 4 and 6 - the radar equation and the
//!   autocovariance (pulse-pair) estimators.
//! * Zrnic 1977, IEEE Trans. Aerospace and Electronic Systems 13, 344,
//!   "Spectral moment estimates from correlated pulse pairs" - the lag-0/lag-1
//!   spectrum width estimator used here.
//! * Bringi and Chandrasekar, *Polarimetric Doppler Weather Radar*, CUP 2001,
//!   chapters 5 and 6 - the differential moments and their noise corrections.
//! * Melnikov, Zrnic, Doviak et al. 2011, J. Appl. Meteor. Climatol. 50, 859 -
//!   correlation-coefficient bias and its dependence on signal-to-noise ratio.
//! * Ivic, Curtis and Torres 2013, J. Atmos. Oceanic Technol. 30, 2737 - noise
//!   power estimation, the term subtracted throughout this module.
//! * The pulse-pair chain was cross-read against OU RadarKit
//!   (github.com/OURadar/RadarKit, MIT licensed) while this was written.
//!
//! # Sign and ordering conventions, and how they were pinned
//!
//! These are conventions, not derivations: they follow the handedness of the
//! digitiser, and getting one backwards produces a field that looks entirely
//! plausible and is wrong. The two that matter were fixed against real pulses
//! rather than asserted, and both are now pinned by tests that run in this
//! repository rather than by prose: the synthetic pins are in this file, and
//! the real-pulse pins are `crates/nexrad_io/tests/iq_moments_real.rs`, which
//! reads a 39,664-byte slice of the reference file whose provenance that test
//! states in full.
//!
//! * **Lag-1 ordering.** `R(1) = mean(x[k] conj(x[k+1]))` and
//!   `V = (lambda / 4 pi T) arg R(1)`. Ground clutter is the only target whose
//!   velocity is known a priori, so it is the anchor: the 1 km gate of the
//!   reference file, which returns 47 dBZ at a coherency of 0.98, reads a
//!   median +0.03 m/s across the file's twenty-eight non-overlapping 64-pulse
//!   dwells (spread -0.43 to +0.52 m/s), i.e. zero to within estimator noise
//!   inside a 33.24 m/s Nyquist interval.
//! * **Cross-correlation ordering.** `C(0) = mean(v[k] conj(h[k]))` and
//!   `PhiDP = arg C(0)`. With the `exp(-j 2 k r)` receiver convention the
//!   horizontal channel accumulates the *larger* propagation phase delay, which
//!   appears as the more negative argument, so it is `v conj(h)` and not
//!   `h conj(v)` that increases with range. That is the physical anchor -
//!   differential phase must RISE along a path through rain, because the
//!   oblate drops delay the horizontal wave more - and it is what the real
//!   pulses show: through the rain shaft from 1.0 to 37.5 km the reference file
//!   gives +1.2 degrees per km taken over every gate, and +1.9 degrees per km
//!   over the fifty-eight of them above 20 dB SNR and 0.8 correlation. The
//!   opposite ordering is the same numbers negated, i.e. a differential phase
//!   that falls through rain, which is unphysical.
//!
//! Neither claim rests on the two channels being told apart correctly, which is
//! itself a decode question: the channel split was confirmed from the data by
//! the correlation itself (channel-major blocks give a median RhoHV of 0.89
//! against 0.33 for a bin-interleaved reading, and a plus-or-minus two bin
//! offset scan peaks decisively at zero offset), and the packed-float scaling
//! was confirmed against the header's own burst magnitude and against the
//! far-range noise floor, which measures -80.9 dBm against a declared -80.56.
//!
//! If a later dataset shows the opposite handedness, that belongs in the reader
//! as a per-instrument flag; it must not be "fixed" by flipping a sign here,
//! because these two conventions are locked to each other by the receiver's
//! complex convention and flipping one alone makes the pair inconsistent.

use super::fft::Complex;
use super::taper::Taper;
use crate::iq::DopplerPhaseConvention;

/// The meaning of a logarithmic power value produced from stored I/Q.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerReference {
    /// Absolute received power.
    AbsoluteDbm,
    /// Power relative to one squared unit in the stored I/Q integers.
    RelativeStoredIqSquared,
}

impl PowerReference {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AbsoluteDbm => "dBm",
            Self::RelativeStoredIqSquared => "dB re stored I/Q unit²",
        }
    }
}

/// Calibration constants applied to a dwell. Relative acquisitions are a
/// distinct variant so a missing radar constant or noise floor cannot be
/// filled with a plausible-looking numeric placeholder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MomentCalibration {
    Absolute {
        /// SIGMET/IRIS dBZ0: the reflectivity of a target at 1 km range that
        /// returns a signal power equal to the receiver noise power. The RVP8/RVP900
        /// `fDBzCalib` header field is exactly this quantity, so it maps straight in.
        ///
        /// The reference point is worth stating explicitly because the other common
        /// convention - dBZ at 1 km for 0 dBm of received power - differs from it by
        /// the noise power, which is 80 dB. A constant quoted that way converts as
        /// `dbz0_db = quoted + noise_dbm`.
        ///
        /// Measured, not asserted, on the reference file: over its first 64-pulse
        /// dwell the convective core peaks at 66.7 dBZ at 11.5 km, and over the
        /// whole file read as one 1830-pulse dwell at 65.1 dBZ at 13.5 km, against
        /// a declared noise power of -80.5555 dBm and a saturation level of
        /// +6 dBm. There are two ways to get that core wrong and they differ from
        /// each other, so both are worth naming:
        ///
        /// * Reading -35.5 as if it were 0 dBm-referenced - that is, building
        ///   reflectivity on received power in dBm rather than on SNR - moves every
        ///   gate down by `noise_dbm`, 80.56 dB, and puts the core at -14 dBZ.
        /// * Building it on the decoded I/Q power directly, in the normalised scale
        ///   where a magnitude of 1.0 is `saturation_dbm`, moves it down by a
        ///   further 6 dB - `noise_dbm - saturation_dbm`, 86.56 dB in all - and
        ///   puts the core at -20 dBZ.
        ///
        /// Either is exactly the kind of plausible-looking wrong field this module
        /// is written to avoid; `crates/nexrad_io/tests/iq_moments_real.rs` fails on
        /// both.
        dbz0_db: f32,
        /// Receiver noise power, dBm, `[horizontal, vertical]`.
        noise_dbm: [f32; 2],
        /// The dBm corresponding to a decoded I/Q magnitude of 1.0. RVP8
        /// `fSaturationDBM`.
        saturation_dbm: f32,
        /// `D_cal` added to differential reflectivity.
        zdr_offset_db: f32,
        /// `phi_cal` added to differential phase, degrees, before wrapping.
        phidp_offset_deg: f32,
        /// Two-way gaseous attenuation, dB per km of range, added back into
        /// reflectivity. Defaults to zero so that nothing is applied unasked; the
        /// WSR-88D applies a fixed two-way value of about 0.016 dB per km at low
        /// elevation angles in a standard atmosphere (Doviak and Zrnic 1993,
        /// section 3.3).
        gaseous_attenuation_db_per_km: f32,
    },
    /// Stored receiver units only. No receiver-noise, dBm, dBZ or calibrated
    /// polarimetric quantity may be derived from this variant.
    RelativeStoredIq,
}

impl MomentCalibration {
    #[allow(clippy::too_many_arguments)]
    pub const fn absolute(
        dbz0_db: f32,
        noise_dbm: [f32; 2],
        saturation_dbm: f32,
        zdr_offset_db: f32,
        phidp_offset_deg: f32,
        gaseous_attenuation_db_per_km: f32,
    ) -> Self {
        Self::Absolute {
            dbz0_db,
            noise_dbm,
            saturation_dbm,
            zdr_offset_db,
            phidp_offset_deg,
            gaseous_attenuation_db_per_km,
        }
    }

    #[must_use]
    pub const fn power_reference(self) -> PowerReference {
        match self {
            Self::Absolute { .. } => PowerReference::AbsoluteDbm,
            Self::RelativeStoredIq => PowerReference::RelativeStoredIqSquared,
        }
    }

    #[must_use]
    pub const fn power_db_offset(self) -> f32 {
        match self {
            Self::Absolute { saturation_dbm, .. } => saturation_dbm,
            Self::RelativeStoredIq => 0.0,
        }
    }

    #[must_use]
    pub fn noise_dbm(self, channel: usize) -> Option<f32> {
        match self {
            Self::Absolute { noise_dbm, .. } => Some(noise_dbm[channel.min(1)]),
            Self::RelativeStoredIq => None,
        }
    }

    /// Noise power of a channel in the same normalised linear units the decoded
    /// I/Q samples carry (magnitude 1.0 is `saturation_dbm`).
    pub fn noise_linear(self, channel: usize) -> Option<f64> {
        match self {
            Self::Absolute {
                noise_dbm,
                saturation_dbm,
                ..
            } => {
                let dbm = f64::from(noise_dbm[channel.min(1)]);
                Some(10f64.powf((dbm - f64::from(saturation_dbm)) / 10.0))
            }
            Self::RelativeStoredIq => None,
        }
    }
}

/// How weak a gate may be before its moments are hidden.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SnrCensor {
    /// Apply no threshold. Every gate whose power exceeds the receiver noise
    /// reports whatever the estimator produced, however marginal. This is the
    /// setting the feature exists for: it is the only way to see what the
    /// operational threshold was throwing away, and the only way to judge
    /// whether a weak echo was real.
    ///
    /// It is not the same as "nothing is ever blank". A gate whose measured
    /// `R(0)` does not exceed the noise power has a *negative* estimated
    /// signal, from which no reflectivity, no width and no differential moment
    /// exists to report, so those gates still come back blank - about half of
    /// them on pure noise at a 64-pulse dwell, and none of them above -1 dB
    /// SNR. That is a different event from a threshold decision and is counted
    /// separately: see [`GateEstimate::below_noise`] and
    /// [`super::ProcessingReport::below_noise_samples`].
    Off,
    /// Hide gates whose horizontal-channel signal-to-noise ratio is below this
    /// many dB.
    MinDb(f32),
}

impl SnrCensor {
    /// The operational WSR-88D threshold, and the default: 2 dB. Chosen as the
    /// default so that a Level 1 file processed with no arguments produces the
    /// same population of gates the operational Level II product would, which
    /// makes the two comparable; an analyst then turns it [`SnrCensor::Off`]
    /// deliberately, and knows they have.
    pub const OPERATIONAL: Self = Self::MinDb(2.0);

    fn hides(self, snr_db: f64) -> bool {
        match self {
            Self::Off => false,
            // A gate with no estimable SNR - no signal above noise at all -
            // counts as hidden, which is why this is not written as a plain
            // `<` comparison.
            Self::MinDb(threshold) => snr_db.is_nan() || snr_db < f64::from(threshold),
        }
    }
}

impl Default for SnrCensor {
    fn default() -> Self {
        Self::OPERATIONAL
    }
}

/// Precomputed window weights and the two normalising sums the lag estimates
/// need. Built once per dwell, reused for every gate.
#[derive(Clone, Debug)]
///
/// The window the weights came from travels with them. It used to be passed
/// separately to [`super::spectrum::gate_spectrum`] alongside the weights, so a
/// caller could compute a spectrum through one window and label it - and report
/// its equivalent noise bandwidth - as another. The pair is one thing, so it is
/// stored as one thing.
pub struct DwellWeights {
    taper: Taper,
    weights: Vec<f64>,
    /// `sum(w[k]^2)` - the lag-0 normaliser.
    lag0_norm: f64,
    /// `sum(w[k] w[k+1])` - the lag-1 normaliser.
    ///
    /// Lag 0 and lag 1 need *different* normalisers, because the expected value
    /// of the windowed lag-1 sum carries `sum(w[k] w[k+1])` and not `sum(w^2)`.
    /// Using one for both leaves a window-shaped bias in `R(0)/|R(1)|`, and
    /// that ratio is the spectrum width.
    lag1_norm: f64,
}

impl DwellWeights {
    pub fn new(taper: Taper, pulses: usize) -> Self {
        let weights = taper.weights(pulses);
        let lag0_norm = weights.iter().map(|w| w * w).sum();
        let lag1_norm = weights
            .windows(2)
            .map(|pair| pair[0] * pair[1])
            .sum::<f64>();
        Self {
            taper,
            weights,
            lag0_norm,
            lag1_norm,
        }
    }

    pub fn pulses(&self) -> usize {
        self.weights.len()
    }

    /// The window these weights were built from. Anything that labels a result
    /// with a window name reads it from here rather than being told separately.
    pub fn taper(&self) -> Taper {
        self.taper
    }

    pub fn weights(&self) -> &[f64] {
        &self.weights
    }

    /// Windowed lag-0 autocovariance: total power, signal plus noise.
    fn lag0(&self, samples: &[Complex]) -> f64 {
        if self.lag0_norm == 0.0 {
            return 0.0;
        }
        let sum: f64 = samples
            .iter()
            .zip(self.weights.iter())
            .map(|(z, w)| z.norm_sqr() * w * w)
            .sum();
        sum / self.lag0_norm
    }

    /// Windowed lag-1 autocovariance, ordered `x[k] conj(x[k+1])`.
    fn lag1(&self, samples: &[Complex]) -> Complex {
        if self.lag1_norm == 0.0 || samples.len() < 2 {
            return Complex::ZERO;
        }
        let mut acc = Complex::ZERO;
        for k in 0..samples.len() - 1 {
            let weight = self.weights[k] * self.weights[k + 1];
            acc += (samples[k] * samples[k + 1].conj()).scale(weight);
        }
        acc.scale(1.0 / self.lag1_norm)
    }

    /// Windowed zero-lag cross-covariance, ordered `v[k] conj(h[k])`.
    fn cross_lag0(&self, h: &[Complex], v: &[Complex]) -> Complex {
        if self.lag0_norm == 0.0 {
            return Complex::ZERO;
        }
        let mut acc = Complex::ZERO;
        for (k, (hs, vs)) in h.iter().zip(v.iter()).enumerate() {
            let weight = self.weights[k] * self.weights[k];
            acc += (*vs * hs.conj()).scale(weight);
        }
        acc.scale(1.0 / self.lag0_norm)
    }
}

/// Geometry and timing constants shared by every gate of a dwell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DwellGeometry {
    pub wavelength_m: f64,
    pub prt_s: f64,
    pub doppler_phase_convention: DopplerPhaseConvention,
}

impl DwellGeometry {
    /// `v_a = lambda / 4 T`.
    pub fn nyquist_velocity_mps(&self) -> f64 {
        self.wavelength_m / (4.0 * self.prt_s)
    }

    /// Convert the argument of `x[k] * conj(x[k + 1])` into signed velocity.
    pub fn velocity_from_lag_phase(&self, phase_rad: f64) -> f64 {
        self.doppler_phase_convention.velocity_multiplier() * self.wavelength_m
            / (4.0 * std::f64::consts::PI * self.prt_s)
            * phase_rad
    }
}

/// Every moment this module produces for one gate of one dwell.
///
/// All fields are `f32` and use `NaN` for "not available here", which is not a
/// stylistic choice: it is the same censoring contract the rasteriser already
/// reads. `render2d`'s `f32` storage path skips any sample that is not finite,
/// so a censored gate arrives at the renderer as an empty pixel with no
/// translation step in between.
///
/// Every *moment* is censored together. The diagnostics - `power_h_db`,
/// `snr_h_db`, `snr_v_db` and `sqi` - are not, because they are what explains a
/// censored gate and none of them can be misread as weather.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateEstimate {
    pub range_m: f32,
    /// Total received power before noise subtraction. Read its reference from
    /// [`Self::power_reference`]; a relative file must never be labelled dBm.
    pub power_h_db: f32,
    pub power_reference: PowerReference,
    pub snr_h_db: f32,
    pub snr_v_db: f32,
    /// `|R(1)| / R(0)`, the normalised coherency, sometimes NCP. Near 1 for
    /// clutter and narrow spectra, near 0 for noise.
    pub sqi: f32,
    pub reflectivity_dbz: f32,
    pub velocity_mps: f32,
    pub spectrum_width_mps: f32,
    pub differential_reflectivity_db: f32,
    pub correlation_coefficient: f32,
    pub differential_phase_deg: f32,
    /// True when this gate's moments are blank, whether because the SNR censor
    /// hid them or because there was no signal above the noise to estimate.
    pub censored: bool,
    /// True when the gate's measured `R(0)` did not exceed the receiver noise
    /// power, so the estimated signal `S = R(0) - N` came out at or below zero
    /// and there was nothing to compute a moment from.
    ///
    /// Distinct from a censoring decision, and reported separately from it,
    /// because it happens under [`SnrCensor::Off`] as well - `Off` declines to
    /// apply a *threshold*, it cannot conjure a signal that the dwell did not
    /// measure. Roughly half the gates of a pure-noise dwell land here at 64
    /// pulses; none do above -1 dB SNR.
    pub below_noise: bool,
}

impl GateEstimate {
    /// The largest standard deviation any Doppler spectrum can have, given that
    /// it is confined to a single Nyquist interval of width `2 v_a`.
    ///
    /// The widest such distribution is the uniform one, whose standard
    /// deviation is `2 v_a / sqrt(12)`. Nothing measured through a pulse-pair
    /// estimator can honestly exceed it: a dwell of pure noise drives
    /// `S / |R(1)|` arbitrarily high and the Zrnic 1977 estimator, which is
    /// derived for a narrow Gaussian spectrum, then reports a width that no
    /// spectrum on this Nyquist interval could produce - 41 m/s has been
    /// measured against a ceiling of 19.2. Reporting the ceiling instead says
    /// "as wide as this waveform can see", which is true, rather than a number
    /// that is not.
    ///
    /// This is deliberately unlike the treatment of `rho_hv`, which is left
    /// unclamped above 1.0: there, exceeding the physical bound is the *only*
    /// signal that a gate was too weak for the estimator, and clipping it would
    /// disguise that as perfect correlation. Here the same information is
    /// already carried, unclipped, by `sqi` and `snr_h_db`.
    pub fn max_spectrum_width_mps(nyquist_velocity_mps: f64) -> f64 {
        2.0 * nyquist_velocity_mps / 12f64.sqrt()
    }

    fn blank(range_m: f32) -> Self {
        Self {
            range_m,
            power_h_db: f32::NAN,
            power_reference: PowerReference::RelativeStoredIqSquared,
            snr_h_db: f32::NAN,
            snr_v_db: f32::NAN,
            sqi: f32::NAN,
            reflectivity_dbz: f32::NAN,
            velocity_mps: f32::NAN,
            spectrum_width_mps: f32::NAN,
            differential_reflectivity_db: f32::NAN,
            correlation_coefficient: f32::NAN,
            differential_phase_deg: f32::NAN,
            censored: true,
            below_noise: false,
        }
    }
}

/// Pulse-pair moments for one gate.
///
/// `h` and `v` are one gate's samples across the dwell, in pulse order; `v` is
/// empty for a single-pol sweep. `range_m` is that gate's own range, which the
/// sweep states per bin rather than implying from an index - a Level 1 file may
/// record alternate bins.
pub fn estimate_gate(
    h: &[Complex],
    v: &[Complex],
    weights: &DwellWeights,
    geometry: &DwellGeometry,
    calibration: &MomentCalibration,
    censor: SnrCensor,
    range_m: f32,
) -> GateEstimate {
    if h.len() < 2 || h.len() != weights.pulses() {
        return GateEstimate::blank(range_m);
    }

    let r0h = weights.lag0(h);
    let r1h = weights.lag1(h);
    let mut estimate = GateEstimate::blank(range_m);
    estimate.power_reference = calibration.power_reference();
    estimate.power_h_db = (to_db(r0h) + f64::from(calibration.power_db_offset())) as f32;
    estimate.sqi = if r0h > 0.0 {
        (r1h.norm() / r0h) as f32
    } else {
        f32::NAN
    };

    // A relative cube has no receiver-noise measurement. Its received-power
    // field and lag-1 phase remain meaningful, but SNR censoring, dBZ, width
    // and calibrated dual-pol products do not. Returning here is the guard
    // against those absent capabilities quietly acquiring zero-valued
    // calibration constants.
    if matches!(calibration, MomentCalibration::RelativeStoredIq) {
        estimate.below_noise = false;
        estimate.censored = false;
        estimate.velocity_mps = if r1h.norm() > 0.0 {
            geometry.velocity_from_lag_phase(r1h.arg()) as f32
        } else {
            f32::NAN
        };
        return estimate;
    }

    let noise_h = calibration
        .noise_linear(0)
        .expect("absolute calibration carries horizontal noise");
    let noise_v = calibration
        .noise_linear(1)
        .expect("absolute calibration carries vertical noise");
    let signal_h = r0h - noise_h;
    let snr_h = if noise_h > 0.0 {
        signal_h / noise_h
    } else {
        f64::INFINITY
    };
    let snr_h_db = to_db(snr_h);

    estimate.snr_h_db = snr_h_db as f32;

    // Velocity obeys the censor exactly like every other moment. It is
    // tempting to exempt it - `arg R(1)` is an angle, not a magnitude, so it
    // returns a number at any SNR - but the number a noise gate returns is a
    // uniformly distributed angle, and a field of those is not weak data, it is
    // no data wearing the colours of data. Rendered, it fills the whole sweep
    // with red and green speckle that reads as a velocity field. The way to see
    // below the threshold is [`SnrCensor::Off`], which is deliberate and
    // visible; a per-moment exemption would not be.
    //
    // What survives censoring is the diagnostics - received power, SNR and
    // coherency - because those are what explain WHY a gate was hidden, and
    // none of them can be mistaken for weather.
    // Two separate events, reported separately. `signal_h <= 0` is not a
    // threshold decision at all: the dwell measured no power above the receiver
    // noise, so there is no signal from which a moment could be formed, and
    // that stays true with the censor off.
    estimate.below_noise = signal_h <= 0.0 || signal_h.is_nan();
    if censor.hides(snr_h_db) || estimate.below_noise {
        estimate.censored = true;
        return estimate;
    }
    estimate.censored = false;

    estimate.velocity_mps = geometry.velocity_from_lag_phase(r1h.arg()) as f32;

    // Z = 10 log10(S/N) + dBZ0 + 20 log10(r_km) + gaseous attenuation.
    let range_km = (f64::from(range_m).max(1.0)) / 1000.0;
    let MomentCalibration::Absolute {
        dbz0_db,
        zdr_offset_db,
        phidp_offset_deg,
        gaseous_attenuation_db_per_km,
        ..
    } = *calibration
    else {
        unreachable!("relative calibration returned before calibrated moments")
    };
    estimate.reflectivity_dbz = (snr_h_db
        + f64::from(dbz0_db)
        + 20.0 * range_km.log10()
        + f64::from(gaseous_attenuation_db_per_km) * range_km)
        as f32;

    // W = (lambda / 2 sqrt(2) pi T) sqrt(ln(S / |R(1)|)), Zrnic 1977. The log
    // argument goes below 1 whenever estimator noise makes |R(1)| exceed the
    // noise-subtracted power, which happens routinely at high SNR and narrow
    // spectra; clamping to zero width is the standard treatment and is what
    // keeps a coherent clutter gate at 0 m/s rather than NaN.
    //
    // The upper clamp is the same argument run the other way: the estimator is
    // unbounded above, but a Doppler spectrum is confined to one Nyquist
    // interval and so has a hard maximum standard deviation. See
    // [`GateEstimate::max_spectrum_width_mps`].
    let r1_magnitude = r1h.norm();
    estimate.spectrum_width_mps = if r1_magnitude > 0.0 {
        let ratio = signal_h / r1_magnitude;
        let log = ratio.ln().max(0.0);
        let scale = geometry.wavelength_m
            / (2.0 * std::f64::consts::SQRT_2 * std::f64::consts::PI * geometry.prt_s);
        let ceiling = GateEstimate::max_spectrum_width_mps(geometry.nyquist_velocity_mps());
        (scale * log.sqrt()).min(ceiling) as f32
    } else {
        f32::NAN
    };

    if v.len() == h.len() {
        let r0v = weights.lag0(v);
        let signal_v = r0v - noise_v;
        let snr_v = if noise_v > 0.0 {
            signal_v / noise_v
        } else {
            f64::INFINITY
        };
        let snr_v_db = to_db(snr_v);
        estimate.snr_v_db = snr_v_db as f32;

        if signal_v > 0.0 && !censor.hides(snr_v_db) {
            estimate.differential_reflectivity_db =
                (10.0 * (signal_h / signal_v).log10() + f64::from(zdr_offset_db)) as f32;

            let c0 = weights.cross_lag0(h, v);

            // rho_hv = |C(0)| / sqrt(S_h S_v). Identical to the form written as
            // |C(0)| / sqrt(R0_h R0_v) times sqrt((1 + 1/SNR_h)(1 + 1/SNR_v)),
            // since sqrt(1 + 1/SNR) = sqrt(R0 / S); this arrangement just avoids
            // multiplying a small number by a large one. Left unclamped on
            // purpose: an estimate above 1.0 is the signature of a gate too weak
            // for the estimator, and clipping it to 1.0 would disguise that as
            // perfect correlation. The SNR censor removes those gates by default.
            let denominator = (signal_h * signal_v).sqrt();
            estimate.correlation_coefficient = if denominator > 0.0 {
                (c0.norm() / denominator) as f32
            } else {
                f32::NAN
            };

            let phi = c0.arg().to_degrees() + f64::from(phidp_offset_deg);
            estimate.differential_phase_deg = wrap_degrees(phi) as f32;
        }
    }

    estimate
}

fn to_db(linear: f64) -> f64 {
    if linear > 0.0 {
        10.0 * linear.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// Wrap to (-180, 180].
fn wrap_degrees(degrees: f64) -> f64 {
    let wrapped = (degrees + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped <= -180.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calibration() -> MomentCalibration {
        MomentCalibration::absolute(-35.5, [-80.0, -80.0], 6.0, 0.0, 0.0, 0.0)
    }

    fn geometry() -> DwellGeometry {
        DwellGeometry {
            wavelength_m: 0.1108,
            prt_s: 833.375e-6,
            doppler_phase_convention: DopplerPhaseConvention::default(),
        }
    }

    /// A noiseless coherent tone at a chosen radial velocity, plus enough noise
    /// power to sit at the stated SNR.
    fn tone(
        pulses: usize,
        velocity_mps: f64,
        amplitude: f64,
        geometry: &DwellGeometry,
    ) -> Vec<Complex> {
        let phase_step =
            -4.0 * std::f64::consts::PI * velocity_mps * geometry.prt_s / geometry.wavelength_m;
        (0..pulses)
            .map(|k| Complex::from_polar(amplitude, phase_step * k as f64))
            .collect()
    }

    #[test]
    fn nyquist_velocity_follows_wavelength_over_four_prt() {
        let value = geometry().nyquist_velocity_mps();
        assert!((value - 33.238).abs() < 0.01, "{value}");
    }

    #[test]
    fn a_coherent_tone_returns_its_own_velocity() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        for target in [-30.0f64, -12.5, 0.0, 7.25, 31.0] {
            let h = tone(64, target, 1e-3, &geometry);
            let estimate = estimate_gate(
                &h,
                &[],
                &weights,
                &geometry,
                &calibration(),
                SnrCensor::Off,
                10_000.0,
            );
            assert!(
                (f64::from(estimate.velocity_mps) - target).abs() < 1e-3,
                "target {target} recovered {}",
                estimate.velocity_mps
            );
        }
    }

    #[test]
    fn relative_amplitude_rescaling_moves_power_but_not_velocity_or_fake_calibration() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 32);
        let low = tone(32, 7.25, 1.0, &geometry);
        let high = tone(32, 7.25, 10.0, &geometry);
        let low = estimate_gate(
            &low,
            &[],
            &weights,
            &geometry,
            &MomentCalibration::RelativeStoredIq,
            SnrCensor::MinDb(20.0),
            1_000.0,
        );
        let high = estimate_gate(
            &high,
            &[],
            &weights,
            &geometry,
            &MomentCalibration::RelativeStoredIq,
            SnrCensor::MinDb(20.0),
            1_000.0,
        );

        assert!((high.power_h_db - low.power_h_db - 20.0).abs() < 1.0e-4);
        assert!((high.velocity_mps - low.velocity_mps).abs() < 1.0e-5);
        assert!((low.velocity_mps - 7.25).abs() < 0.01);
        assert_eq!(low.power_reference, PowerReference::RelativeStoredIqSquared);
        assert!(low.snr_h_db.is_nan());
        assert!(low.reflectivity_dbz.is_nan());
        assert!(low.spectrum_width_mps.is_nan());
        assert!(
            !low.censored,
            "an unavailable SNR censor must not hide data"
        );
        assert!(!low.below_noise, "no unmeasured noise floor may be implied");
    }

    #[test]
    fn a_coherent_tone_has_zero_spectrum_width_and_unit_coherency() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        let h = tone(64, 5.0, 1e-3, &geometry);
        let estimate = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            10_000.0,
        );
        assert!(estimate.spectrum_width_mps.abs() < 1e-3);
        assert!((estimate.sqi - 1.0).abs() < 1e-3, "sqi {}", estimate.sqi);
    }

    #[test]
    fn velocity_folds_at_the_nyquist_interval_rather_than_running_away() {
        let geometry = geometry();
        let nyquist = geometry.nyquist_velocity_mps();
        let weights = DwellWeights::new(Taper::Rectangular, 32);
        let h = tone(32, nyquist + 5.0, 1e-3, &geometry);
        let estimate = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            10_000.0,
        );
        let expected = nyquist + 5.0 - 2.0 * nyquist;
        assert!(
            (f64::from(estimate.velocity_mps) - expected).abs() < 1e-3,
            "expected fold to {expected}, got {}",
            estimate.velocity_mps
        );
    }

    #[test]
    fn reflectivity_carries_the_twenty_log_r_range_correction() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 32);
        let h = tone(32, 0.0, 1e-3, &geometry);
        let near = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            10_000.0,
        );
        let far = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            100_000.0,
        );
        // Ten times the range is exactly 20 dB of range correction.
        assert!(
            (far.reflectivity_dbz - near.reflectivity_dbz - 20.0).abs() < 1e-3,
            "{} vs {}",
            far.reflectivity_dbz,
            near.reflectivity_dbz
        );
    }

    #[test]
    fn a_known_power_ratio_between_channels_is_the_differential_reflectivity() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        // 2 dB of ZDR is a voltage ratio of 10^(2/20).
        let h = tone(64, 3.0, 1e-3, &geometry);
        let v = tone(64, 3.0, 1e-3 / 10f64.powf(2.0 / 20.0), &geometry);
        let estimate = estimate_gate(
            &h,
            &v,
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            20_000.0,
        );
        assert!(
            (estimate.differential_reflectivity_db - 2.0).abs() < 0.02,
            "zdr {}",
            estimate.differential_reflectivity_db
        );
        assert!(
            (estimate.correlation_coefficient - 1.0).abs() < 0.01,
            "rho {}",
            estimate.correlation_coefficient
        );
    }

    #[test]
    fn differential_phase_reads_the_v_minus_h_argument_and_wraps() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        let h = tone(64, 0.0, 1e-3, &geometry);
        // v leads h by 40 degrees: PhiDP = arg(v conj(h)) = +40.
        let rotation = Complex::from_polar(1.0, 40f64.to_radians());
        let v: Vec<Complex> = h.iter().map(|z| *z * rotation).collect();
        let estimate = estimate_gate(
            &h,
            &v,
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            20_000.0,
        );
        assert!(
            (estimate.differential_phase_deg - 40.0).abs() < 1e-2,
            "phidp {}",
            estimate.differential_phase_deg
        );
    }

    /// The physical pin for the cross-correlation ordering, as opposed to the
    /// one above, which only restates the implementation's own definition of
    /// `C(0)`: swap the conjugate ordering and the comment with it, and
    /// `differential_phase_reads_the_v_minus_h_argument_and_wraps` still
    /// passes.
    ///
    /// Differential phase is a *propagation* effect. Oblate raindrops present a
    /// larger horizontal cross-section, so the horizontal wave travels slightly
    /// slower and accumulates phase delay along the path; PhiDP is therefore
    /// the integral of that difference and must INCREASE with range through
    /// rain (Bringi and Chandrasekar 2001, section 4.3). Here the horizontal
    /// channel is given exactly that: a lag of `k_dp * r` radians that grows
    /// gate by gate, with everything else - power, velocity, noise - identical
    /// between the channels. A reported PhiDP that fell with range would mean
    /// the estimator had the two channels the wrong way round.
    ///
    /// `crates/nexrad_io/tests/iq_moments_real.rs` makes the same measurement
    /// on real rain.
    #[test]
    fn differential_phase_rises_with_range_when_the_horizontal_channel_lags() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        // 1.5 degrees per km of two-way differential propagation phase, the
        // order of magnitude a heavy rain shaft produces at S band.
        let kdp_deg_per_km = 1.5f64;
        let mut previous = f64::NEG_INFINITY;
        let mut first = f64::NAN;
        let mut last = f64::NAN;
        for gate in 0..40 {
            let range_m = 1_000.0 + 500.0 * gate as f64;
            let lag = (kdp_deg_per_km * range_m / 1000.0).to_radians();
            let v = tone(64, 6.0, 1e-3, &geometry);
            // H lags V by the accumulated propagation phase.
            let rotation = Complex::from_polar(1.0, -lag);
            let h: Vec<Complex> = v.iter().map(|z| *z * rotation).collect();
            let estimate = estimate_gate(
                &h,
                &v,
                &weights,
                &geometry,
                &calibration(),
                SnrCensor::Off,
                range_m as f32,
            );
            let phi = f64::from(estimate.differential_phase_deg);
            assert!(
                phi > previous,
                "PhiDP must rise through rain: gate {gate} at {range_m} m gave {phi} after \
                 {previous}"
            );
            if gate == 0 {
                first = phi;
            }
            last = phi;
            previous = phi;
        }
        // And it rises at the rate it was built with: 39 gates of 500 m.
        let slope = (last - first) / (39.0 * 0.5);
        assert!(
            (slope - kdp_deg_per_km).abs() < 1e-3,
            "PhiDP slope {slope} deg/km against a built {kdp_deg_per_km}"
        );
    }

    #[test]
    fn spectrum_width_is_capped_at_the_widest_spectrum_one_nyquist_interval_holds() {
        let geometry = geometry();
        let nyquist = geometry.nyquist_velocity_mps();
        let ceiling = GateEstimate::max_spectrum_width_mps(nyquist);
        // 2 v_a / sqrt(12): the standard deviation of a spectrum spread evenly
        // over the whole unambiguous interval, which nothing can exceed.
        assert!((ceiling - 19.19).abs() < 0.01, "ceiling {ceiling}");

        // A fully decorrelated dwell. The phase step walks a full turn across
        // the dwell, so the 63 lag-1 products are 63 of the 64th roots of unity
        // and sum to a single leftover term: |R(1)| is R(0)/63, which is what
        // drives S / |R(1)| high enough for the unclamped Zrnic estimator to
        // run past the ceiling by 11 m/s.
        let calibration = calibration();
        let pulses = 64usize;
        let amplitude = (calibration.noise_linear(0).unwrap() * (1.0 + 100.0)).sqrt();
        let mut phase = 0.0f64;
        let samples: Vec<Complex> = (0..pulses)
            .map(|k| {
                let sample = Complex::from_polar(amplitude, phase);
                phase -= std::f64::consts::TAU * k as f64 / pulses as f64;
                sample
            })
            .collect();
        let weights = DwellWeights::new(Taper::Rectangular, pulses);
        let estimate = estimate_gate(
            &samples,
            &[],
            &weights,
            &geometry,
            &calibration,
            SnrCensor::Off,
            30_000.0,
        );
        // The dwell really is one the unclamped formula would run away on.
        let signal = weights.lag0(&samples) - calibration.noise_linear(0).unwrap();
        let unclamped = geometry.wavelength_m
            / (2.0 * std::f64::consts::SQRT_2 * std::f64::consts::PI * geometry.prt_s)
            * (signal / weights.lag1(&samples).norm()).ln().sqrt();
        assert!(
            unclamped > ceiling + 5.0,
            "this dwell should provoke the runaway; unclamped {unclamped} against {ceiling}"
        );
        assert!(
            (f64::from(estimate.spectrum_width_mps) - ceiling).abs() < 1e-3,
            "width {} should be pinned at the ceiling {ceiling}",
            estimate.spectrum_width_mps
        );
    }

    #[test]
    fn a_gate_with_no_power_above_the_noise_is_blanked_and_named_even_with_the_censor_off() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        let calibration = calibration();
        // Half the noise power: R(0) < N, so S is negative and no moment
        // exists. `Off` declines to apply a threshold; it cannot invent a
        // signal the dwell did not measure.
        let amplitude = (calibration.noise_linear(0).unwrap() * 0.5).sqrt();
        let h = tone(64, 4.0, amplitude, &geometry);
        let estimate = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration,
            SnrCensor::Off,
            30_000.0,
        );
        assert!(estimate.below_noise, "S = R(0) - N is negative here");
        assert!(estimate.censored);
        assert!(estimate.reflectivity_dbz.is_nan());
        assert!(estimate.velocity_mps.is_nan());
        // The diagnostics still explain it.
        assert!(estimate.power_h_db.is_finite());
        assert!(estimate.sqi.is_finite());

        // A gate with real signal is neither, whatever the censor says.
        let strong = estimate_gate(
            &tone(
                64,
                4.0,
                (calibration.noise_linear(0).unwrap() * 100.0).sqrt(),
                &geometry,
            ),
            &[],
            &weights,
            &geometry,
            &calibration,
            SnrCensor::Off,
            30_000.0,
        );
        assert!(!strong.below_noise);
        assert!(!strong.censored);

        // And a gate the THRESHOLD hides is censored without being below the
        // noise, which is the distinction the two flags exist to keep.
        let marginal = estimate_gate(
            &tone(
                64,
                4.0,
                (calibration.noise_linear(0).unwrap() * (1.0 + 10f64.powf(0.1))).sqrt(),
                &geometry,
            ),
            &[],
            &weights,
            &geometry,
            &calibration,
            SnrCensor::OPERATIONAL,
            30_000.0,
        );
        assert!(marginal.censored);
        assert!(!marginal.below_noise);
    }

    #[test]
    fn dwell_weights_carry_the_window_they_were_built_from() {
        for taper in Taper::ALL {
            let weights = DwellWeights::new(taper, 32);
            assert_eq!(weights.taper(), taper);
            // And the weights really are that window's, so nothing downstream
            // has to be told twice.
            assert_eq!(weights.weights(), taper.weights(32).as_slice());
        }
    }

    #[test]
    fn phase_wrapping_stays_inside_the_half_open_interval() {
        assert!((wrap_degrees(190.0) + 170.0).abs() < 1e-9);
        assert!((wrap_degrees(-190.0) - 170.0).abs() < 1e-9);
        assert!((wrap_degrees(180.0) - 180.0).abs() < 1e-9);
        assert!((wrap_degrees(540.0) - 180.0).abs() < 1e-9);
    }

    #[test]
    fn the_operational_threshold_hides_a_noise_gate_and_off_reveals_it() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        let calibration = calibration();
        // Amplitude chosen so total power sits 1 dB above noise: below the 2 dB
        // operational threshold, above nothing at all.
        // The synthetic tone carries no noise of its own, so the amplitude is
        // set to the TOTAL power the estimator will see: noise plus a signal
        // 1 dB above it. After the estimator subtracts the noise, SNR is 1 dB.
        let noise = calibration.noise_linear(0).unwrap();
        let amplitude = (noise * (1.0 + 10f64.powf(0.1))).sqrt();
        let h = tone(64, 4.0, amplitude, &geometry);
        let hidden = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration,
            SnrCensor::OPERATIONAL,
            30_000.0,
        );
        assert!(hidden.censored);
        assert!(hidden.reflectivity_dbz.is_nan());
        // Velocity is censored with the rest: a noise gate's `arg R(1)` is a
        // uniformly distributed angle, and drawing those fills a sweep with
        // speckle that reads as a velocity field.
        assert!(hidden.velocity_mps.is_nan());
        // The diagnostics survive censoring, which is the point of censoring
        // rather than dropping.
        assert!(hidden.power_h_db.is_finite());
        assert!(hidden.snr_h_db.is_finite());
        assert!(hidden.sqi.is_finite());

        let shown = estimate_gate(
            &h,
            &[],
            &weights,
            &geometry,
            &calibration,
            SnrCensor::Off,
            30_000.0,
        );
        assert!(!shown.censored);
        assert!(shown.reflectivity_dbz.is_finite());
        assert!(
            (shown.snr_h_db - 1.0).abs() < 0.05,
            "snr {}",
            shown.snr_h_db
        );
    }

    #[test]
    fn lag_normalisers_differ_for_a_tapered_dwell_and_agree_for_a_rectangular_one() {
        let rectangular = DwellWeights::new(Taper::Rectangular, 32);
        assert!((rectangular.lag0_norm - 32.0).abs() < 1e-9);
        assert!((rectangular.lag1_norm - 31.0).abs() < 1e-9);
        let hann = DwellWeights::new(Taper::VonHann, 32);
        assert!(hann.lag0_norm > 0.0 && hann.lag1_norm > 0.0);
        assert!((hann.lag0_norm - hann.lag1_norm).abs() > 1e-6);
    }

    #[test]
    fn every_window_recovers_the_same_velocity_from_the_same_tone() {
        let geometry = geometry();
        let h = tone(64, -18.0, 1e-3, &geometry);
        for taper in Taper::ALL {
            let weights = DwellWeights::new(taper, 64);
            let estimate = estimate_gate(
                &h,
                &[],
                &weights,
                &geometry,
                &calibration(),
                SnrCensor::Off,
                40_000.0,
            );
            assert!(
                (f64::from(estimate.velocity_mps) + 18.0).abs() < 1e-3,
                "{}: {}",
                taper.label(),
                estimate.velocity_mps
            );
        }
    }

    #[test]
    fn a_dwell_shorter_than_two_pulses_produces_a_blank_gate_rather_than_a_panic() {
        let weights = DwellWeights::new(Taper::Rectangular, 1);
        let estimate = estimate_gate(
            &[Complex::new(1.0, 0.0)],
            &[],
            &weights,
            &geometry(),
            &calibration(),
            SnrCensor::Off,
            1_000.0,
        );
        assert!(estimate.censored);
        assert!(estimate.velocity_mps.is_nan());
    }
}
