package browserprotocol

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func fixturePath(name string) string {
	return filepath.Join("..", "..", "protocol", "browser", "fixtures", name)
}

func fixtureBytes(t *testing.T, name string) []byte {
	t.Helper()
	value, err := os.ReadFile(fixturePath(name))
	if err != nil {
		t.Fatal(err)
	}
	return bytes.TrimSpace(value)
}

func TestControlFixturesRoundTrip(t *testing.T) {
	tests := []struct {
		name string
		body func() ([]byte, error)
	}{
		{"hello", func() ([]byte, error) {
			return EncodeHello(Hello{DaemonID: "000102030405060708090a0b0c0d0e0f", BootID: "101112131415161718191a1b1c1d1e1f", ConnectionNonce: "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f"})
		}},
		{"pair_prove", func() ([]byte, error) {
			return EncodePairProve("pair-1", PairProve{Challenge: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f", PublicKeySEC1: "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5", Signature: "7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978590880bbb870c3d990948ae0a125780fb7d3551078d00dd7d2effd32273fd853"})
		}},
		{"pair_result", func() ([]byte, error) {
			return EncodePairResult("pair-1", PairResult{ClientID: "606162636465666768696a6b6c6d6e6f", Capabilities: 15})
		}},
		{"auth_prove", func() ([]byte, error) {
			return EncodeAuthProve("auth-1", AuthProve{ClientID: "606162636465666768696a6b6c6d6e6f", Signature: "7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc476699787e2963d01c3c5aa8d7cbb5fcf666c7a16e892b0e889805d2a547e76b4450ee80"})
		}},
		{"auth_result", func() ([]byte, error) {
			return EncodeAuthResult("auth-1", AuthResult{ClientID: "606162636465666768696a6b6c6d6e6f", Capabilities: 9})
		}},
		{"error", func() ([]byte, error) { return EncodeError("", Error{Code: ErrorUnauthorized}) }},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, err := test.body()
			if err != nil {
				t.Fatal(err)
			}
			want := fixtureBytes(t, test.name+".json")
			if !bytes.Equal(got, want) {
				t.Fatalf("fixture drift:\n got %s\nwant %s", got, want)
			}
			decoded, err := decodeFixtureControl(test.name, got)
			if err != nil {
				t.Fatal(err)
			}
			again, err := encodeDecoded(decoded)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(again, want) {
				t.Fatalf("round-trip drift:\n got %s\nwant %s", again, want)
			}
		})
	}
}

func decodeFixtureControl(name string, data []byte) (ControlFrame, error) {
	switch name {
	case "pair_prove", "auth_prove", "state_get", "state_watch", "human_request_detail_get", "task_enqueue", "terminal_target_get":
		return DecodeClientControl(data)
	default:
		return DecodeServerControl(data)
	}
}

func encodeDecoded(frame ControlFrame) ([]byte, error) {
	switch value := frame.Body.(type) {
	case Hello:
		return EncodeHello(value)
	case PairProve:
		return EncodePairProve(frame.ID, value)
	case PairResult:
		return EncodePairResult(frame.ID, value)
	case AuthProve:
		return EncodeAuthProve(frame.ID, value)
	case AuthResult:
		return EncodeAuthResult(frame.ID, value)
	case StateGet:
		return EncodeStateGet(frame.ID, value)
	case StateSnapshot:
		return EncodeStateSnapshot(frame.ID, value)
	case StateWatch:
		return EncodeStateWatch(frame.ID, value)
	case StateChanged:
		return EncodeStateChanged(frame.ID, value)
	case HumanRequestDetailGet:
		return EncodeHumanRequestDetailGet(frame.ID, value)
	case HumanRequestDetail:
		return EncodeHumanRequestDetail(frame.ID, value)
	case HumanRequestReply:
		return encodeControl(TypeHumanRequestReply, frame.ID, value)
	case HumanRequestReplyResult:
		return encodeControl(TypeHumanRequestReplyResult, frame.ID, value)
	case HumanRequestCancelRun:
		return encodeControl(TypeHumanRequestCancelRun, frame.ID, value)
	case HumanRequestCancelRunResult:
		return encodeControl(TypeHumanRequestCancelRunResult, frame.ID, value)
	case TaskEnqueue:
		return EncodeTaskEnqueue(frame.ID, value)
	case TaskEnqueueResult:
		return EncodeTaskEnqueueResult(frame.ID, value)
	case AgentUpdate:
		return encodeControl(TypeAgentUpdate, frame.ID, value)
	case AgentUpdateResult:
		return EncodeAgentUpdateResult(frame.ID, value)
	case TaskUpdate:
		return encodeControl(TypeTaskUpdate, frame.ID, value)
	case TaskUpdateResult:
		return EncodeTaskUpdateResult(frame.ID, value)
	case TopologyGet:
		return encodeControl(TypeTopologyGet, frame.ID, value)
	case Topology:
		return EncodeTopology(frame.ID, value)
	case RunPathsGet:
		return encodeControl(TypeRunPathsGet, frame.ID, value)
	case RunPaths:
		return EncodeRunPaths(frame.ID, value)
	case TerminalTargetGet:
		return EncodeTerminalTargetGet(frame.ID, value)
	case TerminalTarget:
		return EncodeTerminalTarget(frame.ID, value)
	case TerminalAttach:
		return encodeControl(TypeTerminalAttach, frame.ID, value)
	case TerminalAttached:
		return encodeControl(TypeTerminalAttached, frame.ID, value)
	case TerminalAck:
		return encodeControl(TypeTerminalAck, frame.ID, value)
	case TerminalLeaseAcquire:
		return encodeControl(TypeTerminalLeaseAcquire, frame.ID, value)
	case TerminalLeaseRenew:
		return encodeControl(frame.Type, frame.ID, value)
	case TerminalLeaseRelease:
		return EncodeTerminalLeaseRelease(frame.ID, value)
	case TerminalLeaseResult:
		return encodeControl(TypeTerminalLeaseResult, frame.ID, value)
	case TerminalResize:
		return encodeControl(TypeTerminalResize, frame.ID, value)
	case TerminalResized:
		return encodeControl(TypeTerminalResized, frame.ID, value)
	case TerminalDetach:
		return encodeControl(frame.Type, frame.ID, value)
	case TerminalDetached:
		return EncodeTerminalDetached(frame.ID, value)
	case TerminalInputResult:
		return encodeControl(TypeTerminalInputResult, frame.ID, value)
	case TerminalEOF:
		return encodeControl(TypeTerminalEOF, frame.ID, value)
	case TerminalExit:
		return encodeControl(TypeTerminalExit, frame.ID, value)
	case TerminalReset:
		return encodeControl(TypeTerminalReset, frame.ID, value)
	case RemoteInvite:
		return EncodeRemoteInvite(frame.ID, value)
	case RemoteInviteResult:
		return EncodeRemoteInviteResult(frame.ID, value)
	case Error:
		return EncodeError(frame.ID, value)
	default:
		return nil, errors.New("unknown decoded body")
	}
}

