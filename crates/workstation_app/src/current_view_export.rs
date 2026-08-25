//! Full-window still and animated export through eframe's composited screenshots.
//!
//! No native dialog is involved. The action asks eframe for the pixels that
//! were actually presented, tags that request so unrelated screenshot users
//! cannot be consumed accidentally, then writes a non-colliding PNG directly
//! into the user's Downloads folder on a worker thread. Animated GIFs use a
//! dependency-free adaptive palette and temporal delta compression.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use chrono::{DateTime, Utc};
use eframe::egui;

#[derive(Clone, Debug)]
struct CaptureTag {
    file_base: String,
}

#[derive(Debug)]
enum ExportUpdate {
    Saved { path: PathBuf, animated: bool },
    Failed(String),
}

/// State for one tagged capture at a time.
pub struct CurrentViewExport {
    sender: Sender<ExportUpdate>,
    receiver: Receiver<ExportUpdate>,
    in_flight: bool,
    status: Option<String>,
    detail: Option<String>,
}

impl Default for CurrentViewExport {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            in_flight: false,
            status: None,
            detail: None,
        }
    }
}

impl CurrentViewExport {
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Ask eframe for the next composited main-window image.
    pub fn request(&mut self, context: &egui::Context, file_base: String) {
        if self.in_flight {
            return;
        }
        self.in_flight = true;
        self.status = Some("Export: capturing current view…".to_owned());
        self.detail = Some(
            "The rendered application window will be written as a PNG directly to Downloads."
                .to_owned(),
        );
        context.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
            CaptureTag { file_base },
        )));
        context.request_repaint();
    }

    /// Consume only screenshot replies carrying this module's private tag.
    pub fn handle_capture_events(&mut self, context: &egui::Context) {
        let captures: Vec<(CaptureTag, Arc<egui::ColorImage>)> = context.input(|input| {
            input
                .raw
                .events
                .iter()
                .filter_map(|event| {
                    let egui::Event::Screenshot {
                        user_data, image, ..
                    } = event
                    else {
                        return None;
                    };
                    let tag = user_data.data.as_ref()?.downcast_ref::<CaptureTag>()?;
                    Some((tag.clone(), Arc::clone(image)))
                })
                .collect()
        });

        for (tag, image) in captures {
            let sender = self.sender.clone();
            let repaint = context.clone();
            let spawned = thread::Builder::new()
                .name("radar-current-view-export".to_owned())
                .spawn(move || {
                    let update = match save_capture_png(&image, &tag.file_base) {
                        Ok(path) => ExportUpdate::Saved {
                            path,
                            animated: false,
                        },
                        Err(message) => ExportUpdate::Failed(message),
                    };
                    let _ = sender.send(update);
                    repaint.request_repaint();
                });
            match spawned {
                Ok(_) => {
                    self.status = Some("Export: writing PNG to Downloads…".to_owned());
                }
                Err(error) => {
                    self.in_flight = false;
                    self.status = Some("Export failed".to_owned());
                    self.detail = Some(format!("could not start PNG writer: {error}"));
                }
            }
        }
    }

    /// Encode already-composited timeline frames without blocking the renderer.
    ///
    /// Capture stays on the UI thread because eframe owns the viewport, while
    /// palette construction, delta detection, LZW and disk I/O happen on this
    /// dedicated worker. An `Arc` transfers screenshot ownership without
    /// cloning a full-resolution frame on the interactive thread.
    pub fn request_loop(
        &mut self,
        context: &egui::Context,
        file_base: String,
        frames: Vec<Arc<egui::ColorImage>>,
        delay_ms: u32,
    ) {
        if self.in_flight {
            return;
        }

        let frame_count = frames.len();
        self.in_flight = true;
        self.status = Some(format!("Export: encoding {frame_count} loop frames…"));
        self.detail = Some(
            "An optimized, continuously repeating animated GIF is being written to Downloads."
                .to_owned(),
        );

        let sender = self.sender.clone();
        let repaint = context.clone();
        match thread::Builder::new()
            .name("radar-loop-export".to_owned())
            .spawn(move || {
                let update = match save_capture_gif(&frames, &file_base, delay_ms) {
                    Ok(path) => ExportUpdate::Saved {
                        path,
                        animated: true,
                    },
                    Err(message) => ExportUpdate::Failed(message),
                };
                let _ = sender.send(update);
                repaint.request_repaint();
            }) {
            Ok(_) => context.request_repaint(),
            Err(error) => {
                self.in_flight = false;
                self.status = Some("Export failed".to_owned());
                self.detail = Some(format!("could not start animated GIF writer: {error}"));
            }
        }
    }

    pub fn poll(&mut self) {
        while let Ok(update) = self.receiver.try_recv() {
            self.in_flight = false;
            match update {
                ExportUpdate::Saved { path, animated } => {
                    let description = if animated {
                        "radar loop"
                    } else {
                        "current view"
                    };
                    self.status = Some(format!("Exported {description}"));
                    self.detail = Some(format!("Saved {description} to {}", path.display()));
                }
                ExportUpdate::Failed(message) => {
                    self.status = Some("Export failed".to_owned());
                    self.detail = Some(message);
                }
            }
        }
    }
}

