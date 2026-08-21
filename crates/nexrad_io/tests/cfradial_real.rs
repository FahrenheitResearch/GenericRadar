//! CfRadial 1.x decoder tests.
//!
//! The real-data fixture is ARM X-SAPR (X-band Scanning ARM Precipitation
//! Radar) PPI at the Southern Great Plains site, 2011-05-20 10:54:16 UTC,
//! 40 rays x 42 gates, `reflectivity_horizontal`. Source: ARM-DOE/pyart
//! `pyart/testing/data/example_cfradial_ppi.nc` (BSD-3-Clause; itself
//! ray/gate-decimated from the full X-SAPR file by Py-ART's
//! `make_small_cfradial_ppi.py`).
//!
//! It is stored TWICE, in both netCDF containers, because CfRadial 1.x is
//! written into both and this decoder reads both:
//!
//! * `cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc` — the published file, byte
//!   for byte, in the netCDF-4 (HDF5) container that CfRadial 1 in the wild
//!   is dominantly written in.
//! * `cfrad.xsapr_sgp_ppi_20110520.classic.nc` — the same file copied
//!   variable-for-variable into the NETCDF3_CLASSIC (CDF-1) container with
//!   no mask and no scaling applied, so the values are the published ones
//!   and only the container differs. Classic is what early Radx wrote.
//!
//! netCDF4-python compares every variable of the two as equal, so the pair
//! is a container-difference and nothing else — which is what makes
//! `the_netcdf4_and_classic_containers_of_one_volume_decode_identically` a
//! test of the readers rather than of the data.
//!
//! Every golden number below was read with netCDF4-python 1.7.4, an
//! independent reader, not with this crate.
//!
//! The multi-sweep, packed-short, `_FillValue`, RHI and CDF-2 (64-bit offset)
//! paths are additionally pinned by the hand-built fixtures further down.
//! Those are synthetic on purpose — they make one decode rule fail one test —
//! and they SUPPLEMENT rather than replace real data: the same paths were
//! verified on real multi-sweep volumes (NCAR SPOL 9-sweep surveillance,
//! WSR-88D KDDC 14-sweep, CSWR DOW8 RHI) outside the repository, where the
//! decode matched both netCDF4-python and the reference decoder gate for gate.

use chrono::{DateTime, TimeZone, Utc};
use nexrad_io::cfradial::{decode_cfradial1_volume, looks_like_netcdf3_bytes};
use radar_core::{GateRange, MomentStorage, MomentType};

const XSAPR_PPI: &[u8] = include_bytes!("data/cfrad.xsapr_sgp_ppi_20110520.classic.nc");
/// The same volume in the container it was PUBLISHED in. See
/// `the_netcdf4_and_classic_containers_of_one_volume_decode_identically`.
const XSAPR_PPI_NETCDF4: &[u8] = include_bytes!("data/cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc");
/// A five-variable netCDF-4 CfRadial volume, small enough that netCDF-C
/// keeps its links in COMPACT storage. Written by netCDF-C itself; see
/// `tests/data/gen_cfradial_nc4_compact.py` for the generator and the
/// values it declares.
const TINY_COMPACT_LINKS: &[u8] = include_bytes!("data/cfrad.tiny_compact_links.netcdf4.nc");
/// A volume whose fields were DECLARED and (mostly) never written, in both
/// containers. `_FillValue` is -9999.0 on all three fields; `reflectivity`
/// (contiguous) and `velocity` (chunked) hold no bytes at all, and
/// `spectrum_width` (chunked) has only its first four rays. See
/// `tests/data/gen_cfradial_unwritten_storage.py`.
const UNWRITTEN_NETCDF4: &[u8] = include_bytes!("data/cfrad.unwritten_storage.netcdf4.nc");
const UNWRITTEN_CLASSIC: &[u8] = include_bytes!("data/cfrad.unwritten_storage.classic.nc");
/// Rays of the unwritten-storage fixture, and how many of them `spectrum_width`
/// actually carries.
const UNWRITTEN_RAYS: usize = 8;
const UNWRITTEN_GATES: usize = 6;
const UNWRITTEN_WRITTEN_RAYS: usize = 4;
/// An ODIM_H5 polar volume: the OTHER radar format that carries the HDF5
/// signature, used to pin that the router still tells the two apart.
/// Belgian Wideumont PVOL, already in this crate's fixtures for the ODIM
/// tests (EUMETNET OPERA / RMI Belgium sample).
const ODIM_PVOL: &[u8] = include_bytes!("data/20130429043000.rad.bewid.pvol.dbzh.scan1.hdf");

#[track_caller]
fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

#[test]
fn real_xsapr_ppi_decodes_site_geometry_and_gates() {
    assert!(looks_like_netcdf3_bytes(XSAPR_PPI));
    let volume = decode_cfradial1_volume(XSAPR_PPI).expect("decode X-SAPR PPI");

    assert_eq!(volume.site.id, "xsapr-sgp");
    assert_close(volume.site.latitude_deg.unwrap(), 36.490833, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), -97.594_17, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 214.0, 1e-3, "alt");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2011, 5, 20, 10, 54, 16).unwrap()
    );

    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.4998779, 1e-6, "fixed angle");
    assert_eq!(cut.radials.len(), 40);
    assert_close(cut.radials[0].azimuth_deg, 359.936_83, 1e-3, "az0");
    assert_close(cut.radials[0].elevation_deg, 0.483398, 1e-3, "el0");
    assert_close(cut.radials[20].azimuth_deg, 179.945_07, 1e-3, "az20");
    assert_close(
        cut.radials[0].nyquist_velocity_mps.expect("nyquist"),
        17.220499,
        1e-3,
        "nyq",
    );
    // `time(time)` counts from the epoch in its own `units` attribute —
    // "seconds since 2011-05-20T10:54:08Z" — which this file sets 8 s before
    // `time_coverage_start` (10:54:16Z) and which its `time_reference`
    // variable repeats. `time_offset_ms` is measured from `volume_time`, so
    // the 8 s between the two epochs comes off every ray. Reading the raw
    // seconds as offsets from `time_coverage_start` would put ray 0 eight
    // seconds late.
    //
    // Independently checked: the raw seconds are 8 for ray 0, 1 for ray 20
    // and 7 for ray 39 (netCDF4-python), so the rays run 10:54:09 to
    // 10:54:22 — and the file's own `time_coverage_end`, 10:54:15Z, is
    // 10:54:08 + 7 s, i.e. it only lines up with a ray time under this
    // reading.
    assert_eq!(cut.radials[0].time_offset_ms, 0);
    assert_eq!(cut.radials[20].time_offset_ms, -7000);
    assert_eq!(cut.radials[39].time_offset_ms, -1000);

    // Py-ART's decimation left range centers at 0, 960, 1920, ...
    // `first_gate_m` in this model is the range to the CENTER of gate 0
    // (as it is for NEXRAD, ICD 2620002), so it is that first center
    // unchanged. `meters_between_gates` still says 60 m in this file — the
    // decimation never updated it — which is why the spacing comes from the
    // coordinate values instead.
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 960, 42)
    );

    // "reflectivity_horizontal" is one of CF's own spellings, and the
    // variable also carries standard_name "equivalent_reflectivity_factor";
    // either way it is reflectivity, and a volume whose only field decoded
    // as Unknown would offer no products at all.
    let reflectivity = cut
        .moments
        .get(&MomentType::Reflectivity)
        .expect("reflectivity_horizontal decodes as reflectivity");
    assert_eq!(reflectivity.radial_count(), 40);
    assert_close(
        reflectivity.scaled_value(0, 0).unwrap(),
        -6.05,
        1e-3,
        "[0,0]",
    );
    assert_close(
        reflectivity.scaled_value(0, 21).unwrap(),
        23.30,
        1e-3,
        "[0,21]",
    );
    assert_close(
        reflectivity.scaled_value(10, 14).unwrap(),
        25.23,
        1e-3,
        "[10,14]",
    );
    assert_close(
        reflectivity.scaled_value(20, 10).unwrap(),
        20.54,
        1e-3,
        "[20,10]",
    );
    assert_close(
        reflectivity.scaled_value(39, 41).unwrap(),
        19.68,
        1e-3,
        "[39,41]",
    );

    // 1665 of 1680 gates carry data; the rest are _FillValue = -9999.
    let valid = (0..40)
        .flat_map(|ray| (0..42).map(move |gate| (ray, gate)))
        .filter(|(ray, gate)| {
            reflectivity
                .scaled_value(*ray, *gate)
                .is_some_and(|value| !value.is_nan())
        })
        .count();
    assert_eq!(valid, 1665);
}

/// The gate lookup every consumer in this workspace uses, copied from
/// `render2d`: a slant range in metres resolves to
/// `round((range_m - first_gate_m) / gate_spacing_m)`.
fn gate_at_range(gates: &GateRange, range_m: f32) -> isize {
    ((range_m - gates.first_gate_m as f32) / gates.gate_spacing_m.max(1) as f32).round() as isize
}

