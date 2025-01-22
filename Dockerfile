FROM debian:bookworm-slim as common

RUN apt-get update \
    # Install ca-certificates and wget (used for healthcheck)
    && apt-get install -y ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

FROM --platform=$BUILDPLATFORM rust:1.84 AS prepare

RUN apt-get update && apt-get install -y \
    gcc-aarch64-linux-gnu \
    gcc-x86-64-linux-gnu \
    libc6-dev-amd64-cross \
    libc6-dev-arm64-cross

ENV CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
ENV CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc

ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

RUN rustup target add x86_64-unknown-linux-gnu \
                      aarch64-unknown-linux-gnu

FROM prepare AS build

RUN mkdir -p /build

ENV RUST_BACKTRACE=1
ENV RUNTIME_SNAPSHOT_PATH=/build/snapshot.bin

WORKDIR /build

COPY . /build

RUN touch $RUNTIME_SNAPSHOT_PATH

RUN --mount=type=cache,sharing=locked,target=$CARGO_HOME/git \
    --mount=type=cache,sharing=locked,target=$CARGO_HOME/registry \
    --mount=type=cache,sharing=locked,target=/build/openworkers-runner/target \
    cargo run --release --bin snapshot

FROM build AS platform

ARG TARGETPLATFORM

RUN echo "Building for $TARGETPLATFORM"

RUN case "$TARGETPLATFORM" in \
    "linux/arm64")  --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=$CARGO_HOME/git \
                    --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=$CARGO_HOME/registry \
                    --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=/build/openworkers-runner/target \
                    cargo build --release --target aarch64-unknown-linux-gnu && \
                    mv /build/target/aarch64-unknown-linux-gnu/release/openworkers-runner /build/output ;; \
    "linux/amd64")  --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=$CARGO_HOME/git \
                    --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=$CARGO_HOME/registry \
                    --mount=type=cache,id=apt-$TARGETARCH$TARGETVARIANT,sharing=locked,target=/build/openworkers-runner/target \
                    cargo build --release --target x86_64-unknown-linux-gnu && \
                 mv /build/target/x86_64-unknown-linux-gnu/release/openworkers-runner  /build/output ;; \
    *) echo "Unsupported platform: $TARGETPLATFORM" && exit 1 ;; \
    esac

FROM common AS runner

COPY --from=platform /build/output /usr/local/bin/openworkers-runner

CMD ["/usr/local/bin/openworkers-runner"]

EXPOSE 8080
