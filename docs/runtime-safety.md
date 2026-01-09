# Runtime Safety Guidelines

## Critical Rules

### ⚠️ Never Use Global Runtime-Bound Resources

Resources that bind to tokio runtimes **MUST** be thread-local:

```rust
// ❌ WRONG: Global client
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::new()
});

// ✅ CORRECT: Thread-local client
thread_local! {
    static HTTP_CLIENT: once_cell::unsync::Lazy<reqwest::Client> =
        once_cell::unsync::Lazy::new(|| reqwest::Client::new());
}
```

**Why?** Each worker thread has its own tokio runtime. A global resource created in one runtime will fail when used in another with "runtime dropped the dispatch task" errors.

### ⚠️ Always Create Fresh LocalSet Per Worker

Never reuse a LocalSet across workers:

```rust
// ❌ WRONG: Shared LocalSet
let local = LocalSet::new();
loop {
    local.run_until(worker()).await;  // spawn_local tasks leak!
}

// ✅ CORRECT: Fresh LocalSet per worker
loop {
    let local = LocalSet::new();
    local.run_until(worker()).await;
    // LocalSet dropped → spawn_local tasks cleaned
}
```

**Why?** spawn_local tasks persist in the LocalSet until it's dropped. Reusing a LocalSet causes task contamination between workers.

### ⚠️ Use once_cell::unsync::Lazy for Thread-Local Initialization

For thread-local lazy initialization, use `once_cell::unsync::Lazy`:

```rust
thread_local! {
    static RESOURCE: once_cell::unsync::Lazy<ResourceType> =
        once_cell::unsync::Lazy::new(|| {
            // Initialize once per thread
            ResourceType::new()
        });
}
```

**Why?** Ensures one-time initialization per thread, created in that thread's runtime context.

## Runtime-Bound Resources

### Common Examples

Resources that bind to tokio runtimes include:

- ✅ `reqwest::Client` - HTTP client
- ✅ `hyper::Client` - Low-level HTTP
- ✅ Database connection pools (tokio-based)
- ✅ gRPC clients
- ✅ Redis clients
- ✅ Any resource using hyper/tokio internally

### Detection

A resource is runtime-bound if:
1. It uses tokio's I/O (TcpStream, timers, etc.)
2. It spawns background tasks
3. Documentation mentions "requires tokio runtime"
4. You see errors like "runtime dropped the dispatch task"

## Worker Execution Context

### Thread Structure

```
Main Process
├─ Worker Thread 0
│  └─ Multi-threaded tokio runtime (2 threads)
│     └─ LocalSet (per worker)
│        └─ V8 Isolate + Worker
├─ Worker Thread 1
│  └─ Multi-threaded tokio runtime (2 threads)
│     └─ LocalSet (per worker)
│        └─ V8 Isolate + Worker
└─ Worker Thread N...
```

Each worker runs in:
1. **Worker thread** - Dedicated OS thread
2. **Tokio runtime** - Multi-threaded (2 worker threads per worker thread)
3. **LocalSet** - Fresh per worker
4. **V8 Isolate** - One at a time per worker thread

### Safe Patterns

#### Pattern 1: Thread-Local HTTP Client

```rust
thread_local! {
    static HTTP_CLIENT: once_cell::unsync::Lazy<reqwest::Client> =
        once_cell::unsync::Lazy::new(|| {
            reqwest::Client::builder()
                .pool_max_idle_per_host(100)
                .build()
                .expect("Failed to create HTTP client")
        });
}

async fn fetch(url: &str) -> Result<Response, Error> {
    let client = HTTP_CLIENT.with(|c| (**c).clone());
    client.get(url).send().await
}
```

#### Pattern 2: Thread-Local Database Pool

```rust
thread_local! {
    static DB_POOL: once_cell::unsync::Lazy<sqlx::PgPool> =
        once_cell::unsync::Lazy::new(|| {
            // Note: Pool creation should be sync
            // Use tokio::runtime::Handle::current().block_on() if needed
            create_pool_sync()
        });
}
```

#### Pattern 3: Fresh LocalSet

```rust
rt.block_on(async {
    while let Ok(task_fn) = rx.recv_async().await {
        // Fresh LocalSet for each worker
        let local = tokio::task::LocalSet::new();
        let future = task_fn();
        local.run_until(future).await;
        // LocalSet dropped here
    }
});
```

## Common Pitfalls

### Pitfall 1: Global Client "Works in Tests"

```rust
// This may work in sequential tests but fails in production
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| reqwest::Client::new());
```

**Why it seems to work:** Sequential tests run workers on the same thread, using the same runtime.

**Why it fails:** Production runs workers on different threads with different runtimes.

**Fix:** Always test with parallel execution:
```bash
cargo test --features v8  # Runs tests in parallel
```

### Pitfall 2: Shared LocalSet

```rust
// Looks efficient but causes task leaks
static LOCAL: Lazy<LocalSet> = Lazy::new(|| LocalSet::new());
```

**Why it seems to work:** First worker succeeds, tasks may complete before next worker.

**Why it fails:** spawn_local tasks from one worker leak into the next.

**Fix:** Create fresh LocalSet per worker.

### Pitfall 3: Runtime Context Assumptions

```rust
// Don't assume you're in a specific runtime
async fn my_function() {
    tokio::spawn(async { /* ... */ });  // Which runtime?
}
```

**Better:**
```rust
async fn my_function() {
    tokio::task::spawn_local(async { /* ... */ });  // Current LocalSet
}
```

## Testing Guidelines

### Test for Runtime Issues

1. **Always run tests in parallel:**
   ```bash
   cargo test --features v8
   ```

2. **Test with multiple workers:**
   ```rust
   #[tokio::test]
   async fn test_concurrent_workers() {
       // Run 10+ workers to stress test
   }
   ```

3. **Watch for these errors:**
   - "runtime dropped the dispatch task"
   - "no reactor running"
   - "spawn failed"
   - Intermittent failures in parallel mode

### Debug Runtime Issues

Add debug logging:
```rust
thread_local! {
    static CLIENT: once_cell::unsync::Lazy<reqwest::Client> =
        once_cell::unsync::Lazy::new(|| {
            eprintln!("Creating client on thread {:?}", std::thread::current().id());
            reqwest::Client::new()
        });
}
```

## Checklist for Adding Runtime-Bound Resources

- [ ] Resource uses thread-local storage (`thread_local!`)
- [ ] Initialization uses `once_cell::unsync::Lazy`
- [ ] Tests run in parallel (`cargo test --features v8`)
- [ ] Tested with 10+ concurrent workers
- [ ] No global static for runtime-bound types
- [ ] Documentation mentions thread-safety

## References

- [HTTP Client Thread-Local Fix](./http-client-thread-local.md)
- [Worker Pool Architecture](./worker-pool.md)
- Tokio docs: [Runtime](https://docs.rs/tokio/latest/tokio/runtime/)
- Tokio docs: [LocalSet](https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html)
