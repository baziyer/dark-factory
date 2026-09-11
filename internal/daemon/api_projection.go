package daemon

import (
	"encoding/hex"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func projectSnapshot(snapshot kernel.DashboardSnapshot) api.DashboardSnapshot {
	result := api.DashboardSnapshot{
		Head: uint64(snapshot.Head.Int64()),
		Factory: api.FactorySummary{
			DispatchEnabled: snapshot.Factory.DispatchEnabled,
			Capacity:        snapshot.Factory.Capacity,
			ActiveRuns:      snapshot.Factory.ActiveRuns,
			Revision:        uint64(snapshot.Factory.Revision.Int64()),
		},
		Projects: make([]api.ProjectSummary, 0, len(snapshot.Projects)),
		Agents:   make([]api.AgentSummary, 0, len(snapshot.Agents)),
		Tasks:    make([]api.TaskSummary, 0, len(snapshot.Tasks)),
	}
	for _, project := range snapshot.Projects {
		result.Projects = append(result.Projects, api.ProjectSummary{
			ID: project.ID.String(), Name: project.Name, RunBudgetLimit: project.RunBudgetLimit, RunsUsed: project.RunsUsed, MaxRunSeconds: project.MaxRunSeconds, Revision: uint64(project.Revision.Int64()),
		})
	}
	for _, agent := range snapshot.Agents {
		result.Agents = append(result.Agents, api.AgentSummary{
			ID: agent.ID.String(), ProjectID: agent.ProjectID.String(), Name: agent.Name,
			Role: agent.Role, Provider: agent.Provider, Paused: agent.Paused, Revision: uint64(agent.Revision.Int64()),
		})
	}
	for _, task := range snapshot.Tasks {
		result.Tasks = append(result.Tasks, api.TaskSummary{
			ID: task.ID.String(), ProjectID: task.ProjectID.String(), AssignedAgentID: task.AssignedAgentID.String(),
			Title: task.Title, Status: task.Status, Priority: task.Priority, Revision: uint64(task.Revision.Int64()),
		})
	}
	return result
}

func projectOverseerSnapshot(snapshot kernel.OverseerSnapshot) api.OverseerSnapshot {
	result := api.OverseerSnapshot{
		ProjectID: snapshot.ProjectID.String(), Head: uint64(snapshot.Head.Int64()), NextOffset: snapshot.NextOffset, NextTextOffset: snapshot.NextTextOffset,
		Agents: []api.AgentSummary{}, Tasks: []api.OverseerTask{}, Runs: []api.OverseerRun{}, Questions: []api.OverseerQuestion{}, PeerQuestions: []api.PeerQuestion{}, History: []api.OverseerIntervention{},
	}
	for _, agent := range snapshot.Agents {
		result.Agents = append(result.Agents, api.AgentSummary{ID: agent.ID.String(), ProjectID: agent.ProjectID.String(), Name: agent.Name, Role: agent.Role, Provider: agent.Provider, Paused: agent.Paused, Revision: uint64(agent.Revision.Int64())})
	}
	for _, task := range snapshot.Tasks {
		result.Tasks = append(result.Tasks, api.OverseerTask{ID: task.ID.String(), ProjectID: task.ProjectID.String(), AssignedAgentID: task.AssignedAgentID.String(), Title: task.Title, Objective: task.Objective, ObjectiveTruncated: task.ObjectiveTruncated, Status: task.Status.String(), Priority: task.Priority, BlockedReason: task.BlockedReason, Result: task.Result, ResultTruncated: task.ResultTruncated, Revision: uint64(task.Revision.Int64())})
	}
	for _, run := range snapshot.Runs {
		result.Runs = append(result.Runs, api.OverseerRun{ID: run.ID.String(), AgentID: run.AgentID.String(), TaskID: run.TaskID.String(), Phase: run.Phase.String(), Revision: uint64(run.Revision.Int64())})
	}
	for _, question := range snapshot.Questions {
		result.Questions = append(result.Questions, api.OverseerQuestion{ID: question.ID.String(), AgentID: question.AgentID.String(), TaskID: question.TaskID.String(), Status: question.Status.String(), Revision: uint64(question.Revision.Int64()), Question: question.Question})
	}
	for _, question := range snapshot.PeerQuestions {
		result.PeerQuestions = append(result.PeerQuestions, api.PeerQuestion{ID: question.ID.String(), SourceTaskID: question.SourceTaskID.String(), TargetTaskID: question.TargetTaskID.String(), Question: question.Question, Answer: question.Answer, RecipientDeliveryState: question.RecipientDeliveryState.String(), AnswerDeliveryState: question.AnswerDeliveryState.String(), Revision: uint64(question.Revision.Int64())})
	}
	for _, item := range snapshot.History {
		detail := ""
		if item.ResultDetail != nil {
			detail = *item.ResultDetail
		}
		payload, truncated := item.Payload, snapshot.HistoryExcerpt
		if truncated {
			payload, truncated = overseerAPIExcerpt(payload)
		}
		successor := ""
		if item.SuccessorTaskID != nil {
			successor = item.SuccessorTaskID.String()
		}
		result.History = append(result.History, api.OverseerIntervention{OperationID: item.OperationID.String(), TaskID: item.TaskID.String(), RunID: item.RunID.String(), SuccessorTaskID: successor, Kind: item.Kind.String(), Actor: item.Actor.String(), Payload: payload, PayloadTruncated: truncated, State: item.State.String(), Detail: detail, CreatedAtMs: uint64(item.CreatedAt.Int64())})
	}
	return result
}

func overseerAPIExcerpt(value string) (string, bool) {
	const limit = 1024
	if len(value) <= limit {
		return value, false
	}
	value = value[:limit]
	for !utf8.ValidString(value) {
		value = value[:len(value)-1]
	}
	return value, true
}

func parseID(value string) ([]byte, error) {
	if len(value) != 32 || value != strings.ToLower(value) || value == strings.Repeat("0", 32) {
		return nil, fmt.Errorf("%w: invalid identifier", kernel.ErrInvalidValue)
	}
	decoded, err := hex.DecodeString(value)
	if err != nil {
		return nil, fmt.Errorf("%w: invalid identifier", kernel.ErrInvalidValue)
	}
	return decoded, nil
}

func parseProjectID(value string) (kernel.ProjectID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.ProjectID{}, err
	}
	return kernel.ProjectIDFromBytes(decoded)
}

