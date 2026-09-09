//go:build darwin

package daemon

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/changeworker"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

type supervisorKeys struct {
	run       kernel.RunID
	session   kernel.TerminalSessionID
	change    kernel.ChangeID
	resources kernel.AdmissionResourceIDs
	token     [32]byte
	proof     [32]byte
}

// supervisorAttemptOwner owns the outer runner until the live attempt is
// registered. After registration, the per-attempt owner loop owns all
// controller operations; this outer owner only joins that loop and then
// synchronously terminates/waits the outer child. A context or goroutine
// ending is never treated as cleanup.
type supervisorAttemptOwner struct {
	controller *runner.AttemptController
	live       *liveAttempt
	child      *runner.OwnedChild
	activated  bool
	reaped     bool
	outerExit  *runner.Exit
}

// reap waits the exact outer child and keeps what it learned. The status is the
// only thing that distinguishes a runner that exited cleanly from one that
// failed to exec or was killed, and four call sites waiting the child directly
// is how it came to be discarded on the most common path. Every wait that can
// still observe a status goes through here; OwnedChild.Close waits too, but
// only ever after this has already reached stateWaited, so it has none left to
// discard.
func (owner *supervisorAttemptOwner) reap(timeout time.Duration) (runner.Exit, error) {
	exit, err := owner.child.FinishAfterExit(timeout)
	if err != nil {
		return runner.Exit{}, err
	}
	owner.reaped = true
	owner.outerExit = &exit
	return exit, nil
}

// outerRunnerEvidence names how the outer runner ended. A control read that
// ends the stream, or a write that finds the peer gone, says only that the
// socket is finished — the same thing however the runner died. The owner has
// waited that exact child, so the status that tells those apart is in hand.
func (owner *supervisorAttemptOwner) outerRunnerEvidence() error {
	if owner.outerExit == nil {
		return nil
	}
	exit := *owner.outerExit
	launch := ""
	if exit.LaunchErr != "" {
		launch = fmt.Sprintf(" launch=%q", exit.LaunchErr)
	}
	return fmt.Errorf("daemon: outer runner exit code=%d signal=%d%s", exit.Code, exit.Signal, launch)
}

func (owner *supervisorAttemptOwner) close() error {
	if owner == nil {
		return nil
	}
	var terminationErr, controllerErr error
	if live := owner.live; live != nil {
		// live.close joins the owner goroutine, and that owner always closes
		// the controller before it returns. AttemptController.Close clears the
		// capability even when the close itself fails, so there is no state in
		// which this outer owner could usefully terminate or close it again.
		// The controller is kept, not nilled: its Spent bit is how the caller
		// learns whether the transport ended or a caller closed it.
		controllerErr = live.close()
		owner.live = nil
	} else if owner.controller != nil {
		// No live owner ever took over, so this owner still holds the only
		// control capability. A released provider belongs to the inner runner's
		// distinct process group; ask it to converge before dropping the
		// capability, because killing only the outer group cannot prove absence.
		terminationErr = owner.controller.Terminate()
		if errors.Is(terminationErr, runner.ErrState) {
			terminationErr = nil
		}
		controllerErr = owner.controller.Close()
	}
	if owner.child != nil {
		if owner.activated && !owner.reaped {
			// Once activated, the outer runner is the only owner of the inner
			// provider group. Let it converge and exit; terminating the outer
			// first could orphan that distinct group.
			for {
				if _, err := owner.reap(8 * time.Second); err == nil {
					break
				}
				time.Sleep(25 * time.Millisecond)
			}
		}
		// OwnedChild retains the exact outer identity. Any residual descriptor
		// cleanup or inert abort also completes before ownership is dropped.
		for {
			if err := owner.child.Close(); err == nil {
				break
			}
			time.Sleep(25 * time.Millisecond)
		}
		owner.child = nil
	}
	return errors.Join(terminationErr, controllerErr)
}

