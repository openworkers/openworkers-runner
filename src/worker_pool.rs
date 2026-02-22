use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
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

/// Worker Pool with Cooperative Interleaving
///
/// Each thread runs a persistent LocalSet on a single-threaded tokio runtime.
/// Tasks are spawn_local'd onto the LocalSet, so when one task awaits I/O,
/// the runtime polls other tasks on the same thread — cooperative scheduling.
///
/// Architecture:
/// - N threads (one per CPU core)
/// - Each thread has its own channel + LocalSet + single-thread runtime
/// - Tasks are distributed via round-robin for even load distribution
/// - Tasks interleave cooperatively during I/O waits (spawn_local)
///
/// This gives us:
/// - Parallelism = N (number of threads)
/// - Interleaved execution within each thread (no head-of-line blocking)
/// - Thread reuse (no creation/destruction overhead)
pub struct SequentialWorkerPool {
    senders: Vec<tokio::sync::mpsc::UnboundedSender<BoxedTask>>,
    next_thread: AtomicUsize,
}

impl SequentialWorkerPool {
    pub fn new(pool_size: usize) -> Self {
        let mut senders = Vec::with_capacity(pool_size);

        for thread_idx in 0..pool_size {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BoxedTask>();
            senders.push(tx);

            // Spawn a dedicated OS thread for this worker slot
            std::thread::Builder::new()
                .name(format!("v8-worker-{}", thread_idx))
                .spawn(move || {
                    // Single-threaded tokio runtime: the event loop, timers, and I/O
                    // all run on this thread. This enables interleaved execution:
                    // when one task yields at .await, the runtime polls other tasks.
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for worker thread");

                    // Persistent LocalSet enables interleaved execution:
                    // while one task awaits I/O, the LocalSet polls other tasks.
                    // Each task holds its own V8 locker on its own isolate.
                    // NOTE: V8 Lockers call Isolate::Enter() which sets the
                    // thread-local "current isolate". With cooperative scheduling,
                    // multiple Lockers coexist and IsolateGuard (enter/exit per
                    // V8 work block) ensures GetCurrent() returns the correct isolate.
                    rt.block_on(async {
                        let local = tokio::task::LocalSet::new();

                        local
                            .run_until(async {
                                while let Some(task_fn) = rx.recv().await {
                                    let future = task_fn();
                                    tokio::task::spawn_local(future);

                                    // Drain all buffered tasks before yielding to the
                                    // LocalSet, so bursts are dispatched in one batch.
                                    while let Ok(task_fn) = rx.try_recv() {
                                        let future = task_fn();
                                        tokio::task::spawn_local(future);
                                    }
                                }
                            })
                            .await;
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

    // ── Internal dispatch ───────────────────────────────────────────────

    /// Send a boxed task to a specific thread.
    fn send_to(&self, thread_idx: usize, task: BoxedTask) {
        if let Err(e) = self.senders[thread_idx].send(task) {
            tracing::error!("Failed to send task to worker thread {}: {}", thread_idx, e);
        }
    }

    // ── Public API ─────────────────────────────────────────────────────

    /// Spawn a task on the pool (round-robin), returning when it's queued (not completed).
    ///
    /// The task will be executed on a dedicated thread with its own LocalSet,
    /// ensuring V8 isolation safety. Tasks on the same thread interleave
    /// cooperatively during I/O waits.
    pub fn spawn<F, Fut>(&self, task: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let thread_idx = self.next_thread.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        self.send_to(thread_idx, Box::new(move || Box::pin(task())));
    }

    /// Spawn a task (round-robin) and wait for it to complete.
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

/// Global worker pool for executing JavaScript workers
///
/// Tasks interleave cooperatively during I/O waits (spawn_local on LocalSet).
/// Routing: round-robin across all threads for even load distribution.
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

/// Per-worker concurrency limiters
///
/// Maps worker IDs to their semaphores, ensuring a single hot worker
/// cannot monopolize the global worker pool or DB connections.
static WORKER_LIMITERS: Lazy<Mutex<HashMap<String, Arc<Semaphore>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Max concurrent requests per worker (from MAX_CONCURRENT_PER_WORKER env)
static MAX_CONCURRENT_PER_WORKER: Lazy<usize> = Lazy::new(|| {
    std::env::var("MAX_CONCURRENT_PER_WORKER")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
});

/// Get or create a semaphore for a specific worker
pub fn get_worker_semaphore(worker_id: &str) -> Arc<Semaphore> {
    let mut map = WORKER_LIMITERS.lock().unwrap();
    map.entry(worker_id.to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(*MAX_CONCURRENT_PER_WORKER)))
        .clone()
}

/// Get the configured max concurrent requests per worker (for logging)
pub fn get_max_concurrent_per_worker() -> usize {
    *MAX_CONCURRENT_PER_WORKER
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Prove that tasks on the same thread can interleave during I/O waits.
    ///
    /// With sequential execution: total ≈ 5 × 50ms = 250ms
    /// With interleaved execution: total ≈ 50ms (all sleeps overlap)
    ///
    /// Lessons learned (pitfalls that caused false sequential behavior):
    ///
    /// 1. **spawn_await is async**: the channel send() happens on first poll,
    ///    NOT when the function is called. Using sequential `handle.await` in
    ///    a loop sends task N only after task N-1 completes → must use join_all.
    ///
    /// 2. **Channel choice matters**: flume's recv_async() doesn't cooperate
    ///    well with tokio's current_thread runtime inside a LocalSet — it
    ///    delivers one item per event loop tick instead of draining the buffer.
    ///    tokio::sync::mpsc resolves buffered items synchronously.
    ///
    /// 3. **try_recv() drain**: after recv().await wakes up, we drain remaining
    ///    buffered tasks with try_recv() so bursts are dispatched in one batch
    ///    before yielding to the LocalSet.
    #[tokio::test]
    async fn test_interleaved_execution_on_single_thread() {
        let pool = SequentialWorkerPool::new(1);

        let num_tasks = 5u64;
        let io_delay_ms = 50u64;

        let start = Instant::now();

        let mut handles = Vec::new();

        for i in 0..num_tasks {
            let handle = pool.spawn_await(move || async move {
                eprintln!("[{:?}] task {} start", start.elapsed(), i);
                tokio::time::sleep(Duration::from_millis(io_delay_ms)).await;
                eprintln!("[{:?}] task {} done", start.elapsed(), i);
            });
            handles.push(handle);
        }

        // join_all polls all futures concurrently — see pitfall #1 above.
        let results = futures::future::join_all(handles).await;

        for r in results {
            r.unwrap();
        }

        let elapsed = start.elapsed();
        let sequential_time = Duration::from_millis(io_delay_ms * num_tasks);

        // With interleaving, all sleeps overlap → total ≈ 1× delay, not 5×
        assert!(
            elapsed < sequential_time / 2,
            "Expected interleaved execution (< {:?}), got {:?}",
            sequential_time / 2,
            elapsed
        );
    }

    /// Baseline: prove that spawn_local + LocalSet IS concurrent.
    ///
    /// This test isolates the LocalSet behavior from the worker pool dispatch.
    /// If this test passes but the pool test fails, the issue is in the channel
    /// or dispatch mechanism, not in LocalSet/spawn_local itself.
    #[tokio::test]
    async fn test_spawn_local_is_concurrent() {
        let start = Instant::now();
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let mut handles = Vec::new();

                for i in 0..5u64 {
                    handles.push(tokio::task::spawn_local(async move {
                        eprintln!("[{:?}] local {} start", start.elapsed(), i);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        eprintln!("[{:?}] local {} done", start.elapsed(), i);
                    }));
                }

                for h in handles {
                    h.await.unwrap();
                }
            })
            .await;

        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "spawn_local should be concurrent, got {:?}",
            elapsed
        );
    }

    #[test]
    fn test_get_worker_semaphore_creates_and_reuses() {
        let sem1 = get_worker_semaphore("test-worker-1");
        let sem2 = get_worker_semaphore("test-worker-1");
        let sem3 = get_worker_semaphore("test-worker-2");

        // Same worker ID returns same semaphore (Arc pointer equality)
        assert!(Arc::ptr_eq(&sem1, &sem2));

        // Different worker ID returns different semaphore
        assert!(!Arc::ptr_eq(&sem1, &sem3));
    }

    #[tokio::test]
    async fn test_worker_semaphore_limits_concurrency() {
        // The default MAX_CONCURRENT_PER_WORKER is 10 (from env or default)
        let limit = get_max_concurrent_per_worker();
        let sem = get_worker_semaphore("test-limiter-worker");

        // Acquire all permits
        let mut permits = Vec::new();
        for _ in 0..limit {
            let permit = sem.clone().acquire_owned().await.unwrap();
            permits.push(permit);
        }

        // Next acquire should not succeed immediately
        let result =
            tokio::time::timeout(Duration::from_millis(10), sem.clone().acquire_owned()).await;

        assert!(result.is_err(), "Should timeout when all permits are taken");

        // Drop one permit → next acquire should succeed
        permits.pop();

        let result =
            tokio::time::timeout(Duration::from_millis(100), sem.clone().acquire_owned()).await;

        assert!(result.is_ok(), "Should succeed after a permit is released");
    }
}
