package browserprotocol

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"reflect"
	"strconv"
	"strings"
	"unicode/utf8"
)

// Adding a message is a contract change: add its manifest entry, fixture and
// explicit case in the role-specific decoders together.
const (
	MaxControlBytes = 64 << 10
	MaxJSONDepth    = 16
	MaxJSONArray    = 32
	MaxJSONObject   = 32
)

// controlLimit is the exact encoded bound for one message type. Client-to-
// server control stays at 64 KiB; only the server's whole-state snapshot may
// reach 1 MiB.
func controlLimit(kind MessageType) int {
	switch kind {
	case TypeStateSnapshot, TypeTopology:
		return MaxSnapshotBytes
	}
	return MaxControlBytes
}

type MessageType string

const (
	TypeHello                       MessageType = "HELLO"
	TypePairProve                   MessageType = "PAIR_PROVE"
	TypePairResult                  MessageType = "PAIR_RESULT"
	TypeAuthProve                   MessageType = "AUTH_PROVE"
	TypeAuthResult                  MessageType = "AUTH_RESULT"
	TypeStateGet                    MessageType = "STATE_GET"
	TypeStateSnapshot               MessageType = "STATE_SNAPSHOT"
	TypeStateWatch                  MessageType = "STATE_WATCH"
	TypeStateChanged                MessageType = "STATE_CHANGED"
	TypeHumanRequestDetailGet       MessageType = "HUMAN_REQUEST_DETAIL_GET"
	TypeHumanRequestDetail          MessageType = "HUMAN_REQUEST_DETAIL"
	TypeHumanRequestReply           MessageType = "HUMAN_REQUEST_REPLY"
	TypeHumanRequestReplyResult     MessageType = "HUMAN_REQUEST_REPLY_RESULT"
	TypeHumanRequestCancelRun       MessageType = "HUMAN_REQUEST_CANCEL_RUN"
	TypeHumanRequestCancelRunResult MessageType = "HUMAN_REQUEST_CANCEL_RUN_RESULT"
	TypeAgentControl                MessageType = "AGENT_CONTROL"
	TypeAgentControlResult          MessageType = "AGENT_CONTROL_RESULT"
	TypeTaskHistoryGet              MessageType = "TASK_HISTORY_GET"
	TypeTaskHistory                 MessageType = "TASK_HISTORY"
	TypeTaskEnqueue                 MessageType = "TASK_ENQUEUE"
	TypeTaskEnqueueResult           MessageType = "TASK_ENQUEUE_RESULT"
	TypeAgentUpdate                 MessageType = "AGENT_UPDATE"
	TypeAgentUpdateResult           MessageType = "AGENT_UPDATE_RESULT"
	TypeTaskUpdate                  MessageType = "TASK_UPDATE"
	TypeTaskUpdateResult            MessageType = "TASK_UPDATE_RESULT"
	TypeTopologyGet                 MessageType = "TOPOLOGY_GET"
	TypeTopology                    MessageType = "TOPOLOGY"
	TypeRunPathsGet                 MessageType = "RUN_PATHS_GET"
	TypeRunPaths                    MessageType = "RUN_PATHS"
	TypeAccountsDiscover            MessageType = "ACCOUNTS_DISCOVER"
	TypeAccounts                    MessageType = "ACCOUNTS"
	TypeAccountLink                 MessageType = "ACCOUNT_LINK"
	TypeAccountLinkResult           MessageType = "ACCOUNT_LINK_RESULT"
	TypeTerminalTargetGet           MessageType = "TERMINAL_TARGET_GET"
	TypeTerminalTarget              MessageType = "TERMINAL_TARGET"
	TypeTerminalAttach              MessageType = "TERMINAL_ATTACH"
	TypeTerminalAttached            MessageType = "TERMINAL_ATTACHED"
	TypeTerminalAck                 MessageType = "TERMINAL_ACK"
	TypeTerminalLeaseAcquire        MessageType = "TERMINAL_LEASE_ACQUIRE"
	TypeTerminalLeaseRenew          MessageType = "TERMINAL_LEASE_RENEW"
	TypeTerminalLeaseRelease        MessageType = "TERMINAL_LEASE_RELEASE"
	TypeTerminalLeaseResult         MessageType = "TERMINAL_LEASE_RESULT"
	TypeTerminalResize              MessageType = "TERMINAL_RESIZE"
	TypeTerminalResized             MessageType = "TERMINAL_RESIZED"
	TypeTerminalDetach              MessageType = "TERMINAL_DETACH"
	TypeTerminalDetached            MessageType = "TERMINAL_DETACHED"
	TypeTerminalInputResult         MessageType = "TERMINAL_INPUT_RESULT"
	TypeTerminalEOF                 MessageType = "TERMINAL_EOF"
	TypeTerminalExit                MessageType = "TERMINAL_EXIT"
	TypeTerminalReset               MessageType = "TERMINAL_RESET"
	TypeRemoteInvite                MessageType = "REMOTE_INVITE"
	TypeRemoteInviteResult          MessageType = "REMOTE_INVITE_RESULT"
	TypeError                       MessageType = "ERROR"
)

// Capability bits are deliberately a fixed wire bitmask, not an extensible
// permission registry. Observe is mandatory on every paired client.
type Capabilities uint32

const (
	CapabilityObserve Capabilities = 1 << iota
	CapabilityPrivateHumanRequestDetail
	CapabilityHumanActions
	CapabilityTerminalInput
	// CapabilityAdministration gates the operator's provider logins: their
	// discovery, linking, and assignment to agents through AGENT_UPDATE.
	CapabilityAdministration
)

