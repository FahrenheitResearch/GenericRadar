//! Validating and decoding a tile body, and building its mip chain.
//!
//! # Order of operations is the security property
//!
//! A tile body arrives from the network or from a cache file that some other
//! process may have mangled. Every check below happens *before* the one after
//! it, and in particular the image header's declared dimensions are read and
//! rejected before the full decode is allowed to allocate a pixel buffer.
//! Nothing is written to the disk cache until the body has decoded
//! successfully, so one bad answer cannot become a permanent one.
//!
//! This matters concretely: the USGS services answer an out-of-coverage tile
//! with HTTP 404 and a several-hundred-byte `text/html` body. A pipeline that
//! stores first and validates later caches that HTML forever.

use std::io::Cursor;

use crate::{MAX_TILE_ENCODED_BYTES, MIP_LEVELS, TILE_TEXELS, TileId, TileProvider};

/// A decoded tile with its CPU-built mip chain.
///
/// The chain is built here, on a worker, rather than on the GPU, because the
/// GPU-side alternative is a render pass per level per tile and this costs a
/// fraction of a millisecond off the UI thread. It is not optional: one LOD
/// bucket admits a 2.55x sweep of camera scale, so a tile is minified by up to
/// 2.03x while still inside the bucket that chose it, and unmipped minification
/// at that ratio shimmers visibly during a zoom.
pub struct DecodedTile {
    pub provider: TileProvider,
    pub tile: TileId,
    /// Level 0 first: 256, 128, 64, 32 square. RGBA8, **straight** alpha,
    /// row-major, tightly packed at `4 * texels` bytes per row.
    pub levels: Vec<Vec<u8>>,
    pub level0_texels: u32,
}

impl DecodedTile {
    #[must_use]
    pub fn mip_level_count(&self) -> u32 {
        self.levels.len() as u32
    }

    /// Total decoded bytes, which is what the texture budget is spent in.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }

    /// Bytes and edge length of one mip level.
    #[must_use]
    pub fn level(&self, index: u32) -> Option<(&[u8], u32)> {
        let bytes = self.levels.get(index as usize)?;
        let texels = self.level0_texels >> index;
        (texels > 0).then_some((bytes.as_slice(), texels))
    }
}

/// Hand-written so a log line prints sizes instead of a megabyte of pixels.
impl std::fmt::Debug for DecodedTile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DecodedTile")
            .field("provider", &self.provider)
            .field("tile", &self.tile)
            .field("level0_texels", &self.level0_texels)
            .field("levels", &self.levels.len())
            .field("bytes", &self.byte_len())
            .finish()
    }
}

/// Why a body was refused. Distinguished because the caller reacts
/// differently: an undecodable body is a transient failure worth one retry,
/// while a body of the wrong size from a provider that only serves 256x256 is
/// a sign we are talking to the wrong endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecodeError {
    Empty,
    TooLarge(usize),
    UnreadableHeader,
    WrongDimensions(u32, u32),
    Undecodable,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(formatter, "empty body"),
            Self::TooLarge(len) => write!(formatter, "body of {len} bytes exceeds the tile limit"),
            Self::UnreadableHeader => write!(formatter, "no readable image header"),
            Self::WrongDimensions(width, height) => {
                write!(formatter, "{width}x{height} is not a {TILE_TEXELS}px tile")
            }
            Self::Undecodable => write!(formatter, "image data did not decode"),
        }
    }
}

