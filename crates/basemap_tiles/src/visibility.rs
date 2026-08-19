//! Which tiles a pane can see.
//!
//! The caller unprojects the pane boundary into geographic coordinates — it
//! owns the camera and the projection, this crate does not — and hands the
//! result over as a [`ViewportGeo`].

use crate::tile_math::{lon_lat_to_tile_xy, tiles_per_axis, wrap_tile_index};
use crate::{MAX_TILE_ZOOM, TileId, WEB_MERCATOR_LAT_LIMIT_DEG};

/// A pane boundary, already unprojected by the caller.
#[derive(Clone, Debug)]
pub struct ViewportGeo {
    /// At least sixteen points walked around the pane edge, in degrees.
    ///
    /// Four corners is not enough and the failure is subtle: in the radar
    /// frame the pane edge is a *curve* in geographic space, bowing outward
    /// between the corners, so a corner-only bounding box silently drops tiles
    /// along the middle of each edge. That shows up as missing imagery in a
    /// band across the view, which reads as a network problem.
    pub boundary_lon_lat: Vec<(f64, f64)>,
    pub center_lon_lat: (f64, f64),
    /// Set when the pane covers more than 180 degrees of longitude or contains
    /// a pole, where a lon/lat bounding box means nothing.
    pub wraps_world: bool,
}

impl ViewportGeo {
    /// Walk a rectangle's edge, `per_edge` points per side, mapping each to
    /// geographic coordinates through `unproject`.
    ///
    /// Convenience for the common caller. Points that fail to unproject are
    /// skipped rather than substituted; if too few survive, `wraps_world` is
    /// set so the caller falls back to the conservative path.
    #[must_use]
    pub fn from_rect_edge<U>(
        min: (f64, f64),
        max: (f64, f64),
        per_edge: usize,
        unproject: U,
    ) -> Self
    where
        U: Fn(f64, f64) -> Option<(f64, f64)>,
    {
        let per_edge = per_edge.max(2);
        let mut boundary = Vec::with_capacity(per_edge * 4);
        let push = |x: f64, y: f64, boundary: &mut Vec<(f64, f64)>| {
            if let Some(point) = unproject(x, y)
                && point.0.is_finite()
                && point.1.is_finite()
            {
                boundary.push(point);
            }
        };
        for step in 0..per_edge {
            let t = step as f64 / (per_edge - 1) as f64;
            let x = min.0 + (max.0 - min.0) * t;
            let y = min.1 + (max.1 - min.1) * t;
            push(x, min.1, &mut boundary);
            push(x, max.1, &mut boundary);
            push(min.0, y, &mut boundary);
            push(max.0, y, &mut boundary);
        }
        let center = unproject((min.0 + max.0) * 0.5, (min.1 + max.1) * 0.5);
        let wraps_world = boundary.len() < 8 || center.is_none();
        Self {
            boundary_lon_lat: boundary,
            center_lon_lat: center.unwrap_or((0.0, 0.0)),
            wraps_world,
        }
    }
}

/// Tiles overlapping `view` at `z`, nearest-to-centre first, at most
/// `max_tiles`.
///
/// Nearest-first is not cosmetic: it is what makes a cold cache fill in from
/// where the user is looking outward, and it makes the request order
/// deterministic so a test can pin it.
///
/// Empty when `view.wraps_world` and the whole world at `z` would exceed
/// `max_tiles`, because a lon/lat bounding box is meaningless in that case and
/// the honest answer is "ask for a coarser zoom".
#[must_use]
pub fn visible_tiles(z: u8, view: &ViewportGeo, max_tiles: usize) -> Vec<TileId> {
    if z > MAX_TILE_ZOOM || max_tiles == 0 {
        return Vec::new();
    }
    let span = tiles_per_axis(z);
    let span_f = f64::from(span);

    let (min_x, max_x, min_y, max_y) = if view.wraps_world {
        let total = (span as usize).saturating_mul(span as usize);
        if total > max_tiles {
            return Vec::new();
        }
        (0.0, span_f - 1.0, 0.0, span_f - 1.0)
    } else {
        let Some(bounds) = bounding_tiles(z, view) else {
            return Vec::new();
        };
        bounds
    };

    // The centre in *unwrapped* tile space, so ordering across the
    // antimeridian is by real distance rather than by index arithmetic.
    let (center_x, center_y) = lon_lat_to_tile_xy(view.center_lon_lat.0, view.center_lon_lat.1, z);
    let center_x = nearest_equivalent(center_x, (min_x + max_x) * 0.5, span_f);

    let first_column = min_x.floor() as i64;
    let last_column = max_x.floor() as i64;
    let first_row = (min_y.floor().max(0.0) as i64).min(i64::from(span) - 1);
    let last_row = (max_y.floor().max(0.0) as i64).min(i64::from(span) - 1);
    if last_column < first_column || last_row < first_row {
        return Vec::new();
    }

    let columns = (last_column - first_column + 1) as usize;
    let rows = (last_row - first_row + 1) as usize;
    let mut candidates = Vec::with_capacity(columns.saturating_mul(rows).min(4_096));
    for row in first_row..=last_row {
        for column in first_column..=last_column {
            let distance = (column as f64 + 0.5 - center_x).hypot(row as f64 + 0.5 - center_y);
            let x = wrap_tile_index(column as f64, span);
            let y = row as u32;
            let Some(tile) = TileId::new(z, x, y) else {
                continue;
            };
            candidates.push((distance, tile));
        }
        if candidates.len() > MAX_CANDIDATES {
            // A pathological bounding box cannot be allowed to allocate
            // without bound before the sort trims it.
            break;
        }
    }

    candidates.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.cmp(&right.1))
    });
    candidates.truncate(max_tiles);
    candidates.dedup_by_key(|(_, tile)| *tile);
    candidates.into_iter().map(|(_, tile)| tile).collect()
}

