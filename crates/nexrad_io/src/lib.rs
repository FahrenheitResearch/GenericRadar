//! NEXRAD Archive II / Level II decoder entry points, plus the magic-byte
//! seam that routes a buffer of unknown provenance to the decoder that owns
//! its container format.
//!
//! Two radial message types are decoded: the modern generic Message Type 31
//! (1 or 0.5 degree azimuth, up to ten moments) and the legacy Message Type 1
//! that pre-2008 volumes are written in (REF/VEL/SW only, fixed resolutions).
//! Unsupported records stay non-fatal so an app can inspect partially decoded
//! volumes while the edge-case corpus grows.
//!
//! Message layouts follow the NEXRAD RDA/RPG Interface Control Document
//! ICD 2620002 (Message Type 1: "Digital Radar Data"; Message Type 31:
//! "Digital Radar Data Generic Format"). Container magic numbers for the
//! non-NEXRAD formats are cited on [`SupportedVolumeFormat`].

// One module per container format this crate can read. The Archive II /
// Level II decoder itself lives in this file; everything else is sniffed by
// [`sniff_supported_volume_format`] and routed to one of these.
pub mod cfradial;
pub mod dorade;
pub mod hdf5lite;
pub mod iq;
/// The Level 1 moment and Doppler-spectrum processor. Level II arrives with
/// its moments already estimated; Level 1 arrives as pulses, so the estimator
/// that the signal processor would have run lives here.
pub mod iq_moments;
pub mod mobile_archive;
pub mod netcdf3;
pub mod netcdf4;
pub mod odim;

use std::collections::btree_map::Entry;
use std::fs;
use std::io::{Cursor, Read};
use std::mem::MaybeUninit;
use std::path::Path;
use std::ptr::NonNull;

use bzip2::bufread::BzDecoder;
use chrono::{DateTime, TimeZone, Utc};
use flate2::read::GzDecoder;
use radar_core::{
    GateRange, MomentGrid, MomentRecombination, MomentType, RadarSite, RadarVolume, Radial,
    RadialStatus, VcpInfo,
};
use rayon::prelude::*;
use thiserror::Error;

const VOLUME_HEADER_LEN: usize = 24;
const CONTROL_WORD_LEN: usize = 12;
const MESSAGE_HEADER_LEN: usize = 16;
const RECORD_BYTES: usize = 2432;
const MSG_1_HEADER_LEN: usize = 100;
const MSG_31_HEADER_LEN: usize = 72;
const GENERIC_DATA_BLOCK_LEN: usize = 28;
/// Counts per dB in the SNR THRESHOLD halfword of a generic data moment
/// header (NEXRAD ICD 2620002W, Build 22.0, 05 June 2023, Table XVII-B,
/// bytes 16-17, Scaled SInteger*2, dB, range -12.0 to +20.0).
///
/// The ICD does not settle the scale on its own: that row's
/// ACCURACY/PRECISION cell reads "0.1/0.125", which are different scales.
/// Real Archive II volumes do settle it. A VCP 212 volume carries exactly two
/// raw values, 16 on its contiguous-surveillance halves and 28 on its Doppler
/// and batch cuts. At 0.125 dB per count those are 2.0 dB and 3.5 dB - round
/// operational settings, and 2.0 dB is the value the ICD itself gives as
/// typical. At 0.1 dB per count they would be 1.6 dB and 2.8 dB, which no
/// operator would dial in. Hence 8 counts per dB.
const SNR_THRESHOLD_COUNTS_PER_DB: f32 = 8.0;
const VOLUME_CONSTANT_BLOCK_LEN: usize = 44;
const RADIAL_CONSTANT_BLOCK_LEN: usize = 20;
const HALF_DEGREE_RADIALS_PER_CUT: usize = 720;
const ONE_DEGREE_RADIALS_PER_CUT: usize = 360;
const FALLBACK_RADIALS_PER_CUT: usize = 760;
const MAX_MESSAGE_31_MOMENTS: usize = 10;
const BZIP_BLOCK_DECODE_CAPACITY_HINT: usize = RECORD_BYTES * 102;
const BZIP_PREVIEW_MAX_BLOCKS: usize = 12;
const GZIP_TRAILER_LEN: usize = 8;
const MAX_GZIP_PREALLOC_RATIO: usize = 128;

pub type Result<T> = std::result::Result<T, NexradError>;

#[derive(Debug, Error)]
pub enum NexradError {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("input is too short for an Archive II volume header: {actual} bytes")]
    ShortVolumeHeader { actual: usize },
    #[error("truncated {what} at offset {offset}: need {needed} bytes, have {available}")]
    Truncated {
        what: &'static str,
        offset: usize,
        needed: usize,
        available: usize,
    },
    #[error("unsupported or corrupt compression wrapper: {0}")]
    Compression(String),
    #[error("invalid message at offset {offset}: {reason}")]
    InvalidMessage { offset: usize, reason: String },
    #[error("moment grid error: {0}")]
    MomentGrid(#[from] radar_core::MomentGridError),
    /// A recognised non-NEXRAD container whose own decoder rejected the
    /// bytes.
    ///
    /// The container name is kept in front of the decoder's complaint
    /// because the first useful question about a failed load is what the
    /// file was taken to be: an unreadable ODIM_H5 volume should not read
    /// like a broken Archive II one. The Archive II arm is deliberately NOT
    /// wrapped - it is the fallthrough for bytes nothing recognised, and its
    /// messages are the existing, familiar error surface.
    #[error("{format}: {source}")]
    Format {
        format: &'static str,
        #[source]
        source: Box<NexradError>,
    },
    /// A container that was recognised correctly and holds something other
    /// than a radar volume.
    ///
    /// NEXRAD Level 1 / I/Q is the case this exists for. It is real radar
    /// data, correctly identified, but it carries the transmitted pulses
    /// rather than the estimated moments, so there is no volume for a volume
    /// decoder to return. Saying that is far more use to whoever opened the
    /// file than the Archive II decoder's complaint about a missing tape
    /// identifier would be.
    #[error("{format}: {detail}")]
    NotAVolume {
        format: &'static str,
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveCompression {
    Gzip,
    Bzip2WholeFile,
    Bzip2Blocks,
    Uncompressed,
}

impl ArchiveCompression {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Bzip2WholeFile => "bzip2-whole-file",
            Self::Bzip2Blocks => "bzip2-blocks",
            Self::Uncompressed => "uncompressed",
        }
    }
}

/// A single-buffer radar container this workspace knows how to identify.
///
/// Identification and decoding are deliberately separate: the sniff is the
/// shared routing contract used by local file open, drag-and-drop and URL
/// polling, and a caller can ask what a file is without paying to decode it.
/// Every variant is decodable - [`decode_supported_volumes_bytes`] hands
/// each one to the module that owns it - and a decoder's failure is wrapped
/// in a [`NexradError::Format`] that still names the container.
///
/// Magic numbers, in the order the sniff tests them:
///
/// * [`Self::MobileDeploymentZip`] — `PK\x03\x04`, the ZIP local file header
///   (PKWARE .ZIP File Format Specification, section 4.3.7). Mobile
///   deployments ship a scan as a zipped bundle of sweepfiles.
/// * [`Self::OdimH5`] — `\x89HDF\r\n\x1a\n`, the HDF5 superblock signature
///   (HDF5 File Format Specification, "Level 0A - Format Signature").
///   HDF5 is a CONTAINER, and this workspace reads two radar formats out of
///   it: ODIM_H5 PVOL/SCAN per the EUMETNET OPERA Data Information Model,
///   and CfRadial 1.x in a netCDF-4 container. The signature cannot tell
///   them apart — nothing in the first eight bytes can — so the decode arm
///   opens the file and asks, and this variant means "an HDF5 radar
///   container" rather than a promise of ODIM.
/// * [`Self::CfRadial1`] — `CDF\x01` / `CDF\x02`, classic netCDF (NetCDF
///   Classic Format Specification), decoded per the NCAR CfRadial 1.x
///   convention. `CDF\x05` (CDF-5, 64-bit data) is routed here too — not
///   because it decodes, but because the netCDF reader's refusal names the
///   format and the conversion, and the Archive II fallthrough's does not.
/// * [`Self::Dorade`] — a leading `COMM`/`SSWB`/`VOLD`/`RADD` descriptor
///   name followed by a block length that is valid in at least one byte
///   order (NCAR/EOL DORADE sweepfile format). DORADE has no file-level
///   magic, so the length check is what keeps the four ASCII names from
///   matching arbitrary text.
/// * [`Self::NexradLevel2`] — `AR2V` or `ARCHIVE2` tape identifiers
///   (ICD 2620002 Archive II volume header), or a whole-file `\x1f\x8b`
///   gzip / `BZh` bzip2 wrapper around one.
/// * [`Self::NexradLevel1TimeSeries`] — a leading `rvp8PulseInfo start` or
///   `rvptsPulseInfo start` line (Vaisala RVP8/RVP900 TS record, the format
///   NEXRAD Level 1 / I/Q is archived in). Recognised so it can be REPORTED
///   as what it is; it holds pulses rather than moments, so it is not a
///   radar volume and [`decode_supported_volumes_bytes`] declines it in
///   those words rather than letting the Archive II decoder call it corrupt.
///   [`iq`] is the reader for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportedVolumeFormat {
    NexradLevel2,
    NexradLevel1TimeSeries,
    OdimH5,
    Dorade,
    CfRadial1,
    MobileDeploymentZip,
}

impl SupportedVolumeFormat {
    /// Human-readable format name, used in error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::NexradLevel2 => "NEXRAD Archive II",
            Self::NexradLevel1TimeSeries => "NEXRAD Level 1 time series (RVP8/RVP900)",
            Self::OdimH5 => "ODIM_H5",
            Self::Dorade => "DORADE",
            Self::CfRadial1 => "CfRadial 1.x",
            Self::MobileDeploymentZip => "mobile deployment zip",
        }
    }
}

/// The two tape identifiers an Archive II volume may open with.
///
/// `AR2V` is the modern one (ICD 2620002 "Archive II volume header record",
/// `AR2Vnnnn.mmm`); `ARCHIVE2` is what pre-2008 NCDC tapes carry. BOTH are
/// Archive II and both decode, so every sniff in this crate has to test the
/// pair — the router did while [`mobile_archive`]'s member sniff tested only
/// `AR2V`, which silently dropped a legacy volume out of a deployment zip.
/// One shared constant is what keeps those two readings identical.
pub(crate) const ARCHIVE_II_MAGICS: [&[u8]; 2] = [b"AR2V", b"ARCHIVE2"];

/// `true` when the buffer opens with either Archive II tape identifier.
pub(crate) fn starts_with_archive_ii_magic(bytes: &[u8]) -> bool {
    ARCHIVE_II_MAGICS
        .iter()
        .any(|magic| bytes.starts_with(magic))
}

/// The four ASCII descriptor names a DORADE sweepfile may open with.
const DORADE_LEAD_DESCRIPTORS: [&[u8; 4]; 4] = [b"COMM", b"SSWB", b"VOLD", b"RADD"];
/// Name plus 4-byte length: the fixed part of a DORADE descriptor block.
const DORADE_BLOCK_HEADER_LEN: usize = 8;

/// Identify a radar container by its leading bytes.
///
/// `None` means "no signature matched". Callers that hold real file bytes
/// should treat `None` the way [`decode_supported_volume_bytes`] does — as
/// an Archive II candidate — because an Archive II volume whose tape
/// identifier is neither `AR2V` nor `ARCHIVE2` is still worth handing to the
/// Level II parser, which produces a specific error of its own. `None` is
/// kept distinct from `Some(NexradLevel2)` so a caller that wants to *know*
/// whether anything was recognised (a file picker, a diagnostic) can ask.
///
/// This reads only the head of the buffer and never decompresses. A gzipped
/// ODIM_H5 file therefore sniffs as `NexradLevel2` from its outer bytes;
/// pass inflated bytes when you want the inner format. The router does that
/// peek for you.
pub fn sniff_supported_volume_format(head: &[u8]) -> Option<SupportedVolumeFormat> {
    if head.starts_with(b"PK\x03\x04") {
        return Some(SupportedVolumeFormat::MobileDeploymentZip);
    }
    if head.starts_with(b"\x89HDF\r\n\x1a\n") {
        return Some(SupportedVolumeFormat::OdimH5);
    }
    // The netCDF reader owns which `CDF` versions it recognises, INCLUDING
    // the CDF-5 it can only refuse: routing a narrower set here left a
    // CDF-5 file falling through to the Archive II arm, which reported a
    // netCDF file as a truncated NEXRAD volume header while the decoder's
    // own "convert with `nccopy -k classic`" message sat unreachable.
    if netcdf3::looks_like_netcdf3_bytes(head) {
        return Some(SupportedVolumeFormat::CfRadial1);
    }
    if iq::looks_like_iq_time_series(head) {
        return Some(SupportedVolumeFormat::NexradLevel1TimeSeries);
    }
    if looks_like_dorade_head(head) {
        return Some(SupportedVolumeFormat::Dorade);
    }
    if starts_with_archive_ii_magic(head)
        || head.starts_with(&[0x1f, 0x8b])
        || head.starts_with(b"BZh")
    {
        return Some(SupportedVolumeFormat::NexradLevel2);
    }
    None
}

/// A DORADE sweepfile opens with a descriptor name and that block's length.
///
/// DORADE files carry no file-level magic number, and solo/Radx write them in
/// whichever byte order the producing machine used, so the name alone is too
/// weak a test — `COMM` is four perfectly ordinary ASCII letters. Requiring
/// the following length to be a sane block size in at least one byte order is
/// what makes the signature specific.
fn looks_like_dorade_head(head: &[u8]) -> bool {
    if head.len() < DORADE_BLOCK_HEADER_LEN {
        return false;
    }
    if !DORADE_LEAD_DESCRIPTORS
        .iter()
        .any(|name| &head[..4] == name.as_slice())
    {
        return false;
    }
    let little = i32::from_le_bytes([head[4], head[5], head[6], head[7]]) as i64;
    let big = i32::from_be_bytes([head[4], head[5], head[6], head[7]]) as i64;
    let available = head.len() as i64;
    let plausible = |len: i64| len >= DORADE_BLOCK_HEADER_LEN as i64 && len <= available;
    plausible(little) || plausible(big)
}

/// How much of a gzip member to inflate before deciding what is inside it.
///
/// Only the first descriptor block or superblock signature is needed, and
/// every signature the sniff knows lives in the first eight bytes. A small
/// peek keeps the cost of routing a gzipped Level II volume to a few
/// microseconds rather than a second inflate of the whole file.
const GZIP_SNIFF_PEEK_BYTES: usize = 512;

