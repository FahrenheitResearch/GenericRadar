//! Deployment-archive tests driven by real sweepfile bytes.
//!
//! The two fixtures under `tests/data/` are real DORADE sweepfiles (see
//! `dorade_real.rs` for their provenance). Here they are packed into zip
//! archives — stored and deflated, with and without a central directory — and
//! read back through the crate's own zip reader, so the archive layer is
//! proven on the same bytes the decoder is.

use std::io::Write;

use flate2::Crc;
use nexrad_io::mobile_archive::{
    MobileVolume, decode_deployment_zip, decode_deployment_zip_from_path, looks_like_zip_bytes,
};

const COW2_NAME: &str = "DORADE/COW2/swp.1260521225514.COW2.229.1.0_SUR_v215";
const NOXP_NAME: &str = "DORADE/NOXP/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1";
const COW2: &[u8] = include_bytes!("data/swp.1260521225514.COW2.229.1.0_SUR_v215.head24");
const NOXP: &[u8] = include_bytes!("data/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3");

const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// Minimal zip writer: enough of APPNOTE 6.3.10 to produce archives a real
/// unzip accepts, with no zip dependency in the test either.
fn build_zip(members: &[(&str, &[u8], u16)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for (name, data, method) in members {
        let mut crc = Crc::new();
        crc.update(data);
        let crc32 = crc.sum();
        let payload = match *method {
            METHOD_STORE => data.to_vec(),
            METHOD_DEFLATE => {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(data).unwrap();
                encoder.finish().unwrap()
            }
            other => panic!("unsupported test method {other}"),
        };
        let local_offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc32.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&payload);

        directory.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        directory.extend_from_slice(&20u16.to_le_bytes());
        directory.extend_from_slice(&20u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&method.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&crc32.to_le_bytes());
        directory.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        directory.extend_from_slice(&(data.len() as u32).to_le_bytes());
        directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&local_offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let directory_offset = out.len() as u32;
    let directory_len = directory.len() as u32;
    out.extend_from_slice(&directory);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&directory_len.to_le_bytes());
    out.extend_from_slice(&directory_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn real_deployment_zip() -> Vec<u8> {
    build_zip(&[
        (COW2_NAME, COW2, METHOD_DEFLATE),
        (NOXP_NAME, NOXP, METHOD_STORE),
        ("GR2 - README.txt", b"deployment notes", METHOD_DEFLATE),
    ])
}

fn find<'a>(volumes: &'a [MobileVolume], site: &str) -> &'a MobileVolume {
    volumes
        .iter()
        .find(|entry| entry.volume.site.id == site)
        .unwrap_or_else(|| panic!("no volume for {site}"))
}

#[test]
fn real_two_radar_deployment_zip_decodes_both_instruments() {
    let archive = real_deployment_zip();
    assert!(looks_like_zip_bytes(&archive));

    let volumes = decode_deployment_zip(&archive).expect("decode deployment zip");
    assert_eq!(volumes.len(), 2, "one volume per instrument");
    // Sorted by scan time: the 2009 NOXP sweep precedes the 2026 COW2 sweep.
    assert_eq!(volumes[0].volume.site.id, "NOXPRVP");
    assert_eq!(volumes[1].volume.site.id, "COW2");

    let cow2 = find(&volumes, "COW2");
    assert_eq!(cow2.member_count, 1);
    assert_eq!(cow2.member_label, COW2_NAME);
    assert_eq!(cow2.volume.cuts.len(), 1);
    assert_eq!(cow2.volume.cuts[0].radials.len(), 21);
    assert_eq!(cow2.volume.metadata.source_path.as_deref(), Some(COW2_NAME));

    let noxp = find(&volumes, "NOXPRVP");
    assert_eq!(noxp.volume.cuts.len(), 1);
    assert_eq!(noxp.volume.cuts[0].radials.len(), 3);
    assert_eq!(noxp.volume.cuts[0].moments.len(), 8);
}

