//go:build darwin || linux

package daemon

import (
	"bytes"
	"context"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// adapterSnapshot performs one STATE_GET round trip and returns both the
// decoded snapshot and its exact wire bytes.
func adapterSnapshot(t *testing.T, fixture *adapterFixture, connection *websocket.Conn, id string) (browserprotocol.StateSnapshot, []byte) {
	t.Helper()
	payload, err := browserprotocol.EncodeStateGet(id, browserprotocol.StateGet{})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, payload)
	frame, raw := adapterReadPayload(t, connection)
	if frame.Type != browserprotocol.TypeStateSnapshot || frame.ID != id {
		t.Fatalf("snapshot response = %+v", frame)
	}
	return frame.Body.(browserprotocol.StateSnapshot), raw
}

// A commit that lands between a client's snapshot and its watch registration
// must still be announced. The watcher rereads the durable head as its first
// action, so the change cannot be lost in that gap.
func TestBrowserStateWatchAnnouncesACommitFromTheSnapshotGap(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	connection := fixture.pair(t)
	snapshot, _ := adapterSnapshot(t, fixture, connection, "state-1")

	projectID, err := kernel.ProjectIDFromBytes(adapterID(t, 70))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fixture.store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "gap", Root: "/private/gap"}, adapterTime(t, 400)); err != nil {
		t.Fatal(err)
	}
	state, err := fixture.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	committed := decimalSequence(state.Head)
	if committed <= snapshot.Head {
		t.Fatalf("commit did not advance head: snapshot=%d committed=%d", snapshot.Head, committed)
	}

	watch, err := browserprotocol.EncodeStateWatch("watch-1", browserprotocol.StateWatch{AfterHead: snapshot.Head})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, watch)
	frame := adapterRead(t, connection)
	if frame.Type != browserprotocol.TypeStateChanged || frame.ID != "watch-1" {
		t.Fatalf("watch response = %+v", frame)
	}
	if changed := frame.Body.(browserprotocol.StateChanged); changed.Head != committed {
		t.Fatalf("announced head = %d, want the committed head %d", changed.Head, committed)
	}

	refreshed, _ := adapterSnapshot(t, fixture, connection, "state-2")
	if refreshed.Head != committed || len(refreshed.Projects) != 1 || refreshed.Projects[0].ID != projectID.String() {
		t.Fatalf("refreshed snapshot = %+v", refreshed)
	}
}

// The gap above is closed causally, not by the poll: the producer's very first
// action is a durable-head read, and this proves the announcement is produced
// by that read alone. The second read blocks forever, so no poll iteration can
// contribute the notification.
func TestBrowserStateWatchRereadsTheDurableHeadBeforeAnyWait(t *testing.T) {
	head, err := kernel.NewEventSequence(9)
	if err != nil {
		t.Fatal(err)
	}
	after, err := kernel.NewEventSequence(7)
	if err != nil {
		t.Fatal(err)
	}
	backend := &browserBackend{subs: make(map[*browserStateWatch]struct{})}
	ownerContext, cancel := context.WithCancel(context.Background())
	defer cancel()
	watch := &browserStateWatch{
		backend: backend, notified: after,
		ctx: ownerContext, cancel: cancel, updates: make(chan browser.StateUpdate, browserStateWatchQueue), done: make(chan struct{}),
	}
	backend.subs[watch] = struct{}{}
	reads := make(chan struct{}, 2)
	go watch.runReading(func() (kernel.EventSequence, error) {
		select {
		case reads <- struct{}{}:
			return head, nil
		default:
		}
		<-ownerContext.Done()
		return kernel.EventSequence{}, context.Canceled
	})
	update, ok := <-watch.Updates()
	if !ok || update.Head != decimalSequence(head) {
		t.Fatalf("first update = %+v, ok=%v", update, ok)
	}
	cancel()
	select {
	case <-watch.Done():
	case <-time.After(3 * time.Second):
		t.Fatal("watch did not join after cancellation")
	}
	if len(backend.subs) != 0 {
		t.Fatalf("watch did not deregister: %d remain", len(backend.subs))
	}
}

