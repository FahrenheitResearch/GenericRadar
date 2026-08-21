//! Navigating the 3D volume: the one place that turns pointer and keyboard
//! input into camera state, plus the controls that choose which camera flies.
//!
//! Self-contained in the style of [`super::pane`]. Everything this module needs
//! arrives in [`FlyInput`]; it reaches into the application nowhere, so the pane
//! and the renderer can still be lifted into a standalone crate by moving files
//! rather than by untangling them.
//!
//! # Why there are two cameras
//!
//! Orbit swings the eye around the box centre at a fixed radius. It is the
//! camera the pane has always had, and it is the reason a second one was asked
//! for: the radius bottoms out at [`orbit_radius_floor`], 1.45 half-widths on
//! the default box - 87 km from the centre - so the wheel stops zooming while
//! the feature under inspection is still a thumbnail, and there is no way to
//! put the eye *inside* an echo at all. Fly puts the eye at `fly_x/y/z` and lets
//! it go where it likes, including through the storm.
//!
//! Switching between them never moves the picture. Both describe a position as
//! (`yaw`, `pitch`) about the box centre, so [`Vol3d::enter_fly_mode`] can seed
//! the fly eye from the orbit eye and [`adopt_fly_eye_as_orbit`] can rewrite the
//! orbit angles from wherever the operator flew. Entering fly keeps the view
//! direction as well; leaving it cannot, because an orbit camera looks at the
//! centre by definition.
//!
//! # Units
//!
//! One world unit is one box half-width, so every speed here is in half-widths
//! per second and scales with the box: the setting that crosses the 120 km box
//! in eleven seconds crosses the 360 km box in eleven seconds. Nothing here
//! needs to know how many kilometres that is.

// This module once carried its own `#![allow(dead_code)]` promising deletion
// "in the commit that pastes [the call sites] into vol3d/pane.rs" - that
// commit is this one: `pane::canvas` drives [`drive_camera`] and the pane
// toolbar surfaces [`camera_controls`]. Note the parent module's own
// `#![allow(dead_code)]` (vol3d.rs, for its ported BowEcho surface) STILL
// reaches this file - lint scoping covers children - and cannot be re-armed
// here with `#![warn(dead_code)]`, because `examples/vol3d_opacity_proof.rs`
// recompiles the whole vol3d tree standalone, where this module's entry
// points are legitimately uncalled. Narrowing that parent allow is vol3d.rs's
// own change.

use eframe::egui;

use super::{Vol3d, Vol3dCameraMode};

/// Radians of turn per point of pointer travel.
///
/// Deliberately the number the pane's orbit drag has always used, so switching
/// cameras does not change how far a given drag turns the view.
const LOOK_RADIANS_PER_POINT: f32 = 0.01;

/// Pitch stop while flying, radians.
///
/// [`Vol3d::enter_fly_mode`] clamps to this on entry; flying has to hold the
/// same bound or the first drag would undo it. Just short of a right angle
/// because `Vol3d::camera_basis` builds `right` from the horizontal part of the
/// forward vector: at exactly +/-90 degrees that part vanishes, the basis
/// degenerates, and the view snaps to an arbitrary roll.
const FLY_PITCH_LIMIT: f32 = 1.45;

/// Pitch stops while orbiting, radians.
///
/// The upper one is the pane's own value. The lower one is not taste:
/// [`Vol3d::enter_orbit_mode`] clamps pitch up to 0.03 on the way in, so a drag
/// that dips the eye below the horizon does not STAY there - the next mode
/// switch snapped it back by 3.3 half-widths, 200 km on the default box.
/// Stopping the drag where the mode stops makes that state unreachable, at the
/// price of a view of the underside of the floor PPI.
const ORBIT_PITCH_MIN: f32 = 0.03;
const ORBIT_PITCH_MAX: f32 = 1.5;

/// Orbit radius stops and wheel gain per point. The pane's own values.
///
/// [`ORBIT_DIST_MIN`] is a floor under the floor: the radius the eye is really
/// placed at is [`orbit_radius_floor`], which is larger and box-dependent.
const ORBIT_DIST_MIN: f32 = 0.35;
const ORBIT_DIST_MAX: f32 = 6.0;
const ORBIT_SCROLL_GAIN: f32 = 0.002;

/// Orbit radius used when `dist` arrives non-finite. `Vol3d::default`'s value.
const ORBIT_DIST_FALLBACK: f32 = 2.4;

/// Eye position used when a non-finite fly position cannot be recovered from
/// the orbit camera either. `Vol3d::default`'s value.
const FLY_POSITION_FALLBACK: [f32; 3] = [0.0, -2.4, 1.0];

/// Longest slice of time the camera integrates in one go, seconds.
///
/// Outside the box the speed depends on where the eye IS, so every step changes
/// the speed of the next. Integrating a whole frame at the speed it started at
/// lands short, and shorter the longer the frame: 0.025 half-widths (1.5 km)
/// between 30 Hz and 60 Hz after a second of held W, i.e. a camera that covers
/// different ground on every machine. Slicing each frame this finely puts every
/// frame rate within 0.01 half-widths of every other, for at most
/// [`MAX_FRAME_DT`] / this = 24 iterations of a dozen flops.
const MAX_INTEGRATION_STEP: f32 = 1.0 / 240.0;

/// Ceiling on those slices, so the loop below is bounded whatever `dt` says.
const MAX_INTEGRATION_SLICES: f32 = 24.0;

/// Furthest one frame may carry the eye, in box half-widths.
///
/// A quarter of a box: far enough that ordinary flight never reaches it (the
/// default speed at the distance ceiling, boost held, is 19.2 half-widths a
/// second, or 0.32 in a 60 Hz frame), close enough that no one frame can carry
/// the operator through the storm and out the far side. Without it a
/// five-second stall at the top of the speed slider moved 9.6 half-widths in
/// ONE frame, and one frame of wheel four half-widths out moved 2.3.
const MAX_STEP_PER_FRAME: f32 = 0.5;

/// Longest frame the camera will integrate, seconds.
///
/// A stalled frame - a resample landing, a GPU hitch, a breakpoint - reports
/// its true wall time in `stable_dt`, and integrating five seconds of held W in
/// one go would teleport the eye out the far side of the volume. A tenth of a
/// second is about a hundredth of a box crossing, so the worst a stall costs is
/// a step the operator will not notice.
const MAX_FRAME_DT: f32 = 0.1;

/// Speed multiplier while the eye is inside the box.
///
/// Distance-proportional speed alone would stop dead at the box surface, which
/// is precisely where the operator wants to be moving. 0.15 of the slider
/// setting puts the default 1.2 at 0.18 half-widths per second: about eleven
/// seconds to cross the box, whatever size the box is.
const SPEED_NEAR: f32 = 0.15;

/// Ceiling on the distance-proportional multiplier. At 4.0 the far corner of
/// the reachable region ([`FLY_RANGE`]) is a few seconds away instead of a
/// minute, and the speed stops growing well before an approach becomes
/// unsteerable. [`MAX_STEP_PER_FRAME`] bounds what any one frame does with it.
const SPEED_FAR: f32 = 4.0;

/// Shift multiplier. Conventional, and the cheapest possible answer to "the far
/// end of a 120 km volume takes too long".
const BOOST_MULTIPLIER: f32 = 4.0;

/// Ctrl multiplier, for placing the eye beside a couplet rather than crossing
/// the county.
const PRECISION_MULTIPLIER: f32 = 0.25;

/// Seconds of forward flight bought by one point of wheel travel.
///
/// egui reports a wheel notch as roughly fifty points, so a notch is worth
/// about a second and a half on the stick: far enough to read as a zoom step,
/// short enough not to fling the eye out of the volume. This is the control
/// "nothing zooms in quite enough" was about - in fly mode the wheel moves the
/// eye instead of shortening an orbit radius that has a floor.
const SCROLL_SECONDS_PER_POINT: f32 = 0.03;

/// Slider bounds for `fly_speed`. The minimum doubles as the floor applied to
/// whatever value the field happens to hold.
pub const MIN_FLY_SPEED: f32 = 0.05;
pub const MAX_FLY_SPEED: f32 = 6.0;

/// How far the eye may wander from the box centre, in box half-widths.
///
/// Twelve is six box widths out - room for a wide establishing shot, near
/// enough that a stuck key cannot leave the operator staring at empty space.
/// It also clears every orbit eye the pane can produce (`dist` caps at
/// [`ORBIT_DIST_MAX`]), so entering fly mode never clips the seeded position,
/// which would undo the no-jump seeding `enter_fly_mode` exists to provide.
const FLY_RANGE: f32 = 12.0;

/// How far below the ground plane the eye may go, in box half-widths.
///
/// The floor PPI is drawn at z=0 and the volume sits above it, so there is
/// nothing to see underneath; one half-width is enough to look up at a storm
/// base without losing the horizon entirely.
const FLY_FLOOR: f32 = -1.0;

/// Forward, back and strafe.
///
/// These keys are NOT free. `pane_canvas::keyboard_nav` binds the same
/// W/A/S/D and arrows to pan the active 2D pane, and `product_picker::read_keys`
/// binds the arrows to walk its list. See [`drive_camera`] for how keyboard
/// focus keeps one set of keys meaning one thing.
pub const KEYS_FORWARD: &[egui::Key] = &[egui::Key::W, egui::Key::ArrowUp];
pub const KEYS_BACK: &[egui::Key] = &[egui::Key::S, egui::Key::ArrowDown];
pub const KEYS_LEFT: &[egui::Key] = &[egui::Key::A, egui::Key::ArrowLeft];
pub const KEYS_RIGHT: &[egui::Key] = &[egui::Key::D, egui::Key::ArrowRight];

