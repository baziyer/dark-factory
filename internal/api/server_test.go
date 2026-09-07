//go:build darwin || linux

package api

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

type serverReceiveResult struct {
	call Call
	err  error
}

var apiTestHomes sync.Map

func newAPITestListener(t testing.TB, bearer credential) (*Listener, string) {
	t.Helper()
	directory := privateTestDirectory(t)
	homePath := filepath.Join(directory, "home")
	if socket := install.LocalAPISocketPath(homePath); len(socket) > install.MaxSocketPathBytes {
		t.Fatalf("api socket path is %d bytes, over the %d-byte budget: %q", len(socket), install.MaxSocketPathBytes, socket)
	}
	if _, err := install.Init(context.Background(), homePath); err != nil {
		if errors.Is(err, install.ErrUnsupported) {
			t.Skip("operational local API is unsupported on this platform")
		}
		t.Fatal(err)
	}
	tokenPath := filepath.Join(homePath, "operator.token")
	writeTestToken(t, tokenPath, bearer)
	home, err := install.OpenOperationalHome(context.Background(), homePath)
	if err != nil {
		t.Fatal(err)
	}
	authority, err := home.OpenLocalAPI(context.Background())
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	listener, err := Listen(authority)
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	apiTestHomes.Store(listener, home)
	t.Cleanup(func() {
		closeAPITestListener(t, listener)
	})
	socketPath := install.LocalAPISocketPath(homePath)
	return listener, socketPath
}

func closeAPITestListener(t testing.TB, listener *Listener) {
	t.Helper()
	if err := listener.Close(); err != nil {
		t.Errorf("close local API listener: %v", err)
	}
	if value, ok := apiTestHomes.LoadAndDelete(listener); ok {
		if err := value.(*install.OperationalHome).Close(); err != nil {
			t.Errorf("close operational API home: %v", err)
		}
	}
}

// The receive budget is the test's own lifetime rather than a fixed second.
// This is not what any observed flake was about -- `setDeadline` still caps the
// wait at `requestTimeout` when the context carries no earlier deadline, so the
// change buys headroom (one second to five), not the removal of a wall clock. It is here because a helper's
// arbitrary timer racing the client script it serves is a hazard worth not
// having, and `t.Context()` ends exactly when the test does.
func startServerReceive(t testing.TB, listener *Listener, reply func(Call) Reply) <-chan serverReceiveResult {
	done := make(chan serverReceiveResult, 1)
	ctx := t.Context()
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			done <- serverReceiveResult{err: err}
			return
		}
		defer connection.Close()
		call, err := connection.Receive(ctx)
		if err == nil && reply != nil {
			var response Reply
			response, err = connection.Dispatch(reply)
			if err == nil {
				err = connection.Respond(response)
			}
			if err == nil {
				err = connection.AwaitOutcomeReceipt(ctx)
			}
		}
		done <- serverReceiveResult{call: call, err: err}
	}()
	return done
}

func rawRequest(domain byte, bearer credential, body []byte) []byte {
	payload := make([]byte, requestPrelude+len(body))
	payload[0] = domain
	copy(payload[1:requestPrelude], bearer[:])
	copy(payload[requestPrelude:], body)
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(payload)))
	return append(header[:], payload...)
}

func exchangeRaw(socketPath string, request []byte, closeWrite bool) ([]byte, error) {
	connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	_ = connection.SetDeadline(time.Now().Add(time.Second))
	if _, err := connection.Write(request); err != nil {
		return nil, err
	}
	outcome := rawOutcomeRequest(request)
	if closeWrite && !outcome {
		// The server may reject and close before this half-close reaches it.
		// The response read below is the proof for this helper; preserve errors
		// other than the expected BSD peer-close race.
		if err := connection.CloseWrite(); err != nil && !errors.Is(err, syscall.ENOTCONN) {
			return nil, err
		}
	}
	response, err := readTestFrame(connection)
	if err != nil {
		return nil, err
	}
	if outcome && len(response) >= responsePrelude && bytes.Contains(response[responsePrelude:], []byte(`"ok":true`)) {
		receipt, err := readTestFrame(connection)
		if err != nil {
			return nil, err
		}
		if len(receipt) != outcomeReceiptBytes {
			return nil, ErrProtocol
		}
		if err := requireEOF(connection); err != nil {
			return nil, err
		}
		if err := writeFrame(connection, receipt); err != nil {
			return nil, err
		}
		if err := connection.CloseWrite(); err != nil {
			return nil, err
		}
		return response, nil
	}
	if err := requireEOF(connection); err != nil {
		return nil, err
	}
	return response, nil
}

func rawOutcomeRequest(request []byte) bool {
	if len(request) < 4+requestPrelude {
		return false
	}
	encoded := request[4+requestPrelude:]
	return bytes.Contains(encoded, []byte(`"method":"succeed"`)) ||
		bytes.Contains(encoded, []byte(`"method":"block"`)) ||
		bytes.Contains(encoded, []byte(`"method":"fail"`))
}

func decodeTestResponse(t testing.TB, payload []byte, domain byte, output any) error {
	t.Helper()
	if len(payload) < responsePrelude || payload[0] != domain {
		t.Fatalf("response prelude = %v", payload)
	}
	return decodeResponse(payload[responsePrelude:], output)
}

func replyForCall(call Call) Reply {
	switch call.Kind() {
	case CallHealth:
		return NewHealthReply(HealthStatus{Ready: true})
	case CallSnapshot:
		reply, _ := NewSnapshotReply(DashboardSnapshot{
			Head: 1, Factory: FactorySummary{Capacity: 1, Revision: 1},
			Projects: []ProjectSummary{}, Agents: []AgentSummary{}, Tasks: []TaskSummary{},
		})
		return reply
	case CallAttemptTask:
		reply, _ := NewAttemptTaskReply(AttemptTask{Task: "private-attempt-task"})
		return reply
	default:
		reply, _ := NewMutationReply(MutationResult{Head: 0, Revision: 1})
		return reply
	}
}

func TestListenerCreatesExactOwnerSocketAndGuardsRemoval(t *testing.T) {
	bearer := testCredential('L')
	listener, socketPath := newAPITestListener(t, bearer)
	info, err := os.Lstat(socketPath)
	if err != nil {
		t.Fatal(err)
	}
	identity, ok := identityOf(info)
	if !ok || !validSocketInfo(info) || identity.uid != uint32(os.Geteuid()) {
		t.Fatalf("listener socket metadata = %#v", info)
	}
	closeAPITestListener(t, listener)
	if _, err := os.Lstat(socketPath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("closed listener socket remains: %v", err)
	}
	if got, err := Listen(nil); got != nil || !errors.Is(err, ErrInvalidListener) {
		t.Fatalf("nil authority listener = %+v, %v", got, err)
	}
}

