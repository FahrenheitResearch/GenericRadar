//! Read-only proof against a directory of per-sweep research-radar exports.
//!
//! Run with:
//!
//! ```text
//! RADAR_RESEARCH_SWEEP_DIR=/path/to/msg31 cargo test --release -p nexrad_io \
//!     --test research_sweep_assembly_real -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nexrad_io::sweep_assembly::{
    ProvenSweepMembership, SweepAssemblyClassification, SweepAssemblyDecision, append_proven_sweep,
    classify_archive_sweep, decide_adjacent_sweeps,
};
use radar_core::RadarVolume;

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_file() {
            files.push(path);
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(path) {
            pending.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
        }
    }
    files.sort();
    files
}

#[test]
#[ignore = "set RADAR_RESEARCH_SWEEP_DIR to a real per-sweep Archive II corpus"]
fn real_research_sweeps_assemble_only_on_internal_identity() {
    let root = PathBuf::from(
        std::env::var("RADAR_RESEARCH_SWEEP_DIR").expect("RADAR_RESEARCH_SWEEP_DIR is not set"),
    );
    let paths = files_under(&root);
    assert!(!paths.is_empty(), "{} contains no files", root.display());

    let mut per_site: BTreeMap<String, Vec<(PathBuf, RadarVolume, ProvenSweepMembership)>> =
        BTreeMap::new();
    let mut refused = Vec::new();
    for path in paths {
        let raw = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
        let volume = nexrad_io::decode_volume_from_bytes(&raw)
            .unwrap_or_else(|error| panic!("could not decode {}: {error}", path.display()));
        match classify_archive_sweep(&raw, &volume) {
            SweepAssemblyClassification::Proven(evidence) => {
                per_site
                    .entry(volume.site.id.clone())
                    .or_default()
                    .push((path, volume, evidence));
            }
            SweepAssemblyClassification::Refused(reason) => refused.push((path, reason)),
        }
    }
    assert!(
        refused.is_empty(),
        "the corpus contains members that cannot be classified: {refused:?}"
    );

    let mut decoded_files = 0usize;
    let mut logical_volumes = 0usize;
    let mut multi_member_volumes = 0usize;
    for (site, mut members) in per_site {
        members.sort_by(|left, right| {
            left.1
                .volume_time
                .cmp(&right.1.volume_time)
                .then_with(|| left.0.cmp(&right.0))
        });
        decoded_files += members.len();
        let mut pending: Option<(RadarVolume, ProvenSweepMembership)> = None;
        for (_, volume, evidence) in members {
            let can_append = pending.as_ref().is_some_and(|(_, current)| {
                decide_adjacent_sweeps(current, &evidence)
                    == SweepAssemblyDecision::ProvenSameVolume
            });
            if can_append {
                let (assembled, current) = pending.as_mut().expect("checked above");
                append_proven_sweep(assembled, current, volume, evidence).unwrap();
            } else {
                if let Some((assembled, current)) = pending.take() {
                    assert_eq!(assembled.cuts.len(), current.member_count);
                    multi_member_volumes += usize::from(current.member_count > 1);
                    logical_volumes += 1;
                }
                pending = Some((volume, evidence));
            }
        }
        if let Some((assembled, current)) = pending {
            assert_eq!(assembled.cuts.len(), current.member_count);
            multi_member_volumes += usize::from(current.member_count > 1);
            logical_volumes += 1;
        }
        println!("{site}: internal identity verified");
    }

    assert!(decoded_files > 1);
    assert!(
        multi_member_volumes > 0,
        "the corpus proved no split volumes"
    );
    assert!(logical_volumes < decoded_files);
    println!(
        "{decoded_files} sweep files -> {logical_volumes} logical volumes; \
         {multi_member_volumes} volumes contain multiple files"
    );
}