/// Climb and descend.
///
/// E/Q is the flying-camera convention; Page Up/Page Down is the reachable
/// alternative for a left hand already on W/A/S/D. Space and Ctrl - the other
/// conventional pair - are deliberately NOT bound: egui turns Space into a
/// click on the focused widget, and the canvas IS that widget while flying,
/// while Ctrl is the precision modifier here. `Home`, `+`, `=` and `-` are
/// avoided because `pane_canvas` owns them.
pub const KEYS_UP: &[egui::Key] = &[egui::Key::E, egui::Key::PageUp];
pub const KEYS_DOWN: &[egui::Key] = &[egui::Key::Q, egui::Key::PageDown];

/// Every key that steers, in one place, so a collision test can enumerate them.
/// Test-only on purpose: nothing at runtime iterates the whole map, and (once
/// vol3d.rs narrows its module-wide `allow(dead_code)` - today that allow
/// still reaches this file, see the note at the top) an unused runtime const
/// here would be the first thing the re-armed gate reports.
#[cfg(test)]
pub const FLIGHT_KEY_TABLES: &[&[egui::Key]] = &[
    KEYS_FORWARD,
    KEYS_BACK,
    KEYS_LEFT,
    KEYS_RIGHT,
    KEYS_UP,
    KEYS_DOWN,
];

/// What the canvas keeps for itself while it holds the keyboard.
///
/// The arrows steer. Without this egui reads them as "move focus to the widget
/// in that direction" and the canvas loses the keyboard mid-flight. Tab and
/// Escape are left to egui, but note that [`drive_camera`] re-claims the
/// keyboard every frame the pointer is over a flying canvas: the way to let go
/// of it is to point somewhere else, which is also the way to get to the
/// controls.
const FLY_EVENT_FILTER: egui::EventFilter = egui::EventFilter {
    tab: false,
    horizontal_arrows: true,
    vertical_arrows: true,
    escape: false,
};

/// The whole key map in one line, for the Fly button's tooltip.
///
/// The first sentence is the one that matters: the keys only steer while the
/// pointer is over the view, and nothing about a flying camera makes that
/// obvious.
pub const FLY_KEY_HINT: &str = "Point at the view, then: W/S forward and back, \
A/D strafe, E/Q climb and descend. Arrow keys and Page Up/Page Down do the same. \
Drag to look, wheel to dolly. Hold Shift to move 4x faster, Ctrl to move 4x \
slower. Speed grows with distance from the box, so the far corner is not a long \
haul.";

const ORBIT_HINT: &str =
    "Swing around the box at a fixed radius. Drag to turn, wheel to change the radius.";

const RECENTER_HINT: &str = "Put the eye back where the orbit camera would be, looking at the box.";

/// Everything the camera needs from the pane, and nothing more.
pub struct FlyInput<'a> {
    /// The camera state to update.
    pub vol3d: &'a mut Vol3d,
    /// The canvas the volume is drawn into, allocated with
    /// `egui::Sense::click_and_drag()`. Its hover and drag state is what keeps
    /// the camera still while the pointer is over a control.
    pub response: &'a egui::Response,
    /// This frame's own duration, seconds - see [`frame_dt`]. Re-clamped
    /// inside [`drive_camera`]: the field is public.
    pub dt: f32,
}

/// The three movement axes a frame's keys asked for, each in -1..=1, and the
/// triple never longer than 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlyAxes {
    /// Positive is to the camera's right.
    pub strafe: f32,
    /// Positive is along the view direction.
    pub forward: f32,
    /// Positive is up, in world z rather than camera up.
    pub vertical: f32,
}

impl FlyAxes {
    pub const STILL: Self = Self {
        strafe: 0.0,
        forward: 0.0,
        vertical: 0.0,
    };

    pub fn is_still(self) -> bool {
        self.strafe == 0.0 && self.forward == 0.0 && self.vertical == 0.0
    }
}

/// This frame's duration, already clamped to something a camera can integrate.
///
/// The pane passes this to [`FlyInput::dt`]. `stable_dt` rather than
/// `unstable_dt` because the smoothed figure keeps the camera from stuttering
/// through compositor jitter; the clamp keeps it from teleporting on a stall.
#[must_use]
pub fn frame_dt(ctx: &egui::Context) -> f32 {
    clamp_dt(ctx.input(|state| state.stable_dt))
}

/// Switch cameras without moving the picture.
///
/// The only place this module changes `camera_mode`. `enter_fly_mode` seeds the
/// fly position from the current orbit eye so the view does not jump, and both
/// entry points re-clamp pitch into their own mode's range; assigning the field
/// here instead would silently skip both.
///
/// The other direction needs [`adopt_fly_eye_as_orbit`] to be equally quiet:
/// `enter_orbit_mode` leaves `yaw`, `pitch` and `dist` describing wherever the
/// orbit camera last pointed, so Orbit after any flight used to snap the eye
/// back there - half a half-width after half a second of flight, and further
/// the longer the operator flew.
pub fn set_camera_mode(vol3d: &mut Vol3d, mode: Vol3dCameraMode) {
    match mode {
        Vol3dCameraMode::Orbit => {
            adopt_fly_eye_as_orbit(vol3d);
            vol3d.enter_orbit_mode();
        }
        Vol3dCameraMode::Fly => vol3d.enter_fly_mode(),
    }
}

/// Re-describe where the fly camera is in the orbit camera's own terms - yaw,
/// pitch and radius about the box centre - so that leaving fly mode leaves the
/// eye where the operator flew it.
///
/// The two cameras already agree on the angles: `Vol3d::orbit_eye` places the
/// eye at (yaw, pitch) about the centre and `Vol3d::fly_forward` points from
/// there back at it, so a position rewritten this way is one both describe
/// identically. What orbit cannot express - a radius under
/// [`orbit_radius_floor`] or over [`ORBIT_DIST_MAX`], a pitch under
/// [`ORBIT_PITCH_MIN`] - steps to the nearest orbit the mode allows rather than
/// to a view from last time. The view DIRECTION is not preserved and cannot be:
/// an orbit camera looks at the box centre by definition.
fn adopt_fly_eye_as_orbit(vol3d: &mut Vol3d) {
    if vol3d.camera_mode != Vol3dCameraMode::Fly {
        return;
    }
    let center = vol3d.orbit_center();
    let (dx, dy, dz) = (
        vol3d.fly_x - center[0],
        vol3d.fly_y - center[1],
        vol3d.fly_z - center[2],
    );
    let radius = (dx * dx + dy * dy + dz * dz).sqrt();
    if !radius.is_finite() || radius <= 1.0e-4 {
        // The eye is sitting on the centre of the box; there is no direction to
        // derive, so leave the orbit camera describing what it described.
        return;
    }
    vol3d.yaw = dy.atan2(dx);
    vol3d.pitch = (dz / radius).clamp(-1.0, 1.0).asin();
    vol3d.dist = radius.clamp(orbit_radius_floor(vol3d), ORBIT_DIST_MAX);
}

/// The shortest radius `Vol3d::orbit_distance` will actually place the eye at.
///
/// Mirrored rather than called because `orbit_distance` only reveals the floor
/// once `dist` is already below it; pinned against the real thing by
/// `the_orbit_radius_floor_is_the_one_the_camera_uses`. It matters because it is
/// larger than [`ORBIT_DIST_MIN`] - 1.3 to 2.1 across every box size and
/// exaggeration the pane offers - so clamping the wheel to `ORBIT_DIST_MIN`
/// left a dozen notches that changed the number and moved nothing, which is
/// most of what "nothing zooms in quite enough" felt like. Below this radius
/// the answer is the fly camera.
fn orbit_radius_floor(vol3d: &Vol3d) -> f32 {
    let floor = vol3d.zspan() * 0.45 + 1.25;
    if floor.is_finite() {
        floor.clamp(ORBIT_DIST_MIN, ORBIT_DIST_MAX)
    } else {
        ORBIT_DIST_FALLBACK
    }
}