#[test]
fn a_gate_centre_resolves_to_its_own_gate() {
    // `first_gate_m` is a gate CENTER, so the true center of gate g has to
    // resolve back to g. Setting it to the near edge instead shifts every
    // gate half a spacing inward, and because the lookup rounds half away
    // from zero, the center of gate 0 then reads gate 1 — the file's first
    // gate never appears where the file put it.
    let volume = decode_cfradial1_volume(XSAPR_PPI).expect("decode X-SAPR PPI");
    let gates = &volume.cuts[0].radials[0].gate_range;
    // Range centers, straight out of netCDF4-python: 0, 960, ..., 39360.
    for gate in [0usize, 1, 20, 41] {
        let centre = (gate as f32) * 960.0;
        assert_eq!(
            gate_at_range(gates, centre),
            gate as isize,
            "range {centre} m must resolve to gate {gate}"
        );
    }
    // And the sampler's last-gate center (derived/sampling.rs) has to be the
    // file's last range value.
    let last_centre = gates.first_gate_m + gates.gate_spacing_m * (gates.gate_count as i32 - 1);
    assert_eq!(last_centre, 39_360);

    // Same rule on a file whose first gate is not at the radar.
    let synthetic = decode_cfradial1_volume(&synthetic_two_sweep(false)).expect("decode synthetic");
    let gates = &synthetic.cuts[0].radials[0].gate_range;
    assert_eq!(gate_at_range(gates, 125.0), 0);
    assert_eq!(gate_at_range(gates, 875.0), 3);
}

#[test]
fn ray_times_count_from_the_units_epoch_not_the_coverage_start() {
    // `units` puts the epoch 56 s before `time_coverage_start`, which is
    // what `volume_time` becomes, so a ray at 56.0 s is AT the volume time.
    let bytes = timed_volume("seconds since 2026-08-19T12:34:00Z", &[56.0, 57.5, 55.25]);
    let volume = decode_cfradial1_volume(&bytes).expect("decode shifted epoch");
    let radials = &volume.cuts[0].radials;
    assert_eq!(radials[0].time_offset_ms, 0);
    assert_eq!(radials[1].time_offset_ms, 1500);
    assert_eq!(radials[2].time_offset_ms, -750);

    // A file that counts from the Unix epoch instead — legal CF, and the
    // reading that saturates i32 milliseconds if the epoch is ignored.
    let bytes = timed_volume(
        "seconds since 1970-01-01T00:00:00Z",
        &[1_787_142_896.0, 1_787_142_897.0, 1_787_142_895.5],
    );
    let volume = decode_cfradial1_volume(&bytes).expect("decode absolute epoch");
    let radials = &volume.cuts[0].radials;
    assert_eq!(radials[0].time_offset_ms, 0);
    assert_eq!(radials[1].time_offset_ms, 1000);
    assert_eq!(radials[2].time_offset_ms, -500);

    // No `units` attribute: the offsets stay relative to time_coverage_start.
    let bytes = timed_volume("", &[0.0, 1.5, 3.0]);
    let volume = decode_cfradial1_volume(&bytes).expect("decode without units");
    let radials = &volume.cuts[0].radials;
    assert_eq!(radials[0].time_offset_ms, 0);
    assert_eq!(radials[1].time_offset_ms, 1500);

    // Units in something other than seconds are not silently rescaled;
    // the epoch is ignored and the reading falls back.
    let bytes = timed_volume("days since 2026-08-19T12:34:00Z", &[0.0, 2.0, 4.0]);
    let volume = decode_cfradial1_volume(&bytes).expect("decode odd units");
    assert_eq!(volume.cuts[0].radials[1].time_offset_ms, 2000);
}

#[test]
fn a_missing_time_coverage_start_falls_back_to_the_ray_epoch() {
    // `time_coverage_start` is mandatory in CfRadial 1.4, so a file without
    // it is malformed — but the ray offsets are measured FROM the volume
    // time, so falling back to the Unix epoch puts the volume time decades
    // away from the epoch `time(time)` counts from and folds that difference
    // into every ray. The i32 the model carries saturates, and every ray in
    // the volume reports the same clamped instant. The ray epoch is the
    // honest fallback: the offsets come back as the file's own `time`
    // values, which is what the reference decoder reports too.
    let bytes = timed_volume_inner(
        "seconds since 2026-08-19T12:34:48Z",
        &[8.0, 9.0, 10.0],
        None,
    );
    let volume = decode_cfradial1_volume(&bytes).expect("decode without coverage start");
    let radials = &volume.cuts[0].radials;
    assert_eq!(radials[0].time_offset_ms, 8_000);
    assert_eq!(radials[1].time_offset_ms, 9_000);
    assert_eq!(radials[2].time_offset_ms, 10_000);
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 8, 19, 12, 34, 48).unwrap()
    );

    // Neither the coverage start nor a usable `units`: the Unix epoch is
    // still the last resort, and the offsets are still the raw seconds.
    let bytes = timed_volume_inner("", &[8.0, 9.0, 10.0], None);
    let volume = decode_cfradial1_volume(&bytes).expect("decode without either");
    assert_eq!(volume.volume_time, DateTime::<Utc>::UNIX_EPOCH);
    assert_eq!(volume.cuts[0].radials[0].time_offset_ms, 8_000);
}

/// Three rays, one sweep, `time_coverage_start` 2026-08-19T12:34:56Z, with
/// `time`'s `units` attribute set to `units` (omitted when empty).
fn timed_volume(units: &str, seconds: &[f64]) -> Vec<u8> {
    timed_volume_inner(units, seconds, Some("2026-08-19T12:34:56Z"))
}

/// As [`timed_volume`], with the `time_coverage_start` variable written only
/// when `coverage_start` is `Some`.
fn timed_volume_inner(units: &str, seconds: &[f64], coverage_start: Option<&str>) -> Vec<u8> {
    let mut time = Var::new("time", &[0], NcType::Double).data(f64_bytes(seconds));
    if !units.is_empty() {
        time = time.attr_text("units", units);
    }
    let mut builder = NcBuilder::new(false)
        .gattr_text("instrument_name", "TIMED")
        .dim("time", seconds.len() as u32)
        .dim("range", 2)
        .dim("sweep", 1)
        .dim("string_length", 32)
        .var(time)
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&vec![90.0; seconds.len()])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&vec![0.5; seconds.len()])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5])));
    if let Some(start) = coverage_start {
        builder = builder.var(
            Var::new("time_coverage_start", &[3], NcType::Char).data(char_matrix(&[start], 32)),
        );
    }
    builder
        .var(
            Var::new("DBZ", &[0, 1], NcType::Float).data(f32_bytes(&vec![10.0; seconds.len() * 2])),
        )
        .build()
}

#[test]
fn cf_field_names_and_standard_names_reach_canonical_moments() {
    // ARM and Py-ART write the spec's own spellings rather than DORADE short
    // names; a volume whose fields all land under Unknown offers no products
    // at all. `velocity_texture` is a texture field and must NOT be caught.
    let bytes = NcBuilder::new(false)
        .gattr_text("instrument_name", "CFNAMES")
        .gattr_text("site_name", "   ")
        .dim("time", 2)
        .dim("range", 2)
        .dim("sweep", 1)
        .var(Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 1.0])))
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&[10.0, 20.0])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&[0.5, 0.5])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5])))
        .var(
            Var::new("mean_doppler_velocity", &[0, 1], NcType::Float)
                .data(f32_bytes(&[1.0, 2.0, 3.0, 4.0])),
        )
        .var(
            Var::new("spectral_width", &[0, 1], NcType::Float)
                .data(f32_bytes(&[0.5, 0.5, 0.5, 0.5])),
        )
        .var(
            // House name, CF standard name: the only handle on this field.
            Var::new("DR", &[0, 1], NcType::Float)
                .attr_text("standard_name", "log_differential_reflectivity_hv")
                .data(f32_bytes(&[1.5, 1.5, 1.5, 1.5])),
        )
        .var(
            // Py-ART writes its texture retrieval with the VELOCITY
            // standard name and long name — see the dedicated test below —
            // so the fixture carries the attributes the real file has.
            Var::new("velocity_texture", &[0, 1], NcType::Float)
                .attr_text(
                    "standard_name",
                    "radial_velocity_of_scatterers_away_from_instrument",
                )
                .attr_text("long_name", "Mean dopper velocity")
                .data(f32_bytes(&[9.0, 9.0, 9.0, 9.0])),
        )
        .build();

    let volume = decode_cfradial1_volume(&bytes).expect("decode CF-named fields");
    let cut = &volume.cuts[0];
    assert!(cut.moments.contains_key(&MomentType::Velocity));
    assert!(cut.moments.contains_key(&MomentType::SpectrumWidth));
    assert!(
        cut.moments
            .contains_key(&MomentType::DifferentialReflectivity),
        "standard_name must identify a house-named field"
    );
    assert!(
        cut.moments
            .contains_key(&MomentType::Unknown("velocity_texture".to_owned())),
        "a texture field must not be guessed into a moment"
    );
    assert_close(
        cut.moments
            .get(&MomentType::Velocity)
            .expect("velocity")
            .scaled_value(1, 0)
            .unwrap(),
        3.0,
        1e-6,
        "velocity row 1",
    );

    // A whitespace-only site_name is absent, not a blank label.
    assert_eq!(volume.site.name, None);
}

