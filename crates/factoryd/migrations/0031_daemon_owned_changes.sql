-- Replace Stage 1's quarantine-only Change placeholder with daemon-owned
-- source checkouts. Rust preflight rejects any schema-30 run linked to the
-- placeholder table; no source path is inspected or adopted by this migration.

DROP TABLE project_repository_authority;

-- Runs retain a private cwd projection, but Stage 2 removes the linked Git
-- worktree concept from the live kernel vocabulary.
ALTER TABLE runs RENAME COLUMN worktree TO source_root;

CREATE TABLE legacy_sources (
    id TEXT PRIMARY KEY CHECK (length(CAST(id AS BLOB)) BETWEEN 1 AND 128),
    project_id TEXT NOT NULL REFERENCES projects(id),
    former_agent_id TEXT,
    source_path TEXT NOT NULL CHECK (
        length(CAST(source_path AS BLOB)) BETWEEN 1 AND 4096
        AND substr(source_path, 1, 1) = '/'
    ),
    retained_reason TEXT NOT NULL CHECK (
        length(CAST(retained_reason AS BLOB)) BETWEEN 1 AND 4096
    ),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms >= 0)
) STRICT;

INSERT INTO legacy_sources (
    id, project_id, former_agent_id, source_path, retained_reason, recorded_at_ms
)
SELECT
    'legacy-' || printf('%016x', rowid),
    project_id,
    CASE
        WHEN id LIKE 'legacy-agent-%' THEN substr(id, length('legacy-agent-') + 1)
        ELSE NULL
    END,
    worktree,
    COALESCE(retained_reason, 'retained schema-30 source path'),
    updated_at_ms
FROM changes;

DROP TABLE changes;

-- The live runtime no longer projects or writes a per-agent source path. The
-- schema-30 Change rows above already preserved the only migration evidence.
ALTER TABLE agents DROP COLUMN worktree;

