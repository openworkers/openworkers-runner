# OpenWorkers runner

OpenWorkers is a runtime for running javascript code in a serverless environment.

This runner manages instances of [OpenWorkers Runtime](https://github.com/openworkers/openworkers-runtime-v8).

## Single-runner deployment

Run one runner per platform database. It serves public HTTP and TLS, executes
workers and cron events, and stores and streams console logs. Do not start the
standalone logs or scheduler services with it. Nginx is not required.

| Setting                                | Default                                                                      | Purpose                                        |
| -------------------------------------- | ---------------------------------------------------------------------------- | ---------------------------------------------- |
| `HTTP_ADDR`                            | `0.0.0.0:8081`                                                               | Public HTTP listener                           |
| `HTTPS_ADDR`                           | `0.0.0.0:8443`                                                               | Public TLS listener                            |
| `HTTP_TLS_CERTIFICATE`, `HTTP_TLS_KEY` | unset                                                                        | PEM files; set both to enable HTTPS and HTTP/2 |
| `INBOUND_ALLOWLIST_FILE`               | unset                                                                        | Public peer IP/CIDR allowlist                  |
| `DASHBOARD_HOSTS`                      | `dash.openworkers.com,dash.openworkers.dev,dash.dev.localhost,dash.dev.kube` | Hosts served by the API worker                 |
| `API_WORKER_NAME`                      | `openworkers-api`                                                            | Dashboard and API worker                       |
| `WORKER_DOMAINS`                       | unset                                                                        | Worker name or UUID subdomains                 |

Other hosts use the database domain and project routes. Public requests cannot
set the worker selector or internal routing headers. The internal listener
`127.0.0.1:8080` serves bindings and local administration; do not publish it.
Client IP headers reflect the TCP peer. Forwarded headers from external proxies
are not trusted. HTTP serves requests directly; it does not redirect to HTTPS.
HTTP/2 uses TLS with ALPN; cleartext listeners only accept HTTP/1.1.
Certificate and allowlist changes require a restart. There is no certificate provisioning.

The optional allowlist contains one IPv4/IPv6 address or CIDR per line; blank
lines and `#` comments are accepted. An unset file allows all peers, an empty
file denies all public peers, and an unreadable or invalid file prevents startup.
Filtering uses the TCP peer before TLS or HTTP parsing. The internal loopback
listener is exempt. The infra production overlay supplies Cloudflare ranges;
maintain that file when the permitted networks change.

The runner holds a PostgreSQL advisory lock and refuses a second instance.
Stop the current runner before replacing it; updates have downtime. Loss of the
lock connection stops the process. This prevents accidental duplicate runners;
it is not a distributed ownership or failover protocol.

Cron schedules use UTC, with optional seconds. Overdue schedules run once, then
advance to the next future occurrence. Dispatch is best effort: a crash after
claiming an occurrence or a saturated worker pool can skip that occurrence.

Console logs still use NATS. The runner persists messages in the existing
`logs` table and serves the last ten plus live events at
`/api/v1/workers/{id}/logs` (SSE) and `/ws-logs` (WebSocket) on dashboard hosts.
Access uses the API worker's session and worker ownership check. Slow clients
must reconnect. NATS and log delivery are not durable.

## Usage

### Build

One JavaScript engine per build, and `wasm` on top of it if the runner should
also serve components. Selecting no backend, or two JavaScript engines, is a
compile error.

```bash
cargo build --release --features v8,wasm   # recommended for production
cargo build --release --features jsc
cargo build --release --features quickjs
cargo build --release --features boa
cargo build --release --features nova
cargo build --release --features wasm
```

A worker goes to the backend its code type names, so a build carrying both
serves JavaScript workers and components from the same process.

Every backend serves `fetch` and, through `Event::Task` with a schedule source,
`scheduled`. Bindings are supported per type: the runner refuses a worker that
declares a binding its backend cannot serve, naming the types, rather than
handing the guest an undefined `env.ASSETS`.

| Backend | Feature   | Code type            | Snapshot / code cache | env | Bindings              | Known limitations                                                                                                   |
| ------- | --------- | -------------------- | --------------------- | --- | --------------------- | ------------------------------------------------------------------------------------------------------------------- |
| V8      | `v8`      | javascript, snapshot | yes                   | yes | all but images        | no images handler on any backend; the only backend with an isolate pool, warm reuse and websockets                  |
| JSC     | `jsc`     | javascript           | no                    | yes | none                  | links the system JavaScriptCore; no websockets; a fresh context per request                                         |
| QuickJS | `quickjs` | javascript           | no                    | no  | none                  | no `env`, no websockets; a fresh runtime per request                                                                |
| Boa     | `boa`     | javascript           | no                    | no  | none                  | no `env`, no websockets; a fresh context per request                                                                |
| Nova    | `nova`    | javascript           | no                    | yes | assets, database      | pure Rust, no C; no `fetch()`, no WebAssembly; `crypto.subtle` stops at HMAC and AES-GCM; a fresh agent per request |
| WASM    | `wasm`    | wasm                 | no                    | yes | kv, database, storage | `wasi:http/proxy` components only; env arrives as WASI vars, not `env`; no assets or worker bindings                |

The wasm guest reaches its bindings through the `openworkers:bindings` WIT
package rather than an `env` object: every call names its binding, and the
runner resolves that name against the worker's bindings.

Nova is the one backend with no C in it: engine, parser and platform layer are
all Rust, which is what makes it the candidate for a target where a V8 build is
not worth its size. It scores 429 of the 448 tests `openworkers-conformance`
measures, against v8's 448, and serves the dashboard.

Optional on top of a backend: `database` (default) and `telemetry`.

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
WORKER_DOMAINS='workers.rocks,workers.dev.localhost'
```

### Environment Variables

#### Required

| Variable       | Description                  |
| -------------- | ---------------------------- |
| `DATABASE_URL` | PostgreSQL connection string |
| `NATS_SERVERS` | NATS server URL              |

#### Networking

| Variable                      | Default | Description                                                            |
| ----------------------------- | ------- | ---------------------------------------------------------------------- |
| `WORKER_DOMAINS`              | unset   | Comma-separated list of worker domains for public and internal routing |
| `HTTP_POOL_MAX_IDLE_PER_HOST` | `100`   | Max idle HTTP connections per host (for worker `fetch()`)              |

#### Code cache

Holds V8 code caches and precompiled wasm components, so a worker version is
compiled once instead of on every cold start.

| Variable               | Default     | Description                                        |
| ---------------------- | ----------- | -------------------------------------------------- |
| `CODE_CACHE_MAX`       | `5000`      | Max entries in the in-memory LRU                   |
| `CODE_CACHE_MAX_BYTES` | `536870912` | Max total bytes in that LRU, whichever binds first |

`SNAPSHOT_CACHE_MAX` and `SNAPSHOT_CACHE_MAX_BYTES` are still read when the
`CODE_CACHE_*` name is unset, with a warning.

#### V8 Runtime

| Variable                 | Default   | Description                                       |
| ------------------------ | --------- | ------------------------------------------------- |
| `V8_EXECUTE`             | `PINNED`  | Execution mode: `PINNED`, `POOLED`, or `ONESHOT`  |
| `WORKER_POOL_SIZE`       | CPU cores | Number of V8 worker threads                       |
| `MAX_QUEUED_WORKERS`     | pool × 10 | Max queued tasks before backpressure              |
| `WORKER_WAIT_TIMEOUT_MS` | `10000`   | Timeout (ms) waiting for a worker slot            |
| `ISOLATE_MAX_CONCURRENT` | `1`       | Requests one isolate serves at once (images: 20)  |
| `CONTEXT_MAX_REUSES`     | `1000`    | Requests a warm context serves before it ages out |

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
