package daemon

import (
	"context"
	"encoding/hex"
	"fmt"
	"io"
	"math"
	"strconv"
	"strings"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/relayhost"
	"rsc.io/qr"
)

const (
	// remoteCapabilities is the remote grant. It is deliberately the web mask
	// without terminal input: a remote controller observes, reads private
	// HumanRequest detail and answers it, and never types into a provider PTY.
	remoteCapabilities = kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityPrivateHumanRequestDetail | kernel.BrowserCapabilityHumanActions
	// remoteLinkFragment opens the PWA route that consumes an invitation. The
	// members that follow are plain query pairs so the browser can read them
	// with URLSearchParams and this side with url.ParseQuery: every byte of
	// JSON or base64 wrapper is a byte of QR code nobody can scan.
	remoteLinkFragment = "/remote#df_remote"
	// qrQuietModules is the standard four-module margin. It is drawn rather
	// than assumed: the edge of an image element is not a quiet zone.
	qrQuietModules = 4
)

// RemoteInvitation is one minted remote pairing invitation. Link carries the
// pairing challenge and the relay ticket in its fragment, so it is a secret:
// the daemon returns it only to the caller that minted it and never logs it.
type RemoteInvitation struct {
	Link    string
	Expires int64
}

// RemotePair mints one remote pairing invitation: a browser pairing challenge
// bound to the production origin, plus a single-use relay ticket signed by the
// node key. The challenge and the ticket are secrets that exist only in this
// reply; the daemon never logs them and keeps no copy it could reveal later.
func (daemon *Daemon) RemotePair(ctx context.Context) (RemoteInvitation, error) {
	// The same gate OpenBrowser uses. A challenge must not be minted against a
	// transport that browser shutdown has already begun to invalidate.
	daemon.browserLifecycleMu.Lock()
	defer daemon.browserLifecycleMu.Unlock()
	runtime, valid := daemon.webRuntime()
	if !valid || !browserRuntimeReady(runtime) {
		return RemoteInvitation{}, fmt.Errorf("%w: browser transport unavailable", kernel.ErrBusy)
	}
	relay, err := daemon.relayRuntime()
	if err != nil {
		return RemoteInvitation{}, err
	}
	if !runtimeAllowsProductionOrigin(runtime) {
		return RemoteInvitation{}, fmt.Errorf("%w: production browser origin is not configured", kernel.ErrInvalidValue)
	}
	state, err := daemon.store.Factory(ctx)
	if err != nil {
		return RemoteInvitation{}, err
	}
	at, err := daemon.timestamp()
	if err != nil {
		return RemoteInvitation{}, err
	}
	if at.Int64() > math.MaxInt64-int64(webChallengeTTL/time.Millisecond) {
		return RemoteInvitation{}, fmt.Errorf("%w: browser challenge clock exhausted", kernel.ErrInvalidValue)
	}
	expires, err := kernel.NewUnixMillis(at.Int64() + int64(webChallengeTTL/time.Millisecond))
	if err != nil {
		return RemoteInvitation{}, err
	}
	var challenge [browserprotocol.ChallengeSize]byte
	var controller [relayhost.ControllerIDSize]byte
	runtime.backend.randomMu.Lock()
	_, challengeErr := io.ReadFull(runtime.backend.random, challenge[:])
	_, controllerErr := io.ReadFull(runtime.backend.random, controller[:])
	runtime.backend.randomMu.Unlock()
	if challengeErr != nil || controllerErr != nil || allZero(challenge[:]) || allZero(controller[:]) {
		return RemoteInvitation{}, fmt.Errorf("%w: remote pairing secret generation failed", kernel.ErrBusy)
	}
	expiry := time.UnixMilli(expires.Int64())
	ticket := relayhost.PairTicket(relay.identity, controller, expiry)
	if ticket == "" {
		return RemoteInvitation{}, fmt.Errorf("%w: relay pairing ticket could not be minted", kernel.ErrBusy)
	}
	digest := kernel.HashBrowserChallenge(challenge[:])
	// A commit-uncertain mint is a plain failure here. There is no exact
	// cleanup identity to hand back, and an unopened challenge simply expires.
	if _, err := daemon.store.CreateBrowserPairingChallenge(ctx, digest, runtime.backend.boot, webProductionOrigin, remoteCapabilities, at, expires); err != nil {
		return RemoteInvitation{}, err
	}
	// Every value is already URL-safe, so the fragment is built literally
	// rather than through Values.Encode, which would escape the relay origin's
	// own separators for nothing. The node public key is deliberately absent:
	// only the relay checks it, and the daemon id is the fingerprint a browser
	// pins. A default relay origin or browser address is omitted rather than
	// transmitted, because every byte here is a QR module the operator has to
	// scan.
	link := webProductionOrigin + remoteLinkFragment +
		"&node=" + relay.identity.NodeID() +
		"&daemon=" + state.DaemonID.String() +
		"&challenge=" + hex.EncodeToString(challenge[:]) +
		"&ticket=" + ticket +
		"&expires=" + strconv.FormatInt(expiry.Unix(), 10)
	if relay.relayOrigin != relayhost.DefaultRelayOrigin {
		link += "&relay=" + relay.relayOrigin
	}
	if relay.browserAddress != DefaultBrowserAddress {
		link += "&host=" + relay.browserAddress
	}
	return RemoteInvitation{Link: link, Expires: expiry.Unix()}, nil
}

