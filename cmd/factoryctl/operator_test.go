//go:build darwin

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/api"
)

func TestParseExactOperatorCommands(t *testing.T) {
	id := strings.Repeat("ab", 16)
	valid := []struct {
		name string
		args []string
	}{
		{name: "project create", args: []string{"project", "create", "--name", "North Workshop", "--root", "/private/tmp/repo"}},
		{name: "agent create shell default role", args: []string{"agent", "create", "--project", id, "--name", "Builder One", "--provider", "shell", "--tool-budget", "100"}},
		{name: "agent create codex controls", args: []string{"agent", "create", "--project", id, "--name", "Foreman", "--provider", "codex", "--model", "gpt-5.6-luna", "--reasoning-effort", "medium", "--tool-budget", "100", "--role", "orchestrator"}},
		{name: "task add minimal", args: []string{"task", "add", "--project", id, "--agent", id, "--title", "Tighten the queue ordering"}},
		{name: "task add full", args: []string{"task", "add", "--project", id, "--agent", id, "--title", "t", "--body", "b", "--priority", "-5"}},
		{name: "task send back", args: []string{"task", "send-back", "--task", id, "--note", "five findings"}},
		{name: "dispatch on", args: []string{"dispatch", "on"}},
		{name: "dispatch off", args: []string{"dispatch", "off"}},
	}
	for _, test := range valid {
		t.Run(test.name, func(t *testing.T) {
			command, help, ok := parse(test.args)
			if help || !ok {
				t.Fatalf("valid operator command rejected: %+v help=%v ok=%v", command, help, ok)
			}
		})
	}
	if command, _, _ := parse(valid[1].args); command.role != "worker" {
		t.Fatalf("default role = %q", command.role)
	}
	invalid := [][]string{
		{"project", "create", "--name", "n"},
		{"project", "create", "--root", "/private/tmp/repo"},
		{"project", "create", "--name", "n", "--root", "relative"},
		{"project", "create", "--name", "n", "--root", "/r", "--name", "n"},
		{"project", "create", "--name", "n", "--root", "/r", "--project", id},
		{"agent", "create", "--project", id, "--name", "n"},
		{"agent", "create", "--project", id, "--name", "n", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "unknown", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "shell", "--model", "model", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "shell", "--reasoning-effort", "medium", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "codex", "--reasoning-effort", "extreme", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "claude_code", "--reasoning-effort", "ultra", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "codex", "--model", strings.Repeat("m", 129), "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "shell", "--tool-budget", "0"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "shell", "--tool-budget", "01"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "shell", "--tool-budget", "x"},
		{"agent", "create", "--project", "short", "--name", "n", "--provider", "shell", "--tool-budget", "1"},
		{"agent", "create", "--project", id, "--name", "n", "--provider", "shell", "--tool-budget", "1", "--role", "manager"},
		{"task", "add", "--project", id, "--title", "t"},
		{"task", "add", "--project", id, "--agent", id},
		{"task", "add", "--project", id, "--agent", id, "--title", ""},
		{"task", "add", "--project", id, "--agent", id, "--title", "t", "--priority", "1000001"},
		{"task", "add", "--project", id, "--agent", id, "--title", "t", "--priority", "+1"},
		{"task", "send-back", "--task", id},
		{"task", "send-back", "--note", "n"},
		{"task", "send-back", "--task", id, "--note", ""},
		{"task", "send-back", "--task", "short", "--note", "n"},
		{"task", "send-back", "--task", id, "--note", strings.Repeat("n", 8193)},
		{"dispatch", "toggle"},
		{"dispatch"},
		{"project", "make", "--name", "n", "--root", "/r"},
	}
	for _, args := range invalid {
		t.Run("invalid "+strings.Join(args, " "), func(t *testing.T) {
			if command, help, ok := parse(args); help || ok {
				t.Fatalf("invalid operator command accepted: %+v", command)
			}
		})
	}
	for _, args := range [][]string{{"project", "--help"}, {"project", "create", "--help"}, {"task", "add", "-h"}, {"dispatch", "--help"}} {
		if _, help, ok := parse(args); !help || !ok {
			t.Fatalf("help form rejected: %v", args)
		}
	}
}

func TestOperatorCommandsRequireExactEnvironmentBeforeDialing(t *testing.T) {
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"dispatch", "on"}, func(string) string { return "" }, &stdout, &stderr)
	if exit != exitFailure || stdout.Len() != 0 || !strings.Contains(stderr.String(), "operator client configuration is invalid") {
		t.Fatalf("missing environment = exit %d stdout %q stderr %q", exit, stdout.String(), stderr.String())
	}
}

