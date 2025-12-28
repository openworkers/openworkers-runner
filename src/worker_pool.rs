use once_cell::sync::Lazy;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Semaphore, oneshot};

// Default pool size configuration
const DEFAULT_WORKER_POOL_SIZE: usize = 1;
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

/// A boxed future that can be sent to worker threads
type BoxedTask = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// Sequential Worker Pool for V8 Isolation Safety
///
/// This pool ensures that only ONE V8 isolate runs per thread at any time.
/// V8 isolates must be dropped in LIFO order, which async interleaving breaks.
/// By processing tasks sequentially per thread, we guarantee isolation safety.
///
/// Architecture:
/// - N threads (one per CPU core)
/// - Each thread has its own channel + LocalSet + single-thread runtime
/// - Tasks are distributed round-robin across threads
/// - Each thread processes ONE task at a time (no interleaving)
///
/// This gives us:
/// - Parallelism = N (number of threads)
/// - Zero V8 conflicts (one isolate per thread at a time)
/// - Thread reuse (no creation/destruction overhead)
pub struct SequentialWorkerPool {
    senders: Vec<flume::Sender<BoxedTask>>,
    next_thread: AtomicUsize,
}

impl SequentialWorkerPool {
    pub fn new(pool_size: usize) -> Self {
        let mut senders = Vec::with_capacity(pool_size);

        for thread_idx in 0..pool_size {
            let (tx, rx) = flume::unbounded::<BoxedTask>();
            senders.push(tx);

            // Spawn a dedicated OS thread for this worker slot
            std::thread::Builder::new()
                .name(format!("v8-worker-{}", thread_idx))
                .spawn(move || {
                    // Create a single-threaded tokio runtime for this thread
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for worker thread");

                    // Create a LocalSet for spawn_local support (needed by V8 runtime)
                    let local = tokio::task::LocalSet::new();

                    // Process tasks sequentially - ONE AT A TIME
                    local.block_on(&rt, async {
                        while let Ok(task_fn) = rx.recv_async().await {
                            // Execute the task and wait for it to complete
                            // before processing the next one
                            let future = task_fn();
                            future.await;
                        }
                    });

                    log::debug!("Worker thread {} shutting down", thread_idx);
                })
                .expect("Failed to spawn worker thread");
        }

        log::info!(
            "Sequential worker pool initialized with {} threads",
            pool_size
        );

        Self {
            senders,
            next_thread: AtomicUsize::new(0),
        }
    }

    /// Spawn a task on the pool, returning when it's queued (not completed)
    ///
    /// The task will be executed on a dedicated thread with its own LocalSet,
    /// ensuring V8 isolation safety. Tasks on the same thread run sequentially.
    pub fn spawn<F, Fut>(&self, task: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        // Round-robin thread selection
        let thread_idx = self.next_thread.fetch_add(1, Ordering::Relaxed) % self.senders.len();

        let boxed_task: BoxedTask = Box::new(move || Box::pin(task()));

        // Send to the selected thread's queue
        if let Err(e) = self.senders[thread_idx].send(boxed_task) {
            log::error!("Failed to send task to worker thread {}: {}", thread_idx, e);
        }
    }

    /// Spawn a task and wait for it to complete
    pub async fn spawn_await<F, Fut, T>(&self, task: F) -> Result<T, oneshot::error::RecvError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();

        self.spawn(move || async move {
            let result = task().await;
            let _ = tx.send(result);
        });

        rx.await
    }
}

/// Global sequential worker pool for executing JavaScript workers
///
/// This pool ensures V8 isolation safety by running only ONE worker
/// per thread at any time. Tasks are distributed round-robin.
pub static WORKER_POOL: Lazy<SequentialWorkerPool> = Lazy::new(|| {
    let pool_size = get_pool_size();
    SequentialWorkerPool::new(pool_size)
});

/// Semaphore to limit queued workers and prevent unbounded queue growth
///
/// This limits the total number of workers (running + queued).
/// With sequential execution, at most pool_size workers run concurrently,
/// but more can be queued. This semaphore prevents queue explosion under load.
///
/// Default: pool_size * 10 (e.g., 8 threads = 80 max queued workers)
pub static WORKER_SEMAPHORE: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let pool_size = get_pool_size();
    let max_queued = std::env::var("MAX_QUEUED_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(pool_size * 10);

    log::info!(
        "Initializing worker semaphore with {} max queued workers",
        max_queued
    );
    Arc::new(Semaphore::new(max_queued))
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
