package daemon

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"golang.org/x/sys/unix"

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

// accountDaemon is a daemon whose supervisor has published accountHome, which
// is the only thing the resolver may derive a provider config path from.
func accountDaemon(accountHome string) *Daemon {
	daemon := &Daemon{now: time.Now}
	daemon.rememberSupervisorAccount("", accountHome)
	return daemon
}

func TestProviderDefaultsReadTheAccountTheSupervisorLaunchesUnder(t *testing.T) {
	// internal/provider constructs the run environment from this account home
	// (claude_code gets HOME=<accountHome>, codex gets CODEX_HOME=<it>/.codex),
	// so an unrelated HOME or CODEX_HOME in this process must change nothing.
	accountHome := t.TempDir()
	t.Setenv("HOME", t.TempDir())
	t.Setenv("CODEX_HOME", t.TempDir())
	claudeSettings := filepath.Join(accountHome, ".claude", "settings.json")
	codexConfig := filepath.Join(accountHome, ".codex", "config.toml")
	writeProviderConfig(t, claudeSettings, `{"model":"claude-fable-5-1[1m]","effortLevel":"xhigh"}`)
	// The profile table below names a different model. Only the top-level
	// table describes the run the factory will start.
	writeProviderConfig(t, codexConfig, "model = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n\n[profiles.other]\nmodel = \"gpt-4\"\n")

	daemon := accountDaemon(accountHome)
	for _, want := range []struct{ provider, model, effort, source string }{
		{"claude_code", "claude-fable-5-1[1m]", "", claudeSettings},
		{"codex", "gpt-6-astra", "high", codexConfig},
		{"shell", "", "", ""},
		{"unknown_provider", "", "", ""},
	} {
		model, effort, source := daemon.providerAccountDefaults(want.provider, "")
		if model != want.model || effort != want.effort || source != want.source {
			t.Fatalf("%s defaults = %q/%q/%q want %q/%q/%q", want.provider, model, effort, source, want.model, want.effort, want.source)
		}
	}
	// Before a supervisor has published an account, there is nothing to read.
	if model, _, source := (&Daemon{now: time.Now}).providerAccountDefaults("codex", ""); model != "" || source != "" {
		t.Fatalf("unpublished account read %q from %q", model, source)
	}
}

func TestProviderDefaultsTreatUnreadableConfigurationAsUnknown(t *testing.T) {
	// Each case gets its own account home so none can shadow another.
	for _, unusable := range []struct{ name, provider, file, content string }{
		{"claude without a model", "claude_code", ".claude/settings.json", `{"statusLine":{"type":"command"}}`},
		{"claude that is not JSON", "claude_code", ".claude/settings.json", "model = nope"},
		{"codex model only under a profile", "codex", ".codex/config.toml", "[profiles.only]\nmodel = \"gpt-4\"\n"},
		{"codex model too large for the wire", "codex", ".codex/config.toml", "model = \"" + strings.Repeat("m", browserprotocol.MaxAgentModelBytes+1) + "\"\n"},
		{"codex multi-line string", "codex", ".codex/config.toml", "model = \"\"\"gpt-4\"\"\"\n"},
		// readBoundedFile answers only for a bounded regular file, on a path
		// a browser request reaches. An oversized file would otherwise be
		// read whole and a fifo would block; a directory os.ReadFile refuses
		// by itself, so that case pins the answer rather than the guard.
		// "" means the case builds the path itself below.
		{"claude settings larger than the bound", "claude_code", ".claude/settings.json", `{"model":"gpt-6-astra","pad":"` + strings.Repeat("x", maxProviderConfigBytes) + `"}`},
		{"codex config that is a directory", "codex", ".codex/config.toml", ""},
		{"codex config that is a fifo", "codex", ".codex/config.toml", ""},
	} {
		t.Run(unusable.name, func(t *testing.T) {
			accountHome := t.TempDir()
			path := filepath.Join(accountHome, unusable.file)
			switch {
			case strings.HasSuffix(unusable.name, "is a directory"):
				if err := os.MkdirAll(path, 0o700); err != nil {
					t.Fatal(err)
				}
			case strings.HasSuffix(unusable.name, "is a fifo"):
				// A named pipe with no writer blocks forever on read, so a
				// reader that skipped the stat would hang this test rather
				// than fail it. That is the point of refusing it unread.
				if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
					t.Fatal(err)
				}
				if err := unix.Mkfifo(path, 0o600); err != nil {
					t.Skipf("mkfifo unavailable: %v", err)
				}
			default:
				writeProviderConfig(t, path, unusable.content)
			}
			done := make(chan [3]string, 1)
			go func() {
				model, effort, source := accountDaemon(accountHome).providerAccountDefaults(unusable.provider, "")
				done <- [3]string{model, effort, source}
			}()
			var answer [3]string
			select {
			case answer = <-done:
			case <-time.After(10 * time.Second):
				t.Fatal("reading the configuration blocked; it was not refused unread")
			}
			if model, effort, source := answer[0], answer[1], answer[2]; model != "" || effort != "" || source != "" {
				t.Fatalf("%s = %q/%q/%q", unusable.name, model, effort, source)
			}
		})
	}
	// No file at all is the same unknown, not an error.
	empty := accountDaemon(t.TempDir())
	for _, provider := range []string{"claude_code", "codex"} {
		if model, effort, source := empty.providerAccountDefaults(provider, ""); model != "" || effort != "" || source != "" {
			t.Fatalf("%s with no config = %q/%q/%q", provider, model, effort, source)
		}
	}
}