const knownCapabilities = CapabilityObserve |
	CapabilityPrivateHumanRequestDetail |
	CapabilityHumanActions |
	CapabilityTerminalInput |
	CapabilityAdministration

// CapabilityHumanActions covers bounded operator mutations, including task
// enqueue. It remains one durable bit so existing browser pairings and the
// v1 schema do not need a capability migration.

type Hello struct {
	DaemonID        string `json:"daemon_id"`
	BootID          string `json:"boot_id"`
	ConnectionNonce string `json:"connection_nonce"`
}

type PairProve struct {
	Challenge     string `json:"challenge"`
	PublicKeySEC1 string `json:"public_key_sec1"`
	Signature     string `json:"signature"`
}

type PairResult struct {
	ClientID     string       `json:"client_id"`
	Capabilities Capabilities `json:"capabilities"`
}

type AuthProve struct {
	ClientID  string `json:"client_id"`
	Signature string `json:"signature"`
}

type AuthResult struct {
	ClientID     string       `json:"client_id"`
	Capabilities Capabilities `json:"capabilities"`
}

// Error is intentionally terse. Private diagnostics never cross the browser
// boundary; the client can use the finite code and retryable bit only.
type Error struct {
	Code      ErrorCode `json:"code"`
	Retryable bool      `json:"retryable"`
}

type ErrorCode string

const (
	ErrorUnauthorized   ErrorCode = "unauthorized"
	ErrorInvalidRequest ErrorCode = "invalid_request"
	ErrorRateLimited    ErrorCode = "rate_limited"
	ErrorNotFound       ErrorCode = "not_found"
	ErrorStale          ErrorCode = "stale"
	ErrorTooLarge       ErrorCode = "too_large"
	ErrorInternal       ErrorCode = "internal"
	// ErrorUnsupported answers a client control type the daemon does not
	// know: a verb it never had or one it retired. The refusal names the
	// request and the daemon keeps its side of the connection open; a console
	// that knows this code degrades that one feature and carries on.
	ErrorUnsupported ErrorCode = "unsupported"
)

// ControlFrame is the decoded, closed union. Body is always one of the
// concrete protocol structs above; callers never need a map[string]any assertion.
type ControlFrame struct {
	Type MessageType
	ID   string
	Body any
}

var (
	ErrMalformed   = errors.New("browser protocol: malformed control frame")
	ErrOversized   = errors.New("browser protocol: control frame too large")
	ErrUnsupported = errors.New("browser protocol: unsupported control type")
)

// The envelope carries no generation. The contract is unversioned by owner
// decision on 4 September 2026: a member that is not a known name under any
// case is ignored, so evolution is additive and neither side has to move in
// lockstep.
type controlEnvelope struct {
	Type MessageType     `json:"type"`
	ID   json.RawMessage `json:"id,omitempty"`
	Body json.RawMessage `json:"body"`
}

func EncodeHello(value Hello) ([]byte, error) { return encodeControl(TypeHello, "", value) }
func EncodePairProve(id string, value PairProve) ([]byte, error) {
	return encodeControl(TypePairProve, id, value)
}
func EncodePairResult(id string, value PairResult) ([]byte, error) {
	return encodeControl(TypePairResult, id, value)
}
func EncodeAuthProve(id string, value AuthProve) ([]byte, error) {
	return encodeControl(TypeAuthProve, id, value)
}
func EncodeAuthResult(id string, value AuthResult) ([]byte, error) {
	return encodeControl(TypeAuthResult, id, value)
}
func EncodeStateGet(id string, value StateGet) ([]byte, error) {
	return encodeControl(TypeStateGet, id, value)
}
func EncodeStateSnapshot(id string, value StateSnapshot) ([]byte, error) {
	return encodeControl(TypeStateSnapshot, id, value)
}
func EncodeStateWatch(id string, value StateWatch) ([]byte, error) {
	return encodeControl(TypeStateWatch, id, value)
}
func EncodeStateChanged(id string, value StateChanged) ([]byte, error) {
	return encodeControl(TypeStateChanged, id, value)
}
func EncodeHumanRequestDetailGet(id string, value HumanRequestDetailGet) ([]byte, error) {
	return encodeControl(TypeHumanRequestDetailGet, id, value)
}
func EncodeHumanRequestDetail(id string, value HumanRequestDetail) ([]byte, error) {
	return encodeControl(TypeHumanRequestDetail, id, value)
}
func EncodeError(id string, value Error) ([]byte, error) {
	return encodeControl(TypeError, id, value)
}

func encodeControl(kind MessageType, id string, body any) ([]byte, error) {
	if err := validateID(id, kind); err != nil {
		return nil, err
	}
	if err := validateBody(kind, body); err != nil {
		return nil, err
	}
	payload, err := marshalControl(body)
	if err != nil {
		return nil, fmt.Errorf("%w: body: %v", ErrMalformed, err)
	}
	var wireID json.RawMessage
	if id != "" {
		wireID, err = marshalControl(id)
		if err != nil {
			return nil, fmt.Errorf("%w: id: %v", ErrMalformed, err)
		}
	}
	frame, err := marshalControl(controlEnvelope{Type: kind, ID: wireID, Body: payload})
	if err != nil {
		return nil, fmt.Errorf("%w: envelope: %v", ErrMalformed, err)
	}
	if len(frame) > controlLimit(kind) {
		return nil, ErrOversized
	}
	return frame, nil
}

