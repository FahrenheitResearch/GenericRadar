//! Persisting colour table choices, and restoring them defensively.
//!
//! A palette is stored as its **base name** plus its **rendering** - the two
//! halves `color_tables` split a table into when rendering became a property
//! (commit 18af957): `base_name()` is stable across the smooth/stepped
//! switch, and the rendering is one word. Restoring resolves the name
//! through the shipped catalog for that family, then through the analyst's
//! own colour table folder - the one `settings::user_colortables_dir` names,
//! which is both what the folder scanner (`color_tables::user`) reads and
//! where the colour table editor writes, and which is what makes a table
//! made in the editor survive a restart. A name neither of those holds - a
//! palette from a build that no longer ships it, a file that has been
//! deleted - falls back to the family default. **Never** to nothing: a stale
//! settings file must not blank a pane.
//!
//! A name that resolved to nothing is *kept* rather than overwritten. The
//! every-frame mirror captures whatever is installed, so without that rule a
//! single launch with a user table's file temporarily absent - an external
//! drive not mounted, a folder mid-sync, a file being edited - would replace
//! the analyst's choice with the shipped default for good. See
//! [`capture_palettes_preserving`].

use std::collections::BTreeMap;

use color_tables::user::UserTableLibrary;
use color_tables::{ColorTable, ColorTableFamily, ColorTableSet, TableRendering};
use settings::PaletteChoice;

/// Stable stored identifier per family. Same contract as setting ids: these
/// name choices in files already written, never reuse one.
pub fn family_id(family: ColorTableFamily) -> &'static str {
    match family {
        ColorTableFamily::Reflectivity => "reflectivity",
        ColorTableFamily::Velocity => "velocity",
        ColorTableFamily::SpectrumWidth => "spectrum_width",
        ColorTableFamily::DifferentialReflectivity => "differential_reflectivity",
        ColorTableFamily::CorrelationCoefficient => "correlation_coefficient",
        ColorTableFamily::DifferentialPhase => "differential_phase",
        ColorTableFamily::SpecificDifferentialPhase => "specific_differential_phase",
        ColorTableFamily::Generic => "generic",
    }
}

pub fn family_from_id(id: &str) -> Option<ColorTableFamily> {
    ColorTableFamily::ALL
        .into_iter()
        .find(|family| family_id(*family) == id)
}

pub fn rendering_id(rendering: TableRendering) -> &'static str {
    match rendering {
        TableRendering::Smooth => "smooth",
        TableRendering::Stepped => "stepped",
    }
}

/// Unknown strings fall back to Smooth, the shipped default for every family.
pub fn rendering_from_id(id: &str) -> TableRendering {
    match id {
        "stepped" => TableRendering::Stepped,
        _ => TableRendering::Smooth,
    }
}

/// The era of shipped defaults this build writes. Bumped when a family's
/// default palette changes, so `resolve_choice` can tell a passive capture of
/// a PAST default (migrate it) from a deliberate pick of the same palette
/// made under the current defaults (keep it).
///
/// Generation 2: reflectivity moved GR2Analyst Classic -> AWIPS Wilson,
/// velocity moved Analyst Tornado -> GenericRadar VEL (2026-08-19).
const DEFAULTS_GENERATION: u32 = 2;

/// What the analyst has installed, as the snapshot the store persists.
///
/// Unconditional: every family is written from the live set. That is what
/// "Restore Radar defaults" wants - it means the shipped defaults, including
/// throwing away a stored name for a user table. Everything else should use
/// [`capture_palettes_preserving`], which keeps a stored name whose file is
/// only temporarily missing.
pub fn capture_palettes(tables: &ColorTableSet) -> BTreeMap<String, PaletteChoice> {
    let mut choices = BTreeMap::new();
    for family in ColorTableFamily::ALL {
        let table = tables.for_family(family);
        choices.insert(
            family_id(family).to_owned(),
            PaletteChoice {
                name: table.base_name().to_owned(),
                rendering: rendering_id(table.rendering()).to_owned(),
                generation: DEFAULTS_GENERATION,
                ..Default::default()
            },
        );
    }
    choices
}