/// Identify a whole file, seeing through a gzip wrapper if there is one.
///
/// [`sniff_supported_volume_format`] judges only the bytes it is given, which
/// makes a gzipped ODIM_H5 file indistinguishable from a gzipped Archive II
/// volume. This variant inflates a few hundred bytes to settle that, and is
/// what both [`decode_supported_volume_bytes`] and any caller deciding how to
/// treat a file should use.
pub fn sniff_supported_volume_bytes(raw: &[u8]) -> Option<SupportedVolumeFormat> {
    let outer = sniff_supported_volume_format(raw);
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return outer;
    }
    // A gzip member that will not inflate is left to the Archive II decoder,
    // which reports the decompression failure in its own words.
    match peek_gzip_head(raw) {
        Some(inner) => sniff_supported_volume_format(&inner).or(outer),
        None => outer,
    }
}

/// Decode every radar volume in a supported container, chosen by magic bytes.
///
/// This is the one shared router for bytes of unknown provenance: local file
/// open, drag-and-drop, and custom URL polling all go through it so they
/// cannot drift apart. Unrecognised bytes are handed to the Archive II
/// decoder, which is where the useful diagnostic for a not-actually-radar
/// file comes from - so a file that is not radar data at all still fails
/// exactly the way it did before the seam existed.
///
/// The returned vector always holds at least one volume. Only a mobile
/// deployment archive can hold more than one: it is a bundle of sweeps,
/// often from several instruments at several scan times, and reducing it to
/// one volume inside the router would throw away what the analyst opened.
/// Every other container is one scan and yields exactly one volume.
///
/// A gzip wrapper is seen through: a `.h5.gz` is inflated and decoded as
/// ODIM_H5 rather than handed to the Level II decoder because its outer
/// bytes look like a compressed Archive II volume.
pub fn decode_supported_volumes_bytes(raw: &[u8]) -> Result<Vec<RadarVolume>> {
    let Some(format) = sniff_supported_volume_bytes(raw) else {
        // Nothing matched. The Archive II decoder owns this case and its
        // error is the one worth reading.
        return decode_volume_from_bytes(raw).map(|volume| vec![volume]);
    };
    if format == SupportedVolumeFormat::NexradLevel2 {
        // Includes the gzip and bzip2 wrappers, which the Level II decoder
        // unwraps for itself.
        return decode_volume_from_bytes(raw).map(|volume| vec![volume]);
    }

    // Everything below is a self-contained container that cannot unwrap its
    // own gzip, so the wrapper comes off here. The sniff has already looked
    // inside, so this only runs for a genuinely wrapped non-NEXRAD file.
    let inflated;
    let body = if raw.starts_with(&[0x1f, 0x8b]) {
        inflated = decompress_gzip_bytes(raw).map_err(|source| named(format, source))?;
        inflated.as_slice()
    } else {
        raw
    };

    if format == SupportedVolumeFormat::OdimH5 {
        // Two different radar formats share the HDF5 signature, so which one
        // this is can only be settled by looking inside.
        return hdf5_container_volume(body).map(|volume| vec![volume]);
    }
    let decoded = match format {
        SupportedVolumeFormat::NexradLevel2 => unreachable!("handled above"),
        SupportedVolumeFormat::OdimH5 => unreachable!("handled above"),
        SupportedVolumeFormat::CfRadial1 => cfradial::decode_cfradial1_volume(body),
        SupportedVolumeFormat::Dorade => dorade::decode_dorade_sweep(body),
        SupportedVolumeFormat::MobileDeploymentZip => return deployment_volumes(format, body),
        // Correctly identified, and deliberately not decoded here: a time
        // series has no moments to put in a volume. `iq::decode_iq_time_series`
        // is the reader for it.
        SupportedVolumeFormat::NexradLevel1TimeSeries => {
            return Err(time_series_not_a_volume(format, body));
        }
    };
    decoded
        .map(|volume| vec![volume])
        .map_err(|source| named(format, source))
}

/// Decode an HDF5 container as whichever radar format it turns out to hold.
///
/// `\x89HDF\r\n\x1a\n` says "this is HDF5", not what is stored in it, and
/// this workspace reads two formats that use it: ODIM_H5, and CfRadial 1.x
/// in a netCDF-4 container. The file is opened ONCE and the decoders read
/// from that same view, so the question costs one walk of the object tree
/// rather than two.
///
/// The error names the format the file was taken for, exactly as the rest of
/// the router does: a netCDF-4 CfRadial file that fails should not be
/// reported as a broken ODIM volume, which is what routing every HDF5 file
/// to ODIM used to do — and, before this, what it did to every CfRadial file
/// including the ones that were perfectly fine.
fn hdf5_container_volume(body: &[u8]) -> Result<RadarVolume> {
    let file = hdf5lite::H5File::open(body).map_err(|source| {
        // The container is HDF5 either way; below the superblock is where
        // the two formats part company, so a file that will not even open
        // is named for the signature it does carry.
        named(SupportedVolumeFormat::OdimH5, source)
    })?;
    if netcdf4::looks_like_netcdf4(&file) {
        return netcdf4::Nc4File::from_hdf5(file)
            .and_then(|source| cfradial::decode_cfradial1_source(&source))
            .map_err(|source| named(SupportedVolumeFormat::CfRadial1, source));
    }
    odim::decode_odim_h5_file(&file).map_err(|source| named(SupportedVolumeFormat::OdimH5, source))
}

/// Unpack a deployment archive into plain volumes, keeping the member label.
///
/// The archive reader knows which member each volume came from and writes it
/// into `source_path`; [`decode_supported_volume_from_path`] joins the file
/// name onto it, so a load reads `deployment.zip::swp....` rather than losing
/// the sweep's identity the moment it leaves the archive.
fn deployment_volumes(format: SupportedVolumeFormat, body: &[u8]) -> Result<Vec<RadarVolume>> {
    let members =
        mobile_archive::decode_deployment_zip(body).map_err(|source| named(format, source))?;
    if members.is_empty() {
        // The archive reader raises its own error for an archive with no
        // radar in it, so this is belt and braces rather than a live path -
        // but a router that could return an empty vector would make every
        // caller check for one, and none of them should have to.
        return Err(named(
            format,
            NexradError::InvalidMessage {
                offset: 0,
                reason: "archive holds no decodable radar volumes".to_owned(),
            },
        ));
    }
    Ok(members.into_iter().map(|member| member.volume).collect())
}

/// Explain that a correctly identified time-series record is not a volume.
///
/// The record's own header is read for the message, so an analyst who dropped
/// a Level 1 file on a viewer is told which site and acquisition they have and
/// why nothing is drawn — rather than being told the file is broken, which it
/// is not. If even the header will not parse, the reason for that is reported
/// instead.
fn time_series_not_a_volume(format: SupportedVolumeFormat, body: &[u8]) -> NexradError {
    let detail = match iq::peek_iq_time_series(body) {
        Ok(summary) => format!(
            "{} {} holds {} pulses of I/Q time series (iMajorMode {}, {} gates, \
             {} channel(s)), not estimated moments; compute moments from it with the \
             `iq` reader",
            summary.site,
            summary.task_name,
            summary.pulse_count,
            summary.major_mode,
            summary.gate_count,
            summary.channels_recorded,
        ),
        Err(error) => format!("holds I/Q time series, not estimated moments ({error})"),
    };
    NexradError::NotAVolume {
        format: format.label(),
        detail,
    }
}

/// Put the container's name in front of its decoder's complaint.
fn named(format: SupportedVolumeFormat, source: NexradError) -> NexradError {
    NexradError::Format {
        format: format.label(),
        source: Box::new(source),
    }
}

/// Decode one radar volume from any supported container, chosen by magic
/// bytes.
///
/// A pane draws one volume, so this is what the open and drop paths call.
/// For a mobile deployment archive that means the EARLIEST scan in the
/// bundle - [`decode_supported_volumes_bytes`] returns them in scan-time
/// order - which is where an analyst opening a deployment expects to start.
/// A caller that wants the whole deployment uses that function instead.
pub fn decode_supported_volume_bytes(raw: &[u8]) -> Result<RadarVolume> {
    // `decode_supported_volumes_bytes` promises at least one volume, so the
    // `None` arm is unreachable - but it is written as an error rather than
    // an index, because a router that panics when a future format returns an
    // empty list is a worse way to find that out than a message.
    decode_supported_volumes_bytes(raw)?
        .into_iter()
        .next()
        .ok_or_else(|| NexradError::InvalidMessage {
            offset: 0,
            reason: "the container decoded to no volumes at all".to_owned(),
        })
}

/// Inflate at most [`GZIP_SNIFF_PEEK_BYTES`] of a gzip member.
///
/// A short read is a success: a tiny member simply ends early. A corrupt
/// stream returns `None` so the caller falls through to the Archive II
/// decoder and reports the failure in its own words.
fn peek_gzip_head(raw: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = GzDecoder::new(raw);
    let mut head = vec![0u8; GZIP_SNIFF_PEEK_BYTES];
    let mut filled = 0;
    while filled < head.len() {
        match decoder.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(_) => return (filled > 0).then(|| head[..filled].to_vec()),
        }
    }
    head.truncate(filled);
    Some(head)
}

/// Decode a local radar file, routing on magic bytes.
///
/// The extension is not consulted: NEXRAD volumes are routinely stored with
/// no extension at all, and a `.raw` from one network is a different format
/// from a `.raw` from another.
pub fn decode_supported_volume_from_path(path: &Path) -> Result<RadarVolume> {
    let bytes = fs::read(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut volume = decode_supported_volume_bytes(&bytes)?;
    volume.metadata.source_path = Some(source_path_for(path, volume.metadata.source_path.take()));
    Ok(volume)
}

/// Where a decoded volume says it came from.
///
/// Normally the file. A container that holds several scans has already
/// written the member it chose - `swp.1090509143923.NOXPRVP...` - and that is
/// the more informative half, so the two are joined rather than the file name
/// overwriting the member. The separator matches the one the archive reader
/// uses for a deployment opened by path.
fn source_path_for(path: &Path, decoded: Option<String>) -> String {
    match decoded {
        Some(member) => format!("{}::{member}", path.display()),
        None => path.display().to_string(),
    }
}

/// Decode a local Archive II / Level II file into the shared radar model.
pub fn decode_volume_from_path(path: &Path) -> Result<RadarVolume> {
    let bytes = fs::read(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut volume = decode_volume_from_bytes(&bytes)?;
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(volume)
}

/// Decode a byte slice. This is public to support fixtures and embedded tests.
pub fn decode_volume_from_bytes(bytes: &[u8]) -> Result<RadarVolume> {
    if bytes.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader {
            actual: bytes.len(),
        });
    }
    if !bytes.starts_with(&[0x1f, 0x8b])
        && !bytes.starts_with(b"BZh")
        && let Some(decoded_blocks) = try_decompress_bzip_blocks(bytes)?
    {
        return decode_bzip_block_sequence(&bytes[..VOLUME_HEADER_LEN], &decoded_blocks);
    }

    let (bytes, compression) = normalize_archive_bytes(bytes)?;
    decode_normalized_volume_bytes(&bytes, compression)
}

pub fn decode_gzip_volume_from_reader(reader: impl Read) -> Result<RadarVolume> {
    let mut decoder = GzDecoder::new(reader);
    decode_volume_from_stream_until(&mut decoder, ArchiveCompression::Gzip, None).map(|result| {
        debug_assert!(!result.stopped_at_preview);
        result.volume
    })
}

pub fn decode_gzip_volume_from_bytes_with_preview<F>(
    raw: &[u8],
    min_displayable_radials: usize,
    on_preview: F,
) -> Result<RadarVolume>
where
    F: FnMut(RadarVolume),
{
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return decode_volume_from_bytes(raw);
    }

    let mut decoder = GzDecoder::new(raw);
    decode_volume_from_stream(
        &mut decoder,
        ArchiveCompression::Gzip,
        Some(min_displayable_radials),
        false,
        on_preview,
    )
    .map(|result| {
        debug_assert!(!result.stopped_at_preview);
        result.volume
    })
}

pub fn decode_gzip_preview_from_bytes(
    raw: &[u8],
    min_displayable_radials: usize,
) -> Result<Option<RadarVolume>> {
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Ok(None);
    }

    let mut decoder = GzDecoder::new(raw);
    let result = decode_volume_from_stream_until(
        &mut decoder,
        ArchiveCompression::Gzip,
        Some(min_displayable_radials),
    )?;
    Ok(result.stopped_at_preview.then_some(result.volume))
}

/// Decode a completed first displayable cut from NEXRAD block-bzip Level II bytes.
///
/// This is intended for UI preview on low-core machines: it returns `None` for
/// gzip, whole-file bzip, uncompressed, or malformed block-bzip inputs, and it
/// never substitutes for the final full-volume decode.
pub fn decode_bzip_block_preview_from_bytes(
    raw: &[u8],
    min_displayable_radials: usize,
) -> Result<Option<RadarVolume>> {
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }

    let Some(blocks) = collect_bzip_block_slices(raw)? else {
        return Ok(None);
    };

    let mut decoded_blocks = Vec::new();
    for compressed in blocks.into_iter().take(BZIP_PREVIEW_MAX_BLOCKS) {
        decoded_blocks.push(decompress_bzip_block(compressed)?);
        let Ok(preview) = decode_bzip_block_sequence(&raw[..VOLUME_HEADER_LEN], &decoded_blocks)
        else {
            continue;
        };
        if has_complete_displayable_cut(&preview, min_displayable_radials) {
            return Ok(Some(preview));
        }
    }

    Ok(None)
}

