//! GRLevelX-style placefile support: the community-standard overlay format
//! (SpotterNetwork positions, chaser feeds, mesoanalysis contours, local
//! storm reports). This module parses the text format into geolocated draw
//! objects; fetching runs on background threads and drawing goes through the
//! pane's globe-aware map projection. Network access and image decoding run
//! on workers; the last successful overlay remains visible while it refreshes.
//!
//! Supported: Title, Refresh, Color, Threshold, Font, Place, Text, Icon
//! (with real IconFile sprite sheets, fetched and sliced), Line, Polygon,
//! and `Object:` blocks (statements inside draw at pixel offsets from the
//! anchor, +x east / +y north, per the GR convention).

use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc::{self, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use analyst_runtime::{Camera2D, ViewportMetrics};
use chrono::{DateTime, Utc};
use data_source::placefiles::{DecodedIconImage, SourceConfig};
use eframe::egui;
use map_scene::RadarProjection;

/// HTTP(S) sources use the existing community-feed client; everything else
/// is treated as a literal local filesystem path. Local paths are persisted
/// exactly as ordinary absolute paths rather than converted to fragile
/// `file://` URLs (notably important for Windows drive and UNC paths).
pub fn is_remote_source(source: &str) -> bool {
    let source = source.trim();
    source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || source
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

/// Normalize a file-picker result into the absolute path stored in settings.
/// Canonicalization is best-effort so network shares and temporarily
/// unavailable removable media can still remain in the user's layer list.
pub fn persistent_local_source(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    absolute
        .canonicalize()
        .unwrap_or(absolute)
        .to_string_lossy()
        .into_owned()
}

/// Compact pre-parse label for the layer rail. Parsed `Title:` replaces it
/// after load; local files should not expose an entire absolute path as the
/// row title (the full source remains in the hover).
pub fn source_display_name(source: &str) -> String {
    if is_remote_source(source) {
        return source.to_owned();
    }
    Path::new(source)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(source)
        .to_owned()
}

/// Read a downloaded placefile with a text-file sanity check and no artificial size ceiling.
/// GR placefiles are overwhelmingly ASCII; lossy UTF-8 also keeps older
/// Windows-encoded labels usable without making the map layer fail.
pub fn read_local_placefile(source: &str) -> Result<String, String> {
    let bytes = read_local_bytes(source, "placefile")?;
    if bytes.contains(&0) {
        return Err(format!(
            "local placefile contains NUL bytes (expected text): {source}"
        ));
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// Read a sprite sheet referenced by a downloaded placefile. Relative paths
/// have already been resolved beside the text file by `resolve_url`.
pub fn read_local_icon(source: &str) -> Result<Vec<u8>, String> {
    read_local_bytes(source, "placefile icon")
}

fn read_local_bytes(source: &str, kind: &str) -> Result<Vec<u8>, String> {
    let mut file = File::open(source).map_err(|error| format!("read {kind} {source}: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {kind} {source}: {error}"))?;
    Ok(bytes)
}

/// One parsed placefile.
#[derive(Clone, Debug, Default)]
pub struct Placefile {
    pub title: String,
    pub refresh_minutes: u32,
    /// Precise cadence, including GR `RefreshSeconds:` sources.
    pub refresh_seconds: u32,
    pub objects: Vec<PlacefileObject>,
    /// Icon sprite sheets referenced by Icon statements.
    pub icon_sheets: Vec<IconSheetSpec>,
    /// Unrecognized statements (for the honest status line).
    pub skipped: usize,
}

/// `IconFile: index, iconWidth, iconHeight, hotX, hotY, url`
#[derive(Clone, Debug, PartialEq)]
pub struct IconSheetSpec {
    pub index: u32,
    pub icon_w: u32,
    pub icon_h: u32,
    pub hot_x: f32,
    pub hot_y: f32,
    pub url: String,
}

/// When `anchor` is Some, positional fields hold PIXEL OFFSETS from the
/// anchor's screen position (+x east, +y north) instead of lat/lon — the
/// `Object:` block convention used for station plots.
#[derive(Clone, Debug)]
pub enum PlacefileObject {
    Icon {
        lat: f32,
        lon: f32,
        anchor: Option<(f32, f32)>,
        heading_deg: f32,
        file_index: u32,
        icon_index: u32,
        label: Option<String>,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Text {
        lat: f32,
        lon: f32,
        anchor: Option<(f32, f32)>,
        size_px: f32,
        text: String,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Line {
        width: f32,
        points: Vec<(f32, f32)>, // (lat, lon) — or px offsets when anchored
        anchor: Option<(f32, f32)>,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Polygon {
        points: Vec<(f32, f32)>,
        anchor: Option<(f32, f32)>,
        color: [u8; 3],
        threshold_nm: f32,
    },
}

impl PlacefileObject {
    pub fn threshold_nm(&self) -> f32 {
        match self {
            Self::Icon { threshold_nm, .. }
            | Self::Text { threshold_nm, .. }
            | Self::Line { threshold_nm, .. }
            | Self::Polygon { threshold_nm, .. } => *threshold_nm,
        }
    }
}

/// Parse placefile text. Tolerant: unknown statements are ignored, malformed
/// lines are skipped, and a file with no recognized objects still returns
/// (with `objects` empty) so the UI can show an honest status.
pub fn parse_placefile(text: &str, base_url: &str) -> Placefile {
    parse_placefile_at(text, base_url, Utc::now())
}

/// Parse against a historical radar-frame timestamp rather than wall time.
pub fn parse_placefile_at(text: &str, base_url: &str, reference_time: DateTime<Utc>) -> Placefile {
    let mut out = Placefile {
        title: String::new(),
        refresh_minutes: 5,
        refresh_seconds: 300,
        objects: Vec::new(),
        icon_sheets: Vec::new(),
        skipped: 0,
    };
    let mut color: [u8; 3] = [255, 255, 255];
    let mut threshold_nm: f32 = 999.0;
    // TimeRange gating (parse-time approximation: the file refreshes on
    // its cadence, so out-of-window items reappear within one refresh of
    // entering their window — matches GR semantics to that bound).
    let mut in_time_window = true;
    let mut hsluv_mode = false;
    let mut fonts: Vec<(u32, f32)> = Vec::new();
    let mut pending_line: Option<(f32, Vec<(f32, f32)>)> = None;
    let mut pending_polygon: Option<Vec<(f32, f32)>> = None;
    let mut object_anchor: Option<(f32, f32)> = None;

    for raw_line in logical_lines(text) {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with("//") {
            continue;
        }
        let (key, value) = match line.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (String::new(), line),
        };

        // Coordinate rows belong to an open Line/Polygon. (Inside an Object
        // block these are pixel offsets; validation is relaxed accordingly.)
        if key.is_empty() || key.parse::<f64>().is_ok() {
            if let Some(pair) = parse_pair(line, object_anchor.is_some())
                && let Some(sink) = pending_line
                    .as_mut()
                    .map(|(_, points)| points)
                    .or(pending_polygon.as_mut())
            {
                sink.push(pair);
            }
            continue;
        }

        match key.as_str() {
            "title" => out.title = value.to_owned(),
            "refresh" => {
                if let Ok(minutes) = value.parse::<u32>() {
                    out.refresh_minutes = minutes.max(1);
                    out.refresh_seconds = out.refresh_minutes.saturating_mul(60);
                }
            }
            "refreshseconds" => {
                // Preserve the exact source cadence rather than rounding away live updates.
                if let Ok(seconds) = value.parse::<u32>() {
                    out.refresh_seconds = seconds.max(1);
                    out.refresh_minutes = out.refresh_seconds.div_ceil(60);
                }
            }
            "timerange" => {
                // TimeRange: YYYY-MM-DDThh:mm:ss YYYY-MM-DDThh:mm:ss (UTC)
                let mut parts = value.split_whitespace();
                let parse_t =
                    |t: &str| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S").ok();
                if let (Some(start), Some(end)) = (
                    parts.next().and_then(parse_t),
                    parts.next().and_then(parse_t),
                ) {
                    let current = reference_time.naive_utc();
                    in_time_window = start <= end && current >= start && current <= end;
                } else {
                    // A malformed availability window must never invent observations.
                    in_time_window = false;
                    out.skipped += 1;
                }
            }
            "hsluv" => {
                hsluv_mode = value.eq_ignore_ascii_case("true");
            }
            "color" => {
                if hsluv_mode {
                    let parts: Vec<f64> = value
                        .split_whitespace()
                        .filter_map(|p| p.parse::<f64>().ok())
                        .collect();
                    if parts.len() >= 3 {
                        color = hsluv_to_rgb(parts[0], parts[1], parts[2]);
                    }
                } else {
                    let parts: Vec<u8> = value
                        .split_whitespace()
                        .filter_map(|p| p.parse::<u8>().ok())
                        .collect();
                    if parts.len() >= 3 {
                        color = [parts[0], parts[1], parts[2]];
                    }
                }
            }
            "threshold" => {
                if let Ok(nm) = value.parse::<f32>() {
                    threshold_nm = nm.max(0.0);
                }
            }
            "font" => {
                let parts: Vec<&str> = value.split(',').collect();
                if parts.len() >= 2
                    && let (Ok(id), Ok(px)) = (
                        parts[0].trim().parse::<u32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    fonts.retain(|(existing, _)| *existing != id);
                    fonts.push((id, px.clamp(7.0, 32.0)));
                }
            }
            "iconfile" => {
                // IconFile: index, width, height, hotX, hotY, url
                let parts: Vec<&str> = value.splitn(6, ',').collect();
                if parts.len() == 6
                    && let (Ok(index), Ok(w), Ok(h)) = (
                        parts[0].trim().parse::<u32>(),
                        parts[1].trim().parse::<u32>(),
                        parts[2].trim().parse::<u32>(),
                    )
                {
                    let hot_x = parts[3].trim().parse::<f32>().unwrap_or(0.0);
                    let hot_y = parts[4].trim().parse::<f32>().unwrap_or(0.0);
                    let url = resolve_url(base_url, &unquote(parts[5]));
                    if w > 0 && h > 0 && (is_remote_source(&url) || Path::new(&url).is_absolute()) {
                        out.icon_sheets.retain(|sheet| sheet.index != index);
                        out.icon_sheets.push(IconSheetSpec {
                            index,
                            icon_w: w,
                            icon_h: h,
                            hot_x,
                            hot_y,
                            url,
                        });
                    }
                }
            }
            "icon" => {
                // Icon: lat, lon, angle, fileNumber, iconNumber [, hover]
                let parts: Vec<&str> = value.splitn(6, ',').collect();
                if parts.len() >= 5
                    && let Some((lat, lon)) = parse_first_pair(&parts, object_anchor.is_some())
                {
                    let heading = parts[2].trim().parse::<f32>().unwrap_or(0.0);
                    let file_index = parts[3].trim().parse::<u32>().unwrap_or(0);
                    let icon_index = parts[4].trim().parse::<u32>().unwrap_or(1);
                    let label = parts.get(5).map(|s| unquote(s)).filter(|s| !s.is_empty());
                    push_item(
                        &mut out,
                        in_time_window,
                        PlacefileObject::Icon {
                            lat,
                            lon,
                            anchor: object_anchor,
                            heading_deg: heading,
                            file_index,
                            icon_index,
                            label,
                            color,
                            threshold_nm,
                        },
                    );
                }
            }
            "text" => {
                // Text: lat, lon, fontNumber, "string" [, "hover"]
                let parts: Vec<&str> = value.splitn(4, ',').collect();
                if parts.len() >= 4
                    && let Some((lat, lon)) = parse_first_pair(&parts, object_anchor.is_some())
                {
                    let font_id = parts[2].trim().parse::<u32>().unwrap_or(1);
                    let size = fonts
                        .iter()
                        .find(|(id, _)| *id == font_id)
                        .map(|(_, px)| *px)
                        .unwrap_or(11.0);
                    let text = unquote(parts[3].split(',').next().unwrap_or(parts[3]));
                    if !text.is_empty() {
                        push_item(
                            &mut out,
                            in_time_window,
                            PlacefileObject::Text {
                                lat,
                                lon,
                                anchor: object_anchor,
                                size_px: size,
                                text,
                                color,
                                threshold_nm,
                            },
                        );
                    }
                }
            }
            "place" => {
                let parts: Vec<&str> = value.splitn(3, ',').collect();
                if parts.len() >= 3
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    push_item(
                        &mut out,
                        in_time_window,
                        PlacefileObject::Text {
                            lat,
                            lon,
                            anchor: None,
                            size_px: 11.0,
                            text: unquote(parts[2]),
                            color,
                            threshold_nm,
                        },
                    );
                }
            }
            "line" => {
                let width = value
                    .split(',')
                    .next()
                    .and_then(|w| w.trim().parse::<f32>().ok())
                    .unwrap_or(1.5)
                    .clamp(0.5, 8.0);
                pending_line = Some((width, Vec::new()));
            }
            "polygon" => pending_polygon = Some(Vec::new()),
            "object" => {
                // Object: lat, lon — subsequent coordinates are pixel offsets.
                let parts: Vec<&str> = value.splitn(2, ',').collect();
                if parts.len() == 2
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    object_anchor = Some((lat, lon));
                } else {
                    out.skipped += 1;
                }
            }
            "end" => {
                // End: closes the innermost construct: open geometry first,
                // then the Object block.
                if let Some((width, points)) = pending_line.take() {
                    if points.len() >= 2 {
                        push_item(
                            &mut out,
                            in_time_window,
                            PlacefileObject::Line {
                                width,
                                points,
                                anchor: object_anchor,
                                color,
                                threshold_nm,
                            },
                        );
                    }
                } else if let Some(points) = pending_polygon.take() {
                    if points.len() >= 3 {
                        push_item(
                            &mut out,
                            in_time_window,
                            PlacefileObject::Polygon {
                                points,
                                anchor: object_anchor,
                                color,
                                threshold_nm,
                            },
                        );
                    }
                } else {
                    object_anchor = None;
                }
            }
            _ => out.skipped += 1,
        }
    }
    if let Some((width, points)) = pending_line.take()
        && points.len() >= 2
    {
        push_item(
            &mut out,
            in_time_window,
            PlacefileObject::Line {
                width,
                points,
                anchor: object_anchor,
                color,
                threshold_nm,
            },
        );
    }
    if let Some(points) = pending_polygon.take()
        && points.len() >= 3
    {
        push_item(
            &mut out,
            in_time_window,
            PlacefileObject::Polygon {
                points,
                anchor: object_anchor,
                color,
                threshold_nm,
            },
        );
    }
    out
}

/// Parse the first two comma fields as a coordinate pair. In geo mode the
/// pair is validated as lat/lon; in offset (Object) mode all finite pixel
/// positions remain valid, including large professional multi-display views.
fn parse_first_pair(parts: &[&str], offsets: bool) -> Option<(f32, f32)> {
    let a = parts.first()?.trim().parse::<f32>().ok()?;
    let b = parts.get(1)?.trim().parse::<f32>().ok()?;
    pair_valid(a, b, offsets).then_some((a, b))
}

fn parse_pair(line: &str, offsets: bool) -> Option<(f32, f32)> {
    let mut parts = line.split(',');
    let a = parts.next()?.trim().parse::<f32>().ok()?;
    let b = parts.next()?.trim().parse::<f32>().ok()?;
    pair_valid(a, b, offsets).then_some((a, b))
}

fn pair_valid(a: f32, b: f32, offsets: bool) -> bool {
    if offsets {
        a.is_finite() && b.is_finite()
    } else {
        (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&b)
    }
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').trim().to_owned()
}

fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for raw in text.lines() {
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(raw);
        update_quote_state(raw, &mut in_quotes);
        if !in_quotes {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn update_quote_state(line: &str, in_quotes: &mut bool) {
    let mut escaped = false;
    for ch in line.chars() {
        if ch == '\\' && !escaped {
            escaped = true;
            continue;
        }
        if ch == '"' && !escaped {
            *in_quotes = !*in_quotes;
        }
        escaped = false;
    }
}

/// Push an item unless the active TimeRange excludes the current moment.
fn push_item(out: &mut Placefile, in_time_window: bool, object: PlacefileObject) {
    if in_time_window {
        out.objects.push(object);
    } else {
        out.skipped += 1;
    }
}

/// Resolve a possibly-relative IconFile URL against the placefile URL
/// ("icons/x.png" loads from the placefile's host).
fn resolve_url(base_url: &str, raw: &str) -> String {
    if is_remote_source(raw) || base_url.is_empty() {
        return raw.to_owned();
    }
    if is_remote_source(base_url)
        && let Some(scheme_end) = base_url.find("://")
    {
        let host_start = scheme_end + 3;
        if raw.starts_with('/') {
            // Host-absolute path.
            let host_end = base_url[host_start..]
                .find('/')
                .map(|i| host_start + i)
                .unwrap_or(base_url.len());
            return format!("{}{}", &base_url[..host_end], raw);
        }
        // Relative to the placefile's directory.
        let dir_end = base_url.rfind('/').filter(|&i| i > host_start);
        if let Some(end) = dir_end {
            return format!("{}/{}", &base_url[..end], raw);
        }
        return format!("{base_url}/{raw}");
    }

    // A downloaded placefile may keep its IconFile beside the text file.
    // Resolve that reference with platform-native path rules; absolute drive,
    // UNC, POSIX, and already-rooted paths pass through unchanged.
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        return raw_path.to_string_lossy().into_owned();
    }
    Path::new(base_url)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(raw_path)
        .to_string_lossy()
        .into_owned()
}

// ---- HSLuv -> sRGB (reference implementation; www.hsluv.org) ----
// H in [0,360], S and L in [0,100]. Needed because GR placefiles can
// switch Color: into HSLuv space via the "HSLuv: true" directive.

const HSLUV_M: [[f64; 3]; 3] = [
    [3.240969941904521, -1.537383177570093, -0.498610760293],
    [-0.96924363628087, 1.87596750150772, 0.041555057407175],
    [0.055630079696993, -0.20397695888897, 1.056971514242878],
];
const HSLUV_REF_U: f64 = 0.19783000664283;
const HSLUV_REF_V: f64 = 0.46831999493879;
const HSLUV_KAPPA: f64 = 903.2962962;
const HSLUV_EPS: f64 = 0.0088564516;

fn hsluv_max_chroma(l: f64, h_deg: f64) -> f64 {
    let hrad = h_deg.to_radians();
    let sub1 = (l + 16.0).powi(3) / 1_560_896.0;
    let sub2 = if sub1 > HSLUV_EPS {
        sub1
    } else {
        l / HSLUV_KAPPA
    };
    let mut min_len = f64::MAX;
    for m in &HSLUV_M {
        for t in 0..2 {
            let t = t as f64;
            let top1 = (284_517.0 * m[0] - 94_839.0 * m[2]) * sub2;
            let top2 = (838_422.0 * m[2] + 769_860.0 * m[1] + 731_718.0 * m[0]) * l * sub2
                - 769_860.0 * t * l;
            let bottom = (632_260.0 * m[2] - 126_452.0 * m[1]) * sub2 + 126_452.0 * t;
            if bottom.abs() < 1e-12 {
                continue;
            }
            let slope = top1 / bottom;
            let intercept = top2 / bottom;
            let denom = hrad.sin() - slope * hrad.cos();
            if denom.abs() < 1e-12 {
                continue;
            }
            let len = intercept / denom;
            if len >= 0.0 {
                min_len = min_len.min(len);
            }
        }
    }
    min_len
}

fn hsluv_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    let l = l.clamp(0.0, 100.0);
    let s = s.clamp(0.0, 100.0);
    if l > 99.999 {
        return [255, 255, 255];
    }
    if l < 0.001 {
        return [0, 0, 0];
    }
    // LCh
    let c = hsluv_max_chroma(l, h) / 100.0 * s;
    let hrad = h.to_radians();
    let (u, v) = (c * hrad.cos(), c * hrad.sin());
    // Luv -> XYZ
    let var_y = if l > 8.0 {
        ((l + 16.0) / 116.0).powi(3)
    } else {
        l / HSLUV_KAPPA
    };
    let var_u = u / (13.0 * l) + HSLUV_REF_U;
    let var_v = v / (13.0 * l) + HSLUV_REF_V;
    let y = var_y;
    let x = -(9.0 * y * var_u) / ((var_u - 4.0) * var_v - var_u * var_v);
    let z = (9.0 * y - 15.0 * var_v * y - var_v * x) / (3.0 * var_v);
    // XYZ -> sRGB
    let gamma = |c: f64| -> f64 {
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let channel = |m: &[f64; 3]| -> u8 {
        (gamma(m[0] * x + m[1] * y + m[2] * z).clamp(0.0, 1.0) * 255.0).round() as u8
    };
    [
        channel(&HSLUV_M[0]),
        channel(&HSLUV_M[1]),
        channel(&HSLUV_M[2]),
    ]
}

const PLACEFILE_CONFIG_NAME: &str = "placefiles.json";
const SPOTTER_NETWORK_REPORTS_URL: &str = "https://www.spotternetwork.org/feeds/reports.txt";
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// One independently refreshable community feed or downloaded placefile.
pub struct PlacefileLayer {
    pub source: String,
    pub enabled: bool,
    pub show_text: bool,
    pub visibility_range_percent: u16,
    pub data: Option<Placefile>,
    pub status: String,
    pub generation: u64,
    pub last_successful_load: Option<Instant>,
    source_text: Option<String>,
    source_generation: u64,
    next_refresh: Option<Instant>,
    source_receiver: Option<mpsc::Receiver<Result<SourceBatch, String>>>,
    icon_receiver: Option<mpsc::Receiver<IconBatch>>,
    icons_generation: Option<u64>,
    sheets: Vec<IconSheet>,
}

struct SourceBatch {
    text: String,
    parsed: Placefile,
}

struct IconBatch {
    source_generation: u64,
    decoded: Vec<(IconSheetSpec, Result<DecodedIconImage, String>)>,
}

struct IconSheet {
    spec: IconSheetSpec,
    width: u32,
    height: u32,
    texture: egui::TextureHandle,
}

impl PlacefileLayer {
    fn from_config(config: SourceConfig) -> Self {
        Self {
            source: config.source,
            enabled: config.enabled,
            show_text: config.show_text,
            visibility_range_percent: config.visibility_range_percent.max(1),
            data: None,
            status: "Waiting to load".to_owned(),
            generation: 0,
            last_successful_load: None,
            source_text: None,
            source_generation: 0,
            next_refresh: None,
            source_receiver: None,
            icon_receiver: None,
            icons_generation: None,
            sheets: Vec::new(),
        }
    }

    fn config(&self) -> SourceConfig {
        SourceConfig {
            source: self.source.clone(),
            enabled: self.enabled,
            show_text: self.show_text,
            visibility_range_percent: self.visibility_range_percent,
        }
    }

    fn title(&self) -> String {
        self.data
            .as_ref()
            .map(|data| data.title.trim())
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| source_display_name(&self.source))
    }

    fn install_source(&mut self, batch: SourceBatch, now: Instant) {
        let interval = Duration::from_secs(u64::from(batch.parsed.refresh_seconds.max(1)));
        self.status = parse_status(&batch.parsed);
        self.source_text = Some(batch.text);
        self.data = Some(batch.parsed);
        self.last_successful_load = Some(now);
        self.next_refresh = now.checked_add(interval);
        self.source_generation = self.source_generation.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
        self.icons_generation = None;
        // Keep existing textures until replacements are ready. Their specs
        // are checked at draw time so a changed index cannot show stale art.
    }

    fn reparse(&mut self, reference_time: DateTime<Utc>) -> bool {
        let Some(text) = self.source_text.as_ref() else {
            return false;
        };
        let has_time_range = text.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(directive, _)| directive.trim().eq_ignore_ascii_case("timerange"))
        });
        if !has_time_range {
            return false;
        }
        let parsed = parse_placefile_at(text, &self.source, reference_time);
        self.status = parse_status(&parsed);
        self.data = Some(parsed);
        self.generation = self.generation.wrapping_add(1);
        // Only object visibility changed. Existing source/icon generations
        // and GPU textures remain valid throughout historical playback.
        true
    }
}

/// Persistent, nonblocking placefile layers shared by all radar panes.
pub struct PlacefileManager {
    pub layers: Vec<PlacefileLayer>,
    config_path: PathBuf,
    source_input: String,
    notice: Option<String>,
    reference_time: Option<DateTime<Utc>>,
}

impl PlacefileManager {
    /// Restore the user's layer list without enabling unsolicited feeds.
    pub fn load() -> Self {
        let config_path = settings::app_config_root().join(PLACEFILE_CONFIG_NAME);
        let (layers, notice) = match data_source::placefiles::load_configs(&config_path) {
            Ok(configurations) => (
                configurations
                    .into_iter()
                    .filter(|configuration| !configuration.source.trim().is_empty())
                    .map(PlacefileLayer::from_config)
                    .collect(),
                None,
            ),
            Err(error) => (Vec::new(), Some(error)),
        };
        Self {
            layers,
            config_path,
            source_input: String::new(),
            notice,
            reference_time: None,
        }
    }

    /// Add an HTTP(S) community feed or an absolute/relative local path.
    pub fn add_source(&mut self, source: String) -> bool {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return false;
        }
        let normalized = if is_remote_source(trimmed) {
            trimmed.to_owned()
        } else {
            persistent_local_source(Path::new(trimmed))
        };
        if let Some(index) = self
            .layers
            .iter()
            .position(|layer| layer.source == normalized)
        {
            self.layers[index].enabled = true;
            self.request_refresh(index);
            self.persist();
            return true;
        }
        self.layers
            .push(PlacefileLayer::from_config(SourceConfig::new(normalized)));
        self.persist();
        true
    }

    /// Add a file selected by a native picker or dragged onto the application.
    pub fn add_path(&mut self, path: &Path) -> bool {
        self.add_source(persistent_local_source(path))
    }

    /// Remove one source without deleting the original file or touching its URL.
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.layers.len() {
            return false;
        }
        self.layers.remove(index);
        self.persist();
        true
    }

    /// Schedule a refresh immediately while retaining the previous overlay.
    pub fn request_refresh(&mut self, index: usize) {
        if let Some(layer) = self.layers.get_mut(index) {
            layer.next_refresh = None;
            layer.status = if layer.data.is_some() {
                "Refreshing; previous overlay remains visible".to_owned()
            } else {
                "Waiting to load".to_owned()
            };
        }
    }

    /// Refresh all layers; requests remain asynchronous and individually isolated.
    pub fn refresh_all(&mut self) {
        for index in 0..self.layers.len() {
            self.request_refresh(index);
        }
    }

    /// Align GR `TimeRange` windows with historical radar playback.
    ///
    /// Cached source text is reparsed only when the timestamp changes; no
    /// network request is made and the existing icon textures stay resident.
    pub fn set_reference_time(&mut self, time: Option<DateTime<Utc>>) -> bool {
        if self.reference_time == time {
            return false;
        }
        self.reference_time = time;
        let reference = time.unwrap_or_else(Utc::now);
        let mut changed = false;
        for layer in &mut self.layers {
            changed |= layer.reparse(reference);
        }
        changed
    }

    /// Install completed worker results, start due refreshes and wake on demand.
    ///
    /// Successful data and textures are never cleared for an in-flight request
    /// or a failed refresh, preventing the flashing common to feed overlays.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for layer in &mut self.layers {
            changed |= poll_source(layer, now, self.reference_time);
            changed |= poll_icons(layer, ctx);
            changed |= reuse_matching_icons(layer);

            if layer.icon_receiver.is_none()
                && layer.enabled
                && layer.data.is_some()
                && layer.icons_generation != Some(layer.source_generation)
            {
                schedule_icons(layer, ctx);
            }

            let due = if layer.enabled {
                layer.next_refresh.is_none_or(|deadline| now >= deadline)
            } else {
                layer.data.is_none() && layer.next_refresh.is_none_or(|deadline| now >= deadline)
            };
            if due && layer.source_receiver.is_none() {
                schedule_source(layer, ctx, self.reference_time);
            }

            if layer.enabled
                && let Some(deadline) = layer.next_refresh
                && deadline > now
            {
                ctx.request_repaint_after(deadline.duration_since(now));
            }
        }
        if changed {
            ctx.request_repaint();
        }
        changed
    }

    /// Full source manager, suitable for a layer window or settings section.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.label("Add a community placefile URL or a local file path.");
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.source_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("HTTPS URL or full path to a local placefile"),
            );
            let submitted =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if ui.button("Add source").clicked() || submitted {
                let source = self.source_input.trim().to_owned();
                if self.add_source(source) {
                    self.source_input.clear();
                    changed = true;
                }
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Add Spotter Network reports").clicked() {
                changed |= self.add_source(SPOTTER_NETWORK_REPORTS_URL.to_owned());
            }
            if !self.layers.is_empty() && ui.button("Refresh all").clicked() {
                self.refresh_all();
                changed = true;
            }
        });

        if let Some(notice) = &self.notice {
            ui.colored_label(ui.visuals().warn_fg_color, notice);
        }
        if self.layers.is_empty() {
            ui.add_space(6.0);
            ui.weak("No placefile sources have been added.");
            return changed;
        }

        ui.separator();
        let mut remove_index = None;
        let mut refresh_index = None;
        let mut preferences_changed = false;
        for (index, layer) in self.layers.iter_mut().enumerate() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    preferences_changed |= ui.checkbox(&mut layer.enabled, "").changed();
                    ui.strong(layer.title()).on_hover_text(&layer.source);
                    if ui.small_button("Refresh").clicked() {
                        refresh_index = Some(index);
                    }
                    if ui.small_button("Remove").clicked() {
                        remove_index = Some(index);
                    }
                });
                ui.horizontal(|ui| {
                    preferences_changed |= ui.checkbox(&mut layer.show_text, "Labels").changed();
                    ui.label("Visible:");
                    egui::ComboBox::from_id_salt(("placefile_visibility_range", index))
                        .selected_text(visibility_range_label(layer.visibility_range_percent))
                        .show_ui(ui, |ui| {
                            for (value, label) in [
                                (100, "Source range"),
                                (200, "2× farther"),
                                (400, "4× farther"),
                                (800, "8× farther"),
                                (u16::MAX, "Always"),
                            ] {
                                preferences_changed |= ui
                                    .selectable_value(
                                        &mut layer.visibility_range_percent,
                                        value,
                                        label,
                                    )
                                    .changed();
                            }
                        });
                });
                let age = layer
                    .last_successful_load
                    .map(|loaded| format!(" · updated {} ago", human_age(loaded.elapsed())));
                ui.weak(format!("{}{}", layer.status, age.unwrap_or_default()));
            });
        }
        if preferences_changed {
            self.persist();
            changed = true;
        }
        if let Some(index) = refresh_index {
            self.request_refresh(index);
            changed = true;
        }
        if let Some(index) = remove_index {
            changed |= self.remove(index);
        }
        changed
    }

    /// Paint genuine GR sprites, vectors and labels in the pane's projection.
    ///
    /// Both the familiar flat radar view and the orthographic globe use the
    /// same transform as basemap features; far-side objects cannot leak across
    /// the horizon and all drawing stays clipped to this radar pane.
    pub fn draw(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        camera: Camera2D,
        viewport: ViewportMetrics,
        projection: Option<&RadarProjection>,
    ) {
        let Some(projection) = projection else {
            return;
        };
        let camera = camera.sanitized();
        let viewport = viewport.sanitized();
        let blend = map_scene::projection::globe::blend_for_pane(camera.km_per_point, viewport);
        let painter = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
        let visible_nm = rect.width() * camera.km_per_point / 1.852;
        let hover = painter
            .ctx()
            .pointer_hover_pos()
            .filter(|point| rect.contains(*point));
        let mut hovered_label = None;

        let project = |lat: f32, lon: f32, anchor: Option<(f32, f32)>| {
            let (latitude, longitude) = anchor.unwrap_or((lat, lon));
            let world = projection.try_lon_lat_to_globe(
                f64::from(longitude),
                f64::from(latitude),
                blend,
            )?;
            let screen = camera.world_to_screen(world, viewport);
            let mut position = egui::pos2(rect.left() + screen.x, rect.top() + screen.y);
            if anchor.is_some() {
                // GR Object coordinates are pixel offsets: +x east, +y north.
                position += egui::vec2(lat, -lon);
            }
            Some(position)
        };

        for layer in self.layers.iter().filter(|layer| layer.enabled) {
            let Some(data) = layer.data.as_ref() else {
                continue;
            };
            for object in &data.objects {
                let threshold = if layer.visibility_range_percent == u16::MAX {
                    f32::INFINITY
                } else {
                    object.threshold_nm() * f32::from(layer.visibility_range_percent) / 100.0
                };
                if visible_nm > threshold {
                    continue;
                }
                match object {
                    PlacefileObject::Icon {
                        lat,
                        lon,
                        anchor,
                        heading_deg,
                        file_index,
                        icon_index,
                        label,
                        color,
                        ..
                    } => {
                        let Some(position) = project(*lat, *lon, *anchor) else {
                            continue;
                        };
                        if !rect.expand(64.0).contains(position) {
                            continue;
                        }
                        let sheet = data
                            .icon_sheets
                            .iter()
                            .find(|spec| spec.index == *file_index)
                            .and_then(|spec| layer.sheets.iter().find(|sheet| sheet.spec == *spec));
                        if let Some(shape) = sheet.and_then(|sheet| {
                            icon_sprite_shape(sheet, *icon_index, position, *heading_deg, 1.0)
                        }) {
                            painter.add(shape);
                        } else {
                            // Only the genuine sheet is preferred. A compact marker
                            // remains useful while its background download finishes.
                            painter.circle_filled(position, 3.5, placefile_color(*color));
                            painter.circle_stroke(
                                position,
                                4.5,
                                egui::Stroke::new(1.0, egui::Color32::BLACK),
                            );
                        }
                        if let (Some(cursor), Some(label)) = (hover, label)
                            && cursor.distance(position) <= 12.0
                        {
                            hovered_label = Some((cursor, label.as_str()));
                        }
                    }
                    PlacefileObject::Text {
                        lat,
                        lon,
                        anchor,
                        size_px,
                        text,
                        color,
                        ..
                    } => {
                        if !layer.show_text {
                            continue;
                        }
                        let Some(position) = project(*lat, *lon, *anchor) else {
                            continue;
                        };
                        if !rect.expand(96.0).contains(position) {
                            continue;
                        }
                        let font = egui::FontId::proportional((*size_px).max(1.0));
                        for offset in [
                            egui::vec2(-1.0, 0.0),
                            egui::vec2(1.0, 0.0),
                            egui::vec2(0.0, -1.0),
                            egui::vec2(0.0, 1.0),
                        ] {
                            painter.text(
                                position + offset,
                                egui::Align2::CENTER_CENTER,
                                text,
                                font.clone(),
                                egui::Color32::BLACK,
                            );
                        }
                        painter.text(
                            position,
                            egui::Align2::CENTER_CENTER,
                            text,
                            font,
                            placefile_color(*color),
                        );
                    }
                    PlacefileObject::Line {
                        width,
                        points,
                        anchor,
                        color,
                        ..
                    } => {
                        let mut visible = Vec::new();
                        for &(lat, lon) in points {
                            if let Some(position) = project(lat, lon, *anchor) {
                                visible.push(position);
                            } else {
                                draw_line_segment(
                                    &painter,
                                    &mut visible,
                                    *width,
                                    placefile_color(*color),
                                );
                            }
                        }
                        draw_line_segment(&painter, &mut visible, *width, placefile_color(*color));
                    }
                    PlacefileObject::Polygon {
                        points,
                        anchor,
                        color,
                        ..
                    } => {
                        let Some(projected) = points
                            .iter()
                            .map(|&(lat, lon)| project(lat, lon, *anchor))
                            .collect::<Option<Vec<_>>>()
                        else {
                            // Closing a partly hidden polygon would draw a false
                            // chord across the visible side of the globe.
                            continue;
                        };
                        if projected.len() < 3 {
                            continue;
                        }
                        let stroke = egui::Stroke::new(1.3, placefile_color(*color));
                        let fill =
                            egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], 52);
                        painter.add(egui::Shape::convex_polygon(projected, fill, stroke));
                    }
                }
            }
        }

        if let Some((cursor, label)) = hovered_label {
            let offset = cursor + egui::vec2(12.0, 12.0);
            let font = egui::FontId::proportional(12.0);
            let galley = painter.layout_no_wrap(label.to_owned(), font, egui::Color32::WHITE);
            let bounds = egui::Rect::from_min_size(offset, galley.size()).expand(5.0);
            painter.rect_filled(bounds, 4.0, egui::Color32::from_black_alpha(230));
            painter.galley(offset, galley, egui::Color32::WHITE);
        }
    }

    /// Durable source list, individual visibility toggles and range preferences.
    pub fn save(&self) -> Result<(), String> {
        let configurations: Vec<_> = self.layers.iter().map(PlacefileLayer::config).collect();
        data_source::placefiles::save_configs(&self.config_path, &configurations)
    }

    pub fn enabled_count(&self) -> usize {
        self.layers.iter().filter(|layer| layer.enabled).count()
    }

    pub fn status_summary(&self) -> String {
        let enabled = self.enabled_count();
        let ready = self
            .layers
            .iter()
            .filter(|layer| layer.enabled && layer.data.is_some())
            .count();
        match (enabled, ready) {
            (0, _) => "No placefile overlays enabled".to_owned(),
            (enabled, ready) if enabled == ready => {
                format!(
                    "{ready} placefile overlay{} active",
                    if ready == 1 { "" } else { "s" }
                )
            }
            _ => format!("{ready} of {enabled} placefile overlays ready"),
        }
    }

    fn persist(&mut self) {
        self.notice = self.save().err();
    }
}

