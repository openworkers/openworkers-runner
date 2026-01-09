# HTTP Client Thread-Local Pattern

## Problem: Runtime Dropped the Dispatch Task

### Symptom
```
reqwest::Error {
  kind: Request,
  source: hyper::Error(User(DispatchGone), "runtime dropped the dispatch task")
}
```

### Root Cause

`reqwest::Client` binds to a tokio runtime **instance** at creation time. When a global client is created in one runtime and used in another, hyper loses its dispatch task.

**Production scenario:**
1. Worker thread A makes first fetch → creates global `HTTP_CLIENT` in its runtime
2. Worker thread B makes fetch → tries to use same client
3. ❌ Runtime mismatch → "runtime dropped the dispatch task" error

Each worker thread has its own tokio runtime (see `worker_pool.rs`):
```rust
std::thread::Builder::new()
    .spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("Failed to create tokio runtime");
        // ...
    })
```

### Solution: Thread-Local HTTP Client

**Before (Broken):**
```rust
// Global client - binds to first runtime that accesses it
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .pool_max_idle_per_host(100)
        .build()
        .expect("Failed to create HTTP client")
});

async fn do_fetch(request: HttpRequest) -> Result<HttpResponse, String> {
    let client = &*HTTP_CLIENT;  // ❌ May be from different runtime!
    // ...
}
```

**After (Fixed):**
```rust
// Thread-local client - each thread creates its own in its own runtime
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

async fn do_fetch(request: HttpRequest) -> Result<HttpResponse, String> {
    // Clone is cheap - reqwest::Client is Arc internally
    let client = HTTP_CLIENT.with(|c| (**c).clone());  // ✅ Thread-local!
    // ...
}
```

## Why This Works

- Each worker thread creates its own `reqwest::Client` in its own tokio runtime
- The client is bound to the correct runtime from creation
- `thread_local!` ensures one client per thread (lazy initialization)
- Cloning `reqwest::Client` is cheap (Arc internally)
- No runtime mismatch → hyper dispatch task stays alive
- Connection pooling still works (per-thread pools)

## Performance Impact

**Minimal** - One client per worker thread (not per worker or per request):
- 8 CPU cores → 8 worker threads → 8 HTTP clients
- Each client: 100 connections/host pool (configurable)
- Total: 800 idle connections max across all threads
- `reqwest::Client::clone()` is O(1) Arc increment

## General Pattern for Runtime-Bound Resources

Any resource that binds to a tokio runtime should use this pattern:

```rust
thread_local! {
    static RESOURCE: once_cell::unsync::Lazy<ResourceType> =
        once_cell::unsync::Lazy::new(|| {
            // Create resource here - runs in current thread's runtime
            ResourceType::new()
        });
}

// Usage:
fn use_resource() {
    RESOURCE.with(|r| {
        let owned = r.clone();  // Clone if needed for async
        // ...
    });
}
```

## Examples of Runtime-Bound Resources

- ✅ `reqwest::Client` - HTTP client
- ✅ Database connection pools (may need runtime)
- ✅ gRPC clients
- ✅ Any resource using hyper/tokio internally

## Testing

This bug manifested in parallel tests where multiple workers ran simultaneously on different threads. Always test with:

```bash
cargo test --features v8  # Run all tests in parallel
```

Individual tests may pass even with a global client if they run sequentially.

## References

- `src/ops.rs` - HTTP client implementation
- `src/worker_pool.rs` - Worker thread pool with separate runtimes
- Tests: `tests/fetch_streaming_test.rs`
