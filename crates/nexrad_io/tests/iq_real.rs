//! Golden-fixture tests for the Level 1 / I/Q reader against REAL NEXRAD
//! time-series records.
//!
//! # Fixture provenance
//!
//! Both fixtures are the leading pulses of files published by the NSSL
//! research data archive at `data.nssl.noaa.gov` (THREDDS branch
//! `RRDD/KOUN/2013/KOUN_20130520/IQ/`), whose catalogue rights statement
//! reads "Freely available". They are from KOUN — the polarimetric research
//! WSR-88D at Norman, Oklahoma — on 20 May 2013. Each is truncated on an
//! exact pulse boundary, so the stride rule still runs to the last byte:
//!
//! - `KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250.head24`:
//!   first 24 of 1,830 pulses of a 4,434,431-byte record (60,051 bytes kept).
//!   19:46:01.730 UTC, elevation 4.0 degrees, `iMajorMode` 0, `iPolarization`
//!   3 (H+V), PRT 833.4 us. Its `iRangeMask` is 62 words of `0x5555` plus a
//!   `0x0055`: 500 ALTERNATE bins of a 250 m mask, so the recorded gates are
//!   500 m apart. This is the fixture that catches a reader which assumes
//!   contiguous gates — it would place every gate at half its true range.
//! - `KOUN_RVP.20130520.224139.456.Ascope_DEFAULT.0.H+V.150.head8`:
//!   first 8 of 1,340 pulses of a 6,975,676-byte record (43,450 bytes kept).
//!   22:41:39.456 UTC, elevation 4.83 degrees, `iMajorMode` 13, PRT 1000.1 us.
//!   Its mask is 600 CONTIGUOUS bins at 250 m, `iNumVecs` is 598 rather than
//!   250, and `iSampleSize` is 128 rather than 32 — a deliberately different
//!   acquisition, here so the reader cannot be tuned to one file.
//!
//! # How the expected values were produced
//!
//! Every sample value below was extracted with an INDEPENDENT implementation
//! of the Vaisala packed-float rule written in Python/NumPy, not with this
//! crate. The two were also compared value by value over the WHOLE of the
//! first reference record as published, not merely over the fixture: 1,830
//! pulses at `iNumVecs * iVIQPerBin * 2` = 1,000 values each, so 1,830,000
//! values in the file, of which the 1,822,680 this reader exposes — both
//! channels' burst reference and all 248 gates of every pulse — were compared
//! and agreed on every bit. The 7,320 it does not expose are the first sample
//! of each burst preamble, decoded by the same function that `iq::packed`
//! pins exhaustively against a naive restatement of the rule over all 65,536
//! codes.
//!
//! Bit equality rather than closeness is possible because the format's values
//! are all small integers scaled by powers of two, and so are exact in `f32`.
//! Over that record no decoded `I` or `Q` exceeds 1.9975585938 in magnitude
//! and no sample exceeds 2.0509 — comfortably inside the format's ceiling just
//! under 4, which is what
//! [`no_decoded_sample_exceeds_the_formats_headroom_above_saturation`] holds
//! the line at.
//!
//! Decoded SAMPLES are therefore asserted exactly, via [`assert_iq`]: a
//! tolerance on them would hide a wrong exponent, which is the mistake this
//! format invites. Quantities DERIVED from the header's integers — angles,
//! intervals, ranges — are asserted with a tolerance via [`assert_close`],
//! because what is worth claiming about those is that the conversion is
//! right, not that a particular rounding happened.

use nexrad_io::iq::{
    IqSweep, MAJOR_MODE_BATCH_STAGGERED_PRT, MAJOR_MODE_SZ2_PHASE_CODED, decode_iq_time_series,
    decode_iq_time_series_limited, looks_like_iq_time_series, peek_iq_time_series,
};

const ALTERNATE_MASK: &[u8] =
    include_bytes!("data/KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250.head24");
const CONTIGUOUS_MASK: &[u8] =
    include_bytes!("data/KOUN_RVP.20130520.224139.456.Ascope_DEFAULT.0.H+V.150.head8");

