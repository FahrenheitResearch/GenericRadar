//! Fetching, caching and decoding tiles, off the UI thread.
//!
//! [`TileStore`] itself is single-threaded and lives on whichever thread drives
//! the scene: `request`, `retain` and `drain_ready` all take `&mut self`. The
//! work happens on a small pool of worker threads that share a bounded,
//! newest-first queue.
//!
//! # The three failure modes this is shaped around
//!
//! **A fast pan must not become a denial of service.** Dragging across the
//! country at z12 sweeps thousands of tiles. Without [`TileStore::retain`],
//! every one of them stays queued and is fetched long after the user stopped
//! looking at it — which for OpenStreetMap is precisely the bulk downloading
//! its policy blocks accounts for. `retain` is called every frame with the
//! union of all panes' wanted sets and drops queued jobs *before* they reach
//! the network.
//!
//! **A firewalled machine must not generate a heartbeat of doomed
//! connections.** A failed tile backs off 5 s, 15 s, 60 s and then stops for
//! the session. It does not retry on a flat interval forever.
//!
//! **Offline must be silent, not broken.** With
//! [`TileCacheConfig::offline`] set, the store serves whatever the disk cache
//! holds, never opens a socket — the HTTP client is not even constructed — and
//! never records a failure. A pane with no tiles is simply today's pane.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use crate::cache::{CachedTile, TileDiskCache, now_unix};
use crate::decode::{DecodedTile, decode_tile};
use crate::{MAX_TILE_ENCODED_BYTES, TileId, TileProvider};

/// Longest a connection attempt may take before the worker gives up. A
/// black-holed route must not pin a worker for the whole request timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest a whole request may take. Measured tiles arrive in 22-111 ms.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Queue depth. Newest-first, dropping from the back, so a drag leaves at most
/// this many stale jobs behind even between `retain` calls.
const MAX_QUEUE_DEPTH: usize = 128;

/// Backoff schedule for a transiently failed tile, then silence for the rest
/// of the session.
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];

type Key = (TileProvider, TileId);

/// How the store is configured.
#[derive(Clone, Debug)]
pub struct TileCacheConfig {
    /// `None` disables the disk cache and makes the store memory-only.
    ///
    /// A provider whose terms require a minimum cache lifetime cannot be
    /// served from a configuration with no disk cache; see
    /// [`TileStore::permits`].
    pub disk_root: Option<PathBuf>,
    pub max_disk_bytes: u64,
    pub max_workers: usize,
    /// Must name this application and carry a contact URL. The OpenStreetMap
    /// Foundation tile usage policy blocks traffic that uses a library's
    /// default User-Agent, because it cannot identify or contact the
    /// application behind it.
    pub user_agent: String,
    pub offline: bool,
}

impl Default for TileCacheConfig {
    fn default() -> Self {
        Self {
            disk_root: default_cache_dir(),
            max_disk_bytes: 512 * 1024 * 1024,
            // Six, measured rather than guessed. Sixteen cold z12 tiles
            // against the live USGS service (see live_providers.rs,
            // `the_worker_pool_is_sized_against_a_measured_cold_pane`, three
            // runs on 2026-08-19): one worker 452-776 ms, four 195-247 ms,
            // six 184-357 ms, eight 184-186 ms. Sequential is ~3x slower than
            // any pool; past four the sixteen-tile gain is marginal, but a
            // full 1500x950-point pane at 2x scale wants 110-196 tiles, where
            // the extra workers amortise their handshakes. Six is also the
            // per-host connection count every mainstream browser uses, so it
            // asks nothing of a provider that the web does not already ask.
            max_workers: 6,
            user_agent: default_user_agent(),
            offline: false,
        }
    }
}

/// `GenericRadar/<version> (+<repository>)`.
///
/// Not decoration. The OpenStreetMap Foundation tile usage policy blocks
/// traffic carrying a library's default User-Agent because it cannot identify
/// or contact the application behind it, so this string is a condition of use
/// for that provider. The fallback URL exists because `CARGO_PKG_REPOSITORY`
/// is empty unless the manifest inherits it, and shipping
/// `GenericRadar/0.1.0 (+)` would defeat the point.
#[must_use]
pub fn default_user_agent() -> String {
    let repository = match env!("CARGO_PKG_REPOSITORY") {
        "" => "https://github.com/FahrenheitResearch/GenericRadar",
        url => url,
    };
    format!(
        "GenericRadar/{} (+{repository})",
        env!("CARGO_PKG_VERSION")
    )
}

/// The platform cache directory, with this application's subdirectory
/// appended. `None` when the environment does not say where that is, in which
/// case the caller should pick somewhere explicit rather than guess.
#[must_use]
pub fn default_cache_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library").join("Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    }?;
    Some(base.join("radar-workstation").join("basemap-tiles"))
}

/// What the store knows about one tile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileState {
    /// Never asked for, or asked for and then cancelled before it reached the
    /// network.
    Unknown,
    /// Queued or in flight.
    ///
    /// **Second meaning, and it is load-bearing:** in offline mode a tile that
    /// is not in the disk cache also reports `Pending`. It is parked, not
    /// retried — no socket is opened and no work is queued for it — until
    /// [`TileStore::set_offline`] is called with `false`, which releases every
    /// parked tile back to `Unknown`. Without this the store would re-queue
    /// the same doomed tile on every frame forever.
    Pending,
    /// Decoded and handed to the caller at some point. Note that the caller
    /// owns the pixels after [`TileStore::drain_ready`]; if it evicts them it
    /// must call [`TileStore::forget`] to get them again.
    Ready,
    /// The provider answered 404: this tile does not exist there. Permanent
    /// for the session, never retried, and deliberately distinct from
    /// [`TileState::Failed`] — see [`TileProvider`] for why 404 is common and
    /// not monotonic in zoom. The caller falls back to an ancestor texture.
    Absent,
    /// A transient failure: timeout, 5xx, connection refused, or a body that
    /// did not decode. Retried after backoff, three times, then dropped for
    /// the session.
    Failed,
}

/// Counters for diagnostics and for tests that need to assert the network was
/// or was not touched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileStoreMetrics {
    pub requested: u64,
    pub served_from_disk: u64,
    pub revalidated_304: u64,
    pub downloaded: u64,
    pub absent: u64,
    pub failed: u64,
    pub in_flight: usize,
    pub queued: usize,
    pub disk_bytes: u64,
    pub bytes_downloaded: u64,
}

pub struct TileStore {
    config: TileCacheConfig,
    shared: Arc<Shared>,
    results: mpsc::Receiver<WorkerResult>,
    result_sender: mpsc::Sender<WorkerResult>,
    states: HashMap<Key, TileState>,
    failures: HashMap<Key, Failure>,
    /// Tiles requested while offline with nothing cached. Parked rather than
    /// retried; released when the store goes back online.
    parked: HashSet<Key>,
    workers_spawned: usize,
}

#[derive(Clone, Copy, Debug)]
struct Failure {
    attempts: u8,
    retry_after: Instant,
}

/// Whether a failed tile may be tried again.
///
/// Three widening attempts and then silence for the session. A flat retry
/// interval — the shape the sibling application uses — turns a firewalled
/// machine into a permanent source of doomed connections, which is rude to the
/// provider and useless to the user.
fn retry_is_due(failure: Option<&Failure>, now: Instant) -> bool {
    match failure {
        // Marked failed with no record of why: allow one attempt rather than
        // stranding the tile.
        None => true,
        Some(failure) => failure.attempts < RETRY_BACKOFF.len() as u8 && now >= failure.retry_after,
    }
}

