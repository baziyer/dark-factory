package kernel

import (
	"context"
	"database/sql"
	"fmt"
)

type lifecycleFootprint struct {
	runtime         Resource
	runner          Resource
	providerProcess Resource
	providerGroup   Resource
	session         TerminalSession
}

func loadLifecycleFootprint(ctx context.Context, connection *sql.Conn, run Run) (lifecycleFootprint, error) {
	resources, err := resourcesForRun(ctx, connection, run.ID)
	if err != nil {
		return lifecycleFootprint{}, err
	}
	if !exactResourceSet(resources, false) {
		return lifecycleFootprint{}, ErrCorruptState
	}
	var result lifecycleFootprint
	for _, resource := range resources {
		switch resource.Kind {
		case ResourceRuntimeRoot:
			result.runtime = resource
		case ResourceRunnerProcess:
			result.runner = resource
		case ResourceProviderProcess:
			result.providerProcess = resource
		case ResourceProviderGroup:
			result.providerGroup = resource
		}
	}
	result.session, _, err = terminalSessionByRunID(ctx, connection, run.ID)
	if err != nil {
		return lifecycleFootprint{}, err
	}
	if result.session.ID.zero() {
		return lifecycleFootprint{}, ErrCorruptState
	}
	if !providerPairConsistent(result.providerProcess, result.providerGroup) {
		return lifecycleFootprint{}, ErrCorruptState
	}
	return result, nil
}

func providerPairConsistent(process, group Resource) bool {
	return process.RunID == group.RunID && process.Kind == ResourceProviderProcess && group.Kind == ResourceProviderGroup &&
		process.State == group.State && resourceIdentityEqual(process.Identity, group.Identity) &&
		(process.ActivatedAt == nil) == (group.ActivatedAt == nil) &&
		(process.ActivatedAt == nil || *process.ActivatedAt == *group.ActivatedAt) &&
		(process.ReleasedAt == nil) == (group.ReleasedAt == nil) &&
		(process.ReleasedAt == nil || *process.ReleasedAt == *group.ReleasedAt) &&
		process.UnresolvedReason == group.UnresolvedReason
}

func lifecycleTimesValid(run Run, footprint lifecycleFootprint, at UnixMillis) bool {
	return at.Int64() >= run.UpdatedAt.Int64() && at.Int64() >= footprint.runtime.UpdatedAt.Int64() &&
		at.Int64() >= footprint.runner.UpdatedAt.Int64() && at.Int64() >= footprint.providerProcess.UpdatedAt.Int64() &&
		at.Int64() >= footprint.providerGroup.UpdatedAt.Int64() && at.Int64() >= footprint.session.UpdatedAt.Int64()
}

