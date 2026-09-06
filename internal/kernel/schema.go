package kernel

import (
	"context"
	"database/sql"
	"fmt"
	"strings"
)

const (
	applicationID = 0x4446474f
	userVersion   = 3

	// SQLite reserves the exact lower-case "sqlite_" prefix. Use a literal,
	// binary prefix test: LIKE would treat '_' as a wildcard and hide names
	// such as sqliteXforeign from exact schema validation.
	internalSchemaNamePredicate = "substr(name, 1, 7) = 'sqlite_' COLLATE BINARY"
)

var schemaStatements = []string{
	// This slice deliberately creates only the kernel authority tables.
	// Final-v1 agent-message, verification-effect, removal, and checkpoint state
	// is absent until its owning slice can add it before the incompatible v1 ships.
	`CREATE TABLE factory (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    daemon_id BLOB NOT NULL CHECK (length(daemon_id) = 16 AND daemon_id <> zeroblob(16)),
    dispatch_enabled INTEGER NOT NULL CHECK (dispatch_enabled IN (0, 1)),
    capacity INTEGER NOT NULL CHECK (capacity BETWEEN 1 AND 1024),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    next_invalidation_sequence INTEGER NOT NULL CHECK (next_invalidation_sequence >= 1),
    invalidation_floor INTEGER NOT NULL CHECK (invalidation_floor >= 1 AND invalidation_floor <= next_invalidation_sequence),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT, WITHOUT ROWID`,
	`CREATE TABLE projects (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
	    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 128),
	    root TEXT NOT NULL CHECK (length(CAST(root AS BLOB)) BETWEEN 1 AND 4096 AND substr(root, 1, 1) = '/'),
	    verification_policy TEXT NOT NULL CHECK (verification_policy IN ('none', 'rust_workspace_test', 'go_workspace_test')),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX projects_root_unique ON projects(root)`,
	`CREATE TABLE accounts (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex')),
    home TEXT NOT NULL CHECK (length(CAST(home AS BLOB)) BETWEEN 1 AND 1024 AND substr(home, 1, 1) = '/'),
    label TEXT NOT NULL CHECK (length(CAST(label AS BLOB)) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX accounts_provider_home_unique ON accounts(provider, home)`,
	`CREATE TABLE agents (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    project_id BLOB NOT NULL CHECK (length(project_id) = 16) REFERENCES projects(id),
    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 128),
    role TEXT NOT NULL CHECK (role IN ('orchestrator', 'worker')),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'shell')),
    model TEXT CHECK (model IS NULL OR length(CAST(model AS BLOB)) BETWEEN 1 AND 128),
    reasoning_effort TEXT CHECK (reasoning_effort IS NULL OR reasoning_effort IN ('low', 'medium', 'high', 'xhigh', 'max', 'ultra')),
    account_id BLOB CHECK (account_id IS NULL OR length(account_id) = 16) REFERENCES accounts(id),
    paused INTEGER NOT NULL CHECK (paused IN (0, 1)),
    tool_budget_limit INTEGER NOT NULL CHECK (tool_budget_limit BETWEEN 1 AND 1000000000),
    tool_calls_used INTEGER NOT NULL CHECK (tool_calls_used >= 0 AND tool_calls_used <= tool_budget_limit),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    CHECK (provider <> 'shell' OR (model IS NULL AND reasoning_effort IS NULL AND account_id IS NULL))
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX agents_id_project_unique ON agents(id, project_id)`,
	`CREATE TABLE tasks (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    project_id BLOB NOT NULL CHECK (length(project_id) = 16),
    assigned_agent_id BLOB NOT NULL CHECK (length(assigned_agent_id) = 16),
    incarnation_id BLOB NOT NULL CHECK (length(incarnation_id) = 16),
    work_revision INTEGER NOT NULL CHECK (work_revision >= 1),
    title TEXT NOT NULL CHECK (length(CAST(title AS BLOB)) BETWEEN 1 AND 1024),
    body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) <= 131072),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'blocked', 'succeeded', 'failed', 'cancelled')),
    priority INTEGER NOT NULL CHECK (priority BETWEEN -1000000 AND 1000000),
    blocked_reason TEXT CHECK (blocked_reason IS NULL OR length(CAST(blocked_reason AS BLOB)) BETWEEN 1 AND 4096),
    result TEXT CHECK (result IS NULL OR length(CAST(result AS BLOB)) <= 131072),
    completed_at_ms INTEGER CHECK (completed_at_ms IS NULL OR completed_at_ms >= 0),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    FOREIGN KEY (assigned_agent_id, project_id) REFERENCES agents(id, project_id),
    CHECK (
        (status IN ('queued', 'running') AND blocked_reason IS NULL AND result IS NULL AND completed_at_ms IS NULL) OR
        (status = 'blocked' AND blocked_reason IS NOT NULL AND result IS NULL AND completed_at_ms IS NULL) OR
        (status = 'succeeded' AND blocked_reason IS NULL AND completed_at_ms IS NOT NULL) OR
        (status IN ('failed', 'cancelled') AND blocked_reason IS NULL AND result IS NULL AND completed_at_ms IS NOT NULL)
    ),
    CHECK (completed_at_ms IS NULL OR completed_at_ms = updated_at_ms)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX tasks_id_project_incarnation_unique ON tasks(id, project_id, incarnation_id)`,
	`CREATE UNIQUE INDEX tasks_incarnation_unique ON tasks(incarnation_id)`,
	`CREATE INDEX tasks_canonical_queue ON tasks(status, priority DESC, created_at_ms ASC, id ASC)`,
	`CREATE TABLE changes (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    project_id BLOB NOT NULL CHECK (length(project_id) = 16),
    task_id BLOB NOT NULL CHECK (length(task_id) = 16),
    task_incarnation_id BLOB NOT NULL CHECK (length(task_incarnation_id) = 16),
	phase TEXT NOT NULL CHECK (phase IN ('reserved', 'prepared', 'available', 'retained', 'abandoned')),
    object_format TEXT CHECK (object_format IS NULL OR object_format IN ('sha1', 'sha256')),
	base_commit BLOB,
	repository_dev INTEGER CHECK (repository_dev IS NULL OR repository_dev >= 0),
	repository_inode INTEGER CHECK (repository_inode IS NULL OR repository_inode > 0),
    prepared_at_ms INTEGER CHECK (prepared_at_ms IS NULL OR prepared_at_ms >= 0),
    tree_digest BLOB CHECK (tree_digest IS NULL OR length(tree_digest) = 32),
    entry_count INTEGER CHECK (entry_count IS NULL OR entry_count BETWEEN 0 AND 10000),
    total_bytes INTEGER CHECK (total_bytes IS NULL OR total_bytes BETWEEN 0 AND 1073741824),
	tree_dev INTEGER CHECK (tree_dev IS NULL OR tree_dev >= 0),
	tree_inode INTEGER CHECK (tree_inode IS NULL OR tree_inode > 0),
    available_at_ms INTEGER CHECK (available_at_ms IS NULL OR available_at_ms >= 0),
	settled_run_id BLOB CHECK (settled_run_id IS NULL OR length(settled_run_id) = 16),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    FOREIGN KEY (task_id, project_id, task_incarnation_id) REFERENCES tasks(id, project_id, incarnation_id),
	FOREIGN KEY (settled_run_id, id, project_id, task_id, task_incarnation_id) REFERENCES runs(id, change_id, project_id, task_id, task_incarnation_id),
	CHECK ((object_format IS NULL AND base_commit IS NULL AND repository_dev IS NULL AND repository_inode IS NULL) OR (object_format = 'sha1' AND length(base_commit) = 20 AND repository_dev IS NOT NULL AND repository_inode IS NOT NULL) OR (object_format = 'sha256' AND length(base_commit) = 32 AND repository_dev IS NOT NULL AND repository_inode IS NOT NULL)),
	CHECK (
		(phase = 'reserved' AND object_format IS NULL AND base_commit IS NULL AND repository_dev IS NULL AND repository_inode IS NULL AND prepared_at_ms IS NULL AND tree_digest IS NULL AND entry_count IS NULL AND total_bytes IS NULL AND tree_dev IS NULL AND tree_inode IS NULL AND available_at_ms IS NULL AND settled_run_id IS NULL) OR
		(phase = 'prepared' AND object_format IS NOT NULL AND base_commit IS NOT NULL AND repository_dev IS NOT NULL AND repository_inode IS NOT NULL AND prepared_at_ms IS NOT NULL AND tree_digest IS NOT NULL AND entry_count IS NOT NULL AND total_bytes IS NOT NULL AND tree_dev IS NOT NULL AND tree_inode IS NOT NULL AND available_at_ms IS NULL AND settled_run_id IS NULL) OR
		(phase = 'available' AND object_format IS NOT NULL AND base_commit IS NOT NULL AND repository_dev IS NOT NULL AND repository_inode IS NOT NULL AND prepared_at_ms IS NOT NULL AND tree_digest IS NOT NULL AND entry_count IS NOT NULL AND total_bytes IS NOT NULL AND tree_dev IS NOT NULL AND tree_inode IS NOT NULL AND available_at_ms IS NOT NULL AND settled_run_id IS NULL) OR
		(phase = 'retained' AND object_format IS NOT NULL AND base_commit IS NOT NULL AND repository_dev IS NOT NULL AND repository_inode IS NOT NULL AND prepared_at_ms IS NOT NULL AND tree_digest IS NOT NULL AND entry_count IS NOT NULL AND total_bytes IS NOT NULL AND tree_dev IS NOT NULL AND tree_inode IS NOT NULL AND available_at_ms IS NOT NULL AND settled_run_id IS NOT NULL) OR
		(phase = 'abandoned' AND object_format IS NULL AND base_commit IS NULL AND repository_dev IS NULL AND repository_inode IS NULL AND prepared_at_ms IS NULL AND tree_digest IS NULL AND entry_count IS NULL AND total_bytes IS NULL AND tree_dev IS NULL AND tree_inode IS NULL AND available_at_ms IS NULL AND settled_run_id IS NOT NULL)
	)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX changes_id_project_task_incarnation_unique ON changes(id, project_id, task_id, task_incarnation_id)`,
	`CREATE UNIQUE INDEX changes_task_incarnation_unique ON changes(project_id, task_id, task_incarnation_id)`,
	`CREATE TABLE runs (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    project_id BLOB NOT NULL CHECK (length(project_id) = 16),
    agent_id BLOB NOT NULL CHECK (length(agent_id) = 16),
    task_id BLOB NOT NULL CHECK (length(task_id) = 16),
    task_incarnation_id BLOB NOT NULL CHECK (length(task_incarnation_id) = 16),
    admitted_task_work_revision INTEGER NOT NULL CHECK (admitted_task_work_revision >= 1),
    change_id BLOB CHECK (change_id IS NULL OR length(change_id) = 16),
	admitted_change_revision INTEGER CHECK (admitted_change_revision IS NULL OR admitted_change_revision >= 1),
    role TEXT NOT NULL CHECK (role IN ('orchestrator', 'worker')),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'shell')),
    model TEXT CHECK (model IS NULL OR length(CAST(model AS BLOB)) BETWEEN 1 AND 128),
	    reasoning_effort TEXT CHECK (reasoning_effort IS NULL OR reasoning_effort IN ('low', 'medium', 'high', 'xhigh', 'max', 'ultra')),
	    verification_policy TEXT NOT NULL CHECK (verification_policy IN ('none', 'rust_workspace_test', 'go_workspace_test')),
	    phase TEXT NOT NULL CHECK (phase IN ('admitted', 'running', 'finalizing', 'terminal')),
    proposal_kind TEXT CHECK (proposal_kind IS NULL OR proposal_kind IN ('succeeded', 'blocked', 'failed', 'cancelled')),
    proposal_code TEXT CHECK (proposal_code IS NULL OR proposal_code IN ('spawn', 'activation', 'source', 'provider_exit', 'runner_exit', 'protocol', 'internal', 'attempt')),
    proposal_detail TEXT CHECK (proposal_detail IS NULL OR length(CAST(proposal_detail AS BLOB)) BETWEEN 1 AND 4096),
    proposal_result TEXT CHECK (proposal_result IS NULL OR length(CAST(proposal_result AS BLOB)) <= 131072),
    terminal_kind TEXT CHECK (terminal_kind IS NULL OR terminal_kind IN ('succeeded', 'blocked', 'failed', 'cancelled')),
	    terminal_code TEXT CHECK (terminal_code IS NULL OR terminal_code IN ('spawn', 'activation', 'source', 'provider_exit', 'runner_exit', 'protocol', 'internal', 'attempt')),
    terminal_detail TEXT CHECK (terminal_detail IS NULL OR length(CAST(terminal_detail AS BLOB)) BETWEEN 1 AND 4096),
    terminal_result TEXT CHECK (terminal_result IS NULL OR length(CAST(terminal_result AS BLOB)) <= 131072),
	    credential_digest BLOB NOT NULL CHECK (length(credential_digest) = 32),
	    result_proof_digest BLOB NOT NULL CHECK (length(result_proof_digest) = 32),
	    credential_revoked_at_ms INTEGER CHECK (credential_revoked_at_ms IS NULL OR credential_revoked_at_ms >= 0),
	    provider_exit_kind TEXT CHECK (provider_exit_kind IS NULL OR provider_exit_kind IN ('code', 'signal', 'recovered_absence')),
	    provider_exit_sequence INTEGER CHECK (provider_exit_sequence IS NULL OR provider_exit_sequence >= 1),
    provider_exit_code INTEGER CHECK (provider_exit_code IS NULL OR provider_exit_code >= 0),
    provider_exit_signal INTEGER CHECK (provider_exit_signal IS NULL OR provider_exit_signal > 0),
    provider_exit_at_ms INTEGER CHECK (provider_exit_at_ms IS NULL OR provider_exit_at_ms >= 0),
	    runner_exit_kind TEXT CHECK (runner_exit_kind IS NULL OR runner_exit_kind IN ('code', 'signal', 'recovered_absence')),
	    runner_exit_sequence INTEGER CHECK (runner_exit_sequence IS NULL OR runner_exit_sequence >= 1),
    runner_exit_code INTEGER CHECK (runner_exit_code IS NULL OR runner_exit_code >= 0),
    runner_exit_signal INTEGER CHECK (runner_exit_signal IS NULL OR runner_exit_signal > 0),
    runner_exit_at_ms INTEGER CHECK (runner_exit_at_ms IS NULL OR runner_exit_at_ms >= 0),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    admitted_at_ms INTEGER NOT NULL CHECK (admitted_at_ms >= 0),
    running_at_ms INTEGER CHECK (running_at_ms IS NULL OR running_at_ms >= admitted_at_ms),
    finalizing_at_ms INTEGER CHECK (finalizing_at_ms IS NULL OR finalizing_at_ms >= admitted_at_ms),
    terminal_at_ms INTEGER CHECK (terminal_at_ms IS NULL OR terminal_at_ms >= admitted_at_ms),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= admitted_at_ms),
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (task_id, project_id, task_incarnation_id) REFERENCES tasks(id, project_id, incarnation_id),
    FOREIGN KEY (change_id, project_id, task_id, task_incarnation_id) REFERENCES changes(id, project_id, task_id, task_incarnation_id),
	    CHECK ((role = 'worker' AND change_id IS NOT NULL AND admitted_change_revision IS NOT NULL) OR (role = 'orchestrator' AND change_id IS NULL AND admitted_change_revision IS NULL)),
    CHECK (provider <> 'shell' OR (model IS NULL AND reasoning_effort IS NULL)),
    CHECK ((proposal_kind IS NULL AND proposal_code IS NULL AND proposal_detail IS NULL AND proposal_result IS NULL) OR (proposal_kind = 'succeeded' AND proposal_code IS NULL AND proposal_detail IS NULL AND proposal_result IS NOT NULL) OR (proposal_kind = 'blocked' AND proposal_code IS NULL AND proposal_detail IS NOT NULL AND proposal_result IS NULL) OR (proposal_kind = 'failed' AND proposal_code IS NOT NULL AND proposal_result IS NULL) OR (proposal_kind = 'cancelled' AND proposal_code IS NULL AND proposal_detail IS NOT NULL AND proposal_result IS NULL)),
    CHECK ((terminal_kind IS NULL AND terminal_code IS NULL AND terminal_detail IS NULL AND terminal_result IS NULL) OR (terminal_kind = 'succeeded' AND terminal_code IS NULL AND terminal_detail IS NULL AND terminal_result IS NOT NULL) OR (terminal_kind = 'blocked' AND terminal_code IS NULL AND terminal_detail IS NOT NULL AND terminal_result IS NULL) OR (terminal_kind = 'failed' AND terminal_code IS NOT NULL AND terminal_result IS NULL) OR (terminal_kind = 'cancelled' AND terminal_code IS NULL AND terminal_detail IS NOT NULL AND terminal_result IS NULL)),
	    CHECK ((phase = 'admitted' AND running_at_ms IS NULL AND finalizing_at_ms IS NULL AND terminal_at_ms IS NULL AND proposal_kind IS NULL AND terminal_kind IS NULL AND credential_revoked_at_ms IS NULL AND provider_exit_kind IS NULL AND runner_exit_kind IS NULL) OR (phase = 'running' AND running_at_ms IS NOT NULL AND running_at_ms = updated_at_ms AND finalizing_at_ms IS NULL AND terminal_at_ms IS NULL AND proposal_kind IS NULL AND terminal_kind IS NULL AND credential_revoked_at_ms IS NULL AND provider_exit_kind IS NULL AND runner_exit_kind IS NULL) OR (phase = 'finalizing' AND finalizing_at_ms IS NOT NULL AND terminal_at_ms IS NULL AND proposal_kind IS NOT NULL AND terminal_kind IS NULL AND credential_revoked_at_ms IS NOT NULL) OR (phase = 'terminal' AND finalizing_at_ms IS NOT NULL AND terminal_at_ms IS NOT NULL AND proposal_kind IS NOT NULL AND terminal_kind IS NOT NULL AND credential_revoked_at_ms IS NOT NULL)),
	    CHECK (running_at_ms IS NULL OR running_at_ms <= updated_at_ms),
	    CHECK (finalizing_at_ms IS NULL OR finalizing_at_ms <= updated_at_ms),
	    CHECK (terminal_at_ms IS NULL OR terminal_at_ms = updated_at_ms),
	    CHECK ((running_at_ms IS NULL OR finalizing_at_ms IS NULL OR running_at_ms <= finalizing_at_ms) AND (finalizing_at_ms IS NULL OR terminal_at_ms IS NULL OR finalizing_at_ms <= terminal_at_ms)),
	    CHECK (credential_revoked_at_ms IS NULL OR credential_revoked_at_ms = finalizing_at_ms),
	    CHECK (provider_exit_at_ms IS NULL OR provider_exit_at_ms <= updated_at_ms),
	    CHECK (runner_exit_at_ms IS NULL OR runner_exit_at_ms <= updated_at_ms),
	    CHECK (phase <> 'terminal' OR (terminal_kind = proposal_kind AND terminal_code IS proposal_code AND terminal_detail IS proposal_detail AND terminal_result IS proposal_result)),
	    CHECK (phase <> 'terminal' OR role <> 'worker' OR verification_policy = 'none' OR proposal_kind <> 'succeeded'),
	    CHECK ((provider_exit_kind IS NULL AND provider_exit_sequence IS NULL AND provider_exit_code IS NULL AND provider_exit_signal IS NULL AND provider_exit_at_ms IS NULL) OR (provider_exit_kind IS 'code' AND provider_exit_sequence IS NOT NULL AND provider_exit_code IS NOT NULL AND provider_exit_signal IS NULL AND provider_exit_at_ms IS NOT NULL) OR (provider_exit_kind IS 'signal' AND provider_exit_sequence IS NOT NULL AND provider_exit_code IS NULL AND provider_exit_signal IS NOT NULL AND provider_exit_at_ms IS NOT NULL) OR (provider_exit_kind IS 'recovered_absence' AND provider_exit_sequence IS NOT NULL AND provider_exit_code IS NULL AND provider_exit_signal IS NULL AND provider_exit_at_ms IS NOT NULL)),
	    CHECK ((runner_exit_kind IS NULL AND runner_exit_sequence IS NULL AND runner_exit_code IS NULL AND runner_exit_signal IS NULL AND runner_exit_at_ms IS NULL) OR (runner_exit_kind IS 'code' AND runner_exit_sequence IS NOT NULL AND runner_exit_code IS NOT NULL AND runner_exit_signal IS NULL AND runner_exit_at_ms IS NOT NULL) OR (runner_exit_kind IS 'signal' AND runner_exit_sequence IS NOT NULL AND runner_exit_code IS NULL AND runner_exit_signal IS NOT NULL AND runner_exit_at_ms IS NOT NULL) OR (runner_exit_kind IS 'recovered_absence' AND runner_exit_sequence IS NOT NULL AND runner_exit_code IS NULL AND runner_exit_signal IS NULL AND runner_exit_at_ms IS NOT NULL))
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX runs_credential_digest_unique ON runs(credential_digest)`,
	`CREATE UNIQUE INDEX runs_result_proof_digest_unique ON runs(result_proof_digest)`,
	`CREATE UNIQUE INDEX runs_one_open_per_agent ON runs(agent_id) WHERE phase <> 'terminal'`,
	`CREATE UNIQUE INDEX runs_one_open_per_task_incarnation ON runs(task_id, task_incarnation_id) WHERE phase <> 'terminal'`,
	`CREATE UNIQUE INDEX runs_one_open_per_change ON runs(change_id) WHERE change_id IS NOT NULL AND phase <> 'terminal'`,
	`CREATE UNIQUE INDEX runs_one_per_task_work_revision ON runs(task_id, task_incarnation_id, admitted_task_work_revision)`,
	`CREATE UNIQUE INDEX runs_change_settlement_target ON runs(id, change_id, project_id, task_id, task_incarnation_id)`,
	`CREATE INDEX runs_recoverable ON runs(phase, admitted_at_ms, id) WHERE phase <> 'terminal'`,
	`CREATE TABLE resources (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    run_id BLOB NOT NULL CHECK (length(run_id) = 16) REFERENCES runs(id),
    kind TEXT NOT NULL CHECK (kind IN ('runtime_root', 'runner_process', 'provider_process', 'provider_group')),
    state TEXT NOT NULL CHECK (state IN ('declared', 'starting', 'active', 'releasing', 'unresolved', 'released')),
    path TEXT CHECK (path IS NULL OR (length(CAST(path AS BLOB)) BETWEEN 1 AND 4096 AND substr(path, 1, 1) = '/')),
    path_dev INTEGER CHECK (path_dev IS NULL OR path_dev >= 0),
    path_inode INTEGER CHECK (path_inode IS NULL OR path_inode > 0),
    pid INTEGER CHECK (pid IS NULL OR pid > 1),
    pgid INTEGER CHECK (pgid IS NULL OR pgid > 1),
    birth_digest BLOB CHECK (birth_digest IS NULL OR length(birth_digest) = 32),
    unresolved_reason TEXT CHECK (unresolved_reason IS NULL OR length(CAST(unresolved_reason AS BLOB)) BETWEEN 1 AND 4096),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    declared_at_ms INTEGER NOT NULL CHECK (declared_at_ms >= 0),
    activated_at_ms INTEGER CHECK (activated_at_ms IS NULL OR (activated_at_ms >= declared_at_ms AND activated_at_ms >= 0)),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= declared_at_ms AND (activated_at_ms IS NULL OR updated_at_ms >= activated_at_ms)),
    released_at_ms INTEGER CHECK (released_at_ms IS NULL OR (released_at_ms >= declared_at_ms AND (activated_at_ms IS NULL OR released_at_ms >= activated_at_ms) AND released_at_ms = updated_at_ms)),
    CHECK ((kind = 'runtime_root' AND path IS NOT NULL AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL) OR (kind IN ('runner_process', 'provider_process', 'provider_group') AND path IS NULL AND path_dev IS NULL AND path_inode IS NULL)),
	    CHECK ((path_dev IS NULL AND path_inode IS NULL) OR (path_dev IS NOT NULL AND path_inode IS NOT NULL)),
	    CHECK ((pid IS NULL AND pgid IS NULL AND birth_digest IS NULL) OR (pid IS NOT NULL AND pgid IS NOT NULL AND birth_digest IS NOT NULL)),
	    CHECK (state <> 'starting' OR (kind = 'runner_process' AND path_dev IS NULL AND path_inode IS NULL AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND activated_at_ms IS NULL AND released_at_ms IS NULL AND unresolved_reason IS NULL)),
	    CHECK (state <> 'declared' OR (path_dev IS NULL AND path_inode IS NULL AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL)),
    CHECK (state <> 'active' OR activated_at_ms IS NOT NULL),
	    CHECK (state <> 'active' OR (kind = 'runtime_root' AND path_dev IS NOT NULL) OR (kind IN ('runner_process', 'provider_process', 'provider_group') AND pid IS NOT NULL)),
	    CHECK (state <> 'declared' OR updated_at_ms = declared_at_ms),
	    CHECK (state <> 'active' OR updated_at_ms = activated_at_ms),
    CHECK ((state = 'released') = (released_at_ms IS NOT NULL)),
    CHECK (state <> 'unresolved' OR unresolved_reason IS NOT NULL)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX resources_run_kind_unique ON resources(run_id, kind)`,
	`CREATE INDEX resources_recoverable ON resources(state, updated_at_ms, id) WHERE state <> 'released'`,
	`CREATE TABLE terminal_sessions (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    run_id BLOB NOT NULL CHECK (length(run_id) = 16) REFERENCES runs(id),
    state TEXT NOT NULL CHECK (state IN ('declared', 'active', 'releasing', 'closed', 'unresolved')),
    unresolved_reason TEXT CHECK (unresolved_reason IS NULL OR length(CAST(unresolved_reason AS BLOB)) BETWEEN 1 AND 4096),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    declared_at_ms INTEGER NOT NULL CHECK (declared_at_ms >= 0),
    activated_at_ms INTEGER CHECK (activated_at_ms IS NULL OR (activated_at_ms >= declared_at_ms AND activated_at_ms >= 0)),
    closed_at_ms INTEGER CHECK (closed_at_ms IS NULL OR (closed_at_ms >= declared_at_ms AND (activated_at_ms IS NULL OR closed_at_ms >= activated_at_ms) AND closed_at_ms = updated_at_ms)),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= declared_at_ms AND (activated_at_ms IS NULL OR updated_at_ms >= activated_at_ms)),
    lease_client_id BLOB CHECK (lease_client_id IS NULL OR (length(lease_client_id) = 16 AND lease_client_id <> zeroblob(16))) REFERENCES browser_clients(id),
    lease_generation INTEGER NOT NULL DEFAULT 0 CHECK (lease_generation >= 0),
    lease_expires_at_ms INTEGER CHECK (lease_expires_at_ms IS NULL OR lease_expires_at_ms >= 0),
    last_input_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_input_sequence >= 0),
    CHECK ((state = 'unresolved') = (unresolved_reason IS NOT NULL)),
    CHECK (state <> 'declared' OR (activated_at_ms IS NULL AND closed_at_ms IS NULL AND updated_at_ms = declared_at_ms)),
    CHECK (state <> 'active' OR (activated_at_ms IS NOT NULL AND closed_at_ms IS NULL AND unresolved_reason IS NULL)),
	CHECK (state <> 'releasing' OR (closed_at_ms IS NULL AND unresolved_reason IS NULL)),
    CHECK (state <> 'unresolved' OR closed_at_ms IS NULL),
    CHECK (state <> 'closed' OR (closed_at_ms IS NOT NULL AND unresolved_reason IS NULL))
    ,CHECK ((lease_client_id IS NULL) = (lease_expires_at_ms IS NULL))
    ,CHECK (lease_client_id IS NULL OR state = 'active')
    ,CHECK (lease_client_id IS NULL OR lease_generation >= 1)
    ,CHECK (lease_client_id IS NOT NULL OR last_input_sequence = 0)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX terminal_sessions_run_unique ON terminal_sessions(run_id)`,
	`CREATE TABLE browser_pairing_challenges (
    secret_digest BLOB PRIMARY KEY CHECK (length(secret_digest) = 32),
    boot_id BLOB NOT NULL CHECK (length(boot_id) = 16 AND boot_id <> zeroblob(16)),
    intended_origin TEXT NOT NULL CHECK (length(CAST(intended_origin AS BLOB)) BETWEEN 1 AND 4096),
    capability_mask INTEGER NOT NULL CHECK (capability_mask BETWEEN 1 AND 31 AND (capability_mask & 1) = 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > created_at_ms AND expires_at_ms <= created_at_ms + 300000),
    redeemed_at_ms INTEGER CHECK (redeemed_at_ms IS NULL OR (redeemed_at_ms >= created_at_ms AND redeemed_at_ms < expires_at_ms AND redeemed_at_ms >= 0))
) STRICT, WITHOUT ROWID`,
	`CREATE TABLE browser_clients (
    id BLOB PRIMARY KEY CHECK (length(id) = 16 AND id <> zeroblob(16)),
    public_key BLOB NOT NULL CHECK (length(public_key) = 65 AND substr(public_key, 1, 1) = X'04'),
    fingerprint BLOB NOT NULL UNIQUE CHECK (length(fingerprint) = 32),
    capability_mask INTEGER NOT NULL CHECK (capability_mask BETWEEN 1 AND 31 AND (capability_mask & 1) = 1),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    revoked_at_ms INTEGER CHECK (revoked_at_ms IS NULL OR (revoked_at_ms >= created_at_ms AND revoked_at_ms <= updated_at_ms AND revoked_at_ms >= 0))
) STRICT, WITHOUT ROWID`,
	`CREATE TABLE browser_security_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence >= 1),
    kind TEXT NOT NULL CHECK (kind IN ('challenge_minted', 'challenge_abandoned', 'client_paired', 'duplicate_fingerprint', 'client_revoked')),
    client_id BLOB CHECK (client_id IS NULL OR (length(client_id) = 16 AND client_id <> zeroblob(16))) REFERENCES browser_clients(id),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms >= 0),
    CHECK ((kind IN ('challenge_minted', 'challenge_abandoned') AND client_id IS NULL) OR (kind NOT IN ('challenge_minted', 'challenge_abandoned') AND client_id IS NOT NULL))
) STRICT`,
	`CREATE INDEX browser_security_events_client ON browser_security_events(client_id, sequence)`,
	`CREATE TABLE human_requests (
    id BLOB PRIMARY KEY CHECK (length(id) = 16 AND id <> zeroblob(16)),
    run_id BLOB NOT NULL CHECK (length(run_id) = 16) REFERENCES runs(id),
    idempotency_key BLOB NOT NULL CHECK (length(idempotency_key) = 16 AND idempotency_key <> zeroblob(16)),
    kind TEXT NOT NULL CHECK (kind = 'question'),
    reason_code TEXT NOT NULL CHECK (reason_code = 'provider_question'),
    question_text TEXT NOT NULL CHECK (length(CAST(question_text AS BLOB)) BETWEEN 1 AND 8192),
    status TEXT NOT NULL CHECK (status IN ('open', 'delivering', 'delivery_unknown', 'resolved', 'stale')),
    delivery_id BLOB CHECK (delivery_id IS NULL OR (length(delivery_id) = 16 AND delivery_id <> zeroblob(16))),
    delivery_started_at_ms INTEGER CHECK (delivery_started_at_ms IS NULL OR delivery_started_at_ms >= 0),
    resolution_kind TEXT CHECK (resolution_kind IS NULL OR resolution_kind IN ('reply', 'stale', 'cancel_run')),
    closed_at_ms INTEGER CHECK (closed_at_ms IS NULL OR closed_at_ms >= 0),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    UNIQUE(run_id, idempotency_key),
    UNIQUE(delivery_id),
    CHECK ((delivery_id IS NULL) = (delivery_started_at_ms IS NULL)),
    CHECK (delivery_started_at_ms IS NULL OR (delivery_started_at_ms >= created_at_ms AND delivery_started_at_ms <= updated_at_ms)),
    CHECK (closed_at_ms IS NULL OR (closed_at_ms >= created_at_ms AND closed_at_ms <= updated_at_ms)),
    CHECK (status = 'open' AND delivery_id IS NULL OR status IN ('delivering', 'delivery_unknown') AND delivery_id IS NOT NULL OR status = 'resolved' AND (resolution_kind = 'reply' AND delivery_id IS NOT NULL OR resolution_kind = 'cancel_run' AND delivery_id IS NULL) OR status = 'stale'),
    CHECK ((status IN ('resolved', 'stale')) = (closed_at_ms IS NOT NULL)),
    CHECK (status = 'resolved' AND resolution_kind IN ('reply', 'cancel_run') OR status = 'stale' AND resolution_kind = 'stale' OR status IN ('open', 'delivering', 'delivery_unknown') AND resolution_kind IS NULL),
    CHECK (status IN ('open', 'delivering', 'delivery_unknown') AND closed_at_ms IS NULL OR status IN ('resolved', 'stale')),
    CHECK (status NOT IN ('resolved', 'stale') OR closed_at_ms = updated_at_ms)
) STRICT, WITHOUT ROWID`,
	`CREATE UNIQUE INDEX human_requests_one_unresolved_per_run ON human_requests(run_id) WHERE status IN ('open', 'delivering', 'delivery_unknown')`,
	`CREATE TABLE invalidations (
    sequence INTEGER PRIMARY KEY CHECK (sequence >= 1),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms >= 0),
    entity_kind TEXT NOT NULL CHECK (entity_kind IN ('factory', 'project', 'agent', 'task', 'change', 'run', 'human_request', 'account')),
    entity_id BLOB NOT NULL CHECK (length(entity_id) = 16),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1))
) STRICT`,
	`CREATE UNIQUE INDEX invalidations_entity_revision_unique ON invalidations(entity_kind, entity_id, revision)`,
}

