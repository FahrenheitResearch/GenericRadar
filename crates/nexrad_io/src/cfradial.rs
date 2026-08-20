//! CfRadial 1.x decoder (classic-netCDF radar moments).
//!
//! Format reference: M. Dixon and W.-C. Lee, "CfRadial Data File Format —
//! CF-compliant netCDF Format for Moments Data for RADAR and LIDAR",
//! NCAR/EOL, version 1.4 (2016-08-01) (versions 1.1–1.4 share the layout
//! read here; 1.5/1.6 add attributes this decoder simply ignores).
//! CfRadial 1 files are classic netCDF (`CDF\x01`/`CDF\x02`) with:
//! - dimensions `time` (rays, usually the unlimited dimension) and `range`
//!   (gates),
//! - per-ray `azimuth(time)`, `elevation(time)`, `time(time)` and an
//!   optional `nyquist_velocity(time)`,
//! - per-sweep `fixed_angle(sweep)`, `sweep_start_ray_index(sweep)`,
//!   `sweep_end_ray_index(sweep)`, `sweep_mode(sweep, string_length)`,
//! - `latitude`/`longitude`/`altitude` — scalar for a fixed site, `(time)`
//!   arrays on a mobile platform — plus `time_coverage_start`,
//! - field variables dimensioned `(time, range)`, optionally packed with
//!   `scale_factor`/`add_offset` and flagged with `_FillValue`
//!   (CF packing: physical = raw * scale_factor + add_offset).
//!
//! CfRadial 2 is netCDF-4, i.e. an HDF5 container, and never reaches this
//! module: it carries the HDF5 signature rather than the `CDF` magic, so
//! [`looks_like_netcdf3_bytes`] rejects it and the routing layer sends it
//! elsewhere. The same is true of CfRadial *1* files written into a
//! netCDF-4 container, which is the common case in the wild today;
//! `nccopy -k classic` converts one for this decoder.
//!
//! Fields decode into F32 moment grids, where NaN is the fill value, because
//! CfRadial's own packing is already `scale_factor`/`add_offset` in the
//! opposite sense to [`radar_core::MomentGrid`]'s NEXRAD-style
//! `(raw - offset) / scale`, and because a file may pack one field as a
//! short and leave the next one a float. Applying CF packing at decode time
//! keeps one representation for every field regardless of how it was stored.
//!
//! Sweeps become elevation cuts. For an RHI sweep the CfRadial fixed angle
//! is the AZIMUTH, and it lands in `ElevationCut::elevation_deg` — the cut's
//! per-radial `elevation_deg` values still carry the true elevations, so an
//! RHI reads as "one cut at a fixed azimuth" rather than being flattened
//! onto a mean elevation.
//!
//! CAVEAT for anything consuming these volumes: there is NO in-band marker
//! that a volume is an RHI. `sweep_mode` is read (§5.8) but this model has no
//! field to carry a scan mode, so a 37.3-degree RHI and a 37.3-degree tilt
//! are indistinguishable downstream, and the cut sort below — by
//! `elevation_deg`, which for a multi-RHI volume means by AZIMUTH — silently
//! reorders such a volume into azimuth order. Beam-height maths and tilt
//! lists will both read an RHI's azimuth as an elevation. Giving RHIs a real
//! signal means adding a scan mode to the shared model, which is a decision
//! for the layer that owns it rather than for one decoder.

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentRow, MomentStorage, MomentType, RadarSite,
    RadarVolume, Radial, VcpInfo,
};

pub use crate::netcdf3::looks_like_netcdf3_bytes;
use crate::netcdf3::{Nc3File, NcArray, NcVar};
use crate::{NexradError, Result};

/// Hard ceiling on how many sweeps one volume may declare.
///
/// A sweep cannot be shorter than a ray, so the ray count is the real bound
/// (see [`decode_cfradial1_volume`]); this is the ceiling that keeps the
/// per-sweep bookkeeping bounded when the ray count itself is large. Real
/// volumes are two orders of magnitude below it: 9 sweeps for an NCAR SPOL
/// surveillance volume, 14 for a WSR-88D VCP, 31 for an ARM Ka-SACR raster.
const MAX_SWEEPS: usize = 4096;

