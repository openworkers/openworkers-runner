# Single-Threaded Event Loop Architecture

## Philosophy: Simple, Scalable, Like Node.js

Instead of one complex multi-threaded process, OpenWorkers uses **N single-threaded instances**:

```
┌─────────────────────────────────────────┐
│         Load Balancer / SO_REUSEPORT    │
│              (nginx / HAProxy)          │
└──────┬──────────┬──────────┬────────────┘
       │          │          │
   ┌───▼───┐  ┌───▼───┐  ┌───▼───┐
   │Runner │  │Runner │  │Runner │
   │  #1   │  │  #2   │  │  #3   │
   │  PID  │  │  PID  │  │  PID  │
   │ 12345 │  │ 12346 │  │ 12347 │
   └───┬───┘  └───┬───┘  └───┬───┘
       │          │          │
    Event      Event      Event
    Loop       Loop       Loop
    (mono)     (mono)     (mono)
       │          │          │
  ┌────▼────┐┌────▼────┐┌────▼────┐
  │Isolate  ││Isolate  ││Isolate  │
  │Pool (16)││Pool (16)││Pool (16)│
  └─────────┘└─────────┘└─────────┘
```

## Per-Instance Architecture

Each runner instance is **completely independent**:

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Single-threaded tokio runtime
    let isolate_pool = IsolatePool::new(POOL_SIZE); // 16-32 isolates

    // HTTP server (single-threaded)
    let listener = TcpListener::bind("0.0.0.0:8080").await?;

    loop {
        let (stream, _) = listener.accept().await?;

        // Handle request in event loop
        tokio::spawn(async move {
            handle_request(stream, &isolate_pool).await
        });
    }
}

async fn handle_request(stream: TcpStream, pool: &IsolatePool) {
    // 1. Acquire isolate from pool
    let mut isolate = pool.acquire().await;

    // 2. Reset state
    isolate.reset();

    // 3. Load worker script
    isolate.evaluate(&worker_script)?;
    isolate.setup_env(&env)?;

    // 4. Execute request
    let response = isolate.trigger_fetch_event(request).await?;

    // 5. Release isolate back to pool
    pool.release(isolate);

    // 6. Send response
    stream.write_all(&response).await?;
}
```

## Key Differences from Multi-Threaded Architecture

| Aspect | Old (Multi-threaded) | New (Single-threaded) |
|--------|---------------------|----------------------|
| **Concurrency Model** | OS threads + thread pool | Event loop + async/await |
| **Threads per instance** | 8 OS + 16 tokio = 24 | 1 (event loop) |
| **Isolate lifecycle** | Create/destroy per request | Pool of reusable isolates |
| **Scaling** | Increase threads (limited) | Increase instances (unlimited) |
| **Complexity** | High (channels, mutexes) | Low (event loop) |
| **Crash isolation** | Thread crash = process crash | Instance crash = others OK |
| **Debugging** | Hard (race conditions) | Easy (sequential) |
| **Memory** | Shared (complex) | Isolated per instance |
| **HTTP Client** | Thread-local (workaround) | Instance-local (natural) |

## Isolate Pool

Each instance maintains a **pool of reusable V8 isolates**:

```rust
pub struct IsolatePool {
    isolates: Vec<Runtime>,
    available: Arc<Mutex<VecDeque<Runtime>>>,
    semaphore: Arc<Semaphore>,
}

impl IsolatePool {
    pub fn new(size: usize) -> Self {
        let mut isolates = Vec::new();
        for _ in 0..size {
            let (runtime, _, _, _) = Runtime::new(None);
            isolates.push(runtime);
        }

        Self {
            available: Arc::new(Mutex::new(isolates.into())),
            semaphore: Arc::new(Semaphore::new(size)),
        }
    }

    pub async fn acquire(&self) -> Runtime {
        // Wait for available isolate
        self.semaphore.acquire().await;

        // Take from pool
        self.available.lock().unwrap().pop_front().unwrap()
    }

    pub fn release(&self, isolate: Runtime) {
        // Return to pool
        self.available.lock().unwrap().push_back(isolate);
        self.semaphore.add_permits(1);
    }
}
```

## Isolate Reuse Flow

```
Request arrives
    ↓
Acquire isolate from pool (wait if all busy)
    ↓
