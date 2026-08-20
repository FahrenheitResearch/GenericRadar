//! The Profiles page of the settings window: the surface over
//! [`settings::profiles`].
//!
//! One install, several ways of working - a chase setup, an office setup, a
//! presentation setup - each a named snapshot of the whole settings document.
//! The page is deliberately the ONLY place profiles are managed, because the
//! main window stays quiet: what a running application shows is the name of
//! the active profile in the File menu, one line, and nothing else.
//!
//! # Switching
//!
//! A switch here does exactly two things: it replaces the settings document
//! ([`settings::SettingsStore::replace_document`]) and it raises
//! [`super::SettingsOutcome::profile_switched`]. The application then runs its
//! ordinary apply path over the new document - the same path a hand-edited
//! settings file takes at startup - so every setting moves, including the ones
//! this file has never heard of. Nothing here enumerates settings, and nothing
//! here should ever start to: see the argument in the `settings::profiles`
//! module documentation.
//!
//! # Honesty
//!
//! Three rules this page keeps, because a profile system that breaks any of
//! them quietly destroys work:
//!
//! * switching away from a modified profile is never silent. The page names
//!   what differs and offers to keep it (into the profile, or as a new one) or
//!   to discard it, and cancelling is always the third option;
//! * the active profile's name, and whether it has been modified since, are
//!   visible here and in the File menu;
//! * a profile file this build cannot fully read is reported, not hidden and
//!   not refused: the parts it understands are applied and the parts it does
//!   not are listed under the row and carried through untouched.

use eframe::egui;
use settings::profiles::{self, ProfileLibrary};
use settings::{SettingsDocument, SettingsRegistry, SettingsStore};

use super::{MIN_INTERACT_HEIGHT, SettingsOutcome};

/// How many differences are listed before the list is summarised. Long enough
/// to recognise what is at stake, short enough that the prompt is still a
/// prompt and not a report.
const MAX_LISTED_DIFFERENCES: usize = 8;

/// What a switch would cost, asked before the switch happens.
enum SwitchGuard {
    /// Nothing would be lost.
    Clear,
    /// The active profile has unsaved changes, named.
    Modified {
        active: String,
        active_has_file: bool,
        differences: Vec<String>,
    },
    /// The document points at a profile that is no longer in the library, so
    /// what has changed since cannot be worked out at all. Said rather than
    /// guessed at.
    ActiveMissing { active: String },
}

/// A switch waiting on an answer to the unsaved-changes question.
struct PendingSwitch {
    target: String,
    guard: SwitchGuard,
}

/// The page's state between frames. Owned by [`super::SettingsUi`].
#[derive(Default)]
pub struct ProfilesUi {
    /// Opened on first draw, not on construction: the library lives under
    /// `settings::app_config_root`, a set-once path that a mobile shell (or a
    /// screenshot harness) injects before the first frame and after this
    /// struct is built. Resolving it in `Default` would nail the root down
    /// early and put the profiles in the wrong place for the rest of the run.
    library: Option<ProfileLibrary>,
    /// What the application declares "as shipped". `None` until it says, and
    /// then `SettingsDocument::default()` is the honest stand-in.
    shipped: Option<SettingsDocument>,
    new_name: String,
    /// `(profile being renamed, the edited name)`.
    renaming: Option<(String, String)>,
    pending: Option<PendingSwitch>,
    /// The last operation's outcome, in its own words.
    message: Option<String>,
    /// What the last switch could not honour.
    switch_notes: Vec<String>,
}

impl ProfilesUi {
    /// Declare the document the shipped profile installs.
    ///
    /// Called by the application once, at startup, with a document describing
    /// how this build behaves with nothing stored - default pane layout,
    /// default colour tables. It is injected rather than assumed because only
    /// the application can build those structured parts.
    pub fn set_shipped(&mut self, shipped: SettingsDocument) {
        self.shipped = Some(shipped);
        // Rebuilt on the next draw, with the registry that draw carries.
        self.library = None;
    }

