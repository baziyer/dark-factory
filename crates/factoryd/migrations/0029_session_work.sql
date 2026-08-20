-- One durable authority for provider-independent factory-work ownership.
-- Provider lifecycle remains in `sessions`; idle/Stop never changes this row.
ALTER TABLE tasks ADD COLUMN work_revision INTEGER NOT NULL DEFAULT 0
    CHECK (work_revision >= 0);

-- Attempts remain the effect/audit journal. These columns retain the exact
-- identity captured when the corresponding work lease was reserved;
-- journal outcome/retry fields may change, but `session_work` is the owner.
ALTER TABLE delivery_attempts ADD COLUMN task_revision INTEGER
    CHECK (task_revision IS NULL OR task_revision >= 0);
ALTER TABLE delivery_attempts ADD COLUMN run_id TEXT;

-- A scalar attempt id is not enough to prove that the referenced row is owned
-- by this exact resident session. The composite key lets SQLite enforce
-- that future non-quarantine authority rows cannot cross project/agent/session
-- identities, even if a caller supplies otherwise valid foreign ids. Run
-- identity is validated transactionally because Delivering/Uncertain reserve
-- the run id before its row exists.
CREATE UNIQUE INDEX delivery_attempts_session_work_identity
    ON delivery_attempts(id, session_id, project_id, agent_id);

CREATE TABLE session_work (
    session_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('empty', 'delivering', 'running', 'uncertain')),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    attempt_id TEXT REFERENCES delivery_attempts(id),
    task_id TEXT,
    task_incarnation_id TEXT,
    task_revision INTEGER CHECK (task_revision IS NULL OR task_revision >= 0),
    run_id TEXT,
    quarantine_reason TEXT CHECK (
        quarantine_reason IS NULL OR length(CAST(quarantine_reason AS BLOB)) BETWEEN 1 AND 4096
    ),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    FOREIGN KEY (session_id, project_id) REFERENCES sessions(id, project_id),
    FOREIGN KEY (session_id, agent_id) REFERENCES sessions(id, agent_id),
    FOREIGN KEY (attempt_id, session_id, project_id, agent_id)
        REFERENCES delivery_attempts(id, session_id, project_id, agent_id),
    CHECK (quarantine_reason IS NULL OR state = 'uncertain'),
    CHECK (
        quarantine_reason IS NOT NULL OR
        (state = 'empty' AND attempt_id IS NULL AND task_id IS NULL
            AND task_incarnation_id IS NULL AND task_revision IS NULL
            AND run_id IS NULL) OR
        (state IN ('delivering', 'uncertain') AND attempt_id IS NOT NULL
            AND ((task_id IS NULL AND task_incarnation_id IS NULL
                    AND task_revision IS NULL AND run_id IS NULL) OR
                 (task_id IS NOT NULL AND task_incarnation_id IS NOT NULL
                    AND task_revision IS NOT NULL AND run_id IS NOT NULL))) OR
        (state = 'running' AND attempt_id IS NOT NULL AND task_id IS NOT NULL
            AND task_incarnation_id IS NOT NULL AND task_revision IS NOT NULL
            AND run_id IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX session_work_attempt_owner
    ON session_work(attempt_id) WHERE attempt_id IS NOT NULL;

CREATE UNIQUE INDEX session_work_run_owner
    ON session_work(run_id) WHERE run_id IS NOT NULL;

-- Existing agent/task uniqueness already makes this true for valid databases;
-- this closes the missing session-scoped defense for all future writes.
-- `Store::open` creates the open-run defense after Rust has quarantined and
-- terminalized any legacy duplicate owners. Creating it here would abort the
-- migration before those corrupt combinations could be recorded durably.
