package daemon

import (
	"context"
	"errors"
	"strings"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
)

var _ browser.AgentControlBackend = (*browserBackend)(nil)

func (backend *browserBackend) ControlAgent(ctx context.Context, principal browser.Principal, request browserprotocol.AgentControl) (browserprotocol.AgentControlResult, error) {
	operation, opErr := browserID(request.OperationID, kernel.TaskInterventionIDFromBytes)
	task, taskErr := browserID(request.TaskID, kernel.TaskIDFromBytes)
	run, runErr := browserID(request.RunID, kernel.RunIDFromBytes)
	taskRevision, taskRevErr := browserDecimal(request.ExpectedTaskRevision)
	runRevision, runRevErr := browserDecimal(request.ExpectedRunRevision)
	if errors.Join(opErr, taskErr, runErr, taskRevErr, runRevErr) != nil {
		return browserprotocol.AgentControlResult{}, browser.ErrStale
	}
	spec := kernel.TaskInterventionRequest{OperationID: operation, TaskID: task, RunID: run, ExpectedTaskRevision: taskRevision, ExpectedRunRevision: runRevision}
	switch request.Action {
	case "message":
		spec.Kind = kernel.TaskInterventionMessage
		spec.Payload = request.Instruction
	case "interrupt":
		spec.Kind = kernel.TaskInterventionInterrupt
	case "stop":
		spec.Kind = kernel.TaskInterventionStop
	case "replace":
		spec.Kind = kernel.TaskInterventionReplace
	default:
		return browserprotocol.AgentControlResult{}, browser.ErrStale
	}
	var receipt kernel.TaskIntervention
	var err error
	if spec.Kind == kernel.TaskInterventionMessage || spec.Kind == kernel.TaskInterventionInterrupt {
		receipt, err = backend.owner.browserIntervention(ctx, principal, spec)
	} else {
		clientID, release, authErr := backend.authorizePrincipal(ctx, principal, kernel.BrowserCapabilityHumanActions)
		if authErr != nil {
			return browserprotocol.AgentControlResult{}, authErr
		}
		defer release()
		var successor *kernel.NewTask
		if spec.Kind == kernel.TaskInterventionReplace {
			id, idErr := browserID(request.SuccessorTaskID, kernel.TaskIDFromBytes)
			incarnation, incErr := browserID(request.SuccessorIncarnationID, kernel.IncarnationIDFromBytes)
			if errors.Join(idErr, incErr) != nil {
				return browserprotocol.AgentControlResult{}, browser.ErrStale
			}
			current, found, readErr := backend.store.Task(ctx, task)
			if readErr != nil {
				return browserprotocol.AgentControlResult{}, mapBrowserError(readErr)
			}
			if !found {
				return browserprotocol.AgentControlResult{}, browser.ErrNotFound
			}
			if err := backend.prepareAgentInstruction(ctx, current.AssignedAgentID, request.Instruction); err != nil {
				return browserprotocol.AgentControlResult{}, err
			}
			successor = &kernel.NewTask{ID: id, IncarnationID: incarnation, Body: request.Instruction}
		}
		at, timeErr := backend.timestamp()
		if timeErr != nil {
			return browserprotocol.AgentControlResult{}, mapBrowserError(timeErr)
		}
		receipt, err = backend.store.StopRunForBrowser(ctx, clientID, spec, successor, at)
	}
	if err != nil {
		return browserprotocol.AgentControlResult{}, mapBrowserError(err)
	}
	backend.owner.notifyScheduler()
	result := browserprotocol.AgentControlResult{OperationID: request.OperationID, TaskID: request.TaskID, RunID: request.RunID}
	switch receipt.State {
	case kernel.TaskInterventionDelivered:
		result.Status = "delivered"
		if spec.Kind == kernel.TaskInterventionStop {
			result.Status = "stopping"
		}
		if spec.Kind == kernel.TaskInterventionReplace {
			result.Status = "queued"
			result.SuccessorTaskID = receipt.SuccessorTaskID.String()
		}
	case kernel.TaskInterventionRejected:
		result.Status = "rejected"
	default:
		result.Status = "delivery_unknown"
	}
	return result, nil
}

// TaskHistory reads explicit interventions only. Observers cannot retrieve
// private message text through the public state snapshot.
func (backend *browserBackend) TaskHistory(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.TaskHistoryGet) (browserprotocol.TaskHistory, error) {
	_, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityPrivateHumanRequestDetail)
	if err != nil {
		return browserprotocol.TaskHistory{}, err
	}
	defer release()
	taskID, err := browserID(request.TaskID, kernel.TaskIDFromBytes)
	if err != nil {
		return browserprotocol.TaskHistory{}, browser.ErrStale
	}
	task, found, err := backend.store.Task(ctx, taskID)
	if err != nil {
		return browserprotocol.TaskHistory{}, mapBrowserError(err)
	}
	if !found {
		return browserprotocol.TaskHistory{}, browser.ErrNotFound
	}
	history, err := backend.store.TaskInterventions(ctx, task.ProjectID, taskID)
	if err != nil {
		return browserprotocol.TaskHistory{}, mapBrowserError(err)
	}
	result := browserprotocol.TaskHistory{TaskID: request.TaskID, Entries: []browserprotocol.TaskHistoryEntry{}}
	for _, item := range history {
		body := item.Payload
		if item.Kind == kernel.TaskInterventionReplace && item.SuccessorTaskID != nil {
			body = "Replaced by task " + item.SuccessorTaskID.String()
		}
		if len(body) > 1024 {
			body = body[:1024]
			for !utf8.ValidString(body) {
				body = body[:len(body)-1]
			}
		}
		result.Entries = append(result.Entries, browserprotocol.TaskHistoryEntry{OperationID: item.OperationID.String(), Kind: item.Kind.String(), Actor: item.Actor.String(), Body: body, Status: item.State.String(), CreatedAtMillis: browserprotocol.Decimal(item.CreatedAt.Int64())})
		// Escaped text can occupy more wire bytes than UTF-8 storage. Keep the
		// newest complete entries inside the control-frame bound.
		if _, err := browserprotocol.EncodeTaskHistory(strings.Repeat("x", 64), result); errors.Is(err, browserprotocol.ErrOversized) {
			result.Entries = result.Entries[:len(result.Entries)-1]
			break
		} else if err != nil {
			return browserprotocol.TaskHistory{}, mapBrowserError(err)
		}
	}
	return result, nil
}

