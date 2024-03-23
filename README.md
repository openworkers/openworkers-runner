# OpenWorkers runner

OpenWorkers is a runtime for running javascript code in a serverless environment.

This runner manages instances of [OpenWorkers Runtime](https://github.com/openworkers/openworkers-runtime).

## Usage

### Build
```bash
cargo build --release
```

### Snapshot the runtime
```bash
cargo run --bin snapshot
```

### Run

```bash
export RUST_LOG=openworkers_runtime=debug,openworkers_runner=debug # Optional

cargo run
```


### Install sqlx-cli (optional - only for development)

```bash
cargo install sqlx-cli --no-default-features --features rustls,postgres
```

#### Prepare the database
```bash
cargo sqlx prepare
```
