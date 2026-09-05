package kernel

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"database/sql"
	"errors"
	"fmt"
	"math/big"
	"path/filepath"
	"sync"
	"testing"
)

func newBrowserStore(t *testing.T) (*Store, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "kernel.db")
	var err error
	path, err = canonicalTestDatabasePath(path)
	if err != nil {
		t.Fatal(err)
	}
	store, err := createTestStore(context.Background(), path, FactoryConfig{}, mustTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	return store, path
}

func browserKey(t *testing.T) []byte {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return elliptic.Marshal(elliptic.P256(), key.X, key.Y)
}

func mintBrowserChallenge(t *testing.T, store *Store, seed byte, boot BootID, created, expires int64, capabilities BrowserCapabilityMask) BrowserChallengeDigest {
	t.Helper()
	digest := HashBrowserChallenge([]byte(fmt.Sprintf("causal-challenge-%d", seed)))
	if _, err := store.CreateBrowserPairingChallenge(context.Background(), digest, boot, "https://app.example", capabilities, mustTime(t, created), mustTime(t, expires)); err != nil {
		t.Fatal(err)
	}
	return digest
}

func TestBrowserChallengeCommitOutcomeReconcilesExactIdentity(t *testing.T) {
	for _, test := range []struct {
		name         string
		fault        storeFaultKind
		wantDigest   bool
		wantRows     int64
		wantRetained bool
	}{
		{name: "commit before apply", fault: storeFaultCommitBefore, wantRows: 0},
		{name: "commit after apply", fault: storeFaultCommitAfter, wantDigest: true, wantRows: 1, wantRetained: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, path := newBrowserStore(t)
			defer store.Close()
			boot := browserTestBoot(t, 190)
			digest := HashBrowserChallenge([]byte("ambiguous-browser-challenge"))
			plan := installFaultWriter(t, store, path)
			plan.arm(test.fault)
			challenge, err := store.CreateBrowserPairingChallenge(context.Background(), digest, boot, "https://app.example", BrowserCapabilityObserve, mustTime(t, 10), mustTime(t, 100))
			requireStoreOutcomeUnknown(t, err)
			assertFaultWriterDisposition(t, store, plan, test.wantRetained)
			got := !bytes.Equal(challenge.Digest.Bytes(), make([]byte, DigestBytes))
			if got != test.wantDigest {
				t.Fatalf("returned cleanup identity known = %t, want %t", got, test.wantDigest)
			}
			if got := browserTableCount(t, store, "browser_pairing_challenges"); got != test.wantRows {
				t.Fatalf("durable challenge rows = %d, want %d", got, test.wantRows)
			}
			if test.wantDigest && !bytes.Equal(challenge.Digest.Bytes(), digest.Bytes()) {
				t.Fatalf("returned digest = %x, want exact %x", challenge.Digest.Bytes(), digest.Bytes())
			}
		})
	}
}

func TestInvalidateBrowserPairingChallengesSurfacesAmbiguousCommit(t *testing.T) {
	for _, test := range []struct {
		name         string
		fault        storeFaultKind
		wantRows     int64
		wantRetained bool
	}{
		{name: "commit before apply", fault: storeFaultCommitBefore, wantRows: 1},
		{name: "commit after apply", fault: storeFaultCommitAfter, wantRows: 0, wantRetained: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, path := newBrowserStore(t)
			defer store.Close()
			boot := browserTestBoot(t, 191)
			mintBrowserChallenge(t, store, 191, boot, 10, 100, BrowserCapabilityObserve)
			plan := installFaultWriter(t, store, path)
			plan.arm(test.fault)
			err := store.InvalidateBrowserPairingChallenges(context.Background(), boot)
			requireStoreOutcomeUnknown(t, err)
			assertFaultWriterDisposition(t, store, plan, test.wantRetained)
			if got := browserTableCount(t, store, "browser_pairing_challenges"); got != test.wantRows {
				t.Fatalf("remaining challenge rows = %d, want %d", got, test.wantRows)
			}
		})
	}
}

func pairBrowserClient(t *testing.T, store *Store, digest BrowserChallengeDigest, boot BootID, id BrowserClientID, publicKey []byte, at int64) BrowserClient {
	t.Helper()
	client, err := store.RedeemBrowserPairingChallenge(context.Background(), digest, boot, "https://app.example", id, publicKey, mustTime(t, at))
	if err != nil {
		t.Fatal(err)
	}
	return client
}

func browserTableCount(t *testing.T, store *Store, table string) int64 {
	t.Helper()
	var count int64
	if err := store.readers.QueryRow("SELECT COUNT(*) FROM " + table).Scan(&count); err != nil {
		t.Fatal(err)
	}
	return count
}

func TestBrowserCapabilityHasRequiresSingleKnownBit(t *testing.T) {
	mask := BrowserCapabilityObserve | BrowserCapabilityTerminalInput
	if !mask.Has(BrowserCapabilityObserve) || !mask.Has(BrowserCapabilityTerminalInput) {
		t.Fatalf("known capability probe rejected: %v", mask)
	}
	for _, probe := range []BrowserCapabilityMask{0, BrowserCapabilityObserve | BrowserCapabilityTerminalInput, BrowserCapabilityMask(16)} {
		if mask.Has(probe) {
			t.Fatalf("invalid capability probe accepted: %v", probe)
		}
	}
}

func TestBrowserClientPagesAreBoundedDeterministicAndPrivate(t *testing.T) {
	store, _ := newBrowserStore(t)
	defer store.Close()
	ctx := context.Background()
	boot := browserTestBoot(t, 201)
	for index := 0; index < BrowserClientPageSize+2; index++ {
		digest := mintBrowserChallenge(t, store, byte(index+1), boot, int64(10+index), 10_000, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail)
		pairBrowserClient(t, store, digest, boot, browserTestID(t, byte(index+1)), browserKey(t), int64(20+index))
	}
	first, err := store.ListBrowserClients(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(first.Items) != BrowserClientPageSize || first.NextAfter == nil || first.Items[0].ID != browserTestID(t, 1) || first.Items[len(first.Items)-1].ID != browserTestID(t, BrowserClientPageSize) {
		t.Fatalf("first page = len %d next %v first %v last %v", len(first.Items), first.NextAfter, first.Items[0].ID, first.Items[len(first.Items)-1].ID)
	}
	second, err := store.ListBrowserClients(ctx, first.NextAfter)
	if err != nil {
		t.Fatal(err)
	}
	if len(second.Items) != 2 || second.NextAfter != nil || second.Items[0].ID != browserTestID(t, BrowserClientPageSize+1) {
		t.Fatalf("second page = %+v", second)
	}
	if _, err := store.ListBrowserClients(ctx, &BrowserClientID{}); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("zero cursor error = %v", err)
	}
	unknown := browserTestID(t, 250)
	if _, err := store.ListBrowserClients(ctx, &unknown); !errors.Is(err, ErrNotFound) {
		t.Fatalf("unknown cursor error = %v", err)
	}
	counts, err := store.BrowserClientCounts(ctx, boot, mustTime(t, 100))
	if err != nil || counts.Active != BrowserClientPageSize+2 || counts.Revoked != 0 || counts.ActiveChallenges != 0 {
		t.Fatalf("initial client counts = %+v, %v", counts, err)
	}
	client, found, err := store.BrowserClient(ctx, browserTestID(t, 1))
	if err != nil || !found {
		t.Fatalf("client lookup = %+v, %t, %v", client, found, err)
	}
	if _, err := store.RevokeBrowserClient(ctx, client.ID, client.Revision, mustTime(t, 101)); err != nil {
		t.Fatal(err)
	}
	counts, err = store.BrowserClientCounts(ctx, boot, mustTime(t, 101))
	if err != nil || counts.Active != BrowserClientPageSize+1 || counts.Revoked != 1 {
		t.Fatalf("revoked client counts = %+v, %v", counts, err)
	}
	pending := HashBrowserChallenge([]byte("pending-list-count"))
	if _, err := store.CreateBrowserPairingChallenge(ctx, pending, boot, "https://app.example", BrowserCapabilityObserve, mustTime(t, 102), mustTime(t, 202)); err != nil {
		t.Fatal(err)
	}
	counts, err = store.BrowserClientCounts(ctx, boot, mustTime(t, 102))
	if err != nil || counts.ActiveChallenges != 1 {
		t.Fatalf("pending challenge count = %+v, %v", counts, err)
	}
}

