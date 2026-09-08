package kernel

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"errors"
	"fmt"
	"unicode/utf8"
)

const (
	MaxTaskInterventionPayloadBytes = 8192
	MaxTaskInterventionHistory      = 32
)

type TaskInterventionKind uint8

const (
	TaskInterventionMessage TaskInterventionKind = iota + 1
	TaskInterventionInterrupt
	TaskInterventionStop
	TaskInterventionReplace
)

func (kind TaskInterventionKind) String() string {
	switch kind {
	case TaskInterventionMessage:
		return "message"
	case TaskInterventionInterrupt:
		return "interrupt"
	case TaskInterventionStop:
		return "stop"
	case TaskInterventionReplace:
		return "replace"
	default:
		return ""
	}
}

func parseTaskInterventionKind(value string) (TaskInterventionKind, error) {
	for kind := TaskInterventionMessage; kind <= TaskInterventionReplace; kind++ {
		if kind.String() == value {
			return kind, nil
		}
	}
	return 0, corruptControl("task intervention kind", value)
}

type TaskInterventionActor uint8

const (
	TaskInterventionOperator TaskInterventionActor = iota + 1
	TaskInterventionOrchestrator
)

func (actor TaskInterventionActor) String() string {
	switch actor {
	case TaskInterventionOperator:
		return "operator"
	case TaskInterventionOrchestrator:
		return "orchestrator"
	default:
		return ""
	}
}

func parseTaskInterventionActor(value string) (TaskInterventionActor, error) {
	for actor := TaskInterventionOperator; actor <= TaskInterventionOrchestrator; actor++ {
		if actor.String() == value {
			return actor, nil
		}
	}
	return 0, corruptControl("task intervention actor", value)
}

type TaskInterventionState uint8

const (
	TaskInterventionPending TaskInterventionState = iota + 1
	TaskInterventionDelivered
	TaskInterventionUnknown
	TaskInterventionRejected
)

func (state TaskInterventionState) String() string {
	switch state {
	case TaskInterventionPending:
		return "pending"
	case TaskInterventionDelivered:
		return "delivered"
	case TaskInterventionUnknown:
		return "unknown"
	case TaskInterventionRejected:
		return "rejected"
	default:
		return ""
	}
}

func parseTaskInterventionState(value string) (TaskInterventionState, error) {
	for state := TaskInterventionPending; state <= TaskInterventionRejected; state++ {
		if state.String() == value {
			return state, nil
		}
	}
	return 0, corruptControl("task intervention state", value)
}

type TaskInterventionRequest struct {
	OperationID          TaskInterventionID
	TaskID               TaskID
	RunID                RunID
	ExpectedTaskRevision Revision
	ExpectedRunRevision  Revision
	Actor                TaskInterventionActor
	ActorRunID           *RunID
	ActorBrowserClientID *BrowserClientID
	Kind                 TaskInterventionKind
	Payload              string
	SuccessorTaskID      *TaskID
}

type TaskIntervention struct {
	OperationID          TaskInterventionID
	ProjectID            ProjectID
	TaskID               TaskID
	RunID                RunID
	ExpectedTaskRevision Revision
	ExpectedRunRevision  Revision
	Actor                TaskInterventionActor
	ActorRunID           *RunID
	ActorBrowserClientID *BrowserClientID
	Kind                 TaskInterventionKind
	Payload              string
	SuccessorTaskID      *TaskID
	State                TaskInterventionState
	ResultDetail         *string
	CreatedAt            UnixMillis
	UpdatedAt            UnixMillis
	TerminalAt           *UnixMillis
}

func (request TaskInterventionRequest) valid() error {
	if request.OperationID.zero() || request.TaskID.zero() || request.RunID.zero() || request.ExpectedTaskRevision.Int64() < 1 || request.ExpectedRunRevision.Int64() < 1 || request.Actor.String() == "" || request.Kind.String() == "" || !utf8.ValidString(request.Payload) || byteLen(request.Payload) > MaxTaskInterventionPayloadBytes {
		return fmt.Errorf("%w: invalid task intervention", ErrInvalidValue)
	}
	if request.Actor == TaskInterventionOrchestrator && (request.ActorRunID == nil || request.ActorRunID.zero() || request.ActorBrowserClientID != nil) || request.Actor == TaskInterventionOperator && request.ActorRunID != nil || request.ActorBrowserClientID != nil && request.ActorBrowserClientID.zero() {
		return fmt.Errorf("%w: invalid task intervention actor", ErrInvalidValue)
	}
	if request.Kind == TaskInterventionMessage && byteLen(request.Payload) < 1 || (request.Kind == TaskInterventionInterrupt || request.Kind == TaskInterventionStop) && request.Payload != "" || request.Kind == TaskInterventionReplace && (byteLen(request.Payload) < 1 || request.SuccessorTaskID == nil || request.SuccessorTaskID.zero()) || request.Kind != TaskInterventionReplace && request.SuccessorTaskID != nil {
		return fmt.Errorf("%w: invalid task intervention payload", ErrInvalidValue)
	}
	return nil
}