// Revocation must not return while a state watcher is still running. The
// transport joins its subscription before its connection joins, and revocation
// joins the connection, so an empty subscription set is the positive proof.
func TestBrowserRevocationJoinsTheStateWatcher(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	connection := fixture.pair(t)
	snapshot, _ := adapterSnapshot(t, fixture, connection, "state-1")
	watch, err := browserprotocol.EncodeStateWatch("watch-1", browserprotocol.StateWatch{AfterHead: snapshot.Head})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, watch)
	adapterWaitWatchers(t, fixture.backend, 1)

	client, err := fixture.daemon.RevokeBrowserClient(context.Background(), fixture.client.ID, fixture.client.Revision)
	if err != nil {
		t.Fatalf("revoke = %+v, %v", client, err)
	}
	fixture.backend.subMu.Lock()
	remaining := len(fixture.backend.subs)
	fixture.backend.subMu.Unlock()
	if remaining != 0 {
		t.Fatalf("revocation returned with %d live state watchers", remaining)
	}
	adapterAssertSocketClosed(t, connection)
}

func adapterWaitWatchers(t *testing.T, backend *browserBackend, want int) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		backend.subMu.Lock()
		got := len(backend.subs)
		backend.subMu.Unlock()
		if got == want {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("state watchers = %d, want %d", got, want)
		}
	}
}

// Every private durable column carries a unique sentinel. None may appear in
// the encoded snapshot, which is the exact byte sequence the browser receives.
func TestBrowserStateSnapshotCannotCarryPrivateData(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	connection := fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 100)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 130))
	if _, err := fixture.store.CreateHumanQuestionForAttempt(context.Background(), run.CredentialDigest, kernel.NewHumanQuestion{
		IdempotencyKey: key, QuestionText: "QUESTION_PRIVATE_SENTINEL",
	}, adapterTime(t, 500)); err != nil {
		t.Fatal(err)
	}
	projectID, err := kernel.ProjectIDFromBytes(adapterID(t, 140))
	if err != nil {
		t.Fatal(err)
	}
	agentID, err := kernel.AgentIDFromBytes(adapterID(t, 141))
	if err != nil {
		t.Fatal(err)
	}
	taskID, err := kernel.TaskIDFromBytes(adapterID(t, 142))
	if err != nil {
		t.Fatal(err)
	}
	incarnationID, err := kernel.IncarnationIDFromBytes(adapterID(t, 143))
	if err != nil {
		t.Fatal(err)
	}
	project, err := fixture.store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "public name", Root: "/private/ROOT_PRIVATE_SENTINEL"}, adapterTime(t, 510))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := fixture.store.CreateAgent(context.Background(), kernel.NewAgent{
		ID: agentID, ProjectID: project.ID, Name: "public agent", Role: kernel.RoleWorker, Provider: kernel.ProviderCodex,
		Model: "MODEL_SERVED_FACT", ReasoningEffort: "medium", ToolBudgetLimit: 3,
	}, adapterTime(t, 511))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fixture.store.EnqueueTask(context.Background(), kernel.NewTask{
		ID: taskID, ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID,
		Title: "public title", Body: "BODY_PRIVATE_SENTINEL",
	}, adapterTime(t, 512)); err != nil {
		t.Fatal(err)
	}

	snapshot, raw := adapterSnapshot(t, fixture, connection, "state-1")
	for _, sentinel := range []string{"QUESTION_PRIVATE_SENTINEL", "ROOT_PRIVATE_SENTINEL", "BODY_PRIVATE_SENTINEL"} {
		if bytes.Contains(raw, []byte(sentinel)) {
			t.Fatalf("encoded snapshot carries %q", sentinel)
		}
	}
	if len(snapshot.HumanRequests) != 1 || len(snapshot.Projects) != 2 || len(snapshot.Agents) != 2 || len(snapshot.Tasks) != 2 {
		t.Fatalf("snapshot cardinality = %+v", snapshot)
	}
	if strings.Contains(string(raw), "PRIVATE_SENTINEL") {
		t.Fatalf("encoded snapshot carries a private sentinel: %s", raw)
	}
}