#[test]
fn a_derived_field_never_takes_a_moment_slot_from_its_standard_name() {
    // Py-ART derives a field by copying the SOURCE moment's metadata
    // dictionary and overwriting only the values, so on the real ARM CSAPR2
    // CMAC granule `velocity_texture` — the local standard deviation of
    // Doppler velocity, the input to Py-ART's gate filters — is written with
    // `standard_name = radial_velocity_of_scatterers_away_from_instrument`
    // and `long_name = "Mean dopper velocity"`, both the velocity
    // boilerplate verbatim (typo included). `simulated_velocity`, a wind
    // profile projected onto the beam rather than a measurement, inherits
    // the same pair.
    //
    // The damage needs a file where the derived field is the only one the
    // velocity standard name identifies: on the CMAC granule itself
    // `clutter_masked_velocity` sorts first and takes the slot, so name
    // order alone hides it. Here reflectivity is the only other field, which
    // is the shape a texture-plus-reflectivity product has — and the wrong
    // reading colours a texture on the velocity table and offers it to
    // dealiasing as the volume's velocity.
    for derived in ["velocity_texture", "simulated_velocity"] {
        let bytes = reflectivity_plus_field_named(derived);
        let volume = decode_cfradial1_volume(&bytes).expect("decode derived field");
        let cut = &volume.cuts[0];
        assert!(cut.moments.contains_key(&MomentType::Reflectivity));
        assert!(
            !cut.moments.contains_key(&MomentType::Velocity),
            "{derived} was promoted to velocity by the standard_name it inherited"
        );
        let kept = cut
            .moments
            .get(&MomentType::Unknown(derived.to_owned()))
            .unwrap_or_else(|| panic!("{derived} must keep its own CF name"));
        assert_close(kept.scaled_value(0, 0).unwrap(), 3.5, 1e-6, derived);
    }

    // The guard is on those derived NAMES, not on the standard name itself:
    // a house-named field that really is the moment still reaches it, which
    // is the whole reason the attribute is consulted.
    let bytes = reflectivity_plus_field_named("clutter_masked_velocity");
    let volume = decode_cfradial1_volume(&bytes).expect("decode house-named velocity");
    assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
}

#[test]
fn every_texture_field_keeps_its_own_name_whatever_moment_it_textures() {
    // Py-ART names every texture retrieval `<field>_texture` — its
    // `default_config.py` carries `reflectivity_texture`,
    // `differential_reflectivity_texture`, `cross_correlation_ratio_texture`
    // and `differential_phase_texture` alongside the `velocity_texture` its
    // gate filters run on — and a texture built by copying the source
    // moment's metadata dictionary inherits that moment's `standard_name`,
    // which is exactly what the ARM granules do for velocity. So the guard is
    // on the `_texture` SUFFIX rather than on the one name seen in the wild:
    // narrowed to that name, every other member of the family would still
    // reach a moment through the attribute fallback, which is the same defect
    // one field over. Each texture below is the volume's ONLY field, so there
    // is nothing else that could be holding the slot it must not take.
    for (texture, standard_name) in [
        ("reflectivity_texture", "equivalent_reflectivity_factor"),
        (
            "differential_reflectivity_texture",
            "log_differential_reflectivity_hv",
        ),
        (
            "cross_correlation_ratio_texture",
            "cross_correlation_ratio_hv",
        ),
        ("differential_phase_texture", "differential_phase_hv"),
        (
            "velocity_texture",
            "radial_velocity_of_scatterers_away_from_instrument",
        ),
    ] {
        let bytes = one_field_volume(texture, standard_name);
        let volume = decode_cfradial1_volume(&bytes).expect("decode texture field");
        let moments = &volume.cuts[0].moments;
        assert_eq!(
            moments.len(),
            1,
            "{texture} declaring standard_name {standard_name} produced {:?}",
            moments.keys().collect::<Vec<_>>()
        );
        assert!(
            moments.contains_key(&MomentType::Unknown(texture.to_owned())),
            "{texture} was promoted to a moment by the standard_name it inherited"
        );
    }
}

#[test]
fn arm_spells_the_correlation_coefficient_its_own_way_and_it_still_reaches_rho() {
    // ARM's radars write rhohv as `copol_correlation_coeff`, declaring
    // `standard_name = radar_correlation_coefficient_hv` and a long name
    // that says "Copolar correlation coefficient (also known as rhohv)".
    // Neither spelling is the spec's, so before both were known the CSAPR2
    // CMAC and CSAPR granules resolved NO correlation coefficient at all -
    // and a volume that offers no RHO gives a rhohv gate filter nothing to
    // censor on, so the filter appears to run and removes nothing.
    //
    // The two decoys are the ones a sloppier rule would swallow. The
    // `uncorrected_` copy carries the same standard name, so the PLAIN field
    // has to take the slot and leave it under its own CF name. And the
    // Ka-SACR products carry `co_to_crosspol_correlation_coeff`, a
    // copolar-H to crosspolar-V correlation that is NOT rhohv - it sorts
    // FIRST here, ahead of the real field, so a substring or suffix rule
    // would hand it the slot before the right field was ever reached.
    let bytes = arm_correlation_volume();
    let volume = decode_cfradial1_volume(&bytes).expect("decode ARM correlation names");
    let cut = &volume.cuts[0];
    let rho = cut
        .moments
        .get(&MomentType::CorrelationCoefficient)
        .expect("copol_correlation_coeff must reach RHO");
    assert_close(
        rho.scaled_value(0, 0).expect("rhohv gate"),
        0.98,
        1e-6,
        "the RHO slot must hold the plain copol_correlation_coeff",
    );
    assert!(
        cut.moments.contains_key(&MomentType::Unknown(
            "uncorrected_copol_correlation_coeff".to_owned()
        )),
        "the uncorrected copy must not take the slot from the plain field"
    );
    assert!(
        cut.moments.contains_key(&MomentType::Unknown(
            "co_to_crosspol_correlation_coeff".to_owned()
        )),
        "a copolar-to-crosspolar correlation is not rhohv"
    );
}

/// One sweep carrying the three correlation-shaped fields ARM writes, each
/// with the standard name that product declares and a value of its own so
/// the slot's occupant can be told apart. Two rays, two gates.
fn arm_correlation_volume() -> Vec<u8> {
    NcBuilder::new(false)
        .gattr_text("instrument_name", "ARMRHO")
        .dim("time", 2)
        .dim("range", 2)
        .dim("sweep", 1)
        .var(Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 1.0])))
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&[10.0, 20.0])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&[0.5, 0.5])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5])))
        .var(
            Var::new("co_to_crosspol_correlation_coeff", &[0, 1], NcType::Float)
                .attr_text(
                    "standard_name",
                    "radar_correlation_coefficient_copolar_h_crosspolar_v",
                )
                .attr_text(
                    "long_name",
                    "Copolar-H to crosspolar-V correlation coefficient",
                )
                .data(f32_bytes(&[0.25, 0.25, 0.25, 0.25])),
        )
        .var(
            Var::new("copol_correlation_coeff", &[0, 1], NcType::Float)
                .attr_text("standard_name", "radar_correlation_coefficient_hv")
                .attr_text(
                    "long_name",
                    "Copolar correlation coefficient (also known as rhohv)",
                )
                .data(f32_bytes(&[0.98, 0.98, 0.98, 0.98])),
        )
        .var(
            Var::new(
                "uncorrected_copol_correlation_coeff",
                &[0, 1],
                NcType::Float,
            )
            .attr_text("standard_name", "radar_correlation_coefficient_hv")
            .attr_text("long_name", "Uncorrected copolar correlation coefficient")
            .data(f32_bytes(&[0.50, 0.50, 0.50, 0.50])),
        )
        .build()
}

/// One sweep whose only `(time, range)` field is `field`, declaring
/// `standard_name` as Py-ART's field-metadata table writes it. Two rays, two
/// gates, constant values.
fn one_field_volume(field: &str, standard_name: &str) -> Vec<u8> {
    NcBuilder::new(false)
        .gattr_text("instrument_name", "PYART")
        .dim("time", 2)
        .dim("range", 2)
        .dim("sweep", 1)
        .var(Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 1.0])))
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&[10.0, 20.0])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&[0.5, 0.5])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5])))
        .var(
            Var::new(field, &[0, 1], NcType::Float)
                .attr_text("standard_name", standard_name)
                .data(f32_bytes(&[3.5, 3.5, 3.5, 3.5])),
        )
        .build()
}

/// One sweep of `reflectivity` plus a second `(time, range)` field called
/// `field`, declaring the velocity standard name and long name exactly as
/// Py-ART writes them. Two rays, two gates, constant values.
fn reflectivity_plus_field_named(field: &str) -> Vec<u8> {
    NcBuilder::new(false)
        .gattr_text("instrument_name", "PYART")
        .dim("time", 2)
        .dim("range", 2)
        .dim("sweep", 1)
        .var(Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 1.0])))
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&[10.0, 20.0])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&[0.5, 0.5])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5])))
        .var(
            Var::new("reflectivity", &[0, 1], NcType::Float)
                .attr_text("standard_name", "equivalent_reflectivity_factor")
                .data(f32_bytes(&[20.0, 20.0, 20.0, 20.0])),
        )
        .var(
            Var::new(field, &[0, 1], NcType::Float)
                .attr_text(
                    "standard_name",
                    "radial_velocity_of_scatterers_away_from_instrument",
                )
                .attr_text("long_name", "Mean dopper velocity")
                .data(f32_bytes(&[3.5, 3.5, 3.5, 3.5])),
        )
        .build()
}

#[test]
fn a_blank_site_name_is_absent() {
    // Real behaviour: the NCAR SPOL surveillance volumes write site_name as
    // the empty string. Some("") reaches the UI as a blank site label.
    let bytes = one_sweep_with_site("");
    let volume = decode_cfradial1_volume(&bytes).expect("decode blank site");
    assert_eq!(volume.site.name, None);
    assert_eq!(volume.site.id, "BLANK");

    let bytes = one_sweep_with_site("Kwajalein");
    let volume = decode_cfradial1_volume(&bytes).expect("decode named site");
    assert_eq!(volume.site.name.as_deref(), Some("Kwajalein"));
}

