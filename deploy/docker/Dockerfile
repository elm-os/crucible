# The Crucible image: the `crucible` binary with the web UI embedded, on a distroless base that runs as a
# non-root user (uid 65532). Development preview; see docs/self-hosting/README.md. From the repository root:
#
#   docker build -f deploy/docker/Dockerfile -t crucible:dev .
#
# Every stage but the last runs on the build machine. An image for another architecture is cross-compiled, so a
# multi-platform build (linux/amd64 and linux/arm64) needs no emulation.
#
# The image has no shell or HTTP client, so it declares no HEALTHCHECK. Probe `GET /readyz` on port 8080 from
# outside the container: the `ready` service in deploy/compose/compose.yaml does that with a curl image.

ARG NODE_IMAGE=node:24.22.3-bookworm-slim@sha256:dc26ae92c9fb15cf6b1d63e20f0944b2efe0f115b20e00be48fb23fceb061c54
ARG RUST_IMAGE=rust:1.100.0-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

# The UI's static export (ui/out). It is the same on every platform.
FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS ui
ENV CI=true NEXT_TELEMETRY_DISABLED=1
# The version in package.json's `packageManager`.
RUN npm install --global pnpm@11.7.0
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm fetch
COPY ui ui
# --ignore-scripts skips the root `prepare` script, which installs git hooks.
RUN pnpm install --offline --frozen-lockfile --ignore-scripts --filter crucible-ui \
 && pnpm --filter crucible-ui build

# Rust tools that do not depend on the target platform.
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS tools
ARG CARGO_CHEF_VERSION=0.1.78
ARG CARGO_AUDITABLE_VERSION=0.7.7
# Lower it (for example --build-arg CARGO_BUILD_JOBS=2) when the build runs out of memory.
ARG CARGO_BUILD_JOBS=default
RUN cargo install --locked "cargo-chef@${CARGO_CHEF_VERSION}" "cargo-auditable@${CARGO_AUDITABLE_VERSION}"
# `cargo xtask` runs in the dev profile; its debug info would only grow the build cache.
ENV CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=false CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
WORKDIR /src

# The dependency recipe: it changes only when a manifest or Cargo.lock does.
FROM tools AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates crates
COPY xtask xtask
RUN cargo chef prepare --recipe-path recipe.json

FROM tools AS build
ARG TARGETARCH
# The Rust target, and a C cross compiler (mimalloc, zstd and ring compile C) when the target architecture is not
# the build machine's.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; gcc=gcc-x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gcc=gcc-aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH '$TARGETARCH': build linux/amd64 or linux/arm64" >&2; exit 1 ;; \
    esac; \
    if [ "$(dpkg --print-architecture)" != "$TARGETARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "$gcc" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"; \
    echo "$triple" > /etc/crucible-target
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
COPY --from=planner /src/recipe.json recipe.json
# Dependencies only, so this layer is reused until the recipe changes. Both builds are cooked exactly as they run
# below: the server on its own (a workspace build would add test-only features) and xtask for the build machine.
RUN cargo chef cook --locked --release --target "$(cat /etc/crucible-target)" --package crucible \
      --features embed-ui --recipe-path recipe.json \
 && cargo chef cook --locked --package xtask --recipe-path recipe.json
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates crates
COPY xtask xtask
COPY --from=ui /src/ui/out ui/out
# `cargo auditable build` is `cargo build` plus the dependency list in a binary section, which SBOM scanners and
# `cargo audit bin` read.
RUN set -eu; \
    target="$(cat /etc/crucible-target)"; \
    cargo xtask ui compress; \
    cargo auditable build --locked --release --target "$target" --package crucible --features embed-ui; \
    mkdir -p /out/data; \
    cp "target/$target/release/crucible" /out/crucible

FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="Crucible" \
      org.opencontainers.image.description="Observability and analytics for AI agents and MCP servers (development preview)" \
      org.opencontainers.image.source="https://github.com/elm-os/crucible" \
      org.opencontainers.image.documentation="https://github.com/elm-os/crucible/blob/main/docs/self-hosting/README.md" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /out/crucible /usr/local/bin/crucible
COPY LICENSE THIRD_PARTY_NOTICES /usr/share/doc/crucible/
# The data directory (WAL, quarantine, exports), owned by the runtime user. A named volume mounted here starts
# with this ownership.
COPY --from=build --chown=65532:65532 /out/data /var/lib/crucible
WORKDIR /var/lib/crucible
VOLUME ["/var/lib/crucible"]
USER 65532:65532
# 8080: UI, REST API, OTLP/HTTP ingest and the health paths. 4318: OTLP/HTTP ingest. 4317: OTLP/gRPC ingest
# (planned; nothing listens on it yet).
EXPOSE 8080 4318 4317
ENTRYPOINT ["/usr/local/bin/crucible"]
CMD ["serve", "--data-dir=/var/lib/crucible/data"]
