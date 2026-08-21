//! Whether a photograph harness may open a window, and who decides.
//!
//! Several examples in this workspace exist to take a picture of real chrome
//! through the real `eframe` pipeline: they build a viewport, pump a handful
//! of frames so fonts rasterise and layout settles, fire
//! `ViewportCommand::Screenshot`, write a PNG and close. The picture is the
//! product; the window is the canvas it has to be painted on.
//!
//! **A harness that photographs through `eframe::run_native` puts a real,
//! focused window on whatever display it is started on, and there is no flag
//! in this workspace that changes that.** Two places in `eframe` 0.34.3 decide
//! it, and neither consults the caller:
//!
//! * `create_window` in `src/native/wgpu_integration.rs` sets the viewport
//!   builder's own `visible` flag to false before creating the window, so
//!   whatever the caller passed is overwritten - every `eframe` app starts
//!   hidden, and a caller that asks for hidden has asked for nothing;
//! * `EpiIntegration::post_rendering` in `src/native/epi_integration.rs` then
//!   calls `window.set_visible(true)` once the first frame has been painted,
//!   with no condition attached. It is reached on Windows: the `is_visible`
//!   gate around it reads `ViewportInfo::visible()`, which is a function of
//!   `minimized` and `occluded` only, and `winit` does not emit `Occluded` on
//!   Windows, so it stays `None` and defaults to true.
//!
//! Asking the window to hide itself again from inside the app does not fix it
//! either. `ViewportCommand::Visible(false)` is applied by
//! `handle_viewport_output`, which runs *after* `post_rendering` in the same
//! frame, so the window would be shown and then hidden: a flash on somebody's
//! screen, not an absence of one.
//!
//! So the rule here is not "suppress the window". It is: **a harness that
//! cannot take its picture without a window does not get to open one by
//! accident.** Run one with no arguments and it refuses to start, and says
//! why. A window costs the operator [`WINDOW_FLAG`], typed on purpose, at a
//! machine where a window appearing is expected.
//!
//! That inverts the failure mode this file exists for. Forgetting the flag now
//! costs a picture nobody has taken yet, instead of costing whoever is using
//! that machine the use of their screen.
//!
//! A harness that renders *without* a window - `theme_gallery`'s default mode
//! builds a `wgpu` device with no surface and never calls `run_native` - needs
//! none of this, and asks [`requested_by_process`] only so that the flag has
//! one spelling across the workspace.

/// The one argument that buys a window on the screen.
///
/// Spelled out rather than inferred from "no other arguments were given",
/// because an inferred rule is one a tired reader gets wrong in the direction
/// that hurts.
pub const WINDOW_FLAG: &str = "--window";

/// Whether the operator asked for a window by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowRequest {
    /// No [`WINDOW_FLAG`] in the arguments. The default, deliberately: it is
    /// the answer that cannot cost anyone their screen.
    #[default]
    NotAsked,
    /// A window, because someone asked for one by name.
    Asked,
}

/// Read the request out of a caller's arguments.
///
/// Anything that is not [`WINDOW_FLAG`] is somebody else's argument - an
/// output path, a preset name - and is left alone.
pub fn requested<I, S>(arguments: I) -> WindowRequest
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if arguments
        .into_iter()
        .any(|argument| argument.as_ref() == WINDOW_FLAG)
    {
        WindowRequest::Asked
    } else {
        WindowRequest::NotAsked
    }
}

/// The same question asked of this process's own command line.
#[must_use]
pub fn requested_by_process() -> WindowRequest {
    requested(std::env::args().skip(1))
}

/// This process's arguments with [`WINDOW_FLAG`] removed, so a harness that
/// reads positional arguments does not mistake the flag for a file name.
#[must_use]
pub fn positional_arguments() -> Vec<String> {
    strip_window_flag(std::env::args().skip(1))
}

/// [`positional_arguments`] without the process: the part worth testing.
pub fn strip_window_flag<I, S>(arguments: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    arguments
        .into_iter()
        .filter(|argument| argument.as_ref() != WINDOW_FLAG)
        .map(|argument| argument.as_ref().to_owned())
        .collect()
}

