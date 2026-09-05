//go:build darwin || linux

package api

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"sync"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/install"
)

// CallKind is the closed set of requests accepted by the local API.
type CallKind uint8

const (
	CallHealth CallKind = iota + 1
	CallSnapshot
	CallCreateProject
	CallCreateAgent
	CallEnqueueTask
	CallSetDispatch
	CallAttemptTask
	CallSucceed
	CallBlock
	CallFail
	CallRequestHuman
	CallWebStatus
	CallWebListClients
	CallWebRevokeClient
	CallRemoteStatus
)

// AttemptDigest is the SHA-256 digest of one raw attempt bearer. The bearer is
// erased before Receive returns and is never representable outside this package.
type AttemptDigest struct{ value [sha256.Size]byte }

func (digest AttemptDigest) Bytes() [sha256.Size]byte { return digest.value }
func (AttemptDigest) String() string                  { return "AttemptDigest(<redacted>)" }
func (AttemptDigest) GoString() string                { return "AttemptDigest(<redacted>)" }

func digestAttemptCredential(bearer credential) AttemptDigest {
	return AttemptDigest{value: sha256.Sum256(bearer[:])}
}

// Call is an immutable decoded request. Only the accessor matching Kind
// returns true.
type Call struct {
	kind             CallKind
	digest           AttemptDigest
	project          CreateProjectInput
	agent            CreateAgentInput
	task             EnqueueTaskInput
	humanQuestion    HumanQuestionInput
	webClient        WebClientRevocationInput
	webAfter         string
	expectedRevision uint64
	enabled          bool
	text             string
}

func (call Call) Kind() CallKind { return call.kind }
func (call Call) String() string { return "Call(<redacted>)" }
func (call Call) GoString() string {
	return "Call(<redacted>)"
}

func (call Call) AttemptDigest() (AttemptDigest, bool) {
	switch call.kind {
	case CallAttemptTask, CallSucceed, CallBlock, CallFail, CallRequestHuman:
		return call.digest, true
	default:
		return AttemptDigest{}, false
	}
}

func (call Call) CreateProjectInput() (CreateProjectInput, bool) {
	return call.project, call.kind == CallCreateProject
}

func (call Call) CreateAgentInput() (CreateAgentInput, bool) {
	return call.agent, call.kind == CallCreateAgent
}

func (call Call) EnqueueTaskInput() (EnqueueTaskInput, bool) {
	return call.task, call.kind == CallEnqueueTask
}

func (call Call) Dispatch() (uint64, bool, bool) {
	return call.expectedRevision, call.enabled, call.kind == CallSetDispatch
}

func (call Call) Result() (string, bool) {
	return call.text, call.kind == CallSucceed
}

func (call Call) Detail() (string, bool) {
	return call.text, call.kind == CallBlock || call.kind == CallFail
}

func (call Call) HumanQuestionInput() (HumanQuestionInput, bool) {
	return call.humanQuestion, call.kind == CallRequestHuman
}

func (call Call) WebClientRevocationInput() (WebClientRevocationInput, bool) {
	return call.webClient, call.kind == CallWebRevokeClient
}

func (call Call) WebListAfter() (string, bool) {
	return call.webAfter, call.kind == CallWebListClients
}

type replyKind uint8

const (
	replyHealth replyKind = iota + 1
	replySnapshot
	replyMutation
	replyAttemptTask
	replyWebStatus
	replyWebClients
	replyWebRevoke
	replyRemoteStatus
	replyError
)

// Reply is constructed only through its fixed reply constructors.
type Reply struct {
	kind        replyKind
	health      HealthStatus
	snapshot    DashboardSnapshot
	mutation    MutationResult
	attemptTask AttemptTask
	webStatus   WebStatus
	webClients  WebClientPage
	webRevoke   WebRevokeResult
	remote      RemoteStatus
	code        RemoteErrorCode
}

func (Reply) String() string   { return "Reply(<redacted>)" }
func (Reply) GoString() string { return "Reply(<redacted>)" }

