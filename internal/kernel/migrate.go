package kernel

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
)

// legacyUserVersion is the schema every home created before the accounts slice
// carries. That slice added the accounts table, agents.account_id, and the
// wider invalidations entity_kind check as schemaStatements edits with no
// migration, so every existing home became foreign to the new build. The two
// definitions below are frozen at their pre-accounts text: migrateLegacy
// recognises a v1 home by comparing them literally before it rewrites anything.
const (
	legacyUserVersion = 1

	legacyAgents = `CREATE TABLE agents (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    project_id BLOB NOT NULL CHECK (length(project_id) = 16) REFERENCES projects(id),
    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 128),
    role TEXT NOT NULL CHECK (role IN ('orchestrator', 'worker')),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'shell')),
    model TEXT CHECK (model IS NULL OR length(CAST(model AS BLOB)) BETWEEN 1 AND 128),
    reasoning_effort TEXT CHECK (reasoning_effort IS NULL OR reasoning_effort IN ('low', 'medium', 'high', 'xhigh', 'max', 'ultra')),
    paused INTEGER NOT NULL CHECK (paused IN (0, 1)),
    tool_budget_limit INTEGER NOT NULL CHECK (tool_budget_limit BETWEEN 1 AND 1000000000),
    tool_calls_used INTEGER NOT NULL CHECK (tool_calls_used >= 0 AND tool_calls_used <= tool_budget_limit),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    CHECK (provider <> 'shell' OR (model IS NULL AND reasoning_effort IS NULL))
) STRICT, WITHOUT ROWID`

	legacyInvalidations = `CREATE TABLE invalidations (
    sequence INTEGER PRIMARY KEY CHECK (sequence >= 1),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms >= 0),
    entity_kind TEXT NOT NULL CHECK (entity_kind IN ('factory', 'project', 'agent', 'task', 'change', 'run', 'human_request')),
    entity_id BLOB NOT NULL CHECK (length(entity_id) = 16),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1))
) STRICT`

	legacyAgentColumns        = `id, project_id, name, role, provider, model, reasoning_effort, paused, tool_budget_limit, tool_calls_used, revision, created_at_ms, updated_at_ms`
	legacyInvalidationColumns = `sequence, occurred_at_ms, entity_kind, entity_id, revision, deleted`
)

// legacySchemaStatements is the exact v1 schema: the current one without the
// accounts objects and with the two frozen definitions substituted. Every other
// statement is read live from schemaStatements, so editing any of them silently
// changes what this claims v1 was and stops recognising real v1 homes. The next
// schema change has to freeze the text it replaces here and extend the
// migration, in the same change; TestSchemaDigestsArePinned pins both this set
// and schemaStatements and fails until it does.
func legacySchemaStatements() []string {
	statements := make([]string, 0, len(schemaStatements))
	for _, statement := range schemaStatements {
		switch _, name := schemaObjectIdentity(statement); name {
		case "accounts", "accounts_provider_home_unique":
			continue
		case "agents":
			statement = legacyAgents
		case "invalidations":
			statement = legacyInvalidations
		}
		statements = append(statements, statement)
	}
	return statements
}

// validateOpenableSnapshot accepts either a current database or the exact v1
// shape that Open migrates. The durable-control pass reads columns only v2 has,
// so a v1 snapshot is checked here for its exact schema and integrity alone and
// validated in full once the migration has committed.
func validateOpenableSnapshot(ctx context.Context, connection *sql.Conn) error {
	if _, version, err := inspectIdentity(ctx, connection); err != nil {
		return err
	} else if version == legacyUserVersion {
		if err := validateSchemaVersion(ctx, connection, legacyUserVersion, legacySchemaStatements()); err != nil {
			return err
		}
		return validateIntegrity(ctx, connection)
	}
	return validateDatabaseSnapshot(ctx, connection)
}

// migrateLegacy upgrades an exact v1 home to the current schema in one
// transaction, or leaves the database byte-untouched and refuses. Open calls it
// before the store is published, so it is the only writer. One case is neither:
// if restoring foreign key enforcement fails after the commit, the home is
// migrated and this open is still refused, because a connection that cannot
// enforce foreign keys must not serve the daemon. The next open then finds a v2
// home and nothing to migrate. The migration is one way -- a build from before
// it refuses user_version 2 -- so the rollback plan for an operator home is the
// operator's pre-upgrade copy of factory.sqlite3, which docs/install.md tells
// them to take; nothing here makes one.
func (store *Store) migrateLegacy(ctx context.Context) error {
	connection, err := store.writerConnection(ctx)
	if err != nil {
		return err
	}
	_, version, err := inspectIdentity(ctx, connection)
	if err == nil && version != legacyUserVersion {
		return connection.Close()
	}
	if err == nil {
		err = migrateLegacyConnection(ctx, connection)
	}
	if err != nil {
		releaseUncertainConnection(connection)
		return err
	}
	return connection.Close()
}

