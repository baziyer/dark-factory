-- Standing instructions and memory move out of the database and become
-- operator- and agent-editable files under `$DARK_FACTORY_HOME/projects`
-- (see `factory_core::paths` and `factoryd::guidance`). `migrate()` writes
-- any existing non-empty text out to those files before this file runs.
ALTER TABLE agent_profiles DROP COLUMN instructions;
ALTER TABLE agent_profiles DROP COLUMN memory;

-- Provider-scoped permission mode, validated and stored like `model`
-- (Claude: default/acceptEdits/plan; Codex: on-request/never). NULL means
-- the provider default. Not consumed by launch yet.
ALTER TABLE agent_profiles ADD COLUMN permission_mode TEXT;
