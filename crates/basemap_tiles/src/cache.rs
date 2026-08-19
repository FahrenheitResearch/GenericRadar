//! The on-disk tile cache.
//!
//! The cache is not an optimisation here, it is how the rate limit is
//! honoured: [`crate::TileProvider::min_cache_seconds`] is enforced by this
//! module refusing to go to the network for a tile it already holds. A build
//! that quietly loses its cache directory starts hammering the provider.
//!
//! # Entry format
//!
//! One file per tile, at `<root>/<provider key>/<z>/<x>_<y>.tile`, with a
//! fixed 15-byte header ahead of the untouched encoded body:
//!
//! ```text
//! 0..4    magic, b"RWT1"
//! 4       format version, 1
//! 5..7    ETag length, u16 little-endian
//! 7..15   fetch time, u64 little-endian, seconds since the Unix epoch
//! 15..    ETag bytes, then the encoded image body verbatim
//! ```
//!
//! The ETag rides with the body rather than in a sidecar because two files
//! that must agree are two files that eventually will not. Keeping the body
//! byte-exact matters too: it is what lets `If-None-Match` revalidation work,
//! and it means the cache never re-encodes anybody's imagery.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{MAX_TILE_ENCODED_BYTES, TileId, TileProvider};

const MAGIC: [u8; 4] = *b"RWT1";
const FORMAT_VERSION: u8 = 1;
const HEADER_BYTES: usize = 15;
/// ETags are short; this bounds a corrupt length field before it is trusted.
const MAX_ETAG_BYTES: usize = 512;
const MAX_ENTRY_BYTES: u64 = (MAX_TILE_ENCODED_BYTES + HEADER_BYTES + MAX_ETAG_BYTES) as u64;

/// Bytes written since the last sweep that trigger the next one. A sweep walks
/// the whole cache directory, so it must not run per tile; 16 MiB is roughly
/// every five hundred tiles.
const SWEEP_INTERVAL_BYTES: u64 = 16 * 1024 * 1024;

/// A sweep prunes down to this fraction of the budget rather than exactly to
/// it, so the next write does not immediately trigger another sweep.
const PRUNE_TARGET_FRACTION: f64 = 0.9;

/// One entry read back off disk.
#[derive(Clone, Debug)]
pub(crate) struct CachedTile {
    pub(crate) etag: Option<String>,
    /// Seconds since the Unix epoch at which this body was fetched.
    pub(crate) fetched_at_unix: u64,
    pub(crate) body: Vec<u8>,
}

impl CachedTile {
    /// Whether this entry is still inside the provider's minimum cache
    /// lifetime, i.e. whether the network may be touched for it at all.
    pub(crate) fn is_fresh(&self, min_cache_seconds: u64, now_unix: u64) -> bool {
        now_unix.saturating_sub(self.fetched_at_unix) < min_cache_seconds
    }
}

/// What a sweep did. Returned so a test can assert the bound rather than
/// trusting a comment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SweepReport {
    pub(crate) files_before: u64,
    pub(crate) bytes_before: u64,
    pub(crate) files_removed: u64,
    pub(crate) bytes_after: u64,
}

pub(crate) struct TileDiskCache {
    root: PathBuf,
    max_bytes: u64,
    bytes: AtomicU64,
    unswept_bytes: AtomicU64,
    scanned: AtomicBool,
    writable: AtomicBool,
    sweeping: Mutex<()>,
}