func TestBinaryFixturesRoundTrip(t *testing.T) {
	for _, name := range []string{"terminal_input.hex", "terminal_output.hex"} {
		t.Run(name, func(t *testing.T) {
			want, err := hex.DecodeString(string(fixtureBytes(t, name)))
			if err != nil {
				t.Fatal(err)
			}
			frame, err := DecodeTerminalFrame(want)
			if err != nil {
				t.Fatal(err)
			}
			got, err := encodeTerminalFrame(frame)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(got, want) {
				t.Fatalf("binary fixture drift: %x != %x", got, want)
			}
		})
	}
}

func TestControlMalformed(t *testing.T) {
	valid := string(fixtureBytes(t, "hello.json"))
	deep := `{"type":"HELLO","body":{"daemon_id":"` + strings.Repeat("00", 16) + `","boot_id":"` + strings.Repeat("11", 16) + `","connection_nonce":"` + strings.Repeat("22", 32) + `","x":[` + strings.Repeat("[", MaxJSONDepth+2) + "0" + strings.Repeat("]", MaxJSONDepth+2) + `]}}`
	cases := []struct{ name, data string }{
		{"unknown type", strings.Replace(valid, `"HELLO"`, `"NOPE"`, 1)},
		{"missing body", `{"type":"HELLO"}`},
		{"missing body member", strings.Replace(valid, `"boot_id"`, `"future_id"`, 1)},
		{"duplicate envelope", strings.Replace(valid, `{"type"`, `{"type":"HELLO","type"`, 1)},
		{"duplicate body", strings.Replace(valid, `,"boot_id"`, `,"daemon_id":"`+strings.Repeat("00", 16)+`","boot_id"`, 1)},
		{"trailing", valid + ` {}`},
		{"depth bound", deep},
		{"upper hex", strings.Replace(valid, "000102030405060708090a0b0c0d0e0f", "000102030405060708090A0b0c0d0e0f", 1)},
		{"bad id", strings.Replace(valid, `"type":"HELLO"`, `"type":"HELLO","id":"bad id"`, 1)},
		{"hello id", strings.Replace(valid, `"type":"HELLO"`, `"type":"HELLO","id":"x"`, 1)},
		{"error missing retryable", `{"type":"ERROR","body":{"code":"unauthorized"}}`},
		// encoding/json would match these onto the exact member they shadow.
		{"envelope case collision", strings.Replace(valid, `{"type"`, `{"TYPE":"NOPE","type"`, 1)},
		{"body case collision", strings.Replace(valid, `,"boot_id"`, `,"DAEMON_ID":"`+strings.Repeat("ff", 16)+`","boot_id"`, 1)},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			if _, err := DecodeServerControl([]byte(test.data)); err != ErrMalformed {
				t.Fatalf("malformed error = %v, want exact ErrMalformed", err)
			}
		})
	}
	if err := func() error { _, err := DecodeServerControl(append([]byte(valid), 0xff)); return err }(); err != ErrMalformed {
		t.Fatalf("invalid UTF-8 error = %v", err)
	}
	// A server frame may reach the snapshot bound; a client frame may not.
	if _, err := DecodeServerControl(bytes.Repeat([]byte{' '}, MaxSnapshotBytes+1)); !errors.Is(err, ErrOversized) {
		t.Fatalf("server oversize error = %v", err)
	}
	if _, err := DecodeClientControl(bytes.Repeat([]byte{' '}, MaxControlBytes+1)); !errors.Is(err, ErrOversized) {
		t.Fatalf("client oversize error = %v", err)
	}
}

// assertIgnoredMember proves an added member cannot change what a frame means.
// The mutated frame decodes to exactly the frame the unmutated one decodes to,
// so the extra bytes reach no field and become no authority.
func assertIgnoredMember(t *testing.T, base, mutated string, server bool) {
	t.Helper()
	decode := DecodeClientControl
	if server {
		decode = DecodeServerControl
	}
	want, err := decode([]byte(base))
	if err != nil {
		t.Fatalf("base frame rejected: %v", err)
	}
	got, err := decode([]byte(mutated))
	if err != nil {
		t.Fatalf("frame with an added member rejected: %v", err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("added member changed the decoded frame:\n got %+v\nwant %+v", got, want)
	}
}

// The contract tolerates additive change: a peer may add a member this build
// does not know without a coordinated release. Bounds, required members, type
// validation, frame types and directions stay exactly as strict as before.
func TestControlIgnoresUnknownMembers(t *testing.T) {
	watch := string(fixtureBytes(t, "state_watch.json"))
	withUnknownEnvelope := strings.Replace(watch, `,"body"`, `,"future":{"nested":[1,2,3]},"body"`, 1)
	withUnknownBody := strings.Replace(watch, `}}`, `,"future":"ignored"}}`, 1)
	withBoth := strings.Replace(withUnknownEnvelope, `}}`, `,"future":"ignored"}}`, 1)
	want, err := DecodeClientControl([]byte(watch))
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct{ name, data string }{
		{"unknown envelope member", withUnknownEnvelope},
		{"unknown body member", withUnknownBody},
		{"both", withBoth},
	} {
		t.Run(test.name, func(t *testing.T) {
			frame, err := DecodeClientControl([]byte(test.data))
			if err != nil {
				t.Fatalf("tolerant decode failed: %v", err)
			}
			// The unknown member is ignored, not surfaced: the decoded body is
			// exactly the closed struct this build knows.
			if !reflect.DeepEqual(frame, want) {
				t.Fatalf("decoded %+v, want %+v", frame, want)
			}
		})
	}
	// Tolerance is additive only. Size still binds on the same frame.
	padded := strings.Replace(watch, `,"body"`, `,"future":"`+strings.Repeat("x", MaxControlBytes)+`","body"`, 1)
	if _, err := DecodeClientControl([]byte(padded)); !errors.Is(err, ErrOversized) {
		t.Fatalf("oversized tolerant frame error = %v, want ErrOversized", err)
	}
	// So does the object-member bound: an unknown member is still a member.
	members := make([]string, 0, MaxJSONObject)
	for i := 0; i < MaxJSONObject; i++ {
		members = append(members, fmt.Sprintf("%q:%d", fmt.Sprintf("future_%d", i), i))
	}
	crowded := strings.Replace(watch, `}}`, `,`+strings.Join(members, ",")+`}}`, 1)
	if _, err := DecodeClientControl([]byte(crowded)); err != ErrMalformed {
		t.Fatalf("object-bound tolerant frame error = %v, want ErrMalformed", err)
	}
	// And the array bound: a tolerated member is not an unmeasured hole. A
	// client frame keeps the small array bound wherever the array appears.
	wide := strings.Replace(watch, `}}`, `,"future":[`+strings.TrimSuffix(strings.Repeat("0,", MaxJSONArray+1), ",")+`]}}`, 1)
	if _, err := DecodeClientControl([]byte(wide)); err != ErrMalformed {
		t.Fatalf("array-bound tolerant frame error = %v, want ErrMalformed", err)
	}
	// And the depth bound.
	nested := strings.Replace(watch, `}}`, `,"future":`+strings.Repeat("[", MaxJSONDepth+2)+"0"+strings.Repeat("]", MaxJSONDepth+2)+`}}`, 1)
	if _, err := DecodeClientControl([]byte(nested)); err != ErrMalformed {
		t.Fatalf("depth-bound tolerant frame error = %v, want ErrMalformed", err)
	}
}

// encoding/json matches a struct field case-insensitively when no exact name
// matches, so a member differing only in case from a known one is not unknown
// to the decoder even though the shape check would skip it. Left alone, that
// member would overwrite the exact member this package validated, and the
// TypeScript decoder -- whose keys are exact -- would read the frame
// differently. Both sides refuse it instead.
func TestCaseVariantOfAKnownMemberIsRefused(t *testing.T) {
	exact := strings.Repeat("00", 16)
	shadow := strings.Repeat("ff", 16)
	valid := `{"type":"HELLO","body":{"daemon_id":"` + exact + `","boot_id":"` + strings.Repeat("11", 16) + `","connection_nonce":"` + strings.Repeat("22", 32) + `"}}`
	frame, err := DecodeServerControl([]byte(valid))
	if err != nil {
		t.Fatal(err)
	}
	if frame.Body.(Hello).DaemonID != exact {
		t.Fatalf("baseline daemon_id = %q", frame.Body.(Hello).DaemonID)
	}
	for name, mutated := range map[string]string{
		"upper":       strings.Replace(valid, `"boot_id"`, `"DAEMON_ID":"`+shadow+`","boot_id"`, 1),
		"mixed":       strings.Replace(valid, `"boot_id"`, `"Daemon_Id":"`+shadow+`","boot_id"`, 1),
		"before":      strings.Replace(valid, `"daemon_id"`, `"DAEMON_ID":"`+shadow+`","daemon_id"`, 1),
		"no exact":    strings.Replace(valid, `"daemon_id"`, `"DAEMON_ID"`, 1),
		"envelope":    strings.Replace(valid, `{"type"`, `{"Type":"NOPE","type"`, 1),
		"nested item": strings.Replace(valid, `"boot_id"`, `"Connection_Nonce":"`+strings.Repeat("33", 32)+`","boot_id"`, 1),
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := DecodeServerControl([]byte(mutated)); err != ErrMalformed {
				t.Fatalf("case variant accepted: %v", err)
			}
		})
	}
	// A long s folds onto s under Go's Unicode fold, so `ſession_id` would have
	// reached session_id; a non-ASCII name is refused before that can happen.
	attached := string(fixtureBytes(t, "terminal_attached.json"))
	folded := strings.Replace(attached, `"floor"`, `"\u017fession_id":"`+strings.Repeat("ee", 16)+`","floor"`, 1)
	if _, err := DecodeServerControl([]byte(folded)); err != ErrMalformed {
		t.Fatalf("unicode fold variant accepted: %v", err)
	}
	// An ASCII name that is unknown under every case is still ignored.
	tolerated := strings.Replace(valid, `"boot_id"`, `"FUTURE_ID":"`+shadow+`","boot_id"`, 1)
	assertIgnoredMember(t, valid, tolerated, true)
}

