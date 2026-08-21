//! Sweep-level tests: the shape of what reaches the renderer, and the four
//! refusals that keep a wrong field off the screen.

use super::*;
use crate::iq::{IqCalibration, IqPulse};

const WAVELENGTH_M: f32 = 0.1108;
const PRT_S: f32 = 833.375e-6;
const NOISE_DBM: f32 = -80.0;
const SATURATION_DBM: f32 = 6.0;

fn nyquist() -> f64 {
    f64::from(WAVELENGTH_M) / (4.0 * f64::from(PRT_S))
}

fn noise_linear() -> f64 {
    10f64.powf(f64::from(NOISE_DBM - SATURATION_DBM) / 10.0)
}

/// A sweep of coherent tones: gate `g` returns a tone at `velocity_of(g)` with
/// a power `snr_db_of(g)` above the noise, and the azimuth advances by
/// `azimuth_step_deg` per pulse.
struct SweepBuilder {
    pulses: usize,
    gates: usize,
    burst_samples: usize,
    gate_spacing_m: f32,
    first_bin_m: f32,
    azimuth_start_deg: f32,
    azimuth_step_deg: f32,
    dual_pol: bool,
    zdr_db: f32,
    phidp_deg: f32,
}

impl Default for SweepBuilder {
    fn default() -> Self {
        Self {
            pulses: 128,
            gates: 24,
            burst_samples: 0,
            gate_spacing_m: 250.0,
            first_bin_m: 250.0,
            azimuth_start_deg: 90.0,
            azimuth_step_deg: 0.01,
            dual_pol: true,
            zdr_db: 1.5,
            phidp_deg: 30.0,
        }
    }
}

impl SweepBuilder {
    fn velocity_of(gate: usize) -> f64 {
        // A ramp that stays well inside the Nyquist interval.
        -20.0 + 1.5 * gate as f64
    }

    fn snr_db_of(gate: usize) -> f64 {
        // Strong near, fading to below the operational threshold at the far
        // end. Deliberately never lands exactly on 2 dB, so the censor test
        // measures where the threshold bites rather than how a tie breaks.
        41.0 - 2.0 * gate as f64
    }

    fn build(&self) -> IqSweep {
        let bins = self.gates + self.burst_samples;
        let mut pulses = Vec::with_capacity(self.pulses);
        let zdr_ratio = 10f64.powf(-f64::from(self.zdr_db) / 10.0);
        let phidp = f64::from(self.phidp_deg).to_radians();
        for pulse_index in 0..self.pulses {
            let mut h = Vec::with_capacity(bins);
            let mut v = Vec::with_capacity(bins);
            for bin in 0..bins {
                if bin < self.burst_samples {
                    // Burst samples: enormous, constant, and at zero velocity -
                    // exactly what would be read as a hard target at minimum
                    // range if they were left in.
                    h.push((0.4, 0.0));
                    v.push((0.4, 0.0));
                    continue;
                }
                let gate = bin - self.burst_samples;
                let snr = 10f64.powf(Self::snr_db_of(gate) / 10.0);
                // The tones carry no noise of their own, so each channel's
                // amplitude is set to the TOTAL power the estimator will see -
                // noise plus the intended signal. Building the vertical channel
                // by scaling the horizontal *total* instead would scale the
                // noise along with the signal, and the recovered ZDR would then
                // be a function of SNR rather than the value asked for.
                let amplitude_h = (noise_linear() * (1.0 + snr)).sqrt();
                let amplitude_v = (noise_linear() * (1.0 + snr * zdr_ratio)).sqrt();
                let step = -4.0 * std::f64::consts::PI * Self::velocity_of(gate) * f64::from(PRT_S)
                    / f64::from(WAVELENGTH_M);
                let phase = step * pulse_index as f64 + 0.37 * gate as f64;
                let hs = fft::Complex::from_polar(amplitude_h, phase);
                h.push((hs.re as f32, hs.im as f32));
                // PhiDP = arg(v conj(h)), so v leads h by phidp.
                let vs = fft::Complex::from_polar(amplitude_v, phase + phidp);
                v.push((vs.re as f32, vs.im as f32));
            }
            pulses.push(IqPulse {
                azimuth_deg: (self.azimuth_start_deg + self.azimuth_step_deg * pulse_index as f32)
                    .rem_euclid(360.0),
                elevation_deg: 4.0,
                prt_seconds: PRT_S,
                prt_previous_seconds: PRT_S,
                h,
                v: if self.dual_pol { v } else { Vec::new() },
                ..IqPulse::default()
            });
        }
        IqSweep {
            site: "TST".to_owned(),
            time_utc: 1_369_079_161,
            wavelength_m: WAVELENGTH_M,
            pulse_width_s: Some(1.5e-6),
            gate_spacing_m: Some(self.gate_spacing_m),
            first_gate_m: self.first_bin_m,
            range_bins: (0..bins)
                .map(|bin| self.first_bin_m + self.gate_spacing_m * bin as f32)
                .collect(),
            calibration: IqCalibration::Absolute {
                noise_dbm: [NOISE_DBM, NOISE_DBM],
                dbz_calibration: -35.5,
                saturation_dbm: SATURATION_DBM,
            },
            pulses,
            burst_samples: self.burst_samples,
            ..IqSweep::default()
        }
    }
}