func NewHealthReply(status HealthStatus) Reply {
	return Reply{kind: replyHealth, health: status}
}

func NewSnapshotReply(snapshot DashboardSnapshot) (Reply, error) {
	if !validSnapshot(snapshot) {
		return Reply{}, ErrInvalidInput
	}
	projects := make([]ProjectSummary, len(snapshot.Projects))
	copy(projects, snapshot.Projects)
	snapshot.Projects = projects
	agents := make([]AgentSummary, len(snapshot.Agents))
	copy(agents, snapshot.Agents)
	snapshot.Agents = agents
	tasks := make([]TaskSummary, len(snapshot.Tasks))
	copy(tasks, snapshot.Tasks)
	snapshot.Tasks = tasks
	return Reply{kind: replySnapshot, snapshot: snapshot}, nil
}

func NewMutationReply(result MutationResult) (Reply, error) {
	if result.Revision == 0 {
		return Reply{}, ErrInvalidInput
	}
	return Reply{kind: replyMutation, mutation: result}, nil
}

func NewAttemptTaskReply(task AttemptTask) (Reply, error) {
	if !validAttemptTask(task) {
		return Reply{}, ErrInvalidInput
	}
	return Reply{kind: replyAttemptTask, attemptTask: task}, nil
}

func NewWebStatusReply(status WebStatus) (Reply, error) {
	if !validWebStatus(status) {
		return Reply{}, ErrInvalidInput
	}
	status.Origins = append([]string(nil), status.Origins...)
	return Reply{kind: replyWebStatus, webStatus: status}, nil
}

func NewWebClientsReply(page WebClientPage) (Reply, error) {
	if !validWebClientPage(page) {
		return Reply{}, ErrInvalidInput
	}
	page.Clients = append([]WebClient(nil), page.Clients...)
	if page.NextAfter != nil {
		next := *page.NextAfter
		page.NextAfter = &next
	}
	return Reply{kind: replyWebClients, webClients: page}, nil
}

func NewWebRevokeReply(result WebRevokeResult) (Reply, error) {
	if !validWebRevokeResult(result) {
		return Reply{}, ErrInvalidInput
	}
	return Reply{kind: replyWebRevoke, webRevoke: result}, nil
}

func NewRemoteStatusReply(status RemoteStatus) (Reply, error) {
	if !validRemoteStatus(status) {
		return Reply{}, ErrInvalidInput
	}
	return Reply{kind: replyRemoteStatus, remote: status}, nil
}

func NewErrorReply(code RemoteErrorCode) (Reply, error) {
	if !validRemoteCode(code) {
		return Reply{}, ErrInvalidInput
	}
	return Reply{kind: replyError, code: code}, nil
}

// Listener owns local API framing over one install-retained endpoint. It
// creates no goroutines; the caller owns the accept loop.
type Listener struct {
	authority *install.LocalAPIAuthority
	protocol  *install.LocalAPIProtocol
	closer    *listenerClose
}

type listenerClose struct {
	once sync.Once
	err  error
}

func (Listener) String() string   { return "Listener(<redacted>)" }
func (Listener) GoString() string { return "Listener(<redacted>)" }

// Listen consumes an already-bound install authority. Socket creation, stale
// recovery, operator-principal loading, and filesystem ownership never occur
// in this protocol package.
func Listen(authority *install.LocalAPIAuthority) (*Listener, error) {
	if authority == nil {
		return nil, ErrInvalidListener
	}
	protocol, err := authority.ClaimProtocol()
	if err != nil {
		return nil, ErrInvalidListener
	}
	return &Listener{authority: authority, protocol: protocol, closer: &listenerClose{}}, nil
}

// Accept synchronously accepts one connection and verifies both the socket
// identity and peer EUID. The caller must close the returned Connection.
func (listener *Listener) Accept() (*Connection, error) {
	if listener == nil || listener.protocol == nil {
		return nil, ErrInvalidListener
	}
	connection, err := listener.protocol.Accept()
	if err != nil {
		return nil, ErrTransport
	}
	result := &Connection{connection: connection, protocol: listener.protocol}
	result.self = result
	return result, nil
}

