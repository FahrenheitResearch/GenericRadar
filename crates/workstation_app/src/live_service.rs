use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use analyst_runtime::{
    FrameStage, Generation, LatestLaneReceiver, LatestLaneSender, latest_lane_channel,
};
use chrono::{DateTime, SecondsFormat, Utc};
use data_source::RealtimeLevel2Volume;
use eframe::egui;

const COMMAND_LANE: u8 = 0;
const RESULT_QUEUE_CAPACITY: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(1_200);
const COMMAND_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// How long the backfill will wait for room in the result queue before giving
/// up. It cannot block on `send` the way the live poll does: the live poll is
/// the thread the app is waiting on, whereas an abandoned backfill thread that
/// never returns would hold a volume's worth of memory for the rest of the
/// run.
const BACKFILL_SEND_ATTEMPTS: usize = 20;
const BACKFILL_SEND_RETRY: Duration = Duration::from_millis(100);
const BYTES_PER_MIB: f64 = 1_024.0 * 1_024.0;
/// How often one session sweeps the live cache against its byte budget.
///
/// The walk is a directory listing - milliseconds against the ~1.2 s poll -
/// but a growing cache only moves at ~0.5 GB/day, so once per few minutes
/// bounds it exactly as well as once per poll would.
const LIVE_CACHE_PRUNE_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
struct VolumeFingerprint {
    site: String,
    volume_id: u16,
    volume_time: DateTime<Utc>,
    chunk_count: usize,
    complete: bool,
    total_size: u64,
}

impl From<&RealtimeLevel2Volume> for VolumeFingerprint {
    fn from(volume: &RealtimeLevel2Volume) -> Self {
        Self {
            site: volume.site.clone(),
            volume_id: volume.volume_id,
            volume_time: volume.volume_time,
            chunk_count: volume.chunks.len(),
            complete: volume.complete,
            total_size: volume.total_size,
        }
    }
}

struct LiveSession {
    generation: Generation,
    site: String,
    cache_dir: PathBuf,
    last_fingerprint: Option<VolumeFingerprint>,
    last_error: Option<String>,
    /// Set once the previous-volume backfill has been started, and never
    /// cleared. One session gets one attempt: a site whose previous volume has
    /// already aged out of the chunks bucket must not become a background
    /// download that retries for as long as the session lasts.
    backfill_started: bool,
    /// Raised when this session ends. The backfill runs on a detached thread
    /// and this is the only thing it can see from here, so it is checked
    /// before the listing, between chunk batches, and before the result is
    /// published.
    backfill_cancel: Arc<AtomicBool>,
    /// When the live cache was last swept against its budget, so the sweep
    /// runs on [`LIVE_CACHE_PRUNE_INTERVAL`] rather than per poll.
    last_prune: Option<Instant>,
}

impl LiveSession {
    fn new(generation: Generation, site: String, cache_dir: PathBuf) -> Self {
        Self {
            generation,
            site,
            cache_dir,
            last_fingerprint: None,
            last_error: None,
            backfill_started: false,
            backfill_cancel: Arc::new(AtomicBool::new(false)),
            last_prune: None,
        }
    }

    /// Whether this poll should sweep the cache, claiming the slot if so.
    fn take_prune_slot(&mut self) -> bool {
        let due = self
            .last_prune
            .is_none_or(|at| at.elapsed() >= LIVE_CACHE_PRUNE_INTERVAL);
        if due {
            self.last_prune = Some(Instant::now());
        }
        due
    }

    /// Claim this session's single backfill attempt.
    fn take_backfill_slot(&mut self) -> bool {
        if self.backfill_started {
            return false;
        }
        self.backfill_started = true;
        true
    }
}

impl Drop for LiveSession {
    /// Cancellation rides on the session's lifetime rather than on an explicit
    /// call, because every way a session can end - stop, site switch, worker
    /// shutdown - drops it, and a backfill that outlived one of those paths
    /// would install another radar's volume.
    fn drop(&mut self) {
        self.backfill_cancel.store(true, Ordering::Relaxed);
    }
}

/// Everything the detached backfill thread needs. It deliberately owns copies:
/// the session it came from may be dropped the instant after the spawn.
struct BackfillJob {
    generation: Generation,
    site: String,
    cache_dir: PathBuf,
    after_volume_id: u16,
    after_volume_time: DateTime<Utc>,
    cancel: Arc<AtomicBool>,
}

