# OpenWorkers Runner

Main execution engine that runs JavaScript workers using the V8 runtime.

## Stack

```
openworkers-core        ← Defines traits (Worker, Task, Response...)
openworkers-runtime-v8  ← Implements traits (V8 runtime)
openworkers-runner      ← Uses runtime to execute workers (this crate)
```

## Development setup

`openworkers-core` and `openworkers-runtime-v8` must be in sibling folders.

For local development, update `Cargo.toml`:
```toml
openworkers-core = { path = "../openworkers-core", features = ["hyper"] }
openworkers-runtime-v8 = { path = "../openworkers-runtime-v8", optional = true }
```

## After editing runtime

**IMPORTANT**: After modifying `openworkers-runtime-v8`, you MUST regenerate the runner snapshot:

```bash
# From runner directory
cargo run --features v8 --bin snapshot
```

Output: `/tmp/openworkers-runtime-snapshot.bin`

Without this, tests will fail with cryptic errors like:
- "Cannot read properties of null"
- Empty bodies when they should have content

## Running tests

```bash
cargo test --features v8              # all tests
cargo test --features v8 test_name    # specific test
```

## Feature flags

- `v8` - Enable V8 runtime (default for development)
- `wasm` - Enable WASM runtime
- `database` - Enable database bindings (postgate)
