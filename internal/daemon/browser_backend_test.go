//go:build darwin || linux

package daemon

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"net/http"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const adapterOrigin = "https://app.darkfactory.build"

type adapterFixture struct {
	store   *kernel.Store
	daemon  *Daemon
	runtime *BrowserRuntime
	backend *browserBackend
	server  *browser.Server
	key     *ecdsa.PrivateKey
	client  kernel.BrowserClient
	clock   time.Time
}

func newAdapterFixture(t *testing.T, capabilities kernel.BrowserCapabilityMask) *adapterFixture {
	t.Helper()
	clock := time.UnixMilli(2_000)
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{DispatchEnabled: true, Capacity: 8}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	daemon, err := newDaemon(store, func() time.Time { return clock })
	if err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	runtime, err := daemon.ListenBrowser("127.0.0.1:0", []string{adapterOrigin})
	if err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	backend, server := runtime.backend, runtime.server
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	fixture := &adapterFixture{store: store, daemon: daemon, runtime: runtime, backend: backend, server: server, key: key, clock: clock}
	challenge := bytes.Repeat([]byte{0x43}, browserprotocol.ChallengeSize)
	if _, err := store.CreateBrowserPairingChallenge(context.Background(), kernel.HashBrowserChallenge(challenge), backend.boot, adapterOrigin, capabilities, adapterTime(t, 1_000), adapterTime(t, 3_000)); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := daemon.Close(); err != nil {
			t.Errorf("close daemon: %v", err)
		}
		if err := store.Close(); err != nil {
			t.Errorf("close store: %v", err)
		}
	})
	return fixture
}

func (fixture *adapterFixture) pair(t *testing.T) *websocket.Conn {
	t.Helper()
	connection := adapterDial(t, fixture.server)
	hello := adapterRead(t, connection).Body.(browserprotocol.Hello)
	challenge := bytes.Repeat([]byte{0x43}, browserprotocol.ChallengeSize)
	publicKey := elliptic.Marshal(elliptic.P256(), fixture.key.PublicKey.X, fixture.key.PublicKey.Y)
	transcript, err := browserprotocol.BuildPairTranscript(browserprotocol.PairTranscript{
		DaemonID: adapterHex(t, hello.DaemonID, browserprotocol.DaemonIDSize), BootID: adapterHex(t, hello.BootID, browserprotocol.BootIDSize),
		ConnectionNonce: adapterHex(t, hello.ConnectionNonce, browserprotocol.NonceSize), Challenge: challenge, PublicKeySEC1: publicKey,
		ValidatedHost: fixture.server.Addr(), ValidatedOrigin: adapterOrigin,
	})
	if err != nil {
		t.Fatal(err)
	}
	proof, err := browserprotocol.EncodePairProve("pair", browserprotocol.PairProve{
		Challenge: hex.EncodeToString(challenge), PublicKeySEC1: hex.EncodeToString(publicKey), Signature: hex.EncodeToString(adapterSign(t, fixture.key, transcript)),
	})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, proof)
	frame := adapterRead(t, connection)
	if frame.Type != browserprotocol.TypePairResult || frame.ID != "pair" {
		t.Fatalf("pair result = %+v", frame)
	}
	result := frame.Body.(browserprotocol.PairResult)
	rawClient := adapterHex(t, result.ClientID, browserprotocol.ClientIDSize)
	clientID, err := kernel.BrowserClientIDFromBytes(rawClient)
	if err != nil {
		t.Fatal(err)
	}
	client, found, err := fixture.store.BrowserClient(context.Background(), clientID)
	if err != nil || !found {
		t.Fatalf("durable paired client = %+v, found=%v, err=%v", client, found, err)
	}
	fixture.client = client
	want := browserprotocol.CapabilityObserve
	if client.CapabilityMask.Has(kernel.BrowserCapabilityPrivateHumanRequestDetail) {
		want |= browserprotocol.CapabilityPrivateHumanRequestDetail
	}
	if client.CapabilityMask.Has(kernel.BrowserCapabilityHumanActions) {
		want |= browserprotocol.CapabilityHumanActions
	}
	if client.CapabilityMask.Has(kernel.BrowserCapabilityTerminalInput) {
		want |= browserprotocol.CapabilityTerminalInput
	}
	if result.Capabilities != want {
		t.Fatalf("advertised capabilities = %d, want implemented %d", result.Capabilities, want)
	}
	return connection
}

func (fixture *adapterFixture) authenticate(t *testing.T) *websocket.Conn {
	t.Helper()
	connection := adapterDial(t, fixture.server)
	hello := adapterRead(t, connection).Body.(browserprotocol.Hello)
	transcript, err := browserprotocol.BuildAuthTranscript(browserprotocol.AuthTranscript{
		DaemonID: adapterHex(t, hello.DaemonID, browserprotocol.DaemonIDSize), BootID: adapterHex(t, hello.BootID, browserprotocol.BootIDSize),
		ConnectionNonce: adapterHex(t, hello.ConnectionNonce, browserprotocol.NonceSize), ClientID: fixture.client.ID.Bytes(),
		ValidatedHost: fixture.server.Addr(), ValidatedOrigin: adapterOrigin,
	})
	if err != nil {
		t.Fatal(err)
	}
	proof, err := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{ClientID: fixture.client.ID.String(), Signature: hex.EncodeToString(adapterSign(t, fixture.key, transcript))})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, proof)
	frame := adapterRead(t, connection)
	if frame.Type != browserprotocol.TypeAuthResult || frame.ID != "auth" {
		t.Fatalf("auth result = %+v", frame)
	}
	return connection
}

