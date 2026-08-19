//! Web Mercator (EPSG:3857) and slippy-map z/x/y arithmetic.
//!
//! References: Snyder, J.P. (1987), *Map Projections — A Working Manual*, USGS
//! Professional Paper 1395, pp. 38-47 (Mercator); EPSG:3857 "WGS 84 /
//! Pseudo-Mercator"; <https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames>
//! for the tile indexing convention.
//!
//! The scheme is a *spherical* Mercator on a sphere of the WGS84 semi-major
//! axis. That is a deliberate property of EPSG:3857, not an approximation this
//! module chose: the tiles were rendered that way, so reading them back any
//! other way would misregister the imagery. The ellipsoidal geodesy lives on
//! the other side of the projection closure handed to
//! [`crate::build_tile_mesh`].

use crate::{MAX_TILE_ZOOM, TILE_TEXELS, WEB_MERCATOR_LAT_LIMIT_DEG};

/// WGS84 semi-major axis in metres, the sphere radius EPSG:3857 develops.
const EARTH_RADIUS_M: f64 = 6_378_137.0;

/// Ground metres per texel at the equator, zoom 0: `2 pi a / 256`.
/// The familiar 156543.03392804097.
pub(crate) const EQUATOR_M_PER_TEXEL_Z0: f64 =
    2.0 * std::f64::consts::PI * EARTH_RADIUS_M / TILE_TEXELS as f64;

/// One tile in the slippy-map scheme.
///
/// `y` increases *southward* from the north-west corner of the world, which is
/// the one detail that silently flips a basemap upside down when it is got
/// wrong. Note also that the ArcGIS REST tile path orders the components
/// `{z}/{row}/{col}` — that is z/y/x, not z/x/y; see
/// [`crate::TileProvider::tile_url`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    /// `None` if `z` exceeds [`MAX_TILE_ZOOM`] or `x`/`y` fall outside
    /// `0..2^z`.
    #[must_use]
    pub fn new(z: u8, x: u32, y: u32) -> Option<Self> {
        if z > MAX_TILE_ZOOM {
            return None;
        }
        let span = tiles_per_axis(z);
        (x < span && y < span).then_some(Self { z, x, y })
    }

    /// The tile containing a geographic position, or `None` above the Web
    /// Mercator latitude limit (or for a non-finite input).
    ///
    /// Longitude wraps, because the world is a cylinder here; latitude does
    /// not, because outside the limit there is no tile to name.
    #[must_use]
    pub fn containing(lon_deg: f64, lat_deg: f64, z: u8) -> Option<Self> {
        if z > MAX_TILE_ZOOM || !lon_deg.is_finite() || !lat_deg.is_finite() {
            return None;
        }
        if lat_deg.abs() > WEB_MERCATOR_LAT_LIMIT_DEG {
            return None;
        }
        let (fx, fy) = lon_lat_to_tile_xy(lon_deg, lat_deg, z);
        let span = tiles_per_axis(z);
        let x = wrap_tile_index(fx.floor(), span);
        // `fy` is already inside `0..=span` because of the latitude guard, but
        // the floor of exactly `span` would index one past the last row.
        let y = (fy.floor().max(0.0) as u32).min(span - 1);
        Some(Self { z, x, y })
    }

    #[must_use]
    pub fn parent(self) -> Option<Self> {
        self.ancestor(1)
    }

    /// The tile `levels` zooms coarser that contains this one. `levels == 0`
    /// is the identity; `None` once it would walk past z0.
    #[must_use]
    pub fn ancestor(self, levels: u8) -> Option<Self> {
        if levels == 0 {
            return Some(self);
        }
        if levels > self.z {
            return None;
        }
        Some(Self {
            z: self.z - levels,
            x: self.x >> levels,
            y: self.y >> levels,
        })
    }

    /// `[u_offset, v_offset, u_scale, v_scale]` mapping this tile's `0..1` UV
    /// into `ancestor`'s, or `None` when `ancestor` does not contain this
    /// tile.
    ///
    /// This is the whole mechanism behind two separate behaviours: a coarse
    /// parent standing in for a child that has not arrived yet (so a cold
    /// cache sharpens rather than fills in from nothing), and a coarse parent
    /// standing in for a child that will *never* arrive because the provider
    /// answers 404 there. Both are common; see [`crate::TileProvider`].
    #[must_use]
    pub fn uv_offset_scale_within(self, ancestor: Self) -> Option<[f32; 4]> {
        if ancestor.z > self.z {
            return None;
        }
        let levels = self.z - ancestor.z;
        if (self.x >> levels) != ancestor.x || (self.y >> levels) != ancestor.y {
            return None;
        }
        let scale = 1.0 / f64::from(1u32 << levels);
        let u = f64::from(self.x - (ancestor.x << levels)) * scale;
        let v = f64::from(self.y - (ancestor.y << levels)) * scale;
        Some([u as f32, v as f32, scale as f32, scale as f32])
    }

    /// `(lon_deg, lat_deg)` at fractional position `(u, v)` inside the tile,
    /// with `(0, 0)` its north-west corner and `(1, 1)` its south-east.
    #[must_use]
    pub fn lon_lat_at(self, u: f64, v: f64) -> (f64, f64) {
        tile_xy_to_lon_lat(f64::from(self.x) + u, f64::from(self.y) + v, self.z)
    }

    #[must_use]
    pub fn center_lon_lat(self) -> (f64, f64) {
        self.lon_lat_at(0.5, 0.5)
    }

    /// Ground metres per texel at this tile's own centre latitude.
    ///
    /// Per tile rather than per zoom because Mercator's scale factor is
    /// `1/cos(lat)`: a z9 tile is 250 m/texel over Oklahoma and 129 m/texel
    /// over the Alaska Peninsula, and the subdivision tolerance has to follow
    /// the tile it is applied to.
    #[must_use]
    pub fn ground_resolution_m_per_texel(self) -> f64 {
        ground_resolution_m_per_texel(self.center_lon_lat().1, self.z)
    }

    /// Number of tiles along one axis at this tile's zoom.
    #[must_use]
    pub fn span(self) -> u32 {
        tiles_per_axis(self.z)
    }
}