enum LiveCommand {
    Start {
        generation: Generation,
        site: String,
        cache_dir: PathBuf,
    },
    Stop,
}

pub enum LiveUpdate {
    Started {
        generation: Generation,
        site: String,
    },
    VolumeReady {
        generation: Generation,
        site: String,
        path: PathBuf,
        stage: FrameStage,
        volume_time: DateTime<Utc>,
        chunk_count: usize,
        total_size: u64,
        cache_hit: bool,
    },
    Failed {
        generation: Generation,
        site: String,
        message: String,
    },
    Stopped,
}

pub struct LiveService {
    sender: LatestLaneSender<u8, LiveCommand>,
    receiver: Receiver<LiveUpdate>,
}

impl LiveService {
    pub fn new(context: egui::Context) -> Self {
        let (command_sender, command_receiver) = latest_lane_channel::<u8, LiveCommand>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let _worker = thread::Builder::new()
            .name("radar-workstation-live".to_owned())
            .spawn(move || run_worker(command_receiver, result_sender, context))
            .expect("failed to start live Level II worker");
        Self {
            sender: command_sender,
            receiver: result_receiver,
        }
    }

    pub fn start(
        &self,
        generation: Generation,
        site: impl Into<String>,
        cache_dir: PathBuf,
    ) -> Result<(), String> {
        let site = normalize_site(site.into())?;
        self.sender
            .submit(
                COMMAND_LANE,
                LiveCommand::Start {
                    generation,
                    site,
                    cache_dir,
                },
            )
            .map(|_| ())
            .map_err(|_| "live source worker is closed".to_owned())
    }

    pub fn stop(&self) {
        let _ = self.sender.submit(COMMAND_LANE, LiveCommand::Stop);
    }

    pub fn try_recv(&self) -> Option<LiveUpdate> {
        self.receiver.try_recv().ok()
    }
}

pub fn default_live_cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path)
            .join("FahrenheitResearch")
            .join("RadarWorkstation")
            .join("cache")
            .join("level2-live");
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path)
            .join("radar-workstation")
            .join("level2-live");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("radar-workstation")
            .join("level2-live");
    }
    std::env::temp_dir()
        .join("radar-workstation")
        .join("level2-live")
}

fn run_worker(
    commands: LatestLaneReceiver<u8, LiveCommand>,
    results: SyncSender<LiveUpdate>,
    context: egui::Context,
) {
    let mut session: Option<LiveSession> = None;
    loop {
        let command = if session.is_some() {
            commands.try_recv().map(|(_, command)| command)
        } else {
            commands.recv().map(|(_, command)| command)
        };
        if let Some(command) = command {
            apply_command(command, &mut session, &results, &context);
        } else if session.is_none() {
            break;
        }

        let Some(active) = session.as_mut() else {
            continue;
        };
        poll_session(active, &results, &context);

        let checks = (POLL_INTERVAL.as_millis() / COMMAND_CHECK_INTERVAL.as_millis()).max(1);
        for _ in 0..checks {
            thread::sleep(COMMAND_CHECK_INTERVAL);
            if let Some((_lane, command)) = commands.try_recv() {
                apply_command(command, &mut session, &results, &context);
                break;
            }
        }
    }
}

fn apply_command(
    command: LiveCommand,
    session: &mut Option<LiveSession>,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    match command {
        LiveCommand::Start {
            generation,
            site,
            cache_dir,
        } => {
            // Assigning over the old session drops it, which cancels its
            // backfill before this one can start its own.
            *session = Some(LiveSession::new(generation, site.clone(), cache_dir));
            let _ = results.try_send(LiveUpdate::Started { generation, site });
        }
        LiveCommand::Stop => {
            *session = None;
            let _ = results.try_send(LiveUpdate::Stopped);
        }
    }
    context.request_repaint();
}

