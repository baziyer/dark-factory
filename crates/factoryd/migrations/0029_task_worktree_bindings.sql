ALTER TABLE tasks ADD COLUMN worktree_path TEXT;
ALTER TABLE tasks ADD COLUMN worktree_branch TEXT;
ALTER TABLE tasks ADD COLUMN worktree_starting_head TEXT;
ALTER TABLE tasks ADD COLUMN worktree_git_dir TEXT;
ALTER TABLE tasks ADD COLUMN worktree_common_dir TEXT;
ALTER TABLE tasks ADD COLUMN worktree_device INTEGER;
ALTER TABLE tasks ADD COLUMN worktree_inode INTEGER;
ALTER TABLE tasks ADD COLUMN worktree_git_dir_device INTEGER;
ALTER TABLE tasks ADD COLUMN worktree_git_dir_inode INTEGER;
ALTER TABLE tasks ADD COLUMN worktree_common_dir_device INTEGER;
ALTER TABLE tasks ADD COLUMN worktree_common_dir_inode INTEGER;

CREATE INDEX tasks_worktree_branch_idx ON tasks(worktree_branch);
