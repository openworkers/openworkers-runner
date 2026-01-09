# Configuration Reference

## Environment Variables

### Worker Pool

#### `WORKER_POOL_SIZE`
Number of worker threads in the pool.

- **Default:** Number of CPU cores (4x in test mode)
- **Example:** `WORKER_POOL_SIZE=16`
- **Impact:** More threads = more parallel workers, but more memory

```rust
let pool_size = std::env::var("WORKER_POOL_SIZE")
    .ok()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or_else(|| {
        let base = std::thread::available_parallelism().unwrap().get();
        if cfg!(test) { base * 4 } else { base }
    });
```

#### `MAX_QUEUED_WORKERS`
Maximum number of workers (running + queued) before backpressure.

- **Default:** `WORKER_POOL_SIZE * 10` (100x in test mode)
- **Example:** `MAX_QUEUED_WORKERS=200`
- **Impact:** Higher = more queued workers before rejecting, but more memory

```rust
let max_queued = std::env::var("MAX_QUEUED_WORKERS")
    .ok()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(pool_size * multiplier);
```

#### `WORKER_WAIT_TIMEOUT_MS`
Timeout in milliseconds for acquiring a worker slot.

- **Default:** `10000` (10 seconds)
- **Example:** `WORKER_WAIT_TIMEOUT_MS=5000`
- **Impact:** Lower = fail faster when pool is saturated

### HTTP Client

#### `HTTP_POOL_MAX_IDLE_PER_HOST`
Maximum idle HTTP connections per host in the connection pool (per worker thread).

- **Default:** `100`
- **Example:** `HTTP_POOL_MAX_IDLE_PER_HOST=200`
- **Impact:**
  - Higher = more connections reused, less connection overhead
  - Lower = less memory, but more connection establishment
- **Per thread:** With 8 worker threads and 100 connections/host = 800 max idle connections

```rust
thread_local! {
    static HTTP_CLIENT: once_cell::unsync::Lazy<reqwest::Client> =
        once_cell::unsync::Lazy::new(|| {
            let pool_size = std::env::var("HTTP_POOL_MAX_IDLE_PER_HOST")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(100);

            reqwest::Client::builder()
                .pool_max_idle_per_host(pool_size)
                .pool_idle_timeout(Duration::from_secs(90))
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client")
        });
}
```

**HTTP Client Fixed Timeouts:**
- `pool_idle_timeout`: 90s
- `connect_timeout`: 5s (DNS + TCP handshake)
- `timeout`: 30s (total request)

### Worker Domains

#### `WORKER_DOMAINS`
Comma-separated list of domains for internal worker-to-worker routing.

- **Default:** Empty (no internal routing)
- **Example:** `WORKER_DOMAINS=workers.internal,*.workers.dev`
- **Impact:** Requests to these domains bypass external HTTP and route internally

```rust
static WORKER_DOMAINS: Lazy<Vec<String>> = Lazy::new(|| {
    std::env::var("WORKER_DOMAINS")
        .map(|s| s.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
        .unwrap_or_else(|_| Vec::new())
});
```

## Runtime Limits

Set per worker via `RuntimeLimits` struct:

```rust
pub struct RuntimeLimits {
    pub max_cpu_time_ms: u64,           // CPU time limit
    pub max_wall_clock_time_ms: u64,    // Wall clock time limit
    pub max_memory_bytes: usize,        // Memory limit
    // ... other limits
}
```

### Defaults

```rust
pub const DEFAULT_CPU_TIME_MS: u64 = 100;

#[cfg(test)]
pub const DEFAULT_WALL_CLOCK_TIME_MS: u64 = 5_000;  // 5s in tests

#[cfg(not(test))]
pub const DEFAULT_WALL_CLOCK_TIME_MS: u64 = 60_000;  // 60s in production
```

## Tuning Guidelines

### High Throughput, Low Latency

```bash
WORKER_POOL_SIZE=16              # More parallel workers
MAX_QUEUED_WORKERS=320           # 20x pool size
HTTP_POOL_MAX_IDLE_PER_HOST=200  # More connection reuse
```

**Trade-off:** Higher memory usage

### Low Memory, Controlled Load

```bash
WORKER_POOL_SIZE=4               # Fewer threads
MAX_QUEUED_WORKERS=20            # 5x pool size
HTTP_POOL_MAX_IDLE_PER_HOST=50   # Fewer idle connections
```

**Trade-off:** Lower throughput, higher latency under load

### Development/Testing

```bash
# Tests use these multipliers by default:
# WORKER_POOL_SIZE = cores * 4
# MAX_QUEUED_WORKERS = pool_size * 100
```

### Production Baseline (8 cores)

```bash
WORKER_POOL_SIZE=8               # Default: 1 per core
MAX_QUEUED_WORKERS=80            # Default: 10x pool size
HTTP_POOL_MAX_IDLE_PER_HOST=100  # Default
WORKER_WAIT_TIMEOUT_MS=10000     # Default: 10s
```

**Capacity:**
- 8 workers in parallel
- 80 total workers (72 queued max)
- 800 HTTP connections per host (8 threads × 100)

## Monitoring

### Key Metrics to Track

```rust
// Active workers
let active = get_active_tasks();

// Is draining?
let draining = is_draining();

// Per-worker stats
let stats = worker.ops.stats;
let fetch_count = stats.fetch_count.load(Ordering::Relaxed);
let fetch_bytes_in = stats.fetch_bytes_in.load(Ordering::Relaxed);
let fetch_bytes_out = stats.fetch_bytes_out.load(Ordering::Relaxed);
```

### Saturation Indicators

1. **Worker pool saturated:**
   - `active_tasks` near `MAX_QUEUED_WORKERS`
   - Increase `MAX_QUEUED_WORKERS` or `WORKER_POOL_SIZE`

2. **HTTP connection pool saturated:**
   - Many "connection refused" or "timeout" errors
   - Increase `HTTP_POOL_MAX_IDLE_PER_HOST`

3. **CPU bound:**
   - Workers hitting `max_cpu_time_ms` limit
   - Increase limit or optimize worker code

4. **Memory bound:**
   - System OOM or swap thrashing
   - Decrease `WORKER_POOL_SIZE` or `MAX_QUEUED_WORKERS`

## Test Mode Behavior

The runner automatically adjusts for test mode:

```rust
if cfg!(test) {
    // 4x more worker threads
    pool_size * 4

    // 100x semaphore capacity
    pool_size * 100

    // Faster timeout (5s vs 60s)
    DEFAULT_WALL_CLOCK_TIME_MS = 5_000
}
```

**Why?** Parallel test execution requires more capacity to prevent test contention and timeouts.

## Files

- `src/worker_pool.rs` - Pool configuration
- `src/ops.rs` - HTTP client configuration
- `src/task_executor.rs` - Runtime limits
