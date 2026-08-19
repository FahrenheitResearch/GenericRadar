//! Immutable geographic source features.
//!
//! The generated tables are `&'static` data compiled into the binary, so the
//! dataset borrows them rather than copying nine megabytes onto the heap. Only
//! the small per-feature descriptors are owned.

use std::sync::Arc;

use analyst_runtime::Generation;

use crate::generated::basemap_data as generated;

/// Draw layers, ordered from least to most detailed. The discriminant is the
/// paint order within the map underlay.
///
/// US boundaries are split by level because the source generalises each level
/// independently: the same coastline exists in the country, state and county
/// tables with slightly different vertices. Drawing two of them together
/// renders one shoreline as a pair of offset lines, so the style shows exactly
/// one US level at any scale. Foreign administrative boundaries have no
/// county-level counterpart and so never double.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MapLayer {
    Country,
    ForeignAdmin,
    State,
    County,
}

impl MapLayer {
    pub const ALL: [Self; 4] = [Self::Country, Self::ForeignAdmin, Self::State, Self::County];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Country => "country",
            Self::ForeignAdmin => "admin",
            Self::State => "state",
            Self::County => "county",
        }
    }
}

/// Label importance class. Placement prefers lower values.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LabelClass {
    Place,
    County,
    Admin,
}

/// A geographic polyline in lon/lat degrees, with a precomputed bounding box
/// of `[min_lon, min_lat, max_lon, max_lat]`.
#[derive(Clone, Copy, Debug)]
pub struct GeoLineFeature {
    pub layer: MapLayer,
    pub bbox: [f32; 4],
    pub points: &'static [(f32, f32)],
}

/// A closed geographic ring in lon/lat degrees. The basemap has none today;
/// warning polygons and placefiles arrive on this layer later, and the stress
/// benchmark builds them synthetically.
#[derive(Clone, Debug)]
pub struct GeoPolygonFeature {
    pub bbox: [f32; 4],
    pub ring: Arc<[(f32, f32)]>,
}

#[derive(Clone, Copy, Debug)]
pub struct LabelCandidate {
    pub class: LabelClass,
    pub name: &'static str,
    pub lon: f32,
    pub lat: f32,
    /// Lower ranks are more important.
    pub rank: u8,
}

/// One immutable snapshot of the map source data.
///
/// `generation` is the dataset half of a `GeometryCacheKey`. Replacing the
/// dataset produces a new generation, which invalidates retained geometry.
#[derive(Clone)]
pub struct MapDataset {
    pub generation: Generation,
    pub lines: Arc<[GeoLineFeature]>,
    pub polygons: Arc<[GeoPolygonFeature]>,
    pub labels: Arc<[LabelCandidate]>,
    pub estimated_bytes: usize,
}

impl MapDataset {
    /// Build the dataset from the compiled-in basemap tables.
    pub fn from_generated(generation: Generation) -> Self {
        let mut lines = Vec::new();
        push_lines(
            &mut lines,
            MapLayer::Country,
            generated::BASEMAP_WORLD_COUNTRY_LINES,
        );
        push_lines(
            &mut lines,
            MapLayer::State,
            generated::BASEMAP_US_STATE_LINES,
        );
        push_lines(
            &mut lines,
            MapLayer::ForeignAdmin,
            generated::BASEMAP_CANADA_ADMIN_LINES,
        );
        push_lines(
            &mut lines,
            MapLayer::ForeignAdmin,
            generated::BASEMAP_MEXICO_ADMIN_LINES,
        );
        push_lines(
            &mut lines,
            MapLayer::ForeignAdmin,
            generated::BASEMAP_JAPAN_ADMIN_LINES,
        );
        push_lines(
            &mut lines,
            MapLayer::County,
            generated::BASEMAP_US_COUNTY_LINES,
        );

        let mut labels = Vec::new();
        push_labels(
            &mut labels,
            LabelClass::Place,
            generated::BASEMAP_WORLD_PLACE_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::Place,
            generated::BASEMAP_US_PLACE_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::Place,
            generated::BASEMAP_CANADA_PLACE_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::Place,
            generated::BASEMAP_MEXICO_PLACE_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::County,
            generated::BASEMAP_US_COUNTY_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::Admin,
            generated::BASEMAP_CANADA_ADMIN_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::Admin,
            generated::BASEMAP_MEXICO_ADMIN_LABELS,
        );
        push_labels(
            &mut labels,
            LabelClass::Admin,
            generated::BASEMAP_JAPAN_ADMIN_LABELS,
        );

        Self::from_parts(generation, lines, Vec::new(), labels)
    }

