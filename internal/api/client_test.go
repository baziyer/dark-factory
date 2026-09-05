//go:build darwin || linux

package api

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

const (
	wireOperatorDomain  byte = 1
	wireAttemptDomain   byte = 2
	wireCredentialBytes      = 32
	wireRequestPrelude       = 1 + wireCredentialBytes
	wireMaxFrameBytes        = 1 << 20
)

type wireFixture struct {
	directory string
	socket    string
	token     string
	listener  *net.UnixListener
	request   chan []byte
	done      chan error
	once      sync.Once
}

func newWireFixture(t testing.TB, bearer credential, response func(net.Conn, []byte) error) *wireFixture {
	t.Helper()
	directory := privateTestDirectory(t)
	token := filepath.Join(directory, "token")
	writeTestToken(t, token, bearer)
	socket := filepath.Join(directory, "api.sock")
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: socket, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(socket, 0o600); err != nil {
		listener.Close()
		t.Fatal(err)
	}
	fixture := &wireFixture{
		directory: directory, socket: socket, token: token, listener: listener,
		request: make(chan []byte, 1), done: make(chan error, 1),
	}
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			fixture.done <- err
			return
		}
		defer connection.Close()
		request, err := readTestFrame(connection)
		if err != nil {
			fixture.done <- err
			return
		}
		if !wireOutcomeRequest(request) {
			var extra [1]byte
			if count, readErr := connection.Read(extra[:]); count != 0 || !errors.Is(readErr, io.EOF) {
				fixture.done <- fmt.Errorf("request stream did not end after one frame: count=%d error=%v", count, readErr)
				return
			}
		}
		fixture.request <- request
		fixture.done <- response(connection, request)
	}()
	t.Cleanup(func() { fixture.close() })
	return fixture
}

func (fixture *wireFixture) close() {
	fixture.once.Do(func() { _ = fixture.listener.Close() })
}

func (fixture *wireFixture) wait(t testing.TB) {
	t.Helper()
	select {
	case err := <-fixture.done:
		fixture.close()
		if err != nil {
			t.Fatalf("wire fixture: %v", err)
		}
	case <-time.After(2 * time.Second):
		fixture.close()
		t.Fatal("wire fixture did not finish")
	}
}

func privateTestDirectory(t testing.TB) string {
	t.Helper()
	directory, err := os.MkdirTemp("/private/tmp", "dark-factory-api-test-")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(directory, 0o700); err != nil {
		os.RemoveAll(directory)
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(directory) })
	return directory
}

func testCredential(character byte) credential {
	var result credential
	for index := range result {
		result[index] = character
	}
	return result
}

func writeTestToken(t testing.TB, path string, bearer credential) {
	t.Helper()
	if err := os.WriteFile(path, bearer[:], 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
}

func readTestFrame(reader io.Reader) ([]byte, error) {
	var header [4]byte
	if _, err := io.ReadFull(reader, header[:]); err != nil {
		return nil, err
	}
	size := binary.BigEndian.Uint32(header[:])
	payload := make([]byte, size)
	if _, err := io.ReadFull(reader, payload); err != nil {
		return nil, err
	}
	return payload, nil
}

func writeTestResponse(connection net.Conn, domain byte, body string) error {
	payload := append([]byte{domain}, []byte(body)...)
	return writeTestPayload(connection, payload, nil)
}

func writeTestPayload(connection net.Conn, payload, trailing []byte) error {
	encoded := make([]byte, 4+len(payload)+len(trailing))
	binary.BigEndian.PutUint32(encoded[:4], uint32(len(payload)))
	copy(encoded[4:], payload)
	copy(encoded[4+len(payload):], trailing)
	for len(encoded) > 0 {
		written, err := connection.Write(encoded)
		if err != nil {
			return err
		}
		encoded = encoded[written:]
	}
	return nil
}

func successResponse(data string) string { return `{"ok":true,"data":` + data + `}` }

func mutationResponse() string { return successResponse(`{"head":9,"revision":4}`) }

func wireOutcomeRequest(request []byte) bool {
	if len(request) < wireRequestPrelude {
		return false
	}
	encoded := request[wireRequestPrelude:]
	return bytes.Contains(encoded, []byte(`"method":"succeed"`)) ||
		bytes.Contains(encoded, []byte(`"method":"block"`)) ||
		bytes.Contains(encoded, []byte(`"method":"fail"`))
}

func writeTestOutcomeResponse(connection net.Conn, domain byte, body string) error {
	if err := writeTestResponse(connection, domain, body); err != nil {
		return err
	}
	receipt := bytes.Repeat([]byte{'R'}, outcomeReceiptBytes)
	if err := writeTestPayload(connection, receipt, nil); err != nil {
		return err
	}
	unix, ok := connection.(*net.UnixConn)
	if !ok || unix.CloseWrite() != nil {
		return ErrTransport
	}
	got, err := readTestFrame(connection)
	if err != nil {
		return err
	}
	if !bytes.Equal(got, receipt) {
		return fmt.Errorf("outcome receipt = %x, want %x", got, receipt)
	}
	var extra [1]byte
	if count, readErr := connection.Read(extra[:]); count != 0 || !errors.Is(readErr, io.EOF) {
		return fmt.Errorf("outcome receipt stream did not end: count=%d error=%v", count, readErr)
	}
	return nil
}

func writeTestUnacknowledgedOutcomeResponse(connection net.Conn, domain byte, body string) error {
	if err := writeTestResponse(connection, domain, body); err != nil {
		return err
	}
	receipt := bytes.Repeat([]byte{'R'}, outcomeReceiptBytes)
	if err := writeTestPayload(connection, receipt, nil); err != nil {
		return err
	}
	unix, ok := connection.(*net.UnixConn)
	if !ok || unix.CloseWrite() != nil {
		return ErrTransport
	}
	var extra [1]byte
	if count, readErr := connection.Read(extra[:]); count != 0 || !errors.Is(readErr, io.EOF) {
		return fmt.Errorf("invalid outcome response was acknowledged: count=%d error=%v", count, readErr)
	}
	return nil
}

func TestMutationResultUsesCanonicalHeadAndAllowsZero(t *testing.T) {
	bearer := testCredential('H')
	fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
		return writeTestResponse(connection, wireOperatorDomain, successResponse(`{"head":0,"revision":1}`))
	})
	client, err := NewOperatorClient(fixture.socket, fixture.token)
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.CreateProject(context.Background(), CreateProjectInput{ID: id('1'), Name: "project", Root: "/private/project"})
	if err != nil || result.Head != 0 || result.Revision != 1 {
		t.Fatalf("zero-head mutation result = %+v, %v", result, err)
	}
	<-fixture.request
	fixture.wait(t)

	// The obsolete `sequence` member reaches no field, so it can never be read
	// as a head: the decoded result is the canonical zero head.
	var oldShape MutationResult
	if err := decodeResponse([]byte(successResponse(`{"sequence":1,"revision":1}`)), &oldShape); err != nil {
		t.Fatalf("obsolete sequence response = %v", err)
	}
	if oldShape.Head != 0 || oldShape.Revision != 1 {
		t.Fatalf("obsolete sequence member reached the result: %+v", oldShape)
	}
}

