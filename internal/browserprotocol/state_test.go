package browserprotocol

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"reflect"
	"strings"
	"testing"
	"unicode/utf8"
)

const (
	projectID       = "01010101010101010101010101010101"
	agentID         = "02020202020202020202020202020202"
	taskID          = "03030303030303030303030303030303"
	requestID       = "04040404040404040404040404040404"
	runID           = "05050505050505050505050505050505"
	accountID       = "06060606060606060606060606060606"
	accountHomePath = "/Users/operator/.codex"
)

func factoryItem() FactoryItem {
	return FactoryItem{DispatchEnabled: true, Capacity: 8, ActiveRuns: 2, Revision: 1}
}
func projectItem() ProjectItem {
	return ProjectItem{ID: projectID, Name: "Factory", RunBudgetLimit: 12, RunsUsed: 5, MaxRunSeconds: 900, Revision: 1}
}
func agentItem() AgentItem {
	return AgentItem{ID: agentID, ProjectID: projectID, Name: "Worker", Role: "worker", Provider: "claude_code", Revision: 1}
}
func taskItem() TaskItem {
	return TaskItem{ID: taskID, ProjectID: projectID, AssignedAgentID: agentID, Title: "Ship", Status: "queued", Priority: 1, Revision: 1}
}
func humanRequestItem() HumanRequestItem {
	return HumanRequestItem{ID: requestID, ProjectID: projectID, AgentID: agentID, TaskID: taskID, CreatedAt: 10, UpdatedAt: 11, Revision: 1, Kind: "question", Status: "open", ReplyMaxBytes: MaxHumanReplyBytes, CanReply: true}
}

func accountItem() AccountItem {
	return AccountItem{ID: accountID, Provider: "codex", Home: accountHomePath, Label: "codex", Revision: 1}
}

// snapshotWith is one valid snapshot the individual bound tests mutate. The
// agent selects the account beside it, so the linked pair is the default the
// negative cases below break one member of at a time.
func snapshotWith() StateSnapshot {
	agent := agentItem()
	agent.Provider = "codex"
	agent.AccountID = accountID
	return StateSnapshot{
		Head: 1, Factory: factoryItem(),
		Projects: []ProjectItem{projectItem()}, Agents: []AgentItem{agent},
		Tasks: []TaskItem{taskItem()}, HumanRequests: []HumanRequestItem{humanRequestItem()},
		Accounts: []AccountItem{accountItem()},
	}
}

func hexIdentity(prefix byte, index int) string {
	raw := bytes.Repeat([]byte{prefix}, 16)
	raw[15] = byte(index % 251)
	raw[14] = byte(index / 251)
	return fmt.Sprintf("%x", raw)
}

