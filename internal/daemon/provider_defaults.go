package daemon

import (
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

// providerDefaultsFreshness bounds how stale a served provider default may be.
// Reading one opens and parses the CLI's own configuration file, so a repeated
// snapshot inside this window is answered from the last read. It mirrors
// topologyFreshness: a cost guard, not state, and losing it costs one read.
const providerDefaultsFreshness = 30 * time.Second

// providerAccount is a provider and the configuration directory it reads. The
// pair is the cache key because one provider can have several accounts.
type providerAccount struct {
	provider string
	home     string
}

type providerDefault struct {
	model  string
	effort string
	source string
	at     time.Time
}

// codexModelLine matches the top-level TOML `model` and `model_reasoning_effort`
// assignments. Codex writes both as plain double-quoted basic strings, so a line
// scan reads them without a TOML dependency.
var codexModelLine = regexp.MustCompile(`^[\t ]*(model|model_reasoning_effort)[\t ]*=[\t ]*"([^"]*)"`)

// defaultProviderHome is the account directory today's callers read: the one
// the operator's own CLI uses, since an agent cannot yet name an account of its
// own. "" means the provider has no configuration to read (shell) or this
// process cannot resolve its home.
func defaultProviderHome(provider string) string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	switch provider {
	case "claude_code":
		return filepath.Join(home, ".claude")
	case "codex":
		// The runner forwards CODEX_HOME from this process's environment, so
		// resolving it here names the file the provider itself will read.
		if codexHome := os.Getenv("CODEX_HOME"); codexHome != "" {
			return codexHome
		}
		return filepath.Join(home, ".codex")
	}
	return ""
}

// providerAccountDefaults resolves a provider's default account and reads it.
func (daemon *Daemon) providerAccountDefaults(provider string) (model, effort, source string) {
	return daemon.providerDefaults(provider, defaultProviderHome(provider))
}

// providerDefaults reports the model and reasoning effort the provider CLI
// picks for itself when the agent names neither, plus the configuration file
// that said so. An agent with no model of its own still runs with one; this is
// the factory reading the same file the CLI will.
//
// A missing, unreadable or unparsable file is not an error: the CLI keeps a
// default the factory simply cannot see, and every returned value is empty.
func (daemon *Daemon) providerDefaults(provider, home string) (model, effort, source string) {
	if daemon == nil || home == "" {
		return "", "", ""
	}
	account := providerAccount{provider: provider, home: home}
	now := daemon.now()
	daemon.providerDefaultMu.Lock()
	defer daemon.providerDefaultMu.Unlock()
	held, ok := daemon.providerDefaultCache[account]
	if !ok || now.Before(held.at) || now.Sub(held.at) >= providerDefaultsFreshness {
		held = readProviderDefault(account)
		held.at = now
		if daemon.providerDefaultCache == nil {
			daemon.providerDefaultCache = make(map[providerAccount]providerDefault, 2)
		}
		daemon.providerDefaultCache[account] = held
	}
	return held.model, held.effort, held.source
}

func readProviderDefault(account providerAccount) providerDefault {
	var result providerDefault
	switch account.provider {
	case "claude_code":
		// claude reads its top-level "model" from the account settings file.
		// It has no configured reasoning effort the factory can name.
		result.source = filepath.Join(account.home, "settings.json")
		data, err := os.ReadFile(result.source)
		if err != nil {
			return providerDefault{}
		}
		var settings struct {
			Model string `json:"model"`
		}
		if json.Unmarshal(data, &settings) != nil || settings.Model == "" {
			return providerDefault{}
		}
		result.model = settings.Model
	case "codex":
		result.source = filepath.Join(account.home, "config.toml")
		data, err := os.ReadFile(result.source)
		if err != nil {
			return providerDefault{}
		}
		for _, line := range strings.Split(string(data), "\n") {
			// Only the top-level table describes the default run. A profile
			// or provider table below the first header names something else.
			if strings.HasPrefix(strings.TrimLeft(line, "\t "), "[") {
				break
			}
			match := codexModelLine.FindStringSubmatch(line)
			if match == nil {
				continue
			}
			if match[1] == "model" {
				result.model = match[2]
			} else {
				result.effort = match[2]
			}
		}
		if result.model == "" && result.effort == "" {
			return providerDefault{}
		}
	default:
		// shell runs no model, and the daemon rejects one for it.
		return providerDefault{}
	}
	if !fitsWire(result.model, browserprotocol.MaxAgentModelBytes) || !fitsWire(result.effort, browserprotocol.MaxAgentModelBytes) || !fitsWire(result.source, browserprotocol.MaxModelSourceBytes) {
		return providerDefault{}
	}
	return result
}

func fitsWire(value string, maximum int) bool {
	return len(value) <= maximum && utf8.ValidString(value)
}
