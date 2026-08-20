//! Behaviour pins for the gate filter.
//!
//! The fixtures use the real NEXRAD encodings rather than convenient round
//! numbers, so a test that passes here is testing the arithmetic a sweep
//! actually arrives in, and the split-cut fixture reproduces the elevation
//! wobble that makes companion resolution non-trivial on real data.

use super::*;
use chrono::Utc;
use radar_core::{MomentRow, RadarSite, Radial};

// REF = (raw - 66) / 2 dBZ, VEL = (raw - 129) / 2 m/s,
// rho_HV = (raw + 60.5) / 300. Word 0 is nodata and word 1 is range folded.
const REF_SCALE: f32 = 2.0;
const REF_OFFSET: f32 = 66.0;
const VEL_SCALE: f32 = 2.0;
const VEL_OFFSET: f32 = 129.0;
const RHO_SCALE: f32 = 300.0;
const RHO_OFFSET: f32 = -60.5;

fn dbz_word(dbz: f32) -> u8 {
    (dbz * REF_SCALE + REF_OFFSET).round() as u8
}

fn velocity_word(mps: f32) -> u8 {
    (mps * VEL_SCALE + VEL_OFFSET).round() as u8
}

fn rho_word(rho: f32) -> u8 {
    (rho * RHO_SCALE + RHO_OFFSET).round() as u8
}

fn range_of(first_gate_m: i32, gate_spacing_m: i32, gate_count: usize) -> GateRange {
    GateRange {
        first_gate_m,
        gate_spacing_m,
        gate_count,
    }
}

fn moment_grid(
    moment: MomentType,
    range: GateRange,
    scale: f32,
    offset: f32,
    rows: &[Vec<u8>],
) -> MomentGrid {
    let mut grid = MomentGrid::new_u8(moment, range, scale, offset, Some(0), Some(1));
    for (index, row) in rows.iter().enumerate() {
        grid.push_row(index, MomentRow::U8(row.clone())).unwrap();
    }
    grid
}

fn elevation_cut(
    elevation_deg: f32,
    elevation_number: u8,
    azimuths: &[f32],
    start_ms: i32,
    moments: Vec<MomentGrid>,
) -> ElevationCut {
    let mut cut = ElevationCut::new(elevation_deg, Some(elevation_number));
    for (index, azimuth) in azimuths.iter().enumerate() {
        cut.radials.push(Radial {
            azimuth_deg: *azimuth,
            elevation_deg,
            time_offset_ms: start_ms + index as i32 * 20,
            gate_range: range_of(2_125, 250, 8),
            nyquist_velocity_mps: Some(26.56),
            radial_status: None,
        });
    }
    for grid in moments {
        cut.moments.insert(grid.moment.clone(), grid);
    }
    cut
}

fn radar_volume(cuts: Vec<ElevationCut>) -> RadarVolume {
    let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
    volume.cuts = cuts;
    volume
}

/// One surveillance sweep carrying REF, and rho_HV when it is asked for.
fn single_cut_volume(reflectivity: &[Vec<u8>], correlation: &[Vec<u8>]) -> RadarVolume {
    let azimuths: Vec<f32> = (0..reflectivity.len()).map(|row| row as f32).collect();
    let range = range_of(2_125, 250, reflectivity[0].len());
    let mut moments = vec![moment_grid(
        MomentType::Reflectivity,
        range.clone(),
        REF_SCALE,
        REF_OFFSET,
        reflectivity,
    )];
    if !correlation.is_empty() {
        moments.push(moment_grid(
            MomentType::CorrelationCoefficient,
            range,
            RHO_SCALE,
            RHO_OFFSET,
            correlation,
        ));
    }
    radar_volume(vec![elevation_cut(0.5, 1, &azimuths, 0, moments)])
}

fn reflectivity_of(volume: &RadarVolume, cut_index: usize) -> &MomentGrid {
    volume.cuts[cut_index]
        .moments
        .get(&MomentType::Reflectivity)
        .expect("fixture carries reflectivity")
}

#[test]
fn off_is_inactive_and_summarises_to_nothing() {
    assert!(!GateFilter::OFF.is_active());
    assert_eq!(GateFilter::OFF.hidden_summary(), "");
    assert_eq!(GateFilter::default(), GateFilter::OFF);
}

