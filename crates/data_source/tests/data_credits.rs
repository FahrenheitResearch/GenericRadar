//! The application credits the endpoints it actually calls, in the files that
//! actually reach a reader.
//!
//! NEXRAD Level II is NOAA/NWS data. This application does not fetch it from
//! NOAA/NWS: every archive listing and every real-time chunk comes off the
//! Unidata-hosted buckets named by [`LEVEL2_ARCHIVE_BUCKET`] and
//! [`LEVEL2_CHUNKS_BUCKET`]. Both facts are true and neither replaces the
//! other - NWS produces the data, UCAR/Unidata pays for and serves the objects
//! the traffic lands on - so a credit naming only the producer leaves the host
//! carrying that traffic invisible to anyone reading it.
//!
//! This lives in `data_source` rather than beside the fixture credits in
//! `nexrad_io` because the bucket names are constants here, and comparing the
//! credits against the constants is the part that cannot rot: rename a bucket
//! and the credit stops matching in the same commit.
//!
//! The file checked is `DATA-SOURCES.md`, because a credit only counts where
//! it is published and that is the one that ships verbatim. Neither README is
//! checked: both are replaced on the way to the public repository, so what
//! they say never reaches a reader. The published front page carries a
//! shortened version of the same credits, and is checked beside the release
//! process it belongs to, in
//! `crates/workstation_app/tests/release_process.rs`.

use data_source::{LEVEL2_ARCHIVE_BUCKET, LEVEL2_CHUNKS_BUCKET};

use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("data_source lives two levels under the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> (PathBuf, String) {
    let path = repository_root().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    (path, text)
}

/// The full attribution, shipped verbatim in the release snapshot.
fn shipped_attribution() -> (PathBuf, String) {
    read("DATA-SOURCES.md")
}

#[test]
fn the_shipped_attribution_credits_the_producer_of_the_level_two_data() {
    let (path, attribution) = shipped_attribution();
    assert!(
        attribution.contains("NOAA/NWS"),
        "{}: NEXRAD Level II is NOAA/NWS data and the attribution must say so",
        path.display()
    );
}

#[test]
fn the_shipped_attribution_credits_the_host_the_requests_actually_reach() {
    let (path, attribution) = shipped_attribution();
    assert!(
        attribution.contains("Unidata"),
        "{}: every Level II request in this crate goes to a Unidata-hosted bucket, and the \
         attribution credits nobody for serving it",
        path.display()
    );
    for bucket in [LEVEL2_ARCHIVE_BUCKET, LEVEL2_CHUNKS_BUCKET] {
        assert!(
            attribution.contains(bucket),
            "{}: the attribution should name the bucket it reads ({bucket}) rather than leave a \
             reader to guess which host is meant",
            path.display()
        );
    }
}

/// The other two live endpoints, so the section stays a description of what
/// the application does rather than a list somebody trimmed.
#[test]
fn the_shipped_attribution_credits_the_other_live_endpoints() {
    let (path, attribution) = shipped_attribution();
    for credit in [
        "api.weather.gov",
        "USGS The National Map",
        "OpenStreetMap contributors",
    ] {
        assert!(
            attribution.contains(credit),
            "{}: no credit for {credit}, which the application reads live",
            path.display()
        );
    }
}

#[test]
fn the_shipped_attribution_credits_surface_observation_providers() {
    let (path, attribution) = shipped_attribution();
    for credit in ["Aviation Weather Center", "Iowa Environmental Mesonet"] {
        assert!(
            attribution.contains(credit),
            "{}: no credit for {credit}, which supplies surface observations or their history",
            path.display()
        );
    }
}