impl TileDiskCache {
    pub(crate) fn new(root: PathBuf, max_bytes: u64) -> Self {
        let writable = probe_writable(&root);
        Self {
            root,
            // A zero budget would mean "delete everything you just wrote", so
            // it is treated as a configuration mistake and floored.
            max_bytes: max_bytes.max(MAX_ENTRY_BYTES),
            bytes: AtomicU64::new(0),
            unswept_bytes: AtomicU64::new(0),
            scanned: AtomicBool::new(false),
            writable: AtomicBool::new(writable),
            sweeping: Mutex::new(()),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Whether tiles can actually be *kept* here.
    ///
    /// Measured, not assumed: the directory is created and a probe file
    /// written and removed when the cache is constructed, and a later write
    /// failure (a full disk, a revoked permission) clears the flag. It matters
    /// because a provider whose terms require a minimum cache lifetime cannot
    /// lawfully be served at all from a cache that silently drops everything
    /// written to it — see `TileStore::permits`. Reads are still attempted
    /// either way, so a read-only cache directory still serves an offline
    /// session.
    pub(crate) fn is_writable(&self) -> bool {
        self.writable.load(Ordering::Relaxed)
    }

    /// Best-known total size of the cache on disk. Exact after a sweep, and an
    /// over-estimate at worst between sweeps.
    pub(crate) fn disk_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn path_for(&self, provider: TileProvider, tile: TileId) -> PathBuf {
        self.root
            .join(provider.key())
            .join(tile.z.to_string())
            .join(format!("{}_{}.tile", tile.x, tile.y))
    }

    /// Read an entry back. A file that fails any check is deleted rather than
    /// left as a permanent retry sink: a truncated write or a half-synced
    /// filesystem must not poison a tile forever.
    pub(crate) fn load(&self, provider: TileProvider, tile: TileId) -> Option<CachedTile> {
        // The first read is also when the cache is first measured. This runs
        // on a worker thread, never on the UI thread, and is a no-op after the
        // first sweep - so a store that only ever reads still reports an
        // honest `disk_bytes` and still enforces its budget.
        self.sweep_if_due();
        let path = self.path_for(provider, tile);
        match read_entry(&path) {
            Ok(entry) => Some(entry),
            Err(EntryError::Missing) => None,
            Err(_) => {
                // Ignore removal failures: a read-only cache, an antivirus
                // race, or another process having already replaced the file
                // are all recoverable by simply refetching.
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Write an entry. `body` must already have decoded successfully — this
    /// function does not validate it, and nothing else stands between a 404
    /// error page and a permanently cached hole.
    pub(crate) fn store(
        &self,
        provider: TileProvider,
        tile: TileId,
        etag: Option<&str>,
        body: &[u8],
        fetched_at_unix: u64,
    ) -> std::io::Result<u64> {
        let path = self.path_for(provider, tile);
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            self.writable.store(false, Ordering::Relaxed);
            return Err(error);
        }
        let etag_bytes = etag.map(str::as_bytes).unwrap_or_default();
        let etag_bytes = &etag_bytes[..etag_bytes.len().min(MAX_ETAG_BYTES)];

        let mut buffer = Vec::with_capacity(HEADER_BYTES + etag_bytes.len() + body.len());
        buffer.extend_from_slice(&MAGIC);
        buffer.push(FORMAT_VERSION);
        buffer.extend_from_slice(&(etag_bytes.len() as u16).to_le_bytes());
        buffer.extend_from_slice(&fetched_at_unix.to_le_bytes());
        buffer.extend_from_slice(etag_bytes);
        buffer.extend_from_slice(body);

        // Write beside the target and rename, so a reader never sees a
        // half-written entry and a crash mid-write leaves the old one intact.
        let temporary = path.with_extension(format!("tmp{}", unique_suffix()));
        if let Err(error) = write_all_to(&temporary, &buffer) {
            let _ = fs::remove_file(&temporary);
            self.writable.store(false, Ordering::Relaxed);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            self.writable.store(false, Ordering::Relaxed);
            return Err(error);
        }
        self.writable.store(true, Ordering::Relaxed);

        let written = buffer.len() as u64;
        self.bytes.fetch_add(written, Ordering::Relaxed);
        self.unswept_bytes.fetch_add(written, Ordering::Relaxed);
        self.sweep_if_due();
        Ok(written)
    }

    /// Update an entry's fetch time in place after a 304, without rewriting
    /// the body. Eight bytes at a fixed offset.
    pub(crate) fn touch(
        &self,
        provider: TileProvider,
        tile: TileId,
        fetched_at_unix: u64,
    ) -> std::io::Result<()> {
        let path = self.path_for(provider, tile);
        let mut file = fs::OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(7))?;
        file.write_all(&fetched_at_unix.to_le_bytes())?;
        file.flush()
    }

    /// Delete every cached body. Used when the user clears the cache.
    pub(crate) fn clear(&self) -> std::io::Result<()> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        self.bytes.store(0, Ordering::Relaxed);
        self.unswept_bytes.store(0, Ordering::Relaxed);
        self.scanned.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Sweep if enough has been written since the last one, or if the cache
    /// has never been measured. Cheap and non-blocking when another thread is
    /// already sweeping.
    fn sweep_if_due(&self) {
        let never_measured = !self.scanned.load(Ordering::Relaxed);
        let unswept = self.unswept_bytes.load(Ordering::Relaxed);
        let over_budget = self.bytes.load(Ordering::Relaxed) > self.max_bytes;
        if !never_measured && unswept < SWEEP_INTERVAL_BYTES && !over_budget {
            return;
        }
        let _guard = match self.sweeping.try_lock() {
            Ok(guard) => guard,
            // Another worker is already doing exactly this.
            Err(std::sync::TryLockError::WouldBlock) => return,
            // A previous sweep panicked and poisoned the lock. Sweeping anyway
            // is strictly better than the alternative: `try_lock` would refuse
            // for the rest of the session and the cache would then grow
            // without any bound at all. Nothing here is left half-written by a
            // panic — the sweep only reads directory entries and deletes
            // files.
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        self.unswept_bytes.store(0, Ordering::Relaxed);
        self.scanned.store(true, Ordering::Relaxed);
        let _ = self.sweep();
    }

    /// Walk the cache, total it, and delete oldest-first until it fits.
    ///
    /// Oldest by modification time, which `store` and `touch` both refresh, so
    /// a tile that is being used survives and a tile nobody has looked at
    /// since last month is the one that goes.
    pub(crate) fn sweep(&self) -> SweepReport {
        let mut entries = Vec::new();
        collect_entries(&self.root, &mut entries);
        let bytes_before: u64 = entries.iter().map(|entry| entry.size).sum();
        let files_before = entries.len() as u64;

        if bytes_before <= self.max_bytes {
            self.bytes.store(bytes_before, Ordering::Relaxed);
            return SweepReport {
                files_before,
                bytes_before,
                files_removed: 0,
                bytes_after: bytes_before,
            };
        }

        entries.sort_by_key(|entry| entry.modified_unix);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target = (self.max_bytes as f64 * PRUNE_TARGET_FRACTION) as u64;
        let mut total = bytes_before;
        let mut removed = 0_u64;
        for entry in &entries {
            if total <= target {
                break;
            }
            if fs::remove_file(&entry.path).is_ok() {
                total = total.saturating_sub(entry.size);
                removed += 1;
            }
        }
        self.bytes.store(total, Ordering::Relaxed);
        SweepReport {
            files_before,
            bytes_before,
            files_removed: removed,
            bytes_after: total,
        }
    }
}

struct CacheEntryStat {
    path: PathBuf,
    size: u64,
    modified_unix: u64,
}

fn collect_entries(directory: &Path, out: &mut Vec<CacheEntryStat>) {
    let Ok(listing) = fs::read_dir(directory) else {
        return;
    };
    for entry in listing.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_entries(&path, out);
            continue;
        }
        // Abandoned temporaries from an interrupted write are swept too, and
        // they are the oldest thing in the directory by construction.
        let modified_unix = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        out.push(CacheEntryStat {
            path,
            size: metadata.len(),
            modified_unix,
        });
    }
}

#[derive(Debug)]
enum EntryError {
    Missing,
    Unreadable,
    Malformed,
}

/// Read and validate one entry file, with every length bounded before it is
/// used to allocate.
fn read_entry(path: &Path) -> Result<CachedTile, EntryError> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(EntryError::Missing);
        }
        Err(_) => return Err(EntryError::Unreadable),
    };
    let length = file.metadata().map_err(|_| EntryError::Unreadable)?.len();
    if length < HEADER_BYTES as u64 || length > MAX_ENTRY_BYTES {
        return Err(EntryError::Malformed);
    }

    let mut header = [0u8; HEADER_BYTES];
    file.read_exact(&mut header)
        .map_err(|_| EntryError::Malformed)?;
    if header[..4] != MAGIC || header[4] != FORMAT_VERSION {
        return Err(EntryError::Malformed);
    }
    let etag_len = u16::from_le_bytes([header[5], header[6]]) as usize;
    if etag_len > MAX_ETAG_BYTES {
        return Err(EntryError::Malformed);
    }
    let fetched_at_unix = u64::from_le_bytes([
        header[7], header[8], header[9], header[10], header[11], header[12], header[13], header[14],
    ]);

    let remaining = length - HEADER_BYTES as u64;
    if (etag_len as u64) > remaining {
        return Err(EntryError::Malformed);
    }
    let mut etag_bytes = vec![0u8; etag_len];
    file.read_exact(&mut etag_bytes)
        .map_err(|_| EntryError::Malformed)?;
    let etag = if etag_len == 0 {
        None
    } else {
        Some(String::from_utf8(etag_bytes).map_err(|_| EntryError::Malformed)?)
    };

    let body_len = (remaining - etag_len as u64) as usize;
    if body_len == 0 || body_len > MAX_TILE_ENCODED_BYTES {
        return Err(EntryError::Malformed);
    }
    let mut body = Vec::new();
    body.try_reserve_exact(body_len)
        .map_err(|_| EntryError::Malformed)?;
    body.resize(body_len, 0);
    file.read_exact(&mut body)
        .map_err(|_| EntryError::Malformed)?;

    Ok(CachedTile {
        etag,
        fetched_at_unix,
        body,
    })
}