func (daemon *Daemon) runNext(ctx context.Context, spec SupervisorSpec) (_ kernel.Run, resultErr error) {
	if ctx == nil || daemon == nil || daemon.store == nil || spec.RuntimeParent == nil ||
		spec.ChangeParent == "" || !filepath.IsAbs(spec.ChangeParent) || filepath.Clean(spec.ChangeParent) != spec.ChangeParent ||
		spec.GitExecutable == "" || spec.BaseRevision == "" || spec.AttemptSocket == "" || spec.RunnerExecutable == "" || spec.FactoryctlExecutable == "" ||
		spec.AccountHome == "" || !filepath.IsAbs(spec.AccountHome) || filepath.Clean(spec.AccountHome) != spec.AccountHome || provider.ValidateToolPath(spec.ToolPath) != nil {
		return kernel.Run{}, fmt.Errorf("%w: invalid supervisor specification", kernel.ErrInvalidValue)
	}
	keys, err := newSupervisorKeys(rand.Reader)
	if err != nil {
		return kernel.Run{}, err
	}
	runtimeRoot, err := runtimeChildPath(spec.RuntimeParent, keys.run.String())
	if err != nil {
		return kernel.Run{}, err
	}
	digestBytes := sha256.Sum256(keys.token[:])
	digest, err := kernel.AttemptDigestFromBytes(digestBytes[:])
	if err != nil {
		return kernel.Run{}, err
	}
	resultProof, err := runner.NewResultProof(keys.proof)
	if err != nil {
		return kernel.Run{}, err
	}
	proofDigestBytes := sha256.Sum256(keys.proof[:])
	proofDigest, err := kernel.ResultProofDigestFromBytes(proofDigestBytes[:])
	if err != nil {
		return kernel.Run{}, err
	}
	at, err := daemon.timestamp()
	if err != nil {
		return kernel.Run{}, err
	}
	admissionKeys := kernel.AdmissionKeys{
		RunID: keys.run, TerminalSessionID: keys.session, AttemptDigest: digest, ResultProofDigest: proofDigest, CandidateChangeID: keys.change,
		Resources: keys.resources, RuntimeRoot: runtimeRoot,
	}
	admission, err := daemon.store.AdmitNext(ctx, admissionKeys, at)
	admissionObserved := false
	if err == nil && spec.admissionObserved != nil {
		spec.admissionObserved(admission.Admitted())
		admissionObserved = true
	}
	if err == nil && spec.afterAdmission != nil {
		err = spec.afterAdmission()
	}
	if err != nil {
		// Admission commit acknowledgement is ambiguous. Retain the freshly
		// generated bearer and exact keys until SQLite proves either that no
		// admission committed or that its authority has been revoked.
		reconcileAdmission := daemon.store.ReconcileAdmission
		if spec.reconcileAdmission != nil {
			reconcileAdmission = spec.reconcileAdmission
		}
		var reconcileErr error
		for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
			reconcileCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
			reconciled, readErr := reconcileAdmission(reconcileCtx, admissionKeys)
			cancel()
			reconcileErr = readErr
			if reconcileErr != nil {
				continue
			}
			if reconciled.Admitted() {
				if !admissionObserved && spec.admissionObserved != nil {
					spec.admissionObserved(true)
					admissionObserved = true
				}
				return daemon.failRunBeforeRuntime(*reconciled.Run, keys.resources.RuntimeRoot, kernel.FailureInternal, err)
			}
			if reconciled.Reason == kernel.NoAdmissionNotReconciled {
				// The reconciliation read proves the failed write created no run, so
				// a scheduler can treat this as its ordinary no-admission probe.
				if spec.admissionObserved != nil {
					spec.admissionObserved(false)
				}
				return kernel.Run{}, errors.Join(kernel.ErrConflict, err)
			}
			reconcileErr = kernel.ErrCorruptState
		}
		return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(err, reconcileErr))
	}
	if !admission.Admitted() {
		return kernel.Run{}, fmt.Errorf("%w: no admission (%s)", kernel.ErrConflict, admission.Reason.String())
	}
	run := *admission.Run
	// A worker runs in a Change prepared from the project; an orchestrator has
	// none and runs in its private runtime home, so every Change step below
	// is a worker's alone.
	worker := run.Role == kernel.RoleWorker
	project, found, err := daemon.store.Project(ctx, run.ProjectID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureInternal, err)
	}
	if project.VerificationPolicy != run.VerificationPolicy || project.VerificationPolicy != kernel.VerificationNone {
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSpawn, fmt.Errorf("%w: verification is not part of the kernel spike", kernel.ErrInvalidValue))
	}
	// The account is read from the agent at launch, not copied onto the run:
	// it is configuration, not part of the admitted work.
	accountConfigDir, err := daemon.agentAccountConfigDir(ctx, run.AgentID)
	if err != nil {
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureInternal, err)
	}
	factoryctl, err := runner.CommitExecutableLocator(spec.FactoryctlExecutable)
	if err != nil {
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSpawn, err)
	}
	repositoryIdentity, err := inspectRepositoryIdentity(project.Root)
	if err != nil {
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSource, err)
	}
	if worker != (run.ChangeID != nil && run.AdmittedChangeRevision != nil) {
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureInternal, kernel.ErrCorruptState)
	}
	var changeID kernel.ChangeID
	var finalName, stagingName string
	if worker {
		changeID = *run.ChangeID
		finalName = changeID.String()
		stagingName = "." + finalName + ".stage"
	}
	task, found, err := daemon.store.Task(ctx, run.TaskID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureInternal, err)
	}
	rawProviderTask := []byte(task.Body)
	if run.Provider != kernel.ProviderShell && len(rawProviderTask) == 0 {
		rawProviderTask = []byte(task.Title)
	}
	delivery, preparedTask, err := provider.PrepareTask(run.Provider, rawProviderTask)
	if err != nil {
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSpawn, err)
	}
	var startupInput []byte
	providerTask := rawProviderTask
	switch delivery {
	case provider.TaskDeliveryFD11:
		providerTask = preparedTask
	case provider.TaskDeliveryStartupTerminal:
		startupInput = preparedTask
	case provider.TaskDeliveryAttemptAPI:
		if len(preparedTask) != 0 {
			return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSpawn, provider.ErrInvalid)
		}
		providerTask = nil
	default:
		return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSpawn, provider.ErrInvalid)
	}
	var changeState kernel.Change
	var retained *changeworker.Result
	if worker {
		var found bool
		changeState, found, err = daemon.store.Change(ctx, changeID)
		if err != nil || !found {
			if err == nil {
				err = kernel.ErrCorruptState
			}
			return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureInternal, err)
		}
		if changeState.Phase == kernel.ChangeAvailable && changeState.Revision == *run.AdmittedChangeRevision {
			var retainedRepository change.RepositoryIdentity
			retained, retainedRepository, err = retainedWorkerCheckpoint(changeState)
			if err != nil || !retainedRepository.Equal(repositoryIdentity) {
				return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureSource, errors.Join(err, errInvalidContract))
			}
		} else if changeState.Phase != kernel.ChangeReserved || changeState.Revision != *run.AdmittedChangeRevision {
			return daemon.failRunBeforeRuntime(run, keys.resources.RuntimeRoot, kernel.FailureInternal, kernel.ErrCorruptState)
		}
	}

	// From CreateRuntime until the runtime resource is durably active, a
	// failure cannot be finalized live: the exact-edge grammar requires either
	// trusted runtime absence (unprovable after an uncertain create) or an
	// active runtime. The admitted row stays discoverable for recovery.
	runtimeValue, err := CreateRuntime(spec.RuntimeParent, keys.run.String())
	if err != nil {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(err)
	}
	runtimeOpen := true
	defer func() {
		if runtimeOpen {
			resultErr = errors.Join(resultErr, runtimeValue.Close())
		}
	}()
	binding, err := runtimeValue.Binding()
	if err != nil {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(err)
	}
	gotRuntimePath, runtimeFileIdentity, err := binding.Values()
	if err != nil || gotRuntimePath != runtimeRoot {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(err, errInvalidContract))
	}
	runtimeIdentity, err := pathResourceIdentity(runtimeFileIdentity)
	if err != nil {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(err)
	}
	if _, err := daemon.activateResource(ctx, run.ID, keys.resources.RuntimeRoot, runtimeIdentity); err != nil {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(err)
	}
	if _, err := runtimeValue.PublishAttemptToken(ctx, keys.token); err != nil {
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	config := changeworker.Config{
		Provider: run.Provider, Role: run.Role, Model: run.Model, ReasoningEffort: run.ReasoningEffort,
		RuntimePath: gotRuntimePath, RuntimeIdentity: runtimeFileIdentity,
		GitExecutable: spec.GitExecutable, FactoryctlExecutable: factoryctl.Path(), ToolPath: spec.ToolPath, AccountHome: spec.AccountHome, AccountConfigDir: accountConfigDir, RepositoryRoot: project.Root, RepositoryIdentity: repositoryIdentity,
		Revision: spec.BaseRevision, ChangeParent: spec.ChangeParent, FinalName: finalName, StagingName: stagingName,
		AttemptSocket: spec.AttemptSocket, Retained: retained, ProviderTask: providerTask,
	}
	workerConfig, err := changeworker.EncodeConfig(config)
	if err != nil {
		return daemon.failRun(run, kernel.FailureSource, err)
	}

	runtimeDirectory, lifetime, err := runtimeValue.DuplicateRunnerFiles()
	if err != nil {
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	filesOpen := true
	defer func() {
		if filesOpen {
			resultErr = errors.Join(resultErr, runtimeDirectory.Close(), lifetime.Close())
		}
	}()
	lease, _, err := runner.CreateGateLease(runtimeDirectory, lifetime, runner.OuterActivationMarkerName)
	if err != nil {
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	leaseOpen := true
	defer func() {
		if leaseOpen {
			resultErr = errors.Join(resultErr, lease.Close())
		}
	}()
	controller, childControl, err := runner.NewAttemptController()
	if err != nil {
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	controllerOpen := true
	defer func() {
		if controllerOpen {
			resultErr = errors.Join(resultErr, controller.Close())
		}
	}()
	home, err := binding.ProviderHome()
	if err != nil {
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	wrapper, err := runner.PrepareExecSpec(runner.ExecSpec{
		Target: spec.RunnerExecutable, Args: []string{"--change-worker"},
		Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: home,
	})
	if err != nil {
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	if err := controller.Configure(runner.AttemptSpec{
		AttemptID: run.ID.String(), Wrapper: wrapper,
		MarkerName: runner.InnerActivationMarkerName, ResultName: runner.AttemptResultSpoolName, ResultProof: resultProof,
		StartupInput: startupInput,
	}); err != nil {
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureProtocol, err)
	}
	outer, err := runner.PrepareExecSpec(runner.ExecSpec{
		Target: spec.RunnerExecutable, Args: []string{"--attempt-runner"},
		Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: home, Stdin: workerConfig, Control: childControl,
	})
	if err != nil {
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureSpawn, err)
	}
	// BeginRunnerStart is the durable permission for the sole Start below.
	// From here until ActivateRunner commits, failures converge through
	// positive abort/reap plus RecordUnregisteredRunnerConverged; the generic
	// failure edge deliberately rejects a starting runner.
	runnerResource, found, err := daemon.store.Resource(ctx, keys.resources.RunnerProcess)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	at, err = daemon.timestamp()
	if err != nil {
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	run, runnerResource, err = daemon.store.BeginRunnerStart(ctx, run.ID, keys.resources.RunnerProcess, run.Revision, runnerResource.Revision, at)
	if err != nil {
		_ = childControl.Close()
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	child, err := runner.StartBlocked(lease, spec.RunnerExecutable, outer, true)
	_ = childControl.Close()
	if err != nil {
		controllerOpen = false
		return daemon.convergeUnstartedRunner(run, &supervisorAttemptOwner{controller: controller}, keys.resources.RunnerProcess, err)
	}
	owner := &supervisorAttemptOwner{controller: controller, child: child}
	controllerOpen = false
	defer func() {
		closeErr := owner.close()
		resultErr = errors.Join(resultErr, closeErr)
		// Ask the controller how it ended rather than searching the failure for
		// sentinels. Only the transport sets that bit, and only when the socket
		// itself is finished, so a decoder, a directory read or a git child
		// cannot contribute one by accident.
		if owner.controller.Spent() {
			resultErr = errors.Join(resultErr, owner.outerRunnerEvidence())
		}
	}()
	runnerResourceIdentity, err := processResourceIdentity(child.Identity())
	if err != nil {
		return daemon.convergeUnstartedRunner(run, owner, keys.resources.RunnerProcess, err)
	}
	at, err = daemon.timestamp()
	if err != nil {
		return daemon.convergeUnstartedRunner(run, owner, keys.resources.RunnerProcess, err)
	}
	run, runnerResource, err = daemon.store.ActivateRunner(ctx, run.ID, keys.resources.RunnerProcess, run.Revision, runnerResource.Revision, runnerResourceIdentity, at)
	if err != nil {
		return daemon.convergeUnstartedRunner(run, owner, keys.resources.RunnerProcess, err)
	}
	activateOuter := func(child *runner.OwnedChild) (runner.FileIdentity, error) { return child.Activate() }
	if spec.activateOuter != nil {
		activateOuter = spec.activateOuter
	}
	activationMarker, err := activateOuter(child)
	if activationMarker.Device != 0 && activationMarker.Inode != 0 {
		owner.activated = true
	}
	if err != nil {
		return daemon.convergeActivatedRunner(run, owner, runtimeDirectory, keys.resources.RunnerProcess, runtimeIdentity, runnerResourceIdentity, err)
	}
	if !owner.activated {
		return daemon.convergeActivatedRunner(run, owner, runtimeDirectory, keys.resources.RunnerProcess, runtimeIdentity, runnerResourceIdentity, errInvalidContract)
	}
	ready, err := controller.Next(8 * time.Second)
	if err != nil || ready.Kind != runner.AttemptInnerReady {
		return daemon.convergeActivatedRunner(run, owner, runtimeDirectory, keys.resources.RunnerProcess, runtimeIdentity, runnerResourceIdentity, errors.Join(err, runner.ErrState))
	}
	providerIdentity, err := processResourceIdentity(ready.Identity)
	if err != nil {
		return daemon.convergeActivatedRunner(run, owner, runtimeDirectory, keys.resources.RunnerProcess, runtimeIdentity, runnerResourceIdentity, err)
	}
	if err := daemon.activateProviderResources(ctx, run.ID, keys.resources.ProviderProcess, keys.resources.ProviderGroup, providerIdentity); err != nil {
		return daemon.convergeActivatedRunner(run, owner, runtimeDirectory, keys.resources.RunnerProcess, runtimeIdentity, runnerResourceIdentity, err)
	}

	selectionEvent, err := releaseCheckpoint(controller, runner.StageSelection)
	if err != nil {
		return daemon.failRun(run, kernel.FailureSource, err)
	}
	if len(selectionEvent.Payload) != 0 {
		return daemon.failRun(run, kernel.FailureSource, errInvalidContract)
	}
	if worker {
		var found bool
		changeState, found, err = daemon.store.Change(ctx, changeID)
		if err != nil || !found {
			if err == nil {
				err = kernel.ErrCorruptState
			}
			return daemon.failRun(run, kernel.FailureInternal, err)
		}
	}
	preparationEvent, err := releaseCheckpoint(controller, runner.StagePreparation)
	if err != nil {
		return daemon.failRun(run, kernel.FailureSource, err)
	}
	var workerResult changeworker.Result
	var selection kernel.ChangeSelection
	var stage kernel.FileIdentity
	if worker {
		if workerResult, err = changeworker.DecodeResult(preparationEvent.Payload); err != nil {
			return daemon.failRun(run, kernel.FailureSource, err)
		}
		if selection, err = kernelSelectionCheckpoint(workerResult, repositoryIdentity); err != nil {
			return daemon.failRun(run, kernel.FailureSource, err)
		}
		if stage, err = kernelStageIdentity(workerResult.Tree); err != nil {
			return daemon.failRun(run, kernel.FailureSource, err)
		}
		at, err = daemon.timestamp()
		if err != nil {
			return daemon.failRun(run, kernel.FailureInternal, err)
		}
		if retained == nil {
			changeState, err = daemon.store.RecordChangePrepared(ctx, changeID, changeState.Revision, selection, stage, at)
			if err != nil {
				return daemon.failRun(run, kernel.FailureSource, err)
			}
		}
	} else if len(preparationEvent.Payload) != 0 {
		return daemon.failRun(run, kernel.FailureSource, errInvalidContract)
	}
	populationEvent, err := releaseCheckpoint(controller, runner.StagePopulation)
	if err != nil {
		return daemon.failRun(run, kernel.FailureSource, err)
	}
	if len(populationEvent.Payload) != 0 {
		return daemon.failRun(run, kernel.FailureSource, errInvalidContract)
	}
	if worker {
		facts, err := change.InspectPublished(ctx, spec.ChangeParent, finalName, workerResult.Tree, workerResult.Format, workerResult.Base)
		if err != nil || !resultMatchesFacts(workerResult, facts) {
			return daemon.failRun(run, kernel.FailureSource, errors.Join(err, errInvalidContract))
		}
		availability, err := kernelAvailability(facts)
		if err != nil {
			return daemon.failRun(run, kernel.FailureSource, err)
		}
		if retained == nil {
			at, err = daemon.timestamp()
			if err != nil {
				return daemon.failRun(run, kernel.FailureInternal, err)
			}
			changeState, err = daemon.store.MarkChangeAvailable(ctx, changeID, changeState.Revision, availability, at)
			if err != nil {
				return daemon.failRun(run, kernel.FailureSource, err)
			}
		} else if !retainedWorkerCheckpointsMatch(changeState, selection, stage, availability) {
			return daemon.failRun(run, kernel.FailureSource, errInvalidContract)
		}
	}
	at, err = daemon.timestamp()
	if err != nil {
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	session, found, err := daemon.store.TerminalSessionForRun(ctx, run.ID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	run, err = daemon.store.ActivateRun(ctx, run.ID, session.ID, run.Revision, session.Revision, at)
	if err != nil {
		return daemon.failRun(run, kernel.FailureActivation, err)
	}
	// Register the owner before provider release. The owner is not attachable
	// until it observes TerminalReady, but it already owns the controller and
	// will synchronously converge it if any later step fails.
	live := newLiveAttempt(daemon, run.ID, session.ID, controller)
	live.beforeProviderStateCheck = spec.beforeProviderStateCheck
	if err := daemon.registerLiveAttempt(live); err != nil {
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	startLiveAttempt(live, ctx)
	owner.live = live
	// Retain the exact controller fallback until the live owner has joined and
	// proved Close. It is not used concurrently: supervisorAttemptOwner.close
	// only reaches it after live.close has synchronously joined the owner.
	if spec.beforeProviderRelease != nil {
		spec.beforeProviderRelease()
	}
	if err := factoryctl.Verify(); err != nil {
		return daemon.failRun(run, kernel.FailureActivation, err)
	}
	// This check is the cancellation/release linearization point. Cancellation
	// already visible here leaves the provider inert. Once it returns nil, the
	// release wins; cancellation becoming visible afterward may race the socket
	// write but cannot revoke that release, and the owner converges through
	// terminal evidence.
	if err := ctx.Err(); err != nil {
		return daemon.failRun(run, kernel.FailureInternal, err)
	}
	// The controller write is the irreversible effect immediately following the
	// cancellation decision. Test-only acknowledgement loss is injected only
	// after this write, so a hook cannot stand in for or delay provider release.
	if err := live.releaseProvider(ctx); err != nil {
		return daemon.failRun(run, kernel.FailureProtocol, err)
	}
	if spec.afterProviderRelease != nil {
		if err := spec.afterProviderRelease(); err != nil {
			return daemon.failRun(run, kernel.FailureProtocol, err)
		}
	}

	resultOutcome := live.waitResult()
	// The notice is shape-only and its socket is best-effort: a late credit or
	// terminate write racing the runner's own exit poisons the control socket
	// and loses queued frames. Authority is the exact no-replace artifact in
	// the runtime directory; ConsumeAttemptResult binds it to the durable run,
	// including the equality of the stored result-proof digest. With no notice,
	// wait the owned outer child and authenticate from disk alone.
	var record *runner.AttemptResultRecord
	var outerExit runner.Exit
	if resultOutcome.notice != nil {
		authenticated, authErr := runner.AuthenticateAttemptResult(runtimeDirectory, run.ID.String(), resultOutcome.notice)
		if authErr != nil {
			return daemon.failRun(run, kernel.FailureProtocol, errors.Join(resultOutcome.err, authErr))
		}
		record = authenticated
	} else {
		fallbackExit, waitErr := owner.reap(8 * time.Second)
		if waitErr != nil {
			return daemon.failRun(run, kernel.FailureProtocol, errors.Join(resultOutcome.err, waitErr))
		}
		outerExit = fallbackExit
		authenticated, authErr := runner.AuthenticateAttemptResult(runtimeDirectory, run.ID.String(), nil)
		if authErr != nil {
			return daemon.failRun(run, kernel.FailureProtocol, errors.Join(resultOutcome.err, authErr))
		}
		record = authenticated
	}
	result, err := kernelAttemptResult(record, run.ID, run.CredentialDigest, runtimeIdentity)
	if err != nil {
		return daemon.failRun(run, kernel.FailureProtocol, err)
	}
	run, err = daemon.consumeAttemptResult(result)
	if err != nil {
		return kernel.Run{}, err
	}
	exitEvent, err := terminalExitEvent(record)
	if err != nil {
		return run, err
	}
	if resultOutcome.observersRetained {
		if err := live.finishExit(context.Background(), exitEvent); err != nil {
			return run, fmt.Errorf("daemon: broadcast committed exit: %w", err)
		}
	}
	if !owner.reaped {
		waitedExit, waitErr := owner.reap(8 * time.Second)
		if waitErr != nil {
			return run, fmt.Errorf("daemon: wait outer runner: %w", waitErr)
		}
		outerExit = waitedExit
	}
	exitAt, err := daemon.timestamp()
	if err != nil {
		return run, err
	}
	exit, err := kernelProcessExit(outerExit, exitAt)
	if err != nil {
		return run, err
	}
	run, err = daemon.recordLiveRunnerExit(run.ID, keys.resources.RunnerProcess, runnerResourceIdentity, exit)
	if err != nil {
		return kernel.Run{}, err
	}
	run, err = daemon.closeTerminalAfterRunner(result)
	if err != nil {
		return kernel.Run{}, err
	}
	if err := daemon.removeAttemptResult(runtimeDirectory, result, record); err != nil {
		return run, err
	}
	if err := child.Close(); err != nil {
		return run, err
	}
	owner.child = nil
	if err := live.join(); err != nil && resultOutcome.err == nil {
		return run, err
	}
	// A dead owner loop whose result was recovered from disk is already
	// consumed evidence; its controller was closed by its own shutdown.
	owner.live = nil
	owner.controller = nil
	if err := lease.Close(); err != nil {
		return run, err
	}
	leaseOpen = false
	if err := errors.Join(runtimeDirectory.Close(), lifetime.Close()); err != nil {
		return run, err
	}
	filesOpen = false
	if err := runtimeValue.Close(); err != nil {
		return run, err
	}
	runtimeOpen = false

	presence, err := ObserveRuntimeLifetime(spec.RuntimeParent, keys.run.String(), runtimeFileIdentity)
	if err != nil || presence != RuntimeLeaseAvailable {
		return daemon.unresolvedRuntime(run, keys.resources.RuntimeRoot, errors.Join(err, errRetainedRuntime))
	}
	for {
		done, removeErr := RemoveRecordedRuntime(context.Background(), spec.RuntimeParent, keys.run.String(), runtimeFileIdentity)
		if removeErr != nil {
			return daemon.unresolvedRuntime(run, keys.resources.RuntimeRoot, removeErr)
		}
		if done {
			break
		}
		time.Sleep(25 * time.Millisecond)
	}
	if err := daemon.releaseResources(context.Background(), run.ID, kernel.ResourceRuntimeRoot); err != nil {
		return run, err
	}
	current, found, err := daemon.store.Run(context.Background(), run.ID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return run, err
	}
	at, err = daemon.timestamp()
	if err != nil {
		return run, err
	}
	if !worker {
		final, err := daemon.store.FinalizeRun(context.Background(), run.ID, current.Revision, at)
		return final, errors.Join(err, ctx.Err())
	}
	var settleErr error
	changeState, found, settleErr = daemon.store.Change(context.Background(), changeID)
	if settleErr != nil || !found {
		if settleErr == nil {
			settleErr = kernel.ErrCorruptState
		}
		return current, errors.Join(settleErr, ctx.Err())
	}
	var settlement kernel.ChangeSettlement
	settledFacts, settleErr := change.InspectPublished(context.Background(), spec.ChangeParent, finalName, workerResult.Tree, workerResult.Format, workerResult.Base)
	if refused, refusal := publicationRefused(settleErr); refused {
		// The tree the worker left cannot be published as it is. That is the
		// run's outcome, recorded now, not a reason to leave it finalizing.
		settlement, settleErr = refusedSettlement(spec.ChangeParent, changeState, run.ID, refusal)
	} else if settleErr == nil {
		var settledAvailability kernel.ChangeAvailability
		settledAvailability, settleErr = kernelAvailability(settledFacts)
		if settleErr == nil {
			settlement, settleErr = kernel.NewRetainedChangeSettlement(changeState.Revision, settledAvailability)
		}
	}
	if settleErr != nil {
		return current, errors.Join(settleErr, ctx.Err())
	}
	final, err := daemon.store.FinalizeWorkerRun(context.Background(), run.ID, current.Revision, settlement, at)
	return final, errors.Join(err, ctx.Err())
}

func newSupervisorKeys(reader io.Reader) (supervisorKeys, error) {
	if reader == nil {
		return supervisorKeys{}, fmt.Errorf("%w: missing random source", kernel.ErrInvalidValue)
	}
	readID := func() ([]byte, error) {
		value := make([]byte, kernel.IDBytes)
		_, err := io.ReadFull(reader, value)
		return value, err
	}
	var keys supervisorKeys
	var err error
	if raw, readErr := readID(); readErr != nil {
		return keys, readErr
	} else if keys.run, err = kernel.RunIDFromBytes(raw); err != nil {
		return keys, err
	}
	if raw, readErr := readID(); readErr != nil {
		return keys, readErr
	} else if keys.session, err = kernel.TerminalSessionIDFromBytes(raw); err != nil {
		return keys, err
	}
	if raw, readErr := readID(); readErr != nil {
		return keys, readErr
	} else if keys.change, err = kernel.ChangeIDFromBytes(raw); err != nil {
		return keys, err
	}
	ids := []*kernel.ResourceID{&keys.resources.RuntimeRoot, &keys.resources.RunnerProcess, &keys.resources.ProviderProcess, &keys.resources.ProviderGroup}
	for _, target := range ids {
		raw, readErr := readID()
		if readErr != nil {
			return keys, readErr
		}
		*target, err = kernel.ResourceIDFromBytes(raw)
		if err != nil {
			return keys, err
		}
	}
	if _, err := io.ReadFull(reader, keys.token[:]); err != nil {
		return supervisorKeys{}, err
	}
	if _, err := io.ReadFull(reader, keys.proof[:]); err != nil {
		return supervisorKeys{}, err
	}
	return keys, nil
}

func inspectRepositoryIdentity(path string) (change.RepositoryIdentity, error) {
	var stat syscall.Stat_t
	if !filepath.IsAbs(path) || filepath.Clean(path) != path || syscall.Lstat(path, &stat) != nil || stat.Mode&syscall.S_IFMT != syscall.S_IFDIR {
		return change.RepositoryIdentity{}, fmt.Errorf("%w: invalid durable repository root", kernel.ErrInvalidValue)
	}
	identity, err := change.NewRepositoryIdentity(uint64(stat.Dev), stat.Ino)
	if err != nil {
		return change.RepositoryIdentity{}, err
	}
	return identity, nil
}

func runtimeChildPath(parent *RuntimeParent, basename string) (string, error) {
	return parent.runtimeLocator(basename)
}

func releaseCheckpoint(controller *runner.AttemptController, stage runner.AttemptStage) (runner.AttemptEvent, error) {
	if err := controller.Release(stage); err != nil {
		return runner.AttemptEvent{}, err
	}
	event, err := controller.Next(12 * time.Second)
	if err != nil || event.Kind != runner.AttemptCheckpoint || event.Stage != stage {
		return runner.AttemptEvent{}, errors.Join(err, runner.ErrState)
	}
	return event, nil
}

func resultMatchesFacts(result changeworker.Result, facts change.TreeFacts) bool {
	return result.Tree.Equal(facts.Identity()) && result.Commitment.Equal(facts.Commitment()) &&
		result.EntryCount == facts.EntryCount() && result.BlobBytes == facts.BlobBytes()
}

func (daemon *Daemon) activateResource(ctx context.Context, runID kernel.RunID, resourceID kernel.ResourceID, identity kernel.ResourceIdentity) (kernel.Resource, error) {
	resource, found, err := daemon.store.Resource(ctx, resourceID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return kernel.Resource{}, err
	}
	at, err := daemon.timestamp()
	if err != nil {
		return kernel.Resource{}, err
	}
	return daemon.store.ActivateResource(ctx, runID, resourceID, resource.Revision, identity, at)
}

func (daemon *Daemon) activateProviderResources(ctx context.Context, runID kernel.RunID, processID, groupID kernel.ResourceID, identity kernel.ResourceIdentity) error {
	resources, err := daemon.store.Resources(ctx, runID)
	if err != nil {
		return err
	}
	var process, group kernel.Resource
	for _, resource := range resources {
		switch resource.ID {
		case processID:
			process = resource
		case groupID:
			group = resource
		}
	}
	if process.ID != processID || process.Kind != kernel.ResourceProviderProcess || group.ID != groupID || group.Kind != kernel.ResourceProviderGroup {
		return kernel.ErrCorruptState
	}
	at, err := daemon.timestamp()
	if err != nil {
		return err
	}
	_, _, err = daemon.store.ActivateProviderResources(ctx, runID, process.ID, process.Revision, group.ID, group.Revision, identity, at)
	return err
}

func (daemon *Daemon) releaseResource(ctx context.Context, runID kernel.RunID, resourceID kernel.ResourceID) error {
	resource, found, err := daemon.store.Resource(ctx, resourceID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return err
	}
	if resource.RunID != runID {
		return kernel.ErrConflict
	}
	if resource.State == kernel.ResourceReleased {
		return nil
	}
	at, err := daemon.timestamp()
	if err != nil {
		return err
	}
	_, err = daemon.store.ReleaseResource(ctx, runID, resourceID, resource.Revision, resource.Identity, at)
	return err
}

func (daemon *Daemon) releaseResources(ctx context.Context, runID kernel.RunID, kinds ...kernel.ResourceKind) error {
	resources, err := daemon.store.Resources(ctx, runID)
	if err != nil {
		return err
	}
	byKind := make(map[kernel.ResourceKind]kernel.Resource, len(resources))
	for _, resource := range resources {
		byKind[resource.Kind] = resource
	}
	for _, kind := range kinds {
		resource, found := byKind[kind]
		if !found {
			return kernel.ErrCorruptState
		}
		if err := daemon.releaseResource(ctx, runID, resource.ID); err != nil {
			return err
		}
	}
	return nil
}

func kernelProcessExit(exit runner.Exit, at kernel.UnixMillis) (kernel.ProcessExit, error) {
	if exit.Signal > 0 {
		return kernel.NewProcessExitSignal(1, int64(exit.Signal), at)
	}
	if exit.Code < 0 {
		return kernel.ProcessExit{}, errInvalidContract
	}
	return kernel.NewProcessExitCode(1, int64(exit.Code), at)
}

// failureDetail records why the daemon failed a run. The proposal detail is the
// one durable free-form field a failed run carries, it survives to terminal,
// and it was storing a constant while the cause was discarded.
func failureDetail(cause error) string {
	if cause == nil {
		return "daemon attempt failure"
	}
	detail := cause.Error()
	if len(detail) <= maxFailureDetailBytes {
		return detail
	}
	// The bound is on bytes, so the cut can land inside a rune. Drop bytes off
	// the end until what remains decodes, rather than storing a truncated
	// encoding in a column that is meant to be readable.
	detail = detail[:maxFailureDetailBytes]
	for len(detail) > 0 && !utf8.ValidString(detail) {
		detail = detail[:len(detail)-1]
	}
	return detail
}

const maxFailureDetailBytes = 4096

func (daemon *Daemon) failRun(run kernel.Run, code kernel.FailureCode, cause error) (kernel.Run, error) {
	// Infrastructure failure is another path into finalizing. It must share
	// the same linearization gate as attach and later terminal effects: an
	// attach cannot validate Running and then send a controller command after
	// this transition has revoked durable authority.
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	failure, err := kernel.NewFailureProposal(code, failureDetail(cause))
	if err != nil {
		return run, errors.Join(cause, err)
	}
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, run.ID)
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		if current.Phase != kernel.RunAdmitted && current.Phase != kernel.RunRunning {
			cancel()
			return current, cause
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		failed, failErr := daemon.store.FailRun(storeCtx, current.ID, current.Revision, failure, at)
		cancel()
		if failErr == nil {
			return failed, cause
		}
		lastErr = failErr
	}
	// Live ownership is synchronously converged by the caller's deferred owner.
	// Returning no Run avoids presenting the last admitted/running observation
	// as revoked; durable recovery must reconcile it later.
	return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(cause, lastErr))
}

// failRunBeforeRuntime finalizes an admitted run whose runtime was never
// created. The caller must not have attempted CreateRuntime for this run;
// trusted absence is exactly that precondition.
func (daemon *Daemon) failRunBeforeRuntime(run kernel.Run, runtimeID kernel.ResourceID, code kernel.FailureCode, cause error) (kernel.Run, error) {
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	failure, err := kernel.NewFailureProposal(code, failureDetail(cause))
	if err != nil {
		return run, errors.Join(cause, err)
	}
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, run.ID)
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		if current.Phase != kernel.RunAdmitted {
			cancel()
			return current, cause
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, runtimeID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = resourceErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		failed, failErr := daemon.store.FailRunWithRuntimeAbsent(storeCtx, current.ID, runtimeID, current.Revision, resource.Revision, failure, at)
		cancel()
		if failErr == nil {
			return failed, cause
		}
		lastErr = failErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: fail run before runtime: %w", errors.Join(cause, lastErr)))
}

// convergeUnstartedRunner converges a failure between BeginRunnerStart and
// ActivateRunner: any blocked child is aborted and positively reaped before
// the durable unregistered convergence is recorded.
func (daemon *Daemon) convergeUnstartedRunner(run kernel.Run, owner *supervisorAttemptOwner, runnerID kernel.ResourceID, cause error) (kernel.Run, error) {
	if err := owner.close(); err != nil {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(cause, err))
	}
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, run.ID)
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, runnerID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = resourceErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		converged, convergeErr := daemon.store.RecordUnregisteredRunnerConverged(storeCtx, run.ID, runnerID, current.Revision, resource.Revision, at)
		cancel()
		if convergeErr == nil {
			return converged, cause
		}
		lastErr = convergeErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: converge unstarted runner: %w", errors.Join(cause, lastErr)))
}

// convergeActivatedRunner converges a failure after ActivateRunner while the
// provider pair is still declared. Result authentication is always attempted
// before any absence conclusion; a present but non-authenticating artifact is
// retained fail-closed for recovery.
func (daemon *Daemon) convergeActivatedRunner(run kernel.Run, owner *supervisorAttemptOwner, runtimeDirectory *os.File, runnerID kernel.ResourceID, runtimeIdentity, runnerIdentity kernel.ResourceIdentity, cause error) (kernel.Run, error) {
	var notice *runner.AttemptResultNotice
	var outerExit runner.Exit
	reaped := false
	if owner.activated {
		if owner.controller != nil {
			// Terminate is valid only once the controller has consumed the
			// inner-ready registration; drain events and retry so a failure
			// before that frame still converges the released outer runner.
			terminated := false
			tryTerminate := func() {
				if terminated {
					return
				}
				if err := owner.controller.Terminate(); err == nil {
					terminated = true
				} else if !errors.Is(err, runner.ErrState) {
					cause = errors.Join(cause, err)
					terminated = true
				}
			}
			tryTerminate()
			deadline := time.Now().Add(8 * time.Second)
			for notice == nil && time.Now().Before(deadline) {
				ready, readyErr := owner.controller.NextReady(liveAttemptPoll)
				if readyErr != nil {
					break
				}
				if !ready {
					tryTerminate()
					continue
				}
				event, eventErr := owner.controller.Next(4 * time.Second)
				if eventErr != nil {
					break
				}
				if event.Kind == runner.AttemptResultReady && event.Result != nil {
					notice = event.Result
				} else {
					tryTerminate()
				}
			}
		}
		if owner.child != nil {
			for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
				exit, exitErr := owner.reap(8 * time.Second)
				if exitErr == nil {
					outerExit, reaped = exit, true
					break
				}
			}
			if !reaped {
				return kernel.Run{}, kernel.NewOutcomeUnknownError(cause)
			}
		}
	} else if err := owner.close(); err != nil {
		// A never-released child is aborted and positively reaped; no artifact
		// or marker can exist without the released exec.
		return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(cause, err))
	}
	record, authErr := runner.AuthenticateAttemptResult(runtimeDirectory, run.ID.String(), notice)
	if authErr == nil && record != nil {
		if !reaped {
			return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(cause, errInvalidContract))
		}
		result, resultErr := kernelAttemptResult(record, run.ID, run.CredentialDigest, runtimeIdentity)
		if resultErr != nil {
			return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(cause, resultErr))
		}
		converged, consumeErr := daemon.consumeAttemptResult(result)
		if consumeErr != nil {
			return kernel.Run{}, errors.Join(cause, consumeErr)
		}
		exitAt, clockErr := daemon.timestamp()
		if clockErr != nil {
			return converged, errors.Join(cause, clockErr)
		}
		exit, exitErr := kernelProcessExit(outerExit, exitAt)
		if exitErr != nil {
			return converged, errors.Join(cause, exitErr)
		}
		converged, exitErr = daemon.recordLiveRunnerExit(run.ID, runnerID, runnerIdentity, exit)
		if exitErr != nil {
			return kernel.Run{}, errors.Join(cause, exitErr)
		}
		converged, closeErr := daemon.closeTerminalAfterRunner(result)
		if closeErr != nil {
			return kernel.Run{}, errors.Join(cause, closeErr)
		}
		if removeErr := daemon.removeAttemptResult(runtimeDirectory, result, record); removeErr != nil {
			return converged, errors.Join(cause, removeErr)
		}
		return converged, cause
	}
	if present, presentErr := attemptResultPresent(runtimeDirectory); presentErr != nil || present {
		return kernel.Run{}, kernel.NewOutcomeUnknownError(errors.Join(cause, authErr, presentErr))
	}
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, run.ID)
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, runnerID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = resourceErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		converged, absenceErr := daemon.store.RecordRecoveredPreSessionRunnerAbsence(storeCtx, run.ID, runnerID, current.Revision, resource.Revision, runnerIdentity, at)
		cancel()
		if absenceErr == nil {
			return converged, cause
		}
		lastErr = absenceErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: pre-session runner absence: %w", errors.Join(cause, lastErr)))
}

