CREATE TABLE managed_changes_rebuilt (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id),
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
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
    state TEXT NOT NULL CHECK (state IN ('preparing', 'active', 'removing', 'abandoned')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(project_id, branch)
) STRICT;

INSERT INTO managed_changes_rebuilt
SELECT task_id, project_id, agent_id, worktree, branch, git_dir, common_dir,
       worktree_device, worktree_inode, git_dir_device, git_dir_inode,
       common_dir_device, common_dir_inode, base_sha, head_sha,
       published_head_sha, state, created_at_ms, updated_at_ms
FROM managed_changes;

DROP TABLE managed_changes;
ALTER TABLE managed_changes_rebuilt RENAME TO managed_changes;

CREATE UNIQUE INDEX managed_changes_one_active_agent
    ON managed_changes(project_id, agent_id)
    WHERE state IN ('preparing', 'active', 'removing');