func rawControl(t *testing.T, kind MessageType, id string, body any) []byte {
	t.Helper()
	bodyJSON, err := json.Marshal(body)
	if err != nil {
		t.Fatal(err)
	}
	idJSON, err := json.Marshal(id)
	if err != nil {
		t.Fatal(err)
	}
	result, err := json.Marshal(controlEnvelope{Type: kind, ID: idJSON, Body: bodyJSON})
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func TestStateFixturesRoundTrip(t *testing.T) {
	for _, name := range []string{"state_get", "state_snapshot", "state_watch", "state_changed", "human_request_detail_get", "human_request_detail"} {
		t.Run(name, func(t *testing.T) {
			wire := fixtureBytes(t, name+".json")
			frame, err := decodeFixtureControl(name, wire)
			if err != nil {
				t.Fatal(err)
			}
			encoded, err := encodeDecoded(frame)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(encoded, wire) {
				t.Fatalf("fixture drift:\n got %s\nwant %s", encoded, wire)
			}
		})
	}
	frame, err := DecodeServerControl(fixtureBytes(t, "state_snapshot.json"))
	if err != nil {
		t.Fatal(err)
	}
	snapshot := frame.Body.(StateSnapshot)
	if snapshot.Head != 9_007_199_254_740_993 {
		t.Fatalf("unsafe chronology truncated: %+v", snapshot)
	}
	// The shared fixture is the cross-language contract: it must carry one of
	// every public kind so a TypeScript consumer exercises the whole shape.
	if len(snapshot.Projects) != 1 || len(snapshot.Agents) != 1 || len(snapshot.Tasks) != 1 || len(snapshot.HumanRequests) != 1 {
		t.Fatalf("snapshot fixture does not cover every kind: %+v", snapshot)
	}
}

// The checked-in snapshot fixture is exactly what this Go encoder produces, so
// the fixture a TypeScript consumer reads cannot drift from the server.
func TestGoProducesTheSnapshotFixtureByteForByte(t *testing.T) {
	wire := fixtureBytes(t, "state_snapshot.json")
	frame, err := DecodeServerControl(wire)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := EncodeStateSnapshot(frame.ID, frame.Body.(StateSnapshot))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(encoded, wire) {
		t.Fatalf("Go-produced snapshot differs from the fixture:\n got %s\nwant %s", encoded, wire)
	}
}

// STATE_GET carries nothing. A body with any member is malformed, so a client
// cannot smuggle a selector past a server that no longer reads one.
func TestStateGetCarriesNoSelector(t *testing.T) {
	encoded, err := EncodeStateGet("state", StateGet{})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(encoded, []byte(`"body":{}`)) {
		t.Fatalf("STATE_GET body is not empty: %s", encoded)
	}
	if _, err := DecodeClientControl(encoded); err != nil {
		t.Fatal(err)
	}
	// STATE_GET has no selector member, so a caller cannot smuggle one: each
	// of these decodes to exactly the same empty request.
	for _, body := range []string{`{"cursor":null}`, `{"kind":"task"}`, `{"after":"1"}`} {
		wire := fmt.Sprintf(`{"type":"STATE_GET","id":"state","body":%s}`, body)
		assertIgnoredMember(t, string(encoded), wire, false)
	}
}

// The snapshot entity bound is exact and fails closed. Nothing is trimmed.
func TestSnapshotEntityCountBoundFailsClosed(t *testing.T) {
	fill := func(total int) StateSnapshot {
		value := StateSnapshot{Head: 1, Factory: factoryItem(), Projects: []ProjectItem{}, Agents: []AgentItem{}, Tasks: []TaskItem{}, HumanRequests: []HumanRequestItem{}}
		for index := 1; index <= total; index++ {
			item := projectItem()
			item.ID = hexIdentity(0xa1, index)
			value.Projects = append(value.Projects, item)
		}
		return value
	}
	if _, err := EncodeStateSnapshot("state", fill(MaxSnapshotEntities-1)); err != nil {
		t.Fatalf("maximum snapshot rejected: %v", err)
	}
	if _, err := EncodeStateSnapshot("state", fill(MaxSnapshotEntities)); !errors.Is(err, ErrMalformed) {
		t.Fatalf("oversized snapshot accepted: %v", err)
	}
	// The bound counts every collection plus the factory together.
	spread := StateSnapshot{Head: 1, Factory: factoryItem(), Projects: []ProjectItem{}, Agents: []AgentItem{}, Tasks: []TaskItem{}, HumanRequests: []HumanRequestItem{}}
	for index := 1; index <= MaxSnapshotEntities/2; index++ {
		project := projectItem()
		project.ID = hexIdentity(0xa1, index)
		agent := agentItem()
		agent.ID = hexIdentity(0xa2, index)
		spread.Projects = append(spread.Projects, project)
		spread.Agents = append(spread.Agents, agent)
	}
	if _, err := EncodeStateSnapshot("state", spread); !errors.Is(err, ErrMalformed) {
		t.Fatalf("spread overflow accepted: %v", err)
	}
	// Accounts count toward the same bound as everything else.
	accounts := StateSnapshot{Head: 1, Factory: factoryItem(), Projects: spread.Projects, Agents: []AgentItem{}, Tasks: []TaskItem{}, HumanRequests: []HumanRequestItem{}}
	for index := 1; index <= MaxSnapshotEntities/2; index++ {
		item := accountItem()
		item.ID = hexIdentity(0xa4, index)
		accounts.Accounts = append(accounts.Accounts, item)
	}
	if _, err := EncodeStateSnapshot("state", accounts); !errors.Is(err, ErrMalformed) {
		t.Fatalf("account overflow accepted: %v", err)
	}
}

// One encoded snapshot may reach 1 MiB and not a byte more. Oversize is a
// finite refusal on both sides; nothing is truncated or partially published.
func TestSnapshotByteBoundIsOneMebibyteAndFailsClosed(t *testing.T) {
	value := StateSnapshot{Head: 1, Factory: factoryItem(), Projects: []ProjectItem{}, Agents: []AgentItem{}, Tasks: []TaskItem{}, HumanRequests: []HumanRequestItem{}}
	for index := 1; len(value.Tasks) < MaxSnapshotEntities-2; index++ {
		item := taskItem()
		item.ID = hexIdentity(0xa3, index)
		item.Title = strings.Repeat("x", MaxTaskTitleBytes)
		value.Tasks = append(value.Tasks, item)
	}
	if _, err := EncodeStateSnapshot("state", value); !errors.Is(err, ErrOversized) {
		t.Fatalf("oversized encoded snapshot accepted: %v", err)
	}
	small := snapshotWith()
	encoded, err := EncodeStateSnapshot("state", small)
	if err != nil {
		t.Fatal(err)
	}
	if len(encoded) > MaxSnapshotBytes {
		t.Fatalf("ordinary snapshot is %d bytes", len(encoded))
	}
	// Every other frame in either direction stays inside the control bound.
	if MaxSnapshotBytes != 1<<20 || MaxControlBytes != 64<<10 || controlLimit(TypeStateWatch) != MaxControlBytes || controlLimit(TypeStateSnapshot) != MaxSnapshotBytes {
		t.Fatalf("frame bounds drifted: snapshot=%d control=%d", MaxSnapshotBytes, MaxControlBytes)
	}
	oversizedClient := append([]byte(nil), bytes.Repeat([]byte{' '}, MaxControlBytes+1)...)
	if _, err := DecodeClientControl(oversizedClient); !errors.Is(err, ErrOversized) {
		t.Fatalf("oversized client frame error = %v", err)
	}
	// A server frame that is not a snapshot may not use the snapshot bound.
	watch := fmt.Sprintf(`{"type":"STATE_CHANGED","id":"%s","body":{"head":"1"}}`, strings.Repeat("p", 64))
	padded := append([]byte(watch), bytes.Repeat([]byte{' '}, MaxControlBytes)...)
	if _, err := DecodeServerControl(padded); !errors.Is(err, ErrOversized) && err != ErrMalformed {
		t.Fatalf("oversized non-snapshot server frame error = %v", err)
	}
}

// Identities are unique per collection. A duplicate would let a server hide
// one entity behind another in a map-shaped client.
func TestSnapshotRejectsDuplicateIdentities(t *testing.T) {
	value := snapshotWith()
	value.Projects = append(value.Projects, projectItem())
	if _, err := EncodeStateSnapshot("state", value); !errors.Is(err, ErrMalformed) {
		t.Fatalf("duplicate project accepted: %v", err)
	}
	tasks := snapshotWith()
	tasks.Tasks = append(tasks.Tasks, taskItem())
	if _, err := EncodeStateSnapshot("state", tasks); !errors.Is(err, ErrMalformed) {
		t.Fatalf("duplicate task accepted: %v", err)
	}
}

func TestDecimalCanonicalBoundaries(t *testing.T) {
	valid := []uint64{0, 1, (1 << 53) - 1, 1 << 53, (1 << 53) + 1, math.MaxInt64}
	for _, value := range valid {
		encoded := strconvUint(value)
		parsed, err := parseDecimal(encoded)
		if err != nil || uint64(parsed) != value {
			t.Fatalf("decimal %s: parsed=%d err=%v", encoded, parsed, err)
		}
		wire, err := EncodeStateWatch("watch", StateWatch{AfterHead: Decimal(value)})
		if err != nil {
			t.Fatal(err)
		}
		frame, err := DecodeClientControl(wire)
		if err != nil || uint64(frame.Body.(StateWatch).AfterHead) != value {
			t.Fatalf("wire decimal %s: %+v %v", encoded, frame, err)
		}
	}
	for _, value := range []string{"", "+1", "-1", "-0", "00", "01", " 1", "1 ", "9223372036854775808", "18446744073709551615", "1.0", "1e0"} {
		if _, err := parseDecimal(value); err == nil {
			t.Fatalf("invalid decimal %q accepted", value)
		}
		wire := fmt.Sprintf(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":%q}}`, value)
		if _, err := DecodeClientControl([]byte(wire)); err != ErrMalformed {
			t.Fatalf("wire decimal %q accepted: %v", value, err)
		}
	}
	for _, number := range []string{"0", "1", "9007199254740991", "9007199254740992", "9223372036854775807", "-1"} {
		wire := fmt.Sprintf(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":%s}}`, number)
		if _, err := DecodeClientControl([]byte(wire)); err != ErrMalformed {
			t.Fatalf("JSON number chronology %s accepted: %v", number, err)
		}
	}
	if _, err := EncodeStateWatch("watch", StateWatch{AfterHead: Decimal(MaxSQLiteInteger + 1)}); !errors.Is(err, ErrMalformed) {
		t.Fatalf("overflow encode error = %v", err)
	}
}

func strconvUint(value uint64) string { return fmt.Sprintf("%d", value) }

// STATE_CHANGED is a bare head. A zero head is not a change, and the frame has
// no room for entity, revision or tombstone data.
func TestStateChangedIsAHeadAndNothingElse(t *testing.T) {
	wire, err := EncodeStateChanged("watch", StateChanged{Head: 9})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(wire, []byte(fmt.Sprintf(`{"type":"STATE_CHANGED","id":"watch","body":{"head":"9"}}`))) {
		t.Fatalf("STATE_CHANGED shape = %s", wire)
	}
	if _, err := EncodeStateChanged("watch", StateChanged{Head: 0}); !errors.Is(err, ErrMalformed) {
		t.Fatalf("zero head accepted: %v", err)
	}
	// STATE_CHANGED carries a head and nothing else. Residue from the deleted
	// per-entity taxonomy reaches no field, so it changes nothing.
	for _, body := range []string{
		`{"head":"9","entity_kind":"task"}`,
		`{"head":"9","revision":"2"}`,
		`{"head":"9","deleted":false}`,
		`{"head":"9","sequence":"9"}`,
	} {
		frame := fmt.Sprintf(`{"type":"STATE_CHANGED","id":"watch","body":%s}`, body)
		assertIgnoredMember(t, string(wire), frame, true)
	}
	// The head itself stays required.
	if _, err := DecodeServerControl([]byte(fmt.Sprintf(`{"type":"STATE_CHANGED","id":"watch","body":{}}`))); err != ErrMalformed {
		t.Fatalf("STATE_CHANGED without a head accepted: %v", err)
	}
}

func TestStateExactFieldAndEnumBounds(t *testing.T) {
	badFactories := []FactoryItem{
		{Capacity: 0, Revision: 1},
		{Capacity: MaxFactoryCapacity + 1, Revision: 1},
		{Capacity: 1, ActiveRuns: 3, Revision: 1},
		{Capacity: 1, Revision: 0},
	}
	for _, item := range badFactories {
		value := snapshotWith()
		value.Factory = item
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("invalid factory accepted: %+v", item)
		}
	}
	badProjects := []ProjectItem{{ID: projectID, Name: "", Revision: 1}, {ID: projectID, Name: strings.Repeat("x", MaxProjectNameBytes+1), Revision: 1}, {ID: strings.Repeat("AB", 16), Name: "x", Revision: 1}, {ID: projectID, Name: "x", Revision: 0}}
	for _, item := range badProjects {
		value := snapshotWith()
		value.Projects = []ProjectItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("invalid project accepted: %+v", item)
		}
	}
	badAgent := agentItem()
	badAgent.Role = "manager"
	value := snapshotWith()
	value.Agents = []AgentItem{badAgent}
	if _, err := EncodeStateSnapshot("x", value); err == nil {
		t.Fatal("unknown agent role accepted")
	}
	for _, provider := range []string{"", "claude", "CODEX", "bash"} {
		item := agentItem()
		item.Provider = provider
		value := snapshotWith()
		value.Agents = []AgentItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("agent provider %q accepted", provider)
		}
	}
	for _, status := range []string{"", "ready", "OPEN"} {
		item := taskItem()
		item.Status = status
		value := snapshotWith()
		value.Tasks = []TaskItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("task status %q accepted", status)
		}
	}
	for _, priority := range []int64{-MaxTaskPriority - 1, MaxTaskPriority + 1} {
		item := taskItem()
		item.Priority = priority
		value := snapshotWith()
		value.Tasks = []TaskItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("priority %d accepted", priority)
		}
	}
	oversizedTitle := taskItem()
	oversizedTitle.Title = strings.Repeat("x", MaxTaskTitleBytes+1)
	value = snapshotWith()
	value.Tasks = []TaskItem{oversizedTitle}
	if _, err := EncodeStateSnapshot("x", value); err == nil {
		t.Fatal("oversized task title accepted")
	}
	for _, edit := range []func(*HumanRequestItem){
		func(v *HumanRequestItem) { v.Kind = "reply" },
		func(v *HumanRequestItem) { v.Status = "resolved" },
		func(v *HumanRequestItem) { v.UpdatedAt = v.CreatedAt - 1 },
		func(v *HumanRequestItem) { v.ReplyMaxBytes = 0 },
		func(v *HumanRequestItem) { v.ReplyMaxBytes = MaxHumanReplyBytes + 1 },
	} {
		item := humanRequestItem()
		edit(&item)
		value := snapshotWith()
		value.HumanRequests = []HumanRequestItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("invalid human request accepted: %+v", item)
		}
	}
}