// Close closes the retained authority. A replacement at the recorded name is
// left untouched and makes the authority's stable result uncertain.
func (listener *Listener) Close() error {
	if listener == nil {
		return nil
	}
	listener.closer.once.Do(func() {
		if listener.authority == nil {
			return
		}
		if err := listener.authority.Close(); err != nil {
			listener.closer.err = ErrInvalidListener
		}
	})
	return listener.closer.err
}

type connectionState uint8

const (
	connectionNew connectionState = iota
	connectionReading
	connectionReceived
	connectionDispatching
	connectionDispatched
	connectionResponded
	connectionClosed
)

// Connection owns one accepted socket and permits exactly one Receive and one
// Respond. Receive owns and joins its cancellation watcher before returning.
type Connection struct {
	self       *Connection
	connection *install.LocalAPIConnection
	protocol   *install.LocalAPIProtocol
	domain     byte
	kind       CallKind
	call       Call
	state      connectionState
	receipt    [outcomeReceiptBytes]byte
	receiptDue bool
	closeOnce  sync.Once
	closeErr   error
}

func (*Connection) String() string   { return "Connection(<redacted>)" }
func (*Connection) GoString() string { return "Connection(<redacted>)" }

// Receive reads one complete request. Ordinary calls require client EOF before
// dispatch; attempt outcomes keep the write half open for their response
// receipt. Invalid requests are answered, when a valid response domain is
// available, with a fixed error code.
func (connection *Connection) Receive(ctx context.Context) (Call, error) {
	if connection == nil || connection.self != connection || connection.connection == nil || connection.protocol == nil || connection.state != connectionNew {
		return Call{}, ErrProtocol
	}
	connection.state = connectionReading
	if err := connection.setDeadline(ctx); err != nil {
		return Call{}, err
	}
	stopCancellation := watchCancellation(ctx, connection.connection)
	defer stopCancellation()
	payload, err := readFrame(connection.connection)
	if err != nil {
		return Call{}, classifyFrameError(ctx, err)
	}
	if len(payload) < requestPrelude {
		return Call{}, connection.reject(RemoteInvalidRequest)
	}
	defer clear(payload[:requestPrelude])
	domain := payload[0]
	if domain == operatorDomain || domain == attemptDomain {
		connection.domain = domain
	}
	if err := ctx.Err(); err != nil {
		return Call{}, err
	}
	if domain != operatorDomain && domain != attemptDomain {
		return Call{}, ErrProtocol
	}
	var bearer credential
	copy(bearer[:], payload[1:requestPrelude])
	defer clear(bearer[:])
	call, code := decodeCall(domain, bearer, payload[requestPrelude:])
	if err := ctx.Err(); err != nil {
		return Call{}, err
	}
	if code != "" {
		return Call{}, connection.reject(code)
	}
	if !outcomeCall(call.kind) {
		if err := requireEOF(connection.connection); err != nil {
			if errors.Is(err, ErrProtocol) {
				return Call{}, connection.reject(RemoteInvalidRequest)
			}
			return Call{}, classifyFrameError(ctx, err)
		}
	}
	if domain == operatorDomain && !connection.protocol.CheckOperator(bearer[:]) || domain == attemptDomain && connection.protocol.Verify() != nil {
		return Call{}, connection.reject(RemoteUnauthorized)
	}
	connection.kind = call.kind
	connection.call = call
	connection.state = connectionReceived
	return call, nil
}

