# Development preview image: wraps a `crucible` binary that you built locally (see
# docs/self-hosting/quickstart.md). It is not a release image.
FROM debian:bookworm-slim
RUN useradd --system --home-dir /var/lib/crucible --create-home crucible
COPY bin/crucible /usr/local/bin/crucible
USER crucible
EXPOSE 8080 4318 4317
ENTRYPOINT ["crucible"]
CMD ["serve"]