type schemaObject struct {
	kind string
	name string
	sql  string
}

func expectedSchemaOf(statements []string) map[string]schemaObject {
	result := make(map[string]schemaObject, len(statements))
	for _, statement := range statements {
		kind, name := schemaObjectIdentity(statement)
		result[name] = schemaObject{kind: kind, name: name, sql: strings.TrimSpace(statement)}
	}
	return result
}

func schemaObjectIdentity(statement string) (string, string) {
	fields := strings.Fields(statement)
	kind, nameAt := "table", 2
	if fields[1] == "UNIQUE" {
		kind, nameAt = "index", 3
	} else if fields[1] == "INDEX" {
		kind = "index"
	}
	return kind, fields[nameAt]
}

func inspectIdentity(ctx context.Context, connection *sql.Conn) (int, int, error) {
	var appID, version int
	if err := connection.QueryRowContext(ctx, "PRAGMA application_id").Scan(&appID); err != nil {
		return 0, 0, fmt.Errorf("read sqlite application id: %w", err)
	}
	if err := connection.QueryRowContext(ctx, "PRAGMA user_version").Scan(&version); err != nil {
		return 0, 0, fmt.Errorf("read sqlite user version: %w", err)
	}
	return appID, version, nil
}

func validateExactSchema(ctx context.Context, connection *sql.Conn) error {
	return validateSchemaVersion(ctx, connection, userVersion, schemaStatements)
}