func TestBrowserClientCursorValidatesCompleteRowBeforePaging(t *testing.T) {
	variants := []struct {
		name   string
		mutate string
		value  any
	}{
		{name: "public key", mutate: `UPDATE browser_clients SET public_key = ?`, value: bytes.Repeat([]byte{0}, 65)},
		{name: "fingerprint", mutate: `UPDATE browser_clients SET fingerprint = ?`, value: bytes.Repeat([]byte{0}, DigestBytes)},
		{name: "capability", mutate: `UPDATE browser_clients SET capability_mask = ?`, value: int64(16)},
		{name: "revision", mutate: `UPDATE browser_clients SET revision = ?`, value: int64(0)},
		{name: "revocation", mutate: `UPDATE browser_clients SET revoked_at_ms = ?`, value: int64(0)},
	}
	for index, variant := range variants {
		t.Run(variant.name, func(t *testing.T) {
			store, _ := newBrowserStore(t)
			defer store.Close()
			boot := browserTestBoot(t, byte(210+index))
			cursorID := browserTestID(t, byte(210+index))
			cursorDigest := mintBrowserChallenge(t, store, byte(210+index), boot, 10, 100, BrowserCapabilityObserve)
			pairBrowserClient(t, store, cursorDigest, boot, cursorID, browserKey(t), 11)
			otherDigest := mintBrowserChallenge(t, store, byte(220+index), boot, 20, 100, BrowserCapabilityObserve)
			pairBrowserClient(t, store, otherDigest, boot, browserTestID(t, byte(220+index)), browserKey(t), 21)
			corruptSQL(t, store, variant.mutate+` WHERE id = ?`, variant.value, cursorID.Bytes())
			if _, err := store.ListBrowserClients(context.Background(), &cursorID); !errors.Is(err, ErrCorruptState) {
				t.Fatalf("corrupt cursor page error = %v", err)
			}
		})
	}
}

func sameBrowserID(left, right *BrowserClientID) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

func sameUnixMillis(left, right *UnixMillis) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

func assertReleaseRejectedWithoutMutation(t *testing.T, store *Store, runID RunID, sessionID TerminalSessionID, beforeRun Run, beforeSession TerminalSession, beforeFactory FactoryState) {
	t.Helper()
	afterRun, _, err := store.Run(context.Background(), runID)
	if err != nil {
		t.Fatal(err)
	}
	afterSession := terminalSessionForRunTest(t, store, runID)
	afterFactory, err := store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if afterRun.Phase != beforeRun.Phase || afterRun.Revision != beforeRun.Revision || afterRun.UpdatedAt != beforeRun.UpdatedAt {
		t.Fatalf("rejected release changed run lifecycle: before=%+v after=%+v", beforeRun, afterRun)
	}
	if afterSession.ID != beforeSession.ID || afterSession.RunID != beforeSession.RunID || afterSession.State != beforeSession.State || afterSession.UnresolvedReason != beforeSession.UnresolvedReason || afterSession.Revision != beforeSession.Revision || afterSession.DeclaredAt != beforeSession.DeclaredAt || !sameUnixMillis(afterSession.ActivatedAt, beforeSession.ActivatedAt) || !sameUnixMillis(afterSession.ClosedAt, beforeSession.ClosedAt) || afterSession.UpdatedAt != beforeSession.UpdatedAt || !sameBrowserID(afterSession.LeaseClientID, beforeSession.LeaseClientID) || afterSession.LeaseGeneration != beforeSession.LeaseGeneration || !sameUnixMillis(afterSession.LeaseExpiresAt, beforeSession.LeaseExpiresAt) || afterSession.LastInputSequence != beforeSession.LastInputSequence {
		t.Fatalf("rejected release changed session: before=%+v after=%+v", beforeSession, afterSession)
	}
	if afterFactory != beforeFactory {
		t.Fatalf("rejected release changed factory invalidation state: before=%+v after=%+v", beforeFactory, afterFactory)
	}
}

func TestBrowserDaemonIDFreshStableAndCorruptionFailsClosed(t *testing.T) {
	ctx := context.Background()
	store, path := newBrowserStore(t)
	first, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Equal(first.DaemonID.Bytes(), make([]byte, IDBytes)) {
		t.Fatal("fresh daemon ID is zero")
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	second, err := reopened.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if first.DaemonID != second.DaemonID {
		t.Fatalf("daemon ID changed across reopen: %x/%x", first.DaemonID.Bytes(), second.DaemonID.Bytes())
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}

	for _, bad := range [][]byte{make([]byte, IDBytes), make([]byte, IDBytes-1)} {
		t.Run(fmt.Sprintf("malformed-%d", len(bad)), func(t *testing.T) {
			store, path := newBrowserStore(t)
			corruptSQL(t, store, `UPDATE factory SET daemon_id = ? WHERE singleton = 1`, bad)
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			opened, err := Open(ctx, path)
			if opened != nil {
				opened.Close()
			}
			if !errors.Is(err, ErrCorruptState) {
				t.Fatalf("corrupt daemon ID Open error = %v", err)
			}
		})
	}
}

func TestBrowserFreshSchemaReopenAndTerminalLeaseForeignKey(t *testing.T) {
	ctx := context.Background()
	store, path := newBrowserStore(t)
	connection, err := store.readerConnection(ctx)
	if err != nil {
		t.Fatal(err)
	}
	rows, err := connection.QueryContext(ctx, `PRAGMA foreign_key_list('terminal_sessions')`)
	if err != nil {
		connection.Close()
		t.Fatal(err)
	}
	found := false
	for rows.Next() {
		var id, seq int
		var table, from, to, onUpdate, onDelete, match string
		if err := rows.Scan(&id, &seq, &table, &from, &to, &onUpdate, &onDelete, &match); err != nil {
			rows.Close()
			connection.Close()
			t.Fatal(err)
		}
		if table == "browser_clients" && from == "lease_client_id" && to == "id" {
			found = true
		}
	}
	if err := errors.Join(rows.Err(), rows.Close(), connection.Close()); err != nil {
		t.Fatal(err)
	}
	if !found {
		t.Fatal("terminal lease does not have browser_clients foreign key")
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestBrowserChallengeBoundsBootOriginExpiryAndPruning(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	boot := browserTestBoot(t, 1)
	invalid := []struct {
		name             string
		boot             BootID
		origin           string
		mask             BrowserCapabilityMask
		created, expires int64
	}{
		{"zero boot", BootID{}, "https://app.example", BrowserCapabilityObserve, 10, 20},
		{"empty origin", boot, "", BrowserCapabilityObserve, 10, 20},
		{"unknown capability", boot, "https://app.example", BrowserCapabilityObserve | 16, 10, 20},
		{"observe required", boot, "https://app.example", BrowserCapabilityTerminalInput, 10, 20},
		{"zero capability", boot, "https://app.example", 0, 10, 20},
		{"zero ttl", boot, "https://app.example", BrowserCapabilityObserve, 10, 10},
		{"ttl too long", boot, "https://app.example", BrowserCapabilityObserve, 10, 300011},
	}
	for index, test := range invalid {
		t.Run(test.name, func(t *testing.T) {
			digest := HashBrowserChallenge([]byte(fmt.Sprintf("invalid-%d", index)))
			if _, err := store.CreateBrowserPairingChallenge(ctx, digest, test.boot, test.origin, test.mask, mustTime(t, test.created), mustTime(t, test.expires)); !errors.Is(err, ErrInvalidValue) {
				t.Fatalf("invalid challenge error = %v", err)
			}
		})
	}
	longOrigin := bytes.Repeat([]byte{'o'}, 4097)
	if _, err := store.CreateBrowserPairingChallenge(ctx, HashBrowserChallenge([]byte("long-origin")), boot, string(longOrigin), BrowserCapabilityObserve, mustTime(t, 10), mustTime(t, 20)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("long origin error = %v", err)
	}

	digest := mintBrowserChallenge(t, store, 10, boot, 10, 20, BrowserCapabilityObserve)
	if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, browserTestBoot(t, 2), "https://app.example", browserTestID(t, 10), browserKey(t), mustTime(t, 11)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("wrong boot error = %v", err)
	}
	if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, boot, "https://wrong.example", browserTestID(t, 10), browserKey(t), mustTime(t, 11)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("wrong origin error = %v", err)
	}
	var redeemed sql.NullInt64
	if err := store.readers.QueryRow(`SELECT redeemed_at_ms FROM browser_pairing_challenges WHERE secret_digest = ?`, digest.Bytes()).Scan(&redeemed); err != nil {
		t.Fatal(err)
	}
	if redeemed.Valid {
		t.Fatal("wrong boot/origin consumed challenge")
	}
	if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, boot, "https://app.example", browserTestID(t, 10), browserKey(t), mustTime(t, 9)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("before-created error = %v", err)
	}
	if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, boot, "https://app.example", browserTestID(t, 10), browserKey(t), mustTime(t, 20)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("exact-expiry error = %v", err)
	}
	validKey := browserKey(t)
	client := pairBrowserClient(t, store, digest, boot, browserTestID(t, 10), validKey, 19)
	if client.CapabilityMask != BrowserCapabilityObserve {
		t.Fatalf("challenge capabilities widened: %v", client.CapabilityMask)
	}
	if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, boot, "https://app.example", browserTestID(t, 11), validKey, mustTime(t, 19)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("replay error = %v", err)
	}
	var rawCount int64
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_pairing_challenges WHERE secret_digest = ?`, []byte("raw challenge never enters Store")).Scan(&rawCount); err != nil {
		t.Fatal(err)
	}
	if rawCount != 0 {
		t.Fatal("raw challenge secret was stored")
	}
	if browserTableCount(t, store, "browser_pairing_challenges") != 1 {
		t.Fatalf("challenge count after redemption = %d", browserTableCount(t, store, "browser_pairing_challenges"))
	}

	old := mintBrowserChallenge(t, store, 11, browserTestBoot(t, 3), 30, 40, BrowserCapabilityObserve)
	newDigest := mintBrowserChallenge(t, store, 12, browserTestBoot(t, 4), 31, 41, BrowserCapabilityObserve)
	if _, err := store.RedeemBrowserPairingChallenge(ctx, old, browserTestBoot(t, 3), "https://app.example", browserTestID(t, 12), browserKey(t), mustTime(t, 32)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("old-boot challenge redemption = %v", err)
	}
	var oldCount int
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_pairing_challenges WHERE secret_digest = ?`, old.Bytes()).Scan(&oldCount); err != nil {
		t.Fatal(err)
	}
	if oldCount != 0 {
		t.Fatal("mint did not prune old-boot challenge")
	}
	if _, err := store.RedeemBrowserPairingChallenge(ctx, newDigest, browserTestBoot(t, 4), "https://app.example", browserTestID(t, 13), browserKey(t), mustTime(t, 32)); err != nil {
		t.Fatalf("current challenge redemption = %v", err)
	}

	for index := 0; index < 32; index++ {
		mintBrowserChallenge(t, store, byte(30+index), browserTestBoot(t, 5), int64(100+index), int64(200+index), BrowserCapabilityObserve)
	}
	if got := browserTableCount(t, store, "browser_pairing_challenges"); got != 32 {
		t.Fatalf("active challenge count = %d, want 32", got)
	}
	if _, err := store.CreateBrowserPairingChallenge(ctx, HashBrowserChallenge([]byte("thirty-three")), browserTestBoot(t, 5), "https://app.example", BrowserCapabilityObserve, mustTime(t, 140), mustTime(t, 240)); !errors.Is(err, ErrBusy) {
		t.Fatalf("active challenge bound error = %v", err)
	}
}