/// Assert a decoded `(I, Q)` pair equals what the independent Python
/// implementation produced, EXACTLY.
///
/// The expected values are written as `f64` and the decoded ones widened to
/// meet them. That is not a loosening: every value the packed format can hold
/// is a small integer scaled by a power of two, so it is exact in both types
/// and the widening is lossless. Writing them as `f64` is what lets the full
/// decimal expansion stay in the file — an `f32` literal carrying every digit
/// reads as excessive precision, and truncating it would hide which decoded
/// value was actually being claimed.
#[track_caller]
fn assert_iq(actual: (f32, f32), expected: (f64, f64), what: &str) {
    assert_eq!(
        (f64::from(actual.0), f64::from(actual.1)),
        expected,
        "{what}"
    );
}

/// Assert a derived quantity is within `tolerance`.
///
/// Used for angles and intervals rather than for samples: those are the
/// result of arithmetic on the header's integers, so the claim worth making
/// about them is that the conversion is right, not that a particular rounding
/// happened.
#[track_caller]
fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

/// One gate's pulse-pair moments over a whole fixture's dwell.
struct GateMoments {
    snr_h_db: f64,
    velocity_m_s: f64,
    width_m_s: f64,
    zdr_db: f64,
    rho_hv: f64,
    phi_dp_deg: f64,
}

/// Pulse-pair moments per gate, from the record's samples and its own
/// calibration — nothing reached back out of the file for.
///
/// The estimators are the standard ones (Doviak and Zrnić 1993 ch. 6; Zrnić
/// 1977; Bringi and Chandrasekar 2001 ch. 5-6), noise-corrected with the
/// record's `fNoiseDBm`:
///
/// ```text
/// S = R(0) - N          SNR = S / N
/// V = (lambda / 4 pi T) arg R(1)
/// W = (lambda / 2 sqrt(2) pi T) sqrt(ln(S / |R(1)|))
/// ZDR = 10 log10(S_h / S_v)
/// rho_hv = |C(0)| / sqrt(S_h S_v) / sqrt((1 + 1/SNR_h)(1 + 1/SNR_v))
/// Phi_DP = arg C(0)
/// ```
///
/// This lives in a test rather than in the crate on purpose: the reader hands
/// a processor samples, and what this proves is that the samples it hands over
/// are the ones the antenna measured, placed where the antenna measured them.
fn pulse_pair(sweep: &IqSweep) -> Vec<GateMoments> {
    let noise_h = f64::from(sweep.noise_power(0).expect("calibrated H noise"));
    let noise_v = f64::from(sweep.noise_power(1).expect("calibrated V noise"));
    let prt = f64::from(sweep.pulses[0].prt_seconds);
    let wavelength = f64::from(sweep.wavelength_m);
    let pulses = sweep.pulses.len() as f64;

    (0..sweep.gate_count())
        .map(|gate| {
            let mut r0_h = 0.0;
            let mut r0_v = 0.0;
            let mut r1 = (0.0, 0.0);
            let mut c0 = (0.0, 0.0);
            let mut previous: Option<(f64, f64)> = None;
            for pulse in &sweep.pulses {
                let h = (f64::from(pulse.h[gate].0), f64::from(pulse.h[gate].1));
                let v = (f64::from(pulse.v[gate].0), f64::from(pulse.v[gate].1));
                r0_h += h.0 * h.0 + h.1 * h.1;
                r0_v += v.0 * v.0 + v.1 * v.1;
                // H * conj(V), the co-polar cross-correlation at lag zero.
                c0.0 += h.0 * v.0 + h.1 * v.1;
                c0.1 += h.1 * v.0 - h.0 * v.1;
                if let Some(last) = previous {
                    // H[m] * conj(H[m-1]), the lag-one autocorrelation.
                    r1.0 += h.0 * last.0 + h.1 * last.1;
                    r1.1 += h.1 * last.0 - h.0 * last.1;
                }
                previous = Some(h);
            }
            let r0_h = r0_h / pulses;
            let r0_v = r0_v / pulses;
            let r1 = (r1.0 / (pulses - 1.0), r1.1 / (pulses - 1.0));
            let c0 = (c0.0 / pulses, c0.1 / pulses);

            let signal_h = (r0_h - noise_h).max(1e-30);
            let signal_v = (r0_v - noise_v).max(1e-30);
            let snr_h = signal_h / noise_h;
            let snr_v = signal_v / noise_v;
            let r1_magnitude = r1.0.hypot(r1.1);
            GateMoments {
                snr_h_db: 10.0 * snr_h.log10(),
                velocity_m_s: wavelength / (4.0 * std::f64::consts::PI * prt) * r1.1.atan2(r1.0),
                width_m_s: wavelength / (2.0 * 2f64.sqrt() * std::f64::consts::PI * prt)
                    * (signal_h / r1_magnitude).ln().max(0.0).sqrt(),
                zdr_db: 10.0 * (signal_h / signal_v).log10(),
                rho_hv: c0.0.hypot(c0.1)
                    / (signal_h * signal_v).sqrt()
                    / ((1.0 + 1.0 / snr_h) * (1.0 + 1.0 / snr_v)).sqrt(),
                phi_dp_deg: c0.1.atan2(c0.0).to_degrees(),
            }
        })
        .collect()
}