// There is no factory identity on the wire any more: the factory is one
// embedded object. Every remaining identity is a nonzero dynamic 16-byte hex.
func TestDynamicEntityIdentitiesAreClosed(t *testing.T) {
	for _, id := range []string{"factory", strings.Repeat("0", 32), strings.Repeat("AB", 16), "", projectID + "00"} {
		item := projectItem()
		item.ID = id
		value := snapshotWith()
		value.Projects = []ProjectItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("invalid identity %q accepted", id)
		}
		if _, err := EncodeHumanRequestDetailGet("x", HumanRequestDetailGet{RequestID: id, ExpectedRevision: 1}); err == nil {
			t.Fatalf("invalid detail identity %q accepted", id)
		}
	}
}

func TestStateBooleanNullIsAlwaysRejected(t *testing.T) {
	serverFrames := []struct {
		name  string
		frame []byte
		field string
		value string
	}{
		{"factory dispatch", mustEncodeStateSnapshot(t, snapshotWith()), "dispatch_enabled", "true"},
		{"agent paused", mustEncodeStateSnapshot(t, snapshotWith()), "paused", "false"},
		{"human can reply", mustEncodeStateSnapshot(t, snapshotWith()), "can_reply", "true"},
		{"error retryable", mustEncodeError(t, Error{Code: ErrorInvalidRequest}), "retryable", "false"},
	}
	for _, test := range serverFrames {
		t.Run(test.name, func(t *testing.T) {
			wire := strings.Replace(string(test.frame), fmt.Sprintf(`%q:%s`, test.field, test.value), fmt.Sprintf(`%q:null`, test.field), 1)
			if wire == string(test.frame) {
				t.Fatalf("test field %s not found in %s", test.field, test.frame)
			}
			if _, err := DecodeServerControl([]byte(wire)); err != ErrMalformed {
				t.Fatalf("null boolean accepted: %v", err)
			}
		})
	}
}

