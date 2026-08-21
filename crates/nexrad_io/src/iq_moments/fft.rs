//! An in-tree discrete Fourier transform, sized for Doppler dwells.
//!
//! This exists because the workspace takes no new crate dependency for it, and
//! because a Doppler spectrum needs so little of what a general FFT library
//! offers: one direction, one precision, transform lengths in the tens to low
//! hundreds, called once per gate per dwell.
//!
//! Two algorithms:
//!
//! * [`forward_radix2`] - iterative decimation-in-time Cooley-Tukey for
//!   power-of-two lengths (Cooley and Tukey 1965, Math. Comp. 19, 297).
//! * [`forward`] - any length, falling back to Bluestein's chirp-z algorithm
//!   (Bluestein 1970, IEEE Trans. AU 18, 451; Rabiner, Schafer and Rader 1969,
//!   IEEE Trans. AU 17, 86) which expresses an `n`-point DFT as a convolution
//!   carried out by three power-of-two transforms.
//!
//! # Why Bluestein and not zero-padding
//!
//! Zero-padding a dwell of 50 pulses up to 64 is cheaper and is what most
//! display code does, but it does not compute the 50-point DFT: it computes the
//! 64-point DFT of a sequence that has been multiplied by a rectangular window,
//! so every spectral line is convolved with that window's kernel, the bin
//! spacing becomes `2 v_a / 64` instead of `2 v_a / 50`, and Parseval no longer
//! ties the spectrum back to the dwell power. This module is the numerical
//! backing for a spectrum that an analyst reads next to a pulse-pair moment
//! computed from the *same* dwell, so the two have to agree: with the exact
//! transform, `sum(|X_k|^2) / n == sum(|x_n|^2)`, and the spectral moments
//! recover the pulse-pair moments. That property is what pays for Bluestein.
//!
//! Zero-padding remains available to a caller who wants a smoother-looking
//! spectrum - pad the slice before calling - but it is then the caller's
//! choice, made visibly, rather than an artefact of the transform.

use std::f64::consts::PI;

/// A complex sample. Deliberately `f64`: a dwell spans the receiver's whole
/// dynamic range - about 87 dB on the 20 May 2013 KOUN files - and the moment
/// estimators subtract a noise power from a signal power that may be only a
/// fraction of a dB larger.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

impl Complex {
    pub const ZERO: Self = Self { re: 0.0, im: 0.0 };

    pub const fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    pub fn from_polar(magnitude: f64, radians: f64) -> Self {
        Self {
            re: magnitude * radians.cos(),
            im: magnitude * radians.sin(),
        }
    }

    pub fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }

    /// `|z|^2`. Preferred over `norm()` wherever a power is wanted: it avoids a
    /// square root that a later `10 log10` would only undo.
    pub fn norm_sqr(self) -> f64 {
        self.re * self.re + self.im * self.im
    }

    pub fn norm(self) -> f64 {
        self.norm_sqr().sqrt()
    }

    pub fn arg(self) -> f64 {
        self.im.atan2(self.re)
    }

    pub fn scale(self, factor: f64) -> Self {
        Self {
            re: self.re * factor,
            im: self.im * factor,
        }
    }
}

impl std::ops::Add for Complex {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            re: self.re + rhs.re,
            im: self.im + rhs.im,
        }
    }
}

impl std::ops::Sub for Complex {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            re: self.re - rhs.re,
            im: self.im - rhs.im,
        }
    }
}

impl std::ops::Mul for Complex {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Self {
            re: self.re * rhs.re - self.im * rhs.im,
            im: self.re * rhs.im + self.im * rhs.re,
        }
    }
}

impl std::ops::AddAssign for Complex {
    fn add_assign(&mut self, rhs: Self) {
        self.re += rhs.re;
        self.im += rhs.im;
    }
}

/// Forward DFT of any length: `X[k] = sum_n x[n] exp(-2 pi i n k / N)`.
///
/// Dispatches to [`forward_radix2`] when the length is a power of two and to
/// Bluestein otherwise. Lengths 0 and 1 are returned unchanged.
pub fn forward(input: &[Complex]) -> Vec<Complex> {
    let n = input.len();
    if n <= 1 {
        return input.to_vec();
    }
    if n.is_power_of_two() {
        let mut buffer = input.to_vec();
        forward_radix2(&mut buffer);
        return buffer;
    }
    bluestein(input)
}

