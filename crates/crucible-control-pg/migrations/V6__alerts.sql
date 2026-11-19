-- Channels, alert rules, their per-series state and their history, and the queue of evaluation ticks
-- (contract 11.1 and 11.7). Forward-only; no column has a default (contract 2.7).

CREATE TABLE channels (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    name text NOT NULL,
    kind text NOT NULL,
    config jsonb NOT NULL,
    secret_id uuid,
    enabled boolean NOT NULL,
    disabled_reason text,
    failures integer NOT NULL,
    last_test jsonb,
    version bigint NOT NULL,
    CONSTRAINT channels_pkey PRIMARY KEY (id),
    CONSTRAINT channels_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
CREATE INDEX channels_project_id_idx ON channels (project_id);

-- `spec` is the definition of the rule. `interval_secs` repeats its interval for the scheduler, and the last
-- three columns are written by evaluations, never by an edit.
CREATE TABLE alert_rules (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    name text NOT NULL,
    enabled boolean NOT NULL,
    spec jsonb NOT NULL,
    interval_secs integer NOT NULL,
    version bigint NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    last_eval_seq bigint,
    eval_failures integer NOT NULL,
    skipped_ticks bigint NOT NULL,
    CONSTRAINT alert_rules_pkey PRIMARY KEY (id),
    CONSTRAINT alert_rules_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
CREATE INDEX alert_rules_project_id_idx ON alert_rules (project_id);
CREATE INDEX alert_rules_enabled_idx ON alert_rules (id) WHERE enabled;

CREATE TABLE alert_state (
    rule_id uuid NOT NULL,
    series_key text NOT NULL,
    status text NOT NULL,
    since timestamptz NOT NULL,
    last_rows_at timestamptz NOT NULL,
    last_notified_at timestamptz,
    value bigint,
    CONSTRAINT alert_state_pkey PRIMARY KEY (rule_id, series_key),
    CONSTRAINT alert_state_rule_id_fkey FOREIGN KEY (rule_id) REFERENCES alert_rules (id) ON DELETE CASCADE
);

CREATE TABLE alert_events (
    id uuid NOT NULL,
    rule_id uuid NOT NULL,
    series_key text NOT NULL,
    kind text NOT NULL,
    from_status text NOT NULL,
    to_status text NOT NULL,
    value bigint,
    notified boolean NOT NULL,
    eval_seq bigint NOT NULL,
    at timestamptz NOT NULL,
    CONSTRAINT alert_events_pkey PRIMARY KEY (id),
    CONSTRAINT alert_events_rule_id_fkey FOREIGN KEY (rule_id) REFERENCES alert_rules (id) ON DELETE CASCADE
);
CREATE INDEX alert_events_rule_idx ON alert_events (rule_id, at DESC, id DESC);

-- The second claimed queue (contract 11.7): one row per rule and evaluation tick, with the tick in the
-- deduplication key, so two nodes that schedule the same tick write one row.
CREATE TABLE alert_evaluations (
    id uuid NOT NULL,
    dedup_key text NOT NULL,
    payload text NOT NULL,
    attempts integer NOT NULL,
    next_attempt_at timestamptz NOT NULL,
    status text NOT NULL,
    lease_until timestamptz,
    CONSTRAINT alert_evaluations_pkey PRIMARY KEY (id),
    CONSTRAINT alert_evaluations_dedup_key_key UNIQUE (dedup_key)
);
CREATE INDEX alert_evaluations_due_idx ON alert_evaluations (next_attempt_at, id) WHERE status = 'pending';
CREATE INDEX alert_evaluations_done_idx ON alert_evaluations (next_attempt_at) WHERE status = 'done';
CREATE INDEX notification_outbox_done_idx ON notification_outbox (next_attempt_at) WHERE status = 'done';
