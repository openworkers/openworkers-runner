# Isolate + Context Architecture (Cloudflare-style)

## The Key Insight: Contexts are Cheap, Isolates are Expensive

### V8 Hierarchy
```
V8 Isolate (~3-5ms to create, ~128MB memory)
├─ Context 1 (~100µs to create, ~10KB memory)
│  └─ Global scope for Worker A
├─ Context 2 (~100µs to create, ~10KB memory)
│  └─ Global scope for Worker B
└─ Context 3 (~100µs to create, ~10KB memory)
   └─ Global scope for Worker C
```

**Key Properties:**
- **Isolate** = V8 engine instance (heap, GC, JIT compiler)
- **Context** = Isolated global scope (window, addEventListener, etc.)
- Multiple contexts can **share one isolate**
- Contexts are **completely isolated** from each other
- Context creation is **30-50x cheaper** than isolate creation

## Architecture

### Previous Approach (Disposable Isolates - WRONG)
```rust
for request in requests {
    let isolate = create_isolate();  // ❌ 3-5ms overhead
    let context = create_context(&isolate);
    execute_script(&context, &worker_script);
    execute_request(&context, request);
    drop(context);
    drop(isolate);  // ❌ Expensive!
}
```

**Problems:**
- Isolate creation: 3-5ms per request
- Memory allocation/deallocation churn
- ~200-300 requests/second maximum

### New Approach (Reusable Isolates + Disposable Contexts - CORRECT)
```rust
// Create isolate pool once at startup (8-16 isolates)
let isolate_pool = IsolatePool::new(8);

for request in requests {
    let isolate = isolate_pool.acquire().await;  // ✅ Already created

    let context = create_context(&isolate);  // ✅ 100µs overhead
    execute_script(&context, &worker_script);
    execute_request(&context, request);
    drop(context);  // ✅ Cheap cleanup

    isolate_pool.release(isolate);  // ✅ Reuse
}
```

**Benefits:**
- Isolate creation: Once at startup
- Context creation: ~100µs per request (30-50x faster)
- Memory stable (no churn)
- ~10,000+ requests/second potential

## Implementation

### Current Runtime Structure (Single Context)
```rust
pub struct Runtime {
    pub isolate: v8::OwnedIsolate,
    pub context: v8::Global<v8::Context>,  // ❌ Single context, created once
    // ... channels, callbacks, etc.
}
```

**Problem:** Context is baked into Runtime, can't create multiple contexts.

### New Structure (Multi-Context Support)

#### Option A: Separate Isolate and Context
```rust
/// Reusable isolate with infrastructure
pub struct IsolateRuntime {
    pub isolate: v8::OwnedIsolate,
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
    // Shared infrastructure (event loop channels, etc.)
}

impl IsolateRuntime {
    /// Create a new context for executing a worker script
    pub fn create_context(&mut self) -> ExecutionContext {
        let scope = v8::HandleScope::new(&mut self.isolate);
        let context = v8::Context::new(&scope);

        ExecutionContext {
            context: v8::Global::new(&scope, context),
            isolate: &mut self.isolate,
        }
    }
}

/// Disposable execution context
pub struct ExecutionContext<'a> {
    context: v8::Global<v8::Context>,
    isolate: &'a mut v8::OwnedIsolate,
}

impl ExecutionContext<'_> {
    pub fn evaluate(&mut self, code: &str) -> Result<(), String> {
        let scope = v8::HandleScope::new(self.isolate);
        let context = v8::Local::new(&scope, &self.context);
        let scope = v8::ContextScope::new(&scope, context);
        // ... execute code in this context
    }
}
```