fn parse_status(placefile: &Placefile) -> String {
    let count = placefile.objects.len();
    let mut status = format!("{count} object{}", if count == 1 { "" } else { "s" });
    if placefile.skipped != 0 {
        status.push_str(&format!(
            " · {} unavailable or unrecognized",
            placefile.skipped
        ));
    }
    status
}

fn poll_source(
    layer: &mut PlacefileLayer,
    now: Instant,
    reference_time: Option<DateTime<Utc>>,
) -> bool {
    let Some(receiver) = layer.source_receiver.as_ref() else {
        return false;
    };
    match receiver.try_recv() {
        Ok(Ok(mut batch)) => {
            layer.source_receiver = None;
            if let Some(reference_time) = reference_time {
                batch.parsed = parse_placefile_at(&batch.text, &layer.source, reference_time);
            }
            layer.install_source(batch, now);
            true
        }
        Ok(Err(error)) => {
            layer.source_receiver = None;
            layer.status = if layer.data.is_some() {
                format!("Refresh failed; last good overlay retained: {error}")
            } else {
                format!("Load failed: {error}")
            };
            layer.next_refresh = now.checked_add(RETRY_INTERVAL);
            true
        }
        Err(TryRecvError::Disconnected) => {
            layer.source_receiver = None;
            layer.status = "Placefile worker ended before finishing".to_owned();
            layer.next_refresh = now.checked_add(RETRY_INTERVAL);
            true
        }
        Err(TryRecvError::Empty) => false,
    }
}