/// Decode a CfRadial 1.x byte buffer into the shared radar model.
pub fn decode_cfradial1_volume(bytes: &[u8]) -> Result<RadarVolume> {
    let file = Nc3File::open(bytes)?;
    let dim = |name: &str| file.dims.iter().position(|(dim_name, _)| dim_name == name);
    let (Some(time_dim), Some(range_dim)) = (dim("time"), dim("range")) else {
        return Err(invalid(
            "netCDF file lacks time/range dimensions — not CfRadial 1.x",
        ));
    };
    let n_rays = file.dims[time_dim].1;
    let n_gates = file.dims[range_dim].1;
    if n_rays == 0 || n_gates == 0 {
        return Err(invalid("CfRadial volume has no rays or gates"));
    }

    let azimuth = read_f64s(&file, "azimuth")?;
    let elevation = read_f64s(&file, "elevation")?;
    if azimuth.len() < n_rays || elevation.len() < n_rays {
        return Err(invalid("azimuth/elevation shorter than the time dimension"));
    }
    let nyquist = read_f64s(&file, "nyquist_velocity").ok();

    // Gate geometry: range(range) holds gate CENTERS in metres (spec §5.5,
    // "Range to center of each bin"). The `meters_to_center_of_first_gate` /
    // `meters_between_gates` attributes are optional — and stale on files
    // that were decimated after they were written — so the geometry comes
    // from the coordinate values themselves, which are always present and
    // are what the attributes are derived from.
    //
    // [`GateRange::first_gate_m`] in this crate's model is likewise the range
    // to the CENTER of gate 0, not to its near edge: the NEXRAD path stores
    // ICD 2620002's "range to center of first range gate" into this field
    // unchanged, and every consumer reads it that way — the raster resolves a
    // gate as `round((range_m - first_gate_m) / gate_spacing_m)` and the
    // derived-product sampler takes the last gate's center to be
    // `first_gate_m + (gate_count - 1) * gate_spacing_m`. So `range[0]` goes
    // in as it stands. Subtracting half a gate here would place every gate
    // half a gate too close to the radar, and because that lookup rounds
    // half away from zero, a pixel at a gate's true center would read the
    // NEXT gate out.
    let range = read_f64s(&file, "range")?;
    if range.len() < 2 {
        return Err(invalid("range coordinate needs at least two gates"));
    }
    let spacing = (range[1] - range[0]).round().max(1.0);
    let gate_range = GateRange {
        first_gate_m: range[0].round() as i32,
        gate_spacing_m: spacing as i32,
        gate_count: n_gates,
    };

    // Sweep index ranges; a missing sweep dimension means one sweep.
    let fixed_angles = read_f64s(&file, "fixed_angle").unwrap_or_default();
    let sweep_starts = read_f64s(&file, "sweep_start_ray_index").unwrap_or_default();
    let sweep_ends = read_f64s(&file, "sweep_end_ray_index").unwrap_or_default();
    // `fixed_angle(sweep)` is mandatory in the spec, so on a conformant file
    // it alone sets the sweep count. The ray-index arrays are consulted too so
    // that a file which omits `fixed_angle` still splits into its real sweeps
    // (with the angle recovered by `fallback_fixed_angle`) instead of
    // collapsing to sweep 0 and silently dropping every later sweep's rays.
    // Both index arrays must cover a sweep for it to count — a lone
    // `sweep_start_ray_index` cannot bound one.
    let indexed_sweeps = sweep_starts.len().min(sweep_ends.len());
    let declared_sweeps = fixed_angles.len().max(indexed_sweeps).max(1);
    // The sweeps PARTITION the ray list — `sweep_start_ray_index` and
    // `sweep_end_ray_index` give each sweep one contiguous, non-overlapping
    // run of rays — so a volume can hold no more sweeps than it holds rays,
    // every sweep needing a ray of its own. Nothing else ties the sweep count
    // to anything that costs bytes: `sweep` is just a dimension length, and
    // every declared sweep goes on to reserve a full ElevationCut and one
    // MomentGrid per field. Without this clamp a few hundred kilobytes of
    // crafted header expand into gigabytes — measured at ~1,900x — and a
    // failed allocation aborts the process rather than unwinding, so it is
    // process death from a dropped file. `MAX_SWEEPS` is the second half of
    // the belt: it keeps the per-sweep bookkeeping bounded on a volume that
    // legitimately holds hundreds of thousands of rays. Neither bound can
    // touch a conformant file — the deepest real volume checked here has 31
    // sweeps against 6,646 rays.
    let sweep_count = declared_sweeps.min(n_rays).min(MAX_SWEEPS);
    let sweep_modes = read_sweep_modes(&file, sweep_count);

    // `time_coverage_start` is mandatory (§4.4) and is the volume time on
    // every conformant file. A file that omits it falls back to the epoch
    // named by `time(time)`'s own `units` attribute rather than to the Unix
    // epoch: the ray offsets below are measured FROM `volume_time`, so a
    // volume time decades away from the ray epoch turns every offset into
    // billions of milliseconds, which saturates the i32 the model carries
    // and reports every ray at the same clamped instant. Falling back to the
    // units epoch keeps the ray offsets exactly the raw `time` values —
    // which is what the file's own coordinate says — and leaves the absolute
    // instant as close as a file without `time_coverage_start` allows.
    let time_epoch = time_units_epoch(&file);
    let volume_time = parse_time_coverage_start(&file)
        .or(time_epoch)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let mut volume = RadarVolume::new(parse_site(&file), volume_time);
    volume.metadata.archive_version = Some(
        file.gattr_str("version")
            .map(str::to_owned)
            .unwrap_or_else(|| "CfRadial-1".to_owned()),
    );
    volume.metadata.compression = Some("cfradial1-netcdf3".to_owned());
    volume.vcp = file
        .gattr_f64("vcp_pattern")
        .filter(|value| value.is_finite() && value.fract() == 0.0)
        .and_then(|value| u16::try_from(value as i64).ok())
        .filter(|pattern| *pattern > 0)
        .map(|pattern| VcpInfo { pattern });

    // Ray times. `time(time)` is an offset from the epoch named in its OWN
    // `units` attribute — CF's "seconds since <ISO 8601 datetime>" — which is
    // not required to be `time_coverage_start`, and routinely is not:
    // `time_reference` is the volume start while `time_coverage_start` can be
    // the first ray of this sweep (Radx writes one sweep per file), and ARM's
    // X-SAPR sets them 8 s apart. `time_offset_ms` in this model is measured
    // from `volume_time`, i.e. from `time_coverage_start`, so the difference
    // between the two epochs is folded in here. Files that omit `units`, or
    // write something unparseable, keep the old reading — offsets straight
    // from `time_coverage_start` — which is also exactly what a conformant
    // file where the two epochs agree produces.
    let ray_seconds = read_f64s(&file, "time").ok();
    let time_epoch_shift_ms = time_epoch
        .map(|epoch| (epoch - volume_time).num_milliseconds() as f64)
        .unwrap_or(0.0);

    // Field variables: anything shaped (time, range).
    let fields: Vec<&NcVar> = file
        .vars
        .values()
        .filter(|var| var.dim_ids.as_slice() == [time_dim, range_dim])
        .collect();
    if fields.is_empty() {
        return Err(invalid("CfRadial volume has no (time, range) fields"));
    }

    // Build sweep geometry first, then read each full (time, range) field
    // once and distribute its rows across every sweep. Reading per sweep
    // instead would reread and reconvert the whole field once per sweep.
    let mut sweeps = Vec::with_capacity(sweep_count);
    // Sweeps the clamp above dropped are reported, not silently forgotten.
    volume.metadata.skipped_message_count += declared_sweeps - sweep_count;
    // A partition cannot cover a ray twice, so the decoded rows cannot run
    // past the ray count — with one ray of slack per sweep, which is what a
    // writer that emits EXCLUSIVE `sweep_end_ray_index` values costs (its
    // sweeps overlap by a single ray at each boundary). Checked as the
    // running total, before each sweep reserves anything, so it bounds the
    // allocation instead of describing it afterwards.
    //
    // A sweep that would push the total past the budget is DROPPED into
    // `skipped_message_count`, exactly as an unbounded sweep is, rather than
    // taking the whole file down with it: a writer whose sweeps overlap by
    // more than the one ray of slack is sloppy, not unreadable, and the
    // sweeps that do fit are real rays the caller can still use. The
    // allocation bound is identical either way — nothing is reserved for a
    // sweep until it has passed this test — so a file whose sweeps each
    // claim the whole ray list still costs a bounded decode, and the count
    // of what was dropped says so out loud.
    let row_budget = n_rays.saturating_add(sweep_count);
    let mut decoded_rows = 0usize;
    for sweep in 0..sweep_count {
        let Some((start_ray, end_ray)) =
            sweep_ray_bounds(&sweep_starts, &sweep_ends, sweep, n_rays)
        else {
            volume.metadata.skipped_message_count += 1;
            continue;
        };
        let sweep_rows = end_ray - start_ray + 1;
        if decoded_rows.saturating_add(sweep_rows) > row_budget {
            volume.metadata.skipped_message_count += 1;
            continue;
        }
        decoded_rows += sweep_rows;
        let fixed = fixed_angles.get(sweep).copied().unwrap_or_else(|| {
            fallback_fixed_angle(
                sweep_modes.get(sweep).copied().flatten(),
                &azimuth[start_ray..=end_ray],
                &elevation[start_ray..=end_ray],
            )
        }) as f32;
        let mut cut = ElevationCut::new(fixed, Some(sweep.min(255) as u8));
        cut.radials.reserve(end_ray - start_ray + 1);
        for ray in start_ray..=end_ray {
            let time_offset_ms = ray_seconds
                .as_ref()
                .and_then(|seconds| seconds.get(ray))
                .filter(|seconds| seconds.is_finite())
                .map(|seconds| (seconds * 1000.0 + time_epoch_shift_ms) as i32)
                .unwrap_or(0);
            cut.radials.push(Radial {
                azimuth_deg: (azimuth[ray] as f32).rem_euclid(360.0),
                elevation_deg: elevation[ray] as f32,
                time_offset_ms,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: nyquist
                    .as_ref()
                    .and_then(|values| values.get(ray))
                    .map(|value| *value as f32)
                    .filter(|value| *value > 0.0),
                radial_status: None,
            });
        }

        sweeps.push(DecodedSweep {
            start_ray,
            end_ray,
            cut,
        });
    }
    if sweeps.is_empty() {
        return Err(invalid("CfRadial volume decoded no sweeps"));
    }

    let expected_values = n_rays
        .checked_mul(n_gates)
        .ok_or_else(|| invalid("CfRadial field dimensions overflow addressable memory"))?;
    let mut canonical_fields = BTreeSet::new();
    for field in fields {
        // First field that claims a canonical moment wins it; a second
        // spelling of the same moment stays under its own CF name so the
        // two never collide in the cut's moment map.
        let moment = match canonical_moment_for_field(field) {
            Some(moment) if canonical_fields.insert(moment.clone()) => moment,
            _ => MomentType::Unknown(field.name.clone()),
        };
        let values = read_field_physical(&file, field)?;
        if values.len() < expected_values {
            return Err(invalid(format!(
                "CfRadial field '{}' has {} values; expected at least {expected_values}",
                field.name,
                values.len()
            )));
        }
        for sweep in &mut sweeps {
            let mut grid = MomentGrid {
                moment: moment.clone(),
                gate_range: gate_range.clone(),
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: Vec::new(),
                storage: MomentStorage::F32(Vec::new()),
            };
            grid.reserve_rows(sweep.end_ray - sweep.start_ray + 1);
            for (radial_index, ray) in (sweep.start_ray..=sweep.end_ray).enumerate() {
                let row_start = ray * n_gates;
                let row = &values[row_start..row_start + n_gates];
                grid.push_row(radial_index, MomentRow::F32(row.to_vec()))?;
            }
            sweep.cut.moments.insert(moment.clone(), grid);
        }
    }
    sweeps.sort_by(|left, right| left.cut.elevation_deg.total_cmp(&right.cut.elevation_deg));
    volume.cuts = sweeps.into_iter().map(|sweep| sweep.cut).collect();
    volume.metadata.decoded_radial_count = volume.cuts.iter().map(|cut| cut.radials.len()).sum();
    volume.metadata.message_count = sweep_count;
    Ok(volume)
}