// Dispatch enters the one scoped authority lease for the exact call returned
// by Receive. Shutdown begun before this method is called refuses the callback;
// an already-entered callback is joined by OperationalHome.Close.
func (connection *Connection) Dispatch(dispatch func(Call) Reply) (Reply, error) {
	if connection == nil || connection.self != connection || connection.protocol == nil || connection.state != connectionReceived || dispatch == nil {
		return Reply{}, ErrProtocol
	}
	lease, err := connection.protocol.BeginDispatch()
	if err != nil {
		return Reply{}, ErrTransport
	}
	connection.state = connectionDispatching
	var reply Reply
	var closeErr error
	func() {
		defer func() { closeErr = lease.Close() }()
		reply = dispatch(connection.call)
	}()
	if closeErr != nil {
		return Reply{}, ErrTransport
	}
	connection.state = connectionDispatched
	return reply, nil
}

func (connection *Connection) setDeadline(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	deadline := time.Now().Add(requestTimeout)
	if requested, ok := ctx.Deadline(); ok && requested.Before(deadline) {
		deadline = requested
	}
	if err := connection.connection.SetDeadline(deadline); err != nil {
		return ErrTransport
	}
	return nil
}

type incomingRequest struct {
	Method string          `json:"method"`
	Params json.RawMessage `json:"params"`
}

func decodeCall(domain byte, bearer credential, encoded []byte) (Call, RemoteErrorCode) {
	var request incomingRequest
	if err := decodeExact(encoded, &request); err != nil || !jsonObject(request.Params) {
		return Call{}, RemoteInvalidRequest
	}
	kind, methodDomain := methodKind(request.Method)
	if kind == 0 {
		return Call{}, RemoteInvalidRequest
	}
	if methodDomain != domain {
		return Call{}, RemoteForbidden
	}
	call := Call{kind: kind}
	if domain == attemptDomain {
		call.digest = digestAttemptCredential(bearer)
	}
	switch kind {
	case CallHealth, CallSnapshot, CallAttemptTask, CallWebStatus, CallRemoteStatus:
		if err := decodeExact(request.Params, &struct{}{}); err != nil {
			return Call{}, RemoteInvalidRequest
		}
	case CallWebListClients:
		var input struct {
			After string `json:"after"`
		}
		if err := decodeExact(request.Params, &input); err != nil || input.After != "" && !validID(input.After) {
			return Call{}, RemoteInvalidRequest
		}
		call.webAfter = input.After
	case CallCreateProject:
		if err := decodeExact(request.Params, &call.project); err != nil || !validID(call.project.ID) || !validText(call.project.Name, 1, 128) || !validText(call.project.Root, 1, 4096) {
			return Call{}, RemoteInvalidRequest
		}
	case CallCreateAgent:
		if err := decodeExact(request.Params, &call.agent); err != nil || !validCreateAgentInput(call.agent) {
			return Call{}, RemoteInvalidRequest
		}
	case CallEnqueueTask:
		if err := decodeExact(request.Params, &call.task); err != nil || !validID(call.task.ID) || !validID(call.task.ProjectID) || !validID(call.task.AssignedAgentID) || !validID(call.task.IncarnationID) || !validText(call.task.Title, 1, 1024) || !validText(call.task.Body, 0, 131072) || call.task.Priority < -1_000_000 || call.task.Priority > 1_000_000 {
			return Call{}, RemoteInvalidRequest
		}
	case CallSetDispatch:
		var input struct {
			ExpectedRevision uint64 `json:"expected_revision"`
			Enabled          bool   `json:"enabled"`
		}
		if err := decodeExact(request.Params, &input); err != nil || input.ExpectedRevision == 0 {
			return Call{}, RemoteInvalidRequest
		}
		call.expectedRevision, call.enabled = input.ExpectedRevision, input.Enabled
	case CallSucceed:
		var input struct {
			Result string `json:"result"`
		}
		if err := decodeExact(request.Params, &input); err != nil || !validText(input.Result, 0, 131072) {
			return Call{}, RemoteInvalidRequest
		}
		call.text = input.Result
	case CallBlock:
		detail, ok := decodeAttemptDetail(request.Params)
		if !ok || !validText(detail, 1, 4096) {
			return Call{}, RemoteInvalidRequest
		}
		call.text = detail
	case CallFail:
		detail, ok := decodeAttemptDetail(request.Params)
		if !ok || !validText(detail, 0, 4096) {
			return Call{}, RemoteInvalidRequest
		}
		call.text = detail
	case CallRequestHuman:
		if err := decodeExact(request.Params, &call.humanQuestion); err != nil || !validID(call.humanQuestion.IdempotencyKey) || !validText(call.humanQuestion.Question, 1, 8192) {
			return Call{}, RemoteInvalidRequest
		}
	case CallWebRevokeClient:
		if err := decodeExact(request.Params, &call.webClient); err != nil || !validID(call.webClient.ID) || call.webClient.ExpectedRevision == 0 {
			return Call{}, RemoteInvalidRequest
		}
	}
	return call, ""
}

