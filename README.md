# OpenWorkers runner

OpenWorkers is a runtime for running javascript code in a serverless environment.

This runner manages instances of [OpenWorkers Runtime](https://github.com/openworkers/openworkers-runtime-v8).

## Usage

### Build

The runner supports multiple JavaScript runtime backends via feature flags.

**V8 is the recommended runtime** for production use.

```bash
# V8 runtime (recommended)
cargo build --release --features v8

# Alternative runtimes (experimental)
cargo build --release --features quickjs   # QuickJS - lightweight
cargo build --release --features boa       # Boa - pure Rust
cargo build --release --features jsc       # JavaScriptCore (macOS/iOS)
cargo build --release --features deno      # Deno - legacy
```

Available features:

- `v8` - **Recommended** - Standalone V8 engine with full Web API support
- `quickjs` - Lightweight QuickJS engine
- `boa` - Pure Rust JS engine (experimental)
- `jsc` - JavaScriptCore (Apple platforms)
- `deno` - Full Deno runtime (legacy, not actively maintained)

### Snapshot the runtime (V8 only)

```bash
cargo run --features v8 --bin snapshot
```

### Prepare the database

```sql
CREATE USER openworkers WITH PASSWORD 'password';
CREATE DATABASE openworkers WITH OWNER openworkers;
```

### Create .env file

```bash
DATABASE_URL='postgres://openworkers:password@localhost:5432/openworkers'
NATS_SERVERS='nats://localhost:4222'
```

### Environment Variables

#### Required

| Variable       | Description                  |
| -------------- | ---------------------------- |
| `DATABASE_URL` | PostgreSQL connection string |
| `NATS_SERVERS` | NATS server URL              |

#### Networking

| Variable                      | Default         | Description                                                 |
| ----------------------------- | --------------- | ----------------------------------------------------------- |
| `WORKER_DOMAINS`              | `workers.rocks` | Comma-separated list of worker domains for internal routing |
| `HTTP_POOL_MAX_IDLE_PER_HOST` | `100`           | Max idle HTTP connections per host (for worker `fetch()`)   |

#### V8 Runtime

| Variable                 | Default   | Description                                      |
| ------------------------ | --------- | ------------------------------------------------ |
| `V8_EXECUTE`             | `PINNED`  | Execution mode: `PINNED`, `POOLED`, or `ONESHOT` |
| `SNAPSHOT_CACHE_MAX`     | `5000`    | Max entries in the in-memory code cache LRU      |
| `WORKER_POOL_SIZE`       | CPU cores | Number of V8 worker threads                      |
| `MAX_QUEUED_WORKERS`     | pool × 10 | Max queued tasks before backpressure             |
| `WORKER_WAIT_TIMEOUT_MS` | `10000`   | Timeout (ms) waiting for a worker slot           |

`V8_EXECUTE` modes:

##### `PINNED` (default)

Thread-local isolate pools — each thread maintains its own pool of V8 isolates, keyed by tenant (`user_id`). Zero cross-thread contention. Multiple isolates can exist per tenant for concurrent requests. Includes backpressure via per-thread queue with configurable size and timeout.

A new V8 context is created per request, so no JS state leaks between requests. The isolate (engine, heap, GC) is reused to avoid the allocation cost.

##### `POOLED`

Single global LRU pool shared across all threads, protected by a mutex. Isolates are keyed by `worker_id`. Simpler model but higher contention under load since all threads compete for the same lock.

##### `ONESHOT`

Fresh V8 isolate per request, destroyed after each response. No reuse, no pooling. Slower (~1-2ms overhead per request) but useful for debugging. Also serves as a workaround for a V8 SIGSEGV (`SEGV_PKUERR`) that affects PINNED and POOLED modes in some containerized environments (see [#2](https://github.com/openworkers/openworkers-runner/issues/2)).

#### Telemetry (OpenTelemetry)

| Variable            | Default              | Description                                |
| ------------------- | -------------------- | ------------------------------------------ |
| `OTLP_ENDPOINT`     | -                    | OTLP exporter endpoint (enables telemetry) |
| `OTLP_SERVICE_NAME` | `openworkers-runner` | Service name reported to OTLP              |
| `OTLP_HEADERS`      | -                    | Extra headers for OTLP exporter            |

#### NATS Authentication

| Variable           | Default | Description                   |
| ------------------ | ------- | ----------------------------- |
| `NATS_CREDENTIALS` | -       | Path to NATS credentials file |

#### Internal Routing (`WORKER_DOMAINS`)

When a worker calls `fetch()` to a URL matching `*.{domain}`, the request is routed internally instead of going through DNS and external network. This improves latency and avoids external bandwidth costs.

```javascript
// These are routed internally (no DNS lookup):
fetch("https://my-api.workers.rocks/endpoint");

// This goes through external network:
fetch("https://example.com/api");
```

Configure for your environment:

```bash
# Production (default)
WORKER_DOMAINS=workers.rocks

# Local development
WORKER_DOMAINS=workers.dev.localhost

# Both
WORKER_DOMAINS=workers.rocks,workers.dev.localhost
```

### Run

```bash
export RUST_LOG=openworkers_runtime=debug,openworkers_runner=debug # Optional

cargo run --features v8
```

### Install sqlx-cli (optional - only for development)

```bash
cargo install sqlx-cli --no-default-features --features rustls,postgres
```

#### Prepare the database

```bash
cargo sqlx prepare
```

## Known Issues

### temporal_rs build failure with Deno runtime

When building with the `deno` feature (default), you may encounter a build error with `temporal_rs`:

```
error: unexpected end of macro invocation
  --> temporal_rs-0.0.11/src/tzdb.rs:60:1
   |
60 | timezone_provider::iana_normalizer_singleton!();
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ missing tokens in macro arguments
```

**Workaround:** Pin `timezone_provider` to version 0.0.13:

```bash
cargo update -p timezone_provider@0.0.16 --precise 0.0.13
```

This is a known upstream issue with `temporal_rs` and newer versions of `timezone_provider`. The `Cargo.lock` file should preserve this fix for subsequent builds.