struct DecodedSweep {
    start_ray: usize,
    end_ray: usize,
    cut: ElevationCut,
}

/// Inclusive ray range for one sweep, or `None` when the file does not
/// describe that sweep at all.
///
/// `sweep_start_ray_index` / `sweep_end_ray_index` are 0-based inclusive
/// indices into the ray list. A file whose `fixed_angle` array is
/// longer than its index arrays has told us the angles of sweeps it never
/// bounded; the only defensible readings are to drop those sweeps or to
/// refuse the file, and dropping keeps the sweeps the file DID describe. The
/// one default left is the file that carries no ray-index arrays whatsoever,
/// which is a single sweep spanning the volume — and only for sweep 0, so a
/// later phantom sweep cannot silently duplicate the whole ray list.
fn sweep_ray_bounds(
    starts: &[f64],
    ends: &[f64],
    sweep: usize,
    n_rays: usize,
) -> Option<(usize, usize)> {
    let last_ray = n_rays.checked_sub(1)?;
    match (starts.get(sweep), ends.get(sweep)) {
        (Some(start), Some(end)) => {
            let start = ray_index(*start)?;
            let end = ray_index(*end)?.min(last_ray);
            (start <= end).then_some((start, end))
        }
        _ if sweep == 0 && starts.is_empty() && ends.is_empty() => Some((0, last_ray)),
        _ => None,
    }
}

