package kernel

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
)

// Schema history. A home is recognised by comparing its schema text literally
// against the set for its user_version, so every replaced definition is frozen
// here at the text it had, and each set derives from the next: v1 from v2, v2
// from the current statements.
//
// v1 is every home created before the accounts slice. That slice added the
// accounts table, agents.account_id, and the wider invalidations entity_kind
// check as schemaStatements edits with no migration, so every existing home
// became foreign to the new build. legacyAgents and legacyInvalidations are
// frozen at their pre-accounts text.
//
// v2 bounded the browser capability mask at four bits. v3 adds administration
// (bit 4) and widens the two CHECKs; nothing else about those tables moved.
//
// v3 had no idle rule on agents. v4 adds the five idle_* columns, every
// existing agent keeping the rule that was always true: wait.
const (
	legacyUserVersion   = 1
	previousUserVersion = 2
	priorUserVersion    = 3

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

	previousPairingChallenges = `CREATE TABLE browser_pairing_challenges (
    secret_digest BLOB PRIMARY KEY CHECK (length(secret_digest) = 32),
    boot_id BLOB NOT NULL CHECK (length(boot_id) = 16 AND boot_id <> zeroblob(16)),
    intended_origin TEXT NOT NULL CHECK (length(CAST(intended_origin AS BLOB)) BETWEEN 1 AND 4096),
    capability_mask INTEGER NOT NULL CHECK (capability_mask BETWEEN 1 AND 15 AND (capability_mask & 1) = 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > created_at_ms AND expires_at_ms <= created_at_ms + 300000),
    redeemed_at_ms INTEGER CHECK (redeemed_at_ms IS NULL OR (redeemed_at_ms >= created_at_ms AND redeemed_at_ms < expires_at_ms AND redeemed_at_ms >= 0))
) STRICT, WITHOUT ROWID`

	previousBrowserClients = `CREATE TABLE browser_clients (
    id BLOB PRIMARY KEY CHECK (length(id) = 16 AND id <> zeroblob(16)),
    public_key BLOB NOT NULL CHECK (length(public_key) = 65 AND substr(public_key, 1, 1) = X'04'),
    fingerprint BLOB NOT NULL UNIQUE CHECK (length(fingerprint) = 32),
    capability_mask INTEGER NOT NULL CHECK (capability_mask BETWEEN 1 AND 15 AND (capability_mask & 1) = 1),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    revoked_at_ms INTEGER CHECK (revoked_at_ms IS NULL OR (revoked_at_ms >= created_at_ms AND revoked_at_ms <= updated_at_ms AND revoked_at_ms >= 0))
) STRICT, WITHOUT ROWID`

	previousPairingChallengeColumns = `secret_digest, boot_id, intended_origin, capability_mask, created_at_ms, expires_at_ms, redeemed_at_ms`
	previousBrowserClientColumns    = `id, public_key, fingerprint, capability_mask, revision, created_at_ms, updated_at_ms, revoked_at_ms`

	priorAgents = `CREATE TABLE agents (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    project_id BLOB NOT NULL CHECK (length(project_id) = 16) REFERENCES projects(id),
    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 128),
    role TEXT NOT NULL CHECK (role IN ('orchestrator', 'worker')),
    provider TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'shell')),
    model TEXT CHECK (model IS NULL OR length(CAST(model AS BLOB)) BETWEEN 1 AND 128),
    reasoning_effort TEXT CHECK (reasoning_effort IS NULL OR reasoning_effort IN ('low', 'medium', 'high', 'xhigh', 'max', 'ultra')),
    account_id BLOB CHECK (account_id IS NULL OR length(account_id) = 16) REFERENCES accounts(id),
    paused INTEGER NOT NULL CHECK (paused IN (0, 1)),
    tool_budget_limit INTEGER NOT NULL CHECK (tool_budget_limit BETWEEN 1 AND 1000000000),
    tool_calls_used INTEGER NOT NULL CHECK (tool_calls_used >= 0 AND tool_calls_used <= tool_budget_limit),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    CHECK (provider <> 'shell' OR (model IS NULL AND reasoning_effort IS NULL AND account_id IS NULL))
) STRICT, WITHOUT ROWID`

	priorAgentColumns = `id, project_id, name, role, provider, model, reasoning_effort, account_id, paused, tool_budget_limit, tool_calls_used, revision, created_at_ms, updated_at_ms`
	idleColumns       = `idle_policy, idle_after_seconds, idle_instruction, idle_run_budget, idle_runs_used`
	idleDefaults      = `'wait', 0, '', 0, 0`
)

// priorSchemaStatements is the exact v3 schema: the current one with the
// frozen agents definition substituted. Every other statement is read live
// from schemaStatements, so editing any of them silently changes what this
// claims v3 was and stops recognising real v3 homes. The next schema change
// has to freeze the text it replaces here and extend the migration, in the
// same change; TestSchemaDigestsArePinned pins every set and fails until it
// does.
func priorSchemaStatements() []string {
	statements := make([]string, 0, len(schemaStatements))
	for _, statement := range schemaStatements {
		if _, name := schemaObjectIdentity(statement); name == "agents" {
			statement = priorAgents
		}
		statements = append(statements, statement)
	}
	return statements
}