#[test]
fn every_field_on_its_own_makes_the_filter_active() {
    for filter in [
        GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
        GateFilter {
            velocity_requires_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
        GateFilter {
            min_correlation: Some(0.8),
            ..GateFilter::OFF
        },
        GateFilter {
            hide_range_folded: true,
            ..GateFilter::OFF
        },
        GateFilter {
            min_range_km: Some(10.0),
            ..GateFilter::OFF
        },
    ] {
        assert!(filter.is_active(), "{filter:?} should be active");
        assert!(
            !filter.hidden_summary().is_empty(),
            "{filter:?} needs a label"
        );
    }
}

/// A stored setting that arrived as a NaN, or a zero-kilometre range hole,
/// cannot hide anything. It must not put a pane into a filtered state whose
/// badge would then name a filter that removes nothing.
#[test]
fn a_threshold_that_cannot_hide_anything_is_not_active() {
    assert!(
        !GateFilter {
            min_reflectivity_dbz: Some(f32::NAN),
            min_correlation: Some(f32::NAN),
            velocity_requires_reflectivity_dbz: Some(f32::NAN),
            min_range_km: Some(0.0),
            hide_range_folded: false,
        }
        .is_active()
    );
}

/// Every criterion's phrase, ALONE, spelled out.
///
/// The expected strings are written as an analyst would say them out loud
/// after the fact - "it hid REF below 5 dBZ" - because that is the sentence
/// the pane band puts a verb in front of. Writing them any other way is how
/// the inversion got in: the phrases used to be `REF > 5 dBZ`, which is a
/// true description of what SURVIVED and a false one of what went, and no
/// test could tell the difference because no test said which side it meant.
///
/// One case per criterion, so a future edit that flips one of the five fails
/// on that one rather than on a combined string nobody can attribute.
#[test]
fn each_criterion_names_what_it_hides() {
    let cases: [(GateFilter, &str); 5] = [
        (
            GateFilter {
                min_reflectivity_dbz: Some(5.0),
                ..GateFilter::OFF
            },
            "REF below 5 dBZ",
        ),
        (
            GateFilter {
                velocity_requires_reflectivity_dbz: Some(20.0),
                ..GateFilter::OFF
            },
            "VEL where REF below 20 dBZ",
        ),
        (
            GateFilter {
                min_correlation: Some(0.80),
                ..GateFilter::OFF
            },
            "RhoHV below 0.80",
        ),
        (
            GateFilter {
                hide_range_folded: true,
                ..GateFilter::OFF
            },
            "range-folded gates",
        ),
        (
            GateFilter {
                min_range_km: Some(5.0),
                ..GateFilter::OFF
            },
            "everything inside 5 km",
        ),
    ];
    for (filter, expected) in cases {
        assert_eq!(
            filter.hidden_summary(),
            expected,
            "{filter:?}: the summary is put after a verb of hiding, so it has to \
             name what WENT. Anything phrased from the surviving side tells an \
             analyst to look for the missing echo in the wrong half of the scene"
        );
    }
}

/// The same five, together, in the order the panel lists them.
#[test]
fn the_summary_names_every_active_criterion() {
    let filter = GateFilter {
        min_reflectivity_dbz: Some(5.0),
        velocity_requires_reflectivity_dbz: None,
        min_correlation: Some(0.80),
        hide_range_folded: false,
        min_range_km: None,
    };
    assert_eq!(filter.hidden_summary(), "REF below 5 dBZ, RhoHV below 0.80");

    let everything = GateFilter {
        min_reflectivity_dbz: Some(7.5),
        velocity_requires_reflectivity_dbz: Some(20.0),
        min_correlation: Some(0.95),
        hide_range_folded: true,
        min_range_km: Some(12.5),
    };
    assert_eq!(
        everything.hidden_summary(),
        "REF below 7.5 dBZ, VEL where REF below 20 dBZ, RhoHV below 0.95, \
         range-folded gates, everything inside 12.5 km"
    );
}

/// The badge is the safety rule made concrete. It must be absent exactly when
/// nothing was filtered, and present in every other case - including the case
/// where an active filter happened to hide nothing, because an analyst who has
/// set a threshold needs to know it is on.
#[test]
fn the_badge_appears_whenever_a_filter_ran() {
    assert_eq!(GateFilterReport::INACTIVE.badge(), None);

    let volume = single_cut_volume(&[vec![dbz_word(2.0), dbz_word(40.0), dbz_word(40.0)]], &[]);
    let (_, hid_one) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    assert_eq!(
        hid_one.badge().as_deref(),
        Some("FILTERED: REF below 5 dBZ - 1 of 3 gates hidden (33.3%)")
    );

    let (_, hid_nothing) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(-30.0),
            ..GateFilter::OFF
        },
    );
    assert!(
        hid_nothing.badge().is_some(),
        "a filter that is on says so even on a frame it did not change"
    );
}

#[test]
fn the_badge_groups_large_gate_counts() {
    let mut row = vec![dbz_word(40.0); 4_000];
    for slot in row.iter_mut().take(1_234) {
        *slot = dbz_word(0.0);
    }
    let volume = single_cut_volume(&[row], &[]);
    let (_, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );

    assert_eq!(
        report.badge().as_deref(),
        Some("FILTERED: REF below 5 dBZ - 1,234 of 4,000 gates hidden (30.8%)")
    );
}

#[test]
fn off_touches_nothing_and_reports_nothing() {
    let volume = single_cut_volume(&[vec![dbz_word(0.0), dbz_word(30.0), 1, 0]], &[]);

    let (filtered, report) =
        apply_gate_filter(&volume, 0, reflectivity_of(&volume, 0), &GateFilter::OFF);

    assert!(filtered.is_none());
    assert_eq!(report, GateFilterReport::INACTIVE);
    assert!(report.is_inactive());
}

#[test]
fn min_reflectivity_hides_exactly_the_gates_below_it() {
    // 4, 5, 6 and 30 dBZ, then a range-folded word and a nodata word.
    let volume = single_cut_volume(
        &[vec![
            dbz_word(4.0),
            dbz_word(5.0),
            dbz_word(6.0),
            dbz_word(30.0),
            1,
            0,
        ]],
        &[],
    );

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.expect("a gate was hidden, so a grid was built");

    // Five gates are visible: four values and the range-folded one. The nodata
    // gate was never drawn, so it is neither counted nor hidden.
    assert_eq!(report.gates_visible, 5);
    assert_eq!(report.gates_hidden, 1);
    assert_eq!(report.hidden_by_min_reflectivity, 1);
    assert_eq!(report.hidden_by_range_folded, 0);
    assert_eq!(filtered.scaled_value(0, 0), None);
    assert_eq!(filtered.scaled_value(0, 1), Some(5.0));
    assert_eq!(filtered.scaled_value(0, 2), Some(6.0));
    assert_eq!(filtered.scaled_value(0, 3), Some(30.0));
}

/// The whole point of writing nodata rather than zero.
#[test]
fn a_censored_gate_is_absent_and_a_zero_gate_still_reads_zero() {
    let volume = single_cut_volume(&[vec![dbz_word(-10.0), dbz_word(0.0)]], &[]);

    let (filtered, _) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(-5.0),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.unwrap();

    assert_eq!(filtered.scaled_value(0, 0), None, "censored gate is absent");
    assert_eq!(
        filtered.scaled_value(0, 1),
        Some(0.0),
        "a measured 0 dBZ survives and still reads 0"
    );
    let MomentStorage::U8(values) = &filtered.storage else {
        panic!("expected u8 storage");
    };
    assert_eq!(values[0], 0, "the censored gate holds the nodata word");
    assert_eq!(
        values[1],
        dbz_word(0.0),
        "the zero gate is untouched, not rewritten"
    );
    assert_ne!(values[0], values[1], "absent and zero stay distinguishable");
}

