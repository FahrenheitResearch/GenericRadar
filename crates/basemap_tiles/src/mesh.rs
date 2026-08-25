//! Turning a Web Mercator tile into geometry in the radar-local frame.
//!
//! # Why a tile is not two triangles
//!
//! A tile is an axis-aligned rectangle in EPSG:3857. The radar frame is a
//! geodesic azimuthal-equidistant projection (Snyder 1987, pp. 191-202,
//! evaluated with Vincenty 1975). Under that map a Mercator parallel is a
//! curve, not a line, and the transverse scale factor `rho / (R sin(rho/R))`
//! varies across the tile. Drawing the tile as two textured triangles — the
//! corner-projection approach most viewers use — therefore leaves a residual
//! that grows with tile size and with distance from the anchor.
//!
//! Measured, for the real sites this application is exercised against and at
//! the sharpest camera scale the selecting LOD bucket admits, corner
//! projection alone is wrong by roughly 45-70 screen pixels at z4, 16-29 at
//! z5, 6-13 at z6 and 3-6 at z7. It only becomes acceptable around z10. That
//! is not a rounding error; it is county lines sliding off the imagery.
//!
//! So [`build_tile_mesh`] subdivides until the piecewise-affine surface is
//! within [`crate::SUBDIVISION_TOLERANCE_TEXELS`] of the truth, and records
//! what it achieved in [`TileMesh::max_error_km`] so the bound can be asserted
//! by a test rather than trusted.
//!
//! # The invariant that makes this cheap
//!
//! The subdivision depends on the tile and on the projection and on **nothing
//! else**. No camera centre, no camera scale. A mesh is therefore cacheable on
//! `(TileId, projection identity)`, exactly as the vector basemap's retained
//! geometry is, and a pan or a zoom inside one LOD bucket reuses it untouched.

use crate::{MAX_SUBDIVISION, MAX_TILE_WORLD_KM, SUBDIVISION_TOLERANCE_TEXELS, TileId};

/// One mesh vertex: a position in the radar-local frame and the tile-space UV
/// it samples.
///
/// `position_km` is in the same units and the same frame as the vector
/// basemap's own vertices, so the two layers share a camera transform.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TileVertex {
    /// Radar-local kilometres: east, north.
    pub position_km: [f32; 2],
    /// `0..1` inside this tile, before [`TileId::uv_offset_scale_within`] is
    /// applied to redirect the sample at an ancestor texture.
    pub uv: [f32; 2],
}

impl TileVertex {
    /// Byte stride, pinned by a test so a field cannot be added without the
    /// vertex-buffer layout being revisited.
    pub const SIZE: usize = 16;
}

/// A tile's geometry in the radar-local frame.
#[derive(Clone, Debug)]
pub struct TileMesh {
    pub tile: TileId,
    /// `N`: the grid is `(N+1)^2` vertices and `2 N^2` triangles.
    pub subdivision: u32,
    /// Row-major, `(N+1) x (N+1)`, with the first row along the tile's north
    /// edge and the first column along its west edge.
    pub vertices: Vec<TileVertex>,
    pub indices: Vec<u32>,
    /// Worst measured deviation of this mesh from the true projection, in
    /// kilometres. Measured, not estimated: every cell is probed at its four
    /// edge midpoints, its centre, and the centroid of each of its two
    /// triangles.
    pub max_error_km: f32,
    pub estimated_bytes: usize,
}

impl TileMesh {
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// The achieved accuracy expressed in the tile's own texels, which is the
    /// scale-free way to state it. Multiply by the tile-to-screen
    /// magnification to get screen pixels.
    #[must_use]
    pub fn max_error_texels(&self) -> f64 {
        let m_per_texel = self.tile.ground_resolution_m_per_texel();
        if m_per_texel <= 0.0 {
            return f64::INFINITY;
        }
        f64::from(self.max_error_km) * 1_000.0 / m_per_texel
    }
}

