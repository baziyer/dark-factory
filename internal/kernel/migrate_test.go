package kernel

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"reflect"
	"strings"
	"testing"

	"github.com/ncruces/go-sqlite3"
	sqliteDriver "github.com/ncruces/go-sqlite3/driver"
)

func TestLegacyHomeMigratesAndKeepsEveryRow(t *testing.T) {
	for _, persistWAL := range []bool{false, true} {
		t.Run(fmt.Sprintf("wal=%v", persistWAL), func(t *testing.T) {
			ctx := context.Background()
			path, before := newLegacyDatabase(t, persistWAL)

			store, err := Open(ctx, path)
			if err != nil {
				t.Fatalf("Open legacy home: %v", err)
			}
			defer store.Close()
			connection, err := store.readerConnection(ctx)
			if err != nil {
				t.Fatal(err)
			}
			defer connection.Close()
			if _, version, err := inspectIdentity(ctx, connection); err != nil || version != userVersion {
				t.Fatalf("user_version = %d, %v, want %d", version, err, userVersion)
			}
			if err := validateExactSchema(ctx, connection); err != nil {
				t.Fatalf("validateExactSchema after migration: %v", err)
			}
			if after := snapshotRows(t, ctx, connection); !reflect.DeepEqual(before, after) {
				for table := range before {
					if !reflect.DeepEqual(before[table], after[table]) {
						t.Errorf("%s changed:\nbefore %v\nafter  %v", table, before[table], after[table])
					}
				}
				t.Fatal("migration did not preserve every row")
			}
			var accounts, adopted int
			if err := connection.QueryRowContext(ctx, `SELECT (SELECT COUNT(*) FROM accounts), (SELECT COUNT(*) FROM agents WHERE account_id IS NOT NULL)`).Scan(&accounts, &adopted); err != nil {
				t.Fatal(err)
			}
			if accounts != 0 || adopted != 0 {
				t.Fatalf("migration invented %d accounts and %d agent links", accounts, adopted)
			}
			if err := connection.Close(); err != nil {
				t.Fatal(err)
			}
			if err := store.Close(); err != nil {
				t.Fatalf("Close: %v", err)
			}

			// A second open must find nothing to migrate.
			reopened, err := Open(ctx, path)
			if err != nil {
				t.Fatalf("reopen migrated home: %v", err)
			}
			defer reopened.Close()
			again, err := reopened.readerConnection(ctx)
			if err != nil {
				t.Fatal(err)
			}
			defer again.Close()
			if _, version, err := inspectIdentity(ctx, again); err != nil || version != userVersion {
				t.Fatalf("reopened user_version = %d, %v, want %d", version, err, userVersion)
			}
			if after := snapshotRows(t, ctx, again); !reflect.DeepEqual(before, after) {
				t.Fatal("reopening a migrated home changed rows")
			}
		})
	}
}

