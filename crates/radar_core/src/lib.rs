//! Core data model for the clean-room Rust radar analyst.
//!
//! The model is intentionally data-oriented: radial geometry lives beside compact
//! moment arrays so decoders, product algorithms, and GPU upload code can share a
//! stable contract without per-gate heap objects.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A NEXRAD, TDWR, or compatible radar site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadarSite {
    pub id: String,
    pub name: Option<String>,
    pub latitude_deg: Option<f32>,
    pub longitude_deg: Option<f32>,
    pub elevation_m: Option<f32>,
}

impl RadarSite {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            latitude_deg: None,
            longitude_deg: None,
            elevation_m: None,
        }
    }
}

/// Decoded radar volume with raw moments grouped by elevation cut.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadarVolume {
    pub site: RadarSite,
    pub volume_time: DateTime<Utc>,
    pub vcp: Option<VcpInfo>,
    pub cuts: Vec<ElevationCut>,
    pub metadata: VolumeMetadata,
}

impl RadarVolume {
    pub fn new(site: RadarSite, volume_time: DateTime<Utc>) -> Self {
        Self {
            site,
            volume_time,
            vcp: None,
            cuts: Vec::new(),
            metadata: VolumeMetadata::default(),
        }
    }

    pub fn find_or_insert_cut(
        &mut self,
        elevation_deg: f32,
        elevation_number: Option<u8>,
    ) -> &mut ElevationCut {
        if let Some(index) = self.cuts.iter().rposition(|cut| {
            cut.elevation_number == elevation_number
                || (cut.elevation_deg - elevation_deg).abs() <= 0.05
        }) {
            return &mut self.cuts[index];
        }

        self.push_cut(elevation_deg, elevation_number)
    }

    pub fn push_cut(
        &mut self,
        elevation_deg: f32,
        elevation_number: Option<u8>,
    ) -> &mut ElevationCut {
        self.cuts
            .push(ElevationCut::new(elevation_deg, elevation_number));
        self.cuts.last_mut().expect("cut was just inserted")
    }
}

/// One elevation sweep/cut in a volume scan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElevationCut {
    pub elevation_deg: f32,
    pub elevation_number: Option<u8>,
    pub radials: Vec<Radial>,
    pub moments: BTreeMap<MomentType, MomentGrid>,
}

impl ElevationCut {
    pub fn new(elevation_deg: f32, elevation_number: Option<u8>) -> Self {
        Self {
            elevation_deg,
            elevation_number,
            radials: Vec::new(),
            moments: BTreeMap::new(),
        }
    }

    pub fn moments_available(&self) -> BTreeSet<MomentType> {
        self.moments.keys().cloned().collect()
    }
}

/// Geometry and timing for one radial.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Radial {
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    pub time_offset_ms: i32,
    pub gate_range: GateRange,
    pub nyquist_velocity_mps: Option<f32>,
    pub radial_status: Option<RadialStatus>,
}

/// Gate layout for a radial or moment grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateRange {
    pub first_gate_m: i32,
    pub gate_spacing_m: i32,
    pub gate_count: usize,
}

/// NEXRAD radial status markers used to detect sweep and volume boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RadialStatus {
    StartElevation,
    Intermediate,
    EndElevation,
    StartVolume,
    EndVolume,
    StartElevationLastCut,
    Unknown(u8),
}

impl From<u8> for RadialStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::StartElevation,
            1 => Self::Intermediate,
            2 => Self::EndElevation,
            3 => Self::StartVolume,
            4 => Self::EndVolume,
            5 => Self::StartElevationLastCut,
            other => Self::Unknown(other),
        }
    }
}

/// Base radar moment. Unknown names are preserved for forward compatibility.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum MomentType {
    Reflectivity,
    /// Uncalibrated power relative to one squared stored I/Q unit.
    RelativePower,
    Velocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    /// Producer-defined research-radar products whose identity is known more
    /// precisely than an arbitrary string.
    Research(ResearchMoment),
    Unknown(String),
}