func attemptResultPresent(dir *os.File) (bool, error) {
	var stat unix.Stat_t
	err := unix.Fstatat(int(dir.Fd()), runner.AttemptResultSpoolName, &stat, unix.AT_SYMLINK_NOFOLLOW)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, unix.ENOENT) {
		return false, nil
	}
	return false, err
}

// kernelAttemptResult is the single constructor turning an authenticated
// runner record into the kernel value. The result-proof digest comes only
// from the record itself, so the Store's digest equality cannot be bypassed
// by daemon code handing the kernel a digest the record did not produce.
func kernelAttemptResult(record *runner.AttemptResultRecord, runID kernel.RunID, attemptDigest kernel.AttemptDigest, runtimeIdentity kernel.ResourceIdentity) (kernel.AttemptResult, error) {
	if record == nil {
		return kernel.AttemptResult{}, errInvalidContract
	}
	recordDigest := record.ProofDigest()
	proofDigest, err := kernel.ResultProofDigestFromBytes(recordDigest[:])
	if err != nil {
		return kernel.AttemptResult{}, err
	}
	value := record.Result()
	if value.AttemptID() != runID.String() {
		return kernel.AttemptResult{}, errInvalidContract
	}
	switch value.Kind() {
	case runner.AttemptResultInnerUnregisteredConverged:
		return kernel.NewInnerUnregisteredConvergedAttemptResult(runID, attemptDigest, proofDigest, runtimeIdentity)
	case runner.AttemptResultInnerConverged:
		identity, present := value.Process()
		if !present {
			return kernel.AttemptResult{}, errInvalidContract
		}
		processIdentity, identityErr := processResourceIdentity(identity)
		if identityErr != nil {
			return kernel.AttemptResult{}, identityErr
		}
		var exit kernel.AttemptResultExit
		if code, ok := value.Code(); ok {
			exit, err = kernel.NewAttemptResultExitCode(int64(code))
		} else if signal, ok := value.Signal(); ok {
			exit, err = kernel.NewAttemptResultExitSignal(int64(signal))
		} else {
			err = errInvalidContract
		}
		if err != nil {
			return kernel.AttemptResult{}, err
		}
		return kernel.NewInnerConvergedAttemptResult(runID, attemptDigest, proofDigest, runtimeIdentity, processIdentity, exit)
	default:
		return kernel.AttemptResult{}, errInvalidContract
	}
}

