-- Add completion verification without changing schema-31 run authority.

ALTER TABLE projects ADD COLUMN incarnation_id TEXT NOT NULL
    DEFAULT '00000000000000000000000000000000'
    CHECK (
        length(incarnation_id) = 32
        AND incarnation_id NOT GLOB '*[^0-9a-f]*'
    );
UPDATE projects SET incarnation_id = lower(hex(randomblob(16)));
CREATE UNIQUE INDEX projects_by_incarnation ON projects(incarnation_id);
CREATE UNIQUE INDEX projects_by_id_incarnation ON projects(id, incarnation_id);

ALTER TABLE projects ADD COLUMN completion_verification TEXT NOT NULL DEFAULT 'none'
    CHECK (completion_verification IN ('none', 'rust_workspace_test'));

DROP INDEX resources_by_run_state;
DROP INDEX resources_reconcile;

PRAGMA legacy_alter_table = ON;
ALTER TABLE resources RENAME TO resources_0031;

CREATE TABLE resources (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    kind TEXT NOT NULL CHECK (
        kind IN ('runner_process', 'provider_process', 'process_group',
                 'runtime_root', 'effect_process', 'effect_group', 'temporary_root')
    ),
    state TEXT NOT NULL CHECK (
        state IN ('declared', 'active', 'releasing', 'released', 'unresolved')
    ),
    locator TEXT NOT NULL CHECK (length(CAST(locator AS BLOB)) BETWEEN 2 AND 4096),
    birth_fingerprint TEXT CHECK (
        birth_fingerprint IS NULL OR
        length(CAST(birth_fingerprint AS BLOB)) BETWEEN 1 AND 1024
    ),
    retry_count INTEGER NOT NULL DEFAULT 0 CHECK (retry_count >= 0),
    last_failure TEXT CHECK (
        last_failure IS NULL OR length(CAST(last_failure AS BLOB)) <= 4096
    ),
    declared_at_ms INTEGER NOT NULL CHECK (declared_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= declared_at_ms),
    released_at_ms INTEGER CHECK (released_at_ms IS NULL OR released_at_ms >= declared_at_ms),
    UNIQUE (run_id, kind, locator),
    CHECK ((state = 'released') = (released_at_ms IS NOT NULL))
) STRICT;

INSERT INTO resources SELECT * FROM resources_0031;
DROP TABLE resources_0031;
PRAGMA legacy_alter_table = OFF;
CREATE INDEX resources_by_run_state ON resources(run_id, state, kind, id);
CREATE INDEX resources_reconcile ON resources(state, updated_at_ms, id)
    WHERE state <> 'released';

CREATE TABLE rust_completion_checks (
    run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id),
    project_incarnation_id TEXT NOT NULL,
    change_id TEXT NOT NULL REFERENCES changes(id),
    phase TEXT NOT NULL CHECK (
        phase IN ('pending', 'running', 'passed', 'failed')
    ),
    cache_key TEXT CHECK (
        cache_key IS NULL OR
        (length(cache_key) = 64 AND cache_key NOT GLOB '*[^0-9a-f]*')
    ),
    source_digest TEXT CHECK (
        source_digest IS NULL OR
        (length(source_digest) = 64 AND source_digest NOT GLOB '*[^0-9a-f]*')
    ),
    bundle_digest TEXT CHECK (
        bundle_digest IS NULL OR
        (length(bundle_digest) = 64 AND bundle_digest NOT GLOB '*[^0-9a-f]*')
    ),
    failure TEXT CHECK (
        failure IS NULL OR length(CAST(failure AS BLOB)) BETWEEN 1 AND 4096
    ),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    requested_at_ms INTEGER NOT NULL CHECK (requested_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= requested_at_ms),
    terminal_at_ms INTEGER CHECK (terminal_at_ms IS NULL OR terminal_at_ms >= requested_at_ms),
    FOREIGN KEY (project_id, project_incarnation_id)
        REFERENCES projects(id, incarnation_id),
    CHECK ((phase IN ('passed', 'failed')) = (terminal_at_ms IS NOT NULL)),
    CHECK ((phase = 'failed') = (failure IS NOT NULL)),
    CHECK (phase IN ('pending', 'failed') OR cache_key IS NOT NULL),
    CHECK (phase <> 'passed' OR (source_digest IS NOT NULL AND bundle_digest IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX rust_checks_one_writer
    ON rust_completion_checks(project_incarnation_id, cache_key)
    WHERE phase = 'running';
CREATE INDEX rust_checks_recoverable
    ON rust_completion_checks(phase, updated_at_ms, run_id)
    WHERE phase NOT IN ('passed', 'failed');

CREATE TABLE rust_build_caches (
    project_incarnation_id TEXT NOT NULL,
    cache_key TEXT NOT NULL CHECK (
        length(cache_key) = 64 AND cache_key NOT GLOB '*[^0-9a-f]*'
    ),
    project_id TEXT NOT NULL REFERENCES projects(id),
    path TEXT NOT NULL UNIQUE CHECK (
        length(CAST(path AS BLOB)) BETWEEN 1 AND 4096 AND substr(path, 1, 1) = '/'
    ),
    dev INTEGER CHECK (dev IS NULL OR dev >= 0),
    inode INTEGER CHECK (inode IS NULL OR inode > 0),
    bytes INTEGER CHECK (bytes IS NULL OR bytes >= 0),
    lifecycle TEXT NOT NULL CHECK (
        lifecycle IN ('declared', 'available', 'reclaiming')
    ),
    failure TEXT CHECK (
        failure IS NULL OR length(CAST(failure AS BLOB)) BETWEEN 1 AND 4096
    ),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    last_used_at_ms INTEGER NOT NULL CHECK (last_used_at_ms >= created_at_ms),
    PRIMARY KEY (project_incarnation_id, cache_key),
    FOREIGN KEY (project_id, project_incarnation_id)
        REFERENCES projects(id, incarnation_id),
    CHECK (lifecycle <> 'available' OR (dev IS NOT NULL AND inode IS NOT NULL))
) STRICT;
