//! Binding operation limiters
//!
//! Provides rate limiting for binding operations (fetch, KV, database, storage).
//! Supports both total request limits and concurrent operation limits.
//!
//! ## Example
//!
//! ```ignore
//! let limiter = BindingLimiter::new(&limits.fetch_limit, "fetch");
//!
//! // This will block if too many concurrent operations
//! // or error if total limit exceeded
//! let _guard = limiter.acquire().await?;
//!
//! // Do the operation...
//! // Guard is automatically released when dropped
//! ```

use openworkers_core::BindingLimit;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Error when a binding limit is exceeded
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitError {
    /// Total calls per request exceeded
    TotalExceeded {
        binding: &'static str,
        limit: u32,
        current: u32,
    },
    /// Semaphore closed (shouldn't happen in normal operation)
    SemaphoreClosed { binding: &'static str },
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitError::TotalExceeded {
                binding,
                limit,
                current,
            } => {
                write!(
                    f,
                    "Binding limit exceeded: {} ({}/{} total calls per request)",
                    binding, current, limit
                )
            }
            LimitError::SemaphoreClosed { binding } => {
                write!(f, "Binding limiter closed: {}", binding)
            }
        }
    }
}

impl std::error::Error for LimitError {}

/// Limiter for a specific binding type
///
/// Tracks total calls per request and limits concurrent operations.
/// Use `acquire()` before each operation and the guard will be
/// automatically released when dropped.
pub struct BindingLimiter {
    /// Semaphore for concurrent limit (None if unlimited)
    semaphore: Option<Arc<Semaphore>>,
    /// Counter for total calls
    total_count: AtomicU32,
    /// Maximum total calls (0 = unlimited)
    max_total: u32,
    /// Binding name for error messages
    name: &'static str,
}

impl BindingLimiter {
    /// Create a new limiter from a BindingLimit config
    pub fn new(limit: &BindingLimit, name: &'static str) -> Self {
        Self {
            semaphore: if limit.max_concurrent > 0 {
                Some(Arc::new(Semaphore::new(limit.max_concurrent as usize)))
            } else {
                None
            },
            total_count: AtomicU32::new(0),
            max_total: limit.max_total,
            name,
        }
    }

    /// Create an unlimited limiter (no restrictions)
    pub fn unlimited(name: &'static str) -> Self {
        Self {
            semaphore: None,
            total_count: AtomicU32::new(0),
            max_total: 0,
            name,
        }
    }

    /// Acquire a permit to perform an operation
    ///
    /// This will:
    /// 1. Check if total limit is exceeded (returns error immediately)
    /// 2. Wait for a concurrent slot if at capacity (blocks)
    ///
    /// The returned guard must be held for the duration of the operation.
    /// When dropped, the concurrent slot is released.
    pub async fn acquire(&self) -> Result<LimiterGuard, LimitError> {
        // Check total limit first
        if self.max_total > 0 {
            let current = self.total_count.fetch_add(1, Ordering::SeqCst);

            if current >= self.max_total {
                // Undo the increment
                self.total_count.fetch_sub(1, Ordering::SeqCst);

                return Err(LimitError::TotalExceeded {
                    binding: self.name,
                    limit: self.max_total,
                    current: current + 1,
                });
            }
        }

        // Acquire concurrent slot (blocks if at capacity)
        let permit = if let Some(ref sem) = self.semaphore {
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| LimitError::SemaphoreClosed { binding: self.name })?;
            Some(permit)
        } else {
            None
        };

        Ok(LimiterGuard { _permit: permit })
    }

    /// Get current count of operations performed
    pub fn count(&self) -> u32 {
        self.total_count.load(Ordering::SeqCst)
    }

    /// Get number of available concurrent slots
    pub fn available_permits(&self) -> usize {
        self.semaphore
            .as_ref()
            .map(|s| s.available_permits())
            .unwrap_or(usize::MAX)
    }

    /// Reset the limiter for a new request
    ///
    /// This resets the total count to 0. Use this when reusing a worker
    /// for multiple requests to ensure limits are per-request.
    ///
    /// Note: Concurrent permits are automatically released when guards are dropped,
    /// so the semaphore doesn't need explicit reset.
    pub fn reset(&self) {
        self.total_count.store(0, Ordering::SeqCst);
    }
}

/// Guard that releases the concurrent slot when dropped
pub struct LimiterGuard {
    _permit: Option<OwnedSemaphorePermit>,
}

/// Collection of limiters for all binding types
pub struct BindingLimiters {
    pub fetch: BindingLimiter,
    pub kv: BindingLimiter,
    pub database: BindingLimiter,
    pub storage: BindingLimiter,
}

