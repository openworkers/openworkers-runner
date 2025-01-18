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
