//! Persistent GPU resources and the egui paint callback.
//!
//! Buffers live in the renderer's callback-resource store for the life of the
//! window. A frame that pans or zooms inside a LOD bucket writes one small
//! uniform and issues the same draw calls over the same buffers: no vertex is
//! rewritten and nothing is uploaded. Uploads happen only when a geometry
//! generation the GPU has not seen becomes visible.

use std::collections::HashMap;
use std::sync::Arc;

use analyst_runtime::{Camera2D, GeometryCacheKey, MAX_PANES, ViewportMetrics};
use eframe::egui_wgpu::{self, CallbackTrait, ScreenDescriptor};
use eframe::wgpu::{self, util::DeviceExt};

use crate::geometry::{MapGeometry, MapVertex};
use crate::residency::{Admission, GeometryResidency, ResidencyMetrics};

// The raster tile underlay's GPU half lives next door but is registered and
// used through this module, so a caller has one place to look for "the map's
// GPU resources" whichever layer it means.
pub use crate::tile_gpu::{
    TileDrawUniform, TilePaintCallback, TilePaneUniform, TileRenderResources, TileResidencyMetrics,
};

/// Uniform block. `repr(C)` and 16-byte aligned to match the WGSL layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MapUniform {
    pub world_to_clip: [[f32; 4]; 4],
    pub viewport_px: [f32; 2],
    pub pixels_per_point: f32,
    /// How far this pane has been carried onto the orthographic globe, from
    /// `crate::projection::globe::blend_for_pane`. It takes the slot that used
    /// to be `_pad`, so the block's size and 16-byte alignment are unchanged.
    /// Zero at every scale an analyst works at, where the shader's morph is an
    /// early return.
    pub globe_blend: f32,
}

/// Vertex as the GPU sees it. The CPU type is already `repr(C)`; this newtype
/// only adds the `Pod` proof, keeping unsafe casts out of the build code.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuVertex {
    position_km: [f32; 2],
    normal: [f32; 2],
    half_width_px: f32,
    color: [u8; 4],
}

impl From<&MapVertex> for GpuVertex {
    fn from(vertex: &MapVertex) -> Self {
        Self {
            position_km: vertex.position_km,
            normal: vertex.normal,
            half_width_px: vertex.half_width_px,
            color: vertex.color,
        }
    }
}

