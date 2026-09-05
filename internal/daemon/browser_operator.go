package daemon

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"math"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const (
	webProductionOrigin = "https://app.darkfactory.build"
	webChallengeTTL     = 5 * time.Minute
	// All four browser effect paths are now reviewed and implemented:
	// observation/private HumanRequest detail, HumanRequest actions, and
	// terminal input. The CLI never selects this mask.
	webCapabilities = kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityPrivateHumanRequestDetail | kernel.BrowserCapabilityHumanActions | kernel.BrowserCapabilityTerminalInput
)

func (daemon *Daemon) webRuntime() (*BrowserRuntime, bool) {
	if daemon == nil {
		return nil, false
	}
	daemon.browserMu.Lock()
	defer daemon.browserMu.Unlock()
	if daemon.browserClosing || len(daemon.browsers) != 1 {
		return nil, false
	}
	for runtime := range daemon.browsers {
		return runtime, runtime != nil && !runtime.closing && runtime.server != nil && runtime.backend != nil
	}
	return nil, false
}

func browserRuntimeReady(runtime *BrowserRuntime) bool {
	if runtime == nil || runtime.server == nil || runtime.backend == nil || runtime.server.Err() != nil {
		return false
	}
	select {
	case <-runtime.server.ServeDone():
		return false
	default:
		return true
	}
}

func (daemon *Daemon) WebStatus(ctx context.Context) (api.WebStatus, error) {
	if daemon == nil || daemon.store == nil {
		return api.WebStatus{}, fmt.Errorf("%w: invalid daemon", kernel.ErrInvalidValue)
	}
	if _, err := daemon.store.Factory(ctx); err != nil {
		return api.WebStatus{}, err
	}
	status := api.WebStatus{State: "stopped"}
	runtime, valid := daemon.webRuntime()
	if !valid {
		return status, nil
	}
	status.Address = runtime.Addr()
	status.Path = browser.Path
	status.Origins = append([]string(nil), runtime.origins...)
	status.Ready = browserRuntimeReady(runtime)
	status.State = "ready"
	if !status.Ready {
		status.State = "degraded"
	}
	at, err := daemon.timestamp()
	if err != nil {
		return api.WebStatus{}, err
	}
	counts, err := daemon.store.BrowserClientCounts(ctx, runtime.backend.boot, at)
	if err != nil {
		return api.WebStatus{}, err
	}
	status.ActiveClients = counts.Active
	status.RevokedClients = counts.Revoked
	status.ActiveChallenges = counts.ActiveChallenges
	return status, nil
}

// OpenBrowser mints the sole browser bootstrap credential and returns the
// one-shot launch link. It is intentionally separate from launching a GUI: the
// caller receives the link only to pass it directly to a browser, never to
// display or persist it. A commit-uncertain mint is a plain failure, as in
// RemotePair: there is no cleanup identity to hand anyone, and an unopened
// challenge simply expires.
func (daemon *Daemon) OpenBrowser(ctx context.Context) (string, error) {
	// Serialize the complete readiness-to-mint operation with daemon browser
	// shutdown. Close marks browserClosing only after any admitted open has
	// finished, then closes the transport and invalidates its challenges.
	daemon.browserLifecycleMu.Lock()
	defer daemon.browserLifecycleMu.Unlock()
	runtime, valid := daemon.webRuntime()
	if !valid || !browserRuntimeReady(runtime) {
		return "", fmt.Errorf("%w: browser transport unavailable", kernel.ErrBusy)
	}
	if !runtimeAllowsProductionOrigin(runtime) {
		return "", fmt.Errorf("%w: production browser origin is not configured", kernel.ErrInvalidValue)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return "", err
	}
	if at.Int64() > math.MaxInt64-int64(webChallengeTTL/time.Millisecond) {
		return "", fmt.Errorf("%w: browser challenge clock exhausted", kernel.ErrInvalidValue)
	}
	expires, err := kernel.NewUnixMillis(at.Int64() + int64(webChallengeTTL/time.Millisecond))
	if err != nil {
		return "", err
	}
	var challenge [browserprotocol.ChallengeSize]byte
	runtime.backend.randomMu.Lock()
	_, readErr := io.ReadFull(runtime.backend.random, challenge[:])
	runtime.backend.randomMu.Unlock()
	if readErr != nil || allZero(challenge[:]) {
		return "", fmt.Errorf("%w: browser challenge generation failed", kernel.ErrBusy)
	}
	digest := kernel.HashBrowserChallenge(challenge[:])
	if _, err := daemon.store.CreateBrowserPairingChallenge(ctx, digest, runtime.backend.boot, webProductionOrigin, webCapabilities, at, expires); err != nil {
		return "", err
	}
	return webProductionOrigin + "/#df_pair=" + hex.EncodeToString(challenge[:]), nil
}

func (daemon *Daemon) WebListClients(ctx context.Context, after string) (api.WebClientPage, error) {
	var cursor *kernel.BrowserClientID
	if after != "" {
		raw, err := parseID(after)
		if err != nil {
			return api.WebClientPage{}, err
		}
		id, err := kernel.BrowserClientIDFromBytes(raw)
		if err != nil {
			return api.WebClientPage{}, err
		}
		cursor = &id
	}
	page, err := daemon.store.ListBrowserClients(ctx, cursor)
	if err != nil {
		return api.WebClientPage{}, err
	}
	result := api.WebClientPage{Clients: make([]api.WebClient, 0, len(page.Items))}
	for _, client := range page.Items {
		item := api.WebClient{ID: client.ID.String(), CapabilityMask: uint8(client.CapabilityMask), Revision: uint64(client.Revision.Int64()), CreatedAtMs: uint64(client.CreatedAt.Int64()), UpdatedAtMs: uint64(client.UpdatedAt.Int64())}
		if client.RevokedAt != nil {
			value := uint64(client.RevokedAt.Int64())
			item.RevokedAtMs = &value
		}
		result.Clients = append(result.Clients, item)
	}
	if page.NextAfter != nil {
		next := page.NextAfter.String()
		result.NextAfter = &next
	}
	return result, nil
}

func (daemon *Daemon) WebRevokeClient(ctx context.Context, input api.WebClientRevocationInput) (api.WebRevokeResult, error) {
	raw, err := parseID(input.ID)
	if err != nil || input.ExpectedRevision > math.MaxInt64 {
		return api.WebRevokeResult{}, kernel.ErrInvalidValue
	}
	id, err := kernel.BrowserClientIDFromBytes(raw)
	if err != nil {
		return api.WebRevokeResult{}, err
	}
	expected, err := kernel.NewRevision(int64(input.ExpectedRevision))
	if err != nil {
		return api.WebRevokeResult{}, err
	}
	client, err := daemon.RevokeBrowserClient(ctx, id, expected)
	if err != nil && !errors.Is(err, ErrBrowserClientCleanup) {
		return api.WebRevokeResult{}, err
	}
	result := api.WebRevokeResult{ID: client.ID.String(), Revision: uint64(client.Revision.Int64())}
	if errors.Is(err, ErrBrowserClientCleanup) {
		return result, err
	}
	return result, nil
}
