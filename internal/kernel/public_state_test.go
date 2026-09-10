package kernel

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"testing"
)

// Every public kind remains coherent; tasks use admission order, while other
// entity collections retain raw identity order.
func TestPublicSnapshotKeepsEveryKindInCanonicalOrder(t *testing.T) {
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	ctx := context.Background()
	project, found, err := store.Project(ctx, run.ProjectID)
	if err != nil || !found {
		t.Fatalf("project = %+v, found=%v, err=%v", project, found, err)
	}
	state, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SetCapacity(ctx, state.Revision, 20, mustTime(t, 450)); err != nil {
		t.Fatal(err)
	}
	for index := 9; index >= 1; index-- {
		id := publicAgentID(t, index)
		if _, err := store.CreateAgent(ctx, NewAgent{ID: id, ProjectID: project.ID, Name: fmt.Sprintf("agent-%d", index), Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 1}, mustTime(t, int64(500+index))); err != nil {
			t.Fatal(err)
		}
	}
	for index := 9; index >= 1; index-- {
		id := publicTaskID(t, index)
		if _, err := store.EnqueueTask(ctx, NewTask{ID: id, ProjectID: project.ID, AssignedAgentID: publicAgentID(t, index), IncarnationID: incarnationID(t, byte(100+index)), Title: fmt.Sprintf("task-%d", index), Priority: int64(100 - index)}, mustTime(t, int64(600+index))); err != nil {
			t.Fatal(err)
		}
	}
	runIDs := []RunID{run.ID}
	seeds := []byte{17, 24, 31, 38, 45, 53, 60, 67}
	for index := 1; index <= 8; index++ {
		keys := admissionKeys(t, seeds[index-1], nil)
		admission, err := store.AdmitNext(ctx, keys, mustTime(t, int64(800+index)))
		if err != nil || !admission.Admitted() || admission.Run == nil {
			t.Fatalf("admit run %d = %+v, %v", index, admission, err)
		}
		materializeAdmittedWorkerChange(t, store, *admission.Run, int64(900+index*10))
		activatedRun := activateAllResourcesUnique(t, store, *admission.Run, int64(900+index*10), int64(5000+index*100))
		session := terminalSessionForRunTest(t, store, admission.Run.ID)
		running, err := store.ActivateRun(ctx, admission.Run.ID, session.ID, activatedRun.Revision, session.Revision, mustTime(t, int64(909+index*10)))
		if err != nil {
			t.Fatal(err)
		}
		runIDs = append(runIDs, running.ID)
	}
	insertPublicHumanRequests(t, store, runIDs)
	state, err = store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}

	snapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Head != state.Head {
		t.Fatalf("snapshot head = %d, want %d", snapshot.Head.Int64(), state.Head.Int64())
	}
	agents := make([]string, 0, len(snapshot.Agents))
	for _, item := range snapshot.Agents {
		agents = append(agents, item.ID.String())
	}
	tasks := make([]string, 0, len(snapshot.Tasks))
	for _, item := range snapshot.Tasks {
		tasks = append(tasks, item.ID.String())
	}
	requests := make([]string, 0, len(snapshot.HumanRequests))
	for _, item := range snapshot.HumanRequests {
		requests = append(requests, item.ID.String())
	}
	wantAgents := append([]string{run.AgentID.String()}, publicIDs(9, func(index int) string { return publicAgentID(t, index).String() })...)
	wantTasks := append([]string{run.TaskID.String()}, publicIDs(9, func(index int) string { return publicTaskID(t, index).String() })...)
	wantRequests := publicIDs(9, func(index int) string { return publicHumanRequestID(t, index).String() })
	if !reflect.DeepEqual(agents, wantAgents) || !reflect.DeepEqual(tasks, wantTasks) || !reflect.DeepEqual(requests, wantRequests) {
		t.Fatalf("snapshot = agents %v, tasks %v, requests %v", agents, tasks, requests)
	}
	if len(snapshot.Projects) != 1 || snapshot.Projects[0].ID != project.ID {
		t.Fatalf("snapshot projects = %+v", snapshot.Projects)
	}
}

