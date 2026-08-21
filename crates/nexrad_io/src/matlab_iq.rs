//! MATLAB Level 5 reader for OU-PRIME I/Q cubes.
//!
//! Level 5 MAT-files are tagged binary streams. MATLAB v7 wraps individual
//! `miMATRIX` elements in zlib-compressed `miCOMPRESSED` elements; it is not
//! the HDF5 container used by MATLAB v7.3. This module walks those tags rather
//! than treating the filename as evidence of the format.
//!
//! The OU-PRIME cubes carry receiver samples and acquisition geometry, but no
//! reflectivity calibration or noise reference. Consequently this decoder
//! returns the samples in their stored, relative units. It deliberately has
//! no conversion to dBZ.
//!
//! Format reference: MathWorks, *MAT-File Format* (Level 5 file format),
//! <https://www.mathworks.com/help/pdf_doc/matlab/matfile_format.pdf>.
//! Radar/event reference: Palmer et al. 2011, *Bulletin of the American
//! Meteorological Society* 92, 871-891, doi:10.1175/2011BAMS3125.1.

use std::collections::BTreeSet;
use std::io::Read;

use chrono::{TimeZone, Utc};
use flate2::read::{GzDecoder, ZlibDecoder};
use thiserror::Error;

use crate::iq::{DopplerPhaseConvention, IqCalibration, IqPulse, IqSweep, PulseLayout, PulseSpan};

const HEADER_LEN: usize = 128;
const HEADER_TEXT_LEN: usize = 116;
const LEVEL5_MAGIC: &[u8] = b"MATLAB 5.0 MAT-file";
const LEVEL5_VERSION: u16 = 0x0100;

const MI_INT8: u32 = 1;
const MI_UINT8: u32 = 2;
const MI_INT16: u32 = 3;
const MI_UINT16: u32 = 4;
const MI_INT32: u32 = 5;
const MI_UINT32: u32 = 6;
const MI_SINGLE: u32 = 7;
const MI_DOUBLE: u32 = 9;
const MI_INT64: u32 = 12;
const MI_UINT64: u32 = 13;
const MI_MATRIX: u32 = 14;
const MI_COMPRESSED: u32 = 15;
const MI_UTF8: u32 = 16;
const MI_UTF16: u32 = 17;
const MI_UTF32: u32 = 18;

const MX_CHAR_CLASS: u8 = 4;
const MX_DOUBLE_CLASS: u8 = 6;
const MX_UINT64_CLASS: u8 = 15;
const ARRAY_FLAG_COMPLEX: u32 = 0x0800;

const MAX_COMPRESSION_DEPTH: usize = 8;
const MAX_DECOMPRESSED_BYTES: usize = 512 * 1024 * 1024;
const MAX_GZIP_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;
const MAX_ARRAY_ELEMENTS: usize = 64 * 1024 * 1024;
const MAX_INITIAL_DECOMPRESSED_CAPACITY: usize = 16 * 1024 * 1024;
const MAX_DIMENSIONS: usize = 16;
const MAX_VARIABLES: usize = 4096;
const MAX_NAME_BYTES: usize = 1024;
const MAX_CHARACTER_UNITS: usize = 64 * 1024;

// The public OU-PRIME sample is 150 rays x 960 gates x 32 pulses, or
// 4,608,000 complex samples per receiver channel. These ceilings leave ample
// room for larger research scans while keeping malformed metadata from
// turning a small compressed input into multi-gigabyte owned allocations.
const MAX_AZIMUTH_COUNT: usize = 16 * 1024;
const MAX_GATE_COUNT: usize = 64 * 1024;
const MAX_PULSE_COUNT: usize = 4 * 1024;
const MAX_FLATTENED_PULSES: usize = 1024 * 1024;
const MAX_IQ_SAMPLES_PER_CHANNEL: usize = 16 * 1024 * 1024;
const MAX_OWNED_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// Convert OU-PRIME's recorded `az_set` coordinate to true-north azimuth.
///
/// Measured against the matching processed sweep
/// `swp.1100510224710.OU-PRIME.0.0.2_SUR_v020` in ARRC's public 10 May 2010
/// DORADE archive (<https://arrc.ou.edu/data.html>): correlating the cube's
/// range-corrected relative power with processed reflectivity gives
/// `azimuth = 216.252° - az_set`. The companion is the same 22:47 scan, not a
/// generic site bearing or a value inferred from the filename.
const OU_PRIME_AZ_SET_SUM_DEG: f64 = 216.252;

/// A complex receiver sample stored as a pair of single-precision values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex32 {
    pub re: f32,
    pub im: f32,
}

/// One OU-PRIME scan decoded from a MATLAB Level 5 MAT-file.
///
/// `horizontal` and `vertical` retain MATLAB's column-major ordering for a
/// cube shaped `[azimuth, gate, pulse]`. Use [`Self::sample_index`] rather
/// than assuming a Rust row-major layout.
#[derive(Clone, Debug, PartialEq)]
pub struct OuPrimeIqCube {
    pub radar: String,
    /// `[year, month, day, hour, minute, second]` in UTC.
    pub scan_time_utc: [u16; 6],
    pub wavelength_m: f64,
    pub elevation_deg: f64,
    /// Instrument `az_set` coordinates as stored in the MAT file. These are
    /// not true-north bearings; [`Self::into_iq_sweep`] applies the sourced
    /// OU-PRIME pedestal transform.
    pub azimuths_deg: Vec<f64>,
    pub gate_spacing_m: f64,
    pub first_gate_m: f64,
    pub pri_seconds: f64,
    pub propagation_speed_m_s: f64,
    pub azimuth_count: usize,
    pub gate_count: usize,
    pub pulse_count: usize,
    /// Uncalibrated horizontal receiver samples in stored, relative units.
    pub horizontal: Vec<Complex32>,
    /// Uncalibrated vertical receiver samples in stored, relative units.
    pub vertical: Vec<Complex32>,
}

impl OuPrimeIqCube {
    /// Linear index for `[azimuth, gate, pulse]` in the stored MATLAB order.
    #[must_use]
    pub fn sample_index(&self, azimuth: usize, gate: usize, pulse: usize) -> Option<usize> {
        if azimuth >= self.azimuth_count || gate >= self.gate_count || pulse >= self.pulse_count {
            return None;
        }

        let gate_and_pulse = self.gate_count.checked_mul(pulse)?.checked_add(gate)?;
        self.azimuth_count
            .checked_mul(gate_and_pulse)?
            .checked_add(azimuth)
    }

    #[must_use]
    pub fn horizontal_sample(
        &self,
        azimuth: usize,
        gate: usize,
        pulse: usize,
    ) -> Option<Complex32> {
        self.sample_index(azimuth, gate, pulse)
            .and_then(|index| self.horizontal.get(index).copied())
    }

    #[must_use]
    pub fn vertical_sample(&self, azimuth: usize, gate: usize, pulse: usize) -> Option<Complex32> {
        self.sample_index(azimuth, gate, pulse)
            .and_then(|index| self.vertical.get(index).copied())
    }

    /// Pulse repetition frequency derived from the stored PRI.
    #[must_use]
    pub fn prf_hz(&self) -> f64 {
        1.0 / self.pri_seconds
    }

    /// Uniform-PRT Nyquist velocity, `lambda / (4 * PRI)`.
    #[must_use]
    pub fn nyquist_velocity_m_s(&self) -> f64 {
        self.wavelength_m / (4.0 * self.pri_seconds)
    }

    #[must_use]
    pub fn gate_range_m(&self, gate: usize) -> Option<f64> {
        (gate < self.gate_count).then_some(self.first_gate_m + self.gate_spacing_m * gate as f64)
    }