func requestJSON(t testing.TB, payload []byte, domain byte, bearer credential) string {
	t.Helper()
	if len(payload) < wireRequestPrelude {
		t.Fatalf("request payload length = %d", len(payload))
	}
	if payload[0] != domain {
		t.Fatalf("request prelude domain = %d", payload[0])
	}
	if !bytes.Equal(payload[1:wireRequestPrelude], bearer[:]) {
		t.Fatal("request bearer differs")
	}
	encoded := string(payload[wireRequestPrelude:])
	if strings.Contains(encoded, string(bearer[:])) {
		t.Fatal("bearer entered request JSON")
	}
	return encoded
}

func id(character byte) string { return strings.Repeat(string(character), 32) }

func TestOperatorClientMethodsUseExactPrivateWire(t *testing.T) {
	bearer := testCredential('O')
	snapshotJSON := `{"head":8,"factory":{"dispatch_enabled":true,"capacity":4,"active_runs":1,"revision":3},"projects":[{"id":"` + id('1') + `","name":"project","revision":1}],"agents":[{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"codex","paused":false,"revision":2}],"tasks":[{"id":"` + id('3') + `","project_id":"` + id('1') + `","assigned_agent_id":"` + id('2') + `","title":"task","status":"queued","priority":7,"revision":3}]}`
	tests := []struct {
		name     string
		response string
		request  string
		invoke   func(*OperatorClient) error
	}{
		{name: "health", response: successResponse(`{"ready":true}`), request: `{"method":"health","params":{}}`, invoke: func(client *OperatorClient) error {
			status, err := client.Health(context.Background())
			if err == nil && !status.Ready {
				return errors.New("health was not ready")
			}
			return err
		}},
		{name: "snapshot", response: successResponse(snapshotJSON), request: `{"method":"snapshot","params":{}}`, invoke: func(client *OperatorClient) error {
			snapshot, err := client.Snapshot(context.Background())
			if err == nil && (snapshot.Head != 8 || len(snapshot.Projects) != 1 || len(snapshot.Agents) != 1 || snapshot.Agents[0].Provider != "codex" || len(snapshot.Tasks) != 1) {
				return errors.New("snapshot differs")
			}
			return err
		}},
		{name: "create project", response: mutationResponse(), request: `{"method":"create_project","params":{"id":"` + id('1') + `","name":"project","root":"/private/project"}}`, invoke: func(client *OperatorClient) error {
			_, err := client.CreateProject(context.Background(), CreateProjectInput{ID: id('1'), Name: "project", Root: "/private/project"})
			return err
		}},
		{name: "create agent", response: mutationResponse(), request: `{"method":"create_agent","params":{"id":"` + id('2') + `","project_id":"` + id('1') + `","name":"agent","role":"worker","provider":"codex","model":"gpt-5.6-luna","reasoning_effort":"medium","tool_budget_limit":20}}`, invoke: func(client *OperatorClient) error {
			_, err := client.CreateAgent(context.Background(), CreateAgentInput{ID: id('2'), ProjectID: id('1'), Name: "agent", Role: "worker", Provider: "codex", Model: "gpt-5.6-luna", ReasoningEffort: "medium", ToolBudgetLimit: 20})
			return err
		}},
		{name: "enqueue task", response: mutationResponse(), request: `{"method":"enqueue_task","params":{"id":"` + id('3') + `","project_id":"` + id('1') + `","assigned_agent_id":"` + id('2') + `","incarnation_id":"` + id('4') + `","title":"task","body":"private body","priority":7}}`, invoke: func(client *OperatorClient) error {
			_, err := client.EnqueueTask(context.Background(), EnqueueTaskInput{ID: id('3'), ProjectID: id('1'), AssignedAgentID: id('2'), IncarnationID: id('4'), Title: "task", Body: "private body", Priority: 7})
			return err
		}},
		{name: "set dispatch", response: mutationResponse(), request: `{"method":"set_dispatch","params":{"expected_revision":3,"enabled":true}}`, invoke: func(client *OperatorClient) error {
			_, err := client.SetDispatch(context.Background(), 3, true)
			return err
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
				return writeTestResponse(connection, wireOperatorDomain, test.response)
			})
			client, err := NewOperatorClient(fixture.socket, fixture.token)
			if err != nil {
				t.Fatal(err)
			}
			if err := test.invoke(client); err != nil {
				t.Fatal(err)
			}
			payload := <-fixture.request
			if got := requestJSON(t, payload, wireOperatorDomain, bearer); got != test.request {
				t.Fatalf("request JSON = %s\nwant %s", got, test.request)
			}
			fixture.wait(t)
		})
	}
}

