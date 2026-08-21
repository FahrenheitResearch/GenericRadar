//! The Level 1 processor's output, drawn by the rasteriser that already exists.
//!
//! This test is the whole claim of the "output shaped so the existing
//! rasteriser can draw it" requirement, made checkable. Nothing between
//! `nexrad_io::iq_moments::process_sweep` and `render2d::render_moment_image`
//! converts anything: the processor emits a `radar_core::ElevationCut` of `f32`
//! `MomentGrid`s, which is the same type the Level II decoder emits, and the
//! renderer reads it through the same `f32` storage path.
//!
//! It lives in `render2d` rather than in `nexrad_io` because `render2d` has
//! `nexrad_io` as a dev-dependency and not the other way round - the renderer
//! may look at the decoder from a test, the decoder must not depend on the
//! renderer at all.

use chrono::{TimeZone, Utc};
use nexrad_io::iq::{IqPulse, IqSweep};
use nexrad_io::iq_moments::estimator::SnrCensor;
use nexrad_io::iq_moments::taper::Taper;
use nexrad_io::iq_moments::{DwellPlan, MomentConfig, process_sweep};
use radar_core::{MomentType, RadarSite};
use render2d::{RasterOptions, render_moment_image};

const WAVELENGTH_M: f32 = 0.1108;
const PRT_S: f32 = 833.375e-6;
const NOISE_DBM: f32 = -80.0;
const SATURATION_DBM: f32 = 6.0;
const GATES: usize = 60;
const GATE_SPACING_M: f32 = 500.0;

/// A sweep that draws a wedge: 90 degrees of azimuth, echo out to two thirds of
/// the gates, noise beyond it.
fn wedge_sweep(pulses: usize) -> IqSweep {
    let noise_linear = 10f64.powf(f64::from(NOISE_DBM - SATURATION_DBM) / 10.0);
    let mut built = Vec::with_capacity(pulses);
    for pulse_index in 0..pulses {
        let mut h = Vec::with_capacity(GATES);
        let mut v = Vec::with_capacity(GATES);
        for gate in 0..GATES {
            let snr_db = if gate < GATES * 2 / 3 {
                45.0 - 0.5 * gate as f64
            } else {
                -10.0
            };
            let snr = 10f64.powf(snr_db / 10.0);
            let amplitude = (noise_linear * (1.0 + snr)).sqrt();
            let velocity = -25.0 + 0.8 * gate as f64;
            let step =
                -4.0 * std::f64::consts::PI * velocity * f64::from(PRT_S) / f64::from(WAVELENGTH_M);
            let phase = step * pulse_index as f64 + 0.11 * gate as f64;
            h.push((
                (amplitude * phase.cos()) as f32,
                (amplitude * phase.sin()) as f32,
            ));
            let vphase = phase + 0.4;
            v.push((
                (amplitude * vphase.cos()) as f32,
                (amplitude * vphase.sin()) as f32,
            ));
        }
        built.push(IqPulse {
            azimuth_deg: 90.0 * pulse_index as f32 / pulses as f32,
            elevation_deg: 0.5,
            prt_seconds: PRT_S,
            prt_previous_seconds: PRT_S,
            h,
            v,
            ..IqPulse::default()
        });
    }
    IqSweep {
        site: "KOUN".to_owned(),
        time_utc: 1_369_079_161,
        wavelength_m: WAVELENGTH_M,
        pulse_width_s: 1.5e-6,
        gate_spacing_m: Some(GATE_SPACING_M),
        first_gate_m: GATE_SPACING_M,
        range_bins: (0..GATES)
            .map(|gate| GATE_SPACING_M * (gate + 1) as f32)
            .collect(),
        noise_dbm: [NOISE_DBM, NOISE_DBM],
        dbz_calibration: -35.5,
        saturation_dbm: SATURATION_DBM,
        pulses: built,
        ..IqSweep::default()
    }
}

fn opaque_pixels(image: &image::ImageBuffer<image::Rgba<u8>, Vec<u8>>) -> usize {
    image.pixels().filter(|pixel| pixel.0[3] > 0).count()
}

#[test]
fn processed_iq_moments_render_through_the_existing_raster_path_unchanged() {
    let sweep = wedge_sweep(1024);
    let config = MomentConfig {
        dwell: DwellPlan::contiguous(64),
        taper: Taper::VonHann,
        censor: SnrCensor::OPERATIONAL,
        ..MomentConfig::default()
    };
    let processed = process_sweep(&sweep, &config).expect("the wedge sweep processes");
    assert_eq!(processed.report.dwells, 16);

    let volume = processed.into_volume(
        RadarSite::new("KOUN"),
        Utc.timestamp_opt(1_369_079_161, 0)
            .single()
            .expect("valid timestamp"),
    );

    let options = RasterOptions {
        width: 512,
        height: 512,
        ..RasterOptions::default()
    };
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
        MomentType::DifferentialPhase,
    ] {
        let image = render_moment_image(&volume, 0, moment.clone(), options)
            .unwrap_or_else(|error| panic!("{moment} should render: {error}"));
        assert_eq!(image.dimensions(), (512, 512));
        let drawn = opaque_pixels(&image);
        assert!(
            drawn > 500,
            "{moment} rendered only {drawn} opaque pixels; the wedge should be visible"
        );
    }
}

#[test]
fn the_snr_censor_shows_up_as_pixels_the_renderer_leaves_empty() {
    // The censoring contract end to end: a censored gate is `NaN` in the grid,
    // the `f32` raster path skips any sample that is not finite, and so the
    // difference between the operational threshold and no threshold at all is
    // visible as drawn area. This is what makes "see what the 2 dB threshold
    // was discarding" a thing an analyst can actually look at.
    let sweep = wedge_sweep(1024);
    let options = RasterOptions {
        width: 512,
        height: 512,
        ..RasterOptions::default()
    };
    let site = RadarSite::new("KOUN");
    let time = Utc
        .timestamp_opt(1_369_079_161, 0)
        .single()
        .expect("valid timestamp");

    let mut drawn = Vec::new();
    for censor in [SnrCensor::OPERATIONAL, SnrCensor::Off] {
        let config = MomentConfig {
            dwell: DwellPlan::contiguous(64),
            censor,
            ..MomentConfig::default()
        };
        let processed = process_sweep(&sweep, &config).expect("processes");
        let censored_samples = processed.report.censored_samples;
        let volume = processed.into_volume(site.clone(), time);
        let image = render_moment_image(&volume, 0, MomentType::Reflectivity, options)
            .expect("reflectivity renders");
        drawn.push((censor, censored_samples, opaque_pixels(&image)));
    }

    let (_, censored_at_threshold, pixels_at_threshold) = drawn[0];
    let (_, censored_off, pixels_off) = drawn[1];
    assert!(censored_at_threshold > 0, "the wedge has gates below 2 dB");
    assert_eq!(censored_off, 0);
    assert!(
        pixels_off > pixels_at_threshold,
        "turning the censor off must reveal area: {pixels_off} vs {pixels_at_threshold}"
    );
}

#[test]
fn a_refused_waveform_never_reaches_the_renderer() {
    let mut sweep = wedge_sweep(256);
    for (index, pulse) in sweep.pulses.iter_mut().enumerate() {
        if index % 2 == 1 {
            pulse.prt_seconds = PRT_S * 2.0 / 3.0;
        }
    }
    assert!(process_sweep(&sweep, &MomentConfig::default()).is_err());
}