func TestServerDecodesClosedMethodMatrix(t *testing.T) {
	operatorBearer := testCredential('O')
	attemptBearer := testCredential('A')
	tests := []struct {
		name   string
		domain byte
		bearer credential
		body   string
		kind   CallKind
		check  func(*testing.T, Call)
	}{
		{name: "health", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"health","params":{}}`, kind: CallHealth},
		{name: "snapshot", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"snapshot","params":{}}`, kind: CallSnapshot},
		{name: "create project", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"create_project","params":{"id":"` + id('1') + `","name":"project","root":"/private/sentinel-root"}}`, kind: CallCreateProject, check: func(t *testing.T, call Call) {
			input, ok := call.CreateProjectInput()
			if !ok || input.Root != "/private/sentinel-root" {
				t.Fatalf("project input = %+v, %t", input, ok)
			}
		}},
		{name: "create project paired surrogate", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"create_project","params":{"id":"` + id('1') + `","name":"\ud83d\ude00","root":"/private/project"}}`, kind: CallCreateProject, check: func(t *testing.T, call Call) {
			input, ok := call.CreateProjectInput()
			if !ok || input.Name != "😀" {
				t.Fatalf("paired project input = %+v, %t", input, ok)
			}
		}},
		{name: "create project literal replacement", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"create_project","params":{"id":"` + id('1') + `","name":"�","root":"/private/project"}}`, kind: CallCreateProject, check: func(t *testing.T, call Call) {
			input, ok := call.CreateProjectInput()
			if !ok || input.Name != "�" {
				t.Fatalf("literal replacement project input = %+v, %t", input, ok)
			}
		}},
		{name: "create agent", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"codex","model":"gpt-5.6-luna","reasoning_effort":"medium","tool_budget_limit":20}}`, kind: CallCreateAgent, check: func(t *testing.T, call Call) {
			input, ok := call.CreateAgentInput()
			if !ok || input.Provider != "codex" || input.Model != "gpt-5.6-luna" || input.ReasoningEffort != "medium" || input.ToolBudgetLimit != 20 {
				t.Fatalf("agent input = %+v, %t", input, ok)
			}
		}},
		{name: "enqueue task", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"enqueue_task","params":{"id":"` + id('3') + `","project_id":"` + id('1') + `","assigned_agent_id":"` + id('2') + `","incarnation_id":"` + id('4') + `","title":"task","body":"private-body-sentinel","priority":7}}`, kind: CallEnqueueTask, check: func(t *testing.T, call Call) {
			input, ok := call.EnqueueTaskInput()
			if !ok || input.Body != "private-body-sentinel" {
				t.Fatalf("task input = %+v, %t", input, ok)
			}
		}},
		{name: "set dispatch", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"set_dispatch","params":{"expected_revision":3,"enabled":true}}`, kind: CallSetDispatch, check: func(t *testing.T, call Call) {
			revision, enabled, ok := call.Dispatch()
			if !ok || revision != 3 || !enabled {
				t.Fatalf("dispatch = %d, %t, %t", revision, enabled, ok)
			}
		}},
		{name: "task", domain: attemptDomain, bearer: attemptBearer, body: `{"method":"task","params":{}}`, kind: CallAttemptTask},
		{name: "succeed", domain: attemptDomain, bearer: attemptBearer, body: `{"method":"succeed","params":{"result":"private-result-sentinel"}}`, kind: CallSucceed, check: func(t *testing.T, call Call) {
			result, ok := call.Result()
			if !ok || result != "private-result-sentinel" {
				t.Fatalf("result = %q, %t", result, ok)
			}
		}},
		{name: "block", domain: attemptDomain, bearer: attemptBearer, body: `{"method":"block","params":{"detail":"blocked"}}`, kind: CallBlock},
		{name: "fail empty detail", domain: attemptDomain, bearer: attemptBearer, body: `{"method":"fail","params":{"detail":""}}`, kind: CallFail, check: func(t *testing.T, call Call) {
			detail, ok := call.Detail()
			if !ok || detail != "" {
				t.Fatalf("failure detail = %q, %t", detail, ok)
			}
		}},
		{name: "send back", domain: attemptDomain, bearer: attemptBearer, body: `{"method":"send_back","params":{"task_id":"0123456789abcdef0123456789abcdef","note":"private-note-sentinel"}}`, kind: CallSendBack, check: func(t *testing.T, call Call) {
			input, ok := call.SendBackInput()
			if !ok || input.TaskID != "0123456789abcdef0123456789abcdef" || input.Note != "private-note-sentinel" {
				t.Fatalf("send-back input = %+v, %t", input, ok)
			}
			if _, ok := call.AttemptDigest(); !ok {
				t.Fatal("send-back carries no attempt digest")
			}
		}},
		{name: "send back task", domain: operatorDomain, bearer: operatorBearer, body: `{"method":"send_back_task","params":{"task_id":"0123456789abcdef0123456789abcdef","note":"operator note"}}`, kind: CallSendBackTask, check: func(t *testing.T, call Call) {
			input, ok := call.SendBackInput()
			if !ok || input.TaskID != "0123456789abcdef0123456789abcdef" || input.Note != "operator note" {
				t.Fatalf("send-back task input = %+v, %t", input, ok)
			}
			if _, ok := call.AttemptDigest(); ok {
				t.Fatal("operator send-back carries an attempt digest")
			}
		}},
		{name: "request human", domain: attemptDomain, bearer: attemptBearer, body: `{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"private-question-sentinel"}}`, kind: CallRequestHuman, check: func(t *testing.T, call Call) {
			input, ok := call.HumanQuestionInput()
			if !ok || input.IdempotencyKey != "0123456789abcdef0123456789abcdef" || input.Question != "private-question-sentinel" {
				t.Fatalf("human question input = %+v, %t", input, ok)
			}
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			listener, socketPath := newAPITestListener(t, operatorBearer)
			done := startServerReceive(t, listener, replyForCall)
			response, err := exchangeRaw(socketPath, rawRequest(test.domain, test.bearer, []byte(test.body)), true)
			if err != nil {
				t.Fatal(err)
			}
			var output any
			switch test.kind {
			case CallHealth:
				output = &HealthStatus{}
			case CallSnapshot:
				output = &DashboardSnapshot{}
			case CallAttemptTask:
				output = &AttemptTask{}
			default:
				output = &MutationResult{}
			}
			if err := decodeTestResponse(t, response, test.domain, output); err != nil {
				t.Fatal(err)
			}
			result := <-done
			if result.err != nil || result.call.Kind() != test.kind {
				t.Fatalf("receive = %v, %v", result.call.Kind(), result.err)
			}
			if test.check != nil {
				test.check(t, result.call)
			}
			if test.domain == attemptDomain {
				digest, ok := result.call.AttemptDigest()
				want := sha256.Sum256(attemptBearer[:])
				if !ok || digest.Bytes() != want {
					t.Fatalf("attempt digest = %x, %t", digest.Bytes(), ok)
				}
			} else if _, ok := result.call.AttemptDigest(); ok {
				t.Fatal("operator call exposed attempt digest")
			}
			formatted := fmt.Sprintf("%v %+v %#v", result.call, result.call, result.call)
			encoded, err := json.Marshal(result.call)
			if err != nil {
				t.Fatal(err)
			}
			for _, private := range []string{string(operatorBearer[:]), string(attemptBearer[:]), "private-body-sentinel", "private-result-sentinel", "/private/sentinel-root"} {
				if strings.Contains(formatted+string(encoded), private) {
					t.Fatalf("public call formatting exposed private value: %s %s", formatted, encoded)
				}
			}
		})
	}
}

