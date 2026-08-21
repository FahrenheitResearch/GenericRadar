//! The Level 1 moment estimators against real pulses.
//!
//! # The fixture and where it came from
//!
//! `data/koun_20130520_194601.iq.rain_shaft.iqd` is 39,664 bytes: the first 32
//! pulses, and the first 76 recorded range bins of each, of
//!
//! ```text
//! KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250
//! ```
//!
//! a 4,434,431-byte Vaisala RVP8 TS-Record time-series file from the NSSL
//! THREDDS archive at `data.nssl.noaa.gov`, under
//! `RRDD/KOUN/2013/KOUN_20130520/IQ/`, whose catalogue states its rights as
//! "Freely available". KOUN is the research WSR-88D at Norman, Oklahoma; this
//! is 19:46:01 UTC on 20 May 2013, elevation 4.0 degrees, azimuth 331 degrees,
//! looking through the storm of that afternoon.
//!
//! The excerpt was made by decoding the RVP8 packed-float samples to `f32` I
//! and Q and rewriting them in this crate's own interchange format (see
//! [`nexrad_io::iq_moments::interchange`]), so the fixture is samples and
//! calibration and nothing else - it carries no parser, and it does not
//! prejudge the reader track's decode. Its header values travel with it and are
//! the file's own: 11.08 cm wavelength, 1.5 us pulse width, `fDBzCalib` -35.5,
//! `fSaturationDBM` +6, `fNoiseDBm` -80.5555 and -80.5955, PRT 833.375 us
//! (Nyquist 33.24 m/s), `iRangeMask` 0x5555 so recorded bins step 500 m from
//! range zero, and two leading burst samples per channel block.
//!
//! Three things about the decode were checked against the file rather than
//! assumed, because each of them would produce a plausible wrong field:
//!
//! * The Vaisala packed-float rule (exponent nonzero: 13-bit signed mantissa
//!   scaled by `2^(exponent-25)`; exponent zero: 12-bit signed scaled by
//!   `2^-24`) reproduces the pulse header's own `fBurstMag` of 0.396577 as
//!   0.396606, and puts the far-range noise floor at -80.9 dBm against a
//!   declared -80.56.
//! * The two channels are stored as consecutive blocks, not interleaved bin by
//!   bin: read as blocks the field has a median RhoHV of 0.89, read interleaved
//!   0.33.
//! * The two channels are aligned bin for bin: a plus-or-minus two bin offset
//!   scan of the vertical channel peaks decisively at zero offset (0.89 against
//!   0.29 to 0.33 either side).
//!
//! # What these tests are for
//!
//! The synthetic tests elsewhere prove the arithmetic against its own
//! definitions. Two of the conventions in this module cannot be pinned that way
//! at all, because a synthetic signal is built with the same convention it is
//! then read back with. Those two are pinned here, on weather:
//!
//! * **Differential phase must RISE along a path through rain.** Oblate drops
//!   delay the horizontal wave more than the vertical one, so PhiDP is a
//!   monotonically accumulating quantity (Bringi and Chandrasekar 2001,
//!   section 4.3). If `C(0)` were ordered `h conj(v)` instead of `v conj(h)`
//!   every number below would be negated and the differential phase would fall
//!   through the shaft, which does not happen in nature.
//! * **Ground clutter is at zero velocity.** It is the only target whose radial
//!   velocity is known a priori, so it is what tells `R(1) = x[k] conj(x[k+1])`
//!   from its conjugate.
//!
//! Every expected value below was produced by an independent implementation of
//! the same estimators, written from Doviak and Zrnic ch. 6 and Zrnic 1977 with
//! its own RVP8 parser and packed-float decoder, and run over the original
//! file rather than over this fixture.

use nexrad_io::iq_moments::estimator::{GateEstimate, SnrCensor};
use nexrad_io::iq_moments::interchange::{DumpVersion, read_dump};
use nexrad_io::iq_moments::taper::Taper;
use nexrad_io::iq_moments::{
    DwellPlan, MomentConfig, ProcessedSweep, process_sweep, sweep_gate_spectrum,
};

const RAIN_SHAFT: &[u8] = include_bytes!("data/koun_20130520_194601.iq.rain_shaft.iqd");

/// The two leading samples of each channel block are the burst, not gates.
const BURST_SAMPLES: usize = 2;