/// Consume this frame's pointer and keyboard input and move the camera.
///
/// Returns true when the camera moved. The pane must request a repaint on true:
/// egui only repaints when something happens and a held key is not an event, so
/// without the repaint the camera would take one step per keystroke instead of
/// flying. Returning false when nothing moved is what stops the pane from
/// spinning the GPU at full rate while the operator reads the screen.
///
/// # Who owns the keyboard
///
/// W/A/S/D and the arrows are already spoken for: `pane_canvas::keyboard_nav`
/// pans the active 2D pane with them and does not ask whether the pointer is
/// anywhere near that pane, and `WorkstationApp::ui` draws the 3D window BEFORE
/// the panes - so without a rule one held W would fly the camera and pan the
/// map behind it at once.
///
/// The rule is keyboard focus, the mechanism both readers already respect:
/// while the camera is in fly mode and the pointer is over the canvas, the
/// canvas takes focus and `keyboard_nav` stands down on its own. The claim is
/// never made against a focused TEXT field, so the product picker's filter
/// keeps the arrows the moment it opens, and it is dropped as soon as the
/// pointer leaves, so the panes get their keys back with no operator action.
#[must_use]
pub fn drive_camera(input: FlyInput<'_>) -> bool {
    let FlyInput {
        vol3d,
        response,
        dt,
    } = input;
    // Repair first, enforce last: whatever state this module is handed - a NaN
    // from a slider, a pitch past the pole - the arithmetic below starts from
    // finite values, and cannot leave them non-finite on the way out.
    sanitize(vol3d);

    let dt = clamp_dt(dt);
    let ctx = &response.ctx;
    // Primary or middle button only. The pane opens its context menu on
    // secondary click, and turning the view out from under the menu the
    // operator is about to read is not navigation.
    let looking = response.dragged_by(egui::PointerButton::Primary)
        || response.dragged_by(egui::PointerButton::Middle);
    // `hovered` rather than `contains_pointer`: it is already false when some
    // other widget is being dragged, which is exactly "the pointer is over a
    // control, not the canvas". `|| looking` keeps a mouse-look going when the
    // drag carries the pointer off the edge of the canvas.
    let on_canvas = response.hovered();
    let mut moved = false;

    if looking {
        moved |= apply_look(vol3d, response.drag_delta());
    }

    let claim_keyboard = vol3d.camera_mode == Vol3dCameraMode::Fly
        && (on_canvas || looking)
        && !ctx.text_edit_focused();
    if claim_keyboard {
        if !response.has_focus() {
            response.request_focus();
        }
        ctx.memory_mut(|memory| {
            memory.set_focus_lock_filter(response.id, FLY_EVENT_FILTER);
            // egui latches a plain arrow as "move focus that way" at the
            // START of the pass, and the filter above refuses to apply until
            // the widget has held focus for a whole frame - so the filter
            // alone cannot defend the FIRST arrow. It handed the keyboard to
            // the nearest widget that way: the pane's own fly-speed slider,
            // which then ate the strafe keys to change its value. Cancelling
            // the pending move every frame closes that window. Tab goes with
            // it, which is the price of a canvas that flies while pointed at.
            memory.move_focus(egui::FocusDirection::None);
        });
    } else if response.has_focus() {
        response.surrender_focus();
    }

    if on_canvas || looking {
        let (keys, modifiers, scroll) = ctx.input(|state| {
            (
                axes_from_keys(state),
                state.modifiers,
                state.smooth_scroll_delta.y,
            )
        });
        let scroll = if on_canvas && scroll.is_finite() {
            scroll
        } else {
            0.0
        };
        // `has_focus` is also false when the native window is not focused, so
        // an alt-tabbed application does not keep flying.
        let keys = if claim_keyboard && response.has_focus() {
            keys
        } else {
            FlyAxes::STILL
        };

        match vol3d.camera_mode {
            Vol3dCameraMode::Orbit => {
                if scroll != 0.0 {
                    let dist = vol3d.dist * (1.0 - scroll * ORBIT_SCROLL_GAIN);
                    if dist.is_finite() {
                        let previous = vol3d.dist;
                        vol3d.dist = dist.clamp(orbit_radius_floor(vol3d), ORBIT_DIST_MAX);
                        // Only a radius that CHANGED is movement. Repainting at
                        // the stop would spin the GPU for a wheel that is doing
                        // nothing.
                        moved |= vol3d.dist != previous;
                    }
                }
            }
            Vol3dCameraMode::Fly => {
                // One budget for the whole frame: the wheel spends first
                // because it is an explicit gesture, the keys take the rest.
                let mut budget = MAX_STEP_PER_FRAME;
                if scroll != 0.0 {
                    let dolly = (fly_speed(vol3d, modifiers) * scroll * SCROLL_SECONDS_PER_POINT)
                        .clamp(-budget, budget);
                    budget -= dolly.abs();
                    moved |= translate(vol3d, 0.0, dolly, 0.0);
                }
                if !keys.is_still() {
                    moved |= fly_keys(vol3d, keys, modifiers, dt, budget);
                }
            }
        }
    }

    sanitize(vol3d);
    moved
}

/// The camera mode toggle, the speed slider and the key map.
///
/// Draws inline with no layout of its own, so it drops into the pane's existing
/// `horizontal_wrapped` toolbar row beside the View menu.
pub fn camera_controls(ui: &mut egui::Ui, vol3d: &mut Vol3d) {
    let orbiting = vol3d.camera_mode == Vol3dCameraMode::Orbit;
    if ui
        .selectable_label(orbiting, "Orbit")
        .on_hover_text(ORBIT_HINT)
        .clicked()
    {
        set_camera_mode(vol3d, Vol3dCameraMode::Orbit);
    }
    if ui
        .selectable_label(!orbiting, "Fly")
        .on_hover_text(FLY_KEY_HINT)
        .clicked()
    {
        set_camera_mode(vol3d, Vol3dCameraMode::Fly);
    }
    if !orbiting {
        ui.add(
            egui::Slider::new(&mut vol3d.fly_speed, MIN_FLY_SPEED..=MAX_FLY_SPEED)
                .logarithmic(true)
                .text("fly speed"),
        );
        if ui.button("Recenter").on_hover_text(RECENTER_HINT).clicked() {
            // The same seeding `enter_fly_mode` does, which is the point: an
            // operator who has flown themselves into the dark gets back to a
            // view of the box without losing their heading.
            vol3d.reset_fly_eye_from_orbit();
        }
    }
}

/// Turn the view. Both cameras steer the same way; only the pitch stop differs.
fn apply_look(vol3d: &mut Vol3d, delta: egui::Vec2) -> bool {
    if !delta.x.is_finite() || !delta.y.is_finite() {
        return false;
    }
    if delta.x == 0.0 && delta.y == 0.0 {
        return false;
    }
    // Signs match the pane's orbit drag exactly: dragging right turns the view
    // right in both modes, dragging down tips it down in both.
    vol3d.yaw -= delta.x * LOOK_RADIANS_PER_POINT;
    vol3d.pitch += delta.y * LOOK_RADIANS_PER_POINT;
    sanitize(vol3d);
    true
}

/// Integrate `dt` seconds of held keys.
///
/// In slices, because the speed depends on where the eye is: one evaluation per
/// frame makes 30 Hz and 144 Hz cover different ground for the same second of
/// held W. See [`MAX_INTEGRATION_STEP`]. `budget` is what is left of this
/// frame's [`MAX_STEP_PER_FRAME`]; running out stops the loop, so a stalled
/// frame ends short of where it asked instead of somewhere unrecognisable.
fn fly_keys(
    vol3d: &mut Vol3d,
    keys: FlyAxes,
    modifiers: egui::Modifiers,
    dt: f32,
    budget: f32,
) -> bool {
    let slices = (dt / MAX_INTEGRATION_STEP)
        .ceil()
        .clamp(1.0, MAX_INTEGRATION_SLICES);
    let slice = dt / slices;
    let mut budget = budget;
    let mut moved = false;
    for _ in 0..slices as usize {
        // Re-read the speed every slice. That it grows as the eye leaves the
        // box is the whole reason one evaluation per frame is not enough.
        let step = (fly_speed(vol3d, modifiers) * slice).min(budget);
        if step <= 0.0 {
            break;
        }
        budget -= step;
        moved |= translate(
            vol3d,
            keys.strafe * step,
            keys.forward * step,
            keys.vertical * step,
        );
    }
    moved
}

/// Half-widths per second the camera would travel from where it is now.
fn fly_speed(vol3d: &Vol3d, modifiers: egui::Modifiers) -> f32 {
    vol3d.fly_speed.max(MIN_FLY_SPEED) * speed_scale(vol3d, modifiers)
}

/// Move the eye by `right * strafe + forward * ahead + world_up * lift`, all
/// already in world units.
///
/// Deliberately not `Vol3d::apply_fly_movement`, otherwise the same arithmetic:
/// that helper clamps z into `[-1, zspan + 1]`, and an eye seeded from a
/// scrolled-out orbit starts above that ceiling, so the first keypress would
/// yank it down by half the box. The forward vector still comes from
/// `Vol3d::fly_forward` and `right` uses `Vol3d::camera_basis`'s convention,
/// which must not drift from what the shader draws.
fn translate(vol3d: &mut Vol3d, strafe: f32, ahead: f32, lift: f32) -> bool {
    if !(strafe.is_finite() && ahead.is_finite() && lift.is_finite()) {
        return false;
    }
    if strafe == 0.0 && ahead == 0.0 && lift == 0.0 {
        return false;
    }
    let forward = vol3d.fly_forward();
    let mut right = [forward[1], -forward[0]];
    let length = (right[0] * right[0] + right[1] * right[1]).sqrt();
    if !length.is_finite() || length <= 1.0e-6 {
        // Only reachable if pitch escaped its clamp; refusing to move beats
        // dividing by a vanishing basis.
        return false;
    }
    right[0] /= length;
    right[1] /= length;

    let x = vol3d.fly_x + right[0] * strafe + forward[0] * ahead;
    let y = vol3d.fly_y + right[1] * strafe + forward[1] * ahead;
    let z = vol3d.fly_z + forward[2] * ahead + lift;
    if !(x.is_finite() && y.is_finite() && z.is_finite()) {
        // Leave the camera exactly where it was rather than clamping a NaN,
        // which `f32::clamp` propagates rather than repairs.
        return false;
    }

    let landed = [
        x.clamp(-FLY_RANGE, FLY_RANGE),
        y.clamp(-FLY_RANGE, FLY_RANGE),
        z.clamp(FLY_FLOOR, FLY_RANGE),
    ];
    // Reporting a move the clamp swallowed would keep the pane repainting at
    // full rate for as long as a key is held against the edge of the region.
    let moved = landed != [vol3d.fly_x, vol3d.fly_y, vol3d.fly_z];
    [vol3d.fly_x, vol3d.fly_y, vol3d.fly_z] = landed;
    moved
}

/// Straight-line distance from the eye to the nearest point of the box, in box
/// half-widths. Zero inside.
fn distance_outside_box(vol3d: &Vol3d) -> f32 {
    let zspan = vol3d.zspan();
    let outside_x = (vol3d.fly_x.abs() - 1.0).max(0.0);
    let outside_y = (vol3d.fly_y.abs() - 1.0).max(0.0);
    let outside_z = (-vol3d.fly_z).max(vol3d.fly_z - zspan).max(0.0);
    (outside_x * outside_x + outside_y * outside_y + outside_z * outside_z).sqrt()
}

/// Multiplier on `fly_speed` for this frame.
///
/// Proportional to the distance from the box because one fixed speed is
/// unusable at both ends of a 120 km volume: fast enough to cross the approach
/// is far too fast to place the eye beside an updraught.
fn speed_scale(vol3d: &Vol3d, modifiers: egui::Modifiers) -> f32 {
    let mut scale = (SPEED_NEAR + distance_outside_box(vol3d)).clamp(SPEED_NEAR, SPEED_FAR);
    if !scale.is_finite() {
        return SPEED_NEAR;
    }
    if modifiers.shift {
        scale *= BOOST_MULTIPLIER;
    }
    if modifiers.ctrl {
        scale *= PRECISION_MULTIPLIER;
    }
    scale
}

