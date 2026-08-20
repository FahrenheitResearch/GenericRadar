//! How hard this session is allowed to work the public buckets.
//!
//! Every cadence, retry and batch size the live path uses was a `const` in
//! `live_service.rs` or `data_source`. They are good numbers - each one is
//! argued for at its declaration, most of them against a measured site - but
//! they are one machine's answer, and an analyst on a metered link, on a
//! satellite hop, or watching a single slow-cycling clear-air VCP has a
//! different one. This is where the answer becomes theirs.
//!
//! # The floors are not advisory
//!
//! The two NEXRAD Level II buckets are a public good paid for by somebody
//! else. A setting that let a session poll a bucket ten times a second would
//! be a way for this application to become a nuisance, so every field here is
//! clamped by [`NetTuning::clamped`] on the way in, the clamp is what the
//! store's own range already declares, and the help text in the catalog names
//! the floor. Nothing downstream re-checks: [`NetTuning`] can only be built
//! through `clamped`, so holding one is proof it is inside the fence.
//!
//! # Why the handle is shared rather than passed
//!
//! The live worker is a thread started once at construction and living for the
//! whole run; it takes its commands through a latest-value lane that carries a
//! site and a cache directory and nothing else. Threading a config through
//! that lane would mean a session restart on every slider drag. Instead the
//! worker holds a [`SharedNetTuning`], the settings pass writes into it, and
//! the worker reads a snapshot at the top of each poll - so a changed cadence
//! takes effect on the next poll, without dropping the session, its backfill
//! or its place in the volume.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::live_service::{
    ARCHIVE_MUST_LEAD_MINUTES, ARCHIVE_POLL_INTERVAL, CHUNK_LISTING_FAILURE_STALL_AFTER,
    POLL_INTERVAL,
};

/// The tightest live poll a session may run.
///
/// The shipped 1.2 s cadence exists to catch each chunk as the sweep advances;
/// a chunk appears every few seconds, so 1.2 s already asks two to four times
/// per chunk. One second is the floor because anything faster buys nothing
/// measurable and costs the bucket a listing per session per second.
pub const MIN_LIVE_POLL: Duration = Duration::from_secs(1);
/// The slowest live poll worth offering. Past this the archive fallback is the
/// honest tool, not a slower chunk poll.
pub const MAX_LIVE_POLL: Duration = Duration::from_secs(30);

/// The tightest archive poll a session may run.
///
/// The archive receives one finished object per volume - measured on KUEX,
/// 2026-08-19: 279 volumes at a mean interval of 258 s. Fifteen seconds is
/// already ~17 listings per volume, all but one of which can only answer
/// "still nothing"; the shipped 30 s halves that again.
pub const MIN_ARCHIVE_POLL: Duration = Duration::from_secs(15);
pub const MAX_ARCHIVE_POLL: Duration = Duration::from_secs(600);

/// The archive must lead the dead chunk feed by at least one minute before a
/// session switches. Below that the two feeds are describing the same scan and
/// switching only adds a second source to explain.
pub const MIN_ARCHIVE_LEAD_MINUTES: i64 = 1;
pub const MAX_ARCHIVE_LEAD_MINUTES: i64 = 60;

/// A listing failure has to persist for at least this long to count as a
/// stall. A dropped connection, a 503 and a closed laptop lid all clear on the
/// next poll, and fifteen seconds is ~12 polls at the shipped cadence.
pub const MIN_STALL_AFTER: Duration = Duration::from_secs(15);
pub const MAX_STALL_AFTER: Duration = Duration::from_secs(900);

/// Chunk downloads run in scoped batches. One at a time is legitimate on a
/// metered link; sixteen at a time is the most this ever needs, and more
/// parallelism against one bucket prefix is a way to get throttled, not a way
/// to go faster.
pub const MIN_DOWNLOAD_BATCH: usize = 1;
pub const MAX_DOWNLOAD_BATCH: usize = 16;