const taskInterventionColumns = `operation_id, project_id, task_id, run_id, expected_task_revision, expected_run_revision,
	actor_kind, actor_run_id, actor_browser_client_id, kind, payload, payload_digest, successor_task_id, state, result_detail, created_at_ms, updated_at_ms, terminal_at_ms`

func scanTaskIntervention(scanner rowScanner) (TaskIntervention, bool, error) {
	var rawOperation, rawProject, rawTask, rawRun []byte
	var expectedTask, expectedRun int64
	var rawActorRun nullableBlob
	var rawActorBrowser nullableBlob
	var actorText, kindText, payload, stateText string
	var digest []byte
	var rawSuccessor nullableBlob
	var detail sql.NullString
	var created, updated int64
	var terminal sql.NullInt64
	if err := scanner.Scan(&rawOperation, &rawProject, &rawTask, &rawRun, &expectedTask, &expectedRun, &actorText, &rawActorRun, &rawActorBrowser, &kindText, &payload, &digest, &rawSuccessor, &stateText, &detail, &created, &updated, &terminal); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return TaskIntervention{}, false, nil
		}
		return TaskIntervention{}, false, fmt.Errorf("scan task intervention: %w", err)
	}
	operation, operationErr := TaskInterventionIDFromBytes(rawOperation)
	project, projectErr := ProjectIDFromBytes(rawProject)
	task, taskErr := TaskIDFromBytes(rawTask)
	run, runErr := RunIDFromBytes(rawRun)
	taskRevision, taskRevisionErr := NewRevision(expectedTask)
	runRevision, runRevisionErr := NewRevision(expectedRun)
	actor, actorErr := parseTaskInterventionActor(actorText)
	kind, kindErr := parseTaskInterventionKind(kindText)
	state, stateErr := parseTaskInterventionState(stateText)
	createdAt, createdErr := NewUnixMillis(created)
	updatedAt, updatedErr := NewUnixMillis(updated)
	if operationErr != nil || projectErr != nil || taskErr != nil || runErr != nil || taskRevisionErr != nil || runRevisionErr != nil || actorErr != nil || kindErr != nil || stateErr != nil || createdErr != nil || updatedErr != nil || len(digest) != DigestBytes || sha256.Sum256([]byte(payload)) != [DigestBytes]byte(digest) {
		return TaskIntervention{}, false, fmt.Errorf("%w: invalid task intervention row", ErrCorruptState)
	}
	result := TaskIntervention{OperationID: operation, ProjectID: project, TaskID: task, RunID: run, ExpectedTaskRevision: taskRevision, ExpectedRunRevision: runRevision, Actor: actor, Kind: kind, Payload: payload, State: state, CreatedAt: createdAt, UpdatedAt: updatedAt}
	if rawActorRun.valid {
		actorRun, err := RunIDFromBytes(rawActorRun.bytes)
		if err != nil {
			return TaskIntervention{}, false, fmt.Errorf("%w: invalid task intervention actor run", ErrCorruptState)
		}
		result.ActorRunID = &actorRun
	}
	if rawActorBrowser.valid {
		browserClient, err := BrowserClientIDFromBytes(rawActorBrowser.bytes)
		if err != nil {
			return TaskIntervention{}, false, fmt.Errorf("%w: invalid task intervention browser actor", ErrCorruptState)
		}
		result.ActorBrowserClientID = &browserClient
	}
	if rawSuccessor.valid {
		successor, err := TaskIDFromBytes(rawSuccessor.bytes)
		if err != nil {
			return TaskIntervention{}, false, fmt.Errorf("%w: invalid task intervention successor", ErrCorruptState)
		}
		result.SuccessorTaskID = &successor
	}
	if detail.Valid {
		if !utf8TextWithin(detail.String, 1, 4096) {
			return TaskIntervention{}, false, fmt.Errorf("%w: invalid task intervention detail", ErrCorruptState)
		}
		result.ResultDetail = &detail.String
	}
	if terminal.Valid {
		value, err := NewUnixMillis(terminal.Int64)
		if err != nil {
			return TaskIntervention{}, false, fmt.Errorf("%w: invalid task intervention terminal time", ErrCorruptState)
		}
		result.TerminalAt = &value
	}
	if err := validateTaskIntervention(result); err != nil {
		return TaskIntervention{}, false, err
	}
	return result, true, nil
}