/// Decode a full volume while optionally emitting an early completed first-cut preview.
///
/// For block-bzip Level II files, the preview blocks are reused by the final
/// decode so the UI can show first pixels without doing that decompression work
/// twice. Other compression formats fall back to a normal full decode.
pub fn decode_volume_from_bytes_with_bzip_preview<F>(
    raw: &[u8],
    min_displayable_radials: usize,
    mut on_preview: F,
) -> Result<RadarVolume>
where
    F: FnMut(RadarVolume),
{
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }

    let Some(blocks) = collect_bzip_block_slices(raw)? else {
        return decode_volume_from_bytes(raw);
    };

    let mut decoded_blocks = Vec::new();
    let preview_limit = blocks.len().min(BZIP_PREVIEW_MAX_BLOCKS);
    for compressed in &blocks[..preview_limit] {
        decoded_blocks.push(decompress_bzip_block(compressed)?);
        let Ok(preview) = decode_bzip_block_sequence(&raw[..VOLUME_HEADER_LEN], &decoded_blocks)
        else {
            continue;
        };
        if has_complete_displayable_cut(&preview, min_displayable_radials) {
            on_preview(preview);
            break;
        }
    }

    let remaining_blocks = &blocks[decoded_blocks.len()..];
    let mut remaining_decoded = remaining_blocks
        .par_iter()
        .map(|compressed| decompress_bzip_block(compressed))
        .collect::<Result<Vec<_>>>()?;
    decoded_blocks.append(&mut remaining_decoded);

    decode_bzip_block_sequence(&raw[..VOLUME_HEADER_LEN], &decoded_blocks)
}

/// Decompress or normalize an Archive II byte slice before Level II parsing.
pub fn normalize_archive_bytes(raw: &[u8]) -> Result<(Vec<u8>, ArchiveCompression)> {
    if raw.len() < VOLUME_HEADER_LEN {
        return Err(NexradError::ShortVolumeHeader { actual: raw.len() });
    }

    if raw.starts_with(&[0x1f, 0x8b]) {
        let decoded = decompress_gzip_bytes(raw)?;
        return Ok((decoded, ArchiveCompression::Gzip));
    }

    if raw.starts_with(b"BZh") {
        let mut decoded = Vec::new();
        BzDecoder::new(Cursor::new(raw))
            .read_to_end(&mut decoded)
            .map_err(|err| NexradError::Compression(err.to_string()))?;
        return Ok((decoded, ArchiveCompression::Bzip2WholeFile));
    }

    if let Some(decoded) = try_decode_bzip_blocks(raw)? {
        return Ok((decoded, ArchiveCompression::Bzip2Blocks));
    }

    Ok((raw.to_vec(), ArchiveCompression::Uncompressed))
}

fn gzip_decoded_capacity_hint(raw: &[u8]) -> Option<usize> {
    let trailer = raw.get(raw.len().checked_sub(GZIP_TRAILER_LEN)?..)?;
    let isize = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;
    let max_reasonable = raw.len().saturating_mul(MAX_GZIP_PREALLOC_RATIO);
    (isize <= max_reasonable).then_some(isize)
}

fn decompress_gzip_bytes(raw: &[u8]) -> Result<Vec<u8>> {
    if let Some(expected_len) = gzip_decoded_capacity_hint(raw)
        && let Some(decoded) = decompress_gzip_bytes_libdeflate(raw, expected_len)
    {
        return Ok(decoded);
    }

    let mut decoded = Vec::with_capacity(gzip_decoded_capacity_hint(raw).unwrap_or(0));
    GzDecoder::new(raw)
        .read_to_end(&mut decoded)
        .map_err(|err| NexradError::Compression(err.to_string()))?;
    Ok(decoded)
}

struct LibdeflateDecompressor {
    ptr: NonNull<libdeflate_sys::libdeflate_decompressor>,
}

thread_local! {
    static LIBDEFLATE_DECOMPRESSOR: Option<LibdeflateDecompressor> =
        LibdeflateDecompressor::new();
}

impl LibdeflateDecompressor {
    fn new() -> Option<Self> {
        NonNull::new(unsafe { libdeflate_sys::libdeflate_alloc_decompressor() })
            .map(|ptr| Self { ptr })
    }
}

impl Drop for LibdeflateDecompressor {
    fn drop(&mut self) {
        unsafe {
            libdeflate_sys::libdeflate_free_decompressor(self.ptr.as_ptr());
        }
    }
}

fn decompress_gzip_bytes_libdeflate(raw: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let mut decoded = Vec::<MaybeUninit<u8>>::with_capacity(expected_len);
    let mut actual_len = 0usize;
    let result = LIBDEFLATE_DECOMPRESSOR.with(|decompressor| {
        let decompressor = decompressor.as_ref()?;
        Some(unsafe {
            libdeflate_sys::libdeflate_gzip_decompress(
                decompressor.ptr.as_ptr(),
                raw.as_ptr().cast(),
                raw.len(),
                decoded.as_mut_ptr().cast(),
                expected_len,
                &mut actual_len,
            )
        })
    })?;
    if result != libdeflate_sys::libdeflate_result_LIBDEFLATE_SUCCESS || actual_len > expected_len {
        return None;
    }

    let ptr = decoded.as_mut_ptr().cast::<u8>();
    let capacity = decoded.capacity();
    std::mem::forget(decoded);
    Some(unsafe { Vec::from_raw_parts(ptr, actual_len, capacity) })
}

fn read_record_prefix<R: Read>(reader: &mut R, buffer: &mut [u8], offset: usize) -> Result<bool> {
    let mut read = 0;
    while read < buffer.len() {
        let count = reader
            .read(&mut buffer[read..])
            .map_err(|err| NexradError::Compression(err.to_string()))?;
        if count == 0 {
            if read == 0 {
                return Ok(false);
            }
            return Err(NexradError::Truncated {
                what: "record prefix",
                offset,
                needed: buffer.len(),
                available: read,
            });
        }
        read += count;
    }
    Ok(true)
}

fn read_exact_required<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    what: &'static str,
    offset: usize,
) -> Result<()> {
    let mut read = 0;
    while read < buffer.len() {
        let count = reader
            .read(&mut buffer[read..])
            .map_err(|err| NexradError::Compression(err.to_string()))?;
        if count == 0 {
            return Err(NexradError::Truncated {
                what,
                offset,
                needed: buffer.len(),
                available: read,
            });
        }
        read += count;
    }
    Ok(())
}

fn read_exact_into_buffer<R: Read>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    len: usize,
    what: &'static str,
    offset: usize,
) -> Result<()> {
    buffer.clear();
    if buffer.capacity() < len {
        buffer.reserve_exact(len);
    }
    let spare = buffer.spare_capacity_mut();
    let target = &mut spare[..len];
    // SAFETY: u8 has no invalid bit patterns, and the slice is within spare capacity.
    let target = unsafe { std::slice::from_raw_parts_mut(target.as_mut_ptr().cast::<u8>(), len) };
    read_exact_required(reader, target, what, offset)?;
    // SAFETY: read_exact_required returned Ok, so every byte in target was initialized.
    unsafe {
        buffer.set_len(len);
    }
    Ok(())
}

fn skip_record_padding<R: Read>(
    reader: &mut R,
    record_len: usize,
    consumed: usize,
    record_offset: usize,
) -> Result<()> {
    let padding = record_len.saturating_sub(consumed);
    skip_exact(reader, padding, "record padding", record_offset + consumed)
}

fn skip_exact<R: Read>(
    reader: &mut R,
    mut bytes: usize,
    what: &'static str,
    offset: usize,
) -> Result<()> {
    let mut buffer = [0; 8192];
    let mut skipped = 0;
    while bytes > 0 {
        let chunk = bytes.min(buffer.len());
        let target = &mut buffer[..chunk];
        read_exact_required(reader, target, what, offset + skipped)?;
        bytes -= chunk;
        skipped += chunk;
    }
    Ok(())
}

/// Parse already-normalized Archive II bytes.
pub fn decode_normalized_volume_bytes(
    bytes: &[u8],
    compression: ArchiveCompression,
) -> Result<RadarVolume> {
    let volume_header = parse_volume_header(bytes)?;
    let mut volume = RadarVolume::new(
        RadarSite::new(volume_header.icao.clone()),
        volume_header.volume_time,
    );
    volume.metadata.archive_version = Some(volume_header.archive_version);
    volume.metadata.compression = Some(compression.as_str().to_owned());

    let mut cursor = VOLUME_HEADER_LEN;
    let mut record_index = 0usize;
    // GR2Analyst-style ".msg31" exports keep the Archive II volume header but
    // carry only a few metadata records before their message 31s - well
    // before the 134th record where variable framing normally begins - and
    // pack those messages back to back with no fixed-record padding.
    // Detected once, at the first early message 31, and latched for the rest
    // of the file so the detector runs once rather than per radial.
    let mut early_variable_msg31 = false;
    while cursor + CONTROL_WORD_LEN + MESSAGE_HEADER_LEN <= bytes.len() {
        let header_offset = cursor + CONTROL_WORD_LEN;
        let header =
            parse_message_header_bytes(&bytes[header_offset..header_offset + MESSAGE_HEADER_LEN]);

        if header.size_halfwords == 0 && record_index < 134 {
            volume.metadata.skipped_message_count += 1;
            cursor = cursor.saturating_add(RECORD_BYTES);
            record_index += 1;
            continue;
        } else if header.size_halfwords == 0 {
            break;
        }

        let message_total_len = usize::from(header.size_halfwords) * 2;
        if message_total_len < MESSAGE_HEADER_LEN {
            return Err(NexradError::InvalidMessage {
                offset: header_offset,
                reason: "message size is smaller than message header".to_owned(),
            });
        }

        volume.metadata.message_count += 1;
        match header.message_type {
            1 => {
                let message_end = header_offset + message_total_len;
                if message_end > bytes.len() {
                    if volume.metadata.decoded_radial_count > 0 {
                        volume.metadata.skipped_message_count += 1;
                        break;
                    }
                    return Err(NexradError::Truncated {
                        what: "message 1 body",
                        offset: header_offset,
                        needed: message_total_len,
                        available: bytes.len().saturating_sub(header_offset),
                    });
                }
                let body = &bytes[header_offset + MESSAGE_HEADER_LEN..message_end];
                parse_message_1(body, &header, &mut volume)?;
            }
            31 => {
                let message_end = header_offset + message_total_len;
                if message_end > bytes.len() {
                    if volume.metadata.decoded_radial_count > 0 {
                        volume.metadata.skipped_message_count += 1;
                        break;
                    }
                    return Err(NexradError::Truncated {
                        what: "message 31 body",
                        offset: header_offset,
                        needed: message_total_len,
                        available: bytes.len().saturating_sub(header_offset),
                    });
                }
                let body = &bytes[header_offset + MESSAGE_HEADER_LEN..message_end];
                parse_message_31(body, &header, &mut volume)?;
            }
            5 => {
                let body_offset = header_offset + MESSAGE_HEADER_LEN;
                let fixed_record_end = cursor.saturating_add(RECORD_BYTES).min(bytes.len());
                let message_end = header_offset.saturating_add(message_total_len);
                let body_end = message_end.min(fixed_record_end);
                if body_offset < body_end {
                    parse_message_5(&bytes[body_offset..body_end], &mut volume);
                }
            }
            _ => volume.metadata.skipped_message_count += 1,
        }

        let record_len = if header.message_type != 31 {
            RECORD_BYTES
        } else if record_index >= 134 || early_variable_msg31 {
            message_total_len + CONTROL_WORD_LEN
        } else if message31_uses_variable_framing(bytes, cursor, message_total_len) {
            early_variable_msg31 = true;
            message_total_len + CONTROL_WORD_LEN
        } else {
            RECORD_BYTES
        };
        cursor = cursor.saturating_add(record_len);
        record_index += 1;
    }

    Ok(volume)
}

/// Decide the framing of a message 31 that appears before the 134th record.
///
/// Real Archive II volumes never place a message 31 that early - the metadata
/// records come first - but GR2Analyst-convention ".msg31" exports do, and
/// the DOW/COW/RaXPol Level II twins written that way pack their messages
/// back to back. Returns `true` when the bytes immediately after this message
/// hold another message 31, or when the file ends exactly there: neither is
/// something fixed 2432-byte framing can produce, because fixed framing would
/// leave padding in between.
///
/// The look-ahead is why this lives only on the whole-buffer path.
/// [`decode_volume_from_stream`] reads from a `Read` and cannot see past the
/// current record, and the block-bzip path decodes LDM-compressed volumes,
/// which is not a container GR2Analyst writes.
fn message31_uses_variable_framing(bytes: &[u8], cursor: usize, message_total_len: usize) -> bool {
    let variable_next = cursor + CONTROL_WORD_LEN + message_total_len;
    if variable_next == bytes.len() {
        return true;
    }
    let header_offset = variable_next + CONTROL_WORD_LEN;
    let Some(header_bytes) = bytes.get(header_offset..header_offset + MESSAGE_HEADER_LEN) else {
        return false;
    };
    let header = parse_message_header_bytes(header_bytes);
    header.message_type == 31
        && usize::from(header.size_halfwords) * 2 >= MESSAGE_HEADER_LEN + MSG_31_HEADER_LEN
}

struct StreamDecodeResult {
    volume: RadarVolume,
    stopped_at_preview: bool,
}

fn decode_volume_from_stream_until<R: Read>(
    reader: &mut R,
    compression: ArchiveCompression,
    preview_min_radials: Option<usize>,
) -> Result<StreamDecodeResult> {
    decode_volume_from_stream(reader, compression, preview_min_radials, true, |_| {})
}

