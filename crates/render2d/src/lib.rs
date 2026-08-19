//! 2D radar rendering contracts.
//!
//! The long-term renderer will be GPU-backed, but this crate already provides a
//! CPU raster path for smoke tests, screenshots, and early visual validation.

pub mod beam;
pub mod derived;
pub mod interpolate;
pub mod quality;
pub mod smooth;
pub mod sweep_blend;
pub mod volumetric;
pub mod volumetric_support;
pub mod xsection;

use std::f32::consts::PI;
use std::ops::Range;
use std::path::Path;

pub use color_tables::{ColorTable, ColorTableFamily, ColorTableSet};
use image::{ImageBuffer, ImageError, Rgba};
pub use interpolate::{InterpolatedGrid, UpsampleFactors, upsample_moment_grid};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, ProductId, RadarVolume,
};
use rayon::prelude::*;
pub use smooth::smooth_moment_grid;
use thiserror::Error;

const AZIMUTH_BINS: usize = 3600;
const AZIMUTH_BIN_WIDTH_DEG: f32 = 0.1;
const MAX_AZIMUTH_HALF_WIDTH_DEG: f32 = 3.0;
const MAX_AZIMUTH_CANDIDATES: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderLayer {
    pub product: ProductId,
    pub moment: Option<MomentType>,
    pub visible: bool,
}

impl RenderLayer {
    pub fn base(moment: MomentType) -> Self {
        Self {
            product: ProductId::from(moment.clone()),
            moment: Some(moment),
            visible: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterOptions {
    pub width: u32,
    pub height: u32,
    pub range_fraction: u8,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 1024,
            range_fraction: 94,
        }
    }
}

/// How hard the renderer works to make one frame look like radar rather than
/// like a mosaic of gates.
///
/// Three independent passes, because they fix three different artefacts and
/// cost three different amounts:
///
/// * `soften` averages neighbouring cells on the polar lattice, which removes
///   the single-gate salt-and-pepper of a noisy field.
/// * `interpolate` inserts sub-beams and sub-gates so a gate stops being a
///   visible block when zoomed in. Both of these run once per volume, cut and
///   product and are cached here, so panning stays free.
/// * `supersample` renders at an integer multiple and box-filters down, and is
///   the only one of the three that fixes ALIASING - the speckle a zoomed-out
///   view gets from taking one sample per pixel where several gates fall. It is
///   also the only one that costs per frame, and it costs roughly the square of
///   the factor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayQuality {
    pub soften: bool,
    pub interpolate: bool,
    pub supersample: u32,
}

impl DisplayQuality {
    /// Exactly what the renderer did before any of this existed: one sample
    /// per screen pixel, off the native polar lattice.
    pub const NATIVE: Self = Self {
        soften: false,
        interpolate: false,
        supersample: 1,
    };

    /// Sub-beams and sub-gates, and two samples per pixel per axis. This is the
    /// default because it is the setting that stops a NEXRAD super-res sweep
    /// looking like a mosaic, and its per-frame cost is about four times the
    /// native raster on a pane-sized viewport - a few milliseconds.
    pub const SMOOTH: Self = Self {
        soften: false,
        interpolate: true,
        supersample: 2,
    };

    /// Adds the soften pass and a third sample per axis.
    pub const HIGH: Self = Self {
        soften: true,
        interpolate: true,
        supersample: 3,
    };

    /// Everything, at roughly sixteen times the native per-frame cost. Worth it
    /// for a still or a screenshot; on a fast loop it will drop frames.
    pub const ULTRA: Self = Self {
        soften: true,
        interpolate: true,
        supersample: 4,
    };

    /// The presets a UI offers, coarse to fine, each with its label.
    pub const PRESETS: [(&'static str, Self); 4] = [
        ("Native", Self::NATIVE),
        ("Smooth", Self::SMOOTH),
        ("High", Self::HIGH),
        ("Ultra", Self::ULTRA),
    ];

    /// The label of the preset this equals, or `None` for a custom setting.
    pub fn preset_label(self) -> Option<&'static str> {
        Self::PRESETS
            .iter()
            .find(|(_, preset)| *preset == self)
            .map(|(label, _)| *label)
    }

    /// True when softening this moment is safe.
    ///
    /// The soften pass is unguarded, so it must not touch a field whose
    /// interpolation is guarded: averaging across a velocity fold invents a
    /// speed nobody measured, and averaging through the rho_hv minimum erases
    /// the melting-layer signature that minimum IS (Giangrande, Krause and
    /// Ryzhkov 2008, J. Appl. Meteor. Climatol., 47, 1354-1364). Measured on
    /// one real sweep, softening velocity displaced 1,926 gates by up to
    /// 40 m/s.
    pub fn may_soften(self, moment: &MomentType) -> bool {
        !matches!(
            moment,
            MomentType::Velocity | MomentType::CorrelationCoefficient
        )
    }
}

