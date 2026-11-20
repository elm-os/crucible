-- Media (contracts 11.1 and 14.3): the stored objects, one per project and SHA-256, and their uses. An object's
-- key in the object store derives from its id. A use holds its object through a foreign key without a cascade:
-- an object row can be deleted only once no use refers to it, and the garbage collector deletes the stored
-- object only after the row is gone. No column has a default (contract 2.7).

CREATE TABLE media_objects (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    sha256 bytea NOT NULL,
    content_type text NOT NULL,
    len bigint NOT NULL,
    state text NOT NULL,
    created_at timestamptz NOT NULL,
    uploaded_at timestamptz,
    CONSTRAINT media_objects_pkey PRIMARY KEY (id),
    CONSTRAINT media_objects_project_id_sha256_key UNIQUE (project_id, sha256),
    CONSTRAINT media_objects_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects (id),
    CONSTRAINT media_objects_state_check CHECK (state IN ('pending', 'uploaded')),
    CONSTRAINT media_objects_sha256_check CHECK (length(sha256) = 32)
);
CREATE INDEX media_objects_gc_idx ON media_objects (state, created_at, id);

-- One row per use: a payload field of a span, or of the trace when `span_id` is empty. `field` is empty when the
-- use names no field. `target_time` is when the use was recorded; retention and deletion requests measure by it.
CREATE TABLE media_refs (
    project_id uuid NOT NULL,
    media_id uuid NOT NULL,
    trace_id bytea NOT NULL,
    span_id bytea NOT NULL,
    field text NOT NULL,
    target_time timestamptz NOT NULL,
    CONSTRAINT media_refs_pkey PRIMARY KEY (media_id, trace_id, span_id, field),
    CONSTRAINT media_refs_media_id_fkey FOREIGN KEY (media_id) REFERENCES media_objects (id)
);
CREATE INDEX media_refs_trace_idx ON media_refs (project_id, trace_id);
CREATE INDEX media_refs_time_idx ON media_refs (project_id, target_time);