impl BindingLimiters {
    /// Create limiters from RuntimeLimits
    pub fn new(limits: &openworkers_core::RuntimeLimits) -> Self {
        Self {
            fetch: BindingLimiter::new(&limits.fetch_limit, "fetch"),
            kv: BindingLimiter::new(&limits.kv_limit, "kv"),
            database: BindingLimiter::new(&limits.database_limit, "database"),
            storage: BindingLimiter::new(&limits.storage_limit, "storage"),
        }
    }

    /// Create unlimited limiters (no restrictions)
    pub fn unlimited() -> Self {
        Self {
            fetch: BindingLimiter::unlimited("fetch"),
            kv: BindingLimiter::unlimited("kv"),
            database: BindingLimiter::unlimited("database"),
            storage: BindingLimiter::unlimited("storage"),
        }
    }

    /// Reset all limiters for a new request
    ///
    /// Use this when reusing a worker/ops instance for multiple requests
    /// to ensure limits are enforced per-request rather than accumulating.
    pub fn reset_all(&self) {
        self.fetch.reset();
        self.kv.reset();
        self.database.reset();
        self.storage.reset();
    }
}

impl Default for BindingLimiters {
    fn default() -> Self {
        Self::new(&openworkers_core::RuntimeLimits::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_total_limit() {
        let limit = BindingLimit::new(3, 0); // 3 total, unlimited concurrent
        let limiter = BindingLimiter::new(&limit, "test");

        // First 3 should succeed
        let _g1 = limiter.acquire().await.unwrap();
        let _g2 = limiter.acquire().await.unwrap();
        let _g3 = limiter.acquire().await.unwrap();

        // 4th should fail
        let result = limiter.acquire().await;
        assert!(matches!(result, Err(LimitError::TotalExceeded { .. })));
    }

    #[tokio::test]
    async fn test_concurrent_limit() {
        let limit = BindingLimit::new(0, 2); // unlimited total, 2 concurrent
        let limiter = Arc::new(BindingLimiter::new(&limit, "test"));

        // Acquire 2 permits
        let g1 = limiter.acquire().await.unwrap();
        let g2 = limiter.acquire().await.unwrap();

        assert_eq!(limiter.available_permits(), 0);

        // Drop one, should free a slot
        drop(g1);
        assert_eq!(limiter.available_permits(), 1);

        // Can acquire again
        let _g3 = limiter.acquire().await.unwrap();
        assert_eq!(limiter.available_permits(), 0);

        drop(g2);
        assert_eq!(limiter.available_permits(), 1);
    }

    #[tokio::test]
    async fn test_unlimited() {
        let limiter = BindingLimiter::unlimited("test");

        // Should be able to acquire many
        for _ in 0..1000 {
            let _g = limiter.acquire().await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_concurrent_blocking() {
        let limit = BindingLimit::new(0, 1); // 1 concurrent
        let limiter = Arc::new(BindingLimiter::new(&limit, "test"));

        let g1 = limiter.acquire().await.unwrap();

        // Spawn a task that tries to acquire
        let limiter_clone = limiter.clone();
        let handle = tokio::spawn(async move {
            let _g = limiter_clone.acquire().await.unwrap();
            "acquired"
        });

        // Give the spawned task time to block
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Task should still be pending
        assert!(!handle.is_finished());

        // Release our permit
        drop(g1);

        // Now the task should complete
        let result = handle.await.unwrap();
        assert_eq!(result, "acquired");
    }

    #[tokio::test]
    async fn test_reset_allows_reuse() {
        let limit = BindingLimit::new(3, 0); // 3 total, unlimited concurrent
        let limiter = BindingLimiter::new(&limit, "test");

        // Use up the limit
        let _g1 = limiter.acquire().await.unwrap();
        let _g2 = limiter.acquire().await.unwrap();
        let _g3 = limiter.acquire().await.unwrap();

        // 4th should fail
        let result = limiter.acquire().await;
        assert!(matches!(result, Err(LimitError::TotalExceeded { .. })));
        assert_eq!(limiter.count(), 3);

        // Reset the limiter (simulating new request)
        limiter.reset();
        assert_eq!(limiter.count(), 0);

        // Now we can acquire again
        let _g4 = limiter.acquire().await.unwrap();
        let _g5 = limiter.acquire().await.unwrap();
        let _g6 = limiter.acquire().await.unwrap();
        assert_eq!(limiter.count(), 3);

        // And 4th fails again
        let result = limiter.acquire().await;
        assert!(matches!(result, Err(LimitError::TotalExceeded { .. })));
    }
}
