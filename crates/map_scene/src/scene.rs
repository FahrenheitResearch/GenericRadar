//! The map scene controller.
//!
//! Owns the dataset, the projection, the style, the background build worker and
//! the CPU-side geometry cache. The application talks only to this: it reports
//! the radar site and each pane's camera scale, and gets back retained geometry
//! that is already correct for the current generations.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;

use analyst_runtime::{
    Camera2D, Generation, GenerationClock, GeometryCacheKey, LatestLaneSender, LodBucket,
    LodSelector, MAX_PANES, ViewportMetrics, latest_lane_channel,
};
use basemap_tiles::TileProvider;

use crate::build::{LOD_REFERENCE_KM_PER_POINT, MapBuildRequest, build_geometry};
use crate::dataset::MapDataset;
use crate::geometry::MapGeometry;
use crate::projection::RadarProjection;
use crate::residency::DEFAULT_BUDGET_BYTES;
use crate::style::MapStyle;
use crate::style_presets::MapChrome;
use crate::tiles::{TileFrame, TileMetrics, TileSceneController};

/// Ceiling for CPU-side retained geometry, mirroring the GPU budget.
pub const DEFAULT_CPU_BUDGET_BYTES: usize = DEFAULT_BUDGET_BYTES;

/// Counters that make the scene's behaviour assertable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SceneMetrics {
    /// Builds actually completed and installed.
    pub geometry_builds: u64,
    /// Build requests handed to the worker.
    pub build_requests: u64,
    /// Results discarded because their generations no longer matched.
    pub stale_results: u64,
    pub resident_bytes: usize,
    pub resident_generations: usize,
}

/// A completed background build.
struct BuiltGeometry {
    geometry: Arc<MapGeometry>,
}

pub struct MapSceneController {
    dataset: MapDataset,
    style: MapStyle,
    style_clock: GenerationClock,
    projection: Option<RadarProjection>,
    projection_clock: GenerationClock,
    lod: [LodSelector; MAX_PANES],
    geometry: HashMap<GeometryCacheKey, Arc<MapGeometry>>,
    /// Insertion order for LRU eviction of the CPU cache.
    use_clock: u64,
    last_used: HashMap<GeometryCacheKey, u64>,
    pending: HashSet<GeometryCacheKey>,
    requests: LatestLaneSender<i16, MapBuildRequest>,
    results: std::sync::mpsc::Receiver<BuiltGeometry>,
    budget_bytes: usize,
    metrics: SceneMetrics,
    /// The raster tile underlay. A private field on purpose: the application
    /// already owns this controller and already talks to it, so the imagery
    /// layer costs the app no new dependency and no new state.
    tiles: TileSceneController,
    /// Display scale, which the tile zoom depends on. Set once per frame by
    /// the host; 1.0 until then, which selects one coarser zoom on a HiDPI
    /// display rather than failing.
    pixels_per_point: f32,
}

