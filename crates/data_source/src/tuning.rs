//! Transfer policy for the bucket calls: batch size, retries, backoff.
//!
//! These three numbers used to be `const`s beside the functions that read
//! them. They are read from inside free functions that already carry a site, a
//! volume and a cancellation flag through several layers - `latest_realtime_
//! level2_volume`, `append_realtime_chunks`, `download_s3_object_to_path` -
//! and none of those signatures has anywhere to put a config without every
//! caller in the workspace learning about one. So the policy lives here, in
//! three atomics, set once by whoever composes the application.
//!
//! That is a process-global, which is a thing to justify rather than
//! apologise for:
//!
//! * There is exactly one data layer per process and exactly one analyst
//!   turning the knob. Two different retry policies in one run would be a bug,
//!   not a feature, so a per-call parameter would be modelling a distinction
//!   that does not exist.
//! * Each value is read independently, at the top of the operation it governs,
//!   and none of them has to agree with any other. There is no torn-read
//!   hazard to protect against, which is why this is three atomics rather than
//!   a mutex around a struct - unlike `workstation_app::net_tuning`, whose
//!   fields do have to move together.
//! * The floors are enforced HERE, in [`set_transfer_tuning`], rather than at
//!   the call site. A caller cannot reach around them, and a build that
//!   forgets to call the setter at all gets the shipped defaults.
//!
//! Defaults are the constants this module replaced, so a process that never
//! calls the setter makes exactly the requests it always made.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// How many chunk objects one scoped batch downloads at a time.
pub const DEFAULT_CHUNK_DOWNLOAD_BATCH: usize = 8;
/// Total attempts per S3 object, retries included.
pub const DEFAULT_DOWNLOAD_ATTEMPTS: usize = 3;
/// The pause between attempts.
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(150);

/// The fences. More parallelism against one bucket prefix is a way to get
/// throttled rather than a way to go faster; a retry loop with no pause is a
/// way to become a nuisance.
pub const MIN_CHUNK_DOWNLOAD_BATCH: usize = 1;
pub const MAX_CHUNK_DOWNLOAD_BATCH: usize = 16;
pub const MIN_DOWNLOAD_ATTEMPTS: usize = 1;
pub const MAX_DOWNLOAD_ATTEMPTS: usize = 6;
pub const MIN_RETRY_BACKOFF: Duration = Duration::from_millis(100);
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);

static CHUNK_DOWNLOAD_BATCH: AtomicUsize = AtomicUsize::new(DEFAULT_CHUNK_DOWNLOAD_BATCH);
static DOWNLOAD_ATTEMPTS: AtomicUsize = AtomicUsize::new(DEFAULT_DOWNLOAD_ATTEMPTS);
static RETRY_BACKOFF_MILLIS: AtomicU64 = AtomicU64::new(150);

/// Install a transfer policy for this process. Every value is clamped into its
/// fence here, so nothing downstream has to re-check and nothing upstream can
/// reach around it.
pub fn set_transfer_tuning(batch: usize, attempts: usize, retry_backoff: Duration) {
    CHUNK_DOWNLOAD_BATCH.store(
        batch.clamp(MIN_CHUNK_DOWNLOAD_BATCH, MAX_CHUNK_DOWNLOAD_BATCH),
        Ordering::Relaxed,
    );
    DOWNLOAD_ATTEMPTS.store(
        attempts.clamp(MIN_DOWNLOAD_ATTEMPTS, MAX_DOWNLOAD_ATTEMPTS),
        Ordering::Relaxed,
    );
    RETRY_BACKOFF_MILLIS.store(
        retry_backoff
            .clamp(MIN_RETRY_BACKOFF, MAX_RETRY_BACKOFF)
            .as_millis()
            // Every legal backoff is well under `u64::MAX` milliseconds; the
            // saturating cast is here so the conversion cannot be the thing
            // that fails if the fence is ever widened.
            .min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}

/// Restore the shipped policy. Used by the tests here, and by anything that
/// wants a known starting point.
pub fn reset_transfer_tuning() {
    set_transfer_tuning(
        DEFAULT_CHUNK_DOWNLOAD_BATCH,
        DEFAULT_DOWNLOAD_ATTEMPTS,
        DEFAULT_RETRY_BACKOFF,
    );
}

pub fn chunk_download_batch() -> usize {
    // `max(1)` so a chunked iteration can never be handed a zero width, which
    // panics. The setter's floor already guarantees it; this is the belt that
    // makes the guarantee local to the reader.
    CHUNK_DOWNLOAD_BATCH.load(Ordering::Relaxed).max(1)
}

pub fn download_attempts() -> usize {
    DOWNLOAD_ATTEMPTS.load(Ordering::Relaxed).max(1)
}

pub fn retry_backoff() -> Duration {
    Duration::from_millis(RETRY_BACKOFF_MILLIS.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The atomics are process-global, so the tests that write them run as one
    /// test rather than as several racing ones.
    #[test]
    fn the_transfer_policy_defaults_to_the_constants_it_replaced_and_clamps_everything_else() {
        reset_transfer_tuning();
        assert_eq!(chunk_download_batch(), DEFAULT_CHUNK_DOWNLOAD_BATCH);
        assert_eq!(download_attempts(), DEFAULT_DOWNLOAD_ATTEMPTS);
        assert_eq!(retry_backoff(), DEFAULT_RETRY_BACKOFF);

        // Nothing can ask for unbounded parallelism or a pauseless retry loop.
        set_transfer_tuning(4_096, 1_000, Duration::ZERO);
        assert_eq!(chunk_download_batch(), MAX_CHUNK_DOWNLOAD_BATCH);
        assert_eq!(download_attempts(), MAX_DOWNLOAD_ATTEMPTS);
        assert_eq!(retry_backoff(), MIN_RETRY_BACKOFF);

        // And a zero batch cannot reach `slice::chunks`, which would panic.
        set_transfer_tuning(0, 0, Duration::from_secs(3_600));
        assert_eq!(chunk_download_batch(), MIN_CHUNK_DOWNLOAD_BATCH);
        assert_eq!(download_attempts(), MIN_DOWNLOAD_ATTEMPTS);
        assert_eq!(retry_backoff(), MAX_RETRY_BACKOFF);

        // A legal policy survives untouched.
        set_transfer_tuning(2, 5, Duration::from_millis(400));
        assert_eq!(chunk_download_batch(), 2);
        assert_eq!(download_attempts(), 5);
        assert_eq!(retry_backoff(), Duration::from_millis(400));

        reset_transfer_tuning();
    }
}
