ALTER TABLE agents
    ADD COLUMN provider_session_cwd TEXT
    CHECK (
        provider_session_cwd IS NULL OR
        (length(provider_session_cwd) BETWEEN 1 AND 4096 AND
         substr(provider_session_cwd, 1, 1) = '/')
    );

ALTER TABLE agents
    ADD COLUMN codex_home TEXT
    CHECK (
        codex_home IS NULL OR
        (provider = 'codex' AND
         length(codex_home) BETWEEN 1 AND 4096 AND
         substr(codex_home, 1, 1) = '/')
    );

-- Existing session ownership predates an agent-level cwd. Bind it only when
-- every exact-session run at the latest timestamp agrees on one valid cwd.
-- IDs are not chronology, and a project root is not session evidence, so
-- absent or ambiguous history remains unusable until explicitly repaired.
UPDATE agents
SET provider_session_cwd = (
    SELECT MIN(runs.worktree)
    FROM runs
    WHERE runs.agent_id = agents.id
      AND runs.project_id = agents.project_id
      AND runs.provider_session_id = agents.provider_session_id
      AND runs.started_at_ms = (
          SELECT MAX(latest.started_at_ms)
          FROM runs AS latest
          WHERE latest.agent_id = agents.id
            AND latest.project_id = agents.project_id
            AND latest.provider_session_id = agents.provider_session_id
      )
    GROUP BY runs.started_at_ms
    HAVING COUNT(DISTINCT runs.worktree) = 1
       AND MIN(
           length(runs.worktree) BETWEEN 1 AND 4096 AND
           substr(runs.worktree, 1, 1) = '/'
       ) = 1
)
WHERE provider_session_id IS NOT NULL;

-- SQLite cannot add a cross-column table CHECK without rebuilding the table.
-- These triggers preserve the same invariant for all future store writes.
CREATE TRIGGER agents_provider_session_context_insert
BEFORE INSERT ON agents
WHEN (NEW.provider_session_id IS NULL) <> (NEW.provider_session_cwd IS NULL)
  OR (NEW.codex_home IS NOT NULL AND
      (NEW.provider <> 'codex' OR NEW.provider_session_id IS NULL))
BEGIN
    SELECT RAISE(ABORT, 'invalid provider session context');
END;

CREATE TRIGGER agents_provider_session_context_update
BEFORE UPDATE OF provider, provider_session_id, provider_session_cwd, codex_home ON agents
WHEN (NEW.provider_session_id IS NULL) <> (NEW.provider_session_cwd IS NULL)
  OR (NEW.codex_home IS NOT NULL AND
      (NEW.provider <> 'codex' OR NEW.provider_session_id IS NULL))
BEGIN
    SELECT RAISE(ABORT, 'invalid provider session context');
END;
