DROP INDEX runs_recoverable;

ALTER TABLE runs
    RENAME COLUMN runner_acknowledged_at_ms TO runner_reconciled_at_ms;

CREATE INDEX runs_recoverable
    ON runs(project_id, started_at_ms, id)
    WHERE ended_at_ms IS NULL
       OR (terminal_runner_sequence IS NOT NULL AND runner_reconciled_at_ms IS NULL);
