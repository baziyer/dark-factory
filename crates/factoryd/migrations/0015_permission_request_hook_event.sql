-- Widens `sessions.last_hook_event`'s CHECK constraint to also accept
-- 'permission_request': Codex 0.147.0's approval-prompt hook event, added
-- so a session blocked on the provider's own approval prompt shows
-- `waiting_for_input` instead of `working` (docs/dogfood/2026-08-17.md,
-- "Sessions and hooks"). 0014 landed in the v0.1.0 release already
-- dogfooded on a real `$DARK_FACTORY_HOME`, so unlike the two 5C
-- amendments noted in `0014_sessions.sql`'s own header, this cannot be
-- amended in place -- it needs its own migration, applied on top of an
-- already-migrated real database.
--
-- SQLite has no `ALTER TABLE ... ALTER CONSTRAINT`, so the table is
-- rebuilt exactly as `0014_sessions.sql` defined it, with only the
-- `last_hook_event` CHECK's allowed list widened. `runs.session_id` and
-- `agent_messages.delivered_session_id` both reference `sessions`, so this
-- runs with `PRAGMA foreign_keys` off (toggled by `store::migrate`, same
-- as 0014) and is verified with `PRAGMA foreign_key_check` afterward.

CREATE TABLE sessions_new (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'shell')),
    provider_session_id TEXT CHECK (
        provider_session_id IS NULL OR length(provider_session_id) BETWEEN 1 AND 256
    ),
    worktree TEXT NOT NULL CHECK (
        length(worktree) BETWEEN 1 AND 4096 AND substr(worktree, 1, 1) = '/'
    ),
    codex_home TEXT CHECK (
        codex_home IS NULL OR
        (provider = 'codex' AND length(codex_home) BETWEEN 1 AND 4096
         AND substr(codex_home, 1, 1) = '/')
    ),
    hook_token TEXT NOT NULL CHECK (length(hook_token) = 64 AND hook_token GLOB '[0-9a-f]*'),
    state TEXT NOT NULL CHECK (
        state IN ('starting', 'idle', 'working', 'waiting_for_input', 'stopped', 'failed')
    ),
    state_since_ms INTEGER NOT NULL,
    activity TEXT CHECK (activity IS NULL OR length(activity) <= 512),
    activity_inferred INTEGER NOT NULL DEFAULT 0 CHECK (activity_inferred IN (0, 1)),
    wait_reason TEXT CHECK (wait_reason IS NULL OR length(wait_reason) <= 512),
    observer_health TEXT NOT NULL DEFAULT 'unknown'
        CHECK (observer_health IN ('unknown', 'healthy', 'degraded')),
    observer_health_since_ms INTEGER NOT NULL DEFAULT 0 CHECK (observer_health_since_ms >= 0),
    runner_instance_id TEXT NOT NULL UNIQUE,
    runner_runtime TEXT NOT NULL UNIQUE CHECK (length(runner_runtime) BETWEEN 1 AND 4096),
    runner_protocol_version INTEGER NOT NULL CHECK (runner_protocol_version > 0),
    last_hook_event TEXT CHECK (
        last_hook_event IN ('session_start', 'user_prompt_submit', 'pre_tool_use',
                             'permission_request', 'post_tool_use', 'notification',
                             'stop', 'subagent_stop', 'session_end')
    ),
    last_hook_at_ms INTEGER,
    started_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    exit_code INTEGER CHECK (exit_code IS NULL OR exit_code >= 0),
    exit_signal INTEGER CHECK (exit_signal IS NULL OR exit_signal > 0),
    stop_requested_at_ms INTEGER CHECK (stop_requested_at_ms IS NULL OR stop_requested_at_ms >= 0),
    UNIQUE (id, project_id),
    UNIQUE (id, agent_id),
    CHECK (exit_code IS NULL OR exit_signal IS NULL),
    CHECK ((state IN ('stopped', 'failed')) = (ended_at_ms IS NOT NULL)),
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id)
) STRICT;

INSERT INTO sessions_new (
    id, project_id, agent_id, provider, provider_session_id, worktree, codex_home,
    hook_token, state, state_since_ms, activity, activity_inferred, wait_reason,
    observer_health, observer_health_since_ms, runner_instance_id, runner_runtime,
    runner_protocol_version, last_hook_event, last_hook_at_ms, started_at_ms,
    updated_at_ms, ended_at_ms, exit_code, exit_signal, stop_requested_at_ms
)
SELECT
    id, project_id, agent_id, provider, provider_session_id, worktree, codex_home,
    hook_token, state, state_since_ms, activity, activity_inferred, wait_reason,
    observer_health, observer_health_since_ms, runner_instance_id, runner_runtime,
    runner_protocol_version, last_hook_event, last_hook_at_ms, started_at_ms,
    updated_at_ms, ended_at_ms, exit_code, exit_signal, stop_requested_at_ms
FROM sessions;

DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;

CREATE UNIQUE INDEX sessions_one_live_per_agent
    ON sessions(agent_id) WHERE ended_at_ms IS NULL;

CREATE UNIQUE INDEX sessions_one_owner_per_provider_session
    ON sessions(provider, provider_session_id)
    WHERE provider_session_id IS NOT NULL AND ended_at_ms IS NULL;

CREATE INDEX sessions_by_project_agent ON sessions(project_id, agent_id, id);

CREATE INDEX sessions_recoverable
    ON sessions(project_id, started_at_ms, id)
    WHERE ended_at_ms IS NULL;
