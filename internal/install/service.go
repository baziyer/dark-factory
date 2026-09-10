package install

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

// DefaultServiceLabel is the production launchd label. Every test and E2E
// uses an explicit disposable label instead; the default is never implied
// for mutation by anything but the operator-facing factoryctl defaults.
const DefaultServiceLabel = "com.dark-factory.factoryd"

const serviceLabel = DefaultServiceLabel

const serviceStderrLogName = "factoryd.stderr.log"

const (
	serviceMaxPathBytes  = 4096
	serviceMaxLabelBytes = 127
	serviceMaxPlistBytes = 4*serviceMaxPathBytes*6 + MaxRelayOriginBytes*6 + 4096
)

// MaxRelayOriginBytes is the one bound on ServiceConfig.RelayOrigin, shared
// with the factoryctl flag that supplies it.
const MaxRelayOriginBytes = 2048

var (
	ErrServiceAmbiguous = errors.New("service ownership is ambiguous")
	ErrServiceLaunchctl = errors.New("launchctl result is not authoritative")
	ErrServicePlist     = errors.New("service plist is invalid")
	ErrServiceReceipt   = errors.New("service receipt is invalid")
	ErrServiceForeign   = errors.New("service artifact is not this installation's property")
	ErrServiceResidue   = errors.New("service installation residue requires uninstall")

	// ErrServiceRelayOrigin is the one refusal factoryctl prints verbatim, so
	// it is assembled only from this engine's own words and the origin its own
	// receipt recorded; no platform diagnostic is ever joined onto it.
	ErrServiceRelayOrigin = fmt.Errorf("%w: the installed service uses a different relay origin", ErrServiceForeign)
)

// ServiceState is the bounded read-only projection returned by factoryctl.
// Absence is provable directly; present states are provable only through the
// durable install receipt written by ServiceInstall — a present external fact
// with no matching receipt remains ambiguous.
type ServiceState string

const (
	ServiceAbsent    ServiceState = "absent"
	ServiceAmbiguous ServiceState = "ambiguous"
	ServiceInstalled ServiceState = "installed"
	ServiceRunning   ServiceState = "running"
)

// ServiceConfig selects the launchd label and the directory holding the
// rendered plist. The zero PlistDirectory means the account's
// ~/Library/LaunchAgents; tests and the isolated E2E pass explicit temporary
// values so the operator's launchd artifacts are never touched.
type ServiceConfig struct {
	Label          string
	PlistDirectory string
	// RelayOrigin, when set, adds --relay-origin to the installed job's
	// arguments so the factory dials the hosted relay. It is read by
	// ServiceInstall only: every other verb recovers the origin from the
	// receipt, because uninstall and status are invoked without the flag.
	RelayOrigin string
}

// DefaultServiceConfig is the production configuration.
func DefaultServiceConfig() ServiceConfig { return ServiceConfig{Label: DefaultServiceLabel} }

func (config ServiceConfig) valid() bool {
	if !validServiceLabel(config.Label) || !validServiceRelayOrigin(config.RelayOrigin) {
		return false
	}
	return config.PlistDirectory == "" || validServicePath(config.PlistDirectory)
}

func (config ServiceConfig) plistName() string { return config.Label + ".plist" }

func validServiceLabel(label string) bool {
	if label == "" || len(label) > serviceMaxLabelBytes || label[0] == '.' || label[len(label)-1] == '.' || strings.Contains(label, "..") {
		return false
	}
	for _, character := range label {
		switch {
		case character >= 'a' && character <= 'z':
		case character >= 'A' && character <= 'Z':
		case character >= '0' && character <= '9':
		case character == '.' || character == '-':
		default:
			return false
		}
	}
	return true
}

// ServiceDirectoryPath is the sibling directory that owns every managed
// service artifact (binaries and the receipt). It is deliberately outside the
// home so the home census contract is untouched by installation.
func ServiceDirectoryPath(home string) string { return home + ".service" }

func serviceProgramPath(home string) string {
	return filepath.Join(ServiceDirectoryPath(home), "bin", "current", "factoryd")
}

func serviceStderrPath(home string) string {
	return filepath.Join(ServiceDirectoryPath(home), serviceStderrLogName)
}