fn config() -> MomentConfig {
    MomentConfig {
        dwell: DwellPlan::contiguous(32),
        taper: Taper::Rectangular,
        // Open, so that what is asserted is what the estimators produced and
        // not what a threshold left behind.
        censor: SnrCensor::Off,
        burst_samples: BURST_SAMPLES,
        ..MomentConfig::default()
    }
}

fn processed() -> ProcessedSweep {
    let dump = read_dump(RAIN_SHAFT).expect("the fixture reads");
    assert_eq!(dump.version, DumpVersion::V2);
    process_sweep(&dump.sweep, &config()).expect("real pulses process")
}

/// Gates strong enough and coherent enough for a differential phase to mean
/// something: the same population an analyst would read PhiDP from.
fn quality_gates(dwell: &[GateEstimate]) -> Vec<&GateEstimate> {
    dwell
        .iter()
        .filter(|gate| gate.snr_h_db >= 20.0 && gate.correlation_coefficient >= 0.8)
        .collect()
}

#[test]
fn the_fixture_carries_the_files_own_calibration_and_range_ladder() {
    let dump = read_dump(RAIN_SHAFT).expect("the fixture reads");
    let sweep = dump.sweep;
    assert_eq!(sweep.site, "KOUN");
    // 19:46:01 UTC, 20 May 2013.
    assert_eq!(sweep.time_utc, 1_369_079_161);
    assert_eq!(sweep.pulses.len(), 32);
    assert_eq!(sweep.range_bins.len(), 76);
    assert!((sweep.wavelength_m - 0.1108).abs() < 1e-6);
    assert!((sweep.pulse_width_s.unwrap() - 1.5e-6).abs() < 1e-12);
    assert!((sweep.calibration.dbz_calibration().unwrap() - -35.5).abs() < 1e-4);
    assert!((sweep.calibration.saturation_dbm().unwrap() - 6.0).abs() < 1e-4);
    let noise_dbm = sweep.calibration.noise_dbm().unwrap();
    assert!((noise_dbm[0] - -80.5555).abs() < 1e-3);
    assert!((noise_dbm[1] - -80.5955).abs() < 1e-3);
    for pulse in &sweep.pulses {
        assert!((pulse.prt_seconds - 833.375e-6).abs() < 1e-9);
        assert!((pulse.elevation_deg - 4.0).abs() < 0.01);
        assert_eq!(pulse.h.len(), 76);
        assert_eq!(pulse.v.len(), 76, "H+V file: both channels recorded");
    }
    // iRangeMask 0x5555 records alternate 250 m bins, so the ladder steps 500 m
    // and starts at range zero, where the transmit pulse is.
    assert!(sweep.range_bins[0].abs() < 1e-3);
    for (index, range_m) in sweep.range_bins.iter().enumerate() {
        assert!(
            (range_m - 500.0 * index as f32).abs() < 1e-3,
            "bin {index} at {range_m} m"
        );
    }
}

#[test]
fn the_sweep_lands_on_the_geometry_the_file_declares() {
    let processed = processed();
    assert_eq!(processed.report.dwells, 1);
    assert_eq!(processed.report.gates, 76 - BURST_SAMPLES);
    assert!(processed.report.dual_pol);
    assert_eq!(processed.report.burst_samples_dropped, BURST_SAMPLES);
    // The burst is dropped, so gate 0 is a gate: 1.0 km, stepping 500 m to
    // 37.5 km.
    assert_eq!(processed.gate_range.first_gate_m, 1_000);
    assert_eq!(processed.gate_range.gate_spacing_m, 500);
    let dwell = processed.dwell(0).expect("one dwell");
    assert!((dwell[0].range_m - 1_000.0).abs() < 1e-3);
    assert!((dwell[73].range_m - 37_500.0).abs() < 1e-3);
    // lambda / 4T for 11.08 cm at 833.375 us.
    assert!(
        (processed.report.nyquist_velocity_mps - 33.238).abs() < 0.01,
        "nyquist {}",
        processed.report.nyquist_velocity_mps
    );
    // Every gate of this excerpt is in the storm; none is under the noise.
    assert_eq!(processed.report.below_noise_samples, 0);
    assert_eq!(processed.report.censored_samples, 0);
}

