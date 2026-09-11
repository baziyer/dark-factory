package main

import (
	"reflect"
	"strings"
	"testing"
)

func TestHumanOptionsRequireExplicitBoundedSuggestions(t *testing.T) {
	base := []string{"attempt", "request-human", "--idempotency-key", "0123456789abcdef0123456789abcdef", "--question", "Which scope?"}
	command, help, ok := parse(append(base, "--option", "Narrow fix", "--option", "Stop task"))
	if !ok || help || !reflect.DeepEqual(command.options, []string{"Narrow fix", "Stop task"}) {
		t.Fatalf("options: %+v %v %v", command, help, ok)
	}
	for _, suffix := range [][]string{
		{"--option"}, {"--option", ""}, {"--option", "  "}, {"--option", "bad\nlabel"},
		{"--option", strings.Repeat("é", 81)}, {"--option", "same", "--option", "same"},
		{"--option", "1", "--option", "2", "--option", "3", "--option", "4", "--option", "5"},
	} {
		if _, _, ok := parse(append(append([]string{}, base...), suffix...)); ok {
			t.Fatalf("accepted invalid options %q", suffix)
		}
	}
}