// terminalExitEvent maps the authenticated converged result to the exact wire
// exit for browser observers. The abort flag is daemon protocol state and is
// applied by the owner loop, never taken from the artifact.
func terminalExitEvent(record *runner.AttemptResultRecord) (TerminalEvent, error) {
	value := record.Result()
	if code, ok := value.Code(); ok {
		return TerminalEvent{Kind: TerminalEventExit, ExitCode: code}, nil
	}
	if signal, ok := value.Signal(); ok {
		return TerminalEvent{Kind: TerminalEventExit, ExitSignal: signal}, nil
	}
	return TerminalEvent{}, errInvalidContract
}

func (daemon *Daemon) consumeAttemptResult(result kernel.AttemptResult) (kernel.Run, error) {
	return daemon.consumeAttemptResultWithRecoveryReplay(result, false)
}

// consumeRecoveredAttemptResult recognizes the exact result that recovery
// already consumed before a later durable recovery edge was interrupted.
func (daemon *Daemon) consumeRecoveredAttemptResult(result kernel.AttemptResult) (kernel.Run, error) {
	return daemon.consumeAttemptResultWithRecoveryReplay(result, true)
}

func (daemon *Daemon) consumeAttemptResultWithRecoveryReplay(result kernel.AttemptResult, recovered bool) (kernel.Run, error) {
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, result.RunID())
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		consumed, consumeErr := daemon.store.ConsumeAttemptResult(storeCtx, result, current.Revision, at)
		if errors.Is(consumeErr, kernel.ErrConflict) && recovered && current.Phase == kernel.RunFinalizing && current.Revision.Int64() > 1 {
			previous, revisionErr := kernel.NewRevision(current.Revision.Int64() - 1)
			if revisionErr == nil {
				consumed, consumeErr = daemon.store.ConsumeAttemptResult(storeCtx, result, previous, at)
			} else {
				consumeErr = revisionErr
			}
		}
		cancel()
		if consumeErr == nil {
			return consumed, nil
		}
		lastErr = consumeErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: consume attempt result: %w", lastErr))
}