impl Default for DisplayQuality {
    fn default() -> Self {
        Self::SMOOTH
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportRasterOptions {
    pub width: u32,
    pub height: u32,
    pub radar_x_px: f32,
    pub radar_y_px: f32,
    pub km_per_px_x: f32,
    pub km_per_px_y: f32,
}

pub fn viewport_rgba_buffer_len(options: ViewportRasterOptions) -> usize {
    let (width, height) = viewport_dimensions(options);
    rgba_len(width, height)
}

pub fn viewport_sample_cache_storage_upper_bound(options: ViewportRasterOptions) -> usize {
    let (width, height) = viewport_dimensions(options);
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(std::mem::size_of::<CachedSample>())
        .saturating_add((height as usize).saturating_mul(std::mem::size_of::<CachedRowSpan>()))
}

pub fn viewport_sample_cache_storage_upper_bound_for_grid(
    grid: &MomentGrid,
    options: ViewportRasterOptions,
) -> usize {
    let (_, height) = viewport_dimensions(options);
    let geometry = viewport_geometry(grid, options);
    let sample_slots = (0..height)
        .filter_map(|y| geometry.x_range_for_row(y))
        .map(|range| range.len())
        .sum::<usize>();
    sample_slots
        .saturating_mul(std::mem::size_of::<CachedSample>())
        .saturating_add((height as usize).saturating_mul(std::mem::size_of::<CachedRowSpan>()))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StormMotion {
    pub direction_deg: f32,
    pub speed_mps: f32,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("cut index {index} is out of range for {cut_count} cuts")]
    CutOutOfRange { index: usize, cut_count: usize },
    #[error("moment {moment} is not available in cut {cut_index}")]
    MissingMoment {
        cut_index: usize,
        moment: MomentType,
    },
    #[error("moment {moment} in cut {cut_index} has no decoded rows")]
    EmptyMoment {
        cut_index: usize,
        moment: MomentType,
    },
    #[error("RGBA buffer has {actual} bytes, expected {expected} for {width}x{height}")]
    BufferSizeMismatch {
        actual: usize,
        expected: usize,
        width: u32,
        height: u32,
    },
    #[error("viewport render cache belongs to a different radar volume")]
    CacheVolumeMismatch,
    #[error("viewport render cache is for cut {actual}, expected cut {expected}")]
    CacheCutMismatch { expected: usize, actual: usize },
    #[error("viewport render cache is for {actual}, expected {expected}")]
    CacheMomentMismatch {
        expected: MomentType,
        actual: MomentType,
    },
    #[error("viewport render cache storage no longer matches the moment storage")]
    CacheStorageMismatch,
    #[error("viewport geometry cache does not match this moment grid")]
    GeometryCacheMismatch,
    #[error("image write failed: {0}")]
    Image(#[from] ImageError),
}

pub type Result<T> = std::result::Result<T, RenderError>;

/// Render a decoded polar moment to a simple radar PNG.
pub fn render_moment_png(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    out_path: &Path,
    options: RasterOptions,
) -> Result<()> {
    let image = render_moment_image(volume, cut_index, moment, options)?;
    image.save(out_path)?;
    Ok(())
}

pub fn render_moment_image(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: RasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or(RenderError::CutOutOfRange {
            index: cut_index,
            cut_count: volume.cuts.len(),
        })?;
    let grid = cut
        .moments
        .get(&moment)
        .ok_or_else(|| RenderError::MissingMoment {
            cut_index,
            moment: moment.clone(),
        })?;

    if grid.radial_indices.is_empty() {
        return Err(RenderError::EmptyMoment { cut_index, moment });
    }

    let row_lookup = AzimuthLookup::new(cut, grid);
    let width = options.width.max(64);
    let height = options.height.max(64);
    let center_x = (width as f32 - 1.0) / 2.0;
    let center_y = (height as f32 - 1.0) / 2.0;
    let radius_px = center_x.min(center_y) * (f32::from(options.range_fraction) / 100.0);
    let max_range_m = max_range_m(grid).max(1.0);

    let mut pixels = vec![0; width as usize * height as usize * 4];
    let color_tables = ColorTableSet::default();
    let color_table = color_tables.for_family(color_family_for_moment(&grid.moment));

    match &grid.storage {
        MomentStorage::U8(values) => {
            let palette = build_u8_palette(grid, color_table);
            render_compact_storage(
                &mut pixels,
                values,
                &palette,
                grid,
                &row_lookup,
                RasterGeometry {
                    width,
                    center_x,
                    center_y,
                    radius_px,
                    radius_sq_px: radius_px * radius_px,
                    max_range_m,
                },
                false,
            );
        }
        MomentStorage::U16(values) => {
            let palette = build_u16_palette(grid, color_table);
            render_compact_storage(
                &mut pixels,
                values,
                &palette,
                grid,
                &row_lookup,
                RasterGeometry {
                    width,
                    center_x,
                    center_y,
                    radius_px,
                    radius_sq_px: radius_px * radius_px,
                    max_range_m,
                },
                false,
            );
        }
        MomentStorage::F32(values) => render_f32_storage(
            &mut pixels,
            values,
            grid,
            &row_lookup,
            color_table,
            RasterGeometry {
                width,
                center_x,
                center_y,
                radius_px,
                radius_sq_px: radius_px * radius_px,
                max_range_m,
            },
            false,
        ),
    }

    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_moment_viewport_image(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let (width, height, pixels) = render_moment_viewport_rgba(volume, cut_index, moment, options)?;
    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_moment_viewport_rgba(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
) -> Result<(u32, u32, Vec<u8>)> {
    let (width, height) = viewport_dimensions(options);
    let mut pixels = vec![0; rgba_len(width, height)];
    render_moment_viewport_rgba_into(volume, cut_index, moment, options, &mut pixels)?;
    Ok((width, height, pixels))
}

pub fn render_moment_viewport_rgba_into(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
) -> Result<(u32, u32)> {
    let cache = ViewportMomentCache::new(volume, cut_index, moment)?;
    cache.render_moment_rgba_into(volume, options, pixels)
}

pub struct ViewportMomentCache {
    volume_ptr: usize,
    cut_index: usize,
    moment: MomentType,
    row_lookup: AzimuthLookup,
    color_lookup: CachedColorLookup,
    storm_motion_basis: Option<StormMotionBasis>,
    /// A grid the cache owns and draws INSTEAD of the one in the cut.
    ///
    /// Two things put a grid here: velocity dealiasing, which replaces folded
    /// values with unfolded ones, and the display-quality passes, which soften
    /// and/or upsample the polar lattice. Both produce a grid that is not in
    /// the volume, and both must be built once per data change rather than per
    /// frame, which is what this cache is for.
    display_grid: Option<MomentGrid>,
}

pub struct ViewportSampleCache {
    volume_ptr: usize,
    cut_index: usize,
    moment: MomentType,
    width: u32,
    height: u32,
    sample_count: usize,
    row_spans: Vec<CachedRowSpan>,
    samples: Vec<CachedSample>,
}

pub struct ViewportGeometryCache {
    width: u32,
    height: u32,
    gate_range: GateRange,
    sample_count: usize,
    row_spans: Vec<CachedRowSpan>,
    samples: Vec<CachedSample>,
}

pub struct StormRelativePaletteCache {
    volume_ptr: usize,
    cut_index: usize,
    row_palettes: Vec<[[u8; 4]; 256]>,
}

impl ViewportSampleCache {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    pub fn storage_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<CachedSample>()
            + self.row_spans.len() * std::mem::size_of::<CachedRowSpan>()
    }

    fn geometry(&self) -> CachedViewportGeometry<'_> {
        CachedViewportGeometry {
            row_spans: &self.row_spans,
            samples: &self.samples,
        }
    }
}

impl ViewportGeometryCache {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    pub fn storage_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<CachedSample>()
            + self.row_spans.len() * std::mem::size_of::<CachedRowSpan>()
    }

    fn geometry(&self) -> CachedViewportGeometry<'_> {
        CachedViewportGeometry {
            row_spans: &self.row_spans,
            samples: &self.samples,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CachedRowSpan {
    start: u32,
    end: u32,
    sample_offset: usize,
}

impl CachedRowSpan {
    fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            sample_offset: 0,
        }
    }

    fn range(self) -> Option<Range<u32>> {
        (self.start < self.end).then_some(self.start..self.end)
    }
}

struct CachedRowBuild {
    start: u32,
    samples: Vec<CachedSample>,
    sample_count: usize,
}

impl CachedRowBuild {
    fn empty() -> Self {
        Self {
            start: 0,
            samples: Vec::new(),
            sample_count: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CachedSample(u32);

impl CachedSample {
    const GATE_BITS: u32 = 16;
    const GATE_MASK: u32 = (1 << Self::GATE_BITS) - 1;
    const SKIP_FLAG: u32 = 1 << 31;
    const SKIP_MASK: u32 = Self::SKIP_FLAG - 1;
    const ROW_LIMIT: usize = 1 << (u32::BITS - Self::GATE_BITS - 1);

    fn new(sample: ResolvedSample) -> Option<Self> {
        if sample.row >= Self::ROW_LIMIT || sample.gate > Self::GATE_MASK as usize {
            return None;
        }
        Some(Self(
            ((sample.row as u32) << Self::GATE_BITS) | sample.gate as u32,
        ))
    }

    fn skip(pixel_count: u32) -> Option<Self> {
        (pixel_count > 0 && pixel_count <= Self::SKIP_MASK)
            .then_some(Self(Self::SKIP_FLAG | pixel_count))
    }

    #[cfg(test)]
    fn sample(self) -> Option<ResolvedSample> {
        (!self.is_skip()).then_some(ResolvedSample {
            row: (self.0 >> Self::GATE_BITS) as usize,
            gate: (self.0 & Self::GATE_MASK) as usize,
        })
    }

    #[inline]
    fn is_skip(self) -> bool {
        self.0 & Self::SKIP_FLAG != 0
    }

    #[inline]
    fn skip_len(self) -> Option<u32> {
        self.is_skip().then_some(self.0 & Self::SKIP_MASK)
    }

    #[inline]
    fn row(self) -> usize {
        (self.0 >> Self::GATE_BITS) as usize
    }

    #[inline]
    fn gate(self) -> usize {
        (self.0 & Self::GATE_MASK) as usize
    }
}

struct StormMotionBasis {
    beam_cos: Vec<f32>,
    beam_sin: Vec<f32>,
}

impl StormMotionBasis {
    fn new(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        let mut beam_cos = Vec::with_capacity(grid.radial_indices.len());
        let mut beam_sin = Vec::with_capacity(grid.radial_indices.len());
        for radial_index in &grid.radial_indices {
            let azimuth_rad = cut
                .radials
                .get(*radial_index)
                .map(|radial| radial.azimuth_deg.to_radians())
                .unwrap_or(0.0);
            beam_cos.push(azimuth_rad.cos());
            beam_sin.push(azimuth_rad.sin());
        }
        Self { beam_cos, beam_sin }
    }

    fn row_motion_components(&self, storm_motion: StormMotion) -> Vec<f32> {
        let direction_rad = storm_motion.direction_deg.to_radians();
        let storm_cos = storm_motion.speed_mps * direction_rad.cos();
        let storm_sin = storm_motion.speed_mps * direction_rad.sin();
        self.beam_cos
            .iter()
            .zip(&self.beam_sin)
            .map(|(beam_cos, beam_sin)| storm_cos * *beam_cos + storm_sin * *beam_sin)
            .collect()
    }
}

enum CachedColorLookup {
    U8 {
        palette: Box<[[u8; 4]; 256]>,
        color_table: ColorTable,
    },
    U16 {
        palette: Vec<[u8; 4]>,
        color_table: ColorTable,
    },
    F32 {
        color_table: ColorTable,
    },
}

impl CachedColorLookup {
    fn new(grid: &MomentGrid, color_tables: &ColorTableSet) -> Self {
        let color_table = color_tables
            .for_family(color_family_for_moment(&grid.moment))
            .clone();
        match &grid.storage {
            MomentStorage::U8(_) => Self::U8 {
                palette: Box::new(build_u8_palette(grid, &color_table)),
                color_table,
            },
            MomentStorage::U16(_) => Self::U16 {
                palette: build_u16_palette(grid, &color_table),
                color_table,
            },
            MomentStorage::F32(_) => Self::F32 { color_table },
        }
    }

    fn color_table(&self) -> &ColorTable {
        match self {
            Self::U8 { color_table, .. }
            | Self::U16 { color_table, .. }
            | Self::F32 { color_table } => color_table,
        }
    }
}

impl ViewportMomentCache {
    pub fn new(volume: &RadarVolume, cut_index: usize, moment: MomentType) -> Result<Self> {
        Self::new_with_color_tables(volume, cut_index, moment, &ColorTableSet::default())
    }

    pub fn new_with_color_tables(
        volume: &RadarVolume,
        cut_index: usize,
        moment: MomentType,
        color_tables: &ColorTableSet,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let grid = cut
            .moments
            .get(&moment)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index,
                moment: moment.clone(),
            })?;

        if grid.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment { cut_index, moment });
        }

        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            storm_motion_basis: (moment == MomentType::Velocity)
                .then(|| StormMotionBasis::new(cut, grid)),
            moment,
            row_lookup: AzimuthLookup::new(cut, grid),
            color_lookup: CachedColorLookup::new(grid, color_tables),
            display_grid: None,
        })
    }

    pub fn new_dealiased_velocity(volume: &RadarVolume, cut_index: usize) -> Result<Self> {
        Self::new_dealiased_velocity_with_color_tables(volume, cut_index, &ColorTableSet::default())
    }

    pub fn new_dealiased_velocity_with_color_tables(
        volume: &RadarVolume,
        cut_index: usize,
        color_tables: &ColorTableSet,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let source_grid =
            cut.moments
                .get(&MomentType::Velocity)
                .ok_or_else(|| RenderError::MissingMoment {
                    cut_index,
                    moment: MomentType::Velocity,
                })?;

        if source_grid.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment {
                cut_index,
                moment: MomentType::Velocity,
            });
        }

        let dealiased_grid = dealias_velocity_grid(cut, source_grid);
        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            moment: MomentType::Velocity,
            row_lookup: AzimuthLookup::new(cut, &dealiased_grid),
            color_lookup: CachedColorLookup::new(&dealiased_grid, color_tables),
            storm_motion_basis: Some(StormMotionBasis::new(cut, &dealiased_grid)),
            display_grid: Some(dealiased_grid),
        })
    }

    /// Build a cache whose grid has been through the display-quality passes.
    ///
    /// The workstation's rasteriser samples one gate per screen pixel, so a
    /// coarse polar lattice reads as speckle when zoomed out and as blocks when
    /// zoomed in. Softening and polar upsampling fix that at the DATA end -
    /// once per volume/cut/product on the render worker, cached here - rather
    /// than by blurring the finished picture, which would also blur the map and
    /// the range rings.
    ///
    /// Softening is refused for the moments whose interpolation is guarded.
    /// The soften pass has no `InterpPolicy`: it averages straight through a
    /// velocity fold and through the rho_hv minimum at the melting layer. On
    /// one real sweep that displaced 1,926 gates by up to 40 m/s, which is not
    /// a cosmetic difference to anyone reading a couplet.
    pub fn new_display_quality(
        volume: &RadarVolume,
        cut_index: usize,
        moment: MomentType,
        color_tables: &ColorTableSet,
        quality: DisplayQuality,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let source = cut
            .moments
            .get(&moment)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index,
                moment: moment.clone(),
            })?;
        if source.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment { cut_index, moment });
        }

        let (display_grid, row_lookup) = apply_display_quality(cut, &moment, source, quality);
        let grid = display_grid.as_ref().unwrap_or(source);
        let color_lookup = CachedColorLookup::new(grid, color_tables);
        let storm_motion_basis =
            (moment == MomentType::Velocity).then(|| StormMotionBasis::new(cut, grid));

        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            moment,
            row_lookup,
            color_lookup,
            storm_motion_basis,
            display_grid,
        })
    }

    /// The dealiased-velocity cache, with the display-quality passes applied to
    /// the UNFOLDED grid.
    ///
    /// Order matters and this is the only correct one. Interpolating folded
    /// velocity is stopped dead by the 30 m/s guard, so a fold would leave a
    /// band of native-resolution blocks straight through the couplet an analyst
    /// is looking at. Unfolding first removes the discontinuity, and the guard
    /// then has nothing to refuse.
    pub fn new_dealiased_velocity_display_quality(
        volume: &RadarVolume,
        cut_index: usize,
        color_tables: &ColorTableSet,
        quality: DisplayQuality,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let source_grid =
            cut.moments
                .get(&MomentType::Velocity)
                .ok_or_else(|| RenderError::MissingMoment {
                    cut_index,
                    moment: MomentType::Velocity,
                })?;
        if source_grid.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment {
                cut_index,
                moment: MomentType::Velocity,
            });
        }

        let dealiased = dealias_velocity_grid(cut, source_grid);
        // Softening is allowed here where it is refused for raw velocity: the
        // reason for that refusal is the fold, and there is no longer one.
        let quality = DisplayQuality {
            soften: quality.soften,
            ..quality
        };
        let (upgraded, row_lookup) = apply_display_quality_unguarded(cut, &dealiased, quality);
        let grid = upgraded.as_ref().unwrap_or(&dealiased);
        let color_lookup = CachedColorLookup::new(grid, color_tables);
        let storm_motion_basis = Some(StormMotionBasis::new(cut, grid));

        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            moment: MomentType::Velocity,
            row_lookup,
            color_lookup,
            storm_motion_basis,
            display_grid: Some(upgraded.unwrap_or(dealiased)),
        })
    }

    pub fn cut_index(&self) -> usize {
        self.cut_index
    }

    pub fn moment(&self) -> &MomentType {
        &self.moment
    }

    pub fn render_moment_rgba_into(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        let (_, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        ensure_rgba_buffer(pixels, width, height)?;
        render_moment_viewport_grid_into(
            grid,
            &self.row_lookup,
            &self.color_lookup,
            options,
            pixels,
            true,
        )?;
        Ok((width, height))
    }

    pub fn build_sample_cache(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<ViewportSampleCache> {
        let (_, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        let geometry = viewport_geometry(grid, options);
        let lookup_table = ViewportLookupTable::new(grid, geometry);

        let row_builds = match &grid.storage {
            MomentStorage::U8(values) => {
                build_sample_cache_rows(height, &lookup_table, &self.row_lookup, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::U16(values) => {
                build_sample_cache_rows(height, &lookup_table, &self.row_lookup, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::F32(values) => {
                build_sample_cache_rows(height, &lookup_table, &self.row_lookup, |sample| {
                    resolve_f32_sample(values, grid, &self.row_lookup, sample)
                })
            }
        };

        Ok(viewport_sample_cache_from_rows(
            self.volume_ptr,
            self.cut_index,
            self.moment.clone(),
            width,
            height,
            row_builds,
        ))
    }

    pub fn build_geometry_cache(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<ViewportGeometryCache> {
        let (_, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        let geometry = viewport_geometry(grid, options);
        let lookup_table = ViewportLookupTable::new(grid, geometry);
        let row_builds = build_geometry_cache_rows(height, &lookup_table, &self.row_lookup);
        let (sample_count, row_spans, samples) = flatten_cached_rows(height, row_builds);

        Ok(ViewportGeometryCache {
            width,
            height,
            gate_range: grid.gate_range.clone(),
            sample_count,
            row_spans,
            samples,
        })
    }

    pub fn build_sample_cache_from_geometry_cache(
        &self,
        volume: &RadarVolume,
        geometry_cache: &ViewportGeometryCache,
    ) -> Result<ViewportSampleCache> {
        let (_, grid) = self.cut_and_grid(volume)?;
        if grid.gate_range != geometry_cache.gate_range {
            return Err(RenderError::GeometryCacheMismatch);
        }
        let geometry = geometry_cache.geometry();
        let row_builds = match &grid.storage {
            MomentStorage::U8(values) => {
                build_sample_cache_rows_from_geometry(geometry_cache.height, geometry, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::U16(values) => {
                build_sample_cache_rows_from_geometry(geometry_cache.height, geometry, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::F32(values) => {
                build_sample_cache_rows_from_geometry(geometry_cache.height, geometry, |sample| {
                    resolve_f32_sample(values, grid, &self.row_lookup, sample)
                })
            }
        };

        Ok(viewport_sample_cache_from_rows(
            self.volume_ptr,
            self.cut_index,
            self.moment.clone(),
            geometry_cache.width,
            geometry_cache.height,
            row_builds,
        ))
    }

    pub fn sample_cache_storage_upper_bound(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<usize> {
        let (_, grid) = self.cut_and_grid(volume)?;
        Ok(viewport_sample_cache_storage_upper_bound_for_grid(
            grid, options,
        ))
    }

    pub fn render_moment_rgba_with_sample_cache(
        &self,
        volume: &RadarVolume,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_moment_rgba_with_sample_cache_impl(volume, sample_cache, pixels, true)
    }

    /// Renders over an existing RGBA buffer without clearing transparent pixels first.
    ///
    /// Callers must only use this when `pixels` was last rendered with the same
    /// volume, cut, moment, and viewport sample footprint. The app worker tracks
    /// that provenance before taking this path.
    pub fn render_moment_rgba_with_sample_cache_reusing_transparency(
        &self,
        volume: &RadarVolume,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_moment_rgba_with_sample_cache_impl(volume, sample_cache, pixels, false)
    }

    fn render_moment_rgba_with_sample_cache_impl(
        &self,
        volume: &RadarVolume,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
        clear_pixels: bool,
    ) -> Result<(u32, u32)> {
        let (_, grid) = self.cut_and_grid(volume)?;
        self.ensure_sample_cache(sample_cache)?;
        ensure_rgba_buffer(pixels, sample_cache.width, sample_cache.height)?;
        render_moment_sample_cache_grid_into(
            grid,
            &self.color_lookup,
            sample_cache,
            pixels,
            clear_pixels,
        )?;
        Ok(sample_cache.dimensions())
    }

    pub fn render_storm_relative_velocity_rgba_into(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_storm_relative_velocity_rgba_into_cached(
            volume,
            storm_motion,
            None,
            options,
            pixels,
        )
    }

    pub fn build_storm_relative_velocity_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
    ) -> Result<Option<StormRelativePaletteCache>> {
        if self.moment != MomentType::Velocity {
            return Err(RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: self.moment.clone(),
            });
        }

        let (cut, grid) = self.cut_and_grid(volume)?;
        let MomentStorage::U8(_) = &grid.storage else {
            return Ok(None);
        };
        let row_motion = self
            .storm_motion_basis
            .as_ref()
            .map(|basis| basis.row_motion_components(storm_motion))
            .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
        Ok(Some(StormRelativePaletteCache {
            volume_ptr: self.volume_ptr,
            cut_index: self.cut_index,
            row_palettes: build_storm_relative_u8_row_palettes(
                grid,
                &row_motion,
                self.color_lookup.color_table(),
            ),
        }))
    }

    pub fn render_storm_relative_velocity_rgba_into_with_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: &StormRelativePaletteCache,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.ensure_storm_relative_palette_cache(palette_cache)?;
        self.render_storm_relative_velocity_rgba_into_cached(
            volume,
            storm_motion,
            Some(palette_cache),
            options,
            pixels,
        )
    }

    fn render_storm_relative_velocity_rgba_into_cached(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: Option<&StormRelativePaletteCache>,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        if self.moment != MomentType::Velocity {
            return Err(RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: self.moment.clone(),
            });
        }

        let (cut, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        ensure_rgba_buffer(pixels, width, height)?;
        render_storm_relative_velocity_viewport_grid_into(
            cut,
            grid,
            StormRelativeRenderCache {
                row_lookup: &self.row_lookup,
                storm_motion_basis: self.storm_motion_basis.as_ref(),
                color_table: self.color_lookup.color_table(),
                palette_cache,
            },
            storm_motion,
            options,
            pixels,
            true,
        );
        Ok((width, height))
    }

    pub fn render_storm_relative_velocity_rgba_with_sample_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            None,
            sample_cache,
            pixels,
            true,
        )
    }

    /// Renders SRV over an existing RGBA buffer without clearing transparent pixels first.
    ///
    /// This is safe only when the buffer came from the same velocity sample
    /// footprint. The storm motion may differ because every cached velocity
    /// sample is overwritten during this render.
    pub fn render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            None,
            sample_cache,
            pixels,
            false,
        )
    }

    pub fn render_storm_relative_velocity_rgba_with_sample_cache_and_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: &StormRelativePaletteCache,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.ensure_storm_relative_palette_cache(palette_cache)?;
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            Some(palette_cache),
            sample_cache,
            pixels,
            true,
        )
    }

    pub fn render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency_and_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: &StormRelativePaletteCache,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.ensure_storm_relative_palette_cache(palette_cache)?;
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            Some(palette_cache),
            sample_cache,
            pixels,
            false,
        )
    }

    fn render_storm_relative_velocity_rgba_with_sample_cache_impl(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: Option<&StormRelativePaletteCache>,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
        clear_pixels: bool,
    ) -> Result<(u32, u32)> {
        if self.moment != MomentType::Velocity {
            return Err(RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: self.moment.clone(),
            });
        }

        let (cut, grid) = self.cut_and_grid(volume)?;
        self.ensure_sample_cache(sample_cache)?;
        ensure_rgba_buffer(pixels, sample_cache.width, sample_cache.height)?;
        render_storm_relative_velocity_sample_cache_grid_into(
            cut,
            grid,
            StormRelativeRenderCache {
                row_lookup: &self.row_lookup,
                storm_motion_basis: self.storm_motion_basis.as_ref(),
                color_table: self.color_lookup.color_table(),
                palette_cache,
            },
            storm_motion,
            sample_cache,
            pixels,
            clear_pixels,
        );
        Ok(sample_cache.dimensions())
    }

    fn ensure_sample_cache(&self, sample_cache: &ViewportSampleCache) -> Result<()> {
        if self.volume_ptr != sample_cache.volume_ptr {
            return Err(RenderError::CacheVolumeMismatch);
        }
        if self.cut_index != sample_cache.cut_index {
            return Err(RenderError::CacheCutMismatch {
                expected: self.cut_index,
                actual: sample_cache.cut_index,
            });
        }
        if self.moment != sample_cache.moment {
            return Err(RenderError::CacheMomentMismatch {
                expected: self.moment.clone(),
                actual: sample_cache.moment.clone(),
            });
        }
        Ok(())
    }

    fn ensure_storm_relative_palette_cache(
        &self,
        palette_cache: &StormRelativePaletteCache,
    ) -> Result<()> {
        if self.volume_ptr != palette_cache.volume_ptr {
            return Err(RenderError::CacheVolumeMismatch);
        }
        if self.cut_index != palette_cache.cut_index {
            return Err(RenderError::CacheCutMismatch {
                expected: self.cut_index,
                actual: palette_cache.cut_index,
            });
        }
        Ok(())
    }

    fn cut_and_grid<'a>(
        &'a self,
        volume: &'a RadarVolume,
    ) -> Result<(&'a ElevationCut, &'a MomentGrid)> {
        if self.volume_ptr != volume as *const RadarVolume as usize {
            return Err(RenderError::CacheVolumeMismatch);
        }

        let cut = volume
            .cuts
            .get(self.cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: self.cut_index,
                cut_count: volume.cuts.len(),
            })?;
        if let Some(grid) = &self.display_grid {
            return Ok((cut, grid));
        }
        let grid = cut
            .moments
            .get(&self.moment)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index: self.cut_index,
                moment: self.moment.clone(),
            })?;
        Ok((cut, grid))
    }
}