#[test]
fn ground_clutter_at_one_kilometre_reads_zero_velocity() {
    // The lag-1 ordering pin. Clutter is the only target whose velocity is
    // known before the measurement, and 0.14 m/s inside a 33.24 m/s Nyquist
    // interval is zero; the conjugate ordering would still be small here, but
    // the storm gates below would then all have the wrong sign, and a 12 m/s
    // outbound core reading 12 m/s inbound is what this rules out.
    let processed = processed();
    let dwell = processed.dwell(0).expect("one dwell");
    let clutter = dwell[0];
    assert!(
        clutter.velocity_mps.abs() < 0.5,
        "clutter velocity {} m/s",
        clutter.velocity_mps
    );
    // Coherent, strong, and at the reflectivity a ground target gives.
    assert!(clutter.sqi > 0.97, "clutter coherency {}", clutter.sqi);
    assert!(
        (40.0..55.0).contains(&clutter.reflectivity_dbz),
        "clutter Z {}",
        clutter.reflectivity_dbz
    );
    assert!(
        (clutter.reflectivity_dbz - 47.17).abs() < 0.05,
        "clutter Z {}",
        clutter.reflectivity_dbz
    );

    // The storm is moving, and it is moving outbound along this azimuth.
    let core = dwell[16];
    assert!(
        (core.velocity_mps - 12.40).abs() < 0.05,
        "9 km velocity {}",
        core.velocity_mps
    );
}

#[test]
fn the_dbz0_reference_point_puts_the_convective_core_where_a_core_belongs() {
    // `fDBzCalib` is referenced to the noise power, so reflectivity is built on
    // SNR. Read instead as if it were referenced to 0 dBm, every number below
    // would be 80.6 dB lower and this core would be a -13 dBZ whisper.
    let processed = processed();
    let dwell = processed.dwell(0).expect("one dwell");
    let (index, peak) = dwell
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.reflectivity_dbz.total_cmp(&b.1.reflectivity_dbz))
        .expect("a peak");
    assert!(
        (60.0..72.0).contains(&peak.reflectivity_dbz),
        "peak Z {} at gate {index}",
        peak.reflectivity_dbz
    );
    assert!(
        (peak.reflectivity_dbz - 67.94).abs() < 0.05,
        "peak Z {}",
        peak.reflectivity_dbz
    );
    assert!(
        (10_000.0..13_000.0).contains(&peak.range_m),
        "peak at {} m",
        peak.range_m
    );
}

#[test]
fn differential_phase_rises_through_the_rain_shaft() {
    // THE pin for `C(0) = mean(v conj(h))`. Everything else about the
    // differential moments could be self-consistent and still have the two
    // channels the wrong way round; only real rain says which way is up,
    // because PhiDP accumulates along the path and cannot fall through it.
    let processed = processed();
    let dwell = processed.dwell(0).expect("one dwell");
    let quality = quality_gates(dwell);
    assert_eq!(
        quality.len(),
        55,
        "the population the assertions below are made over"
    );

    // Four range bands across the shaft, each a mean over its own gates, so no
    // single gate can carry the result.
    let bands = [
        (1_000.0f32, 10_000.0f32),
        (10_000.0, 20_000.0),
        (20_000.0, 30_000.0),
        (30_000.0, 37_500.0),
    ];
    let means: Vec<f32> = bands
        .iter()
        .map(|(low, high)| {
            let inside: Vec<f32> = quality
                .iter()
                .filter(|gate| gate.range_m >= *low && gate.range_m <= *high)
                .map(|gate| gate.differential_phase_deg)
                .collect();
            assert!(
                inside.len() >= 8,
                "band {low}-{high} m has {} gates",
                inside.len()
            );
            inside.iter().sum::<f32>() / inside.len() as f32
        })
        .collect();
    for window in means.windows(2) {
        assert!(
            window[1] > window[0],
            "PhiDP must climb band by band through rain, got {means:?}"
        );
    }
    assert!(
        means[3] - means[0] > 20.0,
        "PhiDP rise across the shaft is only {} deg: {means:?}",
        means[3] - means[0]
    );

    // And as a rate: a least-squares fit over the quality gates.
    let slope = phidp_slope_deg_per_km(&quality);
    assert!(
        (0.5..3.0).contains(&slope),
        "PhiDP slope {slope} deg/km is not a rain shaft"
    );
    assert!(
        (slope - 1.668).abs() < 0.01,
        "PhiDP slope {slope} deg/km against the independently measured 1.668"
    );

    // What the opposite convention would look like on the same rain: the
    // cross-correlation is Hermitian under exchange of the channels, so
    // `arg(h conj(v))` is exactly the negation - a differential phase falling
    // 1.67 degrees per km through a rain shaft, which is unphysical.
    let mirrored: Vec<GateEstimate> = quality
        .iter()
        .map(|gate| {
            let mut flipped = **gate;
            flipped.differential_phase_deg = -gate.differential_phase_deg;
            flipped
        })
        .collect();
    let mirrored_slope = phidp_slope_deg_per_km(&mirrored.iter().collect::<Vec<_>>());
    assert!(
        (mirrored_slope + slope).abs() < 1e-3,
        "the mirrored convention should be the exact negation, got {mirrored_slope}"
    );
    assert!(mirrored_slope < 0.0);
}