fn decode_volume_from_stream<R: Read, F>(
    reader: &mut R,
    compression: ArchiveCompression,
    preview_min_radials: Option<usize>,
    stop_at_preview: bool,
    mut on_preview: F,
) -> Result<StreamDecodeResult>
where
    F: FnMut(RadarVolume),
{
    let mut volume_header_bytes = [0; VOLUME_HEADER_LEN];
    read_exact_required(reader, &mut volume_header_bytes, "volume header", 0)?;
    let volume_header = parse_volume_header(&volume_header_bytes)?;
    let mut volume = RadarVolume::new(
        RadarSite::new(volume_header.icao.clone()),
        volume_header.volume_time,
    );
    volume.metadata.archive_version = Some(volume_header.archive_version);
    volume.metadata.compression = Some(compression.as_str().to_owned());

    let mut cursor = VOLUME_HEADER_LEN;
    let mut record_index = 0usize;
    let mut prefix = [0; CONTROL_WORD_LEN + MESSAGE_HEADER_LEN];
    let mut body_buffer = Vec::with_capacity(RECORD_BYTES);
    let mut preview_emitted = false;
    while read_record_prefix(reader, &mut prefix, cursor)? {
        let header_offset = cursor + CONTROL_WORD_LEN;
        let header = parse_message_header_bytes(&prefix[CONTROL_WORD_LEN..]);

        if header.size_halfwords == 0 && record_index < 134 {
            volume.metadata.skipped_message_count += 1;
            skip_exact(
                reader,
                RECORD_BYTES - prefix.len(),
                "empty fixed record",
                cursor + prefix.len(),
            )?;
            cursor = cursor.saturating_add(RECORD_BYTES);
            record_index += 1;
            continue;
        } else if header.size_halfwords == 0 {
            break;
        }

        let message_total_len = usize::from(header.size_halfwords) * 2;
        if message_total_len < MESSAGE_HEADER_LEN {
            return Err(NexradError::InvalidMessage {
                offset: header_offset,
                reason: "message size is smaller than message header".to_owned(),
            });
        }

        let record_len = if record_index < 134 || header.message_type != 31 {
            RECORD_BYTES
        } else {
            message_total_len + CONTROL_WORD_LEN
        };
        let body_len = message_total_len - MESSAGE_HEADER_LEN;
        volume.metadata.message_count += 1;

        match header.message_type {
            1 => {
                if let Err(err) = read_exact_into_buffer(
                    reader,
                    &mut body_buffer,
                    body_len,
                    "message 1 body",
                    header_offset,
                ) {
                    if volume.metadata.decoded_radial_count > 0 {
                        volume.metadata.skipped_message_count += 1;
                        break;
                    }
                    return Err(err);
                }
                parse_message_1(&body_buffer, &header, &mut volume)?;
                skip_record_padding(reader, record_len, prefix.len() + body_len, cursor)?;
            }
            31 => {
                if let Err(err) = read_exact_into_buffer(
                    reader,
                    &mut body_buffer,
                    body_len,
                    "message 31 body",
                    header_offset,
                ) {
                    if volume.metadata.decoded_radial_count > 0 {
                        volume.metadata.skipped_message_count += 1;
                        break;
                    }
                    return Err(err);
                }
                parse_message_31(&body_buffer, &header, &mut volume)?;
                skip_record_padding(reader, record_len, prefix.len() + body_len, cursor)?;
                if let Some(min_radials) = preview_min_radials
                    && !preview_emitted
                    && has_complete_displayable_cut(&volume, min_radials)
                {
                    preview_emitted = true;
                    if stop_at_preview {
                        return Ok(StreamDecodeResult {
                            volume,
                            stopped_at_preview: true,
                        });
                    }
                    on_preview(volume.clone());
                }
            }
            5 => {
                let fixed_body_len = RECORD_BYTES.saturating_sub(prefix.len());
                let body_read_len = body_len.min(fixed_body_len);
                read_exact_into_buffer(
                    reader,
                    &mut body_buffer,
                    body_read_len,
                    "message 5 body",
                    header_offset,
                )?;
                parse_message_5(&body_buffer, &mut volume);
                skip_record_padding(reader, record_len, prefix.len() + body_read_len, cursor)?;
            }
            _ => {
                volume.metadata.skipped_message_count += 1;
                skip_record_padding(reader, record_len, prefix.len(), cursor)?;
            }
        }

        cursor = cursor.saturating_add(record_len);
        record_index += 1;
    }

    Ok(StreamDecodeResult {
        volume,
        stopped_at_preview: false,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VolumeHeader {
    archive_version: String,
    volume_time: DateTime<Utc>,
    icao: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageHeader {
    pub size_halfwords: u16,
    pub channels: u8,
    pub message_type: u8,
    pub sequence_id: u16,
    pub date: u16,
    pub milliseconds: u32,
    pub segments: u16,
    pub segment_number: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message31Header {
    pub collect_ms: u32,
    pub collect_date: u16,
    pub azimuth_number: u16,
    pub azimuth_angle: f32,
    pub radial_length: u16,
    pub azimuth_resolution: u8,
    pub radial_status: RadialStatus,
    pub elevation_number: u8,
    pub cut_sector: u8,
    pub elevation_angle: f32,
    pub block_pointers: [usize; 10],
}

#[derive(Clone, Debug, PartialEq)]
struct MomentBlock<'a> {
    moment: MomentType,
    gate_range: GateRange,
    scale: f32,
    offset: f32,
    snr_threshold_db: f32,
    recombination: MomentRecombination,
    row: MomentPayload<'a>,
}

#[derive(Clone, Debug, PartialEq)]
enum MomentPayload<'a> {
    U8(&'a [u8]),
    U16(&'a [u8]),
}

struct BzipBlockCursor<'a> {
    volume_header: &'a [u8],
    blocks: &'a [Vec<u8>],
    chunk_index: usize,
    chunk_offset: usize,
    absolute_offset: usize,
}

impl<'a> BzipBlockCursor<'a> {
    fn new(volume_header: &'a [u8], blocks: &'a [Vec<u8>]) -> Self {
        Self {
            volume_header,
            blocks,
            chunk_index: 0,
            chunk_offset: 0,
            absolute_offset: 0,
        }
    }

    fn current_chunk(&self) -> Option<&[u8]> {
        match self.chunk_index {
            0 => Some(self.volume_header),
            index => self.blocks.get(index - 1).map(Vec::as_slice),
        }
    }

    fn current_chunk_len(&self) -> Option<usize> {
        match self.chunk_index {
            0 => Some(self.volume_header.len()),
            index => self.blocks.get(index - 1).map(Vec::len),
        }
    }

    fn chunk_slice(&self, index: usize, start: usize, end: usize) -> &'a [u8] {
        match index {
            0 => &self.volume_header[start..end],
            index => &self.blocks[index - 1][start..end],
        }
    }

    fn skip_empty_chunks(&mut self) {
        while self
            .current_chunk()
            .is_some_and(|chunk| self.chunk_offset >= chunk.len())
        {
            self.chunk_index += 1;
            self.chunk_offset = 0;
        }
    }

    fn read_exact_into(
        &mut self,
        mut output: &mut [u8],
        what: &'static str,
        offset: usize,
    ) -> Result<()> {
        let mut written = 0;
        while !output.is_empty() {
            self.skip_empty_chunks();
            let Some(chunk) = self.current_chunk() else {
                return Err(NexradError::Truncated {
                    what,
                    offset,
                    needed: written + output.len(),
                    available: written,
                });
            };
            let available = &chunk[self.chunk_offset..];
            let count = available.len().min(output.len());
            output[..count].copy_from_slice(&available[..count]);
            self.chunk_offset += count;
            self.absolute_offset += count;
            let (_, rest) = output.split_at_mut(count);
            output = rest;
            written += count;
        }
        Ok(())
    }

    fn read_optional_prefix(&mut self, output: &mut [u8], offset: usize) -> Result<bool> {
        self.skip_empty_chunks();
        if self.current_chunk().is_none() {
            return Ok(false);
        }
        self.read_exact_into(output, "record prefix", offset)?;
        Ok(true)
    }

    fn read_slice_or_copy<'b>(
        &'b mut self,
        scratch: &'b mut Vec<u8>,
        len: usize,
        what: &'static str,
        offset: usize,
    ) -> Result<&'b [u8]> {
        scratch.clear();
        self.skip_empty_chunks();
        if len == 0 {
            return Ok(&[]);
        }
        let Some(chunk_len) = self.current_chunk_len() else {
            return Err(NexradError::Truncated {
                what,
                offset,
                needed: len,
                available: 0,
            });
        };
        if self.chunk_offset + len <= chunk_len {
            let index = self.chunk_index;
            let start = self.chunk_offset;
            let end = start + len;
            self.chunk_offset = end;
            self.absolute_offset += len;
            return Ok(self.chunk_slice(index, start, end));
        }

        if scratch.capacity() < len {
            scratch.reserve_exact(len - scratch.capacity());
        }
        let mut remaining = len;
        while remaining > 0 {
            self.skip_empty_chunks();
            let Some(chunk) = self.current_chunk() else {
                return Err(NexradError::Truncated {
                    what,
                    offset,
                    needed: len,
                    available: scratch.len(),
                });
            };
            let available = &chunk[self.chunk_offset..];
            let count = available.len().min(remaining);
            scratch.extend_from_slice(&available[..count]);
            self.chunk_offset += count;
            self.absolute_offset += count;
            remaining -= count;
        }
        Ok(scratch.as_slice())
    }

    fn skip_exact(&mut self, len: usize, what: &'static str, offset: usize) -> Result<()> {
        let mut skipped = 0;
        while skipped < len {
            self.skip_empty_chunks();
            let Some(chunk) = self.current_chunk() else {
                return Err(NexradError::Truncated {
                    what,
                    offset,
                    needed: len,
                    available: skipped,
                });
            };
            let count = (len - skipped).min(chunk.len() - self.chunk_offset);
            self.chunk_offset += count;
            self.absolute_offset += count;
            skipped += count;
        }
        Ok(())
    }
}

fn decode_bzip_block_sequence(volume_header: &[u8], blocks: &[Vec<u8>]) -> Result<RadarVolume> {
    let mut cursor_reader = BzipBlockCursor::new(volume_header, blocks);
    let mut volume_header_buffer = Vec::new();
    let volume_header_bytes = cursor_reader.read_slice_or_copy(
        &mut volume_header_buffer,
        VOLUME_HEADER_LEN,
        "volume header",
        0,
    )?;
    let volume_header = parse_volume_header(volume_header_bytes)?;
    let mut volume = RadarVolume::new(
        RadarSite::new(volume_header.icao.clone()),
        volume_header.volume_time,
    );
    volume.metadata.archive_version = Some(volume_header.archive_version);
    volume.metadata.compression = Some(ArchiveCompression::Bzip2Blocks.as_str().to_owned());

    let mut cursor = VOLUME_HEADER_LEN;
    let mut record_index = 0usize;
    let mut prefix = [0; CONTROL_WORD_LEN + MESSAGE_HEADER_LEN];
    let mut body_buffer = Vec::with_capacity(RECORD_BYTES);
    while cursor_reader.read_optional_prefix(&mut prefix, cursor)? {
        let header_offset = cursor + CONTROL_WORD_LEN;
        let header = parse_message_header_bytes(&prefix[CONTROL_WORD_LEN..]);

        if header.size_halfwords == 0 && record_index < 134 {
            volume.metadata.skipped_message_count += 1;
            cursor_reader.skip_exact(
                RECORD_BYTES - prefix.len(),
                "empty fixed record",
                cursor + prefix.len(),
            )?;
            cursor = cursor.saturating_add(RECORD_BYTES);
            record_index += 1;
            continue;
        } else if header.size_halfwords == 0 {
            break;
        }

        let message_total_len = usize::from(header.size_halfwords) * 2;
        if message_total_len < MESSAGE_HEADER_LEN {
            return Err(NexradError::InvalidMessage {
                offset: header_offset,
                reason: "message size is smaller than message header".to_owned(),
            });
        }

        let record_len = if record_index < 134 || header.message_type != 31 {
            RECORD_BYTES
        } else {
            message_total_len + CONTROL_WORD_LEN
        };
        let body_len = message_total_len - MESSAGE_HEADER_LEN;
        volume.metadata.message_count += 1;

        match header.message_type {
            1 => {
                let body = match cursor_reader.read_slice_or_copy(
                    &mut body_buffer,
                    body_len,
                    "message 1 body",
                    header_offset,
                ) {
                    Ok(body) => body,
                    Err(err) => {
                        if volume.metadata.decoded_radial_count > 0 {
                            volume.metadata.skipped_message_count += 1;
                            break;
                        }
                        return Err(err);
                    }
                };
                parse_message_1(body, &header, &mut volume)?;
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len() + body_len),
                    "record padding",
                    cursor + prefix.len() + body_len,
                )?;
            }
            31 => {
                let body = match cursor_reader.read_slice_or_copy(
                    &mut body_buffer,
                    body_len,
                    "message 31 body",
                    header_offset,
                ) {
                    Ok(body) => body,
                    Err(err) => {
                        if volume.metadata.decoded_radial_count > 0 {
                            volume.metadata.skipped_message_count += 1;
                            break;
                        }
                        return Err(err);
                    }
                };
                parse_message_31(body, &header, &mut volume)?;
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len() + body_len),
                    "record padding",
                    cursor + prefix.len() + body_len,
                )?;
            }
            5 => {
                let fixed_body_len = RECORD_BYTES.saturating_sub(prefix.len());
                let body_read_len = body_len.min(fixed_body_len);
                let body = cursor_reader.read_slice_or_copy(
                    &mut body_buffer,
                    body_read_len,
                    "message 5 body",
                    header_offset,
                )?;
                parse_message_5(body, &mut volume);
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len() + body_read_len),
                    "record padding",
                    cursor + prefix.len() + body_read_len,
                )?;
            }
            _ => {
                volume.metadata.skipped_message_count += 1;
                cursor_reader.skip_exact(
                    record_len.saturating_sub(prefix.len()),
                    "record padding",
                    cursor + prefix.len(),
                )?;
            }
        }

        cursor = cursor.saturating_add(record_len);
        record_index += 1;
    }

    Ok(volume)
}

fn has_complete_displayable_cut(volume: &RadarVolume, min_displayable_radials: usize) -> bool {
    volume.cuts.iter().enumerate().any(|(index, cut)| {
        if cut.radials.len() < min_displayable_radials {
            return false;
        }
        let has_displayable_moment = cut
            .moments
            .values()
            .any(|grid| grid.radial_count() >= min_displayable_radials);
        if !has_displayable_moment {
            return false;
        }
        let ended = cut.radials.last().is_some_and(|radial| {
            matches!(
                radial.radial_status,
                Some(RadialStatus::EndElevation | RadialStatus::EndVolume)
            )
        });
        ended || index + 1 < volume.cuts.len()
    })
}

fn try_decode_bzip_blocks(raw: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(decoded_blocks) = try_decompress_bzip_blocks(raw)? else {
        return Ok(None);
    };

    let decoded_len = decoded_blocks.iter().map(Vec::len).sum::<usize>();
    let mut output = Vec::with_capacity(VOLUME_HEADER_LEN + decoded_len);
    output.extend_from_slice(&raw[..VOLUME_HEADER_LEN]);
    for block in decoded_blocks {
        output.extend(block);
    }

    Ok(Some(output))
}

fn try_decompress_bzip_blocks(raw: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
    let Some(blocks) = collect_bzip_block_slices(raw)? else {
        return Ok(None);
    };

    let decoded_blocks = blocks
        .par_iter()
        .map(|compressed| decompress_bzip_block(compressed))
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(decoded_blocks))
}

