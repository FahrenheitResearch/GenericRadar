//! Native DORADE sweepfile (`swp.*`) decoder for mobile research radars
//! (DOW6/DOW7/DOW8, COW, RaXPol, NOXP, and other CSWR/OU/EOL sweepfile
//! producers).
//!
//! One sweepfile is one sweep, so each file contributes one
//! [`radar_core::ElevationCut`] to a [`radar_core::RadarVolume`]. Moments land
//! in compact [`radar_core::MomentGrid`] storage without an intermediate
//! volume model: 16-bit DORADE integers stay 16-bit, shifted into unsigned
//! space so the grid's `(raw - offset) / scale` reproduces DORADE's
//! `(raw - bias) / scale` exactly.
//!
//! # Format references
//!
//! - R. Oye and M. Case, *DORADE Data Format*, NCAR/ATD, 1995 (revised
//!   2003/2010 by W.-C. Lee, NCAR/EOL) — block layouts and semantics. This is
//!   the primary specification this module implements.
//! - lrose-core `DoradeData.hh` (NCAR/EOL) — authoritative struct offsets.
//! - HRD run-length encoding from the NOAA Hurricane Research Division, as
//!   described in the DORADE document and implemented in `soloii`/Radx.
//! - Nyquist relations: R. J. Doviak and D. S. Zrnić, *Doppler Radar and
//!   Weather Observations*, 2nd ed., 1993, eq. 3.17; staggered-PRT extension
//!   from D. S. Zrnić and P. Mahapatra, IEEE Trans. Aerosp. Electron. Syst.
//!   AES-21, 1985, and S. Torres, Y. Dubel, and D. S. Zrnić,
//!   *J. Atmos. Oceanic Technol.* 21, 2004, 1389-1399.
//!
//! # Entry points
//!
//! [`decode_dorade_sweep`] decodes one sweepfile into a single-cut volume.
//! [`append_dorade_sweep`] plus [`finalize_dorade_volume`] assemble several
//! sweepfiles into one volume scan; `mobile_archive` uses that pair.
//!
//! # Divergences from the Fahrenheit Research BowEcho decoder this was ported
//! from
//!
//! The block parsing is a faithful port. The adaptations are:
//!
//! - **Scan mode**: this crate's [`radar_core`] has no scan-mode enum, so the
//!   RADD `scan_mode` code is surfaced two ways instead: as
//!   [`DoradeSweepHeader::scan_mode`] from [`peek_dorade_sweep`], and as a
//!   suffix on `VolumeMetadata::archive_version` (`"DORADE PPI"`,
//!   `"DORADE RHI"`, `"DORADE VERT"`, `"DORADE OTHER"`). An RHI sweep would
//!   otherwise decode as a plan view with no way to tell. When `radar_core`
//!   grows a scan-mode field, that is where this belongs.
//! - **Radar frequency**: no metadata field exists for it here, so the RADD
//!   frequency is used only for the Nyquist fallback and is not published.
//! - **Empty volume seed**: `RadarVolume` has no `Default` here, so
//!   [`empty_dorade_volume`] seeds an unnamed site at the Unix epoch; the
//!   first appended sweep overwrites both.
//! - **Uncompressed rows are truncated to the declared cell count.** DORADE
//!   blocks are padded to a 4-byte boundary, so an odd cell count leaves one
//!   trailing word inside RDAT that is padding, not a gate. Real VORTEX-2
//!   NOXP sweepfiles declare 1001 cells (in PARM *and* CSFD) and carry 1002
//!   words per uncompressed RDAT; without the truncation the moment grid
//!   silently grows to 1002 gates and disagrees with the gate count on every
//!   [`radar_core::Radial`] in the same cut. RLE rows are already exact.
//! - **Site coordinates are range-checked.** The same NOXP corpus writes
//!   longitude into both the longitude and the latitude slot (`-99.99996` and
//!   `260.00003`, which are the same meridian in the two sign conventions), so
//!   an unchecked read parks the radar at latitude 260. A latitude outside
//!   ±90° is reported as unknown rather than as a number, and a longitude in
//!   the 0-360 convention is wrapped into ±180.
//!
//! # Known limitations, documented rather than silent
//!
//! - Multi-segment CSFD range geometry is flattened to the first segment's
//!   spacing because [`radar_core::GateRange`] models uniform gates only.
//! - Per-ray platform georeferencing (`ASIB`) is ignored: DOW/COW/RaXPol
//!   deployments are parked, so the RADD site position applies to the whole
//!   sweep. Airborne tail radars would need ASIB handling.
//! - 16-bit float fields (`binary_format` 5) are skipped rather than decoded;
//!   no writer in the validation corpus emits them.
//! - HRD run-length encoding is applied to 16-bit fields only, which is what
//!   the compression scheme is defined over. An 8-bit or 32-bit field inside a
//!   sweep whose RADD declares compression is read verbatim; no observed
//!   writer mixes the two.
//! - Rays flagged in antenna transition (RYIB `ray_status != 0`) are dropped,
//!   because they span the gap between two fixed angles and smear a PPI. A
//!   sweep whose rays are *all* flagged keeps them: that is a writer quirk,
//!   not an empty sweep.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use radar_core::{
    GateRange, MomentGrid, MomentRow, MomentStorage, MomentType, RadarSite, RadarVolume, Radial,
    ResearchMoment,
};

use crate::{NexradError, Result};

const BLOCK_HEADER_LEN: usize = 8;
const DORADE_BAD_F32: f32 = -9999.0;
/// DORADE altitude fields are kilometres MSL.
const KM_TO_M: f64 = 1000.0;
/// Real mobile-radar sweepfiles in the validation corpus stay below 2,000
/// gates per radial. This ceiling allows unusually long research rays while
/// rejecting an attacker-controlled PARM/CSFD count before RLE decoding
/// allocates a multi-gigabyte row.
const MAX_DORADE_GATES_PER_RADIAL: usize = 16 * 1024;
/// Aggregate decoded cells retained while assembling one sweep. The cap is
/// independent of input compression and bounds the combined moment rows.
const MAX_DORADE_CELLS_PER_SWEEP: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn i16(self, bytes: &[u8], offset: usize) -> i16 {
        let raw = [bytes[offset], bytes[offset + 1]];
        match self {
            Self::Little => i16::from_le_bytes(raw),
            Self::Big => i16::from_be_bytes(raw),
        }
    }

    fn i32(self, bytes: &[u8], offset: usize) -> i32 {
        let raw = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        match self {
            Self::Little => i32::from_le_bytes(raw),
            Self::Big => i32::from_be_bytes(raw),
        }
    }

    fn f32(self, bytes: &[u8], offset: usize) -> f32 {
        f32::from_bits(self.i32(bytes, offset) as u32)
    }
}

/// Antenna scan mode recorded in the RADD block.
///
/// Codes per the DORADE format document (Oye and Case 1995) and the
/// authoritative lrose-core `DoradeData.hh` enum: 0 = CAL (calibration),
/// 1 = PPI (sector), 2 = COP (coplane), 3 = RHI, 4 = VER (vertical pointing),
/// 5 = TAR (target/stationary), 6 = MAN (manual), 7 = IDL (idle),
/// 8 = SUR (360° surveillance), 9 = AIR (airborne), 10 = HOR (horizontal).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoradeScanMode {
    /// Plan view: RADD code 1 (sector) or 8 (surveillance).
    Ppi,
    /// Range-height: RADD code 3. The cut's "elevation" is a fixed azimuth.
    Rhi,
    /// Vertically pointing: RADD code 4.
    VerticalPointing,
    /// Anything else, including calibration and idle scans.
    Other,
}

impl DoradeScanMode {
    fn from_radd(code: i16) -> Self {
        match code {
            1 | 8 => Self::Ppi,
            3 => Self::Rhi,
            4 => Self::VerticalPointing,
            _ => Self::Other,
        }
    }

    /// Tag appended to `VolumeMetadata::archive_version`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ppi => "PPI",
            Self::Rhi => "RHI",
            Self::VerticalPointing => "VERT",
            Self::Other => "OTHER",
        }
    }
}

/// Cheap header peek used to group sweepfiles into volume scans without a
/// full decode. Parsing stops at the first ray.
#[derive(Clone, Debug, PartialEq)]
pub struct DoradeSweepHeader {
    pub instrument: String,
    pub volume_number: i32,
    pub sweep_number: i32,
    pub fixed_angle_deg: f32,
    pub start_time: Option<DateTime<Utc>>,
    pub scan_mode: DoradeScanMode,
    /// Site position as recorded, with the range checks described in the
    /// module docs applied. `None` means the file did not record a usable
    /// value.
    pub latitude_deg: Option<f32>,
    pub longitude_deg: Option<f32>,
    pub altitude_m: Option<f32>,
}

/// `true` when the buffer starts with a plausible DORADE descriptor block.
///
/// Sweepfiles written by solo/Radx begin with `COMM`, `SSWB`, or `VOLD`; the
/// 4-byte length that follows must be valid in at least one byte order.
pub fn looks_like_dorade_bytes(bytes: &[u8]) -> bool {
    if bytes.len() < BLOCK_HEADER_LEN {
        return false;
    }
    if !matches!(&bytes[..4], b"COMM" | b"SSWB" | b"VOLD" | b"RADD") {
        return false;
    }
    detect_endian(bytes).is_ok()
}

