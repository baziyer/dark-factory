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

func TestBrowserTaskDetailPagesMaximumResultPastTheFormerCursorLimit(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 175)
	result := strings.Repeat("x", 131072)
	completed := completeAdapterRun(t, fixture.store, run, result)
	task, found, err := fixture.store.Task(context.Background(), completed.TaskID)
	if err != nil || !found {
		t.Fatal(err)
	}
	var rebuilt string
	passedFormerLimit := false
	for offset := 0; ; offset += 2048 {
		encoded, err := browserprotocol.EncodeTaskDetailGet("detail", browserprotocol.TaskDetailGet{TaskID: task.ID.String(), ExpectedRevision: decimalRevision(task.Revision), TextOffset: browserprotocol.Decimal(offset)})
		if err != nil {
			t.Fatalf("page %d request = %v", offset, err)
		}
		frame, err := browserprotocol.DecodeClientControl(encoded)
		if err != nil {
			t.Fatalf("page %d decode = %v", offset, err)
		}
		if offset == 34816 {
			passedFormerLimit = true
		}
		detail, err := fixture.backend.TaskDetail(context.Background(), rawBrowserClient(fixture.client.ID), frame.Body.(browserprotocol.TaskDetailGet))
		if err != nil || detail.Outcome == nil {
			t.Fatalf("page %d detail = %+v, %v", offset, detail, err)
		}
		rebuilt += *detail.Outcome
		if _, err := browserprotocol.EncodeTaskDetail("detail", detail); err != nil {
			t.Fatalf("page %d response = %v", offset, err)
		}
		if detail.NextTextOffset == nil {
			break
		}
		if *detail.NextTextOffset != browserprotocol.Decimal(offset+2048) {
			t.Fatalf("page %d continuation = %d", offset, *detail.NextTextOffset)
		}
	}
	if rebuilt != result {
		t.Fatalf("result reconstruction = %d bytes, want %d", len(rebuilt), len(result))
	}
	if !passedFormerLimit {
		t.Fatal("never requested a page past the former 32768-rune cursor limit")
	}
}

func completeAdapterRun(t *testing.T, store *kernel.Store, run kernel.Run, resultText string) kernel.Run {
	t.Helper()
	ctx := context.Background()
	proposal, err := kernel.NewSuccessProposal(resultText)
	if err != nil {
		t.Fatal(err)
	}
	current, err := store.ProposeAttemptOutcome(ctx, run.CredentialDigest, proposal, adapterTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	resources, err := store.Resources(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	var runtime, provider, runner kernel.Resource
	for _, resource := range resources {
		switch resource.Kind {
		case kernel.ResourceRuntimeRoot:
			runtime = resource
		case kernel.ResourceProviderProcess:
			provider = resource
		case kernel.ResourceRunnerProcess:
			runner = resource
		}
	}
	providerExit, err := kernel.NewAttemptResultExitCode(0)
	if err != nil {
		t.Fatal(err)
	}
	attemptResult, err := kernel.NewInnerConvergedAttemptResult(run.ID, run.CredentialDigest, run.ResultProofDigest(), runtime.Identity, provider.Identity, providerExit)
	if err != nil {
		t.Fatal(err)
	}
	current, err = store.ConsumeAttemptResult(ctx, attemptResult, current.Revision, adapterTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	resources, err = store.Resources(ctx, run.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, resource := range resources {
		switch resource.Kind {
		case kernel.ResourceRuntimeRoot:
			runtime = resource
		case kernel.ResourceRunnerProcess:
			runner = resource
		}
	}
	runnerExit, err := kernel.NewProcessExitCode(1, 0, adapterTime(t, 402))
	if err != nil {
		t.Fatal(err)
	}
	current, _, err = store.RecordLiveRunnerExitAndRelease(ctx, run.ID, runner.ID, current.Revision, runner.Revision, runner.Identity, runnerExit, adapterTime(t, 403))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ReleaseResource(ctx, run.ID, runtime.ID, runtime.Revision, runtime.Identity, adapterTime(t, 404)); err != nil {
		t.Fatal(err)
	}
	session, found, err := store.TerminalSessionForRun(ctx, run.ID)
	if err != nil || !found {
		t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	current, _, err = store.CloseTerminalAfterRunner(ctx, attemptResult, current.Revision, session.Revision, adapterTime(t, 405))
	if err != nil {
		t.Fatal(err)
	}
	terminal, err := store.FinalizeRun(ctx, run.ID, current.Revision, adapterTime(t, 406))
	if err != nil {
		t.Fatal(err)
	}
	return terminal
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

func TestBrowserTaskListRequiresPrivateCapabilityAndPagesCompletedWork(t *testing.T) {
	for _, private := range []bool{false, true} {
		caps := kernel.BrowserCapabilityObserve
		if private {
			caps |= kernel.BrowserCapabilityPrivateHumanRequestDetail
		}
		fixture := newAdapterFixture(t, caps)
		fixture.pair(t)
		run := adapterRunningRun(t, fixture.store, 189)
		result, err := fixture.backend.TaskList(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TaskListGet{AgentID: run.AgentID.String()})
		if !private {
			if !errors.Is(err, browser.ErrUnauthorized) {
				t.Fatalf("unprivileged list = %+v %v", result, err)
			}
		} else if err != nil || result.AgentID != run.AgentID.String() || len(result.Tasks) != 0 || result.Total != 0 {
			t.Fatalf("active task leaked to completed list = %+v %v", result, err)
		}
	}
}
