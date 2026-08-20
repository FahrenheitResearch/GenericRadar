//! The analyst's own colour tables, in the application.
//!
//! `color_tables::user` does the reading; this is the half that knows where
//! the folder is, when to look at it again, and how to say what happened.
//!
//! **Where.** `settings::app_config_root()/colortables` - the same root the
//! settings file resolves against, so all of the application's own state is
//! in one place and a mobile shell that injects a sandbox root moves the
//! colour tables with everything else. The path is derived here rather than
//! spelled out anywhere, and it is the contract a colour table editor writes
//! against: save a `.pal` into this folder and the next scan picks it up.
//!
//! **When.** Three moments, and no polling:
//!
//! * once at startup, before the first palette is resolved;
//! * whenever the window regains keyboard focus, because the way an analyst
//!   edits a palette is to alt-tab to a text editor and come back;
//! * after a drop, which is the only change this application makes itself.
//!
//! A file watcher would be a fourth, and a worse one: it costs a thread and
//! a platform-specific dependency to catch a case the focus rescan already
//! catches a second later.
//!
//! **What it costs.** The focus rescan runs on the UI thread, between two
//! frames, so it has to be free. It is: `UserTableLibrary::refresh` compares
//! the folder's listing against the one it read last and stops there when
//! nothing moved, so an alt-tab out and back is one directory listing (tens
//! of microseconds) rather than a re-read and re-parse of every file in the
//! folder. A file stamped too recently for that scan to vouch for is read
//! again anyway, so the ordinary case of editing a palette twice in quick
//! succession is caught; what a listing still cannot see is a change that
//! keeps the byte count AND puts the timestamp back, which is what a
//! timestamp-preserving copy does. The Rescan button in Settings > Radar is
//! the escape hatch for exactly that, and goes through
//! `UserTableLibrary::reread`.
//!
//! **What a drop does not do.** It does not install the table. A dropped
//! palette joins its family's list and says so; which palette is on screen
//! stays the analyst's choice, made in the picker. A drop that repainted the
//! pane would overrule a deliberate pick - and for a reflectivity palette
//! dropped while the pane shows velocity it would repaint nothing visible
//! anyway, which is worse: a control that sometimes acts and sometimes does
//! not.
//!
//! **How it says so.** A drop answers in a status line - the table's name
//! and family when it loaded, the parser's own reason and line number when
//! it did not. It is a floating notice rather than the timeline's status
//! string because that string is only shown while no volume is loaded, and
//! the moment an analyst drops a palette is exactly the moment a volume IS
//! loaded. Standing faults (files sitting in the folder that do not parse)
//! belong in the settings window instead, where they can be read at leisure;
//! see `settings_ui::draw_user_tables_section`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use color_tables::user::{USER_TABLE_EXTENSIONS, UserTableLibrary};
use eframe::egui;

/// How long a drop notice stays up before it fades on its own.
///
/// Long enough to read a parse error with a line number in it, short enough
/// that it is gone before it becomes furniture. Dismissable at any point.
const NOTICE_LIFETIME: Duration = Duration::from_secs(9);

/// Minimum hit-target height for the notice's dismiss control, in points -
/// the same touch floor the settings window holds itself to.
const MIN_INTERACT_HEIGHT: f32 = 24.0;

/// The folder the analyst's colour tables live in.
///
/// One line over `settings::user_colortables_dir`, which is the single place
/// the path is spelled. This folder is read by the scanner below, written by
/// the colour table editor (`crate::palette_editor::store`) and searched at
/// launch by `crate::settings_ui::palettes`; three spellings of one path is
/// three chances for a table to be saved where nothing looks for it, so
/// there is one, and it lives in `settings` because that crate owns every
/// path the application uses and is the only one all three can see.
///
/// Derived there from `settings::app_config_root()`, never hardcoded: on iOS
/// the only process that knows the sandbox path is the shell, which injects
/// it before the UI starts (`settings::set_app_config_root`).
pub fn user_tables_dir() -> PathBuf {
    settings::user_colortables_dir()
}

