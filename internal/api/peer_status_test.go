package api

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
)

func TestPeerStatusEscapesTerminalControlsAndKeepsEmptyLists(t *testing.T) {
	status := PeerStatus{Head: 1, Targets: []PeerTarget{}, Questions: []PeerQuestion{}}
	encoded, err := status.MarshalJSON()
	if err != nil || string(encoded) != `{"head":1,"targets":[],"questions":[],"next_target_offset":null,"next_offset":null}` {
		t.Fatalf("empty status=%s %v", encoded, err)
	}
	status.Questions = append(status.Questions, PeerQuestion{ID: "0123456789abcdef0123456789abcdef", SourceTaskID: "1123456789abcdef0123456789abcdef", TargetTaskID: "2123456789abcdef0123456789abcdef", Question: "\x1b\u009b", RecipientDeliveryState: "pending", AnswerDeliveryState: "pending", Revision: 1})
	encoded, err = status.MarshalJSON()
	if err != nil || bytes.Contains(encoded, []byte{0x1b, 0xc2, 0x9b}) || !bytes.Contains(encoded, []byte(`\u009b`)) {
		t.Fatalf("terminal unsafe status=%q %v", encoded, err)
	}
}

func TestPeerStatusWorstEscapedPageFitsControlFrame(t *testing.T) {
	control := strings.Repeat("\x1b", 2048)
	status := PeerStatus{Head: 1, Targets: []PeerTarget{}, Questions: []PeerQuestion{{ID: "0123456789abcdef0123456789abcdef", SourceTaskID: "1123456789abcdef0123456789abcdef", TargetTaskID: "2123456789abcdef0123456789abcdef", Question: control, Answer: control, RecipientDeliveryState: "pending", AnswerDeliveryState: "pending", RecipientAvailability: "available", AnswerAvailability: "pending", Revision: 1}}}
	for i := 0; i < 4; i++ {
		status.Targets = append(status.Targets, PeerTarget{TaskID: "3123456789abcdef0123456789abcdef", AgentID: "4123456789abcdef0123456789abcdef", Name: strings.Repeat("\x1b", 128), Title: strings.Repeat("\x1b", 1024), Status: "queued", Revision: 1})
	}
	if _, err := NewPeerStatusReply(status); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(struct {
		Result PeerStatus `json:"result"`
	}{status})
	if err != nil || len(encoded)+responsePrelude >= 64<<10 {
		t.Fatalf("encoded peer page=%d %v", len(encoded), err)
	}
}
