package kernel

import (
	"context"
	"crypto/elliptic"
	"crypto/sha256"
	"crypto/subtle"
	"database/sql"
	"errors"
	"fmt"
	"math"
	"unicode/utf8"
)

// BrowserCapabilityMask is the deliberately closed browser authority mask.
type BrowserCapabilityMask uint8

const (
	BrowserCapabilityObserve BrowserCapabilityMask = 1 << iota
	BrowserCapabilityPrivateHumanRequestDetail
	BrowserCapabilityHumanActions
	BrowserCapabilityTerminalInput
)

const BrowserCapabilityKnownMask = BrowserCapabilityObserve | BrowserCapabilityPrivateHumanRequestDetail | BrowserCapabilityHumanActions | BrowserCapabilityTerminalInput
const BrowserTerminalLeaseTTL int64 = 30 * 1000

func (m BrowserCapabilityMask) validPairing() bool {
	return m != 0 && m&^BrowserCapabilityKnownMask == 0 && m&BrowserCapabilityObserve != 0
}

// Has reports whether one known capability bit is set. Composite, unknown and
// zero probes are intentionally not accepted as capability names.
func (m BrowserCapabilityMask) Has(bit BrowserCapabilityMask) bool {
	return bit != 0 && bit&^BrowserCapabilityKnownMask == 0 && bit&(bit-1) == 0 && m&bit != 0
}

type BrowserPairingChallenge struct {
	Digest         BrowserChallengeDigest
	BootID         BootID
	IntendedOrigin string
	CapabilityMask BrowserCapabilityMask
	CreatedAt      UnixMillis
	ExpiresAt      UnixMillis
	RedeemedAt     *UnixMillis
}

type BrowserClient struct {
	ID             BrowserClientID
	PublicKey      []byte
	Fingerprint    [DigestBytes]byte
	CapabilityMask BrowserCapabilityMask
	Revision       Revision
	CreatedAt      UnixMillis
	UpdatedAt      UnixMillis
	RevokedAt      *UnixMillis
}

// BrowserClientSummary is the bounded operator projection of one browser
// identity. It deliberately does not carry the public key or fingerprint;
// neither is needed to identify or revoke a client and neither belongs in a
// routine diagnostic response.
type BrowserClientSummary struct {
	ID             BrowserClientID
	CapabilityMask BrowserCapabilityMask
	Revision       Revision
	CreatedAt      UnixMillis
	UpdatedAt      UnixMillis
	RevokedAt      *UnixMillis
}

type BrowserClientPage struct {
	Items     []BrowserClientSummary
	NextAfter *BrowserClientID
}

const BrowserClientPageSize = 128

type BrowserClientCounts struct {
	Active           uint64
	Revoked          uint64
	ActiveChallenges uint64
}

// BrowserClientPrincipal is deliberately only a durable client identity. The
// daemon reloads BrowserClient before every effect instead of caching authority
// in a connection principal.
type BrowserClientPrincipal struct {
	id BrowserClientID
}

func (principal BrowserClientPrincipal) ClientID() BrowserClientID { return principal.id }

type BrowserSecurityEventKind string

const (
	BrowserSecurityChallengeMinted      BrowserSecurityEventKind = "challenge_minted"
	BrowserSecurityChallengeAbandoned   BrowserSecurityEventKind = "challenge_abandoned"
	BrowserSecurityClientPaired         BrowserSecurityEventKind = "client_paired"
	BrowserSecurityDuplicateFingerprint BrowserSecurityEventKind = "duplicate_fingerprint"
	BrowserSecurityClientRevoked        BrowserSecurityEventKind = "client_revoked"
)

func HashBrowserChallenge(raw []byte) BrowserChallengeDigest {
	return BrowserChallengeDigestFromHash(sha256.Sum256(raw))
}

func BrowserChallengeDigestFromHash(sum [DigestBytes]byte) BrowserChallengeDigest {
	d, _ := BrowserChallengeDigestFromBytes(sum[:])
	return d
}

func validateOrigin(origin string) error {
	if !utf8.ValidString(origin) || byteLen(origin) < 1 || byteLen(origin) > 4096 {
		return fmt.Errorf("%w: invalid browser origin", ErrInvalidValue)
	}
	return nil
}

func validateBrowserKey(key []byte) ([DigestBytes]byte, error) {
	if len(key) != 65 || key[0] != 4 {
		return [DigestBytes]byte{}, fmt.Errorf("%w: invalid browser public key", ErrInvalidValue)
	}
	x, y := elliptic.Unmarshal(elliptic.P256(), key)
	if x == nil || y == nil || !elliptic.P256().IsOnCurve(x, y) {
		return [DigestBytes]byte{}, fmt.Errorf("%w: invalid browser public key", ErrInvalidValue)
	}
	return sha256.Sum256(key), nil
}

func validateBrowserID(id BrowserClientID) error {
	if id.zero() {
		return fmt.Errorf("%w: invalid browser client identifier", ErrInvalidValue)
	}
	return nil
}