/// `true` when the file name uses the `swp.*` sweepfile convention.
pub fn looks_like_dorade_name(name: &str) -> bool {
    let file_name = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    file_name.starts_with("swp.") || file_name.ends_with(".swp") || file_name.ends_with(".dorade")
}

/// Convenience: path-based variant of [`looks_like_dorade_name`].
pub fn looks_like_dorade_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(looks_like_dorade_name)
}

/// Parse only the descriptor blocks (everything before the first ray).
pub fn peek_dorade_sweep(bytes: &[u8]) -> Result<DoradeSweepHeader> {
    let mut parse = SweepParse::new(detect_endian(bytes)?);
    parse.run(bytes, true)?;
    Ok(DoradeSweepHeader {
        instrument: parse.instrument.clone(),
        volume_number: parse.volume_number,
        sweep_number: parse.sweep_number,
        fixed_angle_deg: parse.fixed_angle_deg,
        start_time: parse.start_time,
        scan_mode: DoradeScanMode::from_radd(parse.scan_mode),
        latitude_deg: parse.site_latitude_deg(),
        longitude_deg: parse.site_longitude_deg(),
        altitude_m: parse.site_altitude_m(),
    })
}

/// Decode one DORADE sweepfile into a fresh single-cut volume.
///
/// This is the crate entry point for the format.
pub fn decode_dorade_sweep(bytes: &[u8]) -> Result<RadarVolume> {
    let mut volume = empty_dorade_volume();
    append_dorade_sweep(bytes, &mut volume)?;
    finalize_dorade_volume(&mut volume);
    Ok(volume)
}

/// Read and decode one sweepfile from disk.
pub fn decode_dorade_sweep_from_path(path: &Path) -> Result<RadarVolume> {
    let bytes = std::fs::read(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut volume = decode_dorade_sweep(&bytes)?;
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(volume)
}

/// An empty volume ready for [`append_dorade_sweep`].
///
/// The site id is empty and the time is the Unix epoch; the first appended
/// sweep replaces both from its RADD and SSWB blocks.
pub fn empty_dorade_volume() -> RadarVolume {
    RadarVolume::new(
        RadarSite::new(String::new()),
        DateTime::<Utc>::from_timestamp(0, 0).expect("the Unix epoch is a valid timestamp"),
    )
}

/// Decode a set of sweepfiles forming one volume scan.
///
/// Cuts are appended in input order and then sorted by elevation (ties keep
/// input order, which callers arrange to be scan time). The site position
/// comes from the first sweep's RADD block — mobile radars move between
/// deployments, so the coordinates always come from the file.
pub fn decode_dorade_volume_from_slices<S: AsRef<[u8]>>(sweeps: &[S]) -> Result<RadarVolume> {
    if sweeps.is_empty() {
        return Err(invalid(0, "no DORADE sweeps to decode"));
    }
    let mut volume = empty_dorade_volume();
    for sweep in sweeps {
        append_dorade_sweep(sweep.as_ref(), &mut volume)?;
    }
    finalize_dorade_volume(&mut volume);
    Ok(volume)
}

/// Decode a set of sweepfile paths forming one volume scan.
pub fn decode_dorade_volume_from_paths<P: AsRef<Path>>(paths: &[P]) -> Result<RadarVolume> {
    if paths.is_empty() {
        return Err(invalid(0, "no DORADE sweep paths to decode"));
    }
    let mut volume = empty_dorade_volume();
    for path in paths {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| NexradError::Io {
            path: path.display().to_string(),
            source,
        })?;
        append_dorade_sweep(&bytes, &mut volume)?;
    }
    volume.metadata.source_path = Some(paths[0].as_ref().display().to_string());
    finalize_dorade_volume(&mut volume);
    Ok(volume)
}

/// Decode one sweepfile and append it as a cut on `volume`.
///
/// The first appended sweep populates the site, volume time, and metadata;
/// later sweeps must come from the same instrument.
pub fn append_dorade_sweep(bytes: &[u8], volume: &mut RadarVolume) -> Result<()> {
    let mut parse = SweepParse::new(detect_endian(bytes)?);
    parse.run(bytes, false)?;
    parse.finish_into(volume)
}

/// Sort cuts by elevation and refresh volume-level bookkeeping. Called once
/// after the last [`append_dorade_sweep`].
pub fn finalize_dorade_volume(volume: &mut RadarVolume) {
    // Stable sort: same-elevation cuts (single-tilt surveillance sequences)
    // keep their scan-time order.
    volume
        .cuts
        .sort_by(|left, right| left.elevation_deg.total_cmp(&right.elevation_deg));
    volume.metadata.decoded_radial_count = volume.cuts.iter().map(|cut| cut.radials.len()).sum();
}

