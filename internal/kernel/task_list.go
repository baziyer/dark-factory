package kernel

import (
	"context"
	"fmt"
)

const TaskListPageSize = 10

type TaskList struct {
	Head    EventSequence
	Total   uint64
	Tasks   []TaskSummary
	HasMore bool
}

// ReadTaskList reads a bounded completion page in one transaction. The last
// (update time, ID) is a keyset cursor, so unrelated events do not invalidate it.
func (store *Store) ReadTaskList(ctx context.Context, agentID AgentID, beforeAt UnixMillis, beforeID TaskID) (TaskList, error) {
	if agentID.zero() {
		return TaskList{}, ErrInvalidValue
	}
	tx, err := store.beginRead(ctx)
	if err != nil {
		return TaskList{}, err
	}
	defer tx.Close()
	if _, found, err := agentByID(ctx, tx.connection, agentID); err != nil {
		return TaskList{}, err
	} else if !found {
		return TaskList{}, ErrNotFound
	}
	factory, err := factoryState(ctx, tx.connection)
	if err != nil {
		return TaskList{}, err
	}
	result := TaskList{Head: factory.Head}
	if err := tx.connection.QueryRowContext(ctx, `SELECT COUNT(*) FROM tasks WHERE assigned_agent_id = ? AND status NOT IN ('queued', 'running')`, agentID.Bytes()).Scan(&result.Total); err != nil {
		return TaskList{}, err
	}
	query := `SELECT ` + publicTaskColumns + ` FROM tasks WHERE assigned_agent_id = ? AND status NOT IN ('queued', 'running')`
	args := []any{agentID.Bytes()}
	if !beforeID.zero() {
		query += ` AND (updated_at_ms, id) < (?, ?)`
		args = append(args, beforeAt.Int64(), beforeID.Bytes())
	}
	rows, err := tx.connection.QueryContext(ctx, query+` ORDER BY updated_at_ms DESC, id DESC LIMIT ?`, append(args, TaskListPageSize+1)...)
	if err != nil {
		return TaskList{}, fmt.Errorf("read completed tasks: %w", err)
	}
	defer rows.Close()
	result.Tasks, err = scanPublicTasks(rows)
	if err != nil {
		return TaskList{}, err
	}
	result.HasMore = len(result.Tasks) > TaskListPageSize
	if result.HasMore {
		result.Tasks = result.Tasks[:TaskListPageSize]
	}
	return result, nil
}
