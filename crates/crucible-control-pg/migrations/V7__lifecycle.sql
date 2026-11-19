-- Retention and deletion (contracts 11.1, 11.7 and 13): the node registry, deletion requests and their per-node
-- tombstone acknowledgements, the ledger of leader actions on ClickHouse, and the retention floors.

-- The node registry. `heartbeat_at` is the database clock, so that liveness does not depend on node clocks.
CREATE TABLE nodes (
    wal_epoch uuid NOT NULL,
    heartbeat_at timestamptz NOT NULL,
    version text NOT NULL,
    checkpoint_lsn bigint NOT NULL,
    address text NOT NULL,
    CONSTRAINT nodes_pkey PRIMARY KEY (wal_epoch)
);

CREATE SEQUENCE deletion_seq;
CREATE TABLE deletion_requests (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    kind text NOT NULL,
    selector jsonb NOT NULL,
    state text NOT NULL,
    requested_at timestamptz NOT NULL,
    deletion_seq bigint NOT NULL,
    requested_by text NOT NULL,
    idempotency_key text,
    request_hash text NOT NULL,
    places jsonb NOT NULL,
    attempts integer NOT NULL,
    error text,
    updated_at timestamptz NOT NULL,
    CONSTRAINT deletion_requests_pkey PRIMARY KEY (id),
    CONSTRAINT deletion_requests_deletion_seq_key UNIQUE (deletion_seq),
    CONSTRAINT deletion_requests_idempotency_key UNIQUE (project_id, requested_by, idempotency_key),
    CONSTRAINT deletion_requests_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
CREATE INDEX deletion_requests_project_idx ON deletion_requests (project_id, deletion_seq DESC);
CREATE INDEX deletion_requests_open_idx ON deletion_requests (deletion_seq)
    WHERE state NOT IN ('completed', 'failed');

CREATE TABLE deletion_node_acks (
    request_id uuid NOT NULL,
    wal_epoch uuid NOT NULL,
    tombstone_lsn bigint NOT NULL,
    passed_at timestamptz,
    CONSTRAINT deletion_node_acks_pkey PRIMARY KEY (request_id, wal_epoch),
    CONSTRAINT deletion_node_acks_request_id_fkey FOREIGN KEY (request_id) REFERENCES deletion_requests (id)
);
CREATE INDEX deletion_node_acks_wal_epoch_idx ON deletion_node_acks (wal_epoch);

-- Leader actions on ClickHouse (contract 11.7): each is recorded under the fencing token before it is issued.
CREATE TABLE lifecycle_actions (
    id uuid NOT NULL,
    key text NOT NULL,
    kind text NOT NULL,
    table_id integer NOT NULL,
    partition_id text NOT NULL,
    project_id uuid,
    request_id uuid,
    detail jsonb NOT NULL,
    state text NOT NULL,
    fencing_token bigint NOT NULL,
    mutation text,
    attempts integer NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CONSTRAINT lifecycle_actions_pkey PRIMARY KEY (id),
    CONSTRAINT lifecycle_actions_key_key UNIQUE (key)
);
CREATE INDEX lifecycle_actions_open_idx ON lifecycle_actions (created_at, id)
    WHERE state NOT IN ('verified', 'failed');
CREATE INDEX lifecycle_actions_request_id_idx ON lifecycle_actions (request_id) WHERE request_id IS NOT NULL;

-- The cut-offs a project's reads never go below again (contract 13.1).
CREATE TABLE retention_floors (
    project_id uuid NOT NULL,
    retention timestamptz,
    payload timestamptz,
    mcp_content timestamptz,
    CONSTRAINT retention_floors_pkey PRIMARY KEY (project_id),
    CONSTRAINT retention_floors_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
