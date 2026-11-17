# Formal-methods and fuzzing layer on top of the dev container:
# Kani (bounded model checking), Quint (protocol specs, with Java for TLC/Apalache), cargo-fuzz.
ARG BASE=crucible-dev:1
FROM ${BASE}

ARG KANI_VERSION=0.68.0
ARG QUINT_VERSION=0.33.1
ARG CARGO_FUZZ_VERSION=0.13.2

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
 && curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && mkdir -p /etc/apt/keyrings \
 && curl -fsSL https://packages.adoptium.net/artifactory/api/gpg/key/public | gpg --dearmor -o /etc/apt/keyrings/adoptium.gpg \
 && echo "deb [signed-by=/etc/apt/keyrings/adoptium.gpg] https://packages.adoptium.net/artifactory/deb bookworm main" > /etc/apt/sources.list.d/adoptium.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends temurin-21-jre \
 && rm -rf /var/lib/apt/lists/*

RUN npm install -g "@informalsystems/quint@${QUINT_VERSION}"

RUN cargo install --locked "kani-verifier@${KANI_VERSION}" \
 && cargo kani setup

RUN cargo +nightly install --locked "cargo-fuzz@${CARGO_FUZZ_VERSION}"