func adapterDial(t *testing.T, server *browser.Server) *websocket.Conn {
	t.Helper()
	header := make(http.Header)
	header.Set("Origin", adapterOrigin)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	connection, _, err := websocket.Dial(ctx, "ws://"+server.Addr()+browser.Path, &websocket.DialOptions{HTTPHeader: header, CompressionMode: websocket.CompressionDisabled})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = connection.CloseNow() })
	return connection
}

func adapterRead(t *testing.T, connection *websocket.Conn) browserprotocol.ControlFrame {
	t.Helper()
	frame, _ := adapterReadPayload(t, connection)
	return frame
}

func adapterReadPayload(t *testing.T, connection *websocket.Conn) (browserprotocol.ControlFrame, []byte) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	kind, payload, err := connection.Read(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if kind != websocket.MessageText {
		t.Fatalf("frame kind = %d", kind)
	}
	frame, err := browserprotocol.DecodeServerControl(payload)
	if err != nil {
		t.Fatalf("decode server frame %q: %v", payload, err)
	}
	return frame, payload
}

func adapterWrite(t *testing.T, connection *websocket.Conn, payload []byte) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	if err := connection.Write(ctx, websocket.MessageText, payload); err != nil {
		t.Fatal(err)
	}
}

func adapterSign(t *testing.T, key *ecdsa.PrivateKey, transcript []byte) []byte {
	t.Helper()
	digest := sha256.Sum256(transcript)
	r, s, err := ecdsa.Sign(rand.Reader, key, digest[:])
	if err != nil {
		t.Fatal(err)
	}
	result := make([]byte, browserprotocol.SignatureSize)
	r.FillBytes(result[:32])
	s.FillBytes(result[32:])
	return result
}

func adapterHex(t *testing.T, value string, size int) []byte {
	t.Helper()
	decoded, err := hex.DecodeString(value)
	if err != nil || len(decoded) != size {
		t.Fatalf("fixed hex %q = %x, %v", value, decoded, err)
	}
	return decoded
}

func adapterTime(t *testing.T, value int64) kernel.UnixMillis {
	t.Helper()
	result, err := kernel.NewUnixMillis(value)
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func adapterID(t *testing.T, seed byte) []byte {
	t.Helper()
	value := bytes.Repeat([]byte{seed}, kernel.IDBytes)
	value[len(value)-1] ^= 0x5a
	return value
}

func TestBrowserAdapterPairsAuthenticatesSnapshotsAndReloadsRevocation(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail|kernel.BrowserCapabilityHumanActions|kernel.BrowserCapabilityTerminalInput)
	ctx := context.Background()
	projectID, _ := kernel.ProjectIDFromBytes(adapterID(t, 1))
	agentID, _ := kernel.AgentIDFromBytes(adapterID(t, 2))
	taskID, _ := kernel.TaskIDFromBytes(adapterID(t, 3))
	incarnationID, _ := kernel.IncarnationIDFromBytes(adapterID(t, 4))
	project, err := fixture.store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "public project", Root: "/PRIVATE_ROOT_SENTINEL"}, adapterTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := fixture.store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: project.ID, Name: "public agent", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderCodex, Model: "SERVED_MODEL_FACT", ToolBudgetLimit: 4}, adapterTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fixture.store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID, Title: "public task", Body: "PRIVATE_BODY_SENTINEL"}, adapterTime(t, 12)); err != nil {
		t.Fatal(err)
	}

	paired := fixture.pair(t)
	stateGet, _ := browserprotocol.EncodeStateGet("state-1", browserprotocol.StateGet{})
	adapterWrite(t, paired, stateGet)
	frame, payload := adapterReadPayload(t, paired)
	if frame.Type != browserprotocol.TypeStateSnapshot {
		t.Fatalf("state frame = %+v", frame)
	}
	snapshot := frame.Body.(browserprotocol.StateSnapshot)
	if len(snapshot.Projects) != 1 || len(snapshot.Agents) != 1 || len(snapshot.Tasks) != 1 {
		t.Fatalf("one snapshot did not carry the whole Factory: %+v", snapshot)
	}
	for _, sentinel := range []string{"PRIVATE_ROOT_SENTINEL", "PRIVATE_BODY_SENTINEL"} {
		if bytes.Contains(payload, []byte(sentinel)) {
			t.Fatalf("public state leaked %q: %s", sentinel, payload)
		}
	}
	// The launch controls the console edits are served facts, not leaks.
	if !bytes.Contains(payload, []byte("SERVED_MODEL_FACT")) {
		t.Fatalf("public state dropped the served model: %s", payload)
	}

	authenticated := fixture.authenticate(t)
	stateGet, _ = browserprotocol.EncodeStateGet("state-2", browserprotocol.StateGet{})
	adapterWrite(t, authenticated, stateGet)
	if frame := adapterRead(t, authenticated); frame.Type != browserprotocol.TypeStateSnapshot {
		t.Fatalf("authenticated state = %+v", frame)
	}
	revoked, err := fixture.store.RevokeBrowserClient(ctx, fixture.client.ID, fixture.client.Revision, adapterTime(t, 2_100))
	if err != nil || revoked.RevokedAt == nil {
		t.Fatalf("revoke = %+v, %v", revoked, err)
	}
	stateGet, _ = browserprotocol.EncodeStateGet("state-3", browserprotocol.StateGet{})
	adapterWrite(t, authenticated, stateGet)
	frame = adapterRead(t, authenticated)
	if frame.Type != browserprotocol.TypeError || frame.Body.(browserprotocol.Error).Code != browserprotocol.ErrorUnauthorized {
		t.Fatalf("post-revoke operation = %+v", frame)
	}
}