// ServiceStatus contains no home, executable, plist, socket, token, or
// launchctl diagnostic text. PID is reported only after a strict launchctl
// parse and does not grant process authority.
type ServiceStatus struct {
	State ServiceState `json:"state"`
	PID   int          `json:"pid,omitempty"`
}

// InspectService is a strictly read-only service projection. It never opens a
// mutation lock, recovers pending state, writes installation metadata, or
// invokes a mutating launchctl verb.
func InspectService(ctx context.Context, home string) (ServiceStatus, error) {
	return InspectServiceWithConfig(ctx, home, DefaultServiceConfig())
}

// InspectServiceWithConfig is InspectService for an explicit label and plist
// directory (isolation surface for tests and the service E2E).
func InspectServiceWithConfig(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	return inspectServiceForAccount(ctx, home, config, runLaunchctl)
}

// ServiceInstall places the managed launchd job for one Go home: binaries and
// receipt in the sibling service directory, the rendered plist, and one
// launchctl bootstrap. sourceDir names the directory holding the factoryd,
// factoryctl, and factory-runner binaries to install (normally the invoking
// factoryctl's own directory). Repeating an exact install is recognized, a
// foreign artifact refuses, and crash residue resolves through
// ServiceUninstall.
func ServiceInstall(ctx context.Context, home string, config ServiceConfig, sourceDir string) (ServiceStatus, error) {
	return serviceInstall(ctx, home, config, sourceDir)
}

// ServiceStart bootstraps an installed-but-unloaded service.
func ServiceStart(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	return serviceStart(ctx, home, config)
}

// ServiceStop boots the job out of launchd while keeping the installation.
func ServiceStop(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	return serviceStop(ctx, home, config)
}

// ServiceUninstall removes exactly this installation's artifacts and is the
// resolution path for crash residue, including this engine's own stage
// files. It is evidence-first: no mutating launchctl verb runs until a
// matching receipt or an exactly rendered plist proves the label maps to
// this home. It never deletes foreign bytes.
func ServiceUninstall(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	return serviceUninstall(ctx, home, config)
}

// ServicePlist renders the one Go-v1 launchd job. Exact byte comparison is the
// parser: accepting a plist means accepting precisely this finite allowlist.
// An empty relayOrigin renders the job with no relay argument at all.
func ServicePlist(home, label, relayOrigin string) ([]byte, [sha256.Size]byte, error) {
	if !validServicePath(home) || filepath.Base(home) == string(filepath.Separator) {
		return nil, [sha256.Size]byte{}, fmt.Errorf("%w: home path", ErrServicePlist)
	}
	if !validServiceLabel(label) {
		return nil, [sha256.Size]byte{}, fmt.Errorf("%w: label", ErrServicePlist)
	}
	if !validServiceRelayOrigin(relayOrigin) {
		return nil, [sha256.Size]byte{}, fmt.Errorf("%w: relay origin", ErrServicePlist)
	}
	program := serviceProgramPath(home)
	stderrPath := serviceStderrPath(home)
	var escapedHome, escapedProgram, escapedStderr bytes.Buffer
	escapeXML(&escapedHome, home)
	escapeXML(&escapedProgram, program)
	escapeXML(&escapedStderr, stderrPath)
	relay := ""
	if relayOrigin != "" {
		var escapedRelay bytes.Buffer
		escapeXML(&escapedRelay, relayOrigin)
		relay = "\n        <string>--relay-origin</string>\n        <string>" + escapedRelay.String() + "</string>"
	}
	body := []byte(`<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>` + label + `</string>
    <key>ProgramArguments</key>
    <array>
        <string>` + escapedProgram.String() + `</string>
        <string>--home</string>
        <string>` + escapedHome.String() + `</string>` + relay + `
    </array>
    <key>WorkingDirectory</key>
    <string>` + escapedHome.String() + `</string>
    <key>StandardErrorPath</key>
    <string>` + escapedStderr.String() + `</string>
    <key>RunAtLoad</key>
    <true/>
    <key>AbandonProcessGroup</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>Umask</key>
    <integer>63</integer>
</dict>
</plist>
`)
	return body, sha256.Sum256(body), nil
}

