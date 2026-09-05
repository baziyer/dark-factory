//go:build darwin || linux

package api

import (
	"context"
	"errors"
	"net"
	"strings"
	"testing"
)

const (
	contractNodeID  = "abcdefghijklmnopqrstuvwxyz234567"
	contractRelayWS = "wss://relay.darkfactory.build"
)

func TestRemoteMethodsDecodeAsEmptyOperatorCalls(t *testing.T) {
	bearer := testCredential('R')
	for _, test := range []struct {
		name string
		body string
		kind CallKind
	}{
		{name: "status", body: `{"method":"remote_status","params":{}}`, kind: CallRemoteStatus},
	} {
		t.Run(test.name, func(t *testing.T) {
			call, code := decodeCall(operatorDomain, bearer, []byte(test.body))
			if code != "" || call.Kind() != test.kind {
				t.Fatalf("decode = %v, %q", call.Kind(), code)
			}
			// An operator call must never carry an attempt identity, and the
			// no-input operations expose no accessor at all.
			if _, ok := call.AttemptDigest(); ok {
				t.Fatal("remote call exposed an attempt digest")
			}
			if _, code := decodeCall(attemptDomain, bearer, []byte(test.body)); code != RemoteForbidden {
				t.Fatalf("attempt domain = %q, want %s", code, RemoteForbidden)
			}
			// Tolerance is additive: an unknown member reaches no field of a
			// no-input call, while a duplicate name is still refused outright.
			withInput := strings.Replace(test.body, `"params":{}`, `"params":{"node_id":"`+contractNodeID+`"}`, 1)
			if _, code := decodeCall(operatorDomain, bearer, []byte(withInput)); code != "" {
				t.Fatalf("unknown member = %q, want tolerated", code)
			}
			duplicated := strings.Replace(test.body, `"params":{}`, `"params":{"a":1,"a":2}`, 1)
			if _, code := decodeCall(operatorDomain, bearer, []byte(duplicated)); code != RemoteInvalidRequest {
				t.Fatalf("duplicate member = %q, want %s", code, RemoteInvalidRequest)
			}
		})
	}
}

func TestRemoteRepliesArePairedWithTheirOwnCalls(t *testing.T) {
	status, err := NewRemoteStatusReply(RemoteStatus{NodeID: contractNodeID, RelayOrigin: contractRelayWS})
	if err != nil {
		t.Fatal(err)
	}
	if !replyMatches(CallRemoteStatus, status.kind) || replyMatches(CallWebStatus, status.kind) {
		t.Fatal("the remote status reply does not match exactly its own call")
	}
}

func TestRemoteStatusValidationIsStrict(t *testing.T) {
	connected := RemoteStatus{NodeID: contractNodeID, RelayOrigin: contractRelayWS, Connected: true, Sessions: 3}
	if !validRemoteStatus(connected) || !validRemoteStatus(RemoteStatus{NodeID: contractNodeID, RelayOrigin: "ws://127.0.0.1:8787"}) {
		t.Fatal("canonical remote status rejected")
	}
	for name, invalid := range map[string]RemoteStatus{
		"missing node":         {RelayOrigin: contractRelayWS},
		"https origin":         {NodeID: contractNodeID, RelayOrigin: "https://relay.darkfactory.build"},
		"empty origin":         {NodeID: contractNodeID},
		"wildcard origin":      {NodeID: contractNodeID, RelayOrigin: "wss://*"},
		"negative sessions":    {NodeID: contractNodeID, RelayOrigin: contractRelayWS, Connected: true, Sessions: -1},
		"sessions while down":  {NodeID: contractNodeID, RelayOrigin: contractRelayWS, Sessions: 1},
		"space in relay":       {NodeID: contractNodeID, RelayOrigin: "wss://relay a"},
		"node id is hex-wide":  {NodeID: "0123456789abcdef0123456789abcdef", RelayOrigin: contractRelayWS},
		"node id has padding":  {NodeID: "abcdefghijklmnopqrstuvwxyz23456=", RelayOrigin: contractRelayWS},
		"origin without proto": {NodeID: contractNodeID, RelayOrigin: "relay.darkfactory.build"},
	} {
		if validRemoteStatus(invalid) {
			t.Fatalf("invalid remote status accepted: %s", name)
		}
		if _, err := NewRemoteStatusReply(invalid); !errors.Is(err, ErrInvalidInput) {
			t.Fatalf("invalid remote status reply constructed: %s: %v", name, err)
		}
	}
}

func TestRemoteClientUsesExactWireAndRefusesInexactReplies(t *testing.T) {
	statusBody := `{"node_id":"` + contractNodeID + `","relay_origin":"` + contractRelayWS + `","connected":true,"sessions":2}`
	for _, test := range []struct {
		name     string
		response string
		request  string
		call     func(*OperatorClient) (any, error)
		wantErr  error
	}{
		{
			name: "status", response: successResponse(statusBody), request: `{"method":"remote_status","params":{}}`,
			call: func(client *OperatorClient) (any, error) { return client.RemoteStatus(context.Background()) },
		},
		{
			name: "status refuses sessions without a connection", response: successResponse(`{"node_id":"` + contractNodeID + `","relay_origin":"` + contractRelayWS + `","connected":false,"sessions":2}`),
			request: `{"method":"remote_status","params":{}}`, wantErr: ErrProtocol,
			call: func(client *OperatorClient) (any, error) { return client.RemoteStatus(context.Background()) },
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			bearer := testCredential('R')
			fixture := newWireFixture(t, bearer, func(connection net.Conn, _ []byte) error {
				return writeTestResponse(connection, wireOperatorDomain, test.response)
			})
			client, err := NewOperatorClient(fixture.socket, fixture.token)
			if err != nil {
				t.Fatal(err)
			}
			result, callErr := test.call(client)
			if test.wantErr != nil {
				if !errors.Is(callErr, test.wantErr) {
					t.Fatalf("call error = %v, want %v", callErr, test.wantErr)
				}
				// A refused reply yields nothing at all.
				switch value := result.(type) {
				case RemoteStatus:
					if value != (RemoteStatus{}) {
						t.Fatalf("refused status retained %+v", value)
					}
				default:
					t.Fatalf("unexpected result %T", result)
				}
			} else if callErr != nil {
				t.Fatal(callErr)
			}
			if got := requestJSON(t, <-fixture.request, wireOperatorDomain, bearer); got != test.request {
				t.Fatalf("request = %s, want %s", got, test.request)
			}
			fixture.wait(t)
		})
	}
}