/// In-place radix-2 forward DFT. Panics unless `buffer.len()` is a power of two
/// (or 0), which is a programming error rather than a data condition:
/// [`forward`] is the entry point that accepts any length.
pub fn forward_radix2(buffer: &mut [Complex]) {
    let n = buffer.len();
    if n <= 1 {
        return;
    }
    assert!(
        n.is_power_of_two(),
        "forward_radix2 needs a power-of-two length, got {n}; call `forward` for arbitrary lengths"
    );

    bit_reverse_permute(buffer);

    let mut half = 1usize;
    while half < n {
        let step = half * 2;
        // Principal root for this stage. Negative angle: forward transform.
        let theta = -PI / half as f64;
        for start in (0..n).step_by(step) {
            // Recurrence-free: recompute the twiddle per butterfly from a
            // trig call rather than iterating `w *= w_step`, which drifts by
            // roughly `log2(n)` ulps per stage. At these lengths the cost is
            // irrelevant and the accuracy is what the DFT pin test measures.
            for offset in 0..half {
                let twiddle = Complex::from_polar(1.0, theta * offset as f64);
                let even = buffer[start + offset];
                let odd = buffer[start + offset + half] * twiddle;
                buffer[start + offset] = even + odd;
                buffer[start + offset + half] = even - odd;
            }
        }
        half = step;
    }
}

fn bit_reverse_permute(buffer: &mut [Complex]) {
    let n = buffer.len();
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = (i as u64).reverse_bits() >> (64 - bits);
        let j = j as usize;
        if j > i {
            buffer.swap(i, j);
        }
    }
}

/// Bluestein's chirp-z transform: the exact `n`-point DFT for any `n`.
///
/// `n k = (n^2 + k^2 - (k - n)^2) / 2` turns the DFT into a convolution of
/// `x[n] exp(-i pi n^2 / N)` with `exp(+i pi m^2 / N)`, and the convolution is
/// done with three power-of-two transforms of length `m >= 2n - 1`.
fn bluestein(input: &[Complex]) -> Vec<Complex> {
    let n = input.len();
    let conv_len = (2 * n - 1).next_power_of_two();

    // exp(-i pi n^2 / N). `n^2` is reduced modulo `2N` first: the angle is
    // periodic in `2N`, and for a few hundred pulses `n * n` would otherwise
    // reach values where the fractional part of `n^2 / N` loses bits.
    let chirp: Vec<Complex> = (0..n)
        .map(|k| {
            let phase = -PI * ((k * k) % (2 * n)) as f64 / n as f64;
            Complex::from_polar(1.0, phase)
        })
        .collect();

    let mut a = vec![Complex::ZERO; conv_len];
    for (slot, (sample, chirp)) in a.iter_mut().zip(input.iter().zip(chirp.iter())) {
        *slot = *sample * *chirp;
    }

    // The kernel is the conjugate chirp, laid out so that index `m` and index
    // `conv_len - m` both carry `exp(+i pi m^2 / N)`; the cyclic convolution
    // then reproduces the linear one over the range that matters.
    let mut b = vec![Complex::ZERO; conv_len];
    for (m, chirp) in chirp.iter().enumerate() {
        let value = chirp.conj();
        b[m] = value;
        if m > 0 {
            b[conv_len - m] = value;
        }
    }

    forward_radix2(&mut a);
    forward_radix2(&mut b);
    for (slot, kernel) in a.iter_mut().zip(b.iter()) {
        *slot = *slot * *kernel;
    }
    inverse_radix2(&mut a);

    a.iter()
        .take(n)
        .zip(chirp.iter())
        .map(|(value, chirp)| *value * *chirp)
        .collect()
}

/// In-place inverse radix-2 DFT, normalised by `1/n`. Private: the only caller
/// is Bluestein's convolution step, and nothing about a Doppler spectrum needs
/// an inverse transform of its own.
fn inverse_radix2(buffer: &mut [Complex]) {
    for value in buffer.iter_mut() {
        *value = value.conj();
    }
    forward_radix2(buffer);
    let scale = 1.0 / buffer.len() as f64;
    for value in buffer.iter_mut() {
        *value = value.conj().scale(scale);
    }
}

