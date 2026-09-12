package daemon

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// devicePushKeys is what a phone hands over: the raw public point its
// subscription was created with and the PKCS#8 private key behind it.
func devicePushKeys(t *testing.T) (*ecdsa.PrivateKey, string, string) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	der, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	return key, base64.RawURLEncoding.EncodeToString(elliptic.Marshal(elliptic.P256(), key.X, key.Y)), base64.RawURLEncoding.EncodeToString(der)
}

func TestSendPushPresentsAVerifiableVAPIDHeaderAndNoBody(t *testing.T) {
	key, public, private := devicePushKeys(t)
	var got http.Header
	var body []byte
	var status atomic.Int32
	status.Store(http.StatusCreated)
	service := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Clone()
		body, _ = io.ReadAll(r.Body)
		w.WriteHeader(int(status.Load()))
	}))
	defer service.Close()
	subscription := browserprotocol.PushSubscribe{Endpoint: service.URL + "/send/abc", PublicKey: public, PrivateKey: private}
	now := time.Unix(1_700_000_000, 0)

	gone, err := sendPush(context.Background(), service.Client(), subscription, now)
	if err != nil || gone {
		t.Fatalf("send = gone %v, %v", gone, err)
	}
	if len(body) != 0 || got.Get("TTL") != "86400" || got.Get("Urgency") != "high" || got.Get("Topic") != "needs-you" {
		t.Fatalf("push request headers %v body %q", got, body)
	}
	authorization := got.Get("Authorization")
	if !strings.HasPrefix(authorization, "vapid t=") || !strings.HasSuffix(authorization, ", k="+public) {
		t.Fatalf("authorization = %q", authorization)
	}
	token := strings.TrimSuffix(strings.TrimPrefix(authorization, "vapid t="), ", k="+public)
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		t.Fatalf("jwt = %q", token)
	}
	claimsJSON, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	var claims struct {
		Audience string `json:"aud"`
		Expires  int64  `json:"exp"`
		Subject  string `json:"sub"`
	}
	if err := json.Unmarshal(claimsJSON, &claims); err != nil {
		t.Fatal(err)
	}
	if claims.Audience != service.URL || claims.Expires != now.Add(12*time.Hour).Unix() || claims.Subject != "https://darkfactory.build" {
		t.Fatalf("claims = %+v", claims)
	}
	signature, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil || len(signature) != 64 {
		t.Fatalf("signature %d bytes, %v", len(signature), err)
	}
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	if !ecdsa.Verify(&key.PublicKey, digest[:], new(big.Int).SetBytes(signature[:32]), new(big.Int).SetBytes(signature[32:])) {
		t.Fatal("VAPID signature does not verify against the device's public key")
	}

	// The service saying the subscription is gone is the one answer that
	// changes what the factory keeps; a passing failure is not.
	status.Store(http.StatusGone)
	if gone, err := sendPush(context.Background(), service.Client(), subscription, now); err != nil || !gone {
		t.Fatalf("410: gone %v, %v", gone, err)
	}
	status.Store(http.StatusBadGateway)
	if gone, err := sendPush(context.Background(), service.Client(), subscription, now); err == nil || gone {
		t.Fatalf("502: gone %v, %v", gone, err)
	}
}

func TestParsePushKeysRefusesAMismatchedPair(t *testing.T) {
	_, public, _ := devicePushKeys(t)
	_, _, otherPrivate := devicePushKeys(t)
	if _, err := parsePushKeys(browserprotocol.PushSubscribe{Endpoint: "https://push.example/a", PublicKey: public, PrivateKey: otherPrivate}); err == nil {
		t.Fatal("a private key that is not behind the public key was accepted")
	}
	if _, err := parsePushKeys(browserprotocol.PushSubscribe{Endpoint: "https://push.example/a", PublicKey: public, PrivateKey: "AQ"}); err == nil {
		t.Fatal("garbage was accepted as a key")
	}
}

