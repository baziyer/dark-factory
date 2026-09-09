package kernel

import (
	"context"
	"fmt"
)

// AgentPatch is the console's agent-configuration edit. A nil member is not
// part of the edit and leaves the durable column alone.
type AgentPatch struct {
	Model           *string
	ReasoningEffort *string
	// AccountID selects a linked provider login; the zero identity clears the
	// selection back to the provider's default configuration directory.
	AccountID *AccountID
	Paused    *bool
	// The idle rule. A new budget starts the used count again; that is the
	// one explicit operator action that resets it.
	IdlePolicy       *IdlePolicy
	IdleAfterSeconds *uint32
	IdleInstruction  *string
	IdleRunBudget    *uint32
}

// TaskPatch is the console's queue edit. A nil member leaves the durable
// column alone; Cancel is the only status transition the console may ask for.
type TaskPatch struct {
	Title           *string
	Body            *string
	Priority        *int64
	AssignedAgentID *AgentID
	Cancel          bool
}

// UpdateAgent applies one bounded configuration edit to an existing agent at
// an exact observed revision. The launch controls go through the same domain
// validation CreateAgent uses, so the wire cannot widen what a provider may
// be launched with.
func (store *Store) UpdateAgent(ctx context.Context, id AgentID, expected Revision, patch AgentPatch, at UnixMillis) (Agent, error) {
	return store.updateAgent(ctx, nil, id, expected, patch, at)
}

// UpdateAgentForOverseer applies a worker pause/resume inside the running
// orchestrator's project. Authorization and the exact revision update share
// one write transaction. Keeping the authority's input to this one field
// prevents it from acquiring console configuration controls.
func (store *Store) UpdateAgentForOverseer(ctx context.Context, digest AttemptDigest, id AgentID, expected Revision, paused bool, at UnixMillis) (Agent, error) {
	return store.updateAgent(ctx, &digest, id, expected, AgentPatch{Paused: &paused}, at)
}