/// Research-radar products that must remain distinct from the operational
/// moment set.
///
/// DOW6 and DOW7 are dual-frequency systems. Their `1`, `2`, and `M` products
/// are separate first-frequency, second-frequency, and downstream-merged
/// fields; collapsing all six reflectivity names onto [`MomentType::Reflectivity`]
/// would silently discard five grids when a sweep carries the complete set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum ResearchMoment {
    DowReceivedPower {
        receiver: RadarReceiverChannel,
        frequency: DowFrequencyProduct,
    },
    DowEquivalentReflectivity {
        receiver: RadarReceiverChannel,
        frequency: DowFrequencyProduct,
    },
}

/// Receiver channel encoded by the `H` or `V` in a DOW product name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum RadarReceiverChannel {
    Horizontal,
    Vertical,
}

/// Frequency leg encoded by the final character of a DOW product name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum DowFrequencyProduct {
    Frequency1,
    Frequency2,
    /// The producer's downstream merge, not an inferred arithmetic mean.
    Merged,
}

impl ResearchMoment {
    /// Resolve an exact producer token. Callers normalize case and whitespace;
    /// this function deliberately does not accept aliases that could collapse
    /// two independently sampled fields.
    pub fn from_producer_name(name: &str) -> Option<Self> {
        use DowFrequencyProduct::{Frequency1, Frequency2, Merged};
        use RadarReceiverChannel::{Horizontal, Vertical};

        let moment = match name {
            "DBMH1" => Self::DowReceivedPower {
                receiver: Horizontal,
                frequency: Frequency1,
            },
            "DBMH2" => Self::DowReceivedPower {
                receiver: Horizontal,
                frequency: Frequency2,
            },
            "DBMHM" => Self::DowReceivedPower {
                receiver: Horizontal,
                frequency: Merged,
            },
            "DBMV1" => Self::DowReceivedPower {
                receiver: Vertical,
                frequency: Frequency1,
            },
            "DBMV2" => Self::DowReceivedPower {
                receiver: Vertical,
                frequency: Frequency2,
            },
            "DBMVM" => Self::DowReceivedPower {
                receiver: Vertical,
                frequency: Merged,
            },
            "DBZH1" => Self::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Frequency1,
            },
            "DBZH2" => Self::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Frequency2,
            },
            "DBZHM" => Self::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Merged,
            },
            "DBZV1" => Self::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Frequency1,
            },
            "DBZV2" => Self::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Frequency2,
            },
            "DBZVM" => Self::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Merged,
            },
            _ => return None,
        };
        Some(moment)
    }

    pub const fn short_name(self) -> &'static str {
        use DowFrequencyProduct::{Frequency1, Frequency2, Merged};
        use RadarReceiverChannel::{Horizontal, Vertical};
        match self {
            Self::DowReceivedPower {
                receiver: Horizontal,
                frequency: Frequency1,
            } => "DBMH1",
            Self::DowReceivedPower {
                receiver: Horizontal,
                frequency: Frequency2,
            } => "DBMH2",
            Self::DowReceivedPower {
                receiver: Horizontal,
                frequency: Merged,
            } => "DBMHM",
            Self::DowReceivedPower {
                receiver: Vertical,
                frequency: Frequency1,
            } => "DBMV1",
            Self::DowReceivedPower {
                receiver: Vertical,
                frequency: Frequency2,
            } => "DBMV2",
            Self::DowReceivedPower {
                receiver: Vertical,
                frequency: Merged,
            } => "DBMVM",
            Self::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Frequency1,
            } => "DBZH1",
            Self::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Frequency2,
            } => "DBZH2",
            Self::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Merged,
            } => "DBZHM",
            Self::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Frequency1,
            } => "DBZV1",
            Self::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Frequency2,
            } => "DBZV2",
            Self::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Merged,
            } => "DBZVM",
        }
    }
}

