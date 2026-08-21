//! Opt-in verification against the OU-PRIME MATLAB cube kept outside the
//! repository. The file is not redistributed; set `OU_PRIME_MAT_SAMPLE` to
//! its local path when running this test. The velocity-sign test also takes
//! `OU_PRIME_DORADE_COMPANION`, an extracted processed sweep from ARRC's
//! public 10 May 2010 DORADE archive:
//! <https://arrc.ou.edu/data.html>.

use std::collections::BTreeSet;
use std::path::PathBuf;

use nexrad_io::iq::{DopplerPhaseConvention, IqCalibration, PulseLayout};
use nexrad_io::iq_moments::estimator::PowerReference;
use nexrad_io::iq_moments::{
    DwellPlan, MomentConfig, SnrApplication, process_sweep, sweep_gate_spectrum,
};
use nexrad_io::matlab_iq::decode_ou_prime_mat;
use radar_core::{ElevationCut, MomentGrid, MomentType};

#[test]
#[ignore = "set OU_PRIME_MAT_SAMPLE to the local 10 May 2010 OU-PRIME MAT file"]
fn real_ou_prime_cube_stays_relative_and_uses_one_native_dwell_per_ray() {
    let path =
        PathBuf::from(std::env::var_os("OU_PRIME_MAT_SAMPLE").expect("OU_PRIME_MAT_SAMPLE is set"));
    let bytes = std::fs::read(&path).expect("read OU-PRIME MAT sample");
    let cube = decode_ou_prime_mat(&bytes).expect("decode OU-PRIME MAT sample");

    assert_eq!(cube.radar, "OUPRIME");
    assert_eq!(cube.scan_time_utc, [2010, 5, 10, 22, 47, 11]);
    assert_eq!(
        [cube.azimuth_count, cube.gate_count, cube.pulse_count],
        [150, 960, 32]
    );
    assert_eq!(
        cube.horizontal_sample(0, 0, 0)
            .map(|sample| (sample.re, sample.im)),
        Some((-2667.0, -1424.0))
    );
    assert_eq!(
        cube.vertical_sample(0, 0, 0)
            .map(|sample| (sample.re, sample.im)),
        Some((924.0, -1135.0))
    );
    assert!((cube.prf_hz() - 1180.0).abs() < 0.01);
    assert!((cube.nyquist_velocity_m_s() - 16.048).abs() < 0.001);

    let sweep = cube.into_iq_sweep().expect("map cube to I/Q sweep");
    assert_eq!(sweep.calibration, IqCalibration::RelativeStoredIq);
    assert_eq!(
        sweep.doppler_phase_convention,
        DopplerPhaseConvention::PositiveLagPhaseIsNegativeVelocity
    );
    assert_eq!(sweep.pulse_width_s, None);
    let PulseLayout::Rays(spans) = &sweep.pulse_layout else {
        panic!("MATLAB cube must preserve native ray boundaries");
    };
    assert_eq!(spans.len(), 150);
    assert!(
        spans
            .iter()
            .enumerate()
            .all(|(ray, span)| span.start == ray * 32 && span.len == 32)
    );

    let config = MomentConfig {
        dwell: DwellPlan::contiguous(32),
        ..MomentConfig::default()
    };
    let processed = process_sweep(&sweep, &config).expect("process native rays");
    assert_eq!(processed.report.dwells, 150);
    assert_eq!(processed.report.pulses_used, 4_800);
    assert_eq!(
        processed.report.snr_application,
        SnrApplication::UnavailableNoNoiseCalibration
    );
    assert!((processed.report.nyquist_velocity_mps - 16.048).abs() < 0.001);
    assert_eq!(processed.cut.radials.len(), 150);
    assert!((processed.cut.radials[0].azimuth_deg - 201.252).abs() < 0.01);
    assert!((processed.cut.radials[149].azimuth_deg - 126.752).abs() < 0.01);

    let moments: BTreeSet<_> = processed.cut.moments.keys().cloned().collect();
    assert_eq!(
        moments,
        BTreeSet::from([MomentType::RelativePower, MomentType::Velocity])
    );

    let spectrum = sweep_gate_spectrum(&sweep, &config, 0, 0, 0).expect("relative spectrum");
    assert_eq!(
        spectrum.power_reference,
        PowerReference::RelativeStoredIqSquared
    );
    assert_eq!(spectrum.noise_db, None);
    assert_eq!(spectrum.noise_per_bin_db, None);
}