Reset isolate state
    ↓
Load worker script into isolate
    ↓
Setup environment (env vars, bindings)
    ↓
Execute fetch event
    ↓
Return response
    ↓
Release isolate back to pool
```

## Benefits

### 1. Simplicity
- No thread synchronization
- No mutexes (except simple pool)
- No channels between threads
- Sequential execution (easy to reason about)

### 2. Scalability
```bash
# Scale horizontally with docker-compose
docker-compose scale runner=16

# Or with systemd
systemctl start runner@1
systemctl start runner@2
...
```

### 3. Isolation
- Instance crash doesn't affect others
- Memory isolated per process
- Independent health monitoring
- Rolling updates (stop instance, update, start)

### 4. Performance
- **No thread context switching**
- **No thread contention**
- **Isolate reuse** (no create/destroy overhead)
- **Event loop efficiency** (like Node.js)

### 5. Compatibility
- Same model as Node.js cluster mode
- Same model as Cloudflare Workers
- Well-understood patterns
- Standard load balancing

## Deployment

### Docker Compose
```yaml
services:
  runner:
    image: openworkers/runner
    deploy:
      replicas: 8  # 8 instances
    environment:
      - ISOLATE_POOL_SIZE=16
      - PORT=8080
    ports:
      - "8080"  # Load balancer picks
```

### Systemd (systemd socket activation)
```ini
# /etc/systemd/system/runner@.service
[Unit]
Description=OpenWorkers Runner Instance %i

[Service]
Type=simple
ExecStart=/usr/local/bin/openworkers-runner
Environment=ISOLATE_POOL_SIZE=16
Restart=always

[Install]
WantedBy=multi-user.target
```

```bash
# Start 8 instances
for i in {1..8}; do
  systemctl start runner@$i
done
```

### Nginx Load Balancer
```nginx
upstream runners {
    server 127.0.0.1:8081;
    server 127.0.0.1:8082;
    server 127.0.0.1:8083;
    server 127.0.0.1:8084;
    server 127.0.0.1:8085;
    server 127.0.0.1:8086;
    server 127.0.0.1:8087;
    server 127.0.0.1:8088;
}