func validateTaskIntervention(value TaskIntervention) error {
	if value.OperationID.zero() || value.ProjectID.zero() || value.TaskID.zero() || value.RunID.zero() || value.ExpectedTaskRevision.Int64() < 1 || value.ExpectedRunRevision.Int64() < 1 || value.Actor.String() == "" || value.Kind.String() == "" || value.State.String() == "" || value.CreatedAt.Int64() > value.UpdatedAt.Int64() || !utf8.ValidString(value.Payload) || byteLen(value.Payload) > MaxTaskInterventionPayloadBytes {
		return fmt.Errorf("%w: invalid task intervention controls", ErrCorruptState)
	}
	if value.Actor == TaskInterventionOrchestrator && (value.ActorRunID == nil || value.ActorBrowserClientID != nil) || value.Actor == TaskInterventionOperator && value.ActorRunID != nil || value.ActorRunID != nil && value.ActorRunID.zero() || value.ActorBrowserClientID != nil && value.ActorBrowserClientID.zero() || value.Kind == TaskInterventionMessage && (byteLen(value.Payload) < 1 || value.SuccessorTaskID != nil) || (value.Kind == TaskInterventionInterrupt || value.Kind == TaskInterventionStop) && (value.Payload != "" || value.SuccessorTaskID != nil) || value.Kind == TaskInterventionReplace && (byteLen(value.Payload) < 1 || value.SuccessorTaskID == nil || value.SuccessorTaskID.zero()) {
		return fmt.Errorf("%w: invalid task intervention boundary", ErrCorruptState)
	}
	terminal := value.State != TaskInterventionPending
	if terminal != (value.TerminalAt != nil) || value.TerminalAt != nil && value.TerminalAt.Int64() != value.UpdatedAt.Int64() || value.TerminalAt != nil && value.TerminalAt.Int64() < value.CreatedAt.Int64() {
		return fmt.Errorf("%w: invalid task intervention terminal state", ErrCorruptState)
	}
	return nil
}

func taskInterventionByID(ctx context.Context, connection *sql.Conn, operationID TaskInterventionID) (TaskIntervention, bool, error) {
	return scanTaskIntervention(connection.QueryRowContext(ctx, `SELECT `+taskInterventionColumns+` FROM task_interventions WHERE operation_id = ?`, operationID.Bytes()))
}

// ReserveTaskInterventionForBrowser rechecks the browser client's human-action
// authority in the same write transaction that creates its receipt.
func (store *Store) ReserveTaskInterventionForBrowser(ctx context.Context, clientID BrowserClientID, request TaskInterventionRequest, at UnixMillis) (TaskIntervention, bool, error) {
	if clientID.zero() || request.Actor != 0 || request.ActorRunID != nil || request.ActorBrowserClientID != nil {
		return TaskIntervention{}, false, fmt.Errorf("%w: invalid browser task intervention", ErrInvalidValue)
	}
	request.Actor, request.ActorBrowserClientID = TaskInterventionOperator, &clientID
	if err := request.valid(); err != nil {
		return TaskIntervention{}, false, err
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TaskIntervention{}, false, err
	}
	defer tx.Close()
	client, found, err := browserClientByID(ctx, tx.connection, clientID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return TaskIntervention{}, false, err
	}
	if client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityHumanActions) || (request.Kind == TaskInterventionMessage || request.Kind == TaskInterventionInterrupt) && !client.CapabilityMask.Has(BrowserCapabilityTerminalInput) {
		return TaskIntervention{}, false, ErrUnauthorized
	}
	result, reserved, err := reserveTaskInterventionTx(ctx, tx, request, at)
	if err != nil {
		return TaskIntervention{}, false, err
	}
	if err := tx.Commit(ctx); err != nil {
		return TaskIntervention{}, false, err
	}
	return result, reserved, nil
}