// An unknown frame type and a frame sent in the wrong direction stay finite
// refusals; only members are tolerated.
func TestControlRefusesUnknownTypesAndWrongDirection(t *testing.T) {
	watch := string(fixtureBytes(t, "state_watch.json"))
	if _, err := DecodeClientControl([]byte(strings.Replace(watch, `"STATE_WATCH"`, `"STATE_FUTURE"`, 1))); err != ErrMalformed {
		t.Fatalf("unknown type error = %v, want ErrMalformed", err)
	}
	// A known client frame carrying an unknown member still cannot cross into
	// the server decoder.
	if _, err := DecodeServerControl([]byte(strings.Replace(watch, `}}`, `,"future":1}}`, 1))); err != ErrMalformed {
		t.Fatalf("wrong-direction error = %v, want ErrMalformed", err)
	}
	snapshot := string(fixtureBytes(t, "state_changed.json"))
	if _, err := DecodeClientControl([]byte(snapshot)); err != ErrMalformed {
		t.Fatalf("server frame accepted by client decoder: %v", err)
	}
}

// AUTH_PROVE has no public-key member at all: the daemon reads the key from
// its durable client row. A caller-supplied one is ignored, never adopted.
func TestAuthProveIgnoresCallerSuppliedPublicKey(t *testing.T) {
	valid := string(fixtureBytes(t, "auth_prove.json"))
	withKey := strings.Replace(valid, `"signature"`, `"public_key_sec1":"046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5","signature"`, 1)
	frame, err := DecodeClientControl([]byte(withKey))
	if err != nil {
		t.Fatalf("AUTH_PROVE with an extra member: %v", err)
	}
	body, ok := frame.Body.(AuthProve)
	if !ok {
		t.Fatalf("AUTH_PROVE body type %T", frame.Body)
	}
	expected, err := DecodeClientControl([]byte(valid))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(body, expected.Body) {
		t.Fatalf("AUTH_PROVE body %+v, want %+v", body, expected.Body)
	}
	if strings.Contains(fmt.Sprintf("%+v", body), "046b17d1") {
		t.Fatal("AUTH_PROVE carried a caller-supplied public key")
	}
}

func TestControlRoleAndIDPresence(t *testing.T) {
	serverOnly := fixtureBytes(t, "hello.json")
	clientOnly := fixtureBytes(t, "pair_prove.json")
	if _, err := DecodeClientControl(serverOnly); err != ErrMalformed {
		t.Fatalf("client accepted HELLO: %v", err)
	}
	if _, err := DecodeServerControl(clientOnly); err != ErrMalformed {
		t.Fatalf("server accepted PAIR_PROVE: %v", err)
	}
	for _, name := range []string{"hello.json", "error.json"} {
		valid := string(fixtureBytes(t, name))
		for _, id := range []string{`null`, `""`, `1`} {
			candidate := strings.Replace(valid, `,"body"`, `,"id":`+id+`,"body"`, 1)
			if _, err := DecodeServerControl([]byte(candidate)); err != ErrMalformed {
				t.Fatalf("%s explicit id %s accepted: %v", name, id, err)
			}
		}
	}
	validPair := string(clientOnly)
	for _, replacement := range []string{`"id":null`, `"id":""`, `"id":1`} {
		candidate := strings.Replace(validPair, `"id":"pair-1"`, replacement, 1)
		if _, err := DecodeClientControl([]byte(candidate)); err != ErrMalformed {
			t.Fatalf("PAIR_PROVE id %s accepted: %v", replacement, err)
		}
	}
	withoutID := strings.Replace(validPair, `,"id":"pair-1"`, "", 1)
	if _, err := DecodeClientControl([]byte(withoutID)); err != ErrMalformed {
		t.Fatalf("PAIR_PROVE omitted id accepted: %v", err)
	}
	if _, err := DecodeServerControl(fixtureBytes(t, "error.json")); err != nil {
		t.Fatalf("ERROR omitted id rejected: %v", err)
	}
}