func TestCurrentHomeOpensWithoutMigration(t *testing.T) {
	ctx := context.Background()
	store, path := newTestStore(t)
	seedDurableAuthority(t, store)
	connection, err := store.readerConnection(ctx)
	if err != nil {
		t.Fatal(err)
	}
	before := snapshotRows(t, ctx, connection)
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, err := Open(ctx, path)
	if err != nil {
		t.Fatalf("Open current home: %v", err)
	}
	defer reopened.Close()
	again, err := reopened.readerConnection(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if _, version, err := inspectIdentity(ctx, again); err != nil || version != userVersion {
		t.Fatalf("user_version = %d, %v, want %d", version, err, userVersion)
	}
	if after := snapshotRows(t, ctx, again); !reflect.DeepEqual(before, after) {
		t.Fatal("opening a current home changed rows")
	}
}

func TestLegacyHomeWithUnknownObjectRefusesToMigrate(t *testing.T) {
	path, _ := newLegacyDatabase(t, false, `CREATE TABLE stowaway (id INTEGER PRIMARY KEY)`)
	store, err := Open(context.Background(), path)
	if store != nil {
		store.Close()
	}
	if !errors.Is(err, ErrForeignDatabase) {
		t.Fatalf("Open = %v, want ErrForeignDatabase", err)
	}
	requireUnmigrated(t, path)
}

func TestLegacyHomeWithForeignKeyViolationRefusesToMigrate(t *testing.T) {
	orphan := `INSERT INTO tasks(id, project_id, assigned_agent_id, incarnation_id, work_revision, title, body, status, priority, blocked_reason, result, completed_at_ms, revision, created_at_ms, updated_at_ms)
		VALUES(X'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', X'01010101010101010101010101010101', X'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', X'abababababababababababababababab', 1, 't', '', 'queued', 0, NULL, NULL, NULL, 1, 6, 6)`
	path, _ := newLegacyDatabase(t, false, orphan)
	store, err := Open(context.Background(), path)
	if store != nil {
		store.Close()
	}
	if !errors.Is(err, ErrCorruptState) {
		t.Fatalf("Open = %v, want ErrCorruptState", err)
	}
	requireUnmigrated(t, path)
}

// requireUnmigrated proves a refused open left the home exactly as it was.
func requireUnmigrated(t *testing.T, path string) {
	t.Helper()
	ctx := context.Background()
	pool, connection := openRawDatabase(t, path, false)
	defer pool.Close()
	defer connection.Close()
	if _, version, err := inspectIdentity(ctx, connection); err != nil || version != legacyUserVersion {
		t.Fatalf("refused open left user_version = %d, %v, want %d", version, err, legacyUserVersion)
	}
	var objects int
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('accounts', 'accounts_provider_home_unique')`).Scan(&objects); err != nil {
		t.Fatal(err)
	}
	if objects != 0 {
		t.Fatalf("refused open created %d accounts objects", objects)
	}
}

// newLegacyDatabase builds a populated home in the exact pre-accounts shape by
// downgrading a real one: every row is written through the public API, then the
// three objects the accounts slice changed are put back the way v1 had them.
// The returned snapshot is every v1 row, for comparison after the migration.
func newLegacyDatabase(t *testing.T, persistWAL bool, extra ...string) (string, map[string][]string) {
	t.Helper()
	ctx := context.Background()
	store, path := newTestStore(t)
	seedDurableAuthority(t, store)
	project := projectID(t, 1)
	for _, agent := range []struct {
		seed     byte
		provider Provider
	}{{20, ProviderShell}, {21, ProviderClaudeCode}} {
		spec := NewAgent{ID: agentID(t, agent.seed), ProjectID: project, Name: agent.provider.String() + "-agent", Role: RoleWorker, Provider: agent.provider, ToolBudgetLimit: 4}
		if agent.provider != ProviderShell {
			spec.Model = "opus"
			spec.ReasoningEffort = "high"
		}
		if _, err := store.CreateAgent(ctx, spec, mustTime(t, 6)); err != nil {
			t.Fatalf("create %s agent: %v", agent.provider, err)
		}
	}
	if _, err := store.SetDispatch(ctx, mustRevision(t, 1), true, mustTime(t, 7)); err != nil {
		t.Fatalf("SetDispatch: %v", err)
	}
	// The remaining invalidation kinds have no public writer that reaches them
	// from this fixture, so append them the way the log itself is written.
	for _, kind := range []string{"change", "run", "human_request"} {
		if _, err := store.writer.Exec(`INSERT INTO invalidations(sequence, occurred_at_ms, entity_kind, entity_id, revision, deleted)
			SELECT next_invalidation_sequence, 8, ?, X'0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c', 1, 0 FROM factory`, kind); err != nil {
			t.Fatalf("append %s invalidation: %v", kind, err)
		}
		if _, err := store.writer.Exec(`UPDATE factory SET next_invalidation_sequence = next_invalidation_sequence + 1, updated_at_ms = 8 WHERE singleton = 1`); err != nil {
			t.Fatal(err)
		}
	}
	if err := store.Close(); err != nil {
		t.Fatalf("Close seeded store: %v", err)
	}

	pool, connection := openRawDatabase(t, path, persistWAL)
	if err := setForeignKeys(ctx, connection, false); err != nil {
		t.Fatal(err)
	}
	legacy := expectedSchemaOf(legacySchemaStatements())
	statements := []string{"BEGIN IMMEDIATE"}
	statements = append(statements, extra...)
	for _, statement := range statements {
		if _, err := connection.ExecContext(ctx, statement); err != nil {
			t.Fatalf("prepare legacy home: %v", err)
		}
	}
	if err := rebuildTable(ctx, connection, legacy, "agents", legacyAgentColumns, "agents_id_project_unique"); err != nil {
		t.Fatal(err)
	}
	if err := rebuildTable(ctx, connection, legacy, "invalidations", legacyInvalidationColumns, "invalidations_entity_revision_unique"); err != nil {
		t.Fatal(err)
	}
	for _, statement := range []string{"DROP TABLE accounts", fmt.Sprintf("PRAGMA user_version = %d", legacyUserVersion), "COMMIT"} {
		if _, err := connection.ExecContext(ctx, statement); err != nil {
			t.Fatalf("downgrade to v1: %v", err)
		}
	}
	if len(extra) == 0 {
		if err := validateSchemaVersion(ctx, connection, legacyUserVersion, legacySchemaStatements()); err != nil {
			t.Fatalf("fixture is not an exact v1 home: %v", err)
		}
	}
	requireLegacyPopulation(t, ctx, connection)
	before := snapshotRows(t, ctx, connection)
	if err := errors.Join(connection.Close(), pool.Close()); err != nil {
		t.Fatal(err)
	}
	// SQLite creates sidecars from the process umask; a real home has them 0600.
	for _, sidecar := range []string{path + "-wal", path + "-shm"} {
		if _, err := os.Stat(sidecar); err == nil {
			if err := os.Chmod(sidecar, 0o600); err != nil {
				t.Fatal(err)
			}
		}
	}
	return path, before
}

func openRawDatabase(t *testing.T, path string, persistWAL bool) (*sql.DB, *sql.Conn) {
	t.Helper()
	pool, err := sql.Open(driverName, configuredDataSource(path))
	if err != nil {
		t.Fatal(err)
	}
	pool.SetMaxOpenConns(1)
	pool.SetMaxIdleConns(1)
	connection, err := pool.Conn(context.Background())
	if err != nil {
		t.Fatal(errors.Join(err, pool.Close()))
	}
	if !persistWAL {
		return pool, connection
	}
	// The operational store keeps its WAL sidecars across shutdown, so a home
	// that a stopped daemon left behind still has them.
	if err := connection.Raw(func(driverConnection any) error {
		_, err := driverConnection.(sqliteDriver.Conn).Raw().FileControl("", sqlite3.FCNTL_PERSIST_WAL, true)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	return pool, connection
}

// snapshotRows reads every v1 table, agents through its v1 columns so the added
// account_id cannot hide a lost value.
func snapshotRows(t *testing.T, ctx context.Context, connection *sql.Conn) map[string][]string {
	t.Helper()
	result := make(map[string][]string)
	for name, object := range expectedSchemaOf(legacySchemaStatements()) {
		if object.kind != "table" {
			continue
		}
		columns := "*"
		if name == "agents" {
			columns = legacyAgentColumns
		}
		rows, err := connection.QueryContext(ctx, "SELECT "+columns+" FROM "+name+" ORDER BY 1")
		if err != nil {
			t.Fatalf("read %s: %v", name, err)
		}
		names, err := rows.Columns()
		if err != nil {
			t.Fatal(errors.Join(err, rows.Close()))
		}
		values := make([]any, len(names))
		targets := make([]any, len(names))
		for index := range values {
			targets[index] = &values[index]
		}
		result[name] = []string{}
		for rows.Next() {
			if err := rows.Scan(targets...); err != nil {
				t.Fatal(errors.Join(err, rows.Close()))
			}
			result[name] = append(result[name], fmt.Sprintf("%v", values))
		}
		if err := errors.Join(rows.Err(), rows.Close()); err != nil {
			t.Fatalf("read %s: %v", name, err)
		}
	}
	return result
}

// requireLegacyPopulation keeps the migration proof from passing on an empty
// database: every table the migration touches has to carry real rows.
func requireLegacyPopulation(t *testing.T, ctx context.Context, connection *sql.Conn) {
	t.Helper()
	var projects, agents, providers, tasks, runs, kinds int
	if err := connection.QueryRowContext(ctx, `SELECT
		(SELECT COUNT(*) FROM projects), (SELECT COUNT(*) FROM agents), (SELECT COUNT(DISTINCT provider) FROM agents),
		(SELECT COUNT(*) FROM tasks), (SELECT COUNT(*) FROM runs), (SELECT COUNT(DISTINCT entity_kind) FROM invalidations)`).
		Scan(&projects, &agents, &providers, &tasks, &runs, &kinds); err != nil {
		t.Fatal(err)
	}
	if projects < 1 || agents < 3 || providers != 3 || tasks < 1 || runs < 1 || kinds != 7 {
		t.Fatalf("thin fixture: projects=%d agents=%d providers=%d tasks=%d runs=%d invalidation kinds=%d", projects, agents, providers, tasks, runs, kinds)
	}
}

// TestLegacySchemaIsPinned trips on any schema edit, because
// legacySchemaStatements derives every unchanged statement from
// schemaStatements: a new statement there would rewrite what v1 is claimed to
// have been, and real v1 homes would stop opening. Re-pinning this digest
// without freezing the replaced text and extending the migration ships the
// outage this migration exists to fix.
func TestLegacySchemaIsPinned(t *testing.T) {
	const pinned = "63a444a2fe57a994b712bfe5b56764d684b2cb3ed73d7324465d894107f96f33"
	digest := sha256.Sum256([]byte(strings.Join(legacySchemaStatements(), "\n")))
	if got := hex.EncodeToString(digest[:]); got != pinned {
		t.Fatalf("v1 schema digest = %s, want %s", got, pinned)
	}
}

// TestLegacyHomeWithBrokenDurableStateRollsBackAndRefuses is the only refusal
// that reaches inside the migration transaction: the two above are rejected by
// the preflight, on its disposable copy, before any pool exists.
func TestLegacyHomeWithBrokenDurableStateRollsBackAndRefuses(t *testing.T) {
	ctx := context.Background()
	// An invalidation head that no longer matches the log passes the exact
	// schema, the integrity check and foreign_key_check that the preflight
	// runs, and fails only the durable-control pass inside the transaction.
	path, before := newLegacyDatabase(t, false, `UPDATE factory SET next_invalidation_sequence = next_invalidation_sequence + 5 WHERE singleton = 1`)
	evidence := captureDatabaseEvidence(t, path)
	store, err := Open(ctx, path)
	if store != nil {
		store.Close()
	}
	if !errors.Is(err, ErrCorruptState) {
		t.Fatalf("Open = %v, want ErrCorruptState", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, evidence)
	requireUnmigrated(t, path)

	// Repaired, the same home migrates and keeps the rows it always had.
	pool, connection := openRawDatabase(t, path, false)
	if _, err := connection.ExecContext(ctx, `UPDATE factory SET next_invalidation_sequence = next_invalidation_sequence - 5 WHERE singleton = 1`); err != nil {
		t.Fatal(err)
	}
	if err := errors.Join(connection.Close(), pool.Close()); err != nil {
		t.Fatal(err)
	}
	repaired, err := Open(ctx, path)
	if err != nil {
		t.Fatalf("Open repaired home: %v", err)
	}
	defer repaired.Close()
	reader, err := repaired.readerConnection(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	if _, version, err := inspectIdentity(ctx, reader); err != nil || version != userVersion {
		t.Fatalf("repaired user_version = %d, %v, want %d", version, err, userVersion)
	}
	after := snapshotRows(t, ctx, reader)
	before["factory"] = after["factory"] // the repair rewrote the invalidation head
	if !reflect.DeepEqual(before, after) {
		t.Fatal("migration after repair did not preserve every row")
	}
}

// TestRefusedMigrationReturnsTheWriterConnection covers what the rollback is
// for. Closing the connection would roll the transaction back anyway, but a
// connection left mid-transaction is destroyed rather than returned, and the
// operational writer set is sealed at activation and cannot mint a
// replacement.
func TestRefusedMigrationReturnsTheWriterConnection(t *testing.T) {
	ctx := context.Background()
	path, _ := newLegacyDatabase(t, false, `UPDATE factory SET next_invalidation_sequence = next_invalidation_sequence + 5 WHERE singleton = 1`)
	store, err := openPools(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if err := store.migrateLegacy(ctx); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("migrateLegacy = %v, want ErrCorruptState", err)
	}
	if stats := store.writer.Stats(); stats.OpenConnections != 1 || stats.Idle != 1 {
		t.Fatalf("refused migration did not return the writer connection: open=%d idle=%d", stats.OpenConnections, stats.Idle)
	}
}