/// Build the mesh for `tile`.
///
/// `project` maps `(lon_deg, lat_deg)` to `(east_km, north_km)` and returns
/// `None` where the geodesic does not converge. Callers must pass the
/// *fallible* projection: a projection that collapses non-convergent points
/// onto the anchor would staple a tile of the far side of the world to the
/// radar.
///
/// Returns `None` when any node fails to project, or when any node lands more
/// than [`MAX_TILE_WORLD_KM`] from the anchor. A tile the projection cannot
/// express is simply not drawn; nothing is substituted for it.
///
/// Pure in `(TileId, projection)`. Cache it on that key.
#[must_use]
pub fn build_tile_mesh<P>(tile: TileId, project: P) -> Option<TileMesh>
where
    P: Fn(f64, f64) -> Option<(f64, f64)>,
{
    build_tile_mesh_with_floor(tile, 1, project)
}

/// Build an adaptive tile mesh with at least `min_subdivision` edge segments.
///
/// Neighboring tiles need matching edge subdivisions to avoid T-junctions
/// after projection. The scene can therefore apply the largest subdivision
/// required by any visible tile to the entire set. The floor is rounded up to
/// the next supported power of two and cannot exceed [`MAX_SUBDIVISION`].
#[must_use]
pub fn build_tile_mesh_with_floor<P>(
    tile: TileId,
    min_subdivision: u32,
    project: P,
) -> Option<TileMesh>
where
    P: Fn(f64, f64) -> Option<(f64, f64)>,
{
    let m_per_texel = tile.ground_resolution_m_per_texel();
    if !m_per_texel.is_finite() || m_per_texel <= 0.0 {
        return None;
    }
    let tolerance_km = SUBDIVISION_TOLERANCE_TEXELS * m_per_texel / 1_000.0;

    let mut subdivision = min_subdivision
        .clamp(1, MAX_SUBDIVISION)
        .next_power_of_two()
        .min(MAX_SUBDIVISION);
    let (nodes, max_error_km) = loop {
        let nodes = project_grid(tile, subdivision, &project)?;
        let deviation = measure_deviation(tile, subdivision, &nodes, &project)?;
        if deviation <= tolerance_km || subdivision >= MAX_SUBDIVISION {
            break (nodes, deviation);
        }
        subdivision *= 2;
    };

    let stride = (subdivision + 1) as usize;
    let inverse = 1.0 / f64::from(subdivision);
    let mut vertices = Vec::with_capacity(stride * stride);
    for row in 0..stride {
        for column in 0..stride {
            let (east_km, north_km) = nodes[row * stride + column];
            vertices.push(TileVertex {
                position_km: [east_km as f32, north_km as f32],
                uv: [
                    (column as f64 * inverse) as f32,
                    (row as f64 * inverse) as f32,
                ],
            });
        }
    }

    let cells = subdivision as usize;
    let mut indices = Vec::with_capacity(cells * cells * 6);
    for row in 0..cells {
        for column in 0..cells {
            let north_west = (row * stride + column) as u32;
            let north_east = north_west + 1;
            let south_west = north_west + stride as u32;
            let south_east = south_west + 1;
            // Diagonal north-west to south-east. `measure_deviation` probes
            // this same triangulation, so the recorded error is the error of
            // the geometry actually emitted.
            indices.extend_from_slice(&[north_west, north_east, south_east]);
            indices.extend_from_slice(&[north_west, south_east, south_west]);
        }
    }

    let estimated_bytes = vertices.len() * TileVertex::SIZE + indices.len() * size_of::<u32>();
    Some(TileMesh {
        tile,
        subdivision,
        vertices,
        indices,
        max_error_km: max_error_km as f32,
        estimated_bytes,
    })
}

/// Project the `(n+1)^2` grid nodes, row-major. `None` if any node fails to
/// project or lands beyond [`MAX_TILE_WORLD_KM`].
fn project_grid<P>(tile: TileId, subdivision: u32, project: &P) -> Option<Vec<(f64, f64)>>
where
    P: Fn(f64, f64) -> Option<(f64, f64)>,
{
    let stride = (subdivision + 1) as usize;
    let inverse = 1.0 / f64::from(subdivision);
    let mut nodes = Vec::with_capacity(stride * stride);
    for row in 0..stride {
        let v = row as f64 * inverse;
        for column in 0..stride {
            let u = column as f64 * inverse;
            let (lon_deg, lat_deg) = tile.lon_lat_at(u, v);
            let point = project(lon_deg, lat_deg)?;
            if !point.0.is_finite() || !point.1.is_finite() {
                return None;
            }
            if point.0.hypot(point.1) > MAX_TILE_WORLD_KM {
                return None;
            }
            nodes.push(point);
        }
    }
    Some(nodes)
}