// An empty Factory still returns a complete snapshot: the collections are
// present and empty rather than absent or nil-typed.
func TestPublicSnapshotOfAnEmptyFactoryIsComplete(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	snapshot, err := store.ReadPublicSnapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Factory.Capacity == 0 || snapshot.Factory.Revision.Int64() < 1 {
		t.Fatalf("factory summary = %+v", snapshot.Factory)
	}
	if snapshot.Projects == nil || snapshot.Agents == nil || snapshot.Tasks == nil || snapshot.HumanRequests == nil {
		t.Fatalf("empty snapshot has an absent collection: %+v", snapshot)
	}
	if len(snapshot.Projects)+len(snapshot.Agents)+len(snapshot.Tasks)+len(snapshot.HumanRequests) != 0 {
		t.Fatalf("empty snapshot is not empty: %+v", snapshot)
	}
}

// The entity bound is exact and fails closed. At the limit the snapshot is
// refused entirely; nothing partial or truncated is returned.
func TestPublicSnapshotCountBoundFailsClosedWithoutTruncation(t *testing.T) {
	for _, count := range []int{PublicStateEntityLimit - 1, PublicStateEntityLimit} {
		t.Run(fmt.Sprintf("dynamic_%d", count), func(t *testing.T) {
			store, _ := newTestStore(t)
			defer store.Close()
			insertPublicProjects(t, store, count)
			snapshot, err := store.ReadPublicSnapshot(context.Background())
			if count == PublicStateEntityLimit-1 {
				if err != nil || len(snapshot.Projects) != count {
					t.Fatalf("maximum accepted snapshot = %d projects, %v", len(snapshot.Projects), err)
				}
				return
			}
			if !errors.Is(err, ErrSnapshotTooLarge) {
				t.Fatalf("oversized snapshot = %v", err)
			}
			if len(snapshot.Projects) != 0 || len(snapshot.Agents) != 0 || len(snapshot.Tasks) != 0 || len(snapshot.HumanRequests) != 0 || snapshot.Head.Int64() != 0 {
				t.Fatalf("oversized read returned partial state: %+v", snapshot)
			}
		})
	}
}

// The bound counts the factory plus every dynamic kind together, so no single
// kind can be under its own limit while the whole is over.
func TestPublicSnapshotCountBoundIncludesEveryDynamicKind(t *testing.T) {
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	ctx := context.Background()
	if _, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(130), QuestionText: "private"}, mustTime(t, 400)); err != nil {
		t.Fatal(err)
	}
	// The running fixture contributes one project, agent, and task; its open
	// request is the fourth dynamic kind. Fill projects to 4,095 dynamic rows.
	insertPublicProjects(t, store, PublicStateEntityLimit-5)
	if _, err := store.ReadPublicSnapshot(ctx); err != nil {
		t.Fatalf("distributed maximum snapshot: %v", err)
	}
	overflow := publicProjectID(t, PublicStateEntityLimit-4)
	if _, err := store.writer.Exec(`INSERT INTO projects(id, name, root, verification_policy, revision, created_at_ms, updated_at_ms) VALUES(?, 'overflow', '/public/overflow', 'none', 1, 401, 401)`, overflow.Bytes()); err != nil {
		t.Fatal(err)
	}
	snapshot, err := store.ReadPublicSnapshot(ctx)
	if !errors.Is(err, ErrSnapshotTooLarge) || len(snapshot.Projects) != 0 {
		t.Fatalf("distributed overflow snapshot = %+v, %v", snapshot, err)
	}
}

