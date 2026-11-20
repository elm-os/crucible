# Crucible documentation

| Topic | Page |
|---|---|
| Architecture map | [architecture/overview.md](architecture/overview.md) |
| Binding contracts (records, WAL, API, schemas) | [architecture/contracts.md](architecture/contracts.md) |
| Testing methodology | [testing/methodology.md](testing/methodology.md) |
| Formal model of the ingest protocol | [testing/formal-model.md](testing/formal-model.md) |
| Performance rig, A/B gate, load generator and fsync-honesty probe | [testing/perf.md](testing/perf.md) |
| Throughput baseline of `crucible serve` (milestone 1) | [testing/perf-baseline.md](testing/perf-baseline.md) |
| UI surfaces by state (Playwright and axe) | [testing/ui-surfaces.md](testing/ui-surfaces.md) |
| Control store: the Postgres differential and the lease simulation | [testing/control-store.md](testing/control-store.md) |
| SQL workspace isolation: the `sqliso` suite, its findings and results | [testing/sqliso.md](testing/sqliso.md) |
| Background jobs: adding one | [architecture/adding-a-job.md](architecture/adding-a-job.md) |
| Background jobs: the simulation, its oracle and its mutants | [testing/jobs-simulation.md](testing/jobs-simulation.md) |
| Alerts: the simulation, its oracle and its mutants | [testing/alerts-simulation.md](testing/alerts-simulation.md) |
| Retention and deletion: the simulation, its oracle and its mutants | [testing/lifecycle-simulation.md](testing/lifecycle-simulation.md) |
| Media: the simulation, its oracle and its mutants, and the object store conformance rows | [testing/media-simulation.md](testing/media-simulation.md) |
| Development setup | [development.md](development.md) |
| Self-hosting: deployments, the image, Compose, the binary and ports | [self-hosting/README.md](self-hosting/README.md) |
| Quickstart: Docker Compose from an empty machine to a first trace | [self-hosting/quickstart.md](self-hosting/quickstart.md) |
| Configuration: every setting, with its key, environment variable, flag and default | [self-hosting/configuration.md](self-hosting/configuration.md) |
| Operations: upgrades, backups, health checks, metrics and logs | [self-hosting/operations.md](self-hosting/operations.md) |
| Security: what is on by default and the switches that harden a deployment | [self-hosting/security.md](self-hosting/security.md) |
| Retention and deletion | [self-hosting/retention-and-deletion.md](self-hosting/retention-and-deletion.md) |
| Alerts and egress: outbound calls, the egress policy and email | [self-hosting/alerts-and-egress.md](self-hosting/alerts-and-egress.md) |
| SQL workspace: provisioning, the self-probe, limits and the quota | [self-hosting/sql-workspace.md](self-hosting/sql-workspace.md) |
| Troubleshooting: common refusals and their fixes | [self-hosting/troubleshooting.md](self-hosting/troubleshooting.md) |
| Media: images, audio and files on S3-compatible object storage | [self-hosting/media.md](self-hosting/media.md) |
| Guides for sending telemetry and reading it back | [guides/README.md](guides/README.md) |
| Sending traces over OTLP from any OpenTelemetry SDK or the Collector | [guides/sending-traces.md](guides/sending-traces.md) |
| TypeScript SDK | [guides/sdk-typescript.md](guides/sdk-typescript.md) |
| Python SDK | [guides/sdk-python.md](guides/sdk-python.md) |
| Go SDK | [guides/sdk-go.md](guides/sdk-go.md) |
| REST API: auth, errors, windows, cursors | [api/README.md](api/README.md) |
| MCP proxy | [guides/mcp-proxy.md](guides/mcp-proxy.md) |
| Alerts and notification channels | [guides/alerts.md](guides/alerts.md) |