    /// Convert the measured cube into the common I/Q processing contract.
    ///
    /// MATLAB stores `[azimuth, gate, pulse]` column-major. The common model is
    /// a pulse stream, so this emits one contiguous native-pulse span per
    /// azimuth in ray-major order and records every boundary. The source
    /// contains no radar constant or receiver-noise measurement; that absence
    /// is preserved as [`IqCalibration::RelativeStoredIq`].
    pub fn into_iq_sweep(self) -> Result<IqSweep> {
        let [year, month, day, hour, minute, second] = self.scan_time_utc;
        let start = Utc
            .with_ymd_and_hms(
                i32::from(year),
                u32::from(month),
                u32::from(day),
                u32::from(hour),
                u32::from(minute),
                u32::from(second),
            )
            .single()
            .ok_or_else(|| invalid_variable("scan_time", "is not a valid UTC timestamp"))?;

        let wavelength_m = finite_f32_value("lambda", self.wavelength_m)?;
        let elevation_deg = finite_f32_value("el", self.elevation_deg)?;
        let gate_spacing_m = finite_f32_value("delr", self.gate_spacing_m)?;
        let first_gate_m = finite_f32_value("r_min", self.first_gate_m)?;
        let pri_seconds = finite_f32_value("pri", self.pri_seconds)?;
        let (total_pulses, sample_count) =
            validate_iq_dimensions(self.azimuth_count, self.gate_count, self.pulse_count)?;
        if self.azimuths_deg.len() != self.azimuth_count {
            return Err(invalid_variable(
                "az_set",
                format!(
                    "contains {} azimuths but num_az is {}",
                    self.azimuths_deg.len(),
                    self.azimuth_count
                ),
            ));
        }
        if self.horizontal.len() != sample_count || self.vertical.len() != sample_count {
            return Err(invalid_variable(
                "X_h/X_v",
                format!(
                    "receiver cubes contain {} and {} samples; dimensions require {sample_count}",
                    self.horizontal.len(),
                    self.vertical.len()
                ),
            ));
        }

        let mut spans = Vec::new();
        spans
            .try_reserve_exact(self.azimuth_count)
            .map_err(|_| MatlabIqError::Allocation {
                what: "native ray spans",
                elements: self.azimuth_count,
            })?;
        let mut pulses = Vec::new();
        pulses
            .try_reserve_exact(total_pulses)
            .map_err(|_| MatlabIqError::Allocation {
                what: "flattened I/Q pulses",
                elements: total_pulses,
            })?;
        for azimuth in 0..self.azimuth_count {
            let span_start = pulses.len();
            let azimuth_deg = finite_f32_value(
                "az_set",
                (OU_PRIME_AZ_SET_SUM_DEG - self.azimuths_deg[azimuth]).rem_euclid(360.0),
            )?;
            for pulse in 0..self.pulse_count {
                let flattened = span_start + pulse;
                let elapsed_millis = (flattened as f64 * self.pri_seconds * 1000.0).round() as i64;
                let pulse_time_utc = start.timestamp() + elapsed_millis.div_euclid(1000);
                let pulse_millis = elapsed_millis.rem_euclid(1000) as u16;

                let mut h = Vec::new();
                h.try_reserve_exact(self.gate_count)
                    .map_err(|_| MatlabIqError::Allocation {
                        what: "horizontal pulse samples",
                        elements: self.gate_count,
                    })?;
                let mut v = Vec::new();
                v.try_reserve_exact(self.gate_count)
                    .map_err(|_| MatlabIqError::Allocation {
                        what: "vertical pulse samples",
                        elements: self.gate_count,
                    })?;
                for gate in 0..self.gate_count {
                    let index = azimuth + self.azimuth_count * (gate + self.gate_count * pulse);
                    let horizontal = self.horizontal[index];
                    let vertical = self.vertical[index];
                    h.push((horizontal.re, horizontal.im));
                    v.push((vertical.re, vertical.im));
                }
                pulses.push(IqPulse {
                    azimuth_deg,
                    elevation_deg,
                    prt_seconds: pri_seconds,
                    prt_previous_seconds: pri_seconds,
                    time_utc: pulse_time_utc,
                    time_millis: pulse_millis,
                    burst: None,
                    h,
                    v,
                });
            }
            spans.push(PulseSpan {
                start: span_start,
                len: self.pulse_count,
            });
        }

        let mut range_bins = Vec::new();
        range_bins
            .try_reserve_exact(self.gate_count)
            .map_err(|_| MatlabIqError::Allocation {
                what: "range bins",
                elements: self.gate_count,
            })?;
        range_bins
            .extend((0..self.gate_count).map(|gate| first_gate_m + gate_spacing_m * gate as f32));

        Ok(IqSweep {
            site: self.radar,
            task_name: String::new(),
            processor_version: String::new(),
            time_utc: start.timestamp(),
            time_millis: 0,
            wavelength_m,
            // `delr` is a sampling interval, not proof of a transmitted pulse
            // width, so the latter remains absent.
            pulse_width_s: None,
            polarization_code: None,
            channels_recorded: 2,
            burst_samples: 0,
            major_mode: None,
            nominal_sample_size: self.pulse_count as i64,
            range_mask_res_m: gate_spacing_m,
            gate_spacing_m: Some(gate_spacing_m),
            first_gate_m,
            range_bins,
            calibration: IqCalibration::RelativeStoredIq,
            pulse_layout: PulseLayout::Rays(spans),
            // Pinned against ARRC's processed DORADE sweep beginning
            // 2010-05-10 22:47:10 UTC, the public companion measurement for
            // this 22:47:11 cube. See `tests/matlab_iq_real.rs`; using the RVP
            // convention makes the real weather-motion comparison worse by
            // several metres per second rather than merely changing a
            // synthetic tone's expected sign.
            doppler_phase_convention: DopplerPhaseConvention::PositiveLagPhaseIsNegativeVelocity,
            pulses,
        })
    }
}

fn finite_f32_value(name: &'static str, value: f64) -> Result<f32> {
    let narrowed = value as f32;
    if narrowed.is_finite() {
        Ok(narrowed)
    } else {
        Err(invalid_variable(
            name,
            format!("{value} cannot be represented as a finite f32"),
        ))
    }
}

pub type Result<T> = std::result::Result<T, MatlabIqError>;

#[derive(Debug, Error)]
pub enum MatlabIqError {
    #[error("input is too short for a MATLAB Level 5 header: {actual} bytes")]
    ShortHeader { actual: usize },
    #[error("input does not have the MATLAB Level 5 signature")]
    InvalidSignature,
    #[error("MATLAB Level 5 header has an invalid endian indicator")]
    InvalidEndianIndicator,
    #[error("unsupported MATLAB Level 5 version 0x{version:04x}")]
    UnsupportedVersion { version: u16 },
    #[error("truncated {what} at offset {offset}: need {needed} bytes, have {available}")]
    Truncated {
        what: &'static str,
        offset: usize,
        needed: usize,
        available: usize,
    },
    #[error("invalid MAT data element at offset {offset}: {reason}")]
    InvalidElement { offset: usize, reason: String },
    #[error("MAT zlib element could not be decompressed: {0}")]
    Compression(String),
    #[error("gzip-wrapped MAT could not be decompressed: {0}")]
    GzipCompression(String),
    #[error("MAT input exceeds the {what} limit of {limit}")]
    Limit { what: &'static str, limit: usize },
    #[error("duplicate MAT variable {name:?}")]
    DuplicateVariable { name: String },
    #[error("required OU-PRIME variable {name:?} is missing")]
    MissingVariable { name: &'static str },
    #[error("invalid OU-PRIME variable {name:?}: {reason}")]
    InvalidVariable { name: String, reason: String },
    #[error("could not allocate {elements} elements for {what}")]
    Allocation { what: &'static str, elements: usize },
}

/// Whether `bytes` has a complete MATLAB Level 5 header and endian marker.
#[must_use]
pub fn is_matlab_level5(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN
        && bytes[..HEADER_TEXT_LEN].starts_with(LEVEL5_MAGIC)
        && matches!(&bytes[126..128], b"IM" | b"MI")
}

/// Decode an OU-PRIME I/Q cube from a MATLAB Level 5 MAT-file.
///
/// Both uncompressed `miMATRIX` elements and MATLAB v7 zlib-wrapped
/// `miCOMPRESSED` elements are accepted. Unknown numeric or character
/// variables are validated and ignored, permitting producers to add metadata
/// without changing the decoder contract.
pub fn decode_ou_prime_mat(bytes: &[u8]) -> Result<OuPrimeIqCube> {
    let inflated;
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        inflated = decompress_gzip_limited(bytes, MAX_GZIP_DECOMPRESSED_BYTES)?;
        inflated.as_slice()
    } else {
        bytes
    };

    let endian = parse_header(bytes)?;
    let mut builder = OuPrimeBuilder::default();
    let mut decompressed_bytes = 0;
    parse_element_stream(
        &bytes[HEADER_LEN..],
        endian,
        0,
        &mut decompressed_bytes,
        &mut builder,
    )?;
    builder.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: &[u8]) -> u16 {
        let bytes = [bytes[0], bytes[1]];
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn i16(self, bytes: &[u8]) -> i16 {
        self.u16(bytes) as i16
    }

    fn u32(self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }

    fn i32(self, bytes: &[u8]) -> i32 {
        self.u32(bytes) as i32
    }

    fn u64(self, bytes: &[u8]) -> u64 {
        let bytes = [
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ];
        match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        }
    }

    fn i64(self, bytes: &[u8]) -> i64 {
        self.u64(bytes) as i64
    }

    fn f32(self, bytes: &[u8]) -> f32 {
        f32::from_bits(self.u32(bytes))
    }

    fn f64(self, bytes: &[u8]) -> f64 {
        f64::from_bits(self.u64(bytes))
    }
}

fn parse_header(bytes: &[u8]) -> Result<Endian> {
    if bytes.len() < HEADER_LEN {
        return Err(MatlabIqError::ShortHeader {
            actual: bytes.len(),
        });
    }
    if !bytes[..HEADER_TEXT_LEN].starts_with(LEVEL5_MAGIC) {
        return Err(MatlabIqError::InvalidSignature);
    }

    let endian = match &bytes[126..128] {
        b"IM" => Endian::Little,
        b"MI" => Endian::Big,
        _ => return Err(MatlabIqError::InvalidEndianIndicator),
    };
    let version = endian.u16(&bytes[124..126]);
    if version != LEVEL5_VERSION {
        return Err(MatlabIqError::UnsupportedVersion { version });
    }
    Ok(endian)
}

#[derive(Debug)]
struct Element<'a> {
    data_type: u32,
    data: &'a [u8],
    offset: usize,
    raw_end: usize,
    aligned_end: usize,
    small: bool,
}

