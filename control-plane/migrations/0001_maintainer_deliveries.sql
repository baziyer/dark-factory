CREATE TABLE public.maintainer_deliveries (
    delivery_id TEXT NOT NULL,
    hook_id BIGINT NOT NULL,
    target_id BIGINT NOT NULL,
    target_type TEXT NOT NULL,
    event TEXT NOT NULL,
    action TEXT,
    body_digest TEXT NOT NULL,
    disposition TEXT NOT NULL,
    secret_revision TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT maintainer_deliveries_pkey PRIMARY KEY (delivery_id),
    CONSTRAINT maintainer_deliveries_delivery_id_format CHECK (
        delivery_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    ),
    CONSTRAINT maintainer_deliveries_hook_id_positive CHECK (hook_id > 0),
    CONSTRAINT maintainer_deliveries_target_id_positive CHECK (target_id > 0),
    CONSTRAINT maintainer_deliveries_target_type_length CHECK (
        octet_length(target_type) BETWEEN 1 AND 64
    ),
    CONSTRAINT maintainer_deliveries_target_type_format CHECK (
        target_type ~ '^[a-zA-Z0-9_-]+$'
    ),
    CONSTRAINT maintainer_deliveries_event_length CHECK (
        octet_length(event) BETWEEN 1 AND 64
    ),
    CONSTRAINT maintainer_deliveries_event_format CHECK (event ~ '^[a-z_]+$'),
    CONSTRAINT maintainer_deliveries_action_format CHECK (
        action IS NULL OR (
            octet_length(action) BETWEEN 1 AND 64
            AND action ~ '^[a-z_]+$'
        )
    ),
    CONSTRAINT maintainer_deliveries_body_digest_format CHECK (
        body_digest ~ '^[0-9a-f]{64}$'
    ),
    CONSTRAINT maintainer_deliveries_disposition_values CHECK (
        disposition IN ('ping', 'policy_rejected', 'payload_rejected')
    ),
    CONSTRAINT maintainer_deliveries_secret_revision_length CHECK (
        octet_length(secret_revision) BETWEEN 1 AND 64
    ),
    CONSTRAINT maintainer_deliveries_secret_revision_format CHECK (
        secret_revision ~ '^[a-z0-9_-]+$'
    )
);
