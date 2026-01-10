//! Isolate pool with LRU eviction using v8::Locker
//!
//! This module implements a thread-safe pool of V8 isolates with LRU eviction.
//! Unlike the old implementation which was disabled due to LIFO constraints,
//! this version uses v8::Locker which allows isolates to be acquired and
//! released in any order.
//!
//! Architecture:
//! - Pool stores worker_id → LockerManagedIsolate mapping
//! - LRU eviction when pool is full
//! - Lazy creation (isolates created on first use)
//! - Thread-safe via Arc<Mutex<>>
//!
//! Usage:
//! ```ignore
//! let pool = IsolatePool::new(1000, limits);
//! let pooled = pool.acquire("worker_123").await;
//! pooled.with_lock_async(|isolate| async {
//!     // Use isolate...
//! }).await;
//! // Auto-unlock and mark as recently used on drop
//! ```

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[cfg(feature = "v8")]
use openworkers_runtime_v8::LockerManagedIsolate;

use openworkers_core::RuntimeLimits;

type WorkerId = String;

/// Entry in the pool (isolate + metadata)
struct IsolateEntry {
    isolate: LockerManagedIsolate,
    created_at: Instant,
    total_requests: u64,
}

impl IsolateEntry {
    fn new(limits: RuntimeLimits) -> Self {
        log::debug!("Creating new LockerManagedIsolate");
        let start = Instant::now();
        let isolate = LockerManagedIsolate::new(limits);
        let duration = start.elapsed();
        log::info!(
            "LockerManagedIsolate created in {:?} (snapshot: {})",
            duration,
            isolate.use_snapshot
        );

        Self {
            isolate,
            created_at: Instant::now(),
            total_requests: 0,
        }
    }
}

/// Global pool of isolates with LRU eviction
///
/// This pool maintains a cache of isolates keyed by worker_id.
/// When the pool is full, the least recently used isolate is evicted.
pub struct IsolatePool {
    /// LRU cache: worker_id → isolate entry
    cache: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
    /// Max isolates in pool
    max_size: usize,
    /// Limits for new isolates
    limits: RuntimeLimits,
}

impl IsolatePool {
    /// Create new isolate pool
    ///
    /// # Arguments
    /// * `max_size` - Maximum number of isolates to cache (e.g., 1000)
    /// * `limits` - Runtime limits for isolates (heap size, etc.)
    pub fn new(max_size: usize, limits: RuntimeLimits) -> Self {
        log::info!(
            "Initializing IsolatePool with max_size={}, heap_max={}MB",
            max_size,
            limits.heap_max_mb
        );

        Self {
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).unwrap(),
            ))),
            max_size,
            limits,
        }
    }

    /// Acquire isolate for a worker (or create if not exists)
    ///
    /// This method:
    /// 1. Locks the cache
    /// 2. Gets existing entry or creates new
    /// 3. Unlocks cache
    /// 4. Returns PooledIsolate guard
    ///
    /// The returned PooledIsolate can be used to lock the isolate
    /// for the current thread using v8::Locker.
    pub async fn acquire(&self, worker_id: &str) -> PooledIsolate {
        let mut cache = self.cache.lock().await;

        // Try to get from cache
        let entry = if let Some(entry) = cache.get(worker_id) {
            log::debug!("Isolate cache HIT for worker {}", worker_id);
            Arc::clone(entry)
        } else {
            log::debug!("Isolate cache MISS for worker {}, creating new", worker_id);

            // Create new isolate entry
            let entry = Arc::new(Mutex::new(IsolateEntry::new(self.limits.clone())));

            // Insert into LRU cache
            // If cache is full, LRU will evict the least recently used
            if let Some((evicted_worker_id, evicted_entry)) =
                cache.push(worker_id.to_string(), Arc::clone(&entry))
            {
                log::info!(
                    "Isolate LRU eviction: worker {} evicted (cache full at {})",
                    evicted_worker_id,
                    self.max_size
                );

                // Evicted entry will be dropped when Arc refcount reaches 0
                // If it's still in use (PooledIsolate exists), drop waits
                drop(evicted_entry);
            }

            entry
        };

        // Unlock cache before returning guard
        drop(cache);

        PooledIsolate {
            entry,
            worker_id: worker_id.to_string(),
            pool: Arc::clone(&self.cache),
        }
    }

    /// Get pool statistics
    pub async fn stats(&self) -> PoolStats {
        let cache = self.cache.lock().await;
        PoolStats {
            total: self.max_size,
            cached: cache.len(),
            capacity: cache.cap().get(),
        }
    }
}

/// RAII guard for isolate usage
///
/// This guard provides safe access to an isolate from the pool.
/// When dropped, it marks the isolate as recently used in the LRU cache.
pub struct PooledIsolate {
    entry: Arc<Mutex<IsolateEntry>>,
    worker_id: WorkerId,
    pool: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
}