func TestProjectCreateMintsIdentityAndReportsResult(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	var received api.CreateProjectInput
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		input, ok := call.CreateProjectInput()
		if !ok {
			t.Errorf("call kind = %v", call.Kind())
		}
		received = input
		reply, err := api.NewMutationReply(api.MutationResult{Head: 7, Revision: 3})
		if err != nil {
			t.Fatal(err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"project", "create", "--name", "North Workshop", "--root", "/private/tmp/repo"}, webEnvironment(fixture), &stdout, &stderr)
	awaitServer(t, done)
	if exit != 0 || stderr.Len() != 0 {
		t.Fatalf("project create = exit %d stderr %q", exit, stderr.String())
	}
	if received.Name != "North Workshop" || received.Root != "/private/tmp/repo" || !validHumanRequestKey(received.ID) {
		t.Fatalf("daemon received %+v", received)
	}
	var printed struct {
		ID       string `json:"id"`
		Head     uint64 `json:"head"`
		Revision uint64 `json:"revision"`
	}
	if err := json.Unmarshal(stdout.Bytes(), &printed); err != nil || printed.ID != received.ID || printed.Head != 7 || printed.Revision != 3 {
		t.Fatalf("printed %q parsed %+v err %v", stdout.String(), printed, err)
	}
}