/// A ray index has to be a finite, non-negative whole number of rays. An
/// out-of-range one saturates on the cast and is then rejected by the
/// `start <= end` test above rather than wrapping into a valid-looking index.
fn ray_index(value: f64) -> Option<usize> {
    (value.is_finite() && value >= 0.0).then_some(value as usize)
}

/// CfRadial 1.4 §5.8 `sweep_mode` vocabulary, reduced to the distinction
/// this decoder acts on: whether the sweep's fixed angle is an elevation
/// (PPI) or an azimuth (RHI).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SweepMode {
    Ppi,
    Rhi,
    Other,
}

fn sweep_mode_from_str(mode: &str) -> SweepMode {
    match mode {
        "azimuth_surveillance" | "sector" | "manual_ppi" => SweepMode::Ppi,
        "rhi" | "manual_rhi" => SweepMode::Rhi,
        _ => SweepMode::Other,
    }
}

/// `sweep_mode(sweep, string_length)` char matrix → per-sweep scan modes.
fn read_sweep_modes(file: &Nc3File<'_>, sweep_count: usize) -> Vec<Option<SweepMode>> {
    let Some(var) = file.vars.get("sweep_mode") else {
        return vec![None; sweep_count];
    };
    let dims = file.var_dims(var);
    let [rows, width] = dims.as_slice() else {
        return vec![None; sweep_count];
    };
    let (rows, width) = (*rows, *width);
    let Ok(NcArray::Char(chars)) = file.read_var("sweep_mode") else {
        return vec![None; sweep_count];
    };
    (0..sweep_count)
        .map(|sweep| {
            if sweep >= rows || (sweep + 1) * width > chars.len() {
                return None;
            }
            let raw = &chars[sweep * width..(sweep + 1) * width];
            let text = raw.split(|byte| *byte == 0).next().unwrap_or_default();
            Some(sweep_mode_from_str(String::from_utf8_lossy(text).trim()))
        })
        .collect()
}

/// CfRadial's fixed angle is elevation for PPI sweeps and azimuth for RHI
/// sweeps. Azimuth needs a circular mean so a 359-degree/1-degree RHI points
/// north, rather than being mislabeled as 180 degrees when `fixed_angle` is
/// absent.
fn fallback_fixed_angle(mode: Option<SweepMode>, azimuth: &[f64], elevation: &[f64]) -> f64 {
    if mode == Some(SweepMode::Rhi) {
        circular_mean_degrees(azimuth)
            .or_else(|| azimuth.iter().copied().find(|value| value.is_finite()))
            .map(|value| value.rem_euclid(360.0))
            .unwrap_or(0.0)
    } else {
        arithmetic_mean(elevation).unwrap_or(0.0)
    }
}

fn circular_mean_degrees(values: &[f64]) -> Option<f64> {
    let mut sin_sum = 0.0;
    let mut cos_sum = 0.0;
    let mut count = 0usize;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        let radians = value.to_radians();
        sin_sum += radians.sin();
        cos_sum += radians.cos();
        count += 1;
    }
    if count == 0 || sin_sum.hypot(cos_sum) <= f64::EPSILON {
        return None;
    }
    Some(sin_sum.atan2(cos_sum).to_degrees().rem_euclid(360.0))
}

fn arithmetic_mean(values: &[f64]) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0usize;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        sum += value;
        count += 1;
    }
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

