# Worker Pool Architecture

> ⚠️ **Note**: This document describes the **current disposable isolate architecture**. This architecture is inefficient because it creates and destroys a V8 isolate for every request. See [Isolate Pooling](./isolate-pooling.md) for the proposed persistent worker architecture that reuses isolates (like Cloudflare Workers).

## Overview

The `SequentialWorkerPool` ensures V8 isolation safety by running **one worker at a time per thread**, while enabling parallelism through **multiple worker threads**.

**Current Limitation**: Each "worker" is a disposable V8 isolate that is created, executes one task, then is destroyed. This is inefficient compared to Cloudflare's approach of reusing isolates across multiple requests.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                 SequentialWorkerPool                │
├─────────────────────────────────────────────────────┤
│  Round-robin task distribution                      │
└──────┬──────────┬──────────┬──────────┬─────────────┘
       │          │          │          │
   ┌───▼───┐  ┌───▼───┐  ┌───▼───┐  ┌───▼───┐
   │Thread │  │Thread │  │Thread │  │Thread │
   │   0   │  │   1   │  │   2   │  │   3   │
   └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘
       │          │          │          │
   ┌───▼───────────▼──────────▼──────────▼───┐
   │    Multi-threaded tokio runtime (2 threads) │
   │    + LocalSet for spawn_local support    │
   └──────────────────────────────────────────┘
```

## Components

### Worker Threads

Each worker thread is a dedicated OS thread with:
- **Own tokio runtime** (`new_multi_thread` with 2 worker threads)
- **Sequential execution** - processes one worker at a time
- **Fresh LocalSet per worker** - prevents task contamination

```rust
std::thread::Builder::new()
    .name(format!("v8-worker-{}", thread_idx))
    .spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            while let Ok(task_fn) = rx.recv_async().await {
                // Fresh LocalSet per worker - isolation!
                let local = tokio::task::LocalSet::new();
                let future = task_fn();
                local.run_until(future).await;
                // LocalSet dropped here, cleaning up spawn_local tasks
            }
        });
    })
```

## V8 Isolation Safety

### Why One Worker at a Time?

**Clarification**: "One worker at a time per thread" means we process workers **sequentially within each thread**, not globally. With 8 threads, we can run **8 workers in parallel** (one per thread).

**The Real Problem**: Each "worker" is a fresh isolate that's immediately destroyed after execution. This is the inefficiency, not the sequential processing. See [Isolate Pooling](./isolate-pooling.md) for the solution (persistent workers that handle multiple requests).

V8 isolates must be dropped in **LIFO order** (Last In, First Out). Async interleaving breaks this:

```rust
// ❌ BROKEN: Async interleaving
async {
    let isolate1 = create_isolate();  // Created first
    let isolate2 = create_isolate();  // Created second
    // If isolate1 drops first → CRASH
}

// ✅ CORRECT: Sequential execution
async {
    let isolate1 = create_isolate();
    drop(isolate1);  // LIFO: dropped before next created

    let isolate2 = create_isolate();
    drop(isolate2);
}
```

By processing tasks **one at a time per thread**, we guarantee LIFO drop order.

### LocalSet Isolation

Each worker gets a **fresh LocalSet** to prevent spawn_local contamination:

```rust
// Without fresh LocalSet (❌ BROKEN):
let local = LocalSet::new();
loop {
    local.run_until(worker1()).await;  // spawn_local tasks persist
    local.run_until(worker2()).await;  // ← sees worker1's tasks!
}

// With fresh LocalSet per worker (✅ CORRECT):
loop {
    let local = LocalSet::new();       // Fresh!
    local.run_until(worker1()).await;
    // LocalSet dropped → spawn_local tasks cleaned up
}
```

## Parallelism

While each thread processes workers **sequentially**, multiple threads provide **parallelism**:

- 8 CPU cores → 8 worker threads
- 8 workers can run **in parallel** (one per thread)
- No V8 conflicts (one isolate per thread at a time)

## Task Distribution

Workers are distributed **round-robin** across threads:

```rust
let thread_idx = self.next_thread.fetch_add(1, Ordering::Relaxed) % self.senders.len();
```

This ensures even load distribution across worker threads.

## Backpressure

The `WORKER_SEMAPHORE` limits total concurrent workers (running + queued):

```rust
pub static WORKER_SEMAPHORE: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let pool_size = get_pool_size();
    let multiplier = if cfg!(test) { 100 } else { 10 };
    let max_queued = pool_size * multiplier;
    Arc::new(Semaphore::new(max_queued))
});
```

- Production: `pool_size * 10` (e.g., 80 for 8 cores)
- Tests: `pool_size * 100` (for parallel test execution)

## Configuration

### Environment Variables

- `WORKER_POOL_SIZE` - Number of worker threads (default: CPU cores, 4x in tests)
- `MAX_QUEUED_WORKERS` - Semaphore limit (default: pool_size * 10, 100x in tests)

### Test Mode Adjustments

```rust
fn get_pool_size() -> usize {
    // In test mode, increase pool size to reduce contention
    if cfg!(test) {
        base_size * 4
    } else {
        base_size
    }
}
```

## Graceful Shutdown

### Draining State

```rust
pub static IS_DRAINING: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

pub fn set_draining(draining: bool) {
    IS_DRAINING.store(draining, Ordering::SeqCst);
}
```

When draining:
1. Refuse new requests
2. Wait for active tasks to complete
3. Task completion notifies drain monitor

### Task Permits

`TaskPermit` automatically notifies on drop:

```rust
pub struct TaskPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for TaskPermit {
    fn drop(&mut self) {
        notify_task_completed();  // Automatic notification
    }
}
```

## Performance Characteristics

- **Parallelism**: N (number of worker threads)
- **Per-thread concurrency**: 1 (sequential)
- **Total capacity**: pool_size * multiplier
- **Thread reuse**: Yes (no creation/destruction overhead)
- **V8 safety**: 100% (one isolate per thread at a time)

## Files

- `src/worker_pool.rs` - Worker pool implementation
- `src/task_executor.rs` - Task execution logic
- Tests: `tests/fetch_streaming_test.rs`, `tests/worker_tests.rs`