func mustEncodeStateSnapshot(t *testing.T, value StateSnapshot) []byte {
	t.Helper()
	wire, err := EncodeStateSnapshot("boolean", value)
	if err != nil {
		t.Fatal(err)
	}
	return wire
}

func mustEncodeError(t *testing.T, value Error) []byte {
	t.Helper()
	wire, err := EncodeError("boolean", value)
	if err != nil {
		t.Fatal(err)
	}
	return wire
}

func TestAgentItemServesExactlyThePublicFields(t *testing.T) {
	fields := reflect.VisibleFields(reflect.TypeOf(AgentItem{}))
	actual := make([]string, 0, len(fields))
	for _, field := range fields {
		actual = append(actual, field.Tag.Get("json"))
	}
	want := []string{"id", "project_id", "name", "role", "provider", "paused", "model", "reasoning_effort", "effective_model", "effective_reasoning_effort", "model_source", "revision", "account_id", "idle_policy", "idle_after_seconds", "idle_instruction", "idle_run_budget", "idle_runs_used"}
	if !reflect.DeepEqual(actual, want) {
		t.Fatalf("public AgentItem fields drifted: got %v want %v", actual, want)
	}
	for _, provider := range []string{"claude_code", "codex", "shell"} {
		item := agentItem()
		item.Provider = provider
		value := snapshotWith()
		value.Agents = []AgentItem{item}
		wire := mustEncodeStateSnapshot(t, value)
		frame, err := DecodeServerControl(wire)
		if err != nil {
			t.Fatalf("provider %q round-trip: %v", provider, err)
		}
		agents := frame.Body.(StateSnapshot).Agents
		if len(agents) != 1 || agents[0].Provider != provider {
			t.Fatalf("provider %q decoded as %+v", provider, agents)
		}
		// The launch controls the console edits are public by owner decision on
		// 5 September 2026; the budget columns beside them are still private.
		// model_source deliberately serves a filesystem path. It is the local
		// provider CLI's own configuration file, which the operator reading the
		// console already owns and can open; without it "inherited" is a claim
		// the console cannot attribute. No path outside the operator's account
		// reaches this field, and nothing inside those files is served.
		for _, private := range []string{`"tool_budget`, `"tool_calls`} {
			if bytes.Contains(wire, []byte(private)) {
				t.Fatalf("public agent item exposed %q: %s", private, wire)
			}
		}
	}
}

