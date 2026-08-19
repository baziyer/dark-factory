ALTER TABLE sessions ADD COLUMN observer_reason TEXT
    CHECK (observer_reason IS NULL OR length(CAST(observer_reason AS BLOB)) <= 512);
