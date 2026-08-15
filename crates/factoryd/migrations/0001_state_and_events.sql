CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root TEXT NOT NULL UNIQUE,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    parent_task_id TEXT,
    assigned_agent_id TEXT,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    status TEXT NOT NULL CHECK (
        status IN ('queued', 'running', 'blocked', 'succeeded', 'failed', 'cancelled')
    ),
    priority INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (id, project_id),
    CHECK (parent_task_id IS NULL OR parent_task_id <> id),
    FOREIGN KEY (parent_task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE INDEX tasks_by_project_status_priority
    ON tasks(project_id, status, priority DESC, created_at_ms, id);

CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    occurred_at_ms INTEGER NOT NULL,
    project_id TEXT,
    task_id TEXT,
    agent_id TEXT,
    run_id TEXT,
    kind TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    payload_json TEXT NOT NULL
) STRICT;

CREATE INDEX events_by_project_id ON events(project_id, id);