// BeginRunnerStart durably records permission for the sole outer Start call.
// Replaying the exact predecessor revisions is safe; any other starting row is
// ambiguity and fails closed.
func (store *Store) BeginRunnerStart(ctx context.Context, runID RunID, runnerID ResourceID, expectedRun, expectedRunner Revision, at UnixMillis) (Run, Resource, error) {
	if runID.zero() || runnerID.zero() {
		return Run{}, Resource{}, fmt.Errorf("%w: invalid runner start identifiers", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, Resource{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if footprint.runner.ID != runnerID {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if run.Phase == RunAdmitted && run.Revision.Int64() == expectedRun.Int64()+1 && footprint.runner.State == ResourceStarting && footprint.runner.Revision.Int64() == expectedRunner.Int64()+1 {
		return run, footprint.runner, tx.Rollback(nil)
	}
	if run.Phase != RunAdmitted || run.Proposal != nil || run.CredentialRevokedAt != nil || run.Revision != expectedRun ||
		footprint.runtime.State != ResourceActive || footprint.runtime.Identity.Empty() ||
		footprint.runner.State != ResourceDeclared || !footprint.runner.Identity.Empty() || footprint.runner.Revision != expectedRunner ||
		footprint.providerProcess.State != ResourceDeclared || !footprint.providerProcess.Identity.Empty() ||
		footprint.session.State != TerminalSessionDeclared || footprint.session.ActivatedAt != nil || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'starting', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runner_process' AND state = 'declared' AND revision = ? AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND activated_at_ms IS NULL`, at.Int64(), runnerID.Bytes(), runID.Bytes(), expectedRunner.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE runs SET revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'admitted' AND proposal_kind IS NULL AND credential_revoked_at_ms IS NULL AND revision = ?`, at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	run, found, err = runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	runner, found, err := resourceByID(ctx, tx.connection, runnerID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, Resource{}, err
	}
	return run, runner, nil
}

// ActivateRunner binds successful Start to its exact identity. It is the only
// starting-to-active edge.
func (store *Store) ActivateRunner(ctx context.Context, runID RunID, runnerID ResourceID, expectedRun, expectedRunner Revision, identity ResourceIdentity, at UnixMillis) (Run, Resource, error) {
	if runID.zero() || runnerID.zero() || !identity.validFor(ResourceRunnerProcess) {
		return Run{}, Resource{}, fmt.Errorf("%w: invalid runner activation", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, Resource{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if footprint.runner.ID != runnerID {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if run.Phase == RunAdmitted && run.Revision.Int64() == expectedRun.Int64()+1 && footprint.runner.State == ResourceActive && footprint.runner.Revision.Int64() == expectedRunner.Int64()+1 && resourceIdentityEqual(footprint.runner.Identity, identity) {
		return run, footprint.runner, tx.Rollback(nil)
	}
	if run.Phase != RunAdmitted || run.Proposal != nil || run.Revision != expectedRun || footprint.runner.State != ResourceStarting || footprint.runner.Revision != expectedRunner || !footprint.runner.Identity.Empty() || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if err := ensureResourceIdentityUnused(ctx, tx.connection, footprint.runner, identity); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	pid, pgid, birth, _ := identity.Process()
	updated, err := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'active', pid = ?, pgid = ?, birth_digest = ?, activated_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runner_process' AND state = 'starting' AND revision = ? AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND activated_at_ms IS NULL`, pid, pgid, birth.Bytes(), at.Int64(), at.Int64(), runnerID.Bytes(), runID.Bytes(), expectedRunner.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE runs SET revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'admitted' AND proposal_kind IS NULL AND revision = ?`, at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	runner, _, err := resourceByID(ctx, tx.connection, runnerID)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, Resource{}, err
	}
	return run, runner, nil
}

// RecordUnregisteredRunnerConverged consumes trusted evidence that the sole
// Start either failed without a child or that any child was positively reaped
// before an inner process identity was registered. Recovery may use the same
// edge only after exact lifetime and marker evidence proves that postcondition.
func (store *Store) RecordUnregisteredRunnerConverged(ctx context.Context, runID RunID, runnerID ResourceID, expectedRun, expectedRunner Revision, at UnixMillis) (Run, error) {
	failure, _ := NewFailureProposal(FailureSpawn, "runner converged without a registered inner process")
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if footprint.runner.ID != runnerID {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if unregisteredRunnerConvergedPostcondition(run, footprint, failure, expectedRun, expectedRunner) {
		return run, tx.Rollback(nil)
	}
	if run.Phase != RunAdmitted || run.Proposal != nil || run.ProviderExit != nil || run.RunnerExit != nil || run.Revision != expectedRun ||
		footprint.runtime.State != ResourceActive || footprint.runner.State != ResourceStarting || footprint.runner.Revision != expectedRunner || !footprint.runner.Identity.Empty() ||
		footprint.providerProcess.State != ResourceDeclared || !footprint.providerProcess.Identity.Empty() || footprint.session.State != TerminalSessionDeclared || footprint.session.ActivatedAt != nil || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if err := requireFinalizingTime(ctx, tx.connection, run, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	kind, code, proposalDetail, result := proposalSQL(failure)
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'finalizing', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, credential_revoked_at_ms = ?, finalizing_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'admitted' AND proposal_kind IS NULL AND provider_exit_kind IS NULL AND runner_exit_kind IS NULL AND revision = ?`, kind, code, proposalDetail, result, at.Int64(), at.Int64(), at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runtime_root' AND state = 'active'`, at.Int64(), footprint.runtime.ID.Bytes(), runID.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND ((id = ? AND kind = 'runner_process' AND state = 'starting' AND revision = ?) OR (id = ? AND kind = 'provider_process' AND state = 'declared') OR (id = ? AND kind = 'provider_group' AND state = 'declared')) AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL`, at.Int64(), at.Int64(), runID.Bytes(), runnerID.Bytes(), expectedRunner.Int64(), footprint.providerProcess.ID.Bytes(), footprint.providerGroup.ID.Bytes())
	if err := requireRows(updated, err, 3); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'closed', closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND activated_at_ms IS NULL AND closed_at_ms IS NULL`, at.Int64(), at.Int64(), footprint.session.ID.Bytes(), runID.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	// The exact predecessor is admitted with a never-activated terminal. Human
	// requests require running attempt authority, so validated state proves
	// there is no request to converge on this pre-registration edge.
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

func unregisteredRunnerConvergedPostcondition(run Run, footprint lifecycleFootprint, failure Proposal, expectedRun, expectedRunner Revision) bool {
	return run.Phase == RunFinalizing && run.Revision.Int64() == expectedRun.Int64()+1 && run.Proposal != nil && run.Proposal.equal(failure) && run.ProviderExit == nil && run.RunnerExit == nil &&
		footprint.runtime.State == ResourceReleasing && footprint.runner.State == ResourceReleased && footprint.runner.Revision.Int64() == expectedRunner.Int64()+1 && footprint.runner.Identity.Empty() &&
		footprint.providerProcess.State == ResourceReleased && footprint.providerProcess.Identity.Empty() && footprint.providerGroup.State == ResourceReleased && footprint.providerGroup.Identity.Empty() && footprint.session.State == TerminalSessionClosed && footprint.session.ActivatedAt == nil
}

// ConsumeAttemptResult atomically consumes one exact runtime/attempt-bound
// result according to the closed admitted/running/finalizing matrix.
func (store *Store) ConsumeAttemptResult(ctx context.Context, result AttemptResult, expected Revision, at UnixMillis) (Run, error) {
	if !result.valid() {
		return Run{}, fmt.Errorf("%w: malformed attempt result", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, result.runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, tx.Rollback(err)
	}
	if run.CredentialDigest != result.attemptDigest || run.resultProofDigest != result.resultProofDigest || !attemptResultConsumablePhase(run.Phase) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !resourceIdentityEqual(footprint.runtime.Identity, result.runtimeIdentity) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if attemptResultConsumedPostcondition(run, footprint, result, expected) {
		return run, tx.Rollback(nil)
	}
	if run.Revision != expected || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	if err := validateAttemptResultPrecondition(run, footprint, result); err != nil {
		return Run{}, tx.Rollback(err)
	}
	wasFinalizing := run.Phase == RunFinalizing
	if !wasFinalizing {
		failureCode, detail := FailureActivation, "provider converged before activation completed"
		if result.kind == AttemptInnerUnregisteredConverged {
			failureCode, detail = FailureSpawn, "runner converged without a registered inner process"
		} else if run.Phase == RunRunning {
			failureCode, detail = FailureProviderExit, "provider exited before an attempt outcome"
		}
		proposal, _ := NewFailureProposal(failureCode, detail)
		kind, code, proposalDetail, proposalResult := proposalSQL(proposal)
		exitKind, exitCode, exitSignal := attemptResultExitSQL(result)
		updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'finalizing', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, credential_revoked_at_ms = ?, finalizing_at_ms = ?, provider_exit_kind = ?, provider_exit_sequence = ?, provider_exit_code = ?, provider_exit_signal = ?, provider_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = ? AND proposal_kind IS NULL AND credential_revoked_at_ms IS NULL AND provider_exit_kind IS NULL AND revision = ?`, kind, code, proposalDetail, proposalResult, at.Int64(), at.Int64(), exitKind, nullableResultSequence(result), exitCode, exitSignal, nullableResultTime(result, at), at.Int64(), run.ID.Bytes(), run.Phase.String(), expected.Int64())
		if err := requireOneRow(updated, err); err != nil {
			return Run{}, tx.Rollback(err)
		}
		updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND ((id = ? AND kind = 'runtime_root' AND state = 'active') OR (id = ? AND kind = 'runner_process' AND state = 'active'))`, at.Int64(), run.ID.Bytes(), footprint.runtime.ID.Bytes(), footprint.runner.ID.Bytes())
		if err := requireRows(updated, err, 2); err != nil {
			return Run{}, tx.Rollback(err)
		}
		if err := moveTerminalToReleasing(ctx, tx.connection, footprint.session, at); err != nil {
			return Run{}, tx.Rollback(err)
		}
		requestInvalidations, transitionErr := transitionHumanRequestsForRun(ctx, tx.connection, run.ID, at, false, nil)
		if transitionErr != nil {
			return Run{}, tx.Rollback(transitionErr)
		}
		if len(requestInvalidations) != 0 {
			if err := appendInvalidations(ctx, tx.connection, at, requestInvalidations); err != nil {
				return Run{}, tx.Rollback(err)
			}
		}
	} else {
		exitKind, exitCode, exitSignal := attemptResultExitSQL(result)
		updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET provider_exit_kind = ?, provider_exit_sequence = 1, provider_exit_code = ?, provider_exit_signal = ?, provider_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND proposal_kind IS NOT NULL AND credential_revoked_at_ms IS NOT NULL AND provider_exit_kind IS NULL AND revision = ?`, exitKind, exitCode, exitSignal, at.Int64(), at.Int64(), run.ID.Bytes(), expected.Int64())
		if err := requireOneRow(updated, err); err != nil {
			return Run{}, tx.Rollback(err)
		}
	}
	if err := releaseProviderPairFromResult(ctx, tx.connection, footprint, result, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: run.ID.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, run.ID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

func attemptResultConsumablePhase(phase RunPhase) bool {
	return phase == RunAdmitted || phase == RunRunning || phase == RunFinalizing
}

func validateAttemptResultPrecondition(run Run, footprint lifecycleFootprint, result AttemptResult) error {
	if run.Phase == RunAdmitted || run.Phase == RunRunning {
		if footprint.runtime.State != ResourceActive {
			return ErrConflict
		}
		wantSession := TerminalSessionDeclared
		if run.Phase == RunRunning {
			wantSession = TerminalSessionActive
		}
		if footprint.session.State != wantSession {
			return ErrConflict
		}
		// The attempt-runner target is the only trusted result writer and it
		// cannot execute before ActivateRunner commits, so every consumable
		// result implies an activated, identity-bound outer runner.
		if footprint.runner.State != ResourceActive || footprint.runner.Identity.Empty() {
			return ErrConflict
		}
		if result.kind == AttemptInnerUnregisteredConverged {
			if run.Phase != RunAdmitted || footprint.providerProcess.State != ResourceDeclared || !footprint.providerProcess.Identity.Empty() {
				return ErrConflict
			}
			return nil
		}
		if footprint.providerProcess.State == ResourceDeclared {
			if run.Phase != RunAdmitted || !footprint.providerProcess.Identity.Empty() {
				return ErrConflict
			}
			return nil
		}
		if footprint.providerProcess.State != ResourceActive || !resourceIdentityEqual(footprint.providerProcess.Identity, result.processIdentity) {
			return ErrConflict
		}
		return nil
	}
	if run.Phase != RunFinalizing || run.Proposal == nil || run.CredentialRevokedAt == nil || result.kind != AttemptInnerConverged ||
		(footprint.runtime.State != ResourceReleasing && footprint.runtime.State != ResourceUnresolved) ||
		(footprint.runner.State != ResourceReleasing && footprint.runner.State != ResourceUnresolved) ||
		(footprint.session.State != TerminalSessionReleasing && footprint.session.State != TerminalSessionUnresolved) ||
		(footprint.providerProcess.State != ResourceReleasing && footprint.providerProcess.State != ResourceUnresolved) ||
		!resourceIdentityEqual(footprint.providerProcess.Identity, result.processIdentity) || run.ProviderExit != nil {
		return ErrConflict
	}
	return nil
}

func releaseProviderPairFromResult(ctx context.Context, connection *sql.Conn, footprint lifecycleFootprint, result AttemptResult, at UnixMillis) error {
	if result.kind == AttemptInnerUnregisteredConverged {
		updated, err := connection.ExecContext(ctx, `UPDATE resources SET state = 'released', released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'declared' AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND kind IN ('provider_process', 'provider_group')`, at.Int64(), at.Int64(), footprint.providerProcess.RunID.Bytes())
		return requireRows(updated, err, 2)
	}
	pid, pgid, birth, _ := result.processIdentity.Process()
	if footprint.providerProcess.State == ResourceDeclared {
		updated, err := connection.ExecContext(ctx, `UPDATE resources SET state = 'released', pid = ?, pgid = ?, birth_digest = ?, activated_at_ms = ?, released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'declared' AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND kind IN ('provider_process', 'provider_group')`, pid, pgid, birth.Bytes(), at.Int64(), at.Int64(), at.Int64(), footprint.providerProcess.RunID.Bytes())
		return requireRows(updated, err, 2)
	}
	updated, err := connection.ExecContext(ctx, `UPDATE resources SET state = 'released', unresolved_reason = NULL, released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = ? AND pid = ? AND pgid = ? AND birth_digest = ? AND kind IN ('provider_process', 'provider_group')`, at.Int64(), at.Int64(), footprint.providerProcess.RunID.Bytes(), footprint.providerProcess.State.String(), pid, pgid, birth.Bytes())
	return requireRows(updated, err, 2)
}

func attemptResultExitSQL(result AttemptResult) (kind, code, signal any) {
	if result.kind != AttemptInnerConverged {
		return nil, nil, nil
	}
	if value, ok := result.exit.Code(); ok {
		return processExitCode.String(), value, nil
	}
	value, _ := result.exit.Signal()
	return processExitSignal.String(), nil, value
}

func nullableResultSequence(result AttemptResult) any {
	if result.kind == AttemptInnerConverged {
		return int64(1)
	}
	return nil
}

func nullableResultTime(result AttemptResult, at UnixMillis) any {
	if result.kind == AttemptInnerConverged {
		return at.Int64()
	}
	return nil
}

func attemptResultConsumedPostcondition(run Run, footprint lifecycleFootprint, result AttemptResult, expected Revision) bool {
	if run.Phase != RunFinalizing || run.Revision.Int64() != expected.Int64()+1 || run.Proposal == nil || footprint.providerProcess.State != ResourceReleased || footprint.providerGroup.State != ResourceReleased {
		return false
	}
	if result.kind == AttemptInnerUnregisteredConverged {
		return run.Proposal.code == FailureSpawn && run.ProviderExit == nil && footprint.providerProcess.Identity.Empty() &&
			(footprint.session.State == TerminalSessionReleasing || footprint.session.State == TerminalSessionUnresolved) &&
			(footprint.runner.State == ResourceReleasing || footprint.runner.State == ResourceUnresolved || footprint.runner.State == ResourceReleased)
	}
	if footprint.session.State != TerminalSessionReleasing && footprint.session.State != TerminalSessionUnresolved {
		return false
	}
	if !resourceIdentityEqual(footprint.providerProcess.Identity, result.processIdentity) || run.ProviderExit == nil || run.ProviderExit.Sequence() != 1 {
		return false
	}
	return attemptResultExitEqual(result.exit, run.ProviderExit)
}

// attemptResultExitEqual compares only the significant exit arm. The raw
// first return of Code/Signal is the union value for both arms on
// AttemptResultExit but a nil-guarded zero on ProcessExit, so comparing
// insignificant values would wrongly reject every nonzero exit.
func attemptResultExitEqual(exit AttemptResultExit, stored *ProcessExit) bool {
	if stored == nil {
		return false
	}
	if code, ok := exit.Code(); ok {
		storedCode, storedOK := stored.Code()
		return storedOK && code == storedCode
	}
	if signal, ok := exit.Signal(); ok {
		storedSignal, storedOK := stored.Signal()
		return storedOK && signal == storedSignal
	}
	return false
}

func moveTerminalToReleasing(ctx context.Context, connection *sql.Conn, session TerminalSession, at UnixMillis) error {
	generation := int64(session.LeaseGeneration)
	if session.LeaseClientID != nil {
		var err error
		generation, err = leaseGenerationNext(generation)
		if err != nil {
			return err
		}
	}
	updated, err := connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'releasing', lease_client_id = NULL, lease_expires_at_ms = NULL, lease_generation = ?, last_input_sequence = 0, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = ? AND revision = ?`, generation, at.Int64(), session.ID.Bytes(), session.RunID.Bytes(), session.State.String(), session.Revision.Int64())
	return requireOneRow(updated, err)
}

// MarkProviderResourcesUnresolved moves the provider process/group pair as one
// durable unit. A split pair is never a recoverable intermediate state.
func (store *Store) MarkProviderResourcesUnresolved(ctx context.Context, runID RunID, processID, groupID ResourceID, expectedRun, expectedProcess, expectedGroup Revision, identity ResourceIdentity, reason string, at UnixMillis) (Run, Resource, Resource, error) {
	if byteLen(reason) < 1 || byteLen(reason) > 4096 {
		return Run{}, Resource{}, Resource{}, fmt.Errorf("%w: invalid provider unresolved reason", ErrInvalidValue)
	}
	return store.updateProviderPair(ctx, runID, processID, groupID, expectedRun, expectedProcess, expectedGroup, identity, reason, ResourceUnresolved, at)
}

// ReleaseProviderResources releases the exact provider pair atomically.
func (store *Store) ReleaseProviderResources(ctx context.Context, runID RunID, processID, groupID ResourceID, expectedRun, expectedProcess, expectedGroup Revision, identity ResourceIdentity, at UnixMillis) (Run, Resource, Resource, error) {
	return store.updateProviderPair(ctx, runID, processID, groupID, expectedRun, expectedProcess, expectedGroup, identity, "", ResourceReleased, at)
}

func (store *Store) updateProviderPair(ctx context.Context, runID RunID, processID, groupID ResourceID, expectedRun, expectedProcess, expectedGroup Revision, identity ResourceIdentity, reason string, target ResourceState, at UnixMillis) (Run, Resource, Resource, error) {
	if runID.zero() || processID.zero() || groupID.zero() || processID == groupID || (target != ResourceUnresolved && target != ResourceReleased) {
		return Run{}, Resource{}, Resource{}, fmt.Errorf("%w: invalid provider pair transition", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, Resource{}, Resource{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	if footprint.providerProcess.ID != processID || footprint.providerGroup.ID != groupID || !resourceIdentityEqual(footprint.providerProcess.Identity, identity) {
		return Run{}, Resource{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if run.Phase == RunFinalizing && run.Revision.Int64() == expectedRun.Int64()+1 && footprint.providerProcess.State == target && footprint.providerProcess.Revision.Int64() == expectedProcess.Int64()+1 && footprint.providerGroup.Revision.Int64() == expectedGroup.Int64()+1 && footprint.providerProcess.UnresolvedReason == reason {
		return run, footprint.providerProcess, footprint.providerGroup, tx.Rollback(nil)
	}
	if run.Phase != RunFinalizing || run.Revision != expectedRun || footprint.providerProcess.Revision != expectedProcess || footprint.providerGroup.Revision != expectedGroup || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, Resource{}, Resource{}, tx.Rollback(ErrRevisionConflict)
	}
	if target == ResourceReleased && !identity.Empty() && run.ProviderExit == nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(ErrConflict)
	}
	allowed := footprint.providerProcess.State == ResourceReleasing || footprint.providerProcess.State == ResourceUnresolved
	if target == ResourceUnresolved {
		allowed = footprint.providerProcess.State == ResourceReleasing
	}
	if !allowed {
		return Run{}, Resource{}, Resource{}, tx.Rollback(ErrConflict)
	}
	pid, pgid, birth, processIdentity := identity.Process()
	if !processIdentity && !identity.Empty() {
		return Run{}, Resource{}, Resource{}, tx.Rollback(ErrConflict)
	}
	var updated sql.Result
	if target == ResourceUnresolved {
		updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'unresolved', unresolved_reason = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'releasing' AND revision IN (?, ?) AND kind IN ('provider_process', 'provider_group') AND ((pid IS NULL AND ? = 0) OR (pid = ? AND pgid = ? AND birth_digest = ? AND ? = 1))`, reason, at.Int64(), runID.Bytes(), expectedProcess.Int64(), expectedGroup.Int64(), boolInt(processIdentity), pid, pgid, birth.Bytes(), boolInt(processIdentity))
	} else {
		updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', unresolved_reason = NULL, released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = ? AND revision IN (?, ?) AND kind IN ('provider_process', 'provider_group') AND ((pid IS NULL AND ? = 0) OR (pid = ? AND pgid = ? AND birth_digest = ? AND ? = 1))`, at.Int64(), at.Int64(), runID.Bytes(), footprint.providerProcess.State.String(), expectedProcess.Int64(), expectedGroup.Int64(), boolInt(processIdentity), pid, pgid, birth.Bytes(), boolInt(processIdentity))
	}
	if err := requireRows(updated, err, 2); err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE runs SET revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND revision = ?`, at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	process, _, err := resourceByID(ctx, tx.connection, processID)
	if err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	group, _, err := resourceByID(ctx, tx.connection, groupID)
	if err != nil {
		return Run{}, Resource{}, Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, Resource{}, Resource{}, err
	}
	return run, process, group, nil
}

// RecordRecoveredRunnerAbsence records the caller's positive exact-identity
// absence proof and releases that runner in one transaction. It never signals
// or treats a numeric PID/PGID as authority.
func (store *Store) RecordRecoveredRunnerAbsence(ctx context.Context, runID RunID, runnerID ResourceID, expectedRun, expectedRunner Revision, identity ResourceIdentity, at UnixMillis) (Run, Resource, error) {
	if runID.zero() || runnerID.zero() || !identity.validFor(ResourceRunnerProcess) {
		return Run{}, Resource{}, fmt.Errorf("%w: invalid recovered runner absence", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, Resource{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if footprint.runner.ID != runnerID || !resourceIdentityEqual(footprint.runner.Identity, identity) {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if run.Phase == RunFinalizing && run.Revision.Int64() == expectedRun.Int64()+1 && footprint.runner.State == ResourceReleased && footprint.runner.Revision.Int64() == expectedRunner.Int64()+1 && run.RunnerExit != nil && run.RunnerExit.RecoveredAbsence() {
		return run, footprint.runner, tx.Rollback(nil)
	}
	if run.Phase != RunFinalizing || run.RunnerExit != nil || run.Revision != expectedRun || footprint.runner.Revision != expectedRunner || (footprint.runner.State != ResourceReleasing && footprint.runner.State != ResourceUnresolved) || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET runner_exit_kind = 'recovered_absence', runner_exit_sequence = 1, runner_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND runner_exit_kind IS NULL AND revision = ?`, at.Int64(), at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	pid, pgid, birth, _ := identity.Process()
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', unresolved_reason = NULL, released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runner_process' AND state = ? AND revision = ? AND pid = ? AND pgid = ? AND birth_digest = ?`, at.Int64(), at.Int64(), runnerID.Bytes(), runID.Bytes(), footprint.runner.State.String(), expectedRunner.Int64(), pid, pgid, birth.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	runner, _, err := resourceByID(ctx, tx.connection, runnerID)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, Resource{}, err
	}
	return run, runner, nil
}

// RecordRecoveredPreSessionRunnerAbsence finalizes the stranded pre-registration
// state: the runner row is active with a bound identity while the provider
// pair is still declared and the session never activated. The kernel cannot
// distinguish whether the outer exec was never released or the runner died
// after release but before registering anything; both histories share this
// exact durable state and the same convergence. The caller must hold positive
// exact-identity absence proof plus stable absence of activation/result
// residue; this edge then finalizes the run, records the recovered-absence
// runner exit and releases every runner-side resource in one transaction.
func (store *Store) RecordRecoveredPreSessionRunnerAbsence(ctx context.Context, runID RunID, runnerID ResourceID, expectedRun, expectedRunner Revision, identity ResourceIdentity, at UnixMillis) (Run, error) {
	if runID.zero() || runnerID.zero() || !identity.validFor(ResourceRunnerProcess) {
		return Run{}, fmt.Errorf("%w: invalid recovered pre-session runner absence", ErrInvalidValue)
	}
	failure, _ := NewFailureProposal(FailureActivation, "runner absent without provider registration, attempt result, or session activation")
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if footprint.runner.ID != runnerID || !resourceIdentityEqual(footprint.runner.Identity, identity) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if preSessionRunnerAbsencePostcondition(run, footprint, failure, expectedRun, expectedRunner) {
		return run, tx.Rollback(nil)
	}
	if run.Phase != RunAdmitted || run.Proposal != nil || run.ProviderExit != nil || run.RunnerExit != nil || run.Revision != expectedRun ||
		footprint.runtime.State != ResourceActive || footprint.runner.State != ResourceActive || footprint.runner.Revision != expectedRunner ||
		footprint.providerProcess.State != ResourceDeclared || !footprint.providerProcess.Identity.Empty() ||
		footprint.session.State != TerminalSessionDeclared || footprint.session.ActivatedAt != nil || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if err := requireFinalizingTime(ctx, tx.connection, run, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	kind, code, proposalDetail, result := proposalSQL(failure)
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'finalizing', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, credential_revoked_at_ms = ?, finalizing_at_ms = ?, runner_exit_kind = 'recovered_absence', runner_exit_sequence = 1, runner_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'admitted' AND proposal_kind IS NULL AND credential_revoked_at_ms IS NULL AND provider_exit_kind IS NULL AND runner_exit_kind IS NULL AND revision = ?`, kind, code, proposalDetail, result, at.Int64(), at.Int64(), at.Int64(), at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runtime_root' AND state = 'active'`, at.Int64(), footprint.runtime.ID.Bytes(), runID.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	pid, pgid, birth, _ := identity.Process()
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runner_process' AND state = 'active' AND revision = ? AND pid = ? AND pgid = ? AND birth_digest = ?`, at.Int64(), at.Int64(), runnerID.Bytes(), runID.Bytes(), expectedRunner.Int64(), pid, pgid, birth.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'declared' AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND kind IN ('provider_process', 'provider_group')`, at.Int64(), at.Int64(), runID.Bytes())
	if err := requireRows(updated, err, 2); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'closed', closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND activated_at_ms IS NULL AND closed_at_ms IS NULL`, at.Int64(), at.Int64(), footprint.session.ID.Bytes(), runID.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	// The exact predecessor is admitted with a never-activated terminal. Human
	// requests require running attempt authority, so validated state proves
	// there is no request to converge on this pre-session edge.
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

func preSessionRunnerAbsencePostcondition(run Run, footprint lifecycleFootprint, failure Proposal, expectedRun, expectedRunner Revision) bool {
	return run.Phase == RunFinalizing && run.Revision.Int64() == expectedRun.Int64()+1 && run.Proposal != nil && run.Proposal.equal(failure) &&
		run.ProviderExit == nil && run.RunnerExit != nil && run.RunnerExit.RecoveredAbsence() &&
		footprint.runtime.State == ResourceReleasing && footprint.runner.State == ResourceReleased && footprint.runner.Revision.Int64() == expectedRunner.Int64()+1 && !footprint.runner.Identity.Empty() &&
		footprint.providerProcess.State == ResourceReleased && footprint.providerProcess.Identity.Empty() && footprint.providerGroup.State == ResourceReleased && footprint.providerGroup.Identity.Empty() &&
		footprint.session.State == TerminalSessionClosed && footprint.session.ActivatedAt == nil
}

// RecordLiveRunnerExitAndRelease consumes the live owner's exact Wait result
// and releases the outer runner in the same transaction. Recovery uses the
// separate positive-absence edge above and cannot manufacture code or signal.
func (store *Store) RecordLiveRunnerExitAndRelease(ctx context.Context, runID RunID, runnerID ResourceID, expectedRun, expectedRunner Revision, identity ResourceIdentity, exit ProcessExit, at UnixMillis) (Run, Resource, error) {
	if runID.zero() || runnerID.zero() || !identity.validFor(ResourceRunnerProcess) || !exit.valid() || exit.RecoveredAbsence() || exit.Sequence() != 1 || at.Int64() < exit.At().Int64() {
		return Run{}, Resource{}, fmt.Errorf("%w: invalid live runner exit", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, Resource{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if footprint.runner.ID != runnerID || !resourceIdentityEqual(footprint.runner.Identity, identity) {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if liveRunnerExitPostcondition(run, footprint, exit, expectedRun, expectedRunner) {
		return run, footprint.runner, tx.Rollback(nil)
	}
	if run.Phase != RunFinalizing || run.RunnerExit != nil || run.Revision != expectedRun || footprint.runner.Revision != expectedRunner ||
		(footprint.runner.State != ResourceReleasing && footprint.runner.State != ResourceUnresolved) ||
		footprint.providerProcess.State != ResourceReleased || footprint.providerGroup.State != ResourceReleased ||
		(footprint.session.State != TerminalSessionReleasing && footprint.session.State != TerminalSessionUnresolved) ||
		footprint.runner.ActivatedAt == nil || exit.At().Int64() < footprint.runner.ActivatedAt.Int64() || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, Resource{}, tx.Rollback(ErrConflict)
	}
	exitKind, code, signal := exitSQL(exit)
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET runner_exit_kind = ?, runner_exit_sequence = 1, runner_exit_code = ?, runner_exit_signal = ?, runner_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND runner_exit_kind IS NULL AND revision = ?`, exitKind, code, signal, exit.At().Int64(), at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	pid, pgid, birth, _ := identity.Process()
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', unresolved_reason = NULL, released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runner_process' AND state = ? AND revision = ? AND pid = ? AND pgid = ? AND birth_digest = ?`, at.Int64(), at.Int64(), runnerID.Bytes(), runID.Bytes(), footprint.runner.State.String(), expectedRunner.Int64(), pid, pgid, birth.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, Resource{}, tx.Rollback(err)
	}
	run, found, err = runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	runner, found, err := resourceByID(ctx, tx.connection, runnerID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, Resource{}, err
	}
	return run, runner, nil
}

func liveRunnerExitPostcondition(run Run, footprint lifecycleFootprint, exit ProcessExit, expectedRun, expectedRunner Revision) bool {
	return run.Phase == RunFinalizing && run.Revision.Int64() == expectedRun.Int64()+1 && run.RunnerExit != nil && run.RunnerExit.equal(exit) &&
		footprint.runner.State == ResourceReleased && footprint.runner.Revision.Int64() == expectedRunner.Int64()+1 &&
		footprint.providerProcess.State == ResourceReleased && footprint.providerGroup.State == ResourceReleased &&
		(footprint.session.State == TerminalSessionReleasing || footprint.session.State == TerminalSessionUnresolved)
}

// CloseTerminalAfterRunner reauthenticates the exact result after both process
// owners are durably released. Missing or different result authority cannot
// close the session.
func (store *Store) CloseTerminalAfterRunner(ctx context.Context, result AttemptResult, expectedRun, expectedSession Revision, at UnixMillis) (Run, TerminalSession, error) {
	if !result.valid() {
		return Run{}, TerminalSession{}, fmt.Errorf("%w: malformed attempt result", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, TerminalSession{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, result.runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	if run.CredentialDigest != result.attemptDigest || run.resultProofDigest != result.resultProofDigest || run.Phase != RunFinalizing {
		return Run{}, TerminalSession{}, tx.Rollback(ErrConflict)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	if run.Revision.Int64() == expectedRun.Int64()+1 && footprint.session.Revision.Int64() == expectedSession.Int64()+1 && footprint.session.State == TerminalSessionClosed && terminalResultPostcondition(run, footprint, result) {
		return run, footprint.session, tx.Rollback(nil)
	}
	if run.Revision != expectedRun || footprint.session.Revision != expectedSession || (footprint.session.State != TerminalSessionReleasing && footprint.session.State != TerminalSessionUnresolved) || !lifecycleTimesValid(run, footprint, at) || !terminalResultPostcondition(run, footprint, result) {
		return Run{}, TerminalSession{}, tx.Rollback(ErrConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'closed', unresolved_reason = NULL, closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = ? AND revision = ?`, at.Int64(), at.Int64(), footprint.session.ID.Bytes(), run.ID.Bytes(), footprint.session.State.String(), expectedSession.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE runs SET revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND revision = ?`, at.Int64(), run.ID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: run.ID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	run, _, err = runByID(ctx, tx.connection, run.ID)
	if err != nil {
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	session, _, err := terminalSessionByID(ctx, tx.connection, footprint.session.ID)
	if err != nil {
		return Run{}, TerminalSession{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, TerminalSession{}, err
	}
	return run, session, nil
}

func terminalResultPostcondition(run Run, footprint lifecycleFootprint, result AttemptResult) bool {
	// Every trusted result was written by an activated runner, so the released
	// runner must retain its bound identity for either result kind.
	if !resourceIdentityEqual(footprint.runtime.Identity, result.runtimeIdentity) || footprint.providerProcess.State != ResourceReleased || footprint.providerGroup.State != ResourceReleased || footprint.runner.State != ResourceReleased || footprint.runner.Identity.Empty() {
		return false
	}
	if result.kind == AttemptInnerUnregisteredConverged {
		return run.ProviderExit == nil && footprint.providerProcess.Identity.Empty() && run.Proposal != nil && run.Proposal.code == FailureSpawn
	}
	if run.ProviderExit == nil || !resourceIdentityEqual(footprint.providerProcess.Identity, result.processIdentity) {
		return false
	}
	return attemptResultExitEqual(result.exit, run.ProviderExit)
}

// AuthorizeAttemptResultRemoval recognizes the exact natural postcondition
// that makes result-file removal (or completion of a previously unlinked
// result) safe. It is read-only and remains valid after later cleanup and
// terminalization; no receipt or result-history row is created.
func (store *Store) AuthorizeAttemptResultRemoval(ctx context.Context, result AttemptResult) (Run, error) {
	if !result.valid() {
		return Run{}, fmt.Errorf("%w: malformed attempt result", ErrInvalidValue)
	}
	tx, err := store.beginRead(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, result.runID)
	if err != nil {
		return Run{}, err
	}
	if !found {
		return Run{}, ErrNotFound
	}
	if run.CredentialDigest != result.attemptDigest || run.resultProofDigest != result.resultProofDigest || (run.Phase != RunFinalizing && run.Phase != RunTerminal) {
		return Run{}, ErrConflict
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, err
	}
	if footprint.session.State != TerminalSessionClosed || footprint.runner.State != ResourceReleased ||
		(footprint.runtime.State != ResourceReleasing && footprint.runtime.State != ResourceUnresolved && footprint.runtime.State != ResourceReleased) ||
		!terminalResultPostcondition(run, footprint, result) {
		return Run{}, ErrConflict
	}
	return run, nil
}
