//go:build darwin || linux

package daemon

import (
	"context"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func TestRunLimitWatchdogTerminatesOwnedProvider(t *testing.T) {
	fixture := newSupervisorFixture(t, "set -eu\nprintf x >> __WITNESS__\nsleep 30\n")
	project, found, err := fixture.store.Project(context.Background(), supervisorProjectID(t, 1))
	if err != nil || !found {
		t.Fatalf("project = %+v, found=%v, err=%v", project, found, err)
	}
	if _, err := fixture.store.SetProjectLimits(context.Background(), project.ID, project.Revision, 0, 1, supervisorTime()); err != nil {
		t.Fatal(err)
	}
	done := make(chan struct {
		run kernel.Run
		err error
	}, 1)
	go func() {
		run, err := fixture.daemon.RunNext(context.Background(), fixture.spec)
		done <- struct {
			run kernel.Run
			err error
		}{run, err}
	}()
	if err := waitForWitness(fixture.witness, 10*time.Second); err != nil {
		t.Fatal(err)
	}
	time.Sleep(1100 * time.Millisecond)
	if err := fixture.daemon.enforceRunLimits(context.Background()); err != nil {
		t.Fatal(err)
	}
	var result struct {
		run kernel.Run
		err error
	}
	select {
	case result = <-done:
	case <-time.After(12 * time.Second):
		t.Fatal("watchdog did not terminate owned provider")
	}
	if result.err != nil || result.run.Phase != kernel.RunTerminal || result.run.Proposal == nil || result.run.Proposal.Kind() != kernel.OutcomeCancelled || result.run.Proposal.Detail() != runLimitDetail {
		t.Fatalf("watchdog run = %+v, err=%v", result.run, result.err)
	}
	fixture.assertReleased(t, result.run)
	if err := fixture.daemon.enforceRunLimits(context.Background()); err != nil {
		t.Fatal(err)
	}
	project, found, err = fixture.store.Project(context.Background(), project.ID)
	if err != nil || !found || project.RunsUsed != 1 {
		t.Fatalf("run allowance after watchdog = %+v, found=%v, err=%v", project, found, err)
	}
}