// One pinned read cannot observe a commit that lands after it began.
func TestPublicSnapshotPinnedReadCannotMixConcurrentCommit(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	tx, err := store.beginRead(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Close()
	if err := validateDurableControls(ctx, tx.connection); err != nil {
		t.Fatal(err)
	}
	if err := enforcePublicStateCount(ctx, tx.connection); err != nil {
		t.Fatal(err)
	}
	if _, err := store.CreateProject(ctx, NewProject{ID: publicProjectID(t, 1), Name: "later", Root: "/later"}, mustTime(t, 2)); err != nil {
		t.Fatal(err)
	}
	projects, err := readPublicProjects(ctx, tx.connection)
	if err != nil || len(projects) != 0 {
		t.Fatalf("pinned projects = %+v, %v", projects, err)
	}
	current, err := store.ReadPublicSnapshot(ctx)
	if err != nil || len(current.Projects) != 1 {
		t.Fatalf("current snapshot = %+v, %v", current, err)
	}
}

// A concurrent writer can never produce a mixed-head snapshot: the head and
// the rows always come from the same transaction, so the project count is
// exactly the number of commits the head accounts for.
func TestPublicSnapshotConcurrentWriterNeverMixesHeadAndRows(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	const writes = 8
	specs := make([]NewProject, writes)
	times := make([]UnixMillis, writes)
	for index := range specs {
		specs[index] = NewProject{ID: publicProjectID(t, index+1), Name: fmt.Sprintf("project-%d", index+1), Root: fmt.Sprintf("/concurrent/%d", index+1)}
		times[index] = mustTime(t, int64(index+2))
	}
	done := make(chan error, 1)
	go func() {
		for index := range specs {
			if _, err := store.CreateProject(ctx, specs[index], times[index]); err != nil {
				done <- err
				return
			}
		}
		done <- nil
	}()
	writerDone := false
	observed := 0
	for !writerDone {
		snapshot, err := store.ReadPublicSnapshot(ctx)
		if err != nil {
			t.Fatal(err)
		}
		// Every commit in this fixture is exactly one project creation, so the
		// head is the number of projects the same transaction must contain.
		if len(snapshot.Projects) != int(snapshot.Head.Int64()) {
			t.Fatalf("mixed snapshot: head=%d projects=%d", snapshot.Head.Int64(), len(snapshot.Projects))
		}
		for index, summary := range snapshot.Projects {
			if summary.ID != publicProjectID(t, index+1) {
				t.Fatalf("mixed row %d = %+v", index, summary)
			}
		}
		observed++
		select {
		case err := <-done:
			if err != nil {
				t.Fatal(err)
			}
			writerDone = true
		default:
		}
	}
	if observed == 0 {
		t.Fatal("no snapshot was observed during the concurrent write")
	}
	state, err := store.Factory(ctx)
	if err != nil || state.Head.Int64() != writes {
		t.Fatalf("final state = %+v, %v", state, err)
	}
}

// The snapshot is a positive allowlist. Private durable columns are not even
// selected, so they cannot reach a projection.
func TestPublicSnapshotProjectionOmitsPrivateRows(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	project, err := store.CreateProject(ctx, NewProject{ID: publicProjectID(t, 1), Name: "public project", Root: "/PRIVATE_ROOT_SENTINEL"}, mustTime(t, 2))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := store.CreateAgent(ctx, NewAgent{ID: publicAgentID(t, 1), ProjectID: project.ID, Name: "public agent", Role: RoleWorker, Provider: ProviderCodex, Model: "SERVED_MODEL_SENTINEL", ReasoningEffort: "xhigh", ToolBudgetLimit: 987654321}, mustTime(t, 3))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(ctx, NewTask{ID: publicTaskID(t, 1), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 231), Title: "public task", Body: "PRIVATE_BODY_SENTINEL", Priority: 9}, mustTime(t, 4)); err != nil {
		t.Fatal(err)
	}
	snapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	// The agent's provider, model and reasoning effort are served public facts
	// (the console displays and edits the last two by owner decision on 5
	// September 2026); tool budgets, roots and bodies remain private.
	for _, private := range []string{"PRIVATE_ROOT_SENTINEL", "PRIVATE_BODY_SENTINEL", "987654321"} {
		if strings.Contains(string(encoded), private) {
			t.Fatalf("snapshot exposed %q: %s", private, encoded)
		}
	}
	for _, served := range []string{"codex", "SERVED_MODEL_SENTINEL", "xhigh"} {
		if !strings.Contains(string(encoded), served) {
			t.Fatalf("snapshot dropped the served fact %q: %s", served, encoded)
		}
	}
}

func TestAgentSummariesServeTheExactProviderOnEveryReadPath(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	project, err := store.CreateProject(ctx, NewProject{ID: publicProjectID(t, 1), Name: "workshop", Root: "/tmp/workshop"}, mustTime(t, 2))
	if err != nil {
		t.Fatal(err)
	}
	providers := map[byte]Provider{1: ProviderClaudeCode, 2: ProviderCodex, 3: ProviderShell}
	want := map[string]string{}
	for seed, provider := range providers {
		agent := NewAgent{ID: publicAgentID(t, int(seed)), ProjectID: project.ID, Name: fmt.Sprintf("agent %d", seed), Role: RoleWorker, Provider: provider, ToolBudgetLimit: 5}
		if provider != ProviderShell {
			agent.Model = "model"
			agent.ReasoningEffort = "medium"
		}
		created, err := store.CreateAgent(ctx, agent, mustTime(t, 3+int64(seed)))
		if err != nil {
			t.Fatal(err)
		}
		want[created.ID.String()] = provider.String()
	}
	publicSnapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	publicProviders := map[string]string{}
	for _, summary := range publicSnapshot.Agents {
		publicProviders[summary.ID.String()] = summary.Provider
	}
	if !reflect.DeepEqual(publicProviders, want) {
		t.Fatalf("public snapshot providers = %v, want %v", publicProviders, want)
	}
	snapshot, err := store.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	snapshotProviders := map[string]string{}
	for _, summary := range snapshot.Agents {
		snapshotProviders[summary.ID.String()] = summary.Provider
	}
	if !reflect.DeepEqual(snapshotProviders, want) {
		t.Fatalf("dashboard providers = %v, want %v", snapshotProviders, want)
	}
}

