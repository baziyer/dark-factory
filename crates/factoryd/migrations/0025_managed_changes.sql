CREATE TABLE managed_changes (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    worktree TEXT NOT NULL CHECK (length(worktree) BETWEEN 1 AND 4096),
    branch TEXT NOT NULL CHECK (length(branch) BETWEEN 1 AND 255),
    git_dir TEXT NOT NULL CHECK (length(git_dir) BETWEEN 1 AND 4096),
    common_dir TEXT NOT NULL CHECK (length(common_dir) BETWEEN 1 AND 4096),
    worktree_device INTEGER NOT NULL,
    worktree_inode INTEGER NOT NULL,
    git_dir_device INTEGER NOT NULL,
    git_dir_inode INTEGER NOT NULL,
    common_dir_device INTEGER NOT NULL,
    common_dir_inode INTEGER NOT NULL,
    base_sha TEXT NOT NULL CHECK (length(base_sha) BETWEEN 1 AND 128),
    head_sha TEXT NOT NULL CHECK (length(head_sha) BETWEEN 1 AND 128),
    published_head_sha TEXT CHECK (published_head_sha IS NULL OR length(published_head_sha) BETWEEN 1 AND 128),
    state TEXT NOT NULL CHECK (state IN ('active', 'abandoned')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(project_id, branch)
) STRICT;

CREATE UNIQUE INDEX managed_changes_one_active_agent
    ON managed_changes(project_id, agent_id) WHERE state = 'active';