func TestAttemptClientHasExactScopedOutcomesAndNoOperatorFallback(t *testing.T) {
	attemptBearer := testCredential('A')
	operatorBearer := testCredential('O')
	tests := []struct {
		name    string
		request string
		invoke  func(*AttemptClient) error
	}{
		{name: "succeed", request: `{"method":"succeed","params":{"result":"result"}}`, invoke: func(client *AttemptClient) error {
			_, err := client.Succeed(context.Background(), "result")
			return err
		}},
		{name: "block", request: `{"method":"block","params":{"detail":"blocked"}}`, invoke: func(client *AttemptClient) error {
			_, err := client.Block(context.Background(), "blocked")
			return err
		}},
		{name: "fail empty detail", request: `{"method":"fail","params":{"detail":""}}`, invoke: func(client *AttemptClient) error {
			_, err := client.Fail(context.Background(), "")
			return err
		}},
		{name: "fail maximum detail", request: `{"method":"fail","params":{"detail":"` + strings.Repeat("x", 4096) + `"}}`, invoke: func(client *AttemptClient) error {
			_, err := client.Fail(context.Background(), strings.Repeat("x", 4096))
			return err
		}},
		{name: "request human", request: `{"method":"request_human","params":{"idempotency_key":"0123456789abcdef0123456789abcdef","question":"private-question-sentinel"}}`, invoke: func(client *AttemptClient) error {
			_, err := client.RequestHuman(context.Background(), HumanQuestionInput{IdempotencyKey: "0123456789abcdef0123456789abcdef", Question: "private-question-sentinel"})
			return err
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newWireFixture(t, attemptBearer, func(connection net.Conn, request []byte) error {
				if wireOutcomeRequest(request) {
					return writeTestOutcomeResponse(connection, wireAttemptDomain, mutationResponse())
				}
				return writeTestResponse(connection, wireAttemptDomain, mutationResponse())
			})
			operatorPath := filepath.Join(fixture.directory, "operator.token")
			writeTestToken(t, operatorPath, operatorBearer)
			t.Setenv(attemptTokenFileEnv, fixture.token)
			t.Setenv("DARK_FACTORY_OPERATOR_TOKEN_FILE", operatorPath)
			client, err := NewAttemptClientFromEnvironment(fixture.socket)
			if err != nil {
				t.Fatal(err)
			}
			if err := test.invoke(client); err != nil {
				t.Fatal(err)
			}
			encoded := requestJSON(t, <-fixture.request, wireAttemptDomain, attemptBearer)
			if encoded != test.request {
				t.Fatalf("attempt request = %s, want %s", encoded, test.request)
			}
			if strings.Contains(encoded, `"id"`) || strings.Contains(encoded, `"code"`) || strings.Contains(encoded, string(operatorBearer[:])) {
				t.Fatalf("attempt request widened scope: %s", encoded)
			}
			fixture.wait(t)
		})
	}

	t.Run("missing attempt environment", func(t *testing.T) {
		directory := privateTestDirectory(t)
		listener, socket := testListener(t, directory)
		defer listener.Close()
		operatorPath := filepath.Join(directory, "operator.token")
		writeTestToken(t, operatorPath, operatorBearer)
		t.Setenv("DARK_FACTORY_OPERATOR_TOKEN_FILE", operatorPath)
		if client, err := NewAttemptClientFromEnvironment(socket); client != nil || !errors.Is(err, ErrInvalidClient) {
			t.Fatalf("attempt fallback = %+v, %v", client, err)
		}
	})
}

func TestAttemptClientReadsExactTask(t *testing.T) {
	bearer := testCredential('T')
	fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
		return writeTestResponse(connection, wireAttemptDomain, successResponse(`{"task":"private-task-sentinel"}`))
	})
	t.Setenv(attemptTokenFileEnv, fixture.token)
	client, err := NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	task, err := client.Task(context.Background())
	if err != nil || task.Task != "private-task-sentinel" {
		t.Fatalf("task = %+v, %v", task, err)
	}
	if formatted := fmt.Sprintf("%v %+v %#v", task, task, task); strings.Contains(formatted, task.Task) {
		t.Fatalf("attempt task formatting exposed private text: %s", formatted)
	}
	if encoded := requestJSON(t, <-fixture.request, wireAttemptDomain, bearer); encoded != `{"method":"task","params":{}}` {
		t.Fatalf("attempt task request = %s", encoded)
	}
	fixture.wait(t)
}

func TestAttemptClientRejectsInvalidTaskResponse(t *testing.T) {
	bearer := testCredential('V')
	fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
		return writeTestResponse(connection, wireAttemptDomain, successResponse(`{"task":"bad\u0000task"}`))
	})
	t.Setenv(attemptTokenFileEnv, fixture.token)
	client, err := NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.Task(context.Background()); !errors.Is(err, ErrProtocol) {
		t.Fatalf("invalid task response = %v", err)
	}
	<-fixture.request
	fixture.wait(t)
}