/// The definition, evaluated term by term. Not used in the moment path - it is
/// `O(n^2)` - but it is what the fast transforms are pinned against, and it is
/// short enough to be read as the specification of what they should produce.
pub fn naive_dft(input: &[Complex]) -> Vec<Complex> {
    let n = input.len();
    (0..n)
        .map(|k| {
            let mut acc = Complex::ZERO;
            for (index, sample) in input.iter().enumerate() {
                let phase = -2.0 * PI * ((index * k) % n) as f64 / n as f64;
                acc += *sample * Complex::from_polar(1.0, phase);
            }
            acc
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reproducible pseudo-random complex sequence. A fixed 64-bit LCG so a
    /// failure is always the same failure; no dependency, and the statistics do
    /// not need to be good, only varied.
    fn pseudo_random(n: usize, seed: u64) -> Vec<Complex> {
        let mut state = seed | 1;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        (0..n).map(|_| Complex::new(next(), next())).collect()
    }

    fn worst_absolute_error(a: &[Complex], b: &[Complex]) -> f64 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (*x - *y).norm())
            .fold(0.0f64, f64::max)
    }

    /// The stated tolerance. Both transforms accumulate rounding over
    /// `log2(n)` (radix-2) or `3 log2(m)` (Bluestein) stages against a naive
    /// sum whose own error grows with `n`; `1e-9` on unit-scale input is
    /// several orders looser than either achieves and several orders tighter
    /// than anything a 16-bit packed-float I/Q sample could resolve.
    const DFT_TOLERANCE: f64 = 1e-9;

    #[test]
    fn radix2_matches_the_naive_dft_at_every_power_of_two_a_dwell_uses() {
        for &n in &[2usize, 4, 8, 16, 32, 64, 128, 256, 512, 1024] {
            let input = pseudo_random(n, n as u64 * 7 + 1);
            let mut fast = input.clone();
            forward_radix2(&mut fast);
            let slow = naive_dft(&input);
            let error = worst_absolute_error(&fast, &slow);
            assert!(
                error < DFT_TOLERANCE,
                "radix-2 vs naive DFT at n={n}: worst error {error:e} exceeds {DFT_TOLERANCE:e}"
            );
        }
    }

    #[test]
    fn bluestein_matches_the_naive_dft_at_non_power_of_two_dwell_lengths() {
        // 41, 50, 64 and 96 are real NEXRAD-family dwells; 3, 5, 7, 13 and 17
        // are the small primes that break a lazy Bluestein indexing.
        for &n in &[
            3usize, 5, 6, 7, 9, 13, 15, 17, 31, 41, 50, 63, 65, 96, 100, 129,
        ] {
            let input = pseudo_random(n, n as u64 * 31 + 5);
            let fast = forward(&input);
            let slow = naive_dft(&input);
            assert_eq!(fast.len(), n);
            let error = worst_absolute_error(&fast, &slow);
            assert!(
                error < DFT_TOLERANCE,
                "Bluestein vs naive DFT at n={n}: worst error {error:e} exceeds {DFT_TOLERANCE:e}"
            );
        }
    }

    #[test]
    fn parseval_holds_for_both_paths_so_a_spectrum_can_be_read_against_a_moment() {
        // This is the property that makes the spectrum comparable with the
        // pulse-pair estimate of the same dwell, and it is the reason this
        // module does not zero-pad on the caller's behalf.
        for &n in &[16usize, 50, 64, 41] {
            let input = pseudo_random(n, n as u64 + 99);
            let spectrum = forward(&input);
            let time_power: f64 = input.iter().map(|z| z.norm_sqr()).sum();
            let spectral_power: f64 = spectrum.iter().map(|z| z.norm_sqr()).sum::<f64>() / n as f64;
            assert!(
                (time_power - spectral_power).abs() <= 1e-9 * time_power.max(1.0),
                "Parseval at n={n}: {time_power} vs {spectral_power}"
            );
        }
    }

    #[test]
    fn a_pure_tone_lands_in_exactly_one_bin() {
        // exp(+2 pi i b k / n) is the sequence whose energy belongs in bin `b`
        // of a forward transform; the negative exponent lands in bin `n - b`.
        let n = 64;
        let bin = 9;
        let input: Vec<Complex> = (0..n)
            .map(|k| Complex::from_polar(1.0, 2.0 * PI * (bin * k) as f64 / n as f64))
            .collect();
        let spectrum = forward(&input);
        for (k, value) in spectrum.iter().enumerate() {
            let expected = if k == bin { n as f64 } else { 0.0 };
            assert!(
                (value.norm() - expected).abs() < 1e-9,
                "bin {k}: |X| = {} expected {expected}",
                value.norm()
            );
        }
    }

    #[test]
    fn short_inputs_pass_through() {
        assert!(forward(&[]).is_empty());
        let one = [Complex::new(3.0, -4.0)];
        assert_eq!(forward(&one), one.to_vec());
    }
}
