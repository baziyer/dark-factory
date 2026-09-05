package daemon

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/topology"
)

// consoleFixture pairs one browser client and gives it a project, an agent and
// a queued task to edit.
type consoleFixture struct {
	*adapterFixture
	// cacheRoot is the cache directory this fixture redirected, so a test can
	// prove the topology cache landed inside it and not in the operator's.
	cacheRoot string
	project   kernel.Project
	agent     kernel.Agent
	task      kernel.Task
}

func newConsoleFixture(t *testing.T, capabilities kernel.BrowserCapabilityMask, root string) *consoleFixture {
	t.Helper()
	// Topology writes a regenerable cache under os.UserCacheDir, which reads
	// HOME on Darwin and XDG_CACHE_HOME (else HOME) elsewhere. Redirecting both
	// keeps that write inside the test's own directory on any host, and the
	// cache root is then whatever os.UserCacheDir resolves to, never a literal.
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CACHE_HOME", filepath.Join(home, "cache"))
	cacheRoot, err := os.UserCacheDir()
	if err != nil {
		t.Fatal(err)
	}
	fixture := newAdapterFixture(t, capabilities)
	fixture.pair(t)
	ctx := context.Background()
	projectID, _ := kernel.ProjectIDFromBytes(adapterID(t, 0x21))
	agentID, _ := kernel.AgentIDFromBytes(adapterID(t, 0x22))
	taskID, _ := kernel.TaskIDFromBytes(adapterID(t, 0x23))
	incarnationID, _ := kernel.IncarnationIDFromBytes(adapterID(t, 0x24))
	project, err := fixture.store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "console", Root: root}, adapterTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := fixture.store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: project.ID, Name: "console agent", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderCodex, ToolBudgetLimit: 4}, adapterTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	task, err := fixture.store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID, Title: "console task", Priority: 1}, adapterTime(t, 12))
	if err != nil {
		t.Fatal(err)
	}
	return &consoleFixture{adapterFixture: fixture, cacheRoot: cacheRoot, project: project, agent: agent, task: task}
}

func consoleRoot(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	writeTopologyFixture(t, root, "go.mod", "module example.com/console\n")
	writeTopologyFixture(t, root, "one/one.go", "package one\n")
	return root
}

// The adapter is the authority on both the edit and the revision the client
// gets back: the result must name the same entity at the next revision.
func TestBrowserConsoleUpdatesAdvanceTheExactRevision(t *testing.T) {
	fixture := newConsoleFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions, consoleRoot(t))
	ctx := context.Background()
	client := rawBrowserClient(fixture.client.ID)
	model, effort, paused := "gpt-5-codex", "high", browserprotocol.Bool(true)
	agentResult, err := fixture.backend.UpdateAgent(ctx, client, browserprotocol.AgentUpdate{
		AgentID: fixture.agent.ID.String(), ExpectedRevision: decimalRevision(fixture.agent.Revision),
		Model: &model, ReasoningEffort: &effort, Paused: &paused,
	})
	if err != nil || agentResult.AgentID != fixture.agent.ID.String() || agentResult.Revision != decimalRevision(fixture.agent.Revision)+1 {
		t.Fatalf("agent update = %+v, %v", agentResult, err)
	}
	stored, found, err := fixture.store.Agent(ctx, fixture.agent.ID)
	if err != nil || !found || stored.Model != model || stored.ReasoningEffort != effort || !stored.Paused {
		t.Fatalf("stored agent = %+v, found=%v, err=%v", stored, found, err)
	}
	// The same observation cannot be spent twice.
	if _, err := fixture.backend.UpdateAgent(ctx, client, browserprotocol.AgentUpdate{
		AgentID: fixture.agent.ID.String(), ExpectedRevision: decimalRevision(fixture.agent.Revision), Paused: &paused,
	}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("replayed agent update = %v", err)
	}

	title, priority, status := "edited", int64(5), "cancelled"
	taskResult, err := fixture.backend.UpdateTask(ctx, client, browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: decimalRevision(fixture.task.Revision),
		Title: &title, Priority: &priority,
	})
	if err != nil || taskResult.TaskID != fixture.task.ID.String() || taskResult.Revision != decimalRevision(fixture.task.Revision)+1 {
		t.Fatalf("task update = %+v, %v", taskResult, err)
	}
	cancelled, err := fixture.backend.UpdateTask(ctx, client, browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: taskResult.Revision, Status: &status,
	})
	if err != nil || cancelled.Revision != taskResult.Revision+1 {
		t.Fatalf("task cancel = %+v, %v", cancelled, err)
	}
	storedTask, found, err := fixture.store.Task(ctx, fixture.task.ID)
	if err != nil || !found || storedTask.Title != title || storedTask.Priority != priority || storedTask.Status != kernel.TaskCancelled {
		t.Fatalf("stored task = %+v, found=%v, err=%v", storedTask, found, err)
	}
	// A task that has left the queue is a conflict, not a fresh edit.
	if _, err := fixture.backend.UpdateTask(ctx, client, browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: cancelled.Revision, Title: &title,
	}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("edit after cancel = %v", err)
	}
}