#[test]
fn the_differential_moments_describe_rain_and_not_noise() {
    let processed = processed();
    let dwell = processed.dwell(0).expect("one dwell");
    let quality = quality_gates(dwell);

    let mut rho: Vec<f32> = quality
        .iter()
        .map(|gate| gate.correlation_coefficient)
        .collect();
    rho.sort_by(f32::total_cmp);
    let median_rho = rho[rho.len() / 2];
    assert!(
        (0.85..0.98).contains(&median_rho),
        "median RhoHV {median_rho} over 55 storm gates"
    );

    let mut zdr: Vec<f32> = quality
        .iter()
        .map(|gate| gate.differential_reflectivity_db)
        .collect();
    zdr.sort_by(f32::total_cmp);
    let median_zdr = zdr[zdr.len() / 2];
    // Positive: big oblate drops, which is what a May storm at 4 degrees
    // elevation over Oklahoma should give.
    assert!(
        (0.5..6.0).contains(&median_zdr),
        "median ZDR {median_zdr} dB over 55 storm gates"
    );

    // Width stays inside what one Nyquist interval can hold, on real data as
    // well as on the constructed corner case.
    let ceiling =
        GateEstimate::max_spectrum_width_mps(f64::from(processed.report.nyquist_velocity_mps));
    for gate in dwell {
        assert!(
            f64::from(gate.spectrum_width_mps) <= ceiling + 1e-3,
            "gate at {} m reports a width of {} against a ceiling of {ceiling}",
            gate.range_m,
            gate.spectrum_width_mps
        );
    }
}

#[test]
fn the_spectrum_of_a_real_gate_peaks_at_its_pulse_pair_velocity() {
    // The velocity axis, checked on weather rather than on a tone: the two
    // estimates are computed by different routes - the argument of one lag
    // against the whole periodogram - and a reversed or misscaled axis puts
    // them on opposite sides of zero.
    let dump = read_dump(RAIN_SHAFT).expect("the fixture reads");
    let config = config();
    let processed = process_sweep(&dump.sweep, &config).expect("processes");
    let dwell = processed.dwell(0).expect("one dwell");
    let bin_width = 2.0 * processed.report.nyquist_velocity_mps / config.dwell.pulses as f32;

    for gate in [8usize, 16, 21, 32] {
        let spectrum =
            sweep_gate_spectrum(&dump.sweep, &config, 0, gate, 0).expect("a real gate's spectrum");
        let peak = spectrum
            .power_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(index, _)| index)
            .expect("a peak");
        let pulse_pair = dwell[gate].velocity_mps;
        assert!(
            (spectrum.velocities_mps[peak] - pulse_pair).abs() <= bin_width,
            "gate {gate} at {} m: spectral peak {} against pulse-pair {pulse_pair}",
            dwell[gate].range_m,
            spectrum.velocities_mps[peak]
        );
        assert_eq!(spectrum.taper, config.taper);
    }
}

/// Least-squares slope of differential phase against range, degrees per km.
///
/// No phase unwrapping: across this excerpt the quality gates run from 85 to
/// 157 degrees and come nowhere near the wrap, which the test above checks by
/// asserting the band means rather than trusting the fit alone.
fn phidp_slope_deg_per_km(gates: &[&GateEstimate]) -> f32 {
    let n = gates.len() as f64;
    let mean_range = gates
        .iter()
        .map(|gate| f64::from(gate.range_m) / 1000.0)
        .sum::<f64>()
        / n;
    let mean_phi = gates
        .iter()
        .map(|gate| f64::from(gate.differential_phase_deg))
        .sum::<f64>()
        / n;
    let mut covariance = 0.0;
    let mut variance = 0.0;
    for gate in gates {
        let range_km = f64::from(gate.range_m) / 1000.0 - mean_range;
        covariance += range_km * (f64::from(gate.differential_phase_deg) - mean_phi);
        variance += range_km * range_km;
    }
    (covariance / variance) as f32
}