func validateSchemaVersion(ctx context.Context, connection *sql.Conn, wantVersion int, statements []string) error {
	appID, version, err := inspectIdentity(ctx, connection)
	if err != nil {
		return err
	}
	if appID != applicationID || version != wantVersion {
		return fmt.Errorf("%w: application_id=%#x user_version=%d", ErrForeignDatabase, appID, version)
	}

	expected := expectedSchemaOf(statements)
	rows, err := connection.QueryContext(ctx, `SELECT type, name, sql FROM sqlite_schema WHERE NOT (`+internalSchemaNamePredicate+`) ORDER BY type, name`)
	if err != nil {
		return fmt.Errorf("read sqlite schema: %w", err)
	}
	defer rows.Close()
	seen := make(map[string]bool, len(expected))
	for rows.Next() {
		var kind, name, definition string
		if err := rows.Scan(&kind, &name, &definition); err != nil {
			return fmt.Errorf("scan sqlite schema: %w", err)
		}
		want, ok := expected[name]
		if !ok || want.kind != kind || want.sql != definition {
			return fmt.Errorf("%w: unexpected or changed schema object %s %s", ErrForeignDatabase, kind, name)
		}
		seen[name] = true
	}
	if err := rows.Err(); err != nil {
		return fmt.Errorf("iterate sqlite schema: %w", err)
	}
	if len(seen) != len(expected) {
		return fmt.Errorf("%w: schema has %d of %d required objects", ErrForeignDatabase, len(seen), len(expected))
	}

	var violations int
	foreignKeys, err := connection.QueryContext(ctx, "PRAGMA foreign_key_check")
	if err != nil {
		return fmt.Errorf("check foreign keys: %w", err)
	}
	for foreignKeys.Next() {
		violations++
	}
	if err := foreignKeys.Close(); err != nil {
		return fmt.Errorf("close foreign key check: %w", err)
	}
	if violations != 0 {
		return fmt.Errorf("%w: %d foreign key violations", ErrCorruptState, violations)
	}
	return nil
}

