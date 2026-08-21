//! Every redistributed test fixture is credited where a reader will find it.
//!
//! `crates/nexrad_io/tests/data` carries about 2.19 MB of somebody else's real
//! radar files and `crates/basemap_tiles/tests/data` two real map tiles, and
//! the release process strips neither directory: those bytes are published
//! with the application. Four of them are used under CC BY 4.0, whose whole
//! ask is attribution accompanying the redistribution.
//!
//! They were credited only in the doc comments of the test modules that read
//! them. That is the right place for provenance a decoder maintainer needs -
//! which writer, which HDF5 dialect, which quirk - and the wrong place for a
//! licence condition, because nobody checking whether an application credits
//! its data goes reading `#[cfg(test)]` headers.
//!
//! The credit therefore lives in [`DATA-SOURCES.md`] at the repository root,
//! and this test is what keeps the two from drifting. That file was chosen
//! because it is one of the few that genuinely reaches a reader of the
//! released software: the release snapshot replaces both READMEs, so a credit
//! written in a README is a credit nobody is ever shown. `DATA-SOURCES.md`
//! ships verbatim.
//!
//! What this actually pins: every file present in either fixture directory is
//! accounted for, and every party the table names is present in the
//! attribution file. A new fixture therefore fails this test until somebody
//! decides who it belongs to - which is the point. A fixture that needs no
//! credit still has to be listed here, as [`NO_ATTRIBUTION_NEEDED`], so the
//! decision is recorded rather than inferred from silence.
//!
//! [`DATA-SOURCES.md`]: ../../../DATA-SOURCES.md

use std::path::{Path, PathBuf};

/// The directories whose contents ship with the application, relative to the
/// repository root. Both are scanned; a fixture in either one owes a decision.
const FIXTURE_DIRECTORIES: &[&str] = &[
    "crates/nexrad_io/tests/data",
    "crates/basemap_tiles/tests/data",
];

/// Fixtures this repository wrote itself, or that carry no third party's data.
/// Listed rather than pattern-matched so each one is a decision on the record.
const NO_ATTRIBUTION_NEEDED: &[&str] = &[
    // Written by `gen_odim_fixture.py` / `gen_odim_declared_only.py`, which
    // are themselves in this directory: synthetic ODIM volumes that make one
    // decode rule fail one test.
    "odim_pvol_synth.h5",
    "odim_pvol_declared_only.h5",
    "gen_odim_fixture.py",
    "gen_odim_declared_only.py",
    "ref_odim.py",
    // A zip container written by CPython's `zipfile` around fixtures that are
    // themselves credited below; the container is not anybody's data.
    "deployment_python_zipfile.zip.bin",
    // Written by `gen_cfradial_unwritten_storage.py` and
    // `gen_cfradial_nc4_compact.py`, both in this directory: files whose
    // storage is deliberately left unallocated, or whose links are compact,
    // so the netCDF-4 reader is made to answer for cases no published file
    // exercises. No instrument recorded them.
    "cfrad.unwritten_storage.netcdf4.nc",
    "cfrad.unwritten_storage.classic.nc",
    "cfrad.tiny_compact_links.netcdf4.nc",
    "gen_cfradial_unwritten_storage.py",
    "gen_cfradial_nc4_compact.py",
];