// marshalControl is json.Marshal without HTML escaping. The browser encoder is
// JSON.stringify, which leaves <, > and & literal; escaping them here would
// make the two implementations emit different bytes for the same frame, and
// the shared fixtures assert that they do not.
func marshalControl(value any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// DecodeClientControl accepts only frames a browser client may send. ERROR is
// bidirectional; all other message directions are closed in the manifest.
func DecodeClientControl(data []byte) (ControlFrame, error) {
	return decodeControl(data, clientRole)
}

// DecodeServerControl accepts only frames factoryd may send. ERROR is
// bidirectional; all other message directions are closed in the manifest.
func DecodeServerControl(data []byte) (ControlFrame, error) {
	return decodeControl(data, serverRole)
}

type senderRole byte

const (
	clientRole senderRole = 1
	serverRole senderRole = 2
)

func decodeControl(data []byte, role senderRole) (ControlFrame, error) {
	// A browser can only ever send a 64 KiB control frame. Only the server
	// direction admits the larger whole-state snapshot, and the exact
	// per-type bound below still applies once the type is known.
	entryLimit, arrayLimit := MaxControlBytes, MaxJSONArray
	if role == serverRole {
		entryLimit, arrayLimit = MaxSnapshotBytes, MaxSnapshotEntities
	}
	if len(data) > entryLimit {
		return ControlFrame{}, ErrOversized
	}
	if len(data) == 0 || !utf8.Valid(data) {
		return ControlFrame{}, ErrMalformed
	}
	if err := validateJSON(data, arrayLimit); err != nil {
		return ControlFrame{}, ErrMalformed
	}
	var envelope controlEnvelope
	if err := unmarshalObject(data, &envelope); err != nil {
		return ControlFrame{}, ErrMalformed
	}
	if len(data) > controlLimit(envelope.Type) {
		return ControlFrame{}, ErrOversized
	}
	id, idPresent, ok := decodeID(envelope.ID)
	if !ok || !validIDPresence(id, idPresent, envelope.Type) {
		return ControlFrame{}, ErrMalformed
	}
	if len(envelope.Body) == 0 || bytes.Equal(bytes.TrimSpace(envelope.Body), []byte("null")) {
		return ControlFrame{}, ErrMalformed
	}
	if err := rejectNullMembers(envelope.Type, envelope.Body); err != nil {
		return ControlFrame{}, ErrMalformed
	}
	if !typeAllowed(role, envelope.Type) {
		if envelope.Type == "" || typeAllowed(clientRole, envelope.Type) || typeAllowed(serverRole, envelope.Type) {
			return ControlFrame{}, ErrMalformed
		}
		// A type neither direction knows is one this build never had or no
		// longer has: the peer is newer, or older and still sending a retired
		// verb. Either way the frame comes back with its identity so the
		// caller can refuse exactly that request and keep the connection.
		return ControlFrame{Type: envelope.Type, ID: id}, ErrUnsupported
	}
	var body any
	switch envelope.Type {
	case TypeHello:
		body = new(Hello)
	case TypePairProve:
		body = new(PairProve)
	case TypePairResult:
		body = new(PairResult)
	case TypeAuthProve:
		body = new(AuthProve)
	case TypeAuthResult:
		body = new(AuthResult)
	case TypeStateGet:
		body = new(StateGet)
	case TypeStateSnapshot:
		body = new(StateSnapshot)
	case TypeStateWatch:
		body = new(StateWatch)
	case TypeStateChanged:
		body = new(StateChanged)
	case TypeHumanRequestDetailGet:
		body = new(HumanRequestDetailGet)
	case TypeHumanRequestDetail:
		body = new(HumanRequestDetail)
	case TypeHumanRequestReply:
		body = new(HumanRequestReply)
	case TypeHumanRequestReplyResult:
		body = new(HumanRequestReplyResult)
	case TypeHumanRequestCancelRun:
		body = new(HumanRequestCancelRun)
	case TypeHumanRequestCancelRunResult:
		body = new(HumanRequestCancelRunResult)
	case TypeAgentControl:
		body = new(AgentControl)
	case TypeAgentControlResult:
		body = new(AgentControlResult)
	case TypeTaskHistoryGet:
		body = new(TaskHistoryGet)
	case TypeTaskHistory:
		body = new(TaskHistory)
	case TypeTaskEnqueue:
		body = new(TaskEnqueue)
	case TypeTaskEnqueueResult:
		body = new(TaskEnqueueResult)
	case TypeAgentUpdate:
		body = new(AgentUpdate)
	case TypeAgentUpdateResult:
		body = new(AgentUpdateResult)
	case TypeTaskUpdate:
		body = new(TaskUpdate)
	case TypeTaskUpdateResult:
		body = new(TaskUpdateResult)
	case TypeTopologyGet:
		body = new(TopologyGet)
	case TypeTopology:
		body = new(Topology)
	case TypeRunPathsGet:
		body = new(RunPathsGet)
	case TypeRunPaths:
		body = new(RunPaths)
	case TypeAccountsDiscover:
		body = new(AccountsDiscover)
	case TypeAccounts:
		body = new(Accounts)
	case TypeAccountLink:
		body = new(AccountLink)
	case TypeAccountLinkResult:
		body = new(AccountLinkResult)
	case TypeTerminalTargetGet:
		body = new(TerminalTargetGet)
	case TypeTerminalTarget:
		body = new(TerminalTarget)
	case TypeTerminalAttach:
		body = new(TerminalAttach)
	case TypeTerminalAttached:
		body = new(TerminalAttached)
	case TypeTerminalAck:
		body = new(TerminalAck)
	case TypeTerminalLeaseAcquire:
		body = new(TerminalLeaseAcquire)
	case TypeTerminalLeaseRenew:
		body = new(TerminalLeaseRenew)
	case TypeTerminalLeaseRelease:
		body = new(TerminalLeaseRelease)
	case TypeTerminalLeaseResult:
		body = new(TerminalLeaseResult)
	case TypeTerminalResize:
		body = new(TerminalResize)
	case TypeTerminalResized:
		body = new(TerminalResized)
	case TypeTerminalDetach:
		body = new(TerminalDetach)
	case TypeTerminalDetached:
		body = new(TerminalDetached)
	case TypeTerminalInputResult:
		body = new(TerminalInputResult)
	case TypeTerminalEOF:
		body = new(TerminalEOF)
	case TypeTerminalExit:
		body = new(TerminalExit)
	case TypeTerminalReset:
		body = new(TerminalReset)
	case TypeRemoteInvite:
		body = new(RemoteInvite)
	case TypeRemoteInviteResult:
		body = new(RemoteInviteResult)
	case TypeError:
		var value struct {
			Code      ErrorCode `json:"code"`
			Retryable *bool     `json:"retryable"`
		}
		if err := unmarshalObject(envelope.Body, &value); err != nil {
			return ControlFrame{}, ErrMalformed
		}
		if value.Retryable == nil {
			return ControlFrame{}, ErrMalformed
		}
		body = Error{Code: value.Code, Retryable: *value.Retryable}
		if err := validateBody(envelope.Type, body); err != nil {
			return ControlFrame{}, ErrMalformed
		}
		return ControlFrame{Type: envelope.Type, ID: id, Body: body}, nil
	default:
		return ControlFrame{}, ErrMalformed
	}
	if err := unmarshalObject(envelope.Body, body); err != nil {
		return ControlFrame{}, ErrMalformed
	}
	if err := validateBody(envelope.Type, body); err != nil {
		return ControlFrame{}, ErrMalformed
	}
	return ControlFrame{Type: envelope.Type, ID: id, Body: dereferenceBody(body)}, nil
}

func decodeID(raw json.RawMessage) (string, bool, bool) {
	if len(raw) == 0 {
		return "", false, true
	}
	if bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
		return "", true, false
	}
	var value string
	if json.Unmarshal(raw, &value) != nil {
		return "", true, false
	}
	return value, true, true
}