/// A sortable animated-loop name using the same safe components as a still.
pub fn loop_file_base(
    site: Option<&str>,
    volume_time: Option<DateTime<Utc>>,
    product: &str,
    captured_at: DateTime<Utc>,
) -> String {
    capture_file_base(site, volume_time, product, captured_at).replacen(
        "_current-view_",
        "_loop_",
        1,
    )
}

/// A sortable, Windows-safe name carrying the frame and active product.
pub fn capture_file_base(
    site: Option<&str>,
    volume_time: Option<DateTime<Utc>>,
    product: &str,
    captured_at: DateTime<Utc>,
) -> String {
    let site = safe_component(site.unwrap_or("no-data"));
    let product = safe_component(product);
    let frame = volume_time
        .map(|time| time.format("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|| "no-frame".to_owned());
    format!(
        "GenericRadar_current-view_{site}_{frame}_{product}_{}",
        captured_at.format("%Y%m%dT%H%M%SZ")
    )
}

fn safe_component(value: &str) -> String {
    let mut safe = String::with_capacity(value.len().min(48));
    let mut separator = false;
    for ch in value.chars() {
        if safe.len() >= 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            safe.push(ch);
            separator = false;
        } else if !separator && !safe.is_empty() {
            safe.push('-');
            separator = true;
        }
    }
    while safe.ends_with('-') {
        safe.pop();
    }
    if safe.is_empty() {
        "unknown".to_owned()
    } else {
        safe
    }
}

fn downloads_directory() -> Result<PathBuf, String> {
    downloads_directory_from(
        std::env::var_os("USERPROFILE").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
    .ok_or_else(|| "could not locate the user's Downloads folder".to_owned())
}

fn downloads_directory_from(
    user_profile: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    user_profile
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.filter(|path| !path.as_os_str().is_empty()))
        .map(|root| root.join("Downloads"))
}

/// Builds `<dir>/<base>[_N].png` without replacing an existing export.
fn unique_png_path(directory: &Path, base: &str) -> PathBuf {
    unique_export_path(directory, base, "png")
}

fn unique_export_path(directory: &Path, base: &str, extension: &str) -> PathBuf {
    let mut path = directory.join(format!("{base}.{extension}"));
    let mut counter = 2_u32;
    while path.exists() {
        path = directory.join(format!("{base}_{counter}.{extension}"));
        counter += 1;
    }
    path
}

fn rgba_bytes(image: &egui::ColorImage) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
    // A main-window capture is opaque. Flattening alpha also avoids handing a
    // premultiplied egui colour to a straight-alpha PNG encoder.
    for pixel in &image.pixels {
        rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), u8::MAX]);
    }
    rgba
}

fn save_capture_png(image: &egui::ColorImage, file_base: &str) -> Result<PathBuf, String> {
    let directory = downloads_directory()?;
    save_capture_png_in(image, file_base, &directory)
}