fn uncensored() -> MomentConfig {
    MomentConfig {
        censor: SnrCensor::Off,
        ..MomentConfig::default()
    }
}

#[test]
fn a_processed_sweep_arrives_in_the_shape_the_rasteriser_reads() {
    let sweep = SweepBuilder::default().build();
    let processed = process_sweep(&sweep, &uncensored()).expect("sweep processes");

    assert_eq!(processed.report.dwells, 2);
    assert_eq!(processed.cut.radials.len(), 2);
    assert_eq!(processed.report.gates, 24);

    for (moment, grid) in &processed.cut.moments {
        assert_eq!(&grid.moment, moment, "grid keyed by its own moment");
        assert_eq!(grid.gate_range.gate_count, 24);
        assert_eq!(grid.radial_indices, vec![0, 1]);
        let MomentStorage::F32(values) = &grid.storage else {
            panic!("{moment} should use f32 storage");
        };
        assert_eq!(values.len(), 2 * 24);
        // Every radial index the grid names must exist in the cut, or the
        // renderer's azimuth lookup reads out of bounds.
        for index in &grid.radial_indices {
            assert!(*index < processed.cut.radials.len());
        }
    }

    let available = processed.cut.moments_available();
    for expected in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
        MomentType::DifferentialPhase,
    ] {
        assert!(available.contains(&expected), "missing {expected}");
    }
    // Diagnostics stay out of the cut unless asked for.
    assert!(!available.contains(&MomentType::Unknown("SNR".to_owned())));
}

#[test]
fn the_moments_recover_the_synthetic_field_they_were_built_from() {
    let sweep = SweepBuilder::default().build();
    let processed = process_sweep(&sweep, &uncensored()).expect("sweep processes");
    let dwell = processed.dwell(0).expect("first dwell");

    for (gate, estimate) in dwell.iter().enumerate() {
        let expected_velocity = SweepBuilder::velocity_of(gate);
        assert!(
            (f64::from(estimate.velocity_mps) - expected_velocity).abs() < 0.05,
            "gate {gate}: velocity {} expected {expected_velocity}",
            estimate.velocity_mps
        );
        let expected_snr = SweepBuilder::snr_db_of(gate);
        assert!(
            (f64::from(estimate.snr_h_db) - expected_snr).abs() < 0.2,
            "gate {gate}: SNR {} expected {expected_snr}",
            estimate.snr_h_db
        );
        assert!(
            (estimate.differential_reflectivity_db - 1.5).abs() < 0.15,
            "gate {gate}: ZDR {}",
            estimate.differential_reflectivity_db
        );
        assert!(
            (estimate.differential_phase_deg - 30.0).abs() < 0.5,
            "gate {gate}: PhiDP {}",
            estimate.differential_phase_deg
        );
        // Range is stated per gate, not implied.
        assert!(
            (estimate.range_m - (250.0 + 250.0 * gate as f32)).abs() < 1e-3,
            "gate {gate}: range {}",
            estimate.range_m
        );
    }
}