/// Median of `values`, which the moment checks use instead of a mean so one
/// wild gate cannot carry the claim.
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Mean power in dBm over the last `gates` range bins of every pulse.
///
/// Averaged in linear power and then converted, which is the only correct
/// order: the per-sample power of a noise-only gate is exponentially
/// distributed, and averaging its decibels instead biases the answer about
/// 2.5 dB low.
fn far_range_mean_dbm(sweep: &IqSweep, vertical: bool, gates: usize) -> f32 {
    let mut total = 0.0f64;
    let mut count = 0usize;
    for pulse in &sweep.pulses {
        let channel = if vertical { &pulse.v } else { &pulse.h };
        for (i, q) in channel.iter().rev().take(gates) {
            total += f64::from(*i) * f64::from(*i) + f64::from(*q) * f64::from(*q);
            count += 1;
        }
    }
    let mean = total / count as f64;
    (10.0 * mean.log10()) as f32
        + sweep
            .calibration
            .saturation_dbm()
            .expect("RVP fixture is absolutely calibrated")
}

#[test]
fn the_alternate_mask_record_decodes_with_its_documented_header() {
    let sweep = decode_iq_time_series(ALTERNATE_MASK).unwrap();

    assert_eq!(sweep.site, "KOUN_RVP");
    assert_eq!(sweep.task_name, "Ascope_DEFAULT");
    assert_eq!(sweep.processor_version, "8.12.8");
    assert_eq!(sweep.major_mode, Some(0));
    assert_eq!(sweep.polarization_code, Some(3));
    assert_eq!(sweep.channels_recorded, 2);
    assert!(sweep.is_dual_channel());
    assert_eq!(sweep.nominal_sample_size, 32);

    // fWavelengthCM=11.08, fPWidthUSec=1.5, fDBzCalib=-35.5,
    // fSaturationDBM=6, fRangeMaskRes=250, fNoiseDBm=-80.5555 -80.5955.
    assert_eq!(sweep.wavelength_m, 0.1108);
    assert_eq!(sweep.pulse_width_s, Some(1.5e-6));
    assert_eq!(sweep.calibration.dbz_calibration(), Some(-35.5));
    assert_eq!(sweep.calibration.saturation_dbm(), Some(6.0));
    assert_eq!(sweep.range_mask_res_m, 250.0);
    assert_eq!(sweep.calibration.noise_dbm(), Some([-80.5555, -80.5955]));

    assert_eq!(sweep.pulses.len(), 24);
    assert_eq!(sweep.time_utc, 1_369_079_161);
    assert_eq!(sweep.time_millis, 730);
}

#[test]
fn the_alternate_mask_places_gates_at_five_hundred_metres_not_two_fifty() {
    // iRangeMask sets 500 bits at a 250 m resolution, but only every other
    // one: bins 0, 2, 4, ... So the recorded gates are 500 m apart even
    // though fRangeMaskRes says 250. iNumVecs is 250, and the recorded
    // samples take mask positions in order, the first TWO of them holding the
    // burst — so 248 gates follow from mask bin 4 at 1000, 1500, 2000 ... m.
    let sweep = decode_iq_time_series(ALTERNATE_MASK).unwrap();
    assert_eq!(sweep.burst_samples, 2);
    assert_eq!(sweep.gate_count(), 248);
    assert_eq!(sweep.gate_spacing_m, Some(500.0));
    assert_eq!(sweep.first_gate_m, 1000.0);
    assert_eq!(sweep.range_bins[0], 1000.0);
    assert_eq!(sweep.range_bins[1], 1500.0);
    assert_eq!(sweep.range_bins[2], 2000.0);
    assert_eq!(*sweep.range_bins.last().unwrap(), 124_500.0);

    // The last recorded gate sits inside the unambiguous range, and within one
    // gate of it, which is what an acquisition that runs to r_a looks like.
    let r_max = sweep.unambiguous_range_m().unwrap();
    assert!((124_919.0..125_000.0).contains(&r_max), "{r_max}");
    assert!(*sweep.range_bins.last().unwrap() < r_max);
    assert!(r_max - *sweep.range_bins.last().unwrap() < 500.0);
}

