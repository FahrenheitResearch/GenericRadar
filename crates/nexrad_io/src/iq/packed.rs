//! The Vaisala RVP8/RVP900 16-bit packed floating point format that carries
//! every I/Q sample in a time-series (TS) record.
//!
//! # The rule, and the transposition trap
//!
//! There are two published statements of this format and they disagree. The
//! NOAA/ROC interface control document for Level 1 (ICD 2620076) states the
//! two branches TRANSPOSED with respect to the Vaisala manual it cites. The
//! Vaisala rule — implemented here, and the one the real files obey — is:
//!
//! ```text
//! bits 15..12  exponent  (4 bits, 0..15)
//! bits 11..0   mantissa  (12 bits, bit 11 doubling as the sign)
//!
//! exponent != 0   value = sign * (0x800 | mantissa[10..0]) * 2^(exponent-25)
//! exponent == 0   value = sext12(mantissa) * 2^-24
//! ```
//!
//! The exponent-zero branch is the denormal one: the low twelve bits are read
//! as a plain two's-complement integer, so the representable magnitudes run
//! from 0 up to 2047 x 2^-24. The exponent-one branch restores a hidden bit at
//! position 11 and picks up exactly where the denormals stop
//! (2048 x 2^-24), which is what makes the format continuous across the
//! branch and is the cheapest way to tell the two readings apart: under the
//! transposed reading the two branches overlap and the whole record collapses
//! into a band about 1e-4 wide with no dynamic range at all.
//!
//! Values are receiver voltage normalised so that unit magnitude corresponds
//! to the saturation power reported as `fSaturationDBM` in the record header;
//! a sample's power in dBm is therefore
//! `10*log10(i*i + q*q) + fSaturationDBM`. The representable magnitude tops
//! out just below 4, which is the headroom the RVP8 keeps above saturation.
//!
//! # Exactness
//!
//! Every decoded value is an integer of at most twelve significant bits
//! scaled by a power of two between 2^-24 and 2^-10, so all of them are
//! exactly representable in `f32` and the decode is exact rather than
//! approximate. That is what lets an independent implementation in another
//! language be compared for bit equality rather than for closeness.
//!
//! # References
//!
//! - Vaisala, *RVP900 Digital Receiver and Signal Processor User's Guide*,
//!   section 8 (time series record format); the same format is documented in
//!   the earlier RVP8 guide. Public at `ftp.sigmet.vaisala.com/files/manuals/`.
//! - NOAA/ROC, *Interface Control Document for the NEXRAD Level 1 Data*,
//!   ICD 2620076 — cited for context only; see the transposition note above.

/// Exponent bias: a normalised code's magnitude is scaled by
/// `2^(exponent - 25)`, so this is what the four-bit exponent field is
/// subtracted FROM to get the shift.
///
/// It is neither the field's width (four bits) nor how many values it holds
/// (sixteen). Saying so here would matter more than usual: the whole point of
/// this module is that two published statements of the format disagree about
/// the exponent, so a wrong word about it reads as a third.
const EXPONENT_BIAS: u32 = 25;
/// Hidden bit restored for a normalised (exponent != 0) code.
const HIDDEN_BIT: u16 = 0x0800;
/// Mantissa field of a normalised code, below the hidden bit.
const NORMAL_MANTISSA_MASK: u16 = 0x07FF;
/// Whole twelve-bit mantissa field, used as a signed integer when the
/// exponent is zero.
const DENORMAL_MANTISSA_MASK: u16 = 0x0FFF;

/// Decode one packed 16-bit code into normalised receiver voltage.
///
/// The result is exact: see the module note on exactness.
#[inline]
#[must_use]
pub fn unpack(code: u16) -> f32 {
    let exponent = u32::from(code >> 12);
    if exponent == 0 {
        // Denormal: the low twelve bits are a two's-complement integer, and
        // the sign bit of the format (bit 11) is simply its sign bit.
        let mantissa = i32::from(code & DENORMAL_MANTISSA_MASK);
        let signed = if mantissa >= 0x0800 {
            mantissa - 0x1000
        } else {
            mantissa
        };
        // 2^-24 exactly.
        signed as f32 / 16_777_216.0
    } else {
        // Normalised: restore the hidden bit, then apply bit 11 as a sign
        // rather than as part of the magnitude.
        let magnitude = i32::from(HIDDEN_BIT | (code & NORMAL_MANTISSA_MASK));
        let signed = if code & HIDDEN_BIT == 0 {
            magnitude
        } else {
            -magnitude
        };
        // exponent is 1..=15 here, so the shift distance is 10..=24 and the
        // divisor is an exact power of two well inside f32.
        let divisor = (1u32 << (EXPONENT_BIAS - exponent)) as f32;
        signed as f32 / divisor
    }
}

