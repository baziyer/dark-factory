package kernel

import (
	"context"
	"database/sql"
	"unicode/utf8"
)

type OverseerSnapshot struct {
	ProjectID      ProjectID
	Head           EventSequence
	Agents         []AgentSummary
	Tasks          []OverseerTask
	Runs           []OverseerRunSummary
	Questions      []OverseerQuestion
	History        []TaskIntervention
	HistoryExcerpt bool
}

// OverseerTask carries the private work objective and terminal progress that a
// project's running orchestrator needs to supervise its workers.
type OverseerTask struct {
	ID                 TaskID
	ProjectID          ProjectID
	AssignedAgentID    AgentID
	Title              string
	Objective          string
	ObjectiveTruncated bool
	Status             TaskStatus
	Priority           int64
	BlockedReason      string
	Result             string
	ResultTruncated    bool
	Revision           Revision
}

type OverseerRunSummary struct {
	ID       RunID
	AgentID  AgentID
	TaskID   TaskID
	Phase    RunPhase
	Revision Revision
}

type OverseerQuestion struct {
	ID       HumanRequestID
	AgentID  AgentID
	TaskID   TaskID
	Status   HumanRequestStatus
	Revision Revision
	Question string
}

// OverseerSnapshotForAttempt returns only the live orchestrator's project.
// It intentionally carries outstanding question text because the overseer is
// the project authority that must route it; the ordinary dashboard does not.
func (store *Store) OverseerSnapshotForAttempt(ctx context.Context, digest AttemptDigest, selected *TaskID) (OverseerSnapshot, error) {
	read, err := store.beginRead(ctx)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	defer read.Close()
	authority, err := overseerRun(ctx, read.connection, digest)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	state, err := factoryState(ctx, read.connection)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	result := OverseerSnapshot{ProjectID: authority.ProjectID, Head: state.Head, Agents: []AgentSummary{}, Tasks: []OverseerTask{}, Runs: []OverseerRunSummary{}, Questions: []OverseerQuestion{}, History: []TaskIntervention{}, HistoryExcerpt: selected == nil}
	agents, err := read.connection.QueryContext(ctx, agentSummarySelect+` WHERE a.project_id = ? ORDER BY a.id LIMIT ?`, authority.ProjectID.Bytes(), SnapshotEntityLimit+1)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	for agents.Next() {
		agent, err := scanAgentSummary(agents)
		if err != nil {
			agents.Close()
			return OverseerSnapshot{}, err
		}
		if len(result.Agents) == SnapshotEntityLimit {
			agents.Close()
			return OverseerSnapshot{}, ErrSnapshotTooLarge
		}
		result.Agents = append(result.Agents, agent)
	}
	if err := agents.Err(); err != nil {
		agents.Close()
		return OverseerSnapshot{}, err
	}
	if err := agents.Close(); err != nil {
		return OverseerSnapshot{}, err
	}
	taskQuery, taskArgs := `SELECT id, project_id, assigned_agent_id, incarnation_id, work_revision, title, body, sent_back_instruction_bytes, status, priority, blocked_reason, result, completed_at_ms, revision, created_at_ms, updated_at_ms FROM tasks WHERE project_id = ? AND (status IN ('queued', 'running') OR id IN (SELECT id FROM tasks WHERE project_id = ? AND status NOT IN ('queued', 'running') ORDER BY updated_at_ms DESC, id DESC LIMIT 32)) ORDER BY priority DESC, created_at_ms ASC, id ASC LIMIT ?`, []any{authority.ProjectID.Bytes(), authority.ProjectID.Bytes(), SnapshotEntityLimit + 1}
	if selected != nil {
		taskQuery, taskArgs = `SELECT id, project_id, assigned_agent_id, incarnation_id, work_revision, title, body, sent_back_instruction_bytes, status, priority, blocked_reason, result, completed_at_ms, revision, created_at_ms, updated_at_ms FROM tasks WHERE project_id = ? AND id = ?`, []any{authority.ProjectID.Bytes(), selected.Bytes()}
	}
	tasks, err := read.connection.QueryContext(ctx, taskQuery, taskArgs...)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	for tasks.Next() {
		task, found, err := scanTask(tasks)
		if err != nil || !found {
			tasks.Close()
			if err == nil {
				err = ErrCorruptState
			}
			return OverseerSnapshot{}, err
		}
		if len(result.Tasks) == SnapshotEntityLimit {
			tasks.Close()
			return OverseerSnapshot{}, ErrSnapshotTooLarge
		}
		objective, objectiveTruncated := overseerExcerpt(task.Body, selected == nil)
		resultText, resultTruncated := overseerExcerpt(task.Result, selected == nil)
		result.Tasks = append(result.Tasks, OverseerTask{ID: task.ID, ProjectID: task.ProjectID, AssignedAgentID: task.AssignedAgentID, Title: task.Title, Objective: objective, ObjectiveTruncated: objectiveTruncated, Status: task.Status, Priority: task.Priority, BlockedReason: task.BlockedReason, Result: resultText, ResultTruncated: resultTruncated, Revision: task.Revision})
	}
	if err := tasks.Err(); err != nil {
		tasks.Close()
		return OverseerSnapshot{}, err
	}
	if err := tasks.Close(); err != nil {
		return OverseerSnapshot{}, err
	}
	if selected != nil && len(result.Tasks) == 0 {
		return OverseerSnapshot{}, ErrNotFound
	}
	historyQuery, historyArgs := `SELECT `+taskInterventionColumns+` FROM task_interventions WHERE project_id = ? ORDER BY created_at_ms DESC, operation_id DESC LIMIT ?`, []any{authority.ProjectID.Bytes(), MaxTaskInterventionHistory}
	if selected != nil {
		historyQuery, historyArgs = `SELECT `+taskInterventionColumns+` FROM task_interventions WHERE project_id = ? AND task_id = ? ORDER BY created_at_ms DESC, operation_id DESC LIMIT ?`, []any{authority.ProjectID.Bytes(), selected.Bytes(), MaxTaskInterventionHistory}
	}
	{
		history, err := read.connection.QueryContext(ctx, historyQuery, historyArgs...)
		if err != nil {
			return OverseerSnapshot{}, err
		}
		for history.Next() {
			item, found, err := scanTaskIntervention(history)
			if err != nil || !found {
				history.Close()
				if err == nil {
					err = ErrCorruptState
				}
				return OverseerSnapshot{}, err
			}
			result.History = append(result.History, item)
		}
		if err := history.Err(); err != nil {
			history.Close()
			return OverseerSnapshot{}, err
		}
		if err := history.Close(); err != nil {
			return OverseerSnapshot{}, err
		}
	}
	runs, err := read.connection.QueryContext(ctx, `SELECT `+runColumns+` FROM runs WHERE project_id = ? AND phase <> 'terminal' ORDER BY admitted_at_ms ASC, id ASC LIMIT ?`, authority.ProjectID.Bytes(), SnapshotEntityLimit+1)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	activeRuns := make(map[RunID]OverseerRunSummary)
	for runs.Next() {
		run, found, err := scanRun(runs)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return OverseerSnapshot{}, err
		}
		if len(result.Runs) == SnapshotEntityLimit {
			runs.Close()
			return OverseerSnapshot{}, ErrSnapshotTooLarge
		}
		summary := OverseerRunSummary{ID: run.ID, AgentID: run.AgentID, TaskID: run.TaskID, Phase: run.Phase, Revision: run.Revision}
		result.Runs = append(result.Runs, summary)
		activeRuns[run.ID] = summary
	}
	if err := runs.Err(); err != nil {
		runs.Close()
		return OverseerSnapshot{}, err
	}
	if err := runs.Close(); err != nil {
		return OverseerSnapshot{}, err
	}
	questions, err := read.connection.QueryContext(ctx, `SELECT `+humanRequestColumns+` FROM human_requests WHERE run_id IN (SELECT id FROM runs WHERE project_id = ?) AND status IN ('open', 'delivering', 'delivery_unknown') ORDER BY created_at_ms ASC, id ASC LIMIT ?`, authority.ProjectID.Bytes(), MaxOpenHumanRequests+1)
	if err != nil {
		return OverseerSnapshot{}, err
	}
	for questions.Next() {
		request, found, err := scanHumanRequest(questions)
		if err != nil || !found {
			if err == nil {
				err = ErrCorruptState
			}
			return OverseerSnapshot{}, err
		}
		run, found := activeRuns[request.RunID]
		if !found {
			questions.Close()
			return OverseerSnapshot{}, ErrCorruptState
		}
		if len(result.Questions) == MaxOpenHumanRequests {
			questions.Close()
			return OverseerSnapshot{}, ErrSnapshotTooLarge
		}
		result.Questions = append(result.Questions, OverseerQuestion{ID: request.ID, AgentID: run.AgentID, TaskID: run.TaskID, Status: request.Status, Revision: request.Revision, Question: request.QuestionText})
	}
	if err := questions.Err(); err != nil {
		questions.Close()
		return OverseerSnapshot{}, err
	}
	if err := questions.Close(); err != nil {
		return OverseerSnapshot{}, err
	}
	return result, nil
}