// TaskDetail reads task text through the same private capability as task
// history. Its instruction excludes the managed send-back note so an editor
// never appends that note a second time.
func (backend *browserBackend) TaskDetail(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.TaskDetailGet) (browserprotocol.TaskDetail, error) {
	_, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityPrivateHumanRequestDetail)
	if err != nil {
		return browserprotocol.TaskDetail{}, err
	}
	defer release()
	taskID, err := browserID(request.TaskID, kernel.TaskIDFromBytes)
	if err != nil {
		return browserprotocol.TaskDetail{}, browser.ErrStale
	}
	expected, err := browserDecimal(request.ExpectedRevision)
	if err != nil {
		return browserprotocol.TaskDetail{}, browser.ErrStale
	}
	task, found, err := backend.store.Task(ctx, taskID)
	if err != nil {
		return browserprotocol.TaskDetail{}, mapBrowserError(err)
	}
	if !found {
		return browserprotocol.TaskDetail{}, browser.ErrNotFound
	}
	if task.Revision != expected {
		return browserprotocol.TaskDetail{}, browser.ErrStale
	}
	expectedHead := kernel.EventSequence{}
	if request.ExpectedHead != nil {
		value, err := browserSequence(*request.ExpectedHead)
		if err != nil {
			return browserprotocol.TaskDetail{}, browser.ErrStale
		}
		expectedHead, err = kernel.NewEventSequence(int64(value))
		if err != nil {
			return browserprotocol.TaskDetail{}, browser.ErrStale
		}
	}
	instruction, instructionMore := taskDetailTextChunk(kernel.TaskInstruction(task), uint64(request.TextOffset))
	feedback, feedbackMore := taskDetailTextChunk(kernel.TaskFeedback(task), uint64(request.TextOffset))
	questions, nextPeerOffset, head, err := backend.store.PeerQuestionsForTask(ctx, task.ID, uint64(request.PeerOffset), expectedHead)
	if err != nil {
		return browserprotocol.TaskDetail{}, mapBrowserError(err)
	}
	result := browserprotocol.TaskDetail{TaskID: task.ID.String(), Revision: decimalRevision(task.Revision), Head: decimalSequence(head), Instruction: instruction, Feedback: feedback, PeerQuestions: []browserprotocol.TaskPeerQuestion{}}
	for _, question := range questions {
		item := browserprotocol.TaskPeerQuestion{
			ID: question.ID.String(), SourceTaskID: question.SourceTaskID.String(), TargetTaskID: question.TargetTaskID.String(),
			Question: question.Question, RecipientDeliveryState: question.RecipientDeliveryState.String(), AnswerDeliveryState: question.AnswerDeliveryState.String(),
			Revision: decimalRevision(question.Revision), CreatedAtMillis: browserprotocol.Decimal(question.CreatedAt.Int64()), UpdatedAtMillis: browserprotocol.Decimal(question.UpdatedAt.Int64()),
		}
		if question.Answer != "" {
			answer := question.Answer
			item.Answer = &answer
		}
		result.PeerQuestions = append(result.PeerQuestions, item)
	}
	if instructionMore || feedbackMore {
		next := browserprotocol.Decimal(uint64(request.TextOffset) + 2048)
		result.NextTextOffset = &next
	}
	if nextPeerOffset != nil {
		next := browserprotocol.Decimal(*nextPeerOffset)
		result.NextPeerOffset = &next
	}
	return result, nil
}

func taskDetailTextChunk(value string, offset uint64) (string, bool) {
	start := taskDetailRuneOffset(value, offset)
	end := taskDetailRuneOffset(value[start:], 2048) + start
	return value[start:end], end < len(value)
}

func taskDetailRuneOffset(value string, count uint64) int {
	for offset := range value {
		if count == 0 {
			return offset
		}
		count--
	}
	return len(value)
}

func (backend *browserBackend) prepareAgentInstruction(ctx context.Context, agentID kernel.AgentID, instruction string) error {
	agent, found, err := backend.store.Agent(ctx, agentID)
	if err != nil {
		return mapBrowserError(err)
	}
	if !found {
		return browser.ErrNotFound
	}
	if _, _, err := provider.PrepareTask(agent.Provider, []byte(instruction)); err != nil {
		return browser.ErrTooLarge
	}
	return nil
}
