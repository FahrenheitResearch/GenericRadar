//! The wireframe, the height ladder and the ground-plane orientation labels.
//!
//! Everything drawn here is a 2D overlay painted on top of the ray-marched
//! volume, through the same camera the shader uses ([`Vol3d::project_point`]).
//! Self-contained in the style of [`super::pane`] and [`super::camera`]:
//! [`draw`] takes the explorer, a painter and the pane rectangle, and reaches
//! into the application nowhere.
//!
//! # Why this exists
//!
//! The pane used to paint a floor grid and a box wireframe and nothing else. A
//! wireframe with no numbers on it says a storm is "about this tall", which is
//! not a statement anybody can put in a warning. GR2Analyst's 3D view is easier
//! to read for two reasons that have nothing to do with its renderer: a box
//! that frames the storm, and a labelled height scale up the side of that box.
//! Those two things are what this module adds. It is not a clone of that view -
//! no attempt is made to copy its styling, its floor rings or its layout.
//!
//! # Why kilofeet
//!
//! The box is built in metres and the shader knows nothing but normalised
//! units, but a warning forecaster reads storm tops in kilofeet: "tops to 55"
//! is the phrase, and "16 764 m" is not. So the ladder converts once, at the
//! label, through [`product_engine::units::METERS_TO_KILOFEET`] - the exact
//! definition, since a foot is exactly 0.3048 m, not a rounded literal - and
//! names the unit ONCE at the head of the ladder rather than suffixing every
//! rung. Five short numbers in a column read as a scale; five numbers each
//! carrying "kft" read as a paragraph.
//!
//! # Why the ladder is derived and not hardcoded
//!
//! 10/20/30/40/50 is the ladder an 18 km box wants, and writing those five
//! numbers down would be wrong the first time somebody changed
//! [`super::BOX_TOP_M`]. The rungs come from
//! [`product_engine::ticks::nice_ticks`], the same Heckbert ladder the 2D
//! legend uses - Paul S. Heckbert, "Nice Numbers for Graph Labels", in Andrew
//! S. Glassner (ed.), *Graphics Gems*, Academic Press, 1990, pp. 61-63 (ISBN
//! 0-12-286165-5). An 18 km box (59.06 kft) gets 10/20/30/40/50 out of it; a
//! 30 km box gets 20/40/60/80 instead of ten crowded rungs, which is the whole
//! point of asking.
//!
//! # Why the ladder moves from edge to edge
//!
//! A ladder painted on a fixed corner is unreadable half the time: orbit past a
//! quarter turn and the front-left edge is now the back-right one, with the
//! whole storm in front of it. [`ladder_spine`] picks the LEFT-HAND SILHOUETTE
//! edge instead - of the four vertical edges, the leftmost one that still has
//! the whole projected box behind it. Nothing in the box can then project
//! outward of that edge, so the tick arms and the labels hung outward from it
//! lie over empty sky rather than over the storm.
//!
//! # Why the edge is chosen from the projection and not from the bearing
//!
//! Because a bearing rule gets it wrong, and not marginally. A rule that reads
//! only the camera's bearing has to put its switchovers on the diagonals of the
//! footprint. The real ones are where the EYE CROSSES THE PLANE OF A FACE,
//! which for an eye 2.16 half-widths out - the pane's own default - is 27.6
//! degrees off the diagonal, so 31 per cent of a turn falls between the two.
//! In there the bearing rule hangs the ladder on an edge that is behind the
//! volume. At a camera bearing of 179.9 degrees, due west of the box and
//! looking at its west face, the diagonal rule names the NORTH-EAST edge - the
//! far side - and the labels land 216 points inside the silhouette, straight
//! across the storm. The band is `90 - acos(1 / R)` degrees wide per quadrant
//! for an eye `R` half-widths out, so it widens as the camera closes in.
//!
//! Choosing from the projected corners also puts the switchover where it
//! belongs. It happens as the eye crosses a face plane, and a plane THROUGH the
//! eye projects to a line: at that instant the two candidate edges lie on one
//! screen line, so the ladder slides along itself instead of jumping across the
//! box. There is nothing here for hysteresis to fix - the choice is a function
//! of the camera alone, so it cannot oscillate while the camera is still, it
//! changes once as the operator drags through the crossing, and both sides of
//! that crossing draw the same line.
//!
//! # Why the box top is `zspan` and not 1.0
//!
//! The shader ray-marches `[-1,-1,0] .. [1,1,zspan]` (see `fs_main`, and
//! `clip_high_z = clip_high * u.zspan`). The wireframe this module replaces
//! drew its top at a literal `1.0`, and [`Vol3d::zspan`] is only ever `1.0` by
//! coincidence - it is `top_km / box_half_km * exaggeration`, so every box size
//! and every exaggeration setting moves it. The drawn box therefore stood taller
//! or shorter than the volume inside it by whatever that ratio happened to be,
//! and a height ladder hung on that box would have read every echo top wrong by
//! the same factor. Everything here takes the top from [`Vol3d::zspan`], so the
//! ladder measures the box the shader actually drew.
//!
//! # Integration
//!
//! `vol3d/pane.rs` should delete its own `draw_annotations` entirely - the
//! floor grid and the wireframe move here, so the two cannot drift apart about
//! where the top of the box is - and call, at the end of `canvas`:
//!
//! ```ignore
//! annotations::draw(vol3d, &ui.painter_at(rect), rect);
//! ```
//!
//! `rect` must be the rectangle the shader callback was given AND the painter's
//! clip rectangle. The first is what makes [`Vol3d::project_point`] agree with
//! the WGSL camera; the second is what keeps a ladder that runs off a zoomed-in
//! pane clipped to the pane instead of drawn over the panel beside it.

// Finished drawing code is "never used" until the pane calls it, and clippy
// runs with `-D warnings`. Delete this attribute in the commit that pastes the
// call above into `vol3d/pane.rs`.
#![allow(dead_code)]

use eframe::egui;
use product_engine::ticks::nice_ticks;
use product_engine::units::{DisplayUnit, METERS_TO_KILOFEET};

use super::{Vol3d, Vol3dCameraMode};

/// Half-width of the box footprint, in the world units
/// [`Vol3d::project_point`] takes. The resampled box spans -1..1 in x (east)
/// and y (north); its top is [`Vol3d::zspan`], which is a different number.
const FOOTPRINT_HALF: f32 = 1.0;

/// Floor grid lines per axis, both edges included.
///
/// Five each way is the spacing the pane has always drawn, kept exactly: four
/// cells across the box, so each cell is a quarter of the box side whatever
/// side `box_frame` currently offers.
const GRID_LINES: usize = 5;

/// Intervals the height ladder asks [`nice_ticks`] for.
///
/// A wish, not a promise - the ladder answers with the round step whose
/// interval count lands nearest. Five is what puts 10/20/30/40/50 on the 18 km
/// box: a step of 10 kft cuts 59.06 kft into 5.91 intervals, and the next rung
/// up (20 kft) manages only 2.95.
const LADDER_TARGET_INTERVALS: u8 = 5;

/// Length of a tick arm, and the gap between the arm and its label, in points.
const TICK_ARM_PX: f32 = 9.0;
const TICK_LABEL_GAP_PX: f32 = 4.0;

/// Shortest projected box edge that can carry even one rung, points.
///
/// Looking straight down, or at the widest box with the exaggeration turned
/// down, the vertical edges collapse toward a point and every label stacks on
/// every other. Below one label height there is nothing left to read, so the
/// ladder is dropped rather than drawn as a smudge; an operator looking
/// straight down is reading a plan view and is not reading heights off it.
const MIN_LADDER_PX: f32 = 16.0;

/// The box two rung labels must not share, in points: a row height and a column
/// width, because a label is a RECTANGLE and two of them clear one another by
/// separating on either axis, not by being some distance apart.
///
/// Euclidean distance is the wrong measure and the trap is worth spelling out.
/// A ladder leaning at 33 degrees puts consecutive rungs 14.5 points apart and
/// only 12.2 of that is vertical, so two 13-point-tall labels still overlap by
/// most of a line while passing a 13-point distance test. Both axes, and an
/// `AND` between them: labels collide only when they are too close on BOTH.
///
/// The row is a laid-out rung label (13.0 points at an 11 point monospace font,
/// pinned by a test that lays one out) plus two points of air. The column is
/// the widest label this ladder can write - "0.25", four characters at 6.63
/// points each - because the labels are anchored on one edge, so two of them
/// clear horizontally only once they are a whole label apart.
const LABEL_ROW_PX: f32 = 15.0;
const LABEL_COLUMN_PX: f32 = 28.0;

/// Rung spacing is compared against the MEASURED distance between two projected
/// rungs, never against `spine / rungs`. That estimate is wrong in both of the
/// two ways available to it: the rungs do not span the whole edge (the top rung
/// of an 18 km box stands at 0.85 of it), and perspective bunches the upper
/// rungs together. Both make the real gap SMALLER than the estimate, so a
/// seven-rung ladder could pass the estimate at 13 points and still print its
/// labels 12.8 apart before perspective was counted at all. Measuring costs one
/// projection per rung per halving, and cannot be wrong.
fn label_gap() -> egui::Vec2 {
    egui::vec2(LABEL_COLUMN_PX, LABEL_ROW_PX)
}

/// How far the rest of the box may reach past an edge before that edge counts
/// as inside the silhouette rather than on it, in points.
///
/// Zero is the exact answer: a silhouette edge has the whole projected box
/// strictly behind it. It is not zero here because the switchover between two
/// edges happens exactly as the eye crosses the plane of the face between them,
/// and a plane through the eye projects to a line - at that instant both
/// candidates lie ON the line, both clearances are zero, and which side of zero
/// each lands on is a rounding decision. Half a point of slack keeps a ladder
/// drawn through the crossing, and is half the width of the stroke it protects.
const SILHOUETTE_TOLERANCE_PX: f32 = 0.5;