#[test]
fn i_max_vecs_does_not_decide_where_the_gates_start() {
    // Both records carry iMaxVecs exactly one more than the bits set in
    // iRangeMask — 501 against 500, and 601 against 600 — while reserving TWO
    // samples for the burst. Reading the preamble off that relationship gives
    // one sample, and one sample is wrong: it hands the transmit pulse over as
    // gate 0 and pushes every real gate one recorded bin further out, 500 m
    // here and 250 m in the contiguous record. Pin the counts that rule would
    // have produced so that reading can never come back quietly.
    for (name, bytes, wrong_gates, wrong_first_range) in [
        ("alternate mask", ALTERNATE_MASK, 249, 500.0),
        ("contiguous mask", CONTIGUOUS_MASK, 597, 250.0),
    ] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        assert_ne!(sweep.gate_count(), wrong_gates, "{name}");
        // A one-sample preamble leaves the LAST gate where it belongs, so the
        // near end is what tells the two readings apart: it would keep one
        // burst sample as gate 0 and label it with the range of a real gate.
        assert_ne!(sweep.range_bins[0], wrong_first_range, "{name}");
        assert_eq!(sweep.burst_samples, 2, "{name}");
    }
}

#[test]
fn gate_zero_is_a_measured_gate_and_not_the_transmit_pulse() {
    // The cheapest test that catches a preamble read one sample short, and
    // the one this reader did not have. The burst is a single coupled sample
    // written into BOTH channel blocks, so the two channels are bit-identical
    // through the preamble; two independent receiver chains do not produce
    // bit-identical 12-bit I/Q from the sky.
    //
    // The gates checked here are the first three, which in both records are
    // the T/R switch recovering — within a few dB of saturation. Coincidence
    // at that level is not a possibility. It is at the noise floor: over the
    // whole 1,830-pulse record exactly one of 453,840 gate comparisons
    // matches, at a pure-noise gate 77.5 km out, so this claim is made about
    // lit gates rather than about every gate in a file.
    for (name, bytes) in [
        ("alternate mask", ALTERNATE_MASK),
        ("contiguous mask", CONTIGUOUS_MASK),
    ] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        for (index, pulse) in sweep.pulses.iter().enumerate() {
            let burst = pulse.burst.expect("the record reserves a burst preamble");
            assert_eq!(
                Some(burst.h),
                burst.v,
                "{name} pulse {index}: the burst is one coupled sample and must \
                 read the same on both channels"
            );
            for gate in 0..3 {
                assert_ne!(
                    pulse.h[gate], pulse.v[gate],
                    "{name} pulse {index} gate {gate}: H and V are bit-identical, \
                     so this is the burst being served as a gate"
                );
            }
        }
    }
}

#[test]
fn the_burst_is_the_sample_whose_magnitude_the_header_reports() {
    // The processor phase-references each record to the burst, so the burst
    // sample has Q exactly zero and its magnitude is what the header writes
    // as RX[n].fBurstMag. That identifies WHICH preamble sample is the burst:
    // the last one. The sample before it is coherent too — it is the leading
    // edge of the same burst window — but reads 11.7% high in the alternate
    // record and 13.4% high in the contiguous one, at 119 degrees rather than
    // zero, so a reader that returned it would put a wrong transmitter phase
    // into every magnetron or drift correction built on it. The tolerance
    // below is a thousandth: tight enough that the earlier sample misses it by
    // two orders of magnitude, loose enough for quantisation.
    for (name, bytes) in [
        ("alternate mask", ALTERNATE_MASK),
        ("contiguous mask", CONTIGUOUS_MASK),
    ] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        for (index, pulse) in sweep.pulses.iter().enumerate() {
            let burst = pulse.burst.unwrap();
            assert_eq!(burst.preamble_samples, 2, "{name} pulse {index}");
            assert_eq!(burst.h.1, 0.0, "{name} pulse {index}: burst Q");
            let reported = burst.reported_magnitude[0];
            assert!(
                reported > 0.0,
                "{name} pulse {index}: no reported magnitude"
            );
            let error = (burst.h.0.abs() - reported).abs() / reported;
            assert!(
                error < 1e-3,
                "{name} pulse {index}: decoded burst |{}| against a reported \
                 {reported}, relative error {error}",
                burst.h.0,
            );
        }
    }
}