func TestControlIDAndBodyValidation(t *testing.T) {
	goodID := "x"
	if _, err := EncodePairResult(goodID, PairResult{ClientID: strings.Repeat("60", 16), Capabilities: CapabilityObserve}); err != nil {
		t.Fatal(err)
	}
	for _, id := range []string{"", strings.Repeat("x", 65), "x y", "é", "\x00"} {
		if _, err := EncodePairResult(id, PairResult{ClientID: strings.Repeat("60", 16), Capabilities: CapabilityObserve}); err == nil {
			t.Fatalf("invalid id %q accepted", id)
		}
	}
	for _, caps := range []Capabilities{0, 16, 0xffffffff} {
		if _, err := EncodePairResult("x", PairResult{ClientID: strings.Repeat("60", 16), Capabilities: caps}); err == nil {
			t.Fatalf("invalid capability %d accepted", caps)
		}
	}
	if _, err := EncodeError("x", Error{Code: "private-sentinel"}); err == nil {
		t.Fatal("unknown error accepted")
	}
}

func TestManifestMatchesImplementedRegistry(t *testing.T) {
	data := fixtureBytes(t, "../manifest.json")
	var manifest struct {
		Name         string `json:"name"`
		Capabilities []struct {
			Name  string `json:"name"`
			Bit   int    `json:"bit"`
			Value byte   `json:"value"`
		} `json:"capabilities"`
		Bounds struct {
			MaxControlBytes              int    `json:"max_control_bytes"`
			MaxJSONDepth                 int    `json:"max_json_depth"`
			MaxArrayItems                int    `json:"max_array_items"`
			MaxObjectMembers             int    `json:"max_object_members"`
			MaxSnapshotBytes             int    `json:"max_snapshot_bytes"`
			MaxSnapshotEntities          int    `json:"max_snapshot_entities"`
			MaxProjectNameBytes          int    `json:"max_project_name_bytes"`
			MaxAgentNameBytes            int    `json:"max_agent_name_bytes"`
			MaxTaskTitleBytes            int    `json:"max_task_title_bytes"`
			MaxHumanQuestionBytes        int    `json:"max_human_question_bytes"`
			MaxHumanReplyBytes           int    `json:"max_human_reply_bytes"`
			MaxTaskInstructionBytes      int    `json:"max_task_instruction_bytes"`
			MaxFactoryCapacity           int    `json:"max_factory_capacity"`
			MaxTaskPriority              int64  `json:"max_task_priority"`
			MaxSQLiteInteger             string `json:"max_sqlite_integer"`
			MaxTerminalUnackedBytes      int    `json:"max_terminal_unacked_bytes"`
			TerminalAckTimeoutMS         int    `json:"terminal_ack_timeout_ms"`
			TerminalLeaseRenewIntervalMS int    `json:"terminal_lease_renew_interval_ms"`
			MaxTerminalRows              int    `json:"max_terminal_rows"`
			MaxTerminalCols              int    `json:"max_terminal_cols"`
			MaxAgentModelBytes           int    `json:"max_agent_model_bytes"`
			MaxRemoteInviteLinkBytes     int    `json:"max_remote_invite_link_bytes"`
			MaxRemoteInviteSVGBytes      int    `json:"max_remote_invite_svg_bytes"`
		} `json:"bounds"`
		Control []struct {
			Type      string `json:"type"`
			Direction string `json:"direction"`
			ID        string `json:"id"`
			Fixture   string `json:"fixture"`
		} `json:"control"`
		Terminal struct {
			Magic       string `json:"magic"`
			Version     int    `json:"version"`
			HeaderBytes int    `json:"header_bytes"`
			MaxPayload  int    `json:"max_payload_bytes"`
			Opcodes     []struct {
				Name      string `json:"name"`
				Value     byte   `json:"value"`
				Direction string `json:"direction"`
				Fixture   string `json:"fixture"`
			} `json:"opcodes"`
		} `json:"terminal"`
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&manifest); err != nil {
		t.Fatal(err)
	}
	var trailing any
	if err := decoder.Decode(&trailing); err != io.EOF {
		t.Fatalf("manifest trailing JSON: %v", err)
	}
	// The manifest carries a stable name, not a generation: the contract is
	// unversioned by owner decision on 4 September 2026.
	if manifest.Name != "dark-factory/browser" || len(manifest.Control) != 45 || len(manifest.Terminal.Opcodes) != 2 {
		t.Fatalf("manifest registry incomplete: %+v", manifest)
	}
	capabilityNames := []string{"observe", "private_human_request_detail", "human_actions", "terminal_input"}
	capabilityValues := []byte{1, 2, 4, 8}
	if len(manifest.Capabilities) != len(capabilityNames) {
		t.Fatalf("capability registry size = %d", len(manifest.Capabilities))
	}
	for i, name := range capabilityNames {
		if manifest.Capabilities[i].Name != name || manifest.Capabilities[i].Bit != i || manifest.Capabilities[i].Value != capabilityValues[i] {
			t.Fatalf("capability[%d] drift: %+v", i, manifest.Capabilities[i])
		}
	}
	wantBounds := struct {
		MaxControlBytes, MaxJSONDepth, MaxArrayItems, MaxObjectMembers                                                int
		MaxSnapshotBytes, MaxSnapshotEntities                                                                         int
		MaxProjectNameBytes, MaxAgentNameBytes, MaxTaskTitleBytes                                                     int
		MaxHumanQuestionBytes, MaxHumanReplyBytes, MaxTaskInstructionBytes, MaxFactoryCapacity                        int
		MaxTaskPriority                                                                                               int64
		MaxSQLiteInteger                                                                                              string
		MaxTerminalUnackedBytes, TerminalAckTimeoutMS, TerminalLeaseRenewIntervalMS, MaxTerminalRows, MaxTerminalCols int
		MaxAgentModelBytes, MaxRemoteInviteLinkBytes, MaxRemoteInviteSVGBytes                                         int
	}{MaxControlBytes, MaxJSONDepth, MaxJSONArray, MaxJSONObject, MaxSnapshotBytes, MaxSnapshotEntities, MaxProjectNameBytes, MaxAgentNameBytes, MaxTaskTitleBytes, MaxHumanQuestionBytes, MaxHumanReplyBytes, MaxTaskInstructionBytes, MaxFactoryCapacity, MaxTaskPriority, fmt.Sprint(MaxSQLiteInteger), MaxTerminalUnackedBytes, TerminalAckTimeoutMS, TerminalLeaseRenewIntervalMS, int(MaxTerminalRows), int(MaxTerminalCols), MaxAgentModelBytes, MaxRemoteInviteLinkBytes, MaxRemoteInviteSVGBytes}
	if fmt.Sprint(manifest.Bounds) != fmt.Sprint(wantBounds) {
		t.Fatalf("bounds drift: got %+v want %+v", manifest.Bounds, wantBounds)
	}
	want := []struct{ name, direction, id, fixture string }{
		{"HELLO", "server", "forbidden", "hello.json"},
		{"PAIR_PROVE", "client", "required", "pair_prove.json"},
		{"PAIR_RESULT", "server", "required", "pair_result.json"},
		{"AUTH_PROVE", "client", "required", "auth_prove.json"},
		{"AUTH_RESULT", "server", "required", "auth_result.json"},
		{"STATE_GET", "client", "required", "state_get.json"},
		{"STATE_SNAPSHOT", "server", "required", "state_snapshot.json"},
		{"STATE_WATCH", "client", "required", "state_watch.json"},
		{"STATE_CHANGED", "server", "required", "state_changed.json"},
		{"HUMAN_REQUEST_DETAIL_GET", "client", "required", "human_request_detail_get.json"},
		{"HUMAN_REQUEST_DETAIL", "server", "required", "human_request_detail.json"},
		{"HUMAN_REQUEST_REPLY", "client", "required", "human_request_reply.json"},
		{"HUMAN_REQUEST_REPLY_RESULT", "server", "required", "human_request_reply_result.json"},
		{"HUMAN_REQUEST_CANCEL_RUN", "client", "required", "human_request_cancel_run.json"},
		{"HUMAN_REQUEST_CANCEL_RUN_RESULT", "server", "required", "human_request_cancel_run_result.json"},
		{"TASK_ENQUEUE", "client", "required", "task_enqueue.json"},
		{"TASK_ENQUEUE_RESULT", "server", "required", "task_enqueue_result.json"},
		{"TERMINAL_TARGET_GET", "client", "required", "terminal_target_get.json"},
		{"TERMINAL_TARGET", "server", "required", "terminal_target.json"},
		{"TERMINAL_ATTACH", "client", "required", "terminal_attach.json"},
		{"TERMINAL_ATTACHED", "server", "required", "terminal_attached.json"},
		{"TERMINAL_ACK", "client", "forbidden", "terminal_ack.json"},
		{"TERMINAL_LEASE_ACQUIRE", "client", "required", "terminal_lease_acquire.json"},
		{"TERMINAL_LEASE_RENEW", "client", "required", "terminal_lease_renew.json"},
		{"TERMINAL_LEASE_RELEASE", "client", "required", "terminal_lease_release.json"},
		{"TERMINAL_LEASE_RESULT", "server", "required", "terminal_lease_result.json"},
		{"TERMINAL_RESIZE", "client", "required", "terminal_resize.json"},
		{"TERMINAL_RESIZED", "server", "required", "terminal_resized.json"},
		{"TERMINAL_DETACH", "client", "required", "terminal_detach.json"},
		{"TERMINAL_DETACHED", "server", "required", "terminal_detached.json"},
		{"TERMINAL_INPUT_RESULT", "server", "required", "terminal_input_result.json"},
		{"TERMINAL_EOF", "server", "required", "terminal_eof.json"},
		{"TERMINAL_EXIT", "server", "required", "terminal_exit.json"},
		{"TERMINAL_RESET", "server", "required", "terminal_reset.json"},
		{"AGENT_UPDATE", "client", "required", "agent_update.json"},
		{"AGENT_UPDATE_RESULT", "server", "required", "agent_update_result.json"},
		{"TASK_UPDATE", "client", "required", "task_update.json"},
		{"TASK_UPDATE_RESULT", "server", "required", "task_update_result.json"},
		{"TOPOLOGY_GET", "client", "required", "topology_get.json"},
		{"TOPOLOGY", "server", "required", "topology.json"},
		{"RUN_PATHS_GET", "client", "required", "run_paths_get.json"},
		{"RUN_PATHS", "server", "required", "run_paths.json"},
		{"REMOTE_INVITE", "client", "required", "remote_invite.json"},
		{"REMOTE_INVITE_RESULT", "server", "required", "remote_invite_result.json"},
		{"ERROR", "both", "optional", "error.json"},
	}
	seenFixtures := make(map[string]bool, len(want))
	for i, expected := range want {
		actual := manifest.Control[i]
		if actual.Type != expected.name || actual.Direction != expected.direction || actual.ID != expected.id || actual.Fixture != expected.fixture {
			t.Fatalf("control[%d] drift: %+v", i, actual)
		}
		if seenFixtures[actual.Fixture] {
			t.Fatalf("duplicate control fixture %q", actual.Fixture)
		}
		seenFixtures[actual.Fixture] = true
		data := fixtureBytes(t, actual.Fixture)
		frame, err := decodeRole(actual.Direction, data)
		if err != nil {
			t.Fatalf("%s fixture rejected: %v", actual.Type, err)
		}
		if frame.Type != MessageType(actual.Type) {
			t.Fatalf("fixture type %q, want %q", frame.Type, actual.Type)
		}
		if encoded, err := encodeDecoded(frame); err != nil || !bytes.Equal(encoded, data) {
			t.Fatalf("%s fixture does not canonical round-trip", actual.Type)
		}
		if actual.Direction == "both" {
			if _, err := DecodeClientControl(data); err != nil {
				t.Fatalf("%s client decode: %v", actual.Type, err)
			}
			if _, err := DecodeServerControl(data); err != nil {
				t.Fatalf("%s server decode: %v", actual.Type, err)
			}
		} else if actual.Direction == "client" {
			if _, err := DecodeServerControl(data); err != ErrMalformed {
				t.Fatalf("%s crossed into server decoder: %v", actual.Type, err)
			}
		} else if actual.Direction == "server" {
			if _, err := DecodeClientControl(data); err != ErrMalformed {
				t.Fatalf("%s crossed into client decoder: %v", actual.Type, err)
			}
		}
	}
	if manifest.Terminal.Magic != "DF" || manifest.Terminal.Version != 1 || manifest.Terminal.HeaderBytes != TerminalHeaderSize || manifest.Terminal.MaxPayload != MaxTerminalPayload {
		t.Fatal("binary manifest drift")
	}
	wantOpcodes := []struct {
		name               string
		value              byte
		direction, fixture string
	}{
		{"TERMINAL_INPUT", byte(TerminalInputOpcode), "client", "terminal_input.hex"},
		{"TERMINAL_OUTPUT", byte(TerminalOutputOpcode), "server", "terminal_output.hex"},
	}
	for i, expected := range wantOpcodes {
		actual := manifest.Terminal.Opcodes[i]
		if actual.Name != expected.name || actual.Value != expected.value || actual.Direction != expected.direction || actual.Fixture != expected.fixture {
			t.Fatalf("opcode[%d] drift: %+v", i, actual)
		}
		value, err := hex.DecodeString(string(fixtureBytes(t, actual.Fixture)))
		if err != nil {
			t.Fatal(err)
		}
		frame, err := DecodeTerminalFrame(value)
		if err != nil || byte(frame.Opcode) != actual.Value {
			t.Fatalf("%s fixture mismatch: %v", actual.Name, err)
		}
		encoded, err := encodeTerminalFrame(frame)
		if err != nil || !bytes.Equal(encoded, value) {
			t.Fatalf("%s fixture is not canonical", actual.Name)
		}
	}
	entries, err := os.ReadDir(filepath.Dir(fixturePath("hello.json")))
	if err != nil {
		t.Fatal(err)
	}
	expectedFiles := map[string]bool{"transcript.json": true, "hello.json": true, "pair_prove.json": true, "pair_result.json": true, "auth_prove.json": true, "auth_result.json": true, "state_get.json": true, "state_snapshot.json": true, "state_watch.json": true, "state_changed.json": true, "human_request_detail_get.json": true, "human_request_detail.json": true, "error.json": true, "terminal_input.hex": true, "terminal_output.hex": true, "human_request_reply.json": true, "human_request_reply_result.json": true, "human_request_cancel_run.json": true, "human_request_cancel_run_result.json": true, "task_enqueue.json": true, "task_enqueue_result.json": true, "terminal_target_get.json": true, "terminal_target.json": true, "terminal_attach.json": true, "terminal_attached.json": true, "terminal_ack.json": true, "terminal_lease_acquire.json": true, "terminal_lease_renew.json": true, "terminal_lease_release.json": true, "terminal_lease_result.json": true, "terminal_resize.json": true, "terminal_resized.json": true, "terminal_detach.json": true, "terminal_detached.json": true, "terminal_input_result.json": true, "terminal_eof.json": true, "terminal_exit.json": true, "terminal_reset.json": true, "agent_update.json": true, "agent_update_result.json": true, "task_update.json": true, "task_update_result.json": true, "topology_get.json": true, "topology.json": true, "remote_invite.json": true, "remote_invite_result.json": true, "run_paths_get.json": true, "run_paths.json": true}
	if len(entries) != len(expectedFiles) {
		t.Fatalf("fixture count = %d, want %d", len(entries), len(expectedFiles))
	}
	for _, entry := range entries {
		if entry.IsDir() || !expectedFiles[entry.Name()] {
			t.Fatalf("unexpected fixture %q", entry.Name())
		}
		delete(expectedFiles, entry.Name())
	}
	if len(expectedFiles) != 0 {
		t.Fatalf("missing fixtures: %v", expectedFiles)
	}
	for _, entry := range manifest.Control {
		if !seenFixtures[entry.Fixture] {
			t.Fatalf("manifest fixture not exercised: %q", entry.Fixture)
		}
	}
	for _, name := range []string{"hello", "pair_prove", "pair_result", "auth_prove", "auth_result", "state_get", "state_snapshot", "state_watch", "state_changed", "human_request_detail_get", "human_request_detail", "task_enqueue", "task_enqueue_result", "error"} {
		if len(fixtureBytes(t, name+".json")) == 0 {
			t.Fatal("empty fixture")
		}
	}
}

