package changeworker

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"path/filepath"
	"strings"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

const (
	AttemptTokenName     = "attempt.token"
	HomeName             = "home"
	TempName             = "tmp"
	ResultLimit          = 32 << 10
	ConfigLimit          = 256 << 10
	maximumLocatorBytes  = 4096
	maximumRevisionBytes = 4096
)

var ErrInvalidContract = errors.New("Change worker: invalid private contract")

type Config struct {
	Provider             kernel.Provider
	Model                string
	ReasoningEffort      string
	RuntimePath          string
	RuntimeIdentity      runner.FileIdentity
	GitExecutable        string
	FactoryctlExecutable string
	ToolPath             string
	AccountHome          string
	// AccountConfigDir is the linked provider login this run launches with.
	// Empty means the provider's own default configuration directory.
	AccountConfigDir   string
	RepositoryRoot     string
	RepositoryIdentity change.RepositoryIdentity
	Revision           string
	ChangeParent       string
	FinalName          string
	StagingName        string
	AttemptSocket      string
	Retained           *Result
	// ProviderTask selects and verifies the provider's closed delivery path.
	// Shell seals it on fd 11 and Claude receives a terminal-safe prompt. It is
	// empty for Codex, whose task remains in the daemon behind the attempt API.
	ProviderTask []byte
}

func (Config) String() string   { return "Change worker config (private)" }
func (Config) GoString() string { return "changeworker.Config{private}" }

// Result binds selected content to one prepared or retained tree. Repository
// identity is absent because the daemon owns and independently verifies it.
type Result struct {
	Format     change.ObjectFormat
	Base       change.ObjectID
	Commitment change.Commitment
	EntryCount uint64
	BlobBytes  uint64
	Tree       change.StageIdentity
}

func (Result) String() string   { return "Change worker result (private)" }
func (Result) GoString() string { return "changeworker.Result{private}" }

type identityWire struct {
	Device uint64 `json:"device"`
	Inode  uint64 `json:"inode"`
}

type resultWire struct {
	Format     string       `json:"format"`
	Base       string       `json:"base"`
	Commitment string       `json:"commitment"`
	EntryCount *uint64      `json:"entry_count"`
	BlobBytes  *uint64      `json:"blob_bytes"`
	Tree       identityWire `json:"tree"`
}

type configWire struct {
	Provider             string       `json:"provider"`
	Model                string       `json:"model"`
	ReasoningEffort      string       `json:"reasoning_effort"`
	RuntimePath          string       `json:"runtime_path"`
	RuntimeIdentity      identityWire `json:"runtime_identity"`
	GitExecutable        string       `json:"git_executable"`
	FactoryctlExecutable string       `json:"factoryctl_executable"`
	ToolPath             string       `json:"tool_path"`
	AccountHome          string       `json:"account_home"`
	AccountConfigDir     string       `json:"account_config_dir"`
	RepositoryRoot       string       `json:"repository_root"`
	RepositoryIdentity   identityWire `json:"repository_identity"`
	Revision             string       `json:"revision"`
	ChangeParent         string       `json:"change_parent"`
	FinalName            string       `json:"final_name"`
	StagingName          string       `json:"staging_name"`
	AttemptSocket        string       `json:"attempt_socket"`
	Retained             *resultWire  `json:"retained,omitempty"`
	ProviderTask         []byte       `json:"provider_task"`
}

func EncodeConfig(config Config) ([]byte, error) {
	if err := validateConfig(config); err != nil {
		return nil, err
	}
	wire := configWire{
		Provider: config.Provider.String(), Model: config.Model, ReasoningEffort: config.ReasoningEffort,
		RuntimePath: config.RuntimePath, RuntimeIdentity: identityWire{Device: config.RuntimeIdentity.Device, Inode: config.RuntimeIdentity.Inode},
		GitExecutable: config.GitExecutable, FactoryctlExecutable: config.FactoryctlExecutable, ToolPath: config.ToolPath, AccountHome: config.AccountHome, AccountConfigDir: config.AccountConfigDir,
		RepositoryRoot: config.RepositoryRoot, RepositoryIdentity: identityWire{Device: config.RepositoryIdentity.Device(), Inode: config.RepositoryIdentity.Inode()}, Revision: config.Revision,
		ChangeParent: config.ChangeParent, FinalName: config.FinalName, StagingName: config.StagingName,
		AttemptSocket: config.AttemptSocket, ProviderTask: bytes.Clone(config.ProviderTask),
	}
	if config.Retained != nil {
		retained := resultToWire(*config.Retained)
		wire.Retained = &retained
	}
	return encodeJSON(wire, ConfigLimit)
}

