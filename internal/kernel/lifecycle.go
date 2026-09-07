package kernel

import (
	"context"
	"database/sql"
	"fmt"
)

func (store *Store) ActivateResource(ctx context.Context, runID RunID, resourceID ResourceID, expected Revision, identity ResourceIdentity, at UnixMillis) (Resource, error) {
	return store.transitionResource(ctx, runID, resourceID, expected, identity, "", ResourceActive, at)
}

// ActivateProviderResources binds the one provider process/group authority in
// one transaction. Neither resource may acquire an identity independently.
func (store *Store) ActivateProviderResources(ctx context.Context, runID RunID, processID ResourceID, processExpected Revision, groupID ResourceID, groupExpected Revision, identity ResourceIdentity, at UnixMillis) (Resource, Resource, error) {
	if runID.zero() || processID.zero() || groupID.zero() || processID == groupID || !identity.validFor(ResourceProviderProcess) {
		return Resource{}, Resource{}, fmt.Errorf("%w: invalid provider resource activation", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Resource{}, Resource{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	process, processFound, err := resourceByID(ctx, tx.connection, processID)
	if err != nil {
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	group, groupFound, err := resourceByID(ctx, tx.connection, groupID)
	if err != nil {
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	if !processFound || !groupFound {
		return Resource{}, Resource{}, tx.Rollback(ErrNotFound)
	}
	if process.RunID != runID || process.Kind != ResourceProviderProcess || group.RunID != runID || group.Kind != ResourceProviderGroup {
		return Resource{}, Resource{}, tx.Rollback(ErrConflict)
	}
	if (process.ActivatedAt == nil) != (group.ActivatedAt == nil) || process.ActivatedAt != nil && *process.ActivatedAt != *group.ActivatedAt {
		return Resource{}, Resource{}, tx.Rollback(ErrCorruptState)
	}
	if process.State == ResourceActive && group.State == ResourceActive && resourceIdentityEqual(process.Identity, identity) && resourceIdentityEqual(group.Identity, identity) && process.Revision.Int64() == processExpected.Int64()+1 && group.Revision.Int64() == groupExpected.Int64()+1 {
		if err := tx.Rollback(nil); err != nil {
			return Resource{}, Resource{}, err
		}
		return process, group, nil
	}
	if run.Phase != RunAdmitted || process.State != ResourceDeclared || group.State != ResourceDeclared || process.Revision != processExpected || group.Revision != groupExpected || at.Int64() < process.UpdatedAt.Int64() || at.Int64() < group.UpdatedAt.Int64() {
		return Resource{}, Resource{}, tx.Rollback(ErrRevisionConflict)
	}
	if err := ensureResourceIdentityUnused(ctx, tx.connection, process, identity); err != nil {
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	pid, pgid, birth, _ := identity.Process()
	updated, err := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'active', pid = ?, pgid = ?, birth_digest = ?, activated_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'declared' AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND activated_at_ms IS NULL AND ((id = ? AND kind = 'provider_process' AND revision = ?) OR (id = ? AND kind = 'provider_group' AND revision = ?))`,
		pid, pgid, birth.Bytes(), at.Int64(), at.Int64(), runID.Bytes(), processID.Bytes(), processExpected.Int64(), groupID.Bytes(), groupExpected.Int64())
	if err := requireRows(updated, err, 2); err != nil {
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	process, processFound, err = resourceByID(ctx, tx.connection, processID)
	if err != nil || !processFound {
		if err == nil {
			err = ErrCorruptState
		}
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	group, groupFound, err = resourceByID(ctx, tx.connection, groupID)
	if err != nil || !groupFound {
		if err == nil {
			err = ErrCorruptState
		}
		return Resource{}, Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Resource{}, Resource{}, err
	}
	return process, group, nil
}

func (store *Store) BeginResourceRelease(ctx context.Context, runID RunID, resourceID ResourceID, expected Revision, identity ResourceIdentity, at UnixMillis) (Resource, error) {
	return store.transitionResource(ctx, runID, resourceID, expected, identity, "", ResourceReleasing, at)
}

func (store *Store) MarkResourceUnresolved(ctx context.Context, runID RunID, resourceID ResourceID, expected Revision, identity ResourceIdentity, reason string, at UnixMillis) (Resource, error) {
	if byteLen(reason) < 1 || byteLen(reason) > 4096 {
		return Resource{}, fmt.Errorf("%w: invalid unresolved reason", ErrInvalidValue)
	}
	return store.transitionResource(ctx, runID, resourceID, expected, identity, reason, ResourceUnresolved, at)
}

func (store *Store) ReleaseResource(ctx context.Context, runID RunID, resourceID ResourceID, expected Revision, identity ResourceIdentity, at UnixMillis) (Resource, error) {
	return store.transitionResource(ctx, runID, resourceID, expected, identity, "", ResourceReleased, at)
}

func (store *Store) transitionResource(ctx context.Context, runID RunID, resourceID ResourceID, expected Revision, identity ResourceIdentity, reason string, target ResourceState, at UnixMillis) (Resource, error) {
	if runID.zero() || resourceID.zero() || target.String() == "" {
		return Resource{}, fmt.Errorf("%w: invalid resource transition", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Resource{}, err
	}
	defer tx.Close()
	resource, found, err := resourceByID(ctx, tx.connection, resourceID)
	if err != nil {
		return Resource{}, tx.Rollback(err)
	}
	if !found {
		return Resource{}, tx.Rollback(ErrNotFound)
	}
	if resource.RunID != runID {
		return Resource{}, tx.Rollback(ErrConflict)
	}
	if resource.Kind == ResourceProviderProcess || resource.Kind == ResourceProviderGroup || resource.Kind == ResourceRunnerProcess && (target == ResourceActive || target == ResourceReleased) {
		return Resource{}, tx.Rollback(ErrConflict)
	}
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Resource{}, tx.Rollback(err)
	}
	if target == ResourceReleased && !resource.Identity.Empty() {
		switch resource.Kind {
		case ResourceProviderProcess, ResourceProviderGroup:
			if run.ProviderExit == nil {
				return Resource{}, tx.Rollback(ErrConflict)
			}
		case ResourceRunnerProcess:
			if run.RunnerExit == nil {
				return Resource{}, tx.Rollback(ErrConflict)
			}
		}
	}
	if resource.State == target && resourceIdentityEqual(resource.Identity, identity) && resource.UnresolvedReason == reason && resource.Revision.Int64() == expected.Int64()+1 {
		if err := tx.Rollback(nil); err != nil {
			return Resource{}, err
		}
		return resource, nil
	}
	if target == ResourceReleased && !resource.Identity.Empty() {
		switch resource.Kind {
		case ResourceProviderProcess, ResourceProviderGroup:
			if at.Int64() < run.ProviderExit.At().Int64() {
				return Resource{}, tx.Rollback(ErrRevisionConflict)
			}
		case ResourceRunnerProcess:
			if at.Int64() < run.RunnerExit.At().Int64() {
				return Resource{}, tx.Rollback(ErrRevisionConflict)
			}
		}
	}
	if resource.Revision != expected || at.Int64() < resource.UpdatedAt.Int64() {
		return Resource{}, tx.Rollback(ErrRevisionConflict)
	}
	if target == ResourceActive && run.Phase != RunAdmitted || target != ResourceActive && run.Phase != RunFinalizing {
		return Resource{}, tx.Rollback(ErrConflict)
	}
	if err := validateResourceEdge(resource, target, identity, reason); err != nil {
		return Resource{}, tx.Rollback(err)
	}
	if target == ResourceActive {
		if err := ensureResourceIdentityUnused(ctx, tx.connection, resource, identity); err != nil {
			return Resource{}, tx.Rollback(err)
		}
	}
	var result sql.Result
	switch target {
	case ResourceActive:
		if resource.Kind == ResourceRuntimeRoot {
			pathIdentity, _ := identity.Path()
			result, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'active', path_dev = ?, path_inode = ?, activated_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND revision = ? AND path_dev IS NULL AND path_inode IS NULL AND activated_at_ms IS NULL`, pathIdentity.device, pathIdentity.inode, at.Int64(), at.Int64(), resourceID.Bytes(), runID.Bytes(), expected.Int64())
		} else {
			pid, pgid, birth, _ := identity.Process()
			result, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'active', pid = ?, pgid = ?, birth_digest = ?, activated_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND revision = ? AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND activated_at_ms IS NULL`, pid, pgid, birth.Bytes(), at.Int64(), at.Int64(), resourceID.Bytes(), runID.Bytes(), expected.Int64())
		}
	case ResourceReleasing:
		result, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state IN ('declared', 'active') AND revision = ?`, at.Int64(), resourceID.Bytes(), runID.Bytes(), expected.Int64())
	case ResourceUnresolved:
		result, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'unresolved', unresolved_reason = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state IN ('declared', 'active', 'releasing') AND revision = ?`, reason, at.Int64(), resourceID.Bytes(), runID.Bytes(), expected.Int64())
	case ResourceReleased:
		result, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', unresolved_reason = NULL, released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state IN ('declared', 'active', 'releasing', 'unresolved') AND revision = ?`, at.Int64(), at.Int64(), resourceID.Bytes(), runID.Bytes(), expected.Int64())
	default:
		return Resource{}, tx.Rollback(ErrInvalidValue)
	}
	if err := requireOneRow(result, err); err != nil {
		return Resource{}, tx.Rollback(err)
	}
	resource, found, err = resourceByID(ctx, tx.connection, resourceID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Resource{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Resource{}, err
	}
	return resource, nil
}

func validateResourceEdge(resource Resource, target ResourceState, identity ResourceIdentity, reason string) error {
	exactIdentity := resourceIdentityEqual(resource.Identity, identity)
	switch target {
	case ResourceActive:
		if resource.State != ResourceDeclared || !resource.Identity.Empty() || !identity.validFor(resource.Kind) {
			return ErrConflict
		}
	case ResourceReleasing:
		if resource.State != ResourceDeclared && resource.State != ResourceActive || !exactIdentity || reason != "" {
			return ErrConflict
		}
	case ResourceUnresolved:
		if resource.State != ResourceDeclared && resource.State != ResourceActive && resource.State != ResourceReleasing || !exactIdentity || byteLen(reason) < 1 || byteLen(reason) > 4096 {
			return ErrConflict
		}
	case ResourceReleased:
		if resource.State == ResourceReleased || resource.State.String() == "" || !exactIdentity || reason != "" {
			return ErrConflict
		}
	default:
		return ErrInvalidValue
	}
	return nil
}

func resourceIdentityEqual(left, right ResourceIdentity) bool { return left == right }

func ensureResourceIdentityUnused(ctx context.Context, connection *sql.Conn, resource Resource, identity ResourceIdentity) error {
	if resource.Kind == ResourceRuntimeRoot {
		var collisions int
		pathIdentity, _ := identity.Path()
		if err := connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM resources WHERE id <> ? AND path_dev = ? AND path_inode = ?`, resource.ID.Bytes(), pathIdentity.device, pathIdentity.inode).Scan(&collisions); err != nil {
			return err
		}
		if collisions != 0 {
			return ErrConflict
		}
		return nil
	}
	pid, pgid, birth, _ := identity.Process()
	rows, err := connection.QueryContext(ctx, `SELECT run_id, kind FROM resources WHERE id <> ? AND pid = ? AND pgid = ? AND birth_digest = ?`, resource.ID.Bytes(), pid, pgid, birth.Bytes())
	if err != nil {
		return err
	}
	defer rows.Close()
	for rows.Next() {
		var rawRunID []byte
		var rawKind string
		if err := rows.Scan(&rawRunID, &rawKind); err != nil {
			return err
		}
		runID, runErr := RunIDFromBytes(rawRunID)
		kind, kindErr := parseResourceKind(rawKind)
		providerPair := resource.RunID == runID && (resource.Kind == ResourceProviderProcess && kind == ResourceProviderGroup || resource.Kind == ResourceProviderGroup && kind == ResourceProviderProcess)
		if runErr != nil || kindErr != nil || !providerPair {
			return ErrConflict
		}
	}
	return rows.Err()
}

// ActivateRun binds the exact admitted terminal session to the run. The run
// and session revisions are guarded and advanced together.
func (store *Store) ActivateRun(ctx context.Context, runID RunID, sessionID TerminalSessionID, expectedRun Revision, expectedSession Revision, at UnixMillis) (Run, error) {
	if runID.zero() || sessionID.zero() {
		return Run{}, fmt.Errorf("%w: invalid terminal activation identifiers", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, tx.Rollback(ErrNotFound)
	}
	session, sessionFound, err := terminalSessionByID(ctx, tx.connection, sessionID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !sessionFound {
		return Run{}, tx.Rollback(ErrNotFound)
	}
	if session.RunID != run.ID {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if run.Phase == RunRunning && run.Revision.Int64() == expectedRun.Int64()+1 && session.State == TerminalSessionActive && session.Revision.Int64() == expectedSession.Int64()+1 {
		if err := tx.Rollback(nil); err != nil {
			return Run{}, err
		}
		return run, nil
	}
	if run.Phase != RunAdmitted || run.Revision != expectedRun || session.Revision != expectedSession || session.State != TerminalSessionDeclared || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	relationships, err := loadRunRelationships(ctx, tx.connection, run)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if run.Role == RoleWorker && !relationships.changeOwnership.available() {
		return Run{}, tx.Rollback(ErrConflict)
	}
	resources := relationships.resources
	if !exactResourceSet(resources, true) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if err := requireResourceTime(resources, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	if run.Role == RoleWorker && (relationships.change.AvailableAt == nil || relationships.change.AvailableAt.Int64() > at.Int64() || relationships.change.UpdatedAt.Int64() > at.Int64()) {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	result, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'active', activated_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND revision = ?`, at.Int64(), at.Int64(), session.ID.Bytes(), run.ID.Bytes(), expectedSession.Int64())
	if err := requireOneRow(result, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	result, err = tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'running', running_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'admitted' AND revision = ?`, at.Int64(), at.Int64(), run.ID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(result, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: run.ID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, found, err = runByID(ctx, tx.connection, run.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

// MarkTerminalSessionUnresolved records uncertainty without making it
// terminal. The reason is bounded by the durable schema and scanner.
func (store *Store) MarkTerminalSessionUnresolved(ctx context.Context, runID RunID, sessionID TerminalSessionID, expectedRun Revision, expectedSession Revision, reason string, at UnixMillis) (TerminalSession, error) {
	if byteLen(reason) < 1 || byteLen(reason) > 4096 {
		return TerminalSession{}, fmt.Errorf("%w: invalid terminal session unresolved reason", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return TerminalSession{}, err
	}
	defer tx.Close()
	run, session, _, err := loadTerminalSessionMutation(ctx, tx, runID, sessionID)
	if err != nil {
		return TerminalSession{}, tx.Rollback(err)
	}
	if run.Phase == RunFinalizing && run.Revision.Int64() == expectedRun.Int64()+1 && session.Revision.Int64() == expectedSession.Int64()+1 && session.State == TerminalSessionUnresolved && session.UnresolvedReason == reason {
		return session, tx.Rollback(nil)
	}
	if run.Revision != expectedRun || session.Revision != expectedSession || at.Int64() < run.UpdatedAt.Int64() || at.Int64() < session.UpdatedAt.Int64() {
		return TerminalSession{}, tx.Rollback(ErrRevisionConflict)
	}
	if session.State != TerminalSessionDeclared && session.State != TerminalSessionActive && session.State != TerminalSessionReleasing {
		return TerminalSession{}, tx.Rollback(ErrConflict)
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'unresolved', unresolved_reason = ?, lease_client_id = NULL, lease_expires_at_ms = NULL, last_input_sequence = 0, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state IN ('declared', 'active', 'releasing') AND revision = ?`, reason, at.Int64(), session.ID.Bytes(), run.ID.Bytes(), expectedSession.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return TerminalSession{}, tx.Rollback(err)
	}
	return commitTerminalSessionMutation(ctx, tx, run, session, expectedRun, at)
}

func loadTerminalSessionMutation(ctx context.Context, tx *writeTx, runID RunID, sessionID TerminalSessionID) (Run, TerminalSession, runRelationships, error) {
	if runID.zero() || sessionID.zero() {
		return Run{}, TerminalSession{}, runRelationships{}, fmt.Errorf("%w: invalid terminal session transition", ErrInvalidValue)
	}
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, TerminalSession{}, runRelationships{}, err
	}
	if !found {
		return Run{}, TerminalSession{}, runRelationships{}, ErrNotFound
	}
	session, found, err := terminalSessionByID(ctx, tx.connection, sessionID)
	if err != nil {
		return Run{}, TerminalSession{}, runRelationships{}, err
	}
	if !found {
		return Run{}, TerminalSession{}, runRelationships{}, ErrNotFound
	}
	if session.RunID != run.ID {
		return Run{}, TerminalSession{}, runRelationships{}, ErrConflict
	}
	if run.Phase != RunFinalizing {
		return Run{}, TerminalSession{}, runRelationships{}, ErrRevisionConflict
	}
	relationships, err := loadRunRelationships(ctx, tx.connection, run)
	if err != nil {
		return Run{}, TerminalSession{}, runRelationships{}, err
	}
	return run, session, relationships, nil
}

func commitTerminalSessionMutation(ctx context.Context, tx *writeTx, run Run, txSession TerminalSession, expectedRun Revision, at UnixMillis) (TerminalSession, error) {
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND revision = ?`, at.Int64(), run.ID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return TerminalSession{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityRun, id: run.ID.Bytes(), revision: expectedRun.Int64() + 1}}); err != nil {
		return TerminalSession{}, tx.Rollback(err)
	}
	result, found, err := terminalSessionByID(ctx, tx.connection, txSession.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return TerminalSession{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return TerminalSession{}, err
	}
	return result, nil
}

func (store *Store) ProposeAttemptOutcome(ctx context.Context, digest AttemptDigest, proposal Proposal, at UnixMillis) (Run, error) {
	if !proposal.valid() {
		return Run{}, fmt.Errorf("%w: invalid attempt proposal", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByDigest(ctx, tx.connection, digest)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !found || run.Phase != RunRunning || run.CredentialRevokedAt != nil {
		return Run{}, tx.Rollback(ErrUnauthorized)
	}
	return store.enterFinalizing(ctx, tx, run, run.Revision, proposal, at, nil)
}

// FailRun records a daemon-owned infrastructure failure before or during a
// running attempt. It never acts as attempt authority and never overwrites an
// outcome that reached finalizing first.
func (store *Store) FailRun(ctx context.Context, runID RunID, expected Revision, failure Proposal, at UnixMillis) (Run, error) {
	if runID.zero() || !failure.valid() || failure.kind != OutcomeFailed {
		return Run{}, fmt.Errorf("%w: invalid run failure", ErrInvalidValue)
	}
	switch failure.code {
	case FailureSpawn, FailureActivation, FailureSource, FailureProtocol, FailureInternal:
	default:
		return Run{}, fmt.Errorf("%w: failure code is not daemon infrastructure authority", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, tx.Rollback(ErrNotFound)
	}
	if run.Phase == RunFinalizing && run.Proposal != nil && run.Proposal.equal(failure) && run.Revision.Int64() == expected.Int64()+1 {
		if err := tx.Rollback(nil); err != nil {
			return Run{}, err
		}
		return run, nil
	}
	if run.Phase != RunAdmitted && run.Phase != RunRunning {
		return Run{}, tx.Rollback(ErrConflict)
	}
	return store.enterFinalizing(ctx, tx, run, expected, failure, at, nil)
}

// FailRunWithRuntimeAbsent is the sole no-runtime-effect failure edge. Its
// caller must have trusted evidence that the exact run root is absent, whether
// before the sole CreateRuntime call or after positively verified cleanup. A
// declared runtime after an uncertain CreateRuntime is not absence evidence.
func (store *Store) FailRunWithRuntimeAbsent(ctx context.Context, runID RunID, runtimeID ResourceID, expectedRun, expectedRuntime Revision, failure Proposal, at UnixMillis) (Run, error) {
	if runID.zero() || runtimeID.zero() || !failure.valid() || failure.kind != OutcomeFailed {
		return Run{}, fmt.Errorf("%w: invalid pre-runtime failure", ErrInvalidValue)
	}
	switch failure.code {
	case FailureSpawn, FailureActivation, FailureSource, FailureProtocol, FailureInternal:
	default:
		return Run{}, fmt.Errorf("%w: failure code is not daemon infrastructure authority", ErrInvalidValue)
	}
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
	if footprint.runtime.ID != runtimeID {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if preRuntimeFailurePostcondition(run, footprint, expectedRun, expectedRuntime, failure) {
		return run, tx.Rollback(nil)
	}
	if run.Phase != RunAdmitted || run.Revision != expectedRun || footprint.runtime.Revision != expectedRuntime ||
		footprint.runtime.State != ResourceDeclared || !footprint.runtime.Identity.Empty() ||
		footprint.runner.State != ResourceDeclared || !footprint.runner.Identity.Empty() || footprint.runner.Revision != expectedRuntime ||
		footprint.providerProcess.State != ResourceDeclared || !footprint.providerProcess.Identity.Empty() || footprint.providerProcess.Revision != expectedRuntime ||
		footprint.providerGroup.State != ResourceDeclared || !footprint.providerGroup.Identity.Empty() || footprint.providerGroup.Revision != expectedRuntime ||
		footprint.session.State != TerminalSessionDeclared || footprint.session.ActivatedAt != nil ||
		at.Int64() < run.UpdatedAt.Int64() || !lifecycleTimesValid(run, footprint, at) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if err := requireFinalizingTime(ctx, tx.connection, run, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	kind, code, detail, result := proposalSQL(failure)
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'finalizing', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, credential_revoked_at_ms = ?, finalizing_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'admitted' AND proposal_kind IS NULL AND credential_revoked_at_ms IS NULL AND revision = ?`, kind, code, detail, result, at.Int64(), at.Int64(), at.Int64(), runID.Bytes(), expectedRun.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'declared' AND path_dev IS NULL AND path_inode IS NULL AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND revision = ?`, at.Int64(), at.Int64(), runID.Bytes(), expectedRuntime.Int64())
	if err := requireRows(updated, err, 4); err != nil {
		return Run{}, tx.Rollback(err)
	}
	updated, err = tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'closed', closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND activated_at_ms IS NULL AND closed_at_ms IS NULL`, at.Int64(), at.Int64(), footprint.session.ID.Bytes(), runID.Bytes())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	requestInvalidations, err := transitionHumanRequestsForRun(ctx, tx.connection, runID, at, false, nil)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	pending := append([]pendingInvalidation{{kind: EntityRun, id: runID.Bytes(), revision: expectedRun.Int64() + 1}}, requestInvalidations...)
	if err := appendInvalidations(ctx, tx.connection, at, pending); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, found, err = runByID(ctx, tx.connection, runID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

func preRuntimeFailurePostcondition(run Run, footprint lifecycleFootprint, expectedRun, expectedRuntime Revision, failure Proposal) bool {
	if run.Phase != RunFinalizing || run.Revision.Int64() != expectedRun.Int64()+1 || run.Proposal == nil || !run.Proposal.equal(failure) || footprint.runtime.ID.zero() {
		return false
	}
	for _, resource := range []Resource{footprint.runtime, footprint.runner, footprint.providerProcess, footprint.providerGroup} {
		if resource.State != ResourceReleased || !resource.Identity.Empty() || resource.Revision.Int64() != expectedRuntime.Int64()+1 {
			return false
		}
	}
	return footprint.session.State == TerminalSessionClosed && footprint.session.ActivatedAt == nil
}

func (store *Store) CancelRun(ctx context.Context, runID RunID, expected Revision, detail string, at UnixMillis) (Run, error) {
	proposal, err := NewCancelledProposal(detail)
	if err != nil {
		return Run{}, err
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, tx.Rollback(ErrNotFound)
	}
	if (run.Phase == RunFinalizing || run.Phase == RunTerminal) && run.Proposal != nil && run.Proposal.equal(proposal) && run.Revision.Int64() > expected.Int64() {
		if err := tx.Rollback(nil); err != nil {
			return Run{}, err
		}
		return run, nil
	}
	if run.Phase != RunAdmitted && run.Phase != RunRunning {
		return Run{}, tx.Rollback(ErrConflict)
	}
	return store.enterFinalizing(ctx, tx, run, expected, proposal, at, nil)
}

func (store *Store) enterFinalizing(ctx context.Context, tx *writeTx, run Run, expected Revision, proposal Proposal, at UnixMillis, cancelRequest *HumanRequestID) (Run, error) {
	if run.Revision != expected || at.Int64() < run.UpdatedAt.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	if err := requireFinalizingTime(ctx, tx.connection, run, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	footprint, err := loadLifecycleFootprint(ctx, tx.connection, run)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	preRunnerStart := run.Phase == RunAdmitted && footprint.runtime.State == ResourceActive && !footprint.runtime.Identity.Empty() &&
		footprint.runner.State == ResourceDeclared && footprint.runner.Identity.Empty() &&
		footprint.providerProcess.State == ResourceDeclared && footprint.providerProcess.Identity.Empty() &&
		footprint.session.State == TerminalSessionDeclared && footprint.session.ActivatedAt == nil
	wantSession := TerminalSessionDeclared
	if run.Phase == RunRunning {
		wantSession = TerminalSessionActive
	}
	activeAttempt := footprint.runtime.State == ResourceActive && footprint.runner.State == ResourceActive && !footprint.runner.Identity.Empty() &&
		footprint.providerProcess.State == ResourceActive && !footprint.providerProcess.Identity.Empty() && footprint.session.State == wantSession
	if !preRunnerStart && !activeAttempt {
		return Run{}, tx.Rollback(ErrConflict)
	}
	kind, code, detail, result := proposalSQL(proposal)
	updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'finalizing', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, credential_revoked_at_ms = ?, finalizing_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase IN ('admitted', 'running') AND proposal_kind IS NULL AND credential_revoked_at_ms IS NULL AND revision = ?`,
		kind, code, detail, result, at.Int64(), at.Int64(), at.Int64(), run.ID.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	if preRunnerStart {
		releasing, releaseErr := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND kind = 'runtime_root' AND state = 'active'`, at.Int64(), footprint.runtime.ID.Bytes(), run.ID.Bytes())
		if err := requireOneRow(releasing, releaseErr); err != nil {
			return Run{}, tx.Rollback(err)
		}
		released, releaseErr := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'released', released_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'declared' AND pid IS NULL AND pgid IS NULL AND birth_digest IS NULL AND kind IN ('runner_process', 'provider_process', 'provider_group')`, at.Int64(), at.Int64(), run.ID.Bytes())
		if err := requireRows(released, releaseErr, 3); err != nil {
			return Run{}, tx.Rollback(err)
		}
		closed, closeErr := tx.connection.ExecContext(ctx, `UPDATE terminal_sessions SET state = 'closed', closed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND run_id = ? AND state = 'declared' AND activated_at_ms IS NULL AND closed_at_ms IS NULL`, at.Int64(), at.Int64(), footprint.session.ID.Bytes(), run.ID.Bytes())
		if err := requireOneRow(closed, closeErr); err != nil {
			return Run{}, tx.Rollback(err)
		}
	} else {
		releasing, releaseErr := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state = 'active'`, at.Int64(), run.ID.Bytes())
		if err := requireRows(releasing, releaseErr, 4); err != nil {
			return Run{}, tx.Rollback(err)
		}
		if err := moveTerminalToReleasing(ctx, tx.connection, footprint.session, at); err != nil {
			return Run{}, tx.Rollback(err)
		}
	}
	pending := []pendingInvalidation{{kind: EntityRun, id: run.ID.Bytes(), revision: expected.Int64() + 1}}
	requestInvalidations, err := transitionHumanRequestsForRun(ctx, tx.connection, run.ID, at, false, cancelRequest)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	pending = append(pending, requestInvalidations...)
	if err := appendInvalidations(ctx, tx.connection, at, pending); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, found, err := runByID(ctx, tx.connection, run.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

// ObserveProviderExit consumes the live owner's exact provider Wait result.
// Runner exits flow only through the atomic result/absence release edges.
func (store *Store) ObserveProviderExit(ctx context.Context, runID RunID, expected Revision, identity ResourceIdentity, exit ProcessExit, at UnixMillis) (Run, error) {
	if runID.zero() || !identity.validFor(ResourceProviderProcess) || !exit.valid() || at.Int64() < exit.at.Int64() {
		return Run{}, fmt.Errorf("%w: invalid process exit observation", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, tx.Rollback(ErrNotFound)
	}
	if exit.at.Int64() < run.AdmittedAt.Int64() {
		return Run{}, tx.Rollback(ErrInvalidValue)
	}
	resources, err := resourcesForRun(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	activatedAt, matched := exitIdentityMatches(resources, identity)
	if !matched {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if exit.at.Int64() < activatedAt.Int64() {
		return Run{}, tx.Rollback(ErrConflict)
	}
	existing := run.ProviderExit
	if existing != nil {
		if !existing.equal(exit) {
			return Run{}, tx.Rollback(ErrConflict)
		}
		if err := tx.Rollback(nil); err != nil {
			return Run{}, err
		}
		return run, nil
	}
	if run.Phase == RunTerminal || run.Revision != expected || at.Int64() < run.UpdatedAt.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	exitKind, code, signal := exitSQL(exit)
	newRevision := expected.Int64() + 1
	pending := []pendingInvalidation{{kind: EntityRun, id: run.ID.Bytes(), revision: newRevision}}
	if run.Phase == RunAdmitted || run.Phase == RunRunning {
		if err := requireFinalizingTime(ctx, tx.connection, run, at); err != nil {
			return Run{}, tx.Rollback(err)
		}
		proposal, _ := NewFailureProposal(FailureProviderExit, "provider exited before an attempt outcome")
		kind, proposalCode, proposalDetail, result := proposalSQL(proposal)
		updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'finalizing', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, credential_revoked_at_ms = ?, finalizing_at_ms = ?, provider_exit_kind = ?, provider_exit_sequence = ?, provider_exit_code = ?, provider_exit_signal = ?, provider_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase IN ('admitted', 'running') AND proposal_kind IS NULL AND provider_exit_kind IS NULL AND revision = ?`,
			kind, proposalCode, proposalDetail, result, at.Int64(), at.Int64(), exitKind, exit.sequence, code, signal, exit.at.Int64(), at.Int64(), run.ID.Bytes(), expected.Int64())
		if err := requireOneRow(updated, err); err != nil {
			return Run{}, tx.Rollback(err)
		}
		releasing, err := tx.connection.ExecContext(ctx, `UPDATE resources SET state = 'releasing', revision = revision + 1, updated_at_ms = ? WHERE run_id = ? AND state IN ('declared', 'active')`, at.Int64(), run.ID.Bytes())
		if err := requireRows(releasing, err, 4); err != nil {
			return Run{}, tx.Rollback(err)
		}
		session, found, sessionErr := terminalSessionByRunID(ctx, tx.connection, run.ID)
		if sessionErr != nil || !found {
			if sessionErr == nil {
				sessionErr = ErrCorruptState
			}
			return Run{}, tx.Rollback(sessionErr)
		}
		if err := moveTerminalToReleasing(ctx, tx.connection, session, at); err != nil {
			return Run{}, tx.Rollback(err)
		}
		requestInvalidations, transitionErr := transitionHumanRequestsForRun(ctx, tx.connection, run.ID, at, false, nil)
		if transitionErr != nil {
			return Run{}, tx.Rollback(transitionErr)
		}
		pending = append(pending, requestInvalidations...)
	} else if run.Phase == RunFinalizing {
		updated, err := tx.connection.ExecContext(ctx, `UPDATE runs SET provider_exit_kind = ?, provider_exit_sequence = ?, provider_exit_code = ?, provider_exit_signal = ?, provider_exit_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND provider_exit_kind IS NULL AND revision = ?`, exitKind, exit.sequence, code, signal, exit.at.Int64(), at.Int64(), run.ID.Bytes(), expected.Int64())
		if err := requireOneRow(updated, err); err != nil {
			return Run{}, tx.Rollback(err)
		}
	} else {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if err := appendInvalidations(ctx, tx.connection, at, pending); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, found, err = runByID(ctx, tx.connection, run.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

func exitIdentityMatches(resources []Resource, identity ResourceIdentity) (UnixMillis, bool) {
	providerProcessMatched := false
	providerGroupMatched := false
	var activatedAt UnixMillis
	var activationFound bool
	for _, resource := range resources {
		if resource.State == ResourceDeclared || !resourceIdentityEqual(resource.Identity, identity) {
			continue
		}
		switch resource.Kind {
		case ResourceProviderProcess:
			providerProcessMatched = true
		case ResourceProviderGroup:
			providerGroupMatched = true
		default:
			continue
		}
		if resource.ActivatedAt == nil || activationFound && *resource.ActivatedAt != activatedAt {
			return UnixMillis{}, false
		}
		activatedAt = *resource.ActivatedAt
		activationFound = true
	}
	return activatedAt, providerProcessMatched && providerGroupMatched && activationFound
}

func (store *Store) FinalizeRun(ctx context.Context, runID RunID, expected Revision, at UnixMillis) (Run, error) {
	return store.finalizeRun(ctx, runID, expected, nil, at)
}

func (store *Store) FinalizeWorkerRun(ctx context.Context, runID RunID, expected Revision, settlement ChangeSettlement, at UnixMillis) (Run, error) {
	if !settlement.valid() {
		return Run{}, fmt.Errorf("%w: invalid worker Change settlement", ErrInvalidValue)
	}
	return store.finalizeRun(ctx, runID, expected, &settlement, at)
}

func (store *Store) finalizeRun(ctx context.Context, runID RunID, expected Revision, settlement *ChangeSettlement, at UnixMillis) (Run, error) {
	if runID.zero() {
		return Run{}, fmt.Errorf("%w: zero run identifier", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Run{}, err
	}
	defer tx.Close()
	run, found, err := runByID(ctx, tx.connection, runID)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if !found {
		return Run{}, tx.Rollback(ErrNotFound)
	}
	if run.Phase == RunTerminal && run.Revision.Int64() > expected.Int64() {
		if run.Proposal == nil || run.Terminal == nil || !run.Terminal.equal(*run.Proposal) || settlement != nil && settlement.refusal != nil && !run.Terminal.equal(*settlement.refusal) {
			return Run{}, tx.Rollback(ErrConflict)
		}
		if err := tx.Rollback(nil); err != nil {
			return Run{}, err
		}
		return run, nil
	}
	if run.Role == RoleWorker && settlement == nil || run.Role == RoleOrchestrator && settlement != nil {
		return Run{}, tx.Rollback(ErrConflict)
	}
	relationships, err := loadRunRelationships(ctx, tx.connection, run)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if run.Phase != RunFinalizing || run.Proposal == nil || run.CredentialRevokedAt == nil || run.Revision != expected || at.Int64() < run.UpdatedAt.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	if err := requireResourceTime(relationships.resources, at); err != nil {
		return Run{}, tx.Rollback(err)
	}
	if relationships.task.UpdatedAt.Int64() > at.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	if relationships.change != nil && relationships.change.UpdatedAt.Int64() > at.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	factory, err := factoryState(ctx, tx.connection)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	if at.Int64() < factory.updatedAt.Int64() {
		return Run{}, tx.Rollback(ErrRevisionConflict)
	}
	if run.Role == RoleWorker && run.VerificationPolicy != VerificationNone && run.Proposal.kind == OutcomeSucceeded {
		return Run{}, tx.Rollback(ErrConflict)
	}
	if relationships.session.State != TerminalSessionClosed {
		return Run{}, tx.Rollback(ErrConflict)
	}
	terminal := *run.Proposal
	if settlement != nil && settlement.refusal != nil {
		// The daemon refused the published tree: its refusal is the outcome,
		// recorded as the proposal too so the record stays one story.
		terminal = *settlement.refusal
	}
	resources := relationships.resources
	if !exactResourceSet(resources, false) {
		return Run{}, tx.Rollback(ErrConflict)
	}
	requiresProviderExit := false
	requiresRunnerExit := false
	for _, resource := range resources {
		if !resource.Identity.Empty() {
			switch resource.Kind {
			case ResourceProviderProcess, ResourceProviderGroup:
				requiresProviderExit = true
			case ResourceRunnerProcess:
				requiresRunnerExit = true
			}
		}
		if resource.State != ResourceReleased {
			return Run{}, tx.Rollback(ErrConflict)
		}
	}
	if requiresProviderExit && run.ProviderExit == nil || requiresRunnerExit && run.RunnerExit == nil {
		return Run{}, tx.Rollback(ErrConflict)
	}
	task := relationships.task
	taskRevision := task.Revision.Int64() + 1
	var taskStatus string
	var blocked, result, completed any
	switch terminal.kind {
	case OutcomeSucceeded:
		taskStatus, result, completed = TaskSucceeded.String(), terminal.result, at.Int64()
	case OutcomeBlocked:
		taskStatus, blocked = TaskBlocked.String(), terminal.detail
	case OutcomeFailed:
		taskStatus, completed = TaskFailed.String(), at.Int64()
	case OutcomeCancelled:
		taskStatus, completed = TaskCancelled.String(), at.Int64()
	default:
		return Run{}, tx.Rollback(ErrCorruptState)
	}
	var settlingChange *Change
	changeRevision := int64(0)
	if settlement != nil {
		settlingChange = relationships.change
		if settlingChange == nil || run.ChangeID == nil || settlingChange.Revision != settlement.expected || at.Int64() < settlingChange.UpdatedAt.Int64() || !relationships.changeOwnership.canSettleAs(settlement.phase, settlement.refusal != nil) {
			return Run{}, tx.Rollback(ErrConflict)
		}
		if settlement.phase == ChangeRetained && (settlement.availability == nil || settlingChange.TreeIdentity == nil || *settlingChange.TreeIdentity != settlement.availability.tree || run.RunningAt == nil && !changeAvailabilityMatches(*settlingChange, *settlement.availability)) {
			return Run{}, tx.Rollback(ErrConflict)
		}
		changeRevision = settlingChange.Revision.Int64() + 1
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE tasks SET status = ?, blocked_reason = ?, result = ?, completed_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND project_id = ? AND incarnation_id = ? AND work_revision = ? AND status = 'running' AND revision = ?`,
		taskStatus, blocked, result, completed, at.Int64(), task.ID.Bytes(), run.ProjectID.Bytes(), run.TaskIncarnationID.Bytes(), run.AdmittedTaskWorkRevision.Int64(), task.Revision.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	terminalKind, terminalCode, terminalDetail, terminalResult := proposalSQL(terminal)
	updated, err = tx.connection.ExecContext(ctx, `UPDATE runs SET phase = 'terminal', proposal_kind = ?, proposal_code = ?, proposal_detail = ?, proposal_result = ?, terminal_kind = ?, terminal_code = ?, terminal_detail = ?, terminal_result = ?, terminal_at_ms = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'finalizing' AND proposal_kind IS NOT NULL AND credential_revoked_at_ms IS NOT NULL AND revision = ?`, terminalKind, terminalCode, terminalDetail, terminalResult, terminalKind, terminalCode, terminalDetail, terminalResult, at.Int64(), at.Int64(), run.ID.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	if settlement != nil {
		change := settlingChange
		switch settlement.phase {
		case ChangeRetained:
			updated, err = tx.connection.ExecContext(ctx, `UPDATE changes SET phase = 'retained', tree_digest = ?, entry_count = ?, total_bytes = ?, settled_run_id = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase = 'available' AND revision = ? AND settled_run_id IS NULL AND tree_dev = ? AND tree_inode = ?`, settlement.availability.commitment.Bytes(), int64(settlement.availability.entries), int64(settlement.availability.bytes), run.ID.Bytes(), at.Int64(), change.ID.Bytes(), settlement.expected.Int64(), settlement.availability.tree.device, settlement.availability.tree.inode)
		case ChangeAbandoned:
			updated, err = tx.connection.ExecContext(ctx, `UPDATE changes SET phase = 'abandoned', object_format = NULL, base_commit = NULL, repository_dev = NULL, repository_inode = NULL, tree_digest = NULL, entry_count = NULL, total_bytes = NULL, tree_dev = NULL, tree_inode = NULL, prepared_at_ms = NULL, available_at_ms = NULL, settled_run_id = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND phase IN ('reserved', 'prepared', 'available') AND revision = ? AND settled_run_id IS NULL`, run.ID.Bytes(), at.Int64(), change.ID.Bytes(), settlement.expected.Int64())
		default:
			return Run{}, tx.Rollback(ErrCorruptState)
		}
		if err := requireOneRow(updated, err); err != nil {
			return Run{}, tx.Rollback(err)
		}
	}
	factoryRevision := factory.Revision.Int64() + 1
	updated, err = tx.connection.ExecContext(ctx, `UPDATE factory SET revision = revision + 1, updated_at_ms = ? WHERE singleton = 1 AND revision = ?`, at.Int64(), factory.Revision.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Run{}, tx.Rollback(err)
	}
	requestInvalidations, err := transitionHumanRequestsForRun(ctx, tx.connection, run.ID, at, true, nil)
	if err != nil {
		return Run{}, tx.Rollback(err)
	}
	pending := []pendingInvalidation{
		{kind: EntityFactory, id: factoryEntityID[:], revision: factoryRevision},
		{kind: EntityTask, id: task.ID.Bytes(), revision: taskRevision},
		{kind: EntityRun, id: run.ID.Bytes(), revision: expected.Int64() + 1},
	}
	if settlement != nil {
		pending = append(pending, pendingInvalidation{kind: EntityChange, id: (*run.ChangeID).Bytes(), revision: changeRevision})
	}
	pending = append(pending, requestInvalidations...)
	if err := appendInvalidations(ctx, tx.connection, at, pending); err != nil {
		return Run{}, tx.Rollback(err)
	}
	run, found, err = runByID(ctx, tx.connection, run.ID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Run{}, err
	}
	return run, nil
}

func exactResourceSet(resources []Resource, requireActive bool) bool {
	if len(resources) != 4 {
		return false
	}
	seen := map[ResourceKind]bool{}
	for _, resource := range resources {
		if resource.Kind.String() == "" || seen[resource.Kind] || requireActive && resource.State != ResourceActive {
			return false
		}
		seen[resource.Kind] = true
	}
	return seen[ResourceRuntimeRoot] && seen[ResourceRunnerProcess] && seen[ResourceProviderProcess] && seen[ResourceProviderGroup]
}

func requireResourceTime(resources []Resource, at UnixMillis) error {
	for _, resource := range resources {
		if resource.UpdatedAt.Int64() > at.Int64() {
			return ErrRevisionConflict
		}
	}
	return nil
}

func requireFinalizingTime(ctx context.Context, connection *sql.Conn, run Run, at UnixMillis) error {
	resources, err := resourcesForRun(ctx, connection, run.ID)
	if err != nil {
		return err
	}
	var change *Change
	if run.ChangeID != nil {
		value, found, err := changeByID(ctx, connection, *run.ChangeID)
		if err != nil {
			return err
		}
		if !found {
			return ErrCorruptState
		}
		change = &value
	}
	if err := requireResourceTime(resources, at); err != nil {
		return err
	}
	if change != nil && change.UpdatedAt.Int64() > at.Int64() {
		return ErrRevisionConflict
	}
	return nil
}

func proposalSQL(proposal Proposal) (kind string, code, detail, result any) {
	kind = proposal.kind.String()
	if proposal.code != 0 {
		code = proposal.code.String()
	}
	if proposal.detail != "" {
		detail = proposal.detail
	}
	if proposal.kind == OutcomeSucceeded {
		result = proposal.result
	}
	return
}

func exitSQL(exit ProcessExit) (kind string, code, signal any) {
	kind = exit.kind.String()
	if exit.code != nil {
		code = *exit.code
	}
	if exit.signal != nil {
		signal = *exit.signal
	}
	return
}
