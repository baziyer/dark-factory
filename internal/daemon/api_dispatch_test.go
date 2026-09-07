//go:build darwin || linux

package daemon

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

type dispatchFixture struct {
	daemon   *Daemon
	store    *kernel.Store
	listener *api.Listener
	socket   string
	operator string
}

func newDispatchFixture(t *testing.T) *dispatchFixture {
	t.Helper()
	directory, err := os.MkdirTemp("/private/tmp", "dark-factory-dispatch-")
	if err != nil {
		t.Fatal(err)
	}
	directory, err = filepath.Abs(directory)
	if err != nil {
		_ = os.RemoveAll(directory)
		t.Fatal(err)
	}
	if err := os.Chmod(directory, 0o700); err != nil {
		_ = os.RemoveAll(directory)
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(directory) })
	databasePath := filepath.Join(directory, "kernel.sqlite")
	initial, err := kernel.NewUnixMillis(100)
	if err != nil {
		t.Fatal(err)
	}
	store, err := createTestStore(context.Background(), databasePath, kernel.FactoryConfig{Capacity: 2}, initial)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(1000) })
	if err != nil {
		t.Fatal(err)
	}
	authHomePath := filepath.Join(directory, "home")
	if socket := install.LocalAPISocketPath(authHomePath); len(socket) > install.MaxSocketPathBytes {
		t.Fatalf("api socket path is %d bytes, over the %d-byte budget: %q", len(socket), install.MaxSocketPathBytes, socket)
	}
	if _, err := install.Init(context.Background(), authHomePath); err != nil {
		if errors.Is(err, install.ErrUnsupported) {
			t.Skip("operational local API is unsupported on this platform")
		}
		t.Fatal(err)
	}
	operatorToken := filepath.Join(authHomePath, "operator.token")
	if err := os.WriteFile(operatorToken, bytes.Repeat([]byte{'o'}, 32), 0o600); err != nil {
		t.Fatal(err)
	}
	home, err := install.OpenOperationalHome(context.Background(), authHomePath)
	if err != nil {
		t.Fatal(err)
	}
	authority, err := home.OpenLocalAPI(context.Background())
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	listener, err := api.Listen(authority)
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = listener.Close()
		_ = home.Close()
	})
	socket := install.LocalAPISocketPath(authHomePath)
	return &dispatchFixture{daemon: daemon, store: store, listener: listener, socket: socket, operator: operatorToken}
}

func (fixture *dispatchFixture) serve(t *testing.T) <-chan error {
	t.Helper()
	done := make(chan error, 1)
	go func() {
		connection, err := fixture.listener.Accept()
		if err != nil {
			done <- err
			return
		}
		done <- fixture.daemon.HandleConnection(context.Background(), connection)
	}()
	return done
}

func waitDispatch(t *testing.T, done <-chan error) {
	t.Helper()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("daemon API handler did not finish")
	}
}

func testID(value byte) string {
	return hex.EncodeToString(bytes.Repeat([]byte{value}, kernel.IDBytes))
}

