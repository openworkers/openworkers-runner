//! In-memory LRU cache for compiled worker code.
//!
//! Holds V8 code caches and precompiled wasm components, keyed by
//! `(worker_id, version)`. Both spare a cold start the work of compiling code
//! the runner has already compiled once.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;
use once_cell::sync::Lazy;

/// Default max number of cached entries.
const DEFAULT_MAX_ENTRIES: usize = 5000;

/// Default byte budget. An entry runs to hundreds of KB, so the entry count
/// alone lets the cache grow to several GB.
const DEFAULT_MAX_BYTES: usize = 512 * 1024 * 1024;

/// Cache key: (worker_id, version)
type CacheKey = (String, i32);

/// LRU of compiled code blobs, bounded by total bytes as well as by entry count.
struct CodeCache {
    entries: LruCache<CacheKey, Vec<u8>>,
    bytes: usize,
    max_bytes: usize,
}

impl CodeCache {
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

    fn put(&mut self, key: CacheKey, blob: &[u8]) {
        // A blob over budget would empty the cache and still not fit
        if blob.len() > self.max_bytes {
            tracing::warn!(
                "Code cache: rejecting {} ({} bytes over the {} byte budget)",
                key.0,
                blob.len(),
                self.max_bytes
            );

            return;
        }

        self.bytes += blob.len();

        // push hands back whatever it replaced or evicted, whose bytes are gone
        if let Some((_, dropped)) = self.entries.push(key, blob.to_vec()) {
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

/// Global in-memory LRU cache for compiled worker code.
static CODE_CACHE: Lazy<Mutex<CodeCache>> = Lazy::new(|| {
    let max_entries = read_env("CODE_CACHE_MAX", "SNAPSHOT_CACHE_MAX")
        .and_then(NonZeroUsize::new)
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_MAX_ENTRIES).unwrap());

    let max_bytes =
        read_env("CODE_CACHE_MAX_BYTES", "SNAPSHOT_CACHE_MAX_BYTES").unwrap_or(DEFAULT_MAX_BYTES);

    tracing::info!(
        "Code cache: in-memory, max_entries={}, max_bytes={}",
        max_entries,
        max_bytes
    );

    Mutex::new(CodeCache::new(max_entries, max_bytes))
});

/// Value of `name`, or of the deprecated `fallback` when only that one is set.
fn read_env(name: &str, fallback: &str) -> Option<usize> {
    if let Some(value) = parse_env(name) {
        return Some(value);
    }

    let value = parse_env(fallback)?;

    tracing::warn!("{fallback} is deprecated, rename it to {name}");

    Some(value)
}

fn parse_env(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Try to read a worker's V8 code cache from memory.
pub fn get(worker_id: &str, version: i32) -> Option<Vec<u8>> {
    let key = (worker_id.to_string(), version);

    CODE_CACHE.lock().unwrap().get(&key)
}

/// Store a worker's V8 code cache, unless it alone exceeds the byte budget.
pub fn put(worker_id: &str, version: i32, snapshot: &[u8]) {
    let key = (worker_id.to_string(), version);

    CODE_CACHE.lock().unwrap().put(key, snapshot);
}

/// Id half of a precompiled wasm component's key.
///
/// `engine_key` names the engine settings the artifact was compiled with. It
/// rides in the key rather than being checked on read, so an upgraded runtime
/// stops looking the old entries up and the LRU evicts them. The prefix keeps
/// components and V8 code caches of the same worker apart.
fn wasm_id(worker_id: &str, engine_key: &str) -> String {
    format!("wasm:{engine_key}:{worker_id}")
}

/// Try to read a worker's precompiled wasm component from memory.
pub fn get_wasm(worker_id: &str, version: i32, engine_key: &str) -> Option<Vec<u8>> {
    let key = (wasm_id(worker_id, engine_key), version);

    CODE_CACHE.lock().unwrap().get(&key)
}

/// Store a worker's precompiled wasm component, unless it alone exceeds the
/// byte budget.
pub fn put_wasm(worker_id: &str, version: i32, engine_key: &str, component: &[u8]) {
    let key = (wasm_id(worker_id, engine_key), version);

    CODE_CACHE.lock().unwrap().put(key, component);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(max_bytes: usize) -> CodeCache {
        CodeCache::new(NonZeroUsize::new(1024).unwrap(), max_bytes)
    }

    fn key(id: &str) -> CacheKey {
        (id.to_string(), 1)
    }

    /// A deployment that still sets SNAPSHOT_CACHE_MAX_BYTES keeps its budget.
    /// The names are test-local, because the process environment is shared with
    /// every other test in this binary.
    #[test]
    fn the_deprecated_env_name_is_the_fallback() {
        const NEW: &str = "CODE_CACHE_TEST_NEW";
        const OLD: &str = "CODE_CACHE_TEST_OLD";

        assert_eq!(read_env(NEW, OLD), None);

        // SAFETY: single-threaded within this test, and no other test reads
        // these two names
        unsafe { std::env::set_var(OLD, "123") };

        assert_eq!(read_env(NEW, OLD), Some(123));

        // SAFETY: as above
        unsafe { std::env::set_var(NEW, "456") };

        assert_eq!(read_env(NEW, OLD), Some(456));
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
