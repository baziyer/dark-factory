package browserprotocol

import (
	"strings"
	"testing"
)

func TestHumanOptionsRoundTripAndBounds(t *testing.T) {
	detail := HumanRequestDetail{RequestID: strings.Repeat("a", 32), Revision: 1, Question: "Proceed?", ReplyMaxBytes: MaxHumanReplyBytes, Options: []string{"Proceed", "Stop task"}}
	wire, err := EncodeHumanRequestDetail("question", detail)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(wire), `"options":["Proceed","Stop task"]`) {
		t.Fatalf("choices missing: %s", wire)
	}
	for _, options := range [][]string{{"same", "same"}, {" "}, {strings.Repeat("é", 81)}, {"1", "2", "3", "4", "5"}, {"bad\nlabel"}} {
		detail.Options = options
		if _, err := EncodeHumanRequestDetail("question", detail); err == nil {
			t.Fatalf("accepted %q", options)
		}
	}
}
