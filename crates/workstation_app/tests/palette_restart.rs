//! Save, apply, restart - through the REAL settings paths, in a process of
//! its own.
//!
//! The promise the colour table editor makes is that a palette an analyst
//! saves is theirs and comes back on the next launch. Everything else about it
//! can be checked against a scratch directory; that promise cannot, because
//! the two halves of it are wired together by
//! `settings::user_colortables_dir` - the editor writes there and
//! `settings_ui::palettes` reads there - and a test that passes both halves
//! the same directory by hand would still pass if they named two different
//! ones.
//!
//! So this injects a config root with `settings::set_app_config_root`, writes
//! through the editor's own store into whatever directory that produces, and
//! restores through `capture_palettes` / `apply_palettes`, which is literally
//! what the application does at shutdown and at launch.
//!
//! # Why it is a binary and not a `#[test]`
//!
//! The config root is a process-global `OnceLock`: the first lookup wins and
//! every later injection is refused. Under the ordinary test harness the
//! included modules' own `#[cfg(test)]` suites would run in parallel with this
//! one, several of them resolve a palette (and therefore the root) on the way,
//! and whichever thread got there first would decide whether this file was
//! writing into a scratch directory or into the analyst's own. `harness =
//! false` (see `Cargo.toml`) makes this a plain binary: rustc is not given
//! `--test`, so the `#[test]` functions the includes carry are never collected
//! and `main` is the only thing that runs, in the order it says.
//! `settings/tests/paths_override.rs` is its own binary for exactly the same
//! reason.

// The editor's file layer and the settings restore path, compiled here the way
// the binary compiles them. `store` reaches `super::model` and `super::pal`,
// which is why all three are declared at the crate root.
#[allow(dead_code, unused_imports)]
#[path = "../src/palette_editor/model.rs"]
mod model;
#[allow(dead_code, unused_imports)]
#[path = "../src/palette_editor/pal.rs"]
mod pal;
#[allow(dead_code, unused_imports)]
#[path = "../src/settings_ui/palettes.rs"]
mod palettes;
#[allow(dead_code, unused_imports)]
#[path = "../src/palette_editor/store.rs"]
mod store;

use std::collections::BTreeMap;

use color_tables::{ColorTable, ColorTableFamily, ColorTableSet, Rgba8};

use model::EditorTable;

fn main() {
    let root = std::env::temp_dir().join(format!(
        "palette-restart-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("scratch config root");
    assert!(
        settings::set_app_config_root(&root),
        "nothing may have resolved a path before this line"
    );
    let directory = store::user_colortables_dir();
    assert_eq!(
        directory,
        root.join("colortables"),
        "the editor writes somewhere other than the injected root"
    );

    a_saved_palette_comes_back_after_a_restart(&directory);
    a_reserved_name_is_refused_instead_of_being_lost(&directory);
    println!("palette restart: all claims held, under {}", root.display());
    let _ = std::fs::remove_dir_all(&root);
}

/// The promise itself, for a name that carries a rendering suffix in the
/// middle of it - the awkward case that is still perfectly savable.
fn a_saved_palette_comes_back_after_a_restart(directory: &std::path::Path) {
    let mut table = bench_table("Storm (stepped) v2");
    let path = store::free_path_in(directory, &table.pal_name());
    store::save(&table, &path).expect("an ordinary name saves");
    assert!(path.starts_with(directory), "{} escaped", path.display());

    // Applied to the pane, then the application shuts down and starts again:
    // the store keeps the choice, and the launch resolves it.
    let mut installed = ColorTableSet::default();
    installed.set_family(
        ColorTableFamily::Reflectivity,
        table.to_color_table().expect("builds"),
    );
    let restored = palettes::apply_palettes(&palettes::capture_palettes(&installed));
    let back = restored.for_family(ColorTableFamily::Reflectivity);
    assert_eq!(
        back.base_name(),
        "Storm (stepped) v2",
        "the analyst's own palette did not come back"
    );
    assert_eq!(
        back.stops(),
        installed.for_family(ColorTableFamily::Reflectivity).stops(),
        "it came back painting something else"
    );

    // And it is found by the name inside the file rather than by the filename,
    // so renaming it in the editor moves the palette rather than orphaning it.
    table.name = "Storm Cell, night".to_owned();
    store::save(&table, &path).expect("the rename saves into the same file");
    let mut choices: BTreeMap<String, settings::PaletteChoice> = BTreeMap::new();
    choices.insert(
        "reflectivity".to_owned(),
        settings::PaletteChoice {
            name: "Storm Cell, night".to_owned(),
            rendering: "smooth".to_owned(),
            generation: 2,
            ..Default::default()
        },
    );
    assert_eq!(
        palettes::apply_palettes(&choices)
            .for_family(ColorTableFamily::Reflectivity)
            .base_name(),
        "Storm Cell, night"
    );
    std::fs::remove_file(&path).expect("tidy up");
}

/// A name that ENDS in a rendering suffix is refused at save time, and the
/// refusal is the whole point: written, it would be lost at the next launch
/// with nothing said.
fn a_reserved_name_is_refused_instead_of_being_lost(directory: &std::path::Path) {
    let table = bench_table("Storm (stepped)");
    let path = store::free_path_in(directory, &table.pal_name());
    let error = store::save(&table, &path).expect_err("the editor refuses the name");
    let said = error.to_string();
    assert!(
        said.contains("(stepped)") && said.contains("restart"),
        "the refusal must say what it is refusing and why: {said}"
    );
    assert!(
        !path.exists(),
        "a refused save still wrote {}",
        path.display()
    );

    // What it would cost, if it were written anyway: this is the file the
    // editor declines to create, put there by hand.
    let planted = directory.join("planted.pal");
    std::fs::write(&planted, table.pal_text()).expect("write");
    let mut installed = ColorTableSet::default();
    installed.set_family(
        ColorTableFamily::Reflectivity,
        table.to_color_table().expect("builds"),
    );
    let stored = palettes::capture_palettes(&installed);
    assert_eq!(
        stored["reflectivity"].name, "Storm",
        "the identity the application stores is the name without the suffix"
    );
    let restored = palettes::apply_palettes(&stored);
    assert_eq!(
        restored
            .for_family(ColorTableFamily::Reflectivity)
            .base_name(),
        ColorTableSet::default()
            .for_family(ColorTableFamily::Reflectivity)
            .base_name(),
        "a suffixed name comes back as the shipped default - which is why the \
         editor will not write one"
    );
    std::fs::remove_file(&planted).expect("tidy up");
}

/// A small table with a shape that is unmistakable when it comes back.
fn bench_table(name: &str) -> EditorTable {
    let mut table = EditorTable::new(ColorTableFamily::Reflectivity, name);
    table.clear_stops();
    table.push_stop(-10.0, Rgba8::new(0, 0, 0, 0), None);
    table.push_stop(
        25.0,
        Rgba8::opaque(9, 199, 77),
        Some(Rgba8::opaque(1, 2, 3)),
    );
    table.push_stop(60.0, Rgba8::opaque(255, 255, 255), None);
    assert!(
        ColorTable::parse(name, &table.pal_text()).is_ok(),
        "the fixture is a colour table"
    );
    table
}