#[test]
fn range_folded_gates_survive_until_asked_to_go() {
    let volume = single_cut_volume(&[vec![dbz_word(30.0), 1, 1, 0]], &[]);

    let (kept, kept_report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    assert!(
        kept.is_none(),
        "no gate is below 5 dBZ, so no grid was built"
    );
    assert_eq!(kept_report.gates_visible, 3);
    assert_eq!(kept_report.gates_hidden, 0);

    let (hidden, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            hide_range_folded: true,
            ..GateFilter::OFF
        },
    );
    let hidden = hidden.unwrap();
    assert_eq!(report.hidden_by_range_folded, 2);
    assert_eq!(report.gates_hidden, 2);
    let MomentStorage::U8(values) = &hidden.storage else {
        panic!("expected u8 storage");
    };
    assert_eq!(values[0], dbz_word(30.0));
    assert_eq!(values[1], 0);
    assert_eq!(values[2], 0);
}

#[test]
fn min_range_hides_the_near_field_by_gate_centre() {
    // Gate centres at 2125, 2375, 2625, 2875, 3125 m.
    let volume = single_cut_volume(&[vec![dbz_word(30.0); 5]], &[]);

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_range_km: Some(2.625),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.unwrap();

    assert_eq!(report.hidden_by_min_range, 2, "2125 m and 2375 m go");
    assert_eq!(filtered.scaled_value(0, 0), None);
    assert_eq!(filtered.scaled_value(0, 1), None);
    assert_eq!(
        filtered.scaled_value(0, 2),
        Some(30.0),
        "the gate centred exactly on the threshold stays"
    );
}

#[test]
fn correlation_censors_from_the_same_sweep_when_it_carries_rho() {
    let volume = single_cut_volume(
        &[vec![dbz_word(30.0), dbz_word(30.0), dbz_word(30.0)]],
        &[vec![rho_word(0.55), rho_word(0.80), rho_word(0.99)]],
    );

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_correlation: Some(0.80),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.unwrap();

    assert_eq!(
        report.correlation_companion,
        CompanionSweep::SameSweep { cut_index: 0 }
    );
    assert_eq!(report.hidden_by_min_correlation, 1);
    assert_eq!(filtered.scaled_value(0, 0), None);
    assert!(filtered.scaled_value(0, 1).is_some());
    assert!(filtered.scaled_value(0, 2).is_some());
}

/// A volume with no rho_HV anywhere. The criterion must go quiet, not blank
/// the picture.
#[test]
fn a_missing_companion_makes_the_criterion_a_no_op_and_says_so() {
    let volume = single_cut_volume(&[vec![dbz_word(30.0); 4]], &[]);

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_correlation: Some(0.99),
            ..GateFilter::OFF
        },
    );

    assert!(filtered.is_none(), "nothing may be hidden without rho_HV");
    assert_eq!(report.gates_hidden, 0);
    assert_eq!(report.correlation_companion, CompanionSweep::Unavailable);
    assert_eq!(
        report.notes(),
        vec!["RhoHV filter idle: no companion sweep".to_owned()]
    );
}

/// The gate a companion has no number for is KEPT, and counted, so the residue
/// is explainable rather than mysterious.
#[test]
fn an_unknown_companion_value_keeps_the_gate_and_is_counted() {
    let volume = single_cut_volume(
        &[vec![dbz_word(30.0), dbz_word(30.0)]],
        // The second gate's rho_HV is nodata.
        &[vec![rho_word(0.40), 0]],
    );

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_correlation: Some(0.95),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.unwrap();

    assert_eq!(report.hidden_by_min_correlation, 1);
    assert_eq!(report.kept_unknown_correlation, 1);
    assert_eq!(filtered.scaled_value(0, 0), None);
    assert_eq!(filtered.scaled_value(0, 1), Some(30.0));
}

/// A split-cut volume shaped like the real one: a surveillance/Doppler pair
/// whose recorded elevations wobble by a quarter of a degree, a second
/// surveillance sweep one nominal tilt up, and a later repeat of the low tilt
/// that is CLOSER in elevation to the Doppler sweep than that sweep's own
/// partner is.
fn split_cut_volume() -> RadarVolume {
    let azimuths: Vec<f32> = (0..8).map(|row| row as f32 * 45.0).collect();
    let range = range_of(2_125, 250, 4);
    let reflectivity = vec![vec![dbz_word(30.0); 4]; 8];
    let correlation = vec![vec![rho_word(0.99); 4]; 8];
    let velocity = vec![vec![velocity_word(10.0); 4]; 8];

    let surveillance = |elevation: f32, number: u8, start_ms: i32| {
        elevation_cut(
            elevation,
            number,
            &azimuths,
            start_ms,
            vec![
                moment_grid(
                    MomentType::Reflectivity,
                    range.clone(),
                    REF_SCALE,
                    REF_OFFSET,
                    &reflectivity,
                ),
                moment_grid(
                    MomentType::CorrelationCoefficient,
                    range.clone(),
                    RHO_SCALE,
                    RHO_OFFSET,
                    &correlation,
                ),
            ],
        )
    };
    let doppler = |elevation: f32, number: u8, start_ms: i32| {
        elevation_cut(
            elevation,
            number,
            &azimuths,
            start_ms,
            vec![moment_grid(
                MomentType::Velocity,
                range.clone(),
                VEL_SCALE,
                VEL_OFFSET,
                &velocity,
            )],
        )
    };

    radar_volume(vec![
        surveillance(0.69, 1, 0),
        doppler(0.44, 2, 18_000),
        surveillance(0.79, 3, 41_000),
        surveillance(0.29, 9, 148_000),
    ])
}