/// A deflated member and a stored member must produce identical volumes: the
/// zip layer must not touch the radar bytes.
#[test]
fn stored_and_deflated_members_decode_identically() {
    let stored = decode_deployment_zip(&build_zip(&[(COW2_NAME, COW2, METHOD_STORE)])).unwrap();
    let deflated = decode_deployment_zip(&build_zip(&[(COW2_NAME, COW2, METHOD_DEFLATE)])).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(deflated.len(), 1);
    assert_eq!(stored[0].volume, deflated[0].volume);
}

/// Decoding a member out of the archive must match decoding the same bytes
/// directly, apart from the recorded provenance.
#[test]
fn archive_member_matches_a_direct_sweep_decode() {
    let volumes = decode_deployment_zip(&build_zip(&[(NOXP_NAME, NOXP, METHOD_DEFLATE)])).unwrap();
    let mut from_archive = volumes[0].volume.clone();
    let direct = nexrad_io::dorade::decode_dorade_sweep(NOXP).unwrap();
    from_archive.metadata.source_path = direct.metadata.source_path.clone();
    assert_eq!(from_archive, direct);
}

/// The same archive with its central directory removed — a truncated download
/// or a streamed upload — still reads through the local-file-record fallback.
#[test]
fn streamed_archive_without_a_central_directory_still_decodes() {
    let archive = real_deployment_zip();
    let eocd = archive
        .windows(4)
        .rposition(|window| window == 0x0605_4b50u32.to_le_bytes())
        .expect("eocd");
    let directory_offset =
        u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let streamed = &archive[..directory_offset];

    let full = decode_deployment_zip(&archive).unwrap();
    let partial = decode_deployment_zip(streamed).unwrap();
    assert_eq!(full.len(), partial.len());
    for (left, right) in full.iter().zip(&partial) {
        assert_eq!(left.volume, right.volume);
        assert_eq!(left.member_label, right.member_label);
    }
}

/// A single flipped byte inside a real member must fail the archive, not
/// decode into plausible-looking radials.
#[test]
fn a_corrupted_real_member_fails_the_archive() {
    let mut archive = build_zip(&[(NOXP_NAME, NOXP, METHOD_STORE)]);
    // 30-byte local header + name, then well inside the RADD block.
    let payload_start = 30 + NOXP_NAME.len();
    archive[payload_start + 800] ^= 0x5a;
    let err = decode_deployment_zip(&archive).unwrap_err();
    assert!(err.to_string().contains("CRC-32"), "{err}");
}

#[test]
fn deployment_zip_reads_the_same_from_a_file_as_from_memory() {
    let archive = real_deployment_zip();
    let path = std::env::temp_dir().join("radar_workstation_real_deployment.zip");
    std::fs::write(&path, &archive).unwrap();

    let from_memory = decode_deployment_zip(&archive).unwrap();
    let from_file = decode_deployment_zip_from_path(&path).unwrap();
    assert_eq!(from_memory.len(), from_file.len());
    for (left, right) in from_memory.iter().zip(&from_file) {
        assert_eq!(left.member_label, right.member_label);
        assert_eq!(left.volume.site, right.volume.site);
        assert_eq!(left.volume.cuts, right.volume.cuts);
        // Only the provenance differs: the file variant records the archive.
        assert!(
            right
                .volume
                .metadata
                .source_path
                .as_deref()
                .is_some_and(|value| value.contains("::"))
        );
    }
    std::fs::remove_file(&path).ok();
}

// ---------------------------------------------------------------------------
// Containers written by an independent zip writer
//
// Everything above packs the real sweepfiles with `build_zip`, a writer that
// lives in this file — so reader and writer could share a misreading of
// APPNOTE and the tests would still pass. The fixture below is a container
// this project did not write: it was produced by CPython 3.11's `zipfile`
// module and carries the same two real sweepfiles (deflated) plus a stored
// text member.
// ---------------------------------------------------------------------------

/// Deployment archive written by CPython 3.11 `zipfile`, holding the two real
/// sweepfile fixtures under the member names `build_zip` uses above.
///
/// It really is a zip; the `.bin` suffix only keeps it clear of the
/// repository's `*.zip` ignore rule. Rename it to open it in an unzip tool.
const PYTHON_ZIP: &[u8] = include_bytes!("data/deployment_python_zipfile.zip.bin");