func TestDaemonDispatchesOperatorCallsAndBoundsProjection(t *testing.T) {
	fixture := newDispatchFixture(t)
	client, err := api.NewOperatorClient(fixture.socket, fixture.operator)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()

	done := fixture.serve(t)
	initialSnapshot, err := client.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	waitDispatch(t, done)
	assertNoSchedulerWake(t, fixture.daemon)
	if initialSnapshot.Head != 0 || initialSnapshot.Projects == nil || initialSnapshot.Agents == nil || initialSnapshot.Tasks == nil || len(initialSnapshot.Projects) != 0 || len(initialSnapshot.Agents) != 0 || len(initialSnapshot.Tasks) != 0 {
		t.Fatalf("fresh snapshot = %+v", initialSnapshot)
	}
	done = fixture.serve(t)
	noOp, err := client.SetDispatch(ctx, 1, false)
	if err != nil || noOp.Head != 0 || noOp.Revision != 1 {
		t.Fatalf("head-zero no-op mutation = %+v, %v", noOp, err)
	}
	waitDispatch(t, done)
	assertNoSchedulerWake(t, fixture.daemon)

	done = fixture.serve(t)
	health, err := client.Health(ctx)
	if err != nil || !health.Ready {
		t.Fatalf("health = %+v, %v", health, err)
	}
	waitDispatch(t, done)
	assertNoSchedulerWake(t, fixture.daemon)

	projectInput := api.CreateProjectInput{ID: testID(1), Name: "project", Root: filepath.Join(t.TempDir(), "source-root")}
	done = fixture.serve(t)
	projectResult, err := client.CreateProject(ctx, projectInput)
	if err != nil || projectResult.Revision != 1 || projectResult.Head != 1 {
		t.Fatalf("create project = %+v, %v", projectResult, err)
	}
	waitDispatch(t, done)
	assertNoSchedulerWake(t, fixture.daemon)

	done = fixture.serve(t)
	agentResult, err := client.CreateAgent(ctx, api.CreateAgentInput{
		ID: testID(2), ProjectID: projectInput.ID, Name: "agent", Role: "orchestrator",
		Provider: "codex", Model: "gpt-5.6-luna", ReasoningEffort: "medium", ToolBudgetLimit: 50,
	})
	if err != nil || agentResult.Revision != 1 || agentResult.Head != 2 {
		t.Fatalf("create agent = %+v, %v", agentResult, err)
	}
	waitDispatch(t, done)
	assertNoSchedulerWake(t, fixture.daemon)

	done = fixture.serve(t)
	taskResult, err := client.EnqueueTask(ctx, api.EnqueueTaskInput{
		ID: testID(3), ProjectID: projectInput.ID, AssignedAgentID: testID(2), IncarnationID: testID(4),
		Title: "public title", Body: "private task body sentinel", Priority: 7,
	})
	if err != nil || taskResult.Revision != 1 || taskResult.Head != 3 {
		t.Fatalf("enqueue task = %+v, %v", taskResult, err)
	}
	waitDispatch(t, done)
	assertSchedulerWake(t, fixture.daemon)

	done = fixture.serve(t)
	dispatchResult, err := client.SetDispatch(ctx, 1, true)
	if err != nil || dispatchResult.Revision != 2 || dispatchResult.Head != 4 {
		t.Fatalf("set dispatch = %+v, %v", dispatchResult, err)
	}
	waitDispatch(t, done)
	assertSchedulerWake(t, fixture.daemon)

	done = fixture.serve(t)
	snapshot, err := client.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	waitDispatch(t, done)
	if snapshot.Head != 4 || len(snapshot.Projects) != 1 || len(snapshot.Agents) != 1 || len(snapshot.Tasks) != 1 {
		t.Fatalf("snapshot = %+v", snapshot)
	}
	if snapshot.Projects[0].ID != projectInput.ID || snapshot.Projects[0].Name != projectInput.Name || snapshot.Agents[0].Role != "orchestrator" || snapshot.Tasks[0].Title != "public title" {
		t.Fatalf("projection lost public fields: %+v", snapshot)
	}
	if strings.Contains(snapshot.Tasks[0].Title, "private task body sentinel") {
		t.Fatal("task body crossed public projection")
	}

	project, found, err := fixture.store.Project(ctx, mustProjectID(t, projectInput.ID))
	if err != nil || !found || project.Root != projectInput.Root || project.VerificationPolicy != kernel.VerificationNone {
		t.Fatalf("durable project = %+v, found=%v, err=%v", project, found, err)
	}
	agent, found, err := fixture.store.Agent(ctx, mustAgentID(t, testID(2)))
	if err != nil || !found || agent.Provider != kernel.ProviderCodex || agent.Model != "gpt-5.6-luna" || agent.ReasoningEffort != "medium" {
		t.Fatalf("durable agent = %+v, found=%v, err=%v", agent, found, err)
	}
	task, found, err := fixture.store.Task(ctx, mustTaskID(t, testID(3)))
	if err != nil || !found || task.Body != "private task body sentinel" {
		t.Fatalf("durable task = %+v, found=%v, err=%v", task, found, err)
	}
}

func assertSchedulerWake(t *testing.T, daemon *Daemon) {
	t.Helper()
	select {
	case <-daemon.schedulerWake:
	default:
		t.Fatal("durable runnable mutation did not wake scheduler")
	}
}

func assertNoSchedulerWake(t *testing.T, daemon *Daemon) {
	t.Helper()
	select {
	case <-daemon.schedulerWake:
		t.Fatal("non-runnable mutation woke scheduler")
	default:
	}
}