func (daemon *Daemon) recordLiveRunnerExit(runID kernel.RunID, resourceID kernel.ResourceID, identity kernel.ResourceIdentity, exit kernel.ProcessExit) (kernel.Run, error) {
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, runID)
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		resource, resourceFound, resourceErr := daemon.store.Resource(storeCtx, resourceID)
		if resourceErr != nil || !resourceFound {
			cancel()
			lastErr = resourceErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		recorded, _, recordErr := daemon.store.RecordLiveRunnerExitAndRelease(storeCtx, runID, resourceID, current.Revision, resource.Revision, identity, exit, at)
		cancel()
		if recordErr == nil {
			return recorded, nil
		}
		lastErr = recordErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: record live runner exit: %w", lastErr))
}

func (daemon *Daemon) closeTerminalAfterRunner(result kernel.AttemptResult) (kernel.Run, error) {
	var lastErr error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		current, found, readErr := daemon.store.Run(storeCtx, result.RunID())
		if readErr != nil || !found {
			cancel()
			lastErr = readErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		session, sessionFound, sessionErr := daemon.store.TerminalSessionForRun(storeCtx, result.RunID())
		if sessionErr != nil || !sessionFound {
			cancel()
			lastErr = sessionErr
			if lastErr == nil {
				lastErr = kernel.ErrCorruptState
			}
			continue
		}
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			cancel()
			lastErr = clockErr
			continue
		}
		closedRun, _, closeErr := daemon.store.CloseTerminalAfterRunner(storeCtx, result, current.Revision, session.Revision, at)
		cancel()
		if closeErr == nil {
			return closedRun, nil
		}
		lastErr = closeErr
	}
	return kernel.Run{}, kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: close terminal after runner: %w", lastErr))
}

