//! When a toolbar popup should close.
//!
//! This module exists because of a specific bug that made the product picker
//! completely unusable: clicking the product button did nothing at all.
//!
//! The toolbar opened the popup from the button's `clicked()` and then, IN THE
//! SAME FRAME, closed it again on `response.clicked_elsewhere()`. That reads as
//! correct until you notice the click being tested is the very click that
//! opened the popup - and it landed on the button, which is "elsewhere" as far
//! as the popup's own rectangle is concerned. So the popup opened and closed
//! inside one frame, forever, and the only symptom was a button that appeared
//! to be dead.
//!
//! The lesson is that a dismissal rule needs to know about the click that
//! opened it, and a rule spread across four lines of a 240-line toolbar closure
//! is a rule nobody can test. So the decision lives here as a value, and the
//! case that broke is the first test in the file.

use eframe::egui;

/// Everything that bears on whether an open popup should close this frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PopupDismissal {
    /// The popup was opened by a click on THIS frame.
    ///
    /// The field that makes the rule correct. While it is set, no
    /// pointer-outside test may close the popup, because the only press this
    /// frame is the one that asked for it to be open.
    pub opened_this_frame: bool,
    /// The popup itself asked to close - a row was chosen, or Escape was
    /// pressed inside it.
    pub explicitly_dismissed: bool,
    /// A pointer button went down somewhere this frame.
    pub pointer_pressed: bool,
    /// That press was inside the popup's own rectangle.
    pub pointer_inside_popup: bool,
    /// That press was inside the button that opens the popup.
    ///
    /// Also load-bearing, and for a second reason: the button toggles. A press
    /// on it while the popup is open is a request to close, which the button's
    /// own `clicked()` already handles - so this rule must not close it a
    /// second time, or the two together would close and reopen.
    pub pointer_inside_button: bool,
}

impl PopupDismissal {
    /// True when the popup should close.
    pub fn should_close(self) -> bool {
        if self.explicitly_dismissed {
            return true;
        }
        if self.opened_this_frame {
            return false;
        }
        self.pointer_pressed && !self.pointer_inside_popup && !self.pointer_inside_button
    }
}

/// Read the pointer facts for a popup from egui.
///
/// `popup` is the rectangle the popup drew into and `button` the widget that
/// opens it. A press with no position - which is what a touch release or a
/// synthetic event can look like - counts as inside both, so an event the
/// caller cannot place never closes anything.
pub fn dismissal_from_input(
    ctx: &egui::Context,
    popup: egui::Rect,
    button: egui::Rect,
    opened_this_frame: bool,
    explicitly_dismissed: bool,
) -> PopupDismissal {
    let (pointer_pressed, position) =
        ctx.input(|input| (input.pointer.any_pressed(), input.pointer.interact_pos()));
    let escape = ctx.input(|input| input.key_pressed(egui::Key::Escape));
    PopupDismissal {
        opened_this_frame,
        explicitly_dismissed: explicitly_dismissed || escape,
        pointer_pressed,
        pointer_inside_popup: position.is_none_or(|at| popup.contains(at)),
        pointer_inside_button: position.is_none_or(|at| button.contains(at)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module was written for. A popup opened by a click on the
    /// button must survive that same click.
    #[test]
    fn the_click_that_opens_a_popup_does_not_also_close_it() {
        let opening = PopupDismissal {
            opened_this_frame: true,
            explicitly_dismissed: false,
            // The press really did happen, and it really was outside the popup:
            // it was on the button. Every one of these flags is true, which is
            // why the old rule closed the popup and the button looked dead.
            pointer_pressed: true,
            pointer_inside_popup: false,
            pointer_inside_button: true,
        };
        assert!(
            !opening.should_close(),
            "the popup closed on the click that opened it, so it can never be opened"
        );

        // And it must still be open on the next frame, when nothing has
        // happened yet.
        let idle = PopupDismissal {
            opened_this_frame: false,
            ..Default::default()
        };
        assert!(!idle.should_close());
    }

    #[test]
    fn a_press_outside_both_the_popup_and_its_button_closes_it() {
        let outside = PopupDismissal {
            opened_this_frame: false,
            explicitly_dismissed: false,
            pointer_pressed: true,
            pointer_inside_popup: false,
            pointer_inside_button: false,
        };
        assert!(outside.should_close());
    }

    #[test]
    fn a_press_inside_the_popup_leaves_it_open() {
        // Scrolling the list, typing in the filter, or clicking a row that does
        // not resolve to a product must not dismiss the popup underneath the
        // pointer.
        let inside = PopupDismissal {
            opened_this_frame: false,
            explicitly_dismissed: false,
            pointer_pressed: true,
            pointer_inside_popup: true,
            pointer_inside_button: false,
        };
        assert!(!inside.should_close());
    }

    #[test]
    fn a_press_on_the_button_of_an_open_popup_is_left_to_the_button() {
        // The button toggles, so its own `clicked()` closes the popup. Closing
        // here as well would be two closes for one click - and because the
        // toggle runs first, the second one would fight it.
        let on_button = PopupDismissal {
            opened_this_frame: false,
            explicitly_dismissed: false,
            pointer_pressed: true,
            pointer_inside_popup: false,
            pointer_inside_button: true,
        };
        assert!(!on_button.should_close());
    }

    #[test]
    fn an_explicit_dismissal_closes_it_even_on_the_frame_it_opened() {
        // Choosing a row on the opening frame is possible with a keyboard, and
        // it means the same thing whenever it happens.
        let chosen = PopupDismissal {
            opened_this_frame: true,
            explicitly_dismissed: true,
            pointer_pressed: false,
            pointer_inside_popup: false,
            pointer_inside_button: false,
        };
        assert!(chosen.should_close());
    }

    #[test]
    fn a_press_with_no_position_never_closes_anything() {
        // `dismissal_from_input` reports an unplaceable press as inside both
        // rectangles. Check the combination that produces, rather than trusting
        // the constructor.
        let unplaceable = PopupDismissal {
            opened_this_frame: false,
            explicitly_dismissed: false,
            pointer_pressed: true,
            pointer_inside_popup: true,
            pointer_inside_button: true,
        };
        assert!(!unplaceable.should_close());
    }

    #[test]
    fn escape_closes_it_through_the_input_reader() {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let _ = ctx.run_ui(input, |_| {});
        let dismissal = dismissal_from_input(
            &ctx,
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0)),
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0)),
            false,
            false,
        );
        assert!(dismissal.explicitly_dismissed, "Escape was not picked up");
        assert!(dismissal.should_close());
    }
}