/// Run the display-quality passes over one grid, returning the owned result (if
/// any pass ran) and the azimuth lookup that matches it.
///
/// `None` means every pass declined, and the caller must keep reading the grid
/// out of the cut - so the default path allocates nothing at all.
fn apply_display_quality(
    cut: &ElevationCut,
    moment: &MomentType,
    source: &MomentGrid,
    quality: DisplayQuality,
) -> (Option<MomentGrid>, AzimuthLookup) {
    let quality = DisplayQuality {
        soften: quality.soften && quality.may_soften(moment),
        ..quality
    };
    apply_display_quality_unguarded(cut, source, quality)
}

/// As `apply_display_quality`, but the caller has already decided softening is
/// safe for this field. Only the dealiased-velocity path may say that.
fn apply_display_quality_unguarded(
    cut: &ElevationCut,
    source: &MomentGrid,
    quality: DisplayQuality,
) -> (Option<MomentGrid>, AzimuthLookup) {
    let softened = quality
        .soften
        .then(|| crate::smooth::smooth_moment_grid(source));
    let base = softened.as_ref().unwrap_or(source);

    let upsampled = quality
        .interpolate
        .then(|| crate::interpolate::upsample_moment_grid(cut, base))
        .flatten();

    match upsampled {
        Some(interpolated) => {
            let lookup = AzimuthLookup::from_row_azimuths(
                &interpolated.row_azimuths_deg,
                &interpolated.grid,
            );
            (Some(interpolated.grid), lookup)
        }
        None => {
            let lookup = AzimuthLookup::new(cut, base);
            (softened, lookup)
        }
    }
}

fn render_moment_viewport_grid_into(
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    color_lookup: &CachedColorLookup,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
    clear_pixels: bool,
) -> Result<()> {
    let geometry = viewport_geometry(grid, options);
    let lookup_table = ViewportLookupTable::new(grid, geometry);

    match (&grid.storage, color_lookup) {
        (MomentStorage::U8(values), CachedColorLookup::U8 { palette, .. }) => {
            render_compact_viewport_storage(
                pixels,
                values,
                palette.as_ref(),
                grid,
                row_lookup,
                &lookup_table,
                clear_pixels,
            );
        }
        (MomentStorage::U16(values), CachedColorLookup::U16 { palette, .. }) => {
            render_compact_viewport_storage(
                pixels,
                values,
                palette,
                grid,
                row_lookup,
                &lookup_table,
                clear_pixels,
            );
        }
        (MomentStorage::F32(values), color_lookup) => {
            render_f32_viewport_storage(
                pixels,
                values,
                grid,
                row_lookup,
                color_lookup.color_table(),
                &lookup_table,
                clear_pixels,
            );
        }
        _ => return Err(RenderError::CacheStorageMismatch),
    }
    Ok(())
}

fn render_moment_sample_cache_grid_into(
    grid: &MomentGrid,
    color_lookup: &CachedColorLookup,
    sample_cache: &ViewportSampleCache,
    pixels: &mut [u8],
    clear_pixels: bool,
) -> Result<()> {
    match (&grid.storage, color_lookup) {
        (MomentStorage::U8(values), CachedColorLookup::U8 { palette, .. }) => {
            render_compact_sample_cache_storage(
                pixels,
                values,
                palette.as_ref(),
                grid,
                sample_cache,
                clear_pixels,
            );
        }
        (MomentStorage::U16(values), CachedColorLookup::U16 { palette, .. }) => {
            render_compact_sample_cache_storage(
                pixels,
                values,
                palette,
                grid,
                sample_cache,
                clear_pixels,
            );
        }
        (MomentStorage::F32(values), color_lookup) => {
            render_f32_sample_cache_storage(
                pixels,
                values,
                grid,
                color_lookup.color_table(),
                sample_cache,
                clear_pixels,
            );
        }
        _ => return Err(RenderError::CacheStorageMismatch),
    }
    Ok(())
}

pub fn render_storm_relative_velocity_image(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: RasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or(RenderError::CutOutOfRange {
            index: cut_index,
            cut_count: volume.cuts.len(),
        })?;
    let grid =
        cut.moments
            .get(&MomentType::Velocity)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index,
                moment: MomentType::Velocity,
            })?;

    if grid.radial_indices.is_empty() {
        return Err(RenderError::EmptyMoment {
            cut_index,
            moment: MomentType::Velocity,
        });
    }

    let row_lookup = AzimuthLookup::new(cut, grid);
    let row_motion = row_motion_components(cut, grid, storm_motion);
    let width = options.width.max(64);
    let height = options.height.max(64);
    let center_x = (width as f32 - 1.0) / 2.0;
    let center_y = (height as f32 - 1.0) / 2.0;
    let radius_px = center_x.min(center_y) * (f32::from(options.range_fraction) / 100.0);
    let max_range_m = max_range_m(grid).max(1.0);

    let mut pixels = vec![0; width as usize * height as usize * 4];
    let color_tables = ColorTableSet::default();
    let color_table = color_tables.for_family(ColorTableFamily::Velocity);
    let geometry = RasterGeometry {
        width,
        center_x,
        center_y,
        radius_px,
        radius_sq_px: radius_px * radius_px,
        max_range_m,
    };

    match &grid.storage {
        MomentStorage::U8(values) => {
            let row_palettes = build_storm_relative_u8_row_palettes(grid, &row_motion, color_table);
            render_storm_relative_u8_storage(
                &mut pixels,
                values,
                grid,
                &row_lookup,
                &row_palettes,
                geometry,
                false,
            );
        }
        MomentStorage::U16(values) => {
            render_storm_relative_storage(
                &mut pixels,
                values,
                grid,
                &row_lookup,
                StormRelativeValueLookup {
                    row_motion: &row_motion,
                    color_table,
                },
                geometry,
                false,
            );
        }
        MomentStorage::F32(values) => render_storm_relative_f32_storage(
            &mut pixels,
            values,
            grid,
            &row_lookup,
            StormRelativeValueLookup {
                row_motion: &row_motion,
                color_table,
            },
            geometry,
            false,
        ),
    }

    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_storm_relative_velocity_viewport_image(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let (width, height, pixels) =
        render_storm_relative_velocity_viewport_rgba(volume, cut_index, storm_motion, options)?;
    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_storm_relative_velocity_viewport_rgba(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
) -> Result<(u32, u32, Vec<u8>)> {
    let (width, height) = viewport_dimensions(options);
    let mut pixels = vec![0; rgba_len(width, height)];
    render_storm_relative_velocity_viewport_rgba_into(
        volume,
        cut_index,
        storm_motion,
        options,
        &mut pixels,
    )?;
    Ok((width, height, pixels))
}

pub fn render_storm_relative_velocity_viewport_rgba_into(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
) -> Result<(u32, u32)> {
    let cache = ViewportMomentCache::new(volume, cut_index, MomentType::Velocity)?;
    cache.render_storm_relative_velocity_rgba_into(volume, storm_motion, options, pixels)
}

fn render_storm_relative_velocity_viewport_grid_into(
    cut: &ElevationCut,
    grid: &MomentGrid,
    render_cache: StormRelativeRenderCache<'_>,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
    clear_pixels: bool,
) {
    let geometry = viewport_geometry(grid, options);
    let lookup_table = ViewportLookupTable::new(grid, geometry);

    match &grid.storage {
        MomentStorage::U8(values) => {
            let built_palettes;
            let row_palettes = if let Some(palette_cache) = render_cache.palette_cache {
                &palette_cache.row_palettes
            } else {
                let row_motion = render_cache
                    .storm_motion_basis
                    .map(|basis| basis.row_motion_components(storm_motion))
                    .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
                built_palettes = build_storm_relative_u8_row_palettes(
                    grid,
                    &row_motion,
                    render_cache.color_table,
                );
                &built_palettes
            };
            render_storm_relative_u8_viewport_storage(
                pixels,
                values,
                grid,
                render_cache.row_lookup,
                row_palettes,
                &lookup_table,
                clear_pixels,
            );
        }
        MomentStorage::U16(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_viewport_storage(
                pixels,
                values,
                grid,
                render_cache.row_lookup,
                StormRelativeValueLookup {
                    row_motion: &row_motion,
                    color_table: render_cache.color_table,
                },
                &lookup_table,
                clear_pixels,
            );
        }
        MomentStorage::F32(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_f32_viewport_storage(
                pixels,
                values,
                grid,
                render_cache.row_lookup,
                StormRelativeValueLookup {
                    row_motion: &row_motion,
                    color_table: render_cache.color_table,
                },
                &lookup_table,
                clear_pixels,
            );
        }
    }
}

fn render_storm_relative_velocity_sample_cache_grid_into(
    cut: &ElevationCut,
    grid: &MomentGrid,
    render_cache: StormRelativeRenderCache<'_>,
    storm_motion: StormMotion,
    sample_cache: &ViewportSampleCache,
    pixels: &mut [u8],
    clear_pixels: bool,
) {
    match &grid.storage {
        MomentStorage::U8(values) => {
            let built_palettes;
            let row_palettes = if let Some(palette_cache) = render_cache.palette_cache {
                &palette_cache.row_palettes
            } else {
                let row_motion = render_cache
                    .storm_motion_basis
                    .map(|basis| basis.row_motion_components(storm_motion))
                    .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
                built_palettes = build_storm_relative_u8_row_palettes(
                    grid,
                    &row_motion,
                    render_cache.color_table,
                );
                &built_palettes
            };
            render_storm_relative_u8_sample_cache_storage(
                pixels,
                values,
                grid,
                row_palettes,
                sample_cache,
                clear_pixels,
            );
        }
        MomentStorage::U16(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_sample_cache_storage(
                pixels,
                values,
                grid,
                &row_motion,
                render_cache.color_table,
                sample_cache,
                clear_pixels,
            );
        }
        MomentStorage::F32(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_f32_sample_cache_storage(
                pixels,
                values,
                grid,
                &row_motion,
                render_cache.color_table,
                sample_cache,
                clear_pixels,
            );
        }
    }
}

struct StormRelativeRenderCache<'a> {
    row_lookup: &'a AzimuthLookup,
    storm_motion_basis: Option<&'a StormMotionBasis>,
    color_table: &'a ColorTable,
    palette_cache: Option<&'a StormRelativePaletteCache>,
}

#[derive(Clone, Copy)]
struct StormRelativeValueLookup<'a> {
    row_motion: &'a [f32],
    color_table: &'a ColorTable,
}

#[derive(Clone, Copy, Debug)]
struct RasterGeometry {
    width: u32,
    center_x: f32,
    center_y: f32,
    radius_px: f32,
    radius_sq_px: f32,
    max_range_m: f32,
}

#[derive(Clone, Copy, Debug)]
struct ViewportGeometry {
    width: u32,
    radar_x_px: f32,
    radar_y_px: f32,
    km_per_px_x: f32,
    km_per_px_y: f32,
    max_range_km_sq: f32,
}

fn viewport_dimensions(options: ViewportRasterOptions) -> (u32, u32) {
    (options.width.max(1), options.height.max(1))
}

fn viewport_geometry(grid: &MomentGrid, options: ViewportRasterOptions) -> ViewportGeometry {
    let (width, _) = viewport_dimensions(options);
    let max_range_km = max_range_m(grid).max(1.0) / 1000.0;
    ViewportGeometry {
        width,
        radar_x_px: options.radar_x_px,
        radar_y_px: options.radar_y_px,
        km_per_px_x: options.km_per_px_x.max(f32::EPSILON),
        km_per_px_y: options.km_per_px_y.max(f32::EPSILON),
        max_range_km_sq: max_range_km * max_range_km,
    }
}

