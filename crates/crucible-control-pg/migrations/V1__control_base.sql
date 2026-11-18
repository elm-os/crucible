-- The control-plane tables of contract 11.1 that milestone 4 needs first. Forward-only: a change is a new file.
-- No column has a default: ids and timestamps come from the server process (contract 2.7).

CREATE TABLE org (
    id uuid NOT NULL,
    singleton boolean NOT NULL,
    name text NOT NULL,
    settings jsonb NOT NULL,
    version bigint NOT NULL,
    CONSTRAINT org_pkey PRIMARY KEY (id),
    CONSTRAINT org_singleton_key UNIQUE (singleton),
    CONSTRAINT org_singleton_check CHECK (singleton)
);

CREATE TABLE users (
    id uuid NOT NULL,
    email text NOT NULL,
    name text NOT NULL,
    status text NOT NULL,
    email_verified_at timestamptz,
    CONSTRAINT users_pkey PRIMARY KEY (id),
    CONSTRAINT users_email_key UNIQUE (email)
);

CREATE TABLE credentials (
    user_id uuid NOT NULL,
    password_hash text NOT NULL,
    params_version integer NOT NULL,
    failed_count integer NOT NULL,
    locked_until timestamptz,
    CONSTRAINT credentials_pkey PRIMARY KEY (user_id),
    CONSTRAINT credentials_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE sessions (
    token_hash bytea NOT NULL,
    user_id uuid NOT NULL,
    csrf_token text NOT NULL,
    idle_expires_at timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    auth_method text NOT NULL,
    revoked_at timestamptz,
    CONSTRAINT sessions_pkey PRIMARY KEY (token_hash),
    CONSTRAINT sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);
CREATE INDEX sessions_user_id_idx ON sessions (user_id);
CREATE INDEX sessions_absolute_expires_at_idx ON sessions (absolute_expires_at);

CREATE TABLE projects (
    id uuid NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    settings jsonb NOT NULL,
    retention_days integer,
    payload_retention_days integer,
    mcp_content_retention_days integer,
    config_version bigint NOT NULL,
    version bigint NOT NULL,
    deleted_at timestamptz,
    CONSTRAINT projects_pkey PRIMARY KEY (id),
    CONSTRAINT projects_slug_key UNIQUE (slug)
);

CREATE TABLE org_memberships (
    user_id uuid NOT NULL,
    role text NOT NULL,
    CONSTRAINT org_memberships_pkey PRIMARY KEY (user_id),
    CONSTRAINT org_memberships_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE project_memberships (
    project_id uuid NOT NULL,
    user_id uuid NOT NULL,
    role text NOT NULL,
    CONSTRAINT project_memberships_pkey PRIMARY KEY (project_id, user_id),
    CONSTRAINT project_memberships_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id),
    CONSTRAINT project_memberships_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);
CREATE INDEX project_memberships_user_id_idx ON project_memberships (user_id);

CREATE TABLE api_keys (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    public_key text NOT NULL,
    secret_salt bytea NOT NULL,
    secret_hash bytea NOT NULL,
    lookup_tag bytea NOT NULL,
    scopes text[] NOT NULL,
    import boolean NOT NULL,
    note text NOT NULL,
    expires_at timestamptz,
    last_used_at timestamptz,
    revoked_at timestamptz,
    version bigint NOT NULL,
    CONSTRAINT api_keys_pkey PRIMARY KEY (id),
    CONSTRAINT api_keys_public_key_key UNIQUE (public_key),
    CONSTRAINT api_keys_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
CREATE INDEX api_keys_project_id_idx ON api_keys (project_id);

-- The sequence is read explicitly by the statement that changes a key. Numbers become visible out of order
-- and a rolled-back change leaves a gap (contract 11.8). `at` is the database clock, because the poll's
-- overlap window is judged by the database clock.
CREATE SEQUENCE key_feed_seq;
CREATE TABLE key_feed (
    seq bigint NOT NULL,
    key_id uuid NOT NULL,
    change text NOT NULL,
    key_version bigint NOT NULL,
    at timestamptz NOT NULL,
    CONSTRAINT key_feed_pkey PRIMARY KEY (seq)
);
CREATE INDEX key_feed_at_idx ON key_feed (at);

CREATE TABLE secrets (
    id uuid NOT NULL,
    scope text NOT NULL,
    kind text NOT NULL,
    key_id text NOT NULL,
    nonce bytea NOT NULL,
    ciphertext bytea NOT NULL,
    CONSTRAINT secrets_pkey PRIMARY KEY (id)
);

CREATE TABLE signing_keys (
    kid text NOT NULL,
    purpose text NOT NULL,
    sealed_key bytea NOT NULL,
    state text NOT NULL,
    created_at timestamptz NOT NULL,
    CONSTRAINT signing_keys_pkey PRIMARY KEY (kid)
);

-- Lease expiry is the database clock (contract 11.7). A released lease keeps its row and its token.
CREATE TABLE job_leases (
    name text NOT NULL,
    holder text NOT NULL,
    fencing_token bigint NOT NULL,
    expires_at timestamptz NOT NULL,
    CONSTRAINT job_leases_pkey PRIMARY KEY (name)
);

CREATE TABLE job_cursors (
    name text NOT NULL,
    cursor text NOT NULL,
    CONSTRAINT job_cursors_pkey PRIMARY KEY (name)
);

-- The first claimed queue (contract 11.7): rows are claimed with a `lease_until` and an attempt counter.
CREATE TABLE notification_outbox (
    id uuid NOT NULL,
    dedup_key text NOT NULL,
    payload text NOT NULL,
    attempts integer NOT NULL,
    next_attempt_at timestamptz NOT NULL,
    status text NOT NULL,
    lease_until timestamptz,
    CONSTRAINT notification_outbox_pkey PRIMARY KEY (id),
    CONSTRAINT notification_outbox_dedup_key_key UNIQUE (dedup_key)
);
CREATE INDEX notification_outbox_due_idx ON notification_outbox (next_attempt_at, id) WHERE status = 'pending';

-- The migrations ledger (contract 6.6): Postgres migrations and ClickHouse schema and access steps.
CREATE TABLE schema_migrations (
    id text NOT NULL,
    kind text NOT NULL,
    applied_at timestamptz NOT NULL,
    checksum text NOT NULL,
    CONSTRAINT schema_migrations_pkey PRIMARY KEY (id)
);
