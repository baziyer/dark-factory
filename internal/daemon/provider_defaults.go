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
// assignments in either single-line string form, basic ("...") or literal
// ('...'), so a line scan reads them without a TOML dependency. A multi-line
// string ("""..."""), which codex does not write, reads as empty and so as
// unknown rather than as a wrong answer.
var codexModelLine = regexp.MustCompile(`^[\t ]*(model|model_reasoning_effort)[\t ]*=[\t ]*("[^"]*"|'[^']*')`)

// providerConfigHome is the directory a provider CLI reads its configuration
// from, derived from the one account home the supervisor launches every run
// under. internal/provider constructs that environment rather than forwarding
// this process's: claude_code is given HOME=<accountHome> and codex is given
// CODEX_HOME=<accountHome>/.codex, so reading os.UserHomeDir or CODEX_HOME
// here would name a different account than the run uses.
func providerConfigHome(provider, accountHome string) string {
	if accountHome == "" {
		return ""
	}
	switch provider {
	case "claude_code":
		return filepath.Join(accountHome, ".claude")
	case "codex":
		return filepath.Join(accountHome, ".codex")
	}
	return ""
}

// providerAccountDefaults reads the configuration of the account the supervisor
// will actually launch this provider under. It answers unknown until a
// supervisor has published one, which RunScheduler does with its first probe.
func (daemon *Daemon) providerAccountDefaults(provider string) (model, effort, source string) {
	if daemon == nil {
		return "", "", ""
	}
	accountHome := ""
	if published := daemon.accountHome.Load(); published != nil {
		accountHome = *published
	}
	return daemon.providerDefaults(provider, providerConfigHome(provider, accountHome))
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
			value := strings.Trim(match[2], `"'`)
			if match[1] == "model" {
				result.model = value
			} else {
				result.effort = value
			}
		}
		if result.model == "" {
			// model_source names the model. Without one the path would caption
			// an empty box "inherited from"; an effort beside it is still real.
			result.source = ""
			if result.effort == "" {
				return providerDefault{}
			}
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