/// Canonical moment for a field variable: its NAME first, then the CF
/// `standard_name` attribute it carries.
///
/// The standard name is the spec's own identifier for the quantity — its
/// standard-name table pins `equivalent_reflectivity_factor`,
/// `radial_velocity_of_scatterers_away_from_instrument`,
/// `doppler_spectrum_width`, `log_differential_reflectivity_hv`,
/// `cross_correlation_ratio_hv`, `differential_phase_hv` and
/// `specific_differential_phase_hv` — so consulting it identifies moments
/// whose local field name is a house short name this table does not carry —
/// the GPM-VN WSR-88D files call differential reflectivity `DR` but do give
/// its standard name. The field name still wins where both are present,
/// because a file that names a field `VEL` and mislabels its standard name
/// is far more common than the reverse.
///
/// The standard name is consulted ONLY for a field whose name does not
/// already say it is something else — see
/// [`field_name_is_a_derived_diagnostic`]. Py-ART copies a moment's metadata
/// wholesale into the fields it DERIVES from that moment, so on real ARM
/// output `velocity_texture` carries
/// `standard_name = radial_velocity_of_scatterers_away_from_instrument` and
/// even the velocity long name; trusting that attribute would colour a
/// texture on the velocity table and hand it to dealiasing as the volume's
/// only velocity.
fn canonical_moment_for_field(var: &NcVar) -> Option<MomentType> {
    canonical_moment(&var.name).or_else(|| {
        if field_name_is_a_derived_diagnostic(&var.name) {
            return None;
        }
        var.attr_str("standard_name").and_then(canonical_moment)
    })
}

/// Field names that carry a MOMENT's `standard_name` while holding something
/// derived from that moment rather than the moment itself.
///
/// Py-ART builds a derived field by copying the source moment's metadata
/// dictionary and overwriting only the values, so the derived field inherits
/// the moment's `standard_name` and `long_name` verbatim. On the ARM CSAPR2
/// CMAC product checked here, `velocity_texture` — the local standard
/// deviation of Doppler velocity, in m/s but a TEXTURE, and the input to
/// Py-ART's despeckling and dealiasing gate filters — is written with
/// `standard_name = radial_velocity_of_scatterers_away_from_instrument` and
/// `long_name = "Mean dopper velocity"`, both straight from the velocity
/// entry of Py-ART's field-metadata table. `simulated_velocity` (a wind
/// profile projected onto the beam, not a measurement) inherits the same
/// pair.
///
/// Such a field keeps its own CF name as a [`MomentType::Unknown`], which is
/// what the file said and what the reference decoder does. The check is on
/// the whole name, and the `_texture` suffix covers the family rather than
/// one member: Py-ART names every texture retrieval `<field>_texture`.
fn field_name_is_a_derived_diagnostic(name: &str) -> bool {
    let normalized = name.trim().to_ascii_uppercase();
    normalized
        .strip_suffix("_TEXTURE")
        .is_some_and(|stem| !stem.is_empty())
        || normalized == "SIMULATED_VELOCITY"
}

/// Map a CfRadial field name onto a canonical moment.
///
/// CfRadial does not mandate field names. Files in the wild carry either the
/// short names their acquisition system used (Radx/DORADE style: `DBZ`,
/// `VEL`, `ZDR`) or a spelled-out CF name (`reflectivity_horizontal`,
/// `mean_doppler_velocity`, `spectral_width` — ARM products, Py-ART output
/// and the standard-name table itself), so both are matched here. Matching
/// is on the WHOLE name: `velocity_texture` is a texture field, not
/// velocity, and must not be caught by a substring rule — nor, since it
/// inherits the velocity `standard_name` on real files, by the attribute
/// fallback in [`canonical_moment_for_field`]. Polarization and filtering
/// suffixes (`DBZ_HC`, `VEL_F`, `ZDRHC`) are peeled off one at a time before
/// the stem is matched again.
///
/// A name with no recognised spelling is deliberately NOT guessed at: it
/// stays a [`MomentType::Unknown`] carrying its CF name, which is honest
/// about what the file said. That is where `SNR`, `NCP`,
/// `linear_depolarization_ratio` and every other quantity outside this
/// model's seven moments land.
fn canonical_moment(name: &str) -> Option<MomentType> {
    let normalized = name.trim().to_ascii_uppercase();
    let mut stem = normalized.as_str();
    loop {
        if let Some(moment) = match_moment_stem(stem) {
            return Some(moment);
        }
        stem = ["_F", "_HC", "_VC", "HC", "_V", "_H"]
            .iter()
            .find_map(|suffix| stem.strip_suffix(suffix).filter(|rest| !rest.is_empty()))?;
    }
}

