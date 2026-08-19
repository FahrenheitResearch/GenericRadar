//! Bounded label placement.
//!
//! Every candidate is already projected into world kilometres by the geometry
//! build, so placement never touches geographic coordinates. The pass is hard
//! bounded in candidates inspected and labels accepted, and it is deterministic:
//! the same camera and geometry always produce the same labels in the same
//! order, so text does not shimmer as the view moves.

use analyst_runtime::{Camera2D, ScreenPoint, ViewportMetrics};
use basemap_tiles::TileProvider;

use crate::dataset::LabelClass;
use crate::geometry::{MapGeometry, ProjectedLabel};
use crate::projection::globe;

/// Hard ceiling on candidates examined per pane per frame.
pub const MAX_CANDIDATES_INSPECTED: usize = 4_000;
/// Hard ceiling on labels drawn per pane.
pub const MAX_LABELS_PLACED: usize = 64;
/// Approximate glyph box used for overlap rejection, in screen points.
const CHARACTER_WIDTH_POINTS: f32 = 6.0;
const LINE_HEIGHT_POINTS: f32 = 13.0;
/// Padding around a placed label so text does not crowd.
const PADDING_POINTS: f32 = 3.0;

/// A label that survived placement, in pane-local screen points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedLabel {
    pub name: &'static str,
    pub position: ScreenPoint,
    pub class: LabelClass,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlacementMetrics {
    pub inspected: usize,
    pub placed: usize,
    pub rejected_offscreen: usize,
    pub rejected_overlap: usize,
    /// Candidates on the far side of the globe's limb. Zero at every scale an
    /// analyst works at, because the limb only exists once the view is a
    /// globe.
    pub rejected_behind_limb: usize,
    pub budget_exhausted: bool,
    /// The whole layer was switched off because the raster underneath already
    /// carries place names. See [`provider_draws_its_own_labels`].
    pub suppressed_by_provider: bool,
}

/// Whether a raster provider burns place names into its own imagery.
///
/// With USGS Topo or Imagery Topo selected the tiles arrive with "Oklahoma
/// City", "Edmond" and "Midwest City" already printed on them, and this crate
/// then draws its own copy of each on top - the same name twice, a few pixels
/// apart, in two different typefaces. Dropping the provider would be the wrong
/// fix: Topo is the most legible basemap under a reflectivity colour table,
/// which is exactly why it is worth keeping. So the vector label layer stands
/// down instead, and the raster's own cartography is left to do the job.
///
/// The match is exhaustive on purpose. A new provider must make this decision
/// explicitly rather than inherit whichever answer a wildcard arm happened to
/// give, because the failure is silent either way round: too few names, or
/// every name twice.
#[must_use]
pub fn provider_draws_its_own_labels(provider: TileProvider) -> bool {
    match provider {
        // Orthoimagery with roads and place names burned in - the provider's
        // own documentation says so, and it is why this one is the default.
        TileProvider::UsgsImageryTopo => true,
        // The US Topo product: roads, place names, hydrography, boundaries.
        TileProvider::UsgsTopo => true,
        // Aerial photography. No lettering of any kind.
        TileProvider::UsgsImagery => false,
        // Terrain shape only, deliberately quiet. No lettering.
        TileProvider::UsgsShadedRelief => false,
        // OpenStreetMap Standard is a street map and does carry names, but it
        // is the fallback wherever the USGS layers are blank - which is
        // outside the United States, which is exactly where this crate's own
        // label set is thinnest. Keeping our layer on there adds names the
        // raster does not have far more often than it doubles one it does.
        TileProvider::OpenStreetMap => false,
    }
}

#[derive(Clone, Copy, Debug)]
struct Box2D {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Box2D {
    fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && other.min_x < self.max_x
            && self.min_y < other.max_y
            && other.min_y < self.max_y
    }
}

/// What the pane placing these labels is showing.
///
/// Two facts placement cannot work out for itself: which raster is underneath
/// (so it can stand down where the raster already prints names), and how far
/// the view has been carried onto the globe.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LabelContext {
    /// The raster basemap under this pane, if any.
    pub provider: Option<TileProvider>,
    /// [`globe::blend_for_pane`] of the pane's camera scale and size.
    ///
    /// Taken from the LIVE camera rather than from the geometry's LOD bucket,
    /// because a per-bucket blend is a step of tens of screen points at the
    /// edge of the pane, and a step is what the analyst reads as "the map
    /// jumped".
    pub globe_blend: f32,
}

impl LabelContext {
    /// No raster and no globe: the radar-local layer exactly as it shipped.
    pub const RADAR_LOCAL: Self = Self {
        provider: None,
        globe_blend: 0.0,
    };