func TestDaemonDispatchesAttemptOutcomeAfterCommit(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 11)
	ctx := context.Background()
	done := fixture.serve(t)
	result, err := active.client.Succeed(ctx, "private result sentinel")
	if err != nil || result.Revision != uint64(active.run.Revision.Int64()+1) || result.Head != 11 {
		t.Fatalf("attempt succeed = %+v, %v", result, err)
	}
	waitDispatch(t, done)
	finalizing, found, err := fixture.store.Run(ctx, active.run.ID)
	if err != nil || !found || finalizing.Phase != kernel.RunFinalizing || finalizing.Proposal == nil || finalizing.Proposal.Kind() != kernel.OutcomeSucceeded || finalizing.Proposal.Result() != "private result sentinel" {
		t.Fatalf("durable outcome = %+v, found=%v, err=%v", finalizing, found, err)
	}
	done = fixture.serve(t)
	if _, err := active.client.Fail(ctx, "late"); err == nil {
		t.Fatal("revoked attempt credential remained usable")
	}
	waitDispatch(t, done)
}

// A send-back reaches the kernel through both domains with the kernel's own
// refusals mapped to remote codes: an orchestrator may not send back its own
// task, a shell task takes no note, a queued task is a conflict, a task that
// does not exist is not found, a note the provider could not be handed is
// too large, and an unknown bearer learns none of that. The accepting paths
// are the kernel's tests.
func TestDaemonDispatchesSendBackThroughBothDomains(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 51)
	ctx := context.Background()
	operator, err := api.NewOperatorClient(fixture.socket, fixture.operator)
	if err != nil {
		t.Fatal(err)
	}
	own := active.run.TaskID.String()
	var remote *api.RemoteError
	done := fixture.serve(t)
	if _, err := active.client.SendBack(ctx, api.SendBackInput{TaskID: own, Note: "myself"}); !errors.As(err, &remote) || remote.Code() != api.RemoteConflict {
		t.Fatalf("orchestrator sending back its own task = %v", err)
	}
	waitDispatch(t, done)
	// The fixture's orchestrator is a shell agent: its task is a program and
	// takes no note, whatever its status.
	done = fixture.serve(t)
	if _, err := operator.SendBackTask(ctx, api.SendBackInput{TaskID: own, Note: "still running"}); !errors.As(err, &remote) || remote.Code() != api.RemoteInvalidRequest {
		t.Fatalf("operator sending back a shell task = %v", err)
	}
	waitDispatch(t, done)
	done = fixture.serve(t)
	if _, err := operator.SendBackTask(ctx, api.SendBackInput{TaskID: testID(99), Note: "nobody"}); !errors.As(err, &remote) || remote.Code() != api.RemoteNotFound {
		t.Fatalf("operator sending back a missing task = %v", err)
	}
	waitDispatch(t, done)
	if _, err := active.client.SendBack(ctx, api.SendBackInput{TaskID: own, Note: ""}); !errors.Is(err, api.ErrInvalidInput) {
		t.Fatalf("empty note left the client = %v", err)
	}
	// A note the task's provider could not be handed is too large, whatever
	// the kernel would have said about the task.
	claude, claudeTask := testID(61), testID(62)
	done = fixture.serve(t)
	if _, err := operator.CreateAgent(ctx, api.CreateAgentInput{ID: claude, ProjectID: active.run.ProjectID.String(), Name: "typist", Role: "worker", Provider: "claude_code", ToolBudgetLimit: 1}); err != nil {
		t.Fatal(err)
	}
	waitDispatch(t, done)
	done = fixture.serve(t)
	if _, err := operator.EnqueueTask(ctx, api.EnqueueTaskInput{ID: claudeTask, ProjectID: active.run.ProjectID.String(), AssignedAgentID: claude, IncarnationID: testID(63), Title: "typed", Body: strings.Repeat("x", 7000)}); err != nil {
		t.Fatal(err)
	}
	waitDispatch(t, done)
	done = fixture.serve(t)
	if _, err := operator.SendBackTask(ctx, api.SendBackInput{TaskID: claudeTask, Note: strings.Repeat("&", 1024)}); !errors.As(err, &remote) || remote.Code() != api.RemoteTooLarge {
		t.Fatalf("a note past the provider's prompt = %v", err)
	}
	waitDispatch(t, done)
	// A note that fits reaches the kernel, which refuses the queued task.
	done = fixture.serve(t)
	if _, err := operator.SendBackTask(ctx, api.SendBackInput{TaskID: claudeTask, Note: "fits"}); !errors.As(err, &remote) || remote.Code() != api.RemoteConflict {
		t.Fatalf("operator sending back a queued task = %v", err)
	}
	waitDispatch(t, done)
	// A credential the kernel would refuse learns nothing: an unknown bearer
	// naming a missing task is unauthorized, never not found.
	wrongToken := filepath.Join(filepath.Dir(fixture.socket), "wrong-send-back.token")
	if err := os.WriteFile(wrongToken, bytes.Repeat([]byte{'y'}, 32), 0o600); err != nil {
		t.Fatal(err)
	}
	activeToken := os.Getenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE")
	if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", wrongToken); err != nil {
		t.Fatal(err)
	}
	wrong, err := api.NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", activeToken); err != nil {
		t.Fatal(err)
	}
	done = fixture.serve(t)
	if _, err := wrong.SendBack(ctx, api.SendBackInput{TaskID: testID(98), Note: "who is there"}); !errors.As(err, &remote) || remote.Code() != api.RemoteUnauthorized {
		t.Fatalf("unknown bearer naming a missing task = %v", err)
	}
	waitDispatch(t, done)
	task, found, err := fixture.store.Task(ctx, active.run.TaskID)
	if err != nil || !found || task.Status != kernel.TaskRunning || task.WorkRevision.Int64() != 1 {
		t.Fatalf("task after refused send-backs = %+v, found=%v, %v", task, found, err)
	}
}

