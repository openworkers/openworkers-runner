//! In-memory LRU cache for worker snapshots.
//!
//! Stores V8 snapshot blobs in memory, keyed by `(worker_id, version)`.
//! This avoids disk I/O on every request and keeps snapshot bytes readily
//! available for V8 isolate creation.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;
use once_cell::sync::Lazy;

/// Default max number of cached snapshots.
const DEFAULT_MAX_ENTRIES: usize = 5000;

/// Default byte budget. A snapshot runs to hundreds of KB, so the entry count
/// alone lets the cache grow to several GB.
const DEFAULT_MAX_BYTES: usize = 512 * 1024 * 1024;

/// Cache key: (worker_id, version)
type CacheKey = (String, i32);

/// LRU of snapshot blobs, bounded by total bytes as well as by entry count.
struct SnapshotCache {
    entries: LruCache<CacheKey, Vec<u8>>,
    bytes: usize,
    max_bytes: usize,
}

impl SnapshotCache {
    fn new(max_entries: NonZeroUsize, max_bytes: usize) -> Self {
        Self {
            entries: LruCache::new(max_entries),
            bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<Vec<u8>> {
        self.entries.get(key).cloned()
    }

    fn put(&mut self, key: CacheKey, snapshot: &[u8]) {
        // A blob over budget would empty the cache and still not fit
        if snapshot.len() > self.max_bytes {
            tracing::warn!(
                "Snapshot cache: rejecting {} ({} bytes over the {} byte budget)",
                key.0,
                snapshot.len(),
                self.max_bytes
            );

            return;
        }

        self.bytes += snapshot.len();

        // push hands back whatever it replaced or evicted, whose bytes are gone
        if let Some((_, dropped)) = self.entries.push(key, snapshot.to_vec()) {
            self.bytes -= dropped.len();
        }

        while self.bytes > self.max_bytes {
            match self.entries.pop_lru() {
                Some((_, evicted)) => self.bytes -= evicted.len(),
                None => break,
            }
        }
    }
}

/// Global in-memory LRU cache for worker snapshots.
static SNAPSHOT_CACHE: Lazy<Mutex<SnapshotCache>> = Lazy::new(|| {
    let max_entries = read_env("SNAPSHOT_CACHE_MAX")
        .and_then(NonZeroUsize::new)
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_MAX_ENTRIES).unwrap());

    let max_bytes = read_env("SNAPSHOT_CACHE_MAX_BYTES").unwrap_or(DEFAULT_MAX_BYTES);

    tracing::info!(
        "Snapshot cache: in-memory, max_entries={}, max_bytes={}",
        max_entries,
        max_bytes
    );

    Mutex::new(SnapshotCache::new(max_entries, max_bytes))
});

fn read_env(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Try to read a cached worker snapshot from memory.
pub fn get(worker_id: &str, version: i32) -> Option<Vec<u8>> {
    let key = (worker_id.to_string(), version);

    SNAPSHOT_CACHE.lock().unwrap().get(&key)
}

/// Store a worker snapshot in memory, unless it alone exceeds the byte budget.
pub fn put(worker_id: &str, version: i32, snapshot: &[u8]) {
    let key = (worker_id.to_string(), version);

    SNAPSHOT_CACHE.lock().unwrap().put(key, snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(max_bytes: usize) -> SnapshotCache {
        SnapshotCache::new(NonZeroUsize::new(1024).unwrap(), max_bytes)
    }

    fn key(id: &str) -> CacheKey {
        (id.to_string(), 1)
    }

    #[test]
    fn evicts_oldest_over_budget() {
        let mut cache = cache(250);

        cache.put(key("a"), &[0u8; 100]);
        cache.put(key("b"), &[0u8; 100]);
        cache.put(key("c"), &[0u8; 100]);

        assert_eq!(cache.get(&key("a")), None);
        assert_eq!(cache.bytes, 200);
        assert!(cache.get(&key("b")).is_some());
        assert!(cache.get(&key("c")).is_some());
    }

    #[test]
    fn rejects_blob_over_budget() {
        let mut cache = cache(250);

        cache.put(key("a"), &[0u8; 100]);
        cache.put(key("big"), &[0u8; 300]);

        assert_eq!(cache.get(&key("big")), None);
        assert!(cache.get(&key("a")).is_some());
        assert_eq!(cache.bytes, 100);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = cache(250);

        cache.put(key("a"), &[0u8; 100]);
        cache.put(key("b"), &[0u8; 100]);

        assert!(cache.get(&key("a")).is_some());

        cache.put(key("c"), &[0u8; 100]);

        assert_eq!(cache.get(&key("b")), None);
        assert!(cache.get(&key("a")).is_some());
    }

    #[test]
    fn replacing_a_key_does_not_leak_bytes() {
        let mut cache = cache(250);

        cache.put(key("a"), &[0u8; 100]);
        cache.put(key("a"), &[0u8; 50]);

        assert_eq!(cache.bytes, 50);
    }
}
