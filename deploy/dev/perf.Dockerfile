# Performance-gate layer on top of the dev container: the Docker CLI and its buildx plugin, so that
# `cargo xtask perf` can build the images under test and drive the rig through the host's Docker socket
# (docs/testing/perf.md). tools/perf/xtask.sh builds it on first use:
#
#   docker build -t crucible-dev-perf:1 -f deploy/dev/perf.Dockerfile deploy/dev
ARG BASE=crucible-dev:1
ARG DOCKER_CLI=docker:29.10.0-cli

FROM ${DOCKER_CLI} AS docker

FROM ${BASE}
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker /usr/local/libexec/docker/cli-plugins/docker-buildx /usr/local/libexec/docker/cli-plugins/docker-buildx
