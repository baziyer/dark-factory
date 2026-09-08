package kernel

import (
	"bytes"
	"context"
	"crypto/rand"
	"database/sql"
	"errors"
	"fmt"
)

const humanRequestColumns = `id, run_id, idempotency_key, kind, reason_code,
    question_text, status, delivery_id, delivery_started_at_ms,
    resolution_kind, closed_at_ms, revision, created_at_ms, updated_at_ms`

func scanHumanRequest(scanner rowScanner) (HumanRequest, bool, error) {
	var rawID, rawRunID, rawKey []byte
	var rawDelivery nullableBlob
	var rawKind, rawStatus string
	var rawReason, rawResolution sql.NullString
	var question string
	var deliveryAt, closedAt sql.NullInt64
	var revision, createdAt, updatedAt int64
	if err := scanner.Scan(&rawID, &rawRunID, &rawKey, &rawKind, &rawReason,
		&question, &rawStatus, &rawDelivery, &deliveryAt, &rawResolution,
		&closedAt, &revision, &createdAt, &updatedAt); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return HumanRequest{}, false, nil
		}
		return HumanRequest{}, false, fmt.Errorf("scan human request: %w", err)
	}
	id, idErr := HumanRequestIDFromBytes(rawID)
	runID, runErr := RunIDFromBytes(rawRunID)
	kind, kindErr := parseHumanRequestKind(rawKind)
	status, statusErr := parseHumanRequestStatus(rawStatus)
	rev, revErr := NewRevision(revision)
	created, createdErr := NewUnixMillis(createdAt)
	updated, updatedErr := NewUnixMillis(updatedAt)
	if idErr != nil || runErr != nil || len(rawKey) != IDBytes || bytes.Equal(rawKey, make([]byte, IDBytes)) || kindErr != nil || rawReason.String != "provider_question" || !rawReason.Valid || statusErr != nil || revErr != nil || createdErr != nil || updatedErr != nil || !utf8TextWithin(question, 1, MaxHumanRequestQuestionBytes) {
		return HumanRequest{}, false, fmt.Errorf("%w: invalid human request row", ErrCorruptState)
	}
	var key [IDBytes]byte
	copy(key[:], rawKey)
	result := HumanRequest{ID: id, RunID: runID, IdempotencyKey: key, Kind: kind, Status: status, QuestionText: question, Revision: rev, CreatedAt: created, UpdatedAt: updated}
	if rawDelivery.valid {
		delivery, err := HumanRequestDeliveryIDFromBytes(rawDelivery.bytes)
		if err != nil {
			return HumanRequest{}, false, fmt.Errorf("%w: invalid human delivery identity", ErrCorruptState)
		}
		result.DeliveryID = &delivery
	}
	if deliveryAt.Valid {
		value, err := NewUnixMillis(deliveryAt.Int64)
		if err != nil {
			return HumanRequest{}, false, fmt.Errorf("%w: invalid human delivery time", ErrCorruptState)
		}
		result.DeliveryStartedAt = &value
	}
	if rawResolution.Valid {
		resolution, err := parseHumanRequestResolution(rawResolution.String)
		if err != nil {
			return HumanRequest{}, false, err
		}
		result.Resolution = &resolution
	}
	if closedAt.Valid {
		value, err := NewUnixMillis(closedAt.Int64)
		if err != nil {
			return HumanRequest{}, false, fmt.Errorf("%w: invalid human request close time", ErrCorruptState)
		}
		result.ClosedAt = &value
	}
	if err := validateHumanRequestState(result); err != nil {
		return HumanRequest{}, false, err
	}
	return result, true, nil
}

