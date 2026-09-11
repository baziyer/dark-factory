package kernel

import (
	"context"
	"fmt"
)

const maxProjectRunSeconds = 86400

// SetProjectLimits replaces a project's future run allowance and per-run
// wall-clock ceiling. An allowance is additional to the lifetime count already
// recorded; zero disables the count ceiling without erasing that history.
func (store *Store) SetProjectLimits(ctx context.Context, id ProjectID, expected Revision, allowance uint64, maxRunSeconds uint32, at UnixMillis) (Project, error) {
	if id.zero() || expected.Int64() < 1 || allowance > uint64(^uint64(0)>>1) || maxRunSeconds > maxProjectRunSeconds {
		return Project{}, fmt.Errorf("%w: project limits", ErrInvalidValue)
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Project{}, err
	}
	defer tx.Close()
	project, found, err := projectByID(ctx, tx.connection, id)
	if err != nil || !found {
		if err == nil {
			err = ErrNotFound
		}
		return Project{}, tx.Rollback(err)
	}
	if project.Revision != expected || at.Int64() < project.UpdatedAt.Int64() {
		return Project{}, tx.Rollback(ErrRevisionConflict)
	}
	limit := uint64(0)
	if allowance != 0 {
		if project.RunsUsed > uint64(^uint64(0)>>1)-allowance {
			return Project{}, tx.Rollback(ErrInvalidValue)
		}
		limit = project.RunsUsed + allowance
	}
	updated, err := tx.connection.ExecContext(ctx, `UPDATE projects SET run_budget_limit = ?, max_run_seconds = ?, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`, int64(limit), int64(maxRunSeconds), at.Int64(), id.Bytes(), expected.Int64())
	if err := requireOneRow(updated, err); err != nil {
		return Project{}, tx.Rollback(err)
	}
	if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityProject, id: id.Bytes(), revision: expected.Int64() + 1}}); err != nil {
		return Project{}, tx.Rollback(err)
	}
	project, found, err = projectByID(ctx, tx.connection, id)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Project{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Project{}, err
	}
	return project, nil
}

// OverdueRuns returns admitted and running runs whose project ceiling has
// elapsed. The caller cancels each at its returned revision; a concurrent
// result or stop simply wins the CAS.
func (store *Store) OverdueRuns(ctx context.Context, at UnixMillis) ([]Run, error) {
	tx, err := store.beginRead(ctx)
	if err != nil {
		return nil, err
	}
	defer tx.Close()
	rows, err := tx.connection.QueryContext(ctx, `SELECT `+runColumns+` FROM runs AS r JOIN projects AS p ON p.id = r.project_id WHERE r.phase IN ('admitted', 'running') AND p.max_run_seconds > 0 AND r.admitted_at_ms + p.max_run_seconds * 1000 <= ? ORDER BY r.admitted_at_ms, r.id`, at.Int64())
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var runs []Run
	for rows.Next() {
		run, found, err := scanRun(rows)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return nil, err
		}
		runs = append(runs, run)
	}
	return runs, rows.Err()
}