fn poll_session(
    session: &mut LiveSession,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    let volume = match data_source::latest_realtime_level2_volume(&session.site) {
        Ok(volume) => volume,
        Err(error) => {
            publish_error(session, error.to_string(), results, context);
            return;
        }
    };
    let fingerprint = VolumeFingerprint::from(&volume);
    if session.last_fingerprint.as_ref() == Some(&fingerprint) {
        session.last_error = None;
        return;
    }

    let downloaded = match data_source::download_realtime_volume(&volume, &session.cache_dir) {
        Ok(downloaded) => downloaded,
        Err(error) => {
            publish_error(session, error.to_string(), results, context);
            return;
        }
    };

    let stage = if volume.complete {
        FrameStage::Complete
    } else {
        FrameStage::Partial
    };
    let update = LiveUpdate::VolumeReady {
        generation: session.generation,
        site: session.site.clone(),
        path: downloaded.path,
        stage,
        volume_time: volume.volume_time,
        chunk_count: volume.chunks.len(),
        total_size: volume.total_size,
        cache_hit: downloaded.cache_hit,
    };
    if results.send(update).is_ok() {
        session.last_fingerprint = Some(fingerprint);
        session.last_error = None;
        context.request_repaint();
        // The app now has data for this session, so the previous volume can be
        // fetched behind it. This only spawns a thread; the caller returns to
        // the chunk poll immediately, which stays the priority because it is
        // what keeps the current tilt fresh.
        let _spawned = spawn_backfill(session, &volume, results, context);
        // Keep the disk bounded while the feed runs: this cache measured
        // 1,072 MB after ~2 days unbounded, with a 17.5 GB proven endpoint
        // on this machine. The prune's own age guard protects the volume
        // still assembling and any backfill mid-download.
        if session.take_prune_slot() {
            let report = data_source::prune_live_cache(
                &session.cache_dir,
                data_source::DEFAULT_LIVE_CACHE_BUDGET_BYTES,
            );
            if report.entries_removed > 0 {
                eprintln!(
                    "{} live cache pruned: {} entries removed, {:.1} -> {:.1} MiB",
                    session.site,
                    report.entries_removed,
                    report.bytes_before as f64 / BYTES_PER_MIB,
                    report.bytes_after as f64 / BYTES_PER_MIB
                );
            }
        }
    }
}

/// Start the one background fetch of the volume before `current`.
///
/// A session that has just started holds a single tilt. The 3D box builds each
/// voxel from a vertical profile through the tilt stack, so one tilt fills
/// only the shell within half a beamwidth of that one beam, and the 2D sweep
/// animation has no previous picture of the tilt to paint the unswept wedge
/// with. Both stay broken until most of a VCP has arrived - one to five
/// minutes. The previous volume is already complete in the bucket, so fetching
/// it costs one download and closes both gaps at once.
///
/// Returns whether a thread was actually started, so "once per session" can be
/// counted in a test rather than reasoned about.
fn spawn_backfill(
    session: &mut LiveSession,
    current: &RealtimeLevel2Volume,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) -> bool {
    if !session.take_backfill_slot() {
        return false;
    }

    let job = BackfillJob {
        generation: session.generation,
        site: session.site.clone(),
        cache_dir: session.cache_dir.clone(),
        after_volume_id: current.volume_id,
        after_volume_time: current.volume_time,
        cancel: Arc::clone(&session.backfill_cancel),
    };
    let results = results.clone();
    let context = context.clone();
    let spawned = thread::Builder::new()
        .name("radar-workstation-live-backfill".to_owned())
        .spawn(move || {
            let site = job.site.clone();
            let outcome = run_backfill(job, &results, &context);
            // Every path is named in the log, so a backfill that never appears
            // can be told apart from one that was never asked for.
            eprintln!("{site} live backfill finished: {outcome:?}");
        });
    match spawned {
        Ok(_handle) => true,
        Err(error) => {
            // The slot stays claimed: if the OS refused a thread once, retrying
            // it every poll is not going to help.
            eprintln!("live backfill worker could not start: {error}");
            false
        }
    }
}

/// What one backfill attempt did. Every early return is named so that the
/// session-safety gates can be asserted on directly instead of inferred from a
/// silent thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackfillOutcome {
    /// The session had already ended when the thread started running, so no
    /// request was made at all.
    CancelledBeforeListing,
    /// The session ended after the predecessor was found, or after it was
    /// fetched, and before anything was published.
    CancelledBeforePublish,
    /// No usable predecessor: aged out of the bucket, or the site has only the
    /// one volume, or nothing recent enough was complete.
    Unavailable,
    /// The listing succeeded but the transfer did not.
    DownloadFailed,
    /// Handed to the app.
    Published,
    /// The app never made room for it before the retry budget ran out.
    Dropped,
}