func TestNotifyPushWakesLiveClientsAndForgetsTheRest(t *testing.T) {
	store, err := createTestStore(context.Background(), filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 2}, adapterTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(5_000) })
	if err != nil {
		t.Fatal(err)
	}
	boot, err := kernel.BootIDFromBytes(bytes.Repeat([]byte{0x5a}, 16))
	if err != nil {
		t.Fatal(err)
	}
	pair := func(seed byte) kernel.BrowserClient {
		challenge := bytes.Repeat([]byte{seed}, browserprotocol.ChallengeSize)
		if _, err := store.CreateBrowserPairingChallenge(context.Background(), kernel.HashBrowserChallenge(challenge), boot, adapterOrigin, kernel.BrowserCapabilityObserve, adapterTime(t, 2_000), adapterTime(t, 3_000)); err != nil {
			t.Fatal(err)
		}
		key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
		if err != nil {
			t.Fatal(err)
		}
		clientID, err := kernel.BrowserClientIDFromBytes(bytes.Repeat([]byte{seed}, kernel.IDBytes))
		if err != nil {
			t.Fatal(err)
		}
		client, err := store.RedeemBrowserPairingChallenge(context.Background(), kernel.HashBrowserChallenge(challenge), boot, adapterOrigin, clientID, elliptic.Marshal(elliptic.P256(), key.X, key.Y), adapterTime(t, 2_000))
		if err != nil {
			t.Fatal(err)
		}
		return client
	}
	live := pair(0x11)
	revoked := pair(0x22)
	dead := pair(0x33)
	if _, err := store.RevokeBrowserClient(context.Background(), revoked.ID, revoked.Revision, adapterTime(t, 4_000)); err != nil {
		t.Fatal(err)
	}

	var hits atomic.Int32
	service := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		if strings.HasSuffix(r.URL.Path, "/dead") {
			w.WriteHeader(http.StatusGone)
			return
		}
		w.WriteHeader(http.StatusCreated)
	}))
	defer service.Close()
	_, public, private := devicePushKeys(t)
	subscribe := func(id string, path string) browserprotocol.PushSubscribe {
		return browserprotocol.PushSubscribe{Endpoint: service.URL + path, PublicKey: public, PrivateKey: private}
	}
	daemon.push = newPushStore(t.TempDir())
	_, deadPublic, deadPrivate := devicePushKeys(t)
	if err := daemon.push.update(func(subscriptions map[string]browserprotocol.PushSubscribe) {
		subscriptions[live.ID.String()] = subscribe(live.ID.String(), "/live")
		subscriptions[revoked.ID.String()] = subscribe(revoked.ID.String(), "/revoked")
		subscriptions["not-a-client"] = subscribe("", "/stranger")
		subscriptions[dead.ID.String()] = browserprotocol.PushSubscribe{Endpoint: service.URL + "/dead", PublicKey: deadPublic, PrivateKey: deadPrivate}
	}); err != nil {
		t.Fatal(err)
	}

	daemon.notifyPush(context.Background(), service.Client())

	// The live and dead clients were pushed; the revoked one and the stranger
	// never reached the service, and the dead one's 410 forgot it.
	if hits.Load() != 2 {
		t.Fatalf("push service hits = %d", hits.Load())
	}
	remaining, err := daemon.push.load()
	if err != nil {
		t.Fatal(err)
	}
	if len(remaining) != 1 || remaining[live.ID.String()].Endpoint != service.URL+"/live" {
		t.Fatalf("remaining subscriptions = %+v", remaining)
	}
}

func TestBrowserSubscribePushStoresOneSubscriptionPerClient(t *testing.T) {
	fixture := newAdapterFixture(t, webCapabilities)
	dialRelayFixture(t, fixture)
	fixture.pair(t)
	ctx := context.Background()
	_, public, private := devicePushKeys(t)
	subscription := browserprotocol.PushSubscribe{Endpoint: "https://push.example/send/abc", PublicKey: public, PrivateKey: private}

	if err := fixture.backend.SubscribePush(ctx, rawBrowserClient(fixture.client.ID), subscription); err != nil {
		t.Fatal(err)
	}
	fixture.backend.owner.browserMu.Lock()
	store := fixture.backend.owner.push
	fixture.backend.owner.browserMu.Unlock()
	if store == nil {
		t.Fatal("a relay home has no push store")
	}
	stored, err := store.load()
	if err != nil {
		t.Fatal(err)
	}
	if len(stored) != 1 || stored[fixture.client.ID.String()] != subscription {
		t.Fatalf("stored = %+v", stored)
	}

	// A key pair that does not belong together could never be signed for.
	_, otherPublic, _ := devicePushKeys(t)
	if err := fixture.backend.SubscribePush(ctx, rawBrowserClient(fixture.client.ID), browserprotocol.PushSubscribe{Endpoint: subscription.Endpoint, PublicKey: otherPublic, PrivateKey: private}); !errors.Is(err, browser.ErrInvalidRequest) {
		t.Fatalf("mismatched keys: %v", err)
	}
	// Re-registering replaces, so a device that re-subscribed is not pushed twice.
	replacement := browserprotocol.PushSubscribe{Endpoint: "https://push.example/send/def", PublicKey: public, PrivateKey: private}
	if err := fixture.backend.SubscribePush(ctx, rawBrowserClient(fixture.client.ID), replacement); err != nil {
		t.Fatal(err)
	}
	if stored, err = store.load(); err != nil || len(stored) != 1 || stored[fixture.client.ID.String()] != replacement {
		t.Fatalf("stored = %+v, %v", stored, err)
	}
}
