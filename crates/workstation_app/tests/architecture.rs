//! Architectural constraints on the workstation binary.
//!
//! There is deliberately NO line-count limit here, and one should not be added
//! back. An earlier version of this file capped every module at 2000 lines and
//! `main.rs` at 300. It did not produce small modules; it produced modules that
//! stopped at exactly the limit - seven files landed between 1927 and 2000
//! lines, `app.rs` at precisely 2000 - and it charged the same rent for a
//! `#[cfg(test)]` block as for shipped code, so the cheapest way to stay under
//! it was to write fewer tests or to scatter a file for arithmetic reasons
//! rather than for a reason a reader would recognise. Module size is a
//! judgement call, and it is made by whoever is reading the module.
//!
//! What remains is the dependency firewall, which is a real architectural
//! boundary rather than a proxy for one: it says which crates the workstation
//! may talk to directly, so the graphics backend, the network and the GIS
//! machinery stay behind the crates that own them.

use std::collections::BTreeSet;

const ALLOWED_DIRECT_DEPENDENCIES: &[&str] = &[
    "analyst_runtime",
    "chrono",
    "color_tables",
    "data_source",
    "eframe",
    // The map scene owns the graphics backend; the workstation depends on it
    // rather than on wgpu, bytemuck or any GIS crate directly.
    "map_scene",
    "nexrad_io",
    // Product meaning — units, domains, availability, cut policy — is declared
    // once in the product engine and read here. Admitted deliberately: without
    // it the workstation would keep its own second catalog, which is what let a
    // legend and a colour table disagree about the same product.
    "product_engine",
    "radar_core",
    // The 3D volume explorer parallelises its floor-texture build. Admitted
    // deliberately, and narrowly: rayon is a CPU work-splitting primitive, not
    // a new capability - it cannot reach the GPU, the network, or the disk. It
    // is here because `vol3d.rs` is kept byte-close to the BowEcho module it
    // was ported from, so that upstream patches to that viewer keep applying
    // to both repositories. Splitting the file to avoid one dependency would
    // trade a working shared upstream for a lint.
    "rayon",
    "render2d",
    // Persisted settings: the store, the registry, the platform paths. Pure
    // std plus serde - no network, no GPU, no GIS - and deliberately at the
    // bottom of the workspace so any crate can declare settings without a
    // cycle. Admitted so the workstation can be the one composition root that
    // loads, applies and mirrors them.
    "settings",
];

#[test]
fn direct_dependencies_stay_inside_the_radar_workstation_firewall() {
    let manifest = include_str!("../Cargo.toml");
    let dependencies = dependency_names(manifest);
    let allowed = ALLOWED_DIRECT_DEPENDENCIES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let unexpected = dependencies
        .difference(&allowed)
        .copied()
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "unexpected direct workstation dependencies: {unexpected:?}"
    );
}

fn dependency_names(manifest: &str) -> BTreeSet<&str> {
    let mut in_dependencies = false;
    let mut names = BTreeSet::new();
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _value)) = line.split_once('=') {
            // A dependency may be declared as `name = ...` or with a dotted
            // key such as `name.workspace = true`; the crate is the first
            // segment either way.
            let name = key.trim().split('.').next().unwrap_or_default().trim();
            if !name.is_empty() {
                names.insert(name);
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parser_reads_only_direct_dependency_section() {
        let manifest = r#"
[package]
name = "example"

[dependencies]
a = "1"
b = { path = "../b" }

[dev-dependencies]
c = "1"
"#;
        assert_eq!(dependency_names(manifest), BTreeSet::from(["a", "b"]));
    }

    #[test]
    fn manifest_parser_reads_workspace_inherited_dependencies() {
        let manifest = r#"
[dependencies]
a.workspace = true
b = { path = "../b" }
"#;
        assert_eq!(dependency_names(manifest), BTreeSet::from(["a", "b"]));
    }
}