// ReserveTaskInterventionForAttempt rechecks a live orchestrator credential in
// the same write transaction. The caller cannot nominate a different actor.
func (store *Store) ReserveTaskInterventionForAttempt(ctx context.Context, digest AttemptDigest, request TaskInterventionRequest, at UnixMillis) (TaskIntervention, bool, error) {
	if len(digest.Bytes()) != DigestBytes || request.Actor != 0 || request.ActorRunID != nil || request.ActorBrowserClientID != nil {
		return TaskIntervention{}, false, fmt.Errorf("%w: invalid attempt task intervention", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TaskIntervention{}, false, err
	}
	defer tx.Close()
	actorRun, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil || !found {
		if err == nil {
			err = ErrUnauthorized
		}
		return TaskIntervention{}, false, err
	}
	if actorRun.Role != RoleOrchestrator || actorRun.Phase != RunRunning || actorRun.CredentialRevokedAt != nil {
		return TaskIntervention{}, false, ErrUnauthorized
	}
	request.Actor, request.ActorRunID = TaskInterventionOrchestrator, &actorRun.ID
	if err := request.valid(); err != nil {
		return TaskIntervention{}, false, err
	}
	result, reserved, err := reserveTaskInterventionTx(ctx, tx, request, at)
	if err != nil {
		return TaskIntervention{}, false, err
	}
	if err := tx.Commit(ctx); err != nil {
		return TaskIntervention{}, false, err
	}
	return result, reserved, nil
}

func reserveTaskInterventionTx(ctx context.Context, tx *writeTx, request TaskInterventionRequest, at UnixMillis) (TaskIntervention, bool, error) {
	existing, found, err := taskInterventionByID(ctx, tx.connection, request.OperationID)
	if err != nil {
		return TaskIntervention{}, false, err
	}
	if found {
		if !taskInterventionMatchesRequest(existing, request) {
			return TaskIntervention{}, false, ErrConflict
		}
		return existing, false, nil
	}
	task, found, err := taskByID(ctx, tx.connection, request.TaskID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return TaskIntervention{}, false, err
	}
	run, found, err := runByID(ctx, tx.connection, request.RunID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return TaskIntervention{}, false, err
	}
	if task.ProjectID != run.ProjectID || task.ID != run.TaskID || task.Revision != request.ExpectedTaskRevision || run.Revision != request.ExpectedRunRevision || task.Status != TaskRunning || run.Phase != RunRunning || at.Int64() < task.UpdatedAt.Int64() || at.Int64() < run.UpdatedAt.Int64() {
		return TaskIntervention{}, false, ErrRevisionConflict
	}
	if request.ActorRunID != nil {
		actorRun, found, err := runByID(ctx, tx.connection, *request.ActorRunID)
		if err != nil || !found {
			if err == nil {
				err = ErrNotFound
			}
			return TaskIntervention{}, false, err
		}
		if actorRun.Role != RoleOrchestrator || actorRun.Phase != RunRunning || actorRun.ProjectID != task.ProjectID || actorRun.ID == run.ID {
			return TaskIntervention{}, false, ErrUnauthorized
		}
	}
	digest := sha256.Sum256([]byte(request.Payload))
	var actorRun any
	if request.ActorRunID != nil {
		actorRun = request.ActorRunID.Bytes()
	}
	var actorBrowser any
	if request.ActorBrowserClientID != nil {
		actorBrowser = request.ActorBrowserClientID.Bytes()
	}
	var successor any
	if request.SuccessorTaskID != nil {
		successor = request.SuccessorTaskID.Bytes()
	}
	if _, err := tx.connection.ExecContext(ctx, `INSERT INTO task_interventions(operation_id, project_id, task_id, run_id, expected_task_revision, expected_run_revision, actor_kind, actor_run_id, actor_browser_client_id, kind, payload, payload_digest, successor_task_id, state, result_detail, created_at_ms, updated_at_ms, terminal_at_ms) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', NULL, ?, ?, NULL)`, request.OperationID.Bytes(), task.ProjectID.Bytes(), task.ID.Bytes(), run.ID.Bytes(), request.ExpectedTaskRevision.Int64(), request.ExpectedRunRevision.Int64(), request.Actor.String(), actorRun, actorBrowser, request.Kind.String(), request.Payload, digest[:], successor, at.Int64(), at.Int64()); err != nil {
		return TaskIntervention{}, false, err
	}
	if _, err := touchTaskForIntervention(ctx, tx.connection, task, at); err != nil {
		return TaskIntervention{}, false, err
	}
	result, found, err := taskInterventionByID(ctx, tx.connection, request.OperationID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return TaskIntervention{}, false, err
	}
	return result, true, nil
}

func taskInterventionMatchesRequest(existing TaskIntervention, request TaskInterventionRequest) bool {
	return existing.OperationID == request.OperationID && existing.TaskID == request.TaskID && existing.RunID == request.RunID && existing.ExpectedTaskRevision == request.ExpectedTaskRevision && existing.ExpectedRunRevision == request.ExpectedRunRevision && existing.Actor == request.Actor && (existing.ActorRunID == nil) == (request.ActorRunID == nil) && (existing.ActorRunID == nil || *existing.ActorRunID == *request.ActorRunID) && (existing.ActorBrowserClientID == nil) == (request.ActorBrowserClientID == nil) && (existing.ActorBrowserClientID == nil || *existing.ActorBrowserClientID == *request.ActorBrowserClientID) && existing.Kind == request.Kind && existing.Payload == request.Payload && (existing.SuccessorTaskID == nil) == (request.SuccessorTaskID == nil) && (existing.SuccessorTaskID == nil || *existing.SuccessorTaskID == *request.SuccessorTaskID)
}

// ResolveTaskIntervention records the one terminal effect result. A matching
// replay returns the receipt; a different result is a conflict rather than a
// second terminal transition.
func (store *Store) ResolveTaskIntervention(ctx context.Context, operationID TaskInterventionID, state TaskInterventionState, detail string, at UnixMillis) (TaskIntervention, error) {
	if operationID.zero() || state == TaskInterventionPending || state.String() == "" || !utf8.ValidString(detail) || byteLen(detail) > 4096 {
		return TaskIntervention{}, fmt.Errorf("%w: invalid task intervention outcome", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TaskIntervention{}, err
	}
	defer tx.Close()
	result, err := resolveTaskInterventionTx(ctx, tx, operationID, state, detail, at)
	if err != nil {
		return TaskIntervention{}, err
	}
	if err := tx.Commit(ctx); err != nil {
		return TaskIntervention{}, err
	}
	return result, nil
}

func resolveTaskInterventionTx(ctx context.Context, tx *writeTx, operationID TaskInterventionID, state TaskInterventionState, detail string, at UnixMillis) (TaskIntervention, error) {
	receipt, found, err := taskInterventionByID(ctx, tx.connection, operationID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return TaskIntervention{}, err
	}
	if receipt.State != TaskInterventionPending {
		if receipt.State != state || nullString(receipt.ResultDetail) != detail {
			return TaskIntervention{}, ErrConflict
		}
		return receipt, nil
	}
	task, found, err := taskByID(ctx, tx.connection, receipt.TaskID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return TaskIntervention{}, err
	}
	if task.ProjectID != receipt.ProjectID || at.Int64() < task.UpdatedAt.Int64() {
		return TaskIntervention{}, ErrRevisionConflict
	}
	if _, err := tx.connection.ExecContext(ctx, `UPDATE task_interventions SET state = ?, result_detail = ?, updated_at_ms = ?, terminal_at_ms = ? WHERE operation_id = ? AND state = 'pending'`, state.String(), nullableString(detail), at.Int64(), at.Int64(), operationID.Bytes()); err != nil {
		return TaskIntervention{}, err
	}
	if _, err := touchTaskForIntervention(ctx, tx.connection, task, at); err != nil {
		return TaskIntervention{}, err
	}
	result, found, err := taskInterventionByID(ctx, tx.connection, operationID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return TaskIntervention{}, err
	}
	return result, nil
}

func nullString(value *string) string {
	if value == nil {
		return ""
	}
	return *value
}

func touchTaskForIntervention(ctx context.Context, connection *sql.Conn, task Task, at UnixMillis) (Task, error) {
	updated, err := connection.ExecContext(ctx, `UPDATE tasks SET revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`, at.Int64(), task.ID.Bytes(), task.Revision.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Task{}, err
	}
	if err := appendInvalidations(ctx, connection, at, []pendingInvalidation{{kind: EntityTask, id: task.ID.Bytes(), revision: task.Revision.Int64() + 1}}); err != nil {
		return Task{}, err
	}
	result, found, err := taskByID(ctx, connection, task.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Task{}, err
	}
	return result, nil
}

// TaskInterventions returns the newest bounded durable history for one task.
func (store *Store) TaskInterventions(ctx context.Context, projectID ProjectID, taskID TaskID) ([]TaskIntervention, error) {
	if projectID.zero() || taskID.zero() {
		return nil, fmt.Errorf("%w: invalid task intervention history", ErrInvalidValue)
	}
	connection, err := store.readerConnection(ctx)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.QueryContext(ctx, `SELECT `+taskInterventionColumns+` FROM task_interventions WHERE project_id = ? AND task_id = ? ORDER BY created_at_ms DESC, operation_id DESC LIMIT ?`, projectID.Bytes(), taskID.Bytes(), MaxTaskInterventionHistory)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var result []TaskIntervention
	for rows.Next() {
		item, found, err := scanTaskIntervention(rows)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return nil, err
		}
		result = append(result, item)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	return result, nil
}