fn collect_bzip_block_slices(raw: &[u8]) -> Result<Option<Vec<&[u8]>>> {
    if raw.len() < VOLUME_HEADER_LEN + 4 {
        return Ok(None);
    }

    let mut cursor = VOLUME_HEADER_LEN;
    let mut blocks = Vec::new();

    while cursor + 4 <= raw.len() {
        let signed_block_size = i32_at(raw, cursor)?;
        if signed_block_size == -1 && cursor + 4 == raw.len() {
            break;
        }
        if signed_block_size == 0 {
            return Ok(None);
        }

        cursor += 4;
        let is_last_block = signed_block_size < 0;
        let block_size = usize::try_from(signed_block_size.unsigned_abs())
            .map_err(|_| NexradError::Compression("bzip2 block size overflow".to_owned()))?;
        if cursor + block_size > raw.len() {
            return Ok(None);
        }

        let compressed = &raw[cursor..cursor + block_size];
        if !compressed.starts_with(b"BZh") {
            return Ok(None);
        }

        blocks.push(compressed);
        cursor += block_size;
        if is_last_block {
            break;
        }
    }

    if blocks.is_empty() {
        return Ok(None);
    }

    Ok(Some(blocks))
}

fn decompress_bzip_block(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut decoded =
        Vec::with_capacity(BZIP_BLOCK_DECODE_CAPACITY_HINT.max(compressed.len().saturating_mul(2)));
    BzDecoder::new(Cursor::new(compressed))
        .read_to_end(&mut decoded)
        .map_err(|err| NexradError::Compression(err.to_string()))?;
    Ok(decoded)
}

fn parse_volume_header(bytes: &[u8]) -> Result<VolumeHeader> {
    require_len(bytes, 0, VOLUME_HEADER_LEN, "volume header")?;
    let tape = ascii_trim(&bytes[0..9]);
    let extension = ascii_trim(&bytes[9..12]);
    let date = u32_at(bytes, 12)?;
    let milliseconds = u32_at(bytes, 16)?;
    let icao = ascii_trim(&bytes[20..24]);

    Ok(VolumeHeader {
        archive_version: format!("{tape}{extension}"),
        volume_time: nexrad_date_ms_to_datetime(date, milliseconds),
        icao,
    })
}

pub fn parse_message_header(bytes: &[u8], offset: usize) -> Result<MessageHeader> {
    require_len(bytes, offset, MESSAGE_HEADER_LEN, "message header")?;
    Ok(parse_message_header_bytes(
        &bytes[offset..offset + MESSAGE_HEADER_LEN],
    ))
}

fn parse_message_header_bytes(bytes: &[u8]) -> MessageHeader {
    debug_assert!(bytes.len() >= MESSAGE_HEADER_LEN);
    MessageHeader {
        size_halfwords: be_u16(bytes, 0),
        channels: bytes[2],
        message_type: bytes[3],
        sequence_id: be_u16(bytes, 4),
        date: be_u16(bytes, 6),
        milliseconds: be_u32(bytes, 8),
        segments: be_u16(bytes, 12),
        segment_number: be_u16(bytes, 14),
    }
}

fn parse_message_5(body: &[u8], volume: &mut RadarVolume) {
    if body.len() >= 6 {
        let pattern = u16::from_be_bytes([body[4], body[5]]);
        if pattern != 0 {
            volume.vcp = Some(VcpInfo { pattern });
        }
    }
}

/// Byte offsets into a Message Type 1 body, from ICD 2620002 Table
/// "Digital Radar Data (Message Type 1)". The ICD numbers halfwords from 1;
/// each constant below is `(halfword - 1) * 2`, and the halfword is named so
/// the mapping can be checked against the document without arithmetic.
mod msg1 {
    /// Halfword 1-2: radial collection time, ms past midnight.
    pub const COLLECT_MS: usize = 0;
    /// Halfword 3: modified Julian date, days since 1 January 1970.
    pub const COLLECT_DATE: usize = 4;
    /// Halfword 5: azimuth angle, coded (see `legacy_binary_angle_deg`).
    pub const AZIMUTH_ANGLE: usize = 8;
    /// Halfword 7: radial status.
    pub const RADIAL_STATUS: usize = 12;
    /// Halfword 8: elevation angle, coded.
    pub const ELEVATION_ANGLE: usize = 14;
    /// Halfword 9: elevation number within the volume scan.
    pub const ELEVATION_NUMBER: usize = 16;
    /// Halfword 10: surveillance range - range to the first reflectivity gate, m.
    pub const SURVEILLANCE_RANGE: usize = 18;
    /// Halfword 11: Doppler range - range to the first velocity gate, m.
    pub const DOPPLER_RANGE: usize = 20;
    /// Halfword 12: surveillance range sample interval (reflectivity gate spacing), m.
    pub const SURVEILLANCE_RANGE_STEP: usize = 22;
    /// Halfword 13: Doppler range sample interval (velocity gate spacing), m.
    pub const DOPPLER_RANGE_STEP: usize = 24;
    /// Halfword 14: number of surveillance (reflectivity) bins.
    pub const SURVEILLANCE_BIN_COUNT: usize = 26;
    /// Halfword 15: number of Doppler (velocity/spectrum width) bins.
    pub const DOPPLER_BIN_COUNT: usize = 28;
    /// Halfword 19: reflectivity data pointer, bytes from the start of this body.
    pub const REFLECTIVITY_POINTER: usize = 36;
    /// Halfword 20: velocity data pointer.
    pub const VELOCITY_POINTER: usize = 38;
    /// Halfword 21: spectrum width data pointer.
    pub const SPECTRUM_WIDTH_POINTER: usize = 40;
    /// Halfword 22: Doppler velocity resolution code (2 = 0.5 m/s, 4 = 1.0 m/s).
    pub const VELOCITY_RESOLUTION: usize = 42;
    /// Halfword 23: volume coverage pattern number.
    pub const VOLUME_COVERAGE_PATTERN: usize = 44;
    /// Halfword 31: Nyquist velocity, cm/s.
    ///
    /// Halfwords 24-27 are reserved for RDA internal use and halfwords 28-30
    /// repeat the three data pointers for playback, so the Nyquist sits nine
    /// halfwords past the VCP rather than one. Verified on KTLX 1999-05-03
    /// 23:36:31Z: at this offset the field is exactly zero on the two
    /// surveillance-only cuts of the VCP 11 split scan and 2610/2819/3041
    /// cm/s on every cut that carries Doppler bins, and halfwords 28-30 hold
    /// byte-for-byte copies of halfwords 19-21.
    pub const NYQUIST_VELOCITY: usize = 60;
}

/// Legacy reflectivity coding, ICD 2620002: `dBZ = (code - 66) / 2`, giving
/// 0.5 dBZ resolution from -32 dBZ at code 2. Codes 0 and 1 are
/// below-threshold and range-folded.
const LEGACY_REFLECTIVITY_SCALE: f32 = 2.0;
const LEGACY_REFLECTIVITY_OFFSET: f32 = 66.0;
/// Legacy Doppler coding: `m/s = (code - 129) / scale`, where the scale is
/// 2 for 0.5 m/s velocity resolution and 1 for 1.0 m/s. Spectrum width is
/// always carried at 0.5 m/s resolution regardless of the velocity code.
const LEGACY_DOPPLER_OFFSET: f32 = 129.0;
const LEGACY_SPECTRUM_WIDTH_SCALE: f32 = 2.0;

/// Decode one legacy Message Type 1 radial (ICD 2620002, "Digital Radar
/// Data"), the format every NEXRAD volume before the 2008 Build 10 generic
/// -format cutover is written in.
///
/// Message 1 carries only the three legacy moments at fixed resolutions -
/// reflectivity on 1 km gates, velocity and spectrum width on 250 m gates -
/// with the moment layout described by pointers into the message body rather
/// than by the self-describing blocks of Message 31. Split cuts appear as
/// separate elevation numbers, one surveillance-only and one Doppler-only,
/// and are kept as separate cuts here for the same reason Message 31 split
/// cuts are: they are separate sweeps of the antenna.
fn parse_message_1(
    body: &[u8],
    _message_header: &MessageHeader,
    volume: &mut RadarVolume,
) -> Result<()> {
    require_len(body, 0, MSG_1_HEADER_LEN, "message 1 header")?;

    let collect_ms = be_u32(body, msg1::COLLECT_MS);
    let collect_date = be_u16(body, msg1::COLLECT_DATE);
    // Message 1 volumes have no Message 31 volume-constant block and, on
    // pre-2008 tapes, an all-zero ICAO field in the volume header, so the
    // first radial's own timestamp is the best volume time available.
    if volume.metadata.decoded_radial_count == 0 && collect_date > 0 {
        volume.volume_time = nexrad_date_ms_to_datetime(u32::from(collect_date), collect_ms);
    }

    let azimuth_angle = legacy_binary_angle_deg(be_u16(body, msg1::AZIMUTH_ANGLE));
    let radial_status = RadialStatus::from(be_u16(body, msg1::RADIAL_STATUS) as u8);
    let elevation_angle = legacy_binary_angle_deg(be_u16(body, msg1::ELEVATION_ANGLE));
    // Elevation numbers are 1-based; a zero here would collide with "no cut
    // number recorded" downstream.
    let elevation_number = (be_u16(body, msg1::ELEVATION_NUMBER) as u8).max(1);

    let reflectivity_range = GateRange {
        first_gate_m: i32::from(be_i16(body, msg1::SURVEILLANCE_RANGE)),
        gate_spacing_m: i32::from(be_u16(body, msg1::SURVEILLANCE_RANGE_STEP).max(1)),
        gate_count: usize::from(be_u16(body, msg1::SURVEILLANCE_BIN_COUNT)),
    };
    let doppler_range = GateRange {
        first_gate_m: i32::from(be_i16(body, msg1::DOPPLER_RANGE)),
        gate_spacing_m: i32::from(be_u16(body, msg1::DOPPLER_RANGE_STEP).max(1)),
        gate_count: usize::from(be_u16(body, msg1::DOPPLER_BIN_COUNT)),
    };
    let reflectivity_pointer = usize::from(be_u16(body, msg1::REFLECTIVITY_POINTER));
    let velocity_pointer = usize::from(be_u16(body, msg1::VELOCITY_POINTER));
    let spectrum_width_pointer = usize::from(be_u16(body, msg1::SPECTRUM_WIDTH_POINTER));
    let velocity_resolution = be_u16(body, msg1::VELOCITY_RESOLUTION);

    let vcp = be_u16(body, msg1::VOLUME_COVERAGE_PATTERN);
    if vcp != 0 {
        volume.vcp = Some(VcpInfo { pattern: vcp });
    }
    let nyquist_velocity_mps = match be_i16(body, msg1::NYQUIST_VELOCITY) {
        raw if raw > 0 => Some(f32::from(raw) / 100.0),
        _ => None,
    };

    let reflectivity_row =
        legacy_message_1_row(body, reflectivity_pointer, reflectivity_range.gate_count);
    let velocity_row = legacy_message_1_row(body, velocity_pointer, doppler_range.gate_count);
    let spectrum_width_row =
        legacy_message_1_row(body, spectrum_width_pointer, doppler_range.gate_count);
    if reflectivity_row.is_none() && velocity_row.is_none() && spectrum_width_row.is_none() {
        volume.metadata.skipped_message_count += 1;
        return Ok(());
    }

    let gate_range = if reflectivity_row.is_some() {
        reflectivity_range.clone()
    } else {
        doppler_range.clone()
    };
    let radial = Radial {
        azimuth_deg: azimuth_angle,
        elevation_deg: elevation_angle,
        time_offset_ms: collect_ms as i32,
        gate_range,
        nyquist_velocity_mps,
        radial_status: Some(radial_status),
    };

    let cut = select_cut_for_radial(volume, radial_status, elevation_angle, elevation_number);
    if cut.radials.is_empty() {
        cut.radials.reserve(ONE_DEGREE_RADIALS_PER_CUT);
    }
    let radial_index = cut.radials.len();
    cut.radials.push(radial);

    if let Some(row) = reflectivity_row {
        let grid = legacy_u8_grid(
            cut,
            MomentType::Reflectivity,
            reflectivity_range,
            LEGACY_REFLECTIVITY_SCALE,
            LEGACY_REFLECTIVITY_OFFSET,
        );
        grid.push_u8_row_slice(radial_index, row)?;
    }
    if let Some(row) = velocity_row {
        let grid = legacy_u8_grid(
            cut,
            MomentType::Velocity,
            doppler_range.clone(),
            legacy_message_1_velocity_scale(velocity_resolution),
            LEGACY_DOPPLER_OFFSET,
        );
        grid.push_u8_row_slice(radial_index, row)?;
    }
    if let Some(row) = spectrum_width_row {
        let grid = legacy_u8_grid(
            cut,
            MomentType::SpectrumWidth,
            doppler_range,
            LEGACY_SPECTRUM_WIDTH_SCALE,
            LEGACY_DOPPLER_OFFSET,
        );
        grid.push_u8_row_slice(radial_index, row)?;
    }

    volume.metadata.decoded_radial_count += 1;
    Ok(())
}

/// Pick the cut a radial belongs to, shared by Message 1 and Message 31.
///
/// A start-of-elevation marker opens a new cut, which is what keeps the two
/// halves of a split cut apart even though they sit at the same angle.
/// Otherwise the radial extends the cut in progress when it matches, and only
/// a radial that matches neither goes looking through earlier cuts.
fn select_cut_for_radial(
    volume: &mut RadarVolume,
    radial_status: RadialStatus,
    elevation_angle: f32,
    elevation_number: u8,
) -> &mut radar_core::ElevationCut {
    let starts_elevation = matches!(
        radial_status,
        RadialStatus::StartElevation
            | RadialStatus::StartVolume
            | RadialStatus::StartElevationLastCut
    );
    let last_cut_has_radials = volume
        .cuts
        .last()
        .is_some_and(|cut| !cut.radials.is_empty());
    let last_cut_matches = volume.cuts.last().is_some_and(|cut| {
        cut.elevation_number == Some(elevation_number)
            || (cut.elevation_deg - elevation_angle).abs() <= 0.05
    });
    if starts_elevation && last_cut_has_radials {
        volume.push_cut(elevation_angle, Some(elevation_number))
    } else if last_cut_matches {
        volume
            .cuts
            .last_mut()
            .expect("last cut existence was checked before borrowing")
    } else {
        volume.find_or_insert_cut(elevation_angle, Some(elevation_number))
    }
}

