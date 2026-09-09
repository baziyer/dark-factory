//go:build darwin

package daemon

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

// RecoveredRunAction is the bounded disposition of one run in one sweep pass.
type RecoveredRunAction string

const (
	// RecoveredRuntimeAbsent finalized an admitted run whose runtime was
	// positively never created.
	RecoveredRuntimeAbsent RecoveredRunAction = "runtime-absent-failed"
	// RecoveredUnregistered converged a starting runner with no activation or
	// result residue.
	RecoveredUnregistered RecoveredRunAction = "unregistered-converged"
	// RecoveredPreSessionAbsence converged an activated runner that is positively
	// absent with the provider pair still declared and no artifact.
	RecoveredPreSessionAbsence RecoveredRunAction = "pre-session-absence"
	// RecoveredResultConsumed authenticated and consumed the on-disk result,
	// released the runner by positive absence, closed the terminal and removed
	// the artifact and runtime.
	RecoveredResultConsumed RecoveredRunAction = "result-consumed"
	// RecoveredResultConsumedUnsettled marks a consumed result whose run
	// could not be settled to a terminal record; the run stays finalizing
	// and discoverable, and the refusal rides the disposition.
	RecoveredResultConsumedUnsettled RecoveredRunAction = "result-consumed-unsettled"
	// RecoveredNoResultUnresolved converged what could be proven for an
	// activated attempt with no trusted result: failure proposal, unresolved
	// provider pair, released-by-absence runner. Deliberately not terminal.
	RecoveredNoResultUnresolved RecoveredRunAction = "no-result-unresolved"
	// RecoveredLiveHolder observed a held runtime lifetime lease and concluded
	// nothing: the attempt tree is alive.
	RecoveredLiveHolder RecoveredRunAction = "live-holder"
	// RecoveredConverged found no actionable residue this pass.
	RecoveredConverged RecoveredRunAction = "already-converged"
	// RecoveredUncertain is the fail-closed disposition: evidence conflicted
	// or was insufficient, and nothing was mutated beyond durable unresolved
	// markers already permitted by the grammar.
	RecoveredUncertain RecoveredRunAction = "uncertain"
)

// RecoveredRunDisposition reports one run's sweep outcome.
type RecoveredRunDisposition struct {
	RunID  kernel.RunID
	Action RecoveredRunAction
	Err    error
}

// RecoverAbandonedRuns is the finite recovery sweep over every nonterminal
// run. Result authentication is always attempted before any absence
// conclusion; absence is only a positive exact-identity observation combined
// with runtime-lifetime-lease availability; a held lease concludes nothing.
// The sweep is idempotent: every durable edge it uses recognizes its own
// exact replay.
func (daemon *Daemon) RecoverAbandonedRuns(ctx context.Context, parent *RuntimeParent, changeParent string) ([]RecoveredRunDisposition, error) {
	if daemon == nil || daemon.store == nil || ctx == nil || parent == nil || changeParent == "" {
		return nil, fmt.Errorf("%w: invalid recovery sweep", kernel.ErrInvalidValue)
	}
	runs, err := daemon.store.RecoverableRuns(ctx)
	if err != nil {
		return nil, err
	}
	dispositions := make([]RecoveredRunDisposition, 0, len(runs))
	for _, recoverable := range runs {
		daemon.attemptMu.Lock()
		_, live := daemon.attempts[recoverable.Run.ID]
		daemon.attemptMu.Unlock()
		if live {
			// A registered live owner is not abandoned; recovery never races it.
			continue
		}
		action, runErr := daemon.recoverRun(ctx, parent, changeParent, recoverable)
		dispositions = append(dispositions, RecoveredRunDisposition{RunID: recoverable.Run.ID, Action: action, Err: runErr})
	}
	return dispositions, nil
}