func (daemon *Daemon) removeAttemptResult(runtimeDirectory *os.File, result kernel.AttemptResult, record *runner.AttemptResultRecord) error {
	storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	_, err := daemon.store.AuthorizeAttemptResultRemoval(storeCtx, result)
	cancel()
	if err != nil {
		return err
	}
	if err := runner.RemoveAttemptResult(runtimeDirectory, record); err != nil {
		return err
	}
	return runner.FinishAttemptResultRemoval(runtimeDirectory)
}

func (daemon *Daemon) unresolvedRuntime(run kernel.Run, resourceID kernel.ResourceID, cause error) (kernel.Run, error) {
	resource, found, err := daemon.store.Resource(context.Background(), resourceID)
	if err == nil && found && resource.State != kernel.ResourceReleased && resource.State != kernel.ResourceUnresolved {
		at, clockErr := daemon.timestamp()
		if clockErr != nil {
			err = clockErr
		} else {
			_, markErr := daemon.store.MarkResourceUnresolved(context.Background(), run.ID, resourceID, resource.Revision, resource.Identity, "runtime cleanup could not prove absence", at)
			err = markErr
		}
	}
	current, found, readErr := daemon.store.Run(context.Background(), run.ID)
	if readErr == nil && found {
		run = current
	}
	return run, errors.Join(cause, err, readErr)
}
