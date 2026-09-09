package daemon

import (
	"context"
	"fmt"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
)

// prepareQueuedTaskPatch checks the effective task text only when an edit can
// change it or its provider. Priority and cancellation remain possible for a
// legacy queued body a current provider would refuse.
func prepareQueuedTaskPatch(ctx context.Context, store *kernel.Store, id kernel.TaskID, expected kernel.Revision, patch kernel.TaskPatch) error {
	if patch.Title == nil && patch.Body == nil && patch.AssignedAgentID == nil {
		return nil
	}
	task, found, err := store.Task(ctx, id)
	if err != nil {
		return err
	}
	if !found {
		return kernel.ErrNotFound
	}
	if task.Revision != expected {
		return kernel.ErrRevisionConflict
	}
	if task.Status != kernel.TaskQueued {
		return kernel.ErrConflict
	}
	if patch.Title != nil {
		task.Title = *patch.Title
	}
	if patch.Body != nil {
		task.Body, task.SentBackInstructionBytes = kernel.TaskBodyWithInstruction(task, *patch.Body)
	}
	if patch.AssignedAgentID != nil {
		task.AssignedAgentID = *patch.AssignedAgentID
	}
	agent, found, err := store.Agent(ctx, task.AssignedAgentID)
	if err != nil {
		return err
	}
	if !found || agent.ProjectID != task.ProjectID {
		return kernel.ErrConflict
	}
	effectiveBody := task.Body
	if effectiveBody == "" && agent.Provider != kernel.ProviderShell {
		effectiveBody = task.Title
	}
	if _, _, err := provider.PrepareTask(agent.Provider, []byte(effectiveBody)); err != nil {
		return fmt.Errorf("%w: task does not fit provider", kernel.ErrInvalidValue)
	}
	return nil
}