/// Worst distance, in kilometres, between the true projection and the
/// piecewise-affine surface the emitted triangles describe.
///
/// Probed at seven points per cell. The four edge midpoints and the cell
/// centre lie on shared edges or on the diagonal, where both triangles agree,
/// so their affine estimate is the midpoint of the relevant pair. The two
/// triangle centroids are genuinely interior samples, which is what keeps this
/// from under-reporting the way an edges-only probe does.
fn measure_deviation<P>(
    tile: TileId,
    subdivision: u32,
    nodes: &[(f64, f64)],
    project: &P,
) -> Option<f64>
where
    P: Fn(f64, f64) -> Option<(f64, f64)>,
{
    let stride = (subdivision + 1) as usize;
    let inverse = 1.0 / f64::from(subdivision);
    let mut worst_km = 0.0_f64;

    for row in 0..subdivision as usize {
        for column in 0..subdivision as usize {
            let north_west = nodes[row * stride + column];
            let north_east = nodes[row * stride + column + 1];
            let south_west = nodes[(row + 1) * stride + column];
            let south_east = nodes[(row + 1) * stride + column + 1];

            let u0 = column as f64 * inverse;
            let u1 = (column + 1) as f64 * inverse;
            let v0 = row as f64 * inverse;
            let v1 = (row + 1) as f64 * inverse;
            let um = (u0 + u1) * 0.5;
            let vm = (v0 + v1) * 0.5;

            let probes = [
                (um, v0, midpoint(north_west, north_east)),
                (um, v1, midpoint(south_west, south_east)),
                (u0, vm, midpoint(north_west, south_west)),
                (u1, vm, midpoint(north_east, south_east)),
                // The cell centre sits on the north-west/south-east diagonal,
                // where the two triangles meet.
                (um, vm, midpoint(north_west, south_east)),
                // Interior of triangle (NW, NE, SE).
                (
                    (u0 + 2.0 * u1) / 3.0,
                    (2.0 * v0 + v1) / 3.0,
                    centroid(north_west, north_east, south_east),
                ),
                // Interior of triangle (NW, SE, SW).
                (
                    (2.0 * u0 + u1) / 3.0,
                    (v0 + 2.0 * v1) / 3.0,
                    centroid(north_west, south_east, south_west),
                ),
            ];

            for (u, v, estimate) in probes {
                let (lon_deg, lat_deg) = tile.lon_lat_at(u, v);
                let truth = project(lon_deg, lat_deg)?;
                if !truth.0.is_finite() || !truth.1.is_finite() {
                    return None;
                }
                let error_km = (truth.0 - estimate.0).hypot(truth.1 - estimate.1);
                if error_km > worst_km {
                    worst_km = error_km;
                }
            }
        }
    }
    Some(worst_km)
}

fn midpoint(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5)
}