func validIDPresence(id string, present bool, kind MessageType) bool {
	if kind == TypeTerminalAck {
		return !present
	}
	required := idRequired(kind)
	if !present {
		return !required && (kind == TypeHello || kind == TypeError)
	}
	if id == "" {
		return false
	}
	return validateID(id, kind) == nil
}

func idRequired(kind MessageType) bool {
	switch kind {
	case TypePairProve, TypePairResult, TypeAuthProve, TypeAuthResult,
		TypeStateGet, TypeStateSnapshot, TypeStateWatch, TypeStateChanged,
		TypeHumanRequestDetailGet, TypeHumanRequestDetail,
		TypeHumanRequestReply, TypeHumanRequestReplyResult, TypeHumanRequestCancelRun, TypeHumanRequestCancelRunResult,
		TypeTaskEnqueue, TypeTaskEnqueueResult, TypeAgentControl, TypeAgentControlResult, TypeTaskHistoryGet, TypeTaskHistory,
		TypeAgentUpdate, TypeAgentUpdateResult, TypeTaskUpdate, TypeTaskUpdateResult, TypeTopologyGet, TypeTopology,
		TypeRunPathsGet, TypeRunPaths,
		TypeAccountsDiscover, TypeAccounts, TypeAccountLink, TypeAccountLinkResult,
		TypeTerminalTargetGet, TypeTerminalTarget,
		TypeTerminalAttach, TypeTerminalAttached, TypeTerminalLeaseAcquire, TypeTerminalLeaseRenew, TypeTerminalLeaseRelease,
		TypeTerminalLeaseResult, TypeTerminalResize, TypeTerminalResized, TypeTerminalDetach, TypeTerminalDetached,
		TypeTerminalInputResult, TypeTerminalEOF, TypeTerminalExit, TypeTerminalReset,
		TypeRemoteInvite, TypeRemoteInviteResult:
		return true
	default:
		return false
	}
}

func typeAllowed(role senderRole, kind MessageType) bool {
	if kind == TypeError {
		return true
	}
	if role == clientRole {
		return kind == TypePairProve || kind == TypeAuthProve || kind == TypeStateGet ||
			kind == TypeStateWatch || kind == TypeHumanRequestDetailGet || kind == TypeHumanRequestReply || kind == TypeHumanRequestCancelRun || kind == TypeTerminalTargetGet || kind == TypeTerminalAttach || kind == TypeTerminalAck || kind == TypeTerminalLeaseAcquire || kind == TypeTerminalLeaseRenew || kind == TypeTerminalLeaseRelease || kind == TypeTerminalResize || kind == TypeTerminalDetach || kind == TypeTaskEnqueue || kind == TypeRemoteInvite ||
			kind == TypeAgentControl || kind == TypeTaskHistoryGet || kind == TypeAgentUpdate || kind == TypeTaskUpdate || kind == TypeTopologyGet || kind == TypeRunPathsGet ||
			kind == TypeAccountsDiscover || kind == TypeAccountLink
	}
	return role == serverRole && (kind == TypeHello || kind == TypePairResult || kind == TypeAuthResult ||
		kind == TypeStateSnapshot || kind == TypeStateChanged || kind == TypeHumanRequestDetail || kind == TypeHumanRequestReplyResult || kind == TypeHumanRequestCancelRunResult || kind == TypeTaskEnqueueResult || kind == TypeTerminalTarget || kind == TypeTerminalAttached || kind == TypeTerminalLeaseResult || kind == TypeTerminalResized || kind == TypeTerminalDetached || kind == TypeTerminalInputResult || kind == TypeTerminalEOF || kind == TypeTerminalExit || kind == TypeTerminalReset || kind == TypeRemoteInviteResult ||
		kind == TypeAgentControlResult || kind == TypeTaskHistory || kind == TypeAgentUpdateResult || kind == TypeTaskUpdateResult || kind == TypeTopology || kind == TypeRunPaths ||
		kind == TypeAccounts || kind == TypeAccountLinkResult)
}