#[test]
fn the_alternate_mask_record_matches_the_independent_python_decode() {
    let sweep = decode_iq_time_series(ALTERNATE_MASK).unwrap();
    let first = &sweep.pulses[0];

    // iAz=60335 and iEl=728 as 16-bit binary angles.
    assert_close(first.azimuth_deg, 331.430_05, 1e-4, "pulse 0 azimuth");
    assert_close(first.elevation_deg, 3.999_023_4, 1e-4, "pulse 0 elevation");
    // iNextPRT=59950 ticks of the 71.9364 MHz acquisition clock.
    assert_close(first.prt_seconds * 1e6, 833.375, 1e-3, "pulse 0 PRT in us");
    assert_eq!(first.prt_previous_seconds, first.prt_seconds);
    assert_eq!(first.time_utc, 1_369_079_161);
    assert_eq!(first.time_millis, 730);

    // The burst is the LAST sample of the two-sample preamble, and it is the
    // same sample on both channels because it is the transmitter's own
    // coupled pulse rather than anything that came back through the antenna.
    // Its Q is exactly zero and its I is the magnitude the header reports as
    // RX[0].fBurstMag = 0.396577, which is what identifies it.
    let burst = first.burst.expect("the record reserves a burst preamble");
    assert_iq(burst.h, (0.3966064453125, 0.0), "pulse 0 burst H");
    assert_iq(
        burst.v.expect("dual channel"),
        (0.3966064453125, 0.0),
        "pulse 0 burst V",
    );

    assert_eq!(first.h.len(), 248);
    assert_eq!(first.v.len(), 248);
    // Gate 0 is at range zero and is a MEASURED sample: the T/R switch
    // recovering, a few dB below saturation and decaying fast. It differs
    // between the channels, which the burst never does.
    assert_iq(
        first.h[0],
        (0.6416015625, 0.1702880859375),
        "pulse 0 H gate 0",
    );
    assert_iq(
        first.v[0],
        (-0.882080078125, 0.801025390625),
        "pulse 0 V gate 0",
    );
    assert_iq(
        first.h[1],
        (0.0382843017578125, 0.09759521484375),
        "pulse 0 H gate 1",
    );
    assert_iq(
        first.h[2],
        (0.02161407470703125, 0.0059967041015625),
        "pulse 0 H gate 2",
    );
    assert_iq(
        first.h[247],
        (-7.790327072143555e-05, -2.9802322387695312e-06),
        "pulse 0 H last gate",
    );
    assert_iq(
        first.v[247],
        (-2.7835369110107422e-05, 3.260374069213867e-05),
        "pulse 0 V last gate",
    );

    // The antenna is scanning: the 24th pulse is a fifth of a degree back.
    let last = sweep.pulses.last().unwrap();
    assert_close(last.azimuth_deg, 331.193_85, 1e-4, "pulse 23 azimuth");
    assert_iq(
        last.h[0],
        (0.63134765625, 0.04071044921875),
        "pulse 23 H gate 0",
    );
}