/// Validate and decode an encoded tile body into a mip chain.
pub(crate) fn decode_tile(
    provider: TileProvider,
    tile: TileId,
    bytes: &[u8],
) -> Result<DecodedTile, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    if bytes.len() > MAX_TILE_ENCODED_BYTES {
        return Err(DecodeError::TooLarge(bytes.len()));
    }

    // Header first. `into_dimensions` parses the header only, so a hostile or
    // corrupt body cannot make the decoder allocate before this check.
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| DecodeError::UnreadableHeader)?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| DecodeError::UnreadableHeader)?;
    if width != TILE_TEXELS || height != TILE_TEXELS {
        return Err(DecodeError::WrongDimensions(width, height));
    }

    let image = image::load_from_memory(bytes).map_err(|_| DecodeError::Undecodable)?;
    // The header is not the decoder. Re-check what actually came out.
    if image.width() != width || image.height() != height {
        return Err(DecodeError::WrongDimensions(image.width(), image.height()));
    }
    let level0 = image.to_rgba8().into_raw();
    if level0.len() != (TILE_TEXELS * TILE_TEXELS * 4) as usize {
        return Err(DecodeError::Undecodable);
    }

    Ok(DecodedTile {
        provider,
        tile,
        levels: build_mip_chain(level0, TILE_TEXELS),
        level0_texels: TILE_TEXELS,
    })
}

/// Box-filter mip chain, [`MIP_LEVELS`] levels or until a level would be
/// smaller than one texel.
///
/// The average is taken in **linear light**, not in sRGB code values. The
/// texture is uploaded as `Rgba8UnormSrgb`, so averaging the encoded values
/// directly would darken every mip level — most visibly on high-contrast
/// imagery, exactly where a shimmering basemap is most distracting.
fn build_mip_chain(level0: Vec<u8>, texels: u32) -> Vec<Vec<u8>> {
    let mut levels = Vec::with_capacity(MIP_LEVELS as usize);
    let mut edge = texels;
    levels.push(level0);
    while levels.len() < MIP_LEVELS as usize && edge >= 2 {
        let source = levels.last().expect("at least level 0 is present");
        let next_edge = edge / 2;
        levels.push(downsample_half(source, edge, next_edge));
        edge = next_edge;
    }
    levels
}

fn downsample_half(source: &[u8], edge: u32, next_edge: u32) -> Vec<u8> {
    let stride = (edge * 4) as usize;
    let mut out = vec![0u8; (next_edge * next_edge * 4) as usize];
    for row in 0..next_edge as usize {
        for column in 0..next_edge as usize {
            let top_left = row * 2 * stride + column * 2 * 4;
            let top_right = top_left + 4;
            let bottom_left = top_left + stride;
            let bottom_right = bottom_left + 4;
            let destination = (row * next_edge as usize + column) * 4;
            for channel in 0..3 {
                let sum = srgb_to_linear(source[top_left + channel])
                    + srgb_to_linear(source[top_right + channel])
                    + srgb_to_linear(source[bottom_left + channel])
                    + srgb_to_linear(source[bottom_right + channel]);
                out[destination + channel] = linear_to_srgb(sum * 0.25);
            }
            // Alpha is a coverage fraction, already linear.
            let alpha = u32::from(source[top_left + 3])
                + u32::from(source[top_right + 3])
                + u32::from(source[bottom_left + 3])
                + u32::from(source[bottom_right + 3]);
            out[destination + 3] = ((alpha + 2) / 4) as u8;
        }
    }
    out
}

/// sRGB electro-optical transfer function, IEC 61966-2-1.
fn srgb_to_linear(value: u8) -> f32 {
    static TABLE: std::sync::LazyLock<[f32; 256]> = std::sync::LazyLock::new(|| {
        let mut table = [0.0_f32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let channel = index as f32 / 255.0;
            *slot = if channel <= 0.040_45 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            };
        }
        table
    });
    TABLE[value as usize]
}

