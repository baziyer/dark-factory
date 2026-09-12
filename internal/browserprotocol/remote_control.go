package browserprotocol

import (
	"encoding/base64"
	"fmt"
	"net/url"
	"strings"
	"unicode/utf8"
)

const (
	MaxRemoteInviteLinkBytes = 8192
	MaxRemoteInviteSVGBytes  = 32768
)

// remoteInviteLinkPrefix is the exact everything-before-the-members of the
// invitation the daemon mints. The link is a live pairing secret, so the wire
// admits only that one syntactic form.
const remoteInviteLinkPrefix = "https://app.darkfactory.build/remote#df_remote&"

// RemoteInvite asks the factory for one remote pairing invitation. It carries
// no members: the invitation is entirely the daemon's to mint.
type RemoteInvite struct{}

type RemoteInviteResult struct {
	Link        string  `json:"link"`
	ExpiresAtMS Decimal `json:"expires_at_ms"`
	SVG         string  `json:"svg"`
}

func EncodeRemoteInvite(id string, value RemoteInvite) ([]byte, error) {
	return encodeControl(TypeRemoteInvite, id, value)
}

func EncodeRemoteInviteResult(id string, value RemoteInviteResult) ([]byte, error) {
	return encodeControl(TypeRemoteInviteResult, id, value)
}

// MaxPushEndpointBytes bounds a push service URL; the services in use issue
// URLs a few hundred bytes long.
const MaxPushEndpointBytes = 2048

// pushServiceHosts are the only origins a factory will ever POST a push to.
// A subscription names the endpoint, and any observing client may register
// one, so without this list the verb would be a daemon-side request to an
// address of the client's choosing. These are the push services behind every
// browser that can install the remote console; Edge's is per-tenant under one
// suffix.
var pushServiceHosts = map[string]bool{
	"web.push.apple.com":                true,
	"fcm.googleapis.com":                true,
	"updates.push.services.mozilla.com": true,
}

// pushServiceEndpoint reports whether a subscription endpoint belongs to a
// known push service: https, one of the listed hosts or an Edge tenant, no
// credentials, no explicit port.
func pushServiceEndpoint(endpoint string) bool {
	parsed, err := url.Parse(endpoint)
	if err != nil || parsed.Scheme != "https" || parsed.User != nil || parsed.Port() != "" || parsed.Host != parsed.Hostname() {
		return false
	}
	host := parsed.Hostname()
	return pushServiceHosts[host] || strings.HasSuffix(host, ".notify.windows.com") && len(host) > len(".notify.windows.com")
}

// PushSubscribe hands the factory one device's Web Push subscription and the
// VAPID key pair the device generated for it. The device owns the key so one
// subscription can serve every factory it pairs with: a push service binds a
// subscription to exactly one application server key, and the daemons cannot
// share one. Holding the private key only lets a factory send this device
// empty "needs you" pushes, which is the whole point of handing it over.
type PushSubscribe struct {
	Endpoint   string `json:"endpoint"`
	PublicKey  string `json:"public_key"`
	PrivateKey string `json:"private_key"`
}

// PushSubscribeResult carries nothing: the subscription is stored or the
// request is refused.
type PushSubscribeResult struct{}

func EncodePushSubscribe(id string, value PushSubscribe) ([]byte, error) {
	return encodeControl(TypePushSubscribe, id, value)
}

func EncodePushSubscribeResult(id string, value PushSubscribeResult) ([]byte, error) {
	return encodeControl(TypePushSubscribeResult, id, value)
}

// base64url without padding, as PushManager and WebCrypto exports are carried.
func base64URL(value string, min, max int) bool {
	if len(value) < min || len(value) > max {
		return false
	}
	_, err := base64.RawURLEncoding.DecodeString(value)
	return err == nil
}

func validRemoteControl(kind MessageType, body any) error {
	body = indirect(body)
	bad := func() error { return fmt.Errorf("%w: invalid %s", ErrMalformed, kind) }
	printable := func(value string) bool {
		if !utf8.ValidString(value) {
			return false
		}
		for _, character := range value {
			if character < 0x20 || character == 0x7f {
				return false
			}
		}
		return true
	}
	link := func(value string) bool {
		return len(value) != 0 && len(value) <= MaxRemoteInviteLinkBytes && strings.HasPrefix(value, remoteInviteLinkPrefix) && printable(value)
	}
	switch value := body.(type) {
	case RemoteInvite:
	case PushSubscribeResult:
	case PushSubscribe:
		// 87 characters is exactly one uncompressed P-256 point; a PKCS#8
		// P-256 private key exports to 138 bytes, bounded loosely.
		if len(value.Endpoint) > MaxPushEndpointBytes || !printable(value.Endpoint) || !pushServiceEndpoint(value.Endpoint) ||
			!base64URL(value.PublicKey, 87, 87) || !base64URL(value.PrivateKey, 1, 512) {
			return bad()
		}
	case RemoteInviteResult:
		if !link(value.Link) || value.ExpiresAtMS == 0 || !utf8.ValidString(value.SVG) ||
			len(value.SVG) == 0 || len(value.SVG) > MaxRemoteInviteSVGBytes || !strings.HasPrefix(value.SVG, "<svg") {
			return bad()
		}
	default:
		return bad()
	}
	return nil
}
