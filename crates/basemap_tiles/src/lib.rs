//! Raster basemap tiles for a radar-local scene.
//!
//! This crate is the CPU half of the tile basemap: Web Mercator arithmetic,
//! the provider table and its attribution obligations, mesh generation into
//! radar-local kilometres, and the fetch / decode / disk-cache pipeline. It
//! contains no GPU code and no UI toolkit, so all of it is testable without a
//! display.
//!
//! # The standards this implements
//!
//! * **Web Mercator / EPSG:3857** — "WGS 84 / Pseudo-Mercator", the projection
//!   every XYZ tile service on the public web publishes in. See Snyder, J.P.
//!   (1987), *Map Projections — A Working Manual*, USGS Professional Paper
//!   1395, pp. 38-47 for the Mercator formulae, and the EPSG Geodetic
//!   Parameter Dataset entry for EPSG:3857 for the spherical-development
//!   convention that makes it *pseudo*-Mercator.
//! * **The slippy-map z/x/y tile scheme** — origin at the north-west corner,
//!   `x` increasing east, `y` increasing *south*, `2^z` tiles per axis, 256
//!   texels per tile. Documented at
//!   <https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames>. The
//!   latitude limit of [`WEB_MERCATOR_LAT_LIMIT_DEG`] is the latitude at which
//!   the Mercator development of a sphere of radius `a` produces a square
//!   world, i.e. `atan(sinh(pi))`.
//! * **The radar-local frame** these tiles are drawn into is a geodesic
//!   azimuthal-equidistant projection (Snyder 1987, pp. 191-202) evaluated
//!   with Vincenty, T. (1975), "Direct and inverse solutions of geodesics on
//!   the ellipsoid with application of nested equations", *Survey Review*
//!   23(176), 88-93. This crate never names that projection: callers hand
//!   [`build_tile_mesh`] a closure, so the tile code cannot learn what a radar
//!   is.
//!
//! # Why a tile is not a rectangle here
//!
//! A tile is an axis-aligned rectangle in EPSG:3857 and a curved quadrilateral
//! in the radar frame. Drawing it as two textured triangles — projecting only
//! the four corners, which is what most viewers do — was measured against the
//! true projection and is wrong by up to tens of screen pixels at continental
//! zoom. [`build_tile_mesh`] therefore subdivides adaptively until the
//! piecewise-affine surface is within [`SUBDIVISION_TOLERANCE_TEXELS`] of the
//! truth, and records what it achieved in [`TileMesh::max_error_km`] so a test
//! can assert the bound instead of trusting it.
//!
//! The subdivision depends on the tile and the projection and on *nothing
//! else* — no camera centre, no camera scale — so a mesh is cacheable on
//! `(TileId, projection identity)` and a pan reuses it exactly as a pan reuses
//! a retained vertex buffer today.

mod cache;
mod decode;
mod mesh;
mod provider;
mod store;
mod tile_math;
mod visibility;

pub use decode::DecodedTile;
pub use mesh::{TileMesh, TileVertex, build_tile_mesh};
pub use provider::TileProvider;
pub use store::{
    TileCacheConfig, TileState, TileStore, TileStoreMetrics, default_cache_dir, default_user_agent,
};
pub use tile_math::{
    TileId, ground_resolution_m_per_texel, lon_lat_to_tile_xy, tile_xy_to_lon_lat,
    zoom_for_ground_resolution,
};
pub use visibility::{ViewportGeo, visible_tiles};

/// Texels per axis in one tile. Every provider in [`TileProvider`] serves this
/// and nothing else; a response of any other size is rejected before the
/// decoder allocates.
pub const TILE_TEXELS: u32 = 256;

/// Coarsest tile zoom the layer will draw.
///
/// Below this the adaptive mesh needs 16x16 subdivision to stay sub-pixel — a
/// 1569-geodesic-evaluation tile on the least useful zoom — and the camera is
/// wider than 5 km/point anyway, i.e. a pane several times wider than any
/// radar's 460 km footprint, where the vector coastline is the better picture.
/// Coarser than this the tile layer switches *off* rather than clamping, which
/// is the deliberate degrade path back to today's vector-only behaviour.
pub const MIN_TILE_ZOOM: u8 = 5;

/// Finest tile zoom the layer will draw. The USGS services publish 24 levels
/// (z0-z23) and the OpenStreetMap standard layer publishes to z19; this cap is
/// ours, chosen because a NEXRAD gate is 250 m and z16 is already 2.4 m/texel
/// at mid-latitudes.
pub const MAX_TILE_ZOOM: u8 = 16;

/// `atan(sinh(pi))` in degrees: the latitude at which the Web Mercator
/// development closes into a square world. Nothing outside this can be
/// addressed by a tile index.
pub const WEB_MERCATOR_LAT_LIMIT_DEG: f64 = 85.051_128_779_806_59;

/// Hard ceiling on adaptive subdivision: an `N x N` grid of cells, so
/// `(N+1)^2` vertices and `2 N^2` triangles.
pub const MAX_SUBDIVISION: u32 = 8;

/// Subdivision target, expressed in the tile's *own* texels so it means the
/// same thing at every zoom and latitude. 0.25 texel, against a worst-case
/// magnification of about 1.77x inside one LOD bucket, keeps the residual
/// under half a screen pixel.
pub const SUBDIVISION_TOLERANCE_TEXELS: f64 = 0.25;

/// A tile whose geometry lands further than this from the projection anchor is
/// dropped rather than drawn. Past roughly this distance an azimuthal-
/// equidistant frame is no longer a picture of anything.
pub const MAX_TILE_WORLD_KM: f64 = 9_000.0;

/// How far the scene layer should walk up the tile pyramid looking for a
/// texture to stand in for one that is missing.
///
/// Four levels is a sixteenfold magnification, which is the point at which a
/// stand-in stops being a coarse picture and starts being a blur. Past it,
/// draw nothing and let the vector basemap show through.
///
/// This is sized against measured coverage, not taste. The USGS shaded-relief
/// service is missing zooms 9, 10 and 11 over KRTX, so a z11 view has to reach
/// z8 — three levels. It is missing 14 through 16 over KTLX, KRTX and TJUA, so
/// a z16 view has to reach z13 — three levels again.
pub const MAX_ANCESTOR_LEVELS: u8 = 4;

/// Levels in the CPU-built mip chain: 256, 128, 64, 32.
///
/// Not optional. One LOD bucket admits a 2.55x sweep of camera scale, so a
/// tile is minified by up to 2.03x while still inside the bucket that selected
/// it. Without mips that shimmers visibly during a zoom.
pub const MIP_LEVELS: u32 = 4;

/// Ceiling on an encoded tile body, applied before anything is read into
/// memory. Real tiles measure 4-40 KiB; this bounds a corrupt cache file or a
/// hostile response.
pub const MAX_TILE_ENCODED_BYTES: usize = 4 * 1024 * 1024;