// qrSVG draws one QR code as an SVG: a light square with one path segment per
// horizontal run of dark modules, offset by the quiet zone. Runs rather than
// per-module rectangles keep a real invitation inside the wire bound, and the
// symbol scales to whatever box the browser gives it.
func qrSVG(text string) (string, error) {
	code, err := qr.Encode(text, qr.L)
	if err != nil {
		return "", err
	}
	if code == nil || code.Size <= 0 {
		return "", fmt.Errorf("%w: the invitation produced no QR code", kernel.ErrInvalidValue)
	}
	span := code.Size + 2*qrQuietModules
	var builder strings.Builder
	fmt.Fprintf(&builder, `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 %d %d" shape-rendering="crispEdges"><rect width="%d" height="%d" fill="#fff"/><path fill="#000" d="`, span, span, span, span)
	for row := 0; row < code.Size; row++ {
		for column := 0; column < code.Size; {
			if !code.Black(column, row) {
				column++
				continue
			}
			start := column
			for column < code.Size && code.Black(column, row) {
				column++
			}
			fmt.Fprintf(&builder, "M%d %dh%dv1h-%dz", start+qrQuietModules, row+qrQuietModules, column-start, column-start)
		}
	}
	builder.WriteString(`"/></svg>`)
	if builder.Len() > browserprotocol.MaxRemoteInviteSVGBytes {
		return "", fmt.Errorf("%w: the invitation code exceeds the wire bound", kernel.ErrInvalidValue)
	}
	return builder.String(), nil
}

// RemoteStatus is the bounded operator view of the relay connector.
func (daemon *Daemon) RemoteStatus() (api.RemoteStatus, error) {
	relay, err := daemon.relayRuntime()
	if err != nil {
		return api.RemoteStatus{}, err
	}
	status := relay.Status()
	return api.RemoteStatus{
		NodeID:      status.NodeID,
		RelayOrigin: relay.relayOrigin,
		Connected:   status.Connected,
		Sessions:    status.Sessions,
	}, nil
}

// relayRuntime returns the registered relay connector. A factory started
// without a relay origin has no remote surface at all, which is a refusal
// rather than an empty projection: there is no node to invite anyone to.
func (daemon *Daemon) relayRuntime() (*RelayRuntime, error) {
	if daemon == nil || daemon.store == nil {
		return nil, fmt.Errorf("%w: invalid daemon", kernel.ErrInvalidValue)
	}
	daemon.browserMu.Lock()
	relay := daemon.relay
	daemon.browserMu.Unlock()
	if relay == nil || relay.connector == nil || relay.identity.NodeID() == "" || relay.relayOrigin == "" || relay.browserAddress == "" {
		return nil, fmt.Errorf("%w: the relay is not enabled", kernel.ErrNotFound)
	}
	return relay, nil
}

func runtimeAllowsProductionOrigin(runtime *BrowserRuntime) bool {
	for _, origin := range runtime.origins {
		if origin == webProductionOrigin {
			return true
		}
	}
	return false
}
