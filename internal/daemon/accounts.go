package daemon

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
)

// An account is a provider plus the configuration directory that CLI logs in
// to: CLAUDE_CONFIG_DIR for claude_code, CODEX_HOME for codex. Discovery reads
// only the identity those directories already publish about themselves. The
// tokens beside that identity are never read into a returned value, and the
// JWT that carries a Codex e-mail is decoded, never verified: it is a display
// label, not authority.
const maxDiscoveredAccounts = browserprotocol.MaxJSONArray

// accountProviders is the closed set of providers that have logins at all.
// Their configuration directory names come from internal/provider, which is
// what a run is actually launched against, so discovery cannot drift from it.
var accountProviders = []kernel.Provider{kernel.ProviderClaudeCode, kernel.ProviderCodex}

// providerForConfigDir matches one home entry against the provider whose
// configuration directory it is named after. A second login for the same
// provider is a suffixed sibling of that name (.codex-dogfood), so the match
// is by prefix.
func providerForConfigDir(name string) (kernel.Provider, bool) {
	for _, kind := range accountProviders {
		if strings.HasPrefix(name, provider.ConfigDirName(kind)) {
			return kind, true
		}
	}
	return 0, false
}

// discoverAccounts lists the provider logins present directly under home. It
// never recurses: a login is one directory named .claude* or .codex* holding
// that CLI's own credential file.
func (daemon *Daemon) discoverAccounts(home string) []browserprotocol.DiscoveredAccount {
	entries, err := os.ReadDir(home)
	if err != nil {
		return nil
	}
	result := make([]browserprotocol.DiscoveredAccount, 0, len(entries))
	for _, entry := range entries {
		name := entry.Name()
		kind, ok := providerForConfigDir(name)
		if !ok {
			continue
		}
		directory := filepath.Join(home, name)
		if info, err := os.Stat(directory); err != nil || !info.IsDir() {
			continue
		}
		account, ok := daemon.describeAccount(kind, home, directory, name)
		if !ok {
			continue
		}
		result = append(result, account)
	}
	sort.Slice(result, func(left, right int) bool { return result[left].Home < result[right].Home })
	if len(result) > maxDiscoveredAccounts {
		result = result[:maxDiscoveredAccounts]
	}
	return result
}

func (daemon *Daemon) describeAccount(kind kernel.Provider, home, directory, name string) (browserprotocol.DiscoveredAccount, bool) {
	account := browserprotocol.DiscoveredAccount{Provider: kind.String(), Home: directory, Label: name}
	switch kind {
	case kernel.ProviderClaudeCode:
		// The default ~/.claude keeps its identity beside itself in
		// ~/.claude.json; a second config directory carries its own copy.
		// The file that proves the login is not always the file that names
		// it -- ~/.claude/.claude.json can hold only local flags -- so read
		// every candidate until one carries an account.
		candidates := []string{filepath.Join(directory, ".claude.json")}
		if directory == filepath.Join(home, ".claude") {
			candidates = append(candidates, filepath.Join(home, ".claude.json"))
		}
		login := false
		for _, candidate := range candidates {
			if _, err := os.Stat(candidate); err != nil {
				continue
			}
			login = true
			if email, organization := claudeIdentity(candidate); email != "" {
				account.Email, account.Organization = email, organization
				break
			}
		}
		if !login {
			return browserprotocol.DiscoveredAccount{}, false
		}
	case kernel.ProviderCodex:
		email, ok := codexIdentity(filepath.Join(directory, "auth.json"))
		if !ok {
			return browserprotocol.DiscoveredAccount{}, false
		}
		account.Email = email
	default:
		return browserprotocol.DiscoveredAccount{}, false
	}
	// The same reader the console's effective model uses, so a login's listed
	// default is exactly what an agent assigned to it would be shown.
	account.DefaultModel, account.DefaultReasoningEffort, _ = daemon.providerDefaults(kind.String(), directory)
	if browserprotocol.ValidDiscoveredAccount(account) != nil {
		return browserprotocol.DiscoveredAccount{}, false
	}
	return account, true
}

// readJSONFile is deliberately bounded: a login file that is not a small JSON
// object is simply an account whose identity is unknown, never an error that
// hides the other logins on the machine.
func readJSONFile(path string) map[string]json.RawMessage {
	data, err := os.ReadFile(path)
	if err != nil || len(data) > 1<<22 {
		return nil
	}
	var object map[string]json.RawMessage
	if json.Unmarshal(data, &object) != nil {
		return nil
	}
	return object
}

func jsonString(object map[string]json.RawMessage, key string) string {
	raw, ok := object[key]
	if !ok {
		return ""
	}
	var value string
	if json.Unmarshal(raw, &value) != nil {
		return ""
	}
	return value
}

func claudeIdentity(path string) (email, organization string) {
	object := readJSONFile(path)
	raw, ok := object["oauthAccount"]
	if !ok {
		return "", ""
	}
	var account map[string]json.RawMessage
	if json.Unmarshal(raw, &account) != nil {
		return "", ""
	}
	return jsonString(account, "emailAddress"), jsonString(account, "organizationName")
}

// codexIdentity reads the e-mail out of the stored OIDC identity token's
// payload. The signature is not checked and no token value is returned: this
// is a label for a login the operator already owns.
func codexIdentity(path string) (string, bool) {
	object := readJSONFile(path)
	raw, ok := object["tokens"]
	if !ok {
		return "", false
	}
	var tokens map[string]json.RawMessage
	if json.Unmarshal(raw, &tokens) != nil {
		return "", false
	}
	if jsonString(tokens, "account_id") == "" {
		return "", false
	}
	parts := strings.Split(jsonString(tokens, "id_token"), ".")
	if len(parts) != 3 {
		return "", true
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return "", true
	}
	var claims map[string]json.RawMessage
	if json.Unmarshal(payload, &claims) != nil {
		return "", true
	}
	return jsonString(claims, "email"), true
}

// agentAccountConfigDir is the configuration directory one agent's selected
// account lives in, or the empty string when it uses the provider default.
// A selected account that no longer exists is corrupt state, not a silent
// fall back to somebody else's login.
func (daemon *Daemon) agentAccountConfigDir(ctx context.Context, id kernel.AgentID) (string, error) {
	agent, found, err := daemon.store.Agent(ctx, id)
	if err != nil {
		return "", err
	}
	if !found {
		return "", kernel.ErrCorruptState
	}
	if (agent.AccountID == kernel.AccountID{}) {
		return "", nil
	}
	accounts, err := daemon.store.ListAccounts(ctx)
	if err != nil {
		return "", err
	}
	for _, account := range accounts {
		if account.ID == agent.AccountID {
			return account.Home, nil
		}
	}
	return "", kernel.ErrCorruptState
}

// operatorHome is the one home directory discovery and linking look in. It is
// the account record's home, never a caller-supplied environment value.
func operatorHome() (string, error) { return install.AccountHome() }