impl TileStore {
    /// `wake` is called when a decode lands, so the host can schedule a frame.
    /// It runs on a worker thread and must not block.
    #[must_use]
    pub fn new(config: TileCacheConfig, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let cache = config
            .disk_root
            .clone()
            .map(|root| TileDiskCache::new(root, config.max_disk_bytes));
        let (result_sender, results) = mpsc::channel();
        let shared = Arc::new(Shared {
            queue: Mutex::new(JobQueue::default()),
            wake_worker: Condvar::new(),
            cache,
            offline: AtomicBool::new(config.offline),
            shutdown: AtomicBool::new(false),
            notify: wake,
            user_agent: config.user_agent.clone(),
            client: OnceLock::new(),
            counters: Counters::default(),
        });
        Self {
            config,
            shared,
            results,
            result_sender,
            states: HashMap::new(),
            failures: HashMap::new(),
            parked: HashSet::new(),
            workers_spawned: 0,
        }
    }

    /// Whether this store's configuration satisfies `provider`'s terms.
    ///
    /// `false` only for a provider that requires a minimum cache lifetime when
    /// the store has no *working* disk cache to enforce it with.
    /// [`Self::request`] returns [`TileState::Absent`] for such a provider —
    /// nothing will ever arrive for it — and a picker should hide it rather
    /// than offer a provider that cannot be used lawfully.
    ///
    /// "Working" means measured, not configured. A cache directory that cannot
    /// be created or written — a full disk, a redirected `LOCALAPPDATA`, a
    /// revoked permission — reads as *no* cache here, because every tile
    /// written to it is silently lost and the next session downloads the same
    /// tiles again. That is precisely the behaviour a minimum cache lifetime
    /// exists to prevent, and a store that merely *held a path* would go on
    /// fetching from a provider whose terms it can no longer meet.
    #[must_use]
    pub fn permits(&self, provider: TileProvider) -> bool {
        if provider.prefetch_permitted() {
            return true;
        }
        self.shared
            .cache
            .as_ref()
            .is_some_and(TileDiskCache::is_writable)
    }

    /// Ask for a tile. Idempotent, and a hash lookup once the state is known.
    ///
    /// Newest request first, so a pan does not queue behind a stale view.
    pub fn request(&mut self, provider: TileProvider, tile: TileId) -> TileState {
        let key = (provider, tile);
        if tile.z > provider.max_zoom() || !self.permits(provider) {
            self.states.insert(key, TileState::Absent);
            return TileState::Absent;
        }
        match self.states.get(&key).copied() {
            Some(TileState::Ready) => TileState::Ready,
            Some(TileState::Absent) => TileState::Absent,
            Some(TileState::Pending) => TileState::Pending,
            Some(TileState::Failed) => {
                if retry_is_due(self.failures.get(&key), Instant::now()) {
                    self.enqueue(key)
                } else {
                    TileState::Failed
                }
            }
            Some(TileState::Unknown) | None => self.enqueue(key),
        }
    }

    #[must_use]
    pub fn state(&self, provider: TileProvider, tile: TileId) -> TileState {
        self.states
            .get(&(provider, tile))
            .copied()
            .unwrap_or(TileState::Unknown)
    }

    /// Take decoded tiles, at most `max_tiles`. The caller owns the pixels and
    /// should drop them once they are on the GPU.
    pub fn drain_ready(&mut self, max_tiles: usize) -> Vec<Arc<DecodedTile>> {
        let mut ready = Vec::new();
        if max_tiles == 0 {
            return ready;
        }
        loop {
            let Ok(result) = self.results.try_recv() else {
                return ready;
            };
            let key = result.key;
            match result.outcome {
                Outcome::Ready(decoded) => {
                    self.states.insert(key, TileState::Ready);
                    self.failures.remove(&key);
                    self.revive_after_success(key.0);
                    ready.push(decoded);
                }
                Outcome::Absent => {
                    self.states.insert(key, TileState::Absent);
                    self.failures.remove(&key);
                }
                Outcome::Failed => {
                    let failure = self.failures.entry(key).or_insert(Failure {
                        attempts: 0,
                        retry_after: Instant::now(),
                    });
                    let index = usize::from(failure.attempts).min(RETRY_BACKOFF.len() - 1);
                    failure.attempts = failure.attempts.saturating_add(1);
                    failure.retry_after = Instant::now() + RETRY_BACKOFF[index];
                    self.states.insert(key, TileState::Failed);
                }
                Outcome::Parked => {
                    // Offline with nothing cached. Stay Pending and stop
                    // asking, rather than spinning the queue every frame.
                    self.parked.insert(key);
                    self.states.insert(key, TileState::Pending);
                }
            }
            if ready.len() >= max_tiles {
                // More results are waiting; ask the host for another frame so
                // they are not stranded until the next unrelated repaint.
                (self.shared.notify)();
                return ready;
            }
        }
    }

    /// Cancellation. Everything queued and not in `wanted` is dropped before it
    /// reaches the network.
    ///
    /// Call this once per frame with the union of every pane's tile set. Jobs
    /// a worker has *already* started cannot be recalled — at most
    /// `max_workers` tiles overshoot, and they land in the disk cache rather
    /// than being wasted.
    ///
    /// This is also the frame boundary the queue's ordering is built on: the
    /// requests that follow this call are one batch, fetched centre-out in
    /// the order the caller made them, ahead of any earlier batch that
    /// survives cancellation. See [`JobQueue::push`].
    pub fn retain(&mut self, wanted: &HashSet<Key>) {
        let cancelled = {
            let mut queue = self.shared.queue.lock().expect("tile queue");
            queue.begin_batch();
            queue.retain(wanted)
        };
        for key in cancelled {
            // Back to Unknown, so the tile is requested again if it returns to
            // view. Ready/Absent/Failed are never touched here.
            //
            // *Removed* rather than stored as `Unknown`, which reads the same
            // through `state()` but does not leave an entry behind. `retain`
            // runs every frame and a long pan cancels tens of thousands of
            // tiles, so storing the nothing-known state is a slow leak in the
            // one map that is never otherwise pruned.
            if self.states.get(&key) == Some(&TileState::Pending) {
                self.states.remove(&key);
            }
        }
    }

    /// A tile arriving releases the tiles that gave up on the same provider.
    ///
    /// Three attempts and silence for the session is the right answer to a
    /// firewall, and the wrong one to a laptop whose wifi dropped for ninety
    /// seconds: every tile on screen spends its schedule during the outage and
    /// the pane keeps those holes until the application is restarted, on a
    /// network that is now working. Nothing in this store polls, so no timer
    /// could notice the recovery — but a tile that *arrives* is proof, and it
    /// costs no extra request. One does arrive as soon as the user pans, or as
    /// soon as anything queued behind the outage completes.
    ///
    /// Scoped to the provider that answered: a USGS tile arriving says nothing
    /// about whether OpenStreetMap is reachable. Bounded work — it walks the
    /// failure record, which is empty in the ordinary case and holds one entry
    /// per failed tile in the bad one, not the whole tile state.
    fn revive_after_success(&mut self, provider: TileProvider) {
        if self.failures.is_empty() {
            return;
        }
        let revived: Vec<Key> = self
            .failures
            .keys()
            .copied()
            .filter(|(failed_provider, _)| *failed_provider == provider)
            .collect();
        for key in revived {
            self.failures.remove(&key);
            if self.states.get(&key) == Some(&TileState::Failed) {
                self.states.remove(&key);
            }
        }
    }