fn one_sweep_with_site(site_name: &str) -> Vec<u8> {
    NcBuilder::new(false)
        .gattr_text("instrument_name", "BLANK")
        .gattr_text("site_name", site_name)
        .dim("time", 2)
        .dim("range", 2)
        .dim("sweep", 1)
        .var(Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 1.0])))
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&[10.0, 20.0])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&[0.5, 0.5])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5])))
        .var(Var::new("DBZ", &[0, 1], NcType::Float).data(f32_bytes(&[1.0, 2.0, 3.0, 4.0])))
        .build()
}

#[test]
fn netcdf4_container_never_sniffs_as_classic() {
    // The HDF5 signature that netCDF-4 (and therefore CfRadial 2, and
    // CfRadial 1 written by modern Radx) carries must not reach this decoder.
    assert!(!looks_like_netcdf3_bytes(b"\x89HDF\r\n\x1a\n"));
    assert!(looks_like_netcdf3_bytes(b"CDF\x01anything"));
    assert!(looks_like_netcdf3_bytes(b"CDF\x02anything"));
}

#[test]
fn rejects_a_classic_netcdf_that_is_not_cfradial() {
    let bytes = NcBuilder::new(false)
        .dim("x", 3)
        .var(Var::new("v", &[0], NcType::Float).data(f32_bytes(&[1.0, 2.0, 3.0])))
        .build();
    let err = decode_cfradial1_volume(&bytes).expect_err("not CfRadial");
    assert!(
        err.to_string().contains("time/range dimensions"),
        "unhelpful error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Synthetic CfRadial fixtures.
// ---------------------------------------------------------------------------

/// Rays 0-2 form sweep 0 at 0.5 deg, rays 3-5 sweep 1 at 1.5 deg. `DBZ` is a
/// packed short (scale_factor 0.01) with one `_FillValue` gate; `VEL` is a
/// float with its own `_FillValue`, so both packing paths run in one file.
fn synthetic_two_sweep(offset64: bool) -> Vec<u8> {
    let mut dbz = Vec::new();
    for ray in 0..6i16 {
        for gate in 0..4i16 {
            if ray == 0 && gate == 3 {
                dbz.push(-32768); // _FillValue
            } else {
                dbz.push(1000 + 100 * ray + 1000 * gate);
            }
        }
    }
    let mut vel = Vec::new();
    for ray in 0..6 {
        for gate in 0..4 {
            if ray == 2 && gate == 2 {
                vel.push(-9999.0f32); // _FillValue
            } else {
                vel.push(ray as f32 - 2.5 + gate as f32 * 0.25);
            }
        }
    }

    NcBuilder::new(offset64)
        .gattr_text("instrument_name", "SYNTH")
        .gattr_text("site_name", "Synthetic Site")
        .gattr_text("version", "CfRadial-1.4")
        .dim("time", 6)
        .dim("range", 4)
        .dim("sweep", 2)
        .dim("string_length", 32)
        .var(
            Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 0.5, 1.0, 2.0, 2.5, 3.0])),
        )
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[125.0, 375.0, 625.0, 875.0])))
        .var(
            Var::new("azimuth", &[0], NcType::Float)
                .data(f32_bytes(&[10.0, 20.0, 30.0, 10.0, 20.0, 30.0])),
        )
        .var(
            Var::new("elevation", &[0], NcType::Float)
                .data(f32_bytes(&[0.5, 0.5, 0.5, 1.5, 1.5, 1.5])),
        )
        .var(Var::new("nyquist_velocity", &[0], NcType::Float).data(f32_bytes(&[16.5; 6])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(&[0.5, 1.5])))
        .var(Var::new("sweep_start_ray_index", &[2], NcType::Int).data(i32_bytes(&[0, 3])))
        .var(Var::new("sweep_end_ray_index", &[2], NcType::Int).data(i32_bytes(&[2, 5])))
        .var(
            Var::new("sweep_mode", &[2, 3], NcType::Char).data(char_matrix(
                &["azimuth_surveillance", "azimuth_surveillance"],
                32,
            )),
        )
        .var(Var::new("latitude", &[], NcType::Double).data(f64_bytes(&[39.7392])))
        .var(Var::new("longitude", &[], NcType::Double).data(f64_bytes(&[-104.9903])))
        .var(Var::new("altitude", &[], NcType::Double).data(f64_bytes(&[1609.0])))
        .var(
            Var::new("time_coverage_start", &[3], NcType::Char)
                .data(char_matrix(&["2026-08-19T12:34:56Z"], 32)),
        )
        .var(
            Var::new("DBZ", &[0, 1], NcType::Short)
                .attr_f32("scale_factor", 0.01)
                .attr_f32("add_offset", 0.0)
                .attr_i16("_FillValue", -32768)
                .data(i16_bytes(&dbz)),
        )
        .var(
            Var::new("VEL", &[0, 1], NcType::Float)
                .attr_f32("_FillValue", -9999.0)
                .data(f32_bytes(&vel)),
        )
        .build()
}

#[test]
fn synthetic_volume_splits_sweeps_and_applies_cf_packing() {
    let bytes = synthetic_two_sweep(false);
    let volume = decode_cfradial1_volume(&bytes).expect("decode synthetic");

    assert_eq!(volume.site.id, "SYNTH");
    assert_eq!(volume.site.name.as_deref(), Some("Synthetic Site"));
    assert_close(volume.site.latitude_deg.unwrap(), 39.7392, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), -104.9903, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 1609.0, 1e-3, "alt");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 8, 19, 12, 34, 56).unwrap()
    );
    assert_eq!(
        volume.metadata.archive_version.as_deref(),
        Some("CfRadial-1.4")
    );
    assert_eq!(volume.metadata.decoded_radial_count, 6);

    // sweep_start/end_ray_index split six rays into two cuts, not one.
    assert_eq!(volume.cuts.len(), 2);
    let low = &volume.cuts[0];
    let high = &volume.cuts[1];
    assert_close(low.elevation_deg, 0.5, 1e-6, "cut0 fixed");
    assert_close(high.elevation_deg, 1.5, 1e-6, "cut1 fixed");
    assert_eq!(low.radials.len(), 3);
    assert_eq!(high.radials.len(), 3);
    assert_close(low.radials[0].azimuth_deg, 10.0, 1e-6, "cut0 az0");
    assert_close(high.radials[2].azimuth_deg, 30.0, 1e-6, "cut1 az2");
    assert_close(high.radials[0].elevation_deg, 1.5, 1e-6, "cut1 el0");
    assert_eq!(low.radials[1].time_offset_ms, 500);
    assert_eq!(high.radials[0].time_offset_ms, 2000);
    assert_close(
        low.radials[0].nyquist_velocity_mps.expect("nyquist"),
        16.5,
        1e-6,
        "nyquist",
    );

    // range centers 125, 375, ... -> 250 m gates, gate 0 CENTRED at 125 m.
    let gates = &low.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (125, 250, 4)
    );

    // DBZ -> REF via the canonical stem; scale_factor 0.01 applied.
    let low_ref = low.moments.get(&MomentType::Reflectivity).expect("REF");
    assert_eq!(low_ref.radial_count(), 3);
    assert_close(low_ref.scaled_value(0, 0).unwrap(), 10.0, 1e-4, "REF[0,0]");
    assert_close(low_ref.scaled_value(0, 2).unwrap(), 30.0, 1e-4, "REF[0,2]");
    assert_close(low_ref.scaled_value(2, 3).unwrap(), 42.0, 1e-4, "REF[2,3]");
    assert!(
        low_ref.scaled_value(0, 3).unwrap().is_nan(),
        "packed _FillValue must decode to NaN"
    );

    // Rows are re-indexed per sweep: cut 1 row 0 is file ray 3.
    let high_ref = high.moments.get(&MomentType::Reflectivity).expect("REF");
    assert_close(high_ref.scaled_value(0, 0).unwrap(), 13.0, 1e-4, "REF[3,0]");

    // VEL is an unpacked float with its own fill value.
    let low_vel = low.moments.get(&MomentType::Velocity).expect("VEL");
    assert_close(low_vel.scaled_value(0, 0).unwrap(), -2.5, 1e-6, "VEL[0,0]");
    assert_close(low_vel.scaled_value(1, 1).unwrap(), -1.25, 1e-6, "VEL[1,1]");
    assert!(
        low_vel.scaled_value(2, 2).unwrap().is_nan(),
        "float _FillValue must decode to NaN"
    );
}

#[test]
fn cdf2_64bit_offsets_decode_identically_to_cdf1() {
    let cdf1 = decode_cfradial1_volume(&synthetic_two_sweep(false)).expect("cdf1");
    let cdf2 = decode_cfradial1_volume(&synthetic_two_sweep(true)).expect("cdf2");
    // Compared through Debug rather than `==`: the moment grids hold NaN fill
    // values, and NaN != NaN would make structural equality fail on two
    // byte-identical decodes. Debug prints NaN as "NaN", so it compares equal.
    assert_eq!(format!("{cdf1:?}"), format!("{cdf2:?}"));
}

