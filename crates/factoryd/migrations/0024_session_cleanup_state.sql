ALTER TABLE sessions
    ADD COLUMN cleanup_state TEXT NOT NULL DEFAULT 'none'
    CHECK (cleanup_state IN ('none', 'failed', 'verified'));