fn run_backfill(
    job: BackfillJob,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) -> BackfillOutcome {
    let cancelled = || job.cancel.load(Ordering::Relaxed);
    if cancelled() {
        return BackfillOutcome::CancelledBeforeListing;
    }

    let started = Instant::now();
    let volume = match data_source::previous_complete_realtime_level2_volume(
        &job.site,
        job.after_volume_id,
        job.after_volume_time,
    ) {
        Ok(volume) => volume,
        Err(error) => {
            // Nothing the analyst can act on - most often the previous volume
            // has aged out, or this site has only just come back on the air.
            // The live poll owns the status line, so this stays in the log.
            eprintln!("{} live backfill unavailable: {error}", job.site);
            return BackfillOutcome::Unavailable;
        }
    };
    if cancelled() {
        return BackfillOutcome::CancelledBeforePublish;
    }

    // Announced before the transfer starts, because this is the largest single
    // fetch the live path makes and the analyst did not ask for it by hand.
    eprintln!(
        "{} live backfill: previous volume {} at {} · {} chunk(s) · {:.1} MiB",
        job.site,
        volume.volume_id,
        volume
            .volume_time
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        volume.chunks.len(),
        volume.total_size as f64 / BYTES_PER_MIB
    );

    let downloaded = match data_source::download_realtime_volume_cancellable(
        &volume,
        &job.cache_dir,
        &cancelled,
    ) {
        Ok(downloaded) => downloaded,
        Err(error) => {
            eprintln!("{} live backfill download failed: {error}", job.site);
            return BackfillOutcome::DownloadFailed;
        }
    };
    if cancelled() {
        return BackfillOutcome::CancelledBeforePublish;
    }
    eprintln!(
        "{} live backfill: {:.1} MiB {} in {:.1} s",
        job.site,
        volume.total_size as f64 / BYTES_PER_MIB,
        if downloaded.cache_hit {
            "already cached"
        } else {
            "downloaded"
        },
        started.elapsed().as_secs_f32()
    );

    // Delivered as an ordinary complete volume: it is one, and the app's
    // history sorts it into place by volume time. `generation` is stamped from
    // the session that asked for it, so a session that has since stopped or
    // changed site drops this even if the cancel flag is raised in the window
    // between the check above and the send below.
    send_when_room(
        results,
        LiveUpdate::VolumeReady {
            generation: job.generation,
            site: job.site.clone(),
            path: downloaded.path,
            stage: FrameStage::Complete,
            volume_time: volume.volume_time,
            chunk_count: volume.chunks.len(),
            total_size: volume.total_size,
            cache_hit: downloaded.cache_hit,
        },
        &job.cancel,
        context,
    )
}

/// Publish `update` once the result queue has room, abandoning it if the
/// session ends first.
fn send_when_room(
    results: &SyncSender<LiveUpdate>,
    update: LiveUpdate,
    cancel: &AtomicBool,
    context: &egui::Context,
) -> BackfillOutcome {
    let mut pending = update;
    for attempt in 0..BACKFILL_SEND_ATTEMPTS {
        if cancel.load(Ordering::Relaxed) {
            return BackfillOutcome::CancelledBeforePublish;
        }
        match results.try_send(pending) {
            Ok(()) => {
                context.request_repaint();
                return BackfillOutcome::Published;
            }
            Err(TrySendError::Full(returned)) => {
                pending = returned;
                if attempt + 1 < BACKFILL_SEND_ATTEMPTS {
                    thread::sleep(BACKFILL_SEND_RETRY);
                }
            }
            // The app is gone. Nothing to say and nobody to say it to.
            Err(TrySendError::Disconnected(_)) => return BackfillOutcome::Dropped,
        }
    }
    BackfillOutcome::Dropped
}

