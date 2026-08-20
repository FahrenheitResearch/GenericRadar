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
use data_source::{ArchiveLevel2Volume, FeedFreshness, RealtimeLevel2Volume};
use eframe::egui;

use crate::net_tuning::{NetTuning, SharedNetTuning};

const COMMAND_LANE: u8 = 0;
const RESULT_QUEUE_CAPACITY: usize = 16;
/// The shipped live poll cadence, and the source of
/// [`crate::net_tuning::NetTuning`]'s `live_poll` default - the policy struct
/// READS this rather than restating it, so there is one copy of the number and
/// it lives next to the reasoning. The poll loop runs at the session's own
/// policy, which Settings > Data & network moves without restarting anything.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(1_200);
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

/// How often a session that has lost its chunk feed asks the ARCHIVE bucket
/// what it is holding.
///
/// Not the 1.2 s chunk cadence, and the gap is deliberate. The two feeds are
/// shaped differently: a chunk appears every few seconds and the poll exists to
/// catch each one as the sweep advances, whereas the archive receives one
/// finished object per volume - measured on KUEX 2026-08-19, 279 volumes at
/// intervals of min 196 s, max 418 s, mean 258 s, each uploaded a further
/// 4-9 minutes after the volume started. Polling that at 1.2 s would make ~215
/// requests per volume, all but one of which can only answer "still nothing".
///
/// Thirty seconds is the largest number that is invisible next to the latency
/// the archive already has. Worst-case detection lag is one interval, so a
/// volume that lands 250 s after its predecessor is seen 250-280 s after it,
/// a spread of 12% on a picture that is inherently ~5 minutes behind. Sixty
/// seconds would halve the request count and double that spread; thirty buys
/// the responsiveness for 20 requests per hour of stall, and a warm request is
/// an empty `start-after` listing - one S3 envelope, ~350 bytes - so an hour of
/// fallback costs about 25 KB of listings. See
/// `data_source::archive_listing_plan` for what one poll actually lists.
pub(crate) const ARCHIVE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How far ahead of the dead chunk feed the archive has to be before a session
/// abandons the chunk feed for it, minutes.
///
/// This is the guard against the case that matters most: a radar that is
/// genuinely OFF THE AIR. When the radar itself stops, both pipes stop within
/// one volume of each other, so the archive's newest and the chunk feed's
/// newest are the same scan and the archive has nothing better to offer.
/// Switching then would swap one three-day-old picture for another and add a
/// second source to explain, while the analyst's actual situation - this radar
/// is down - is unchanged.
///
/// One VCP is the right size for that margin: 4.2 minutes for VCP 12/212, 6
/// for 215, up to 10 for the clear-air VCPs. Five minutes is inside the common
/// case and comfortably outside zero. KUEX on 2026-08-19 cleared it by three
/// days.
pub(crate) const ARCHIVE_MUST_LEAD_MINUTES: i64 = 5;

/// How long the chunk listing has to keep failing before the failure counts as
/// a stall, and the archive becomes the better source.
///
/// A listing failure is not the same event as a stalled feed: a dropped
/// connection, a 503 from S3, or a laptop lid closing all produce one, and all
/// of them clear on the next poll. Only a failure that PERSISTS says anything
/// about the radar. A minute is ~50 polls at the 1.2 s cadence - far more than
/// any transient needs - and is still short enough that a session pointed at a
/// site whose chunk prefix has been deleted outright reaches the archive
/// inside the first minute rather than showing an error for ever.
pub(crate) const CHUNK_LISTING_FAILURE_STALL_AFTER: Duration = Duration::from_secs(60);

/// Which of the two buckets a session is publishing from.
///
/// Every NEXRAD site is uploaded twice by independent machinery - a growing
/// chunk set into `unidata-nexrad-level2-chunks`, one finished object per
/// volume into `unidata-nexrad-level2` - and on 2026-08-19 KUEX's chunk feed
/// had been dead for three days while its archive prefix was current to within
/// nine minutes. A session that can only read one of them shows Saturday's
/// storm under today's warning polygons.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LiveSource {
    /// The realtime chunk feed. Fresher when it works, and it almost always
    /// works, so it is where every session starts.
    #[default]
    Chunks,
    /// The Level II archive bucket, because the chunk feed stopped.
    Archive,
}

/// What one poll learned about the realtime chunk feed.
///
/// [`FeedFreshness::ArchiveFallback`] can never appear in here: this type
/// describes the chunk feed alone, and the fallback is a statement about the
/// SESSION. Mixing the two is how a status line ends up claiming a feed
/// recovered when what really happened is that the app went looking elsewhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkFeedState {
    /// The bucket answered, with the newest volume it holds and the age
    /// verdict on it.
    Listed {
        volume_time: DateTime<Utc>,
        freshness: FeedFreshness,
    },
    /// The listing has been failing for this long. Zero on the first failure.
    Unavailable { failing_for: Duration },
}

impl ChunkFeedState {
    /// The newest chunk volume time, or `None` when the listing failed.
    fn volume_time(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Listed { volume_time, .. } => Some(volume_time),
            Self::Unavailable { .. } => None,
        }
    }

    /// The newest chunk volume time, but only while the chunk feed is keeping
    /// up. `None` covers both "stalled" and "could not ask".
    fn live_volume_time(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Listed {
                volume_time,
                freshness,
            } if !freshness.is_stalled() => Some(volume_time),
            _ => None,
        }
    }

    /// Whether the chunk feed has stopped being a source worth staying on.
    ///
    /// `stall_after` is the session's own threshold - shipped
    /// [`CHUNK_LISTING_FAILURE_STALL_AFTER`], settable in Data & network - and
    /// it is passed in rather than read from a const so this stays a pure
    /// function of what one poll saw.
    fn warrants_fallback(self, stall_after: Duration) -> bool {
        match self {
            Self::Listed { freshness, .. } => freshness.is_stalled(),
            Self::Unavailable { failing_for } => failing_for >= stall_after,
        }
    }
}

/// Which bucket the session should read next, given what this poll saw.
///
/// Pure, and every flapping question is answered here rather than in the poll
/// loop, because flapping is a property of the DECISION and a decision made
/// inside an IO loop can only be tested by making requests.
///
/// THE HYSTERESIS, in one sentence: a session leaves the chunk feed only when
/// that feed is stalled AND the archive is at least
/// [`ARCHIVE_MUST_LEAD_MINUTES`] ahead of it, and returns only when the chunk
/// feed is current AND is no older than the archive. The two conditions cannot
/// both hold, so there is a dead band - a chunk feed the archive leads by
/// under a volume - in which whichever source is already in use stays in use.
///
/// The stronger property, and the one the "no flapping" requirement really
/// wants: a source change requires
/// [`data_source::classify_feed_age`]'s two-state verdict on the CHUNK feed to
/// change. Two consecutive polls that see the same feed therefore cannot
/// change source, whatever the archive is doing - and since that verdict only
/// flips when a volume arrives or when fifteen minutes of silence pass, the
/// fastest alternation this can produce is bounded by the feed's own volume
/// interval, not by the 1.2 s poll.
fn next_source_tuned(
    current: LiveSource,
    chunks: ChunkFeedState,
    archive_newest: Option<DateTime<Utc>>,
    tuning: NetTuning,
) -> LiveSource {
    match current {
        LiveSource::Chunks => {
            if !chunks.warrants_fallback(tuning.stall_after) {
                return LiveSource::Chunks;
            }
            let Some(archive) = archive_newest else {
                // Nothing known about the archive - either it has not been
                // asked yet or it answered with nothing. A stalled feed with
                // no second source is still the honest picture.
                return LiveSource::Chunks;
            };
            match chunks.volume_time() {
                Some(chunk_time)
                    if archive
                        <= chunk_time + chrono::Duration::minutes(tuning.archive_lead_minutes) =>
                {
                    LiveSource::Chunks
                }
                // Either the archive is a whole volume ahead, or the chunk
                // feed could not even be listed and anything real beats it.
                _ => LiveSource::Archive,
            }
        }
        LiveSource::Archive => {
            let Some(chunk_time) = chunks.live_volume_time() else {
                return LiveSource::Archive;
            };
            if archive_newest.is_none_or(|archive| chunk_time >= archive) {
                LiveSource::Chunks
            } else {
                LiveSource::Archive
            }
        }
    }
}

