//go:build darwin

package daemon

import (
	"context"
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