/// The trap this rule exists for. Cut 3 is 0.15 degrees from the Doppler sweep
/// and cut 0 is 0.25 degrees from it, so nearest-in-elevation would reach past
/// the split cut's own surveillance leg for a repeat flown two minutes later.
#[test]
fn the_companion_is_the_adjacent_sweep_and_not_the_nearest_elevation() {
    let volume = split_cut_volume();

    let resolved = resolve_companion_sweep(&volume, 1, &MomentType::CorrelationCoefficient);

    let CompanionSweep::Companion {
        cut_index,
        seconds_from_target,
        ..
    } = resolved
    else {
        panic!("expected a companion, got {resolved:?}");
    };
    assert_eq!(cut_index, 0, "the surveillance leg of the same split cut");
    assert!(
        seconds_from_target < 0.0,
        "the surveillance leg is flown first: {seconds_from_target}"
    );

    let nearest_elevation = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, cut)| {
            *index != 1
                && cut
                    .moments
                    .contains_key(&MomentType::CorrelationCoefficient)
        })
        .min_by(|(_, left), (_, right)| {
            (left.elevation_deg - 0.44)
                .abs()
                .total_cmp(&(right.elevation_deg - 0.44).abs())
        })
        .map(|(index, _)| index);
    assert_eq!(
        nearest_elevation,
        Some(3),
        "the fixture really does contain the trap"
    );
}

/// A tie in scan-order distance is broken by elevation: cut 0 and cut 2 are
/// both one sweep away from cut 1.
#[test]
fn a_scan_order_tie_is_broken_by_elevation() {
    let volume = split_cut_volume();
    assert_eq!(volume.cuts[0].elevation_deg, 0.69);
    assert_eq!(volume.cuts[2].elevation_deg, 0.79);

    let resolved = resolve_companion_sweep(&volume, 1, &MomentType::CorrelationCoefficient);

    assert!(
        matches!(resolved, CompanionSweep::Companion { cut_index: 0, .. }),
        "got {resolved:?}"
    );
}

/// When the sweep carries the moment itself there is no companion to find,
/// which is the non-split-cut path and the one every other radar format will
/// take.
#[test]
fn a_sweep_that_carries_the_moment_is_its_own_companion() {
    let volume = split_cut_volume();
    assert_eq!(
        resolve_companion_sweep(&volume, 0, &MomentType::CorrelationCoefficient),
        CompanionSweep::SameSweep { cut_index: 0 }
    );
    assert_eq!(
        resolve_companion_sweep(&volume, 1, &MomentType::Velocity),
        CompanionSweep::SameSweep { cut_index: 1 }
    );
}

/// A sweep more than half a degree away is never admitted, even when it is the
/// only sweep that carries the moment.
#[test]
fn a_companion_at_a_different_tilt_is_refused() {
    let azimuths = [0.0_f32, 90.0, 180.0, 270.0];
    let range = range_of(2_125, 250, 2);
    let volume = radar_volume(vec![
        elevation_cut(
            0.5,
            1,
            &azimuths,
            0,
            vec![moment_grid(
                MomentType::Velocity,
                range.clone(),
                VEL_SCALE,
                VEL_OFFSET,
                &vec![vec![velocity_word(10.0); 2]; 4],
            )],
        ),
        elevation_cut(
            4.5,
            2,
            &azimuths,
            18_000,
            vec![moment_grid(
                MomentType::CorrelationCoefficient,
                range,
                RHO_SCALE,
                RHO_OFFSET,
                &vec![vec![rho_word(0.10); 2]; 4],
            )],
        ),
    ]);

    assert_eq!(
        resolve_companion_sweep(&volume, 0, &MomentType::CorrelationCoefficient),
        CompanionSweep::Unavailable
    );
    let source = volume.cuts[0].moments.get(&MomentType::Velocity).unwrap();
    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        source,
        &GateFilter {
            min_correlation: Some(0.80),
            ..GateFilter::OFF
        },
    );
    assert!(filtered.is_none(), "a 4 degree tilt may not censor a 0.5");
    assert_eq!(report.gates_hidden, 0);
}