func decodeAttemptDetail(encoded []byte) (string, bool) {
	var input struct {
		Detail string `json:"detail"`
	}
	if err := decodeExact(encoded, &input); err != nil {
		return "", false
	}
	return input.Detail, true
}

func jsonObject(encoded []byte) bool {
	trimmed := bytes.TrimSpace(encoded)
	return len(trimmed) >= 2 && trimmed[0] == '{' && trimmed[len(trimmed)-1] == '}'
}

func methodKind(method string) (CallKind, byte) {
	switch method {
	case "health":
		return CallHealth, operatorDomain
	case "snapshot":
		return CallSnapshot, operatorDomain
	case "create_project":
		return CallCreateProject, operatorDomain
	case "create_agent":
		return CallCreateAgent, operatorDomain
	case "enqueue_task":
		return CallEnqueueTask, operatorDomain
	case "set_dispatch":
		return CallSetDispatch, operatorDomain
	case "task":
		return CallAttemptTask, attemptDomain
	case "succeed":
		return CallSucceed, attemptDomain
	case "block":
		return CallBlock, attemptDomain
	case "fail":
		return CallFail, attemptDomain
	case "request_human":
		return CallRequestHuman, attemptDomain
	case "web_status":
		return CallWebStatus, operatorDomain
	case "web_list_clients":
		return CallWebListClients, operatorDomain
	case "web_revoke_client":
		return CallWebRevokeClient, operatorDomain
	case "remote_status":
		return CallRemoteStatus, operatorDomain
	default:
		return 0, 0
	}
}

// Respond writes exactly one response for the previously received Call. The
// response domain is taken from that request and cannot be supplied by callers.
func (connection *Connection) Respond(reply Reply) error {
	if connection == nil || connection.self != connection || connection.connection == nil || connection.state != connectionDispatched || !replyMatches(connection.kind, reply.kind) {
		return ErrProtocol
	}
	connection.state = connectionResponded
	if err := connection.writeReply(reply); err != nil {
		return err
	}
	return nil
}

// AwaitOutcomeReceipt joins the response-consumption acknowledgement for a
// successful attempt outcome. Other replies have no receipt and return
// immediately. The random frame cannot be sent before the response is read.
func (connection *Connection) AwaitOutcomeReceipt(ctx context.Context) error {
	if connection == nil || connection.self != connection || connection.connection == nil || connection.state != connectionResponded {
		return ErrProtocol
	}
	if !connection.receiptDue {
		return nil
	}
	expected := connection.receipt
	defer func() {
		clear(connection.receipt[:])
		connection.receiptDue = false
	}()
	if err := connection.setDeadline(ctx); err != nil {
		return err
	}
	stopCancellation := watchCancellation(ctx, connection.connection)
	defer stopCancellation()
	receipt, err := readFrame(connection.connection)
	if err != nil {
		return classifyFrameError(ctx, err)
	}
	if len(receipt) != len(expected) || !bytes.Equal(receipt, expected[:]) {
		return ErrProtocol
	}
	if err := requireEOF(connection.connection); err != nil {
		return classifyFrameError(ctx, err)
	}
	return nil
}

func outcomeCall(kind CallKind) bool {
	return kind == CallSucceed || kind == CallBlock || kind == CallFail
}