/// [`next_source_tuned`] under the shipped policy.
///
/// The hysteresis tests below are about the DECISION - which source wins, and
/// that two identical polls cannot move it twice - rather than about the
/// numbers, so they ask under the defaults and stay readable.
/// `next_source_tuned` is what the poll loop calls, and a test of its own
/// moves the lead threshold to prove the setting reaches the decision.
#[cfg(test)]
fn next_source(
    current: LiveSource,
    chunks: ChunkFeedState,
    archive_newest: Option<DateTime<Utc>>,
) -> LiveSource {
    next_source_tuned(current, chunks, archive_newest, NetTuning::default())
}

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
    /// Raised when this session ends. The backfill and the archive fetch both
    /// run on detached threads and this is the only thing either can see from
    /// here, so it is checked before any request, inside the transfer, and
    /// before the result is published.
    session_cancel: Arc<AtomicBool>,
    /// Which bucket this session is publishing from. See [`next_source`].
    source: LiveSource,
    /// Since when the chunk listing has been failing, or `None` while it
    /// works. Monotonic: a listing failure is judged by how long it has lasted
    /// and wall clock can step sideways.
    chunks_failing_since: Option<Instant>,
    /// The newest volume the ARCHIVE bucket is known to hold for this site.
    ///
    /// The whole volume and not just its time, because the fallback both
    /// decides with it (is the archive far enough ahead to be worth switching
    /// to?) and fetches it. `None` until the archive has been asked, which for
    /// a healthy site is never.
    archive_volume: Option<ArchiveLevel2Volume>,
    /// The archive volume time already handed to a fetch thread, so a poll
    /// every 1.2 s does not re-fetch the same 11 MB object.
    archive_requested: Option<DateTime<Utc>>,
    /// When the archive was last asked, so it is asked on
    /// [`ARCHIVE_POLL_INTERVAL`] and not per poll.
    last_archive_poll: Option<Instant>,
    /// How many times this session has asked the archive anything. Counted so
    /// "the archive is never touched for a healthy site" and "a ten-minute
    /// stall costs a bounded number of listings" are assertions rather than
    /// claims. One poll is one S3 listing warm, at most three cold.
    archive_polls: u64,
    /// Guards the single in-flight archive transfer. Shared with the fetch
    /// thread, which is the only thing that can release it.
    archive_fetch: Arc<ArchiveFetchSlot>,
    /// When the live cache was last swept against its budget, so the sweep
    /// runs on [`LIVE_CACHE_PRUNE_INTERVAL`] rather than per poll.
    last_prune: Option<Instant>,
    /// The network policy this session runs under, snapshotted at the top of
    /// each poll from the shared handle the settings pass writes. A snapshot
    /// rather than a live read, so one poll - its listing, its source
    /// decision, its prune - runs under one consistent set of numbers.
    tuning: NetTuning,
    /// What was last reported to the app about the feed itself: the newest
    /// volume time the bucket is holding, and whether that counts as current.
    ///
    /// Held so the report is published when it CHANGES rather than on every
    /// 1.2 s poll. It is not the same thing as `last_fingerprint`: the
    /// fingerprint is about the volume being downloaded, and a stalled feed
    /// hands back the identical fingerprint for ever while its age keeps
    /// growing, which is exactly the case that has to reach the status line.
    last_feed: Option<(DateTime<Utc>, FeedFreshness)>,
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
            session_cancel: Arc::new(AtomicBool::new(false)),
            source: LiveSource::Chunks,
            chunks_failing_since: None,
            archive_volume: None,
            archive_requested: None,
            last_archive_poll: None,
            archive_polls: 0,
            archive_fetch: Arc::new(ArchiveFetchSlot::default()),
            last_prune: None,
            tuning: NetTuning::default(),
            last_feed: None,
        }
    }

    /// Reduce what this poll saw of the chunk feed to what the source decision
    /// needs, and keep the failure clock.
    ///
    /// `listed` is `None` when the listing itself failed - which is a
    /// different fact from "the feed is old", and is treated as a stall only
    /// once it has persisted for [`CHUNK_LISTING_FAILURE_STALL_AFTER`].
    fn observe_chunk_feed(
        &mut self,
        listed: Option<&RealtimeLevel2Volume>,
        now: DateTime<Utc>,
        monotonic_now: Instant,
    ) -> ChunkFeedState {
        match listed {
            Some(volume) => {
                self.chunks_failing_since = None;
                ChunkFeedState::Listed {
                    volume_time: volume.volume_time,
                    freshness: volume.freshness_at(now),
                }
            }
            None => {
                let since = *self.chunks_failing_since.get_or_insert(monotonic_now);
                ChunkFeedState::Unavailable {
                    failing_for: monotonic_now.saturating_duration_since(since),
                }
            }
        }
    }

    /// Whether this poll may ask the archive anything, claiming the slot if
    /// so. Every archive request in the module goes through here, which is
    /// what makes the cadence a bound rather than an intention.
    fn take_archive_poll_slot(&mut self, monotonic_now: Instant) -> bool {
        let due = self.last_archive_poll.is_none_or(|at| {
            monotonic_now.saturating_duration_since(at) >= self.tuning.archive_poll
        });
        if due {
            self.last_archive_poll = Some(monotonic_now);
            self.archive_polls += 1;
        }
        due
    }

    /// The newest archive volume time known to this session.
    fn archive_newest(&self) -> Option<DateTime<Utc>> {
        self.archive_volume
            .as_ref()
            .map(|volume| volume.volume_time)
    }

    /// Whether `(volume_time, freshness)` is news the app has not been told.
    ///
    /// Asked before the send and recorded only after one, by
    /// [`Self::record_feed_report`], so a report that could not be queued is
    /// retried on the next poll instead of being lost. A stall that nobody
    /// hears is the failure this whole path exists to prevent.
    fn feed_report_is_news(&self, volume_time: DateTime<Utc>, freshness: FeedFreshness) -> bool {
        self.last_feed != Some((volume_time, freshness))
    }

    fn record_feed_report(&mut self, volume_time: DateTime<Utc>, freshness: FeedFreshness) {
        self.last_feed = Some((volume_time, freshness));
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
    /// shutdown - drops it, and a transfer that outlived one of those paths
    /// would install another radar's volume.
    fn drop(&mut self) {
        self.session_cancel.store(true, Ordering::Relaxed);
    }
}

/// The one-at-a-time permit for the archive transfer.
///
/// An archive volume is a single 6-17 MB object and a poll runs every 1.2 s,
/// so without a permit a session entering the fallback would start a new
/// download of the same object on every poll until the first one finished.
/// Shared with the fetch thread because the thread is the only thing that
/// knows when it is done.
#[derive(Debug, Default)]
struct ArchiveFetchSlot {
    busy: AtomicBool,
    /// Raised by a fetch that ended without publishing, so the poll thread
    /// tries the SAME volume again at its next slot instead of waiting for the
    /// next volume to appear - which on a 4-10 minute cadence would mean
    /// minutes of blank screen after one dropped connection.
    retry: AtomicBool,
}

impl ArchiveFetchSlot {
    /// Take the permit, or report that someone else holds it.
    fn claim(&self) -> bool {
        !self.busy.swap(true, Ordering::SeqCst)
    }

    /// Hand the permit back. `retry` asks the poll thread to re-request the
    /// volume this fetch failed to deliver.
    fn release(&self, retry: bool) {
        if retry {
            self.retry.store(true, Ordering::SeqCst);
        }
        self.busy.store(false, Ordering::SeqCst);
    }