impl MapSceneController {
    /// Start the controller and its single background build worker.
    ///
    /// `repaint` is called when a build lands so the host can schedule a frame;
    /// it must not block.
    ///
    /// `Sync` is required because the same closure is shared with the tile
    /// worker pool as well as the geometry worker. Both existing call sites -
    /// a closure capturing an `egui::Context`, and `|| {}` - satisfy it
    /// unchanged.
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self::with_dataset(MapDataset::from_generated(Generation::new(1)), repaint)
    }

    pub fn with_dataset(dataset: MapDataset, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (request_sender, request_receiver) = latest_lane_channel::<i16, MapBuildRequest>();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        let build_repaint = Arc::clone(&repaint);
        let _worker = thread::Builder::new()
            .name("map-scene-build".to_owned())
            .spawn(move || {
                while let Some((_lane, request)) = request_receiver.recv() {
                    let geometry = Arc::new(build_geometry(&request));
                    if result_sender.send(BuiltGeometry { geometry }).is_err() {
                        break;
                    }
                    build_repaint();
                }
            })
            .expect("failed to start map build worker");

        let mut style_clock = GenerationClock::default();
        style_clock.bump();

        Self {
            dataset,
            style: MapStyle::default(),
            style_clock,
            projection: None,
            projection_clock: GenerationClock::default(),
            lod: [LodSelector::new(LOD_REFERENCE_KM_PER_POINT, LOD_REFERENCE_KM_PER_POINT);
                MAX_PANES],
            geometry: HashMap::new(),
            use_clock: 0,
            last_used: HashMap::new(),
            pending: HashSet::new(),
            requests: request_sender,
            results: result_receiver,
            budget_bytes: DEFAULT_CPU_BUDGET_BYTES,
            metrics: SceneMetrics::default(),
            tiles: TileSceneController::new(repaint),
            pixels_per_point: 1.0,
        }
    }

    pub fn metrics(&self) -> SceneMetrics {
        SceneMetrics {
            resident_bytes: self.resident_bytes(),
            resident_generations: self.geometry.len(),
            ..self.metrics
        }
    }

    pub fn projection(&self) -> Option<RadarProjection> {
        self.projection
    }

    pub fn style(&self) -> MapStyle {
        self.style
    }

    /// Geographic centre of the contiguous United States, used to show a map
    /// before any radar has said where it is.
    pub const DEFAULT_ANCHOR: (f64, f64) = (39.83, -98.58);

    /// Anchor at [`Self::DEFAULT_ANCHOR`] so the application opens on a map
    /// rather than an empty pane. A real volume replaces this.
    pub fn set_default_anchor(&mut self) -> bool {
        self.set_radar_anchor(Self::DEFAULT_ANCHOR.0, Self::DEFAULT_ANCHOR.1)
    }

    /// Whether the current anchor is the placeholder rather than a radar.
    pub fn is_default_anchor(&self) -> bool {
        self.projection.map(|projection| projection.id())
            == Some(RadarProjection::new(Self::DEFAULT_ANCHOR.0, Self::DEFAULT_ANCHOR.1).id())
    }

    /// Point the scene at a radar site. Re-anchoring bumps the projection
    /// generation, which makes every previously retained generation
    /// unreachable; nothing built for the old site can be drawn afterwards.
    ///
    /// Returns true when the anchor actually moved.
    pub fn set_radar_anchor(&mut self, lat_deg: f64, lon_deg: f64) -> bool {
        let candidate = RadarProjection::new(lat_deg, lon_deg);
        if self.projection.map(|current| current.id()) == Some(candidate.id()) {
            return false;
        }
        self.projection = Some(candidate);
        self.projection_clock.bump();
        self.geometry.clear();
        self.last_used.clear();
        self.pending.clear();
        true
    }

    pub fn set_style(&mut self, style: MapStyle) {
        if self.style == style {
            return;
        }
        self.style = style;
        self.style_clock.bump();
        self.geometry.clear();
        self.last_used.clear();
        self.pending.clear();
    }

    /// Update a pane's LOD from its camera scale and return the key it needs.
    ///
    /// Hysteresis lives in `LodSelector`, so a small wheel delta returns the
    /// same key and therefore reuses the same geometry.
    pub fn key_for_pane(
        &mut self,
        pane_index: usize,
        km_per_point: f32,
    ) -> Option<GeometryCacheKey> {
        // A key is meaningless without an anchor: there is nothing to project
        // against, so the pane has no map yet.
        self.projection?;
        let selector = self.lod.get_mut(pane_index)?;
        let lod = selector.update(km_per_point);
        Some(GeometryCacheKey {
            dataset: self.dataset.generation,
            projection: self.projection_clock.current(),
            style: self.style_clock.current(),
            lod,
        })
    }

    /// Retained geometry for a key, if it is already built. Marks it used.
    pub fn geometry(&mut self, key: &GeometryCacheKey) -> Option<Arc<MapGeometry>> {
        let geometry = self.geometry.get(key)?.clone();
        self.use_clock += 1;
        self.last_used.insert(*key, self.use_clock);
        Some(geometry)
    }

    /// Ask for a key to be built if it is neither resident nor already queued.
    ///
    /// Returns true if a request was submitted. Callers may invoke this every
    /// frame: once the geometry is resident this is a hash lookup and nothing
    /// else, which is what keeps a pan free of build work.
    pub fn request(&mut self, key: GeometryCacheKey) -> bool {
        if self.geometry.contains_key(&key) || self.pending.contains(&key) {
            return false;
        }
        let Some(projection) = self.projection else {
            return false;
        };
        let request = MapBuildRequest {
            key,
            dataset: self.dataset.clone(),
            projection,
            style: self.style,
        };
        if self.requests.submit(key.lod.0, request).is_err() {
            return false;
        }
        self.pending.insert(key);
        self.metrics.build_requests += 1;
        true
    }

    /// Convenience for the common "make sure this pane can draw" call.
    pub fn geometry_for_pane(
        &mut self,
        pane_index: usize,
        km_per_point: f32,
    ) -> Option<Arc<MapGeometry>> {
        let key = self.key_for_pane(pane_index, km_per_point)?;
        let existing = self.geometry(&key);
        if existing.is_none() {
            self.request(key);
        }
        existing
    }

    /// The display scale, which the tile zoom depends on.
    ///
    /// It arrives here rather than through every pane so the per-pane call
    /// stays short, and because it is a property of the window, not of a pane.
    pub fn set_pixels_per_point(&mut self, pixels_per_point: f32) {
        if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            self.pixels_per_point = pixels_per_point;
        }
    }

    #[must_use]
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    /// This pane's raster tile underlay, or `None` for the vector-only pane.
    ///
    /// `None` when no provider is selected, when there is no radar anchor, or
    /// when the camera is coarser than `basemap_tiles::MIN_TILE_ZOOM` - each
    /// of which leaves the pane exactly as it is today. Call it every frame:
    /// the meshes and the textures behind it are cached, and the visible tile
    /// set is the only part that follows the camera.
    pub fn tiles_for_pane(
        &mut self,
        pane_index: usize,
        camera: Camera2D,
        rect: eframe::egui::Rect,
    ) -> Option<Arc<TileFrame>> {
        let projection = self.projection?;
        let selector = self.lod.get_mut(pane_index)?;
        // Idempotent with the `geometry_for_pane` call beside it: the same
        // scale returns the same bucket, so this is safe whether or not the
        // vector layer has already asked.
        let lod = selector.update(camera.sanitized().km_per_point);
        let viewport = ViewportMetrics {
            width_points: rect.width().max(1.0),
            height_points: rect.height().max(1.0),
            pixels_per_point: self.pixels_per_point,
        };
        // The scrim is the pane's OWN ground, partially over the imagery, so a
        // light look dims towards light and a dark look towards dark. That
        // ties the imagery to the four presets without making the tile layer
        // part of the geometry cache key.
        let canvas = MapChrome::for_style(self.style).canvas;
        self.tiles.frame_for_pane(
            &projection,
            self.projection_clock.current(),
            lod,
            camera,
            viewport,
            [canvas.r, canvas.g, canvas.b],
        )
    }

    #[must_use]
    pub fn tile_provider(&self) -> Option<TileProvider> {
        self.tiles.provider()
    }

    /// Choose the ground imagery. `None` is the shipped vector-only pane and
    /// stays the default, so an offline or firewalled machine is never worse
    /// off than it is today.
    pub fn set_tile_provider(&mut self, provider: Option<TileProvider>) {
        self.tiles.set_provider(provider);
    }

    /// Whether a provider may be used with this store's configuration. A
    /// picker should hide the ones that may not.
    #[must_use]
    pub fn tile_provider_permitted(&self, provider: TileProvider) -> bool {
        self.tiles.permits(provider)
    }

    /// The credit string the pane must draw. Displaying it is a condition of
    /// use for every provider here, which is why there is no switch for it.
    #[must_use]
    pub fn tile_attribution(&self) -> Option<&'static str> {
        self.tiles.attribution()
    }

    /// How much the imagery is dimmed on the current look, 0..1.
    ///
    /// Computed against the pane's own ground rather than read from a table:
    /// three of the five providers are light maps, and how much dimming they
    /// need depends on what they are drawn over.
    #[must_use]
    pub fn tile_scrim(&self) -> f32 {
        let canvas = MapChrome::for_style(self.style).canvas;
        self.tiles.scrim_for_ground([canvas.r, canvas.g, canvas.b])
    }

    pub fn set_tile_scrim(&mut self, alpha: f32) {
        self.tiles.set_scrim(alpha);
    }

    pub fn set_tiles_offline(&mut self, offline: bool) {
        self.tiles.set_offline(offline);
    }

    #[must_use]
    pub fn tiles_offline(&self) -> bool {
        self.tiles.is_offline()
    }

    #[must_use]
    pub fn tile_metrics(&self) -> TileMetrics {
        self.tiles.metrics()
    }

    #[must_use]
    pub fn tile_cache_root(&self) -> Option<&std::path::Path> {
        self.tiles.cache_root()
    }

    /// Install completed builds. Call once per frame before drawing.
    ///
    /// Also the tile layer's frame boundary: decoded tiles are taken from the
    /// store, the GPU's uploads and evictions are applied, and every tile no
    /// pane asked for last frame is cancelled before it reaches the network.
    pub fn poll(&mut self) -> usize {
        self.tiles.poll();
        let mut installed = 0;
        while let Ok(result) = self.results.try_recv() {
            let key = result.geometry.key;
            self.pending.remove(&key);
            if !self.key_is_current(&key) {
                self.metrics.stale_results += 1;
                continue;
            }
            self.use_clock += 1;
            self.last_used.insert(key, self.use_clock);
            self.geometry.insert(key, result.geometry);
            self.metrics.geometry_builds += 1;
            installed += 1;
        }
        if installed > 0 {
            self.enforce_budget();
        }
        installed
    }

    /// Whether a key still matches every current generation. A build that
    /// finishes after a site switch fails this and is dropped.
    pub fn key_is_current(&self, key: &GeometryCacheKey) -> bool {
        key.dataset == self.dataset.generation
            && key.projection == self.projection_clock.current()
            && key.style == self.style_clock.current()
    }

    pub fn resident_bytes(&self) -> usize {
        self.geometry
            .values()
            .map(|geometry| geometry.estimated_bytes)
            .sum()
    }

    pub fn resident_lods(&self) -> Vec<LodBucket> {
        let mut lods: Vec<LodBucket> = self.geometry.keys().map(|key| key.lod).collect();
        lods.sort_unstable();
        lods
    }

    fn enforce_budget(&mut self) {
        while self.resident_bytes() > self.budget_bytes && self.geometry.len() > 1 {
            let Some(victim) = self
                .last_used
                .iter()
                .filter(|(key, _)| self.geometry.contains_key(*key))
                .min_by_key(|(_, used)| **used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.geometry.remove(&victim);
            self.last_used.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{GeoLineFeature, MapLayer};

    static LINE: &[(f32, f32)] = &[(-97.5, 35.2), (-97.4, 35.25), (-97.3, 35.3), (-97.2, 35.35)];

    fn test_dataset() -> MapDataset {
        let mut bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for (lon, lat) in LINE {
            bbox[0] = bbox[0].min(*lon);
            bbox[1] = bbox[1].min(*lat);
            bbox[2] = bbox[2].max(*lon);
            bbox[3] = bbox[3].max(*lat);
        }
        MapDataset::from_parts(
            Generation::new(1),
            vec![GeoLineFeature {
                layer: MapLayer::County,
                bbox,
                points: LINE,
            }],
            Vec::new(),
            Vec::new(),
        )
    }

    fn controller() -> MapSceneController {
        MapSceneController::with_dataset(test_dataset(), || {})
    }

    /// Drain the worker until the key lands, so tests do not race it.
    fn settle(controller: &mut MapSceneController, key: GeometryCacheKey) {
        for _ in 0..200 {
            controller.poll();
            if controller.geometry.contains_key(&key) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("geometry for {key:?} never arrived");
    }

    #[test]
    fn a_key_requires_an_anchor() {
        let mut controller = controller();
        assert_eq!(controller.key_for_pane(0, 0.35), None);
        assert!(controller.projection().is_none());
    }

    #[test]
    fn panning_never_changes_the_key_or_requests_a_build() {
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let key = controller.key_for_pane(0, 0.35).expect("key");
        assert!(controller.request(key));
        settle(&mut controller, key);

        let builds_after_first = controller.metrics().build_requests;
        // A pan does not touch scale at all, so the pane asks for the same key
        // ten thousand times and must never queue another build.
        for _ in 0..10_000 {
            let repeated = controller.key_for_pane(0, 0.35).expect("key");
            assert_eq!(repeated, key);
            assert!(!controller.request(repeated));
        }
        assert_eq!(controller.metrics().build_requests, builds_after_first);
        assert_eq!(controller.metrics().geometry_builds, 1);
    }

    #[test]
    fn a_small_zoom_inside_the_bucket_reuses_the_geometry() {
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let key = controller.key_for_pane(0, 0.35).expect("key");
        settle_request(&mut controller, key);

        // Wheel deltas either side of the starting scale, inside hysteresis.
        for scale in [0.34_f32, 0.36, 0.33, 0.37, 0.35] {
            let repeated = controller.key_for_pane(0, scale).expect("key");
            assert_eq!(repeated, key, "scale {scale} left the bucket");
            assert!(!controller.request(repeated));
        }
        assert_eq!(controller.metrics().build_requests, 1);
    }

    #[test]
    fn a_large_zoom_crosses_the_bucket_and_builds_once() {
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let near = controller.key_for_pane(0, 0.35).expect("key");
        settle_request(&mut controller, near);

        let far = controller.key_for_pane(0, 8.0).expect("key");
        assert_ne!(far.lod, near.lod, "a 20x zoom must change bucket");
        assert!(controller.request(far));
        // Asking again before it lands must not queue a second build.
        assert!(!controller.request(far));
        assert_eq!(controller.metrics().build_requests, 2);
    }

    #[test]
    fn cameras_with_different_centres_share_one_key() {
        // The controller never sees a camera centre; this asserts the shape of
        // the key itself, which is what makes that true.
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let a = controller.key_for_pane(0, 0.35).expect("key");
        let b = controller.key_for_pane(1, 0.35).expect("key");
        assert_eq!(a, b, "two panes at one scale must share geometry");
    }

    #[test]
    fn changing_site_invalidates_every_previous_generation() {
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let ktlx = controller.key_for_pane(0, 0.35).expect("key");
        settle_request(&mut controller, ktlx);
        assert!(controller.geometry(&ktlx).is_some());

        assert!(controller.set_radar_anchor(45.7150, -122.9650));
        assert!(
            controller.geometry(&ktlx).is_none(),
            "old site still drawable"
        );
        assert!(!controller.key_is_current(&ktlx));

        let krtx = controller.key_for_pane(0, 0.35).expect("key");
        assert_ne!(krtx.projection, ktlx.projection);
    }

    #[test]
    fn re_anchoring_to_the_same_site_is_a_no_op() {
        let mut controller = controller();
        assert!(controller.set_radar_anchor(35.3333, -97.2778));
        let key = controller.key_for_pane(0, 0.35).expect("key");
        settle_request(&mut controller, key);

        assert!(!controller.set_radar_anchor(35.3333, -97.2778));
        assert!(
            controller.geometry(&key).is_some(),
            "an identical anchor must not throw away resident geometry"
        );
    }

    #[test]
    fn a_style_change_invalidates_geometry_without_touching_the_dataset() {
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let before = controller.key_for_pane(0, 0.35).expect("key");
        settle_request(&mut controller, before);

        let mut style = controller.style();
        style.county.width_px += 1.0;
        controller.set_style(style);

        let after = controller.key_for_pane(0, 0.35).expect("key");
        assert_ne!(after.style, before.style);
        assert_eq!(
            after.dataset, before.dataset,
            "dataset identity is unchanged"
        );
        assert!(controller.geometry(&before).is_none());
    }

    #[test]
    fn a_stale_build_cannot_install() {
        let mut controller = controller();
        controller.set_radar_anchor(35.3333, -97.2778);
        let stale = controller.key_for_pane(0, 0.35).expect("key");
        controller.request(stale);
        // Switch sites while the build is in flight.
        controller.set_radar_anchor(45.7150, -122.9650);

        for _ in 0..200 {
            controller.poll();
            if controller.metrics().stale_results > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(controller.metrics().stale_results, 1);
        assert!(controller.geometry(&stale).is_none());
        assert_eq!(controller.metrics().geometry_builds, 0);
    }

    fn settle_request(controller: &mut MapSceneController, key: GeometryCacheKey) {
        controller.request(key);
        settle(controller, key);
    }
}
