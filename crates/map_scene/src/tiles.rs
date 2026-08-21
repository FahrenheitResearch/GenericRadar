//! Scene policy for the raster tile basemap.
//!
//! This is the half of the tile layer that decides *what* to draw: which tile
//! zoom a pane is at, which tiles that pane can see, which texture stands in
//! for one that has not arrived, and what the imagery is dimmed with so the
//! radar stays readable on top of it. It owns the [`TileStore`] and the mesh
//! cache. There is no wgpu here — the GPU half is [`crate::tile_gpu`], and it
//! only executes the draw list this produces.
//!
//! # What this layer is not allowed to do
//!
//! The imagery is an addition, never a replacement. When no provider is
//! selected, when the camera is coarser than [`basemap_tiles::MIN_TILE_ZOOM`],
//! when there is no radar anchor, or when every fetch fails, this returns
//! `None` and the pane is byte for byte the vector-only pane that shipped.
//! That degrade path is reached deliberately in each of those cases rather
//! than by accident.
//!
//! # Zoom selection, and why it is derived from the LOD bucket
//!
//! Rounding `log2` of the raw camera scale — what the sibling application does
//! — has no hysteresis: a camera parked on a rounding boundary flips zoom
//! every frame, and each flip discards one whole tile set and requests
//! another. This workspace already solved that problem once, for vector
//! geometry, in `analyst_runtime::LodSelector` (half-octave buckets, 12%
//! hysteresis). [`tile_zoom_for`] therefore reads the bucket's *centre* scale,
//! never the instantaneous camera scale, which makes the tile zoom a pure
//! function of `(LodBucket, anchor latitude, pixels_per_point)` and lets the
//! tile layer inherit that selector's hysteresis. A pan cannot change the tile
//! zoom, and a zoom inside a bucket cannot either — structurally, not merely
//! in practice.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use analyst_runtime::{Camera2D, Generation, LodBucket, MAX_PANES, ScreenPoint, ViewportMetrics};
use basemap_tiles::{
    DecodedTile, MAX_ANCESTOR_LEVELS, MAX_TILE_ZOOM, MIN_TILE_ZOOM, TileCacheConfig, TileId,
    TileMesh, TileProvider, TileState, TileStore, TileStoreMetrics, ViewportGeo, build_tile_mesh,
    visible_tiles, zoom_for_ground_resolution,
};

use crate::build::LOD_REFERENCE_KM_PER_POINT;
use crate::projection::RadarProjection;

/// Identity of one tile in one provider's pyramid.
pub type TileKey = (TileProvider, TileId);

/// Ceiling on tiles considered for one pane at one zoom.
///
/// A 1500x950 point pane needs 39 tiles at a bucket centre and 64 at the wide
/// end of a bucket; at `pixels_per_point = 2` that is 110 and 196. This cap is
/// therefore slack for every ordinary window and exists for the ultrawide,
/// 3x-scale, maximised case. Saturating it drops the zoom by one, which
/// quarters the count.
pub const MAX_TILES_PER_PANE: usize = 256;

/// Ceiling on *draws* for one pane: each visible tile can carry at most one
/// extra draw — the resident ancestor kept underneath it while it fades in,
/// which is what turns a tile's arrival into a crossfade instead of a blink.
/// The GPU layer sizes its per-draw uniform buffer against this.
pub const MAX_DRAWS_PER_PANE: usize = 2 * MAX_TILES_PER_PANE;

/// Tiles warmed at the next tile zoom when the camera is a notch away from
/// the boundary that will ask for them. Eight is the centre 2x4 of a pane —
/// the ground under the cursor during a zoom — and is deliberately one level
/// and a handful of tiles, never a ring at every level: prefetch exists to
/// hide the *first* step of a deliberate zoom, and anything larger is a
/// speculative fetch storm a phone would pay for. Only providers whose terms
/// permit prefetch at all are ever warmed; see
/// [`TileProvider::prefetch_permitted`].
const PREFETCH_TILES: usize = 8;

/// Meshes built on the UI thread in one frame.
///
/// Measured by the core crate: 1500 meshes across five real sites and z5-z16
/// build in 0.04 s in release, about 27 us each, and z11 and finer collapse to
/// a single quad. A whole cold pane at a wide zoom is therefore about 7 ms —
/// worth bounding so a site change cannot drop a frame, not worth another
/// worker pool.
const MAX_MESH_BUILDS_PER_FRAME: usize = 64;

/// Decoded tiles taken from the store in one frame.
const MAX_DRAIN_PER_FRAME: usize = 24;

/// Meshes, and fade clocks, held for tiles that are no longer on screen.
///
/// A BOUND, not a target, and it exists because a mesh is cached on
/// `(TileId, projection)` and the projection does not change while an operator
/// works one site. Without this the cache holds every tile the camera has ever
/// crossed: measured, an ordinary 900 km pan at the default scale leaves 223
/// meshes behind, and 600 km of panning at 0.01 km/point leaves 1838, with
/// nothing anywhere to remove them. That is small in bytes and unbounded in
/// principle, which is exactly the shape of leak that is invisible for an hour
/// and then is not.
///
/// Sized against the largest working set the layer can have on screen at once:
/// [`MAX_PANES`] panes of [`MAX_TILES_PER_PANE`] tiles is 1024, so this is
/// four times it. Eviction is least-recently-used and every tile drawn this
/// frame carries the newest stamp, so the tiles on screen are never the ones
/// evicted - which is what keeps the pan invariant that the mesh cache exists
/// for.
pub const MAX_RESIDENT_MESHES: usize = 4 * MAX_PANES * MAX_TILES_PER_PANE;

/// A sweep prunes to this fraction of the bound rather than exactly to it, so
/// the next tile drawn does not immediately trigger another sweep.
const MESH_PRUNE_TARGET: usize = MAX_RESIDENT_MESHES * 3 / 4;

/// Decoded tiles held while waiting for the GPU to acknowledge the upload.
///
/// The pixels are handed over by [`TileStore::drain_ready`] and are gone from
/// the store afterwards, so they are held here until a `prepare` says they are
/// resident. Without that acknowledgement a frame egui never paints — a
/// minimised window, a pane removed from the layout — would drop the only copy
/// and leave a permanent hole while the store still called the tile `Ready`.
const MAX_PENDING_UPLOADS: usize = 48;

/// How long a newly arrived tile takes to reach full opacity.
const FADE_SECONDS: f32 = 0.15;

/// How far the imagery is pulled from the pane's own ground towards mid grey.
///
/// MEASURED, not chosen. Rendering real USGS Imagery+Topo over KTLX with the
/// provider's shipped 0.20 scrim on the Slate ground gives a frame whose mean
/// luminance is 92/255 = 0.36, which is the picture the plan was tuned to. On
/// the Slate ground (0.035) this constant reproduces exactly that scrim for
/// exactly that provider, and corrects the ones the provider table gets wrong
/// - see [`TileSceneController::scrim_for_ground`].
const TARGET_PULL_TO_MID: f32 = 0.70;

/// Ceiling on the computed scrim: past this the imagery is the ground with a
/// faint texture, and there is no point drawing it at all.
const MAX_SCRIM: f32 = 0.80;

/// The computed scrim is rounded to this, so a drifting estimate cannot make
/// the ground shimmer while tiles arrive.
const SCRIM_QUANTUM: f32 = 0.05;

/// Tiles sampled before a provider's brightness estimate is frozen. One pane
/// is 16 to 64 tiles, so this settles within the first screenful.
const LUMINANCE_SAMPLES: u32 = 16;

/// Points sampled along each pane edge when unprojecting the view.
///
/// Four corners is not enough: the pane edge is a curve in geographic space,
/// so a corner-only bounding box drops tiles along the middle of an edge. Six
/// per edge yields 24 boundary points.
const VIEW_EDGE_SAMPLES: usize = 6;

/// The tile zoom for a LOD bucket, or `None` when the layer should switch off.
///
/// `None` means the camera is coarser than the coarsest zoom the mesh
/// generator can keep sub-pixel ([`basemap_tiles::MIN_TILE_ZOOM`]). The layer
/// switches *off* there rather than clamping, because a 4 km/texel picture
/// stretched under a continental view looks broken, and the vector coastline
/// is the better map at that scale anyway.
#[must_use]
pub fn tile_zoom_for(lod: LodBucket, anchor_lat_deg: f64, pixels_per_point: f32) -> Option<u8> {
    let km_per_point = lod.center_scale(LOD_REFERENCE_KM_PER_POINT);
    if !km_per_point.is_finite() || km_per_point <= 0.0 {
        return None;
    }
    let pixels_per_point = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let m_per_px = f64::from(km_per_point) * 1_000.0 / f64::from(pixels_per_point);
    // Floor of 0 rather than MIN_TILE_ZOOM: the function clamps, and a clamped
    // answer would hide the "too coarse for tiles" case behind a z5 that is
    // wrong for the camera.
    let ideal = zoom_for_ground_resolution(m_per_px, anchor_lat_deg, 0, MAX_TILE_ZOOM);
    (ideal >= MIN_TILE_ZOOM).then_some(ideal)
}

/// Identity of one pane's tile picture.
///
/// The camera centre is deliberately absent, exactly as it is absent from
/// `GeometryCacheKey`: only the provider, the projection and the zoom change
/// what the picture *is*. Which tiles of it are on screen is a camera
/// question, answered per frame.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TileFrameKey {
    pub provider: TileProvider,
    pub projection: Generation,
    pub zoom: u8,
}

/// One tile, ready to draw.
#[derive(Clone, Debug)]
pub struct TileDraw {
    /// The ground this covers, subdivided into radar-local kilometres.
    pub mesh: Arc<TileMesh>,
    /// The tile whose *texture* is sampled: `mesh.tile` when that one is
    /// resident, otherwise an ancestor. With `uv_offset_scale` this is what
    /// turns a cold cache, and a permanent 404, into a coarser picture rather
    /// than a hole.
    pub texture: TileId,
    /// `[u_offset, v_offset, u_scale, v_scale]` applied to `TileVertex::uv`.
    /// `[0, 0, 1, 1]` when `texture == mesh.tile`.
    pub uv_offset_scale: [f32; 4],
    /// Per-tile fade, 0..1, so a filling map does not flicker.
    pub alpha: f32,
}

