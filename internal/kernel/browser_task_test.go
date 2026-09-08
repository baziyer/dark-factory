package kernel

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"
)

func TestEnqueueTaskForBrowserAgentBindsAuthorityAndKeepsInstructionPrivate(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	client := terminalTargetClient(t, store, browserTestID(t, 180), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	task := taskID(t, 181)
	incarnation := incarnationID(t, 182)
	instruction := "PRIVATE_DIRECT_INSTRUCTION_SENTINEL"
	before, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}

	created, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, task, incarnation, agent.ID, agent.Revision, instruction, mustTime(t, 102))
	if err != nil {
		t.Fatal(err)
	}
	if created.AgentRevision != agent.Revision {
		t.Fatalf("agent revision = %d, want %d", created.AgentRevision.Int64(), agent.Revision.Int64())
	}
	got := created.Task
	if got.ID != task || got.ProjectID != project.ID || got.AssignedAgentID != agent.ID || got.IncarnationID != incarnation || got.Title != "Direct instruction" || got.Body != instruction || got.Priority != 0 || got.Status != TaskQueued || got.Revision.Int64() != 1 {
		t.Fatalf("created task = %+v", got)
	}

	snapshot, err := store.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Tasks) != 1 {
		t.Fatalf("public tasks = %+v", snapshot.Tasks)
	}
	public := snapshot.Tasks[0]
	if public.ID != task || public.ProjectID != project.ID || public.AssignedAgentID != agent.ID || public.Title != "Direct instruction" || public.Status != "queued" || public.Priority != 0 || public.Revision.Int64() != 1 {
		t.Fatalf("public task = %+v", public)
	}
	encoded, err := json.Marshal(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), instruction) {
		t.Fatalf("public state leaked private instruction: %s", encoded)
	}

	invalidations := invalidationsAfter(t, store, before.Head)
	if len(invalidations) != 1 || invalidations[0].EntityKind != EntityTask.String() || invalidations[0].EntityID != task.String() || invalidations[0].Revision.Int64() != 1 || invalidations[0].Deleted {
		t.Fatalf("enqueue invalidations = %+v", invalidations)
	}
	enqueued, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}

	replayed, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, task, incarnation, agent.ID, agent.Revision, instruction, mustTime(t, 103))
	if err != nil {
		t.Fatalf("exact replay: %v", err)
	}
	if replayed.Task != got || replayed.AgentRevision != agent.Revision {
		t.Fatalf("replay = %+v, want %+v", replayed, created)
	}
	afterReplay, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if afterReplay.Head != enqueued.Head {
		t.Fatalf("replay advanced head from %d to %d", enqueued.Head.Int64(), afterReplay.Head.Int64())
	}

	if _, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, task, incarnation, agent.ID, agent.Revision, "different instruction", mustTime(t, 104)); !errors.Is(err, ErrConflict) {
		t.Fatalf("conflicting task replay = %v, want conflict", err)
	}
	afterConflict, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if afterConflict.Head != afterReplay.Head {
		t.Fatalf("conflict advanced head from %d to %d", afterReplay.Head.Int64(), afterConflict.Head.Int64())
	}
}

func TestEnqueueTaskForBrowserAgentRejectsUnauthorizedStaleAndPaused(t *testing.T) {
	tests := []struct {
		name       string
		capability BrowserCapabilityMask
		mutate     func(*testing.T, *Store, Agent) Agent
		expected   func(Agent) Revision
		want       error
	}{
		{
			name:       "missing human-actions capability",
			capability: BrowserCapabilityObserve,
			expected:   func(agent Agent) Revision { return agent.Revision },
			want:       ErrUnauthorized,
		},
		{
			name:       "stale agent revision",
			capability: BrowserCapabilityObserve | BrowserCapabilityHumanActions,
			expected:   func(agent Agent) Revision { return mustRevision(t, agent.Revision.Int64()+1) },
			want:       ErrRevisionConflict,
		},
		{
			name:       "paused agent",
			capability: BrowserCapabilityObserve | BrowserCapabilityHumanActions,
			mutate:     pauseBrowserTaskAgent,
			expected:   func(agent Agent) Revision { return agent.Revision },
			want:       ErrRevisionConflict,
		},
	}
	for index, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
			defer store.Close()
			ctx := context.Background()
			client := terminalTargetClient(t, store, browserTestID(t, byte(190+index*2)), test.capability)
			if test.mutate != nil {
				agent = test.mutate(t, store, agent)
			}
			before, err := store.Factory(ctx)
			if err != nil {
				t.Fatal(err)
			}
			task := taskID(t, byte(191+index*2))
			if _, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, task, incarnationID(t, byte(201+index)), agent.ID, test.expected(agent), "do the work", mustTime(t, 200)); !errors.Is(err, test.want) {
				t.Fatalf("enqueue error = %v, want %v", err, test.want)
			}
			if _, found, err := store.Task(ctx, task); err != nil || found {
				t.Fatalf("rejected enqueue task found=%v err=%v", found, err)
			}
			after, err := store.Factory(ctx)
			if err != nil {
				t.Fatal(err)
			}
			if after.Head != before.Head {
				t.Fatalf("rejected enqueue advanced head from %d to %d", before.Head.Int64(), after.Head.Int64())
			}
		})
	}
}