func dereferenceBody(body any) any {
	switch value := body.(type) {
	case *Hello:
		return *value
	case *PairProve:
		return *value
	case *PairResult:
		return *value
	case *AuthProve:
		return *value
	case *AuthResult:
		return *value
	case *StateGet:
		return *value
	case *StateSnapshot:
		return *value
	case *StateWatch:
		return *value
	case *StateChanged:
		return *value
	case *HumanRequestDetailGet:
		return *value
	case *HumanRequestDetail:
		return *value
	case *HumanRequestReply:
		return *value
	case *HumanRequestReplyResult:
		return *value
	case *HumanRequestCancelRun:
		return *value
	case *HumanRequestCancelRunResult:
		return *value
	case *AgentControl:
		return *value
	case *AgentControlResult:
		return *value
	case *TaskHistoryGet:
		return *value
	case *TaskHistory:
		return *value
	case *TaskEnqueue:
		return *value
	case *TaskEnqueueResult:
		return *value
	case *AgentUpdate:
		return *value
	case *AgentUpdateResult:
		return *value
	case *TaskUpdate:
		return *value
	case *TaskUpdateResult:
		return *value
	case *TopologyGet:
		return *value
	case *Topology:
		return *value
	case *RunPathsGet:
		return *value
	case *RunPaths:
		return *value
	case *AccountsDiscover:
		return *value
	case *Accounts:
		return *value
	case *AccountLink:
		return *value
	case *AccountLinkResult:
		return *value
	case *TerminalTargetGet:
		return *value
	case *TerminalTarget:
		return *value
	case *TerminalAttach:
		return *value
	case *TerminalAttached:
		return *value
	case *TerminalAck:
		return *value
	case *TerminalLeaseAcquire:
		return *value
	case *TerminalLeaseRenew:
		return *value
	case *TerminalLeaseRelease:
		return *value
	case *TerminalLeaseResult:
		return *value
	case *TerminalResize:
		return *value
	case *TerminalResized:
		return *value
	case *TerminalDetach:
		return *value
	case *TerminalDetached:
		return *value
	case *TerminalInputResult:
		return *value
	case *TerminalEOF:
		return *value
	case *TerminalExit:
		return *value
	case *TerminalReset:
		return *value
	case *RemoteInvite:
		return *value
	case *RemoteInviteResult:
		return *value
	case *Error:
		return *value
	default:
		panic("browser protocol: unhandled body")
	}
}

func unmarshalObject(data []byte, target any) error {
	trimmed := bytes.TrimSpace(data)
	if len(trimmed) < 2 || trimmed[0] != '{' || trimmed[len(trimmed)-1] != '}' {
		return fmt.Errorf("%w: object required", ErrMalformed)
	}
	if err := validateJSONShape(trimmed, reflect.TypeOf(target)); err != nil {
		return err
	}
	decoder := json.NewDecoder(bytes.NewReader(trimmed))
	if err := decoder.Decode(target); err != nil {
		return fmt.Errorf("%w: %v", ErrMalformed, err)
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return fmt.Errorf("%w: trailing JSON", ErrMalformed)
	}
	return nil
}

func unmarshalArray(data []byte, target any) error {
	trimmed := bytes.TrimSpace(data)
	if len(trimmed) < 2 || trimmed[0] != '[' || trimmed[len(trimmed)-1] != ']' {
		return fmt.Errorf("%w: array required", ErrMalformed)
	}
	if err := validateJSONShape(trimmed, reflect.TypeOf(target)); err != nil {
		return err
	}
	decoder := json.NewDecoder(bytes.NewReader(trimmed))
	if err := decoder.Decode(target); err != nil {
		return fmt.Errorf("%w: %v", ErrMalformed, err)
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return fmt.Errorf("%w: trailing JSON", ErrMalformed)
	}
	return nil
}