/// A self-extracting stub, or any installer payload, pushes the whole archive
/// down the file. Offsets recorded inside a zip are relative to the start of
/// the zip data, not to the start of the file, so the reader has to recover
/// the delta. Sizes span one byte — the smallest possible shift — to 64 KiB.
const STUB_SIZES: [usize; 6] = [1, 2, 16, 100, 1024, 65536];

fn with_prepended_stub(archive: &[u8], stub_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(stub_len + archive.len());
    out.extend_from_slice(b"MZ");
    while out.len() < stub_len {
        out.push(b'#');
    }
    out.truncate(stub_len);
    out.extend_from_slice(archive);
    out
}

/// Member label, site and total radial count per volume: enough to catch a
/// member being dropped, reordered, or decoded from the wrong offset.
fn labelled(volumes: &[MobileVolume]) -> Vec<(String, String, usize)> {
    volumes
        .iter()
        .map(|entry| {
            (
                entry.member_label.clone(),
                entry.volume.site.id.clone(),
                entry.volume.cuts.iter().map(|cut| cut.radials.len()).sum(),
            )
        })
        .collect()
}

fn find_eocd(archive: &[u8]) -> usize {
    archive
        .windows(4)
        .rposition(|window| window == 0x0605_4b50u32.to_le_bytes())
        .expect("eocd")
}

/// The third-party container must decode to exactly what our own writer's
/// container does, member for member and radial for radial.
#[test]
fn archive_written_by_an_independent_writer_decodes_identically() {
    let ours = decode_deployment_zip(&real_deployment_zip()).expect("our own writer");
    let theirs = decode_deployment_zip(PYTHON_ZIP).expect("python zipfile writer");

    assert_eq!(theirs.len(), 2);
    assert_eq!(labelled(&ours), labelled(&theirs));
    for (left, right) in ours.iter().zip(&theirs) {
        assert_eq!(left.volume, right.volume);
    }
}

/// Prepending a stub must not change a single decoded value. Before the
/// archive-offset delta was recovered, every one of these sizes — one byte
/// included — failed with "declares 2 entries but 0 were readable".
#[test]
fn real_archive_behind_a_prepended_stub_decodes_identically() {
    let flat = decode_deployment_zip(PYTHON_ZIP).expect("flat archive");
    for stub_len in STUB_SIZES {
        let stubbed = with_prepended_stub(PYTHON_ZIP, stub_len);
        assert!(!looks_like_zip_bytes(&stubbed));
        let volumes = decode_deployment_zip(&stubbed)
            .unwrap_or_else(|err| panic!("{stub_len}-byte stub: {err}"));
        assert_eq!(labelled(&flat), labelled(&volumes), "{stub_len}-byte stub");
        for (left, right) in flat.iter().zip(&volumes) {
            assert_eq!(left.volume, right.volume, "{stub_len}-byte stub");
        }
    }
}

/// A stub in front of a *streamed* archive (no central directory) has to be
/// skipped too: the local-record walk starts at the first local file header,
/// not at byte zero.
#[test]
fn streamed_archive_behind_a_prepended_stub_still_decodes() {
    let archive = real_deployment_zip();
    let eocd = find_eocd(&archive);
    let directory_offset =
        u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let streamed = &archive[..directory_offset];

    let flat = decode_deployment_zip(streamed).expect("streamed archive");
    for stub_len in STUB_SIZES {
        let stubbed = with_prepended_stub(streamed, stub_len);
        let volumes = decode_deployment_zip(&stubbed)
            .unwrap_or_else(|err| panic!("{stub_len}-byte stub: {err}"));
        assert_eq!(labelled(&flat), labelled(&volumes), "{stub_len}-byte stub");
    }
}