impl PooledIsolate {
    /// Execute closure with locked isolate (blocking)
    ///
    /// Creates v8::Locker, calls closure, drops locker.
    /// This is a blocking operation - use with_lock_async for async code.
    #[cfg(feature = "v8")]
    pub fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&v8::Isolate) -> R,
    {
        let mut entry = self.entry.blocking_lock();

        // Create v8::Locker (locks V8 side)
        let _locker = v8::Locker::new(entry.isolate.as_isolate());

        // Update stats
        entry.total_requests += 1;

        // Execute user closure
        f(entry.isolate.as_isolate())
    }

    /// Execute async closure with locked isolate
    ///
    /// Creates v8::Locker, calls async closure, drops locker.
    /// This is the preferred method for async operations.
    #[cfg(feature = "v8")]
    pub async fn with_lock_async<F, Fut, R>(&self, f: F) -> R
    where
        F: FnOnce(&v8::Isolate) -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let mut entry = self.entry.lock().await;

        // Create v8::Locker
        let _locker = v8::Locker::new(entry.isolate.as_isolate());

        // Update stats
        entry.total_requests += 1;

        log::trace!(
            "Isolate locked for worker {} (total_requests: {})",
            self.worker_id,
            entry.total_requests
        );

        // Execute async user closure
        let result = f(entry.isolate.as_isolate()).await;

        log::trace!("Isolate unlocked for worker {}", self.worker_id);

        result
    }

    /// Get worker ID
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }
}

impl Drop for PooledIsolate {
    fn drop(&mut self) {
        // Touch LRU to mark as recently used
        // This is async-safe because we use try_lock
        if let Ok(mut cache) = self.pool.try_lock() {
            // get() touches the entry in LRU (marks as recently used)
            cache.get(&self.worker_id);
            log::trace!("Isolate marked as recently used: {}", self.worker_id);
        }
        // If lock fails, isolate will still be in cache but not marked as used
        // This is acceptable - better than blocking on drop
    }
}

/// Pool statistics
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Maximum capacity of the pool
    pub total: usize,
    /// Current number of cached isolates
    pub cached: usize,
    /// LRU capacity (same as total)
    pub capacity: usize,
}

#[cfg(all(test, feature = "v8"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_acquire_and_release() {
        let pool = IsolatePool::new(10, RuntimeLimits::default());

        let pooled = pool.acquire("worker_1").await;
        pooled.with_lock(|isolate| {
            // Isolate is locked and usable
            assert!(!isolate.is_execution_terminating());
        });
        // Drop releases back to pool
    }

    #[tokio::test]
    async fn test_pool_cache_hit() {
        let pool = IsolatePool::new(10, RuntimeLimits::default());

        // First acquire (cache miss)
        {
            let _pooled = pool.acquire("worker_1").await;
        }

        // Second acquire (cache hit)
        {
            let _pooled = pool.acquire("worker_1").await;
            // Should reuse same isolate
        }

        let stats = pool.stats().await;
        assert_eq!(stats.cached, 1);
    }

    #[tokio::test]
    async fn test_pool_lru_eviction() {
        let pool = IsolatePool::new(2, RuntimeLimits::default()); // Small pool

        // Fill pool
        {
            let _p1 = pool.acquire("worker_1").await;
        }
        {
            let _p2 = pool.acquire("worker_2").await;
        }

        // This should evict worker_1 (LRU)
        {
            let _p3 = pool.acquire("worker_3").await;
        }

        let stats = pool.stats().await;
        assert_eq!(stats.cached, 2);
    }

    #[tokio::test]
    async fn test_pool_concurrent_access() {
        let pool = Arc::new(IsolatePool::new(10, RuntimeLimits::default()));

        // Multiple threads accessing different workers
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let pool = Arc::clone(&pool);
                tokio::spawn(async move {
                    let worker_id = format!("worker_{}", i);
                    let pooled = pool.acquire(&worker_id).await;
                    pooled.with_lock(|_isolate| {
                        // Simulate work
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    });
                })
            })
            .collect();

        for handle in handles {
            handle.await.unwrap();
        }

        let stats = pool.stats().await;
        assert_eq!(stats.cached, 5);
    }

    #[tokio::test]
    async fn test_pool_same_worker_sequential() {
        let pool = Arc::new(IsolatePool::new(10, RuntimeLimits::default()));

        // Same worker, multiple sequential requests
        for _ in 0..10 {
            let pooled = pool.acquire("worker_1").await;
            pooled.with_lock(|_isolate| {
                // Simulate work
            });
        }

        // Should still have only 1 isolate in cache
        let stats = pool.stats().await;
        assert_eq!(stats.cached, 1);
    }
}