#[test]
#[ignore = "set OU_PRIME_MAT_SAMPLE and OU_PRIME_DORADE_COMPANION to the matched real files"]
fn ou_prime_velocity_sign_matches_the_processed_companion_sweep() {
    let mat_path = required_path("OU_PRIME_MAT_SAMPLE");
    let dorade_path = required_path("OU_PRIME_DORADE_COMPANION");

    let cube = decode_ou_prime_mat(&std::fs::read(mat_path).expect("read OU-PRIME MAT sample"))
        .expect("decode OU-PRIME MAT sample");
    let sweep = cube.into_iq_sweep().expect("map cube to I/Q sweep");
    let config = MomentConfig {
        dwell: DwellPlan::contiguous(32),
        ..MomentConfig::default()
    };
    let estimated = process_sweep(&sweep, &config).expect("process native rays");
    let reference = nexrad_io::decode_supported_volume_from_path(&dorade_path)
        .expect("decode matched OU-PRIME DORADE sweep");
    let reference_cut = reference.cuts.first().expect("one processed DORADE cut");

    assert_eq!(reference.site.id, "OU-PRIME");
    assert_eq!(reference.volume_time.timestamp(), 1_273_531_630);
    assert!((reference_cut.elevation_deg - estimated.cut.elevation_deg).abs() < 0.2);

    let estimated_velocity = estimated
        .cut
        .moments
        .get(&MomentType::Velocity)
        .expect("estimated velocity");
    let estimated_power = estimated
        .cut
        .moments
        .get(&MomentType::RelativePower)
        .expect("estimated relative power");
    let reference_velocity = reference_cut
        .moments
        .get(&MomentType::Velocity)
        .expect("processed DORADE velocity");
    let reference_reflectivity = reference_cut
        .moments
        .get(&MomentType::Reflectivity)
        .expect("processed DORADE reflectivity");

    let (azimuth_slope, azimuth_offset_deg, power_correlation) = best_azimuth_alignment(
        &estimated.cut,
        estimated_power,
        reference_cut,
        reference_reflectivity,
    );
    eprintln!(
        "matched relative power to processed reflectivity: azimuth = \
         {azimuth_slope:+.0} * az_set + {azimuth_offset_deg:.3} degrees, r={power_correlation:.3}"
    );
    assert!(
        power_correlation > 0.35,
        "relative power did not locate the MAT rays in the matched processed sweep"
    );
    assert_eq!(
        azimuth_slope, 1.0,
        "the source-specific az_set transform regressed; the processed sweep now needs a mirror"
    );
    assert!(
        angular_difference_deg(azimuth_offset_deg, 0.0).abs() < 0.1,
        "the calibrated MAT azimuths still need a {azimuth_offset_deg:.3}-degree offset"
    );

    let mut same_sign_error = Vec::new();
    let mut reversed_sign_error = Vec::new();
    for estimated_row in 0..estimated_velocity.radial_count() {
        let estimated_radial = radial_for_row(&estimated.cut, estimated_velocity, estimated_row);
        let absolute_azimuth =
            (azimuth_slope * estimated_radial.azimuth_deg + azimuth_offset_deg).rem_euclid(360.0);
        let (reference_row, azimuth_error) =
            closest_radial_row(reference_cut, reference_velocity, absolute_azimuth);
        assert!(
            azimuth_error <= 0.3,
            "no processed radial matches {:.2} degrees (nearest differs by {azimuth_error:.2})",
            absolute_azimuth
        );

        for estimated_gate in 0..estimated_velocity.gate_range.gate_count {
            let range_m = estimated_velocity.gate_range.first_gate_m as f32
                + estimated_velocity.gate_range.gate_spacing_m as f32 * estimated_gate as f32;
            let Some(reference_gate) = gate_at_range(reference_velocity, range_m) else {
                continue;
            };
            let Some(reflectivity_dbz) =
                reference_reflectivity.scaled_value(reference_row, reference_gate)
            else {
                continue;
            };
            if reflectivity_dbz < 20.0 {
                continue;
            }
            let Some(estimated_mps) =
                estimated_velocity.scaled_value(estimated_row, estimated_gate)
            else {
                continue;
            };
            let Some(reference_mps) =
                reference_velocity.scaled_value(reference_row, reference_gate)
            else {
                continue;
            };
            if !estimated_mps.is_finite()
                || !reference_mps.is_finite()
                || !(2.0..=8.0).contains(&reference_mps.abs())
                || estimated_mps.abs() > 8.0
            {
                continue;
            }
            same_sign_error.push((estimated_mps - reference_mps).abs());
            reversed_sign_error.push((-estimated_mps - reference_mps).abs());
        }
    }

    assert!(
        same_sign_error.len() >= 1_000,
        "only {} weather-echo matched gates; the files are not a useful sign reference",
        same_sign_error.len()
    );
    let same_median = median(&mut same_sign_error);
    let reversed_median = median(&mut reversed_sign_error);
    eprintln!(
        "matched {} weather-echo gates: same-sign median error {same_median:.3} m/s, \
         reversed-sign {reversed_median:.3} m/s",
        same_sign_error.len()
    );
    assert!(
        same_median + 1.0 < reversed_median,
        "the MAT velocity convention is not pinned: same-sign median error {same_median:.3} \
         m/s versus {reversed_median:.3} m/s after reversing it"
    );
}

