//! Data windows applied across the pulses of one dwell.
//!
//! The window is one of the two knobs this whole feature exists for (the other
//! being the dwell length), so it is a parameter of every estimator rather than
//! a constant chosen once inside them. What it buys is dynamic range in the
//! Doppler spectrum: a rectangular window leaks about -13 dB into the first
//! sidelobe, so a 60 dB ground-clutter return at zero velocity buries a weak
//! weather signal several bins away; von Hann drops that to -31 dB and Blackman
//! to -58 dB, at the cost of a wider main lobe and fewer independent samples.
//!
//! References for the coefficients and the sidelobe / equivalent-noise-bandwidth
//! figures: Harris 1978, Proc. IEEE 66, 51, "On the use of windows for harmonic
//! analysis with the discrete Fourier transform"; applied to weather radar in
//! Doviak and Zrnic, *Doppler Radar and Weather Observations*, 2nd ed. 1993,
//! section 6.4, and in Bringi and Chandrasekar, *Polarimetric Doppler Weather
//! Radar*, CUP 2001, section 5.3.
//!
//! All four are the **periodic** (DFT-symmetric, denominator `n`) forms rather
//! than the symmetric (denominator `n - 1`) forms. Periodic is the correct
//! choice when the window precedes a DFT, which is the only thing this module's
//! output is used for; the symmetric forms belong to FIR filter design.

/// The window applied across the pulses of a dwell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum Taper {
    /// No window. Narrowest main lobe, so the least spectrum-width bias and the
    /// most independent samples per dwell - and -13 dB sidelobes, so useless
    /// next to strong ground clutter. This is what an unwindowed pulse-pair
    /// estimator uses, and it is the default because it is the estimator the
    /// published formulas describe.
    #[default]
    Rectangular,
    /// Von Hann, `0.5 - 0.5 cos(2 pi k / n)`. -31 dB first sidelobe, 18 dB per
    /// octave rolloff. The usual default for a weather Doppler spectrum.
    VonHann,
    /// Hamming, `0.54 - 0.46 cos(2 pi k / n)`. -43 dB first sidelobe but only
    /// 6 dB per octave beyond it, so it beats von Hann close in and loses to it
    /// far out.
    Hamming,
    /// Blackman, `0.42 - 0.5 cos(2 pi k / n) + 0.08 cos(4 pi k / n)`. -58 dB
    /// sidelobes; the one to reach for when a spectrum has to show weather
    /// beside clutter that is 50 dB stronger.
    Blackman,
}

impl Taper {
    pub const ALL: [Self; 4] = [
        Self::Rectangular,
        Self::VonHann,
        Self::Hamming,
        Self::Blackman,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Rectangular => "rectangular",
            Self::VonHann => "von Hann",
            Self::Hamming => "Hamming",
            Self::Blackman => "Blackman",
        }
    }

    /// The window's weights for a dwell of `n` pulses.
    pub fn weights(self, n: usize) -> Vec<f64> {
        if n == 0 {
            return Vec::new();
        }
        let denominator = n as f64;
        (0..n)
            .map(|k| {
                let phase = std::f64::consts::TAU * k as f64 / denominator;
                match self {
                    Self::Rectangular => 1.0,
                    Self::VonHann => 0.5 - 0.5 * phase.cos(),
                    Self::Hamming => 0.54 - 0.46 * phase.cos(),
                    Self::Blackman => 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos(),
                }
            })
            .collect()
    }

    /// Equivalent noise bandwidth in DFT bins: `n * sum(w^2) / sum(w)^2`.
    ///
    /// The factor by which a window widens the noise band each spectral bin
    /// collects, and therefore the factor by which it reduces the number of
    /// independent samples in a dwell. Reported alongside a spectrum so the
    /// width read off it can be read honestly.
    pub fn equivalent_noise_bandwidth_bins(self, n: usize) -> f64 {
        if n == 0 {
            return 0.0;
        }
        let weights = self.weights(n);
        let sum: f64 = weights.iter().sum();
        let sum_sq: f64 = weights.iter().map(|w| w * w).sum();
        if sum == 0.0 {
            return 0.0;
        }
        n as f64 * sum_sq / (sum * sum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangular_is_all_ones() {
        assert_eq!(Taper::Rectangular.weights(5), vec![1.0; 5]);
    }

    #[test]
    fn periodic_windows_start_at_their_minimum_and_never_repeat_the_endpoint() {
        // The periodic form has w[0] at the minimum and w[n-1] strictly above
        // it; the symmetric form would put a second zero at w[n-1]. Getting
        // this backwards biases every windowed lag estimate by one sample.
        for taper in [Taper::VonHann, Taper::Hamming, Taper::Blackman] {
            let w = taper.weights(8);
            assert!(w[0] < 1e-9 || (taper == Taper::Hamming && (w[0] - 0.08).abs() < 1e-9));
            assert!(w[7] > w[0], "{}: w[7] should exceed w[0]", taper.label());
            assert!((w[4] - w.iter().cloned().fold(0.0, f64::max)).abs() < 1e-9);
        }
    }

    #[test]
    fn windows_are_symmetric_about_the_middle_sample() {
        for taper in Taper::ALL {
            let w = taper.weights(16);
            for k in 1..8 {
                assert!(
                    (w[k] - w[16 - k]).abs() < 1e-12,
                    "{} asymmetric at k={k}",
                    taper.label()
                );
            }
        }
    }

    #[test]
    fn equivalent_noise_bandwidth_matches_the_published_figures() {
        // Harris 1978, table 1: 1.00, 1.50, 1.36, 1.73 bins.
        let n = 512;
        let expected = [
            (Taper::Rectangular, 1.00),
            (Taper::VonHann, 1.50),
            (Taper::Hamming, 1.36),
            (Taper::Blackman, 1.73),
        ];
        for (taper, published) in expected {
            let enbw = taper.equivalent_noise_bandwidth_bins(n);
            assert!(
                (enbw - published).abs() < 0.01,
                "{}: ENBW {enbw:.4} bins, published {published}",
                taper.label()
            );
        }
    }

    #[test]
    fn zero_length_dwell_is_not_a_panic() {
        assert!(Taper::Blackman.weights(0).is_empty());
        assert_eq!(Taper::Blackman.equivalent_noise_bandwidth_bins(0), 0.0);
    }
}
