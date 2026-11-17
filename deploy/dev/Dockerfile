# Crucible development container: the canonical Linux environment for building,
# testing, simulating and benchmarking the Rust workspace (see docs/development.md).
FROM rust:1.100.0-bookworm

ARG NEXTEST_VERSION=0.9.152
ARG CARGO_DENY_VERSION=0.19.2

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      clang lld cmake pkg-config git curl ca-certificates jq python3 xz-utils zstd \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy \
 && rustup toolchain install nightly --profile minimal

RUN curl -LsSf "https://get.nexte.st/${NEXTEST_VERSION}/linux" | tar zxf - -C "${CARGO_HOME}/bin" \
 && curl -LsSf "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar zxf - --strip-components=1 -C "${CARGO_HOME}/bin" "cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl/cargo-deny"

ENV CARGO_TARGET_DIR=/target \
    CARGO_INCREMENTAL=0 \
    RUST_BACKTRACE=1

WORKDIR /work