// CreateBrowserPairingChallenge returns the durable challenge when the write
// commits. On an ambiguous COMMIT it reconciles by digest: a returned
// challenge is an exact cleanup identity, while a zero challenge means the
// write was durably absent. Callers must not retry the ambiguous write.
func (store *Store) CreateBrowserPairingChallenge(ctx context.Context, digest BrowserChallengeDigest, bootID BootID, origin string, capabilities BrowserCapabilityMask, created, expires UnixMillis) (BrowserPairingChallenge, error) {
	if err := validateOrigin(origin); err != nil || bootID.zero() || created.Int64() < 0 || expires.Int64() < 0 || !capabilities.validPairing() || expires.Int64() <= created.Int64() || expires.Int64()-created.Int64() > 5*60*1000 {
		if err != nil {
			return BrowserPairingChallenge{}, err
		}
		return BrowserPairingChallenge{}, fmt.Errorf("%w: invalid browser pairing challenge", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return BrowserPairingChallenge{}, err
	}
	defer tx.Close()
	// Keep only current, live pairing opportunities and enforce the bounded
	// challenge surface before admitting the new row.
	if _, err := tx.connection.ExecContext(ctx, `DELETE FROM browser_pairing_challenges WHERE redeemed_at_ms IS NOT NULL OR expires_at_ms <= ? OR boot_id <> ?`, created.Int64(), bootID.Bytes()); err != nil {
		return BrowserPairingChallenge{}, tx.Rollback(err)
	}
	var active int64
	if err := tx.connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM browser_pairing_challenges WHERE boot_id = ? AND redeemed_at_ms IS NULL`, bootID.Bytes()).Scan(&active); err != nil {
		return BrowserPairingChallenge{}, tx.Rollback(err)
	}
	if active >= 32 {
		return BrowserPairingChallenge{}, tx.Rollback(ErrBusy)
	}
	var existing int
	lookupErr := tx.connection.QueryRowContext(ctx, `SELECT 1 FROM browser_pairing_challenges WHERE secret_digest = ?`, digest.Bytes()).Scan(&existing)
	if lookupErr == nil {
		return BrowserPairingChallenge{}, tx.Rollback(ErrConflict)
	}
	if !errors.Is(lookupErr, sql.ErrNoRows) {
		return BrowserPairingChallenge{}, tx.Rollback(lookupErr)
	}
	inserted, err := tx.connection.ExecContext(ctx, `INSERT INTO browser_pairing_challenges(secret_digest, boot_id, intended_origin, capability_mask, created_at_ms, expires_at_ms) VALUES(?, ?, ?, ?, ?, ?)`, digest.Bytes(), bootID.Bytes(), origin, int64(capabilities), created.Int64(), expires.Int64())
	if err := requireOneRow(inserted, err); err != nil {
		return BrowserPairingChallenge{}, tx.Rollback(err)
	}
	if err := insertBrowserSecurityEvent(ctx, tx.connection, BrowserSecurityChallengeMinted, nil, created); err != nil {
		return BrowserPairingChallenge{}, tx.Rollback(err)
	}
	challenge, found, err := browserChallengeByDigest(ctx, tx.connection, digest)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return BrowserPairingChallenge{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		var unknown *OutcomeUnknownError
		if !errors.As(err, &unknown) {
			return BrowserPairingChallenge{}, err
		}
		// COMMIT can be durable even when SQLite reports an ambiguous result.
		// Reconcile by the exact digest before returning: a found row lets the
		// daemon return a safe cleanup identity, while a missing row preserves
		// the ordinary pre-mint rejection. If the read itself is uncertain,
		// retain the locally known identity so the caller fails closed.
		observed, found, observeErr := store.browserChallengeAfterCommitUnknown(ctx, digest)
		if observeErr != nil {
			return challenge, errors.Join(err, observeErr)
		}
		if !found {
			return BrowserPairingChallenge{}, err
		}
		return observed, err
	}
	return challenge, nil
}

func (store *Store) browserChallengeAfterCommitUnknown(ctx context.Context, digest BrowserChallengeDigest) (BrowserPairingChallenge, bool, error) {
	connection, err := store.readerConnection(ctx)
	if err != nil {
		return BrowserPairingChallenge{}, false, err
	}
	defer connection.Close()
	return browserChallengeByDigest(ctx, connection, digest)
}

// InvalidateBrowserPairingChallenges removes every still-unredeemed challenge
// minted by one browser runtime boot. Runtime shutdown calls this only after
// joining the transport, so an in-flight pair cannot leave an active
// challenge behind. Redeemed rows are already inactive and remain bounded
// until the next mint cleanup.
func (store *Store) InvalidateBrowserPairingChallenges(ctx context.Context, bootID BootID) error {
	if bootID.zero() {
		return fmt.Errorf("%w: invalid browser runtime boot", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return err
	}
	defer tx.Close()
	if _, err := tx.connection.ExecContext(ctx, `DELETE FROM browser_pairing_challenges WHERE boot_id = ? AND redeemed_at_ms IS NULL`, bootID.Bytes()); err != nil {
		return tx.Rollback(err)
	}
	return tx.Commit(ctx)
}

// RedeemBrowserPairingChallenge consumes a challenge before checking duplicate identity.
func (store *Store) RedeemBrowserPairingChallenge(ctx context.Context, digest BrowserChallengeDigest, bootID BootID, origin string, clientID BrowserClientID, publicKey []byte, at UnixMillis) (BrowserClient, error) {
	if bootID.zero() || validateOrigin(origin) != nil || validateBrowserID(clientID) != nil {
		return BrowserClient{}, ErrUnauthorized
	}
	fingerprint, err := validateBrowserKey(publicKey)
	if err != nil {
		return BrowserClient{}, ErrUnauthorized
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return BrowserClient{}, err
	}
	defer tx.Close()
	challenge, found, err := browserChallengeByDigest(ctx, tx.connection, digest)
	if err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	if !found || challenge.BootID != bootID || challenge.IntendedOrigin != origin || challenge.RedeemedAt != nil || at.Int64() < challenge.CreatedAt.Int64() || at.Int64() >= challenge.ExpiresAt.Int64() {
		return BrowserClient{}, tx.Rollback(ErrUnauthorized)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE browser_pairing_challenges SET redeemed_at_ms = ? WHERE secret_digest = ? AND redeemed_at_ms IS NULL`, at.Int64(), digest.Bytes())
	if err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	if err := requireOneRow(updated, nil); err != nil {
		return BrowserClient{}, tx.Rollback(ErrConflict)
	}
	var existingRaw []byte
	err = tx.connection.QueryRowContext(ctx, `SELECT id FROM browser_clients WHERE fingerprint = ?`, fingerprint[:]).Scan(&existingRaw)
	if err == nil {
		existing, parseErr := BrowserClientIDFromBytes(existingRaw)
		if parseErr != nil {
			return BrowserClient{}, tx.Rollback(ErrCorruptState)
		}
		if err := insertBrowserSecurityEvent(ctx, tx.connection, BrowserSecurityDuplicateFingerprint, &existing, at); err != nil {
			return BrowserClient{}, tx.Rollback(err)
		}
		if err := tx.Commit(ctx); err != nil {
			return BrowserClient{}, err
		}
		return BrowserClient{}, ErrConflict
	}
	if !errors.Is(err, sql.ErrNoRows) {
		return BrowserClient{}, tx.Rollback(err)
	}
	inserted, err := tx.connection.ExecContext(ctx, `INSERT INTO browser_clients(id, public_key, fingerprint, capability_mask, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, ?, 1, ?, ?)`, clientID.Bytes(), publicKey, fingerprint[:], int64(challenge.CapabilityMask), at.Int64(), at.Int64())
	if err := requireOneRow(inserted, err); err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	if err := insertBrowserSecurityEvent(ctx, tx.connection, BrowserSecurityClientPaired, &clientID, at); err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	client, found, err := browserClientByID(ctx, tx.connection, clientID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return BrowserClient{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return BrowserClient{}, err
	}
	return client, nil
}

func browserChallengeByDigest(ctx context.Context, c *sql.Conn, digest BrowserChallengeDigest) (BrowserPairingChallenge, bool, error) {
	var rawDigest, rawBoot []byte
	var origin string
	var mask, created, expires sql.NullInt64
	var redeemed sql.NullInt64
	err := c.QueryRowContext(ctx, `SELECT secret_digest, boot_id, intended_origin, capability_mask, created_at_ms, expires_at_ms, redeemed_at_ms FROM browser_pairing_challenges WHERE secret_digest = ?`, digest.Bytes()).Scan(&rawDigest, &rawBoot, &origin, &mask, &created, &expires, &redeemed)
	if errors.Is(err, sql.ErrNoRows) {
		return BrowserPairingChallenge{}, false, nil
	}
	if err != nil {
		return BrowserPairingChallenge{}, false, err
	}
	d, derr := BrowserChallengeDigestFromBytes(rawDigest)
	b, berr := BootIDFromBytes(rawBoot)
	ca, cerr := NewUnixMillis(created.Int64)
	ea, eerr := NewUnixMillis(expires.Int64)
	if derr != nil || berr != nil || !utf8.ValidString(origin) || byteLen(origin) < 1 || byteLen(origin) > 4096 || mask.Int64 < 1 || mask.Int64 > int64(BrowserCapabilityKnownMask) || mask.Int64&int64(BrowserCapabilityObserve) == 0 || cerr != nil || eerr != nil || expires.Int64 <= created.Int64 || expires.Int64-created.Int64 > 300000 || redeemed.Valid && (redeemed.Int64 < created.Int64 || redeemed.Int64 >= expires.Int64 || redeemed.Int64 < 0) {
		return BrowserPairingChallenge{}, false, fmt.Errorf("%w: invalid browser challenge row", ErrCorruptState)
	}
	result := BrowserPairingChallenge{Digest: d, BootID: b, IntendedOrigin: origin, CapabilityMask: BrowserCapabilityMask(mask.Int64), CreatedAt: ca, ExpiresAt: ea}
	if redeemed.Valid {
		value, _ := NewUnixMillis(redeemed.Int64)
		result.RedeemedAt = &value
	}
	return result, true, nil
}

func browserClientByID(ctx context.Context, c *sql.Conn, id BrowserClientID) (BrowserClient, bool, error) {
	var rawID, key, fingerprint []byte
	var mask, revision, created, updated sql.NullInt64
	var revoked sql.NullInt64
	err := c.QueryRowContext(ctx, `SELECT id, public_key, fingerprint, capability_mask, revision, created_at_ms, updated_at_ms, revoked_at_ms FROM browser_clients WHERE id = ?`, id.Bytes()).Scan(&rawID, &key, &fingerprint, &mask, &revision, &created, &updated, &revoked)
	if errors.Is(err, sql.ErrNoRows) {
		return BrowserClient{}, false, nil
	}
	if err != nil {
		return BrowserClient{}, false, err
	}
	parsedID, idErr := BrowserClientIDFromBytes(rawID)
	fp, fpErr := digestFromBytes(fingerprint)
	keyFP, keyErr := validateBrowserKey(key)
	rev, revErr := NewRevision(revision.Int64)
	ca, caErr := NewUnixMillis(created.Int64)
	ua, uaErr := NewUnixMillis(updated.Int64)
	if idErr != nil || fpErr != nil || keyErr != nil || subtle.ConstantTimeCompare(fp.b[:], keyFP[:]) != 1 || mask.Int64 < 1 || mask.Int64 > int64(BrowserCapabilityKnownMask) || mask.Int64&int64(BrowserCapabilityObserve) == 0 || revErr != nil || caErr != nil || uaErr != nil || updated.Int64 < created.Int64 || revoked.Valid && (revoked.Int64 < created.Int64 || revoked.Int64 > updated.Int64 || revoked.Int64 < 0) {
		return BrowserClient{}, false, fmt.Errorf("%w: invalid browser client row", ErrCorruptState)
	}
	result := BrowserClient{ID: parsedID, PublicKey: append([]byte(nil), key...), CapabilityMask: BrowserCapabilityMask(mask.Int64), Revision: rev, CreatedAt: ca, UpdatedAt: ua}
	copy(result.Fingerprint[:], fp.b[:])
	if revoked.Valid {
		value, _ := NewUnixMillis(revoked.Int64)
		result.RevokedAt = &value
	}
	return result, true, nil
}

func (store *Store) BrowserClient(ctx context.Context, id BrowserClientID) (BrowserClient, bool, error) {
	if err := validateBrowserID(id); err != nil {
		return BrowserClient{}, false, err
	}
	c, err := store.readerConnection(ctx)
	if err != nil {
		return BrowserClient{}, false, err
	}
	defer c.Close()
	return browserClientByID(ctx, c, id)
}

// ListBrowserClients returns a deterministic, fixed-size page. Browser client
// rows are never deleted because their fingerprints remain a durable anti-
// cloning boundary; pagination keeps this operator response bounded even if
// an installation accumulates many revoked identities.
func (store *Store) ListBrowserClients(ctx context.Context, after *BrowserClientID) (BrowserClientPage, error) {
	if after != nil {
		if err := validateBrowserID(*after); err != nil {
			return BrowserClientPage{}, err
		}
	}
	c, err := store.readerConnection(ctx)
	if err != nil {
		return BrowserClientPage{}, err
	}
	defer c.Close()

	query := `SELECT id FROM browser_clients ORDER BY created_at_ms, id LIMIT ?`
	args := []any{BrowserClientPageSize + 1}
	if after != nil {
		cursor, found, err := browserClientByID(ctx, c, *after)
		if err != nil {
			return BrowserClientPage{}, err
		}
		if !found {
			return BrowserClientPage{}, ErrNotFound
		}
		created := cursor.CreatedAt.Int64()
		query = `SELECT id FROM browser_clients WHERE (created_at_ms > ? OR (created_at_ms = ? AND id > ?)) ORDER BY created_at_ms, id LIMIT ?`
		args = []any{created, created, after.Bytes(), BrowserClientPageSize + 1}
	}
	rows, err := c.QueryContext(ctx, query, args...)
	if err != nil {
		return BrowserClientPage{}, err
	}
	defer rows.Close()
	ids := make([]BrowserClientID, 0, BrowserClientPageSize+1)
	for rows.Next() {
		var raw []byte
		if err := rows.Scan(&raw); err != nil {
			return BrowserClientPage{}, err
		}
		id, err := BrowserClientIDFromBytes(raw)
		if err != nil {
			return BrowserClientPage{}, fmt.Errorf("%w: invalid browser client list row", ErrCorruptState)
		}
		ids = append(ids, id)
	}
	if err := rows.Err(); err != nil {
		return BrowserClientPage{}, err
	}
	page := BrowserClientPage{Items: make([]BrowserClientSummary, 0, len(ids))}
	if len(ids) > BrowserClientPageSize {
		ids = ids[:BrowserClientPageSize]
		last := ids[len(ids)-1]
		page.NextAfter = &last
	}
	for _, id := range ids {
		client, found, err := browserClientByID(ctx, c, id)
		if err != nil {
			return BrowserClientPage{}, err
		}
		if !found {
			return BrowserClientPage{}, fmt.Errorf("%w: browser client disappeared during page read", ErrCorruptState)
		}
		page.Items = append(page.Items, BrowserClientSummary{
			ID: client.ID, CapabilityMask: client.CapabilityMask, Revision: client.Revision,
			CreatedAt: client.CreatedAt, UpdatedAt: client.UpdatedAt, RevokedAt: client.RevokedAt,
		})
	}
	return page, nil
}

func (store *Store) BrowserClientCounts(ctx context.Context, bootID BootID, at UnixMillis) (BrowserClientCounts, error) {
	if bootID.zero() || at.Int64() < 0 {
		return BrowserClientCounts{}, fmt.Errorf("%w: invalid browser client count request", ErrInvalidValue)
	}
	c, err := store.readerConnection(ctx)
	if err != nil {
		return BrowserClientCounts{}, err
	}
	defer c.Close()
	var result BrowserClientCounts
	if err := c.QueryRowContext(ctx, `SELECT COUNT(*), COALESCE(SUM(CASE WHEN revoked_at_ms IS NOT NULL THEN 1 ELSE 0 END), 0), (SELECT COUNT(*) FROM browser_pairing_challenges WHERE boot_id = ? AND redeemed_at_ms IS NULL AND expires_at_ms > ?) FROM browser_clients`, bootID.Bytes(), at.Int64()).Scan(&result.Active, &result.Revoked, &result.ActiveChallenges); err != nil {
		return BrowserClientCounts{}, err
	}
	if result.Revoked > result.Active {
		return BrowserClientCounts{}, fmt.Errorf("%w: invalid browser client counts", ErrCorruptState)
	}
	result.Active -= result.Revoked
	return result, nil
}

func (store *Store) AuthenticateBrowserClient(ctx context.Context, id BrowserClientID) (BrowserClientPrincipal, error) {
	client, found, err := store.BrowserClient(ctx, id)
	if err != nil {
		return BrowserClientPrincipal{}, err
	}
	if !found || client.RevokedAt != nil {
		return BrowserClientPrincipal{}, ErrUnauthorized
	}
	return BrowserClientPrincipal{id: client.ID}, nil
}

func insertBrowserSecurityEvent(ctx context.Context, c *sql.Conn, kind BrowserSecurityEventKind, clientID *BrowserClientID, at UnixMillis) error {
	if !validBrowserSecurityKind(kind) || isBrowserChallengeEvent(kind) != (clientID == nil) {
		return fmt.Errorf("%w: invalid browser security event", ErrInvalidValue)
	}
	inserted, err := c.ExecContext(ctx, `INSERT INTO browser_security_events(kind, client_id, occurred_at_ms) VALUES(?, ?, ?)`, string(kind), nullableBrowserClient(clientID), at.Int64())
	if err := requireOneRow(inserted, err); err != nil {
		return err
	}
	_, err = c.ExecContext(ctx, `DELETE FROM browser_security_events WHERE sequence < COALESCE((SELECT sequence FROM browser_security_events ORDER BY sequence DESC LIMIT 1 OFFSET 4095), 0)`)
	if err != nil {
		return err
	}
	var count int64
	if err := c.QueryRowContext(ctx, `SELECT COUNT(*) FROM browser_security_events`).Scan(&count); err != nil {
		return err
	}
	if count > EventRetentionLimit {
		return fmt.Errorf("%w: browser security event retention exceeded", ErrCorruptState)
	}
	return nil
}
func nullableBrowserClient(id *BrowserClientID) any {
	if id == nil {
		return nil
	}
	return id.Bytes()
}
func validBrowserSecurityKind(kind BrowserSecurityEventKind) bool {
	switch kind {
	case BrowserSecurityChallengeMinted, BrowserSecurityChallengeAbandoned, BrowserSecurityClientPaired, BrowserSecurityDuplicateFingerprint, BrowserSecurityClientRevoked:
		return true
	default:
		return false
	}
}

func isBrowserChallengeEvent(kind BrowserSecurityEventKind) bool {
	return kind == BrowserSecurityChallengeMinted || kind == BrowserSecurityChallengeAbandoned
}

type TerminalLease struct {
	RunID             RunID
	SessionID         TerminalSessionID
	ClientID          BrowserClientID
	Generation        uint64
	ExpiresAt         UnixMillis
	LastInputSequence uint64
	RunRevision       Revision
	SessionRevision   Revision
}
type InputReservation struct {
	RunID           RunID
	SessionID       TerminalSessionID
	ClientID        BrowserClientID
	Generation      uint64
	Sequence        uint64
	RunRevision     Revision
	SessionRevision Revision
}

func leaseGenerationNext(value int64) (int64, error) {
	if value < 0 || value >= math.MaxInt64 {
		return 0, fmt.Errorf("%w: terminal lease generation overflow", ErrCorruptState)
	}
	return value + 1, nil
}
func sequenceNext(value int64) (int64, error) {
	if value < 0 || value >= math.MaxInt64 {
		return 0, fmt.Errorf("%w: terminal input sequence overflow", ErrCorruptState)
	}
	return value + 1, nil
}

func (store *Store) AcquireTerminalLease(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, expectedRun, expectedSession Revision, at UnixMillis) (TerminalLease, error) {
	if runID.zero() || sessionID.zero() || validateBrowserID(clientID) != nil {
		return TerminalLease{}, fmt.Errorf("%w: invalid terminal lease request", ErrInvalidValue)
	}
	if at.Int64() > math.MaxInt64-BrowserTerminalLeaseTTL {
		return TerminalLease{}, fmt.Errorf("%w: terminal lease expiry overflow", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TerminalLease{}, err
	}
	defer tx.Close()
	run, session, client, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || run.Phase != RunRunning || session.State != TerminalSessionActive || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityTerminalInput) {
		return TerminalLease{}, tx.Rollback(ErrUnauthorized)
	}
	// Another client's unexpired lease refuses the acquire. The holder itself
	// may acquire again from a new connection, for example after a page reload
	// that could not release, or from a second tab of the same browser
	// profile, which shares the client key; the fresh generation fences the
	// earlier connection exactly like an expiry replacement.
	if session.LeaseClientID != nil && session.LeaseExpiresAt != nil && session.LeaseExpiresAt.Int64() > at.Int64() && *session.LeaseClientID != clientID {
		return TerminalLease{}, tx.Rollback(ErrConflict)
	}
	next, err := leaseGenerationNext(int64(session.LeaseGeneration))
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	expiry := at.Int64() + BrowserTerminalLeaseTTL
	if err := updateLease(ctx, tx.connection, session, clientID, next, expiry, at); err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return TerminalLease{}, err
	}
	return TerminalLease{RunID: run.ID, SessionID: session.ID, ClientID: clientID, Generation: uint64(next), ExpiresAt: UnixMillis{value: expiry}, RunRevision: run.Revision, SessionRevision: session.Revision}, nil
}

