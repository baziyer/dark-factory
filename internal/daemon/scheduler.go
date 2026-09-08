package daemon

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const schedulerPollInterval = time.Second

type schedulerEventKind uint8

const (
	schedulerAdmission schedulerEventKind = iota + 1
	schedulerDone
)

type schedulerEvent struct {
	kind     schedulerEventKind
	id       uint64
	admitted bool
	run      kernel.Run
	err      error
}

type scheduledOwner struct {
	observed bool
	admitted bool
}

// RunScheduler is the one process-owned admission coordinator. It owns and
// joins every synchronous RunNext call it starts. SQLite still chooses the
// exact runnable work and enforces capacity inside AdmitNext; wakeups and the
// single unobserved probe are only bounded scheduling hints.
func (daemon *Daemon) RunScheduler(ctx context.Context, spec SupervisorSpec) error {
	if daemon == nil || daemon.store == nil || ctx == nil {
		return fmt.Errorf("%w: invalid scheduler", kernel.ErrInvalidValue)
	}
	if err := daemon.beginScheduler(); err != nil {
		return err
	}
	defer daemon.endScheduler()

	ownedCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	runAttempt := spec.scheduledAttempt
	if runAttempt == nil {
		runAttempt = daemon.RunNext
	}
	validateCompletion := spec.scheduledCompletion
	if validateCompletion == nil {
		validateCompletion = func(observed kernel.Run) error {
			return daemon.validateScheduledCompletion(spec.ChangeParent, spec.UnsettledCompletion, observed)
		}
	}
	events := make(chan schedulerEvent, (kernel.MaxFactoryCapacity+1)*2)
	owners := make(map[uint64]*scheduledOwner, kernel.MaxFactoryCapacity+1)
	var nextID, probeID uint64

	startProbe := func() {
		nextID++
		id := nextID
		probeID = id
		owners[id] = &scheduledOwner{}
		attemptSpec := spec
		attemptSpec.scheduledAttempt = nil
		attemptSpec.scheduledCompletion = nil
		attemptSpec.schedulerPoll = nil
		go func() {
			observations := 0
			attemptSpec.admissionObserved = func(admitted bool) {
				observations++
				events <- schedulerEvent{kind: schedulerAdmission, id: id, admitted: admitted, err: duplicateAdmissionObservation(observations)}
			}
			run, err := runAttempt(ownedCtx, attemptSpec)
			events <- schedulerEvent{kind: schedulerDone, id: id, run: run, err: err}
		}()
	}

	pollEvents := spec.schedulerPoll
	var poll *time.Ticker
	if pollEvents == nil {
		poll = time.NewTicker(schedulerPollInterval)
		pollEvents = poll.C
		defer poll.Stop()
	}
	stopping := false
	var resultErr error
	ctxDone := ctx.Done()
	if err := ownedCtx.Err(); err == nil {
		startProbe()
	} else {
		stopping = true
	}

	for !stopping || len(owners) != 0 {
		select {
		case <-ctxDone:
			if !stopping {
				stopping = true
				cancel()
			}
			ctxDone = nil
		case <-daemon.schedulerWake:
			if !stopping && resultErr == nil && probeID == 0 {
				startProbe()
			}
		case <-pollEvents:
			if !stopping && resultErr == nil {
				// Worker idle rules and event-driven overseer wakeups are enqueued
				// on the tick ahead of the probe that admits them. A round that
				// fails is retried next tick; the admission probe stays exact.
				if at, err := daemon.timestamp(); err == nil {
					_, _ = daemon.store.EnqueueIdleInstructions(ownedCtx, at)
					_, _ = daemon.store.EnqueueOverseerWakeups(ownedCtx, at)
				}
			}
			if !stopping && resultErr == nil && probeID == 0 {
				startProbe()
			}
		case event := <-events:
			owner := owners[event.id]
			if owner == nil {
				resultErr = errors.Join(resultErr, fmt.Errorf("%w: unknown scheduler owner", kernel.ErrCorruptState))
				if !stopping {
					stopping = true
					cancel()
				}
				continue
			}
			switch event.kind {
			case schedulerAdmission:
				if owner.observed || event.err != nil {
					resultErr = errors.Join(resultErr, event.err, fmt.Errorf("%w: duplicate admission observation", kernel.ErrCorruptState))
					if !stopping {
						stopping = true
						cancel()
					}
					continue
				}
				owner.observed = true
				owner.admitted = event.admitted
				if probeID == event.id {
					probeID = 0
				}
				if event.admitted && !stopping && resultErr == nil {
					startProbe()
				}
			case schedulerDone:
				admittedOwner := owner.observed && owner.admitted
				delete(owners, event.id)
				if probeID == event.id {
					probeID = 0
				}
				if !owner.observed {
					if !(stopping && errors.Is(event.err, context.Canceled)) {
						resultErr = errors.Join(resultErr, event.err, fmt.Errorf("%w: attempt ended before admission was observed", kernel.ErrCorruptState))
					}
				} else if owner.admitted {
					resultErr = errors.Join(resultErr, validateCompletion(event.run))
				} else if event.run.ID != (kernel.RunID{}) || event.err == nil || !errors.Is(event.err, kernel.ErrConflict) {
					resultErr = errors.Join(resultErr, event.err, fmt.Errorf("%w: invalid no-admission completion", kernel.ErrCorruptState))
				}
				if resultErr != nil && !stopping {
					stopping = true
					cancel()
				} else if admittedOwner && !stopping && probeID == 0 {
					startProbe()
				}
			default:
				resultErr = errors.Join(resultErr, fmt.Errorf("%w: invalid scheduler event", kernel.ErrCorruptState))
				if !stopping {
					stopping = true
					cancel()
				}
			}
		}
	}
	return resultErr
}