/// Whether a dropped path should be read as a colour table rather than as a
/// radar volume.
///
/// Keyed on exactly the extensions the scanner reads, so "what the folder
/// holds" and "what a drop accepts" cannot drift apart. A Level II volume
/// carries no extension at all (`KDVN20260819_150217_V06`) or a `.gz`, so
/// nothing that belongs on the load path is caught here.
pub fn is_colour_table_drop(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| USER_TABLE_EXTENSIONS.contains(&extension.as_str()))
}

/// Split one drop into the colour tables in it and everything else.
///
/// A drop can carry several files, and a handful of palettes is exactly the
/// kind of thing an analyst drags in one go, so every colour table is kept.
/// The rest are returned in the order they were dropped rather than reduced
/// to one here: a pane draws one volume, but WHICH of the remaining paths is
/// the volume is the load path's judgement to make
/// ([`crate::app_support::choose_dropped_radar_file`] looks at the names),
/// and this function has no business guessing on its behalf.
pub fn split_drop(paths: impl IntoIterator<Item = PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut tables = Vec::new();
    let mut rest = Vec::new();
    for path in paths {
        if is_colour_table_drop(&path) {
            tables.push(path);
        } else {
            rest.push(path);
        }
    }
    (tables, rest)
}

/// The application's view of the folder: what is in it, and what the last
/// drop had to say about it.
pub struct UserTables {
    library: UserTableLibrary,
    notice: Option<Notice>,
    /// Focus as of the previous frame, so the rescan fires on the EDGE.
    /// Rescanning every focused frame would stat the folder at the frame
    /// rate.
    was_focused: bool,
}

struct Notice {
    text: String,
    /// Whether the drop succeeded, which decides whether the notice reads as
    /// information or as a problem.
    trouble: bool,
    raised: Instant,
}

impl Default for UserTables {
    fn default() -> Self {
        Self::open(user_tables_dir())
    }
}

impl UserTables {
    /// Scan a directory now. Public for tests and for a shell that resolves
    /// its own folder; the application uses [`UserTables::default`].
    pub fn open(directory: impl Into<PathBuf>) -> Self {
        Self {
            library: UserTableLibrary::open(directory),
            notice: None,
            // egui's `RawInput` starts focused, so starting anywhere else
            // would fire a rescan on the first frame for no reason.
            was_focused: true,
        }
    }

    pub fn library(&self) -> &UserTableLibrary {
        &self.library
    }

    /// Re-read the folder because somebody asked in so many words - the
    /// Rescan button. Unlike the focus rescan this does not trust the
    /// folder's listing: an explicit instruction gets an actual read.
    pub fn rescan(&mut self) {
        self.library.reread();
    }

    /// Re-read the folder when the window has just come back to the front.
    ///
    /// Returns whether the folder's contents actually moved, not merely
    /// whether the window regained focus, so the caller re-resolves palettes
    /// only when there is something new to resolve against. An alt-tab onto
    /// an untouched folder costs one directory listing and nothing else.
    pub fn poll_focus(&mut self, context: &egui::Context) -> bool {
        let focused = context.input(|input| input.raw.focused);
        let regained = focused && !self.was_focused;
        self.was_focused = focused;
        regained && self.library.refresh()
    }

    /// Import every dropped colour table, in the order they were dropped.
    /// Returns whether anything landed in the folder, so the caller can
    /// re-resolve its palettes.
    pub fn import_all(&mut self, paths: &[PathBuf]) -> bool {
        let mut loaded = false;
        let mut lines = Vec::new();
        let mut trouble = false;
        for path in paths {
            // Every outcome becomes a line, success or not. A drop that
            // reports nothing is the failure mode this whole notice exists
            // to prevent.
            let outcome = self.library.import(path);
            loaded |= outcome.is_loaded();
            // Not `!is_loaded()`: a drop of a palette that is already in the
            // folder filed nothing and is still perfectly fine, so it must
            // not paint the notice in the warning colour.
            trouble |= outcome.is_problem();
            lines.push(outcome.status_line());
        }
        if !lines.is_empty() {
            self.raise(lines.join("  |  "), trouble);
        }
        loaded
    }

