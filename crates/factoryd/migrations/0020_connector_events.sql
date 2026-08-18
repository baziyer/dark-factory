CREATE TABLE connector_events (
    endpoint_id TEXT NOT NULL,
    event_id TEXT NOT NULL CHECK (length(event_id) BETWEEN 1 AND 128),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('task', 'message')),
    result_id TEXT NOT NULL,
    received_at_ms INTEGER NOT NULL,
    PRIMARY KEY (endpoint_id, event_id)
) STRICT;