func TestBrowserAdapterBadPairProofDoesNotConsumeChallenge(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	connection := adapterDial(t, fixture.server)
	hello := adapterRead(t, connection).Body.(browserprotocol.Hello)
	challenge := bytes.Repeat([]byte{0x43}, browserprotocol.ChallengeSize)
	publicKey := elliptic.Marshal(elliptic.P256(), fixture.key.PublicKey.X, fixture.key.PublicKey.Y)
	bad, _ := browserprotocol.EncodePairProve("bad", browserprotocol.PairProve{Challenge: hex.EncodeToString(challenge), PublicKeySEC1: hex.EncodeToString(publicKey), Signature: strings.Repeat("01", browserprotocol.SignatureSize)})
	adapterWrite(t, connection, bad)
	if frame := adapterRead(t, connection); frame.Type != browserprotocol.TypeError || frame.Body.(browserprotocol.Error).Code != browserprotocol.ErrorUnauthorized {
		t.Fatalf("bad proof = %+v", frame)
	}
	// A failed proof closes only its connection. The same durable challenge is
	// still redeemable with the exact signed transcript on a fresh connection.
	paired := fixture.pair(t)
	_ = paired
	if fixture.client.ID == (kernel.BrowserClientID{}) {
		t.Fatal("correct proof after bad proof did not pair")
	}
	_ = hello
}

func TestBrowserAdapterObserveOnlyCannotReadPrivateDetail(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	fixture.pair(t)
	requestID, _ := kernel.HumanRequestIDFromBytes(adapterID(t, 90))
	revision, _ := kernel.NewRevision(1)
	_, err := fixture.backend.HumanRequestDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.HumanRequestDetailGet{RequestID: requestID.String(), ExpectedRevision: decimalRevision(revision)})
	if !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("observe-only private detail = %v", err)
	}
}

func TestBrowserAdapterTerminalTargetProjectsExactActiveAndNoTarget(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	connection := fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 140)
	agent, found, err := fixture.store.Agent(context.Background(), run.AgentID)
	if err != nil || !found {
		t.Fatalf("agent = %+v, found=%v, err=%v", agent, found, err)
	}
	state, err := fixture.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	request := browserprotocol.TerminalTargetGet{AgentID: agent.ID.String(), ExpectedAgentRevision: decimalRevision(agent.Revision), ExpectedHead: decimalSequence(state.Head)}
	result, err := fixture.backend.TerminalTarget(context.Background(), rawBrowserClient(fixture.client.ID), request)
	if err != nil || result.AgentID != request.AgentID || result.AgentRevision != request.ExpectedAgentRevision || result.Head != request.ExpectedHead || result.Target == nil {
		t.Fatalf("active target = %+v, err=%v", result, err)
	}
	if result.Target.RunID != run.ID.String() || result.Target.SessionID == "" || result.Target.RunRevision != decimalRevision(run.Revision) {
		t.Fatalf("active target coordinates = %+v, run=%+v", result.Target, run)
	}
	wireRequest, err := browserprotocol.EncodeTerminalTargetGet("target-wire", request)
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, wireRequest)
	wireResult := adapterRead(t, connection)
	wireTarget, ok := wireResult.Body.(browserprotocol.TerminalTarget)
	if wireResult.Type != browserprotocol.TypeTerminalTarget || wireResult.ID != "target-wire" || !ok || wireTarget.AgentID != request.AgentID || wireTarget.AgentRevision != request.ExpectedAgentRevision || wireTarget.Head != request.ExpectedHead || wireTarget.Target == nil || wireTarget.Target.RunID != run.ID.String() || wireTarget.Target.SessionID != result.Target.SessionID || wireTarget.Target.RunRevision != result.Target.RunRevision || wireTarget.Target.SessionRevision != result.Target.SessionRevision {
		t.Fatalf("wire target = %+v", wireResult)
	}

	projectID, _ := kernel.ProjectIDFromBytes(adapterID(t, 150))
	agentID, _ := kernel.AgentIDFromBytes(adapterID(t, 151))
	project, err := fixture.store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "no-target-project", Root: "/no-target-project"}, adapterTime(t, 700))
	if err != nil {
		t.Fatal(err)
	}
	noRunAgent, err := fixture.store.CreateAgent(context.Background(), kernel.NewAgent{ID: agentID, ProjectID: project.ID, Name: "no-target-agent", Role: kernel.RoleWorker, Provider: kernel.ProviderShell, ToolBudgetLimit: 1}, adapterTime(t, 701))
	if err != nil {
		t.Fatal(err)
	}
	state, err = fixture.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	noTargetRequest := browserprotocol.TerminalTargetGet{AgentID: noRunAgent.ID.String(), ExpectedAgentRevision: decimalRevision(noRunAgent.Revision), ExpectedHead: decimalSequence(state.Head)}
	noTarget, err := fixture.backend.TerminalTarget(context.Background(), rawBrowserClient(fixture.client.ID), noTargetRequest)
	if err != nil || noTarget.Target != nil || noTarget.AgentID != noTargetRequest.AgentID || noTarget.AgentRevision != noTargetRequest.ExpectedAgentRevision || noTarget.Head != noTargetRequest.ExpectedHead {
		t.Fatalf("no target = %+v, err=%v", noTarget, err)
	}
	stale := noTargetRequest
	stale.ExpectedHead++
	if _, err := fixture.backend.TerminalTarget(context.Background(), rawBrowserClient(fixture.client.ID), stale); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("stale head error = %v", err)
	}
}