fn save_capture_png_in(
    image: &egui::ColorImage,
    file_base: &str,
    directory: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = unique_png_path(directory, file_base);
    render2d::write_rgba_png(
        &rgba_bytes(image),
        image.size[0] as u32,
        image.size[1] as u32,
        &path,
    )
    .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

fn save_capture_gif(
    frames: &[Arc<egui::ColorImage>],
    file_base: &str,
    delay_ms: u32,
) -> Result<PathBuf, String> {
    save_capture_gif_in(frames, file_base, delay_ms, &downloads_directory()?)
}

fn save_capture_gif_in(
    frames: &[Arc<egui::ColorImage>],
    file_base: &str,
    delay_ms: u32,
    directory: &Path,
) -> Result<PathBuf, String> {
    let [width, height] = validate_gif_frames(frames)?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = unique_export_path(directory, file_base, "gif");
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;

    let result = (|| {
        let mut writer = BufWriter::new(file);
        write_animated_gif(&mut writer, frames, width, height, delay_ms)?;
        writer
            .flush()
            .map_err(|error| format!("could not finish animated GIF: {error}"))
    })();
    if let Err(error) = result {
        // This exact path was created above with `create_new`; never leave a
        // truncated download that looks like a successfully exported movie.
        let _ = std::fs::remove_file(&path);
        return Err(format!("could not write {}: {error}", path.display()));
    }
    Ok(path)
}

fn validate_gif_frames(frames: &[Arc<egui::ColorImage>]) -> Result<[u16; 2], String> {
    let first = frames
        .first()
        .ok_or_else(|| "cannot export an empty radar loop".to_owned())?;
    let [width, height] = first.size;
    if width == 0 || height == 0 {
        return Err("cannot export a radar loop with an empty viewport".to_owned());
    }
    let width = u16::try_from(width)
        .map_err(|_| "the GIF format cannot represent viewport widths above 65,535 pixels")?;
    let height = u16::try_from(height)
        .map_err(|_| "the GIF format cannot represent viewport heights above 65,535 pixels")?;
    for (index, frame) in frames.iter().enumerate() {
        if frame.size != first.size {
            return Err(format!(
                "viewport dimensions changed during loop capture at frame {} (expected {}×{}, got {}×{})",
                index + 1,
                width,
                height,
                frame.size[0],
                frame.size[1]
            ));
        }
        if frame.pixels.len() != usize::from(width) * usize::from(height) {
            return Err(format!(
                "loop frame {} has an invalid screenshot pixel count",
                index + 1
            ));
        }
    }
    Ok([width, height])
}

const HISTOGRAM_LEN: usize = 32 * 32 * 32;
const PALETTE_SIZE: usize = 256;

#[derive(Clone, Copy, Default)]
struct HistogramBin {
    count: u64,
    red: u64,
    green: u64,
    blue: u64,
}

#[derive(Clone, Copy)]
struct HistogramColor {
    bin: usize,
    rgb: [u8; 3],
    count: u64,
}

#[derive(Clone, Copy)]
struct ColorBox {
    start: usize,
    end: usize,
    count: u64,
    minimum: [u8; 3],
    maximum: [u8; 3],
}

impl ColorBox {
    fn new(colors: &[HistogramColor], start: usize, end: usize) -> Self {
        let mut minimum = [u8::MAX; 3];
        let mut maximum = [0; 3];
        let mut count = 0_u64;
        for color in &colors[start..end] {
            count = count.saturating_add(color.count);
            for channel in 0..3 {
                minimum[channel] = minimum[channel].min(color.rgb[channel]);
                maximum[channel] = maximum[channel].max(color.rgb[channel]);
            }
        }
        Self {
            start,
            end,
            count,
            minimum,
            maximum,
        }
    }

    fn split_channel(self) -> usize {
        // Green carries most perceived detail, followed by red and then blue.
        // Those weights also protect radar's green/yellow/red boundaries.
        [3_u32, 4, 2]
            .into_iter()
            .enumerate()
            .max_by_key(|&(channel, weight)| {
                weight * u32::from(self.maximum[channel] - self.minimum[channel])
            })
            .map_or(0, |(channel, _)| channel)
    }

    fn split_priority(self) -> u64 {
        if self.end - self.start < 2 {
            return 0;
        }
        let channel = self.split_channel();
        let span = u64::from(self.maximum[channel] - self.minimum[channel]);
        span.saturating_mul(self.count)
    }
}

struct AdaptivePalette {
    colors: [[u8; 3]; PALETTE_SIZE],
    indices: Vec<u8>,
    exact: Option<HashMap<u32, u8>>,
}

impl AdaptivePalette {
    fn from_frames(frames: &[Arc<egui::ColorImage>]) -> Self {
        let mut histogram = vec![HistogramBin::default(); HISTOGRAM_LEN];
        let mut exact = Some(HashMap::<u32, u64>::with_capacity(PALETTE_SIZE + 1));

        for image in frames {
            for pixel in &image.pixels {
                let rgb = [pixel.r(), pixel.g(), pixel.b()];
                let bin = &mut histogram[histogram_index(rgb)];
                bin.count = bin.count.saturating_add(1);
                bin.red = bin.red.saturating_add(u64::from(rgb[0]));
                bin.green = bin.green.saturating_add(u64::from(rgb[1]));
                bin.blue = bin.blue.saturating_add(u64::from(rgb[2]));

                if let Some(unique) = exact.as_mut() {
                    let packed = packed_rgb(rgb);
                    *unique.entry(packed).or_default() += 1;
                    if unique.len() > PALETTE_SIZE {
                        exact = None;
                    }
                }
            }
        }

        if let Some(exact) = exact {
            return Self::from_exact_colors(exact);
        }

        let mut colors: Vec<HistogramColor> = histogram
            .into_iter()
            .enumerate()
            .filter(|(_, value)| value.count > 0)
            .map(|(bin, value)| HistogramColor {
                bin,
                rgb: [
                    (value.red / value.count) as u8,
                    (value.green / value.count) as u8,
                    (value.blue / value.count) as u8,
                ],
                count: value.count,
            })
            .collect();
        let mut boxes = vec![ColorBox::new(&colors, 0, colors.len())];

        while boxes.len() < PALETTE_SIZE {
            let Some((box_index, selected)) = boxes
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, selected)| selected.end - selected.start > 1)
                .max_by_key(|(_, selected)| selected.split_priority())
            else {
                break;
            };
            let channel = selected.split_channel();
            colors[selected.start..selected.end]
                .sort_unstable_by_key(|color| (color.rgb[channel], packed_rgb(color.rgb)));

            let midpoint = selected.count / 2;
            let mut accumulated = 0_u64;
            let mut split = selected.start + 1;
            for (offset, color) in colors[selected.start..selected.end - 1].iter().enumerate() {
                accumulated = accumulated.saturating_add(color.count);
                split = selected.start + offset + 1;
                if accumulated >= midpoint {
                    break;
                }
            }
            boxes[box_index] = ColorBox::new(&colors, selected.start, split);
            boxes.push(ColorBox::new(&colors, split, selected.end));
        }

        let mut palette = [[0_u8; 3]; PALETTE_SIZE];
        for (index, selected) in boxes.iter().enumerate() {
            let mut sums = [0_u64; 3];
            for color in &colors[selected.start..selected.end] {
                for (channel, sum) in sums.iter_mut().enumerate() {
                    *sum = sum.saturating_add(u64::from(color.rgb[channel]) * color.count);
                }
            }
            palette[index] = sums.map(|sum| (sum / selected.count) as u8);
        }

        let mut indices = vec![0_u8; HISTOGRAM_LEN];
        for color in colors {
            let index = palette[..boxes.len()]
                .iter()
                .enumerate()
                .min_by_key(|(_, candidate)| color_distance(color.rgb, **candidate))
                .map_or(0, |(index, _)| index);
            indices[color.bin] = index as u8;
        }

        Self {
            colors: palette,
            indices,
            exact: None,
        }
    }

    fn from_exact_colors(colors: HashMap<u32, u64>) -> Self {
        let mut sorted: Vec<(u32, u64)> = colors.into_iter().collect();
        // Frequency-first ordering improves early LZW prefixes, and the packed
        // color resolves ties so the same loop always produces the same bytes.
        sorted.sort_unstable_by_key(|&(color, count)| (std::cmp::Reverse(count), color));

        let mut palette = [[0_u8; 3]; PALETTE_SIZE];
        let mut exact = HashMap::with_capacity(sorted.len());
        for (index, (packed, _)) in sorted.into_iter().enumerate() {
            palette[index] = [
                ((packed >> 16) & 0xff) as u8,
                ((packed >> 8) & 0xff) as u8,
                (packed & 0xff) as u8,
            ];
            exact.insert(packed, index as u8);
        }

        Self {
            colors: palette,
            indices: Vec::new(),
            exact: Some(exact),
        }
    }

    fn index(&self, pixel: egui::Color32) -> u8 {
        let rgb = [pixel.r(), pixel.g(), pixel.b()];
        match self.exact.as_ref() {
            Some(exact) => exact[&packed_rgb(rgb)],
            None => self.indices[histogram_index(rgb)],
        }
    }

    fn index_frame(&self, frame: &egui::ColorImage) -> Vec<u8> {
        frame
            .pixels
            .iter()
            .copied()
            .map(|pixel| self.index(pixel))
            .collect()
    }
}

