# Wraps a `crucible` binary that you built locally (see docs/development.md, "A local build in Compose").
# deploy/docker/Dockerfile builds the release image.
FROM debian:bookworm-slim
RUN useradd --system --home-dir /var/lib/crucible --create-home crucible
COPY bin/crucible /usr/local/bin/crucible
USER crucible
EXPOSE 8080 4318 4317
ENTRYPOINT ["crucible"]
CMD ["serve"]
