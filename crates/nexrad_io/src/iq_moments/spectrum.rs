//! The Doppler spectrum of one gate over one dwell.
//!
//! This is the thing no moment product contains. Level II hands over three
//! numbers per gate - power, mean velocity, width - which is a Gaussian fit to
//! something that is very often not Gaussian: a mesocyclone gate holds two
//! populations at different velocities, a hail core holds a broad pedestal, a
//! clutter-contaminated gate holds a zero-velocity spike sitting on top of the
//! weather. All three collapse to the same three numbers. The spectrum is what
//! shows the difference.
//!
//! References: Doviak and Zrnic, *Doppler Radar and Weather Observations*, 2nd
//! ed. 1993, section 6.4 (the periodogram and its moments); Bringi and
//! Chandrasekar, *Polarimetric Doppler Weather Radar*, CUP 2001, section 5.3;
//! Harris 1978, Proc. IEEE 66, 51 (window effect on the periodogram).
//!
//! # Normalisation
//!
//! Bin power is `|X_k|^2 / (n sum(w^2))`, which makes `sum_k P_k` exactly the
//! windowed lag-0 power `R(0)` that [`super::estimator::estimate_gate`]
//! computes from the same dwell. So the spectrum and the moments are two views
//! of one number rather than two independently scaled pictures, and an analyst
//! can read a bin's power against the gate's total and get an answer that adds
//! up. Keeping that identity is why [`super::fft::forward`] computes an exact
//! transform at any dwell length instead of zero-padding to a power of two.
//!
//! # Velocity axis
//!
//! A target receding at `v` returns `x[m] = exp(-i 4 pi v T m / lambda)`, so its
//! DFT line sits at `k / n = -v / 2 v_a` and bin `k` therefore carries
//! `v = -2 v_a k' / n`, where `k'` is `k` folded into `-n/2 .. n/2 - 1` and
//! `v_a = lambda / 4 T`. The minus sign is not decoration: dropping it mirrors
//! every spectrum about zero velocity, which leaves a plausible picture in which
//! inbound and outbound are swapped. The pinned check is that the spectral peak
//! must land on the pulse-pair velocity of the same dwell, and there are tests
//! for it at both the gate and the sweep level.
//!
//! The returned axis is sorted ascending and spans one full Nyquist interval,
//! `-v_a` (exclusive) to `+v_a` (inclusive) for an even dwell length.

use super::estimator::{DwellGeometry, DwellWeights, MomentCalibration};
use super::fft::{Complex, forward};
use super::taper::Taper;

/// A power spectrum against radial velocity for one gate of one dwell.
#[derive(Clone, Debug, PartialEq)]
pub struct DopplerSpectrum {
    pub range_m: f32,
    pub nyquist_velocity_mps: f32,
    /// Bin centres, ascending, `-v_a` to just under `+v_a`.
    pub velocities_mps: Vec<f32>,
    /// Bin powers, dBm, aligned with `velocities_mps`.
    pub power_dbm: Vec<f32>,
    /// The receiver noise floor for this channel, dBm - the level the spectrum
    /// sits on where there is no signal. Carried so a plot can draw it.
    pub noise_dbm: f32,
    /// The noise floor spread across `n` bins, dBm: what one *bin* of pure noise
    /// reads. This, not `noise_dbm`, is the line a spectrum plot should show,
    /// and the two differ by `10 log10(n)` - about 18 dB for a 64-pulse dwell.
    pub noise_per_bin_dbm: f32,
    /// The window the transform was actually taken through, read back off the
    /// weights rather than supplied alongside them.
    pub taper: Taper,
    /// Window equivalent noise bandwidth, in bins. A width read off the spectrum
    /// is broadened by roughly this much. Derived from [`Self::taper`], so it
    /// always describes the window the powers above were computed with.
    pub equivalent_noise_bandwidth_bins: f32,
}