fn clamp_dt(dt: f32) -> f32 {
    if dt.is_finite() {
        dt.clamp(0.0, MAX_FRAME_DT)
    } else {
        0.0
    }
}

fn any_down(state: &egui::InputState, keys: &[egui::Key]) -> bool {
    keys.iter().any(|key| state.key_down(*key))
}

fn axis(state: &egui::InputState, positive: &[egui::Key], negative: &[egui::Key]) -> f32 {
    f32::from(any_down(state, positive)) - f32::from(any_down(state, negative))
}

fn axes_from_keys(state: &egui::InputState) -> FlyAxes {
    let mut axes = FlyAxes {
        strafe: axis(state, KEYS_RIGHT, KEYS_LEFT),
        forward: axis(state, KEYS_FORWARD, KEYS_BACK),
        vertical: axis(state, KEYS_UP, KEYS_DOWN),
    };
    // Without this, W+D is sqrt(2) times faster than W, which reads as the
    // camera lurching whenever a turn is started.
    let length =
        (axes.strafe * axes.strafe + axes.forward * axes.forward + axes.vertical * axes.vertical)
            .sqrt();
    if length > 1.0 {
        axes.strafe /= length;
        axes.forward /= length;
        axes.vertical /= length;
    }
    axes
}

/// The pitch range this mode is allowed to hold, low first.
///
/// Both are the ranges `vol3d.rs` enforces when the mode is entered. Steering
/// inside them is what keeps a switch from moving the view.
fn pitch_limits(mode: Vol3dCameraMode) -> (f32, f32) {
    match mode {
        Vol3dCameraMode::Orbit => (ORBIT_PITCH_MIN, ORBIT_PITCH_MAX),
        Vol3dCameraMode::Fly => (-FLY_PITCH_LIMIT, FLY_PITCH_LIMIT),
    }
}

/// Force the camera back onto its invariants: finite everywhere, pitch inside
/// this mode's stop, eye inside the reachable region.
///
/// Idempotent, and cheap enough to run twice a frame. `yaw` is repaired but not
/// wrapped: an unwrapped angle only loses a visible fraction of a radian past a
/// million of them, and wrapping would make every comparison here modular.
fn sanitize(vol3d: &mut Vol3d) {
    if !vol3d.yaw.is_finite() {
        vol3d.yaw = 0.0;
    }
    if !vol3d.pitch.is_finite() {
        vol3d.pitch = 0.0;
    }
    let (low, high) = pitch_limits(vol3d.camera_mode);
    vol3d.pitch = vol3d.pitch.clamp(low, high);
    if !vol3d.dist.is_finite() {
        vol3d.dist = ORBIT_DIST_FALLBACK;
    }

    if !fly_position_is_finite(vol3d) {
        // Recover to the orbit eye rather than to the origin: it is a position
        // known to be outside the box and looking at it, so an operator who
        // hits this sees the volume rather than the inside of a voxel.
        vol3d.reset_fly_eye_from_orbit();
    }
    if !fly_position_is_finite(vol3d) {
        // `orbit_eye` reads `focus_height_km` and `vertical_exaggeration` too,
        // and neither belongs to this module; if one of those is NaN the
        // recovery above returns NaN as well.
        vol3d.fly_x = FLY_POSITION_FALLBACK[0];
        vol3d.fly_y = FLY_POSITION_FALLBACK[1];
        vol3d.fly_z = FLY_POSITION_FALLBACK[2];
    }
    vol3d.fly_x = vol3d.fly_x.clamp(-FLY_RANGE, FLY_RANGE);
    vol3d.fly_y = vol3d.fly_y.clamp(-FLY_RANGE, FLY_RANGE);
    vol3d.fly_z = vol3d.fly_z.clamp(FLY_FLOOR, FLY_RANGE);
}