/// Fetch or create the 8-bit grid a legacy moment accumulates into.
///
/// Message 1 predates the generic data moment header, so it carries neither
/// an SNR threshold nor control flags (NEXRAD ICD 2620002W Table XVII-B
/// describes the Message 31 block; the Message 1 digital radial data header
/// of Table XVII has no counterpart). The grid's `snr_threshold_db` and
/// `recombination` therefore stay `None` here, and the display shows nothing
/// rather than inventing a threshold the file never stated.
fn legacy_u8_grid(
    cut: &mut radar_core::ElevationCut,
    moment: MomentType,
    gate_range: GateRange,
    scale: f32,
    offset: f32,
) -> &mut MomentGrid {
    match cut.moments.entry(moment) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let mut grid = MomentGrid::new_u8(
                entry.key().clone(),
                gate_range,
                scale,
                offset,
                Some(0),
                Some(1),
            );
            grid.reserve_rows(ONE_DEGREE_RADIALS_PER_CUT);
            entry.insert(grid)
        }
    }
}

/// Borrow one moment's gates out of a Message 1 body.
///
/// A zero pointer means the moment is absent from this radial, which is the
/// normal state of a split cut's surveillance half. A pointer that runs off
/// the end of the message is treated the same way rather than as a fatal
/// error, so one damaged radial costs one radial.
fn legacy_message_1_row(body: &[u8], pointer: usize, gate_count: usize) -> Option<&[u8]> {
    if pointer == 0 || gate_count == 0 {
        return None;
    }
    body.get(pointer..pointer.checked_add(gate_count)?)
}

/// Velocity scale from the Doppler velocity resolution code (ICD 2620002
/// halfword 22): code 2 is 0.5 m/s per count, code 4 is 1.0 m/s per count.
/// An out-of-range code falls back to the far more common 0.5 m/s.
fn legacy_message_1_velocity_scale(velocity_resolution: u16) -> f32 {
    match velocity_resolution {
        4 => 1.0,
        _ => 2.0,
    }
}

/// Decode a legacy binary angle: a full turn spread over the 16-bit range.
fn legacy_binary_angle_deg(raw: u16) -> f32 {
    f32::from(raw) * 360.0 / 65_536.0
}

fn parse_message_31(
    body: &[u8],
    _message_header: &MessageHeader,
    volume: &mut RadarVolume,
) -> Result<()> {
    let header = parse_message_31_header(body, 0)?;
    let expected_radials = expected_radials_for_azimuth_resolution(header.azimuth_resolution);

    let mut nyquist_velocity_mps = None;
    let mut moments: [Option<MomentBlock<'_>>; MAX_MESSAGE_31_MOMENTS] =
        std::array::from_fn(|_| None);
    let mut moment_count = 0;
    let needs_volume_constants = volume_needs_constant_block(volume);

    for pointer in &header.block_pointers {
        if *pointer == 0 {
            continue;
        }
        let pointer = *pointer;
        if pointer > body.len().saturating_sub(4) {
            continue;
        }

        match body[pointer] {
            b'R' if &body[pointer + 1..pointer + 4] == b"VOL" => {
                if needs_volume_constants {
                    parse_volume_constant_block(body, pointer, volume)?;
                }
            }
            b'R' if &body[pointer + 1..pointer + 4] == b"RAD" => {
                nyquist_velocity_mps = parse_radial_constant_block(body, pointer)?;
            }
            b'D' if moment_count < moments.len() => {
                moments[moment_count] = Some(parse_generic_moment_block(body, pointer)?);
                moment_count += 1;
            }
            _ => {}
        }
    }

    let gate_range = moments[..moment_count]
        .iter()
        .flatten()
        .next()
        .map(|moment| moment.gate_range.clone())
        .unwrap_or(GateRange {
            first_gate_m: 0,
            gate_spacing_m: 0,
            gate_count: 0,
        });
    let radial = Radial {
        azimuth_deg: header.azimuth_angle,
        elevation_deg: header.elevation_angle,
        time_offset_ms: header.collect_ms as i32,
        gate_range,
        nyquist_velocity_mps,
        radial_status: Some(header.radial_status),
    };

    let cut = select_cut_for_radial(
        volume,
        header.radial_status,
        header.elevation_angle,
        header.elevation_number,
    );
    if cut.radials.is_empty() {
        cut.radials.reserve(expected_radials);
    }
    let radial_index = cut.radials.len();
    cut.radials.push(radial);

    for moment in moments.into_iter().take(moment_count).flatten() {
        let MomentBlock {
            moment,
            gate_range,
            scale,
            offset,
            snr_threshold_db,
            recombination,
            row,
        } = moment;
        let grid = match cut.moments.entry(moment) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut grid = match &row {
                    MomentPayload::U8(_) => MomentGrid::new_u8(
                        entry.key().clone(),
                        gate_range.clone(),
                        scale,
                        offset,
                        Some(0),
                        Some(1),
                    ),
                    MomentPayload::U16(_) => MomentGrid::new_u16(
                        entry.key().clone(),
                        gate_range.clone(),
                        scale,
                        offset,
                        Some(0),
                        Some(1),
                    ),
                };
                // Taken from this sweep's first radial, the same way scale and
                // offset already are: the RDA sets both per moment for the
                // whole cut, so the first block that opens the grid describes
                // every radial that follows it.
                grid.snr_threshold_db = Some(snr_threshold_db);
                grid.recombination = Some(recombination);
                grid.reserve_rows(expected_radials);
                entry.insert(grid)
            }
        };
        match row {
            MomentPayload::U8(row) => grid.push_u8_row_slice(radial_index, row)?,
            MomentPayload::U16(row) => grid.push_u16_be_row_bytes(radial_index, row)?,
        }
    }

    volume.metadata.decoded_radial_count += 1;
    Ok(())
}

fn expected_radials_for_azimuth_resolution(azimuth_resolution: u8) -> usize {
    match azimuth_resolution {
        1 => HALF_DEGREE_RADIALS_PER_CUT,
        2 => ONE_DEGREE_RADIALS_PER_CUT,
        _ => FALLBACK_RADIALS_PER_CUT,
    }
}

fn volume_needs_constant_block(volume: &RadarVolume) -> bool {
    volume.site.latitude_deg.is_none()
        || volume.site.longitude_deg.is_none()
        || volume.site.elevation_m.is_none()
        || volume.vcp.is_none()
}

pub fn parse_message_31_header(bytes: &[u8], offset: usize) -> Result<Message31Header> {
    require_len(bytes, offset, MSG_31_HEADER_LEN, "message 31 header")?;
    let bytes = &bytes[offset..offset + MSG_31_HEADER_LEN];
    if bytes[..4]
        .iter()
        .all(|byte| *byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(NexradError::InvalidMessage {
            offset,
            reason: "empty message 31 id".to_owned(),
        });
    }

    let mut block_pointers = [0; 10];
    for (index, pointer) in block_pointers.iter_mut().enumerate() {
        *pointer = be_u32(bytes, 32 + index * 4) as usize;
    }

    Ok(Message31Header {
        collect_ms: be_u32(bytes, 4),
        collect_date: be_u16(bytes, 8),
        azimuth_number: be_u16(bytes, 10),
        azimuth_angle: be_f32(bytes, 12),
        radial_length: be_u16(bytes, 18),
        azimuth_resolution: bytes[20],
        radial_status: RadialStatus::from(bytes[21]),
        elevation_number: bytes[22],
        cut_sector: bytes[23],
        elevation_angle: be_f32(bytes, 24),
        block_pointers,
    })
}

fn parse_volume_constant_block(
    bytes: &[u8],
    offset: usize,
    volume: &mut RadarVolume,
) -> Result<()> {
    require_len(
        bytes,
        offset,
        VOLUME_CONSTANT_BLOCK_LEN,
        "volume constant block",
    )?;
    let bytes = &bytes[offset..offset + VOLUME_CONSTANT_BLOCK_LEN];
    volume.site.latitude_deg = Some(be_f32(bytes, 8));
    volume.site.longitude_deg = Some(be_f32(bytes, 12));

    let tower_height_m = be_i16(bytes, 16) as f32;
    let feedhorn_height_m = be_u16(bytes, 18) as f32;
    volume.site.elevation_m = Some(tower_height_m + feedhorn_height_m);

    let vcp = be_u16(bytes, 40);
    if vcp != 0 {
        volume.vcp = Some(VcpInfo { pattern: vcp });
    }
    Ok(())
}

fn parse_radial_constant_block(bytes: &[u8], offset: usize) -> Result<Option<f32>> {
    require_len(
        bytes,
        offset,
        RADIAL_CONSTANT_BLOCK_LEN,
        "radial constant block",
    )?;
    let raw = be_i16(&bytes[offset..offset + RADIAL_CONSTANT_BLOCK_LEN], 16);
    Ok((raw > 0).then_some(raw as f32 / 100.0))
}

fn parse_generic_moment_block(bytes: &[u8], offset: usize) -> Result<MomentBlock<'_>> {
    require_len(
        bytes,
        offset,
        GENERIC_DATA_BLOCK_LEN,
        "generic moment block",
    )?;
    let header = &bytes[offset..offset + GENERIC_DATA_BLOCK_LEN];
    let moment = MomentType::from_nexrad_bytes(&header[1..4]);
    let gate_count = usize::from(be_u16(header, 8));
    let first_gate_m = i32::from(be_i16(header, 10));
    let gate_spacing_m = i32::from(be_i16(header, 12));
    // ICD 2620002W Table XVII-B: bytes 16-17 SNR THRESHOLD (the SNR below
    // which the processor censored gates out of this moment before the file
    // was written), byte 18 CONTROL FLAGS (what, if anything, was
    // recombined). Both are per moment, per radial, and both are read here
    // for the first time - the parser used to step over them.
    let snr_threshold_db = f32::from(be_i16(header, 16)) / SNR_THRESHOLD_COUNTS_PER_DB;
    let recombination = MomentRecombination::from_control_flags(header[18]);
    let word_size = header[19];
    let scale = be_f32(header, 20);
    let offset_value = be_f32(header, 24);
    let data_offset = offset + GENERIC_DATA_BLOCK_LEN;

    let row = match word_size {
        8 => {
            require_len(bytes, data_offset, gate_count, "8-bit moment gates")?;
            MomentPayload::U8(&bytes[data_offset..data_offset + gate_count])
        }
        16 => {
            let byte_count = gate_count
                .checked_mul(2)
                .ok_or(NexradError::InvalidMessage {
                    offset,
                    reason: "16-bit moment gate count overflow".to_owned(),
                })?;
            require_len(bytes, data_offset, byte_count, "16-bit moment gates")?;
            MomentPayload::U16(&bytes[data_offset..data_offset + byte_count])
        }
        other => {
            return Err(NexradError::InvalidMessage {
                offset,
                reason: format!("unsupported moment word size {other}"),
            });
        }
    };

    Ok(MomentBlock {
        moment,
        gate_range: GateRange {
            first_gate_m,
            gate_spacing_m,
            gate_count,
        },
        scale,
        offset: offset_value,
        snr_threshold_db,
        recombination,
        row,
    })
}

