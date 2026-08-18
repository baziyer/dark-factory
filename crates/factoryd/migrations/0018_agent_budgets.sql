CREATE TABLE agent_budgets (
    agent_id TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    max_tool_calls INTEGER CHECK (max_tool_calls IS NULL OR max_tool_calls > 0),
    tool_calls INTEGER NOT NULL DEFAULT 0 CHECK (tool_calls >= 0),
    exhausted INTEGER NOT NULL DEFAULT 0 CHECK (exhausted IN (0, 1)),
    reset_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

INSERT INTO agent_budgets (agent_id, max_tool_calls, reset_at_ms, updated_at_ms)
SELECT id, 1000, created_at_ms, created_at_ms FROM agents;
