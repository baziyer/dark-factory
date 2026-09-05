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
}

// TaskPatch is the console's queue edit. A nil member leaves the durable
// column alone; Cancel is the only status transition the console may ask for.
type TaskPatch struct {
	Title           *string
	Priority        *int64
	AssignedAgentID *AgentID
	Cancel          bool
}

// UpdateAgent applies one bounded configuration edit to an existing agent at
// an exact observed revision. The launch controls go through the same domain
// validation CreateAgent uses, so the wire cannot widen what a provider may
// be launched with.
func (store *Store) UpdateAgent(ctx context.Context, id AgentID, expected Revision, patch AgentPatch, at UnixMillis) (Agent, error) {
	if id.zero() || expected.Int64() < 1 {
		return Agent{}, fmt.Errorf("%w: invalid agent update", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Agent{}, err
	}
	defer tx.Close()
	agent, found, err := agentByID(ctx, tx.connection, id)
	if err != nil {
		return Agent{}, tx.Rollback(err)
	}
	if !found {
		return Agent{}, tx.Rollback(ErrNotFound)
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
	result, err := tx.connection.ExecContext(ctx, `UPDATE agents SET model = ?, reasoning_effort = ?, account_id = ?, paused = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`,
		nullableString(agent.Model), nullableString(agent.ReasoningEffort), nullableID(agent.AccountID), boolInt(agent.Paused), at.Int64(), id.Bytes(), expected.Int64())
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
	if id.zero() || expected.Int64() < 1 {
		return Task{}, fmt.Errorf("%w: invalid task update", ErrInvalidValue)
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
	if task.Status != TaskQueued {
		return Task{}, tx.Rollback(ErrConflict)
	}
	if task.Revision != expected || at.Int64() < task.UpdatedAt.Int64() {
		return Task{}, tx.Rollback(ErrRevisionConflict)
	}
	if patch.Title != nil {
		task.Title = *patch.Title
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
		task.AssignedAgentID = agent.ID
	}
	status, completed := task.Status.String(), any(nil)
	if patch.Cancel {
		status, completed = TaskCancelled.String(), at.Int64()
	}
	if byteLen(task.Title) < 1 || byteLen(task.Title) > 1024 || task.Priority < -1_000_000 || task.Priority > 1_000_000 {
		return Task{}, tx.Rollback(fmt.Errorf("%w: invalid task update", ErrInvalidValue))
	}
	result, err := tx.connection.ExecContext(ctx, `UPDATE tasks SET title = ?, priority = ?, assigned_agent_id = ?, status = ?, completed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND status = 'queued' AND revision = ?`,
		task.Title, task.Priority, task.AssignedAgentID.Bytes(), status, completed, at.Int64(), id.Bytes(), expected.Int64())
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