/// [`capture_palettes`], except that a stored name nothing can currently
/// resolve is carried forward instead of being replaced by the fallback the
/// pane is drawing.
///
/// The case this exists for: an analyst picks a colour table out of their own
/// folder, and next week the file is not there when the application starts -
/// deleted, renamed, on a drive that has not mounted yet, or open in an
/// editor mid-save. `resolve_choice` puts the family default on screen, which
/// is right. The every-frame mirror then captures that default and, without
/// this, would overwrite the analyst's choice with it - so putting the file
/// back would no longer bring the palette back.
///
/// The rule is narrow on purpose. A stored name is only preserved while the
/// family is still showing the shipped default, so the first time the analyst
/// installs anything at all the stored name follows them. The rendering is
/// taken from the live table either way: flipping smooth/stepped on the
/// fallback is a real choice and is stored, while the palette name waits for
/// its file.
///
/// One ambiguity is accepted and is invisible in practice: deliberately
/// selecting the family default while an unresolvable name is stored looks
/// exactly like the fallback, so the stored name stays. The pane draws the
/// same picture either way; only a returning file would tell them apart.
pub fn capture_palettes_preserving(
    tables: &ColorTableSet,
    previous: &BTreeMap<String, PaletteChoice>,
    library: &UserTableLibrary,
) -> BTreeMap<String, PaletteChoice> {
    let mut choices = capture_palettes(tables);
    let defaults = ColorTableSet::default();
    for family in ColorTableFamily::ALL {
        let id = family_id(family);
        let Some(stored) = previous.get(id) else {
            continue;
        };
        if stored.name.is_empty() || name_resolves(family, &stored.name, Some(library)) {
            continue;
        }
        let live = tables.for_family(family);
        if live.base_name() != defaults.for_family(family).base_name() {
            continue;
        }
        let mut preserved = stored.clone();
        preserved.rendering = rendering_id(live.rendering()).to_owned();
        choices.insert(id.to_owned(), preserved);
    }
    choices
}

/// Whether a stored palette name names something this build can install:
/// a shipped palette in that family, or one of the analyst's own files.
fn name_resolves(family: ColorTableFamily, name: &str, library: Option<&UserTableLibrary>) -> bool {
    if name.is_empty() {
        return false;
    }
    if color_tables::builtin_tables_for_family(family)
        .iter()
        .any(|table| table.base_name() == name)
    {
        return true;
    }
    library.is_some_and(|library| library.table_for_family_named(family, name).is_some())
}

/// Resolve one family's stored choice against the shipped catalog and then
/// against the analyst's own colour table folder.
///
/// The name is matched by `base_name` so a file written while stepped finds
/// the same palette as one written while smooth; the stored rendering is
/// then applied to whatever was found. An unknown name keeps the family's
/// default palette (in the stored rendering, which was understood even if
/// the name was not).
///
/// Shipped palettes are searched first, always. A user file whose name
/// shadows a shipped base name is renamed with
/// `color_tables::user::USER_NAME_SUFFIX` by the folder scanner, and the
/// editor refuses to save under one at all, precisely so that the two can
/// never both answer to one stored name.
fn resolve_choice(
    family: ColorTableFamily,
    choice: &PaletteChoice,
    library: Option<&UserTableLibrary>,
) -> ColorTable {
    let directory = user_directory(library);
    resolve_choice_in(&directory, family, choice, library)
}

/// The analyst's colour table folder as this resolution should read it: the
/// one the library is already scanning when the application owns a library,
/// and the one `settings` names otherwise. One folder either way - see
/// `crate::user_tables::user_tables_dir`, which is that same function.
fn user_directory(library: Option<&UserTableLibrary>) -> std::path::PathBuf {
    match library {
        Some(library) => library.directory().to_path_buf(),
        None => settings::user_colortables_dir(),
    }
}