// Topology is an observation; the two updates are operator mutations. An
// observe-only pairing may do the first and none of the second.
func TestBrowserConsoleGatesUpdatesOnHumanActionsButNotTopology(t *testing.T) {
	root := consoleRoot(t)
	fixture := newConsoleFixture(t, kernel.BrowserCapabilityObserve, root)
	ctx := context.Background()
	client := rawBrowserClient(fixture.client.ID)
	paused, status := browserprotocol.Bool(true), "cancelled"
	if _, err := fixture.backend.UpdateAgent(ctx, client, browserprotocol.AgentUpdate{
		AgentID: fixture.agent.ID.String(), ExpectedRevision: decimalRevision(fixture.agent.Revision), Paused: &paused,
	}); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("observe-only agent update = %v", err)
	}
	if _, err := fixture.backend.UpdateTask(ctx, client, browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: decimalRevision(fixture.task.Revision), Status: &status,
	}); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("observe-only task update = %v", err)
	}
	// A refused mutation left nothing behind.
	stored, found, err := fixture.store.Agent(ctx, fixture.agent.ID)
	if err != nil || !found || stored.Revision != fixture.agent.Revision || stored.Paused {
		t.Fatalf("agent after refused update = %+v, found=%v, err=%v", stored, found, err)
	}
	result, err := fixture.backend.Topology(ctx, client, browserprotocol.TopologyGet{ProjectID: fixture.project.ID.String()})
	// consoleRoot is a Go module with one package below it, so the served tree
	// is exactly the module and the repository at ".", plus the package. An
	// exact count is what catches a projection that silently drops a subtree.
	if err != nil || result.ProjectID != fixture.project.ID.String() || len(result.Digest) != 64 || len(result.Nodes) != 3 {
		t.Fatalf("observe-only topology = %+v, %v", result, err)
	}
	for _, node := range result.Nodes {
		if node.Kind == "" || node.Path == "" || node.SizeBucket == "" {
			t.Fatalf("topology node is not projected: %+v", node)
		}
	}
	// The regenerable cache the request wrote is inside this test's own cache
	// directory, which is the whole reason the fixture redirects it.
	cacheFile := filepath.Join(fixture.cacheRoot, "dark-factory", "topology", fixture.project.ID.String(), "snapshot.json")
	if _, err := os.Stat(cacheFile); err != nil {
		t.Fatalf("topology cache is not under the test home: %v", err)
	}
	// An unknown project is not found; a caller that gave up gets a retryable
	// answer rather than a fault.
	unknown, _ := kernel.ProjectIDFromBytes(adapterID(t, 0x25))
	if _, err := fixture.backend.Topology(ctx, client, browserprotocol.TopologyGet{ProjectID: unknown.String()}); !errors.Is(err, browser.ErrNotFound) {
		t.Fatalf("unknown project topology = %v", err)
	}
	cancelled, cancel := context.WithCancel(ctx)
	cancel()
	if _, err := fixture.backend.Topology(cancelled, client, browserprotocol.TopologyGet{ProjectID: fixture.project.ID.String()}); !errors.Is(err, browser.ErrRateLimited) {
		t.Fatalf("abandoned topology request = %v", err)
	}
}

