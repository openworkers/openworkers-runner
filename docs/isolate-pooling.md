# Isolate Pooling Architecture

## Current Problem: Disposable Isolates

### Current Architecture (Inefficient)

OpenWorkers currently creates and destroys a V8 isolate **for every single request/task**:

```rust
// task_executor.rs - Current flow
WORKER_POOL.spawn_await(move || async move {
    // 1. Create NEW isolate (~3-5ms overhead)
    let mut worker = create_worker(...).await?;

    // 2. Execute ONE task
    let result = run_task_with_timeout(&mut worker, task, ...).await;

    // 3. DESTROY isolate (drop worker)
    result
})
```

**Performance Impact:**
- Isolate creation: ~3-5ms per request (Cloudflare benchmark)
- Memory allocation overhead per request
- No isolate reuse
- Worker startup overhead repeated constantly

**Current Capacity (8 cores):**
- 8 concurrent isolates (one per thread)
- Each isolate is disposable (create → execute → destroy)
- ~200-300 requests/second maximum throughput

### Why This Is Wrong

V8 isolates are designed to be **long-lived and reused**, not disposable. Creating an isolate is expensive:
- Memory allocation for heap
- Context initialization
- Snapshot deserialization
- Built-in JavaScript objects initialization

## Cloudflare Workers Architecture (Efficient)

### How Cloudflare Does It

From Cloudflare's documentation:

> "A single V8 engine can run hundreds or even thousands of isolates concurrently, all within the same OS process, with seamless context switching thousands of times per second."

**Key Insight**: Cloudflare **reuses isolates across requests**:

```
┌─────────────────────────────────────┐
│         Single V8 Process           │
├─────────────────────────────────────┤
│  Isolate 1 (Worker Script A)       │
│    ├─ Request 1 (async)            │
│    ├─ Request 2 (async)            │
│    └─ Request N (async)            │
│                                     │
│  Isolate 2 (Worker Script B)       │
│    ├─ Request 1 (async)            │
│    └─ Request 2 (async)            │
│                                     │
│  Isolate 3 (Worker Script C)       │
│    └─ Request 1 (async)            │
└─────────────────────────────────────┘
```

**Per Isolate:**
- Created **once** when worker is deployed
- Stays **alive indefinitely**
- Handles **multiple concurrent requests** via async event loop (like Node.js)
- Single-threaded async execution within the isolate

**Performance:**
- Isolate startup: Once per worker deployment (not per request)
- ~0ms overhead per request (isolate already warm)
- High density: Hundreds of isolates per process
- Thousands of requests per second per isolate

### Single-Threaded Event Loop

Like Node.js, each Cloudflare Worker isolate runs a **single-threaded event loop**:

```javascript
// All requests execute in the same isolate
addEventListener('fetch', event => {
  event.respondWith(handleRequest(event.request));
});

async function handleRequest(request) {
  // This function may be called concurrently for different requests
  const response = await fetch('https://api.example.com');
  // While this isolate awaits, it can process other requests
  return new Response(response);
}
```

**Concurrency Model:**
- One isolate can handle many concurrent requests
- Async/await allows interleaving (while one request awaits fetch, another runs)
- No thread-per-request overhead
- No isolate-per-request overhead

## Proposed Solution: Persistent Workers with Isolate Reuse

### Architecture Overview

Instead of disposable isolates, create **persistent workers** that stay alive:

```
┌─────────────────────────────────────────────────────┐
│              PersistentWorkerPool                   │
├─────────────────────────────────────────────────────┤
│  Round-robin request distribution                   │
└──────┬──────────┬──────────┬──────────┬─────────────┘
       │          │          │          │
   ┌───▼───┐  ┌───▼───┐  ┌───▼───┐  ┌───▼───┐
   │Worker │  │Worker │  │Worker │  │Worker │
   │   0   │  │   1   │  │   2   │  │   3   │
   └───┬───┘  └───┬───┘  └───┬───┘  └───┬───┘
       │          │          │          │
   Isolate    Isolate    Isolate    Isolate
   (lives)    (lives)    (lives)    (lives)
       │          │          │          │
   Event      Event      Event      Event
   Loop       Loop       Loop       Loop
```

### Persistent Worker Model

```rust
struct PersistentWorker {
    /// V8 isolate (stays alive)
    runtime: JsRuntime,

    /// Request queue
    task_rx: mpsc::Receiver<TaskRequest>,

    /// Worker metadata
    worker_id: String,
    script: Script,
}

impl PersistentWorker {
    async fn run_event_loop(mut self) {
        // Isolate created ONCE at startup
        loop {
            // Receive next task from queue
            let task = self.task_rx.recv().await;

            // Execute in the SAME isolate
            let result = self.execute_task(task).await;

            // Send result back
            task.respond(result);

            // Isolate stays alive for next request
        }
    }

    async fn execute_task(&mut self, task: TaskRequest) -> TaskResult {
        // Reset isolate state (clear globals, etc.)
        self.reset_state();

        // Execute task in existing isolate
        self.runtime.execute_script(&task.script)?;

        // Handle async operations via event loop
        self.runtime.run_event_loop(false).await?;

        // Return result (isolate NOT dropped)
        Ok(result)
    }
}
```

### Key Differences

| Aspect | Current (Disposable) | Proposed (Persistent) |
|--------|---------------------|----------------------|
| **Isolate Lifetime** | Per-request | Long-lived |
| **Startup Overhead** | Every request (~3-5ms) | Once at startup |
| **Memory** | Allocate/deallocate constantly | Allocated once, reused |
| **Throughput** | ~200-300 req/s (8 cores) | ~2000-5000 req/s (8 cores) |
| **Concurrency per Isolate** | 1 (one task) | N (async event loop) |
| **Worker Pool Size** | 8 (one per core) | 8-64 (configurable) |