// validateJSONShape checks the type of every member this build knows and
// requires every non-optional one. A member that is not a known name under any
// case is ignored, so the peer may add one without a coordinated release. Size,
// depth, array and object-member bounds still apply to the whole frame.
func validateJSONShape(data []byte, target reflect.Type) error {
	for target.Kind() == reflect.Pointer {
		target = target.Elem()
	}
	if target == reflect.TypeOf(json.RawMessage{}) {
		return nil
	}
	switch target.Kind() {
	case reflect.Struct:
		var object map[string]json.RawMessage
		if err := json.Unmarshal(data, &object); err != nil {
			return fmt.Errorf("%w: object required", ErrMalformed)
		}
		fields := make(map[string]reflect.Type, target.NumField())
		required := make(map[string]bool, target.NumField())
		folded := make(map[string]struct{}, target.NumField())
		for index := 0; index < target.NumField(); index++ {
			field := target.Field(index)
			tag := field.Tag.Get("json")
			name, options, _ := strings.Cut(tag, ",")
			if name == "-" {
				continue
			}
			if name == "" {
				name = field.Name
			}
			fields[name] = field.Type
			required[name] = options != "omitempty"
			folded[strings.ToLower(name)] = struct{}{}
		}
		for name, raw := range object {
			fieldType, ok := fields[name]
			if !ok {
				// encoding/json falls back to a case-insensitive match, so a
				// member that case-folds onto a known name is not unknown to
				// the decoder: ignoring it here would let `DAEMON_ID` silently
				// overwrite the `daemon_id` this function validated, and the
				// TypeScript decoder -- whose object keys are exact -- would
				// disagree about the same frame. That fallback folds Unicode
				// (a long s matches `s`), so a non-ASCII name is refused outright
				// and ASCII names compare by lower case; only an ASCII name that
				// is unknown under every case is ignored.
				for _, r := range name {
					if r >= utf8.RuneSelf {
						return fmt.Errorf("%w: non-ASCII object field name", ErrMalformed)
					}
				}
				if _, collides := folded[strings.ToLower(name)]; collides {
					return fmt.Errorf("%w: object field case collision", ErrMalformed)
				}
				continue
			}
			if bytes.Equal(bytes.TrimSpace(raw), []byte("null")) && fieldType.Kind() == reflect.Pointer {
				continue
			}
			if err := validateJSONShape(raw, fieldType); err != nil {
				return err
			}
		}
		for name, needed := range required {
			if _, ok := object[name]; needed && !ok {
				return fmt.Errorf("%w: missing object field", ErrMalformed)
			}
		}
	case reflect.Slice, reflect.Array:
		var values []json.RawMessage
		if err := json.Unmarshal(data, &values); err != nil {
			return fmt.Errorf("%w: array required", ErrMalformed)
		}
		for _, raw := range values {
			if err := validateJSONShape(raw, target.Elem()); err != nil {
				return err
			}
		}
	}
	return nil
}

func validateID(id string, kind MessageType) error {
	required := idRequired(kind)
	if id == "" {
		if required {
			return fmt.Errorf("%w: %s requires id", ErrMalformed, kind)
		}
		return nil
	}
	if len(id) > 64 {
		return fmt.Errorf("%w: id too long", ErrMalformed)
	}
	for _, b := range []byte(id) {
		if b < 0x21 || b > 0x7e {
			return fmt.Errorf("%w: id must be printable ASCII", ErrMalformed)
		}
	}
	if kind == TypeHello {
		return fmt.Errorf("%w: HELLO cannot have id", ErrMalformed)
	}
	return nil
}

func validateBody(kind MessageType, body any) error {
	switch kind {
	case TypeHello:
		value, ok := body.(Hello)
		if !ok {
			if pointer, ok := body.(*Hello); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: HELLO body type", ErrMalformed)
			}
		}
		return validateHello(value)
	case TypePairProve:
		value, ok := body.(PairProve)
		if !ok {
			if pointer, ok := body.(*PairProve); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: PAIR_PROVE body type", ErrMalformed)
			}
		}
		return validatePairProve(value)
	case TypePairResult:
		value, ok := body.(PairResult)
		if !ok {
			if pointer, ok := body.(*PairResult); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: PAIR_RESULT body type", ErrMalformed)
			}
		}
		return validatePairResult(value)
	case TypeAuthProve:
		value, ok := body.(AuthProve)
		if !ok {
			if pointer, ok := body.(*AuthProve); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: AUTH_PROVE body type", ErrMalformed)
			}
		}
		return validateAuthProve(value)
	case TypeAuthResult:
		value, ok := body.(AuthResult)
		if !ok {
			if pointer, ok := body.(*AuthResult); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: AUTH_RESULT body type", ErrMalformed)
			}
		}
		return validateAuthResult(value)
	case TypeStateGet:
		value, ok := body.(StateGet)
		if !ok {
			if pointer, ok := body.(*StateGet); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: STATE_GET body type", ErrMalformed)
			}
		}
		return validateStateGet(value)
	case TypeStateSnapshot:
		value, ok := body.(StateSnapshot)
		if !ok {
			if pointer, ok := body.(*StateSnapshot); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: STATE_SNAPSHOT body type", ErrMalformed)
			}
		}
		return validateStateSnapshot(value)
	case TypeStateWatch:
		value, ok := body.(StateWatch)
		if !ok {
			if pointer, ok := body.(*StateWatch); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: STATE_WATCH body type", ErrMalformed)
			}
		}
		return validateStateWatch(value)
	case TypeStateChanged:
		value, ok := body.(StateChanged)
		if !ok {
			if pointer, ok := body.(*StateChanged); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: STATE_CHANGED body type", ErrMalformed)
			}
		}
		return validateStateChanged(value)
	case TypeHumanRequestDetailGet:
		value, ok := body.(HumanRequestDetailGet)
		if !ok {
			if pointer, ok := body.(*HumanRequestDetailGet); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: HUMAN_REQUEST_DETAIL_GET body type", ErrMalformed)
			}
		}
		return validateHumanRequestDetailGet(value)
	case TypeHumanRequestDetail:
		value, ok := body.(HumanRequestDetail)
		if !ok {
			if pointer, ok := body.(*HumanRequestDetail); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: HUMAN_REQUEST_DETAIL body type", ErrMalformed)
			}
		}
		return validateHumanRequestDetail(value)
	case TypeHumanRequestReply:
		return validTerminalControl(kind, body)
	case TypeHumanRequestReplyResult:
		return validTerminalControl(kind, body)
	case TypeHumanRequestCancelRun:
		return validTerminalControl(kind, body)
	case TypeHumanRequestCancelRunResult:
		return validTerminalControl(kind, body)
	case TypeAgentControl, TypeAgentControlResult, TypeTaskHistoryGet, TypeTaskHistory:
		return validAgentControl(kind, body)
	case TypeTaskEnqueue, TypeTaskEnqueueResult:
		return validTaskControl(kind, body)
	case TypeAgentUpdate, TypeAgentUpdateResult, TypeTaskUpdate, TypeTaskUpdateResult, TypeTopologyGet, TypeTopology,
		TypeRunPathsGet, TypeRunPaths, TypeAccountsDiscover, TypeAccounts, TypeAccountLink, TypeAccountLinkResult:
		return validConsoleControl(kind, body)
	case TypeTerminalTargetGet, TypeTerminalTarget:
		return validTerminalControl(kind, body)
	case TypeTerminalAttach:
		return validTerminalControl(kind, body)
	case TypeTerminalAttached:
		return validTerminalControl(kind, body)
	case TypeTerminalAck:
		return validTerminalControl(kind, body)
	case TypeTerminalLeaseAcquire:
		return validTerminalControl(kind, body)
	case TypeTerminalLeaseRenew, TypeTerminalLeaseRelease:
		return validTerminalControl(kind, body)
	case TypeTerminalLeaseResult:
		return validTerminalControl(kind, body)
	case TypeTerminalResize:
		return validTerminalControl(kind, body)
	case TypeTerminalResized:
		return validTerminalControl(kind, body)
	case TypeTerminalDetach:
		return validTerminalControl(kind, body)
	case TypeTerminalDetached:
		return validTerminalControl(kind, body)
	case TypeTerminalInputResult:
		return validTerminalControl(kind, body)
	case TypeTerminalEOF:
		return validTerminalControl(kind, body)
	case TypeTerminalExit:
		return validTerminalControl(kind, body)
	case TypeTerminalReset:
		return validTerminalControl(kind, body)
	case TypeRemoteInvite, TypeRemoteInviteResult:
		return validRemoteControl(kind, body)
	case TypeError:
		value, ok := body.(Error)
		if !ok {
			if pointer, ok := body.(*Error); ok {
				value = *pointer
			} else {
				return fmt.Errorf("%w: ERROR body type", ErrMalformed)
			}
		}
		return validateError(value)
	default:
		return fmt.Errorf("%w: unknown type", ErrMalformed)
	}
}

