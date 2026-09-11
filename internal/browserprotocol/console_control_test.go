package browserprotocol

import (
	"fmt"
	"strings"
	"testing"
)

// The manifest fixture loop proves the positive path and the closed direction
// for every console message. This proves the bounds beside it: without them a
// widened body would ship green.
func TestConsoleControlBounds(t *testing.T) {
	agent := "02020202020202020202020202020202"
	task := "03030303030303030303030303030303"
	node := strings.Repeat("a1", 32)
	for _, frame := range []string{
		// Every mutable member is optional; only the identity and the observed
		// revision are required.
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"7"}}`,
		`{"type":"PROJECT_LIMITS","id":"x","body":{"project_id":"` + agent + `","expected_revision":"7","run_budget":"12","max_run_seconds":900}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7"}}`,
	} {
		if _, err := DecodeClientControl([]byte(frame)); err != nil {
			t.Fatalf("optional members refused: %v", err)
		}
	}
	for _, frame := range []string{
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"0"}}`,
		`{"type":"PROJECT_LIMITS","id":"x","body":{"project_id":"` + agent + `","expected_revision":"7","run_budget":"12","max_run_seconds":86401}}`,
		`{"type":"PROJECT_LIMITS","id":"x","body":{"project_id":"` + agent + `","expected_revision":"7","run_budget":"9223372036854775808","max_run_seconds":900}}`,
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"7","model":"` + strings.Repeat("m", MaxAgentModelBytes+1) + `"}}`,
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"7","reasoning_effort":"` + strings.Repeat("e", MaxAgentModelBytes+1) + `"}}`,
		// Cancellation is the only status transition the console may ask for.
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","status":"succeeded"}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","priority":1000001}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","title":""}}`,
		`{"type":"TOPOLOGY_GET","id":"x","body":{"project_id":"0000000000000000000000000000000"}}`,
		// An explicit null is not "absent". encoding/json would leave the
		// pointer nil and report success with an advanced revision, while the
		// browser's exact decoder refuses the same frame.
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"7","model":null}}`,
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"7","reasoning_effort":null}}`,
		`{"type":"AGENT_UPDATE","id":"x","body":{"agent_id":"` + agent + `","expected_revision":"7","paused":null}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","title":null}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","priority":null}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","assigned_agent_id":null}}`,
		`{"type":"TASK_UPDATE","id":"x","body":{"task_id":"` + task + `","expected_revision":"7","status":null}}`,
	} {
		if _, err := DecodeClientControl([]byte(frame)); err != ErrMalformed {
			t.Fatalf("%s accepted: %v", frame, err)
		}
	}
	topology := func(nodes string) string {
		return `{"type":"TOPOLOGY","id":"x","body":{"project_id":"01010101010101010101010101010101","digest":"` +
			strings.Repeat("ab", 32) + `","source_revision":"","nodes":[` + nodes + `]}}`
	}
	good := `{"id":"` + node + `","parent_id":"","kind":"package","path":"internal/kernel","label":"kernel","language":"go","size_bucket":"large"}`
	if _, err := DecodeServerControl([]byte(topology(good))); err != nil {
		t.Fatalf("topology refused: %v", err)
	}
	for _, bad := range []string{
		strings.Replace(good, `"kind":"package"`, `"kind":"symbol"`, 1),
		strings.Replace(good, `"size_bucket":"large"`, `"size_bucket":"huge"`, 1),
		strings.Replace(good, `"id":"`+node+`"`, `"id":"a1"`, 1),
		strings.Replace(good, `"label":"kernel"`, `"label":""`, 1),
	} {
		if _, err := DecodeServerControl([]byte(topology(bad))); err != ErrMalformed {
			t.Fatalf("%s accepted: %v", bad, err)
		}
	}
}

// TOPOLOGY is the second frame allowed past the 64 KiB control bound: a
// repository graph does not fit in it. The bound it does have is the
// snapshot's, and it still fails closed.
func TestTopologyIsBoundedBySnapshotBytesNotControlBytes(t *testing.T) {
	nodes := func(count, pathBytes int) []TopologyNode {
		result := make([]TopologyNode, 0, count)
		for index := 0; index < count; index++ {
			result = append(result, TopologyNode{
				ID: fmt.Sprintf("%064x", index+1), Kind: "directory", Path: strings.Repeat("p", pathBytes),
				Label: "leaf", SizeBucket: "small",
			})
		}
		return result
	}
	body := func(items []TopologyNode) Topology {
		return Topology{ProjectID: "01010101010101010101010101010101", Digest: strings.Repeat("ab", 32), Nodes: items}
	}
	wide, err := EncodeTopology("x", body(nodes(512, 128)))
	if err != nil {
		t.Fatalf("topology past the control bound: %v", err)
	}
	if len(wide) <= MaxControlBytes || len(wide) > MaxSnapshotBytes {
		t.Fatalf("topology frame is %d bytes, want between %d and %d", len(wide), MaxControlBytes, MaxSnapshotBytes)
	}
	frame, err := DecodeServerControl(wide)
	if err != nil || len(frame.Body.(Topology).Nodes) != 512 {
		t.Fatalf("wide topology decode: %+v, %v", frame, err)
	}
	// The larger bound belongs to the server direction alone: a browser never
	// sends this frame and cannot send one this size at all.
	if _, err := DecodeClientControl(wide); err != ErrOversized {
		t.Fatalf("topology crossed into the client decoder: %v", err)
	}
	if _, err := EncodeTopology("x", body(nodes(MaxSnapshotEntities, MaxTaskTitleBytes))); err != ErrOversized {
		t.Fatalf("oversized topology encoded: %v", err)
	}
	if _, err := DecodeServerControl(make([]byte, MaxSnapshotBytes+1)); err != ErrOversized {
		t.Fatalf("oversized frame decoded: %v", err)
	}
}

// The manifest fixture loop proves RUN_PATHS decodes and round-trips. This
// proves the bounds beside it: an agent with no live run is in no room.
func TestRunPathsBounds(t *testing.T) {
	agent := "02020202020202020202020202020202"
	run := "04040404040404040404040404040404"
	runPaths := func(runID, paths string) string {
		return `{"type":"RUN_PATHS","id":"x","body":{"agent_id":"` + agent + `","run_id":"` + runID + `","paths":[` + paths + `]}}`
	}
	if _, err := DecodeServerControl([]byte(runPaths("", ""))); err != nil {
		t.Fatalf("idle agent refused: %v", err)
	}
	rooms := make([]string, MaxJSONArray+1)
	for index := range rooms {
		rooms[index] = `"internal/kernel"`
	}
	for _, frame := range []string{
		runPaths("", `"internal/kernel"`),
		runPaths(run, `""`),
		runPaths(run, strings.Join(rooms, ",")),
		`{"type":"RUN_PATHS","id":"x","body":{"agent_id":"` + agent + `","run_id":"` + run + `","paths":null}}`,
	} {
		if _, err := DecodeServerControl([]byte(frame)); err != ErrMalformed {
			t.Fatalf("%s accepted: %v", frame, err)
		}
	}
}