    fn library(&mut self, registry: &SettingsRegistry) -> &mut ProfileLibrary {
        if self.library.is_none() {
            self.library = Some(ProfileLibrary::open(
                settings::profiles_dir(),
                self.shipped.clone().unwrap_or_default(),
                registry,
            ));
        }
        self.library.as_mut().expect("just built")
    }

    /// The active profile's name and whether the live settings have moved
    /// away from it - for the File menu, which shows one line and no controls.
    ///
    /// `None` before anything has ever been switched to *and* while the live
    /// settings still match the shipped ones: a fresh install has no profile
    /// worth naming.
    pub fn summary(
        &mut self,
        registry: &SettingsRegistry,
        store: &SettingsStore,
    ) -> Option<(String, bool)> {
        let named = profiles::active_profile(store.document()).map(str::to_owned);
        let library = self.library(registry);
        let name = named.or_else(|| {
            let shipped = library.profiles().first()?;
            profiles::differs(store.document(), &shipped.document, registry)
                .then(|| shipped.name.clone())
        })?;
        let modified = library
            .find(&name)
            .map(|profile| profiles::differs(store.document(), &profile.document, registry))
            // A profile that is no longer here cannot be matched, and saying
            // "unmodified" about it would be a guess.
            .unwrap_or(true);
        Some((name, modified))
    }

    /// The profile the live document belongs to: the one it names, else the
    /// shipped one, which is what a settings file that has never met a profile
    /// is measured against.
    fn active_name(&mut self, registry: &SettingsRegistry, store: &SettingsStore) -> String {
        profiles::active_profile(store.document())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                self.library(registry)
                    .profiles()
                    .first()
                    .map(|profile| profile.name.clone())
                    .unwrap_or_else(|| profiles::SHIPPED_NAME.to_owned())
            })
    }

    /// What switching would cost right now.
    fn guard(&mut self, registry: &SettingsRegistry, store: &SettingsStore) -> SwitchGuard {
        let active = self.active_name(registry, store);
        let library = self.library(registry);
        let Some(profile) = library.find(&active) else {
            return SwitchGuard::ActiveMissing { active };
        };
        let active_has_file = !profile.is_shipped();
        let differences: Vec<String> =
            profiles::differences(store.document(), &profile.document, registry)
                .iter()
                .map(|difference| difference.describe(registry))
                .collect();
        if differences.is_empty() {
            SwitchGuard::Clear
        } else {
            SwitchGuard::Modified {
                active,
                active_has_file,
                differences,
            }
        }
    }

    /// Ask for a switch. Applies it when nothing would be lost, and otherwise
    /// puts the question on screen.
    fn request_switch(
        &mut self,
        target: &str,
        registry: &SettingsRegistry,
        store: &mut SettingsStore,
        outcome: &mut SettingsOutcome,
    ) {
        match self.guard(registry, store) {
            SwitchGuard::Clear => self.apply_switch(target, registry, store, outcome),
            guard => {
                self.pending = Some(PendingSwitch {
                    target: target.to_owned(),
                    guard,
                });
            }
        }
    }

    /// Do the switch: replace the document, and tell the application to apply
    /// it. Everything else about a profile switch happens on the application's
    /// side of that flag.
    fn apply_switch(
        &mut self,
        target: &str,
        registry: &SettingsRegistry,
        store: &mut SettingsStore,
        outcome: &mut SettingsOutcome,
    ) {
        let library = self.library(registry);
        let Some(profile) = library.find(target) else {
            self.pending = None;
            self.message = Some(format!("There is no profile called '{target}'."));
            return;
        };
        let name = profile.name.clone();
        let notes: Vec<String> = profile
            .faults
            .iter()
            .map(profiles::ProfileNote::message)
            .collect();
        let merged = profiles::merge_for_switch(store.document(), &profile.document, &name);
        store.replace_document(merged);
        outcome.profile_switched = true;
        self.pending = None;
        self.switch_notes = notes;
        self.message = Some(match self.switch_notes.len() {
            0 => format!("Switched to '{name}'."),
            count => format!("Switched to '{name}' - {count} thing(s) this build could not use:"),
        });
    }

    fn report(&mut self, result: Result<String, profiles::ProfileError>) {
        self.message = Some(match result {
            Ok(message) => message,
            Err(error) => format!("Refused: {error}"),
        });
    }
}

