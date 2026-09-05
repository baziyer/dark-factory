package daemon

import (
	"encoding/base64"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// discoveryDaemon is the smallest daemon discovery needs: a clock, because it
// reads provider defaults through the same cached reader the console uses.
func discoveryDaemon() *Daemon { return &Daemon{now: time.Now} }

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

// fakeIDToken is an unsigned JWT shape: only the payload is ever read, and
// only for the e-mail label.
func fakeIDToken(payload string) string {
	return "header." + base64.RawURLEncoding.EncodeToString([]byte(payload)) + ".signature"
}

// Discovery finds the CLI logins that already exist under one home, reads the
// identity and defaults those logins publish about themselves, and returns no
// token value at all.
func TestDiscoverAccountsReadsLoginsAndNeverTokens(t *testing.T) {
	home := t.TempDir()
	writeFile(t, filepath.Join(home, ".claude.json"), `{"oauthAccount":{"emailAddress":"operator@example.com","organizationName":"Example Org"}}`)
	writeFile(t, filepath.Join(home, ".claude", "settings.json"), `{"model":"claude-fable-5-1"}`)
	writeFile(t, filepath.Join(home, ".codex", "auth.json"), `{"tokens":{"account_id":"acct-1","id_token":"`+fakeIDToken(`{"email":"one@example.com"}`)+`","access_token":"SECRET-ACCESS","refresh_token":"SECRET-REFRESH"},"OPENAI_API_KEY":"SECRET-KEY"}`)
	writeFile(t, filepath.Join(home, ".codex", "config.toml"), "model = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n\n[projects.\"/x\"]\nmodel = \"never-read\"\n")
	writeFile(t, filepath.Join(home, ".codex-dogfood", "auth.json"), `{"tokens":{"account_id":"acct-2","id_token":"`+fakeIDToken(`{"email":"two@example.com"}`)+`"}}`)
	// Neither of these is a login: one has no credential file, the other is a
	// plain file whose name happens to match.
	if err := os.MkdirAll(filepath.Join(home, ".codex-empty"), 0o700); err != nil {
		t.Fatal(err)
	}
	writeFile(t, filepath.Join(home, ".claudette"), "not a directory")

	found := discoveryDaemon().discoverAccounts(home)
	if len(found) != 3 {
		t.Fatalf("discovered %d logins: %+v", len(found), found)
	}
	byHome := map[string]int{}
	for index, account := range found {
		byHome[account.Home] = index
	}
	claude := found[byHome[filepath.Join(home, ".claude")]]
	if claude.Provider != "claude_code" || claude.Email != "operator@example.com" || claude.Organization != "Example Org" || claude.DefaultModel != "claude-fable-5-1" || claude.Label != ".claude" {
		t.Fatalf("claude login = %+v", claude)
	}
	codex := found[byHome[filepath.Join(home, ".codex")]]
	if codex.Provider != "codex" || codex.Email != "one@example.com" || codex.DefaultModel != "gpt-6-astra" || codex.DefaultReasoningEffort != "high" {
		t.Fatalf("codex login = %+v", codex)
	}
	dogfood := found[byHome[filepath.Join(home, ".codex-dogfood")]]
	if dogfood.Email != "two@example.com" || dogfood.DefaultModel != "" || dogfood.LinkedID != "" {
		t.Fatalf("second codex login = %+v", dogfood)
	}
	for _, account := range found {
		served := account.Provider + account.Home + account.Label + account.Email + account.Organization + account.DefaultModel + account.DefaultReasoningEffort + account.LinkedID
		for _, secret := range []string{"SECRET-ACCESS", "SECRET-REFRESH", "SECRET-KEY", "acct-1", "acct-2", ".signature"} {
			if strings.Contains(served, secret) {
				t.Fatalf("discovery leaked %q in %+v", secret, account)
			}
		}
	}
}

// The file that carries a Claude login's account is not the same file for the
// default directory and for a sibling, and neither may borrow the other's: a
// sibling that read $HOME's account would report the default login's identity.
func TestClaudeIdentityIsPerDirectoryWithNoFallback(t *testing.T) {
	home := t.TempDir()
	for _, name := range []string{".claude", ".claude-work"} {
		if err := os.MkdirAll(filepath.Join(home, name), 0o700); err != nil {
			t.Fatal(err)
		}
	}
	// No account file anywhere: neither directory is a login.
	if found := discoveryDaemon().discoverAccounts(home); len(found) != 0 {
		t.Fatalf("logins without an account file discovered: %+v", found)
	}

	// $HOME's account names the default directory only. The sibling is not a
	// login at all, and certainly does not inherit that identity.
	writeFile(t, filepath.Join(home, ".claude.json"), `{"oauthAccount":{"emailAddress":"default@example.com"}}`)
	found := discoveryDaemon().discoverAccounts(home)
	if len(found) != 1 || found[0].Home != filepath.Join(home, ".claude") || found[0].Email != "default@example.com" {
		t.Fatalf("default login = %+v", found)
	}

	// The default directory's own file is never read for identity. It carries
	// a decoy account here so the assertion tells the two readers apart: one
	// that preferred the inner file would answer with this address instead.
	writeFile(t, filepath.Join(home, ".claude", ".claude.json"), `{"firstStartTime":"2026-01-01","oauthAccount":{"emailAddress":"inner-decoy@example.com"}}`)
	if again := discoveryDaemon().discoverAccounts(home); len(again) != 1 || again[0].Email != "default@example.com" {
		t.Fatalf("default login read the inner file = %+v", again)
	}

	// A sibling becomes a login when it carries its own account, and reports
	// that one rather than $HOME's.
	writeFile(t, filepath.Join(home, ".claude-work", ".claude.json"), `{"oauthAccount":{"emailAddress":"work@example.com"}}`)
	found = discoveryDaemon().discoverAccounts(home)
	if len(found) != 2 || found[0].Email != "default@example.com" || found[1].Email != "work@example.com" {
		t.Fatalf("both claude logins = %+v", found)
	}
}

// A login is not hidden because something it says about itself is too long for
// the wire: the display field is dropped and the login stays linkable.
func TestOversizedIdentityKeepsTheLoginDiscoverable(t *testing.T) {
	home := t.TempDir()
	long := strings.Repeat("x", 200)
	writeFile(t, filepath.Join(home, ".codex", "auth.json"), `{"tokens":{"account_id":"acct-1","id_token":"`+fakeIDToken(`{"email":"`+long+`@example.com"}`)+`"}}`)
	writeFile(t, filepath.Join(home, ".codex", "config.toml"), "model = \""+long+"\"\n")
	writeFile(t, filepath.Join(home, ".claude.json"), `{"oauthAccount":{"emailAddress":"operator@example.com","organizationName":"`+long+`"}}`)
	if err := os.MkdirAll(filepath.Join(home, ".claude"), 0o700); err != nil {
		t.Fatal(err)
	}
	found := discoveryDaemon().discoverAccounts(home)
	if len(found) != 2 {
		t.Fatalf("oversized identity dropped a login: %+v", found)
	}
	claude, codex := found[0], found[1]
	if claude.Provider != "claude_code" || claude.Email != "operator@example.com" || claude.Organization != "" {
		t.Fatalf("claude login = %+v", claude)
	}
	if codex.Provider != "codex" || codex.Email != "" || codex.DefaultModel != "" || codex.Home == "" {
		t.Fatalf("codex login = %+v", codex)
	}

	// A directory name too long to be a label is shortened, not dropped.
	name := "." + strings.Repeat("codex", 40)
	writeFile(t, filepath.Join(home, name, "auth.json"), `{"tokens":{"account_id":"acct-2","id_token":"`+fakeIDToken(`{"email":"two@example.com"}`)+`"}}`)
	for _, account := range discoveryDaemon().discoverAccounts(home) {
		if account.Home != filepath.Join(home, name) {
			continue
		}
		if len(account.Label) != 128 || !strings.HasPrefix(name, account.Label) {
			t.Fatalf("long directory label = %q (%d bytes)", account.Label, len(account.Label))
		}
		return
	}
	t.Fatal("a long directory name dropped its login")
}