fn schedule_source(
    layer: &mut PlacefileLayer,
    ctx: &egui::Context,
    reference_time: Option<DateTime<Utc>>,
) {
    let source = layer.source.clone();
    let (sender, receiver) = mpsc::channel();
    layer.source_receiver = Some(receiver);
    layer.status = if layer.data.is_some() {
        "Refreshing; previous overlay remains visible".to_owned()
    } else {
        "Loading placefile…".to_owned()
    };
    let wake = ctx.clone();
    let started = thread::Builder::new()
        .name("genericradar-placefile".to_owned())
        .spawn(move || {
            let text = if is_remote_source(&source) {
                data_source::placefiles::load_source_text(&source)
            } else {
                read_local_placefile(&source)
            };
            let outcome = text.map(|text| {
                let parsed = match reference_time {
                    Some(reference) => parse_placefile_at(&text, &source, reference),
                    None => parse_placefile(&text, &source),
                };
                SourceBatch { text, parsed }
            });
            let _ = sender.send(outcome);
            wake.request_repaint();
        });
    if let Err(error) = started {
        layer.source_receiver = None;
        layer.next_refresh = Instant::now().checked_add(RETRY_INTERVAL);
        layer.status = format!("Unable to start placefile worker: {error}");
    }
}

fn poll_icons(layer: &mut PlacefileLayer, ctx: &egui::Context) -> bool {
    let Some(receiver) = layer.icon_receiver.as_ref() else {
        return false;
    };
    match receiver.try_recv() {
        Ok(batch) => {
            layer.icon_receiver = None;
            if batch.source_generation != layer.source_generation {
                layer.icons_generation = None;
                return false;
            }
            let mut textures_by_url: HashMap<String, (u32, u32, egui::TextureHandle)> =
                HashMap::new();
            let mut installed = false;
            let mut errors = Vec::new();
            for (spec, image) in batch.decoded {
                match image {
                    Ok(image) => {
                        let (width, height, texture) = textures_by_url
                            .entry(spec.url.clone())
                            .or_insert_with(|| {
                                let dimensions = [image.width as usize, image.height as usize];
                                let pixels = egui::ColorImage::from_rgba_unmultiplied(
                                    dimensions,
                                    &image.rgba,
                                );
                                let texture = ctx.load_texture(
                                    format!("genericradar-placefile:{}", spec.url),
                                    pixels,
                                    egui::TextureOptions::LINEAR,
                                );
                                (image.width, image.height, texture)
                            })
                            .clone();
                        layer.sheets.retain(|sheet| sheet.spec.index != spec.index);
                        layer.sheets.push(IconSheet {
                            spec,
                            width,
                            height,
                            texture,
                        });
                        installed = true;
                    }
                    Err(error) => errors.push(error),
                }
            }
            if !errors.is_empty() {
                layer
                    .status
                    .push_str(&format!(" · icon sheet: {}", errors.join("; ")));
            }
            if installed {
                layer.generation = layer.generation.wrapping_add(1);
            }
            installed || !errors.is_empty()
        }
        Err(TryRecvError::Disconnected) => {
            layer.icon_receiver = None;
            layer.icons_generation = None;
            layer
                .status
                .push_str(" · icon worker ended before finishing");
            true
        }
        Err(TryRecvError::Empty) => false,
    }
}