func TestHumanRequestPublicPrivacyAndDetailBounds(t *testing.T) {
	fields := reflect.VisibleFields(reflect.TypeOf(HumanRequestItem{}))
	actual := make([]string, 0, len(fields))
	for _, field := range fields {
		actual = append(actual, field.Tag.Get("json"))
	}
	want := []string{"id", "project_id", "agent_id", "task_id", "created_at", "updated_at", "revision", "kind", "status", "reply_max_bytes", "can_reply"}
	if !reflect.DeepEqual(actual, want) {
		t.Fatalf("public HumanRequest fields drifted: got %v want %v", actual, want)
	}
	encoded, err := EncodeStateSnapshot("state", snapshotWith())
	if err != nil {
		t.Fatal(err)
	}
	for _, private := range []string{`"run_id":`, `"question":`, `"reply":`, `"terminal_target":`, `"cancel_run":`, `"action":`, `"project_name":`, `"agent_name":`, `"task_title":`, `"summary":`, `"why_human_needed":`, `"root":`, `"instruction":`} {
		if bytes.Contains(encoded, []byte(private)) {
			t.Fatalf("public snapshot exposed private field %q: %s", private, encoded)
		}
	}
	for _, question := range []string{"x", strings.Repeat("x", MaxHumanQuestionBytes), strings.Repeat("\x00", MaxHumanQuestionBytes)} {
		if !utf8.ValidString(question) {
			t.Fatal("test question invalid")
		}
		wire, err := EncodeHumanRequestDetail("detail", HumanRequestDetail{RequestID: requestID, Revision: 1, Question: question, ReplyMaxBytes: MaxHumanReplyBytes})
		if err != nil {
			t.Fatal(err)
		}
		if len(wire) > MaxControlBytes {
			t.Fatalf("detail frame %d exceeds max", len(wire))
		}
	}
	for _, question := range []string{"", strings.Repeat("x", MaxHumanQuestionBytes+1), string([]byte{0xff})} {
		if _, err := EncodeHumanRequestDetail("detail", HumanRequestDetail{RequestID: requestID, Revision: 1, Question: question, ReplyMaxBytes: MaxHumanReplyBytes}); err == nil {
			t.Fatalf("invalid question accepted: %d bytes", len(question))
		}
	}
	if _, err := EncodeHumanRequestDetailGet("detail", HumanRequestDetailGet{RequestID: requestID}); err == nil {
		t.Fatal("zero expected revision accepted")
	}
	target := TerminalTargetDescriptor{RunID: "11111111111111111111111111111111", SessionID: "22222222222222222222222222222222", RunRevision: 8, SessionRevision: 9}
	cancelRun := HumanRequestCancelRunDescriptor{ExpectedRequestRevision: 2, ExpectedRunRevision: 8}
	authorized := HumanRequestDetail{RequestID: requestID, Revision: 2, Question: "choose", CanReply: true, ReplyMaxBytes: MaxHumanReplyBytes, TerminalTarget: &target, CancelRun: &cancelRun}
	if _, err := EncodeHumanRequestDetail("detail", authorized); err != nil {
		t.Fatalf("authorized detail = %v", err)
	}
	for name, mutate := range map[string]func(*HumanRequestDetail){
		"reply without target": func(value *HumanRequestDetail) { value.TerminalTarget = nil },
		"reply without cancel": func(value *HumanRequestDetail) { value.CancelRun = nil },
		"cancel without reply": func(value *HumanRequestDetail) { value.CanReply = false },
		"request revision":     func(value *HumanRequestDetail) { value.CancelRun.ExpectedRequestRevision++ },
		"run revision":         func(value *HumanRequestDetail) { value.CancelRun.ExpectedRunRevision++ },
	} {
		t.Run(name, func(t *testing.T) {
			value := authorized
			copiedTarget, copiedCancelRun := *authorized.TerminalTarget, *authorized.CancelRun
			value.TerminalTarget, value.CancelRun = &copiedTarget, &copiedCancelRun
			mutate(&value)
			if _, err := EncodeHumanRequestDetail("detail", value); err == nil {
				t.Fatalf("inconsistent detail accepted: %+v", value)
			}
		})
	}
}