func validateHumanRequestState(request HumanRequest) error {
	if request.Kind != HumanRequestQuestion || request.Status.String() == "" || request.Revision.Int64() < 1 || request.CreatedAt.Int64() > request.UpdatedAt.Int64() {
		return fmt.Errorf("%w: invalid human request controls", ErrCorruptState)
	}
	withDelivery := request.DeliveryID != nil
	if (request.DeliveryStartedAt != nil) != withDelivery {
		return fmt.Errorf("%w: human delivery nullability mismatch", ErrCorruptState)
	}
	if request.DeliveryStartedAt != nil && (request.DeliveryStartedAt.Int64() < request.CreatedAt.Int64() || request.DeliveryStartedAt.Int64() > request.UpdatedAt.Int64()) {
		return fmt.Errorf("%w: invalid human delivery chronology", ErrCorruptState)
	}
	if request.ClosedAt != nil && (request.ClosedAt.Int64() < request.CreatedAt.Int64() || request.ClosedAt.Int64() > request.UpdatedAt.Int64()) {
		return fmt.Errorf("%w: invalid human request close chronology", ErrCorruptState)
	}
	switch request.Status {
	case HumanRequestOpen:
		if withDelivery || request.Resolution != nil || request.ClosedAt != nil {
			return fmt.Errorf("%w: invalid open human request", ErrCorruptState)
		}
	case HumanRequestDelivering, HumanRequestDeliveryUnknown:
		if !withDelivery || request.Resolution != nil || request.ClosedAt != nil {
			return fmt.Errorf("%w: invalid pending human delivery", ErrCorruptState)
		}
	case HumanRequestResolved:
		if request.Resolution == nil || (*request.Resolution != HumanRequestResolutionReply && *request.Resolution != HumanRequestResolutionCancelRun) || *request.Resolution == HumanRequestResolutionReply && !withDelivery || *request.Resolution == HumanRequestResolutionCancelRun && withDelivery || request.ClosedAt == nil || request.ClosedAt.Int64() != request.UpdatedAt.Int64() {
			return fmt.Errorf("%w: invalid resolved human request", ErrCorruptState)
		}
	case HumanRequestStale:
		if request.Resolution == nil || *request.Resolution != HumanRequestResolutionStale || request.ClosedAt == nil || request.ClosedAt.Int64() != request.UpdatedAt.Int64() {
			return fmt.Errorf("%w: invalid stale human request", ErrCorruptState)
		}
	default:
		return fmt.Errorf("%w: unknown human request status", ErrCorruptState)
	}
	return nil
}

func humanRequestByID(ctx context.Context, connection *sql.Conn, id HumanRequestID) (HumanRequest, bool, error) {
	if id.zero() {
		return HumanRequest{}, false, fmt.Errorf("%w: zero human request identifier", ErrInvalidValue)
	}
	return scanHumanRequest(connection.QueryRowContext(ctx, `SELECT `+humanRequestColumns+` FROM human_requests WHERE id = ?`, id.Bytes()))
}

func (store *Store) CreateHumanQuestionForAttempt(ctx context.Context, digest AttemptDigest, input NewHumanQuestion, at UnixMillis) (HumanRequest, error) {
	return store.createHumanQuestionForAttempt(ctx, digest, input, at, MaxOpenHumanRequests)
}

