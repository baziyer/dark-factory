package daemon

import (
	"context"
	"errors"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const runLimitDetail = "Run time limit reached"

// enforceRunLimits uses admission time so a run stuck before provider startup
// cannot evade the same ceiling. Cancellation is CAS-protected: a normal
// result or operator stop that won first is left untouched.
func (daemon *Daemon) enforceRunLimits(ctx context.Context) error {
	at, err := daemon.timestamp()
	if err != nil {
		return err
	}
	runs, err := daemon.store.OverdueRuns(ctx, at)
	if err != nil {
		return err
	}
	for _, run := range runs {
		if _, err := daemon.store.CancelRun(ctx, run.ID, run.Revision, runLimitDetail, at); err != nil && !errors.Is(err, kernel.ErrConflict) && !errors.Is(err, kernel.ErrRevisionConflict) {
			return err
		}
	}
	return nil
}
