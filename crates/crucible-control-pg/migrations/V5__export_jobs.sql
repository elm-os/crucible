-- Export jobs (contracts 11.1 and 12.8). The table is also the queue of the export job (contract 11.7): its
-- rows are claimed with the generic claim columns (`dedup_key`, `payload`, `attempts`, `next_attempt_at`,
-- `status`, `lease_until`). The export's life cycle is read from them and from the outcome columns: pending
-- under a live claim is running, done with a file is ready, done with an error or parked is failed, and an
-- `expires_at` in the past is expired. No column has a default (contract 2.7).

CREATE TABLE export_jobs (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    format text NOT NULL,
    selector text NOT NULL,
    created_by text NOT NULL,
    created_at timestamptz NOT NULL,
    as_of timestamptz NOT NULL,
    dedup_key text NOT NULL,
    payload text NOT NULL,
    attempts integer NOT NULL,
    next_attempt_at timestamptz NOT NULL,
    status text NOT NULL,
    lease_until timestamptz,
    spans bigint NOT NULL,
    bytes bigint NOT NULL,
    skipped bigint NOT NULL,
    file text,
    checksum text,
    expires_at timestamptz,
    error text,
    CONSTRAINT export_jobs_pkey PRIMARY KEY (id),
    CONSTRAINT export_jobs_dedup_key_key UNIQUE (dedup_key),
    CONSTRAINT export_jobs_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
CREATE INDEX export_jobs_due_idx ON export_jobs (next_attempt_at, id) WHERE status = 'pending';
CREATE INDEX export_jobs_project_idx ON export_jobs (project_id, created_at);