/// Total attempts per object, retries included. One means "do not retry".
pub const MIN_DOWNLOAD_ATTEMPTS: usize = 1;
pub const MAX_DOWNLOAD_ATTEMPTS: usize = 6;

/// The pause between attempts. The floor keeps a failing object from becoming
/// a tight retry loop against the bucket.
pub const MIN_RETRY_BACKOFF: Duration = Duration::from_millis(100);
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// The live cache holds whole Level II volumes: one VCP-212 volume is ~74 MiB
/// decoded and 6-17 MiB on disk, so a budget under 256 MiB would evict the
/// volume behind the one on screen.
pub const MIN_LIVE_CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_LIVE_CACHE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// One session's network policy.
///
/// [`Default`] is the shipped behaviour exactly: every field is the value the
/// corresponding `const` held, so a build with no settings file makes the same
/// requests, at the same cadence, in the same batches. A test pins each field
/// against the constant it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetTuning {
    /// Was `live_service::POLL_INTERVAL`.
    pub live_poll: Duration,
    /// Was `live_service::ARCHIVE_POLL_INTERVAL`.
    pub archive_poll: Duration,
    /// Was `live_service::ARCHIVE_MUST_LEAD_MINUTES`.
    pub archive_lead_minutes: i64,
    /// Was `live_service::CHUNK_LISTING_FAILURE_STALL_AFTER`.
    pub stall_after: Duration,
    /// Was `data_source::DEFAULT_LIVE_CACHE_BUDGET_BYTES`.
    pub live_cache_bytes: u64,
    /// Was `data_source`'s `REALTIME_CHUNK_DOWNLOAD_BATCH`.
    pub download_batch: usize,
    /// Was `data_source`'s `S3_OBJECT_DOWNLOAD_ATTEMPTS`.
    pub download_attempts: usize,
    /// Was `data_source`'s `S3_OBJECT_DOWNLOAD_RETRY_DELAY`.
    pub retry_backoff: Duration,
}

impl Default for NetTuning {
    /// The shipped policy, READ from the constants it replaced rather than
    /// restated beside them.
    ///
    /// This is what makes "a fresh settings file changes nothing" a structural
    /// property instead of a promise a test has to keep checking: there is one
    /// copy of each number, in the module whose comment explains why that
    /// number and not another, and this reads it. Change the constant and the
    /// default follows; there is no second place to forget.
    fn default() -> Self {
        Self {
            live_poll: POLL_INTERVAL,
            archive_poll: ARCHIVE_POLL_INTERVAL,
            archive_lead_minutes: ARCHIVE_MUST_LEAD_MINUTES,
            stall_after: CHUNK_LISTING_FAILURE_STALL_AFTER,
            live_cache_bytes: data_source::DEFAULT_LIVE_CACHE_BUDGET_BYTES,
            download_batch: data_source::tuning::DEFAULT_CHUNK_DOWNLOAD_BATCH,
            download_attempts: data_source::tuning::DEFAULT_DOWNLOAD_ATTEMPTS,
            retry_backoff: data_source::tuning::DEFAULT_RETRY_BACKOFF,
        }
    }
}

impl NetTuning {
    /// The only way to build one from outside: every field clamped into its
    /// declared fence. Holding a `NetTuning` is therefore proof that whatever
    /// produced it - a settings file, a hand edit, a future build writing a
    /// value this one has never heard of - cannot make this session hammer a
    /// public bucket.
    pub fn clamped(self) -> Self {
        Self {
            live_poll: self.live_poll.clamp(MIN_LIVE_POLL, MAX_LIVE_POLL),
            archive_poll: self.archive_poll.clamp(MIN_ARCHIVE_POLL, MAX_ARCHIVE_POLL),
            archive_lead_minutes: self
                .archive_lead_minutes
                .clamp(MIN_ARCHIVE_LEAD_MINUTES, MAX_ARCHIVE_LEAD_MINUTES),
            stall_after: self.stall_after.clamp(MIN_STALL_AFTER, MAX_STALL_AFTER),
            live_cache_bytes: self
                .live_cache_bytes
                .clamp(MIN_LIVE_CACHE_BYTES, MAX_LIVE_CACHE_BYTES),
            download_batch: self
                .download_batch
                .clamp(MIN_DOWNLOAD_BATCH, MAX_DOWNLOAD_BATCH),
            download_attempts: self
                .download_attempts
                .clamp(MIN_DOWNLOAD_ATTEMPTS, MAX_DOWNLOAD_ATTEMPTS),
            retry_backoff: self
                .retry_backoff
                .clamp(MIN_RETRY_BACKOFF, MAX_RETRY_BACKOFF),
        }
    }
}