func decodeRole(direction string, data []byte) (ControlFrame, error) {
	if direction == "client" {
		return DecodeClientControl(data)
	}
	return DecodeServerControl(data)
}

func TestBinaryMalformed(t *testing.T) {
	valid, err := hex.DecodeString(string(fixtureBytes(t, "terminal_input.hex")))
	if err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		name   string
		mutate func([]byte)
	}{
		{"short", func(v []byte) { _ = v[:TerminalHeaderSize-1] }},
		{"magic", func(v []byte) { v[0] = 'X' }},
		{"version", func(v []byte) { v[2] = 2 }},
		{"opcode", func(v []byte) { v[3] = 9 }},
		{"zero session", func(v []byte) {
			for i := 4; i < 20; i++ {
				v[i] = 0
			}
		}},
		{"zero input sequence", func(v []byte) {
			for i := 20; i < 28; i++ {
				v[i] = 0
			}
		}},
		{"zero input generation", func(v []byte) {
			for i := 28; i < 36; i++ {
				v[i] = 0
			}
		}},
		{"wrong length", func(v []byte) { v[39]++ }},
		{"zero payload", func(v []byte) { v[39] = 0; v = v[:TerminalHeaderSize] }},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			value := append([]byte(nil), valid...)
			if test.name == "short" {
				if _, err := DecodeTerminalFrame(value[:TerminalHeaderSize-1]); err == nil {
					t.Fatal("short accepted")
				}
				return
			}
			test.mutate(value)
			if _, err := DecodeTerminalFrame(value); err != ErrMalformed {
				t.Fatalf("malformed error = %v, want exact ErrMalformed", err)
			}
		})
	}
	output, err := hex.DecodeString(string(fixtureBytes(t, "terminal_output.hex")))
	if err != nil {
		t.Fatal(err)
	}
	output[35] = 1
	if _, err := DecodeTerminalFrame(output); err == nil {
		t.Fatal("output generation accepted")
	}
	tooLarge := make([]byte, TerminalHeaderSize+MaxTerminalPayload+1)
	tooLarge[0], tooLarge[1], tooLarge[2], tooLarge[3] = 'D', 'F', 1, byte(TerminalOutputOpcode)
	if _, err := DecodeTerminalFrame(tooLarge); err == nil {
		t.Fatal("oversize accepted")
	}
	if _, err := EncodeTerminalInput([16]byte{1}, 1, 1, bytes.Repeat([]byte{'x'}, MaxTerminalPayload+1)); err == nil {
		t.Fatal("oversize encode accepted")
	}
}