// serviceReceipt is the durable, canonical record ServiceInstall writes into
// the service directory. Present service states are provable only through it:
// launchd facts that do not match an exact receipt stay ambiguous.
type serviceReceipt struct {
	Version       int    `json:"version"`
	Label         string `json:"label"`
	PlistPath     string `json:"plist_path"`
	PlistDigest   string `json:"plist_digest"`
	ProgramDigest string `json:"program_digest"`
	// RelayOrigin is the exact --relay-origin argument the installed plist
	// was rendered with. It is last and omitted when empty so a receipt
	// written before this member existed still round-trips byte for byte.
	RelayOrigin string `json:"relay_origin,omitempty"`
}

const (
	serviceReceiptName     = "receipt"
	serviceReceiptVersion  = 1
	serviceReceiptMaxBytes = 8192
)

func (receipt serviceReceipt) valid() bool {
	return receipt.Version == serviceReceiptVersion && validServiceLabel(receipt.Label) &&
		validServicePath(receipt.PlistPath) && filepath.Base(receipt.PlistPath) == receipt.Label+".plist" &&
		validDigestHex(receipt.PlistDigest) && validDigestHex(receipt.ProgramDigest) &&
		validServiceRelayOrigin(receipt.RelayOrigin)
}

func validDigestHex(value string) bool {
	if len(value) != sha256.Size*2 {
		return false
	}
	for _, character := range value {
		if (character < '0' || character > '9') && (character < 'a' || character > 'f') {
			return false
		}
	}
	return true
}

func encodeServiceReceipt(receipt serviceReceipt) ([]byte, error) {
	if !receipt.valid() {
		return nil, fmt.Errorf("%w: fields", ErrServiceReceipt)
	}
	body, err := json.Marshal(receipt)
	if err != nil || len(body) > serviceReceiptMaxBytes-1 {
		return nil, fmt.Errorf("%w: encoding", ErrServiceReceipt)
	}
	return append(body, '\n'), nil
}

func parseServiceReceipt(body []byte) (serviceReceipt, error) {
	if len(body) == 0 || len(body) > serviceReceiptMaxBytes {
		return serviceReceipt{}, fmt.Errorf("%w: size", ErrServiceReceipt)
	}
	var receipt serviceReceipt
	if err := json.Unmarshal(body, &receipt); err != nil {
		return serviceReceipt{}, fmt.Errorf("%w: syntax", ErrServiceReceipt)
	}
	canonical, err := encodeServiceReceipt(receipt)
	if err != nil || !bytes.Equal(body, canonical) {
		return serviceReceipt{}, fmt.Errorf("%w: not canonical", ErrServiceReceipt)
	}
	return receipt, nil
}

// ValidRelayOrigin is the one relay-origin grammar in the tree: a bounded,
// exact wss:// or ws:// origin. relayhost and the factoryctl flag both defer
// to it, and it lives here because relayhost imports this package. A path,
// query, fragment or credential is refused because the connector appends its
// own host path, and because a plist rendered from one would crash-loop.
func ValidRelayOrigin(value string) bool {
	if value == "" || len(value) > MaxRelayOriginBytes || !utf8.ValidString(value) {
		return false
	}
	parsed, err := url.Parse(value)
	return err == nil && (parsed.Scheme == "ws" || parsed.Scheme == "wss") && parsed.Host != "" &&
		parsed.User == nil && parsed.Path == "" && parsed.RawQuery == "" && parsed.Fragment == "" && parsed.String() == value
}

// validServiceRelayOrigin additionally accepts the empty origin, which renders
// a job with no relay argument at all.
func validServiceRelayOrigin(value string) bool { return value == "" || ValidRelayOrigin(value) }

func validServicePath(value string) bool {
	return value != "" && len(value) <= serviceMaxPathBytes && utf8.ValidString(value) && filepath.IsAbs(value) && filepath.Clean(value) == value && validXMLText(value) && !strings.ContainsRune(value, 0)
}

func validXMLText(value string) bool {
	for _, character := range value {
		if character < 0x20 && character != '\t' && character != '\n' && character != '\r' {
			return false
		}
	}
	return true
}

func escapeXML(destination *bytes.Buffer, value string) {
	for _, character := range value {
		switch character {
		case '&':
			destination.WriteString("&amp;")
		case '<':
			destination.WriteString("&lt;")
		case '>':
			destination.WriteString("&gt;")
		case '"':
			destination.WriteString("&quot;")
		case '\'':
			destination.WriteString("&apos;")
		default:
			destination.WriteRune(character)
		}
	}
}