#[test]
fn the_reflectivity_grid_carries_the_range_correction_gate_by_gate() {
    let sweep = SweepBuilder::default().build();
    let processed = process_sweep(&sweep, &uncensored()).expect("sweep processes");
    let dwell = processed.dwell(0).expect("first dwell");
    for (gate, estimate) in dwell.iter().enumerate() {
        let range_km = f64::from(estimate.range_m) / 1000.0;
        let expected = SweepBuilder::snr_db_of(gate)
            + f64::from(
                sweep
                    .calibration
                    .dbz_calibration()
                    .expect("synthetic sweep is calibrated"),
            )
            + 20.0 * range_km.log10();
        assert!(
            (f64::from(estimate.reflectivity_dbz) - expected).abs() < 0.25,
            "gate {gate}: Z {} expected {expected}",
            estimate.reflectivity_dbz
        );
    }
}

#[test]
fn the_snr_threshold_censors_weak_gates_and_off_shows_what_it_was_hiding() {
    let sweep = SweepBuilder::default().build();

    let operational = process_sweep(&sweep, &MomentConfig::default()).expect("processes");
    let open = process_sweep(&sweep, &uncensored()).expect("processes");

    // SNR ramps 40 dB down to -6 dB across 24 gates, crossing 2 dB at gate 19.
    assert!(operational.report.censored_samples > 0);
    assert_eq!(open.report.censored_samples, 0);
    assert!(operational.report.censored_fraction() > 0.0);

    let censored = operational.dwell(0).expect("dwell");
    let uncensored = open.dwell(0).expect("dwell");
    let first_hidden = censored
        .iter()
        .position(|estimate| estimate.censored)
        .expect("some gate is censored");
    assert!(
        SweepBuilder::snr_db_of(first_hidden) < 2.0
            && SweepBuilder::snr_db_of(first_hidden - 1) >= 2.0,
        "the censor should bite exactly where SNR crosses 2 dB, bit at gate {first_hidden}          where the built SNR is {} dB",
        SweepBuilder::snr_db_of(first_hidden)
    );
    assert!(censored[first_hidden].reflectivity_dbz.is_nan());
    assert!(uncensored[first_hidden].reflectivity_dbz.is_finite());
    // The diagnostic that explains the censoring survives it.
    assert!(censored[first_hidden].power_h_db.is_finite());

    // And the censored cells reach the renderer as NaN, which its f32 path
    // skips - no sentinel value, no second convention.
    let grid = operational
        .cut
        .moments
        .get(&MomentType::Reflectivity)
        .expect("reflectivity grid");
    assert_eq!(grid.nodata, None);
    assert!(grid.scaled_value(0, first_hidden).expect("cell").is_nan());
}

#[test]
fn burst_samples_are_dropped_so_gate_zero_is_a_gate() {
    let sweep = SweepBuilder {
        burst_samples: 2,
        ..SweepBuilder::default()
    }
    .build();

    let leaving_them_in = process_sweep(&sweep, &uncensored()).expect("processes");
    let first = leaving_them_in.dwell(0).expect("dwell")[0];
    // The burst is a saturating, zero-velocity return: exactly the hard target
    // at minimum range that a reader which forgets it would draw.
    assert!(first.snr_h_db > 60.0, "burst SNR {}", first.snr_h_db);

    let config = MomentConfig {
        burst_samples: 2,
        ..uncensored()
    };
    let dropped = process_sweep(&sweep, &config).expect("processes");
    assert_eq!(dropped.report.gates, 24);
    assert_eq!(dropped.report.burst_samples_dropped, 2);
    let first = dropped.dwell(0).expect("dwell")[0];
    assert!(
        (f64::from(first.snr_h_db) - SweepBuilder::snr_db_of(0)).abs() < 0.2,
        "gate 0 SNR {}",
        first.snr_h_db
    );
    // And the range ladder starts at the first real gate, not at the burst.
    assert!(
        (first.range_m - 750.0).abs() < 1e-3,
        "range {}",
        first.range_m
    );
    assert_eq!(dropped.gate_range.first_gate_m, 750);
    assert_eq!(dropped.gate_range.gate_spacing_m, 250);
}