impl MomentType {
    pub fn from_nexrad_name(name: &str) -> Self {
        match name.trim() {
            "REF" => Self::Reflectivity,
            "PWR_REL" => Self::RelativePower,
            "VEL" => Self::Velocity,
            "SW" => Self::SpectrumWidth,
            "ZDR" => Self::DifferentialReflectivity,
            "RHO" => Self::CorrelationCoefficient,
            "PHI" => Self::DifferentialPhase,
            "KDP" => Self::SpecificDifferentialPhase,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn from_nexrad_bytes(name: &[u8]) -> Self {
        match name {
            b"REF" => return Self::Reflectivity,
            b"PWR_REL" => return Self::RelativePower,
            b"VEL" => return Self::Velocity,
            b"SW " | b"SW" => return Self::SpectrumWidth,
            b"ZDR" => return Self::DifferentialReflectivity,
            b"RHO" => return Self::CorrelationCoefficient,
            b"PHI" => return Self::DifferentialPhase,
            b"KDP" => return Self::SpecificDifferentialPhase,
            _ => {}
        }

        match trim_ascii_name(name) {
            b"REF" => Self::Reflectivity,
            b"PWR_REL" => Self::RelativePower,
            b"VEL" => Self::Velocity,
            b"SW" => Self::SpectrumWidth,
            b"ZDR" => Self::DifferentialReflectivity,
            b"RHO" => Self::CorrelationCoefficient,
            b"PHI" => Self::DifferentialPhase,
            b"KDP" => Self::SpecificDifferentialPhase,
            other => Self::Unknown(String::from_utf8_lossy(other).into_owned()),
        }
    }

    pub fn short_name(&self) -> &str {
        match self {
            Self::Reflectivity => "REF",
            Self::RelativePower => "PWR_REL",
            Self::Velocity => "VEL",
            Self::SpectrumWidth => "SW",
            Self::DifferentialReflectivity => "ZDR",
            Self::CorrelationCoefficient => "RHO",
            Self::DifferentialPhase => "PHI",
            Self::SpecificDifferentialPhase => "KDP",
            Self::Research(moment) => moment.short_name(),
            Self::Unknown(name) => name.as_str(),
        }
    }
}

fn trim_ascii_name(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(0 | b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(0 | b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

impl fmt::Display for MomentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short_name())
    }
}

/// Product identifier used by future base and derived-product registries.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct ProductId(pub String);

impl From<MomentType> for ProductId {
    fn from(moment: MomentType) -> Self {
        Self(moment.short_name().to_owned())
    }
}

/// What the signal processor recombined before the moment was written.
///
/// Decoded from the CONTROL FLAGS byte of the generic data moment header
/// (NEXRAD ICD 2620002W, Build 22.0, 05 June 2023, Table XVII-B, "Data Block
/// (Descriptor of Generic Data Moment Type)", byte 18, Code*1). The ICD gives
/// four codes: 0 none, 1 recombined azimuthal radials, 2 recombined range
/// gates, 3 recombined radials and range gates to legacy resolution.
///
/// Recombination is not the same thing as a coarse sweep. A cut collected
/// natively at 1.0 degree azimuth reports code 0, because nothing was
/// combined - it was never finer. Only the codes above mean the data on disk
/// is coarser than what the radar measured.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MomentRecombination {
    /// Code 0: gates and radials are as collected.
    None,
    /// Code 1: azimuthal radials were recombined.
    AzimuthalRadials,
    /// Code 2: range gates were recombined.
    RangeGates,
    /// Code 3: radials and range gates were recombined to legacy resolution.
    RadialsAndRangeGates,
    /// A code the ICD does not define, carried through rather than guessed at.
    Unknown(u8),
}

impl MomentRecombination {
    /// Decode the CONTROL FLAGS byte (ICD 2620002W Table XVII-B, byte 18).
    pub fn from_control_flags(code: u8) -> Self {
        match code {
            0 => Self::None,
            1 => Self::AzimuthalRadials,
            2 => Self::RangeGates,
            3 => Self::RadialsAndRangeGates,
            other => Self::Unknown(other),
        }
    }

    /// True when the ICD code says the stored data is coarser than what was
    /// collected. An undefined code is not claimed to mean anything.
    pub fn reduces_resolution(&self) -> bool {
        matches!(
            self,
            Self::AzimuthalRadials | Self::RangeGates | Self::RadialsAndRangeGates
        )
    }

    /// What was recombined, in words, for display beside the sweep's facts.
    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "not recombined",
            Self::AzimuthalRadials => "azimuthal radials recombined",
            Self::RangeGates => "range gates recombined",
            Self::RadialsAndRangeGates => "radials and range gates recombined to legacy resolution",
            Self::Unknown(_) => "undocumented control flag",
        }
    }
}