/// Rewrite a classic end-of-central-directory record as a Zip64 one: the
/// 32-bit fields go to their sentinels and the real values move into a Zip64
/// EOCD record plus locator (APPNOTE 4.3.14/4.3.15). The member data and the
/// central directory stay exactly as the third-party writer laid them out.
fn to_zip64(archive: &[u8]) -> Vec<u8> {
    let eocd = find_eocd(archive);
    let entries = u64::from(u16::from_le_bytes(
        archive[eocd + 10..eocd + 12].try_into().unwrap(),
    ));
    let directory_len = u64::from(u32::from_le_bytes(
        archive[eocd + 12..eocd + 16].try_into().unwrap(),
    ));
    let directory_offset = u64::from(u32::from_le_bytes(
        archive[eocd + 16..eocd + 20].try_into().unwrap(),
    ));

    let mut out = archive[..eocd].to_vec();
    let zip64_eocd = out.len() as u64;
    out.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
    out.extend_from_slice(&44u64.to_le_bytes());
    out.extend_from_slice(&45u16.to_le_bytes());
    out.extend_from_slice(&45u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&directory_len.to_le_bytes());
    out.extend_from_slice(&directory_offset.to_le_bytes());

    out.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&zip64_eocd.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());

    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&u16::MAX.to_le_bytes());
    out.extend_from_slice(&u16::MAX.to_le_bytes());
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Zip64 archives read the same flat or behind a stub. Behind a stub the
/// Zip64 EOCD record has itself moved, so the locator's offset needs the same
/// delta as every other recorded offset.
#[test]
fn zip64_archive_decodes_flat_and_behind_a_stub() {
    let flat = decode_deployment_zip(PYTHON_ZIP).expect("flat archive");
    let zip64 = to_zip64(PYTHON_ZIP);
    let from_zip64 = decode_deployment_zip(&zip64).expect("zip64 archive");
    assert_eq!(labelled(&flat), labelled(&from_zip64));

    for stub_len in STUB_SIZES {
        let stubbed = with_prepended_stub(&zip64, stub_len);
        let volumes = decode_deployment_zip(&stubbed)
            .unwrap_or_else(|err| panic!("zip64 behind a {stub_len}-byte stub: {err}"));
        assert_eq!(labelled(&flat), labelled(&volumes), "{stub_len}-byte stub");
    }
}

/// The EOCD entry count is advisory: an archive whose count disagrees with
/// its own central directory still reads, in full. The walk is bounded by the
/// records' own signatures rather than by the count, which is what keeps a
/// count of 1 in front of three members from quietly hiding two of them.
#[test]
fn a_wrong_entry_count_in_the_eocd_does_not_reject_the_archive() {
    let expected = labelled(&decode_deployment_zip(PYTHON_ZIP).expect("flat archive"));
    let eocd = find_eocd(PYTHON_ZIP);

    for declared in [0u16, 1, 2, 9999] {
        let mut archive = PYTHON_ZIP.to_vec();
        archive[eocd + 8..eocd + 10].copy_from_slice(&declared.to_le_bytes());
        archive[eocd + 10..eocd + 12].copy_from_slice(&declared.to_le_bytes());
        let volumes = decode_deployment_zip(&archive)
            .unwrap_or_else(|err| panic!("declared count {declared}: {err}"));
        assert_eq!(expected, labelled(&volumes), "declared count {declared}");
    }
}

/// Documented divergence: two members under one name are two sweeps here.
/// The reader this was ported from indexes members by name and keeps one of
/// them — checked against it on a real archive holding the same sweepfile
/// twice: one volume there, two here. Both records really are in the archive,
/// and a deployment that recorded the same sweep twice should show both
/// frames.
#[test]
fn duplicate_member_names_are_both_decoded() {
    let archive = build_zip(&[
        ("DORADE/NOXP/swp.dup", NOXP, METHOD_DEFLATE),
        ("DORADE/NOXP/swp.dup", NOXP, METHOD_STORE),
    ]);
    let volumes = decode_deployment_zip(&archive).expect("duplicate names");
    assert_eq!(volumes.len(), 2);
    assert_eq!(volumes[0].volume, volumes[1].volume);
}