fn nexrad_date_ms_to_datetime(date: u32, milliseconds: u32) -> DateTime<Utc> {
    let days = i64::from(date.saturating_sub(1));
    let seconds = days * 86_400 + i64::from(milliseconds / 1000);
    let nanos = (milliseconds % 1000) * 1_000_000;
    Utc.timestamp_opt(seconds, nanos)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn ascii_trim(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_owned()
}

fn require_len(bytes: &[u8], offset: usize, needed: usize, what: &'static str) -> Result<()> {
    let available = bytes.len().saturating_sub(offset);
    if available < needed {
        Err(NexradError::Truncated {
            what,
            offset,
            needed,
            available,
        })
    } else {
        Ok(())
    }
}

fn be_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn be_i16(bytes: &[u8], offset: usize) -> i16 {
    i16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn be_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn be_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(be_u32(bytes, offset))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    require_len(bytes, offset, 4, "u32")?;
    Ok(u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
    require_len(bytes, offset, 4, "i32")?;
    Ok(i32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    #[test]
    fn parses_archive_volume_header() {
        let bytes = synthetic_archive(false);
        let header = parse_volume_header(&bytes).unwrap();

        assert_eq!(header.archive_version, "AR2V000001");
        assert_eq!(header.icao, "KTLX");
        assert_eq!(
            header.volume_time,
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 1).unwrap()
        );
    }

    #[test]
    fn gzip_capacity_hint_reads_isize_footer() {
        let mut bytes = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0, 1, 2];
        bytes.extend_from_slice(&0xfeed_beefu32.to_le_bytes());
        bytes.extend_from_slice(&1_024u32.to_le_bytes());

        assert_eq!(gzip_decoded_capacity_hint(&bytes), Some(1_024));
    }

    #[test]
    fn gzip_capacity_hint_rejects_wildly_large_trailer() {
        let mut bytes = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());

        assert_eq!(gzip_decoded_capacity_hint(&bytes), None);
    }

    #[test]
    fn parses_message_header() {
        let bytes = synthetic_archive(false);
        let header = parse_message_header(&bytes, VOLUME_HEADER_LEN + CONTROL_WORD_LEN).unwrap();

        assert_eq!(header.message_type, 31);
        assert_eq!(header.sequence_id, 7);
        assert!(usize::from(header.size_halfwords) * 2 >= MESSAGE_HEADER_LEN + MSG_31_HEADER_LEN);
    }

    #[test]
    fn parses_message_31_header() {
        let body = synthetic_message_31_body(false);
        let header = parse_message_31_header(&body, 0).unwrap();

        assert_eq!(header.azimuth_number, 1);
        assert_eq!(header.azimuth_angle, 180.5);
        assert_eq!(header.elevation_angle, 0.5);
        assert_eq!(header.radial_status, RadialStatus::StartVolume);
        assert_eq!(header.block_pointers[0], 72);
        assert_eq!(header.block_pointers[3], 136);
    }

    #[test]
    fn decodes_synthetic_message_31_volume() {
        let bytes = synthetic_archive(false);
        let volume = decode_volume_from_bytes(&bytes).unwrap();

        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.site.latitude_deg, Some(35.333));
        assert_eq!(volume.vcp, Some(VcpInfo { pattern: 212 }));
        assert_eq!(volume.cuts.len(), 1);
        assert_eq!(volume.cuts[0].radials.len(), 1);

        let reflectivity = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .unwrap();
        assert_eq!(reflectivity.radial_count(), 1);
        assert_eq!(reflectivity.scaled_value(0, 1), Some(0.0));
        assert_eq!(reflectivity.scaled_value(0, 2), Some(7.0));
    }

    #[test]
    fn decodes_gzip_stream_without_normalized_buffer() {
        let bytes = synthetic_archive(false);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let volume = decode_volume_from_bytes(&compressed).unwrap();

        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("gzip".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn gzip_preview_waits_for_complete_displayable_cut() {
        let bytes = synthetic_archive(false);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let preview = decode_gzip_preview_from_bytes(&compressed, 1).unwrap();

        assert!(preview.is_none());
    }

    #[test]
    fn gzip_preview_returns_completed_displayable_cut() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let preview = decode_gzip_preview_from_bytes(&compressed, 1)
            .unwrap()
            .expect("completed first cut preview");

        assert_eq!(preview.site.id, "KTLX");
        assert_eq!(preview.metadata.compression, Some("gzip".to_owned()));
        assert_eq!(preview.cuts.len(), 1);
        assert_eq!(preview.cuts[0].radials.len(), 1);
        assert!(preview.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn gzip_preview_callback_continues_to_full_volume() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut preview_radials = None;

        let volume = decode_gzip_volume_from_bytes_with_preview(&compressed, 1, |preview| {
            preview_radials = Some(preview.metadata.decoded_radial_count);
        })
        .unwrap();

        assert_eq!(preview_radials, Some(1));
        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("gzip".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn decodes_bzip_blocks_without_concatenated_normalized_buffer() {
        let bytes = synthetic_archive(false);
        let compressed = synthetic_bzip_block_archive(&bytes);

        let volume = decode_volume_from_bytes(&compressed).unwrap();

        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("bzip2-blocks".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn bzip_preview_waits_for_complete_displayable_cut() {
        let bytes = synthetic_archive(false);
        let compressed = synthetic_bzip_block_archive(&bytes);

        let preview = decode_bzip_block_preview_from_bytes(&compressed, 1).unwrap();

        assert!(preview.is_none());
    }

    #[test]
    fn bzip_preview_returns_completed_displayable_cut() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let compressed = synthetic_bzip_block_archive(&bytes);

        let preview = decode_bzip_block_preview_from_bytes(&compressed, 1)
            .unwrap()
            .expect("completed first cut preview");

        assert_eq!(preview.site.id, "KTLX");
        assert_eq!(
            preview.metadata.compression,
            Some("bzip2-blocks".to_owned())
        );
        assert_eq!(preview.cuts.len(), 1);
        assert_eq!(preview.cuts[0].radials.len(), 1);
        assert!(
            preview.cuts[0]
                .moments
                .contains_key(&MomentType::Reflectivity)
        );
    }

    #[test]
    fn bzip_preview_full_decode_reuses_path_and_returns_full_volume() {
        let mut bytes = synthetic_archive(false);
        set_first_synthetic_radial_status(&mut bytes, RadialStatus::EndElevation);
        let compressed = synthetic_bzip_block_archive(&bytes);
        let mut preview_radials = None;

        let volume = decode_volume_from_bytes_with_bzip_preview(&compressed, 1, |preview| {
            preview_radials = Some(preview.metadata.decoded_radial_count);
        })
        .unwrap();

        assert_eq!(preview_radials, Some(1));
        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.compression, Some("bzip2-blocks".to_owned()));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert!(volume.cuts[0].moments.contains_key(&MomentType::Velocity));
    }

    #[test]
    fn decodes_synthetic_16_bit_moment() {
        let bytes = synthetic_archive(true);
        let volume = decode_volume_from_bytes(&bytes).unwrap();
        let phi = volume.cuts[0]
            .moments
            .get(&MomentType::DifferentialPhase)
            .unwrap();

        assert_eq!(phi.storage.word_size_bits(), 16);
        assert_eq!(phi.scaled_value(0, 1), Some(20.0));
    }

    #[test]
    fn expected_radials_follow_message31_azimuth_resolution_code() {
        assert_eq!(expected_radials_for_azimuth_resolution(1), 720);
        assert_eq!(expected_radials_for_azimuth_resolution(2), 360);
        assert_eq!(
            expected_radials_for_azimuth_resolution(0),
            FALLBACK_RADIALS_PER_CUT
        );
    }

    #[ignore = "set NEXRAD_LEVEL2_SAMPLE to a public Archive II file path to run manually"]
    #[test]
    fn decodes_real_public_level2_file_from_env() {
        let path = std::env::var("NEXRAD_LEVEL2_SAMPLE").expect("NEXRAD_LEVEL2_SAMPLE is not set");
        let volume = decode_volume_from_path(Path::new(&path)).unwrap();

        assert!(!volume.site.id.is_empty());
        assert!(
            !volume.cuts.is_empty(),
            "expected at least one decoded elevation cut"
        );
    }

    #[test]
    fn sniffs_every_container_by_its_magic_number() {
        let cases: [(&[u8], SupportedVolumeFormat); 8] = [
            (b"AR2V0006.473", SupportedVolumeFormat::NexradLevel2),
            (b"ARCHIVE2.027", SupportedVolumeFormat::NexradLevel2),
            (
                &[0x1f, 0x8b, 0x08, 0x00],
                SupportedVolumeFormat::NexradLevel2,
            ),
            (b"BZh91AY&SY", SupportedVolumeFormat::NexradLevel2),
            (b"\x89HDF\r\n\x1a\n\x00\x00", SupportedVolumeFormat::OdimH5),
            (b"CDF\x01\x00\x00\x00\x00", SupportedVolumeFormat::CfRadial1),
            (b"CDF\x02\x00\x00\x00\x00", SupportedVolumeFormat::CfRadial1),
            (
                b"PK\x03\x04\x14\x00\x00\x00",
                SupportedVolumeFormat::MobileDeploymentZip,
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                sniff_supported_volume_format(bytes),
                Some(expected),
                "wrong format for {:?}",
                &bytes[..4.min(bytes.len())]
            );
        }
    }

    /// A CDF-5 file gets the netCDF decoder's own explanation, not the
    /// Archive II decoder's.
    ///
    /// [`netcdf3::looks_like_netcdf3_bytes`] accepts `CDF\x05` on purpose,
    /// so that [`netcdf3::Nc3File::open`] can say "CDF-5 (64-bit data)
    /// netCDF is unsupported; convert with `nccopy -k classic`". The router
    /// used to test `1 | 2` alone, so the sniff returned `None` and the
    /// bytes fell through to the Archive II arm — where a perfectly
    /// identifiable netCDF file was reported as a short NEXRAD volume
    /// header. The message existed; nothing could reach it.
    #[test]
    fn a_cdf5_file_reaches_the_netcdf_decoders_own_message() {
        let mut bytes = b"CDF\x05".to_vec();
        bytes.resize(512, 0);

        assert_eq!(
            sniff_supported_volume_format(&bytes),
            Some(SupportedVolumeFormat::CfRadial1),
            "CDF-5 is a netCDF container, whatever this crate can do with it"
        );

        let error = decode_supported_volume_bytes(&bytes).unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with("CfRadial 1.x"),
            "the container should be named, got {message}"
        );
        assert!(
            message.contains("CDF-5"),
            "the CDF-5 message is the whole point, got {message}"
        );
        assert!(
            !message.contains("Archive II"),
            "a netCDF file must not be reported as a NEXRAD volume, got {message}"
        );
    }

    #[test]
    fn sniffs_dorade_only_when_the_block_length_is_credible() {
        // `COMM` followed by a block length that fits the buffer.
        let mut sweep = b"COMM".to_vec();
        sweep.extend_from_slice(&32u32.to_be_bytes());
        sweep.resize(64, 0);
        assert_eq!(
            sniff_supported_volume_format(&sweep),
            Some(SupportedVolumeFormat::Dorade)
        );

        // The same four letters as ordinary prose, which is why the length
        // check has to be part of the signature.
        assert_eq!(
            sniff_supported_volume_format(b"COMMENTARY ON THE WEATHER, AT LENGTH"),
            None
        );
    }

    #[test]
    fn sniff_returns_none_for_bytes_that_are_not_radar_data() {
        assert_eq!(sniff_supported_volume_format(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(sniff_supported_volume_format(b"{\"json\": true}"), None);
        assert_eq!(sniff_supported_volume_format(b""), None);
    }

    #[test]
    fn router_decodes_the_level2_arm() {
        let volume = decode_supported_volume_bytes(&synthetic_archive(false)).unwrap();
        assert_eq!(volume.site.id, "KTLX");
        assert_eq!(volume.metadata.decoded_radial_count, 1);
    }

    #[test]
    fn a_failed_decode_still_names_the_container_it_was_taken_for() {
        // Each of these is a valid signature followed by rubbish, so the
        // format's own decoder is reached and rejects the bytes. The point
        // of the seam is that the analyst is told WHICH decoder rejected
        // them: "ODIM_H5: ..." sends you to look at an HDF5 file, whereas
        // the Archive II parser's complaint about a short volume header
        // sends you looking for a truncated NEXRAD download that does not
        // exist.
        for (bytes, expected) in [
            (b"\x89HDF\r\n\x1a\n\x00\x00".as_slice(), "ODIM_H5"),
            (b"CDF\x01\x00\x00\x00\x00".as_slice(), "CfRadial 1.x"),
            (
                b"PK\x03\x04\x14\x00\x00\x00".as_slice(),
                "mobile deployment zip",
            ),
            // A COMM descriptor whose declared block length is honest, so
            // the sniff accepts it and the DORADE reader is the one that
            // finds nothing behind it.
            (b"COMM\x00\x00\x00\x10rubbish!".as_slice(), "DORADE"),
        ] {
            let error = decode_supported_volume_bytes(bytes).unwrap_err();
            assert!(
                matches!(&error, NexradError::Format { format, .. } if *format == expected),
                "expected {expected} to be named, got {error}"
            );
            assert!(
                error.to_string().starts_with(expected),
                "the message should lead with the container, got {error}"
            );
        }
    }

    #[test]
    fn router_looks_inside_a_gzip_wrapper_before_deciding() {
        // A gzipped ODIM_H5 file is byte-indistinguishable from a gzipped
        // Archive II volume from the outside, so the router has to inflate a
        // little before it can name the format.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(b"\x89HDF\r\n\x1a\n").unwrap();
        encoder.write_all(&[0u8; 256]).unwrap();
        let gzipped = encoder.finish().unwrap();

        assert_eq!(
            sniff_supported_volume_format(&gzipped),
            Some(SupportedVolumeFormat::NexradLevel2),
            "the outer bytes say gzip, which is a Level II wrapper"
        );
        assert_eq!(
            sniff_supported_volume_bytes(&gzipped),
            Some(SupportedVolumeFormat::OdimH5),
            "the inner bytes say HDF5"
        );
        // And the wrapper comes off before the ODIM decoder sees the bytes,
        // so the failure is HDF5's ("not enough bytes for a superblock"),
        // not gzip's.
        assert!(matches!(
            decode_supported_volume_bytes(&gzipped).unwrap_err(),
            NexradError::Format {
                format: "ODIM_H5",
                ..
            }
        ));
    }

    #[test]
    fn router_hands_unrecognised_bytes_to_the_archive_ii_decoder() {
        // Not "unsupported format" - the Archive II decoder's own complaint,
        // which is the one that helps when a file is not radar data at all.
        let error = decode_supported_volume_bytes(b"not a radar volume").unwrap_err();
        assert!(
            matches!(error, NexradError::ShortVolumeHeader { .. }),
            "expected the Level II decoder's error, got {error}"
        );
    }

    #[test]
    fn decodes_legacy_message_1_reflectivity_velocity_and_spectrum_width() {
        let volume = decode_volume_from_bytes(&synthetic_legacy_archive()).unwrap();

        assert_eq!(volume.vcp.map(|vcp| vcp.pattern), Some(11));
        assert_eq!(volume.metadata.decoded_radial_count, 1);
        assert_eq!(volume.cuts.len(), 1);
        let cut = &volume.cuts[0];
        assert_eq!(cut.elevation_number, Some(1));
        assert!((cut.elevation_deg - 0.4998).abs() < 0.001);

        let radial = &cut.radials[0];
        assert!((radial.azimuth_deg - 90.0).abs() < 0.01);
        assert_eq!(radial.radial_status, Some(RadialStatus::StartVolume));

        let reflectivity = &cut.moments[&MomentType::Reflectivity];
        assert_eq!(reflectivity.gate_range.gate_spacing_m, 1_000);
        assert_eq!(reflectivity.gate_range.first_gate_m, 0);
        // Codes 0 and 1 are below-threshold and range-folded; the rest decode
        // as dBZ = (code - 66) / 2 per ICD 2620002.
        assert_eq!(reflectivity.scaled_value(0, 0), None);
        assert_eq!(reflectivity.scaled_value(0, 1), None);
        assert_eq!(reflectivity.scaled_value(0, 2), Some(0.0));
        assert_eq!(reflectivity.scaled_value(0, 3), Some(7.0));

        let velocity = &cut.moments[&MomentType::Velocity];
        assert_eq!(velocity.gate_range.gate_spacing_m, 250);
        assert_eq!(velocity.gate_range.first_gate_m, -375);
        // m/s = (code - 129) / 2 at the 0.5 m/s resolution code.
        assert_eq!(velocity.scaled_value(0, 0), Some(0.0));
        assert_eq!(velocity.scaled_value(0, 1), Some(10.0));
        assert_eq!(velocity.scaled_value(0, 2), Some(-10.0));

        let spectrum_width = &cut.moments[&MomentType::SpectrumWidth];
        assert_eq!(spectrum_width.scaled_value(0, 0), Some(0.0));
        assert_eq!(spectrum_width.scaled_value(0, 1), Some(2.0));
        assert_eq!(spectrum_width.scaled_value(0, 2), Some(4.0));
    }

    /// Message 1 predates the generic data moment header, so it states no SNR
    /// threshold and no control flags. Both must stay `None` rather than
    /// decode as a 0.0 dB threshold on an un-recombined sweep, which is what a
    /// non-`Option` field would have implied about a file that says nothing.
    #[test]
    fn legacy_message_1_states_no_censoring_facts() {
        let volume = decode_volume_from_bytes(&synthetic_legacy_archive()).unwrap();
        for (moment, grid) in &volume.cuts[0].moments {
            assert_eq!(grid.snr_threshold_db, None, "{moment}");
            assert_eq!(grid.recombination, None, "{moment}");
        }
    }

    #[test]
    fn legacy_nyquist_comes_from_halfword_31_not_the_reserved_field() {
        // Halfword 24 is reserved for RDA internal use. The fixture puts a
        // decoy there that would decode as 99.99 m/s if it were read as the
        // Nyquist velocity, which is what pins the correct offset in place.
        let volume = decode_volume_from_bytes(&synthetic_legacy_archive()).unwrap();
        let nyquist = volume.cuts[0].radials[0].nyquist_velocity_mps;
        assert_eq!(nyquist, Some(26.1));
    }

    #[test]
    fn legacy_velocity_resolution_code_selects_the_scale() {
        assert_eq!(legacy_message_1_velocity_scale(2), 2.0);
        assert_eq!(legacy_message_1_velocity_scale(4), 1.0);
        // An out-of-range code falls back to the common 0.5 m/s coding
        // rather than producing a zero scale and infinite velocities.
        assert_eq!(legacy_message_1_velocity_scale(0), 2.0);
    }

    #[test]
    fn legacy_radial_without_any_moment_is_skipped_not_fatal() {
        let mut bytes = synthetic_legacy_archive();
        let pointers = VOLUME_HEADER_LEN + CONTROL_WORD_LEN + MESSAGE_HEADER_LEN;
        for offset in [
            msg1::REFLECTIVITY_POINTER,
            msg1::VELOCITY_POINTER,
            msg1::SPECTRUM_WIDTH_POINTER,
        ] {
            bytes[pointers + offset..pointers + offset + 2].copy_from_slice(&0u16.to_be_bytes());
        }

        let volume = decode_volume_from_bytes(&bytes).unwrap();
        assert_eq!(volume.metadata.decoded_radial_count, 0);
        assert_eq!(volume.metadata.skipped_message_count, 1);
    }

    #[test]
    fn decodes_back_to_back_message_31s_framed_the_gr2analyst_way() {
        let bytes = synthetic_variable_framed_archive(3);
        let volume = decode_volume_from_bytes(&bytes).unwrap();

        assert_eq!(
            volume.metadata.decoded_radial_count, 3,
            "all three variable-framed radials should decode"
        );
    }

    #[test]
    fn variable_framing_is_detected_only_when_the_next_record_is_a_message_31() {
        let variable = synthetic_variable_framed_archive(2);
        let message_total_len =
            usize::from(be_u16(&variable, VOLUME_HEADER_LEN + CONTROL_WORD_LEN)) * 2;
        assert!(message31_uses_variable_framing(
            &variable,
            VOLUME_HEADER_LEN,
            message_total_len
        ));

        // The standard fixture pads its single message out to a full 2432
        // byte record, which is exactly what the detector must not mistake
        // for back-to-back framing.
        let fixed = synthetic_archive(false);
        let fixed_len = usize::from(be_u16(&fixed, VOLUME_HEADER_LEN + CONTROL_WORD_LEN)) * 2;
        assert!(!message31_uses_variable_framing(
            &fixed,
            VOLUME_HEADER_LEN,
            fixed_len
        ));
    }

    #[test]
    fn fixed_framed_early_message_31_still_decodes_as_a_fixed_record() {
        // The latch must not change how an ordinary Archive II volume is
        // walked; this is the regression guard for that.
        let volume = decode_volume_from_bytes(&synthetic_archive(false)).unwrap();
        assert_eq!(volume.metadata.decoded_radial_count, 1);
    }

    #[ignore = "set NEXRAD_LEGACY_SAMPLE to a pre-2008 Archive II file path to run manually"]
    #[test]
    fn decodes_real_legacy_message_1_file_from_env() {
        let path = std::env::var("NEXRAD_LEGACY_SAMPLE").expect("NEXRAD_LEGACY_SAMPLE is not set");
        let volume = decode_volume_from_path(Path::new(&path)).unwrap();

        assert!(!volume.cuts.is_empty());
        assert!(volume.metadata.decoded_radial_count > 1_000);
        assert!(
            volume
                .cuts
                .iter()
                .any(|cut| cut.moments.contains_key(&MomentType::Velocity)),
            "a legacy volume should carry Doppler cuts"
        );
        assert!(
            volume
                .cuts
                .iter()
                .flat_map(|cut| &cut.radials)
                .any(|radial| {
                    radial
                        .nyquist_velocity_mps
                        .is_some_and(|nyquist| (10.0..40.0).contains(&nyquist))
                }),
            "Doppler radials should carry a physically plausible Nyquist velocity"
        );
    }

    /// A one-record Archive II volume whose single message is a legacy
    /// Message Type 1 radial carrying all three legacy moments.
    ///
    /// `pub(crate)` so `mobile_archive`'s tests can put a pre-2008 tape
    /// inside a zip and prove the archive reader accepts the same tape
    /// identifiers the top-level router does.
    pub(crate) fn synthetic_legacy_archive() -> Vec<u8> {
        let mut body = vec![0u8; MSG_1_HEADER_LEN];
        let put_u16 = |body: &mut Vec<u8>, offset: usize, value: u16| {
            body[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
        };
        body[msg1::COLLECT_MS..msg1::COLLECT_MS + 4].copy_from_slice(&1_000u32.to_be_bytes());
        put_u16(&mut body, msg1::COLLECT_DATE, 19_724);
        // 90 degrees, as a full turn spread over the 16-bit range.
        put_u16(&mut body, msg1::AZIMUTH_ANGLE, 16_384);
        put_u16(&mut body, msg1::RADIAL_STATUS, 3);
        put_u16(&mut body, msg1::ELEVATION_ANGLE, 91);
        put_u16(&mut body, msg1::ELEVATION_NUMBER, 1);
        put_u16(&mut body, msg1::SURVEILLANCE_RANGE, 0);
        put_u16(&mut body, msg1::DOPPLER_RANGE, (-375i16) as u16);
        put_u16(&mut body, msg1::SURVEILLANCE_RANGE_STEP, 1_000);
        put_u16(&mut body, msg1::DOPPLER_RANGE_STEP, 250);
        put_u16(&mut body, msg1::SURVEILLANCE_BIN_COUNT, 4);
        put_u16(&mut body, msg1::DOPPLER_BIN_COUNT, 3);
        put_u16(&mut body, msg1::REFLECTIVITY_POINTER, 100);
        put_u16(&mut body, msg1::VELOCITY_POINTER, 104);
        put_u16(&mut body, msg1::SPECTRUM_WIDTH_POINTER, 107);
        put_u16(&mut body, msg1::VELOCITY_RESOLUTION, 2);
        put_u16(&mut body, msg1::VOLUME_COVERAGE_PATTERN, 11);
        // Decoy in the reserved halfword 24 that an off-by-seven-halfword
        // read of the Nyquist velocity would pick up as 99.99 m/s.
        put_u16(&mut body, 46, 9_999);
        put_u16(&mut body, msg1::NYQUIST_VELOCITY, 2_610);

        body.extend_from_slice(&[0, 1, 66, 80]);
        body.extend_from_slice(&[129, 149, 109]);
        body.extend_from_slice(&[129, 133, 137]);

        let mut bytes = legacy_volume_header();
        bytes.extend_from_slice(&[0u8; CONTROL_WORD_LEN]);
        bytes.extend_from_slice(&message_header(1, &body));
        bytes.extend_from_slice(&body);
        bytes.resize(VOLUME_HEADER_LEN + RECORD_BYTES, 0);
        bytes
    }

    /// The 24-byte volume header a pre-2008 tape carries: an `ARCHIVE2` tape
    /// identifier and, unlike modern files, no ICAO.
    fn legacy_volume_header() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ARCHIVE2.");
        bytes.extend_from_slice(b"027");
        bytes.extend_from_slice(&19_724u32.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes
    }

    fn message_header(message_type: u8, body: &[u8]) -> Vec<u8> {
        let mut header = Vec::with_capacity(MESSAGE_HEADER_LEN);
        let size = u16::try_from((MESSAGE_HEADER_LEN + body.len()) / 2).unwrap();
        header.extend_from_slice(&size.to_be_bytes());
        header.push(0);
        header.push(message_type);
        header.extend_from_slice(&7u16.to_be_bytes());
        header.extend_from_slice(&19_724u16.to_be_bytes());
        header.extend_from_slice(&1_000u32.to_be_bytes());
        header.extend_from_slice(&1u16.to_be_bytes());
        header.extend_from_slice(&1u16.to_be_bytes());
        header
    }

    /// A GR2Analyst-convention export: an Archive II volume header followed
    /// immediately by message 31 records packed back to back, each preceded
    /// by its control word and followed by no padding at all.
    ///
    /// `pub(crate)` so `mobile_archive`'s tests can put one inside a zip and
    /// prove the framing latch is reached through the archive path too.
    pub(crate) fn synthetic_variable_framed_archive(radials: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"AR2V00000");
        bytes.extend_from_slice(b"1  ");
        bytes.extend_from_slice(&19_724u32.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(b"KTLX");

        for radial in 0..radials {
            let mut body = synthetic_message_31_body(false);
            // Azimuth number and angle advance so the radials are distinct;
            // every radial after the first is an intermediate one.
            body[10..12].copy_from_slice(&(radial as u16 + 1).to_be_bytes());
            let azimuth = 180.5f32 + radial as f32;
            body[12..16].copy_from_slice(&azimuth.to_bits().to_be_bytes());
            if radial > 0 {
                body[21] = 1;
            }
            bytes.extend_from_slice(&[0u8; CONTROL_WORD_LEN]);
            bytes.extend_from_slice(&message_header(31, &body));
            bytes.extend_from_slice(&body);
        }
        bytes
    }

    fn synthetic_archive(include_phi_16: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"AR2V00000");
        bytes.extend_from_slice(b"1  ");
        bytes.extend_from_slice(&19_724u32.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(b"KTLX");

        bytes.extend_from_slice(&[0u8; CONTROL_WORD_LEN]);
        let body = synthetic_message_31_body(include_phi_16);
        let message_size = u16::try_from((MESSAGE_HEADER_LEN + body.len()) / 2).unwrap();
        bytes.extend_from_slice(&message_size.to_be_bytes());
        bytes.push(0);
        bytes.push(31);
        bytes.extend_from_slice(&7u16.to_be_bytes());
        bytes.extend_from_slice(&19_724u16.to_be_bytes());
        bytes.extend_from_slice(&1_000u32.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes.resize(VOLUME_HEADER_LEN + RECORD_BYTES, 0);
        bytes
    }

    fn set_first_synthetic_radial_status(bytes: &mut [u8], status: RadialStatus) {
        let status = match status {
            RadialStatus::StartElevation => 0,
            RadialStatus::Intermediate => 1,
            RadialStatus::EndElevation => 2,
            RadialStatus::StartVolume => 3,
            RadialStatus::EndVolume => 4,
            RadialStatus::StartElevationLastCut => 5,
            RadialStatus::Unknown(value) => value,
        };
        let offset = VOLUME_HEADER_LEN + CONTROL_WORD_LEN + MESSAGE_HEADER_LEN + 21;
        bytes[offset] = status;
    }

    fn synthetic_bzip_block_archive(normalized: &[u8]) -> Vec<u8> {
        let mut encoder = BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder.write_all(&normalized[VOLUME_HEADER_LEN..]).unwrap();
        let compressed_block = encoder.finish().unwrap();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&normalized[..VOLUME_HEADER_LEN]);
        bytes.extend_from_slice(
            &i32::try_from(compressed_block.len())
                .expect("compressed block length fits")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&compressed_block);
        bytes.extend_from_slice(&(-1_i32).to_be_bytes());
        bytes
    }

    fn synthetic_message_31_body(include_phi_16: bool) -> Vec<u8> {
        let mut body = vec![0u8; MSG_31_HEADER_LEN];
        body[0..4].copy_from_slice(b"AR2V");
        body[4..8].copy_from_slice(&1_000u32.to_be_bytes());
        body[8..10].copy_from_slice(&19_724u16.to_be_bytes());
        body[10..12].copy_from_slice(&1u16.to_be_bytes());
        body[12..16].copy_from_slice(&180.5f32.to_bits().to_be_bytes());
        body[18..20].copy_from_slice(&1u16.to_be_bytes());
        body[20] = 2;
        body[21] = 3;
        body[22] = 1;
        body[23] = 1;
        body[24..28].copy_from_slice(&0.5f32.to_bits().to_be_bytes());
        body[30..32].copy_from_slice(&(if include_phi_16 { 5u16 } else { 4u16 }).to_be_bytes());

        let vol_pointer = body.len();
        push_volume_block(&mut body);
        let rad_pointer = body.len();
        push_radial_block(&mut body);
        let ref_pointer = body.len();
        push_u8_moment(&mut body, b"DREF", &[0, 66, 80]);
        let vel_pointer = body.len();
        push_u8_moment(&mut body, b"DVEL", &[129, 139, 119]);
        let phi_pointer = body.len();
        if include_phi_16 {
            push_u16_moment(&mut body, b"DPHI", &[0, 20, 40]);
        }

        set_pointer(&mut body, 0, vol_pointer);
        set_pointer(&mut body, 2, rad_pointer);
        set_pointer(&mut body, 3, ref_pointer);
        set_pointer(&mut body, 4, vel_pointer);
        if include_phi_16 {
            set_pointer(&mut body, 7, phi_pointer);
        }
        body
    }

    fn push_volume_block(body: &mut Vec<u8>) {
        body.extend_from_slice(b"RVOL");
        body.extend_from_slice(&1u16.to_be_bytes());
        body.push(1);
        body.push(0);
        body.extend_from_slice(&35.333f32.to_bits().to_be_bytes());
        body.extend_from_slice(&(-97.277f32).to_bits().to_be_bytes());
        body.extend_from_slice(&370i16.to_be_bytes());
        body.extend_from_slice(&20u16.to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&212u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
    }

    fn push_radial_block(body: &mut Vec<u8>) {
        body.extend_from_slice(b"RRAD");
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&2_500i16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
    }

    fn push_u8_moment(body: &mut Vec<u8>, id: &[u8; 4], gates: &[u8]) {
        body.extend_from_slice(id);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(gates.len() as u16).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&250i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0);
        body.push(8);
        body.extend_from_slice(&2.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&66.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(gates);
        if !body.len().is_multiple_of(2) {
            body.push(0);
        }
    }

    fn push_u16_moment(body: &mut Vec<u8>, id: &[u8; 4], gates: &[u16]) {
        body.extend_from_slice(id);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(gates.len() as u16).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&250i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0);
        body.push(16);
        body.extend_from_slice(&1.0f32.to_bits().to_be_bytes());
        body.extend_from_slice(&0.0f32.to_bits().to_be_bytes());
        for gate in gates {
            body.extend_from_slice(&gate.to_be_bytes());
        }
    }

    fn set_pointer(body: &mut [u8], pointer_index: usize, value: usize) {
        let offset = 32 + pointer_index * 4;
        body[offset..offset + 4].copy_from_slice(&(value as u32).to_be_bytes());
    }
}
