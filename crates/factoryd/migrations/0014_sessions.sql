-- Track 5: resident sessions replace the per-run ephemeral runner ledger.
-- See TRACK5-DESIGN.md section 1 and TRACK5-WIRE.md "Migration 0014".
--
-- `sessions` becomes the one row per agent's live-or-historical resident
-- interactive provider process (one PTY-backed `claude`/`codex` invocation
-- spanning many task episodes). `runs` becomes a task-episode inside a
-- session: everything that was really about the runner *process instance*
-- (runner_instance_id/runtime/protocol version, provider session identity,
-- observer health, exit code/signal) moves to `sessions`. `agents` drops the
-- provider-session pin (it now lives on `sessions`) and gains a durable
-- pause flag and a per-agent worktree.

-- Runs still open from the dead ephemeral-runner model cannot be adopted by
-- a resident session; force-close them before the rebuild below copies
-- terminal rows forward. Each row's own `updated_at_ms` stands in for
-- "closed at" -- this is pre-GA local state, not a fabricated backfill.
UPDATE runs
SET status = 'failed',
    status_since_ms = updated_at_ms,
    ended_at_ms = updated_at_ms,
    exit_code = NULL,
    exit_signal = NULL,
    failure_reason = 'unverifiable'
WHERE ended_at_ms IS NULL;

-- sessions -------------------------------------------------------------

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex')),
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
                             'post_tool_use', 'notification', 'stop', 'subagent_stop',
                             'session_end')
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

CREATE UNIQUE INDEX sessions_one_live_per_agent
    ON sessions(agent_id) WHERE ended_at_ms IS NULL;

CREATE UNIQUE INDEX sessions_one_owner_per_provider_session
    ON sessions(provider, provider_session_id) WHERE provider_session_id IS NOT NULL;

CREATE INDEX sessions_by_project_agent ON sessions(project_id, agent_id, id);

CREATE INDEX sessions_recoverable
    ON sessions(project_id, started_at_ms, id)
    WHERE ended_at_ms IS NULL;

-- agents -----------------------------------------------------------------
-- Drop the columns/index/triggers that moved to sessions; add the durable
-- pause flag and per-agent worktree (D2, D3).

DROP TRIGGER agents_provider_session_context_insert;
DROP TRIGGER agents_provider_session_context_update;
DROP INDEX agents_one_owner_per_provider_session;

CREATE TABLE agents_new (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    parent_agent_id TEXT,
    role TEXT NOT NULL CHECK (role IN ('orchestrator', 'worker')),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex')),
    paused INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    worktree TEXT CHECK (
        worktree IS NULL OR
        (length(worktree) BETWEEN 1 AND 4096 AND substr(worktree, 1, 1) = '/')
    ),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (id, project_id),
    CHECK (parent_agent_id IS NULL OR parent_agent_id <> id),
    FOREIGN KEY (parent_agent_id, project_id) REFERENCES agents(id, project_id)
) STRICT;

INSERT INTO agents_new (
    id, project_id, parent_agent_id, role, provider, created_at_ms, updated_at_ms
)
SELECT id, project_id, parent_agent_id, role, provider, created_at_ms, updated_at_ms
FROM agents;

DROP TABLE agents;
ALTER TABLE agents_new RENAME TO agents;

CREATE INDEX agents_by_project_parent ON agents(project_id, parent_agent_id, id);

-- runs ---------------------------------------------------------------------
-- Drop runner-process/decoder columns (moved to sessions); add session_id
-- and closed_by; keep stop_requested_at_ms (0011) and the failure/terminal
-- shape.

CREATE TABLE runs_new (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    session_id TEXT,
    parent_run_id TEXT,
    task_id TEXT,
    status TEXT NOT NULL CHECK (
        status IN ('starting', 'running', 'waiting', 'blocked', 'paused',
                   'succeeded', 'failed', 'stopped')
    ),
    activity TEXT CHECK (activity IS NULL OR length(activity) <= 512),
    wait_reason TEXT CHECK (wait_reason IS NULL OR length(wait_reason) <= 512),
    worktree TEXT NOT NULL CHECK (length(worktree) BETWEEN 1 AND 4096),
    started_at_ms INTEGER NOT NULL,
    status_since_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    closed_by TEXT CHECK (
        closed_by IN ('task_done', 'task_blocked', 'operator_cancel',
                       'operator_stop', 'session_ended')
    ),
    failure_reason TEXT CHECK (
        failure_reason IN ('protocol', 'provider', 'permission', 'limit',
                           'process', 'spawn', 'incomplete', 'unverifiable')
    ),
    stop_requested_at_ms INTEGER CHECK (stop_requested_at_ms IS NULL OR stop_requested_at_ms >= 0),
    UNIQUE (id, project_id),
    UNIQUE (id, agent_id),
    CHECK (parent_run_id IS NULL OR parent_run_id <> id),
    CHECK (
        (status IN ('succeeded', 'failed', 'stopped')
            AND ended_at_ms IS NOT NULL AND closed_by IS NOT NULL) OR
        (status IN ('starting', 'running', 'waiting', 'blocked', 'paused')
            AND ended_at_ms IS NULL AND closed_by IS NULL)
    ),
    CHECK ((status = 'failed') = (failure_reason IS NOT NULL)),
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (session_id, project_id) REFERENCES sessions(id, project_id),
    FOREIGN KEY (parent_run_id, project_id) REFERENCES runs(id, project_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

INSERT INTO runs_new (
    id, project_id, agent_id, session_id, parent_run_id, task_id, status,
    activity, wait_reason, worktree, started_at_ms, status_since_ms,
    updated_at_ms, ended_at_ms, closed_by, failure_reason, stop_requested_at_ms
)
SELECT id, project_id, agent_id, NULL, parent_run_id, task_id, status,
    activity, wait_reason, worktree, started_at_ms, status_since_ms,
    updated_at_ms, ended_at_ms,
    CASE WHEN ended_at_ms IS NOT NULL THEN 'session_ended' END,
    failure_reason, stop_requested_at_ms
FROM runs;

DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE UNIQUE INDEX runs_one_open_per_agent ON runs(agent_id) WHERE ended_at_ms IS NULL;

CREATE UNIQUE INDEX runs_one_open_per_task ON runs(task_id)
    WHERE task_id IS NOT NULL AND ended_at_ms IS NULL;

CREATE INDEX runs_by_status_started ON runs(status, started_at_ms, id);

CREATE INDEX runs_by_session ON runs(session_id, id);

-- tasks ----------------------------------------------------------------

ALTER TABLE tasks
    ADD COLUMN blocked_reason TEXT
    CHECK (blocked_reason IS NULL OR length(CAST(blocked_reason AS BLOB)) <= 4096);

-- agent_messages ---------------------------------------------------------
-- A message may be delivered without a run ever opening (a standalone
-- nudge), so delivery is keyed to the session it was typed/replied into,
-- with the run id recorded alongside when one happened to be open.

ALTER TABLE agent_messages
    ADD COLUMN delivered_session_id TEXT REFERENCES sessions(id);