/// How far outside the footprint the cardinal labels sit, in box half-widths.
///
/// OUTSIDE, not inside: the floor of the box carries the low-tilt PPI or the
/// column-maximum projection, and pale text over a 60 dBZ core is unreadable.
/// A tenth of a half-width clears the wireframe without floating free of it.
const CARDINAL_OFFSET: f32 = 1.09;

/// Rung labels are monospaced so the digit columns line up down the ladder; the
/// cardinals are proportional and larger, because they are read at a glance and
/// never compared with one another.
const TICK_FONT_SIZE: f32 = 11.0;
const CARDINAL_FONT_SIZE: f32 = 14.0;

/// Decimal places a rung label may reach before [`ladder_label_decimals`] gives
/// up. Every box top this application can produce lands on whole kilofeet; the
/// cap exists so that a future 500 m box gets "0.5" rungs rather than a ladder
/// of identical "0"s.
const MAX_LABEL_DECIMALS: usize = 2;

/// The pane's own grid and wireframe colours, carried over unchanged.
const GRID_COLOR: egui::Color32 = egui::Color32::from_rgb(38, 48, 62);
const BOX_COLOR: egui::Color32 = egui::Color32::from_rgb(70, 88, 110);

/// The ladder spine is deliberately brighter than the wireframe it lies on top
/// of. That is what tells the operator WHICH of the four edges carries the
/// scale, at a glance, without reading a label.
const LADDER_COLOR: egui::Color32 = egui::Color32::from_rgb(126, 148, 176);
const TICK_LABEL_COLOR: egui::Color32 = egui::Color32::from_rgb(196, 210, 226);
const UNIT_LABEL_COLOR: egui::Color32 = egui::Color32::from_rgb(138, 156, 178);
const CARDINAL_COLOR: egui::Color32 = egui::Color32::from_rgb(150, 170, 196);

const HAIRLINE: f32 = 1.0;

/// One of the four vertical edges of the box, named by the footprint corner it
/// stands on.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LadderEdge {
    SouthWest,
    SouthEast,
    NorthEast,
    NorthWest,
}

impl LadderEdge {
    /// Anticlockwise around the footprint, so consecutive entries share a wall.
    pub const ALL: [Self; 4] = [
        Self::SouthWest,
        Self::SouthEast,
        Self::NorthEast,
        Self::NorthWest,
    ];

    /// (east, north) of the corner, in box half-widths.
    pub fn footprint(self) -> (f32, f32) {
        match self {
            Self::SouthWest => (-FOOTPRINT_HALF, -FOOTPRINT_HALF),
            Self::SouthEast => (FOOTPRINT_HALF, -FOOTPRINT_HALF),
            Self::NorthEast => (FOOTPRINT_HALF, FOOTPRINT_HALF),
            Self::NorthWest => (-FOOTPRINT_HALF, FOOTPRINT_HALF),
        }
    }

    /// Bearing of the corner from the box centre, radians, measured the way
    /// [`Vol3d::yaw`] is: zero along +east, increasing toward +north. The four
    /// land on 45, 135, 225 and 315 degrees.
    pub fn bearing(self) -> f32 {
        let (east, north) = self.footprint();
        north.atan2(east)
    }
}

/// One of the four ground-plane orientation labels.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Cardinal {
    North,
    South,
    East,
    West,
}

impl Cardinal {
    pub const ALL: [Self; 4] = [Self::North, Self::South, Self::East, Self::West];

    pub fn label(self) -> &'static str {
        match self {
            Self::North => "NORTH",
            Self::South => "SOUTH",
            Self::East => "EAST",
            Self::West => "WEST",
        }
    }

    /// Outward normal of the floor edge this label sits on, (east, north) in
    /// box half-widths.
    ///
    /// +x is east and +y is north because that is how the box is resampled:
    /// `volume_box_resample_moment_with_support` walks `north` over the outer
    /// loop and `east` over the inner one, and the shader reads
    /// `uvw = ((x + 1) / 2, (y + 1) / 2, z / zspan)`.
    pub fn outward(self) -> (f32, f32) {
        match self {
            Self::North => (0.0, 1.0),
            Self::South => (0.0, -1.0),
            Self::East => (1.0, 0.0),
            Self::West => (-1.0, 0.0),
        }
    }
}

/// Paint the floor grid, the box wireframe, the height ladder and the
/// orientation labels, as the explorer's three toggles allow.
///
/// `show_grid` owns the floor grid and `show_box` the wireframe, exactly as
/// they always have. `show_labels` owns everything with a number or a word on
/// it: the height ladder and the cardinals. The ladder draws its own spine, so
/// it stays a legible scale with the wireframe switched off.
///
/// `rect` is the pane the volume was drawn into, and must be the same rectangle
/// the shader callback was given: [`Vol3d::project_point`] is the CPU companion
/// to the WGSL camera and shares its aspect handling.
pub fn draw(vol3d: &Vol3d, painter: &egui::Painter, rect: egui::Rect) {
    // One early out for the whole module. With all three toggles off this costs
    // three bool reads and not one projection, which is the "must cost nothing
    // when they are off" requirement written as code rather than hoped for.
    if !vol3d.show_grid && !vol3d.show_box && !vol3d.show_labels {
        return;
    }

    if vol3d.show_grid {
        draw_floor_grid(vol3d, painter, rect);
    }
    if vol3d.show_box {
        draw_wireframe(vol3d, painter, rect);
    }
    if vol3d.show_labels {
        draw_height_ladder(vol3d, painter, rect);
        draw_cardinals(vol3d, painter, rect);
    }
}

/// Project both ends, or draw nothing.
///
/// [`Vol3d::project_point`] answers `None` for a point behind the camera.
/// Substituting anything for that - the pane edge, the last good position, a
/// clamp - draws a line to a place no part of the box occupies, and a wireframe
/// with one wrong strut in it is worse than one with a missing strut, because
/// it still looks like a box.
fn segment(
    vol3d: &Vol3d,
    painter: &egui::Painter,
    rect: egui::Rect,
    from: [f32; 3],
    to: [f32; 3],
    stroke: egui::Stroke,
) {
    if let (Some(from), Some(to)) = (
        vol3d.project_point(rect, from),
        vol3d.project_point(rect, to),
    ) {
        painter.line_segment([from, to], stroke);
    }
}

fn draw_floor_grid(vol3d: &Vol3d, painter: &egui::Painter, rect: egui::Rect) {
    let stroke = egui::Stroke::new(HAIRLINE, GRID_COLOR);
    let last = (GRID_LINES - 1) as f32;
    for line in 0..GRID_LINES {
        let offset = -FOOTPRINT_HALF + 2.0 * FOOTPRINT_HALF * line as f32 / last;
        segment(
            vol3d,
            painter,
            rect,
            [offset, -FOOTPRINT_HALF, 0.0],
            [offset, FOOTPRINT_HALF, 0.0],
            stroke,
        );
        segment(
            vol3d,
            painter,
            rect,
            [-FOOTPRINT_HALF, offset, 0.0],
            [FOOTPRINT_HALF, offset, 0.0],
            stroke,
        );
    }
}

fn draw_wireframe(vol3d: &Vol3d, painter: &egui::Painter, rect: egui::Rect) {
    let stroke = egui::Stroke::new(HAIRLINE, BOX_COLOR);
    let top = vol3d.zspan();
    let corners = LadderEdge::ALL;
    for (index, corner) in corners.iter().enumerate() {
        let (east, north) = corner.footprint();
        let (next_east, next_north) = corners[(index + 1) % corners.len()].footprint();
        // Floor ring, roof ring, and the vertical strut between them. Walking
        // the corner enum rather than a literal table of twelve edges is what
        // keeps the wireframe and the ladder agreeing about where a corner is.
        segment(
            vol3d,
            painter,
            rect,
            [east, north, 0.0],
            [next_east, next_north, 0.0],
            stroke,
        );
        segment(
            vol3d,
            painter,
            rect,
            [east, north, top],
            [next_east, next_north, top],
            stroke,
        );
        segment(
            vol3d,
            painter,
            rect,
            [east, north, 0.0],
            [east, north, top],
            stroke,
        );
    }
}

/// The eye position, whichever camera is flying.
fn camera_eye(vol3d: &Vol3d) -> [f32; 3] {
    match vol3d.camera_mode {
        Vol3dCameraMode::Orbit => vol3d.orbit_eye(),
        Vol3dCameraMode::Fly => [vol3d.fly_x, vol3d.fly_y, vol3d.fly_z],
    }
}

/// The eight box corners, projected: `[foot, head]` per vertical edge, in
/// [`LadderEdge::ALL`] order, and `None` for an edge with either end behind the
/// camera.
///
/// One pass, so that the ladder picks its edge, points its labels and proves
/// its clearance from the SAME numbers. Working any of those out separately is
/// how a scale ends up disagreeing with the box it is drawn on.
fn project_edges(vol3d: &Vol3d, rect: egui::Rect) -> [Option<[egui::Pos2; 2]>; 4] {
    let top = vol3d.zspan();
    let mut projected = [None; 4];
    for (slot, edge) in projected.iter_mut().zip(LadderEdge::ALL) {
        let (east, north) = edge.footprint();
        // Both ends or neither: a spine anchored at one end and extrapolated
        // from the other is a scale that lies, and half an edge is no bound on
        // the silhouette either.
        if let (Some(foot), Some(head)) = (
            vol3d.project_point(rect, [east, north, 0.0]),
            vol3d.project_point(rect, [east, north, top]),
        ) {
            *slot = Some([foot, head]);
        }
    }
    projected
}

/// The edge the ladder rides, as it lands on the screen.
#[derive(Clone, Copy, Debug)]
pub struct LadderSpine {
    pub edge: LadderEdge,
    /// The projected `z = 0` corner.
    pub foot: egui::Pos2,
    /// The projected `z = zspan` corner.
    pub head: egui::Pos2,
    /// Unit screen direction the tick arms and the labels run in: perpendicular
    /// to the spine, pointing away from the box.
    ///
    /// Perpendicular to the EDGE rather than simply leftward across the screen,
    /// because a vertical world line only projects to a vertical screen line
    /// when the camera is level; at 60 degrees of pitch the box edges lean
    /// visibly and horizontal arms would meet them at a slant.
    pub outward: egui::Vec2,
}