#[test]
fn sweeps_survive_a_file_that_omits_the_mandatory_fixed_angle() {
    // Same two-sweep geometry, but with `fixed_angle(sweep)` removed. The
    // ray-index arrays still describe two sweeps, so both must survive with
    // their angles recovered from the ray elevations — not collapse to sweep
    // 0 and drop rays 3-5.
    let bytes = synthetic_two_sweep_without_fixed_angle();
    let volume = decode_cfradial1_volume(&bytes).expect("decode without fixed_angle");
    assert_eq!(volume.cuts.len(), 2, "both sweeps must survive");
    assert_eq!(volume.metadata.decoded_radial_count, 6, "no rays dropped");
    assert_close(
        volume.cuts[0].elevation_deg,
        0.5,
        1e-4,
        "cut0 mean elevation",
    );
    assert_close(
        volume.cuts[1].elevation_deg,
        1.5,
        1e-4,
        "cut1 mean elevation",
    );
    let high = volume.cuts[1]
        .moments
        .get(&MomentType::Reflectivity)
        .expect("REF");
    assert_close(high.scaled_value(0, 0).unwrap(), 13.0, 1e-4, "REF[3,0]");
}

/// The two-sweep fixture with its `fixed_angle` variable left out. Deleting
/// the variable from the built bytes would mean re-patching every following
/// data offset, so the file is declared again minus that one variable.
fn synthetic_two_sweep_without_fixed_angle() -> Vec<u8> {
    let mut dbz = Vec::new();
    for ray in 0..6i16 {
        for gate in 0..4i16 {
            dbz.push(1000 + 100 * ray + 1000 * gate);
        }
    }
    NcBuilder::new(false)
        .gattr_text("instrument_name", "SYNTH")
        .dim("time", 6)
        .dim("range", 4)
        .dim("sweep", 2)
        .dim("string_length", 32)
        .var(
            Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 0.5, 1.0, 2.0, 2.5, 3.0])),
        )
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[125.0, 375.0, 625.0, 875.0])))
        .var(
            Var::new("azimuth", &[0], NcType::Float)
                .data(f32_bytes(&[10.0, 20.0, 30.0, 10.0, 20.0, 30.0])),
        )
        .var(
            Var::new("elevation", &[0], NcType::Float)
                .data(f32_bytes(&[0.5, 0.5, 0.5, 1.5, 1.5, 1.5])),
        )
        .var(Var::new("sweep_start_ray_index", &[2], NcType::Int).data(i32_bytes(&[0, 3])))
        .var(Var::new("sweep_end_ray_index", &[2], NcType::Int).data(i32_bytes(&[2, 5])))
        .var(
            Var::new("sweep_mode", &[2, 3], NcType::Char).data(char_matrix(
                &["azimuth_surveillance", "azimuth_surveillance"],
                32,
            )),
        )
        .var(
            Var::new("DBZ", &[0, 1], NcType::Short)
                .attr_f32("scale_factor", 0.01)
                .attr_i16("_FillValue", -32768)
                .data(i16_bytes(&dbz)),
        )
        .build()
}

#[test]
fn rhi_without_fixed_angle_falls_back_to_the_wrap_aware_azimuth() {
    // An RHI sweeping in elevation at a fixed azimuth that straddles north.
    // The naive arithmetic mean of 359/0/1 is 120; the answer is ~0.
    let bytes = NcBuilder::new(false)
        .gattr_text("instrument_name", "RHI1")
        .dim("time", 3)
        .dim("range", 2)
        .dim("sweep", 1)
        .dim("string_length", 32)
        .var(Var::new("time", &[0], NcType::Double).data(f64_bytes(&[0.0, 1.0, 2.0])))
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[50.0, 150.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&[359.0, 0.0, 1.0])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&[1.0, 20.0, 40.0])))
        .var(Var::new("sweep_start_ray_index", &[2], NcType::Int).data(i32_bytes(&[0])))
        .var(Var::new("sweep_end_ray_index", &[2], NcType::Int).data(i32_bytes(&[2])))
        .var(Var::new("sweep_mode", &[2, 3], NcType::Char).data(char_matrix(&["rhi"], 32)))
        .var(
            Var::new("DBZ", &[0, 1], NcType::Float)
                .data(f32_bytes(&[5.0, 6.0, 7.0, 8.0, 9.0, 10.0])),
        )
        .build();

    let volume = decode_cfradial1_volume(&bytes).expect("decode RHI");
    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    // The fixed angle of an RHI is its AZIMUTH, wrapped around north.
    let fixed = cut.elevation_deg.rem_euclid(360.0);
    assert!(
        !(0.01..=359.99).contains(&fixed),
        "RHI fixed azimuth was {fixed}, expected ~0"
    );
    // The rays keep their true, varying elevations.
    assert_close(cut.radials[0].elevation_deg, 1.0, 1e-6, "el0");
    assert_close(cut.radials[2].elevation_deg, 40.0, 1e-6, "el2");
    assert_close(cut.radials[0].azimuth_deg, 359.0, 1e-6, "az0");
    // Gate centers 50, 150 -> 100 m gates, gate 0 centred at 50 m.
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (50, 100, 2)
    );
}

// ---------------------------------------------------------------------------
// Hostile geometry: what a declared sweep count is allowed to cost.
// ---------------------------------------------------------------------------

/// `rays` rays of two gates each, with a `fixed_angle` array of `sweeps`
/// entries and optional ray-index arrays. `fixed_angle` is what a file
/// declares its sweep count with, and nothing about it costs bytes: it is a
/// dimension length times four.
fn crafted_sweeps(rays: usize, sweeps: usize, indices: Option<(Vec<i32>, Vec<i32>)>) -> Vec<u8> {
    let mut builder = NcBuilder::new(false)
        .gattr_text("instrument_name", "CRAFT")
        .dim("time", rays as u32)
        .dim("range", 2)
        .dim("sweep", sweeps as u32)
        .var(
            Var::new("time", &[0], NcType::Double)
                .data(f64_bytes(&(0..rays).map(|r| r as f64).collect::<Vec<_>>())),
        )
        .var(Var::new("range", &[1], NcType::Float).data(f32_bytes(&[100.0, 300.0])))
        .var(Var::new("azimuth", &[0], NcType::Float).data(f32_bytes(&vec![10.0; rays])))
        .var(Var::new("elevation", &[0], NcType::Float).data(f32_bytes(&vec![0.5; rays])))
        .var(Var::new("fixed_angle", &[2], NcType::Float).data(f32_bytes(
            &(0..sweeps).map(|s| s as f32 * 0.1).collect::<Vec<_>>(),
        )));
    if let Some((starts, ends)) = indices {
        // The index arrays get their own dimension so a file can describe
        // fewer sweeps than `fixed_angle` declares — the shape that makes a
        // decoder invent geometry for the sweeps left over.
        assert_eq!(starts.len(), ends.len());
        builder = builder
            .dim("sweep_index", starts.len() as u32)
            .var(Var::new("sweep_start_ray_index", &[3], NcType::Int).data(i32_bytes(&starts)))
            .var(Var::new("sweep_end_ray_index", &[3], NcType::Int).data(i32_bytes(&ends)));
    }
    builder
        .var(Var::new("DBZ", &[0, 1], NcType::Float).data(f32_bytes(&vec![10.0; rays * 2])))
        .build()
}

#[test]
fn a_declared_sweep_count_cannot_outrun_the_ray_list() {
    // A sweep owns at least one ray, so four rays cannot be four thousand
    // sweeps. Unbounded, each declared sweep reserves a full cut AND a full
    // moment grid per field, so a dimension length — four bytes of header per
    // sweep — multiplies into hundreds of megabytes; the growth is linear in
    // that length, so an ordinary-looking file reaches an allocation failure,
    // which aborts the process instead of unwinding.
    let bytes = crafted_sweeps(4, 200_000, None);
    let volume = decode_cfradial1_volume(&bytes).expect("decode crafted sweeps");
    // `message_count` is the sweep count the decoder committed to, and every
    // one of those sweeps costs a cut, a sweep-mode slot and a moment grid
    // per field before anything checks whether the file has rays for it.
    assert_eq!(volume.metadata.message_count, 4);
    assert_eq!(volume.cuts.len(), 1);
    assert_eq!(volume.metadata.decoded_radial_count, 4);
    // The dropped sweeps are reported rather than silently forgotten.
    assert_eq!(volume.metadata.skipped_message_count, 199_999);

    // With rays to spare, the hard ceiling is what stops it.
    let bytes = crafted_sweeps(5_000, 200_000, None);
    let volume = decode_cfradial1_volume(&bytes).expect("decode crafted sweeps");
    assert_eq!(volume.metadata.message_count, 4096);
    assert_eq!(volume.metadata.decoded_radial_count, 5_000);

    // Same bound through the ray-index arrays, which is the route the older
    // reading of `sweep_count` did not have: 4096 sweeps each claiming all
    // four rays decode as the two the row budget affords, not as 16,384
    // rows, and the rest are counted as dropped.
    let bytes = crafted_sweeps(4, 4096, Some((vec![0; 4096], vec![3; 4096])));
    let volume = decode_cfradial1_volume(&bytes).expect("decode overlapping sweeps");
    assert_eq!(volume.metadata.message_count, 4);
    assert_eq!(volume.cuts.len(), 2);
    assert_eq!(volume.metadata.decoded_radial_count, 8);
    assert_eq!(volume.metadata.skipped_message_count, 4094);
}

