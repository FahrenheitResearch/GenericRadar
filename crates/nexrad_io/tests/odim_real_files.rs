//! Golden-fixture tests for the ODIM_H5 decoder against REAL operational
//! polar volumes (four writers, four HDF5 feature mixes).
//!
//! Fixture provenance (expected values extracted with an independent
//! Python reader — h5py 3.15.1 — not with this crate):
//!
//! - `tests/data/bejab.pvol.hdf`: RMI Belgium Jabbeke C-band PVOL,
//!   2019-06-06 00:00:22 UTC, 11 DBZH sweeps, H5rad 2.0, superblock v0,
//!   gzip-chunked u8 planes. From wradlib/wradlib-data (MIT),
//!   `data/hdf5/bejab.pvol.hdf`.
//! - `tests/data/20130429043000.rad.bewid.pvol.dbzh.scan1.hdf`: RMI Belgium
//!   Wideumont PVOL, 2013-04-29 04:30 UTC, 5 DBZH sweeps, H5rad 2.1 with
//!   VARIABLE-LENGTH string attributes (global-heap path) and a root
//!   /how NI. From wradlib/wradlib-data (MIT).
//! - `tests/data/T_PAGZ35_C_ENMI_20170421090837.hdf`: met.no Røst (norst)
//!   PVOL, 2017-04-21 09:08:37 UTC, 6 DBZH sweeps, H5rad 2.2 with
//!   SUPERBLOCK VERSION 1 and a 720-ray half-degree lowest sweep. From
//!   openradar/open-radar-data (MIT). (First three fetched 2026-06-11.)
//! - `tests/data/espdg.pvol.20260707.dbzh_vradh.h5`: AEMET Spain Perdiguera
//!   Doppler PVOL, 2026-07-07 19:27:49 UTC, 2 sweeps × (VRADH + DBZH),
//!   H5rad 2.4 (IRIS 10.3 export) with VERSION 2 OBJECT HEADERS
//!   (OHDR/OCHK + Jenkins lookup3 checksums) on the leaf metadata groups
//!   under a v0 superblock and old-style groups, and float64 gzip-chunked
//!   data planes. Fetched 2026-07-07 from the OPERA ORD 24h bucket
//!   (`.../2026/07/07/ES/espdg/PVOL/espdg@20260707T1927@0.5_1.5@
//!   DBZH_VRADH.h5`) — the object that first required `hdf5lite` to learn
//!   the v2 header dialect, and the only fixture here that covers it.
//! - `tests/data/seang.scan.20260820.dbzh_th_vradh.h5`: SMHI Sweden
//!   Ängelholm SCAN, 2026-08-20 00:00 UTC, one 0.5° sweep of DBZH + TH +
//!   VRADH, H5rad 2.4 with SIGNED int16 planes and a UTF-8 site name. Fetched
//!   2026-08-20 from the OPERA ORD 24h bucket
//!   (`.../2026/08/20/SE/seang/SCAN/seang@20260820T0000@0.5@
//!   DBZH_TH_VRADH.h5`, CC BY 4.0 — EUMETNET OPERA / SMHI). It is here as the
//!   counter-example to the copied-`what`-group velocity recovery: it
//!   declares the SAME `nodata`/`undetect` on reflectivity and velocity while
//!   scaling each its own way, and it holds genuine 0.00 m/s Doppler gates
//!   where reflectivity was filtered out.

use chrono::{TimeZone, Utc};
use radar_core::{MomentStorage, MomentType};

const BEJAB: &[u8] = include_bytes!("data/bejab.pvol.hdf");
const BEWID: &[u8] = include_bytes!("data/20130429043000.rad.bewid.pvol.dbzh.scan1.hdf");
const NORST: &[u8] = include_bytes!("data/T_PAGZ35_C_ENMI_20170421090837.hdf");
const ESPDG: &[u8] = include_bytes!("data/espdg.pvol.20260707.dbzh_vradh.h5");
const SEANG: &[u8] = include_bytes!("data/seang.scan.20260820.dbzh_th_vradh.h5");

fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