// snapshotRequest returns the public card for one request, if the coherent
// snapshot still carries it. A resolved request simply stops appearing.
func snapshotRequest(t *testing.T, fixture *adapterFixture, id kernel.HumanRequestID) (browserprotocol.HumanRequestItem, bool) {
	t.Helper()
	snapshot, err := fixture.backend.StateSnapshot(context.Background(), rawBrowserClient(fixture.client.ID))
	if err != nil {
		t.Fatal(err)
	}
	for _, item := range snapshot.HumanRequests {
		if item.ID == id.String() {
			return item, true
		}
	}
	return browserprotocol.HumanRequestItem{}, false
}

func TestBrowserAdapterHumanRequestDetailAndResolvedRequestLeavesTheSnapshot(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail|kernel.BrowserCapabilityHumanActions)
	fixture.pair(t)
	run := adapterRunningRun(t, fixture.store, 100)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 120))
	request, err := fixture.store.CreateHumanQuestionForAttempt(context.Background(), run.CredentialDigest, kernel.NewHumanQuestion{
		IdempotencyKey: key, QuestionText: "QUESTION_PRIVATE_SENTINEL",
	}, adapterTime(t, 500))
	if err != nil {
		t.Fatal(err)
	}
	item, present := snapshotRequest(t, fixture, request.ID)
	if !present || item.Revision != decimalRevision(request.Revision) {
		t.Fatalf("open request card = %+v, present=%v", item, present)
	}
	if strings.Contains(fmt.Sprintf("%+v", item), "QUESTION_PRIVATE_SENTINEL") {
		t.Fatalf("unsafe public HumanRequest item = %+v", item)
	}
	detail, err := fixture.backend.HumanRequestDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.HumanRequestDetailGet{
		RequestID: request.ID.String(), ExpectedRevision: decimalRevision(request.Revision),
	})
	if err != nil || detail.Question != "QUESTION_PRIVATE_SENTINEL" || !bool(detail.CanReply) || detail.TerminalTarget == nil || detail.TerminalTarget.RunID != run.ID.String() || detail.CancelRun == nil || detail.CancelRun.ExpectedRequestRevision != decimalRevision(request.Revision) || detail.CancelRun.ExpectedRunRevision != decimalRevision(run.Revision) {
		t.Fatalf("authorized detail = %+v, %v", detail, err)
	}
	deliveryID, _ := kernel.HumanRequestDeliveryIDFromBytes(adapterID(t, 121))
	delivery, err := fixture.store.BeginHumanReply(context.Background(), fixture.client.ID, request.ID, request.Revision, deliveryID, "reply", adapterTime(t, 501))
	if err != nil {
		t.Fatal(err)
	}
	deliveringDetail, err := fixture.backend.HumanRequestDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.HumanRequestDetailGet{RequestID: request.ID.String(), ExpectedRevision: decimalRevision(delivery.Revision)})
	if err != nil || bool(deliveringDetail.CanReply) || deliveringDetail.TerminalTarget != nil || deliveringDetail.CancelRun != nil {
		t.Fatalf("delivering detail = %+v, %v", deliveringDetail, err)
	}
	if err := fixture.store.AcknowledgeHumanReply(context.Background(), request.ID, delivery.DeliveryID, delivery.Revision, adapterTime(t, 502)); err != nil {
		t.Fatal(err)
	}
	if resolved, present := snapshotRequest(t, fixture, request.ID); present {
		t.Fatalf("resolved request remained in the snapshot: %+v", resolved)
	}

	copy(key[:], adapterID(t, 123))
	unknownRequest, err := fixture.store.CreateHumanQuestionForAttempt(context.Background(), run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "UNKNOWN_PRIVATE_SENTINEL"}, adapterTime(t, 503))
	if err != nil {
		t.Fatal(err)
	}
	unknownDeliveryID, _ := kernel.HumanRequestDeliveryIDFromBytes(adapterID(t, 124))
	unknownDelivery, err := fixture.store.BeginHumanReply(context.Background(), fixture.client.ID, unknownRequest.ID, unknownRequest.Revision, unknownDeliveryID, "reply", adapterTime(t, 504))
	if err != nil {
		t.Fatal(err)
	}
	if err := fixture.store.MarkHumanDeliveryUnknown(context.Background(), unknownRequest.ID, unknownDelivery.DeliveryID, unknownDelivery.Revision, adapterTime(t, 505)); err != nil {
		t.Fatal(err)
	}
	unknownDetail, err := fixture.backend.HumanRequestDetail(context.Background(), rawBrowserClient(fixture.client.ID), browserprotocol.HumanRequestDetailGet{RequestID: unknownRequest.ID.String(), ExpectedRevision: browserprotocol.Decimal(unknownRequest.Revision.Int64() + 2)})
	if err != nil || bool(unknownDetail.CanReply) || unknownDetail.TerminalTarget != nil || unknownDetail.CancelRun != nil {
		t.Fatalf("delivery-unknown detail = %+v, %v", unknownDetail, err)
	}
}