func TestAttemptOutcomeAcknowledgesErrorResponse(t *testing.T) {
	bearer := testCredential('E')
	fixture := newWireFixture(t, bearer, func(connection net.Conn, request []byte) error {
		if !wireOutcomeRequest(request) {
			return fmt.Errorf("expected outcome request")
		}
		return writeTestOutcomeResponse(connection, wireAttemptDomain, `{"ok":false,"error":"unavailable"}`)
	})
	t.Setenv(attemptTokenFileEnv, fixture.token)
	client, err := NewAttemptClientFromEnvironment(fixture.socket)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.Succeed(context.Background(), "committed"); err == nil {
		t.Fatal("error response succeeded")
	} else {
		var remote *RemoteError
		if !errors.As(err, &remote) || remote.Code() != RemoteUnavailable {
			t.Fatalf("error response = %v", err)
		}
	}
	fixture.wait(t)
}

func TestAttemptOutcomeDoesNotAcknowledgeInvalidResponse(t *testing.T) {
	for _, test := range []struct {
		name string
		body string
	}{
		{name: "malformed envelope", body: `{"ok":true,"error":"internal","data":{"head":9,"revision":4}}`},
		{name: "zero revision", body: `{"ok":true,"data":{"head":9,"revision":0}}`},
	} {
		t.Run(test.name, func(t *testing.T) {
			bearer := testCredential('I')
			fixture := newWireFixture(t, bearer, func(connection net.Conn, request []byte) error {
				if !wireOutcomeRequest(request) {
					return fmt.Errorf("expected outcome request")
				}
				return writeTestUnacknowledgedOutcomeResponse(connection, wireAttemptDomain, test.body)
			})
			t.Setenv(attemptTokenFileEnv, fixture.token)
			client, err := NewAttemptClientFromEnvironment(fixture.socket)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := client.Succeed(context.Background(), "invalid-response"); !errors.Is(err, ErrProtocol) {
				t.Fatalf("invalid outcome response = %v", err)
			}
			fixture.wait(t)
		})
	}
}

func TestResponseFramingIsExactBoundedAndStrict(t *testing.T) {
	bearer := testCredential('R')
	valid := successResponse(`{"ready":true}`)
	tests := []struct {
		name   string
		write  func(net.Conn) error
		want   error
		remote RemoteErrorCode
	}{
		{name: "wrong domain", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireAttemptDomain, valid)
		}, want: ErrProtocol},
		{name: "zero frame", write: func(connection net.Conn) error { return writeTestBytes(connection, []byte{0, 0, 0, 0}) }, want: ErrProtocol},
		{name: "oversized frame", write: func(connection net.Conn) error {
			var header [4]byte
			binary.BigEndian.PutUint32(header[:], wireMaxFrameBytes+1)
			return writeTestBytes(connection, header[:])
		}, want: ErrProtocol},
		{name: "truncated header", write: func(connection net.Conn) error { return writeTestBytes(connection, []byte{0, 0}) }, want: ErrTransport},
		{name: "truncated payload", write: func(connection net.Conn) error {
			return writeTestBytes(connection, []byte{0, 0, 0, 10, wireOperatorDomain, '{'})
		}, want: ErrTransport},
		{name: "extra JSON", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, valid+`{}`)
		}, want: ErrProtocol},
		{name: "duplicate envelope ok", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":false,"ok":true,"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "duplicate envelope data", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":false},"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "duplicate envelope error", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":false,"error":"forbidden","error":"unauthorized"}`)
		}, want: ErrProtocol},
		{name: "lowercase then uppercase ok alias", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":false,"OK":true,"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "uppercase then lowercase ok alias", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"OK":false,"ok":true,"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "nested uppercase ready alias", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":false,"READY":true}}`)
		}, want: ErrProtocol},
		{name: "unicode fold ok alias", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"o\u212a":true,"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "escaped uppercase ok alias", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"\u004fK":true,"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "standalone noncanonical ok", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"Ok":true,"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "punctuated member name", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":true},"bad-key":1}`)
		}, want: ErrProtocol},
		{name: "whitespace member name", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":true},"bad key":1}`)
		}, want: ErrProtocol},
		{name: "control member name", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":true},"\u0001":1}`)
		}, want: ErrProtocol},
		{name: "escaped canonical lowercase names", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"\u006f\u006b":true,"data":{"\u0072eady":true}}`)
		}},
		{name: "invalid UTF-8 error", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, "{\"ok\":false,\"error\":\"\xff\"}")
		}, want: ErrProtocol},
		// Tolerance is additive: a member this build does not know is ignored
		// in the envelope and in the data, and the answer is still served.
		{name: "unknown envelope member", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":true},"future":"added"}`)
		}},
		{name: "unknown data member", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":true,"data":{"ready":true,"future":"added"}}`)
		}},
		{name: "missing ok", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"data":{"ready":true}}`)
		}, want: ErrProtocol},
		{name: "trailing wire byte", write: func(connection net.Conn) error {
			payload := append([]byte{wireOperatorDomain}, []byte(valid)...)
			return writeTestPayload(connection, payload, []byte{'x'})
		}, want: ErrProtocol},
		{name: "unknown fixed error", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":false,"error":"private-sentinel"}`)
		}, want: ErrProtocol},
		{name: "fixed remote error", write: func(connection net.Conn) error {
			return writeTestResponse(connection, wireOperatorDomain, `{"ok":false,"error":"unauthorized"}`)
		}, remote: RemoteUnauthorized},
		{name: "maximum exact frame", write: func(connection net.Conn) error {
			payload := append([]byte{wireOperatorDomain}, []byte(valid)...)
			payload = append(payload, bytes.Repeat([]byte{' '}, wireMaxFrameBytes-len(payload))...)
			return writeTestPayload(connection, payload, nil)
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error { return test.write(connection) })
			client, err := NewOperatorClient(fixture.socket, fixture.token)
			if err != nil {
				t.Fatal(err)
			}
			_, err = client.Health(context.Background())
			if test.remote != "" {
				var remote *RemoteError
				if !errors.As(err, &remote) || remote.Code() != test.remote {
					t.Fatalf("remote error = %v", err)
				}
			} else if test.want != nil {
				if !errors.Is(err, test.want) {
					t.Fatalf("client error = %v, want %v", err, test.want)
				}
			} else if err != nil {
				t.Fatalf("client error = %v", err)
			}
			<-fixture.request
			fixture.wait(t)
		})
	}
}

