package kernel

import (
	"bytes"
	"context"
	"crypto/rand"
	"database/sql"
	"errors"
	"fmt"
)

const peerQuestionColumns = `id, project_id, source_task_id, target_task_id, idempotency_key, question_text,
    answer_idempotency_key, answer_text, recipient_delivery_id, recipient_delivery_state,
    answer_delivery_id, answer_delivery_state, revision, created_at_ms, updated_at_ms`

func scanPeerQuestion(scanner rowScanner) (PeerQuestion, bool, error) {
	var rawID, rawProject, rawSource, rawTarget, rawKey []byte
	var rawAnswerKey, rawRecipient, rawAnswerDelivery nullableBlob
	var question string
	var answer sql.NullString
	var recipientState, answerState string
	var revision, created, updated int64
	if err := scanner.Scan(&rawID, &rawProject, &rawSource, &rawTarget, &rawKey, &question,
		&rawAnswerKey, &answer, &rawRecipient, &recipientState, &rawAnswerDelivery, &answerState, &revision, &created, &updated); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return PeerQuestion{}, false, nil
		}
		return PeerQuestion{}, false, fmt.Errorf("scan peer question: %w", err)
	}
	id, e1 := PeerQuestionIDFromBytes(rawID)
	project, e2 := ProjectIDFromBytes(rawProject)
	source, e3 := TaskIDFromBytes(rawSource)
	target, e4 := TaskIDFromBytes(rawTarget)
	rev, e5 := NewRevision(revision)
	createdAt, e6 := NewUnixMillis(created)
	updatedAt, e7 := NewUnixMillis(updated)
	recipientStateValue, e8 := parsePeerDeliveryState(recipientState)
	answerStateValue, e9 := parsePeerDeliveryState(answerState)
	if e1 != nil || e2 != nil || e3 != nil || e4 != nil || e5 != nil || e6 != nil || e7 != nil || e8 != nil || e9 != nil || source == target || len(rawKey) != IDBytes || bytes.Equal(rawKey, make([]byte, IDBytes)) || !utf8TextWithin(question, 1, MaxPeerQuestionBytes) || !answer.Valid && rawAnswerKey.valid || answer.Valid != rawAnswerKey.valid || answer.Valid && !utf8TextWithin(answer.String, 1, MaxPeerQuestionBytes) || createdAt.Int64() > updatedAt.Int64() {
		return PeerQuestion{}, false, fmt.Errorf("%w: invalid peer question row", ErrCorruptState)
	}
	var key [IDBytes]byte
	copy(key[:], rawKey)
	result := PeerQuestion{ID: id, ProjectID: project, SourceTaskID: source, TargetTaskID: target, IdempotencyKey: key, Question: question, RecipientDeliveryState: recipientStateValue, AnswerDeliveryState: answerStateValue, Revision: rev, CreatedAt: createdAt, UpdatedAt: updatedAt}
	if rawAnswerKey.valid {
		var value [IDBytes]byte
		if len(rawAnswerKey.bytes) != IDBytes || bytes.Equal(rawAnswerKey.bytes, make([]byte, IDBytes)) {
			return PeerQuestion{}, false, fmt.Errorf("%w: invalid peer answer key", ErrCorruptState)
		}
		copy(value[:], rawAnswerKey.bytes)
		result.AnswerIdempotencyKey = &value
		result.Answer = answer.String
	}
	if rawRecipient.valid {
		value, err := PeerDeliveryIDFromBytes(rawRecipient.bytes)
		if err != nil {
			return PeerQuestion{}, false, err
		}
		result.RecipientDeliveryID = &value
	}
	if rawAnswerDelivery.valid {
		value, err := PeerDeliveryIDFromBytes(rawAnswerDelivery.bytes)
		if err != nil {
			return PeerQuestion{}, false, err
		}
		result.AnswerDeliveryID = &value
	}
	if (result.RecipientDeliveryID == nil) != (result.RecipientDeliveryState == PeerDeliveryPending) || (!answer.Valid && (result.AnswerDeliveryID != nil || result.AnswerDeliveryState != PeerDeliveryPending)) || (result.AnswerDeliveryID == nil && answer.Valid && result.AnswerDeliveryState != PeerDeliveryPending) || (result.AnswerDeliveryID != nil && result.AnswerDeliveryState == PeerDeliveryPending) {
		return PeerQuestion{}, false, fmt.Errorf("%w: invalid peer delivery state", ErrCorruptState)
	}
	return result, true, nil
}

