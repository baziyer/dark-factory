//go:build darwin || linux

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/install"
)

type serverResult struct {
	call api.Call
	err  error
}

func TestVersionRequiresNoHomeOrCredential(t *testing.T) {
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"--version"}, func(string) string { return "private" }, &stdout, &stderr)
	if exit != 0 || stdout.String() != "factoryctl development\n" || stderr.Len() != 0 {
		t.Fatalf("version exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
	}
}

func TestBuildIdentityRequiresNoHomeOrCredential(t *testing.T) {
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"--build-identity"}, func(string) string { return "private" }, &stdout, &stderr)
	if exit != 0 || !strings.Contains(stdout.String(), `"release":false`) || stderr.Len() != 0 {
		t.Fatalf("build identity exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
	}
}

type apiFixture struct {
	directory   string
	socket      string
	attemptPath string
	bearer      [32]byte
	listener    *api.Listener
	home        *install.OperationalHome
}

func newAPIFixture(t testing.TB) *apiFixture {
	t.Helper()
	directory, err := os.MkdirTemp("/private/tmp", "dark-factory-factoryctl-attempt-")
	if err != nil {
		t.Fatal(err)
	}
	cleanup := true
	defer func() {
		if cleanup {
			_ = os.RemoveAll(directory)
		}
	}()
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
	operatorPath := filepath.Join(homePath, "operator.token")
	attemptPath := filepath.Join(directory, "attempt.token")
	writeToken(t, operatorPath, bytes.Repeat([]byte{'O'}, 32))
	attempt := bytes.Repeat([]byte{'A'}, 32)
	writeToken(t, attemptPath, attempt)
	home, err := install.OpenOperationalHome(context.Background(), homePath)
	if err != nil {
		t.Fatal(err)
	}
	authority, err := home.OpenLocalAPI(context.Background())
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	listener, err := api.Listen(authority)
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	socket := install.LocalAPISocketPath(homePath)
	fixture := &apiFixture{directory: directory, socket: socket, attemptPath: attemptPath, listener: listener, home: home}
	copy(fixture.bearer[:], attempt)
	cleanup = false
	return fixture
}

func writeToken(t testing.TB, path string, value []byte) {
	t.Helper()
	if err := os.WriteFile(path, value, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
}

func (fixture *apiFixture) close(t testing.TB) {
	t.Helper()
	if fixture.listener != nil {
		if err := fixture.listener.Close(); err != nil && !errors.Is(err, api.ErrInvalidListener) {
			t.Errorf("close listener: %v", err)
		}
		fixture.listener = nil
	}
	if fixture.home != nil {
		if err := fixture.home.Close(); err != nil {
			t.Errorf("close operational home: %v", err)
		}
		fixture.home = nil
	}
	if _, err := os.Lstat(fixture.socket); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("socket remains after close: %v", err)
	}
	if err := os.RemoveAll(fixture.directory); err != nil {
		t.Errorf("remove fixture: %v", err)
	}
}

func serveOne(listener *api.Listener, reply func(api.Call) api.Reply) <-chan serverResult {
	done := make(chan serverResult, 1)
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			done <- serverResult{err: err}
			return
		}
		defer connection.Close()
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		call, err := connection.Receive(ctx)
		if err == nil {
			var response api.Reply
			response, err = connection.Dispatch(reply)
			if err == nil {
				err = connection.Respond(response)
			}
			if err == nil {
				err = connection.AwaitOutcomeReceipt(ctx)
			}
		}
		done <- serverResult{call: call, err: err}
	}()
	return done
}

func awaitServer(t testing.TB, done <-chan serverResult) serverResult {
	t.Helper()
	select {
	case result := <-done:
		return result
	case <-time.After(2 * time.Second):
		t.Fatal("server did not finish")
		return serverResult{}
	}
}

