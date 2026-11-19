# SQL workspace

The SQL workspace lets project members run read-only SQL over their project's telemetry, from the **SQL** page or
the API. It needs the control plane (Postgres). This page covers what an operator runs and sets.

## How isolation works

Each project's queries run as that project's own ClickHouse user, behind row policies that let it see only its
project's rows within the project's retention, a read-only settings profile with the workspace's limits, and an
hourly quota. Crucible sends the query text to ClickHouse as written, so isolation rests on ClickHouse's access
control, not on parsing. Queries read curated views in the `crucible_sql` database, such as `crucible_sql.spans`
and `crucible_sql.mcp_operations`; the SQL page's schema browser and `GET .../sql/schema` list them with their
columns.

## Provisioning

`--migrate` (or `crucible migrate`) creates what the workspace needs in ClickHouse with the admin user, which is
why that user needs access management. A project's own ClickHouse user is created the first time someone runs a
query in the project. After that, a job on the server that holds the `sql_identities` lease runs every 5 minutes.
It:

- rewrites a project's row policies when its retention changes;
- provisions a user again when it has not been confirmed for an hour, which repairs drift;
- removes the user of a project that was deleted, or that has run no query for 30 days (the next query creates it
  again);
- deletes run history older than 30 days.

No workspace password is stored: each is derived from the master key, the user name and a generation number.

## The self-probe

Every 5 minutes, and every 30 seconds until it first succeeds, each server checks that a workspace user sees only
its own project. The result is in each signed-in session's `features.sql_workspace`, and the SQL page shows it.

| Reason | What it means | What to do |
|---|---|---|
| none (`enabled: true`) | The last check passed | Nothing |
| `probe_pending` | The server has just started and the first check has not finished | Wait a few seconds |
| `probe_unavailable` | The check could not reach its ClickHouse user | Check that ClickHouse is up, then run `crucible migrate` |
| `isolation_probe_failed` | The check found a breach: a workspace user saw rows it should not | The workspace stays off on every server. Run `crucible migrate` and check the ClickHouse server's access configuration, such as a user or role that grants more than Crucible's |

While the workspace is off, runs answer 503 `sql_workspace_unavailable` with the reason, and the rest of Crucible
works as before.

## Limits and the quota

| Limit | Value |
|---|---|
| Rows in a result | 10,000; more are cut and the result says `truncated: true` |
| Bytes in a result | 16 MiB |
| Bytes in one cell | 64 KiB; longer cells are cut |
| Time | 30 seconds |
| Runs at once | 1 per person, 4 per project |
| Runs per minute | 30 per person |
| Statements per hour | `sql.statements_per_hour`, 1,800 by default, per project |

A run is three statements: two checks and the query. Raise or lower the quota with `sql.statements_per_hour`
(`CRUCIBLE_SQL_STATEMENTS_PER_HOUR`); the server applies it to ClickHouse at every start. A project over its quota
gets 429 `sql_quota_exceeded`, with a message that names the setting.