func TestOutcomeReceiptIsExactAndRequired(t *testing.T) {
	for _, test := range []struct {
		name       string
		errorReply bool
		wantError  bool
		send       func(*net.UnixConn, []byte) error
	}{
		{name: "wrong receipt", wantError: true, send: func(connection *net.UnixConn, receipt []byte) error {
			receipt[0] ^= 0xff
			if err := writeFrame(connection, receipt); err != nil {
				return err
			}
			return connection.CloseWrite()
		}},
		{name: "dropped receipt", wantError: true, send: func(connection *net.UnixConn, _ []byte) error {
			return connection.CloseWrite()
		}},
		{name: "error response exact receipt", errorReply: true, send: func(connection *net.UnixConn, receipt []byte) error {
			if err := writeFrame(connection, receipt); err != nil {
				return err
			}
			return connection.CloseWrite()
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			bearer := testCredential('A')
			listener, socketPath := newAPITestListener(t, testCredential('O'))
			reply := replyForCall
			if test.errorReply {
				reply = func(Call) Reply {
					result, _ := NewErrorReply(RemoteUnavailable)
					return result
				}
			}
			done := startServerReceive(t, listener, reply)
			connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
			if err != nil {
				t.Fatal(err)
			}
			defer connection.Close()
			if err := connection.SetDeadline(time.Now().Add(2 * time.Second)); err != nil {
				t.Fatal(err)
			}
			request := rawRequest(attemptDomain, bearer, []byte(`{"method":"succeed","params":{"result":"receipt-test"}}`))
			if _, err := connection.Write(request); err != nil {
				t.Fatal(err)
			}
			if _, err := readTestFrame(connection); err != nil {
				t.Fatal(err)
			}
			receipt, err := readTestFrame(connection)
			if err != nil || len(receipt) != outcomeReceiptBytes {
				t.Fatalf("receipt = %x, %v", receipt, err)
			}
			if err := requireEOF(connection); err != nil {
				t.Fatal(err)
			}
			if err := test.send(connection, receipt); err != nil {
				t.Fatal(err)
			}
			result := <-done
			if result.call.Kind() != CallSucceed || (result.err != nil) != test.wantError {
				t.Fatalf("outcome receipt = %v, %v", result.call.Kind(), result.err)
			}
		})
	}
}