func validateHello(value Hello) error {
	if _, err := fixedHex("daemon_id", value.DaemonID, DaemonIDSize); err != nil {
		return err
	}
	if _, err := fixedHex("boot_id", value.BootID, BootIDSize); err != nil {
		return err
	}
	if _, err := fixedHex("connection_nonce", value.ConnectionNonce, NonceSize); err != nil {
		return err
	}
	return nil
}

func validatePairProve(value PairProve) error {
	if _, err := fixedHex("challenge", value.Challenge, ChallengeSize); err != nil {
		return err
	}
	if key, err := fixedHex("public_key_sec1", value.PublicKeySEC1, PublicKeySize); err != nil {
		return err
	} else if key[0] != 4 {
		return fmt.Errorf("%w: public key encoding", ErrMalformed)
	}
	_, err := fixedHex("signature", value.Signature, SignatureSize)
	return err
}

func validatePairResult(value PairResult) error {
	if _, err := fixedHex("client_id", value.ClientID, ClientIDSize); err != nil {
		return err
	}
	return validateCapabilities(value.Capabilities)
}

func validateAuthProve(value AuthProve) error {
	if _, err := fixedHex("client_id", value.ClientID, ClientIDSize); err != nil {
		return err
	}
	_, err := fixedHex("signature", value.Signature, SignatureSize)
	return err
}

func validateAuthResult(value AuthResult) error {
	if _, err := fixedHex("client_id", value.ClientID, ClientIDSize); err != nil {
		return err
	}
	return validateCapabilities(value.Capabilities)
}

func validateCapabilities(value Capabilities) error {
	if value&^knownCapabilities != 0 || value&CapabilityObserve == 0 {
		return fmt.Errorf("%w: invalid capabilities", ErrMalformed)
	}
	return nil
}

func validateError(value Error) error {
	switch value.Code {
	case ErrorUnauthorized, ErrorInvalidRequest, ErrorRateLimited, ErrorNotFound, ErrorStale, ErrorTooLarge, ErrorInternal, ErrorUnsupported:
		return nil
	default:
		return fmt.Errorf("%w: unknown error code %q", ErrMalformed, value.Code)
	}
}

func fixedHex(name, value string, size int) ([]byte, error) {
	if len(value) != size*2 || value != strings.ToLower(value) {
		return nil, fmt.Errorf("%w: %s must be lowercase %d-byte hex", ErrMalformed, name, size)
	}
	decoded, err := hex.DecodeString(value)
	if err != nil || len(decoded) != size {
		return nil, fmt.Errorf("%w: %s must be lowercase %d-byte hex", ErrMalformed, name, size)
	}
	return decoded, nil
}

