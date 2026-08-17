CREATE TABLE agent_messages (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    sender_agent_id TEXT,
    recipient_agent_id TEXT NOT NULL,
    body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) BETWEEN 1 AND 65536),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    delivered_at_ms INTEGER CHECK (
        delivered_at_ms IS NULL OR delivered_at_ms >= created_at_ms
    ),
    delivered_run_id TEXT,
    UNIQUE (id, project_id),
    FOREIGN KEY (sender_agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (recipient_agent_id, project_id) REFERENCES agents(id, project_id),
    FOREIGN KEY (delivered_run_id, project_id) REFERENCES runs(id, project_id)
) STRICT;

CREATE INDEX agent_messages_by_recipient
    ON agent_messages(project_id, recipient_agent_id, created_at_ms, id);