// CheckTerminalLease authorizes a terminal effect without changing any
// durable state. The pinned read transaction keeps the run, session and
// client observations from crossing a concurrent lifecycle boundary.
func (store *Store) CheckTerminalLease(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, generation uint64, expectedRun, expectedSession Revision, at UnixMillis) (TerminalLease, error) {
	if runID.zero() || sessionID.zero() || validateBrowserID(clientID) != nil || generation == 0 || generation > math.MaxInt64 {
		return TerminalLease{}, fmt.Errorf("%w: invalid terminal lease check", ErrInvalidValue)
	}
	tx, err := store.beginRead(ctx)
	if err != nil {
		return TerminalLease{}, err
	}
	defer tx.Close()
	run, session, client, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return TerminalLease{}, err
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || run.Phase != RunRunning || session.State != TerminalSessionActive || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityTerminalInput) || session.LeaseClientID == nil || *session.LeaseClientID != clientID || session.LeaseGeneration != generation || session.LeaseExpiresAt == nil || session.LeaseExpiresAt.Int64() <= at.Int64() {
		return TerminalLease{}, ErrUnauthorized
	}
	return TerminalLease{RunID: run.ID, SessionID: session.ID, ClientID: client.ID, Generation: generation, ExpiresAt: *session.LeaseExpiresAt, LastInputSequence: session.LastInputSequence, RunRevision: run.Revision, SessionRevision: session.Revision}, nil
}

