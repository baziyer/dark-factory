package kernel

import (
	"context"
	"database/sql"
	"fmt"
)

// EnqueueOverseerWakeups consumes worker activity from the durable
// invalidation journal. A cursor is deliberately left behind a queued or
// running overseer, so activity while it works causes one follow-up after it
// exits. Journal pruning is conservative: a cursor behind the floor wakes once.
func (store *Store) EnqueueOverseerWakeups(ctx context.Context, at UnixMillis) ([]Task, error) {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return nil, err
	}
	defer tx.Close()
	factory, err := factoryState(ctx, tx.connection)
	if err != nil {
		return nil, tx.Rollback(err)
	}
	rows, err := tx.connection.QueryContext(ctx, `SELECT `+agentColumns+` FROM agents
		WHERE role = 'orchestrator' AND idle_policy = 'standing_instruction' AND paused = 0
		  AND idle_runs_used < idle_run_budget AND tool_calls_used < tool_budget_limit
		  AND MAX(updated_at_ms, COALESCE((SELECT MAX(terminal_at_ms) FROM runs WHERE agent_id = agents.id), 0)) + idle_after_seconds * 1000 <= ?
		ORDER BY id`, at.Int64())
	if err != nil {
		return nil, tx.Rollback(err)
	}
	var agents []Agent
	for rows.Next() {
		agent, found, err := scanAgent(rows)
		if err != nil || !found {
			rows.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return nil, tx.Rollback(err)
		}
		agents = append(agents, agent)
	}
	if err := rows.Err(); err != nil {
		return nil, tx.Rollback(err)
	}
	if err := rows.Close(); err != nil {
		return nil, tx.Rollback(err)
	}
	var tasks []Task
	changed := false
	for _, agent := range agents {
		cursor, found, err := overseerWakeCursor(ctx, tx.connection, agent.ID)
		if err != nil {
			return nil, tx.Rollback(err)
		}
		if !found {
			cursor = 0
		}
		var active int
		if err := tx.connection.QueryRowContext(ctx, `SELECT EXISTS(SELECT 1 FROM tasks WHERE assigned_agent_id = ? AND status IN ('queued', 'running'))`, agent.ID.Bytes()).Scan(&active); err != nil {
			return nil, tx.Rollback(err)
		}
		if active != 0 {
			continue
		}
		// A missing cursor is the one initial inspection for a newly enabled
		// rule. Thereafter only worker activity (or conservative prune recovery)
		// can start the instruction again.
		activity := !found || cursor < factory.Floor.Int64()-1
		if !activity {
			activity, err = workerInvalidationAfter(ctx, tx.connection, agent.ProjectID, cursor, factory.Head.Int64())
			if err != nil {
				return nil, tx.Rollback(err)
			}
		}
		if activity {
			task, err := enqueueStandingTask(ctx, tx.connection, agent, at)
			if err != nil {
				return nil, tx.Rollback(err)
			}
			tasks = append(tasks, task)
		}
		if cursor != factory.Head.Int64() || !found {
			if err := setOverseerWakeCursor(ctx, tx.connection, agent, factory.Head.Int64()); err != nil {
				return nil, tx.Rollback(err)
			}
			changed = true
		}
	}
	if !changed {
		return nil, tx.Rollback(nil)
	}
	if err := tx.Commit(ctx); err != nil {
		return nil, err
	}
	return tasks, nil
}

func overseerWakeCursor(ctx context.Context, connection *sql.Conn, agentID AgentID) (int64, bool, error) {
	var sequence int64
	err := connection.QueryRowContext(ctx, `SELECT invalidation_sequence FROM overseer_wake_cursors WHERE agent_id = ?`, agentID.Bytes()).Scan(&sequence)
	if err == sql.ErrNoRows {
		return 0, false, nil
	}
	if err != nil {
		return 0, false, err
	}
	if sequence < 0 {
		return 0, false, fmt.Errorf("%w: invalid overseer wake cursor", ErrCorruptState)
	}
	return sequence, true, nil
}

func setOverseerWakeCursor(ctx context.Context, connection *sql.Conn, agent Agent, sequence int64) error {
	if sequence < 0 {
		return fmt.Errorf("%w: invalid overseer wake cursor", ErrInvalidValue)
	}
	_, err := connection.ExecContext(ctx, `INSERT INTO overseer_wake_cursors(agent_id, project_id, invalidation_sequence) VALUES(?, ?, ?)
		ON CONFLICT(agent_id) DO UPDATE SET project_id = excluded.project_id, invalidation_sequence = excluded.invalidation_sequence`, agent.ID.Bytes(), agent.ProjectID.Bytes(), sequence)
	return err
}

func workerInvalidationAfter(ctx context.Context, connection *sql.Conn, projectID ProjectID, cursor, head int64) (bool, error) {
	if cursor >= head {
		return false, nil
	}
	var found int
	err := connection.QueryRowContext(ctx, `SELECT EXISTS(
		SELECT 1 FROM invalidations AS i
		WHERE i.sequence > ? AND i.sequence <= ? AND (
			(i.entity_kind = 'task' AND EXISTS(
				SELECT 1 FROM tasks AS t JOIN agents AS a ON a.id = t.assigned_agent_id
				WHERE t.id = i.entity_id AND t.project_id = ? AND a.project_id = t.project_id AND a.role = 'worker'
			)) OR
			(i.entity_kind = 'run' AND EXISTS(
				SELECT 1 FROM runs AS r WHERE r.id = i.entity_id AND r.project_id = ? AND r.role = 'worker'
			)) OR
			(i.entity_kind = 'human_request' AND EXISTS(
				SELECT 1 FROM human_requests AS h JOIN runs AS r ON r.id = h.run_id
				WHERE h.id = i.entity_id AND r.project_id = ? AND r.role = 'worker'
			)) OR
			(i.entity_kind = 'peer_question' AND EXISTS(
				SELECT 1 FROM peer_questions AS q WHERE q.id = i.entity_id AND q.project_id = ?
			))
		)
	)`, cursor, head, projectID.Bytes(), projectID.Bytes(), projectID.Bytes(), projectID.Bytes()).Scan(&found)
	if err != nil {
		return false, err
	}
	return found != 0, nil
}