fn read_element(bytes: &[u8], offset: usize, endian: Endian) -> Result<Element<'_>> {
    let available = bytes.len().saturating_sub(offset);
    if available < 8 {
        return Err(MatlabIqError::Truncated {
            what: "MAT data-element tag",
            offset,
            needed: 8,
            available,
        });
    }

    let tag_bytes = &bytes[offset..offset + 4];
    let regular_data_type = endian.u32(tag_bytes);
    if !known_data_type(regular_data_type) {
        // A small-data-element tag is two independently endian-encoded u16
        // fields: data type followed by byte count. Treating the pair as one
        // u32 happens to work in little endian but swaps the fields in big
        // endian MAT files.
        let data_type = u32::from(endian.u16(&tag_bytes[..2]));
        let small_len = usize::from(endian.u16(&tag_bytes[2..]));
        if small_len > 4 {
            return Err(invalid_element(
                offset,
                format!("small-data element declares {small_len} bytes; maximum is 4"),
            ));
        }
        if !known_data_type(data_type) {
            return Err(invalid_element(
                offset,
                format!("unknown small-data type {data_type}"),
            ));
        }
        return Ok(Element {
            data_type,
            data: &bytes[offset + 4..offset + 4 + small_len],
            offset,
            raw_end: offset + 8,
            aligned_end: offset + 8,
            small: true,
        });
    }

    let data_type = regular_data_type;
    let byte_count = endian.u32(&bytes[offset + 4..offset + 8]) as usize;
    let data_start = offset
        .checked_add(8)
        .ok_or_else(|| invalid_element(offset, "data offset overflow"))?;
    let raw_end = data_start
        .checked_add(byte_count)
        .ok_or_else(|| invalid_element(offset, "data length overflow"))?;
    if raw_end > bytes.len() {
        return Err(MatlabIqError::Truncated {
            what: "MAT data element",
            offset: data_start,
            needed: byte_count,
            available: bytes.len().saturating_sub(data_start),
        });
    }
    let padding = (8 - byte_count % 8) % 8;
    let aligned_end = raw_end
        .checked_add(padding)
        .ok_or_else(|| invalid_element(offset, "aligned data length overflow"))?;

    Ok(Element {
        data_type,
        data: &bytes[data_start..raw_end],
        offset,
        raw_end,
        aligned_end,
        small: false,
    })
}

fn known_data_type(data_type: u32) -> bool {
    matches!(
        data_type,
        MI_INT8
            | MI_UINT8
            | MI_INT16
            | MI_UINT16
            | MI_INT32
            | MI_UINT32
            | MI_SINGLE
            | MI_DOUBLE
            | MI_INT64
            | MI_UINT64
            | MI_MATRIX
            | MI_COMPRESSED
            | MI_UTF8
            | MI_UTF16
            | MI_UTF32
    )
}

fn invalid_element(offset: usize, reason: impl Into<String>) -> MatlabIqError {
    MatlabIqError::InvalidElement {
        offset,
        reason: reason.into(),
    }
}

fn parse_element_stream(
    bytes: &[u8],
    endian: Endian,
    depth: usize,
    decompressed_bytes: &mut usize,
    builder: &mut OuPrimeBuilder,
) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        if remaining.len() < 8 && remaining.iter().all(|byte| *byte == 0) {
            break;
        }

        let element = read_element(bytes, offset, endian)?;
        match element.data_type {
            MI_MATRIX => {
                if element.small {
                    return Err(invalid_element(
                        element.offset,
                        "miMATRIX cannot use the small-data format",
                    ));
                }
                let matrix = parse_matrix(element.data, endian, element.offset + 8)?;
                builder.accept(matrix)?;
                offset = require_aligned_end(bytes, &element)?;
            }
            MI_COMPRESSED => {
                if element.small {
                    return Err(invalid_element(
                        element.offset,
                        "miCOMPRESSED cannot use the small-data format",
                    ));
                }
                if depth >= MAX_COMPRESSION_DEPTH {
                    return Err(MatlabIqError::Limit {
                        what: "nested compression depth",
                        limit: MAX_COMPRESSION_DEPTH,
                    });
                }
                let remaining_budget = MAX_DECOMPRESSED_BYTES
                    .checked_sub(*decompressed_bytes)
                    .ok_or(MatlabIqError::Limit {
                        what: "total decompressed byte count",
                        limit: MAX_DECOMPRESSED_BYTES,
                    })?;
                let inflated = decompress_limited(element.data, remaining_budget)?;
                *decompressed_bytes =
                    decompressed_bytes
                        .checked_add(inflated.len())
                        .ok_or(MatlabIqError::Limit {
                            what: "total decompressed byte count",
                            limit: MAX_DECOMPRESSED_BYTES,
                        })?;
                parse_element_stream(&inflated, endian, depth + 1, decompressed_bytes, builder)?;
                // MATLAB's own v7 writer does not pad miCOMPRESSED payloads,
                // although ordinary Level 5 elements are aligned to 8 bytes.
                // Accept padding from other conforming writers when it is
                // unambiguous.
                offset = compressed_next_offset(bytes, &element, endian);
            }
            other => {
                return Err(invalid_element(
                    element.offset,
                    format!(
                        "top-level element has type {other}, expected miMATRIX or miCOMPRESSED"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn require_aligned_end(bytes: &[u8], element: &Element<'_>) -> Result<usize> {
    if element.aligned_end > bytes.len() {
        return Err(MatlabIqError::Truncated {
            what: "MAT data-element padding",
            offset: element.raw_end,
            needed: element.aligned_end - element.raw_end,
            available: bytes.len().saturating_sub(element.raw_end),
        });
    }
    Ok(element.aligned_end)
}

fn compressed_next_offset(bytes: &[u8], element: &Element<'_>, endian: Endian) -> usize {
    if element.raw_end == bytes.len() || looks_like_top_level_tag(bytes, element.raw_end, endian) {
        return element.raw_end;
    }
    if element.aligned_end <= bytes.len()
        && bytes[element.raw_end..element.aligned_end]
            .iter()
            .all(|byte| *byte == 0)
        && (element.aligned_end == bytes.len()
            || looks_like_top_level_tag(bytes, element.aligned_end, endian))
    {
        return element.aligned_end;
    }
    element.raw_end
}

fn looks_like_top_level_tag(bytes: &[u8], offset: usize, endian: Endian) -> bool {
    if bytes.len().saturating_sub(offset) < 8 {
        return false;
    }
    let word = endian.u32(&bytes[offset..offset + 4]);
    let small_len = word >> 16;
    small_len == 0 && matches!(word, MI_MATRIX | MI_COMPRESSED)
}

fn decompress_limited(bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(bytes);
    let initial_capacity = bytes
        .len()
        .saturating_mul(4)
        .min(limit)
        .min(MAX_INITIAL_DECOMPRESSED_CAPACITY);
    let mut inflated = Vec::new();
    inflated
        .try_reserve_exact(initial_capacity)
        .map_err(|_| MatlabIqError::Allocation {
            what: "decompressed MAT element bytes",
            elements: initial_capacity,
        })?;
    let read_limit = u64::try_from(limit)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    {
        let mut limited = (&mut decoder).take(read_limit);
        limited
            .read_to_end(&mut inflated)
            .map_err(|error| MatlabIqError::Compression(error.to_string()))?;
    }
    if inflated.len() > limit {
        return Err(MatlabIqError::Limit {
            what: "total decompressed byte count",
            limit: MAX_DECOMPRESSED_BYTES,
        });
    }
    if decoder.total_in() != bytes.len() as u64 {
        return Err(MatlabIqError::Compression(format!(
            "zlib stream consumed {} of {} declared bytes",
            decoder.total_in(),
            bytes.len()
        )));
    }
    Ok(inflated)
}

fn decompress_gzip_limited(bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(bytes);
    let initial_capacity = bytes
        .len()
        .saturating_mul(4)
        .min(limit)
        .min(MAX_INITIAL_DECOMPRESSED_CAPACITY);
    let mut inflated = Vec::new();
    inflated
        .try_reserve_exact(initial_capacity)
        .map_err(|_| MatlabIqError::Allocation {
            what: "gzip-decoded MAT bytes",
            elements: initial_capacity,
        })?;
    let read_limit = u64::try_from(limit)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    {
        let mut limited = (&mut decoder).take(read_limit);
        limited
            .read_to_end(&mut inflated)
            .map_err(|error| MatlabIqError::GzipCompression(error.to_string()))?;
    }
    if inflated.len() > limit {
        return Err(MatlabIqError::Limit {
            what: "gzip-decoded MAT byte count",
            limit,
        });
    }
    Ok(inflated)
}

#[derive(Debug)]
struct ParsedMatrix<'a> {
    name: String,
    dimensions: Vec<usize>,
    payload: MatrixPayload<'a>,
}

#[derive(Debug)]
enum MatrixPayload<'a> {
    Numeric {
        real: NumericData<'a>,
        imaginary: Option<NumericData<'a>>,
    },
    Character(String),
}

fn parse_matrix<'a>(
    bytes: &'a [u8],
    endian: Endian,
    base_offset: usize,
) -> Result<ParsedMatrix<'a>> {
    let mut offset = 0;

    let flags_element = read_matrix_element(bytes, &mut offset, endian, base_offset)?;
    if flags_element.data_type != MI_UINT32 || flags_element.data.len() != 8 {
        return Err(invalid_element(
            base_offset + flags_element.offset,
            "array flags must be an 8-byte miUINT32 element",
        ));
    }
    let flags = endian.u32(&flags_element.data[..4]);
    let array_class = (flags & 0xff) as u8;
    let is_complex = flags & ARRAY_FLAG_COMPLEX != 0;

    let dimensions_element = read_matrix_element(bytes, &mut offset, endian, base_offset)?;
    if dimensions_element.data_type != MI_INT32 || dimensions_element.data.len() % 4 != 0 {
        return Err(invalid_element(
            base_offset + dimensions_element.offset,
            "array dimensions must be an miINT32 vector",
        ));
    }
    let dimension_count = dimensions_element.data.len() / 4;
    if !(1..=MAX_DIMENSIONS).contains(&dimension_count) {
        return Err(MatlabIqError::Limit {
            what: "MAT array dimension count",
            limit: MAX_DIMENSIONS,
        });
    }
    let mut dimensions = Vec::new();
    dimensions
        .try_reserve_exact(dimension_count)
        .map_err(|_| MatlabIqError::Allocation {
            what: "MAT array dimensions",
            elements: dimension_count,
        })?;
    let mut element_count = 1usize;
    for chunk in dimensions_element.data.chunks_exact(4) {
        let dimension = endian.i32(chunk);
        if dimension < 0 {
            return Err(invalid_element(
                base_offset + dimensions_element.offset,
                format!("negative array dimension {dimension}"),
            ));
        }
        let dimension = dimension as usize;
        element_count = element_count
            .checked_mul(dimension)
            .ok_or(MatlabIqError::Limit {
                what: "MAT array element count",
                limit: MAX_ARRAY_ELEMENTS,
            })?;
        if element_count > MAX_ARRAY_ELEMENTS {
            return Err(MatlabIqError::Limit {
                what: "MAT array element count",
                limit: MAX_ARRAY_ELEMENTS,
            });
        }
        dimensions.push(dimension);
    }

    let name_element = read_matrix_element(bytes, &mut offset, endian, base_offset)?;
    if !matches!(name_element.data_type, MI_INT8 | MI_UINT8 | MI_UTF8) {
        return Err(invalid_element(
            base_offset + name_element.offset,
            "array name must be an 8-bit text element",
        ));
    }
    if name_element.data.len() > MAX_NAME_BYTES {
        return Err(MatlabIqError::Limit {
            what: "MAT variable name byte count",
            limit: MAX_NAME_BYTES,
        });
    }
    let name = std::str::from_utf8(name_element.data)
        .map_err(|error| {
            invalid_element(
                base_offset + name_element.offset,
                format!("array name is not UTF-8: {error}"),
            )
        })?
        .to_owned();
    if name.is_empty() {
        return Err(invalid_element(
            base_offset + name_element.offset,
            "top-level array has an empty name",
        ));
    }

    let real_element = read_matrix_element(bytes, &mut offset, endian, base_offset)?;
    let payload = if array_class == MX_CHAR_CLASS {
        if is_complex {
            return Err(invalid_variable(&name, "character array is marked complex"));
        }
        let text = decode_character_data(
            real_element.data_type,
            real_element.data,
            endian,
            element_count,
            base_offset + real_element.offset,
        )?;
        MatrixPayload::Character(text)
    } else if (MX_DOUBLE_CLASS..=MX_UINT64_CLASS).contains(&array_class) {
        let real = NumericData::new(
            real_element.data_type,
            real_element.data,
            endian,
            base_offset + real_element.offset,
        )?;
        if real.len() != element_count {
            return Err(invalid_variable(
                &name,
                format!(
                    "real component has {} values, dimensions require {element_count}",
                    real.len()
                ),
            ));
        }
        let imaginary = if is_complex {
            let imaginary_element = read_matrix_element(bytes, &mut offset, endian, base_offset)?;
            let imaginary = NumericData::new(
                imaginary_element.data_type,
                imaginary_element.data,
                endian,
                base_offset + imaginary_element.offset,
            )?;
            if imaginary.data_type != real.data_type {
                return Err(invalid_variable(
                    &name,
                    "real and imaginary components use different numeric types",
                ));
            }
            if imaginary.len() != element_count {
                return Err(invalid_variable(
                    &name,
                    format!(
                        "imaginary component has {} values, dimensions require {element_count}",
                        imaginary.len()
                    ),
                ));
            }
            Some(imaginary)
        } else {
            None
        };
        MatrixPayload::Numeric { real, imaginary }
    } else {
        return Err(invalid_variable(
            &name,
            format!("unsupported MATLAB array class {array_class}"),
        ));
    };

    if offset != bytes.len() {
        return Err(invalid_element(
            base_offset + offset,
            format!("{} trailing bytes remain in miMATRIX", bytes.len() - offset),
        ));
    }

    Ok(ParsedMatrix {
        name,
        dimensions,
        payload,
    })
}

fn read_matrix_element<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    endian: Endian,
    base_offset: usize,
) -> Result<Element<'a>> {
    let element = read_element(bytes, *offset, endian).map_err(|error| match error {
        MatlabIqError::Truncated {
            what,
            offset: nested_offset,
            needed,
            available,
        } => MatlabIqError::Truncated {
            what,
            offset: base_offset + nested_offset,
            needed,
            available,
        },
        MatlabIqError::InvalidElement {
            offset: nested_offset,
            reason,
        } => MatlabIqError::InvalidElement {
            offset: base_offset + nested_offset,
            reason,
        },
        other => other,
    })?;
    *offset = require_aligned_end(bytes, &element).map_err(|error| match error {
        MatlabIqError::Truncated {
            what,
            offset: nested_offset,
            needed,
            available,
        } => MatlabIqError::Truncated {
            what,
            offset: base_offset + nested_offset,
            needed,
            available,
        },
        other => other,
    })?;
    Ok(element)
}