impl LadderSpine {
    pub fn length(self) -> f32 {
        (self.head - self.foot).length()
    }
}

/// The left-hand silhouette edge of the box as this camera sees it, and the
/// direction its labels run in - or `None` when no edge can carry a ladder.
///
/// Three conditions, in this order.
///
/// 1. Every one of the eight corners must project. A corner behind the camera
///    has no screen position to compare against - not a distant one, none at
///    all - so the silhouette test below would be measuring three edges and
///    guessing the fourth. That is the eye at, or inside, the box: there is no
///    vertical edge with empty sky beside it to label into, and the honest
///    answer is no ladder rather than numbers written across the storm.
/// 2. An edge is a candidate only if the whole projected box lies behind it,
///    measured along whichever of the two perpendiculars leaves the box behind.
///    That is the definition of a silhouette edge, in the only space that
///    matters, and it is what makes the arms and the labels provably clear of
///    the volume. Both silhouette edges of a box pass it - the left one and the
///    right one.
/// 3. Of the candidates, the leftmost wins, so the labels grow away from the
///    box into the pane rather than back across it. Strictly leftmost: a tie is
///    settled by [`LadderEdge::ALL`] order and never by float noise, so one
///    camera always chooses one edge.
pub fn ladder_spine(vol3d: &Vol3d, rect: egui::Rect) -> Option<LadderSpine> {
    let projected = project_edges(vol3d, rect);
    if projected.iter().any(Option::is_none) {
        return None;
    }
    let mut best: Option<(f32, LadderSpine)> = None;
    for (index, edge) in LadderEdge::ALL.into_iter().enumerate() {
        let Some([foot, head]) = projected[index] else {
            continue;
        };
        let spine = head - foot;
        let length = spine.length();
        // Deliberately not MIN_LADDER_PX. Whether a ladder is long enough to
        // read is a question about the edge this function CHOOSES, asked once
        // by the caller; applying it here would let a marginally short left
        // edge hand the ladder to the right-hand side of the box instead, which
        // is a jump across the whole pane at a threshold nobody can see coming.
        if !length.is_finite() || length <= 1.0e-3 {
            continue;
        }
        let perpendicular = egui::vec2(spine.y, -spine.x) / length;
        // How far the rest of the box reaches past this edge, each way. This
        // edge's own two corners are left out because they sit exactly on the
        // line and would pin both answers to zero.
        let mut reach = [f32::NEG_INFINITY; 2];
        for (other, ends) in projected.iter().enumerate() {
            if other == index {
                continue;
            }
            let Some(ends) = ends else { continue };
            for point in ends {
                let offset = *point - foot;
                reach[0] = reach[0].max(offset.dot(perpendicular));
                reach[1] = reach[1].max(offset.dot(-perpendicular));
            }
        }
        let (clearance, outward) = if reach[0] <= reach[1] {
            (reach[0], perpendicular)
        } else {
            (reach[1], -perpendicular)
        };
        if !clearance.is_finite() || clearance > SILHOUETTE_TOLERANCE_PX {
            continue;
        }
        let leftmost = foot.x.min(head.x);
        if best.is_none_or(|(best_x, _)| leftmost < best_x) {
            best = Some((
                leftmost,
                LadderSpine {
                    edge,
                    foot,
                    head,
                    outward,
                },
            ));
        }
    }
    best.map(|(_, spine)| spine)
}

/// The vertical edge the height ladder hangs on, for this camera and pane.
pub fn ladder_edge(vol3d: &Vol3d, rect: egui::Rect) -> Option<LadderEdge> {
    ladder_spine(vol3d, rect).map(|spine| spine.edge)
}

/// The height ladder for a box `top_m` metres tall, in kilofeet, ascending.
///
/// Empty when `top_m` is not a positive finite height: [`nice_ticks`] answers
/// an empty ladder rather than a column of identical labels, and an empty
/// ladder draws nothing.
pub fn ladder_ticks_kft(top_m: f32) -> Vec<f64> {
    let top_kft = f64::from(top_m) * METERS_TO_KILOFEET;
    nice_ticks(0.0, top_kft, LADDER_TARGET_INTERVALS)
        .into_iter()
        // The floor IS the zero. A "0" rung would land in the floor grid and
        // among the cardinals, and ground level is the one height nobody has
        // ever needed a scale to find.
        .filter(|value| *value > 0.0)
        .collect()
}

/// Decimal places every rung needs to be written exactly.
///
/// The same rule the 2D legend uses, and for the same reason: formatting 0.3
/// with a fixed two places gives "0.30", and formatting it with whatever `{}`
/// chooses gives "0.30000000000000004" the moment a rung lands off a binary
/// fraction.
fn ladder_label_decimals(ticks: &[f64]) -> usize {
    for decimals in 0..=MAX_LABEL_DECIMALS {
        let scale = 10f64.powi(decimals as i32);
        let exact = ticks.iter().all(|tick| {
            let scaled = tick * scale;
            // Relative, with a floor of one: the absolute error in a rung grows
            // with its magnitude, and a rung of 0 has no magnitude to scale by.
            (scaled - scaled.round()).abs() <= scaled.abs().max(1.0) * 1e-9
        });
        if exact {
            return decimals;
        }
    }
    MAX_LABEL_DECIMALS
}

/// World z of a rung, on a ladder whose `top_kft` stands at `zspan`.
///
/// One multiplication, and it is a named function because it is the whole
/// measurement. `value / top_kft` is the fraction of the box the rung sits at,
/// and the box is `zspan` tall in the units the shader marches, so reversing it
/// (`z / zspan * top_m`) has to give back the metres the label claims. A test
/// does exactly that, to within a metre, for four different box tops; get this
/// wrong and every echo top read off the ladder is wrong by the same ratio,
/// silently, in the direction of a warning.
fn rung_z(value_kft: f64, top_kft: f64, zspan: f32) -> f32 {
    (value_kft / top_kft) as f32 * zspan
}

/// Whether two labels anchored at `a` and `b` would overlap, given the box one
/// label occupies. See [`LABEL_ROW_PX`] for why this is two axes and not a
/// distance.
fn labels_collide(a: egui::Pos2, b: egui::Pos2, gap: egui::Vec2) -> bool {
    (a.x - b.x).abs() < gap.x && (a.y - b.y).abs() < gap.y
}

/// Halve the ladder until no two rungs land on top of one another.
///
/// Halving is the only thinning that keeps the ladder evenly spaced. Dropping
/// rungs off the top instead would label the bottom of a storm and leave its
/// top blank, which reads as missing data rather than as a thinned scale - the
/// same argument the 2D legend's `thin_to_fit` makes.
///
/// `place` is the real projection, not a model of it, so the spacing this
/// enforces is the spacing the operator sees.
fn thin_to_fit<P>(ticks: &mut Vec<f64>, place: P, gap: egui::Vec2)
where
    P: Fn(f64) -> Option<egui::Pos2>,
{
    // Terminates: the length strictly decreases while it is above one.
    while ticks.len() > 1 && rungs_are_crowded(ticks, &place, gap) {
        // `retain` keeps the buffer it was given, so thinning a ladder costs no
        // allocation on the frame that thins it.
        let mut index = 0usize;
        ticks.retain(|_| {
            let keep = index.is_multiple_of(2);
            index += 1;
            keep
        });
    }
}

/// Whether any two neighbouring rung labels would overlap.
fn rungs_are_crowded<P>(ticks: &[f64], place: &P, gap: egui::Vec2) -> bool
where
    P: Fn(f64) -> Option<egui::Pos2>,
{
    let mut previous: Option<egui::Pos2> = None;
    for value in ticks {
        let Some(position) = place(*value) else {
            // A rung that does not project is a rung that is not painted, so it
            // crowds nothing - and it breaks the chain, because the distance
            // across it is not the distance between two labels.
            previous = None;
            continue;
        };
        if let Some(last) = previous.replace(position)
            && labels_collide(position, last, gap)
        {
            return true;
        }
    }
    false
}