#[test]
fn sweeps_that_all_claim_every_ray_are_dropped_not_decoded() {
    // The sweeps partition the ray list, so their lengths cannot sum past the
    // ray count. Without that bound, N sweeps each claiming all N rays is a
    // quadratic decode out of a linear file. The bound is enforced by
    // dropping the sweeps that would exceed it — nothing is reserved for a
    // sweep until it has passed — so the decode stays linear.
    let bytes = crafted_sweeps(64, 64, Some((vec![0; 64], vec![63; 64])));
    let volume = decode_cfradial1_volume(&bytes).expect("decode overlapping sweeps");
    assert_eq!(volume.cuts.len(), 2);
    assert_eq!(volume.metadata.decoded_radial_count, 128);
    assert_eq!(volume.metadata.skipped_message_count, 62);
    assert!(volume.metadata.decoded_radial_count <= 64 + volume.metadata.message_count);
}

#[test]
fn a_sweep_that_overruns_the_row_budget_costs_only_itself() {
    // A writer whose sweeps overlap by MORE than the one ray of slack an
    // exclusive `sweep_end_ray_index` costs is sloppy, not unreadable: four
    // sweeps of a 400-ray file overlapping by two rays at each boundary.
    // Refusing the file outright threw away 400 perfectly good rays; the
    // sweeps that fit the budget are decoded and the one that does not is
    // reported.
    let starts = vec![0, 99, 199, 299];
    let ends = vec![100, 200, 300, 399];
    let bytes = crafted_sweeps(400, 4, Some((starts, ends)));
    let volume = decode_cfradial1_volume(&bytes).expect("decode overlapping partition");
    assert_eq!(volume.cuts.len(), 3);
    assert_eq!(volume.metadata.decoded_radial_count, 305);
    assert_eq!(volume.metadata.skipped_message_count, 1);
}

#[test]
fn exclusive_end_ray_indices_still_decode() {
    // A writer that emits EXCLUSIVE `sweep_end_ray_index` overlaps its sweeps
    // by one ray at each boundary. That is inside the tolerance, so such a
    // file still decodes rather than being refused as a non-partition.
    let bytes = crafted_sweeps(6, 2, Some((vec![0, 3], vec![3, 6])));
    let volume = decode_cfradial1_volume(&bytes).expect("decode exclusive ends");
    assert_eq!(volume.cuts.len(), 2);
    assert_eq!(volume.metadata.decoded_radial_count, 7);
}

#[test]
fn sweeps_the_file_never_bounded_are_dropped_not_duplicated() {
    // `fixed_angle` says two sweeps; the ray-index arrays describe one. The
    // second sweep was never bounded, so inventing "every ray" for it
    // duplicates the whole volume — 2 cuts x 4 rays out of a 4-ray file —
    // and gives the caller no hint that the geometry was made up.
    let bytes = crafted_sweeps(4, 2, Some((vec![0], vec![3])));
    let volume = decode_cfradial1_volume(&bytes).expect("decode short index arrays");
    assert_eq!(volume.cuts.len(), 1);
    assert_eq!(volume.metadata.decoded_radial_count, 4);
    assert_eq!(volume.metadata.skipped_message_count, 1);

    // With no ray-index arrays at all, sweep 0 is still the whole volume —
    // that is the one file shape where the default is the only reading —
    // and the later declared sweeps are dropped rather than duplicated.
    let bytes = crafted_sweeps(4, 3, None);
    let volume = decode_cfradial1_volume(&bytes).expect("decode without index arrays");
    assert_eq!(volume.cuts.len(), 1);
    assert_eq!(volume.metadata.decoded_radial_count, 4);
    assert_eq!(volume.metadata.skipped_message_count, 2);
}

#[test]
fn a_record_count_lie_is_bounded_by_the_array_ceiling() {
    // Worth stating outright, because it is the ceiling a dropped file can
    // reach: a classic netCDF header describes its arrays in COUNTS, not in
    // bytes the file must actually contain, so a small file can honestly ask
    // for a large read. `numrecs` is the multiplier on every record
    // variable's slab and may claim up to 100,000,000 records — a 540-byte
    // file with a 2-byte-per-record variable and that count asks for ~191
    // MiB, a ~370,000x amplification. `MAX_NC_ARRAY_BYTES` (256 MB, in
    // netcdf3.rs) is the whole of the bound: above it the request is refused
    // before anything is reserved, and below it the reservation goes through
    // `try_reserve` — an `Err`, never an abort — and the read then fails at
    // the first byte the file does not contain.
    //
    // Every record variable in this fixture is 4 bytes or more per record,
    // so 100,000,000 records lands above the ceiling and the read never
    // starts.
    let mut damaged = XSAPR_PPI.to_vec();
    damaged[4..8].copy_from_slice(&100_000_000u32.to_be_bytes());
    let err = decode_cfradial1_volume(&damaged).expect_err("record count lie");
    assert!(err.to_string().contains("limit"), "unhelpful error: {err}");
}

#[test]
fn mutated_real_bytes_never_panic() {
    // The bounds above only hold if the decoder reaches them, so the same
    // real file is fed back in damaged: truncated at every 4-byte boundary
    // (netCDF is a 4-byte-aligned format, so these are the cuts that leave a
    // plausible header behind) and with single bytes rewritten across the
    // header, where the counts and offsets live. Every case must come back
    // as Ok or Err, never as a panic or an allocation failure — a dropped
    // file is untrusted input.
    for cut in (4..XSAPR_PPI.len()).step_by(4) {
        let _ = decode_cfradial1_volume(&XSAPR_PPI[..cut]);
    }
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut damaged = XSAPR_PPI.to_vec();
    for _ in 0..4_000 {
        // xorshift64*, so the case list is fixed rather than flaky.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let roll = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        // The first 4 KB covers the whole header of this fixture: magic,
        // numrecs, every dimension length, and every variable's offset.
        let at = (roll >> 32) as usize % 4096.min(damaged.len());
        let byte = (roll >> 16) as u8;
        let original = damaged[at];
        damaged[at] = byte;
        let volume = decode_cfradial1_volume(&damaged);
        if let Ok(volume) = volume {
            // Whatever a damaged header claims, the decode stays bounded by
            // the rays the file could actually carry.
            assert!(volume.cuts.len() <= volume.metadata.decoded_radial_count.max(1));
            assert!(volume.metadata.decoded_radial_count <= 2 * XSAPR_PPI.len());
        }
        damaged[at] = original;
    }
}

// ---------------------------------------------------------------------------
// A minimal classic-netCDF writer, used only to build the fixtures above.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum NcType {
    Char = 2,
    Short = 3,
    Int = 4,
    Float = 5,
    Double = 6,
}

impl NcType {
    fn size(self) -> usize {
        match self {
            Self::Char => 1,
            Self::Short => 2,
            Self::Int | Self::Float => 4,
            Self::Double => 8,
        }
    }
}

enum Attr {
    Text(String),
    F32(f32),
    I16(i16),
}

struct Var {
    name: String,
    dim_ids: Vec<usize>,
    nc_type: NcType,
    attrs: Vec<(String, Attr)>,
    data: Vec<u8>,
}

impl Var {
    fn new(name: &str, dim_ids: &[usize], nc_type: NcType) -> Self {
        Self {
            name: name.to_owned(),
            dim_ids: dim_ids.to_vec(),
            nc_type,
            attrs: Vec::new(),
            data: Vec::new(),
        }
    }

    fn attr_f32(mut self, name: &str, value: f32) -> Self {
        self.attrs.push((name.to_owned(), Attr::F32(value)));
        self
    }

    fn attr_text(mut self, name: &str, value: &str) -> Self {
        self.attrs
            .push((name.to_owned(), Attr::Text(value.to_owned())));
        self
    }

    fn attr_i16(mut self, name: &str, value: i16) -> Self {
        self.attrs.push((name.to_owned(), Attr::I16(value)));
        self
    }

    fn data(mut self, data: Vec<u8>) -> Self {
        self.data = data;
        self
    }
}

struct NcBuilder {
    offset64: bool,
    dims: Vec<(String, u32)>,
    gattrs: Vec<(String, Attr)>,
    vars: Vec<Var>,
}

impl NcBuilder {
    fn new(offset64: bool) -> Self {
        Self {
            offset64,
            dims: Vec::new(),
            gattrs: Vec::new(),
            vars: Vec::new(),
        }
    }

    fn dim(mut self, name: &str, len: u32) -> Self {
        self.dims.push((name.to_owned(), len));
        self
    }

    fn gattr_text(mut self, name: &str, value: &str) -> Self {
        self.gattrs
            .push((name.to_owned(), Attr::Text(value.to_owned())));
        self
    }

    fn var(mut self, var: Var) -> Self {
        self.vars.push(var);
        self
    }

    fn build(self) -> Vec<u8> {
        // Offsets are fixed-width, so a first pass with placeholder offsets
        // measures a header of exactly the final length.
        let header_len = self.header(&vec![0u64; self.vars.len()]).len();
        let mut begins = Vec::with_capacity(self.vars.len());
        let mut at = header_len as u64;
        for var in &self.vars {
            begins.push(at);
            at += pad4(var.data.len()) as u64;
        }

        let mut bytes = self.header(&begins);
        assert_eq!(bytes.len(), header_len, "header length must be stable");
        for var in &self.vars {
            bytes.extend_from_slice(&var.data);
            bytes.resize(pad4(bytes.len()), 0);
        }
        bytes
    }