func TestCanonicalJSONNameGrammar(t *testing.T) {
	for _, name := range []string{"ok", "active_runs", "x1", "a_b_2"} {
		if !canonicalJSONName(name) {
			t.Errorf("canonical name %q rejected", name)
		}
	}
	for _, name := range []string{"", "OK", "Ok", "oK", "_ok", "1ok", "bad-key", "bad key", "bad.name", "bad\x01name"} {
		if canonicalJSONName(name) {
			t.Errorf("noncanonical name %q accepted", name)
		}
	}
}

func TestJSONSurrogateEscapesAreLexicallyExact(t *testing.T) {
	for _, encoded := range []string{
		`{"value":"\ud83d\ude00"}`,
		`{"value":"�"}`,
		`{"value":"\\ud800"}`,
		`{"value":"\ufffd"}`,
	} {
		if err := validateJSONNames([]byte(encoded)); err != nil {
			t.Errorf("valid JSON %s rejected: %v", encoded, err)
		}
	}
	for _, encoded := range []string{
		`{"value":"\ud800"}`,
		`{"value":"\udc00"}`,
		`{"value":"\udc00\ud800"}`,
		`{"value":"\ud800\u0041"}`,
		`{"value":"\ud800\ud801"}`,
	} {
		if err := validateJSONNames([]byte(encoded)); !errors.Is(err, ErrProtocol) {
			t.Errorf("invalid surrogate JSON %s = %v", encoded, err)
		}
	}
}

func TestSnapshotJSONRejectsDuplicateNestedNamesAndInvalidUTF8(t *testing.T) {
	bearer := testCredential('J')
	prefix := `{"head":1,"factory":{"dispatch_enabled":true,"capacity":1,"active_runs":0,"revision":1},"projects":[{"id":"` + id('1') + `",`
	suffix := `,"revision":1}],"agents":[],"tasks":[]}`
	tests := []struct {
		name string
		body string
	}{
		{name: "duplicate entity name", body: prefix + `"name":"first","name":"second"` + suffix},
		{name: "entity case alias", body: prefix + `"name":"first","Name":"second"` + suffix},
		{name: "duplicate nested snapshot field", body: `{"head":1,"factory":{"dispatch_enabled":true,"capacity":1,"capacity":2,"active_runs":0,"revision":1},"projects":[],"agents":[],"tasks":[]}`},
		{name: "nested snapshot case alias", body: `{"head":1,"factory":{"dispatch_enabled":true,"capacity":1,"Capacity":2,"active_runs":0,"revision":1},"projects":[],"agents":[],"tasks":[]}`},
		{name: "invalid UTF-8 entity name", body: prefix + "\"name\":\"\xff\"" + suffix},
		{name: "lone high surrogate", body: prefix + `"name":"\ud800"` + suffix},
		{name: "lone low surrogate", body: prefix + `"name":"\udc00"` + suffix},
		{name: "reversed surrogate pair", body: prefix + `"name":"\udc00\ud800"` + suffix},
		{name: "mismatched surrogate pair", body: prefix + `"name":"\ud800\u0041"` + suffix},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
				return writeTestResponse(connection, wireOperatorDomain, successResponse(test.body))
			})
			client, err := NewOperatorClient(fixture.socket, fixture.token)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := client.Snapshot(context.Background()); !errors.Is(err, ErrProtocol) {
				t.Fatalf("snapshot error = %v, want %v", err, ErrProtocol)
			}
			<-fixture.request
			fixture.wait(t)
		})
	}
}

func TestSnapshotJSONAcceptsPairedSurrogateAndLiteralReplacement(t *testing.T) {
	bearer := testCredential('U')
	tests := []struct {
		name string
		wire string
		want string
	}{
		{name: "paired surrogate", wire: `\ud83d\ude00`, want: "😀"},
		{name: "literal replacement", wire: "�", want: "�"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			body := `{"head":1,"factory":{"dispatch_enabled":true,"capacity":1,"active_runs":0,"revision":1},"projects":[{"id":"` + id('1') + `","name":"` + test.wire + `","revision":1}],"agents":[],"tasks":[]}`
			fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
				return writeTestResponse(connection, wireOperatorDomain, successResponse(body))
			})
			client, err := NewOperatorClient(fixture.socket, fixture.token)
			if err != nil {
				t.Fatal(err)
			}
			snapshot, err := client.Snapshot(context.Background())
			if err != nil || len(snapshot.Projects) != 1 || snapshot.Projects[0].Name != test.want {
				t.Fatalf("snapshot project = %+v, %v", snapshot.Projects, err)
			}
			<-fixture.request
			fixture.wait(t)
		})
	}
}

func writeTestBytes(connection net.Conn, encoded []byte) error {
	for len(encoded) > 0 {
		written, err := connection.Write(encoded)
		if err != nil {
			return err
		}
		encoded = encoded[written:]
	}
	return nil
}