// validateJSON checks the properties encoding/json intentionally leaves
// permissive: duplicate names, bounded nesting/arrays, and safe integer
// spelling. Typed decoding below then rejects unknown fields.
func validateJSON(data []byte, arrayLimit int) error {
	if err := validateSurrogateEscapes(data); err != nil {
		return fmt.Errorf("%w: %v", ErrMalformed, err)
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	if err := scanJSONValue(decoder, 0, arrayLimit); err != nil {
		return fmt.Errorf("%w: %v", ErrMalformed, err)
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return fmt.Errorf("%w: trailing JSON", ErrMalformed)
	}
	return nil
}

// encoding/json replaces an escaped lone UTF-16 surrogate with U+FFFD. Scan
// the source spelling first so the wire cannot silently change the reply or
// any other protocol text while decoding.
func validateSurrogateEscapes(data []byte) error {
	for index := 0; index < len(data); index++ {
		if data[index] != '"' {
			continue
		}
		for index++; index < len(data); index++ {
			switch data[index] {
			case '"':
				goto nextString
			case '\\':
				index++
				if index >= len(data) {
					return errors.New("unterminated escape")
				}
				if data[index] != 'u' {
					continue
				}
				if index+4 >= len(data) {
					return errors.New("short unicode escape")
				}
				value, ok := parseUnicodeEscape(data[index+1 : index+5])
				if !ok {
					return errors.New("invalid unicode escape")
				}
				index += 4
				if value >= 0xdc00 && value <= 0xdfff {
					return errors.New("lone low surrogate")
				}
				if value < 0xd800 || value > 0xdbff {
					continue
				}
				if index+6 >= len(data) || data[index+1] != '\\' || data[index+2] != 'u' {
					return errors.New("lone high surrogate")
				}
				low, ok := parseUnicodeEscape(data[index+3 : index+7])
				if !ok || low < 0xdc00 || low > 0xdfff {
					return errors.New("invalid surrogate pair")
				}
				index += 6
			}
		}
		return errors.New("unterminated string")
	nextString:
	}
	return nil
}

func parseUnicodeEscape(value []byte) (uint16, bool) {
	if len(value) != 4 {
		return 0, false
	}
	var result uint16
	for _, digit := range value {
		result <<= 4
		switch {
		case digit >= '0' && digit <= '9':
			result |= uint16(digit - '0')
		case digit >= 'a' && digit <= 'f':
			result |= uint16(digit-'a') + 10
		case digit >= 'A' && digit <= 'F':
			result |= uint16(digit-'A') + 10
		default:
			return 0, false
		}
	}
	return result, true
}

// rejectNullMembers refuses an explicit null where the Go body holds a pointer
// or a scalar encoding/json would silently zero. An optional console member is
// absent or a value; null would otherwise decode as "unchanged" here while the
// browser's exact decoder refuses the same frame.
func rejectNullMembers(kind MessageType, body []byte) error {
	var fields []string
	switch kind {
	case TypeTerminalLeaseResult:
		fields = []string{"expires_at_ms"}
	case TypeTerminalExit:
		fields = []string{"session_id", "exit_code", "exit_signal", "aborted"}
	case TypeAgentUpdate:
		fields = []string{"model", "reasoning_effort", "account_id", "paused", "idle_policy", "idle_after_seconds", "idle_instruction", "idle_run_budget"}
	case TypeTaskEnqueue:
		fields = []string{"mode"}
	case TypeTaskUpdate:
		fields = []string{"title", "priority", "assigned_agent_id", "status"}
	default:
		return nil
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(body, &object); err != nil {
		return err
	}
	for _, field := range fields {
		if raw, ok := object[field]; ok && bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
			return errors.New("null scalar")
		}
	}
	return nil
}

func scanJSONValue(decoder *json.Decoder, depth, arrayLimit int) error {
	if depth > MaxJSONDepth {
		return errors.New("JSON nesting too deep")
	}
	token, err := decoder.Token()
	if err != nil {
		return err
	}
	switch value := token.(type) {
	case json.Delim:
		switch value {
		case '{':
			keys := make(map[string]struct{}, MaxJSONObject)
			for decoder.More() {
				keyToken, err := decoder.Token()
				if err != nil {
					return err
				}
				key, ok := keyToken.(string)
				if !ok {
					return errors.New("object key is not a string")
				}
				if _, exists := keys[key]; exists {
					return fmt.Errorf("duplicate object key %q", key)
				}
				if len(keys) == MaxJSONObject {
					return errors.New("too many object members")
				}
				keys[key] = struct{}{}
				if err := scanJSONValue(decoder, depth+1, arrayLimit); err != nil {
					return err
				}
			}
			end, err := decoder.Token()
			if err != nil || end != json.Delim('}') {
				return errors.New("unterminated object")
			}
		case '[':
			count := 0
			for decoder.More() {
				if count == arrayLimit {
					return errors.New("array too large")
				}
				if err := scanJSONValue(decoder, depth+1, arrayLimit); err != nil {
					return err
				}
				count++
			}
			end, err := decoder.Token()
			if err != nil || end != json.Delim(']') {
				return errors.New("unterminated array")
			}
		default:
			return errors.New("unexpected delimiter")
		}
	case json.Number:
		if err := validateJSONNumber(string(value)); err != nil {
			return err
		}
	case string, bool, nil:
		// Raw input UTF-8 was checked before tokenization.
	default:
		return errors.New("unexpected JSON token")
	}
	return nil
}

func validateJSONNumber(value string) error {
	if value == "" || strings.ContainsAny(value, ".eE") {
		return errors.New("only canonical integers are allowed")
	}
	negative := value[0] == '-'
	digits := value
	if negative {
		digits = value[1:]
	}
	if digits == "" || (len(digits) > 1 && digits[0] == '0') || (len(digits) > 0 && digits[0] == '+') || negative && digits == "0" {
		return errors.New("invalid integer")
	}
	parsed, err := strconv.ParseUint(digits, 10, 64)
	if err != nil || parsed > (1<<53)-1 {
		return errors.New("unsafe integer")
	}
	return nil
}
