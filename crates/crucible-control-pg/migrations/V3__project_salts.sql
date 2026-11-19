-- The project salts of contract 11.1: one sealed random salt per project, written once and never changed.
-- The normalizer hashes MCP user subjects with it (contract 9.3). Forward-only; no column has a default
-- (contract 2.7). The row has no foreign key to projects: a salt is created on the first frame of a project,
-- and a failed insert would hold that frame.

CREATE TABLE project_salts (
    project_id uuid NOT NULL,
    key_id text NOT NULL,
    nonce bytea NOT NULL,
    ciphertext bytea NOT NULL,
    CONSTRAINT project_salts_pkey PRIMARY KEY (project_id)
);