func TestDeadlineClosesOneShotConnectionPromptly(t *testing.T) {
	bearer := testCredential('D')
	release := make(chan struct{})
	fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
		<-release
		return writeTestResponse(connection, wireOperatorDomain, successResponse(`{"ready":true}`))
	})
	client, err := NewOperatorClient(fixture.socket, fixture.token)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	started := time.Now()
	_, err = client.Health(ctx)
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("deadline error = %v", err)
	}
	if elapsed := time.Since(started); elapsed > 500*time.Millisecond {
		t.Fatalf("deadline took %v", elapsed)
	}
	close(release)
	<-fixture.request
	select {
	case <-fixture.done:
	case <-time.After(time.Second):
		t.Fatal("deadline fixture retained connection")
	}
}

func TestClientFormattingAndErrorsNeverExposeBearerOrPaths(t *testing.T) {
	bearer := testCredential('S')
	fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
		return writeTestResponse(connection, wireOperatorDomain, `{"ok":false,"error":"unauthorized"}`)
	})
	client, err := NewOperatorClient(fixture.socket, fixture.token)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(client)
	if err != nil {
		t.Fatal(err)
	}
	formatted := fmt.Sprintf("%v %+v %#v %v %+v %#v %s", client, client, client, *client, *client, *client, encoded)
	for _, private := range []string{string(bearer[:]), fixture.socket, fixture.token} {
		if strings.Contains(formatted, private) {
			t.Fatalf("client formatting exposed private value: %s", formatted)
		}
	}
	_, err = client.Health(context.Background())
	if err == nil {
		t.Fatal("remote refusal succeeded")
	}
	for _, private := range []string{string(bearer[:]), fixture.socket, fixture.token} {
		if strings.Contains(err.Error(), private) {
			t.Fatalf("error exposed private value: %s", err)
		}
	}
	<-fixture.request
	fixture.wait(t)
}

