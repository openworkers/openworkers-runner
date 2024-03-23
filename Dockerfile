FROM rust:1.77 as builder

RUN mkdir -p /build/openworkers-runner

ENV RUST_BACKTRACE=1
ENV RUNTIME_SNAPSHOT_PATH=/build/snapshot.bin

WORKDIR /build/openworkers-runner

COPY . /build/openworkers-runner

RUN touch $RUNTIME_SNAPSHOT_PATH

RUN --mount=type=cache,target=~/.cargo \
    --mount=type=cache,target=/build/openworkers-runner/target \
    cargo run --release --bin snapshot && \
    cargo build --release && cp /build/openworkers-runner/target/release/openworkers-runner /build/output

FROM debian:bookworm-slim

COPY --from=builder /build/output /usr/local/bin/openworkers-runner

CMD ["/usr/local/bin/openworkers-runner"]

EXPOSE 8080