/// What a harness prints instead of opening a window nobody asked for.
///
/// Kept separate from the exit so it can be read in a test, and written to be
/// read by somebody who has just been refused: it says what would have
/// happened, why the harness will not do it quietly, and the exact line to
/// type when a window is wanted.
#[must_use]
pub fn refusal(harness: &str, usage: &str) -> String {
    format!(
        "{harness} takes its photograph through a real eframe window, and eframe maps that \
         window onto the display as soon as the first frame is painted. There is no way to run \
         it without a window appearing, so it will not start unless the window was asked for.\n\
         \n\
         Nothing has been started and nothing has been written.\n\
         \n\
         To run it, add {WINDOW_FLAG} - at a machine where a window appearing is expected, or on \
         a desktop of your own:\n\
         \n    {usage}\n"
    )
}

/// Refuse to start unless a window was asked for by name.
///
/// For the harnesses that cannot do their job without one. The harness calls
/// this before it decodes anything, so a refusal costs nothing and reads as
/// the first thing that happens.
pub fn require_window_or_exit(harness: &str, usage: &str) {
    if requested_by_process() == WindowRequest::Asked {
        return;
    }
    eprint!("{}", refusal(harness, usage));
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_is_not_a_request_for_a_window() {
        assert_eq!(requested::<[&str; 0], &str>([]), WindowRequest::NotAsked);
        assert_eq!(
            requested(["volume-file", "out.png", "imperial"]),
            WindowRequest::NotAsked
        );
    }

    #[test]
    fn a_window_has_to_be_asked_for_by_name() {
        assert_eq!(requested([WINDOW_FLAG]), WindowRequest::Asked);
        assert_eq!(
            requested(["volume-file", "out.png", WINDOW_FLAG]),
            WindowRequest::Asked
        );
    }

    /// A near miss is not a request. `--windowed`, `-window` and `window` are
    /// all somebody's positional argument as far as this is concerned, and the
    /// safe reading of an argument nobody recognises is "no window".
    #[test]
    fn a_near_miss_is_not_a_request() {
        for argument in ["--windowed", "-window", "window", "--show", ""] {
            assert_eq!(
                requested([argument]),
                WindowRequest::NotAsked,
                "{argument:?} should not have bought a window"
            );
        }
    }

    #[test]
    fn the_default_is_no_window() {
        assert_eq!(WindowRequest::default(), WindowRequest::NotAsked);
    }

    #[test]
    fn the_flag_is_not_left_in_the_positional_arguments() {
        assert_eq!(
            strip_window_flag(["volume-file", WINDOW_FLAG, "out.png"]),
            vec!["volume-file".to_owned(), "out.png".to_owned()]
        );
        assert_eq!(
            strip_window_flag(["volume-file", "out.png"]),
            vec!["volume-file".to_owned(), "out.png".to_owned()]
        );
    }

    /// The refusal has one job beyond stopping: telling the reader how to get
    /// what they wanted. A message that omits the flag or the usage line sends
    /// them to the source to find out.
    #[test]
    fn the_refusal_says_how_to_ask_for_the_window() {
        let message = refusal("pane_proof", "pane_proof <volume> <out.png> --window");
        assert!(message.contains("pane_proof"), "{message}");
        assert!(message.contains(WINDOW_FLAG), "{message}");
        assert!(
            message.contains("pane_proof <volume> <out.png> --window"),
            "{message}"
        );
    }

    /// And it must not promise the thing this module cannot deliver. A
    /// refusal that offers to hide the window is the false claim that made
    /// this file necessary; the whole message is "there will be a window".
    ///
    /// The workspace-wide version of this check, over every source file and
    /// document rather than one string, is
    /// `tests/harness_windows.rs::nothing_claims_a_harness_window_is_hidden`.
    #[test]
    fn the_refusal_does_not_offer_to_hide_the_window() {
        let message = refusal("pane_proof", "usage").to_ascii_lowercase();
        for word in ["invisible", "hidden", "suppress", "offscreen"] {
            assert!(
                !message.contains(word),
                "the refusal says {word:?}, which promises something eframe will not do"
            );
        }
    }
}