fn detect_endian(bytes: &[u8]) -> Result<Endian> {
    if bytes.len() < BLOCK_HEADER_LEN {
        return Err(NexradError::Truncated {
            what: "DORADE block header",
            offset: 0,
            needed: BLOCK_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let le = Endian::Little.i32(bytes, 4);
    let be = Endian::Big.i32(bytes, 4);
    let len = bytes.len() as i64;
    let le_ok = le as i64 >= BLOCK_HEADER_LEN as i64 && le as i64 <= len;
    let be_ok = be as i64 >= BLOCK_HEADER_LEN as i64 && be as i64 <= len;
    match (le_ok, be_ok) {
        (true, false) => Ok(Endian::Little),
        (false, true) => Ok(Endian::Big),
        // Network byte order is the DORADE default; prefer it on a tie.
        (true, true) => Ok(Endian::Big),
        (false, false) => Err(invalid(4, "cannot determine DORADE byte order")),
    }
}

/// One PARM descriptor plus the moment grid it feeds.
struct ParamState {
    name: String,
    description: Option<String>,
    units: Option<String>,
    scale: f32,
    bias: f32,
    bad_data: i32,
    /// DORADE `binary_format`: 1 = i8, 2 = i16, 3 = i32, 4 = f32.
    binary_format: i16,
    /// Extended (1997+) PARM gate metadata; the 104-byte 1995 PARM lacks it.
    number_cells: Option<usize>,
    first_cell_m: Option<f32>,
    cell_spacing_m: Option<f32>,
    moment: MomentType,
    grid: Option<MomentGrid>,
    /// Decoded row for the in-flight ray, if any.
    pending_row: Option<MomentRow>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Cfac {
    azimuth_deg: f32,
    elevation_deg: f32,
    range_delay_m: f32,
    longitude_deg: f32,
    latitude_deg: f32,
    radar_altitude_km: f32,
}

#[derive(Clone, Copy, Debug)]
struct PendingRay {
    azimuth_deg: f32,
    elevation_deg: f32,
    /// RYIB `ray_status`: 0 = normal, 1 = in transition, 2 = bad.
    status: i32,
    time: Option<DateTime<Utc>>,
}

struct SweepParse {
    endian: Endian,
    instrument: String,
    volume_number: i32,
    sweep_number: i32,
    fixed_angle_deg: f32,
    scan_mode: i16,
    compression: i16,
    radd_longitude_deg: f32,
    radd_latitude_deg: f32,
    radd_altitude_km: f32,
    eff_unamb_vel_mps: Option<f32>,
    frequency_ghz: Option<f32>,
    prt1_ms: Option<f32>,
    prt2_ms: Option<f32>,
    num_ipps_trans: Option<i16>,
    cfac: Cfac,
    start_time: Option<DateTime<Utc>>,
    vold_date: Option<NaiveDate>,
    params: Vec<ParamState>,
    /// CELV per-cell ranges or CSFD-derived uniform axis.
    range_first_m: Option<f32>,
    range_spacing_m: Option<f32>,
    range_gate_count: Option<usize>,
    rays: Vec<(PendingRay, Vec<(usize, MomentRow)>)>,
    /// Antenna-transition rays, kept aside so an all-transition sweep can
    /// still decode instead of erroring.
    transition_rays: Vec<(PendingRay, Vec<(usize, MomentRow)>)>,
    current_ray: Option<PendingRay>,
    skipped_field_blocks: usize,
    decoded_cells: usize,
}

impl SweepParse {
    fn new(endian: Endian) -> Self {
        Self {
            endian,
            instrument: String::new(),
            volume_number: 0,
            sweep_number: 0,
            fixed_angle_deg: f32::NAN,
            scan_mode: 8,
            compression: 0,
            radd_longitude_deg: f32::NAN,
            radd_latitude_deg: f32::NAN,
            radd_altitude_km: f32::NAN,
            eff_unamb_vel_mps: None,
            frequency_ghz: None,
            prt1_ms: None,
            prt2_ms: None,
            num_ipps_trans: None,
            cfac: Cfac::default(),
            start_time: None,
            vold_date: None,
            params: Vec::new(),
            range_first_m: None,
            range_spacing_m: None,
            range_gate_count: None,
            rays: Vec::new(),
            transition_rays: Vec::new(),
            current_ray: None,
            skipped_field_blocks: 0,
            decoded_cells: 0,
        }
    }

    /// Latitude with the CFAC correction applied, rejected when it is not a
    /// latitude. See the module docs for the corpus that forced this check.
    fn site_latitude_deg(&self) -> Option<f32> {
        let latitude = self.radd_latitude_deg + self.cfac.latitude_deg;
        (latitude.is_finite() && latitude.abs() <= 90.0).then_some(latitude)
    }

    /// Longitude with the CFAC correction applied, wrapped into ±180 so a
    /// writer using the 0-360 convention still lands in the right place.
    fn site_longitude_deg(&self) -> Option<f32> {
        let longitude = self.radd_longitude_deg + self.cfac.longitude_deg;
        if !longitude.is_finite() || longitude.abs() > 360.0 {
            return None;
        }
        let wrapped = if longitude > 180.0 {
            longitude - 360.0
        } else if longitude < -180.0 {
            longitude + 360.0
        } else {
            longitude
        };
        Some(wrapped)
    }

    fn site_altitude_m(&self) -> Option<f32> {
        let altitude =
            ((self.radd_altitude_km + self.cfac.radar_altitude_km) as f64 * KM_TO_M) as f32;
        altitude.is_finite().then_some(altitude)
    }

    fn run(&mut self, bytes: &[u8], stop_at_first_ray: bool) -> Result<()> {
        let mut pos = 0usize;
        while pos + BLOCK_HEADER_LEN <= bytes.len() {
            let id: [u8; 4] = bytes[pos..pos + 4].try_into().expect("4-byte block id");
            let nbytes = self.endian.i32(bytes, pos + 4);
            if nbytes < BLOCK_HEADER_LEN as i32 {
                // NULL terminator blocks or padding: stop cleanly at a
                // recognizable end marker, error otherwise.
                if &id == b"NULL" || id == [0; 4] {
                    break;
                }
                return Err(invalid(pos, format!("invalid DORADE block size {nbytes}")));
            }
            let end = pos + nbytes as usize;
            if end > bytes.len() {
                // Tolerate a truncated trailing block: real sweepfiles end
                // with an RKTB rotation-angle table whose declared length can
                // overrun the file, and partial downloads cut mid-ray. The
                // descriptor blocks must be complete for a usable sweep.
                if !self.rays.is_empty() {
                    break;
                }
                return Err(NexradError::Truncated {
                    what: "DORADE block",
                    offset: pos,
                    needed: nbytes as usize,
                    available: bytes.len() - pos,
                });
            }
            let block = &bytes[pos..end];
            match &id {
                b"VOLD" => self.parse_vold(block, pos)?,
                b"RADD" => self.parse_radd(block, pos)?,
                b"CFAC" => self.parse_cfac(block),
                b"PARM" => self.parse_parm(block, pos)?,
                b"CELV" => self.parse_celv(block, pos)?,
                b"CSFD" => self.parse_csfd(block, pos)?,
                b"SWIB" => self.parse_swib(block, pos)?,
                b"SSWB" => self.parse_sswb(block, pos)?,
                b"RYIB" => {
                    if stop_at_first_ray {
                        return Ok(());
                    }
                    self.finish_current_ray();
                    self.current_ray = Some(self.parse_ryib(block, pos)?);
                }
                b"RDAT" => self.parse_rdat(block, pos)?,
                // COMM, ASIB, XSTF, RKTB, SEDS, FRIB, FRAD, WAVE, ...: skipped.
                _ => {}
            }
            pos = end;
        }
        self.finish_current_ray();
        Ok(())
    }

    fn parse_vold(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 48, offset, "VOLD")?;
        self.volume_number = i32::from(self.endian.i16(block, 10));
        // Standard layout: proj_name[20] at 16, then year at 36.
        let year = i32::from(self.endian.i16(block, 36));
        let month = self.endian.i16(block, 38);
        let day = self.endian.i16(block, 40);
        let hour = self.endian.i16(block, 42);
        let minute = self.endian.i16(block, 44);
        let second = self.endian.i16(block, 46);
        if let Some(date) = NaiveDate::from_ymd_opt(year, month.max(0) as u32, day.max(0) as u32) {
            self.vold_date = Some(date);
            if self.start_time.is_none() {
                self.start_time = date
                    .and_hms_opt(
                        hour.max(0) as u32,
                        minute.max(0) as u32,
                        second.max(0) as u32,
                    )
                    .map(|naive| Utc.from_utc_datetime(&naive));
            }
        }
        Ok(())
    }

    fn parse_radd(&mut self, block: &[u8], offset: usize) -> Result<()> {
        // The standard 1995 RADD is 144 bytes; Radx writes a 300-byte
        // extended version with identical leading offsets.
        require(block, 144, offset, "RADD")?;
        self.instrument = text(&block[8..16]);
        self.scan_mode = self.endian.i16(block, 50);
        self.compression = self.endian.i16(block, 68);
        self.radd_longitude_deg = self.endian.f32(block, 80);
        self.radd_latitude_deg = self.endian.f32(block, 84);
        self.radd_altitude_km = self.endian.f32(block, 88);
        self.eff_unamb_vel_mps = valid_dorade_f32(self.endian.f32(block, 92));
        self.num_ipps_trans = Some(self.endian.i16(block, 102));
        self.frequency_ghz = valid_dorade_f32(self.endian.f32(block, 104))
            .filter(|frequency| (0.1..=300.0).contains(frequency));
        self.prt1_ms = valid_dorade_f32(self.endian.f32(block, 124));
        self.prt2_ms = valid_dorade_f32(self.endian.f32(block, 128));
        Ok(())
    }

    fn parse_cfac(&mut self, block: &[u8]) {
        // CFAC: correction floats starting at offset 8 (azimuth, elevation,
        // range delay, longitude, latitude, pressure altitude, radar
        // altitude, EW ground speed, NS ground speed, ...).
        if block.len() < 36 {
            return;
        }
        self.cfac = Cfac {
            azimuth_deg: self.endian.f32(block, 8),
            elevation_deg: self.endian.f32(block, 12),
            range_delay_m: self.endian.f32(block, 16),
            longitude_deg: self.endian.f32(block, 20),
            latitude_deg: self.endian.f32(block, 24),
            radar_altitude_km: self.endian.f32(block, 32),
        };
    }

    fn parse_parm(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 104, offset, "PARM")?;
        let name = text(&block[8..16]);
        let description = text(&block[16..56]);
        let units = text(&block[56..64]);
        let binary_format = self.endian.i16(block, 78);
        let scale = self.endian.f32(block, 92);
        let bias = self.endian.f32(block, 96);
        let bad_data = self.endian.i32(block, 100);
        // 1997+ extended PARM (216 bytes) carries per-field gate geometry.
        let (number_cells, first_cell_m, cell_spacing_m) = if block.len() >= 212 {
            let number_cells = self.endian.i32(block, 200).max(0) as usize;
            validate_gate_count(number_cells, offset, "PARM")?;
            (
                Some(number_cells),
                Some(self.endian.f32(block, 204)),
                Some(self.endian.f32(block, 208)),
            )
        } else {
            (None, None, None)
        };
        self.params.push(ParamState {
            name,
            description: (!description.is_empty()).then_some(description),
            units: (!units.is_empty()).then_some(units),
            scale: if scale.abs() > 1.0e-6 { scale } else { 1.0 },
            bias,
            bad_data,
            binary_format,
            number_cells,
            first_cell_m,
            cell_spacing_m,
            moment: MomentType::Unknown(String::new()),
            grid: None,
            pending_row: None,
        });
        Ok(())
    }

    fn parse_celv(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 16, offset, "CELV")?;
        let cells = self.endian.i32(block, 8).max(0) as usize;
        let available = (block.len() - 12) / 4;
        let count = cells.min(available);
        if count == 0 {
            return Ok(());
        }
        validate_gate_count(count, offset, "CELV")?;
        let first = self.endian.f32(block, 12);
        let spacing = if count >= 2 {
            // CELV lists every cell range; radar_core models uniform gates,
            // so use the lead spacing (uniform in the observed corpus).
            self.endian.f32(block, 16) - first
        } else {
            0.0
        };
        self.range_first_m = Some(first);
        self.range_spacing_m = Some(spacing);
        self.range_gate_count = Some(count);
        Ok(())
    }

    fn parse_csfd(&mut self, block: &[u8], offset: usize) -> Result<()> {
        // CSFD: num_segments (i32 at 8), dist_to_first (f32 at 12),
        // spacing[8] (f32 at 16), num_cells[8] (i16 at 48). 64 bytes.
        require(block, 64, offset, "CSFD")?;
        let segments = self.endian.i32(block, 8).clamp(0, 8) as usize;
        if segments == 0 {
            return Ok(());
        }
        let first = self.endian.f32(block, 12);
        let spacing = self.endian.f32(block, 16);
        let mut total_cells = 0usize;
        for segment in 0..segments {
            total_cells += self.endian.i16(block, 48 + segment * 2).max(0) as usize;
        }
        if total_cells == 0 {
            return Ok(());
        }
        validate_gate_count(total_cells, offset, "CSFD")?;
        // Multi-segment geometry flattens to the first segment's spacing;
        // see the module docs.
        self.range_first_m = Some(first);
        self.range_spacing_m = Some(spacing);
        self.range_gate_count = Some(total_cells);
        Ok(())
    }

    fn parse_swib(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 36, offset, "SWIB")?;
        self.sweep_number = self.endian.i32(block, 16);
        self.fixed_angle_deg = self.endian.f32(block, 32);
        Ok(())
    }

    fn parse_sswb(&mut self, block: &[u8], offset: usize) -> Result<()> {
        require(block, 20, offset, "SSWB")?;
        let start = self.endian.i32(block, 12);
        if start > 0 {
            self.start_time = DateTime::<Utc>::from_timestamp(i64::from(start), 0);
        }
        Ok(())
    }

    fn parse_ryib(&mut self, block: &[u8], offset: usize) -> Result<PendingRay> {
        require(block, 44, offset, "RYIB")?;
        let julian_day = self.endian.i32(block, 12);
        let hour = self.endian.i16(block, 16);
        let minute = self.endian.i16(block, 18);
        let second = self.endian.i16(block, 20);
        let millisecond = self.endian.i16(block, 22);
        let time = self.ray_time(julian_day, hour, minute, second, millisecond);
        Ok(PendingRay {
            azimuth_deg: self.endian.f32(block, 24) + self.cfac.azimuth_deg,
            elevation_deg: self.endian.f32(block, 28) + self.cfac.elevation_deg,
            status: self.endian.i32(block, 40),
            time,
        })
    }

    fn ray_time(
        &self,
        julian_day: i32,
        hour: i16,
        minute: i16,
        second: i16,
        millisecond: i16,
    ) -> Option<DateTime<Utc>> {
        let base_year = self
            .start_time
            .map(|time| time.date_naive())
            .or(self.vold_date)?
            .year();
        if !(1..=366).contains(&julian_day) {
            return None;
        }
        let date = NaiveDate::from_yo_opt(base_year, julian_day as u32)?;
        let naive = date.and_hms_milli_opt(
            hour.clamp(0, 23) as u32,
            minute.clamp(0, 59) as u32,
            second.clamp(0, 59) as u32,
            millisecond.clamp(0, 999) as u32,
        )?;
        let mut time = Utc.from_utc_datetime(&naive);
        // Year rollover: a sweep started Dec 31 can have rays on Jan 1.
        if let Some(start) = self.start_time {
            if time < start - Duration::days(180) {
                let next = NaiveDate::from_yo_opt(base_year + 1, julian_day as u32)?;
                time = Utc.from_utc_datetime(&next.and_time(naive.time()));
            } else if time > start + Duration::days(180) {
                let previous = NaiveDate::from_yo_opt(base_year - 1, julian_day as u32)?;
                time = Utc.from_utc_datetime(&previous.and_time(naive.time()));
            }
        }
        Some(time)
    }

    fn parse_rdat(&mut self, block: &[u8], offset: usize) -> Result<()> {
        if self.current_ray.is_none() {
            return Ok(());
        }
        require(block, 16, offset, "RDAT")?;
        let name = text(&block[8..16]);
        let Some(param_index) = self.params.iter().position(|param| param.name == name) else {
            self.skipped_field_blocks += 1;
            return Ok(());
        };
        let payload = &block[16..];
        let endian = self.endian;
        let compressed = self.compression == 1;
        let gate_count = self.gate_count_for_param(param_index);
        if let Some(gates) = gate_count {
            validate_gate_count(gates, offset, "RDAT")?;
        }
        let param = &self.params[param_index];
        let row = match param.binary_format {
            1 => {
                // i8 → u8 storage; +128 keeps (raw − offset)/scale intact.
                let mut row: Vec<u8> = payload
                    .iter()
                    .map(|byte| (*byte as i8 as i16 + 128) as u8)
                    .collect();
                truncate_to_declared(&mut row, gate_count);
                MomentRow::U8(row)
            }
            2 => {
                let mut words: Vec<i16> = payload
                    .chunks_exact(2)
                    .map(|pair| match endian {
                        Endian::Little => i16::from_le_bytes([pair[0], pair[1]]),
                        Endian::Big => i16::from_be_bytes([pair[0], pair[1]]),
                    })
                    .collect();
                words = if compressed {
                    let gates = gate_count.ok_or_else(|| {
                        invalid(
                            offset,
                            format!("no gate count for compressed DORADE field '{name}'"),
                        )
                    })?;
                    decode_hrd_rle(&words, gates, param.bad_data as i16)?
                } else {
                    truncate_to_declared(&mut words, gate_count);
                    words
                };
                // i16 → u16 storage; +32768 keeps (raw − offset)/scale intact.
                MomentRow::U16(
                    words
                        .into_iter()
                        .map(|word| (i32::from(word) + 32768) as u16)
                        .collect(),
                )
            }
            3 => {
                let mut row: Vec<f32> = payload
                    .chunks_exact(4)
                    .map(|quad| {
                        let raw = endian.i32(quad, 0);
                        if raw == param.bad_data {
                            f32::NAN
                        } else {
                            (raw as f32 - param.bias) / param.scale
                        }
                    })
                    .collect();
                truncate_to_declared(&mut row, gate_count);
                MomentRow::F32(row)
            }
            4 => {
                let mut row: Vec<f32> = payload
                    .chunks_exact(4)
                    .map(|quad| {
                        let raw = endian.f32(quad, 0);
                        if raw == param.bad_data as f32 || raw <= DORADE_BAD_F32 {
                            f32::NAN
                        } else {
                            (raw - param.bias) / param.scale
                        }
                    })
                    .collect();
                truncate_to_declared(&mut row, gate_count);
                MomentRow::F32(row)
            }
            _ => {
                // 16-bit float (format 5) is unobserved in the wild corpus;
                // skip the field rather than failing the sweep.
                self.skipped_field_blocks += 1;
                return Ok(());
            }
        };
        let decoded_cells = self
            .decoded_cells
            .checked_add(row.len())
            .ok_or_else(|| invalid(offset, "DORADE decoded-cell count overflow"))?;
        if decoded_cells > MAX_DORADE_CELLS_PER_SWEEP {
            return Err(invalid(
                offset,
                format!("DORADE sweep exceeds the {MAX_DORADE_CELLS_PER_SWEEP}-cell decode limit"),
            ));
        }
        self.decoded_cells = decoded_cells;
        self.params[param_index].pending_row = Some(row);
        Ok(())
    }

    fn gate_count_for_param(&self, param_index: usize) -> Option<usize> {
        self.range_gate_count
            .or_else(|| self.params[param_index].number_cells)
    }

    fn finish_current_ray(&mut self) {
        let Some(ray) = self.current_ray.take() else {
            return;
        };
        let rows: Vec<(usize, MomentRow)> = self
            .params
            .iter_mut()
            .enumerate()
            .filter_map(|(index, param)| param.pending_row.take().map(|row| (index, row)))
            .collect();
        // ray_status: 0 = normal, 1 = antenna in transition, 2 = bad.
        if ray.status != 0 {
            self.transition_rays.push((ray, rows));
        } else {
            self.rays.push((ray, rows));
        }
    }

    fn gate_range(&self) -> Result<GateRange> {
        if let (Some(first), Some(spacing), Some(count)) = (
            self.range_first_m,
            self.range_spacing_m,
            self.range_gate_count,
        ) {
            return Ok(GateRange {
                first_gate_m: (first + self.cfac.range_delay_m).round() as i32,
                gate_spacing_m: spacing.round().max(1.0) as i32,
                gate_count: count,
            });
        }
        let param = self
            .params
            .iter()
            .find(|param| param.number_cells.unwrap_or(0) > 0)
            .ok_or_else(|| invalid(0, "DORADE sweep has no CELV/CSFD/PARM range metadata"))?;
        Ok(GateRange {
            first_gate_m: (param.first_cell_m.unwrap_or(0.0) + self.cfac.range_delay_m).round()
                as i32,
            gate_spacing_m: param.cell_spacing_m.unwrap_or(1000.0).round().max(1.0) as i32,
            gate_count: param.number_cells.unwrap_or(0),
        })
    }

    /// Effective Nyquist (fold) velocity for the recorded velocity field.
    ///
    /// RADD `eff_unamb_vel` is authoritative when present: for staggered-PRT
    /// systems it already holds the extended unambiguous velocity the radar
    /// dealiased to. Otherwise fall back to the wavelength/PRT relations
    /// (Doviak and Zrnić 1993, eq. 3.17; Torres, Dubel, and Zrnić 2004 for
    /// the staggered extension λ/(4·(T2 − T1))).
    fn nyquist_velocity_mps(&self) -> Option<f32> {
        if let Some(value) = self.eff_unamb_vel_mps.filter(|value| *value > 0.0) {
            return Some(value);
        }
        let wavelength_m = 299_792_458.0f32 / (self.frequency_ghz? * 1.0e9);
        let mut prts: Vec<f32> = [self.prt1_ms, self.prt2_ms]
            .into_iter()
            .flatten()
            .filter(|prt| *prt > 0.0)
            .map(|prt| prt / 1000.0)
            .collect();
        prts.sort_by(f32::total_cmp);
        match prts.as_slice() {
            [] => None,
            [short] => Some(wavelength_m / (4.0 * short)),
            [short, long, ..] => {
                if self.num_ipps_trans.unwrap_or(1) >= 2 && (long - short) > f32::EPSILON {
                    Some(wavelength_m / (4.0 * (long - short)))
                } else {
                    Some(wavelength_m / (4.0 * short))
                }
            }
        }
    }

    fn finish_into(mut self, volume: &mut RadarVolume) -> Result<()> {
        let mut skipped_transition_rays = self.transition_rays.len();
        if self.rays.is_empty() {
            if self.transition_rays.is_empty() {
                return Err(invalid(0, "DORADE sweep contains no rays"));
            }
            // All-transition sweep: the status flag is the only thing wrong
            // with the data, so keep it rather than failing the whole volume.
            self.rays = std::mem::take(&mut self.transition_rays);
            skipped_transition_rays = 0;
        }
        if self.instrument.is_empty() {
            self.instrument = "DORADE".to_owned();
        }
        if volume.site.id.is_empty() {
            volume.site = RadarSite {
                id: self.instrument.clone(),
                name: Some(format!("{} (mobile)", self.instrument)),
                latitude_deg: self.site_latitude_deg(),
                longitude_deg: self.site_longitude_deg(),
                elevation_m: self.site_altitude_m(),
            };
            let scan_mode = DoradeScanMode::from_radd(self.scan_mode);
            volume.metadata.archive_version = Some(format!("DORADE {}", scan_mode.as_str()));
            volume.metadata.compression = Some(
                if self.compression == 1 {
                    "dorade-hrd-rle"
                } else {
                    "dorade-uncompressed"
                }
                .to_owned(),
            );
        } else if volume.site.id != self.instrument {
            return Err(invalid(
                0,
                format!(
                    "DORADE sweep instrument '{}' does not match volume '{}'",
                    self.instrument, volume.site.id
                ),
            ));
        }
        let sweep_start = self.start_time;
        if let Some(start) = sweep_start
            && (volume.cuts.is_empty() || start < volume.volume_time)
        {
            volume.volume_time = start;
        }

        let gate_range = self.gate_range()?;
        let nyquist = self.nyquist_velocity_mps();
        let fixed_angle = if self.fixed_angle_deg.is_finite() {
            self.fixed_angle_deg
        } else {
            let sum: f32 = self.rays.iter().map(|(ray, _)| ray.elevation_deg).sum();
            sum / self.rays.len() as f32
        };

        // Map params to canonical moments; first match per type wins, later
        // duplicates (e.g. DOW corrected fields DCZ/VC next to DZ/VE) keep
        // their DORADE name as MomentType::Unknown so nothing is dropped.
        let mut taken: BTreeSet<MomentType> = BTreeSet::new();
        for param in &mut self.params {
            let canonical = canonical_dorade_param_moment(
                &self.instrument,
                &param.name,
                param.description.as_deref(),
            );
            param.moment = match canonical {
                Some(moment) if !taken.contains(&moment) => {
                    taken.insert(moment.clone());
                    moment
                }
                _ => MomentType::Unknown(param.name.clone()),
            };
            param.grid = Some(new_grid(param, gate_range.clone()));
        }

        let elevation_number = u8::try_from(self.sweep_number.clamp(0, 255)).ok();
        let cut = volume.push_cut(fixed_angle, elevation_number);
        cut.radials.reserve(self.rays.len());
        let rays = std::mem::take(&mut self.rays);
        for (ray, rows) in rays {
            let radial_index = cut.radials.len();
            let time_offset_ms = match (ray.time, sweep_start) {
                (Some(time), Some(start)) => (time - start)
                    .num_milliseconds()
                    .clamp(i64::from(i32::MIN), i64::from(i32::MAX))
                    as i32,
                _ => 0,
            };
            cut.radials.push(Radial {
                azimuth_deg: normalize_azimuth(ray.azimuth_deg),
                elevation_deg: ray.elevation_deg,
                time_offset_ms,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: nyquist,
                radial_status: None,
            });
            for (param_index, row) in rows {
                let param = &mut self.params[param_index];
                if let Some(grid) = param.grid.as_mut() {
                    grid.push_row(radial_index, row)?;
                }
            }
        }
        for param in &mut self.params {
            if let Some(grid) = param.grid.take()
                && grid.radial_count() > 0
            {
                cut.moments.insert(grid.moment.clone(), grid);
            }
        }

        volume.metadata.message_count += 1;
        volume.metadata.skipped_message_count +=
            skipped_transition_rays + self.skipped_field_blocks;
        Ok(())
    }
}