fn centroid(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> (f64, f64) {
    ((a.0 + b.0 + c.0) / 3.0, (a.1 + b.1 + c.1) / 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature world 8000 km across, so every tile stays inside
    /// [`MAX_TILE_WORLD_KM`].
    const TEST_WORLD_KM: f64 = 8_000.0;

    /// Web Mercator itself, which is the one projection under which a tile
    /// really *is* a rectangle. `build_tile_mesh` should notice there is
    /// nothing to subdivide and emit a single quad.
    ///
    /// Note what this rules out as a test: a projection that is merely linear
    /// in `(lon, lat)` is **not** linear in tile space, because latitude is a
    /// nonlinear function of the tile's `v`. Using one would have made this
    /// test assert subdivision where none was warranted.
    fn mercator_projection(lon_deg: f64, lat_deg: f64) -> Option<(f64, f64)> {
        let (x, y) = crate::tile_math::lon_lat_to_tile_xy(lon_deg, lat_deg, 0);
        Some(((x - 0.5) * TEST_WORLD_KM, (0.5 - y) * TEST_WORLD_KM))
    }

    /// Mercator with real curvature added: quadratic in the east coordinate,
    /// so strongly non-affine, cheap and deterministic.
    fn curved_projection(lon_deg: f64, lat_deg: f64) -> Option<(f64, f64)> {
        let (east, north) = mercator_projection(lon_deg, lat_deg)?;
        Some((east + east * east / TEST_WORLD_KM, north))
    }

    #[test]
    fn an_affine_projection_needs_no_subdivision() {
        let tile = TileId::new(5, 7, 12).expect("valid");
        let mesh =
            build_tile_mesh(tile, mercator_projection).expect("mercator projects everywhere");
        assert_eq!(mesh.subdivision, 1);
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices.len(), 6);
        assert_eq!(mesh.triangle_count(), 2);
        assert!(mesh.max_error_km < 1e-9, "{}", mesh.max_error_km);
    }

    #[test]
    fn vertex_layout_is_pinned() {
        assert_eq!(size_of::<TileVertex>(), TileVertex::SIZE);
        assert_eq!(align_of::<TileVertex>(), 4);
        assert_eq!(TileVertex::SIZE, 16);
    }

    /// UV must run 0..1 across the tile, with `v = 0` on the tile's north edge
    /// and the vertex grid row-major. Getting this backwards mirrors the map.
    #[test]
    fn uv_corners_match_the_grid_order() {
        let tile = TileId::new(5, 7, 12).expect("valid");
        let mesh = build_tile_mesh(tile, mercator_projection).expect("projects");
        assert_eq!(mesh.vertices[0].uv, [0.0, 0.0]);
        assert_eq!(mesh.vertices[1].uv, [1.0, 0.0]);
        assert_eq!(mesh.vertices[2].uv, [0.0, 1.0]);
        assert_eq!(mesh.vertices[3].uv, [1.0, 1.0]);

        // v = 0 is the north edge, so its latitude must be the greater one.
        let north = tile.lon_lat_at(0.0, 0.0).1;
        let south = tile.lon_lat_at(0.0, 1.0).1;
        assert!(north > south, "v=0 must be the north edge");
        // And under this test projection north is +y, so vertex 0 is above
        // vertex 2.
        assert!(mesh.vertices[0].position_km[1] > mesh.vertices[2].position_km[1]);
    }

    /// A projection with real curvature must be subdivided, and the recorded
    /// error must actually fall as the subdivision rises.
    #[test]
    fn curvature_forces_subdivision_and_the_error_falls() {
        let curved = curved_projection;
        let coarse = TileId::new(5, 7, 12).expect("valid");
        let fine = TileId::new(12, 1_024, 1_600).expect("valid");

        let coarse_mesh = build_tile_mesh(coarse, curved).expect("projects");
        let fine_mesh = build_tile_mesh(fine, curved).expect("projects");
        assert!(
            coarse_mesh.subdivision > 1,
            "curvature across a z5 tile must force subdivision"
        );
        assert!(
            fine_mesh.subdivision <= coarse_mesh.subdivision,
            "a narrower tile must never need more subdivision: {} vs {}",
            fine_mesh.subdivision,
            coarse_mesh.subdivision
        );
        // And the same tile, with the curvature removed, needs none.
        assert_eq!(
            build_tile_mesh(coarse, mercator_projection)
                .expect("projects")
                .subdivision,
            1
        );
        assert_eq!(
            coarse_mesh.vertices.len(),
            ((coarse_mesh.subdivision + 1) * (coarse_mesh.subdivision + 1)) as usize
        );
        assert_eq!(
            coarse_mesh.indices.len(),
            (coarse_mesh.subdivision * coarse_mesh.subdivision * 6) as usize
        );
    }

    #[test]
    fn a_projection_that_fails_anywhere_drops_the_tile() {
        let tile = TileId::new(5, 7, 12).expect("valid");
        let refuses_one_corner = |lon_deg: f64, lat_deg: f64| {
            if lat_deg > 60.0 {
                None
            } else {
                mercator_projection(lon_deg, lat_deg)
            }
        };
        let northern = TileId::new(5, 7, 5).expect("valid");
        assert!(build_tile_mesh(northern, refuses_one_corner).is_none());
        // The same projection still builds a tile that stays inside its
        // domain, so the rejection is local rather than a blanket failure.
        assert!(build_tile_mesh(tile, refuses_one_corner).is_some());
    }

    #[test]
    fn a_tile_beyond_the_world_radius_is_dropped_rather_than_drawn() {
        let tile = TileId::new(5, 7, 12).expect("valid");
        let far_away = |_lon: f64, _lat: f64| Some((MAX_TILE_WORLD_KM + 1.0, 0.0));
        assert!(build_tile_mesh(tile, far_away).is_none());
        let just_inside = |_lon: f64, _lat: f64| Some((MAX_TILE_WORLD_KM - 1.0, 0.0));
        assert!(build_tile_mesh(tile, just_inside).is_some());
    }

    #[test]
    fn a_floor_of_one_preserves_the_original_adaptive_mesh() {
        let tile = TileId::new(5, 7, 12).expect("valid");
        let original = build_tile_mesh(tile, curved_projection).expect("projects");
        let floored = build_tile_mesh_with_floor(tile, 1, curved_projection).expect("projects");

        assert_eq!(floored.subdivision, original.subdivision);
        assert_eq!(floored.vertices, original.vertices);
        assert_eq!(floored.indices, original.indices);
        assert_eq!(
            floored.max_error_km.to_bits(),
            original.max_error_km.to_bits()
        );
    }

    #[test]
    fn subdivision_floor_rounds_up_and_stays_within_the_supported_ladder() {
        let tile = TileId::new(5, 7, 12).expect("valid");
        for (requested, expected) in [
            (0, 1),
            (1, 1),
            (2, 2),
            (3, 4),
            (5, MAX_SUBDIVISION),
            (u32::MAX, MAX_SUBDIVISION),
        ] {
            let mesh =
                build_tile_mesh_with_floor(tile, requested, mercator_projection).expect("projects");
            assert_eq!(mesh.subdivision, expected, "requested {requested}");
        }
    }

    #[test]
    fn neighboring_tiles_at_the_same_floor_share_exact_edge_positions() {
        let west = TileId::new(5, 7, 12).expect("valid");
        let east = TileId::new(5, 8, 12).expect("valid");
        let south = TileId::new(5, 7, 13).expect("valid");

        for floor in [1, 2, 4, MAX_SUBDIVISION] {
            let west_mesh =
                build_tile_mesh_with_floor(west, floor, curved_projection).expect("projects");
            let east_mesh =
                build_tile_mesh_with_floor(east, floor, curved_projection).expect("projects");
            let south_mesh =
                build_tile_mesh_with_floor(south, floor, curved_projection).expect("projects");
            assert_eq!(west_mesh.subdivision, east_mesh.subdivision);
            assert_eq!(west_mesh.subdivision, south_mesh.subdivision);

            let segments = west_mesh.subdivision as usize;
            let stride = segments + 1;
            for row in 0..stride {
                assert_eq!(
                    west_mesh.vertices[row * stride + segments].position_km,
                    east_mesh.vertices[row * stride].position_km,
                    "east/west seam at floor {floor}, row {row}"
                );
            }
            for column in 0..stride {
                assert_eq!(
                    west_mesh.vertices[segments * stride + column].position_km,
                    south_mesh.vertices[column].position_km,
                    "north/south seam at floor {floor}, column {column}"
                );
            }
        }
    }

    #[test]
    fn indices_stay_inside_the_vertex_buffer() {
        let curved = curved_projection;
        for tile in [
            TileId::new(5, 7, 12).expect("valid"),
            TileId::new(9, 117, 202).expect("valid"),
            TileId::new(16, 15_000, 25_000).expect("valid"),
        ] {
            let mesh = build_tile_mesh(tile, curved).expect("projects");
            let limit = mesh.vertices.len() as u32;
            assert!(mesh.indices.iter().all(|index| *index < limit), "{tile:?}");
            assert_eq!(mesh.indices.len() % 3, 0);
            assert!(mesh.subdivision <= MAX_SUBDIVISION);
            assert_eq!(
                mesh.estimated_bytes,
                mesh.vertices.len() * 16 + mesh.indices.len() * 4
            );
        }
    }
}
