ALTER TABLE sessions
ADD COLUMN principal_version INTEGER NOT NULL DEFAULT 0
CHECK (principal_version IN (0, 1));

CREATE UNIQUE INDEX sessions_one_live_principal_hook_token
ON sessions(hook_token)
WHERE ended_at_ms IS NULL AND principal_version = 1;