func peerQuestionByID(ctx context.Context, connection *sql.Conn, id PeerQuestionID) (PeerQuestion, bool, error) {
	if id.zero() {
		return PeerQuestion{}, false, fmt.Errorf("%w: zero peer question identifier", ErrInvalidValue)
	}
	return scanPeerQuestion(connection.QueryRowContext(ctx, `SELECT `+peerQuestionColumns+` FROM peer_questions WHERE id = ?`, id.Bytes()))
}

func peerQuestionByKey(ctx context.Context, connection *sql.Conn, taskID TaskID, key [IDBytes]byte) (PeerQuestion, bool, error) {
	return scanPeerQuestion(connection.QueryRowContext(ctx, `SELECT `+peerQuestionColumns+` FROM peer_questions WHERE source_task_id = ? AND idempotency_key = ?`, taskID.Bytes(), key[:]))
}

// CreatePeerQuestionForAttempt stores an asynchronous question. It grants no
// terminal authority to either worker and never waits for the recipient.
func (store *Store) CreatePeerQuestionForAttempt(ctx context.Context, digest AttemptDigest, input NewPeerQuestion, at UnixMillis) (PeerQuestion, error) {
	if err := input.valid(); err != nil {
		return PeerQuestion{}, err
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return PeerQuestion{}, err
	}
	defer tx.Close()
	run, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil || !found {
		if err == nil {
			err = ErrUnauthorized
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if run.Role != RoleWorker || run.Phase != RunRunning || run.CredentialRevokedAt != nil || at.Int64() < run.UpdatedAt.Int64() {
		return PeerQuestion{}, tx.Rollback(ErrUnauthorized)
	}
	existing, found, err := peerQuestionByKey(ctx, tx.connection, run.TaskID, input.IdempotencyKey)
	if err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	if found {
		if existing.TargetTaskID != input.TargetTaskID || existing.Question != input.Question {
			return PeerQuestion{}, tx.Rollback(ErrConflict)
		}
		if err := tx.Rollback(nil); err != nil {
			return PeerQuestion{}, err
		}
		return existing, nil
	}
	target, found, err := taskByID(ctx, tx.connection, input.TargetTaskID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	agent, found, err := agentByID(ctx, tx.connection, target.AssignedAgentID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if target.ProjectID != run.ProjectID || target.ID == run.TaskID || agent.Role != RoleWorker || agent.Provider != ProviderCodex || (target.Status != TaskQueued && target.Status != TaskRunning) {
		return PeerQuestion{}, tx.Rollback(ErrUnauthorized)
	}
	var raw [IDBytes]byte
	if _, err := rand.Read(raw[:]); err != nil || raw == [IDBytes]byte{} {
		if err == nil {
			err = ErrCorruptState
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if _, err := tx.connection.ExecContext(ctx, `INSERT INTO peer_questions(id, project_id, source_task_id, target_task_id, idempotency_key, question_text, recipient_delivery_state, answer_delivery_state, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, ?, ?, ?, 'pending', 'pending', 1, ?, ?)`, raw[:], run.ProjectID.Bytes(), run.TaskID.Bytes(), target.ID.Bytes(), input.IdempotencyKey[:], input.Question, at.Int64(), at.Int64()); err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityPeerQuestion, id: raw[:], revision: 1}}); err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	id, err := PeerQuestionIDFromBytes(raw[:])
	if err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	result, found, err := peerQuestionByID(ctx, tx.connection, id)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return PeerQuestion{}, err
	}
	return result, nil
}

// AnswerPeerQuestionForAttempt lets only the live worker which owns the
// linked target task append the one immutable answer.
func (store *Store) AnswerPeerQuestionForAttempt(ctx context.Context, digest AttemptDigest, input PeerAnswer, at UnixMillis) (PeerQuestion, error) {
	if err := input.valid(); err != nil {
		return PeerQuestion{}, err
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return PeerQuestion{}, err
	}
	defer tx.Close()
	run, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil || !found {
		if err == nil {
			err = ErrUnauthorized
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if run.Role != RoleWorker || run.Phase != RunRunning || run.CredentialRevokedAt != nil || at.Int64() < run.UpdatedAt.Int64() {
		return PeerQuestion{}, tx.Rollback(ErrUnauthorized)
	}
	question, found, err := peerQuestionByID(ctx, tx.connection, input.QuestionID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if question.ProjectID != run.ProjectID || question.TargetTaskID != run.TaskID || at.Int64() < question.UpdatedAt.Int64() {
		return PeerQuestion{}, tx.Rollback(ErrRevisionConflict)
	}
	if question.AnswerIdempotencyKey != nil {
		if *question.AnswerIdempotencyKey != input.IdempotencyKey || question.Answer != input.Answer {
			return PeerQuestion{}, tx.Rollback(ErrConflict)
		}
		if err := tx.Rollback(nil); err != nil {
			return PeerQuestion{}, err
		}
		return question, nil
	}
	if question.Revision != input.Expected {
		return PeerQuestion{}, tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE peer_questions SET answer_idempotency_key=?, answer_text=?, revision=revision+1, updated_at_ms=? WHERE id=? AND revision=?`, input.IdempotencyKey[:], input.Answer, at.Int64(), question.ID.Bytes(), input.Expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityPeerQuestion, id: question.ID.Bytes(), revision: question.Revision.Int64() + 1}}); err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	result, found, err := peerQuestionByID(ctx, tx.connection, question.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return PeerQuestion{}, err
	}
	return result, nil
}

// PeerQuestionsForTask is bounded task-linked history. Browser callers must
// first authorize their private task-detail read before using this projection.
func (store *Store) PeerQuestionsForTask(ctx context.Context, taskID TaskID, offset uint64) ([]PeerQuestion, *uint64, error) {
	if offset > uint64(^uint64(0)>>1)-1 {
		return nil, nil, ErrInvalidValue
	}
	read, err := store.beginRead(ctx)
	if err != nil {
		return nil, nil, err
	}
	defer read.Close()
	rows, err := read.connection.QueryContext(ctx, `SELECT `+peerQuestionColumns+` FROM peer_questions WHERE source_task_id=? OR target_task_id=? ORDER BY created_at_ms DESC,id DESC LIMIT 2 OFFSET ?`, taskID.Bytes(), taskID.Bytes(), int64(offset))
	if err != nil {
		return nil, nil, err
	}
	defer rows.Close()
	result := []PeerQuestion{}
	for rows.Next() {
		item, found, err := scanPeerQuestion(rows)
		if err != nil {
			return nil, nil, err
		}
		if !found {
			return nil, nil, ErrCorruptState
		}
		if len(result) == 1 {
			next := offset + 1
			return result, &next, nil
		}
		result = append(result, item)
	}
	if err := rows.Err(); err != nil {
		return nil, nil, err
	}
	return result, nil, nil
}

// ReservePeerDelivery records uncertainty before the daemon writes a fixed
// terminal notice. Replays must never write a question or answer twice.
func (store *Store) ReservePeerDelivery(ctx context.Context, questionID PeerQuestionID, runID RunID, deliveryID PeerDeliveryID, answer bool, at UnixMillis) (PeerDelivery, bool, error) {
	if questionID.zero() || runID.zero() || deliveryID.zero() {
		return PeerDelivery{}, false, ErrInvalidValue
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return PeerDelivery{}, false, err
	}
	defer tx.Close()
	question, found, err := peerQuestionByID(ctx, tx.connection, questionID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return PeerDelivery{}, false, tx.Rollback(err)
	}
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return PeerDelivery{}, false, tx.Rollback(err)
	}
	if run.Role != RoleWorker || run.Provider != ProviderCodex || run.Phase != RunRunning || run.CredentialRevokedAt != nil || run.ProjectID != question.ProjectID || at.Int64() < run.UpdatedAt.Int64() {
		return PeerDelivery{}, false, tx.Rollback(ErrRevisionConflict)
	}
	wantTask := question.TargetTaskID
	state := question.RecipientDeliveryState
	existing := question.RecipientDeliveryID
	payload := []byte("A peer question is available. Run `factoryctl attempt peer status` to read and answer it asynchronously.")
	if answer {
		wantTask = question.SourceTaskID
		state = question.AnswerDeliveryState
		existing = question.AnswerDeliveryID
		payload = []byte("A peer answer is available. Run `factoryctl attempt peer status` to read it.")
	}
	if question.AnswerIdempotencyKey == nil && answer {
		return PeerDelivery{}, false, tx.Rollback(ErrRevisionConflict)
	}
	if run.TaskID != wantTask {
		return PeerDelivery{}, false, tx.Rollback(ErrUnauthorized)
	}
	if existing != nil {
		if *existing != deliveryID {
			return PeerDelivery{}, false, tx.Rollback(ErrConflict)
		}
		if err := tx.Rollback(nil); err != nil {
			return PeerDelivery{}, false, err
		}
		return PeerDelivery{QuestionID: question.ID, RunID: run.ID, Provider: run.Provider, DeliveryID: deliveryID, Revision: question.Revision, Payload: payload, Answer: answer}, false, nil
	}
	if state != PeerDeliveryPending {
		return PeerDelivery{}, false, tx.Rollback(ErrCorruptState)
	}
	column := "recipient_delivery"
	if answer {
		column = "answer_delivery"
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE peer_questions SET `+column+`_id=?, `+column+`_state='unknown', revision=revision+1, updated_at_ms=? WHERE id=? AND revision=?`, deliveryID.Bytes(), at.Int64(), question.ID.Bytes(), question.Revision.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return PeerDelivery{}, false, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityPeerQuestion, id: question.ID.Bytes(), revision: question.Revision.Int64() + 1}}); err != nil {
		return PeerDelivery{}, false, tx.Rollback(err)
	}
	next, err := NewRevision(question.Revision.Int64() + 1)
	if err != nil {
		return PeerDelivery{}, false, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return PeerDelivery{}, false, err
	}
	return PeerDelivery{QuestionID: question.ID, RunID: run.ID, Provider: run.Provider, DeliveryID: deliveryID, Revision: next, Payload: payload, Answer: answer}, true, nil
}

func (store *Store) AcknowledgePeerDelivery(ctx context.Context, questionID PeerQuestionID, deliveryID PeerDeliveryID, answer bool, expected Revision, at UnixMillis) (PeerQuestion, error) {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return PeerQuestion{}, err
	}
	defer tx.Close()
	question, found, err := peerQuestionByID(ctx, tx.connection, questionID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	column := "recipient_delivery"
	state := question.RecipientDeliveryState
	current := question.RecipientDeliveryID
	if answer {
		column = "answer_delivery"
		state = question.AnswerDeliveryState
		current = question.AnswerDeliveryID
	}
	if question.Revision != expected || current == nil || *current != deliveryID || state != PeerDeliveryUnknown {
		return PeerQuestion{}, tx.Rollback(ErrRevisionConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE peer_questions SET `+column+`_state='delivered', revision=revision+1, updated_at_ms=? WHERE id=? AND revision=?`, at.Int64(), question.ID.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityPeerQuestion, id: question.ID.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return PeerQuestion{}, tx.Rollback(err)
	}
	result, found, err := peerQuestionByID(ctx, tx.connection, questionID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return PeerQuestion{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return PeerQuestion{}, err
	}
	return result, nil
}

func (store *Store) RunningPeerTarget(ctx context.Context, question PeerQuestion) (Run, bool, error) {
	read, err := store.beginRead(ctx)
	if err != nil {
		return Run{}, false, err
	}
	defer read.Close()
	task := question.TargetTaskID
	if question.AnswerIdempotencyKey != nil {
		task = question.SourceTaskID
	}
	return scanRun(read.connection.QueryRowContext(ctx, `SELECT `+runColumns+` FROM runs WHERE task_id=? AND phase='running'`, task.Bytes()))
}