#[test]
fn the_contiguous_mask_record_is_a_different_acquisition_and_decodes_too() {
    // Same site, same day, deliberately different everything else: mode 13
    // rather than 0, a contiguous 600-bin mask rather than an alternate one,
    // 598 vectors rather than 250, a 1000 us PRT rather than 833, and a
    // 128-pulse nominal dwell rather than 32.
    let sweep = decode_iq_time_series(CONTIGUOUS_MASK).unwrap();

    assert_eq!(sweep.site, "KOUN_RVP");
    assert_eq!(sweep.major_mode, Some(13));
    assert_eq!(sweep.nominal_sample_size, 128);
    assert_eq!(sweep.pulses.len(), 8);
    assert_eq!(sweep.burst_samples, 2);
    assert_eq!(sweep.gate_count(), 596);
    assert_eq!(sweep.gate_spacing_m, Some(250.0));
    // Contiguous mask, two burst samples over bins 0 and 1: gate 0 is bin 2.
    assert_eq!(sweep.range_bins[0], 500.0);
    assert_eq!(sweep.range_bins[1], 750.0);
    assert_eq!(*sweep.range_bins.last().unwrap(), 149_250.0);

    let first = &sweep.pulses[0];
    assert_eq!(first.time_utc, 1_369_089_699);
    assert_eq!(first.time_millis, 456);
    assert_close(first.azimuth_deg, 113.483_28, 1e-4, "pulse 0 azimuth");
    assert_close(first.elevation_deg, 4.833_984_4, 1e-4, "pulse 0 elevation");
    assert_close(first.prt_seconds * 1e6, 1000.05, 1e-3, "pulse 0 PRT in us");

    let burst = first.burst.expect("the record reserves a burst preamble");
    // RX[0].fBurstMag = 0.393627 here, and the burst sample carries it.
    assert_iq(burst.h, (0.3936767578125, 0.0), "pulse 0 burst H");
    assert_iq(
        first.h[0],
        (0.09759521484375, -0.179931640625),
        "pulse 0 H gate 0",
    );
    assert_iq(
        first.v[0],
        (0.040802001953125, 0.011180877685546875),
        "pulse 0 V gate 0",
    );
    assert_iq(
        first.h[1],
        (-0.107208251953125, 0.07269287109375),
        "pulse 0 H gate 1",
    );
    assert_iq(
        first.h[2],
        (0.003330230712890625, 0.1611328125),
        "pulse 0 H gate 2",
    );
    assert_iq(
        first.h[595],
        (5.829334259033203e-05, 1.3589859008789062e-05),
        "pulse 0 H last gate",
    );
    assert_iq(
        first.v[595],
        (-1.8715858459472656e-05, -2.0265579223632812e-05),
        "pulse 0 V last gate",
    );
    assert_close(
        sweep.pulses.last().unwrap().azimuth_deg,
        113.532_715,
        1e-4,
        "pulse 7 azimuth",
    );

    let r_max = sweep.unambiguous_range_m().unwrap();
    assert!((149_900.0..149_910.0).contains(&r_max), "{r_max}");
    assert!(*sweep.range_bins.last().unwrap() < r_max);
    let nyquist = sweep.nyquist_velocity_m_s().unwrap();
    assert!((nyquist - 27.70).abs() < 0.01, "{nyquist}");
}

#[test]
fn decoded_power_reproduces_the_noise_floor_the_processor_measured() {
    // The physical check that the packed-float rule is the right one. Gates
    // beyond the weather are receiver noise, and the record states what that
    // noise is in dBm. Decode the samples correctly and the two agree to a
    // fraction of a decibel; decode them with the transposed rule from the
    // NOAA ICD and the whole record collapses into a band with no dynamic
    // range, nowhere near the stated floor.
    for (name, bytes) in [
        ("alternate mask", ALTERNATE_MASK),
        ("contiguous mask", CONTIGUOUS_MASK),
    ] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        for (channel, vertical) in [("H", false), ("V", true)] {
            let measured = far_range_mean_dbm(&sweep, vertical, 20);
            let stated = sweep
                .calibration
                .noise_dbm()
                .expect("RVP fixture is absolutely calibrated")[usize::from(vertical)];
            assert!(
                (measured - stated).abs() < 1.5,
                "{name} {channel}: far-range mean {measured} dBm against a stated \
                 floor of {stated} dBm"
            );
        }
    }
}

#[test]
fn no_decoded_sample_exceeds_the_formats_headroom_above_saturation() {
    // Unit magnitude is fSaturationDBM and the packed format tops out just
    // below 4 in each of I and Q. A sample beyond that would mean the
    // exponent had been read from the wrong bits.
    for bytes in [ALTERNATE_MASK, CONTIGUOUS_MASK] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        let mut peak = 0.0f32;
        for pulse in &sweep.pulses {
            for (i, q) in pulse.h.iter().chain(pulse.v.iter()) {
                peak = peak.max(i.hypot(*q));
                assert!(i.abs() < 4.0 && q.abs() < 4.0, "{i} {q}");
            }
        }
        // And the record spans a real dynamic range, so the scale is not
        // merely small enough to pass: the strongest sample stands more than
        // 60 dB over the noise floor. A decode stuck in the denormal branch
        // — which is what the transposed rule produces — cannot do that,
        // because every value it can represent lies within about 8 dB of
        // every other.
        let noise_amplitude = sweep.noise_power(0).expect("calibrated H noise").sqrt();
        let dynamic_range_db = 20.0 * (peak / noise_amplitude).log10();
        assert!(
            dynamic_range_db > 60.0,
            "dynamic range {dynamic_range_db} dB"
        );
    }
}

