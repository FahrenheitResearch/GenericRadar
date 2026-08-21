//! In-app browser for public research-radar data.
//!
//! Network and catalog semantics live in `data_source::research_archive`; this
//! module owns only the egui state and a worker thread. No request runs on the
//! UI thread, including catalog navigation.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use data_source::research_archive::{
    self, DownloadedResearchFile, NsslCatalog, NsslCatalogPath, NsslLevel1File,
};
use eframe::egui;

const REQUEST_QUEUE_CAPACITY: usize = 2;
const UPDATE_QUEUE_CAPACITY: usize = 64;

enum Request {
    Catalog(NsslCatalogPath),
    Download {
        file: NsslLevel1File,
        directory: PathBuf,
        cancelled: Arc<AtomicBool>,
    },
}

enum Update {
    Catalog {
        result: data_source::Result<NsslCatalog>,
    },
    DownloadProgress {
        name: String,
        copied: u64,
        total: Option<u64>,
    },
    DownloadFinished {
        name: String,
        result: data_source::Result<DownloadedResearchFile>,
    },
}

struct OnlineDataService {
    sender: SyncSender<Request>,
    receiver: Receiver<Update>,
}

impl OnlineDataService {
    fn new(context: egui::Context) -> Self {
        let (request_sender, request_receiver) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
        let (update_sender, update_receiver) = mpsc::sync_channel(UPDATE_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("radar-online-data".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    match request {
                        Request::Catalog(path) => {
                            let result = research_archive::list_nssl_koun_catalog(&path);
                            let _ = update_sender.send(Update::Catalog { result });
                        }
                        Request::Download {
                            file,
                            directory,
                            cancelled,
                        } => {
                            let name = file.name.clone();
                            let progress_name = name.clone();
                            let progress_sender = update_sender.clone();
                            let repaint = context.clone();
                            let progress = move |copied, total| {
                                let _ = progress_sender.try_send(Update::DownloadProgress {
                                    name: progress_name.clone(),
                                    copied,
                                    total,
                                });
                                repaint.request_repaint();
                            };
                            let result = research_archive::download_nssl_level1_file(
                                &file,
                                &directory,
                                &progress,
                                &|| cancelled.load(Ordering::Relaxed),
                            );
                            let _ = update_sender.send(Update::DownloadFinished { name, result });
                        }
                    }
                    context.request_repaint();
                }
            })
            .expect("failed to start online-data worker");
        Self {
            sender: request_sender,
            receiver: update_receiver,
        }
    }
}

enum Activity {
    Idle,
    LoadingCatalog,
    Downloading {
        name: String,
        copied: u64,
        total: Option<u64>,
        cancelled: Arc<AtomicBool>,
        open_when_done: bool,
    },
}

impl Activity {
    fn busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

pub(super) struct OnlineDataBrowser {
    open: bool,
    service: OnlineDataService,
    catalog: Option<NsslCatalog>,
    history: Vec<NsslCatalogPath>,
    selected: Option<NsslLevel1File>,
    filter: String,
    activity: Activity,
    status: String,
    status_is_error: bool,
    downloads_dir: Option<PathBuf>,
    open_after_download: bool,
    pending_open: Option<PathBuf>,
}

impl OnlineDataBrowser {
    pub(super) fn new(context: egui::Context) -> Self {
        Self {
            open: false,
            service: OnlineDataService::new(context),
            catalog: None,
            history: Vec::new(),
            selected: None,
            filter: String::new(),
            activity: Activity::Idle,
            status: String::new(),
            status_is_error: false,
            downloads_dir: research_archive::default_downloads_directory(),
            open_after_download: true,
            pending_open: None,
        }
    }

    pub(super) fn open(&mut self) {
        self.open = true;
        if self.catalog.is_none() && !self.activity.busy() {
            self.request_catalog(NsslCatalogPath::verified_case(), false);
        }
    }

    /// Draw if open and always drain the worker. A completed download can ask
    /// the application to open its file even if the window was closed while
    /// the transfer ran.
    pub(super) fn draw(&mut self, context: &egui::Context) -> Option<PathBuf> {
        self.poll();
        if self.open {
            let mut window_open = true;
            egui::Window::new("Download research radar data")
                .id(egui::Id::new("workstation-online-research-data"))
                .open(&mut window_open)
                .default_size([820.0, 620.0])
                .min_size([620.0, 440.0])
                .show(context, |ui| self.contents(ui));
            self.open = window_open;
        }
        self.pending_open.take()
    }

