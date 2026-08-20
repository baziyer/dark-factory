ALTER TABLE sessions
ADD COLUMN principal_version INTEGER NOT NULL DEFAULT 0
CHECK (principal_version IN (0, 1));