fn draw_height_ladder(vol3d: &Vol3d, painter: &egui::Painter, rect: egui::Rect) {
    // Geometry before arithmetic. A camera that cannot carry a ladder - looking
    // straight down the box, or standing inside it - pays for eight projections
    // and not one allocation.
    let Some(spine) = ladder_spine(vol3d, rect) else {
        return;
    };
    let spine_px = spine.length();
    // The finite test is not redundant with the comparison: a NaN length -
    // impossible today, but one non-finite camera field away - compares false
    // against everything, and has to fall out here rather than reach the
    // painter as a ladder drawn at NaN.
    if !spine_px.is_finite() || spine_px < MIN_LADDER_PX {
        return;
    }

    let top_m = vol3d.top_km() * 1000.0;
    let mut ticks = ladder_ticks_kft(top_m);
    if ticks.is_empty() {
        return;
    }
    // Non-zero and finite, because `ladder_ticks_kft` came back non-empty.
    let top_kft = f64::from(top_m) * METERS_TO_KILOFEET;
    let zspan = vol3d.zspan();
    let (east, north) = spine.edge.footprint();
    // Each rung is projected in its own right rather than interpolated along
    // the projected spine. Perspective is not affine along a line: equally
    // spaced heights do NOT land equally spaced on screen, and interpolating
    // would print 30 where the box is showing 32.
    //
    // Both ends of this edge are known to project, and projected depth is
    // affine in the point, so every height between them has positive depth as
    // well: no rung of a drawn ladder can fail to project. The `else` arms
    // below are there because that argument is about today's camera rather than
    // a promise made by `project_point`.
    let place =
        |value: f64| vol3d.project_point(rect, [east, north, rung_z(value, top_kft, zspan)]);
    thin_to_fit(&mut ticks, place, label_gap());

    let stroke = egui::Stroke::new(HAIRLINE, LADDER_COLOR);
    let label_offset = spine.outward * (TICK_ARM_PX + TICK_LABEL_GAP_PX);
    // The labels sit on whichever side the arms point, so the text grows away
    // from the box in both cases.
    let anchor = if spine.outward.x <= 0.0 {
        egui::Align2::RIGHT_CENTER
    } else {
        egui::Align2::LEFT_CENTER
    };
    let font = egui::FontId::monospace(TICK_FONT_SIZE);

    painter.line_segment([spine.foot, spine.head], stroke);

    let decimals = ladder_label_decimals(&ticks);
    let mut top_rung = None;
    for value in &ticks {
        let Some(position) = place(*value) else {
            continue;
        };
        painter.line_segment([position, position + spine.outward * TICK_ARM_PX], stroke);
        // `format_args!` and not `format!`: `Painter::text` takes an
        // `impl ToString` and calls it, and `String`'s own `ToString` clones,
        // so building the `String` here would allocate one to throw away and a
        // second to keep. Formatting arguments allocate once, inside the
        // painter. It is one small allocation per rung per frame either way -
        // the ladder holds no state between frames to cache it in - and one is
        // half of two on the thread that paints.
        painter.text(
            position + label_offset,
            anchor,
            format_args!("{value:.decimals$}"),
            font.clone(),
            TICK_LABEL_COLOR,
        );
        // The ticks ascend, so the last one to project is the highest.
        top_rung = Some(position);
    }

    // The unit, once, at the head of the ladder - lifted clear of the top rung
    // rather than allowed to sit on it. The top rung is a FIXED fraction of the
    // box height below the roof (50 kft of an 18 km box is 0.85 of it), so on a
    // short spine those two labels collide however many rungs are thinned away;
    // thinning until they did not would strip the whole ladder to one rung. A
    // unit written just above the roof line still reads as the ladder's.
    //
    // A straight line projects to a straight line, so the rungs, the foot and
    // the head are colinear on the screen and the separation the lift has to
    // reach is exact rather than approximate: `along * (distance + lift)` has
    // to clear the label box on ONE of its two axes, so the lift needed is the
    // smaller of the two demands. `along` is a unit vector, so one of its
    // components is at least 0.707 and this cannot ask for infinity.
    // `spine_px` is at least MIN_LADDER_PX, so the division is safe.
    let along = (spine.head - spine.foot) / spine_px;
    let clear_on = |component: f32, gap: f32| {
        if component.abs() > 1.0e-6 {
            gap / component.abs()
        } else {
            f32::INFINITY
        }
    };
    let needed = clear_on(along.x, LABEL_COLUMN_PX).min(clear_on(along.y, LABEL_ROW_PX));
    let lift = top_rung.map_or(0.0, |rung| (needed - (spine.head - rung).length()).max(0.0));
    // Both ends are known to project, so this cannot become the one label
    // painted at a garbage position.
    let unit_position = spine.head + along * lift + label_offset;
    painter.text(
        unit_position,
        anchor,
        DisplayUnit::Kilofeet.label(),
        font,
        UNIT_LABEL_COLOR,
    );
}

/// Whether the floor edge carrying `cardinal` stands between the camera and the
/// volume, or behind it.
///
/// The box is `|east| <= 1`, `|north| <= 1`, so an eye whose component along a
/// face's outward normal exceeds 1 is outside that face's plane, and every ray
/// from it to a point ON that plane stays outside the box. That is exactly the
/// condition for the label to be clear of the storm. Zoom in far enough that
/// the eye crosses the plane and the far label is dropped rather than painted
/// over the echo it is now behind.
fn cardinal_is_in_front(vol3d: &Vol3d, cardinal: Cardinal) -> bool {
    let eye = camera_eye(vol3d);
    let (east, north) = cardinal.outward();
    let along = eye[0] * east + eye[1] * north;
    along.is_finite() && along > FOOTPRINT_HALF
}

