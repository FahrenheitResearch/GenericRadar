use radar_core::ProductId;
use serde::{Deserialize, Serialize};

use crate::{Camera2D, ViewportMetrics, WorldPoint};

pub const MAX_PANES: usize = 4;
const PANE_0: PaneId = PaneId(0);
const PANE_1: PaneId = PaneId(1);
const PANE_2: PaneId = PaneId(2);
const PANE_3: PaneId = PaneId(3);
const ONE_PANE: &[PaneId] = &[PANE_0];
const TWO_PANES: &[PaneId] = &[PANE_0, PANE_1];
const FOUR_PANES: &[PaneId] = &[PANE_0, PANE_1, PANE_2, PANE_3];

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(u8);

impl PaneId {
    pub const fn new(index: u8) -> Option<Self> {
        if index < MAX_PANES as u8 {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PaneLayout {
    One,
    TwoHorizontal,
    TwoVertical,
    Four,
}

impl PaneLayout {
    pub const fn visible_panes(self) -> &'static [PaneId] {
        match self {
            Self::One => ONE_PANE,
            Self::TwoHorizontal | Self::TwoVertical => TWO_PANES,
            Self::Four => FOUR_PANES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TiltSelection {
    LowestAvailable,
    NearestElevationTenths(i16),
    CutIndex(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SmoothingMode {
    Nearest,
    Linear,
    HighQuality,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StormMotionIntent {
    /// Meteorological direction from which the storm moves.
    pub direction_from_deg: f32,
    pub speed_mps: f32,
}

impl Default for StormMotionIntent {
    fn default() -> Self {
        Self {
            direction_from_deg: 240.0,
            speed_mps: 15.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneLinkGroups {
    pub camera: Option<u8>,
    pub timeline: Option<u8>,
    pub tilt: Option<u8>,
    pub product: Option<u8>,
    pub cursor: Option<u8>,
}

impl Default for PaneLinkGroups {
    fn default() -> Self {
        Self {
            camera: Some(0),
            timeline: Some(0),
            tilt: None,
            product: None,
            cursor: Some(0),
        }
    }
}

/// Serializable user intent for one pane. Runtime-only textures, worker state,
/// and caches live outside this type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneIntent {
    pub product: ProductId,
    pub tilt: TiltSelection,
    pub camera: Camera2D,
    pub opacity: u8,
    pub smoothing: SmoothingMode,
    pub storm_motion: StormMotionIntent,
    pub links: PaneLinkGroups,
    pub overlays_visible: bool,
}

impl Default for PaneIntent {
    fn default() -> Self {
        Self {
            product: ProductId("REF".to_owned()),
            tilt: TiltSelection::LowestAvailable,
            camera: Camera2D::default(),
            opacity: u8::MAX,
            smoothing: SmoothingMode::Linear,
            storm_motion: StormMotionIntent::default(),
            links: PaneLinkGroups::default(),
            overlays_visible: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub layout: PaneLayout,
    pub active_pane: PaneId,
    pub panes: [PaneIntent; MAX_PANES],
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            layout: PaneLayout::One,
            active_pane: PANE_0,
            panes: std::array::from_fn(|_| PaneIntent::default()),
        }
    }
}

impl WorkspaceState {
    pub fn visible_panes(&self) -> &'static [PaneId] {
        self.layout.visible_panes()
    }

    pub fn pane(&self, id: PaneId) -> &PaneIntent {
        &self.panes[id.index()]
    }

    pub fn pane_mut(&mut self, id: PaneId) -> &mut PaneIntent {
        &mut self.panes[id.index()]
    }

    pub fn active(&self) -> &PaneIntent {
        self.pane(self.active_pane)
    }

    pub fn active_mut(&mut self) -> &mut PaneIntent {
        self.pane_mut(self.active_pane)
    }

    pub fn set_layout(&mut self, layout: PaneLayout) {
        self.layout = layout;
        if !self.visible_panes().contains(&self.active_pane) {
            self.active_pane = self.visible_panes()[0];
        }
    }

    pub fn set_active(&mut self, pane: PaneId) -> bool {
        if !self.visible_panes().contains(&pane) {
            return false;
        }
        self.active_pane = pane;
        true
    }

    pub fn cycle_active(&mut self, delta: isize) -> PaneId {
        let visible = self.visible_panes();
        let current = visible
            .iter()
            .position(|pane| *pane == self.active_pane)
            .unwrap_or(0) as isize;
        let len = visible.len() as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        self.active_pane = visible[next];
        self.active_pane
    }

    /// Install a camera change from one pane and propagate it to panes in the
    /// same camera link group. Returns every pane whose intent changed.
    pub fn apply_camera_from(&mut self, source: PaneId, camera: Camera2D) -> Vec<PaneId> {
        let source_group = self.pane(source).links.camera;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let pane = PaneId(index as u8);
            let linked = pane == source
                || source_group.is_some_and(|group| self.pane(pane).links.camera == Some(group));
            if linked && self.pane(pane).camera != camera {
                self.pane_mut(pane).camera = camera;
                changed.push(pane);
            }
        }
        changed
    }

    pub fn apply_product_from(&mut self, source: PaneId, product: ProductId) -> Vec<PaneId> {
        let source_group = self.pane(source).links.product;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let pane = PaneId(index as u8);
            let linked = pane == source
                || source_group.is_some_and(|group| self.pane(pane).links.product == Some(group));
            if linked && self.pane(pane).product != product {
                self.pane_mut(pane).product = product.clone();
                changed.push(pane);
            }
        }
        changed
    }

    pub fn apply_tilt_from(&mut self, source: PaneId, tilt: TiltSelection) -> Vec<PaneId> {
        let source_group = self.pane(source).links.tilt;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let pane = PaneId(index as u8);
            let linked = pane == source
                || source_group.is_some_and(|group| self.pane(pane).links.tilt == Some(group));
            if linked && self.pane(pane).tilt != tilt {
                self.pane_mut(pane).tilt = tilt;
                changed.push(pane);
            }
        }
        changed
    }

    /// Put the current anchor in the middle of every pane at `km_per_point`.
    ///
    /// This is the opening move of a session, before any radar has been on
    /// screen and so before there is any view of the analyst's to protect. The
    /// hand-over at the other end of the overview is
    /// [`Self::leave_overview`], which is a different question and answers it
    /// differently. Rotation is carried over rather than reset -- an analyst
    /// who turned the map to put a front across the screen asked for that, and
    /// no part of arriving at a radar undoes it.
    ///
    /// Returns every pane whose camera moved. It ignores camera link groups
    /// deliberately: every pane is being pointed at the same anchor, so they
    /// all end up sharing the camera a link group would have given them.
    pub fn centre_on_anchor(&mut self, km_per_point: f32) -> Vec<PaneId> {
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let camera = &mut self.panes[index].camera;
            let next = Camera2D {
                center_east_km: 0.0,
                center_north_km: 0.0,
                km_per_point,
                rotation_rad: camera.rotation_rad,
            }
            .sanitized();
            if *camera != next {
                *camera = next;
                changed.push(PaneId(index as u8));
            }
        }
        changed
    }

    /// Re-derive every pane's camera across a change of radar.
    ///
    /// [`Camera2D`] is stated in kilometres east and north OF THE ANTENNA, so
    /// changing radar silently redefines what it means. Leaving it alone --
    /// which is what the application did before this existed -- teleports the
    /// screen centre by the distance between the two sites: measured on the
    /// real catalog, 156.1 km for KMKX to KLOT and 22.1 km for KTLX to the
    /// terminal radar over the same city. Neither is a view the analyst asked
    /// for, and 22 km is enough to lose a mesocyclone at analysis zoom.
    ///
    /// `reproject` maps a point in the OLD anchor's world frame to the same
    /// point on the ground in the NEW anchor's frame, and returns `None` where
    /// that cannot be answered. It is supplied by the caller because the
    /// geodesy lives in the map scene, which depends on this crate; the
    /// transform is not a translation, so composing the inverse and forward
    /// projections is the only exact way to do it. See Vincenty, T., 1975:
    /// Direct and inverse solutions of geodesics on the ellipsoid with
    /// application of nested equations. Survey Review, 23, 88-93,
    /// doi:10.1179/sre.1975.23.176.88; and, for the modern algorithm the
    /// implementation was cross-checked against, Karney, C. F. F., 2013:
    /// Algorithms for geodesics. J. Geodesy, 87, 43-55,
    /// doi:10.1007/s00190-012-0578-0.
    ///
    /// `viewports` is each pane's last known size, `None` for a pane that has
    /// never been laid out. Such a pane is measured as a point: it has never
    /// shown the analyst anything, so there is no view of theirs to protect,
    /// and it will usually end up on the antenna. In the layouts that hide
    /// panes those are the hidden ones, and by default they are camera-linked
    /// to the visible pane and follow it instead.
    ///
    /// Camera link groups decide ONCE, on the active pane where it is a
    /// member. Two panes that share a camera can genuinely disagree -- in
    /// every layout the visible panes are the same size, but a pane that has
    /// never been laid out is measured as a point and falls out of coverage
    /// while the visible pane it is linked to holds -- and a group left
    /// straddling the threshold would show the analyst two different pieces of
    /// ground the moment they switched layout.
    ///
    /// The group's DECISION is shared; the group's CAMERA is not. Each pane
    /// carries its OWN centre across, so a group that was already holding two
    /// cameras -- which the "Link cameras" button produces in two clicks, by
    /// unlinking, panning the panes apart and linking again -- keeps them.
    /// Copying the leader's camera over the rest would teleport every other
    /// pane in the group to the active pane's ground, which is the complaint
    /// this method exists to answer, not a fix for it.
    ///
    /// Returns every pane whose camera moved.
    pub fn apply_site_change(
        &mut self,
        viewports: &[Option<ViewportMetrics>; MAX_PANES],
        mut reproject: impl FnMut(WorldPoint) -> Option<WorldPoint>,
    ) -> Vec<PaneId> {
        let mut decided = [None; MAX_PANES];
        for index in std::iter::once(self.active_pane.index()).chain(0..MAX_PANES) {
            if decided[index].is_some() {
                continue;
            }
            let leader = PaneId(index as u8);
            let previous = self.pane(leader).camera.sanitized();
            let held = reproject(WorldPoint::new(
                previous.center_east_km,
                previous.center_north_km,
            ));
            let decision = decide_across_site_change(previous, held, viewports[index]);
            let group = self.pane(leader).links.camera;
            for (follower, slot) in decided.iter_mut().enumerate() {
                let shares_camera = follower == index
                    || (group.is_some() && self.panes[follower].links.camera == group);
                if shares_camera && slot.is_none() {
                    *slot = Some(decision);
                }
            }
        }
        let mut changed = Vec::new();
        for (index, decision) in decided.into_iter().enumerate() {
            let Some(decision) = decision else {
                continue;
            };
            let previous = self.panes[index].camera.sanitized();
            let camera = match decision {
                SiteChangeDecision::Recentre => recentred(previous),
                SiteChangeDecision::HoldGeographic => {
                    match reproject(WorldPoint::new(
                        previous.center_east_km,
                        previous.center_north_km,
                    ))
                    .filter(|held| held.east_km.is_finite() && held.north_km.is_finite())
                    {
                        Some(held) => Camera2D {
                            center_east_km: held.east_km,
                            center_north_km: held.north_km,
                            ..previous
                        },
                        // Only reachable for a pane whose own centre the
                        // geodesic cannot answer while the group's leader is
                        // holdable, which means the group was already carrying
                        // two cameras. The antenna is the one place that is
                        // certainly real; see `SiteChangeDecision::Recentre`.
                        None => recentred(previous),
                    }
                }
            };
            if self.panes[index].camera != camera {
                self.panes[index].camera = camera;
                changed.push(PaneId(index as u8));
            }
        }
        changed
    }

    /// Hand the opening overview over to the session's first radar.
    ///
    /// The overview is a map of the country at `overview_km_per_point` with
    /// the placeholder anchor -- a fixed point near the geographic centre of
    /// the contiguous United States, not a place any analyst chose -- in the
    /// middle. A pane still holding it exactly as it opened has nothing of the
    /// analyst's on it, so it drops to `km_per_point`, the scale a radar is
    /// read at. That is the whole hand-over: the centre is already the anchor,
    /// and the anchor is now the antenna.
    ///
    /// A pane whose camera has been STATED is left completely alone, and the
    /// distinction matters more than it looks. The application used to reset
    /// every pane to [`Camera2D::default`] here, which silently discarded the
    /// `--zoom` and `--center` startup flags -- the flags that exist so a
    /// particular view can be photographed on real data without driving the
    /// window by hand, and which therefore have to survive the first volume,
    /// because there is nothing on screen until one arrives.
    ///
    /// Nothing is reprojected here, and that is deliberate: this is the ONE
    /// anchor change in the application where radar-local kilometres are the
    /// right thing to keep. `--center 40,-60` means 40 km east and 60 km north
    /// OF THE RADAR -- it is written by someone who has not seen the
    /// placeholder and does not care where it is -- so carrying the ground
    /// under it across from Kansas would be a faithful answer to a question
    /// nobody asked. Every LATER anchor change is a change between two real
    /// radars the analyst has actually been looking at, and those are held
    /// geographically by [`Self::apply_site_change`].
    ///
    /// Returns every pane whose camera moved.
    pub fn leave_overview(&mut self, overview_km_per_point: f32, km_per_point: f32) -> Vec<PaneId> {
        let overview_km_per_point = Camera2D {
            km_per_point: overview_km_per_point,
            ..Camera2D::default()
        }
        .sanitized()
        .km_per_point;
        let mut changed = Vec::new();
        for index in 0..MAX_PANES {
            let camera = self.panes[index].camera.sanitized();
            // Rotation is deliberately not compared: turning the overview to
            // put a front across the screen states an orientation, not a view,
            // and the hand-over carries it through either way.
            let untouched = camera.center_east_km == 0.0
                && camera.center_north_km == 0.0
                && camera.km_per_point == overview_km_per_point;
            if !untouched {
                continue;
            }
            let next = Camera2D {
                km_per_point,
                ..camera
            }
            .sanitized();
            if self.panes[index].camera != next {
                self.panes[index].camera = next;
                changed.push(PaneId(index as u8));
            }
        }
        changed
    }
}

/// Nominal ground range of a WSR-88D surveillance sweep, in kilometres.
///
/// 460 km is the reflectivity sweep, not the Doppler sweep -- velocity stops
/// well short of it -- and the wider figure is a deliberate choice rather than
/// an oversight. It is the radius inside which the incoming radar sees
/// anything at all, and the two errors do not cost the same. Holding a view
/// the new radar barely reaches leaves the analyst on their own ground with
/// thin data over it, one drag from where they want to be. Recentring a view
/// the new radar could have covered throws away where they were looking, which
/// they cannot get back. So the threshold errs towards holding.
///
/// It is applied to the middle of the screen, and only there; see
/// [`decide_across_site_change`] for the measurement that settled that.
///
/// Crum, T. D., and R. L. Alberty, 1993: The WSR-88D and the WSR-88D
/// Operational Support Facility. Bull. Amer. Meteor. Soc., 74, 1669-1687,
/// doi:10.1175/1520-0477(1993)074<1669:TWATWO>2.0.CO;2.
const NEXRAD_SURVEILLANCE_RANGE_KM: f64 = 460.0;

/// What a site change did to one camera.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SiteChangeDecision {
    /// The new radar covers the ground the analyst was on, so the same ground
    /// stays on screen at the same scale and orientation. Nothing visibly
    /// moves except the data.
    HoldGeographic,
    /// The new radar is too far from that ground for holding it to show
    /// anything, so the antenna goes in the middle. The analyst's scale and
    /// rotation are still theirs; only the centre moves.
    Recentre,
}

/// The camera `previous` becomes when the new radar cannot serve the view:
/// the antenna in the middle, at the analyst's own scale and rotation.
fn recentred(previous: Camera2D) -> Camera2D {
    Camera2D {
        center_east_km: 0.0,
        center_north_km: 0.0,
        ..previous.sanitized()
    }
}

/// Decide one camera's fate across a site change.
///
/// The question is not "is the new radar near the old one" but "does the new
/// radar serve what this analyst is looking at": an analyst who has followed a
/// storm 200 km downrange is looking at ground that a neighbouring radar may
/// cover far better than the one they are leaving, and that is exactly the
/// moment they change radar. So the test is about the ground on screen, not
/// about the distance between the two sites.
///
/// Two things qualify as being served, and they are different questions:
///
/// 1. The new radar reaches the MIDDLE of the screen. That is the ground under
///    the analyst's eye, and if the new radar covers it the picture is simply
///    redrawn from a better angle.
/// 2. The new antenna is ON the screen. Then the analyst can already see where
///    the radar is and how much of their view it covers, and moving the view
///    would take something away without giving anything back. This is the
///    clause that leaves a continent-wide view completely alone.
///
/// It is deliberately NOT enough for the coverage to graze a corner of the
/// viewport. That was the first rule written here -- the gap from the antenna
/// to the nearest point of the viewport, against the surveillance range -- and
/// it is wrong by a wide margin, because at radar working scale a viewport is
/// hundreds of kilometres of margin around the point the analyst cares about.
/// Measured over the workstation's own site catalogue (208 sites), an analyst
/// centred on their antenna at the default 0.35 km per point in the default
/// 1500x950 window: of the 2835 ordered site pairs that rule holds, 1529 leave
/// the middle of the screen outside the new radar's surveillance range
/// entirely, the worst of them KDGX to KTLX at 755 km -- a blank screen and a
/// four-screen-width drag to find the data. Under the rule above that count is
/// zero at that window size, and the pairs it still holds are the ones where
/// the analyst can see the antenna.
fn decide_across_site_change(
    previous: Camera2D,
    held_center: Option<WorldPoint>,
    viewport: Option<ViewportMetrics>,
) -> SiteChangeDecision {
    let previous = previous.sanitized();
    // A geodesic that did not converge is not a place. Substituting one would
    // put the analyst somewhere real and wrong, which is worse than the
    // antenna, where at least the screen agrees with the title bar.
    let Some(held) =
        held_center.filter(|held| held.east_km.is_finite() && held.north_km.is_finite())
    else {
        return SiteChangeDecision::Recentre;
    };
    let held_camera = Camera2D {
        center_east_km: held.east_km,
        center_north_km: held.north_km,
        ..previous
    };
    let reaches_the_middle = held.east_km.hypot(held.north_km) <= NEXRAD_SURVEILLANCE_RANGE_KM;
    let antenna_on_screen = antenna_gap_km(held_camera, viewport) == 0.0;
    if reaches_the_middle || antenna_on_screen {
        SiteChangeDecision::HoldGeographic
    } else {
        SiteChangeDecision::Recentre
    }
}

/// Ground distance in kilometres from the antenna -- the world origin -- to
/// the nearest point of the viewport, zero when the antenna is on screen.
///
/// Only the zero matters to [`decide_across_site_change`]; the magnitude is
/// kept because it is what makes the zero exact, and because it is the natural
/// thing to assert on in a test.
///
/// The viewport is a rectangle in SCREEN space, so the antenna is rotated into
/// screen-aligned kilometres and clamped there. The forward rotation is the
/// one [`Camera2D::world_to_screen`] applies, restated in kilometres so that a
/// pane's pixel density cannot enter a decision about geography.
fn antenna_gap_km(camera: Camera2D, viewport: Option<ViewportMetrics>) -> f64 {
    let camera = camera.sanitized();
    let (sin, cos) = camera.rotation_rad.sin_cos();
    let east = -camera.center_east_km;
    let north = -camera.center_north_km;
    let across_km = f64::from(cos) * east + f64::from(sin) * north;
    let down_km = f64::from(sin) * east - f64::from(cos) * north;
    let (half_width_km, half_height_km) = match viewport {
        Some(viewport) => {
            let viewport = viewport.sanitized();
            let km_per_point = f64::from(camera.km_per_point);
            (
                0.5 * f64::from(viewport.width_points) * km_per_point,
                0.5 * f64::from(viewport.height_points) * km_per_point,
            )
        }
        None => (0.0, 0.0),
    };
    (across_km.abs() - half_width_km)
        .max(0.0)
        .hypot((down_km.abs() - half_height_km).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_KM_PER_POINT, MAX_KM_PER_POINT};

    #[test]
    fn layout_limits_active_panes() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        assert!(workspace.set_active(PANE_3));
        workspace.set_layout(PaneLayout::One);
        assert_eq!(workspace.active_pane, PANE_0);
        assert!(!workspace.set_active(PANE_1));
    }

    #[test]
    fn camera_propagates_only_inside_link_group() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.pane_mut(PANE_2).links.camera = Some(1);
        workspace.pane_mut(PANE_3).links.camera = None;
        let camera = Camera2D {
            center_east_km: 70.0,
            center_north_km: -20.0,
            ..Camera2D::default()
        };
        let changed = workspace.apply_camera_from(PANE_0, camera);
        assert_eq!(changed, vec![PANE_0, PANE_1]);
        assert_eq!(workspace.pane(PANE_0).camera, camera);
        assert_eq!(workspace.pane(PANE_1).camera, camera);
        assert_ne!(workspace.pane(PANE_2).camera, camera);
        assert_ne!(workspace.pane(PANE_3).camera, camera);
    }

    #[test]
    fn product_links_are_independent_from_camera_links() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.pane_mut(PANE_0).links.product = Some(4);
        workspace.pane_mut(PANE_2).links.product = Some(4);
        let changed = workspace.apply_product_from(PANE_0, ProductId("VEL".to_owned()));
        assert_eq!(changed, vec![PANE_0, PANE_2]);
        assert_eq!(workspace.pane(PANE_1).product.0, "REF");
    }

    #[test]
    fn active_pane_cycles_within_visible_layout() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::TwoVertical);
        assert_eq!(workspace.cycle_active(1), PANE_1);
        assert_eq!(workspace.cycle_active(1), PANE_0);
        assert_eq!(workspace.cycle_active(-1), PANE_1);
    }

    // ----------------------------------------------------------------------
    // Site-change policy.
    //
    // Every number below is real. The anchors are the site latitude and
    // longitude carried in the `RVOL` block of message 31 of Level II volumes
    // sitting in the workstation's own live cache, read on 2026-08-18:
    //
    //     KMKX 42.967899322509766  -88.55066680908203
    //     KLOT 41.60444259643555   -88.08444213867188
    //     KARX 43.822776794433594  -91.19110870361328
    //     KTLX 35.3333625793457    -97.27776336669922
    //     KEAX 38.81024932861328   -94.26447296142578
    //     TOKC 35.2760009765625    -97.51000213623047
    //
    // The reprojections are what `map_scene::RadarProjection` -- the geodesic
    // azimuthal-equidistant projection the application actually anchors with
    // -- returns for those anchors, cross-checked against PROJ 9 through
    // pyproj 3.7.2 (Karney's geodesic) and agreeing to better than a
    // millimetre on every pair used here.
    // ----------------------------------------------------------------------

    /// One measured reprojection: a camera centre in the OLD anchor's world
    /// frame, and the same ground in the NEW anchor's world frame.
    type Reprojection = ((f64, f64), (f64, f64));

    /// KMKX to KLOT: 156.256 km between the antennas, coverage heavily
    /// overlapping. `(0, 0)` is the Milwaukee antenna itself; `(40, -60)` is a
    /// storm south-east of it.
    const KMKX_TO_KLOT: &[Reprojection] = &[
        ((40.0, -60.0), (1.631_773, 91.336_141)),
        ((0.0, 0.0), (-38.039_582, 151.554_885)),
    ];

    /// KTLX to TOKC: the terminal radar over the same city, 22.060 km away.
    const KTLX_TO_TOKC: &[Reprojection] = &[((15.0, -25.0), (36.172_705, -18.576_150))];

    /// KTLX to KEAX: 469.712 km between the antennas, which puts the Norman
    /// antenna just outside Pleasant Hill's surveillance sweep and a storm
    /// north-east of Norman comfortably inside it.
    const KTLX_TO_KEAX: &[Reprojection] = &[
        ((0.0, 0.0), (-274.078_999, -381.457_276)),
        ((120.0, 180.0), (-148.430_324, -205.353_865)),
    ];

    /// KTLX to KMKX: 1133.371 km apart, no shared coverage whatsoever.
    const KTLX_TO_KMKX: &[Reprojection] = &[
        ((0.0, 0.0), (-794.544_771, -808.225_853)),
        ((60.0, 90.0), (-726.182_746, -724.394_549)),
    ];

    // The two pairs below are the extremes, and their anchors come from the
    // workstation's own site catalogue (`radar-sites.tsv`, 208 sites) rather
    // than from a cached volume, because neither TMKE nor KVTX has one:
    //
    //     KMKX 42.967769622802734  -88.55055236816406
    //     TMKE 42.819000244140625  -88.0459976196289
    //     KTLX 35.33304977416992   -97.27774810791016
    //     KVTX 34.411659240722656  -119.17859649658203
    //
    // The catalogue and the volumes disagree by about 15 m on the sites that
    // appear in both, which is four orders of magnitude below anything this
    // policy decides on.

    /// KMKX to TMKE: 44.403 km, the Milwaukee terminal radar. The near case.
    const KMKX_TO_TMKE: &[Reprojection] = &[
        ((0.0, 0.0), (-41.163_180, 16.650_193)),
        ((25.0, -15.0), (-16.253_524, 1.500_626)),
    ];

    /// KTLX to KVTX: 2000.925 km, Norman to Los Angeles. The far case, and far
    /// enough that no scale the camera can reach puts both on one screen.
    const KTLX_TO_KVTX: &[Reprojection] = &[
        ((0.0, 0.0), (1_975.188_228, 319.895_759)),
        ((60.0, 90.0), (2_013.658_707, 422.429_518)),
    ];

    /// KDGX to KTLX: 755.171 km, Jackson to Norman. Anchors from the
    /// catalogue: KDGX 32.279991149902344 -89.98444366455078.
    ///
    /// This is the worst real pair in the catalogue for the viewport rule that
    /// used to live here: at the default window and scale it held, and left
    /// the analyst 755 km from the radar they had just chosen.
    const KDGX_TO_KTLX: &[Reprojection] = &[((0.0, 0.0), (686.812_522, -313.962_059))];

    /// PGUA to KTLX: 11546.372 km, Andersen AFB on Guam to Norman, across the
    /// dateline and most of a hemisphere. Anchor from the catalogue:
    /// PGUA 13.455829620361328 144.81112670898438.
    const PGUA_TO_KTLX: &[Reprojection] = &[
        ((0.0, 0.0), (-10_202.793_114, 5_405.711_075)),
        ((120.0, -80.0), (-10_290.173_001, 5_158.136_029)),
    ];

    /// A reprojection that answers only for the measured points, so a test
    /// cannot pass by feeding the policy a camera nobody measured.
    fn measured(pairs: &'static [Reprojection]) -> impl FnMut(WorldPoint) -> Option<WorldPoint> {
        move |world| {
            for ((east_km, north_km), (to_east_km, to_north_km)) in pairs {
                if (world.east_km - east_km).abs() < 1e-9
                    && (world.north_km - north_km).abs() < 1e-9
                {
                    return Some(WorldPoint::new(*to_east_km, *to_north_km));
                }
            }
            panic!("no measured reprojection for {world:?}");
        }
    }

    fn viewport(width_points: f32, height_points: f32) -> Option<ViewportMetrics> {
        Some(ViewportMetrics {
            width_points,
            height_points,
            pixels_per_point: 1.0,
        })
    }

    fn everywhere(viewport: Option<ViewportMetrics>) -> [Option<ViewportMetrics>; MAX_PANES] {
        [viewport; MAX_PANES]
    }

    fn camera_at(east_km: f64, north_km: f64, km_per_point: f32) -> Camera2D {
        Camera2D {
            center_east_km: east_km,
            center_north_km: north_km,
            km_per_point,
            rotation_rad: 0.0,
        }
    }

    fn assert_centre(camera: Camera2D, east_km: f64, north_km: f64) {
        assert!(
            (camera.center_east_km - east_km).abs() < 1e-6
                && (camera.center_north_km - north_km).abs() < 1e-6,
            "camera centred at ({}, {}), expected ({east_km}, {north_km})",
            camera.center_east_km,
            camera.center_north_km
        );
    }

    /// The complaint this policy exists for: changing radar moved the picture.
    /// Radar-local kilometres name a different place once the anchor moves, so
    /// an untouched camera teleports the analyst 156.1 km down the road.
    #[test]
    fn holding_the_radar_local_camera_teleports_the_analyst_across_the_ground() {
        let previous = camera_at(40.0, -60.0, 0.35);
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, previous);
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KMKX_TO_KLOT));
        let held = workspace.pane(PANE_0).camera;
        let moved = (held.center_east_km - previous.center_east_km)
            .hypot(held.center_north_km - previous.center_north_km);
        assert!(
            (moved - 156.124).abs() < 0.01,
            "the fix moves the camera {moved} km in radar-local kilometres; \
             leaving it alone is what moved the GROUND by that much"
        );
    }

    /// KMKX to KLOT with the analyst on a storm: the same ground stays on
    /// screen, at the same scale and orientation.
    #[test]
    fn an_overlapping_radar_keeps_the_analyst_on_the_same_ground() {
        // Rotated, because a screen the analyst has turned is exactly the kind
        // of thing a site change has no business straightening.
        let previous = Camera2D {
            rotation_rad: 0.5,
            ..camera_at(40.0, -60.0, 0.35)
        };
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, previous);
        let changed = workspace
            .apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KMKX_TO_KLOT));
        assert_eq!(changed, vec![PANE_0, PANE_1, PANE_2, PANE_3]);
        let camera = workspace.pane(PANE_0).camera;
        assert_centre(camera, 1.631_773, 91.336_141);
        assert_eq!(camera.km_per_point, previous.km_per_point);
        assert_eq!(camera.rotation_rad, previous.rotation_rad);
    }

    /// The decision itself, stated plainly on real pairs rather than inferred
    /// from where the camera ended up.
    #[test]
    fn the_decision_is_whether_the_new_radar_reaches_the_view() {
        let pane = viewport(1200.0, 800.0);
        let downrange = decide_across_site_change(
            camera_at(120.0, 180.0, 0.35),
            Some(WorldPoint::new(-148.430_324, -205.353_865)),
            pane,
        );
        assert_eq!(downrange, SiteChangeDecision::HoldGeographic);

        let across_the_country = decide_across_site_change(
            camera_at(60.0, 90.0, 0.35),
            Some(WorldPoint::new(-726.182_746, -724.394_549)),
            pane,
        );
        assert_eq!(across_the_country, SiteChangeDecision::Recentre);

        let unanswerable = decide_across_site_change(camera_at(60.0, 90.0, 0.35), None, pane);
        assert_eq!(unanswerable, SiteChangeDecision::Recentre);
    }

    /// The defect the first rule here shipped with, stated on the pair that
    /// shows it worst on real data.
    ///
    /// KTLX is 469.712 km from KEAX -- 9.7 km outside the surveillance sweep.
    /// An analyst parked on the Norman antenna in the application's own
    /// default window (1500x950 points, a 1484x820 point pane) at the default
    /// 0.35 km per point is looking at a 519x287 km rectangle, so the nearest
    /// corner of that rectangle is only 238 km from Pleasant Hill. A rule that
    /// asks whether coverage reaches the VIEWPORT therefore holds, and hands
    /// the analyst a screen whose middle Pleasant Hill cannot see at all.
    ///
    /// The same arithmetic over the 208-site catalogue turns 1529 of that
    /// rule's 2835 holds into blank screens. This is the regression test for
    /// all of them.
    #[test]
    fn a_view_the_new_radar_cannot_see_the_middle_of_is_not_held() {
        let held = WorldPoint::new(-274.078_999, -381.457_276);
        let default_pane = viewport(1484.0, 820.0);
        assert!(
            antenna_gap_km(
                Camera2D {
                    center_east_km: held.east_km,
                    center_north_km: held.north_km,
                    ..camera_at(0.0, 0.0, 0.35)
                },
                default_pane,
            ) < NEXRAD_SURVEILLANCE_RANGE_KM,
            "the pair has to be one the viewport rule would have held, or this \
             test is not about the defect"
        );
        assert!(held.east_km.hypot(held.north_km) > NEXRAD_SURVEILLANCE_RANGE_KM);
        assert_eq!(
            decide_across_site_change(camera_at(0.0, 0.0, 0.35), Some(held), default_pane),
            SiteChangeDecision::Recentre,
        );
    }

    /// The second clause, and the reason the viewport is still consulted at
    /// all: a view wide enough to contain the new antenna is left alone even
    /// though its middle is far outside the surveillance sweep. Recentring
    /// here would move a picture that already shows the analyst everything the
    /// decision is about.
    #[test]
    fn a_view_with_the_new_antenna_on_screen_is_left_alone() {
        let held = WorldPoint::new(-794.544_771, -808.225_853);
        assert!(held.east_km.hypot(held.north_km) > NEXRAD_SURVEILLANCE_RANGE_KM);
        assert_eq!(
            decide_across_site_change(
                camera_at(0.0, 0.0, 4.0),
                Some(held),
                viewport(1200.0, 800.0),
            ),
            SiteChangeDecision::HoldGeographic,
        );
        // The same ground at working scale: the antenna is off screen and the
        // middle is out of range, so there is nothing left to hold.
        assert_eq!(
            decide_across_site_change(
                camera_at(0.0, 0.0, 0.35),
                Some(held),
                viewport(1200.0, 800.0),
            ),
            SiteChangeDecision::Recentre,
        );
    }

    /// The case that makes radar-local holding indefensible: KTLX and TOKC
    /// look at the same city from 22 km apart, so the old behaviour slid the
    /// screen 22.1 km at the exact moment the analyst asked for a closer look.
    #[test]
    fn swapping_to_the_terminal_radar_over_the_same_city_holds_the_storm() {
        let previous = camera_at(15.0, -25.0, 0.1);
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, previous);
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KTLX_TO_TOKC));
        assert_centre(workspace.pane(PANE_0).camera, 36.172_705, -18.576_150);
    }

    /// Following a storm downrange is when radar gets changed, and the storm
    /// is what has to stay on screen.
    #[test]
    fn a_storm_downrange_of_the_old_radar_survives_the_change() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(120.0, 180.0, 0.35));
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KTLX_TO_KEAX));
        assert_centre(workspace.pane(PANE_0).camera, -148.430_324, -205.353_865);
    }

    /// KTLX to KMKX is 1133 km. Holding the ground would leave the analyst
    /// staring at Illinois farmland with a radar in Wisconsin, so the antenna
    /// goes in the middle -- but the scale and rotation are still theirs.
    #[test]
    fn a_radar_that_reaches_none_of_the_view_takes_the_middle_of_the_screen() {
        let previous = Camera2D {
            rotation_rad: 0.5,
            ..camera_at(60.0, 90.0, 0.35)
        };
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, previous);
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KTLX_TO_KMKX));
        let camera = workspace.pane(PANE_0).camera;
        assert_centre(camera, 0.0, 0.0);
        assert_eq!(camera.km_per_point, previous.km_per_point);
        assert_eq!(camera.rotation_rad, previous.rotation_rad);
    }

    /// The same 1133 km change, seen from a continent-wide view: the antenna
    /// is already on screen, so nothing whatsoever moves. Recentring here --
    /// or snapping the scale to the new radar -- would be the "locks into
    /// place" behaviour with extra steps.
    #[test]
    fn a_continent_wide_view_is_not_dragged_by_a_site_change() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 4.0));
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KTLX_TO_KMKX));
        assert_centre(workspace.pane(PANE_0).camera, -794.544_771, -808.225_853);
        assert_eq!(workspace.pane(PANE_0).camera.km_per_point, 4.0);
    }

    /// The threshold, straddled on one real pair by the configuration the
    /// application actually produces. KTLX is 469.712 km from KEAX, just
    /// outside the sweep, so the middle of the screen is not served either
    /// way. A pane laid out at 1200x800 points and 4 km per point is 4800 km
    /// across and has the Pleasant Hill antenna on it, so it holds. A pane
    /// that has never been laid out is measured as a point, has nothing on it,
    /// and recentres. That is not a contrivance: in the One and Two layouts
    /// the hidden panes have exactly that viewport.
    #[test]
    fn the_hold_decision_straddles_on_whether_the_antenna_is_on_screen() {
        let mut inside = WorkspaceState::default();
        inside.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 4.0));
        inside.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KTLX_TO_KEAX));
        assert_centre(inside.pane(PANE_0).camera, -274.078_999, -381.457_276);

        let mut outside = WorkspaceState::default();
        outside.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 4.0));
        outside.apply_site_change(&everywhere(None), measured(KTLX_TO_KEAX));
        assert_centre(outside.pane(PANE_0).camera, 0.0, 0.0);
    }

    /// Panes that share a camera must not come out of a site change straddling
    /// the threshold. These two straddle it on their own -- the laid-out one
    /// holds, the never-laid-out one recentres -- and the group takes the
    /// active pane's decision.
    #[test]
    fn camera_linked_panes_take_one_decision_across_a_site_change() {
        for (active, expected_east_km, expected_north_km) in
            [(PANE_0, -274.078_999, -381.457_276), (PANE_1, 0.0, 0.0)]
        {
            let mut workspace = WorkspaceState::default();
            workspace.set_layout(PaneLayout::Four);
            assert!(workspace.set_active(active));
            workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 4.0));
            let mut viewports = everywhere(viewport(1200.0, 800.0));
            viewports[PANE_1.index()] = None;
            workspace.apply_site_change(&viewports, measured(KTLX_TO_KEAX));
            assert_centre(
                workspace.pane(PANE_0).camera,
                expected_east_km,
                expected_north_km,
            );
            assert_eq!(
                workspace.pane(PANE_0).camera,
                workspace.pane(PANE_1).camera,
                "active {active:?} left a camera link group holding two cameras"
            );
        }
    }

    /// A pane the analyst has unlinked answers for itself, and does not drag
    /// the linked group with it.
    #[test]
    fn an_unlinked_pane_decides_alone() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 4.0));
        workspace.pane_mut(PANE_1).links.camera = None;
        let mut viewports = everywhere(viewport(1200.0, 800.0));
        viewports[PANE_1.index()] = None;
        workspace.apply_site_change(&viewports, measured(KTLX_TO_KEAX));
        assert_centre(workspace.pane(PANE_0).camera, -274.078_999, -381.457_276);
        assert_centre(workspace.pane(PANE_1).camera, 0.0, 0.0);
        assert_centre(workspace.pane(PANE_2).camera, -274.078_999, -381.457_276);
    }

    /// A camera link group that was ALREADY holding two cameras keeps them.
    ///
    /// Two clicks on "Link cameras" produce exactly this: unlink, drag the
    /// panes to two different storms, link again -- the button rewrites
    /// `links.camera` and nothing rewrites the cameras. A site change that
    /// copied the active pane's camera over the rest of its group would
    /// teleport the other pane off its storm, which is the complaint this file
    /// exists to answer. One DECISION is shared; the ground is not.
    #[test]
    fn a_site_change_does_not_snap_a_divergent_camera_group_together() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.pane_mut(PANE_0).camera = camera_at(40.0, -60.0, 0.35);
        workspace.pane_mut(PANE_1).camera = camera_at(0.0, 0.0, 0.35);
        assert_eq!(workspace.pane(PANE_0).links.camera, Some(0));
        assert_eq!(workspace.pane(PANE_1).links.camera, Some(0));

        workspace.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(KMKX_TO_KLOT));

        assert_centre(workspace.pane(PANE_0).camera, 1.631_773, 91.336_141);
        assert_centre(workspace.pane(PANE_1).camera, -38.039_582, 151.554_885);
    }

    /// The follower of a holdable group whose OWN centre the geodesic cannot
    /// answer. Only a group that was already carrying two cameras can reach
    /// this -- the leader's centre converged, so a shared one would have too --
    /// and the answer has to be the antenna, not a NaN centre written into
    /// serializable intent that every later camera sum would spread.
    ///
    /// This case is why the non-finite guard is applied a second time where
    /// the camera is written and not only where the decision is taken: with
    /// the write-side guard removed, every other test in this file still
    /// passed, because the decision-side guard means no other test ever
    /// reaches the hold branch with a bad point.
    #[test]
    fn a_follower_whose_own_geodesic_fails_takes_the_antenna_not_a_nan() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.pane_mut(PANE_0).camera = camera_at(40.0, -60.0, 0.35);
        workspace.pane_mut(PANE_1).camera = camera_at(10.0, 10.0, 0.35);
        workspace.apply_site_change(&everywhere(viewport(1484.0, 820.0)), |world| {
            if world.east_km == 10.0 {
                Some(WorldPoint::new(f64::INFINITY, f64::NAN))
            } else {
                Some(WorldPoint::new(1.631_773, 91.336_141))
            }
        });
        assert_centre(workspace.pane(PANE_0).camera, 1.631_773, 91.336_141);
        assert_centre(workspace.pane(PANE_1).camera, 0.0, 0.0);
        assert!(workspace.pane(PANE_1).camera.center_east_km.is_finite());
    }

    /// A geodesic that did not converge is not a place to put the analyst.
    #[test]
    fn a_reprojection_that_cannot_answer_recentres_rather_than_inventing_a_place() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(40.0, -60.0, 0.35));
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), |_| None);
        assert_centre(workspace.pane(PANE_0).camera, 0.0, 0.0);
    }

    /// A non-finite reprojection is refused for the same reason, and cannot
    /// poison the camera with a NaN centre that later arithmetic spreads. It
    /// is refused at BOTH ends: a decision cannot be taken from a non-place,
    /// and a camera cannot be written from one. `Camera2D::sanitized` turns a
    /// NaN centre into the origin, so without the first of those the viewport
    /// test would be handed a camera sitting exactly on the antenna and would
    /// happily answer "hold".
    #[test]
    fn a_non_finite_reprojection_cannot_reach_the_camera() {
        let poison = Some(WorldPoint::new(f64::NAN, 12.0));
        assert_eq!(
            decide_across_site_change(
                camera_at(40.0, -60.0, 0.35),
                poison,
                viewport(1200.0, 800.0)
            ),
            SiteChangeDecision::Recentre,
        );
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(40.0, -60.0, 0.35));
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), |_| poison);
        assert_centre(workspace.pane(PANE_0).camera, 0.0, 0.0);
    }

    /// The viewport test is a rectangle in SCREEN space, so a screen the
    /// analyst has turned changes which ground is on it. On the real KTLX to
    /// KEAX pair -- 469.712 km, so the middle of the screen is out of range
    /// either way and only the on-screen clause can decide -- a 1484x820 point
    /// pane at 0.7 km per point has the Pleasant Hill antenna off the bottom
    /// of the screen unrotated and on the screen at 0.6 rad.
    ///
    /// The gap is also checked against `Camera2D::world_to_screen`, which is
    /// the transform that actually puts the antenna on the glass: the two must
    /// agree about what "on screen" means or the rule is deciding about a
    /// picture nobody is looking at.
    #[test]
    fn the_on_screen_test_follows_the_rotation_of_the_screen() {
        let held = WorldPoint::new(-274.078_999, -381.457_276);
        let pane = viewport(1484.0, 820.0).expect("viewport");
        for (rotation_rad, expected) in [
            (0.0, SiteChangeDecision::Recentre),
            (0.6, SiteChangeDecision::HoldGeographic),
        ] {
            let previous = Camera2D {
                rotation_rad,
                ..camera_at(0.0, 0.0, 0.7)
            };
            assert_eq!(
                decide_across_site_change(previous, Some(held), Some(pane)),
                expected,
                "rotation {rotation_rad} rad"
            );
            let held_camera = Camera2D {
                center_east_km: held.east_km,
                center_north_km: held.north_km,
                ..previous
            };
            let antenna = held_camera.world_to_screen(WorldPoint::ORIGIN, pane);
            let on_glass = (0.0..=pane.width_points).contains(&antenna.x)
                && (0.0..=pane.height_points).contains(&antenna.y);
            assert_eq!(
                antenna_gap_km(held_camera, Some(pane)) == 0.0,
                on_glass,
                "rotation {rotation_rad} rad: the gap and the render transform \
                 disagree about whether the antenna is on screen"
            );
        }
    }

    /// Changing radar is not permission to rearrange the workspace.
    #[test]
    fn a_site_change_leaves_the_layout_and_the_active_pane_alone() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        assert!(workspace.set_active(PANE_2));
        workspace.apply_camera_from(PANE_0, camera_at(40.0, -60.0, 0.35));
        workspace.apply_site_change(&everywhere(viewport(1200.0, 800.0)), measured(KMKX_TO_KLOT));
        assert_eq!(workspace.layout, PaneLayout::Four);
        assert_eq!(workspace.active_pane, PANE_2);
    }

    /// The worst real pair the shipped viewport rule held, driven end to end.
    /// In the application's own default window and default scale it left the
    /// analyst centred 755.171 km from Jackson -- 295 km beyond the far edge
    /// of the surveillance sweep, on a screen with no data on it at all, and
    /// about four screen widths of dragging from any.
    #[test]
    fn the_worst_real_pair_of_the_old_rule_now_takes_the_middle_of_the_screen() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, DEFAULT_KM_PER_POINT));
        workspace.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(KDGX_TO_KTLX));
        assert_centre(workspace.pane(PANE_0).camera, 0.0, 0.0);
    }

    /// Across the dateline and most of a hemisphere: Guam to Norman. At
    /// working scale the antenna goes in the middle, and at the furthest the
    /// camera can zoom out -- where a 1484x820 point pane is 74200 km across
    /// and holds the whole earth -- the ground on screen is held instead.
    /// Nothing in this file has an opinion about longitude; it sees only the
    /// kilometres the projection hands back, which is why one pair like this
    /// is enough to pin it.
    #[test]
    fn a_site_change_across_the_dateline_is_decided_the_same_way_as_any_other() {
        let mut working = WorkspaceState::default();
        working.apply_camera_from(PANE_0, camera_at(120.0, -80.0, DEFAULT_KM_PER_POINT));
        working.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(PGUA_TO_KTLX));
        assert_centre(working.pane(PANE_0).camera, 0.0, 0.0);

        let mut whole_earth = WorkspaceState::default();
        whole_earth.apply_camera_from(PANE_0, camera_at(0.0, 0.0, MAX_KM_PER_POINT));
        whole_earth.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(PGUA_TO_KTLX));
        assert_centre(
            whole_earth.pane(PANE_0).camera,
            -10_202.793_114,
            5_405.711_075,
        );
    }

    /// The near extreme: 44.4 km, Milwaukee's WSR-88D to Milwaukee's terminal
    /// radar. Nothing about the view changes but the numbers behind it.
    #[test]
    fn a_radar_forty_kilometres_away_holds_the_ground() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(25.0, -15.0, 0.35));
        workspace.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(KMKX_TO_TMKE));
        assert_centre(workspace.pane(PANE_0).camera, -16.253_524, 1.500_626);
    }

    /// The far extreme: 2000.9 km, Norman to Los Angeles. At working scale the
    /// antenna goes in the middle -- holding would leave the analyst on
    /// Oklahoma ground with a Californian radar, which is the case the
    /// recentring branch exists for.
    #[test]
    fn a_radar_two_thousand_kilometres_away_takes_the_middle_of_the_screen() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(60.0, 90.0, 0.35));
        workspace.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(KTLX_TO_KVTX));
        assert_centre(workspace.pane(PANE_0).camera, 0.0, 0.0);
        assert_eq!(workspace.pane(PANE_0).camera.km_per_point, 0.35);
    }

    /// The same 2000.9 km change seen from the furthest the camera can zoom
    /// out. Both antennas are on one screen, so the ground on screen is held
    /// and nothing appears to move; the scale is what decides, and it is the
    /// analyst's.
    #[test]
    fn the_far_extreme_holds_its_ground_when_one_screen_holds_both_radars() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, MAX_KM_PER_POINT));
        workspace.apply_site_change(&everywhere(viewport(1484.0, 820.0)), measured(KTLX_TO_KVTX));
        assert_centre(workspace.pane(PANE_0).camera, 1_975.188_228, 319.895_759);
        assert_eq!(workspace.pane(PANE_0).camera.km_per_point, MAX_KM_PER_POINT);
    }

    /// A pane that has never been laid out is measured as a point, so the
    /// KTLX antenna's 469.712 km from Pleasant Hill puts it on the antenna.
    #[test]
    fn a_pane_with_no_viewport_is_measured_as_a_point() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 0.05));
        workspace.apply_site_change(&everywhere(None), measured(KTLX_TO_KEAX));
        assert_centre(workspace.pane(PANE_0).camera, 0.0, 0.0);
    }

    #[test]
    fn centring_on_the_anchor_keeps_rotation_and_reports_only_what_moved() {
        let mut workspace = WorkspaceState::default();
        workspace.apply_camera_from(
            PANE_0,
            Camera2D {
                rotation_rad: 0.5,
                ..camera_at(40.0, -60.0, 0.35)
            },
        );
        let changed = workspace.centre_on_anchor(4.0);
        assert_eq!(changed, vec![PANE_0, PANE_1, PANE_2, PANE_3]);
        let camera = workspace.pane(PANE_0).camera;
        assert_centre(camera, 0.0, 0.0);
        assert_eq!(camera.km_per_point, 4.0);
        assert_eq!(camera.rotation_rad, 0.5);
        assert!(workspace.centre_on_anchor(4.0).is_empty());
    }

    /// The ordinary opening: nobody has touched the overview, so the first
    /// radar arrives in the middle of the screen at working scale.
    #[test]
    fn an_untouched_overview_hands_over_at_working_scale() {
        let mut workspace = WorkspaceState::default();
        workspace.centre_on_anchor(4.0);
        let changed = workspace.leave_overview(4.0, DEFAULT_KM_PER_POINT);
        assert_eq!(changed, vec![PANE_0, PANE_1, PANE_2, PANE_3]);
        let camera = workspace.pane(PANE_0).camera;
        assert_centre(camera, 0.0, 0.0);
        assert_eq!(camera.km_per_point, DEFAULT_KM_PER_POINT);
    }

    /// A camera stated before the first volume is the analyst's, and survives
    /// it untouched. `--zoom` and `--center` exist so a particular view can be
    /// photographed on real data; the application used to reset every pane to
    /// `Camera2D::default()` on the first volume, which meant neither flag
    /// could ever be photographed, because nothing is on screen until one
    /// arrives.
    ///
    /// The centre is kept in radar-local kilometres rather than carried across
    /// from the placeholder anchor, because that is what `--center 40,-60`
    /// means: 40 km east and 60 km north of the radar being loaded.
    #[test]
    fn a_camera_stated_before_the_first_volume_survives_it() {
        let mut workspace = WorkspaceState::default();
        workspace.centre_on_anchor(4.0);
        // What `--zoom 0.35 --center 40,-60` leaves behind.
        workspace.apply_camera_from(PANE_0, camera_at(40.0, -60.0, 0.35));
        let changed = workspace.leave_overview(4.0, DEFAULT_KM_PER_POINT);
        assert!(changed.is_empty(), "the hand-over moved a stated camera");
        assert_centre(workspace.pane(PANE_0).camera, 40.0, -60.0);
        assert_eq!(workspace.pane(PANE_0).camera.km_per_point, 0.35);
    }

    /// `--zoom` alone leaves the centre on the anchor, so the hand-over cannot
    /// be decided on the centre alone.
    #[test]
    fn a_stated_scale_alone_still_counts_as_a_camera() {
        let mut workspace = WorkspaceState::default();
        workspace.centre_on_anchor(4.0);
        workspace.apply_camera_from(PANE_0, camera_at(0.0, 0.0, 0.05));
        workspace.leave_overview(4.0, DEFAULT_KM_PER_POINT);
        assert_eq!(workspace.pane(PANE_0).camera.km_per_point, 0.05);
    }

    /// One pane the analyst nudged must not keep the others at continent
    /// scale: the hand-over answers per pane, so a pane nobody touched still
    /// arrives at the radar.
    #[test]
    fn one_stated_pane_does_not_hold_the_rest_at_overview_scale() {
        let mut workspace = WorkspaceState::default();
        workspace.set_layout(PaneLayout::Four);
        workspace.centre_on_anchor(4.0);
        workspace.pane_mut(PANE_1).links.camera = None;
        workspace.apply_camera_from(PANE_1, camera_at(300.0, 200.0, 4.0));
        let changed = workspace.leave_overview(4.0, DEFAULT_KM_PER_POINT);
        assert_eq!(changed, vec![PANE_0, PANE_2, PANE_3]);
        assert_eq!(
            workspace.pane(PANE_0).camera.km_per_point,
            DEFAULT_KM_PER_POINT
        );
        assert_centre(workspace.pane(PANE_1).camera, 300.0, 200.0);
        assert_eq!(workspace.pane(PANE_1).camera.km_per_point, 4.0);
    }

    /// Rotation is an orientation, not a place. An overview the analyst turned
    /// and did not move still hands over to working scale, keeping the turn.
    #[test]
    fn a_turned_overview_still_hands_over_and_keeps_the_turn() {
        let mut workspace = WorkspaceState::default();
        workspace.centre_on_anchor(4.0);
        workspace.apply_camera_from(
            PANE_0,
            Camera2D {
                rotation_rad: 0.5,
                ..camera_at(0.0, 0.0, 4.0)
            },
        );
        workspace.leave_overview(4.0, DEFAULT_KM_PER_POINT);
        let camera = workspace.pane(PANE_0).camera;
        assert_centre(camera, 0.0, 0.0);
        assert_eq!(camera.km_per_point, DEFAULT_KM_PER_POINT);
        assert_eq!(camera.rotation_rad, 0.5);
    }

    /// The scale is clamped on the way in, so a caller cannot install a camera
    /// the rest of the view code would have to defend against.
    #[test]
    fn centring_on_the_anchor_sanitizes_the_scale() {
        let mut workspace = WorkspaceState::default();
        workspace.centre_on_anchor(f32::NAN);
        assert_eq!(
            workspace.pane(PANE_0).camera.km_per_point,
            DEFAULT_KM_PER_POINT
        );
        workspace.centre_on_anchor(1e9);
        assert_eq!(workspace.pane(PANE_0).camera.km_per_point, MAX_KM_PER_POINT);
    }
}
