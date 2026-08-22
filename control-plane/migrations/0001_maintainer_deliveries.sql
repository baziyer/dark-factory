CREATE TABLE IF NOT EXISTS maintainer_deliveries (
    delivery_id TEXT PRIMARY KEY CHECK (
        length(delivery_id) = 36
        AND substr(delivery_id, 9, 1) = '-'
        AND substr(delivery_id, 14, 1) = '-'
        AND substr(delivery_id, 19, 1) = '-'
        AND substr(delivery_id, 24, 1) = '-'
        AND length(replace(delivery_id, '-', '')) = 32
        AND replace(delivery_id, '-', '') NOT GLOB '*[^0-9a-f]*'
    ),
    hook_id INTEGER NOT NULL CHECK (hook_id > 0),
    target_id INTEGER NOT NULL CHECK (target_id > 0),
    target_type TEXT NOT NULL CHECK (
        length(CAST(target_type AS BLOB)) BETWEEN 1 AND 64
        AND target_type NOT GLOB '*[^a-zA-Z0-9_-]*'
    ),
    event TEXT NOT NULL CHECK (
        length(CAST(event AS BLOB)) BETWEEN 1 AND 64
        AND event NOT GLOB '*[^a-z_]*'
    ),
    action TEXT CHECK (
        action IS NULL OR (
            length(CAST(action AS BLOB)) BETWEEN 1 AND 64
            AND action NOT GLOB '*[^a-z_]*'
        )
    ),
    body_digest TEXT NOT NULL CHECK (
        length(body_digest) = 64
        AND body_digest NOT GLOB '*[^0-9a-f]*'
    ),
    disposition TEXT NOT NULL CHECK (
        disposition IN ('ping', 'policy_rejected', 'payload_rejected')
    ),
    secret_revision TEXT NOT NULL CHECK (
        length(CAST(secret_revision AS BLOB)) BETWEEN 1 AND 64
        AND secret_revision NOT GLOB '*[^a-z0-9_-]*'
    ),
    received_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (received_at >= 0)
) STRICT;
