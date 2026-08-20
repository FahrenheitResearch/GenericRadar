//! Golden-fixture tests for the DORADE decoder against real radar bytes.
//!
//! Two writers, two byte orders, two compression states, three generations of
//! field naming — the corpus that shaped the decoder:
//!
//! * `data/swp.1260521225514.COW2.229.1.0_SUR_v215.head24` — the first 37,380
//!   bytes (all descriptor blocks plus the first 24 ray groups, cut at a block
//!   boundary) of a CSWR COW2 surveillance sweepfile: Radx-written,
//!   **big-endian**, **HRD RLE compressed**, CSFD gate geometry, staggered
//!   PRT, Radx `_F` filtered field names.
//! * `data/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3` — the first 51,832
//!   bytes (descriptor blocks plus 3 ray groups, cut at a block boundary) of a
//!   VORTEX-2 NOXP sweepfile: **little-endian**, **uncompressed**, Sigmet
//!   `DB_`-prefixed field names, 8 fields, and the two writer quirks the
//!   decoder documents — RDAT block padding past the declared cell count, and
//!   longitude written into the latitude slot. Excerpted under CC BY 4.0 from
//!   "VORTEX-2 2009-2010 radar data from NOAA X-band dual Polarimetric radar
//!   (NOXP)", Zenodo, doi:10.5281/zenodo.14194361.
//!
//! Every expected value below was extracted with an independent Python block
//! walker and RLE decoder written from the DORADE format document, not with
//! this crate.

use chrono::{TimeZone, Utc};
use nexrad_io::dorade::{
    DoradeScanMode, decode_dorade_sweep, looks_like_dorade_bytes, peek_dorade_sweep,
};
use radar_core::MomentType;

const COW2: &[u8] = include_bytes!("data/swp.1260521225514.COW2.229.1.0_SUR_v215.head24");
const NOXP: &[u8] = include_bytes!("data/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3");

fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

// ---------------------------------------------------------------------------
// COW2: big-endian, RLE compressed
// ---------------------------------------------------------------------------

#[test]
fn real_cow2_sweep_decodes_site_and_geometry() {
    assert!(looks_like_dorade_bytes(COW2));
    let volume = decode_dorade_sweep(COW2).expect("decode COW2 fixture");

    // Site identity and deployment coordinates come from RADD.
    assert_eq!(volume.site.id, "COW2");
    assert_close(volume.site.latitude_deg.unwrap(), 39.74, 1e-4, "latitude");
    assert_close(
        volume.site.longitude_deg.unwrap(),
        -103.2927,
        1e-4,
        "longitude",
    );
    assert_close(volume.site.elevation_m.unwrap(), 1519.0, 0.5, "altitude");

    // Volume time from SSWB; RADD scan mode 8 (surveillance) is a plan view.
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap()
    );
    assert_eq!(
        volume.metadata.archive_version.as_deref(),
        Some("DORADE PPI")
    );
    assert_eq!(
        volume.metadata.compression.as_deref(),
        Some("dorade-hrd-rle")
    );

    // One cut; the fixture's 24 rays include 3 antenna-transition rays that
    // must be dropped.
    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 1.005_255_6, 1e-5, "fixed angle");
    assert_eq!(cut.radials.len(), 21);
    assert_eq!(volume.metadata.skipped_message_count, 3);
    assert_eq!(volume.metadata.decoded_radial_count, 21);

    // CSFD gate geometry: 375 gates, first at 50 m, 100 m spacing.
    let radial = &cut.radials[0];
    assert_eq!(radial.gate_range.gate_count, 375);
    assert_eq!(radial.gate_range.first_gate_m, 50);
    assert_eq!(radial.gate_range.gate_spacing_m, 100);

    // First kept ray: az 73.0, el 0.8184814453125, 22:55:14.280 (offset
    // 280 ms from the SSWB start).
    assert_close(radial.azimuth_deg, 73.0, 1e-4, "azimuth");
    assert_close(radial.elevation_deg, 0.818_481_4, 1e-5, "ray elevation");
    assert_eq!(radial.time_offset_ms, 280);

    // Half-degree ray spacing across the whole kept sweep.
    for (index, pair) in cut.radials.windows(2).enumerate() {
        assert_close(
            pair[1].azimuth_deg - pair[0].azimuth_deg,
            0.5,
            1e-3,
            &format!("azimuth step {index}"),
        );
    }

    // RADD eff_unamb_vel (staggered-PRT extended Nyquist) on every radial.
    assert_close(radial.nyquist_velocity_mps.unwrap(), 68.76, 0.01, "nyquist");
    assert!(
        cut.radials
            .iter()
            .all(|radial| radial.nyquist_velocity_mps.is_some())
    );
}