func TestBinaryPayloadBoundary(t *testing.T) {
	id := [16]byte{1}
	payload := bytes.Repeat([]byte{'x'}, MaxTerminalPayload)
	input, err := EncodeTerminalInput(id, 1, 1, payload)
	if err != nil {
		t.Fatal(err)
	}
	output, err := EncodeTerminalOutput(id, 1, payload)
	if err != nil {
		t.Fatal(err)
	}
	for _, value := range [][]byte{input, output} {
		frame, err := DecodeTerminalFrame(value)
		if err != nil {
			t.Fatal(err)
		}
		if len(frame.Payload) != MaxTerminalPayload {
			t.Fatalf("payload length = %d, want %d", len(frame.Payload), MaxTerminalPayload)
		}
	}
	if _, err := EncodeTerminalInput(id, 1, 1, append(payload, 'x')); err == nil {
		t.Fatal("8193-byte input accepted")
	}
	if _, err := EncodeTerminalOutput(id, 1, append(payload, 'x')); err == nil {
		t.Fatal("8193-byte output accepted")
	}
}

func TestBinaryMutationGuards(t *testing.T) {
	id := [16]byte{1}
	if _, err := EncodeTerminalOutput(id, 1, []byte("x")); err != nil {
		t.Fatal(err)
	}
	if _, err := EncodeTerminalInput(id, 1, 1, []byte("x")); err != nil {
		t.Fatal(err)
	}
	// These are the causal checks that kill the common guard mutations:
	// accepting output generation, skipping payload-length equality, and
	// accepting a zero input sequence/generation.
	output, _ := EncodeTerminalOutput(id, 1, []byte("x"))
	output[35] = 1
	if _, err := DecodeTerminalFrame(output); err == nil {
		t.Fatal("output generation mutation survived")
	}
	input, _ := EncodeTerminalInput(id, 1, 1, []byte("x"))
	input[39] = 2
	if _, err := DecodeTerminalFrame(input); err == nil {
		t.Fatal("payload-length mutation survived")
	}
	input, _ = EncodeTerminalInput(id, 1, 1, []byte("x"))
	input[27] = 0
	for i := 20; i < 28; i++ {
		input[i] = 0
	}
	if _, err := DecodeTerminalFrame(input); err == nil {
		t.Fatal("zero sequence mutation survived")
	}
}

func TestBinaryOutputRangeOverflow(t *testing.T) {
	id := [16]byte{1}
	max := ^uint64(0)
	for _, size := range []int{1, MaxTerminalPayload} {
		payload := bytes.Repeat([]byte{'x'}, size)
		start := max - uint64(size)
		encoded, err := EncodeTerminalOutput(id, start, payload)
		if err != nil {
			t.Fatalf("max representable output rejected for %d bytes: %v", size, err)
		}
		if _, err := DecodeTerminalFrame(encoded); err != nil {
			t.Fatalf("max representable output decode failed for %d bytes: %v", size, err)
		}
		if _, err := EncodeTerminalOutput(id, start+1, payload); err != ErrMalformed {
			t.Fatalf("overflow output error = %v", err)
		}
		encoded[20] = 0xff
		for i := 21; i < 28; i++ {
			encoded[i] = 0xff
		}
		if _, err := DecodeTerminalFrame(encoded); err != ErrMalformed {
			t.Fatalf("wire overflow output error = %v", err)
		}
	}
}