/// Fixture file name → the phrases that must appear in the shipped
/// attribution file.
///
/// The phrases are the ones a reader would search for: the organisation that
/// collected the data, and - where a licence names a condition - the licence.
/// One entry names no organisation on purpose; see the comment on it.
const ATTRIBUTION: &[(&str, &[&str])] = &[
    (
        "bejab.pvol.hdf",
        &["RMI Belgium", "Royal Meteorological Institute of Belgium"],
    ),
    (
        "20130429043000.rad.bewid.pvol.dbzh.scan1.hdf",
        &["RMI Belgium", "wradlib-data"],
    ),
    (
        "T_PAGZ35_C_ENMI_20170421090837.hdf",
        &["met.no", "Norwegian Meteorological Institute"],
    ),
    (
        "espdg.pvol.20260707.dbzh_vradh.h5",
        &["EUMETNET OPERA / AEMET", "CC BY 4.0"],
    ),
    (
        "seang.scan.20260820.dbzh_th_vradh.h5",
        &["EUMETNET OPERA / SMHI", "CC BY 4.0"],
    ),
    (
        "swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3",
        &["NOXP", "CC BY 4.0", "10.5281/zenodo.14194361"],
    ),
    // Deliberately not a credit. Nothing in this repository records who
    // collected this sweep or whether they permit redistribution; the only
    // basis for the "CSWR" that used to be printed here was an unsourced doc
    // comment. Pinning the honest phrase keeps the open question visible
    // instead of letting an expanded acronym harden into a credited body.
    (
        "swp.1260521225514.COW2.229.1.0_SUR_v215.head24",
        &[
            "Operator and terms not established",
            "swp.1260521225514.COW2.229.1.0_SUR_v215.head24",
        ],
    ),
    (
        "cfrad.xsapr_sgp_ppi_20110520.classic.nc",
        &["ARM", "BSD-3-Clause"],
    ),
    // The same sweep in a netCDF-4 container, which is the form most CfRadial
    // files in the wild take; byte-identical to Py-ART's own copy.
    (
        "cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc",
        &["ARM", "BSD-3-Clause"],
    ),
    (
        "KDVN20260819_192802_V06.rec0_1_7_79",
        &["NOAA/NWS", "NEXRAD Level II"],
    ),
    // Three excerpts of KOUN time-series records: two truncated headers and a
    // 32-pulse cut re-encoded into this crate's interchange format. The
    // re-encoding is ours; the pulses are NSSL's.
    (
        "KOUN_RVP.20130520.194601.730.Ascope_DEFAULT.0.H+V.250.head24",
        &["NSSL", "Freely available"],
    ),
    (
        "KOUN_RVP.20130520.224139.456.Ascope_DEFAULT.0.H+V.150.head8",
        &["NSSL", "Freely available"],
    ),
    (
        "koun_20130520_194601.iq.rain_shaft.iqd",
        &["NSSL", "Freely available"],
    ),
    (
        "usgs-imagery-9-117-202.jpg",
        &["USGS The National Map", "public domain"],
    ),
    (
        "osm-9-117-202.png",
        &["OpenStreetMap contributors", "Open Database License"],
    ),
];

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("nexrad_io lives two levels under the repository root")
        .to_path_buf()
}

/// The attribution that ships with the application.
///
/// Neither README is it. The release process replaces `README.md` and
/// `crates/workstation_app/README.md` with the published copies in
/// `docs/release/`, so anything written in the dev versions never reaches a
/// reader. This file is copied into the snapshot untouched.
fn shipped_attribution() -> (PathBuf, String) {
    let path = repository_root().join("DATA-SOURCES.md");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    (path, text)
}

/// Every file in every shipped fixture directory, by name.
fn fixture_names() -> Vec<String> {
    let root = repository_root();
    let mut names = Vec::new();
    for directory in FIXTURE_DIRECTORIES {
        let directory = root.join(directory);
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            names.push(
                entry
                    .expect("data entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    names.sort();
    assert!(
        !names.is_empty(),
        "found no fixtures in {FIXTURE_DIRECTORIES:?} - the checks below would pass vacuously"
    );
    names
}

#[test]
fn every_fixture_on_disk_has_been_decided_about() {
    let undecided = fixture_names()
        .into_iter()
        .filter(|name| {
            !NO_ATTRIBUTION_NEEDED.contains(&name.as_str())
                && !ATTRIBUTION.iter().any(|(fixture, _)| fixture == name)
        })
        .collect::<Vec<_>>();
    assert!(
        undecided.is_empty(),
        "these fixtures ship with the application and nobody has said whose data they are: \
         {undecided:?}. Add them to ATTRIBUTION with a credit that appears in DATA-SOURCES.md, \
         or to NO_ATTRIBUTION_NEEDED if they are this repository's own."
    );
}

#[test]
fn every_credit_this_repository_owes_is_in_the_shipped_attribution() {
    let (path, attribution) = shipped_attribution();
    let mut missing = Vec::new();
    for (fixture, phrases) in ATTRIBUTION {
        for phrase in *phrases {
            if !attribution.contains(phrase) {
                missing.push(format!("{fixture}: {phrase:?}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} does not carry the credits these redistributed fixtures owe: {missing:#?}",
        path.display()
    );
}

/// The table is only worth anything if the files it names are really there.
/// Without this, deleting a fixture would leave a credit for data the
/// repository no longer ships, and the test above would keep passing.
#[test]
fn the_attribution_table_describes_fixtures_that_exist() {
    let present = fixture_names();
    let stale = ATTRIBUTION
        .iter()
        .map(|(fixture, _)| *fixture)
        .chain(NO_ATTRIBUTION_NEEDED.iter().copied())
        .filter(|fixture| !present.iter().any(|name| name == fixture))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these fixtures are credited or excused but are in none of the shipped fixture \
         directories: {stale:?}"
    );
}

/// The Level II credit is pinned in `data_source`, next to the bucket
/// constants it is about; putting it here would have meant giving this decode
/// crate a dev-dependency on an HTTP crate to read two strings.
/// See `data_source/tests/data_credits.rs`.
#[test]
fn the_attribution_file_has_a_section_for_redistributed_data() {
    let (path, attribution) = shipped_attribution();
    assert!(
        attribution.contains("## Redistributed in this repository"),
        "{}: the credits above have nowhere to live",
        path.display()
    );
}
