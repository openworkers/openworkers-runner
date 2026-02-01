FROM rust:1.91-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

ARG RUNTIME=v8

ENV FEATURES=$RUNTIME,database,telemetry
ENV RUST_BACKTRACE=1
ENV RUNTIME_SNAPSHOT_PATH=/build/snapshot.bin

RUN touch $RUNTIME_SNAPSHOT_PATH

COPY --from=planner /build/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
RUN --mount=type=cache,target=$CARGO_HOME/git \
    --mount=type=cache,target=$CARGO_HOME/registry \
    --mount=type=cache,target=/build/target \
    cargo chef cook --release --features=$FEATURES --recipe-path recipe.json

# Build application
COPY . .

RUN touch $RUNTIME_SNAPSHOT_PATH

RUN --mount=type=cache,target=$CARGO_HOME/git \
    --mount=type=cache,target=$CARGO_HOME/registry \
    --mount=type=cache,target=/build/target \
    cargo run --release --features=$FEATURES --bin snapshot && \
    # Build the runner and copy executable out of the cache so it can be used in the next stage
    cargo build --release --features=$FEATURES && cp /build/target/release/openworkers-runner /build/output

FROM debian:bookworm-slim

RUN apt-get update \
    # Install ca-certificates and wget (used for healthcheck)
    && apt-get install -y ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/output /usr/local/bin/openworkers-runner
COPY --from=builder /build/snapshot.bin /build/snapshot.bin

CMD ["/usr/local/bin/openworkers-runner"]

EXPOSE 8080
