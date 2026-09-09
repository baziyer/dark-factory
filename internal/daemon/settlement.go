package daemon

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

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
	storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	run, found, err := daemon.store.Run(storeCtx, runID)
	cancel()
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
	if run.Role == kernel.RoleOrchestrator {
		at, err := daemon.timestamp()
		if err != nil {
			return run, err
		}
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		final, err := daemon.store.FinalizeRun(storeCtx, run.ID, run.Revision, at)
		cancel()
		return final, err
	}
	if run.ChangeID == nil {
		return run, fmt.Errorf("%w: worker run without a candidate change", kernel.ErrCorruptState)
	}
	storeCtx, cancel = context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	changeState, found, err := daemon.store.Change(storeCtx, *run.ChangeID)
	cancel()
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
		at, err := daemon.timestamp()
		if err != nil {
			return run, err
		}
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		final, err := daemon.store.FinalizeWorkerRun(storeCtx, run.ID, run.Revision, settlement, at)
		cancel()
		return final, err
	case kernel.ChangeAvailable:
		settleRetained := daemon.settleRetained
		if settleRetained == nil {
			settleRetained = retainedSettlement
		}
		inspectionCtx, cancel := context.WithTimeout(context.Background(), supervisorInspectionWindow)
		settlement, err := settleRetained(inspectionCtx, changeParent, changeState)
		cancel()
		if refused, refusal := publicationRefused(err); refused {
			settlement, err = refusedSettlement(changeParent, changeState, run.ID, refusal)
		} else if refusedEarlier(changeParent, changeState, run.ID, err) {
			settlement, err = refusedSettlement(changeParent, changeState, run.ID, errRefusedEarlier)
		}
		if err != nil {
			return run, err
		}
		at, err := daemon.timestamp()
		if err != nil {
			return run, err
		}
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		final, err := daemon.store.FinalizeWorkerRun(storeCtx, run.ID, run.Revision, settlement, at)
		cancel()
		return final, err
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
	if errors.As(err, &validation) && validation.Tree {
		return true, validation
	}
	if errors.As(err, &limit) && limit.Tree {
		return true, limit
	}
	return false, nil
}

// errRefusedEarlier stands for a refusal an earlier pass decided and moved
// the tree for, whose settlement did not commit.
var errRefusedEarlier = errors.New("published tree refused by an earlier pass")

// refusedEarlier reports the tree gone from the Change's own name and present
// under the name a refusal by this run moves it to: the refusal was decided
// and the move made, and only the settlement is still owed.
func refusedEarlier(changeParent string, changeState kernel.Change, runID kernel.RunID, err error) bool {
	if !errors.Is(err, fs.ErrNotExist) {
		return false
	}
	_, statErr := os.Lstat(filepath.Join(changeParent, changeState.ID.String()+".refused-"+runID.String()[:8]))
	return statErr == nil
}

// refusedSettlement turns a refused inspection into the run's outcome: the
// tree is moved aside under the name the failure detail gives, since a retry
// of the task prepares a fresh tree under the Change's own name, and the
// Change is abandoned from available with the run failed for the reason. A
// move that fails leaves the run as it was, to be settled again later.
func refusedSettlement(changeParent string, changeState kernel.Change, runID kernel.RunID, refusal error) (kernel.ChangeSettlement, error) {
	aside := changeState.ID.String() + ".refused-" + runID.String()[:8]
	source, target := filepath.Join(changeParent, changeState.ID.String()), filepath.Join(changeParent, aside)
	if _, err := os.Lstat(target); err != nil {
		if !errors.Is(err, os.ErrNotExist) {
			return kernel.ChangeSettlement{}, err
		}
		if err := os.Rename(source, target); err != nil {
			return kernel.ChangeSettlement{}, err
		}
	}
	// The settlement commits after the move, so the move must be on disk
	// first, on every pass: a retry prepares under the Change's own name, and
	// a name still taken after a crash would refuse every retry for good. A
	// pass that finds the move made cannot know an earlier pass synced it.
	if err := syncChangeParent(changeParent); err != nil {
		return kernel.ChangeSettlement{}, err
	}
	return kernel.NewRefusedChangeSettlement(changeState.Revision, fmt.Sprintf("%v; the tree was moved to changes/%s", refusal, aside))
}

func syncChangeParent(path string) error {
	directory, err := os.Open(path)
	if err != nil {
		return err
	}
	return errors.Join(directory.Sync(), directory.Close())
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