func (store *Store) updateAgent(ctx context.Context, digest *AttemptDigest, id AgentID, expected Revision, patch AgentPatch, at UnixMillis) (Agent, error) {
	if id.zero() || expected.Int64() < 1 {
		return Agent{}, fmt.Errorf("%w: invalid agent update", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Agent{}, err
	}
	defer tx.Close()
	var overseer Run
	if digest != nil {
		overseer, err = overseerRun(ctx, tx.connection, *digest)
		if err != nil {
			return Agent{}, tx.Rollback(err)
		}
	}
	agent, found, err := agentByID(ctx, tx.connection, id)
	if err != nil {
		return Agent{}, tx.Rollback(err)
	}
	if !found {
		return Agent{}, tx.Rollback(ErrNotFound)
	}
	if digest != nil && (agent.ProjectID != overseer.ProjectID || agent.Role != RoleWorker) {
		return Agent{}, tx.Rollback(ErrUnauthorized)
	}
	if agent.Revision != expected || at.Int64() < agent.UpdatedAt.Int64() {
		return Agent{}, tx.Rollback(ErrRevisionConflict)
	}
	if patch.Model != nil {
		agent.Model = *patch.Model
	}
	if patch.ReasoningEffort != nil {
		agent.ReasoningEffort = *patch.ReasoningEffort
	}
	if patch.AccountID != nil {
		agent.AccountID = *patch.AccountID
		if err := requireAccountForProvider(ctx, tx.connection, agent.Provider, agent.AccountID); err != nil {
			return Agent{}, tx.Rollback(err)
		}
	}
	if patch.Paused != nil {
		agent.Paused = *patch.Paused
	}
	if patch.IdlePolicy != nil {
		agent.Idle.Policy = *patch.IdlePolicy
	}
	if patch.IdleAfterSeconds != nil {
		agent.Idle.AfterSeconds = *patch.IdleAfterSeconds
	}
	if patch.IdleInstruction != nil {
		agent.Idle.Instruction = *patch.IdleInstruction
	}
	if patch.IdleRunBudget != nil {
		agent.Idle.RunBudget, agent.Idle.RunsUsed = *patch.IdleRunBudget, 0
	}
	if err := validateIdleRule(agent.Idle); err != nil {
		return Agent{}, tx.Rollback(err)
	}
	// Only an edit that touches a launch control is held to the launch rules.
	// A stored row may legitimately hold a combination new launches refuse
	// (Claude ultra), and pausing such an agent must not be blocked by it.
	if patch.Model != nil || patch.ReasoningEffort != nil {
		if err := ValidateProviderLaunchControls(agent.Provider, agent.Model, agent.ReasoningEffort); err != nil {
			return Agent{}, tx.Rollback(err)
		}
	} else if err := validateStoredProviderControls(agent.Provider, agent.Model, agent.ReasoningEffort); err != nil {
		return Agent{}, tx.Rollback(err)
	}
	result, err := tx.connection.ExecContext(ctx, `UPDATE agents SET model = ?, reasoning_effort = ?, account_id = ?, paused = ?, idle_policy = ?, idle_after_seconds = ?, idle_instruction = ?, idle_run_budget = ?, idle_runs_used = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`,
		nullableString(agent.Model), nullableString(agent.ReasoningEffort), nullableID(agent.AccountID), boolInt(agent.Paused), string(agent.Idle.Policy), int64(agent.Idle.AfterSeconds), agent.Idle.Instruction, int64(agent.Idle.RunBudget), int64(agent.Idle.RunsUsed), at.Int64(), id.Bytes(), expected.Int64())
	if err := requireOneRow(result, err); err != nil {
		return Agent{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityAgent, id: id.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return Agent{}, tx.Rollback(err)
	}
	updated, found, err := agentByID(ctx, tx.connection, id)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Agent{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Agent{}, err
	}
	return updated, nil
}

// UpdateTask edits one task that is still queued. A task that has left the
// queue is a conflict, not a not-found: the console observed it while it was
// still editable and lost the race with the supervisor.
func (store *Store) UpdateTask(ctx context.Context, id TaskID, expected Revision, patch TaskPatch, at UnixMillis) (Task, error) {
	return store.updateTask(ctx, nil, id, expected, patch, at)
}

// UpdateTaskForOverseer edits a queued worker task in the running
// orchestrator's project, with authorization checked in the update transaction.
func (store *Store) UpdateTaskForOverseer(ctx context.Context, digest AttemptDigest, id TaskID, expected Revision, patch TaskPatch, at UnixMillis) (Task, error) {
	return store.updateTask(ctx, &digest, id, expected, patch, at)
}

// AuthorizeWorkerTaskForOverseer establishes that a live overseer may inspect
// a worker task before the daemon validates a provider-specific edit.
func (store *Store) AuthorizeWorkerTaskForOverseer(ctx context.Context, digest AttemptDigest, id TaskID) error {
	if id.zero() {
		return fmt.Errorf("%w: invalid task authorization", ErrInvalidValue)
	}
	read, err := store.beginRead(ctx)
	if err != nil {
		return err
	}
	defer read.Close()
	overseer, err := overseerRun(ctx, read.connection, digest)
	if err != nil {
		return err
	}
	task, found, err := taskByID(ctx, read.connection, id)
	if err != nil {
		return err
	}
	if !found || task.ProjectID != overseer.ProjectID {
		return ErrUnauthorized
	}
	agent, found, err := agentByID(ctx, read.connection, task.AssignedAgentID)
	if err != nil {
		return err
	}
	if !found || agent.Role != RoleWorker {
		return ErrUnauthorized
	}
	return nil
}

func (store *Store) updateTask(ctx context.Context, digest *AttemptDigest, id TaskID, expected Revision, patch TaskPatch, at UnixMillis) (Task, error) {
	if id.zero() || expected.Int64() < 1 {
		return Task{}, fmt.Errorf("%w: invalid task update", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Task{}, err
	}
	defer tx.Close()
	var overseer Run
	if digest != nil {
		overseer, err = overseerRun(ctx, tx.connection, *digest)
		if err != nil {
			return Task{}, tx.Rollback(err)
		}
	}
	task, found, err := taskByID(ctx, tx.connection, id)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if !found {
		return Task{}, tx.Rollback(ErrNotFound)
	}
	if digest != nil && task.ProjectID != overseer.ProjectID {
		return Task{}, tx.Rollback(ErrUnauthorized)
	}
	if digest != nil {
		agent, found, err := agentByID(ctx, tx.connection, task.AssignedAgentID)
		if err != nil {
			return Task{}, tx.Rollback(err)
		}
		if !found || agent.Role != RoleWorker {
			return Task{}, tx.Rollback(ErrUnauthorized)
		}
	}
	if task.Status != TaskQueued {
		return Task{}, tx.Rollback(ErrConflict)
	}
	if task.Revision != expected || at.Int64() < task.UpdatedAt.Int64() {
		return Task{}, tx.Rollback(ErrRevisionConflict)
	}
	if patch.Title != nil {
		task.Title = *patch.Title
	}
	if patch.Body != nil {
		task.Body, task.SentBackInstructionBytes = TaskBodyWithInstruction(task, *patch.Body)
	}
	if patch.Priority != nil {
		task.Priority = *patch.Priority
	}
	if patch.AssignedAgentID != nil {
		agent, found, err := agentByID(ctx, tx.connection, *patch.AssignedAgentID)
		if err != nil {
			return Task{}, tx.Rollback(err)
		}
		// The durable foreign key is (agent, project) together, so a reassignment
		// across projects would be a corrupt row rather than a rejected edit.
		if !found || agent.ProjectID != task.ProjectID {
			return Task{}, tx.Rollback(ErrConflict)
		}
		if digest != nil && agent.Role != RoleWorker {
			return Task{}, tx.Rollback(ErrUnauthorized)
		}
		task.AssignedAgentID = agent.ID
	}
	status, completed := task.Status.String(), any(nil)
	if patch.Cancel {
		status, completed = TaskCancelled.String(), at.Int64()
	}
	if byteLen(task.Title) < 1 || byteLen(task.Title) > 1024 || byteLen(task.Body) > 131072 || task.Priority < -1_000_000 || task.Priority > 1_000_000 {
		return Task{}, tx.Rollback(fmt.Errorf("%w: invalid task update", ErrInvalidValue))
	}
	var sentBack any
	if task.SentBackInstructionBytes != nil {
		sentBack = *task.SentBackInstructionBytes
	}
	result, err := tx.connection.ExecContext(ctx, `UPDATE tasks SET title = ?, body = ?, sent_back_instruction_bytes = ?, priority = ?, assigned_agent_id = ?, status = ?, completed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'queued' AND revision = ?`,
		task.Title, task.Body, sentBack, task.Priority, task.AssignedAgentID.Bytes(), status, completed, at.Int64(), id.Bytes(), expected.Int64())
	if err := requireOneRow(result, err); err != nil {
		return Task{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityTask, id: id.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return Task{}, tx.Rollback(err)
	}
	updated, found, err := taskByID(ctx, tx.connection, id)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Task{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Task{}, err
	}
	return updated, nil
}
