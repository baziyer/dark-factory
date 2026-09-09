//go:build darwin

package daemon

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"sync/atomic"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func TestSchedulerRetriesUncertainTerminalCompletionRead(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x87)
	fixture.failBeforeRuntime(t)
	terminal, err := fixture.daemon.settleRun(fixture.changeParent, fixture.run.ID)
	if err != nil || terminal.Phase != kernel.RunTerminal {
		t.Fatalf("terminal row = %+v, %v", terminal, err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	var attempts atomic.Int64
	var completionReads atomic.Int64
	recovered := make(chan struct{})
	fixture.daemon.scheduledRun = func(ctx context.Context, id kernel.RunID) (kernel.Run, bool, error) {
		if completionReads.Add(1) == 1 {
			return kernel.Run{}, false, context.DeadlineExceeded
		}
		defer close(recovered)
		return fixture.store.Run(ctx, id)
	}
	spec := SupervisorSpec{
		scheduledAttempt: func(_ context.Context, spec SupervisorSpec) (kernel.Run, error) {
			if attempts.Add(1) == 1 {
				spec.admissionObserved(true)
				return terminal, nil
			}
			spec.admissionObserved(false)
			return kernel.Run{}, fmt.Errorf("%w: empty", kernel.ErrConflict)
		},
	}
	go func() { done <- fixture.daemon.RunScheduler(ctx, spec) }()
	waitSchedulerCalls(t, &attempts, 2)
	select {
	case <-recovered:
	case err := <-done:
		t.Fatalf("scheduler stopped after recovered read = %v", err)
	case <-time.After(time.Second):
		t.Fatal("scheduler did not retry terminal completion")
	}
	cancel()
	if err := waitSchedulerDone(t, done); err != nil {
		t.Fatalf("scheduler after recovered read = %v", err)
	}
}

func TestSchedulerKeepsAnActiveRunAfterReconciledAbsentAdmission(t *testing.T) {
	fixture := newSupervisorFixture(t, supervisorProgram(t, false, false))
	lock, err := sql.Open("sqlite3", "file:"+fixture.storePath)
	if err != nil {
		t.Fatal(err)
	}
	lock.SetMaxOpenConns(1)
	if _, err := lock.Exec("BEGIN IMMEDIATE"); err != nil {
		_ = lock.Close()
		t.Fatal(err)
	}
	released := false
	release := func() error {
		if released {
			return nil
		}
		released = true
		if _, err := lock.Exec("ROLLBACK"); err != nil {
			return err
		}
		return lock.Close()
	}
	t.Cleanup(func() {
		if err := release(); err != nil {
			t.Error(err)
		}
	})

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	probe := make(chan error, 1)
	activeJoined := make(chan struct{})
	var calls atomic.Int64
	spec := fixture.spec
	spec.scheduledCompletion = func(kernel.Run) error { return nil }
	spec.scheduledAttempt = func(ctx context.Context, scheduled SupervisorSpec) (kernel.Run, error) {
		switch calls.Add(1) {
		case 1:
			scheduled.admissionObserved(true)
			<-ctx.Done()
			close(activeJoined)
			return kernel.Run{ID: schedulerRunID(t, 0xa1)}, ctx.Err()
		case 2:
			probeSpec := fixture.spec
			probeSpec.admissionObserved = scheduled.admissionObserved
			run, err := fixture.daemon.RunNext(ctx, probeSpec)
			if releaseErr := release(); releaseErr != nil {
				err = errors.Join(err, releaseErr)
			}
			probe <- err
			return run, err
		default:
			scheduled.admissionObserved(false)
			return kernel.Run{}, fmt.Errorf("%w: empty", kernel.ErrConflict)
		}
	}
	go func() { done <- fixture.daemon.RunScheduler(ctx, spec) }()

	select {
	case err := <-probe:
		if !errors.Is(err, kernel.ErrBusy) || !errors.Is(err, kernel.ErrConflict) {
			t.Fatalf("reconciled absent admission = %v", err)
		}
	case err := <-done:
		t.Fatalf("scheduler stopped before absent admission reconciled: %v", err)
	case <-time.After(8 * time.Second):
		t.Fatal("absent admission did not finish")
	}
	select {
	case <-activeJoined:
		t.Fatal("scheduler canceled active run after reconciled absent admission")
	default:
	}
	fixture.daemon.notifyScheduler()
	waitSchedulerCalls(t, &calls, 3)
	select {
	case err := <-done:
		t.Fatalf("scheduler stopped after reconciled absent admission: %v", err)
	default:
	}
	cancel()
	if err := waitSchedulerDone(t, done); err != nil {
		t.Fatalf("scheduler shutdown = %v", err)
	}
	select {
	case <-activeJoined:
	case <-time.After(time.Second):
		t.Fatal("scheduler did not join active run")
	}
}
