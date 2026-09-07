package kernel

import (
	"context"
	"database/sql"
	"fmt"
)

// MaxSendBackNoteBytes bounds the note a send-back appends to a task's body.
const MaxSendBackNoteBytes = 8192

// MaxSentBackBodyBytes bounds the body a send-back leaves. A provider receives
// the whole body as its task: Claude Code as a typed prompt of at most 8 KiB
// with its lead and quoting, Codex as an attempt task of at most 8 KiB. A body
// past this would be queued only to fail at launch.
const MaxSentBackBodyBytes = 7 << 10

// SendBackTask returns a finished task to its queue at the next work revision
// with a note appended to its body, so the worker's next run reopens the
// retained Change and continues from the tree it left. Any terminal outcome
// may be sent back, a success included: the reviewer, not the worker, decides
// when work is done. A task that never ran has nothing to go back to.
func (store *Store) SendBackTask(ctx context.Context, id TaskID, expected Revision, note string, at UnixMillis) (Task, error) {
	if id.zero() || expected.Int64() < 1 {
		return Task{}, fmt.Errorf("%w: invalid task send-back", ErrInvalidValue)
	}
	if byteLen(note) < 1 || byteLen(note) > MaxSendBackNoteBytes {
		return Task{}, fmt.Errorf("%w: invalid send-back note", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Task{}, err
	}
	defer tx.Close()
	task, found, err := taskByID(ctx, tx.connection, id)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if !found {
		return Task{}, tx.Rollback(ErrNotFound)
	}
	if task.Revision != expected {
		return Task{}, tx.Rollback(ErrRevisionConflict)
	}
	updated, err := sendBackTask(ctx, tx.connection, task, note, at)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Task{}, err
	}
	return updated, nil
}

// SendBackTaskForAttempt is the orchestrator's form of SendBackTask: the
// attempt must be a running orchestrator, the task must belong to its
// project (another project's is not its authority), and the task must not
// be the attempt's own.
func (store *Store) SendBackTaskForAttempt(ctx context.Context, digest AttemptDigest, id TaskID, note string, at UnixMillis) (Task, error) {
	if id.zero() {
		return Task{}, fmt.Errorf("%w: invalid task send-back", ErrInvalidValue)
	}
	if byteLen(note) < 1 || byteLen(note) > MaxSendBackNoteBytes {
		return Task{}, fmt.Errorf("%w: invalid send-back note", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Task{}, err
	}
	defer tx.Close()
	run, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if !found || run.Phase != RunRunning || run.CredentialRevokedAt != nil || run.Role != RoleOrchestrator {
		return Task{}, tx.Rollback(ErrUnauthorized)
	}
	task, found, err := taskByID(ctx, tx.connection, id)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if !found {
		return Task{}, tx.Rollback(ErrNotFound)
	}
	if task.ProjectID != run.ProjectID {
		return Task{}, tx.Rollback(ErrUnauthorized)
	}
	if task.ID == run.TaskID {
		return Task{}, tx.Rollback(ErrConflict)
	}
	updated, err := sendBackTask(ctx, tx.connection, task, note, at)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Task{}, err
	}
	return updated, nil
}

func sendBackTask(ctx context.Context, connection *sql.Conn, task Task, note string, at UnixMillis) (Task, error) {
	switch task.Status {
	case TaskSucceeded, TaskFailed, TaskBlocked, TaskCancelled:
	default:
		return Task{}, ErrConflict
	}
	if at.Int64() < task.UpdatedAt.Int64() {
		return Task{}, ErrRevisionConflict
	}
	var runs int64
	if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM runs WHERE task_id = ? AND task_incarnation_id = ? AND admitted_task_work_revision = ?`,
		task.ID.Bytes(), task.IncarnationID.Bytes(), task.WorkRevision.Int64()).Scan(&runs); err != nil {
		return Task{}, err
	}
	if runs != 1 {
		return Task{}, ErrConflict
	}
	next := task.WorkRevision.Int64() + 1
	body := fmt.Sprintf("%s\n\n## Sent back for work revision %d\n\n%s", task.Body, next, note)
	if byteLen(body) > MaxSentBackBodyBytes {
		return Task{}, fmt.Errorf("%w: send-back note does not fit the provider's task bound", ErrInvalidValue)
	}
	result, err := connection.ExecContext(ctx, `UPDATE tasks SET status = 'queued', work_revision = ?, body = ?, blocked_reason = NULL, result = NULL, completed_at_ms = NULL, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`,
		next, body, at.Int64(), task.ID.Bytes(), task.Revision.Int64())
	if err := requireOneRow(result, err); err != nil {
		return Task{}, err
	}
	if err := appendInvalidations(ctx, connection, at, []pendingInvalidation{{kind: EntityTask, id: task.ID.Bytes(), revision: task.Revision.Int64() + 1}}); err != nil {
		return Task{}, err
	}
	updated, found, err := taskByID(ctx, connection, task.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Task{}, err
	}
	return updated, nil
}