fn decode_character_data(
    data_type: u32,
    bytes: &[u8],
    endian: Endian,
    expected_units: usize,
    offset: usize,
) -> Result<String> {
    if expected_units > MAX_CHARACTER_UNITS {
        return Err(MatlabIqError::Limit {
            what: "MAT character count",
            limit: MAX_CHARACTER_UNITS,
        });
    }

    match data_type {
        MI_INT8 | MI_UINT8 => {
            if bytes.len() != expected_units {
                return Err(invalid_element(
                    offset,
                    format!(
                        "character data has {} bytes, dimensions require {expected_units}",
                        bytes.len()
                    ),
                ));
            }
            let capacity = bytes.len().checked_mul(2).ok_or(MatlabIqError::Limit {
                what: "decoded MAT character bytes",
                limit: MAX_CHARACTER_UNITS * 4,
            })?;
            let mut text = String::new();
            text.try_reserve_exact(capacity)
                .map_err(|_| MatlabIqError::Allocation {
                    what: "decoded MAT characters",
                    elements: capacity,
                })?;
            text.extend(bytes.iter().map(|byte| char::from(*byte)));
            Ok(text)
        }
        MI_UTF8 => {
            if bytes.len() > MAX_CHARACTER_UNITS * 4 {
                return Err(MatlabIqError::Limit {
                    what: "decoded MAT character bytes",
                    limit: MAX_CHARACTER_UNITS * 4,
                });
            }
            let text = std::str::from_utf8(bytes).map_err(|error| {
                invalid_element(offset, format!("invalid UTF-8 character data: {error}"))
            })?;
            if text.chars().count() != expected_units {
                return Err(invalid_element(
                    offset,
                    format!(
                        "character data has {} characters, dimensions require {expected_units}",
                        text.chars().count()
                    ),
                ));
            }
            Ok(text.to_owned())
        }
        MI_UINT16 | MI_UTF16 => {
            if !bytes.len().is_multiple_of(2) || bytes.len() / 2 != expected_units {
                return Err(invalid_element(
                    offset,
                    "UTF-16 character byte count does not match its dimensions",
                ));
            }
            let mut units = Vec::new();
            units
                .try_reserve_exact(expected_units)
                .map_err(|_| MatlabIqError::Allocation {
                    what: "decoded MAT UTF-16 units",
                    elements: expected_units,
                })?;
            units.extend(bytes.chunks_exact(2).map(|chunk| endian.u16(chunk)));
            String::from_utf16(&units).map_err(|error| {
                invalid_element(offset, format!("invalid UTF-16 character data: {error}"))
            })
        }
        MI_UTF32 | MI_UINT32 => {
            if !bytes.len().is_multiple_of(4) || bytes.len() / 4 != expected_units {
                return Err(invalid_element(
                    offset,
                    "UTF-32 character byte count does not match its dimensions",
                ));
            }
            let capacity = expected_units.checked_mul(4).ok_or(MatlabIqError::Limit {
                what: "decoded MAT character bytes",
                limit: MAX_CHARACTER_UNITS * 4,
            })?;
            let mut text = String::new();
            text.try_reserve_exact(capacity)
                .map_err(|_| MatlabIqError::Allocation {
                    what: "decoded MAT characters",
                    elements: capacity,
                })?;
            for chunk in bytes.chunks_exact(4) {
                let codepoint = endian.u32(chunk);
                let character = char::from_u32(codepoint).ok_or_else(|| {
                    invalid_element(offset, format!("invalid UTF-32 code point 0x{codepoint:x}"))
                })?;
                text.push(character);
            }
            Ok(text)
        }
        other => Err(invalid_element(
            offset,
            format!("unsupported character data type {other}"),
        )),
    }
}

#[derive(Clone, Copy, Debug)]
struct NumericData<'a> {
    data_type: u32,
    bytes: &'a [u8],
    endian: Endian,
    width: usize,
}

impl<'a> NumericData<'a> {
    fn new(data_type: u32, bytes: &'a [u8], endian: Endian, offset: usize) -> Result<Self> {
        let width = match data_type {
            MI_INT8 | MI_UINT8 => 1,
            MI_INT16 | MI_UINT16 => 2,
            MI_INT32 | MI_UINT32 | MI_SINGLE => 4,
            MI_DOUBLE | MI_INT64 | MI_UINT64 => 8,
            other => {
                return Err(invalid_element(
                    offset,
                    format!("unsupported numeric data type {other}"),
                ));
            }
        };
        if !bytes.len().is_multiple_of(width) {
            return Err(invalid_element(
                offset,
                format!(
                    "numeric byte count {} is not divisible by element width {width}",
                    bytes.len()
                ),
            ));
        }
        Ok(Self {
            data_type,
            bytes,
            endian,
            width,
        })
    }