/// Drop trailing block padding: DORADE pads a descriptor block to a 4-byte
/// boundary, which can leave one word past the last declared cell.
fn truncate_to_declared<T>(values: &mut Vec<T>, declared: Option<usize>) {
    if let Some(gates) = declared
        && values.len() > gates
    {
        values.truncate(gates);
    }
}

fn new_grid(param: &ParamState, gate_range: GateRange) -> MomentGrid {
    let mut grid = match param.binary_format {
        1 => MomentGrid::new_u8(
            param.moment.clone(),
            gate_range,
            param.scale,
            param.bias + 128.0,
            i32_to_u8_sentinel(param.bad_data),
            None,
        ),
        2 => MomentGrid::new_u16(
            param.moment.clone(),
            gate_range,
            param.scale,
            param.bias + 32768.0,
            i32_to_u16_sentinel(param.bad_data),
            None,
        ),
        // 32-bit fields are scaled during decode, so the grid is a plain
        // float passthrough and bad gates are already NaN.
        _ => MomentGrid {
            moment: param.moment.clone(),
            producer_name: None,
            producer_description: None,
            producer_units: None,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            // DORADE carries no NEXRAD generic data moment header, so there
            // is no censoring threshold or recombination code here.
            snr_threshold_db: None,
            recombination: None,
            radial_indices: Vec::new(),
            storage: MomentStorage::F32(Vec::new()),
        },
    };
    grid.producer_name = Some(param.name.clone());
    grid.producer_description = param.description.clone();
    grid.producer_units = param.units.clone();
    grid
}