#[test]
fn real_cow2_sweep_decodes_known_moment_values() {
    let volume = decode_dorade_sweep(COW2).expect("decode COW2 fixture");
    let cut = &volume.cuts[0];

    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
    ] {
        let grid = cut
            .moments
            .get(&moment)
            .unwrap_or_else(|| panic!("missing {moment}"));
        assert_eq!(grid.radial_count(), 21, "{moment} rows");
        assert_eq!(grid.gate_range.gate_count, 375, "{moment} gates");
        assert_eq!(
            grid.gate_range, cut.radials[0].gate_range,
            "{moment} geometry must match the radials"
        );
    }

    // Row 0 = first kept ray (file ray index 3). Raw i16 values from the
    // independent decoder: REF (scale 100) -3030, bad, -69; VEL (scale 100)
    // -586, 478, 452, ..., 5805; ZDR (scale 100) -189, bad, 221; RHOHV
    // (scale 10000) 3235, bad, 9759.
    let reflectivity = &cut.moments[&MomentType::Reflectivity];
    assert_close(
        reflectivity.scaled_value(0, 0).unwrap(),
        -30.30,
        1e-3,
        "REF gate 0",
    );
    assert_eq!(reflectivity.scaled_value(0, 50), None, "REF gate 50 is bad");
    assert_close(
        reflectivity.scaled_value(0, 100).unwrap(),
        -0.69,
        1e-3,
        "REF gate 100",
    );

    let velocity = &cut.moments[&MomentType::Velocity];
    assert_close(velocity.scaled_value(0, 0).unwrap(), -5.86, 1e-3, "VEL 0");
    assert_close(velocity.scaled_value(0, 50).unwrap(), 4.78, 1e-3, "VEL 50");
    assert_close(
        velocity.scaled_value(0, 100).unwrap(),
        4.52,
        1e-3,
        "VEL 100",
    );
    assert_close(
        velocity.scaled_value(0, 374).unwrap(),
        58.05,
        1e-3,
        "VEL 374 (last gate)",
    );
    // Row 2 keeps the opposite fold sign at the same last gate.
    assert_close(
        velocity.scaled_value(2, 374).unwrap(),
        -59.55,
        1e-3,
        "VEL row 2 gate 374",
    );

    let zdr = &cut.moments[&MomentType::DifferentialReflectivity];
    assert_close(zdr.scaled_value(0, 0).unwrap(), -1.89, 1e-3, "ZDR 0");
    assert_close(zdr.scaled_value(0, 100).unwrap(), 2.21, 1e-3, "ZDR 100");

    let rhohv = &cut.moments[&MomentType::CorrelationCoefficient];
    assert_close(rhohv.scaled_value(0, 0).unwrap(), 0.3235, 1e-4, "RHO 0");
    assert_close(rhohv.scaled_value(0, 100).unwrap(), 0.9759, 1e-4, "RHO 100");
    // A different scale factor (10000) on the same sweep must survive.
    assert_eq!(rhohv.scale, 10_000.0);
    assert_eq!(reflectivity.scale, 100.0);
}

#[test]
fn real_cow2_sweep_header_peek_matches_full_decode() {
    let header = peek_dorade_sweep(COW2).expect("peek COW2 fixture");
    assert_eq!(header.instrument, "COW2");
    assert_eq!(header.volume_number, 215);
    assert_eq!(header.sweep_number, 6);
    assert_eq!(header.scan_mode, DoradeScanMode::Ppi);
    assert_close(header.fixed_angle_deg, 1.005_255_6, 1e-5, "fixed angle");
    assert_eq!(
        header.start_time,
        Some(Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap())
    );

    let volume = decode_dorade_sweep(COW2).expect("decode COW2 fixture");
    assert_eq!(header.instrument, volume.site.id);
    assert_close(
        header.fixed_angle_deg,
        volume.cuts[0].elevation_deg,
        1e-6,
        "peek vs decode fixed angle",
    );
    assert_eq!(header.start_time, Some(volume.volume_time));
}

// ---------------------------------------------------------------------------
// NOXP: little-endian, uncompressed
// ---------------------------------------------------------------------------