#[test]
fn the_dwell_length_is_a_parameter_and_changes_how_many_radials_appear() {
    let sweep = SweepBuilder {
        pulses: 256,
        ..SweepBuilder::default()
    }
    .build();
    for pulses in [16usize, 32, 64, 128] {
        let config = MomentConfig {
            dwell: DwellPlan::contiguous(pulses),
            ..uncensored()
        };
        let processed = process_sweep(&sweep, &config).expect("processes");
        assert_eq!(processed.report.dwells, 256 / pulses);
        assert_eq!(processed.cut.radials.len(), 256 / pulses);
        assert_eq!(processed.report.pulses_per_dwell, pulses);
    }
}

#[test]
fn a_sliding_dwell_produces_overlapping_radials() {
    let sweep = SweepBuilder {
        pulses: 256,
        ..SweepBuilder::default()
    }
    .build();
    let config = MomentConfig {
        dwell: DwellPlan::sliding(64, 16),
        ..uncensored()
    };
    let processed = process_sweep(&sweep, &config).expect("processes");
    assert_eq!(processed.report.dwells, (256 - 64) / 16 + 1);
    assert_eq!(processed.report.stride, 16);
}

#[test]
fn every_window_is_reachable_and_changes_nothing_about_the_grid_shape() {
    let sweep = SweepBuilder::default().build();
    for taper in Taper::ALL {
        let config = MomentConfig {
            taper,
            ..uncensored()
        };
        let processed = process_sweep(&sweep, &config).expect("processes");
        assert_eq!(processed.report.taper, taper);
        assert_eq!(processed.cut.radials.len(), 2);
        let dwell = processed.dwell(0).expect("dwell");
        assert!(
            (f64::from(dwell[4].velocity_mps) - SweepBuilder::velocity_of(4)).abs() < 0.05,
            "{}: velocity {}",
            taper.label(),
            dwell[4].velocity_mps
        );
    }
}

#[test]
fn a_dwell_straddling_north_averages_to_north_and_not_to_south() {
    let sweep = SweepBuilder {
        pulses: 64,
        azimuth_start_deg: 359.0,
        azimuth_step_deg: 2.0 / 63.0,
        ..SweepBuilder::default()
    }
    .build();
    let config = MomentConfig {
        dwell: DwellPlan::contiguous(64),
        ..uncensored()
    };
    let processed = process_sweep(&sweep, &config).expect("processes");
    let azimuth = processed.cut.radials[0].azimuth_deg;
    assert!(
        !(0.5..=359.5).contains(&azimuth),
        "circular mean of 359..1 should be near 0, got {azimuth}"
    );
}

#[test]
fn a_single_pol_sweep_produces_no_dual_pol_moments_rather_than_empty_ones() {
    let sweep = SweepBuilder {
        dual_pol: false,
        ..SweepBuilder::default()
    }
    .build();
    let processed = process_sweep(&sweep, &uncensored()).expect("processes");
    assert!(!processed.report.dual_pol);
    let available = processed.cut.moments_available();
    assert!(available.contains(&MomentType::Reflectivity));
    assert!(!available.contains(&MomentType::DifferentialReflectivity));
    assert!(!available.contains(&MomentType::CorrelationCoefficient));
    assert!(!available.contains(&MomentType::DifferentialPhase));
}

#[test]
fn diagnostic_moments_appear_only_when_asked_for() {
    let sweep = SweepBuilder::default().build();
    let config = MomentConfig {
        emit_diagnostic_moments: true,
        ..uncensored()
    };
    let processed = process_sweep(&sweep, &config).expect("processes");
    let available = processed.cut.moments_available();
    assert!(available.contains(&MomentType::Unknown("SNR".to_owned())));
    assert!(available.contains(&MomentType::Unknown("SQI".to_owned())));
}