/// Hard ceiling on how many candidates are gathered before sorting. A pane at
/// a sane zoom needs at most a few hundred; anything past this is a sign the
/// caller should have dropped a zoom, and `visible_tiles` truncating is a
/// better failure than an unbounded allocation.
const MAX_CANDIDATES: usize = 16_384;

/// The bounding box of the boundary in *unwrapped* tile space, so a pane
/// straddling the antimeridian produces a contiguous column range rather than
/// two ranges at opposite edges of the world.
fn bounding_tiles(z: u8, view: &ViewportGeo) -> Option<(f64, f64, f64, f64)> {
    let span_f = f64::from(tiles_per_axis(z));
    let mut reference: Option<f64> = None;
    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);

    for (lon_deg, lat_deg) in &view.boundary_lon_lat {
        if !lon_deg.is_finite() || !lat_deg.is_finite() {
            continue;
        }
        let lat = lat_deg.clamp(-WEB_MERCATOR_LAT_LIMIT_DEG, WEB_MERCATOR_LAT_LIMIT_DEG);
        let (x, y) = lon_lat_to_tile_xy(*lon_deg, lat, z);
        let x = match reference {
            None => {
                reference = Some(x);
                x
            }
            Some(anchor) => nearest_equivalent(x, anchor, span_f),
        };
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }

    if !min_x.is_finite() || !max_x.is_finite() {
        return None;
    }
    // A boundary that has walked more than the whole world in longitude is the
    // `wraps_world` case arriving by another route.
    if max_x - min_x >= span_f {
        return None;
    }
    Some((min_x, max_x, min_y.max(0.0), max_y.min(span_f - 1e-9)))
}

