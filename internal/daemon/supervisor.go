package daemon

import (
	"context"
	"errors"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

const (
	supervisorReconcileAttempts  = 3
	supervisorStoreAttemptWindow = 250 * time.Millisecond
)

// SupervisorSpec supplies the external installation and filesystem
// capabilities that do not belong in SQLite. RunNext derives the project,
// task, provider, source root, provider task, run identity and credential from
// durable state or fresh daemon-generated values.
type SupervisorSpec struct {
	RuntimeParent        *RuntimeParent
	ChangeParent         string
	GitExecutable        string
	BaseRevision         string
	AttemptSocket        string
	RunnerExecutable     string
	FactoryctlExecutable string
	ToolPath             string
	AccountHome          string

	// UnsettledCompletion reports a scheduled attempt whose durable
	// convergence remains finalizing after RunNext. The run stays discoverable
	// while the scheduler continues serving every other attempt.
	UnsettledCompletion func(kernel.RunID, error)

	// These unexported hooks are package-test-only ambiguity seams. Production
	// callers outside daemon cannot set them. beforeProviderStateCheck runs
	// inside the accepted release command immediately before the durable state
	// reread; it can arrange a competing finalization without changing the
	// production release path. afterProviderRelease runs only after the real
	// provider release frame has been written; it can report a lost
	// acknowledgement without replacing that irreversible write.
	activateOuter            func(*runner.OwnedChild) (runner.FileIdentity, error)
	afterAdmission           func() error
	beforeProviderStateCheck func() error
	afterProviderRelease     func() error
	reconcileAdmission       func(context.Context, kernel.AdmissionKeys) (kernel.AdmissionResult, error)
	beforeProviderRelease    func()

	// admissionObserved is a package-private scheduling hint. The Darwin
	// supervisor invokes it exactly once after AdmitNext commits successfully;
	// it carries no selected identity or transition authority.
	admissionObserved func(bool)

	// scheduledAttempt is a package-test-only seam for the joined coordinator.
	// Production always calls the concrete synchronous RunNext method.
	scheduledAttempt func(context.Context, SupervisorSpec) (kernel.Run, error)
	// scheduledCompletion is the matching package-test-only durable-read seam.
	// Production always reloads the returned run from the concrete Store.
	scheduledCompletion func(kernel.Run) error
	// schedulerPoll replaces the wall-clock ticker only in package tests.
	schedulerPoll <-chan time.Time
}

// RunNext admits and synchronously owns one complete provider attempt.
// The Darwin implementation does not return while it still owns a child.
func (daemon *Daemon) RunNext(ctx context.Context, spec SupervisorSpec) (run kernel.Run, resultErr error) {
	registration, err := daemon.registerSupervisor(ctx)
	if err != nil {
		return kernel.Run{}, err
	}
	daemon.rememberChangeParent(spec.ChangeParent)
	defer func() { daemon.endSupervisor(registration, resultErr) }()
	run, resultErr = daemon.runNext(registration.ctx, spec)
	if run.Phase == kernel.RunFinalizing && run.ID != (kernel.RunID{}) {
		recovered, recoverErr := daemon.recoverReturnedRun(spec.RuntimeParent, spec.ChangeParent, run.ID)
		run = recovered
		resultErr = errors.Join(resultErr, recoverErr)
	}
	return run, resultErr
}
