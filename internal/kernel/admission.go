package kernel

import (
	"bytes"
	"context"
	"database/sql"
	"errors"
	"fmt"
)

func (store *Store) AdmitNext(ctx context.Context, keys AdmissionKeys, at UnixMillis) (AdmissionResult, error) {
	if !keys.valid() {
		return AdmissionResult{}, fmt.Errorf("%w: invalid admission request", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return AdmissionResult{}, err
	}
	defer tx.Close()

	factory, err := factoryState(ctx, tx.connection)
	if err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	}
	if result, found, err := reconcileAdmissionOnConnection(ctx, tx.connection, keys); err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	} else if found {
		if err := tx.Rollback(nil); err != nil {
			return AdmissionResult{}, err
		}
		return result, nil
	}
	if at.Int64() < factory.updatedAt.Int64() {
		return AdmissionResult{}, tx.Rollback(ErrRevisionConflict)
	}
	if !factory.DispatchEnabled {
		return rollbackNoAdmission(tx, NoAdmissionDispatchDisabled)
	}
	task, found, err := scanTask(tx.connection.QueryRowContext(ctx, `SELECT t.id, t.project_id, t.assigned_agent_id, t.incarnation_id, t.work_revision, t.title, t.body, t.sent_back_instruction_bytes, t.status, t.priority, t.blocked_reason, t.result, t.completed_at_ms, t.revision, t.created_at_ms, t.updated_at_ms
		FROM tasks AS t
		JOIN agents AS a ON a.id = t.assigned_agent_id AND a.project_id = t.project_id
		WHERE t.status = 'queued'
		  AND a.paused = 0
		  AND a.tool_calls_used < a.tool_budget_limit
		  AND NOT EXISTS (SELECT 1 FROM runs AS r WHERE r.agent_id = a.id AND r.phase <> 'terminal')
		  AND ((a.role = 'worker' AND (SELECT COUNT(*) FROM runs WHERE role = 'worker' AND phase <> 'terminal') < ?)
		    OR (a.role = 'orchestrator' AND (SELECT COUNT(*) FROM runs WHERE role = 'orchestrator' AND phase <> 'terminal') < 1))
		ORDER BY t.priority DESC, t.created_at_ms ASC, t.id ASC
		LIMIT 1`, factory.Capacity))
	if err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	}
	if !found {
		var queued int64
		if err := tx.connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM tasks WHERE status = 'queued'`).Scan(&queued); err != nil {
			return AdmissionResult{}, tx.Rollback(err)
		}
		if queued == 0 {
			return rollbackNoAdmission(tx, NoAdmissionQueueEmpty)
		}
		var capacityBlocked int
		if err := tx.connection.QueryRowContext(ctx, `SELECT EXISTS(
			SELECT 1 FROM tasks AS t
			JOIN agents AS a ON a.id = t.assigned_agent_id AND a.project_id = t.project_id
			WHERE t.status = 'queued' AND a.paused = 0 AND a.tool_calls_used < a.tool_budget_limit
			  AND NOT EXISTS (SELECT 1 FROM runs AS r WHERE r.agent_id = a.id AND r.phase <> 'terminal')
			  AND ((a.role = 'worker' AND (SELECT COUNT(*) FROM runs WHERE role = 'worker' AND phase <> 'terminal') >= ?)
			    OR (a.role = 'orchestrator' AND (SELECT COUNT(*) FROM runs WHERE role = 'orchestrator' AND phase <> 'terminal') >= 1))
		)`, factory.Capacity).Scan(&capacityBlocked); err != nil {
			return AdmissionResult{}, tx.Rollback(err)
		}
		if capacityBlocked != 0 {
			return rollbackNoAdmission(tx, NoAdmissionAtCapacity)
		}
		return rollbackNoAdmission(tx, NoAdmissionNoEligibleWork)
	}
	agent, found, err := agentByID(ctx, tx.connection, task.AssignedAgentID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return AdmissionResult{}, tx.Rollback(err)
	}
	if task.ProjectID != agent.ProjectID {
		return AdmissionResult{}, tx.Rollback(ErrCorruptState)
	}
	if at.Int64() < task.UpdatedAt.Int64() {
		return AdmissionResult{}, tx.Rollback(ErrRevisionConflict)
	}
	project, found, err := projectByID(ctx, tx.connection, task.ProjectID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return AdmissionResult{}, tx.Rollback(err)
	}
	if err := validateAdmissionLocatorOwnership(ctx, tx.connection, keys); err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	}

	var change *Change
	changeRevision := int64(0)
	switch agent.Role {
	case RoleWorker:
		value, err := bindAdmissionChange(ctx, tx.connection, task, keys.CandidateChangeID, at)
		if err != nil {
			return AdmissionResult{}, tx.Rollback(err)
		}
		change, changeRevision = &value, value.Revision.Int64()
	case RoleOrchestrator:
	default:
		return AdmissionResult{}, tx.Rollback(ErrCorruptState)
	}

	updatedTaskRevision := task.Revision.Int64() + 1
	result, err := tx.connection.ExecContext(ctx, `UPDATE tasks SET status = 'running', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND project_id = ? AND incarnation_id = ? AND work_revision = ? AND status = 'queued' AND revision = ?`,
		at.Int64(), task.ID.Bytes(), task.ProjectID.Bytes(), task.IncarnationID.Bytes(), task.WorkRevision.Int64(), task.Revision.Int64())
	if err := requireOneRow(result, err); err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	}
	var changeID any
	var admittedChangeRevision any
	if change != nil {
		changeID = change.ID.Bytes()
		admittedChangeRevision = change.Revision.Int64()
	}
	_, err = tx.connection.ExecContext(ctx, `INSERT INTO runs(
		id, project_id, agent_id, task_id, task_incarnation_id, admitted_task_work_revision,
		change_id, admitted_change_revision, role, provider, model, reasoning_effort, verification_policy, phase,
		proposal_kind, proposal_code, proposal_detail, proposal_result,
		terminal_kind, terminal_code, terminal_detail, terminal_result,
		credential_digest, result_proof_digest, credential_revoked_at_ms,
		provider_exit_kind, provider_exit_sequence, provider_exit_code, provider_exit_signal, provider_exit_at_ms,
		runner_exit_kind, runner_exit_sequence, runner_exit_code, runner_exit_signal, runner_exit_at_ms,
		revision, admitted_at_ms, running_at_ms, finalizing_at_ms, terminal_at_ms, updated_at_ms
		) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'admitted', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, ?, ?, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 1, ?, NULL, NULL, NULL, ?)`,
		keys.RunID.Bytes(), task.ProjectID.Bytes(), agent.ID.Bytes(), task.ID.Bytes(), task.IncarnationID.Bytes(), task.WorkRevision.Int64(),
		changeID, admittedChangeRevision, agent.Role.String(), agent.Provider.String(), nullableString(agent.Model), nullableString(agent.ReasoningEffort), project.VerificationPolicy.String(),
		keys.AttemptDigest.Bytes(), keys.ResultProofDigest.bytes(), at.Int64(), at.Int64())
	if err != nil {
		return AdmissionResult{}, tx.Rollback(classifyAdmissionConflict(ctx, tx.connection, keys, err))
	}
	inserted, err := tx.connection.ExecContext(ctx, `INSERT INTO terminal_sessions(id, run_id, state, unresolved_reason, revision, declared_at_ms, activated_at_ms, closed_at_ms, updated_at_ms) VALUES(?, ?, 'declared', NULL, 1, ?, NULL, NULL, ?)`, keys.TerminalSessionID.Bytes(), keys.RunID.Bytes(), at.Int64(), at.Int64())
	if err := requireOneRow(inserted, err); err != nil {
		return AdmissionResult{}, tx.Rollback(classifyAdmissionConflict(ctx, tx.connection, keys, err))
	}
	resources := []struct {
		id   ResourceID
		kind ResourceKind
		path any
	}{
		{keys.Resources.RuntimeRoot, ResourceRuntimeRoot, keys.RuntimeRoot},
		{keys.Resources.RunnerProcess, ResourceRunnerProcess, nil},
		{keys.Resources.ProviderProcess, ResourceProviderProcess, nil},
		{keys.Resources.ProviderGroup, ResourceProviderGroup, nil},
	}
	for _, resource := range resources {
		inserted, err := tx.connection.ExecContext(ctx, `INSERT INTO resources(id, run_id, kind, state, path, revision, declared_at_ms, updated_at_ms) VALUES(?, ?, ?, 'declared', ?, 1, ?, ?)`, resource.id.Bytes(), keys.RunID.Bytes(), resource.kind.String(), resource.path, at.Int64(), at.Int64())
		if err := requireOneRow(inserted, err); err != nil {
			return AdmissionResult{}, tx.Rollback(classifyAdmissionConflict(ctx, tx.connection, keys, err))
		}
	}
	factoryRevision := factory.Revision.Int64() + 1
	result, err = tx.connection.ExecContext(ctx, `UPDATE factory SET revision = revision + 1, updated_at_ms = ? WHERE singleton = 1 AND revision = ?`, at.Int64(), factory.Revision.Int64())
	if err := requireOneRow(result, err); err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	}
	pending := []pendingInvalidation{
		{kind: EntityFactory, id: factoryEntityID[:], revision: factoryRevision},
		{kind: EntityTask, id: task.ID.Bytes(), revision: updatedTaskRevision},
	}
	if change != nil {
		pending = append(pending, pendingInvalidation{kind: EntityChange, id: change.ID.Bytes(), revision: changeRevision})
	}
	pending = append(pending, pendingInvalidation{kind: EntityRun, id: keys.RunID.Bytes(), revision: 1})
	if err := appendInvalidations(ctx, tx.connection, at, pending); err != nil {
		return AdmissionResult{}, tx.Rollback(err)
	}
	run, found, err := runByID(ctx, tx.connection, keys.RunID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return AdmissionResult{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return AdmissionResult{}, err
	}
	return AdmissionResult{Run: &run}, nil
}

func rollbackNoAdmission(tx *writeTx, reason NoAdmissionReason) (AdmissionResult, error) {
	if err := tx.Rollback(nil); err != nil {
		return AdmissionResult{}, err
	}
	return AdmissionResult{Reason: reason}, nil
}

func bindAdmissionChange(ctx context.Context, connection *sql.Conn, task Task, candidate ChangeID, at UnixMillis) (Change, error) {
	existing, found, err := changeForTask(ctx, connection, task)
	if err != nil {
		return Change{}, err
	}
	if found {
		if at.Int64() < existing.UpdatedAt.Int64() {
			return Change{}, ErrRevisionConflict
		}
		if task.WorkRevision.Int64() <= 1 || existing.SettledRunID == nil || existing.Phase != ChangeRetained && existing.Phase != ChangeAbandoned {
			return Change{}, ErrCorruptState
		}
		predecessor, found, err := runByID(ctx, connection, *existing.SettledRunID)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return Change{}, err
		}
		if predecessor.Phase != RunTerminal || predecessor.Role != RoleWorker || predecessor.ChangeID == nil || *predecessor.ChangeID != existing.ID || predecessor.ProjectID != task.ProjectID || predecessor.TaskID != task.ID || predecessor.TaskIncarnationID != task.IncarnationID || predecessor.AdmittedTaskWorkRevision.Int64()+1 != task.WorkRevision.Int64() {
			return Change{}, ErrCorruptState
		}
		next := existing.Revision.Int64() + 1
		if existing.Phase == ChangeRetained {
			result, err := connection.ExecContext(ctx, `UPDATE changes SET phase = 'available', settled_run_id = NULL, revision = ?, updated_at_ms = ? WHERE id = ? AND phase = 'retained' AND revision = ? AND settled_run_id = ?`, next, at.Int64(), existing.ID.Bytes(), existing.Revision.Int64(), predecessor.ID.Bytes())
			if err := requireOneRow(result, err); err != nil {
				return Change{}, err
			}
		} else {
			result, err := connection.ExecContext(ctx, `UPDATE changes SET phase = 'reserved', settled_run_id = NULL, revision = ?, updated_at_ms = ? WHERE id = ? AND phase = 'abandoned' AND revision = ? AND settled_run_id = ?`, next, at.Int64(), existing.ID.Bytes(), existing.Revision.Int64(), predecessor.ID.Bytes())
			if err := requireOneRow(result, err); err != nil {
				return Change{}, err
			}
		}
		updated, found, err := changeByID(ctx, connection, existing.ID)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return Change{}, err
		}
		return updated, nil
	}
	if task.WorkRevision.Int64() != 1 {
		return Change{}, ErrCorruptState
	}
	_, err = connection.ExecContext(ctx, `INSERT INTO changes(id, project_id, task_id, task_incarnation_id, phase, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, ?, 'reserved', 1, ?, ?)`, candidate.Bytes(), task.ProjectID.Bytes(), task.ID.Bytes(), task.IncarnationID.Bytes(), at.Int64(), at.Int64())
	if err != nil {
		return Change{}, err
	}
	created, found, err := changeByID(ctx, connection, candidate)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Change{}, err
	}
	return created, nil
}

func classifyAdmissionConflict(ctx context.Context, connection *sql.Conn, keys AdmissionKeys, cause error) error {
	if _, found, reconcileErr := reconcileAdmissionOnConnection(ctx, connection, keys); found && reconcileErr == nil {
		return cause
	}
	return errors.Join(ErrConflict, cause)
}

func (store *Store) ReconcileAdmission(ctx context.Context, keys AdmissionKeys) (AdmissionResult, error) {
	if !keys.valid() {
		return AdmissionResult{}, fmt.Errorf("%w: invalid admission keys", ErrInvalidValue)
	}
	tx, err := store.beginRead(ctx)
	if err != nil {
		return AdmissionResult{}, err
	}
	defer tx.Close()
	result, found, err := reconcileAdmissionOnConnection(ctx, tx.connection, keys)
	if err != nil {
		return AdmissionResult{}, err
	}
	if !found {
		return AdmissionResult{Reason: NoAdmissionNotReconciled}, nil
	}
	return result, nil
}

func reconcileAdmissionOnConnection(ctx context.Context, connection *sql.Conn, keys AdmissionKeys) (AdmissionResult, bool, error) {
	run, found, err := runByID(ctx, connection, keys.RunID)
	if err != nil || !found {
		return AdmissionResult{}, found, err
	}
	if !bytes.Equal(run.CredentialDigest.Bytes(), keys.AttemptDigest.Bytes()) || !bytes.Equal(run.resultProofDigest.bytes(), keys.ResultProofDigest.bytes()) {
		return AdmissionResult{}, true, ErrConflict
	}
	relationships, err := loadRunRelationships(ctx, connection, run)
	if err != nil {
		return AdmissionResult{}, true, err
	}
	if err := validateAdmissionLocatorOwnership(ctx, connection, keys); err != nil {
		return AdmissionResult{}, true, err
	}
	if run.Role == RoleOrchestrator {
		if run.ChangeID != nil || run.AdmittedChangeRevision != nil {
			return AdmissionResult{}, true, ErrConflict
		}
	} else if run.Role == RoleWorker {
		if run.ChangeID == nil || run.AdmittedChangeRevision == nil {
			return AdmissionResult{}, true, ErrConflict
		}
		change := relationships.change
		if change == nil {
			return AdmissionResult{}, true, ErrCorruptState
		}
		if change.ProjectID != run.ProjectID || change.TaskID != run.TaskID || change.TaskIncarnationID != run.TaskIncarnationID {
			return AdmissionResult{}, true, ErrConflict
		}
		if run.AdmittedChangeRevision.Int64() == 1 && *run.ChangeID != keys.CandidateChangeID || change.Revision.Int64() < run.AdmittedChangeRevision.Int64() {
			return AdmissionResult{}, true, ErrConflict
		}
	} else {
		return AdmissionResult{}, true, ErrCorruptState
	}
	resources := relationships.resources
	expected := map[ResourceKind]ResourceID{
		ResourceRuntimeRoot:     keys.Resources.RuntimeRoot,
		ResourceRunnerProcess:   keys.Resources.RunnerProcess,
		ResourceProviderProcess: keys.Resources.ProviderProcess,
		ResourceProviderGroup:   keys.Resources.ProviderGroup,
	}
	if len(resources) != len(expected) {
		return AdmissionResult{}, true, ErrConflict
	}
	for _, resource := range resources {
		id, ok := expected[resource.Kind]
		if !ok || resource.ID != id || resource.RunID != run.ID || resource.Kind == ResourceRuntimeRoot && resource.Path != keys.RuntimeRoot {
			return AdmissionResult{}, true, ErrConflict
		}
	}
	session, sessionFound, err := terminalSessionByRunID(ctx, connection, run.ID)
	if err != nil {
		return AdmissionResult{}, true, err
	}
	if !sessionFound || session.ID != keys.TerminalSessionID || session.RunID != run.ID || session.State != TerminalSessionDeclared || session.Revision.Int64() != 1 || session.DeclaredAt.Int64() != run.AdmittedAt.Int64() {
		return AdmissionResult{}, true, ErrConflict
	}
	return AdmissionResult{Run: &run}, true, nil
}