func DecodeConfig(encoded []byte) (Config, error) {
	var wire configWire
	if err := decodeJSON(encoded, ConfigLimit, &wire); err != nil {
		return Config{}, err
	}
	providerKind, err := providerFromString(wire.Provider)
	if err != nil {
		return Config{}, err
	}
	repositoryIdentity, err := change.NewRepositoryIdentity(wire.RepositoryIdentity.Device, wire.RepositoryIdentity.Inode)
	if err != nil {
		return Config{}, invalidContract(err)
	}
	var retained *Result
	if wire.Retained != nil {
		result, resultErr := resultFromWire(*wire.Retained)
		if resultErr != nil {
			return Config{}, resultErr
		}
		retained = &result
	}
	config := Config{
		Provider: providerKind, Model: wire.Model, ReasoningEffort: wire.ReasoningEffort,
		RuntimePath: wire.RuntimePath, RuntimeIdentity: runner.FileIdentity{Device: wire.RuntimeIdentity.Device, Inode: wire.RuntimeIdentity.Inode},
		GitExecutable: wire.GitExecutable, FactoryctlExecutable: wire.FactoryctlExecutable, ToolPath: wire.ToolPath, AccountHome: wire.AccountHome, AccountConfigDir: wire.AccountConfigDir,
		RepositoryRoot: wire.RepositoryRoot, RepositoryIdentity: repositoryIdentity, Revision: wire.Revision,
		ChangeParent: wire.ChangeParent, FinalName: wire.FinalName, StagingName: wire.StagingName,
		AttemptSocket: wire.AttemptSocket, Retained: retained, ProviderTask: bytes.Clone(wire.ProviderTask),
	}
	if err := validateConfig(config); err != nil {
		return Config{}, err
	}
	return config, nil
}

func validateConfig(config Config) error {
	paths := []string{config.RuntimePath, config.GitExecutable, config.FactoryctlExecutable, config.AccountHome, config.RepositoryRoot, config.ChangeParent, config.AttemptSocket}
	for _, path := range paths {
		if !validAbsolute(path, maximumLocatorBytes) {
			return invalidContract(nil)
		}
	}
	if config.AccountConfigDir != "" && !validAbsolute(config.AccountConfigDir, maximumLocatorBytes) {
		return invalidContract(nil)
	}
	if len(config.AttemptSocket) > install.MaxSocketPathBytes || config.RuntimeIdentity.Device == 0 || config.RuntimeIdentity.Inode == 0 ||
		kernel.ValidateProviderLaunchControls(config.Provider, config.Model, config.ReasoningEffort) != nil || provider.ValidateToolPath(config.ToolPath) != nil ||
		!validText(config.Revision, maximumRevisionBytes) || !validChangeName(config.FinalName) || !validChangeName(config.StagingName) ||
		config.FinalName == config.StagingName {
		return invalidContract(nil)
	}
	if _, _, err := prepareProviderTask(config.Provider, config.ProviderTask); err != nil {
		return invalidContract(err)
	}
	if _, err := change.NewRepositoryIdentity(config.RepositoryIdentity.Device(), config.RepositoryIdentity.Inode()); err != nil {
		return invalidContract(err)
	}
	if config.Retained != nil {
		if validateResult(*config.Retained) != nil {
			return invalidContract(nil)
		}
	}
	return nil
}

func prepareProviderTask(kind kernel.Provider, task []byte) (provider.TaskDelivery, []byte, error) {
	if kind == kernel.ProviderCodex {
		if len(task) != 0 {
			return 0, nil, provider.ErrInvalid
		}
		return provider.TaskDeliveryAttemptAPI, nil, nil
	}
	return provider.PrepareTask(kind, task)
}