// A hostile snapshot maximally escaped in JSON still has to fit the declared
// snapshot bound, and a maximal HumanRequest detail still fits the 64 KiB
// control bound.
func TestMaximallyEscapedFramesStayBounded(t *testing.T) {
	value := StateSnapshot{Head: Decimal(MaxSQLiteInteger), Factory: factoryItem(), Projects: []ProjectItem{}, Agents: []AgentItem{}, Tasks: []TaskItem{}, HumanRequests: []HumanRequestItem{}}
	for index := 1; index <= 8; index++ {
		item := taskItem()
		item.ID = hexIdentity(0xa3, index)
		item.Title = strings.Repeat("\x00", MaxTaskTitleBytes)
		value.Tasks = append(value.Tasks, item)
	}
	snapshot, err := EncodeStateSnapshot("max-snapshot", value)
	if err != nil {
		t.Fatal(err)
	}
	detail, err := EncodeHumanRequestDetail("max-detail", HumanRequestDetail{RequestID: requestID, Revision: Decimal(MaxSQLiteInteger), Question: strings.Repeat("\x00", MaxHumanQuestionBytes), ReplyMaxBytes: MaxHumanReplyBytes})
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot) < 49_000 || len(snapshot) > MaxSnapshotBytes || len(detail) < 49_000 || len(detail) > MaxControlBytes {
		t.Fatalf("unexpected maximal frame sizes: snapshot=%d detail=%d", len(snapshot), len(detail))
	}
	if _, err := DecodeServerControl(snapshot); err != nil {
		t.Fatalf("maximal snapshot did not decode: %v", err)
	}
	t.Logf("maximally escaped snapshot=%d bytes (limit %d), question detail=%d bytes (limit %d)", len(snapshot), MaxSnapshotBytes, len(detail), MaxControlBytes)
}