fn match_moment_stem(stem: &str) -> Option<MomentType> {
    match stem {
        // Acquisition-system short names (Radx/DORADE lineage).
        "DBZ" | "DZ" | "DBZH" | "DBZV" | "REF" | "CZ" | "UZ" => Some(MomentType::Reflectivity),
        "VR" | "VE" | "VEL" | "VU" | "VG" | "VT" => Some(MomentType::Velocity),
        "SW" | "WIDTH" | "SPW" | "SPECTRUM_WIDTH" => Some(MomentType::SpectrumWidth),
        "ZDR" | "ZD" | "UZDR" => Some(MomentType::DifferentialReflectivity),
        "RHOHV" | "RHO" | "RH" | "ROHV" => Some(MomentType::CorrelationCoefficient),
        "PHIDP" | "PHI" | "PH" | "UPHIDP" => Some(MomentType::DifferentialPhase),
        "KDP" | "KD" => Some(MomentType::SpecificDifferentialPhase),
        // CF spellings: the standard names from the spec's table, plus the
        // field names ARM and Py-ART write, and the `corrected_*` forms
        // those two use for the quality-controlled copy of a moment. Where a
        // file carries both the raw and the corrected copy the corrected one
        // takes the canonical slot (it sorts first) and the raw one keeps its
        // own CF name, which is the right way round for display.
        "EQUIVALENT_REFLECTIVITY_FACTOR"
        | "REFLECTIVITY"
        | "REFLECTIVITY_HORIZONTAL"
        | "REFLECTIVITY_VERTICAL"
        | "CORRECTED_REFLECTIVITY"
        | "CORRECTED_REFLECTIVITY_HORIZONTAL" => Some(MomentType::Reflectivity),
        "RADIAL_VELOCITY_OF_SCATTERERS_AWAY_FROM_INSTRUMENT"
        | "MEAN_DOPPLER_VELOCITY"
        | "DOPPLER_VELOCITY"
        | "RADIAL_VELOCITY"
        | "VELOCITY"
        | "CORRECTED_VELOCITY" => Some(MomentType::Velocity),
        "DOPPLER_SPECTRUM_WIDTH" | "SPECTRAL_WIDTH" => Some(MomentType::SpectrumWidth),
        "LOG_DIFFERENTIAL_REFLECTIVITY_HV"
        | "DIFFERENTIAL_REFLECTIVITY"
        | "CORRECTED_DIFFERENTIAL_REFLECTIVITY" => Some(MomentType::DifferentialReflectivity),
        // ARM writes the correlation coefficient under its own names rather
        // than the spec's: `copol_coeff` on the older products and
        // `copol_correlation_coeff` on CMAC and the c1-level radars, whose
        // long name says "Copolar correlation coefficient (also known as
        // rhohv)". Both spellings are here, and so is ARM's dialect of the
        // standard name — the spec says `cross_correlation_ratio_hv`, ARM
        // says `radar_correlation_coefficient_hv` — because a file that
        // resolves no RHOHV offers no correlation coefficient to filter on
        // at all. Whole-name matching is what keeps this narrow: the Ka-SACR
        // products carry `co_to_crosspol_correlation_coeff`, a copolar-H to
        // crosspolar-V correlation and NOT rhohv, and it declares
        // `radar_correlation_coefficient_copolar_h_crosspolar_v` — a
        // different name and a different standard name, so neither rule
        // reaches it.
        "CROSS_CORRELATION_RATIO_HV"
        | "RADAR_CORRELATION_COEFFICIENT_HV"
        | "CROSS_CORRELATION_RATIO"
        | "COPOL_COEFF"
        | "COPOL_CORRELATION_COEFF" => Some(MomentType::CorrelationCoefficient),
        "DIFFERENTIAL_PHASE_HV"
        | "DIFFERENTIAL_PHASE"
        | "UNFOLDED_DIFFERENTIAL_PHASE"
        | "CORRECTED_DIFFERENTIAL_PHASE" => Some(MomentType::DifferentialPhase),
        "SPECIFIC_DIFFERENTIAL_PHASE_HV"
        | "SPECIFIC_DIFFERENTIAL_PHASE"
        | "CORRECTED_SPECIFIC_DIFFERENTIAL_PHASE" => Some(MomentType::SpecificDifferentialPhase),
        _ => None,
    }
}

/// Apply CF packing (physical = raw·scale_factor + add_offset) and
/// `_FillValue`/`missing_value` masking; everything lands in f32.
fn read_field_physical(file: &Nc3File<'_>, var: &NcVar) -> Result<Vec<f32>> {
    let scale = var.attr_f64("scale_factor").unwrap_or(1.0);
    let offset = var.attr_f64("add_offset").unwrap_or(0.0);
    let fill = var
        .attr_f64("_FillValue")
        .or_else(|| var.attr_f64("missing_value"));
    let raw = file.read_var(&var.name)?;
    let count = raw.len();
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        match raw.get_f64(index) {
            Some(value) if Some(value) != fill && value.is_finite() => {
                out.push((value * scale + offset) as f32);
            }
            _ => out.push(f32::NAN),
        }
    }
    Ok(out)
}

fn read_f64s(file: &Nc3File<'_>, name: &str) -> Result<Vec<f64>> {
    let raw = file.read_var(name)?;
    let count = raw.len();
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        out.push(
            raw.get_f64(index)
                .ok_or_else(|| invalid(format!("variable '{name}' is not numeric")))?,
        );
    }
    Ok(out)
}