/// Decode a run of little-endian packed codes into interleaved I/Q floats.
///
/// `bytes` must hold a whole number of 16-bit codes. RVP8 time-series records
/// are written by the processor's own (Intel) host, so the codes are always
/// little-endian; there is no byte-order flag in the record to consult and no
/// producer that writes them big-endian.
pub fn unpack_all(bytes: &[u8], out: &mut Vec<f32>) {
    out.clear();
    out.reserve(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push(unpack(u16::from_le_bytes([pair[0], pair[1]])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deliberately naive restatement of the Vaisala rule, written from the
    /// manual's wording rather than from the implementation above, and using
    /// `f64` arithmetic and `powi` instead of an integer shift. If the two
    /// agree on all 65,536 codes then the fast path's shift arithmetic and
    /// its sign handling are both right.
    fn reference(code: u16) -> f64 {
        let exponent = i32::from(code >> 12);
        let mantissa = i32::from(code & 0x0FFF);
        if exponent == 0 {
            let signed = if mantissa >= 2048 {
                mantissa - 4096
            } else {
                mantissa
            };
            f64::from(signed) * 2f64.powi(-24)
        } else {
            let magnitude = 2048 + i32::from(code & 0x07FF);
            let signed = if code & 0x0800 == 0 {
                magnitude
            } else {
                -magnitude
            };
            f64::from(signed) * 2f64.powi(exponent - 25)
        }
    }

    #[test]
    fn every_code_matches_the_naive_restatement_of_the_rule() {
        for code in 0..=u16::MAX {
            let fast = unpack(code);
            let slow = reference(code);
            assert_eq!(
                f64::from(fast),
                slow,
                "code {code:#06x} decoded to {fast} but the rule says {slow}"
            );
        }
    }

    #[test]
    fn the_decode_is_exact_in_f32() {
        // Every value is a <=12-bit integer times a power of two, so the f64
        // reference must be exactly representable in f32 with no rounding.
        for code in 0..=u16::MAX {
            let slow = reference(code);
            assert_eq!(f64::from(slow as f32), slow, "code {code:#06x} rounded");
        }
    }

    #[test]
    fn denormals_join_the_first_normal_binade_without_a_gap() {
        // The largest denormal and the smallest normal magnitude must be one
        // quantum apart. This is the property that distinguishes the Vaisala
        // reading from the transposed one in the NOAA ICD: get the branches
        // the wrong way round and the two ranges overlap instead.
        let largest_denormal = unpack(0x07FF);
        let smallest_normal = unpack(0x1000);
        let quantum = 2f32.powi(-24);
        assert_eq!(largest_denormal, 2047.0 * quantum);
        assert_eq!(smallest_normal, 2048.0 * quantum);
        assert_eq!(smallest_normal - largest_denormal, quantum);
    }

    #[test]
    fn sign_is_symmetric_about_zero() {
        assert_eq!(unpack(0x0000), 0.0);
        // Denormal +1 and -1 quantum.
        assert_eq!(unpack(0x0001), 2f32.powi(-24));
        assert_eq!(unpack(0x0FFF), -(2f32.powi(-24)));
        // Normalised: bit 11 flips the sign and nothing else.
        assert_eq!(unpack(0x1000), -unpack(0x1800));
        assert_eq!(unpack(0xF7FF), -unpack(0xFFFF));
    }

    #[test]
    fn full_scale_is_the_documented_headroom_above_saturation() {
        // Largest magnitude the format can hold: exponent 15, mantissa all
        // ones. Unit magnitude is saturation, so the format keeps a little
        // under 12 dB of headroom above it.
        assert_eq!(unpack(0xF7FF), 4095.0 / 1024.0);
        assert!((unpack(0xF7FF) - 3.999_023_4).abs() < 1e-6);
    }

    #[test]
    fn unpack_all_reads_little_endian_pairs() {
        let mut out = Vec::new();
        // 0x1000 little-endian is 0x00, 0x10.
        unpack_all(&[0x00, 0x10, 0x01, 0x00], &mut out);
        assert_eq!(out, vec![unpack(0x1000), unpack(0x0001)]);
    }
}
