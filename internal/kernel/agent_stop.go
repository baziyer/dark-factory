package kernel

import (
	"context"
	"crypto/sha256"
	"fmt"
)

// StopRunForBrowser commits the intervention, optional successor and normal
// cancellation together. Cleanup still owns release and next-task admission.
func (store *Store) StopRunForBrowser(ctx context.Context, clientID BrowserClientID, request TaskInterventionRequest, successor *NewTask, at UnixMillis) (TaskIntervention, error) {
	if clientID.zero() || request.Actor != 0 || request.ActorRunID != nil || request.ActorBrowserClientID != nil {
		return TaskIntervention{}, ErrInvalidValue
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TaskIntervention{}, err
	}
	defer tx.Close()
	client, found, err := browserClientByID(ctx, tx.connection, clientID)
	if err != nil {
		return TaskIntervention{}, err
	}
	if !found || client.RevokedAt != nil || !client.CapabilityMask.Has(BrowserCapabilityHumanActions) {
		return TaskIntervention{}, ErrUnauthorized
	}
	request.Actor, request.ActorBrowserClientID = TaskInterventionOperator, &clientID
	return store.stopRunTx(ctx, tx, request, successor, at)
}

func (store *Store) StopRunForAttempt(ctx context.Context, digest AttemptDigest, request TaskInterventionRequest, successor *NewTask, at UnixMillis) (TaskIntervention, error) {
	if request.Actor != 0 || request.ActorRunID != nil || request.ActorBrowserClientID != nil {
		return TaskIntervention{}, ErrInvalidValue
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TaskIntervention{}, err
	}
	defer tx.Close()
	actor, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil {
		return TaskIntervention{}, err
	}
	if !found || actor.Role != RoleOrchestrator || actor.Phase != RunRunning || actor.CredentialRevokedAt != nil {
		return TaskIntervention{}, ErrUnauthorized
	}
	request.Actor, request.ActorRunID = TaskInterventionOrchestrator, &actor.ID
	return store.stopRunTx(ctx, tx, request, successor, at)
}

func (store *Store) stopRunTx(ctx context.Context, tx *writeTx, request TaskInterventionRequest, successor *NewTask, at UnixMillis) (TaskIntervention, error) {
	if request.Payload != "" || request.SuccessorTaskID != nil || request.Kind != TaskInterventionStop && request.Kind != TaskInterventionReplace || (request.Kind == TaskInterventionReplace) != (successor != nil) {
		return TaskIntervention{}, ErrInvalidValue
	}
	task, found, err := taskByID(ctx, tx.connection, request.TaskID)
	if err != nil {
		return TaskIntervention{}, err
	}
	if !found {
		return TaskIntervention{}, ErrNotFound
	}
	run, found, err := runByID(ctx, tx.connection, request.RunID)
	if err != nil {
		return TaskIntervention{}, err
	}
	if !found {
		return TaskIntervention{}, ErrNotFound
	}
	if run.TaskID != task.ID {
		return TaskIntervention{}, ErrConflict
	}
	if request.Actor == TaskInterventionOrchestrator && run.Role != RoleWorker {
		return TaskIntervention{}, ErrUnauthorized
	}
	if request.ActorRunID != nil {
		actor, found, err := runByID(ctx, tx.connection, *request.ActorRunID)
		if err != nil {
			return TaskIntervention{}, err
		}
		if !found || actor.ProjectID != task.ProjectID || actor.ID == run.ID {
			return TaskIntervention{}, ErrUnauthorized
		}
	}
	var spec NewTask
	if successor != nil {
		spec = NewTask{ID: successor.ID, IncarnationID: successor.IncarnationID, ProjectID: task.ProjectID, AssignedAgentID: task.AssignedAgentID, Title: "Direct instruction", Body: successor.Body, Priority: task.Priority}
		if spec.ID == task.ID {
			return TaskIntervention{}, ErrInvalidValue
		}
		if err := validateNewTask(spec); err != nil {
			return TaskIntervention{}, err
		}
		request.SuccessorTaskID = &spec.ID
		// The task keeps the full instruction; the receipt binds it and incarnation
		// without duplicating the body or shrinking the provider's prompt budget.
		request.Payload = fmt.Sprintf("%s:%x", spec.IncarnationID.String(), sha256.Sum256([]byte(spec.Body)))
	}
	if err := request.valid(); err != nil {
		return TaskIntervention{}, err
	}
	existing, found, err := taskInterventionByID(ctx, tx.connection, request.OperationID)
	if err != nil {
		return TaskIntervention{}, err
	}
	if found {
		if !taskInterventionMatchesRequest(existing, request) {
			return TaskIntervention{}, ErrConflict
		}
		if existing.State != TaskInterventionDelivered {
			return TaskIntervention{}, ErrConflict
		}
		return existing, nil
	}
	if successor != nil {
		if _, err := insertTaskOnConnection(ctx, tx.connection, spec, at); err != nil {
			return TaskIntervention{}, err
		}
	}
	if _, _, err := reserveTaskInterventionTx(ctx, tx, request, at); err != nil {
		return TaskIntervention{}, err
	}
	receipt, err := resolveTaskInterventionTx(ctx, tx, request.OperationID, TaskInterventionDelivered, "", at)
	if err != nil {
		return TaskIntervention{}, err
	}
	detail := "Stopped by " + request.Actor.String()
	if successor != nil {
		detail = "Replaced by task " + spec.ID.String() + " by " + request.Actor.String()
	}
	proposal, err := NewCancelledProposal(detail)
	if err != nil {
		return TaskIntervention{}, err
	}
	// enterFinalizing commits this shared transaction, including the receipt and
	// optional successor above, so a stop is durable before it returns.
	if _, err := store.enterFinalizing(ctx, tx, run, request.ExpectedRunRevision, proposal, at, nil); err != nil {
		return TaskIntervention{}, err
	}
	return receipt, nil
}
