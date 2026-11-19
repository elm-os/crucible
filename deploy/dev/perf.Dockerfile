# Performance-gate layer on top of the dev container: the Docker CLI and its buildx plugin, so that
# `cargo xtask perf` can build the images under test and drive the rig through the host's Docker socket, and the
# GitHub CLI for `perf gate --post` (docs/testing/perf.md). tools/perf/xtask.sh builds it on first use:
#
#   docker build -t crucible-dev-perf:1 -f deploy/dev/perf.Dockerfile deploy/dev
ARG BASE=crucible-dev:1
ARG DOCKER_CLI=docker:29.10.0-cli

FROM ${DOCKER_CLI} AS docker

FROM ${BASE}
ARG GH_VERSION=2.102.0
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker /usr/local/libexec/docker/cli-plugins/docker-buildx /usr/local/libexec/docker/cli-plugins/docker-buildx
RUN arch="$(dpkg --print-architecture)" \
 && curl -LsSf "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_${arch}.tar.gz" \
    | tar -xz --strip-components=2 -C /usr/local/bin "gh_${GH_VERSION}_linux_${arch}/bin/gh"