func TestParseExactAttemptCommands(t *testing.T) {
	tests := []struct {
		name    string
		args    []string
		command attemptCommand
		help    bool
	}{
		{name: "root help", args: []string{"--help"}, help: true},
		{name: "attempt help", args: []string{"attempt", "-h"}, help: true},
		{name: "verb help", args: []string{"attempt", "block", "--help"}, help: true},
		{name: "task", args: []string{"attempt", "task"}, command: attemptCommand{kind: commandAttemptTask}},
		{name: "empty success", args: []string{"attempt", "succeed"}, command: attemptCommand{kind: commandSucceed}},
		{name: "success", args: []string{"attempt", "succeed", "--result", "done"}, command: attemptCommand{kind: commandSucceed, text: "done"}},
		{name: "block", args: []string{"attempt", "block", "--detail", "waiting"}, command: attemptCommand{kind: commandBlock, text: "waiting"}},
		{name: "empty failure", args: []string{"attempt", "fail"}, command: attemptCommand{kind: commandFail}},
		{name: "failure", args: []string{"attempt", "fail", "--detail", "broken"}, command: attemptCommand{kind: commandFail, text: "broken"}},
		{name: "explicit empty success", args: []string{"attempt", "succeed", "--result", ""}, command: attemptCommand{kind: commandSucceed}},
		{name: "explicit empty failure", args: []string{"attempt", "fail", "--detail", ""}, command: attemptCommand{kind: commandFail}},
		{name: "human request", args: []string{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "what now?"}, command: attemptCommand{kind: commandRequestHuman, idempotencyKey: "0123456789abcdef0123456789abcdef", text: "what now?"}},
		{name: "send back", args: []string{"attempt", "send-back", "--task", "0123456789abcdef0123456789abcdef", "--note", "five findings"}, command: attemptCommand{kind: commandSendBack, id: "0123456789abcdef0123456789abcdef", text: "five findings"}},
		{name: "send back maximum note", args: []string{"attempt", "send-back", "--task", "ffffffffffffffffffffffffffffffff", "--note", strings.Repeat("n", 8192)}, command: attemptCommand{kind: commandSendBack, id: "ffffffffffffffffffffffffffffffff", text: strings.Repeat("n", 8192)}},
		{name: "human request maximum question", args: []string{"attempt", "request-human", "--idempotency-key", "ffffffffffffffffffffffffffffffff", "--question", strings.Repeat("q", 8192)}, command: attemptCommand{kind: commandRequestHuman, idempotencyKey: "ffffffffffffffffffffffffffffffff", text: strings.Repeat("q", 8192)}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			command, help, ok := parse(test.args)
			if !ok || help != test.help || command != test.command {
				t.Fatalf("parse = %+v, help=%t ok=%t", command, help, ok)
			}
		})
	}
}

func TestParseOverseerTaskUpdateKeepsPriority(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	for _, test := range []struct {
		name     string
		args     []string
		priority int64
		agent    string
	}{
		{name: "priority only", args: []string{"overseer", "task", "update", "--task", id, "--revision", "7", "--priority", "-5"}, priority: -5},
		{name: "priority and agent", args: []string{"overseer", "task", "update", "--task", id, "--revision", "7", "--priority", "5", "--agent", id}, priority: 5, agent: id},
	} {
		t.Run(test.name, func(t *testing.T) {
			command, help, ok := parse(test.args)
			if !ok || help || command.kind != commandOverseerTaskUpdate || !command.prioritySet || command.priority != test.priority || command.agent != test.agent {
				t.Fatalf("parse = %+v, help=%t, ok=%t", command, help, ok)
			}
		})
	}
}

func TestParseOverseerStatusPaging(t *testing.T) {
	id := "0123456789abcdef0123456789abcdef"
	command, help, ok := parse([]string{"overseer", "status", "--task", id, "--offset", "4", "--text-offset", "4096", "--head", "7"})
	if !ok || help || command.kind != commandOverseerStatus || command.id != id || command.offset != 4 || command.textOffset != 4096 || command.head != 7 {
		t.Fatalf("paged status = %+v, help=%t, ok=%t", command, help, ok)
	}
	for _, args := range [][]string{
		{"overseer", "status", "--offset", "4"},
		{"overseer", "status", "--text-offset", "1", "--head", "7"},
	} {
		if _, _, ok := parse(args); ok {
			t.Fatalf("invalid paged status accepted: %v", args)
		}
	}
}