impl DopplerSpectrum {
    /// The first three spectral moments: total power in dBm, power-weighted
    /// mean velocity, and power-weighted width.
    ///
    /// Computed with the circular (vector) mean rather than an arithmetic one,
    /// because velocity is an angle here: a spectrum straddling the Nyquist edge
    /// has an arithmetic mean of roughly zero and a circular mean at the fold,
    /// and only the second is right. This is the same estimate the pulse-pair
    /// lag-1 argument makes, arrived at from the other direction, which is what
    /// makes it useful as a check on both.
    pub fn moments(&self) -> SpectralMoments {
        let nyquist = f64::from(self.nyquist_velocity_mps);
        // Bin powers are dBm on a common offset, so summing them in linear
        // form and converting back gives a total on the same dBm scale without
        // this type needing to know the saturation level.
        let mut power_sum = 0.0f64;
        let mut vector = Complex::ZERO;
        for (velocity, power_dbm) in self.velocities_mps.iter().zip(self.power_dbm.iter()) {
            if !power_dbm.is_finite() {
                continue;
            }
            let power = 10f64.powf(f64::from(*power_dbm) / 10.0);
            let angle = std::f64::consts::PI * f64::from(*velocity) / nyquist;
            power_sum += power;
            vector += Complex::from_polar(power, angle);
        }
        if power_sum <= 0.0 {
            return SpectralMoments {
                power_dbm: f32::NEG_INFINITY,
                velocity_mps: f32::NAN,
                width_mps: f32::NAN,
            };
        }
        let mean_angle = vector.arg();
        let mean_velocity = mean_angle * nyquist / std::f64::consts::PI;

        // Second central moment about the circular mean, with each bin's offset
        // wrapped into +/- v_a first.
        let mut variance = 0.0f64;
        for (velocity, power_dbm) in self.velocities_mps.iter().zip(self.power_dbm.iter()) {
            if !power_dbm.is_finite() {
                continue;
            }
            let power = 10f64.powf(f64::from(*power_dbm) / 10.0);
            let mut offset = f64::from(*velocity) - mean_velocity;
            while offset > nyquist {
                offset -= 2.0 * nyquist;
            }
            while offset < -nyquist {
                offset += 2.0 * nyquist;
            }
            variance += power * offset * offset;
        }
        SpectralMoments {
            power_dbm: (10.0 * power_sum.log10()) as f32,
            velocity_mps: mean_velocity as f32,
            width_mps: (variance / power_sum).sqrt() as f32,
        }
    }
}

/// Moments read back off a spectrum. See [`DopplerSpectrum::moments`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectralMoments {
    /// Total power summed across every bin, dBm - the same quantity, and the
    /// same scale, as `GateEstimate::power_h_dbm` from the same dwell.
    pub power_dbm: f32,
    pub velocity_mps: f32,
    pub width_mps: f32,
}

