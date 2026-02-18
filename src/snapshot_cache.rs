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
const DEFAULT_MAX_ENTRIES: usize = 500;

/// Cache key: (worker_id, version)
type CacheKey = (String, i32);

/// Global in-memory LRU cache for worker snapshots.
static SNAPSHOT_CACHE: Lazy<Mutex<LruCache<CacheKey, Vec<u8>>>> = Lazy::new(|| {
    let max_entries = std::env::var("SNAPSHOT_CACHE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ENTRIES);

    tracing::info!("Snapshot cache: in-memory, max_entries={}", max_entries);

    Mutex::new(LruCache::new(
        NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::new(DEFAULT_MAX_ENTRIES).unwrap()),
    ))
});

/// Try to read a cached worker snapshot from memory.
pub fn get(worker_id: &str, version: i32) -> Option<Vec<u8>> {
    let key = (worker_id.to_string(), version);
    let mut cache = SNAPSHOT_CACHE.lock().unwrap();
    cache.get(&key).cloned()
}

/// Store a worker snapshot in memory.
pub fn put(worker_id: &str, version: i32, snapshot: &[u8]) {
    let key = (worker_id.to_string(), version);
    let mut cache = SNAPSHOT_CACHE.lock().unwrap();
    cache.put(key, snapshot.to_vec());
}
