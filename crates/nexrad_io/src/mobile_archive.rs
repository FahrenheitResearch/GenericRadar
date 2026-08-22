//! Zip-archive and deployment-folder ingest for mobile research radar data
//! (DOW/COW/RaXPol/NOXP).
//!
//! Field deployments are distributed as `.zip` files holding DORADE
//! sweepfiles (`swp.*`) and/or GR2-style `.msg31` Archive II twins, often for
//! several radars in one archive (a Goodland deployment zip carries
//! `DORADE/DOW7/...` next to `DORADE/COW2/...`). This module discovers radar
//! members, groups DORADE sweeps into volume scans, and decodes everything
//! into [`radar_core::RadarVolume`]s. `.msg31`/Archive II members (either tape
//! identifier) are routed back through this crate's Level II decoder, then
//! joined only when [`crate::sweep_assembly`] proves a shared internal volume
//! identity.
//!
//! Sibling-directory grouping for loose (non-zip) sweepfiles lives here too:
//! opening one `swp.*` file pulls in the rest of its ascending run from the
//! same directory.
//!
//! # The zip reader is bespoke
//!
//! This crate takes no zip dependency, so the reader here is written against
//! the format specification directly: PKWARE *APPNOTE.TXT*, `.ZIP` File
//! Format Specification, version 6.3.10 — sections 4.3.6 (local file header),
//! 4.3.12 (central directory header), 4.3.14 (Zip64 end of central
//! directory), 4.3.15 (Zip64 locator), and 4.3.16 (end of central
//! directory). It is deliberately a *reader* and deliberately small:
//!
//! - The central directory is authoritative when present, and Zip64 archives
//!   read through their Zip64 end-of-central-directory record.
//! - Offsets recorded inside a zip are relative to the start of the zip data,
//!   not to the start of the file, so an archive behind a prepended stub (a
//!   self-extracting executable, an installer payload) is read by recovering
//!   that delta from the end-of-central-directory record and applying it to
//!   the directory and to every member — see [`resolve_archive_delta`]. One
//!   prepended byte is enough to matter, and the delta is confirmed against
//!   the record magic before it is used.
//! - When no end-of-central-directory record is found — a truncated or
//!   streamed archive — it falls back to walking local file records from the
//!   first one in the file, which is enough for every writer that records
//!   sizes in the local header.
//! - Store (method 0) and deflate (method 8) are supported; deflate uses the
//!   `flate2` decoder already in this crate. Any other method is reported by
//!   name rather than silently skipped.
//! - Every member's CRC-32 is verified against the directory entry, so a
//!   corrupt archive fails loudly instead of decoding into garbage radials.
//! - Encrypted members (general-purpose bit 0) are rejected.
//!
//! # Volume grouping
//!
//! DORADE sweeps are grouped per instrument into ascending fixed-angle runs
//! ordered by sweep start time, the way a VCP executes: a volume spread
//! across `Tilt 0.5/ ... Tilt 4.0/` member directories reassembles into one
//! five-cut volume scan, while a single-tilt 12-second surveillance sequence
//! becomes one frame per sweep instead of a 24-cut blob. VOLD volume numbers
//! are deliberately not the key: they are writer-dependent (some writers
//! increment per sweep, some per volume scan), so elevation-run segmentation
//! is the only convention that holds across radars. A new run also starts
//! after a 15-minute gap (deployment pause).
//!
//! One-cut Archive II exports have a stronger source-specific fact: their
//! internal three-digit volume-header sequence. They are grouped per radar
//! only when that sequence, exact recorded position, UTC day and VCP all match
//! and radial time is contiguous. A `vNNN` filename is never used as a key.
//!
//! Ported from the Fahrenheit Research BowEcho archive reader; the zip layer
//! is new, because that one used a zip crate and this workspace does not
//! take the dependency.
//!
//! # Deliberate divergences from the reader this was ported from
//!
//! - **Duplicate member names.** An archive holding two members under one
//!   name decodes here as two sweeps. The reader this was ported from indexes
//!   members by name, so it reports one. Both records really are in the
//!   archive, and a deployment that wrote the same sweep twice should show
//!   both frames rather than have one silently disappear.
//! - **Entry counts are advisory.** The count in the end-of-central-directory
//!   record is not enforced against the number of records the directory walk
//!   actually yields; see [`read_central_directory`]. The reader this was
//!   ported from trusts that count instead, and on real archives carrying a
//!   wrong one that goes two ways, both bad: a count of 1 in front of two
//!   members yields one volume and the second sweep disappears silently,
//!   while a count of 9999 makes the whole archive unreadable. The walk here
//!   is bounded by the records' own signatures, so every member that is
//!   really in the archive is read. A directory that yields nothing at all is
//!   still an error.
//! - The DORADE-level divergences (block padding, unusable latitudes,
//!   `DB_`-prefixed field names) are documented in [`crate::dorade`].

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use flate2::Crc;
use flate2::read::DeflateDecoder;
use radar_core::RadarVolume;
use rayon::prelude::*;

use crate::dorade::{
    append_dorade_sweep, decode_dorade_sweep, empty_dorade_volume, finalize_dorade_volume,
    looks_like_dorade_bytes, looks_like_dorade_name, peek_dorade_sweep,
};
use crate::sweep_assembly::{
    ProvenSweepMembership, SweepAssemblyClassification, SweepAssemblyDecision, append_proven_sweep,
    classify_archive_sweep, decide_adjacent_sweeps,
};
use crate::{NexradError, Result, decode_volume_from_bytes};

const ZIP_LOCAL_FILE_MAGIC: &[u8; 4] = b"PK\x03\x04";
/// Mobile deployments can contain hundreds of real sweeps, but retaining an
/// unbounded archive before parallel decode lets a zip bomb or an accidental
/// multi-day tree exhaust memory. These ceilings are deliberately much larger
/// than one operational volume while keeping peak retention finite.
const MAX_MOBILE_MEMBER_BYTES: usize = 256 * 1024 * 1024;
const MAX_MOBILE_ARCHIVE_BYTES: usize = 1024 * 1024 * 1024;
const MAX_MOBILE_MEMBERS: usize = 4096;

/// `true` when the buffer starts with a local-file zip signature.
///
/// The empty-archive variant (`PK\x05\x06`) is not radar data and is not
/// accepted.
pub fn looks_like_zip_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == ZIP_LOCAL_FILE_MAGIC
}

/// `true` when the path claims to be a zip archive.
pub fn looks_like_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

/// One decoded volume scan plus where it came from inside the archive.
#[derive(Clone, Debug)]
pub struct MobileVolume {
    pub volume: RadarVolume,
    /// Display label: first member name of the group (`swp....` or `*.msg31`).
    pub member_label: String,
    /// Number of archive members merged into this volume.
    pub member_count: usize,
}

/// Decode every radar volume in a deployment zip archive, sorted by scan time.
///
/// This is the crate entry point for deployment archives. DORADE members
/// group per instrument into ascending fixed-angle runs (see the module
/// docs); `.msg31`/Archive II members decode one volume each through the Level II
/// decoder. Non-radar members are ignored; a corrupt radar member fails the
/// whole load with a descriptive error, because a deployment archive with
/// undecodable scans should be visible rather than silently thinner.
pub fn decode_deployment_zip(bytes: &[u8]) -> Result<Vec<MobileVolume>> {
    let members = read_zip_radar_members(bytes, "zip archive")?;
    if members.is_empty() {
        return Err(invalid_archive(
            "zip archive contains no radar members (swp.* sweepfiles or .msg31/Archive II volumes)"
                .to_owned(),
        ));
    }
    decode_members(None, members)
}

/// Read a deployment zip from disk and decode it.
pub fn decode_deployment_zip_from_path(path: &Path) -> Result<Vec<MobileVolume>> {
    let bytes = read_file_limited(path, MAX_MOBILE_ARCHIVE_BYTES)?;
    let label = path.display().to_string();
    let members = read_zip_radar_members(&bytes, &label)?;
    if members.is_empty() {
        return Err(invalid_archive(format!(
            "zip archive {label} contains no radar members (swp.* sweepfiles or .msg31/Archive II volumes)"
        )));
    }
    decode_members(Some(&label), members)
}

