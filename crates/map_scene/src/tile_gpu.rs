//! GPU resources and the egui paint callback for the raster tile basemap.
//!
//! Shaped exactly like [`crate::gpu`], which draws the vector map: persistent
//! resources live in the renderer's callback-resource store for the life of
//! the window, `prepare` decides what has to be uploaded, and `paint` issues
//! draws over what is already resident. The two layers are separate pipelines
//! in the same render pass, and the paint order in `pane_canvas` is what puts
//! imagery under boundaries.
//!
//! Three bind groups, in the order their contents change:
//!
//! * group 0 — the pane's camera matrix and scrim. One buffer per pane,
//!   because panes paint in the same frame with different cameras.
//! * group 1 — the per-tile UV window and fade, in one dynamic-offset uniform
//!   buffer written once per frame. Not wgpu immediate data: `Features::IMMEDIATE`
//!   plus a non-zero `Limits::max_immediate_size` are both required for that
//!   and eframe's default device request enables neither.
//! * group 2 — the tile's texture and the shared sampler. One bind group per
//!   resident tile, so the residency set and the draw list stay independent.
//!
//! Tile *textures* are retained and LRU-evicted, exactly as geometry is. Tile
//! *meshes* are packed into one vertex and one index buffer per pane and
//! rewritten each frame: the visible tile set changes on every pan, so there
//! is nothing to retain, and the whole packed mesh is 16 KiB in the common
//! case (four vertices per tile at z11 and finer) and about 330 KiB at the
//! coarsest zoom this layer draws.

use std::collections::HashMap;
use std::sync::Arc;

use analyst_runtime::{Camera2D, MAX_PANES, ViewportMetrics};
use basemap_tiles::{DecodedTile, TileVertex};
use eframe::egui_wgpu::{self, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;

use crate::tiles::{MAX_DRAWS_PER_PANE, TileFrame, TileKey};

/// Per-pane uniform. `repr(C)`, 16-byte aligned, matching the WGSL layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TilePaneUniform {
    pub world_to_clip: [[f32; 4]; 4],
    /// `[r, g, b, a]`, straight alpha; mixed into the sampled texel.
    pub scrim: [f32; 4],
}

/// Per-draw uniform, written into a dynamic-offset buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TileDrawUniform {
    pub uv_offset_scale: [f32; 4],
    /// rgb multiplier, `a` = the tile's fade.
    pub tint: [f32; 4],
}

/// What the texture cache did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileResidencyMetrics {
    pub uploads: u64,
    pub evictions: u64,
    pub hits: u64,
    pub resident_tiles: usize,
    pub resident_bytes: usize,
    /// Draws skipped because their texture was not resident. Zero in steady
    /// state; a persistently non-zero value means the budget is thrashing and
    /// tiles are being evicted in the same frame they upload.
    pub skipped_draws: u64,
}

/// One tile's texture, its view, and the bind group that samples it.
struct ResidentTile {
    bind_group: wgpu::BindGroup,
    bytes: usize,
    last_used: u64,
}

/// One draw the pass will issue, decided in `prepare` and executed in `paint`.
struct PaneDraw {
    texture: TileKey,
    index_start: u32,
    index_count: u32,
    /// Slot in this pane's region of the draw-uniform buffer.
    slot: u32,
}

/// One pane's packed tile mesh for this frame.
struct PaneBuffers {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    vertex_capacity: usize,
    index_capacity: usize,
    draws: Vec<PaneDraw>,
}

/// Everything the tile layer needs on the GPU, registered once at startup.
pub struct TileRenderResources {
    pipeline: wgpu::RenderPipeline,
    /// Kept because a texture bind group is created per resident tile, long
    /// after construction. The pane and draw layouts are not: their bind
    /// groups are made once, here.
    texture_layout: wgpu::BindGroupLayout,
    pane_uniforms: Vec<wgpu::Buffer>,
    pane_groups: Vec<wgpu::BindGroup>,
    draw_uniform: wgpu::Buffer,
    draw_group: wgpu::BindGroup,
    draw_stride: u32,
    sampler: wgpu::Sampler,
    textures: HashMap<TileKey, ResidentTile>,
    panes: Vec<PaneBuffers>,
    budget_bytes: usize,
    resident_bytes: usize,
    clock: u64,
    metrics: TileResidencyMetrics,
}

