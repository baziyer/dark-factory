//go:build darwin || linux

package daemon

import (
	"context"
	"encoding/hex"
	"errors"
	"net/http"
	"net/url"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func TestProductionBrowserPairingUsesExactlyImplementedCapabilities(t *testing.T) {
	want := kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityPrivateHumanRequestDetail | kernel.BrowserCapabilityHumanActions | kernel.BrowserCapabilityTerminalInput
	if webCapabilities != want || webCapabilities&^kernel.BrowserCapabilityKnownMask != 0 {
		t.Fatalf("production browser capabilities = %#x, want exactly %#x", webCapabilities, want)
	}
}

func TestWebOperatorOpenStatusListAndRevoke(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	ctx := context.Background()
	status, err := fixture.daemon.WebStatus(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if status.State != "ready" || !status.Ready || status.Address == "" || status.Path != "/browser" || len(status.Origins) != 1 || status.Origins[0] != adapterOrigin || status.ActiveClients != 0 || status.RevokedClients != 0 || status.ActiveChallenges != 1 {
		t.Fatalf("initial web status = %+v", status)
	}
	link, err := fixture.daemon.OpenBrowser(ctx)
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := url.Parse(link)
	if err != nil || parsed.Scheme != "https" || parsed.Host != "app.darkfactory.build" || parsed.Path != "/" || parsed.RawQuery != "" || !strings.HasPrefix(parsed.Fragment, "df_pair=") {
		t.Fatalf("launch URL = %q, err=%v", link, err)
	}
	// The mint is the pair page's only credential source, so a backend with no
	// owner must produce nothing at all.
	if _, err := (&browserBackend{}).PairLink(ctx); !errors.Is(err, browser.ErrUnauthorized) {
		t.Fatalf("ownerless pair link = %v", err)
	}
	rawChallenge, err := hex.DecodeString(strings.TrimPrefix(parsed.Fragment, "df_pair="))
	if err != nil || len(rawChallenge) != 32 {
		t.Fatalf("launch challenge = %q, err=%v", parsed.Fragment, err)
	}
	status, err = fixture.daemon.WebStatus(ctx)
	if err != nil || status.ActiveChallenges != 2 {
		t.Fatalf("post-open status = %+v, %v", status, err)
	}
	page, err := fixture.daemon.WebListClients(ctx, "")
	if err != nil || len(page.Clients) != 0 || page.NextAfter != nil {
		t.Fatalf("empty client page = %+v, %v", page, err)
	}

	fixture.pair(t)
	page, err = fixture.daemon.WebListClients(ctx, "")
	if err != nil || len(page.Clients) != 1 || page.Clients[0].ID != fixture.client.ID.String() || page.Clients[0].CapabilityMask != uint8(fixture.client.CapabilityMask) || page.Clients[0].Revision != uint64(fixture.client.Revision.Int64()) {
		t.Fatalf("paired client page = %+v, %v", page, err)
	}
	if page.Clients[0].RevokedAtMs != nil {
		t.Fatal("fresh client listed as revoked")
	}
	if _, err := fixture.daemon.WebRevokeClient(ctx, api.WebClientRevocationInput{ID: fixture.client.ID.String(), ExpectedRevision: uint64(fixture.client.Revision.Int64())}); err != nil {
		t.Fatal(err)
	}
	page, err = fixture.daemon.WebListClients(ctx, "")
	if err != nil || len(page.Clients) != 1 || page.Clients[0].RevokedAtMs == nil || page.Clients[0].Revision != uint64(fixture.client.Revision.Int64()+1) {
		t.Fatalf("revoked client page = %+v, %v", page, err)
	}
}

// TestPairPageMintsTheSameChallengeAsTheOwnerMint drives the production listener
// the way a browser does: the page's own same-origin form post answers with a
// redirect carrying a fresh challenge, and the daemon counts it exactly once.
func TestPairPageMintsTheSameChallengeAsTheOwnerMint(t *testing.T) {
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityPrivateHumanRequestDetail)
	ctx := context.Background()
	status, err := fixture.daemon.WebStatus(ctx)
	if err != nil || status.ActiveChallenges != 1 {
		t.Fatalf("initial status = %+v, %v", status, err)
	}
	client := &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	request, err := http.NewRequest(http.MethodPost, "http://"+status.Address+"/pair", strings.NewReader(""))
	if err != nil {
		t.Fatal(err)
	}
	for key, value := range map[string]string{"Sec-Fetch-Mode": "navigate", "Sec-Fetch-Dest": "document", "Sec-Fetch-Site": "same-origin"} {
		request.Header.Set(key, value)
	}
	response, err := client.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	location := response.Header.Get("Location")
	parsed, parseErr := url.Parse(location)
	if response.StatusCode != http.StatusSeeOther || parseErr != nil || parsed.Scheme != "https" || parsed.Host != "app.darkfactory.build" || parsed.Path != "/" || !strings.HasPrefix(parsed.Fragment, "df_pair=") {
		t.Fatalf("pair post status=%d location=%q err=%v", response.StatusCode, location, parseErr)
	}
	if raw, err := hex.DecodeString(strings.TrimPrefix(parsed.Fragment, "df_pair=")); err != nil || len(raw) != 32 {
		t.Fatalf("pair challenge = %q, err=%v", parsed.Fragment, err)
	}
	status, err = fixture.daemon.WebStatus(ctx)
	if err != nil || status.ActiveChallenges != 2 {
		t.Fatalf("post-pair status = %+v, %v", status, err)
	}
}