fn reuse_matching_icons(layer: &mut PlacefileLayer) -> bool {
    if !is_remote_source(&layer.source) {
        return false;
    }
    let Some(data) = layer.data.as_ref() else {
        return false;
    };
    let mut aliases = Vec::new();
    for spec in &data.icon_sheets {
        if layer.sheets.iter().any(|sheet| sheet.spec == *spec) {
            continue;
        }
        if let Some(existing) = layer.sheets.iter().find(|sheet| sheet.spec.url == spec.url) {
            aliases.push(IconSheet {
                spec: spec.clone(),
                width: existing.width,
                height: existing.height,
                texture: existing.texture.clone(),
            });
        }
    }
    if aliases.is_empty() {
        return false;
    }
    layer.sheets.extend(aliases);
    layer.generation = layer.generation.wrapping_add(1);
    true
}

fn schedule_icons(layer: &mut PlacefileLayer, ctx: &egui::Context) {
    let Some(data) = layer.data.as_ref() else {
        return;
    };
    let reload_local = !is_remote_source(&layer.source);
    let requested: Vec<_> = data
        .icon_sheets
        .iter()
        .filter(|spec| reload_local || !layer.sheets.iter().any(|sheet| sheet.spec == **spec))
        .cloned()
        .collect();
    layer.icons_generation = Some(layer.source_generation);
    if requested.is_empty() {
        return;
    }

    let generation = layer.source_generation;
    let (sender, receiver) = mpsc::channel();
    layer.icon_receiver = Some(receiver);
    let wake = ctx.clone();
    let started = thread::Builder::new()
        .name("genericradar-placefile-icons".to_owned())
        .spawn(move || {
            let mut cache: HashMap<String, Result<DecodedIconImage, String>> = HashMap::new();
            let mut decoded = Vec::with_capacity(requested.len());
            for spec in requested {
                let image = cache
                    .entry(spec.url.clone())
                    .or_insert_with(|| {
                        if is_remote_source(&spec.url) {
                            data_source::placefiles::load_icon_image(&spec.url)
                        } else {
                            read_local_icon(&spec.url).and_then(|bytes| {
                                data_source::placefiles::decode_icon_image(&bytes)
                            })
                        }
                    })
                    .clone();
                decoded.push((spec, image));
            }
            let _ = sender.send(IconBatch {
                source_generation: generation,
                decoded,
            });
            wake.request_repaint();
        });
    if let Err(error) = started {
        layer.icon_receiver = None;
        layer.icons_generation = None;
        layer
            .status
            .push_str(&format!(" · unable to start icon worker: {error}"));
    }
}