fn rgba_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

fn ensure_rgba_buffer(pixels: &[u8], width: u32, height: u32) -> Result<()> {
    let expected = rgba_len(width, height);
    if pixels.len() == expected {
        Ok(())
    } else {
        Err(RenderError::BufferSizeMismatch {
            actual: pixels.len(),
            expected,
            width,
            height,
        })
    }
}

trait LookupGeometry: Copy + Sync {
    fn width(self) -> u32;
    fn x_range_for_row(self, y: u32) -> Option<Range<u32>>;
    fn lookup(
        self,
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
    ) -> Option<SampleLookup>;
}

impl LookupGeometry for RasterGeometry {
    fn width(self) -> u32 {
        self.width
    }

    fn x_range_for_row(self, _y: u32) -> Option<Range<u32>> {
        Some(0..self.width)
    }

    fn lookup(
        self,
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
    ) -> Option<SampleLookup> {
        raster_lookup(x, y, grid, row_lookup, self)
    }
}

impl LookupGeometry for ViewportGeometry {
    fn width(self) -> u32 {
        self.width
    }

    fn x_range_for_row(self, y: u32) -> Option<Range<u32>> {
        let dy_km = (self.radar_y_px - (y as f32 + 0.5)) * self.km_per_px_y;
        let dy_km_sq = dy_km * dy_km;
        if dy_km_sq > self.max_range_km_sq {
            return None;
        }

        let max_dx_km = (self.max_range_km_sq - dy_km_sq).max(0.0).sqrt();
        let max_dx_px = max_dx_km / self.km_per_px_x;
        let first = (self.radar_x_px - max_dx_px - 0.5).floor() as i64 - 1;
        let last_exclusive = (self.radar_x_px + max_dx_px - 0.5).ceil() as i64 + 2;
        let width = i64::from(self.width);
        let start = first.clamp(0, width) as u32;
        let end = last_exclusive.clamp(0, width) as u32;
        (start < end).then_some(start..end)
    }

    fn lookup(
        self,
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
    ) -> Option<SampleLookup> {
        viewport_lookup(x, y, grid, row_lookup, self)
    }
}

#[derive(Debug)]
struct ViewportLookupTable {
    geometry: ViewportGeometry,
    first_gate_m: f32,
    gate_spacing_m: f32,
    gate_count: usize,
}

impl ViewportLookupTable {
    fn new(grid: &MomentGrid, geometry: ViewportGeometry) -> Self {
        Self {
            geometry,
            first_gate_m: grid.gate_range.first_gate_m as f32,
            gate_spacing_m: grid.gate_range.gate_spacing_m.max(1) as f32,
            gate_count: grid.gate_range.gate_count,
        }
    }

    fn width(&self) -> u32 {
        self.geometry.width
    }

    fn row(&self, y: u32) -> Option<ViewportLookupRow> {
        let dy_km = (self.geometry.radar_y_px - (y as f32 + 0.5)) * self.geometry.km_per_px_y;
        let dy_km_sq = dy_km * dy_km;
        if dy_km_sq > self.geometry.max_range_km_sq {
            return None;
        }

        let max_dx_km = (self.geometry.max_range_km_sq - dy_km_sq).max(0.0).sqrt();
        let max_dx_px = max_dx_km / self.geometry.km_per_px_x;
        let first = (self.geometry.radar_x_px - max_dx_px - 0.5).floor() as i64 - 1;
        let last_exclusive = (self.geometry.radar_x_px + max_dx_px - 0.5).ceil() as i64 + 2;
        let width = i64::from(self.geometry.width);
        let start = first.clamp(0, width) as u32;
        let end = last_exclusive.clamp(0, width) as u32;
        (start < end).then_some(ViewportLookupRow {
            x_range: start..end,
            dy_km,
            dy_km_sq,
            max_range_km_sq: self.geometry.max_range_km_sq,
            radar_x_px: self.geometry.radar_x_px,
            km_per_px_x: self.geometry.km_per_px_x,
            first_gate_m: self.first_gate_m,
            gate_spacing_m: self.gate_spacing_m,
            gate_count: self.gate_count,
        })
    }
}

#[derive(Clone, Debug)]
struct ViewportLookupRow {
    x_range: Range<u32>,
    dy_km: f32,
    dy_km_sq: f32,
    max_range_km_sq: f32,
    radar_x_px: f32,
    km_per_px_x: f32,
    first_gate_m: f32,
    gate_spacing_m: f32,
    gate_count: usize,
}

impl ViewportLookupRow {
    fn lookup(&self, x: u32, row_lookup: &AzimuthLookup) -> Option<SampleLookup> {
        let dx_km = (x as f32 + 0.5 - self.radar_x_px) * self.km_per_px_x;
        let range_km_sq = dx_km.mul_add(dx_km, self.dy_km_sq);
        if range_km_sq > self.max_range_km_sq {
            return None;
        }

        let range_m = range_km_sq.sqrt() * 1000.0;
        let gate = ((range_m - self.first_gate_m) / self.gate_spacing_m).round() as isize;
        if gate < 0 || gate as usize >= self.gate_count {
            return None;
        }

        let azimuth_deg = azimuth_from_xy(dx_km, self.dy_km);
        let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
        Some(SampleLookup {
            azimuth_bin,
            gate: gate as usize,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct CachedViewportGeometry<'a> {
    row_spans: &'a [CachedRowSpan],
    samples: &'a [CachedSample],
}

impl<'a> CachedViewportGeometry<'a> {
    fn row_samples(&self, y: usize) -> Option<(u32, &'a [CachedSample])> {
        let span = self.row_spans.get(y)?;
        let range = span.range()?;
        let start = span.sample_offset;
        let end = start + (range.end - range.start) as usize;
        Some((range.start, &self.samples[start..end]))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SampleLookup {
    azimuth_bin: usize,
    gate: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedSample {
    row: usize,
    gate: usize,
}

trait RawMomentValue: Copy + Sync {
    fn to_usize(self) -> usize;
}

impl RawMomentValue for u8 {
    fn to_usize(self) -> usize {
        usize::from(self)
    }
}

impl RawMomentValue for u16 {
    fn to_usize(self) -> usize {
        usize::from(self)
    }
}

fn render_compact_storage<T: RawMomentValue, G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[T],
    palette: &[[u8; 4]],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let color = palette[raw.to_usize()];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_compact_viewport_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    palette: &[[u8; 4]],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let color = palette[raw.to_usize()];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_compact_sample_cache_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    palette: &[[u8; 4]],
    grid: &MomentGrid,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let index = cached_sample.row() * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                let color = palette[values[index].to_usize()];
                if color[3] != 0 {
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                }
                pixel += 4;
            }
        });
}

fn render_f32_storage<G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    color_table: &ColorTable,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(value) = values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let color = color_table.color_for_value(value);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_f32_viewport_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    color_table: &ColorTable,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(value) = values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let color = color_table.color_for_value(value);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_f32_sample_cache_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    color_table: &ColorTable,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let index = cached_sample.row() * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                let value = values[index];
                if value.is_finite() {
                    let color = color_table.color_for_value(value);
                    if color[3] != 0 {
                        row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    }
                }
                pixel += 4;
            }
        });
}

fn render_storm_relative_storage<T: RawMomentValue, G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[T],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied().map(RawMomentValue::to_usize) else {
                        continue;
                    };
                    if grid.nodata == Some(raw as u16) {
                        continue;
                    }
                    let color = if grid.range_folded == Some(raw as u16) {
                        value_lookup.color_table.range_folded_color()
                    } else {
                        let velocity = (raw as f32 - grid.offset) / grid.scale;
                        let relative = velocity
                            - value_lookup
                                .row_motion
                                .get(candidate.row)
                                .copied()
                                .unwrap_or(0.0);
                        value_lookup.color_table.color_for_value(relative)
                    };
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_viewport_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied().map(RawMomentValue::to_usize) else {
                        continue;
                    };
                    if grid.nodata == Some(raw as u16) {
                        continue;
                    }
                    let color = if grid.range_folded == Some(raw as u16) {
                        value_lookup.color_table.range_folded_color()
                    } else {
                        let velocity = (raw as f32 - grid.offset) / grid.scale;
                        let relative = velocity
                            - value_lookup
                                .row_motion
                                .get(candidate.row)
                                .copied()
                                .unwrap_or(0.0);
                        value_lookup.color_table.color_for_value(relative)
                    };
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn build_storm_relative_u8_row_palettes(
    grid: &MomentGrid,
    row_motion: &[f32],
    color_table: &ColorTable,
) -> Vec<[[u8; 4]; 256]> {
    row_motion
        .par_iter()
        .map(|motion| {
            let mut palette = [[0, 0, 0, 0]; 256];
            for raw in 0..=u8::MAX {
                palette[usize::from(raw)] =
                    storm_relative_u8_color_for_raw(grid, color_table, raw, *motion);
            }
            palette
        })
        .collect()
}

fn storm_relative_u8_color_for_raw(
    grid: &MomentGrid,
    color_table: &ColorTable,
    raw: u8,
    row_motion: f32,
) -> [u8; 4] {
    let raw = u16::from(raw);
    if grid.nodata == Some(raw) {
        return [0, 0, 0, 0];
    }
    if grid.range_folded == Some(raw) {
        return color_table.range_folded_color();
    }
    let velocity = (raw as f32 - grid.offset) / grid.scale;
    color_table.color_for_value(velocity - row_motion)
}

fn render_storm_relative_u8_storage<G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[u8],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    row_palettes: &[[[u8; 4]; 256]],
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let Some(palette) = row_palettes.get(candidate.row) else {
                        continue;
                    };
                    let color = palette[usize::from(raw)];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_u8_viewport_storage(
    pixels: &mut [u8],
    values: &[u8],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    row_palettes: &[[[u8; 4]; 256]],
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let Some(palette) = row_palettes.get(candidate.row) else {
                        continue;
                    };
                    let color = palette[usize::from(raw)];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_u8_sample_cache_storage(
    pixels: &mut [u8],
    values: &[u8],
    grid: &MomentGrid,
    row_palettes: &[[[u8; 4]; 256]],
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let row = cached_sample.row();
                let index = row * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                debug_assert!(row < row_palettes.len());
                let color = row_palettes[row][usize::from(values[index])];
                if color[3] != 0 {
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                }
                pixel += 4;
            }
        });
}

fn render_storm_relative_sample_cache_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    grid: &MomentGrid,
    row_motion: &[f32],
    color_table: &ColorTable,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let row = cached_sample.row();
                let index = row * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                debug_assert!(row < row_motion.len());
                let raw = values[index].to_usize();
                if grid.nodata == Some(raw as u16) {
                    pixel += 4;
                    continue;
                }
                let color = if grid.range_folded == Some(raw as u16) {
                    color_table.range_folded_color()
                } else {
                    let velocity = (raw as f32 - grid.offset) / grid.scale;
                    let relative = velocity - row_motion[row];
                    color_table.color_for_value(relative)
                };
                if color[3] != 0 {
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                }
                pixel += 4;
            }
        });
}

fn render_storm_relative_f32_storage<G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(velocity) =
                        values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let relative = velocity
                        - value_lookup
                            .row_motion
                            .get(candidate.row)
                            .copied()
                            .unwrap_or(0.0);
                    let color = value_lookup.color_table.color_for_value(relative);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_f32_viewport_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(velocity) =
                        values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let relative = velocity
                        - value_lookup
                            .row_motion
                            .get(candidate.row)
                            .copied()
                            .unwrap_or(0.0);
                    let color = value_lookup.color_table.color_for_value(relative);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_f32_sample_cache_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_motion: &[f32],
    color_table: &ColorTable,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let row = cached_sample.row();
                let index = row * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                debug_assert!(row < row_motion.len());
                let velocity = values[index];
                if velocity.is_finite() {
                    let relative = velocity - row_motion[row];
                    let color = color_table.color_for_value(relative);
                    if color[3] != 0 {
                        row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    }
                }
                pixel += 4;
            }
        });
}

fn build_sample_cache_rows<R>(
    height: u32,
    lookup_table: &ViewportLookupTable,
    row_lookup: &AzimuthLookup,
    resolve: R,
) -> Vec<CachedRowBuild>
where
    R: Fn(SampleLookup) -> Option<ResolvedSample> + Sync,
{
    (0..height as usize)
        .into_par_iter()
        .map(|y| {
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return CachedRowBuild::empty();
            };
            let x_range = row_lookup_table.x_range.clone();
            let x_range_len = x_range.len();
            let mut start = None;
            let mut next_x = 0u32;
            let mut samples = Vec::with_capacity(x_range_len);
            let mut count = 0;
            for x in x_range {
                if let Some(sample) = row_lookup_table.lookup(x, row_lookup).and_then(&resolve)
                    && let Some(cached_sample) = CachedSample::new(sample)
                {
                    let start_x = *start.get_or_insert(x);
                    if samples.is_empty() {
                        next_x = start_x;
                    }
                    if x > next_x {
                        push_cached_sample_skip(&mut samples, x - next_x);
                    }
                    samples.push(cached_sample);
                    count += 1;
                    next_x = x + 1;
                }
            }
            if samples.is_empty() {
                CachedRowBuild::empty()
            } else {
                CachedRowBuild {
                    start: start.expect("non-empty row has a start"),
                    samples,
                    sample_count: count,
                }
            }
        })
        .collect()
}

fn build_geometry_cache_rows(
    height: u32,
    lookup_table: &ViewportLookupTable,
    row_lookup: &AzimuthLookup,
) -> Vec<CachedRowBuild> {
    (0..height as usize)
        .into_par_iter()
        .map(|y| {
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return CachedRowBuild::empty();
            };
            let x_range = row_lookup_table.x_range.clone();
            let mut start = None;
            let mut next_x = 0u32;
            let mut samples = Vec::with_capacity(x_range.len());
            let mut count = 0usize;
            for x in x_range {
                if let Some(sample) = row_lookup_table.lookup(x, row_lookup)
                    && let Some(cached_sample) = CachedSample::new(ResolvedSample {
                        row: sample.azimuth_bin,
                        gate: sample.gate,
                    })
                {
                    let start_x = *start.get_or_insert(x);
                    if samples.is_empty() {
                        next_x = start_x;
                    }
                    if x > next_x {
                        push_cached_sample_skip(&mut samples, x - next_x);
                    }
                    samples.push(cached_sample);
                    count += 1;
                    next_x = x + 1;
                }
            }
            if samples.is_empty() {
                CachedRowBuild::empty()
            } else {
                CachedRowBuild {
                    start: start.expect("non-empty geometry row has a start"),
                    samples,
                    sample_count: count,
                }
            }
        })
        .collect()
}

