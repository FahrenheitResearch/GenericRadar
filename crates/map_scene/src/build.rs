//! Projection, simplification and tessellation.
//!
//! Everything expensive happens here, off the UI thread, once per
//! `GeometryCacheKey`. The build depends only on dataset, projection, style and
//! LOD bucket — never on where the camera happens to be pointing or its exact
//! scale — which is what allows a pan to be a uniform update.

use analyst_runtime::{GeometryCacheKey, LodBucket};

use crate::dataset::{GeoLineFeature, MapDataset, MapLayer};
use crate::geometry::{GeometryStats, MapDraw, MapGeometry, MapVertex, ProjectedLabel};
use crate::projection::RadarProjection;
use crate::style::MapStyle;

/// Smallest half-extent of the retained region, in kilometres. Comfortably
/// exceeds the 460 km Level II footprint so context remains when the analyst
/// pans off the radar.
pub const MIN_BUILD_HALF_EXTENT_KM: f64 = 1_000.0;
/// Largest half-extent. Beyond this the whole visible earth is covered and
/// growing further only adds work.
pub const MAX_BUILD_HALF_EXTENT_KM: f64 = 20_000.0;
/// Half-extents of viewport width to cover at a given scale.
const COVERAGE_POINTS: f64 = 4_000.0;

/// Half of the square, anchor-centred region that is projected and retained,
/// for one LOD bucket.
///
/// This is a coverage bound, not a camera key. It is a function of the bucket
/// alone, never of where the camera is pointing, so panning inside the region
/// still never triggers a rebuild. It grows with the bucket because a coarse
/// view sees much more ground: a fixed region left the continent-scale view
/// showing a square island of map surrounded by nothing.
pub fn build_half_extent_km(lod: LodBucket) -> f64 {
    let km_per_point = f64::from(lod.center_scale(LOD_REFERENCE_KM_PER_POINT));
    (km_per_point * COVERAGE_POINTS).clamp(MIN_BUILD_HALF_EXTENT_KM, MAX_BUILD_HALF_EXTENT_KM)
}

/// Simplification tolerance in screen pixels. Points that would land within
/// this distance of the retained line at the bucket's scale are dropped.
const SIMPLIFY_TOLERANCE_PX: f64 = 0.6;

/// Reference scale that anchors `LodBucket` to kilometres per point.
pub const LOD_REFERENCE_KM_PER_POINT: f32 = analyst_runtime::DEFAULT_KM_PER_POINT;

/// Inputs for one build. Immutable and cheap to clone.
#[derive(Clone)]
pub struct MapBuildRequest {
    pub key: GeometryCacheKey,
    pub dataset: MapDataset,
    pub projection: RadarProjection,
    pub style: MapStyle,
}

/// Project, simplify and tessellate one LOD of the dataset.
pub fn build_geometry(request: &MapBuildRequest) -> MapGeometry {
    let km_per_point = f64::from(request.key.lod.center_scale(LOD_REFERENCE_KM_PER_POINT));
    // The tolerance is measured in the radar-local frame even when the pane is
    // showing the globe. That is sound rather than an approximation left
    // unchecked: the globe morph is a contraction (`globe::radial_factor` is
    // proved to stay in `0..=1`), so a point this simplification is willing to
    // drop can only move LESS on the globe than it does here.
    let tolerance_km = km_per_point * SIMPLIFY_TOLERANCE_PX;
    let half_extent_km = build_half_extent_km(request.key.lod);

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut draws = Vec::new();
    let mut stats = GeometryStats::default();

    // Layers are emitted in enum order so the paint order is deterministic.
    for layer in MapLayer::ALL {
        if !request.style.is_visible(layer, km_per_point as f32) {
            continue;
        }
        let style = request.style.layer(layer);
        let color = pack_color(style.color.to_array());
        let half_width_px = style.width_px * 0.5;
        let index_start = indices.len() as u32;

        for line in request
            .dataset
            .lines
            .iter()
            .filter(|line| line.layer == layer)
        {
            stats.source_points += line.points.len();
            if !bbox_intersects_build_region(line, &request.projection, half_extent_km) {
                stats.features_culled += 1;
                continue;
            }
            let projected = project_and_clip(line, &request.projection, half_extent_km);
            for run in projected {
                let simplified = simplify(&run, tolerance_km);
                if simplified.len() < 2 {
                    continue;
                }
                stats.features_built += 1;
                stats.retained_points += simplified.len();
                tessellate_polyline(
                    &simplified,
                    half_width_px,
                    color,
                    &mut vertices,
                    &mut indices,
                );
            }
        }

        let index_count = indices.len() as u32 - index_start;
        if index_count > 0 {
            draws.push(MapDraw {
                layer,
                index_start,
                index_count,
            });
        }
    }

    let labels = project_labels(request, half_extent_km);
    MapGeometry::new(request.key, vertices, indices, draws, labels, stats)
}

