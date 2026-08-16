CREATE TABLE subscription_usage_probes (
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex')),
    attempted_at_ms INTEGER NOT NULL CHECK (attempted_at_ms >= 0),
    outcome TEXT NOT NULL CHECK (outcome IN ('observed', 'failed')),
    used_percent INTEGER CHECK (used_percent BETWEEN 0 AND 100),
    limit_window TEXT CHECK (
        limit_window IN ('primary', 'secondary', 'current_session', 'current_week')
    ),
    resets_at_ms INTEGER,
    exhausted INTEGER CHECK (exhausted IN (0, 1)),
    severity TEXT NOT NULL CHECK (severity IN ('ok', 'warning', 'critical')),
    failure_category TEXT CHECK (
        failure_category IN ('timeout', 'protocol', 'process', 'output_limit', 'unavailable')
    ),
    notification_task_id TEXT REFERENCES tasks(id),
    PRIMARY KEY (provider, attempted_at_ms),
    CHECK (
        (outcome = 'observed' AND used_percent IS NOT NULL AND limit_window IS NOT NULL
            AND exhausted IS NOT NULL AND failure_category IS NULL)
        OR
        (outcome = 'failed' AND used_percent IS NULL AND limit_window IS NULL
            AND resets_at_ms IS NULL AND exhausted IS NULL
            AND failure_category IS NOT NULL)
    )
) STRICT;

CREATE TABLE subscription_usage_state (
    provider TEXT PRIMARY KEY CHECK (provider IN ('claude_code', 'codex')),
    last_attempt_at_ms INTEGER NOT NULL CHECK (last_attempt_at_ms >= 0),
    last_success_at_ms INTEGER,
    used_percent INTEGER CHECK (used_percent BETWEEN 0 AND 100),
    limit_window TEXT CHECK (
        limit_window IN ('primary', 'secondary', 'current_session', 'current_week')
    ),
    resets_at_ms INTEGER,
    exhausted INTEGER CHECK (exhausted IN (0, 1)),
    capacity_severity TEXT NOT NULL CHECK (
        capacity_severity IN ('ok', 'warning', 'critical')
    ),
    severity TEXT NOT NULL CHECK (severity IN ('ok', 'warning', 'critical')),
    consecutive_failures INTEGER NOT NULL CHECK (consecutive_failures >= 0),
    CHECK (
        (last_success_at_ms IS NULL AND used_percent IS NULL AND limit_window IS NULL
            AND resets_at_ms IS NULL AND exhausted IS NULL)
        OR
        (last_success_at_ms IS NOT NULL AND used_percent IS NOT NULL AND limit_window IS NOT NULL
            AND exhausted IS NOT NULL)
    )
) STRICT;