func TestDaemonDispatchesHumanQuestionWithDurableIdempotency(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 31)
	ctx := context.Background()
	input := api.HumanQuestionInput{IdempotencyKey: testID(41), Question: "private human question sentinel"}
	before, err := fixture.store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}

	done := fixture.serve(t)
	created, err := active.client.RequestHuman(ctx, input)
	if err != nil || created.Revision != 1 || created.Head != uint64(before.Head.Int64()+1) {
		t.Fatalf("request human = %+v, %v", created, err)
	}
	waitDispatch(t, done)
	requests, err := fixture.store.Snapshot(ctx)
	if err != nil || len(requests.HumanRequests) != 1 {
		t.Fatalf("durable human requests = %+v, %v", requests.HumanRequests, err)
	}
	request := requests.HumanRequests[0]
	if request.Status != kernel.HumanRequestOpen {
		t.Fatalf("durable human request = %+v", request)
	}
	encoded, err := json.Marshal(created)
	if err != nil || bytes.Contains(encoded, []byte(input.Question)) || bytes.Contains(encoded, []byte(request.ID.String())) {
		t.Fatalf("mutation response exposed private request data: %s, %v", encoded, err)
	}

	beforeReplay, err := fixture.store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	done = fixture.serve(t)
	replay, err := active.client.RequestHuman(ctx, input)
	if err != nil || replay.Revision != created.Revision || replay.Head != uint64(beforeReplay.Head.Int64()) {
		t.Fatalf("idempotent request human = %+v, %v", replay, err)
	}
	waitDispatch(t, done)
	afterReplay, err := fixture.store.Snapshot(ctx)
	if err != nil || len(afterReplay.HumanRequests) != 1 || afterReplay.HumanRequests[0].ID != request.ID {
		t.Fatalf("idempotent durable human requests = %+v, %v", afterReplay.HumanRequests, err)
	}

	conflicting := input
	conflicting.Question = "different private question"
	done = fixture.serve(t)
	if _, err := active.client.RequestHuman(ctx, conflicting); err == nil {
		t.Fatal("same human request key with different question succeeded")
	} else {
		var remote *api.RemoteError
		if !errors.As(err, &remote) || remote.Code() != api.RemoteConflict {
			t.Fatalf("conflicting human request error = %v", err)
		}
	}
	waitDispatch(t, done)
	afterConflict, err := fixture.store.Snapshot(ctx)
	if err != nil || len(afterConflict.HumanRequests) != 1 || afterConflict.HumanRequests[0].ID != request.ID {
		t.Fatalf("conflicting durable human requests = %+v, %v", afterConflict.HumanRequests, err)
	}
}