#[test]
fn every_pulse_in_both_records_has_the_same_geometry_and_a_uniform_prt() {
    for bytes in [ALTERNATE_MASK, CONTIGUOUS_MASK] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        assert!(sweep.has_uniform_prt(1e-9));
        for pulse in &sweep.pulses {
            assert_eq!(pulse.h.len(), sweep.gate_count());
            assert_eq!(pulse.v.len(), sweep.gate_count());
            assert!(pulse.burst.is_some());
            assert!((3.9..5.0).contains(&pulse.elevation_deg));
        }
    }
}

#[test]
fn peeking_names_the_record_without_decoding_its_samples() {
    let summary = peek_iq_time_series(ALTERNATE_MASK).unwrap();
    assert_eq!(summary.site, "KOUN_RVP");
    assert_eq!(summary.task_name, "Ascope_DEFAULT");
    assert_eq!(summary.major_mode, 0);
    assert_eq!(summary.polarization_code, 3);
    assert_eq!(summary.channels_recorded, 2);
    assert_eq!(summary.burst_samples, 2);
    assert_eq!(summary.gate_count, 248);
    assert_eq!(summary.pulse_count, 24);
    assert_eq!(summary.time_utc, 1_369_079_161);

    let summary = peek_iq_time_series(CONTIGUOUS_MASK).unwrap();
    assert_eq!(summary.major_mode, 13);
    assert_eq!(summary.gate_count, 596);
    assert_eq!(summary.pulse_count, 8);

    // And a peek describes the same record the decoder will return, not a
    // different one: a router that peeks to name a file and then decodes it
    // must not print a gate count the decode contradicts.
    for bytes in [ALTERNATE_MASK, CONTIGUOUS_MASK] {
        let summary = peek_iq_time_series(bytes).unwrap();
        let sweep = decode_iq_time_series(bytes).unwrap();
        assert_eq!(summary.gate_count, sweep.gate_count());
        assert_eq!(summary.burst_samples, sweep.burst_samples);
        assert_eq!(summary.pulse_count, sweep.pulses.len());
    }
}

#[test]
fn a_pulse_limit_reads_a_dwell_without_reading_the_record() {
    let clipped = decode_iq_time_series_limited(ALTERNATE_MASK, 8).unwrap();
    let whole = decode_iq_time_series(ALTERNATE_MASK).unwrap();
    assert_eq!(clipped.pulses.len(), 8);
    assert_eq!(whole.pulses.len(), 24);
    assert_eq!(clipped.range_bins, whole.range_bins);
    assert_eq!(clipped.calibration, whole.calibration);
    for (left, right) in clipped.pulses.iter().zip(&whole.pulses) {
        assert_eq!(left.h, right.h);
    }
}

