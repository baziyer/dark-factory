-- Split the old `auto_mode` switch into one admission policy and one typed,
-- per-agent provider authority. Existing profiles preserve the effective
-- launch posture they would receive at migration time; newly created Codex
-- and Claude profiles use the safer workspace-write default.

ALTER TABLE factory_settings RENAME COLUMN auto_mode TO dispatch_enabled;

ALTER TABLE agent_profiles ADD COLUMN execution_mode TEXT NOT NULL
    DEFAULT 'workspace_write'
    CHECK (execution_mode IN ('plan_only', 'workspace_write', 'unrestricted'));

-- An explicit legacy provider permission wins over the old global bypass,
-- just as it did in provider launch. Only values that belonged to that exact
-- provider are recognized. A cross-provider value is corrupt authority and
-- deliberately reaches the CHECK-failing ELSE branch. Interactive/default
-- modes become the closest non-interactive, fail-closed workspace posture. A
-- NULL profile used the old global bypass and therefore maps from its current
-- durable value. Shell never had a native boundary and remains unrestricted.
UPDATE agent_profiles
SET execution_mode = CASE
    WHEN (SELECT provider FROM agents WHERE id = agent_profiles.agent_id) = 'shell'
        THEN 'unrestricted'
    WHEN (SELECT provider FROM agents WHERE id = agent_profiles.agent_id) = 'claude_code'
         AND permission_mode = 'plan' THEN 'plan_only'
    WHEN (SELECT provider FROM agents WHERE id = agent_profiles.agent_id) = 'claude_code'
         AND permission_mode IN ('default', 'acceptEdits')
        THEN 'workspace_write'
    WHEN (SELECT provider FROM agents WHERE id = agent_profiles.agent_id) = 'codex'
         AND permission_mode IN ('on-request', 'never')
        THEN 'workspace_write'
    WHEN permission_mode IS NULL
         AND (SELECT dispatch_enabled FROM factory_settings WHERE singleton = 1) = 1
        THEN 'unrestricted'
    WHEN permission_mode IS NULL THEN 'workspace_write'
    -- Schema 33 did not constrain this legacy column. A fixed invalid sentinel
    -- guarantees the new CHECK aborts even when corrupt legacy text happens
    -- to equal one of the new execution-mode values.
    ELSE 'invalid_legacy_permission_mode'
END;

ALTER TABLE agent_profiles DROP COLUMN permission_mode;

-- Runtime permission strings recorded only explicit profile overrides. NULL
-- cannot reveal whether the old mutable global bypass was active at admission,
-- so it remains NULL as honest legacy-unknown metadata. New runs always store
-- one typed value.
ALTER TABLE runs ADD COLUMN runtime_execution_mode TEXT
    CHECK (
        runtime_execution_mode IS NULL OR
        runtime_execution_mode IN ('plan_only', 'workspace_write', 'unrestricted')
    );

UPDATE runs
SET runtime_execution_mode = CASE
    WHEN provider = 'shell' THEN 'unrestricted'
    WHEN provider = 'claude_code' AND runtime_permission_mode = 'plan'
        THEN 'plan_only'
    WHEN provider = 'claude_code'
         AND runtime_permission_mode IN ('default', 'acceptEdits')
        THEN 'workspace_write'
    WHEN provider = 'codex'
         AND runtime_permission_mode IN ('on-request', 'never')
        THEN 'workspace_write'
    -- Runs are audit history, not current authority. Unknown and
    -- cross-provider legacy values remain honestly unknown.
    ELSE NULL
END;

ALTER TABLE runs DROP COLUMN runtime_permission_mode;