    /// Put a line in front of the analyst. Used by the caller for the part
    /// of a drop this module does not own - "installed into Velocity".
    pub fn raise(&mut self, text: String, trouble: bool) {
        self.notice = Some(Notice {
            text,
            trouble,
            raised: Instant::now(),
        });
    }

    /// The notice currently up, for tests and for a caller that wants to
    /// echo it somewhere else.
    pub fn notice_text(&self) -> Option<&str> {
        self.notice.as_ref().map(|notice| notice.text.as_str())
    }

    /// Draw the drop notice, if there is one. Cheap when there is not.
    ///
    /// Repaint is requested for the moment the notice expires and at no
    /// other time, so a notice on screen does not turn the application into
    /// a spinning redraw.
    pub fn draw_notice(&mut self, context: &egui::Context) {
        let Some(notice) = &self.notice else {
            return;
        };
        let elapsed = notice.raised.elapsed();
        if elapsed >= NOTICE_LIFETIME {
            self.notice = None;
            return;
        }
        let text = notice.text.clone();
        let trouble = notice.trouble;
        let mut dismissed = false;
        egui::Area::new(egui::Id::new("workstation-user-table-notice"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 64.0))
            .show(context, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(680.0);
                    ui.horizontal_wrapped(|ui| {
                        let colour = if trouble {
                            ui.visuals().warn_fg_color
                        } else {
                            ui.visuals().strong_text_color()
                        };
                        ui.label(egui::RichText::new(&text).color(colour));
                    });
                    // A visible control, not a hover affordance and not a
                    // click-anywhere: hover does not exist on glass.
                    if ui
                        .add_sized([90.0, MIN_INTERACT_HEIGHT], egui::Button::new("Dismiss"))
                        .clicked()
                    {
                        dismissed = true;
                    }
                });
            });
        if dismissed {
            self.notice = None;
        } else {
            context.request_repaint_after(NOTICE_LIFETIME - elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn scratch_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("workstation-user-tables")
            .join(format!(
                "{test}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock after 1970")
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    const VELOCITY_PAL: &str = "\
Product: BV
Units: KTS
Color: -60 200   0 200    60 220 220
Color:  60 220  60  60   255 255 255
";

    /// The same palette with the last row's own colour nudged, and exactly
    /// as many bytes: the edit a directory listing cannot see by length.
    /// (In this dialect a row names its own colour first and the far end of
    /// its interval second; the stop takes the first.)
    const VELOCITY_PAL_EDITED: &str = "\
Product: BV
Units: KTS
Color: -60 200   0 200    60 220 220
Color:  60 219  60  60   255 255 255
";

    #[test]
    fn the_folder_hangs_off_the_same_root_the_settings_file_does() {
        // The shared contract with anything that writes a palette for this
        // application to read: the settings root, then `colortables`.
        let root = settings::app_config_root();
        let directory = user_tables_dir();
        assert_eq!(directory.parent(), Some(root.as_path()));
        assert!(directory.ends_with("colortables"), "{directory:?}");
        assert_eq!(
            settings::default_settings_file().parent(),
            Some(root.as_path()),
            "the settings file and the colour tables must share a root"
        );
    }

    /// The scanner, the colour table editor and the launch-time restore each
    /// name this folder through their own front door, and all three doors
    /// have to open on one room. They used to be two independent
    /// `app_config_root().join("colortables")` expressions; a table saved
    /// where nothing looked for it is silent and total, so the two are one
    /// function now and this is the pin that keeps them one.
    #[test]
    fn the_scanner_the_editor_and_settings_name_one_folder() {
        assert_eq!(user_tables_dir(), settings::user_colortables_dir());
        assert_eq!(
            user_tables_dir(),
            crate::palette_editor::store::user_colortables_dir(),
            "the editor writes where the scanner reads"
        );
    }

    #[test]
    fn a_drop_is_read_as_a_colour_table_only_for_the_extensions_the_scanner_reads() {
        assert!(is_colour_table_drop(Path::new("mine.pal")));
        assert!(is_colour_table_drop(Path::new("MINE.PAL")));
        assert!(is_colour_table_drop(Path::new("shared.txt")));
        // What a Level II volume looks like, in both shapes it arrives in.
        assert!(!is_colour_table_drop(Path::new("KDVN20260819_150217_V06")));
        assert!(!is_colour_table_drop(Path::new(
            "KDVN20260819_150217_V06.gz"
        )));
    }

    #[test]
    fn a_drop_of_several_files_keeps_every_palette_and_one_volume() {
        // The regression this guards: routing the whole drop to the load
        // path, which is what happened before colour tables were droppable
        // and would have made a dragged palette a failed volume decode.
        let (tables, rest) = split_drop([
            PathBuf::from("a.pal"),
            PathBuf::from("KDVN20260819_150217_V06"),
            PathBuf::from("b.txt"),
            PathBuf::from("KDVN20260819_151500_V06"),
        ]);
        assert_eq!(tables, [PathBuf::from("a.pal"), PathBuf::from("b.txt")]);
        // Every non-palette path survives, in drop order: choosing between
        // them belongs to the load path, not here.
        assert_eq!(
            rest,
            [
                PathBuf::from("KDVN20260819_150217_V06"),
                PathBuf::from("KDVN20260819_151500_V06"),
            ]
        );

        // A drop with no palette in it is exactly what it was before.
        let (tables, rest) = split_drop([PathBuf::from("KDVN20260819_150217_V06")]);
        assert!(tables.is_empty());
        assert_eq!(rest, [PathBuf::from("KDVN20260819_150217_V06")]);
    }

    #[test]
    fn a_dropped_table_lands_in_the_folder_and_says_which_family_it_joined() {
        let folder = scratch_dir("drop-folder");
        let elsewhere = scratch_dir("drop-source");
        let source = elsewhere.join("Ramp Velocity.pal");
        std::fs::write(&source, VELOCITY_PAL).expect("write palette");

        let mut tables = UserTables::open(&folder);
        assert!(tables.import_all(&[source]));
        assert!(folder.join("Ramp Velocity.pal").is_file());
        let notice = tables.notice_text().expect("a drop says something");
        assert!(notice.contains("Ramp Velocity"), "{notice}");
        assert!(notice.contains("Velocity"), "{notice}");
        assert_eq!(tables.library().tables().len(), 1);
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn dropping_the_same_file_twice_files_it_once_and_says_it_is_already_there() {
        let folder = scratch_dir("drop-twice-folder");
        let elsewhere = scratch_dir("drop-twice-source");
        let source = elsewhere.join("Ramp Velocity.pal");
        std::fs::write(&source, VELOCITY_PAL).expect("write palette");

        let mut tables = UserTables::open(&folder);
        assert!(tables.import_all(std::slice::from_ref(&source)));

        // The same file again. Nothing new is filed, and - the part that
        // matters for the notice - this is reported as information, not as
        // trouble, because the analyst has done nothing wrong.
        assert!(
            !tables.import_all(std::slice::from_ref(&source)),
            "a duplicate loads nothing new, so nothing needs re-resolving"
        );
        let notice = tables.notice_text().expect("a duplicate drop still speaks");
        assert!(notice.contains("already imported as"), "{notice}");
        assert!(notice.contains("Ramp Velocity"), "{notice}");
        assert_eq!(tables.library().tables().len(), 1);
        assert!(!folder.join("Ramp Velocity (2).pal").exists());
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_dropped_file_that_does_not_parse_says_so_with_its_line_and_is_not_filed() {
        let folder = scratch_dir("drop-broken-folder");
        let elsewhere = scratch_dir("drop-broken-source");
        let source = elsewhere.join("bad.pal");
        std::fs::write(&source, "Product: BV\nColor: 0 0 0 0\nColor: 5 0 0 999\n")
            .expect("write palette");

        let mut tables = UserTables::open(&folder);
        assert!(!tables.import_all(std::slice::from_ref(&source)));
        let notice = tables.notice_text().expect("a failed drop still speaks");
        assert!(notice.contains("bad.pal"), "{notice}");
        assert!(notice.contains("line 3"), "{notice}");
        assert!(tables.library().tables().is_empty());
        assert!(!folder.join("bad.pal").exists());
        assert!(source.is_file(), "the analyst's file was moved or deleted");
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_focus_rescan_fires_on_the_edge_and_picks_up_a_file_written_behind_our_back() {
        let folder = scratch_dir("focus");
        let mut tables = UserTables::open(&folder);
        assert!(tables.library().tables().is_empty());

        let context = egui::Context::default();
        // A frame while focused - egui's default - changes nothing.
        assert!(!tables.poll_focus(&context));

        // The analyst alt-tabs to a text editor, writes a palette, comes
        // back. The scan happens on the way back, not on the way out.
        let unfocused = egui::RawInput {
            focused: false,
            ..Default::default()
        };
        let _ = context.run_ui(unfocused, |_| {});
        assert!(!tables.poll_focus(&context));
        std::fs::write(folder.join("Written Elsewhere.pal"), VELOCITY_PAL).expect("write palette");
        assert!(tables.library().tables().is_empty(), "no polling happens");

        let _ = context.run_ui(egui::RawInput::default(), |_| {});
        assert!(tables.poll_focus(&context), "the edge must fire a rescan");
        assert_eq!(tables.library().tables().len(), 1);
        // And only on the edge: a second focused frame does not rescan.
        assert!(!tables.poll_focus(&context));
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_rescan_button_picks_up_the_one_edit_the_focus_rescan_cannot_see() {
        // Where the Settings > Radar "Rescan colour table folder" button
        // lands (`SettingsOutcome::user_tables_rescan` -> `app::
        // rescan_user_tables` -> here). It is the analyst's way out of the
        // one change a directory listing cannot see - the same byte count
        // under a timestamp that was put back, which is what a
        // timestamp-preserving copy or an archive extraction produces - so
        // it must NOT go through the same listing comparison the focus
        // rescan does.
        let folder = scratch_dir("rescan-button");
        let path = folder.join("Ramp Velocity.pal");
        std::fs::write(&path, VELOCITY_PAL).expect("write palette");
        // An ordinary file saved a while back, so the listing is trusted and
        // this is not about the racy-timestamp guard.
        set_modified(&path, SystemTime::now() - Duration::from_secs(60));

        let mut tables = UserTables::open(&folder);
        assert_eq!(last_velocity_colour(&tables), (220, 60, 60));

        let stamp = modified_of(&path);
        assert_eq!(VELOCITY_PAL.len(), VELOCITY_PAL_EDITED.len());
        std::fs::write(&path, VELOCITY_PAL_EDITED).expect("edit palette");
        set_modified(&path, stamp);

        // Alt-tab away and back: the listing says nothing happened, and by
        // contract it is right as far as it can see.
        let context = egui::Context::default();
        let unfocused = egui::RawInput {
            focused: false,
            ..Default::default()
        };
        let _ = context.run_ui(unfocused, |_| {});
        assert!(!tables.poll_focus(&context));
        let _ = context.run_ui(egui::RawInput::default(), |_| {});
        assert!(
            !tables.poll_focus(&context),
            "a listing cannot see this edit, which is why the button exists"
        );
        assert_eq!(last_velocity_colour(&tables), (220, 60, 60));

        // The button.
        tables.rescan();
        assert_eq!(
            last_velocity_colour(&tables),
            (219, 60, 60),
            "one click on Rescan has to put the analyst's own edit on screen"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The colour of the velocity table's last stop - what the edit above
    /// moves, and so the proof that the file was read again.
    fn last_velocity_colour(tables: &UserTables) -> (u8, u8, u8) {
        let colour = tables
            .library()
            .table_for_family_named(color_tables::ColorTableFamily::Velocity, "Ramp Velocity")
            .expect("the folder holds Ramp Velocity")
            .stops()
            .last()
            .expect("stops exist")
            .color;
        (colour.r, colour.g, colour.b)
    }

    fn modified_of(path: &Path) -> SystemTime {
        std::fs::metadata(path)
            .expect("stat fixture")
            .modified()
            .expect("this platform records modification times")
    }

    fn set_modified(path: &Path, when: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open fixture")
            .set_modified(when)
            .expect("this platform sets modification times");
    }
}
