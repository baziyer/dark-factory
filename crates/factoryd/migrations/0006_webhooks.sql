ALTER TABLE tasks
    ADD COLUMN started_at_ms INTEGER;

ALTER TABLE tasks
    ADD COLUMN completed_at_ms INTEGER;

ALTER TABLE tasks
    ADD COLUMN result TEXT
    CHECK (
        result IS NULL OR
        length(CAST(result AS BLOB)) <= 131072
    );

CREATE TABLE task_dependencies (
    task_id TEXT NOT NULL,
    depends_on_task_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (task_id, depends_on_task_id),
    UNIQUE (task_id, ordinal),
    CHECK (task_id <> depends_on_task_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id),
    FOREIGN KEY (depends_on_task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE INDEX task_dependencies_by_dependency
    ON task_dependencies(project_id, depends_on_task_id, task_id);

CREATE TABLE task_questions (
    id INTEGER PRIMARY KEY,
    task_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    text TEXT NOT NULL CHECK (length(text) BETWEEN 1 AND 100000),
    asked_at_ms INTEGER,
    answer TEXT CHECK (answer IS NULL OR length(answer) BETWEEN 1 AND 100000),
    answered_at_ms INTEGER,
    UNIQUE (task_id, ordinal),
    UNIQUE (id, project_id),
    CHECK ((answer IS NULL) = (answered_at_ms IS NULL)),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE INDEX task_questions_open_latest
    ON task_questions(project_id, task_id, asked_at_ms DESC, ordinal DESC)
    WHERE answer IS NULL;

CREATE TABLE task_documents (
    project_id TEXT NOT NULL REFERENCES projects(id),
    id TEXT NOT NULL CHECK (length(id) BETWEEN 1 AND 128),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 512),
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 4096),
    revision TEXT NOT NULL CHECK (length(revision) BETWEEN 1 AND 128),
    content TEXT NOT NULL CHECK (length(CAST(content AS BLOB)) <= 131072),
    PRIMARY KEY (project_id, id)
) STRICT;

CREATE TRIGGER task_documents_immutable_update
BEFORE UPDATE ON task_documents
BEGIN
    SELECT RAISE(ABORT, 'task document snapshots are immutable');
END;

CREATE TRIGGER task_documents_immutable_delete
BEFORE DELETE ON task_documents
BEGIN
    SELECT RAISE(ABORT, 'task document snapshots are immutable');
END;

CREATE TABLE task_question_documents (
    question_id INTEGER NOT NULL,
    project_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (question_id, document_id),
    UNIQUE (question_id, ordinal),
    FOREIGN KEY (question_id, project_id) REFERENCES task_questions(id, project_id),
    FOREIGN KEY (project_id, document_id) REFERENCES task_documents(project_id, id)
) STRICT;

CREATE TABLE webhook_task_capabilities (
    task_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    endpoint_id TEXT NOT NULL CHECK (
        length(endpoint_id) BETWEEN 1 AND 64 AND
        substr(endpoint_id, 1, 1) GLOB '[a-z0-9]' AND
        endpoint_id NOT GLOB '*[^a-z0-9_-]*'
    ),
    token_sha256 BLOB NOT NULL UNIQUE CHECK (length(token_sha256) = 32),
    PRIMARY KEY (task_id),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE INDEX webhook_capabilities_by_endpoint
    ON webhook_task_capabilities(endpoint_id, project_id, token_sha256);