/// What one pane draws this frame. Immutable and `Arc`-shared.
pub struct TileFrame {
    pub key: TileFrameKey,
    pub draws: Arc<[TileDraw]>,
    /// Decoded tiles not yet known to be on the GPU. The same list goes to
    /// every pane; the GPU side skips anything already resident, so uploading
    /// once is the natural outcome rather than a special case.
    pub uploads: Arc<[Arc<DecodedTile>]>,
    /// The provider's required credit, which the pane draws. Not optional: see
    /// [`basemap_tiles::TileProvider::attribution`].
    pub attribution: &'static str,
    /// `[r, g, b, a]`, straight alpha. The tile shader mixes this into the
    /// sampled texel, so the imagery is dimmed and nothing else is — a
    /// full-pane scrim would also dim the ground where a tile is missing,
    /// which is precisely where the vector-only fallback needs its contrast.
    pub scrim: [f32; 4],
    /// Fraction of visible tiles drawn with their own texture rather than an
    /// ancestor's, 0..1. Diagnostics, and the "still loading" signal.
    pub coverage: f32,
    /// The GPU's backchannel: which tiles it uploaded, and which it evicted.
    pub feedback: Arc<TileFeedback>,
}

impl std::fmt::Debug for TileFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TileFrame")
            .field("key", &self.key)
            .field("draws", &self.draws.len())
            .field("uploads", &self.uploads.len())
            .field("coverage", &self.coverage)
            .finish_non_exhaustive()
    }
}

/// What the GPU layer reports back to the scene layer.
///
/// Two facts, both load-bearing. An *upload* is the acknowledgement that lets
/// the scene stop holding a decoded tile's pixels. An *eviction* is the
/// scene's only way to learn that a tile it believes is drawable no longer has
/// pixels anywhere: the store would keep answering `Ready` for it forever, and
/// the tile would be a permanent hole that appears only under memory pressure.
/// The scene answers an eviction with [`TileStore::forget`], which normally
/// costs one disk read rather than a download.
#[derive(Debug, Default)]
pub struct TileFeedback {
    inner: Mutex<FeedbackInner>,
}

#[derive(Debug, Default)]
struct FeedbackInner {
    uploaded: Vec<TileKey>,
    evicted: Vec<TileKey>,
}

impl TileFeedback {
    /// Called from `prepare`, on the render thread, once a texture is
    /// resident.
    pub fn record_upload(&self, key: TileKey) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.uploaded.push(key);
        }
    }

    /// Called from `prepare` when the texture budget evicts a tile.
    pub fn record_eviction(&self, key: TileKey) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.evicted.push(key);
        }
    }

    /// Take everything recorded since the last call.
    #[must_use]
    pub fn take(&self) -> (Vec<TileKey>, Vec<TileKey>) {
        match self.inner.lock() {
            Ok(mut inner) => (
                std::mem::take(&mut inner.uploaded),
                std::mem::take(&mut inner.evicted),
            ),
            Err(_) => (Vec::new(), Vec::new()),
        }
    }
}

/// Counters that make the tile layer's behaviour assertable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileMetrics {
    pub store: TileStoreMetrics,
    pub meshes_built: u64,
    pub meshes_resident: usize,
    pub mesh_bytes: usize,
    /// Draws served by an ancestor texture rather than by the exact tile.
    pub ancestor_substitutions: u64,
    /// Frames handed to a pane.
    pub frames_built: u64,
    /// Tiles whose GPU texture was evicted and whose store state was reset.
    pub textures_forgotten: u64,
    /// Meshes dropped by [`MAX_RESIDENT_MESHES`]. Zero for an ordinary
    /// session; non-zero means the camera has crossed more ground than the
    /// bound holds, which is the case this counter exists to make visible.
    pub meshes_evicted: u64,
    /// Fade clocks currently held, one per tile recently drawn. Bounded by
    /// [`MAX_RESIDENT_MESHES`].
    pub fade_clocks_tracked: usize,
    /// Decoded tiles the GPU acknowledged.
    pub tiles_uploaded: u64,
    /// Next-zoom tiles whose fetch was started ahead of the camera crossing a
    /// zoom boundary. Always zero for a provider whose terms forbid prefetch.
    pub tiles_prefetched: u64,
}

/// One cached mesh and when it was last drawn.
struct ResidentMesh {
    mesh: Arc<TileMesh>,
    last_used: u64,
}

/// One tile's fade: when it first drew, and when it last did.
struct FadeClock {
    first_seen: Instant,
    last_used: u64,
}

/// Owns the tile store, the mesh cache and the per-frame draw lists.
pub struct TileSceneController {
    provider: Option<TileProvider>,
    store: TileStore,
    /// Meshes for the current projection only; a new anchor drops all of them.
    /// Bounded by [`MAX_RESIDENT_MESHES`] and evicted least-recently-used.
    meshes: HashMap<TileId, ResidentMesh>,
    mesh_projection: Option<Generation>,
    /// Ticks once per pane per frame. The stamp on a mesh or a fade clock, and
    /// the only ordering eviction needs.
    clock: u64,
    /// Decoded tiles held until the GPU acknowledges the upload.
    pending_uploads: Vec<(TileKey, Arc<DecodedTile>)>,
    /// This frame's snapshot of `pending_uploads`, shared by every pane.
    uploads: Arc<[Arc<DecodedTile>]>,
    /// Every tile any pane asked for since the last [`Self::poll`]. Handed to
    /// [`TileStore::retain`], which is the cancellation path.
    wanted: HashSet<TileKey>,
    /// When each drawn tile first appeared, for the fade. Bounded exactly as
    /// the mesh cache is, and by the same clock, so a tile that is on screen
    /// is never pruned and can never re-fade under a viewer.
    first_seen: HashMap<TileKey, FadeClock>,
    feedback: Arc<TileFeedback>,
    /// Mean luminance of each provider's imagery, measured from the tiles that
    /// actually arrived: `(sum, count)`. Frozen at [`LUMINANCE_SAMPLES`].
    luminance: HashMap<TileProvider, (f32, u32)>,
    scrim_alpha: Option<f32>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    metrics: TileMetrics,
}

impl TileSceneController {
    #[must_use]
    pub fn new(repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::with_config(TileCacheConfig::default(), repaint)
    }

    #[must_use]
    pub fn with_config(config: TileCacheConfig, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        let store = TileStore::new(usable_cache(config), Arc::clone(&repaint));
        Self {
            provider: None,
            store,
            meshes: HashMap::new(),
            mesh_projection: None,
            clock: 0,
            pending_uploads: Vec::new(),
            uploads: Vec::new().into(),
            wanted: HashSet::new(),
            first_seen: HashMap::new(),
            feedback: Arc::new(TileFeedback::default()),
            luminance: HashMap::new(),
            scrim_alpha: None,
            repaint,
            metrics: TileMetrics::default(),
        }
    }

    #[must_use]
    pub fn provider(&self) -> Option<TileProvider> {
        self.provider
    }

    /// Choose the imagery, or `None` for the shipped vector-only pane.
    ///
    /// Switching providers drops nothing but the draw lists: a mesh depends on
    /// the projection, not on which imagery fills it, and the disk cache is
    /// already per provider.
    pub fn set_provider(&mut self, provider: Option<TileProvider>) {
        if self.provider == provider {
            return;
        }
        self.provider = provider;
        // The scrim follows the provider unless the operator has pinned one:
        // an aerial photograph and a shaded relief need very different amounts
        // of dimming, and carrying one number across a provider switch is how
        // a topographic map ends up needlessly murky.
        self.scrim_alpha = None;
        (self.repaint)();
    }

    /// Whether this store's configuration satisfies a provider's terms.
    ///
    /// `false` only where a provider requires a minimum cache lifetime and
    /// this store has no disk cache to enforce it with. A picker should hide
    /// such a provider rather than offer one that cannot be used lawfully.
    #[must_use]
    pub fn permits(&self, provider: TileProvider) -> bool {
        self.store.permits(provider)
    }

    /// The provider's own suggestion, or the operator's pin. The picker's
    /// starting point; [`Self::scrim_for_ground`] is what actually draws.
    #[must_use]
    pub fn scrim(&self) -> f32 {
        self.scrim_alpha
            .unwrap_or_else(|| self.provider.map_or(0.0, TileProvider::default_scrim_alpha))
    }

    /// The measured mean luminance of a provider's imagery, 0..1, once any of
    /// it has arrived. Diagnostics, and the input to [`Self::scrim_for_ground`].
    #[must_use]
    pub fn imagery_luminance(&self, provider: TileProvider) -> Option<f32> {
        let (sum, count) = self.luminance.get(&provider)?;
        (*count > 0).then(|| sum / *count as f32)
    }

    /// How much to dim this provider's imagery towards a pane whose ground is
    /// `ground_rgb`.
    ///
    /// The provider table alone is not enough, and the frame that proved it is
    /// checked in: USGS shaded relief over Oklahoma renders at a mean
    /// luminance of 240/255 with its suggested 0.05 scrim - a white pane, on
    /// which low-dBZ returns and near-zero velocity are simply gone. The table
    /// grades providers by how *busy* the imagery is, which is the right axis
    /// for clutter and the wrong one for contrast: three of the five providers
    /// (topo, shaded relief, OpenStreetMap) are light maps, and a light map on
    /// a dark pane needs far more dimming than an aerial photograph does.
    ///
    /// So the amount is computed rather than tabulated. The imagery's own mean
    /// luminance is measured from the 32x32 mip the decoder already built, and
    /// the scrim is the mix that lands it at a target pulled from the pane's
    /// ground towards mid grey. It never dims *less* than the provider asked
    /// for, so this can only correct the table upwards, and it is quantised so
    /// a settling estimate cannot make the ground shimmer.
    ///
    /// On the Slate ground this returns 0.20 for USGS Imagery+Topo - the
    /// provider's own suggested value, and the one the measured frame was
    /// tuned to - and about 0.65 for shaded relief, which lands both at the
    /// same readable luminance.
    #[must_use]
    pub fn scrim_for_ground(&self, ground_rgb: [f32; 3]) -> f32 {
        if let Some(pinned) = self.scrim_alpha {
            return pinned;
        }
        let Some(provider) = self.provider else {
            return 0.0;
        };
        let floor = provider.default_scrim_alpha();
        let Some((sum, count)) = self.luminance.get(&provider) else {
            return floor;
        };
        if *count == 0 {
            return floor;
        }
        let imagery = sum / *count as f32;
        let ground = luminance_of(ground_rgb);
        let target = ground + (0.5 - ground) * TARGET_PULL_TO_MID;
        let span = imagery - ground;
        if imagery <= target || span.abs() < 1e-3 {
            // Already at or below the target, or the ground is as bright as
            // the imagery, so no amount of mixing would move it.
            return floor;
        }
        let alpha = (imagery - target) / span;
        if alpha > 1.0 {
            // The target is past the ground itself, so no amount of mixing
            // reaches it: a near-white map on a near-white pane cannot be
            // dimmed by mixing towards white. Dimming anyway would erase the
            // imagery without changing its brightness, so the provider's own
            // suggestion stands. On a light look this is the right answer
            // anyway - radar over a light ground is that look's premise.
            return floor;
        }
        let quantised = (alpha / SCRIM_QUANTUM).round() * SCRIM_QUANTUM;
        // `f32::clamp` PANICS when its minimum exceeds its maximum, and the
        // minimum here is provider data. Every shipped provider asks for 0.35
        // or less today, but this runs inside a paint and a panic there takes
        // the window with it, so the floor is bounded rather than trusted.
        quantised.clamp(floor.min(MAX_SCRIM), MAX_SCRIM)
    }

