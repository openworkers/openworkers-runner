//! Isolate pool for reusing V8 isolates across requests
//!
//! Instead of creating a new V8 isolate for each request (expensive ~3-5ms),
//! we maintain a pool of warm isolates that can be reused.
//!
//! Each request creates a fresh ExecutionContext (cheap ~100µs) within a
//! pooled isolate, providing complete isolation.

use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

#[cfg(feature = "v8")]
use openworkers_runtime_v8::SharedIsolate;

use openworkers_core::RuntimeLimits;

/// Pool of reusable V8 isolates
///
/// Each isolate in the pool can be used to create multiple ExecutionContexts.
/// The pool manages isolate allocation to ensure efficient reuse.
pub struct IsolatePool {
    /// Available isolates ready to use
    available: Arc<Mutex<VecDeque<SharedIsolate>>>,
    /// Semaphore to limit concurrent isolate usage
    semaphore: Arc<Semaphore>,
    /// Limits for creating new isolates
    limits: RuntimeLimits,
    /// Pool size
    size: usize,
}

impl IsolatePool {
    /// Create a new isolate pool with the specified size
    ///
    /// This is expensive as it pre-creates all isolates (~3-5ms each).
    /// Should be done once at startup.
    pub fn new(size: usize, limits: RuntimeLimits) -> Self {
        let mut isolates = VecDeque::new();

        // Pre-create all isolates
        for _ in 0..size {
            let shared_isolate = SharedIsolate::new(limits.clone());
            isolates.push_back(shared_isolate);
        }

        Self {
            available: Arc::new(Mutex::new(isolates)),
            semaphore: Arc::new(Semaphore::new(size)),
            limits,
            size,
        }
    }

    /// Acquire an isolate from the pool
    ///
    /// This will wait if all isolates are currently in use.
    /// The caller should create an ExecutionContext from this isolate.
    pub async fn acquire(&self) -> SharedIsolate {
        // Wait for an available slot
        let _permit = self.semaphore.acquire().await.unwrap();

        // Take an isolate from the pool
        let mut pool = self.available.lock().await;
        let isolate = pool.pop_front();

        // Release the lock before potentially blocking
        drop(pool);

        match isolate {
            Some(shared_isolate) => shared_isolate,
            None => {
                // Pool was empty (shouldn't happen with semaphore)
                // Create a new one as fallback
                eprintln!("[IsolatePool] Warning: Pool empty, creating new isolate");
                SharedIsolate::new(self.limits.clone())
            }
        }
    }

    /// Release an isolate back to the pool
    ///
    /// The isolate will be reused for future requests.
    /// Make sure all ExecutionContexts created from this isolate are dropped first.
    pub async fn release(&self, shared_isolate: SharedIsolate) {
        // Add back to available pool
        let mut pool = self.available.lock().await;
        pool.push_back(shared_isolate);
        drop(pool);

        // Release semaphore permit
        self.semaphore.add_permits(1);
    }

    /// Get pool statistics
    pub async fn stats(&self) -> PoolStats {
        let pool = self.available.lock().await;
        let available = pool.len();

        PoolStats {
            total: self.size,
            available,
            busy: self.size - available,
        }
    }
}

/// Statistics about the isolate pool
#[derive(Debug, Clone)]
pub struct PoolStats {
    pub total: usize,
    pub available: usize,
    pub busy: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn test_isolate_pool_acquire_release() {
        let limits = RuntimeLimits::default();
        let pool = IsolatePool::new(2, limits);

        // Acquire first isolate
        let isolate1 = pool.acquire().await;
        let stats = pool.stats().await;
        assert_eq!(stats.available, 1);
        assert_eq!(stats.busy, 1);

        // Acquire second isolate
        let isolate2 = pool.acquire().await;
        let stats = pool.stats().await;
        assert_eq!(stats.available, 0);
        assert_eq!(stats.busy, 2);

        // Release first isolate
        pool.release(isolate1).await;
        let stats = pool.stats().await;
        assert_eq!(stats.available, 1);
        assert_eq!(stats.busy, 1);

        // Release second isolate
        pool.release(isolate2).await;
        let stats = pool.stats().await;
        assert_eq!(stats.available, 2);
        assert_eq!(stats.busy, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_isolate_pool_concurrent() {
        let local_set = tokio::task::LocalSet::new();

        local_set
            .run_until(async {
                let limits = RuntimeLimits::default();
                let pool = Arc::new(IsolatePool::new(4, limits));

                // Spawn multiple tasks that acquire and release
                let mut handles = vec![];

                for _ in 0..10 {
                    let pool_clone = pool.clone();
                    let handle = tokio::task::spawn_local(async move {
                        let isolate = pool_clone.acquire().await;
                        // Simulate some work
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        pool_clone.release(isolate).await;
                    });
                    handles.push(handle);
                }

                // Wait for all tasks to complete
                for handle in handles {
                    handle.await.unwrap();
                }

                // All isolates should be back in the pool
                let stats = pool.stats().await;
                assert_eq!(stats.available, 4);
                assert_eq!(stats.busy, 0);
            })
            .await;
    }
}
