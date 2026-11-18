-- The SQL workspace's tables (contracts 7.4 and 11.1). Forward-only: a change is a new file.
-- No column has a default: ids and timestamps come from the server process (contract 2.7).
-- The tables reference only `projects`, which V1 created.

-- One row per project whose ClickHouse identity was requested. `generation` is the password generation the
-- identity was provisioned with; rotation raises `target_generation` and the reconciler follows. The three
-- retention columns are the values the project's row policies were last written with.
CREATE TABLE ch_sql_identities (
    project_id uuid NOT NULL,
    generation integer NOT NULL,
    target_generation integer NOT NULL,
    state text NOT NULL,
    retention_days integer,
    payload_retention_days integer,
    mcp_content_retention_days integer,
    last_probe timestamptz,
    last_used_at timestamptz NOT NULL,
    CONSTRAINT ch_sql_identities_pkey PRIMARY KEY (project_id),
    CONSTRAINT ch_sql_identities_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);

-- The owner is a user or an API key: exactly one of the two columns is set.
CREATE TABLE saved_queries (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    owner_id uuid,
    owner_key_id uuid,
    name text NOT NULL,
    description text NOT NULL,
    visibility text NOT NULL,
    sql text NOT NULL,
    parameters jsonb NOT NULL,
    version bigint NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CONSTRAINT saved_queries_pkey PRIMARY KEY (id),
    CONSTRAINT saved_queries_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id),
    CONSTRAINT saved_queries_owner_check CHECK ((owner_id IS NULL) <> (owner_key_id IS NULL))
);
CREATE INDEX saved_queries_project_id_idx ON saved_queries (project_id);

-- The audit row of every run. `outcome` is `running` until the run ends. Rows are kept 30 days.
CREATE TABLE sql_runs (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    user_id uuid,
    api_key_id uuid,
    query_id uuid NOT NULL,
    kind text NOT NULL,
    saved_query_id uuid,
    sql text NOT NULL,
    started_at timestamptz NOT NULL,
    outcome text NOT NULL,
    error_code text,
    result_rows bigint NOT NULL,
    truncated boolean NOT NULL,
    rows_read bigint NOT NULL,
    bytes_read bigint NOT NULL,
    elapsed_ms bigint NOT NULL,
    finished_at timestamptz,
    CONSTRAINT sql_runs_pkey PRIMARY KEY (id),
    CONSTRAINT sql_runs_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id),
    CONSTRAINT sql_runs_actor_check CHECK ((user_id IS NULL) <> (api_key_id IS NULL))
);
CREATE INDEX sql_runs_project_id_started_at_idx ON sql_runs (project_id, started_at DESC);
CREATE INDEX sql_runs_started_at_idx ON sql_runs (started_at);