fn i32_to_u8_sentinel(bad_data: i32) -> Option<u8> {
    u8::try_from(bad_data + 128).ok()
}

fn i32_to_u16_sentinel(bad_data: i32) -> Option<u16> {
    u16::try_from(i64::from(bad_data) + 32768).ok()
}

/// Map a DORADE parameter name onto the canonical moment set.
///
/// Names come from three generations of writers: solo-era two-letter codes
/// (DZ/VE/SW), long names (DBZ/VEL/WIDTH/RHOHV), and Radx `_F` (filtered) /
/// polarization-suffixed names (DBZHC_F/VEL_F/ZDR_F/RHOHV_F). Sigmet-derived
/// sweepfiles add a `DB_` prefix (DB_ZDR/DB_RHOHV/DB_PHIDP). Affixes are
/// stripped iteratively until a stem matches or none remains, so
/// `DBZHC_F` → `DBZHC` → `DBZ` and `DB_RHOHV` → `RHOHV`.
pub(crate) fn canonical_moment(name: &str) -> Option<MomentType> {
    let normalized = name.trim().to_ascii_uppercase();
    if let Some(moment) = ResearchMoment::from_producer_name(&normalized) {
        return Some(MomentType::Research(moment));
    }
    let mut stem = normalized.as_str();
    loop {
        if let Some(moment) = match_moment_stem(stem) {
            return Some(moment);
        }
        stem = strip_one_affix(stem)?;
    }
}

fn canonical_dorade_param_moment(
    instrument: &str,
    name: &str,
    description: Option<&str>,
) -> Option<MomentType> {
    canonical_moment(name).or_else(|| {
        matches!(instrument, "DOW6" | "DOW7")
            .then(|| exact_dow_description_moment(name, description?))
            .flatten()
    })
}

/// A narrow DOW6/7 exception for producer-described reflectivity fields whose storage
/// key is not their scientific product name.
///
/// In the observed DOW7 sweepfile, `ZH1C`, `ZH2C`, `ZV1C`, and `ZV2C` are the
/// RDAT/PARM keys while the producer wrote the exact modeled product tokens
/// `DBZH1`, `DBZH2`, `DBZV1`, and `DBZV2` into the PARM description. DORADE
/// defines those as separate name and description fields, so treating an exact
/// description as the semantic name is using producer metadata, not deriving
/// a moment from the `ZH1C` spelling.
///
/// The caller enables this only when RADD says exactly DOW6 or DOW7. There is
/// deliberately no fuzzy matching and no rule for the `V*` dBm fields: their
/// descriptions repeat the storage names and do not say which DBMH/DBMV
/// receiver chain they represent. There is also no merged product in this
/// file, so none is invented.
fn exact_dow_description_moment(name: &str, description: &str) -> Option<MomentType> {
    match (name, description) {
        ("ZH1C", "DBZH1") | ("ZH2C", "DBZH2") | ("ZV1C", "DBZV1") | ("ZV2C", "DBZV2") => {
            ResearchMoment::from_producer_name(description).map(MomentType::Research)
        }
        _ => None,
    }
}

fn strip_one_affix(stem: &str) -> Option<&str> {
    if let Some(rest) = stem.strip_prefix("DB_").filter(|rest| !rest.is_empty()) {
        return Some(rest);
    }
    ["_F", "_HC", "_VC", "HC", "_V", "_H"]
        .iter()
        .find_map(|suffix| stem.strip_suffix(suffix).filter(|rest| !rest.is_empty()))
}

fn match_moment_stem(stem: &str) -> Option<MomentType> {
    match stem {
        "DBZ" | "DZ" | "DBZH" | "DBZV" | "REF" | "CZ" | "UZ" => Some(MomentType::Reflectivity),
        "VR" | "VE" | "VEL" | "VU" | "VG" | "VT" => Some(MomentType::Velocity),
        "SW" | "WIDTH" | "SPW" | "SPECTRUM_WIDTH" => Some(MomentType::SpectrumWidth),
        "ZDR" | "ZD" | "UZDR" => Some(MomentType::DifferentialReflectivity),
        "RHOHV" | "RHO" | "RH" | "ROHV" => Some(MomentType::CorrelationCoefficient),
        "PHIDP" | "PHI" | "PH" | "UPHIDP" => Some(MomentType::DifferentialPhase),
        "KDP" | "KD" => Some(MomentType::SpecificDifferentialPhase),
        _ => None,
    }
}

