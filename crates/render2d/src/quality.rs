//! Supersampled rasterising and box downsampling for the viewport path.
//!
//! The base viewport rasteriser takes exactly one nearest-neighbour sample per
//! screen pixel (`ViewportLookupRow::lookup`). That is a point sample of a
//! signal whose bandwidth has nothing to do with the screen grid: zoomed out a
//! single pixel spans several gates radially and dozens of radials
//! azimuthally, so the one sample that survives is essentially arbitrary and
//! the sweep breaks up into speckle and dashed radials. Supersampling —
//! rasterising at an integer multiple of the display grid and box-filtering
//! back down — is the standard cure (Crow 1981, "A Comparison of Antialiasing
//! Techniques", IEEE CG&A 1(1) 40-48, doi:10.1109/MCG.1981.167381).
//!
//! The filter averages in PREMULTIPLIED (Porter & Duff's "associated") alpha.
//! Averaging straight RGBA is the classic wrong answer: the RGB of a fully
//! transparent pixel is meaningless, and in this rasteriser it is black, so a
//! straight average drags every echo edge toward black and rings each storm
//! with a dark fringe. See Porter & Duff 1984, "Compositing Digital Images",
//! SIGGRAPH '84, Computer Graphics 18(3) 253-259, doi:10.1145/800031.808606,
//! and Blinn 1994, "Compositing, Part 1: Theory", IEEE CG&A 14(5) 83-87,
//! doi:10.1109/38.310740.
//!
//! REGISTRATION. `ViewportRasterOptions::radar_x_px` is a pixel-CORNER
//! coordinate: the rasteriser samples pixel `x` at ground offset
//! `(x + 0.5 - radar_x_px) * km_per_px_x`, so the radar itself sits on the
//! boundary between pixels `radar_x_px - 1` and `radar_x_px`. Scaling that
//! coordinate by the plain factor `s` therefore maps base pixel `i` onto
//! exactly the supersampled block `[s·i, s·i + s)`, and the box filter over
//! that block covers exactly the ground the base pixel covered. Mixing in a
//! pixel-CENTRE convention here — `(radar_x_px + 0.5)·s - 0.5` — would shift
//! the image by half a base pixel at every factor. The test
//! `real_volume_subsample_planes_land_where_a_plain_render_would_sample` pins
//! the correct convention against sixteen from-scratch renders of a live
//! sweep, and shows the centre convention failing the same comparison.
//!
//! [`ViewportRasterOptions`] lives in the crate root, so the scaling is offered
//! as free functions here rather than as an inherent method.

use radar_core::RadarVolume;
use rayon::prelude::*;

use crate::{
    RenderError, Result, ViewportMomentCache, ViewportRasterOptions, rgba_len, viewport_dimensions,
};

/// Hard ceiling on the supersampled RGBA scratch buffer: a single-allocation
/// safety cap, 64 MiB, so a quality render can never ask the allocator for an
/// unbounded frame. Total loop memory is deliberately not capped here (the
/// caller's cache budget owns that).
///
/// This is a BYTE budget rather than BowEcho's per-dimension ceiling. BowEcho
/// caps each dimension at 4096 because its supersampled raster is uploaded as
/// a wgpu texture and resampled by the hardware; nothing here is a texture —
/// the raster is CPU scratch that is box-filtered down to the display size and
/// then dropped, so the only real constraint is the size of that one
/// allocation. Keeping the dimension form would have been actively harmful:
/// combined with the integer floor this path needs (see
/// [`effective_supersample_factor`]), `4096 / 2560 = 1` means every viewport
/// wider than 2048 px silently gets no supersampling at all, which is most of
/// a maximised pane on a 1440p or 4K display. The byte budget admits
/// 2560x1440 at 2x (56.25 MiB) while leaving the worst-case allocation
/// identical.
///
/// The trade is that a very lopsided viewport can now produce a supersampled
/// dimension past 4096 (a 16000x3 sliver at 8x is 128000x24, still only
/// 12 MB). Nothing in this module cares, but a caller that renders into
/// [`supersampled`] options itself and uploads THAT raster as a GPU texture —
/// the shape BowEcho's cache has — would have to re-impose a dimension limit.
pub const MAX_SUPERSAMPLED_RGBA_BYTES: usize = 64 << 20;

/// The supersample factor actually used for `options`, after the
/// [`MAX_SUPERSAMPLED_RGBA_BYTES`] budget clamps the request DOWN. Never zero.
///
/// The clamp is integral, unlike BowEcho's GPU path which can afford a
/// fractional `cap_scale` because the hardware sampler resamples the texture at
/// draw time. Here the reduction is a CPU box filter, so the supersampled
/// raster has to be an exact integer multiple of the display raster or blocks
/// stop lining up with output pixels; flooring the cap is what buys that.
///
/// A base raster that already exceeds the budget is left at 1x rather than
/// being scaled down — the supersample only ever adds detail.
///
/// Callers that show the user a quality setting should display THIS value, not
/// the requested one: on a large pane the budget can silently turn a requested
/// 4x into 2x or 1x.
pub fn effective_supersample_factor(options: ViewportRasterOptions, factor: u32) -> u32 {
    if factor <= 1 {
        return 1;
    }
    let (width, height) = viewport_dimensions(options);
    // `viewport_dimensions` floors both axes at 1, so this is at least 4 and
    // the division below cannot divide by zero.
    let base_bytes = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    // The raster grows as s², so the largest affordable scale is the integer
    // square root of the budget expressed in base-frames. `saturating_mul`
    // above turns a pathologically large viewport into `base_bytes` far past
    // the budget, which floors this at 1 — never a downscale.
    let budget_scale = (MAX_SUPERSAMPLED_RGBA_BYTES / base_bytes).isqrt().max(1);
    factor.min(budget_scale as u32)
}

/// The render size for a quality factor, ground coverage preserved.
///
/// More raster pixels over the SAME map rect, so the polar data is sampled
/// finer (genuine detail, not upscaling). Width and height scale up by the
/// effective factor `s` while `km_per_px` scales down by `s`, holding
/// `dimension · km_per_px` — the ground the raster covers — invariant. The
/// radar's pixel position scales by `s` too, so a world point at ground offset
/// `d` km lands at `s · (radar_px + d / km_per_px)`: the same fractional
/// position on the display grid, at every factor.
///
/// `factor <= 1`, and any factor the budget clamps back to 1, return `options`
/// untouched (bit-identical to Standard).
pub fn supersampled(options: ViewportRasterOptions, factor: u32) -> ViewportRasterOptions {
    let scale = effective_supersample_factor(options, factor);
    if scale <= 1 {
        return options;
    }
    // Scale the clamped dimensions, not the raw fields: a degenerate 0-wide
    // request renders as 1 px either way, and multiplying the raw 0 would leave
    // the supersampled raster covering different ground than the base.
    let (width, height) = viewport_dimensions(options);
    let scale_f = scale as f32;
    ViewportRasterOptions {
        width: width.saturating_mul(scale),
        height: height.saturating_mul(scale),
        radar_x_px: options.radar_x_px * scale_f,
        radar_y_px: options.radar_y_px * scale_f,
        km_per_px_x: options.km_per_px_x / scale_f,
        km_per_px_y: options.km_per_px_y / scale_f,
        rotation_rad: options.rotation_rad,
    }
}

/// Dimensions of the supersampled raster that `factor` renders at.
pub fn supersampled_dimensions(options: ViewportRasterOptions, factor: u32) -> (u32, u32) {
    viewport_dimensions(supersampled(options, factor))
}

/// RGBA byte length of the scratch buffer the supersampled render needs.
/// Never exceeds [`MAX_SUPERSAMPLED_RGBA_BYTES`] unless the BASE viewport
/// already did, in which case the factor has been clamped to 1 and this is
/// just the base frame.
pub fn supersampled_rgba_buffer_len(options: ViewportRasterOptions, factor: u32) -> usize {
    let (width, height) = supersampled_dimensions(options, factor);
    rgba_len(width, height)
}

/// Dimensions of the buffer [`render_moment_viewport_quality_rgba_into`]
/// writes. Derived by running the same two steps the render runs — scale up,
/// then divide by the same effective factor — so the size a caller allocates
/// cannot drift from the size the box filter produces. It always equals the
/// base viewport dimensions: quality buys sampling density, never a bigger
/// picture.
pub fn quality_output_dimensions(options: ViewportRasterOptions, factor: u32) -> (u32, u32) {
    let scale = effective_supersample_factor(options, factor);
    let (src_width, src_height) = supersampled_dimensions(options, factor);
    (src_width.div_ceil(scale), src_height.div_ceil(scale))
}

