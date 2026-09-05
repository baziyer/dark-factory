//go:build darwin || linux

package daemon

import (
	"bytes"
	"context"
	"crypto/elliptic"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"net/url"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/relayhost"
	"rsc.io/qr"
)

// remoteInvitationMembers is the exact fragment contract when the factory runs
// on the production relay and the default browser address. A member added here
// without a client that reads it is dead weight in a QR code, and a member
// removed silently breaks every paired browser, so the set is asserted.
var remoteInvitationMembers = []string{"df_remote", "node", "daemon", "challenge", "ticket", "expires"}

func TestRemotePairingGrantsEveryHumanCapabilityExceptTerminalInput(t *testing.T) {
	want := kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityPrivateHumanRequestDetail | kernel.BrowserCapabilityHumanActions
	if remoteCapabilities != want || remoteCapabilities&kernel.BrowserCapabilityTerminalInput != 0 || remoteCapabilities&^kernel.BrowserCapabilityKnownMask != 0 {
		t.Fatalf("remote capabilities = %#x, want exactly %#x", remoteCapabilities, want)
	}
	// The remote grant is strictly weaker than the local web grant: a relayed
	// controller must never be able to type into a provider PTY.
	if webCapabilities&^kernel.BrowserCapabilityTerminalInput != remoteCapabilities {
		t.Fatalf("remote capabilities are not the web grant minus terminal input: %#x vs %#x", remoteCapabilities, webCapabilities)
	}
}

func TestRemotePairMintsAnExactInvitationBoundToTheNodeAndThisDaemon(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	relay, _, identity := dialRelayFixture(t, fixture)
	ctx := context.Background()

	invitation, err := fixture.daemon.RemotePair(ctx)
	if err != nil {
		t.Fatal(err)
	}
	values := decodeInvitation(t, invitation.Link)

	// The fixture listens on an ephemeral port and a fake relay, so both
	// non-default members are present here; the default-omission cases are
	// proved separately below.
	for _, name := range append(remoteInvitationMembers, "relay", "host") {
		if _, ok := values[name]; !ok {
			t.Fatalf("invitation is missing member %q: %v", name, values)
		}
	}
	if len(values) != len(remoteInvitationMembers)+2 {
		t.Fatalf("invitation members = %v, want exactly %v plus relay and host", values, remoteInvitationMembers)
	}
	// The node public key is the relay's business alone, and a machine label
	// is the phone's to choose: carrying either would only cost QR modules.
	if strings.Contains(invitation.Link, base64.RawURLEncoding.EncodeToString(identity.PublicKey())) {
		t.Fatal("the invitation carries the node public key")
	}

	state, err := fixture.store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if values.Get("relay") != relay.origin() || values.Get("node") != identity.NodeID() ||
		values.Get("daemon") != state.DaemonID.String() || len(values.Get("daemon")) != 32 ||
		values.Get("host") != fixture.server.Addr() {
		t.Fatalf("invitation values = %v", values)
	}
	if values.Get("expires") != strconv.FormatInt(invitation.Expires, 10) || invitation.Expires <= 0 {
		t.Fatalf("invitation expiry = %q, reply says %d", values.Get("expires"), invitation.Expires)
	}

	challenge := invitationChallenge(t, values)

	// The ticket is a pair-purpose credential the relay will check against the
	// node key alone, with the same lifetime as the durable challenge.
	ticket, err := relayhost.VerifyTicket(identity.PublicKey(), values.Get("ticket"))
	if err != nil {
		t.Fatalf("relay ticket does not verify against the node key: %v", err)
	}
	if ticket.Purpose != relayhost.PurposePair || ticket.Device != "" || ticket.Node != identity.NodeID() {
		t.Fatalf("relay ticket = %+v", ticket)
	}
	if controller, err := base64.RawURLEncoding.DecodeString(ticket.Controller); err != nil || len(controller) != relayhost.ControllerIDSize {
		t.Fatalf("relay ticket controller = %q: %v", ticket.Controller, err)
	}
	challengeExpiry := fixture.clock.Add(webChallengeTTL)
	if ticket.Expires != challengeExpiry.Unix() || invitation.Expires != challengeExpiry.Unix() {
		t.Fatalf("ticket expiry = %d, challenge expiry = %d", ticket.Expires, challengeExpiry.Unix())
	}

	// Redeeming the minted challenge is the only proof of what the durable row
	// says: the pairing is admitted at the production origin the transcript
	// binds, and the client it creates carries exactly the remote mask.
	client := pairWithChallenge(t, fixture, challenge)
	if client.CapabilityMask != remoteCapabilities || uint8(client.CapabilityMask) != 7 {
		t.Fatalf("remote client mask = %#x, want %#x", client.CapabilityMask, remoteCapabilities)
	}
	if client.CapabilityMask.Has(kernel.BrowserCapabilityTerminalInput) {
		t.Fatal("a remotely paired client was granted terminal input")
	}
}