/// Decompress one HRD run-length-encoded 16-bit field row.
///
/// Marker word semantics (DORADE document, "compression scheme" appendix):
/// high bit set → `count` data words follow verbatim; high bit clear →
/// `count` gates of missing data; a bare `1` terminates the row.
fn decode_hrd_rle(words: &[i16], gates: usize, bad_data: i16) -> Result<Vec<i16>> {
    validate_gate_count(gates, 0, "HRD RLE")?;
    let mut out = vec![bad_data; gates];
    let mut input = 0usize;
    let mut output = 0usize;
    while input < words.len() && output < gates {
        let marker = words[input] as u16;
        input += 1;
        let count = (marker & 0x7fff) as usize;
        if marker == 1 {
            // End-of-row sentinel.
            break;
        }
        if count == 0 {
            continue;
        }
        if marker & 0x8000 != 0 {
            let take = count
                .min(gates - output)
                .min(words.len().saturating_sub(input));
            out[output..output + take].copy_from_slice(&words[input..input + take]);
            input += count.min(words.len().saturating_sub(input));
            output += take;
        } else {
            output += count.min(gates - output);
        }
    }
    Ok(out)
}

fn normalize_azimuth(azimuth_deg: f32) -> f32 {
    let normalized = azimuth_deg.rem_euclid(360.0);
    if normalized.is_finite() {
        normalized
    } else {
        0.0
    }
}

fn valid_dorade_f32(value: f32) -> Option<f32> {
    (value.is_finite() && value > DORADE_BAD_F32 && value != 0.0).then_some(value)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_owned()
}

fn require(block: &[u8], needed: usize, offset: usize, what: &'static str) -> Result<()> {
    if block.len() < needed {
        return Err(NexradError::Truncated {
            what,
            offset,
            needed,
            available: block.len(),
        });
    }
    Ok(())
}

fn validate_gate_count(gates: usize, offset: usize, descriptor: &'static str) -> Result<()> {
    if gates > MAX_DORADE_GATES_PER_RADIAL {
        return Err(invalid(
            offset,
            format!(
                "{descriptor} declares {gates} gates per radial (limit {MAX_DORADE_GATES_PER_RADIAL})"
            ),
        ));
    }
    Ok(())
}