// RenewTerminalLease changes only expiry. Generation, sequence and lifecycle
// revisions are deliberately untouched by renewal.
func (store *Store) RenewTerminalLease(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, generation uint64, expectedRun, expectedSession Revision, at UnixMillis) (TerminalLease, error) {
	if at.Int64() > math.MaxInt64-BrowserTerminalLeaseTTL {
		return TerminalLease{}, fmt.Errorf("%w: terminal lease expiry overflow", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TerminalLease{}, err
	}
	defer tx.Close()
	run, session, client, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || run.Phase != RunRunning || session.State != TerminalSessionActive || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityTerminalInput) || session.LeaseClientID == nil || *session.LeaseClientID != clientID || session.LeaseGeneration != generation || session.LeaseExpiresAt == nil || at.Int64() >= session.LeaseExpiresAt.Int64() {
		return TerminalLease{}, tx.Rollback(ErrUnauthorized)
	}
	expires := at.Int64() + BrowserTerminalLeaseTTL
	if expires < session.LeaseExpiresAt.Int64() {
		return TerminalLease{}, tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET lease_expires_at_ms = ? WHERE id = ? AND lease_client_id = ? AND lease_generation = ? AND lease_expires_at_ms = ?`, expires, session.ID.Bytes(), clientID.Bytes(), generation, session.LeaseExpiresAt.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return TerminalLease{}, err
	}
	return TerminalLease{RunID: run.ID, SessionID: session.ID, ClientID: clientID, Generation: generation, ExpiresAt: UnixMillis{value: expires}, LastInputSequence: session.LastInputSequence, RunRevision: run.Revision, SessionRevision: session.Revision}, nil
}

func leaseRows(ctx context.Context, c *sql.Conn, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID) (Run, TerminalSession, BrowserClient, error) {
	run, found, err := runByID(ctx, c, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, TerminalSession{}, BrowserClient{}, err
	}
	session, found, err := terminalSessionByID(ctx, c, sessionID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, TerminalSession{}, BrowserClient{}, err
	}
	if session.RunID != run.ID {
		return Run{}, TerminalSession{}, BrowserClient{}, ErrConflict
	}
	client, found, err := browserClientByID(ctx, c, clientID)
	if err != nil || !found {
		if err == nil {
			err = ErrUnauthorized
		}
		return Run{}, TerminalSession{}, BrowserClient{}, err
	}
	return run, session, client, nil
}

func updateLease(ctx context.Context, c *sql.Conn, session TerminalSession, clientID BrowserClientID, generation int64, expiry int64, at UnixMillis) error {
	// Serialized writes already give concurrent acquires one durable winner;
	// pinning the observed generation is defence in depth against a drifted
	// read. The guard admits the current holder's own reacquire alongside the
	// vacant and expired cases.
	result, err := c.ExecContext(ctx, `UPDATE terminal_sessions SET lease_client_id = ?, lease_generation = ?, lease_expires_at_ms = ?, last_input_sequence = 0 WHERE id = ? AND lease_generation = ? AND (lease_client_id IS NULL OR lease_expires_at_ms <= ? OR lease_client_id = ?)`, clientID.Bytes(), generation, expiry, session.ID.Bytes(), int64(session.LeaseGeneration), at.Int64(), clientID.Bytes())
	if err := requireOneRow(result, err); err != nil {
		return err
	}
	return nil
}

func (store *Store) ReleaseTerminalLease(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, generation uint64, expectedRun, expectedSession Revision, at UnixMillis) (TerminalLease, error) {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TerminalLease{}, err
	}
	defer tx.Close()
	run, session, _, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || session.LeaseClientID == nil || *session.LeaseClientID != clientID || session.LeaseGeneration != generation || session.LeaseExpiresAt == nil || at.Int64() >= session.LeaseExpiresAt.Int64() {
		return TerminalLease{}, tx.Rollback(ErrRevisionConflict)
	}
	next, err := leaseGenerationNext(int64(generation))
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET lease_client_id = NULL, lease_expires_at_ms = NULL, lease_generation = ?, last_input_sequence = 0 WHERE id = ? AND lease_client_id = ? AND lease_generation = ?`, next, session.ID.Bytes(), clientID.Bytes(), generation)
	if err := requireOneRow(updated, err); err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return TerminalLease{}, err
	}
	return TerminalLease{RunID: run.ID, SessionID: session.ID, ClientID: clientID, Generation: uint64(next), RunRevision: run.Revision, SessionRevision: session.Revision}, nil
}

