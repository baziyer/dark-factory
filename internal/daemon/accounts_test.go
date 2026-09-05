package daemon

import (
	"encoding/base64"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

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

	found := discoverAccounts(home)
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

// A directory named like a login but holding no credential file is not one,
// and a second Claude config directory must carry its own identity.
func TestDiscoverAccountsRequiresTheLoginFile(t *testing.T) {
	home := t.TempDir()
	if err := os.MkdirAll(filepath.Join(home, ".claude"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(home, ".claude-work"), 0o700); err != nil {
		t.Fatal(err)
	}
	if found := discoverAccounts(home); len(found) != 0 {
		t.Fatalf("logins without credentials discovered: %+v", found)
	}
	// The default directory borrows the home-level identity file.
	writeFile(t, filepath.Join(home, ".claude.json"), `{"oauthAccount":{"emailAddress":"operator@example.com"}}`)
	found := discoverAccounts(home)
	if len(found) != 1 || found[0].Home != filepath.Join(home, ".claude") {
		t.Fatalf("default claude login = %+v", found)
	}
	// A second directory needs its own.
	writeFile(t, filepath.Join(home, ".claude-work", ".claude.json"), `{"oauthAccount":{"emailAddress":"work@example.com"}}`)
	found = discoverAccounts(home)
	if len(found) != 2 || found[1].Email != "work@example.com" {
		t.Fatalf("second claude login = %+v", found)
	}
}