func TestServerRejectsDomainFallbackAndInvalidRequests(t *testing.T) {
	operatorBearer := testCredential('O')
	attemptBearer := testCredential('A')
	tests := []struct {
		name   string
		domain byte
		bearer credential
		body   []byte
		code   RemoteErrorCode
	}{
		{name: "operator bearer does not authorize attempt domain", domain: attemptDomain, bearer: operatorBearer, body: []byte(`{"method":"health","params":{}}`), code: RemoteForbidden},
		{name: "attempt bearer does not authorize operator", domain: operatorDomain, bearer: attemptBearer, body: []byte(`{"method":"health","params":{}}`), code: RemoteUnauthorized},
		{name: "operator domain cannot invoke attempt", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"fail","params":{"detail":"x"}}`), code: RemoteForbidden},
		{name: "operator domain cannot read attempt task", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"task","params":{}}`), code: RemoteForbidden},
		{name: "unknown method", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"delete_all","params":{}}`), code: RemoteInvalidRequest},
		{name: "null params", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"health","params":null}`), code: RemoteInvalidRequest},
		{name: "array params", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"health","params":[]}`), code: RemoteInvalidRequest},
		{name: "scalar params", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"health","params":1}`), code: RemoteInvalidRequest},
		{name: "duplicate method", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"health","method":"snapshot","params":{}}`), code: RemoteInvalidRequest},
		{name: "casefold method", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"health","Method":"snapshot","params":{}}`), code: RemoteInvalidRequest},
		{name: "nested duplicate", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"set_dispatch","params":{"expected_revision":1,"expected_revision":2,"enabled":true}}`), code: RemoteInvalidRequest},
		{name: "nested noncanonical", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"set_dispatch","params":{"Expected_revision":1,"enabled":true}}`), code: RemoteInvalidRequest},
		{name: "removed shell-only create alias", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_shell_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","tool_budget_limit":20}}`), code: RemoteInvalidRequest},
		{name: "create agent requires provider", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","tool_budget_limit":20}}`), code: RemoteInvalidRequest},
		{name: "shell agent rejects model", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"shell","model":"private","tool_budget_limit":20}}`), code: RemoteInvalidRequest},
		{name: "claude rejects ultra effort", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"claude_code","reasoning_effort":"ultra","tool_budget_limit":20}}`), code: RemoteInvalidRequest},
		{name: "empty block detail", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"block","params":{"detail":""}}`), code: RemoteInvalidRequest},
		{name: "oversized failure detail", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"fail","params":{"detail":"` + strings.Repeat("x", 4097) + `"}}`), code: RemoteInvalidRequest},
		{name: "request human operator domain", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"question"}}`), code: RemoteForbidden},
		{name: "send back operator domain", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"send_back","params":{"task_id":"0123456789abcdef0123456789abcdef","note":"n"}}`), code: RemoteForbidden},
		{name: "send back task attempt domain", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"send_back_task","params":{"task_id":"0123456789abcdef0123456789abcdef","note":"n"}}`), code: RemoteForbidden},
		{name: "send back short task", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"send_back","params":{"task_id":"0123456789abcdef","note":"n"}}`), code: RemoteInvalidRequest},
		{name: "send back empty note", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"send_back","params":{"task_id":"0123456789abcdef0123456789abcdef","note":""}}`), code: RemoteInvalidRequest},
		{name: "send back oversized note", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"send_back","params":{"task_id":"0123456789abcdef0123456789abcdef","note":"` + strings.Repeat("x", 8193) + `"}}`), code: RemoteInvalidRequest},
		{name: "request human zero key", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"00000000000000000000000000000000","question":"question"}}`), code: RemoteInvalidRequest},
		{name: "request human uppercase key", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789ABCDEF0123456789abcdef","question":"question"}}`), code: RemoteInvalidRequest},
		{name: "request human nonhex key", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdegf","question":"question"}}`), code: RemoteInvalidRequest},
		{name: "request human empty question", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":""}}`), code: RemoteInvalidRequest},
		{name: "request human oversized question", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"` + strings.Repeat("x", 8193) + `"}}`), code: RemoteInvalidRequest},
		{name: "request human duplicate question", domain: attemptDomain, bearer: attemptBearer, body: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"one","question":"two"}}`), code: RemoteInvalidRequest},
		{name: "invalid UTF-8", domain: operatorDomain, bearer: operatorBearer, body: []byte("{\"method\":\"health\",\"params\":{},\"x\":\"\xff\"}"), code: RemoteInvalidRequest},
		{name: "lone high surrogate", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_project","params":{"id":"` + id('1') + `","name":"\ud800","root":"/private/project"}}`), code: RemoteInvalidRequest},
		{name: "lone low surrogate", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_project","params":{"id":"` + id('1') + `","name":"\udc00","root":"/private/project"}}`), code: RemoteInvalidRequest},
		{name: "reversed surrogate pair", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_project","params":{"id":"` + id('1') + `","name":"\udc00\ud800","root":"/private/project"}}`), code: RemoteInvalidRequest},
		{name: "mismatched surrogate pair", domain: operatorDomain, bearer: operatorBearer, body: []byte(`{"method":"create_project","params":{"id":"` + id('1') + `","name":"\ud800\u0041","root":"/private/project"}}`), code: RemoteInvalidRequest},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			listener, socketPath := newAPITestListener(t, operatorBearer)
			done := startServerReceive(t, listener, nil)
			response, err := exchangeRaw(socketPath, rawRequest(test.domain, test.bearer, test.body), true)
			if err != nil {
				t.Fatal(err)
			}
			var output HealthStatus
			responseDomain := test.domain
			if err := decodeTestResponse(t, response, responseDomain, &output); err == nil {
				t.Fatal("invalid request succeeded")
			} else {
				var remote *RemoteError
				if !errors.As(err, &remote) || remote.Code() != test.code {
					t.Fatalf("remote error = %v, want %s", err, test.code)
				}
			}
			if result := <-done; !errors.Is(result.err, ErrProtocol) || result.call.Kind() != 0 {
				t.Fatalf("invalid request dispatched: %+v, %v", result.call, result.err)
			}
		})
	}
}

// The local API tolerates a member this build does not know, so a newer
// factoryctl and an older factoryd still talk. Tolerance is only additive: an
// added member reaches no field, so it cannot become authority. Each case
// below is a value a caller must never be able to supply -- an entity id, a
// failure code, a run destination, an action, a removed launch policy -- and
// the proof is that the derived call is byte-identical to the call decoded
// from the same request without it.
func TestUnknownMembersAreIgnoredAndCannotBecomeAuthority(t *testing.T) {
	operatorBearer := testCredential('O')
	attemptBearer := testCredential('A')
	receive := func(t *testing.T, domain byte, bearer credential, body []byte) Call {
		t.Helper()
		listener, socketPath := newAPITestListener(t, operatorBearer)
		done := startServerReceive(t, listener, nil)
		// The helper never dispatches, so the server closes without a reply;
		// the received call below is the observation this test needs.
		_, _ = exchangeRaw(socketPath, rawRequest(domain, bearer, body), true)
		result := <-done
		if result.err != nil {
			t.Fatalf("tolerant request refused: %v", result.err)
		}
		return result.call
	}
	tests := []struct {
		name           string
		domain         byte
		bearer         credential
		clean, mutated []byte
	}{
		{
			name: "operator cannot smuggle a dispatch member", domain: operatorDomain, bearer: operatorBearer,
			clean:   []byte(`{"method":"set_dispatch","params":{"expected_revision":1,"enabled":true}}`),
			mutated: []byte(`{"method":"set_dispatch","params":{"expected_revision":1,"enabled":true,"private":"sentinel"}}`),
		},
		{
			name: "removed selectable launch policy", domain: operatorDomain, bearer: operatorBearer,
			clean:   []byte(`{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"shell","tool_budget_limit":20}}`),
			mutated: []byte(`{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"shell","tool_budget_limit":20,"execution_mode":"unrestricted"}}`),
		},
		{
			name: "attempt cannot supply an id", domain: attemptDomain, bearer: attemptBearer,
			clean:   []byte(`{"method":"fail","params":{"detail":"x"}}`),
			mutated: []byte(`{"method":"fail","params":{"detail":"x","id":"` + id('1') + `"}}`),
		},
		{
			name: "attempt task takes no selector", domain: attemptDomain, bearer: attemptBearer,
			clean:   []byte(`{"method":"task","params":{}}`),
			mutated: []byte(`{"method":"task","params":{"id":"` + id('1') + `"}}`),
		},
		{
			name: "attempt cannot supply a failure code", domain: attemptDomain, bearer: attemptBearer,
			clean:   []byte(`{"method":"fail","params":{"detail":"x"}}`),
			mutated: []byte(`{"method":"fail","params":{"detail":"x","code":"internal"}}`),
		},
		{
			name: "request_human takes no run destination", domain: attemptDomain, bearer: attemptBearer,
			clean:   []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"question"}}`),
			mutated: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"question","run_id":"` + id('1') + `"}}`),
		},
		{
			name: "request_human takes no action", domain: attemptDomain, bearer: attemptBearer,
			clean:   []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"question"}}`),
			mutated: []byte(`{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"question","action":"publish"}}`),
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			clean := receive(t, test.domain, test.bearer, test.clean)
			mutated := receive(t, test.domain, test.bearer, test.mutated)
			if !reflect.DeepEqual(clean, mutated) {
				t.Fatalf("added member changed the call:\n got %+v\nwant %+v", mutated, clean)
			}
			if strings.Contains(fmt.Sprintf("%+v", mutated), "sentinel") || strings.Contains(fmt.Sprintf("%+v", mutated), "unrestricted") || strings.Contains(fmt.Sprintf("%+v", mutated), "publish") {
				t.Fatalf("added member reached the call: %+v", mutated)
			}
		})
	}
}

// Tolerance is additive only: an oversized frame is still refused, and the
// added member counts toward the same bound.
func TestUnknownMembersDoNotRelaxTheFrameBound(t *testing.T) {
	bearer := testCredential('O')
	listener, socketPath := newAPITestListener(t, bearer)
	done := startServerReceive(t, listener, nil)
	body := []byte(`{"method":"health","params":{},"future":"` + strings.Repeat("x", maxFrameBytes) + `"}`)
	if _, err := exchangeRaw(socketPath, rawRequest(operatorDomain, bearer, body), true); err == nil {
		t.Fatal("oversized tolerant request was answered")
	}
	if result := <-done; result.err == nil || result.call.Kind() != 0 {
		t.Fatalf("oversized tolerant request dispatched: %+v, %v", result.call, result.err)
	}
}

func TestServerRequiresRequestEOFBeforedispatch(t *testing.T) {
	bearer := testCredential('E')
	listener, socketPath := newAPITestListener(t, bearer)
	done := startServerReceive(t, listener, replyForCall)
	connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	request := rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`))
	if _, err := connection.Write(request); err != nil {
		t.Fatal(err)
	}
	select {
	case result := <-done:
		t.Fatalf("request dispatched before EOF: %+v", result)
	case <-time.After(50 * time.Millisecond):
	}
	if _, err := connection.Write(request); err != nil {
		t.Fatal(err)
	}
	// The second frame is what this test wants rejected, and the server rejects
	// it the moment it arrives: `requireEOF` reads a byte, finds data, and the
	// receive helper returns and closes its end. Whether that beats this
	// half-close is a race with a microsecond window that no ordering here can
	// remove -- and losing it is not a failure. On BSD, shutting down a socket
	// whose peer has already closed is ENOTCONN. The rejection frame was
	// written before that close, so it is still readable, and every assertion
	// that matters is below.
	if err := connection.CloseWrite(); err != nil && !errors.Is(err, syscall.ENOTCONN) {
		t.Fatal(err)
	}
	if _, err := readTestFrame(connection); err != nil {
		t.Fatal(err)
	}
	if result := <-done; !errors.Is(result.err, ErrProtocol) || result.call.Kind() != 0 {
		t.Fatalf("second frame dispatched: %+v, %v", result.call, result.err)
	}
}

