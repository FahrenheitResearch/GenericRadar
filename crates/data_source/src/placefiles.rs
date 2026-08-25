//! Transport, image decoding and durable source preferences for GR placefiles.
//!
//! Network access and image codecs stay in this crate. The workstation receives
//! ordinary text, RGBA pixels and typed preferences, so its dependency firewall
//! never needs an HTTP client, an image codec or a serialization library.

use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn default_enabled() -> bool {
    true
}

fn default_show_text() -> bool {
    true
}

fn default_visibility_range_percent() -> u16 {
    100
}

/// One persisted local file or HTTP(S) community feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceConfig {
    /// An HTTP(S) URL or a platform-native absolute local path.
    ///
    /// `url` is accepted while reading older GR-compatible configuration.
    #[serde(alias = "url")]
    pub source: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_show_text")]
    pub show_text: bool,
    #[serde(default = "default_visibility_range_percent")]
    pub visibility_range_percent: u16,
}

impl SourceConfig {
    /// Create an enabled source using the placefile's own visibility range.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            enabled: true,
            show_text: true,
            visibility_range_percent: default_visibility_range_percent(),
        }
    }
}

/// Decoded unassociated RGBA pixels ready for an egui texture upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedIconImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Whether a source is an HTTP(S) feed rather than a native filesystem path.
pub fn is_remote_source(source: &str) -> bool {
    let source = source.trim();
    source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || source
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

/// Fetch remote text or read a downloaded placefile without blocking the UI.
///
/// Callers run this on a worker. Some community services require the query
/// parameters sent by GR clients; retry the original URL if they reject them.
pub fn load_source_text(source: &str) -> Result<String, String> {
    let bytes = if is_remote_source(source) {
        let separator = if source.contains('?') { '&' } else { '?' };
        let gr_url = format!("{source}{separator}version=1.5&dpi=96");
        read_remote_bytes(&gr_url, "placefile")
            .or_else(|_| read_remote_bytes(source, "placefile"))?
    } else {
        read_local_bytes(Path::new(source), "placefile")?
    };

    if bytes.contains(&0) {
        return Err(format!(
            "placefile contains NUL bytes; a text file was expected: {source}"
        ));
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// Fetch a relative-resolved local or remote icon sheet and decode real pixels.
pub fn load_icon_image(source: &str) -> Result<DecodedIconImage, String> {
    let bytes = if is_remote_source(source) {
        read_remote_bytes(source, "placefile icon")?
    } else {
        read_local_bytes(Path::new(source), "placefile icon")?
    };
    decode_icon_image(&bytes)
}

/// Decode PNG/JPEG icon sheets without imposing an arbitrary resolution limit.
pub fn decode_icon_image(bytes: &[u8]) -> Result<DecodedIconImage, String> {
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("identify placefile icon: {error}"))?
        .into_dimensions()
        .map_err(|error| format!("read placefile icon dimensions: {error}"))?;
    let pixels = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok());
    if dimensions.0 == 0 || dimensions.1 == 0 || pixels.is_none() {
        return Err(format!(
            "placefile icon dimensions {} × {} cannot be represented on this system",
            dimensions.0, dimensions.1
        ));
    }
    // `image::load_from_memory` silently installs a 512 MiB allocation
    // ceiling. These are explicit user-selected professional overlays, so
    // honor the application's no-ceiling policy instead of inheriting it.
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("identify placefile icon: {error}"))?;
    reader.no_limits();
    let image = reader
        .decode()
        .map_err(|error| format!("decode placefile icon: {error}"))?
        .into_rgba8();
    Ok(DecodedIconImage {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

/// Restore source preferences. A missing file means a clean first launch.
pub fn load_configs(path: &Path) -> Result<Vec<SourceConfig>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "read placefile settings {}: {error}",
                path.display()
            ));
        }
    };
    serde_json::from_str(&text)
        .map_err(|error| format!("parse placefile settings {}: {error}", path.display()))
}

/// Persist all source preferences with a flushed temporary file and rename.
pub fn save_configs(path: &Path, sources: &[SourceConfig]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "create placefile settings folder {}: {error}",
                parent.display()
            )
        })?;
    }

    let mut document = serde_json::to_vec_pretty(sources)
        .map_err(|error| format!("serialize placefile settings: {error}"))?;
    document.push(b'\n');
    let temporary = temporary_settings_path(path);
    let outcome = (|| -> io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(&document)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if let Err(error) = outcome {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "save placefile settings {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

fn temporary_settings_path(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp{}", std::process::id()))
}

fn read_remote_bytes(url: &str, kind: &str) -> Result<Vec<u8>, String> {
    let response = super::download_http_client()
        .get(url)
        .send()
        .map_err(|error| format!("request {kind} {url}: {error}"))?
        .error_for_status()
        .map_err(|error| format!("request {kind} {url}: {error}"))?;
    read_bytes(response, kind, url)
}

fn read_local_bytes(path: &Path, kind: &str) -> Result<Vec<u8>, String> {
    let file =
        File::open(path).map_err(|error| format!("read {kind} {}: {error}", path.display()))?;
    read_bytes(file, kind, &path.to_string_lossy())
}

fn read_bytes(mut reader: impl Read, kind: &str, source: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {kind} {source}: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "genericradar-placefile-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ))
    }

    #[test]
    fn configurations_round_trip_and_legacy_url_fields_still_load() {
        let path = temporary_path("settings.json");
        let configured = vec![SourceConfig {
            source: "https://example.test/community.txt".to_owned(),
            enabled: false,
            show_text: false,
            visibility_range_percent: 400,
        }];
        save_configs(&path, &configured).expect("save sources");
        assert_eq!(load_configs(&path).expect("restore sources"), configured);
        let changed = vec![SourceConfig::new("https://example.test/updated.txt")];
        save_configs(&path, &changed).expect("replace existing source settings");
        assert_eq!(
            load_configs(&path).expect("restore updated sources"),
            changed
        );
        fs::write(&path, r#"[{"url":"C:\\\\maps\\\\storm.txt"}]"#).expect("write legacy settings");
        let restored = load_configs(&path).expect("restore legacy source");
        assert!(restored[0].enabled);
        assert!(restored[0].show_text);
        assert_eq!(restored[0].visibility_range_percent, 100);
        fs::remove_file(path).expect("remove test settings");
    }

    #[test]
    fn local_text_accepts_bom_but_rejects_binary_data() {
        let path = temporary_path("text");
        fs::write(&path, b"\xEF\xBB\xBFTitle: Nearby observers\n").expect("write text placefile");
        let source = path.to_string_lossy();
        assert_eq!(
            load_source_text(&source).expect("read local placefile"),
            "Title: Nearby observers\n"
        );
        fs::write(&path, b"Title:\0binary").expect("write binary source");
        assert!(load_source_text(&source).unwrap_err().contains("NUL"));
        fs::remove_file(path).expect("remove test source");
    }

    #[test]
    fn icon_decoder_returns_real_rgba_pixels() {
        let pixels = image::RgbaImage::from_pixel(2, 1, image::Rgba([25, 50, 75, 200]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode PNG");
        let decoded = decode_icon_image(encoded.get_ref()).expect("decode sprite sheet");
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(&decoded.rgba[..4], &[25, 50, 75, 200]);
    }
}