/// [`resolve_choice`], against a stated user directory rather than the
/// analyst's own, so the precedence between the shipped catalogue, a user file
/// and the family default can be pinned without a real config root.
pub(crate) fn resolve_choice_in(
    directory: &std::path::Path,
    family: ColorTableFamily,
    choice: &PaletteChoice,
    library: Option<&UserTableLibrary>,
) -> ColorTable {
    let rendering = rendering_from_id(&choice.rendering);
    let catalog = color_tables::builtin_tables_for_family(family);
    let default = ColorTableSet::default();
    // A stored old-default name written under an EARLIER defaults
    // generation carries no analyst intent: the every-frame mirror wrote it
    // for whoever launched the app, so when the shipped default moves the
    // name follows it - once. A deliberate pick of the same classic under
    // the current generation is stored with that generation and respected.
    let superseded = choice.generation < DEFAULTS_GENERATION
        && match family {
            ColorTableFamily::Reflectivity => choice.name == "GR2Analyst Classic REF",
            ColorTableFamily::Velocity => choice.name == "Analyst Tornado VEL",
            _ => false,
        };
    if superseded {
        return default.for_family(family).clone();
    }
    let base = catalog
        .into_iter()
        .find(|table| table.base_name() == choice.name)
        // Then the analyst's own folder, which is where a table made in the
        // colour table editor lives. Without this pass a palette that was
        // saved and applied came back as the family default on the next
        // launch: the name was not in the shipped catalogue, and nothing else
        // in the build read the folder the editor writes into.
        .or_else(|| user_table_named_in(directory, family, &choice.name, library))
        .unwrap_or_else(|| default.for_family(family).clone());
    base.rendered(rendering)
}

/// The palette of this name in the analyst's colour table folder, if they
/// have one there.
///
/// The shipped catalogue is searched first and this second, so a user file can
/// never shadow a preset - a name collision leaves the preset installed and
/// the file still reachable through the colour table editor, rather than
/// silently replacing a table the rest of the build documents.
///
/// Two readings of one folder, in this order:
///
/// * the running library's scan, when the application has one. It is already
///   in memory, it is what the picker offered the name from, and it is the
///   only half that knows a table's *family* and the display name the folder
///   scanner had to give it (`color_tables::user::USER_NAME_SUFFIX`, the
///   per-family numbering) when a file's own name could not be carried as it
///   stood;
/// * otherwise, and as the fallback for a name the library holds no entry
///   for, `color_tables::palette_named_in` - the single shared search, which
///   is also what the colour table editor's store calls to find the file
///   behind a table it has been asked to edit.
///
/// The two agree by construction rather than by luck: both identify a file by
/// the `Name:` row inside it falling back to its stem, both read the same
/// extension set, and both walk the folder in the same order, so where both
/// answer they answer with the same file. That shared home is the point. This
/// module and the editor used to carry a search each, each internally
/// deterministic and each picking a different file out of a directory holding
/// two files of one name - so Edit opened one palette and the next launch
/// installed the other. It lives in `color_tables` because this module is
/// compiled in a second home that does not have the application's crate at
/// all - see the harness at `settings/tests/workstation_settings_ui.rs` - so
/// the one place both sides can reach is the crate that already owns what a
/// palette file says.
fn user_table_named_in(
    directory: &std::path::Path,
    family: ColorTableFamily,
    name: &str,
    library: Option<&UserTableLibrary>,
) -> Option<ColorTable> {
    if let Some(library) = library
        && let Some(table) = library.table_for_family_named(family, name)
    {
        return Some(table.clone());
    }
    color_tables::palette_named_in(directory, name).map(|found| found.table)
}

/// Rebuild a full table set from the snapshot, shipped palettes only.
///
/// Families the snapshot does not mention keep their defaults. Callers that
/// own a user colour table folder want [`apply_palettes_with_user`]; this is
/// for the ones that do not.
pub fn apply_palettes(choices: &BTreeMap<String, PaletteChoice>) -> ColorTableSet {
    apply_resolved(choices, None)
}

/// Rebuild a full table set from the snapshot, resolving names the analyst's
/// own colour table folder supplies as well as the shipped ones.
pub fn apply_palettes_with_user(
    choices: &BTreeMap<String, PaletteChoice>,
    library: &UserTableLibrary,
) -> ColorTableSet {
    apply_resolved(choices, Some(library))
}