func TestAgentCreateCarriesProviderControls(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	projectID := strings.Repeat("11", 16)
	var received api.CreateAgentInput
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		input, ok := call.CreateAgentInput()
		if !ok {
			t.Errorf("call kind = %v", call.Kind())
		}
		received = input
		reply, err := api.NewMutationReply(api.MutationResult{Head: 8, Revision: 1})
		if err != nil {
			t.Fatal(err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{
		"agent", "create", "--project", projectID, "--name", "Builder", "--provider", "codex",
		"--model", "gpt-5.6-luna", "--reasoning-effort", "medium", "--tool-budget", "100", "--role", "orchestrator",
	}, webEnvironment(fixture), &stdout, &stderr)
	awaitServer(t, done)
	if exit != 0 || stderr.Len() != 0 {
		t.Fatalf("agent create = exit %d stderr %q", exit, stderr.String())
	}
	if received.ProjectID != projectID || received.Name != "Builder" || received.Role != "orchestrator" || received.Provider != "codex" || received.Model != "gpt-5.6-luna" || received.ReasoningEffort != "medium" || received.ToolBudgetLimit != 100 || !validHumanRequestKey(received.ID) {
		t.Fatalf("daemon received %+v", received)
	}
	if !strings.Contains(stdout.String(), received.ID) || !strings.Contains(stdout.String(), `"head":8`) || !strings.Contains(stdout.String(), `"revision":1`) {
		t.Fatalf("printed %q", stdout.String())
	}
}

func TestTaskAddMintsDistinctTaskAndIncarnationIdentities(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	projectID := strings.Repeat("11", 16)
	agentID := strings.Repeat("22", 16)
	var received api.EnqueueTaskInput
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		input, ok := call.EnqueueTaskInput()
		if !ok {
			t.Errorf("call kind = %v", call.Kind())
		}
		received = input
		reply, err := api.NewMutationReply(api.MutationResult{Head: 9, Revision: 1})
		if err != nil {
			t.Fatal(err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"task", "add", "--project", projectID, "--agent", agentID, "--title", "Probe the flaky gate", "--body", "private body", "--priority", "-3"}, webEnvironment(fixture), &stdout, &stderr)
	awaitServer(t, done)
	if exit != 0 || stderr.Len() != 0 {
		t.Fatalf("task add = exit %d stderr %q", exit, stderr.String())
	}
	if received.ProjectID != projectID || received.AssignedAgentID != agentID || received.Title != "Probe the flaky gate" || received.Body != "private body" || received.Priority != -3 {
		t.Fatalf("daemon received %+v", received)
	}
	if !validHumanRequestKey(received.ID) || !validHumanRequestKey(received.IncarnationID) || received.ID == received.IncarnationID {
		t.Fatalf("minted identities = %q, %q", received.ID, received.IncarnationID)
	}
	if !strings.Contains(stdout.String(), received.ID) || !strings.Contains(stdout.String(), received.IncarnationID) {
		t.Fatalf("printed %q", stdout.String())
	}
}

func TestTaskSendBackCarriesTaskAndNote(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	taskID := strings.Repeat("33", 16)
	var received api.SendBackInput
	var kind api.CallKind
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		kind = call.Kind()
		received, _ = call.SendBackInput()
		reply, err := api.NewMutationReply(api.MutationResult{Head: 12, Revision: 4})
		if err != nil {
			t.Fatal(err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"task", "send-back", "--task", taskID, "--note", "private note sentinel"}, webEnvironment(fixture), &stdout, &stderr)
	awaitServer(t, done)
	if exit != 0 || stderr.Len() != 0 {
		t.Fatalf("task send-back = exit %d stderr %q", exit, stderr.String())
	}
	if kind != api.CallSendBackTask || received != (api.SendBackInput{TaskID: taskID, Note: "private note sentinel"}) {
		t.Fatalf("daemon received kind %v, %+v", kind, received)
	}
	var printed struct {
		ID       string `json:"id"`
		Head     uint64 `json:"head"`
		Revision uint64 `json:"revision"`
	}
	if err := json.Unmarshal(stdout.Bytes(), &printed); err != nil || printed.ID != taskID || printed.Head != 12 || printed.Revision != 4 {
		t.Fatalf("printed %q, %v", stdout.String(), err)
	}
	if strings.Contains(stdout.String(), "private note sentinel") {
		t.Fatal("output leaked the note")
	}
	for _, args := range [][]string{
		{"task", "send-back", "--task", taskID, "--note", "n", "--name", "x"},
		{"task", "send-back", "--task", taskID, "--note", "n", "--project", taskID},
	} {
		if _, help, ok := parse(args); help || ok {
			t.Fatalf("parsed %q", strings.Join(args, " "))
		}
	}
}

func TestDispatchReadsExactFactoryRevisionThenSets(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	var setRevision uint64
	var setEnabled bool
	done := serveMany(fixture.listener,
		func(call api.Call) api.Reply {
			if call.Kind() != api.CallSnapshot {
				t.Errorf("first call kind = %v", call.Kind())
			}
			reply, err := api.NewSnapshotReply(api.DashboardSnapshot{Head: 4, Factory: api.FactorySummary{DispatchEnabled: false, Capacity: 8, ActiveRuns: 0, Revision: 21}, Projects: []api.ProjectSummary{}, Agents: []api.AgentSummary{}, Tasks: []api.TaskSummary{}})
			if err != nil {
				t.Fatal(err)
			}
			return reply
		},
		func(call api.Call) api.Reply {
			revision, enabled, ok := call.Dispatch()
			if !ok || call.Kind() != api.CallSetDispatch {
				t.Errorf("second call = %v", call.Kind())
			}
			setRevision, setEnabled = revision, enabled
			reply, err := api.NewMutationReply(api.MutationResult{Head: 5, Revision: 22})
			if err != nil {
				t.Fatal(err)
			}
			return reply
		})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"dispatch", "on"}, webEnvironment(fixture), &stdout, &stderr)
	awaitMany(t, done, 2)
	if exit != 0 || stderr.Len() != 0 {
		t.Fatalf("dispatch = exit %d stderr %q", exit, stderr.String())
	}
	if setRevision != 21 || !setEnabled {
		t.Fatalf("set_dispatch received revision %d enabled %v", setRevision, setEnabled)
	}
	if !strings.Contains(stdout.String(), `"enabled":true`) || !strings.Contains(stdout.String(), `"revision":22`) {
		t.Fatalf("printed %q", stdout.String())
	}
}

func TestOperatorRemoteRejectionIsReportedWithoutFabricatedSuccess(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		reply, err := api.NewErrorReply(api.RemoteConflict)
		if err != nil {
			t.Fatal(err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"agent", "create", "--project", strings.Repeat("11", 16), "--name", "Builder", "--provider", "shell", "--tool-budget", "10"}, webEnvironment(fixture), &stdout, &stderr)
	awaitServer(t, done)
	if exit != exitFailure || stdout.Len() != 0 || !strings.Contains(stderr.String(), "agent create was not accepted") {
		t.Fatalf("rejection = exit %d stdout %q stderr %q", exit, stdout.String(), stderr.String())
	}
}