func TestStateMalformedEnvelopeBodyAndGlobalBounds(t *testing.T) {
	valid := string(fixtureBytes(t, "state_snapshot.json"))
	cases := []string{
		strings.Replace(valid, `"head"`, `"Head"`, 1),
		strings.Replace(valid, `"factory"`, `"Factory"`, 1),
		strings.Replace(valid, `"title"`, `"TITLE"`, 1),
		strings.Replace(valid, `"head":"9007199254740993"`, `"head":"9007199254740993","head":"2"`, 1),
		valid + `{}`,
	}
	// An added item member is tolerated but reaches no field: the decoded
	// snapshot is identical to the one without it.
	assertIgnoredMember(t, valid, strings.Replace(valid, `"title"`, `"extra":false,"title"`, 1), true)
	for _, wire := range cases {
		if _, err := DecodeServerControl([]byte(wire)); err != ErrMalformed {
			t.Fatalf("malformed state accepted: %v\n%s", err, wire)
		}
	}
	objectMembers := strings.Repeat(`"x":0,`, MaxJSONObject) + `"tail":0`
	tooManyMembers := fmt.Sprintf(`{"type":"STATE_WATCH","id":"watch","body":{%s}}`, objectMembers)
	if _, err := DecodeClientControl([]byte(tooManyMembers)); err != ErrMalformed {
		t.Fatalf("object member bound error = %v", err)
	}
	// A client frame keeps the small array bound; the server snapshot is the
	// only direction that may carry a large array.
	clientArray := fmt.Sprintf(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":[%s]}}`, strings.TrimSuffix(strings.Repeat(`0,`, MaxJSONArray+1), ","))
	if _, err := DecodeClientControl([]byte(clientArray)); err != ErrMalformed {
		t.Fatalf("client array bound error = %v", err)
	}
	tooManyArray := fmt.Sprintf(`{"type":"STATE_SNAPSHOT","id":"state","body":{"head":"1","factory":{},"projects":[%s],"agents":[],"tasks":[],"human_requests":[]}}`, strings.TrimSuffix(strings.Repeat(`{},`, MaxSnapshotEntities+1), ","))
	if _, err := DecodeServerControl([]byte(tooManyArray)); err != ErrMalformed {
		t.Fatalf("snapshot array bound error = %v", err)
	}
	if _, err := DecodeServerControl(append(fixtureBytes(t, "state_snapshot.json"), 0xff)); err != ErrMalformed {
		t.Fatalf("invalid UTF-8 error = %v", err)
	}
	if _, err := DecodeServerControl(bytes.Repeat([]byte{' '}, MaxSnapshotBytes+1)); !errors.Is(err, ErrOversized) {
		t.Fatalf("oversize error = %v", err)
	}
}

// A retired message type is refused as unsupported, so the deleted paging
// contract cannot come back through a frame this build does not implement. The
// envelope has no generation to refuse: an older client's `v` member is simply
// ignored.
func TestRetiredStateTypesAreRefused(t *testing.T) {
	legacy := `{"v":1,"type":"STATE_GET","id":"state","body":{"cursor":null}}`
	frame, err := DecodeClientControl([]byte(legacy))
	if err != nil {
		t.Fatalf("legacy envelope member refused: %v", err)
	}
	if frame.Type != TypeStateGet || frame.Body != (StateGet{}) {
		t.Fatalf("legacy frame decoded to %+v", frame)
	}
	for _, kind := range []string{"STATE_SUBSCRIBE", "STATE_ENTITY_GET"} {
		wire := fmt.Sprintf(`{"type":%q,"id":"x","body":{}}`, kind)
		if _, err := DecodeClientControl([]byte(wire)); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("retired client type %s accepted: %v", kind, err)
		}
	}
	for _, kind := range []string{"STATE_RESTART", "STATE_EVENT", "STATE_ENTITY"} {
		wire := fmt.Sprintf(`{"type":%q,"id":"x","body":{}}`, kind)
		if _, err := DecodeServerControl([]byte(wire)); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("retired server type %s accepted: %v", kind, err)
		}
	}
}

func TestErrorTooLargeIsFinite(t *testing.T) {
	wire, err := EncodeError("state", Error{Code: ErrorTooLarge})
	if err != nil {
		t.Fatal(err)
	}
	frame, err := DecodeServerControl(wire)
	if err != nil || frame.Body.(Error).Code != ErrorTooLarge {
		t.Fatalf("too_large round trip: %+v %v", frame, err)
	}
}

// Every account rule on the wire, one broken member at a time. Without these
// each clause could be deleted and every other gate would stay green.
func TestAccountWireRulesAreClosed(t *testing.T) {
	// An agent's account must be one canonical identity, and shell has no
	// logins at all, so it can never name one.
	for _, broken := range []struct {
		name             string
		provider, accoun string
	}{
		{"shell agent with an account", "shell", accountID},
		{"malformed account identity", "codex", "not-hex"},
		{"zero account identity", "codex", strings.Repeat("0", 32)},
		{"short account identity", "codex", strings.Repeat("ab", 8)},
	} {
		value := snapshotWith()
		item := agentItem()
		item.Provider, item.AccountID = broken.provider, broken.accoun
		value.Agents = []AgentItem{item}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("%s accepted", broken.name)
		}
	}
	// A shell agent with no account is still fine: the clause is about the
	// pairing, not about shell agents in a snapshot.
	value := snapshotWith()
	shell := agentItem()
	shell.Provider, shell.Model, shell.ReasoningEffort, shell.EffectiveModel, shell.EffectiveReasoningEffort = "shell", "", "", "", ""
	value.Agents = []AgentItem{shell}
	if _, err := EncodeStateSnapshot("x", value); err != nil {
		t.Fatalf("shell agent without an account refused: %v", err)
	}

	for _, broken := range []struct {
		name string
		item AccountItem
	}{
		{"relative home", AccountItem{ID: accountID, Provider: "codex", Home: "Users/operator/.codex", Label: "codex", Revision: 1}},
		{"empty home", AccountItem{ID: accountID, Provider: "codex", Home: "", Label: "codex", Revision: 1}},
		{"oversized home", AccountItem{ID: accountID, Provider: "codex", Home: "/" + strings.Repeat("d", MaxTaskTitleBytes), Label: "codex", Revision: 1}},
		{"home with a NUL", AccountItem{ID: accountID, Provider: "codex", Home: "/Users/operator/.co\x00dex", Label: "codex", Revision: 1}},
		{"shell provider", AccountItem{ID: accountID, Provider: "shell", Home: accountHomePath, Label: "codex", Revision: 1}},
		{"unknown provider", AccountItem{ID: accountID, Provider: "CODEX", Home: accountHomePath, Label: "codex", Revision: 1}},
		{"empty label", AccountItem{ID: accountID, Provider: "codex", Home: accountHomePath, Label: "", Revision: 1}},
		{"oversized label", AccountItem{ID: accountID, Provider: "codex", Home: accountHomePath, Label: strings.Repeat("l", MaxAgentNameBytes+1), Revision: 1}},
		{"malformed identity", AccountItem{ID: "nope", Provider: "codex", Home: accountHomePath, Label: "codex", Revision: 1}},
		{"zero revision", AccountItem{ID: accountID, Provider: "codex", Home: accountHomePath, Label: "codex", Revision: 0}},
	} {
		value := snapshotWith()
		value.Accounts = []AccountItem{broken.item}
		value.Agents = []AgentItem{agentItem()}
		if _, err := EncodeStateSnapshot("x", value); err == nil {
			t.Fatalf("account item %s accepted", broken.name)
		}
	}
	// Two accounts cannot share one identity, on the same terms as every
	// other collection in the snapshot.
	duplicate := snapshotWith()
	duplicate.Accounts = []AccountItem{accountItem(), accountItem()}
	if _, err := EncodeStateSnapshot("x", duplicate); err == nil {
		t.Fatal("duplicate account identity accepted")
	}
}