fn packed_rgb(rgb: [u8; 3]) -> u32 {
    (u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2])
}

fn histogram_index(rgb: [u8; 3]) -> usize {
    (usize::from(rgb[0] >> 3) << 10) | (usize::from(rgb[1] >> 3) << 5) | usize::from(rgb[2] >> 3)
}

fn color_distance(left: [u8; 3], right: [u8; 3]) -> u32 {
    [3_u32, 4, 2]
        .into_iter()
        .enumerate()
        .map(|(channel, weight)| {
            let delta = i32::from(left[channel]) - i32::from(right[channel]);
            weight * (delta * delta) as u32
        })
        .sum()
}

fn write_animated_gif<W: Write>(
    writer: &mut W,
    frames: &[Arc<egui::ColorImage>],
    width: u16,
    height: u16,
    delay_ms: u32,
) -> Result<(), String> {
    let palette = AdaptivePalette::from_frames(frames);
    let delay_centiseconds = u64::from(delay_ms.saturating_add(5) / 10).max(2);

    writer
        .write_all(b"GIF89a")
        .and_then(|()| writer.write_all(&width.to_le_bytes()))
        .and_then(|()| writer.write_all(&height.to_le_bytes()))
        // Global color table present; eight-bit color resolution and entries.
        .and_then(|()| writer.write_all(&[0xf7, 0, 0]))
        .map_err(|error| format!("could not write GIF header: {error}"))?;
    for color in palette.colors {
        writer
            .write_all(&color)
            .map_err(|error| format!("could not write GIF color table: {error}"))?;
    }
    writer
        .write_all(&[
            0x21, 0xff, 11, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 3, 1,
            0, 0, 0,
        ])
        .map_err(|error| format!("could not write GIF repeat extension: {error}"))?;

    let mut displayed: Option<Vec<u8>> = None;
    let mut pending: Option<Vec<u8>> = None;
    let mut pending_delay = 0_u64;
    for frame in frames {
        let indexed = palette.index_frame(frame);
        if pending
            .as_ref()
            .is_some_and(|previous| *previous == indexed)
        {
            pending_delay = pending_delay.saturating_add(delay_centiseconds);
            continue;
        }
        if let Some(previous) = pending.replace(indexed) {
            write_delayed_gif_frame(
                writer,
                &previous,
                displayed.as_deref(),
                width,
                height,
                pending_delay,
            )?;
            displayed = Some(previous);
        }
        pending_delay = delay_centiseconds;
    }
    if let Some(last) = pending {
        write_delayed_gif_frame(
            writer,
            &last,
            displayed.as_deref(),
            width,
            height,
            pending_delay,
        )?;
    }

    writer
        .write_all(&[0x3b])
        .map_err(|error| format!("could not finish GIF stream: {error}"))
}