func TestBrowserAdapterSubscriptionReloadsAuthorityAndJoins(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	fixture.pair(t)
	state, err := fixture.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	projectID, _ := kernel.ProjectIDFromBytes(adapterID(t, 130))
	if _, err := fixture.store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "subscription", Root: "/subscription"}, adapterTime(t, 600)); err != nil {
		t.Fatal(err)
	}
	committed, err := fixture.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	subscription, err := fixture.backend.WatchState(context.Background(), rawBrowserClient(fixture.client.ID), decimalSequence(state.Head))
	if err != nil {
		t.Fatal(err)
	}
	update := adapterNextUpdate(t, subscription)
	if update.Head != decimalSequence(committed.Head) {
		t.Fatalf("watch update = %+v, want head %d", update, committed.Head.Int64())
	}
	subscription.Cancel()
	adapterWaitSubscription(t, subscription)
	if err := subscription.Err(); err != nil {
		t.Fatalf("cancelled subscription error = %v", err)
	}
	fixture.backend.subMu.Lock()
	remaining := len(fixture.backend.subs)
	fixture.backend.subMu.Unlock()
	if remaining != 0 {
		t.Fatalf("joined subscription remained registered: %d", remaining)
	}

	// A new subscription cannot keep streaming from an authority cached by the
	// paired WebSocket after durable revocation.
	revoked, err := fixture.store.RevokeBrowserClient(context.Background(), fixture.client.ID, fixture.client.Revision, adapterTime(t, 2_100))
	if err != nil || revoked.RevokedAt == nil {
		t.Fatal(err)
	}
	if _, err := fixture.backend.WatchState(context.Background(), rawBrowserClient(fixture.client.ID), decimalSequence(state.Head)); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("post-revoke subscription = %v", err)
	}
}

// A watcher announces only heads the durable store has reached, so an
// after_head above the current head can never be satisfied. Installing a
// producer for it would leave the client waiting on a notification that cannot
// arrive; it is one finite stale answer instead.
func TestBrowserAdapterWatchRefusesAnAfterHeadAboveTheDurableHead(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	fixture.pair(t)
	state, err := fixture.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	head := decimalSequence(state.Head)
	if _, err := fixture.backend.WatchState(context.Background(), rawBrowserClient(fixture.client.ID), head+1); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("future after_head = %v, want ErrStale", err)
	}
	// Nothing is registered and no producer is started. A store read that
	// fails before registration returns through this same early exit.
	fixture.backend.subMu.Lock()
	remaining := len(fixture.backend.subs)
	fixture.backend.subMu.Unlock()
	if remaining != 0 {
		t.Fatalf("refused watch registered %d subscriptions", remaining)
	}
	// The current head itself is the ordinary case and still subscribes.
	subscription, err := fixture.backend.WatchState(context.Background(), rawBrowserClient(fixture.client.ID), head)
	if err != nil {
		t.Fatalf("current head refused: %v", err)
	}
	subscription.Cancel()
	adapterWaitSubscription(t, subscription)
}

func TestBrowserAdapterRestartUsesNewBootAndDurableClient(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	paired := fixture.pair(t)
	_ = paired.CloseNow()
	oldIdentity, err := fixture.backend.Identity(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if err := fixture.server.Close(); err != nil {
		t.Fatal(err)
	}
	if err := fixture.backend.close(); err != nil {
		t.Fatal(err)
	}
	randomBytes := append(bytes.Repeat([]byte{0x7f}, kernel.IDBytes), bytes.Repeat([]byte{0x7e}, kernel.IDBytes*2)...)
	restarted, err := newBrowserBackend(fixture.store, func() time.Time { return fixture.clock }, bytes.NewReader(randomBytes))
	if err != nil {
		t.Fatal(err)
	}
	server, err := browser.Listen(browser.Config{Address: "127.0.0.1:0", AllowedOrigins: []string{adapterOrigin}, Backend: restarted})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = server.Close()
		_ = restarted.close()
	})
	newIdentity, err := restarted.Identity(context.Background())
	if err != nil || newIdentity.DaemonID != oldIdentity.DaemonID || newIdentity.BootID == oldIdentity.BootID {
		t.Fatalf("restart identity old=%+v new=%+v err=%v", oldIdentity, newIdentity, err)
	}
	connection := adapterDial(t, server)
	hello := adapterRead(t, connection).Body.(browserprotocol.Hello)
	transcript, err := browserprotocol.BuildAuthTranscript(browserprotocol.AuthTranscript{
		DaemonID: adapterHex(t, hello.DaemonID, browserprotocol.DaemonIDSize), BootID: adapterHex(t, hello.BootID, browserprotocol.BootIDSize),
		ConnectionNonce: adapterHex(t, hello.ConnectionNonce, browserprotocol.NonceSize), ClientID: fixture.client.ID.Bytes(),
		ValidatedHost: server.Addr(), ValidatedOrigin: adapterOrigin,
	})
	if err != nil {
		t.Fatal(err)
	}
	proof, _ := browserprotocol.EncodeAuthProve("restart-auth", browserprotocol.AuthProve{ClientID: fixture.client.ID.String(), Signature: hex.EncodeToString(adapterSign(t, fixture.key, transcript))})
	adapterWrite(t, connection, proof)
	if frame := adapterRead(t, connection); frame.Type != browserprotocol.TypeAuthResult {
		t.Fatalf("restart auth = %+v", frame)
	}
}