func TestBrowserConsoleRejectsUnusableIdentitiesAndRevisions(t *testing.T) {
	fixture := newConsoleFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions, consoleRoot(t))
	ctx := context.Background()
	client := rawBrowserClient(fixture.client.ID)
	unknownAgent, _ := kernel.AgentIDFromBytes(adapterID(t, 0x31))
	if _, err := fixture.backend.UpdateAgent(ctx, client, browserprotocol.AgentUpdate{
		AgentID: unknownAgent.String(), ExpectedRevision: 1,
	}); !errors.Is(err, browser.ErrNotFound) {
		t.Fatalf("unknown agent = %v", err)
	}
	if _, err := fixture.backend.UpdateTask(ctx, client, browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: decimalRevision(fixture.task.Revision) + 5,
	}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("stale task revision = %v", err)
	}
	// The durable key is (agent, project) together, so a foreign agent is a
	// refused edit rather than a corrupt row.
	foreignProject, _ := kernel.ProjectIDFromBytes(adapterID(t, 0x32))
	foreignAgent, _ := kernel.AgentIDFromBytes(adapterID(t, 0x33))
	other, err := fixture.store.CreateProject(ctx, kernel.NewProject{ID: foreignProject, Name: "other", Root: filepath.Join(t.TempDir(), "other")}, adapterTime(t, 20))
	if err != nil {
		t.Fatal(err)
	}
	stranger, err := fixture.store.CreateAgent(ctx, kernel.NewAgent{ID: foreignAgent, ProjectID: other.ID, Name: "stranger", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderCodex, ToolBudgetLimit: 1}, adapterTime(t, 21))
	if err != nil {
		t.Fatal(err)
	}
	assigned := stranger.ID.String()
	if _, err := fixture.backend.UpdateTask(ctx, client, browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: decimalRevision(fixture.task.Revision), AssignedAgentID: &assigned,
	}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("cross-project reassignment = %v", err)
	}
}

// An owner-side effect whose cause carries a deadline still says "the outcome
// is unknown". Mapping that to retryable busyness would invite a second
// attempt at an effect that may already have landed.
func TestBrowserEffectVerdictSurvivesItsDeadlineCause(t *testing.T) {
	// browser.ErrRateLimited is the only kind the transport answers as
	// retryable; every other browser kind is a definite, non-retryable answer,
	// and an unmapped error becomes internal.
	retryable := func(err error) bool { return errors.Is(err, browser.ErrRateLimited) }
	for _, verdict := range []error{ErrTerminalEffectUncertain, ErrTerminalEffectPartial, ErrTerminalEffectRejected} {
		// This is exactly the shape uncertainTerminalEffect(context.DeadlineExceeded)
		// produces on the owner side.
		mapped := mapBrowserError(errors.Join(verdict, context.DeadlineExceeded))
		if !errors.Is(mapped, verdict) || retryable(mapped) {
			t.Fatalf("%v mapped to %v", verdict, mapped)
		}
	}
	unknown := kernel.NewOutcomeUnknownError(context.DeadlineExceeded)
	if mapped := mapBrowserError(unknown); !errors.Is(mapped, unknown) || retryable(mapped) {
		t.Fatalf("outcome-unknown mapped to %v", mapped)
	}
	// A bare deadline, with no owner verdict, is still retryable busyness.
	if mapped := mapBrowserError(context.DeadlineExceeded); !retryable(mapped) {
		t.Fatalf("bare deadline = %v", mapped)
	}
}

