package provider

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// A run whose agent selects an account points that provider's CLI at the
// account's own configuration directory, and nothing else in the environment
// moves. No account leaves the environment exactly as it was.
func TestAccountConfigDirectorySelectsTheProviderLogin(t *testing.T) {
	selected := "/Users/operator/.codex-dogfood"
	for _, kind := range []kernel.Provider{kernel.ProviderClaudeCode, kernel.ProviderCodex, kernel.ProviderShell} {
		t.Run(kind.String(), func(t *testing.T) {
			tools := t.TempDir()
			if kind != kernel.ProviderShell {
				tool := claudeTool
				if kind == kernel.ProviderCodex {
					tool = codexTool
				}
				if err := os.Symlink("/usr/bin/true", filepath.Join(tools, tool)); err != nil {
					t.Fatal(err)
				}
			}
			toolPath := strings.Join([]string{tools, "/usr/bin", "/bin"}, string(filepath.ListSeparator))
			installation, err := ResolveInstallation(kind, toolPath)
			if err != nil {
				t.Fatal(err)
			}
			accountHome := filepath.Join(t.TempDir(), "account")
			base := runtimeFixture(t, toolPath, accountHome)
			plain := requestFor(t, kind, installation, base, "", "").runtime.environment(kind)

			root := t.TempDir()
			withAccount, err := NewRuntimePaths(
				root+"/home", root+"/tmp", "/private/tmp/df-account-test.sock", root+"/attempt.token",
				"/usr/local/bin/factoryctl", root+"/changes", toolPath, accountHome, selected,
			)
			if err != nil {
				t.Fatal(err)
			}
			chosen := withAccount.environment(kind)

			switch kind {
			case kernel.ProviderClaudeCode:
				if slices.ContainsFunc(plain, func(entry string) bool { return strings.HasPrefix(entry, "CLAUDE_CONFIG_DIR=") }) {
					t.Fatalf("no account still set a config directory: %q", plain)
				}
				if !slices.Contains(chosen, "CLAUDE_CONFIG_DIR="+selected) {
					t.Fatalf("account did not reach the environment: %q", chosen)
				}
			case kernel.ProviderCodex:
				if !slices.Contains(plain, "CODEX_HOME="+filepath.Join(accountHome, codexConfigDir)) {
					t.Fatalf("no account changed CODEX_HOME: %q", plain)
				}
				if !slices.Contains(chosen, "CODEX_HOME="+selected) {
					t.Fatalf("account did not reach CODEX_HOME: %q", chosen)
				}
			case kernel.ProviderShell:
				if !slices.Equal(namesOnly(plain), namesOnly(chosen)) {
					t.Fatalf("shell environment names moved: %q vs %q", plain, chosen)
				}
				for _, entry := range chosen {
					if strings.Contains(entry, selected) {
						t.Fatalf("shell launch received an account: %q", entry)
					}
				}
			}
		})
	}

	// The account directory is one absolute path or nothing at all.
	root := t.TempDir()
	for _, bad := range []string{"relative", "/x/", string(filepath.Separator)} {
		if _, err := NewRuntimePaths(root+"/home", root+"/tmp", "/private/tmp/df-account-test.sock", root+"/attempt.token",
			"/usr/local/bin/factoryctl", root+"/changes", "/usr/bin:/bin", root+"/account", bad); err == nil {
			t.Fatalf("account config %q accepted", bad)
		}
	}
}

func namesOnly(environment []string) []string {
	names := make([]string, 0, len(environment))
	for _, entry := range environment {
		name, _, _ := strings.Cut(entry, "=")
		names = append(names, name)
	}
	slices.Sort(names)
	return names
}