func TestDaemonCloseJoinsHijackedBrowserConnectionsBeforeStoreClose(t *testing.T) {
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 2}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(2_000) })
	if err != nil {
		t.Fatal(err)
	}
	runtime, err := daemon.ListenBrowser("127.0.0.1:0", []string{adapterOrigin})
	if err != nil {
		t.Fatal(err)
	}
	connection := adapterDial(t, runtime.server)
	if frame := adapterRead(t, connection); frame.Type != browserprotocol.TypeHello {
		t.Fatalf("hello = %+v", frame)
	}
	closed := make(chan error, 1)
	go func() { closed <- daemon.Close() }()
	select {
	case err := <-closed:
		if err != nil {
			t.Fatalf("daemon close = %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("daemon close did not join browser connection")
	}
	select {
	case <-runtime.server.ServeDone():
	default:
		t.Fatal("daemon close returned before browser listener joined")
	}
	if _, err := store.Factory(context.Background()); err != nil {
		t.Fatalf("browser close prematurely closed Store: %v", err)
	}
	if _, err := daemon.ListenBrowser("127.0.0.1:0", []string{adapterOrigin}); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("browser listener after daemon close = %v", err)
	}
}

func TestBrowserCloseLinearizesOpenAndClearsChallenges(t *testing.T) {
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 2}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	enteredClock := make(chan struct{})
	releaseClock := make(chan struct{})
	clock := func() time.Time {
		select {
		case <-enteredClock:
		default:
			close(enteredClock)
		}
		<-releaseClock
		return time.UnixMilli(2_000)
	}
	daemon, err := newDaemon(store, clock)
	if err != nil {
		t.Fatal(err)
	}
	runtime, err := daemon.ListenBrowser("127.0.0.1:0", []string{adapterOrigin})
	if err != nil {
		t.Fatal(err)
	}
	openDone := make(chan struct {
		link string
		err  error
	}, 1)
	go func() {
		link, openErr := daemon.OpenBrowser(context.Background())
		openDone <- struct {
			link string
			err  error
		}{link: link, err: openErr}
	}()
	select {
	case <-enteredClock:
	case <-time.After(3 * time.Second):
		t.Fatal("open did not reach its lifecycle gate")
	}
	closeDone := make(chan error, 1)
	startedClose := make(chan struct{})
	go func() {
		close(startedClose)
		closeDone <- runtime.Close()
	}()
	<-startedClose
	for index := 0; index < 100; index++ {
		daemon.browserMu.Lock()
		closing := daemon.browserClosing
		daemon.browserMu.Unlock()
		if closing {
			t.Fatal("daemon close passed browser lifecycle gate while open was active")
		}
		select {
		case err := <-closeDone:
			t.Fatalf("runtime close completed before open released: %v", err)
		default:
		}
		time.Sleep(time.Millisecond)
	}
	close(releaseClock)
	opened := <-openDone
	if opened.err != nil || !strings.HasPrefix(opened.link, adapterOrigin+"/#df_pair=") {
		t.Fatalf("gated open = %+v, want a launch link", opened)
	}
	if err := <-closeDone; err != nil {
		t.Fatalf("daemon close = %v", err)
	}
	counts, err := store.BrowserClientCounts(context.Background(), runtime.backend.boot, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	if counts.ActiveChallenges != 0 {
		t.Fatalf("active challenges after close = %d, want 0", counts.ActiveChallenges)
	}
	if _, err := daemon.OpenBrowser(context.Background()); !errors.Is(err, kernel.ErrBusy) {
		t.Fatalf("open after close = %v, want busy", err)
	}
}

func TestOpenRejectsMarkedRuntimeBeforeMint(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	ctx := context.Background()
	before, err := fixture.store.BrowserClientCounts(ctx, fixture.backend.boot, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	fixture.daemon.browserMu.Lock()
	fixture.runtime.closing = true
	fixture.daemon.browserMu.Unlock()
	if _, err := fixture.daemon.OpenBrowser(ctx); !errors.Is(err, kernel.ErrBusy) {
		t.Fatalf("open on marked runtime = %v, want busy", err)
	}
	after, err := fixture.store.BrowserClientCounts(ctx, fixture.backend.boot, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	if after.ActiveChallenges != before.ActiveChallenges {
		t.Fatalf("marked-runtime open changed active challenges: before=%d after=%d", before.ActiveChallenges, after.ActiveChallenges)
	}
}

func TestBrowserCloseJoinsSharedCleanupAndRetainsRegistry(t *testing.T) {
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 2}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(2_000) })
	if err != nil {
		t.Fatal(err)
	}
	boot, err := kernel.BootIDFromBytes(bytes.Repeat([]byte{0x5a}, 16))
	if err != nil {
		t.Fatal(err)
	}
	backend := &browserBackend{store: store, boot: boot, subs: make(map[*browserStateWatch]struct{})}
	blocked := &browserStateWatch{backend: backend, done: make(chan struct{}), cancel: func() {}}
	backend.subs[blocked] = struct{}{}
	runtime := &BrowserRuntime{daemon: daemon, backend: backend}
	daemon.browserMu.Lock()
	daemon.browsers[runtime] = struct{}{}
	daemon.browserMu.Unlock()
	release := func() {
		select {
		case <-blocked.done:
		default:
			close(blocked.done)
		}
	}
	defer release()

	directDone := make(chan error, 1)
	go func() { directDone <- runtime.Close() }()
	closingDeadline := time.After(3 * time.Second)
	for {
		daemon.browserMu.Lock()
		closing := runtime.closing
		_, registered := daemon.browsers[runtime]
		daemon.browserMu.Unlock()
		if closing && registered {
			break
		}
		select {
		case <-closingDeadline:
			t.Fatal("runtime did not become marked-closing while cleanup was blocked")
		default:
			time.Sleep(time.Millisecond)
		}
	}
	if _, err := daemon.OpenBrowser(context.Background()); !errors.Is(err, kernel.ErrBusy) {
		t.Fatalf("open while runtime cleanup is blocked = %v, want busy", err)
	}

	daemonDone := make(chan error, 1)
	go func() { daemonDone <- daemon.Close() }()
	select {
	case err := <-daemonDone:
		t.Fatalf("daemon close returned before shared runtime cleanup: %v", err)
	case <-time.After(25 * time.Millisecond):
	}
	release()
	directErr := <-directDone
	daemonErr := <-daemonDone
	if directErr != nil || daemonErr != nil || (directErr == nil) != (daemonErr == nil) {
		t.Fatalf("shared close results: direct=%v daemon=%v", directErr, daemonErr)
	}
	daemon.browserMu.Lock()
	_, registered := daemon.browsers[runtime]
	daemon.browserMu.Unlock()
	if registered {
		t.Fatal("runtime remained registered after joined cleanup")
	}
}