    pub fn set_scrim(&mut self, alpha: f32) {
        self.scrim_alpha = Some(alpha.clamp(0.0, 1.0));
    }

    pub fn set_offline(&mut self, offline: bool) {
        self.store.set_offline(offline);
    }

    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.store.is_offline()
    }

    #[must_use]
    pub fn attribution(&self) -> Option<&'static str> {
        self.provider.map(TileProvider::attribution)
    }

    #[must_use]
    pub fn cache_root(&self) -> Option<&std::path::Path> {
        self.store.cache_root()
    }

    #[must_use]
    pub fn metrics(&self) -> TileMetrics {
        TileMetrics {
            store: self.store.metrics(),
            meshes_resident: self.meshes.len(),
            mesh_bytes: self
                .meshes
                .values()
                .map(|resident| resident.mesh.estimated_bytes)
                .sum(),
            fade_clocks_tracked: self.first_seen.len(),
            ..self.metrics
        }
    }

    /// Frame boundary: apply the GPU's feedback, cancel what no pane wants,
    /// and take newly decoded tiles. Returns the number of tiles waiting to be
    /// uploaded.
    ///
    /// Call once per frame *before* the panes draw. `wanted` is the union from
    /// the previous frame, which is what makes cancellation a whole-frame
    /// decision rather than a per-pane one — otherwise the first pane's set
    /// would cancel the second pane's tiles.
    pub fn poll(&mut self) -> usize {
        let (uploaded, evicted) = self.feedback.take();
        if !uploaded.is_empty() {
            let done: HashSet<TileKey> = uploaded.into_iter().collect();
            self.metrics.tiles_uploaded += done.len() as u64;
            self.pending_uploads.retain(|(key, _)| !done.contains(key));
        }
        for key in evicted {
            // The pixels are gone from the GPU and from here. Forget the tile
            // so the next request fetches it again, which is normally one disk
            // read rather than a download.
            self.store.forget(key.0, key.1);
            // The fade clock deliberately SURVIVES the eviction. The fade
            // exists to soften a tile's first appearance; a tile that was
            // already on screen a moment ago and lost its texture to the
            // budget is not appearing for the first time, and restarting its
            // fade makes it flash. Under sustained pressure - four panes at
            // four zooms, which is the layout that exceeds the budget - every
            // tile is evicted and re-uploaded repeatedly, so restarting the
            // fade means the pane never reaches full opacity at all: measured,
            // it pulsed through 11,048 refetches in sixty seconds without one
            // settled frame. The clock is bounded by the same sweep as the
            // mesh cache, so a tile that has genuinely been gone a while falls
            // out of it and does fade in again.
            self.metrics.textures_forgotten += 1;
        }

        self.store.retain(&self.wanted);
        self.wanted.clear();

        if self.provider.is_some() && self.pending_uploads.len() < MAX_PENDING_UPLOADS {
            for decoded in self.store.drain_ready(MAX_DRAIN_PER_FRAME) {
                self.sample_luminance(&decoded);
                self.pending_uploads
                    .push(((decoded.provider, decoded.tile), decoded));
            }
        }
        // Oldest first if the GPU is not keeping up: drop the pixels and let
        // the store fetch them again rather than growing without bound.
        while self.pending_uploads.len() > MAX_PENDING_UPLOADS {
            let (key, _) = self.pending_uploads.remove(0);
            self.store.forget(key.0, key.1);
        }
        self.uploads = self
            .pending_uploads
            .iter()
            .map(|(_, decoded)| Arc::clone(decoded))
            .collect();
        self.uploads.len()
    }

    /// Build one pane's tile picture.
    ///
    /// `None` when no provider is selected, when the camera is coarser than
    /// [`basemap_tiles::MIN_TILE_ZOOM`], or when nothing is drawable — each of
    /// which leaves the pane exactly as it is today.
    pub fn frame_for_pane(
        &mut self,
        projection: &RadarProjection,
        projection_generation: Generation,
        lod: LodBucket,
        camera: Camera2D,
        viewport: ViewportMetrics,
        scrim_rgb: [f32; 3],
    ) -> Option<Arc<TileFrame>> {
        let provider = self.provider?;
        self.clock += 1;
        let viewport = viewport.sanitized();
        // The raster stands down once THIS PANE's map stops being flat. A tile
        // mesh is built in radar-local kilometres and cached per projection
        // generation, so it cannot follow the globe morph the vector layer is
        // drawn under. Asked of the pane rather than of the LOD bucket, so a
        // small pane - which reaches the globe at a coarser scale - keeps its
        // imagery for longer.
        if crate::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport) > 0.0
        {
            return None;
        }
        let zoom = tile_zoom_for(lod, projection.radar_lat_deg(), viewport.pixels_per_point)?;
        let zoom = zoom.min(provider.max_zoom());
        if zoom < MIN_TILE_ZOOM {
            return None;
        }
        self.sync_projection(projection_generation);

        let camera = camera.sanitized();
        let view = pane_view(projection, camera, viewport);

        // Saturating the cap means the pane wants more tiles than the budget
        // allows; one coarser zoom quarters the count. Bounded below by
        // MIN_TILE_ZOOM, past which the layer switches off entirely.
        //
        // A ROTATED pane asks for more tiles than it needs, and it is worth
        // saying why rather than leaving it as a mystery. A Web Mercator tile
        // is an axis-aligned rectangle in a north-up frame, and
        // `basemap_tiles::visibility::bounding_tiles` takes the AXIS-ALIGNED
        // bounding box of the pane's projected boundary. Turn the pane and
        // that box grows: by a factor of 2.06 at 32 degrees on a 16:9 pane,
        // 2.17 at 45. Through the loop below the failure is bounded and
        // self-correcting - at most one extra zoom is dropped, and only when
        // the unrotated pane was already asking for close to the cap - so a
        // rotated pane can be one level blurrier than an unrotated one. The
        // fix, when it is worth it, is in `bounding_tiles`: reject candidates
        // outside the true rotated boundary instead of taking a box around it.
        let mut zoom = zoom;
        let mut tiles = visible_tiles(zoom, &view, MAX_TILES_PER_PANE);
        while (tiles.is_empty() || tiles.len() >= MAX_TILES_PER_PANE) && zoom > MIN_TILE_ZOOM {
            zoom -= 1;
            tiles = visible_tiles(zoom, &view, MAX_TILES_PER_PANE);
        }
        if tiles.is_empty() {
            return None;
        }

        let now = Instant::now();
        let mut draws = Vec::with_capacity(tiles.len());
        let mut builds = 0_usize;
        let mut exact = 0_usize;
        let mut fading = false;
        let mut deferred = false;
        for tile in &tiles {
            let Some(mesh) = self.mesh_for(*tile, projection, &mut builds, &mut deferred) else {
                continue;
            };
            let Some((texture, uv_offset_scale, levels_up)) = self.texture_for(provider, *tile)
            else {
                continue;
            };
            if levels_up == 0 {
                exact += 1;
            } else {
                self.metrics.ancestor_substitutions += 1;
            }
            let alpha = self.fade_alpha((provider, texture), now);
            fading |= alpha < 1.0;
            if alpha < 1.0 {
                // ANCESTOR HANDOFF. A texture still fading in must CROSSFADE
                // over whatever coarser picture was covering this ground a
                // frame ago, so the resident ancestor keeps drawing beneath
                // it until the fade finishes. Without this underlay a tile's
                // arrival blinks — ancestor imagery, then bare ground, then
                // the child fading up from nothing — and on a warm cache,
                // where a whole zoom level arrives in one frame, the entire
                // pane flashed to ground at every zoom step (measured by
                // `tests/tile_quickzoom_proof.rs`: painted fraction 1.000 →
                // 0.000 → fade). Drawn first, so the fading texture
                // composites over it.
                //
                // The underlay must itself be OPAQUE, which on a fast flick
                // the nearest ready ancestor is not: three zoom steps inside
                // one fade length leave the intermediate level mid-fade, and
                // an underlay at alpha 0.6 beneath a child at alpha 0.0 lets
                // 40% of bare ground through both (measured by the harness's
                // fast-flick gesture: worst per-tile ground bleed 0.275 at
                // the z10→z11 flip while a fully settled z9 sat resident on
                // the GPU). So the walk prefers the nearest ancestor whose
                // fade has FINISHED and only settles for a fading one when
                // nothing settled is resident at all.
                if let Some((under, under_uv, _)) = underlay_beneath(
                    *tile,
                    levels_up + 1,
                    |t| self.store.state(provider, t),
                    |t| self.fade_settled((provider, t), now),
                ) {
                    let under_alpha = self.fade_alpha((provider, under), now);
                    draws.push(TileDraw {
                        mesh: Arc::clone(&mesh),
                        texture: under,
                        uv_offset_scale: under_uv,
                        alpha: under_alpha,
                    });
                }
            }
            draws.push(TileDraw {
                mesh,
                texture,
                uv_offset_scale,
                alpha,
            });
        }
        self.prefetch_toward_next_zoom(
            provider,
            lod,
            zoom,
            camera,
            &view,
            projection.radar_lat_deg(),
            viewport.pixels_per_point,
        );

        if deferred || fading {
            // More work lands next frame; ask for one rather than waiting on
            // an unrelated repaint.
            (self.repaint)();
        }

        // Everything this frame drew carries the current clock, so it sorts
        // to the front of the LRU and cannot be what a sweep drops.
        self.prune();
        self.metrics.frames_built += 1;
        let scrim_alpha = self.scrim_for_ground(scrim_rgb);
        Some(Arc::new(TileFrame {
            key: TileFrameKey {
                provider,
                projection: projection_generation,
                zoom,
            },
            draws: draws.into(),
            uploads: Arc::clone(&self.uploads),
            attribution: provider.attribution(),
            scrim: [scrim_rgb[0], scrim_rgb[1], scrim_rgb[2], scrim_alpha],
            coverage: exact as f32 / tiles.len() as f32,
            feedback: Arc::clone(&self.feedback),
        }))
    }

    /// Drop every mesh when the projection changes. Textures survive: a
    /// texture is imagery of a place and does not depend on the anchor, which
    /// is a real saving when stepping between neighbouring sites.
    fn sync_projection(&mut self, projection: Generation) {
        if self.mesh_projection != Some(projection) {
            self.meshes.clear();
            self.mesh_projection = Some(projection);
        }
    }

    /// The cached mesh for a tile, building it if there is budget this frame.
    ///
    /// A tile the projection cannot express — a failed geodesic, or a corner
    /// past [`basemap_tiles::MAX_TILE_WORLD_KM`] — is simply not drawn, and
    /// nothing is substituted for it.
    fn mesh_for(
        &mut self,
        tile: TileId,
        projection: &RadarProjection,
        builds: &mut usize,
        deferred: &mut bool,
    ) -> Option<Arc<TileMesh>> {
        if let Some(resident) = self.meshes.get_mut(&tile) {
            resident.last_used = self.clock;
            return Some(Arc::clone(&resident.mesh));
        }
        if *builds >= MAX_MESH_BUILDS_PER_FRAME {
            *deferred = true;
            return None;
        }
        *builds += 1;
        // The FALLIBLE projection, deliberately: `lon_lat_to_world` collapses
        // a non-convergent geodesic onto the anchor, which would staple a tile
        // of the far side of the world to the radar.
        let mesh = build_tile_mesh(tile, |lon, lat| {
            projection
                .try_lon_lat_to_world(lon, lat)
                .map(|world| (world.east_km, world.north_km))
        })?;
        self.metrics.meshes_built += 1;
        let mesh = Arc::new(mesh);
        self.meshes.insert(
            tile,
            ResidentMesh {
                mesh: Arc::clone(&mesh),
                last_used: self.clock,
            },
        );
        Some(mesh)
    }

    /// Evict least-recently-drawn meshes and fade clocks once either map is
    /// over [`MAX_RESIDENT_MESHES`].
    ///
    /// Called at the END of a frame, so everything this frame drew carries the
    /// current clock value and sorts to the front: the working set on screen
    /// is never what gets dropped, which is what leaves the pan invariant
    /// intact. Pruning to [`MESH_PRUNE_TARGET`] rather than exactly to the
    /// bound stops the next tile from triggering another sweep.
    fn prune(&mut self) {
        if self.meshes.len() > MAX_RESIDENT_MESHES {
            let mut stamps: Vec<u64> = self
                .meshes
                .values()
                .map(|resident| resident.last_used)
                .collect();
            let cut = stamps.len() - MESH_PRUNE_TARGET;
            stamps.select_nth_unstable(cut);
            let threshold = stamps[cut];
            let before = self.meshes.len();
            self.meshes
                .retain(|_, resident| resident.last_used >= threshold);
            self.metrics.meshes_evicted += (before - self.meshes.len()) as u64;
        }
        if self.first_seen.len() > MAX_RESIDENT_MESHES {
            let mut stamps: Vec<u64> = self
                .first_seen
                .values()
                .map(|clock| clock.last_used)
                .collect();
            let cut = stamps.len() - MESH_PRUNE_TARGET;
            stamps.select_nth_unstable(cut);
            let threshold = stamps[cut];
            self.first_seen
                .retain(|_, clock| clock.last_used >= threshold);
        }
    }

    /// Which texture draws this tile, the UV window inside it, and how many
    /// levels above the tile that texture sits (0 = the tile's own).
    ///
    /// Requests the exact tile, then falls back up the pyramid. The 404 case
    /// is neither hypothetical nor regional: the USGS shaded-relief service is
    /// missing z9 over KTLX and z9-z11 over KRTX, so a pane there answers 404
    /// on *every* tile and an ancestor is the only thing that draws at all.
    ///
    /// The walk continues past an ancestor that is merely queued or in
    /// flight, because on a quick multi-step zoom that is the common shape:
    /// the *intermediate* zoom's fetches are milliseconds old and Pending,
    /// while the zoom the user came FROM is resident two levels up. Stopping
    /// at the first in-flight ancestor — the previous shape of this function
    /// — showed bare ground over imagery the GPU was already holding. The
    /// walk is [`nearest_ready_ancestor`], a pure lookup: nothing is ever
    /// *requested* for an ancestor unless the exact tile is permanently
    /// [`TileState::Absent`]. Speculatively fetching ancestors for tiles that
    /// are merely still in flight would multiply every cold pane's traffic,
    /// and for OpenStreetMap it would be the "pre-emptive fetching of tiles
    /// other than those a user is actively viewing" its policy forbids.
    fn texture_for(
        &mut self,
        provider: TileProvider,
        tile: TileId,
    ) -> Option<(TileId, [f32; 4], u8)> {
        let state = self.store.request(provider, tile);
        self.wanted.insert((provider, tile));
        if state == TileState::Ready {
            return Some((tile, [0.0, 0.0, 1.0, 1.0], 0));
        }
        let fallback = self.ready_ancestor(provider, tile, 1);
        if state == TileState::Absent {
            // The exact tile will never exist here, so an ancestor is this
            // ground's *final* picture, not a stopgap — keep one ancestor
            // fetch alive, and only for a level that would improve on what is
            // already drawable.
            let drawable = fallback.map_or(MAX_ANCESTOR_LEVELS + 1, |(_, _, level)| level);
            if let Some(ancestor) =
                ancestor_worth_fetching(tile, drawable, |t| self.store.state(provider, t))
            {
                self.store.request(provider, ancestor);
                self.wanted.insert((provider, ancestor));
            }
        }
        fallback
    }

    /// The nearest resident ancestor at or above `from_level` levels up. A
    /// pure lookup — nothing is requested — shared by the fallback path and
    /// by the crossfade underlay.
    fn ready_ancestor(
        &self,
        provider: TileProvider,
        tile: TileId,
        from_level: u8,
    ) -> Option<(TileId, [f32; 4], u8)> {
        nearest_ready_ancestor(tile, from_level, |t| self.store.state(provider, t))
    }

    /// Warm the next tile zoom's centre tiles when the camera is a notch away
    /// from the boundary that will ask for them, so a deliberate zoom-in
    /// lands on imagery that is already decoding rather than on a stretch of
    /// the previous zoom.
    ///
    /// Policy first: the OSMF Standard Tile Layer Usage Policy (s.4) defines
    /// bulk downloading as "any pre-emptive fetching of tiles other than
    /// those a user is actively viewing", so this is gated on
    /// [`TileProvider::prefetch_permitted`] and never runs for OpenStreetMap.
    /// Where it is permitted it is bounded to [`PREFETCH_TILES`] centre tiles
    /// of ONE level — never a ring at every level — and the requests go
    /// through the same `wanted` set as everything else, so the moment the
    /// camera stops flirting with the boundary the next poll cancels them
    /// instead of letting them download. That bound is what keeps this sane
    /// on a phone: at most a handful of extra fetches per gesture, none at
    /// all while the camera is parked mid-bucket, and nothing here asks for a
    /// repaint.
    #[allow(clippy::too_many_arguments)]
    fn prefetch_toward_next_zoom(
        &mut self,
        provider: TileProvider,
        lod: LodBucket,
        zoom: u8,
        camera: Camera2D,
        view: &ViewportGeo,
        anchor_lat_deg: f64,
        pixels_per_point: f32,
    ) {
        if !provider.prefetch_permitted() || zoom >= provider.max_zoom() {
            return;
        }
        // Only from the fine half of the bucket — past the centre, one more
        // wheel notch crosses the boundary — and only when the next bucket
        // actually maps to a deeper tile zoom. Two half-octave buckets share
        // each zoom, so half the bucket boundaries change nothing and warm
        // nothing.
        let bucket_centre = lod.center_scale(LOD_REFERENCE_KM_PER_POINT);
        if !(camera.km_per_point > 0.0 && camera.km_per_point < bucket_centre) {
            return;
        }
        if tile_zoom_for(LodBucket(lod.0 - 1), anchor_lat_deg, pixels_per_point) != Some(zoom + 1) {
            return;
        }
        for tile in visible_tiles(zoom + 1, view, PREFETCH_TILES) {
            if self.store.state(provider, tile) == TileState::Unknown {
                self.metrics.tiles_prefetched += 1;
            }
            self.store.request(provider, tile);
            self.wanted.insert((provider, tile));
        }
    }

    /// Fold one arrived tile into its provider's brightness estimate.
    ///
    /// Read from the smallest mip - 32x32, 1024 texels - which the decoder has
    /// already built, so this is a thousand adds per tile and needs no second
    /// pass over the full image.
    fn sample_luminance(&mut self, decoded: &DecodedTile) {
        let entry = self.luminance.entry(decoded.provider).or_insert((0.0, 0));
        if entry.1 >= LUMINANCE_SAMPLES {
            return;
        }
        let smallest = decoded.mip_level_count().saturating_sub(1);
        let Some((pixels, _side)) = decoded.level(smallest) else {
            return;
        };
        let mut total = 0.0_f32;
        let mut counted = 0_u32;
        for texel in pixels.chunks_exact(4) {
            // Fully transparent no-data is not part of the picture.
            if texel[3] == 0 {
                continue;
            }
            total += luminance_of([
                f32::from(texel[0]) / 255.0,
                f32::from(texel[1]) / 255.0,
                f32::from(texel[2]) / 255.0,
            ]);
            counted += 1;
        }
        if counted == 0 {
            return;
        }
        entry.0 += total / counted as f32;
        entry.1 += 1;
    }

    /// Whether this texture's fade has finished — a pure read: a tile that
    /// has never drawn has no clock and is NOT settled (it would start from
    /// alpha 0), and peeking here must not start one.
    fn fade_settled(&self, key: TileKey, now: Instant) -> bool {
        self.first_seen.get(&key).is_some_and(|clock| {
            now.saturating_duration_since(clock.first_seen)
                .as_secs_f32()
                >= FADE_SECONDS
        })
    }

    fn fade_alpha(&mut self, key: TileKey, now: Instant) -> f32 {
        let clock = self.first_seen.entry(key).or_insert(FadeClock {
            first_seen: now,
            last_used: self.clock,
        });
        clock.last_used = self.clock;
        let elapsed = now
            .saturating_duration_since(clock.first_seen)
            .as_secs_f32();
        (elapsed / FADE_SECONDS).clamp(0.0, 1.0)
    }
}