/// Reject features whose geographic bounding box cannot reach the build
/// region. This is a coarse degrees-based test; exact clipping happens after
/// projection.
fn bbox_intersects_build_region(
    line: &GeoLineFeature,
    projection: &RadarProjection,
    half_extent_km: f64,
) -> bool {
    let [min_lon, min_lat, max_lon, max_lat] = line.bbox.map(f64::from);
    // Latitude is the cheap, always-valid axis: one degree is ~111 km.
    let margin_deg = half_extent_km / 111.0 * std::f64::consts::SQRT_2;
    if margin_deg >= 180.0 {
        return true;
    }
    let lat = projection.radar_lat_deg();
    if min_lat > lat + margin_deg || max_lat < lat - margin_deg {
        return false;
    }
    // Longitude degrees shrink with latitude; near the poles the test degrades
    // to "keep it", which is correct if conservative.
    let cos_lat = lat.to_radians().cos().abs();
    if cos_lat < 0.05 {
        return true;
    }
    let lon_margin_deg = margin_deg / cos_lat;
    let lon = projection.radar_lon_deg();
    let west = wrap_delta(min_lon - lon);
    let east = wrap_delta(max_lon - lon);
    // A box straddling the wrap keeps both ends; treat it as overlapping.
    if west > east {
        return true;
    }
    !(west > lon_margin_deg || east < -lon_margin_deg)
}

fn wrap_delta(delta_deg: f64) -> f64 {
    let mut value = (delta_deg + 180.0).rem_euclid(360.0) - 180.0;
    if value <= -180.0 {
        value += 360.0;
    }
    value
}

