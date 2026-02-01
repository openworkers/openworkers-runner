use once_cell::sync::Lazy;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore, oneshot};

// Default pool size configuration
const DEFAULT_WORKER_POOL_SIZE: usize = 1;
const DEFAULT_WORKER_WAIT_TIMEOUT_MS: u64 = 10_000; // 10 seconds

fn get_pool_size() -> usize {
    std::env::var("WORKER_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| {
            let base_size = std::thread::available_parallelism()
                .ok()
                .map(|n| n.get())
                .unwrap_or(DEFAULT_WORKER_POOL_SIZE);

            // In test mode, increase pool size to reduce LocalSet contention
            // when many tests run in parallel
            if cfg!(test) { base_size * 4 } else { base_size }
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
                    // Create a multi-threaded tokio runtime for this thread
                    // We use worker_threads(2) to prevent potential starvation/deadlock
                    // if one thread is blocked and IO driver needs to run
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for worker thread");

                    // Process tasks sequentially - ONE AT A TIME
                    // IMPORTANT: Create a NEW LocalSet for EACH task to prevent
                    // spawn_local tasks from one worker contaminating the next worker
                    rt.block_on(async {
                        while let Ok(task_fn) = rx.recv_async().await {
                            // Create a fresh LocalSet for this worker
                            // This ensures spawn_local tasks are isolated per worker
                            let local = tokio::task::LocalSet::new();

                            let future = task_fn();

                            // Run the worker in its own LocalSet
                            local.run_until(future).await;

                            // LocalSet is dropped here, cleaning up spawn_local tasks
                        }
                    });

                    tracing::debug!("Worker thread {} shutting down", thread_idx);
                })
                .expect("Failed to spawn worker thread");
        }

        tracing::info!(
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
            tracing::error!("Failed to send task to worker thread {}: {}", thread_idx, e);
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
/// In test mode: pool_size * 100 to handle parallel test execution
pub static WORKER_SEMAPHORE: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let pool_size = get_pool_size();

    let default_multiplier = if cfg!(test) { 100 } else { 10 };

    let max_queued = std::env::var("MAX_QUEUED_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(pool_size * default_multiplier);

    tracing::info!(
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

/// Global draining state
///
/// When set to true, the runner refuses new requests and waits for active tasks to complete.
/// This is used by the firecracker-pool manager to gracefully drain a VM before recycling it.
pub static IS_DRAINING: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

/// Set the draining state
pub fn set_draining(draining: bool) {
    IS_DRAINING.store(draining, Ordering::SeqCst);
    if draining {
        tracing::info!("Runner is now draining - refusing new requests");
    } else {
        tracing::info!("Runner is no longer draining - accepting requests");
    }
}

/// Check if the runner is draining
pub fn is_draining() -> bool {
    IS_DRAINING.load(Ordering::SeqCst)
}

/// Get the number of active tasks
pub fn get_active_tasks() -> usize {
    let max_queued = std::env::var("MAX_QUEUED_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(get_pool_size() * 10);

    max_queued - WORKER_SEMAPHORE.available_permits()
}

/// Notify for task completion events
///
/// This is notified whenever a task completes, allowing the drain monitor
/// to immediately check if all tasks are done instead of polling.
pub static TASK_COMPLETION_NOTIFY: Lazy<Arc<Notify>> = Lazy::new(|| Arc::new(Notify::new()));

/// Notify that a task has completed
///
/// Should be called when releasing the semaphore permit.
pub fn notify_task_completed() {
    TASK_COMPLETION_NOTIFY.notify_waiters();
}

/// Wrapper around OwnedSemaphorePermit that automatically notifies on drop
///
/// This ensures that ALL task types (fetch, scheduled, etc.) properly notify
/// the drain monitor when they complete, without requiring manual calls.
pub struct TaskPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl TaskPermit {
    /// Wrap an OwnedSemaphorePermit with automatic notification on drop
    pub fn new(permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self { _permit: permit }
    }
}

impl Drop for TaskPermit {
    fn drop(&mut self) {
        // Automatically notify drain monitor when task completes
        notify_task_completed();
    }
}