/// Demote a cache root that cannot actually be written to.
///
/// This is a POLICY check, not a convenience. `TileStore::permits` answers
/// "may this provider be used at all?" with "does this store have a disk
/// cache?", because the disk cache is the only thing enforcing a provider's
/// minimum cache lifetime - the rate limit. `TileDiskCache::new` merely stores
/// a path; it never touches the filesystem, so a read-only home directory, a
/// full disk, a roaming profile that failed to mount, or a `LOCALAPPDATA` that
/// points somewhere unwritable all produce a store that *claims* a disk cache,
/// answers `permits` with `true`, offers OpenStreetMap in the picker, and then
/// re-downloads every tile it is ever asked for again. That is precisely the
/// bulk-download behaviour the OSMF policy blocks clients over.
///
/// So the root is probed once, here, by creating it and writing a byte into
/// it. If that fails the store is configured memory-only, which makes
/// `permits` refuse the provider that needs a persistent cache and leaves the
/// U.S. Government layers working. The probe file is removed either way.
fn usable_cache(mut config: TileCacheConfig) -> TileCacheConfig {
    let Some(root) = config.disk_root.as_ref() else {
        return config;
    };
    let probe = root.join(".write-probe");
    let writable = std::fs::create_dir_all(root).is_ok() && std::fs::write(&probe, b"0").is_ok();
    let _ = std::fs::remove_file(&probe);
    if !writable {
        config.disk_root = None;
    }
    config
}