func TestDaemonRejectsForgedAndFinalizingHumanQuestion(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 51)
	ctx := context.Background()
	wrongToken := filepath.Join(filepath.Dir(fixture.socket), "wrong-human.token")
	if err := os.WriteFile(wrongToken, bytes.Repeat([]byte{'z'}, 32), 0o600); err != nil {
		t.Fatal(err)
	}
	activeToken := os.Getenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE")
	if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", wrongToken); err != nil {
		t.Fatal(err)
	}
	wrong, err := api.NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", activeToken); err != nil {
		t.Fatal(err)
	}

	done := fixture.serve(t)
	if _, err := wrong.RequestHuman(ctx, api.HumanQuestionInput{IdempotencyKey: testID(61), Question: "forged question"}); err == nil {
		t.Fatal("forged human request succeeded")
	} else {
		var remote *api.RemoteError
		if !errors.As(err, &remote) || remote.Code() != api.RemoteUnauthorized {
			t.Fatalf("forged human request error = %v", err)
		}
	}
	waitDispatch(t, done)

	done = fixture.serve(t)
	if _, err := active.client.Succeed(ctx, "finalize attempt"); err != nil {
		t.Fatalf("finalize attempt = %v", err)
	}
	waitDispatch(t, done)

	done = fixture.serve(t)
	if _, err := active.client.RequestHuman(ctx, api.HumanQuestionInput{IdempotencyKey: testID(62), Question: "finalizing question"}); err == nil {
		t.Fatal("finalizing human request succeeded")
	} else {
		var remote *api.RemoteError
		if !errors.As(err, &remote) || remote.Code() != api.RemoteUnauthorized {
			t.Fatalf("finalizing human request error = %v", err)
		}
	}
	waitDispatch(t, done)
	requests, err := fixture.store.Snapshot(ctx)
	if err != nil || len(requests.HumanRequests) != 0 {
		t.Fatalf("unauthorized human requests became durable = %+v, %v", requests.HumanRequests, err)
	}
}

type activeAttempt struct {
	client *api.AttemptClient
	run    kernel.Run
	bearer []byte
}

