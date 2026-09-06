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
	for _, version := range []int{legacyUserVersion, previousUserVersion, priorUserVersion} {
		for _, persistWAL := range []bool{false, true} {
			t.Run(fmt.Sprintf("v%d/wal=%v", version, persistWAL), func(t *testing.T) {
				testLegacyHomeMigratesAndKeepsEveryRow(t, version, persistWAL)
			})
		}
	}
}

func testLegacyHomeMigratesAndKeepsEveryRow(t *testing.T, version int, persistWAL bool) {
	ctx := context.Background()
	path, before := newLegacyDatabase(t, persistWAL, version)

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
	if _, got, err := inspectIdentity(ctx, connection); err != nil || got != userVersion {
		t.Fatalf("user_version = %d, %v, want %d", got, err, userVersion)
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
	// Every migrated agent keeps the rule that was always true: wait, with
	// nothing spent.
	var ruled int
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM agents WHERE idle_policy <> 'wait' OR idle_after_seconds <> 0 OR idle_instruction <> '' OR idle_run_budget <> 0 OR idle_runs_used <> 0`).Scan(&ruled); err != nil {
		t.Fatal(err)
	}
	if ruled != 0 {
		t.Fatalf("migration invented an idle rule on %d agents", ruled)
	}
	// Loopback pairings (the ones with terminal_input) gained
	// administration; the relay pairing did not. Every pending
	// challenge is treated the same way.
	for _, table := range []string{"browser_clients", "browser_pairing_challenges"} {
		var loopback, relay int
		if err := connection.QueryRowContext(ctx, `SELECT MIN(capability_mask), MAX(capability_mask) FROM `+table).Scan(&relay, &loopback); err != nil {
			t.Fatal(err)
		}
		if want := int(BrowserCapabilityKnownMask); loopback != want || relay != int(testRelayMask) {
			t.Fatalf("%s masks after migration: loopback=%d relay=%d, want %d and %d", table, loopback, relay, want, testRelayMask)
		}
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
	if _, got, err := inspectIdentity(ctx, again); err != nil || got != userVersion {
		t.Fatalf("reopened user_version = %d, %v, want %d", got, err, userVersion)
	}
	if after := snapshotRows(t, ctx, again); !reflect.DeepEqual(before, after) {
		t.Fatal("reopening a migrated home changed rows")
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
	path, _ := newLegacyDatabase(t, false, legacyUserVersion, `CREATE TABLE stowaway (id INTEGER PRIMARY KEY)`)
	store, err := Open(context.Background(), path)
	if store != nil {
		store.Close()
	}
	if !errors.Is(err, ErrForeignDatabase) {
		t.Fatalf("Open = %v, want ErrForeignDatabase", err)
	}
	requireUnmigrated(t, path, legacyUserVersion)
}

func TestLegacyHomeWithForeignKeyViolationRefusesToMigrate(t *testing.T) {
	orphan := `INSERT INTO tasks(id, project_id, assigned_agent_id, incarnation_id, work_revision, title, body, status, priority, blocked_reason, result, completed_at_ms, revision, created_at_ms, updated_at_ms)
		VALUES(X'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', X'01010101010101010101010101010101', X'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', X'abababababababababababababababab', 1, 't', '', 'queued', 0, NULL, NULL, NULL, 1, 6, 6)`
	path, _ := newLegacyDatabase(t, false, legacyUserVersion, orphan)
	store, err := Open(context.Background(), path)
	if store != nil {
		store.Close()
	}
	if !errors.Is(err, ErrCorruptState) {
		t.Fatalf("Open = %v, want ErrCorruptState", err)
	}
	requireUnmigrated(t, path, legacyUserVersion)
}

// requireUnmigrated proves a refused open left the home exactly as it was.
func requireUnmigrated(t *testing.T, path string, wantVersion int) {
	t.Helper()
	ctx := context.Background()
	pool, connection := openRawDatabase(t, path, false)
	defer pool.Close()
	defer connection.Close()
	if _, version, err := inspectIdentity(ctx, connection); err != nil || version != wantVersion {
		t.Fatalf("refused open left user_version = %d, %v, want %d", version, err, wantVersion)
	}
	// The objects each step rewrites still carry their earlier text.
	var stale, accounts, ruled int
	if err := connection.QueryRowContext(ctx, `SELECT
		(SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('browser_clients', 'browser_pairing_challenges') AND sql LIKE '%BETWEEN 1 AND 15%'),
		(SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('accounts', 'accounts_provider_home_unique')),
		(SELECT COUNT(*) FROM sqlite_schema WHERE name = 'agents' AND sql LIKE '%idle_policy%')`).Scan(&stale, &accounts, &ruled); err != nil {
		t.Fatal(err)
	}
	if ruled != 0 {
		t.Fatalf("refused open rewrote agents with the idle rule for v%d", wantVersion)
	}
	wantAccounts := 2 // the table and its unique index, absent from a v1 home
	if wantVersion == legacyUserVersion {
		wantAccounts = 0
	}
	wantStale := 2 // both browser tables still bound at 15, until v3
	if wantVersion == priorUserVersion {
		wantStale = 0
	}
	if stale != wantStale || accounts != wantAccounts {
		t.Fatalf("refused open rewrote objects: pre-v3 browser tables=%d accounts objects=%d for v%d, want %d and %d", stale, accounts, wantVersion, wantStale, wantAccounts)
	}
}

// The relay pairing mask: every bit but terminal_input and administration.
const testRelayMask = BrowserCapabilityObserve | BrowserCapabilityPrivateHumanRequestDetail | BrowserCapabilityHumanActions

// newLegacyDatabase builds a populated home in the exact shape of an earlier
// user_version by downgrading a real one: every row is written through the
// public API, then the objects later versions changed are put back the way
// that version had them. The returned snapshot is every row, for comparison
// after the migration.
func newLegacyDatabase(t *testing.T, persistWAL bool, version int, extra ...string) (string, map[string][]string) {
	t.Helper()
	ctx := context.Background()
	store, path := newTestStore(t)
	seedDurableAuthority(t, store)
	// One loopback pairing and one relay pairing, each redeemed and each
	// with a second challenge still pending, written the way the daemon
	// writes them. Before v3 the loopback mask was every bit but
	// administration; from v3 it is the full mask.
	loopback := BrowserCapabilityObserve | BrowserCapabilityPrivateHumanRequestDetail | BrowserCapabilityHumanActions | BrowserCapabilityTerminalInput
	if version >= priorUserVersion {
		loopback = BrowserCapabilityKnownMask
	}
	boot := browserTestBoot(t, 1)
	for index, mask := range []BrowserCapabilityMask{loopback, testRelayMask} {
		redeemed := HashBrowserChallenge([]byte(fmt.Sprintf("redeemed %d", index)))
		pending := HashBrowserChallenge([]byte(fmt.Sprintf("pending %d", index)))
		for _, digest := range []BrowserChallengeDigest{redeemed, pending} {
			if _, err := store.CreateBrowserPairingChallenge(ctx, digest, boot, "https://app.example", mask, mustTime(t, 1), mustTime(t, 2)); err != nil {
				t.Fatalf("mint pairing challenge: %v", err)
			}
		}
		if _, err := store.RedeemBrowserPairingChallenge(ctx, redeemed, boot, "https://app.example", browserTestID(t, byte(10+index)), browserKey(t), mustTime(t, 1)); err != nil {
			t.Fatalf("redeem pairing challenge: %v", err)
		}
	}
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
	wantStatements, migrates := migratableSchema(version)
	if !migrates {
		t.Fatalf("no migratable schema for v%d", version)
	}
	legacy := expectedSchemaOf(wantStatements)
	statements := []string{"BEGIN IMMEDIATE"}
	statements = append(statements, extra...)
	for _, statement := range statements {
		if _, err := connection.ExecContext(ctx, statement); err != nil {
			t.Fatalf("prepare legacy home: %v", err)
		}
	}
	// Every version before v4 has agents without the idle rule; the columns
	// the fixture carries down are the version's own.
	agentColumnsFor := testAgentColumnsV3
	if version == legacyUserVersion {
		agentColumnsFor = testAgentColumns
	}
	if err := rebuildTable(ctx, connection, legacy, "agents", agentColumnsFor, "agents_id_project_unique", "", ""); err != nil {
		t.Fatal(err)
	}
	if version < priorUserVersion {
		if err := rebuildTable(ctx, connection, legacy, "browser_pairing_challenges", previousPairingChallengeColumns, "", "", ""); err != nil {
			t.Fatal(err)
		}
		if err := rebuildTable(ctx, connection, legacy, "browser_clients", previousBrowserClientColumns, "", "", ""); err != nil {
			t.Fatal(err)
		}
	}
	downgrade := []string{fmt.Sprintf("PRAGMA user_version = %d", version), "COMMIT"}
	if version == legacyUserVersion {
		if err := rebuildTable(ctx, connection, legacy, "invalidations", legacyInvalidationColumns, "invalidations_entity_revision_unique", "", ""); err != nil {
			t.Fatal(err)
		}
		downgrade = append([]string{"DROP TABLE accounts"}, downgrade...)
	}
	for _, statement := range downgrade {
		if _, err := connection.ExecContext(ctx, statement); err != nil {
			t.Fatalf("downgrade to v%d: %v", version, err)
		}
	}
	if len(extra) == 0 {
		if err := validateSchemaVersion(ctx, connection, version, wantStatements); err != nil {
			t.Fatalf("fixture is not an exact v%d home: %v", version, err)
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

// testAgentColumns is the v1 agents column list spelled out, used to build the
// fixture and to read it back. Both have to stay independent of
// legacyAgentColumns: sharing that constant would drop a column from the
// fixture and from the comparison at the same time, hiding a column the
// migration stopped copying.
const testAgentColumns = `id, project_id, name, role, provider, model, reasoning_effort, paused, tool_budget_limit, tool_calls_used, revision, created_at_ms, updated_at_ms`

// testAgentColumnsV3 is the v2/v3 agent row: v1 plus account_id, before the idle rule.
const testAgentColumnsV3 = `id, project_id, name, role, provider, model, reasoning_effort, account_id, paused, tool_budget_limit, tool_calls_used, revision, created_at_ms, updated_at_ms`

var testBrowserColumns = map[string]string{
	"browser_pairing_challenges": strings.ReplaceAll(previousPairingChallengeColumns, "capability_mask, ", ""),
	"browser_clients":            strings.ReplaceAll(previousBrowserClientColumns, "capability_mask, ", ""),
}

// snapshotRows reads every v1 table, agents through the list above because the
// added account_id makes SELECT * differ either side of the migration.
func snapshotRows(t *testing.T, ctx context.Context, connection *sql.Conn) map[string][]string {
	t.Helper()
	result := make(map[string][]string)
	for name, object := range expectedSchemaOf(legacySchemaStatements()) {
		if object.kind != "table" {
			continue
		}
		columns := "*"
		switch name {
		case "agents":
			columns = testAgentColumns
		case "browser_pairing_challenges", "browser_clients":
			// The v3 migration rewrites masks on purpose; the migration test
			// asserts them separately.
			columns = testBrowserColumns[name]
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
	var projects, agents, providers, tasks, runs, kinds, clients, challenges, events int
	if err := connection.QueryRowContext(ctx, `SELECT
		(SELECT COUNT(*) FROM projects), (SELECT COUNT(*) FROM agents), (SELECT COUNT(DISTINCT provider) FROM agents),
		(SELECT COUNT(*) FROM tasks), (SELECT COUNT(*) FROM runs), (SELECT COUNT(DISTINCT entity_kind) FROM invalidations),
		(SELECT COUNT(DISTINCT capability_mask) FROM browser_clients), (SELECT COUNT(DISTINCT capability_mask) FROM browser_pairing_challenges),
		(SELECT COUNT(*) FROM browser_security_events WHERE client_id IS NOT NULL)`).
		Scan(&projects, &agents, &providers, &tasks, &runs, &kinds, &clients, &challenges, &events); err != nil {
		t.Fatal(err)
	}
	if projects < 1 || agents < 3 || providers != 3 || tasks < 1 || runs < 1 || kinds != 7 || clients != 2 || challenges != 2 || events < 1 {
		t.Fatalf("thin fixture: projects=%d agents=%d providers=%d tasks=%d runs=%d invalidation kinds=%d client masks=%d challenge masks=%d client events=%d",
			projects, agents, providers, tasks, runs, kinds, clients, challenges, events)
	}
}

// TestSchemaDigestsArePinned trips on any schema edit. Both sets need a pin:
// legacySchemaStatements derives every unchanged statement live from
// schemaStatements, so an edit there rewrites what v1 is claimed to have been
// and real v1 homes stop opening, while an edit to an object the v1 derivation
// drops or replaces (accounts, agents, invalidations) moves the target without
// touching the v1 digest and every migrated home is refused on the next start.
// A schema change re-pins both, and re-pinning without freezing the replaced
// text and extending the migration ships the outage this migration exists to
// fix.
func TestSchemaDigestsArePinned(t *testing.T) {
	for _, pin := range []struct {
		name       string
		statements []string
		digest     string
	}{
		{"current", schemaStatements, "6eb8be2af2f3efc8ed7d40ecf9bd1ec316675e39ad11fb8b0827a228e9232cf1"},
		{"v3", priorSchemaStatements(), "2d5319a0afce6206d963631465833bc5f25d0f2261537f4f33c92a8e38a36009"},
		{"v2", previousSchemaStatements(), "6a1de54c3fcad5f6770c6d80b91fda3f914e8b236f34d62bb875a7f4efde347c"},
		{"v1", legacySchemaStatements(), "63a444a2fe57a994b712bfe5b56764d684b2cb3ed73d7324465d894107f96f33"},
	} {
		sum := sha256.Sum256([]byte(strings.Join(pin.statements, "\n")))
		if got := hex.EncodeToString(sum[:]); got != pin.digest {
			t.Errorf("%s schema digest = %s, want %s", pin.name, got, pin.digest)
		}
	}
}

// TestLegacyHomeWithBrokenDurableStateRollsBackAndRefuses is the only refusal
// that reaches inside the migration transaction: the two above are rejected by
// the preflight, on its disposable copy, before any pool exists.
func TestLegacyHomeWithBrokenDurableStateRollsBackAndRefuses(t *testing.T) {
	for _, version := range []int{legacyUserVersion, previousUserVersion, priorUserVersion} {
		t.Run(fmt.Sprintf("v%d", version), func(t *testing.T) {
			testLegacyHomeWithBrokenDurableStateRollsBackAndRefuses(t, version)
		})
	}
}

func testLegacyHomeWithBrokenDurableStateRollsBackAndRefuses(t *testing.T, version int) {
	ctx := context.Background()
	// An invalidation head that no longer matches the log passes the exact
	// schema, the integrity check and foreign_key_check that the preflight
	// runs, and fails only the durable-control pass inside the transaction.
	path, before := newLegacyDatabase(t, false, version, `UPDATE factory SET next_invalidation_sequence = next_invalidation_sequence + 5 WHERE singleton = 1`)
	evidence := captureDatabaseEvidence(t, path)
	store, err := Open(ctx, path)
	if store != nil {
		store.Close()
	}
	if !errors.Is(err, ErrCorruptState) {
		t.Fatalf("Open = %v, want ErrCorruptState", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, evidence)
	requireUnmigrated(t, path, version)

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
	path, _ := newLegacyDatabase(t, false, legacyUserVersion, `UPDATE factory SET next_invalidation_sequence = next_invalidation_sequence + 5 WHERE singleton = 1`)
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
