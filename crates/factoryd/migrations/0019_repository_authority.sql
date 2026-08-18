CREATE TABLE project_repository_authority (
    project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    remote_url TEXT NOT NULL CHECK (length(remote_url) BETWEEN 1 AND 4096),
    base_branch TEXT NOT NULL CHECK (length(base_branch) BETWEEN 1 AND 255),
    updated_at_ms INTEGER NOT NULL
) STRICT;