/// Project a feature and split it into runs that lie inside the build region.
///
/// Where a feature crosses the boundary the segment is cut at the boundary
/// itself. Carrying the outside vertex instead would draw a straight line from
/// the edge of the region to a point that can be thousands of kilometres away,
/// which appears as a spurious line ruled straight across the pane.
fn project_and_clip(
    line: &GeoLineFeature,
    projection: &RadarProjection,
    half_extent_km: f64,
) -> Vec<Vec<[f64; 2]>> {
    let mut runs = Vec::new();
    let mut current: Vec<[f64; 2]> = Vec::new();
    let mut previous: Option<([f64; 2], bool)> = None;

    for (lon, lat) in line.points {
        // A point the geodesic cannot resolve breaks the run rather than
        // contributing a fabricated position.
        let Some(world) = projection.try_lon_lat_to_world(f64::from(*lon), f64::from(*lat)) else {
            if current.len() >= 2 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            previous = None;
            continue;
        };
        let point = [world.east_km, world.north_km];
        let inside = is_inside(point, half_extent_km);

        match (previous, inside) {
            (_, true) => {
                if let Some((previous_point, false)) = previous {
                    // Entering: start at the boundary crossing.
                    if let Some(crossing) = clip_to_region(point, previous_point, half_extent_km) {
                        current.push(crossing);
                    }
                }
                current.push(point);
            }
            (Some((previous_point, true)), false) => {
                // Leaving: finish at the boundary crossing.
                if let Some(crossing) = clip_to_region(previous_point, point, half_extent_km) {
                    current.push(crossing);
                }
                if current.len() >= 2 {
                    runs.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            _ => {}
        }
        previous = Some((point, inside));
    }

    if current.len() >= 2 {
        runs.push(current);
    }
    runs
}

fn is_inside(point: [f64; 2], half_extent_km: f64) -> bool {
    point[0].abs() <= half_extent_km && point[1].abs() <= half_extent_km
}

/// Walk from `inside` towards `outside` and return the last point still inside
/// the region. Bisection keeps this exact enough for a 1000 km boundary that is
/// never on screen, without special-casing which edge was crossed.
fn clip_to_region(inside: [f64; 2], outside: [f64; 2], half_extent_km: f64) -> Option<[f64; 2]> {
    if !is_inside(inside, half_extent_km) || is_inside(outside, half_extent_km) {
        return None;
    }
    let mut low = 0.0_f64;
    let mut high = 1.0_f64;
    for _ in 0..24 {
        let mid = (low + high) * 0.5;
        let candidate = [
            inside[0] + (outside[0] - inside[0]) * mid,
            inside[1] + (outside[1] - inside[1]) * mid,
        ];
        if is_inside(candidate, half_extent_km) {
            low = mid;
        } else {
            high = mid;
        }
    }
    Some([
        inside[0] + (outside[0] - inside[0]) * low,
        inside[1] + (outside[1] - inside[1]) * low,
    ])
}

/// Ramer-Douglas-Peucker, iterative so a pathological feature cannot blow the
/// stack.
fn simplify(points: &[[f64; 2]], tolerance_km: f64) -> Vec<[f64; 2]> {
    if points.len() <= 2 || tolerance_km <= 0.0 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;

    let mut stack = vec![(0_usize, points.len() - 1)];
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let mut worst_index = start;
        let mut worst_distance = 0.0_f64;
        for (offset, point) in points[start + 1..end].iter().enumerate() {
            let distance = perpendicular_distance(*point, points[start], points[end]);
            if distance > worst_distance {
                worst_distance = distance;
                worst_index = start + 1 + offset;
            }
        }
        if worst_distance > tolerance_km {
            keep[worst_index] = true;
            stack.push((start, worst_index));
            stack.push((worst_index, end));
        }
    }

    points
        .iter()
        .zip(keep)
        .filter_map(|(point, keep)| keep.then_some(*point))
        .collect()
}

fn perpendicular_distance(point: [f64; 2], start: [f64; 2], end: [f64; 2]) -> f64 {
    let dx = end[0] - start[0];
    let dy = end[1] - start[1];
    let length_squared = dx * dx + dy * dy;
    if length_squared <= f64::EPSILON {
        return (point[0] - start[0]).hypot(point[1] - start[1]);
    }
    let cross = (point[0] - start[0]) * dy - (point[1] - start[1]) * dx;
    cross.abs() / length_squared.sqrt()
}

/// Expand a polyline into a triangle strip of quads, two triangles per
/// segment. Joins are left as butt ends: at these widths the gap is
/// sub-pixel, and mitre generation would cost more than it shows.
fn tessellate_polyline(
    points: &[[f64; 2]],
    half_width_px: f32,
    color: [u8; 4],
    vertices: &mut Vec<MapVertex>,
    indices: &mut Vec<u32>,
) {
    for window in points.windows(2) {
        let [start, end] = [window[0], window[1]];
        let dx = end[0] - start[0];
        let dy = end[1] - start[1];
        let length = dx.hypot(dy);
        if length <= f64::EPSILON {
            continue;
        }
        // Unit perpendicular in world space.
        let nx = (-dy / length) as f32;
        let ny = (dx / length) as f32;
        let base = vertices.len() as u32;
        for (position, sign) in [(start, 1.0_f32), (start, -1.0), (end, 1.0), (end, -1.0)] {
            vertices.push(MapVertex {
                position_km: [position[0] as f32, position[1] as f32],
                normal: [nx * sign, ny * sign],
                half_width_px,
                color,
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base + 2, base + 1, base + 3]);
    }
}

/// Project the label candidates that fall inside the retained region.
///
/// The FALLIBLE projection, deliberately. `lon_lat_to_world` collapses a
/// geodesic that does not converge onto the anchor, and the anchor is inside
/// every region, so the infallible call silently piles every unresolvable
/// place name onto the radar itself. Measured from KTLX against the shipped
/// dataset that is zero names today, because the antipode of a radar in the
/// contiguous United States is empty ocean - but it is zero by luck of
/// geography, not by construction, and a radar in the western Pacific would
/// stack Atlantic place names on the antenna.
fn project_labels(request: &MapBuildRequest, half_extent_km: f64) -> Vec<ProjectedLabel> {
    request
        .dataset
        .labels
        .iter()
        .filter_map(|label| {
            let world = request
                .projection
                .try_lon_lat_to_world(f64::from(label.lon), f64::from(label.lat))?;
            let inside = is_inside([world.east_km, world.north_km], half_extent_km);
            inside.then_some(ProjectedLabel {
                class: label.class,
                name: label.name,
                east_km: world.east_km as f32,
                north_km: world.north_km as f32,
                rank: label.rank,
            })
        })
        .collect()
}

fn pack_color(color: [f32; 4]) -> [u8; 4] {
    color.map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// The LOD bucket a camera scale selects, without hysteresis. Callers that
/// interact use `LodSelector` instead; this is for one-shot builds and tests.
pub fn bucket_for_scale(km_per_point: f32) -> LodBucket {
    LodBucket::ideal(km_per_point, LOD_REFERENCE_KM_PER_POINT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::Generation;

    use crate::dataset::GeoLineFeature;

    fn key(lod: LodBucket) -> GeometryCacheKey {
        GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod,
        }
    }

    fn line(layer: MapLayer, points: &'static [(f32, f32)]) -> GeoLineFeature {
        let mut bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for (lon, lat) in points {
            bbox[0] = bbox[0].min(*lon);
            bbox[1] = bbox[1].min(*lat);
            bbox[2] = bbox[2].max(*lon);
            bbox[3] = bbox[3].max(*lat);
        }
        GeoLineFeature {
            layer,
            bbox,
            points,
        }
    }

    static NEAR_KTLX: &[(f32, f32)] = &[
        (-97.5, 35.2),
        (-97.4, 35.25),
        (-97.3, 35.3),
        (-97.2, 35.35),
        (-97.1, 35.4),
    ];
    static FAR_AWAY: &[(f32, f32)] = &[(2.0, 48.0), (2.1, 48.1)];

    fn request(lod: LodBucket) -> MapBuildRequest {
        MapBuildRequest {
            key: key(lod),
            dataset: MapDataset::from_parts(
                Generation::new(1),
                vec![
                    line(MapLayer::County, NEAR_KTLX),
                    line(MapLayer::County, FAR_AWAY),
                ],
                Vec::new(),
                Vec::new(),
            ),
            projection: RadarProjection::new(35.3333, -97.2778),
            style: MapStyle::default(),
        }
    }

    #[test]
    fn builds_triangles_for_nearby_features_and_culls_distant_ones() {
        let geometry = build_geometry(&request(LodBucket(-4)));
        assert!(geometry.vertex_count() > 0);
        assert_eq!(geometry.index_count() % 3, 0);
        assert_eq!(geometry.stats.features_culled, 1, "Paris should be culled");
        assert!(geometry.stats.features_built >= 1);
        assert_eq!(geometry.draws.len(), 1);
        assert_eq!(geometry.draws[0].layer, MapLayer::County);
    }

    #[test]
    fn a_coarser_bucket_retains_fewer_points() {
        let fine = build_geometry(&request(LodBucket(-6)));
        let coarse = build_geometry(&request(LodBucket(2)));
        assert!(
            coarse.stats.retained_points <= fine.stats.retained_points,
            "coarse {} should not exceed fine {}",
            coarse.stats.retained_points,
            fine.stats.retained_points
        );
    }

    #[test]
    fn county_lines_vanish_at_continental_scale() {
        // LodBucket(12) is far coarser than the county visibility ceiling.
        let geometry = build_geometry(&request(LodBucket(12)));
        assert!(geometry.is_empty());
        assert!(geometry.draws.is_empty());
    }

    #[test]
    fn a_feature_leaving_the_region_is_cut_at_the_boundary() {
        // A line running from beside the radar out to the far side of the
        // world. Every retained point must stay inside the build region: a
        // single distant vertex would rule a false line across the display.
        static LONG: &[(f32, f32)] = &[(-97.3, 35.3), (-97.0, 35.5), (2.35, 48.85), (30.0, 50.0)];
        let projection = RadarProjection::new(35.3333, -97.2778);
        let feature = line(MapLayer::County, LONG);
        let half_extent = build_half_extent_km(LodBucket(-4));
        let runs = project_and_clip(&feature, &projection, half_extent);

        assert!(!runs.is_empty(), "the nearby portion should survive");
        for run in &runs {
            for point in run {
                assert!(
                    is_inside(*point, half_extent),
                    "retained point {point:?} escaped the build region"
                );
            }
        }
    }

    #[test]
    fn a_feature_that_re_enters_produces_separate_runs() {
        // Near, far, near again: two runs, never one line joining them.
        static RE_ENTERS: &[(f32, f32)] = &[
            (-97.3, 35.3),
            (-97.2, 35.4),
            (2.35, 48.85),
            (-97.1, 35.2),
            (-97.0, 35.25),
        ];
        let projection = RadarProjection::new(35.3333, -97.2778);
        let runs = project_and_clip(
            &line(MapLayer::County, RE_ENTERS),
            &projection,
            build_half_extent_km(LodBucket(-4)),
        );
        assert_eq!(runs.len(), 2, "expected the excursion to split the feature");
    }

    /// The shipped basemap, from a real anchor, still builds the same square
    /// retained field at every bucket - including the coarse ones the globe is
    /// drawn at.
    ///
    /// The globe does NOT cull here. It cannot: one vertex buffer is drawn
    /// across the whole scale range its bucket covers, and the limb moves with
    /// the live camera scale AND with the size of the pane, so any radius this
    /// function could pick would be wrong at one end of the bucket or on one
    /// pane. The far hemisphere is hidden where the limb is actually known -
    /// at draw time, by `globe::limb_fade` and `globe::warp_world`.
    #[test]
    fn the_real_basemap_builds_inside_the_shipped_square_at_every_bucket() {
        let projection = RadarProjection::new(35.333_049_774_169_92, -97.277_748_107_910_16);
        let dataset = MapDataset::from_generated(analyst_runtime::Generation::new(1));
        for bucket in [-6_i16, -4, -2, 0, 2, 4, 6, 8, 12, 13, 14] {
            let lod = LodBucket(bucket);
            let geometry = build_geometry(&MapBuildRequest {
                key: key(lod),
                dataset: dataset.clone(),
                projection,
                style: MapStyle::default(),
            });
            let half = build_half_extent_km(lod);
            for vertex in geometry.vertices.iter() {
                assert!(
                    f64::from(vertex.position_km[0]).abs() <= half + 1.0
                        && f64::from(vertex.position_km[1]).abs() <= half + 1.0
                );
            }
            assert!(geometry.vertex_count() > 0, "bucket {bucket} built nothing");
        }
    }

    #[test]
    fn a_label_the_geodesic_cannot_resolve_is_dropped_rather_than_stacked_on_the_radar() {
        // A place exactly at the antipode of the anchor: Vincenty does not
        // converge there, and the infallible projection used to answer with the
        // origin, which is the antenna.
        let projection = RadarProjection::new(0.0, 0.0);
        let dataset = MapDataset::from_parts(
            analyst_runtime::Generation::new(1),
            Vec::new(),
            Vec::new(),
            vec![crate::dataset::LabelCandidate {
                class: crate::dataset::LabelClass::Place,
                name: "Antipode",
                lon: 180.0,
                lat: 0.0,
                rank: 0,
            }],
        );
        let geometry = build_geometry(&MapBuildRequest {
            key: key(LodBucket(12)),
            dataset,
            projection,
            style: MapStyle::default(),
        });
        assert!(
            geometry.labels.is_empty(),
            "an unresolvable label reached the map at {:?}",
            geometry.labels.first().map(|l| (l.east_km, l.north_km))
        );
    }

    #[test]
    fn simplification_keeps_the_endpoints() {
        let points = vec![
            [0.0, 0.0],
            [1.0, 0.001],
            [2.0, 0.0],
            [3.0, 0.002],
            [4.0, 0.0],
        ];
        let simplified = simplify(&points, 0.5);
        assert_eq!(simplified.first(), Some(&[0.0, 0.0]));
        assert_eq!(simplified.last(), Some(&[4.0, 0.0]));
        assert!(simplified.len() < points.len());
    }

    #[test]
    fn simplification_preserves_a_sharp_corner() {
        let points = vec![[0.0, 0.0], [5.0, 0.0], [5.0, 5.0]];
        let simplified = simplify(&points, 0.5);
        assert_eq!(simplified.len(), 3, "the corner must survive");
    }

    #[test]
    fn every_segment_emits_two_triangles() {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        tessellate_polyline(
            &[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            0.5,
            [1, 2, 3, 4],
            &mut vertices,
            &mut indices,
        );
        assert_eq!(vertices.len(), 8, "four vertices per segment");
        assert_eq!(indices.len(), 12, "six indices per segment");
    }

    #[test]
    fn zero_length_segments_are_dropped() {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        tessellate_polyline(
            &[[0.0, 0.0], [0.0, 0.0]],
            0.5,
            [1, 2, 3, 4],
            &mut vertices,
            &mut indices,
        );
        assert!(vertices.is_empty());
        assert!(indices.is_empty());
    }

    #[test]
    fn normals_are_unit_length_and_opposed_across_the_line() {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        tessellate_polyline(
            &[[0.0, 0.0], [3.0, 4.0]],
            1.0,
            [0; 4],
            &mut vertices,
            &mut indices,
        );
        let length = vertices[0].normal[0].hypot(vertices[0].normal[1]);
        assert!((length - 1.0).abs() < 1e-5, "normal length was {length}");
        assert_eq!(vertices[0].normal[0], -vertices[1].normal[0]);
        assert_eq!(vertices[0].normal[1], -vertices[1].normal[1]);
    }
}