fn build_sample_cache_rows_from_geometry<R>(
    height: u32,
    geometry: CachedViewportGeometry<'_>,
    resolve: R,
) -> Vec<CachedRowBuild>
where
    R: Fn(SampleLookup) -> Option<ResolvedSample> + Sync,
{
    (0..height as usize)
        .into_par_iter()
        .map(|y| {
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return CachedRowBuild::empty();
            };
            let mut start = None;
            let mut next_x = 0u32;
            let mut x = row_start_x;
            let mut samples = Vec::with_capacity(row_samples.len());
            let mut count = 0usize;
            for cached_lookup in row_samples {
                if let Some(skip) = cached_lookup.skip_len() {
                    x += skip;
                    continue;
                }
                let sample = SampleLookup {
                    azimuth_bin: cached_lookup.row(),
                    gate: cached_lookup.gate(),
                };
                if let Some(sample) = resolve(sample)
                    && let Some(cached_sample) = CachedSample::new(sample)
                {
                    let start_x = *start.get_or_insert(x);
                    if samples.is_empty() {
                        next_x = start_x;
                    }
                    if x > next_x {
                        push_cached_sample_skip(&mut samples, x - next_x);
                    }
                    samples.push(cached_sample);
                    count += 1;
                    next_x = x + 1;
                }
                x += 1;
            }
            if samples.is_empty() {
                CachedRowBuild::empty()
            } else {
                CachedRowBuild {
                    start: start.expect("non-empty resolved geometry row has a start"),
                    samples,
                    sample_count: count,
                }
            }
        })
        .collect()
}

fn viewport_sample_cache_from_rows(
    volume_ptr: usize,
    cut_index: usize,
    moment: MomentType,
    width: u32,
    height: u32,
    row_builds: Vec<CachedRowBuild>,
) -> ViewportSampleCache {
    let (sample_count, row_spans, samples) = flatten_cached_rows(height, row_builds);
    ViewportSampleCache {
        volume_ptr,
        cut_index,
        moment,
        width,
        height,
        sample_count,
        row_spans,
        samples,
    }
}

fn flatten_cached_rows(
    height: u32,
    row_builds: Vec<CachedRowBuild>,
) -> (usize, Vec<CachedRowSpan>, Vec<CachedSample>) {
    let sample_storage_len = row_builds.iter().map(|row| row.samples.len()).sum();
    let mut row_spans = Vec::with_capacity(height as usize);
    let mut samples = Vec::with_capacity(sample_storage_len);
    let mut sample_count = 0;
    for row in row_builds {
        if row.samples.is_empty() {
            row_spans.push(CachedRowSpan::empty());
            continue;
        }
        let sample_offset = samples.len();
        let end = row.start + row.samples.len() as u32;
        sample_count += row.sample_count;
        row_spans.push(CachedRowSpan {
            start: row.start,
            end,
            sample_offset,
        });
        samples.extend(row.samples);
    }
    while row_spans.len() < height as usize {
        row_spans.push(CachedRowSpan::empty());
    }
    (sample_count, row_spans, samples)
}

fn push_cached_sample_skip(samples: &mut Vec<CachedSample>, mut pixel_count: u32) {
    while pixel_count > 0 {
        let chunk = pixel_count.min(CachedSample::SKIP_MASK);
        samples.push(CachedSample::skip(chunk).expect("positive skip chunk fits"));
        pixel_count -= chunk;
    }
}

fn resolve_compact_sample<T: RawMomentValue>(
    values: &[T],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    sample: SampleLookup,
) -> Option<ResolvedSample> {
    let gate_count = grid.gate_range.gate_count;
    for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
        let index = candidate.row * gate_count + sample.gate;
        if index >= values.len() {
            continue;
        }
        let raw = values[index].to_usize() as u16;
        if grid.nodata == Some(raw) {
            continue;
        }
        return Some(ResolvedSample {
            row: candidate.row,
            gate: sample.gate,
        });
    }
    None
}

fn resolve_f32_sample(
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    sample: SampleLookup,
) -> Option<ResolvedSample> {
    let gate_count = grid.gate_range.gate_count;
    for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
        let index = candidate.row * gate_count + sample.gate;
        if index < values.len() && values[index].is_finite() {
            return Some(ResolvedSample {
                row: candidate.row,
                gate: sample.gate,
            });
        }
    }
    None
}

fn raster_lookup(
    x: u32,
    y: u32,
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    geometry: RasterGeometry,
) -> Option<SampleLookup> {
    let dx = x as f32 - geometry.center_x;
    let dy = geometry.center_y - y as f32;
    let radius_sq = dx.mul_add(dx, dy * dy);
    if radius_sq > geometry.radius_sq_px {
        return None;
    }

    let radius = radius_sq.sqrt();
    let range_m = radius / geometry.radius_px * geometry.max_range_m;
    let gate = ((range_m - grid.gate_range.first_gate_m as f32)
        / grid.gate_range.gate_spacing_m.max(1) as f32)
        .round() as isize;
    if gate < 0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }

    let azimuth_deg = azimuth_from_xy(dx, dy);
    let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
    Some(SampleLookup {
        azimuth_bin,
        gate: gate as usize,
    })
}

fn viewport_lookup(
    x: u32,
    y: u32,
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    geometry: ViewportGeometry,
) -> Option<SampleLookup> {
    let dx_km = (x as f32 + 0.5 - geometry.radar_x_px) * geometry.km_per_px_x;
    let dy_km = (geometry.radar_y_px - (y as f32 + 0.5)) * geometry.km_per_px_y;
    let range_km_sq = dx_km.mul_add(dx_km, dy_km * dy_km);
    if range_km_sq > geometry.max_range_km_sq {
        return None;
    }

    let range_m = range_km_sq.sqrt() * 1000.0;
    let gate = ((range_m - grid.gate_range.first_gate_m as f32)
        / grid.gate_range.gate_spacing_m.max(1) as f32)
        .round() as isize;
    if gate < 0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }

    let azimuth_deg = azimuth_from_xy(dx_km, dy_km);
    let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
    Some(SampleLookup {
        azimuth_bin,
        gate: gate as usize,
    })
}

fn build_u8_palette(grid: &MomentGrid, color_table: &ColorTable) -> [[u8; 4]; 256] {
    let mut palette = [[0, 0, 0, 0]; 256];
    for raw in 0..=u8::MAX {
        palette[usize::from(raw)] = color_for_raw(grid, color_table, u16::from(raw));
    }
    palette
}

fn build_u16_palette(grid: &MomentGrid, color_table: &ColorTable) -> Vec<[u8; 4]> {
    let max_raw = match &grid.storage {
        MomentStorage::U16(values) => values.iter().copied().max().unwrap_or(0),
        _ => u16::MAX,
    };
    let mut palette = vec![[0, 0, 0, 0]; usize::from(max_raw) + 1];
    for raw in 0..=max_raw {
        palette[usize::from(raw)] = color_for_raw(grid, color_table, raw);
    }
    palette
}

fn color_for_raw(grid: &MomentGrid, color_table: &ColorTable, raw: u16) -> [u8; 4] {
    if grid.nodata == Some(raw) {
        return [0, 0, 0, 0];
    }
    if grid.range_folded == Some(raw) {
        return color_table.range_folded_color();
    }
    color_table.color_for_value((raw as f32 - grid.offset) / grid.scale)
}

pub fn dealias_velocity_grid(cut: &ElevationCut, source: &MomentGrid) -> MomentGrid {
    let rows = source.radial_count();
    let gate_count = source.gate_range.gate_count;
    let fallback_nyquist = median_nyquist_mps(cut, source);
    let mut corrected = vec![DEALIASED_VELOCITY_NODATA; rows.saturating_mul(gate_count)];

    corrected
        .par_chunks_mut(gate_count.max(1))
        .enumerate()
        .for_each_init(
            || (vec![f32::NAN; gate_count], vec![f32::NAN; gate_count]),
            |(observed, row_values), (row, output)| {
                if output.len() != gate_count {
                    return;
                }

                copy_scaled_velocity_row(source, row, observed);
                row_values.fill(f32::NAN);
                let nyquist = row_nyquist_mps(cut, source, row).or(fallback_nyquist);
                if let Some(nyquist) = nyquist.filter(|value| value.is_finite() && *value > 0.0) {
                    if let Some(seed) = pick_dealias_seed(observed, nyquist) {
                        row_values[seed] = observed[seed];
                        walk_dealias_radial(observed, nyquist, None, row_values, seed, 1);
                        walk_dealias_radial(observed, nyquist, None, row_values, seed, -1);
                    }
                } else {
                    row_values.copy_from_slice(observed);
                }

                encode_dealiased_velocity_row(row_values, output);
            },
        );

    apply_azimuthal_dealias_consensus(cut, source, &mut corrected, fallback_nyquist, 2);
    suppress_isolated_dealias_spikes(cut, source, &mut corrected, fallback_nyquist);

    MomentGrid {
        moment: MomentType::Velocity,
        gate_range: source.gate_range.clone(),
        scale: DEALIASED_VELOCITY_SCALE,
        offset: DEALIASED_VELOCITY_OFFSET,
        nodata: Some(DEALIASED_VELOCITY_NODATA),
        range_folded: None,
        radial_indices: source.radial_indices.clone(),
        storage: MomentStorage::U16(corrected),
    }
}

const DEALIASED_VELOCITY_SCALE: f32 = 10.0;
const DEALIASED_VELOCITY_OFFSET: f32 = 32_768.0;
const DEALIASED_VELOCITY_NODATA: u16 = 0;
const DEALIAS_SPIKE_NEIGHBOR_ROWS: isize = 3;
const DEALIAS_SPIKE_NEIGHBOR_GATES: isize = 1;
const DEALIAS_SPIKE_MIN_SUPPORT: usize = 2;
const DEALIAS_CONSENSUS_MAX_FOLD: i32 = 4;
const DEALIAS_RADIAL_CHAIN_MAX_GATE_GAP: usize = 3;
const DEALIAS_RADIAL_MAX_FOLD: i32 = 3;

fn encode_dealiased_velocity_row(values: &[f32], output: &mut [u16]) {
    debug_assert_eq!(values.len(), output.len());
    for (value, raw) in values.iter().zip(output.iter_mut()) {
        *raw = encode_dealiased_velocity(*value);
    }
}

fn encode_dealiased_velocity(value: f32) -> u16 {
    if !value.is_finite() {
        return DEALIASED_VELOCITY_NODATA;
    }
    (value * DEALIASED_VELOCITY_SCALE + DEALIASED_VELOCITY_OFFSET)
        .round()
        .clamp(1.0, u16::MAX as f32) as u16
}

fn decode_dealiased_velocity(raw: u16) -> Option<f32> {
    if raw == DEALIASED_VELOCITY_NODATA {
        return None;
    }
    Some((raw as f32 - DEALIASED_VELOCITY_OFFSET) / DEALIASED_VELOCITY_SCALE)
}

fn apply_azimuthal_dealias_consensus(
    cut: &ElevationCut,
    source: &MomentGrid,
    corrected: &mut [u16],
    fallback_nyquist: Option<f32>,
    passes: usize,
) {
    let rows = source.radial_count();
    let gate_count = source.gate_range.gate_count;
    if rows < 3 || gate_count == 0 || corrected.len() != rows.saturating_mul(gate_count) {
        return;
    }

    for _ in 0..passes {
        let snapshot = corrected.to_vec();
        corrected
            .par_chunks_mut(gate_count)
            .enumerate()
            .for_each(|(row, output)| {
                let Some(nyquist) = row_nyquist_mps(cut, source, row)
                    .or(fallback_nyquist)
                    .filter(|value| value.is_finite() && *value > 0.0)
                else {
                    return;
                };

                for (gate, raw) in output.iter_mut().enumerate() {
                    let Some(observed) = source.scaled_value(row, gate) else {
                        continue;
                    };
                    let mut references = [0.0; 4];
                    let mut reference_count = 0usize;
                    for (neighbor_row, neighbor_gate) in [
                        gate.checked_sub(1).map(|gate| (row, gate)),
                        (gate + 1 < gate_count).then_some((row, gate + 1)),
                        row.checked_sub(1).map(|row| (row, gate)),
                        (row + 1 < rows).then_some((row + 1, gate)),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let Some(value) = decode_dealiased_velocity(
                            snapshot[neighbor_row * gate_count + neighbor_gate],
                        )
                        .filter(|value| value.is_finite()) else {
                            continue;
                        };
                        references[reference_count] = value;
                        reference_count += 1;
                    }
                    if reference_count < 2 {
                        continue;
                    }
                    let reference = median_small_f32(&mut references, reference_count);
                    let unfolded = unfold_velocity_to_reference(
                        observed,
                        reference,
                        nyquist,
                        reference_count,
                        DEALIAS_CONSENSUS_MAX_FOLD,
                    );
                    *raw = encode_dealiased_velocity(unfolded);
                }
            });
    }
}

fn suppress_isolated_dealias_spikes(
    cut: &ElevationCut,
    source: &MomentGrid,
    corrected: &mut [u16],
    fallback_nyquist: Option<f32>,
) {
    let rows = source.radial_count();
    let gate_count = source.gate_range.gate_count;
    if rows < 3 || gate_count == 0 || corrected.len() != rows.saturating_mul(gate_count) {
        return;
    }

    let original = corrected.to_vec();
    let support_context = DealiasSpikeContext {
        cut,
        source,
        corrected: &original,
        fallback_nyquist,
    };
    corrected
        .par_chunks_mut(gate_count)
        .enumerate()
        .for_each(|(row, output)| {
            let Some(nyquist) = row_nyquist_mps(cut, source, row)
                .or(fallback_nyquist)
                .filter(|value| value.is_finite() && *value > 0.0)
            else {
                return;
            };
            for (gate, raw) in output.iter_mut().enumerate() {
                let Some(observed) = source.scaled_value(row, gate) else {
                    continue;
                };
                let Some(corrected_value) =
                    decode_dealiased_velocity(original[row * gate_count + gate])
                else {
                    continue;
                };
                let Some(fold) = dealias_fold_count(observed, corrected_value, nyquist) else {
                    continue;
                };
                let support = dealias_fold_neighbor_support(
                    &support_context,
                    row,
                    gate,
                    fold,
                    corrected_value,
                );
                if support < DEALIAS_SPIKE_MIN_SUPPORT {
                    *raw = encode_dealiased_velocity(observed);
                }
            }
        });
}

struct DealiasSpikeContext<'a> {
    cut: &'a ElevationCut,
    source: &'a MomentGrid,
    corrected: &'a [u16],
    fallback_nyquist: Option<f32>,
}

