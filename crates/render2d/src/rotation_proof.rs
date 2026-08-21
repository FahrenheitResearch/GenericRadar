//! The camera rotation, checked against REAL Level II sweeps.
//!
//! The claim is that a camera rotation about the antenna's own screen position
//! is an ISOMETRY of this raster: the ground range a pixel reads, the gate it
//! lands in and the circular span of its row are all unchanged, and the only
//! thing that moves is the compass azimuth, by exactly the rotation. That is
//! what lets the echo be turned without being resampled, and it is why a range
//! ring drawn as a screen circle still sits on the data.
//!
//! Real sweeps rather than a fixture, because a real cut has uneven azimuth
//! gaps, missing radials and duplicated beams, and it is the bin filling around
//! those gaps that an evenly spaced synthetic sweep would not exercise.

use std::path::PathBuf;

use radar_core::{MomentType, RadarVolume};

use crate::{
    AZIMUTH_BIN_WIDTH_DEG, AZIMUTH_BINS, AzimuthLookup, LookupGeometry, ViewportRasterOptions,
    viewport_geometry, viewport_lookup,
};

/// The same cache the rest of this crate's real-data tests read.
fn level2_cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("RADAR_WORKSTATION_L2_CACHE") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path)
            .join("FahrenheitResearch")
            .join("RadarWorkstation")
            .join("cache")
            .join("level2-live");
    }
    PathBuf::from("level2-live")
}

fn cached_volumes(limit: usize) -> Vec<(String, RadarVolume)> {
    let Ok(entries) = std::fs::read_dir(level2_cache_dir()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_V06"))
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            let raw = std::fs::read(&path).ok()?;
            let volume = nexrad_io::decode_volume_from_bytes(&raw).ok()?;
            Some((name, volume))
        })
        .take(limit)
        .collect()
}

fn options(width: u32, height: u32, km_per_px: f32, rotation_rad: f32) -> ViewportRasterOptions {
    ViewportRasterOptions {
        width,
        height,
        radar_x_px: width as f32 * 0.5,
        radar_y_px: height as f32 * 0.5,
        km_per_px_x: km_per_px,
        km_per_px_y: km_per_px,
        rotation_rad,
    }
}

#[test]
fn a_rotation_moves_only_the_azimuth_and_leaves_range_and_gate_alone() {
    let volumes = cached_volumes(3);
    if volumes.is_empty() {
        eprintln!(
            "SKIPPED a_rotation_moves_only_the_azimuth_and_leaves_range_and_gate_alone: no \
             cached Level II volumes, so the rotated raster is UNPROVEN on this machine"
        );
        return;
    }
    // 30 degrees is exactly 300 bins of the 0.1 degree azimuth lattice, so the
    // shift can be asserted as an integer. 32.327957 is the rotation the
    // eastern seaboard actually gets from a Portland anchor.
    for rotation_deg in [30.0_f32, 32.327_957, -30.0, 179.9] {
        let shift_bins = f64::from(rotation_deg / AZIMUTH_BIN_WIDTH_DEG).round() as i64;
        for (name, volume) in &volumes {
            let Some(cut) = volume.cuts.first() else {
                continue;
            };
            let Some(grid) = cut.moments.get(&MomentType::Reflectivity) else {
                continue;
            };
            let lookup = AzimuthLookup::new(cut, grid);
            let unrotated = viewport_geometry(grid, options(1600, 900, 0.35, 0.0));
            let rotated =
                viewport_geometry(grid, options(1600, 900, 0.35, rotation_deg.to_radians()));

            let mut compared = 0_u64;
            let mut off_by_more_than_one = 0_u64;
            let mut worst_bin_error = 0_i64;
            let mut only_one_resolved = 0_u64;
            for y in 0..900 {
                // A row's span is a chord of a CIRCLE about the antenna, and a
                // circle does not care how the camera is turned.
                assert_eq!(
                    unrotated.x_range_for_row(y),
                    rotated.x_range_for_row(y),
                    "{name} at {rotation_deg} deg: row {y} changed span"
                );
                let Some(range) = unrotated.x_range_for_row(y) else {
                    continue;
                };
                for x in range {
                    let before = viewport_lookup(x, y, grid, &lookup, unrotated);
                    let after = viewport_lookup(x, y, grid, &lookup, rotated);
                    match (before, after) {
                        (Some(before), Some(after)) => {
                            assert_eq!(
                                before.gate, after.gate,
                                "{name} at {rotation_deg} deg: pixel ({x}, {y}) changed gate"
                            );
                            compared += 1;
                            let delta = (after.azimuth_bin as i64 - before.azimuth_bin as i64
                                + shift_bins)
                                .rem_euclid(AZIMUTH_BINS as i64);
                            let error = delta.min(AZIMUTH_BINS as i64 - delta);
                            worst_bin_error = worst_bin_error.max(error);
                            if error > 1 {
                                off_by_more_than_one += 1;
                            }
                        }
                        // A shifted azimuth can land in a bin no radial
                        // reached, which is a property of the sweep and not of
                        // the rotation.
                        (None, None) => {}
                        _ => only_one_resolved += 1,
                    }
                }
            }
            assert!(
                compared > 100_000,
                "{name}: only {compared} pixels resolved, so this proves nothing"
            );
            // A tenth-degree bin is a fifth of a real radial, so a pixel whose
            // azimuth sits on a bin edge can round either way. Those are the
            // only disagreements allowed, and they have to be rare.
            let stray = off_by_more_than_one as f64 / compared as f64;
            let unresolved = only_one_resolved as f64 / compared as f64;
            println!(
                "{name} at {rotation_deg} deg: {compared} pixels, worst bin error \
                 {worst_bin_error}, {off_by_more_than_one} off by more than one bin \
                 ({:.6}%), {only_one_resolved} resolved on one side only ({:.6}%)",
                stray * 100.0,
                unresolved * 100.0
            );
            assert!(
                stray < 1e-4,
                "{name} at {rotation_deg} deg: {stray} of pixels moved by more than one \
                 azimuth bin"
            );
            assert!(
                unresolved < 1e-3,
                "{name} at {rotation_deg} deg: {unresolved} of pixels resolved on one side only"
            );
        }
    }
}
