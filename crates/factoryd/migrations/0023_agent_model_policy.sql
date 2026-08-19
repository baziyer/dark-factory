-- #155: retain the operator/policy explanation and Codex reasoning tier
-- alongside the configured model. Existing profiles remain unchanged and
-- NULL remains honest for profiles created before this policy existed.
ALTER TABLE agent_profiles ADD COLUMN reasoning_effort TEXT;
ALTER TABLE agent_profiles ADD COLUMN model_selection_reason TEXT;