/// Draw the Profiles page.
pub fn draw_profiles_page(
    ui: &mut egui::Ui,
    state: &mut ProfilesUi,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    outcome: &mut SettingsOutcome,
) {
    ui.label(
        egui::RichText::new(
            "A profile is a named snapshot of every setting in this window - one install can \
             hold a chase setup, an office setup and a presentation setup and move between \
             them. Switching applies the whole snapshot at once, including settings this \
             page has never heard of, because a profile carries the settings file itself \
             rather than a list of knobs.",
        )
        .small()
        .weak(),
    );
    ui.add_space(6.0);

    draw_active_block(ui, state, registry, store, outcome);
    if state.pending.is_some() {
        draw_pending_block(ui, state, registry, store, outcome);
    }
    draw_new_profile_block(ui, state, registry, store);
    ui.add_space(8.0);
    draw_profile_rows(ui, state, registry, store, outcome);
    draw_broken_rows(ui, state, registry);
    draw_footer(ui, state, registry);
}

/// Which profile is in force, whether it has been changed since, and the two
/// things to do about it.
fn draw_active_block(
    ui: &mut egui::Ui,
    state: &mut ProfilesUi,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    outcome: &mut SettingsOutcome,
) {
    let active = state.active_name(registry, store);
    let guard = state.guard(registry, store);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        match &guard {
            SwitchGuard::Clear => {
                ui.strong(format!("Active profile: {active}"));
                ui.label(
                    egui::RichText::new("Every setting matches it.")
                        .small()
                        .weak(),
                );
            }
            SwitchGuard::ActiveMissing { active } => {
                ui.strong(format!(
                    "Active profile: {active} - no longer in the library"
                ));
                ui.label(
                    egui::RichText::new(
                        "Its file has been deleted or renamed outside the application, so what \
                         has changed since cannot be listed. Saving the current settings under \
                         a name puts them back on record.",
                    )
                    .small()
                    .weak(),
                );
            }
            SwitchGuard::Modified {
                active,
                active_has_file,
                differences,
            } => {
                ui.strong(format!(
                    "Active profile: {active} - modified ({} setting(s) differ)",
                    differences.len()
                ));
                list_differences(ui, differences);
                ui.horizontal_wrapped(|ui| {
                    if *active_has_file {
                        let active = active.clone();
                        if ui.button(format!("Save these into '{active}'")).clicked() {
                            let document = store.document().clone();
                            let result = state
                                .library(registry)
                                .overwrite(&active, &document, registry)
                                .map(|()| format!("Saved the current settings into '{active}'."));
                            state.report(result);
                        }
                    }
                    let active = active.clone();
                    if ui
                        .button(format!("Discard them - go back to '{active}'"))
                        .clicked()
                    {
                        state.apply_switch(&active, registry, store, outcome);
                    }
                });
            }
        }
    });
    ui.add_space(6.0);
}