func TestServerFramingDeadlinePeerAndResponseBounds(t *testing.T) {
	bearer := testCredential('F')
	t.Run("frame bounds and truncation", func(t *testing.T) {
		requests := [][]byte{
			{0, 0, 0, 0},
			{0, 0x10, 0, 1},
			{0, 0},
			{0, 0, 0, 10, 1, operatorDomain},
		}
		for index, request := range requests {
			t.Run(fmt.Sprint(index), func(t *testing.T) {
				listener, socketPath := newAPITestListener(t, bearer)
				done := startServerReceive(t, listener, nil)
				connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
				if err != nil {
					t.Fatal(err)
				}
				if _, err := connection.Write(request); err != nil {
					t.Fatal(err)
				}
				if err := connection.CloseWrite(); err != nil && !errors.Is(err, syscall.ENOTCONN) {
					t.Fatal(err)
				}
				_ = connection.Close()
				if result := <-done; result.err == nil || result.call.Kind() != 0 {
					t.Fatalf("bad frame dispatched: %+v, %v", result.call, result.err)
				}
			})
		}
	})

	t.Run("blocked read deadline", func(t *testing.T) {
		listener, socketPath := newAPITestListener(t, bearer)
		done := make(chan error, 1)
		go func() {
			connection, err := listener.Accept()
			if err != nil {
				done <- err
				return
			}
			defer connection.Close()
			ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
			defer cancel()
			_, err = connection.Receive(ctx)
			done <- err
		}()
		connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
		if err != nil {
			t.Fatal(err)
		}
		defer connection.Close()
		started := time.Now()
		if err := <-done; !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("blocked read = %v", err)
		}
		if elapsed := time.Since(started); elapsed > 500*time.Millisecond {
			t.Fatalf("blocked read took %v", elapsed)
		}
	})

	t.Run("complete frame without EOF never dispatches", func(t *testing.T) {
		listener, socketPath := newAPITestListener(t, bearer)
		done := make(chan serverReceiveResult, 1)
		go func() {
			connection, err := listener.Accept()
			if err != nil {
				done <- serverReceiveResult{err: err}
				return
			}
			defer connection.Close()
			ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
			defer cancel()
			call, err := connection.Receive(ctx)
			done <- serverReceiveResult{call: call, err: err}
		}()
		connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
		if err != nil {
			t.Fatal(err)
		}
		defer connection.Close()
		request := rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`))
		if _, err := connection.Write(request); err != nil {
			t.Fatal(err)
		}
		result := <-done
		if !errors.Is(result.err, context.DeadlineExceeded) || result.call.Kind() != 0 {
			t.Fatalf("missing EOF dispatched: %+v, %v", result.call, result.err)
		}
	})

	t.Run("oversized snapshot becomes fixed too large", func(t *testing.T) {
		listener, socketPath := newAPITestListener(t, bearer)
		tasks := make([]TaskSummary, maxSnapshotEntries)
		for index := range tasks {
			tasks[index] = TaskSummary{ID: id('1'), ProjectID: id('2'), AssignedAgentID: id('3'), Title: strings.Repeat("x", 1024), Status: "queued", Revision: 1}
		}
		snapshot := DashboardSnapshot{Head: 1, Factory: FactorySummary{Capacity: 1, Revision: 1}, Projects: []ProjectSummary{}, Agents: []AgentSummary{}, Tasks: tasks}
		reply, err := NewSnapshotReply(snapshot)
		if err != nil {
			t.Fatal(err)
		}
		done := startServerReceive(t, listener, func(Call) Reply { return reply })
		response, err := exchangeRaw(socketPath, rawRequest(operatorDomain, bearer, []byte(`{"method":"snapshot","params":{}}`)), true)
		if err != nil {
			t.Fatal(err)
		}
		var output DashboardSnapshot
		err = decodeTestResponse(t, response, operatorDomain, &output)
		var remote *RemoteError
		if !errors.As(err, &remote) || remote.Code() != RemoteTooLarge {
			t.Fatalf("oversized response = %v", err)
		}
		if result := <-done; result.err != nil {
			t.Fatal(result.err)
		}
	})
}

func TestServerReceiveCancellationCutsJoinWatcher(t *testing.T) {
	bearer := testCredential('K')
	var partialPayloadHeader [4]byte
	binary.BigEndian.PutUint32(partialPayloadHeader[:], 128)
	cuts := []struct {
		name  string
		write []byte
	}{
		{name: "partial header", write: []byte{0, 0}},
		{name: "partial payload", write: append(partialPayloadHeader[:], 1, operatorDomain, 'x')},
		{name: "missing request EOF", write: rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`))},
	}
	for _, cut := range cuts {
		t.Run(cut.name, func(t *testing.T) {
			baselineGoroutines := runtime.NumGoroutine()
			baselineFDs := countTestFDs(t)
			for range 20 {
				listener, socketPath := newAPITestListener(t, bearer)
				ctx, cancel := context.WithCancel(context.Background())
				accepted := make(chan struct{})
				done := make(chan serverReceiveResult, 1)
				go func() {
					connection, err := listener.Accept()
					if err != nil {
						done <- serverReceiveResult{err: err}
						return
					}
					close(accepted)
					call, receiveErr := connection.Receive(ctx)
					firstClose := connection.Close()
					secondClose := connection.Close()
					if firstClose != nil {
						receiveErr = firstClose
					}
					if secondClose != nil {
						receiveErr = secondClose
					}
					done <- serverReceiveResult{call: call, err: receiveErr}
				}()
				client, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
				if err != nil {
					cancel()
					t.Fatal(err)
				}
				<-accepted
				if _, err := client.Write(cut.write); err != nil {
					cancel()
					client.Close()
					t.Fatal(err)
				}
				time.Sleep(5 * time.Millisecond)
				started := time.Now()
				cancel()
				select {
				case result := <-done:
					if !errors.Is(result.err, context.Canceled) || result.call.Kind() != 0 {
						t.Fatalf("cancelled receive dispatched: %+v, %v", result.call, result.err)
					}
				case <-time.After(500 * time.Millisecond):
					t.Fatal("cancelled receive retained its connection")
				}
				if elapsed := time.Since(started); elapsed > 500*time.Millisecond {
					t.Fatalf("cancelled receive took %v", elapsed)
				}
				if err := client.SetReadDeadline(time.Now().Add(100 * time.Millisecond)); err != nil {
					t.Fatal(err)
				}
				if count, err := client.Read(make([]byte, 1)); count != 0 || err == nil {
					t.Fatalf("server connection remained open: count=%d error=%v", count, err)
				}
				if err := client.Close(); err != nil {
					t.Fatal(err)
				}
				closeAPITestListener(t, listener)
			}
			deadline := time.Now().Add(500 * time.Millisecond)
			for runtime.NumGoroutine() != baselineGoroutines && time.Now().Before(deadline) {
				time.Sleep(5 * time.Millisecond)
			}
			if after := runtime.NumGoroutine(); after != baselineGoroutines {
				t.Fatalf("cancelled receives changed goroutine census: before=%d after=%d", baselineGoroutines, after)
			}
			if after := countTestFDs(t); after != baselineFDs {
				t.Fatalf("cancelled receives changed FD census: before=%d after=%d", baselineFDs, after)
			}
		})
	}
}

