//! The offline-validation interchange format: `IQDUMP`.
//!
//! Deliberately not a radar format and never written by the application. It
//! exists so that two independent implementations of the estimators can be
//! handed byte-identical samples and their answers compared number by number,
//! which is the only way to tell a correct moment from a plausible one -
//! synthetic recovery on its own proves only that the code agrees with itself.
//! It is also what carries the small real-pulse fixture in
//! `crates/nexrad_io/tests/data`.
//!
//! It lives in the library rather than in the two offline examples that read it
//! because those two had a copy each, and because a format that carries the
//! evidence deserves tests of its own.
//!
//! # Layout
//!
//! Little-endian throughout.
//!
//! ```text
//! magic          8 bytes   "IQDUMP01" or "IQDUMP02"
//! pulses         u32
//! bins           u32       recorded bins per pulse, burst samples included
//! dual_pol       u32       0 or 1
//! wavelength_m   f32
//! prt_s          f32       nominal; version 2 carries the real per-pulse value
//! pulse_width_s  f32
//! noise_h_dbm    f32
//! noise_v_dbm    f32
//! dbz0_db        f32
//! saturation_dbm f32
//!   -- version 2 only --
//! time_utc       i64       unix seconds of the first pulse
//! site_len       u32
//! site           site_len bytes, ASCII
//! prt_s          f32 x pulses
//!   -- both again --
//! range_bins     f32 x bins
//! azimuth_deg    f32 x pulses
//! elevation_deg  f32 x pulses
//! samples        per pulse: h as (i, q) x bins, then v as (i, q) x bins if dual
//! ```
//!
//! # Why version 2 exists
//!
//! Version 1 carried one scalar PRT for a whole sweep and stamped it on every
//! pulse. That made one of this module's four advertised refusals unreachable
//! through the tools: the timing-based staggered-PRT detector - the guard that
//! catches a file which fails to declare its mode - can never fire on pulses
//! whose intervals were all copied from the same number. Version 2 carries the
//! interval that was actually recorded for each pulse, so a staggered waveform
//! survives the round trip and is refused on the far side.
//!
//! Version 1 is still read, because dumps of it exist; it is reported as
//! [`DumpVersion::V1`] so a caller can say that its timing is nominal.

use crate::iq::{IqPulse, IqSweep};

/// Which layout a dump was written in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DumpVersion {
    /// One scalar PRT for the sweep, stamped on every pulse. Pulse timing is
    /// nominal, so the staggered-PRT guard cannot see anything in it.
    V1,
    /// Per-pulse PRT, site and time.
    V2,
}

impl DumpVersion {
    pub fn magic(self) -> &'static [u8; 8] {
        match self {
            Self::V1 => b"IQDUMP01",
            Self::V2 => b"IQDUMP02",
        }
    }
}

/// Why a dump could not be read.
#[derive(Clone, Debug, thiserror::Error, PartialEq)]
pub enum DumpError {
    #[error("not an IQDUMP file: expected a magic of IQDUMP01 or IQDUMP02")]
    NotADump,
    #[error("truncated at byte {offset}: {needed} more bytes were expected")]
    Truncated { offset: usize, needed: usize },
    #[error("site name is {length} bytes, which is longer than the {remaining} bytes that remain")]
    SiteTooLong { length: usize, remaining: usize },
    #[error("site name is not ASCII")]
    SiteNotAscii,
}

/// A dump and the version it was written in.
#[derive(Clone, Debug, PartialEq)]
pub struct Dump {
    pub version: DumpVersion,
    pub sweep: IqSweep,
}