    /// Forget what is known about one tile, so the next [`Self::request`]
    /// fetches it again.
    ///
    /// The GPU layer needs this: it evicts textures under a budget, and a tile
    /// whose texture was evicted is `Ready` here but has no pixels anywhere.
    /// Refetching normally costs one disk read, because the encoded body is
    /// still cached.
    pub fn forget(&mut self, provider: TileProvider, tile: TileId) {
        let key = (provider, tile);
        self.states.remove(&key);
        self.failures.remove(&key);
        self.parked.remove(&key);
    }

    /// Offline: serve the disk cache, never open a socket, never mark a tile
    /// failed. Going back online releases every parked tile and clears the
    /// failure backoff, because regaining a network is exactly the moment a
    /// retry is worth making.
    pub fn set_offline(&mut self, offline: bool) {
        let was_offline = self.shared.offline.swap(offline, Ordering::Relaxed);
        self.config.offline = offline;
        if was_offline && !offline {
            for key in self.parked.drain() {
                self.states.remove(&key);
            }
            self.failures.clear();
            self.states.retain(|_, state| *state != TileState::Failed);
        }
    }

    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.shared.offline.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn metrics(&self) -> TileStoreMetrics {
        let queued = self
            .shared
            .queue
            .lock()
            .map(|queue| queue.jobs.len())
            .unwrap_or(0);
        let counters = &self.shared.counters;
        TileStoreMetrics {
            requested: counters.requested.load(Ordering::Relaxed),
            served_from_disk: counters.served_from_disk.load(Ordering::Relaxed),
            revalidated_304: counters.revalidated_304.load(Ordering::Relaxed),
            downloaded: counters.downloaded.load(Ordering::Relaxed),
            absent: counters.absent.load(Ordering::Relaxed),
            failed: counters.failed.load(Ordering::Relaxed),
            in_flight: counters.in_flight.load(Ordering::Relaxed),
            queued,
            disk_bytes: self
                .shared
                .cache
                .as_ref()
                .map_or(0, TileDiskCache::disk_bytes),
            bytes_downloaded: counters.bytes_downloaded.load(Ordering::Relaxed),
        }
    }

    /// Drop everything: the in-memory state, the pending queue, and the disk
    /// cache this store owns.
    pub fn clear(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.clear();
        }
        self.states.clear();
        self.failures.clear();
        self.parked.clear();
        if let Some(cache) = self.shared.cache.as_ref() {
            let _ = cache.clear();
        }
    }

    /// The disk cache root in use, for a "show me the cache" affordance.
    #[must_use]
    pub fn cache_root(&self) -> Option<&std::path::Path> {
        self.shared.cache.as_ref().map(TileDiskCache::root)
    }

    fn enqueue(&mut self, key: Key) -> TileState {
        if self.parked.contains(&key) {
            return TileState::Pending;
        }
        if self.shared.is_offline() && self.shared.cache.is_none() {
            // Offline with nowhere to read from: there is no work a worker
            // could do, so none is queued and no thread is ever started. Park
            // the tile so the next frame does not ask again.
            self.parked.insert(key);
            self.states.insert(key, TileState::Pending);
            return TileState::Pending;
        }
        {
            let mut queue = self.shared.queue.lock().expect("tile queue");
            for dropped in queue.push(key) {
                // Same reasoning as `retain`: forgetting a tile is `remove`,
                // not an `Unknown` entry that outlives the pan that made it.
                self.states.remove(&dropped);
            }
        }
        self.states.insert(key, TileState::Pending);
        // The failure record is deliberately NOT cleared here. This is the
        // path a retry takes, and clearing it would restart the backoff at
        // zero on every attempt — the tile would be retried every five seconds
        // for the life of the session. It is cleared where a retry has served
        // its purpose: on a successful decode or a 404 in `drain_ready`, on
        // `forget`, and when the store comes back online.
        self.shared
            .counters
            .requested
            .fetch_add(1, Ordering::Relaxed);
        self.spawn_workers_if_needed();
        self.shared.wake_worker.notify_one();
        TileState::Pending
    }

    /// Workers are spawned lazily and only as deep as the queue: one waiting
    /// tile spawns one thread, not a pool. An offline store with no disk cache
    /// never queues anything, so it never spawns a thread at all.
    fn spawn_workers_if_needed(&mut self) {
        let depth = self
            .shared
            .queue
            .lock()
            .map(|queue| queue.jobs.len())
            .unwrap_or(0);
        let wanted = depth.min(self.config.max_workers.max(1));
        while self.workers_spawned < wanted {
            let shared = Arc::clone(&self.shared);
            let sender = self.result_sender.clone();
            let name = format!("basemap-tile-{}", self.workers_spawned);
            let spawned = std::thread::Builder::new()
                .name(name)
                .spawn(move || worker_loop(&shared, &sender));
            match spawned {
                Ok(_handle) => self.workers_spawned += 1,
                // If the OS will not give us a thread, the tiles simply do not
                // arrive. That is a degraded basemap, not a broken app.
                Err(_) => break,
            }
        }
    }

    #[cfg(test)]
    fn http_client_was_constructed(&self) -> bool {
        self.shared.client.get().is_some()
    }

    #[cfg(test)]
    fn worker_count(&self) -> usize {
        self.workers_spawned
    }
}

impl Drop for TileStore {
    fn drop(&mut self) {
        // Ask the workers to stop and do NOT join them: a worker blocked in a
        // 15-second request would otherwise hold up application shutdown. They
        // observe the flag when their current request finishes.
        self.shared.shutdown.store(true, Ordering::Relaxed);
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.clear();
        }
        self.shared.wake_worker.notify_all();
    }
}

#[derive(Default)]
struct JobQueue {
    /// `(key, batch)`, ordered newest batch first and FIFO *within* a batch.
    jobs: VecDeque<(Key, u64)>,
    queued: HashSet<Key>,
    /// The current batch. [`TileStore::retain`] bumps it once per frame, so a
    /// batch is one frame's requests.
    batch: u64,
}

impl JobQueue {
    /// Start a new batch. Everything already queued becomes "a previous
    /// frame's work" and sorts behind whatever the next frame asks for.
    fn begin_batch(&mut self) {
        self.batch = self.batch.wrapping_add(1);
    }

    /// Queue a job, trimming from the back and returning whatever was
    /// dropped.
    ///
    /// Two orderings, both load-bearing, and they pull in opposite directions:
    ///
    /// * **Newest batch first** is what keeps a pan responsive: the tiles
    ///   under the cursor now outrank the ones that were under it a second
    ///   ago.
    /// * **FIFO within a batch**, because the caller requests tiles
    ///   centre-out ([`crate::visible_tiles`] orders them that way precisely
    ///   so a cold view fills in from where the user is looking). A pure LIFO
    ///   queue — the previous shape of this structure — silently reversed
    ///   that within every frame: the centre tile was requested first, pushed
    ///   deepest, and *fetched last*, so a cold zoom sharpened from the edges
    ///   inward. Measured on a scripted z9→z11 quick zoom over KTLX before
    ///   this ordering existed.
    ///
    /// The back of the queue is therefore the oldest batch's least-central
    /// tile, which is also the right thing to drop on overflow.
    fn push(&mut self, key: Key) -> Vec<Key> {
        if !self.queued.insert(key) {
            return Vec::new();
        }
        // The current batch is always a prefix of the deque: batches are
        // pushed in increasing number and popped from the front.
        let position = self
            .jobs
            .iter()
            .take_while(|(_, batch)| *batch == self.batch)
            .count();
        self.jobs.insert(position, (key, self.batch));
        let mut dropped = Vec::new();
        while self.jobs.len() > MAX_QUEUE_DEPTH {
            if let Some((stale, _)) = self.jobs.pop_back() {
                self.queued.remove(&stale);
                dropped.push(stale);
            }
        }
        dropped
    }