// recoverReturnedRun applies the restart recovery grammar to one exact run
// after RunNext has joined every owner and closed its runtime handles. It is
// intentionally narrower than RecoverAbandonedRuns: a scheduler may have
// other admitted attempts that are still between admission and registration,
// and a global sweep here could misclassify those concurrent owners.
func (daemon *Daemon) recoverReturnedRun(parent *RuntimeParent, changeParent string, runID kernel.RunID) (kernel.Run, error) {
	if daemon == nil || daemon.store == nil || parent == nil || changeParent == "" || runID == (kernel.RunID{}) {
		return kernel.Run{}, fmt.Errorf("%w: invalid returned-run recovery", kernel.ErrInvalidValue)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	recoverable, found, err := daemon.store.RecoverableRun(ctx, runID)
	if err != nil {
		return kernel.Run{}, err
	}
	if found {
		_, recoverErr := daemon.recoverRun(ctx, parent, changeParent, recoverable)
		current, currentFound, readErr := daemon.store.Run(ctx, runID)
		if readErr != nil || !currentFound {
			if readErr == nil {
				readErr = kernel.ErrCorruptState
			}
			return kernel.Run{}, errors.Join(recoverErr, readErr)
		}
		return current, recoverErr
	}
	current, found, err := daemon.store.Run(ctx, runID)
	if err != nil {
		return kernel.Run{}, err
	}
	if !found {
		return kernel.Run{}, kernel.ErrNotFound
	}
	return current, nil
}

func (daemon *Daemon) recoverRun(ctx context.Context, parent *RuntimeParent, changeParent string, recoverable kernel.RecoverableRun) (RecoveredRunAction, error) {
	run := recoverable.Run
	var runtimeRoot, runnerProcess, providerProcess, providerGroup kernel.Resource
	for _, resource := range recoverable.Resources {
		switch resource.Kind {
		case kernel.ResourceRuntimeRoot:
			runtimeRoot = resource
		case kernel.ResourceRunnerProcess:
			runnerProcess = resource
		case kernel.ResourceProviderProcess:
			providerProcess = resource
		case kernel.ResourceProviderGroup:
			providerGroup = resource
		}
	}
	if runtimeRoot.ID == (kernel.ResourceID{}) {
		return RecoveredUncertain, errInvalidContract
	}
	if runtimeRoot.Identity.Empty() {
		if run.Phase == kernel.RunFinalizing && runtimeRoot.State == kernel.ResourceReleased &&
			runnerProcess.State == kernel.ResourceReleased && providerProcess.State == kernel.ResourceReleased && providerGroup.State == kernel.ResourceReleased &&
			recoverable.TerminalSession.State == kernel.TerminalSessionClosed {
			_, settleErr := daemon.settleRun(changeParent, run.ID)
			return RecoveredConverged, settleErr
		}
		return daemon.recoverBeforeRuntime(ctx, parent, changeParent, run, runtimeRoot)
	}
	fileIdentity, err := runtimeFileIdentity(runtimeRoot.Identity)
	if err != nil {
		return RecoveredUncertain, err
	}
	if runtimeRoot.State == kernel.ResourceReleased {
		// The runtime is durably gone; only process/session residue can remain.
		if run.Phase == kernel.RunFinalizing && runnerProcess.State == kernel.ResourceReleased &&
			providerProcess.State == kernel.ResourceReleased && providerGroup.State == kernel.ResourceReleased &&
			recoverable.TerminalSession.State == kernel.TerminalSessionClosed {
			_, settleErr := daemon.settleRun(changeParent, run.ID)
			return RecoveredConverged, settleErr
		}
		return daemon.recoverReleasedRuntimeResidue(ctx, run, runnerProcess, providerProcess, providerGroup)
	}
	if run.Phase == kernel.RunFinalizing && runtimeRoot.State == kernel.ResourceUnresolved &&
		runnerProcess.State == kernel.ResourceReleased && providerProcess.State == kernel.ResourceReleased && providerGroup.State == kernel.ResourceReleased &&
		recoverable.TerminalSession.State == kernel.TerminalSessionClosed {
		// The supervisor consumed and removed the result, then could not
		// remove the runtime: what is left is a partly removed tree, which
		// cannot speak and may not open as a runtime. A held lifetime lease
		// still concludes nothing: something alive inherited it. Otherwise
		// try the removal again; what refused it may be gone, or removable
		// under a later rule.
		if presence, observeErr := ObserveRuntimeLifetime(parent, run.ID.String(), fileIdentity); observeErr == nil && presence == RuntimeLeaseHeld {
			return RecoveredLiveHolder, nil
		}
		if removeErr := daemon.removeRecordedRuntime(parent, run.ID, fileIdentity); removeErr != nil {
			return RecoveredUncertain, removeErr
		}
		_, settleErr := daemon.settleRun(changeParent, run.ID)
		return RecoveredConverged, settleErr
	}
	recovered, err := OpenRecoveredRuntime(ctx, parent, run.ID.String(), fileIdentity)
	if err != nil {
		if errors.Is(err, errRuntimeBusy) {
			return RecoveredLiveHolder, nil
		}
		if runtimeRoot.State == kernel.ResourceReleasing && errors.Is(err, errRecoveredRuntimeLayout) {
			if result, resultErr := recoveredConsumedAttemptResult(run, runtimeRoot, providerProcess); resultErr == nil {
				storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
				_, authorizeErr := daemon.store.AuthorizeAttemptResultRemoval(storeCtx, result)
				cancel()
				if authorizeErr == nil {
					if removeErr := daemon.removeRecordedRuntime(parent, run.ID, fileIdentity); removeErr != nil {
						return RecoveredUncertain, removeErr
					}
					_, settleErr := daemon.settleRun(changeParent, run.ID)
					return RecoveredConverged, settleErr
				}
			}
		}
		return RecoveredUncertain, err
	}
	defer func() { _ = recovered.Close() }()
	// Ordering rule: the artifact always speaks before any absence edge.
	if recovered.HasAttemptResult() {
		record, authErr := recovered.AuthenticateResult(run.ID.String())
		if authErr != nil {
			// Torn publish or tampering: retain the file, conclude nothing.
			return RecoveredUncertain, authErr
		}
		return daemon.recoverAuthenticatedResult(ctx, parent, changeParent, run, recovered, record, runtimeRoot, runnerProcess, fileIdentity)
	}
	if run.Phase == kernel.RunFinalizing && runtimeRoot.State == kernel.ResourceReleasing &&
		runnerProcess.State == kernel.ResourceReleased && providerProcess.State == kernel.ResourceReleased && providerGroup.State == kernel.ResourceReleased &&
		recoverable.TerminalSession.State == kernel.TerminalSessionClosed {
		if removeErr := daemon.removeRecoveredRuntime(ctx, parent, run.ID, recovered, fileIdentity); removeErr != nil {
			return RecoveredConverged, removeErr
		}
		_, settleErr := daemon.settleRun(changeParent, run.ID)
		return RecoveredConverged, settleErr
	}
	switch {
	case runnerProcess.State == kernel.ResourceStarting:
		if recovered.OuterActivated() {
			// Impossible by construction: the marker exists only after
			// ActivateRunner commits. Corrupt or hostile; fail closed.
			return RecoveredUncertain, errInvalidContract
		}
		converged, convergeErr := daemon.recordUnregisteredRunnerConverged(run.ID, runnerProcess.ID)
		if convergeErr != nil {
			return RecoveredUncertain, convergeErr
		}
		_ = converged
		if removeErr := daemon.removeRecoveredRuntime(ctx, parent, run.ID, recovered, fileIdentity); removeErr != nil {
			return RecoveredUnregistered, removeErr
		}
		_, settleErr := daemon.settleRun(changeParent, run.ID)
		return RecoveredUnregistered, settleErr
	case runnerProcess.State == kernel.ResourceActive && providerProcess.State == kernel.ResourceDeclared && run.Phase == kernel.RunAdmitted:
		if !daemon.recoveredRunnerAbsent(runnerProcess) {
			return RecoveredUncertain, errInvalidContract
		}
		if _, absenceErr := daemon.recordPreSessionRunnerAbsence(run.ID, runnerProcess.ID, runnerProcess.Identity); absenceErr != nil {
			return RecoveredUncertain, absenceErr
		}
		if removeErr := daemon.removeRecoveredRuntime(ctx, parent, run.ID, recovered, fileIdentity); removeErr != nil {
			return RecoveredPreSessionAbsence, removeErr
		}
		_, settleErr := daemon.settleRun(changeParent, run.ID)
		return RecoveredPreSessionAbsence, settleErr
	default:
		return daemon.recoverWithoutResult(ctx, run, runnerProcess, providerProcess, providerGroup)
	}
}

// recoverBeforeRuntime handles an admitted run whose runtime resource never
// acquired an identity: either CreateRuntime never ran (positively absent by
// name) or its outcome is unknown and the run stays discoverable.
func (daemon *Daemon) recoverBeforeRuntime(ctx context.Context, parent *RuntimeParent, changeParent string, run kernel.Run, runtimeRoot kernel.Resource) (RecoveredRunAction, error) {
	if run.Phase != kernel.RunAdmitted || runtimeRoot.State != kernel.ResourceDeclared {
		return RecoveredUncertain, errInvalidContract
	}
	present, err := runtimeChildPresent(parent, run.ID.String())
	if err != nil {
		return RecoveredUncertain, err
	}
	if present {
		// A directory exists but was never durably bound; its create was
		// uncertain. Conclude nothing until an operator or a later pass with
		// stronger evidence resolves it.
		return RecoveredUncertain, nil
	}
	failed, failErr := daemon.failRunBeforeRuntime(run, runtimeRoot.ID, kernel.FailureSpawn, fmt.Errorf("daemon: runtime absent at recovery"))
	if failErr != nil && failed.Phase != kernel.RunFinalizing {
		return RecoveredUncertain, failErr
	}
	_, settleErr := daemon.settleRun(changeParent, run.ID)
	return RecoveredRuntimeAbsent, settleErr
}

func (daemon *Daemon) recoverAuthenticatedResult(ctx context.Context, parent *RuntimeParent, changeParent string, run kernel.Run, recovered *RecoveredRuntime, record *runner.AttemptResultRecord, runtimeRoot, runnerProcess kernel.Resource, fileIdentity runner.FileIdentity) (RecoveredRunAction, error) {
	result, err := kernelAttemptResult(record, run.ID, run.CredentialDigest, runtimeRoot.Identity)
	if err != nil {
		return RecoveredUncertain, err
	}
	if _, err := daemon.consumeRecoveredAttemptResult(result); err != nil {
		return RecoveredUncertain, err
	}
	current, found, err := daemon.store.Resource(context.Background(), runnerProcess.ID)
	if err != nil || !found {
		return RecoveredUncertain, errors.Join(err, errInvalidContract)
	}
	if current.State == kernel.ResourceReleasing || current.State == kernel.ResourceUnresolved {
		if !daemon.recoveredRunnerAbsent(current) {
			return RecoveredUncertain, errInvalidContract
		}
		if _, absenceErr := daemon.recordRecoveredRunnerAbsence(run.ID, current.ID, current.Identity); absenceErr != nil {
			return RecoveredUncertain, absenceErr
		}
	}
	if _, err := daemon.closeTerminalAfterRunner(result); err != nil {
		return RecoveredUncertain, err
	}
	storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	_, authorizeErr := daemon.store.AuthorizeAttemptResultRemoval(storeCtx, result)
	cancel()
	if authorizeErr != nil {
		return RecoveredUncertain, authorizeErr
	}
	if err := recovered.RemoveResult(record); err != nil {
		return RecoveredUncertain, err
	}
	if removeErr := daemon.removeRecoveredRuntime(ctx, parent, run.ID, recovered, fileIdentity); removeErr != nil {
		return RecoveredResultConsumed, removeErr
	}
	// The consumed result's run settles to its terminal record: abandoned
	// for an unpublished change, retained for a verified published tree,
	// failed for a published tree whose own contents the inspection refuses.
	// Any other refusal keeps the run finalizing and discoverable and is
	// surfaced as its own disposition rather than logged indistinguishably
	// from success.
	if _, settleErr := daemon.settleRun(changeParent, run.ID); settleErr != nil {
		return RecoveredResultConsumedUnsettled, settleErr
	}
	return RecoveredResultConsumed, nil
}

// recoverWithoutResult converges what an activated attempt with no trusted
// result can prove: the run fails closed, the bound provider pair becomes
// durably unresolved, and the absent runner is released by observation. It is
// deliberately not terminal.
func (daemon *Daemon) recoverWithoutResult(ctx context.Context, run kernel.Run, runnerProcess, providerProcess, providerGroup kernel.Resource) (RecoveredRunAction, error) {
	acted := false
	if run.Phase == kernel.RunAdmitted || run.Phase == kernel.RunRunning {
		failure, err := kernel.NewFailureProposal(kernel.FailureInternal, "recovered active attempt without an attempt result")
		if err != nil {
			return RecoveredUncertain, err
		}
		at, err := daemon.timestamp()
		if err != nil {
			return RecoveredUncertain, err
		}
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		failed, failErr := daemon.store.FailRun(storeCtx, run.ID, run.Revision, failure, at)
		cancel()
		if failErr != nil {
			return RecoveredUncertain, failErr
		}
		run = failed
		acted = true
	}
	current := func(id kernel.ResourceID) (kernel.Resource, error) {
		resource, found, err := daemon.store.Resource(context.Background(), id)
		if err != nil || !found {
			return kernel.Resource{}, errors.Join(err, errInvalidContract)
		}
		return resource, nil
	}
	process, err := current(providerProcess.ID)
	if err != nil {
		return RecoveredUncertain, err
	}
	group, err := current(providerGroup.ID)
	if err != nil {
		return RecoveredUncertain, err
	}
	if process.State == kernel.ResourceReleasing && !process.Identity.Empty() && run.ProviderExit == nil {
		if !daemon.recoveredProviderAbsent(process) {
			return RecoveredUncertain, errInvalidContract
		}
		freshRun, found, err := daemon.store.Run(context.Background(), run.ID)
		if err != nil || !found {
			return RecoveredUncertain, errors.Join(err, errInvalidContract)
		}
		at, err := daemon.timestamp()
		if err != nil {
			return RecoveredUncertain, err
		}
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		_, _, _, markErr := daemon.store.MarkProviderResourcesUnresolved(storeCtx, run.ID, process.ID, group.ID, freshRun.Revision, process.Revision, group.Revision, process.Identity, "provider absent without an attempt result at recovery", at)
		cancel()
		if markErr != nil {
			return RecoveredUncertain, markErr
		}
		acted = true
	}
	runnerCurrent, err := current(runnerProcess.ID)
	if err != nil {
		return RecoveredUncertain, err
	}
	if (runnerCurrent.State == kernel.ResourceReleasing || runnerCurrent.State == kernel.ResourceUnresolved) && !runnerCurrent.Identity.Empty() {
		if !daemon.recoveredRunnerAbsent(runnerCurrent) {
			return RecoveredUncertain, errInvalidContract
		}
		if _, absenceErr := daemon.recordRecoveredRunnerAbsence(run.ID, runnerCurrent.ID, runnerCurrent.Identity); absenceErr != nil {
			return RecoveredUncertain, absenceErr
		}
		acted = true
	}
	if !acted {
		return RecoveredConverged, nil
	}
	return RecoveredNoResultUnresolved, nil
}

// recoverReleasedRuntimeResidue converges finalizing residue whose runtime is
// already durably released: an absent runner still awaiting its release edge.
func (daemon *Daemon) recoverReleasedRuntimeResidue(ctx context.Context, run kernel.Run, runnerProcess, providerProcess, providerGroup kernel.Resource) (RecoveredRunAction, error) {
	if (runnerProcess.State == kernel.ResourceReleasing || runnerProcess.State == kernel.ResourceUnresolved) && !runnerProcess.Identity.Empty() && run.RunnerExit == nil {
		if !daemon.recoveredRunnerAbsent(runnerProcess) {
			return RecoveredUncertain, errInvalidContract
		}
		if _, err := daemon.recordRecoveredRunnerAbsence(run.ID, runnerProcess.ID, runnerProcess.Identity); err != nil {
			return RecoveredUncertain, err
		}
		return RecoveredNoResultUnresolved, nil
	}
	return RecoveredConverged, nil
}

// recoveredRunnerAbsent is the positive exact-identity absence observation.
// The caller already holds the runtime lifetime lease, so a live tree cannot
// reach this check; the observation still refuses a present identity.
func (daemon *Daemon) recoveredRunnerAbsent(resource kernel.Resource) bool {
	identity, err := runnerIdentity(resource.Identity)
	if err != nil {
		return false
	}
	observation := runner.ObserveProcess(identity)
	return observation.Presence == runner.Absent || observation.Presence == runner.Reused
}

func (daemon *Daemon) recoveredProviderAbsent(resource kernel.Resource) bool {
	identity, err := runnerIdentity(resource.Identity)
	if err != nil {
		return false
	}
	observation := runner.ObserveProcessGroup(identity)
	return observation.Presence == runner.Absent || observation.Presence == runner.Reused
}

func (daemon *Daemon) recordUnregisteredRunnerConverged(runID kernel.RunID, runnerID kernel.ResourceID) (kernel.Run, error) {
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, runID)
		if readErr != nil || !found {
			cancel()
			lastErr = errors.Join(readErr, errInvalidContract)
			continue
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, runnerID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = errors.Join(resourceErr, errInvalidContract)
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		converged, convergeErr := daemon.store.RecordUnregisteredRunnerConverged(storeCtx, runID, runnerID, current.Revision, resource.Revision, at)
		cancel()
		if convergeErr == nil {
			return converged, nil
		}
		lastErr = convergeErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: recovered unregistered convergence: %w", lastErr))
}

func (daemon *Daemon) recordRecoveredRunnerAbsence(runID kernel.RunID, resourceID kernel.ResourceID, identity kernel.ResourceIdentity) (kernel.Run, error) {
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, runID)
		if readErr != nil || !found {
			cancel()
			lastErr = errors.Join(readErr, errInvalidContract)
			continue
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, resourceID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = errors.Join(resourceErr, errInvalidContract)
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		recorded, _, recordErr := daemon.store.RecordRecoveredRunnerAbsence(storeCtx, runID, resourceID, current.Revision, resource.Revision, identity, at)
		cancel()
		if recordErr == nil {
			return recorded, nil
		}
		lastErr = recordErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: recovered runner absence: %w", lastErr))
}

// removeRecoveredRuntime tears down the recovered runtime after durable
// convergence: the read capability closes first so the removal can rebind and
// prove the exact identity, then the runtime resource releases.
func (daemon *Daemon) removeRecoveredRuntime(ctx context.Context, parent *RuntimeParent, runID kernel.RunID, recovered *RecoveredRuntime, fileIdentity runner.FileIdentity) error {
	if err := recovered.Close(); err != nil {
		return err
	}
	return daemon.removeRecordedRuntime(parent, runID, fileIdentity)
}

func recoveredConsumedAttemptResult(run kernel.Run, runtimeRoot, providerProcess kernel.Resource) (kernel.AttemptResult, error) {
	if run.ProviderExit == nil {
		return kernel.NewInnerUnregisteredConvergedAttemptResult(run.ID, run.CredentialDigest, run.ResultProofDigest(), runtimeRoot.Identity)
	}
	if code, ok := run.ProviderExit.Code(); ok {
		exit, err := kernel.NewAttemptResultExitCode(code)
		if err != nil {
			return kernel.AttemptResult{}, err
		}
		return kernel.NewInnerConvergedAttemptResult(run.ID, run.CredentialDigest, run.ResultProofDigest(), runtimeRoot.Identity, providerProcess.Identity, exit)
	}
	signal, ok := run.ProviderExit.Signal()
	if !ok {
		return kernel.AttemptResult{}, errInvalidContract
	}
	exit, err := kernel.NewAttemptResultExitSignal(signal)
	if err != nil {
		return kernel.AttemptResult{}, err
	}
	return kernel.NewInnerConvergedAttemptResult(run.ID, run.CredentialDigest, run.ResultProofDigest(), runtimeRoot.Identity, providerProcess.Identity, exit)
}

func (daemon *Daemon) removeRecordedRuntime(parent *RuntimeParent, runID kernel.RunID, fileIdentity runner.FileIdentity) error {
	deadline := time.Now().Add(4 * time.Second)
	for {
		done, err := RemoveRecordedRuntime(context.Background(), parent, runID.String(), fileIdentity)
		if err != nil {
			return err
		}
		if done {
			break
		}
		if time.Now().After(deadline) {
			return errInvalidContract
		}
		time.Sleep(25 * time.Millisecond)
	}
	return daemon.releaseResources(context.Background(), runID, kernel.ResourceRuntimeRoot)
}

// runtimeFileIdentity converts the durable runtime path identity back to the
// exact filesystem identity recovery must rebind.
func runtimeFileIdentity(identity kernel.ResourceIdentity) (runner.FileIdentity, error) {
	path, ok := identity.Path()
	if !ok || path.Device() < 0 || path.Inode() <= 0 {
		return runner.FileIdentity{}, errInvalidContract
	}
	return runner.FileIdentity{Device: uint64(path.Device()), Inode: uint64(path.Inode())}, nil
}

// recordPreSessionRunnerAbsence commits the atomic pre-session finalization for an
// activated runner that is positively absent with the provider pair declared.
func (daemon *Daemon) recordPreSessionRunnerAbsence(runID kernel.RunID, resourceID kernel.ResourceID, identity kernel.ResourceIdentity) (kernel.Run, error) {
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, runID)
		if readErr != nil || !found {
			cancel()
			lastErr = errors.Join(readErr, errInvalidContract)
			continue
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, resourceID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = errors.Join(resourceErr, errInvalidContract)
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		converged, absenceErr := daemon.store.RecordRecoveredPreSessionRunnerAbsence(storeCtx, runID, resourceID, current.Revision, resource.Revision, identity, at)
		cancel()
		if absenceErr == nil {
			return converged, nil
		}
		lastErr = absenceErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: pre-session runner absence: %w", lastErr))
}

// runtimeChildPresent is the by-name presence probe for a runtime that never
// bound an identity. Presence alone is never treated as ownership.
func runtimeChildPresent(parent *RuntimeParent, basename string) (present bool, resultErr error) {
	if parent == nil || !validRuntimeName(basename) {
		return false, invalidContract(nil)
	}
	operation, err := parent.begin()
	if err != nil {
		return false, err
	}
	defer func() { resultErr = errors.Join(resultErr, operation.Close()) }()
	directory, err := operation.directory()
	if err != nil {
		return false, err
	}
	var stat unix.Stat_t
	err = unix.Fstatat(int(directory.Fd()), basename, &stat, unix.AT_SYMLINK_NOFOLLOW)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, unix.ENOENT) {
		return false, nil
	}
	return false, err
}