## Implementation Challenges

### 1. State Isolation Between Requests

**Problem**: If an isolate is reused, global state from one request might leak to another.

**Solution**: Reset isolate state between requests:

```rust
fn reset_state(&mut self) {
    // Clear global variables
    // Reset environment
    // Clean up event listeners
    // GC collection (optional)
}
```

Alternatively, use V8 **Contexts** instead of Isolates:
- One isolate can have multiple contexts
- Each context is isolated
- Cheaper than creating new isolates

### 2. Memory Leaks

**Problem**: Long-lived isolates may accumulate memory if not properly managed.

**Solution**:
- Periodic garbage collection (`isolate.low_memory_notification()`)
- Monitor memory usage per worker
- Kill and restart workers that exceed memory limits
- Use `RuntimeLimits.max_memory_bytes` per execution

### 3. Error Handling

**Problem**: A fatal error in one request shouldn't kill the entire isolate.

**Solution**:
- Catch all errors at request boundary
- Isolate error recovery (reset state)
- If isolate is corrupted (unrecoverable error), restart it
- Health checks per worker

### 4. Graceful Shutdown

**Problem**: Workers may be processing requests during shutdown.

**Solution**:
- Drain mode: Stop accepting new requests
- Wait for in-flight requests to complete (with timeout)
- Gracefully drop isolates after draining

### 5. Request Queuing and Backpressure

**Problem**: If requests come in faster than workers can process, queues grow unbounded.

**Solution**:
- Bounded channels for task queues
- Reject requests when queue is full (backpressure)
- Monitor queue depth per worker

## Implementation Plan

### Phase 1: Core Persistent Worker

1. **Create `PersistentWorker` struct** (`src/persistent_worker.rs`)
   - Owns `JsRuntime` (V8 isolate)
   - Event loop for processing tasks
   - State reset mechanism

2. **Create `PersistentWorkerPool`** (refactor `src/worker_pool.rs`)
   - Initialize N persistent workers at startup
   - Round-robin task distribution
   - Worker health monitoring

3. **Implement task execution**
   - Execute task in existing isolate
   - Handle async operations
   - Return results to caller

### Phase 2: State Isolation

4. **Implement state reset**
   - Clear global variables between tasks
   - Reset environment state
   - Test isolation (one request shouldn't affect another)

5. **Context-based isolation** (optional optimization)
   - Use V8 Contexts instead of Isolates
   - One isolate, multiple contexts
   - Even cheaper than isolate reuse

### Phase 3: Production Hardening

6. **Memory management**
   - Periodic GC
   - Memory limit enforcement
   - Worker restart on memory threshold

7. **Error recovery**
   - Catch errors per request
   - Isolate recovery vs restart
   - Health checks

8. **Metrics and monitoring**
   - Request queue depth
   - Worker utilization
   - Memory usage per worker
   - Request latency distribution

### Phase 4: Advanced Features

9. **Worker warmup**
   - Pre-initialize workers on deployment
   - Keep workers warm (periodic health checks)

10. **Dynamic pool sizing**
    - Scale workers based on load
    - Graceful worker shutdown

## Performance Expectations

### Current (8 cores, disposable isolates)
- 8 concurrent requests maximum
- ~3-5ms startup overhead per request
- ~200-300 requests/second

### Proposed (8 cores, persistent isolates)
- 8+ persistent workers (configurable)
- ~0ms startup overhead per request (isolate already warm)
- ~2000-5000 requests/second (10-20x improvement)

With 64 persistent workers:
- 64 concurrent request handlers
- Each worker can queue multiple requests
- ~10,000+ requests/second potential

## V8 Constraints (Unchanged)

These fundamental constraints still apply:

1. **One thread per isolate at a time** - An isolate can be entered by at most one thread at any given time (V8 hard constraint)

2. **LIFO drop order** - `OwnedIsolate` instances must be dropped in reverse creation order (rusty_v8 constraint)

3. **HandleScope lifetimes** - V8 scopes must not cross async boundaries (RAII safety)

**Important**: These constraints apply to **isolate access**, not to **isolate reuse**. Reusing an isolate for multiple sequential requests doesn't violate any of these constraints.

## References

- [How Workers works · Cloudflare Workers docs](https://developers.cloudflare.com/workers/reference/how-workers-works/)
- [Cloud Computing without Containers - Cloudflare Blog](https://blog.cloudflare.com/cloud-computing-without-containers/)
- [V8 Isolate Documentation](https://v8.github.io/api/head/classv8_1_1Isolate.html)
- rusty_v8: `/Users/max/Documents/forks/rusty_v8/src/isolate.rs`
- deno_core: [JsRuntime Documentation](https://docs.rs/deno_core/latest/deno_core/struct.JsRuntime.html)

## Files to Modify

- `src/persistent_worker.rs` (new) - Persistent worker implementation
- `src/worker_pool.rs` - Refactor to persistent worker pool
- `src/task_executor.rs` - Update to use persistent workers
- `src/worker.rs` - Add state reset methods
- `docs/worker-pool.md` - Update architecture documentation
- `docs/configuration.md` - Add persistent worker configuration

## Configuration

New environment variables:

- `PERSISTENT_WORKER_POOL_SIZE` - Number of persistent workers (default: CPU cores * 2)
- `WORKER_RESTART_MEMORY_THRESHOLD_MB` - Memory threshold to restart worker (default: 256MB)
- `WORKER_HEALTH_CHECK_INTERVAL_MS` - Health check interval (default: 30000)
- `WORKER_TASK_QUEUE_SIZE` - Max queued tasks per worker (default: 100)