func TestDaemonCloseFirstSharesRuntimeCleanupWithDirectClose(t *testing.T) {
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 2}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(2_000) })
	if err != nil {
		t.Fatal(err)
	}
	boot, err := kernel.BootIDFromBytes(bytes.Repeat([]byte{0x5b}, 16))
	if err != nil {
		t.Fatal(err)
	}
	backend := &browserBackend{store: store, boot: boot, subs: make(map[*browserStateWatch]struct{})}
	blocked := &browserStateWatch{backend: backend, done: make(chan struct{}), cancel: func() {}}
	backend.subs[blocked] = struct{}{}
	runtime := &BrowserRuntime{daemon: daemon, backend: backend}
	daemon.browserMu.Lock()
	daemon.browsers[runtime] = struct{}{}
	daemon.browserMu.Unlock()
	release := func() {
		select {
		case <-blocked.done:
		default:
			close(blocked.done)
		}
	}
	defer release()

	daemonDone := make(chan error, 1)
	go func() { daemonDone <- daemon.Close() }()
	closingDeadline := time.After(3 * time.Second)
	for {
		daemon.browserMu.Lock()
		globalClosing := daemon.browserClosing
		closing := runtime.closing
		_, registered := daemon.browsers[runtime]
		daemon.browserMu.Unlock()
		if globalClosing && closing && registered {
			break
		}
		select {
		case <-closingDeadline:
			t.Fatal("daemon close did not mark and retain runtime")
		default:
			time.Sleep(time.Millisecond)
		}
	}
	directDone := make(chan error, 1)
	go func() { directDone <- runtime.Close() }()
	select {
	case err := <-directDone:
		t.Fatalf("direct close returned before daemon cleanup: %v", err)
	case <-time.After(25 * time.Millisecond):
	}
	release()
	daemonErr := <-daemonDone
	directErr := <-directDone
	if daemonErr != nil || directErr != nil {
		t.Fatalf("daemon-first close results: daemon=%v direct=%v", daemonErr, directErr)
	}
	daemon.browserMu.Lock()
	_, registered := daemon.browsers[runtime]
	daemon.browserMu.Unlock()
	if registered {
		t.Fatal("daemon-first runtime remained registered after cleanup")
	}
}

func TestBrowserCleanupFailureRetainsRuntimeOwnership(t *testing.T) {
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 2}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(2_000) })
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	boot, err := kernel.BootIDFromBytes(bytes.Repeat([]byte{0x5c}, 16))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	backend := &browserBackend{store: store, boot: boot, subs: make(map[*browserStateWatch]struct{})}
	runtime := &BrowserRuntime{daemon: daemon, backend: backend}
	daemon.browserMu.Lock()
	daemon.browsers[runtime] = struct{}{}
	daemon.browserMu.Unlock()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	first := runtime.Close()
	if !errors.Is(first, ErrBrowserRuntimeCleanup) {
		t.Fatalf("first runtime close = %v, want cleanup uncertainty", first)
	}
	second := runtime.Close()
	if second != first {
		t.Fatalf("repeated runtime close changed stable error: first=%v second=%v", first, second)
	}
	daemonErr := daemon.Close()
	if !errors.Is(daemonErr, ErrBrowserRuntimeCleanup) {
		t.Fatalf("daemon close = %v, want cleanup uncertainty", daemonErr)
	}
	if !errors.Is(daemon.Close(), ErrBrowserRuntimeCleanup) {
		t.Fatal("repeated daemon close lost cleanup uncertainty")
	}
	daemon.browserMu.Lock()
	_, registered := daemon.browsers[runtime]
	daemon.browserMu.Unlock()
	if !registered {
		t.Fatal("runtime was unregistered after unresolved cleanup")
	}
	if _, err := daemon.OpenBrowser(context.Background()); !errors.Is(err, kernel.ErrBusy) {
		t.Fatalf("open after unresolved cleanup = %v, want busy", err)
	}
}