#[test]
fn real_bejab_pvol_decodes_site_geometry_and_gates() {
    assert!(nexrad_io::odim::looks_like_hdf5_bytes(BEJAB));
    let volume = nexrad_io::odim::decode_odim_h5_volume(BEJAB).expect("decode bejab");

    // source = "WMO:06410,RAD:BX42,PLC:Jabbeke,NOD:bejab,..." — NOD wins.
    assert_eq!(volume.site.id, "BEJAB");
    assert_eq!(volume.site.name.as_deref(), Some("Jabbeke"));
    assert_close(volume.site.latitude_deg.unwrap(), 51.1917, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 3.0642, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 50.0, 1e-3, "height");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2019, 6, 6, 0, 0, 22).unwrap()
    );
    assert_eq!(volume.cuts.len(), 11);
    assert_eq!(volume.metadata.decoded_radial_count, 3960);

    // Lowest sweep: 0.3 deg, 360 rays x 598 gates, 500 m spacing from 0 km.
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.3, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 360);
    assert_close(cut.radials[0].azimuth_deg, 0.5, 1e-5, "az0");
    assert_eq!(
        cut.radials[0].time_offset_ms, 237_000,
        "dataset1 starts at 00:04:19, 237 seconds after the volume stamp"
    );
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 500, 598)
    );

    // DBZH golden gates (h5py: phys = 0.5*raw - 32; 0=undetect, 255=nodata).
    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_close(dbzh.scaled_value(0, 0).unwrap(), 22.5, 1e-4, "v[0,0]");
    assert_close(dbzh.scaled_value(0, 299).unwrap(), 33.5, 1e-4, "v[0,299]");
    assert_eq!(dbzh.scaled_value(90, 199), None, "v[90,199] undetect");
    assert_close(dbzh.scaled_value(180, 10).unwrap(), 28.5, 1e-4, "v[180,10]");
    assert_close(
        dbzh.scaled_value(359, 597).unwrap(),
        18.5,
        1e-4,
        "v[359,597]",
    );

    // Top sweep changes geometry (25 deg, 300 gates) — chunk clipping etc.
    let top = &volume.cuts[10];
    assert_close(top.elevation_deg, 25.0, 1e-5, "top elangle");
    assert_eq!(
        top.radials[0].time_offset_ms, 0,
        "dataset11 starts at the 00:00:22 volume stamp"
    );
    assert_eq!(top.radials[0].gate_range.gate_count, 300);
    let top_dbzh = top.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_close(
        top_dbzh.scaled_value(0, 0).unwrap(),
        34.5,
        1e-4,
        "top v[0,0]",
    );
    assert_close(
        top_dbzh.scaled_value(180, 10).unwrap(),
        27.5,
        1e-4,
        "top v[180,10]",
    );
    assert_eq!(top_dbzh.scaled_value(359, 299), None, "top v[359,299]");
}

#[test]
fn real_bewid_pvol_reads_vlen_string_attrs_and_root_nyquist() {
    let volume = nexrad_io::odim::decode_odim_h5_volume(BEWID).expect("decode bewid");

    assert_eq!(volume.site.id, "BEWID");
    assert_eq!(volume.site.name.as_deref(), Some("Wideumont"));
    assert_close(volume.site.latitude_deg.unwrap(), 49.9143, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 5.5056, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 592.0, 1e-3, "height");
    // /what date+time are VARIABLE-LENGTH strings in this writer — decoding
    // them exercises the hdf5lite global-heap path on real bytes.
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2013, 4, 29, 4, 30, 0).unwrap()
    );

    assert_eq!(volume.cuts.len(), 5);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.3, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 360);
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 250, 960)
    );
    // Root /how NI = 7.98 m/s applies to every sweep.
    for cut in &volume.cuts {
        assert_close(
            cut.radials[0].nyquist_velocity_mps.expect("NI"),
            7.98,
            1e-3,
            "root NI",
        );
    }

    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_eq!(dbzh.scaled_value(0, 0), None, "v[0,0] undetect");
    assert_close(
        dbzh.scaled_value(180, 10).unwrap(),
        -22.0,
        1e-4,
        "v[180,10]",
    );
    let top = volume.cuts[4]
        .moments
        .get(&MomentType::Reflectivity)
        .expect("DBZH");
    assert_close(
        top.scaled_value(180, 10).unwrap(),
        -20.5,
        1e-4,
        "top v[180,10]",
    );
}

