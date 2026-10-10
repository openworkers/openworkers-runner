# OpenWorkers runner

OpenWorkers is a runtime for running javascript code in a serverless environment.

This runner manages instances of [OpenWorkers Runtime](https://github.com/openworkers/openworkers-runtime-v8).

## Versions

- Major: the deployment changes (ports, services, variables). Read the notes
  of the tag before you update.
- Minor: the version of `openworkers-core` the runner is built on.
- Patch: the other changes.

## Single-runner deployment

Run one runner per platform database. The runner serves public HTTP and
HTTPS, runs the workers and their crons, and stores and streams the console
logs. It does not use nginx, NATS, openworkers-logs or openworkers-scheduler.

| Setting                                | Default                                                                      | Purpose                                                    |
| -------------------------------------- | ---------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `HTTP_ADDR`                            | `0.0.0.0:8081`                                                               | Public HTTP listener (HTTP/1.1)                            |
| `HTTPS_ADDR`                           | `0.0.0.0:8443`                                                               | Public HTTPS listener (HTTP/2 and HTTP/1.1, through ALPN)  |
| `HTTP_TLS_CERTIFICATE`, `HTTP_TLS_KEY` | unset                                                                        | PEM files; set both to start the HTTPS listener            |
| `HTTPS_CLIENT_CA_FILE`                 | unset                                                                        | PEM CA; HTTPS clients present a cert it signs |
| `HTTPS_CLIENT_CERT_MODE`               | `require`                                                                    | `require` closes a client without a cert; `log` serves it and logs a warning |
| `HTTP_LISTENERS`                       | CPU count                                                                    | Accept loops per address, with `SO_REUSEPORT`              |
| `INBOUND_ALLOWLIST_FILE`               | unset                                                                        | The peers that can connect to the public listeners         |
| `CLIENT_IP_HEADER`                     | unset                                                                        | The header that gives the client address (needs allowlist) |
| `DASHBOARD_HOSTS`                      | `dash.openworkers.com,dash.openworkers.dev,dash.dev.localhost,dash.dev.kube` | Hosts of the API worker                                    |
| `API_WORKER_NAME`                      | `openworkers-api`                                                            | The dashboard and API worker                               |
| `WORKER_DOMAINS`                       | unset                                                                        | Domains of `{name}.{domain}` and `{uuid}.{domain}` hosts   |

A dashboard host goes to the API worker. A host under a worker domain goes to
the worker that its first label names. Other hosts go to the domain and project
routes of the database. A public request cannot set `x-worker-id`,
`x-worker-name`, `x-request-id` or the `x-openworkers-*` headers.

The internal listener is `127.0.0.1:8080`. It serves the worker-to-worker calls
and the admin endpoints. Do not publish it. The admin endpoints answer only on
this listener.

The runner sets `x-real-ip`, `x-forwarded-for` and `cf-connecting-ip` to the
client address, and `x-forwarded-proto` to the scheme of the client. The client
is the TCP peer. When `CLIENT_IP_HEADER` is set and the peer is in the
allowlist, the client address comes from that header and the scheme from
`x-forwarded-proto`. Behind Cloudflare, set `CLIENT_IP_HEADER=cf-connecting-ip`
and put the Cloudflare ranges in the allowlist.

The allowlist has one IPv4 or IPv6 address or network per line; `#` starts a
comment. Without a file, all peers can connect. An empty file refuses all peers.
A file that cannot be read or parsed stops the start. The runner closes a
refused connection before it reads a byte.

With `HTTPS_CLIENT_CA_FILE`, the HTTPS listener asks each client for a cert that
this CA signs. A cert of another CA fails the handshake. A client without a
cert is closed (`require`), or served with a warning that names its host
(`log`): use `log` to find the hosts that arrive without the cert before you
refuse them. Behind a proxy that presents a client cert to the origin, only
that proxy can then reach the listener and give the client address.

A client must send its request headers in 30 s, and end the TLS handshake in
10 s. The worker upload of the dashboard (`/api/v1/workers/{id}/upload` on a
dashboard host) takes a body of 30 MiB; other requests take
`MAX_REQUEST_BODY_BYTES`. A path that only a scanner asks for (`/.env`, `/.git/`, `id_rsa`,
`/etc/passwd`, `*.php`) gets a 404, and no worker runs. The runner reads the
certificate, the key and the allowlist at start.

The runner holds a PostgreSQL advisory lock on a connection of its own, and a
second runner on the same database does not start. Stop the runner before you
start its replacement; a deploy stops the traffic for that time. When the lock
connection closes, the runner stops. The lock does not fence a runner that has
lost it, so it is not an ownership protocol for several runners.

Crons use UTC, with an optional seconds field. A late cron runs once, then waits
for its next run. The runner records the scheduled event before it runs it, so
a stop between the two, or a full worker pool, skips that run.

Console logs stay in the runner process. Each line goes to the live log streams
at once, and to the `logs` table in batches. A dashboard host serves
`/api/v1/workers/{uuid}/logs` (SSE) and `/api/v1/workers/{uuid}/ws-logs`
(WebSocket): the last 10 lines, then the live lines. The API worker checks the
session of the client and the owner of the worker. A client that falls behind
by 1024 lines is disconnected and must connect again. When 10 000 lines wait for
the table, the runner drops new lines and logs a warning. A message in the
table keeps 255 characters.

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
WORKER_DOMAINS='workers.rocks,workers.dev.localhost'
```

### Environment Variables

#### Required

| Variable       | Description                  |
| -------------- | ---------------------------- |
| `DATABASE_URL` | PostgreSQL connection string |

#### Networking

| Variable                      | Default | Description                                                            |
| ----------------------------- | ------- | ---------------------------------------------------------------------- |
| `WORKER_DOMAINS`              | unset   | Comma-separated list of worker domains for public and internal routing |
| `HTTP_POOL_MAX_IDLE_PER_HOST` | `100`   | Max idle HTTP connections per host (for worker `fetch()`)              |
| `MAX_REQUEST_BODY_BYTES`      | 10 MiB  | Largest request body; a larger body gets 413                           |

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