func schemaIsEmpty(ctx context.Context, connection *sql.Conn) (bool, error) {
	appID, version, err := inspectIdentity(ctx, connection)
	if err != nil {
		return false, err
	}
	var objects int
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM sqlite_schema WHERE NOT (`+internalSchemaNamePredicate+`)`).Scan(&objects); err != nil {
		return false, fmt.Errorf("count sqlite schema objects: %w", err)
	}
	return appID == 0 && version == 0 && objects == 0, nil
}

func validateIntegrity(ctx context.Context, connection *sql.Conn) error {
	rows, err := connection.QueryContext(ctx, "PRAGMA integrity_check")
	if err != nil {
		return fmt.Errorf("check sqlite integrity: %w", err)
	}
	defer rows.Close()
	count := 0
	for rows.Next() {
		var result string
		if err := rows.Scan(&result); err != nil {
			return fmt.Errorf("scan sqlite integrity result: %w", err)
		}
		count++
		if result != "ok" {
			return fmt.Errorf("%w: sqlite integrity check: %s", ErrCorruptState, result)
		}
	}
	if err := rows.Err(); err != nil {
		return fmt.Errorf("iterate sqlite integrity results: %w", err)
	}
	if count != 1 {
		return fmt.Errorf("%w: sqlite integrity check returned %d results", ErrCorruptState, count)
	}
	return nil
}