fn fly_position_is_finite(vol3d: &Vol3d) -> bool {
    vol3d.fly_x.is_finite() && vol3d.fly_y.is_finite() && vol3d.fly_z.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame length every hand-computed expectation below is written for.
    const DT: f32 = 1.0 / 60.0;
    const CANVAS: egui::Vec2 = egui::vec2(300.0, 200.0);
    const SCREEN: egui::Vec2 = egui::vec2(400.0, 300.0);

    /// `Vol3d::default`'s fly speed, restated so the sums below stay readable.
    const SPEED: f32 = 1.2;

    /// What else is on screen, competing for the pointer and the keyboard.
    #[derive(Clone, Copy, PartialEq)]
    enum Decoy {
        /// Nothing but the canvas.
        Nothing,
        /// A focused text field, standing in for the product picker's filter.
        TextField,
        /// A horizontal slider directly above the canvas, standing in for the
        /// fly-speed slider [`camera_controls`] draws in the pane's toolbar:
        /// egui hands it focus when it reads an arrow as focus navigation, and
        /// it then CONSUMES the strafe keys to set its own value.
        Slider,
    }

    /// A headless 3D canvas: one egui context, one `Vol3d`, and one widget
    /// allocated exactly the way `pane::canvas` allocates it.
    struct Bench {
        ctx: egui::Context,
        vol3d: Vol3d,
        modifiers: egui::Modifiers,
        /// What else is on screen, competing for the pointer and the keyboard.
        decoy: Decoy,
        /// False stands in for an alt-tabbed application. egui reports it in
        /// `RawInput::focused`, and `Response::has_focus` folds it in.
        window_focused: bool,
        filter: String,
        slider_value: f32,
        slider_rect: egui::Rect,
        rect: egui::Rect,
        canvas_id: Option<egui::Id>,
        hovered: bool,
        focused: bool,
        /// Who holds the keyboard once the pass is OVER, which is where egui's
        /// focus navigation leaves it - not where the widget code put it.
        focus_after_pass: Option<egui::Id>,
    }

    impl Bench {
        fn new() -> Self {
            Self::build(Decoy::Nothing)
        }

        fn with_focused_text_field() -> Self {
            Self::build(Decoy::TextField)
        }

        /// A canvas with the pane's own fly-speed slider drawn above it.
        fn with_toolbar_slider() -> Self {
            Self::build(Decoy::Slider)
        }

        fn build(decoy: Decoy) -> Self {
            let mut bench = Self {
                ctx: egui::Context::default(),
                vol3d: Vol3d::default(),
                modifiers: egui::Modifiers::default(),
                decoy,
                window_focused: true,
                filter: String::new(),
                slider_value: SPEED,
                slider_rect: egui::Rect::NOTHING,
                rect: egui::Rect::NOTHING,
                canvas_id: None,
                hovered: false,
                focused: false,
                focus_after_pass: None,
            };
            // egui only knows where a widget is once it has been laid out, so
            // the first frame can never report a hover.
            bench.frame(Vec::new(), DT);
            bench
        }

        fn frame(&mut self, events: Vec<egui::Event>, dt: f32) -> bool {
            let ctx = self.ctx.clone();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
                modifiers: self.modifiers,
                focused: self.window_focused,
                events,
                ..egui::RawInput::default()
            };
            let vol3d = &mut self.vol3d;
            let decoy = self.decoy;
            let filter = &mut self.filter;
            let slider_value = &mut self.slider_value;
            let mut slider_rect = egui::Rect::NOTHING;
            let mut rect = egui::Rect::NOTHING;
            let mut canvas_id = None;
            let mut hovered = false;
            let mut focused = false;
            let mut moved = false;
            let _ = ctx.run_ui(raw, |ui| {
                match decoy {
                    Decoy::Nothing => {}
                    Decoy::TextField => {
                        // Focus claimed before the field is drawn, exactly as
                        // `product_picker::filter_field` claims it.
                        let id = egui::Id::new("bench-filter");
                        ui.memory_mut(|memory| memory.request_focus(id));
                        ui.add(egui::TextEdit::singleline(filter).id(id));
                    }
                    Decoy::Slider => {
                        // Above the canvas, where the pane's toolbar is, and
                        // the same widget `camera_controls` puts there.
                        let speed = egui::Slider::new(slider_value, MIN_FLY_SPEED..=MAX_FLY_SPEED);
                        slider_rect = ui.add(speed).rect;
                    }
                }
                let (allocated, response) =
                    ui.allocate_exact_size(CANVAS, egui::Sense::click_and_drag());
                rect = allocated;
                canvas_id = Some(response.id);
                hovered = response.hovered();
                moved = drive_camera(FlyInput {
                    vol3d,
                    response: &response,
                    dt,
                });
                focused = response.has_focus();
            });
            self.slider_rect = slider_rect;
            self.rect = rect;
            self.canvas_id = canvas_id;
            self.hovered = hovered;
            self.focused = focused;
            self.focus_after_pass = ctx.memory(|memory| memory.focused());
            moved
        }

        /// Run `count` frames of `dt` with no new events - a held key produces
        /// none, which is the whole reason the camera integrates time.
        fn coast(&mut self, count: usize, dt: f32) {
            for _ in 0..count {
                self.frame(Vec::new(), dt);
            }
        }

        /// Put the pointer over the decoy slider instead of the canvas.
        fn hover_slider(&mut self) {
            self.point_at(self.slider_rect.center(), false);
        }

        fn center(&self) -> egui::Pos2 {
            self.rect.center()
        }

        fn key(&self, key: egui::Key, pressed: bool) -> egui::Event {
            egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: self.modifiers,
            }
        }

        fn wheel(&self, points: f32) -> egui::Event {
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, points),
                phase: egui::TouchPhase::Move,
                modifiers: self.modifiers,
            }
        }

        /// Move the pointer and run frames until egui reports the hover the
        /// caller asked for. Hover resolves at the END of a pass, so it always
        /// lands one frame after the pointer moves.
        fn point_at(&mut self, pos: egui::Pos2, on_canvas: bool) {
            for _ in 0..5 {
                self.frame(vec![egui::Event::PointerMoved(pos)], DT);
                if self.hovered == on_canvas {
                    return;
                }
            }
            panic!("the canvas hover never became {on_canvas}");
        }

        fn hover(&mut self) {
            self.point_at(self.center(), true);
        }

        fn leave(&mut self) {
            self.point_at(self.rect.right_bottom() + egui::vec2(20.0, 20.0), false);
        }

        fn press(&mut self) {
            let event = egui::Event::PointerButton {
                pos: self.center(),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: self.modifiers,
            };
            self.frame(vec![event], DT);
        }

        fn move_pointer(&mut self, pos: egui::Pos2) {
            self.frame(vec![egui::Event::PointerMoved(pos)], DT);
        }

        fn eye(&self) -> [f32; 3] {
            [self.vol3d.fly_x, self.vol3d.fly_y, self.vol3d.fly_z]
        }

        /// Fly mode, looking due west along -x with a level pitch, parked in the
        /// middle of the box so `distance_outside_box` is zero and the speed
        /// multiplier is exactly [`SPEED_NEAR`].
        fn park_inside_the_box(&mut self) {
            set_camera_mode(&mut self.vol3d, Vol3dCameraMode::Fly);
            self.vol3d.yaw = 0.0;
            self.vol3d.pitch = 0.0;
            self.vol3d.fly_x = 0.0;
            self.vol3d.fly_y = 0.0;
            // `zspan` is 0.45 at the default 1.5x exaggeration, so 0.3 is
            // inside the box and `distance_outside_box` is exactly zero.
            self.vol3d.fly_z = 0.3;
            self.vol3d.fly_speed = SPEED;
        }
    }

    fn assert_finite(vol3d: &Vol3d, what: &str) {
        for (name, value) in [
            ("yaw", vol3d.yaw),
            ("pitch", vol3d.pitch),
            ("dist", vol3d.dist),
            ("fly_x", vol3d.fly_x),
            ("fly_y", vol3d.fly_y),
            ("fly_z", vol3d.fly_z),
        ] {
            assert!(value.is_finite(), "{what}: {name} became {value}");
        }
        let in_reach = vol3d.fly_x.abs() <= FLY_RANGE
            && vol3d.fly_y.abs() <= FLY_RANGE
            && (FLY_FLOOR..=FLY_RANGE).contains(&vol3d.fly_z);
        assert!(in_reach, "{what}: the eye escaped the reachable region");
    }

    #[test]
    fn re_entering_fly_mode_does_not_drag_the_eye_back_to_the_orbit() {
        let mut vol3d = Vol3d::default();
        set_camera_mode(&mut vol3d, Vol3dCameraMode::Fly);
        vol3d.fly_x = 4.0;
        vol3d.fly_y = -0.5;
        vol3d.fly_z = 0.25;

        // Choosing "Fly" while already flying must be a no-op, or every stray
        // click on the toolbar would teleport the operator back outside.
        set_camera_mode(&mut vol3d, Vol3dCameraMode::Fly);
        assert_eq!([vol3d.fly_x, vol3d.fly_y, vol3d.fly_z], [4.0, -0.5, 0.25]);

        // Leaving and returning re-seeds, which is what makes the switch
        // predictable rather than a return to wherever you last were.
        set_camera_mode(&mut vol3d, Vol3dCameraMode::Orbit);
        set_camera_mode(&mut vol3d, Vol3dCameraMode::Fly);
        assert_eq!([vol3d.fly_x, vol3d.fly_y, vol3d.fly_z], vol3d.orbit_eye());
    }

    #[test]
    fn flying_forward_for_one_second_lands_where_the_arithmetic_says() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.hover();

        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], DT);
        for _ in 1..60 {
            bench.frame(Vec::new(), DT);
        }

        // Sixty frames of `fly_speed * SPEED_NEAR * DT` along the view
        // direction, which at yaw = pitch = 0 is exactly -x:
        //   60 * 1.2 * 0.15 * (1/60) = 0.18 box half-widths, 10.8 km on the
        // default box, so crossing all 120 km takes about eleven seconds.
        let expected = -(SPEED * SPEED_NEAR);
        assert!(
            (bench.vol3d.fly_x - expected).abs() < 1.0e-5,
            "x was {} not {expected}",
            bench.vol3d.fly_x
        );
        assert!(bench.vol3d.fly_y.abs() < 1.0e-6, "y drifted");
        assert!((bench.vol3d.fly_z - 0.3).abs() < 1.0e-6, "z drifted");
    }

    #[test]
    fn a_five_second_stall_moves_one_clamped_frame_and_no_further() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.hover();

        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], 5.0);

        // Not five seconds of flight - that would be 0.9 half-widths, most of
        // the way across the box - but one clamped frame.
        let expected = -(SPEED * SPEED_NEAR * MAX_FRAME_DT);
        assert!(
            (bench.vol3d.fly_x - expected).abs() < 1.0e-6,
            "a stalled frame moved to {} instead of {expected}",
            bench.vol3d.fly_x
        );
        assert!(bench.vol3d.fly_x.abs() < 0.02, "the stall moved too far");
    }

    #[test]
    fn a_negative_or_non_finite_frame_time_moves_nothing() {
        for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            let mut bench = Bench::new();
            bench.park_inside_the_box();
            bench.hover();
            let press = bench.key(egui::Key::W, true);
            bench.frame(vec![press], dt);
            assert_eq!(bench.eye(), [0.0, 0.0, 0.3], "dt {dt} moved the camera");
        }
    }

    #[test]
    fn dragging_right_turns_right_and_dragging_down_tips_the_view_down() {
        // Pins the sign convention against the orbit drag the pane has always
        // had; flipping either sign would make the two cameras disagree about
        // which way the mouse turns them.
        let mut vol3d = Vol3d {
            yaw: 0.0,
            pitch: 0.0,
            ..Vol3d::default()
        };
        assert!(apply_look(&mut vol3d, egui::vec2(10.0, 0.0)));
        assert!((vol3d.yaw + 0.1).abs() < 1.0e-6, "yaw was {}", vol3d.yaw);

        vol3d.yaw = 0.0;
        // From a pitch the orbit camera allows, so this measures the drag and
        // not [`ORBIT_PITCH_MIN`].
        vol3d.pitch = 0.5;
        assert!(apply_look(&mut vol3d, egui::vec2(0.0, 10.0)));
        assert!(
            (vol3d.pitch - 0.6).abs() < 1.0e-6,
            "pitch was {}",
            vol3d.pitch
        );
        // Positive pitch tips the fly camera's forward vector downwards.
        assert!(vol3d.fly_forward()[2] < 0.0);
    }

    #[test]
    fn pitch_stops_at_the_fly_limit_however_long_the_drag() {
        let mut bench = Bench::new();
        set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Fly);
        bench.hover();
        bench.press();

        // 200 points a frame is two radians of pitch a frame at the module's
        // sensitivity, so forty frames ask for eighty radians in one direction.
        let mut pointer = bench.center();
        for _ in 0..40 {
            pointer.y += 200.0;
            bench.move_pointer(pointer);
            assert!(
                bench.vol3d.pitch <= FLY_PITCH_LIMIT + 1.0e-6,
                "pitch reached {}",
                bench.vol3d.pitch
            );
        }
        assert!(
            (bench.vol3d.pitch - FLY_PITCH_LIMIT).abs() < 1.0e-6,
            "the drag never reached the stop: {}",
            bench.vol3d.pitch
        );

        for _ in 0..40 {
            pointer.y -= 200.0;
            bench.move_pointer(pointer);
            assert!(
                bench.vol3d.pitch >= -FLY_PITCH_LIMIT - 1.0e-6,
                "pitch reached {}",
                bench.vol3d.pitch
            );
        }
        assert!((bench.vol3d.pitch + FLY_PITCH_LIMIT).abs() < 1.0e-6);
    }

    #[test]
    fn no_input_sequence_leaves_the_camera_non_finite() {
        let mut bench = Bench::new();
        set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Fly);
        bench.hover();

        // Poison every field this module reads.
        bench.vol3d.yaw = f32::NAN;
        bench.vol3d.pitch = f32::INFINITY;
        bench.vol3d.dist = f32::NAN;
        bench.vol3d.fly_x = f32::NAN;
        bench.vol3d.fly_y = f32::NEG_INFINITY;
        bench.vol3d.fly_z = f32::NAN;
        bench.vol3d.fly_speed = f32::NAN;
        bench.modifiers = egui::Modifiers {
            shift: true,
            ctrl: true,
            ..egui::Modifiers::default()
        };

        // Every axis pushed the same way at once, so nothing cancels out.
        let held: Vec<egui::Event> = [KEYS_FORWARD, KEYS_RIGHT, KEYS_UP]
            .iter()
            .flat_map(|table| table.iter())
            .map(|key| bench.key(*key, true))
            .collect();
        bench.frame(held, f32::NAN);
        assert_finite(&bench.vol3d, "poisoned state with every key down");

        for dt in [f32::INFINITY, -1.0, 1.0e30, 0.0, 5.0, DT] {
            bench.frame(Vec::new(), dt);
            assert_finite(&bench.vol3d, "held keys");
        }

        // And a pointer that jumps a million points a frame, then to NaN.
        bench.press();
        let mut pointer = bench.center();
        for _ in 0..20 {
            pointer.x += 1.0e6;
            pointer.y += 1.0e6;
            bench.move_pointer(pointer);
            assert_finite(&bench.vol3d, "runaway drag");
        }
        bench.move_pointer(egui::pos2(f32::NAN, f32::NAN));
        assert_finite(&bench.vol3d, "NaN pointer");

        // Non-finite wheel travel.
        let wheel = bench.wheel(f32::INFINITY);
        bench.frame(vec![wheel], DT);
        assert_finite(&bench.vol3d, "infinite wheel");
    }

    #[test]
    fn nothing_moves_while_the_pointer_is_off_the_canvas() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.hover();
        bench.leave();

        let before = bench.eye();
        let (yaw, pitch) = (bench.vol3d.yaw, bench.vol3d.pitch);
        let press = bench.key(egui::Key::W, true);
        assert!(!bench.frame(vec![press], DT), "the camera moved off-canvas");
        for _ in 0..30 {
            assert!(!bench.frame(Vec::new(), DT), "the camera moved off-canvas");
        }
        let wheel = bench.wheel(50.0);
        assert!(
            !bench.frame(vec![wheel], DT),
            "the wheel reached off-canvas"
        );

        assert_eq!(bench.eye(), before);
        assert_eq!((bench.vol3d.yaw, bench.vol3d.pitch), (yaw, pitch));
    }

    #[test]
    fn a_focused_text_field_keeps_the_arrow_keys() {
        let mut bench = Bench::with_focused_text_field();
        bench.park_inside_the_box();
        bench.hover();
        // `text_edit_focused` needs the field's state stored, which happens at
        // the end of its first frame.
        assert!(
            bench.ctx.text_edit_focused(),
            "the stand-in filter field is not focused"
        );

        let before = bench.eye();
        let press = bench.key(egui::Key::ArrowUp, true);
        bench.frame(vec![press], DT);
        for _ in 0..30 {
            bench.frame(Vec::new(), DT);
        }
        assert_eq!(
            bench.eye(),
            before,
            "the product picker's arrows flew the camera"
        );
        assert!(
            !bench.focused,
            "the camera took the keyboard from a text field"
        );
    }

    #[test]
    fn flying_takes_the_keyboard_and_hands_it_back_on_the_way_out() {
        // `pane_canvas::keyboard_nav` binds the same W/A/S/D and arrows to pan
        // the active 2D pane and stands down only when something wants the
        // keyboard. This is the whole collision resolution, in one test.
        let mut bench = Bench::new();
        bench.hover();
        assert!(
            !bench.ctx.egui_wants_keyboard_input(),
            "orbiting uses no keys and must not take them from the 2D panes"
        );

        set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Fly);
        bench.frame(Vec::new(), DT);
        assert!(bench.focused, "the fly camera did not claim the keyboard");
        assert!(
            bench.ctx.egui_wants_keyboard_input(),
            "the 2D panes would still pan while the camera flies"
        );

        bench.leave();
        assert!(
            !bench.focused,
            "the camera kept the keyboard after the pointer left"
        );
        assert!(
            !bench.ctx.egui_wants_keyboard_input(),
            "the 2D panes never got their keys back"
        );
    }

    #[test]
    fn speed_grows_with_distance_from_the_box() {
        // On the multiplier itself, where the arithmetic is exact: measuring
        // it through a frame of flight instead costs more precision to
        // catastrophic cancellation than the effect being measured.
        let mut vol3d = Vol3d {
            // `Vol3d::default` parks the fly eye outside the box at y = -2.4.
            fly_x: 0.0,
            fly_y: 0.0,
            fly_z: 0.3,
            ..Vol3d::default()
        };
        let plain = egui::Modifiers::default();
        assert!((speed_scale(&vol3d, plain) - SPEED_NEAR).abs() < 1.0e-6);
        // Three half-widths out is two clear of the box face.
        vol3d.fly_x = 3.0;
        assert!((speed_scale(&vol3d, plain) - (SPEED_NEAR + 2.0)).abs() < 1.0e-5);
        // And it stops growing, so no approach is unsteerable.
        vol3d.fly_x = FLY_RANGE;
        assert!((speed_scale(&vol3d, plain) - SPEED_FAR).abs() < 1.0e-6);

        // And that the multiplier reaches the eye rather than just existing.
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.hover();
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], DT);
        let inside = bench.vol3d.fly_x.abs();
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.vol3d.fly_x = 3.0;
        bench.hover();
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], DT);
        let outside = 3.0 - bench.vol3d.fly_x;
        assert!(
            outside > inside * 10.0,
            "one frame outside covered {outside} against {inside} inside"
        );
    }

    #[test]
    fn shift_moves_four_times_faster_and_ctrl_four_times_slower() {
        fn one_frame(modifiers: egui::Modifiers) -> f32 {
            let mut bench = Bench::new();
            bench.park_inside_the_box();
            bench.modifiers = modifiers;
            bench.hover();
            let press = bench.key(egui::Key::W, true);
            bench.frame(vec![press], DT);
            bench.vol3d.fly_x.abs()
        }

        let plain = one_frame(egui::Modifiers::default());
        let boosted = one_frame(egui::Modifiers {
            shift: true,
            ..egui::Modifiers::default()
        });
        let fine = one_frame(egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::default()
        });
        assert!((boosted / plain - BOOST_MULTIPLIER).abs() < 1.0e-3);
        assert!((fine / plain - PRECISION_MULTIPLIER).abs() < 1.0e-3);
    }

    #[test]
    fn a_diagonal_is_no_faster_than_a_straight_line() {
        fn one_frame(keys: &[egui::Key]) -> f32 {
            let mut bench = Bench::new();
            bench.park_inside_the_box();
            bench.hover();
            let events = keys.iter().map(|key| bench.key(*key, true)).collect();
            bench.frame(events, DT);
            let [x, y, z] = bench.eye();
            (x * x + y * y + (z - 0.3) * (z - 0.3)).sqrt()
        }

        let straight = one_frame(&[egui::Key::W]);
        let diagonal = one_frame(&[egui::Key::W, egui::Key::D, egui::Key::E]);
        assert!(
            (diagonal - straight).abs() < 1.0e-6,
            "three keys at once moved {diagonal} against {straight}"
        );
    }

    #[test]
    fn the_wheel_dollies_the_eye_while_flying_and_the_radius_while_orbiting() {
        let mut flying = Bench::new();
        flying.park_inside_the_box();
        flying.hover();
        let radius_before = flying.vol3d.dist;
        for _ in 0..6 {
            let wheel = flying.wheel(50.0);
            flying.frame(vec![wheel], DT);
        }
        // Scrolling up moves the eye along the view direction, -x here.
        // This is what the orbit radius could not provide: it has a floor.
        assert!(
            flying.vol3d.fly_x < -1.0e-4,
            "the wheel did not dolly the eye: {}",
            flying.vol3d.fly_x
        );
        assert_eq!(
            flying.vol3d.dist, radius_before,
            "the wheel changed the orbit radius while flying"
        );

        let mut orbiting = Bench::new();
        orbiting.hover();
        let eye_before = orbiting.eye();
        for _ in 0..6 {
            let wheel = orbiting.wheel(50.0);
            orbiting.frame(vec![wheel], DT);
        }
        assert!(
            orbiting.vol3d.dist < radius_before,
            "the wheel did not shorten the orbit radius"
        );
        assert!(orbiting.vol3d.dist >= orbit_radius_floor(&orbiting.vol3d));
        assert_eq!(orbiting.eye(), eye_before, "orbiting moved the fly eye");
    }

    #[test]
    fn entering_fly_mode_from_a_scrolled_out_orbit_does_not_drop_the_eye() {
        // Why this module translates rather than calling
        // `Vol3d::apply_fly_movement`: that helper clamps z into
        // [-1, zspan + 1], and a scrolled-out orbit eye starts above it.
        let mut bench = Bench::new();
        bench.vol3d.dist = ORBIT_DIST_MAX;
        let orbit_eye = bench.vol3d.orbit_eye();
        assert!(
            orbit_eye[2] > bench.vol3d.zspan() + 1.0,
            "the fixture no longer reproduces the case: {}",
            orbit_eye[2]
        );

        set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Fly);
        assert_eq!(bench.eye(), orbit_eye);
        bench.hover();
        assert_eq!(bench.eye(), orbit_eye, "idling moved the seeded eye");

        let before_z = bench.vol3d.fly_z;
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], DT);
        assert!(
            (before_z - bench.vol3d.fly_z) < 0.1,
            "the first keypress dropped the eye from {before_z} to {}",
            bench.vol3d.fly_z
        );
    }

    #[test]
    fn no_flight_key_is_bound_twice_or_to_a_key_this_application_reserves() {
        let mut seen: Vec<egui::Key> = Vec::new();
        for table in FLIGHT_KEY_TABLES {
            for key in *table {
                assert!(!seen.contains(key), "{key:?} drives two axes");
                seen.push(*key);
            }
        }

        // Enter and Escape belong to the product picker and to `popup.rs`;
        // Home, Plus, Equals and Minus belong to `pane_canvas`; Space and Tab
        // belong to egui itself, which turns them into activation and focus
        // movement on the very widget the camera is holding.
        for reserved in [
            egui::Key::Enter,
            egui::Key::Escape,
            egui::Key::Home,
            egui::Key::Plus,
            egui::Key::Equals,
            egui::Key::Minus,
            egui::Key::Space,
            egui::Key::Tab,
        ] {
            assert!(
                !seen.contains(&reserved),
                "{reserved:?} is already spoken for"
            );
        }
    }

    #[test]
    fn the_module_never_writes_the_camera_mode_itself() {
        // `enter_fly_mode` carries the no-jump seeding and both entry points
        // carry their own pitch clamp. An assignment anywhere in this file
        // would skip them, and the two paths would drift apart silently.
        let source = include_str!("camera.rs");
        // Spelled at run time so this test is not itself a match.
        let assignment = format!(".camera_mode {} ", '=');
        assert!(
            !source.contains(&assignment),
            "something in this module writes the mode field instead of going \
             through enter_orbit_mode / enter_fly_mode"
        );
        assert!(source.contains("vol3d.enter_orbit_mode()"));
        assert!(source.contains("vol3d.enter_fly_mode()"));
    }

    // ------------------------------------------------------------------
    // Frame rate, mode switching, clamps, focus.
    // ------------------------------------------------------------------

    /// The eye and view direction the renderer will actually use, whichever
    /// camera is driving.
    ///
    /// `Vol3d::camera_basis` is the function the shader uniform is built from,
    /// so a mode switch that leaves both of these alone is a switch the
    /// operator cannot see - which is the whole no-jump contract.
    fn rendered_view(vol3d: &Vol3d) -> ([f32; 3], [f32; 3]) {
        let (eye, forward, _, _) = vol3d.camera_basis().expect("the camera basis degenerated");
        (eye, forward)
    }

    fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
        let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// `hz` frames of exactly one `hz`th of a second: one simulated second.
    fn even_frames(hz: usize) -> Vec<f32> {
        vec![1.0 / hz as f32; hz]
    }

    /// One simulated second of frames alternating 45 ms and 5 ms - a machine
    /// hitching every other frame, which is what a real one under load does.
    fn jittery_frames() -> Vec<f32> {
        let mut frames = Vec::new();
        for _ in 0..20 {
            frames.push(0.045);
            frames.push(0.005);
        }
        frames
    }

    /// Hold W for the given frame times, starting at `start_x` and facing
    /// `yaw`, and report where the eye ended up.
    fn fly_held_forward(start_x: f32, yaw: f32, frames: &[f32]) -> [f32; 3] {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.vol3d.fly_x = start_x;
        bench.vol3d.yaw = yaw;
        bench.hover();
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], frames[0]);
        for dt in &frames[1..] {
            bench.frame(Vec::new(), *dt);
        }
        bench.eye()
    }

    /// Facing this way, `fly_forward` is +x: straight out of the box along the
    /// axis the fixtures below measure.
    const FACING_OUT: f32 = std::f32::consts::PI;

    #[test]
    fn a_second_inside_the_box_ends_in_the_same_place_at_every_frame_rate() {
        let sixty = fly_held_forward(0.0, 0.0, &even_frames(60));
        for (label, frames) in [
            ("30 Hz", even_frames(30)),
            ("144 Hz", even_frames(144)),
            ("jittery", jittery_frames()),
        ] {
            let other = fly_held_forward(0.0, 0.0, &frames);
            let gap = distance(sixty, other);
            assert!(
                gap < 1.0e-4,
                "{label} ended {gap} half-widths from where 60 Hz ended ({other:?} vs {sixty:?})"
            );
        }
    }

    #[test]
    fn a_second_outside_the_box_ends_in_the_same_place_at_every_frame_rate() {
        // Outside the box each step changes the speed of the next, which is
        // where a camera integrated one frame at a time drifts apart between
        // machines: the same second of held W covers different ground.
        let sixty = fly_held_forward(3.0, FACING_OUT, &even_frames(60));
        let travelled = sixty[0] - 3.0;
        assert!(
            travelled > 1.0,
            "the fixture no longer leaves the box behind: {travelled}"
        );
        for (label, frames) in [
            ("30 Hz", even_frames(30)),
            ("144 Hz", even_frames(144)),
            ("jittery", jittery_frames()),
        ] {
            let other = fly_held_forward(3.0, FACING_OUT, &frames);
            let gap = distance(sixty, other);
            // A hundredth of a half-width is 600 m on the default 120 km box:
            // below anything an operator could see, after a second of flight
            // that covered several kilometres.
            assert!(
                gap < 0.01,
                "{label} ended {gap} half-widths from where 60 Hz ended after \
                 flying {travelled} ({other:?} vs {sixty:?})"
            );
        }
    }

    #[test]
    fn a_stalled_frame_cannot_carry_the_eye_across_the_box() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        // The worst case the controls allow: speed slider at its top stop,
        // boost held, eye far enough out that the distance multiplier is at
        // ITS ceiling too.
        bench.vol3d.fly_speed = MAX_FLY_SPEED;
        bench.vol3d.fly_x = 5.0;
        bench.vol3d.yaw = 0.0;
        bench.modifiers = egui::Modifiers {
            shift: true,
            ..egui::Modifiers::default()
        };
        bench.hover();

        let start = bench.vol3d.fly_x;
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], 5.0);
        let step = (start - bench.vol3d.fly_x).abs();
        // The box is two half-widths wide. A frame that moves further than one
        // has put the operator somewhere they cannot recognise, which is
        // exactly what the frame-time clamp exists to prevent.
        assert!(
            step < 1.0,
            "a five-second stall moved the eye {step} half-widths, across a box two wide"
        );
    }

    #[test]
    fn no_single_frame_of_wheel_carries_the_eye_through_the_box() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        // Four half-widths clear of the box face, where the distance
        // multiplier is at its ceiling: the approach an operator makes after
        // scrolling out for an establishing shot.
        bench.vol3d.fly_x = 5.0;
        bench.vol3d.yaw = 0.0;
        bench.hover();

        let mut worst = 0.0_f32;
        for _ in 0..12 {
            let before = bench.eye();
            let wheel = bench.wheel(50.0);
            bench.frame(vec![wheel], DT);
            worst = worst.max(distance(before, bench.eye()));
        }
        assert!(
            worst < 1.0,
            "one frame of wheel moved {worst} half-widths - the wheel is a zoom, \
             and a zoom that jumps through the storm is not usable"
        );
    }

    #[test]
    fn every_mode_switch_leaves_the_picture_where_it_was() {
        let mut bench = Bench::new();
        bench.hover();

        for round in 0..3 {
            let (eye_before, view_before) = rendered_view(&bench.vol3d);
            set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Fly);
            let (eye_after, view_after) = rendered_view(&bench.vol3d);
            assert!(
                distance(eye_before, eye_after) < 1.0e-4,
                "round {round}: entering fly moved the eye {} half-widths",
                distance(eye_before, eye_after)
            );
            let turn = distance(view_before, view_after);
            assert!(turn < 1.0e-4, "round {round}: entering fly turned the view");

            // Fly, which is the point of the mode. Backing straight out
            // along the orbit's own line of sight is a position the orbit
            // camera can describe exactly, so this switch has no excuse. The
            // one it cannot describe - closer than `orbit_radius_floor` - is
            // pinned by the test below.
            let press = bench.key(egui::Key::S, true);
            bench.frame(vec![press], DT);
            bench.coast(14, DT);
            let release = bench.key(egui::Key::S, false);
            bench.frame(vec![release], DT);

            let (eye_before, view_before) = rendered_view(&bench.vol3d);
            set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Orbit);
            let (eye_after, view_after) = rendered_view(&bench.vol3d);
            assert!(
                distance(eye_before, eye_after) < 1.0e-3,
                "round {round}: leaving fly teleported the eye {} half-widths, \
                 from {eye_before:?} to {eye_after:?}",
                distance(eye_before, eye_after)
            );
            let turn = distance(view_before, view_after);
            assert!(turn < 1.0e-3, "round {round}: leaving fly turned the view");
        }
    }

    #[test]
    fn flying_closer_than_the_orbit_allows_returns_to_the_closest_orbit() {
        // The one position the orbit camera cannot adopt. It steps out to its
        // own near stop, keeping the bearing the operator flew, rather than
        // back to wherever the orbit was pointed before the flight - which is
        // as close to not moving as the mode permits.
        let mut bench = Bench::new();
        set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Fly);
        bench.vol3d.fly_x = 0.4;
        bench.vol3d.fly_y = 0.0;
        bench.vol3d.fly_z = bench.vol3d.orbit_center()[2];
        set_camera_mode(&mut bench.vol3d, Vol3dCameraMode::Orbit);

        let floor = orbit_radius_floor(&bench.vol3d);
        let (eye, _) = rendered_view(&bench.vol3d);
        let radius = distance(eye, bench.vol3d.orbit_center());
        assert!(
            (radius - floor).abs() < 1.0e-3,
            "the eye came back at radius {radius}, not the {floor} near stop"
        );
        // Same bearing: straight out from where the operator left it, not
        // back to the old orbit on the far side of the box.
        assert!(eye[0] > 0.0 && eye[1].abs() < 1.0e-3, "bearing: {eye:?}");
    }

    #[test]
    fn the_orbit_drag_cannot_reach_a_view_the_orbit_camera_refuses() {
        // `Vol3d::enter_orbit_mode` clamps pitch into 0.03..=1.50: the orbit
        // eye is not allowed under the floor PPI. A drag below that stop does
        // not STAY there - the next mode switch pulls it back up, and the
        // operator loses the view they just set up.
        let mut bench = Bench::new();
        assert_eq!(bench.vol3d.camera_mode, Vol3dCameraMode::Orbit);
        bench.hover();
        bench.press();

        // 200 points a frame is two radians of pitch a frame at this
        // module's sensitivity, so forty frames ask for eighty.
        let mut pointer = bench.center();
        for _ in 0..40 {
            // Dragging DOWN tips the view down, which raises the orbit eye.
            pointer.y += 200.0;
            bench.move_pointer(pointer);
            assert!(bench.vol3d.pitch <= ORBIT_PITCH_MAX + 1.0e-6);
        }
        assert!((bench.vol3d.pitch - ORBIT_PITCH_MAX).abs() < 1.0e-6);
        for _ in 0..40 {
            pointer.y -= 200.0;
            bench.move_pointer(pointer);
            assert!(bench.vol3d.pitch >= ORBIT_PITCH_MIN - 1.0e-6);
        }

        let (dragged_eye, dragged_view) = rendered_view(&bench.vol3d);
        let dragged_pitch = bench.vol3d.pitch;
        bench.vol3d.enter_orbit_mode();
        let (settled_eye, settled_view) = rendered_view(&bench.vol3d);
        assert!(
            distance(dragged_eye, settled_eye) < 1.0e-4,
            "the drag reached pitch {dragged_pitch}, which the orbit camera \
             refuses: re-entering the mode jumped the eye {} half-widths",
            distance(dragged_eye, settled_eye)
        );
        assert!(distance(dragged_view, settled_view) < 1.0e-4);
    }

    #[test]
    fn one_enormous_drag_cannot_push_the_view_past_the_pole() {
        for mode in [Vol3dCameraMode::Orbit, Vol3dCameraMode::Fly] {
            for delta in [1.0e6_f32, -1.0e6, f32::MAX, -f32::MAX] {
                let mut vol3d = Vol3d::default();
                set_camera_mode(&mut vol3d, mode);
                apply_look(&mut vol3d, egui::vec2(delta, delta));
                assert!(
                    vol3d.pitch.abs() <= ORBIT_PITCH_MAX,
                    "{mode:?} reached pitch {} on a {delta} point drag",
                    vol3d.pitch
                );
                assert!(vol3d.yaw.is_finite(), "{mode:?}: yaw became {}", vol3d.yaw);
                // The clamp exists so the basis does not degenerate; this is
                // the assertion that actually checks that, rather than
                // checking the number the clamp produced.
                assert!(
                    vol3d.camera_basis().is_some(),
                    "{mode:?}: the view flipped at pitch {}",
                    vol3d.pitch
                );
            }
        }
    }

    #[test]
    fn a_degenerate_box_neither_stops_the_camera_nor_breaks_it() {
        // Every one is reachable: the box size combo, the exaggeration
        // slider and the focus height slider all feed `zspan`, and the speed
        // multiplier is measured against the box `zspan` describes.
        for (half_km, exaggeration, focus_km) in [
            (0.0_f32, 1.0_f32, 6.0_f32),
            (1.0e-6, 0.0, 0.0),
            (f32::NAN, 2.0, 6.0),
            (60.0, f32::NAN, f32::NAN),
            (f32::INFINITY, f32::INFINITY, 1.0e30),
        ] {
            let mut bench = Bench::new();
            bench.park_inside_the_box();
            bench.vol3d.box_half_km = half_km;
            bench.vol3d.vertical_exaggeration = exaggeration;
            bench.vol3d.focus_height_km = focus_km;
            bench.hover();

            let press = bench.key(egui::Key::W, true);
            bench.frame(vec![press], DT);
            bench.coast(30, DT);
            assert_finite(
                &bench.vol3d,
                &format!("box half {half_km} km, exaggeration {exaggeration}, focus {focus_km} km"),
            );
        }
    }

    #[test]
    fn an_hour_of_flight_stays_finite_and_within_reach() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.hover();
        bench.modifiers = egui::Modifiers {
            shift: true,
            ..egui::Modifiers::default()
        };

        let held: Vec<egui::Event> = [KEYS_FORWARD, KEYS_RIGHT, KEYS_UP]
            .iter()
            .flat_map(|table| table.iter())
            .map(|key| bench.key(*key, true))
            .collect();
        bench.frame(held, MAX_FRAME_DT);
        // Thirty-six thousand frames of the longest frame the camera will
        // integrate is one hour of simulated flight, every axis held, boosted.
        bench.coast(36_000, MAX_FRAME_DT);
        assert_finite(&bench.vol3d, "an hour of held keys");
        // Pinned against the far edge of the region, a held key is no longer
        // movement: the pane must be allowed to stop repainting.
        assert!(
            !bench.frame(Vec::new(), MAX_FRAME_DT),
            "the camera kept asking for repaints while pinned at the wall"
        );
    }

    #[test]
    fn the_camera_holds_still_while_the_pointer_is_over_a_control() {
        let mut bench = Bench::with_toolbar_slider();
        bench.park_inside_the_box();
        bench.hover();
        bench.hover_slider();

        let before = bench.eye();
        let speed_before = bench.slider_value;
        let press = bench.key(egui::Key::W, true);
        assert!(
            !bench.frame(vec![press], DT),
            "the camera moved while the pointer was on the speed slider"
        );
        for _ in 0..30 {
            assert!(
                !bench.frame(Vec::new(), DT),
                "the camera kept flying while the pointer was on the speed slider"
            );
        }
        assert_eq!(bench.eye(), before);
        assert!(
            !bench.focused,
            "the canvas held the keyboard while the pointer was on a control"
        );
        assert_eq!(
            bench.slider_value, speed_before,
            "W reached the slider instead of the camera"
        );
    }

    #[test]
    fn an_alt_tabbed_window_does_not_keep_flying() {
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.hover();
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], DT);
        assert!(bench.vol3d.fly_x < 0.0, "the fixture never started flying");

        // The key is still down as far as `InputState` knows - nothing
        // released it - so without the window-focus test inside
        // `Response::has_focus` the camera would fly on behind whatever the
        // operator switched to.
        bench.window_focused = false;
        let parked = bench.eye();
        for _ in 0..30 {
            assert!(
                !bench.frame(Vec::new(), DT),
                "the camera flew on with the application in the background"
            );
        }
        assert_eq!(bench.eye(), parked);
    }

    #[test]
    fn flying_with_the_arrows_does_not_hand_the_keyboard_to_the_toolbar() {
        // egui reads a plain arrow as "move focus that way" unless the
        // focused widget filters it out, and the filter cannot be set until
        // the widget has held focus for a whole frame. So the first arrow can
        // hand the keyboard to the nearest widget that way - for a canvas with
        // a toolbar above it, the fly-speed slider, which then EATS the strafe
        // keys to change its own value.
        let mut bench = Bench::with_toolbar_slider();
        bench.park_inside_the_box();
        bench.hover();
        let canvas = bench.canvas_id.expect("the canvas was never allocated");
        let speed_before = bench.slider_value;

        let hold = bench.key(egui::Key::ArrowUp, true);
        bench.frame(vec![hold], DT);
        for frame in 0..20 {
            // The operating system repeats a held key, so press events keep
            // arriving for as long as the operator is flying.
            let repeat = egui::Event::Key {
                key: egui::Key::ArrowUp,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: bench.modifiers,
            };
            let strafe = bench.key(egui::Key::ArrowRight, true);
            bench.frame(vec![repeat, strafe], DT);
            assert_eq!(
                bench.focus_after_pass,
                Some(canvas),
                "frame {frame}: egui moved the keyboard off the canvas mid-flight"
            );
        }
        assert_eq!(
            bench.slider_value, speed_before,
            "strafing with the arrows dragged the fly-speed slider"
        );
    }

    #[test]
    fn the_orbit_radius_floor_is_the_one_the_camera_uses() {
        // `orbit_radius_floor` mirrors a number owned by
        // `Vol3d::orbit_distance`, and this mirror rotting would put the dead
        // zone back with nothing failing. Check it against the original at
        // every box size and exaggeration the pane offers.
        for half_km in [60.0_f32, 120.0, 180.0] {
            for exaggeration in [0.5_f32, 1.5, 3.0, 6.0] {
                let mut vol3d = Vol3d {
                    box_half_km: half_km,
                    vertical_exaggeration: exaggeration,
                    ..Vol3d::default()
                };
                vol3d.dist = ORBIT_DIST_MIN;
                let floor = orbit_radius_floor(&vol3d);
                assert!(
                    (vol3d.orbit_distance() - floor).abs() < 1.0e-6,
                    "{half_km} km box at {exaggeration}x: the camera uses {} \
                     where this module thinks the floor is {floor}",
                    vol3d.orbit_distance()
                );
                assert!(
                    floor > ORBIT_DIST_MIN,
                    "the fixture stopped exercising the floor at {half_km}/{exaggeration}"
                );
            }
        }
    }

    #[test]
    fn the_orbit_wheel_has_no_dead_zone_at_the_near_stop() {
        // Every notch used to shrink `dist` towards 0.35 while
        // `orbit_distance` held the eye at 1.3 or more, so a dozen notches
        // moved the number and not the picture - the control stopping
        // answering before it stops turning.
        let mut bench = Bench::new();
        bench.hover();
        for _ in 0..40 {
            let wheel = bench.wheel(50.0);
            bench.frame(vec![wheel], DT);
        }
        assert!(
            (bench.vol3d.dist - bench.vol3d.orbit_distance()).abs() < 1.0e-6,
            "the wheel wound the radius to {} while the eye stayed at {}",
            bench.vol3d.dist,
            bench.vol3d.orbit_distance()
        );
        // And once there, more wheel is not movement to repaint for.
        let wheel = bench.wheel(50.0);
        assert!(
            !bench.frame(vec![wheel], DT),
            "the wheel kept reporting movement at the near stop"
        );
    }

    #[test]
    fn the_two_dimensional_panes_still_stand_down_for_whoever_holds_the_keyboard() {
        // This module's collision resolution is not local. `pane_canvas`
        // binds the same W/A/S/D and arrows to pan the active 2D pane, and the
        // only thing keeping the map still under a flying camera is its early
        // return while something else wants the keyboard. If that guard goes,
        // the claim here protects nothing and the map silently drifts. So pin
        // it from the side that depends on it.
        let source = include_str!("../pane_canvas.rs");
        assert!(
            source.contains("if !active || ui.ctx().egui_wants_keyboard_input() {"),
            "pane_canvas::keyboard_nav no longer stands down for the widget \
             holding the keyboard, so the 2D panes now pan while the 3D camera flies"
        );
    }

    #[test]
    fn the_speed_slider_covers_the_floor_the_camera_applies() {
        // `MIN_FLY_SPEED` is both the slider's lower stop and the floor on
        // whatever `fly_speed` holds, so no value can freeze the camera.
        let mut bench = Bench::new();
        bench.park_inside_the_box();
        bench.vol3d.fly_speed = 0.0;
        bench.hover();
        let press = bench.key(egui::Key::W, true);
        bench.frame(vec![press], DT);
        let expected = -(MIN_FLY_SPEED * SPEED_NEAR * DT);
        assert!(
            (bench.vol3d.fly_x - expected).abs() < 1.0e-9,
            "a zero speed moved {} not {expected}",
            bench.vol3d.fly_x
        );
    }
}