func TestDecodeErrorsAreStable(t *testing.T) {
	_, err := DecodeServerControl([]byte(`{"type":"NOPE","body":{}}`))
	if err != ErrMalformed {
		t.Fatalf("error = %v, want exact ErrMalformed", err)
	}
	if strings.Contains(fmt.Sprint(err), "private") {
		t.Fatal("private text in public error")
	}
	malformed := [][]byte{
		[]byte(`{"type":"HELLO","id":null,"body":{}}`),
		[]byte(`{"type":"HELLO","body":{"daemon_id":"private-sentinel"}}`),
		// Tolerated members do not soften the refusal or the error: this frame
		// is missing connection_nonce, and the ignored member's text must not
		// reach the public error either.
		[]byte(`{"type":"HELLO","body":{"daemon_id":"` + strings.Repeat("00", 16) + `","boot_id":"` + strings.Repeat("11", 16) + `","private":"sentinel"}}`),
	}
	for _, value := range malformed {
		err := func() error { _, err := DecodeServerControl(value); return err }()
		if err != ErrMalformed || strings.Contains(fmt.Sprint(err), "private") || strings.Contains(fmt.Sprint(err), "daemon_id") {
			t.Fatalf("malformed public error leaked details: %v", err)
		}
	}
	if err := func() error { _, err := DecodeTerminalFrame([]byte("bad")); return err }(); err != ErrMalformed {
		t.Fatalf("binary public error = %v", err)
	}
}

func TestTerminalControlRejectsSurrogateAndNullMutations(t *testing.T) {
	reply := string(fixtureBytes(t, "human_request_reply.json"))
	for _, escape := range []string{`\ud800`, `\udc00`, `\udc00\ud800`, `\ud800\u0041`} {
		mutated := strings.Replace(reply, `"reply":"ok"`, `"reply":"`+escape+`"`, 1)
		if _, err := DecodeClientControl([]byte(mutated)); err != ErrMalformed {
			t.Fatalf("reply escape %q accepted: %v", escape, err)
		}
	}
	paired := strings.Replace(reply, `"reply":"ok"`, `"reply":"\ud83d\ude00"`, 1)
	frame, err := DecodeClientControl([]byte(paired))
	if err != nil || frame.Body.(HumanRequestReply).Reply != "😀" {
		t.Fatalf("paired surrogate rejected or changed: frame=%+v err=%v", frame, err)
	}

	lease := string(fixtureBytes(t, "terminal_lease_result.json"))
	for _, mutated := range []string{
		strings.Replace(lease, `"expires_at_ms":"10000"`, `"expires_at_ms":null`, 1),
		strings.Replace(lease, `,"expires_at_ms":"10000"`, ``, 1),
		strings.Replace(lease, `"expires_at_ms":"10000"`, `"expires_at_ms":"0"`, 1),
		strings.Replace(strings.Replace(lease, `"operation":"acquired"`, `"operation":"released"`, 1), `,"expires_at_ms":"10000"`, ``, 1),
	} {
		if strings.Contains(mutated, `"operation":"released"`) {
			if _, err := DecodeServerControl([]byte(mutated)); err != nil {
				t.Fatalf("released lease without expiry rejected: %v", err)
			}
			continue
		}
		if _, err := DecodeServerControl([]byte(mutated)); err != ErrMalformed {
			t.Fatalf("null lease expiry accepted: %v", err)
		}
	}
	for _, field := range []string{"exit_code", "exit_signal", "aborted"} {
		exit := strings.Replace(string(fixtureBytes(t, "terminal_exit.json")), `"`+field+`":`+map[string]string{"exit_code": "0", "exit_signal": "0", "aborted": "false"}[field], `"`+field+`":null`, 1)
		if _, err := DecodeServerControl([]byte(exit)); err != ErrMalformed {
			t.Fatalf("null terminal exit %s accepted: %v", field, err)
		}
	}
}

func TestTerminalExitRequiresOneCanonicalStatusArm(t *testing.T) {
	const sessionID = "22222222222222222222222222222222"
	for _, test := range []struct {
		name         string
		code, signal int64
	}{
		{name: "success", code: 0, signal: 0},
		{name: "failure", code: 7, signal: 0},
		{name: "signal", code: 0, signal: 15},
	} {
		t.Run(test.name, func(t *testing.T) {
			payload, err := EncodeTerminalExit("exit", TerminalExit{SessionID: sessionID, ExitCode: test.code, ExitSignal: test.signal})
			if err != nil {
				t.Fatal(err)
			}
			frame, err := DecodeServerControl(payload)
			if err != nil {
				t.Fatal(err)
			}
			got := frame.Body.(TerminalExit)
			if got.ExitCode != test.code || got.ExitSignal != test.signal {
				t.Fatalf("exit = code %d signal %d", got.ExitCode, got.ExitSignal)
			}
		})
	}

	for _, test := range []struct {
		name         string
		code, signal int64
	}{
		{name: "negative code", code: -1, signal: 0},
		{name: "runner sentinel", code: -1, signal: 15},
		{name: "contradictory", code: 7, signal: 9},
		{name: "negative signal", code: 0, signal: -1},
		{name: "code overflow", code: MaxJSONInteger + 1, signal: 0},
		{name: "signal overflow", code: 0, signal: MaxJSONInteger + 1},
	} {
		t.Run(test.name, func(t *testing.T) {
			if _, err := EncodeTerminalExit("exit", TerminalExit{SessionID: sessionID, ExitCode: test.code, ExitSignal: test.signal}); !errors.Is(err, ErrMalformed) {
				t.Fatalf("noncanonical terminal exit accepted: %v", err)
			}
			wire := fmt.Sprintf(`{"type":"TERMINAL_EXIT","id":"exit","body":{"session_id":"%s","exit_code":%d,"exit_signal":%d,"aborted":false}}`, sessionID, test.code, test.signal)
			if _, err := DecodeServerControl([]byte(wire)); !errors.Is(err, ErrMalformed) {
				t.Fatalf("noncanonical terminal exit decoded: %v", err)
			}
		})
	}
}