fn required_path(variable: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(variable).unwrap_or_else(|| panic!("{variable} is set")))
}

fn radial_for_row<'a>(
    cut: &'a ElevationCut,
    grid: &MomentGrid,
    row: usize,
) -> &'a radar_core::Radial {
    &cut.radials[grid.radial_indices[row]]
}

fn closest_radial_row(cut: &ElevationCut, grid: &MomentGrid, target_deg: f32) -> (usize, f32) {
    (0..grid.radial_count())
        .map(|row| {
            let candidate = radial_for_row(cut, grid, row).azimuth_deg;
            (row, angular_difference_deg(candidate, target_deg).abs())
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .expect("reference velocity has radials")
}

fn angular_difference_deg(left: f32, right: f32) -> f32 {
    (left - right + 180.0).rem_euclid(360.0) - 180.0
}

fn gate_at_range(grid: &MomentGrid, range_m: f32) -> Option<usize> {
    let spacing = grid.gate_range.gate_spacing_m as f32;
    if spacing <= 0.0 {
        return None;
    }
    let fractional = (range_m - grid.gate_range.first_gate_m as f32) / spacing;
    if fractional < -0.25 {
        return None;
    }
    let gate = fractional.round() as usize;
    (gate < grid.gate_range.gate_count && (fractional - gate as f32).abs() <= 0.25).then_some(gate)
}

fn median(values: &mut [f32]) -> f32 {
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

fn best_azimuth_alignment(
    source_cut: &ElevationCut,
    source_power: &MomentGrid,
    reference_cut: &ElevationCut,
    reference_reflectivity: &MomentGrid,
) -> (f32, f32, f32) {
    let mut best = (1.0, 0.0, f32::NEG_INFINITY);
    for slope in [1.0, -1.0] {
        for half_degree in 0..720 {
            let offset = half_degree as f32 * 0.5;
            if let Some(correlation) = power_alignment_correlation(
                source_cut,
                source_power,
                reference_cut,
                reference_reflectivity,
                slope,
                offset,
            ) && correlation > best.2
            {
                best = (slope, offset, correlation);
            }
        }
    }

    let coarse = best;
    for step in -30..=30 {
        let offset = (coarse.1 + step as f32 * 0.02).rem_euclid(360.0);
        if let Some(correlation) = power_alignment_correlation(
            source_cut,
            source_power,
            reference_cut,
            reference_reflectivity,
            coarse.0,
            offset,
        ) && correlation > best.2
        {
            best = (coarse.0, offset, correlation);
        }
    }
    let fitted_offset = fitted_azimuth_offset(
        source_cut,
        source_power,
        reference_cut,
        reference_reflectivity,
        best.0,
        best.1,
    );
    let fitted_correlation = power_alignment_correlation(
        source_cut,
        source_power,
        reference_cut,
        reference_reflectivity,
        best.0,
        fitted_offset,
    )
    .expect("fitted alignment retains the matched rays");
    (best.0, fitted_offset, fitted_correlation)
}

fn fitted_azimuth_offset(
    source_cut: &ElevationCut,
    source_grid: &MomentGrid,
    reference_cut: &ElevationCut,
    reference_grid: &MomentGrid,
    slope: f32,
    trial_offset_deg: f32,
) -> f32 {
    let mut offsets = Vec::new();
    for source_row in 0..source_grid.radial_count() {
        let source = radial_for_row(source_cut, source_grid, source_row).azimuth_deg;
        let trial = (slope * source + trial_offset_deg).rem_euclid(360.0);
        let (reference_row, error) = closest_radial_row(reference_cut, reference_grid, trial);
        if error <= 0.3 {
            let reference =
                radial_for_row(reference_cut, reference_grid, reference_row).azimuth_deg;
            offsets.push((reference - slope * source).rem_euclid(360.0));
        }
    }
    median(&mut offsets)
}

fn power_alignment_correlation(
    source_cut: &ElevationCut,
    source_power: &MomentGrid,
    reference_cut: &ElevationCut,
    reference_reflectivity: &MomentGrid,
    slope: f32,
    offset_deg: f32,
) -> Option<f32> {
    let mut matched_rays = 0;
    let mut count = 0.0_f64;
    let mut sum_x = 0.0_f64;
    let mut sum_y = 0.0_f64;
    let mut sum_xx = 0.0_f64;
    let mut sum_yy = 0.0_f64;
    let mut sum_xy = 0.0_f64;

    for source_row in 0..source_power.radial_count() {
        let source_azimuth = radial_for_row(source_cut, source_power, source_row).azimuth_deg;
        let target = (slope * source_azimuth + offset_deg).rem_euclid(360.0);
        let (reference_row, azimuth_error) =
            closest_radial_row(reference_cut, reference_reflectivity, target);
        if azimuth_error > 0.3 {
            continue;
        }
        matched_rays += 1;

        for source_gate in (1..source_power.gate_range.gate_count).step_by(4) {
            let range_m = source_power.gate_range.first_gate_m as f32
                + source_power.gate_range.gate_spacing_m as f32 * source_gate as f32;
            let Some(reference_gate) = gate_at_range(reference_reflectivity, range_m) else {
                continue;
            };
            let Some(relative_db) = source_power.scaled_value(source_row, source_gate) else {
                continue;
            };
            let Some(reflectivity_dbz) = reference_reflectivity
                .scaled_value(reference_row, reference_gate)
                .filter(|value| *value >= -10.0)
            else {
                continue;
            };
            if !relative_db.is_finite() {
                continue;
            }

            // The missing radar constant is an additive offset. Range spreading
            // is not: adding 20 log10(r_km) puts relative received power and
            // processed reflectivity on shapes that can be correlated without
            // pretending the unknown offset is known.
            let range_corrected = relative_db + 20.0 * (range_m / 1_000.0).max(0.125).log10();
            let x = f64::from(range_corrected);
            let y = f64::from(reflectivity_dbz);
            count += 1.0;
            sum_x += x;
            sum_y += y;
            sum_xx += x * x;
            sum_yy += y * y;
            sum_xy += x * y;
        }
    }

    if matched_rays < 100 || count < 1_000.0 {
        return None;
    }
    let covariance = count * sum_xy - sum_x * sum_y;
    let spread_x = count * sum_xx - sum_x * sum_x;
    let spread_y = count * sum_yy - sum_y * sum_y;
    let denominator = (spread_x * spread_y).sqrt();
    (denominator > 0.0).then_some((covariance / denominator) as f32)
}