func prepareActiveAttempt(t *testing.T, fixture *dispatchFixture, seed byte) activeAttempt {
	t.Helper()
	ctx := context.Background()
	projectID, agentID, taskID, incarnationID := testID(seed), testID(seed+1), testID(seed+2), testID(seed+3)
	operator, err := api.NewOperatorClient(fixture.socket, fixture.operator)
	if err != nil {
		t.Fatal(err)
	}
	call := func(invoke func() error) {
		done := fixture.serve(t)
		if err := invoke(); err != nil {
			t.Fatal(err)
		}
		waitDispatch(t, done)
	}
	call(func() error {
		_, err := operator.CreateProject(ctx, api.CreateProjectInput{ID: projectID, Name: "project", Root: filepath.Join(filepath.Dir(fixture.socket), "source-root")})
		return err
	})
	call(func() error {
		_, err := operator.CreateAgent(ctx, api.CreateAgentInput{ID: agentID, ProjectID: projectID, Name: "agent", Role: "orchestrator", Provider: "shell", ToolBudgetLimit: 10})
		return err
	})
	call(func() error {
		_, err := operator.EnqueueTask(ctx, api.EnqueueTaskInput{ID: taskID, ProjectID: projectID, AssignedAgentID: agentID, IncarnationID: incarnationID, Title: "task", Body: "private", Priority: 1})
		return err
	})
	call(func() error {
		_, err := operator.SetDispatch(ctx, 1, true)
		return err
	})
	bearer := bytes.Repeat([]byte{seed}, 32)
	digestBytes := sha256.Sum256(bearer)
	digest, err := kernel.AttemptDigestFromBytes(digestBytes[:])
	if err != nil {
		t.Fatal(err)
	}
	rawCandidate, err := hex.DecodeString(testID(seed + 9))
	if err != nil {
		t.Fatal(err)
	}
	candidateChange, err := kernel.ChangeIDFromBytes(rawCandidate)
	if err != nil {
		t.Fatal(err)
	}
	keys := kernel.AdmissionKeys{
		RunID: mustRunID(t, testID(seed+4)), TerminalSessionID: mustTerminalSessionID(t, testID(seed+14)), AttemptDigest: digest, CandidateChangeID: candidateChange,
		RuntimeRoot: filepath.Join(filepath.Dir(fixture.socket), "runtime"),
		Resources: kernel.AdmissionResourceIDs{
			RuntimeRoot: mustResourceID(t, testID(seed+5)), RunnerProcess: mustResourceID(t, testID(seed+6)),
			ProviderProcess: mustResourceID(t, testID(seed+7)), ProviderGroup: mustResourceID(t, testID(seed+8)),
		},
	}
	at := mustKernelTime(t, 1000)
	admission, err := fixture.store.AdmitNext(ctx, keys, at)
	if err != nil || !admission.Admitted() {
		t.Fatalf("admission = %+v, %v", admission, err)
	}
	run := admission.Run
	pathIdentity, err := kernel.NewPathResourceIdentity(1, int64(seed)+100)
	if err != nil {
		t.Fatal(err)
	}
	activate := func(id kernel.ResourceID, identity kernel.ResourceIdentity) {
		resource, found, readErr := fixture.store.Resource(ctx, id)
		if readErr != nil || !found {
			t.Fatalf("resource %s = %+v, found=%v, err=%v", id, resource, found, readErr)
		}
		if _, err := fixture.store.ActivateResource(ctx, run.ID, id, resource.Revision, identity, at); err != nil {
			t.Fatal(err)
		}
	}
	birthOne, _ := kernel.BirthDigestFromBytes(bytes.Repeat([]byte{seed + 1}, 32))
	birthTwo, _ := kernel.BirthDigestFromBytes(bytes.Repeat([]byte{seed + 2}, 32))
	processOne, _ := kernel.NewProcessResourceIdentity(int64(seed)+10, int64(seed)+11, birthOne)
	processTwo, _ := kernel.NewProcessResourceIdentity(int64(seed)+12, int64(seed)+13, birthTwo)
	activate(keys.Resources.RuntimeRoot, pathIdentity)
	run2 := startAndActivateRunner(t, fixture.store, run.ID, keys.Resources.RunnerProcess, processOne, at)
	providerProcess, found, err := fixture.store.Resource(ctx, keys.Resources.ProviderProcess)
	if err != nil || !found {
		t.Fatalf("provider process = %+v, found=%v, err=%v", providerProcess, found, err)
	}
	providerGroup, found, err := fixture.store.Resource(ctx, keys.Resources.ProviderGroup)
	if err != nil || !found {
		t.Fatalf("provider group = %+v, found=%v, err=%v", providerGroup, found, err)
	}
	if _, _, err := fixture.store.ActivateProviderResources(ctx, run.ID, providerProcess.ID, providerProcess.Revision, providerGroup.ID, providerGroup.Revision, processTwo, at); err != nil {
		t.Fatal(err)
	}
	session, found, err := fixture.store.TerminalSessionForRun(ctx, run.ID)
	if err != nil || !found {
		t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	active, err := fixture.store.ActivateRun(ctx, run.ID, session.ID, run2.Revision, session.Revision, at)
	if err != nil {
		t.Fatal(err)
	}
	attemptToken := filepath.Join(filepath.Dir(fixture.socket), "attempt.token")
	if err := os.WriteFile(attemptToken, bearer, 0o600); err != nil {
		t.Fatal(err)
	}
	previous := os.Getenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE")
	if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", attemptToken); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", previous) })
	client, err := api.NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	return activeAttempt{client: client, run: active, bearer: append([]byte(nil), bearer...)}
}