// createHumanQuestionForAttempt keeps the cap as an argument only for bounded
// package tests; production always uses MaxOpenHumanRequests above.
func (store *Store) createHumanQuestionForAttempt(ctx context.Context, digest AttemptDigest, input NewHumanQuestion, at UnixMillis, openLimit int64) (HumanRequest, error) {
	if err := input.valid(); err != nil {
		return HumanRequest{}, err
	}
	if openLimit < 1 {
		return HumanRequest{}, fmt.Errorf("%w: invalid human request bound", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return HumanRequest{}, err
	}
	defer tx.Close()
	run, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil {
		return HumanRequest{}, tx.Rollback(err)
	}
	if !found || run.Phase != RunRunning || run.CredentialRevokedAt != nil {
		return HumanRequest{}, tx.Rollback(ErrUnauthorized)
	}
	if at.Int64() < run.UpdatedAt.Int64() {
		return HumanRequest{}, tx.Rollback(ErrRevisionConflict)
	}
	var existing HumanRequest
	var existingFound bool
	existing, existingFound, err = humanRequestByKey(ctx, tx.connection, run.ID, input.IdempotencyKey)
	if err != nil {
		return HumanRequest{}, tx.Rollback(err)
	}
	if existingFound {
		if existing.QuestionText != input.QuestionText {
			return HumanRequest{}, tx.Rollback(ErrConflict)
		}
		if err := tx.Rollback(nil); err != nil {
			return HumanRequest{}, err
		}
		return existing, nil
	}
	var open int64
	if err := tx.connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM human_requests WHERE status IN ('open', 'delivering', 'delivery_unknown')`).Scan(&open); err != nil {
		return HumanRequest{}, tx.Rollback(err)
	}
	if open >= openLimit {
		return HumanRequest{}, tx.Rollback(ErrBusy)
	}
	var rawID [IDBytes]byte
	if _, err := rand.Read(rawID[:]); err != nil || rawID == [IDBytes]byte{} {
		if err == nil {
			err = fmt.Errorf("%w: generated zero human request identifier", ErrCorruptState)
		}
		return HumanRequest{}, tx.Rollback(err)
	}
	inserted, err := tx.connection.ExecContext(ctx, `INSERT INTO human_requests(id, run_id, idempotency_key, kind, reason_code, question_text, status, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, 'question', 'provider_question', ?, 'open', 1, ?, ?)`, rawID[:], run.ID.Bytes(), input.IdempotencyKey[:], input.QuestionText, at.Int64(), at.Int64())
	if err := requireOneRow(inserted, err); err != nil {
		return HumanRequest{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityHumanRequest, id: rawID[:], revision: 1}}); err != nil {
		return HumanRequest{}, tx.Rollback(err)
	}
	requestID, err := HumanRequestIDFromBytes(rawID[:])
	if err != nil {
		return HumanRequest{}, tx.Rollback(err)
	}
	request, found, err := humanRequestByID(ctx, tx.connection, requestID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return HumanRequest{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return HumanRequest{}, err
	}
	return request, nil
}

func humanRequestByKey(ctx context.Context, connection *sql.Conn, runID RunID, key [IDBytes]byte) (HumanRequest, bool, error) {
	return scanHumanRequest(connection.QueryRowContext(ctx, `SELECT `+humanRequestColumns+` FROM human_requests WHERE run_id = ? AND idempotency_key = ?`, runID.Bytes(), key[:]))
}

func (store *Store) HumanRequest(ctx context.Context, id HumanRequestID) (HumanRequestProjection, bool, error) {
	tx, err := store.beginRead(ctx)
	if err != nil {
		return HumanRequestProjection{}, false, err
	}
	defer tx.Close()
	return humanRequestProjectionByID(ctx, tx.connection, id)
}

// CancelHumanRequestRun is the sole daemon-authorized HumanRequest cancellation.
// Client capability, exact request/run revisions and the finalizing/revocation
// transition are one SQLite transaction; callers cannot split or replay it.
func (store *Store) CancelHumanRequestRun(ctx context.Context, clientID BrowserClientID, requestID HumanRequestID, expectedRequest, expectedRun Revision, at UnixMillis) (Run, HumanRequest, error) {
	if requestID.zero() || expectedRequest.Int64() < 1 || expectedRun.Int64() < 1 {
		return Run{}, HumanRequest{}, fmt.Errorf("%w: invalid human request cancellation", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, HumanRequest{}, err
	}
	defer tx.Close()
	client, found, err := browserClientByID(ctx, tx.connection, clientID)
	if err != nil {
		return Run{}, HumanRequest{}, tx.Rollback(err)
	}
	if !found || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityHumanActions) {
		return Run{}, HumanRequest{}, tx.Rollback(ErrUnauthorized)
	}
	request, found, err := humanRequestByID(ctx, tx.connection, requestID)
	if err != nil {
		return Run{}, HumanRequest{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, HumanRequest{}, tx.Rollback(ErrNotFound)
	}
	run, found, err := runByID(ctx, tx.connection, request.RunID)
	if err != nil {
		return Run{}, HumanRequest{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, HumanRequest{}, tx.Rollback(corruptControl("human request run", request.RunID.String()))
	}
	if request.Status != HumanRequestOpen || request.Revision != expectedRequest || run.Phase != RunRunning || run.Revision != expectedRun || at.Int64() < request.UpdatedAt.Int64() || at.Int64() < run.UpdatedAt.Int64() {
		return Run{}, HumanRequest{}, tx.Rollback(ErrRevisionConflict)
	}
	proposal, err := NewCancelledProposal("cancelled by human request")
	if err != nil {
		return Run{}, HumanRequest{}, tx.Rollback(err)
	}
	updatedRun, err := store.enterFinalizing(ctx, tx, run, expectedRun, proposal, at, &request.ID)
	if err != nil {
		return Run{}, HumanRequest{}, err
	}
	// enterFinalizing commits the shared transaction. Construct
	// the exact post-transition request from the validated source row rather
	// than opening a second write or read transaction before its close.
	updatedRequest := request
	updatedRequest.Status = HumanRequestResolved
	resolution := HumanRequestResolutionCancelRun
	updatedRequest.Resolution = &resolution
	closed := at
	updatedRequest.ClosedAt = &closed
	updatedRequest.Revision, err = NewRevision(request.Revision.Int64() + 1)
	if err != nil {
		return Run{}, HumanRequest{}, err
	}
	updatedRequest.UpdatedAt = at
	return updatedRun, updatedRequest, nil
}

func humanRequestProjections(ctx context.Context, connection *sql.Conn, limit int) ([]HumanRequestProjection, error) {
	rows, err := connection.QueryContext(ctx, `SELECT id FROM human_requests WHERE status IN ('open', 'delivering', 'delivery_unknown') ORDER BY created_at_ms ASC, id ASC LIMIT ?`, limit+1)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var ids []HumanRequestID
	for rows.Next() {
		var rawID []byte
		if err := rows.Scan(&rawID); err != nil {
			return nil, err
		}
		id, err := HumanRequestIDFromBytes(rawID)
		if err != nil {
			return nil, fmt.Errorf("%w: invalid human request projection identity", ErrCorruptState)
		}
		ids = append(ids, id)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	if len(ids) > limit {
		return nil, ErrSnapshotTooLarge
	}
	result := make([]HumanRequestProjection, 0, len(ids))
	for _, id := range ids {
		projection, found, err := humanRequestProjectionByID(ctx, connection, id)
		if err != nil {
			return nil, err
		}
		if !found {
			return nil, fmt.Errorf("%w: human request disappeared during snapshot", ErrCorruptState)
		}
		result = append(result, projection)
	}
	return result, nil
}

func humanRequestProjectionByID(ctx context.Context, connection *sql.Conn, id HumanRequestID) (HumanRequestProjection, bool, error) {
	if id.zero() {
		return HumanRequestProjection{}, false, fmt.Errorf("%w: zero human request identifier", ErrInvalidValue)
	}
	var rawID, rawProjectID, rawAgentID, rawTaskID []byte
	var kindValue, statusValue, runPhaseValue string
	var createdAt, updatedAt, revision int64
	err := connection.QueryRowContext(ctx, `SELECT h.id, p.id, a.id, t.id, h.kind, h.status, r.phase, h.created_at_ms, h.updated_at_ms, h.revision
        FROM human_requests h JOIN runs r ON r.id = h.run_id
        JOIN projects p ON p.id = r.project_id
        JOIN agents a ON a.id = r.agent_id AND a.project_id = r.project_id
        JOIN tasks t ON t.id = r.task_id AND t.project_id = r.project_id AND t.incarnation_id = r.task_incarnation_id
		WHERE h.id = ?`, id.Bytes()).Scan(&rawID, &rawProjectID, &rawAgentID, &rawTaskID, &kindValue, &statusValue, &runPhaseValue, &createdAt, &updatedAt, &revision)
	if errors.Is(err, sql.ErrNoRows) {
		return HumanRequestProjection{}, false, nil
	}
	if err != nil {
		return HumanRequestProjection{}, false, fmt.Errorf("read human request projection: %w", err)
	}
	requestID, idErr := HumanRequestIDFromBytes(rawID)
	projectID, projectErr := ProjectIDFromBytes(rawProjectID)
	agentID, agentErr := AgentIDFromBytes(rawAgentID)
	taskID, taskErr := TaskIDFromBytes(rawTaskID)
	kind, kindErr := parseHumanRequestKind(kindValue)
	status, statusErr := parseHumanRequestStatus(statusValue)
	runPhase, runPhaseErr := parseRunPhase(runPhaseValue)
	created, createdErr := NewUnixMillis(createdAt)
	updated, updatedErr := NewUnixMillis(updatedAt)
	rev, revErr := NewRevision(revision)
	if idErr != nil || projectErr != nil || agentErr != nil || taskErr != nil || kindErr != nil || statusErr != nil || runPhaseErr != nil || createdErr != nil || updatedErr != nil || revErr != nil || updatedAt < createdAt || (status == HumanRequestOpen || status == HumanRequestDelivering) && runPhase != RunRunning {
		return HumanRequestProjection{}, false, fmt.Errorf("%w: invalid human request projection", ErrCorruptState)
	}
	return HumanRequestProjection{ID: requestID, ProjectID: projectID, AgentID: agentID, TaskID: taskID, CreatedAt: created, UpdatedAt: updated, Revision: rev, Kind: kind, Status: status, ReplyMaxBytes: MaxHumanRequestReplyBytes, CanReply: status == HumanRequestOpen && runPhase == RunRunning}, true, nil
}

func (store *Store) HumanRequestDetail(ctx context.Context, clientID BrowserClientID, id HumanRequestID, expected Revision) (HumanRequestDetail, error) {
	if id.zero() || expected.Int64() < 1 {
		return HumanRequestDetail{}, fmt.Errorf("%w: invalid human request detail locator", ErrInvalidValue)
	}
	tx, err := store.beginRead(ctx)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	defer tx.Close()
	return humanRequestDetail(ctx, tx.connection, clientID, id, expected)
}

func humanRequestDetail(ctx context.Context, connection *sql.Conn, clientID BrowserClientID, id HumanRequestID, expected Revision) (HumanRequestDetail, error) {
	client, found, err := browserClientByID(ctx, connection, clientID)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	if !found || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityPrivateHumanRequestDetail) {
		return HumanRequestDetail{}, ErrUnauthorized
	}
	request, found, err := humanRequestByID(ctx, connection, id)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	if !found {
		return HumanRequestDetail{}, ErrNotFound
	}
	if request.Status == HumanRequestResolved || request.Status == HumanRequestStale {
		return HumanRequestDetail{}, ErrConflict
	}
	if request.Revision != expected {
		return HumanRequestDetail{}, ErrRevisionConflict
	}
	detail := HumanRequestDetail{ID: request.ID, Revision: request.Revision, QuestionText: request.QuestionText, ReplyMaxBytes: MaxHumanRequestReplyBytes}
	if request.Status != HumanRequestOpen {
		return detail, nil
	}
	run, found, err := runByID(ctx, connection, request.RunID)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	if !found {
		return HumanRequestDetail{}, fmt.Errorf("%w: human request origin missing", ErrCorruptState)
	}
	if run.Phase != RunRunning {
		return detail, nil
	}
	session, found, err := terminalSessionByRunID(ctx, connection, run.ID)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	if !found || session.State != TerminalSessionActive {
		return detail, nil
	}
	if err := validateTerminalSessionLease(ctx, connection, run, session); err != nil {
		return HumanRequestDetail{}, err
	}
	relationships, err := loadRunRelationships(ctx, connection, run)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	target, err := humanRequestTerminalTarget(run, session, relationships.session)
	if err != nil {
		return HumanRequestDetail{}, err
	}
	detail.TerminalTarget = &target
	if client.CapabilityMask.Has(BrowserCapabilityHumanActions) {
		detail.CanReply = true
		detail.CancelRun = &HumanRequestCancelRun{expectedRequestRevision: request.Revision, expectedRunRevision: run.Revision}
	}
	return detail, nil
}

func humanRequestTerminalTarget(run Run, selected, validated TerminalSession) (TerminalTarget, error) {
	if !sameTerminalSession(validated, selected) {
		return TerminalTarget{}, fmt.Errorf("%w: selected human request terminal session changed", ErrCorruptState)
	}
	return newTerminalTarget(run.ProjectID, run.AgentID, run, validated)
}

func (store *Store) BeginHumanReply(ctx context.Context, clientID BrowserClientID, requestID HumanRequestID, expected Revision, deliveryID HumanRequestDeliveryID, reply string, at UnixMillis) (HumanDelivery, error) {
	if requestID.zero() || deliveryID.zero() || expected.Int64() < 1 || !utf8TextWithin(reply, 1, MaxHumanRequestReplyBytes) {
		return HumanDelivery{}, fmt.Errorf("%w: invalid human request reply", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return HumanDelivery{}, err
	}
	defer tx.Close()
	client, found, err := browserClientByID(ctx, tx.connection, clientID)
	if err != nil {
		return HumanDelivery{}, tx.Rollback(err)
	}
	if !found || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityHumanActions) {
		return HumanDelivery{}, tx.Rollback(ErrUnauthorized)
	}
	return store.beginHumanReplyTx(ctx, tx, requestID, expected, deliveryID, reply, at, ProjectID{}, RunID{})
}

// BeginHumanReplyForAttempt answers a worker question only from a distinct,
// live orchestrator in the same project. The authority check and delivery
// reservation share one transaction with the target request.
func (store *Store) BeginHumanReplyForAttempt(ctx context.Context, digest AttemptDigest, requestID HumanRequestID, expected Revision, deliveryID HumanRequestDeliveryID, reply string, at UnixMillis) (HumanDelivery, error) {
	if requestID.zero() || deliveryID.zero() || expected.Int64() < 1 || !utf8TextWithin(reply, 1, MaxHumanRequestReplyBytes) {
		return HumanDelivery{}, fmt.Errorf("%w: invalid human request reply", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return HumanDelivery{}, err
	}
	defer tx.Close()
	actor, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil {
		return HumanDelivery{}, tx.Rollback(err)
	}
	if !found || actor.Role != RoleOrchestrator || actor.Phase != RunRunning || actor.CredentialRevokedAt != nil {
		return HumanDelivery{}, tx.Rollback(ErrUnauthorized)
	}
	return store.beginHumanReplyTx(ctx, tx, requestID, expected, deliveryID, reply, at, actor.ProjectID, actor.ID)
}

func (store *Store) beginHumanReplyTx(ctx context.Context, tx *writeTx, requestID HumanRequestID, expected Revision, deliveryID HumanRequestDeliveryID, reply string, at UnixMillis, actorProject ProjectID, actorRun RunID) (HumanDelivery, error) {
	request, found, err := humanRequestByID(ctx, tx.connection, requestID)
	if err != nil {
		return HumanDelivery{}, tx.Rollback(err)
	}
	if !found {
		return HumanDelivery{}, tx.Rollback(ErrNotFound)
	}
	run, found, err := runByID(ctx, tx.connection, request.RunID)
	if err != nil {
		return HumanDelivery{}, tx.Rollback(err)
	}
	if !found {
		return HumanDelivery{}, tx.Rollback(ErrCorruptState)
	}
	if !actorProject.zero() && (run.ProjectID != actorProject || run.ID == actorRun) {
		return HumanDelivery{}, tx.Rollback(ErrUnauthorized)
	}
	var deliveryCollision int
	if err := tx.connection.QueryRowContext(ctx, `SELECT 1 FROM human_requests WHERE delivery_id = ?`, deliveryID.Bytes()).Scan(&deliveryCollision); err == nil {
		return HumanDelivery{}, tx.Rollback(ErrConflict)
	} else if !errors.Is(err, sql.ErrNoRows) {
		return HumanDelivery{}, tx.Rollback(err)
	}
	if request.Status != HumanRequestOpen || request.Revision != expected || run.Phase != RunRunning || at.Int64() < request.UpdatedAt.Int64() || at.Int64() < run.UpdatedAt.Int64() {
		return HumanDelivery{}, tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE human_requests SET status = 'delivering', delivery_id = ?, delivery_started_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'open' AND revision = ?`, deliveryID.Bytes(), at.Int64(), at.Int64(), request.ID.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return HumanDelivery{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityHumanRequest, id: request.ID.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return HumanDelivery{}, tx.Rollback(err)
	}
	request, found, err = humanRequestByID(ctx, tx.connection, request.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return HumanDelivery{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return HumanDelivery{}, err
	}
	return HumanDelivery{RequestID: request.ID, RunID: request.RunID, Provider: run.Provider, DeliveryID: deliveryID, Revision: request.Revision, Reply: []byte(reply)}, nil
}

func (store *Store) AcknowledgeHumanReply(ctx context.Context, requestID HumanRequestID, deliveryID HumanRequestDeliveryID, expected Revision, at UnixMillis) error {
	if requestID.zero() || deliveryID.zero() || expected.Int64() < 1 {
		return fmt.Errorf("%w: invalid human delivery acknowledgement", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return err
	}
	defer tx.Close()
	request, found, err := humanRequestByID(ctx, tx.connection, requestID)
	if err != nil {
		return tx.Rollback(err)
	}
	if !found {
		return tx.Rollback(ErrNotFound)
	}
	if request.Status == HumanRequestResolved && request.DeliveryID != nil && *request.DeliveryID == deliveryID {
		return tx.Rollback(nil)
	}
	run, found, err := runByID(ctx, tx.connection, request.RunID)
	if err != nil {
		return tx.Rollback(err)
	}
	if !found {
		return tx.Rollback(ErrCorruptState)
	}
	if request.Status != HumanRequestDelivering || request.DeliveryID == nil || *request.DeliveryID != deliveryID || request.Revision != expected || run.Phase != RunRunning || at.Int64() < request.UpdatedAt.Int64() || at.Int64() < run.UpdatedAt.Int64() {
		return tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE human_requests SET status = 'resolved', resolution_kind = 'reply', closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'delivering' AND delivery_id = ? AND revision = ?`, at.Int64(), at.Int64(), request.ID.Bytes(), deliveryID.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityHumanRequest, id: request.ID.Bytes(), revision: expected.Int64() + 1, deleted: true}}); err != nil {
		return tx.Rollback(err)
	}
	return tx.Commit(ctx)
}

func (store *Store) MarkHumanDeliveryUnknown(ctx context.Context, requestID HumanRequestID, deliveryID HumanRequestDeliveryID, expected Revision, at UnixMillis) error {
	if requestID.zero() || deliveryID.zero() || expected.Int64() < 1 {
		return fmt.Errorf("%w: invalid human delivery uncertainty", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return err
	}
	defer tx.Close()
	request, found, err := humanRequestByID(ctx, tx.connection, requestID)
	if err != nil {
		return tx.Rollback(err)
	}
	if !found {
		return tx.Rollback(ErrNotFound)
	}
	if request.Status == HumanRequestDeliveryUnknown && request.DeliveryID != nil && *request.DeliveryID == deliveryID && request.Revision.Int64() == expected.Int64()+1 {
		return tx.Rollback(nil)
	}
	if request.Status != HumanRequestDelivering || request.DeliveryID == nil || *request.DeliveryID != deliveryID || request.Revision != expected || at.Int64() < request.UpdatedAt.Int64() {
		return tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE human_requests SET status = 'delivery_unknown', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'delivering' AND delivery_id = ? AND revision = ?`, at.Int64(), request.ID.Bytes(), deliveryID.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityHumanRequest, id: request.ID.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return tx.Rollback(err)
	}
	return tx.Commit(ctx)
}

// RecoverHumanDeliveries is called explicitly during daemon startup. It never
// replays a reply; a receipt stranded in delivering is permanently uncertain.
func (store *Store) RecoverHumanDeliveries(ctx context.Context, at UnixMillis) (int, error) {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return 0, err
	}
	defer tx.Close()
	rows, err := tx.connection.QueryContext(ctx, `SELECT `+humanRequestColumns+` FROM human_requests WHERE status = 'delivering' ORDER BY id`)
	if err != nil {
		return 0, tx.Rollback(err)
	}
	var requests []HumanRequest
	for rows.Next() {
		request, _, scanErr := scanHumanRequest(rows)
		if scanErr != nil {
			rows.Close()
			return 0, tx.Rollback(scanErr)
		}
		requests = append(requests, request)
	}
	if err := rows.Close(); err != nil {
		return 0, tx.Rollback(err)
	}
	for _, request := range requests {
		if at.Int64() < request.UpdatedAt.Int64() {
			return 0, tx.Rollback(ErrRevisionConflict)
		}
		updated, err := tx.connection.ExecContext(ctx, `UPDATE human_requests SET status = 'delivery_unknown', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'delivering' AND revision = ?`, at.Int64(), request.ID.Bytes(), request.Revision.Int64())
		if err := requireOneRow(updated, err); err != nil {
			return 0, tx.Rollback(err)
		}
		if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityHumanRequest, id: request.ID.Bytes(), revision: request.Revision.Int64() + 1}}); err != nil {
			return 0, tx.Rollback(err)
		}
	}
	if err := tx.Commit(ctx); err != nil {
		return 0, err
	}
	return len(requests), nil
}