/// RGBA byte length of the output buffer for a given `(options, factor)`.
pub fn quality_rgba_buffer_len(options: ViewportRasterOptions, factor: u32) -> usize {
    let (width, height) = quality_output_dimensions(options, factor);
    rgba_len(width, height)
}

/// Box-filter an RGBA buffer down by an integer factor.
///
/// Returns the destination dimensions, `ceil(src / factor)` on each axis. When
/// the source dimensions are not a multiple of `factor` the trailing block row
/// and column are partial; those blocks average only the pixels that exist.
///
/// Channels are averaged in premultiplied alpha and un-premultiplied on the way
/// out, so a transparent pixel contributes its coverage but not its (black)
/// colour. `factor <= 1` is an exact byte-for-byte copy — including the RGB of
/// fully transparent pixels, which the filter would otherwise be free to zero.
///
/// Every pixel of the returned frame is written, at every factor: a source too
/// short for its declared dimensions costs coverage, never a stale pixel left
/// over from whatever `dst` held before.
///
/// # Panics
/// If `dst` is shorter than `rgba_len` of the returned dimensions, or if those
/// dimensions do not fit in a `usize`. Silently filtering into an undersized
/// buffer would hand the caller a torn frame.
pub fn downsample_rgba(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    factor: u32,
    dst: &mut [u8],
) -> (u32, u32) {
    if factor <= 1 {
        let full = checked_rgba_len(src_width, src_height);
        assert!(
            dst.len() >= full,
            "downsample_rgba destination has {} bytes, needs {full} for {src_width}x{src_height}",
            dst.len()
        );
        let copied = full.min(src.len());
        dst[..copied].copy_from_slice(&src[..copied]);
        // A short source leaves a hole, not the caller's previous frame.
        dst[copied..full].fill(0);
        return (src_width, src_height);
    }

    let dst_width = src_width.div_ceil(factor);
    let dst_height = src_height.div_ceil(factor);
    let needed = checked_rgba_len(dst_width, dst_height);
    assert!(
        dst.len() >= needed,
        "downsample_rgba destination has {} bytes, needs {needed} for {dst_width}x{dst_height}",
        dst.len()
    );
    if needed == 0 {
        return (dst_width, dst_height);
    }

    let src_stride = src_width as usize * 4;
    let dst_stride = dst_width as usize * 4;
    // The block arithmetic below stays in `u32` deliberately: `dst_y · factor`
    // is bounded by `src_height` and the assert above has already rejected any
    // frame big enough for that to wrap, so the hot loop pays for no extra
    // checks.
    dst[..needed]
        .par_chunks_exact_mut(dst_stride)
        .enumerate()
        .for_each(|(dst_y, dst_row)| {
            let y_start = dst_y as u32 * factor;
            let y_end = (y_start + factor).min(src_height);
            for (dst_x, out) in dst_row.chunks_exact_mut(4).enumerate() {
                let x_start = dst_x as u32 * factor;
                let x_end = (x_start + factor).min(src_width);

                // Premultiplied sums: colour weighted by its own coverage.
                let mut sum_red = 0u64;
                let mut sum_green = 0u64;
                let mut sum_blue = 0u64;
                let mut sum_alpha = 0u64;
                let mut present = 0u64;
                for src_y in y_start..y_end {
                    let row_base = src_y as usize * src_stride;
                    for src_x in x_start..x_end {
                        let offset = row_base + src_x as usize * 4;
                        // Defensive: a short `src` costs coverage, never a panic.
                        let Some(pixel) = src.get(offset..offset + 4) else {
                            continue;
                        };
                        let alpha = u64::from(pixel[3]);
                        sum_red += u64::from(pixel[0]) * alpha;
                        sum_green += u64::from(pixel[1]) * alpha;
                        sum_blue += u64::from(pixel[2]) * alpha;
                        sum_alpha += alpha;
                        present += 1;
                    }
                }

                if present == 0 || sum_alpha == 0 {
                    out.copy_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                // Un-premultiply: mean(c·a)/mean(a) == sum(c·a)/sum(a).
                // Adding half the divisor rounds to nearest instead of down.
                out[0] = ((sum_red + sum_alpha / 2) / sum_alpha) as u8;
                out[1] = ((sum_green + sum_alpha / 2) / sum_alpha) as u8;
                out[2] = ((sum_blue + sum_alpha / 2) / sum_alpha) as u8;
                out[3] = ((sum_alpha + present / 2) / present) as u8;
            }
        });

    (dst_width, dst_height)
}

/// `rgba_len` that refuses to wrap. The crate-internal helper multiplies in
/// `usize` unchecked, which is fine for viewport-derived dimensions but not for
/// the arbitrary `u32`s [`downsample_rgba`] accepts from outside the crate: a
/// wrapped length would pass the buffer assert and then report a frame size
/// nothing was written to.
fn checked_rgba_len(width: u32, height: u32) -> usize {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .expect("RGBA frame size overflows usize")
}

/// The whole path in one call, so a caller cannot get the two out of step.
///
/// `rgba` must be [`quality_rgba_buffer_len`] bytes. At an effective factor of
/// 1 this delegates straight to the plain viewport render — no scratch buffer,
/// no filter, byte-identical output.
pub fn render_moment_viewport_quality_rgba_into(
    cache: &ViewportMomentCache,
    volume: &RadarVolume,
    options: ViewportRasterOptions,
    factor: u32,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    let mut scratch = Vec::new();
    render_moment_viewport_quality_rgba_into_with_scratch(
        cache,
        volume,
        options,
        factor,
        &mut scratch,
        rgba,
    )
}

/// As [`render_moment_viewport_quality_rgba_into`], but reusing a caller-owned
/// scratch buffer for the supersampled raster. At the budget ceiling that
/// scratch is 64 MiB, so a loop that renders frame after frame wants to hold
/// onto it rather than allocate and drop one per frame.
pub fn render_moment_viewport_quality_rgba_into_with_scratch(
    cache: &ViewportMomentCache,
    volume: &RadarVolume,
    options: ViewportRasterOptions,
    factor: u32,
    scratch: &mut Vec<u8>,
    rgba: &mut [u8],
) -> Result<(u32, u32)> {
    let (out_width, out_height) = quality_output_dimensions(options, factor);
    let expected = rgba_len(out_width, out_height);
    if rgba.len() != expected {
        return Err(RenderError::BufferSizeMismatch {
            actual: rgba.len(),
            expected,
            width: out_width,
            height: out_height,
        });
    }

    let scale = effective_supersample_factor(options, factor);
    if scale <= 1 {
        return cache.render_moment_rgba_into(volume, options, rgba);
    }

    let high = supersampled(options, factor);
    let (src_width, src_height) = viewport_dimensions(high);
    let needed = rgba_len(src_width, src_height);
    // Grow only. The renderer clears every row it owns (`clear_pixels: true`
    // fills each row before the early-out for rows outside the sweep), so
    // re-zeroing a reused scratch would just be a wasted 64 MiB memset.
    if scratch.len() < needed {
        scratch.resize(needed, 0);
    }
    cache.render_moment_rgba_into(volume, high, &mut scratch[..needed])?;
    Ok(downsample_rgba(
        &scratch[..needed],
        src_width,
        src_height,
        scale,
        rgba,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{render_moment_viewport_rgba_into, viewport_rgba_buffer_len};
    use radar_core::{
        ElevationCut, GateRange, MomentGrid, MomentRow, MomentStorage, MomentType, RadarSite,
        RadarVolume, Radial,
    };

    fn sample_options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 800,
            height: 600,
            radar_x_px: 400.0,
            radar_y_px: 300.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            rotation_rad: 0.0,
        }
    }

    /// Screen position of a world point `d_km` from the radar, in raster pixels.
    fn screen_x_for_offset_km(options: ViewportRasterOptions, d_km: f32) -> f32 {
        options.radar_x_px + d_km / options.km_per_px_x
    }

    fn screen_y_for_offset_km(options: ViewportRasterOptions, d_km: f32) -> f32 {
        options.radar_y_px + d_km / options.km_per_px_y
    }

    #[test]
    fn supersample_factor_zero_and_one_are_identities() {
        let base = sample_options();
        assert_eq!(supersampled(base, 0), base);
        assert_eq!(supersampled(base, 1), base);
        assert_eq!(effective_supersample_factor(base, 0), 1);
        assert_eq!(effective_supersample_factor(base, 1), 1);
    }

    #[test]
    fn supersample_scales_every_geometry_field_by_the_factor() {
        // Hand-computed: 800x600 at 0.5 km/px, radar at (400, 300), factor 3.
        let high = supersampled(sample_options(), 3);
        assert_eq!(high.width, 2400);
        assert_eq!(high.height, 1800);
        assert_eq!(high.radar_x_px, 1200.0);
        assert_eq!(high.radar_y_px, 900.0);
        assert!((high.km_per_px_x - 0.5 / 3.0).abs() < 1e-9);
        assert!((high.km_per_px_y - 0.5 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn supersample_preserves_ground_coverage_at_every_factor() {
        let base = sample_options();
        // 800 px * 0.5 km/px = 400 km across, 600 * 0.5 = 300 km down.
        let base_ground_x = base.width as f32 * base.km_per_px_x;
        let base_ground_y = base.height as f32 * base.km_per_px_y;
        assert_eq!(base_ground_x, 400.0);
        assert_eq!(base_ground_y, 300.0);

        for factor in 1..=5 {
            let high = supersampled(base, factor);
            let scale = effective_supersample_factor(base, factor);
            assert_eq!(scale, factor, "no clamping expected at this base size");

            let ground_x = high.width as f32 * high.km_per_px_x;
            let ground_y = high.height as f32 * high.km_per_px_y;
            assert!(
                (ground_x - base_ground_x).abs() < 1e-2,
                "factor {factor}: {ground_x} km across vs {base_ground_x}"
            );
            assert!(
                (ground_y - base_ground_y).abs() < 1e-2,
                "factor {factor}: {ground_y} km down vs {base_ground_y}"
            );

            // The load-bearing invariant: the same world point lands on the
            // same FRACTIONAL screen position once the factor is divided out.
            // A point 137.5 km east and 42.25 km south of the radar sits at
            // 400 + 137.5/0.5 = 675.0 px, 300 + 42.25/0.5 = 384.5 px.
            assert_eq!(screen_x_for_offset_km(base, 137.5), 675.0);
            assert_eq!(screen_y_for_offset_km(base, 42.25), 384.5);
            let high_x = screen_x_for_offset_km(high, 137.5) / scale as f32;
            let high_y = screen_y_for_offset_km(high, 42.25) / scale as f32;
            assert!(
                (high_x - 675.0).abs() < 1e-3,
                "factor {factor}: x {high_x} vs 675.0"
            );
            assert!(
                (high_y - 384.5).abs() < 1e-3,
                "factor {factor}: y {high_y} vs 384.5"
            );
        }
    }

    /// Non-square, radar well off centre, different km/px on the two axes —
    /// the shape every registration bug hides behind.
    fn skewed_options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 173,
            height: 91,
            radar_x_px: 41.25,
            radar_y_px: 66.75,
            km_per_px_x: 0.37,
            km_per_px_y: 0.21,
            rotation_rad: 0.0,
        }
    }

    /// Ground offset of the CENTRE of raster column `x`, reproducing
    /// `ViewportLookupRow::lookup`: `dx_km = (x + 0.5 - radar_x_px) * km_per_px_x`.
    fn sample_dx_km(options: ViewportRasterOptions, x: u32) -> f32 {
        (x as f32 + 0.5 - options.radar_x_px) * options.km_per_px_x
    }

    /// Ground offset of the CENTRE of raster row `y`, reproducing
    /// `ViewportLookupTable::row`: `dy_km = (radar_y_px - (y + 0.5)) * km_per_px_y`.
    fn sample_dy_km(options: ViewportRasterOptions, y: u32) -> f32 {
        (options.radar_y_px - (y as f32 + 0.5)) * options.km_per_px_y
    }

    #[test]
    fn subsamples_of_a_block_straddle_the_base_pixel_symmetrically() {
        // Registration in one arithmetic statement. Base pixel `i` is sampled
        // at ground offset (i + 0.5 - radar_px)·km_per_px; the `s` supersampled
        // pixels of its block are sampled at (i + (k + 0.5)/s - radar_px)·
        // km_per_px for k = 0..s. Those are symmetric about the base offset, so
        // their mean IS the base offset and the block covers exactly the ground
        // the base pixel covered. A pixel-centre scaling of radar_px biases the
        // mean by half a base pixel, which is what this catches.
        let base = skewed_options();
        for factor in 2..=5u32 {
            let high = supersampled(base, factor);
            assert_eq!(effective_supersample_factor(base, factor), factor);

            for x in [0u32, 1, 40, 41, 86, 172] {
                let want = sample_dx_km(base, x);
                let mean = (0..factor)
                    .map(|k| sample_dx_km(high, x * factor + k))
                    .sum::<f32>()
                    / factor as f32;
                // Express the error in BASE pixels; 1e-4 px is far below the
                // 0.5 px a convention mix-up would produce.
                let error_px = (mean - want).abs() / base.km_per_px_x;
                assert!(error_px < 1e-4, "factor {factor} x {x}: {error_px} px off");
            }
            for y in [0u32, 1, 66, 67, 45, 90] {
                let want = sample_dy_km(base, y);
                let mean = (0..factor)
                    .map(|l| sample_dy_km(high, y * factor + l))
                    .sum::<f32>()
                    / factor as f32;
                let error_px = (mean - want).abs() / base.km_per_px_y;
                assert!(error_px < 1e-4, "factor {factor} y {y}: {error_px} px off");
            }
        }
    }

    #[test]
    fn the_byte_budget_clamps_the_factor_instead_of_allocating_a_giant_raster() {
        // 1500x1000 is 6 MB per frame, so 3x (54 MB) fits the 64 MiB budget
        // and 4x (96 MB) does not.
        let base = ViewportRasterOptions {
            width: 1500,
            height: 1000,
            radar_x_px: 700.0,
            radar_y_px: 500.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
            rotation_rad: 0.0,
        };
        assert_eq!(effective_supersample_factor(base, 4), 3);
        let high = supersampled(base, 4);
        assert_eq!((high.width, high.height), (4500, 3000));
        assert!(supersampled_rgba_buffer_len(base, 4) <= MAX_SUPERSAMPLED_RGBA_BYTES);
        // Clamping does not disturb ground coverage.
        assert_eq!(high.width as f32 * high.km_per_px_x, 1500.0);
        assert_eq!(high.height as f32 * high.km_per_px_y, 1000.0);
    }

    #[test]
    fn realistic_pane_sizes_still_get_a_supersample() {
        // The regression this budget exists to prevent. Under a 4096-per-
        // dimension ceiling every one of these except 1920-wide clamps to 1 and
        // the quality setting silently does nothing. Hand-computed frame sizes
        // against the 67,108,864-byte budget:
        //   900x700   = 2.52 MB -> 4x = 40.3 MB   (fits)
        //   1920x1080 = 8.29 MB -> 2x = 33.2 MB   (fits), 3x = 74.6 MB (does not)
        //   2560x1440 = 14.7 MB -> 2x = 59.0 MB   (fits), 3x = 132.7 MB (does not)
        //   3840x2160 = 33.2 MB -> 2x = 132.7 MB  (does not)
        for (width, height, requested, want) in [
            (900u32, 700u32, 4u32, 4u32),
            (1920, 1080, 4, 2),
            (2560, 1440, 4, 2),
            (2560, 1440, 2, 2),
            (3840, 2160, 4, 1),
        ] {
            let options = ViewportRasterOptions {
                width,
                height,
                radar_x_px: width as f32 / 2.0,
                radar_y_px: height as f32 / 2.0,
                km_per_px_x: 0.5,
                km_per_px_y: 0.5,
                rotation_rad: 0.0,
            };
            assert_eq!(
                effective_supersample_factor(options, requested),
                want,
                "{width}x{height} at {requested}x"
            );
            assert!(
                supersampled_rgba_buffer_len(options, requested)
                    <= MAX_SUPERSAMPLED_RGBA_BYTES.max(viewport_rgba_buffer_len(options)),
                "{width}x{height} at {requested}x overruns the scratch budget"
            );
        }
    }

    #[test]
    fn an_oversized_base_is_never_downscaled() {
        // 5000x3000 is 60 MB on its own; 2x would be 240 MB.
        let base = ViewportRasterOptions {
            width: 5000,
            height: 3000,
            radar_x_px: 10.0,
            radar_y_px: 20.0,
            km_per_px_x: 0.25,
            km_per_px_y: 0.25,
            rotation_rad: 0.0,
        };
        assert_eq!(effective_supersample_factor(base, 4), 1);
        assert_eq!(supersampled(base, 4), base);

        // And a viewport so large its own frame overflows the budget maths.
        let absurd = ViewportRasterOptions {
            width: u32::MAX,
            height: u32::MAX,
            ..base
        };
        assert_eq!(effective_supersample_factor(absurd, 4), 1);
        assert_eq!(supersampled(absurd, 4), absurd);
    }

    #[test]
    fn no_viewport_size_pushes_the_scratch_past_the_budget() {
        // Sweep shapes the budget maths could get wrong: square, extreme
        // aspect ratios, off-by-one around the exact-fit sizes, degenerates.
        for (width, height) in [
            (0u32, 0u32),
            (1, 1),
            (1, 4096),
            (4096, 1),
            (16_000, 3),
            (3, 16_000),
            (2047, 2047),
            (2048, 2048),
            (2049, 2049),
            (4096, 4096),
            (4097, 4097),
            (3840, 2160),
            (7680, 4320),
        ] {
            let options = ViewportRasterOptions {
                width,
                height,
                radar_x_px: 1.5,
                radar_y_px: 2.5,
                km_per_px_x: 0.4,
                km_per_px_y: 0.4,
                rotation_rad: 0.0,
            };
            for factor in [0u32, 1, 2, 3, 4, 8, u32::MAX] {
                let scale = effective_supersample_factor(options, factor);
                assert!(scale >= 1, "{width}x{height} at {factor}x gave scale 0");
                assert!(scale <= factor.max(1), "{width}x{height} at {factor}x");

                let scratch = supersampled_rgba_buffer_len(options, factor);
                let base = viewport_rgba_buffer_len(options);
                assert!(
                    scratch <= MAX_SUPERSAMPLED_RGBA_BYTES || scratch == base,
                    "{width}x{height} at {factor}x wants {scratch} bytes of scratch"
                );
                // The output frame never grows, whatever the factor does.
                assert_eq!(
                    quality_output_dimensions(options, factor),
                    viewport_dimensions(options),
                    "{width}x{height} at {factor}x changed the output size"
                );
                assert_eq!(quality_rgba_buffer_len(options, factor), base);
            }
        }
    }

    #[test]
    fn downsample_factor_zero_and_one_copy_bytes_exactly() {
        // Includes RGB under a zero alpha, which the averaging path is free to
        // discard but the identity path must not touch.
        let src: Vec<u8> = (0u8..=63).collect();
        for factor in [0u32, 1] {
            let mut dst = vec![0xAAu8; src.len()];
            assert_eq!(downsample_rgba(&src, 4, 4, factor, &mut dst), (4, 4));
            assert_eq!(dst, src, "factor {factor} must be a byte copy");
        }
    }

    #[test]
    fn downsample_factor_one_blanks_what_a_short_source_cannot_supply() {
        // Two of the four declared pixels exist. The rest must come back
        // transparent rather than as whatever the destination held before,
        // matching what the averaging path does for a missing block.
        let src: Vec<u8> = (0u8..8).collect();
        let mut dst = vec![0xAAu8; 16];
        assert_eq!(downsample_rgba(&src, 2, 2, 1, &mut dst), (2, 2));
        assert_eq!(&dst[..8], &src[..]);
        assert_eq!(&dst[8..], &[0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn downsample_averages_in_premultiplied_alpha() {
        // One opaque red pixel beside three fully transparent BLACK ones.
        // Premultiplied: alpha = 255/4 = 63.75 -> 64; colour = 255*255/255 =
        // 255, i.e. still pure red, just a quarter covered.
        // Straight-average (the bug) would give (64, 0, 0, 64): dark red.
        let src = [
            255, 0, 0, 255, // opaque red
            0, 0, 0, 0, // transparent black
            0, 0, 0, 0, // transparent black
            0, 0, 0, 0, // transparent black
        ];
        let mut dst = [0u8; 4];
        assert_eq!(downsample_rgba(&src, 2, 2, 2, &mut dst), (1, 1));
        assert_eq!(dst, [255, 0, 0, 64]);
        assert_ne!(dst, [64, 0, 0, 64], "straight-alpha average leaks black");
    }

    #[test]
    fn downsample_at_the_real_factor_four_keeps_saturated_colour_beside_nothing() {
        // The shape an echo edge actually has at 4x: a 4x4 block with three
        // opaque saturated pixels and thirteen transparent black ones.
        // Hand-computed premultiplied result:
        //   sum_alpha = 3*255 = 765, present = 16
        //   red   = (255*765 + 765/2) / 765 = (195075 + 382) / 765 = 255
        //   green = (128*765 + 382) / 765   = (97920 + 382) / 765   = 128
        //   alpha = (765 + 8) / 16 = 48   (exact mean 47.8125)
        // Straight alpha would give red 3*255/16 = 47, green 24 — a black
        // smear where the colour should be untouched.
        let mut src = vec![0u8; 16 * 4];
        for pixel in 0..3 {
            src[pixel * 4..pixel * 4 + 4].copy_from_slice(&[255, 128, 0, 255]);
        }
        let mut dst = [0u8; 4];
        assert_eq!(downsample_rgba(&src, 4, 4, 4, &mut dst), (1, 1));
        assert_eq!(dst, [255, 128, 0, 48]);
    }

    #[test]
    fn downsample_weights_unequal_alphas_by_their_own_coverage() {
        // Opaque white next to a half-covered black and two empties.
        //   sum_alpha = 255 + 128 = 383, present = 4
        //   red = (255*255 + 0*128 + 383/2) / 383 = (65025 + 191) / 383
        //       = 65216 / 383 = 170  (exact 170.28)
        //   alpha = (383 + 2) / 4 = 96  (exact 95.75)
        // A straight average would put red at (255 + 0 + 0 + 0)/4 = 63.
        let src = [
            255, 255, 255, 255, //
            0, 0, 0, 128, //
            0, 0, 0, 0, //
            0, 0, 0, 0, //
        ];
        let mut dst = [0u8; 4];
        downsample_rgba(&src, 2, 2, 2, &mut dst);
        assert_eq!(dst, [170, 170, 170, 96]);
    }

    #[test]
    fn downsample_of_partially_transparent_neighbours_keeps_the_hue() {
        // Two half-covered blues and two empties: coverage halves again but the
        // hue is untouched. alpha = (128 + 128) / 4 = 64; blue channel =
        // (200*128 + 200*128) / (128 + 128) = 200.
        let src = [
            0, 0, 200, 128, //
            0, 0, 200, 128, //
            0, 0, 0, 0, //
            0, 0, 0, 0, //
        ];
        let mut dst = [0u8; 4];
        downsample_rgba(&src, 2, 2, 2, &mut dst);
        assert_eq!(dst, [0, 0, 200, 64]);
    }

    #[test]
    fn downsample_of_two_opaque_colours_is_their_plain_mean() {
        // With every source pixel opaque the premultiplied filter must reduce
        // to the ordinary box average: (240 + 0 + 0 + 0) / 4 = 60.
        let src = [
            240, 100, 20, 255, //
            0, 100, 20, 255, //
            0, 100, 20, 255, //
            0, 100, 20, 255, //
        ];
        let mut dst = [0u8; 4];
        downsample_rgba(&src, 2, 2, 2, &mut dst);
        assert_eq!(dst, [60, 100, 20, 255]);
    }

    #[test]
    fn downsample_handles_odd_dimensions_with_partial_blocks() {
        // 3x3 at factor 2 -> 2x2. Only the top-left block is full; the right
        // column and bottom row blocks hold 2 pixels, the corner just 1.
        // Every pixel opaque green with a distinct red ramp so the averages
        // are easy to hand-check.
        let reds: [u8; 9] = [10, 20, 30, 40, 50, 60, 70, 80, 90];
        let mut src = Vec::with_capacity(9 * 4);
        for red in reds {
            src.extend_from_slice(&[red, 255, 0, 255]);
        }
        let mut dst = vec![0u8; 2 * 2 * 4];
        assert_eq!(downsample_rgba(&src, 3, 3, 2, &mut dst), (2, 2));
        // (10+20+40+50)/4 = 30; (30+60)/2 = 45; (70+80)/2 = 75; 90 alone.
        assert_eq!(&dst[0..4], &[30, 255, 0, 255]);
        assert_eq!(&dst[4..8], &[45, 255, 0, 255]);
        assert_eq!(&dst[8..12], &[75, 255, 0, 255]);
        assert_eq!(&dst[12..16], &[90, 255, 0, 255]);
    }

    #[test]
    fn downsample_never_reads_past_a_short_source() {
        // A source one pixel shy of 3x3: the missing pixel contributes nothing
        // and the corner block ends up empty rather than panicking.
        let mut src = Vec::new();
        for red in [10u8, 20, 30, 40, 50, 60, 70, 80] {
            src.extend_from_slice(&[red, 255, 0, 255]);
        }
        let mut dst = vec![0u8; 2 * 2 * 4];
        assert_eq!(downsample_rgba(&src, 3, 3, 2, &mut dst), (2, 2));
        assert_eq!(&dst[0..4], &[30, 255, 0, 255]);
        assert_eq!(&dst[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_fully_transparent_block_stays_fully_transparent() {
        let src = [0u8; 16];
        let mut dst = [0xFFu8; 4];
        downsample_rgba(&src, 2, 2, 2, &mut dst);
        assert_eq!(dst, [0, 0, 0, 0]);
    }

    #[test]
    fn downsample_factor_larger_than_the_source_collapses_to_one_pixel() {
        let src = [
            200, 0, 0, 255, //
            0, 0, 0, 0, //
            0, 0, 0, 0, //
            0, 0, 0, 0, //
        ];
        let mut dst = [0u8; 4];
        assert_eq!(downsample_rgba(&src, 2, 2, 8, &mut dst), (1, 1));
        assert_eq!(dst, [200, 0, 0, 64]);
    }

    // ---- whole-path tests against the plain viewport render ----

    fn test_gate_range() -> GateRange {
        GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 6,
        }
    }

    fn test_cut() -> ElevationCut {
        let gate_range = test_gate_range();
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
        cut
    }

    fn volume_with(cut: ElevationCut) -> RadarVolume {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), chrono::Utc::now());
        volume.cuts.push(cut);
        volume
    }

    fn test_volume() -> RadarVolume {
        let mut cut = test_cut();
        let mut reflectivity = MomentGrid::new_u8(
            MomentType::Reflectivity,
            test_gate_range(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        for radial_index in 0..4 {
            reflectivity
                .push_u8_row_slice(radial_index, &[20, 30, 40, 50, 60, 70])
                .expect("reflectivity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);
        volume_with(cut)
    }

    fn test_u16_volume() -> RadarVolume {
        let mut cut = test_cut();
        let mut reflectivity = MomentGrid::new_u16(
            MomentType::Reflectivity,
            test_gate_range(),
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
        volume_with(cut)
    }

    fn test_f32_volume() -> RadarVolume {
        let mut cut = test_cut();
        // No `MomentGrid::new_f32` exists; swap the storage before the first
        // row so `push_row` takes the F32 arm.
        let mut reflectivity = MomentGrid::new_u16(
            MomentType::Reflectivity,
            test_gate_range(),
            1.0,
            0.0,
            None,
            None,
        );
        reflectivity.storage = MomentStorage::F32(Vec::new());
        for radial_index in 0..4 {
            reflectivity
                .push_row(
                    radial_index,
                    // dBZ, spanning several colour-table bands.
                    MomentRow::F32(vec![5.0, 18.0, 27.5, 41.0, 52.25, 63.0]),
                )
                .expect("f32 reflectivity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);
        volume_with(cut)
    }

    fn synthetic_options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 96,
            height: 72,
            radar_x_px: 48.0,
            radar_y_px: 36.0,
            km_per_px_x: 0.2,
            km_per_px_y: 0.2,
            rotation_rad: 0.0,
        }
    }

    #[test]
    fn quality_factor_zero_and_one_match_the_plain_render_byte_for_byte() {
        // Every storage width, and a viewport that is non-square, off centre
        // and anisotropic — the identity must not depend on any of that.
        for (label, volume) in [
            ("u8", test_volume()),
            ("u16", test_u16_volume()),
            ("f32", test_f32_volume()),
        ] {
            let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
                .expect("reflectivity cache");
            for options in [synthetic_options(), skewed_options()] {
                let mut plain = vec![0xEEu8; viewport_rgba_buffer_len(options)];
                render_moment_viewport_rgba_into(
                    &volume,
                    0,
                    MomentType::Reflectivity,
                    options,
                    &mut plain,
                )
                .expect("plain render");
                assert!(
                    plain.chunks_exact(4).any(|pixel| pixel[3] > 0),
                    "{label} at {}x{} painted nothing, so the identity proves nothing",
                    options.width,
                    options.height
                );

                for factor in [0u32, 1] {
                    let mut quality = vec![0xEEu8; quality_rgba_buffer_len(options, factor)];
                    let dimensions = render_moment_viewport_quality_rgba_into(
                        &cache,
                        &volume,
                        options,
                        factor,
                        &mut quality,
                    )
                    .expect("quality render");
                    assert_eq!(dimensions, (options.width, options.height));
                    assert_eq!(
                        quality, plain,
                        "{label} factor {factor} at {}x{} must be an exact identity",
                        options.width, options.height
                    );
                }
            }
        }
    }

    #[test]
    fn quality_render_returns_base_dimensions_at_every_factor() {
        let volume = test_volume();
        let options = synthetic_options();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");

        for factor in 1..=4 {
            let mut quality = vec![0u8; quality_rgba_buffer_len(options, factor)];
            let dimensions = render_moment_viewport_quality_rgba_into(
                &cache,
                &volume,
                options,
                factor,
                &mut quality,
            )
            .expect("quality render");
            assert_eq!(dimensions, (96, 72), "factor {factor}");
        }
    }

    #[test]
    fn quality_render_rejects_a_wrongly_sized_buffer() {
        let volume = test_volume();
        let options = synthetic_options();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");

        // The supersampled scratch size is NOT the caller's buffer size.
        let mut oversized = vec![0u8; supersampled_rgba_buffer_len(options, 2)];
        let err =
            render_moment_viewport_quality_rgba_into(&cache, &volume, options, 2, &mut oversized)
                .expect_err("buffer size mismatch");
        assert!(
            matches!(err, RenderError::BufferSizeMismatch { expected, .. } if expected == 96 * 72 * 4),
            "{err}"
        );
    }

    #[test]
    fn a_reused_dirty_scratch_renders_the_same_frame_as_a_fresh_one() {
        // The scratch is grown but never re-zeroed, on the claim that the
        // rasteriser clears every row it owns. If that were wrong, a scratch
        // left over from a bigger frame would bleed through.
        let volume = test_volume();
        let options = synthetic_options();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");

        let mut fresh = vec![0u8; quality_rgba_buffer_len(options, 3)];
        render_moment_viewport_quality_rgba_into(&cache, &volume, options, 3, &mut fresh)
            .expect("fresh render");

        let mut scratch = vec![0xC3u8; supersampled_rgba_buffer_len(options, 4) * 2];
        let mut reused = vec![0u8; quality_rgba_buffer_len(options, 3)];
        render_moment_viewport_quality_rgba_into_with_scratch(
            &cache,
            &volume,
            options,
            3,
            &mut scratch,
            &mut reused,
        )
        .expect("reused render");
        assert_eq!(reused, fresh);

        // And a second pass through the same scratch, now holding the previous
        // frame rather than a constant fill.
        let mut again = vec![0u8; quality_rgba_buffer_len(options, 3)];
        render_moment_viewport_quality_rgba_into_with_scratch(
            &cache,
            &volume,
            options,
            3,
            &mut scratch,
            &mut again,
        )
        .expect("second reused render");
        assert_eq!(again, fresh);
    }

    #[test]
    fn degenerate_viewports_render_instead_of_panicking() {
        let volume = test_volume();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        for (width, height) in [(0u32, 0u32), (1, 1), (0, 5), (5, 0), (7, 3)] {
            let options = ViewportRasterOptions {
                width,
                height,
                radar_x_px: 3.5,
                radar_y_px: 1.5,
                km_per_px_x: 0.3,
                km_per_px_y: 0.4,
                rotation_rad: 0.0,
            };
            for factor in [0u32, 1, 2, 3, 4] {
                let mut rgba = vec![0u8; quality_rgba_buffer_len(options, factor)];
                let dimensions = render_moment_viewport_quality_rgba_into(
                    &cache, &volume, options, factor, &mut rgba,
                )
                .unwrap_or_else(|err| panic!("{width}x{height} at {factor}x: {err}"));
                assert_eq!(dimensions, viewport_dimensions(options));
            }
        }
    }

    /// Sum of the RGB channels — a stand-in for brightness, and the quantity a
    /// straight-alpha average drags toward zero at every echo edge.
    fn rgb_sum(pixel: &[u8]) -> u32 {
        u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2])
    }

    /// The dimmest colour carrying any coverage in `rgba`.
    fn dimmest_covered(rgba: &[u8]) -> u32 {
        rgba.chunks_exact(4)
            .filter(|pixel| pixel[3] > 0)
            .map(rgb_sum)
            .min()
            .expect("the raster paints something")
    }

    #[test]
    fn supersampling_leaves_no_dark_fringe_around_an_echo_edge() {
        // Premultiplied averaging makes each output channel a convex
        // combination of the channels of the covered source pixels, so no
        // output pixel can be darker than the darkest colour that went into
        // it. A straight-alpha average admits transparent black (RGB 0) as a
        // full-weight term and violates this at every echo boundary.
        let volume = test_volume();
        let options = synthetic_options();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");

        let high = supersampled(options, 4);
        let mut source = vec![0u8; viewport_rgba_buffer_len(high)];
        cache
            .render_moment_rgba_into(&volume, high, &mut source)
            .expect("supersampled render");
        let floor = dimmest_covered(&source);

        let mut quality = vec![0u8; quality_rgba_buffer_len(options, 4)];
        render_moment_viewport_quality_rgba_into(&cache, &volume, options, 4, &mut quality)
            .expect("quality render");

        for (index, pixel) in quality.chunks_exact(4).enumerate() {
            if pixel[3] == 0 {
                continue;
            }
            // Slack of 3: the per-channel round-to-nearest can shave one unit
            // off each of the three channels.
            assert!(
                rgb_sum(pixel) + 3 >= floor,
                "pixel {index} {pixel:?} is darker than any colour it averaged ({floor})"
            );
        }
    }

    // ---- real Level II data ----
    //
    // Numbers quoted in the comments below were measured on
    // KABR20260818_064314_RT698_V06 (15 cuts, 0.5 deg reflectivity) rendered
    // into a 900x700 viewport at 1.1 km/px — 990 km across, the zoomed-out
    // regime where one screen pixel spans several gates and dozens of radials.

    /// A real volume, most deterministic source first: the file named by
    /// `NEXRAD_LEVEL2_SAMPLE`, else the first volume in the live cache the app
    /// fills. Absent on a machine that has never run the app, so the tests
    /// below skip rather than fail.
    ///
    /// The cache is mutable machine state -- it holds whatever radar was last
    /// looked at, and it changes while the app runs -- so no test below may
    /// fail because of what it found there. Pin `NEXRAD_LEVEL2_SAMPLE` to make
    /// a run repeatable, which is what a gate needs.
    fn cached_level2_volume() -> Option<RadarVolume> {
        let mut paths = if let Some(path) = pinned_sample() {
            vec![path]
        } else {
            let dir = level2_cache_dir()?;
            std::fs::read_dir(&dir)
                .ok()?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect()
        };
        paths.sort();
        paths.into_iter().find_map(|path| {
            let volume = nexrad_io::decode_volume_from_path(&path).ok()?;
            volume
                .cuts
                .iter()
                .any(|cut| cut.moments.contains_key(&MomentType::Reflectivity))
                .then_some(volume)
        })
    }

    /// One Archive II file named by `NEXRAD_LEVEL2_SAMPLE`, the workspace's
    /// convention for pointing a test at real data.
    fn pinned_sample() -> Option<std::path::PathBuf> {
        let path = std::path::PathBuf::from(std::env::var_os("NEXRAD_LEVEL2_SAMPLE")?);
        assert!(
            path.is_file(),
            "NEXRAD_LEVEL2_SAMPLE names {}, which is not a file",
            path.display()
        );
        Some(path)
    }

    fn level2_cache_dir() -> Option<std::path::PathBuf> {
        if let Some(path) = std::env::var_os("RADAR_WORKSTATION_L2_CACHE") {
            let path = std::path::PathBuf::from(path);
            return path.is_dir().then_some(path);
        }
        let local = std::env::var_os("LOCALAPPDATA")?;
        let path = std::path::PathBuf::from(local)
            .join("FahrenheitResearch")
            .join("RadarWorkstation")
            .join("cache")
            .join("level2-live");
        path.is_dir().then_some(path)
    }

    /// The zoomed-out viewport every real-data test below renders.
    fn wide_options() -> ViewportRasterOptions {
        ViewportRasterOptions {
            width: 900,
            height: 700,
            radar_x_px: 450.0,
            radar_y_px: 350.0,
            km_per_px_x: 1.1,
            km_per_px_y: 1.1,
            rotation_rad: 0.0,
        }
    }

    fn reflectivity_cache(volume: &RadarVolume) -> ViewportMomentCache {
        let cut_index = volume
            .cuts
            .iter()
            .position(|cut| cut.moments.contains_key(&MomentType::Reflectivity))
            .expect("a reflectivity cut");
        ViewportMomentCache::new(volume, cut_index, MomentType::Reflectivity)
            .expect("reflectivity cache")
    }

    fn render_quality(
        cache: &ViewportMomentCache,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
        factor: u32,
    ) -> Vec<u8> {
        let mut rgba = vec![0u8; quality_rgba_buffer_len(options, factor)];
        let (width, height) =
            render_moment_viewport_quality_rgba_into(cache, volume, options, factor, &mut rgba)
                .expect("quality render");
        assert_eq!(
            (width, height),
            (options.width, options.height),
            "factor {factor} must render into the base rect"
        );
        rgba
    }

    fn pixel_at(rgba: &[u8], width: u32, x: usize, y: usize) -> [u8; 4] {
        let offset = y * width as usize * 4 + x * 4;
        [
            rgba[offset],
            rgba[offset + 1],
            rgba[offset + 2],
            rgba[offset + 3],
        ]
    }

    /// Isolated single-pixel colour islands — a covered pixel with no covered
    /// neighbour in any of the four cardinal directions. That is exactly the
    /// speckle one-sample-per-pixel produces when a screen pixel spans many
    /// radials: a lone gate wins the point sample and lands as a dot in an
    /// otherwise empty neighbourhood.
    fn speckle_islands(rgba: &[u8], width: u32, height: u32) -> usize {
        let mut islands = 0;
        for y in 1..height as usize - 1 {
            for x in 1..width as usize - 1 {
                if pixel_at(rgba, width, x, y)[3] == 0 {
                    continue;
                }
                if pixel_at(rgba, width, x - 1, y)[3] == 0
                    && pixel_at(rgba, width, x + 1, y)[3] == 0
                    && pixel_at(rgba, width, x, y - 1)[3] == 0
                    && pixel_at(rgba, width, x, y + 1)[3] == 0
                {
                    islands += 1;
                }
            }
        }
        islands
    }

    /// Distance in pixels from `(centre_x, centre_y)` to the furthest covered
    /// pixel — the outer edge of the sweep, and the thing that would move if
    /// the supersample changed the projection.
    fn max_covered_radius_px(rgba: &[u8], width: u32, height: u32, centre: (f32, f32)) -> f32 {
        let mut furthest = 0.0f32;
        for y in 0..height as usize {
            for x in 0..width as usize {
                if pixel_at(rgba, width, x, y)[3] == 0 {
                    continue;
                }
                let dx = x as f32 - centre.0;
                let dy = y as f32 - centre.1;
                furthest = furthest.max((dx * dx + dy * dy).sqrt());
            }
        }
        furthest
    }

    #[test]
    fn real_volume_supersampling_removes_isolated_speckle_pixels() {
        let Some(volume) = cached_level2_volume() else {
            eprintln!("no cached Level II volume; skipping the real-data check");
            return;
        };
        let cache = reflectivity_cache(&volume);
        let options = wide_options();

        let mut islands = Vec::new();
        for factor in [1u32, 2, 4] {
            let rgba = render_quality(&cache, &volume, options, factor);
            islands.push(speckle_islands(&rgba, options.width, options.height));
        }
        eprintln!("speckle islands at 1x/2x/4x: {islands:?}");

        // Measured on KABR: 2691 -> 1367 -> 240, a 91% reduction.
        assert!(islands[0] > 0, "the 1x raster should be speckled at all");
        assert!(
            islands[1] < islands[0],
            "2x speckle {} should fall below 1x {}",
            islands[1],
            islands[0]
        );
        assert!(
            islands[2] < islands[1],
            "4x speckle {} should fall below 2x {}",
            islands[2],
            islands[1]
        );
        assert!(
            islands[2] * 4 <= islands[0],
            "4x speckle {} should be at most a quarter of 1x {}",
            islands[2],
            islands[0]
        );
    }

    #[test]
    fn real_volume_supersampling_only_adds_coverage_and_never_moves_it() {
        // Ground coverage is held invariant, so the sweep must land in exactly
        // the same place; the extra samples only fill in the gates the single
        // point sample skipped over. Measured on KABR: 45,962 covered pixels at
        // 1x and 73,107 at 4x (+59% — the 1x raster was missing over a third of
        // the echo), with 4 of the 45,962 lost and the outer edge moving 0.45px.
        let Some(volume) = cached_level2_volume() else {
            eprintln!("no cached Level II volume; skipping the real-data check");
            return;
        };
        let cache = reflectivity_cache(&volume);
        let options = wide_options();
        let centre = (options.radar_x_px, options.radar_y_px);

        let plain = render_quality(&cache, &volume, options, 1);
        let high = render_quality(&cache, &volume, options, 4);

        let plain_covered = plain.chunks_exact(4).filter(|p| p[3] > 0).count();
        let high_covered = high.chunks_exact(4).filter(|p| p[3] > 0).count();
        let lost = plain
            .chunks_exact(4)
            .zip(high.chunks_exact(4))
            .filter(|(before, after)| before[3] > 0 && after[3] == 0)
            .count();
        let plain_radius = max_covered_radius_px(&plain, options.width, options.height, centre);
        let high_radius = max_covered_radius_px(&high, options.width, options.height, centre);
        eprintln!(
            "covered 1x {plain_covered} -> 4x {high_covered} (lost {lost}); \
             outer radius {plain_radius:.2}px -> {high_radius:.2}px"
        );

        assert!(plain_covered > 0, "the real volume should paint something");
        assert!(
            high_covered > plain_covered,
            "4x should fill gaps, not lose them ({plain_covered} -> {high_covered})"
        );
        assert!(
            lost * 200 < plain_covered,
            "4x dropped {lost} of the {plain_covered} pixels 1x had covered - \
             more than 0.5% means the projection moved, not just resampled"
        );
        assert!(
            (high_radius - plain_radius).abs() < 2.0,
            "the outer edge of the sweep moved from {plain_radius:.2}px to {high_radius:.2}px"
        );
    }

    /// The base viewport whose factor-1 render must reproduce, pixel for pixel,
    /// subsample `(k, l)` of every `factor`-sized block of the supersampled
    /// raster.
    ///
    /// Supersampled pixel `s·i + k` is sampled at ground offset
    /// `(i + (k + 0.5)/s - radar_x_px) · km_per_px`, and a plain render whose
    /// radar sits at `radar_x_px + 0.5 - (k + 0.5)/s` samples base pixel `i` at
    /// exactly that offset. So the whole supersampled raster is nothing but the
    /// `s²` sub-pixel-offset plain renders interleaved — if, and only if, the
    /// supersample scaling is registered to the ground correctly.
    fn subsample_plane_options(
        options: ViewportRasterOptions,
        factor: u32,
        k: u32,
        l: u32,
    ) -> ViewportRasterOptions {
        let scale = factor as f32;
        ViewportRasterOptions {
            radar_x_px: options.radar_x_px + 0.5 - (k as f32 + 0.5) / scale,
            radar_y_px: options.radar_y_px + 0.5 - (l as f32 + 0.5) / scale,
            ..options
        }
    }

    /// A deliberately WRONG scaling that treats `radar_x_px` as a pixel-CENTRE
    /// coordinate instead of the pixel-CORNER coordinate the rasteriser uses.
    /// Kept only so the registration test below can show it has teeth.
    fn supersampled_pixel_centre_convention(
        options: ViewportRasterOptions,
        factor: u32,
    ) -> ViewportRasterOptions {
        let scale = factor as f32;
        let (width, height) = viewport_dimensions(options);
        ViewportRasterOptions {
            width: width * factor,
            height: height * factor,
            radar_x_px: (options.radar_x_px + 0.5) * scale - 0.5,
            radar_y_px: (options.radar_y_px + 0.5) * scale - 0.5,
            km_per_px_x: options.km_per_px_x / scale,
            km_per_px_y: options.km_per_px_y / scale,
            rotation_rad: 0.0,
        }
    }

    /// Pixels of `plane_rgba` (a base-sized render) that differ from subsample
    /// `(k, l)` of every `factor`-sized block of `high_rgba`.
    fn plane_mismatch_count(
        high: (&[u8], ViewportRasterOptions),
        plane_rgba: &[u8],
        base: ViewportRasterOptions,
        factor: u32,
        (k, l): (u32, u32),
    ) -> usize {
        let (high_rgba, high_options) = high;
        let (base_width, base_height) = (base.width as usize, base.height as usize);
        let (high_width, factor) = (high_options.width as usize, factor as usize);
        let mut mismatches = 0;
        for y in 0..base_height {
            for x in 0..base_width {
                let plane = &plane_rgba[(y * base_width + x) * 4..][..4];
                let high_index = (y * factor + l as usize) * high_width + x * factor + k as usize;
                if plane != &high_rgba[high_index * 4..][..4] {
                    mismatches += 1;
                }
            }
        }
        mismatches
    }

    #[test]
    fn real_volume_subsample_planes_land_where_a_plain_render_would_sample() {
        // THE registration test. Every one of the 16 sub-pixel planes of a 4x
        // supersampled real sweep is compared against an independently
        // rasterised frame that samples the same ground points, so a half-pixel
        // convention error anywhere in `supersampled` shows up as tens of
        // thousands of differing pixels. Measured on KABR at 900x700: 0
        // mismatches out of 630,000 for all 16 planes; the pixel-centre
        // variant kept below misses on 36,864.
        let Some(volume) = cached_level2_volume() else {
            eprintln!("no cached Level II volume; skipping the real-data check");
            return;
        };
        let cache = reflectivity_cache(&volume);
        let options = wide_options();
        let factor = 4u32;

        let high = supersampled(options, factor);
        assert_eq!(effective_supersample_factor(options, factor), factor);
        let mut high_rgba = vec![0u8; supersampled_rgba_buffer_len(options, factor)];
        cache
            .render_moment_rgba_into(&volume, high, &mut high_rgba)
            .expect("supersampled render");

        for l in 0..factor {
            for k in 0..factor {
                let plane_options = subsample_plane_options(options, factor, k, l);
                let mut plane = vec![0u8; viewport_rgba_buffer_len(plane_options)];
                cache
                    .render_moment_rgba_into(&volume, plane_options, &mut plane)
                    .expect("plane render");
                assert!(
                    plane.chunks_exact(4).any(|pixel| pixel[3] > 0),
                    "plane ({k}, {l}) painted nothing"
                );
                let mismatches =
                    plane_mismatch_count((&high_rgba, high), &plane, options, factor, (k, l));
                assert_eq!(
                    mismatches, 0,
                    "subsample plane ({k}, {l}) of the 4x raster does not sample the ground a \
                     plain render at radar ({}, {}) samples - the supersample is mis-registered",
                    plane_options.radar_x_px, plane_options.radar_y_px
                );
            }
        }

        // Teeth: the same comparison against the pixel-centre scaling, which is
        // the half-pixel bug this test exists to catch.
        let wrong = supersampled_pixel_centre_convention(options, factor);
        let mut wrong_rgba = vec![0u8; viewport_rgba_buffer_len(wrong)];
        cache
            .render_moment_rgba_into(&volume, wrong, &mut wrong_rgba)
            .expect("mis-registered render");
        let plane_options = subsample_plane_options(options, factor, 0, 0);
        let mut plane = vec![0u8; viewport_rgba_buffer_len(plane_options)];
        cache
            .render_moment_rgba_into(&volume, plane_options, &mut plane)
            .expect("plane render");
        let wrong_mismatches =
            plane_mismatch_count((&wrong_rgba, wrong), &plane, options, factor, (0, 0));
        assert!(
            wrong_mismatches > 10_000,
            "the pixel-centre scaling only missed {wrong_mismatches} pixels, so this test \
             would not catch the half-pixel bug it is aimed at"
        );
    }

    /// A deliberately WRONG box filter that averages straight RGBA, kept only
    /// so the tests below can show what it costs on a real frame.
    fn downsample_straight_alpha(
        src: &[u8],
        src_width: u32,
        src_height: u32,
        factor: u32,
        dst: &mut [u8],
    ) {
        let dst_width = src_width.div_ceil(factor);
        let dst_height = src_height.div_ceil(factor);
        let src_stride = src_width as usize * 4;
        for dst_y in 0..dst_height as usize {
            for dst_x in 0..dst_width as usize {
                let mut sums = [0u64; 4];
                let mut count = 0u64;
                let y_start = dst_y as u32 * factor;
                let x_start = dst_x as u32 * factor;
                for src_y in y_start..(y_start + factor).min(src_height) {
                    for src_x in x_start..(x_start + factor).min(src_width) {
                        let offset = src_y as usize * src_stride + src_x as usize * 4;
                        for (channel, sum) in sums.iter_mut().enumerate() {
                            *sum += u64::from(src[offset + channel]);
                        }
                        count += 1;
                    }
                }
                let offset = dst_y * dst_width as usize * 4 + dst_x * 4;
                for (channel, sum) in sums.iter().enumerate() {
                    dst[offset + channel] = if count == 0 { 0 } else { (sum / count) as u8 };
                }
            }
        }
    }

    #[test]
    fn real_volume_premultiplied_filter_beats_straight_alpha_on_a_real_frame() {
        // Both filters reduce the SAME supersampled raster, so the only
        // difference is the alpha handling. No output colour may be darker
        // than the darkest colour the source raster contained; every pixel
        // that is, is a dark fringe the user would see ringing the echo.
        let Some(volume) = cached_level2_volume() else {
            eprintln!("no cached Level II volume; skipping the real-data check");
            return;
        };
        let cache = reflectivity_cache(&volume);
        let options = wide_options();

        let high = supersampled(options, 4);
        let mut source = vec![0u8; supersampled_rgba_buffer_len(options, 4)];
        cache
            .render_moment_rgba_into(&volume, high, &mut source)
            .expect("supersampled render");
        let floor = dimmest_covered(&source);

        let mut premultiplied = vec![0u8; quality_rgba_buffer_len(options, 4)];
        downsample_rgba(&source, high.width, high.height, 4, &mut premultiplied);
        let mut straight = vec![0u8; quality_rgba_buffer_len(options, 4)];
        downsample_straight_alpha(&source, high.width, high.height, 4, &mut straight);

        // Slack of 3: round-to-nearest can shave a unit off each channel.
        let fringe = |rgba: &[u8]| {
            rgba.chunks_exact(4)
                .filter(|pixel| pixel[3] > 0)
                .filter(|pixel| rgb_sum(pixel) + 3 < floor)
                .count()
        };
        let premultiplied_fringe = fringe(&premultiplied);
        let straight_fringe = fringe(&straight);
        eprintln!(
            "dark-fringe pixels below the dimmest source colour ({floor}): \
             premultiplied {premultiplied_fringe}, straight-alpha {straight_fringe}"
        );

        // Measured on KABR: 0 vs 19,742 out of 73,107 covered pixels.
        assert_eq!(
            premultiplied_fringe, 0,
            "the premultiplied filter must not darken any pixel below its source colours"
        );
        // How fringed the straight-alpha reference gets is a property of the
        // frame, not of this crate: a volume with little partial coverage
        // fringes little. It is the CONTROL, so when it is weak the comparison
        // simply has nothing to show, and saying so is the honest outcome --
        // the claim above (premultiplied darkens nothing) was made either way.
        if straight_fringe <= 1_000 {
            eprintln!(
                "the straight-alpha control fringed only {straight_fringe} pixels, so this frame \
                 barely exercises partial coverage; the premultiplied claim above still held. \
                 Point NEXRAD_LEVEL2_SAMPLE at a volume with more echo to compare on"
            );
        }
    }

    #[test]
    fn real_volume_downsampled_colour_never_leaves_its_own_block() {
        // A far tighter statement than the global brightness floor above: each
        // output channel must land inside the per-channel min/max of the
        // COVERED source pixels of its own 4x4 block. Premultiplied averaging
        // makes that a mathematical certainty (a convex combination of the
        // covered colours); straight alpha adds transparent black as a
        // full-weight term and falls out the bottom. Measured on KABR at 4x:
        // 0 violating pixels premultiplied, 47,848 straight-alpha (worst 238
        // levels below the block's darkest colour).
        let Some(volume) = cached_level2_volume() else {
            eprintln!("no cached Level II volume; skipping the real-data check");
            return;
        };
        let cache = reflectivity_cache(&volume);
        let options = wide_options();
        let factor = 4usize;

        let high = supersampled(options, 4);
        let mut source = vec![0u8; supersampled_rgba_buffer_len(options, 4)];
        cache
            .render_moment_rgba_into(&volume, high, &mut source)
            .expect("supersampled render");

        let mut premultiplied = vec![0u8; quality_rgba_buffer_len(options, 4)];
        downsample_rgba(&source, high.width, high.height, 4, &mut premultiplied);
        let mut straight = vec![0u8; quality_rgba_buffer_len(options, 4)];
        downsample_straight_alpha(&source, high.width, high.height, 4, &mut straight);

        let high_width = high.width as usize;
        let (base_width, base_height) = (options.width as usize, options.height as usize);
        let mut premultiplied_violations = 0usize;
        let mut straight_violations = 0usize;
        let mut worst_straight = 0i32;
        for y in 0..base_height {
            for x in 0..base_width {
                let mut low = [255u8; 3];
                let mut high_bound = [0u8; 3];
                let mut covered = false;
                for l in 0..factor {
                    for k in 0..factor {
                        let index = (y * factor + l) * high_width + x * factor + k;
                        let pixel = &source[index * 4..][..4];
                        if pixel[3] == 0 {
                            continue;
                        }
                        covered = true;
                        for channel in 0..3 {
                            low[channel] = low[channel].min(pixel[channel]);
                            high_bound[channel] = high_bound[channel].max(pixel[channel]);
                        }
                    }
                }
                if !covered {
                    continue;
                }
                let escape = |rgba: &[u8]| {
                    let pixel = &rgba[(y * base_width + x) * 4..][..4];
                    (0..3)
                        .map(|channel| {
                            let value = i32::from(pixel[channel]);
                            (i32::from(low[channel]) - value)
                                .max(value - i32::from(high_bound[channel]))
                        })
                        .max()
                        .unwrap_or(0)
                };
                // Slack of 1 absorbs the round-to-nearest on each channel.
                if escape(&premultiplied) > 1 {
                    premultiplied_violations += 1;
                }
                let straight_escape = escape(&straight);
                if straight_escape > 1 {
                    straight_violations += 1;
                    worst_straight = worst_straight.max(straight_escape);
                }
            }
        }
        eprintln!(
            "out-of-block pixels: premultiplied {premultiplied_violations}, \
             straight-alpha {straight_violations} (worst {worst_straight} levels)"
        );

        assert_eq!(
            premultiplied_violations, 0,
            "a downsampled pixel took a colour no pixel in its own block had"
        );
        // As above: the straight-alpha reference is the control, and how hard
        // it escapes its block is the frame's business. The claim is the line
        // before this one.
        if straight_violations <= 1_000 {
            eprintln!(
                "the straight-alpha control escaped its block only {straight_violations} times, \
                 so this frame barely exercises partial coverage; the premultiplied claim above \
                 still held. Point NEXRAD_LEVEL2_SAMPLE at a volume with more echo to compare on"
            );
        }
    }
}