struct ResidentGeometry {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// Everything the map needs on the GPU, registered once at startup.
pub struct MapRenderResources {
    pipeline: wgpu::RenderPipeline,
    /// One uniform buffer and bind group per pane: panes paint in the same
    /// frame with different cameras, so they cannot share a single buffer.
    uniforms: Vec<wgpu::Buffer>,
    bind_groups: Vec<wgpu::BindGroup>,
    geometries: HashMap<GeometryCacheKey, ResidentGeometry>,
    residency: GeometryResidency,
}

impl MapRenderResources {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("map_scene shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("map_scene bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("map_scene pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("map_scene pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 8,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32,
                            offset: 16,
                            shader_location: 2,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Unorm8x4,
                            offset: 20,
                            shader_location: 3,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // The shader emits premultiplied alpha, matching how egui
                    // composites its own primitives.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // Line quads are emitted in whichever winding the segment
                // direction produces, so culling would drop half of them.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let mut uniforms = Vec::with_capacity(MAX_PANES);
        let mut bind_groups = Vec::with_capacity(MAX_PANES);
        for pane in 0..MAX_PANES {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("map_scene uniform"),
                size: std::mem::size_of::<MapUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("map_scene bind group"),
                layout: &bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            let _ = pane;
            uniforms.push(buffer);
            bind_groups.push(bind_group);
        }

        Self {
            pipeline,
            uniforms,
            bind_groups,
            geometries: HashMap::new(),
            residency: GeometryResidency::default(),
        }
    }

    pub fn metrics(&self) -> ResidencyMetrics {
        self.residency.metrics()
    }

    /// Upload `geometry` if this generation is not already resident.
    fn ensure_resident(&mut self, device: &wgpu::Device, geometry: &MapGeometry) {
        match self.residency.touch(geometry.key, geometry.estimated_bytes) {
            Admission::AlreadyResident => {}
            Admission::Rejected => {}
            Admission::Admitted { evicted } => {
                for key in evicted {
                    self.geometries.remove(&key);
                }
                let vertices: Vec<GpuVertex> =
                    geometry.vertices.iter().map(GpuVertex::from).collect();
                let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("map_scene vertices"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
                let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("map_scene indices"),
                    contents: bytemuck::cast_slice(&geometry.indices),
                    usage: wgpu::BufferUsages::INDEX,
                });
                self.geometries.insert(
                    geometry.key,
                    ResidentGeometry {
                        vertices: vertex_buffer,
                        indices: index_buffer,
                        index_count: geometry.indices.len() as u32,
                    },
                );
            }
        }
    }
}

/// One pane's map draw for one frame.
///
/// Holds an `Arc` to the geometry so the generation cannot be freed mid-frame,
/// plus the camera state needed to build a uniform. Cheap to clone.
pub struct MapPaintCallback {
    pub pane_index: usize,
    pub geometry: Arc<MapGeometry>,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
    /// Pane rectangle in physical pixels, used to build the projection matrix.
    pub rect_px: [f32; 4],
}

impl MapPaintCallback {
    /// `pub(crate)` so the tile layer's own test can assert that both layers
    /// build the identical camera matrix; nothing outside this crate needs it.
    pub(crate) fn uniform(&self) -> MapUniform {
        let camera = self.camera.sanitized();
        let viewport = self.viewport.sanitized();
        let width_px = (self.rect_px[2] - self.rect_px[0]).max(1.0);
        let height_px = (self.rect_px[3] - self.rect_px[1]).max(1.0);

        // World kilometres -> pane-local points -> clip. Screen y grows
        // downward and clip y grows upward, hence the negated row.
        let scale = 1.0 / camera.km_per_point.max(f32::MIN_POSITIVE);
        let (sin, cos) = camera.rotation_rad.sin_cos();
        let half_width_points = width_px / viewport.pixels_per_point * 0.5;
        let half_height_points = height_px / viewport.pixels_per_point * 0.5;

        // Camera-relative world offset, rotated, scaled to points, then
        // normalised into clip space by the pane's half extents.
        let sx = scale / half_width_points;
        let sy = scale / half_height_points;
        let cx = camera.center_east_km as f32;
        let cy = camera.center_north_km as f32;

        // Column-major for WGSL: columns are the images of the basis vectors.
        let m00 = cos * sx;
        let m10 = sin * sx;
        let m01 = -sin * sy;
        let m11 = cos * sy;
        let tx = -(m00 * cx + m01 * cy);
        let ty = -(m10 * cx + m11 * cy);

        MapUniform {
            world_to_clip: [
                [m00, m10, 0.0, 0.0],
                [m01, m11, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [tx, ty, 0.0, 1.0],
            ],
            viewport_px: [width_px, height_px],
            pixels_per_point: viewport.pixels_per_point,
            // `camera` and `viewport` are the sanitized locals the matrix above
            // was built from, so the blend the shader morphs by is the same
            // number the marker and label layers derive from the same two
            // values. The VIEWPORT is part of it: a globe fits a 900-point
            // pane at 15.7 km/point and a 450-point pane at 31.5, so a handoff
            // that ignored the pane would be wrong on one of them.
            globe_blend: crate::projection::globe::blend_for_pane(camera.km_per_point, viewport),
        }
    }
}

impl CallbackTrait for MapPaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(resources) = callback_resources.get_mut::<MapRenderResources>() else {
            return Vec::new();
        };
        resources.ensure_resident(device, &self.geometry);
        if let Some(buffer) = resources.uniforms.get(self.pane_index) {
            queue.write_buffer(buffer, 0, bytemuck::bytes_of(&self.uniform()));
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(resources) = callback_resources.get::<MapRenderResources>() else {
            return;
        };
        let Some(resident) = resources.geometries.get(&self.geometry.key) else {
            return;
        };
        let Some(bind_group) = resources.bind_groups.get(self.pane_index) else {
            return;
        };
        if resident.index_count == 0 {
            return;
        }

        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_vertex_buffer(0, resident.vertices.slice(..));
        render_pass.set_index_buffer(resident.indices.slice(..), wgpu::IndexFormat::Uint32);
        for draw in self.geometry.draws.iter() {
            let end = draw.index_start + draw.index_count;
            if end <= resident.index_count {
                render_pass.draw_indexed(draw.index_start..end, 0, 0..1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{Generation, LodBucket};

    use crate::geometry::GeometryStats;

    fn geometry() -> Arc<MapGeometry> {
        Arc::new(MapGeometry::new(
            GeometryCacheKey {
                dataset: Generation::new(1),
                projection: Generation::new(1),
                style: Generation::new(1),
                lod: LodBucket(0),
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            GeometryStats::default(),
        ))
    }

    fn callback(camera: Camera2D) -> MapPaintCallback {
        MapPaintCallback {
            pane_index: 0,
            geometry: geometry(),
            camera,
            viewport: ViewportMetrics {
                width_points: 800.0,
                height_points: 600.0,
                pixels_per_point: 1.0,
            },
            rect_px: [0.0, 0.0, 800.0, 600.0],
        }
    }

    /// Apply the uniform the way the vertex shader does.
    fn project(uniform: &MapUniform, world: [f32; 2]) -> [f32; 2] {
        let m = uniform.world_to_clip;
        [
            m[0][0] * world[0] + m[1][0] * world[1] + m[3][0],
            m[0][1] * world[0] + m[1][1] * world[1] + m[3][1],
        ]
    }

    #[test]
    fn the_camera_centre_lands_at_clip_origin() {
        let camera = Camera2D {
            center_east_km: 25.0,
            center_north_km: -40.0,
            km_per_point: 0.5,
            rotation_rad: 0.0,
        };
        let clip = project(&callback(camera).uniform(), [25.0, -40.0]);
        assert!(clip[0].abs() < 1e-5 && clip[1].abs() < 1e-5, "got {clip:?}");
    }

    #[test]
    fn north_is_up_and_east_is_right() {
        let camera = Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 1.0,
            rotation_rad: 0.0,
        };
        let uniform = callback(camera).uniform();
        let east = project(&uniform, [10.0, 0.0]);
        let north = project(&uniform, [0.0, 10.0]);
        assert!(east[0] > 0.0, "east should be +x, got {east:?}");
        assert!(north[1] > 0.0, "north should be +y in clip, got {north:?}");
    }

    #[test]
    fn a_pane_width_of_world_maps_to_the_clip_cube() {
        // 800 points wide at 1 km/point: 400 km right of centre is the edge.
        let camera = Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 1.0,
            rotation_rad: 0.0,
        };
        let clip = project(&callback(camera).uniform(), [400.0, 0.0]);
        assert!((clip[0] - 1.0).abs() < 1e-4, "got {clip:?}");
    }

    #[test]
    fn panning_changes_only_the_translation_column() {
        let base = callback(Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 0.5,
            rotation_rad: 0.0,
        })
        .uniform();
        let panned = callback(Camera2D {
            center_east_km: 137.0,
            center_north_km: -88.0,
            km_per_point: 0.5,
            rotation_rad: 0.0,
        })
        .uniform();

        // The linear part is untouched; only translation moves. This is the
        // matrix-level statement of "a pan is a uniform update".
        assert_eq!(base.world_to_clip[0], panned.world_to_clip[0]);
        assert_eq!(base.world_to_clip[1], panned.world_to_clip[1]);
        assert_ne!(base.world_to_clip[3], panned.world_to_clip[3]);
    }

    #[test]
    fn rotation_turns_the_basis_without_changing_scale() {
        let uniform = callback(Camera2D {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_point: 1.0,
            rotation_rad: std::f32::consts::FRAC_PI_2,
        })
        .uniform();
        // A quarter turn sends east to the vertical axis.
        let east = project(&uniform, [10.0, 0.0]);
        assert!(east[0].abs() < 1e-5, "east kept an x component: {east:?}");
        assert!(east[1].abs() > 0.0);
    }
}