#[test]
fn real_norst_pvol_reads_superblock_v1_and_half_degree_sweep() {
    let volume = nexrad_io::odim::decode_odim_h5_volume(NORST).expect("decode norst");

    // source = "WMO:01104,NOD:norst" — NOD preferred over the leading WMO.
    assert_eq!(volume.site.id, "NORST");
    assert_close(volume.site.latitude_deg.unwrap(), 67.5307, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 12.0986, 1e-4, "lon");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2017, 4, 21, 9, 8, 37).unwrap()
    );

    assert_eq!(volume.cuts.len(), 6);
    assert_eq!(volume.metadata.decoded_radial_count, 2520);

    // Lowest sweep is 720 half-degree rays; centers at 0.25, 0.75, ...
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.5, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 720);
    assert_eq!(
        cut.radials[0].time_offset_ms, -60_000,
        "a sweep declared before the nominal volume stamp retains its truthful negative offset"
    );
    assert_close(cut.radials[0].azimuth_deg, 0.25, 1e-5, "az0");
    assert_close(cut.radials[1].azimuth_deg, 0.75, 1e-5, "az1");
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 250, 960)
    );

    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_eq!(dbzh.scaled_value(0, 0), None, "v[0,0] undetect");
    assert_close(
        dbzh.scaled_value(180, 320).unwrap(),
        2.5,
        1e-4,
        "v[180,320]",
    );
    assert_close(dbzh.scaled_value(360, 10).unwrap(), -7.0, 1e-4, "v[360,10]");
    assert_eq!(dbzh.scaled_value(719, 959), None, "v[719,959] undetect");

    // Upper sweeps drop back to 360 rays and shorter ranges.
    let top = &volume.cuts[5];
    assert_close(top.elevation_deg, 9.4, 1e-5, "top elangle");
    assert_eq!(top.radials[0].time_offset_ms, 142_000);
    assert_eq!(top.radials.len(), 360);
    assert_eq!(top.radials[0].gate_range.gate_count, 300);
    let top_dbzh = top.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_close(
        top_dbzh.scaled_value(180, 10).unwrap(),
        -18.5,
        1e-4,
        "top v[180,10]",
    );
}