func replyMatches(kind CallKind, reply replyKind) bool {
	if reply == replyError {
		return true
	}
	switch kind {
	case CallHealth:
		return reply == replyHealth
	case CallSnapshot:
		return reply == replySnapshot
	case CallAttemptTask:
		return reply == replyAttemptTask
	case CallCreateProject, CallCreateAgent, CallEnqueueTask, CallSetDispatch, CallSucceed, CallBlock, CallFail, CallRequestHuman:
		return reply == replyMutation
	case CallWebStatus:
		return reply == replyWebStatus
	case CallWebListClients:
		return reply == replyWebClients
	case CallWebRevokeClient:
		return reply == replyWebRevoke
	case CallRemoteStatus:
		return reply == replyRemoteStatus
	default:
		return false
	}
}

func (connection *Connection) reject(code RemoteErrorCode) error {
	if connection.domain != operatorDomain && connection.domain != attemptDomain {
		return ErrProtocol
	}
	reply, err := NewErrorReply(code)
	if err != nil {
		return ErrProtocol
	}
	_ = connection.writeReply(reply)
	connection.state = connectionResponded
	return ErrProtocol
}

func (connection *Connection) writeReply(reply Reply) error {
	var data []byte
	var err error
	switch reply.kind {
	case replyHealth:
		data, err = json.Marshal(reply.health)
	case replySnapshot:
		data, err = json.Marshal(reply.snapshot)
	case replyMutation:
		data, err = json.Marshal(reply.mutation)
	case replyAttemptTask:
		data, err = json.Marshal(reply.attemptTask)
	case replyWebStatus:
		data, err = json.Marshal(reply.webStatus)
	case replyWebClients:
		data, err = json.Marshal(reply.webClients)
	case replyWebRevoke:
		data, err = json.Marshal(reply.webRevoke)
	case replyRemoteStatus:
		data, err = json.Marshal(reply.remote)
	case replyError:
	default:
		return ErrProtocol
	}
	if err != nil {
		return ErrProtocol
	}
	ok := reply.kind != replyError
	envelope := responseEnvelope{OK: &ok}
	if ok {
		envelope.Data = data
	} else {
		envelope.Error = reply.code
	}
	encoded, err := json.Marshal(envelope)
	if err != nil {
		return ErrProtocol
	}
	if len(encoded)+responsePrelude > maxFrameBytes {
		if reply.kind == replyError {
			return ErrProtocol
		}
		tooLarge, _ := NewErrorReply(RemoteTooLarge)
		return connection.writeReply(tooLarge)
	}
	payload := make([]byte, responsePrelude+len(encoded))
	payload[0] = connection.domain
	copy(payload[responsePrelude:], encoded)
	// Every dispatched outcome response carries a receipt, including an error
	// produced after the durable outcome committed. Receipt presence must not
	// depend on a fallible post-commit response projection.
	withReceipt := outcomeCall(connection.kind)
	if withReceipt {
		if _, err := rand.Read(connection.receipt[:]); err != nil {
			return ErrTransport
		}
	}
	if err := writeFrame(connection.connection, payload); err != nil {
		clear(connection.receipt[:])
		return ErrTransport
	}
	if withReceipt {
		if err := writeFrame(connection.connection, connection.receipt[:]); err != nil {
			clear(connection.receipt[:])
			return ErrTransport
		}
		connection.receiptDue = true
	}
	if err := connection.connection.CloseWrite(); err != nil {
		clear(connection.receipt[:])
		connection.receiptDue = false
		return ErrTransport
	}
	return nil
}

func (connection *Connection) Close() error {
	if connection == nil {
		return nil
	}
	if connection.self != connection {
		return ErrProtocol
	}
	connection.closeOnce.Do(func() {
		connection.state = connectionClosed
		clear(connection.receipt[:])
		connection.receiptDue = false
		if connection.connection != nil {
			if err := connection.connection.Close(); err != nil {
				connection.closeErr = ErrTransport
			}
		}
	})
	return connection.closeErr
}