/// The unsaved-changes question, on screen until it is answered.
fn draw_pending_block(
    ui: &mut egui::Ui,
    state: &mut ProfilesUi,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    outcome: &mut SettingsOutcome,
) {
    let Some(pending) = state.pending.as_ref() else {
        return;
    };
    let target = pending.target.clone();
    let (active, active_has_file, differences, missing) = match &pending.guard {
        SwitchGuard::Modified {
            active,
            active_has_file,
            differences,
        } => (active.clone(), *active_has_file, differences.clone(), false),
        SwitchGuard::ActiveMissing { active } => (active.clone(), false, Vec::new(), true),
        // A guard that costs nothing never becomes a pending switch.
        SwitchGuard::Clear => return,
    };
    let reapplying = target.eq_ignore_ascii_case(&active);
    let mut answered = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        if missing {
            ui.strong(format!(
                "'{active}' is no longer in the library, so the changes made since cannot be \
                 listed. Switching to '{target}' would replace them."
            ));
        } else if reapplying {
            ui.strong(format!(
                "Reapplying '{active}' would discard {} unsaved change(s) to it:",
                differences.len()
            ));
        } else {
            ui.strong(format!(
                "Switching to '{target}' would discard {} unsaved change(s) to '{active}':",
                differences.len()
            ));
        }
        list_differences(ui, &differences);
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if active_has_file
                && ui
                    .button(format!("Keep them - save into '{active}', then switch"))
                    .clicked()
            {
                let document = store.document().clone();
                let saved = state
                    .library(registry)
                    .overwrite(&active, &document, registry);
                match saved {
                    Ok(()) => {
                        state.apply_switch(&target, registry, store, outcome);
                        answered = true;
                    }
                    Err(error) => state.message = Some(format!("Refused: {error}")),
                }
            }
            // Always offered, and the only way to keep the changes when the
            // active profile is the shipped one, which has no file to save
            // into.
            let named = !state.new_name.trim().is_empty();
            let save_as = ui.add_enabled(
                named,
                egui::Button::new("Keep them - save as a new profile, then switch"),
            );
            if save_as.clicked() {
                let document = store.document().clone();
                let name = state.new_name.clone();
                let saved = state.library(registry).save_as(&name, &document, registry);
                match saved {
                    Ok(name) => {
                        state.new_name.clear();
                        state.apply_switch(&target, registry, store, outcome);
                        state.message = Some(format!(
                            "Saved the previous settings as '{name}', then switched to '{target}'."
                        ));
                        answered = true;
                    }
                    Err(error) => state.message = Some(format!("Refused: {error}")),
                }
            }
            if !named {
                ui.label(
                    egui::RichText::new("(type a name in the New profile box below)")
                        .small()
                        .weak(),
                );
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button("Discard them and switch").clicked() {
                state.apply_switch(&target, registry, store, outcome);
                answered = true;
            }
            if ui.button("Cancel - stay here").clicked() {
                state.pending = None;
                state.message = Some("Stayed on the current settings.".to_owned());
                answered = true;
            }
        });
    });
    if answered {
        state.pending = None;
    }
    ui.add_space(6.0);
}

fn draw_new_profile_block(
    ui: &mut egui::Ui,
    state: &mut ProfilesUi,
    registry: &SettingsRegistry,
    store: &SettingsStore,
) {
    ui.horizontal(|ui| {
        ui.label("New profile");
        ui.add_sized(
            [200.0, MIN_INTERACT_HEIGHT],
            egui::TextEdit::singleline(&mut state.new_name).hint_text("Chase, Office, ..."),
        );
        if ui.button("Save current settings as this").clicked() {
            let document = store.document().clone();
            let name = state.new_name.clone();
            let result = state
                .library(registry)
                .save_as(&name, &document, registry)
                .map(|name| {
                    state.new_name.clear();
                    format!("Saved the current settings as '{name}'. Switch to it to make it the active profile.")
                });
            state.report(result);
        }
    });
    ui.label(
        egui::RichText::new(
            "Saves every setting except the window's size and position and the site that was \
             last live - those belong to this computer rather than to a way of working.",
        )
        .small()
        .weak(),
    );
}