func TestParseExplicitHomeCommands(t *testing.T) {
	for _, test := range []struct {
		args []string
		kind commandKind
	}{
		{args: []string{"init", "--home", "/private/tmp/factory"}, kind: commandInit},
		{args: []string{"doctor", "--home", "/private/tmp/factory"}, kind: commandDoctor},
	} {
		command, help, ok := parse(test.args)
		if !ok || help || command.kind != test.kind || command.home != test.args[2] {
			t.Fatalf("parse(%q) = %+v, help=%t, ok=%t", test.args, command, help, ok)
		}
	}
	for _, args := range [][]string{
		{"init"}, {"doctor"}, {"init", "--home", "relative"},
		{"doctor", "--home", "/private/tmp/factory/.."},
		{"init", "--home", "/"},
		{"doctor", "--home", "/" + strings.Repeat("a", maxHomeArgumentBytes)},
	} {
		if _, _, ok := parse(args); ok {
			t.Fatalf("parse(%q) accepted invalid explicit home", args)
		}
	}
}

func TestHomeCLIOutputIsBoundedAndRedacted(t *testing.T) {
	parent, err := os.MkdirTemp("/private/tmp", "dark-factory-factoryctl-home-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(parent) })
	secret := "operator-token-must-not-appear"
	home := filepath.Join(parent, secret)
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"init", "--home", home}, func(string) string { return "" }, &stdout, &stderr)
	if runtime.GOOS == "darwin" {
		if exit != 0 || stdout.String() != "home initialized\n" || stderr.Len() != 0 {
			t.Fatalf("darwin init output: exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
		}
	} else if exit != exitFailure || stdout.Len() != 0 || stderr.String() != "factoryctl: Go home operations are unsupported on this platform\n" {
		t.Fatalf("unsupported init output: exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
	}
	if len(stdout.Bytes())+len(stderr.Bytes()) > 256 || strings.Contains(stdout.String()+stderr.String(), secret) {
		t.Fatalf("home output leaked or exceeded bound: stdout=%q stderr=%q", stdout.String(), stderr.String())
	}
	if runtime.GOOS == "darwin" {
		stdout.Reset()
		stderr.Reset()
		exit = run(context.Background(), []string{"doctor", "--home", home}, func(string) string { return "" }, &stdout, &stderr)
		if exit != 0 || stdout.String() != "home ready\n" || stderr.Len() != 0 {
			t.Fatalf("darwin doctor output: exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
		}
	}
}

func TestInvalidSyntaxStopsBeforeEnvironmentOrConnection(t *testing.T) {
	tests := [][]string{
		nil,
		{"attempt"},
		{"task", "done"},
		{"attempt", "unknown"},
		{"attempt", "task", "extra"},
		{"attempt", "task", "--socket", "/private/socket"},
		{"attempt", "block"},
		{"attempt", "block", "--detail", ""},
		{"attempt", "succeed", "positional"},
		{"attempt", "succeed", "--result=private-result"},
		{"attempt", "fail", "--detail=private-detail"},
		{"attempt", "succeed", "--result", "a", "--result", "b"},
		{"attempt", "fail", "--detail", "a", "--result", "b"},
		{"attempt", "block", "--detail", "a", "extra"},
		{"attempt", "succeed", "--socket", "/private/socket"},
		{"attempt", "fail", "--run", "private-run"},
		{"attempt", "request-human"},
		{"attempt", "send-back"},
		{"attempt", "send-back", "--task", "0123456789abcdef0123456789abcdef"},
		{"attempt", "send-back", "--task", "0123456789abcdef0123456789abcdef", "--note", ""},
		{"attempt", "send-back", "--task", "0123456789abcdef", "--note", "short id"},
		{"attempt", "send-back", "--note", "reordered", "--task", "0123456789abcdef0123456789abcdef"},
		{"attempt", "send-back", "--task", "0123456789abcdef0123456789abcdef", "--note", strings.Repeat("n", 8193)},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question"},
		{"attempt", "request-human", "--idempotency-key=0123456789abcdef0123456789abcdef", "--question", "private-question"},
		{"attempt", "request-human", "--question", "private-question", "--idempotency-key", "0123456789abcdef0123456789abcdef"},
		{"attempt", "request-human", "--idempotency-key", "00000000000000000000000000000000", "--question", "private-question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef", "--question", "private-question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789ABCDEF0123456789abcdef", "--question", "private-question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdegf", "--question", "private-question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", ""},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", string([]byte{0xff})},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "bad\x00question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", strings.Repeat("q", 8193)},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question=private-question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--socket", "/private/socket"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--token", "private-token"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--run", "private-run"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--task", "private-task"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--destination", "private-destination"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--action", "private-action"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--idempotency-key", "fedcba9876543210fedcba9876543210", "--question", "private-question"},
		{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question", "--question", "private-question-two"},
		{"attempt", "request_human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question"},
	}
	for index, args := range tests {
		t.Run(fmt.Sprintf("case-%d", index), func(t *testing.T) {
			lookups := 0
			var stdout, stderr bytes.Buffer
			exit := run(context.Background(), args, func(string) string {
				lookups++
				return "/private/should-not-be-read"
			}, &stdout, &stderr)
			if exit != exitUsage || lookups != 0 || stdout.Len() != 0 || stderr.String() != usage {
				t.Fatalf("run = exit %d, lookups %d, stdout %q, stderr %q", exit, lookups, stdout.String(), stderr.String())
			}
			for _, private := range []string{"private-result", "private-detail", "private-question", "private-token", "/private/socket", "private-run", "private-task", "private-destination", "private-action"} {
				if strings.Contains(stderr.String(), private) {
					t.Fatalf("usage leaked %q", private)
				}
			}
		})
	}
}

func TestHelpIsExactAndHasNoClientEffect(t *testing.T) {
	var stdout, stderr bytes.Buffer
	lookups := 0
	exit := run(context.Background(), []string{"attempt", "succeed", "--help"}, func(string) string {
		lookups++
		return "private"
	}, &stdout, &stderr)
	if exit != 0 || lookups != 0 || stdout.String() != usage || stderr.Len() != 0 {
		t.Fatalf("help = exit %d, lookups %d, stdout %q, stderr %q", exit, lookups, stdout.String(), stderr.String())
	}
}

func TestAttemptCommandsUseExactTypedCalls(t *testing.T) {
	tests := []struct {
		name string
		args []string
		kind api.CallKind
		key  string
		text string
	}{
		{name: "succeed empty", args: []string{"attempt", "succeed"}, kind: api.CallSucceed},
		{name: "succeed result", args: []string{"attempt", "succeed", "--result", "private-result-sentinel"}, kind: api.CallSucceed, text: "private-result-sentinel"},
		{name: "block", args: []string{"attempt", "block", "--detail", "private-block-sentinel"}, kind: api.CallBlock, text: "private-block-sentinel"},
		{name: "fail empty", args: []string{"attempt", "fail"}, kind: api.CallFail},
		{name: "fail detail", args: []string{"attempt", "fail", "--detail", "private-fail-sentinel"}, kind: api.CallFail, text: "private-fail-sentinel"},
		{name: "send back", args: []string{"attempt", "send-back", "--task", "fedcba9876543210fedcba9876543210", "--note", "private-note-sentinel"}, kind: api.CallSendBack, key: "fedcba9876543210fedcba9876543210", text: "private-note-sentinel"},
		{name: "human request", args: []string{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question-sentinel"}, kind: api.CallRequestHuman, key: "0123456789abcdef0123456789abcdef", text: "private-question-sentinel"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newAPIFixture(t)
			defer fixture.close(t)
			t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", fixture.attemptPath)
			done := serveOne(fixture.listener, func(api.Call) api.Reply {
				reply, err := api.NewMutationReply(api.MutationResult{Head: 17, Revision: 9})
				if err != nil {
					t.Errorf("new reply: %v", err)
				}
				return reply
			})
			var stdout, stderr bytes.Buffer
			exit := run(context.Background(), test.args, func(name string) string {
				if name == "DARK_FACTORY_SOCKET" {
					return fixture.socket
				}
				return ""
			}, &stdout, &stderr)
			result := awaitServer(t, done)
			if exit != 0 || result.err != nil || result.call.Kind() != test.kind {
				t.Fatalf("run = exit %d, call %v, server %v", exit, result.call.Kind(), result.err)
			}
			digest, ok := result.call.AttemptDigest()
			wantDigest := sha256.Sum256(fixture.bearer[:])
			if !ok || digest.Bytes() != wantDigest {
				t.Fatalf("attempt digest = %x, %t", digest.Bytes(), ok)
			}
			switch test.kind {
			case api.CallSucceed:
				text, ok := result.call.Result()
				if !ok || text != test.text {
					t.Fatalf("result = %q, %t", text, ok)
				}
			case api.CallBlock, api.CallFail:
				text, ok := result.call.Detail()
				if !ok || text != test.text {
					t.Fatalf("detail = %q, %t", text, ok)
				}
			case api.CallRequestHuman:
				input, ok := result.call.HumanQuestionInput()
				if !ok || input != (api.HumanQuestionInput{IdempotencyKey: test.key, Question: test.text}) {
					t.Fatalf("human question = %+v, %t", input, ok)
				}
			case api.CallSendBack:
				input, ok := result.call.SendBackInput()
				if !ok || input != (api.SendBackInput{TaskID: test.key, Note: test.text}) {
					t.Fatalf("send-back = %+v, %t", input, ok)
				}
			}
			wantOutput := "attempt outcome request accepted: head=17 revision=9\n"
			if test.kind == api.CallRequestHuman {
				wantOutput = "human request accepted: head=17 revision=9\n"
			} else if test.kind == api.CallSendBack {
				wantOutput = "task sent back: head=17 revision=9\n"
			}
			if stdout.String() != wantOutput || stderr.Len() != 0 {
				t.Fatalf("output = stdout %q, stderr %q", stdout.String(), stderr.String())
			}
			for _, private := range []string{fixture.socket, fixture.attemptPath, string(fixture.bearer[:]), test.key, test.text} {
				if private != "" && strings.Contains(stdout.String()+stderr.String(), private) {
					t.Fatalf("output leaked private sentinel")
				}
			}
		})
	}
}

func TestAttemptTaskUsesExactTypedCallAndWritesJSON(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", fixture.attemptPath)
	privateTask := "private\u007f\u009btask-sentinel"
	done := serveOne(fixture.listener, func(api.Call) api.Reply {
		reply, err := api.NewAttemptTaskReply(api.AttemptTask{Task: privateTask})
		if err != nil {
			t.Errorf("new attempt task reply: %v", err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := run(context.Background(), []string{"attempt", "task"}, func(name string) string {
		if name == "DARK_FACTORY_SOCKET" {
			return fixture.socket
		}
		return ""
	}, &stdout, &stderr)
	result := awaitServer(t, done)
	if exit != 0 || result.err != nil || result.call.Kind() != api.CallAttemptTask {
		t.Fatalf("run = exit %d, call %v, server %v", exit, result.call.Kind(), result.err)
	}
	digest, ok := result.call.AttemptDigest()
	wantDigest := sha256.Sum256(fixture.bearer[:])
	if !ok || digest.Bytes() != wantDigest {
		t.Fatalf("attempt digest = %x, %t", digest.Bytes(), ok)
	}
	if stdout.String() != "{\"task\":\"private\\u007f\\u009btask-sentinel\"}\n" || stderr.Len() != 0 {
		t.Fatalf("output = stdout %q, stderr %q", stdout.String(), stderr.String())
	}
	if strings.Contains(stdout.String(), "\u007f") || strings.Contains(stdout.String(), "\u009b") {
		t.Fatal("attempt task output contains terminal control characters")
	}
}

func TestAttemptTaskInvalidEnvironmentFailsNormally(t *testing.T) {
	for _, test := range []struct {
		name       string
		socket     string
		attemptEnv string
	}{
		{name: "missing socket", attemptEnv: "/private/missing-attempt-token"},
		{name: "missing attempt token", socket: "/private/missing-factory.sock"},
	} {
		t.Run(test.name, func(t *testing.T) {
			restoreEnvironment(t, "DARK_FACTORY_ATTEMPT_TOKEN_FILE")
			if test.attemptEnv == "" {
				if err := os.Unsetenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE"); err != nil {
					t.Fatal(err)
				}
			} else {
				t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", test.attemptEnv)
			}
			var stdout, stderr bytes.Buffer
			exit := run(context.Background(), []string{"attempt", "task"}, func(name string) string {
				if name == "DARK_FACTORY_SOCKET" {
					return test.socket
				}
				return ""
			}, &stdout, &stderr)
			if exit != exitFailure || stdout.Len() != 0 || stderr.String() != "factoryctl: attempt client configuration is invalid\n" {
				t.Fatalf("invalid environment = exit %d, stdout %q, stderr %q", exit, stdout.String(), stderr.String())
			}
		})
	}
}

func TestAcceptedOutputDoesNotClaimTerminalState(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", fixture.attemptPath)
	done := serveOne(fixture.listener, func(api.Call) api.Reply {
		reply, _ := api.NewMutationReply(api.MutationResult{Head: 0, Revision: 1})
		return reply
	})
	var stdout, stderr bytes.Buffer
	if exit := run(context.Background(), []string{"attempt", "succeed"}, func(string) string { return fixture.socket }, &stdout, &stderr); exit != 0 {
		t.Fatalf("run exit = %d, stderr = %q", exit, stderr.String())
	}
	if result := awaitServer(t, done); result.err != nil {
		t.Fatal(result.err)
	}
	want := "attempt outcome request accepted: head=0 revision=1\n"
	if stdout.String() != want || strings.Contains(stdout.String(), "terminal") || strings.Contains(stdout.String(), "succeeded") {
		t.Fatalf("accepted output = %q", stdout.String())
	}
}

func TestRuntimeErrorsAreFixedAndPrivate(t *testing.T) {
	key := "0123456789abcdef0123456789abcdef"
	question := "private-human-question-sentinel"
	tests := []api.RemoteErrorCode{
		api.RemoteInvalidRequest,
		api.RemoteUnauthorized,
		api.RemoteForbidden,
		api.RemoteNotFound,
		api.RemoteConflict,
		api.RemoteRevisionConflict,
		api.RemoteTooLarge,
		api.RemoteUnavailable,
		api.RemoteInternal,
	}
	for _, code := range tests {
		t.Run(string(code), func(t *testing.T) {
			fixture := newAPIFixture(t)
			defer fixture.close(t)
			t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", fixture.attemptPath)
			done := serveOne(fixture.listener, func(api.Call) api.Reply {
				reply, err := api.NewErrorReply(code)
				if err != nil {
					t.Errorf("new error reply: %v", err)
				}
				return reply
			})
			var stdout, stderr bytes.Buffer
			exit := run(context.Background(), []string{"attempt", "request-human", "--idempotency-key", key, "--question", question}, func(string) string { return fixture.socket }, &stdout, &stderr)
			if result := awaitServer(t, done); result.err != nil {
				t.Fatal(result.err)
			}
			if exit != exitFailure || stdout.Len() != 0 || stderr.String() != "factoryctl: human request was not accepted\n" {
				t.Fatalf("error output = exit %d, stdout %q, stderr %q", exit, stdout.String(), stderr.String())
			}
			for _, private := range []string{fixture.socket, fixture.attemptPath, string(fixture.bearer[:]), key, question} {
				if strings.Contains(stdout.String()+stderr.String(), private) {
					t.Fatalf("error output leaked private sentinel")
				}
			}
		})
	}
}

func TestMissingSocketOrAttemptTokenCannotFallBack(t *testing.T) {
	t.Run("missing socket", func(t *testing.T) {
		t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", "/private/missing-attempt-token")
		var stdout, stderr bytes.Buffer
		exit := run(context.Background(), []string{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question"}, func(string) string { return "" }, &stdout, &stderr)
		if exit != exitFailure || stdout.Len() != 0 || stderr.String() != "factoryctl: attempt client configuration is invalid\n" {
			t.Fatalf("missing socket = exit %d, stdout %q, stderr %q", exit, stdout.String(), stderr.String())
		}
	})

	for _, test := range []struct {
		name     string
		setToken bool
	}{
		{name: "absent attempt token cannot use operator token"},
		{name: "empty attempt token cannot use operator token", setToken: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newAPIFixture(t)
			defer fixture.close(t)
			restoreEnvironment(t, "DARK_FACTORY_ATTEMPT_TOKEN_FILE")
			if test.setToken {
				if err := os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", ""); err != nil {
					t.Fatal(err)
				}
			} else if err := os.Unsetenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE"); err != nil {
				t.Fatal(err)
			}
			t.Setenv("DARK_FACTORY_OPERATOR_TOKEN_FILE", filepath.Join(fixture.directory, "home", "operator.token"))
			accepted := make(chan bool, 1)
			listener := fixture.listener
			go func() {
				connection, err := listener.Accept()
				if err == nil {
					_ = connection.Close()
					accepted <- true
					return
				}
				accepted <- false
			}()
			var stdout, stderr bytes.Buffer
			exit := run(context.Background(), []string{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "private-question"}, func(string) string { return fixture.socket }, &stdout, &stderr)
			_ = listener.Close()
			select {
			case connected := <-accepted:
				if connected {
					t.Fatal("missing attempt token connected with operator authority")
				}
			case <-time.After(time.Second):
				t.Fatal("accept did not stop")
			}
			fixture.listener = nil
			if exit != exitFailure || stdout.Len() != 0 || stderr.String() != "factoryctl: attempt client configuration is invalid\n" {
				t.Fatalf("missing attempt token = exit %d, stdout %q, stderr %q", exit, stdout.String(), stderr.String())
			}
		})
	}
}

func restoreEnvironment(t testing.TB, name string) {
	t.Helper()
	value, found := os.LookupEnv(name)
	t.Cleanup(func() {
		if found {
			_ = os.Setenv(name, value)
		} else {
			_ = os.Unsetenv(name)
		}
	})
}

func TestCancellationAndDeadlineJoinConnectionOwnership(t *testing.T) {
	tests := []struct {
		name       string
		newContext func() (context.Context, context.CancelFunc)
		want       string
	}{
		{name: "canceled", newContext: func() (context.Context, context.CancelFunc) {
			return context.WithCancel(context.Background())
		}, want: "factoryctl: outcome request canceled\n"},
		{name: "deadline", newContext: func() (context.Context, context.CancelFunc) {
			return context.WithTimeout(context.Background(), 50*time.Millisecond)
		}, want: "factoryctl: outcome request timed out\n"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newAPIFixture(t)
			defer fixture.close(t)
			t.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", fixture.attemptPath)
			accepted := make(chan *api.Connection, 1)
			go func() {
				connection, _ := fixture.listener.Accept()
				accepted <- connection
			}()
			ctx, cancel := test.newContext()
			defer cancel()
			var stdout, stderr bytes.Buffer
			done := make(chan int, 1)
			go func() {
				done <- run(ctx, []string{"attempt", "fail"}, func(string) string { return fixture.socket }, &stdout, &stderr)
			}()
			var connection *api.Connection
			select {
			case connection = <-accepted:
				if connection == nil {
					t.Fatal("server did not accept connection")
				}
			case <-time.After(time.Second):
				t.Fatal("connection was not accepted")
			}
			if test.name == "canceled" {
				cancel()
			}
			select {
			case exit := <-done:
				if exit != exitFailure || stdout.Len() != 0 || stderr.String() != test.want {
					t.Fatalf("canceled run = exit %d, stdout %q, stderr %q", exit, stdout.String(), stderr.String())
				}
			case <-time.After(500 * time.Millisecond):
				t.Fatal("run did not stop promptly")
			}
			if err := connection.Close(); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestRepeatedCallsLeaveNoFDOrSocketOrTemp(t *testing.T) {
	baselineFD := countFDs(t)
	previousToken, tokenWasSet := os.LookupEnv("DARK_FACTORY_ATTEMPT_TOKEN_FILE")
	defer func() {
		if tokenWasSet {
			_ = os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", previousToken)
		} else {
			_ = os.Unsetenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE")
		}
	}()
	for index := 0; index < 12; index++ {
		fixture := newAPIFixture(t)
		os.Setenv("DARK_FACTORY_ATTEMPT_TOKEN_FILE", fixture.attemptPath)
		done := serveOne(fixture.listener, func(api.Call) api.Reply {
			reply, _ := api.NewMutationReply(api.MutationResult{Head: uint64(index), Revision: uint64(index + 1)})
			return reply
		})
		var stdout, stderr bytes.Buffer
		if exit := run(context.Background(), []string{"attempt", "fail"}, func(string) string { return fixture.socket }, &stdout, &stderr); exit != 0 {
			t.Fatalf("iteration %d exit = %d, stderr = %q", index, exit, stderr.String())
		}
		if result := awaitServer(t, done); result.err != nil {
			t.Fatalf("iteration %d: %v", index, result.err)
		}
		fixture.close(t)
	}
	if got := countFDs(t); got != baselineFD {
		t.Fatalf("FD count = %d, want %d", got, baselineFD)
	}
}

func countFDs(t testing.TB) int {
	t.Helper()
	entries, err := os.ReadDir("/dev/fd")
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}
