use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Instant;

use analyst_runtime::{FrameOrigin, FrameStage, Generation, LatestLaneSender, latest_lane_channel};
use eframe::egui;
use radar_core::RadarVolume;

const LOAD_LANE: u8 = 0;
const MIN_PREVIEW_RADIALS: usize = 180;
const RESULT_QUEUE_CAPACITY: usize = 8;

pub struct LoadRequest {
    pub generation: Generation,
    pub path: PathBuf,
    pub origin: FrameOrigin,
    pub final_stage: FrameStage,
    pub source_label: String,
}

/// The decoded file itself is preserved on `volume.metadata.source_path`;
/// `source_label` carries the display identity (a file path, or a live
/// site and volume time).
pub struct LoadedVolume {
    pub generation: Generation,
    pub origin: FrameOrigin,
    pub source_label: String,
    pub stage: FrameStage,
    pub volume: Arc<RadarVolume>,
    pub elapsed_ms: f32,
}

pub enum LoadUpdate {
    Started {
        generation: Generation,
        source_label: String,
    },
    Volume(LoadedVolume),
    Failed {
        generation: Generation,
        source_label: String,
        message: String,
    },
}

pub struct LoadService {
    sender: LatestLaneSender<u8, LoadRequest>,
    receiver: Receiver<LoadUpdate>,
}

impl LoadService {
    pub fn new(context: egui::Context) -> Self {
        let (request_sender, request_receiver) = latest_lane_channel::<u8, LoadRequest>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let _worker = thread::Builder::new()
            .name("radar-workstation-load".to_owned())
            .spawn(move || {
                while let Some((_lane, request)) = request_receiver.recv() {
                    process_request(request, &result_sender, &context);
                }
            })
            .expect("failed to start radar load worker");

        Self {
            sender: request_sender,
            receiver: result_receiver,
        }
    }

    pub fn request(&self, request: LoadRequest) -> Result<(), LoadRequest> {
        self.sender
            .submit(LOAD_LANE, request)
            .map(|_| ())
            .map_err(|closed| closed.0)
    }

    pub fn try_recv(&self) -> Option<LoadUpdate> {
        self.receiver.try_recv().ok()
    }
}

fn process_request(request: LoadRequest, sender: &SyncSender<LoadUpdate>, context: &egui::Context) {
    let generation = request.generation;
    let path = request.path;
    let origin = request.origin;
    let final_stage = request.final_stage;
    let source_label = request.source_label;
    let _ = sender.send(LoadUpdate::Started {
        generation,
        source_label: source_label.clone(),
    });
    context.request_repaint();

    let started = Instant::now();
    let result = std::fs::read(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))
        .and_then(|raw| {
            decode_with_previews(
                &raw,
                generation,
                &path,
                origin,
                &source_label,
                started,
                sender,
                context,
            )
        });

    match result {
        Ok(mut volume) => {
            volume.metadata.source_path = Some(path.display().to_string());
            let _ = sender.send(LoadUpdate::Volume(LoadedVolume {
                generation,
                origin,
                source_label,
                stage: final_stage,
                volume: Arc::new(volume),
                elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
            }));
        }
        Err(message) => {
            let _ = sender.send(LoadUpdate::Failed {
                generation,
                source_label,
                message,
            });
        }
    }
    context.request_repaint();
}

#[allow(clippy::too_many_arguments)]
fn decode_with_previews(
    raw: &[u8],
    generation: Generation,
    path: &Path,
    origin: FrameOrigin,
    source_label: &str,
    started: Instant,
    sender: &SyncSender<LoadUpdate>,
    context: &egui::Context,
) -> Result<RadarVolume, String> {
    let mut publish_preview = |mut preview: RadarVolume| {
        preview.metadata.source_path = Some(path.display().to_string());
        let update = LoadUpdate::Volume(LoadedVolume {
            generation,
            origin,
            source_label: source_label.to_owned(),
            stage: FrameStage::Preview,
            volume: Arc::new(preview),
            elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
        });
        if sender.try_send(update).is_ok() {
            context.request_repaint();
        }
    };

    if raw.starts_with(&[0x1f, 0x8b]) {
        nexrad_io::decode_gzip_volume_from_bytes_with_preview(
            raw,
            MIN_PREVIEW_RADIALS,
            &mut publish_preview,
        )
        .map_err(|error| error.to_string())
    } else {
        nexrad_io::decode_volume_from_bytes_with_bzip_preview(
            raw,
            MIN_PREVIEW_RADIALS,
            &mut publish_preview,
        )
        .map_err(|error| error.to_string())
    }
}