func TestDaemonDispatchesBlockAndFailCalls(t *testing.T) {
	for _, test := range []struct {
		name string
		call func(context.Context, *api.AttemptClient) (api.MutationResult, error)
		kind kernel.OutcomeKind
	}{
		{name: "block", call: func(ctx context.Context, client *api.AttemptClient) (api.MutationResult, error) {
			return client.Block(ctx, "needs operator")
		}, kind: kernel.OutcomeBlocked},
		{name: "fail empty detail", call: func(ctx context.Context, client *api.AttemptClient) (api.MutationResult, error) {
			return client.Fail(ctx, "")
		}, kind: kernel.OutcomeFailed},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newDispatchFixture(t)
			active := prepareActiveAttempt(t, fixture, byte(21+len(test.name)))
			done := fixture.serve(t)
			result, err := test.call(context.Background(), active.client)
			if err != nil || result.Revision != uint64(active.run.Revision.Int64()+1) || result.Head != 11 {
				t.Fatalf("outcome = %+v, %v", result, err)
			}
			waitDispatch(t, done)
			observed, found, err := fixture.store.Run(context.Background(), active.run.ID)
			if err != nil || !found || observed.Phase != kernel.RunFinalizing || observed.Proposal == nil || observed.Proposal.Kind() != test.kind {
				t.Fatalf("durable proposal = %+v, found=%v, err=%v", observed, found, err)
			}
		})
	}
}

func TestDaemonServesTaskOnlyToLiveAttempt(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 51)
	done := fixture.serve(t)
	task, err := active.client.Task(context.Background())
	if err != nil || task.Task != "private" {
		t.Fatalf("attempt task = %+v, %v", task, err)
	}
	waitDispatch(t, done)

	done = fixture.serve(t)
	if _, err := active.client.Succeed(context.Background(), "done"); err != nil {
		t.Fatal(err)
	}
	waitDispatch(t, done)

	done = fixture.serve(t)
	if _, err := active.client.Task(context.Background()); err == nil {
		t.Fatal("finalizing attempt read task")
	} else {
		var remote *api.RemoteError
		if !errors.As(err, &remote) || remote.Code() != api.RemoteUnauthorized {
			t.Fatalf("finalizing task error = %v", err)
		}
	}
	waitDispatch(t, done)
}

func TestDaemonRejectsForgedAttemptOutcome(t *testing.T) {
	fixture := newDispatchFixture(t)
	_ = prepareActiveAttempt(t, fixture, 61)
	wrongBearer := bytes.Repeat([]byte{'z'}, 32)
	wrongToken := filepath.Join(filepath.Dir(fixture.socket), "wrong.token")
	if err := os.WriteFile(wrongToken, wrongBearer, 0o600); err != nil {
		t.Fatal(err)
	}
	previous := os.Getenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE")
	if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", wrongToken); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", previous) })
	wrong, err := api.NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	done := fixture.serve(t)
	if _, err := wrong.Succeed(context.Background(), "forged"); err == nil {
		t.Fatal("forged attempt outcome succeeded")
	}
	waitDispatch(t, done)
}

func TestDaemonConcurrentAttemptOutcomesHaveOneDurableWinner(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 71)
	type outcomeResult struct {
		result api.MutationResult
		err    error
	}
	firstDone := fixture.serve(t)
	secondDone := fixture.serve(t)
	results := make(chan outcomeResult, 2)
	var group sync.WaitGroup
	group.Add(2)
	go func() {
		defer group.Done()
		result, err := active.client.Succeed(context.Background(), "winner")
		results <- outcomeResult{result: result, err: err}
	}()
	go func() {
		defer group.Done()
		result, err := active.client.Block(context.Background(), "loser")
		results <- outcomeResult{result: result, err: err}
	}()
	group.Wait()
	waitDispatch(t, firstDone)
	waitDispatch(t, secondDone)
	var accepted, rejected int
	for range 2 {
		outcome := <-results
		if outcome.err == nil {
			accepted++
			if outcome.result.Revision == 0 {
				t.Fatal("accepted outcome omitted revision")
			}
			continue
		}
		rejected++
		var remote *api.RemoteError
		if !errors.As(outcome.err, &remote) || remote.Code() != api.RemoteUnauthorized {
			t.Fatalf("losing outcome error = %v", outcome.err)
		}
	}
	if accepted != 1 || rejected != 1 {
		t.Fatalf("outcome counts accepted=%d rejected=%d", accepted, rejected)
	}
	observed, found, err := fixture.store.Run(context.Background(), active.run.ID)
	if err != nil || !found || observed.Phase != kernel.RunFinalizing || observed.Proposal == nil {
		t.Fatalf("concurrent durable result = %+v, found=%v, err=%v", observed, found, err)
	}
}