func TestPublicSnapshotRejectsMalformedDurableControls(t *testing.T) {
	for _, test := range []struct {
		name    string
		corrupt string
		secret  string
	}{
		{name: "factory boolean", corrupt: `UPDATE factory SET dispatch_enabled = -1`},
		{name: "private agent control", corrupt: `UPDATE agents SET provider = 'PRIVATE_PROVIDER_SENTINEL'`, secret: "PRIVATE_PROVIDER_SENTINEL"},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, _ := newTestStore(t)
			defer store.Close()
			if test.secret != "" {
				seedDurableAuthority(t, store)
			}
			corruptSQL(t, store, test.corrupt)
			if _, err := store.ReadPublicSnapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
				t.Fatalf("snapshot = %v", err)
			} else if test.secret != "" && strings.Contains(err.Error(), test.secret) {
				t.Fatalf("snapshot error exposed private control: %v", err)
			}
		})
	}
}

func insertPublicProjects(t *testing.T, store *Store, count int) {
	t.Helper()
	tx, err := store.writer.Begin()
	if err != nil {
		t.Fatal(err)
	}
	statement, err := tx.Prepare(`INSERT INTO projects(id, name, root, verification_policy, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, 'none', 1, 1, 1)`)
	if err != nil {
		tx.Rollback()
		t.Fatal(err)
	}
	for index := count; index >= 1; index-- {
		if _, err := statement.Exec(publicProjectID(t, index).Bytes(), fmt.Sprintf("project-%d", index), fmt.Sprintf("/public/%d", index)); err != nil {
			statement.Close()
			tx.Rollback()
			t.Fatal(err)
		}
	}
	if err := statement.Close(); err != nil {
		tx.Rollback()
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
}

func insertPublicHumanRequests(t *testing.T, store *Store, runIDs []RunID) {
	t.Helper()
	tx, err := store.writer.Begin()
	if err != nil {
		t.Fatal(err)
	}
	statement, err := tx.Prepare(`INSERT INTO human_requests(id, run_id, idempotency_key, kind, reason_code, question_text, status, revision, created_at_ms, updated_at_ms) VALUES(?, ?, ?, 'question', 'provider_question', ?, 'open', 1, 1200, 1200)`)
	if err != nil {
		tx.Rollback()
		t.Fatal(err)
	}
	for index := len(runIDs); index >= 1; index-- {
		key := publicRawID(0xbc, index)
		if _, err := statement.Exec(publicHumanRequestID(t, index).Bytes(), runIDs[index-1].Bytes(), key, fmt.Sprintf("PRIVATE_HR_%d", index)); err != nil {
			statement.Close()
			tx.Rollback()
			t.Fatal(err)
		}
	}
	if err := statement.Close(); err != nil {
		tx.Rollback()
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
}

func publicRawID(prefix byte, index int) []byte {
	raw := bytes.Repeat([]byte{prefix}, IDBytes)
	binary.BigEndian.PutUint64(raw[IDBytes-8:], uint64(index))
	return raw
}

func publicProjectID(t *testing.T, index int) ProjectID {
	t.Helper()
	id, err := ProjectIDFromBytes(publicRawID(0xa1, index))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func publicAgentID(t *testing.T, index int) AgentID {
	t.Helper()
	id, err := AgentIDFromBytes(publicRawID(0xa2, index))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func publicTaskID(t *testing.T, index int) TaskID {
	t.Helper()
	id, err := TaskIDFromBytes(publicRawID(0xa3, index))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func publicHumanRequestID(t *testing.T, index int) HumanRequestID {
	t.Helper()
	id, err := HumanRequestIDFromBytes(publicRawID(0xa4, index))
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func publicIDs(count int, at func(int) string) []string {
	result := make([]string, 0, count)
	for index := 1; index <= count; index++ {
		result = append(result, at(index))
	}
	return result
}