func TestHumanRequestAuthorityWireRejectsLegacyAndForgedShapes(t *testing.T) {
	reply := string(fixtureBytes(t, "human_request_reply.json"))
	cancel := string(fixtureBytes(t, "human_request_cancel_run.json"))
	detail := string(fixtureBytes(t, "human_request_detail.json"))
	result := string(fixtureBytes(t, "human_request_cancel_run_result.json"))
	// A reply or cancellation has no run destination member at all, so a
	// caller-supplied one reaches nothing: the frame decodes to exactly the
	// frame without it and the Store still derives the origin.
	assertIgnoredMember(t, reply, strings.Replace(reply, `"request_id":`, `"run_id":"11111111111111111111111111111111","request_id":`, 1), false)
	assertIgnoredMember(t, cancel, strings.Replace(cancel, `"request_id":`, `"run_id":"11111111111111111111111111111111","request_id":`, 1), false)
	for name, test := range map[string]struct {
		wire   string
		server bool
	}{
		"cancel field case":     {wire: strings.Replace(cancel, `"expected_run_revision"`, `"Expected_Run_Revision"`, 1)},
		"detail missing target": {wire: strings.Replace(detail, `,"terminal_target":{"run_id":"11111111111111111111111111111111","session_id":"22222222222222222222222222222222","run_revision":"8","session_revision":"9"}`, ``, 1), server: true},
	} {
		t.Run(name, func(t *testing.T) {
			var err error
			if test.server {
				_, err = DecodeServerControl([]byte(test.wire))
			} else {
				_, err = DecodeClientControl([]byte(test.wire))
			}
			if err != ErrMalformed {
				t.Fatalf("legacy/forged authority accepted: %v", err)
			}
		})
	}
	legacyResult := strings.Replace(result, `"type":"HUMAN_REQUEST_CANCEL_RUN_RESULT"`, `"type":"HUMAN_REQUEST_ACTION_RESULT"`, 1)
	legacyResult = strings.Replace(legacyResult, `"body":{`, `"body":{"action":"cancel_run","status":"resolved",`, 1)
	if _, err := DecodeServerControl([]byte(legacyResult)); err != ErrMalformed {
		t.Fatalf("legacy generic result accepted: %v", err)
	}
	// Residue from the deleted generic result shape carries no meaning: it is
	// ignored, so the exact typed result is still the only authority.
	for _, mutation := range []string{
		strings.Replace(result, `"run_id":`, `"action":"cancel_run","run_id":`, 1),
		strings.Replace(result, `"request_revision":"2"`, `"request_revision":"2","status":"resolved"`, 1),
	} {
		assertIgnoredMember(t, result, mutation, true)
	}
}

func TestTerminalTargetContract(t *testing.T) {
	get := fixtureBytes(t, "terminal_target_get.json")
	request, err := DecodeClientControl(get)
	if err != nil {
		t.Fatal(err)
	}
	if request.Type != TypeTerminalTargetGet || request.ID != "target-get-1" {
		t.Fatalf("request = %+v", request)
	}
	if encoded, err := encodeDecoded(request); err != nil || !bytes.Equal(encoded, get) {
		t.Fatalf("request round trip: %v", err)
	}

	response := fixtureBytes(t, "terminal_target.json")
	frame, err := DecodeServerControl(response)
	if err != nil {
		t.Fatal(err)
	}
	value, ok := frame.Body.(TerminalTarget)
	if !ok || value.Target == nil || value.Head != 9007199254740993 {
		t.Fatalf("response = %+v", frame.Body)
	}
	if encoded, err := encodeDecoded(frame); err != nil || !bytes.Equal(encoded, response) {
		t.Fatalf("response round trip: %v", err)
	}

	unavailable := bytes.Replace(response, []byte(`"target":{"run_id":"11111111111111111111111111111111","session_id":"22222222222222222222222222222222","run_revision":"8","session_revision":"9"}`), []byte(`"target":null`), 1)
	if decoded, err := DecodeServerControl(unavailable); err != nil || decoded.Body.(TerminalTarget).Target != nil {
		t.Fatalf("null target = %+v, %v", decoded, err)
	}
	nullValue := TerminalTarget{AgentID: "02020202020202020202020202020202", AgentRevision: 7, Head: 9007199254740993}
	if encoded, err := EncodeTerminalTarget("target-1", nullValue); err != nil || !bytes.Equal(encoded, unavailable) {
		t.Fatalf("null target round trip: %v", err)
	}
	for _, mutation := range []string{
		strings.Replace(string(response), `"agent_revision":"7"`, `"agent_revision":"0"`, 1),
		strings.Replace(string(response), `"head":"9007199254740993"`, `"head":"01"`, 1),
		strings.Replace(string(response), `,"session_revision":"9"`, "", 1),
	} {
		if _, err := DecodeServerControl([]byte(mutation)); err != ErrMalformed {
			t.Fatalf("target mutation accepted: %v", err)
		}
	}
	// A terminal target is an observation coordinate with a closed shape. A
	// process identity added beside it is ignored, so it can never be read.
	assertIgnoredMember(t, string(response), strings.Replace(string(response), `"target":{`, `"target":{"pid":1,`, 1), true)
	leaseResult := string(fixtureBytes(t, "terminal_lease_result.json"))
	assertIgnoredMember(t, leaseResult, strings.Replace(leaseResult, `"expires_at_ms":"10000"`, `"expires_at_ms":"10000","renew_after_ms":"10000"`, 1), true)
	if _, err := DecodeClientControl(bytes.Replace(get, []byte(`"id":"target-get-1"`), nil, 1)); err != ErrMalformed {
		t.Fatalf("missing request id accepted: %v", err)
	}
}

func TestTerminalInputResultStatusAndCountContract(t *testing.T) {
	const id = "01010101010101010101010101010101"
	base := TerminalInputResult{SessionID: "22222222222222222222222222222222", Generation: 1, Sequence: 1}
	for _, test := range []struct {
		status string
		count  Decimal
	}{
		{status: "accepted", count: 1},
		{status: "accepted", count: MaxTerminalPayload},
		{status: "partial", count: 1},
		{status: "partial", count: MaxTerminalPayload},
		{status: "rejected", count: 0},
		{status: "uncertain", count: 0},
	} {
		body := base
		body.Status, body.AcceptedBytes = test.status, test.count
		wire, err := EncodeTerminalInputResult(id, body)
		if err != nil {
			t.Fatalf("valid %s/%d encode: %v", test.status, test.count, err)
		}
		if _, err := DecodeServerControl(wire); err != nil {
			t.Fatalf("valid %s/%d decode: %v", test.status, test.count, err)
		}
	}

	fixture := string(fixtureBytes(t, "terminal_input_result.json"))
	for _, test := range []struct {
		status string
		count  string
	}{
		{status: "accepted", count: "0"},
		{status: "rejected", count: "1"},
		{status: "partial", count: "0"},
		{status: "uncertain", count: "1"},
		{status: "accepted", count: "8193"},
	} {
		body := base
		body.Status = test.status
		body.AcceptedBytes = Decimal(0)
		if test.count != "0" {
			body.AcceptedBytes = Decimal(1)
		}
		if test.count == "8193" {
			body.AcceptedBytes = Decimal(MaxTerminalPayload + 1)
		}
		if _, err := EncodeTerminalInputResult(id, body); !errors.Is(err, ErrMalformed) {
			t.Fatalf("invalid %s/%s encode accepted: %v", test.status, test.count, err)
		}
		wire := strings.Replace(fixture, `"status":"accepted"`, `"status":"`+test.status+`"`, 1)
		wire = strings.Replace(wire, `"accepted_bytes":"2"`, `"accepted_bytes":"`+test.count+`"`, 1)
		if _, err := DecodeServerControl([]byte(wire)); err != ErrMalformed {
			t.Fatalf("invalid %s/%s decode accepted: %v", test.status, test.count, err)
		}
	}
}