/// Decode every radar volume under a deployment FOLDER (recursive, a few
/// levels).
///
/// Research data also ships as directories of per-sweep DORADE files — one
/// file per tilt — so the folder, not the file, is the natural open unit.
/// Same sniffing and volume grouping as zips.
pub fn decode_mobile_dir_from_path(dir: &Path) -> Result<Vec<MobileVolume>> {
    let mut members = Vec::new();
    let mut budget = MemberBudget::default();
    collect_dir_members(dir, dir, &mut members, &mut budget, 0)?;
    if members.is_empty() {
        return Err(invalid_archive(format!(
            "folder {} contains no radar files (swp.* sweepfiles or .msg31/Archive II volumes)",
            dir.display()
        )));
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    let label = dir.display().to_string();
    decode_members(Some(&label), members)
}

/// Deployment trees are shallow (day/instrument levels); the cap only guards
/// against scanning an accidentally-chosen huge root.
const MAX_DIR_DEPTH: usize = 4;

fn collect_dir_members(
    root: &Path,
    dir: &Path,
    members: &mut Vec<RadarMember>,
    budget: &mut MemberBudget,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DIR_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| NexradError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dir_members(root, &path, members, budget, depth + 1)?;
            continue;
        }
        let name = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if !plausible_radar_member_name(&name) {
            continue;
        }
        let declared = file_len_usize(&path)?;
        budget.reserve(&name, declared)?;
        let bytes = read_file_limited(&path, MAX_MOBILE_MEMBER_BYTES)?;
        if looks_like_radar_member(&bytes) {
            members.push(RadarMember { name, bytes });
        }
    }
    Ok(())
}

#[derive(Debug)]
struct RadarMember {
    name: String,
    bytes: Vec<u8>,
}

/// Content sniff: the name pre-filter only narrows the candidates.
///
/// The Archive II arm tests [`crate::ARCHIVE_II_MAGICS`] — the SAME pair the
/// top-level router tests — so a pre-2008 `ARCHIVE2` tape inside a
/// deployment zip decodes exactly as it does loose on disk. Testing only
/// `AR2V` here dropped such a member without a word: `decode_deployment_zip`
/// keeps what this returns `true` for and never sees the rest, so a legacy
/// volume did not fail to decode, it failed to EXIST.
fn looks_like_radar_member(bytes: &[u8]) -> bool {
    looks_like_dorade_bytes(bytes)
        || crate::starts_with_archive_ii_magic(bytes)
        || bytes.starts_with(&[0x1f, 0x8b])
        || bytes.starts_with(b"BZh")
}

#[derive(Default)]
struct MemberBudget {
    candidates: usize,
    expanded_bytes: usize,
}

