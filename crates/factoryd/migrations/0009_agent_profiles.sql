CREATE TABLE agent_profiles (
    agent_id TEXT PRIMARY KEY REFERENCES agents(id),
    model TEXT,
    instructions TEXT NOT NULL DEFAULT '',
    memory TEXT NOT NULL DEFAULT '',
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;

INSERT INTO agent_profiles (agent_id, updated_at_ms)
SELECT id, updated_at_ms FROM agents
WHERE NOT EXISTS (
    SELECT 1 FROM agent_profiles WHERE agent_profiles.agent_id = agents.id
);
