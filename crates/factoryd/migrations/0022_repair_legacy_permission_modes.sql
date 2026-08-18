-- Issue #146 review finding: profile writes were validated only after older
-- databases could already contain arbitrary provider permission values.
-- Clear only values outside each provider's declared capability set; NULL
-- truthfully restores that provider's native/default posture and lets the
-- operator choose a valid override again.
UPDATE agent_profiles
SET permission_mode = NULL
WHERE agent_id IN (SELECT id FROM agents WHERE provider = 'codex')
  AND permission_mode IS NOT NULL
  AND permission_mode NOT IN ('on-request', 'never');

UPDATE agent_profiles
SET permission_mode = NULL
WHERE agent_id IN (SELECT id FROM agents WHERE provider = 'claude_code')
  AND permission_mode IS NOT NULL
  AND permission_mode NOT IN ('default', 'acceptEdits', 'plan');

UPDATE agent_profiles
SET permission_mode = NULL
WHERE agent_id IN (SELECT id FROM agents WHERE provider = 'shell')
  AND permission_mode IS NOT NULL;