impl TileRenderResources {
    /// 160 MiB, about 480 tiles at 256x256 RGBA with a four-level mip chain.
    ///
    /// A 1500x950 point pane at `pixels_per_point = 2` needs up to 196 tiles,
    /// which is 65 MiB; four panes at four *different* zooms would want 261
    /// MiB, which is more than the whole geometry budget. Panes share this
    /// cache — the key is provider and tile, never the pane — so the ordinary
    /// four-identical-panes case costs one tile set.
    pub const DEFAULT_BUDGET_BYTES: usize = 160 * 1024 * 1024;

    /// Texture format. Non-sRGB on purpose: see `tile_shader.wgsl`.
    pub const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    #[must_use]
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        Self::with_budget(device, target_format, Self::DEFAULT_BUDGET_BYTES)
    }

    #[must_use]
    pub fn with_budget(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        budget_bytes: usize,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("map_scene tile shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("tile_shader.wgsl").into()),
        });

        let pane_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("map_scene tile pane layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("map_scene tile draw layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<TileDrawUniform>() as u64
                    ),
                },
                count: None,
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("map_scene tile texture layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("map_scene tile pipeline layout"),
            bind_group_layouts: &[
                Some(&pane_layout),
                Some(&draw_layout),
                Some(&texture_layout),
            ],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("map_scene tile pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: TileVertex::SIZE as wgpu::BufferAddress,
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
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // The same blend as the vector map, so the two layers
                    // composite the same way onto the pane.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // The mesh winding follows the projection, which flips across
                // the anchor, so culling would drop half the ground.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let mut pane_uniforms = Vec::with_capacity(MAX_PANES);
        let mut pane_groups = Vec::with_capacity(MAX_PANES);
        for _ in 0..MAX_PANES {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("map_scene tile pane uniform"),
                size: std::mem::size_of::<TilePaneUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("map_scene tile pane bind group"),
                layout: &pane_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            pane_uniforms.push(buffer);
            pane_groups.push(group);
        }

        let draw_stride = draw_uniform_stride(device);
        let draw_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("map_scene tile draw uniform"),
            size: u64::from(draw_stride) * (MAX_PANES * MAX_DRAWS_PER_PANE) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("map_scene tile draw bind group"),
            layout: &draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &draw_uniform,
                    offset: 0,
                    size: wgpu::BufferSize::new(std::mem::size_of::<TileDrawUniform>() as u64),
                }),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("map_scene tile sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // One LOD bucket admits a 2.55x sweep of camera scale, so a tile is
            // minified by up to 2.03x while still inside the bucket that chose
            // it. Without mip filtering that shimmers during a zoom.
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let panes = (0..MAX_PANES).map(|_| PaneBuffers::new(device)).collect();

        Self {
            pipeline,
            texture_layout,
            pane_uniforms,
            pane_groups,
            draw_uniform,
            draw_group,
            draw_stride,
            sampler,
            textures: HashMap::new(),
            panes,
            budget_bytes: budget_bytes.max(1),
            resident_bytes: 0,
            clock: 0,
            metrics: TileResidencyMetrics::default(),
        }
    }

    #[must_use]
    pub fn metrics(&self) -> TileResidencyMetrics {
        TileResidencyMetrics {
            resident_tiles: self.textures.len(),
            resident_bytes: self.resident_bytes,
            ..self.metrics
        }
    }

    #[must_use]
    pub fn is_resident(&self, key: &TileKey) -> bool {
        self.textures.contains_key(key)
    }

    /// Upload a decoded tile if it is not already resident.
    ///
    /// Idempotent across panes: the same `uploads` list reaches every pane in
    /// the frame, and the second pane finds everything resident. Every upload
    /// and every eviction is reported through the frame's feedback channel,
    /// which is the scene layer's only way to learn that a tile it believes is
    /// drawable has lost its pixels.
    fn ensure_resident(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        decoded: &DecodedTile,
        frame: &TileFrame,
    ) {
        let key = (decoded.provider, decoded.tile);
        self.clock += 1;
        if let Some(resident) = self.textures.get_mut(&key) {
            resident.last_used = self.clock;
            self.metrics.hits += 1;
            // Already here, so the scene may stop holding the pixels.
            frame.feedback.record_upload(key);
            return;
        }

        let bytes = decoded.byte_len();
        if bytes == 0 || bytes > self.budget_bytes {
            return;
        }
        self.evict_until(bytes, frame);

        let levels = decoded.mip_level_count();
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("map_scene tile texture"),
            size: wgpu::Extent3d {
                width: decoded.level0_texels,
                height: decoded.level0_texels,
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::TEXTURE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for level in 0..levels {
            let Some((pixels, side)) = decoded.level(level) else {
                continue;
            };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(side * 4),
                    rows_per_image: Some(side),
                },
                wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("map_scene tile texture bind group"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.textures.insert(
            key,
            ResidentTile {
                bind_group,
                bytes,
                last_used: self.clock,
            },
        );
        self.resident_bytes += bytes;
        self.metrics.uploads += 1;
        frame.feedback.record_upload(key);
    }

    /// Evict least-recently-used tiles until `bytes` fits under the budget.
    fn evict_until(&mut self, bytes: usize, frame: &TileFrame) {
        while self.resident_bytes + bytes > self.budget_bytes && !self.textures.is_empty() {
            let Some(victim) = self
                .textures
                .iter()
                .min_by_key(|(_, resident)| resident.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(resident) = self.textures.remove(&victim) {
                self.resident_bytes = self.resident_bytes.saturating_sub(resident.bytes);
                self.metrics.evictions += 1;
                // The scene must hear about this or the tile is a permanent
                // hole: its store state would stay `Ready` with no pixels
                // anywhere.
                frame.feedback.record_eviction(victim);
            }
        }
    }

    /// Pack this pane's meshes and per-draw uniforms for the frame.
    fn prepare_pane(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pane_index: usize,
        frame: &TileFrame,
    ) {
        let Some(pane) = self.panes.get_mut(pane_index) else {
            return;
        };
        pane.draws.clear();

        let mut vertices: Vec<TileVertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut uniforms: Vec<u8> = Vec::new();
        let stride = self.draw_stride as usize;

        for draw in frame.draws.iter().take(MAX_DRAWS_PER_PANE) {
            let key: TileKey = (frame.key.provider, draw.texture);
            if !self.textures.contains_key(&key) {
                // Nothing to sample yet. The pane shows its own ground here,
                // which is today's behaviour, not a hole in a picture.
                self.metrics.skipped_draws += 1;
                continue;
            }
            let base = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
            let index_start = u32::try_from(indices.len()).unwrap_or(u32::MAX);
            vertices.extend_from_slice(&draw.mesh.vertices);
            indices.extend(draw.mesh.indices.iter().map(|index| index + base));
            let slot = u32::try_from(pane.draws.len()).unwrap_or(0);

            let uniform = TileDrawUniform {
                uv_offset_scale: draw.uv_offset_scale,
                tint: [1.0, 1.0, 1.0, draw.alpha.clamp(0.0, 1.0)],
            };
            uniforms.resize(slot as usize * stride, 0);
            uniforms.extend_from_slice(bytemuck::bytes_of(&uniform));
            uniforms.resize((slot as usize + 1) * stride, 0);

            pane.draws.push(PaneDraw {
                texture: key,
                index_start,
                index_count: u32::try_from(draw.mesh.indices.len()).unwrap_or(0),
                slot,
            });
        }

        if pane.draws.is_empty() || vertices.is_empty() || indices.is_empty() {
            pane.draws.clear();
            return;
        }
        pane.ensure_capacity(device, vertices.len(), indices.len());
        queue.write_buffer(&pane.vertices, 0, bytemuck::cast_slice(&vertices));
        queue.write_buffer(&pane.indices, 0, bytemuck::cast_slice(&indices));
        let offset = u64::from(self.draw_stride) * (pane_index * MAX_DRAWS_PER_PANE) as u64;
        queue.write_buffer(&self.draw_uniform, offset, &uniforms);
    }

    fn write_pane_uniform(&self, queue: &wgpu::Queue, pane_index: usize, uniform: TilePaneUniform) {
        if let Some(buffer) = self.pane_uniforms.get(pane_index) {
            queue.write_buffer(buffer, 0, bytemuck::bytes_of(&uniform));
        }
    }
}

impl PaneBuffers {
    fn new(device: &wgpu::Device) -> Self {
        Self {
            vertices: empty_buffer(device, wgpu::BufferUsages::VERTEX),
            indices: empty_buffer(device, wgpu::BufferUsages::INDEX),
            vertex_capacity: 0,
            index_capacity: 0,
            draws: Vec::new(),
        }
    }

    /// Grow the packed buffers if this frame needs more room. Capacity only
    /// ever grows, so a steady view stops reallocating after the first frame.
    fn ensure_capacity(&mut self, device: &wgpu::Device, vertices: usize, indices: usize) {
        if vertices > self.vertex_capacity {
            let capacity = vertices.next_power_of_two();
            self.vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("map_scene tile vertices"),
                size: (capacity * TileVertex::SIZE) as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vertex_capacity = capacity;
        }
        if indices > self.index_capacity {
            let capacity = indices.next_power_of_two();
            self.indices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("map_scene tile indices"),
                size: (capacity * std::mem::size_of::<u32>()) as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.index_capacity = capacity;
        }
    }
}

fn empty_buffer(device: &wgpu::Device, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("map_scene tile buffer"),
        size: 4,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Stride between per-draw uniforms, honouring the device's dynamic-offset
/// alignment.
fn draw_uniform_stride(device: &wgpu::Device) -> u32 {
    stride_for_alignment(device.limits().min_uniform_buffer_offset_alignment)
}

/// The alignment arithmetic on its own, so it can be tested without a device.
fn stride_for_alignment(alignment: u32) -> u32 {
    let size = std::mem::size_of::<TileDrawUniform>() as u32;
    let alignment = alignment.max(1);
    size.div_ceil(alignment) * alignment
}

/// One pane's tile draw for one frame.
pub struct TilePaintCallback {
    pub pane_index: usize,
    pub frame: Arc<TileFrame>,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
    /// Pane rectangle in physical pixels.
    pub rect_px: [f32; 4],
}

impl TilePaintCallback {
    /// The same world-to-clip transform the vector map builds, so the two
    /// layers cannot drift apart by a pixel.
    fn uniform(&self) -> TilePaneUniform {
        let camera = self.camera.sanitized();
        let viewport = self.viewport.sanitized();
        let width_px = (self.rect_px[2] - self.rect_px[0]).max(1.0);
        let height_px = (self.rect_px[3] - self.rect_px[1]).max(1.0);

        let scale = 1.0 / camera.km_per_point.max(f32::MIN_POSITIVE);
        let (sin, cos) = camera.rotation_rad.sin_cos();
        let half_width_points = width_px / viewport.pixels_per_point * 0.5;
        let half_height_points = height_px / viewport.pixels_per_point * 0.5;

        let sx = scale / half_width_points;
        let sy = scale / half_height_points;
        let cx = camera.center_east_km as f32;
        let cy = camera.center_north_km as f32;

        // Same packing, same row/column discipline, and the same reason: see
        // the comment on [`crate::gpu::MapPaintCallback::uniform`]. The two
        // must agree scalar for scalar, which
        // `the_tile_camera_matches_the_vector_map_camera_exactly` pins.
        let m00 = cos * sx;
        let m01 = sin * sx;
        let m10 = -sin * sy;
        let m11 = cos * sy;
        let tx = -(m00 * cx + m01 * cy);
        let ty = -(m10 * cx + m11 * cy);

        TilePaneUniform {
            world_to_clip: [
                [m00, m10, 0.0, 0.0],
                [m01, m11, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [tx, ty, 0.0, 1.0],
            ],
            scrim: self.frame.scrim,
        }
    }
}

impl CallbackTrait for TilePaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(resources) = callback_resources.get_mut::<TileRenderResources>() else {
            return Vec::new();
        };
        for decoded in self.frame.uploads.iter() {
            resources.ensure_resident(device, queue, decoded, &self.frame);
        }
        resources.prepare_pane(device, queue, self.pane_index, &self.frame);
        resources.write_pane_uniform(queue, self.pane_index, self.uniform());
        Vec::new()
    }

    fn paint(
        &self,
        _info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(resources) = callback_resources.get::<TileRenderResources>() else {
            return;
        };
        let Some(pane) = resources.panes.get(self.pane_index) else {
            return;
        };
        let Some(pane_group) = resources.pane_groups.get(self.pane_index) else {
            return;
        };
        if pane.draws.is_empty() {
            return;
        }

        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, pane_group, &[]);
        render_pass.set_vertex_buffer(0, pane.vertices.slice(..));
        render_pass.set_index_buffer(pane.indices.slice(..), wgpu::IndexFormat::Uint32);
        let base = (self.pane_index * MAX_DRAWS_PER_PANE) as u32;
        for draw in &pane.draws {
            let Some(resident) = resources.textures.get(&draw.texture) else {
                continue;
            };
            let offset = resources.draw_stride * (base + draw.slot);
            render_pass.set_bind_group(1, &resources.draw_group, &[offset]);
            render_pass.set_bind_group(2, &resident.bind_group, &[]);
            render_pass.draw_indexed(
                draw.index_start..draw.index_start + draw.index_count,
                0,
                0..1,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::Generation;
    use basemap_tiles::TileProvider;

    use crate::tiles::{TileFeedback, TileFrameKey};

    fn frame(scrim: [f32; 4]) -> Arc<TileFrame> {
        Arc::new(TileFrame {
            key: TileFrameKey {
                provider: TileProvider::UsgsImagery,
                projection: Generation::new(1),
                zoom: 9,
            },
            draws: Vec::new().into(),
            uploads: Vec::new().into(),
            attribution: TileProvider::UsgsImagery.attribution(),
            scrim,
            coverage: 0.0,
            feedback: Arc::new(TileFeedback::default()),
        })
    }

    fn callback(camera: Camera2D) -> TilePaintCallback {
        TilePaintCallback {
            pane_index: 0,
            frame: frame([0.0, 0.0, 0.0, 0.35]),
            camera,
            viewport: ViewportMetrics {
                width_points: 800.0,
                height_points: 600.0,
                pixels_per_point: 1.0,
            },
            rect_px: [0.0, 0.0, 800.0, 600.0],
        }
    }

    fn project(uniform: &TilePaneUniform, world: [f32; 2]) -> [f32; 2] {
        let m = uniform.world_to_clip;
        [
            m[0][0] * world[0] + m[1][0] * world[1] + m[3][0],
            m[0][1] * world[0] + m[1][1] * world[1] + m[3][1],
        ]
    }

    /// The claim that makes imagery and boundaries one picture: both layers
    /// build the same matrix from the same camera.
    #[test]
    fn the_tile_camera_matches_the_vector_map_camera_exactly() {
        let camera = Camera2D {
            center_east_km: 31.0,
            center_north_km: -12.5,
            km_per_point: 0.42,
            rotation_rad: 0.3,
        };
        let tile = callback(camera).uniform().world_to_clip;
        let map = crate::gpu::MapPaintCallback {
            pane_index: 0,
            geometry: Arc::new(crate::geometry::MapGeometry::new(
                analyst_runtime::GeometryCacheKey {
                    dataset: Generation::new(1),
                    projection: Generation::new(1),
                    style: Generation::new(1),
                    lod: analyst_runtime::LodBucket(0),
                },
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                crate::geometry::GeometryStats::default(),
            )),
            camera,
            viewport: ViewportMetrics {
                width_points: 800.0,
                height_points: 600.0,
                pixels_per_point: 1.0,
            },
            rect_px: [0.0, 0.0, 800.0, 600.0],
        }
        .uniform()
        .world_to_clip;
        assert_eq!(tile, map);
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
    fn panning_changes_only_the_translation_column() {
        let base = callback(Camera2D::default()).uniform();
        let panned = callback(Camera2D {
            center_east_km: 137.0,
            center_north_km: -88.0,
            ..Camera2D::default()
        })
        .uniform();
        assert_eq!(base.world_to_clip[0], panned.world_to_clip[0]);
        assert_eq!(base.world_to_clip[1], panned.world_to_clip[1]);
        assert_ne!(base.world_to_clip[3], panned.world_to_clip[3]);
    }

    #[test]
    fn the_scrim_reaches_the_uniform_unchanged() {
        let mut callback = callback(Camera2D::default());
        callback.frame = frame([0.05, 0.06, 0.07, 0.35]);
        assert_eq!(callback.uniform().scrim, [0.05, 0.06, 0.07, 0.35]);
    }

    /// The vertex layout the pipeline declares has to match the mesh the tile
    /// crate emits, and neither side can move without this failing.
    #[test]
    fn the_vertex_layout_matches_the_mesh_the_tile_crate_emits() {
        assert_eq!(TileVertex::SIZE, 16);
        assert_eq!(std::mem::size_of::<TileVertex>(), 16);
        assert_eq!(std::mem::size_of::<TileDrawUniform>(), 32);
        assert_eq!(std::mem::size_of::<TilePaneUniform>(), 80);
    }

    /// Every pane's slice of the dynamic-offset buffer has to start on the
    /// device's alignment and never reach into the next pane's slice.
    #[test]
    fn the_draw_uniform_regions_are_aligned_and_disjoint() {
        for alignment in [1_u32, 4, 32, 64, 256, 512] {
            let stride = stride_for_alignment(alignment);
            assert!(stride >= std::mem::size_of::<TileDrawUniform>() as u32);
            assert_eq!(stride % alignment.max(1), 0);
            let mut previous_end = 0_u32;
            for pane in 0..MAX_PANES {
                let base = (pane * MAX_DRAWS_PER_PANE) as u32;
                let start = stride * base;
                assert!(start >= previous_end, "pane {pane} overlaps its neighbour");
                assert_eq!(start % alignment.max(1), 0);
                previous_end = stride * (base + MAX_DRAWS_PER_PANE as u32);
            }
        }
    }
}
