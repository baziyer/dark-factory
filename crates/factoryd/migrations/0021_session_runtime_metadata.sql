-- Issue #146: retain the provider values actually resolved for each session.
-- NULL means the provider/launch path could not establish that value.

ALTER TABLE sessions ADD COLUMN runtime_model TEXT;
ALTER TABLE sessions ADD COLUMN runtime_reasoning_effort TEXT;
ALTER TABLE sessions ADD COLUMN runtime_permission_mode TEXT;
ALTER TABLE sessions ADD COLUMN runtime_control_mode TEXT;