fn dealias_fold_neighbor_support(
    context: &DealiasSpikeContext<'_>,
    row: usize,
    gate: usize,
    fold: i32,
    corrected_value: f32,
) -> usize {
    let source = context.source;
    let rows = source.radial_count();
    let gate_count = source.gate_range.gate_count;
    let mut support = 0;
    for row_offset in -DEALIAS_SPIKE_NEIGHBOR_ROWS..=DEALIAS_SPIKE_NEIGHBOR_ROWS {
        if row_offset == 0 {
            continue;
        }
        let Some(neighbor_row) = row.checked_add_signed(row_offset) else {
            continue;
        };
        if neighbor_row >= rows {
            continue;
        }
        let Some(neighbor_nyquist) = row_nyquist_mps(context.cut, source, neighbor_row)
            .or(context.fallback_nyquist)
            .filter(|value| value.is_finite() && *value > 0.0)
        else {
            continue;
        };
        for gate_offset in -DEALIAS_SPIKE_NEIGHBOR_GATES..=DEALIAS_SPIKE_NEIGHBOR_GATES {
            let Some(neighbor_gate) = gate.checked_add_signed(gate_offset) else {
                continue;
            };
            if neighbor_gate >= gate_count {
                continue;
            }
            let Some(neighbor_observed) = source.scaled_value(neighbor_row, neighbor_gate) else {
                continue;
            };
            let Some(neighbor_corrected) = decode_dealiased_velocity(
                context.corrected[neighbor_row * gate_count + neighbor_gate],
            ) else {
                continue;
            };
            if dealias_fold_count(neighbor_observed, neighbor_corrected, neighbor_nyquist)
                == Some(fold)
                && (neighbor_corrected - corrected_value).abs() <= 0.65 * neighbor_nyquist
            {
                support += 1;
            }
        }
    }
    support
}

fn dealias_fold_count(observed: f32, corrected: f32, nyquist: f32) -> Option<i32> {
    if !observed.is_finite() || !corrected.is_finite() || !nyquist.is_finite() || nyquist <= 0.0 {
        return None;
    }
    let fold = ((corrected - observed) / (2.0 * nyquist)).round() as i32;
    if fold == 0 {
        return None;
    }
    let expected_delta = 2.0 * nyquist * fold as f32;
    let residual = (corrected - observed - expected_delta).abs();
    (residual <= 0.35 * nyquist).then_some(fold)
}

fn copy_scaled_velocity_row(source: &MomentGrid, row: usize, row_values: &mut [f32]) {
    row_values.fill(f32::NAN);
    let gate_count = source.gate_range.gate_count;
    if gate_count == 0 || row_values.len() != gate_count {
        return;
    }
    let Some(row_start) = row.checked_mul(gate_count) else {
        return;
    };
    let row_end = row_start + gate_count;
    match &source.storage {
        MomentStorage::U8(values) => {
            let Some(raw_row) = values.get(row_start..row_end) else {
                return;
            };
            for (raw, value) in raw_row.iter().zip(row_values.iter_mut()) {
                let raw = u16::from(*raw);
                if source.nodata == Some(raw) || source.range_folded == Some(raw) {
                    continue;
                }
                *value = (raw as f32 - source.offset) / source.scale;
            }
        }
        MomentStorage::U16(values) => {
            let Some(raw_row) = values.get(row_start..row_end) else {
                return;
            };
            for (raw, value) in raw_row.iter().zip(row_values.iter_mut()) {
                if source.nodata == Some(*raw) || source.range_folded == Some(*raw) {
                    continue;
                }
                *value = (*raw as f32 - source.offset) / source.scale;
            }
        }
        MomentStorage::F32(values) => {
            let Some(source_row) = values.get(row_start..row_end) else {
                return;
            };
            row_values.copy_from_slice(source_row);
        }
    }
}

fn median_nyquist_mps(cut: &ElevationCut, grid: &MomentGrid) -> Option<f32> {
    let mut values = grid
        .radial_indices
        .iter()
        .filter_map(|radial_index| cut.radials.get(*radial_index)?.nyquist_velocity_mps)
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    Some(values[values.len() / 2])
}

fn row_nyquist_mps(cut: &ElevationCut, grid: &MomentGrid, row: usize) -> Option<f32> {
    let radial_index = *grid.radial_indices.get(row)?;
    cut.radials.get(radial_index)?.nyquist_velocity_mps
}

fn pick_dealias_seed(row_values: &[f32], nyquist: f32) -> Option<usize> {
    let mut fallback = None;
    let gate_count = row_values.len();
    let gate_midpoint = gate_count / 2;
    for offset in 0..gate_count {
        let left = gate_midpoint.checked_sub(offset);
        let right = gate_midpoint + offset;
        for gate in [left, (right < gate_count).then_some(right)]
            .into_iter()
            .flatten()
        {
            let Some(value) = row_values
                .get(gate)
                .copied()
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            fallback.get_or_insert(gate);
            if value.abs() <= 0.85 * nyquist {
                return Some(gate);
            }
        }
    }
    fallback
}

fn walk_dealias_radial(
    observed_values: &[f32],
    nyquist: f32,
    previous_row: Option<&[f32]>,
    row_values: &mut [f32],
    seed: usize,
    direction: isize,
) {
    let gate_count = observed_values.len();
    let mut gate = seed as isize + direction;
    let mut last_gate = Some(seed);
    let mut last_two_gate: Option<usize> = None;
    while (0..gate_count as isize).contains(&gate) {
        let current_gate = gate as usize;
        let Some(observed) = observed_values
            .get(current_gate)
            .copied()
            .filter(|value| value.is_finite())
        else {
            gate += direction;
            continue;
        };
        let mut references = [0.0; 3];
        let mut reference_count = 0usize;
        if let Some(last) = last_gate
            && current_gate.abs_diff(last) <= DEALIAS_RADIAL_CHAIN_MAX_GATE_GAP
            && row_values[last].is_finite()
        {
            references[reference_count] = row_values[last];
            reference_count += 1;
            if let Some(last_two) = last_two_gate
                && last.abs_diff(last_two) <= DEALIAS_RADIAL_CHAIN_MAX_GATE_GAP
                && row_values[last_two].is_finite()
            {
                let slope = row_values[last] - row_values[last_two];
                references[reference_count] = row_values[last] + slope;
                reference_count += 1;
            }
        }
        if let Some(previous) = previous_row
            && let Some(previous_value) = previous.get(current_gate).copied()
            && previous_value.is_finite()
        {
            references[reference_count] = previous_value;
            reference_count += 1;
        }
        if reference_count == 0 {
            row_values[current_gate] = observed;
            last_two_gate = None;
        } else {
            let reference = median_small_f32(&mut references, reference_count);
            row_values[current_gate] = unfold_velocity_to_reference(
                observed,
                reference,
                nyquist,
                reference_count,
                DEALIAS_RADIAL_MAX_FOLD,
            );
            last_two_gate = last_gate;
        }
        last_gate = Some(current_gate);
        gate += direction;
    }
}

fn median_small_f32(values: &mut [f32], count: usize) -> f32 {
    debug_assert!(count > 0 && count <= values.len());
    values[..count].sort_by(f32::total_cmp);
    values[count / 2]
}

fn unfold_velocity_to_reference(
    observed: f32,
    reference: f32,
    nyquist: f32,
    reference_count: usize,
    max_abs_fold: i32,
) -> f32 {
    let fold = ((reference - observed) / (2.0 * nyquist))
        .round()
        .clamp(-(max_abs_fold as f32), max_abs_fold as f32);
    if fold == 0.0 {
        return observed;
    }
    let unfolded = observed + 2.0 * nyquist * fold;
    let continuity_error = (unfolded - reference).abs();
    let close_enough = continuity_error <= (0.35 * nyquist).max(4.0);
    let high_opposite_sides = observed.signum() != reference.signum()
        && observed.abs() >= 0.60 * nyquist
        && reference.abs() >= 0.60 * nyquist;
    if close_enough && (high_opposite_sides || reference_count >= 2) {
        unfolded
    } else {
        observed
    }
}

fn max_range_m(grid: &MomentGrid) -> f32 {
    grid.gate_range.first_gate_m as f32
        + grid.gate_range.gate_spacing_m as f32 * grid.gate_range.gate_count as f32
}

fn azimuth_from_xy(dx: f32, dy: f32) -> f32 {
    let mut degrees = dx.atan2(dy) * 180.0 / PI;
    if degrees < 0.0 {
        degrees += 360.0;
    }
    degrees
}

struct AzimuthLookup {
    bins: Vec<AzimuthBin>,
}

impl AzimuthLookup {
    fn new(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        Self::from_row_azimuths_iter(
            grid,
            grid.radial_indices
                .iter()
                .enumerate()
                .filter_map(|(row, radial_index)| {
                    cut.radials
                        .get(*radial_index)
                        .map(|radial| (row, radial.azimuth_deg))
                }),
        )
    }

    /// Build the lookup from azimuths given per grid row rather than read from
    /// the cut.
    ///
    /// This is what an upsampled grid needs. `interpolate::upsample_moment_grid`
    /// inserts synthetic sub-beams between the native ones and points each
    /// one's `radial_indices` entry at its nearest PARENT radial, so that
    /// Nyquist and beam-geometry lookups stay valid. That makes the cut unable
    /// to say where a sub-beam actually points: every one of them would report
    /// its parent's azimuth, the sub-beams would collapse back onto the native
    /// beams, and the display would be identical to native while costing two to
    /// four times the memory.
    fn from_row_azimuths(row_azimuths_deg: &[f32], grid: &MomentGrid) -> Self {
        Self::from_row_azimuths_iter(grid, row_azimuths_deg.iter().copied().enumerate())
    }

    fn from_row_azimuths_iter(
        grid: &MomentGrid,
        row_azimuths: impl Iterator<Item = (usize, f32)>,
    ) -> Self {
        let mut groups = vec![None; AZIMUTH_BINS];
        for (row, azimuth_deg) in row_azimuths {
            let azimuth = azimuth_deg.rem_euclid(360.0);
            let bin = azimuth_bin(azimuth);
            let group = groups[bin].get_or_insert_with(|| AzimuthGroup {
                azimuth: bin as f32 * AZIMUTH_BIN_WIDTH_DEG,
                candidates: Vec::new(),
            });
            group.candidates.push(RowCandidate {
                row,
                valid_extent: row_valid_extent(grid, row),
            });
        }

        let mut groups = groups.into_iter().flatten().collect::<Vec<_>>();
        for group in &mut groups {
            group
                .candidates
                .sort_by_key(|candidate| std::cmp::Reverse(candidate.rank()));
        }
        groups.sort_by(|left, right| left.azimuth.total_cmp(&right.azimuth));

        let mut bins = vec![AzimuthBin::default(); AZIMUTH_BINS];
        if groups.is_empty() {
            return Self { bins };
        }
        if groups.len() == 1 {
            fill_azimuth_bins(&mut bins, 0.0, 360.0, &groups[0].candidates);
            return Self { bins };
        }

        for index in 0..groups.len() {
            let group = &groups[index];
            let prev_azimuth = groups
                .get(index.wrapping_sub(1))
                .or_else(|| groups.last())
                .map(|group| group.azimuth)
                .unwrap_or(group.azimuth);
            let next_azimuth = groups
                .get(index + 1)
                .or_else(|| groups.first())
                .map(|group| group.azimuth)
                .unwrap_or(group.azimuth);
            let left_width = (clockwise_delta_deg(prev_azimuth, group.azimuth) * 0.5)
                .min(MAX_AZIMUTH_HALF_WIDTH_DEG);
            let right_width = (clockwise_delta_deg(group.azimuth, next_azimuth) * 0.5)
                .min(MAX_AZIMUTH_HALF_WIDTH_DEG);
            fill_azimuth_bins(
                &mut bins,
                group.azimuth - left_width,
                group.azimuth + right_width,
                &group.candidates,
            );
        }

        Self { bins }
    }

    #[cfg(test)]
    fn row_for_azimuth(&self, azimuth_deg: f32) -> Option<usize> {
        self.candidates_for_bin(self.filled_bin_for_azimuth(azimuth_deg)?)
            .first()
            .map(|candidate| candidate.row)
    }

    fn filled_bin_for_azimuth(&self, azimuth_deg: f32) -> Option<usize> {
        let bin = azimuth_bin(azimuth_deg);
        (!self.bins[bin].is_empty()).then_some(bin)
    }

    fn candidates_for_bin(&self, bin: usize) -> &[RowCandidate] {
        self.bins[bin].candidates()
    }
}

#[derive(Clone, Copy, Debug)]
struct RowCandidate {
    row: usize,
    valid_extent: usize,
}

impl RowCandidate {
    fn rank(self) -> (usize, usize) {
        (self.valid_extent, self.row)
    }
}

