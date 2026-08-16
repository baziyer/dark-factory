ALTER TABLE runs
    ADD COLUMN observer_health TEXT NOT NULL DEFAULT 'unknown'
    CHECK (observer_health IN ('unknown', 'healthy', 'degraded'));

ALTER TABLE runs
    ADD COLUMN observer_health_since_ms INTEGER NOT NULL DEFAULT 0
    CHECK (observer_health_since_ms >= 0);

UPDATE runs
SET observer_health_since_ms = MAX(updated_at_ms, 0);
