use once_cell::sync::Lazy;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::task::LocalPoolHandle;

// Default pool size configuration
const DEFAULT_WORKER_POOL_SIZE: usize = 1;
const DEFAULT_WORKERS_PER_THREAD: usize = 10;
const DEFAULT_WORKER_WAIT_TIMEOUT_MS: u64 = 10_000; // 10 seconds

fn get_pool_size() -> usize {
    std::env::var("WORKER_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .map(|n| n.get())
                .unwrap_or(DEFAULT_WORKER_POOL_SIZE)
        })
}

fn get_max_workers() -> usize {
    std::env::var("MAX_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| {
            // Default: 10 workers per thread
            // This works well for I/O-bound JavaScript workers
            get_pool_size() * DEFAULT_WORKERS_PER_THREAD
        })
}

/// Global worker pool for executing JavaScript workers
///
/// This pool uses a fixed number of threads (based on CPU cores) to execute
/// JavaScript workers. Each thread has its own LocalSet to support !Send futures
/// (required by V8/deno_core).
///
/// Benefits:
/// - Prevents unlimited thread spawning under high load
/// - Better resource utilization
/// - Automatic load balancing across threads
pub static WORKER_POOL: Lazy<LocalPoolHandle> = Lazy::new(|| {
    let pool_size = get_pool_size();
    log::info!("Initializing worker pool with {} threads", pool_size);
    LocalPoolHandle::new(pool_size)
});

/// Semaphore to limit concurrent workers and prevent queue buildup
///
/// This limits the number of concurrent JavaScript workers, not threads.
/// By default, allows 10 workers per thread (e.g., 8 threads = 80 concurrent workers).
///
/// This works well because JavaScript workers are I/O-bound and spend most of their
/// time waiting on async operations (fetch, database queries, etc.), allowing a
/// single thread to multiplex many workers efficiently.
///
/// When saturated, requests will wait up to 10 seconds for a slot to become available
/// before receiving a 503 Service Unavailable response.
pub static WORKER_SEMAPHORE: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let max_workers = get_max_workers();
    log::info!(
        "Initializing worker semaphore with {} max concurrent workers",
        max_workers
    );
    Arc::new(Semaphore::new(max_workers))
});

/// Timeout for waiting on a worker slot
pub fn get_worker_wait_timeout() -> Duration {
    Duration::from_millis(
        std::env::var("WORKER_WAIT_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_WORKER_WAIT_TIMEOUT_MS),
    )
}
