package daemon

import (
	"context"
	"errors"
	"fmt"

	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// settleRun commits the terminal outcome of a finalizing run through the
// reviewed finalize edges. An unpublished candidate change settles abandoned;
// a published change settles retained after the published tree is re-read and
// verified against its recorded identity, base and format. Every state the
// kernel edges refuse is returned unchanged with the refusal — settlement
// never invents evidence.
func (daemon *Daemon) settleRun(changeParent string, runID kernel.RunID) (kernel.Run, error) {
	if daemon == nil || daemon.store == nil || runID == (kernel.RunID{}) {
		return kernel.Run{}, fmt.Errorf("%w: invalid run settlement", kernel.ErrInvalidValue)
	}
	ctx, cancel := context.WithTimeout(context.Background(), supervisorStoreAttemptWindow)
	defer cancel()
	run, found, err := daemon.store.Run(ctx, runID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrNotFound
		}
		return kernel.Run{}, err
	}
	if run.Phase == kernel.RunTerminal {
		return run, nil
	}
	if run.Phase != kernel.RunFinalizing || run.Proposal == nil {
		return run, fmt.Errorf("%w: run is not settleable", kernel.ErrConflict)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return run, err
	}
	if run.Role == kernel.RoleOrchestrator {
		return daemon.store.FinalizeRun(ctx, run.ID, run.Revision, at)
	}
	if run.ChangeID == nil {
		return run, fmt.Errorf("%w: worker run without a candidate change", kernel.ErrCorruptState)
	}
	changeState, found, err := daemon.store.Change(ctx, *run.ChangeID)
	if err != nil || !found {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return run, err
	}
	switch changeState.Phase {
	case kernel.ChangeReserved, kernel.ChangePrepared:
		settlement, err := kernel.NewAbandonedChangeSettlement(changeState.Revision)
		if err != nil {
			return run, err
		}
		return daemon.store.FinalizeWorkerRun(ctx, run.ID, run.Revision, settlement, at)
	case kernel.ChangeAvailable:
		settlement, err := retainedSettlement(ctx, changeParent, changeState)
		if refused, refusal := publicationRefused(err); refused {
			settlement, err = refusedSettlement(changeState, refusal)
		}
		if err != nil {
			return run, err
		}
		return daemon.store.FinalizeWorkerRun(ctx, run.ID, run.Revision, settlement, at)
	default:
		return run, fmt.Errorf("%w: change %s is not settleable for a finalizing run", kernel.ErrCorruptState, changeState.Phase.String())
	}
}

// publicationRefused tells the inspection's refusals of a published tree's
// own contents (a mode, a link, an empty directory, a bound) from every
// other error: a fault of the parent, the arguments or the durable record
// may pass tomorrow and is returned as before; a refusal never will.
func publicationRefused(err error) (bool, error) {
	var validation *change.ValidationError
	var limit *change.LimitError
	if errors.As(err, &validation) && validation.Tree || errors.As(err, &limit) && limit.Tree {
		return true, err
	}
	return false, nil
}

// refusedSettlement turns a refused inspection into the run's outcome: the
// Change is abandoned from available and the run fails with the reason and
// the path of the tree, which stays on disk for a person to read.
func refusedSettlement(changeState kernel.Change, refusal error) (kernel.ChangeSettlement, error) {
	return kernel.NewRefusedChangeSettlement(changeState.Revision, fmt.Sprintf("%v; the tree stays at changes/%s", refusal, changeState.ID.String()))
}

// retainedSettlement re-reads the published tree the durable change row names
// and verifies it against the recorded identity, base and format before any
// settlement authority exists, the evidence the supervisor's own finalize
// takes. The selection on an available change is the tree as the daemon
// made it before the worker ran, so the tree's contents settle as found;
// the inspection refuses a tree that is not the recorded one, and nothing
// here repairs anything.
func retainedSettlement(ctx context.Context, changeParent string, changeState kernel.Change) (kernel.ChangeSettlement, error) {
	if changeParent == "" || changeState.Selection == nil || changeState.TreeIdentity == nil {
		return kernel.ChangeSettlement{}, fmt.Errorf("%w: published change lacks retained evidence", kernel.ErrConflict)
	}
	format, base, stage, err := inspectPublishedArguments(*changeState.Selection, *changeState.TreeIdentity)
	if err != nil {
		return kernel.ChangeSettlement{}, err
	}
	facts, err := change.InspectPublished(ctx, changeParent, changeState.ID.String(), stage, format, base)
	if err != nil {
		return kernel.ChangeSettlement{}, errors.Join(fmt.Errorf("%w: published tree did not verify", kernel.ErrConflict), err)
	}
	availability, err := kernelAvailability(facts)
	if err != nil {
		return kernel.ChangeSettlement{}, err
	}
	return kernel.NewRetainedChangeSettlement(changeState.Revision, availability)
}
