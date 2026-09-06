package daemon

import (
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func TestProjectBrowserAuthenticationProjectsExactDurableEffectCapabilities(t *testing.T) {
	rawID := make([]byte, kernel.IDBytes)
	for index := range rawID {
		rawID[index] = byte(index + 1)
	}
	clientID, err := kernel.BrowserClientIDFromBytes(rawID)
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name string
		mask kernel.BrowserCapabilityMask
		wire browserprotocol.Capabilities
	}{
		{name: "observe", mask: kernel.BrowserCapabilityObserve, wire: browserprotocol.CapabilityObserve},
		{name: "human actions", mask: kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityHumanActions, wire: browserprotocol.CapabilityObserve | browserprotocol.CapabilityHumanActions},
		{name: "terminal input", mask: kernel.BrowserCapabilityObserve | kernel.BrowserCapabilityTerminalInput, wire: browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput},
		{name: "all", mask: kernel.BrowserCapabilityKnownMask, wire: browserprotocol.CapabilityObserve | browserprotocol.CapabilityPrivateHumanRequestDetail | browserprotocol.CapabilityHumanActions | browserprotocol.CapabilityTerminalInput | browserprotocol.CapabilityAdministration},
	} {
		t.Run(test.name, func(t *testing.T) {
			authentication, err := projectBrowserAuthentication(kernel.BrowserClient{ID: clientID, CapabilityMask: test.mask})
			if err != nil {
				t.Fatal(err)
			}
			if authentication.Capabilities != test.wire {
				t.Fatalf("wire capabilities=%d want %d", authentication.Capabilities, test.wire)
			}
		})
	}
	for _, mask := range []kernel.BrowserCapabilityMask{0, kernel.BrowserCapabilityHumanActions, kernel.BrowserCapabilityObserve | (kernel.BrowserCapabilityKnownMask + 1)} {
		if _, err := projectBrowserAuthentication(kernel.BrowserClient{ID: clientID, CapabilityMask: mask}); err == nil {
			t.Fatalf("accepted invalid durable capability mask %d", mask)
		}
	}
}