impl MemberBudget {
    fn reserve(&mut self, name: &str, bytes: usize) -> Result<()> {
        if bytes > MAX_MOBILE_MEMBER_BYTES {
            return Err(invalid_archive(format!(
                "archive member {name} declares {bytes} bytes (per-member limit {MAX_MOBILE_MEMBER_BYTES})"
            )));
        }
        let candidates = self
            .candidates
            .checked_add(1)
            .ok_or_else(|| invalid_archive("mobile archive member count overflow".to_owned()))?;
        if candidates > MAX_MOBILE_MEMBERS {
            return Err(invalid_archive(format!(
                "mobile archive contains more than {MAX_MOBILE_MEMBERS} candidate radar members"
            )));
        }
        let expanded_bytes = self
            .expanded_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid_archive("mobile archive expanded-size overflow".to_owned()))?;
        if expanded_bytes > MAX_MOBILE_ARCHIVE_BYTES {
            return Err(invalid_archive(format!(
                "mobile archive candidate members exceed the {MAX_MOBILE_ARCHIVE_BYTES}-byte aggregate limit"
            )));
        }
        self.candidates = candidates;
        self.expanded_bytes = expanded_bytes;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bespoke zip reader (PKWARE APPNOTE.TXT 6.3.10)
// ---------------------------------------------------------------------------

const EOCD_MAGIC: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_MAGIC: u32 = 0x0706_4b50;
const ZIP64_EOCD_MAGIC: u32 = 0x0606_4b50;
const CENTRAL_HEADER_MAGIC: u32 = 0x0201_4b50;
const LOCAL_HEADER_MAGIC: u32 = 0x0403_4b50;
const EOCD_MIN_LEN: usize = 22;
/// APPNOTE 4.3.14: signature, 8-byte record size, and a 44-byte body.
const ZIP64_EOCD_MIN_LEN: usize = 56;
/// APPNOTE 4.3.15: the locator is fixed-length and sits between the Zip64
/// record and the classic one.
const ZIP64_LOCATOR_LEN: usize = 20;
const CENTRAL_HEADER_MIN_LEN: usize = 46;
const LOCAL_HEADER_MIN_LEN: usize = 30;
/// APPNOTE 4.4.11: the trailing comment is a 16-bit length.
const MAX_ZIP_COMMENT_LEN: usize = u16::MAX as usize;
const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;
const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
/// General purpose bit 0: the member is encrypted.
const FLAG_ENCRYPTED: u16 = 1 << 0;

/// One central-directory (or local-record) entry, resolved to a byte range.
#[derive(Debug)]
struct ZipEntry {
    name: String,
    method: u16,
    crc32: u32,
    compressed_offset: usize,
    compressed_len: usize,
    uncompressed_len: usize,
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|slice| u16::from_le_bytes([slice[0], slice[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|slice| u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes.get(offset..offset + 8).map(|slice| {
        u64::from_le_bytes([
            slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
        ])
    })
}

/// Locate the end-of-central-directory record (APPNOTE 4.3.16).
fn find_end_of_central_directory(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < EOCD_MIN_LEN {
        return None;
    }
    let highest = bytes.len() - EOCD_MIN_LEN;
    let lowest = highest.saturating_sub(MAX_ZIP_COMMENT_LEN);
    (lowest..=highest)
        .rev()
        .find(|&offset| le_u32(bytes, offset) == Some(EOCD_MAGIC))
}

/// Where the central directory really starts, and how far the whole archive
/// sits from the start of the file.
#[derive(Debug)]
struct CentralDirectory {
    /// Absolute position of the first central-directory header.
    start: usize,
    /// Entry count as declared by the EOCD; advisory, see
    /// [`read_central_directory`].
    entries: usize,
    /// Bytes of non-zip data in front of the archive; add it to every offset
    /// the archive records about itself.
    delta: usize,
}

/// `true` when a central-directory header really is at `cd_offset + delta`.
///
/// This is how a candidate archive delta is confirmed: the recorded offset is
/// only meaningful once the right delta is added to it.
fn central_directory_is_at(bytes: &[u8], cd_offset: u64, delta: usize, entries: usize) -> bool {
    let Ok(offset) = usize::try_from(cd_offset) else {
        return false;
    };
    let Some(start) = offset.checked_add(delta) else {
        return false;
    };
    if entries == 0 {
        return start <= bytes.len();
    }
    le_u32(bytes, start) == Some(CENTRAL_HEADER_MAGIC)
}

/// Recover the archive delta: how many bytes of something else sit in front
/// of the zip data.
///
/// Every offset a zip records about itself is relative to the start of the
/// *zip data*, which is not the start of the file when a self-extracting
/// stub, an installer, or any other payload has been prepended (APPNOTE
/// 4.1.9, and the 4.3.16 note that the archive may be preceded by other
/// data). The delta is recoverable because the position of the
/// end-of-central-directory record is known absolutely and the directory it
/// describes must end exactly where that record begins:
///
/// ```text
/// delta = eocd_position - central_directory_size - central_directory_offset
/// ```
///
/// The result is confirmed against the header magic at the implied start
/// before it is trusted; anything that does not resolve falls back to zero,
/// which is the flat-archive case.
fn resolve_archive_delta(
    bytes: &[u8],
    descriptor: usize,
    cd_size: u64,
    cd_offset: u64,
    entries: usize,
) -> usize {
    let computed = usize::try_from(cd_size).ok().and_then(|size| {
        let recorded = usize::try_from(cd_offset).ok()?;
        descriptor.checked_sub(size)?.checked_sub(recorded)
    });
    match computed {
        Some(delta) if central_directory_is_at(bytes, cd_offset, delta, entries) => delta,
        _ => 0,
    }
}

/// Find the Zip64 EOCD record the locator points at (APPNOTE 4.3.14).
///
/// The offset in the locator is relative to the start of the zip data like
/// every other recorded offset, so a stub moves it too. Candidates, in order:
/// the offset shifted by the delta recovered from the classic record, the
/// offset as written, and the standard layout where the record sits directly
/// in front of its own locator.
fn locate_zip64_eocd(bytes: &[u8], eocd: usize, recorded: u64, delta: usize) -> Option<usize> {
    let recorded = usize::try_from(recorded).ok()?;
    let adjacent = eocd.checked_sub(ZIP64_LOCATOR_LEN + ZIP64_EOCD_MIN_LEN);
    [recorded.checked_add(delta), Some(recorded), adjacent]
        .into_iter()
        .flatten()
        .find(|&candidate| le_u32(bytes, candidate) == Some(ZIP64_EOCD_MAGIC))
}

/// Central-directory position and entry count, honouring Zip64 when the
/// 32-bit fields are saturated (APPNOTE 4.3.14/4.3.15) and any prepended
/// data (see [`resolve_archive_delta`]).
fn central_directory_location(bytes: &[u8], eocd: usize) -> Result<CentralDirectory> {
    let mut entries = usize::from(
        le_u16(bytes, eocd + 10).ok_or_else(|| invalid_archive("truncated zip EOCD".to_owned()))?,
    );
    let mut size = u64::from(
        le_u32(bytes, eocd + 12).ok_or_else(|| invalid_archive("truncated zip EOCD".to_owned()))?,
    );
    let mut offset = u64::from(
        le_u32(bytes, eocd + 16).ok_or_else(|| invalid_archive("truncated zip EOCD".to_owned()))?,
    );
    let mut descriptor = eocd;

    let saturated = entries == usize::from(u16::MAX)
        || offset == u64::from(u32::MAX)
        || size == u64::from(u32::MAX);
    if saturated
        && eocd >= ZIP64_LOCATOR_LEN
        && le_u32(bytes, eocd - ZIP64_LOCATOR_LEN) == Some(ZIP64_LOCATOR_MAGIC)
    {
        let recorded = le_u64(bytes, eocd - ZIP64_LOCATOR_LEN + 8)
            .ok_or_else(|| invalid_archive("truncated Zip64 locator".to_owned()))?;
        let classic_delta = resolve_archive_delta(bytes, eocd, size, offset, entries);
        let zip64_eocd =
            locate_zip64_eocd(bytes, eocd, recorded, classic_delta).ok_or_else(|| {
                invalid_archive("Zip64 locator does not point at a Zip64 EOCD record".to_owned())
            })?;
        let zip64_entries = le_u64(bytes, zip64_eocd + 32)
            .ok_or_else(|| invalid_archive("truncated Zip64 EOCD".to_owned()))?;
        let zip64_size = le_u64(bytes, zip64_eocd + 40)
            .ok_or_else(|| invalid_archive("truncated Zip64 EOCD".to_owned()))?;
        let zip64_offset = le_u64(bytes, zip64_eocd + 48)
            .ok_or_else(|| invalid_archive("truncated Zip64 EOCD".to_owned()))?;
        entries = usize::try_from(zip64_entries)
            .map_err(|_| invalid_archive("Zip64 entry count overflows this platform".to_owned()))?;
        size = zip64_size;
        offset = zip64_offset;
        descriptor = zip64_eocd;
    }

    let delta = resolve_archive_delta(bytes, descriptor, size, offset, entries);
    let offset = usize::try_from(offset).map_err(|_| {
        invalid_archive("zip central directory offset overflows this platform".to_owned())
    })?;
    let start = offset.checked_add(delta).ok_or_else(|| {
        invalid_archive("zip central directory offset overflows this platform".to_owned())
    })?;
    if start > bytes.len() {
        return Err(invalid_archive(format!(
            "zip central directory starts past the end of the archive ({start} > {})",
            bytes.len()
        )));
    }
    Ok(CentralDirectory {
        start,
        entries,
        delta,
    })
}

/// Zip64 extended-information extra field (APPNOTE 4.5.3): the 64-bit values
/// appear in a fixed order, but only for the 32-bit fields that were
/// saturated.
fn apply_zip64_extra(
    extra: &[u8],
    uncompressed: &mut u64,
    compressed: &mut u64,
    local_offset: &mut u64,
) {
    let mut cursor = 0usize;
    while cursor + 4 <= extra.len() {
        let Some(id) = le_u16(extra, cursor) else {
            return;
        };
        let Some(size) = le_u16(extra, cursor + 2).map(usize::from) else {
            return;
        };
        let body_start = cursor + 4;
        let body_end = body_start.saturating_add(size);
        if body_end > extra.len() {
            return;
        }
        if id == ZIP64_EXTRA_FIELD_ID {
            let body = &extra[body_start..body_end];
            let mut read = 0usize;
            for (saturated, target) in [
                (*uncompressed == u64::from(u32::MAX), &mut *uncompressed),
                (*compressed == u64::from(u32::MAX), &mut *compressed),
                (*local_offset == u64::from(u32::MAX), &mut *local_offset),
            ] {
                if !saturated {
                    continue;
                }
                let Some(value) = le_u64(body, read) else {
                    return;
                };
                *target = value;
                read += 8;
            }
            return;
        }
        cursor = body_end;
    }
}

/// Resolve the payload range of a member whose header was already parsed.
fn locate_member_payload(
    bytes: &[u8],
    name: &str,
    local_offset: usize,
    compressed_len: usize,
) -> Result<usize> {
    if le_u32(bytes, local_offset) != Some(LOCAL_HEADER_MAGIC) {
        return Err(invalid_archive(format!(
            "zip member {name} has no local file header at offset {local_offset}"
        )));
    }
    let name_len = usize::from(le_u16(bytes, local_offset + 26).ok_or_else(|| {
        invalid_archive(format!("zip member {name} has a truncated local header"))
    })?);
    let extra_len = usize::from(le_u16(bytes, local_offset + 28).ok_or_else(|| {
        invalid_archive(format!("zip member {name} has a truncated local header"))
    })?);
    let data_start = local_offset
        .checked_add(LOCAL_HEADER_MIN_LEN)
        .and_then(|value| value.checked_add(name_len))
        .and_then(|value| value.checked_add(extra_len))
        .ok_or_else(|| invalid_archive(format!("zip member {name} local header overflows")))?;
    let data_end = data_start
        .checked_add(compressed_len)
        .ok_or_else(|| invalid_archive(format!("zip member {name} payload range overflows")))?;
    if data_end > bytes.len() {
        return Err(NexradError::Truncated {
            what: "zip member payload",
            offset: data_start,
            needed: compressed_len,
            available: bytes.len().saturating_sub(data_start),
        });
    }
    Ok(data_start)
}

/// Walk the central directory (APPNOTE 4.3.12).
fn read_central_directory(bytes: &[u8], eocd: usize) -> Result<Vec<ZipEntry>> {
    let directory = central_directory_location(bytes, eocd)?;
    let mut cursor = directory.start;
    let mut entries = Vec::new();
    while le_u32(bytes, cursor) == Some(CENTRAL_HEADER_MAGIC) {
        let header = bytes
            .get(cursor..cursor + CENTRAL_HEADER_MIN_LEN)
            .ok_or_else(|| invalid_archive("truncated zip central directory header".to_owned()))?;
        let flags = le_u16(header, 8).unwrap_or_default();
        let method = le_u16(header, 10).unwrap_or_default();
        let crc32 = le_u32(header, 16).unwrap_or_default();
        let mut compressed = u64::from(le_u32(header, 20).unwrap_or_default());
        let mut uncompressed = u64::from(le_u32(header, 24).unwrap_or_default());
        let name_len = usize::from(le_u16(header, 28).unwrap_or_default());
        let extra_len = usize::from(le_u16(header, 30).unwrap_or_default());
        let comment_len = usize::from(le_u16(header, 32).unwrap_or_default());
        let mut local_offset = u64::from(le_u32(header, 42).unwrap_or_default());

        let name_start = cursor + CENTRAL_HEADER_MIN_LEN;
        let name_end = name_start + name_len;
        let extra_end = name_end + extra_len;
        let next = extra_end + comment_len;
        if next > bytes.len() {
            return Err(invalid_archive(
                "zip central directory entry runs past the end of the archive".to_owned(),
            ));
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_end]).replace('\\', "/");
        apply_zip64_extra(
            &bytes[name_end..extra_end],
            &mut uncompressed,
            &mut compressed,
            &mut local_offset,
        );

        if flags & FLAG_ENCRYPTED != 0 {
            return Err(invalid_archive(format!("zip member {name} is encrypted")));
        }
        entries.push(build_entry(
            bytes,
            name,
            method,
            crc32,
            compressed,
            uncompressed,
            local_offset,
            directory.delta,
        )?);
        cursor = next;
    }
    // The declared count is advisory. The walk is bounded by the header magic
    // of the records themselves, and writers do disagree with their own
    // directory (an archive edited in place, a count left at a sentinel), so
    // a mismatch is not by itself a reason to refuse real radar data. A
    // directory that yielded nothing at all is a different matter: that means
    // the archive did not lead where it said it would.
    if entries.is_empty() && directory.entries > 0 {
        return Err(invalid_archive(format!(
            "zip central directory declares {} entries but none were readable at offset {}",
            directory.entries, directory.start
        )));
    }
    Ok(entries)
}

/// `delta` is the archive's distance from the start of the file (zero for a
/// flat archive); the recorded local-header offset is relative to the zip
/// data, so it needs the same shift as the directory itself.
#[allow(clippy::too_many_arguments)]
fn build_entry(
    bytes: &[u8],
    name: String,
    method: u16,
    crc32: u32,
    compressed: u64,
    uncompressed: u64,
    local_offset: u64,
    delta: usize,
) -> Result<ZipEntry> {
    let compressed_len = usize::try_from(compressed).map_err(|_| {
        invalid_archive(format!(
            "zip member {name} compressed size overflows this platform"
        ))
    })?;
    let uncompressed_len = usize::try_from(uncompressed)
        .map_err(|_| invalid_archive(format!("zip member {name} size overflows this platform")))?;
    let local_offset = usize::try_from(local_offset)
        .ok()
        .and_then(|offset| offset.checked_add(delta))
        .ok_or_else(|| {
            invalid_archive(format!(
                "zip member {name} local header offset overflows this platform"
            ))
        })?;
    let compressed_offset = locate_member_payload(bytes, &name, local_offset, compressed_len)?;
    Ok(ZipEntry {
        name,
        method,
        crc32,
        compressed_offset,
        compressed_len,
        uncompressed_len,
    })
}

/// Position of the first local file record (APPNOTE 4.3.6), skipping any
/// prepended stub.
///
/// Almost every archive starts with one, so that is checked first; the search
/// is bounded because a stub is a program, not a data file, and a runaway
/// scan over a large non-zip file would cost more than it can ever return.
fn find_first_local_header(bytes: &[u8]) -> Option<usize> {
    /// Comfortably larger than any self-extracting stub in circulation.
    const MAX_STUB_SEARCH: usize = 4 * 1024 * 1024;
    if le_u32(bytes, 0) == Some(LOCAL_HEADER_MAGIC) {
        return Some(0);
    }
    let horizon = bytes.len().min(MAX_STUB_SEARCH);
    bytes
        .get(..horizon)?
        .windows(4)
        .position(|window| window == ZIP_LOCAL_FILE_MAGIC)
}

/// Fallback for archives with no readable end-of-central-directory record:
/// walk local file records from `start` (APPNOTE 4.3.6).
///
/// A member that defers its sizes to a data descriptor (general purpose bit
/// 3) cannot be measured without the central directory, so the walk stops
/// there and returns whatever it has already read.
fn scan_local_file_records(bytes: &[u8], start: usize) -> Result<Vec<ZipEntry>> {
    let mut entries = Vec::new();
    let mut cursor = start;
    while le_u32(bytes, cursor) == Some(LOCAL_HEADER_MAGIC) {
        let header = match bytes.get(cursor..cursor + LOCAL_HEADER_MIN_LEN) {
            Some(header) => header,
            None => break,
        };
        let flags = le_u16(header, 6).unwrap_or_default();
        let method = le_u16(header, 8).unwrap_or_default();
        let crc32 = le_u32(header, 14).unwrap_or_default();
        let compressed = u64::from(le_u32(header, 18).unwrap_or_default());
        let uncompressed = u64::from(le_u32(header, 22).unwrap_or_default());
        let name_len = usize::from(le_u16(header, 26).unwrap_or_default());
        let extra_len = usize::from(le_u16(header, 28).unwrap_or_default());
        let name_start = cursor + LOCAL_HEADER_MIN_LEN;
        let name_end = name_start + name_len;
        let extra_end = name_end + extra_len;
        if extra_end > bytes.len() {
            break;
        }
        if flags & FLAG_ENCRYPTED != 0 {
            let name = String::from_utf8_lossy(&bytes[name_start..name_end]).into_owned();
            return Err(invalid_archive(format!("zip member {name} is encrypted")));
        }
        // Bit 3: sizes live in a trailing data descriptor.
        if flags & (1 << 3) != 0 || (compressed == 0 && uncompressed != 0) {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_end]).replace('\\', "/");
        // `cursor` is already an absolute position, so there is no delta to
        // apply here: the walk never consults a recorded offset.
        let entry = build_entry(
            bytes,
            name,
            method,
            crc32,
            compressed,
            uncompressed,
            cursor as u64,
            0,
        )?;
        cursor = entry.compressed_offset + entry.compressed_len;
        entries.push(entry);
    }
    Ok(entries)
}

/// All member headers in an archive, central directory first.
fn read_zip_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    if let Some(eocd) = find_end_of_central_directory(bytes) {
        return read_central_directory(bytes, eocd);
    }
    match find_first_local_header(bytes) {
        Some(start) => scan_local_file_records(bytes, start),
        None => Err(invalid_archive(
            "not a zip archive: no local file record and no end-of-central-directory".to_owned(),
        )),
    }
}

/// Decompress one member, bounded by [`MAX_MOBILE_MEMBER_BYTES`].
fn read_zip_member(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>> {
    let payload = bytes
        .get(entry.compressed_offset..entry.compressed_offset + entry.compressed_len)
        .ok_or_else(|| {
            invalid_archive(format!(
                "zip member {} payload runs past the end of the archive",
                entry.name
            ))
        })?;
    let data = match entry.method {
        METHOD_STORE => payload.to_vec(),
        METHOD_DEFLATE => {
            let limit = entry.uncompressed_len.min(MAX_MOBILE_MEMBER_BYTES);
            let mut out = Vec::with_capacity(limit.min(1 << 20));
            let mut decoder = DeflateDecoder::new(payload).take(limit as u64 + 1);
            decoder.read_to_end(&mut out).map_err(|source| {
                invalid_archive(format!(
                    "zip member {}: deflate failed: {source}",
                    entry.name
                ))
            })?;
            if out.len() > limit {
                return Err(invalid_archive(format!(
                    "zip member {} expands beyond its declared {} bytes",
                    entry.name, entry.uncompressed_len
                )));
            }
            out
        }
        other => {
            return Err(invalid_archive(format!(
                "zip member {} uses unsupported compression method {other}",
                entry.name
            )));
        }
    };
    if data.len() != entry.uncompressed_len {
        return Err(invalid_archive(format!(
            "zip member {} decoded to {} bytes, expected {}",
            entry.name,
            data.len(),
            entry.uncompressed_len
        )));
    }
    let mut crc = Crc::new();
    crc.update(&data);
    if crc.sum() != entry.crc32 {
        return Err(invalid_archive(format!(
            "zip member {} fails its CRC-32 check",
            entry.name
        )));
    }
    Ok(data)
}

/// Read, sniff, and keep the radar members of an archive held in memory.
fn read_zip_radar_members(bytes: &[u8], label: &str) -> Result<Vec<RadarMember>> {
    let entries = read_zip_entries(bytes)
        .map_err(|err| invalid_archive(format!("{label} is not readable: {err}")))?;
    let mut members = Vec::new();
    let mut budget = MemberBudget::default();
    for entry in entries {
        if entry.name.ends_with('/') {
            continue;
        }
        if !plausible_radar_member_name(&entry.name) {
            continue;
        }
        budget.reserve(&entry.name, entry.uncompressed_len)?;
        let bytes = read_zip_member(bytes, &entry)?;
        if looks_like_radar_member(&bytes) {
            members.push(RadarMember {
                name: entry.name,
                bytes,
            });
        }
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(members)
}

// ---------------------------------------------------------------------------
// Member classification and volume grouping
// ---------------------------------------------------------------------------

/// Names worth opening: `swp.*` sweepfiles and Level II-style members.
fn plausible_radar_member_name(name: &str) -> bool {
    let file_name = name.rsplit('/').next().unwrap_or("");
    if file_name.is_empty() || file_name.starts_with('.') {
        return false;
    }
    if looks_like_dorade_name(file_name) {
        return true;
    }
    Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "msg31" | "ar2v" | "raw" | "gz" | "bz2" | "v06" | "v08"
            )
        })
}