fn draw_profile_rows(
    ui: &mut egui::Ui,
    state: &mut ProfilesUi,
    registry: &SettingsRegistry,
    store: &mut SettingsStore,
    outcome: &mut SettingsOutcome,
) {
    let active = state.active_name(registry, store);
    // Cloned out of the library before the rows are drawn: every button in
    // the loop writes the library the rows are read from.
    let rows: Vec<(String, bool, Option<String>, Vec<String>)> = state
        .library(registry)
        .profiles()
        .iter()
        .map(|profile| {
            (
                profile.name.clone(),
                profile.is_shipped(),
                profile.file.as_ref().and_then(|file| {
                    file.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                }),
                profile
                    .faults
                    .iter()
                    .map(profiles::ProfileNote::message)
                    .collect(),
            )
        })
        .collect();

    // Not "Profiles": the page is already called that, and a heading that
    // repeats the page's name tells a reader nothing about what is under it.
    ui.strong("Saved profiles");
    ui.add_space(2.0);
    for (name, shipped, file, faults) in rows {
        let is_active = name.eq_ignore_ascii_case(&active);
        ui.horizontal_wrapped(|ui| {
            let title = if is_active {
                format!("{name}  (active)")
            } else {
                name.clone()
            };
            ui.label(egui::RichText::new(title).strong());
            let switch_label = if is_active { "Reapply" } else { "Switch to it" };
            if ui.button(switch_label).clicked() {
                state.request_switch(&name, registry, store, outcome);
            }
            if !shipped {
                if ui.button("Overwrite with current").clicked() {
                    let document = store.document().clone();
                    let result = state
                        .library(registry)
                        .overwrite(&name, &document, registry)
                        .map(|()| format!("'{name}' now holds the current settings."));
                    state.report(result);
                }
                if ui.button("Rename").clicked() {
                    state.renaming = Some((name.clone(), name.clone()));
                }
            }
            if ui.button("Duplicate").clicked() {
                let result = state
                    .library(registry)
                    .duplicate(&name, registry)
                    .map(|copy| format!("Copied '{name}' to '{copy}'."));
                state.report(result);
            }
            if !shipped && ui.button("Delete").clicked() {
                let result = state
                    .library(registry)
                    .delete(&name, registry)
                    .map(|()| format!("Deleted '{name}'. Its file is gone from the folder."));
                state.report(result);
            }
        });
        if shipped {
            ui.label(
                egui::RichText::new(
                    "How this build behaves with nothing stored. Always here, and cannot be \
                     deleted or written over - it is the way back to a known state.",
                )
                .small()
                .weak(),
            );
        } else if let Some(file) = &file {
            ui.label(egui::RichText::new(format!("file: {file}")).small().weak());
        }
        for fault in &faults {
            ui.label(
                egui::RichText::new(format!("this build cannot use: {fault}"))
                    .small()
                    .weak(),
            );
        }
        draw_rename_row(ui, state, registry, &name);
        ui.add_space(6.0);
    }
}

fn draw_rename_row(
    ui: &mut egui::Ui,
    state: &mut ProfilesUi,
    registry: &SettingsRegistry,
    name: &str,
) {
    let renaming = state
        .renaming
        .as_ref()
        .is_some_and(|(target, _)| target == name);
    if !renaming {
        return;
    }
    let mut edited = state
        .renaming
        .as_ref()
        .map(|(_, edited)| edited.clone())
        .unwrap_or_default();
    let mut finish: Option<bool> = None;
    ui.horizontal(|ui| {
        ui.label("Rename to");
        ui.add_sized(
            [200.0, MIN_INTERACT_HEIGHT],
            egui::TextEdit::singleline(&mut edited),
        );
        if ui.button("Rename").clicked() {
            finish = Some(true);
        }
        if ui.button("Cancel").clicked() {
            finish = Some(false);
        }
    });
    match finish {
        Some(true) => {
            let result = state
                .library(registry)
                .rename(name, &edited, registry)
                .map(|new_name| format!("'{name}' is now called '{new_name}'."));
            state.report(result);
            state.renaming = None;
        }
        Some(false) => state.renaming = None,
        None => {
            if let Some((_, stored)) = state.renaming.as_mut() {
                *stored = edited;
            }
        }
    }
}

