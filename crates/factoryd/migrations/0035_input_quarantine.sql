-- Provider-neutral external input is immutable quarantine state. This schema
-- has no foreign key or trigger that can create a task, message, or run.

CREATE TABLE input_envelopes (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    source_kind TEXT NOT NULL CHECK (
        length(CAST(source_kind AS BLOB)) BETWEEN 1 AND 64
        AND source_kind NOT GLOB '*[^a-z0-9_-]*'
    ),
    source_id TEXT NOT NULL CHECK (
        length(CAST(source_id AS BLOB)) BETWEEN 1 AND 512
    ),
    delivery_id TEXT NOT NULL CHECK (
        length(CAST(delivery_id AS BLOB)) BETWEEN 1 AND 256
    ),
    source_revision TEXT NOT NULL CHECK (
        length(CAST(source_revision AS BLOB)) BETWEEN 1 AND 256
    ),
    content TEXT NOT NULL CHECK (
        length(CAST(content AS BLOB)) <= 65536
    ),
    content_digest TEXT NOT NULL CHECK (
        length(content_digest) = 64
        AND content_digest NOT GLOB '*[^0-9a-f]*'
    ),
    request_digest TEXT NOT NULL CHECK (
        length(request_digest) = 64
        AND request_digest NOT GLOB '*[^0-9a-f]*'
    ),
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms >= 0),
    UNIQUE (id, project_id),
    UNIQUE (source_kind, delivery_id)
) STRICT;

CREATE TABLE work_candidates (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    origin_envelope_id TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (
        length(CAST(source_kind AS BLOB)) BETWEEN 1 AND 64
        AND source_kind NOT GLOB '*[^a-z0-9_-]*'
    ),
    source_id TEXT NOT NULL CHECK (
        length(CAST(source_id AS BLOB)) BETWEEN 1 AND 512
    ),
    source_revision TEXT NOT NULL CHECK (
        length(CAST(source_revision AS BLOB)) BETWEEN 1 AND 256
    ),
    content_digest TEXT NOT NULL CHECK (
        length(content_digest) = 64
        AND content_digest NOT GLOB '*[^0-9a-f]*'
    ),
    status TEXT NOT NULL CHECK (
        status IN ('quarantined', 'stale', 'rejected')
    ),
    status_reason TEXT CHECK (
        status_reason IS NULL OR
        length(CAST(status_reason AS BLOB)) BETWEEN 1 AND 1024
    ),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    UNIQUE (id, project_id),
    UNIQUE (id, project_id, source_kind, source_id),
    UNIQUE (project_id, source_kind, source_id, source_revision),
    FOREIGN KEY (origin_envelope_id, project_id)
        REFERENCES input_envelopes(id, project_id),
    CHECK (
        (status = 'quarantined' AND status_reason IS NULL) OR
        (status IN ('stale', 'rejected') AND status_reason IS NOT NULL)
    )
) STRICT;

CREATE TABLE work_candidate_envelopes (
    envelope_id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    FOREIGN KEY (envelope_id, project_id)
        REFERENCES input_envelopes(id, project_id),
    FOREIGN KEY (candidate_id, project_id)
        REFERENCES work_candidates(id, project_id)
) STRICT;

CREATE TABLE input_sources (
    project_id TEXT NOT NULL REFERENCES projects(id),
    source_kind TEXT NOT NULL,
    source_id TEXT NOT NULL,
    current_candidate_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    PRIMARY KEY (project_id, source_kind, source_id),
    FOREIGN KEY (
        current_candidate_id, project_id, source_kind, source_id
    ) REFERENCES work_candidates(id, project_id, source_kind, source_id)
) STRICT;

CREATE INDEX input_envelopes_by_project
    ON input_envelopes(project_id, id);
CREATE INDEX work_candidates_by_project
    ON work_candidates(project_id, id);
CREATE INDEX work_candidates_by_status
    ON work_candidates(project_id, status, id);

-- `connector_events` is inert historical metadata from the deleted webhook
-- implementation. Preserve it without adopting it as executable authority.