func TestTokenAndSocketPathsFailClosed(t *testing.T) {
	bearer := testCredential('P')
	t.Run("token classes", func(t *testing.T) {
		tests := []struct {
			name  string
			build func(testing.TB, string) string
		}{
			{name: "symlink", build: func(t testing.TB, directory string) string {
				target := filepath.Join(directory, "target")
				writeTestToken(t, target, bearer)
				path := filepath.Join(directory, "token")
				if err := os.Symlink(target, path); err != nil {
					t.Fatal(err)
				}
				return path
			}},
			{name: "directory", build: func(t testing.TB, directory string) string {
				path := filepath.Join(directory, "token")
				if err := os.Mkdir(path, 0o600); err != nil {
					t.Fatal(err)
				}
				return path
			}},
			{name: "fifo", build: func(t testing.TB, directory string) string {
				path := filepath.Join(directory, "token")
				if err := syscall.Mkfifo(path, 0o600); err != nil {
					t.Fatal(err)
				}
				return path
			}},
			{name: "broad mode", build: func(t testing.TB, directory string) string {
				path := filepath.Join(directory, "token")
				writeTestToken(t, path, bearer)
				if err := os.Chmod(path, 0o640); err != nil {
					t.Fatal(err)
				}
				return path
			}},
			{name: "short", build: func(t testing.TB, directory string) string {
				path := filepath.Join(directory, "token")
				if err := os.WriteFile(path, bearer[:31], 0o600); err != nil {
					t.Fatal(err)
				}
				return path
			}},
			{name: "long", build: func(t testing.TB, directory string) string {
				path := filepath.Join(directory, "token")
				if err := os.WriteFile(path, append(bearer[:], 'x'), 0o600); err != nil {
					t.Fatal(err)
				}
				return path
			}},
			{name: "hard link", build: func(t testing.TB, directory string) string {
				target := filepath.Join(directory, "target")
				writeTestToken(t, target, bearer)
				path := filepath.Join(directory, "token")
				if err := os.Link(target, path); err != nil {
					t.Fatal(err)
				}
				return path
			}},
		}
		for _, test := range tests {
			t.Run(test.name, func(t *testing.T) {
				directory := privateTestDirectory(t)
				listener, socket := testListener(t, directory)
				defer listener.Close()
				token := test.build(t, directory)
				if client, err := NewOperatorClient(socket, token); client != nil || !errors.Is(err, ErrInvalidClient) {
					t.Fatalf("unsafe token client = %+v, %v", client, err)
				}
			})
		}
	})

	t.Run("noncanonical and symlinked parent", func(t *testing.T) {
		directory := privateTestDirectory(t)
		actual := filepath.Join(directory, "actual")
		if err := os.Mkdir(actual, 0o700); err != nil {
			t.Fatal(err)
		}
		nested := filepath.Join(actual, "nested")
		if err := os.Mkdir(nested, 0o700); err != nil {
			t.Fatal(err)
		}
		listener, socket := testListener(t, nested)
		defer listener.Close()
		token := filepath.Join(nested, "token")
		writeTestToken(t, token, bearer)
		alias := filepath.Join(directory, "alias")
		if err := os.Symlink(actual, alias); err != nil {
			t.Fatal(err)
		}
		for _, pair := range [][2]string{{socket, nested + "/./token"}, {filepath.Join(alias, "nested", "api.sock"), filepath.Join(alias, "nested", "token")}} {
			if client, err := NewOperatorClient(pair[0], pair[1]); client != nil || !errors.Is(err, ErrInvalidClient) {
				t.Fatalf("unsafe path client = %+v, %v", client, err)
			}
		}
	})

	t.Run("socket classes", func(t *testing.T) {
		directory := privateTestDirectory(t)
		token := filepath.Join(directory, "token")
		writeTestToken(t, token, bearer)
		realDirectory := privateTestDirectory(t)
		listener, realSocket := testListener(t, realDirectory)
		defer listener.Close()
		symlink := filepath.Join(directory, "symlink.sock")
		if err := os.Symlink(realSocket, symlink); err != nil {
			t.Fatal(err)
		}
		regular := filepath.Join(directory, "regular.sock")
		if err := os.WriteFile(regular, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		for _, path := range []string{symlink, regular} {
			if client, err := NewOperatorClient(path, token); client != nil || !errors.Is(err, ErrInvalidClient) {
				t.Fatalf("unsafe socket client = %+v, %v", client, err)
			}
		}
		if err := os.Chmod(realSocket, 0o660); err != nil {
			t.Fatal(err)
		}
		if client, err := NewOperatorClient(realSocket, token); client != nil || !errors.Is(err, ErrInvalidClient) {
			t.Fatalf("broad socket client = %+v, %v", client, err)
		}
	})

	t.Run("broad parent mode", func(t *testing.T) {
		directory := privateTestDirectory(t)
		listener, socket := testListener(t, directory)
		defer listener.Close()
		token := filepath.Join(directory, "token")
		writeTestToken(t, token, bearer)
		if err := os.Chmod(directory, 0o750); err != nil {
			t.Fatal(err)
		}
		if client, err := NewOperatorClient(socket, token); client != nil || !errors.Is(err, ErrInvalidClient) {
			t.Fatalf("broad parent client = %+v, %v", client, err)
		}
	})

	t.Run("token replacement after construction", func(t *testing.T) {
		directory := privateTestDirectory(t)
		listener, socket := testListener(t, directory)
		defer listener.Close()
		token := filepath.Join(directory, "token")
		writeTestToken(t, token, bearer)
		client, err := NewOperatorClient(socket, token)
		if err != nil {
			t.Fatal(err)
		}
		replacement := filepath.Join(directory, "replacement")
		writeTestToken(t, replacement, bearer)
		if err := os.Rename(replacement, token); err != nil {
			t.Fatal(err)
		}
		if _, err := client.Health(context.Background()); !errors.Is(err, ErrInvalidClient) {
			t.Fatalf("replacement token = %v", err)
		}
	})

	t.Run("token parent replacement after construction", func(t *testing.T) {
		tokenDirectory := privateTestDirectory(t)
		token := filepath.Join(tokenDirectory, "token")
		writeTestToken(t, token, bearer)
		socketDirectory := privateTestDirectory(t)
		listener, socket := testListener(t, socketDirectory)
		defer listener.Close()
		client, err := NewOperatorClient(socket, token)
		if err != nil {
			t.Fatal(err)
		}
		moved := tokenDirectory + "-moved"
		t.Cleanup(func() { _ = os.RemoveAll(moved) })
		if err := os.Rename(tokenDirectory, moved); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(tokenDirectory, 0o700); err != nil {
			t.Fatal(err)
		}
		writeTestToken(t, token, bearer)
		if _, err := client.Health(context.Background()); !errors.Is(err, ErrInvalidClient) {
			t.Fatalf("replacement token parent = %v", err)
		}
	})
}

func testListener(t testing.TB, directory string) (*net.UnixListener, string) {
	t.Helper()
	socket := filepath.Join(directory, "api.sock")
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: socket, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(socket, 0o600); err != nil {
		listener.Close()
		t.Fatal(err)
	}
	return listener, socket
}

func TestSocketParentSwapAfterDialIsRejected(t *testing.T) {
	bearer := testCredential('W')
	tokenDirectory := privateTestDirectory(t)
	token := filepath.Join(tokenDirectory, "token")
	writeTestToken(t, token, bearer)
	socketDirectory := privateTestDirectory(t)
	movedSocketDirectory := socketDirectory + "-moved"
	t.Cleanup(func() { _ = os.RemoveAll(movedSocketDirectory) })
	listener, socket := testListener(t, socketDirectory)
	defer listener.Close()
	done := make(chan error, 1)
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			done <- err
			return
		}
		defer connection.Close()
		if _, err := readTestFrame(connection); err != nil {
			done <- err
			return
		}
		var extra [1]byte
		_, _ = connection.Read(extra[:])
		if err := os.Rename(socketDirectory, movedSocketDirectory); err != nil {
			done <- err
			return
		}
		if err := os.Mkdir(socketDirectory, 0o700); err != nil {
			done <- err
			return
		}
		done <- writeTestResponse(connection, wireOperatorDomain, successResponse(`{"ready":true}`))
	}()
	client, err := NewOperatorClient(socket, token)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.Health(context.Background()); !errors.Is(err, ErrInvalidClient) {
		t.Fatalf("socket parent swap = %v", err)
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestInputBoundsFailBeforeConnection(t *testing.T) {
	bearer := testCredential('B')
	directory := privateTestDirectory(t)
	listener, socket := testListener(t, directory)
	defer listener.Close()
	token := filepath.Join(directory, "token")
	writeTestToken(t, token, bearer)
	operator, err := NewOperatorClient(socket, token)
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv(attemptTokenFileEnv, token)
	attempt, err := NewAttemptClientFromEnvironment(socket)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := operator.CreateProject(context.Background(), CreateProjectInput{ID: "bad", Name: "name", Root: "/root"}); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("invalid ID = %v", err)
	}
	for name, input := range map[string]CreateAgentInput{
		"missing provider": {ID: id('2'), ProjectID: id('1'), Name: "agent", Role: "worker", ToolBudgetLimit: 1},
		"unknown provider": {ID: id('2'), ProjectID: id('1'), Name: "agent", Role: "worker", Provider: "other", ToolBudgetLimit: 1},
		"shell model":      {ID: id('2'), ProjectID: id('1'), Name: "agent", Role: "worker", Provider: "shell", Model: "private", ToolBudgetLimit: 1},
		"claude ultra":     {ID: id('2'), ProjectID: id('1'), Name: "agent", Role: "worker", Provider: "claude_code", ReasoningEffort: "ultra", ToolBudgetLimit: 1},
		"large model":      {ID: id('2'), ProjectID: id('1'), Name: "agent", Role: "worker", Provider: "codex", Model: strings.Repeat("m", 129), ToolBudgetLimit: 1},
	} {
		t.Run("create agent "+name, func(t *testing.T) {
			if _, err := operator.CreateAgent(context.Background(), input); !errors.Is(err, ErrInvalidInput) {
				t.Fatalf("invalid agent = %v", err)
			}
		})
	}
	if _, err := attempt.Block(context.Background(), strings.Repeat("x", 4097)); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("oversized detail = %v", err)
	}
	if _, err := attempt.Block(context.Background(), ""); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("empty blocked detail = %v", err)
	}
	if _, err := attempt.Fail(context.Background(), strings.Repeat("x", 4097)); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("oversized failure detail = %v", err)
	}
	if _, err := attempt.Succeed(context.Background(), strings.Repeat("x", 131073)); !errors.Is(err, ErrInvalidInput) {
		t.Fatalf("oversized result = %v", err)
	}
	for name, input := range map[string]HumanQuestionInput{
		"zero key":       {IdempotencyKey: strings.Repeat("0", 32), Question: "question"},
		"uppercase key":  {IdempotencyKey: "0123456789ABCDEF0123456789abcdef", Question: "question"},
		"short key":      {IdempotencyKey: "abc", Question: "question"},
		"empty question": {IdempotencyKey: id('1'), Question: ""},
		"large question": {IdempotencyKey: id('1'), Question: strings.Repeat("x", 8193)},
		"invalid utf8":   {IdempotencyKey: id('1'), Question: string([]byte{0xff})},
	} {
		t.Run("request human "+name, func(t *testing.T) {
			if _, err := attempt.RequestHuman(context.Background(), input); !errors.Is(err, ErrInvalidInput) {
				t.Fatalf("invalid human question = %v", err)
			}
		})
	}
	if err := listener.SetDeadline(time.Now().Add(75 * time.Millisecond)); err != nil {
		t.Fatal(err)
	}
	if connection, err := listener.Accept(); err == nil {
		connection.Close()
		t.Fatal("invalid input opened a connection")
	}
}