impl Default for RowCandidate {
    fn default() -> Self {
        Self {
            row: usize::MAX,
            valid_extent: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct AzimuthGroup {
    azimuth: f32,
    candidates: Vec<RowCandidate>,
}

#[derive(Clone, Copy, Debug)]
struct AzimuthBin {
    candidates: [RowCandidate; MAX_AZIMUTH_CANDIDATES],
    len: usize,
}

impl Default for AzimuthBin {
    fn default() -> Self {
        Self {
            candidates: [RowCandidate::default(); MAX_AZIMUTH_CANDIDATES],
            len: 0,
        }
    }
}

impl AzimuthBin {
    fn is_empty(self) -> bool {
        self.len == 0
    }

    fn candidates(&self) -> &[RowCandidate] {
        &self.candidates[..self.len]
    }

    fn push_candidate(&mut self, candidate: RowCandidate) {
        if self
            .candidates()
            .iter()
            .any(|existing| existing.row == candidate.row)
        {
            return;
        }

        let insert_at = self
            .candidates()
            .iter()
            .position(|existing| candidate.rank() > existing.rank())
            .unwrap_or(self.len);
        if self.len < MAX_AZIMUTH_CANDIDATES {
            for index in (insert_at..self.len).rev() {
                self.candidates[index + 1] = self.candidates[index];
            }
            self.candidates[insert_at] = candidate;
            self.len += 1;
        } else if insert_at < MAX_AZIMUTH_CANDIDATES {
            for index in (insert_at..MAX_AZIMUTH_CANDIDATES - 1).rev() {
                self.candidates[index + 1] = self.candidates[index];
            }
            self.candidates[insert_at] = candidate;
        }
    }
}

fn azimuth_bin(azimuth_deg: f32) -> usize {
    ((azimuth_deg.rem_euclid(360.0) / AZIMUTH_BIN_WIDTH_DEG).round() as usize) % AZIMUTH_BINS
}

fn row_valid_extent(grid: &MomentGrid, row: usize) -> usize {
    let gate_count = grid.gate_range.gate_count;
    let start = row.saturating_mul(gate_count);
    let Some(end) = start.checked_add(gate_count) else {
        return 0;
    };
    match &grid.storage {
        MomentStorage::U8(values) => values
            .get(start..end)
            .and_then(|row| {
                row.iter().rposition(|raw| {
                    let raw = u16::from(*raw);
                    grid.nodata != Some(raw)
                })
            })
            .map(|gate| gate + 1)
            .unwrap_or(0),
        MomentStorage::U16(values) => values
            .get(start..end)
            .and_then(|row| row.iter().rposition(|raw| grid.nodata != Some(*raw)))
            .map(|gate| gate + 1)
            .unwrap_or(0),
        MomentStorage::F32(values) => values
            .get(start..end)
            .and_then(|row| row.iter().rposition(|value| value.is_finite()))
            .map(|gate| gate + 1)
            .unwrap_or(0),
    }
}

fn fill_azimuth_bins(bins: &mut [AzimuthBin], start_deg: f32, end_deg: f32, rows: &[RowCandidate]) {
    let start_bin = (start_deg / AZIMUTH_BIN_WIDTH_DEG).floor() as i32;
    let end_bin = (end_deg / AZIMUTH_BIN_WIDTH_DEG).ceil() as i32;
    for bin in start_bin..=end_bin {
        let target = &mut bins[bin.rem_euclid(AZIMUTH_BINS as i32) as usize];
        for row in rows {
            target.push_candidate(*row);
        }
    }
}

fn clockwise_delta_deg(from_deg: f32, to_deg: f32) -> f32 {
    (to_deg - from_deg).rem_euclid(360.0)
}

fn row_motion_components(
    cut: &ElevationCut,
    grid: &MomentGrid,
    storm_motion: StormMotion,
) -> Vec<f32> {
    grid.radial_indices
        .iter()
        .map(|radial_index| {
            cut.radials
                .get(*radial_index)
                .map(|radial| motion_component_away_mps(storm_motion, radial.azimuth_deg))
                .unwrap_or(0.0)
        })
        .collect()
}

pub fn storm_relative_velocity_mps(
    radar_velocity_mps: f32,
    beam_azimuth_deg: f32,
    storm_motion: StormMotion,
) -> f32 {
    radar_velocity_mps - motion_component_away_mps(storm_motion, beam_azimuth_deg)
}

fn motion_component_away_mps(storm_motion: StormMotion, beam_azimuth_deg: f32) -> f32 {
    let delta = (storm_motion.direction_deg - beam_azimuth_deg).to_radians();
    storm_motion.speed_mps * delta.cos()
}

pub fn color_family_for_moment(moment: &MomentType) -> ColorTableFamily {
    // Exhaustive on purpose - no wildcard. The dual-polarimetric moments used
    // to fall through a `_ => Generic` arm onto a ramp spanning 0..100, which
    // rendered ZDR (-13..20 dB) and correlation coefficient (0.21..1.05) as a
    // single flat wash. A wildcard here cannot be told apart from a deliberate
    // choice, so the next moment added to `radar_core` must fail to compile
    // rather than silently join them.
    match moment {
        MomentType::Reflectivity => ColorTableFamily::Reflectivity,
        MomentType::Velocity => ColorTableFamily::Velocity,
        MomentType::SpectrumWidth => ColorTableFamily::SpectrumWidth,
        MomentType::DifferentialReflectivity => ColorTableFamily::DifferentialReflectivity,
        MomentType::CorrelationCoefficient => ColorTableFamily::CorrelationCoefficient,
        MomentType::DifferentialPhase => ColorTableFamily::DifferentialPhase,
        MomentType::SpecificDifferentialPhase => ColorTableFamily::SpecificDifferentialPhase,
        // Only genuinely unclassified moments. Every cached WSR-88D volume
        // carries `Unknown("CFP")`, clutter filter power, which has no family
        // of its own yet.
        MomentType::Unknown(_) => ColorTableFamily::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentRow, RadarSite, RadarVolume, Radial};

    #[test]
    fn base_layer_starts_visible() {
        assert!(RenderLayer::base(MomentType::Reflectivity).visible);
    }

    #[test]
    fn azimuth_places_north_at_zero_degrees() {
        assert_eq!(azimuth_from_xy(0.0, 1.0).round(), 0.0);
        assert_eq!(azimuth_from_xy(1.0, 0.0).round(), 90.0);
        assert_eq!(azimuth_from_xy(0.0, -1.0).round(), 180.0);
        assert_eq!(azimuth_from_xy(-1.0, 0.0).round(), 270.0);
    }

    #[test]
    fn velocity_table_has_a_hard_zero_boundary() {
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Velocity);
        let inbound = table.color_for_value(-2.0);
        let outbound = table.color_for_value(2.0);
        let neutral = table.color_for_value(0.0);

        assert_ne!(inbound, outbound);
        assert_ne!(neutral, inbound);
        assert_ne!(neutral, outbound);
    }

    #[test]
    fn range_folded_gates_are_visible() {
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Velocity);

        assert_eq!(table.range_folded_color()[3], 245);
    }

    #[test]
    fn velocity_range_folded_bins_render_table_rf_color() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Velocity);