fn linear_to_srgb(value: f32) -> u8 {
    let clamped = value.clamp(0.0, 1.0);
    let encoded = if clamped <= 0.003_130_8 {
        clamped * 12.92
    } else {
        1.055 * clamped.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real tile body captured from the USGS National Map imagery service on
    /// 2026-08-18: `/USGSImageryOnly/MapServer/tile/9/202/117`, which is the
    /// z9 tile over KTLX. Checked in specifically so that dropping the `jpeg`
    /// feature from the workspace `image` dependency fails a test instead of
    /// silently blanking the basemap.
    const REAL_JPEG_TILE: &[u8] = include_bytes!("../tests/data/usgs-imagery-9-117-202.jpg");

    /// A real OpenStreetMap tile body captured the same day: the standard
    /// layer serves palette PNG, which is a different decoder path.
    const REAL_PNG_TILE: &[u8] = include_bytes!("../tests/data/osm-9-117-202.png");

    fn tile() -> TileId {
        TileId::new(9, 117, 202).expect("valid")
    }

    fn encoded_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut buffer = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("encode");
        buffer.into_inner()
    }

    /// The check that pins the `jpeg` feature. Every tile the USGS services
    /// return is `Content-Type: image/jpeg`; without the feature this decode
    /// returns `Err` and the whole layer is permanently, silently blank.
    #[test]
    fn decodes_a_real_usgs_jpeg_tile() {
        let decoded = decode_tile(TileProvider::UsgsImagery, tile(), REAL_JPEG_TILE)
            .expect("the workspace image dependency must keep its jpeg feature");
        assert_eq!(decoded.level0_texels, 256);
        assert_eq!(decoded.levels[0].len(), 256 * 256 * 4);
        assert_eq!(decoded.mip_level_count(), MIP_LEVELS);
        assert_eq!(decoded.tile, tile());
        assert_eq!(decoded.provider, TileProvider::UsgsImagery);

        // Real orthoimagery is not a flat colour. If it were, the fixture
        // would have been replaced by an error page at some point.
        let level0 = &decoded.levels[0];
        let unique: std::collections::HashSet<_> =
            level0.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect();
        assert!(
            unique.len() > 1_000,
            "only {} distinct colours",
            unique.len()
        );
        assert!(level0.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn decodes_a_real_openstreetmap_png_tile() {
        let decoded = decode_tile(TileProvider::OpenStreetMap, tile(), REAL_PNG_TILE)
            .expect("palette PNG must decode");
        assert_eq!(decoded.level0_texels, 256);
        assert_eq!(decoded.levels[0].len(), 256 * 256 * 4);
        assert_eq!(decoded.mip_level_count(), MIP_LEVELS);
    }

    #[test]
    fn the_mip_chain_halves_each_level() {
        let decoded =
            decode_tile(TileProvider::UsgsImagery, tile(), REAL_JPEG_TILE).expect("decodes");
        for level in 0..decoded.mip_level_count() {
            let (bytes, edge) = decoded.level(level).expect("level present");
            assert_eq!(edge, 256 >> level);
            assert_eq!(bytes.len(), (edge * edge * 4) as usize);
        }
        assert!(decoded.level(MIP_LEVELS).is_none());
        // 256 + 128 + 64 + 32 squared, times four channels.
        assert_eq!(decoded.byte_len(), (65_536 + 16_384 + 4_096 + 1_024) * 4);
    }

    /// A flat colour must survive downsampling unchanged. Averaging in sRGB
    /// code values instead of linear light passes this test too, which is why
    /// the next one exists.
    #[test]
    fn a_flat_tile_downsamples_to_the_same_flat_colour() {
        let flat = vec![[137u8, 42, 200, 255]; 256 * 256]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();
        let levels = build_mip_chain(flat, 256);
        for level in &levels {
            assert!(
                level.chunks_exact(4).all(|p| p == [137, 42, 200, 255]),
                "a flat colour drifted while downsampling"
            );
        }
    }

    /// Half black, half white, averaged in linear light, is mid-grey at linear
    /// 0.5 — sRGB 188, not sRGB 128. Averaging the encoded values gives 127/128
    /// and darkens the image. This is the test that pins the transfer function.
    #[test]
    fn downsampling_averages_in_linear_light() {
        let mut checker = vec![0u8; 4 * 4 * 4];
        for row in 0..4 {
            for column in 0..4 {
                let value = if (row + column) % 2 == 0 { 0 } else { 255 };
                let base = (row * 4 + column) * 4;
                checker[base] = value;
                checker[base + 1] = value;
                checker[base + 2] = value;
                checker[base + 3] = 255;
            }
        }
        let half = downsample_half(&checker, 4, 2);
        for pixel in half.chunks_exact(4) {
            assert!(
                (185..=191).contains(&pixel[0]),
                "expected linear-light mid grey near 188, got {}",
                pixel[0]
            );
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn the_transfer_function_round_trips() {
        for value in 0..=255u8 {
            let back = linear_to_srgb(srgb_to_linear(value));
            assert_eq!(back, value, "sRGB {value} round-tripped to {back}");
        }
        assert_eq!(linear_to_srgb(0.0), 0);
        assert_eq!(linear_to_srgb(1.0), 255);
        assert_eq!(linear_to_srgb(-1.0), 0);
        assert_eq!(linear_to_srgb(2.0), 255);
    }

    /// The 404 body the USGS services actually return, and the other shapes a
    /// bad response takes. None of these may reach the disk cache.
    #[test]
    fn non_image_and_wrong_size_bodies_are_refused() {
        let provider = TileProvider::UsgsImagery;
        let refused = |bytes: &[u8]| decode_tile(provider, tile(), bytes).err();
        assert_eq!(refused(b""), Some(DecodeError::Empty));
        assert_eq!(
            refused(b"<html><head><title>404</title></head></html>"),
            Some(DecodeError::UnreadableHeader)
        );
        assert_eq!(
            refused(&encoded_png(1, 1)),
            Some(DecodeError::WrongDimensions(1, 1))
        );
        assert_eq!(
            refused(&encoded_png(512, 512)),
            Some(DecodeError::WrongDimensions(512, 512))
        );
        assert_eq!(
            refused(&vec![0u8; MAX_TILE_ENCODED_BYTES + 1]),
            Some(DecodeError::TooLarge(MAX_TILE_ENCODED_BYTES + 1))
        );
    }

    /// Truncation, at every length, must never produce a buffer of the wrong
    /// size and must never panic.
    ///
    /// MEASURED, and worth knowing: `zune-jpeg` is lenient about a truncated
    /// JPEG and will happily return a full-size image with the missing rows
    /// left grey, so "truncated" and "rejected" are *not* the same thing here.
    /// That is acceptable because nothing downstream trusts tile content — but
    /// it is exactly why the size check below is on the decoded buffer rather
    /// than on the decoder's error, and why the disk cache writes only after a
    /// successful decode of the bytes it is about to store.
    #[test]
    fn truncation_never_yields_a_buffer_of_the_wrong_size() {
        for numerator in [1usize, 2, 3, 5, 7, 9] {
            let cut = REAL_JPEG_TILE.len() * numerator / 10;
            // `Err` is equally acceptable: the point is that a partial
            // decode never produces a buffer of the wrong size.
            if let Ok(decoded) =
                decode_tile(TileProvider::UsgsImagery, tile(), &REAL_JPEG_TILE[..cut])
            {
                assert_eq!(decoded.level0_texels, TILE_TEXELS, "cut at {cut}");
                assert_eq!(decoded.levels[0].len(), 256 * 256 * 4, "cut at {cut}");
            }
        }
        // A prefix too short to hold an image header is refused outright.
        let result = decode_tile(TileProvider::UsgsImagery, tile(), &REAL_JPEG_TILE[..4]);
        assert!(result.is_err(), "a 4-byte prefix must not decode");
    }

    #[test]
    fn debug_output_prints_sizes_not_pixels() {
        let decoded =
            decode_tile(TileProvider::UsgsImagery, tile(), REAL_JPEG_TILE).expect("decodes");
        let text = format!("{decoded:?}");
        assert!(text.len() < 200, "Debug printed {} chars", text.len());
        assert!(text.contains("level0_texels"));
    }
}
