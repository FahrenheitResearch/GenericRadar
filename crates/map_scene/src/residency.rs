//! Which geometry generations are resident on the GPU, and what that costs.
//!
//! Kept separate from the wgpu code on purpose: upload decisions are the thing
//! the pan-invariance claim rests on, and they can be tested exhaustively here
//! without a graphics device. The GPU layer only executes what this decides.

use std::collections::HashMap;

use analyst_runtime::GeometryCacheKey;

/// Default ceiling for resident map geometry. Comfortably holds several LODs of
/// the county layer for one site while bounding a pathological dataset.
pub const DEFAULT_BUDGET_BYTES: usize = 192 * 1024 * 1024;

/// What a `touch` decided.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Admission {
    /// Already on the GPU. This is the pan and small-zoom path: no upload.
    AlreadyResident,
    /// Newly admitted; the caller must upload. Any evicted keys must be freed.
    Admitted { evicted: Vec<GeometryCacheKey> },
    /// Larger than the whole budget, so it cannot be admitted.
    Rejected,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResidencyMetrics {
    /// Uploads performed. The pan benchmark asserts this does not move.
    pub uploads: u64,
    pub evictions: u64,
    pub hits: u64,
    pub resident_bytes: usize,
}

#[derive(Debug)]
struct Entry {
    bytes: usize,
    /// Monotonic use stamp; the smallest is evicted first.
    last_used: u64,
}

/// Byte-budgeted LRU set of resident geometry keys.
#[derive(Debug)]
pub struct GeometryResidency {
    entries: HashMap<GeometryCacheKey, Entry>,
    budget_bytes: usize,
    clock: u64,
    metrics: ResidencyMetrics,
}

impl Default for GeometryResidency {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

impl GeometryResidency {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget_bytes,
            clock: 0,
            metrics: ResidencyMetrics::default(),
        }
    }

    pub fn metrics(&self) -> ResidencyMetrics {
        self.metrics
    }

    pub fn is_resident(&self, key: &GeometryCacheKey) -> bool {
        self.entries.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Mark a key as used this frame, admitting it if needed.
    pub fn touch(&mut self, key: GeometryCacheKey, bytes: usize) -> Admission {
        self.clock += 1;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.clock;
            self.metrics.hits += 1;
            return Admission::AlreadyResident;
        }
        if bytes > self.budget_bytes {
            return Admission::Rejected;
        }

        // Evict least-recently-used entries until the newcomer fits. The key
        // being admitted is not yet present, so it cannot evict itself.
        let mut evicted = Vec::new();
        while self.metrics.resident_bytes + bytes > self.budget_bytes {
            let Some(victim) = self.least_recently_used() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.metrics.resident_bytes -= entry.bytes;
                self.metrics.evictions += 1;
                evicted.push(victim);
            }
        }

        self.entries.insert(
            key,
            Entry {
                bytes,
                last_used: self.clock,
            },
        );
        self.metrics.resident_bytes += bytes;
        self.metrics.uploads += 1;
        Admission::Admitted { evicted }
    }

    /// Drop every key that does not satisfy `keep`. Used when a site or dataset
    /// change makes older generations unreachable.
    pub fn retain(&mut self, keep: impl Fn(&GeometryCacheKey) -> bool) -> Vec<GeometryCacheKey> {
        let dropped: Vec<GeometryCacheKey> = self
            .entries
            .keys()
            .filter(|key| !keep(key))
            .copied()
            .collect();
        for key in &dropped {
            if let Some(entry) = self.entries.remove(key) {
                self.metrics.resident_bytes -= entry.bytes;
                self.metrics.evictions += 1;
            }
        }
        dropped
    }

    fn least_recently_used(&self) -> Option<GeometryCacheKey> {
        self.entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| *key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{Generation, LodBucket};

    fn key(lod: i16) -> GeometryCacheKey {
        GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod: LodBucket(lod),
        }
    }

    fn key_with_projection(projection: u64, lod: i16) -> GeometryCacheKey {
        GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(projection),
            style: Generation::new(1),
            lod: LodBucket(lod),
        }
    }

    #[test]
    fn the_first_touch_uploads_and_the_rest_do_not() {
        let mut residency = GeometryResidency::new(1_000);
        assert_eq!(
            residency.touch(key(0), 100),
            Admission::Admitted {
                evicted: Vec::new()
            }
        );
        for _ in 0..10_000 {
            assert_eq!(residency.touch(key(0), 100), Admission::AlreadyResident);
        }
        assert_eq!(residency.metrics().uploads, 1);
        assert_eq!(residency.metrics().hits, 10_000);
        assert_eq!(residency.metrics().resident_bytes, 100);
    }

    #[test]
    fn eviction_is_least_recently_used() {
        let mut residency = GeometryResidency::new(250);
        residency.touch(key(0), 100);
        residency.touch(key(1), 100);
        // Use bucket 0 again so bucket 1 becomes the eviction candidate.
        residency.touch(key(0), 100);

        let Admission::Admitted { evicted } = residency.touch(key(2), 100) else {
            panic!("expected admission");
        };
        assert_eq!(evicted, vec![key(1)]);
        assert!(residency.is_resident(&key(0)));
        assert!(residency.is_resident(&key(2)));
        assert_eq!(residency.metrics().resident_bytes, 200);
    }

    #[test]
    fn something_larger_than_the_budget_is_rejected_not_thrashed() {
        let mut residency = GeometryResidency::new(100);
        residency.touch(key(0), 100);
        assert_eq!(residency.touch(key(1), 101), Admission::Rejected);
        // The rejection must not have evicted the resident entry.
        assert!(residency.is_resident(&key(0)));
        assert_eq!(residency.metrics().uploads, 1);
    }

    #[test]
    fn a_site_change_drops_every_stale_projection() {
        let mut residency = GeometryResidency::new(10_000);
        residency.touch(key_with_projection(1, 0), 100);
        residency.touch(key_with_projection(1, 1), 100);
        residency.touch(key_with_projection(2, 0), 100);

        let dropped = residency.retain(|key| key.projection == Generation::new(2));
        assert_eq!(dropped.len(), 2);
        assert_eq!(residency.len(), 1);
        assert_eq!(residency.metrics().resident_bytes, 100);
        assert!(residency.is_resident(&key_with_projection(2, 0)));
    }

    #[test]
    fn byte_accounting_returns_to_zero_when_everything_is_dropped() {
        let mut residency = GeometryResidency::new(10_000);
        residency.touch(key(0), 400);
        residency.touch(key(1), 600);
        assert_eq!(residency.metrics().resident_bytes, 1_000);
        residency.retain(|_| false);
        assert_eq!(residency.metrics().resident_bytes, 0);
        assert!(residency.is_empty());
    }
}