// The tree bounds nothing. A node the wire cannot carry is dropped with its
// subtree so the whole project stays serveable.
func TestProjectTopologyDropsNodesTheWireCannotCarry(t *testing.T) {
	root := topology.Node{ID: strings.Repeat("a1", 32), Kind: topology.NodeRepository, RelativePath: ".", Label: "repository", SizeBucket: "small"}
	longLabel := topology.Node{ID: strings.Repeat("b2", 32), ParentID: root.ID, Kind: topology.NodeDirectory, RelativePath: "wide", Label: strings.Repeat("l", browserprotocol.MaxAgentNameBytes+1), SizeBucket: "small"}
	childOfLongLabel := topology.Node{ID: strings.Repeat("c3", 32), ParentID: longLabel.ID, Kind: topology.NodeDirectory, RelativePath: "wide/inner", Label: "inner", SizeBucket: "small"}
	longPath := topology.Node{ID: strings.Repeat("d4", 32), ParentID: root.ID, Kind: topology.NodeDirectory, RelativePath: strings.Repeat("p", browserprotocol.MaxTaskTitleBytes+1), Label: "deep", SizeBucket: "small"}
	invalidUTF8 := topology.Node{ID: strings.Repeat("e5", 32), ParentID: root.ID, Kind: topology.NodeDirectory, RelativePath: "bad", Label: string([]byte{0xff}), SizeBucket: "small"}
	keeper := topology.Node{ID: strings.Repeat("f6", 32), ParentID: root.ID, Kind: topology.NodePackage, RelativePath: "internal/kernel", Label: "kernel", Language: "go", SizeBucket: "large"}
	snapshot := topology.Snapshot{
		Digest: strings.Repeat("ab", 32),
		Nodes:  []topology.Node{root, longLabel, childOfLongLabel, longPath, invalidUTF8, keeper},
	}
	result := projectTopology("01010101010101010101010101010101", snapshot)
	served := make([]string, 0, len(result.Nodes))
	for _, node := range result.Nodes {
		served = append(served, node.ID)
	}
	if len(served) != 2 || served[0] != root.ID || served[1] != keeper.ID {
		t.Fatalf("served nodes = %v", served)
	}
	// The frame the console actually receives must encode.
	if _, err := browserprotocol.EncodeTopology("topology", result); err != nil {
		t.Fatalf("clamped topology did not encode: %v", err)
	}
}

// A directory the filesystem accepts but the wire cannot label is real: 129
// bytes is a legal name everywhere Dark Factory runs.
func TestBrowserConsoleServesAProjectWithAnOverLongDirectoryName(t *testing.T) {
	root := consoleRoot(t)
	writeTopologyFixture(t, root, strings.Repeat("d", browserprotocol.MaxAgentNameBytes+1)+"/inner/inner.go", "package inner\n")
	fixture := newConsoleFixture(t, kernel.BrowserCapabilityObserve, root)
	result, err := fixture.backend.Topology(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.TopologyGet{ProjectID: fixture.project.ID.String()})
	if err != nil {
		t.Fatalf("topology with an over-long directory = %v", err)
	}
	// The over-long directory costs its own subtree and nothing else: the
	// module, the repository and the "one" package are still served.
	if len(result.Nodes) != 3 {
		t.Fatalf("served %d nodes: %+v", len(result.Nodes), result.Nodes)
	}
	for _, node := range result.Nodes {
		if len(node.Label) > browserprotocol.MaxAgentNameBytes || len(node.Path) > browserprotocol.MaxTaskTitleBytes {
			t.Fatalf("served an unencodable node: %+v", node)
		}
	}
	if _, err := browserprotocol.EncodeTopology("topology", result); err != nil {
		t.Fatalf("daemon-produced topology did not encode: %v", err)
	}
}