/// Rec. 709 luminance of a colour, on the same code values everything else in
/// the pane composites in (see `tile_shader.wgsl` on colour space).
fn luminance_of(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// The nearest ancestor of `tile` that `state` reports [`TileState::Ready`],
/// searching `from_level..=MAX_ANCESTOR_LEVELS`, with the tile's UV window
/// inside it and the level it was found at.
///
/// Walks past *every* non-ready state — Absent (coverage holes are not
/// monotonic in zoom, so the parent of a missing tile is often missing too),
/// but also Pending, Failed and Unknown, because an in-flight parent must not
/// hide a resident grandparent. During a quick zoom-in that in-flight parent
/// is the intermediate zoom the camera blew through, and the grandparent is
/// the picture the user was just looking at.
fn nearest_ready_ancestor(
    tile: TileId,
    from_level: u8,
    state: impl Fn(TileId) -> TileState,
) -> Option<(TileId, [f32; 4], u8)> {
    for level in from_level..=MAX_ANCESTOR_LEVELS {
        let ancestor = tile.ancestor(level)?;
        if state(ancestor) == TileState::Ready {
            let uv = tile.uv_offset_scale_within(ancestor)?;
            return Some((ancestor, uv, level));
        }
    }
    None
}

/// The texture to draw BENEATH a tile that is still fading in: the nearest
/// resident ancestor at or above `from_level` whose own fade has finished,
/// falling back to the nearest merely-resident one when nothing settled
/// exists (a translucent floor beats bare ground).
///
/// Preferring the settled ancestor is what makes a fast multi-step zoom a
/// true crossfade. Three zoom steps inside one fade length leave the
/// intermediate level resident but mid-fade; an underlay that is itself at
/// alpha 0.6 lets the pane's ground bleed through the whole stack. The
/// settled level the user came FROM is still resident a level or two higher,
/// and it — not the newest picture — is what must carry the pane until every
/// fade above it finishes.
fn underlay_beneath(
    tile: TileId,
    from_level: u8,
    state: impl Fn(TileId) -> TileState,
    settled: impl Fn(TileId) -> bool,
) -> Option<(TileId, [f32; 4], u8)> {
    nearest_ready_ancestor(tile, from_level, |ancestor| {
        // Ready but mid-fade reads as not-there-yet on the first pass: it
        // cannot be an opaque floor.
        match state(ancestor) {
            TileState::Ready if settled(ancestor) => TileState::Ready,
            TileState::Ready => TileState::Pending,
            other => other,
        }
    })
    .or_else(|| nearest_ready_ancestor(tile, from_level, state))
}

/// For a tile that is permanently [`TileState::Absent`]: the one ancestor
/// worth having in flight, or `None` when nothing above it could improve the
/// picture.
///
/// "Improve" means strictly shallower than `drawable_level`, the level whose
/// texture is already drawing this ground (pass `MAX_ANCESTOR_LEVELS + 1`
/// when nothing draws at all). The first non-absent candidate wins, so there
/// is one outstanding ancestor fetch per hole, never a ladder of them.
fn ancestor_worth_fetching(
    tile: TileId,
    drawable_level: u8,
    state: impl Fn(TileId) -> TileState,
) -> Option<TileId> {
    for level in 1..drawable_level.min(MAX_ANCESTOR_LEVELS + 1) {
        let ancestor = tile.ancestor(level)?;
        match state(ancestor) {
            // A hole above a hole: fetching it again would be a doomed
            // request the provider has already answered.
            TileState::Absent => continue,
            // Already drawing or already moving; nothing to start.
            TileState::Ready => return None,
            _ => return Some(ancestor),
        }
    }
    None
}

/// The pane boundary in geographic coordinates.
///
/// The edge is walked rather than cornered because it is a curve in geographic
/// space: a corner-only bounding box silently drops tiles along the middle of
/// an edge.
fn pane_view(
    projection: &RadarProjection,
    camera: Camera2D,
    viewport: ViewportMetrics,
) -> ViewportGeo {
    ViewportGeo::from_rect_edge(
        (0.0, 0.0),
        (
            f64::from(viewport.width_points),
            f64::from(viewport.height_points),
        ),
        VIEW_EDGE_SAMPLES,
        |x, y| {
            let world = camera.screen_to_world(ScreenPoint::new(x as f32, y as f32), viewport);
            let (lon, lat) = projection.world_to_lon_lat(world);
            (lon.is_finite() && lat.is_finite()).then_some((lon, lat))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::LodSelector;

    use crate::style_presets::MapStylePreset;

    /// KTLX, the site every proof in this workspace uses.
    const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);
    /// KRTX, high enough in latitude to move the zoom table.
    const KRTX: (f64, f64) = (45.7150, -122.9650);

    fn viewport() -> ViewportMetrics {
        ViewportMetrics {
            width_points: 1500.0,
            height_points: 950.0,
            pixels_per_point: 1.0,
        }
    }

    /// A controller that cannot touch the network or the user's cache: no disk
    /// root, offline, so `TileStore` never opens a socket and never starts a
    /// worker thread.
    fn controller() -> TileSceneController {
        TileSceneController::with_config(
            TileCacheConfig {
                disk_root: None,
                max_disk_bytes: 0,
                max_workers: 1,
                user_agent: "radar-workstation-test/0 (+https://example.invalid)".to_owned(),
                offline: true,
            },
            Arc::new(|| {}),
        )
    }

    /// A mesh with no geometry: this file's caches key on the tile and stamp,
    /// never on the vertices, so the sweep tests need no geodesy.
    fn flat_mesh(tile: TileId) -> TileMesh {
        TileMesh {
            tile,
            subdivision: 1,
            vertices: Vec::new(),
            indices: Vec::new(),
            max_error_km: 0.0,
            estimated_bytes: 0,
        }
    }

    fn bucket(km_per_point: f32) -> LodBucket {
        LodSelector::new(km_per_point, LOD_REFERENCE_KM_PER_POINT).current()
    }

    #[test]
    fn the_zoom_table_matches_the_measured_plan() {
        // Every entry here was computed from the bucket CENTRE scale, which is
        // what makes the mapping single-valued.
        let expected = [
            (0.010_f32, 14_u8),
            (0.020, 13),
            (0.040, 12),
            (0.080, 11),
            (0.160, 10),
            (0.320, 9),
            (0.640, 8),
            (1.280, 7),
            (2.560, 6),
            (5.120, 5),
        ];
        for (km_per_point, zoom) in expected {
            assert_eq!(
                tile_zoom_for(bucket(km_per_point), KTLX.0, 1.0),
                Some(zoom),
                "{km_per_point} km/point"
            );
        }
    }

    #[test]
    fn a_continental_camera_switches_the_layer_off_rather_than_clamping() {
        // The crossover is where a z5 texel stops matching a screen pixel:
        // 156543 * cos(35.33) / 32 = 3991 m/texel against the camera's own
        // metres per pixel, rounded, which puts the last drawable bucket
        // centre at 5.6 km/point. A camera at 8 km/point is a pane 12000 km
        // wide, twenty-six times any radar's 460 km reach.
        assert_eq!(tile_zoom_for(bucket(5.12), KTLX.0, 1.0), Some(5));
        assert_eq!(tile_zoom_for(bucket(8.0), KTLX.0, 1.0), None);
        assert_eq!(tile_zoom_for(bucket(20.0), KTLX.0, 1.0), None);
        // A HiDPI display packs twice the pixels into the same points, so the
        // same camera is one zoom sharper and is back inside the range.
        assert_eq!(tile_zoom_for(bucket(8.0), KTLX.0, 2.0), Some(5));
    }

    #[test]
    fn a_hidpi_display_gets_sharper_tiles_rather_than_the_same_ones_magnified() {
        for km_per_point in [0.04_f32, 0.16, 0.64, 2.56] {
            let one = tile_zoom_for(bucket(km_per_point), KTLX.0, 1.0).expect("zoom");
            let two = tile_zoom_for(bucket(km_per_point), KTLX.0, 2.0).expect("zoom");
            assert_eq!(two, one + 1, "{km_per_point} km/point");
        }
    }

    #[test]
    fn latitude_moves_the_table_because_a_mercator_texel_shrinks_poleward() {
        // A Web Mercator texel covers cos(latitude) as much ground, so the
        // same camera needs a coarser zoom the further from the equator the
        // radar is. This is a real difference between sites, not a rounding
        // detail: at the default camera KTLX (35.3N) asks for z9 and KRTX
        // (45.7N) for z8, because 156543*cos(45.7)/350 is 312 texels per
        // pixel against 365 at KTLX.
        let by_latitude: Vec<u8> = [0.0, KTLX.0, KRTX.0, 70.0, 82.0]
            .into_iter()
            .map(|lat| tile_zoom_for(bucket(0.35), lat, 1.0).expect("zoom"))
            .collect();
        assert_eq!(by_latitude, vec![9, 9, 8, 7, 6], "{by_latitude:?}");
        for pair in by_latitude.windows(2) {
            assert!(
                pair[0] >= pair[1],
                "zoom rose with latitude: {by_latitude:?}"
            );
        }
    }

    #[test]
    fn a_pan_and_a_small_zoom_cannot_change_the_tile_zoom() {
        // The structural claim: z is a function of the bucket, so hysteresis
        // is inherited rather than reimplemented.
        let mut selector = LodSelector::new(0.35, LOD_REFERENCE_KM_PER_POINT);
        let first = tile_zoom_for(selector.update(0.35), KTLX.0, 1.0);
        for scale in [0.34_f32, 0.36, 0.33, 0.37, 0.30, 0.40, 0.35] {
            assert_eq!(
                tile_zoom_for(selector.update(scale), KTLX.0, 1.0),
                first,
                "scale {scale} moved the tile zoom"
            );
        }
        assert_eq!(first, Some(9));
    }

    /// The zoom is a *function of the bucket*, which is the whole reason the
    /// thrashing failure mode is structurally impossible here.
    ///
    /// Note what this does NOT claim: that returning to a camera scale returns
    /// the previous zoom. `LodSelector` has 12% hysteresis and is deliberately
    /// path dependent, so a camera that zooms out and back can land in the
    /// neighbouring bucket and therefore on a neighbouring zoom. That is the
    /// hysteresis working, not a defect - and it is bounded, which is what the
    /// second half of this asserts.
    #[test]
    fn the_zoom_is_a_function_of_the_bucket_and_steps_by_one() {
        for step in -12..=12_i16 {
            let lod = LodBucket(step);
            let first = tile_zoom_for(lod, KTLX.0, 1.0);
            assert_eq!(first, tile_zoom_for(lod, KTLX.0, 1.0));
            let Some(first) = first else { continue };
            // Two half-octave buckets share one full-octave tile zoom, so a
            // single bucket step can never move the zoom by more than one.
            if let Some(next) = tile_zoom_for(LodBucket(step + 1), KTLX.0, 1.0) {
                assert!(
                    first >= next && first - next <= 1,
                    "bucket {step} -> {first}, bucket {} -> {next}",
                    step + 1
                );
            }
        }
    }

    #[test]
    fn no_provider_means_no_frame_and_no_requests() {
        let mut controller = controller();
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        assert!(controller.provider().is_none());
        assert!(
            controller
                .frame_for_pane(
                    &projection,
                    Generation::new(1),
                    bucket(0.35),
                    Camera2D::default(),
                    viewport(),
                    [0.0, 0.0, 0.0],
                )
                .is_none()
        );
        assert_eq!(controller.metrics().store.requested, 0);
        assert_eq!(controller.metrics().meshes_built, 0);
    }

    #[test]
    fn a_frame_asks_for_the_visible_tiles_and_draws_none_of_them_cold() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsImageryTopo));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        let frame = controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(0.35),
                Camera2D::default(),
                viewport(),
                [0.05, 0.05, 0.06],
            )
            .expect("frame");
        assert_eq!(frame.key.zoom, 9);
        assert_eq!(frame.key.provider, TileProvider::UsgsImageryTopo);
        // Offline with an empty cache: the tiles are wanted and parked, so the
        // pane shows its own ground and nothing hangs.
        assert!(frame.draws.is_empty());
        assert_eq!(frame.coverage, 0.0);
        assert!(controller.metrics().meshes_built > 0);
        assert!(!frame.attribution.is_empty());
    }

    #[test]
    fn a_pan_reuses_every_mesh_it_has_already_built() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsTopo));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        let camera = Camera2D::default();
        // Enough frames that the first pane's whole tile set is built.
        for _ in 0..8 {
            controller.frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(0.35),
                camera,
                viewport(),
                [0.0; 3],
            );
        }
        let built = controller.metrics().meshes_built;
        assert!(built > 0);

        // A pan of a fraction of a tile: the set is the same, so nothing is
        // built. (A larger pan brings new ground into view, which legitimately
        // needs new meshes - that is the only camera-dependent part of the
        // layer.)
        let nudged = Camera2D {
            center_east_km: 1.0,
            center_north_km: -1.0,
            ..camera
        };
        controller.frame_for_pane(
            &projection,
            Generation::new(1),
            bucket(0.35),
            nudged,
            viewport(),
            [0.0; 3],
        );
        assert_eq!(controller.metrics().meshes_built, built);
    }

    #[test]
    fn a_new_anchor_drops_the_meshes_and_keeps_the_textures() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsTopo));
        let ktlx = RadarProjection::new(KTLX.0, KTLX.1);
        controller.frame_for_pane(
            &ktlx,
            Generation::new(1),
            bucket(0.35),
            Camera2D::default(),
            viewport(),
            [0.0; 3],
        );
        let resident = controller.metrics().meshes_resident;
        assert!(resident > 0);

        let krtx = RadarProjection::new(KRTX.0, KRTX.1);
        controller.frame_for_pane(
            &krtx,
            Generation::new(2),
            bucket(0.35),
            Camera2D::default(),
            viewport(),
            [0.0; 3],
        );
        // Rebuilt for the new anchor, not carried over: every mesh resident
        // now was built after the change.
        assert!(controller.metrics().meshes_built > resident as u64);
    }

    #[test]
    fn offline_with_an_empty_cache_never_fails_a_tile_or_spins_the_queue() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsImagery));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        for _ in 0..50 {
            controller.poll();
            controller.frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(0.35),
                Camera2D::default(),
                viewport(),
                [0.0; 3],
            );
        }
        let metrics = controller.metrics();
        assert_eq!(metrics.store.failed, 0, "offline must never mark a failure");
        assert_eq!(metrics.store.downloaded, 0);
        assert_eq!(metrics.store.bytes_downloaded, 0);
        assert_eq!(metrics.store.in_flight, 0);
    }

    #[test]
    fn the_scrim_follows_the_provider_until_the_operator_pins_one() {
        let mut controller = controller();
        assert_eq!(controller.scrim(), 0.0, "no imagery, no scrim");
        controller.set_provider(Some(TileProvider::UsgsImagery));
        assert_eq!(
            controller.scrim(),
            TileProvider::UsgsImagery.default_scrim_alpha()
        );
        controller.set_scrim(0.5);
        assert_eq!(controller.scrim(), 0.5);
        // Switching provider releases the pin, because the right amount of
        // dimming for an aerial photograph is wrong for a shaded relief.
        controller.set_provider(Some(TileProvider::UsgsShadedRelief));
        assert_eq!(
            controller.scrim(),
            TileProvider::UsgsShadedRelief.default_scrim_alpha()
        );
    }

    /// The scrim brings a bright map and a dark photograph to the SAME
    /// readable luminance over the same ground, which is the whole point.
    ///
    /// The luminances injected here are the ones measured off real rendered
    /// frames over KTLX by `tests/tile_render_proof.rs`, not invented: USGS
    /// Imagery+Topo averages 0.4445 and USGS shaded relief 0.9883.
    #[test]
    fn the_scrim_lands_bright_and_dark_imagery_at_one_readable_luminance() {
        let slate = MapStylePreset::Slate.chrome().canvas;
        let ground = [slate.r, slate.g, slate.b];
        let ground_luma = luminance_of(ground);

        for (provider, measured) in [
            (TileProvider::UsgsImageryTopo, 0.4445_f32),
            (TileProvider::UsgsShadedRelief, 0.9883),
        ] {
            let mut controller = controller();
            controller.set_provider(Some(provider));
            controller.luminance.insert(provider, (measured, 1));
            let alpha = controller.scrim_for_ground(ground);
            let drawn = measured * (1.0 - alpha) + ground_luma * alpha;
            assert!(
                (drawn - 0.36).abs() < 0.03,
                "{provider:?}: scrim {alpha} leaves the imagery at {drawn}"
            );
        }

        // And the provider whose suggested value the plan was tuned on gets
        // exactly that value back, so this corrects the table without
        // overriding the one entry that was already right.
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsImageryTopo));
        controller
            .luminance
            .insert(TileProvider::UsgsImageryTopo, (0.4445, 1));
        assert_eq!(
            controller.scrim_for_ground(ground),
            TileProvider::UsgsImageryTopo.default_scrim_alpha()
        );
    }

    /// A light look needs the imagery dimmed towards ITS ground, not towards
    /// black, or Daylight would be the one preset the basemap fights.
    #[test]
    fn a_light_look_dims_far_less_than_a_dark_one() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsShadedRelief));
        controller
            .luminance
            .insert(TileProvider::UsgsShadedRelief, (0.9883, 1));
        let slate = MapStylePreset::Slate.chrome().canvas;
        let daylight = MapStylePreset::Daylight.chrome().canvas;
        let dark = controller.scrim_for_ground([slate.r, slate.g, slate.b]);
        let light = controller.scrim_for_ground([daylight.r, daylight.g, daylight.b]);
        assert!(
            light < dark,
            "a light pane dimmed {light} against a dark pane's {dark}"
        );
        assert!(dark >= 0.6, "a white relief on a black pane needs {dark}");
    }

    /// Never below what the provider asked for: this can only correct the
    /// table upwards, so a provider that knows it needs heavy dimming keeps it
    /// even before any of its imagery has been measured.
    #[test]
    fn the_computed_scrim_never_undercuts_the_provider() {
        let slate = MapStylePreset::Slate.chrome().canvas;
        let ground = [slate.r, slate.g, slate.b];
        for provider in TileProvider::ALL {
            let mut controller = controller();
            controller.set_provider(Some(provider));
            // Before anything arrives.
            assert_eq!(
                controller.scrim_for_ground(ground),
                provider.default_scrim_alpha()
            );
            // And with imagery darker than the ground itself.
            controller.luminance.insert(provider, (0.01, 1));
            assert_eq!(
                controller.scrim_for_ground(ground),
                provider.default_scrim_alpha()
            );
        }
    }

    #[test]
    fn the_frame_carries_the_scrim_colour_the_pane_asked_for() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsImagery));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        let frame = controller
            .frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(0.35),
                Camera2D::default(),
                viewport(),
                [0.1, 0.2, 0.3],
            )
            .expect("frame");
        assert_eq!(frame.scrim[0], 0.1);
        assert_eq!(frame.scrim[1], 0.2);
        assert_eq!(frame.scrim[2], 0.3);
        assert_eq!(
            frame.scrim[3],
            TileProvider::UsgsImagery.default_scrim_alpha()
        );
    }

    #[test]
    fn feedback_round_trips_uploads_and_evictions() {
        let feedback = TileFeedback::default();
        let tile = TileId::new(9, 117, 202).expect("tile");
        feedback.record_upload((TileProvider::UsgsTopo, tile));
        feedback.record_eviction((TileProvider::UsgsTopo, tile));
        let (uploaded, evicted) = feedback.take();
        assert_eq!(uploaded, vec![(TileProvider::UsgsTopo, tile)]);
        assert_eq!(evicted, vec![(TileProvider::UsgsTopo, tile)]);
        let (uploaded, evicted) = feedback.take();
        assert!(uploaded.is_empty() && evicted.is_empty(), "take must drain");
    }

    #[test]
    fn an_eviction_forgets_the_tile_so_it_is_fetched_again() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsTopo));
        let tile = TileId::new(9, 117, 202).expect("tile");
        controller
            .feedback
            .record_eviction((TileProvider::UsgsTopo, tile));
        controller.poll();
        assert_eq!(controller.metrics().textures_forgotten, 1);
        assert_eq!(
            controller.store.state(TileProvider::UsgsTopo, tile),
            TileState::Unknown
        );
    }

    /// Cancellation is wired through `wanted`, and nothing else at this level
    /// tests it: `TileStore::retain` has its own test inside the core crate,
    /// but what this layer HANDS it is the union of the panes' tile sets from
    /// the previous frame, and getting that wrong is silent. Too small a set
    /// cancels tiles that are still on screen every single frame; too large a
    /// set never cancels a fast pan's stale work at all, which for
    /// OpenStreetMap is the bulk downloading its policy blocks accounts for.
    #[test]
    fn the_wanted_set_is_this_frames_tiles_and_is_cleared_by_the_poll_that_uses_it() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsImageryTopo));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        let frame = |controller: &mut TileSceneController, east_km: f64| {
            controller.frame_for_pane(
                &projection,
                Generation::new(1),
                bucket(0.35),
                Camera2D {
                    center_east_km: east_km,
                    ..Camera2D::default()
                },
                viewport(),
                [0.0; 3],
            );
        };

        frame(&mut controller, 0.0);
        let here: HashSet<TileKey> = controller.wanted.clone();
        assert!(
            here.len() >= 16,
            "a pane asked for only {} tiles, so nothing would be kept alive",
            here.len()
        );
        assert!(
            here.iter().all(|(provider, tile)| {
                *provider == TileProvider::UsgsImageryTopo && tile.z == 9
            }),
            "the wanted set names tiles the pane is not drawing"
        );

        // The poll that hands the set to `retain` must also empty it, or the
        // set grows for the life of the session and cancels nothing.
        controller.poll();
        assert!(
            controller.wanted.is_empty(),
            "{} tiles were carried into the next frame's cancellation set",
            controller.wanted.len()
        );

        // A pan of 800 km leaves none of the old view wanted, which is what
        // makes the stale work cancellable.
        frame(&mut controller, 800.0);
        let there = &controller.wanted;
        assert!(!there.is_empty());
        assert!(
            there.is_disjoint(&here),
            "{} tiles from the abandoned view are still being kept alive",
            there.intersection(&here).count()
        );
    }

    /// The ancestor walk must pass an ancestor that is merely in flight and
    /// reach one that is resident.
    ///
    /// This is the quick-zoom uniformity property: two LOD steps inside a
    /// second leave the intermediate zoom Pending, and the zoom the user came
    /// from resident two levels up. The previous walk stopped at the first
    /// Pending ancestor and drew bare ground over imagery the GPU held.
    #[test]
    fn the_ancestor_walk_passes_an_in_flight_parent_to_reach_a_resident_grandparent() {
        let tile = TileId::new(11, 468, 809).expect("tile");
        let parent = tile.ancestor(1).expect("parent");
        let grandparent = tile.ancestor(2).expect("grandparent");

        let mid_zoom_in_flight = |t: TileId| {
            if t == parent {
                TileState::Pending
            } else if t == grandparent {
                TileState::Ready
            } else {
                TileState::Unknown
            }
        };
        let (texture, uv, level) =
            nearest_ready_ancestor(tile, 1, mid_zoom_in_flight).expect("the grandparent draws");
        assert_eq!(texture, grandparent);
        assert_eq!(level, 2);
        assert_eq!(uv, tile.uv_offset_scale_within(grandparent).expect("uv"));

        // Nothing resident anywhere really is nothing to draw.
        assert!(nearest_ready_ancestor(tile, 1, |_| TileState::Pending).is_none());
        // And the walk is bounded: a texture deeper than MAX_ANCESTOR_LEVELS
        // is a blur, not a picture, so it is never chosen.
        let only_the_root = |t: TileId| {
            if t.z < tile.z - MAX_ANCESTOR_LEVELS {
                TileState::Ready
            } else {
                TileState::Unknown
            }
        };
        assert!(nearest_ready_ancestor(tile, 1, only_the_root).is_none());

        // `from_level` starts the walk deeper, which is what the crossfade
        // underlay uses to find the picture UNDER the one that is fading in.
        let both_resident = |t: TileId| {
            if t == parent || t == grandparent {
                TileState::Ready
            } else {
                TileState::Unknown
            }
        };
        assert_eq!(
            nearest_ready_ancestor(tile, 1, both_resident).map(|(t, _, _)| t),
            Some(parent)
        );
        assert_eq!(
            nearest_ready_ancestor(tile, 2, both_resident).map(|(t, _, _)| t),
            Some(grandparent)
        );
    }

    /// The underlay beneath a fading tile must be the settled ancestor, not
    /// the nearest one.
    ///
    /// REGRESSION, measured by the harness's fast-flick gesture before this
    /// choice existed: three zoom steps in ~320 ms leave the intermediate
    /// level resident but mid-fade, and using it as the underlay let 0.275
    /// of bare ground bleed through the composite at the z10→z11 flip while
    /// a fully settled z9 sat resident on the GPU.
    #[test]
    fn the_underlay_beneath_a_fading_tile_is_the_settled_ancestor() {
        let tile = TileId::new(11, 468, 809).expect("tile");
        let parent = tile.ancestor(1).expect("parent");
        let grandparent = tile.ancestor(2).expect("grandparent");
        let both_ready = |t: TileId| {
            if t == parent || t == grandparent {
                TileState::Ready
            } else {
                TileState::Unknown
            }
        };

        // The parent is mid-fade, the grandparent settled: the grandparent
        // carries the pane.
        assert_eq!(
            underlay_beneath(tile, 1, both_ready, |t| t == grandparent).map(|(t, _, _)| t),
            Some(grandparent)
        );
        // Both settled: the nearest wins, as always.
        assert_eq!(
            underlay_beneath(tile, 1, both_ready, |_| true).map(|(t, _, _)| t),
            Some(parent)
        );
        // Nothing settled anywhere: a translucent floor beats bare ground, so
        // the nearest resident ancestor still draws.
        assert_eq!(
            underlay_beneath(tile, 1, both_ready, |_| false).map(|(t, _, _)| t),
            Some(parent)
        );
        // Nothing resident at all really is nothing to draw.
        assert!(underlay_beneath(tile, 1, |_| TileState::Pending, |_| true).is_none());
    }

    /// A permanently absent tile keeps exactly one ancestor fetch alive, and
    /// only for a level that would improve on what already draws.
    #[test]
    fn a_permanent_hole_keeps_exactly_one_useful_ancestor_fetch_alive() {
        let tile = TileId::new(11, 468, 809).expect("tile");
        let parent = tile.ancestor(1).expect("parent");
        let grandparent = tile.ancestor(2).expect("grandparent");

        // The parent is a hole too (coverage is not monotonic in zoom): the
        // grandparent is the one worth having in flight.
        let parent_absent = |t: TileId| {
            if t == parent {
                TileState::Absent
            } else {
                TileState::Unknown
            }
        };
        assert_eq!(
            ancestor_worth_fetching(tile, MAX_ANCESTOR_LEVELS + 1, parent_absent),
            Some(grandparent)
        );
        // Something already draws at level 2, and the only thing shallower is
        // the absent parent: nothing to fetch.
        assert_eq!(ancestor_worth_fetching(tile, 2, parent_absent), None);

        // The parent is merely in flight: it IS the upgrade to keep alive.
        let parent_pending = |t: TileId| {
            if t == parent {
                TileState::Pending
            } else {
                TileState::Unknown
            }
        };
        assert_eq!(
            ancestor_worth_fetching(tile, 3, parent_pending),
            Some(parent)
        );
    }

    /// Prefetch fires only from the fine half of a bucket that borders a
    /// deeper tile zoom, warms at most [`PREFETCH_TILES`] tiles of exactly
    /// one level, and keeps them in `wanted` so a camera that leaves cancels
    /// them.
    #[test]
    fn the_next_zoom_is_warmed_only_near_its_boundary_and_only_a_handful() {
        let mut controller = controller();
        controller.set_provider(Some(TileProvider::UsgsImageryTopo));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        // A bucket whose own centre maps to z9 while the next finer bucket
        // maps to z10 — the last bucket before the boundary.
        let lod = (-24..24_i16)
            .map(LodBucket)
            .find(|bucket| {
                tile_zoom_for(*bucket, KTLX.0, 1.0) == Some(9)
                    && tile_zoom_for(LodBucket(bucket.0 - 1), KTLX.0, 1.0) == Some(10)
            })
            .expect("a bucket bordering the z9/z10 boundary");
        let centre = lod.center_scale(LOD_REFERENCE_KM_PER_POINT);
        let frame = |controller: &mut TileSceneController, km_per_point: f32| {
            controller.poll();
            controller.frame_for_pane(
                &projection,
                Generation::new(1),
                lod,
                Camera2D {
                    km_per_point,
                    ..Camera2D::default()
                },
                viewport(),
                [0.0; 3],
            );
        };

        // Coarse half of the bucket: parked, nothing warmed.
        frame(&mut controller, centre * 1.05);
        assert!(
            controller.wanted.iter().all(|(_, tile)| tile.z == 9),
            "a parked camera warmed tiles it is not near"
        );
        assert_eq!(controller.metrics().tiles_prefetched, 0);

        // Fine half, a notch from the boundary: the next zoom's centre is
        // warmed, bounded, and one level deep only.
        frame(&mut controller, centre * 0.9);
        let warmed: Vec<TileId> = controller
            .wanted
            .iter()
            .filter(|(_, tile)| tile.z == 10)
            .map(|(_, tile)| *tile)
            .collect();
        assert!(!warmed.is_empty(), "nothing was warmed at the boundary");
        assert!(
            warmed.len() <= PREFETCH_TILES,
            "{} tiles warmed, which is a speculative fetch storm",
            warmed.len()
        );
        assert_eq!(controller.metrics().tiles_prefetched, warmed.len() as u64);
        assert!(
            controller.wanted.iter().all(|(_, tile)| tile.z <= 10),
            "prefetch reached deeper than one level"
        );
        // The warmed tiles are the CENTRE of the next zoom: every one of them
        // is a child of a tile the pane is looking at now.
        assert!(
            warmed.iter().all(|tile| {
                controller.wanted.contains(&(
                    TileProvider::UsgsImageryTopo,
                    tile.ancestor(1).expect("parent"),
                ))
            }),
            "a warmed tile is outside the current view"
        );
    }

    /// The OpenStreetMap tile usage policy forbids "any pre-emptive fetching
    /// of tiles other than those a user is actively viewing", so for that
    /// provider the boundary must warm NOTHING — with a fully working disk
    /// cache, at the exact camera that warms USGS.
    #[test]
    fn openstreetmap_is_never_prefetched() {
        let root = std::env::temp_dir().join(format!(
            "map-scene-osm-prefetch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut controller = TileSceneController::with_config(
            TileCacheConfig {
                disk_root: Some(root.clone()),
                max_disk_bytes: 8 * 1024 * 1024,
                max_workers: 1,
                user_agent: "radar-workstation-test/0 (+https://example.invalid)".to_owned(),
                offline: true,
            },
            Arc::new(|| {}),
        );
        controller.set_provider(Some(TileProvider::OpenStreetMap));
        assert!(controller.permits(TileProvider::OpenStreetMap));
        let projection = RadarProjection::new(KTLX.0, KTLX.1);
        let lod = (-24..24_i16)
            .map(LodBucket)
            .find(|bucket| {
                tile_zoom_for(*bucket, KTLX.0, 1.0) == Some(9)
                    && tile_zoom_for(LodBucket(bucket.0 - 1), KTLX.0, 1.0) == Some(10)
            })
            .expect("a bucket bordering the z9/z10 boundary");
        let centre = lod.center_scale(LOD_REFERENCE_KM_PER_POINT);
        controller.poll();
        controller.frame_for_pane(
            &projection,
            Generation::new(1),
            lod,
            Camera2D {
                km_per_point: centre * 0.9,
                ..Camera2D::default()
            },
            viewport(),
            [0.0; 3],
        );
        assert!(
            controller.wanted.iter().all(|(_, tile)| tile.z == 9),
            "the policy-restricted provider was prefetched"
        );
        assert_eq!(controller.metrics().tiles_prefetched, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mesh sweep must drop the LEAST recently drawn tiles and keep the
    /// working set that is on screen.
    ///
    /// Written after the integration test in `tests/tile_pressure.rs` failed to
    /// notice an eviction policy inverted to keep the OLDEST meshes: a pan long
    /// enough to fill the cache leaves it under the bound again after one
    /// sweep, so the parked camera it then measures never triggers another one.
    /// The policy is deterministic, so it is asserted directly instead of being
    /// inferred from a camera.
    #[test]
    fn the_mesh_sweep_drops_the_least_recently_drawn_and_keeps_the_working_set() {
        let mut controller = controller();
        let stale = MAX_RESIDENT_MESHES as u32 + 2_000;
        for index in 0..stale {
            let tile = TileId::new(16, index, 30_000).expect("tile");
            controller.meshes.insert(
                tile,
                ResidentMesh {
                    mesh: Arc::new(flat_mesh(tile)),
                    last_used: u64::from(index),
                },
            );
        }
        // The tiles on screen right now: drawn this frame, so newest.
        controller.clock = u64::from(stale) + 1;
        let visible: Vec<TileId> = (0..64)
            .map(|index| TileId::new(16, 50_000 + index, 30_000).expect("tile"))
            .collect();
        for tile in &visible {
            controller.meshes.insert(
                *tile,
                ResidentMesh {
                    mesh: Arc::new(flat_mesh(*tile)),
                    last_used: controller.clock,
                },
            );
        }

        controller.prune();

        assert!(
            controller.meshes.len() <= MAX_RESIDENT_MESHES,
            "the sweep left {} meshes",
            controller.meshes.len()
        );
        assert!(
            controller.metrics().meshes_evicted > 0,
            "nothing was evicted, so this proves nothing"
        );
        for tile in &visible {
            assert!(
                controller.meshes.contains_key(tile),
                "the sweep dropped {tile:?}, which is on screen: it would be rebuilt \
                 immediately and every frame after"
            );
        }
        // And what went is the oldest, not an arbitrary slice.
        assert!(
            !controller
                .meshes
                .contains_key(&TileId::new(16, 0, 30_000).expect("tile")),
            "the least recently drawn mesh survived the sweep"
        );
    }

    /// The fade clock is the second map that grows one entry per tile drawn,
    /// and it is bounded by the same sweep as the mesh cache.
    ///
    /// It cannot be filled without imagery - a tile with no texture is never
    /// drawn and never faded - so it is filled directly here. What matters is
    /// the policy: the bound holds, and the entries kept are the ones most
    /// recently drawn, because pruning a tile that is still on screen would
    /// restart its fade and flicker it under the viewer.
    #[test]
    fn the_fade_clock_is_bounded_and_keeps_the_tiles_that_are_still_drawing() {
        let mut controller = controller();
        let now = Instant::now();
        // Two thousand tiles the camera has left behind, then the working set
        // on screen now, stamped with the current clock.
        for index in 0..(MAX_RESIDENT_MESHES as u32 + 2_000) {
            let tile = TileId::new(16, index, 20_000).expect("tile");
            controller.first_seen.insert(
                (TileProvider::UsgsTopo, tile),
                FadeClock {
                    first_seen: now,
                    last_used: u64::from(index),
                },
            );
        }
        controller.clock = u64::from(MAX_RESIDENT_MESHES as u32 + 2_000);
        let newest = TileId::new(16, 40_000, 20_000).expect("tile");
        controller.first_seen.insert(
            (TileProvider::UsgsTopo, newest),
            FadeClock {
                first_seen: now,
                last_used: controller.clock,
            },
        );

        controller.prune();

        let tracked = controller.metrics().fade_clocks_tracked;
        assert!(
            tracked <= MAX_RESIDENT_MESHES,
            "the fade clock holds {tracked} entries"
        );
        assert!(
            controller
                .first_seen
                .contains_key(&(TileProvider::UsgsTopo, newest)),
            "the most recently drawn tile was pruned, which would restart its fade"
        );
        // And the oldest is exactly what went.
        let oldest = TileId::new(16, 0, 20_000).expect("tile");
        assert!(
            !controller
                .first_seen
                .contains_key(&(TileProvider::UsgsTopo, oldest)),
            "the least recently drawn tile survived the sweep"
        );
    }

    /// A cache root that cannot be written to must demote the store to
    /// memory-only, because `TileStore::permits` reads "is there a disk
    /// cache?" as "may a provider that requires a persistent cache be used?".
    ///
    /// The unwritable root here is a directory path underneath a regular file,
    /// which `create_dir_all` refuses on every platform this ships to.
    #[test]
    fn a_cache_root_that_cannot_be_written_is_demoted_to_memory_only() {
        let file = std::env::temp_dir().join(format!(
            "map-scene-unwritable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&file, b"not a directory").expect("write the blocking file");
        let root = file.join("basemap-tiles");

        let controller = TileSceneController::with_config(
            TileCacheConfig {
                disk_root: Some(root),
                max_disk_bytes: 8 * 1024 * 1024,
                max_workers: 1,
                user_agent: "radar-workstation-test/0 (+https://example.invalid)".to_owned(),
                offline: false,
            },
            Arc::new(|| {}),
        );
        assert!(
            controller.cache_root().is_none(),
            "an unwritable root was accepted as a disk cache"
        );
        // And the consequence that matters: the provider whose terms require a
        // persistent cache is refused rather than offered and then abused.
        assert!(
            !controller.permits(TileProvider::OpenStreetMap),
            "OpenStreetMap was permitted with no working cache to rate limit it"
        );
        // The U.S. Government layers carry no such condition and still work.
        assert!(controller.permits(TileProvider::UsgsImagery));

        let _ = std::fs::remove_file(&file);
    }

    /// A writable root is left exactly as configured, so the demotion above
    /// cannot be silently disabling the disk cache for everybody.
    #[test]
    fn a_writable_cache_root_is_kept_and_permits_every_provider() {
        let root = std::env::temp_dir().join(format!(
            "map-scene-writable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let controller = TileSceneController::with_config(
            TileCacheConfig {
                disk_root: Some(root.clone()),
                max_disk_bytes: 8 * 1024 * 1024,
                max_workers: 1,
                user_agent: "radar-workstation-test/0 (+https://example.invalid)".to_owned(),
                offline: true,
            },
            Arc::new(|| {}),
        );
        assert_eq!(controller.cache_root(), Some(root.as_path()));
        for provider in TileProvider::ALL {
            assert!(controller.permits(provider), "{provider:?}");
        }
        // The probe must not leave anything behind in the user's cache.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .expect("the probe created the root")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "the probe left {leftovers:?} behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The computed scrim can never exceed its own ceiling, whatever a
    /// provider asks for, and `f32::clamp` panics if a floor above the ceiling
    /// ever reaches it - inside a paint, which would take the window with it.
    #[test]
    fn the_computed_scrim_stays_inside_its_ceiling_for_every_provider() {
        let slate = MapStylePreset::Slate.chrome().canvas;
        let ground = [slate.r, slate.g, slate.b];
        for provider in TileProvider::ALL {
            let mut controller = controller();
            controller.set_provider(Some(provider));
            for luminance in [0.0_f32, 0.01, 0.25, 0.5, 0.75, 0.99, 1.0] {
                controller.luminance.insert(provider, (luminance, 1));
                let alpha = controller.scrim_for_ground(ground);
                assert!(
                    (0.0..=MAX_SCRIM).contains(&alpha),
                    "{provider:?} at luminance {luminance} asked for {alpha}"
                );
            }
        }
    }
}