/// Maximum start-time gap between consecutive sweeps of one volume scan.
const MAX_INTRA_VOLUME_GAP_MINUTES: i64 = 15;
/// Fixed angles within this tolerance count as "not ascending".
const FIXED_ANGLE_EPSILON_DEG: f32 = 0.05;

/// A sweep waiting to be grouped: start time + fixed angle drive the
/// ascending-run segmentation, `label` breaks time ties deterministically.
struct GroupableSweep<T> {
    start_time: Option<DateTime<Utc>>,
    fixed_angle_deg: f32,
    label: String,
    payload: T,
}

/// Group one instrument's sweeps (already time-sorted) into volume scans: a
/// scan continues while the fixed angle strictly ascends and sweeps stay
/// within [`MAX_INTRA_VOLUME_GAP_MINUTES`] of each other.
fn segment_volume_runs<T>(mut sweeps: Vec<GroupableSweep<T>>) -> Vec<Vec<GroupableSweep<T>>> {
    sweeps.sort_by(|left, right| {
        left.start_time
            .cmp(&right.start_time)
            .then_with(|| left.label.cmp(&right.label))
    });
    let mut runs: Vec<Vec<GroupableSweep<T>>> = Vec::new();
    for sweep in sweeps {
        let continues_run = runs.last().and_then(|run| run.last()).is_some_and(|last| {
            let ascending = sweep.fixed_angle_deg > last.fixed_angle_deg + FIXED_ANGLE_EPSILON_DEG;
            let close_in_time = match (last.start_time, sweep.start_time) {
                (Some(previous), Some(current)) => {
                    (current - previous).num_minutes() <= MAX_INTRA_VOLUME_GAP_MINUTES
                }
                _ => true,
            };
            ascending && close_in_time
        });
        if continues_run {
            runs.last_mut().expect("run exists").push(sweep);
        } else {
            runs.push(vec![sweep]);
        }
    }
    runs
}

