# OpenWorkers runner

OpenWorkers is a runtime for running javascript code in a serverless environment.

This runner manages instances of [OpenWorkers Runtime](https://github.com/openworkers/openworkers-runtime).

## Usage

### Build
```bash
cargo build --release
```

### Run

```bash
export RUST_LOG=openworkers_runtime=debug,openworkers_runner=debug # Optional

cargo run
```