    fn pop_newest(&mut self) -> Option<Key> {
        let (key, _) = self.jobs.pop_front()?;
        self.queued.remove(&key);
        Some(key)
    }

    fn retain(&mut self, wanted: &HashSet<Key>) -> Vec<Key> {
        let mut cancelled = Vec::new();
        self.jobs.retain(|(key, _)| {
            if wanted.contains(key) {
                true
            } else {
                cancelled.push(*key);
                false
            }
        });
        for key in &cancelled {
            self.queued.remove(key);
        }
        cancelled
    }

    fn clear(&mut self) {
        self.jobs.clear();
        self.queued.clear();
    }
}

#[derive(Default)]
struct Counters {
    requested: AtomicU64,
    served_from_disk: AtomicU64,
    revalidated_304: AtomicU64,
    downloaded: AtomicU64,
    absent: AtomicU64,
    failed: AtomicU64,
    bytes_downloaded: AtomicU64,
    in_flight: AtomicUsize,
}

struct Shared {
    queue: Mutex<JobQueue>,
    wake_worker: Condvar,
    cache: Option<TileDiskCache>,
    offline: AtomicBool,
    shutdown: AtomicBool,
    notify: Arc<dyn Fn() + Send + Sync>,
    user_agent: String,
    client: OnceLock<Option<reqwest::blocking::Client>>,
    counters: Counters,
}

impl Shared {
    fn is_offline(&self) -> bool {
        self.offline.load(Ordering::Relaxed)
    }

    /// Built once, on first use. Deliberately never touched in offline mode,
    /// which is what lets a test prove no socket was opened.
    fn client(&self) -> Option<&reqwest::blocking::Client> {
        self.client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .user_agent(self.user_agent.clone())
                    .connect_timeout(CONNECT_TIMEOUT)
                    .timeout(REQUEST_TIMEOUT)
                    // Every provider URL is HTTPS; refusing a downgrade costs
                    // nothing and closes a redirect-to-plaintext path.
                    .https_only(true)
                    .build()
                    .ok()
            })
            .as_ref()
    }
}

enum Outcome {
    Ready(Arc<DecodedTile>),
    Absent,
    Failed,
    Parked,
}

struct WorkerResult {
    key: Key,
    outcome: Outcome,
}

fn worker_loop(shared: &Arc<Shared>, sender: &mpsc::Sender<WorkerResult>) {
    loop {
        let key = {
            let mut queue = shared.queue.lock().expect("tile queue");
            loop {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(key) = queue.pop_newest() {
                    break key;
                }
                let (guard, _timeout) = shared
                    .wake_worker
                    .wait_timeout(queue, Duration::from_millis(500))
                    .expect("tile queue wake");
                queue = guard;
            }
        };

        shared.counters.in_flight.fetch_add(1, Ordering::Relaxed);
        let outcome = process(shared, key.0, key.1);
        shared.counters.in_flight.fetch_sub(1, Ordering::Relaxed);

        if sender.send(WorkerResult { key, outcome }).is_err() {
            return; // The store is gone.
        }
        (shared.notify)();
    }
}

fn process(shared: &Arc<Shared>, provider: TileProvider, tile: TileId) -> Outcome {
    let now = now_unix();
    let mut cached = shared
        .cache
        .as_ref()
        .and_then(|cache| cache.load(provider, tile));

    // A cached body inside the provider's minimum lifetime is used without
    // touching the network at all. That is how the rate limit is honoured.
    if let Some(entry) = &cached
        && (entry.is_fresh(provider.min_cache_seconds(), now) || shared.is_offline())
    {
        match decode_tile(provider, tile, &entry.body) {
            Ok(decoded) => {
                shared
                    .counters
                    .served_from_disk
                    .fetch_add(1, Ordering::Relaxed);
                return Outcome::Ready(Arc::new(decoded));
            }
            Err(_) => {
                // The file was structurally valid but is not an image. Drop it
                // so the next attempt refetches instead of failing forever —
                // and forget its ETag with it. Revalidating a body we have
                // just thrown away earns a 304 meaning "your copy is current",
                // which for a copy that does not decode is the one answer we
                // cannot use.
                discard_cached(shared, provider, tile);
                cached = None;
            }
        }
    }

    if shared.is_offline() {
        return Outcome::Parked;
    }
    let Some(client) = shared.client() else {
        return Outcome::Failed;
    };

    let mut request = client.get(provider.tile_url(tile));
    if let Some(etag) = cached.as_ref().and_then(|entry| entry.etag.as_deref()) {
        // The USGS services send an ETag and no Last-Modified, so
        // If-None-Match is the only revalidation that works against them.
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }

    let response = match request.send() {
        Ok(response) => response,
        Err(_) => return stale_or_failed(shared, provider, tile, cached),
    };

    let status = response.status();
    if classify_status(status) == StatusVerdict::NotModified {
        shared
            .counters
            .revalidated_304
            .fetch_add(1, Ordering::Relaxed);
        if let Some(entry) = &cached {
            if let Ok(decoded) = decode_tile(provider, tile, &entry.body) {
                // Only now, having confirmed the body we kept is usable, does
                // its lifetime restart. Touching first would mark an
                // undecodable entry fresh for another day.
                if let Some(cache) = shared.cache.as_ref() {
                    let _ = cache.touch(provider, tile, now);
                }
                return Outcome::Ready(Arc::new(decoded));
            }
            // The provider says our copy is current and our copy is not an
            // image. Delete it so the retry asks unconditionally instead of
            // being told "unchanged" about the same rubbish forever.
            discard_cached(shared, provider, tile);
        }
        return Outcome::Failed;
    }
    if classify_status(status) == StatusVerdict::Absent {
        // The provider has nothing here and never will. Permanent, and
        // distinct from a failure so it is not re-probed every frame.
        shared.counters.absent.fetch_add(1, Ordering::Relaxed);
        return Outcome::Absent;
    }
    if classify_status(status) != StatusVerdict::Body {
        return stale_or_failed(shared, provider, tile, cached);
    }

    // Validate what the response claims before reading a byte of it. USGS
    // answers out-of-coverage with a small `text/html` body, and a captive
    // portal answers everything with one.
    let is_image = declares_an_image(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    );
    if !is_image {
        return stale_or_failed(shared, provider, tile, cached);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TILE_ENCODED_BYTES as u64)
    {
        return stale_or_failed(shared, provider, tile, cached);
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // Read through a limit rather than trusting Content-Length, so a server
    // that lies about its length cannot make a worker allocate without bound.
    let mut body = Vec::new();
    {
        use std::io::Read;
        let mut limited = response.take(MAX_TILE_ENCODED_BYTES as u64 + 1);
        if limited.read_to_end(&mut body).is_err() {
            return stale_or_failed(shared, provider, tile, cached);
        }
    }
    if body.len() > MAX_TILE_ENCODED_BYTES {
        return stale_or_failed(shared, provider, tile, cached);
    }

    let decoded = match decode_tile(provider, tile, &body) {
        Ok(decoded) => decoded,
        Err(_) => return stale_or_failed(shared, provider, tile, cached),
    };

    // Only now, after the body has decoded, does it reach the disk.
    if let Some(cache) = shared.cache.as_ref() {
        let _ = cache.store(provider, tile, etag.as_deref(), &body, now);
    }
    shared.counters.downloaded.fetch_add(1, Ordering::Relaxed);
    shared
        .counters
        .bytes_downloaded
        .fetch_add(body.len() as u64, Ordering::Relaxed);
    Outcome::Ready(Arc::new(decoded))
}

/// What a response status means for a tile.
///
/// Extracted from `process` so it can be tested without a network. It is the
/// most consequential classification in this module and the least visible: the
/// difference between [`StatusVerdict::Absent`] and [`StatusVerdict::Transient`]
/// is the difference between a hole the layer covers with a coarser tile once,
/// and a tile that is re-requested, re-queued and re-fetched three times over a
/// minute — per tile, on a provider where 404 is *normal*. The USGS shaded
/// relief layer answers 404 for every tile in a pane at some zooms over some
/// sites, measured, so getting this wrong is not a corner case there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusVerdict {
    /// 304: our cached body is current.
    NotModified,
    /// The provider says this tile does not exist. Permanent for the session.
    Absent,
    /// Something went wrong that might not go wrong again.
    Transient,
    /// A body worth reading.
    Body,
}