    /// The context for a pane, derived from what it is drawing.
    ///
    /// The VIEWPORT is required, not optional, because the handoff onto the
    /// globe depends on the size of the pane as well as the scale: the same
    /// camera is a flat map in a quarter pane and a formed globe in a full
    /// one. Passing the wrong one would put the names somewhere the
    /// coastlines are not.
    #[must_use]
    pub fn for_pane(
        camera: Camera2D,
        viewport: ViewportMetrics,
        provider: Option<TileProvider>,
    ) -> Self {
        Self {
            provider,
            globe_blend: globe::blend_for_pane(camera.sanitized().km_per_point, viewport),
        }
    }
}

/// Choose which labels to draw for one pane, as the layer shipped.
///
/// Kept so a caller that has not been told about the raster underneath or the
/// globe still compiles and still gets exactly the picture it had.
/// [`place_labels_for_pane`] is the one to call.
pub fn place_labels(
    geometry: &MapGeometry,
    camera: Camera2D,
    viewport: ViewportMetrics,
    max_labels: usize,
) -> (Vec<PlacedLabel>, PlacementMetrics) {
    place_labels_for_pane(
        geometry,
        camera,
        viewport,
        max_labels,
        LabelContext::RADAR_LOCAL,
    )
}

/// Choose which labels to draw for one pane.
///
/// This is where the globe reaches the label layer, and where a raster that
/// carries its own place names switches the vector layer off. With
/// [`LabelContext::RADAR_LOCAL`] it is [`place_labels`], instruction for
/// instruction.
pub fn place_labels_for_pane(
    geometry: &MapGeometry,
    camera: Camera2D,
    viewport: ViewportMetrics,
    max_labels: usize,
    context: LabelContext,
) -> (Vec<PlacedLabel>, PlacementMetrics) {
    let viewport = viewport.sanitized();
    let mut metrics = PlacementMetrics::default();
    if context.provider.is_some_and(provider_draws_its_own_labels) {
        metrics.suppressed_by_provider = true;
        return (Vec::new(), metrics);
    }
    let blend = context.globe_blend;
    let mut placed: Vec<PlacedLabel> = Vec::new();
    let mut occupied: Vec<Box2D> = Vec::new();
    let limit = max_labels.min(MAX_LABELS_PLACED);

    // Candidates arrive in dataset order, which is stable; ranking by
    // (class, rank) keeps the important ones when the budget runs out.
    let mut candidates: Vec<&ProjectedLabel> = geometry
        .labels
        .iter()
        .take(MAX_CANDIDATES_INSPECTED)
        .collect();
    candidates.sort_by_key(|label| (label.class, label.rank, label.name));

    for candidate in candidates {
        if placed.len() >= limit {
            metrics.budget_exhausted = true;
            break;
        }
        metrics.inspected += 1;

        let world = analyst_runtime::WorldPoint::new(
            f64::from(candidate.east_km),
            f64::from(candidate.north_km),
        );
        // `warp_world` returns the input unchanged at zero blend, so the
        // radar-local placement below is the shipped one, bit for bit.
        let Some(world) = globe::warp_world(world, blend) else {
            metrics.rejected_behind_limb += 1;
            continue;
        };
        let screen = camera.world_to_screen(world, viewport);
        if screen.x < 0.0
            || screen.y < 0.0
            || screen.x > viewport.width_points
            || screen.y > viewport.height_points
        {
            metrics.rejected_offscreen += 1;
            continue;
        }

        let half_width =
            candidate.name.chars().count() as f32 * CHARACTER_WIDTH_POINTS * 0.5 + PADDING_POINTS;
        let half_height = LINE_HEIGHT_POINTS * 0.5 + PADDING_POINTS;
        let bounds = Box2D {
            min_x: screen.x - half_width,
            min_y: screen.y - half_height,
            max_x: screen.x + half_width,
            max_y: screen.y + half_height,
        };
        if occupied.iter().any(|other| other.intersects(bounds)) {
            metrics.rejected_overlap += 1;
            continue;
        }

        occupied.push(bounds);
        placed.push(PlacedLabel {
            name: candidate.name,
            position: screen,
            class: candidate.class,
        });
    }

    metrics.placed = placed.len();
    (placed, metrics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{Generation, GeometryCacheKey, LodBucket};

    use crate::geometry::{GeometryStats, MapGeometry};

    fn geometry(labels: Vec<ProjectedLabel>) -> MapGeometry {
        MapGeometry::new(
            GeometryCacheKey {
                dataset: Generation::new(1),
                projection: Generation::new(1),
                style: Generation::new(1),
                lod: LodBucket(0),
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
            labels,
            GeometryStats::default(),
        )
    }

    fn label(name: &'static str, east: f32, north: f32, rank: u8) -> ProjectedLabel {
        ProjectedLabel {
            class: LabelClass::Place,
            name,
            east_km: east,
            north_km: north,
            rank,
        }
    }

    fn viewport() -> ViewportMetrics {
        ViewportMetrics {
            width_points: 800.0,
            height_points: 600.0,
            pixels_per_point: 1.0,
        }
    }

    fn camera() -> Camera2D {
        Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 1.0,
            rotation_rad: 0.0,
        }
    }

    #[test]
    fn labels_outside_the_pane_are_rejected() {
        let scene = geometry(vec![
            label("Inside", 0.0, 0.0, 0),
            label("FarEast", 5_000.0, 0.0, 0),
        ]);
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), 64);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].name, "Inside");
        assert_eq!(metrics.rejected_offscreen, 1);
    }

    #[test]
    fn overlapping_labels_lose_to_the_better_ranked_one() {
        let scene = geometry(vec![
            label("Important", 0.0, 0.0, 0),
            label("Crowding", 1.0, 1.0, 9),
        ]);
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), 64);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].name, "Important", "rank must decide");
        assert_eq!(metrics.rejected_overlap, 1);
    }

    #[test]
    fn placement_is_bounded_however_many_candidates_arrive() {
        // Spread far enough apart that only the budget can stop them.
        let labels: Vec<ProjectedLabel> = (0..5_000)
            .map(|index| {
                label(
                    "Town",
                    (index % 50) as f32 * 8.0 - 200.0,
                    (index / 50) as f32 * 8.0 - 200.0,
                    0,
                )
            })
            .collect();
        let scene = geometry(labels);
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), MAX_LABELS_PLACED);
        assert!(placed.len() <= MAX_LABELS_PLACED);
        assert!(metrics.inspected <= MAX_CANDIDATES_INSPECTED);
        assert!(metrics.budget_exhausted);
    }

    #[test]
    fn placement_is_deterministic_for_the_same_camera() {
        let labels: Vec<ProjectedLabel> = (0..200)
            .map(|index| {
                label(
                    "Place",
                    (index % 20) as f32 * 15.0 - 150.0,
                    (index / 20) as f32 * 15.0,
                    3,
                )
            })
            .collect();
        let scene = geometry(labels);
        let first = place_labels(&scene, camera(), viewport(), 32).0;
        let second = place_labels(&scene, camera(), viewport(), 32).0;
        assert_eq!(first, second, "the same view must place the same labels");
    }

    #[test]
    fn a_provider_that_prints_its_own_names_switches_the_layer_off() {
        let scene = geometry(vec![
            label("Oklahoma City", 0.0, 0.0, 0),
            label("Edmond", 40.0, 40.0, 1),
            label("Midwest City", -40.0, 40.0, 1),
        ]);
        for provider in [TileProvider::UsgsImageryTopo, TileProvider::UsgsTopo] {
            let (placed, metrics) = place_labels_for_pane(
                &scene,
                camera(),
                viewport(),
                64,
                LabelContext::for_pane(camera(), viewport(), Some(provider)),
            );
            assert!(placed.is_empty(), "{provider:?} drew our names as well");
            assert!(metrics.suppressed_by_provider);
        }
    }

    #[test]
    fn a_provider_without_names_keeps_the_whole_label_layer() {
        let scene = geometry(vec![
            label("Oklahoma City", 0.0, 0.0, 0),
            label("Edmond", 40.0, 40.0, 1),
        ]);
        for provider in [
            None,
            Some(TileProvider::UsgsImagery),
            Some(TileProvider::UsgsShadedRelief),
            Some(TileProvider::OpenStreetMap),
        ] {
            let (placed, metrics) = place_labels_for_pane(
                &scene,
                camera(),
                viewport(),
                64,
                LabelContext::for_pane(camera(), viewport(), provider),
            );
            assert_eq!(placed.len(), 2, "{provider:?} lost a name");
            assert!(!metrics.suppressed_by_provider);
        }
    }

    #[test]
    fn every_shipped_provider_has_an_explicit_answer() {
        // The point of the exhaustive match: this loop compiles only while
        // every provider is accounted for, and it fails loudly if the picker
        // ever ships one nobody decided about.
        for provider in TileProvider::ALL {
            let _ = provider_draws_its_own_labels(provider);
        }
        assert!(provider_draws_its_own_labels(TileProvider::UsgsTopo));
        assert!(!provider_draws_its_own_labels(TileProvider::UsgsImagery));
    }

    #[test]
    fn analysis_zoom_places_exactly_what_it_always_did() {
        // The globe blend is a hard zero here, so placement must be identical
        // to the transform-free path down to the screen point.
        let scene = geometry(vec![
            label("Near", 10.0, -20.0, 0),
            label("Further", 120.0, 300.0, 1),
            label("Edge", -3_000.0, 500.0, 2),
        ]);
        let camera = Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: analyst_runtime::DEFAULT_KM_PER_POINT,
            rotation_rad: 0.0,
        };
        let (placed, metrics) = place_labels_for_pane(
            &scene,
            camera,
            viewport(),
            64,
            LabelContext::for_pane(camera, viewport(), None),
        );
        assert_eq!(metrics.rejected_behind_limb, 0);
        for label in &placed {
            let source = scene
                .labels
                .iter()
                .find(|candidate| candidate.name == label.name)
                .expect("placed labels come from the scene");
            let world = analyst_runtime::WorldPoint::new(
                f64::from(source.east_km),
                f64::from(source.north_km),
            );
            let expected = camera.world_to_screen(world, viewport().sanitized());
            assert_eq!(label.position, expected, "{} moved", label.name);
        }
    }

    #[test]
    fn the_far_side_of_the_globe_is_not_labelled() {
        // 15 000 km from the anchor is 135 degrees away - past the limb once
        // the view is a full globe, and legitimately drawn before that.
        let scene = geometry(vec![
            label("NearSide", 0.0, 0.0, 0),
            label("FarSide", 0.0, 15_000.0, 0),
        ]);
        let globe_camera = Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 40.0,
            rotation_rad: 0.0,
        };
        let (placed, metrics) = place_labels_for_pane(
            &scene,
            globe_camera,
            viewport(),
            64,
            LabelContext::for_pane(globe_camera, viewport(), None),
        );
        assert_eq!(
            metrics.rejected_behind_limb, 1,
            "the far side must be culled"
        );
        assert!(placed.iter().all(|label| label.name == "NearSide"));

        // The same candidate at analysis zoom is not culled - it is simply off
        // the pane, which is a different rejection with a different meaning.
        let (_, near) = place_labels_for_pane(
            &scene,
            camera(),
            viewport(),
            64,
            LabelContext::for_pane(camera(), viewport(), None),
        );
        assert_eq!(near.rejected_behind_limb, 0);
    }

    #[test]
    fn the_globe_pulls_a_distant_name_in_towards_the_anchor() {
        // Not a claim that it looks right, a claim about direction: on a globe
        // a place 8 000 km away must sit CLOSER to the centre of the disc than
        // the equidistant projection puts it, because the limb is compressed.
        let scene = geometry(vec![label("Distant", 0.0, 8_000.0, 0)]);
        let wide = ViewportMetrics {
            width_points: 1_600.0,
            height_points: 1_600.0,
            pixels_per_point: 1.0,
        };
        let globe_camera = Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 40.0,
            rotation_rad: 0.0,
        };
        let (placed, _) = place_labels_for_pane(
            &scene,
            globe_camera,
            wide,
            64,
            LabelContext::for_pane(globe_camera, wide, None),
        );
        let drawn = placed.first().expect("the near hemisphere is drawn");
        let flat = globe_camera.world_to_screen(
            analyst_runtime::WorldPoint::new(0.0, 8_000.0),
            wide.sanitized(),
        );
        let centre = wide.sanitized().center();
        let drawn_radius = (drawn.position.x - centre.x).hypot(drawn.position.y - centre.y);
        let flat_radius = (flat.x - centre.x).hypot(flat.y - centre.y);
        assert!(
            drawn_radius < flat_radius,
            "globe put it at {drawn_radius} points, flat map at {flat_radius}"
        );
    }

    #[test]
    fn the_shipped_entry_point_is_the_radar_local_context() {
        let scene = geometry(vec![
            label("A", 0.0, 0.0, 0),
            label("B", 200.0, 200.0, 1),
            label("C", -150.0, 90.0, 2),
        ]);
        for km_per_point in [0.35_f32, 5.0, 40.0] {
            let camera = Camera2D {
                center_east_km: 0.0,
                center_north_km: 0.0,
                km_per_point,
                rotation_rad: 0.0,
            };
            let shipped = place_labels(&scene, camera, viewport(), 64);
            let explicit =
                place_labels_for_pane(&scene, camera, viewport(), 64, LabelContext::RADAR_LOCAL);
            assert_eq!(shipped, explicit, "at {km_per_point} km/point");
        }
    }

    #[test]
    fn an_empty_scene_places_nothing() {
        let scene = geometry(Vec::new());
        let (placed, metrics) = place_labels(&scene, camera(), viewport(), 64);
        assert!(placed.is_empty());
        assert_eq!(metrics.placed, 0);
    }
}