fn write_delayed_gif_frame<W: Write>(
    writer: &mut W,
    current: &[u8],
    previous: Option<&[u8]>,
    width: u16,
    height: u16,
    mut delay: u64,
) -> Result<(), String> {
    let mut reference = previous;
    while delay > 0 {
        let chunk = delay.min(u64::from(u16::MAX)) as u16;
        write_gif_delta_frame(writer, current, reference, width, height, chunk)?;
        reference = Some(current);
        delay -= u64::from(chunk);
    }
    Ok(())
}

fn write_gif_delta_frame<W: Write>(
    writer: &mut W,
    current: &[u8],
    previous: Option<&[u8]>,
    canvas_width: u16,
    canvas_height: u16,
    delay: u16,
) -> Result<(), String> {
    let width = usize::from(canvas_width);
    let height = usize::from(canvas_height);
    let (left, top, right, bottom) = match previous {
        None => (0, 0, width, height),
        Some(previous) => {
            let mut left = width;
            let mut top = height;
            let mut right = 0;
            let mut bottom = 0;
            for (index, (&present, &before)) in current.iter().zip(previous).enumerate() {
                if present != before {
                    let x = index % width;
                    let y = index / width;
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x + 1);
                    bottom = bottom.max(y + 1);
                }
            }
            if left == width {
                // An accumulated duplicate exceeded the GIF format's maximum
                // per-frame delay. A 1×1 unchanged pixel preserves its timing.
                (0, 0, 1, 1)
            } else {
                (left, top, right, bottom)
            }
        }
    };

    let mut used = [false; PALETTE_SIZE];
    let mut has_unchanged = false;
    if let Some(previous) = previous {
        for y in top..bottom {
            for x in left..right {
                let index = y * width + x;
                if current[index] == previous[index] {
                    has_unchanged = true;
                } else {
                    used[usize::from(current[index])] = true;
                }
            }
        }
    }
    let transparent = has_unchanged
        .then(|| used.iter().position(|&present| !present))
        .flatten()
        .map(|index| index as u8);

    let mut pixels = Vec::with_capacity((right - left) * (bottom - top));
    for y in top..bottom {
        for x in left..right {
            let index = y * width + x;
            let unchanged = previous.is_some_and(|previous| current[index] == previous[index]);
            pixels.push(if unchanged {
                transparent.unwrap_or(current[index])
            } else {
                current[index]
            });
        }
    }

    // Disposal method 1 (do not dispose) is essential: unchanged transparent
    // pixels must retain the already-composited preceding radar frame.
    let packed = 0x04 | u8::from(transparent.is_some());
    writer
        .write_all(&[0x21, 0xf9, 4, packed])
        .and_then(|()| writer.write_all(&delay.to_le_bytes()))
        .and_then(|()| writer.write_all(&[transparent.unwrap_or(0), 0, 0x2c]))
        .and_then(|()| writer.write_all(&(left as u16).to_le_bytes()))
        .and_then(|()| writer.write_all(&(top as u16).to_le_bytes()))
        .and_then(|()| writer.write_all(&((right - left) as u16).to_le_bytes()))
        .and_then(|()| writer.write_all(&((bottom - top) as u16).to_le_bytes()))
        .and_then(|()| writer.write_all(&[0, 8]))
        .map_err(|error| format!("could not write GIF frame header: {error}"))?;

    write_gif_lzw(writer, &pixels)
}