func TestProviderDefaultsServeAnEffortWithoutClaimingAModelSource(t *testing.T) {
	// model_source names the model. A config that sets only an effort would
	// otherwise caption an empty model box "inherited from <path>".
	accountHome := t.TempDir()
	writeProviderConfig(t, filepath.Join(accountHome, ".codex", "config.toml"), "model_reasoning_effort = \"high\"\n")
	model, effort, source := accountDaemon(accountHome).providerAccountDefaults("codex", "")
	if model != "" || effort != "high" || source != "" {
		t.Fatalf("effort-only config = %q/%q/%q", model, effort, source)
	}
	// A literal string is the other single-line TOML form codex may be edited
	// into, and reads the same as a basic one.
	literal := t.TempDir()
	writeProviderConfig(t, filepath.Join(literal, ".codex", "config.toml"), "model = 'gpt-6-astra'\nmodel_reasoning_effort = 'low'\n")
	if model, effort, _ := accountDaemon(literal).providerAccountDefaults("codex", ""); model != "gpt-6-astra" || effort != "low" {
		t.Fatalf("literal strings = %q/%q", model, effort)
	}
}

func TestProviderDefaultsServeTheCachedReadWithinItsWindow(t *testing.T) {
	accountHome := t.TempDir()
	settings := filepath.Join(accountHome, ".claude", "settings.json")
	writeProviderConfig(t, settings, `{"model":"first"}`)
	now := time.Now()
	daemon := &Daemon{now: func() time.Time { return now }}
	daemon.rememberSupervisorAccount("", accountHome)
	if model, _, _ := daemon.providerAccountDefaults("claude_code", ""); model != "first" {
		t.Fatalf("first read = %q", model)
	}
	writeProviderConfig(t, settings, `{"model":"second"}`)
	now = now.Add(providerDefaultsFreshness - time.Second)
	if model, _, _ := daemon.providerAccountDefaults("claude_code", ""); model != "first" {
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
	if model, _, _ := daemon.providerAccountDefaults("claude_code", ""); model != "second" {
		t.Fatalf("expired read = %q want second", model)
	}
	if _, _, source := daemon.providerDefaults("claude_code", ""); source != "" {
		t.Fatalf("account with no home read %q", source)
	}
}

func TestProjectAgentResolvesTheModelTheRunWillUse(t *testing.T) {
	defaults := func(provider, configHome string) (string, string, string) {
		if provider == "codex" && configHome == "" {
			return "gpt-6-astra", "high", "/Users/operator/.codex/config.toml"
		}
		if provider == "codex" {
			return "gpt-7-nova", "low", configHome + "/config.toml"
		}
		return "", "", ""
	}
	summary := kernel.AgentSummary{ID: mustAgentID(t, testID(52)), ProjectID: mustProjectID(t, testID(51)), Name: "codex-native-smoke", Role: "worker", Provider: "codex", Revision: mustRevision(t, 3)}
	inherited := projectAgent(summary, "", defaults)
	if inherited.EffectiveModel != "gpt-6-astra" || inherited.EffectiveReasoningEffort != "high" || inherited.ModelSource != "/Users/operator/.codex/config.toml" || inherited.Model != "" {
		t.Fatalf("inherited = %+v", inherited)
	}
	// model_source names where the MODEL came from, so an own effort beside an
	// inherited model still points at the file the model came from.
	effortOnly := summary
	effortOnly.ReasoningEffort = "medium"
	mixed := projectAgent(effortOnly, "", defaults)
	if mixed.EffectiveModel != "gpt-6-astra" || mixed.EffectiveReasoningEffort != "medium" || mixed.ModelSource != "/Users/operator/.codex/config.toml" {
		t.Fatalf("mixed = %+v", mixed)
	}
	owned := summary
	owned.Model = "gpt-5-codex"
	own := projectAgent(owned, "", defaults)
	if own.EffectiveModel != "gpt-5-codex" || own.EffectiveReasoningEffort != "high" || own.ModelSource != "agent" {
		t.Fatalf("own = %+v", own)
	}
	shell := summary
	shell.Provider = "shell"
	unknown := projectAgent(shell, "", defaults)
	if unknown.EffectiveModel != "" || unknown.EffectiveReasoningEffort != "" || unknown.ModelSource != "" {
		t.Fatalf("unknown = %+v", unknown)
	}
}

// An agent that selects an account is shown that account's own default, not
// the operator login's, because that is the directory its run will read.
func TestProjectAgentReadsTheSelectedAccountsDefaults(t *testing.T) {
	asked := ""
	defaults := func(provider, configHome string) (string, string, string) {
		asked = configHome
		return "gpt-7-nova", "low", configHome + "/config.toml"
	}
	summary := kernel.AgentSummary{ID: mustAgentID(t, testID(52)), ProjectID: mustProjectID(t, testID(51)), Name: "second-login", Role: "worker", Provider: "codex", Revision: mustRevision(t, 3)}
	accountID, err := kernel.AccountIDFromBytes(mustIDBytes(t, testID(53)))
	if err != nil {
		t.Fatal(err)
	}
	summary.AccountID = accountID
	item := projectAgent(summary, "/Users/operator/.codex-second", defaults)
	if asked != "/Users/operator/.codex-second" || item.EffectiveModel != "gpt-7-nova" || item.ModelSource != "/Users/operator/.codex-second/config.toml" {
		t.Fatalf("asked %q, item %+v", asked, item)
	}
	if item.AccountID != summary.AccountID.String() {
		t.Fatalf("served account = %q", item.AccountID)
	}
}