func TestEnqueueTaskForBrowserAgentRejectsInvalidInstruction(t *testing.T) {
	store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	client := terminalTargetClient(t, store, browserTestID(t, 210), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	for index, instruction := range []string{"", " \t\n ", string([]byte{0xff}), strings.Repeat("x", 32769)} {
		if _, err := store.EnqueueTaskForBrowserAgent(context.Background(), client.ID, taskID(t, byte(211+index)), incarnationID(t, byte(220+index)), agent.ID, agent.Revision, instruction, mustTime(t, 300)); !errors.Is(err, ErrInvalidValue) {
			t.Fatalf("instruction %d error = %v, want invalid value", index, err)
		}
	}
}

func TestEnqueueTaskForBrowserAgentRequiresIdleAgent(t *testing.T) {
	t.Run("queued task permits only its exact creation replay", func(t *testing.T) {
		store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
		defer store.Close()
		ctx := context.Background()
		client := terminalTargetClient(t, store, browserTestID(t, 220), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
		firstTask := taskID(t, 221)
		firstIncarnation := incarnationID(t, 222)
		created, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, firstTask, firstIncarnation, agent.ID, agent.Revision, "first instruction", mustTime(t, 102))
		if err != nil {
			t.Fatal(err)
		}
		replayed, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, firstTask, firstIncarnation, agent.ID, agent.Revision, "first instruction", mustTime(t, 103))
		if err != nil || replayed.Task != created.Task || replayed.AgentRevision != created.AgentRevision {
			t.Fatalf("exact queued replay = %+v, err=%v", replayed, err)
		}
		beforeRejected, err := store.Factory(ctx)
		if err != nil {
			t.Fatal(err)
		}
		secondTask := taskID(t, 223)
		if _, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, secondTask, incarnationID(t, 224), agent.ID, agent.Revision, "second instruction", mustTime(t, 104)); !errors.Is(err, ErrRevisionConflict) {
			t.Fatalf("enqueue beside queued task = %v, want revision conflict", err)
		}
		if _, found, err := store.Task(ctx, secondTask); err != nil || found {
			t.Fatalf("second queued task found=%v err=%v", found, err)
		}
		afterRejected, err := store.Factory(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if afterRejected.Head != beforeRejected.Head {
			t.Fatalf("queued rejection advanced head from %d to %d", beforeRejected.Head.Int64(), afterRejected.Head.Int64())
		}
	})

	t.Run("running task rejects another instruction", func(t *testing.T) {
		store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
		defer store.Close()
		ctx := context.Background()
		client := terminalTargetClient(t, store, browserTestID(t, 225), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
		firstTask := taskID(t, 226)
		if _, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, firstTask, incarnationID(t, 227), agent.ID, agent.Revision, "running instruction", mustTime(t, 102)); err != nil {
			t.Fatal(err)
		}
		admitted, err := store.AdmitNext(ctx, admissionKeys(t, 228, nil), mustTime(t, 103))
		if err != nil || !admitted.Admitted() || admitted.Run.TaskID != firstTask {
			t.Fatalf("admission = %+v, err=%v", admitted, err)
		}
		beforeRejected, err := store.Factory(ctx)
		if err != nil {
			t.Fatal(err)
		}
		secondTask := taskID(t, 229)
		if _, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, secondTask, incarnationID(t, 230), agent.ID, agent.Revision, "second instruction", mustTime(t, 104)); !errors.Is(err, ErrRevisionConflict) {
			t.Fatalf("enqueue beside running task = %v, want revision conflict", err)
		}
		if _, found, err := store.Task(ctx, secondTask); err != nil || found {
			t.Fatalf("second running task found=%v err=%v", found, err)
		}
		afterRejected, err := store.Factory(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if afterRejected.Head != beforeRejected.Head {
			t.Fatalf("running rejection advanced head from %d to %d", beforeRejected.Head.Int64(), afterRejected.Head.Int64())
		}
	})
}