// ACCOUNTS and ACCOUNT_LINK carry the same closed rules, and no field on a
// discovered login is wide enough to smuggle a credential through.
func TestDiscoveredAccountAndLinkRulesAreClosed(t *testing.T) {
	good := DiscoveredAccount{Provider: "codex", Home: accountHomePath, Label: "codex", Email: "operator@example.com", DefaultModel: "gpt-6-astra", DefaultReasoningEffort: "high"}
	if _, err := EncodeAccounts("x", Accounts{Accounts: []DiscoveredAccount{good}}); err != nil {
		t.Fatalf("valid discovered account refused: %v", err)
	}
	token := strings.Repeat("t", MaxAgentNameBytes+1)
	for _, broken := range []struct {
		name   string
		mutate func(*DiscoveredAccount)
	}{
		{"shell provider", func(a *DiscoveredAccount) { a.Provider = "shell" }},
		{"relative home", func(a *DiscoveredAccount) { a.Home = "Users/operator/.codex" }},
		{"empty home", func(a *DiscoveredAccount) { a.Home = "" }},
		{"empty label", func(a *DiscoveredAccount) { a.Label = "" }},
		{"oversized label", func(a *DiscoveredAccount) { a.Label = token }},
		// A token-shaped value is exactly what must not fit in a display
		// field: the bound is the reason discovery drops one instead.
		{"token-sized email", func(a *DiscoveredAccount) { a.Email = token }},
		{"token-sized organization", func(a *DiscoveredAccount) { a.Organization = token }},
		{"token-sized default model", func(a *DiscoveredAccount) { a.DefaultModel = strings.Repeat("m", MaxAgentModelBytes+1) }},
		{"token-sized default effort", func(a *DiscoveredAccount) { a.DefaultReasoningEffort = strings.Repeat("e", MaxAgentModelBytes+1) }},
		{"malformed linked identity", func(a *DiscoveredAccount) { a.LinkedID = "nope" }},
	} {
		item := good
		broken.mutate(&item)
		if _, err := EncodeAccounts("x", Accounts{Accounts: []DiscoveredAccount{item}}); err == nil {
			t.Fatalf("discovered account %s accepted", broken.name)
		}
	}
	// More logins than one frame may carry is malformed, never a trimmed list.
	many := make([]DiscoveredAccount, MaxJSONArray+1)
	for index := range many {
		item := good
		item.Home = fmt.Sprintf("/Users/operator/.codex-%d", index)
		many[index] = item
	}
	if _, err := EncodeAccounts("x", Accounts{Accounts: many}); err == nil {
		t.Fatal("oversized discovery list accepted")
	}

	for _, broken := range []AccountLink{
		{Provider: "codex", Home: "Users/operator/.codex", Label: "codex"},
		{Provider: "codex", Home: "", Label: "codex"},
		{Provider: "shell", Home: accountHomePath, Label: "shell"},
		{Provider: "", Home: accountHomePath, Label: "codex"},
		{Provider: "codex", Home: accountHomePath, Label: ""},
		{Provider: "codex", Home: accountHomePath, Label: strings.Repeat("l", MaxAgentNameBytes+1)},
	} {
		if _, err := rawEncodeAccountLink(broken); err == nil {
			t.Fatalf("account link %+v accepted", broken)
		}
	}
	if _, err := rawEncodeAccountLink(AccountLink{Provider: "codex", Home: accountHomePath, Label: "codex"}); err != nil {
		t.Fatalf("valid account link refused: %v", err)
	}
	for _, broken := range []AccountLinkResult{{AccountID: "nope", Revision: 1}, {AccountID: accountID, Revision: 0}} {
		if _, err := EncodeAccountLinkResult("x", broken); err == nil {
			t.Fatalf("account link result %+v accepted", broken)
		}
	}
}

func rawEncodeAccountLink(value AccountLink) ([]byte, error) {
	return encodeControl(TypeAccountLink, "x", value)
}