#[test]
fn real_noxp_sweep_decodes_little_endian_geometry() {
    assert!(looks_like_dorade_bytes(NOXP));
    let volume = decode_dorade_sweep(NOXP).expect("decode NOXP fixture");

    assert_eq!(volume.site.id, "NOXPRVP");
    assert_eq!(
        volume.metadata.compression.as_deref(),
        Some("dorade-uncompressed")
    );
    // RADD scan mode 1 is a sector PPI, still a plan view.
    assert_eq!(
        volume.metadata.archive_version.as_deref(),
        Some("DORADE PPI")
    );
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2009, 5, 9, 14, 39, 23).unwrap()
    );

    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.499_877_93, 1e-6, "fixed angle");
    assert_eq!(cut.elevation_number, Some(1));
    assert_eq!(cut.radials.len(), 3);

    // CSFD: 1001 gates, first at 75 m, 150 m spacing (150 km unambiguous
    // range).
    let radial = &cut.radials[0];
    assert_eq!(radial.gate_range.gate_count, 1001);
    assert_eq!(radial.gate_range.first_gate_m, 75);
    assert_eq!(radial.gate_range.gate_spacing_m, 150);

    // Negative RYIB azimuths normalise into [0, 360).
    assert_close(radial.azimuth_deg, 189.970_1, 1e-4, "azimuth 0");
    assert_close(cut.radials[1].azimuth_deg, 190.972_6, 1e-4, "azimuth 1");
    assert_close(cut.radials[2].azimuth_deg, 191.958_62, 1e-4, "azimuth 2");
    assert_close(radial.elevation_deg, 0.483_398_44, 1e-6, "ray elevation");
    // Ray times equal the SSWB start second, so the offsets are zero.
    assert!(cut.radials.iter().all(|radial| radial.time_offset_ms == 0));

    // RADD eff_unamb_vel: single PRT, 7.57625 m/s.
    assert_close(
        radial.nyquist_velocity_mps.unwrap(),
        7.576_25,
        1e-4,
        "nyquist",
    );
}

#[test]
fn real_noxp_sweep_decodes_all_eight_fields() {
    let volume = decode_dorade_sweep(NOXP).expect("decode NOXP fixture");
    let cut = &volume.cuts[0];

    // DZ/VR/SW plus the Sigmet DB_-prefixed polarimetric names, plus DM
    // (received power) which has no canonical moment and keeps its own name.
    assert_eq!(cut.moments.len(), 8);
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::DifferentialPhase,
        MomentType::CorrelationCoefficient,
        MomentType::SpecificDifferentialPhase,
        MomentType::Unknown("DM".to_owned()),
    ] {
        let grid = cut
            .moments
            .get(&moment)
            .unwrap_or_else(|| panic!("missing {moment}"));
        assert_eq!(grid.radial_count(), 3, "{moment} rows");
    }

    // Row 0 raw i16 values from the independent decoder, all scale 100:
    // DZ 600/1600/-450, VR 0/-42/-18, SW 24/33/44, DB_ZDR 300/788/700,
    // DB_PHIDP 0/10205/6378, DB_RHOHV 100/54/58, DM -3150/-3104/-5598.
    let checks: [(MomentType, [f32; 3]); 7] = [
        (MomentType::Reflectivity, [6.0, 16.0, -4.5]),
        (MomentType::Velocity, [0.0, -0.42, -0.18]),
        (MomentType::SpectrumWidth, [0.24, 0.33, 0.44]),
        (MomentType::DifferentialReflectivity, [3.0, 7.88, 7.0]),
        (MomentType::DifferentialPhase, [0.0, 102.05, 63.78]),
        (MomentType::CorrelationCoefficient, [1.0, 0.54, 0.58]),
        (
            MomentType::Unknown("DM".to_owned()),
            [-31.5, -31.04, -55.98],
        ),
    ];
    for (moment, expected) in checks {
        let grid = &cut.moments[&moment];
        for (gate, value) in expected.iter().enumerate() {
            assert_close(
                grid.scaled_value(0, gate).unwrap(),
                *value,
                1e-3,
                &format!("{moment} row 0 gate {gate}"),
            );
        }
    }

    // Later rows differ, so rows are not being duplicated.
    let reflectivity = &cut.moments[&MomentType::Reflectivity];
    assert_close(
        reflectivity.scaled_value(1, 1).unwrap(),
        20.5,
        1e-3,
        "REF row 1 gate 1",
    );
    assert_close(
        reflectivity.scaled_value(2, 1).unwrap(),
        22.5,
        1e-3,
        "REF row 2 gate 1",
    );

    // KDP is entirely the bad-data sentinel in this sweep: present, but no
    // gate resolves to a value.
    let kdp = &cut.moments[&MomentType::SpecificDifferentialPhase];
    assert!(
        (0..kdp.gate_range.gate_count).all(|gate| kdp.scaled_value(0, gate).is_none()),
        "KDP must be all bad data"
    );
    // The far half of the sweep is out of echo range.
    assert_eq!(reflectivity.scaled_value(0, 1000), None);
}