func TestEnqueueTaskForBrowserAgentConcurrentTabsCreateExactlyOneTask(t *testing.T) {
	store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	client := terminalTargetClient(t, store, browserTestID(t, 231), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	taskIDs := []TaskID{taskID(t, 232), taskID(t, 234)}
	incarnationIDs := []IncarnationID{incarnationID(t, 233), incarnationID(t, 235)}
	timestamps := []UnixMillis{mustTime(t, 102), mustTime(t, 103)}
	type outcome struct {
		id  TaskID
		err error
	}
	started := make(chan struct{})
	finished := make(chan outcome, len(taskIDs))
	for index := range taskIDs {
		index := index
		go func() {
			<-started
			_, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, taskIDs[index], incarnationIDs[index], agent.ID, agent.Revision, "tab instruction", timestamps[index])
			finished <- outcome{id: taskIDs[index], err: err}
		}()
	}
	close(started)
	var succeeded TaskID
	successes, conflicts := 0, 0
	for range taskIDs {
		result := <-finished
		switch {
		case result.err == nil:
			successes++
			succeeded = result.id
		case errors.Is(result.err, ErrRevisionConflict):
			conflicts++
		default:
			t.Fatalf("concurrent enqueue %s = %v", result.id.String(), result.err)
		}
	}
	if successes != 1 || conflicts != 1 {
		t.Fatalf("concurrent outcomes successes=%d conflicts=%d", successes, conflicts)
	}
	snapshot, err := store.Snapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Tasks) != 1 || snapshot.Tasks[0].ID != succeeded {
		t.Fatalf("durable concurrent tasks = %+v, successful=%s", snapshot.Tasks, succeeded.String())
	}
}

func pauseBrowserTaskAgent(t *testing.T, store *Store, agent Agent) Agent {
	t.Helper()
	ctx := context.Background()
	tx, err := store.beginValidatedWrite(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Close()
	next := mustRevision(t, agent.Revision.Int64()+1)
	result, err := tx.connection.ExecContext(ctx, `UPDATE agents SET paused = 1, revision = ?, updated_at_ms = ? WHERE id = ? AND revision = ?`, next.Int64(), 120, agent.ID.Bytes(), agent.Revision.Int64())
	if err != nil {
		t.Fatal(tx.Rollback(err))
	}
	if affected, err := result.RowsAffected(); err != nil || affected != 1 {
		t.Fatalf("pause rows = %d, err=%v", affected, err)
	}
	if err := appendInvalidations(ctx, tx.connection, mustTime(t, 120), []pendingInvalidation{{kind: EntityAgent, id: agent.ID.Bytes(), revision: next.Int64()}}); err != nil {
		t.Fatal(tx.Rollback(err))
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	updated, found, err := store.Agent(ctx, agent.ID)
	if err != nil || !found || !updated.Paused || updated.Revision != next {
		t.Fatalf("paused agent = %+v, found=%v, err=%v", updated, found, err)
	}
	return updated
}

func TestBrowserQueueAcceptsBusyPausedAgentWithoutChangingCurrentWork(t *testing.T) {
	store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	client := terminalTargetClient(t, store, browserTestID(t, 180), BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	first, err := store.EnqueueTaskForBrowserAgent(ctx, client.ID, taskID(t, 181), incarnationID(t, 182), agent.ID, agent.Revision, "first", mustTime(t, 102))
	if err != nil {
		t.Fatal(err)
	}
	agent = pauseBrowserTaskAgent(t, store, agent)
	next, err := store.EnqueueTaskForBrowserAgentMode(ctx, client.ID, taskID(t, 183), incarnationID(t, 184), agent.ID, agent.Revision, "next", true, mustTime(t, 130))
	if err != nil {
		t.Fatal(err)
	}
	if next.Task.Status != TaskQueued {
		t.Fatal("queued task lost")
	}
	current, found, err := store.Task(ctx, first.Task.ID)
	if err != nil || !found || current != first.Task {
		t.Fatalf("current changed: %+v %v", current, err)
	}
	if _, err := store.EnqueueTaskForBrowserAgentMode(ctx, client.ID, taskID(t, 185), incarnationID(t, 186), agent.ID, agent.Revision, "now", false, mustTime(t, 131)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("now bypassed queue/paused agent: %v", err)
	}
}
