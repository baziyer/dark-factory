package kernel

import (
	"context"
	"database/sql"
	"fmt"
)

// MaxSendBackNoteBytes bounds the note a send-back leaves at the end of a
// task's body.
const MaxSendBackNoteBytes = 8192

// sentBackMarker opens the section a send-back leaves. Its byte offset is
// stored with the task, so operator text that happens to quote this heading
// is never mistaken for a prior send-back.
const sentBackMarker = "\n\n## Sent back for work revision "

// SentBackBody is the body a send-back leaves: the task's instruction as a
// run receives it (the body, or the title when the body is empty) with the
// note under a heading that names the work revision it opens. An earlier
// send-back's section is replaced, not kept: the body a worker receives is
// its instruction and the latest note, bounded however many times the task
// comes back, and the findings an earlier note pointed at stay where they
// were. The daemon checks the body against the provider that will receive
// it before the send-back is made.
func SentBackBody(task Task, note string) string {
	instruction := sentBackInstruction(task)
	return fmt.Sprintf("%s%s%d\n\n%s", instruction, sentBackMarker, task.WorkRevision.Int64()+1, note)
}

// TaskInstruction is the editable instruction before a retained send-back
// note. It deliberately does not substitute the title: an empty task body is
// meaningful to the normal supervisor, which applies that fallback itself.
func TaskInstruction(task Task) string {
	instruction := task.Body
	if task.SentBackInstructionBytes != nil {
		instruction = instruction[:*task.SentBackInstructionBytes]
	}
	return instruction
}

func sentBackInstruction(task Task) string {
	instruction := TaskInstruction(task)
	if instruction == "" {
		instruction = task.Title
	}
	return instruction
}

// TaskFeedback is the one retained send-back note. It is read-only feedback,
// not part of the instruction an operator replaces.
func TaskFeedback(task Task) string {
	if task.SentBackInstructionBytes == nil {
		return ""
	}
	return task.Body[*task.SentBackInstructionBytes:]
}

// TaskBodyWithInstruction replaces only the editable instruction and retains
// the current send-back note. A blank base before a note falls back to title,
// matching SentBackBody and leaving a useful effective prompt.
func TaskBodyWithInstruction(task Task, instruction string) (string, *int64) {
	feedback := TaskFeedback(task)
	if feedback == "" {
		return instruction, nil
	}
	if instruction == "" {
		instruction = task.Title
	}
	offset := int64(byteLen(instruction))
	return instruction + feedback, &offset
}

// SendBackTask returns a finished task to its queue at the next work revision
// with the note at the end of its body, so the worker's next run reopens the
// retained Change and continues from the tree it left. Any terminal outcome
// may be sent back, a success included: the reviewer, not the worker, decides
// when work is done; a cancelled task comes back the same way. A task that
// never ran has nothing to go back to, and a shell agent's task is a
// program, which no note can be added to.
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
// project (another project's is not its authority), be a worker's, and not
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
	agent, found, err := agentByID(ctx, tx.connection, task.AssignedAgentID)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if !found || agent.Role != RoleWorker {
		return Task{}, tx.Rollback(ErrUnauthorized)
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
	agent, found, err := agentByID(ctx, connection, task.AssignedAgentID)
	if err != nil {
		return Task{}, err
	}
	if !found {
		return Task{}, ErrCorruptState
	}
	if agent.Provider == ProviderShell {
		return Task{}, fmt.Errorf("%w: a shell task is a program and takes no note", ErrInvalidValue)
	}
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
	body := SentBackBody(task, note)
	if byteLen(body) > 131072 {
		return Task{}, fmt.Errorf("%w: send-back note does not fit the task body", ErrInvalidValue)
	}
	result, err := connection.ExecContext(ctx, `UPDATE tasks SET status = 'queued', work_revision = ?, body = ?, sent_back_instruction_bytes = ?, blocked_reason = NULL, result = NULL, completed_at_ms = NULL, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`,
		next, body, byteLen(sentBackInstruction(task)), at.Int64(), task.ID.Bytes(), task.Revision.Int64())
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