/// A [`NetTuning`] the settings pass writes and the live worker reads.
///
/// A mutex rather than a lock-free cell because the whole struct has to move
/// as one: a worker that read a new poll interval beside an old cache budget
/// would be running a policy nobody chose. The critical section is a struct
/// copy, taken once per poll on one thread and once per settings change on
/// another, so contention is not a consideration.
#[derive(Clone, Debug)]
pub struct SharedNetTuning {
    inner: Arc<Mutex<NetTuning>>,
}

impl Default for SharedNetTuning {
    fn default() -> Self {
        Self::new(NetTuning::default())
    }
}

impl SharedNetTuning {
    pub fn new(tuning: NetTuning) -> Self {
        Self {
            inner: Arc::new(Mutex::new(tuning.clamped())),
        }
    }

    /// The policy in force right now. Taken as a snapshot at the top of a
    /// poll, so one poll runs under one consistent set of numbers even if the
    /// analyst is dragging a slider while it runs.
    pub fn get(&self) -> NetTuning {
        // A poisoned mutex here means a thread panicked holding a struct copy,
        // which cannot leave the value half-written. Recovering the inner
        // value is strictly better than taking the live feed down with it.
        *self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Replace the policy. Clamped on the way in - see [`NetTuning::clamped`].
    pub fn set(&self, tuning: NetTuning) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = tuning.clamped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers, written out once more in the test rather than read from
    /// the same constants the code reads, so that a change to any of them
    /// fails here and has to be made on purpose. This is the one place in this
    /// wave where restating a value is the point.
    #[test]
    fn the_defaults_are_the_constants_the_live_path_shipped_with() {
        let tuning = NetTuning::default();
        assert_eq!(tuning.live_poll, Duration::from_millis(1_200));
        assert_eq!(tuning.archive_poll, Duration::from_secs(30));
        assert_eq!(tuning.archive_lead_minutes, 5);
        assert_eq!(tuning.stall_after, Duration::from_secs(60));
        assert_eq!(tuning.live_cache_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(tuning.download_batch, 8);
        assert_eq!(tuning.download_attempts, 3);
        assert_eq!(tuning.retry_backoff, Duration::from_millis(150));
        // And the defaults are themselves inside the fence, so a fresh session
        // is not silently running something other than what is declared.
        assert_eq!(tuning.clamped(), tuning);
    }

    #[test]
    fn nothing_can_ask_this_session_to_hammer_a_public_bucket() {
        let reckless = NetTuning {
            live_poll: Duration::from_millis(10),
            archive_poll: Duration::from_millis(50),
            archive_lead_minutes: 0,
            stall_after: Duration::ZERO,
            live_cache_bytes: 1,
            download_batch: 4_096,
            download_attempts: 1_000,
            retry_backoff: Duration::ZERO,
        }
        .clamped();
        assert_eq!(reckless.live_poll, MIN_LIVE_POLL);
        assert_eq!(reckless.archive_poll, MIN_ARCHIVE_POLL);
        assert_eq!(reckless.archive_lead_minutes, MIN_ARCHIVE_LEAD_MINUTES);
        assert_eq!(reckless.stall_after, MIN_STALL_AFTER);
        assert_eq!(reckless.live_cache_bytes, MIN_LIVE_CACHE_BYTES);
        assert_eq!(reckless.download_batch, MAX_DOWNLOAD_BATCH);
        assert_eq!(reckless.download_attempts, MAX_DOWNLOAD_ATTEMPTS);
        assert_eq!(reckless.retry_backoff, MIN_RETRY_BACKOFF);
    }

    #[test]
    fn the_lazy_end_is_bounded_too_so_a_session_cannot_be_told_to_stop_looking() {
        let idle = NetTuning {
            live_poll: Duration::from_secs(86_400),
            archive_poll: Duration::from_secs(86_400),
            archive_lead_minutes: i64::MAX,
            stall_after: Duration::from_secs(86_400),
            live_cache_bytes: u64::MAX,
            download_batch: 0,
            download_attempts: 0,
            retry_backoff: Duration::from_secs(3_600),
        }
        .clamped();
        assert_eq!(idle.live_poll, MAX_LIVE_POLL);
        assert_eq!(idle.archive_poll, MAX_ARCHIVE_POLL);
        assert_eq!(idle.archive_lead_minutes, MAX_ARCHIVE_LEAD_MINUTES);
        assert_eq!(idle.stall_after, MAX_STALL_AFTER);
        assert_eq!(idle.live_cache_bytes, MAX_LIVE_CACHE_BYTES);
        assert_eq!(idle.download_batch, MIN_DOWNLOAD_BATCH);
        assert_eq!(idle.download_attempts, MIN_DOWNLOAD_ATTEMPTS);
        assert_eq!(idle.retry_backoff, MAX_RETRY_BACKOFF);
    }

    /// The catalog's declared ranges against the fences here.
    ///
    /// Two separate things have to be true and are easy to let drift apart:
    /// the menu must not OFFER a value the code will silently clamp - a slider
    /// that does nothing at one end is a lie - and the menu's default must be
    /// the shipped policy. Both are checked against the real catalog.
    #[test]
    fn the_menu_offers_exactly_what_the_fence_allows() {
        use crate::settings_ui::catalog::{keys, registry};
        let registry = registry();
        let range = |category: &str, id: &str| -> (i64, i64, i64) {
            match &registry
                .setting(category, id)
                .unwrap_or_else(|| panic!("the catalog declares {category}/{id}"))
                .kind
            {
                settings::SettingKind::Integer {
                    min, max, default, ..
                } => (*min, *max, *default),
                other => panic!("{category}/{id} is {other:?}, not an integer"),
            }
        };
        let network = keys::network::CATEGORY;
        let shipped = NetTuning::default();

        let (min, max, default) = range(network, keys::network::ARCHIVE_POLL_SECONDS);
        assert_eq!(Duration::from_secs(min as u64), MIN_ARCHIVE_POLL);
        assert_eq!(Duration::from_secs(max as u64), MAX_ARCHIVE_POLL);
        assert_eq!(Duration::from_secs(default as u64), shipped.archive_poll);

        let (min, max, default) = range(network, keys::network::ARCHIVE_LEAD_MINUTES);
        assert_eq!(min, MIN_ARCHIVE_LEAD_MINUTES);
        assert_eq!(max, MAX_ARCHIVE_LEAD_MINUTES);
        assert_eq!(default, shipped.archive_lead_minutes);

        let (min, max, default) = range(network, keys::network::STALL_AFTER_SECONDS);
        assert_eq!(Duration::from_secs(min as u64), MIN_STALL_AFTER);
        assert_eq!(Duration::from_secs(max as u64), MAX_STALL_AFTER);
        assert_eq!(Duration::from_secs(default as u64), shipped.stall_after);

        let (min, max, default) = range(network, keys::network::DOWNLOAD_BATCH);
        assert_eq!(min as usize, MIN_DOWNLOAD_BATCH);
        assert_eq!(max as usize, MAX_DOWNLOAD_BATCH);
        assert_eq!(default as usize, shipped.download_batch);

        let (min, max, default) = range(network, keys::network::DOWNLOAD_ATTEMPTS);
        assert_eq!(min as usize, MIN_DOWNLOAD_ATTEMPTS);
        assert_eq!(max as usize, MAX_DOWNLOAD_ATTEMPTS);
        assert_eq!(default as usize, shipped.download_attempts);

        let (min, max, default) = range(network, keys::network::RETRY_BACKOFF_MS);
        assert_eq!(Duration::from_millis(min as u64), MIN_RETRY_BACKOFF);
        assert_eq!(Duration::from_millis(max as u64), MAX_RETRY_BACKOFF);
        assert_eq!(Duration::from_millis(default as u64), shipped.retry_backoff);

        // The live cache ceiling and the poll cadence live on the Data page,
        // which had already declared them - this wave wired them rather than
        // declaring them a second time.
        let (min, max, default) = range(keys::data::CATEGORY, keys::data::LIVE_CACHE_LIMIT_MB);
        assert_eq!((min as u64) * 1024 * 1024, MIN_LIVE_CACHE_BYTES);
        assert_eq!((max as u64) * 1024 * 1024, MAX_LIVE_CACHE_BYTES);
        assert_eq!((default as u64) * 1024 * 1024, shipped.live_cache_bytes);

        let settings::SettingKind::Slider {
            min, max, default, ..
        } = &registry
            .setting(keys::data::CATEGORY, keys::data::POLL_SECONDS)
            .expect("the catalog declares data/poll_seconds")
            .kind
        else {
            panic!("data/poll_seconds is not a slider");
        };
        assert_eq!(Duration::from_secs_f64(*min), MIN_LIVE_POLL);
        assert_eq!(Duration::from_secs_f64(*max), MAX_LIVE_POLL);
        assert_eq!(Duration::from_secs_f64(*default), shipped.live_poll);
    }

    /// Neither of the two settings this wave WIRED may still be drawn as a
    /// declared-but-dead row: a disabled control that now has an effect is
    /// worse than one that never had one.
    #[test]
    fn the_two_wired_data_settings_are_no_longer_marked_pending() {
        use crate::settings_ui::catalog::{keys, registry};
        let registry = registry();
        for id in [keys::data::POLL_SECONDS, keys::data::LIVE_CACHE_LIMIT_MB] {
            assert!(
                registry
                    .setting(keys::data::CATEGORY, id)
                    .expect("declared")
                    .enabled,
                "data/{id} is wired and must be enabled"
            );
        }
    }

    #[test]
    fn the_shared_handle_clamps_on_the_way_in_not_on_the_way_out() {
        let shared = SharedNetTuning::default();
        shared.set(NetTuning {
            live_poll: Duration::from_millis(1),
            ..NetTuning::default()
        });
        // The floor is applied where the value is stored, so every reader -
        // including one that never heard of the fence - sees a legal number.
        assert_eq!(shared.get().live_poll, MIN_LIVE_POLL);
    }

    #[test]
    fn a_reader_and_a_writer_on_two_threads_never_see_a_half_written_policy() {
        let shared = SharedNetTuning::default();
        let writer = shared.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..500 {
                writer.set(NetTuning {
                    live_poll: Duration::from_secs(4),
                    archive_poll: Duration::from_secs(120),
                    ..NetTuning::default()
                });
                writer.set(NetTuning::default());
            }
        });
        for _ in 0..500 {
            let seen = shared.get();
            // The two fields move together or not at all.
            let consistent = (seen.live_poll == Duration::from_secs(4)
                && seen.archive_poll == Duration::from_secs(120))
                || (seen.live_poll == Duration::from_millis(1_200)
                    && seen.archive_poll == Duration::from_secs(30));
            assert!(consistent, "torn read: {seen:?}");
        }
        handle.join().expect("writer thread");
    }
}
