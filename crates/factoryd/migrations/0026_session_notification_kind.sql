ALTER TABLE sessions ADD COLUMN notification_kind TEXT
    CHECK (notification_kind IS NULL OR notification_kind IN (
        'permission_prompt', 'elicitation_dialog', 'idle_prompt',
        'auth_success', 'elicitation_complete', 'elicitation_response'
    ));