const DICTIONARY_SLOTS: usize = 8192;
const EMPTY_DICTIONARY_KEY: u32 = u32::MAX;

fn write_gif_lzw<W: Write>(writer: &mut W, pixels: &[u8]) -> Result<(), String> {
    let mut dictionary_keys = vec![EMPTY_DICTIONARY_KEY; DICTIONARY_SLOTS];
    let mut dictionary_codes = vec![0_u16; DICTIONARY_SLOTS];
    let mut blocks = GifSubBlocks::new(writer);
    let mut code_width = 9_u8;
    let mut next_code = 258_u16;

    blocks.push_code(256, code_width)?;
    if let Some((&first, rest)) = pixels.split_first() {
        let mut prefix = u16::from(first);
        for &pixel in rest {
            let key = (u32::from(prefix) << 8) | u32::from(pixel);
            let mut slot = ((key.wrapping_mul(0x9e37_79b1)) as usize) & (DICTIONARY_SLOTS - 1);
            while dictionary_keys[slot] != EMPTY_DICTIONARY_KEY && dictionary_keys[slot] != key {
                slot = (slot + 1) & (DICTIONARY_SLOTS - 1);
            }
            if dictionary_keys[slot] == key {
                prefix = dictionary_codes[slot];
                continue;
            }

            blocks.push_code(prefix, code_width)?;
            if next_code < 4096 {
                dictionary_keys[slot] = key;
                dictionary_codes[slot] = next_code;
                next_code += 1;
                // The decoder learns each new entry one emitted code later;
                // widening too early corrupts frames at 512/1024/2048 codes.
                if next_code > (1_u16 << code_width) && code_width < 12 {
                    code_width += 1;
                }
            } else {
                blocks.push_code(256, code_width)?;
                dictionary_keys.fill(EMPTY_DICTIONARY_KEY);
                code_width = 9;
                next_code = 258;
            }
            prefix = u16::from(pixel);
        }
        blocks.push_code(prefix, code_width)?;
        // The final prefix lets the decoder learn the pending dictionary
        // entry. If that entry reaches a code-width boundary, the following
        // end-of-information marker must use the decoder's newly widened
        // width even though the encoder does not add another entry itself.
        if next_code == (1_u16 << code_width) && code_width < 12 {
            code_width += 1;
        }
    }
    blocks.push_code(257, code_width)?;
    blocks.finish()
}

struct GifSubBlocks<'writer, W: Write> {
    writer: &'writer mut W,
    bytes: [u8; 255],
    length: usize,
    bits: u32,
    bit_count: u8,
}

impl<'writer, W: Write> GifSubBlocks<'writer, W> {
    fn new(writer: &'writer mut W) -> Self {
        Self {
            writer,
            bytes: [0; 255],
            length: 0,
            bits: 0,
            bit_count: 0,
        }
    }