func overseerExcerpt(value string, truncate bool) (string, bool) {
	const limit = 1024
	if !truncate || len(value) <= limit {
		return value, false
	}
	value = value[:limit]
	for !utf8.ValidString(value) {
		value = value[:len(value)-1]
	}
	return value, true
}

// overseerRun authenticates a live orchestrator while a control write holds
// its transaction. Every project-scoped control calls it before reading its
// target, so credential revocation and cross-project selection cannot race the
// write.
func overseerRun(ctx context.Context, connection *sql.Conn, digest AttemptDigest) (Run, error) {
	run, found, err := runByDigest(ctx, connection, digest)
	if err != nil {
		return Run{}, err
	}
	if !found || run.Phase != RunRunning || run.CredentialRevokedAt != nil || run.Role != RoleOrchestrator {
		return Run{}, ErrUnauthorized
	}
	return run, nil
}

// EnqueueTaskForOverseer creates or replays one worker task in the live
// orchestrator's project. The task identity remains the durable idempotency
// key; a retry with different immutable task data conflicts.
func (store *Store) EnqueueTaskForOverseer(ctx context.Context, digest AttemptDigest, spec NewTask, at UnixMillis) (Task, error) {
	if err := validateNewTask(spec); err != nil {
		return Task{}, err
	}
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		return Task{}, err
	}
	defer tx.Close()
	run, err := overseerRun(ctx, tx.connection, digest)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if spec.ProjectID != run.ProjectID {
		return Task{}, tx.Rollback(ErrUnauthorized)
	}
	agent, found, err := agentByID(ctx, tx.connection, spec.AssignedAgentID)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if !found || agent.ProjectID != run.ProjectID || agent.Role != RoleWorker {
		return Task{}, tx.Rollback(ErrUnauthorized)
	}
	existing, replay, err := taskCreationReplay(ctx, tx.connection, spec)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if replay {
		if err := tx.Rollback(nil); err != nil {
			return Task{}, err
		}
		return existing, nil
	}
	result, err := insertTaskOnConnection(ctx, tx.connection, spec, at)
	if err != nil {
		return Task{}, tx.Rollback(err)
	}
	if err := tx.Commit(ctx); err != nil {
		return Task{}, err
	}
	return result, nil
}