fn draw_broken_rows(ui: &mut egui::Ui, state: &mut ProfilesUi, registry: &SettingsRegistry) {
    let broken: Vec<(std::path::PathBuf, String)> = state
        .library(registry)
        .broken()
        .iter()
        .map(|broken| (broken.file.clone(), broken.reason.clone()))
        .collect();
    if broken.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.strong("Files in the folder that are not usable profiles");
    ui.label(
        egui::RichText::new(
            "Left where they are rather than deleted: the file is yours and may hold an edit \
             worth rescuing. Fix it in a text editor, or clear it here.",
        )
        .small()
        .weak(),
    );
    for (file, reason) in broken {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!("{} - {reason}", file.display()))
                    .small()
                    .weak(),
            );
            if ui.button("Delete the file").clicked() {
                let result = state
                    .library(registry)
                    .delete_file(&file, registry)
                    .map(|()| format!("Deleted {}.", file.display()));
                state.report(result);
            }
        });
    }
}

fn draw_footer(ui: &mut egui::Ui, state: &mut ProfilesUi, registry: &SettingsRegistry) {
    ui.add_space(8.0);
    if let Some(message) = state.message.clone() {
        ui.label(message);
    }
    for note in state.switch_notes.clone() {
        ui.label(egui::RichText::new(format!("  {note}")).small().weak());
    }
    let (directory, error) = {
        let library = state.library(registry);
        (
            library.directory().display().to_string(),
            library.directory_error().map(str::to_owned),
        )
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("Profiles are one file each in {directory}"))
                .small()
                .weak(),
        );
        if ui.button("Reload from the folder").clicked() {
            state.library(registry).rescan(registry);
            state.message = Some("Read the profiles folder again.".to_owned());
        }
    });
    if let Some(error) = error {
        ui.label(
            egui::RichText::new(format!("The folder could not be read: {error}"))
                .small()
                .weak(),
        );
    }
}

fn list_differences(ui: &mut egui::Ui, differences: &[String]) {
    for difference in differences.iter().take(MAX_LISTED_DIFFERENCES) {
        ui.label(egui::RichText::new(format!("  - {difference}")).small());
    }
    if differences.len() > MAX_LISTED_DIFFERENCES {
        ui.label(
            egui::RichText::new(format!(
                "  and {} more",
                differences.len() - MAX_LISTED_DIFFERENCES
            ))
            .small()
            .weak(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page's own arithmetic, away from egui: the summary line the File
    /// menu shows has to name the active profile and say whether the live
    /// settings have moved away from it.
    #[test]
    fn the_summary_names_the_active_profile_and_whether_it_has_been_modified() {
        let directory = std::env::temp_dir().join(format!(
            "radar-profiles-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after 1970")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("scratch");
        let registry = super::super::catalog::registry();
        let mut store = settings::SettingsStore::open(directory.join("settings.json"));

        // The library is supplied rather than lazily opened, so the test
        // never resolves - and never fixes - the real config root.
        let mut state = ProfilesUi {
            library: Some(ProfileLibrary::open(
                directory.join("profiles"),
                SettingsDocument::default(),
                &registry,
            )),
            ..Default::default()
        };
        assert_eq!(state.summary(&registry, &store), None);

        // A stored value with no profile named: measured against the shipped
        // profile, and it has moved.
        store.set("map", "site_markers", settings::SettingValue::Bool(false));
        let summary = state.summary(&registry, &store).expect("a summary");
        assert_eq!(summary.0, profiles::SHIPPED_NAME);
        assert!(summary.1, "it differs from the shipped profile");

        // Saved and switched to: named, and not modified.
        let document = store.document().clone();
        state
            .library(&registry)
            .save_as("Chase", &document, &registry)
            .expect("save");
        let merged = profiles::merge_for_switch(store.document(), &document, "Chase");
        store.replace_document(merged);
        let summary = state.summary(&registry, &store).expect("a summary");
        assert_eq!(summary.0, "Chase");
        assert!(!summary.1, "straight after a switch nothing differs");

        // One change later: modified.
        store.set("map", "site_markers", settings::SettingValue::Bool(true));
        let summary = state.summary(&registry, &store).expect("a summary");
        assert_eq!(summary.0, "Chase");
        assert!(summary.1);

        let _ = std::fs::remove_dir_all(&directory);
    }
}