fn draw_cardinals(vol3d: &Vol3d, painter: &egui::Painter, rect: egui::Rect) {
    // Room for the label's own half-height and a little air, without ever
    // inverting on a pane too small to hold it.
    let margin = CARDINAL_FONT_SIZE
        .min(rect.width() * 0.25)
        .min(rect.height() * 0.25)
        .max(0.0);
    let inside = rect.shrink(margin);
    let font = egui::FontId::proportional(CARDINAL_FONT_SIZE);

    for cardinal in Cardinal::ALL {
        if !cardinal_is_in_front(vol3d, cardinal) {
            continue;
        }
        let (east, north) = cardinal.outward();
        // z = 0: the label lies ON the ground plane and swings with it as the
        // camera orbits, which is what makes it read as painted on the floor
        // rather than pinned to the pane.
        let anchor = [east * CARDINAL_OFFSET, north * CARDINAL_OFFSET, 0.0];
        let Some(position) = vol3d.project_point(rect, anchor) else {
            continue;
        };
        if !inside.contains(position) {
            continue;
        }
        painter.text(
            position,
            egui::Align2::CENTER_CENTER,
            cardinal.label(),
            font.clone(),
            CARDINAL_COLOR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A pane big enough that nothing under test is clipped by the pane edge
    /// rather than by the rule being tested.
    fn pane() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    /// One painted label: the rectangle `egui` will actually fill with glyphs,
    /// after the anchor has been applied, and what it says.
    #[derive(Clone, Debug)]
    struct Label {
        area: egui::Rect,
        text: String,
    }

    /// What one call to [`draw`] actually put on the screen.
    #[derive(Default)]
    struct Painted {
        segments: Vec<[egui::Pos2; 2]>,
        labels: Vec<Label>,
        /// The clip rectangle every shape carries. Nothing this module paints
        /// may escape the pane, and this is what proves it.
        clips: Vec<egui::Rect>,
    }

    impl Painted {
        fn says(&self, wanted: &str) -> bool {
            self.labels.iter().any(|label| label.text == wanted)
        }

        fn area_of(&self, wanted: &str) -> Option<egui::Rect> {
            self.labels
                .iter()
                .find(|label| label.text == wanted)
                .map(|label| label.area)
        }

        fn centre_of(&self, wanted: &str) -> Option<egui::Pos2> {
            self.area_of(wanted).map(|area| area.center())
        }

        fn touches(&self, position: egui::Pos2) -> bool {
            self.segments
                .iter()
                .flatten()
                .any(|end| (*end - position).length() < 0.01)
        }

        fn is_cardinal(text: &str) -> bool {
            Cardinal::ALL
                .iter()
                .any(|cardinal| cardinal.label() == text)
        }

        /// Painted labels that belong to the ladder: its rungs and its unit.
        fn ladder_labels(&self) -> Vec<Label> {
            self.labels
                .iter()
                .filter(|label| !Self::is_cardinal(&label.text))
                .cloned()
                .collect()
        }

        /// Painted labels that are ladder rungs: everything that is neither the
        /// unit nor a cardinal.
        fn rungs(&self) -> usize {
            self.labels
                .iter()
                .filter(|label| label.text != DisplayUnit::Kilofeet.label())
                .filter(|label| !Self::is_cardinal(&label.text))
                .count()
        }

        /// Every coordinate that reached the painter is a real number.
        ///
        /// A NaN position does not crash `egui`; it silently drops or smears
        /// the shape, which is exactly the kind of failure that survives to a
        /// release. Checked on every hostile camera below.
        fn is_finite(&self) -> bool {
            self.segments
                .iter()
                .flatten()
                .all(|point| point.x.is_finite() && point.y.is_finite())
                && self
                    .labels
                    .iter()
                    .all(|label| label.area.is_finite() && !label.area.any_nan())
        }
    }

    /// Run [`draw`] through a real headless `egui` context and collect what it
    /// painted.
    ///
    /// Inside `run_ui` because laying out a label needs the font atlas, and a
    /// `Context` refuses to hand one out between frames. That also means these
    /// tests measure the real glyph pipeline rather than a stub of it: the
    /// label rectangles below are the rectangles the operator sees filled.
    fn painted(vol3d: &Vol3d) -> Painted {
        let rect = pane();
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let output = ctx.run_ui(raw, |ui| {
            draw(vol3d, &ui.painter_at(rect), rect);
        });
        let mut collected = Painted::default();
        for clipped in output.shapes {
            collected.clips.push(clipped.clip_rect);
            match clipped.shape {
                egui::Shape::LineSegment { points, .. } => collected.segments.push(points),
                egui::Shape::Text(text) => collected.labels.push(Label {
                    area: egui::Rect::from_min_size(text.pos, text.galley.size()),
                    text: text.galley.text().to_owned(),
                }),
                // The panel background is a `Rect`. Nothing this module can
                // emit is anything but a segment or a galley.
                _ => {}
            }
        }
        collected
    }

    /// Height and greatest width of a laid-out ladder label, from the same font
    /// stack that paints one. The separation constants have to clear these or
    /// the ladder overlaps its own labels however carefully it is spaced.
    fn rung_label_size() -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut measured = (0.0, 0.0);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let lay = |text: &str| {
                ui.painter()
                    .layout_no_wrap(
                        text.to_owned(),
                        egui::FontId::monospace(TICK_FONT_SIZE),
                        egui::Color32::WHITE,
                    )
                    .size()
            };
            // "0.25" is the widest label the ladder can write: MAX_LABEL_DECIMALS
            // caps the rungs at two places, and the unit is three characters.
            let widest = lay("0.25").x.max(lay(DisplayUnit::Kilofeet.label()).x);
            measured = (lay("50").y, widest);
        });
        measured
    }

    /// An orbit camera, which is the one the pane opens with.
    fn orbit(yaw: f32, pitch: f32, dist: f32) -> Vol3d {
        Vol3d {
            yaw,
            pitch,
            dist,
            ..Default::default()
        }
    }

    /// A fly camera, whose eye is wherever it was flown to rather than on a
    /// sphere around the box.
    fn fly(east: f32, north: f32, up: f32) -> Vol3d {
        Vol3d {
            camera_mode: Vol3dCameraMode::Fly,
            fly_x: east,
            fly_y: north,
            fly_z: up,
            ..Default::default()
        }
    }

    fn all_off() -> Vol3d {
        Vol3d {
            show_grid: false,
            show_box: false,
            show_labels: false,
            ..Default::default()
        }
    }

    /// A camera 200 half-widths above the box, pointing further up. Every
    /// annotation point is behind it, so every `project_point` answers `None`.
    fn looking_away() -> Vol3d {
        // East and north of the box, so the two near cardinals pass
        // `cardinal_is_in_front` and are dropped by the projection rather than
        // by the visibility rule - which is the point of the test.
        Vol3d {
            yaw: 0.0,
            pitch: -1.45,
            ..fly(150.0, 150.0, 200.0)
        }
    }

    // -- the ladder is a measurement --------------------------------------

    /// Metres a rung is DRAWN at, recovered from the world z the ladder uses.
    ///
    /// The reverse of [`rung_z`] through the box the shader marches: `z` runs
    /// 0..`zspan` over 0..`top_m`. Deliberately not the reverse of the source
    /// expression - it is the reverse of what the geometry means, so the two
    /// agreeing says the geometry is right rather than that the algebra was
    /// copied.
    fn rung_metres(value_kft: f64, top_m: f32, zspan: f32) -> f64 {
        let top_kft = f64::from(top_m) * METERS_TO_KILOFEET;
        f64::from(rung_z(value_kft, top_kft, zspan)) / f64::from(zspan) * f64::from(top_m)
    }

    #[test]
    fn every_rung_lands_within_a_metre_of_the_height_it_claims() {
        // A foot is exactly 0.3048 m, so every one of these has a right answer
        // and not a rounded one. 18 000 m is 18 000 / 0.3048 = 59 055.118 ft =
        // 59.055 kft, which is why an 18 km box tops out at a 50 rung and not
        // at a 60. An analyst reads a storm top off this ladder and puts it in
        // a warning; a ladder that is off is worse than no ladder at all.
        for (top_m, expected) in [
            // 59.055 kft: 5.91 intervals of 10 beats 2.95 of 20.
            (18_000.0_f32, vec![10.0, 20.0, 30.0, 40.0, 50.0]),
            // 39.370 kft: 3.94 intervals of 10 beats 1.97 of 20.
            (12_000.0, vec![10.0, 20.0, 30.0]),
            // 68.898 kft: 3.44 intervals of 20 (1.56 off the wish) beats 6.89
            // of 10 (1.89 off), so this box coarsens rather than crowding.
            (21_000.0, vec![20.0, 40.0, 60.0]),
            // Deliberately not a round box: 15 500 m is 50.853 kft, and 5.09
            // intervals of 10 is very nearly the five asked for.
            (15_500.0, vec![10.0, 20.0, 30.0, 40.0, 50.0]),
        ] {
            let ticks = ladder_ticks_kft(top_m);
            assert_eq!(ticks, expected, "the ladder for a {top_m} m box");
            // No rung above the roof: a scale that runs off the top of the box
            // it is drawn on is measuring something the operator cannot see.
            let top_kft = f64::from(top_m) * METERS_TO_KILOFEET;
            assert!(
                ticks.iter().all(|rung| *rung <= top_kft),
                "{top_m} m is {top_kft} kft and the ladder reaches {ticks:?}"
            );
            // Every exaggeration the explorer can reach, including both ends of
            // `zspan`'s clamp: the mapping must not depend on how stretched the
            // box is, only on where the top of it is.
            for zspan in [0.06, 0.18, 0.9, 3.0, 24.0] {
                for value in &ticks {
                    let drawn = rung_metres(*value, top_m, zspan);
                    let claimed = value * 304.8;
                    assert!(
                        (drawn - claimed).abs() < 1.0,
                        "{top_m} m box at zspan {zspan}: the {value} kft rung is \
                         drawn at {drawn} m, and {value} kft is {claimed} m"
                    );
                }
            }
        }
    }

    #[test]
    fn the_label_that_says_thirty_is_painted_at_thirty_thousand_feet() {
        // The whole feature in one test, and the only one that would catch a
        // ladder whose arithmetic is right and whose drawing is off by a rung.
        // Everything below is computed from metres - 30 kft is 30 000 x 0.3048
        // = 9 144 m, and 9 144 of 18 000 is 0.508 of the box - so it agrees
        // with `rung_z` only if `rung_z` is right, rather than by construction.
        let vol3d = Vol3d::default();
        assert!(
            (vol3d.top_km() - 18.0).abs() < 1.0e-6,
            "this test hand-computes against an 18 km box and the frame now \
             offers {} km",
            vol3d.top_km()
        );
        let spine = ladder_spine(&vol3d, pane()).expect("the default camera carries a ladder");
        let (east, north) = spine.edge.footprint();
        let zspan = vol3d.zspan();
        let painted = painted(&vol3d);
        for kft in [10.0_f64, 20.0, 30.0, 40.0, 50.0] {
            let metres = kft * 304.8;
            let height = (metres / 18_000.0) as f32 * zspan;
            let expected = vol3d
                .project_point(pane(), [east, north, height])
                .expect("the rung is in front of the default camera");
            assert!(
                painted.touches(expected),
                "no tick arm at {kft} kft, which is {metres} m and {height} of \
                 the way up the box"
            );
            let area = painted
                .area_of(&format!("{kft:.0}"))
                .unwrap_or_else(|| panic!("no label reading {kft}"));
            // The label hangs off the tick, outward, by the arm and the gap -
            // and it is ANCHORED there, so the anchor lies on the edge of the
            // rectangle the glyphs fill whichever side the labels are on.
            let anchor = expected + spine.outward * (TICK_ARM_PX + TICK_LABEL_GAP_PX);
            assert!(
                area.distance_to_pos(anchor) < 1.0,
                "the {kft} label fills {area:?} and its anchor is {anchor:?}"
            );
        }
    }

    #[test]
    fn the_unit_is_lifted_off_a_top_rung_it_would_otherwise_sit_on() {
        // The top rung is a fixed fraction of the box below the roof, so the
        // shorter the spine the closer the unit at the head gets to it. At an
        // exaggeration of 0.3 the two are 8 points apart, which is inside one
        // label, and the fix is to lift the unit past the roof rather than to
        // thin a ladder that is not crowded.
        let squat = Vol3d {
            vertical_exaggeration: 0.3,
            ..Default::default()
        };
        let spine = ladder_spine(&squat, pane()).expect("a squat box still carries a ladder");
        let painted = painted(&squat);
        let unit = painted
            .area_of(DisplayUnit::Kilofeet.label())
            .expect("the unit is painted");
        let along = (spine.head - spine.foot) / spine.length();
        // Past the head, along the spine: the unit sits above the roof line.
        let beyond = (unit.center() - spine.head).dot(along);
        assert!(
            beyond > 0.0,
            "the unit sits {beyond} points along the spine from the head, so it \
             was not lifted at all"
        );
        // And the reason it had to be: the rung it would have collided with.
        // Measured at the TICK and not at the label, because the label carries
        // the outward offset and the anchor shift as well.
        let (east, north) = spine.edge.footprint();
        let top_kft = f64::from(squat.top_km() * 1000.0) * METERS_TO_KILOFEET;
        let top_rung = squat
            .project_point(pane(), [east, north, rung_z(50.0, top_kft, squat.zspan())])
            .expect("the top rung projects");
        assert!(
            (spine.head - top_rung).length() < LABEL_ROW_PX,
            "this fixture wants a head within one label of the top rung, and \
             they are {} points apart",
            (spine.head - top_rung).length()
        );
        let rung_label = painted.area_of("50").expect("the top rung is painted");
        assert!(
            !unit.intersect(rung_label).is_positive(),
            "the unit at {unit:?} still overlaps the top rung at {rung_label:?}"
        );
    }

    #[test]
    fn the_ladder_never_puts_a_rung_on_the_ground() {
        // The floor is the zero, and a "0" rung would land in the floor grid
        // and among the cardinals.
        for top_m in [1_000.0, 6_000.0, 12_000.0, 18_000.0, 30_000.0] {
            let ticks = ladder_ticks_kft(top_m);
            assert!(!ticks.is_empty(), "{top_m} m produced no ladder at all");
            assert!(
                ticks.iter().all(|rung| *rung > 0.0),
                "{top_m} m produced a rung at or below the floor: {ticks:?}"
            );
        }
    }

    #[test]
    fn a_box_with_no_height_produces_no_ladder_rather_than_a_panic() {
        assert!(ladder_ticks_kft(0.0).is_empty());
        assert!(ladder_ticks_kft(-1.0).is_empty());
        assert!(ladder_ticks_kft(f32::NAN).is_empty());
        assert!(ladder_ticks_kft(f32::INFINITY).is_empty());
    }

    #[test]
    fn rung_labels_carry_only_the_decimals_they_need() {
        assert_eq!(ladder_label_decimals(&[10.0, 20.0, 30.0]), 0);
        assert_eq!(ladder_label_decimals(&[0.5, 1.0, 1.5]), 1);
        assert_eq!(ladder_label_decimals(&[0.25, 0.5]), 2);
    }

    #[test]
    fn a_crowded_ladder_halves_instead_of_cropping_its_top() {
        // A synthetic spine, two points of screen per kilofoot, so the five
        // rungs of an 18 km box land exactly 20 points apart and the expected
        // answers below are arithmetic rather than observation.
        let place = |value: f64| Some(egui::pos2(0.0, -2.0 * value as f32));

        let mut ticks = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        thin_to_fit(&mut ticks, place, egui::vec2(28.0, 13.0));
        assert_eq!(ticks, vec![10.0, 20.0, 30.0, 40.0, 50.0]);

        // 25 points wanted and 20 available: five halve to three, now 40 apart,
        // and stop. The TOP of the storm keeps its label, which is what an
        // operator reads the scale for.
        let mut ticks = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        let capacity = ticks.capacity();
        thin_to_fit(&mut ticks, place, egui::vec2(28.0, 25.0));
        assert_eq!(ticks, vec![10.0, 30.0, 50.0]);
        // Thinning reuses the buffer it was given: no allocation on the frame
        // that thins, which is the frame with the least room to spare.
        assert_eq!(ticks.capacity(), capacity);

        // Never below one rung, however little room there is.
        let mut ticks = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        thin_to_fit(&mut ticks, place, egui::vec2(28.0, 1_000.0));
        assert_eq!(ticks, vec![10.0]);

        // Rungs stacked in a COLUMN clear one another by moving apart
        // horizontally: a rule that only measured the vertical gap would thin a
        // ladder that reads perfectly well. The spine here lies flat across the
        // screen, which is what an operator sees looking along the box.
        let sideways = |value: f64| Some(egui::pos2(2.0 * value as f32, 0.0));
        let mut ticks = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        thin_to_fit(&mut ticks, sideways, egui::vec2(15.0, 15.0));
        assert_eq!(ticks, vec![10.0, 20.0, 30.0, 40.0, 50.0]);

        // A rung that does not project crowds nothing and breaks the chain,
        // rather than being measured from wherever the last one was.
        let gappy = |value: f64| (value > 20.0).then(|| egui::pos2(0.0, -2.0 * value as f32));
        let mut ticks = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        thin_to_fit(&mut ticks, gappy, egui::vec2(28.0, 25.0));
        assert_eq!(ticks, vec![10.0, 30.0, 50.0]);
    }

    #[test]
    fn the_label_box_is_at_least_the_size_of_a_label() {
        // The two constants are only a separation rule if they are at least as
        // big as the thing they separate. Measured from the same font stack
        // that paints the labels, because 11 points of font is not 11 points of
        // laid-out line, and because the widest thing the ladder writes is not
        // the thing anyone thinks of first.
        let (height, width) = rung_label_size();
        assert!(
            LABEL_ROW_PX >= height,
            "rung labels lay out {height} points tall and the ladder keeps them \
             {LABEL_ROW_PX} apart, so they overlap"
        );
        assert!(
            LABEL_COLUMN_PX >= width,
            "the widest ladder label lays out {width} points wide and the ladder \
             keeps them {LABEL_COLUMN_PX} apart, so they overlap"
        );
    }

    // -- which edge the ladder rides --------------------------------------

    /// How far the projected box reaches PAST the ladder edge, on the side the
    /// tick arms and the labels are written.
    ///
    /// Negative is the whole point of the choice: it is the clearance between
    /// the numbers and the storm. `None` when this camera draws no ladder.
    fn outward_reach(vol3d: &Vol3d) -> Option<f32> {
        let spine = ladder_spine(vol3d, pane())?;
        let mut reach = f32::NEG_INFINITY;
        for edge in LadderEdge::ALL {
            if edge == spine.edge {
                continue;
            }
            let (east, north) = edge.footprint();
            for z in [0.0, vol3d.zspan()] {
                let point = vol3d.project_point(pane(), [east, north, z])?;
                reach = reach.max((point - spine.foot).dot(spine.outward));
            }
        }
        Some(reach)
    }

    #[test]
    fn the_ladder_edge_always_has_the_whole_box_behind_it() {
        // The defect this replaces: the bearing rule chose its edge by cutting
        // the compass into quadrants at the footprint diagonals, and the real
        // switchovers are where the eye crosses a FACE plane - 27.6 degrees
        // away, for the default camera. Over the 31 per cent of a turn between
        // the two it hung the ladder on an edge behind the volume. Worst case
        // measured on that code: camera bearing 179.9 degrees, due west of the
        // box, ladder on the NORTH-EAST edge, labels 216.4 points inside the
        // silhouette and written straight across the storm.
        for (label, dist, pitch) in [
            ("the default view", 2.4_f32, 0.45_f32),
            ("zoomed to the orbit floor", 1.3, 0.05),
            ("backed off and looking down", 5.0, 0.9),
            ("close and level", 1.7, 0.03),
        ] {
            let mut worst = f32::NEG_INFINITY;
            let mut inside = 0;
            for tenths in 0..3_600 {
                let bearing = tenths as f32 * 0.1;
                let vol3d = orbit(bearing.to_radians(), pitch, dist);
                let spine = ladder_spine(&vol3d, pane())
                    .unwrap_or_else(|| panic!("{label} drew no ladder at yaw {bearing} deg"));
                // Every camera in this list is one an operator can reach and
                // one that draws a readable ladder, so a sweep that stopped
                // drawing would be hiding rather than passing.
                assert!(
                    spine.length() >= MIN_LADDER_PX,
                    "{label} at yaw {bearing} deg has only {} points of spine",
                    spine.length()
                );
                let reach = outward_reach(&vol3d).expect("the spine projects");
                assert!(
                    reach <= SILHOUETTE_TOLERANCE_PX,
                    "{label} at yaw {bearing} deg put the ladder {reach} points \
                     inside the box, so its labels are over the storm"
                );
                worst = worst.max(reach);
                if reach > 0.0 {
                    inside += 1;
                }
            }
            // The clearance passes through zero at each of the four
            // switchovers - that is what a switchover IS, the instant the two
            // candidate edges lie on one screen line - so a sample landing on
            // the wrong side of zero by a fraction of a point is the crossing
            // itself and not a wrong edge. Everywhere else it is strictly
            // clear, which is why this is one per cent of the turn and not the
            // 31 per cent the bearing rule got outright wrong.
            assert!(
                inside * 100 < 3_600,
                "{label}: {inside} of 3600 bearings put the box in front of the \
                 ladder, worst {worst}"
            );
        }

        // And away from a switchover it is not marginal at all. At a bearing of
        // 45 degrees the eye sees the east and north faces square on, the
        // ladder rides the south-east edge, and the nearest thing behind it is
        // most of the box away.
        let mid_quadrant = orbit(std::f32::consts::FRAC_PI_4, 0.45, 2.4);
        let spine = ladder_spine(&mid_quadrant, pane()).expect("a ladder at 45 deg");
        let reach = outward_reach(&mid_quadrant).expect("a clearance at 45 deg");
        assert!(
            reach < -0.5 * spine.length(),
            "at 45 deg the box comes within {reach} points of a ladder {} points long",
            spine.length()
        );
    }

    #[test]
    fn the_ladder_edge_is_always_one_of_the_four_and_walks_all_four_in_a_turn() {
        let mut vol3d = Vol3d::default();
        let mut seen = BTreeSet::new();
        for degrees in 0..360 {
            vol3d.yaw = (degrees as f32).to_radians();
            let edge = ladder_edge(&vol3d, pane())
                .unwrap_or_else(|| panic!("yaw {degrees} deg drew no ladder"));
            assert!(
                LadderEdge::ALL.contains(&edge),
                "yaw {degrees} deg chose {edge:?}, which is not a box edge"
            );
            seen.insert(edge);
        }
        assert_eq!(
            seen.len(),
            4,
            "a full turn should visit every corner, visited {seen:?}"
        );
    }

    #[test]
    fn the_ladder_rides_the_edge_a_quarter_turn_anticlockwise_of_the_camera() {
        // Mid-quadrant bearings, far enough from a face plane that the answer
        // is not in doubt: at 45 degrees the eye sees the east and north faces,
        // so the outline is bounded by the south-east and north-west edges, and
        // the south-east one is the left of those.
        for (yaw_degrees, expected, exactly_anticlockwise) in [
            (45.0_f32, LadderEdge::SouthEast, true),
            (135.0, LadderEdge::NorthEast, true),
            (225.0, LadderEdge::NorthWest, true),
            (315.0, LadderEdge::SouthWest, true),
            // The default view, 0.6 rad = 34.4 deg, sees the same two faces as
            // 45 deg and so hangs the ladder on the same corner - which is then
            // 10.6 deg off a quarter turn, because a box has four corners and a
            // camera has a continuum of bearings. Only at the mid-quadrant
            // bearings above do the two coincide exactly.
            (0.6_f32.to_degrees(), LadderEdge::SouthEast, false),
        ] {
            let vol3d = orbit(yaw_degrees.to_radians(), 0.45, 2.4);
            assert_eq!(
                ladder_edge(&vol3d, pane()),
                Some(expected),
                "camera bearing {yaw_degrees} deg"
            );
            // The same claim from the other side: at these bearings the chosen
            // corner stands a quarter turn anticlockwise of the camera.
            let ideal = yaw_degrees.to_radians() - std::f32::consts::FRAC_PI_2;
            let mut offset = (expected.bearing() - ideal).rem_euclid(std::f32::consts::TAU);
            if offset > std::f32::consts::PI {
                offset -= std::f32::consts::TAU;
            }
            assert_eq!(
                offset.abs() < 1.0e-3,
                exactly_anticlockwise,
                "camera bearing {yaw_degrees} deg chose {expected:?}, {} deg off",
                offset.to_degrees()
            );
        }
    }

    #[test]
    fn the_ladder_slides_along_its_own_line_at_a_switchover_rather_than_jumping() {
        // There is nothing to add hysteresis to: the choice is a function of
        // the camera, so it cannot oscillate while the camera is still. What it
        // must not do is jump the ladder across the box as the operator drags
        // through a switchover. It does not, because a switchover happens as
        // the eye crosses the plane of the face BETWEEN the two edges, and a
        // plane through the eye projects to a line - at that instant both edges
        // lie on the one line.
        let mut vol3d = Vol3d::default();
        let mut previous: Option<LadderSpine> = None;
        let mut switches = 0;
        let mut worst = 0.0_f32;
        for twentieths in 0..7_200 {
            vol3d.yaw = (twentieths as f32 * 0.05).to_radians();
            let Some(spine) = ladder_spine(&vol3d, pane()) else {
                continue;
            };
            if let Some(last) = previous
                && last.edge != spine.edge
            {
                switches += 1;
                for point in [spine.foot, spine.head] {
                    let across = (point - last.foot).dot(last.outward).abs();
                    worst = worst.max(across);
                    assert!(
                        across < 2.0,
                        "at yaw {} deg the ladder moved from {:?} to {:?} and \
                         jumped {across} points sideways off its own line",
                        twentieths as f32 * 0.05,
                        last.edge,
                        spine.edge
                    );
                }
            }
            previous = Some(spine);
        }
        assert_eq!(
            switches, 4,
            "a full turn changes edge four times, not {switches}"
        );
        assert!(worst < 2.0, "worst sideways jump {worst} points");
    }

    #[test]
    fn the_ladder_hangs_on_the_edge_that_was_chosen_and_draws_it_end_to_end() {
        let vol3d = Vol3d {
            show_labels: true,
            ..all_off()
        };
        let spine = ladder_spine(&vol3d, pane()).expect("the default camera carries a ladder");
        let (east, north) = spine.edge.footprint();
        // The spine is the projected edge, not something near it.
        assert_eq!(
            Some(spine.foot),
            vol3d.project_point(pane(), [east, north, 0.0])
        );
        assert_eq!(
            Some(spine.head),
            vol3d.project_point(pane(), [east, north, vol3d.zspan()])
        );
        let painted = painted(&vol3d);
        assert!(painted.touches(spine.foot));
        assert!(painted.touches(spine.head));
    }

    #[test]
    fn a_camera_over_the_footprint_drops_the_ladder_instead_of_labelling_the_storm() {
        // Orbit pitched to 83 degrees and zoomed in: the eye is 0.29 half-widths
        // from the axis, INSIDE the footprint, so no vertical face is turned
        // toward it and no vertical edge has empty sky beside it. Every edge
        // would carry its labels over the volume, so none of them carries them.
        let overhead = orbit(0.6, 1.45, 2.4);
        let eye = camera_eye(&overhead);
        assert!(
            eye[0].hypot(eye[1]) < FOOTPRINT_HALF,
            "this fixture wants the eye over the footprint, it is at {}",
            eye[0].hypot(eye[1])
        );
        for degrees in 0..360 {
            let vol3d = orbit((degrees as f32).to_radians(), 1.45, 2.4);
            assert!(
                ladder_spine(&vol3d, pane()).is_none(),
                "yaw {degrees} deg drew a ladder from over the box"
            );
        }
        let painted = painted(&overhead);
        assert!(!painted.says("kft"), "{:?}", painted.labels);
        assert_eq!(painted.rungs(), 0);
        // The wireframe and the floor grid are unaffected: this is a plan view,
        // and a plan view is a perfectly good thing to be looking at.
        assert!(painted.segments.len() >= 22);
    }

    #[test]
    fn an_eye_inside_the_box_draws_no_ladder_at_all() {
        for (label, x, y, z) in [
            ("the exact box centre", 0.0, 0.0, 0.0),
            ("inside the volume", 0.2, -0.3, 0.4),
            ("on the roof", 0.0, 0.0, 0.9),
            ("under the floor", 0.1, 0.1, -0.5),
        ] {
            let vol3d = fly(x, y, z);
            assert!(
                ladder_spine(&vol3d, pane()).is_none(),
                "{label} drew a ladder from inside the box"
            );
            let painted = painted(&vol3d);
            assert!(!painted.says("kft"), "{label}: {:?}", painted.labels);
            assert!(painted.is_finite(), "{label} painted a non-finite point");
        }
    }

    // -- what actually reaches the screen ---------------------------------

    #[test]
    fn nothing_at_all_is_painted_when_every_toggle_is_off() {
        let painted = painted(&all_off());
        assert!(painted.segments.is_empty(), "{:?}", painted.segments);
        assert!(painted.labels.is_empty(), "{:?}", painted.labels);
    }

    #[test]
    fn each_toggle_pays_for_only_its_own_annotation() {
        let grid = painted(&Vol3d {
            show_grid: true,
            ..all_off()
        });
        // Five lines each way across the footprint, and no words on any of them.
        assert_eq!(grid.segments.len(), 10);
        assert!(grid.labels.is_empty());

        let wireframe = painted(&Vol3d {
            show_box: true,
            ..all_off()
        });
        // Four corners times floor edge, roof edge and vertical strut.
        assert_eq!(wireframe.segments.len(), 12);
        assert!(wireframe.labels.is_empty());

        let labels = painted(&Vol3d {
            show_labels: true,
            ..all_off()
        });
        // One spine plus one arm per rung; the ladder carries its own edge so
        // it stays a scale with the wireframe switched off. Counted from the
        // rungs actually painted rather than from a literal, so a change to the
        // default exaggeration thins the ladder without breaking this.
        assert_eq!(labels.segments.len(), 1 + labels.rungs());
        assert!(labels.says("10") && labels.says("50"));
    }

    #[test]
    fn the_default_view_paints_the_whole_ladder_its_unit_once_and_the_near_cardinals() {
        let painted = painted(&Vol3d::default());
        for rung in ["10", "20", "30", "40", "50"] {
            assert!(
                painted.says(rung),
                "missing rung {rung}: {:?}",
                painted.labels
            );
        }
        // The unit belongs to the ladder, not to each rung.
        assert_eq!(
            painted
                .labels
                .iter()
                .filter(|label| label.text == "kft")
                .count(),
            1,
            "{:?}",
            painted.labels
        );
        // The default camera stands east-north-east of the box at 1.78 east and
        // 1.22 north, both outside their face planes; the other two faces are
        // behind the volume from there.
        assert!(painted.says("NORTH"));
        assert!(painted.says("EAST"));
        assert!(!painted.says("SOUTH"));
        assert!(!painted.says("WEST"));
        // Grid, wireframe, ladder spine, and one arm per rung.
        assert_eq!(painted.segments.len(), 10 + 12 + 1 + painted.rungs());
    }

    #[test]
    fn no_two_ladder_labels_are_painted_on_top_of_one_another() {
        // The rectangles here are the ones `egui` fills with glyphs, so this is
        // legibility measured rather than asserted. The exaggerations run from
        // the bottom of the explorer's range up, because `zspan` is what
        // shortens the spine and crowds the rungs, and the unit label is in the
        // list because it sits a fixed fraction above the top rung and so
        // collides on a short spine however many rungs are thinned away.
        for exaggeration in [0.1, 0.3, 0.5, 1.0, 1.5, 3.0, 6.0] {
            for yaw_degrees in [0.0_f32, 34.4, 90.0, 137.0, 210.0, 300.0] {
                let vol3d = Vol3d {
                    vertical_exaggeration: exaggeration,
                    yaw: yaw_degrees.to_radians(),
                    ..Default::default()
                };
                let painted = painted(&vol3d);
                let labels = painted.ladder_labels();
                for (index, one) in labels.iter().enumerate() {
                    for other in &labels[index + 1..] {
                        let overlap = one.area.intersect(other.area);
                        assert!(
                            !overlap.is_positive(),
                            "at exaggeration {exaggeration} and yaw {yaw_degrees} \
                             deg the labels {:?} and {:?} overlap by {:?}",
                            one,
                            other,
                            overlap.size()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_wireframe_roof_stands_at_the_top_of_the_volume_not_at_one_world_unit() {
        // The bug this replaces: a roof drawn at a literal z = 1.0 while the
        // shader marches to z = zspan stands taller or shorter than the volume
        // inside it by whatever ratio the box size and exaggeration produce,
        // and a height scale hung on it reads every echo top wrong by the same
        // factor.
        let vol3d = Vol3d {
            show_box: true,
            ..all_off()
        };
        // The fixture only says anything while the two heights differ, and the
        // default exaggeration is the pane's to choose, not this test's.
        assert!(
            (vol3d.zspan() - 1.0).abs() > 0.05,
            "the default zspan is {}, too close to the old literal 1.0 for this \
             test to distinguish them",
            vol3d.zspan()
        );
        let (east, north) = LadderEdge::NorthEast.footprint();
        let roof = vol3d
            .project_point(pane(), [east, north, vol3d.zspan()])
            .expect("roof corner is in front of the camera");
        let one_world_unit = vol3d
            .project_point(pane(), [east, north, 1.0])
            .expect("the old roof corner is in front of the camera too");
        let painted = painted(&vol3d);
        assert!(painted.touches(roof), "the roof is not at the volume top");
        assert!(
            !painted.touches(one_world_unit),
            "the roof is still at the old literal 1.0"
        );
    }

    #[test]
    fn a_camera_that_sees_none_of_the_box_paints_no_label_and_no_line() {
        let vol3d = looking_away();
        // The two near faces pass the visibility rule, so anything that reaches
        // the screen got there through a projection rather than around it.
        assert!(cardinal_is_in_front(&vol3d, Cardinal::East));
        assert!(cardinal_is_in_front(&vol3d, Cardinal::North));
        // Nothing on the box is in front of this camera.
        assert!(vol3d.project_point(pane(), [1.0, 1.0, 0.0]).is_none());
        assert!(
            vol3d
                .project_point(pane(), [0.0, CARDINAL_OFFSET, 0.0])
                .is_none()
        );

        let painted = painted(&vol3d);
        assert!(painted.labels.is_empty(), "{:?}", painted.labels);
        assert!(painted.segments.is_empty(), "{:?}", painted.segments);
    }

    #[test]
    fn every_hostile_camera_paints_real_numbers_inside_the_pane() {
        // Nothing here is a camera an operator would choose. They are the
        // states a camera passes THROUGH, plus the states a broken slider could
        // leave it in. A NaN reaching the painter does not crash `egui` - it
        // drops or smears the shape - which is exactly the kind of failure that
        // survives to a release.
        let hostile: Vec<(&str, Vol3d)> = vec![
            ("the box centre", fly(0.0, 0.0, 0.0)),
            (
                "straight down the axis",
                orbit(0.6, std::f32::consts::FRAC_PI_2, 2.4),
            ),
            (
                "straight up the axis",
                orbit(0.6, -std::f32::consts::FRAC_PI_2, 2.4),
            ),
            ("pitched past vertical", orbit(0.6, 3.0, 2.4)),
            (
                "upside down underneath",
                Vol3d {
                    yaw: 3.9,
                    pitch: -0.9,
                    ..fly(1.5, 1.5, -2.0)
                },
            ),
            ("grazing a face plane", fly(FOOTPRINT_HALF, 0.0, 0.45)),
            ("a NaN pitch", orbit(0.6, f32::NAN, 2.4)),
            ("a NaN yaw", orbit(f32::NAN, 0.45, 2.4)),
            ("a NaN eye", fly(f32::NAN, f32::NAN, f32::NAN)),
            (
                "a zero field of view",
                Vol3d {
                    fov_scale: 0.0,
                    ..Default::default()
                },
            ),
            (
                "a NaN field of view",
                Vol3d {
                    fov_scale: f32::NAN,
                    ..Default::default()
                },
            ),
            (
                "an infinite orbit distance",
                orbit(0.6, 0.45, f32::INFINITY),
            ),
            ("no orbit distance", orbit(0.6, 0.45, 0.0)),
            (
                "a flattened box",
                Vol3d {
                    vertical_exaggeration: 0.0,
                    ..Default::default()
                },
            ),
            (
                "a NaN exaggeration",
                Vol3d {
                    vertical_exaggeration: f32::NAN,
                    ..Default::default()
                },
            ),
        ];
        let mut painted_something = false;
        for (label, vol3d) in hostile {
            let painted = painted(&vol3d);
            assert!(
                painted.is_finite(),
                "{label} put a non-finite coordinate on the screen: {:?} {:?}",
                painted.segments,
                painted.labels
            );
            // Everything goes through the pane's own painter, so nothing this
            // module emits can be drawn over the panel beside it however far
            // off the pane the projection lands.
            for clip in &painted.clips {
                assert!(
                    pane().contains_rect(*clip),
                    "{label} painted with a clip rectangle {clip:?} outside the pane"
                );
            }
            painted_something |= !painted.segments.is_empty() || !painted.labels.is_empty();
            // Whatever the camera, an orientation label is either on the pane or
            // not painted: those are anchored points, not clipped lines.
            for cardinal in Cardinal::ALL {
                if let Some(centre) = painted.centre_of(cardinal.label()) {
                    assert!(
                        pane().contains(centre),
                        "{label} painted {} at {centre:?}, off the pane",
                        cardinal.label()
                    );
                }
            }
        }
        // Not vacuous: several of these cameras are ordinary enough to draw a
        // box, and a test that only ever inspected an empty screen would pass
        // whatever the module did with a NaN.
        assert!(
            painted_something,
            "every hostile camera painted nothing, so this proved nothing"
        );
    }

    // -- the ground-plane labels -------------------------------------------

    #[test]
    fn the_cardinals_are_painted_where_the_camera_says_they_are_not_mirrored() {
        // `project_point` builds a right-handed screen frame: `right =
        // (fwd.y, -fwd.x)` with +z up, so for a camera at bearing B the screen
        // right axis is `(-sin B, cos B)`. An observer east-north-east of the
        // box and looking back at it therefore has NORTH on the right of the
        // screen and EAST on the left - stand east of a map facing west and
        // north is on your right hand. Get the sign wrong here and every three
        // dimensional reading an operator makes off this pane is mirrored.
        let from_ene = Vol3d::default();
        let centre = from_ene
            .project_point(pane(), [0.0, 0.0, 0.0])
            .expect("the box centre is in front of the default camera");
        let from_the_near_side = painted(&from_ene);

        for (cardinal, expected_side) in [(Cardinal::North, 1.0_f32), (Cardinal::East, -1.0)] {
            let (east, north) = cardinal.outward();
            let anchor = from_ene
                .project_point(
                    pane(),
                    [east * CARDINAL_OFFSET, north * CARDINAL_OFFSET, 0.0],
                )
                .expect("the near cardinals are in front of the default camera");
            let drawn = from_the_near_side
                .centre_of(cardinal.label())
                .unwrap_or_else(|| panic!("{} was not painted", cardinal.label()));
            // The label is painted AT its anchor, not near it.
            assert!(
                (drawn - anchor).length() < 1.0,
                "{} was painted at {drawn:?} and its anchor projects to {anchor:?}",
                cardinal.label()
            );
            assert!(
                (drawn.x - centre.x) * expected_side > 40.0,
                "{} landed at x {} with the box centre at x {}, which is the \
                 wrong side of the screen",
                cardinal.label(),
                drawn.x,
                centre.x
            );
        }

        // The mirror image of the same claim, from the other side of the box: a
        // camera west-north-west of it, looking east-south-east, has NORTH on
        // its LEFT and WEST on its right.
        let from_wnw = orbit(2.2, 0.45, 2.4);
        let centre = from_wnw
            .project_point(pane(), [0.0, 0.0, 0.0])
            .expect("the box centre is in front of this camera");
        let from_the_other_side = painted(&from_wnw);
        for (cardinal, expected_side) in [(Cardinal::North, -1.0_f32), (Cardinal::West, 1.0)] {
            let drawn = from_the_other_side
                .centre_of(cardinal.label())
                .unwrap_or_else(|| panic!("{} was not painted", cardinal.label()));
            assert!(
                (drawn.x - centre.x) * expected_side > 40.0,
                "from the west-north-west, {} landed at x {} with the box centre \
                 at x {}",
                cardinal.label(),
                drawn.x,
                centre.x
            );
        }
        assert!(
            !from_the_other_side.says("EAST"),
            "the east face is behind the volume"
        );
        assert!(
            !from_the_other_side.says("SOUTH"),
            "the south face is behind the volume"
        );
    }

    #[test]
    fn a_label_that_projects_off_the_pane_is_dropped_rather_than_clipped() {
        // Eye north-east of the box, looking well past it: both near faces are
        // outside their planes, and both labels still project - they simply
        // land outside the pane.
        let vol3d = Vol3d {
            yaw: std::f32::consts::FRAC_PI_4 + 1.0,
            pitch: 0.175,
            ..fly(8.0, 8.0, 2.0)
        };
        assert!(cardinal_is_in_front(&vol3d, Cardinal::East));
        assert!(cardinal_is_in_front(&vol3d, Cardinal::North));
        assert!(
            vol3d
                .project_point(pane(), [0.0, CARDINAL_OFFSET, 0.0])
                .is_some(),
            "the north label is in front of the camera, just not on the pane"
        );

        let painted = painted(&vol3d);
        for cardinal in Cardinal::ALL {
            assert!(
                !painted.says(cardinal.label()),
                "{} was painted off the pane",
                cardinal.label()
            );
        }
    }

    #[test]
    fn an_eye_inside_a_face_plane_loses_that_faces_label() {
        // Flown to 1.4 east and 0.8 north of the box centre. The footprint runs
        // to 1.0 each way, so the eye is outside the east face and INSIDE the
        // north one: a NORTH label from here would be painted through the
        // storm, and an EAST label would not.
        let close = fly(1.4, 0.8, 0.5);
        assert!(cardinal_is_in_front(&close, Cardinal::East));
        assert!(!cardinal_is_in_front(&close, Cardinal::North));
        assert!(!cardinal_is_in_front(&close, Cardinal::South));
        assert!(!cardinal_is_in_front(&close, Cardinal::West));

        // Exactly on the plane counts as inside: the ray to a point on that
        // face then runs along the face, grazing the volume for its whole
        // length.
        let grazing = Vol3d {
            fly_x: FOOTPRINT_HALF,
            ..close
        };
        assert!(!cardinal_is_in_front(&grazing, Cardinal::East));
    }

    #[test]
    fn a_box_too_flat_to_read_drops_the_ladder_and_keeps_the_cardinals() {
        // Exaggeration clamped to the floor of `zspan` leaves about eleven
        // points of projected edge - less than one label height.
        let flat = Vol3d {
            vertical_exaggeration: 0.1,
            ..Default::default()
        };
        assert!((flat.zspan() - 0.06).abs() < 1.0e-6, "{}", flat.zspan());
        assert!(
            ladder_spine(&flat, pane()).is_some_and(|spine| spine.length() < MIN_LADDER_PX),
            "this fixture wants an edge too short to read, not a missing one"
        );
        let painted = painted(&flat);
        assert!(!painted.says("kft"), "{:?}", painted.labels);
        assert!(!painted.says("10"), "{:?}", painted.labels);
        // The orientation labels do not depend on the box height, so they stay.
        assert!(painted.says("NORTH"));
        assert!(painted.says("EAST"));
    }

    #[test]
    fn the_rungs_are_projected_individually_rather_than_spaced_evenly_on_screen() {
        // Perspective is not affine along a line. The box edge leans toward or
        // away from the camera, so equal steps in height do NOT land equally
        // spaced on screen. Interpolating rung positions along the projected
        // spine - the cheap way to draw a ladder - would make these two gaps
        // exactly equal, and this test is what says so.
        let vol3d = Vol3d::default();
        let spine = ladder_spine(&vol3d, pane()).expect("the default camera carries a ladder");
        let (east, north) = spine.edge.footprint();
        let zspan = vol3d.zspan();
        let top_kft = f64::from(vol3d.top_km() * 1000.0) * METERS_TO_KILOFEET;
        let at = |kft: f64| {
            vol3d
                .project_point(pane(), [east, north, rung_z(kft, top_kft, zspan)])
                .expect("rung is in front of the default camera")
        };
        let low = (at(20.0) - at(10.0)).length();
        let high = (at(50.0) - at(40.0)).length();
        assert!(
            (high - low).abs() > low * 0.05,
            "10-20 kft spans {low} points and 40-50 kft spans {high}; \
             the edge is not foreshortening at all"
        );
    }
}