fn visibility_range_label(percent: u16) -> &'static str {
    match percent {
        100 => "Source range",
        200 => "2× farther",
        400 => "4× farther",
        800 => "8× farther",
        u16::MAX => "Always",
        _ => "Custom",
    }
}

fn human_age(age: Duration) -> String {
    match age.as_secs() {
        seconds @ 0..=59 => format!("{seconds}s"),
        seconds @ 60..=3599 => format!("{}m", seconds / 60),
        seconds => format!("{}h", seconds / 3600),
    }
}

fn placefile_color(rgb: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

fn draw_line_segment(
    painter: &egui::Painter,
    points: &mut Vec<egui::Pos2>,
    width: f32,
    color: egui::Color32,
) {
    if points.len() >= 2 {
        painter.add(egui::Shape::line(
            std::mem::take(points),
            egui::Stroke::new(width, color),
        ));
    } else {
        points.clear();
    }
}

/// Draw one correctly anchored, heading-rotated sprite from a real icon sheet.
fn icon_sprite_shape(
    sheet: &IconSheet,
    icon_index: u32,
    position: egui::Pos2,
    heading_deg: f32,
    scale: f32,
) -> Option<egui::Shape> {
    let (icon_w, icon_h) = (sheet.spec.icon_w, sheet.spec.icon_h);
    if icon_w == 0 || icon_h == 0 || sheet.width < icon_w || sheet.height < icon_h {
        return None;
    }
    let columns = sheet.width / icon_w;
    let rows = sheet.height / icon_h;
    let slot = icon_index.saturating_sub(1);
    if u64::from(slot) >= u64::from(columns) * u64::from(rows) {
        return None;
    }
    let (column, row) = (slot % columns, slot / columns);
    let uv_start = egui::pos2(
        (column * icon_w) as f32 / sheet.width as f32,
        (row * icon_h) as f32 / sheet.height as f32,
    );
    let uv_end = egui::pos2(
        ((column + 1) * icon_w) as f32 / sheet.width as f32,
        ((row + 1) * icon_h) as f32 / sheet.height as f32,
    );
    let corners = [
        egui::vec2(0.0, 0.0),
        egui::vec2(icon_w as f32, 0.0),
        egui::vec2(icon_w as f32, icon_h as f32),
        egui::vec2(0.0, icon_h as f32),
    ];
    let coordinates = [
        uv_start,
        egui::pos2(uv_end.x, uv_start.y),
        uv_end,
        egui::pos2(uv_start.x, uv_end.y),
    ];
    let hotspot = egui::vec2(sheet.spec.hot_x, sheet.spec.hot_y);
    let (sin, cos) = heading_deg.to_radians().sin_cos();
    let rotate = |point: egui::Vec2| {
        egui::vec2(point.x * cos - point.y * sin, point.x * sin + point.y * cos)
    };
    let mut mesh = egui::epaint::Mesh::with_texture(sheet.texture.id());
    for (corner, uv) in corners.iter().zip(coordinates) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: position + rotate((*corner - hotspot) * scale.max(0.05)),
            uv,
            color: egui::Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    Some(egui::Shape::mesh(mesh))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
; spotter feed sample
Title: Test Spotters
Refresh: 2
Color: 255 0 0
Threshold: 999
Font: 1, 12, 1, "Arial"
IconFile: 1, 16, 16, 8, 8, "http://example/icons.png"
Icon: 39.05, -94.59, 45, 1, 3, "Spotter One\nReporting"
Text: 38.90, -94.20, 1, "KC METAR", "hover"
Place: 38.50, -94.00, Old Style Label
Color: 0 128 255
Line: 2, 0
 39.0, -95.0
 39.2, -94.8
 39.4, -94.6
End:
Polygon:
 38.0, -95.0
 38.2, -94.8
 38.0, -94.6
End:
Object: 39.0, -94.0
 Icon: 0, 0, 0, 1, 1
 Text: 10, -12, 1, "T"
End:
"#;

    #[test]
    fn parses_the_core_statements() {
        let pf = parse_placefile(SAMPLE, "");
        assert_eq!(pf.title, "Test Spotters");
        assert_eq!(pf.refresh_minutes, 2);
        assert_eq!(pf.icon_sheets.len(), 1);
        assert_eq!(pf.icon_sheets[0].icon_w, 16);
        assert_eq!(pf.icon_sheets[0].url, "http://example/icons.png");
        assert_eq!(pf.objects.len(), 7, "{:#?}", pf.objects);
        match &pf.objects[0] {
            PlacefileObject::Icon {
                lat,
                lon,
                anchor,
                heading_deg,
                file_index,
                icon_index,
                label,
                color,
                ..
            } => {
                assert!((lat - 39.05).abs() < 1e-4);
                assert!((lon + 94.59).abs() < 1e-4);
                assert!(anchor.is_none());
                assert_eq!(*heading_deg, 45.0);
                assert_eq!(*file_index, 1);
                assert_eq!(*icon_index, 3);
                assert_eq!(label.as_deref(), Some("Spotter One\\nReporting"));
                assert_eq!(*color, [255, 0, 0]);
            }
            other => panic!("expected icon, got {other:?}"),
        }
        match &pf.objects[3] {
            PlacefileObject::Line { width, points, .. } => {
                assert_eq!(*width, 2.0);
                assert_eq!(points.len(), 3);
            }
            other => panic!("expected line, got {other:?}"),
        }
        // Object-block members carry the anchor with pixel offsets.
        match &pf.objects[5] {
            PlacefileObject::Icon {
                lat, lon, anchor, ..
            } => {
                assert_eq!((*lat, *lon), (0.0, 0.0));
                assert_eq!(*anchor, Some((39.0, -94.0)));
            }
            other => panic!("expected anchored icon, got {other:?}"),
        }
        match &pf.objects[6] {
            PlacefileObject::Text {
                lat, lon, anchor, ..
            } => {
                assert_eq!((*lat, *lon), (10.0, -12.0));
                assert_eq!(*anchor, Some((39.0, -94.0)));
            }
            other => panic!("expected anchored text, got {other:?}"),
        }
    }

    #[test]
    fn object_anchor_resets_after_end() {
        let pf = parse_placefile(
            "Object: 39.0, -94.0\n Icon: 0, 0, 0, 1, 1\nEnd:\nIcon: 38.0, -95.0, 0, 1, 1\n",
            "",
        );
        assert_eq!(pf.objects.len(), 2);
        match &pf.objects[1] {
            PlacefileObject::Icon { anchor, .. } => assert!(anchor.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_spotter_network_reports_with_multiline_hover() {
        let sample = r#"
Refresh: 1
Threshold: 999
Title: Spotter Network (96dpi) - Reports Only
Font: 1, 11, 0, "Courier New"
IconFile: 3, 30, 30, 15, 15, "https://www.spotternetwork.org/iconsheets/SN_Reports_096.png"
Icon: 40.217899,-79.495102,000,3,3,"Reported By: Test Spotter\nRotating Wall Cloud\nTime: 2026-06-14 23:18:34 UTC\nNotes: first line
second line from wrapped feed"
"#;
        let pf = parse_placefile(sample, "https://www.spotternetwork.org/feeds/reports.txt");
        assert_eq!(pf.refresh_minutes, 1);
        assert_eq!(pf.title, "Spotter Network (96dpi) - Reports Only");
        assert_eq!(pf.icon_sheets.len(), 1);
        assert_eq!(pf.icon_sheets[0].index, 3);
        assert_eq!(pf.objects.len(), 1);
        match &pf.objects[0] {
            PlacefileObject::Icon {
                file_index, label, ..
            } => {
                assert_eq!(*file_index, 3);
                let label = label.as_deref().expect("report hover text");
                assert!(label.contains("Rotating Wall Cloud"));
                assert!(label.contains("second line from wrapped feed"));
            }
            other => panic!("expected report icon, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let pf = parse_placefile(
            "Title: x\nIcon: not, numbers\nText: 1,2\nLine: zz\nEnd:\n",
            "",
        );
        assert_eq!(pf.title, "x");
        assert!(pf.objects.is_empty());
    }

    #[test]
    fn downloaded_placefile_resolves_relative_icon_beside_source() {
        let base = std::env::temp_dir()
            .join("genericradar-placefile-test")
            .join("storm.placefile");
        let source = base.to_string_lossy();
        let pf = parse_placefile("IconFile: 1, 16, 16, 8, 8, \"icons/sheet.png\"\n", &source);
        assert_eq!(pf.icon_sheets.len(), 1);
        assert_eq!(
            PathBuf::from(&pf.icon_sheets[0].url),
            base.parent().unwrap().join("icons/sheet.png")
        );
    }

    #[test]
    fn local_reader_accepts_extensionless_text_and_rejects_binary() {
        let unique = format!(
            "genericradar-placefile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::write(&path, b"\xEF\xBB\xBFTitle: Local\nPlace: 35, -97, Home\n").unwrap();
        let source = path.to_string_lossy();
        let text = read_local_placefile(&source).unwrap();
        assert!(text.starts_with("Title: Local"));

        std::fs::write(&path, b"not\0a text placefile").unwrap();
        let error = read_local_placefile(&source).unwrap_err();
        assert!(error.contains("NUL bytes"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn source_dispatch_only_treats_http_and_https_as_remote() {
        assert!(is_remote_source("HTTP://example.test/layer.txt"));
        assert!(is_remote_source("https://example.test/layer.txt"));
        assert!(!is_remote_source("C:\\downloads\\storm.txt"));
        assert!(!is_remote_source("/tmp/storm"));
        assert!(!is_remote_source("å downloaded placefile"));
    }

    #[test]
    fn refresh_seconds_keep_the_exact_live_feed_cadence() {
        let parsed = parse_placefile("Refresh: 8\nRefreshSeconds: 17\n", "");
        assert_eq!(parsed.refresh_seconds, 17);
        assert_eq!(parsed.refresh_minutes, 1);
    }

    #[test]
    fn time_ranges_follow_the_historical_radar_frame() {
        let text = concat!(
            "TimeRange: 2024-05-06T22:00:00 2024-05-06T23:00:00\n",
            "Place: 35.0, -97.0, Historical report\n",
        );
        let during = DateTime::parse_from_rfc3339("2024-05-06T22:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let after = DateTime::parse_from_rfc3339("2024-05-07T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parse_placefile_at(text, "", during).objects.len(), 1);
        assert!(parse_placefile_at(text, "", after).objects.is_empty());
    }

    #[test]
    fn malformed_or_reversed_time_ranges_never_invent_reports() {
        for range in ["not-a-timestamp", "2024-05-07T00:00:00 2024-05-06T00:00:00"] {
            let parsed = parse_placefile(
                &format!("TimeRange: {range}\nPlace: 35.0, -97.0, Report\n"),
                "",
            );
            assert!(parsed.objects.is_empty(), "bad range was accepted: {range}");
        }
    }

    #[test]
    fn object_pixel_offsets_have_no_artificial_multidisplay_ceiling() {
        let parsed = parse_placefile(
            "Object: 35.0, -97.0\nIcon: 12000, -9500, 0, 1, 1\nEnd:\n",
            "",
        );
        assert_eq!(parsed.objects.len(), 1);
        match &parsed.objects[0] {
            PlacefileObject::Icon { lat, lon, .. } => {
                assert_eq!((*lat, *lon), (12_000.0, -9_500.0));
            }
            other => panic!("expected an unrestricted anchored icon, got {other:?}"),
        }
        assert!(!pair_valid(f32::INFINITY, 0.0, true));
        assert!(!pair_valid(0.0, f32::NAN, true));
    }

    #[test]
    fn playback_reparses_only_timed_sources_and_keeps_icon_generation() {
        let reference = DateTime::parse_from_rfc3339("2024-05-06T22:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut untimed = PlacefileLayer::from_config(SourceConfig::new("untimed.pf"));
        untimed.source_text = Some("Place: 35.0, -97.0, Persistent\n".to_owned());
        untimed.source_generation = 7;
        untimed.icons_generation = Some(7);
        assert!(!untimed.reparse(reference));
        assert_eq!(untimed.source_generation, 7);
        assert_eq!(untimed.icons_generation, Some(7));

        let mut timed = PlacefileLayer::from_config(SourceConfig::new("timed.pf"));
        timed.source_text = Some(
            concat!(
                "  tImErAnGe: 2024-05-06T22:00:00 2024-05-06T23:00:00\n",
                "Place: 35.0, -97.0, Historical report\n",
            )
            .to_owned(),
        );
        timed.source_generation = 11;
        timed.icons_generation = Some(11);
        assert!(timed.reparse(reference));
        assert_eq!(timed.data.as_ref().unwrap().objects.len(), 1);
        assert_eq!(timed.source_generation, 11);
        assert_eq!(timed.icons_generation, Some(11));
        assert_eq!(timed.generation, 1);
    }
}
