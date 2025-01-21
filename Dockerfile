FROM rust:1.84 as builder

ARG TARGETPLATFORM
ARG TARGETOS
ARG TARGETARCH
ARG TARGETVARIANT

RUN mkdir -p /build/openworkers-runner

ENV RUST_BACKTRACE=1
ENV RUNTIME_SNAPSHOT_PATH=/build/snapshot.bin

WORKDIR /build/openworkers-runner

COPY . /build/openworkers-runner

RUN touch $RUNTIME_SNAPSHOT_PATH

RUN --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=$CARGO_HOME/git \
    --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=$CARGO_HOME/registry \
    --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=/build/openworkers-runner/target \
    cargo run --release --bin snapshot && \
    # Build the runner and copy executable out of the cache so it can be used in the next stage
    cargo build --release && cp /build/openworkers-runner/target/release/openworkers-runner /build/output

FROM debian:bookworm-slim

RUN apt-get update \
    # Install ca-certificates and wget (used for healthcheck)
    && apt-get install -y ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/output /usr/local/bin/openworkers-runner

CMD ["/usr/local/bin/openworkers-runner"]

EXPOSE 8080