#### Option B: Runtime with Context Factory (Simpler)
```rust
pub struct Runtime {
    pub isolate: v8::OwnedIsolate,
    // NO fixed context - create per-request
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
}

impl Runtime {
    /// Create a new context and execute script in it
    pub fn execute_in_new_context<F, R>(
        &mut self,
        script: &Script,
        f: F,
    ) -> Result<R, TerminationReason>
    where
        F: FnOnce(&mut ContextScope) -> Result<R, TerminationReason>,
    {
        // Create fresh context
        let scope = v8::HandleScope::new(&mut self.isolate);
        let context = v8::Context::new(&scope);
        let scope = v8::ContextScope::new(&scope, context);

        // Setup globals (addEventListener, fetch, etc.)
        setup_globals(&scope, context)?;

        // Evaluate user script
        evaluate_script(&scope, &script.code)?;

        // Execute callback (trigger fetch event, etc.)
        f(&scope)
    }
}
```

## Detailed Flow

### Startup
```rust
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Create pool of 8 isolates
    let pool = IsolatePool::new(8, RuntimeLimits::default());

    // Each isolate is created once with full setup
    // Ready to create contexts
}
```

### Per-Request
```rust
async fn handle_request(
    worker_script: Script,
    request: HttpRequest,
    pool: &IsolatePool,
) -> Result<HttpResponse, Error> {
    // 1. Acquire isolate from pool (wait if all busy)
    let mut isolate_runtime = pool.acquire().await;

    // 2. Create NEW context for this request (~100µs)
    let context = isolate_runtime.create_context();

    // 3. Setup globals in context (addEventListener, etc.)
    context.setup_globals()?;

    // 4. Evaluate worker script in context
    context.evaluate(&worker_script.code)?;
    context.setup_env(&worker_script.env)?;

    // 5. Trigger fetch event in context
    let response = context.trigger_fetch_event(request).await?;

    // 6. Drop context (cleanup globals, free ~10KB)
    drop(context);

    // 7. Release isolate back to pool (reuse for next request)
    pool.release(isolate_runtime).await;

    Ok(response)
}
```

### Context Isolation Example
```rust
// Request 1: Worker A
{
    let mut isolate = pool.acquire().await;
    let ctx = isolate.create_context();
    ctx.evaluate("addEventListener('fetch', () => new Response('A'))");
    ctx.trigger_fetch().await;  // Returns "A"
    drop(ctx);  // ← Global scope cleaned up
    pool.release(isolate).await;
}

// Request 2: Worker B (same isolate, different context)
{
    let mut isolate = pool.acquire().await;
    let ctx = isolate.create_context();
    ctx.evaluate("addEventListener('fetch', () => new Response('B'))");
    ctx.trigger_fetch().await;  // Returns "B" (not "A"!)
    drop(ctx);  // ← Global scope cleaned up
    pool.release(isolate).await;
}
```

**Worker A's `addEventListener` does NOT leak into Worker B!**

## Memory Management

### Per-Isolate Memory
```
Isolate = ~128 MB (heap_max_mb)
  ├─ V8 heap: ~128 MB
  ├─ JIT code cache: ~10 MB
  ├─ Internal structures: ~10 MB
  └─ Total: ~150 MB
```

### Per-Context Memory
```
Context = ~10 KB
  ├─ Global object: ~5 KB
  ├─ Built-in prototypes: ~3 KB
  ├─ Internal structures: ~2 KB
  └─ Total: ~10 KB
```

### Total Memory (8 isolates, 64 concurrent requests)
```
Isolates: 8 × 150 MB = 1.2 GB
Contexts: 64 × 10 KB = 640 KB
Total: ~1.2 GB (contexts are negligible!)
```

## Performance Comparison

| Metric | Disposable Isolates | Disposable Contexts |
|--------|-------------------|---------------------|
| **Startup overhead** | 3-5ms per request | ~100µs per request |
| **Memory per request** | 150 MB | 10 KB |
| **Throughput** | ~200-300 req/s | ~10,000 req/s |
| **Scalability** | Limited (memory) | Unlimited (cheap) |
| **Improvement** | 1x (baseline) | **30-50x faster** |

## V8 API Changes Needed