/// The representative of `value` modulo `span` that is nearest to `anchor`.
fn nearest_equivalent(value: f64, anchor: f64, span: f64) -> f64 {
    if !value.is_finite() || !anchor.is_finite() || span <= 0.0 {
        return value;
    }
    value - ((value - anchor) / span).round() * span
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small pane around a point, as a ring of boundary samples.
    fn view_around(lon: f64, lat: f64, half_lon: f64, half_lat: f64) -> ViewportGeo {
        let mut boundary = Vec::new();
        for step in 0..16 {
            let t = f64::from(step) / 15.0;
            boundary.push((lon - half_lon + 2.0 * half_lon * t, lat - half_lat));
            boundary.push((lon - half_lon + 2.0 * half_lon * t, lat + half_lat));
            boundary.push((lon - half_lon, lat - half_lat + 2.0 * half_lat * t));
            boundary.push((lon + half_lon, lat - half_lat + 2.0 * half_lat * t));
        }
        ViewportGeo {
            boundary_lon_lat: boundary,
            center_lon_lat: (lon, lat),
            wraps_world: false,
        }
    }

    #[test]
    fn a_tight_view_selects_the_tile_it_is_inside() {
        let view = view_around(-97.2778, 35.3333, 0.001, 0.001);
        let tiles = visible_tiles(9, &view, 256);
        assert_eq!(tiles, vec![TileId::new(9, 117, 202).expect("valid")]);
    }

    #[test]
    fn every_boundary_point_lands_in_a_selected_tile() {
        let view = view_around(-97.2778, 35.3333, 1.5, 1.0);
        for z in 5..=13 {
            let tiles = visible_tiles(z, &view, 4_096);
            let selected: std::collections::HashSet<_> = tiles.iter().copied().collect();
            for (lon, lat) in &view.boundary_lon_lat {
                let expected = TileId::containing(*lon, *lat, z).expect("in range");
                assert!(
                    selected.contains(&expected),
                    "z{z}: {lon},{lat} -> {expected:?} was not selected"
                );
            }
            assert!(
                selected.contains(&TileId::containing(-97.2778, 35.3333, z).expect("in range"))
            );
        }
    }

    #[test]
    fn results_are_ordered_from_the_centre_outward() {
        let view = view_around(-97.2778, 35.3333, 2.0, 1.5);
        let tiles = visible_tiles(9, &view, 256);
        assert!(
            tiles.len() > 4,
            "expected a real spread, got {}",
            tiles.len()
        );
        assert_eq!(
            tiles[0],
            TileId::containing(-97.2778, 35.3333, 9).expect("in range"),
            "the centre tile must be requested first"
        );

        let (center_x, center_y) = lon_lat_to_tile_xy(-97.2778, 35.3333, 9);
        let mut previous = 0.0_f64;
        for tile in &tiles {
            let distance =
                (f64::from(tile.x) + 0.5 - center_x).hypot(f64::from(tile.y) + 0.5 - center_y);
            assert!(
                distance >= previous - 1e-9,
                "ordering regressed at {tile:?}"
            );
            previous = distance;
        }
    }

    #[test]
    fn the_order_is_deterministic() {
        let view = view_around(-97.2778, 35.3333, 2.0, 1.5);
        assert_eq!(visible_tiles(9, &view, 64), visible_tiles(9, &view, 64));
    }

    #[test]
    fn the_cap_is_honoured_and_keeps_the_nearest() {
        let view = view_around(-97.2778, 35.3333, 4.0, 3.0);
        let full = visible_tiles(10, &view, 4_096);
        let capped = visible_tiles(10, &view, 12);
        assert!(full.len() > 12);
        assert_eq!(capped.len(), 12);
        assert_eq!(capped, full[..12].to_vec());
        assert!(visible_tiles(10, &view, 0).is_empty());
    }

    #[test]
    fn no_duplicate_tiles_are_ever_returned() {
        for half in [0.5, 5.0, 40.0, 120.0] {
            let view = view_around(-97.2778, 35.3333, half, half.min(60.0));
            for z in 5..=10 {
                let tiles = visible_tiles(z, &view, 4_096);
                let unique: std::collections::HashSet<_> = tiles.iter().copied().collect();
                assert_eq!(unique.len(), tiles.len(), "z{z}, half {half}");
            }
        }
    }

    /// A pane straddling the antimeridian must produce one contiguous run of
    /// tiles either side of the seam, not the entire width of the world.
    #[test]
    fn a_view_across_the_antimeridian_stays_local() {
        let view = view_around(179.95, 51.88, 0.4, 0.3);
        let tiles = visible_tiles(9, &view, 256);
        assert!(!tiles.is_empty());
        assert!(
            tiles.len() <= 8,
            "expected a local patch, got {} tiles",
            tiles.len()
        );
        let span = tiles_per_axis(9);
        // Both the east edge (x near span-1) and the west edge (x = 0) must be
        // represented, because the pane really does cover both.
        assert!(tiles.iter().any(|tile| tile.x == 0));
        assert!(tiles.iter().any(|tile| tile.x == span - 1));
    }

    #[test]
    fn a_world_wrapping_view_returns_the_whole_world_or_nothing() {
        let view = ViewportGeo {
            boundary_lon_lat: vec![(0.0, 0.0)],
            center_lon_lat: (0.0, 0.0),
            wraps_world: true,
        };
        // 4 x 4 = 16 tiles fits inside a 256-tile budget.
        assert_eq!(visible_tiles(2, &view, 256).len(), 16);
        // 512 x 512 does not, so the honest answer is "ask for a coarser zoom".
        assert!(visible_tiles(9, &view, 256).is_empty());
    }

    #[test]
    fn an_empty_or_broken_boundary_selects_nothing_rather_than_guessing() {
        let empty = ViewportGeo {
            boundary_lon_lat: Vec::new(),
            center_lon_lat: (-97.2778, 35.3333),
            wraps_world: false,
        };
        assert!(visible_tiles(9, &empty, 256).is_empty());

        let broken = ViewportGeo {
            boundary_lon_lat: vec![(f64::NAN, 35.0), (-97.0, f64::INFINITY)],
            center_lon_lat: (-97.2778, 35.3333),
            wraps_world: false,
        };
        assert!(visible_tiles(9, &broken, 256).is_empty());
    }

    #[test]
    fn zoom_beyond_the_ceiling_selects_nothing() {
        let view = view_around(-97.2778, 35.3333, 0.01, 0.01);
        assert!(visible_tiles(MAX_TILE_ZOOM + 1, &view, 256).is_empty());
        assert!(!visible_tiles(MAX_TILE_ZOOM, &view, 256).is_empty());
    }

    #[test]
    fn rect_edge_helper_walks_all_four_sides() {
        // Identity "unprojection" onto a degree grid.
        let view = ViewportGeo::from_rect_edge((-1.0, -1.0), (1.0, 1.0), 8, |x, y| Some((x, y)));
        assert!(!view.wraps_world);
        assert_eq!(view.center_lon_lat, (0.0, 0.0));
        assert!(view.boundary_lon_lat.len() >= 16);
        assert!(view.boundary_lon_lat.iter().any(|point| point.0 <= -1.0));
        assert!(view.boundary_lon_lat.iter().any(|point| point.0 >= 1.0));
        assert!(view.boundary_lon_lat.iter().any(|point| point.1 <= -1.0));
        assert!(view.boundary_lon_lat.iter().any(|point| point.1 >= 1.0));

        // A pane whose edge does not unproject falls back to the conservative
        // path rather than inventing a bounding box.
        let broken = ViewportGeo::from_rect_edge((-1.0, -1.0), (1.0, 1.0), 8, |_, _| None);
        assert!(broken.wraps_world);
    }
}