// RevokeTerminalLease clears one already-committed lease after a runner
// generation install fails or becomes uncertain. Unlike an operator release,
// this cleanup remains valid after expiry. It is a one-shot, exact-generation
// transition and cannot clear a replacement lease. If the caller cannot prove
// whether the commit completed, it must not retry this method; the daemon must
// converge or finalize instead.
func (store *Store) RevokeTerminalLease(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, generation uint64, expectedRun, expectedSession Revision, at UnixMillis) (TerminalLease, error) {
	if runID.zero() || sessionID.zero() || validateBrowserID(clientID) != nil || generation == 0 || generation > math.MaxInt64 {
		return TerminalLease{}, fmt.Errorf("%w: invalid terminal lease revocation", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TerminalLease{}, err
	}
	defer tx.Close()
	run, session, _, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || run.Phase != RunRunning || session.State != TerminalSessionActive {
		return TerminalLease{}, tx.Rollback(ErrRevisionConflict)
	}
	next, err := leaseGenerationNext(int64(generation))
	if err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if session.LeaseClientID == nil || session.LeaseExpiresAt == nil || *session.LeaseClientID != clientID || session.LeaseGeneration != generation {
		return TerminalLease{}, tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET lease_client_id = NULL, lease_expires_at_ms = NULL, lease_generation = ?, last_input_sequence = 0 WHERE id = ? AND lease_client_id = ? AND lease_generation = ?`, next, session.ID.Bytes(), clientID.Bytes(), generation)
	if err := requireOneRow(updated, err); err != nil {
		return TerminalLease{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return TerminalLease{}, err
	}
	return TerminalLease{RunID: run.ID, SessionID: session.ID, ClientID: clientID, Generation: uint64(next), RunRevision: run.Revision, SessionRevision: session.Revision}, nil
}

func (store *Store) ReserveTerminalInputSequence(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, generation, sequence uint64, expectedRun, expectedSession Revision, at UnixMillis) (InputReservation, error) {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return InputReservation{}, err
	}
	defer tx.Close()
	run, session, client, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return InputReservation{}, tx.Rollback(err)
	}
	return reserveTerminalInputSequenceTx(ctx, tx, run, session, client, generation, sequence, expectedRun, expectedSession, at)
}

func reserveTerminalInputSequenceTx(ctx context.Context, tx *writeTx, run Run, session TerminalSession, client BrowserClient, generation, sequence uint64, expectedRun, expectedSession Revision, at UnixMillis) (InputReservation, error) {
	if sequence == 0 || sequence > math.MaxInt64 || sequence != session.LastInputSequence+1 {
		return InputReservation{}, tx.Rollback(ErrRevisionConflict)
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || run.Phase != RunRunning || session.State != TerminalSessionActive || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityTerminalInput) || session.LeaseClientID == nil || *session.LeaseClientID != client.ID || session.LeaseGeneration != generation || session.LeaseExpiresAt == nil || session.LeaseExpiresAt.Int64() <= at.Int64() {
		return InputReservation{}, tx.Rollback(ErrUnauthorized)
	}
	next, err := sequenceNext(int64(session.LastInputSequence))
	if err != nil {
		return InputReservation{}, tx.Rollback(err)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET last_input_sequence = ? WHERE id = ? AND lease_client_id = ? AND lease_generation = ? AND last_input_sequence = ?`, next, session.ID.Bytes(), client.ID.Bytes(), generation, session.LastInputSequence)
	if err := requireOneRow(updated, err); err != nil {
		return InputReservation{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return InputReservation{}, err
	}
	return InputReservation{RunID: run.ID, SessionID: session.ID, ClientID: client.ID, Generation: generation, Sequence: uint64(next), RunRevision: run.Revision, SessionRevision: session.Revision}, nil
}

// RevokeTerminalInputReservation consumes a reserved input sequence and
// revokes its lease generation. It is used for every non-complete delivery,
// not only a known partial write; the reservation is never replayed.
func (store *Store) RevokeTerminalInputReservation(ctx context.Context, runID RunID, sessionID TerminalSessionID, clientID BrowserClientID, generation uint64, sequence uint64, expectedRun, expectedSession Revision, at UnixMillis) error {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return err
	}
	defer tx.Close()
	run, session, _, err := leaseRows(ctx, tx.connection, runID, sessionID, clientID)
	if err != nil {
		return tx.Rollback(err)
	}
	if sequence == 0 || sequence > math.MaxInt64 || run.Revision != expectedRun || session.Revision != expectedSession || run.Phase != RunRunning || session.State != TerminalSessionActive || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() || session.LeaseClientID == nil || *session.LeaseClientID != clientID || session.LeaseGeneration != generation || session.LastInputSequence != sequence {
		return tx.Rollback(ErrRevisionConflict)
	}
	next, err := leaseGenerationNext(int64(generation))
	if err != nil {
		return tx.Rollback(err)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET lease_client_id = NULL, lease_expires_at_ms = NULL, lease_generation = ?, last_input_sequence = 0 WHERE id = ? AND lease_client_id = ? AND lease_generation = ? AND last_input_sequence = ?`, next, session.ID.Bytes(), clientID.Bytes(), generation, sequence)
	if err := requireOneRow(updated, err); err != nil {
		return tx.Rollback(err)
	}
	return tx.Commit(ctx)
}

func (store *Store) RevokeBrowserClient(ctx context.Context, id BrowserClientID, expected Revision, at UnixMillis) (BrowserClient, error) {
	if validateBrowserID(id) != nil || expected.Int64() < 1 {
		return BrowserClient{}, fmt.Errorf("%w: invalid browser revocation request", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return BrowserClient{}, err
	}
	defer tx.Close()
	client, found, err := browserClientByID(ctx, tx.connection, id)
	if err != nil || !found {
		if err == nil {
			err = ErrUnauthorized
		}
		return BrowserClient{}, tx.Rollback(err)
	}
	if client.RevokedAt != nil && client.Revision == expected {
		return client, tx.Rollback(nil)
	}
	if client.RevokedAt != nil || client.Revision != expected || at.Int64() < client.UpdatedAt.Int64() || expected.Int64() >= math.MaxInt64 {
		return BrowserClient{}, tx.Rollback(ErrRevisionConflict)
	}
	updatedClient, err := tx.connection.ExecContext(ctx, `UPDATE browser_clients SET revoked_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ? AND revoked_at_ms IS NULL`, at.Int64(), at.Int64(), id.Bytes(), expected.Int64())
	if err := requireOneRow(updatedClient, err); err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	rows, err := tx.connection.QueryContext(ctx, `SELECT id, lease_generation FROM terminal_sessions WHERE lease_client_id = ?`, id.Bytes())
	if err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	type held struct {
		sid        []byte
		generation int64
	}
	var sessions []held
	for rows.Next() {
		var h held
		if err := rows.Scan(&h.sid, &h.generation); err != nil {
			_ = closeValidatedBrowserRows(rows)
			return BrowserClient{}, tx.Rollback(err)
		}
		sessions = append(sessions, h)
	}
	if err := closeValidatedBrowserRows(rows); err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	for _, h := range sessions {
		_, err := leaseGenerationNext(h.generation)
		if err != nil {
			return BrowserClient{}, tx.Rollback(err)
		}
		updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET lease_client_id = NULL, lease_expires_at_ms = NULL, lease_generation = lease_generation + 1, last_input_sequence = 0 WHERE id = ? AND lease_client_id = ? AND lease_generation = ?`, h.sid, id.Bytes(), h.generation)
		if err := requireOneRow(updated, err); err != nil {
			return BrowserClient{}, tx.Rollback(err)
		}
	}
	if err := insertBrowserSecurityEvent(ctx, tx.connection, BrowserSecurityClientRevoked, &id, at); err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	client, _, err = browserClientByID(ctx, tx.connection, id)
	if err != nil {
		return BrowserClient{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return BrowserClient{}, err
	}
	return client, nil
}

func (store *Store) ResetTerminalLeases(ctx context.Context) error {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return err
	}
	defer tx.Close()
	rows, err := tx.connection.QueryContext(ctx, `SELECT id, lease_generation FROM terminal_sessions WHERE lease_client_id IS NOT NULL`)
	if err != nil {
		return tx.Rollback(err)
	}
	type held struct {
		sid []byte
		gen int64
	}
	var sessions []held
	for rows.Next() {
		var h held
		if err := rows.Scan(&h.sid, &h.gen); err != nil {
			_ = closeValidatedBrowserRows(rows)
			return tx.Rollback(err)
		}
		sessions = append(sessions, h)
	}
	if err := closeValidatedBrowserRows(rows); err != nil {
		return tx.Rollback(err)
	}
	for _, h := range sessions {
		if _, err := leaseGenerationNext(h.gen); err != nil {
			return tx.Rollback(err)
		}
		updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET lease_client_id = NULL, lease_expires_at_ms = NULL, lease_generation = lease_generation + 1, last_input_sequence = 0 WHERE id = ? AND lease_client_id IS NOT NULL AND lease_generation = ?`, h.sid, h.gen)
		if err := requireOneRow(updated, err); err != nil {
			return tx.Rollback(err)
		}
	}
	return tx.Commit(ctx)
}