func duplicateAdmissionObservation(count int) error {
	if count == 1 {
		return nil
	}
	return fmt.Errorf("%w: repeated scheduler observation", kernel.ErrCorruptState)
}

func (daemon *Daemon) validateScheduledCompletion(changeParent string, unsettled func(kernel.RunID, error), observed kernel.Run) error {
	if observed.ID == (kernel.RunID{}) {
		return kernel.NewOutcomeUnknownError(fmt.Errorf("%w: admitted attempt returned no run", kernel.ErrCorruptState))
	}
	readRun := daemon.store.Run
	if daemon.scheduledRun != nil {
		readRun = daemon.scheduledRun
	}
	var current kernel.Run
	var found bool
	var err error
	for attempt := 0; attempt < supervisorReconcileAttempts; attempt++ {
		ctx, cancel := context.WithTimeout(context.Background(), supervisorStoreAttemptWindow)
		current, found, err = readRun(ctx, observed.ID)
		cancel()
		if err == nil || found || errors.Is(err, kernel.ErrCorruptState) {
			break
		}
	}
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return kernel.NewOutcomeUnknownError(err)
	}
	if current.Phase == kernel.RunFinalizing && current.Proposal != nil {
		settled, settleErr := daemon.settleRun(changeParent, current.ID)
		if settleErr != nil {
			if errors.Is(settleErr, kernel.ErrConflict) {
				if unsettled != nil {
					unsettled(current.ID, settleErr)
				}
				return nil
			}
			return kernel.NewOutcomeUnknownError(fmt.Errorf("daemon: scheduled completion was not settled: %w", settleErr))
		}
		current = settled
	}
	if current.Phase != kernel.RunTerminal {
		return kernel.NewOutcomeUnknownError(fmt.Errorf("%w: scheduled run remained %s", kernel.ErrConflict, current.Phase.String()))
	}
	return nil
}

func (daemon *Daemon) notifyScheduler() {
	if daemon == nil || daemon.schedulerWake == nil {
		return
	}
	select {
	case daemon.schedulerWake <- struct{}{}:
	default:
	}
}

func (daemon *Daemon) beginScheduler() error {
	daemon.attemptMu.Lock()
	defer daemon.attemptMu.Unlock()
	daemon.schedulerMu.Lock()
	defer daemon.schedulerMu.Unlock()
	if daemon.closing {
		return ErrTerminalClosed
	}
	if daemon.schedulerRunning {
		return kernel.ErrConflict
	}
	daemon.schedulerRunning = true
	return nil
}

func (daemon *Daemon) endScheduler() {
	daemon.schedulerMu.Lock()
	daemon.schedulerRunning = false
	daemon.schedulerMu.Unlock()
}