server {
    listen 80;
    location / {
        proxy_pass http://runners;
    }
}
```

### SO_REUSEPORT (advanced)
```rust
// Multiple processes bind to same port
// Kernel load-balances connections
let listener = TcpListener::bind("0.0.0.0:8080")?;
listener.set_reuse_port(true)?;
```

Then start N processes:
```bash
openworkers-runner &  # Instance 1
openworkers-runner &  # Instance 2
openworkers-runner &  # Instance 3
...
```

Kernel automatically distributes connections.

## Configuration

### Environment Variables

#### `ISOLATE_POOL_SIZE`
Number of reusable V8 isolates per instance.

- **Default:** 16
- **Recommendation:** 16-32 for most workloads
- **Trade-off:** More isolates = more concurrency, but more memory

#### `PORT`
HTTP server port for this instance.

- **Default:** 8080
- **Multi-instance:** Use different ports or SO_REUSEPORT

#### `INSTANCE_ID` (optional)
Identifier for logging/metrics.

- **Default:** Process PID
- **Example:** `INSTANCE_ID=runner-1`

### Removed Variables

These are **no longer needed** with single-threaded architecture:

- ❌ `WORKER_POOL_SIZE` - No thread pool
- ❌ `MAX_QUEUED_WORKERS` - No queue (backpressure via pool)
- ❌ `WORKER_WAIT_TIMEOUT_MS` - No waiting (async)

## Isolate State Reset

Between requests, isolates must be reset to prevent state leakage:

```rust
impl Runtime {
    pub fn reset(&mut self) {
        // 1. Clear global variables
        self.clear_globals();

        // 2. Cancel pending timers
        self.clear_timers();

        // 3. Close open streams
        self.stream_manager.close_all();

        // 4. Reset event listeners
        self.clear_event_listeners();

        // 5. Run garbage collection (optional)
        self.isolate.low_memory_notification();
    }
}
```

**Note:** Some state persistence is acceptable (like Cloudflare/Node.js). The reset is mainly for:
- Cleaning up resources (streams, timers)
- Preventing memory leaks
- Security (clear sensitive data)

## Memory Management

### Per-Isolate Limits
```rust
RuntimeLimits {
    heap_initial_mb: 10,
    heap_max_mb: 128,
    max_cpu_time_ms: 100,
    max_wall_clock_time_ms: 60_000,
}
```

### Per-Instance Memory
```
Instance memory = (ISOLATE_POOL_SIZE × heap_max_mb) + overhead
Example: 16 isolates × 128 MB = 2 GB + ~200 MB overhead = ~2.2 GB
```

### Total Cluster Memory
```
Total = N instances × Instance memory
Example: 8 instances × 2.2 GB = ~17.6 GB
```

## Monitoring

### Per-Instance Metrics
```rust
struct InstanceMetrics {
    isolates_total: usize,
    isolates_available: usize,
    isolates_busy: usize,
    requests_total: u64,
    requests_failed: u64,
    avg_latency_ms: f64,
}
```

### Health Check
```rust
GET /health
{
  "status": "healthy",
  "instance_id": "runner-1",
  "isolates": {
    "total": 16,
    "available": 12,
    "busy": 4
  },
  "uptime_seconds": 3600
}
```

## Error Handling

### Isolate Corruption
If an isolate becomes corrupted:
```rust
match isolate.exec(task).await {
    Ok(result) => {
        // Success - return isolate to pool
        pool.release(isolate);
    }
    Err(e) if e.is_fatal() => {
        // Fatal error - destroy isolate, create new one
        drop(isolate);
        let new_isolate = Runtime::new(None);
        pool.release(new_isolate);
    }
    Err(e) => {
        // Recoverable error - still return isolate
        pool.release(isolate);
    }
}
```

### Instance Crash
- Other instances continue serving traffic
- Load balancer detects health check failure
- Supervisor (systemd/docker) restarts crashed instance
- Graceful degradation (N-1 capacity)

## Performance Expectations

### Single Instance (16 isolates)
- **Concurrency:** 16 requests simultaneously
- **Throughput:** ~1000-2000 req/s (depending on worker complexity)
- **Latency:** ~5-50ms (no isolate creation overhead)

### Cluster (8 instances × 16 isolates)
- **Concurrency:** 128 requests simultaneously
- **Throughput:** ~8000-16000 req/s
- **Latency:** ~5-50ms

### Comparison to Disposable Isolates

| Metric | Old (Disposable) | New (Pooled) | Improvement |
|--------|-----------------|--------------|-------------|
| Isolate overhead | ~3-5ms per request | ~0ms (reused) | ∞ |
| Throughput | ~200-300 req/s | ~1000-2000 req/s | 5-10x |
| Memory efficiency | Low (churn) | High (stable) | 3-5x |
| Scalability | Limited (threads) | Unlimited (instances) | ∞ |

## Migration from Multi-Threaded

### Code Changes
1. Remove `worker_pool.rs`
2. Remove thread-local HTTP client workaround
3. Add `IsolatePool`
4. Add `Runtime::reset()`
5. Change `main()` to `#[tokio::main(flavor = "current_thread")]`

### Deployment Changes
1. Update docker-compose to run N instances
2. Add load balancer (nginx/HAProxy)
3. Update monitoring for per-instance metrics
4. Update health checks

### Testing
```bash
# Start 8 instances locally
for i in {8081..8088}; do
  PORT=$i openworkers-runner &
done

# Nginx balances across them
```

## References

- [Isolate Pooling](./isolate-pooling.md) - Original motivation
- [Node.js Cluster Mode](https://nodejs.org/api/cluster.html) - Similar pattern
- [Cloudflare Workers Architecture](https://blog.cloudflare.com/cloud-computing-without-containers/)
- [tokio current_thread](https://docs.rs/tokio/latest/tokio/runtime/index.html#current-thread-scheduler)

## Files to Create/Modify

- `src/isolate_pool.rs` (new) - Isolate pool implementation
- `src/main.rs` - Single-threaded event loop
- `src/worker_pool.rs` (delete) - No longer needed
- `src/task_executor.rs` - Simplified (no threads)
- `openworkers-runtime-v8/src/runtime/mod.rs` - Add `reset()` method
- `docs/worker-pool.md` (delete/archive) - Outdated
- `docker-compose.yml` - Multi-instance deployment