/// AEMET Perdiguera: the version-2 object header (OHDR/OCHK) dialect.
/// Golden values from h5py 3.15.1: float64 planes with gain=1/offset=0,
/// nodata=95.5, undetect=-32.0. Both DBZH AND VRADH declare those two dBZ
/// sentinels (IRIS 10.3 copies the reflectivity `what` group onto velocity),
/// but only DBZH's no-echo gates hold undetect (-32.0); VRADH's no-echo gates
/// are filled with offset (0 m/s), which matches no declared sentinel. DBZH
/// valid gates 18389/107640 (0.5 deg) and 16771/107640 (1.5 deg). After the
/// reflectivity-gated velocity recovery, VRADH keeps 18403/107640 (0.5 deg)
/// and 16794/107640 (1.5 deg): the 0 m/s fill co-located with DBZH no-echo is
/// masked, genuine 0 m/s gates that have echo are kept.
#[test]
fn real_espdg_pvol_decodes_v2_object_headers_end_to_end() {
    assert!(nexrad_io::odim::looks_like_hdf5_bytes(ESPDG));
    let volume = nexrad_io::odim::decode_odim_h5_volume(ESPDG).expect("decode espdg");

    // source = "WMO:08162,RAD:SP47,PLC:Perdiguera,NOD:espdg".
    assert_eq!(volume.site.id, "ESPDG");
    assert_eq!(volume.site.name.as_deref(), Some("Perdiguera"));
    assert_close(volume.site.latitude_deg.unwrap(), 41.734, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), -0.54594, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 835.0, 1e-3, "height");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 7, 7, 19, 27, 49).unwrap()
    );
    assert_eq!(
        volume.metadata.archive_version.as_deref(),
        Some("H5rad 2.4")
    );
    // Two Doppler sweeps, ascending (dataset2 = 0.5 deg sorts first).
    assert_eq!(volume.cuts.len(), 2);
    assert_eq!(volume.metadata.decoded_radial_count, 720);
    assert_close(volume.cuts[0].elevation_deg, 0.4998779, 1e-5, "el0");
    assert_close(volume.cuts[1].elevation_deg, 1.4996338, 1e-5, "el1");
    assert_eq!(
        volume.cuts[0].radials[0].time_offset_ms, 24_000,
        "the elevation sort must not discard dataset2's later sweep timestamp"
    );
    assert_eq!(volume.cuts[1].radials[0].time_offset_ms, 0);

    for (label, cut) in [("0.5deg", &volume.cuts[0]), ("1.5deg", &volume.cuts[1])] {
        assert_eq!(cut.radials.len(), 360, "{label} rays");
        assert_close(cut.radials[0].azimuth_deg, 0.5, 1e-5, "az0");
        let gates = &cut.radials[0].gate_range;
        // AEMET writes where/rstart = 200.0 METRES (IRIS quirk; spec says
        // km). The decoder's physical-sanity rule reinterprets it, so the
        // first bin sits at the true 200 m — not 200 km downrange.
        assert_eq!(
            (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
            (200, 500, 299),
            "{label} gates"
        );
        // No per-dataset NI: root /how NI applies to both sweeps.
        assert_close(
            cut.radials[0].nyquist_velocity_mps.expect("NI"),
            39.9217,
            1e-4,
            "NI",
        );
        assert_eq!(cut.moments.len(), 2, "{label} moments");
        assert!(cut.moments.contains_key(&MomentType::Reflectivity));
        assert!(cut.moments.contains_key(&MomentType::Velocity));
    }

    // Float64 planes decode to F32 storage with NaN sentinels; check the
    // whole-plane health h5py reports, not just spot gates.
    let plane_stats = |grid: &radar_core::MomentGrid| -> (usize, usize, f32, f32) {
        let MomentStorage::F32(values) = &grid.storage else {
            panic!("espdg planes must decode to F32 storage");
        };
        let finite: Vec<f32> = values.iter().copied().filter(|v| v.is_finite()).collect();
        let min = finite.iter().copied().fold(f32::INFINITY, f32::min);
        let max = finite.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        (values.len(), finite.len(), min, max)
    };

    let cut = &volume.cuts[0]; // 0.5 deg
    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    let (total, valid, min, max) = plane_stats(dbzh);
    assert_eq!((total, valid), (107_640, 18_389), "DBZH0.5 valid gates");
    assert_close(min, -31.5, 1e-4, "DBZH0.5 min");
    assert_close(max, 49.5, 1e-4, "DBZH0.5 max");
    assert!(dbzh.scaled_value(0, 0).unwrap().is_nan(), "v[0,0] undetect");
    assert_close(dbzh.scaled_value(0, 7).unwrap(), -16.5, 1e-4, "v[0,7]");
    assert_close(dbzh.scaled_value(209, 5).unwrap(), -22.0, 1e-4, "v[209,5]");
    assert_close(
        dbzh.scaled_value(270, 287).unwrap(),
        24.0,
        1e-4,
        "v[270,287]",
    );
    assert_close(
        dbzh.scaled_value(359, 47).unwrap(),
        -14.0,
        1e-4,
        "v[359,47]",
    );

    // VRADH: AEMET stamps the DBZH sentinels (nodata 95.5 / undetect -32.0)
    // onto the velocity plane and fills no-echo gates with offset (0 m/s), so
    // the raw decode is a 0 m/s wall. The reflectivity-gated recovery masks the
    // 89_237 co-located no-echo fill gates (velocity on offset where DBZH is
    // no-echo) and keeps the 12_869 genuine 0 m/s gates that have real echo.
    let vradh = cut.moments.get(&MomentType::Velocity).expect("VRADH");
    let (total, valid, min, max) = plane_stats(vradh);
    assert_eq!((total, valid), (107_640, 18_403), "VRADH0.5 valid gates");
    // Only the 0 m/s fill is removed, so the real velocity extremes stand.
    assert_close(min, -36.7537, 1e-3, "VRADH0.5 min");
    assert_close(max, 39.8951, 1e-3, "VRADH0.5 max");
    // Fill gate (0,0): DBZH there is undetect (-32.0 no-echo) -> velocity masked.
    assert!(vradh.scaled_value(0, 0).unwrap().is_nan(), "vel[0,0] fill");
    // Genuine 0 m/s gate (0,32): DBZH there is a real -18.0 dBZ echo -> kept.
    assert_close(
        vradh.scaled_value(0, 32).unwrap(),
        0.0,
        1e-6,
        "vel[0,32] real 0",
    );
    // Velocity present where DBZH is no-echo (119,24) is off `offset`, so the
    // recovery leaves it untouched.
    assert_close(
        vradh.scaled_value(119, 24).unwrap(),
        6.5968,
        1e-3,
        "vel[119,24]",
    );

    let cut = &volume.cuts[1]; // 1.5 deg
    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    let (total, valid, min, max) = plane_stats(dbzh);
    assert_eq!((total, valid), (107_640, 16_771), "DBZH1.5 valid gates");
    assert_close(min, -31.5, 1e-4, "DBZH1.5 min");
    assert_close(max, 46.0, 1e-4, "DBZH1.5 max");
    assert_close(
        dbzh.scaled_value(209, 273).unwrap(),
        17.0,
        1e-4,
        "v[209,273]",
    );
    assert_close(
        dbzh.scaled_value(272, 241).unwrap(),
        6.0,
        1e-4,
        "v[272,241]",
    );
    let vradh = cut.moments.get(&MomentType::Velocity).expect("VRADH");
    let (total, valid, _min, _max) = plane_stats(vradh);
    assert_eq!((total, valid), (107_640, 16_794), "VRADH1.5 valid gates");
    // A real Doppler gate is untouched by the recovery.
    assert_close(
        vradh.scaled_value(270, 200).unwrap(),
        3.4554768,
        1e-4,
        "vel[270,200]",
    );
    // Fill gate (0,0) masked; genuine 0 m/s gate (1,20) with DBZH -15.5 kept.
    assert!(vradh.scaled_value(0, 0).unwrap().is_nan(), "vel[0,0] fill");
    assert_close(
        vradh.scaled_value(1, 20).unwrap(),
        0.0,
        1e-6,
        "vel[1,20] real 0",
    );
}

