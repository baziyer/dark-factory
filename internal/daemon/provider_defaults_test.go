package daemon

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func writeProviderConfig(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestProviderDefaultsReadsEachCLIsOwnConfiguration(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("CODEX_HOME", "")
	claudeSettings := filepath.Join(home, ".claude", "settings.json")
	codexConfig := filepath.Join(home, ".codex", "config.toml")
	writeProviderConfig(t, claudeSettings, `{"model":"claude-fable-5-1[1m]","effortLevel":"xhigh"}`)
	// The profile table below names a different model. Only the top-level
	// table describes the run the factory will start.
	writeProviderConfig(t, codexConfig, "model = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n\n[profiles.other]\nmodel = \"gpt-4\"\n")

	daemon := &Daemon{now: time.Now}
	for _, want := range []struct{ provider, model, effort, source string }{
		{"claude_code", "claude-fable-5-1[1m]", "", claudeSettings},
		{"codex", "gpt-6-astra", "high", codexConfig},
		{"shell", "", "", ""},
		{"unknown_provider", "", "", ""},
	} {
		model, effort, source := daemon.providerAccountDefaults(want.provider)
		if model != want.model || effort != want.effort || source != want.source {
			t.Fatalf("%s defaults = %q/%q/%q want %q/%q/%q", want.provider, model, effort, source, want.model, want.effort, want.source)
		}
	}
}

func TestProviderDefaultsTreatUnreadableConfigurationAsUnknown(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("CODEX_HOME", "")
	daemon := &Daemon{now: time.Now}
	for _, provider := range []string{"claude_code", "codex"} {
		if model, effort, source := daemon.providerAccountDefaults(provider); model != "" || effort != "" || source != "" {
			t.Fatalf("%s with no config = %q/%q/%q", provider, model, effort, source)
		}
	}
	// A file that exists but says nothing usable is equally unknown, and a
	// value too large for the wire is refused rather than truncated.
	writeProviderConfig(t, filepath.Join(home, ".claude", "settings.json"), `{"statusLine":{"type":"command"}}`)
	writeProviderConfig(t, filepath.Join(home, ".codex", "config.toml"), "[profiles.only]\nmodel = \"gpt-4\"\n")
	oversized := t.TempDir()
	t.Setenv("CODEX_HOME", oversized)
	writeProviderConfig(t, filepath.Join(oversized, "config.toml"), "model = \""+strings.Repeat("m", browserprotocol.MaxAgentModelBytes+1)+"\"\n")
	fresh := &Daemon{now: time.Now}
	for _, provider := range []string{"claude_code", "codex"} {
		if model, effort, source := fresh.providerAccountDefaults(provider); model != "" || effort != "" || source != "" {
			t.Fatalf("%s with unusable config = %q/%q/%q", provider, model, effort, source)
		}
	}
}

func TestProviderDefaultsServeTheCachedReadWithinItsWindow(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("CODEX_HOME", "")
	settings := filepath.Join(home, ".claude", "settings.json")
	writeProviderConfig(t, settings, `{"model":"first"}`)
	now := time.Now()
	daemon := &Daemon{now: func() time.Time { return now }}
	if model, _, _ := daemon.providerAccountDefaults("claude_code"); model != "first" {
		t.Fatalf("first read = %q", model)
	}
	writeProviderConfig(t, settings, `{"model":"second"}`)
	now = now.Add(providerDefaultsFreshness - time.Second)
	if model, _, _ := daemon.providerAccountDefaults("claude_code"); model != "first" {
		t.Fatalf("cached read = %q want first", model)
	}
	// A second account of the same provider is a separate entry, not a hit on
	// the first one: the follow-up that gives an agent its own account home
	// must not read another account's model.
	second := t.TempDir()
	writeProviderConfig(t, filepath.Join(second, "settings.json"), `{"model":"other-account"}`)
	if model, _, source := daemon.providerDefaults("claude_code", second); model != "other-account" || source != filepath.Join(second, "settings.json") {
		t.Fatalf("second account = %q from %q", model, source)
	}
	now = now.Add(2 * time.Second)
	if model, _, _ := daemon.providerAccountDefaults("claude_code"); model != "second" {
		t.Fatalf("expired read = %q want second", model)
	}
	if _, _, source := daemon.providerDefaults("claude_code", ""); source != "" {
		t.Fatalf("account with no home read %q", source)
	}
}

func TestProjectAgentResolvesTheModelTheRunWillUse(t *testing.T) {
	defaults := func(provider string) (string, string, string) {
		if provider == "codex" {
			return "gpt-6-astra", "high", "/Users/operator/.codex/config.toml"
		}
		return "", "", ""
	}
	summary := kernel.AgentSummary{ID: mustAgentID(t, testID(52)), ProjectID: mustProjectID(t, testID(51)), Name: "codex-native-smoke", Role: "worker", Provider: "codex", Revision: mustRevision(t, 3)}
	inherited := projectAgent(summary, defaults)
	if inherited.EffectiveModel != "gpt-6-astra" || inherited.EffectiveReasoningEffort != "high" || inherited.ModelSource != "/Users/operator/.codex/config.toml" || inherited.Model != "" {
		t.Fatalf("inherited = %+v", inherited)
	}
	// model_source names where the MODEL came from, so an own effort beside an
	// inherited model still points at the file the model came from.
	effortOnly := summary
	effortOnly.ReasoningEffort = "medium"
	mixed := projectAgent(effortOnly, defaults)
	if mixed.EffectiveModel != "gpt-6-astra" || mixed.EffectiveReasoningEffort != "medium" || mixed.ModelSource != "/Users/operator/.codex/config.toml" {
		t.Fatalf("mixed = %+v", mixed)
	}
	owned := summary
	owned.Model = "gpt-5-codex"
	own := projectAgent(owned, defaults)
	if own.EffectiveModel != "gpt-5-codex" || own.EffectiveReasoningEffort != "high" || own.ModelSource != "agent" {
		t.Fatalf("own = %+v", own)
	}
	shell := summary
	shell.Provider = "shell"
	unknown := projectAgent(shell, defaults)
	if unknown.EffectiveModel != "" || unknown.EffectiveReasoningEffort != "" || unknown.ModelSource != "" {
		t.Fatalf("unknown = %+v", unknown)
	}
}