    fn header(&self, begins: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(b"CDF");
        out.push(if self.offset64 { 2 } else { 1 });
        put_u32(&mut out, 0); // numrecs: every fixture is fixed-size

        put_list_tag(&mut out, 0x0A, self.dims.len());
        for (name, len) in &self.dims {
            put_name(&mut out, name);
            put_u32(&mut out, *len);
        }

        put_attrs(&mut out, &self.gattrs);

        put_list_tag(&mut out, 0x0B, self.vars.len());
        for (index, var) in self.vars.iter().enumerate() {
            put_name(&mut out, &var.name);
            put_u32(&mut out, var.dim_ids.len() as u32);
            for dim_id in &var.dim_ids {
                put_u32(&mut out, *dim_id as u32);
            }
            put_attrs(&mut out, &var.attrs);
            put_u32(&mut out, var.nc_type as u32);
            put_u32(
                &mut out,
                pad4(var.data.len().max(var.nc_type.size())) as u32,
            );
            if self.offset64 {
                out.extend(begins[index].to_be_bytes());
            } else {
                put_u32(&mut out, begins[index] as u32);
            }
        }
        out
    }
}

fn pad4(len: usize) -> usize {
    len.div_ceil(4) * 4
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_be_bytes());
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    put_u32(out, name.len() as u32);
    out.extend(name.as_bytes());
    out.resize(pad4(out.len()), 0);
}

fn put_list_tag(out: &mut Vec<u8>, tag: u32, count: usize) {
    if count == 0 {
        put_u32(out, 0); // ABSENT
        put_u32(out, 0);
    } else {
        put_u32(out, tag);
        put_u32(out, count as u32);
    }
}

fn put_attrs(out: &mut Vec<u8>, attrs: &[(String, Attr)]) {
    put_list_tag(out, 0x0C, attrs.len());
    for (name, value) in attrs {
        put_name(out, name);
        match value {
            Attr::Text(text) => {
                put_u32(out, NcType::Char as u32);
                put_u32(out, text.len() as u32);
                out.extend(text.as_bytes());
            }
            Attr::F32(number) => {
                put_u32(out, NcType::Float as u32);
                put_u32(out, 1);
                out.extend(number.to_be_bytes());
            }
            Attr::I16(number) => {
                put_u32(out, NcType::Short as u32);
                put_u32(out, 1);
                out.extend(number.to_be_bytes());
            }
        }
        out.resize(pad4(out.len()), 0);
    }
}

/// The SAME volume in both netCDF containers decodes to the same volume.
///
/// `cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc` is Py-ART's published file
/// exactly as ARM-DOE ships it — a netCDF-4 (HDF5) container. The `.classic`
/// fixture beside it is that file copied variable-for-variable into a
/// CDF-1 container with no masking and no scaling applied, so the two hold
/// identical values (confirmed with netCDF4-python: every variable compares
/// equal). Anything that differs between the two decodes is therefore a
/// container-reading bug and nothing else.
///
/// Before netCDF-4 support existed this file did not open AT ALL:
/// [`nexrad_io::hdf5lite`] refused its version 2 superblock, and the router
/// sent every HDF5 file to the ODIM decoder, which answered "not ODIM_H5".
/// The README advertised CfRadial 1.x while the dominant CfRadial 1
/// container silently failed.
#[test]
fn the_netcdf4_and_classic_containers_of_one_volume_decode_identically() {
    let netcdf4 = decode_cfradial1_volume(XSAPR_PPI_NETCDF4).expect("decode netCDF-4 X-SAPR PPI");
    let classic = decode_cfradial1_volume(XSAPR_PPI).expect("decode classic X-SAPR PPI");

    assert!(
        XSAPR_PPI_NETCDF4.starts_with(b"\x89HDF\r\n\x1a\n"),
        "the netCDF-4 fixture should be an HDF5 container"
    );
    assert!(
        !looks_like_netcdf3_bytes(XSAPR_PPI_NETCDF4),
        "a netCDF-4 file carries no CDF magic, which is why it needs its own reader"
    );

    assert_same_volume(&netcdf4, &classic);
}

/// Every part of one volume, compared across the two containers it was
/// written into: geometry, timing and every gate of every moment.
///
/// This is a helper rather than one test's body because the claim it makes
/// — the container is not supposed to be observable — has to hold for more
/// than one volume. A volume whose gates were all WRITTEN cannot catch a
/// reader that invents values for the ones that were not.
#[track_caller]
fn assert_same_volume(netcdf4: &radar_core::RadarVolume, classic: &radar_core::RadarVolume) {
    assert_eq!(netcdf4.site.id, classic.site.id);
    assert_eq!(netcdf4.site.name, classic.site.name);
    assert_eq!(netcdf4.site.latitude_deg, classic.site.latitude_deg);
    assert_eq!(netcdf4.site.longitude_deg, classic.site.longitude_deg);
    assert_eq!(netcdf4.site.elevation_m, classic.site.elevation_m);
    assert_eq!(netcdf4.volume_time, classic.volume_time);
    assert_eq!(
        netcdf4.metadata.decoded_radial_count,
        classic.metadata.decoded_radial_count
    );
    assert_eq!(netcdf4.cuts.len(), classic.cuts.len());

    for (cut_index, (left, right)) in netcdf4.cuts.iter().zip(&classic.cuts).enumerate() {
        assert_eq!(left.elevation_deg, right.elevation_deg, "cut {cut_index}");
        assert_eq!(left.radials.len(), right.radials.len(), "cut {cut_index}");
        for (ray, (a, b)) in left.radials.iter().zip(&right.radials).enumerate() {
            assert_eq!(a.azimuth_deg, b.azimuth_deg, "cut {cut_index} ray {ray} az");
            assert_eq!(
                a.elevation_deg, b.elevation_deg,
                "cut {cut_index} ray {ray} el"
            );
            assert_eq!(
                a.time_offset_ms, b.time_offset_ms,
                "cut {cut_index} ray {ray} t"
            );
            assert_eq!(
                a.gate_range, b.gate_range,
                "cut {cut_index} ray {ray} gates"
            );
            assert_eq!(
                a.nyquist_velocity_mps, b.nyquist_velocity_mps,
                "cut {cut_index} ray {ray} nyquist"
            );
        }
        assert_eq!(
            left.moments.keys().collect::<Vec<_>>(),
            right.moments.keys().collect::<Vec<_>>(),
            "cut {cut_index} moments"
        );
        for (moment, grid) in &left.moments {
            let other = &right.moments[moment];
            let (MomentStorage::F32(mine), MomentStorage::F32(theirs)) =
                (&grid.storage, &other.storage)
            else {
                panic!("CfRadial fields decode to f32 storage");
            };
            assert_eq!(
                mine.len(),
                theirs.len(),
                "cut {cut_index} {moment:?} length"
            );
            // NaN is the fill value on both sides, so compare bit patterns
            // rather than values: `==` would call every masked gate unequal.
            for (gate, (a, b)) in mine.iter().zip(theirs).enumerate() {
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "cut {cut_index} {moment:?} gate {gate}: {a} != {b}"
                );
            }
        }
    }
}

/// Storage the writer never allocated reads back as the field's own
/// `_FillValue`, not as zero.
///
/// HDF5 writes nothing for a variable that was never written — a contiguous
/// dataset gets the undefined data address, a chunked one gets no chunk
/// records — and the value those regions stand for is the dataset's Fill
/// Value message, which is where netCDF puts `_FillValue`. A reader that
/// conjures zeros instead hands the display 0.0 dBZ over ground the file
/// says was never measured: not a gap, but weak echo, drawn in the same
/// colours as real returns. That is the failure this pins.
///
/// Every number below came from netCDF4-python 1.7.4 reading the same file:
/// 0 of 48 reflectivity gates written, 0 of 48 velocity, and exactly the
/// first four rays of `spectrum_width`, running 0.5 to 12.0 m/s.
#[test]
fn unwritten_netcdf4_storage_reads_as_fill_not_as_zero() {
    let volume = decode_cfradial1_volume(UNWRITTEN_NETCDF4).expect("decode netCDF-4 fill fixture");
    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_eq!(cut.radials.len(), UNWRITTEN_RAYS);

    // Contiguous, undefined data address.
    let reflectivity = cut
        .moments
        .get(&MomentType::Reflectivity)
        .expect("reflectivity is declared");
    // Chunked, no chunk records at all.
    let velocity = cut
        .moments
        .get(&MomentType::Velocity)
        .expect("velocity is declared");
    for ray in 0..UNWRITTEN_RAYS {
        for gate in 0..UNWRITTEN_GATES {
            for (name, moment) in [("reflectivity", reflectivity), ("velocity", velocity)] {
                let value = moment.scaled_value(ray, gate).expect("gate in range");
                assert!(
                    value.is_nan(),
                    "{name}[{ray},{gate}] was never written, so it must be no-data, not {value}"
                );
            }
        }
    }

    // Chunked and HALF written: the allocated chunk keeps its values and the
    // absent one must not be invented.
    let width = cut
        .moments
        .get(&MomentType::SpectrumWidth)
        .expect("spectrum_width is declared");
    for ray in 0..UNWRITTEN_RAYS {
        for gate in 0..UNWRITTEN_GATES {
            let value = width.scaled_value(ray, gate).expect("gate in range");
            if ray < UNWRITTEN_WRITTEN_RAYS {
                let expected = (ray * UNWRITTEN_GATES + gate + 1) as f32 * 0.5;
                assert_close(value, expected, 1e-6, &format!("width[{ray},{gate}]"));
            } else {
                assert!(
                    value.is_nan(),
                    "width[{ray},{gate}] is in the chunk the writer never allocated, not {value}"
                );
            }
        }
    }
}