func TestServerRechecksOperatorTokenAndKeepsPublicValuesPrivate(t *testing.T) {
	if os.Getenv("DARK_FACTORY_API_POISON_CHILD") != "1" {
		command := exec.Command(os.Args[0], "-test.run=^TestServerRechecksOperatorTokenAndKeepsPublicValuesPrivate$")
		command.Env = append(os.Environ(), "DARK_FACTORY_API_POISON_CHILD=1")
		if output, err := command.CombinedOutput(); err != nil {
			t.Fatalf("isolated poison proof failed: %v\n%s", err, output)
		}
		return
	}
	bearer := testCredential('S')
	listener, socketPath := newAPITestListener(t, bearer)
	tokenPath := filepath.Join(filepath.Dir(filepath.Dir(socketPath)), "operator.token")
	acceptOne := func() (*net.UnixConn, <-chan serverReceiveResult) {
		accepted := make(chan struct{})
		done := make(chan serverReceiveResult, 1)
		go func() {
			connection, err := listener.Accept()
			if err != nil {
				done <- serverReceiveResult{err: err}
				return
			}
			defer connection.Close()
			close(accepted)
			call, err := connection.Receive(context.Background())
			done <- serverReceiveResult{call: call, err: err}
		}()
		client, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
		if err != nil {
			t.Fatal(err)
		}
		<-accepted
		return client, done
	}
	client, done := acceptOne()
	attemptClient, attemptDone := acceptOne()
	defer attemptClient.Close()
	defer client.Close()
	saved := tokenPath + ".saved"
	if err := os.Rename(tokenPath, saved); err != nil {
		t.Fatal(err)
	}
	replacement := filepath.Join(filepath.Dir(tokenPath), "replacement.token")
	writeTestToken(t, replacement, bearer)
	if err := os.Rename(replacement, tokenPath); err != nil {
		t.Fatal(err)
	}
	request := rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`))
	if _, err := client.Write(request); err != nil {
		t.Fatal(err)
	}
	if err := client.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	response, err := readTestFrame(client)
	if err != nil {
		t.Fatal(err)
	}
	var status HealthStatus
	err = decodeTestResponse(t, response, operatorDomain, &status)
	var remote *RemoteError
	if !errors.As(err, &remote) || remote.Code() != RemoteUnauthorized {
		t.Fatalf("replaced token = %v", err)
	}
	if result := <-done; !errors.Is(result.err, ErrProtocol) {
		t.Fatalf("replaced token dispatched: %+v, %v", result.call, result.err)
	}
	if err := os.Remove(tokenPath); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(saved, tokenPath); err != nil {
		t.Fatal(err)
	}
	attemptRequest := rawRequest(attemptDomain, testCredential('T'), []byte(`{"method":"succeed","params":{"result":"restored-must-not-dispatch"}}`))
	if _, err := attemptClient.Write(attemptRequest); err != nil {
		t.Fatal(err)
	}
	if err := attemptClient.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	attemptResponse, err := readTestFrame(attemptClient)
	if err != nil {
		t.Fatal(err)
	}
	err = decodeTestResponse(t, attemptResponse, attemptDomain, &struct{}{})
	if !errors.As(err, &remote) || remote.Code() != RemoteUnauthorized {
		t.Fatalf("restored poisoned attempt authority = %v", err)
	}
	if result := <-attemptDone; !errors.Is(result.err, ErrProtocol) {
		t.Fatalf("restored poisoned attempt dispatched: %+v, %v", result.call, result.err)
	}
	if listener.protocol.CheckOperator(bearer[:]) || !errors.Is(listener.protocol.Verify(), install.ErrUncertain) {
		t.Fatal("restored namespace revived local API authority")
	}
	values := []any{listener, *listener, AttemptDigest{value: sha256.Sum256(bearer[:])}, Call{text: "private-result-sentinel"}, Reply{code: RemoteInternal}}
	for _, value := range values {
		encoded, marshalErr := json.Marshal(value)
		if marshalErr != nil {
			t.Fatal(marshalErr)
		}
		formatted := fmt.Sprintf("%v %+v %#v %s", value, value, value, encoded)
		for _, private := range []string{string(bearer[:]), socketPath, tokenPath, "private-result-sentinel"} {
			if strings.Contains(formatted, private) {
				t.Fatalf("public formatting exposed private value: %s", formatted)
			}
		}
	}
	// The poisoned authority deliberately retains descriptors and its home
	// lease. Isolate this causal proof so process exit, not a test-only repair
	// path, releases the retained uncertain resources.
	os.Exit(0)
}

func TestListenerClaimsAuthorityExactlyOnce(t *testing.T) {
	listener, _ := newAPITestListener(t, testCredential('Q'))
	if second, err := Listen(listener.authority); second != nil || !errors.Is(err, ErrInvalidListener) {
		t.Fatalf("second protocol claim = %v, %v", second, err)
	}
	closeAPITestListener(t, listener)
	if second, err := Listen(listener.authority); second != nil || !errors.Is(err, ErrInvalidListener) {
		t.Fatalf("claim after close = %v, %v", second, err)
	}
}

func TestServerConnectionsLeaveExactResourceCensus(t *testing.T) {
	bearer := testCredential('C')
	baselineGoroutines := runtime.NumGoroutine()
	baselineFDs := countTestFDs(t)
	for range 20 {
		listener, socketPath := newAPITestListener(t, bearer)
		done := startServerReceive(t, listener, replyForCall)
		response, err := exchangeRaw(socketPath, rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`)), true)
		if err != nil {
			t.Fatal(err)
		}
		var status HealthStatus
		if err := decodeTestResponse(t, response, operatorDomain, &status); err != nil {
			t.Fatal(err)
		}
		if result := <-done; result.err != nil {
			t.Fatal(result.err)
		}
		closeAPITestListener(t, listener)
		if _, err := os.Lstat(socketPath); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("socket retained: %v", err)
		}
	}
	time.Sleep(20 * time.Millisecond)
	if delta := runtime.NumGoroutine() - baselineGoroutines; delta > 1 {
		t.Fatalf("server calls retained %d goroutines", delta)
	}
	if after := countTestFDs(t); after != baselineFDs {
		t.Fatalf("server calls changed FD census: before=%d after=%d", baselineFDs, after)
	}
}