/// Read a dump of either version.
pub fn read_dump(bytes: &[u8]) -> Result<Dump, DumpError> {
    let version = if bytes.starts_with(DumpVersion::V2.magic()) {
        DumpVersion::V2
    } else if bytes.starts_with(DumpVersion::V1.magic()) {
        DumpVersion::V1
    } else {
        return Err(DumpError::NotADump);
    };

    let mut cursor = Cursor { bytes, offset: 8 };
    let pulses = cursor.u32()? as usize;
    let bins = cursor.u32()? as usize;
    let dual_pol = cursor.u32()? != 0;
    let wavelength_m = cursor.f32()?;
    let nominal_prt_s = cursor.f32()?;
    let pulse_width_s = cursor.f32()?;
    let noise_h_dbm = cursor.f32()?;
    let noise_v_dbm = cursor.f32()?;
    let dbz0_db = cursor.f32()?;
    let saturation_dbm = cursor.f32()?;

    let (time_utc, site, prts) = match version {
        DumpVersion::V1 => (0i64, "IQD".to_owned(), vec![nominal_prt_s; pulses]),
        DumpVersion::V2 => {
            let time_utc = cursor.i64()?;
            let site = cursor.ascii()?;
            let prts = cursor.f32_vec(pulses)?;
            (time_utc, site, prts)
        }
    };

    let range_bins = cursor.f32_vec(bins)?;
    let azimuths = cursor.f32_vec(pulses)?;
    let elevations = cursor.f32_vec(pulses)?;

    let mut built = Vec::with_capacity(pulses);
    for pulse in 0..pulses {
        let h = cursor.iq_vec(bins)?;
        let v = if dual_pol {
            cursor.iq_vec(bins)?
        } else {
            Vec::new()
        };
        built.push(IqPulse {
            azimuth_deg: azimuths[pulse],
            elevation_deg: elevations[pulse],
            prt_seconds: prts[pulse],
            // The dump records the interval to the next pulse. The interval
            // SINCE the previous one is that pulse's value, which is what
            // makes an undeclared stagger visible from either direction; the
            // first pulse has nothing before it and reports its own.
            prt_previous_seconds: prts[pulse.saturating_sub(1)],
            time_utc,
            time_millis: 0,
            // A dump carries samples, not a transmitter reference: whatever
            // burst the record had was resolved before it was written.
            burst: None,
            h,
            v,
        });
    }

    // Advisory only - `range_bins` is authoritative - so a mask that is not a
    // uniform ladder is left for `process_sweep` to refuse with the bin it
    // stumbled on, rather than being averaged away here. `None` says the
    // ladder has no single spacing, which is the same thing the reader says
    // about an irregular range mask.
    let gate_spacing_m = uniform_spacing(&range_bins);

    Ok(Dump {
        version,
        sweep: IqSweep {
            site,
            time_utc,
            wavelength_m,
            pulse_width_s,
            gate_spacing_m,
            first_gate_m: range_bins.first().copied().unwrap_or_default(),
            range_bins,
            noise_dbm: [noise_h_dbm, noise_v_dbm],
            dbz_calibration: dbz0_db,
            saturation_dbm,
            pulses: built,
            // Fields the dump does not carry. A dump is an estimator fixture,
            // not a record: it exists to hand two implementations identical
            // samples, so it stores what the estimators read and nothing else.
            // `burst_samples` is zero because the dump's `range_bins` already
            // describe the bins it holds, whatever they are; a caller that
            // dumped a slice including burst samples passes the count through
            // `MomentConfig::burst_samples` as it would for any other reader.
            ..IqSweep::default()
        },
    })
}

/// Spacing between recorded bins when it is the same everywhere.
///
/// Mirrors the reader's rule so a dumped ladder and a decoded one describe
/// themselves the same way.
fn uniform_spacing(range_bins: &[f32]) -> Option<f32> {
    let first = *range_bins.first()?;
    let second = *range_bins.get(1)?;
    let spacing = second - first;
    range_bins
        .windows(2)
        .all(|pair| (pair[1] - pair[0] - spacing).abs() < 1e-3)
        .then_some(spacing)
}

