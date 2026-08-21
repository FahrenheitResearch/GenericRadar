//! Full-window PNG export through eframe's composited screenshot path.
//!
//! No native dialog is involved. The action asks eframe for the pixels that
//! were actually presented, tags that request so unrelated screenshot users
//! cannot be consumed accidentally, then writes a non-colliding PNG directly
//! into the user's Downloads folder on a worker thread.

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
    Saved(PathBuf),
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
                        Ok(path) => ExportUpdate::Saved(path),
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

    pub fn poll(&mut self) {
        while let Ok(update) = self.receiver.try_recv() {
            self.in_flight = false;
            match update {
                ExportUpdate::Saved(path) => {
                    self.status = Some("Exported current view".to_owned());
                    self.detail = Some(format!("Saved current view to {}", path.display()));
                }
                ExportUpdate::Failed(message) => {
                    self.status = Some("Export failed".to_owned());
                    self.detail = Some(message);
                }
            }
        }
    }
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
    let mut path = directory.join(format!("{base}.png"));
    let mut counter = 2_u32;
    while path.exists() {
        path = directory.join(format!("{base}_{counter}.png"));
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
}