/// SMHI Ängelholm: a conformant writer that REUSES the reflectivity
/// sentinels, and must keep its genuine zero-velocity gates.
///
/// ODIM `nodata`/`undetect` are storage-level codes, so plenty of writers
/// stamp the same pair on every quantity: here both DBZH and VRADH declare
/// nodata −32768 / undetect −32767 over signed int16 planes, while DBZH is
/// scaled by gain 0.01 and VRADH by gain 0.2407. Treating that sentinel
/// agreement as the AEMET copied-`what`-group signature deleted 325 real
/// 0.00 m/s Doppler gates from this scan — every gate where the reflectivity
/// filter had removed the echo but the Doppler reading was valid and zero.
///
/// Golden numbers below come from h5py 3.15.1 reading the same file: 9 649
/// valid velocity gates, 11 158 valid reflectivity gates, 375 velocity gates
/// exactly on the plane's offset (325 without co-located echo, 50 with).
#[test]
fn real_seang_scan_keeps_zero_velocity_gates_a_shared_sentinel_would_delete() {
    let volume = nexrad_io::odim::decode_odim_h5_volume(SEANG).expect("decode seang");

    assert_eq!(volume.site.id, "SEANG");
    assert_eq!(
        volume.site.name.as_deref(),
        Some("Ängelholm"),
        "the source string is UTF-8, not ASCII"
    );
    assert_close(volume.site.latitude_deg.unwrap(), 56.36745, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 12.851691, 1e-4, "lon");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 8, 20, 0, 0, 0).unwrap()
    );

    assert_eq!(volume.cuts.len(), 1, "a SCAN object is a single sweep");
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.5, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 360);
    assert_eq!(cut.radials[0].time_offset_ms, 4_000);
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 500, 480)
    );
    assert_close(
        cut.radials[0].nyquist_velocity_mps.expect("NI"),
        24.068974,
        1e-4,
        "root NI",
    );

    // Signed int16 planes have no unsigned storage to land in, so they
    // decode to physical f32 with NaN where a sentinel was.
    let valid_gates = |grid: &radar_core::MomentGrid| -> usize {
        let MomentStorage::F32(values) = &grid.storage else {
            panic!("signed int16 planes must decode to F32 storage");
        };
        values.iter().filter(|value| value.is_finite()).count()
    };
    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    let vradh = cut.moments.get(&MomentType::Velocity).expect("VRADH");
    assert_eq!(valid_gates(dbzh), 11_158, "DBZH valid gates");
    assert_eq!(
        valid_gates(vradh),
        9_649,
        "every valid Doppler gate must survive: the recovery must not arm here"
    );

    // The gates the sentinel-equality heuristic used to delete: a real
    // 0.00 m/s reading where the reflectivity plane holds `undetect`.
    assert!(
        dbzh.scaled_value(2, 1).unwrap().is_nan(),
        "DBZH (2,1) is undetect"
    );
    assert_eq!(
        vradh.scaled_value(2, 1),
        Some(0.0),
        "a genuine 0.00 m/s gate with no co-located echo must be kept"
    );
    assert_eq!(vradh.scaled_value(49, 1), Some(0.0));
    // ... and one with echo, which was never at risk.
    assert_close(dbzh.scaled_value(4, 16).unwrap(), -8.32, 1e-4, "z(4,16)");
    assert_eq!(vradh.scaled_value(4, 16), Some(0.0));
    // An ordinary Doppler gate pins the int16 scaling itself.
    assert_close(
        vradh.scaled_value(0, 76).unwrap(),
        -17.570_35,
        1e-4,
        "vel(0,76)",
    );
}

