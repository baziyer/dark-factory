CREATE TABLE IF NOT EXISTS changes (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    source_issue TEXT NOT NULL CHECK (length(source_issue) BETWEEN 1 AND 256),
    source_task_id TEXT,
    author_agent_id TEXT NOT NULL CHECK (length(author_agent_id) BETWEEN 1 AND 128),
    author_run_id TEXT,
    branch TEXT NOT NULL CHECK (length(branch) BETWEEN 1 AND 255),
    pr_number INTEGER,
    pr_url TEXT,
    head_sha TEXT NOT NULL CHECK (length(head_sha) BETWEEN 1 AND 128),
    base_branch TEXT NOT NULL CHECK (length(base_branch) BETWEEN 1 AND 255),
    current_base_sha TEXT,
    state TEXT NOT NULL CHECK (state IN ('authored', 'review_requested', 'findings',
        'author_responding', 're_review', 'satisfied', 'integration_ready',
        'integrated', 'released', 'abandoned')),
    reviewer_agent_id TEXT,
    reviewer_run_id TEXT,
    reviewed_sha TEXT,
    checks_status TEXT NOT NULL CHECK (checks_status IN ('pending', 'failed', 'green')),
    checks_sha TEXT,
    checks_source TEXT CHECK (checks_source IS NULL OR checks_source IN ('operator', 'connector')),
    ready_by_agent_id TEXT,
    ready_sha TEXT,
    abandoned_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS change_findings (
    change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 10000),
    description TEXT NOT NULL CHECK (length(description) BETWEEN 1 AND 4096),
    author_disposition TEXT,
    reviewer_resolution TEXT,
    PRIMARY KEY (change_id, number)
) STRICT;

CREATE INDEX IF NOT EXISTS changes_updated_id ON changes(updated_at_ms, id);
