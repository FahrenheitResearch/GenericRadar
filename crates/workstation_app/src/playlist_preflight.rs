//! Conservative RAM planning for operator-selected local file playlists.
//!
//! This is deliberately a warning estimate, not a load gate and not a claim
//! that bytes on disk equal bytes in memory. Container compression, moment
//! word width, radial count, allocator capacity, and derived I/Q moments all
//! change the decoded footprint. We use file metadata plus the same leading-
//! byte format identification as the Open window, apply format-specific
//! expansion factors, and say exactly what the estimate omits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use analyst_runtime::Generation;
use eframe::egui;
use nexrad_io::SupportedVolumeFormat;

/// Large enough to deserve an explicit planning warning, never a refusal.
pub(crate) const LARGE_PLAYLIST_WARNING_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Decoded structs, radial indexes, strings, and collection spare capacity
/// that a pure payload multiplier does not represent. Applied per selected
/// path, including a path that later fails, because preflight cannot know that
/// without performing the decode it is meant to precede.
const PER_FILE_OVERHEAD_BYTES: u64 = 4 * 1024 * 1024;

/// A path whose metadata cannot be read gets a visible conservative allowance
/// instead of disappearing from the estimate. The load will name the actual
/// read failure later if the operator continues.
const UNREADABLE_PATH_ALLOWANCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PlanningFormat {
    ArchiveIi,
    Level1Iq,
    MatlabIq,
    Hdf5Radar,
    Dorade,
    CfRadial,
    DeploymentZip,
    Unrecognised,
}

impl PlanningFormat {
    const fn from_supported(format: SupportedVolumeFormat) -> Self {
        match format {
            SupportedVolumeFormat::NexradLevel2 => Self::ArchiveIi,
            SupportedVolumeFormat::NexradLevel1TimeSeries => Self::Level1Iq,
            SupportedVolumeFormat::MatlabIqCube => Self::MatlabIq,
            SupportedVolumeFormat::OdimH5 => Self::Hdf5Radar,
            SupportedVolumeFormat::Dorade => Self::Dorade,
            SupportedVolumeFormat::CfRadial1 => Self::CfRadial,
            SupportedVolumeFormat::MobileDeploymentZip => Self::DeploymentZip,
        }
    }

