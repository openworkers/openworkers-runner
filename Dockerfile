FROM rust:1.84 AS installer

RUN mkdir -p /build/openworkers-runner

ENV RUST_BACKTRACE=1
ENV RUNTIME_SNAPSHOT_PATH=/build/snapshot.bin

WORKDIR /build/openworkers-runner

COPY Cargo.toml Cargo.lock ./

RUN touch $RUNTIME_SNAPSHOT_PATH

RUN mkdir -p bin
RUN echo "fn main() { }" > bin/main.rs
RUN echo "fn main() { }" > bin/snapshot.rs

RUN cargo build --release

# Build the project
FROM installer AS builder

RUN rm -rf bin

COPY . .

RUN cargo run --release --bin snapshot && \
    cargo build --release

# Copy the binary to the final image
FROM debian:bookworm-slim

RUN apt-get update \
    # Install ca-certificates and wget (used for healthcheck)
    && apt-get install -y ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/openworkers-runner/target/release/openworkers-runner /usr/local/bin/openworkers-runner

CMD ["/usr/local/bin/openworkers-runner"]

EXPOSE 8080
