//! Packed world-space geometry.
//!
//! Vertices are radar-local kilometres plus an expansion normal. Stroke width
//! is applied in the shader from a pixel width, so a zoom or pan never
//! rewrites a vertex: the same buffers are drawn under a different camera
//! matrix. Nothing here is in screen space and nothing here is an egui shape.

use std::sync::Arc;

use analyst_runtime::GeometryCacheKey;

use crate::dataset::{LabelClass, MapLayer};

/// One expanded line vertex.
///
/// `normal` is the unit perpendicular of the segment in world space. The
/// shader normalises it after the camera's linear transform, which removes
/// zoom, leaving a screen-space direction that is then offset by
/// `half_width_px` pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapVertex {
    pub position_km: [f32; 2],
    pub normal: [f32; 2],
    pub half_width_px: f32,
    pub color: [u8; 4],
}

impl MapVertex {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// A contiguous index range drawn with one layer's style.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapDraw {
    pub layer: MapLayer,
    pub index_start: u32,
    pub index_count: u32,
}

/// A label already projected into world kilometres, ready for bounded
/// placement without touching geographic coordinates again.
#[derive(Clone, Copy, Debug)]
pub struct ProjectedLabel {
    pub class: LabelClass,
    pub name: &'static str,
    pub east_km: f32,
    pub north_km: f32,
    pub rank: u8,
}

/// Counters that make the pan-invariance claim testable rather than asserted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GeometryStats {
    pub source_points: usize,
    pub retained_points: usize,
    pub features_built: usize,
    pub features_culled: usize,
}

/// Immutable retained geometry for one `GeometryCacheKey`.
pub struct MapGeometry {
    pub key: GeometryCacheKey,
    pub vertices: Arc<[MapVertex]>,
    pub indices: Arc<[u32]>,
    pub draws: Arc<[MapDraw]>,
    pub labels: Arc<[ProjectedLabel]>,
    pub estimated_bytes: usize,
    pub stats: GeometryStats,
}

impl MapGeometry {
    pub fn new(
        key: GeometryCacheKey,
        vertices: Vec<MapVertex>,
        indices: Vec<u32>,
        draws: Vec<MapDraw>,
        labels: Vec<ProjectedLabel>,
        stats: GeometryStats,
    ) -> Self {
        let estimated_bytes = vertices.len() * MapVertex::SIZE
            + indices.len() * std::mem::size_of::<u32>()
            + draws.len() * std::mem::size_of::<MapDraw>()
            + labels.len() * std::mem::size_of::<ProjectedLabel>();
        Self {
            key,
            vertices: vertices.into(),
            indices: indices.into(),
            draws: draws.into(),
            labels: labels.into(),
            estimated_bytes,
            stats,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn index_count(&self) -> usize {
        self.indices.len()
    }

    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{Generation, LodBucket};

    fn key() -> GeometryCacheKey {
        GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod: LodBucket(0),
        }
    }

    #[test]
    fn byte_accounting_covers_every_owned_buffer() {
        let vertex = MapVertex {
            position_km: [1.0, 2.0],
            normal: [0.0, 1.0],
            half_width_px: 0.5,
            color: [255, 255, 255, 255],
        };
        let geometry = MapGeometry::new(
            key(),
            vec![vertex; 4],
            vec![0, 1, 2, 0, 2, 3],
            vec![MapDraw {
                layer: MapLayer::County,
                index_start: 0,
                index_count: 6,
            }],
            Vec::new(),
            GeometryStats::default(),
        );
        let expected =
            4 * MapVertex::SIZE + 6 * std::mem::size_of::<u32>() + std::mem::size_of::<MapDraw>();
        assert_eq!(geometry.estimated_bytes, expected);
        assert_eq!(geometry.vertex_count(), 4);
        assert_eq!(geometry.index_count(), 6);
        assert!(!geometry.is_empty());
    }

    #[test]
    fn the_vertex_stays_compact() {
        // Guards against a casual field addition doubling every buffer.
        assert_eq!(MapVertex::SIZE, 24);
    }
}