/// Print an SNR threshold without rounding a real setting into a wrong one.
///
/// The field is quantized to 0.125 dB (NEXRAD ICD 2620002W, Build 22.0,
/// 05 June 2023, Table XVII-B), so three decimals are lossless and any fewer
/// can misreport an operator's choice - 2.125 dB would print as "2.1". Three
/// decimals with the padding zeros trimmed gives "2.0", "3.5", and "2.125"
/// alike.
///
/// It lives here, beside the value it prints and beside
/// [`MomentRecombination::label`], rather than inside whichever application
/// happens to draw it. A number an analyst reads off one screen and quotes on
/// another has to be spelled the same both times; two copies of this rounding
/// rule is exactly how the two spellings would drift apart.
pub fn format_snr_threshold_db(threshold_db: f32) -> String {
    let mut text = format!("{threshold_db:.3}");
    while text.ends_with('0') && !text.ends_with(".0") {
        text.pop();
    }
    text
}

/// Compact moment grid for one sweep. Rows are linked back to radial indices.
///
/// `snr_threshold_db` and `recombination` describe what the operational
/// processor did to this moment before the file was written. They are
/// `Option` because only the NEXRAD generic data moment header carries them:
/// a Message 1 volume, or any of the other formats this workspace reads, has
/// no equivalent field and must show nothing rather than a made-up zero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MomentGrid {
    pub moment: MomentType,
    /// Exact field key used by the source container, when it differs from (or
    /// is more specific than) the canonical moment identity. DORADE, for
    /// example, can store `ZH1C` while its PARM description names `DBZH1`.
    /// Kept separately so canonical access never erases native access.
    #[serde(default)]
    pub producer_name: Option<String>,
    /// Acquisition-system description attached to this exact field, when its
    /// container supplies one (for example DORADE PARM bytes 16..56).
    #[serde(default)]
    pub producer_description: Option<String>,
    /// Acquisition-system unit token attached to this exact field. Kept as
    /// text because unknown research fields must not have units inferred from
    /// their mnemonic.
    #[serde(default)]
    pub producer_units: Option<String>,
    pub gate_range: GateRange,
    pub scale: f32,
    pub offset: f32,
    pub nodata: Option<u16>,
    pub range_folded: Option<u16>,
    /// Signal-to-noise ratio, in dB, below which the processor censored gates
    /// out of this moment. An adaptable site parameter, not a constant.
    pub snr_threshold_db: Option<f32>,
    /// Whether this moment's gates or radials were combined before writing.
    pub recombination: Option<MomentRecombination>,
    pub radial_indices: Vec<usize>,
    pub storage: MomentStorage,
}

impl MomentGrid {
    pub fn new_u8(
        moment: MomentType,
        gate_range: GateRange,
        scale: f32,
        offset: f32,
        nodata: Option<u8>,
        range_folded: Option<u8>,
    ) -> Self {
        Self {
            moment,
            producer_name: None,
            producer_description: None,
            producer_units: None,
            gate_range,
            scale,
            offset,
            nodata: nodata.map(u16::from),
            range_folded: range_folded.map(u16::from),
            snr_threshold_db: None,
            recombination: None,
            radial_indices: Vec::new(),
            storage: MomentStorage::U8(Vec::new()),
        }
    }

    pub fn new_u16(
        moment: MomentType,
        gate_range: GateRange,
        scale: f32,
        offset: f32,
        nodata: Option<u16>,
        range_folded: Option<u16>,
    ) -> Self {
        Self {
            moment,
            producer_name: None,
            producer_description: None,
            producer_units: None,
            gate_range,
            scale,
            offset,
            nodata,
            range_folded,
            snr_threshold_db: None,
            recombination: None,
            radial_indices: Vec::new(),
            storage: MomentStorage::U16(Vec::new()),
        }
    }

    pub fn radial_count(&self) -> usize {
        self.radial_indices.len()
    }

    pub fn reserve_rows(&mut self, additional_rows: usize) {
        self.radial_indices.reserve(additional_rows);
        let additional_values = additional_rows.saturating_mul(self.gate_range.gate_count);
        match &mut self.storage {
            MomentStorage::U8(values) => values.reserve(additional_values),
            MomentStorage::U16(values) => values.reserve(additional_values),
            MomentStorage::F32(values) => values.reserve(additional_values),
        }
    }