// previousSchemaStatements is the exact v2 schema: v3 with the two frozen
// browser definitions substituted.
func previousSchemaStatements() []string {
	prior := priorSchemaStatements()
	statements := make([]string, 0, len(prior))
	for _, statement := range prior {
		switch _, name := schemaObjectIdentity(statement); name {
		case "browser_pairing_challenges":
			statement = previousPairingChallenges
		case "browser_clients":
			statement = previousBrowserClients
		}
		statements = append(statements, statement)
	}
	return statements
}

// legacySchemaStatements is the exact v1 schema: v2 without the accounts
// objects and with the two frozen definitions substituted.
func legacySchemaStatements() []string {
	previous := previousSchemaStatements()
	statements := make([]string, 0, len(previous))
	for _, statement := range previous {
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

// validateOpenableSnapshot accepts either a current database or an exact
// earlier shape that Open migrates. The durable-control pass reads columns only
// v2 has, so a v1 snapshot is checked here for its exact schema and integrity
// alone and validated in full once the migration has committed.
func validateOpenableSnapshot(ctx context.Context, connection *sql.Conn) error {
	if _, version, err := inspectIdentity(ctx, connection); err != nil {
		return err
	} else if statements, migrates := migratableSchema(version); migrates {
		if err := validateSchemaVersion(ctx, connection, version, statements); err != nil {
			return err
		}
		return validateIntegrity(ctx, connection)
	}
	return validateDatabaseSnapshot(ctx, connection)
}

// migratableSchema is the exact schema text of each user_version Open still
// migrates from.
func migratableSchema(version int) ([]string, bool) {
	switch version {
	case legacyUserVersion:
		return legacySchemaStatements(), true
	case previousUserVersion:
		return previousSchemaStatements(), true
	case priorUserVersion:
		return priorSchemaStatements(), true
	}
	return nil, false
}

// migrateLegacy upgrades an exact earlier home to the current schema, every
// version step it needs in one transaction, or leaves the database
// byte-untouched and refuses. Open calls it before the store is published, so
// it is the only writer. One case is neither: if restoring foreign key
// enforcement fails after the commit, the home is migrated and this open is
// still refused, because a connection that cannot enforce foreign keys must
// not serve the daemon. The next open then finds a current home and nothing
// to migrate. The migration is one way -- a build from before it refuses the
// newer user_version -- so the rollback plan for an operator home is the
// operator's pre-upgrade copy of factory.sqlite3, which docs/install.md tells
// them to take; nothing here makes one.
func (store *Store) migrateLegacy(ctx context.Context) error {
	connection, err := store.writerConnection(ctx)
	if err != nil {
		return err
	}
	_, version, err := inspectIdentity(ctx, connection)
	if err != nil {
		releaseUncertainConnection(connection)
		return err
	}
	all := []func(context.Context, *sql.Conn) error{migrateLegacyTransaction, migratePreviousTransaction, migratePriorTransaction}
	var steps []func(context.Context, *sql.Conn) error
	switch version {
	case legacyUserVersion:
		steps = all
	case previousUserVersion:
		steps = all[1:]
	case priorUserVersion:
		steps = all[2:]
	default:
		return connection.Close()
	}
	err = migrateWithoutForeignKeys(ctx, connection, func(ctx context.Context, connection *sql.Conn) error {
		for _, step := range steps {
			if err := step(ctx, connection); err != nil {
				return err
			}
		}
		// The durable-control pass belongs at the end: it reads columns only
		// the current schema has, and a home it rejects keeps every original
		// byte because nothing has been committed yet.
		return validateDurableControls(ctx, connection)
	})
	if err != nil {
		releaseUncertainConnection(connection)
		return err
	}
	return connection.Close()
}

func migrateWithoutForeignKeys(ctx context.Context, connection *sql.Conn, step func(context.Context, *sql.Conn) error) error {
	// Rebuilding a table other tables reference (agents; browser_clients) is
	// a DROP SQLite refuses with foreign keys enforced. PRAGMA foreign_keys is
	// a no-op inside a transaction, so enforcement is toggled around the step
	// and restored before the connection can be reused.
	if err := setForeignKeys(ctx, connection, false); err != nil {
		return err
	}
	return errors.Join(migrateTransaction(ctx, connection, step), setForeignKeys(ctx, connection, true))
}

func migrateTransaction(ctx context.Context, connection *sql.Conn, step func(context.Context, *sql.Conn) error) (resultErr error) {
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
	if err := step(ctx, connection); err != nil {
		return err
	}
	if _, err := connection.ExecContext(ctx, "COMMIT"); err != nil {
		return fmt.Errorf("commit schema migration: %w", err)
	}
	return nil
}

// migrateLegacyTransaction takes an exact v1 home to v2: the accounts objects,
// agents.account_id and the wider invalidations check.
func migrateLegacyTransaction(ctx context.Context, connection *sql.Conn) error {
	// Refuse anything that is not exactly the schema this migration was written
	// against, foreign key violations included.
	if err := validateSchemaVersion(ctx, connection, legacyUserVersion, legacySchemaStatements()); err != nil {
		return err
	}
	target := expectedSchemaOf(previousSchemaStatements())
	for _, name := range []string{"accounts", "accounts_provider_home_unique"} {
		if _, err := connection.ExecContext(ctx, target[name].sql); err != nil {
			return fmt.Errorf("create %s: %w", name, err)
		}
	}
	if err := rebuildTable(ctx, connection, target, "agents", legacyAgentColumns, "agents_id_project_unique", "", ""); err != nil {
		return err
	}
	if err := rebuildTable(ctx, connection, target, "invalidations", legacyInvalidationColumns, "invalidations_entity_revision_unique", "", ""); err != nil {
		return err
	}
	if _, err := connection.ExecContext(ctx, fmt.Sprintf("PRAGMA user_version = %d", previousUserVersion)); err != nil {
		return fmt.Errorf("set sqlite user version: %w", err)
	}
	// The rewritten text must be byte-identical to the v2 statements, with
	// foreign_key_check, before the next step or the commit.
	return validateSchemaVersion(ctx, connection, previousUserVersion, previousSchemaStatements())
}

// migratePreviousTransaction takes an exact v2 home to v3: the browser
// capability mask gains the administration bit, and every pairing minted on
// loopback (the ones that carry terminal_input, which a relay pairing never
// does) is granted it, pending challenges included.
func migratePreviousTransaction(ctx context.Context, connection *sql.Conn) error {
	if err := validateSchemaVersion(ctx, connection, previousUserVersion, previousSchemaStatements()); err != nil {
		return err
	}
	target := expectedSchemaOf(priorSchemaStatements())
	if err := rebuildTable(ctx, connection, target, "browser_pairing_challenges", previousPairingChallengeColumns, "", "", ""); err != nil {
		return err
	}
	if err := rebuildTable(ctx, connection, target, "browser_clients", previousBrowserClientColumns, "", "", ""); err != nil {
		return err
	}
	for _, table := range []string{"browser_pairing_challenges", "browser_clients"} {
		if _, err := connection.ExecContext(ctx, fmt.Sprintf("UPDATE %s SET capability_mask = capability_mask | %d WHERE capability_mask & %d <> 0",
			table, BrowserCapabilityAdministration, BrowserCapabilityTerminalInput)); err != nil {
			return fmt.Errorf("grant administration in %s: %w", table, err)
		}
	}
	if _, err := connection.ExecContext(ctx, fmt.Sprintf("PRAGMA user_version = %d", priorUserVersion)); err != nil {
		return fmt.Errorf("set sqlite user version: %w", err)
	}
	return validateSchemaVersion(ctx, connection, priorUserVersion, priorSchemaStatements())
}

// migratePriorTransaction takes an exact v3 home to v4: every agent gains the
// idle rule columns, holding the rule that was always in force, wait.
func migratePriorTransaction(ctx context.Context, connection *sql.Conn) error {
	if err := validateSchemaVersion(ctx, connection, priorUserVersion, priorSchemaStatements()); err != nil {
		return err
	}
	target := expectedSchemaOf(schemaStatements)
	if err := rebuildTable(ctx, connection, target, "agents", priorAgentColumns, "agents_id_project_unique", idleColumns, idleDefaults); err != nil {
		return err
	}
	if _, err := connection.ExecContext(ctx, fmt.Sprintf("PRAGMA user_version = %d", userVersion)); err != nil {
		return fmt.Errorf("set sqlite user version: %w", err)
	}
	return validateExactSchema(ctx, connection)
}

// rebuildTable replaces one table with its target definition, preserving every
// row. SQLite cannot alter the constraints of a STRICT table, and a renamed
// table keeps its old text, while the exact-schema check compares that text
// byte for byte. So the target statement is executed verbatim under the real
// name and the rows wait in a scratch table for the moment the real one is
// absent. index names the table's separate index statement, if it has one;
// added and values name columns the target has and the rows do not, with the
// constant each row gets.
func rebuildTable(ctx context.Context, connection *sql.Conn, target map[string]schemaObject, table, columns, index, added, values string) error {
	scratch := table + "_pre_migration"
	into, from := columns, columns
	if added != "" {
		into, from = columns+", "+added, columns+", "+values
	}
	statements := []string{
		"CREATE TABLE " + scratch + " AS SELECT " + columns + " FROM " + table,
		"DROP TABLE " + table,
		target[table].sql,
		"INSERT INTO " + table + "(" + into + ") SELECT " + from + " FROM " + scratch,
		"DROP TABLE " + scratch,
	}
	if index != "" {
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
