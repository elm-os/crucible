-- Dashboards and their widgets (contract 11.1). Built-in dashboards are code, not rows. The dashboard's version
-- covers its widgets, so every widget change is written with the dashboard's compare-and-set. Forward-only; no
-- column has a default (contract 2.7).

CREATE TABLE dashboards (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    name text NOT NULL,
    description text NOT NULL,
    version bigint NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CONSTRAINT dashboards_pkey PRIMARY KEY (id),
    CONSTRAINT dashboards_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id)
);
CREATE INDEX dashboards_project_id_idx ON dashboards (project_id);

-- A widget id is unique within its dashboard, so the statement that rewrites a dashboard's widgets upserts by
-- (dashboard_id, id) and never touches another dashboard's rows.
CREATE TABLE widgets (
    dashboard_id uuid NOT NULL,
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    query jsonb,
    display jsonb NOT NULL,
    layout jsonb NOT NULL,
    CONSTRAINT widgets_pkey PRIMARY KEY (dashboard_id, id),
    CONSTRAINT widgets_dashboard_id_fkey FOREIGN KEY (dashboard_id) REFERENCES dashboards (id) ON DELETE CASCADE
);