fn write_all_to(path: &Path, buffer: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(buffer)?;
    file.flush()
}

/// Create the cache directory and prove a file can be written in it.
///
/// Done once, when the cache is constructed, because "the disk cache exists"
/// and "the disk cache works" are different claims and only the second one
/// satisfies a provider that requires a minimum cache lifetime. A directory
/// that cannot be created, or that refuses a write, must not be reported as a
/// working cache.
fn probe_writable(root: &Path) -> bool {
    if fs::create_dir_all(root).is_err() {
        return false;
    }
    let probe = root.join(format!(".writable-{}", unique_suffix()));
    if write_all_to(&probe, b"radar-workstation basemap tile cache").is_err() {
        let _ = fs::remove_file(&probe);
        return false;
    }
    let _ = fs::remove_file(&probe);
    true
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn unique_suffix() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "basemap-tiles-{label}-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp cache root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tile(index: u32) -> TileId {
        TileId::new(9, 100 + index, 200).expect("valid")
    }

    #[test]
    fn a_stored_entry_reads_back_byte_for_byte() {
        let root = TempRoot::new("roundtrip");
        let cache = TileDiskCache::new(root.0.clone(), 8 * 1024 * 1024);
        let body: Vec<u8> = (0..4_096).map(|index| (index % 251) as u8).collect();

        cache
            .store(
                TileProvider::UsgsImagery,
                tile(0),
                Some("\"197d318d730\""),
                &body,
                1_700_000_000,
            )
            .expect("store");

        let loaded = cache
            .load(TileProvider::UsgsImagery, tile(0))
            .expect("entry present");
        assert_eq!(loaded.body, body, "the body must survive verbatim");
        assert_eq!(loaded.etag.as_deref(), Some("\"197d318d730\""));
        assert_eq!(loaded.fetched_at_unix, 1_700_000_000);

        // A different provider is a different cache namespace.
        assert!(cache.load(TileProvider::UsgsTopo, tile(0)).is_none());
        assert!(cache.load(TileProvider::UsgsImagery, tile(1)).is_none());
    }

    #[test]
    fn an_entry_without_an_etag_round_trips_too() {
        let root = TempRoot::new("no-etag");
        let cache = TileDiskCache::new(root.0.clone(), 8 * 1024 * 1024);
        cache
            .store(TileProvider::OpenStreetMap, tile(0), None, b"body", 42)
            .expect("store");
        let loaded = cache
            .load(TileProvider::OpenStreetMap, tile(0))
            .expect("present");
        assert_eq!(loaded.etag, None);
        assert_eq!(loaded.body, b"body");
    }

    #[test]
    fn freshness_follows_the_providers_minimum_cache_lifetime() {
        let entry = CachedTile {
            etag: None,
            fetched_at_unix: 1_000,
            body: vec![1],
        };
        assert!(entry.is_fresh(86_400, 1_000));
        assert!(entry.is_fresh(86_400, 87_399));
        assert!(!entry.is_fresh(86_400, 87_400));
        // A clock that has gone backwards must not read as stale-forever.
        assert!(entry.is_fresh(86_400, 500));
    }

    #[test]
    fn touch_updates_the_timestamp_without_disturbing_the_body() {
        let root = TempRoot::new("touch");
        let cache = TileDiskCache::new(root.0.clone(), 8 * 1024 * 1024);
        let body = vec![7u8; 1_024];
        cache
            .store(TileProvider::UsgsTopo, tile(0), Some("\"abc\""), &body, 100)
            .expect("store");
        cache
            .touch(TileProvider::UsgsTopo, tile(0), 999_999)
            .expect("touch");

        let loaded = cache
            .load(TileProvider::UsgsTopo, tile(0))
            .expect("present");
        assert_eq!(loaded.fetched_at_unix, 999_999);
        assert_eq!(loaded.body, body);
        assert_eq!(loaded.etag.as_deref(), Some("\"abc\""));
    }

    /// Every way a cache file can be wrong, and the one behaviour that matters
    /// for all of them: `load` returns `None` and the file is gone, so the
    /// next attempt refetches instead of failing forever.
    #[test]
    fn corrupt_entries_are_deleted_rather_than_becoming_permanent() {
        let root = TempRoot::new("corrupt");
        let cache = TileDiskCache::new(root.0.clone(), 8 * 1024 * 1024);

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("short", b"RWT1".to_vec()),
            ("bad magic", {
                let mut bytes = vec![0u8; HEADER_BYTES + 16];
                bytes[..4].copy_from_slice(b"XXXX");
                bytes
            }),
            ("bad version", {
                let mut bytes = vec![0u8; HEADER_BYTES + 16];
                bytes[..4].copy_from_slice(&MAGIC);
                bytes[4] = 99;
                bytes
            }),
            ("etag longer than the file", {
                let mut bytes = vec![0u8; HEADER_BYTES + 2];
                bytes[..4].copy_from_slice(&MAGIC);
                bytes[4] = FORMAT_VERSION;
                bytes[5..7].copy_from_slice(&400u16.to_le_bytes());
                bytes
            }),
            ("header only, no body", {
                let mut bytes = vec![0u8; HEADER_BYTES];
                bytes[..4].copy_from_slice(&MAGIC);
                bytes[4] = FORMAT_VERSION;
                bytes
            }),
        ];

        for (label, contents) in cases {
            let path = cache.path_for(TileProvider::UsgsImagery, tile(0));
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(&path, &contents).expect("write fixture");
            assert!(
                cache.load(TileProvider::UsgsImagery, tile(0)).is_none(),
                "{label} should not load"
            );
            assert!(!path.exists(), "{label} should have been deleted");
        }
    }

    /// An oversized file must be rejected from its metadata, without its body
    /// ever being read into memory.
    #[test]
    fn an_oversized_entry_is_rejected_and_removed() {
        let root = TempRoot::new("oversized");
        let cache = TileDiskCache::new(root.0.clone(), 8 * 1024 * 1024);
        let path = cache.path_for(TileProvider::UsgsImagery, tile(0));
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let file = fs::File::create(&path).expect("create");
        file.set_len(MAX_ENTRY_BYTES + 1).expect("size");
        drop(file);

        assert!(cache.load(TileProvider::UsgsImagery, tile(0)).is_none());
        assert!(!path.exists());
    }

    /// The bound that keeps a long session from filling a disk. Written
    /// against a deliberately small budget so the sweep is forced many times.
    #[test]
    fn the_cache_stays_inside_its_budget() {
        let root = TempRoot::new("bounded");
        let budget = MAX_ENTRY_BYTES + 512 * 1024;
        let cache = TileDiskCache::new(root.0.clone(), budget);
        let body = vec![0xAB_u8; 32 * 1024];

        for index in 0..200u32 {
            cache
                .store(
                    TileProvider::UsgsImagery,
                    TileId::new(12, 1_000 + index, 2_000).expect("valid"),
                    Some("\"e\""),
                    &body,
                    1_700_000_000 + u64::from(index),
                )
                .expect("store");
        }

        let report = cache.sweep();
        let mut on_disk = Vec::new();
        collect_entries(&root.0, &mut on_disk);
        let actual: u64 = on_disk.iter().map(|entry| entry.size).sum();

        assert!(
            actual <= budget,
            "cache grew to {actual} bytes against a {budget} byte budget"
        );
        assert_eq!(actual, report.bytes_after);
        assert_eq!(actual, cache.disk_bytes());
        // 200 x 32 KiB is 6.4 MiB against a ~4.5 MiB budget, so real eviction
        // must have happened rather than the test passing by never filling up.
        assert!(
            on_disk.len() < 200,
            "nothing was evicted; the test did not exercise the bound"
        );
        assert!(!on_disk.is_empty(), "the sweep emptied the whole cache");
    }

    /// Eviction must take the least recently written first, so the tiles the
    /// user is looking at now survive.
    #[test]
    fn the_sweep_evicts_oldest_first() {
        let root = TempRoot::new("lru");
        // Fill with a budget large enough that no sweep runs during the writes,
        // so the ordering below is about `sweep` and nothing else.
        let filling = TileDiskCache::new(root.0.clone(), 1024 * 1024 * 1024);
        let body = vec![0u8; 64 * 1024];

        let mut paths = Vec::new();
        for index in 0..100u32 {
            let id = TileId::new(12, 3_000 + index, 500).expect("valid");
            filling
                .store(TileProvider::UsgsTopo, id, None, &body, 0)
                .expect("store");
            let path = filling.path_for(TileProvider::UsgsTopo, id);
            // Backdate each file so modification order is unambiguous on a
            // filesystem with coarse timestamps.
            set_modified(&path, 1_700_000_000 + u64::from(index));
            paths.push(path);
        }

        // 100 x 64 KiB is 6.4 MiB, against a budget of about 4.3 MiB.
        let budget = MAX_ENTRY_BYTES + 128 * 1024;
        let cache = TileDiskCache::new(root.0.clone(), budget);
        let report = cache.sweep();
        assert!(report.files_removed > 0, "nothing was evicted");
        let survivors: Vec<usize> = paths
            .iter()
            .enumerate()
            .filter(|(_, path)| path.exists())
            .map(|(index, _)| index)
            .collect();
        assert!(!survivors.is_empty() && survivors.len() < paths.len());
        let first_survivor = survivors[0];
        assert!(
            paths[..first_survivor].iter().all(|path| !path.exists()),
            "an older file outlived a newer one"
        );
        assert!(
            paths[first_survivor..].iter().all(|path| path.exists()),
            "eviction was not contiguous from the oldest end"
        );
    }

    #[test]
    fn clearing_removes_everything_and_resets_the_counter() {
        let root = TempRoot::new("clear");
        let cache = TileDiskCache::new(root.0.clone(), 8 * 1024 * 1024);
        cache
            .store(TileProvider::UsgsImagery, tile(0), None, &[1, 2, 3], 0)
            .expect("store");
        assert!(cache.disk_bytes() > 0);
        cache.clear().expect("clear");
        assert_eq!(cache.disk_bytes(), 0);
        assert!(cache.load(TileProvider::UsgsImagery, tile(0)).is_none());
        // And the cache is usable again immediately afterwards.
        cache
            .store(TileProvider::UsgsImagery, tile(0), None, &[4, 5, 6], 0)
            .expect("store after clear");
        assert_eq!(
            cache
                .load(TileProvider::UsgsImagery, tile(0))
                .expect("present")
                .body,
            vec![4, 5, 6]
        );
    }

    #[test]
    fn a_missing_cache_root_is_not_an_error() {
        let root = std::env::temp_dir().join(format!("basemap-tiles-absent-{}", unique_suffix()));
        let cache = TileDiskCache::new(root.clone(), 8 * 1024 * 1024);
        assert!(cache.load(TileProvider::UsgsImagery, tile(0)).is_none());
        assert_eq!(cache.sweep(), SweepReport::default());
        // Storing creates the tree on demand.
        cache
            .store(TileProvider::UsgsImagery, tile(0), None, &[9], 0)
            .expect("store creates directories");
        assert!(cache.load(TileProvider::UsgsImagery, tile(0)).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_namespaced_by_provider_zoom_and_index() {
        let root = TempRoot::new("paths");
        let cache = TileDiskCache::new(root.0.clone(), 1_024 * 1_024);
        let path = cache.path_for(TileProvider::UsgsImagery, TileId::new(9, 117, 202).unwrap());
        assert!(path.ends_with("usgs-imagery/9/117_202.tile"));
        let other = cache.path_for(TileProvider::UsgsTopo, TileId::new(9, 117, 202).unwrap());
        assert_ne!(path, other);
    }

    fn set_modified(path: &Path, unix_seconds: u64) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for timestamp");
        file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(unix_seconds))
            .expect("set modified time");
    }
}