CREATE TABLE changes (
    id TEXT PRIMARY KEY CHECK (length(CAST(id AS BLOB)) BETWEEN 1 AND 128),
    project_id TEXT NOT NULL REFERENCES projects(id),
    task_id TEXT NOT NULL,
    task_incarnation_id TEXT NOT NULL CHECK (
        length(CAST(task_incarnation_id AS BLOB)) BETWEEN 1 AND 255
    ),
    phase TEXT NOT NULL CHECK (
        phase IN ('provisioning', 'available', 'removing', 'removed')
    ),
    source_root TEXT NOT NULL UNIQUE CHECK (
        length(CAST(source_root AS BLOB)) BETWEEN 1 AND 4096
        AND substr(source_root, 1, 1) = '/'
    ),
    base_oid TEXT CHECK (
        base_oid IS NULL OR (
            length(base_oid) IN (40, 64)
            AND base_oid NOT GLOB '*[^0-9a-f]*'
        )
    ),
    base_repository_root TEXT CHECK (
        base_repository_root IS NULL OR (
            length(CAST(base_repository_root AS BLOB)) BETWEEN 1 AND 4096
            AND substr(base_repository_root, 1, 1) = '/'
        )
    ),
    base_repository_dev INTEGER CHECK (
        base_repository_dev IS NULL OR base_repository_dev >= 0
    ),
    base_repository_inode INTEGER CHECK (
        base_repository_inode IS NULL OR base_repository_inode > 0
    ),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    source_dev INTEGER CHECK (source_dev IS NULL OR source_dev >= 0),
    source_inode INTEGER CHECK (source_inode IS NULL OR source_inode > 0),
    size_bytes INTEGER CHECK (size_bytes IS NULL OR size_bytes >= 0),
    measured_at_ms INTEGER CHECK (
        measured_at_ms IS NULL OR measured_at_ms >= created_at_ms
    ),
    last_failure TEXT CHECK (
        last_failure IS NULL OR
        length(CAST(last_failure AS BLOB)) BETWEEN 1 AND 4096
    ),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    available_at_ms INTEGER CHECK (
        available_at_ms IS NULL OR available_at_ms >= created_at_ms
    ),
    removing_at_ms INTEGER CHECK (
        removing_at_ms IS NULL OR removing_at_ms >= created_at_ms
    ),
    removed_at_ms INTEGER CHECK (
        removed_at_ms IS NULL OR removed_at_ms >= created_at_ms
    ),
    UNIQUE (id, project_id),
    UNIQUE (id, project_id, task_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id),
    CHECK (
        (phase = 'provisioning'
            AND ((base_oid IS NULL AND base_repository_root IS NULL
                    AND base_repository_dev IS NULL AND base_repository_inode IS NULL) OR
                 (base_oid IS NOT NULL AND base_repository_root IS NOT NULL
                    AND base_repository_dev IS NOT NULL AND base_repository_inode IS NOT NULL))
            AND source_dev IS NULL AND source_inode IS NULL
            AND ((size_bytes IS NULL AND measured_at_ms IS NULL) OR
                 (size_bytes IS NOT NULL AND measured_at_ms IS NOT NULL))
            AND available_at_ms IS NULL AND removing_at_ms IS NULL
            AND removed_at_ms IS NULL) OR
        (phase = 'available'
            AND base_oid IS NOT NULL AND base_repository_root IS NOT NULL
            AND base_repository_dev IS NOT NULL AND base_repository_inode IS NOT NULL
            AND source_dev IS NOT NULL AND source_inode IS NOT NULL
            AND ((size_bytes IS NULL AND measured_at_ms IS NULL) OR
                 (size_bytes IS NOT NULL AND measured_at_ms IS NOT NULL))
            AND available_at_ms IS NOT NULL AND removing_at_ms IS NULL
            AND removed_at_ms IS NULL AND last_failure IS NULL) OR
        (phase = 'removing'
            AND removing_at_ms IS NOT NULL AND removed_at_ms IS NULL
            AND (((base_oid IS NULL AND base_repository_root IS NULL
                        AND base_repository_dev IS NULL AND base_repository_inode IS NULL) OR
                    (base_oid IS NOT NULL AND base_repository_root IS NOT NULL
                        AND base_repository_dev IS NOT NULL
                        AND base_repository_inode IS NOT NULL))
                AND source_dev IS NULL AND source_inode IS NULL
                AND ((size_bytes IS NULL AND measured_at_ms IS NULL) OR
                     (size_bytes IS NOT NULL AND measured_at_ms IS NOT NULL))
                AND available_at_ms IS NULL OR
                (base_oid IS NOT NULL AND base_repository_root IS NOT NULL
                    AND base_repository_dev IS NOT NULL
                    AND base_repository_inode IS NOT NULL
                    AND source_dev IS NOT NULL AND source_inode IS NOT NULL
                    AND ((size_bytes IS NULL AND measured_at_ms IS NULL) OR
                         (size_bytes IS NOT NULL AND measured_at_ms IS NOT NULL))
                    AND available_at_ms IS NOT NULL))) OR
        (phase = 'removed'
            AND removing_at_ms IS NOT NULL AND removed_at_ms IS NOT NULL
            AND last_failure IS NULL
            AND (((base_oid IS NULL AND base_repository_root IS NULL
                        AND base_repository_dev IS NULL AND base_repository_inode IS NULL) OR
                    (base_oid IS NOT NULL AND base_repository_root IS NOT NULL
                        AND base_repository_dev IS NOT NULL
                        AND base_repository_inode IS NOT NULL))
                AND source_dev IS NULL AND source_inode IS NULL
                AND ((size_bytes IS NULL AND measured_at_ms IS NULL) OR
                     (size_bytes IS NOT NULL AND measured_at_ms IS NOT NULL))
                AND available_at_ms IS NULL OR
                (base_oid IS NOT NULL AND base_repository_root IS NOT NULL
                    AND base_repository_dev IS NOT NULL
                    AND base_repository_inode IS NOT NULL
                    AND source_dev IS NOT NULL AND source_inode IS NOT NULL
                    AND ((size_bytes IS NULL AND measured_at_ms IS NULL) OR
                         (size_bytes IS NOT NULL AND measured_at_ms IS NOT NULL))
                    AND available_at_ms IS NOT NULL)))
    )
) STRICT;

CREATE UNIQUE INDEX changes_one_per_task_incarnation
    ON changes(project_id, task_id, task_incarnation_id);

CREATE INDEX changes_recoverable
    ON changes(phase, updated_at_ms, id)
    WHERE phase IN ('provisioning', 'removing')
       OR (phase = 'available' AND size_bytes IS NULL);

CREATE UNIQUE INDEX runs_one_open_per_change
    ON runs(change_id) WHERE change_id IS NOT NULL AND phase <> 'terminal';