fn invalid(offset: usize, reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_i16(block: &mut [u8], offset: usize, value: i16, endian: Endian) {
        let bytes = match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        block[offset..offset + 2].copy_from_slice(&bytes);
    }

    fn put_i32(block: &mut [u8], offset: usize, value: i32, endian: Endian) {
        let bytes = match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        block[offset..offset + 4].copy_from_slice(&bytes);
    }

    fn put_f32(block: &mut [u8], offset: usize, value: f32, endian: Endian) {
        put_i32(block, offset, value.to_bits() as i32, endian);
    }

    fn base_block(id: &[u8; 4], len: usize, endian: Endian) -> Vec<u8> {
        let mut block = vec![0u8; len];
        block[..4].copy_from_slice(id);
        put_i32(&mut block, 4, len as i32, endian);
        block
    }

    struct Synth {
        endian: Endian,
        compressed: bool,
    }

    impl Synth {
        fn build(&self, rays: &[(f32, f32, i32, &[i16])]) -> Vec<u8> {
            let endian = self.endian;
            let mut bytes = Vec::new();

            let mut sswb = base_block(b"SSWB", 200, endian);
            put_i32(&mut sswb, 12, 1_779_404_114, endian); // 2026-05-21T22:55:14Z
            put_i32(&mut sswb, 16, 1_779_404_126, endian);
            bytes.extend(sswb);

            let mut vold = base_block(b"VOLD", 72, endian);
            put_i16(&mut vold, 10, 42, endian); // volume number
            put_i16(&mut vold, 36, 2026, endian);
            put_i16(&mut vold, 38, 5, endian);
            put_i16(&mut vold, 40, 21, endian);
            put_i16(&mut vold, 42, 22, endian);
            put_i16(&mut vold, 44, 55, endian);
            put_i16(&mut vold, 46, 14, endian);
            bytes.extend(vold);

            let mut radd = base_block(b"RADD", 144, endian);
            radd[8..12].copy_from_slice(b"TST1");
            put_i16(&mut radd, 50, 8, endian); // scan mode SUR
            put_i16(&mut radd, 68, if self.compressed { 1 } else { 0 }, endian);
            put_f32(&mut radd, 80, -103.2927, endian); // lon
            put_f32(&mut radd, 84, 39.74, endian); // lat
            put_f32(&mut radd, 88, 1.519, endian); // alt km
            put_f32(&mut radd, 92, 68.76, endian); // eff unamb vel
            put_i16(&mut radd, 102, 2, endian); // num ipps
            put_f32(&mut radd, 104, 5.45, endian); // freq GHz
            put_f32(&mut radd, 124, 0.4, endian); // prt1 ms
            put_f32(&mut radd, 128, 0.6, endian); // prt2 ms
            bytes.extend(radd);

            let mut parm = base_block(b"PARM", 216, endian);
            parm[8..11].copy_from_slice(b"DBZ");
            parm[16..39].copy_from_slice(b"Equivalent reflectivity");
            parm[56..59].copy_from_slice(b"dBZ");
            put_i16(&mut parm, 78, 2, endian); // 16-bit
            put_f32(&mut parm, 92, 100.0, endian); // scale
            put_f32(&mut parm, 96, 0.0, endian); // bias
            put_i32(&mut parm, 100, -32768, endian); // bad
            put_i32(&mut parm, 200, 4, endian); // cells
            put_f32(&mut parm, 204, 50.0, endian);
            put_f32(&mut parm, 208, 100.0, endian);
            bytes.extend(parm);

            let mut csfd = base_block(b"CSFD", 64, endian);
            put_i32(&mut csfd, 8, 1, endian); // one segment
            put_f32(&mut csfd, 12, 50.0, endian); // first cell m
            put_f32(&mut csfd, 16, 100.0, endian); // spacing m
            put_i16(&mut csfd, 48, 4, endian); // cells
            bytes.extend(csfd);

            let mut swib = base_block(b"SWIB", 40, endian);
            put_i32(&mut swib, 16, 6, endian); // sweep number
            put_i32(&mut swib, 20, rays.len() as i32, endian);
            put_f32(&mut swib, 32, 1.0, endian); // fixed angle
            bytes.extend(swib);

            for (azimuth, elevation, status, gates) in rays {
                let mut ryib = base_block(b"RYIB", 44, self.endian);
                put_i32(&mut ryib, 8, 6, endian); // sweep number
                put_i32(&mut ryib, 12, 141, endian); // julian day (May 21)
                put_i16(&mut ryib, 16, 22, endian);
                put_i16(&mut ryib, 18, 55, endian);
                put_i16(&mut ryib, 20, 15, endian);
                put_i16(&mut ryib, 22, 250, endian);
                put_f32(&mut ryib, 24, *azimuth, endian);
                put_f32(&mut ryib, 28, *elevation, endian);
                put_i32(&mut ryib, 40, *status, endian);
                bytes.extend(ryib);

                let words: Vec<i16> = if self.compressed {
                    let mut encoded = Vec::new();
                    encoded.push((0x8000u16 | gates.len() as u16) as i16);
                    encoded.extend_from_slice(gates);
                    encoded.push(1); // end sentinel
                    encoded
                } else {
                    gates.to_vec()
                };
                let mut rdat = base_block(b"RDAT", 16 + words.len() * 2, endian);
                rdat[8..11].copy_from_slice(b"DBZ");
                for (index, word) in words.iter().enumerate() {
                    put_i16(&mut rdat, 16 + index * 2, *word, endian);
                }
                bytes.extend(rdat);
            }
            bytes
        }
    }

    fn synth_rays() -> Vec<(f32, f32, i32, &'static [i16])> {
        vec![
            (45.0, 1.0, 0, &[1000, 2000, -32768, 500][..]),
            (46.0, 1.0, 0, &[1500, -32768, 700, 800][..]),
            (47.0, 9.5, 1, &[1, 2, 3, 4][..]), // transition ray
        ]
    }

    fn find_block(bytes: &[u8], id: &[u8; 4]) -> usize {
        bytes
            .windows(4)
            .position(|window| window == id)
            .expect("block present")
    }

    #[test]
    fn decodes_big_endian_synthetic_sweep() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        assert!(looks_like_dorade_bytes(&bytes));

        let volume = decode_dorade_sweep(&bytes).expect("decode");
        assert_eq!(volume.site.id, "TST1");
        assert_eq!(
            volume.metadata.archive_version.as_deref(),
            Some("DORADE PPI")
        );
        assert_eq!(volume.site.latitude_deg, Some(39.74));
        assert_eq!(volume.site.longitude_deg, Some(-103.2927));
        assert!((volume.site.elevation_m.unwrap() - 1519.0).abs() < 0.5);
        assert_eq!(
            volume.volume_time,
            Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap()
        );
        assert_eq!(volume.cuts.len(), 1);

        let cut = &volume.cuts[0];
        assert_eq!(cut.elevation_deg, 1.0);
        // Transition ray dropped.
        assert_eq!(cut.radials.len(), 2);
        assert_eq!(cut.radials[0].azimuth_deg, 45.0);
        assert_eq!(cut.radials[0].nyquist_velocity_mps, Some(68.76));
        assert_eq!(cut.radials[0].gate_range.first_gate_m, 50);
        assert_eq!(cut.radials[0].gate_range.gate_spacing_m, 100);
        assert_eq!(cut.radials[0].gate_range.gate_count, 4);
        // RYIB time 22:55:15.250 − SSWB start 22:55:14 = 1250 ms.
        assert_eq!(cut.radials[0].time_offset_ms, 1250);

        let grid = cut.moments.get(&MomentType::Reflectivity).expect("DBZ");
        assert_eq!(grid.radial_count(), 2);
        assert_eq!(grid.scaled_value(0, 0), Some(10.0));
        assert_eq!(grid.scaled_value(0, 1), Some(20.0));
        assert_eq!(grid.scaled_value(0, 2), None); // bad gate
        assert_eq!(grid.scaled_value(1, 2), Some(7.0));
        assert_eq!(
            grid.producer_description.as_deref(),
            Some("Equivalent reflectivity")
        );
        assert_eq!(grid.producer_units.as_deref(), Some("dBZ"));
    }

    #[test]
    fn opaque_nvm_keeps_its_parm_description_and_units_without_inference() {
        let mut bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let parm = find_block(&bytes, b"PARM");
        bytes[parm + 8..parm + 16].fill(0);
        bytes[parm + 8..parm + 11].copy_from_slice(b"NVM");
        bytes[parm + 16..parm + 56].fill(0);
        bytes[parm + 16..parm + 36].copy_from_slice(b"Producer-defined NVM");
        bytes[parm + 56..parm + 64].fill(0);
        bytes[parm + 56..parm + 59].copy_from_slice(b"arb");
        let rdat_positions: Vec<usize> = bytes
            .windows(4)
            .enumerate()
            .filter_map(|(index, window)| (window == b"RDAT").then_some(index))
            .collect();
        for rdat in rdat_positions {
            bytes[rdat + 8..rdat + 16].fill(0);
            bytes[rdat + 8..rdat + 11].copy_from_slice(b"NVM");
        }

        let volume = decode_dorade_sweep(&bytes).expect("decode");
        let moment = MomentType::Unknown("NVM".to_owned());
        let grid = volume.cuts[0].moments.get(&moment).expect("opaque NVM");
        assert_eq!(grid.moment, moment);
        assert_eq!(
            grid.producer_description.as_deref(),
            Some("Producer-defined NVM")
        );
        assert_eq!(grid.producer_units.as_deref(), Some("arb"));
    }

    #[test]
    fn decodes_little_endian_rle_sweep() {
        let bytes = Synth {
            endian: Endian::Little,
            compressed: true,
        }
        .build(&synth_rays());
        assert!(looks_like_dorade_bytes(&bytes));

        let volume = decode_dorade_sweep(&bytes).expect("decode");
        let cut = &volume.cuts[0];
        let grid = cut.moments.get(&MomentType::Reflectivity).expect("DBZ");
        assert_eq!(grid.scaled_value(0, 0), Some(10.0));
        assert_eq!(grid.scaled_value(0, 3), Some(5.0));
        assert_eq!(grid.scaled_value(1, 1), None);
    }

    #[test]
    fn rle_run_of_missing_gates_pads_with_bad() {
        // 2 missing gates, then 2 literal words, end sentinel.
        let words = [2i16, (0x8000u16 | 2) as i16, 700, 800, 1];
        let out = decode_hrd_rle(&words, 6, -32768).unwrap();
        assert_eq!(out, vec![-32768, -32768, 700, 800, -32768, -32768]);
    }

    #[test]
    fn uncompressed_rows_drop_trailing_block_padding() {
        // A real VORTEX-2 NOXP sweepfile declares an odd cell count and pads
        // the RDAT block to a 4-byte boundary, leaving one extra word. The
        // grid must keep the declared gate count, not grow to the padded one.
        let mut rays = synth_rays();
        let padded: &'static [i16] = &[1000, 2000, -32768, 500, -32768];
        rays[0] = (45.0, 1.0, 0, padded);
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&rays);

        let volume = decode_dorade_sweep(&bytes).expect("decode");
        let cut = &volume.cuts[0];
        let grid = cut.moments.get(&MomentType::Reflectivity).expect("DBZ");
        assert_eq!(grid.gate_range.gate_count, 4);
        assert_eq!(cut.radials[0].gate_range.gate_count, 4);
        assert_eq!(grid.gate_range, cut.radials[0].gate_range);
    }

    #[test]
    fn out_of_range_latitude_is_reported_as_unknown() {
        // Real VORTEX-2 NOXP sweepfiles write longitude into both slots.
        let mut bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let radd = find_block(&bytes, b"RADD");
        put_f32(&mut bytes[radd..], 80, -99.99996, Endian::Big);
        put_f32(&mut bytes[radd..], 84, 260.00003, Endian::Big);

        let volume = decode_dorade_sweep(&bytes).expect("decode");
        assert_eq!(volume.site.latitude_deg, None);
        assert!((volume.site.longitude_deg.unwrap() + 99.99996).abs() < 1e-4);
    }

    #[test]
    fn zero_to_360_longitude_wraps_into_signed_range() {
        let mut bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let radd = find_block(&bytes, b"RADD");
        put_f32(&mut bytes[radd..], 80, 256.7073, Endian::Big);

        let volume = decode_dorade_sweep(&bytes).expect("decode");
        assert!((volume.site.longitude_deg.unwrap() + 103.2927).abs() < 1e-3);
    }

    #[test]
    fn rejects_extended_parm_with_absurd_gate_count() {
        let mut block = base_block(b"PARM", 216, Endian::Big);
        block[8..11].copy_from_slice(b"DBZ");
        put_i16(&mut block, 78, 2, Endian::Big);
        put_f32(&mut block, 92, 100.0, Endian::Big);
        put_i32(&mut block, 200, i32::MAX, Endian::Big);

        let mut sweep = SweepParse::new(Endian::Big);
        let err = sweep
            .parse_parm(&block, 0)
            .expect_err("absurd gate count must be rejected");
        assert!(err.to_string().contains("gates per radial"));
    }

    #[test]
    fn peek_reads_grouping_metadata_without_rays() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let header = peek_dorade_sweep(&bytes).expect("peek");
        assert_eq!(header.instrument, "TST1");
        assert_eq!(header.volume_number, 42);
        assert_eq!(header.sweep_number, 6);
        assert_eq!(header.fixed_angle_deg, 1.0);
        assert_eq!(header.scan_mode, DoradeScanMode::Ppi);
        assert_eq!(
            header.start_time,
            Some(Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap())
        );
        assert!((header.latitude_deg.unwrap() - 39.74).abs() < 1e-5);
    }

    #[test]
    fn multi_sweep_volume_sorts_cuts_by_elevation() {
        let synth = Synth {
            endian: Endian::Big,
            compressed: false,
        };
        let high = {
            let mut rays = synth_rays();
            for ray in &mut rays {
                ray.1 = 2.4;
            }
            let mut bytes = synth.build(&rays);
            let swib_pos = find_block(&bytes, b"SWIB");
            put_f32(&mut bytes[swib_pos..], 32, 2.4, Endian::Big);
            bytes
        };
        let low = synth.build(&synth_rays());
        let volume = decode_dorade_volume_from_slices(&[high, low]).expect("decode");
        assert_eq!(volume.cuts.len(), 2);
        assert!(volume.cuts[0].elevation_deg < volume.cuts[1].elevation_deg);
        assert_eq!(volume.metadata.decoded_radial_count, 4);
    }

    #[test]
    fn mismatched_instruments_are_rejected() {
        let synth = Synth {
            endian: Endian::Big,
            compressed: false,
        };
        let first = synth.build(&synth_rays());
        let mut second = synth.build(&synth_rays());
        let radd_pos = find_block(&second, b"RADD");
        second[radd_pos + 8..radd_pos + 12].copy_from_slice(b"TST2");
        let err = decode_dorade_volume_from_slices(&[first, second]).unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn rhi_scan_mode_is_detected_from_radd() {
        // An RHI sweep: RADD scan mode 3, fixed azimuth, elevation-swept rays.
        let rays: Vec<(f32, f32, i32, &[i16])> = vec![
            (271.0, 0.5, 0, &[1000, 2000, 1500, 500][..]),
            (271.0, 1.5, 0, &[1500, 1200, 700, 800][..]),
            (271.0, 2.5, 0, &[900, 1100, 600, 400][..]),
        ];
        let mut bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&rays);
        let radd_pos = find_block(&bytes, b"RADD");
        put_i16(&mut bytes[radd_pos..], 50, 3, Endian::Big); // RHI per DORADE doc
        let volume = decode_dorade_sweep(&bytes).expect("decode");
        assert_eq!(
            volume.metadata.archive_version.as_deref(),
            Some("DORADE RHI")
        );
        assert_eq!(
            peek_dorade_sweep(&bytes).unwrap().scan_mode,
            DoradeScanMode::Rhi
        );
        // Per-radial elevations carry the sweep; azimuth is fixed.
        let cut = &volume.cuts[0];
        assert_eq!(cut.radials.len(), 3);
        assert!(cut.radials.iter().all(|r| r.azimuth_deg == 271.0));
        assert_eq!(cut.radials[0].elevation_deg, 0.5);
        assert_eq!(cut.radials[2].elevation_deg, 2.5);
    }

    #[test]
    fn radd_scan_mode_codes_map_to_the_documented_enum() {
        // Codes per Oye & Case 1995 / lrose DoradeData.hh.
        assert_eq!(DoradeScanMode::from_radd(1), DoradeScanMode::Ppi); // PPI sector
        assert_eq!(DoradeScanMode::from_radd(8), DoradeScanMode::Ppi); // SUR
        assert_eq!(DoradeScanMode::from_radd(3), DoradeScanMode::Rhi);
        assert_eq!(
            DoradeScanMode::from_radd(4),
            DoradeScanMode::VerticalPointing
        );
        for other in [0i16, 2, 5, 6, 7, 9, 10, 99] {
            assert_eq!(DoradeScanMode::from_radd(other), DoradeScanMode::Other);
        }
    }

    #[test]
    fn canonical_moment_maps_observed_corpus_names() {
        // COW2 (Radx _F names), RaXPol, DOW7 solo-era names, NOXP DB_ names.
        assert_eq!(canonical_moment("DBZHC_F"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("VEL_F"), Some(MomentType::Velocity));
        assert_eq!(
            canonical_moment("ZDR_F"),
            Some(MomentType::DifferentialReflectivity)
        );
        assert_eq!(
            canonical_moment("RHOHV_F"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(canonical_moment("DBZ"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("WIDTH"), Some(MomentType::SpectrumWidth));
        assert_eq!(canonical_moment("DZ"), Some(MomentType::Reflectivity));
        assert_eq!(canonical_moment("VE"), Some(MomentType::Velocity));
        assert_eq!(canonical_moment("VR"), Some(MomentType::Velocity));
        assert_eq!(canonical_moment("SW"), Some(MomentType::SpectrumWidth));
        assert_eq!(
            canonical_moment("DB_ZDR"),
            Some(MomentType::DifferentialReflectivity)
        );
        assert_eq!(
            canonical_moment("DB_RHOHV"),
            Some(MomentType::CorrelationCoefficient)
        );
        assert_eq!(
            canonical_moment("DB_PHIDP"),
            Some(MomentType::DifferentialPhase)
        );
        assert_eq!(canonical_moment("NCP"), None);
        assert_eq!(canonical_moment("DM"), None);
        // The DOW field references used here do not define NVM. Preserve it as
        // `Unknown("NVM")` at the decode call site rather than expanding the
        // mnemonic into a quantity the producer did not document here.
        assert_eq!(canonical_moment("NVM"), None);
    }

    #[test]
    fn canonical_moment_keeps_dow_frequency_products_separate() {
        let names = [
            "DBMH1", "DBMH2", "DBMHM", "DBMV1", "DBMV2", "DBMVM", "DBZH1", "DBZH2", "DBZHM",
            "DBZV1", "DBZV2", "DBZVM",
        ];
        let moments: Vec<MomentType> = names
            .iter()
            .map(|name| canonical_moment(name).expect("known research product"))
            .collect();
        let unique: BTreeSet<MomentType> = moments.iter().cloned().collect();
        assert_eq!(unique.len(), names.len());
        for (name, moment) in names.iter().zip(moments) {
            assert_eq!(moment.short_name(), *name);
        }
        assert_ne!(
            canonical_moment("DBZH1"),
            Some(MomentType::Reflectivity),
            "a second DOW reflectivity must not collide with the first"
        );
    }

    #[test]
    fn only_exact_dow6_or_dow7_descriptions_promote_reflectivity_storage_names() {
        let h1 = canonical_moment("DBZH1").expect("modeled DOW field");
        assert_eq!(
            canonical_dorade_param_moment("DOW7", "ZH1C", Some("DBZH1")),
            Some(h1.clone())
        );
        assert_eq!(
            canonical_dorade_param_moment("DOW6", "ZH1C", Some("DBZH1")),
            Some(h1)
        );

        // A description is not a global alias table. Both the instrument and
        // the producer token must be exact before it changes field identity.
        assert_eq!(
            canonical_dorade_param_moment("UMass-XP", "ZH1C", Some("DBZH1")),
            None
        );
        assert_eq!(
            canonical_dorade_param_moment("DOW8", "ZH1C", Some("DBZH1")),
            None
        );
        assert_eq!(
            canonical_dorade_param_moment("DOW7", "ZH1C", Some("dbzh1")),
            None
        );
        assert_eq!(
            canonical_dorade_param_moment("DOW7", "V1", Some("V1")),
            None,
            "a dBm unit does not say which DBMH/DBMV receiver chain V1 is"
        );
        assert_eq!(
            canonical_dorade_param_moment("DOW7", "V1", Some("DBMH1")),
            None,
            "the measured rule is the exact reflectivity name/description pair"
        );
    }

    /// Manual pin for the public/field sample that exposed the split between
    /// the DORADE storage name and producer description. Kept ignored because
    /// the multi-megabyte sweep is not part of the repository.
    #[ignore = "set DOW_DORADE_SAMPLE to the real DOW7 sweepfile"]
    #[test]
    fn real_dow7_reflectivity_descriptions_are_exact_products() {
        let path = std::env::var("DOW_DORADE_SAMPLE")
            .expect("set DOW_DORADE_SAMPLE to a real DOW7 DORADE sweep");
        let bytes = std::fs::read(&path).expect("read DOW_DORADE_SAMPLE");
        let volume = decode_dorade_sweep(&bytes).expect("decode DOW7 sweep");
        assert_eq!(volume.site.id, "DOW7");
        let cut = volume.cuts.first().expect("one decoded sweep");

        for (storage_name, product_name) in [
            ("ZH1C", "DBZH1"),
            ("ZH2C", "DBZH2"),
            ("ZV1C", "DBZV1"),
            ("ZV2C", "DBZV2"),
        ] {
            let moment = canonical_moment(product_name).expect("modeled DOW product");
            let grid = cut
                .moments
                .get(&moment)
                .unwrap_or_else(|| panic!("{storage_name}/{product_name} was not promoted"));
            assert_eq!(grid.producer_description.as_deref(), Some(product_name));
            assert_eq!(grid.producer_units.as_deref(), Some("none"));
            assert!(
                !cut.moments
                    .contains_key(&MomentType::Unknown(storage_name.to_owned())),
                "the promoted field also survived under a second identity"
            );
        }

        for name in [
            "NCP1", "NCP2", "V1", "V2", "VL1", "VL1_CRR", "VL2", "VL2_CRR", "VS1", "VS1_CRR",
            "VS2", "VS2_CRR",
        ] {
            assert!(
                cut.moments
                    .contains_key(&MomentType::Unknown(name.to_owned())),
                "{name} was inferred into a product the file did not name"
            );
        }
        for absent in [
            "DBZHM", "DBZVM", "DBMH1", "DBMH2", "DBMHM", "DBMV1", "DBMV2", "DBMVM",
        ] {
            let moment = canonical_moment(absent).expect("modeled DOW product");
            assert!(
                !cut.moments.contains_key(&moment),
                "{absent} was fabricated from a field that did not name it"
            );
        }
    }

    #[test]
    fn duplicate_canonical_names_keep_original_field() {
        // DOW7 carries DZ (raw) and DCZ/VC (corrected); first match wins and
        // later candidates stay addressable under their DORADE names.
        let mut taken = BTreeSet::new();
        let mut resolved = Vec::new();
        for name in ["DZ", "DCZ", "VE", "VC"] {
            let canonical = canonical_moment(name);
            let moment = match canonical {
                Some(moment) if !taken.contains(&moment) => {
                    taken.insert(moment.clone());
                    moment
                }
                _ => MomentType::Unknown(name.to_owned()),
            };
            resolved.push(moment);
        }
        assert_eq!(resolved[0], MomentType::Reflectivity);
        assert_eq!(resolved[1], MomentType::Unknown("DCZ".to_owned()));
        assert_eq!(resolved[2], MomentType::Velocity);
        assert_eq!(resolved[3], MomentType::Unknown("VC".to_owned()));
    }

    #[test]
    fn u16_grids_preserve_dorade_scaling() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&synth_rays());
        let volume = decode_dorade_sweep(&bytes).expect("decode");
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .unwrap();
        assert!(matches!(grid.storage, MomentStorage::U16(_)));
        assert_eq!(grid.scale, 100.0);
        assert_eq!(grid.offset, 32768.0);
        assert_eq!(grid.nodata, Some(0));
    }

    #[test]
    fn sweep_without_rays_is_an_error() {
        let bytes = Synth {
            endian: Endian::Big,
            compressed: false,
        }
        .build(&[]);
        let err = decode_dorade_sweep(&bytes).unwrap_err();
        assert!(err.to_string().contains("no rays"));
    }

    #[test]
    fn dorade_name_sniffing_matches_the_sweepfile_convention() {
        assert!(looks_like_dorade_name(
            "DORADE/COW2/swp.1260516225229.COW2.515.1.0_SUR_v237"
        ));
        assert!(looks_like_dorade_name("scan.swp"));
        assert!(!looks_like_dorade_name("README.txt"));
        assert!(looks_like_dorade_path(Path::new(
            "c:/data/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1"
        )));
    }
}
