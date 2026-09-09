package daemon

import (
	"bytes"
	"context"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// A lost peer-ask/answer response re-enters notifyPeerDelivery with its
// durable receipt already present. It must not mint a second PTY write.
func TestPeerDeliveryReplayDoesNotTouchTerminal(t *testing.T) {
	id, err := kernel.PeerDeliveryIDFromBytes(bytes.Repeat([]byte{1}, kernel.IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	for _, question := range []kernel.PeerQuestion{{RecipientDeliveryID: &id}, {AnswerIdempotencyKey: &[kernel.IDBytes]byte{1}, AnswerDeliveryID: &id}} {
		if err := (&Daemon{}).notifyPeerDelivery(context.Background(), question); err != nil {
			t.Fatalf("replay notification = %v", err)
		}
	}
}