/// The RDAT blocks in this file carry 1002 words for 1001 declared cells:
/// DORADE pads a block to a 4-byte boundary. The padding word is the bad-data
/// sentinel, so it decodes as an extra empty gate ring unless it is dropped —
/// and it leaves the moment grid disagreeing with its own radials.
#[test]
fn real_noxp_block_padding_does_not_become_an_extra_gate() {
    let volume = decode_dorade_sweep(NOXP).expect("decode NOXP fixture");
    let cut = &volume.cuts[0];
    for (moment, grid) in &cut.moments {
        assert_eq!(
            grid.gate_range.gate_count, 1001,
            "{moment} kept the padded gate count"
        );
        for radial in &cut.radials {
            assert_eq!(
                grid.gate_range, radial.gate_range,
                "{moment} geometry must match every radial"
            );
        }
        assert_eq!(
            grid.storage.len(),
            cut.radials.len() * 1001,
            "{moment} storage must be rows x declared gates"
        );
    }
}

/// This corpus writes the longitude into both the longitude and the latitude
/// slot (-99.99996 and 260.00003 are the same meridian in the two sign
/// conventions). A latitude of 260 is not a latitude.
#[test]
fn real_noxp_unusable_latitude_is_reported_as_unknown() {
    let volume = decode_dorade_sweep(NOXP).expect("decode NOXP fixture");
    assert_eq!(volume.site.latitude_deg, None);
    assert_close(
        volume.site.longitude_deg.unwrap(),
        -99.99996,
        1e-4,
        "longitude",
    );

    let header = peek_dorade_sweep(NOXP).expect("peek NOXP fixture");
    assert_eq!(header.latitude_deg, None);
    assert_eq!(header.instrument, "NOXPRVP");
    assert_eq!(header.scan_mode, DoradeScanMode::Ppi);
    assert_close(header.fixed_angle_deg, 0.499_877_93, 1e-6, "fixed angle");
}

// ---------------------------------------------------------------------------
// Both writers in one volume assembly
// ---------------------------------------------------------------------------

#[test]
fn peeking_is_consistent_with_decoding_for_both_writers() {
    for (label, bytes) in [("COW2", COW2), ("NOXP", NOXP)] {
        let header = peek_dorade_sweep(bytes).unwrap_or_else(|err| panic!("{label}: {err}"));
        let volume = decode_dorade_sweep(bytes).unwrap_or_else(|err| panic!("{label}: {err}"));
        assert_eq!(header.instrument, volume.site.id, "{label} instrument");
        assert_eq!(
            header.start_time,
            Some(volume.volume_time),
            "{label} start time"
        );
        assert_eq!(header.latitude_deg, volume.site.latitude_deg, "{label} lat");
        assert_eq!(
            header.longitude_deg, volume.site.longitude_deg,
            "{label} lon"
        );
        assert_close(
            header.fixed_angle_deg,
            volume.cuts[0].elevation_deg,
            1e-6,
            &format!("{label} fixed angle"),
        );
    }
}

/// Whole-corpus regression over a local directory of real sweepfiles and
/// deployment zips. Opt-in because the corpus is far too large to commit.
#[test]
#[ignore = "requires RADAR_MOBILE_CORPUS_DIR"]
fn mobile_radar_corpus_decodes_every_archive() {
    let Some(corpus) = std::env::var_os("RADAR_MOBILE_CORPUS_DIR").map(std::path::PathBuf::from)
    else {
        eprintln!("skipping corpus test; RADAR_MOBILE_CORPUS_DIR is not set");
        return;
    };
    if !corpus.is_dir() {
        eprintln!("skipping corpus test; {} not found", corpus.display());
        return;
    }

    let mut checked = 0usize;
    for entry in std::fs::read_dir(&corpus)
        .expect("read corpus dir")
        .flatten()
    {
        let path = entry.path();
        let decoded = if nexrad_io::mobile_archive::looks_like_zip_path(&path) {
            nexrad_io::mobile_archive::decode_deployment_zip_from_path(&path)
                .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()))
        } else if nexrad_io::dorade::looks_like_dorade_path(&path) {
            let volume = nexrad_io::mobile_archive::decode_dorade_volume_for_path(&path)
                .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()));
            vec![nexrad_io::mobile_archive::MobileVolume {
                volume,
                member_label: path.display().to_string(),
                member_count: 1,
            }]
        } else {
            continue;
        };
        assert!(!decoded.is_empty(), "{} has no volumes", path.display());
        for entry in &decoded {
            let volume = &entry.volume;
            assert!(!volume.site.id.is_empty());
            assert!(
                !volume.cuts.is_empty(),
                "{} empty volume",
                entry.member_label
            );
            for cut in &volume.cuts {
                assert!(!cut.radials.is_empty());
                assert!(!cut.moments.is_empty());
                for grid in cut.moments.values() {
                    assert_eq!(grid.radial_count(), cut.radials.len());
                    assert_eq!(grid.gate_range, cut.radials[0].gate_range);
                }
            }
        }
        checked += 1;
        eprintln!("{}: {} volumes", path.display(), decoded.len());
    }
    assert!(checked > 0, "no radar files found in corpus");
}
