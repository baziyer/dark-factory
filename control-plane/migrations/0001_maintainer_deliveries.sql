CREATE TABLE public.maintainer_deliveries (
    delivery_id TEXT PRIMARY KEY
        CHECK (delivery_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    hook_id BIGINT NOT NULL CHECK (hook_id > 0),
    target_id BIGINT NOT NULL CHECK (target_id > 0),
    target_type TEXT NOT NULL
        CHECK (octet_length(target_type) BETWEEN 1 AND 64)
        CHECK (target_type ~ '^[a-zA-Z0-9_-]+$'),
    event TEXT NOT NULL
        CHECK (octet_length(event) BETWEEN 1 AND 64)
        CHECK (event ~ '^[a-z_]+$'),
    action TEXT
        CHECK (action IS NULL OR (
            octet_length(action) BETWEEN 1 AND 64
            AND action ~ '^[a-z_]+$'
        )),
    body_digest TEXT NOT NULL
        CHECK (body_digest ~ '^[0-9a-f]{64}$'),
    disposition TEXT NOT NULL
        CHECK (disposition IN ('ping', 'policy_rejected', 'payload_rejected')),
    secret_revision TEXT NOT NULL
        CHECK (octet_length(secret_revision) BETWEEN 1 AND 64)
        CHECK (secret_revision ~ '^[a-z0-9_-]+$'),
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