func TestServerHomeCloseJoinsReceivedCallBeforeAuthorityRelease(t *testing.T) {
	bearer := testCredential('J')
	listener, socketPath := newAPITestListener(t, bearer)
	homeValue, ok := apiTestHomes.Load(listener)
	if !ok {
		t.Fatal("test listener lost its operational home owner")
	}
	home := homeValue.(*install.OperationalHome)
	received := make(chan Call, 1)
	release := make(chan struct{})
	handlerDone := make(chan error, 1)
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			handlerDone <- err
			return
		}
		defer connection.Close()
		call, err := connection.Receive(context.Background())
		if err != nil {
			handlerDone <- err
			return
		}
		received <- call
		<-release
		handlerDone <- nil
	}()
	client, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	request := rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`))
	if _, err := client.Write(request); err != nil {
		t.Fatal(err)
	}
	if err := client.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	select {
	case call := <-received:
		if call.Kind() != CallHealth {
			t.Fatalf("received call = %v", call.Kind())
		}
	case <-time.After(time.Second):
		t.Fatal("server did not receive the exact call")
	}
	closed := make(chan error, 1)
	go func() { closed <- home.Close() }()
	select {
	case err := <-closed:
		t.Fatalf("home close returned before received-call owner joined: %v", err)
	case <-time.After(30 * time.Millisecond):
	}
	close(release)
	if err := <-handlerDone; err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-closed:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("home close deadlocked after connection owner joined")
	}
	apiTestHomes.Delete(listener)
	_ = client.Close()
	if connection, err := net.DialTimeout("unix", socketPath, 50*time.Millisecond); err == nil {
		_ = connection.Close()
		t.Fatal("post-close connection succeeded")
	}
}

func TestConnectionDispatchLeaseLinearizesHomeClose(t *testing.T) {
	newReceived := func(t *testing.T, listener *Listener, socketPath string, bearer credential, root string) (*Connection, *net.UnixConn) {
		t.Helper()
		accepted := make(chan *Connection, 1)
		acceptErr := make(chan error, 1)
		go func() {
			connection, err := listener.Accept()
			if err != nil {
				acceptErr <- err
				return
			}
			accepted <- connection
		}()
		client, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socketPath, Net: "unix"})
		if err != nil {
			t.Fatal(err)
		}
		var connection *Connection
		select {
		case connection = <-accepted:
		case err := <-acceptErr:
			t.Fatal(err)
		case <-time.After(time.Second):
			t.Fatal("dispatch fixture was not accepted")
		}
		body := fmt.Sprintf(`{"method":"create_project","params":{"id":"%s","name":"dispatch-project","root":%q}}`, id('4'), root)
		if _, err := client.Write(rawRequest(operatorDomain, bearer, []byte(body))); err != nil {
			t.Fatal(err)
		}
		if err := client.CloseWrite(); err != nil {
			t.Fatal(err)
		}
		if call, err := connection.Receive(context.Background()); err != nil || call.Kind() != CallCreateProject {
			_ = connection.Close()
			_ = client.Close()
			t.Fatalf("received dispatch call = %v, %v", call.Kind(), err)
		}
		return connection, client
	}
	projectID, err := kernel.ProjectIDFromBytes(bytes.Repeat([]byte{'D'}, kernel.IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	at, err := kernel.NewUnixMillis(10)
	if err != nil {
		t.Fatal(err)
	}

	t.Run("shutdown begun refuses paused call", func(t *testing.T) {
		bearer := testCredential('D')
		listener, socketPath := newAPITestListener(t, bearer)
		homeValue, _ := apiTestHomes.Load(listener)
		home := homeValue.(*install.OperationalHome)
		store, err := home.OpenStore(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		root := filepath.Join(t.TempDir(), "refused-root")
		connection, client := newReceived(t, listener, socketPath, bearer, root)
		closed := make(chan error, 1)
		go func() { closed <- home.Close() }()
		if err := client.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
			t.Fatal(err)
		}
		if count, err := client.Read(make([]byte, 1)); count != 0 || err == nil {
			t.Fatalf("shutdown did not interrupt paused call: count=%d err=%v", count, err)
		}
		entered := false
		_, dispatchErr := connection.Dispatch(func(Call) Reply {
			entered = true
			_, _ = store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "must-not-commit", Root: root}, at)
			return Reply{}
		})
		if err := connection.Close(); err != nil {
			t.Fatal(err)
		}
		if err := <-closed; err != nil {
			t.Fatal(err)
		}
		if !errors.Is(dispatchErr, ErrTransport) {
			t.Errorf("dispatch after shutdown began = %v", dispatchErr)
		}
		if entered {
			t.Error("paused call entered dispatch after shutdown began")
		}
		apiTestHomes.Delete(listener)
		_ = client.Close()
		freshHome, err := install.OpenOperationalHome(context.Background(), filepath.Dir(filepath.Dir(socketPath)))
		if err != nil {
			t.Fatal(err)
		}
		freshStore, err := freshHome.OpenStore(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if _, found, err := freshStore.Project(context.Background(), projectID); err != nil || found {
			t.Fatalf("paused call durable mutation = found %t, err %v", found, err)
		}
		if err := freshHome.Close(); err != nil {
			t.Fatal(err)
		}
	})

	t.Run("entered dispatch finishes before close", func(t *testing.T) {
		bearer := testCredential('E')
		listener, socketPath := newAPITestListener(t, bearer)
		homeValue, _ := apiTestHomes.Load(listener)
		home := homeValue.(*install.OperationalHome)
		store, err := home.OpenStore(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		root := filepath.Join(t.TempDir(), "entered-root")
		connection, client := newReceived(t, listener, socketPath, bearer, root)
		alias := &Connection{
			self: connection, connection: connection.connection, protocol: connection.protocol,
			domain: connection.domain, kind: connection.kind, call: connection.call, state: connection.state,
		}
		aliasEntered := false
		if _, err := alias.Dispatch(func(Call) Reply { aliasEntered = true; return Reply{} }); !errors.Is(err, ErrProtocol) || aliasEntered {
			t.Fatalf("copied API connection dispatch = %v, entered=%t", err, aliasEntered)
		}
		if err := alias.Close(); !errors.Is(err, ErrProtocol) {
			t.Fatalf("copied API connection close = %v", err)
		}
		entered := make(chan struct{})
		release := make(chan struct{})
		dispatched := make(chan error, 1)
		go func() {
			var effectErr error
			_, err := connection.Dispatch(func(Call) Reply {
				close(entered)
				<-release
				_, effectErr = store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "entered", Root: root}, at)
				reply, _ := NewErrorReply(RemoteInternal)
				return reply
			})
			dispatched <- errors.Join(err, effectErr)
		}()
		<-entered
		closed := make(chan error, 1)
		go func() { closed <- home.Close() }()
		if err := client.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
			t.Fatal(err)
		}
		if count, err := client.Read(make([]byte, 1)); count != 0 || err == nil {
			t.Fatalf("shutdown did not interrupt entered call transport: count=%d err=%v", count, err)
		}
		select {
		case err := <-closed:
			t.Fatalf("home close passed an entered dispatch: %v", err)
		case <-time.After(30 * time.Millisecond):
		}
		close(release)
		if err := <-dispatched; err != nil {
			t.Fatal(err)
		}
		if err := connection.Close(); err != nil {
			t.Fatal(err)
		}
		if err := <-closed; err != nil {
			t.Fatal(err)
		}
		apiTestHomes.Delete(listener)
		_ = client.Close()
		freshHome, err := install.OpenOperationalHome(context.Background(), filepath.Dir(filepath.Dir(socketPath)))
		if err != nil {
			t.Fatal(err)
		}
		freshStore, err := freshHome.OpenStore(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if _, found, err := freshStore.Project(context.Background(), projectID); err != nil || !found {
			t.Fatalf("entered dispatch durable mutation = found %t, err %v", found, err)
		}
		if err := freshHome.Close(); err != nil {
			t.Fatal(err)
		}
	})
}

func TestReplyConstructorsAndConnectionOrderAreClosed(t *testing.T) {
	if _, err := NewMutationReply(MutationResult{Head: 0, Revision: 1}); err != nil {
		t.Fatalf("zero canonical head rejected: %v", err)
	}
	if _, err := NewMutationReply(MutationResult{Head: 1}); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("zero revision = %v", err)
	}
	if _, err := NewAttemptTaskReply(AttemptTask{Task: strings.Repeat("x", 131073)}); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("oversized attempt task = %v", err)
	}
	if _, err := NewErrorReply("private-sentinel"); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("arbitrary error = %v", err)
	}
	connection := &Connection{}
	if _, err := connection.Receive(context.Background()); !errors.Is(err, ErrProtocol) {
		t.Fatalf("nil receive = %v", err)
	}
	if err := connection.Respond(NewHealthReply(HealthStatus{Ready: true})); !errors.Is(err, ErrProtocol) {
		t.Fatalf("response before receive = %v", err)
	}
}

func TestRawRequestHelperProducesOneExactFrame(t *testing.T) {
	bearer := testCredential('H')
	request := rawRequest(operatorDomain, bearer, []byte(`{"method":"health","params":{}}`))
	reader := bytes.NewReader(request)
	payload, err := readTestFrame(reader)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(payload[1:requestPrelude], bearer[:]) {
		t.Fatal("test request bearer differs")
	}
	if _, err := reader.Read(make([]byte, 1)); !errors.Is(err, io.EOF) {
		t.Fatalf("test request has trailing bytes: %v", err)
	}
}