fn classify_status(status: reqwest::StatusCode) -> StatusVerdict {
    match status {
        reqwest::StatusCode::NOT_MODIFIED => StatusVerdict::NotModified,
        // 404 and 410 are the two "there is nothing here" answers. Everything
        // else in the 4xx range — 401, 403, 429 — is about *us*, not about the
        // tile, and may well be different on the next attempt.
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE => StatusVerdict::Absent,
        other if other.is_success() => StatusVerdict::Body,
        _ => StatusVerdict::Transient,
    }
}

/// Whether a response claims to carry an image, from its `Content-Type`.
///
/// A missing or non-image type is refused *before* the body is read. This is
/// what stops a captive portal's login page, or an error page served with 200,
/// from being pulled into memory and handed to an image decoder. Extracted for
/// the same reason as [`classify_status`]: it cannot be reached from a test
/// otherwise.
fn declares_an_image(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("image/")
    })
}

/// Delete a cache entry whose body did not decode.
///
/// The error is ignored on purpose: a read-only cache directory, an antivirus
/// holding the file open, or another process having removed it already are all
/// recoverable by simply fetching the tile again.
fn discard_cached(shared: &Arc<Shared>, provider: TileProvider, tile: TileId) {
    if let Some(cache) = shared.cache.as_ref() {
        let _ = std::fs::remove_file(cache.path_for(provider, tile));
    }
}

