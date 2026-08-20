-- One coalesced terminal-worker range per project, plus one durable cycle
-- lease. New terminal events while the lease is active remain the one
-- follow-up range in `pending_*`.
CREATE TABLE orchestrator_scheduler_state (
    project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    pending_from_sequence INTEGER,
    pending_through_sequence INTEGER,
    active_lease_id TEXT,
    active_agent_id TEXT,
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    CHECK ((pending_from_sequence IS NULL) = (pending_through_sequence IS NULL)),
    CHECK (pending_from_sequence IS NULL OR pending_from_sequence <= pending_through_sequence)
) STRICT;

CREATE TABLE orchestrator_cycle_ledger (
    lease_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    from_sequence INTEGER NOT NULL,
    through_sequence INTEGER NOT NULL,
    attempt_id TEXT UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('leased', 'completed', 'recovered')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    completed_at_ms INTEGER,
    CHECK (from_sequence <= through_sequence)
) STRICT;

ALTER TABLE delivery_attempts ADD COLUMN orchestrator_cycle_lease_id TEXT;

CREATE INDEX orchestrator_cycle_ledger_project_state
    ON orchestrator_cycle_ledger(project_id, state);