    /// Consume a pending retry request.
    fn take_retry(&self) -> bool {
        self.retry.swap(false, Ordering::SeqCst)
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

/// Everything the detached archive fetch thread needs. Same shape and same
/// reason as [`BackfillJob`]: the session may be gone the instant after the
/// spawn.
struct ArchiveFetchJob {
    generation: Generation,
    site: String,
    cache_dir: PathBuf,
    volume: ArchiveLevel2Volume,
    cancel: Arc<AtomicBool>,
    slot: Arc<ArchiveFetchSlot>,
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
    /// What the FEED looks like, as opposed to what has been downloaded from
    /// it: the newest volume time the chunks bucket is holding for this site,
    /// and whether that is recent enough to be shown as current.
    ///
    /// Sent before the download, and re-sent whenever either field changes, so
    /// a session pointed at a dead prefix can be told apart from a session
    /// pointed at a quiet one WHILE the transfer runs rather than after a
    /// three-day-old volume has landed looking fresh. `newest_volume_time` is
    /// deliberately a time and not an age: the app recomputes the age against
    /// wall clock every frame, so the number on screen keeps counting up
    /// between polls instead of freezing at whatever it was when this was sent.
    FeedStatus {
        generation: Generation,
        site: String,
        newest_volume_time: DateTime<Utc>,
        freshness: FeedFreshness,
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
    /// The network policy the worker reads. Held here so the settings pass has
    /// somewhere to write it without restarting the session; see
    /// [`crate::net_tuning`] for why it is shared rather than passed.
    tuning: SharedNetTuning,
}

impl LiveService {
    pub fn new(context: egui::Context) -> Self {
        let (command_sender, command_receiver) = latest_lane_channel::<u8, LiveCommand>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let tuning = SharedNetTuning::default();
        let worker_tuning = tuning.clone();
        let _worker = thread::Builder::new()
            .name("radar-workstation-live".to_owned())
            .spawn(move || run_worker(command_receiver, result_sender, context, worker_tuning))
            .expect("failed to start live Level II worker");
        Self {
            sender: command_sender,
            receiver: result_receiver,
            tuning,
        }
    }

    /// The handle the settings pass writes the network policy into. Cheap to
    /// clone; every clone names the same policy.
    pub fn tuning(&self) -> SharedNetTuning {
        self.tuning.clone()
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
    tuning: SharedNetTuning,
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
        // One snapshot per turn: the poll and the sleep that follows it run
        // under the same policy, so a slider moved mid-poll takes effect on
        // the next turn instead of splitting this one in half.
        let policy = tuning.get();
        active.tuning = policy;
        poll_session(active, &results, &context);

        let checks = (policy.live_poll.as_millis() / COMMAND_CHECK_INTERVAL.as_millis()).max(1);
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
    let now = Utc::now();
    let listed = data_source::latest_realtime_level2_volume(&session.site);
    let chunks = session.observe_chunk_feed(listed.as_ref().ok(), now, Instant::now());

    // The archive is asked ONLY when the chunk feed has stopped being worth
    // staying on, and then only on its own cadence. A healthy site therefore
    // never touches the archive bucket at all - see `archive_polls`, which a
    // test asserts is zero for KOAX.
    if chunks.warrants_fallback(session.tuning.stall_after) || session.source == LiveSource::Archive
    {
        refresh_archive_knowledge(session, now);
    }
    retarget_source(session, chunks);

    match session.source {
        LiveSource::Chunks => poll_chunk_feed(session, listed, now, results, context),
        LiveSource::Archive => poll_archive(session, results, context),
    }

    // Keep the disk bounded while the feed runs: this cache measured 1,072 MB
    // after ~2 days unbounded, with a 17.5 GB proven endpoint on this machine.
    // Run for BOTH sources - the archive path writes a whole 6-17 MB volume
    // every few minutes, which is the faster of the two ways to fill a disk,
    // and a prune that only ran on the chunk path would never run at all
    // during a fallback. The prune's own age guard protects the volume still
    // assembling and any transfer mid-flight.
    prune_cache_if_due(session);
}

/// The chunk-feed half of a poll: what this module did before the archive
/// existed, unchanged except that it now runs only while the chunk feed is the
/// chosen source.
fn poll_chunk_feed(
    session: &mut LiveSession,
    listed: data_source::Result<RealtimeLevel2Volume>,
    now: DateTime<Utc>,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    let volume = match listed {
        Ok(volume) => volume,
        Err(error) => {
            publish_error(session, error.to_string(), results, context);
            return;
        }
    };
    // BEFORE the fingerprint gate and before the transfer, both deliberately.
    //
    // The bucket stopped receiving KUEX on 2026-08-16 and a live session
    // started on 2026-08-19 downloaded its last fragment - 3 chunks, 596 KB,
    // three days old - and drew it under the day's warning polygons with
    // nothing on screen but "82 chunk(s) · 14.3 MiB · downloaded". The
    // selection was correct; the silence was the bug. Reporting here means the
    // status line is honest from the first poll, and stays reported for as long
    // as the session lasts, because a stalled feed returns the same fingerprint
    // for ever and the gate below would otherwise return before ever saying so.
    publish_feed_status(
        session,
        volume.volume_time,
        volume.freshness_at(now),
        results,
        context,
    );

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
    }
}

/// Sweep the live cache against its byte budget, at most once per
/// [`LIVE_CACHE_PRUNE_INTERVAL`].
fn prune_cache_if_due(session: &mut LiveSession) {
    if !session.take_prune_slot() {
        return;
    }
    let report = data_source::prune_live_cache(&session.cache_dir, session.tuning.live_cache_bytes);
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

/// Ask the archive what it is holding, at most once per
/// [`ARCHIVE_POLL_INTERVAL`].
///
/// A failure here is logged and not published: while the chunk feed is still
/// the session's source the analyst is already being told the truth about it,
/// and while the archive IS the source the picture on screen is unchanged by
/// one listing that did not answer. Either way the next slot retries.
fn refresh_archive_knowledge(session: &mut LiveSession, now: DateTime<Utc>) {
    if !session.take_archive_poll_slot(Instant::now()) {
        return;
    }
    match data_source::archive_level2_volume_newer_than(
        &session.site,
        now,
        session.archive_newest(),
    ) {
        // `None` means "nothing newer than what I hold", which is the ordinary
        // answer between volumes and must not clear what is held.
        Ok(None) => {}
        Ok(Some(volume)) => {
            eprintln!(
                "{} archive: {} at {} · {:.1} MiB · uploaded {}",
                session.site,
                volume.key().rsplit('/').next().unwrap_or_default(),
                volume
                    .volume_time
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                volume.total_size() as f64 / BYTES_PER_MIB,
                volume
                    .uploaded_at()
                    .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
                    .unwrap_or_else(|| "unknown".to_owned()),
            );
            session.archive_volume = Some(volume);
        }
        Err(error) => eprintln!("{} archive listing failed: {error}", session.site),
    }
}

/// Apply [`next_source`] and announce a change.
///
/// The log line is the forensic record of a switch: on 2026-08-19 the only
/// evidence that KUEX's chunk feed had died was a three-day-old picture, and a
/// session that silently changed bucket would be just as hard to explain
/// afterwards.
fn retarget_source(session: &mut LiveSession, chunks: ChunkFeedState) {
    let next = next_source_tuned(
        session.source,
        chunks,
        session.archive_newest(),
        session.tuning,
    );
    if next == session.source {
        return;
    }
    match next {
        LiveSource::Archive => eprintln!(
            "{} switching to the Level II archive: chunk feed {}, archive newest {}",
            session.site,
            match chunks {
                ChunkFeedState::Listed { volume_time, .. } => format!(
                    "newest {} and stalled",
                    volume_time.to_rfc3339_opts(SecondsFormat::Secs, true)
                ),
                ChunkFeedState::Unavailable { failing_for } =>
                    format!("unlistable for {} s", failing_for.as_secs()),
            },
            session
                .archive_newest()
                .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_else(|| "unknown".to_owned()),
        ),
        LiveSource::Chunks => eprintln!(
            "{} back on the realtime chunk feed: newest {}",
            session.site,
            chunks
                .volume_time()
                .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_else(|| "unknown".to_owned()),
        ),
    }
    session.source = next;
}

/// The archive half of a poll: report the source honestly, and make sure the
/// newest archive volume is on its way.
///
/// The transfer runs on a detached thread rather than here, for the same
/// reason the backfill does: an archive volume is one 6-17 MB object, and a
/// poll thread inside a multi-second download is a poll thread that cannot
/// notice the chunk feed recovering, cannot be cancelled by a site switch, and
/// cannot answer the app.
fn poll_archive(
    session: &mut LiveSession,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    let Some(volume) = session.archive_volume.clone() else {
        // Unreachable while `next_source` only chooses `Archive` with an
        // archive volume in hand, and harmless if that ever changes.
        return;
    };

    // The SOURCE is what is published, never a repaired-looking chunk feed.
    // `newest_volume_time` is the archive's, because that is the data the app
    // is being handed; the variant is what says which bucket it came from.
    publish_feed_status(
        session,
        volume.volume_time,
        FeedFreshness::ArchiveFallback,
        results,
        context,
    );

    if session.archive_fetch.take_retry() {
        session.archive_requested = None;
    }
    if session.archive_requested == Some(volume.volume_time) {
        return;
    }
    if !session.archive_fetch.claim() {
        // A transfer is already running. The volume it is carrying is either
        // this one or the one before it, and either way starting a second
        // download of an 11 MB object helps nobody.
        return;
    }
    session.archive_requested = Some(volume.volume_time);

    let job = ArchiveFetchJob {
        generation: session.generation,
        site: session.site.clone(),
        cache_dir: session.cache_dir.clone(),
        volume,
        cancel: Arc::clone(&session.session_cancel),
        slot: Arc::clone(&session.archive_fetch),
    };
    let results = results.clone();
    let context = context.clone();
    let spawned = thread::Builder::new()
        .name("radar-workstation-live-archive".to_owned())
        .spawn(move || {
            let site = job.site.clone();
            let slot = Arc::clone(&job.slot);
            let outcome = run_archive_fetch(job, &results, &context);
            eprintln!("{site} archive fetch finished: {outcome:?}");
            slot.release(outcome.deserves_retry());
        });
    if let Err(error) = spawned {
        eprintln!("live archive worker could not start: {error}");
        session.archive_requested = None;
        session.archive_fetch.release(false);
    }
}

/// Fetch one archive volume and hand it to the app as a complete frame.
///
/// The publish is [`send_when_room`] with [`FrameStage::Complete`] - the
/// backfill's path, not a third one. An archive volume IS a complete volume:
/// nothing to assemble, no chunks to wait for, every tilt present the moment
/// it lands. So the reveal installs it exactly as it installs a backfilled
/// volume, and the sweep animation and the 3D box get a whole VCP rather than
/// a growing one.
fn run_archive_fetch(
    job: ArchiveFetchJob,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) -> FetchOutcome {
    let cancelled = || job.cancel.load(Ordering::Relaxed);
    if cancelled() {
        return FetchOutcome::CancelledBeforeRequest;
    }

    let started = Instant::now();
    let downloaded = match data_source::download_archive_volume_cancellable(
        &job.volume,
        &job.cache_dir,
        &cancelled,
    ) {
        Ok(downloaded) => downloaded,
        Err(error) => {
            // A cancellation is not a failure and must not ask for a retry:
            // the session it belonged to is gone.
            if cancelled() {
                return FetchOutcome::CancelledBeforePublish;
            }
            eprintln!("{} archive fetch failed: {error}", job.site);
            return FetchOutcome::DownloadFailed;
        }
    };
    if cancelled() {
        return FetchOutcome::CancelledBeforePublish;
    }
    eprintln!(
        "{} archive volume {} · {:.1} MiB {} in {:.1} s",
        job.site,
        job.volume.key().rsplit('/').next().unwrap_or_default(),
        job.volume.total_size() as f64 / BYTES_PER_MIB,
        if downloaded.cache_hit {
            "already cached"
        } else {
            "downloaded"
        },
        started.elapsed().as_secs_f32()
    );

    send_when_room(
        results,
        LiveUpdate::VolumeReady {
            generation: job.generation,
            site: job.site.clone(),
            path: downloaded.path,
            stage: FrameStage::Complete,
            volume_time: job.volume.volume_time,
            // An archive object has no chunks - it is the assembled file the
            // chunk feed would have produced. Zero is the honest count, and
            // the app reads the SOURCE from the `FeedStatus` variant rather
            // than from this number.
            chunk_count: 0,
            total_size: job.volume.total_size(),
            cache_hit: downloaded.cache_hit,
        },
        &job.cancel,
        context,
    )
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
        cancel: Arc::clone(&session.session_cancel),
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

/// What one background fetch did - the once-per-session backfill, or an
/// archive volume during a fallback. Every early return is named so that the
/// session-safety gates can be asserted on directly instead of inferred from a
/// silent thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FetchOutcome {
    /// The session had already ended when the thread started running, so no
    /// request was made at all.
    CancelledBeforeRequest,
    /// The session ended after the volume was found, or after it was fetched,
    /// and before anything was published.
    CancelledBeforePublish,
    /// No usable predecessor: aged out of the bucket, or the site has only the
    /// one volume, or nothing recent enough was complete. Backfill only.
    Unavailable,
    /// The volume was chosen but the transfer did not finish.
    DownloadFailed,
    /// Handed to the app.
    Published,
    /// The app never made room for it before the retry budget ran out.
    Dropped,
}

impl FetchOutcome {
    /// Whether the poll thread should offer this volume again.
    ///
    /// Only the two ways a LIVE session can be left without the picture it
    /// asked for. A cancellation means the session is gone; `Unavailable`
    /// means there was nothing to fetch; `Published` succeeded. Retrying any
    /// of those would be a request for a result nobody can use.
    fn deserves_retry(self) -> bool {
        matches!(self, Self::DownloadFailed | Self::Dropped)
    }
}

fn run_backfill(
    job: BackfillJob,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) -> FetchOutcome {
    let cancelled = || job.cancel.load(Ordering::Relaxed);
    if cancelled() {
        return FetchOutcome::CancelledBeforeRequest;
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
            return FetchOutcome::Unavailable;
        }
    };
    if cancelled() {
        return FetchOutcome::CancelledBeforePublish;
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
            return FetchOutcome::DownloadFailed;
        }
    };
    if cancelled() {
        return FetchOutcome::CancelledBeforePublish;
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
) -> FetchOutcome {
    let mut pending = update;
    for attempt in 0..BACKFILL_SEND_ATTEMPTS {
        if cancel.load(Ordering::Relaxed) {
            return FetchOutcome::CancelledBeforePublish;
        }
        match results.try_send(pending) {
            Ok(()) => {
                context.request_repaint();
                return FetchOutcome::Published;
            }
            Err(TrySendError::Full(returned)) => {
                pending = returned;
                if attempt + 1 < BACKFILL_SEND_ATTEMPTS {
                    thread::sleep(BACKFILL_SEND_RETRY);
                }
            }
            // The app is gone. Nothing to say and nobody to say it to.
            Err(TrySendError::Disconnected(_)) => return FetchOutcome::Dropped,
        }
    }
    FetchOutcome::Dropped
}

/// Tell the app what the feed looks like, if that has changed since the last
/// time it was told.
///
/// The AGE classification is [`data_source`]'s, not this module's: the age and
/// the threshold belong beside the listing that produced the volume time, so
/// there is exactly one definition of "too old" in the workspace. What this
/// module owns is the SOURCE - the caller passes
/// [`FeedFreshness::ArchiveFallback`] when the session has left the chunk
/// feed, and `newest_volume_time` is then the archive's newest, because that
/// is the data the app is being handed.
///
/// The pairing is the honesty contract: the variant can never say the chunk
/// feed recovered while the archive is what is on screen, and the time can
/// never claim a picture is fresher than it is. A radar that is truly off the
/// air reaches the analyst as `ArchiveFallback` with a three-day-old volume
/// time - which reads as "on the archive, and the archive has nothing recent
/// either" - and not as anything resembling "live".
fn publish_feed_status(
    session: &mut LiveSession,
    newest_volume_time: DateTime<Utc>,
    freshness: FeedFreshness,
    results: &SyncSender<LiveUpdate>,
    context: &egui::Context,
) {
    if !session.feed_report_is_news(newest_volume_time, freshness) {
        return;
    }
    let update = LiveUpdate::FeedStatus {
        generation: session.generation,
        site: session.site.clone(),
        newest_volume_time,
        freshness,
    };
    // `try_send`, not `send`: this must never park the poll thread behind a
    // full result queue. Recording only on success is what makes a dropped
    // report a retry on the next poll rather than a stall nobody hears about.
    if results.try_send(update).is_ok() {
        session.record_feed_report(newest_volume_time, freshness);
        context.request_repaint();
    }
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
        let first = Arc::clone(&session.as_ref().expect("session started").session_cancel);
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
        let second = Arc::clone(&session.as_ref().expect("session restarted").session_cancel);
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
            cancel: Arc::clone(&ktlx.session_cancel),
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
            FetchOutcome::CancelledBeforeRequest
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
            FetchOutcome::CancelledBeforePublish
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
            FetchOutcome::Published
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
        session.session_cancel.store(true, Ordering::Relaxed);
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
        next.session_cancel.store(true, Ordering::Relaxed);
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
            LiveUpdate::FeedStatus { .. } => "FeedStatus",
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

    // --- the feed report ----------------------------------------------------
    //
    // Both feeds below are the real ones this was diagnosed against on
    // 2026-08-19 at 16:27Z: KUEX, whose chunk prefix held ids 1..=931 with
    // nothing written since `KUEX/931/20260816-110802-003-I` (LastModified
    // 2026-08-16T11:08:09Z), and KOAX, which had just published
    // `KOAX20260819_162446_RT680_V06` into the same live cache.

    fn observed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-19T16:27:00Z")
            .expect("a fixed observation instant")
            .with_timezone(&Utc)
    }

    fn feed_volume(site: &str, volume_id: u16, rfc3339: &str) -> RealtimeLevel2Volume {
        RealtimeLevel2Volume {
            site: site.to_owned(),
            volume_id,
            volume_time: DateTime::parse_from_rfc3339(rfc3339)
                .expect("a fixed volume time")
                .with_timezone(&Utc),
            chunks: Vec::new(),
            complete: false,
            total_size: 0,
        }
    }

    /// The last thing KUEX ever published: 3 chunks, 596 KB, three days old.
    fn stalled_kuex_volume() -> RealtimeLevel2Volume {
        feed_volume("KUEX", 931, "2026-08-16T11:08:02Z")
    }

    /// KOAX on the same machine at the same moment.
    fn live_koax_volume() -> RealtimeLevel2Volume {
        feed_volume("KOAX", 680, "2026-08-19T16:24:46Z")
    }

    /// [`publish_feed_status`] exactly as the chunk-feed path calls it: the
    /// listed volume's own time, and `data_source`'s age verdict on it. The
    /// archive path calls the same function with the archive's time and
    /// [`FeedFreshness::ArchiveFallback`], which is the only difference
    /// between the two sources as far as the app is concerned.
    fn publish_chunk_feed_status(
        session: &mut LiveSession,
        volume: &RealtimeLevel2Volume,
        now: DateTime<Utc>,
        results: &SyncSender<LiveUpdate>,
        context: &egui::Context,
    ) {
        publish_feed_status(
            session,
            volume.volume_time,
            volume.freshness_at(now),
            results,
            context,
        );
    }

    fn feed_report(update: LiveUpdate) -> (String, DateTime<Utc>, FeedFreshness) {
        let LiveUpdate::FeedStatus {
            site,
            newest_volume_time,
            freshness,
            ..
        } = update
        else {
            panic!("expected a FeedStatus");
        };
        (site, newest_volume_time, freshness)
    }

    /// The field failure in one assertion: the app is told the feed is stalled
    /// from the poll that found it, with nothing downloaded yet.
    #[test]
    fn a_stalled_feed_is_reported_before_a_single_chunk_is_fetched() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = LiveSession::new(
            Generation::new(1),
            "KUEX".to_owned(),
            PathBuf::from("cache"),
        );

        publish_chunk_feed_status(
            &mut session,
            &stalled_kuex_volume(),
            observed_now(),
            &results,
            &context,
        );

        let (site, newest, freshness) = feed_report(drain.try_recv().expect("a feed report"));
        assert_eq!(site, "KUEX");
        assert_eq!(freshness, FeedFreshness::Stalled);
        // A time, not an age: the app counts it up against wall clock itself.
        assert_eq!(
            newest.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-16T11:08:02Z"
        );
    }

    /// The same code path against the healthy feed, so "Stalled" is a
    /// measurement rather than the only answer this function can give.
    #[test]
    fn a_feed_that_is_keeping_up_is_reported_current() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = LiveSession::new(
            Generation::new(1),
            "KOAX".to_owned(),
            PathBuf::from("cache"),
        );

        publish_chunk_feed_status(
            &mut session,
            &live_koax_volume(),
            observed_now(),
            &results,
            &context,
        );

        let (site, _newest, freshness) = feed_report(drain.try_recv().expect("a feed report"));
        assert_eq!(site, "KOAX");
        assert_eq!(freshness, FeedFreshness::Current);
    }