    fn poll(&mut self) {
        while let Ok(update) = self.service.receiver.try_recv() {
            match update {
                Update::Catalog { result } => {
                    self.activity = Activity::Idle;
                    match result {
                        Ok(catalog) => {
                            let count = catalog.directories.len() + catalog.files.len();
                            self.status = format!("{count} catalog entries");
                            self.status_is_error = false;
                            self.catalog = Some(catalog);
                            self.selected = None;
                        }
                        Err(error) => {
                            self.status = format!("Catalog failed: {error}");
                            self.status_is_error = true;
                        }
                    }
                }
                Update::DownloadProgress {
                    name,
                    copied,
                    total,
                } => {
                    if let Activity::Downloading {
                        name: active,
                        copied: active_copied,
                        total: active_total,
                        ..
                    } = &mut self.activity
                        && *active == name
                    {
                        *active_copied = copied;
                        *active_total = total;
                    }
                }
                Update::DownloadFinished { name, result } => {
                    let open_when_done = matches!(
                        &self.activity,
                        Activity::Downloading {
                            name: active,
                            open_when_done: true,
                            ..
                        } if *active == name
                    );
                    self.activity = Activity::Idle;
                    match result {
                        Ok(downloaded) => {
                            self.status = format!(
                                "Downloaded {} to {}",
                                format_bytes(downloaded.bytes),
                                downloaded.path.display()
                            );
                            self.status_is_error = false;
                            if open_when_done {
                                self.pending_open = Some(downloaded.path);
                                self.open = false;
                            }
                        }
                        Err(data_source::DataSourceError::ResearchDownloadCancelled { .. }) => {
                            self.status =
                                "Download cancelled; the partial file was removed".to_owned();
                            self.status_is_error = false;
                        }
                        Err(error) => {
                            self.status = format!("Download failed: {error}");
                            self.status_is_error = true;
                        }
                    }
                }
            }
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui) {
        ui.heading("NOAA/NSSL KOUN Level I (I/Q)");
        ui.label(
            "This is a public, machine-readable archive of raw RVP8/RVP900 time series. \
             It is not a universal research-radar archive: OU, DOW, NCAR/EOL and ARM data \
             live in separate catalogs with different access rules.",
        );
        ui.horizontal_wrapped(|ui| {
            ui.hyperlink_to(
                "Official NSSL catalog and terms",
                research_archive::NSSL_KOUN_CATALOG_PAGE,
            );
            ui.label(
                "Files are saved directly to Downloads; existing files are never overwritten.",
            );
        });
        ui.separator();

        let busy = self.activity.busy();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Verified 20 May 2013 case"))
                .on_hover_text(
                    "The real-weather case used to verify this application's Level-I reader.",
                )
                .clicked()
            {
                self.history.clear();
                self.request_catalog(NsslCatalogPath::verified_case(), false);
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Archive root"))
                .clicked()
            {
                self.history.clear();
                self.request_catalog(NsslCatalogPath::root(), false);
            }
            if ui
                .add_enabled(!busy && !self.history.is_empty(), egui::Button::new("Back"))
                .clicked()
                && let Some(path) = self.history.pop()
            {
                self.request_catalog(path, false);
            }
            if matches!(self.activity, Activity::LoadingCatalog) {
                ui.spinner();
                ui.label("Reading official catalog…");
            }
        });

        if let Some(catalog) = &self.catalog {
            ui.monospace(format!("NSSL / KOUN / {}", catalog.path.display()));
        }
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .desired_width(f32::INFINITY)
                .hint_text("Filter folders or Level-I filenames"),
        );