/// A network error with a stale cached body still beats a hole in the map, so
/// the stale copy is served and the tile is not marked failed.
fn stale_or_failed(
    shared: &Arc<Shared>,
    provider: TileProvider,
    tile: TileId,
    cached: Option<CachedTile>,
) -> Outcome {
    if let Some(entry) = cached
        && let Ok(decoded) = decode_tile(provider, tile, &entry.body)
    {
        shared
            .counters
            .served_from_disk
            .fetch_add(1, Ordering::Relaxed);
        return Outcome::Ready(Arc::new(decoded));
    }
    shared.counters.failed.fetch_add(1, Ordering::Relaxed);
    Outcome::Failed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(index: u32) -> TileId {
        TileId::new(9, 100 + index, 200).expect("valid")
    }

    fn silent_wake() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    fn temp_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "basemap-tiles-store-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp root");
        path
    }

    fn offline_config(disk_root: Option<PathBuf>) -> TileCacheConfig {
        TileCacheConfig {
            disk_root,
            max_disk_bytes: 8 * 1024 * 1024,
            max_workers: 4,
            user_agent: default_user_agent(),
            offline: true,
        }
    }

    #[test]
    fn the_default_user_agent_names_the_application_and_a_contact() {
        let agent = default_user_agent();
        assert!(agent.starts_with("GenericRadar/"), "{agent}");
        assert!(agent.contains("(+https://"), "{agent}");
        // The OSMF policy blocks library default agents specifically.
        assert!(!agent.to_ascii_lowercase().contains("reqwest"), "{agent}");
    }

    /// Offline with no disk cache: nothing to do, so not one thread is
    /// spawned and the HTTP client is never even constructed.
    #[test]
    fn offline_without_a_cache_never_spawns_a_worker_or_opens_a_socket() {
        let mut store = TileStore::new(offline_config(None), silent_wake());
        for index in 0..8 {
            store.request(TileProvider::UsgsImagery, tile(index));
        }
        assert_eq!(store.worker_count(), 0, "offline must not spawn workers");
        assert!(!store.http_client_was_constructed());
        assert!(store.drain_ready(16).is_empty());
        for index in 0..8 {
            assert_ne!(
                store.state(TileProvider::UsgsImagery, tile(index)),
                TileState::Failed,
                "offline must never record a failure"
            );
        }
        assert_eq!(store.metrics().downloaded, 0);
        assert_eq!(store.metrics().failed, 0);
    }

    /// Offline with an empty disk cache: workers may run, because reading and
    /// decoding a cache file is real work that belongs off the UI thread, but
    /// no socket is opened and nothing is ever marked failed.
    #[test]
    fn offline_with_an_empty_cache_parks_tiles_instead_of_failing() {
        let root = temp_root("offline-empty");
        let mut store = TileStore::new(offline_config(Some(root.clone())), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));
        assert_eq!(store.request(key.0, key.1), TileState::Pending);

        let deadline = Instant::now() + Duration::from_secs(5);
        while store.metrics().in_flight > 0 || store.metrics().queued > 0 {
            if Instant::now() > deadline {
                panic!("the offline worker never drained the queue");
            }
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(store.drain_ready(8).is_empty());
        assert_eq!(store.state(key.0, key.1), TileState::Pending);
        assert!(
            !store.http_client_was_constructed(),
            "offline must not construct an HTTP client"
        );

        // The parked tile is not re-queued on subsequent frames: that is what
        // stops an offline machine from spinning the queue forever.
        let requested_before = store.metrics().requested;
        for _ in 0..50 {
            store.request(key.0, key.1);
        }
        assert_eq!(store.metrics().requested, requested_before);
        assert_eq!(store.metrics().queued, 0);

        // Going back online releases it.
        store.set_offline(false);
        assert_eq!(store.state(key.0, key.1), TileState::Unknown);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cache file that is structurally valid but whose body is not an image
    /// must not become a permanent hole.
    ///
    /// This is a real shape, not an invented one: a half-synced filesystem, a
    /// truncated write from a hard power-off, or a byte flipped under the file
    /// all produce it. The entry must be deleted on the first attempt so the
    /// next one refetches — and, once deleted, its ETag must not be used to
    /// revalidate a body that no longer exists.
    #[test]
    fn a_cached_body_that_is_not_an_image_is_deleted_rather_than_retried_forever() {
        let root = temp_root("corrupt-body");
        let mut store = TileStore::new(offline_config(Some(root.clone())), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));
        let cache = store.shared.cache.as_ref().expect("cache");
        // A perfectly well-formed entry wrapped around an error page.
        cache
            .store(
                key.0,
                key.1,
                Some("\"197d318d730\""),
                b"<html><head><title>404 Not Found</title></head></html>",
                now_unix(),
            )
            .expect("store");
        let path = cache.path_for(key.0, key.1);
        assert!(path.exists());

        assert_eq!(store.request(key.0, key.1), TileState::Pending);
        let deadline = Instant::now() + Duration::from_secs(5);
        while path.exists() {
            assert!(Instant::now() < deadline, "the bad entry was never removed");
            std::thread::yield_now();
        }
        assert!(store.drain_ready(4).is_empty(), "nothing decodable arrived");
        assert!(!store.http_client_was_constructed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_provider_without_a_lawful_cache_is_refused_rather_than_fetched() {
        let mut store = TileStore::new(
            TileCacheConfig {
                disk_root: None,
                offline: false,
                ..TileCacheConfig::default()
            },
            silent_wake(),
        );
        assert!(!store.permits(TileProvider::OpenStreetMap));
        assert_eq!(
            store.request(TileProvider::OpenStreetMap, tile(0)),
            TileState::Absent
        );
        assert_eq!(store.worker_count(), 0);
        assert!(!store.http_client_was_constructed());
        // The USGS providers carry no such condition.
        assert!(store.permits(TileProvider::UsgsImagery));
    }

    /// A cache directory that cannot be written is not a cache.
    ///
    /// The OpenStreetMap tile usage policy's minimum cache lifetime is
    /// enforced by the disk cache and by nothing else, so a store whose cache
    /// directory silently swallows every write cannot meet it: every session
    /// would re-download the same tiles from a community-funded server. A
    /// configured-but-broken path must therefore read as *no cache*, not as a
    /// cache.
    ///
    /// The unwritable directory here is a real one — a path whose parent is an
    /// ordinary file, which every platform refuses to create a directory
    /// under.
    #[test]
    fn a_cache_directory_that_cannot_be_written_refuses_the_provider_that_needs_one() {
        let root = temp_root("unwritable");
        let blocker = root.join("not-a-directory");
        std::fs::write(&blocker, b"this is a file").expect("write blocker");
        let unwritable = blocker.join("cache");

        let mut store = TileStore::new(
            TileCacheConfig {
                disk_root: Some(unwritable.clone()),
                offline: false,
                ..TileCacheConfig::default()
            },
            silent_wake(),
        );
        assert!(
            !store.permits(TileProvider::OpenStreetMap),
            "a cache that cannot keep a tile does not satisfy a minimum cache lifetime"
        );
        assert_eq!(
            store.request(TileProvider::OpenStreetMap, tile(0)),
            TileState::Absent,
            "the refused provider must not reach the network"
        );
        assert_eq!(store.worker_count(), 0);
        assert!(!store.http_client_was_constructed());
        // The USGS layers carry no such condition and stay available, they
        // simply run without a cache.
        assert!(store.permits(TileProvider::UsgsImagery));

        // And a directory that really is writable is permitted.
        let working = TileStore::new(
            TileCacheConfig {
                disk_root: Some(root.join("good")),
                offline: false,
                ..TileCacheConfig::default()
            },
            silent_wake(),
        );
        assert!(working.permits(TileProvider::OpenStreetMap));
        drop(working);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Today every provider's ceiling equals [`crate::MAX_TILE_ZOOM`] and
    /// `TileId` refuses to exist above it, so the ceiling branch in `request`
    /// is unreachable through the public API. This test pins that invariant:
    /// if `MAX_TILE_ZOOM` is ever raised past a provider's own ceiling, the
    /// branch starts doing work and this test is where that is recorded.
    #[test]
    fn the_deepest_constructible_tile_is_inside_every_providers_ceiling() {
        let mut store = TileStore::new(offline_config(Some(temp_root("ceiling"))), silent_wake());
        let deepest = TileId::new(crate::MAX_TILE_ZOOM, 1, 1).expect("valid");
        for provider in TileProvider::ALL {
            assert_eq!(provider.max_zoom(), crate::MAX_TILE_ZOOM, "{provider:?}");
        }
        assert!(TileId::new(crate::MAX_TILE_ZOOM + 1, 1, 1).is_none());
        assert_eq!(
            store.request(TileProvider::UsgsImagery, deepest),
            TileState::Pending
        );
        let root = store.cache_root().map(std::path::Path::to_path_buf);
        drop(store);
        if let Some(root) = root {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn requesting_the_same_tile_repeatedly_queues_it_once() {
        let root = temp_root("idempotent");
        let mut store = TileStore::new(offline_config(Some(root.clone())), silent_wake());
        for _ in 0..25 {
            store.request(TileProvider::UsgsTopo, tile(3));
        }
        assert_eq!(store.metrics().requested, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cancellation is what stops a fast pan from downloading the country.
    #[test]
    fn retain_drops_queued_work_before_it_reaches_the_network() {
        let root = temp_root("retain");
        // Offline so nothing actually leaves; the queue behaviour is identical.
        let mut store = TileStore::new(
            TileCacheConfig {
                max_workers: 0,
                ..offline_config(Some(root.clone()))
            },
            silent_wake(),
        );
        // max_workers is floored to one, so hold the workers off by asserting
        // on the queue immediately after enqueueing.
        let mut keys = Vec::new();
        for index in 0..40u32 {
            let key = (TileProvider::UsgsImagery, tile(index));
            store.request(key.0, key.1);
            keys.push(key);
        }
        let wanted: HashSet<Key> = keys.iter().copied().take(3).collect();
        store.retain(&wanted);

        let queue = store.shared.queue.lock().expect("queue");
        assert!(
            queue.jobs.iter().all(|(key, _)| wanted.contains(key)),
            "retain left unwanted work in the queue"
        );
        drop(queue);

        // Cancelled tiles fall back to Unknown so they can be asked for again.
        let cancelled = keys[10];
        assert!(matches!(
            store.state(cancelled.0, cancelled.1),
            TileState::Unknown | TileState::Pending
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cancelling a tile must not leave a record of it behind.
    ///
    /// `retain` runs once per frame and a minute of panning at a fine zoom
    /// cancels tens of thousands of tiles. `states` is the one map nothing
    /// else prunes, so writing "nothing is known about this tile" into it is a
    /// leak that only shows up in a long session — the shape of bug that gets
    /// blamed on the renderer. `state()` reports `Unknown` for a missing key,
    /// so removing the entry is the same answer with none of the growth.
    ///
    /// Built by hand rather than through `request` so that no worker thread
    /// exists to make the count racy: this store is offline with no cache, the
    /// one configuration that never spawns one.
    #[test]
    fn cancelling_work_leaves_no_entry_behind_in_the_state_map() {
        let mut store = TileStore::new(offline_config(None), silent_wake());
        let keys: Vec<Key> = (0..100u32)
            .map(|index| (TileProvider::UsgsImagery, tile(index)))
            .collect();
        {
            let mut queue = store.shared.queue.lock().expect("queue");
            for key in &keys {
                assert!(queue.push(*key).is_empty(), "the queue overflowed");
                store.states.insert(*key, TileState::Pending);
            }
        }
        assert_eq!(store.states.len(), keys.len());

        store.retain(&HashSet::new());
        assert!(
            store.states.is_empty(),
            "{} cancelled tiles were remembered as Unknown",
            store.states.len()
        );
        assert_eq!(store.metrics().queued, 0);
        assert_eq!(store.worker_count(), 0);
        // And the observable answer is unchanged.
        assert_eq!(store.state(keys[0].0, keys[0].1), TileState::Unknown);
    }

    #[test]
    fn the_queue_is_bounded_and_drops_the_oldest_batchs_least_central_work() {
        let mut queue = JobQueue::default();
        let mut dropped_total = 0;
        for index in 0..(MAX_QUEUE_DEPTH as u32 + 50) {
            // A new batch per push: this reproduces the old pure-LIFO shape,
            // where every push outranks everything before it.
            queue.begin_batch();
            dropped_total += queue.push((TileProvider::UsgsImagery, tile(index))).len();
        }
        assert_eq!(queue.jobs.len(), MAX_QUEUE_DEPTH);
        assert_eq!(queue.queued.len(), MAX_QUEUE_DEPTH);
        assert_eq!(dropped_total, 50);
        // Newest batch first: the most recent push is at the head.
        assert_eq!(
            queue.pop_newest(),
            Some((TileProvider::UsgsImagery, tile(MAX_QUEUE_DEPTH as u32 + 49)))
        );
        // And the oldest were the ones dropped.
        assert!(!queue.queued.contains(&(TileProvider::UsgsImagery, tile(0))));
    }

    /// Within one frame's batch the queue must preserve the caller's order,
    /// because the caller requests tiles centre-out and the whole point of
    /// that ordering is that the centre of the view is fetched FIRST.
    ///
    /// REGRESSION, measured before it was fixed: the queue used to be a pure
    /// LIFO, so the centre tile — requested first — was fetched *last*, and a
    /// cold quick zoom over KTLX sharpened from the pane's edges inward.
    #[test]
    fn one_frames_requests_are_fetched_in_the_order_they_were_made() {
        let mut queue = JobQueue::default();
        // Frame one asks for three tiles, centre-out.
        queue.begin_batch();
        for index in 0..3 {
            queue.push((TileProvider::UsgsImagery, tile(index)));
        }
        // Frame two (a pan, say) asks for three different tiles, centre-out.
        queue.begin_batch();
        for index in 10..13 {
            queue.push((TileProvider::UsgsImagery, tile(index)));
        }
        // The newer frame's work comes first, in its own order; then the
        // older frame's remainder, still in ITS order.
        let order: Vec<Key> = std::iter::from_fn(|| queue.pop_newest()).collect();
        let expected: Vec<Key> = [10, 11, 12, 0, 1, 2]
            .into_iter()
            .map(|index| (TileProvider::UsgsImagery, tile(index)))
            .collect();
        assert_eq!(order, expected);
    }

    /// Popping mid-batch and then pushing more of the same batch must not
    /// let the later pushes jump in front of earlier, still-queued ones.
    #[test]
    fn a_batch_stays_in_order_even_while_workers_are_draining_it() {
        let mut queue = JobQueue::default();
        queue.begin_batch();
        for index in 0..4 {
            queue.push((TileProvider::UsgsImagery, tile(index)));
        }
        assert_eq!(
            queue.pop_newest(),
            Some((TileProvider::UsgsImagery, tile(0)))
        );
        queue.push((TileProvider::UsgsImagery, tile(4)));
        let order: Vec<Key> = std::iter::from_fn(|| queue.pop_newest()).collect();
        let expected: Vec<Key> = (1..5)
            .map(|index| (TileProvider::UsgsImagery, tile(index)))
            .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn pushing_a_queued_tile_again_is_a_no_op() {
        let mut queue = JobQueue::default();
        let key = (TileProvider::UsgsTopo, tile(1));
        assert!(queue.push(key).is_empty());
        assert!(queue.push(key).is_empty());
        assert_eq!(queue.jobs.len(), 1);
    }

    #[test]
    fn forget_makes_a_ready_tile_requestable_again() {
        let mut store = TileStore::new(offline_config(None), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));
        store.states.insert(key, TileState::Ready);
        assert_eq!(store.request(key.0, key.1), TileState::Ready);
        store.forget(key.0, key.1);
        assert_eq!(store.state(key.0, key.1), TileState::Unknown);
    }

    #[test]
    fn an_absent_tile_is_never_retried() {
        let mut store = TileStore::new(offline_config(None), silent_wake());
        let key = (TileProvider::UsgsShadedRelief, tile(0));
        store.states.insert(key, TileState::Absent);
        for _ in 0..100 {
            assert_eq!(store.request(key.0, key.1), TileState::Absent);
        }
        assert_eq!(store.metrics().requested, 0);
        assert_eq!(store.metrics().queued, 0);
    }

    /// 404 must mean *absent*, not *failed*.
    ///
    /// This is the classification the whole ancestor-fallback design rests on,
    /// and it was reachable only through a live network test until this one
    /// existed: swapping `Absent` for `Failed` passed the entire offline gate.
    /// The consequence of that swap is not subtle — the USGS shaded-relief
    /// layer answers 404 for *every tile in the pane* at zoom 9 over Oklahoma
    /// City (measured against the live service), so the pane would issue three
    /// rounds of doomed requests for every tile in it, on a layer that is
    /// working exactly as its publisher intended.
    #[test]
    fn the_status_classification_separates_a_missing_tile_from_a_bad_day() {
        use reqwest::StatusCode;
        assert_eq!(
            classify_status(StatusCode::NOT_MODIFIED),
            StatusVerdict::NotModified
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND),
            StatusVerdict::Absent
        );
        assert_eq!(classify_status(StatusCode::GONE), StatusVerdict::Absent);
        assert_eq!(classify_status(StatusCode::OK), StatusVerdict::Body);
        assert_eq!(
            classify_status(StatusCode::NO_CONTENT),
            StatusVerdict::Body,
            "a 2xx is a body; the decoder is what refuses an empty one"
        );
        // Everything that is about us rather than about the tile stays
        // retryable, because it can be different in five seconds.
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert_eq!(
                classify_status(status),
                StatusVerdict::Transient,
                "{status} must be retryable, not permanent"
            );
        }
    }

    /// A response is refused on what it says it is, before its body is read.
    ///
    /// The USGS services answer an out-of-coverage tile with `text/html`, and
    /// a captive portal answers *everything* with one — with status 200. This
    /// guard is the only thing between that and an image decoder, and it too
    /// was unreachable from a test until it was lifted out of `process`.
    #[test]
    fn only_a_declared_image_body_is_read() {
        assert!(declares_an_image(Some("image/jpeg")));
        assert!(declares_an_image(Some("image/png")));
        assert!(declares_an_image(Some("image/png; charset=binary")));
        assert!(declares_an_image(Some(" image/jpeg")));
        // Case is not significant in a media type (RFC 9110 s. 8.3).
        assert!(declares_an_image(Some("Image/PNG")));

        assert!(!declares_an_image(None), "no type means no promise");
        assert!(!declares_an_image(Some("text/html;charset=utf-8")));
        assert!(!declares_an_image(Some("application/json")));
        assert!(!declares_an_image(Some("")));
        assert!(
            !declares_an_image(Some("multipart/related; type=image/jpeg")),
            "the type has to BE an image, not merely mention one"
        );
    }

    /// The backoff schedule: three widening tries, then silence for the
    /// session. A firewalled machine must not generate connections forever.
    #[test]
    fn the_retry_schedule_widens_and_then_stops() {
        let now = Instant::now();
        let past = now - Duration::from_secs(1);
        let future = now + Duration::from_secs(3_600);

        for attempts in 0..RETRY_BACKOFF.len() as u8 {
            assert!(
                retry_is_due(
                    Some(&Failure {
                        attempts,
                        retry_after: past
                    }),
                    now
                ),
                "attempt {attempts} was due and should have been allowed"
            );
            assert!(
                !retry_is_due(
                    Some(&Failure {
                        attempts,
                        retry_after: future
                    }),
                    now
                ),
                "attempt {attempts} retried before its backoff expired"
            );
        }

        // Past the budget, even a long-expired backoff must not retry.
        assert!(!retry_is_due(
            Some(&Failure {
                attempts: RETRY_BACKOFF.len() as u8,
                retry_after: past
            }),
            now
        ));
        assert!(!retry_is_due(
            Some(&Failure {
                attempts: 250,
                retry_after: past
            }),
            now
        ));
        // A tile marked failed with no recorded reason gets one attempt.
        assert!(retry_is_due(None, now));

        // The schedule itself: widening, and long enough to be polite.
        assert_eq!(
            RETRY_BACKOFF,
            [
                Duration::from_secs(5),
                Duration::from_secs(15),
                Duration::from_secs(60)
            ]
        );
    }

    /// The schedule has to survive the retry it schedules.
    ///
    /// REGRESSION. `request` is the only way a retry is ever made, so if it
    /// clears the failure record on its way to the queue the counter restarts
    /// at zero every time: every tile on screen is retried every five seconds
    /// for as long as the application is open. That is the exact "heartbeat of
    /// doomed connections" this module's header says it avoids, and the
    /// schedule unit test above cannot see it because it never calls
    /// `request`.
    ///
    /// Offline **with** a cache directory is used deliberately: that is the
    /// one configuration where `enqueue` runs its whole body — queueing the
    /// job like an online store would — while still guaranteeing no socket is
    /// opened. The worker's answer is never drained here, so nothing it does
    /// can reach `failures`.
    #[test]
    fn retrying_a_failed_tile_keeps_its_place_in_the_backoff_schedule() {
        let root = temp_root("backoff-through-request");
        let mut store = TileStore::new(offline_config(Some(root.clone())), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));

        // The state a firewalled machine is in fifteen seconds after its
        // second failure: two attempts spent, backoff expired.
        store.states.insert(key, TileState::Failed);
        store.failures.insert(
            key,
            Failure {
                attempts: 2,
                retry_after: Instant::now() - Duration::from_secs(1),
            },
        );

        assert_eq!(
            store.request(key.0, key.1),
            TileState::Pending,
            "an expired backoff must allow the retry through"
        );
        assert_eq!(
            store.failures.get(&key).map(|failure| failure.attempts),
            Some(2),
            "the retry erased the failure record, so the next failure restarts \
             the five-second schedule and the backoff never widens"
        );

        // And when that retry fails, the schedule ends rather than looping.
        store
            .result_sender
            .send(WorkerResult {
                key,
                outcome: Outcome::Failed,
            })
            .expect("send");
        store.drain_ready(4);
        assert_eq!(store.failures[&key].attempts, 3);
        assert!(!retry_is_due(store.failures.get(&key), Instant::now()));
        assert_eq!(
            store.request(key.0, key.1),
            TileState::Failed,
            "a tile that has spent its schedule must not be queued again"
        );
        assert!(!store.http_client_was_constructed());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `drain_ready` is what advances a failure through the schedule, so the
    /// two must agree: three failures and the tile is out for the session.
    #[test]
    fn draining_failures_advances_the_backoff_to_exhaustion() {
        let mut store = TileStore::new(offline_config(None), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));
        for expected_attempts in 1..=4u8 {
            store
                .result_sender
                .send(WorkerResult {
                    key,
                    outcome: Outcome::Failed,
                })
                .expect("send");
            assert!(store.drain_ready(4).is_empty());
            assert_eq!(store.state(key.0, key.1), TileState::Failed);
            assert_eq!(store.failures[&key].attempts, expected_attempts);
        }
        assert!(!retry_is_due(store.failures.get(&key), Instant::now()));
    }

    /// A real decoded tile arriving must revive the tiles that gave up during
    /// the outage that preceded it.
    ///
    /// Without this, an outage shorter than the schedule it exhausts is
    /// permanent: eighty seconds of no network turns into a pane with holes in
    /// it for the rest of the session, and the only route back is restarting
    /// the application. `set_offline` cannot cover this, because nothing told
    /// the application the network went away.
    ///
    /// The tile that arrives is the real captured USGS body, decoded through
    /// the real decoder, not a fabricated `DecodedTile`.
    #[test]
    fn a_tile_arriving_revives_the_tiles_that_gave_up_during_the_outage() {
        const REAL_JPEG_TILE: &[u8] = include_bytes!("../tests/data/usgs-imagery-9-117-202.jpg");

        let mut store = TileStore::new(offline_config(None), silent_wake());
        let spent = (TileProvider::UsgsImagery, tile(1));
        let other_provider = (TileProvider::UsgsTopo, tile(2));
        for key in [spent, other_provider] {
            store.states.insert(key, TileState::Failed);
            store.failures.insert(
                key,
                Failure {
                    attempts: RETRY_BACKOFF.len() as u8,
                    retry_after: Instant::now(),
                },
            );
        }
        assert_eq!(
            store.request(spent.0, spent.1),
            TileState::Failed,
            "a spent tile is dead until something revives it"
        );

        let arrived = (TileProvider::UsgsImagery, tile(9));
        let decoded = crate::decode::decode_tile(arrived.0, arrived.1, REAL_JPEG_TILE)
            .expect("the captured tile decodes");
        store
            .result_sender
            .send(WorkerResult {
                key: arrived,
                outcome: Outcome::Ready(Arc::new(decoded)),
            })
            .expect("send");
        assert_eq!(store.drain_ready(4).len(), 1);

        assert_eq!(
            store.state(spent.0, spent.1),
            TileState::Unknown,
            "a tile arriving proves the provider is reachable again"
        );
        assert!(!store.failures.contains_key(&spent));
        assert_eq!(
            store.state(other_provider.0, other_provider.1),
            TileState::Failed,
            "one provider answering says nothing about another one"
        );
    }

    #[test]
    fn going_back_online_clears_the_failure_backoff() {
        let mut store = TileStore::new(offline_config(None), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));
        store.states.insert(key, TileState::Failed);
        store.failures.insert(
            key,
            Failure {
                attempts: 3,
                retry_after: Instant::now() + Duration::from_secs(3_600),
            },
        );
        assert!(store.is_offline());
        store.set_offline(false);
        assert!(!store.is_offline());
        assert_eq!(store.state(key.0, key.1), TileState::Unknown);
    }

    #[test]
    fn clear_forgets_everything_including_the_disk() {
        let root = temp_root("clear");
        let mut store = TileStore::new(offline_config(Some(root.clone())), silent_wake());
        let key = (TileProvider::UsgsImagery, tile(0));
        store
            .shared
            .cache
            .as_ref()
            .expect("cache")
            .store(key.0, key.1, None, &[1, 2, 3], 0)
            .expect("store");
        store.states.insert(key, TileState::Ready);

        store.clear();
        assert_eq!(store.state(key.0, key.1), TileState::Unknown);
        assert!(
            store
                .shared
                .cache
                .as_ref()
                .expect("cache")
                .load(key.0, key.1)
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn metrics_start_at_zero_and_report_the_cache_root() {
        let root = temp_root("metrics");
        let store = TileStore::new(offline_config(Some(root.clone())), silent_wake());
        assert_eq!(store.metrics(), TileStoreMetrics::default());
        assert_eq!(store.cache_root(), Some(root.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_config_is_bounded_and_polite() {
        let config = TileCacheConfig::default();
        assert!(config.max_disk_bytes >= 64 * 1024 * 1024);
        assert!(config.max_workers >= 1 && config.max_workers <= 8);
        assert!(!config.offline);
        assert!(config.user_agent.starts_with("GenericRadar/"));
    }

    #[test]
    fn the_default_cache_directory_is_under_a_platform_cache_root() {
        let Some(directory) = default_cache_dir() else {
            return; // A bare environment with no HOME; the caller must choose.
        };
        assert!(directory.ends_with("radar-workstation/basemap-tiles"));
        assert!(directory.is_absolute(), "{directory:?}");
    }
}
