# OpenWorkers runner

OpenWorkers is a runtime for running javascript code in a serverless environment.

This runner manages instances of [OpenWorkers Runtime](https://github.com/openworkers/openworkers-runtime).

## Usage

### Build

The runner supports multiple JavaScript runtime backends via feature flags:

```bash
# Deno runtime (default)
cargo build --release

# QuickJS runtime
cargo build --release --no-default-features --features quickjs

# V8 runtime (standalone, without Deno)
cargo build --release --no-default-features --features v8

# Boa runtime (pure Rust)
cargo build --release --no-default-features --features boa

# JavaScriptCore runtime (macOS/iOS)
cargo build --release --no-default-features --features jsc
```

Available features:

- `deno` (default) - Full Deno runtime with Web APIs
- `quickjs` - Lightweight QuickJS engine
- `v8` - Standalone V8 engine
- `boa` - Pure Rust JS engine
- `jsc` - JavaScriptCore (Apple platforms)

### Snapshot the runtime

```bash
cargo run --bin snapshot
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

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | - | PostgreSQL connection string |
| `NATS_SERVERS` | - | NATS server URL |
| `WORKER_DOMAINS` | `workers.rocks` | Comma-separated list of worker domains for internal routing |

#### Internal Routing (`WORKER_DOMAINS`)

When a worker calls `fetch()` to a URL matching `*.{domain}`, the request is routed internally instead of going through DNS and external network. This improves latency and avoids external bandwidth costs.

```javascript
// These are routed internally (no DNS lookup):
fetch('https://my-api.workers.rocks/endpoint')

// This goes through external network:
fetch('https://example.com/api')
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

cargo run --features v8|deno|quickjs|boa|jsc
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