fn parse_site(file: &Nc3File<'_>) -> RadarSite {
    // latitude/longitude/altitude are scalars at a fixed site and (time)
    // arrays on a mobile platform; element 0 is the right answer either way.
    let scalar = |name: &str| -> Option<f32> {
        file.read_var(name)
            .ok()
            .and_then(|array| array.get_f64(0))
            .map(|value| value as f32)
    };
    let id = file
        .gattr_str("instrument_name")
        .or_else(|| file.gattr_str("site_name"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("CFRAD")
        .to_owned();
    RadarSite {
        id,
        // Blank is absent, exactly as it is for `id` above: real files write
        // the attribute and leave it empty (NCAR SPOL does), and a `Some("")`
        // name reaches the UI as a blank site label instead of falling back
        // to the id.
        name: file
            .gattr_str("site_name")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        latitude_deg: scalar("latitude"),
        longitude_deg: scalar("longitude"),
        elevation_m: scalar("altitude"),
    }
}

fn parse_time_coverage_start(file: &Nc3File<'_>) -> Option<DateTime<Utc>> {
    // Either a char variable or a global attribute, ISO8601 "...Z".
    let text = match file.read_var("time_coverage_start") {
        Ok(NcArray::Char(chars)) => {
            let bytes: Vec<u8> = chars.into_iter().take_while(|byte| *byte != 0).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => file.gattr_str("time_coverage_start")?.to_owned(),
    };
    parse_iso8601_utc(&text)
}

/// Epoch named by `time(time)`'s CF `units` attribute — "seconds since
/// <datetime>" (CF Conventions 1.7 §4.4, "Time Coordinate"; CfRadial's
/// `time` coordinate requires exactly that form, and every real file checked
/// here writes it). `None` when the attribute is missing, is measured in
/// something other than seconds, or does not parse; the caller then reads
/// ray times as offsets from `time_coverage_start`, which is what the
/// attribute says on every file where the two epochs agree.
fn time_units_epoch(file: &Nc3File<'_>) -> Option<DateTime<Utc>> {
    let units = file.vars.get("time")?.attr_str("units")?;
    let (unit, epoch) = units.split_once(" since ")?;
    if !matches!(
        unit.trim().to_ascii_lowercase().as_str(),
        "second" | "seconds" | "sec" | "secs" | "s"
    ) {
        return None;
    }
    parse_iso8601_utc(epoch)
}

/// ISO 8601 UTC instant as CfRadial writes it: `2011-05-20T10:54:08Z`, with
/// a space separator and fractional seconds both tolerated (Radx emits
/// `...T11:36:06.500Z`, which without `%.f` parses as nothing at all).
/// CfRadial times are UTC by definition, so a trailing zero offset is
/// accepted and anything else is left to fail rather than be misread.
fn parse_iso8601_utc(text: &str) -> Option<DateTime<Utc>> {
    let mut trimmed = text.trim();
    for suffix in ["Z", "z", "UTC", "+00:00", "-00:00", "+0000", "-0000"] {
        if let Some(rest) = trimmed.strip_suffix(suffix) {
            trimmed = rest.trim_end();
            break;
        }
    }
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

fn invalid(reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset: 0,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_mode_vocabulary_maps_to_fixed_angle_convention() {
        assert_eq!(sweep_mode_from_str("azimuth_surveillance"), SweepMode::Ppi);
        assert_eq!(sweep_mode_from_str("sector"), SweepMode::Ppi);
        assert_eq!(sweep_mode_from_str("rhi"), SweepMode::Rhi);
        assert_eq!(sweep_mode_from_str("manual_rhi"), SweepMode::Rhi);
        assert_eq!(sweep_mode_from_str("vertical_pointing"), SweepMode::Other);
        assert_eq!(sweep_mode_from_str("coplane"), SweepMode::Other);
    }

    #[test]
    fn rhi_fixed_angle_fallback_uses_wrap_aware_azimuth_mean() {
        let fixed = fallback_fixed_angle(
            Some(SweepMode::Rhi),
            &[359.0, 0.0, 1.0],
            &[10.0, 20.0, 30.0],
        );
        assert!(!(0.01..=359.99).contains(&fixed), "fixed angle was {fixed}");
    }

    #[test]
    fn ppi_fixed_angle_fallback_still_uses_mean_elevation() {
        let fixed =
            fallback_fixed_angle(Some(SweepMode::Ppi), &[80.0, 90.0, 100.0], &[0.4, 0.5, 0.6]);
        assert!((fixed - 0.5).abs() < 1.0e-9);
    }

    #[test]
    fn canonical_moment_peels_polarization_and_filter_suffixes() {
        assert_eq!(canonical_moment("DBZ"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("DBZHC"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("dbz_hc"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("VEL_F"), Some(MomentType::Velocity));
        assert_eq!(canonical_moment("WIDTH"), Some(MomentType::SpectrumWidth));
        assert_eq!(
            canonical_moment("RHOHV"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(
            canonical_moment("PHIDP"),
            Some(MomentType::DifferentialPhase)
        );
        assert_eq!(
            canonical_moment("KDP"),
            Some(MomentType::SpecificDifferentialPhase)
        );
        // No recognised stem: the caller keeps the CF name rather than guess.
        assert_eq!(canonical_moment("NCP"), None);
        // Suffix peeling must never strip a name down to nothing.
        assert_eq!(canonical_moment("_H"), None);
        assert_eq!(canonical_moment("HC"), None);
    }

    #[test]
    fn canonical_moment_reads_the_cf_spellings_too() {
        // The spec's standard names.
        assert_eq!(
            canonical_moment("equivalent_reflectivity_factor"),
            Some(MomentType::Reflectivity)
        );
        assert_eq!(
            canonical_moment("radial_velocity_of_scatterers_away_from_instrument"),
            Some(MomentType::Velocity)
        );
        assert_eq!(
            canonical_moment("doppler_spectrum_width"),
            Some(MomentType::SpectrumWidth)
        );
        assert_eq!(
            canonical_moment("log_differential_reflectivity_hv"),
            Some(MomentType::DifferentialReflectivity)
        );
        assert_eq!(
            canonical_moment("cross_correlation_ratio_hv"),
            Some(MomentType::CorrelationCoefficient)
        );
        // ARM's own spellings of the same quantity, variable name and
        // standard name both - `copol_coeff` on the older products,
        // `copol_correlation_coeff` on CMAC and the c1 radars.
        assert_eq!(
            canonical_moment("copol_coeff"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(
            canonical_moment("copol_correlation_coeff"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(
            canonical_moment("radar_correlation_coefficient_hv"),
            Some(MomentType::CorrelationCoefficient)
        );
        // But NOT the Ka-SACR copolar-H/crosspolar-V correlation, which is a
        // different quantity that merely ends the same way.
        assert_eq!(canonical_moment("co_to_crosspol_correlation_coeff"), None);
        assert_eq!(
            canonical_moment("radar_correlation_coefficient_copolar_h_crosspolar_v"),
            None
        );
        assert_eq!(
            canonical_moment("differential_phase_hv"),
            Some(MomentType::DifferentialPhase)
        );
        assert_eq!(
            canonical_moment("specific_differential_phase_hv"),
            Some(MomentType::SpecificDifferentialPhase)
        );
        // The field names ARM and Py-ART actually write.
        assert_eq!(
            canonical_moment("reflectivity_horizontal"),
            Some(MomentType::Reflectivity)
        );
        assert_eq!(
            canonical_moment("mean_doppler_velocity"),
            Some(MomentType::Velocity)
        );
        assert_eq!(
            canonical_moment("spectral_width"),
            Some(MomentType::SpectrumWidth)
        );
        assert_eq!(
            canonical_moment("corrected_reflectivity"),
            Some(MomentType::Reflectivity)
        );
        // Matching is on the whole name: these are different quantities.
        assert_eq!(canonical_moment("velocity_texture"), None);
        assert_eq!(canonical_moment("signal_to_noise_ratio"), None);
        assert_eq!(canonical_moment("norm_coherent_power"), None);
        assert_eq!(canonical_moment("linear_depolarization_ratio"), None);
        assert_eq!(canonical_moment("radar_echo_classification"), None);
    }

    #[test]
    fn derived_diagnostics_are_recognised_by_their_own_name() {
        // Py-ART's texture retrievals, which inherit the source moment's
        // standard_name wholesale.
        assert!(field_name_is_a_derived_diagnostic("velocity_texture"));
        assert!(field_name_is_a_derived_diagnostic("VELOCITY_TEXTURE"));
        assert!(field_name_is_a_derived_diagnostic(
            "differential_phase_texture"
        ));
        assert!(field_name_is_a_derived_diagnostic(" simulated_velocity "));
        // Real moments, however they are spelled, are not diagnostics — the
        // attribute fallback exists for exactly these.
        assert!(!field_name_is_a_derived_diagnostic(
            "clutter_masked_velocity"
        ));
        assert!(!field_name_is_a_derived_diagnostic("corrected_velocity"));
        assert!(!field_name_is_a_derived_diagnostic("DR"));
        // Nothing is stripped down to nothing, and "texture" alone is a
        // field name this rule has no evidence about.
        assert!(!field_name_is_a_derived_diagnostic("_texture"));
        assert!(!field_name_is_a_derived_diagnostic("texture"));
    }

    #[test]
    fn sweep_bounds_come_from_the_file_or_not_at_all() {
        // Both arrays cover the sweep: inclusive range, end clamped to the
        // ray list.
        assert_eq!(
            sweep_ray_bounds(&[0.0, 3.0], &[2.0, 9.0], 1, 6),
            Some((3, 5))
        );
        // No arrays at all: sweep 0 is the whole volume, later sweeps are
        // not invented copies of it.
        assert_eq!(sweep_ray_bounds(&[], &[], 0, 6), Some((0, 5)));
        assert_eq!(sweep_ray_bounds(&[], &[], 1, 6), None);
        // Arrays that stop short do not fall back to the whole volume.
        assert_eq!(sweep_ray_bounds(&[0.0], &[5.0], 1, 6), None);
        // Nonsense indices are rejected rather than wrapped into range.
        assert_eq!(sweep_ray_bounds(&[-1.0], &[5.0], 0, 6), None);
        assert_eq!(sweep_ray_bounds(&[f64::NAN], &[5.0], 0, 6), None);
        assert_eq!(sweep_ray_bounds(&[1e300], &[5.0], 0, 6), None);
        assert_eq!(sweep_ray_bounds(&[4.0], &[1.0], 0, 6), None);
        // A volume with no rays has no sweeps.
        assert_eq!(sweep_ray_bounds(&[0.0], &[0.0], 0, 0), None);
    }

    #[test]
    fn iso8601_parsing_covers_the_forms_cfradial_writes() {
        let expected = Utc.with_ymd_and_hms(2011, 5, 20, 10, 54, 8).unwrap();
        assert_eq!(parse_iso8601_utc("2011-05-20T10:54:08Z"), Some(expected));
        assert_eq!(parse_iso8601_utc(" 2011-05-20 10:54:08 "), Some(expected));
        assert_eq!(
            parse_iso8601_utc("2011-05-20T10:54:08+00:00"),
            Some(expected)
        );
        // Radx writes fractional seconds; without them the whole string
        // fails to parse and the caller falls back to the Unix epoch.
        assert_eq!(
            parse_iso8601_utc("2011-05-20T10:54:08.500Z"),
            Some(expected + chrono::TimeDelta::milliseconds(500))
        );
        assert_eq!(parse_iso8601_utc("not a time"), None);
        assert_eq!(parse_iso8601_utc(""), None);
    }
}