func providerFromString(value string) (kernel.Provider, error) {
	for _, candidate := range []kernel.Provider{kernel.ProviderShell, kernel.ProviderClaudeCode, kernel.ProviderCodex} {
		if value == candidate.String() {
			return candidate, nil
		}
	}
	return 0, invalidContract(nil)
}

func EncodeResult(result Result) ([]byte, error) {
	if err := validateResult(result); err != nil {
		return nil, err
	}
	return encodeJSON(resultToWire(result), ResultLimit)
}

func DecodeResult(encoded []byte) (Result, error) {
	var wire resultWire
	if err := decodeJSON(encoded, ResultLimit, &wire); err != nil {
		return Result{}, err
	}
	return resultFromWire(wire)
}

func validateResult(result Result) error {
	if result.Format.OIDLength() == 0 || result.Base.Format() != result.Format || len(result.Base.Bytes()) != result.Format.OIDLength() ||
		len(result.Commitment.Bytes()) != 32 || result.EntryCount > change.MaxEntryCount || result.BlobBytes > change.MaxTotalBlobBytes {
		return invalidContract(nil)
	}
	if _, err := change.NewStageIdentity(result.Tree.Device(), result.Tree.Inode()); err != nil {
		return invalidContract(err)
	}
	return nil
}

func resultToWire(result Result) resultWire {
	entries, blobs := result.EntryCount, result.BlobBytes
	return resultWire{
		Format: result.Format.Name(), Base: result.Base.Hex(), Commitment: result.Commitment.Hex(),
		EntryCount: &entries, BlobBytes: &blobs,
		Tree: identityWire{Device: result.Tree.Device(), Inode: result.Tree.Inode()},
	}
}

func resultFromWire(wire resultWire) (Result, error) {
	if wire.EntryCount == nil || wire.BlobBytes == nil {
		return Result{}, invalidContract(nil)
	}
	format, err := change.NewObjectFormat(wire.Format)
	if err != nil {
		return Result{}, invalidContract(err)
	}
	baseBytes, err := hex.DecodeString(wire.Base)
	if err != nil {
		return Result{}, invalidContract(err)
	}
	base, err := change.NewObjectID(format, baseBytes)
	if err != nil {
		return Result{}, invalidContract(err)
	}
	commitmentBytes, err := hex.DecodeString(wire.Commitment)
	if err != nil {
		return Result{}, invalidContract(err)
	}
	commitment, err := change.ParseCommitment(commitmentBytes)
	if err != nil {
		return Result{}, invalidContract(err)
	}
	tree, err := change.NewStageIdentity(wire.Tree.Device, wire.Tree.Inode)
	if err != nil {
		return Result{}, invalidContract(err)
	}
	result := Result{
		Format: format, Base: base, Commitment: commitment,
		EntryCount: *wire.EntryCount, BlobBytes: *wire.BlobBytes, Tree: tree,
	}
	if err := validateResult(result); err != nil {
		return Result{}, err
	}
	return result, nil
}

func encodeJSON(value any, maximum int) ([]byte, error) {
	encoded, err := json.Marshal(value)
	if err != nil || len(encoded) == 0 || len(encoded) > maximum {
		return nil, invalidContract(err)
	}
	return encoded, nil
}

func decodeJSON(encoded []byte, maximum int, value any) error {
	if len(encoded) == 0 || len(encoded) > maximum || value == nil {
		return invalidContract(nil)
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(value); err != nil {
		return invalidContract(err)
	}
	var trailing json.RawMessage
	if err := decoder.Decode(&trailing); err != io.EOF {
		return invalidContract(err)
	}
	return nil
}

func validText(value string, maximum int) bool {
	return value != "" && len(value) <= maximum && utf8.ValidString(value) && !strings.ContainsRune(value, 0)
}

func validAbsolute(value string, maximum int) bool {
	return validText(value, maximum) && filepath.IsAbs(value) && filepath.Clean(value) == value && value != string(filepath.Separator)
}

func validChangeName(value string) bool {
	return validText(value, 255) && filepath.Base(value) == value && value != "." && value != ".." && !strings.EqualFold(value, ".git")
}

func invalidContract(error) error { return ErrInvalidContract }
