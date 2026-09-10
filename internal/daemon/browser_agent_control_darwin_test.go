package daemon

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func TestBrowserAgentReplacementPreservesHistoryAndProviderLimit(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 170)
	task, found, err := fixture.store.Task(context.Background(), run.TaskID)
	if err != nil || !found {
		t.Fatal(err)
	}
	request := browserprotocol.AgentControl{OperationID: strings.Repeat("01", 16), TaskID: task.ID.String(), RunID: run.ID.String(), ExpectedTaskRevision: decimalRevision(task.Revision), ExpectedRunRevision: decimalRevision(run.Revision), Action: "replace", Instruction: strings.Repeat("x", 8193), SuccessorTaskID: strings.Repeat("02", 16), SuccessorIncarnationID: strings.Repeat("03", 16)}
	principal := terminalEffectPrincipal(fixture.client.ID, 1)
	if _, err := fixture.backend.ControlAgent(context.Background(), principal, request); !errors.Is(err, browser.ErrTooLarge) {
		t.Fatalf("oversized replacement: %v", err)
	}
	unchanged, _, err := fixture.store.Run(context.Background(), run.ID)
	if err != nil || unchanged.Phase != kernel.RunRunning || unchanged.Revision != run.Revision {
		t.Fatalf("oversized instruction stopped run: %+v %v", unchanged, err)
	}
	request.Instruction = "A replacement objective"
	result, err := fixture.backend.ControlAgent(context.Background(), principal, request)
	if err != nil || result.Status != "queued" || result.SuccessorTaskID != request.SuccessorTaskID {
		t.Fatalf("replace result: %+v %v", result, err)
	}
	history, err := fixture.backend.TaskHistory(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TaskHistoryGet{TaskID: task.ID.String()})
	if err != nil || len(history.Entries) != 1 || history.Entries[0].Kind != "replace" || history.Entries[0].Status != "delivered" || !strings.Contains(history.Entries[0].Body, request.SuccessorTaskID) {
		t.Fatalf("history: %+v %v", history, err)
	}
	if _, err := browserprotocol.EncodeTaskHistory("history", history); err != nil {
		t.Fatal(err)
	}
}

func TestBrowserTaskHistoryRequiresPrivateTextCapability(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions)
	fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 170)
	if _, err := fixture.backend.TaskHistory(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TaskHistoryGet{TaskID: run.TaskID.String()}); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("private history exposed: %v", err)
	}
}

func TestBrowserTaskDetailSeparatesEditableInstructionFromFeedback(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 173)
	task, found, err := fixture.store.Task(context.Background(), run.TaskID)
	if err != nil || !found {
		t.Fatal(err)
	}
	base := task.Body
	detail, err := fixture.backend.TaskDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TaskDetailGet{TaskID: task.ID.String(), ExpectedRevision: decimalRevision(task.Revision)})
	if err != nil || detail.Instruction != base || detail.Feedback != "" || detail.Revision != decimalRevision(task.Revision) {
		t.Fatalf("detail = %+v, %v", detail, err)
	}
	if _, err := fixture.backend.TaskDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TaskDetailGet{TaskID: task.ID.String(), ExpectedRevision: decimalRevision(task.Revision) + 1}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("stale detail = %v", err)
	}
}

func TestBrowserTaskDetailRequiresPrivateTextCapability(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 174)
	task, found, err := fixture.store.Task(context.Background(), run.TaskID)
	if err != nil || !found {
		t.Fatal(err)
	}
	if _, err := fixture.backend.TaskDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TaskDetailGet{TaskID: task.ID.String(), ExpectedRevision: decimalRevision(task.Revision)}); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("private outcome detail exposed: %v", err)
	}
}

func TestTaskDetailTextChunkPagesOutcomeWithTheExistingCursor(t *testing.T) {
	outcome := strings.Repeat("x", 2049)
	first, more := taskDetailTextChunk(outcome, 0)
	second, final := taskDetailTextChunk(outcome, 2048)
	if !more || final || first+second != outcome {
		t.Fatalf("outcome page = %q/%q, more=%v/%v", first, second, more, final)
	}
}

func TestBrowserTaskDetailRejectsEditBetweenBriefAndPeerReads(t *testing.T) {
	ctx := context.Background()
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	fixture.pair(t)
	projectID, _ := kernel.ProjectIDFromBytes(adapterID(t, 174))
	project, err := fixture.store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "detail-project", Root: "/detail-project"}, adapterTime(t, 200))
	if err != nil {
		t.Fatal(err)
	}
	agentID, _ := kernel.AgentIDFromBytes(adapterID(t, 175))
	agent, err := fixture.store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: project.ID, Name: "detail-worker", Role: kernel.RoleWorker, Provider: kernel.ProviderCodex, ToolBudgetLimit: 4}, adapterTime(t, 201))
	if err != nil {
		t.Fatal(err)
	}
	taskID, _ := kernel.TaskIDFromBytes(adapterID(t, 176))
	incarnationID, _ := kernel.IncarnationIDFromBytes(adapterID(t, 177))
	task, err := fixture.store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID, Title: "detail", Body: "old brief"}, adapterTime(t, 202))
	if err != nil {
		t.Fatal(err)
	}
	fixture.backend.afterTaskDetailTaskRead = func() {
		body := "new brief"
		if _, err := fixture.store.UpdateTask(ctx, task.ID, task.Revision, kernel.TaskPatch{Body: &body}, adapterTime(t, 203)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := fixture.backend.TaskDetail(ctx, rawBrowserClient(fixture.client.ID), browserprotocol.TaskDetailGet{TaskID: task.ID.String(), ExpectedRevision: decimalRevision(task.Revision)}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("mixed task detail = %v", err)
	}
}