func TestBrowserChallengeRetentionBoundFailsClosedOnOpen(t *testing.T) {
	ctx := context.Background()
	store, path := newBrowserStore(t)
	boot := browserTestBoot(t, 6)
	for index := 0; index < 33; index++ {
		digest := HashBrowserChallenge([]byte(fmt.Sprintf("raw-retention-%d", index)))
		corruptSQL(t, store, `INSERT INTO browser_pairing_challenges(secret_digest, boot_id, intended_origin, capability_mask, created_at_ms, expires_at_ms) VALUES(?, ?, ?, ?, ?, ?)`, digest.Bytes(), boot.Bytes(), "https://app.example", int64(BrowserCapabilityObserve), index+1, index+2)
	}
	if got := browserTableCount(t, store, "browser_pairing_challenges"); got != 33 {
		t.Fatalf("raw challenge count = %d, want 33", got)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	opened, err := Open(ctx, path)
	if opened != nil {
		opened.Close()
	}
	if !errors.Is(err, ErrCorruptState) {
		t.Fatalf("33 challenges Open error = %v", err)
	}
}

func TestBrowserChallengeInsertErrorsRemainSpecific(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	boot := browserTestBoot(t, 7)
	digest := mintBrowserChallenge(t, store, 7, boot, 10, 100, BrowserCapabilityObserve)
	if _, err := store.CreateBrowserPairingChallenge(ctx, digest, boot, "https://app.example", BrowserCapabilityObserve, mustTime(t, 11), mustTime(t, 100)); !errors.Is(err, ErrConflict) {
		t.Fatalf("duplicate digest error = %v", err)
	}
	if _, err := store.writer.Exec(`CREATE TRIGGER reject_browser_challenge_insert BEFORE INSERT ON browser_pairing_challenges BEGIN SELECT RAISE(ABORT, 'unexpected challenge insert'); END`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.CreateBrowserPairingChallenge(ctx, HashBrowserChallenge([]byte("unexpected insert")), boot, "https://app.example", BrowserCapabilityObserve, mustTime(t, 12), mustTime(t, 100)); err == nil || errors.Is(err, ErrConflict) {
		t.Fatalf("unexpected insert error collapsed: %v", err)
	}
	if got := browserTableCount(t, store, "browser_pairing_challenges"); got != 1 {
		t.Fatalf("unexpected insert changed challenge count = %d", got)
	}
}

func TestBrowserRestartPreservesClientAndChallengeUntilBootChanges(t *testing.T) {
	ctx := context.Background()
	store, path := newBrowserStore(t)
	boot := browserTestBoot(t, 115)
	clientID := browserTestID(t, 115)
	key := browserKey(t)
	clientDigest := mintBrowserChallenge(t, store, 115, boot, 10, 100, BrowserCapabilityObserve)
	client := pairBrowserClient(t, store, clientDigest, boot, clientID, key, 11)
	pendingDigest := mintBrowserChallenge(t, store, 116, boot, 20, 100, BrowserCapabilityObserve)
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	loaded, found, err := reopened.BrowserClient(ctx, clientID)
	if err != nil || !found || loaded.Fingerprint != client.Fingerprint {
		t.Fatalf("reopened client = %+v, found=%v, err=%v", loaded, found, err)
	}
	if _, err := reopened.RedeemBrowserPairingChallenge(ctx, pendingDigest, browserTestBoot(t, 116), "https://app.example", browserTestID(t, 116), browserKey(t), mustTime(t, 21)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("old boot redemption after restart = %v", err)
	}
	if _, err := reopened.RedeemBrowserPairingChallenge(ctx, pendingDigest, boot, "https://app.example", browserTestID(t, 116), browserKey(t), mustTime(t, 21)); err != nil {
		t.Fatalf("pending challenge after restart = %v", err)
	}
}

func TestBrowserConcurrentRedemptionAndDuplicateFingerprintConsumes(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	boot := browserTestBoot(t, 20)
	digest := mintBrowserChallenge(t, store, 20, boot, 10, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput)
	key := browserKey(t)
	var wait sync.WaitGroup
	errs := make(chan error, 2)
	wait.Add(2)
	for index := byte(0); index < 2; index++ {
		go func(index byte) {
			defer wait.Done()
			_, err := store.RedeemBrowserPairingChallenge(ctx, digest, boot, "https://app.example", browserTestID(t, 30+index), key, mustTime(t, 11))
			errs <- err
		}(index)
	}
	wait.Wait()
	close(errs)
	winners := 0
	for err := range errs {
		if err == nil {
			winners++
		} else if !errors.Is(err, ErrUnauthorized) && !errors.Is(err, ErrConflict) {
			t.Fatalf("concurrent redemption error = %v", err)
		}
	}
	if winners != 1 || browserTableCount(t, store, "browser_clients") != 1 {
		t.Fatalf("concurrent redemption winners=%d clients=%d", winners, browserTableCount(t, store, "browser_clients"))
	}

	clientID := browserTestID(t, 50)
	first := browserKey(t)
	firstDigest := mintBrowserChallenge(t, store, 21, boot, 110, 200, BrowserCapabilityObserve)
	client := pairBrowserClient(t, store, firstDigest, boot, clientID, first, 111)
	if _, err := store.RevokeBrowserClient(ctx, clientID, client.Revision, mustTime(t, 112)); err != nil {
		t.Fatal(err)
	}
	duplicateDigest := mintBrowserChallenge(t, store, 22, boot, 120, 200, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	if _, err := store.RedeemBrowserPairingChallenge(ctx, duplicateDigest, boot, "https://app.example", browserTestID(t, 51), first, mustTime(t, 121)); !errors.Is(err, ErrConflict) {
		t.Fatalf("revoked duplicate fingerprint error = %v", err)
	}
	var consumed sql.NullInt64
	if err := store.readers.QueryRow(`SELECT redeemed_at_ms FROM browser_pairing_challenges WHERE secret_digest = ?`, duplicateDigest.Bytes()).Scan(&consumed); err != nil {
		t.Fatal(err)
	}
	if !consumed.Valid {
		t.Fatal("duplicate fingerprint did not consume challenge")
	}
	var duplicates int
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_security_events WHERE kind = 'duplicate_fingerprint' AND client_id = ?`, clientID.Bytes()).Scan(&duplicates); err != nil {
		t.Fatal(err)
	}
	if duplicates != 1 {
		t.Fatalf("duplicate fingerprint events = %d", duplicates)
	}
}

func TestBrowserKeyValidationAndClientIDCollisionRollback(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	boot := browserTestBoot(t, 60)
	digest := mintBrowserChallenge(t, store, 60, boot, 10, 100, BrowserCapabilityObserve)
	valid := browserKey(t)
	x, y := mustECDSAPoint(t, valid)
	compressedKey := elliptic.MarshalCompressed(elliptic.P256(), x, y)
	wrongPrefix := append([]byte(nil), valid...)
	wrongPrefix[0] = 2
	offCurve := make([]byte, 65)
	offCurve[0] = 4
	offCurve[32] = 1
	offCurve[64] = 1
	malformed := [][]byte{valid[:64], compressedKey, wrongPrefix, offCurve}
	for index, key := range malformed {
		if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, boot, "https://app.example", browserTestID(t, byte(61+index)), key, mustTime(t, 11)); !errors.Is(err, ErrUnauthorized) {
			t.Fatalf("malformed key %d error = %v", index, err)
		}
	}
	client := pairBrowserClient(t, store, digest, boot, browserTestID(t, 70), valid, 12)
	wantFingerprint, err := validateBrowserKey(valid)
	if err != nil || client.Fingerprint != wantFingerprint {
		t.Fatalf("Store-derived fingerprint = %x want %x (err=%v)", client.Fingerprint, wantFingerprint, err)
	}

	collisionDigest := mintBrowserChallenge(t, store, 71, boot, 20, 100, BrowserCapabilityObserve)
	if _, err := store.RedeemBrowserPairingChallenge(ctx, collisionDigest, boot, "https://app.example", client.ID, browserKey(t), mustTime(t, 21)); err == nil {
		t.Fatal("client-ID collision succeeded")
	}
	var redeemed sql.NullInt64
	if err := store.readers.QueryRow(`SELECT redeemed_at_ms FROM browser_pairing_challenges WHERE secret_digest = ?`, collisionDigest.Bytes()).Scan(&redeemed); err != nil {
		t.Fatal(err)
	}
	if redeemed.Valid {
		t.Fatal("client-ID collision consumed challenge")
	}
}

// mustECDSAPoint decodes a valid SEC1 point so malformed-key tests remain
// independent of the random key's private scalar.
func mustECDSAPoint(t *testing.T, publicKey []byte) (*big.Int, *big.Int) {
	t.Helper()
	x, y := elliptic.Unmarshal(elliptic.P256(), publicKey)
	if x == nil || y == nil {
		t.Fatal("test key did not decode")
	}
	return x, y
}

func TestBrowserSecurityEventsArePrivateBoundedAndGapSafe(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	digest := mintBrowserChallenge(t, store, 80, browserTestBoot(t, 80), 10, 100, BrowserCapabilityObserve)
	secret := []byte("browser raw challenge private sentinel")
	key := browserKey(t)
	if _, err := store.RedeemBrowserPairingChallenge(ctx, digest, browserTestBoot(t, 80), "https://app.example", browserTestID(t, 80), key, mustTime(t, 11)); err != nil {
		t.Fatal(err)
	}
	var rawDigestMatches, secretEvents int
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_pairing_challenges WHERE secret_digest = ?`, secret).Scan(&rawDigestMatches); err != nil {
		t.Fatal(err)
	}
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_security_events WHERE kind NOT IN ('challenge_minted', 'challenge_abandoned', 'client_paired', 'duplicate_fingerprint', 'client_revoked')`).Scan(&secretEvents); err != nil {
		t.Fatal(err)
	}
	if rawDigestMatches != 0 || secretEvents != 0 {
		t.Fatalf("private event payload leaked: raw=%d invalid=%d", rawDigestMatches, secretEvents)
	}
	var eventKeyMatches int
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_security_events WHERE client_id = ?`, key).Scan(&eventKeyMatches); err != nil {
		t.Fatal(err)
	}
	if eventKeyMatches != 0 {
		t.Fatal("browser security events stored public key bytes")
	}

	connection, err := store.writer.Conn(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := connection.ExecContext(ctx, `DELETE FROM browser_security_events`); err != nil {
		connection.Close()
		t.Fatal(err)
	}
	for index := 0; index < EventRetentionLimit+1; index++ {
		if _, err := connection.ExecContext(ctx, `INSERT INTO browser_security_events(kind, client_id, occurred_at_ms) VALUES('challenge_minted', NULL, ?)`, int64(index+1)); err != nil {
			connection.Close()
			t.Fatal(err)
		}
	}
	if err := insertBrowserSecurityEvent(ctx, connection, BrowserSecurityChallengeMinted, nil, mustTime(t, 5000)); err != nil {
		connection.Close()
		t.Fatal(err)
	}
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}
	if got := browserTableCount(t, store, "browser_security_events"); got != EventRetentionLimit {
		t.Fatalf("security event retention = %d, want %d", got, EventRetentionLimit)
	}
	var minimum, maximum int64
	if err := store.readers.QueryRow(`SELECT MIN(sequence), MAX(sequence) FROM browser_security_events`).Scan(&minimum, &maximum); err != nil {
		t.Fatal(err)
	}
	if minimum != 5 || maximum != int64(EventRetentionLimit+4) {
		t.Fatalf("gap-safe retained sequence = %d..%d", minimum, maximum)
	}
}

func TestBrowserPairingSecurityEventFailureAndSequenceOverflowRollBack(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	if _, err := store.writer.Exec(`CREATE TRIGGER suppress_browser_security_insert BEFORE INSERT ON browser_security_events BEGIN SELECT RAISE(IGNORE); END`); err != nil {
		t.Fatal(err)
	}
	beforeChallenges := browserTableCount(t, store, "browser_pairing_challenges")
	beforeEvents := browserTableCount(t, store, "browser_security_events")
	if _, err := store.CreateBrowserPairingChallenge(ctx, HashBrowserChallenge([]byte("suppressed event")), browserTestBoot(t, 81), "https://app.example", BrowserCapabilityObserve, mustTime(t, 10), mustTime(t, 20)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("suppressed security event error = %v", err)
	}
	if browserTableCount(t, store, "browser_pairing_challenges") != beforeChallenges || browserTableCount(t, store, "browser_security_events") != beforeEvents {
		t.Fatal("suppressed event left pairing transaction footprint")
	}
	if _, err := store.writer.Exec(`DROP TRIGGER suppress_browser_security_insert`); err != nil {
		t.Fatal(err)
	}
	mintBrowserChallenge(t, store, 81, browserTestBoot(t, 81), 10, 20, BrowserCapabilityObserve)

	if _, err := store.writer.Exec(`UPDATE sqlite_sequence SET seq = ? WHERE name = 'browser_security_events'`, int64(^uint64(0)>>1)); err != nil {
		t.Fatal(err)
	}
	var sequenceBefore int64
	if err := store.readers.QueryRow(`SELECT seq FROM sqlite_sequence WHERE name = 'browser_security_events'`).Scan(&sequenceBefore); err != nil {
		t.Fatal(err)
	}
	if sequenceBefore != int64(^uint64(0)>>1) {
		t.Fatalf("sequence before overflow = %d", sequenceBefore)
	}
	beforeChallenges = browserTableCount(t, store, "browser_pairing_challenges")
	if _, err := store.CreateBrowserPairingChallenge(ctx, HashBrowserChallenge([]byte("overflow event")), browserTestBoot(t, 82), "https://app.example", BrowserCapabilityObserve, mustTime(t, 20), mustTime(t, 30)); err == nil {
		t.Fatal("security sequence overflow succeeded")
	}
	if browserTableCount(t, store, "browser_pairing_challenges") != beforeChallenges {
		t.Fatal("security sequence overflow left challenge")
	}
}

func TestBrowserRevokeExpectedRevisionAndNoDuplicateEvent(t *testing.T) {
	ctx := context.Background()
	store, _ := newBrowserStore(t)
	defer store.Close()
	id := browserTestID(t, 90)
	boot := browserTestBoot(t, 90)
	digest := mintBrowserChallenge(t, store, 90, boot, 10, 100, BrowserCapabilityObserve)
	client := pairBrowserClient(t, store, digest, boot, id, browserKey(t), 11)
	if _, err := store.RevokeBrowserClient(ctx, id, Revision{value: client.Revision.Int64() + 1}, mustTime(t, 12)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale revoke error = %v", err)
	}
	current, found, err := store.BrowserClient(ctx, id)
	if err != nil || !found || current.RevokedAt != nil || current.Revision != client.Revision {
		t.Fatalf("stale revoke changed client = %+v, err=%v", current, err)
	}
	revoked, err := store.RevokeBrowserClient(ctx, id, client.Revision, mustTime(t, 12))
	if err != nil {
		t.Fatal(err)
	}
	if revoked.Revision.Int64() != client.Revision.Int64()+1 || revoked.RevokedAt == nil {
		t.Fatalf("revoked client = %+v", revoked)
	}
	if _, err := store.RevokeBrowserClient(ctx, id, revoked.Revision, mustTime(t, 13)); err != nil {
		t.Fatalf("current-revision idempotent revoke = %v", err)
	}
	if _, err := store.RevokeBrowserClient(ctx, id, client.Revision, mustTime(t, 13)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("old-revision replay error = %v", err)
	}
	var events int
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_security_events WHERE kind = 'client_revoked' AND client_id = ?`, id.Bytes()).Scan(&events); err != nil {
		t.Fatal(err)
	}
	if events != 1 {
		t.Fatalf("client revoked events = %d", events)
	}
}

func TestBrowserAuthorityRawCorruptionFailsClosed(t *testing.T) {
	tests := []struct {
		name   string
		mutate func(*testing.T, *Store)
	}{
		{
			name: "event challenge kind requires null client",
			mutate: func(t *testing.T, store *Store) {
				boot := browserTestBoot(t, 100)
				digest := mintBrowserChallenge(t, store, 100, boot, 10, 100, BrowserCapabilityObserve)
				id := browserTestID(t, 100)
				pairBrowserClient(t, store, digest, boot, id, browserKey(t), 11)
				corruptSQL(t, store, `UPDATE browser_security_events SET kind = 'client_paired', client_id = NULL WHERE sequence = (SELECT MIN(sequence) FROM browser_security_events)`)
			},
		},
		{
			name: "event client kind requires nonnull client",
			mutate: func(t *testing.T, store *Store) {
				boot := browserTestBoot(t, 101)
				digest := mintBrowserChallenge(t, store, 101, boot, 10, 100, BrowserCapabilityObserve)
				id := browserTestID(t, 101)
				pairBrowserClient(t, store, digest, boot, id, browserKey(t), 11)
				corruptSQL(t, store, `UPDATE browser_security_events SET client_id = NULL WHERE kind = 'client_paired'`)
			},
		},
		{
			name: "unknown client mask",
			mutate: func(t *testing.T, store *Store) {
				boot := browserTestBoot(t, 103)
				digest := mintBrowserChallenge(t, store, 103, boot, 10, 100, BrowserCapabilityObserve)
				id := browserTestID(t, 103)
				pairBrowserClient(t, store, digest, boot, id, browserKey(t), 11)
				corruptSQL(t, store, `UPDATE browser_clients SET capability_mask = 17`)
				if _, _, err := store.BrowserClient(context.Background(), id); !errors.Is(err, ErrCorruptState) {
					t.Fatalf("corrupt mask read error = %v", err)
				}
			},
		},
		{
			name: "redeemed after expiry",
			mutate: func(t *testing.T, store *Store) {
				boot := browserTestBoot(t, 102)
				digest := mintBrowserChallenge(t, store, 102, boot, 10, 100, BrowserCapabilityObserve)
				corruptSQL(t, store, `UPDATE browser_pairing_challenges SET redeemed_at_ms = 101 WHERE secret_digest = ?`, digest.Bytes())
			},
		},
		{
			name: "redeemed at expiry",
			mutate: func(t *testing.T, store *Store) {
				boot := browserTestBoot(t, 104)
				digest := mintBrowserChallenge(t, store, 104, boot, 10, 100, BrowserCapabilityObserve)
				corruptSQL(t, store, `UPDATE browser_pairing_challenges SET redeemed_at_ms = 100 WHERE secret_digest = ?`, digest.Bytes())
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, path := newBrowserStore(t)
			test.mutate(t, store)
			read, err := store.beginRead(context.Background())
			if err != nil {
				t.Fatal(err)
			}
			validationErr := validateBrowserAuthority(context.Background(), read.connection)
			if closeErr := read.Close(); closeErr != nil {
				t.Fatal(closeErr)
			}
			if !errors.Is(validationErr, ErrCorruptState) {
				t.Fatalf("corrupt %s direct validation error = %v", test.name, validationErr)
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			opened, err := Open(context.Background(), path)
			if opened != nil {
				opened.Close()
			}
			if !errors.Is(err, ErrCorruptState) {
				t.Fatalf("corrupt %s Open error = %v", test.name, err)
			}
		})
	}
}

func TestTerminalSessionReadsValidateLeaseClientRelationship(t *testing.T) {
	tests := []struct {
		name   string
		mutate func(*testing.T, *Store, BrowserClient)
	}{
		{
			name: "missing client",
			mutate: func(t *testing.T, store *Store, _ BrowserClient) {
				corruptSQL(t, store, `UPDATE terminal_sessions SET lease_client_id = ?`, browserTestID(t, 250).Bytes())
			},
		},
		{
			name: "revoked client",
			mutate: func(t *testing.T, store *Store, client BrowserClient) {
				corruptSQL(t, store, `UPDATE browser_clients SET revoked_at_ms = updated_at_ms WHERE id = ?`, client.ID.Bytes())
			},
		},
		{
			name: "client without terminal capability",
			mutate: func(t *testing.T, store *Store, client BrowserClient) {
				corruptSQL(t, store, `UPDATE browser_clients SET capability_mask = ? WHERE id = ?`, int64(BrowserCapabilityObserve), client.ID.Bytes())
			},
		},
	}
	for index, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, run, _ := runningOrchestratorRun(t)
			defer store.Close()
			session := terminalSessionForRunTest(t, store, run.ID)
			boot := browserTestBoot(t, byte(220+index))
			client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, byte(220+index), boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, byte(220+index)), browserKey(t), 31)
			if _, err := store.AcquireTerminalLease(context.Background(), run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31)); err != nil {
				t.Fatal(err)
			}
			if _, found, err := store.TerminalSession(context.Background(), session.ID); err != nil || !found {
				t.Fatalf("normal TerminalSession read = found %v, err %v", found, err)
			}
			if _, found, err := store.TerminalSessionForRun(context.Background(), run.ID); err != nil || !found {
				t.Fatalf("normal TerminalSessionForRun read = found %v, err %v", found, err)
			}
			test.mutate(t, store, client)
			if _, found, err := store.TerminalSession(context.Background(), session.ID); !errors.Is(err, ErrCorruptState) || found {
				t.Fatalf("corrupt TerminalSession read = found %v, err %v", found, err)
			}
			if _, found, err := store.TerminalSessionForRun(context.Background(), run.ID); !errors.Is(err, ErrCorruptState) || found {
				t.Fatalf("corrupt TerminalSessionForRun read = found %v, err %v", found, err)
			}
		})
	}
}

func addRunningRunOnStore(t *testing.T, store *Store, seed byte) Run {
	t.Helper()
	project, err := store.CreateProject(context.Background(), NewProject{ID: projectID(t, seed), Name: fmt.Sprintf("browser-project-%d", seed), Root: fmt.Sprintf("/browser-project-%d", seed)}, mustTime(t, 100+int64(seed)))
	if err != nil {
		t.Fatalf("create project: %v", err)
	}
	agent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, seed+1), ProjectID: project.ID, Name: fmt.Sprintf("browser-agent-%d", seed), Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 101+int64(seed)))
	if err != nil {
		t.Fatalf("create agent: %v", err)
	}
	task, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, seed+2), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, seed+3), Title: fmt.Sprintf("browser-run-%d", seed)}, mustTime(t, 102+int64(seed)))
	if err != nil {
		t.Fatalf("enqueue task: %v", err)
	}
	keys := admissionKeys(t, seed+4, nil)
	if keys.RunID == (RunID{}) {
		t.Fatal("zero run fixture")
	}
	result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 200+int64(seed)))
	if err != nil || !result.Admitted() || result.Run == nil {
		t.Fatalf("admit second run = %+v, %v", result, err)
	}
	if result.Run.TaskID != task.ID {
		t.Fatalf("admitted wrong task = %+v", result.Run)
	}
	activatedRun := activateAllResourcesUnique(t, store, *result.Run, 300+int64(seed), int64(seed)*10)
	session := terminalSessionForRunTest(t, store, result.Run.ID)
	running, err := store.ActivateRun(context.Background(), result.Run.ID, session.ID, activatedRun.Revision, session.Revision, mustTime(t, 310+int64(seed)))
	if err != nil {
		t.Fatal(err)
	}
	return running
}

func activateAllResourcesUnique(t *testing.T, store *Store, run Run, at, identitySeed int64) Run {
	t.Helper()
	resources := resourcesForRunTest(t, store, run.ID)
	runtime := resourceOfKind(t, resources, ResourceRuntimeRoot)
	runtimeIdentity, err := NewPathResourceIdentity(10, 10000+identitySeed)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, at)); err != nil {
		t.Fatal(err)
	}
	runner := resourceOfKind(t, resources, ResourceRunnerProcess)
	startedRun, startingRunner, err := store.BeginRunnerStart(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, mustTime(t, at+1))
	if err != nil {
		t.Fatal(err)
	}
	activeRun, _, err := store.ActivateRunner(context.Background(), run.ID, runner.ID, startedRun.Revision, startingRunner.Revision, processIdentity(t, identitySeed+12), mustTime(t, at+2))
	if err != nil {
		t.Fatal(err)
	}
	provider := resourceOfKind(t, resources, ResourceProviderProcess)
	group := resourceOfKind(t, resources, ResourceProviderGroup)
	if _, _, err := store.ActivateProviderResources(context.Background(), run.ID, provider.ID, provider.Revision, group.ID, group.Revision, processIdentity(t, identitySeed+1), mustTime(t, at+3)); err != nil {
		t.Fatal(err)
	}
	return activeRun
}

func TestTerminalLeaseCapabilityCompetitionExpiryAndRenewal(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 120)
	readOnly := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 120, boot, 30, 100, BrowserCapabilityObserve), boot, browserTestID(t, 120), browserKey(t), 31)
	writerKey := browserKey(t)
	writer := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 121, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 121), writerKey, 31)
	secondWriter := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 122, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 122), browserKey(t), 31)
	if _, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, readOnly.ID, run.Revision, session.Revision, mustTime(t, 31)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("read-only acquire error = %v", err)
	}
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, writer.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	if lease.Generation != 1 || lease.ExpiresAt.Int64() != 31+BrowserTerminalLeaseTTL {
		t.Fatalf("first lease = %+v", lease)
	}
	if _, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, secondWriter.ID, run.Revision, session.Revision, mustTime(t, 31)); !errors.Is(err, ErrConflict) {
		t.Fatalf("second writer while live error = %v", err)
	}
	replaced, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, secondWriter.ID, run.Revision, session.Revision, mustTime(t, 31+BrowserTerminalLeaseTTL))
	if err != nil {
		t.Fatal(err)
	}
	if replaced.Generation != 2 || replaced.LastInputSequence != 0 {
		t.Fatalf("expired replacement = %+v", replaced)
	}
	if _, err := store.RenewTerminalLease(ctx, run.ID, session.ID, secondWriter.ID, replaced.Generation, run.Revision, session.Revision, mustTime(t, 29)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("backward chronology renewal error = %v", err)
	}
	if _, err := store.RenewTerminalLease(ctx, run.ID, session.ID, secondWriter.ID, replaced.Generation, run.Revision, session.Revision, mustTime(t, 31)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("expiry regression renewal error = %v", err)
	}
	renewAt := replaced.ExpiresAt.Int64() - 1
	renewed, err := store.RenewTerminalLease(ctx, run.ID, session.ID, secondWriter.ID, replaced.Generation, run.Revision, session.Revision, mustTime(t, renewAt))
	if err != nil || renewed.ExpiresAt.Int64() != renewAt+BrowserTerminalLeaseTTL {
		t.Fatalf("renewal = %+v, %v", renewed, err)
	}
	if _, err := store.RenewTerminalLease(ctx, run.ID, session.ID, secondWriter.ID, replaced.Generation, run.Revision, session.Revision, mustTime(t, renewed.ExpiresAt.Int64())); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("exact expiry renewal error = %v", err)
	}
	afterRun, _, err := store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	afterSession := terminalSessionForRunTest(t, store, run.ID)
	if afterRun.Revision != run.Revision || afterRun.UpdatedAt != run.UpdatedAt || afterSession.Revision != session.Revision || afterSession.UpdatedAt != session.UpdatedAt {
		t.Fatalf("lease chronology drift: run=%+v session=%+v", afterRun, afterSession)
	}
}

func TestTerminalLeaseConcurrentAcquireHasOneDurableWinner(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 125)
	first := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 125, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 125), browserKey(t), 31)
	second := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 126, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 126), browserKey(t), 31)
	results := make(chan error, 2)
	var wait sync.WaitGroup
	wait.Add(2)
	for _, client := range []BrowserClient{first, second} {
		go func(client BrowserClient) {
			defer wait.Done()
			_, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31))
			results <- err
		}(client)
	}
	wait.Wait()
	close(results)
	winners := 0
	for err := range results {
		if err == nil {
			winners++
		} else if !errors.Is(err, ErrConflict) && !errors.Is(err, ErrUnauthorized) {
			t.Fatalf("concurrent acquire error = %v", err)
		}
	}
	if winners != 1 {
		t.Fatalf("concurrent acquire winners = %d", winners)
	}
	acquired := terminalSessionForRunTest(t, store, run.ID)
	if acquired.LeaseClientID == nil || acquired.LeaseGeneration != 1 || acquired.LastInputSequence != 0 {
		t.Fatalf("durable concurrent winner state = %+v", acquired)
	}
}

func TestCheckTerminalLeaseIsExactReadOnlyAuthorization(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 140)
	client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 140, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 140), browserKey(t), 31)
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, client.ID, lease.Generation, 1, run.Revision, session.Revision, mustTime(t, 32)); err != nil {
		t.Fatal(err)
	}
	beforeRun, _, err := store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	beforeSession := terminalSessionForRunTest(t, store, run.ID)
	beforeFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	checked, err := store.CheckTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, 33))
	if err != nil || checked.Generation != lease.Generation || checked.LastInputSequence != 1 || checked.ExpiresAt != lease.ExpiresAt {
		t.Fatalf("checked lease = %+v, err=%v", checked, err)
	}
	afterRun, _, err := store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	afterSession := terminalSessionForRunTest(t, store, run.ID)
	afterFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if afterRun.ID != beforeRun.ID || afterRun.Phase != beforeRun.Phase || afterRun.Revision != beforeRun.Revision || afterRun.UpdatedAt != beforeRun.UpdatedAt || !sameUnixMillis(afterRun.RunningAt, beforeRun.RunningAt) || afterSession.ID != beforeSession.ID || afterSession.RunID != beforeSession.RunID || afterSession.State != beforeSession.State || afterSession.Revision != beforeSession.Revision || afterSession.UpdatedAt != beforeSession.UpdatedAt || !sameBrowserID(afterSession.LeaseClientID, beforeSession.LeaseClientID) || afterSession.LeaseGeneration != beforeSession.LeaseGeneration || !sameUnixMillis(afterSession.LeaseExpiresAt, beforeSession.LeaseExpiresAt) || afterSession.LastInputSequence != beforeSession.LastInputSequence || afterFactory != beforeFactory {
		t.Fatalf("check mutated durable state: run before=%+v after=%+v session before=%+v after=%+v factory before=%+v after=%+v", beforeRun, afterRun, beforeSession, afterSession, beforeFactory, afterFactory)
	}

	for name, check := range map[string]func() error{
		"wrong client": func() error {
			_, err := store.CheckTerminalLease(ctx, run.ID, session.ID, browserTestID(t, 141), lease.Generation, run.Revision, session.Revision, mustTime(t, 33))
			return err
		},
		"wrong generation": func() error {
			_, err := store.CheckTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation+1, run.Revision, session.Revision, mustTime(t, 33))
			return err
		},
		"wrong run revision": func() error {
			_, err := store.CheckTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, Revision{value: run.Revision.Int64() + 1}, session.Revision, mustTime(t, 33))
			return err
		},
		"expired": func() error {
			_, err := store.CheckTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, lease.ExpiresAt.Int64()))
			return err
		},
	} {
		t.Run(name, func(t *testing.T) {
			if err := check(); err == nil {
				t.Fatal("invalid lease check succeeded")
			}
		})
	}
}

func TestRevokeTerminalLeaseAllowsExpiredExactGenerationAndCannotTouchReplacement(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 145)
	client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 145, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 145), browserKey(t), 31)
	otherClient := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 146, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 146), browserKey(t), 31)
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, client.ID, lease.Generation, 1, run.Revision, session.Revision, mustTime(t, 32)); err != nil {
		t.Fatal(err)
	}
	beforeFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	revoked, err := store.RevokeTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, lease.ExpiresAt.Int64()))
	if err != nil || revoked.Generation != lease.Generation+1 {
		t.Fatalf("expired revoke = %+v, err=%v", revoked, err)
	}
	after := terminalSessionForRunTest(t, store, run.ID)
	if after.LeaseClientID != nil || after.LeaseExpiresAt != nil || after.LastInputSequence != 0 || after.LeaseGeneration != lease.Generation+1 {
		t.Fatalf("expired revoke state = %+v", after)
	}
	afterFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if afterFactory != beforeFactory {
		t.Fatalf("expired revoke emitted lifecycle change: before=%+v after=%+v", beforeFactory, afterFactory)
	}
	if _, err := store.RevokeTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, lease.ExpiresAt.Int64()+1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("same-client revoke replay succeeded: %v", err)
	}
	if _, err := store.RevokeTerminalLease(ctx, run.ID, session.ID, otherClient.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, lease.ExpiresAt.Int64()+1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("different-client revoke replay succeeded: %v", err)
	}

	replacement, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 200))
	if err != nil || replacement.Generation != lease.Generation+2 {
		t.Fatalf("replacement lease = %+v, err=%v", replacement, err)
	}
	if _, err := store.RevokeTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, 201)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("old generation revoked replacement: %v", err)
	}
	current := terminalSessionForRunTest(t, store, run.ID)
	if current.LeaseClientID == nil || *current.LeaseClientID != client.ID || current.LeaseGeneration != replacement.Generation {
		t.Fatalf("replacement lease was changed by stale revoke = %+v", current)
	}
}

func TestRevokeTerminalLeaseGuardedUpdateRollsBack(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 150)
	client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 150, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 150), browserKey(t), 31)
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`CREATE TRIGGER suppress_terminal_system_revoke BEFORE UPDATE OF lease_client_id ON terminal_sessions BEGIN SELECT RAISE(IGNORE); END`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.RevokeTerminalLease(ctx, run.ID, session.ID, client.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, lease.ExpiresAt.Int64()+1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("suppressed system revoke error = %v", err)
	}
	if _, err := store.writer.Exec(`DROP TRIGGER suppress_terminal_system_revoke`); err != nil {
		t.Fatal(err)
	}
	unchanged := terminalSessionForRunTest(t, store, run.ID)
	if unchanged.LeaseClientID == nil || *unchanged.LeaseClientID != client.ID || unchanged.LeaseGeneration != lease.Generation || unchanged.LastInputSequence != 0 {
		t.Fatalf("suppressed system revoke changed state = %+v", unchanged)
	}
}

func TestTerminalLeaseSuppressedUpdateRollsBackAuthority(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 127)
	client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 127, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 127), browserKey(t), 31)
	if _, err := store.writer.Exec(`CREATE TRIGGER suppress_terminal_lease_update BEFORE UPDATE OF lease_client_id ON terminal_sessions BEGIN SELECT RAISE(IGNORE); END`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("suppressed lease update error = %v", err)
	}
	if _, err := store.writer.Exec(`DROP TRIGGER suppress_terminal_lease_update`); err != nil {
		t.Fatal(err)
	}
	after := terminalSessionForRunTest(t, store, run.ID)
	if after.LeaseClientID != nil || after.LeaseGeneration != 0 || after.LastInputSequence != 0 || after.Revision != session.Revision || after.UpdatedAt != session.UpdatedAt {
		t.Fatalf("suppressed lease update left state = %+v", after)
	}
}

func TestTerminalLeaseSequenceReleasePartialResetAndFreshGeneration(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 130)
	client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 130, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 130), browserKey(t), 31)
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, client.ID, lease.Generation, 2, run.Revision, session.Revision, mustTime(t, 32)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("skipped sequence error = %v", err)
	}
	reserved, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, client.ID, lease.Generation, 1, run.Revision, session.Revision, mustTime(t, 32))
	if err != nil || reserved.Sequence != 1 {
		t.Fatalf("first reservation = %+v, %v", reserved, err)
	}
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, client.ID, lease.Generation, 3, run.Revision, session.Revision, mustTime(t, 32)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("sequence gap error = %v", err)
	}
	if err := store.RevokeTerminalInputReservation(ctx, run.ID, session.ID, client.ID, lease.Generation, 0, run.Revision, session.Revision, mustTime(t, 33)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("zero partial sequence error = %v", err)
	}
	if err := store.RevokeTerminalInputReservation(ctx, run.ID, session.ID, client.ID, lease.Generation, 1, run.Revision, session.Revision, mustTime(t, 33)); err != nil {
		t.Fatal(err)
	}
	afterPartial := terminalSessionForRunTest(t, store, run.ID)
	if afterPartial.LeaseClientID != nil || afterPartial.LeaseGeneration != lease.Generation+1 || afterPartial.LastInputSequence != 0 {
		t.Fatalf("partial write did not revoke lease = %+v", afterPartial)
	}
	afterRun, _, err := store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	if afterRun.Phase != RunRunning || afterRun.Revision != run.Revision || afterPartial.Revision != session.Revision || afterPartial.UpdatedAt != session.UpdatedAt {
		t.Fatalf("partial write changed lifecycle = run=%+v session=%+v", afterRun, afterPartial)
	}
	if err := store.RevokeTerminalInputReservation(ctx, run.ID, session.ID, client.ID, lease.Generation, 1, run.Revision, session.Revision, mustTime(t, 33)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale partial replay error = %v", err)
	}
	newLease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 40))
	if err != nil || newLease.Generation != lease.Generation+2 {
		t.Fatalf("fresh generation lease = %+v, %v", newLease, err)
	}
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, client.ID, newLease.Generation, 1, run.Revision, session.Revision, mustTime(t, 40)); err != nil {
		t.Fatal(err)
	}
	beforeResetFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := store.ResetTerminalLeases(ctx); err != nil {
		t.Fatal(err)
	}
	afterReset := terminalSessionForRunTest(t, store, run.ID)
	if afterReset.LeaseClientID != nil || afterReset.LeaseExpiresAt != nil || afterReset.LastInputSequence != 0 || afterReset.LeaseGeneration != newLease.Generation+1 || afterReset.Revision != session.Revision || afterReset.UpdatedAt != session.UpdatedAt {
		t.Fatalf("reset lease state = %+v", afterReset)
	}
	afterResetFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if afterResetFactory.Head != beforeResetFactory.Head || afterResetFactory.Revision != beforeResetFactory.Revision {
		t.Fatalf("reset emitted lifecycle invalidation: before=%+v after=%+v", beforeResetFactory, afterResetFactory)
	}
	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, client.ID, newLease.Generation, run.Revision, session.Revision, mustTime(t, 41)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale release error = %v", err)
	}
	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, client.ID, afterReset.LeaseGeneration, run.Revision, session.Revision, mustTime(t, 41)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("cleared-holder release error = %v", err)
	}
	secondRun := addRunningRunOnStore(t, store, 180)
	secondSession := terminalSessionForRunTest(t, store, secondRun.ID)
	secondClientID := browserTestID(t, 131)
	secondClient := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 131, boot, 400, 500, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, secondClientID, browserKey(t), 401)
	if _, err := store.ReleaseTerminalLease(ctx, secondRun.ID, secondSession.ID, secondClient.ID, 0, secondRun.Revision, secondSession.Revision, mustTime(t, 401)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("fresh generation release error = %v", err)
	}
}

func TestTerminalLeaseReleaseRequiresActiveExactHolder(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 150)
	first := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 150, boot, 10, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 150), browserKey(t), 11)
	second := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 151, boot, 12, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 151), browserKey(t), 13)
	lease, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, first.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}

	beforeRun, _, err := store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	beforeSession := terminalSessionForRunTest(t, store, run.ID)
	beforeFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, second.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, 32)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("wrong-client release error = %v", err)
	}
	assertReleaseRejectedWithoutMutation(t, store, run.ID, session.ID, beforeRun, beforeSession, beforeFactory)

	beforeRun, _, err = store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	beforeSession = terminalSessionForRunTest(t, store, run.ID)
	beforeFactory, err = store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, first.ID, lease.Generation, run.Revision, session.Revision, lease.ExpiresAt); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("expired-holder release error = %v", err)
	}
	assertReleaseRejectedWithoutMutation(t, store, run.ID, session.ID, beforeRun, beforeSession, beforeFactory)

	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, first.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, 32)); err != nil {
		t.Fatal(err)
	}
	beforeRun, _, err = store.Run(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	beforeSession = terminalSessionForRunTest(t, store, run.ID)
	beforeFactory, err = store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if beforeSession.LeaseClientID != nil {
		t.Fatalf("successful release retained holder: %+v", beforeSession)
	}
	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, first.ID, lease.Generation+1, run.Revision, session.Revision, mustTime(t, 33)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("repeat cleared-holder release error = %v", err)
	}
	assertReleaseRejectedWithoutMutation(t, store, run.ID, session.ID, beforeRun, beforeSession, beforeFactory)
	if _, err := store.ReleaseTerminalLease(ctx, run.ID, session.ID, first.ID, lease.Generation, run.Revision, session.Revision, mustTime(t, 33)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale-generation release error = %v", err)
	}
	assertReleaseRejectedWithoutMutation(t, store, run.ID, session.ID, beforeRun, beforeSession, beforeFactory)
}

func TestRevokeBrowserClientAtomicallyClearsMultipleLeases(t *testing.T) {
	ctx := context.Background()
	store, firstRun, _ := runningOrchestratorRun(t)
	defer store.Close()
	secondRun := addRunningRunOnStore(t, store, 190)
	firstSession := terminalSessionForRunTest(t, store, firstRun.ID)
	secondSession := terminalSessionForRunTest(t, store, secondRun.ID)
	boot := browserTestBoot(t, 140)
	client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 140, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 140), browserKey(t), 31)
	firstLease, err := store.AcquireTerminalLease(ctx, firstRun.ID, firstSession.ID, client.ID, firstRun.Revision, firstSession.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	secondLease, err := store.AcquireTerminalLease(ctx, secondRun.ID, secondSession.ID, client.ID, secondRun.Revision, secondSession.Revision, mustTime(t, 500))
	if err != nil {
		t.Fatal(err)
	}
	secondBefore := secondSession
	if firstLease.Generation != 1 || secondLease.Generation != 1 {
		t.Fatalf("initial generations = %v/%v", firstLease.Generation, secondLease.Generation)
	}
	firstResult, _, err := store.Run(ctx, firstRun.ID)
	if err != nil {
		t.Fatal(err)
	}
	secondResult, _, err := store.Run(ctx, secondRun.ID)
	if err != nil {
		t.Fatal(err)
	}
	beforeFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	firstRevoked, err := store.RevokeBrowserClient(ctx, client.ID, client.Revision, mustTime(t, 501))
	if err != nil || firstRevoked.RevokedAt == nil {
		t.Fatalf("revoke = %+v, %v", firstRevoked, err)
	}
	for _, runID := range []RunID{firstRun.ID, secondRun.ID} {
		session := terminalSessionForRunTest(t, store, runID)
		if session.LeaseClientID != nil || session.LeaseExpiresAt != nil || session.LastInputSequence != 0 || session.LeaseGeneration != 2 {
			t.Fatalf("revocation left lease on %s: %+v", runID, session)
		}
		wantSession := firstSession
		if runID == secondRun.ID {
			wantSession = secondBefore
		}
		if session.Revision != wantSession.Revision || session.UpdatedAt != wantSession.UpdatedAt {
			t.Fatalf("revocation changed session lifecycle: %+v", session)
		}
	}
	firstAfter, _, err := store.Run(ctx, firstRun.ID)
	if err != nil {
		t.Fatal(err)
	}
	secondAfter, _, err := store.Run(ctx, secondRun.ID)
	if err != nil {
		t.Fatal(err)
	}
	if firstAfter.Revision != firstResult.Revision || secondAfter.Revision != secondResult.Revision || firstAfter.UpdatedAt != firstResult.UpdatedAt || secondAfter.UpdatedAt != secondResult.UpdatedAt {
		t.Fatalf("revocation changed run lifecycle: first=%+v second=%+v", firstAfter, secondAfter)
	}
	afterFactory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if afterFactory.Head != beforeFactory.Head || afterFactory.Revision != beforeFactory.Revision {
		t.Fatalf("revocation emitted lifecycle invalidation: before=%+v after=%+v", beforeFactory, afterFactory)
	}
	var events int
	if err := store.readers.QueryRow(`SELECT COUNT(*) FROM browser_security_events WHERE kind = 'client_revoked' AND client_id = ?`, client.ID.Bytes()).Scan(&events); err != nil {
		t.Fatal(err)
	}
	if events != 1 {
		t.Fatalf("revocation event count = %d", events)
	}
	if _, err := store.AcquireTerminalLease(ctx, firstRun.ID, firstSession.ID, client.ID, firstRun.Revision, firstSession.Revision, mustTime(t, 33)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("revoked client reacquire error = %v", err)
	}
}

func TestBrowserLeaseCorruptionFailsClosedOnReadAndOpen(t *testing.T) {
	t.Run("generation", func(t *testing.T) {
		store, run, _ := runningOrchestratorRun(t)
		session := terminalSessionForRunTest(t, store, run.ID)
		boot := browserTestBoot(t, 150)
		client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 150, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 150), browserKey(t), 31)
		if _, err := store.AcquireTerminalLease(context.Background(), run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31)); err != nil {
			t.Fatal(err)
		}
		corruptSQL(t, store, `UPDATE terminal_sessions SET lease_generation = 0 WHERE id = ?`, session.ID.Bytes())
		if _, _, err := store.TerminalSession(context.Background(), session.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("corrupt generation read error = %v", err)
		}
		store.Close()
	})
	t.Run("inactive holder", func(t *testing.T) {
		store, run, _ := runningOrchestratorRun(t)
		session := terminalSessionForRunTest(t, store, run.ID)
		boot := browserTestBoot(t, 151)
		client := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 151, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 151), browserKey(t), 31)
		if _, err := store.AcquireTerminalLease(context.Background(), run.ID, session.ID, client.ID, run.Revision, session.Revision, mustTime(t, 31)); err != nil {
			t.Fatal(err)
		}
		corruptSQL(t, store, `UPDATE terminal_sessions SET state = 'declared' WHERE id = ?`, session.ID.Bytes())
		if _, _, err := store.TerminalSession(context.Background(), session.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("inactive lease read error = %v", err)
		}
		store.Close()
	})
}

func TestTerminalLeaseHolderSupersedesItsOwnUnexpiredLease(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	session := terminalSessionForRunTest(t, store, run.ID)
	boot := browserTestBoot(t, 130)
	writer := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 130, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 130), browserKey(t), 31)
	other := pairBrowserClient(t, store, mintBrowserChallenge(t, store, 131, boot, 30, 100, BrowserCapabilityObserve|BrowserCapabilityTerminalInput), boot, browserTestID(t, 131), browserKey(t), 31)
	first, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, writer.ID, run.Revision, session.Revision, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, writer.ID, first.Generation, 1, run.Revision, session.Revision, mustTime(t, 32)); err != nil {
		t.Fatal(err)
	}
	// The same paired browser reconnecting before expiry gets a fresh
	// generation with a fresh sequence, exactly like an expiry replacement.
	second, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, writer.ID, run.Revision, session.Revision, mustTime(t, 33))
	if err != nil {
		t.Fatalf("holder reacquire error = %v", err)
	}
	if second.Generation != first.Generation+1 || second.ExpiresAt.Int64() != 33+BrowserTerminalLeaseTTL || second.ClientID != writer.ID {
		t.Fatalf("holder reacquire = %+v", second)
	}
	// The superseded generation is fenced for every effect.
	if _, err := store.CheckTerminalLease(ctx, run.ID, session.ID, writer.ID, first.Generation, run.Revision, session.Revision, mustTime(t, 34)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("superseded check error = %v", err)
	}
	if _, err := store.RenewTerminalLease(ctx, run.ID, session.ID, writer.ID, first.Generation, run.Revision, session.Revision, mustTime(t, 34)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("superseded renewal error = %v", err)
	}
	// Sequence 1 is the next valid sequence after the reacquire, so only the
	// generation guard can refuse this reservation.
	if _, err := store.ReserveTerminalInputSequence(ctx, run.ID, session.ID, writer.ID, first.Generation, 1, run.Revision, session.Revision, mustTime(t, 34)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("superseded input error = %v", err)
	}
	if _, err := store.CheckTerminalLease(ctx, run.ID, session.ID, writer.ID, second.Generation, run.Revision, session.Revision, mustTime(t, 34)); err != nil {
		t.Fatalf("current generation check error = %v", err)
	}
	// A different client still waits for expiry.
	if _, err := store.AcquireTerminalLease(ctx, run.ID, session.ID, other.ID, run.Revision, session.Revision, mustTime(t, 34)); !errors.Is(err, ErrConflict) {
		t.Fatalf("other client while held error = %v", err)
	}
	afterSession := terminalSessionForRunTest(t, store, run.ID)
	if afterSession.LeaseClientID == nil || *afterSession.LeaseClientID != writer.ID || afterSession.LeaseGeneration != second.Generation || afterSession.LastInputSequence != 0 || afterSession.Revision != session.Revision {
		t.Fatalf("session after holder reacquire = %+v", afterSession)
	}
}