    /// A stalled feed returns the identical volume every 1.2 s for days. The
    /// report is published on change, not per poll - but a change in EITHER
    /// field is a change.
    #[test]
    fn the_feed_report_is_published_on_change_and_not_per_poll() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = LiveSession::new(
            Generation::new(1),
            "KUEX".to_owned(),
            PathBuf::from("cache"),
        );
        let stalled = stalled_kuex_volume();

        for _ in 0..8 {
            publish_chunk_feed_status(&mut session, &stalled, observed_now(), &results, &context);
        }
        assert_eq!(
            drain.try_recv().ok().map(update_name),
            Some("FeedStatus"),
            "the first poll has to say it"
        );
        assert!(
            drain.try_recv().is_err(),
            "eight polls of an unchanged feed must produce one report"
        );

        // The prefix comes back to life: same session, new volume time, and the
        // classification flips. Both halves are news.
        let recovered = feed_volume("KUEX", 932, "2026-08-19T16:26:31Z");
        publish_chunk_feed_status(&mut session, &recovered, observed_now(), &results, &context);
        let (_site, newest, freshness) = feed_report(drain.try_recv().expect("a second report"));
        assert_eq!(freshness, FeedFreshness::Current);
        assert_eq!(
            newest.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-19T16:26:31Z"
        );
    }

    /// A feed does not have to CHANGE to go stale - it only has to stop, and
    /// then wall clock does the rest. The same listing, twice, either side of
    /// the threshold: the second poll is news even though the bucket said the
    /// identical thing.
    ///
    /// This is why `publish_feed_status` runs ahead of the fingerprint gate in
    /// `poll_session`. A dead prefix hands back a byte-identical volume for
    /// ever, so that gate returns early on every poll after the first; a report
    /// placed below it could never carry the moment a healthy feed crossed into
    /// stalled. That ordering is not covered by any test - `poll_session` calls
    /// the network directly - so it is pinned here in the only way it can be:
    /// the behaviour the ordering exists to deliver.
    #[test]
    fn a_feed_that_merely_stops_is_reported_when_wall_clock_crosses_the_threshold() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = LiveSession::new(
            Generation::new(1),
            "KOAX".to_owned(),
            PathBuf::from("cache"),
        );
        // The healthy KOAX volume, and then the bucket never moves again.
        let frozen = live_koax_volume();
        let started = frozen.volume_time;

        publish_chunk_feed_status(&mut session, &frozen, started, &results, &context);
        let (_site, _newest, freshness) = feed_report(drain.try_recv().expect("the first poll"));
        assert_eq!(freshness, FeedFreshness::Current);

        // Fourteen minutes of the same answer: a clear-air VCP plus latency,
        // and nothing to say.
        publish_chunk_feed_status(
            &mut session,
            &frozen,
            started + chrono::Duration::minutes(14),
            &results,
            &context,
        );
        assert!(drain.try_recv().is_err(), "14 min is not news");

        // Past the threshold, with the listing byte-identical to the first one.
        publish_chunk_feed_status(
            &mut session,
            &frozen,
            started + chrono::Duration::seconds(data_source::REALTIME_FEED_STALL_AFTER_SECONDS),
            &results,
            &context,
        );
        let (_site, newest, freshness) =
            feed_report(drain.try_recv().expect("the crossing has to be reported"));
        assert_eq!(freshness, FeedFreshness::Stalled);
        assert_eq!(newest, started, "the volume never changed; the clock did");
    }

    /// A report that could not be queued must not be recorded as delivered.
    /// The whole point of this path is that the analyst hears about the stall;
    /// a full queue at the wrong moment must cost a poll, not the message.
    #[test]
    fn a_feed_report_that_cannot_be_queued_is_retried_on_the_next_poll() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(1);
        let context = egui::Context::default();
        let mut session = LiveSession::new(
            Generation::new(1),
            "KUEX".to_owned(),
            PathBuf::from("cache"),
        );
        let stalled = stalled_kuex_volume();

        // Fill the one slot with something else, so the report cannot land.
        results
            .try_send(LiveUpdate::Stopped)
            .expect("the queue starts empty");
        publish_chunk_feed_status(&mut session, &stalled, observed_now(), &results, &context);
        assert!(
            session.last_feed.is_none(),
            "a report that never reached the app must not be remembered as sent"
        );

        assert_eq!(drain.try_recv().ok().map(update_name), Some("Stopped"));
        publish_chunk_feed_status(&mut session, &stalled, observed_now(), &results, &context);
        let (_site, _newest, freshness) = feed_report(drain.try_recv().expect("the retry"));
        assert_eq!(freshness, FeedFreshness::Stalled);
    }

    /// PROVE ON THE REAL FEED. Lists the live chunks bucket for each site and
    /// runs the answer through the real report path, printing what the app
    /// would be told.
    ///
    /// Ignored because it needs the network. `RADAR_LIVE_SITES` overrides the
    /// list. Run it with:
    ///
    /// ```text
    /// cargo test --release -p workstation_app --bin GenericRadar -- \
    ///     --ignored --nocapture the_real_feeds_report_themselves
    /// ```
    #[test]
    #[ignore = "lists the real NEXRAD chunks bucket"]
    fn the_real_feeds_report_themselves_as_the_bucket_actually_is() {
        let sites = std::env::var("RADAR_LIVE_SITES").unwrap_or_else(|_| "KUEX,KOAX".to_owned());
        let context = egui::Context::default();

        for site in sites.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
            let mut session =
                LiveSession::new(Generation::new(1), site.to_owned(), PathBuf::from("cache"));
            let volume = data_source::latest_realtime_level2_volume(site)
                .unwrap_or_else(|error| panic!("{site} listing: {error}"));
            let now = Utc::now();

            publish_chunk_feed_status(&mut session, &volume, now, &results, &context);
            let (reported_site, newest, freshness) =
                feed_report(drain.try_recv().expect("the first poll always reports"));

            println!(
                "{reported_site}  newest id {:>3} at {}  ·  {} chunk(s), {:.1} MiB, complete {}",
                volume.volume_id,
                newest.to_rfc3339_opts(SecondsFormat::Secs, true),
                volume.chunks.len(),
                volume.total_size as f64 / BYTES_PER_MIB,
                volume.complete,
            );
            println!(
                "          judged at {} → {freshness:?}, age {} s\n",
                now.to_rfc3339_opts(SecondsFormat::Secs, true),
                volume.age_at(now).num_seconds(),
            );
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

    // --- the archive fallback -----------------------------------------------
    //
    // Every instant below is a real one, read off the two buckets on
    // 2026-08-19: KUEX's chunk prefix frozen at `KUEX/931/20260816-110802-003-I`
    // (LastModified 2026-08-16T11:08:09Z) while `2026/08/19/KUEX/` was still
    // filling - `KUEX20260819_190636_V06`, 10.7 MiB, uploaded 19:11:18Z - and
    // KOAX healthy on both at the same moment.

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("a fixed instant")
            .with_timezone(&Utc)
    }

    fn chunks_listed(rfc3339: &str, freshness: FeedFreshness) -> ChunkFeedState {
        ChunkFeedState::Listed {
            volume_time: at(rfc3339),
            freshness,
        }
    }

    /// The real archive object KUEX was offering while its chunk feed was
    /// three days dead.
    fn archive_volume(rfc3339: &str) -> ArchiveLevel2Volume {
        let volume_time = at(rfc3339);
        ArchiveLevel2Volume {
            site: "KUEX".to_owned(),
            object: data_source::S3Object {
                key: format!(
                    "{}/KUEX{}_V06",
                    volume_time.format("%Y/%m/%d/KUEX"),
                    volume_time.format("%Y%m%d_%H%M%S")
                ),
                size: 11_255_386,
                last_modified: None,
            },
            volume_time,
        }
    }

    fn archive_session(cache_dir: &str) -> LiveSession {
        let mut session = LiveSession::new(
            Generation::new(1),
            "KUEX".to_owned(),
            PathBuf::from(cache_dir),
        );
        // Raised up front so every fetch thread this test starts returns at
        // its first gate instead of reaching the network: these tests are
        // about what the poll decides, not about what a transfer moves.
        session.session_cancel.store(true, Ordering::Relaxed);
        session.source = LiveSource::Archive;
        session
    }

    /// THE FIELD CASE, reduced to the decision it turns on. KUEX at 19:09Z on
    /// 2026-08-19: the chunk feed's newest is from Saturday, the archive's is
    /// from three minutes ago, and the app was showing Saturday.
    #[test]
    fn a_dead_chunk_feed_with_a_current_archive_switches_to_the_archive() {
        assert_eq!(
            next_source(
                LiveSource::Chunks,
                chunks_listed("2026-08-16T11:08:02Z", FeedFreshness::Stalled),
                Some(at("2026-08-19T19:06:36Z")),
            ),
            LiveSource::Archive
        );
    }

    /// THE RADAR IS ACTUALLY DOWN - the case the fallback must NOT dress up.
    ///
    /// When the radar stops, both pipes stop within a volume of each other, so
    /// the archive is holding the same last scan and has nothing better to
    /// offer. Switching then would swap one three-day-old picture for another
    /// while telling the analyst something changed. The lead margin is what
    /// makes that a no-op.
    #[test]
    fn a_radar_that_is_off_the_air_is_not_relabelled_by_switching_bucket() {
        let dead = chunks_listed("2026-08-16T11:08:02Z", FeedFreshness::Stalled);
        assert_eq!(
            next_source(LiveSource::Chunks, dead, Some(at("2026-08-16T11:12:14Z"))),
            LiveSource::Chunks,
            "one more archived volume than the chunk feed got is not a live radar"
        );
        assert_eq!(
            next_source(LiveSource::Chunks, dead, Some(at("2026-08-16T11:13:02Z"))),
            LiveSource::Chunks,
            "exactly at the margin is still not a switch"
        );
        // Past the margin the archive genuinely has more of this radar, so it
        // is used - and what reaches the analyst is a three-day-old volume
        // time under an "archive fallback" label, which is the truth.
        assert_eq!(
            next_source(LiveSource::Chunks, dead, Some(at("2026-08-16T11:13:03Z"))),
            LiveSource::Archive
        );
    }

    /// No archive answer, or an archive that is no better: stay, and keep
    /// saying stalled.
    #[test]
    fn a_stalled_feed_with_nothing_better_available_stays_where_it_is() {
        let dead = chunks_listed("2026-08-16T11:08:02Z", FeedFreshness::Stalled);
        assert_eq!(
            next_source(LiveSource::Chunks, dead, None),
            LiveSource::Chunks
        );
        assert_eq!(
            next_source(LiveSource::Chunks, dead, Some(at("2026-08-16T10:00:00Z"))),
            LiveSource::Chunks,
            "an archive that is BEHIND the dead feed is not an upgrade"
        );
    }

    /// A healthy feed never reaches for the archive, whatever the archive
    /// holds. This is also why a healthy site costs zero archive requests.
    #[test]
    fn a_healthy_chunk_feed_is_never_abandoned() {
        let healthy = chunks_listed("2026-08-19T19:07:22Z", FeedFreshness::Current);
        assert_eq!(
            next_source(
                LiveSource::Chunks,
                healthy,
                Some(at("2026-08-19T19:20:00Z"))
            ),
            LiveSource::Chunks
        );
    }

    /// Back to the chunk feed the moment it is fresher than the archive - and
    /// not one poll before that.
    #[test]
    fn the_session_returns_to_the_chunk_feed_as_soon_as_it_leads_the_archive() {
        let archive = Some(at("2026-08-19T19:06:36Z"));
        assert_eq!(
            next_source(
                LiveSource::Archive,
                chunks_listed("2026-08-19T19:11:20Z", FeedFreshness::Current),
                archive,
            ),
            LiveSource::Chunks,
            "the feed is back and ahead"
        );
        assert_eq!(
            next_source(
                LiveSource::Archive,
                chunks_listed("2026-08-19T19:06:36Z", FeedFreshness::Current),
                archive,
            ),
            LiveSource::Chunks,
            "the same volume from both pipes counts as recovered"
        );
        assert_eq!(
            next_source(
                LiveSource::Archive,
                chunks_listed("2026-08-19T19:01:00Z", FeedFreshness::Current),
                archive,
            ),
            LiveSource::Archive,
            "a feed that is writing again but is still behind the archive is not \
             a reason to show the analyst an older picture"
        );
        assert_eq!(
            next_source(
                LiveSource::Archive,
                chunks_listed("2026-08-19T19:11:20Z", FeedFreshness::Stalled),
                archive,
            ),
            LiveSource::Archive,
            "a volume time alone does not make a feed live"
        );
        assert_eq!(
            next_source(
                LiveSource::Archive,
                chunks_listed("2026-08-19T19:11:20Z", FeedFreshness::Current),
                None,
            ),
            LiveSource::Chunks,
            "a live feed beats an archive that has stopped answering"
        );
    }

    /// NO FLAPPING, as a property rather than an anecdote.
    ///
    /// Whatever the two feeds look like, applying the decision twice to the
    /// same observation lands on the same source: the first application always
    /// reaches the fixed point. So two consecutive polls that see an unchanged
    /// chunk feed cannot change source, and the fastest alternation the module
    /// can produce is bounded by how often the chunk feed's own age verdict
    /// flips - a new volume, or fifteen minutes of silence - never by the
    /// 1.2 s poll.
    #[test]
    fn one_unchanged_observation_can_move_the_source_at_most_once() {
        let observations = [
            chunks_listed("2026-08-16T11:08:02Z", FeedFreshness::Stalled),
            chunks_listed("2026-08-19T19:07:22Z", FeedFreshness::Current),
            ChunkFeedState::Unavailable {
                failing_for: Duration::ZERO,
            },
            ChunkFeedState::Unavailable {
                failing_for: CHUNK_LISTING_FAILURE_STALL_AFTER,
            },
        ];
        let archives = [
            None,
            Some(at("2026-08-16T11:09:00Z")),
            Some(at("2026-08-19T19:06:36Z")),
            Some(at("2026-08-19T23:59:59Z")),
        ];
        for chunks in observations {
            for archive in archives {
                for start in [LiveSource::Chunks, LiveSource::Archive] {
                    let first = next_source(start, chunks, archive);
                    let second = next_source(first, chunks, archive);
                    assert_eq!(
                        first, second,
                        "{start:?} -> {first:?} -> {second:?} for {chunks:?} / {archive:?}"
                    );
                }
            }
        }
    }

    /// A listing that fails once is a dropped connection, not a dead radar.
    #[test]
    fn a_chunk_listing_has_to_keep_failing_before_it_counts_as_a_stall() {
        let mut session = LiveSession::new(
            Generation::new(1),
            "KUEX".to_owned(),
            PathBuf::from("cache"),
        );
        let start = Instant::now();
        let now = observed_now();

        let first = session.observe_chunk_feed(None, now, start);
        assert_eq!(
            first,
            ChunkFeedState::Unavailable {
                failing_for: Duration::ZERO
            }
        );
        assert!(
            !first.warrants_fallback(CHUNK_LISTING_FAILURE_STALL_AFTER),
            "one failure decides nothing"
        );
        assert!(
            !session
                .observe_chunk_feed(None, now, start + Duration::from_secs(59))
                .warrants_fallback(CHUNK_LISTING_FAILURE_STALL_AFTER),
            "nor does a minute of them, one second short"
        );
        assert!(
            session
                .observe_chunk_feed(None, now, start + CHUNK_LISTING_FAILURE_STALL_AFTER)
                .warrants_fallback(CHUNK_LISTING_FAILURE_STALL_AFTER),
            "a prefix that cannot be listed for a whole minute is a source worth leaving"
        );

        // One good listing resets the clock, so a flaky link never accumulates
        // its way into a fallback.
        let healthy = live_koax_volume();
        let listed =
            session.observe_chunk_feed(Some(&healthy), now, start + Duration::from_secs(61));
        assert_eq!(listed.volume_time(), Some(healthy.volume_time));
        assert!(
            !session
                .observe_chunk_feed(None, now, start + Duration::from_secs(62))
                .warrants_fallback(CHUNK_LISTING_FAILURE_STALL_AFTER)
        );
    }

    /// The wiring, end to end: the setting reaches the source DECISION.
    ///
    /// A dead chunk feed whose newest scan is 11:08:02Z, against an archive
    /// holding 11:12:14Z - four minutes and twelve seconds ahead. Under the
    /// shipped five-minute lead that is inside the dead band and the session
    /// stays put; an analyst who has told the application that three minutes
    /// is enough gets the switch. Same inputs, different policy, different
    /// answer - which is what a wired setting means.
    #[test]
    fn the_archive_lead_setting_moves_where_a_session_gives_up_on_the_chunk_feed() {
        let dead = chunks_listed("2026-08-16T11:08:02Z", FeedFreshness::Stalled);
        let archive = Some(at("2026-08-16T11:12:14Z"));
        let shipped = crate::net_tuning::NetTuning::default();
        assert_eq!(
            next_source_tuned(LiveSource::Chunks, dead, archive, shipped),
            LiveSource::Chunks,
            "4m12s is inside the shipped 5-minute dead band"
        );
        let impatient = crate::net_tuning::NetTuning {
            archive_lead_minutes: 3,
            ..shipped
        }
        .clamped();
        assert_eq!(
            next_source_tuned(LiveSource::Chunks, dead, archive, impatient),
            LiveSource::Archive,
            "the same gap clears a 3-minute lead"
        );
    }

    /// The same, for the other half of the fallback decision: how long a
    /// listing has to keep failing before it counts as a stall.
    #[test]
    fn the_stall_threshold_setting_moves_when_a_failing_listing_counts() {
        let failing = ChunkFeedState::Unavailable {
            failing_for: Duration::from_secs(30),
        };
        assert!(
            !failing.warrants_fallback(CHUNK_LISTING_FAILURE_STALL_AFTER),
            "half the shipped minute decides nothing"
        );
        assert!(
            failing.warrants_fallback(Duration::from_secs(20)),
            "an analyst who set twenty seconds gets the verdict at thirty"
        );
    }

    /// THE CADENCE, COUNTED - the whole justification for not polling the
    /// archive at the chunk rate.
    ///
    /// Ten minutes of a dead feed is 500 polls of the chunk bucket. The
    /// archive sees 20 of them: one on entry and one per 30 s. Each is a
    /// single `start-after` listing once warm, so ten minutes of fallback
    /// costs about 7 KB of listings against the ~11 MB volume it exists to
    /// deliver.
    #[test]
    fn a_ten_minute_stall_costs_a_counted_number_of_archive_polls() {
        let mut session = LiveSession::new(
            Generation::new(1),
            "KUEX".to_owned(),
            PathBuf::from("cache"),
        );
        let start = Instant::now();
        let mut asked = 0;
        for tick in 0..500_u64 {
            if session.take_archive_poll_slot(start + Duration::from_millis(tick * 1_200)) {
                asked += 1;
            }
        }
        assert_eq!(asked, 20, "one per 30 s over ten minutes, entry included");
        assert_eq!(session.archive_polls, asked);

        // And the bound is the CLOCK, not the poll count: a loop running eight
        // times as often asks the archive exactly as often.
        let mut fast = LiveSession::new(
            Generation::new(2),
            "KUEX".to_owned(),
            PathBuf::from("cache"),
        );
        let mut asked_fast = 0;
        for tick in 0..6_000_u64 {
            if fast.take_archive_poll_slot(start + Duration::from_millis(tick * 100)) {
                asked_fast += 1;
            }
        }
        assert_eq!(asked_fast, 20);
    }

    #[test]
    fn only_one_archive_transfer_runs_at_a_time_and_only_a_lost_one_is_retried() {
        let slot = ArchiveFetchSlot::default();
        assert!(slot.claim());
        assert!(
            !slot.claim(),
            "a second download of the same 11 MB object helps nobody"
        );

        slot.release(FetchOutcome::Published.deserves_retry());
        assert!(!slot.take_retry(), "a delivered volume is not re-requested");
        assert!(slot.claim(), "and the permit is back");

        slot.release(FetchOutcome::DownloadFailed.deserves_retry());
        assert!(
            slot.take_retry(),
            "a dropped transfer must be offered again at the next slot rather than \
             leaving the screen blank until the next volume appears"
        );
        assert!(!slot.take_retry(), "and only once");

        // A cancelled fetch belongs to a session that no longer exists.
        assert!(slot.claim());
        slot.release(FetchOutcome::CancelledBeforePublish.deserves_retry());
        assert!(!slot.take_retry());
    }

    /// The archive path says which bucket it is on, then fetches the volume
    /// once however many times it is polled.
    #[test]
    fn the_archive_path_publishes_its_source_and_fetches_each_volume_once() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = archive_session("cache");
        session.archive_volume = Some(archive_volume("2026-08-19T19:06:36Z"));

        poll_archive(&mut session, &results, &context);

        let (site, newest, freshness) =
            feed_report(drain.try_recv().expect("the source has to be published"));
        assert_eq!(site, "KUEX");
        assert_eq!(
            freshness,
            FeedFreshness::ArchiveFallback,
            "the app must never be told the chunk feed recovered"
        );
        assert!(
            freshness.is_stalled(),
            "the notice stays raised: the realtime feed for this radar is still dead"
        );
        assert!(freshness.is_archive_fallback());
        assert_eq!(
            newest.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-19T19:06:36Z",
            "the time published is the ARCHIVE's newest - the data actually on offer"
        );
        assert_eq!(session.archive_requested, Some(at("2026-08-19T19:06:36Z")));

        for _ in 0..7 {
            poll_archive(&mut session, &results, &context);
        }
        assert_eq!(
            session.archive_requested,
            Some(at("2026-08-19T19:06:36Z")),
            "eight polls of one archive volume are one fetch"
        );
        assert!(
            drain.try_recv().is_err(),
            "an unchanged source is not re-reported, and a cancelled fetch publishes nothing"
        );

        // The next volume lands in the bucket: new time, so a new report - and
        // a new fetch as soon as the permit is free. Polling in a loop is not
        // test decoration: while the previous transfer still holds the permit
        // the newer volume is DEFERRED rather than dropped, and this is the
        // assertion that it is picked up rather than waiting for the volume
        // after it.
        session.archive_volume = Some(archive_volume("2026-08-19T19:11:20Z"));
        poll_archive(&mut session, &results, &context);
        let (_site, newest, freshness) =
            feed_report(drain.try_recv().expect("a newer archive volume is news"));
        assert_eq!(freshness, FeedFreshness::ArchiveFallback);
        assert_eq!(
            newest.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-19T19:11:20Z"
        );
        for attempt in 0..200 {
            if session.archive_requested == Some(at("2026-08-19T19:11:20Z")) {
                break;
            }
            assert!(attempt < 199, "the deferred volume was never picked up");
            thread::sleep(Duration::from_millis(10));
            poll_archive(&mut session, &results, &context);
        }
        assert_eq!(session.archive_requested, Some(at("2026-08-19T19:11:20Z")));
    }

    /// THE HONESTY BOUNDARY. A fallback whose archive is ALSO three days old
    /// must not present that as anything but three days old.
    #[test]
    fn an_archive_that_is_itself_stale_is_published_with_its_real_age() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = archive_session("cache");
        session.archive_volume = Some(archive_volume("2026-08-16T11:13:03Z"));

        poll_archive(&mut session, &results, &context);
        let (_site, newest, freshness) = feed_report(drain.try_recv().expect("a report"));

        assert_eq!(freshness, FeedFreshness::ArchiveFallback);
        assert!(
            freshness.is_stalled(),
            "the notice cannot be lowered by a bucket change"
        );
        assert_eq!(
            newest.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-08-16T11:13:03Z",
            "the volume time is the archive's own, so the age the app draws is the real one"
        );
        // What the app computes from that pair, which is the number an analyst
        // reads: three days, and stalled on its own age as well as by source.
        let now = at("2026-08-19T19:09:00Z");
        let age = data_source::volume_age_at(newest, now);
        assert_eq!(age.num_days(), 3);
        assert!(data_source::classify_feed_age(age).is_stalled());
    }

    /// The once-per-session backfill fetches the previous CHUNK volume. On a
    /// session that has fallen back, that volume is as dead as the feed it
    /// comes from - KUEX's predecessor is also from Saturday - so the archive
    /// path must not spend the slot. It stays for the moment the chunk feed
    /// recovers, which is the only moment a predecessor is worth having.
    #[test]
    fn an_archive_sourced_session_keeps_its_backfill_for_the_recovery() {
        let (results, _drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let mut session = archive_session("cache");
        session.archive_volume = Some(archive_volume("2026-08-19T19:06:36Z"));

        for _ in 0..8 {
            poll_archive(&mut session, &results, &context);
        }
        assert!(
            !session.backfill_started,
            "no chunk volume was published, so nothing has a predecessor worth fetching"
        );
        assert!(
            session.take_backfill_slot(),
            "the slot is still there for the recovered feed"
        );
    }

    /// A fetch belonging to a session that has ended makes no request at all -
    /// the gate is before the transfer, so this returns without touching the
    /// network even though the job names a real 11 MB object.
    #[test]
    fn an_archive_fetch_for_a_dead_session_never_reaches_the_bucket() {
        let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
        let context = egui::Context::default();
        let cancel = Arc::new(AtomicBool::new(true));
        let slot = Arc::new(ArchiveFetchSlot::default());

        let outcome = run_archive_fetch(
            ArchiveFetchJob {
                generation: Generation::new(1),
                site: "KUEX".to_owned(),
                cache_dir: PathBuf::from("cache-that-is-never-created"),
                volume: archive_volume("2026-08-19T19:06:36Z"),
                cancel,
                slot,
            },
            &results,
            &context,
        );

        assert_eq!(outcome, FetchOutcome::CancelledBeforeRequest);
        assert!(!outcome.deserves_retry());
        assert!(drain.try_recv().is_err());
    }

    /// PROVE IT ON THE REAL FEEDS. Drives a real live session per site and
    /// prints exactly what the app would be told, then asserts the INVARIANT
    /// rather than today's outage: a stalled chunk feed with a leading archive
    /// must end up on the archive with a volume from today, and a healthy
    /// chunk feed must not cost a single archive request.
    ///
    /// Ignored because it needs the network, and it downloads whole volumes.
    /// `RADAR_LIVE_SITES` overrides the list. Run it with:
    ///
    /// ```text
    /// cargo test --release -p workstation_app --bin GenericRadar -- \
    ///     --ignored --nocapture a_real_session_falls_back
    /// ```
    #[test]
    #[ignore = "drives a real live session against both NEXRAD buckets"]
    fn a_real_session_falls_back_to_the_archive_only_when_the_chunk_feed_is_dead() {
        let sites = std::env::var("RADAR_LIVE_SITES").unwrap_or_else(|_| "KUEX,KOAX".to_owned());
        let context = egui::Context::default();

        for site in sites.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (results, drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
            let cache_dir = std::env::temp_dir().join(format!(
                "radar-workstation-fallback-{}-{site}",
                std::process::id()
            ));
            let mut session =
                LiveSession::new(Generation::new(1), site.to_owned(), cache_dir.clone());

            // What the chunk feed says before anything is decided, so the
            // drive below can be read against it.
            let listed = data_source::latest_realtime_level2_volume(site);
            let now = Utc::now();
            match &listed {
                Ok(volume) => println!(
                    "{site} chunk feed: newest id {} at {} · {} s old · {:?}",
                    volume.volume_id,
                    volume
                        .volume_time
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                    volume.age_at(now).num_seconds(),
                    volume.freshness_at(now),
                ),
                Err(error) => println!("{site} chunk feed: unlistable - {error}"),
            }

            let started = Instant::now();
            let mut first_volume_at = None;
            let mut reports: Vec<(DateTime<Utc>, FeedFreshness)> = Vec::new();
            let mut volumes: Vec<(DateTime<Utc>, FrameStage, usize, u64, bool)> = Vec::new();
            for _ in 0..60 {
                poll_session(&mut session, &results, &context);
                while let Ok(update) = drain.try_recv() {
                    match update {
                        LiveUpdate::FeedStatus {
                            newest_volume_time,
                            freshness,
                            ..
                        } => reports.push((newest_volume_time, freshness)),
                        LiveUpdate::VolumeReady {
                            volume_time,
                            stage,
                            chunk_count,
                            total_size,
                            cache_hit,
                            ..
                        } => {
                            first_volume_at.get_or_insert(started.elapsed());
                            volumes.push((volume_time, stage, chunk_count, total_size, cache_hit));
                        }
                        LiveUpdate::Failed { message, .. } => println!("{site} failed: {message}"),
                        LiveUpdate::Started { .. } | LiveUpdate::Stopped => {}
                    }
                }
                if !volumes.is_empty() {
                    break;
                }
                thread::sleep(Duration::from_millis(400));
            }

            let now = Utc::now();
            println!("{site} source: {:?}", session.source);
            println!("{site} archive polls: {}", session.archive_polls);
            for (newest, freshness) in &reports {
                // The status line an analyst reads, built from exactly what
                // the app receives.
                println!(
                    "{site} status: \"{} {} · newest data {} s old\"",
                    site,
                    freshness.status_label(),
                    data_source::volume_age_at(*newest, now).num_seconds(),
                );
            }
            for (volume_time, stage, chunk_count, total_size, cache_hit) in &volumes {
                println!(
                    "{site} frame: {} · {stage:?} · {chunk_count} chunk(s) · {:.1} MiB · {} · {} s old",
                    volume_time.to_rfc3339_opts(SecondsFormat::Secs, true),
                    *total_size as f64 / BYTES_PER_MIB,
                    if *cache_hit { "cached" } else { "downloaded" },
                    data_source::volume_age_at(*volume_time, now).num_seconds(),
                );
            }
            println!(
                "{site} first frame after {:.1} s",
                first_volume_at.unwrap_or_default().as_secs_f32()
            );

            let chunk_freshness = listed.as_ref().ok().map(|volume| volume.freshness_at(now));
            match chunk_freshness {
                Some(FeedFreshness::Current) => {
                    assert_eq!(
                        session.source,
                        LiveSource::Chunks,
                        "{site}: a healthy chunk feed must not be abandoned"
                    );
                    assert_eq!(
                        session.archive_polls, 0,
                        "{site}: a healthy chunk feed must not cost one archive request"
                    );
                    assert!(
                        reports
                            .iter()
                            .all(|(_, freshness)| *freshness == FeedFreshness::Current),
                        "{site}: nothing but 'live' should have been published"
                    );
                }
                Some(FeedFreshness::Stalled) | None => {
                    assert_eq!(
                        session.source,
                        LiveSource::Archive,
                        "{site}: the chunk feed is dead and the archive is current - \
                         the app must be reading the archive"
                    );
                    let (volume_time, stage, chunk_count, ..) = *volumes
                        .first()
                        .expect("an archive volume must have arrived");
                    assert_eq!(stage, FrameStage::Complete, "an archive volume is whole");
                    assert_eq!(chunk_count, 0, "an archive volume has no chunks");
                    assert!(
                        data_source::volume_age_at(volume_time, now) < chrono::Duration::hours(1),
                        "{site}: the fallback must deliver TODAY's volume, not the newest \
                         thing in the archive's history"
                    );
                    assert!(
                        reports
                            .iter()
                            .any(|(_, freshness)| freshness.is_archive_fallback()),
                        "{site}: the app must be told it is on the archive"
                    );
                    assert!(
                        reports.iter().all(|(_, freshness)| freshness.is_stalled()),
                        "{site}: nothing published may imply the chunk feed recovered"
                    );
                }
                Some(FeedFreshness::ArchiveFallback) => {
                    unreachable!("classify_feed_age never returns a source")
                }
            }
            println!();

            // The session has to outlive its fetch threads, and dropping it
            // here is what cancels them.
            drop(session);
            let _ = std::fs::remove_dir_all(&cache_dir);
        }
    }

    /// THE OTHER DIRECTION, ON A REAL FEED. A session that is already on the
    /// archive must return to the chunk feed the moment that feed is both
    /// current and no older than the archive - which is the ordinary state of
    /// every healthy radar, because the archive is always four to nine minutes
    /// behind.
    ///
    /// KUEX's chunk feed came back at 2026-08-19T19:47Z while this was being
    /// written, three days after it stopped, and this is the path that had to
    /// carry it back.
    ///
    /// Ignored because it needs the network. Run it with:
    ///
    /// ```text
    /// cargo test --release -p workstation_app --bin GenericRadar -- \
    ///     --ignored --nocapture a_real_session_returns_to_a_recovered
    /// ```
    #[test]
    #[ignore = "drives a real live session against both NEXRAD buckets"]
    fn a_real_session_returns_to_a_recovered_chunk_feed() {
        let sites = std::env::var("RADAR_LIVE_SITES").unwrap_or_else(|_| "KOAX".to_owned());
        let context = egui::Context::default();

        for site in sites.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let now = Utc::now();
            let chunks = data_source::latest_realtime_level2_volume(site)
                .unwrap_or_else(|error| panic!("{site} chunk listing: {error}"));
            if chunks.freshness_at(now).is_stalled() {
                println!("{site}: chunk feed still stalled, nothing to recover to");
                continue;
            }

            let archive = data_source::latest_archive_level2_volume(site)
                .unwrap_or_else(|error| panic!("{site} archive: {error}"));
            let (results, _drain) = mpsc::sync_channel::<LiveUpdate>(RESULT_QUEUE_CAPACITY);
            let cache_dir = std::env::temp_dir().join(format!(
                "radar-workstation-recovery-{}-{site}",
                std::process::id()
            ));
            let mut session =
                LiveSession::new(Generation::new(1), site.to_owned(), cache_dir.clone());

            // The state a session is in after an outage: reading the archive,
            // holding the archive's newest volume.
            session.source = LiveSource::Archive;
            session.archive_volume = Some(archive.clone());
            println!(
                "{site} starting on the archive: {} at {} · chunk feed newest {} · {:?}",
                archive.key().rsplit('/').next().unwrap_or_default(),
                archive
                    .volume_time
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                chunks
                    .volume_time
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                chunks.freshness_at(now),
            );

            poll_session(&mut session, &results, &context);

            println!("{site} source after one poll: {:?}", session.source);
            assert_eq!(
                session.source,
                LiveSource::Chunks,
                "{site}: a live chunk feed that is ahead of the archive must win it back \
                 on the first poll that sees it"
            );
            assert_eq!(
                session.archive_polls, 1,
                "{site}: the archive is asked once on the way out and then not again"
            );

            // ... and it stays won: the archive is not re-asked while the feed
            // is healthy, however many polls run.
            for _ in 0..5 {
                poll_session(&mut session, &results, &context);
            }
            assert_eq!(session.source, LiveSource::Chunks);
            assert_eq!(
                session.archive_polls, 1,
                "{site}: a recovered session must stop paying for archive listings"
            );

            drop(session);
            let _ = std::fs::remove_dir_all(&cache_dir);
            println!();
        }
    }
}