    fn len(self) -> usize {
        self.bytes.len() / self.width
    }

    fn value(self, index: usize) -> NumericValue {
        let start = index * self.width;
        let bytes = &self.bytes[start..start + self.width];
        match self.data_type {
            MI_INT8 => NumericValue::Signed(bytes[0] as i8 as i64),
            MI_UINT8 => NumericValue::Unsigned(bytes[0] as u64),
            MI_INT16 => NumericValue::Signed(self.endian.i16(bytes) as i64),
            MI_UINT16 => NumericValue::Unsigned(self.endian.u16(bytes) as u64),
            MI_INT32 => NumericValue::Signed(self.endian.i32(bytes) as i64),
            MI_UINT32 => NumericValue::Unsigned(self.endian.u32(bytes) as u64),
            MI_SINGLE => NumericValue::Float(self.endian.f32(bytes) as f64),
            MI_DOUBLE => NumericValue::Float(self.endian.f64(bytes)),
            MI_INT64 => NumericValue::Signed(self.endian.i64(bytes)),
            MI_UINT64 => NumericValue::Unsigned(self.endian.u64(bytes)),
            _ => unreachable!("NumericData construction validates its type"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum NumericValue {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

impl NumericValue {
    fn as_f64(self) -> f64 {
        match self {
            Self::Signed(value) => value as f64,
            Self::Unsigned(value) => value as f64,
            Self::Float(value) => value,
        }
    }

    fn as_usize(self) -> Option<usize> {
        match self {
            Self::Signed(value) => usize::try_from(value).ok(),
            Self::Unsigned(value) => usize::try_from(value).ok(),
            Self::Float(value)
                if value.is_finite()
                    && value >= 0.0
                    && value.fract() == 0.0
                    && value < usize::MAX as f64 =>
            {
                Some(value as usize)
            }
            Self::Float(_) => None,
        }
    }
}

#[derive(Debug)]
struct StoredCube {
    dimensions: Vec<usize>,
    samples: Vec<Complex32>,
}

#[derive(Default)]
struct OuPrimeBuilder {
    seen: BTreeSet<String>,
    owned_decoded_bytes: usize,
    radar: Option<String>,
    scan_time: Option<Vec<u16>>,
    wavelength_m: Option<f64>,
    elevation_deg: Option<f64>,
    azimuths_deg: Option<Vec<f64>>,
    gate_spacing_m: Option<f64>,
    first_gate_m: Option<f64>,
    pri_seconds: Option<f64>,
    propagation_speed_m_s: Option<f64>,
    azimuth_count: Option<usize>,
    gate_count: Option<usize>,
    pulse_count: Option<usize>,
    horizontal: Option<StoredCube>,
    vertical: Option<StoredCube>,
}

impl OuPrimeBuilder {
    fn accept(&mut self, matrix: ParsedMatrix<'_>) -> Result<()> {
        if self.seen.len() >= MAX_VARIABLES {
            return Err(MatlabIqError::Limit {
                what: "MAT variable count",
                limit: MAX_VARIABLES,
            });
        }
        if !self.seen.insert(matrix.name.clone()) {
            return Err(MatlabIqError::DuplicateVariable { name: matrix.name });
        }

        match matrix.name.as_str() {
            "radar" => {
                self.reserve_owned_decoded_elements(matrix_element_count(&matrix), 4)?;
                self.radar = Some(character_scalar_or_vector(&matrix)?);
            }
            "scan_time" => {
                self.reserve_owned_decoded_elements(
                    matrix_element_count(&matrix),
                    std::mem::size_of::<u16>(),
                )?;
                self.scan_time = Some(u16_vector(&matrix)?);
            }
            "lambda" => self.wavelength_m = Some(finite_scalar(&matrix)?),
            "el" => self.elevation_deg = Some(finite_scalar(&matrix)?),
            "az_set" => {
                self.reserve_owned_decoded_elements(
                    matrix_element_count(&matrix),
                    std::mem::size_of::<f64>(),
                )?;
                self.azimuths_deg = Some(finite_vector(&matrix)?);
            }
            "delr" => self.gate_spacing_m = Some(finite_scalar(&matrix)?),
            "r_min" => self.first_gate_m = Some(finite_scalar(&matrix)?),
            "pri" => self.pri_seconds = Some(finite_scalar(&matrix)?),
            "c" => self.propagation_speed_m_s = Some(finite_scalar(&matrix)?),
            "num_az" => self.azimuth_count = Some(nonnegative_integer_scalar(&matrix)?),
            "num_gates" => self.gate_count = Some(nonnegative_integer_scalar(&matrix)?),
            "num_pulses" => self.pulse_count = Some(nonnegative_integer_scalar(&matrix)?),
            "X_h" => {
                self.reserve_owned_decoded_elements(
                    matrix_element_count(&matrix),
                    std::mem::size_of::<Complex32>(),
                )?;
                self.horizontal = Some(complex_cube(&matrix, "horizontal I/Q")?);
            }
            "X_v" => {
                self.reserve_owned_decoded_elements(
                    matrix_element_count(&matrix),
                    std::mem::size_of::<Complex32>(),
                )?;
                self.vertical = Some(complex_cube(&matrix, "vertical I/Q")?);
            }
            _ => {}
        }
        Ok(())
    }

    fn reserve_owned_decoded_elements(
        &mut self,
        elements: usize,
        element_width: usize,
    ) -> Result<()> {
        let bytes = elements
            .checked_mul(element_width)
            .ok_or(MatlabIqError::Limit {
                what: "owned decoded MAT bytes",
                limit: MAX_OWNED_DECODED_BYTES,
            })?;
        let next = self
            .owned_decoded_bytes
            .checked_add(bytes)
            .ok_or(MatlabIqError::Limit {
                what: "owned decoded MAT bytes",
                limit: MAX_OWNED_DECODED_BYTES,
            })?;
        if next > MAX_OWNED_DECODED_BYTES {
            return Err(MatlabIqError::Limit {
                what: "owned decoded MAT bytes",
                limit: MAX_OWNED_DECODED_BYTES,
            });
        }
        self.owned_decoded_bytes = next;
        Ok(())
    }

    fn finish(self) -> Result<OuPrimeIqCube> {
        let radar = required(self.radar, "radar")?;
        if radar.trim().is_empty() || radar.contains('\0') {
            return Err(invalid_variable(
                "radar",
                "radar name must be nonempty and contain no NUL",
            ));
        }

        let scan_time_values = required(self.scan_time, "scan_time")?;
        let scan_time_utc: [u16; 6] = scan_time_values.try_into().map_err(|values: Vec<u16>| {
            invalid_variable(
                "scan_time",
                format!("expected 6 UTC fields, found {}", values.len()),
            )
        })?;
        validate_scan_time(scan_time_utc)?;

        let wavelength_m = positive(required(self.wavelength_m, "lambda")?, "lambda")?;
        let elevation_deg = required(self.elevation_deg, "el")?;
        if !(-90.0..=90.0).contains(&elevation_deg) {
            return Err(invalid_variable(
                "el",
                format!("elevation {elevation_deg} is outside [-90, 90] degrees"),
            ));
        }
        let azimuths_deg = required(self.azimuths_deg, "az_set")?;
        if let Some(azimuth) = azimuths_deg
            .iter()
            .copied()
            .find(|azimuth| !(0.0..360.0).contains(azimuth))
        {
            return Err(invalid_variable(
                "az_set",
                format!("azimuth {azimuth} is outside [0, 360) degrees"),
            ));
        }
        let gate_spacing_m = positive(required(self.gate_spacing_m, "delr")?, "delr")?;
        let first_gate_m = required(self.first_gate_m, "r_min")?;
        if first_gate_m < 0.0 {
            return Err(invalid_variable("r_min", "first-gate range is negative"));
        }
        let pri_seconds = positive(required(self.pri_seconds, "pri")?, "pri")?;
        let propagation_speed_m_s = positive(required(self.propagation_speed_m_s, "c")?, "c")?;
        let azimuth_count = positive_count(required(self.azimuth_count, "num_az")?, "num_az")?;
        let gate_count = positive_count(required(self.gate_count, "num_gates")?, "num_gates")?;
        let pulse_count = positive_count(required(self.pulse_count, "num_pulses")?, "num_pulses")?;
        if azimuths_deg.len() != azimuth_count {
            return Err(invalid_variable(
                "az_set",
                format!(
                    "contains {} azimuths but num_az is {azimuth_count}",
                    azimuths_deg.len()
                ),
            ));
        }

        let expected_dimensions = [azimuth_count, gate_count, pulse_count];
        let horizontal = required(self.horizontal, "X_h")?;
        validate_cube_dimensions("X_h", &horizontal.dimensions, expected_dimensions)?;
        let vertical = required(self.vertical, "X_v")?;
        validate_cube_dimensions("X_v", &vertical.dimensions, expected_dimensions)?;

        Ok(OuPrimeIqCube {
            radar,
            scan_time_utc,
            wavelength_m,
            elevation_deg,
            azimuths_deg,
            gate_spacing_m,
            first_gate_m,
            pri_seconds,
            propagation_speed_m_s,
            azimuth_count,
            gate_count,
            pulse_count,
            horizontal: horizontal.samples,
            vertical: vertical.samples,
        })
    }
}

fn matrix_element_count(matrix: &ParsedMatrix<'_>) -> usize {
    matrix.dimensions.iter().copied().product()
}

fn required<T>(value: Option<T>, name: &'static str) -> Result<T> {
    value.ok_or(MatlabIqError::MissingVariable { name })
}

fn invalid_variable(name: impl Into<String>, reason: impl Into<String>) -> MatlabIqError {
    MatlabIqError::InvalidVariable {
        name: name.into(),
        reason: reason.into(),
    }
}

fn numeric_payload<'a>(
    matrix: &'a ParsedMatrix<'a>,
) -> Result<(NumericData<'a>, Option<NumericData<'a>>)> {
    match &matrix.payload {
        MatrixPayload::Numeric { real, imaginary } => Ok((*real, *imaginary)),
        MatrixPayload::Character(_) => Err(invalid_variable(
            &matrix.name,
            "expected a numeric array, found character data",
        )),
    }
}

fn finite_scalar(matrix: &ParsedMatrix<'_>) -> Result<f64> {
    let (real, imaginary) = numeric_payload(matrix)?;
    if imaginary.is_some() {
        return Err(invalid_variable(&matrix.name, "expected a real scalar"));
    }
    if real.len() != 1 {
        return Err(invalid_variable(
            &matrix.name,
            format!("expected one value, found {}", real.len()),
        ));
    }
    let value = real.value(0).as_f64();
    if !value.is_finite() {
        return Err(invalid_variable(&matrix.name, "value is not finite"));
    }
    Ok(value)
}

fn nonnegative_integer_scalar(matrix: &ParsedMatrix<'_>) -> Result<usize> {
    let (real, imaginary) = numeric_payload(matrix)?;
    if imaginary.is_some() || real.len() != 1 {
        return Err(invalid_variable(
            &matrix.name,
            "expected one real integer value",
        ));
    }
    real.value(0).as_usize().ok_or_else(|| {
        invalid_variable(
            &matrix.name,
            "value is not a representable nonnegative integer",
        )
    })
}

fn finite_vector(matrix: &ParsedMatrix<'_>) -> Result<Vec<f64>> {
    let (real, imaginary) = numeric_payload(matrix)?;
    if imaginary.is_some() {
        return Err(invalid_variable(&matrix.name, "expected a real vector"));
    }
    ensure_vector_shape(matrix)?;
    if real.len() > MAX_AZIMUTH_COUNT {
        return Err(MatlabIqError::Limit {
            what: "OU-PRIME azimuth metadata count",
            limit: MAX_AZIMUTH_COUNT,
        });
    }
    let mut values = Vec::new();
    values
        .try_reserve_exact(real.len())
        .map_err(|_| MatlabIqError::Allocation {
            what: "OU-PRIME azimuth metadata",
            elements: real.len(),
        })?;
    for index in 0..real.len() {
        let value = real.value(index).as_f64();
        if !value.is_finite() {
            return Err(invalid_variable(
                &matrix.name,
                format!("value at index {index} is not finite"),
            ));
        }
        values.push(value);
    }
    Ok(values)
}

fn u16_vector(matrix: &ParsedMatrix<'_>) -> Result<Vec<u16>> {
    let (real, imaginary) = numeric_payload(matrix)?;
    if imaginary.is_some() {
        return Err(invalid_variable(&matrix.name, "expected a real vector"));
    }
    ensure_vector_shape(matrix)?;
    if real.len() > MAX_AZIMUTH_COUNT {
        return Err(MatlabIqError::Limit {
            what: "OU-PRIME integer metadata count",
            limit: MAX_AZIMUTH_COUNT,
        });
    }
    let mut values = Vec::new();
    values
        .try_reserve_exact(real.len())
        .map_err(|_| MatlabIqError::Allocation {
            what: "OU-PRIME integer metadata",
            elements: real.len(),
        })?;
    for index in 0..real.len() {
        let value = real.value(index).as_usize().ok_or_else(|| {
            invalid_variable(
                &matrix.name,
                format!("value at index {index} is not a nonnegative integer"),
            )
        })?;
        let value = u16::try_from(value).map_err(|_| {
            invalid_variable(
                &matrix.name,
                format!("value at index {index} does not fit in u16"),
            )
        })?;
        values.push(value);
    }
    Ok(values)
}

fn character_scalar_or_vector(matrix: &ParsedMatrix<'_>) -> Result<String> {
    ensure_vector_shape(matrix)?;
    match &matrix.payload {
        MatrixPayload::Character(value) => Ok(value.clone()),
        MatrixPayload::Numeric { .. } => Err(invalid_variable(
            &matrix.name,
            "expected character data, found a numeric array",
        )),
    }
}

fn ensure_vector_shape(matrix: &ParsedMatrix<'_>) -> Result<()> {
    if matrix
        .dimensions
        .iter()
        .filter(|dimension| **dimension > 1)
        .count()
        > 1
    {
        return Err(invalid_variable(
            &matrix.name,
            format!(
                "expected a vector, found dimensions {:?}",
                matrix.dimensions
            ),
        ));
    }
    Ok(())
}

fn complex_cube(matrix: &ParsedMatrix<'_>, what: &'static str) -> Result<StoredCube> {
    let (real, imaginary) = numeric_payload(matrix)?;
    let imaginary = imaginary
        .ok_or_else(|| invalid_variable(&matrix.name, "I/Q cube is not marked complex"))?;
    if matrix.dimensions.len() != 3 {
        return Err(invalid_variable(
            &matrix.name,
            format!(
                "expected [azimuth, gate, pulse], found dimensions {:?}",
                matrix.dimensions
            ),
        ));
    }
    let (azimuth_count, gate_count, pulse_count) = (
        matrix.dimensions[0],
        matrix.dimensions[1],
        matrix.dimensions[2],
    );
    let (_, expected_samples) = validate_iq_dimensions(azimuth_count, gate_count, pulse_count)?;
    if real.len() != expected_samples {
        return Err(invalid_variable(
            &matrix.name,
            format!(
                "cube contains {} samples; dimensions require {expected_samples}",
                real.len()
            ),
        ));
    }

    let mut samples = Vec::new();
    samples
        .try_reserve_exact(real.len())
        .map_err(|_| MatlabIqError::Allocation {
            what,
            elements: real.len(),
        })?;
    for index in 0..real.len() {
        samples.push(Complex32 {
            re: sample_component(&matrix.name, "real", index, real.value(index))?,
            im: sample_component(&matrix.name, "imaginary", index, imaginary.value(index))?,
        });
    }
    Ok(StoredCube {
        dimensions: matrix.dimensions.clone(),
        samples,
    })
}

fn sample_component(
    variable: &str,
    component: &str,
    index: usize,
    value: NumericValue,
) -> Result<f32> {
    let value = value.as_f64();
    if !value.is_finite() || value < f32::MIN as f64 || value > f32::MAX as f64 {
        return Err(invalid_variable(
            variable,
            format!("{component} sample at index {index} is not a finite f32"),
        ));
    }
    Ok(value as f32)
}

fn positive(value: f64, name: &'static str) -> Result<f64> {
    if value <= 0.0 {
        return Err(invalid_variable(name, "value must be positive"));
    }
    Ok(value)
}

fn positive_count(value: usize, name: &'static str) -> Result<usize> {
    if value == 0 {
        return Err(invalid_variable(name, "value must be positive"));
    }
    Ok(value)
}

fn validate_iq_dimensions(
    azimuth_count: usize,
    gate_count: usize,
    pulse_count: usize,
) -> Result<(usize, usize)> {
    for (what, value, limit) in [
        ("OU-PRIME azimuth count", azimuth_count, MAX_AZIMUTH_COUNT),
        ("OU-PRIME gate count", gate_count, MAX_GATE_COUNT),
        ("OU-PRIME pulse count", pulse_count, MAX_PULSE_COUNT),
    ] {
        if value == 0 {
            return Err(invalid_variable(what, "value must be positive"));
        }
        if value > limit {
            return Err(MatlabIqError::Limit { what, limit });
        }
    }

    let total_pulses = azimuth_count
        .checked_mul(pulse_count)
        .ok_or(MatlabIqError::Limit {
            what: "OU-PRIME flattened pulse count",
            limit: MAX_FLATTENED_PULSES,
        })?;
    if total_pulses > MAX_FLATTENED_PULSES {
        return Err(MatlabIqError::Limit {
            what: "OU-PRIME flattened pulse count",
            limit: MAX_FLATTENED_PULSES,
        });
    }

    let sample_count = total_pulses
        .checked_mul(gate_count)
        .ok_or(MatlabIqError::Limit {
            what: "OU-PRIME samples per receiver channel",
            limit: MAX_IQ_SAMPLES_PER_CHANNEL,
        })?;
    if sample_count > MAX_IQ_SAMPLES_PER_CHANNEL {
        return Err(MatlabIqError::Limit {
            what: "OU-PRIME samples per receiver channel",
            limit: MAX_IQ_SAMPLES_PER_CHANNEL,
        });
    }

    Ok((total_pulses, sample_count))
}

fn validate_cube_dimensions(
    name: &'static str,
    actual: &[usize],
    expected: [usize; 3],
) -> Result<()> {
    if actual != expected {
        return Err(invalid_variable(
            name,
            format!("dimensions {actual:?} do not match metadata {expected:?}"),
        ));
    }
    Ok(())
}

fn validate_scan_time(value: [u16; 6]) -> Result<()> {
    let [year, month, day, hour, minute, second] = value;
    if year == 0 || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return Err(invalid_variable(
            "scan_time",
            format!("invalid UTC fields {value:?}"),
        ));
    }
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > days_in_month {
        return Err(invalid_variable(
            "scan_time",
            format!("invalid UTC fields {value:?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::{GzEncoder, ZlibEncoder};

    use super::*;

    #[test]
    fn decodes_little_endian_compressed_cube_and_small_elements() {
        let bytes = fixture(Endian::Little, true, MI_SINGLE, MX_DOUBLE_CLASS);
        let cube = decode_ou_prime_mat(&bytes).expect("valid cube");

        assert_eq!(cube.radar, "OUPRIME");
        assert_eq!(cube.scan_time_utc, [2010, 5, 10, 22, 47, 11]);
        assert_eq!(
            [cube.azimuth_count, cube.gate_count, cube.pulse_count],
            [2, 3, 2]
        );
        assert_eq!(
            cube.horizontal_sample(1, 2, 1),
            Some(Complex32 {
                re: 12.0,
                im: -12.0
            })
        );
        assert_eq!(
            cube.vertical_sample(0, 0, 0),
            Some(Complex32 {
                re: 101.0,
                im: 201.0
            })
        );
        assert_eq!(cube.gate_range_m(2), Some(250.0));
        assert!((cube.prf_hz() - 1180.0).abs() < 1.0e-9);
        assert!((cube.nyquist_velocity_m_s() - 16.048).abs() < 0.001);
    }

    #[test]
    fn conversion_preserves_column_major_samples_and_native_ray_boundaries() {
        let bytes = fixture(Endian::Little, true, MI_SINGLE, MX_DOUBLE_CLASS);
        let sweep = decode_ou_prime_mat(&bytes)
            .expect("valid cube")
            .into_iq_sweep()
            .expect("cube maps to sweep");

        assert_eq!(sweep.site, "OUPRIME");
        assert_eq!(sweep.pulse_width_s, None);
        assert_eq!(sweep.calibration, IqCalibration::RelativeStoredIq);
        assert_eq!(sweep.pulses.len(), 4);
        assert_eq!(
            sweep.pulse_layout,
            PulseLayout::Rays(vec![
                PulseSpan { start: 0, len: 2 },
                PulseSpan { start: 2, len: 2 }
            ])
        );
        assert_eq!(
            sweep.pulses[0].h,
            vec![(1.0, -1.0), (3.0, -3.0), (5.0, -5.0)]
        );
        assert_eq!(
            sweep.pulses[1].h,
            vec![(7.0, -7.0), (9.0, -9.0), (11.0, -11.0)]
        );
        assert_eq!(sweep.pulses[2].h[0], (2.0, -2.0));
        assert_eq!(sweep.range_bins, vec![0.0, 125.0, 250.0]);

        let native = crate::iq_moments::MomentConfig {
            dwell: crate::iq_moments::DwellPlan::contiguous(2),
            ..crate::iq_moments::MomentConfig::default()
        };
        let processed =
            crate::iq_moments::process_sweep(&sweep, &native).expect("one native dwell per ray");
        assert_eq!(processed.report.dwells, 2);
        assert!(
            processed
                .cut
                .moments
                .contains_key(&radar_core::MomentType::RelativePower)
        );
        assert!(
            !processed
                .cut
                .moments
                .contains_key(&radar_core::MomentType::Reflectivity)
        );

        let crossing = crate::iq_moments::MomentConfig {
            dwell: crate::iq_moments::DwellPlan::contiguous(3),
            ..crate::iq_moments::MomentConfig::default()
        };
        assert!(matches!(
            crate::iq_moments::process_sweep(&sweep, &crossing),
            Err(crate::iq_moments::IqMomentError::NativeRayDwellRequired { native: 2, .. })
        ));
    }

    #[test]
    fn decodes_big_endian_tags_and_int32_complex_storage() {
        let bytes = fixture(Endian::Big, true, MI_INT32, MX_DOUBLE_CLASS);
        let cube = decode_ou_prime_mat(&bytes).expect("valid cube");

        assert_eq!(cube.horizontal[0], Complex32 { re: 1.0, im: -1.0 });
        assert_eq!(
            cube.horizontal[11],
            Complex32 {
                re: 12.0,
                im: -12.0
            }
        );
        assert_eq!(
            cube.vertical[11],
            Complex32 {
                re: 112.0,
                im: 212.0
            }
        );
    }

    #[test]
    fn reads_spec_order_big_endian_small_data_tag() {
        // Level 5 stores the small tag as two big-endian u16 fields, not as
        // one packed u32: miUINT8, one byte, then four inline data bytes.
        let bytes = [0x00, 0x02, 0x00, 0x01, 0x7d, 0x00, 0x00, 0x00];
        let element = read_element(&bytes, 0, Endian::Big).expect("small element");
        assert!(element.small);
        assert_eq!(element.data_type, MI_UINT8);
        assert_eq!(element.data, &[0x7d]);
        assert_eq!(element.aligned_end, 8);
    }

    #[test]
    fn decodes_gzip_wrapped_mat() {
        let raw = fixture(Endian::Little, true, MI_SINGLE, MX_DOUBLE_CLASS);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&raw).expect("compress MAT fixture");
        let gzip = encoder.finish().expect("finish gzip fixture");

        let cube = decode_ou_prime_mat(&gzip).expect("decode gzip-wrapped MAT");
        assert_eq!(cube.radar, "OUPRIME");
        assert_eq!(cube.horizontal.len(), 12);
    }

    #[test]
    fn gzip_wrapper_obeys_decoded_byte_limit() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&[0u8; 32])
            .expect("compress bounded fixture");
        let gzip = encoder.finish().expect("finish bounded fixture");

        assert!(matches!(
            decompress_gzip_limited(&gzip, 16),
            Err(MatlabIqError::Limit {
                what: "gzip-decoded MAT byte count",
                limit: 16,
            })
        ));
    }

    #[test]
    fn iq_dimension_and_owned_memory_budgets_are_enforced() {
        assert!(matches!(
            validate_iq_dimensions(MAX_AZIMUTH_COUNT + 1, 1, 1),
            Err(MatlabIqError::Limit {
                what: "OU-PRIME azimuth count",
                limit: MAX_AZIMUTH_COUNT,
            })
        ));
        assert!(matches!(
            validate_iq_dimensions(1, MAX_GATE_COUNT, MAX_PULSE_COUNT),
            Err(MatlabIqError::Limit {
                what: "OU-PRIME samples per receiver channel",
                limit: MAX_IQ_SAMPLES_PER_CHANNEL,
            })
        ));

        let mut builder = OuPrimeBuilder::default();
        builder
            .reserve_owned_decoded_elements(MAX_OWNED_DECODED_BYTES, 1)
            .expect("budget itself is accepted");
        assert!(matches!(
            builder.reserve_owned_decoded_elements(1, 1),
            Err(MatlabIqError::Limit {
                what: "owned decoded MAT bytes",
                limit: MAX_OWNED_DECODED_BYTES,
            })
        ));
    }

    #[test]
    fn decodes_uncompressed_matrix_stream() {
        let bytes = fixture(Endian::Little, false, MI_SINGLE, MX_DOUBLE_CLASS);
        let cube = decode_ou_prime_mat(&bytes).expect("valid uncompressed cube");
        assert_eq!(cube.horizontal.len(), 12);
        assert_eq!(cube.vertical.len(), 12);
    }

    #[test]
    fn rejects_truncated_compressed_payload() {
        let mut bytes = fixture(Endian::Little, true, MI_SINGLE, MX_DOUBLE_CLASS);
        bytes.pop();
        assert!(matches!(
            decode_ou_prime_mat(&bytes),
            Err(MatlabIqError::Truncated { .. }) | Err(MatlabIqError::Compression(_))
        ));
    }

    #[test]
    fn rejects_metadata_that_disagrees_with_cube_shape() {
        let mut matrices = fixture_matrices(Endian::Little, MI_SINGLE, MX_DOUBLE_CLASS);
        matrices.retain(|matrix| matrix_name(matrix, Endian::Little) != Some("num_gates"));
        matrices.push(numeric_matrix(
            "num_gates",
            &[1, 1],
            MI_UINT16,
            &encode_u16(&[4], Endian::Little),
            None,
            MX_DOUBLE_CLASS,
            Endian::Little,
        ));
        let bytes = mat_file(Endian::Little, matrices);

        assert!(matches!(
            decode_ou_prime_mat(&bytes),
            Err(MatlabIqError::InvalidVariable { name, .. }) if name == "X_h"
        ));
    }

    #[test]
    fn rejects_duplicate_variables() {
        let mut matrices = fixture_matrices(Endian::Little, MI_SINGLE, MX_DOUBLE_CLASS);
        matrices.push(numeric_matrix(
            "pri",
            &[1, 1],
            MI_DOUBLE,
            &encode_f64(&[0.001], Endian::Little),
            None,
            MX_DOUBLE_CLASS,
            Endian::Little,
        ));
        let bytes = mat_file(Endian::Little, matrices);
        assert!(matches!(
            decode_ou_prime_mat(&bytes),
            Err(MatlabIqError::DuplicateVariable { name }) if name == "pri"
        ));
    }

    #[test]
    fn reads_every_level5_numeric_primitive_in_both_byte_orders() {
        for endian in [Endian::Little, Endian::Big] {
            assert_numeric_value(MI_INT8, &[0xfe], endian, -2.0);
            assert_numeric_value(MI_UINT8, &[0xfe], endian, 254.0);
            assert_numeric_value(MI_INT16, &encode_i16(&[-1234], endian), endian, -1234.0);
            assert_numeric_value(MI_UINT16, &encode_u16(&[54321], endian), endian, 54321.0);
            assert_numeric_value(
                MI_INT32,
                &encode_i32(&[-123_456], endian),
                endian,
                -123_456.0,
            );
            assert_numeric_value(
                MI_UINT32,
                &encode_u32(&[3_000_000_000], endian),
                endian,
                3_000_000_000.0,
            );
            assert_numeric_value(MI_SINGLE, &encode_f32(&[1.25], endian), endian, 1.25);
            assert_numeric_value(MI_DOUBLE, &encode_f64(&[-2.5], endian), endian, -2.5);
            assert_numeric_value(
                MI_INT64,
                &encode_i64(&[-9_000_000_000], endian),
                endian,
                -9_000_000_000.0,
            );
            assert_numeric_value(
                MI_UINT64,
                &encode_u64(&[9_000_000_000], endian),
                endian,
                9_000_000_000.0,
            );
        }
    }

    fn fixture(endian: Endian, compressed: bool, sample_type: u32, sample_class: u8) -> Vec<u8> {
        let matrices = fixture_matrices(endian, sample_type, sample_class);
        let elements = if compressed {
            matrices
                .into_iter()
                .map(|matrix| compressed_element(&matrix, endian))
                .collect()
        } else {
            matrices
        };
        mat_file(endian, elements)
    }

    fn fixture_matrices(endian: Endian, sample_type: u32, sample_class: u8) -> Vec<Vec<u8>> {
        let mut matrices = vec![
            numeric_matrix(
                "delr",
                &[1, 1],
                MI_UINT8,
                &[125],
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "c",
                &[1, 1],
                MI_INT32,
                &encode_i32(&[300_000_000], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "lambda",
                &[1, 1],
                MI_DOUBLE,
                &encode_f64(&[0.0544], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "el",
                &[1, 1],
                MI_DOUBLE,
                &encode_f64(&[0.120_849_61], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "num_gates",
                &[1, 1],
                MI_UINT16,
                &encode_u16(&[3], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "pri",
                &[1, 1],
                MI_DOUBLE,
                &encode_f64(&[1.0 / 1180.0], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "az_set",
                &[1, 2],
                MI_DOUBLE,
                &encode_f64(&[15.0, 15.5], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "scan_time",
                &[1, 6],
                MI_UINT16,
                &encode_u16(&[2010, 5, 10, 22, 47, 11], endian),
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "num_pulses",
                &[1, 1],
                MI_UINT8,
                &[2],
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "num_az",
                &[1, 1],
                MI_UINT8,
                &[2],
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
            numeric_matrix(
                "r_min",
                &[1, 1],
                MI_UINT8,
                &[0],
                None,
                MX_DOUBLE_CLASS,
                endian,
            ),
        ];

        let horizontal_real: Vec<f64> = (1..=12).map(f64::from).collect();
        let horizontal_imag: Vec<f64> = horizontal_real.iter().map(|value| -*value).collect();
        let vertical_real: Vec<f64> = (101..=112).map(f64::from).collect();
        let vertical_imag: Vec<f64> = (201..=212).map(f64::from).collect();
        matrices.push(complex_sample_matrix(
            "X_h",
            &[2, 3, 2],
            sample_type,
            sample_class,
            &horizontal_real,
            &horizontal_imag,
            endian,
        ));
        matrices.push(complex_sample_matrix(
            "X_v",
            &[2, 3, 2],
            sample_type,
            sample_class,
            &vertical_real,
            &vertical_imag,
            endian,
        ));
        matrices.push(character_matrix("radar", "OUPRIME", endian));
        matrices
    }

    fn complex_sample_matrix(
        name: &str,
        dimensions: &[i32],
        sample_type: u32,
        sample_class: u8,
        real: &[f64],
        imaginary: &[f64],
        endian: Endian,
    ) -> Vec<u8> {
        let encode = |values: &[f64]| match sample_type {
            MI_SINGLE => encode_f32(
                &values.iter().map(|value| *value as f32).collect::<Vec<_>>(),
                endian,
            ),
            MI_INT32 => encode_i32(
                &values.iter().map(|value| *value as i32).collect::<Vec<_>>(),
                endian,
            ),
            _ => unreachable!(),
        };
        numeric_matrix(
            name,
            dimensions,
            sample_type,
            &encode(real),
            Some(&encode(imaginary)),
            sample_class,
            endian,
        )
    }

    fn numeric_matrix(
        name: &str,
        dimensions: &[i32],
        data_type: u32,
        real: &[u8],
        imaginary: Option<&[u8]>,
        class: u8,
        endian: Endian,
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        let flags = class as u32 | imaginary.map_or(0, |_| ARRAY_FLAG_COMPLEX);
        let mut flag_bytes = Vec::new();
        push_u32(&mut flag_bytes, flags, endian);
        push_u32(&mut flag_bytes, 0, endian);
        payload.extend(element(MI_UINT32, &flag_bytes, endian, false));
        payload.extend(element(
            MI_INT32,
            &encode_i32(dimensions, endian),
            endian,
            false,
        ));
        payload.extend(element(MI_INT8, name.as_bytes(), endian, name.len() <= 4));
        payload.extend(element(data_type, real, endian, real.len() <= 4));
        if let Some(imaginary) = imaginary {
            payload.extend(element(data_type, imaginary, endian, imaginary.len() <= 4));
        }
        element(MI_MATRIX, &payload, endian, false)
    }

    fn character_matrix(name: &str, value: &str, endian: Endian) -> Vec<u8> {
        let mut payload = Vec::new();
        let mut flag_bytes = Vec::new();
        push_u32(&mut flag_bytes, MX_CHAR_CLASS as u32, endian);
        push_u32(&mut flag_bytes, 0, endian);
        payload.extend(element(MI_UINT32, &flag_bytes, endian, false));
        payload.extend(element(
            MI_INT32,
            &encode_i32(&[1, value.chars().count() as i32], endian),
            endian,
            false,
        ));
        payload.extend(element(MI_INT8, name.as_bytes(), endian, name.len() <= 4));
        payload.extend(element(MI_UTF8, value.as_bytes(), endian, value.len() <= 4));
        element(MI_MATRIX, &payload, endian, false)
    }

    fn compressed_element(matrix: &[u8], endian: Endian) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(matrix).expect("compress fixture");
        let compressed = encoder.finish().expect("finish fixture compression");
        let mut output = Vec::new();
        push_u32(&mut output, MI_COMPRESSED, endian);
        push_u32(&mut output, compressed.len() as u32, endian);
        output.extend(compressed);
        output
    }

    fn element(data_type: u32, data: &[u8], endian: Endian, small: bool) -> Vec<u8> {
        let mut output = Vec::new();
        if small && !data.is_empty() && data.len() <= 4 {
            push_u16(&mut output, data_type as u16, endian);
            push_u16(&mut output, data.len() as u16, endian);
            output.extend(data);
            output.resize(8, 0);
        } else {
            push_u32(&mut output, data_type, endian);
            push_u32(&mut output, data.len() as u32, endian);
            output.extend(data);
            let aligned = output.len().div_ceil(8) * 8;
            output.resize(aligned, 0);
        }
        output
    }

    fn mat_file(endian: Endian, elements: Vec<Vec<u8>>) -> Vec<u8> {
        let mut bytes = vec![b' '; HEADER_LEN];
        bytes[..LEVEL5_MAGIC.len()].copy_from_slice(LEVEL5_MAGIC);
        bytes[116..124].fill(0);
        let version = match endian {
            Endian::Little => LEVEL5_VERSION.to_le_bytes(),
            Endian::Big => LEVEL5_VERSION.to_be_bytes(),
        };
        bytes[124..126].copy_from_slice(&version);
        bytes[126..128].copy_from_slice(match endian {
            Endian::Little => b"IM",
            Endian::Big => b"MI",
        });
        for element in elements {
            bytes.extend(element);
        }
        bytes
    }

    fn matrix_name(matrix: &[u8], endian: Endian) -> Option<&str> {
        let outer = read_element(matrix, 0, endian).ok()?;
        let mut offset = 0;
        read_matrix_element(outer.data, &mut offset, endian, 8).ok()?;
        read_matrix_element(outer.data, &mut offset, endian, 8).ok()?;
        let name = read_matrix_element(outer.data, &mut offset, endian, 8).ok()?;
        std::str::from_utf8(name.data).ok()
    }

    fn encode_u16(values: &[u16], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        bytes
    }

    fn encode_i16(values: &[i16], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        bytes
    }

    fn encode_i32(values: &[i32], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        bytes
    }

    fn encode_u32(values: &[u32], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            push_u32(&mut bytes, *value, endian);
        }
        bytes
    }

    fn encode_i64(values: &[i64], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        bytes
    }

    fn encode_u64(values: &[u64], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        bytes
    }

    fn encode_f32(values: &[f32], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            push_u32(&mut bytes, value.to_bits(), endian);
        }
        bytes
    }

    fn encode_f64(values: &[f64], endian: Endian) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            let encoded = match endian {
                Endian::Little => value.to_bits().to_le_bytes(),
                Endian::Big => value.to_bits().to_be_bytes(),
            };
            bytes.extend(encoded);
        }
        bytes
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32, endian: Endian) {
        bytes.extend(match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }

    fn push_u16(bytes: &mut Vec<u8>, value: u16, endian: Endian) {
        bytes.extend(match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }

    fn assert_numeric_value(data_type: u32, bytes: &[u8], endian: Endian, expected: f64) {
        let data = NumericData::new(data_type, bytes, endian, 0).expect("numeric primitive");
        assert_eq!(data.len(), 1);
        assert_eq!(data.value(0).as_f64(), expected);
    }
}
