-- Replace resident-session delivery authority with one run/attempt ledger.
-- Rust preflight refuses this migration unless every legacy process, delivery,
-- and run is already quiescent; uncertain external effects are never guessed.

CREATE TABLE changes (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    task_id TEXT,
    task_incarnation_id TEXT,
    branch TEXT CHECK (
        branch IS NULL OR length(CAST(branch AS BLOB)) BETWEEN 1 AND 255
    ),
    worktree TEXT NOT NULL CHECK (
        length(CAST(worktree AS BLOB)) BETWEEN 1 AND 4096
        AND substr(worktree, 1, 1) = '/'
    ),
    ready_at_ms INTEGER CHECK (ready_at_ms IS NULL OR ready_at_ms >= 0),
    retained_reason TEXT CHECK (
        retained_reason IS NULL OR
        length(CAST(retained_reason AS BLOB)) BETWEEN 1 AND 4096
    ),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    UNIQUE (id, project_id),
    UNIQUE (id, project_id, task_id),
    CHECK (
        (task_id IS NULL AND task_incarnation_id IS NULL) OR
        (task_id IS NOT NULL AND task_incarnation_id IS NOT NULL)
    ),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE UNIQUE INDEX changes_one_per_task_incarnation
    ON changes(project_id, task_incarnation_id)
    WHERE task_incarnation_id IS NOT NULL;

-- Preserve legacy per-agent source paths as unlinked retained data. Stage 2
-- decides how new Change worktrees are provisioned; this migration never
-- inspects, moves, cleans, adopts, or removes one.
INSERT INTO changes (
    id, project_id, task_id, task_incarnation_id, branch, worktree,
    ready_at_ms, retained_reason, created_at_ms, updated_at_ms
)
SELECT
    'legacy-agent-' || id, project_id, NULL, NULL, NULL, worktree,
    updated_at_ms, 'retained legacy agent source path from schema 29',
    created_at_ms, updated_at_ms
FROM agents
WHERE worktree IS NOT NULL;

CREATE TABLE runs_new (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    task_incarnation_id TEXT NOT NULL,
    admitted_task_work_revision INTEGER NOT NULL CHECK (admitted_task_work_revision >= 0),
    change_id TEXT,
    parent_run_id TEXT,
    worktree TEXT NOT NULL CHECK (
        length(CAST(worktree AS BLOB)) BETWEEN 1 AND 4096
        AND substr(worktree, 1, 1) = '/'
    ),
    phase TEXT NOT NULL CHECK (
        phase IN ('admitted', 'running', 'finalizing', 'terminal')
    ),
    outcome TEXT CHECK (
        outcome IS NULL OR outcome IN ('succeeded', 'blocked', 'failed', 'cancelled')
    ),
    outcome_detail TEXT CHECK (
        outcome_detail IS NULL OR length(CAST(outcome_detail AS BLOB)) <= 4096
    ),
    outcome_result TEXT CHECK (
        outcome_result IS NULL OR length(CAST(outcome_result AS BLOB)) <= 131072
    ),
    capability_digest TEXT UNIQUE CHECK (
        capability_digest IS NULL OR
        (length(capability_digest) = 64 AND capability_digest GLOB '[0-9a-f]*')
    ),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'shell')),
    runtime_model TEXT,
    runtime_reasoning_effort TEXT,
    runtime_permission_mode TEXT,
    runtime_control_mode TEXT,
    activity TEXT CHECK (activity IS NULL OR length(activity) <= 512),
    wait_reason TEXT CHECK (wait_reason IS NULL OR length(wait_reason) <= 512),
    observer_health TEXT NOT NULL DEFAULT 'unknown'
        CHECK (observer_health IN ('unknown', 'healthy', 'degraded')),
    observer_reason TEXT CHECK (
        observer_reason IS NULL OR length(CAST(observer_reason AS BLOB)) <= 512
    ),
    runner_instance_id TEXT UNIQUE,
    runner_runtime TEXT UNIQUE CHECK (
        runner_runtime IS NULL OR
        (length(CAST(runner_runtime AS BLOB)) BETWEEN 1 AND 4096
         AND substr(runner_runtime, 1, 1) = '/')
    ),
    runner_protocol_version INTEGER CHECK (
        runner_protocol_version IS NULL OR runner_protocol_version > 0
    ),
    last_runner_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_runner_sequence >= 0),
    terminal_runner_sequence INTEGER,
    runner_reconciled_at_ms INTEGER,
    stop_requested_at_ms INTEGER CHECK (stop_requested_at_ms IS NULL OR stop_requested_at_ms >= 0),
    admitted_at_ms INTEGER NOT NULL CHECK (admitted_at_ms >= 0),
    running_at_ms INTEGER CHECK (running_at_ms IS NULL OR running_at_ms >= admitted_at_ms),
    finalizing_at_ms INTEGER CHECK (finalizing_at_ms IS NULL OR finalizing_at_ms >= admitted_at_ms),
    phase_since_ms INTEGER NOT NULL CHECK (phase_since_ms >= admitted_at_ms),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= admitted_at_ms),
    ended_at_ms INTEGER CHECK (ended_at_ms IS NULL OR ended_at_ms >= admitted_at_ms),
    exit_code INTEGER CHECK (exit_code IS NULL OR exit_code >= 0),
    exit_signal INTEGER CHECK (exit_signal IS NULL OR exit_signal > 0),
    UNIQUE (id, project_id),
    UNIQUE (id, agent_id),
    CHECK (parent_run_id IS NULL OR parent_run_id <> id),
    CHECK (
        (phase IN ('admitted', 'running') AND outcome IS NULL
            AND finalizing_at_ms IS NULL AND ended_at_ms IS NULL) OR
        (phase = 'finalizing' AND outcome IS NOT NULL
            AND finalizing_at_ms IS NOT NULL AND ended_at_ms IS NULL) OR
        (phase = 'terminal' AND outcome IS NOT NULL
            AND finalizing_at_ms IS NOT NULL AND ended_at_ms IS NOT NULL)
    ),
    CHECK (phase <> 'running' OR running_at_ms IS NOT NULL),
    CHECK (
        phase = 'terminal' OR
        (capability_digest IS NOT NULL AND runner_instance_id IS NOT NULL
         AND runner_runtime IS NOT NULL AND runner_protocol_version IS NOT NULL)
    ),
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (change_id, project_id, task_id)
        REFERENCES changes(id, project_id, task_id),
    FOREIGN KEY (parent_run_id, project_id) REFERENCES runs_new(id, project_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

-- Only terminal historical runs are copied. Rust preflight rejects every
-- nonterminal legacy row before this SQL is entered.
INSERT INTO runs_new (
    id, project_id, agent_id, task_id, task_incarnation_id,
    admitted_task_work_revision, change_id, parent_run_id, worktree,
    phase, outcome, outcome_detail, outcome_result, capability_digest,
    provider, runtime_model, runtime_reasoning_effort, runtime_permission_mode,
    runtime_control_mode, activity, wait_reason, observer_health, observer_reason,
    runner_instance_id, runner_runtime, runner_protocol_version,
    last_runner_sequence, terminal_runner_sequence, runner_reconciled_at_ms,
    stop_requested_at_ms, admitted_at_ms, running_at_ms, finalizing_at_ms,
    phase_since_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal
)
SELECT
    r.id, r.project_id, r.agent_id, r.task_id, t.incarnation_id,
    t.work_revision, NULL, r.parent_run_id, r.worktree,
    'terminal',
    CASE r.status
        WHEN 'succeeded' THEN 'succeeded'
        WHEN 'blocked' THEN 'blocked'
        WHEN 'stopped' THEN CASE r.closed_by
            WHEN 'task_blocked' THEN 'blocked'
            ELSE 'cancelled'
        END
        ELSE 'failed'
    END,
    CASE r.status
        WHEN 'succeeded' THEN NULL
        WHEN 'blocked' THEN COALESCE(t.blocked_reason, 'legacy block')
        WHEN 'stopped' THEN CASE r.closed_by
            WHEN 'task_blocked' THEN COALESCE(t.blocked_reason, 'legacy block')
            ELSE COALESCE(r.closed_by, 'legacy stop')
        END
        ELSE COALESCE(r.failure_reason, r.closed_by, 'unverifiable')
    END,
    NULL, NULL,
    a.provider,
    s.runtime_model, s.runtime_reasoning_effort, s.runtime_permission_mode,
    s.runtime_control_mode,
    r.activity, r.wait_reason,
    COALESCE(s.observer_health, 'unknown'), s.observer_reason,
    NULL, NULL, s.runner_protocol_version,
    0, NULL, NULL,
    r.stop_requested_at_ms, r.started_at_ms, r.started_at_ms, r.status_since_ms,
    r.status_since_ms, r.updated_at_ms, r.ended_at_ms, NULL, NULL
FROM runs r
JOIN agents a ON a.id = r.agent_id AND a.project_id = r.project_id
JOIN tasks t ON t.id = r.task_id AND t.project_id = r.project_id
LEFT JOIN sessions s ON s.id = r.session_id;

DROP INDEX IF EXISTS runs_one_open_per_session;
DROP INDEX IF EXISTS runs_one_open_per_agent;
DROP INDEX IF EXISTS runs_one_open_per_task;
DROP INDEX IF EXISTS runs_by_status_started;
DROP INDEX IF EXISTS runs_by_session;
DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE UNIQUE INDEX runs_one_open_per_agent
    ON runs(agent_id) WHERE phase <> 'terminal';
CREATE UNIQUE INDEX runs_one_open_per_task
    ON runs(task_id) WHERE task_id IS NOT NULL AND phase <> 'terminal';
CREATE INDEX runs_by_phase_admitted
    ON runs(phase, admitted_at_ms, id);
CREATE INDEX runs_recoverable
    ON runs(project_id, admitted_at_ms, id)
    WHERE phase <> 'terminal';

CREATE TABLE resources (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    kind TEXT NOT NULL CHECK (
        kind IN ('runner_process', 'provider_process', 'process_group',
                 'runtime_root')
    ),
    state TEXT NOT NULL CHECK (
        state IN ('declared', 'active', 'releasing', 'released', 'unresolved')
    ),
    locator TEXT NOT NULL CHECK (length(CAST(locator AS BLOB)) BETWEEN 2 AND 4096),
    birth_fingerprint TEXT CHECK (
        birth_fingerprint IS NULL OR
        length(CAST(birth_fingerprint AS BLOB)) BETWEEN 1 AND 1024
    ),
    retry_count INTEGER NOT NULL DEFAULT 0 CHECK (retry_count >= 0),
    last_failure TEXT CHECK (
        last_failure IS NULL OR length(CAST(last_failure AS BLOB)) <= 4096
    ),
    declared_at_ms INTEGER NOT NULL CHECK (declared_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= declared_at_ms),
    released_at_ms INTEGER CHECK (released_at_ms IS NULL OR released_at_ms >= declared_at_ms),
    UNIQUE (run_id, kind, locator),
    CHECK ((state = 'released') = (released_at_ms IS NOT NULL))
) STRICT;

CREATE INDEX resources_by_run_state ON resources(run_id, state, kind, id);
CREATE INDEX resources_reconcile ON resources(state, updated_at_ms, id)
    WHERE state <> 'released';

CREATE TABLE agent_messages_new (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    sender_agent_id TEXT,
    recipient_agent_id TEXT NOT NULL,
    body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) BETWEEN 1 AND 65536),
    created_at_ms INTEGER NOT NULL,
    delivered_at_ms INTEGER,
    delivered_run_id TEXT REFERENCES runs(id),
    FOREIGN KEY (sender_agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (recipient_agent_id, project_id) REFERENCES agents(id, project_id),
    CHECK (
        (delivered_at_ms IS NULL AND delivered_run_id IS NULL) OR
        (delivered_at_ms IS NOT NULL AND delivered_run_id IS NOT NULL)
    )
) STRICT;

INSERT INTO agent_messages_new (
    id, project_id, sender_agent_id, recipient_agent_id, body,
    created_at_ms, delivered_at_ms, delivered_run_id
)
SELECT
    id, project_id, sender_agent_id, recipient_agent_id, body,
    created_at_ms,
    CASE WHEN delivered_run_id IS NOT NULL THEN delivered_at_ms END,
    delivered_run_id
FROM agent_messages;

DROP TABLE agent_messages;
ALTER TABLE agent_messages_new RENAME TO agent_messages;
CREATE INDEX agent_messages_recipient_delivery
    ON agent_messages(project_id, recipient_agent_id, delivered_at_ms, created_at_ms, id);

DROP TABLE session_work;
DROP TABLE delivery_attempts;
DROP TABLE sessions;
