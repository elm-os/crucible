-- The identity tables of contract 11.1 that the base lacks: sign-in providers, linked provider accounts,
-- sign-in flows, one-time tokens and invitations. Forward-only; no column has a default (contract 2.7).

CREATE TABLE auth_providers (
    id uuid NOT NULL,
    kind text NOT NULL,
    display_name text NOT NULL,
    issuer text NOT NULL,
    client_id text NOT NULL,
    secret_id uuid,
    scopes text[] NOT NULL,
    allowed_domains text[] NOT NULL,
    enabled boolean NOT NULL,
    seeded boolean NOT NULL,
    CONSTRAINT auth_providers_pkey PRIMARY KEY (id)
);
-- Configuration seeds at most one provider per kind, and a restart keeps the row and its id.
CREATE UNIQUE INDEX auth_providers_seeded_kind_key ON auth_providers (kind) WHERE seeded;

CREATE TABLE oauth_accounts (
    provider_id uuid NOT NULL,
    subject text NOT NULL,
    user_id uuid NOT NULL,
    CONSTRAINT oauth_accounts_pkey PRIMARY KEY (provider_id, subject),
    CONSTRAINT oauth_accounts_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES auth_providers (id) ON DELETE CASCADE,
    CONSTRAINT oauth_accounts_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);
CREATE INDEX oauth_accounts_user_id_idx ON oauth_accounts (user_id);

-- The primary key is the SHA-256 of the state parameter; the PKCE verifier is sealed under the master key.
CREATE TABLE auth_flows (
    state_hash bytea NOT NULL,
    provider_id uuid NOT NULL,
    sealed_verifier bytea NOT NULL,
    nonce text NOT NULL,
    next_path text NOT NULL,
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    CONSTRAINT auth_flows_pkey PRIMARY KEY (state_hash)
);
CREATE INDEX auth_flows_expires_at_idx ON auth_flows (expires_at);

-- The primary key is the SHA-256 of the token; the token itself is never stored.
CREATE TABLE one_time_tokens (
    token_hash bytea NOT NULL,
    kind text NOT NULL,
    subject text NOT NULL,
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    CONSTRAINT one_time_tokens_pkey PRIMARY KEY (token_hash)
);
CREATE INDEX one_time_tokens_kind_subject_idx ON one_time_tokens (kind, subject);
CREATE INDEX one_time_tokens_expires_at_idx ON one_time_tokens (expires_at);

-- project_roles holds one `<project id>:<role>` entry per project membership the invitation grants.
CREATE TABLE invitations (
    id uuid NOT NULL,
    email text NOT NULL,
    org_role text NOT NULL,
    project_roles text[] NOT NULL,
    expires_at timestamptz NOT NULL,
    accepted_at timestamptz,
    revoked_at timestamptz,
    CONSTRAINT invitations_pkey PRIMARY KEY (id)
);
CREATE INDEX invitations_email_idx ON invitations (email);
