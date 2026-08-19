CREATE TABLE delivery_attempts (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    task_id TEXT,
    task_incarnation_id TEXT,
    prior_run_count INTEGER,
    message_ids_json TEXT NOT NULL,
    text TEXT NOT NULL CHECK (length(CAST(text AS BLOB)) BETWEEN 1 AND 65536),
    failure_count INTEGER NOT NULL DEFAULT 0 CHECK (failure_count >= 0),
    next_attempt_at_ms INTEGER,
    state TEXT NOT NULL CHECK (
        state IN ('in_flight', 'retryable', 'terminal', 'acknowledged', 'cancelled')
    ),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    FOREIGN KEY (agent_id, project_id) REFERENCES agents(id, project_id)
) STRICT;

CREATE UNIQUE INDEX delivery_attempts_one_active_per_agent
    ON delivery_attempts(project_id, agent_id)
    WHERE state IN ('in_flight', 'retryable', 'terminal');

CREATE INDEX delivery_attempts_by_session
    ON delivery_attempts(session_id, state);