    pub fn push_row(&mut self, radial_index: usize, row: MomentRow) -> Result<(), MomentGridError> {
        if row.len() > self.gate_range.gate_count {
            self.expand_gate_count(row.len());
        }

        match (&mut self.storage, row) {
            (MomentStorage::U8(values), MomentRow::U8(mut row)) => {
                row.resize(self.gate_range.gate_count, self.nodata.unwrap_or(0) as u8);
                values.extend(row);
            }
            (MomentStorage::U16(values), MomentRow::U16(mut row)) => {
                row.resize(self.gate_range.gate_count, self.nodata.unwrap_or(0));
                values.extend(row);
            }
            (MomentStorage::F32(values), MomentRow::F32(mut row)) => {
                row.resize(self.gate_range.gate_count, f32::NAN);
                values.extend(row);
            }
            (storage, row) => {
                return Err(MomentGridError::StorageMismatch {
                    expected: storage.word_size_bits(),
                    actual: row.word_size_bits(),
                });
            }
        }
        self.radial_indices.push(radial_index);
        Ok(())
    }

    pub fn push_u8_row_slice(
        &mut self,
        radial_index: usize,
        row: &[u8],
    ) -> Result<(), MomentGridError> {
        if row.len() > self.gate_range.gate_count {
            self.expand_gate_count(row.len());
        }

        let MomentStorage::U8(values) = &mut self.storage else {
            return Err(MomentGridError::StorageMismatch {
                expected: self.storage.word_size_bits(),
                actual: 8,
            });
        };

        values.extend_from_slice(row);
        if row.len() < self.gate_range.gate_count {
            values.resize(
                values.len() + (self.gate_range.gate_count - row.len()),
                self.nodata.unwrap_or(0) as u8,
            );
        }
        self.radial_indices.push(radial_index);
        Ok(())
    }

    pub fn push_u16_be_row_bytes(
        &mut self,
        radial_index: usize,
        row: &[u8],
    ) -> Result<(), MomentGridError> {
        if !row.len().is_multiple_of(2) {
            return Err(MomentGridError::InvalidRowByteLength {
                word_size_bits: 16,
                byte_len: row.len(),
            });
        }

        let row_gate_count = row.len() / 2;
        if row_gate_count > self.gate_range.gate_count {
            self.expand_gate_count(row_gate_count);
        }

        let expected = self.storage.word_size_bits();
        let MomentStorage::U16(values) = &mut self.storage else {
            return Err(MomentGridError::StorageMismatch {
                expected,
                actual: 16,
            });
        };

        values.extend(
            row.chunks_exact(2)
                .map(|gate| u16::from_be_bytes([gate[0], gate[1]])),
        );
        if row_gate_count < self.gate_range.gate_count {
            values.resize(
                values.len() + (self.gate_range.gate_count - row_gate_count),
                self.nodata.unwrap_or(0),
            );
        }
        self.radial_indices.push(radial_index);
        Ok(())
    }

    pub fn scaled_value(&self, row_index: usize, gate_index: usize) -> Option<f32> {
        if gate_index >= self.gate_range.gate_count {
            return None;
        }

        let index = row_index
            .checked_mul(self.gate_range.gate_count)?
            .checked_add(gate_index)?;

        match &self.storage {
            MomentStorage::U8(values) => {
                let raw = u16::from(*values.get(index)?);
                self.scale_raw(raw)
            }
            MomentStorage::U16(values) => {
                let raw = *values.get(index)?;
                self.scale_raw(raw)
            }
            MomentStorage::F32(values) => values.get(index).copied(),
        }
    }

    fn scale_raw(&self, raw: u16) -> Option<f32> {
        if self.nodata == Some(raw) || self.range_folded == Some(raw) {
            return None;
        }
        Some((raw as f32 - self.offset) / self.scale)
    }