#[test]
fn a_staggered_prt_waveform_is_refused_rather_than_mis_estimated() {
    let mut sweep = SweepBuilder::default().build();
    // The 2/3 stagger a batch mode uses.
    for (index, pulse) in sweep.pulses.iter_mut().enumerate() {
        if index % 2 == 1 {
            pulse.prt_seconds = PRT_S * 2.0 / 3.0;
        }
    }
    let error = process_sweep(&sweep, &uncensored()).expect_err("staggered PRT must be refused");
    assert!(
        matches!(error, IqMomentError::StaggeredPrt { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("Contiguous PRT only"));
}

#[test]
fn clock_jitter_within_a_part_in_a_thousand_is_not_mistaken_for_a_stagger() {
    let mut sweep = SweepBuilder::default().build();
    for (index, pulse) in sweep.pulses.iter_mut().enumerate() {
        pulse.prt_seconds = PRT_S * (1.0 + if index % 2 == 0 { 1e-5 } else { -1e-5 });
    }
    assert!(process_sweep(&sweep, &uncensored()).is_ok());
}

#[test]
fn the_two_refused_major_modes_are_named_rather_than_silently_processed() {
    let sweep = SweepBuilder::default().build();
    for mode in [12u32, 15] {
        let config = MomentConfig {
            declared_major_mode: Some(mode),
            ..uncensored()
        };
        let error = process_sweep(&sweep, &config).expect_err("refused");
        let IqMomentError::UnsupportedMajorMode {
            mode: refused,
            description,
        } = error
        else {
            panic!("expected a major-mode refusal, got {error}");
        };
        assert_eq!(refused, mode);
        assert!(!description.is_empty());
    }
    // Contiguous-PRT modes pass through.
    let config = MomentConfig {
        declared_major_mode: Some(0),
        ..uncensored()
    };
    assert!(process_sweep(&sweep, &config).is_ok());
}

#[test]
fn a_non_uniform_range_mask_is_refused_and_names_the_offending_bin() {
    let mut sweep = SweepBuilder::default().build();
    sweep.range_bins[7] += 120.0;
    let error = process_sweep(&sweep, &uncensored()).expect_err("refused");
    let IqMomentError::NonUniformRangeBins { index, .. } = error else {
        panic!("expected a range-mask refusal, got {error}");
    };
    assert_eq!(index, 7);
}

/// A range ladder that is not a number is refused too, and by the check that
/// exists to refuse a ladder that cannot be drawn.
///
/// `fRangeMaskRes=NaN` multiplies every bin index into a NaN metre range, and
/// the uniformity test above compares deviations with `>` - which is false for
/// every NaN. So the one input that makes EVERY gate's range meaningless was
/// the one input that passed. The reader refuses such a header by name before
/// it gets here; this pins the second door, because `process_sweep` takes a
/// sweep from anywhere.
#[test]
fn a_range_ladder_that_is_not_a_number_is_refused_rather_than_slipping_through() {
    for broken in [f32::NAN, f32::INFINITY] {
        let mut sweep = SweepBuilder::default().build();
        for bin in &mut sweep.range_bins {
            *bin *= broken;
        }
        let error = process_sweep(&sweep, &uncensored())
            .map(|processed| {
                format!(
                    "accepted a {broken} range ladder and placed {} gates from {} m",
                    processed.report.gates, processed.gate_range.first_gate_m
                )
            })
            .expect_err("a range ladder of NaN metres is not a range ladder");
        assert!(
            matches!(error, IqMomentError::NonUniformRangeBins { .. }),
            "expected a range-mask refusal, got {error}"
        );
    }
}

#[test]
fn an_alternate_bin_range_mask_is_uniform_and_processes() {
    // iRangeMask = 0x5555: every second bin at 250 m resolution, which is a
    // uniform 500 m ladder and must not be mistaken for a broken mask.
    let sweep = SweepBuilder {
        gate_spacing_m: 500.0,
        first_bin_m: 500.0,
        ..SweepBuilder::default()
    }
    .build();
    let processed = process_sweep(&sweep, &uncensored()).expect("processes");
    assert_eq!(processed.gate_range.gate_spacing_m, 500);
    assert_eq!(processed.gate_range.first_gate_m, 500);
    assert!(processed.report.worst_range_bin_deviation_m < 1e-3);
}

#[test]
fn a_dwell_longer_than_the_sweep_is_an_error_not_a_panic() {
    let sweep = SweepBuilder {
        pulses: 16,
        ..SweepBuilder::default()
    }
    .build();
    let config = MomentConfig {
        dwell: DwellPlan::contiguous(64),
        ..uncensored()
    };
    assert!(matches!(
        process_sweep(&sweep, &config),
        Err(IqMomentError::DwellExceedsSweep {
            requested: 64,
            available: 16
        })
    ));
}

#[test]
fn a_pulse_whose_bin_count_disagrees_with_the_range_ladder_is_refused() {
    let mut sweep = SweepBuilder::default().build();
    sweep.pulses[3].h.pop();
    let error = process_sweep(&sweep, &uncensored()).expect_err("refused");
    assert!(
        matches!(
            error,
            IqMomentError::RangeBinCountMismatch { pulse_index: 3, .. }
        ),
        "{error}"
    );
}

#[test]
fn one_short_vertical_vector_is_refused_rather_than_demoting_the_whole_sweep() {
    // The failure this guards against is silent and total: computing
    // `dual_pol` as "every pulse's V matches its H" lets a single truncated
    // vector turn a dual-pol sweep into one with no ZDR, no RhoHV and no PhiDP
    // anywhere, visible only as a `false` in the report - while the identical
    // damage to the horizontal channel is refused by name.
    let mut sweep = SweepBuilder::default().build();
    sweep.pulses[5].v.pop();
    let error = process_sweep(&sweep, &uncensored()).expect_err("refused");
    let IqMomentError::VerticalBinCountMismatch {
        pulse_index,
        actual,
        expected,
    } = error
    else {
        panic!("expected a vertical-channel refusal, got {error}");
    };
    assert_eq!(pulse_index, 5);
    assert_eq!(actual, 23);
    assert_eq!(expected, 24);

    // A vertical channel that is missing from ONE pulse is the same fault, not
    // a single-pol sweep.
    let mut sweep = SweepBuilder::default().build();
    sweep.pulses[9].v.clear();
    assert!(matches!(
        process_sweep(&sweep, &uncensored()),
        Err(IqMomentError::VerticalBinCountMismatch { pulse_index: 9, .. })
    ));

    // And a sweep with no vertical channel at all is still single-pol, not an
    // error.
    let single = SweepBuilder {
        dual_pol: false,
        ..SweepBuilder::default()
    }
    .build();
    let processed = process_sweep(&single, &uncensored()).expect("single-pol processes");
    assert!(!processed.report.dual_pol);
}

#[test]
fn gates_with_no_signal_above_the_noise_are_counted_apart_from_the_censor() {
    // `SnrCensor::Off` declines to apply a threshold; it cannot invent a signal
    // the dwell did not measure. Those two events are counted separately so
    // that `censored_fraction` under `Off` cannot be read as a statement about
    // the threshold.
    let mut sweep = SweepBuilder::default().build();
    // Sink gates 16..19 below the receiver noise floor outright, and leave the
    // builder's own gates 20..23 where they are: those sit ABOVE the noise but
    // below the 2 dB threshold, which is the other case.
    let quiet = (noise_linear() * 0.25).sqrt() as f32;
    for pulse in &mut sweep.pulses {
        for bin in 16..20 {
            pulse.h[bin] = (quiet, 0.0);
            pulse.v[bin] = (quiet, 0.0);
        }
    }

    let open = process_sweep(&sweep, &uncensored()).expect("processes");
    assert_eq!(
        open.report.below_noise_samples,
        2 * 4,
        "two dwells, 4 gates"
    );
    assert_eq!(
        open.report.censored_samples,
        open.report.below_noise_samples
    );
    // Nothing was hidden by a threshold, because there was no threshold.
    assert_eq!(open.report.threshold_censored_samples(), 0);
    let dwell = open.dwell(0).expect("dwell");
    assert!(dwell[16].below_noise && dwell[16].censored);
    assert!(dwell[16].reflectivity_dbz.is_nan());
    // The diagnostics that explain it survive.
    assert!(dwell[16].power_h_db.is_finite());
    assert!(!dwell[0].below_noise && !dwell[0].censored);

    // With the operational threshold the same gates are still below the noise,
    // and the threshold removes more on top.
    let operational = process_sweep(&sweep, &MomentConfig::default()).expect("processes");
    assert_eq!(operational.report.below_noise_samples, 2 * 4);
    assert!(operational.report.threshold_censored_samples() > 0);
    assert_eq!(
        operational.report.censored_samples,
        operational.report.below_noise_samples + operational.report.threshold_censored_samples()
    );
}

#[test]
fn the_report_states_the_nyquist_and_unambiguous_range_the_prt_implies() {
    let sweep = SweepBuilder::default().build();
    let processed = process_sweep(&sweep, &uncensored()).expect("processes");
    assert!((f64::from(processed.report.nyquist_velocity_mps) - nyquist()).abs() < 1e-3);
    // c T / 2 for 833.375 us is 125.0 km.
    assert!(
        (processed.report.unambiguous_range_m - 124_919.0).abs() < 200.0,
        "unambiguous range {}",
        processed.report.unambiguous_range_m
    );
}

#[test]
fn a_gate_spectrum_peaks_at_the_velocity_the_pulse_pair_estimate_reports() {
    let sweep = SweepBuilder::default().build();
    let config = uncensored();
    let processed = process_sweep(&sweep, &config).expect("processes");
    for gate in [0usize, 5, 11] {
        let spectrum =
            sweep_gate_spectrum(&sweep, &config, 0, gate, 0).expect("spectrum for a real gate");
        assert_eq!(spectrum.velocities_mps.len(), config.dwell.pulses);
        let peak = spectrum
            .power_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(index, _)| index)
            .expect("a peak");
        let pulse_pair = processed.dwell(0).expect("dwell")[gate].velocity_mps;
        let bin_width = 2.0 * spectrum.nyquist_velocity_mps / config.dwell.pulses as f32;
        assert!(
            (spectrum.velocities_mps[peak] - pulse_pair).abs() <= bin_width,
            "gate {gate}: spectral peak {} vs pulse-pair {pulse_pair}",
            spectrum.velocities_mps[peak]
        );
        assert!((spectrum.range_m - processed.dwell(0).expect("dwell")[gate].range_m).abs() < 1e-3);
    }
}

#[test]
fn spectrum_indices_outside_the_sweep_are_errors_not_panics() {
    let sweep = SweepBuilder::default().build();
    let config = uncensored();
    assert!(matches!(
        sweep_gate_spectrum(&sweep, &config, 99, 0, 0),
        Err(IqMomentError::DwellOutOfRange { index: 99, .. })
    ));
    assert!(matches!(
        sweep_gate_spectrum(&sweep, &config, 0, 999, 0),
        Err(IqMomentError::GateOutOfRange { index: 999, .. })
    ));
    let single = SweepBuilder {
        dual_pol: false,
        ..SweepBuilder::default()
    }
    .build();
    assert!(matches!(
        sweep_gate_spectrum(&single, &config, 0, 0, 1),
        Err(IqMomentError::NoVerticalChannel)
    ));
}

#[test]
fn a_whole_dwell_of_spectra_covers_every_gate_once() {
    let sweep = SweepBuilder::default().build();
    let config = uncensored();
    let spectra = sweep_dwell_spectra(&sweep, &config, 0, 0).expect("spectra");
    assert_eq!(spectra.len(), 24);
    for (gate, spectrum) in spectra.iter().enumerate() {
        assert!((spectrum.range_m - (250.0 + 250.0 * gate as f32)).abs() < 1e-3);
    }
}

#[test]
fn a_volume_built_from_a_processed_sweep_holds_one_cut_at_the_mean_elevation() {
    let sweep = SweepBuilder::default().build();
    let processed = process_sweep(&sweep, &uncensored()).expect("processes");
    let elevation = processed.cut.elevation_deg;
    let volume = processed.into_volume(
        RadarSite::new("KOUN"),
        chrono::DateTime::from_timestamp(1_369_079_161, 0).expect("valid timestamp"),
    );
    assert_eq!(volume.cuts.len(), 1);
    assert!((volume.cuts[0].elevation_deg - elevation).abs() < 1e-6);
    assert!((elevation - 4.0).abs() < 1e-4);
}