func parseAgentID(value string) (kernel.AgentID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.AgentID{}, err
	}
	return kernel.AgentIDFromBytes(decoded)
}

func parseTaskID(value string) (kernel.TaskID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.TaskID{}, err
	}
	return kernel.TaskIDFromBytes(decoded)
}

func parseRunID(value string) (kernel.RunID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.RunID{}, err
	}
	return kernel.RunIDFromBytes(decoded)
}

func parseTaskInterventionID(value string) (kernel.TaskInterventionID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.TaskInterventionID{}, err
	}
	return kernel.TaskInterventionIDFromBytes(decoded)
}

func parseHumanRequestID(value string) (kernel.HumanRequestID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.HumanRequestID{}, err
	}
	return kernel.HumanRequestIDFromBytes(decoded)
}

func parseHumanDeliveryID(value string) (kernel.HumanRequestDeliveryID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.HumanRequestDeliveryID{}, err
	}
	return kernel.HumanRequestDeliveryIDFromBytes(decoded)
}

func parseIncarnationID(value string) (kernel.IncarnationID, error) {
	decoded, err := parseID(value)
	if err != nil {
		return kernel.IncarnationID{}, err
	}
	return kernel.IncarnationIDFromBytes(decoded)
}

func parseAgentRole(value string) (kernel.AgentRole, error) {
	switch value {
	case "worker":
		return kernel.RoleWorker, nil
	case "orchestrator":
		return kernel.RoleOrchestrator, nil
	default:
		return 0, fmt.Errorf("%w: invalid agent role", kernel.ErrInvalidValue)
	}
}