fn publish_error(
    session: &mut LiveSession,
    message: String,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    if session.last_error.as_deref() == Some(message.as_str()) {
        return;
    }
    session.last_error = Some(message.clone());
    let _ = results.try_send(LiveUpdate::Failed {
        generation: session.generation,
        site: session.site.clone(),
        message,
    });
    context.request_repaint();
}

fn normalize_site(site: String) -> Result<String, String> {
    let site = site.trim().to_ascii_uppercase();
    if site.len() != 4 || !site.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("radar site must be a four-character Level II identifier".to_owned());
    }
    Ok(site)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_ids_are_normalized_and_validated() {
        assert_eq!(normalize_site(" krtx ".to_owned()).unwrap(), "KRTX");
        assert!(normalize_site("RTX".to_owned()).is_err());
        assert!(normalize_site("KR/X".to_owned()).is_err());
    }

    /// §2.9: the sweep runs on session start and then on its interval, not
    /// per 1.2 s poll.
    #[test]
    fn the_prune_slot_opens_at_start_and_then_on_the_interval() {
        let mut session = LiveSession::new(
            Generation::new(1),
            "KTLX".to_owned(),
            PathBuf::from("cache"),
        );
        assert!(
            session.take_prune_slot(),
            "the first poll bounds the backlog"
        );
        assert!(
            !session.take_prune_slot(),
            "the next poll must not re-walk the cache"
        );

        // The interval elapses.
        session.last_prune = Some(Instant::now() - LIVE_CACHE_PRUNE_INTERVAL);
        assert!(session.take_prune_slot());
        assert!(!session.take_prune_slot());
    }

    #[test]
    fn the_backfill_slot_is_claimed_once_per_session() {
        let mut session = LiveSession::new(
            Generation::new(1),
            "KTLX".to_owned(),
            PathBuf::from("cache"),
        );

        assert!(session.take_backfill_slot());
        // Every later poll of the same session must decline: the previous
        // volume is fetched once or not at all.
        assert!(!session.take_backfill_slot());
        assert!(!session.take_backfill_slot());

        // A different session gets its own attempt.
        let mut next = LiveSession::new(
            Generation::new(2),
            "KEAX".to_owned(),
            PathBuf::from("cache"),
        );
        assert!(next.take_backfill_slot());
    }

    #[test]
    fn ending_a_session_cancels_its_backfill() {
        let (results, _drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = None;

        apply_command(
            LiveCommand::Start {
                generation: Generation::new(1),
                site: "KTLX".to_owned(),
                cache_dir: PathBuf::from("cache"),
            },
            &mut session,
            &results,
            &context,
        );
        let first = Arc::clone(&session.as_ref().expect("session started").backfill_cancel);
        assert!(!first.load(Ordering::Relaxed));

        // Site switch. KTLX's backfill must not land on a KEAX session.
        apply_command(
            LiveCommand::Start {
                generation: Generation::new(2),
                site: "KEAX".to_owned(),
                cache_dir: PathBuf::from("cache"),
            },
            &mut session,
            &results,
            &context,
        );
        assert!(first.load(Ordering::Relaxed));
        let second = Arc::clone(&session.as_ref().expect("session restarted").backfill_cancel);
        assert!(!second.load(Ordering::Relaxed));

        apply_command(LiveCommand::Stop, &mut session, &results, &context);
        assert!(second.load(Ordering::Relaxed));
    }

    /// The worst failure available here is a backfill installing one radar's
    /// volume into a session that has moved to another radar: the map
    /// re-anchors and the analyst is looking at the wrong state without being
    /// told. Three gates have to hold, and this exercises all three.
    #[test]
    fn a_backfill_in_flight_cannot_reach_a_session_that_has_changed() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = None;

        apply_command(
            LiveCommand::Start {
                generation: Generation::new(1),
                site: "KTLX".to_owned(),
                cache_dir: PathBuf::from("cache"),
            },
            &mut session,
            &results,
            &context,
        );
        let ktlx = session.as_ref().expect("KTLX session");
        let job = BackfillJob {
            generation: ktlx.generation,
            site: ktlx.site.clone(),
            cache_dir: ktlx.cache_dir.clone(),
            after_volume_id: 683,
            after_volume_time: Utc::now(),
            cancel: Arc::clone(&ktlx.backfill_cancel),
        };
        assert_eq!(drain.try_recv().ok().map(update_name), Some("Started"));

        // The analyst switches to KEAX while the KTLX backfill is in flight.
        apply_command(
            LiveCommand::Start {
                generation: Generation::new(2),
                site: "KEAX".to_owned(),
                cache_dir: PathBuf::from("cache"),
            },
            &mut session,
            &results,
            &context,
        );
        assert_eq!(drain.try_recv().ok().map(update_name), Some("Started"));

        // Gate 1: the thread notices before it asks the bucket for anything.
        // Naming the outcome is what makes this a proof rather than a silent
        // thread that may or may not have made a request.
        assert_eq!(
            run_backfill(job, &results, &context),
            BackfillOutcome::CancelledBeforeListing
        );
        assert!(
            drain.try_recv().is_err(),
            "a cancelled backfill must publish nothing"
        );

        // Gate 2: even a backfill that has already fetched its volume - the
        // window between the last cancel check and the send - is refused at the
        // publish step.
        let stale_cancel = Arc::new(AtomicBool::new(true));
        assert_eq!(
            send_when_room(
                &results,
                backfill_update(Generation::new(1), "KTLX"),
                &stale_cancel,
                &context,
            ),
            BackfillOutcome::CancelledBeforePublish
        );
        assert!(drain.try_recv().is_err());

        // Gate 3: if it somehow wins that race, what lands still carries the
        // generation of the session that asked for it. app.rs drops any
        // `VolumeReady` whose generation is not `session_clock.current()`, and
        // `GenerationClock::bump` never reuses a value, so KEAX's session
        // (generation 2) can never accept KTLX's volume (generation 1).
        let never_cancelled = Arc::new(AtomicBool::new(false));
        assert_eq!(
            send_when_room(
                &results,
                backfill_update(Generation::new(1), "KTLX"),
                &never_cancelled,
                &context,
            ),
            BackfillOutcome::Published
        );
        let published = drain.try_recv().expect("update reached the queue");
        let LiveUpdate::VolumeReady {
            generation, site, ..
        } = published
        else {
            panic!("expected a VolumeReady");
        };
        assert_eq!(site, "KTLX");
        assert_eq!(generation, Generation::new(1));
        assert_ne!(
            generation,
            session.as_ref().expect("KEAX session").generation,
            "the live session's generation must not match the stale backfill's"
        );
    }

    /// Once per session, and no retry loop when the answer is "there is no
    /// previous volume".
    #[test]
    fn a_session_starts_exactly_one_backfill_however_many_times_it_polls() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = LiveSession::new(
            Generation::new(1),
            "KTLX".to_owned(),
            PathBuf::from("cache"),
        );
        // Raised up front so the spawned thread returns at its first gate
        // instead of reaching the network: this test is about how many threads
        // are started, not about what they fetch.
        session.backfill_cancel.store(true, Ordering::Relaxed);
        let volume = test_volume(683);

        let spawns = (0..8)
            .filter(|_| spawn_backfill(&mut session, &volume, &results, &context))
            .count();
        assert_eq!(
            spawns, 1,
            "eight polls of one session must start one backfill"
        );
        assert!(drain.try_recv().is_err());

        // The slot is claimed even when the OS or the bucket refuses, so a site
        // whose predecessor has aged out never becomes a background download
        // that retries for the life of the session.
        assert!(session.backfill_started);

        // A new session - a restart, or a different site - gets its own single
        // attempt.
        let mut next = LiveSession::new(
            Generation::new(2),
            "KEAX".to_owned(),
            PathBuf::from("cache"),
        );
        next.backfill_cancel.store(true, Ordering::Relaxed);
        assert!(spawn_backfill(&mut next, &volume, &results, &context));
        assert!(!spawn_backfill(&mut next, &volume, &results, &context));
    }

    /// What the backfill is worth against the real feed, measured rather than
    /// argued: the live volume's tilt count versus the backfilled one's.
    ///
    /// Ignored because it needs the network, a site that is currently on the
    /// air, and about 16 MB of transfer. Run it with:
    ///
    /// ```text
    /// cargo test --release -p workstation_app --bin radar-workstation -- \
    ///     --ignored --nocapture the_backfilled_volume_has_the_tilts_the_live_one_lacks
    /// ```
    ///
    /// `RADAR_LIVE_SITE` picks the site (default KTLX).
    #[test]
    #[ignore = "hits the real NEXRAD chunks bucket and downloads a whole volume"]
    fn the_backfilled_volume_has_the_tilts_the_live_one_lacks() {
        let site = std::env::var("RADAR_LIVE_SITE").unwrap_or_else(|_| "KTLX".to_owned());
        let cache_dir = std::env::temp_dir().join(format!(
            "radar-workstation-backfill-check-{}",
            std::process::id()
        ));

        let live = data_source::latest_realtime_level2_volume(&site).expect("live volume");
        let live_file =
            data_source::download_realtime_volume(&live, &cache_dir).expect("live download");
        let live_volume = nexrad_io::decode_volume_from_path(&live_file.path).expect("live decode");

        let previous = data_source::previous_complete_realtime_level2_volume(
            &site,
            live.volume_id,
            live.volume_time,
        )
        .expect("previous complete volume");
        let previous_file = data_source::download_realtime_volume(&previous, &cache_dir)
            .expect("previous download");
        let previous_volume =
            nexrad_io::decode_volume_from_path(&previous_file.path).expect("previous decode");

        println!(
            "live      id {:>3} at {} · {} chunk(s) · {:.1} MiB · complete {} · {} cut(s)",
            live.volume_id,
            live.volume_time.to_rfc3339_opts(SecondsFormat::Secs, true),
            live.chunks.len(),
            live.total_size as f64 / BYTES_PER_MIB,
            live.complete,
            live_volume.cuts.len()
        );
        println!(
            "backfill  id {:>3} at {} · {} chunk(s) · {:.1} MiB · complete {} · {} cut(s) · {}",
            previous.volume_id,
            previous
                .volume_time
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            previous.chunks.len(),
            previous.total_size as f64 / BYTES_PER_MIB,
            previous.complete,
            previous_volume.cuts.len(),
            if previous_file.cache_hit {
                "cached"
            } else {
                "downloaded"
            }
        );

        assert!(previous.complete, "a backfill must be a whole volume");
        assert!(
            previous.volume_time < live.volume_time,
            "the backfill must precede the live volume"
        );
        assert!(
            live.volume_time - previous.volume_time < chrono::Duration::minutes(30),
            "the backfill must be the previous volume, not a recycled id from days ago"
        );
        assert!(previous_volume.cuts.len() > 1, "a volume has several tilts");

        // The second download is free, which is what keeps a reconnect from
        // paying for the same volume twice.
        let again = data_source::download_realtime_volume(&previous, &cache_dir)
            .expect("previous download again");
        assert!(again.cache_hit);

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    fn update_name(update: LiveUpdate) -> &'static str {
        match update {
            LiveUpdate::Started { .. } => "Started",
            LiveUpdate::VolumeReady { .. } => "VolumeReady",
            LiveUpdate::Failed { .. } => "Failed",
            LiveUpdate::Stopped => "Stopped",
        }
    }

    fn backfill_update(generation: Generation, site: &str) -> LiveUpdate {
        LiveUpdate::VolumeReady {
            generation,
            site: site.to_owned(),
            path: PathBuf::from("backfilled_V06"),
            stage: FrameStage::Complete,
            volume_time: Utc::now(),
            chunk_count: 55,
            total_size: 10_308_652,
            cache_hit: false,
        }
    }

    /// A stand-in for the volume the live poll just published. Only the id and
    /// the time are read by `spawn_backfill`.
    fn test_volume(volume_id: u16) -> RealtimeLevel2Volume {
        RealtimeLevel2Volume {
            site: "KTLX".to_owned(),
            volume_id,
            volume_time: Utc::now(),
            chunks: Vec::new(),
            complete: false,
            total_size: 0,
        }
    }

    #[test]
    fn fingerprint_changes_when_partial_volume_grows() {
        let base = VolumeFingerprint {
            site: "KRTX".to_owned(),
            volume_id: 4,
            volume_time: Utc::now(),
            chunk_count: 3,
            complete: false,
            total_size: 1_000,
        };
        assert_ne!(
            base,
            VolumeFingerprint {
                chunk_count: 4,
                total_size: 1_400,
                ..base.clone()
            }
        );
    }
}