/// `2^z`, saturating at [`MAX_TILE_ZOOM`] so the shift can never overflow.
#[must_use]
pub(crate) fn tiles_per_axis(z: u8) -> u32 {
    1u32 << z.min(MAX_TILE_ZOOM)
}

/// Geographic position to *fractional* tile coordinates at `z`.
///
/// Latitude is clamped to the Web Mercator limit rather than returning an
/// error: callers that must distinguish "outside the world" use
/// [`TileId::containing`].
#[must_use]
pub fn lon_lat_to_tile_xy(lon_deg: f64, lat_deg: f64, z: u8) -> (f64, f64) {
    let span = f64::from(tiles_per_axis(z));
    let x = (lon_deg + 180.0) / 360.0 * span;
    let lat = lat_deg
        .clamp(-WEB_MERCATOR_LAT_LIMIT_DEG, WEB_MERCATOR_LAT_LIMIT_DEG)
        .to_radians();
    // ln(tan(lat) + sec(lat)) is the inverse Gudermannian; asinh(tan(lat)) is
    // the same value computed without the cancellation the sum form suffers
    // near the equator.
    let y = (1.0 - lat.tan().asinh() / std::f64::consts::PI) / 2.0 * span;
    (x, y)
}

/// Fractional tile coordinates back to `(lon_deg, lat_deg)`.
#[must_use]
pub fn tile_xy_to_lon_lat(x: f64, y: f64, z: u8) -> (f64, f64) {
    let span = f64::from(tiles_per_axis(z));
    let lon = x / span * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / span))
        .sinh()
        .atan()
        .to_degrees();
    (lon, lat)
}

/// Ground metres per texel at a latitude and zoom.
#[must_use]
pub fn ground_resolution_m_per_texel(lat_deg: f64, z: u8) -> f64 {
    let lat = lat_deg
        .clamp(-WEB_MERCATOR_LAT_LIMIT_DEG, WEB_MERCATOR_LAT_LIMIT_DEG)
        .to_radians();
    EQUATOR_M_PER_TEXEL_Z0 * lat.cos() / f64::from(tiles_per_axis(z))
}

/// The zoom whose texels most nearly match a screen pixel:
/// `round(log2(equator_m_per_texel(lat) / m_per_px))`, clamped to
/// `[min_z, max_z]`.
///
/// Callers must not feed this the raw camera scale. Rounding a continuous
/// scale has no hysteresis, so a camera parked on a rounding boundary flips
/// zoom every frame and evicts a whole tile set each time. The scene layer
/// feeds it the *centre* scale of the LOD bucket the camera is in, which makes
/// the zoom a function of the bucket and inherits that selector's hysteresis.
#[must_use]
pub fn zoom_for_ground_resolution(m_per_px: f64, lat_deg: f64, min_z: u8, max_z: u8) -> u8 {
    let min_z = min_z.min(MAX_TILE_ZOOM);
    let max_z = max_z.min(MAX_TILE_ZOOM).max(min_z);
    if !m_per_px.is_finite() || m_per_px <= 0.0 {
        return max_z;
    }
    let lat = lat_deg
        .clamp(-WEB_MERCATOR_LAT_LIMIT_DEG, WEB_MERCATOR_LAT_LIMIT_DEG)
        .to_radians();
    // The cosine floor keeps a polar view from demanding an unreachable zoom;
    // it matters only far outside any radar's coverage.
    let equator_m_per_texel = EQUATOR_M_PER_TEXEL_Z0 * lat.cos().max(0.05);
    let zoom = (equator_m_per_texel / m_per_px).log2().round();
    if !zoom.is_finite() {
        return max_z;
    }
    zoom.clamp(f64::from(min_z), f64::from(max_z)) as u8
}

