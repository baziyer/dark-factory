ALTER TABLE sessions
    ADD COLUMN provider_resume_blocked_at_ms INTEGER
        CHECK (provider_resume_blocked_at_ms IS NULL OR provider_resume_blocked_at_ms >= 0);

ALTER TABLE sessions
    ADD COLUMN resumed_provider_session INTEGER NOT NULL DEFAULT 0
        CHECK (resumed_provider_session IN (0, 1));

ALTER TABLE sessions
    ADD COLUMN delivery_recovery_stop_requested_at_ms INTEGER
        CHECK (
            delivery_recovery_stop_requested_at_ms IS NULL
            OR delivery_recovery_stop_requested_at_ms >= 0
        );

UPDATE sessions
SET resumed_provider_session = 1
WHERE provider_session_id IS NOT NULL
  AND EXISTS (
      SELECT 1
      FROM sessions AS prior
      WHERE prior.project_id = sessions.project_id
        AND prior.agent_id = sessions.agent_id
        AND prior.provider = sessions.provider
        AND prior.provider_session_id = sessions.provider_session_id
        AND prior.rowid < sessions.rowid
  );