func TestPublicSnapshotDTOCannotCarryPrivateStoreFields(t *testing.T) {
	snapshot := DashboardSnapshot{
		Head: 1, Factory: FactorySummary{Capacity: 1, Revision: 1},
		Projects: []ProjectSummary{{ID: id('1'), Name: "safe", Revision: 1}},
		Agents:   []AgentSummary{}, Tasks: []TaskSummary{},
	}
	encoded, err := json.Marshal(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	for _, field := range []string{"root", "body", "result", "model", "reasoning_effort", "credential", "source", "path"} {
		if strings.Contains(string(encoded), `"`+field+`"`) {
			t.Fatalf("public DTO contains private field %q: %s", field, encoded)
		}
	}
}

func TestFocusedClientCallsDoNotRetainGoroutinesOrFDs(t *testing.T) {
	baselineGoroutines := runtime.NumGoroutine()
	baselineFDs := countTestFDs(t)
	for range 20 {
		bearer := testCredential('C')
		fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
			return writeTestResponse(connection, wireOperatorDomain, successResponse(`{"ready":true}`))
		})
		client, err := NewOperatorClient(fixture.socket, fixture.token)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := client.Health(context.Background()); err != nil {
			t.Fatal(err)
		}
		<-fixture.request
		fixture.wait(t)
	}
	time.Sleep(20 * time.Millisecond)
	if delta := runtime.NumGoroutine() - baselineGoroutines; delta > 1 {
		t.Fatalf("client calls retained %d goroutines", delta)
	}
	if after := countTestFDs(t); after != baselineFDs {
		t.Fatalf("client calls changed FD census: before=%d after=%d", baselineFDs, after)
	}
}

func countTestFDs(t testing.TB) int {
	t.Helper()
	directory := "/dev/fd"
	if runtime.GOOS == "linux" {
		directory = "/proc/self/fd"
	}
	entries, err := os.ReadDir(directory)
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func TestMetadataPredicatesRejectForeignOwnershipAndWrongLinksOrSize(t *testing.T) {
	directory := privateTestDirectory(t)
	parentInfo, err := os.Lstat(directory)
	if err != nil {
		t.Fatal(err)
	}
	parent := clonedFileInfo{FileInfo: parentInfo, stat: *parentInfo.Sys().(*syscall.Stat_t)}
	parent.stat.Uid = uint32(os.Geteuid()) + 1
	if validPrivateParent(parent) {
		t.Fatal("foreign-owned private parent accepted")
	}

	tokenPath := filepath.Join(directory, "token")
	writeTestToken(t, tokenPath, testCredential('M'))
	tokenInfo, err := os.Lstat(tokenPath)
	if err != nil {
		t.Fatal(err)
	}
	for name, mutate := range map[string]func(*syscall.Stat_t){
		"foreign owner": func(stat *syscall.Stat_t) { stat.Uid = uint32(os.Geteuid()) + 1 },
		"second link":   func(stat *syscall.Stat_t) { stat.Nlink = 2 },
		"wrong size":    func(stat *syscall.Stat_t) { stat.Size = wireCredentialBytes - 1 },
	} {
		t.Run(name, func(t *testing.T) {
			info := clonedFileInfo{FileInfo: tokenInfo, stat: *tokenInfo.Sys().(*syscall.Stat_t)}
			mutate(&info.stat)
			if validTokenInfo(info) {
				t.Fatal("unsafe token metadata accepted")
			}
		})
	}
}

type clonedFileInfo struct {
	os.FileInfo
	stat syscall.Stat_t
}

func (info clonedFileInfo) Sys() any { return &info.stat }