/// A user-defined type declared beside the variables costs the file nothing.
///
/// netCDF stores every user-defined type — enum, compound, vlen — as an HDF5
/// committed (named) datatype in the group next to the variables. It carries
/// a datatype message and no dataspace, so a reader that calls "has a
/// datatype" a dataset asks it for a shape it does not have and fails the
/// WHOLE file: `dataset '/gate_class_t' has no dataspace`, on a file whose
/// every moment is a plain float array it could read. Skipping it is the
/// only thing the decoder needs to do about it.
#[test]
fn a_committed_datatype_beside_the_variables_is_skipped_not_read() {
    let file = nexrad_io::hdf5lite::H5File::open(UNWRITTEN_NETCDF4).expect("open the fixture");
    assert!(
        file.is_committed_datatype("/gate_class_t"),
        "the fixture must actually carry the committed type this pins"
    );
    assert!(!file.is_dataset("/gate_class_t"));
    assert!(file.is_dataset("/reflectivity"), "a real variable still is");

    let volume = decode_cfradial1_volume(UNWRITTEN_NETCDF4).expect("decode past the type");
    assert_eq!(volume.cuts.len(), 1);
    assert_eq!(volume.cuts[0].moments.len(), 3, "all three fields survive");
}

/// The container-equivalence claim, made on a volume that can break it.
///
/// [`the_netcdf4_and_classic_containers_of_one_volume_decode_identically`]
/// compares a volume whose every gate was written, which no fill-value bug
/// can disturb. This one compares the volume that was mostly NOT written,
/// where classic netCDF stores `_FillValue` for every unwritten gate and
/// netCDF-4 stores nothing at all — so the two containers agree only if the
/// HDF5 reader knows what "nothing" means.
#[test]
fn the_two_containers_of_an_unwritten_volume_decode_identically() {
    let netcdf4 = decode_cfradial1_volume(UNWRITTEN_NETCDF4).expect("decode netCDF-4 fill fixture");
    let classic = decode_cfradial1_volume(UNWRITTEN_CLASSIC).expect("decode classic fill fixture");
    assert!(UNWRITTEN_NETCDF4.starts_with(b"\x89HDF\r\n\x1a\n"));
    assert!(looks_like_netcdf3_bytes(UNWRITTEN_CLASSIC));
    assert_same_volume(&netcdf4, &classic);
}

/// The top-level router opens a netCDF-4 CfRadial file.
///
/// The HDF5 signature says "HDF5", not "ODIM_H5", and the router used to
/// read it as the latter — so a CfRadial file dropped on the app came back
/// as "ODIM_H5: HDF5 file has no /what 'object' attribute". This is the pin
/// that the router looks INSIDE an HDF5 container before deciding what it
/// is, and that the ODIM path still gets ODIM files.
#[test]
fn the_router_opens_a_netcdf4_cfradial_file_as_cfradial() {
    assert_eq!(
        nexrad_io::sniff_supported_volume_bytes(XSAPR_PPI_NETCDF4),
        Some(nexrad_io::SupportedVolumeFormat::OdimH5),
        "the head bytes can only say 'HDF5 container'"
    );
    let volume =
        nexrad_io::decode_supported_volume_bytes(XSAPR_PPI_NETCDF4).expect("router decodes");
    assert_eq!(volume.site.id, "xsapr-sgp");
    assert_eq!(volume.cuts.len(), 1);
    assert_eq!(volume.cuts[0].radials.len(), 40);
    assert!(
        volume.cuts[0]
            .moments
            .contains_key(&MomentType::Reflectivity)
    );

    // The other HDF5 radar format still routes to its own decoder.
    let odim = nexrad_io::decode_supported_volume_bytes(ODIM_PVOL).expect("ODIM still decodes");
    assert!(
        !odim.cuts.is_empty(),
        "an ODIM volume must not be mistaken for netCDF-4"
    );
}

/// An attribute this reader cannot decode does not take its neighbours with
/// it.
///
/// netCDF-4 hangs its own bookkeeping off HDF5 attributes with datatypes
/// outside a minimal parser's scope: `DIMENSION_LIST` is a variable-length
/// sequence of object references and `REFERENCE_LIST` is a compound. They
/// sit in the SAME attribute list as `scale_factor` and `add_offset`, so an
/// enumeration that gave up at the first one it could not convert would
/// silently return an unpacked field — right-shaped, wrong values, no error
/// anywhere. This is the pin that says the skip is per attribute.
#[test]
fn an_undecodable_attribute_does_not_hide_the_cf_packing_beside_it() {
    let file = nexrad_io::hdf5lite::H5File::open(XSAPR_PPI_NETCDF4).expect("open");
    let field = "/reflectivity_horizontal";
    let names = file.attr_names(field);
    assert!(
        names.iter().any(|name| name == "DIMENSION_LIST"),
        "the fixture should carry the bookkeeping attribute this guards, got {names:?}"
    );
    let decoded = file.attrs(field);
    assert!(
        !decoded.iter().any(|(name, _)| name == "DIMENSION_LIST"),
        "an undecodable datatype should be skipped, not invented"
    );
    for wanted in ["_FillValue", "units", "standard_name"] {
        assert!(
            decoded.iter().any(|(name, _)| name == wanted),
            "'{wanted}' must survive the attribute this reader skips, got {:?}",
            decoded.iter().map(|(name, _)| name).collect::<Vec<_>>()
        );
    }
}

/// A netCDF-4 group small enough to keep its links COMPACT still opens.
///
/// HDF5 stores a group's links two ways and netCDF-4 uses both: compact link
/// messages inside the object header while the group stays under its
/// max-compact threshold (eight links), dense fractal-heap storage past it.
/// Every published CfRadial file is dense — dozens of variables — so the
/// real fixture cannot reach the compact branch, and a reader that only ever
/// saw dense storage would report a small netCDF-4 file as having no
/// variables at all: no error, just an empty root.
///
/// The fixture is written by the netCDF-C library itself (see
/// `tests/data/gen_cfradial_nc4_compact.py`), so this is that library's own
/// compact layout and not a guess at it. The expected values come from the
/// generator's declared packing — `physical = raw * 0.5 + 1.0` with `-9999`
/// masked — so a packing slip fails here as an arithmetic error, not as a
/// container one.
#[test]
fn a_netcdf4_group_with_compact_link_storage_decodes() {
    let volume = decode_cfradial1_volume(TINY_COMPACT_LINKS).expect("decode compact-link netCDF-4");

    assert_eq!(volume.site.id, "TINY");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()
    );
    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.5, 1e-6, "fixed angle");
    assert_eq!(cut.radials.len(), 3);
    assert_close(cut.radials[0].azimuth_deg, 10.0, 1e-6, "az0");
    assert_close(cut.radials[2].azimuth_deg, 30.0, 1e-6, "az2");
    assert_eq!(
        cut.radials[0].gate_range,
        GateRange {
            first_gate_m: 100,
            gate_spacing_m: 100,
            gate_count: 4,
        }
    );

    let MomentStorage::F32(values) = &cut.moments[&MomentType::Reflectivity].storage else {
        panic!("CfRadial fields decode to f32 storage");
    };
    // Three rays of four gates: raw * 0.5 + 1.0, with the `-9999` gate of
    // ray 1 masked by `_FillValue`.
    let expected = [
        [1.0, 2.0, 3.0, 4.0],
        [5.0, 6.0, f32::NAN, 8.0],
        [9.0, 10.0, 11.0, 12.0],
    ]
    .concat();
    assert_eq!(values.len(), expected.len());
    for (gate, (actual, want)) in values.iter().zip(expected).enumerate() {
        if want.is_nan() {
            assert!(actual.is_nan(), "gate {gate} should be the fill value");
        } else {
            assert_close(*actual, want, 1e-6, &format!("gate {gate}"));
        }
    }
}

/// A grouped netCDF-4 radar file is named, not reported as a broken one.
///
/// CfRadial 2 puts each sweep in its own group. Reading its root would find
/// no `(time, range)` field and complain about a missing dimension, which
/// sends the reader looking for a corrupt file rather than for
/// `RadxConvert`. Any grouped HDF5 file exercises the same guard, so the
/// ODIM fixture — `/what`, `/where`, `/datasetN` — stands in for one.
#[test]
fn a_grouped_netcdf4_file_is_named_as_cfradial2_rather_than_misread() {
    let file = nexrad_io::hdf5lite::H5File::open(ODIM_PVOL).expect("ODIM opens as HDF5");
    let error = nexrad_io::netcdf4::Nc4File::from_hdf5(file)
        .err()
        .expect("a grouped file is not CfRadial 1");
    let message = error.to_string();
    assert!(
        message.contains("CfRadial 2"),
        "the message should name the convention, got {message}"
    );
    assert!(
        message.contains("RadxConvert"),
        "the message should name the way out, got {message}"
    );
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn f64_bytes(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn i32_bytes(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn i16_bytes(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// `char(rows, width)` matrix, NUL-padded — how CfRadial stores per-sweep
/// strings such as `sweep_mode`.
fn char_matrix(rows: &[&str], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; rows.len() * width];
    for (row, text) in rows.iter().enumerate() {
        let bytes = text.as_bytes();
        let len = bytes.len().min(width);
        out[row * width..row * width + len].copy_from_slice(&bytes[..len]);
    }
    out
}