fn apply_resolved(
    choices: &BTreeMap<String, PaletteChoice>,
    library: Option<&UserTableLibrary>,
) -> ColorTableSet {
    let mut tables = ColorTableSet::default();
    for (id, choice) in choices {
        let Some(family) = family_from_id(id) else {
            // A family this build does not know - a future build's. The store
            // carries it forward; there is nothing to install it into.
            continue;
        };
        tables.set_family(family, resolve_choice(family, choice, library));
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_id_round_trips_and_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for family in ColorTableFamily::ALL {
            let id = family_id(family);
            assert!(seen.insert(id), "duplicate family id {id:?}");
            assert_eq!(family_from_id(id), Some(family));
        }
    }

    #[test]
    fn a_real_installed_set_survives_capture_and_apply_exactly() {
        // The real catalog, not fixtures: install a non-default palette in a
        // non-default rendering for two families and round-trip the whole set.
        let mut tables = ColorTableSet::default();
        let velocity_pick = color_tables::builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .nth(2)
            .expect("the velocity catalog ships more than two palettes")
            .rendered(TableRendering::Stepped);
        tables.set_family(ColorTableFamily::Velocity, velocity_pick.clone());
        let reflectivity_pick =
            color_tables::builtin_tables_for_family(ColorTableFamily::Reflectivity)
                .into_iter()
                .nth(1)
                .expect("the reflectivity catalog ships more than one palette");
        tables.set_family(ColorTableFamily::Reflectivity, reflectivity_pick.clone());

        let restored = apply_palettes(&capture_palettes(&tables));
        for family in ColorTableFamily::ALL {
            assert_eq!(
                restored.for_family(family),
                tables.for_family(family),
                "family {family:?} did not survive the round trip"
            );
        }
    }

    #[test]
    fn an_unknown_palette_name_falls_back_to_the_family_default_never_to_nothing() {
        let mut choices = BTreeMap::new();
        choices.insert(
            "velocity".to_owned(),
            PaletteChoice {
                name: "A Palette This Build Never Shipped".to_owned(),
                rendering: "stepped".to_owned(),
                ..Default::default()
            },
        );
        let restored = apply_palettes(&choices);
        let restored_velocity = restored.for_family(ColorTableFamily::Velocity);
        // The default palette, in the rendering that WAS understood.
        assert_eq!(
            restored_velocity.base_name(),
            ColorTableSet::default()
                .for_family(ColorTableFamily::Velocity)
                .base_name()
        );
        assert_eq!(restored_velocity.rendering(), TableRendering::Stepped);
        // And every colour stop is real: the table samples, it does not blank.
        assert!(!restored_velocity.stops().is_empty());
    }

    #[test]
    fn an_unknown_family_id_is_skipped_and_an_unknown_rendering_reads_smooth() {
        let mut choices = BTreeMap::new();
        choices.insert(
            "chroma_futures".to_owned(),
            PaletteChoice {
                name: "X".to_owned(),
                rendering: "holographic".to_owned(),
                ..Default::default()
            },
        );
        choices.insert(
            "reflectivity".to_owned(),
            PaletteChoice {
                name: "GR2Analyst Classic REF".to_owned(),
                rendering: "holographic".to_owned(),
                ..Default::default()
            },
        );
        let restored = apply_palettes(&choices);
        assert_eq!(
            restored
                .for_family(ColorTableFamily::Reflectivity)
                .rendering(),
            TableRendering::Smooth
        );
    }

    /// The 2026-08-19 default change, seen from an existing install: the
    /// store is full of the OLD defaults' names because the every-frame
    /// mirror wrote them for everyone, and nobody deliberately picked them.
    /// A pre-generation store (generation 0) migrates to the new defaults.
    #[test]
    fn an_old_stores_passive_default_capture_migrates_to_the_new_defaults() {
        let mut choices = BTreeMap::new();
        choices.insert(
            "velocity".to_owned(),
            PaletteChoice {
                name: "Analyst Tornado VEL".to_owned(),
                rendering: "smooth".to_owned(),
                ..Default::default()
            },
        );
        choices.insert(
            "reflectivity".to_owned(),
            PaletteChoice {
                name: "GR2Analyst Classic REF".to_owned(),
                rendering: "smooth".to_owned(),
                ..Default::default()
            },
        );
        let restored = apply_palettes(&choices);
        assert_eq!(
            restored.for_family(ColorTableFamily::Velocity).base_name(),
            "GenericRadar VEL"
        );
        assert_eq!(
            restored
                .for_family(ColorTableFamily::Reflectivity)
                .base_name(),
            "AWIPS Wilson REF"
        );
    }

    /// A colour table folder of this test's own, removed at the end. One
    /// helper for the whole module: both halves of this file's pins - the
    /// shared name resolver's and the user folder's - put files in a
    /// directory and read them back.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "settings-palettes-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// A minimal two-stop `.pal` naming itself, as the colour table editor
    /// writes them.
    fn user_pal(name: &str) -> String {
        format!(
            "Name: {name}\nProduct: BV\nMode: continuous\nColor4: -40 10 20 30 255\nColor4: 40 200 210 220 255\n"
        )
    }

    /// Save, Apply, restart. Before this, the restore path looked the stored
    /// name up in the shipped catalogue only and dropped anything it did not
    /// find, so an analyst's own table came back as the family default and the
    /// evening's work was gone with no message.
    #[test]
    fn a_palette_saved_by_the_editor_survives_a_restart() {
        let dir = scratch_dir("restart");
        std::fs::write(dir.join("field-vel.pal"), user_pal("Field VEL")).expect("write");
        let choice = PaletteChoice {
            name: "Field VEL".to_owned(),
            rendering: "stepped".to_owned(),
            generation: DEFAULTS_GENERATION,
            ..Default::default()
        };
        let restored = resolve_choice_in(&dir, ColorTableFamily::Velocity, &choice, None);
        assert_eq!(restored.base_name(), "Field VEL");
        assert_eq!(restored.rendering(), TableRendering::Stepped);
        assert_eq!(restored.stops().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A user file is found by the `Name:` row inside it, never by the
    /// filename a name would produce - that mapping is many-to-one, and a
    /// restore that trusted it would install a different palette than the one
    /// that was stored.
    #[test]
    fn a_user_file_is_matched_on_the_name_row_and_not_on_its_filename() {
        let dir = scratch_dir("names");
        // The stem "Storm Detail v2" would produce, holding a different
        // palette entirely.
        std::fs::write(
            dir.join("storm-detail-v2.pal"),
            user_pal("Storm: Detail / v2"),
        )
        .expect("write");
        let missing = PaletteChoice {
            name: "Storm Detail v2".to_owned(),
            generation: DEFAULTS_GENERATION,
            ..Default::default()
        };
        assert_eq!(
            resolve_choice_in(&dir, ColorTableFamily::Velocity, &missing, None).base_name(),
            ColorTableSet::default()
                .for_family(ColorTableFamily::Velocity)
                .base_name(),
            "a file whose Name row says something else must not answer for this name"
        );
        let present = PaletteChoice {
            name: "Storm: Detail / v2".to_owned(),
            generation: DEFAULTS_GENERATION,
            ..Default::default()
        };
        assert_eq!(
            resolve_choice_in(&dir, ColorTableFamily::Velocity, &present, None).base_name(),
            "Storm: Detail / v2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A half-written or hand-mangled `.pal` costs the analyst the one palette
    /// it holds and not the rest of the directory. This runs at startup, where
    /// a directory that stopped being searched at the first bad file would
    /// blank every palette after it.
    #[test]
    fn one_unreadable_file_does_not_stop_the_search() {
        let dir = scratch_dir("unreadable");
        // Sorts before the real one, so a scan that gave up would never reach it.
        std::fs::write(
            dir.join("aa-half-written.pal"),
            "Name: Field VEL\nColor4: 5 1",
        )
        .expect("write");
        std::fs::write(dir.join("notes.txt"), user_pal("Field VEL")).expect("write");
        std::fs::write(dir.join("zz-field-vel.pal"), user_pal("Field VEL")).expect("write");
        let choice = PaletteChoice {
            name: "Field VEL".to_owned(),
            generation: DEFAULTS_GENERATION,
            ..Default::default()
        };
        assert_eq!(
            resolve_choice_in(&dir, ColorTableFamily::Velocity, &choice, None).base_name(),
            "Field VEL"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shipped catalogue wins a name collision. A file cannot quietly
    /// replace a preset the rest of the build documents by name.
    #[test]
    fn a_user_file_never_shadows_a_shipped_preset() {
        let dir = scratch_dir("shadow");
        let preset = ColorTableSet::default()
            .for_family(ColorTableFamily::Velocity)
            .base_name()
            .to_owned();
        std::fs::write(dir.join("shadow.pal"), user_pal(&preset)).expect("write");
        let choice = PaletteChoice {
            name: preset.clone(),
            generation: DEFAULTS_GENERATION,
            ..Default::default()
        };
        let restored = resolve_choice_in(&dir, ColorTableFamily::Velocity, &choice, None);
        assert_eq!(restored.base_name(), preset);
        assert!(
            restored.stops().len() > 2,
            "the two-stop file replaced the shipped preset"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other side of that coin: the same classic names written under the
    /// CURRENT generation are a deliberate pick from the picker, and stick.
    #[test]
    fn a_deliberate_pick_of_the_old_classics_sticks() {
        let mut choices = BTreeMap::new();
        choices.insert(
            "velocity".to_owned(),
            PaletteChoice {
                name: "Analyst Tornado VEL".to_owned(),
                rendering: "smooth".to_owned(),
                generation: DEFAULTS_GENERATION,
                ..Default::default()
            },
        );
        let restored = apply_palettes(&choices);
        assert_eq!(
            restored.for_family(ColorTableFamily::Velocity).base_name(),
            "Analyst Tornado VEL"
        );
        // And the capture stamps the generation, so its own writes are never
        // mistaken for a past build's.
        let captured = capture_palettes(&restored);
        assert_eq!(captured["velocity"].generation, DEFAULTS_GENERATION);
    }

    /// A RadarScope-shaped velocity palette with two-colour ramp rows, the
    /// form `.pal` files are actually traded in.
    const USER_VELOCITY_PAL: &str = "\
Product: BV
Units: KTS
Color: -60 200   0 200    60 220 220
Color: -20  60 220 220     8  60  70
Color:   1  70  20  20   220  60  60
Color:  60 220  60  60   255 255 255
";

    /// The whole point of the feature, at the persistence layer: a table the
    /// analyst supplied is captured by name and comes back as itself.
    #[test]
    fn a_user_table_survives_capture_and_apply_including_its_rendering() {
        let dir = scratch_dir("round-trip");
        std::fs::write(dir.join("Ramp Velocity.pal"), USER_VELOCITY_PAL).expect("write palette");
        let library = UserTableLibrary::open(&dir);

        let mut tables = ColorTableSet::default();
        let picked = library
            .table_for_family_named(ColorTableFamily::Velocity, "Ramp Velocity")
            .expect("the scan found the file")
            .rendered(TableRendering::Stepped);
        tables.set_family(ColorTableFamily::Velocity, picked.clone());

        let stored = capture_palettes_preserving(&tables, &BTreeMap::new(), &library);
        assert_eq!(stored["velocity"].name, "Ramp Velocity");
        assert_eq!(stored["velocity"].rendering, "stepped");

        let restored = apply_palettes_with_user(&stored, &library);
        assert_eq!(restored.for_family(ColorTableFamily::Velocity), &picked);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file goes away. The pane must draw the family default rather than
    /// nothing, and the stored choice must still be there when the file
    /// comes back.
    #[test]
    fn a_deleted_user_file_falls_back_to_the_default_without_destroying_the_choice() {
        let dir = scratch_dir("deleted");
        let path = dir.join("Ramp Velocity.pal");
        std::fs::write(&path, USER_VELOCITY_PAL).expect("write palette");

        let library = UserTableLibrary::open(&dir);
        let mut tables = ColorTableSet::default();
        tables.set_family(
            ColorTableFamily::Velocity,
            library
                .table_for_family_named(ColorTableFamily::Velocity, "Ramp Velocity")
                .expect("the scan found the file")
                .clone(),
        );
        let stored = capture_palettes_preserving(&tables, &BTreeMap::new(), &library);

        // The file disappears between sessions.
        std::fs::remove_file(&path).expect("delete the palette");
        let library = UserTableLibrary::open(&dir);

        let restored = apply_palettes_with_user(&stored, &library);
        let velocity = restored.for_family(ColorTableFamily::Velocity);
        assert_eq!(
            velocity.base_name(),
            ColorTableSet::default()
                .for_family(ColorTableFamily::Velocity)
                .base_name(),
            "a missing file falls back to the family default"
        );
        assert!(!velocity.stops().is_empty(), "and never to a blank pane");

        // The every-frame mirror runs against that fallback and must NOT
        // overwrite the stored name.
        let mirrored = capture_palettes_preserving(&restored, &stored, &library);
        assert_eq!(
            mirrored["velocity"].name, "Ramp Velocity",
            "the analyst's choice was destroyed by the fallback"
        );

        // The file comes back, and so does the palette.
        std::fs::write(&path, USER_VELOCITY_PAL).expect("restore the palette");
        let library = UserTableLibrary::open(&dir);
        assert_eq!(
            apply_palettes_with_user(&mirrored, &library)
                .for_family(ColorTableFamily::Velocity)
                .base_name(),
            "Ramp Velocity"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of that rule: once the analyst installs something
    /// else, the preserved name stops being carried and their new pick is
    /// what persists.
    #[test]
    fn installing_another_palette_releases_a_preserved_missing_name() {
        let dir = scratch_dir("released");
        let library = UserTableLibrary::open(&dir);
        let mut stored = BTreeMap::new();
        stored.insert(
            "velocity".to_owned(),
            PaletteChoice {
                name: "A Table Nobody Has Any More".to_owned(),
                rendering: "smooth".to_owned(),
                generation: DEFAULTS_GENERATION,
                ..Default::default()
            },
        );

        // Still on the fallback: the name is kept.
        let fallback = apply_palettes_with_user(&stored, &library);
        let kept = capture_palettes_preserving(&fallback, &stored, &library);
        assert_eq!(kept["velocity"].name, "A Table Nobody Has Any More");

        // A flip of the rendering is a real choice and is stored, while the
        // name goes on waiting for its file.
        let mut flipped = fallback.clone();
        flipped.set_family(
            ColorTableFamily::Velocity,
            fallback
                .for_family(ColorTableFamily::Velocity)
                .rendered(TableRendering::Stepped),
        );
        let kept = capture_palettes_preserving(&flipped, &stored, &library);
        assert_eq!(kept["velocity"].name, "A Table Nobody Has Any More");
        assert_eq!(kept["velocity"].rendering, "stepped");

        // A different palette: the stored name follows the analyst.
        let mut moved = ColorTableSet::default();
        let other = color_tables::builtin_tables_for_family(ColorTableFamily::Velocity)
            .into_iter()
            .nth(2)
            .expect("the velocity catalog ships more than two palettes");
        moved.set_family(ColorTableFamily::Velocity, other.clone());
        let captured = capture_palettes_preserving(&moved, &stored, &library);
        assert_eq!(captured["velocity"].name, other.base_name());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "Restore Radar defaults" means the shipped defaults, so the
    /// unconditional capture must drop a preserved name rather than keep it.
    #[test]
    fn the_unconditional_capture_still_writes_exactly_what_is_installed() {
        let defaults = capture_palettes(&ColorTableSet::default());
        assert_eq!(
            defaults["velocity"].name,
            ColorTableSet::default()
                .for_family(ColorTableFamily::Velocity)
                .base_name()
        );
    }
}