fn decode_members(
    archive_label: Option<&str>,
    members: Vec<RadarMember>,
) -> Result<Vec<MobileVolume>> {
    // Split DORADE sweeps from Level II members, peeking DORADE headers for
    // the grouping metadata.
    let mut per_instrument: BTreeMap<String, Vec<GroupableSweep<RadarMember>>> = BTreeMap::new();
    let mut level2_members: Vec<RadarMember> = Vec::new();
    for member in members {
        if looks_like_dorade_bytes(&member.bytes) {
            let header =
                peek_dorade_sweep(&member.bytes).map_err(|err| with_member(&member.name, err))?;
            per_instrument
                .entry(header.instrument)
                .or_default()
                .push(GroupableSweep {
                    start_time: header.start_time,
                    fixed_angle_deg: header.fixed_angle_deg,
                    label: member.name.clone(),
                    payload: member,
                });
        } else {
            level2_members.push(member);
        }
    }

    let source_path = |member: &str| match archive_label {
        Some(label) => format!("{label}::{member}"),
        None => member.to_owned(),
    };
    let mut volumes: Vec<MobileVolume> = Vec::new();

    let runs: Vec<Vec<GroupableSweep<RadarMember>>> = per_instrument
        .into_values()
        .flat_map(segment_volume_runs)
        .collect();
    let dorade_volumes: Vec<MobileVolume> = runs
        .into_par_iter()
        .map(|run| {
            let mut volume = empty_dorade_volume();
            for sweep in &run {
                append_dorade_sweep(&sweep.payload.bytes, &mut volume)
                    .map_err(|err| with_member(&sweep.payload.name, err))?;
            }
            finalize_dorade_volume(&mut volume);
            let member_label = run[0].payload.name.clone();
            volume.metadata.source_path = Some(source_path(&member_label));
            Ok(MobileVolume {
                volume,
                member_label,
                member_count: run.len(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    volumes.extend(dorade_volumes);

    // `.msg31` twins and other Archive II members go back through the Level
    // II decoder, which is where GR2Analyst's variable message framing is
    // detected: `decode_volume_from_bytes` normalises the member and runs
    // the whole-buffer record loop, and that loop latches onto back-to-back
    // message 31s the moment it meets one before the 134th record. So a
    // deployment archive carrying a variable-framed `.msg31` decodes it in
    // full rather than stopping after the first radial. Pinned by
    // `a_variable_framed_msg31_member_decodes_through_the_framing_latch`.
    let level2_volumes: Vec<(MobileVolume, SweepAssemblyClassification)> = level2_members
        .into_par_iter()
        .map(|member| {
            let mut volume = decode_volume_from_bytes(&member.bytes)
                .map_err(|err| with_member(&member.name, err))?;
            let assembly = classify_archive_sweep(&member.bytes, &volume);
            volume.metadata.source_path = Some(source_path(&member.name));
            Ok((
                MobileVolume {
                    volume,
                    member_label: member.name,
                    member_count: 1,
                },
                assembly,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    volumes.extend(assemble_level2_sweeps(level2_volumes));

    volumes.sort_by(|left, right| {
        left.volume
            .volume_time
            .cmp(&right.volume.volume_time)
            .then_with(|| left.member_label.cmp(&right.member_label))
    });
    Ok(volumes)
}

/// Group decoded Archive II members per radar. Other-radar members may be
/// interleaved in archive-time order, so each radar gets its own ordered walk
/// before the completed volumes are merged back into the global timeline.
fn assemble_level2_sweeps(
    decoded: Vec<(MobileVolume, SweepAssemblyClassification)>,
) -> Vec<MobileVolume> {
    let mut per_radar: BTreeMap<String, Vec<(MobileVolume, SweepAssemblyClassification)>> =
        BTreeMap::new();
    for member in decoded {
        per_radar
            .entry(member.0.volume.site.id.trim().to_ascii_uppercase())
            .or_default()
            .push(member);
    }

    let mut assembled = Vec::new();
    for mut members in per_radar.into_values() {
        members.sort_by(|left, right| {
            left.0
                .volume
                .volume_time
                .cmp(&right.0.volume.volume_time)
                .then_with(|| left.0.member_label.cmp(&right.0.member_label))
        });
        let mut pending: Option<(MobileVolume, Option<ProvenSweepMembership>)> = None;
        for (member, classification) in members {
            let evidence = match classification {
                SweepAssemblyClassification::Proven(evidence) => Some(evidence),
                SweepAssemblyClassification::Refused(_) => None,
            };
            let can_append = pending
                .as_ref()
                .and_then(|(_, current)| current.as_ref())
                .zip(evidence.as_ref())
                .is_some_and(|(current, next)| {
                    decide_adjacent_sweeps(current, next) == SweepAssemblyDecision::ProvenSameVolume
                });
            if can_append {
                let (target, current) = pending.as_mut().expect("checked pending above");
                let current = current.as_mut().expect("checked evidence above");
                let next = evidence.expect("checked evidence above");
                append_proven_sweep(&mut target.volume, current, member.volume, next)
                    .expect("a proven adjacent sweep remains proven while appending");
                target.member_count += member.member_count;
                continue;
            }

            if let Some((complete, _)) = pending.replace((member, evidence)) {
                assembled.push(complete);
            }
        }
        if let Some((complete, _)) = pending {
            assembled.push(complete);
        }
    }
    assembled
}

fn with_member(name: &str, err: NexradError) -> NexradError {
    NexradError::InvalidMessage {
        offset: 0,
        reason: format!("archive member {name}: {err}"),
    }
}

fn invalid_archive(reason: String) -> NexradError {
    NexradError::InvalidMessage { offset: 0, reason }
}

fn file_len_usize(path: &Path) -> Result<usize> {
    let metadata = std::fs::metadata(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    usize::try_from(metadata.len()).map_err(|_| {
        invalid_archive(format!(
            "file {} size overflows this platform",
            path.display()
        ))
    })
}

fn read_file_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let declared = file_len_usize(path)?;
    if declared > limit {
        return Err(invalid_archive(format!(
            "file {} is {declared} bytes (limit {limit})",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|source| NexradError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if bytes.len() > limit {
        return Err(invalid_archive(format!(
            "file {} grew past the {limit}-byte limit while reading",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Descriptor blocks live at the head of a sweepfile; this is enough bytes to
/// peek COMM/SSWB/VOLD/RADD/PARM*/CELV/CSFD/SWIB without reading rays.
const PEEK_HEAD_BYTES: usize = 64 * 1024;

/// Decode the full volume scan a loose sweepfile belongs to.
///
/// Scans the file's directory for sibling `swp.*` files from the same
/// instrument, segments them into ascending fixed-angle runs (see the module
/// docs), and decodes the run containing `path` as one volume. Sibling
/// headers are peeked from the first [`PEEK_HEAD_BYTES`] only, so opening a
/// file in a large deployment directory stays cheap.
pub fn decode_dorade_volume_for_path(path: &Path) -> Result<RadarVolume> {
    let bytes = read_file_limited(path, MAX_MOBILE_MEMBER_BYTES)?;
    let header = peek_dorade_sweep(&bytes)?;

    let mut sweeps: Vec<GroupableSweep<PathBuf>> = vec![GroupableSweep {
        start_time: header.start_time,
        fixed_angle_deg: header.fixed_angle_deg,
        label: path.display().to_string(),
        payload: path.to_path_buf(),
    }];
    if let Some(directory) = path.parent()
        && let Ok(entries) = std::fs::read_dir(directory)
    {
        for entry in entries.flatten() {
            let sibling = entry.path();
            if sibling == *path || !sibling.is_file() {
                continue;
            }
            if !crate::dorade::looks_like_dorade_path(&sibling) {
                continue;
            }
            let Some(head) = read_file_head(&sibling, PEEK_HEAD_BYTES) else {
                continue;
            };
            let Ok(sibling_header) = peek_dorade_sweep(&head) else {
                continue;
            };
            if sibling_header.instrument == header.instrument {
                sweeps.push(GroupableSweep {
                    start_time: sibling_header.start_time,
                    fixed_angle_deg: sibling_header.fixed_angle_deg,
                    label: sibling.display().to_string(),
                    payload: sibling,
                });
            }
        }
    }

    let runs = segment_volume_runs(sweeps);
    let run = runs
        .into_iter()
        .find(|run| run.iter().any(|sweep| sweep.payload == *path))
        .expect("the opened sweep belongs to one run");

    if run.len() == 1 {
        let mut volume = decode_dorade_sweep(&bytes)?;
        volume.metadata.source_path = Some(path.display().to_string());
        return Ok(volume);
    }

    let mut volume = empty_dorade_volume();
    for sweep in &run {
        let data = if sweep.payload == *path {
            bytes.clone()
        } else {
            read_file_limited(&sweep.payload, MAX_MOBILE_MEMBER_BYTES)?
        };
        append_dorade_sweep(&data, &mut volume).map_err(|err| with_member(&sweep.label, err))?;
    }
    finalize_dorade_volume(&mut volume);
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(volume)
}

fn read_file_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut head = vec![0u8; limit];
    let mut filled = 0usize;
    loop {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                if filled == head.len() {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    head.truncate(filled);
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Synthetic sweep start times: a fixed base plus per-sweep offsets.
    const BASE_UNIX: i32 = 1_779_404_114; // 2026-05-21T22:55:14Z

    /// Minimal synthetic big-endian DORADE sweep (mirrors the dorade tests).
    fn synthetic_sweep(instrument: &[u8; 4], start_offset_s: i32, fixed_angle: f32) -> Vec<u8> {
        fn block(id: &[u8; 4], len: usize) -> Vec<u8> {
            let mut bytes = vec![0u8; len];
            bytes[..4].copy_from_slice(id);
            bytes[4..8].copy_from_slice(&(len as i32).to_be_bytes());
            bytes
        }
        let mut bytes = Vec::new();

        let mut sswb = block(b"SSWB", 200);
        sswb[12..16].copy_from_slice(&(BASE_UNIX + start_offset_s).to_be_bytes());
        bytes.extend(sswb);

        let mut vold = block(b"VOLD", 72);
        vold[10..12].copy_from_slice(&7i16.to_be_bytes());
        vold[36..38].copy_from_slice(&2026i16.to_be_bytes());
        vold[38..40].copy_from_slice(&5i16.to_be_bytes());
        vold[40..42].copy_from_slice(&21i16.to_be_bytes());
        bytes.extend(vold);

        let mut radd = block(b"RADD", 144);
        radd[8..12].copy_from_slice(instrument);
        radd[50..52].copy_from_slice(&8i16.to_be_bytes());
        radd[80..84].copy_from_slice(&(-103.29f32).to_bits().to_be_bytes());
        radd[84..88].copy_from_slice(&39.74f32.to_bits().to_be_bytes());
        radd[88..92].copy_from_slice(&1.519f32.to_bits().to_be_bytes());
        bytes.extend(radd);

        let mut parm = block(b"PARM", 216);
        parm[8..11].copy_from_slice(b"DBZ");
        parm[78..80].copy_from_slice(&2i16.to_be_bytes());
        parm[92..96].copy_from_slice(&100.0f32.to_bits().to_be_bytes());
        parm[100..104].copy_from_slice(&(-32768i32).to_be_bytes());
        parm[200..204].copy_from_slice(&2i32.to_be_bytes());
        parm[204..208].copy_from_slice(&50.0f32.to_bits().to_be_bytes());
        parm[208..212].copy_from_slice(&100.0f32.to_bits().to_be_bytes());
        bytes.extend(parm);

        let mut swib = block(b"SWIB", 40);
        swib[16..20].copy_from_slice(&1i32.to_be_bytes());
        swib[32..36].copy_from_slice(&fixed_angle.to_bits().to_be_bytes());
        bytes.extend(swib);

        let mut ryib = block(b"RYIB", 44);
        ryib[24..28].copy_from_slice(&45.0f32.to_bits().to_be_bytes());
        ryib[28..32].copy_from_slice(&fixed_angle.to_bits().to_be_bytes());
        bytes.extend(ryib);

        let mut rdat = block(b"RDAT", 20);
        rdat[8..11].copy_from_slice(b"DBZ");
        rdat[16..18].copy_from_slice(&1000i16.to_be_bytes());
        rdat[18..20].copy_from_slice(&2000i16.to_be_bytes());
        bytes.extend(rdat);

        bytes
    }

    /// A deployment archive's `.msg31` twin is decoded whole, not truncated
    /// at the first radial.
    ///
    /// GR2Analyst writes its exports with the messages packed back to back
    /// instead of padded into 2432-byte records. Fixed-record framing reads
    /// the first radial and then walks off into padding that is not there,
    /// so a member decoded the wrong way comes back with one radial and looks
    /// like a corrupt file rather than a framing mismatch. Archive members go
    /// through `decode_volume_from_bytes`, which owns that detection, and
    /// this is the pin that says so: three radials in, three radials out.
    #[test]
    fn a_variable_framed_msg31_member_decodes_through_the_framing_latch() {
        let export = crate::tests::synthetic_variable_framed_archive(3);
        let archive = build_zip(&[(
            "COW2/nexrad.20260521_225514_COW2_v237_SUR.msg31",
            export.clone(),
            METHOD_STORE,
        )]);

        let volumes = decode_deployment_zip(&archive).expect("archive decodes");
        assert_eq!(volumes.len(), 1, "one Level II member, one volume");
        assert_eq!(
            volumes[0].volume.metadata.decoded_radial_count, 3,
            "all three back-to-back radials should decode, not just the first"
        );
        assert_eq!(volumes[0].volume.site.id, "KTLX");
        assert_eq!(
            volumes[0].member_label,
            "COW2/nexrad.20260521_225514_COW2_v237_SUR.msg31"
        );

        // The same bytes loose on disk decode identically, so the archive
        // path is not a second, weaker reader.
        let loose = crate::decode_volume_from_bytes(&export).expect("loose export decodes");
        assert_eq!(
            volumes[0].volume.metadata.decoded_radial_count, loose.metadata.decoded_radial_count,
            "the archive member and the loose file are the same decode"
        );
    }

    fn one_radial_export(sequence: &[u8; 3], collect_ms: u32) -> Vec<u8> {
        let mut export = crate::tests::synthetic_variable_framed_archive(1);
        export[9..12].copy_from_slice(sequence);
        export[16..20].copy_from_slice(&collect_ms.to_be_bytes());
        // Volume header (24) + control word (12) + message header (16), then
        // collection milliseconds at bytes 4..8 of the Message 31 header.
        export[56..60].copy_from_slice(&collect_ms.to_be_bytes());
        export
    }

    #[test]
    fn msg31_members_group_by_internal_volume_identity() {
        let first = one_radial_export(b"210", 79_691_000);
        let second = one_radial_export(b"210", 79_700_000);
        let next_volume = one_radial_export(b"211", 79_709_000);
        let archive = build_zip(&[
            ("DOW7/first.msg31", first, METHOD_STORE),
            ("DOW7/second.msg31", second, METHOD_STORE),
            ("DOW7/next.msg31", next_volume, METHOD_STORE),
        ]);

        let volumes = decode_deployment_zip(&archive).expect("archive decodes");

        assert_eq!(volumes.len(), 2);
        let assembled = volumes
            .iter()
            .find(|volume| volume.member_count == 2)
            .expect("the internal 210 sequence forms one logical volume");
        assert_eq!(assembled.volume.cuts.len(), 2);
        assert_eq!(assembled.volume.metadata.decoded_radial_count, 2);
        assert_eq!(assembled.member_label, "DOW7/first.msg31");
    }

    #[test]
    fn refused_msg31_member_is_a_hard_group_boundary() {
        let first = one_radial_export(b"210", 79_691_000);
        let refused = one_radial_export(b"x10", 79_700_000);
        let third = one_radial_export(b"210", 79_709_000);
        let archive = build_zip(&[
            ("DOW7/first.msg31", first, METHOD_STORE),
            ("DOW7/refused.msg31", refused, METHOD_STORE),
            ("DOW7/third.msg31", third, METHOD_STORE),
        ]);

        let volumes = decode_deployment_zip(&archive).expect("archive decodes");

        assert_eq!(volumes.len(), 3);
        assert!(
            volumes.iter().all(|volume| volume.member_count == 1),
            "members on either side of refused evidence must not bridge across it"
        );
    }

    /// A pre-2008 `ARCHIVE2` tape inside a deployment zip decodes, exactly
    /// as the same bytes do loose on disk.
    ///
    /// [`crate::sniff_supported_volume_format`] accepts BOTH Archive II tape
    /// identifiers — `AR2V` and the pre-2008 `ARCHIVE2` — so a legacy volume
    /// dropped on the app opens. The archive member sniff used to test only
    /// `AR2V`, which silently dropped a legacy member from a deployment zip:
    /// no error, no skip count, just a volume that was not there. The two
    /// sniffs share [`crate::ARCHIVE_II_MAGICS`] now so they cannot drift.
    #[test]
    fn a_legacy_archive2_member_decodes_inside_a_deployment_zip() {
        let tape = crate::tests::synthetic_legacy_archive();
        assert!(
            tape.starts_with(b"ARCHIVE2"),
            "fixture should be a pre-2008 tape, not an AR2V volume"
        );
        let archive = build_zip(&[("DOW7/KTLX_19940604_120000.raw", tape.clone(), METHOD_STORE)]);

        let volumes = decode_deployment_zip(&archive).expect("archive decodes");
        assert_eq!(volumes.len(), 1, "one legacy member, one volume");
        assert_eq!(volumes[0].member_label, "DOW7/KTLX_19940604_120000.raw");

        // The same bytes loose on disk decode identically, so the archive
        // path is not a second, weaker reader.
        let loose = crate::decode_volume_from_bytes(&tape).expect("loose tape decodes");
        assert_eq!(
            volumes[0].volume.metadata.decoded_radial_count, loose.metadata.decoded_radial_count,
            "the archive member and the loose file are the same decode"
        );
        assert!(
            volumes[0].volume.metadata.decoded_radial_count > 0,
            "a legacy tape should yield radials, not an empty volume"
        );
    }

    /// Minimal zip writer for the tests: no dependency, both methods.
    fn build_zip(members: &[(&str, Vec<u8>, u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, data, method) in members {
            let mut crc = Crc::new();
            crc.update(data);
            let crc32 = crc.sum();
            let payload = match *method {
                METHOD_STORE => data.clone(),
                METHOD_DEFLATE => {
                    let mut encoder = flate2::write::DeflateEncoder::new(
                        Vec::new(),
                        flate2::Compression::default(),
                    );
                    encoder.write_all(data).unwrap();
                    encoder.finish().unwrap()
                }
                other => panic!("unsupported test method {other}"),
            };
            let local_offset = out.len() as u32;
            out.extend_from_slice(&LOCAL_HEADER_MAGIC.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // mod time
            out.extend_from_slice(&0u16.to_le_bytes()); // mod date
            out.extend_from_slice(&crc32.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&payload);

            directory.extend_from_slice(&CENTRAL_HEADER_MAGIC.to_le_bytes());
            directory.extend_from_slice(&20u16.to_le_bytes()); // version made by
            directory.extend_from_slice(&20u16.to_le_bytes()); // version needed
            directory.extend_from_slice(&0u16.to_le_bytes()); // flags
            directory.extend_from_slice(&method.to_le_bytes());
            directory.extend_from_slice(&0u16.to_le_bytes());
            directory.extend_from_slice(&0u16.to_le_bytes());
            directory.extend_from_slice(&crc32.to_le_bytes());
            directory.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(data.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
            directory.extend_from_slice(&0u16.to_le_bytes()); // extra
            directory.extend_from_slice(&0u16.to_le_bytes()); // comment
            directory.extend_from_slice(&0u16.to_le_bytes()); // disk
            directory.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            directory.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            directory.extend_from_slice(&local_offset.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }
        let directory_offset = out.len() as u32;
        let directory_len = directory.len() as u32;
        out.extend_from_slice(&directory);
        out.extend_from_slice(&EOCD_MAGIC.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with CD
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&directory_len.to_le_bytes());
        out.extend_from_slice(&directory_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    fn deployment_members() -> Vec<(&'static str, Vec<u8>, u16)> {
        vec![
            // One ascending 0.5°→1.0° run split across tilt member
            // directories, then a new run, then a second radar, plus chaff.
            (
                "Tilt 0.5/swp.1260521225514.TST1.0.0.5_SUR_v7",
                synthetic_sweep(b"TST1", 0, 0.5),
                METHOD_DEFLATE,
            ),
            (
                "Tilt 1.0/swp.1260521225520.TST1.0.1.0_SUR_v7",
                synthetic_sweep(b"TST1", 6, 1.0),
                METHOD_STORE,
            ),
            (
                "Tilt 0.5/swp.1260521225600.TST1.0.0.5_SUR_v8",
                synthetic_sweep(b"TST1", 46, 0.5),
                METHOD_DEFLATE,
            ),
            (
                "OTHER/swp.1260521225514.TST2.0.0.5_SUR_v7",
                synthetic_sweep(b"TST2", 0, 0.5),
                METHOD_STORE,
            ),
            ("README.txt", b"not radar data".to_vec(), METHOD_DEFLATE),
        ]
    }

    #[test]
    fn groups_zip_members_into_ascending_elevation_runs_per_instrument() {
        let archive = build_zip(&deployment_members());
        let volumes = decode_deployment_zip(&archive).unwrap();

        assert_eq!(volumes.len(), 3);
        let two_cut = volumes
            .iter()
            .find(|entry| entry.volume.site.id == "TST1" && entry.member_count == 2)
            .expect("two-cut TST1 volume");
        assert_eq!(two_cut.volume.cuts.len(), 2);
        assert!(two_cut.volume.cuts[0].elevation_deg < two_cut.volume.cuts[1].elevation_deg);
        assert!(
            volumes
                .iter()
                .any(|entry| entry.volume.site.id == "TST1" && entry.member_count == 1)
        );
        assert!(volumes.iter().any(|entry| entry.volume.site.id == "TST2"));
    }

    #[test]
    fn reads_stored_and_deflated_members_with_identical_bytes() {
        let sweep = synthetic_sweep(b"TST1", 0, 0.5);
        let archive = build_zip(&[
            ("swp.stored.TST1", sweep.clone(), METHOD_STORE),
            ("swp.deflated.TST1", sweep.clone(), METHOD_DEFLATE),
        ]);
        let entries = read_zip_entries(&archive).unwrap();
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_eq!(read_zip_member(&archive, entry).unwrap(), sweep);
        }
    }

    #[test]
    fn corrupt_member_bytes_fail_the_crc_check() {
        let sweep = synthetic_sweep(b"TST1", 0, 0.5);
        let mut archive = build_zip(&[("swp.stored.TST1", sweep, METHOD_STORE)]);
        // Flip a byte inside the stored payload.
        let payload = LOCAL_HEADER_MIN_LEN + "swp.stored.TST1".len() + 32;
        archive[payload] ^= 0xff;
        let entries = read_zip_entries(&archive).unwrap();
        let err = read_zip_member(&archive, &entries[0]).unwrap_err();
        assert!(err.to_string().contains("CRC-32"), "{err}");
    }

    #[test]
    fn archive_without_a_central_directory_falls_back_to_local_records() {
        let archive = build_zip(&deployment_members());
        let eocd = find_end_of_central_directory(&archive).expect("eocd");
        // Drop the central directory and the EOCD, as a truncated download
        // or a streamed archive would.
        let directory_offset =
            u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        let streamed = archive[..directory_offset].to_vec();
        assert!(find_end_of_central_directory(&streamed).is_none());

        let entries = scan_local_file_records(&streamed, 0).unwrap();
        assert_eq!(entries.len(), 5);
        let volumes = decode_deployment_zip(&streamed).unwrap();
        assert_eq!(volumes.len(), 3);
    }

    /// The recovered archive delta is confirmed against the directory's own
    /// header magic before it is used, so a writer that miscounts its central
    /// directory does not send the reader off to a phantom offset.
    #[test]
    fn an_unconfirmable_archive_delta_falls_back_to_a_flat_archive() {
        let archive = build_zip(&deployment_members());
        let eocd = find_end_of_central_directory(&archive).expect("eocd");
        let directory_len = u32::from_le_bytes(archive[eocd + 12..eocd + 16].try_into().unwrap());

        let mut damaged = archive.clone();
        // Understate the directory by 8 bytes: the subtraction now resolves
        // to a delta of 8, which points into the middle of a record.
        damaged[eocd + 12..eocd + 16].copy_from_slice(&(directory_len - 8).to_le_bytes());
        let directory = central_directory_location(&damaged, eocd).expect("location");
        assert_eq!(directory.delta, 0);
        assert_eq!(
            read_zip_entries(&damaged).unwrap().len(),
            read_zip_entries(&archive).unwrap().len()
        );
    }

    /// Counts are advisory, but a directory that leads nowhere is not: it
    /// must fail loudly rather than report an archive with no radar in it.
    #[test]
    fn a_central_directory_that_yields_nothing_is_still_an_error() {
        let archive = build_zip(&deployment_members());
        let eocd = find_end_of_central_directory(&archive).expect("eocd");
        let directory_offset =
            u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap());
        let mut damaged = archive.clone();
        // One byte past the real directory: no delta resolves, and the walk
        // lands in the middle of the first record's signature.
        damaged[eocd + 16..eocd + 20].copy_from_slice(&(directory_offset + 1).to_le_bytes());
        let err = read_zip_entries(&damaged).unwrap_err().to_string();
        assert!(err.contains("none were readable"), "{err}");
    }

    #[test]
    fn encrypted_members_are_rejected_rather_than_decoded() {
        let sweep = synthetic_sweep(b"TST1", 0, 0.5);
        let mut archive = build_zip(&[("swp.secret.TST1", sweep, METHOD_STORE)]);
        // Set general purpose bit 0 in both the local and central headers.
        archive[6] |= 1;
        let eocd = find_end_of_central_directory(&archive).unwrap();
        let directory =
            u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        archive[directory + 8] |= 1;
        let err = read_zip_entries(&archive).unwrap_err();
        assert!(err.to_string().contains("encrypted"), "{err}");
    }

    #[test]
    fn unsupported_compression_methods_are_named() {
        let sweep = synthetic_sweep(b"TST1", 0, 0.5);
        let mut archive = build_zip(&[("swp.bzip.TST1", sweep, METHOD_STORE)]);
        // Rewrite the method to bzip2 (12) in both headers.
        archive[8..10].copy_from_slice(&12u16.to_le_bytes());
        let eocd = find_end_of_central_directory(&archive).unwrap();
        let directory =
            u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        archive[directory + 10..directory + 12].copy_from_slice(&12u16.to_le_bytes());
        let entries = read_zip_entries(&archive).unwrap();
        let err = read_zip_member(&archive, &entries[0]).unwrap_err();
        assert!(err.to_string().contains("method 12"), "{err}");
    }

    #[test]
    fn same_elevation_sequences_become_one_volume_per_sweep() {
        // Single-tilt surveillance: 1.0°, 1.0°, 1.0° must NOT merge.
        let runs = segment_volume_runs(vec![
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX), 0),
                fixed_angle_deg: 1.0,
                label: "a".into(),
                payload: (),
            },
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX) + 12, 0),
                fixed_angle_deg: 1.0,
                label: "b".into(),
                payload: (),
            },
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX) + 24, 0),
                fixed_angle_deg: 1.0,
                label: "c".into(),
                payload: (),
            },
        ]);
        assert_eq!(runs.len(), 3);
    }

    #[test]
    fn long_time_gap_splits_an_ascending_run() {
        let runs = segment_volume_runs(vec![
            GroupableSweep {
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX), 0),
                fixed_angle_deg: 0.5,
                label: "a".into(),
                payload: (),
            },
            GroupableSweep {
                // Ascending but an hour later: deployment pause.
                start_time: DateTime::<Utc>::from_timestamp(i64::from(BASE_UNIX) + 3600, 0),
                fixed_angle_deg: 1.0,
                label: "b".into(),
                payload: (),
            },
        ]);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn rejects_archive_without_radar_members() {
        let archive = build_zip(&[("README.txt", b"nothing here".to_vec(), METHOD_STORE)]);
        let err = decode_deployment_zip(&archive).unwrap_err();
        assert!(err.to_string().contains("no radar members"), "{err}");
    }

    #[test]
    fn loose_sweepfile_groups_directory_siblings_from_same_run() {
        let dir = std::env::temp_dir().join("radar_workstation_mobile_loose_test");
        std::fs::create_dir_all(&dir).unwrap();
        // Ascending same-instrument run → grouped; the next run → excluded.
        let low = dir.join("swp.1260521225514.TST1.0.0.5_SUR_v7");
        let high = dir.join("swp.1260521225520.TST1.0.1.0_SUR_v7");
        let other = dir.join("swp.1260521225600.TST1.0.0.5_SUR_v8");
        std::fs::write(&low, synthetic_sweep(b"TST1", 0, 0.5)).unwrap();
        std::fs::write(&high, synthetic_sweep(b"TST1", 6, 1.0)).unwrap();
        std::fs::write(&other, synthetic_sweep(b"TST1", 46, 0.5)).unwrap();

        let volume = decode_dorade_volume_for_path(&low).unwrap();

        assert_eq!(volume.site.id, "TST1");
        assert_eq!(volume.cuts.len(), 2);
        for path in [&low, &high, &other] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn deployment_folder_decodes_like_an_archive() {
        let dir = std::env::temp_dir().join("radar_workstation_mobile_dir_test");
        let tilt_low = dir.join("Tilt 0.5");
        let tilt_high = dir.join("Tilt 1.0");
        std::fs::create_dir_all(&tilt_low).unwrap();
        std::fs::create_dir_all(&tilt_high).unwrap();
        std::fs::write(
            tilt_low.join("swp.1260521225514.TST1.0.0.5_SUR_v7"),
            synthetic_sweep(b"TST1", 0, 0.5),
        )
        .unwrap();
        std::fs::write(
            tilt_high.join("swp.1260521225520.TST1.0.1.0_SUR_v7"),
            synthetic_sweep(b"TST1", 6, 1.0),
        )
        .unwrap();

        let volumes = decode_mobile_dir_from_path(&dir).unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].volume.cuts.len(), 2);
        assert_eq!(volumes[0].member_count, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zip_sniffers_match_magic_and_extension() {
        assert!(looks_like_zip_bytes(b"PK\x03\x04rest"));
        assert!(!looks_like_zip_bytes(b"PK\x05\x06"));
        assert!(looks_like_zip_path(Path::new("c:/data/deploy.ZIP")));
        assert!(!looks_like_zip_path(Path::new("c:/data/deploy.tar")));
    }

    #[test]
    fn member_name_prefilter_accepts_observed_layouts() {
        assert!(plausible_radar_member_name(
            "DORADE/COW2/swp.1260516225229.COW2.515.1.0_SUR_v237"
        ));
        assert!(plausible_radar_member_name(
            "GR2 MSG31/COW2/nexrad.20260516_225229_COW2_v237_SUR.msg31"
        ));
        assert!(!plausible_radar_member_name("GR2 - README.txt"));
        assert!(!plausible_radar_member_name("DORADE/COW2/"));
    }
}
