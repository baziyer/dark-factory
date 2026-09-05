package daemon

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
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
func providerConfigHome(kind, accountHome string) string {
	parsed, err := kernel.ParseProvider(kind)
	if accountHome == "" || err != nil {
		return ""
	}
	name := provider.ConfigDirName(parsed)
	if name == "" {
		return ""
	}
	return filepath.Join(accountHome, name)
}

// providerAccountDefaults reads the configuration of the account the supervisor
// will actually launch this provider under. configHome names it directly when
// the agent has a linked account; otherwise it is the operator's own login for
// that provider, which answers unknown until a supervisor has published the
// account home, as RunScheduler does with its first probe.
func (daemon *Daemon) providerAccountDefaults(kind, configHome string) (model, effort, source string) {
	if daemon == nil {
		return "", "", ""
	}
	if configHome == "" {
		accountHome := ""
		if published := daemon.accountHome.Load(); published != nil {
			accountHome = *published
		}
		configHome = providerConfigHome(kind, accountHome)
	}
	return daemon.providerDefaults(kind, configHome)
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
	if held, ok := daemon.freshProviderDefault(account, now); ok {
		return held.model, held.effort, held.source
	}
	// The read is deliberately outside the lock: a browser request reaches
	// this path, and two racing readers costing one extra bounded file read is
	// cheaper than either of them waiting on the other's disk.
	held := readProviderDefault(account)
	held.at = now
	daemon.providerDefaultMu.Lock()
	if daemon.providerDefaultCache == nil {
		daemon.providerDefaultCache = make(map[providerAccount]providerDefault, 2)
	}
	daemon.providerDefaultCache[account] = held
	daemon.providerDefaultMu.Unlock()
	return held.model, held.effort, held.source
}

func (daemon *Daemon) freshProviderDefault(account providerAccount, now time.Time) (providerDefault, bool) {
	daemon.providerDefaultMu.Lock()
	defer daemon.providerDefaultMu.Unlock()
	held, ok := daemon.providerDefaultCache[account]
	if !ok || now.Before(held.at) || now.Sub(held.at) >= providerDefaultsFreshness {
		return providerDefault{}, false
	}
	return held, true
}

// maxProviderConfigBytes bounds one provider configuration or login file. A
// bigger one is refused unread: these paths are named by the operator's own
// home, but a browser request is what triggers reading them.
const maxProviderConfigBytes = 1 << 20

// readBoundedFile answers only for a regular file that was within the bound
// when it was measured, so a device, a directory or an enormous file costs a
// stat instead of a read. A file that grows between the stat and the read is
// still read whole; the bound is a cost guard on the operator's own home, not
// a fence against something racing it there.
func readBoundedFile(path string) ([]byte, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > maxProviderConfigBytes {
		return nil, fmt.Errorf("provider file %q is not a bounded regular file", filepath.Base(path))
	}
	return os.ReadFile(path)
}

func readProviderDefault(account providerAccount) providerDefault {
	var result providerDefault
	switch account.provider {
	case "claude_code":
		// claude reads its top-level "model" from the account settings file.
		// It has no configured reasoning effort the factory can name.
		result.source = filepath.Join(account.home, "settings.json")
		data, err := readBoundedFile(result.source)
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
		data, err := readBoundedFile(result.source)
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
