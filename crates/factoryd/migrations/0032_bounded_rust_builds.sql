-- Add completion verification without inheriting any live schema-31 authority.
-- Rust preflight refuses this migration while a run is nonterminal.

ALTER TABLE projects ADD COLUMN incarnation_id TEXT NOT NULL
    DEFAULT '00000000000000000000000000000000'
    CHECK (
        length(incarnation_id) = 32
        AND incarnation_id NOT GLOB '*[^0-9a-f]*'
    );
UPDATE projects SET incarnation_id = lower(hex(randomblob(16)));
CREATE UNIQUE INDEX projects_by_incarnation ON projects(incarnation_id);
CREATE UNIQUE INDEX projects_by_id_incarnation ON projects(id, incarnation_id);

ALTER TABLE projects ADD COLUMN completion_verification TEXT NOT NULL DEFAULT 'none'
    CHECK (completion_verification IN ('none', 'rust_workspace_test'));

DROP INDEX runs_one_open_per_agent;
DROP INDEX runs_one_open_per_task;
DROP INDEX runs_by_phase_admitted;
DROP INDEX runs_recoverable;
DROP INDEX runs_one_open_per_change;
DROP INDEX resources_by_run_state;
DROP INDEX resources_reconcile;

PRAGMA legacy_alter_table = ON;
ALTER TABLE resources RENAME TO resources_0031;
ALTER TABLE runs RENAME TO runs_0031;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    task_incarnation_id TEXT NOT NULL,
    admitted_task_work_revision INTEGER NOT NULL CHECK (admitted_task_work_revision >= 0),
    change_id TEXT,
    parent_run_id TEXT,
    source_root TEXT NOT NULL CHECK (
        length(CAST(source_root AS BLOB)) BETWEEN 1 AND 4096
        AND substr(source_root, 1, 1) = '/'
    ),
    phase TEXT NOT NULL CHECK (
        phase IN ('admitted', 'running', 'finalizing', 'terminal')
    ),
    requested_outcome TEXT CHECK (
        requested_outcome IS NULL OR
        requested_outcome IN ('succeeded', 'blocked', 'failed', 'cancelled')
    ),
    requested_outcome_detail TEXT CHECK (
        requested_outcome_detail IS NULL OR
        length(CAST(requested_outcome_detail AS BLOB)) <= 4096
    ),
    requested_outcome_result TEXT CHECK (
        requested_outcome_result IS NULL OR
        length(CAST(requested_outcome_result AS BLOB)) <= 131072
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
        (phase IN ('admitted', 'running')
            AND requested_outcome IS NULL AND outcome IS NULL
            AND finalizing_at_ms IS NULL AND ended_at_ms IS NULL
            AND capability_digest IS NOT NULL) OR
        (phase = 'finalizing'
            AND requested_outcome IS NOT NULL AND outcome IS NULL
            AND finalizing_at_ms IS NOT NULL AND ended_at_ms IS NULL
            AND capability_digest IS NULL) OR
        (phase = 'terminal'
            AND requested_outcome IS NOT NULL AND outcome IS NOT NULL
            AND finalizing_at_ms IS NOT NULL AND ended_at_ms IS NOT NULL
            AND capability_digest IS NULL)
    ),
    CHECK (phase <> 'running' OR running_at_ms IS NOT NULL),
    CHECK (
        phase = 'terminal' OR
        (runner_instance_id IS NOT NULL AND runner_runtime IS NOT NULL
         AND runner_protocol_version IS NOT NULL)
    ),
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (change_id, project_id, task_id)
        REFERENCES changes(id, project_id, task_id),
    FOREIGN KEY (parent_run_id, project_id) REFERENCES runs(id, project_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

INSERT INTO runs (
    id, project_id, agent_id, task_id, task_incarnation_id,
    admitted_task_work_revision, change_id, parent_run_id, source_root,
    phase, requested_outcome, requested_outcome_detail, requested_outcome_result,
    outcome, outcome_detail, outcome_result, capability_digest, provider,
    runtime_model, runtime_reasoning_effort, runtime_permission_mode,
    runtime_control_mode, activity, wait_reason, observer_health, observer_reason,
    runner_instance_id, runner_runtime, runner_protocol_version,
    last_runner_sequence, terminal_runner_sequence, runner_reconciled_at_ms,
    stop_requested_at_ms, admitted_at_ms, running_at_ms, finalizing_at_ms,
    phase_since_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal
)
SELECT
    id, project_id, agent_id, task_id, task_incarnation_id,
    admitted_task_work_revision, change_id, parent_run_id, source_root,
    phase, outcome, outcome_detail, outcome_result,
    outcome, outcome_detail, outcome_result, NULL, provider,
    runtime_model, runtime_reasoning_effort, runtime_permission_mode,
    runtime_control_mode, activity, wait_reason, observer_health, observer_reason,
    runner_instance_id, runner_runtime, runner_protocol_version,
    last_runner_sequence, terminal_runner_sequence, runner_reconciled_at_ms,
    stop_requested_at_ms, admitted_at_ms, running_at_ms, finalizing_at_ms,
    phase_since_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal
FROM runs_0031;

CREATE TABLE resources (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    kind TEXT NOT NULL CHECK (
        kind IN ('runner_process', 'provider_process', 'process_group',
                 'runtime_root', 'effect_process', 'effect_group', 'temporary_root')
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

INSERT INTO resources SELECT * FROM resources_0031;
DROP TABLE resources_0031;
DROP TABLE runs_0031;
PRAGMA legacy_alter_table = OFF;

CREATE UNIQUE INDEX runs_one_open_per_agent
    ON runs(agent_id) WHERE phase <> 'terminal';
CREATE UNIQUE INDEX runs_one_open_per_task
    ON runs(task_id) WHERE phase <> 'terminal';
CREATE INDEX runs_by_phase_admitted ON runs(phase, admitted_at_ms, id);
CREATE INDEX runs_recoverable
    ON runs(project_id, admitted_at_ms, id) WHERE phase <> 'terminal';
CREATE UNIQUE INDEX runs_one_open_per_change
    ON runs(change_id) WHERE change_id IS NOT NULL AND phase <> 'terminal';
CREATE INDEX resources_by_run_state ON resources(run_id, state, kind, id);
CREATE INDEX resources_reconcile ON resources(state, updated_at_ms, id)
    WHERE state <> 'released';

CREATE TABLE rust_completion_checks (
    run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id),
    project_incarnation_id TEXT NOT NULL,
    change_id TEXT NOT NULL REFERENCES changes(id),
    phase TEXT NOT NULL CHECK (
        phase IN ('pending', 'running', 'passed', 'failed')
    ),
    cache_key TEXT CHECK (
        cache_key IS NULL OR
        (length(cache_key) = 64 AND cache_key NOT GLOB '*[^0-9a-f]*')
    ),
    source_digest TEXT CHECK (
        source_digest IS NULL OR
        (length(source_digest) = 64 AND source_digest NOT GLOB '*[^0-9a-f]*')
    ),
    bundle_digest TEXT CHECK (
        bundle_digest IS NULL OR
        (length(bundle_digest) = 64 AND bundle_digest NOT GLOB '*[^0-9a-f]*')
    ),
    failure TEXT CHECK (
        failure IS NULL OR length(CAST(failure AS BLOB)) BETWEEN 1 AND 4096
    ),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    requested_at_ms INTEGER NOT NULL CHECK (requested_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= requested_at_ms),
    terminal_at_ms INTEGER CHECK (terminal_at_ms IS NULL OR terminal_at_ms >= requested_at_ms),
    FOREIGN KEY (project_id, project_incarnation_id)
        REFERENCES projects(id, incarnation_id),
    CHECK ((phase IN ('passed', 'failed')) = (terminal_at_ms IS NOT NULL)),
    CHECK ((phase = 'failed') = (failure IS NOT NULL)),
    CHECK (phase IN ('pending', 'failed') OR cache_key IS NOT NULL),
    CHECK (phase <> 'passed' OR (source_digest IS NOT NULL AND bundle_digest IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX rust_checks_one_writer
    ON rust_completion_checks(project_incarnation_id, cache_key)
    WHERE phase = 'running';
CREATE INDEX rust_checks_recoverable
    ON rust_completion_checks(phase, updated_at_ms, run_id)
    WHERE phase NOT IN ('passed', 'failed');

CREATE TABLE rust_build_caches (
    project_incarnation_id TEXT NOT NULL,
    cache_key TEXT NOT NULL CHECK (
        length(cache_key) = 64 AND cache_key NOT GLOB '*[^0-9a-f]*'
    ),
    project_id TEXT NOT NULL REFERENCES projects(id),
    path TEXT NOT NULL UNIQUE CHECK (
        length(CAST(path AS BLOB)) BETWEEN 1 AND 4096 AND substr(path, 1, 1) = '/'
    ),
    dev INTEGER CHECK (dev IS NULL OR dev >= 0),
    inode INTEGER CHECK (inode IS NULL OR inode > 0),
    bytes INTEGER CHECK (bytes IS NULL OR bytes >= 0),
    lifecycle TEXT NOT NULL CHECK (
        lifecycle IN ('declared', 'available', 'reclaiming', 'removed', 'unresolved')
    ),
    failure TEXT CHECK (
        failure IS NULL OR length(CAST(failure AS BLOB)) BETWEEN 1 AND 4096
    ),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    last_used_at_ms INTEGER NOT NULL CHECK (last_used_at_ms >= created_at_ms),
    PRIMARY KEY (project_incarnation_id, cache_key),
    FOREIGN KEY (project_id, project_incarnation_id)
        REFERENCES projects(id, incarnation_id),
    CHECK (lifecycle <> 'available' OR (dev IS NOT NULL AND inode IS NOT NULL)),
    CHECK ((lifecycle = 'unresolved') = (failure IS NOT NULL))
) STRICT;

CREATE TABLE rust_storage_policy (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    max_cache_count INTEGER NOT NULL CHECK (max_cache_count > 0),
    max_cache_bytes INTEGER NOT NULL CHECK (max_cache_bytes > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;

INSERT INTO rust_storage_policy (
    singleton, max_cache_count, max_cache_bytes, updated_at_ms
) VALUES (1, 8, 68719476736, 0);