/// The Doppler spectrum of one gate's dwell.
///
/// `samples` are that gate's complex samples across the dwell in pulse order;
/// `channel` picks which noise floor to report (0 horizontal, 1 vertical).
///
/// The window is not a separate argument. It used to be, and a caller could
/// then hand over one window's weights and a different window's name: the
/// transform would be computed through the first and the result labelled - and
/// its equivalent noise bandwidth reported - as the second, which is the figure
/// an analyst reads a spectral width against. The window now travels inside
/// [`DwellWeights`], so the label cannot disagree with the arithmetic.
pub fn gate_spectrum(
    samples: &[Complex],
    weights: &DwellWeights,
    geometry: &DwellGeometry,
    calibration: &MomentCalibration,
    channel: usize,
    range_m: f32,
) -> DopplerSpectrum {
    let n = samples.len();
    let taper = weights.taper();
    let nyquist = geometry.nyquist_velocity_mps();
    let noise_dbm = calibration.noise_dbm[channel.min(1)];
    if n == 0 || weights.pulses() != n {
        return DopplerSpectrum {
            range_m,
            nyquist_velocity_mps: nyquist as f32,
            velocities_mps: Vec::new(),
            power_dbm: Vec::new(),
            noise_dbm,
            noise_per_bin_dbm: noise_dbm,
            taper,
            equivalent_noise_bandwidth_bins: taper.equivalent_noise_bandwidth_bins(n) as f32,
        };
    }

    let window = weights.weights();
    let window_energy: f64 = window.iter().map(|w| w * w).sum();
    let windowed: Vec<Complex> = samples
        .iter()
        .zip(window.iter())
        .map(|(sample, weight)| sample.scale(*weight))
        .collect();
    let transformed = forward(&windowed);

    // sum_k |X_k|^2 = n sum_j |y_j|^2 (Parseval), so this scaling makes
    // sum_k P_k equal the windowed lag-0 power the estimator reports.
    let scale = if window_energy > 0.0 {
        1.0 / (n as f64 * window_energy)
    } else {
        0.0
    };
    let saturation = f64::from(calibration.saturation_dbm);

    let half = n.div_ceil(2);
    let mut velocities = Vec::with_capacity(n);
    let mut powers = Vec::with_capacity(n);
    // `k'` walks DOWN from `half - 1` to `half - n`, because velocity runs
    // opposite to DFT bin index; that makes the velocity axis come out
    // ascending without a sort.
    for shifted in 0..n {
        let signed = (half as isize - 1) - shifted as isize;
        let k = signed.rem_euclid(n as isize) as usize;
        let velocity = -2.0 * nyquist * signed as f64 / n as f64;
        let power = transformed[k].norm_sqr() * scale;
        velocities.push(velocity as f32);
        powers.push(if power > 0.0 {
            (10.0 * power.log10() + saturation) as f32
        } else {
            f32::NEG_INFINITY
        });
    }

    DopplerSpectrum {
        range_m,
        nyquist_velocity_mps: nyquist as f32,
        velocities_mps: velocities,
        power_dbm: powers,
        noise_dbm,
        noise_per_bin_dbm: noise_dbm - 10.0 * (n as f32).log10(),
        taper,
        equivalent_noise_bandwidth_bins: taper.equivalent_noise_bandwidth_bins(n) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iq_moments::estimator::{SnrCensor, estimate_gate};

    fn geometry() -> DwellGeometry {
        DwellGeometry {
            wavelength_m: 0.1108,
            prt_s: 833.375e-6,
        }
    }

    fn calibration() -> MomentCalibration {
        MomentCalibration {
            dbz0_db: -35.5,
            noise_dbm: [-80.0, -80.0],
            saturation_dbm: 6.0,
            zdr_offset_db: 0.0,
            phidp_offset_deg: 0.0,
            gaseous_attenuation_db_per_km: 0.0,
        }
    }

    /// A deterministic Gaussian-spectrum dwell: a sum of tones drawn from a
    /// Gaussian velocity distribution with fixed pseudo-random phases. The
    /// moments are known by construction.
    fn gaussian_dwell(
        pulses: usize,
        mean_mps: f64,
        width_mps: f64,
        power: f64,
        geometry: &DwellGeometry,
        seed: u64,
    ) -> Vec<Complex> {
        let components = 512;
        let mut state = seed | 1;
        let mut uniform = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut samples = vec![Complex::ZERO; pulses];
        let mut total_weight = 0.0;
        let mut weighted: Vec<(f64, f64, f64)> = Vec::with_capacity(components);
        for index in 0..components {
            // Deterministic sweep across +/- 4 sigma, so the discretisation of
            // the Gaussian is exact rather than sampled.
            let offset = -4.0 + 8.0 * index as f64 / (components - 1) as f64;
            let weight = (-0.5 * offset * offset).exp();
            let phase = std::f64::consts::TAU * uniform();
            weighted.push((mean_mps + offset * width_mps, weight, phase));
            total_weight += weight;
        }
        for (velocity, weight, phase) in weighted {
            let amplitude = (power * weight / total_weight).sqrt();
            let step =
                -4.0 * std::f64::consts::PI * velocity * geometry.prt_s / geometry.wavelength_m;
            for (k, sample) in samples.iter_mut().enumerate() {
                *sample += Complex::from_polar(amplitude, step * k as f64 + phase);
            }
        }
        samples
    }

    #[test]
    fn the_axis_spans_the_nyquist_interval_and_is_sorted() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        let samples = vec![Complex::new(1e-3, 0.0); 64];
        let spectrum = gate_spectrum(&samples, &weights, &geometry, &calibration(), 0, 25_000.0);
        assert_eq!(spectrum.velocities_mps.len(), 64);
        let nyquist = spectrum.nyquist_velocity_mps;
        assert!(
            spectrum
                .velocities_mps
                .windows(2)
                .all(|pair| pair[1] > pair[0]),
            "the axis must be ascending so it can be plotted as given"
        );
        assert!(spectrum.velocities_mps[0] > -nyquist);
        assert!((spectrum.velocities_mps[63] - nyquist).abs() < 1e-3);
        // One full Nyquist interval, less the one bin that -v_a and +v_a share.
        let span = spectrum.velocities_mps[63] - spectrum.velocities_mps[0];
        assert!(
            (span - 2.0 * nyquist * 63.0 / 64.0).abs() < 1e-3,
            "span {span}"
        );
    }

    #[test]
    fn a_tone_peaks_in_the_bin_that_carries_its_velocity() {
        let geometry = geometry();
        let nyquist = geometry.nyquist_velocity_mps();
        let weights = DwellWeights::new(Taper::Rectangular, 64);
        // Exactly on a bin centre so there is no leakage to argue about.
        let target = 2.0 * nyquist * 11.0 / 64.0;
        let step = -4.0 * std::f64::consts::PI * target * geometry.prt_s / geometry.wavelength_m;
        let samples: Vec<Complex> = (0..64)
            .map(|k| Complex::from_polar(1e-3, step * k as f64))
            .collect();
        let spectrum = gate_spectrum(&samples, &weights, &geometry, &calibration(), 0, 25_000.0);
        let peak = spectrum
            .power_dbm
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(index, _)| index)
            .expect("spectrum is not empty");
        assert!(
            (spectrum.velocities_mps[peak] - target as f32).abs() < 1e-2,
            "peak at {} m/s, tone at {target} m/s",
            spectrum.velocities_mps[peak]
        );
    }

    #[test]
    fn the_spectrum_sums_to_the_lag_zero_power_the_estimator_reports() {
        // The Parseval tie. Checked at non-power-of-two dwells as well, which
        // is the case that would break under zero-padding.
        let geometry = geometry();
        for pulses in [64usize, 50, 41] {
            for taper in Taper::ALL {
                let samples = gaussian_dwell(pulses, 6.0, 3.0, 1e-6, &geometry, 17);
                let weights = DwellWeights::new(taper, pulses);
                let spectrum =
                    gate_spectrum(&samples, &weights, &geometry, &calibration(), 0, 25_000.0);
                let estimate = estimate_gate(
                    &samples,
                    &[],
                    &weights,
                    &geometry,
                    &calibration(),
                    SnrCensor::Off,
                    25_000.0,
                );
                let difference =
                    f64::from(spectrum.moments().power_dbm) - f64::from(estimate.power_h_dbm);
                assert!(
                    difference.abs() < 1e-4,
                    "{} at {pulses} pulses: spectrum total and R(0) differ by {difference} dB",
                    taper.label()
                );
            }
        }
    }

    #[test]
    fn spectral_and_pulse_pair_moments_recover_the_same_synthetic_gaussian() {
        // A Gaussian spectrum of known mean and width, read two independent
        // ways: the lag-1 argument and the lag-0/lag-1 ratio on one side, the
        // power-weighted circular moments of the DFT on the other.
        //
        // The mean velocities are not merely close, they are the same number:
        // the inverse transform of the periodogram at lag 1 IS the lag-1
        // autocovariance, so `arg(sum_k P_k exp(-2 pi i k / n))` and
        // `arg(R(1))` are one quantity. The one thing that separates them is
        // that the periodogram's autocorrelation is circular where the direct
        // sum is linear, so the two agree to rounding only when the window
        // takes the endpoints to zero - which is why both sides are read
        // through von Hann here, and why a rectangular window would leave a
        // one-term difference. That identity failing is a real signal: it is
        // what a wrong velocity axis sign, or a mismatched normaliser, breaks.
        let geometry = geometry();
        let pulses = 256;
        let (mean, width) = (9.0f64, 2.5f64);
        let samples = gaussian_dwell(pulses, mean, width, 1e-6, &geometry, 5);
        let weights = DwellWeights::new(Taper::VonHann, pulses);

        let spectral =
            gate_spectrum(&samples, &weights, &geometry, &calibration(), 0, 25_000.0).moments();
        let pulse_pair = estimate_gate(
            &samples,
            &[],
            &weights,
            &geometry,
            &calibration(),
            SnrCensor::Off,
            25_000.0,
        );

        let disagreement = f64::from(spectral.velocity_mps) - f64::from(pulse_pair.velocity_mps);
        assert!(
            disagreement.abs() < 0.01,
            "spectral {} and pulse-pair {} disagree by {disagreement} m/s",
            spectral.velocity_mps,
            pulse_pair.velocity_mps
        );
        assert!(
            (f64::from(spectral.velocity_mps) - mean).abs() < 0.3,
            "spectral velocity {} against a designed mean of {mean}",
            spectral.velocity_mps
        );
        assert!(
            (f64::from(spectral.width_mps) - width).abs() < 0.4,
            "spectral width {}",
            spectral.width_mps
        );
        assert!(
            (f64::from(pulse_pair.spectrum_width_mps) - width).abs() < 0.4,
            "pulse-pair width {}",
            pulse_pair.spectrum_width_mps
        );
    }

    #[test]
    fn a_window_lowers_the_skirt_beside_a_strong_line() {
        // The reason the window is a parameter: a rectangular window leaks a
        // strong line across the whole spectrum, and a weak second population
        // several bins away disappears under it.
        let geometry = geometry();
        let pulses = 64;
        // Deliberately off a bin centre, which is when leakage is worst.
        let target = 2.0 * geometry.nyquist_velocity_mps() * 10.5 / pulses as f64;
        let step = -4.0 * std::f64::consts::PI * target * geometry.prt_s / geometry.wavelength_m;
        let samples: Vec<Complex> = (0..pulses)
            .map(|k| Complex::from_polar(1.0, step * k as f64))
            .collect();
        let mut skirts = Vec::new();
        for taper in [Taper::Rectangular, Taper::VonHann, Taper::Blackman] {
            let weights = DwellWeights::new(taper, pulses);
            let spectrum =
                gate_spectrum(&samples, &weights, &geometry, &calibration(), 0, 25_000.0);
            let peak = spectrum
                .power_dbm
                .iter()
                .cloned()
                .fold(f32::NEG_INFINITY, f32::max);
            let peak_index = spectrum
                .power_dbm
                .iter()
                .position(|value| *value == peak)
                .expect("peak exists");
            // Ten bins away from the line, relative to the line.
            let far = spectrum.power_dbm[(peak_index + 10) % pulses] - peak;
            skirts.push((taper, far));
        }
        assert!(
            skirts[1].1 < skirts[0].1 - 10.0,
            "von Hann should be at least 10 dB below rectangular ten bins out: {skirts:?}"
        );
        assert!(
            skirts[2].1 < skirts[1].1 - 5.0,
            "Blackman should be below von Hann ten bins out: {skirts:?}"
        );
    }

    #[test]
    fn the_window_a_spectrum_reports_is_the_one_it_was_computed_through() {
        // The label and the equivalent noise bandwidth are what an analyst
        // reads a spectral width against, so they have to describe the window
        // the transform was actually taken through. Both are checked here
        // against the weights themselves rather than against the name that was
        // asked for, which is the check that a separately-supplied window name
        // would fail.
        let geometry = geometry();
        let samples = gaussian_dwell(64, 6.0, 3.0, 1e-6, &geometry, 11);
        for taper in Taper::ALL {
            let weights = DwellWeights::new(taper, 64);
            let spectrum =
                gate_spectrum(&samples, &weights, &geometry, &calibration(), 0, 25_000.0);
            assert_eq!(spectrum.taper, taper);

            let window = weights.weights();
            let sum: f64 = window.iter().sum();
            let sum_sq: f64 = window.iter().map(|w| w * w).sum();
            let enbw = 64.0 * sum_sq / (sum * sum);
            assert!(
                (f64::from(spectrum.equivalent_noise_bandwidth_bins) - enbw).abs() < 1e-6,
                "{}: reported ENBW {} against {enbw} from the weights actually used",
                taper.label(),
                spectrum.equivalent_noise_bandwidth_bins
            );
        }
    }

    #[test]
    fn an_empty_dwell_returns_an_empty_spectrum_rather_than_panicking() {
        let geometry = geometry();
        let weights = DwellWeights::new(Taper::VonHann, 0);
        let spectrum = gate_spectrum(&[], &weights, &geometry, &calibration(), 0, 0.0);
        assert!(spectrum.velocities_mps.is_empty());
        assert!(spectrum.moments().velocity_mps.is_nan());
    }
}