func TestBrowserStateSnapshotIncludesPublicTaskRecency(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	connection := fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 150)
	task, found, err := fixture.store.Task(context.Background(), run.TaskID)
	if err != nil || !found {
		t.Fatal(err)
	}
	snapshot, _ := adapterSnapshot(t, fixture, connection, "recency")
	for _, item := range snapshot.Tasks {
		if item.ID == task.ID.String() {
			if item.UpdatedAtMillis != decimalMillis(task.UpdatedAt) {
				t.Fatalf("task recency = %d, want %d", item.UpdatedAtMillis, task.UpdatedAt)
			}
			return
		}
	}
	t.Fatal("task missing from snapshot")
}

// TASK_ENQUEUE commits the durable task, advances the head, and the very next
// snapshot contains its public card without its private instruction.
func TestBrowserTaskEnqueueAppearsInTheNextSnapshotWithoutItsInstruction(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions)
	projectID, err := kernel.ProjectIDFromBytes(adapterID(t, 150))
	if err != nil {
		t.Fatal(err)
	}
	agentID, err := kernel.AgentIDFromBytes(adapterID(t, 151))
	if err != nil {
		t.Fatal(err)
	}
	taskID, err := kernel.TaskIDFromBytes(adapterID(t, 152))
	if err != nil {
		t.Fatal(err)
	}
	incarnationID, err := kernel.IncarnationIDFromBytes(adapterID(t, 153))
	if err != nil {
		t.Fatal(err)
	}
	project, err := fixture.store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "enqueue", Root: "/private/enqueue"}, adapterTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := fixture.store.CreateAgent(context.Background(), kernel.NewAgent{ID: agentID, ProjectID: project.ID, Name: "idle shell", Role: kernel.RoleWorker, Provider: kernel.ProviderShell, ToolBudgetLimit: 2}, adapterTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	connection := fixture.pair(t)
	before, _ := adapterSnapshot(t, fixture, connection, "state-1")
	if len(before.Tasks) != 0 {
		t.Fatalf("pre-enqueue tasks = %+v", before.Tasks)
	}

	const instruction = "ENQUEUE_PRIVATE_SENTINEL"
	payload, err := browserprotocol.EncodeTaskEnqueue("enqueue", browserprotocol.TaskEnqueue{
		TaskID: taskID.String(), IncarnationID: incarnationID.String(), AgentID: agent.ID.String(),
		ExpectedAgentRevision: decimalRevision(agent.Revision), Instruction: instruction,
	})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, payload)
	frame := adapterRead(t, connection)
	if frame.Type != browserprotocol.TypeTaskEnqueueResult || frame.ID != "enqueue" {
		t.Fatalf("enqueue response = %+v", frame)
	}

	after, raw := adapterSnapshot(t, fixture, connection, "state-2")
	if after.Head <= before.Head {
		t.Fatalf("enqueue did not advance the head: %d -> %d", before.Head, after.Head)
	}
	if len(after.Tasks) != 1 || after.Tasks[0].ID != taskID.String() || after.Tasks[0].Status != "queued" || after.Tasks[0].AssignedAgentID != agent.ID.String() {
		t.Fatalf("post-enqueue tasks = %+v", after.Tasks)
	}
	if bytes.Contains(raw, []byte(instruction)) {
		t.Fatalf("snapshot carries the private instruction: %s", raw)
	}
	stored, found, err := fixture.store.Task(context.Background(), taskID)
	if err != nil || !found || stored.Body != instruction {
		t.Fatalf("durable task = %+v, found=%v, err=%v", stored, found, err)
	}
}