    fn expand_gate_count(&mut self, new_gate_count: usize) {
        let old_gate_count = self.gate_range.gate_count;
        if new_gate_count <= old_gate_count {
            return;
        }

        let rows = self.radial_indices.len();
        if rows == 0 {
            self.gate_range.gate_count = new_gate_count;
            return;
        }

        match &mut self.storage {
            MomentStorage::U8(values) => {
                let fill = self.nodata.unwrap_or(0) as u8;
                *values = expand_rows(values, rows, old_gate_count, new_gate_count, fill);
            }
            MomentStorage::U16(values) => {
                let fill = self.nodata.unwrap_or(0);
                *values = expand_rows(values, rows, old_gate_count, new_gate_count, fill);
            }
            MomentStorage::F32(values) => {
                *values = expand_rows(values, rows, old_gate_count, new_gate_count, f32::NAN);
            }
        }
        self.gate_range.gate_count = new_gate_count;
    }
}

fn expand_rows<T: Copy>(
    values: &[T],
    rows: usize,
    old_gate_count: usize,
    new_gate_count: usize,
    fill: T,
) -> Vec<T> {
    let mut expanded = Vec::with_capacity(rows * new_gate_count);
    for row in values.chunks(old_gate_count).take(rows) {
        expanded.extend_from_slice(row);
        expanded.resize(expanded.len() + (new_gate_count - old_gate_count), fill);
    }
    expanded
}