// Reassignment inside the project is the queue edit the console offers beside
// reorder and cancel.
func TestBrowserConsoleReassignsATaskWithinItsProject(t *testing.T) {
	fixture := newConsoleFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions, consoleRoot(t))
	ctx := context.Background()
	secondID, _ := kernel.AgentIDFromBytes(adapterID(t, 0x41))
	second, err := fixture.store.CreateAgent(ctx, kernel.NewAgent{ID: secondID, ProjectID: fixture.project.ID, Name: "second", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderCodex, ToolBudgetLimit: 4}, adapterTime(t, 13))
	if err != nil {
		t.Fatal(err)
	}
	assigned := second.ID.String()
	result, err := fixture.backend.UpdateTask(ctx, rawBrowserClient(fixture.client.ID), browserprotocol.TaskUpdate{
		TaskID: fixture.task.ID.String(), ExpectedRevision: decimalRevision(fixture.task.Revision), AssignedAgentID: &assigned,
	})
	if err != nil || result.Revision != decimalRevision(fixture.task.Revision)+1 {
		t.Fatalf("reassignment = %+v, %v", result, err)
	}
	stored, found, err := fixture.store.Task(ctx, fixture.task.ID)
	if err != nil || !found || stored.AssignedAgentID != second.ID || stored.Status != kernel.TaskQueued {
		t.Fatalf("reassigned task = %+v, found=%v, err=%v", stored, found, err)
	}
}

// The derived graph is ordered by path and kind, not by ancestry: a root
// go.mod puts the module node before the repository that contains it. A
// projection that trusted slice order dropped the module as "parent absent"
// and, because every top-level node hangs off it, the whole tree with it.
func TestProjectTopologyKeepsARootModuleAheadOfItsRepository(t *testing.T) {
	root := t.TempDir()
	writeTopologyFixture(t, root, "go.mod", "module example.com/console\n")
	writeTopologyFixture(t, root, "one/one.go", "package one\n")
	snapshot, err := topology.Build(context.Background(), root, nil)
	if err != nil {
		t.Fatal(err)
	}
	// The shape this guards against only exists when the module sorts first.
	if len(snapshot.Nodes) == 0 || snapshot.Nodes[0].Kind != topology.NodeModule {
		t.Fatalf("fixture does not reproduce the ordering: %+v", snapshot.Nodes)
	}
	served := projectTopology("01010101010101010101010101010101", snapshot)
	if len(served.Nodes) != len(snapshot.Nodes) {
		t.Fatalf("served %d of %d nodes: %+v", len(served.Nodes), len(snapshot.Nodes), served.Nodes)
	}
	kinds := make(map[string]int, len(served.Nodes))
	for _, node := range served.Nodes {
		kinds[node.Kind]++
	}
	if kinds["module"] != 1 || kinds["repository"] != 1 || kinds["package"] != 1 {
		t.Fatalf("served kinds = %v", kinds)
	}
	if _, err := browserprotocol.EncodeTopology("topology", served); err != nil {
		t.Fatalf("served topology did not encode: %v", err)
	}
}

// A reasoning effort no provider accepts is not a lost race: refetching does
// not make it valid, so answering stale would send the console around a loop
// it cannot leave.
func TestBrowserConsoleAnswersInvalidRequestForARefusedLaunchControl(t *testing.T) {
	fixture := newConsoleFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions, consoleRoot(t))
	ctx := context.Background()
	client := rawBrowserClient(fixture.client.ID)
	effort := "sideways"
	if _, err := fixture.backend.UpdateAgent(ctx, client, browserprotocol.AgentUpdate{
		AgentID: fixture.agent.ID.String(), ExpectedRevision: decimalRevision(fixture.agent.Revision), ReasoningEffort: &effort,
	}); !errors.Is(err, browser.ErrInvalidRequest) {
		t.Fatalf("refused reasoning effort = %v", err)
	}
	// A revision that lost its race is still stale: the two are not the same
	// answer and the console acts on them differently.
	paused := browserprotocol.Bool(true)
	if _, err := fixture.backend.UpdateAgent(ctx, client, browserprotocol.AgentUpdate{
		AgentID: fixture.agent.ID.String(), ExpectedRevision: decimalRevision(fixture.agent.Revision) + 9, Paused: &paused,
	}); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("stale revision = %v", err)
	}
	stored, found, err := fixture.store.Agent(ctx, fixture.agent.ID)
	if err != nil || !found || stored.Revision != fixture.agent.Revision {
		t.Fatalf("agent after refused updates = %+v, found=%v, err=%v", stored, found, err)
	}
}