func TestProjectionHasNoPrivateFieldsAndKeepsEmptySlices(t *testing.T) {
	projectID := mustProjectID(t, testID(51))
	agentID := mustAgentID(t, testID(52))
	taskID := mustTaskID(t, testID(53))
	revision := mustRevision(t, 3)
	head, err := kernel.NewEventSequence(0)
	if err != nil {
		t.Fatal(err)
	}
	projected := projectSnapshot(kernel.DashboardSnapshot{
		Head:     head,
		Factory:  kernel.FactorySummary{Capacity: 2, Revision: revision},
		Projects: []kernel.ProjectSummary{{ID: projectID, Name: "project", Revision: revision}},
		Agents:   []kernel.AgentSummary{{ID: agentID, ProjectID: projectID, Name: "agent", Role: "worker", Provider: "codex", Revision: revision}},
		Tasks:    []kernel.TaskSummary{{ID: taskID, ProjectID: projectID, AssignedAgentID: agentID, Title: "title", Status: "queued", Priority: 3, Revision: revision}},
	})
	if projected.Head != 0 || projected.Projects == nil || projected.Agents == nil || projected.Tasks == nil {
		t.Fatalf("projection emptiness/head = %+v", projected)
	}
	if projected.Projects[0].Name != "project" || projected.Agents[0].Provider != "codex" || projected.Tasks[0].Title != "title" {
		t.Fatalf("projection fields = %+v", projected)
	}
}

func TestRemoteErrorMappingNeverUsesPrivateStoreText(t *testing.T) {
	tests := []struct {
		err  error
		code api.RemoteErrorCode
	}{
		{fmt.Errorf("private body: %w", kernel.ErrInvalidValue), api.RemoteInvalidRequest},
		{fmt.Errorf("private token: %w", kernel.ErrUnauthorized), api.RemoteUnauthorized},
		{fmt.Errorf("private ID: %w", kernel.ErrNotFound), api.RemoteNotFound},
		{fmt.Errorf("private revision: %w", kernel.ErrRevisionConflict), api.RemoteRevisionConflict},
		{fmt.Errorf("private state: %w", kernel.ErrConflict), api.RemoteConflict},
		{fmt.Errorf("private snapshot: %w", kernel.ErrSnapshotTooLarge), api.RemoteTooLarge},
		{fmt.Errorf("private busy: %w", kernel.ErrBusy), api.RemoteUnavailable},
		{errors.New("private unexpected failure"), api.RemoteInternal},
	}
	for _, test := range tests {
		if got := remoteErrorCode(test.err); got != test.code {
			t.Fatalf("remote code = %q, want %q", got, test.code)
		}
		if strings.Contains(newErrorReply(test.code).String(), "private") {
			t.Fatal("private error text entered reply")
		}
	}
}

func mustKernelTime(t *testing.T, value int64) kernel.UnixMillis {
	t.Helper()
	at, err := kernel.NewUnixMillis(value)
	if err != nil {
		t.Fatal(err)
	}
	return at
}

func mustRevision(t *testing.T, value int64) kernel.Revision {
	t.Helper()
	revision, err := kernel.NewRevision(value)
	if err != nil {
		t.Fatal(err)
	}
	return revision
}

func mustProjectID(t *testing.T, value string) kernel.ProjectID {
	t.Helper()
	id, err := kernel.ProjectIDFromBytes(mustIDBytes(t, value))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func mustAgentID(t *testing.T, value string) kernel.AgentID {
	t.Helper()
	id, err := kernel.AgentIDFromBytes(mustIDBytes(t, value))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func mustTaskID(t *testing.T, value string) kernel.TaskID {
	t.Helper()
	id, err := kernel.TaskIDFromBytes(mustIDBytes(t, value))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func mustRunID(t *testing.T, value string) kernel.RunID {
	t.Helper()
	id, err := kernel.RunIDFromBytes(mustIDBytes(t, value))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func mustResourceID(t *testing.T, value string) kernel.ResourceID {
	t.Helper()
	id, err := kernel.ResourceIDFromBytes(mustIDBytes(t, value))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func mustTerminalSessionID(t *testing.T, value string) kernel.TerminalSessionID {
	t.Helper()
	id, err := kernel.TerminalSessionIDFromBytes(mustIDBytes(t, value))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func mustIDBytes(t *testing.T, value string) []byte {
	t.Helper()
	decoded, err := hex.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	return decoded
}