        assert_eq!(color_for_raw(grid, table, 1), table.range_folded_color());
    }

    #[test]
    fn reflectivity_range_folded_bins_render_table_rf_color() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Reflectivity);

        assert_eq!(color_for_raw(grid, table, 1), table.range_folded_color());
    }

    #[test]
    fn lightweight_velocity_dealias_unfolds_radial_continuity() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 5,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: Some(10.0),
            radial_status: None,
        });
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: vec![0],
            storage: MomentStorage::F32(vec![0.0, 5.0, 9.0, -9.0, -7.0]),
        };

        let corrected = dealias_velocity_grid(&cut, &grid);
        assert!(matches!(corrected.storage, MomentStorage::U16(_)));

        let values = (0..corrected.gate_range.gate_count)
            .map(|gate| corrected.scaled_value(0, gate).expect("corrected gate"))
            .collect::<Vec<_>>();
        assert_eq!(values, vec![0.0, 5.0, 9.0, 11.0, 13.0]);
    }

    #[test]
    fn velocity_dealias_caps_radial_fold_excursions() {
        let observed = vec![0.0, 3.0, 106.0];
        let mut row_values = vec![f32::NAN; observed.len()];
        row_values[0] = observed[0];

        walk_dealias_radial(&observed, 10.0, None, &mut row_values, 0, 1);

        assert_eq!(row_values, vec![0.0, 3.0, 106.0]);
    }

    #[test]
    fn velocity_dealias_resets_stale_radial_slope_after_gap() {
        let observed = vec![
            0.0,
            2.0,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            60.0,
            78.0,
        ];
        let mut row_values = vec![f32::NAN; observed.len()];
        row_values[0] = observed[0];

        walk_dealias_radial(&observed, 10.0, None, &mut row_values, 0, 1);

        assert_eq!(row_values[8], 60.0);
        assert_eq!(row_values[9], 78.0);
    }

    #[test]
    fn velocity_dealias_suppresses_unsupported_radial_spikes() {
        let quiet = vec![0.0, 3.0, 5.0, 7.0, 8.0];
        let folded = vec![0.0, 5.0, 9.0, -9.0, -7.0];
        let (cut, grid) = test_velocity_grid_rows(vec![
            quiet.clone(),
            quiet.clone(),
            folded,
            quiet.clone(),
            quiet,
        ]);

        let corrected = dealias_velocity_grid(&cut, &grid);

        assert_eq!(corrected.scaled_value(2, 3), Some(-9.0));
        assert_eq!(corrected.scaled_value(2, 4), Some(-7.0));
    }

    #[test]
    fn velocity_dealias_preserves_supported_adjacent_folds() {
        let quiet = vec![0.0, 3.0, 5.0, 7.0, 8.0];
        let folded = vec![0.0, 5.0, 9.0, -9.0, -7.0];
        let (cut, grid) = test_velocity_grid_rows(vec![
            quiet.clone(),
            folded.clone(),
            folded.clone(),
            folded,
            quiet,
        ]);

        let corrected = dealias_velocity_grid(&cut, &grid);

        assert_eq!(corrected.scaled_value(2, 3), Some(11.0));
        assert_eq!(corrected.scaled_value(2, 4), Some(13.0));
    }

    #[test]
    fn storm_relative_u8_row_palette_matches_direct_color_math() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let tables = ColorTableSet::default();
        let color_table = tables.for_family(ColorTableFamily::Velocity);
        let row_motion = [3.25];
        let palettes = build_storm_relative_u8_row_palettes(grid, &row_motion, color_table);

        for raw in [0, 1, 119, 129, 139] {
            assert_eq!(
                palettes[0][usize::from(raw)],
                storm_relative_u8_color_for_raw(grid, color_table, raw, row_motion[0])
            );
        }
    }

    #[test]
    fn custom_color_table_feeds_precomputed_u8_palette() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let table = ColorTable::parse(
            "unit test velocity",
            "units: m/s\ncolor: -20 1 2 3\ncolor: 0 10 20 30\ncolor: 20 40 50 60",
        )
        .expect("custom color table");

        let palette = build_u8_palette(grid, &table);

        assert_eq!(palette[64], [10, 20, 30, 255]);
        assert_eq!(palette[74], [25, 35, 45, 255]);
    }

    #[test]
    fn storm_relative_velocity_subtracts_motion_along_beam() {
        let storm_motion = StormMotion {
            direction_deg: 0.0,
            speed_mps: 10.0,
        };

        assert_eq!(
            storm_relative_velocity_mps(10.0, 0.0, storm_motion).round(),
            0.0
        );
        assert_eq!(
            storm_relative_velocity_mps(10.0, 180.0, storm_motion).round(),
            20.0
        );
        assert_eq!(
            storm_relative_velocity_mps(10.0, 90.0, storm_motion).round(),
            10.0
        );
    }

    #[test]
    fn storm_motion_basis_matches_direct_projection() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let basis = StormMotionBasis::new(cut, grid);
        let storm_motion = StormMotion {
            direction_deg: 225.0,
            speed_mps: 18.0,
        };
        let row_motion = basis.row_motion_components(storm_motion);

        for (row, radial_index) in grid.radial_indices.iter().enumerate() {
            let radial = &cut.radials[*radial_index];
            let direct = motion_component_away_mps(storm_motion, radial.azimuth_deg);
            assert!((row_motion[row] - direct).abs() < 0.000_01);
        }
    }

    #[test]
    fn cached_sample_packs_lookup_into_four_bytes() {
        assert_eq!(std::mem::size_of::<CachedSample>(), 4);

        let sample = ResolvedSample {
            row: 3_599,
            gate: 1_832,
        };
        let cached = CachedSample::new(sample).expect("sample fits packed cache entry");

        assert_eq!(cached.sample(), Some(sample));
        let skip = CachedSample::skip(37).expect("skip fits packed cache entry");
        assert_eq!(skip.skip_len(), Some(37));
        assert_eq!(skip.sample(), None);
        assert_eq!(
            CachedSample::new(ResolvedSample {
                row: CachedSample::ROW_LIMIT,
                gate: 0
            }),
            None
        );
    }

    #[test]
    fn sample_cache_storage_upper_bound_scales_with_viewport_pixels() {
        let options = ViewportRasterOptions {
            width: 1_920,
            height: 1_080,
            radar_x_px: 960.0,
            radar_y_px: 540.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        };

        assert_eq!(
            viewport_sample_cache_storage_upper_bound(options),
            1_920 * 1_080 * std::mem::size_of::<CachedSample>()
                + 1_080 * std::mem::size_of::<CachedRowSpan>()
        );
    }

    #[test]
    fn grid_sample_cache_upper_bound_tracks_actual_radar_footprint() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let options = ViewportRasterOptions {
            width: 1_920,
            height: 1_080,
            radar_x_px: 960.0,
            radar_y_px: 540.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let full_viewport = viewport_sample_cache_storage_upper_bound(options);
        let radar_footprint = viewport_sample_cache_storage_upper_bound_for_grid(grid, options);

        assert!(radar_footprint < full_viewport);
        assert!(radar_footprint > 1_080 * std::mem::size_of::<CachedRowSpan>());
    }

    #[test]
    fn viewport_lookup_matches_reference_hypot_formula() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let row_lookup = AzimuthLookup::new(cut, grid);
        let max_range_m = max_range_m(grid).max(1.0);
        let max_range_km = max_range_m / 1000.0;
        let geometry = ViewportGeometry {
            width: 333,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            max_range_km_sq: max_range_km * max_range_km,
        };

        for (x, y) in [(0, 0), (166, 108), (180, 110), (220, 70), (332, 216)] {
            assert_eq!(
                viewport_lookup(x, y, grid, &row_lookup, geometry),
                viewport_lookup_reference(x, y, grid, &row_lookup, geometry)
            );
        }
    }

    #[test]
    fn viewport_lookup_table_matches_reference_hypot_formula() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let row_lookup = AzimuthLookup::new(cut, grid);
        let geometry = viewport_geometry(
            grid,
            ViewportRasterOptions {
                width: 333,
                height: 217,
                radar_x_px: 166.5,
                radar_y_px: 108.5,
                km_per_px_x: 0.5,
                km_per_px_y: 0.5,
            },
        );
        let lookup_table = ViewportLookupTable::new(grid, geometry);

        for y in [0, 10, 70, 108, 140, 216] {
            for x in [0, 20, 120, 166, 180, 260, 332] {
                let table_sample = lookup_table.row(y).and_then(|row| {
                    row.x_range
                        .contains(&x)
                        .then(|| row.lookup(x, &row_lookup))
                        .flatten()
                });
                assert_eq!(
                    table_sample,
                    viewport_lookup_reference(x, y, grid, &row_lookup, geometry),
                    "lookup mismatch at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn viewport_row_span_covers_reference_samples() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let row_lookup = AzimuthLookup::new(cut, grid);
        let max_range_m = max_range_m(grid).max(1.0);
        let max_range_km = max_range_m / 1000.0;
        let geometry = ViewportGeometry {
            width: 96,
            radar_x_px: 48.0,
            radar_y_px: 48.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            max_range_km_sq: max_range_km * max_range_km,
        };

        for y in 0..96 {
            let span = geometry.x_range_for_row(y);
            for x in 0..96 {
                if viewport_lookup_reference(x, y, grid, &row_lookup, geometry).is_some() {
                    assert!(
                        span.as_ref().is_some_and(|range| range.contains(&x)),
                        "row span missed reference sample at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn azimuth_lookup_fills_wider_native_radial_sectors() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 1,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );

        for index in 0..180 {
            cut.radials.push(Radial {
                azimuth_deg: index as f32 * 2.0,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            grid.push_u8_row_slice(index, &[20]).expect("radial row");
        }

        let lookup = AzimuthLookup::new(&cut, &grid);
        assert!(lookup.row_for_azimuth(1.0).is_some());
        assert!(lookup.row_for_azimuth(181.0).is_some());
    }

    #[test]
    fn azimuth_lookup_prefers_duplicate_row_with_longer_valid_extent() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 4,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        for azimuth_deg in [0.0, 0.0, 2.0, 4.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        grid.push_u8_row_slice(0, &[20, 0, 0, 0])
            .expect("short duplicate row");
        grid.push_u8_row_slice(1, &[20, 30, 40, 50])
            .expect("long duplicate row");
        grid.push_u8_row_slice(2, &[20, 30, 40, 50])
            .expect("neighbor row");
        grid.push_u8_row_slice(3, &[20, 30, 40, 50])
            .expect("neighbor row");

        let lookup = AzimuthLookup::new(&cut, &grid);
        assert_eq!(lookup.row_for_azimuth(0.0), Some(1));
        assert_eq!(row_valid_extent(&grid, 0), 1);
        assert_eq!(row_valid_extent(&grid, 1), 4);

        let sample = SampleLookup {
            azimuth_bin: azimuth_bin(0.0),
            gate: 3,
        };
        let MomentStorage::U8(values) = &grid.storage else {
            panic!("test grid should use u8 storage");
        };
        let resolved =
            resolve_compact_sample(values, &grid, &lookup, sample).expect("sample should resolve");
        assert_eq!(resolved.row, 1);
        assert_eq!(resolved.gate, 3);
    }

    #[test]
    fn compact_sample_resolution_keeps_visible_range_folded_candidates() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 4,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        cut.radials.push(Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: None,
            radial_status: None,
        });
        grid.push_u8_row_slice(0, &[1, 1, 1, 1])
            .expect("range-folded row");

        let lookup = AzimuthLookup::new(&cut, &grid);
        assert_eq!(row_valid_extent(&grid, 0), 4);

        let MomentStorage::U8(values) = &grid.storage else {
            panic!("test grid should use u8 storage");
        };
        let resolved = resolve_compact_sample(
            values,
            &grid,
            &lookup,
            SampleLookup {
                azimuth_bin: azimuth_bin(0.0),
                gate: 3,
            },
        )
        .expect("range-folded sample should resolve");

        assert_eq!(resolved.row, 0);
        assert_eq!(resolved.gate, 3);
    }

    #[test]
    fn viewport_render_uses_requested_screen_resolution() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let reflectivity =
            render_moment_viewport_image(&volume, 0, MomentType::Reflectivity, options)
                .expect("viewport reflectivity");
        assert_eq!(reflectivity.dimensions(), (333, 217));
        assert!(has_visible_pixel(reflectivity.as_raw()));

        let mut reusable_pixels = vec![255; viewport_rgba_buffer_len(options)];
        let dimensions = render_moment_viewport_rgba_into(
            &volume,
            0,
            MomentType::Reflectivity,
            options,
            &mut reusable_pixels,
        )
        .expect("viewport reflectivity into reusable buffer");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&reusable_pixels));
        assert!(has_transparent_pixel(&reusable_pixels));

        let reflectivity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport reflectivity cache");
        reusable_pixels.fill(255);
        let dimensions = reflectivity_cache
            .render_moment_rgba_into(&volume, options, &mut reusable_pixels)
            .expect("cached viewport reflectivity");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&reusable_pixels));
        assert!(has_transparent_pixel(&reusable_pixels));

        let storm_relative = render_storm_relative_velocity_viewport_image(
            &volume,
            0,
            StormMotion {
                direction_deg: 45.0,
                speed_mps: 10.0,
            },
            options,
        )
        .expect("viewport storm-relative velocity");
        assert_eq!(storm_relative.dimensions(), (333, 217));
        assert!(has_visible_pixel(storm_relative.as_raw()));

        let mut storm_relative_pixels = vec![255; viewport_rgba_buffer_len(options)];
        let dimensions = render_storm_relative_velocity_viewport_rgba_into(
            &volume,
            0,
            StormMotion {
                direction_deg: 45.0,
                speed_mps: 10.0,
            },
            options,
            &mut storm_relative_pixels,
        )
        .expect("viewport storm-relative velocity into reusable buffer");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&storm_relative_pixels));
        assert!(has_transparent_pixel(&storm_relative_pixels));

        let velocity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Velocity)
            .expect("viewport velocity cache");
        storm_relative_pixels.fill(255);
        let dimensions = velocity_cache
            .render_storm_relative_velocity_rgba_into(
                &volume,
                StormMotion {
                    direction_deg: 45.0,
                    speed_mps: 10.0,
                },
                options,
                &mut storm_relative_pixels,
            )
            .expect("cached viewport storm-relative velocity");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&storm_relative_pixels));
        assert!(has_transparent_pixel(&storm_relative_pixels));
    }

    #[test]
    fn viewport_sample_cache_matches_direct_moment_render() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport reflectivity cache");
        let sample_cache = cache
            .build_sample_cache(&volume, options)
            .expect("viewport sample cache");
        let mut direct_pixels = vec![0; viewport_rgba_buffer_len(options)];
        let mut sample_cache_pixels = vec![255; viewport_rgba_buffer_len(options)];

        cache
            .render_moment_rgba_into(&volume, options, &mut direct_pixels)
            .expect("direct viewport render");
        let dimensions = cache
            .render_moment_rgba_with_sample_cache(&volume, &sample_cache, &mut sample_cache_pixels)
            .expect("sample-cache viewport render");

        assert_eq!(dimensions, (333, 217));
        assert_eq!(sample_cache.dimensions(), (333, 217));
        assert!(sample_cache.sample_count() > 0);
        assert!(sample_cache.storage_bytes() < viewport_rgba_buffer_len(options));
        assert_eq!(sample_cache_pixels, direct_pixels);

        let mut reused_pixels = direct_pixels.clone();
        cache
            .render_moment_rgba_with_sample_cache_reusing_transparency(
                &volume,
                &sample_cache,
                &mut reused_pixels,
            )
            .expect("sample-cache reuse viewport render");
        assert_eq!(reused_pixels, sample_cache_pixels);
    }

    #[test]
    fn viewport_geometry_cache_resolves_across_compatible_products() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let reflectivity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        let velocity_cache =
            ViewportMomentCache::new(&volume, 0, MomentType::Velocity).expect("velocity cache");
        let geometry_cache = reflectivity_cache
            .build_geometry_cache(&volume, options)
            .expect("geometry cache");
        let geometry_sample_cache = velocity_cache
            .build_sample_cache_from_geometry_cache(&volume, &geometry_cache)
            .expect("velocity sample cache from geometry");
        let direct_sample_cache = velocity_cache
            .build_sample_cache(&volume, options)
            .expect("direct velocity sample cache");
        let mut geometry_pixels = vec![255; viewport_rgba_buffer_len(options)];
        let mut direct_pixels = vec![255; viewport_rgba_buffer_len(options)];

        velocity_cache
            .render_moment_rgba_with_sample_cache(
                &volume,
                &geometry_sample_cache,
                &mut geometry_pixels,
            )
            .expect("geometry-derived sample render");
        velocity_cache
            .render_moment_rgba_with_sample_cache(&volume, &direct_sample_cache, &mut direct_pixels)
            .expect("direct sample render");

        assert_eq!(geometry_cache.dimensions(), (333, 217));
        assert!(geometry_cache.sample_count() >= geometry_sample_cache.sample_count());
        assert_eq!(geometry_pixels, direct_pixels);
    }

    #[test]
    fn viewport_sample_cache_matches_direct_storm_relative_render() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let storm_motion = StormMotion {
            direction_deg: 45.0,
            speed_mps: 10.0,
        };
        let cache =
            ViewportMomentCache::new(&volume, 0, MomentType::Velocity).expect("velocity cache");
        let sample_cache = cache
            .build_sample_cache(&volume, options)
            .expect("velocity sample cache");
        let mut direct_pixels = vec![0; viewport_rgba_buffer_len(options)];
        let mut sample_cache_pixels = vec![255; viewport_rgba_buffer_len(options)];

        cache
            .render_storm_relative_velocity_rgba_into(
                &volume,
                storm_motion,
                options,
                &mut direct_pixels,
            )
            .expect("direct SRV viewport render");
        let dimensions = cache
            .render_storm_relative_velocity_rgba_with_sample_cache(
                &volume,
                storm_motion,
                &sample_cache,
                &mut sample_cache_pixels,
            )
            .expect("sample-cache SRV viewport render");

        assert_eq!(dimensions, (333, 217));
        assert_eq!(sample_cache_pixels, direct_pixels);

        let next_storm_motion = StormMotion {
            direction_deg: 220.0,
            speed_mps: 18.0,
        };
        let mut cleared_next_pixels = vec![255; viewport_rgba_buffer_len(options)];
        cache
            .render_storm_relative_velocity_rgba_with_sample_cache(
                &volume,
                next_storm_motion,
                &sample_cache,
                &mut cleared_next_pixels,
            )
            .expect("cleared next SRV viewport render");

        let mut reused_next_pixels = sample_cache_pixels;
        cache
            .render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
                &volume,
                next_storm_motion,
                &sample_cache,
                &mut reused_next_pixels,
            )
            .expect("reused next SRV viewport render");
        assert_eq!(reused_next_pixels, cleared_next_pixels);
    }

    #[test]
    fn viewport_sample_cache_rejects_mismatched_cache() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 64,
            height: 64,
            radar_x_px: 32.0,
            radar_y_px: 32.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let reflectivity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        let velocity_cache =
            ViewportMomentCache::new(&volume, 0, MomentType::Velocity).expect("velocity cache");
        let sample_cache = reflectivity_cache
            .build_sample_cache(&volume, options)
            .expect("reflectivity sample cache");
        let mut pixels = vec![0; viewport_rgba_buffer_len(options)];

        let err = velocity_cache
            .render_moment_rgba_with_sample_cache(&volume, &sample_cache, &mut pixels)
            .expect_err("sample cache should be moment-bound");

        assert!(matches!(
            err,
            RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: MomentType::Reflectivity
            }
        ));
    }

    #[test]
    fn viewport_render_rejects_wrong_sized_reusable_buffer() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let mut pixels = vec![0; viewport_rgba_buffer_len(options) - 4];
        let err = render_moment_viewport_rgba_into(
            &volume,
            0,
            MomentType::Reflectivity,
            options,
            &mut pixels,
        )
        .expect_err("wrong buffer size should be rejected");

        assert!(matches!(err, RenderError::BufferSizeMismatch { .. }));
    }

    #[test]
    fn viewport_cache_rejects_different_volume() {
        let volume = test_volume();
        let other_volume = test_volume();
        let options = ViewportRasterOptions {
            width: 64,
            height: 64,
            radar_x_px: 32.0,
            radar_y_px: 32.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport reflectivity cache");
        let mut pixels = vec![0; viewport_rgba_buffer_len(options)];

        let err = cache
            .render_moment_rgba_into(&other_volume, options, &mut pixels)
            .expect_err("cache should be bound to its source volume");

        assert!(matches!(err, RenderError::CacheVolumeMismatch));
    }

    #[test]
    fn viewport_cache_renders_u16_palette_moments() {
        let volume = test_u16_volume();
        let options = ViewportRasterOptions {
            width: 96,
            height: 96,
            radar_x_px: 48.0,
            radar_y_px: 48.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport u16 reflectivity cache");
        let mut pixels = vec![255; viewport_rgba_buffer_len(options)];

        let dimensions = cache
            .render_moment_rgba_into(&volume, options, &mut pixels)
            .expect("cached u16 viewport reflectivity");

        assert_eq!(dimensions, (96, 96));
        assert!(has_visible_pixel(&pixels));
        assert!(has_transparent_pixel(&pixels));
    }

    fn has_visible_pixel(pixels: &[u8]) -> bool {
        pixels.chunks_exact(4).any(|pixel| pixel[3] != 0)
    }

    fn has_transparent_pixel(pixels: &[u8]) -> bool {
        pixels.chunks_exact(4).any(|pixel| pixel[3] == 0)
    }

    fn viewport_lookup_reference(
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
        geometry: ViewportGeometry,
    ) -> Option<SampleLookup> {
        let dx_km = (x as f32 + 0.5 - geometry.radar_x_px) * geometry.km_per_px_x;
        let dy_km = (geometry.radar_y_px - (y as f32 + 0.5)) * geometry.km_per_px_y;
        let range_m = dx_km.hypot(dy_km) * 1000.0;
        let max_range_m = geometry.max_range_km_sq.sqrt() * 1000.0;
        if range_m > max_range_m {
            return None;
        }

        let gate = ((range_m - grid.gate_range.first_gate_m as f32)
            / grid.gate_range.gate_spacing_m.max(1) as f32)
            .round() as isize;
        if gate < 0 || gate as usize >= grid.gate_range.gate_count {
            return None;
        }

        let azimuth_deg = azimuth_from_xy(dx_km, dy_km);
        let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
        Some(SampleLookup {
            azimuth_bin,
            gate: gate as usize,
        })
    }

    fn test_velocity_grid_rows(rows: Vec<Vec<f32>>) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: rows.first().map(Vec::len).unwrap_or(0),
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for index in 0..rows.len() {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(10.0),
                radial_status: None,
            });
        }
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..cut.radials.len()).collect(),
            storage: MomentStorage::F32(rows.into_iter().flatten().collect()),
        };
        (cut, grid)
    }

    fn test_volume() -> RadarVolume {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 6,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for azimuth_deg in [0.0, 90.0, 180.0, 270.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(32.0),
                radial_status: None,
            });
        }

        let mut reflectivity = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        let mut velocity = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        for radial_index in 0..4 {
            reflectivity
                .push_u8_row_slice(radial_index, &[20, 30, 40, 50, 60, 70])
                .expect("reflectivity row");
            velocity
                .push_u8_row_slice(radial_index, &[44, 54, 64, 74, 84, 94])
                .expect("velocity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);
        cut.moments.insert(MomentType::Velocity, velocity);

        let mut volume = RadarVolume::new(RadarSite::new("TST"), chrono::Utc::now());
        volume.cuts.push(cut);
        volume
    }

    fn test_u16_volume() -> RadarVolume {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 6,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for azimuth_deg in [0.0, 90.0, 180.0, 270.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }

        let mut reflectivity = MomentGrid::new_u16(
            MomentType::Reflectivity,
            gate_range,
            2.0,
            64.0,
            Some(0),
            Some(1),
        );
        for radial_index in 0..4 {
            reflectivity
                .push_row(
                    radial_index,
                    MomentRow::U16(vec![80, 100, 120, 140, 160, 180]),
                )
                .expect("u16 reflectivity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);

        let mut volume = RadarVolume::new(RadarSite::new("U16"), chrono::Utc::now());
        volume.cuts.push(cut);
        volume
    }
}