/// Wrap a possibly-out-of-range tile column into `0..span`, because longitude
/// is cyclic. Rows are *not* wrapped: there is nothing north of the north edge.
#[must_use]
pub(crate) fn wrap_tile_index(value: f64, span: u32) -> u32 {
    if !value.is_finite() {
        return 0;
    }
    let span_f = f64::from(span);
    let wrapped = value.rem_euclid(span_f);
    // rem_euclid can return exactly `span` for inputs a hair below zero.
    (wrapped as u32).min(span - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KTLX, the site the projection module's own tests use.
    const KTLX: (f64, f64) = (-97.2778, 35.3333);

    #[test]
    fn round_trips_lon_lat_through_tile_space() {
        for z in 0..=MAX_TILE_ZOOM {
            for (lon, lat) in [KTLX, (-122.965, 45.715), (-156.6294, 58.6794), (0.0, 0.0)] {
                let (x, y) = lon_lat_to_tile_xy(lon, lat, z);
                let (back_lon, back_lat) = tile_xy_to_lon_lat(x, y, z);
                assert!(
                    (back_lon - lon).abs() < 1e-9 && (back_lat - lat).abs() < 1e-9,
                    "z{z}: {lon},{lat} -> {back_lon},{back_lat}"
                );
            }
        }
    }

    /// The published z/x/y worked example: 51.5N 0.0E at zoom 12 is tile
    /// 2048/1362. Checked against the tile-scheme documentation rather than
    /// against arithmetic of our own.
    #[test]
    fn matches_the_documented_slippy_map_example() {
        let tile = TileId::containing(0.0, 51.5, 12).expect("in range");
        assert_eq!((tile.x, tile.y), (2048, 1362));
    }

    /// KTLX at z9 is the tile this crate was actually developed against: the
    /// USGS National Map answered 200 for /tile/9/202/117 in ArcGIS row/col
    /// order, which is x=117, y=202 here.
    #[test]
    fn the_ktlx_tile_matches_the_one_that_was_fetched() {
        let tile = TileId::containing(KTLX.0, KTLX.1, 9).expect("in range");
        assert_eq!((tile.z, tile.x, tile.y), (9, 117, 202));
    }

    #[test]
    fn the_world_is_one_tile_at_zoom_zero() {
        let tile = TileId::containing(KTLX.0, KTLX.1, 0).expect("in range");
        assert_eq!((tile.x, tile.y), (0, 0));
        assert!(TileId::new(0, 1, 0).is_none());
        assert!(TileId::new(MAX_TILE_ZOOM + 1, 0, 0).is_none());
    }

    #[test]
    fn latitude_outside_the_mercator_limit_has_no_tile() {
        assert!(TileId::containing(0.0, 86.0, 5).is_none());
        assert!(TileId::containing(0.0, -86.0, 5).is_none());
        assert!(TileId::containing(f64::NAN, 0.0, 5).is_none());
        assert!(TileId::containing(0.0, WEB_MERCATOR_LAT_LIMIT_DEG, 5).is_some());
    }

    #[test]
    fn longitude_wraps_but_the_index_stays_in_range() {
        let span = tiles_per_axis(4);
        for lon in [-540.0, -180.0, -0.0, 179.999_999, 180.0, 540.0] {
            let tile = TileId::containing(lon, 0.0, 4).expect("in range");
            assert!(tile.x < span, "lon {lon} produced x={}", tile.x);
        }
    }

    #[test]
    fn ancestors_walk_up_and_stop_at_the_root() {
        let tile = TileId::new(9, 117, 202).expect("valid");
        assert_eq!(tile.parent(), TileId::new(8, 58, 101));
        assert_eq!(tile.ancestor(0), Some(tile));
        assert_eq!(tile.ancestor(9), TileId::new(0, 0, 0));
        assert_eq!(tile.ancestor(10), None);
    }

    #[test]
    fn uv_sub_rect_places_a_child_inside_its_ancestor() {
        let tile = TileId::new(9, 117, 202).expect("valid");
        assert_eq!(
            tile.uv_offset_scale_within(tile),
            Some([0.0, 0.0, 1.0, 1.0])
        );

        let parent = tile.parent().expect("has a parent");
        let uv = tile
            .uv_offset_scale_within(parent)
            .expect("child of its own parent");
        // 117 is odd and 202 is even: east half, north half.
        assert_eq!(uv, [0.5, 0.0, 0.5, 0.5]);

        let grandparent = tile.ancestor(2).expect("has a grandparent");
        let uv = tile
            .uv_offset_scale_within(grandparent)
            .expect("descendant of its grandparent");
        assert_eq!(uv, [0.25, 0.5, 0.25, 0.25]);

        // A tile is not inside its neighbour, nor inside its own descendant.
        let neighbour = TileId::new(8, 59, 101).expect("valid");
        assert_eq!(tile.uv_offset_scale_within(neighbour), None);
        let child = TileId::new(10, 234, 404).expect("valid");
        assert_eq!(tile.uv_offset_scale_within(child), None);
    }

    /// The sub-rect must place the child's corners exactly where the child's
    /// own geography says they are. This is the check that catches a y-axis
    /// flip, which otherwise renders as a plausible-looking mirrored map.
    #[test]
    fn the_uv_sub_rect_agrees_with_geography() {
        let tile = TileId::new(11, 468, 809).expect("valid");
        for levels in 1..=4u8 {
            let ancestor = tile.ancestor(levels).expect("ancestor");
            let [u, v, su, sv] = tile
                .uv_offset_scale_within(ancestor)
                .expect("descendant")
                .map(f64::from);
            for (cu, cv) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.5, 0.5)] {
                let direct = tile.lon_lat_at(cu, cv);
                let via = ancestor.lon_lat_at(u + cu * su, v + cv * sv);
                assert!(
                    (direct.0 - via.0).abs() < 1e-9 && (direct.1 - via.1).abs() < 1e-9,
                    "levels {levels} corner {cu},{cv}: {direct:?} vs {via:?}"
                );
            }
        }
    }

    /// Ground resolution against the published Web Mercator table: zoom 0 is
    /// 156543.03 m/texel at the equator, and each zoom halves it.
    #[test]
    fn ground_resolution_matches_the_published_table() {
        assert!((ground_resolution_m_per_texel(0.0, 0) - 156_543.033_928).abs() < 1e-3);
        assert!((ground_resolution_m_per_texel(0.0, 1) - 78_271.516_964).abs() < 1e-3);
        assert!((ground_resolution_m_per_texel(0.0, 16) - 2.388_657).abs() < 1e-5);
        // 60 degrees north halves it again, because Mercator's scale factor is
        // sec(lat) and cos(60) is exactly 0.5.
        let equator = ground_resolution_m_per_texel(0.0, 9);
        let sixty = ground_resolution_m_per_texel(60.0, 9);
        assert!((sixty / equator - 0.5).abs() < 1e-9);
    }

    #[test]
    fn zoom_selection_is_monotonic_and_clamped() {
        let mut previous = u8::MAX;
        for step in 0..24 {
            let m_per_px = 2.0_f64.powi(step);
            let z = zoom_for_ground_resolution(m_per_px, KTLX.1, 0, MAX_TILE_ZOOM);
            assert!(z <= previous, "zoom rose as the view widened at {m_per_px}");
            previous = z;
        }
        // Degenerate inputs land on the fine end rather than panicking or
        // producing an out-of-range zoom.
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let z = zoom_for_ground_resolution(bad, KTLX.1, 5, 16);
            assert!((5..=16).contains(&z), "{bad} produced z{z}");
        }
        assert_eq!(zoom_for_ground_resolution(1e-9, KTLX.1, 5, 16), 16);
        assert_eq!(zoom_for_ground_resolution(1e9, KTLX.1, 5, 16), 5);
    }

    /// The selected zoom's texel size must be within a factor of sqrt(2) of
    /// the requested pixel size — that is what rounding the log2 buys, and it
    /// is the property that keeps the basemap neither blurry nor aliased.
    #[test]
    fn the_selected_zoom_is_within_half_an_octave_of_the_request() {
        for lat in [0.0, 35.3333, 45.715, 58.6794] {
            for exponent in -6..12 {
                let m_per_px = 2.0_f64.powi(exponent) * 0.75;
                let z = zoom_for_ground_resolution(m_per_px, lat, 0, MAX_TILE_ZOOM);
                if z == 0 || z == MAX_TILE_ZOOM {
                    continue; // Clamped, so the ratio is allowed to be wide.
                }
                let ratio = ground_resolution_m_per_texel(lat, z) / m_per_px;
                assert!(
                    (0.706..=1.415).contains(&ratio),
                    "lat {lat}, {m_per_px} m/px -> z{z}, ratio {ratio}"
                );
            }
        }
    }

    #[test]
    fn tile_resolution_uses_its_own_centre_latitude() {
        let oklahoma = TileId::containing(KTLX.0, KTLX.1, 9).expect("in range");
        let alaska = TileId::containing(-156.6294, 58.6794, 9).expect("in range");
        assert!(oklahoma.ground_resolution_m_per_texel() > 200.0);
        assert!(oklahoma.ground_resolution_m_per_texel() < 260.0);
        assert!(alaska.ground_resolution_m_per_texel() < 165.0);
    }
}