func TestRemotePairOmitsTheDefaultRelayAndBrowserAddress(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	_, runtime, _ := dialRelayFixture(t, fixture)
	// The runtime is the only carrier of these two facts, so pretending the
	// factory runs on the production defaults is enough to prove omission
	// without binding a test to port 43123 or dialing the real relay.
	runtime.relayOrigin = relayhost.DefaultRelayOrigin
	runtime.browserAddress = DefaultBrowserAddress

	invitation, err := fixture.daemon.RemotePair(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	values := decodeInvitation(t, invitation.Link)
	if len(values) != len(remoteInvitationMembers) {
		t.Fatalf("default invitation members = %v, want exactly %v", values, remoteInvitationMembers)
	}
	for _, name := range remoteInvitationMembers {
		if _, ok := values[name]; !ok {
			t.Fatalf("default invitation is missing member %q: %v", name, values)
		}
	}
	if strings.Contains(invitation.Link, "relay=") || strings.Contains(invitation.Link, "host=") || strings.Contains(invitation.Link, "label=") {
		t.Fatalf("default invitation transmitted a default: %q", invitation.Link)
	}
}

func TestRemotePairMintsDistinctSecretsEachTime(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	dialRelayFixture(t, fixture)
	ctx := context.Background()

	first, err := fixture.daemon.RemotePair(ctx)
	if err != nil {
		t.Fatal(err)
	}
	second, err := fixture.daemon.RemotePair(ctx)
	if err != nil {
		t.Fatal(err)
	}
	firstValues := decodeInvitation(t, first.Link)
	secondValues := decodeInvitation(t, second.Link)
	if firstValues.Get("challenge") == secondValues.Get("challenge") {
		t.Fatal("two invitations reused one pairing challenge")
	}
	if firstValues.Get("ticket") == secondValues.Get("ticket") {
		t.Fatal("two invitations reused one relay ticket")
	}
	firstTicket, err := relayhost.VerifyTicket(fixtureNodeKey(t, fixture), firstValues.Get("ticket"))
	if err != nil {
		t.Fatal(err)
	}
	secondTicket, err := relayhost.VerifyTicket(fixtureNodeKey(t, fixture), secondValues.Get("ticket"))
	if err != nil {
		t.Fatal(err)
	}
	if firstTicket.Controller == secondTicket.Controller || firstTicket.Ticket == secondTicket.Ticket {
		t.Fatalf("two invitations reused one controller identity: %+v, %+v", firstTicket, secondTicket)
	}
	// Both remain live and distinct, so neither invitation invalidated the
	// other: each is one independent pairing opportunity.
	status, err := fixture.daemon.WebStatus(ctx)
	if err != nil || status.ActiveChallenges != 3 {
		t.Fatalf("active challenges = %+v, %v", status, err)
	}
}

func TestRemoteOperationsRefuseADisabledRelayWithoutMintingAnything(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	ctx := context.Background()
	before, err := fixture.daemon.WebStatus(ctx)
	if err != nil {
		t.Fatal(err)
	}

	invitation, pairErr := fixture.daemon.RemotePair(ctx)
	if !errors.Is(pairErr, kernel.ErrNotFound) || invitation != (RemoteInvitation{}) {
		t.Fatalf("remote pair without a relay = %+v, %v", invitation, pairErr)
	}
	if code := remoteErrorCode(pairErr); code != api.RemoteNotFound {
		t.Fatalf("remote pair error code = %s", code)
	}
	status, statusErr := fixture.daemon.RemoteStatus()
	if !errors.Is(statusErr, kernel.ErrNotFound) || status != (api.RemoteStatus{}) {
		t.Fatalf("remote status without a relay = %+v, %v", status, statusErr)
	}

	after, err := fixture.daemon.WebStatus(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if after.ActiveChallenges != before.ActiveChallenges {
		t.Fatalf("a refused invitation minted a challenge: %d -> %d", before.ActiveChallenges, after.ActiveChallenges)
	}
}

func TestRemoteStatusProjectsTheLiveConnector(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	relay, _, identity := dialRelayFixture(t, fixture)
	relay.accept(t)

	deadline := time.Now().Add(relayTestDeadline)
	var status api.RemoteStatus
	for {
		var err error
		status, err = fixture.daemon.RemoteStatus()
		if err != nil {
			t.Fatal(err)
		}
		if status.Connected || time.Now().After(deadline) {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	if !status.Connected || status.NodeID != identity.NodeID() || status.RelayOrigin != relay.origin() || status.Sessions != 0 {
		t.Fatalf("remote status = %+v", status)
	}
}

func fixtureNodeKey(t *testing.T, fixture *adapterFixture) []byte {
	t.Helper()
	fixture.daemon.browserMu.Lock()
	runtime := fixture.daemon.relay
	fixture.daemon.browserMu.Unlock()
	if runtime == nil {
		t.Fatal("no relay runtime registered")
	}
	return runtime.identity.PublicKey()
}

func decodeInvitation(t *testing.T, link string) url.Values {
	t.Helper()
	base, fragment, found := strings.Cut(link, "#")
	if !found || base != "https://app.darkfactory.build/remote" {
		t.Fatalf("invitation link = %q", link)
	}
	if !strings.HasPrefix(fragment, "df_remote&") {
		t.Fatalf("invitation fragment = %q", fragment)
	}
	values, err := url.ParseQuery(fragment)
	if err != nil {
		t.Fatalf("invitation fragment does not parse: %v", err)
	}
	return values
}

func invitationChallenge(t *testing.T, values url.Values) []byte {
	t.Helper()
	encoded := values.Get("challenge")
	if len(encoded) != 64 || encoded != strings.ToLower(encoded) {
		t.Fatalf("invitation challenge = %q", encoded)
	}
	challenge, err := hex.DecodeString(encoded)
	if err != nil || len(challenge) != browserprotocol.ChallengeSize {
		t.Fatalf("invitation challenge = %q: %v", encoded, err)
	}
	return challenge
}

// pairWithChallenge redeems one specific pairing challenge over the fixture's
// own loopback transport with a fresh device key, and returns the durable
// client the daemon created for it.
func pairWithChallenge(t *testing.T, fixture *adapterFixture, challenge []byte) kernel.BrowserClient {
	t.Helper()
	frame, publicKey := provePairWithChallenge(t, fixture, challenge)
	if frame.Type != browserprotocol.TypePairResult || frame.ID != "pair" {
		t.Fatalf("pair result = %+v", frame)
	}
	result := frame.Body.(browserprotocol.PairResult)
	id, err := kernel.BrowserClientIDFromBytes(adapterHex(t, result.ClientID, browserprotocol.ClientIDSize))
	if err != nil {
		t.Fatal(err)
	}
	client, found, err := fixture.store.BrowserClient(context.Background(), id)
	if err != nil || !found {
		t.Fatalf("durable client = %+v found=%v err=%v", client, found, err)
	}
	if !bytes.Equal(client.PublicKey, publicKey) {
		t.Fatal("the durable client does not carry the paired device key")
	}
	return client
}

// provePairWithChallenge redeems one specific pairing challenge over the
// fixture's own loopback transport with a fresh device key, and returns the
// frame the daemon answered with.
func provePairWithChallenge(t *testing.T, fixture *adapterFixture, challenge []byte) (browserprotocol.ControlFrame, []byte) {
	t.Helper()
	key := relayKey(t)
	connection := adapterDial(t, fixture.server)
	hello := adapterRead(t, connection).Body.(browserprotocol.Hello)
	publicKey := elliptic.Marshal(elliptic.P256(), key.PublicKey.X, key.PublicKey.Y)
	transcript, err := browserprotocol.BuildPairTranscript(browserprotocol.PairTranscript{
		DaemonID: adapterHex(t, hello.DaemonID, browserprotocol.DaemonIDSize), BootID: adapterHex(t, hello.BootID, browserprotocol.BootIDSize),
		ConnectionNonce: adapterHex(t, hello.ConnectionNonce, browserprotocol.NonceSize), Challenge: challenge, PublicKeySEC1: publicKey,
		ValidatedHost: fixture.server.Addr(), ValidatedOrigin: adapterOrigin,
	})
	if err != nil {
		t.Fatal(err)
	}
	proof, err := browserprotocol.EncodePairProve("pair", browserprotocol.PairProve{
		Challenge: hex.EncodeToString(challenge), PublicKeySEC1: hex.EncodeToString(publicKey), Signature: hex.EncodeToString(adapterSign(t, key, transcript)),
	})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, proof)
	return adapterRead(t, connection), publicKey
}

func TestBrowserRemoteInviteMintsTheSameInvitationWithAScannableCode(t *testing.T) {
	fixture := newAdapterFixture(t, webCapabilities)
	dialRelayFixture(t, fixture)
	fixture.pair(t)
	ctx := context.Background()

	result, err := fixture.backend.RemoteInvite(ctx, rawBrowserClient(fixture.client.ID))
	if err != nil {
		t.Fatal(err)
	}
	values := decodeInvitation(t, result.Link)
	for _, name := range remoteInvitationMembers {
		if _, ok := values[name]; !ok {
			t.Fatalf("invitation is missing member %q: %v", name, values)
		}
	}
	invitationChallenge(t, values)
	expires, err := strconv.ParseInt(values.Get("expires"), 10, 64)
	if err != nil || expires <= 0 {
		t.Fatalf("invitation expiry = %q: %v", values.Get("expires"), err)
	}
	if result.ExpiresAtMS != browserprotocol.Decimal(expires*1000) {
		t.Fatalf("expires_at_ms = %d, link says %d seconds", result.ExpiresAtMS, expires)
	}
	if !strings.HasPrefix(result.SVG, "<svg") || !strings.Contains(result.SVG, `<path fill="#000" d="M`) {
		t.Fatalf("invitation code = %.80s", result.SVG)
	}
}

func TestBrowserRemoteInviteRefusesEveryGrantWeakerThanLoopback(t *testing.T) {
	// The last of these is the remote grant itself: it carries human_actions
	// but never terminal_input, so a paired phone cannot propagate its own
	// pairing to another phone.
	for _, capabilities := range []kernel.BrowserCapabilityMask{
		kernel.BrowserCapabilityObserve,
		kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityPrivateHumanRequestDetail,
		remoteCapabilities,
	} {
		t.Run(fmt.Sprintf("mask %#x", uint8(capabilities)), func(t *testing.T) {
			fixture := newAdapterFixture(t, capabilities)
			dialRelayFixture(t, fixture)
			fixture.pair(t)
			ctx := context.Background()
			before, err := fixture.daemon.WebStatus(ctx)
			if err != nil {
				t.Fatal(err)
			}

			result, inviteErr := fixture.backend.RemoteInvite(ctx, rawBrowserClient(fixture.client.ID))
			if !errors.Is(inviteErr, browser.ErrUnauthorized) || result != (browserprotocol.RemoteInviteResult{}) {
				t.Fatalf("invite = %+v, %v", result, inviteErr)
			}

			after, err := fixture.daemon.WebStatus(ctx)
			if err != nil {
				t.Fatal(err)
			}
			if after.ActiveChallenges != before.ActiveChallenges {
				t.Fatalf("a refused invite minted a challenge: %d -> %d", before.ActiveChallenges, after.ActiveChallenges)
			}
		})
	}
}

func TestQRSVGStaysInsideTheWireBoundForALongInvitation(t *testing.T) {
	link := "https://app.darkfactory.build/remote#df_remote&" + strings.Repeat("a", 600-46)
	code, err := qr.Encode(link, qr.L)
	if err != nil {
		t.Fatal(err)
	}
	rendered, err := qrSVG(link)
	if err != nil {
		t.Fatal(err)
	}
	if len(rendered) > browserprotocol.MaxRemoteInviteSVGBytes {
		t.Fatalf("a %d-byte link rendered %d bytes, over the %d bound", len(link), len(rendered), browserprotocol.MaxRemoteInviteSVGBytes)
	}
	want := fmt.Sprintf(`viewBox="0 0 %d %d"`, code.Size+2*qrQuietModules, code.Size+2*qrQuietModules)
	if !strings.Contains(rendered, want) {
		t.Fatalf("rendered code does not carry %s: %.120s", want, rendered)
	}
}

func TestRevokingAClientInvalidatesEveryLivePairingChallenge(t *testing.T) {
	fixture := newAdapterFixture(t, webCapabilities)
	dialRelayFixture(t, fixture)
	fixture.pair(t)
	ctx := context.Background()

	invitation, err := fixture.daemon.RemotePair(ctx)
	if err != nil {
		t.Fatal(err)
	}
	challenge := invitationChallenge(t, decodeInvitation(t, invitation.Link))
	before, err := fixture.daemon.WebStatus(ctx)
	if err != nil || before.ActiveChallenges != 1 {
		t.Fatalf("active challenges before revocation = %+v, %v", before, err)
	}

	if _, err := fixture.daemon.WebRevokeClient(ctx, api.WebClientRevocationInput{ID: fixture.client.ID.String(), ExpectedRevision: uint64(fixture.client.Revision.Int64())}); err != nil {
		t.Fatal(err)
	}

	after, err := fixture.daemon.WebStatus(ctx)
	if err != nil || after.ActiveChallenges != 0 {
		t.Fatalf("active challenges after revocation = %+v, %v", after, err)
	}
	// The link was already handed out; only the durable row stands between it
	// and a fresh client carrying the grant that was just revoked.
	frame, _ := provePairWithChallenge(t, fixture, challenge)
	if frame.Type != browserprotocol.TypeError {
		t.Fatalf("a challenge minted before revocation still paired: %+v", frame)
	}
}

func TestBrowserRemoteInviteHoldsAtMostFourOfTheLiveChallengeSlots(t *testing.T) {
	fixture := newAdapterFixture(t, webCapabilities)
	dialRelayFixture(t, fixture)
	fixture.pair(t)
	ctx := context.Background()
	// The bound is read off the backend's own clock, so the test moves that
	// clock rather than waiting five real minutes.
	var elapsed atomic.Int64
	base := fixture.clock
	fixture.backend.now = func() time.Time { return base.Add(time.Duration(elapsed.Load())) }
	client := rawBrowserClient(fixture.client.ID)

	for attempt := 0; attempt < 4; attempt++ {
		if _, err := fixture.backend.RemoteInvite(ctx, client); err != nil {
			t.Fatalf("invite %d = %v", attempt, err)
		}
	}
	before, err := fixture.daemon.WebStatus(ctx)
	if err != nil || before.ActiveChallenges != 4 {
		t.Fatalf("active challenges after four invites = %+v, %v", before, err)
	}

	// A console looping REMOTE_INVITE cannot take a fifth of the 32 slots, so
	// the loopback /pair page can always still mint one.
	result, inviteErr := fixture.backend.RemoteInvite(ctx, client)
	if !errors.Is(inviteErr, browser.ErrRateLimited) || result != (browserprotocol.RemoteInviteResult{}) {
		t.Fatalf("fifth invite = %+v, %v", result, inviteErr)
	}
	after, err := fixture.daemon.WebStatus(ctx)
	if err != nil || after.ActiveChallenges != before.ActiveChallenges {
		t.Fatalf("a refused invite minted a challenge: %d -> %+v, %v", before.ActiveChallenges, after, err)
	}

	// Once the oldest of the four has outlived the challenge TTL its slot is
	// free again: the bound is a rate, not a lifetime quota.
	elapsed.Store(int64(webChallengeTTL))
	if _, err := fixture.backend.RemoteInvite(ctx, client); err != nil {
		t.Fatalf("invite after the TTL = %v", err)
	}
}