/// Backing storage for a moment grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MomentStorage {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl MomentStorage {
    pub fn word_size_bits(&self) -> u8 {
        match self {
            Self::U8(_) => 8,
            Self::U16(_) => 16,
            Self::F32(_) => 32,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One decoded row of moment values.
#[derive(Clone, Debug, PartialEq)]
pub enum MomentRow {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl MomentRow {
    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn word_size_bits(&self) -> u8 {
        match self {
            Self::U8(_) => 8,
            Self::U16(_) => 16,
            Self::F32(_) => 32,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MomentGridError {
    GateCountMismatch { expected: usize, actual: usize },
    StorageMismatch { expected: u8, actual: u8 },
    InvalidRowByteLength { word_size_bits: u8, byte_len: usize },
}

impl fmt::Display for MomentGridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GateCountMismatch { expected, actual } => {
                write!(f, "gate count mismatch: expected {expected}, got {actual}")
            }
            Self::StorageMismatch { expected, actual } => {
                write!(
                    f,
                    "moment storage mismatch: expected {expected}-bit, got {actual}-bit"
                )
            }
            Self::InvalidRowByteLength {
                word_size_bits,
                byte_len,
            } => {
                write!(
                    f,
                    "{word_size_bits}-bit moment row has invalid byte length {byte_len}"
                )
            }
        }
    }
}

impl Error for MomentGridError {}

/// Volume Coverage Pattern metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VcpInfo {
    pub pattern: u16,
}

/// Provenance and decode statistics.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VolumeMetadata {
    pub source_path: Option<String>,
    pub archive_version: Option<String>,
    pub compression: Option<String>,
    pub message_count: usize,
    pub decoded_radial_count: usize,
    pub skipped_message_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every CONTROL FLAGS code in NEXRAD ICD 2620002W Table XVII-B, and the
    /// words each one turns into. Codes 1-3 are pinned here because no real
    /// volume this workspace has decoded uses them - a VCP 212 file is code 0
    /// on all 46,440 of its moment blocks, including its natively 1.0-degree
    /// batch cuts - so the mapping has no other proof.
    #[test]
    fn control_flags_map_to_recombination_text() {
        assert_eq!(
            MomentRecombination::from_control_flags(0),
            MomentRecombination::None
        );
        assert_eq!(
            MomentRecombination::from_control_flags(1),
            MomentRecombination::AzimuthalRadials
        );
        assert_eq!(
            MomentRecombination::from_control_flags(2),
            MomentRecombination::RangeGates
        );
        assert_eq!(
            MomentRecombination::from_control_flags(3),
            MomentRecombination::RadialsAndRangeGates
        );
        assert_eq!(
            MomentRecombination::from_control_flags(4),
            MomentRecombination::Unknown(4)
        );
        assert_eq!(
            MomentRecombination::from_control_flags(255),
            MomentRecombination::Unknown(255)
        );

        assert!(!MomentRecombination::None.reduces_resolution());
        assert!(MomentRecombination::AzimuthalRadials.reduces_resolution());
        assert!(MomentRecombination::RangeGates.reduces_resolution());
        assert!(MomentRecombination::RadialsAndRangeGates.reduces_resolution());
        // An undefined code is not evidence of anything, so it must not be
        // reported as a resolution loss.
        assert!(!MomentRecombination::Unknown(7).reduces_resolution());

        assert_eq!(MomentRecombination::None.label(), "not recombined");
        assert_eq!(
            MomentRecombination::AzimuthalRadials.label(),
            "azimuthal radials recombined"
        );
        assert_eq!(
            MomentRecombination::RangeGates.label(),
            "range gates recombined"
        );
        assert_eq!(
            MomentRecombination::RadialsAndRangeGates.label(),
            "radials and range gates recombined to legacy resolution"
        );
        assert_eq!(
            MomentRecombination::Unknown(9).label(),
            "undocumented control flag"
        );
    }

    /// The words an SNR threshold is read out in. 0.125 dB is the field's
    /// quantum, so every value it can hold has to survive the trip through
    /// the string rather than being rounded into a setting no operator dialled
    /// in.
    #[test]
    fn snr_threshold_text_keeps_one_decimal_and_loses_nothing() {
        assert_eq!(format_snr_threshold_db(2.0), "2.0");
        assert_eq!(format_snr_threshold_db(3.5), "3.5");
        assert_eq!(format_snr_threshold_db(2.125), "2.125");
        assert_eq!(format_snr_threshold_db(0.25), "0.25");
        assert_eq!(format_snr_threshold_db(20.0), "20.0");
        assert_eq!(format_snr_threshold_db(-12.0), "-12.0");
        assert_eq!(format_snr_threshold_db(0.0), "0.0");
    }

    /// A grid built by either constructor starts with no censoring facts, so
    /// a decoder that cannot supply them cannot accidentally imply a 0.0 dB
    /// threshold or an un-recombined sweep.
    #[test]
    fn new_moment_grids_claim_no_censoring_facts() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 250,
            gate_count: 1,
        };
        let u8_grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        let u16_grid = MomentGrid::new_u16(
            MomentType::DifferentialPhase,
            gate_range,
            2.8361,
            2.0,
            Some(0),
            Some(1),
        );

        assert_eq!(u8_grid.snr_threshold_db, None);
        assert_eq!(u8_grid.recombination, None);
        assert_eq!(u16_grid.snr_threshold_db, None);
        assert_eq!(u16_grid.recombination, None);
    }

    #[test]
    fn moment_grid_scales_compact_u8_rows() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 3,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        grid.push_row(0, MomentRow::U8(vec![0, 66, 80])).unwrap();

        assert_eq!(grid.radial_count(), 1);
        assert_eq!(grid.scaled_value(0, 0), None);
        assert_eq!(grid.scaled_value(0, 1), Some(0.0));
        assert_eq!(grid.scaled_value(0, 2), Some(7.0));
    }

    #[test]
    fn moment_grid_expands_and_pads_variable_gate_rows() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 2,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        grid.push_row(0, MomentRow::U8(vec![66, 80])).unwrap();
        grid.push_row(1, MomentRow::U8(vec![66, 80, 90])).unwrap();

        assert_eq!(grid.gate_range.gate_count, 3);
        assert_eq!(grid.scaled_value(0, 2), None);
        assert_eq!(grid.scaled_value(1, 2), Some(12.0));
    }

    #[test]
    fn moment_grid_pushes_u8_slice_without_row_allocation() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            129.0,
            Some(0),
            Some(1),
        );

        grid.push_u8_row_slice(2, &[129, 139]).unwrap();

        assert_eq!(grid.radial_indices, vec![2]);
        assert_eq!(grid.radial_count(), 1);
        assert_eq!(grid.scaled_value(0, 0), Some(0.0));
        assert_eq!(grid.scaled_value(0, 1), Some(5.0));
        assert_eq!(grid.scaled_value(0, 2), None);
        assert_eq!(grid.scaled_value(0, 3), None);
    }

    #[test]
    fn moment_grid_pushes_u16_be_bytes_without_row_allocation() {
        let mut grid = MomentGrid::new_u16(
            MomentType::DifferentialPhase,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            64.0,
            Some(0),
            Some(1),
        );

        grid.push_u16_be_row_bytes(2, &[0, 80, 0, 100, 0, 120])
            .unwrap();

        let MomentStorage::U16(values) = &grid.storage else {
            panic!("expected u16 storage");
        };
        assert_eq!(grid.radial_indices, vec![2]);
        assert_eq!(values, &vec![80, 100, 120, 0]);
        assert_eq!(grid.scaled_value(0, 0), Some(8.0));
        assert_eq!(grid.scaled_value(0, 3), None);
    }

    #[test]
    fn moment_grid_reserves_rows_and_gate_storage() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 3,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        grid.reserve_rows(4);

        assert!(grid.radial_indices.capacity() >= 4);
        let MomentStorage::U8(values) = &grid.storage else {
            panic!("expected u8 storage");
        };
        assert!(values.capacity() >= 12);
    }

    #[test]
    fn cut_tracks_available_moments() {
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid::new_u8(
                MomentType::Velocity,
                GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 250,
                    gate_count: 1,
                },
                2.0,
                129.0,
                Some(0),
                Some(1),
            ),
        );

        assert!(cut.moments_available().contains(&MomentType::Velocity));
    }

    #[test]
    fn moment_type_parses_padded_nexrad_bytes() {
        assert_eq!(
            MomentType::from_nexrad_bytes(b"SW "),
            MomentType::SpectrumWidth
        );
        assert_eq!(
            MomentType::from_nexrad_bytes(b"\0VEL"),
            MomentType::Velocity
        );
    }

    #[test]
    fn relative_power_has_one_stable_source_name() {
        assert_eq!(
            MomentType::from_nexrad_name(" PWR_REL "),
            MomentType::RelativePower
        );
        assert_eq!(
            MomentType::from_nexrad_bytes(b"PWR_REL\0"),
            MomentType::RelativePower
        );
        assert_eq!(MomentType::RelativePower.short_name(), "PWR_REL");
    }

    #[test]
    fn research_moments_keep_every_producer_name_distinct() {
        use DowFrequencyProduct::{Frequency1, Frequency2, Merged};
        use RadarReceiverChannel::{Horizontal, Vertical};

        let moments = [
            ResearchMoment::DowReceivedPower {
                receiver: Horizontal,
                frequency: Frequency1,
            },
            ResearchMoment::DowReceivedPower {
                receiver: Horizontal,
                frequency: Frequency2,
            },
            ResearchMoment::DowReceivedPower {
                receiver: Horizontal,
                frequency: Merged,
            },
            ResearchMoment::DowReceivedPower {
                receiver: Vertical,
                frequency: Frequency1,
            },
            ResearchMoment::DowReceivedPower {
                receiver: Vertical,
                frequency: Frequency2,
            },
            ResearchMoment::DowReceivedPower {
                receiver: Vertical,
                frequency: Merged,
            },
            ResearchMoment::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Frequency1,
            },
            ResearchMoment::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Frequency2,
            },
            ResearchMoment::DowEquivalentReflectivity {
                receiver: Horizontal,
                frequency: Merged,
            },
            ResearchMoment::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Frequency1,
            },
            ResearchMoment::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Frequency2,
            },
            ResearchMoment::DowEquivalentReflectivity {
                receiver: Vertical,
                frequency: Merged,
            },
        ];
        let names: std::collections::BTreeSet<&str> =
            moments.iter().map(|moment| moment.short_name()).collect();
        assert_eq!(names.len(), moments.len());
        assert_eq!(names.first(), Some(&"DBMH1"));
    }

    #[test]
    fn volume_can_keep_repeated_elevation_cuts_separate() {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());

        volume.push_cut(0.5, Some(1));
        volume.push_cut(0.5, Some(1));

        assert_eq!(volume.cuts.len(), 2);
        let latest = volume.find_or_insert_cut(0.5, Some(1));
        latest.elevation_deg = 0.55;

        assert_eq!(volume.cuts[0].elevation_deg, 0.5);
        assert_eq!(volume.cuts[1].elevation_deg, 0.55);
    }
}