/// Split per-moment volumes stay split at this layer.
///
/// Several OPERA networks publish one file per quantity for the same scan
/// (`site@time@elevations@DBZH.h5` beside `...@VRADH.h5`; observed live on
/// the ORD 24-h bucket for BE, PL and RO on 2026-08-18). Each such file is a
/// complete, self-describing PVOL, and `decode_odim_h5_volume` decodes it as
/// one - every cut carrying exactly the quantities that file contains. The
/// BEWID fixture is a real DBZH-only volume, so it pins that contract.
///
/// Joining the halves into one multi-moment frame needs a SET of files and a
/// time-coherence policy, neither of which a single byte buffer can supply;
/// it belongs to whatever opens the files, not to the decoder.
#[test]
fn single_quantity_volumes_decode_with_only_that_moment() {
    let volume = nexrad_io::odim::decode_odim_h5_volume(BEWID).expect("decode bewid");

    assert!(!volume.cuts.is_empty());
    for (index, cut) in volume.cuts.iter().enumerate() {
        let moments: Vec<String> = cut
            .moments_available()
            .iter()
            .map(|moment| moment.short_name().to_owned())
            .collect();
        assert_eq!(
            moments,
            vec!["REF".to_owned()],
            "cut {index} of a DBZH-only file must carry reflectivity and nothing else"
        );
    }
    // A dual-quantity file from the same information model fills both slots,
    // so the single moment above is the file's content and not a decoder cap.
    let both = nexrad_io::odim::decode_odim_h5_volume(ESPDG).expect("decode espdg");
    let moments: Vec<String> = both.cuts[0]
        .moments_available()
        .iter()
        .map(|moment| moment.short_name().to_owned())
        .collect();
    assert_eq!(moments, vec!["REF".to_owned(), "VEL".to_owned()]);
}

/// The decode entry point reports failure through the crate's own error
/// type, so a future router arm can bubble it up with `?` like any other
/// format rather than translating a bespoke one.
#[test]
fn decode_entry_point_returns_the_crate_error_type() {
    let error: nexrad_io::NexradError = nexrad_io::odim::decode_odim_h5_volume(b"not hdf5")
        .expect_err("a non-HDF5 buffer cannot decode");
    assert!(
        error.to_string().contains("HDF5"),
        "error should name the format it failed to read: {error}"
    );
}
