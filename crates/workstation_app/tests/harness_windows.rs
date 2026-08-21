//! No example may open a window unless it was asked to, and no file may claim
//! that a window has been suppressed.
//!
//! Several examples in this workspace photograph real chrome by driving a real
//! `eframe` viewport. That viewport is a real, focused window on whatever
//! display the harness was started on, and `eframe` maps it without consulting
//! the caller - `src/harness_window.rs` records exactly where.
//! `ViewportBuilder::with_visible(false)` does not suppress that window at
//! runtime, so the source must not claim that it does.
//!
//! Two rules are pinned here:
//!
//! 1. every example that calls `eframe::run_native` either refuses to start
//!    without `--window` (the harnesses that need a window) or reaches
//!    `run_native` only when `--window` was given (`theme_gallery`, whose
//!    default mode renders through `wgpu` with no surface);
//! 2. no source or document in the workspace claims a harness window is
//!    hidden, invisible, or kept off a display. A shape test cannot notice
//!    that a flag is ignored downstream; a claim test at least fails when the
//!    tree says something that has stopped being true.
//!
//! Applications are deliberately out of scope. `workstation_app/src/main.rs`
//! and `app_ui/src/main.rs` are the shipped binaries: a window is the entire
//! point of running them, and nobody starts one by accident while trying to
//! get a picture.

use std::path::{Path, PathBuf};

// The policy module itself, compiled and unit-tested here. Nothing else in the
// workspace declares it as a module - the examples reach it by `#[path]` - and
// an unreferenced source file is not compiled at all, so without this include
// its own tests would silently never run.
//
// `dead_code` is allowed for the same reason the examples allow it: each
// `#[path]` include is its own compilation of the module, and no single one of
// them uses the whole surface. The parts unused here are the ones that read
// `std::env::args`, which a test has no business calling.
#[allow(dead_code)]
#[path = "../src/harness_window.rs"]
mod harness_window;

/// The workspace root: this crate's manifest directory, up out of
/// `crates/workstation_app`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("workstation_app lives two levels under the workspace root")
        .to_path_buf()
}

/// Every `crates/*/examples/*.rs` in the workspace, as (path, source).
fn example_sources() -> Vec<(PathBuf, String)> {
    let crates = workspace_root().join("crates");
    let mut found = Vec::new();
    let entries = std::fs::read_dir(&crates)
        .unwrap_or_else(|error| panic!("read {}: {error}", crates.display()));
    for entry in entries {
        let examples = entry.expect("crates entry").path().join("examples");
        let Ok(files) = std::fs::read_dir(&examples) else {
            continue;
        };
        for file in files {
            let path = file.expect("examples entry").path();
            if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                found.push((path, source));
            }
        }
    }
    assert!(
        !found.is_empty(),
        "found no examples under {} - the scan below would pass vacuously",
        crates.display()
    );
    found
}

/// Every Rust source and Markdown document under the workspace's `crates` and
/// `docs` directories, as (path, source).
fn workspace_prose() -> Vec<(PathBuf, String)> {
    fn walk(directory: &Path, into: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                walk(&path, into);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "rs" || extension == "md")
            {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                into.push((path, source));
            }
        }
    }
    let mut found = Vec::new();
    let root = workspace_root();
    walk(&root.join("crates"), &mut found);
    walk(&root.join("docs"), &mut found);
    found
}

/// Every harness that opens a window says so in its own usage, and can be
/// told to. A usage line that does not mention the flag is how the next
/// person learns the wrong rule.
#[test]
fn windowed_examples_document_the_window_flag() {
    for (path, source) in example_sources() {
        if !source.contains("run_native") {
            continue;
        }
        assert!(
            source.contains(harness_window::WINDOW_FLAG),
            "{} opens a viewport but never mentions {} - a reader has no way to discover that \
             this harness needs a window, or how to say so",
            path.display(),
            harness_window::WINDOW_FLAG
        );
    }
}

/// The rule with teeth: an example cannot reach `eframe::run_native` without
/// the operator having typed the flag.
///
/// Two shapes satisfy it. A harness that has no windowless mode calls
/// `require_window_or_exit`, which stops the process before anything is
/// decoded. A harness that has one - `theme_gallery` - tests the request and
/// only then calls `run_native`. Either way the arguments decide, and
/// forgetting to think about it is not one of the options.
#[test]
fn no_example_reaches_run_native_without_being_asked_for_a_window() {
    let mut offenders = Vec::new();
    let mut checked = 0_usize;
    for (path, source) in example_sources() {
        if !source.contains("run_native") {
            continue;
        }
        checked += 1;
        let refuses = source.contains("harness_window::require_window_or_exit(");
        let branches = source.contains("harness_window::requested_by_process()");
        if !refuses && !branches {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        checked > 0,
        "no example calls run_native - this test would pass vacuously"
    );
    assert!(
        offenders.is_empty(),
        "these examples call eframe::run_native without consulting harness_window, so they open \
         a real window on whoever's display they are started on whether or not anyone asked: \
         {offenders:#?}"
    );
}

/// Nothing in the tree may claim a harness window is suppressed.
///
/// This is the test that would have caught the previous attempt. The claims
/// below were all shipped simultaneously with a `with_visible(false)` call
/// that `eframe` discards, so every one of them was false while a shape test
/// on the call site passed.
#[test]
fn nothing_claims_a_harness_window_is_hidden() {
    // The scanner is not its own subject: this file names the phrases it is
    // looking for, and would otherwise report itself.
    let scanner = Path::new(file!())
        .file_name()
        .expect("this file has a name")
        .to_owned();
    // Phrases, not words: "nothing on screen" is ordinary prose about the
    // application's own empty state and appears all over the workspace, so a
    // scan on words would be noise nobody keeps. Each of these was in the tree
    // at the same time as a call that did not work.
    const CLAIMS: &[&str] = &[
        "never puts anything on a display",
        "never mapped to a display",
        "nothing appears on a display",
        "built invisible",
        "invisible by default",
        "keeps it from being mapped",
        "with_visible(false)",
    ];
    let mut found = Vec::new();
    for (path, source) in workspace_prose() {
        if path.file_name() == Some(scanner.as_ref()) {
            continue;
        }
        let lowered = source.to_ascii_lowercase();
        for claim in CLAIMS {
            if lowered.contains(claim) {
                found.push(format!("{}: {claim:?}", path.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "these files claim a harness window is kept off a display. eframe maps it anyway - see \
         crates/workstation_app/src/harness_window.rs - so the claim is false and a reader \
         acting on it loses their screen: {found:#?}"
    );
}
