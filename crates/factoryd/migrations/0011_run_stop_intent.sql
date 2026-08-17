ALTER TABLE runs
    ADD COLUMN stop_requested_at_ms INTEGER
    CHECK (stop_requested_at_ms IS NULL OR stop_requested_at_ms >= 0);
