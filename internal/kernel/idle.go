package kernel

import (
	"context"
	"crypto/rand"
	"fmt"
	"strings"
	"unicode/utf8"
)

const (
	MaxIdleAfterSeconds = 604800
	MaxIdleRunBudget    = 1000000
	maxIdleInstruction  = 32768
)

// validateIdleRule is the one rule the schema CHECKs also state: a standing
// instruction needs a wait, an instruction and a budget; wait needs nothing.
func validateIdleRule(rule IdleRule) error {
	if _, err := ParseIdlePolicy(string(rule.Policy)); err != nil {
		return err
	}
	if rule.AfterSeconds > MaxIdleAfterSeconds || rule.RunBudget > MaxIdleRunBudget || rule.RunsUsed > rule.RunBudget ||
		!utf8.ValidString(rule.Instruction) || byteLen(rule.Instruction) > maxIdleInstruction {
		return fmt.Errorf("%w: idle rule out of bounds", ErrInvalidValue)
	}
	if rule.Policy == IdleStandingInstruction && (rule.AfterSeconds < 1 || strings.Trim(rule.Instruction, " \t\r\n") == "" || rule.RunBudget < 1) {
		return fmt.Errorf("%w: a standing instruction needs a wait, text and a budget", ErrInvalidValue)
	}
	return nil
}

func idleRuleFromRow(policy string, after int64, instruction string, budget, used int64) (IdleRule, error) {
	if after < 0 || after > MaxIdleAfterSeconds || budget < 0 || budget > MaxIdleRunBudget || used < 0 || used > budget {
		return IdleRule{}, fmt.Errorf("%w: idle rule out of bounds", ErrInvalidValue)
	}
	rule := IdleRule{Policy: IdlePolicy(policy), AfterSeconds: uint32(after), Instruction: instruction, RunBudget: uint32(budget), RunsUsed: uint32(used)}
	return rule, validateIdleRule(rule)
}

// EnqueueIdleInstructions enqueues each idle agent's standing instruction to
// itself once its quiet spell has passed, and spends one of its idle runs
// for it, in one transaction. Idle means the agent itself could take work
// (not paused, tool budget left; the factory's dispatch switch and capacity
// stay admission's to apply once the task is queued) and has no queued or
// running task, so the rule never stacks on work; a run in flight is a
// running task, which the durable checks enforce. The quiet spell starts at
// the later of the agent's last edit and its
// last run's end, so editing the rule restarts the clock. An agent with any
// queued or running task is left alone, and so is a paused one; a budget
// already spent is never touched again until the operator sets a new one.
// The agent revision is the same CAS the console's enqueue uses, so a human
// instruction landing in the same window wins or loses cleanly.
func (store *Store) EnqueueIdleInstructions(ctx context.Context, at UnixMillis) ([]Task, error) {
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return nil, err
	}
	defer tx.Close()
	rows, err := tx.connection.QueryContext(ctx, `SELECT `+agentColumns+` FROM agents
		WHERE idle_policy = 'standing_instruction' AND paused = 0 AND idle_runs_used < idle_run_budget AND tool_calls_used < tool_budget_limit
		  AND NOT EXISTS (SELECT 1 FROM tasks WHERE assigned_agent_id = agents.id AND status IN ('queued', 'running'))
		  AND MAX(updated_at_ms, COALESCE((SELECT MAX(terminal_at_ms) FROM runs WHERE agent_id = agents.id), 0)) + idle_after_seconds * 1000 <= ?
		ORDER BY id`, at.Int64())
	if err != nil {
		return nil, tx.Rollback(err)
	}
	var due []Agent
	for rows.Next() {
		agent, found, err := scanAgent(rows)
		if err != nil || !found {
			rows.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return nil, tx.Rollback(err)
		}
		due = append(due, agent)
	}
	if err := rows.Err(); err != nil {
		return nil, tx.Rollback(err)
	}
	var tasks []Task
	for _, agent := range due {
		var ids [2][IDBytes]byte
		for index := range ids {
			if _, err := rand.Read(ids[index][:]); err != nil || ids[index] == [IDBytes]byte{} {
				if err == nil {
					err = fmt.Errorf("%w: generated zero task identifier", ErrCorruptState)
				}
				return nil, tx.Rollback(err)
			}
		}
		taskID, _ := TaskIDFromBytes(ids[0][:])
		incarnationID, _ := IncarnationIDFromBytes(ids[1][:])
		spec := NewTask{ID: taskID, ProjectID: agent.ProjectID, AssignedAgentID: agent.ID, IncarnationID: incarnationID, Title: "Standing instruction", Body: agent.Idle.Instruction, Priority: 0}
		if err := validateNewTask(spec); err != nil {
			return nil, tx.Rollback(err)
		}
		task, err := insertTaskOnConnection(ctx, tx.connection, spec, at)
		if err != nil {
			return nil, tx.Rollback(err)
		}
		result, err := tx.connection.ExecContext(ctx, `UPDATE agents SET idle_runs_used = idle_runs_used + 1, revision = revision + 1, updated_at_ms = ? WHERE id = ? AND revision = ?`,
			at.Int64(), agent.ID.Bytes(), agent.Revision.Int64())
		if err := requireOneRow(result, err); err != nil {
			return nil, tx.Rollback(err)
		}
		if err := appendInvalidations(ctx, tx.connection, at, []pendingInvalidation{{kind: EntityAgent, id: agent.ID.Bytes(), revision: agent.Revision.Int64() + 1}}); err != nil {
			return nil, tx.Rollback(err)
		}
		tasks = append(tasks, task)
	}
	if len(tasks) == 0 {
		return nil, tx.Rollback(nil)
	}
	if err := tx.Commit(ctx); err != nil {
		return nil, err
	}
	return tasks, nil
}
