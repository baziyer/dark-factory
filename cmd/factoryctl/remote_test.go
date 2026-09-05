//go:build darwin || linux

package main

import (
	"bytes"
	"context"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/api"
)

const remoteTestNode = "abcdefghijklmnopqrstuvwxyz234567"

func TestParseExactRemoteCommands(t *testing.T) {
	for _, test := range []struct {
		name    string
		args    []string
		command attemptCommand
		help    bool
	}{
		{name: "remote help", args: []string{"remote", "--help"}, help: true},
		{name: "status help", args: []string{"remote", "status", "--help"}, help: true},
		{name: "status", args: []string{"remote", "status"}, command: attemptCommand{kind: commandRemoteStatus}},
	} {
		t.Run(test.name, func(t *testing.T) {
			command, help, ok := parse(test.args)
			if !ok || help != test.help || command != test.command {
				t.Fatalf("parse = %+v, help=%t, ok=%t", command, help, ok)
			}
		})
	}
}

func TestInvalidRemoteSyntaxStopsBeforeEnvironment(t *testing.T) {
	for index, args := range [][]string{
		{"remote"},
		{"remote", "open"},
		{"remote", "pair"},
		{"remote", "pair", "--help"},
		{"remote", "status", "extra"},
	} {
		t.Run(string(rune('a'+index)), func(t *testing.T) {
			lookups := 0
			var stdout, stderr bytes.Buffer
			exit := runWithOpener(context.Background(), args, func(string) string {
				lookups++
				return "/private/should-not-be-read"
			}, &stdout, &stderr, nil)
			if exit != exitUsage || lookups != 0 || stdout.Len() != 0 || stderr.String() != usage {
				t.Fatalf("run = exit %d lookups %d stdout %q stderr %q", exit, lookups, stdout.String(), stderr.String())
			}
		})
	}
}

func TestRemoteCommandsReportADisabledRelayExactly(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		if call.Kind() != api.CallRemoteStatus {
			return mustWebErrorReply(t, api.RemoteInvalidRequest)
		}
		return mustWebErrorReply(t, api.RemoteNotFound)
	})
	var stdout, stderr bytes.Buffer
	exit := runWithOpener(context.Background(), []string{"remote", "status"}, webEnvironment(fixture), &stdout, &stderr, nil)
	if result := awaitServer(t, done); result.err != nil {
		t.Fatal(result.err)
	}
	if exit == 0 || stdout.Len() != 0 || stderr.String() != "factoryctl: remote access is not enabled; start factoryd with --relay-origin\n" {
		t.Fatalf("disabled relay = exit %d stdout %q stderr %q", exit, stdout.String(), stderr.String())
	}
}

func TestRemoteStatusPrintsTheNodeRelayAndConnection(t *testing.T) {
	fixture := newAPIFixture(t)
	defer fixture.close(t)
	done := serveOne(fixture.listener, func(call api.Call) api.Reply {
		if call.Kind() != api.CallRemoteStatus {
			return mustWebErrorReply(t, api.RemoteInvalidRequest)
		}
		reply, err := api.NewRemoteStatusReply(api.RemoteStatus{NodeID: remoteTestNode, RelayOrigin: "wss://relay.darkfactory.build", Connected: true, Sessions: 2})
		if err != nil {
			t.Fatal(err)
		}
		return reply
	})
	var stdout, stderr bytes.Buffer
	exit := runWithOpener(context.Background(), []string{"remote", "status"}, webEnvironment(fixture), &stdout, &stderr, nil)
	if result := awaitServer(t, done); result.err != nil {
		t.Fatal(result.err)
	}
	want := `{"node_id":"` + remoteTestNode + `","relay_origin":"wss://relay.darkfactory.build","connected":true,"sessions":2}` + "\n"
	if exit != 0 || stderr.Len() != 0 || stdout.String() != want {
		t.Fatalf("remote status = exit %d stdout %q stderr %q", exit, stdout.String(), stderr.String())
	}
}