### Current Code (Single Context)
```rust
// openworkers-runtime-v8/src/runtime/mod.rs
impl Runtime {
    pub fn new() -> Self {
        let mut isolate = v8::Isolate::new(params);

        // Context created ONCE, baked into Runtime
        let context = {
            let scope = v8::HandleScope::new(&mut isolate);
            let context = v8::Context::new(&scope);
            v8::Global::new(&scope, context)  // ← Stored forever
        };

        Self { isolate, context, /* ... */ }
    }

    pub fn evaluate(&mut self, code: &str) -> Result<(), String> {
        // Always uses self.context
        let scope = v8::HandleScope::new(&mut self.isolate);
        let context = v8::Local::new(&scope, &self.context);
        let scope = v8::ContextScope::new(&scope, context);
        // ...
    }
}
```

### New Code (Multi-Context)
```rust
impl Runtime {
    pub fn new() -> Self {
        let mut isolate = v8::Isolate::new(params);
        // NO context created yet
        Self { isolate, /* ... */ }
    }

    /// Create a new context for one-time use
    pub fn with_new_context<F, R>(&mut self, f: F) -> Result<R, String>
    where
        F: FnOnce(&mut v8::ContextScope) -> Result<R, String>,
    {
        // Create fresh context
        let scope = v8::HandleScope::new(&mut self.isolate);
        let context = v8::Context::new(&scope);
        let mut scope = v8::ContextScope::new(&scope, context);

        // Execute callback in this context
        let result = f(&mut scope)?;

        // Context dropped here, scope cleaned up
        Ok(result)
    }
}
```

## Refactoring Plan

### Phase 1: Isolate/Context Separation
1. Remove `context: v8::Global<v8::Context>` from `Runtime` struct
2. Add `Runtime::with_new_context()` method
3. Refactor all `Runtime` methods to take `&mut ContextScope` parameter
4. Update `Worker` to create context per execution

### Phase 2: Isolate Pool
1. Create `IsolatePool` that manages `Runtime` instances
2. `IsolatePool::acquire()` returns a reusable `Runtime`
3. `IsolatePool::release()` returns `Runtime` to pool
4. Each request acquires isolate, creates context, releases isolate

### Phase 3: Single-Threaded Event Loop
1. Remove `worker_pool.rs` (no more threads)
2. Update `main.rs` to use `#[tokio::main(flavor = "current_thread")]`
3. Single event loop handles all requests
4. Scale by running multiple instances

## Testing

### Context Isolation Test
```rust
#[tokio::test]
async fn test_context_isolation() {
    let mut runtime = Runtime::new(None);

    // Context 1: Set global variable
    runtime.with_new_context(|scope| {
        scope.evaluate("globalThis.test = 123")?;
        let value = scope.get_global("test")?;
        assert_eq!(value, 123);
        Ok(())
    }).unwrap();

    // Context 2: Should NOT see Context 1's variable
    runtime.with_new_context(|scope| {
        let value = scope.get_global("test")?;
        assert_eq!(value, undefined);  // ✅ Isolated!
        Ok(())
    }).unwrap();
}
```

### Performance Benchmark
```rust
#[tokio::test]
async fn bench_context_creation() {
    let mut runtime = Runtime::new(None);

    let start = Instant::now();
    for _ in 0..1000 {
        runtime.with_new_context(|scope| {
            scope.evaluate("1 + 1")?;
            Ok(())
        }).unwrap();
    }
    let elapsed = start.elapsed();

    // Should be ~100ms for 1000 contexts
    // vs ~3000ms for 1000 isolates
    assert!(elapsed.as_millis() < 500);
}
```

## Benefits Summary

✅ **30-50x faster** request startup (100µs vs 3-5ms)
✅ **10x higher throughput** (~10k vs ~300 req/s)
✅ **Perfect isolation** between requests (separate contexts)
✅ **Low memory overhead** (10KB vs 150MB per request)
✅ **Cloudflare-compatible** architecture
✅ **Simple single-threaded** event loop
✅ **Horizontal scaling** via multiple instances

## References

- [V8 Contexts Documentation](https://v8.dev/docs/embed#contexts)
- [Cloudflare Workers: How They Work](https://blog.cloudflare.com/cloud-computing-without-containers/)
- [V8 Memory Management](https://v8.dev/blog/trash-talk)
- rusty_v8: [Context API](https://docs.rs/v8/latest/v8/struct.Context.html)