    /// Planning multipliers, not measured promises. Archive II and deployment
    /// archives receive the largest allowance because their compressed input
    /// can expand into several independently allocated moment grids. I/Q
    /// inputs retain estimated moments rather than every raw pulse once a
    /// playlist file is installed, so their multiplier is lower.
    const fn multiplier(self) -> u64 {
        match self {
            Self::ArchiveIi | Self::DeploymentZip => 16,
            Self::MatlabIq | Self::Hdf5Radar | Self::CfRadial | Self::Unrecognised => 8,
            Self::Dorade => 6,
            Self::Level1Iq => 4,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ArchiveIi => "Archive II/compressed Level II",
            Self::Level1Iq => "Level 1 I/Q",
            Self::MatlabIq => "MATLAB I/Q",
            Self::Hdf5Radar => "HDF5 radar container",
            Self::Dorade => "DORADE",
            Self::CfRadial => "CfRadial",
            Self::DeploymentZip => "deployment ZIP",
            Self::Unrecognised => "unrecognised/fallback",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FileEvidence {
    input_bytes: Option<u64>,
    format: PlanningFormat,
    signature_recognised: bool,
}

/// What preflight can honestly say before any selected file is decoded.
#[derive(Clone, Debug)]
pub(crate) struct PlaylistRamEstimate {
    pub file_count: usize,
    pub input_bytes: u64,
    pub estimated_decoded_bytes: u64,
    pub metadata_unavailable: usize,
    pub signature_recognised: usize,
    formats: BTreeMap<PlanningFormat, usize>,
}

pub(crate) struct PlaylistPreflightUpdate {
    pub generation: Generation,
    pub paths: Vec<PathBuf>,
    pub estimate: PlaylistRamEstimate,
}

/// Generation-tagged metadata/signature planning.
///
/// Paths may live on slow or unavailable network mounts, so neither metadata
/// nor the 8 KiB signature read is ever performed from egui's update thread.
/// A generation on every result lets the app cancel or supersede a selection
/// without an old worker result starting it later. Each explicit selection has
/// its own worker because an operating-system read on a dead mount cannot be
/// cancelled safely; a stale blocked read therefore never starves the next
/// selection.
pub(crate) struct PlaylistPreflightService {
    sender: Sender<PlaylistPreflightUpdate>,
    receiver: Receiver<PlaylistPreflightUpdate>,
    context: egui::Context,
}

impl PlaylistPreflightService {
    pub fn new(context: egui::Context) -> Self {
        let (result_sender, result_receiver) = mpsc::channel();
        Self {
            sender: result_sender,
            receiver: result_receiver,
            context,
        }
    }

    pub fn request(&self, generation: Generation, paths: Vec<PathBuf>) -> Result<(), String> {
        let sender = self.sender.clone();
        let context = self.context.clone();
        thread::Builder::new()
            .name(format!(
                "radar-workstation-playlist-preflight-{}",
                generation.get()
            ))
            .spawn(move || {
                let estimate = estimate_paths(&paths);
                if sender
                    .send(PlaylistPreflightUpdate {
                        generation,
                        paths,
                        estimate,
                    })
                    .is_ok()
                {
                    context.request_repaint();
                }
            })
            .map(|_worker| ())
            .map_err(|error| error.to_string())
    }

    pub fn try_recv(&self) -> Option<PlaylistPreflightUpdate> {
        self.receiver.try_recv().ok()
    }
}

impl PlaylistRamEstimate {
    pub fn requires_confirmation(&self) -> bool {
        self.estimated_decoded_bytes > LARGE_PLAYLIST_WARNING_BYTES
    }

    pub fn input_size_text(&self) -> String {
        let known = format_binary_bytes(self.input_bytes);
        if self.metadata_unavailable == 0 {
            known
        } else {
            format!(
                "{known} known; metadata unavailable for {} path(s)",
                self.metadata_unavailable
            )
        }
    }

    pub fn method_text(&self) -> String {
        let classes = self
            .formats
            .iter()
            .map(|(format, count)| format!("{count} {} ×{}", format.label(), format.multiplier()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "File sizes from local metadata; first {} KiB signatures recognised for {}/{} \
             path(s); {classes}; plus {} MiB per path for decoded bookkeeping.",
            crate::file_browser::HEAD_BYTES / 1024,
            self.signature_recognised,
            self.file_count,
            PER_FILE_OVERHEAD_BYTES / (1024 * 1024)
        )
    }

    pub const fn caveat_text(&self) -> &'static str {
        "Planning estimate only. Compression ratio, gates, moments, allocator capacity, render \
         textures, derived-product caches, and worker scratch space can make actual process RAM \
         lower or higher. Playlist I/Q pulses are decoded one file at a time and are not retained \
         after their estimated moment volume is installed."
    }
}

/// Inspect metadata and only the small leading-byte window used by the file
/// browser. No selected file is decoded here.
pub(crate) fn estimate_paths(paths: &[PathBuf]) -> PlaylistRamEstimate {
    let evidence = paths.iter().map(|path| inspect_path(path));
    estimate_evidence(evidence)
}

fn inspect_path(path: &Path) -> FileEvidence {
    let input_bytes = std::fs::metadata(path).ok().map(|metadata| metadata.len());
    let identity = crate::file_browser::identify(path);
    let (format, signature_recognised) = match identity {
        crate::file_browser::FileIdentity::Radar(format) => {
            (PlanningFormat::from_supported(format), true)
        }
        crate::file_browser::FileIdentity::Unread
        | crate::file_browser::FileIdentity::Unrecognised
        | crate::file_browser::FileIdentity::Unreadable(_) => (PlanningFormat::Unrecognised, false),
    };
    FileEvidence {
        input_bytes,
        format,
        signature_recognised,
    }
}

fn estimate_evidence(evidence: impl IntoIterator<Item = FileEvidence>) -> PlaylistRamEstimate {
    let mut estimate = PlaylistRamEstimate {
        file_count: 0,
        input_bytes: 0,
        estimated_decoded_bytes: 0,
        metadata_unavailable: 0,
        signature_recognised: 0,
        formats: BTreeMap::new(),
    };

    for file in evidence {
        estimate.file_count = estimate.file_count.saturating_add(1);
        if file.signature_recognised {
            estimate.signature_recognised = estimate.signature_recognised.saturating_add(1);
        }
        *estimate.formats.entry(file.format).or_default() += 1;
        let decoded_payload = match file.input_bytes {
            Some(input_bytes) => {
                estimate.input_bytes = estimate.input_bytes.saturating_add(input_bytes);
                input_bytes.saturating_mul(file.format.multiplier())
            }
            None => {
                estimate.metadata_unavailable = estimate.metadata_unavailable.saturating_add(1);
                UNREADABLE_PATH_ALLOWANCE_BYTES
            }
        };
        estimate.estimated_decoded_bytes = estimate
            .estimated_decoded_bytes
            .saturating_add(decoded_payload)
            .saturating_add(PER_FILE_OVERHEAD_BYTES);
    }
    estimate
}

pub(crate) fn format_binary_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= TIB {
        format!("{:.1} TiB", bytes / TIB)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(input_bytes: u64, format: PlanningFormat) -> FileEvidence {
        FileEvidence {
            input_bytes: Some(input_bytes),
            format,
            signature_recognised: true,
        }
    }

    #[test]
    fn one_file_warning_is_strictly_above_the_16_gib_planning_threshold() {
        let multiplier = PlanningFormat::Unrecognised.multiplier();
        let at_threshold_input =
            (LARGE_PLAYLIST_WARNING_BYTES - PER_FILE_OVERHEAD_BYTES) / multiplier;
        let at_threshold =
            estimate_evidence([known(at_threshold_input, PlanningFormat::Unrecognised)]);
        assert_eq!(
            at_threshold.estimated_decoded_bytes,
            LARGE_PLAYLIST_WARNING_BYTES
        );
        assert!(!at_threshold.requires_confirmation());

        let above =
            estimate_evidence([known(at_threshold_input + 1, PlanningFormat::Unrecognised)]);
        assert!(above.estimated_decoded_bytes > LARGE_PLAYLIST_WARNING_BYTES);
        assert!(above.requires_confirmation());
    }

    #[test]
    fn estimate_keeps_all_1075_selected_path_evidence() {
        let evidence = (0..1_075)
            .map(|_| known(16 * 1024 * 1024, PlanningFormat::ArchiveIi))
            .collect::<Vec<_>>();
        let estimate = estimate_evidence(evidence);

        assert_eq!(estimate.file_count, 1_075);
        assert_eq!(estimate.signature_recognised, 1_075);
        assert!(estimate.requires_confirmation());
        assert!(estimate.method_text().contains("1075 Archive II"));
    }

    #[test]
    fn unreadable_metadata_is_counted_and_never_treated_as_zero_ram() {
        let estimate = estimate_evidence([FileEvidence {
            input_bytes: None,
            format: PlanningFormat::Unrecognised,
            signature_recognised: false,
        }]);
        assert_eq!(estimate.file_count, 1);
        assert_eq!(estimate.input_bytes, 0);
        assert_eq!(estimate.metadata_unavailable, 1);
        assert_eq!(
            estimate.estimated_decoded_bytes,
            UNREADABLE_PATH_ALLOWANCE_BYTES + PER_FILE_OVERHEAD_BYTES
        );
        assert!(estimate.input_size_text().contains("metadata unavailable"));
    }
}