func migrateLegacyConnection(ctx context.Context, connection *sql.Conn) error {
	// Rebuilding agents drops a table that tasks and runs reference, which
	// SQLite refuses with foreign keys enforced. PRAGMA foreign_keys is a no-op
	// inside a transaction, so enforcement is toggled around it and restored
	// before the connection can be reused.
	if err := setForeignKeys(ctx, connection, false); err != nil {
		return err
	}
	return errors.Join(migrateLegacyTransaction(ctx, connection), setForeignKeys(ctx, connection, true))
}

func migrateLegacyTransaction(ctx context.Context, connection *sql.Conn) (resultErr error) {
	if _, err := connection.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
		return fmt.Errorf("begin schema migration: %w", err)
	}
	defer func() {
		if resultErr == nil {
			return
		}
		if _, err := connection.ExecContext(context.Background(), "ROLLBACK"); err != nil {
			resultErr = errors.Join(resultErr, fmt.Errorf("roll back schema migration: %w", err))
		}
	}()
	// Refuse anything that is not exactly the schema this migration was written
	// against, foreign key violations included.
	if err := validateSchemaVersion(ctx, connection, legacyUserVersion, legacySchemaStatements()); err != nil {
		return err
	}
	target := expectedSchemaOf(schemaStatements)
	for _, name := range []string{"accounts", "accounts_provider_home_unique"} {
		if _, err := connection.ExecContext(ctx, target[name].sql); err != nil {
			return fmt.Errorf("create %s: %w", name, err)
		}
	}
	if err := rebuildTable(ctx, connection, target, "agents", legacyAgentColumns, "agents_id_project_unique"); err != nil {
		return err
	}
	if err := rebuildTable(ctx, connection, target, "invalidations", legacyInvalidationColumns, "invalidations_entity_revision_unique"); err != nil {
		return err
	}
	if _, err := connection.ExecContext(ctx, fmt.Sprintf("PRAGMA user_version = %d", userVersion)); err != nil {
		return fmt.Errorf("set sqlite user version: %w", err)
	}
	// validateExactSchema proves the rewritten text is byte-identical to
	// schemaStatements and runs foreign_key_check, all before the commit. The
	// durable-control pass belongs here too: it is the one Open cannot run on a
	// v1 snapshot, and a home it rejects must keep its original bytes.
	if err := validateExactSchema(ctx, connection); err != nil {
		return err
	}
	if err := validateDurableControls(ctx, connection); err != nil {
		return err
	}
	if _, err := connection.ExecContext(ctx, "COMMIT"); err != nil {
		return fmt.Errorf("commit schema migration: %w", err)
	}
	return nil
}

// rebuildTable replaces one table with its current definition, preserving every
// row. SQLite cannot alter the constraints of a STRICT table, and a renamed
// table keeps its old text, while validateExactSchema compares that text byte
// for byte. So the current statement is executed verbatim under the real name
// and the rows wait in a scratch table for the moment the real one is absent.
func rebuildTable(ctx context.Context, connection *sql.Conn, target map[string]schemaObject, table, columns string, indexes ...string) error {
	scratch := table + "_pre_migration"
	statements := []string{
		"CREATE TABLE " + scratch + " AS SELECT " + columns + " FROM " + table,
		"DROP TABLE " + table,
		target[table].sql,
		"INSERT INTO " + table + "(" + columns + ") SELECT " + columns + " FROM " + scratch,
		"DROP TABLE " + scratch,
	}
	for _, index := range indexes {
		statements = append(statements, target[index].sql)
	}
	for _, statement := range statements {
		if _, err := connection.ExecContext(ctx, statement); err != nil {
			return fmt.Errorf("rebuild %s: %w", table, err)
		}
	}
	return nil
}

func setForeignKeys(ctx context.Context, connection *sql.Conn, enforced bool) error {
	statement, want := "PRAGMA foreign_keys = OFF", 0
	if enforced {
		statement, want = "PRAGMA foreign_keys = ON", 1
	}
	if _, err := connection.ExecContext(ctx, statement); err != nil {
		return fmt.Errorf("set sqlite foreign key enforcement: %w", err)
	}
	var got int
	if err := connection.QueryRowContext(ctx, "PRAGMA foreign_keys").Scan(&got); err != nil {
		return fmt.Errorf("read sqlite foreign key enforcement: %w", err)
	}
	if got != want {
		return fmt.Errorf("%w: sqlite foreign key enforcement is %d, want %d", ErrCorruptState, got, want)
	}
	return nil
}