// transitionHumanRequestsForRun is called from the run transition transaction.
// It never delivers or removes a request: it only makes the loss of its exact
// running origin durable before the run transition becomes observable.
func transitionHumanRequestsForRun(ctx context.Context, connection *sql.Conn, runID RunID, at UnixMillis, terminal bool, cancelRequest *HumanRequestID) ([]pendingInvalidation, error) {
	rows, err := connection.QueryContext(ctx, `SELECT id, status, revision, updated_at_ms FROM human_requests WHERE run_id = ? AND status IN ('open', 'delivering', 'delivery_unknown') ORDER BY id`, runID.Bytes())
	if err != nil {
		return nil, err
	}
	type candidate struct {
		id       HumanRequestID
		status   HumanRequestStatus
		revision Revision
		updated  UnixMillis
	}
	var candidates []candidate
	for rows.Next() {
		var rawID []byte
		var rawStatus string
		var revision, updated int64
		if err := rows.Scan(&rawID, &rawStatus, &revision, &updated); err != nil {
			rows.Close()
			return nil, err
		}
		id, idErr := HumanRequestIDFromBytes(rawID)
		status, statusErr := parseHumanRequestStatus(rawStatus)
		rev, revErr := NewRevision(revision)
		when, whenErr := NewUnixMillis(updated)
		if idErr != nil || statusErr != nil || revErr != nil || whenErr != nil {
			rows.Close()
			return nil, fmt.Errorf("%w: invalid human request transition row", ErrCorruptState)
		}
		candidates = append(candidates, candidate{id: id, status: status, revision: rev, updated: when})
	}
	if err := rows.Close(); err != nil {
		return nil, err
	}
	pending := make([]pendingInvalidation, 0, len(candidates))
	for _, item := range candidates {
		if at.Int64() < item.updated.Int64() {
			return nil, ErrRevisionConflict
		}
		target := item.status
		resolution, closed := "", any(nil)
		isCancel := cancelRequest != nil && item.id == *cancelRequest
		if isCancel {
			if item.status != HumanRequestOpen || terminal {
				return nil, ErrRevisionConflict
			}
			target = HumanRequestResolved
			resolution, closed = HumanRequestResolutionCancelRun.String(), at.Int64()
		}
		switch item.status {
		case HumanRequestOpen:
			if isCancel {
				break
			}
			target = HumanRequestStale
			resolution, closed = HumanRequestResolutionStale.String(), at.Int64()
		case HumanRequestDelivering:
			if terminal {
				target = HumanRequestStale
				resolution, closed = HumanRequestResolutionStale.String(), at.Int64()
			} else {
				target = HumanRequestDeliveryUnknown
			}
		case HumanRequestDeliveryUnknown:
			if terminal {
				target = HumanRequestStale
				resolution, closed = HumanRequestResolutionStale.String(), at.Int64()
			}
		}
		if target == item.status {
			continue
		}
		var result sql.Result
		if target == HumanRequestResolved {
			result, err = connection.ExecContext(ctx, `UPDATE human_requests SET status = 'resolved', resolution_kind = ?, closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'open' AND revision = ?`, resolution, closed, at.Int64(), item.id.Bytes(), item.revision.Int64())
		} else if target == HumanRequestStale {
			result, err = connection.ExecContext(ctx, `UPDATE human_requests SET status = 'stale', resolution_kind = ?, closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = ? AND revision = ?`, resolution, closed, at.Int64(), item.id.Bytes(), item.status.String(), item.revision.Int64())
		} else {
			result, err = connection.ExecContext(ctx, `UPDATE human_requests SET status = 'delivery_unknown', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'delivering' AND revision = ?`, at.Int64(), item.id.Bytes(), item.revision.Int64())
		}
		if err := requireOneRow(result, err); err != nil {
			return nil, err
		}
		pending = append(pending, pendingInvalidation{kind: EntityHumanRequest, id: item.id.Bytes(), revision: item.revision.Int64() + 1, deleted: target == HumanRequestStale || target == HumanRequestResolved})
	}
	return pending, nil
}