    fn push_code(&mut self, code: u16, width: u8) -> Result<(), String> {
        self.bits |= u32::from(code) << self.bit_count;
        self.bit_count += width;
        while self.bit_count >= 8 {
            let byte = self.bits as u8;
            self.bits >>= 8;
            self.bit_count -= 8;
            self.push_byte(byte)?;
        }
        Ok(())
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), String> {
        self.bytes[self.length] = byte;
        self.length += 1;
        if self.length == self.bytes.len() {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<(), String> {
        if self.length == 0 {
            return Ok(());
        }
        self.writer
            .write_all(&[self.length as u8])
            .and_then(|()| self.writer.write_all(&self.bytes[..self.length]))
            .map_err(|error| format!("could not write GIF LZW data: {error}"))?;
        self.length = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        if self.bit_count > 0 {
            self.push_byte(self.bits as u8)?;
        }
        self.flush_block()?;
        self.writer
            .write_all(&[0])
            .map_err(|error| format!("could not finish GIF LZW data: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "genericradar-current-view-export-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create export scratch directory");
        path
    }

    #[test]
    fn filename_is_descriptive_sortable_and_windows_safe() {
        let frame = DateTime::parse_from_rfc3339("2013-05-20T19:46:01Z")
            .unwrap()
            .with_timezone(&Utc);
        let captured = DateTime::parse_from_rfc3339("2026-08-21T20:01:02Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            capture_file_base(Some("KTLX:west"), Some(frame), "DVEL / m/s", captured),
            "GenericRadar_current-view_KTLX-west_20130520T194601Z_DVEL-m-s_20260821T200102Z"
        );
    }

    #[test]
    fn downloads_prefers_the_windows_profile_and_has_a_home_fallback() {
        assert_eq!(
            downloads_directory_from(Some(PathBuf::from("C:/Users/analyst")), None),
            Some(PathBuf::from("C:/Users/analyst/Downloads"))
        );
        assert_eq!(
            downloads_directory_from(None, Some(PathBuf::from("/home/analyst"))),
            Some(PathBuf::from("/home/analyst/Downloads"))
        );
    }

    #[test]
    fn synthetic_current_view_writes_a_real_non_colliding_png() {
        let directory = scratch();
        let image = egui::ColorImage::new(
            [2, 2],
            vec![
                egui::Color32::RED,
                egui::Color32::GREEN,
                egui::Color32::BLUE,
                egui::Color32::WHITE,
            ],
        );
        let first = save_capture_png_in(&image, "safe-view", &directory).expect("write first PNG");
        let second =
            save_capture_png_in(&image, "safe-view", &directory).expect("write second PNG");
        assert_eq!(first.file_name().unwrap(), "safe-view.png");
        assert_eq!(second.file_name().unwrap(), "safe-view_2.png");
        let decoded = image::open(&first)
            .expect("decode the exported PNG")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[derive(Debug, PartialEq, Eq)]
    struct EncodedFrame {
        left: u16,
        top: u16,
        width: u16,
        height: u16,
        delay: u16,
        packed: u8,
        data_bytes: usize,
    }

    fn encoded_frames(bytes: &[u8]) -> Vec<EncodedFrame> {
        assert_eq!(&bytes[..6], b"GIF89a");
        let mut cursor = 13 + 3 * PALETTE_SIZE;
        let mut delay = 0;
        let mut packed = 0;
        let mut frames = Vec::new();
        while bytes[cursor] != 0x3b {
            match bytes[cursor] {
                0x21 if bytes[cursor + 1] == 0xf9 => {
                    packed = bytes[cursor + 3];
                    delay = u16::from_le_bytes([bytes[cursor + 4], bytes[cursor + 5]]);
                    cursor += 8;
                }
                0x21 => {
                    cursor += 2;
                    while bytes[cursor] != 0 {
                        cursor += usize::from(bytes[cursor]) + 1;
                    }
                    cursor += 1;
                }
                0x2c => {
                    let read_u16 = |offset| {
                        u16::from_le_bytes([bytes[cursor + offset], bytes[cursor + offset + 1]])
                    };
                    let left = read_u16(1);
                    let top = read_u16(3);
                    let width = read_u16(5);
                    let height = read_u16(7);
                    cursor += 11;
                    let mut data_bytes = 0;
                    while bytes[cursor] != 0 {
                        let length = usize::from(bytes[cursor]);
                        data_bytes += length;
                        cursor += length + 1;
                    }
                    cursor += 1;
                    frames.push(EncodedFrame {
                        left,
                        top,
                        width,
                        height,
                        delay,
                        packed,
                        data_bytes,
                    });
                }
                other => panic!("unexpected GIF block {other:#04x} at {cursor}"),
            }
        }
        frames
    }

    fn solid_frame(width: usize, height: usize, color: egui::Color32) -> Arc<egui::ColorImage> {
        Arc::new(egui::ColorImage::new(
            [width, height],
            vec![color; width * height],
        ))
    }

    #[test]
    fn animated_loop_preserves_exact_colors_and_repeats_forever() {
        let first = Arc::new(egui::ColorImage::new(
            [2, 2],
            vec![
                egui::Color32::from_rgb(17, 29, 43),
                egui::Color32::from_rgb(18, 29, 43),
                egui::Color32::RED,
                egui::Color32::GREEN,
            ],
        ));
        let second = solid_frame(2, 2, egui::Color32::BLUE);
        let mut bytes = Vec::new();
        write_animated_gif(&mut bytes, &[first, second], 2, 2, 700).expect("encode animated GIF");

        assert_eq!(&bytes[..6], b"GIF89a");
        assert!(
            bytes.windows(11).any(|window| window == b"NETSCAPE2.0"),
            "the exported loop must repeat forever"
        );
        let colors: Vec<[u8; 3]> = bytes[13..13 + 3 * PALETTE_SIZE]
            .chunks_exact(3)
            .map(|color| [color[0], color[1], color[2]])
            .collect();
        assert!(colors.contains(&[17, 29, 43]));
        assert!(
            colors.contains(&[18, 29, 43]),
            "the exact-color fast path must not merge colors sharing an RGB555 bin"
        );
        let frames = encoded_frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].delay, 70);
        assert_eq!(frames[1].delay, 70);
    }

    #[test]
    fn duplicate_frames_coalesce_and_changed_regions_are_cropped() {
        let first = solid_frame(8, 8, egui::Color32::BLACK);
        let mut changed = (*first).clone();
        changed.pixels[2 * 8 + 3] = egui::Color32::RED;
        changed.pixels[4 * 8 + 5] = egui::Color32::GREEN;
        let changed = Arc::new(changed);
        let mut bytes = Vec::new();
        write_animated_gif(
            &mut bytes,
            &[Arc::clone(&first), first, Arc::clone(&changed), changed],
            8,
            8,
            125,
        )
        .expect("encode cropped radar loop");

        let frames = encoded_frames(&bytes);
        assert_eq!(frames.len(), 2, "identical adjacent frames must coalesce");
        assert_eq!(frames[0].delay, 26);
        assert_eq!(frames[1].delay, 26);
        assert_eq!(
            (
                frames[1].left,
                frames[1].top,
                frames[1].width,
                frames[1].height
            ),
            (3, 2, 3, 3)
        );
        assert_eq!(frames[1].packed & 0x1c, 0x04, "delta frames must be kept");
        assert_eq!(
            frames[1].packed & 1,
            1,
            "unchanged pixels inside the changed region must be transparent"
        );
    }

    #[test]
    fn lzw_compresses_repeated_pixels_and_survives_dictionary_resets() {
        let solid = solid_frame(256, 256, egui::Color32::from_rgb(40, 70, 100));
        let patterned = Arc::new(egui::ColorImage::new(
            [256, 256],
            (0..256 * 256)
                .map(|index| {
                    let scrambled = ((index as u32).wrapping_mul(2_654_435_761) >> 16) as u8;
                    egui::Color32::from_rgb(scrambled, scrambled.rotate_left(3), 0)
                })
                .collect(),
        ));
        let mut bytes = Vec::new();
        write_animated_gif(&mut bytes, &[solid, patterned], 256, 256, 100)
            .expect("encode dictionary-width and reset exercise");
        let frames = encoded_frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert!(
            frames[0].data_bytes < 1_000,
            "a flat 65,536-pixel frame should genuinely compress, got {} bytes",
            frames[0].data_bytes
        );
        assert!(
            frames[1].data_bytes > 4_096,
            "the varied frame must exercise the full dictionary and reset path"
        );
    }

    #[test]
    fn animated_loop_rejects_empty_changed_or_unrepresentable_dimensions() {
        assert!(
            validate_gif_frames(&[])
                .unwrap_err()
                .contains("empty radar loop")
        );
        let first = solid_frame(2, 2, egui::Color32::BLACK);
        let resized = solid_frame(3, 2, egui::Color32::BLACK);
        assert!(
            validate_gif_frames(&[first, resized])
                .unwrap_err()
                .contains("dimensions changed")
        );
        let oversized = solid_frame(65_536, 1, egui::Color32::BLACK);
        assert!(
            validate_gif_frames(&[oversized])
                .unwrap_err()
                .contains("above 65,535")
        );
    }

    #[test]
    fn animated_loop_names_and_downloads_never_overwrite_existing_files() {
        let captured = DateTime::parse_from_rfc3339("2026-08-25T01:02:03Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            loop_file_base(Some("KTLX"), None, "REF", captured),
            "GenericRadar_loop_KTLX_no-frame_REF_20260825T010203Z"
        );

        let directory = scratch();
        let frame = solid_frame(4, 4, egui::Color32::RED);
        let first = save_capture_gif_in(&[Arc::clone(&frame)], "safe-loop", 100, &directory)
            .expect("save animated radar loop");
        let second = save_capture_gif_in(&[frame], "safe-loop", 100, &directory)
            .expect("save a second animated radar loop");
        assert_eq!(first.file_name().unwrap(), "safe-loop.gif");
        assert_eq!(second.file_name().unwrap(), "safe-loop_2.gif");
        assert_eq!(&std::fs::read(first).unwrap()[..6], b"GIF89a");
        let _ = std::fs::remove_dir_all(directory);
    }
}