func adapterNextUpdate(t *testing.T, subscription browser.StateSubscription) browser.StateUpdate {
	t.Helper()
	select {
	case update, ok := <-subscription.Updates():
		if !ok {
			t.Fatalf("subscription closed early: %v", subscription.Err())
		}
		return update
	case <-time.After(3 * time.Second):
		t.Fatal("subscription update timed out")
		return browser.StateUpdate{}
	}
}

func adapterWaitSubscription(t *testing.T, subscription browser.StateSubscription) {
	t.Helper()
	select {
	case <-subscription.Done():
	case <-time.After(3 * time.Second):
		t.Fatal("subscription did not join")
	}
}

func adapterRunningRun(t *testing.T, store *kernel.Store, seed byte) kernel.Run {
	t.Helper()
	ctx := context.Background()
	projectID, _ := kernel.ProjectIDFromBytes(adapterID(t, seed))
	agentID, _ := kernel.AgentIDFromBytes(adapterID(t, seed+1))
	taskID, _ := kernel.TaskIDFromBytes(adapterID(t, seed+2))
	incarnationID, _ := kernel.IncarnationIDFromBytes(adapterID(t, seed+3))
	project, err := store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: fmt.Sprintf("run-project-%d", seed), Root: fmt.Sprintf("/run-project-%d", seed)}, adapterTime(t, 200))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: project.ID, Name: fmt.Sprintf("run-agent-%d", seed), Role: kernel.RoleOrchestrator, Provider: kernel.ProviderCodex, ToolBudgetLimit: 4}, adapterTime(t, 201))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID, Title: "run task"}, adapterTime(t, 202)); err != nil {
		t.Fatal(err)
	}
	runID, _ := kernel.RunIDFromBytes(adapterID(t, seed+4))
	sessionID, _ := kernel.TerminalSessionIDFromBytes(adapterID(t, seed+5))
	attemptDigest, _ := kernel.AttemptDigestFromBytes(bytes.Repeat([]byte{seed + 6}, kernel.DigestBytes))
	candidateChange, _ := kernel.ChangeIDFromBytes(adapterID(t, seed+11))
	resourceID := func(value byte) kernel.ResourceID {
		id, _ := kernel.ResourceIDFromBytes(adapterID(t, value))
		return id
	}
	keys := kernel.AdmissionKeys{
		RunID: runID, TerminalSessionID: sessionID, AttemptDigest: attemptDigest, CandidateChangeID: candidateChange, RuntimeRoot: fmt.Sprintf("/runtime/adapter-%d", seed),
		Resources: kernel.AdmissionResourceIDs{RuntimeRoot: resourceID(seed + 7), RunnerProcess: resourceID(seed + 8), ProviderProcess: resourceID(seed + 9), ProviderGroup: resourceID(seed + 10)},
	}
	admission, err := store.AdmitNext(ctx, keys, adapterTime(t, 300))
	if err != nil || admission.Run == nil {
		t.Fatalf("admission = %+v, %v", admission, err)
	}
	resources, err := store.Resources(ctx, runID)
	if err != nil {
		t.Fatal(err)
	}
	var provider, group, runnerProcess kernel.Resource
	for _, resource := range resources {
		switch resource.Kind {
		case kernel.ResourceProviderProcess:
			provider = resource
		case kernel.ResourceProviderGroup:
			group = resource
		case kernel.ResourceRunnerProcess:
			runnerProcess = resource
		case kernel.ResourceRuntimeRoot:
			identity, _ := kernel.NewPathResourceIdentity(10, int64(10_000+int(seed)))
			if _, err := store.ActivateResource(ctx, runID, resource.ID, resource.Revision, identity, adapterTime(t, 310)); err != nil {
				t.Fatal(err)
			}
		}
	}
	runnerIdentity := adapterProcessIdentity(t, int64(20_000+int(seed)*100))
	updated := startAndActivateRunner(t, store, runID, runnerProcess.ID, runnerIdentity, adapterTime(t, 315))
	providerIdentity := adapterProcessIdentity(t, int64(30_000+int(seed)))
	if _, _, err := store.ActivateProviderResources(ctx, runID, provider.ID, provider.Revision, group.ID, group.Revision, providerIdentity, adapterTime(t, 320)); err != nil {
		t.Fatal(err)
	}
	session, found, err := store.TerminalSessionForRun(ctx, runID)
	if err != nil || !found {
		t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	running, err := store.ActivateRun(ctx, runID, session.ID, updated.Revision, session.Revision, adapterTime(t, 330))
	if err != nil {
		t.Fatal(err)
	}
	return running
}

func adapterProcessIdentity(t *testing.T, seed int64) kernel.ResourceIdentity {
	t.Helper()
	digest := sha256.Sum256([]byte(fmt.Sprintf("birth-%d", seed)))
	birth, err := kernel.BirthDigestFromBytes(digest[:])
	if err != nil {
		t.Fatal(err)
	}
	identity, err := kernel.NewProcessResourceIdentity(seed, seed, birth)
	if err != nil {
		t.Fatal(err)
	}
	return identity
}

func rawBrowserClient(id kernel.BrowserClientID) [browserprotocol.ClientIDSize]byte {
	var result [browserprotocol.ClientIDSize]byte
	copy(result[:], id.Bytes())
	return result
}