/// Write a sweep as a version 2 dump. Round-trips through [`read_dump`].
pub fn write_dump(sweep: &IqSweep) -> Vec<u8> {
    let pulses = sweep.pulses.len();
    let bins = sweep.range_bins.len();
    let dual_pol = pulses > 0 && sweep.pulses.iter().all(|pulse| pulse.v.len() == bins);

    let mut out = Vec::new();
    out.extend_from_slice(DumpVersion::V2.magic());
    out.extend_from_slice(&(pulses as u32).to_le_bytes());
    out.extend_from_slice(&(bins as u32).to_le_bytes());
    out.extend_from_slice(&u32::from(dual_pol).to_le_bytes());
    let nominal_prt = sweep
        .pulses
        .first()
        .map(|pulse| pulse.prt_seconds)
        .unwrap_or_default();
    for value in [
        sweep.wavelength_m,
        nominal_prt,
        sweep.pulse_width_s,
        sweep.noise_dbm[0],
        sweep.noise_dbm[1],
        sweep.dbz_calibration,
        sweep.saturation_dbm,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&sweep.time_utc.to_le_bytes());
    out.extend_from_slice(&(sweep.site.len() as u32).to_le_bytes());
    out.extend_from_slice(sweep.site.as_bytes());
    for pulse in &sweep.pulses {
        out.extend_from_slice(&pulse.prt_seconds.to_le_bytes());
    }
    for range_m in &sweep.range_bins {
        out.extend_from_slice(&range_m.to_le_bytes());
    }
    for pulse in &sweep.pulses {
        out.extend_from_slice(&pulse.azimuth_deg.to_le_bytes());
    }
    for pulse in &sweep.pulses {
        out.extend_from_slice(&pulse.elevation_deg.to_le_bytes());
    }
    for pulse in &sweep.pulses {
        for (i, q) in &pulse.h {
            out.extend_from_slice(&i.to_le_bytes());
            out.extend_from_slice(&q.to_le_bytes());
        }
        if dual_pol {
            for (i, q) in &pulse.v {
                out.extend_from_slice(&i.to_le_bytes());
                out.extend_from_slice(&q.to_le_bytes());
            }
        }
    }
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Cursor<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], DumpError> {
        let end = self.offset.checked_add(count).ok_or(DumpError::Truncated {
            offset: self.offset,
            needed: count,
        })?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(DumpError::Truncated {
                offset: self.offset,
                needed: count,
            })?;
        self.offset = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, DumpError> {
        let slice = self.take(4)?;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn i64(&mut self) -> Result<i64, DumpError> {
        let slice = self.take(8)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(slice);
        Ok(i64::from_le_bytes(bytes))
    }

    fn f32(&mut self) -> Result<f32, DumpError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn f32_vec(&mut self, count: usize) -> Result<Vec<f32>, DumpError> {
        (0..count).map(|_| self.f32()).collect()
    }

    fn iq_vec(&mut self, count: usize) -> Result<Vec<(f32, f32)>, DumpError> {
        (0..count).map(|_| Ok((self.f32()?, self.f32()?))).collect()
    }

    fn ascii(&mut self) -> Result<String, DumpError> {
        let length = self.u32()? as usize;
        let remaining = self.bytes.len().saturating_sub(self.offset);
        if length > remaining {
            return Err(DumpError::SiteTooLong { length, remaining });
        }
        let slice = self.take(length)?;
        if !slice.is_ascii() {
            return Err(DumpError::SiteNotAscii);
        }
        Ok(String::from_utf8_lossy(slice).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iq_moments::{DwellPlan, IqMomentError, MomentConfig, process_sweep};

    fn sweep(prts: &[f32]) -> IqSweep {
        let pulses = prts
            .iter()
            .enumerate()
            .map(|(index, prt)| IqPulse {
                azimuth_deg: 90.0 + 0.01 * index as f32,
                elevation_deg: 4.0,
                prt_seconds: *prt,
                prt_previous_seconds: prts[index.saturating_sub(1)],
                time_utc: 1_369_079_161,
                h: (0..8)
                    .map(|bin| (0.01 * bin as f32, 0.002 * index as f32))
                    .collect(),
                v: (0..8)
                    .map(|bin| (0.009 * bin as f32, 0.002 * index as f32))
                    .collect(),
                ..IqPulse::default()
            })
            .collect();
        IqSweep {
            site: "KOUN".to_owned(),
            time_utc: 1_369_079_161,
            wavelength_m: 0.1108,
            pulse_width_s: 1.5e-6,
            gate_spacing_m: Some(500.0),
            first_gate_m: 0.0,
            range_bins: (0..8).map(|bin| 500.0 * bin as f32).collect(),
            noise_dbm: [-80.5555, -80.5955],
            dbz_calibration: -35.5,
            saturation_dbm: 6.0,
            pulses,
            ..IqSweep::default()
        }
    }

    #[test]
    fn a_version_two_dump_round_trips_every_field_including_per_pulse_timing() {
        let original = sweep(&[833.375e-6, 833.375e-6, 555.583e-6, 833.375e-6]);
        let round_tripped = read_dump(&write_dump(&original)).expect("reads back");
        assert_eq!(round_tripped.version, DumpVersion::V2);
        assert_eq!(round_tripped.sweep, original);
    }

    #[test]
    fn a_staggered_waveform_survives_the_round_trip_and_is_refused_on_the_far_side() {
        // The point of version 2. A dump that stamps one PRT on every pulse
        // cannot express this file at all, so the timing guard - one of the
        // module's four refusals - could never be exercised through the
        // offline tools that validated everything else.
        let mut original = sweep(&[833.375e-6; 64]);
        for (index, pulse) in original.pulses.iter_mut().enumerate() {
            if index % 2 == 1 {
                pulse.prt_seconds = 833.375e-6 * 2.0 / 3.0;
            }
        }
        let round_tripped = read_dump(&write_dump(&original)).expect("reads back");
        let config = MomentConfig {
            dwell: DwellPlan::contiguous(8),
            ..MomentConfig::default()
        };
        let error = process_sweep(&round_tripped.sweep, &config).expect_err("refused");
        assert!(
            matches!(error, IqMomentError::StaggeredPrt { pulse_index: 1, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_version_one_dump_still_reads_and_says_its_timing_is_nominal() {
        // Version 1 bytes, built by hand: the same header without the time,
        // site and per-pulse PRT block.
        let original = sweep(&[833.375e-6; 3]);
        let two = write_dump(&original);
        let mut one = Vec::new();
        one.extend_from_slice(DumpVersion::V1.magic());
        // pulses, bins, dual_pol and the seven scalars are byte-identical.
        one.extend_from_slice(&two[8..8 + 12 + 28]);
        // Skip version 2's time, site block and per-pulse PRT array.
        let skip = 8 + 12 + 28 + 8 + 4 + original.site.len() + 4 * original.pulses.len();
        one.extend_from_slice(&two[skip..]);

        let read = read_dump(&one).expect("reads back");
        assert_eq!(read.version, DumpVersion::V1);
        assert_eq!(read.sweep.site, "IQD");
        assert_eq!(read.sweep.time_utc, 0);
        for pulse in &read.sweep.pulses {
            assert!((pulse.prt_seconds - 833.375e-6).abs() < 1e-12);
        }
        assert_eq!(read.sweep.range_bins, original.range_bins);
        assert_eq!(read.sweep.pulses[2].h, original.pulses[2].h);
    }

    #[test]
    fn a_single_pol_sweep_round_trips_without_a_vertical_channel() {
        let mut original = sweep(&[833.375e-6; 4]);
        for pulse in &mut original.pulses {
            pulse.v.clear();
        }
        let round_tripped = read_dump(&write_dump(&original)).expect("reads back");
        assert_eq!(round_tripped.sweep, original);
    }

    #[test]
    fn a_truncated_or_foreign_file_is_an_error_and_not_a_panic() {
        assert_eq!(read_dump(b"not a dump at all"), Err(DumpError::NotADump));
        assert_eq!(read_dump(&[]), Err(DumpError::NotADump));
        let full = write_dump(&sweep(&[833.375e-6; 4]));
        for cut in [8usize, 20, 40, full.len() - 1] {
            assert!(matches!(
                read_dump(&full[..cut]),
                Err(DumpError::Truncated { .. })
            ));
        }
    }
}