        let needle = self.filter.trim().to_ascii_lowercase();
        let mut navigate = None;
        let mut selected = None;
        egui::ScrollArea::vertical()
            .id_salt("nssl-level1-catalog-entries")
            .max_height(315.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if let Some(catalog) = &self.catalog {
                    for directory in &catalog.directories {
                        if !needle.is_empty()
                            && !directory.name.to_ascii_lowercase().contains(&needle)
                        {
                            continue;
                        }
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(format!("Folder  {}", directory.name))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            navigate = Some(directory.path.clone());
                        }
                    }
                    for file in &catalog.files {
                        if !needle.is_empty() && !file.name.to_ascii_lowercase().contains(&needle) {
                            continue;
                        }
                        let chosen = self
                            .selected
                            .as_ref()
                            .is_some_and(|current| current.name == file.name);
                        let size = file
                            .estimated_size_bytes
                            .map(format_bytes)
                            .unwrap_or_else(|| "size unknown".to_owned());
                        let badge = if file.is_verified_sample() {
                            "  VERIFIED SAMPLE"
                        } else if file.is_known_truncated() {
                            "  KNOWN TRUNCATED"
                        } else {
                            ""
                        };
                        let label = format!("{}   {size}{badge}", file.name);
                        if ui.selectable_label(chosen, label).clicked() {
                            selected = Some(file.clone());
                        }
                    }
                    if catalog.directories.is_empty() && catalog.files.is_empty() {
                        ui.weak("This catalog level contains no KOUN RVP Level-I files.");
                    }
                }
            });

        if let Some(path) = navigate
            && let Some(current) = self.catalog.as_ref().map(|catalog| catalog.path.clone())
        {
            self.history.push(current);
            self.request_catalog(path, false);
        }
        if let Some(file) = selected {
            self.selected = Some(file);
        }

        ui.separator();
        if let Some(file) = &self.selected {
            ui.label(egui::RichText::new(&file.name).strong());
            if file.is_verified_sample() {
                ui.label(
                    "Verified complete: this exact record is part of the real-data Level-I regression.",
                );
            } else if file.is_known_truncated() {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "The public object is truncated mid-pulse. The reader correctly refuses it; download is disabled.",
                );
            } else {
                ui.label(
                    "Official archive record. It has not been individually certified by this project's real-data tests.",
                );
            }
        } else {
            ui.weak("Select a Level-I record above.");
        }

        if let Some(directory) = &self.downloads_dir {
            ui.monospace(format!("Destination: {}", directory.display()));
        } else {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "No per-user Downloads directory is available in this environment.",
            );
        }
        ui.checkbox(
            &mut self.open_after_download,
            "Open in the workstation after download",
        );

        match &self.activity {
            Activity::Downloading {
                copied,
                total,
                cancelled,
                ..
            } => {
                let fraction = total
                    .filter(|total| *total > 0)
                    .map_or(0.0, |total| (*copied as f32 / total as f32).clamp(0.0, 1.0));
                let text = total.map_or_else(
                    || format!("{} downloaded", format_bytes(*copied)),
                    |total| format!("{} / {}", format_bytes(*copied), format_bytes(total)),
                );
                ui.add(egui::ProgressBar::new(fraction).text(text));
                if ui.button("Cancel download").clicked() {
                    cancelled.store(true, Ordering::Relaxed);
                }
            }
            _ => {
                let downloadable = self
                    .selected
                    .as_ref()
                    .is_some_and(|file| !file.is_known_truncated())
                    && self.downloads_dir.is_some()
                    && !busy;
                if ui
                    .add_enabled(downloadable, egui::Button::new("Download to Downloads"))
                    .clicked()
                {
                    self.request_download();
                }
            }
        }

        if !self.status.is_empty() {
            if self.status_is_error {
                ui.colored_label(ui.visuals().error_fg_color, &self.status);
            } else {
                ui.label(&self.status);
            }
        }
    }

    fn request_catalog(&mut self, path: NsslCatalogPath, preserve_selection: bool) {
        match self.service.sender.try_send(Request::Catalog(path)) {
            Ok(()) => {
                self.activity = Activity::LoadingCatalog;
                self.status = "Reading NOAA/NSSL catalog…".to_owned();
                self.status_is_error = false;
                if !preserve_selection {
                    self.selected = None;
                }
            }
            Err(error) => {
                self.status = format!("Could not queue catalog request: {error}");
                self.status_is_error = true;
            }
        }
    }

    fn request_download(&mut self) {
        let (Some(file), Some(directory)) = (self.selected.clone(), self.downloads_dir.clone())
        else {
            return;
        };
        if file.is_known_truncated() {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request::Download {
            file: file.clone(),
            directory,
            cancelled: cancelled.clone(),
        };
        match self.service.sender.try_send(request) {
            Ok(()) => {
                self.activity = Activity::Downloading {
                    name: file.name,
                    copied: 0,
                    total: file.estimated_size_bytes,
                    cancelled,
                    open_when_done: self.open_after_download,
                };
                self.status = "Starting secure download from NOAA/NSSL…".to_owned();
                self.status_is_error = false;
            }
            Err(error) => {
                self.status = format!("Could not queue download: {error}");
                self.status_is_error = true;
            }
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_readouts_are_compact_and_binary() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(16 * 1024 * 1024), "16.0 MiB");
    }
}