    /// Build a dataset from explicit features. Used by tests and benchmarks.
    pub fn from_parts(
        generation: Generation,
        lines: Vec<GeoLineFeature>,
        polygons: Vec<GeoPolygonFeature>,
        labels: Vec<LabelCandidate>,
    ) -> Self {
        let estimated_bytes = estimate_bytes(&lines, &polygons, &labels);
        Self {
            generation,
            lines: lines.into(),
            polygons: polygons.into(),
            labels: labels.into(),
            estimated_bytes,
        }
    }

    pub fn empty(generation: Generation) -> Self {
        Self::from_parts(generation, Vec::new(), Vec::new(), Vec::new())
    }

    /// Total vertex count across every line, for budgeting and reporting.
    pub fn line_point_count(&self) -> usize {
        self.lines.iter().map(|line| line.points.len()).sum()
    }
}

fn push_lines(
    output: &mut Vec<GeoLineFeature>,
    layer: MapLayer,
    source: &'static [generated::BasemapLine],
) {
    output.extend(source.iter().map(|line| GeoLineFeature {
        layer,
        bbox: line.bbox,
        points: line.points,
    }));
}

fn push_labels(
    output: &mut Vec<LabelCandidate>,
    class: LabelClass,
    source: &'static [generated::BasemapLabel],
) {
    output.extend(source.iter().map(|label| LabelCandidate {
        class,
        name: label.name,
        lon: label.lon,
        lat: label.lat,
        rank: label.rank,
    }));
}

/// Resident bytes owned by the dataset. The `&'static` point arrays live in
/// the binary image and are counted as borrowed, not owned.
fn estimate_bytes(
    lines: &[GeoLineFeature],
    polygons: &[GeoPolygonFeature],
    labels: &[LabelCandidate],
) -> usize {
    let line_bytes = std::mem::size_of_val(lines);
    let polygon_bytes = std::mem::size_of_val(polygons)
        + polygons
            .iter()
            .map(|polygon| std::mem::size_of_val(&*polygon.ring))
            .sum::<usize>();
    let label_bytes = std::mem::size_of_val(labels);
    line_bytes + polygon_bytes + label_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_dataset_carries_every_layer() {
        let dataset = MapDataset::from_generated(Generation::new(1));
        for layer in MapLayer::ALL {
            let count = dataset
                .lines
                .iter()
                .filter(|line| line.layer == layer)
                .count();
            assert!(count > 0, "{} layer had no features", layer.label());
        }
        assert!(dataset.line_point_count() > 100_000);
        assert!(!dataset.labels.is_empty());
        assert!(dataset.estimated_bytes > 0);
    }

    #[test]
    fn bounding_boxes_contain_their_points() {
        let dataset = MapDataset::from_generated(Generation::new(1));
        for line in dataset.lines.iter().take(2_000) {
            let [min_lon, min_lat, max_lon, max_lat] = line.bbox;
            for (lon, lat) in line.points {
                assert!(
                    *lon >= min_lon && *lon <= max_lon && *lat >= min_lat && *lat <= max_lat,
                    "point ({lon}, {lat}) escaped bbox {:?}",
                    line.bbox
                );
            }
        }
    }

    #[test]
    fn an_empty_dataset_is_valid_and_cheap() {
        let dataset = MapDataset::empty(Generation::new(1));
        assert_eq!(dataset.line_point_count(), 0);
        assert_eq!(dataset.estimated_bytes, 0);
    }
}
