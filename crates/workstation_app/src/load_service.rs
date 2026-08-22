use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Instant;

use analyst_runtime::{FrameOrigin, FrameStage, Generation, LatestLaneSender, latest_lane_channel};
use eframe::egui;
use nexrad_io::SupportedVolumeFormat;
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
    /// The estimator settings a NEXRAD Level 1 (time series) file is to be
    /// processed with, if this turns out to be one.
    ///
    /// Carried on the request rather than read by the worker because the
    /// worker has no settings store, and defaulted rather than optional
    /// because a caller that forgot would otherwise get a silently different
    /// picture from the one the settings page is showing.
    pub iq_controls: crate::iq_session::IqControls,
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
    /// Internal Archive II evidence that this complete file is one sweep of a
    /// larger logical source volume. `None` means the conservative classifier
    /// refused assembly; the filename is never used as a fallback key.
    pub assembly: Option<nexrad_io::sweep_assembly::ProvenSweepMembership>,
    /// The classifier's reason when `assembly` is absent. The playlist keeps
    /// candidate-specific refusals for its completion detail; ordinary formats
    /// and already-complete volumes remain quiet.
    pub assembly_refusal: Option<nexrad_io::sweep_assembly::SweepAssemblyRefusal>,
    /// Present only for a NEXRAD Level 1 (time series) file: the pulses the
    /// volume above was estimated FROM, so the knobs can re-run the estimator
    /// and the spectrum readout can transform a gate without the file being
    /// read again. `None` for every ordinary volume, which arrives with its
    /// moments already made.
    pub iq: Option<Box<crate::iq_session::IqSession>>,
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
                request.iq_controls,
                started,
                sender,
                context,
            )
            .map(|decoded| (raw, decoded))
        });

    match result {
        Ok((raw, Decoded { mut volume, iq })) => {
            let (assembly, assembly_refusal) =
                match nexrad_io::sweep_assembly::classify_archive_sweep(&raw, &volume) {
                    nexrad_io::sweep_assembly::SweepAssemblyClassification::Proven(evidence) => {
                        (Some(evidence), None)
                    }
                    nexrad_io::sweep_assembly::SweepAssemblyClassification::Refused(reason) => {
                        (None, Some(reason))
                    }
                };
            // Normally the file is the whole answer. A deployment archive is
            // not: its reader has already written which member this scan came
            // from, and that half is the informative one, so the file name is
            // joined onto it rather than written over it.
            volume.metadata.source_path = Some(match volume.metadata.source_path.take() {
                Some(member) => format!("{}::{member}", path.display()),
                None => path.display().to_string(),
            });
            let _ = sender.send(LoadUpdate::Volume(LoadedVolume {
                generation,
                origin,
                source_label,
                stage: final_stage,
                volume: Arc::new(volume),
                elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
                assembly,
                assembly_refusal,
                iq,
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

/// What a decode produced: a volume, and for a time-series record the pulses it
/// was estimated from.
struct Decoded {
    volume: RadarVolume,
    iq: Option<Box<crate::iq_session::IqSession>>,
}

impl From<RadarVolume> for Decoded {
    fn from(volume: RadarVolume) -> Self {
        Self { volume, iq: None }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_with_previews(
    raw: &[u8],
    generation: Generation,
    path: &Path,
    origin: FrameOrigin,
    source_label: &str,
    iq_controls: crate::iq_session::IqControls,
    started: Instant,
    sender: &SyncSender<LoadUpdate>,
    context: &egui::Context,
) -> Result<Decoded, String> {
    let publish_preview = |mut preview: RadarVolume| {
        preview.metadata.source_path = Some(path.display().to_string());
        let update = LoadUpdate::Volume(LoadedVolume {
            generation,
            origin,
            source_label: source_label.to_owned(),
            stage: FrameStage::Preview,
            volume: Arc::new(preview),
            elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
            assembly: None,
            assembly_refusal: None,
            iq: None,
        });
        if sender.try_send(update).is_ok() {
            context.request_repaint();
        }
    };

    // Which decoder owns these bytes is decided once, by magic number, in the
    // io crate - not here by file extension, because radar volumes are
    // routinely stored with no extension and the same extension means
    // different formats on different networks.
    match nexrad_io::sniff_supported_volume_bytes(raw) {
        // Unrecognised bytes go to the Archive II decoder on purpose: its
        // error message is the useful one for a file that is not radar data
        // at all, and it is what this path did before the seam existed.
        Some(SupportedVolumeFormat::NexradLevel2) | None => {
            decode_level2_with_previews(raw, publish_preview).map(Decoded::from)
        }
        // NEXRAD Level 1 is the one container that holds no moments to decode.
        // It carries the transmitted pulses, so the moments on screen are the
        // ones this application estimates from them - which is why the sweep
        // travels back beside the volume rather than being dropped here.
        Some(SupportedVolumeFormat::NexradLevel1TimeSeries) => {
            decode_time_series(raw, source_label, iq_controls)
        }
        Some(SupportedVolumeFormat::MatlabIqCube) => {
            decode_matlab_iq(raw, source_label, iq_controls)
        }
        // Progressive preview is a property of the Archive II record stream;
        // the other containers decode whole or not at all.
        Some(_) => nexrad_io::decode_supported_volume_bytes(raw)
            .map_err(|error| error.to_string())
            .map(Decoded::from),
    }
}

/// Decode a NEXRAD Level 1 record and estimate its moments.
///
/// The site the record names is carried through with no position on it. Filling
/// one in is the application's job, not this worker's: the record states a
/// signal-processor name and no coordinates, and the two catalogs that know
/// where `KOUN` is - the station directory and the sourced research table -
/// both live on the UI side. A guess made here would be a guess nobody could
/// see.
fn decode_time_series(
    raw: &[u8],
    source_label: &str,
    controls: crate::iq_session::IqControls,
) -> Result<Decoded, String> {
    let session = crate::iq_session::IqSession::open(raw, source_label, controls)?;
    decoded_iq_session(session)
}

/// Decode a MATLAB Level 5 OU-PRIME cube without inventing receiver or
/// reflectivity calibration. The cube-to-sweep conversion records its native
/// ray boundaries, and `IqSession` fixes processing to one dwell per ray.
fn decode_matlab_iq(
    raw: &[u8],
    source_label: &str,
    controls: crate::iq_session::IqControls,
) -> Result<Decoded, String> {
    let sweep = nexrad_io::matlab_iq::decode_ou_prime_mat(raw)
        .and_then(nexrad_io::matlab_iq::OuPrimeIqCube::into_iq_sweep)
        .map_err(|error| format!("MATLAB Level 5 I/Q cube: {error}"))?;
    let session = crate::iq_session::IqSession::from_sweep(sweep, source_label, controls)?;
    decoded_iq_session(session)
}

fn decoded_iq_session(session: crate::iq_session::IqSession) -> Result<Decoded, String> {
    let volume = session.volume(radar_core::RadarSite::new(session.site_id()));
    Ok(Decoded {
        volume,
        iq: Some(Box::new(session)),
    })
}

/// The Archive II path, which can hand the UI a first displayable cut before
/// the rest of the volume has been decoded.
fn decode_level2_with_previews(
    raw: &[u8],
    mut publish_preview: impl FnMut(RadarVolume),
) -> Result<RadarVolume, String> {
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

/// The io crate's real-data fixtures.
///
/// The load path under test is the app's, but the bytes belong to the
/// decoders: copying a megabyte of real radar into a second directory to
/// test the same bytes twice would waste it, and the two copies would
/// drift apart the first time a fixture was refreshed.
///
/// At module level, and `pub(crate)`, because the load path is no longer the
/// only test that wants a real volume of a named format: `app` draws the
/// toolbar over these same files to check what each format can and cannot
/// tell an analyst. One spelling of where the fixtures live.
#[cfg(test)]
pub(crate) fn io_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("nexrad_io")
        .join("tests")
        .join("data")
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::MomentType;

    /// Run the load path's decode step on some bytes, discarding previews.
    ///
    /// `egui::Context::default()` is a real context and works without a
    /// window, so this exercises the shipped function rather than a copy of
    /// its logic.
    fn decode(raw: &[u8]) -> Result<RadarVolume, String> {
        decode_all(raw).map(|decoded| decoded.volume)
    }

    /// The whole decode, including a time series' pulses.
    fn decode_all(raw: &[u8]) -> Result<Decoded, String> {
        let (sender, receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let result = decode_with_previews(
            raw,
            Generation::default(),
            Path::new("fixture"),
            FrameOrigin::Local,
            "fixture",
            crate::iq_session::IqControls::default(),
            Instant::now(),
            &sender,
            &context,
        );
        drop(receiver);
        result
    }

    #[test]
    fn a_broken_file_is_named_by_its_own_format_rather_than_misparsed() {
        // An HDF5 signature with nothing behind it. Before the seam these
        // bytes went to the Archive II parser and came back as a complaint
        // about a short volume header, which sends the analyst looking for a
        // truncated NEXRAD download that does not exist. Now the ODIM
        // decoder gets them and the message says so.
        let error = decode(b"\x89HDF\r\n\x1a\n\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")
            .unwrap_err();
        assert!(
            error.contains("ODIM_H5"),
            "the error should name the format, got {error:?}"
        );
    }

    /// A failed decode names the container it was taken for, whichever
    /// module owns that container.
    #[test]
    fn a_failed_decode_names_the_container_the_seam_chose() {
        for (bytes, expected) in [
            (
                b"CDF\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".as_slice(),
                "CfRadial",
            ),
            (
                b"PK\x03\x04\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".as_slice(),
                "zip",
            ),
        ] {
            let error = decode(bytes).unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?} to be named, got {error:?}"
            );
        }
    }

    #[test]
    fn a_file_too_short_to_be_a_volume_is_reported_as_an_error() {
        for bytes in [b"".as_slice(), b"nope".as_slice()] {
            let error = decode(bytes).unwrap_err();
            assert!(!error.is_empty(), "the failure must carry a message");
        }
    }

    /// Junk longer than a 24-byte volume header decodes to a volume with no
    /// cuts rather than to an error, because `parse_volume_header` only
    /// slices bytes and the message loop then finds nothing to read.
    ///
    /// This is the surface as it was before the routing seam existed, and the
    /// seam deliberately preserves it: unrecognised bytes still go to the
    /// Archive II decoder. It is pinned here because it is surprising - a
    /// load that "succeeds" with nothing to draw - so that whoever decides to
    /// change it does so on purpose and sees this test fail.
    #[test]
    fn unrecognised_bytes_still_reach_the_archive_ii_decoder_and_never_panic() {
        let volume = decode(b"this is not a radar volume at all").expect("no panic, no error");
        assert!(volume.cuts.is_empty());
        assert_eq!(volume.metadata.decoded_radial_count, 0);
    }

    // -----------------------------------------------------------------
    // One real file per format, all the way through the load path.
    //
    // These are the pins that say the routing seam is actually wired: each
    // one starts from a path the way a drop does, lets `app_support` choose
    // it out of the drop, and lets `process_request` read, sniff, route and
    // decode it. A regression in any one of the four format modules, or in
    // the router that reaches them, fails here rather than only inside the
    // io crate's own tests.
    // -----------------------------------------------------------------

    /// Drive one file through the load path exactly as a drop does.
    ///
    /// The drop handler picks the file out of what was dropped, the load
    /// worker reads it, the seam sniffs it and the decoder that owns the
    /// container decodes it. Returns the complete volume, or the message the
    /// analyst would have seen.
    fn load_as_a_drop_would(path: &Path) -> Result<Arc<RadarVolume>, String> {
        load_complete_as_a_drop_would(path).map(|loaded| loaded.volume)
    }

    /// Same path as [`load_as_a_drop_would`], retaining the I/Q session so a
    /// test can prove the pulses were not discarded after rendering a volume.
    fn load_complete_as_a_drop_would(path: &Path) -> Result<LoadedVolume, String> {
        let chosen = crate::app_support::choose_dropped_radar_file([path.to_path_buf()])
            .expect("a drop of one file chooses that file");
        let (sender, receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let source_label = chosen.display().to_string();
        process_request(
            LoadRequest {
                iq_controls: crate::iq_session::IqControls::default(),
                generation: Generation::default(),
                path: chosen,
                origin: FrameOrigin::Local,
                final_stage: FrameStage::Complete,
                source_label,
            },
            &sender,
            &egui::Context::default(),
        );
        drop(sender);
        let mut outcome = Err("the load path produced no result at all".to_owned());
        while let Ok(update) = receiver.try_recv() {
            match update {
                LoadUpdate::Volume(loaded) if loaded.stage == FrameStage::Complete => {
                    outcome = Ok(loaded);
                }
                LoadUpdate::Failed { message, .. } => outcome = Err(message),
                _ => {}
            }
        }
        outcome
    }

    #[test]
    fn the_load_path_decodes_a_real_odim_h5_volume() {
        // SMHI Angelholm, 2026-08-20 00:00 UTC, one 0.5 deg sweep of
        // DBZH + TH + VRADH (OPERA ORD, CC BY 4.0).
        let volume = load_as_a_drop_would(&io_fixture("seang.scan.20260820.dbzh_th_vradh.h5"))
            .expect("ODIM_H5 should decode through the load path");
        assert_eq!(volume.site.id, "SEANG");
        assert_eq!(volume.cuts.len(), 1);
        assert_eq!(volume.metadata.decoded_radial_count, 360);
        assert!(
            volume.cuts[0].moments.contains_key(&MomentType::Velocity),
            "the velocity plane must survive the copied-what-group recovery"
        );
    }

    #[test]
    fn the_load_path_decodes_a_real_cfradial_volume() {
        // ARM X-SAPR at SGP, 2011-05-20, a 40-ray classic-netCDF PPI.
        let volume = load_as_a_drop_would(&io_fixture("cfrad.xsapr_sgp_ppi_20110520.classic.nc"))
            .expect("CfRadial 1.x should decode through the load path");
        assert_eq!(volume.site.id, "xsapr-sgp");
        assert_eq!(volume.cuts.len(), 1);
        assert_eq!(volume.metadata.decoded_radial_count, 40);
        assert!(
            volume.cuts[0]
                .moments
                .contains_key(&MomentType::Reflectivity)
        );
    }

    #[test]
    fn the_load_path_decodes_a_real_dorade_sweepfile() {
        // VORTEX-2 NOXP, 2009-05-09 (Zenodo doi:10.5281/zenodo.14194361,
        // CC BY 4.0). Its name has no usable extension, which is why the
        // drop handler has to know the `swp.` convention.
        let path = io_fixture("swp.1090509143923.NOXPRVP.0.0.5_PPI_v1.head3");
        let volume = load_as_a_drop_would(&path)
            .expect("a DORADE sweepfile should decode through the load path");
        assert_eq!(volume.site.id, "NOXPRVP");
        assert_eq!(volume.cuts.len(), 1);
        assert_eq!(volume.metadata.decoded_radial_count, 3);
        assert_eq!(
            volume.cuts[0].moments[&MomentType::Reflectivity]
                .gate_range
                .gate_count,
            1001,
            "the trailing block-padding word is not a gate"
        );
    }

    #[test]
    fn the_load_path_decodes_a_real_deployment_archive_and_says_which_sweep() {
        // A CPython `zipfile` archive of two real sweepfiles. A pane draws
        // one volume, so the load path opens the earliest scan in the
        // bundle - the 2009 NOXP sweep, not the 2026 COW2 one.
        let path = io_fixture("deployment_python_zipfile.zip.bin");
        let volume = load_as_a_drop_would(&path)
            .expect("a deployment archive should decode through the load path");
        assert_eq!(volume.site.id, "NOXPRVP");
        assert_eq!(volume.metadata.decoded_radial_count, 3);

        // The member is the informative half of "where did this come from",
        // so the archive path keeps it instead of letting the file name
        // overwrite it.
        let source = volume
            .metadata
            .source_path
            .as_deref()
            .expect("a load records where it came from");
        assert!(
            source.starts_with(&path.display().to_string()),
            "the archive file should still be named, got {source:?}"
        );
        assert!(
            source.ends_with("swp.1090509143923.NOXPRVP.0.0.5_PPI_v1"),
            "the member should be named too, got {source:?}"
        );
    }

    #[test]
    fn the_load_path_retains_a_real_matlab_iq_session_without_fabricated_products() {
        let Ok(path) = std::env::var("OU_PRIME_MAT_SAMPLE") else {
            eprintln!("skipping: set OU_PRIME_MAT_SAMPLE to the OU-PRIME MAT file");
            return;
        };
        let path = PathBuf::from(path);
        if !path.exists() {
            eprintln!(
                "skipping: OU_PRIME_MAT_SAMPLE points at {}, which is not there",
                path.display()
            );
            return;
        }

        let loaded = load_complete_as_a_drop_would(&path)
            .expect("MATLAB I/Q should decode through the complete load path");
        let session = loaded.iq.as_ref().expect("I/Q session is retained");
        assert_eq!(session.site_id(), "OUPRIME");
        assert_eq!(session.native_dwell_pulses(), Some(32));
        assert_eq!(session.processed().report.dwells, 150);
        assert!(
            loaded.volume.cuts[0]
                .moments
                .contains_key(&MomentType::RelativePower)
        );
        assert!(
            !loaded.volume.cuts[0]
                .moments
                .contains_key(&MomentType::Reflectivity)
        );
        assert!(session.provenance().contains("SNR unavailable"));
    }

    /// The Level II path, on the real volume the renders are pinned to.
    ///
    /// Not checked in: an operational NEXRAD volume is eleven megabytes, and
    /// this repository does not carry one. Point `NEXRAD_LEVEL2_SAMPLE` at a
    /// real Archive II file to run it.
    #[test]
    fn the_load_path_decodes_a_real_level2_volume() {
        let Ok(path) = std::env::var("NEXRAD_LEVEL2_SAMPLE") else {
            eprintln!(
                "skipping: set NEXRAD_LEVEL2_SAMPLE to a real Archive II volume to run this test"
            );
            return;
        };
        let path = PathBuf::from(path);
        if !path.exists() {
            eprintln!(
                "skipping: NEXRAD_LEVEL2_SAMPLE points at {}, which is not there",
                path.display()
            );
            return;
        }
        let volume =
            load_as_a_drop_would(&path).expect("Archive II should decode through the load path");
        assert!(
            volume.metadata.decoded_radial_count > 1_000,
            "an operational volume carries thousands of radials, got {}",
            volume.metadata.decoded_radial_count
        );
        assert!(volume.cuts.len() > 1, "an operational volume has many cuts");
        assert_eq!(
            volume.metadata.source_path.as_deref(),
            Some(path.display().to_string().as_str()),
            "a single-scan container records the file and nothing else"
        );
    }

    #[ignore = "set NEXRAD_LEGACY_SAMPLE to a pre-2008 Archive II file path to run manually"]
    #[test]
    fn the_load_path_decodes_a_real_legacy_volume() {
        let path = std::env::var("NEXRAD_LEGACY_SAMPLE").expect("NEXRAD_LEGACY_SAMPLE is not set");
        let raw = std::fs::read(path).expect("read sample");
        let volume = decode(&raw).expect("legacy volume should decode through the load path");
        assert!(volume.metadata.decoded_radial_count > 1_000);
    }
}
