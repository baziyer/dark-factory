CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    parent_agent_id TEXT,
    role TEXT NOT NULL CHECK (role IN ('orchestrator', 'worker')),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex')),
    provider_session_id TEXT CHECK (
        provider_session_id IS NULL OR length(provider_session_id) BETWEEN 1 AND 256
    ),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (id, project_id),
    CHECK (parent_agent_id IS NULL OR parent_agent_id <> id),
    FOREIGN KEY (parent_agent_id, project_id) REFERENCES agents(id, project_id)
) STRICT;

CREATE INDEX agents_by_project_parent ON agents(project_id, parent_agent_id, id);

CREATE UNIQUE INDEX agents_one_owner_per_provider_session
    ON agents(provider, provider_session_id)
    WHERE provider_session_id IS NOT NULL;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    parent_run_id TEXT,
    task_id TEXT,
    status TEXT NOT NULL CHECK (
        status IN ('starting', 'running', 'waiting', 'blocked', 'paused',
                   'succeeded', 'failed', 'stopped')
    ),
    activity TEXT CHECK (activity IS NULL OR length(activity) <= 512),
    wait_reason TEXT CHECK (wait_reason IS NULL OR length(wait_reason) <= 512),
    worktree TEXT NOT NULL CHECK (length(worktree) BETWEEN 1 AND 4096),
    provider_session_id TEXT CHECK (
        provider_session_id IS NULL OR length(provider_session_id) BETWEEN 1 AND 256
    ),
    resumes_provider_session INTEGER NOT NULL CHECK (resumes_provider_session IN (0, 1)),
    provider_session_confirmed_at_ms INTEGER,
    runner_instance_id TEXT NOT NULL UNIQUE,
    runner_protocol_version INTEGER NOT NULL CHECK (runner_protocol_version > 0),
    runner_runtime TEXT NOT NULL UNIQUE CHECK (length(runner_runtime) BETWEEN 1 AND 4096),
    last_runner_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_runner_sequence >= 0),
    terminal_runner_sequence INTEGER,
    runner_acknowledged_at_ms INTEGER,
    runner_terminal_kind TEXT CHECK (runner_terminal_kind IN ('exited', 'spawn_failed')),
    started_at_ms INTEGER NOT NULL,
    status_since_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    exit_code INTEGER CHECK (exit_code IS NULL OR exit_code >= 0),
    exit_signal INTEGER CHECK (exit_signal IS NULL OR exit_signal > 0),
    failure_reason TEXT CHECK (
        failure_reason IN ('protocol', 'provider', 'permission', 'limit',
                           'process', 'spawn', 'incomplete', 'unverifiable')
    ),
    UNIQUE (id, project_id),
    UNIQUE (id, agent_id),
    CHECK (parent_run_id IS NULL OR parent_run_id <> id),
    CHECK (resumes_provider_session = 0 OR provider_session_id IS NOT NULL),
    CHECK (provider_session_confirmed_at_ms IS NULL OR provider_session_id IS NOT NULL),
    CHECK (
        (terminal_runner_sequence IS NULL AND runner_terminal_kind IS NULL) OR
        (terminal_runner_sequence > 0 AND
         terminal_runner_sequence = last_runner_sequence AND
         runner_terminal_kind IS NOT NULL AND
         status IN ('succeeded', 'failed', 'stopped'))
    ),
    CHECK (runner_acknowledged_at_ms IS NULL OR terminal_runner_sequence IS NOT NULL),
    CHECK (exit_code IS NULL OR exit_signal IS NULL),
    CHECK (
        runner_terminal_kind <> 'spawn_failed' OR
        (status = 'failed' AND failure_reason = 'spawn' AND
         exit_code IS NULL AND exit_signal IS NULL)
    ),
    CHECK (runner_terminal_kind <> 'exited' OR failure_reason IS NULL OR failure_reason <> 'spawn'),
    CHECK (
        (status IN ('succeeded', 'failed', 'stopped') AND ended_at_ms IS NOT NULL) OR
        (status IN ('starting', 'running', 'waiting', 'blocked', 'paused') AND ended_at_ms IS NULL)
    ),
    CHECK ((status = 'failed') = (failure_reason IS NOT NULL)),
    CHECK (status <> 'succeeded' OR (exit_code = 0 AND exit_signal IS NULL)),
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (parent_run_id, project_id) REFERENCES runs(id, project_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE UNIQUE INDEX runs_one_open_per_agent
    ON runs(agent_id)
    WHERE ended_at_ms IS NULL;

CREATE UNIQUE INDEX runs_one_open_per_task
    ON runs(task_id)
    WHERE task_id IS NOT NULL
      AND ended_at_ms IS NULL;

CREATE INDEX runs_by_status_started
    ON runs(status, started_at_ms, id);

CREATE INDEX runs_recoverable
    ON runs(project_id, started_at_ms, id)
    WHERE ended_at_ms IS NULL
       OR (terminal_runner_sequence IS NOT NULL AND runner_acknowledged_at_ms IS NULL);