#[test]
fn the_refused_modes_are_refused_on_a_real_header() {
    // Take the real record and change only iMajorMode, so the refusal is
    // exercised against genuine framing rather than a synthetic stub.
    for (mode, expected) in [
        (MAJOR_MODE_BATCH_STAGGERED_PRT, "staggered PRT"),
        (MAJOR_MODE_SZ2_PHASE_CODED, "SZ-2"),
    ] {
        const DECLARED: &[u8] = b"iMajorMode=0\n";
        let mut edited = ALTERNATE_MASK.to_vec();
        let at = edited
            .windows(DECLARED.len())
            .position(|window| window == DECLARED)
            .expect("record declares iMajorMode");
        edited.splice(
            at..at + DECLARED.len(),
            format!("iMajorMode={mode}\n").bytes(),
        );
        let error = decode_iq_time_series(&edited).unwrap_err().to_string();
        assert!(error.contains(&format!("iMajorMode {mode}")), "{error}");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn a_real_record_sniffs_as_level_one_and_is_declined_as_a_volume_by_name() {
    // The router must not hand a time series to the Archive II decoder and
    // let it be reported as a corrupt volume. It is not corrupt; it is a
    // different kind of data, and the message has to say so.
    assert!(looks_like_iq_time_series(ALTERNATE_MASK));
    assert_eq!(
        nexrad_io::sniff_supported_volume_bytes(ALTERNATE_MASK),
        Some(nexrad_io::SupportedVolumeFormat::NexradLevel1TimeSeries)
    );

    let error = nexrad_io::decode_supported_volume_bytes(ALTERNATE_MASK)
        .unwrap_err()
        .to_string();
    assert!(error.contains("NEXRAD Level 1 time series"), "{error}");
    assert!(error.contains("KOUN_RVP"), "{error}");
    assert!(error.contains("24 pulses"), "{error}");
    assert!(error.contains("not estimated moments"), "{error}");
}

#[test]
fn a_real_dwell_yields_weather_and_no_gate_zero_artifact() {
    // The whole point of the reader, checked the only way that counts: run
    // the standard estimators over a real dwell and look at what comes out.
    //
    // Serve the burst as gate 0 and this is what happens there. The burst is
    // written identically into both channel blocks, so H and V are the same
    // bits: S_h / S_v is exactly one and ZDR exactly 0.000 dB, C(0) is exactly
    // real and positive so rho_hv is exactly 1.000 and PhiDP exactly 0, and
    // R(1) is exactly real so V and W are exactly zero. That is a 78 dB-SNR
    // return at range zero which no censor would drop and which is pure
    // artifact. None of those exact values occurs at a measured gate.
    for (name, bytes, first_gate_m) in [
        ("alternate mask", ALTERNATE_MASK, 1000.0),
        ("contiguous mask", CONTIGUOUS_MASK, 500.0),
    ] {
        let sweep = decode_iq_time_series(bytes).unwrap();
        let moments = pulse_pair(&sweep);

        let gate0 = &moments[0];
        assert_eq!(sweep.range_bins[0], first_gate_m, "{name}");
        assert!(
            gate0.snr_h_db > 40.0,
            "{name}: gate 0 SNR {}",
            gate0.snr_h_db
        );
        assert!(
            gate0.rho_hv < 0.999,
            "{name}: gate 0 rho_hv {} — the burst served as a gate",
            gate0.rho_hv
        );
        assert!(
            gate0.zdr_db.abs() > 0.1,
            "{name}: gate 0 ZDR {} dB — the burst served as a gate",
            gate0.zdr_db
        );
        assert!(
            gate0.phi_dp_deg.abs() > 1.0,
            "{name}: gate 0 PhiDP {} deg — the burst served as a gate",
            gate0.phi_dp_deg
        );

        // And the field itself is weather rather than arithmetic. Take the
        // gates the dwell actually resolves and ask whether the polarimetric
        // variables land where rain and wet hail land.
        let weather: Vec<&GateMoments> =
            moments.iter().filter(|gate| gate.snr_h_db > 20.0).collect();
        assert!(
            weather.len() > 40,
            "{name}: only {} lit gates",
            weather.len()
        );

        let rho = median(weather.iter().map(|gate| gate.rho_hv).collect());
        let width = median(weather.iter().map(|gate| gate.width_m_s).collect());
        let zdr = median(weather.iter().map(|gate| gate.zdr_db).collect());
        let speed = median(weather.iter().map(|gate| gate.velocity_m_s.abs()).collect());
        let nyquist = f64::from(sweep.nyquist_velocity_m_s().unwrap());
        assert!(
            (0.80..0.99).contains(&rho),
            "{name}: median rho_hv {rho} is not precipitation"
        );
        assert!(
            (1.5..7.0).contains(&width),
            "{name}: median spectrum width {width} m/s"
        );
        assert!((-2.0..8.0).contains(&zdr), "{name}: median ZDR {zdr} dB");
        assert!(
            speed > 0.5 && speed < nyquist,
            "{name}: median |V| {speed} m/s against a Nyquist of {nyquist}"
        );
        for gate in &weather {
            assert!(gate.rho_hv <= 1.05, "{name}: rho_hv {}", gate.rho_hv);
            assert!(gate.velocity_m_s.abs() <= nyquist, "{name}: aliased V");
        }
    }
}