/// Reflectivity gating on a Doppler sweep with no reflectivity of its own
/// reaches the surveillance leg - and reads it by geometry, across azimuths
/// that do not line up and a gate ladder that does not match.
#[test]
fn cross_sweep_gating_maps_by_geometry_and_not_by_index() {
    let doppler_azimuths = [0.0_f32, 90.0, 180.0, 270.0];
    // The surveillance leg is offset half a degree in azimuth and starts its
    // gates 125 m closer, so index equality is impossible.
    let surveillance_azimuths = [0.5_f32, 90.5, 180.5, 270.5];
    let doppler_range = range_of(2_125, 250, 3);
    let surveillance_range = range_of(2_000, 250, 4);

    // Surveillance gate centres: 2000, 2250, 2500, 2750 m.
    // Doppler gate centres:      2125, 2375, 2625 m, which round onto
    // surveillance gates 1, 2 and 3 (2125 is exactly between 2000 and 2250,
    // and `f32::round` breaks that tie away from zero, onto gate 1).
    let reflectivity = vec![vec![dbz_word(50.0), dbz_word(0.0), dbz_word(50.0), dbz_word(0.0),]; 4];
    let velocity = vec![vec![velocity_word(10.0); 3]; 4];

    let volume = radar_volume(vec![
        elevation_cut(
            0.69,
            1,
            &surveillance_azimuths,
            0,
            vec![moment_grid(
                MomentType::Reflectivity,
                surveillance_range,
                REF_SCALE,
                REF_OFFSET,
                &reflectivity,
            )],
        ),
        elevation_cut(
            0.44,
            2,
            &doppler_azimuths,
            18_000,
            vec![moment_grid(
                MomentType::Velocity,
                doppler_range,
                VEL_SCALE,
                VEL_OFFSET,
                &velocity,
            )],
        ),
    ]);
    let source = volume.cuts[1].moments.get(&MomentType::Velocity).unwrap();

    let (filtered, report) = apply_gate_filter(
        &volume,
        1,
        source,
        &GateFilter {
            velocity_requires_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.expect("gates were hidden");

    assert!(
        matches!(
            report.reflectivity_companion,
            CompanionSweep::Companion { cut_index: 0, .. }
        ),
        "got {:?}",
        report.reflectivity_companion
    );
    assert_eq!(report.hidden_by_velocity_reflectivity, 8);
    for row in 0..4 {
        assert_eq!(filtered.scaled_value(row, 0), None, "row {row} gate 0");
        assert_eq!(
            filtered.scaled_value(row, 1),
            Some(10.0),
            "row {row} gate 1"
        );
        assert_eq!(filtered.scaled_value(row, 2), None, "row {row} gate 2");
    }
}

/// The azimuth ring wraps. A sweep drawn at due north must find the companion
/// radials that sit just anticlockwise of 360, or a seam of ungated gates runs
/// straight up the middle of the picture.
#[test]
fn the_companion_lookup_wraps_across_north() {
    let range = range_of(2_125, 250, 2);
    // The sweep being drawn has a radial just BELOW 360 and one just above 0.
    // Each one's nearest companion radial is on the other side of the wrap:
    // 359.9 pairs with 359.4 and 0.1 pairs with 0.6, and a sorted search that
    // did not close the ring would answer neither.
    let volume = radar_volume(vec![
        elevation_cut(
            0.69,
            1,
            &[0.6, 179.8, 359.4],
            0,
            vec![moment_grid(
                MomentType::Reflectivity,
                range.clone(),
                REF_SCALE,
                REF_OFFSET,
                &vec![vec![dbz_word(0.0); 2]; 3],
            )],
        ),
        elevation_cut(
            0.44,
            2,
            &[359.9, 0.1, 180.0],
            18_000,
            vec![moment_grid(
                MomentType::Velocity,
                range,
                VEL_SCALE,
                VEL_OFFSET,
                &vec![vec![velocity_word(10.0); 2]; 3],
            )],
        ),
    ]);
    let source = volume.cuts[1].moments.get(&MomentType::Velocity).unwrap();

    let (filtered, report) = apply_gate_filter(
        &volume,
        1,
        source,
        &GateFilter {
            velocity_requires_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.expect("the radials either side of north were gated");

    assert_eq!(
        report.kept_unknown_reflectivity, 0,
        "no gate went unanswered"
    );
    assert_eq!(report.hidden_by_velocity_reflectivity, 6);
    assert_eq!(filtered.scaled_value(0, 0), None, "the radial below 360");
    assert_eq!(filtered.scaled_value(1, 0), None, "the radial above 0");
    assert_eq!(filtered.scaled_value(2, 0), None, "the radial at due south");
}

/// A companion whose radials are nowhere near the sweep being drawn cannot gate
/// it. Every gate is kept and every gate is counted as unknown.
#[test]
fn a_companion_with_no_nearby_radial_keeps_every_gate() {
    let range = range_of(2_125, 250, 2);
    let reflectivity = vec![vec![dbz_word(0.0); 2]];
    let velocity = vec![vec![velocity_word(10.0); 2]; 4];

    let volume = radar_volume(vec![
        elevation_cut(
            0.69,
            1,
            &[10.0],
            0,
            vec![moment_grid(
                MomentType::Reflectivity,
                range.clone(),
                REF_SCALE,
                REF_OFFSET,
                &reflectivity,
            )],
        ),
        elevation_cut(
            0.44,
            2,
            &[100.0, 150.0, 200.0, 250.0],
            18_000,
            vec![moment_grid(
                MomentType::Velocity,
                range,
                VEL_SCALE,
                VEL_OFFSET,
                &velocity,
            )],
        ),
    ]);
    let source = volume.cuts[1].moments.get(&MomentType::Velocity).unwrap();

    let (filtered, report) = apply_gate_filter(
        &volume,
        1,
        source,
        &GateFilter {
            velocity_requires_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );

    assert!(filtered.is_none(), "nothing may be hidden on a guess");
    assert_eq!(report.gates_hidden, 0);
    assert_eq!(report.kept_unknown_reflectivity, 8);
}

/// Reflectivity gating is for velocity. Pointing it at a reflectivity grid must
/// not quietly become a self-threshold with a different name.
#[test]
fn velocity_gating_does_not_apply_to_a_reflectivity_grid() {
    let volume = single_cut_volume(&[vec![dbz_word(0.0); 4]], &[]);

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            velocity_requires_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );

    assert!(filtered.is_none());
    assert_eq!(report.gates_hidden, 0);
    assert_eq!(
        report.reflectivity_companion,
        CompanionSweep::NotRequested,
        "no companion is resolved for a criterion that does not apply"
    );
}

/// Likewise the reflectivity threshold: it is a reflectivity criterion, and
/// applying it to a velocity grid would censor by comparing metres per second
/// against decibels.
#[test]
fn the_reflectivity_threshold_does_not_apply_to_a_velocity_grid() {
    let volume = split_cut_volume();
    let source = volume.cuts[1].moments.get(&MomentType::Velocity).unwrap();

    let (filtered, report) = apply_gate_filter(
        &volume,
        1,
        source,
        &GateFilter {
            min_reflectivity_dbz: Some(60.0),
            ..GateFilter::OFF
        },
    );

    assert!(filtered.is_none());
    assert_eq!(report.hidden_by_min_reflectivity, 0);
}

/// Two criteria hide the union of what each hides alone, and applying them one
/// after the other in either order lands on the same grid.
#[test]
fn filters_compose_into_a_union_regardless_of_order() {
    let volume = single_cut_volume(
        &[vec![
            dbz_word(2.0),
            dbz_word(2.0),
            dbz_word(40.0),
            dbz_word(40.0),
            dbz_word(40.0),
        ]],
        &[vec![
            rho_word(0.99),
            rho_word(0.40),
            rho_word(0.40),
            rho_word(0.99),
            rho_word(0.99),
        ]],
    );
    let source = reflectivity_of(&volume, 0);

    let weak = GateFilter {
        min_reflectivity_dbz: Some(5.0),
        ..GateFilter::OFF
    };
    let noisy = GateFilter {
        min_correlation: Some(0.80),
        ..GateFilter::OFF
    };
    let both = GateFilter {
        min_reflectivity_dbz: Some(5.0),
        min_correlation: Some(0.80),
        ..GateFilter::OFF
    };

    let (weak_grid, weak_report) = apply_gate_filter(&volume, 0, source, &weak);
    let (noisy_grid, noisy_report) = apply_gate_filter(&volume, 0, source, &noisy);
    let (both_grid, both_report) = apply_gate_filter(&volume, 0, source, &both);
    let weak_grid = weak_grid.unwrap();
    let noisy_grid = noisy_grid.unwrap();
    let both_grid = both_grid.unwrap();

    assert_eq!(weak_report.gates_hidden, 2, "gates 0 and 1");
    assert_eq!(noisy_report.gates_hidden, 2, "gates 1 and 2");
    assert_eq!(both_report.gates_hidden, 3, "the union is gates 0, 1 and 2");
    assert_eq!(both_report.hidden_by_min_reflectivity, 2);
    assert_eq!(both_report.hidden_by_min_correlation, 2);
    assert!(
        both_report.gates_hidden
            <= both_report.hidden_by_min_reflectivity + both_report.hidden_by_min_correlation
    );

    // Order independence, both ways round, against a single combined pass.
    let (weak_then_noisy, _) = apply_gate_filter(&volume, 0, &weak_grid, &noisy);
    let (noisy_then_weak, _) = apply_gate_filter(&volume, 0, &noisy_grid, &weak);
    let weak_then_noisy = weak_then_noisy.unwrap();
    let noisy_then_weak = noisy_then_weak.unwrap();
    assert_eq!(weak_then_noisy.storage, noisy_then_weak.storage);
    assert_eq!(weak_then_noisy.storage, both_grid.storage);
}

#[test]
fn the_mask_indexes_the_grid_it_was_built_for() {
    let volume = single_cut_volume(
        &[
            vec![dbz_word(2.0), dbz_word(40.0)],
            vec![dbz_word(40.0), dbz_word(2.0)],
        ],
        &[],
    );

    let outcome = evaluate_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let mask = outcome.mask.expect("gates were hidden");

    assert_eq!(mask.rows(), 2);
    assert_eq!(mask.gate_count(), 2);
    assert_eq!(mask.hidden_count(), 2);
    assert!(mask.hides(0, 0));
    assert!(!mask.hides(0, 1));
    assert!(!mask.hides(1, 0));
    assert!(mask.hides(1, 1));
    assert!(!mask.hides(9, 9), "out of range is not hidden");
}

/// A mask spanning more than one 64-gate word, so the bit arithmetic is
/// exercised past the first word boundary.
#[test]
fn a_mask_wider_than_one_word_hides_the_right_gates() {
    let mut row = vec![dbz_word(40.0); 200];
    for gate in [0, 63, 64, 65, 127, 128, 199] {
        row[gate] = dbz_word(0.0);
    }
    let volume = single_cut_volume(&[row], &[]);
    let source = reflectivity_of(&volume, 0);

    let outcome = evaluate_gate_filter(
        &volume,
        0,
        source,
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let mask = outcome.mask.unwrap();
    let filtered = masked_grid(source, &mask).expect("a grid with a nodata word can be blanked");

    assert_eq!(mask.hidden_count(), 7);
    for gate in 0..200 {
        let expected = matches!(gate, 0 | 63 | 64 | 65 | 127 | 128 | 199);
        assert_eq!(mask.hides(0, gate), expected, "gate {gate}");
        assert_eq!(
            filtered.scaled_value(0, gate).is_none(),
            expected,
            "gate {gate} in the masked grid"
        );
    }
}

/// rho_HV censors a range-folded gate too. The purple is a measurement of
/// ambiguous range, not a licence to ignore the polarimetric evidence that what
/// is there is not weather.
#[test]
fn correlation_censors_a_range_folded_gate() {
    let volume = single_cut_volume(&[vec![1, 1]], &[vec![rho_word(0.40), rho_word(0.99)]]);

    let outcome = evaluate_gate_filter(
        &volume,
        0,
        reflectivity_of(&volume, 0),
        &GateFilter {
            min_correlation: Some(0.80),
            ..GateFilter::OFF
        },
    );

    assert_eq!(outcome.report.gates_visible, 2);
    assert_eq!(outcome.report.hidden_by_min_correlation, 1);
    let mask = outcome.mask.unwrap();
    assert!(mask.hides(0, 0));
    assert!(!mask.hides(0, 1));
}

#[test]
fn a_floating_point_grid_is_censored_with_nan() {
    let mut source = MomentGrid::new_u16(
        MomentType::Reflectivity,
        range_of(2_125, 250, 3),
        1.0,
        0.0,
        None,
        None,
    );
    source.storage = MomentStorage::F32(vec![0.0, 10.0, 40.0]);
    source.radial_indices = vec![0];

    let mut cut = ElevationCut::new(0.5, Some(1));
    cut.radials.push(Radial {
        azimuth_deg: 0.0,
        elevation_deg: 0.5,
        time_offset_ms: 0,
        gate_range: range_of(2_125, 250, 3),
        nyquist_velocity_mps: None,
        radial_status: None,
    });
    cut.moments.insert(MomentType::Reflectivity, source.clone());
    let volume = radar_volume(vec![cut]);

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        &source,
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.unwrap();

    assert_eq!(report.hidden_by_min_reflectivity, 1);
    let MomentStorage::F32(values) = &filtered.storage else {
        panic!("expected f32 storage");
    };
    assert!(values[0].is_nan(), "censored gate is NaN, not zero");
    assert_eq!(values[1], 10.0);
    assert_eq!(values[2], 40.0);
}

/// One radial of a grid whose encoding puts REAL DATA at raw 0.
///
/// `nodata: None`, `range_folded: Some(1)`, REF scale and offset, so raw 0 is a
/// measurement of -33 dBZ and not an absence. This is the shape a decoder for a
/// format other than NEXRAD can hand over - ODIM, CFRadial and DORADE all
/// express absence some other way - and it is the shape that catches a censor
/// which invents a nodata word.
fn grid_without_a_nodata_word(row: &[u8]) -> (RadarVolume, MomentGrid) {
    let range = range_of(2_125, 250, row.len());
    let mut grid = MomentGrid::new_u8(
        MomentType::Reflectivity,
        range.clone(),
        REF_SCALE,
        REF_OFFSET,
        None,
        Some(1),
    );
    grid.push_row(0, MomentRow::U8(row.to_vec())).unwrap();

    let mut cut = ElevationCut::new(0.5, Some(1));
    cut.radials.push(Radial {
        azimuth_deg: 0.0,
        elevation_deg: 0.5,
        time_offset_ms: 0,
        gate_range: range,
        nyquist_velocity_mps: None,
        radial_status: None,
    });
    cut.moments.insert(MomentType::Reflectivity, grid.clone());
    (radar_volume(vec![cut]), grid)
}

/// A censor may never blank a gate the filter did not select, and it may never
/// redefine what an existing raw word means in order to do so.
///
/// The hostile case: raw 0 is real data at -33 dBZ, the filter selects only the
/// three NEAR gates, and the raw-0 gate is the FARTHEST one - so a censor that
/// declares `nodata = 0` blanks a gate at a range the filter never looked at.
/// This grid uses raw 2 for nothing, so a safe word exists and the censor is
/// expected to find it rather than refuse.
#[test]
fn censoring_a_grid_without_a_nodata_word_never_blanks_an_unselected_gate() {
    // 40, 45, 50 dBZ near, then raw 0 - a real -33 dBZ - farthest out.
    let row = vec![dbz_word(40.0), dbz_word(45.0), dbz_word(50.0), 0];
    let (volume, source) = grid_without_a_nodata_word(&row);

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        &source,
        &GateFilter {
            // The gates sit at 2125, 2375, 2625 and 2875 m, so 2.8 km selects
            // the three near ones and nothing else.
            min_range_km: Some(2.8),
            ..GateFilter::OFF
        },
    );
    let filtered = filtered.expect("an unused raw code exists, so this grid can be censored");

    assert_eq!(report.hidden_by_min_range, 3);
    assert_eq!(report.gates_hidden, 3);
    assert_eq!(
        source.nodata, None,
        "the source grid is not touched by a censor"
    );

    let blanked = filtered.nodata.expect("a word was chosen to mean absent");
    assert!(
        !row.contains(&(blanked as u8)),
        "the word chosen to mean absent, {blanked}, is one this grid already uses"
    );
    assert_ne!(
        Some(blanked),
        source.range_folded,
        "the range-folded word is a drawn colour, not an absence"
    );

    assert_eq!(filtered.scaled_value(0, 0), None);
    assert_eq!(filtered.scaled_value(0, 1), None);
    assert_eq!(filtered.scaled_value(0, 2), None);
    assert_eq!(
        filtered.scaled_value(0, 3),
        Some(-33.0),
        "the gate the filter never selected still reads -33 dBZ"
    );
}

/// And when no safe word exists, the censor refuses rather than blanking gates
/// nobody asked about.
///
/// Every one of the 256 raw codes is in use here, so there is nothing left that
/// can mean "absent". `masked_grid` returns `None`, the caller keeps the sweep
/// it already had, and the report still says what the filter would have hidden
/// so the pane can still put a badge up.
#[test]
fn a_grid_with_no_spare_raw_code_refuses_to_be_blanked() {
    let row: Vec<u8> = (0..=u8::MAX).collect();
    let (volume, source) = grid_without_a_nodata_word(&row);

    let (filtered, report) = apply_gate_filter(
        &volume,
        0,
        &source,
        &GateFilter {
            min_range_km: Some(2.5),
            ..GateFilter::OFF
        },
    );

    assert!(report.gates_hidden > 0, "the filter still selected gates");
    assert!(
        filtered.is_none(),
        "a grid that cannot say 'absent' must not be blanked with a word that means something"
    );
}

/// rho_HV is a correlation coefficient, so a threshold outside 0..=1 names no
/// measurable quantity. The contract's documented range is enforced by the
/// engine rather than left to whichever caller happens to validate first.
#[test]
fn a_correlation_threshold_is_held_to_the_range_a_correlation_lives_in() {
    let above = GateFilter {
        min_correlation: Some(2.0),
        ..GateFilter::OFF
    };
    assert_eq!(above.correlation_threshold(), Some(1.0));
    assert!(above.is_active());
    assert_eq!(above.hidden_summary(), "RhoHV below 1.00");

    for refused in [-1.0, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let filter = GateFilter {
            min_correlation: Some(refused),
            ..GateFilter::OFF
        };
        assert_eq!(
            filter.correlation_threshold(),
            None,
            "{refused} cannot hide a gate, so it must not put a pane into a filtered state"
        );
        assert!(!filter.is_active(), "{refused}");
        assert!(filter.hidden_summary().is_empty(), "{refused}");
    }
}

/// The clamp is not cosmetic: a stored 2.0 used to hide every gate whose rho_HV
/// was known. Held to 1.0 it hides only the gates genuinely below a perfect
/// correlation, and the badge names the threshold that ran.
#[test]
fn an_out_of_range_correlation_threshold_censors_as_the_clamped_one() {
    let volume = single_cut_volume(
        &[vec![dbz_word(40.0), dbz_word(45.0)]],
        &[vec![rho_word(0.40), rho_word(1.0)]],
    );
    let source = reflectivity_of(&volume, 0);

    let clamped = evaluate_gate_filter(
        &volume,
        0,
        source,
        &GateFilter {
            min_correlation: Some(1.0),
            ..GateFilter::OFF
        },
    );
    let out_of_range = evaluate_gate_filter(
        &volume,
        0,
        source,
        &GateFilter {
            min_correlation: Some(2.0),
            ..GateFilter::OFF
        },
    );

    assert_eq!(
        out_of_range.report.hidden_by_min_correlation,
        clamped.report.hidden_by_min_correlation
    );
    assert_eq!(out_of_range.report.gates_hidden, 1);
    assert_eq!(
        out_of_range.report.badge().unwrap(),
        "FILTERED: RhoHV below 1.00 - 1 of 2 gates hidden (50.0%)"
    );
}

/// An active filter with no gates to run against still says it is on.
///
/// The three ways out of `evaluate_gate_filter` short of running - a cut index
/// this volume does not have, a sweep with no radials, a gate ladder of length
/// zero - all used to answer `GateFilterReport::INACTIVE`, whose filter is
/// `GateFilter::OFF`. So `is_inactive()` was true, `badge()` was `None`, and
/// the pane header dropped its filter line for that frame while the chip, the
/// band and the legend badge all still said FILTERED.
///
/// An empty pane is exactly when this matters. The analyst is looking at
/// nothing, and "nothing" has two explanations - clear sky, or a censor that
/// took everything - and the header is where the two are told apart.
#[test]
fn an_active_filter_on_an_empty_sweep_still_says_it_is_on() {
    let filter = GateFilter {
        min_reflectivity_dbz: Some(20.0),
        ..GateFilter::OFF
    };

    // 1. A cut index this volume does not have.
    let volume = single_cut_volume(&[vec![dbz_word(40.0)]], &[]);
    let grid = reflectivity_of(&volume, 0).clone();
    let outcome = evaluate_gate_filter(&volume, 99, &grid, &filter);
    assert_eq!(
        outcome.report.badge().as_deref(),
        Some("FILTERED: REF below 20 dBZ - 0 of 0 gates hidden (0.0%)"),
        "a filter with nowhere to run went silent instead of saying it is on"
    );

    // 2. A sweep with no radials at all.
    let empty_rows: [Vec<u8>; 0] = [];
    let mut empty_volume = radar_volume(vec![elevation_cut(
        0.5,
        1,
        &[],
        0,
        vec![moment_grid(
            MomentType::Reflectivity,
            range_of(2_125, 250, 8),
            REF_SCALE,
            REF_OFFSET,
            &empty_rows,
        )],
    )]);
    let empty_grid = reflectivity_of(&empty_volume, 0).clone();
    let outcome = evaluate_gate_filter(&empty_volume, 0, &empty_grid, &filter);
    assert_eq!(outcome.report.gates_visible, 0);
    assert!(
        !outcome.report.is_inactive(),
        "an empty sweep reported the filter as off: {:?}",
        outcome.report
    );
    assert!(
        outcome.report.badge().is_some(),
        "the pane header would have dropped the filter statement on an empty sweep"
    );

    // 3. A gate ladder of length zero.
    empty_volume.cuts[0].moments.insert(
        MomentType::Reflectivity,
        moment_grid(
            MomentType::Reflectivity,
            range_of(2_125, 250, 0),
            REF_SCALE,
            REF_OFFSET,
            &empty_rows,
        ),
    );
    let no_gates = reflectivity_of(&empty_volume, 0).clone();
    let outcome = evaluate_gate_filter(&empty_volume, 0, &no_gates, &filter);
    assert!(outcome.report.badge().is_some());

    // The distinction that makes this report the right one rather than
    // `not_applicable`: the filter IS in force here, so the pane must not be
    // told the product is exempt from it.
    assert!(
        outcome.report.is_applicable(),
        "an empty sweep is not a product the filter cannot run against"
    );
    assert!(!outcome.report.hid_anything());

    // And an inactive filter still leaves an empty pane clean.
    let off = evaluate_gate_filter(&empty_volume, 0, &no_gates, &GateFilter::OFF);
    assert!(off.report.is_inactive());
    assert_eq!(off.report.badge(), None);
}

/// A product a filter cannot run against says so. It does not go quiet.
#[test]
fn a_product_the_filter_cannot_run_against_says_so_rather_than_going_quiet() {
    let filter = GateFilter {
        min_correlation: Some(0.80),
        ..GateFilter::OFF
    };
    let report = GateFilterReport::not_applicable(
        filter,
        "this product is integrated from the whole volume, not rastered from one sweep",
    );

    assert!(!report.is_inactive(), "a pane must not be left clean");
    assert!(!report.is_applicable());
    assert_eq!(
        report.badge().unwrap(),
        "FILTER NOT APPLIED: RhoHV below 0.80 - this product is integrated from the whole volume, \
         not rastered from one sweep"
    );

    // A filter that is off is not a filter that failed to apply.
    assert_eq!(
        GateFilterReport::not_applicable(GateFilter::OFF, "whatever"),
        GateFilterReport::INACTIVE
    );
    assert!(GateFilterReport::INACTIVE.is_applicable());
}

/// The mask that carries a censor across a shape-changing transform.
#[test]
fn the_absence_delta_names_exactly_the_gates_that_went_missing() {
    let volume = single_cut_volume(
        &[
            vec![dbz_word(40.0), dbz_word(0.0), dbz_word(45.0)],
            vec![dbz_word(1.0), dbz_word(50.0), dbz_word(2.0)],
        ],
        &[],
    );
    let source = reflectivity_of(&volume, 0);
    let outcome = evaluate_gate_filter(
        &volume,
        0,
        source,
        &GateFilter {
            min_reflectivity_dbz: Some(5.0),
            ..GateFilter::OFF
        },
    );
    let mask = outcome.mask.expect("gates were hidden");
    let censored = masked_grid(source, &mask).expect("this grid has a nodata word");

    let delta = absence_delta_mask(source, &censored).expect("gates went absent");
    assert_eq!(delta.hidden_count(), mask.hidden_count());
    for row in 0..2 {
        for gate in 0..3 {
            assert_eq!(
                delta.hides(row, gate),
                mask.hides(row, gate),
                "{row},{gate}"
            );
        }
    }

    assert!(
        absence_delta_mask(source, source).is_none(),
        "nothing went absent between a grid and itself"
    );
}
