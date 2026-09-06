package changeworker

import (
	"bytes"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

func TestConfigRoundTripIsExactBoundedAndPrivate(t *testing.T) {
	want := configFixture(t)
	encoded, err := EncodeConfig(want)
	if err != nil {
		t.Fatal(err)
	}
	got, err := DecodeConfig(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("round trip changed config")
	}
	encoded[0] ^= 1
	if _, err := DecodeConfig(encoded); !errors.Is(err, ErrInvalidContract) {
		t.Fatalf("corrupt JSON: %v", err)
	}
	for _, value := range []any{want, Result{}} {
		formatted := fmt.Sprintf("%v %+v %#v", value, value, value)
		for _, sentinel := range []string{want.RuntimePath, want.FactoryctlExecutable, want.RepositoryRoot, string(want.ProviderTask)} {
			if strings.Contains(formatted, sentinel) {
				t.Fatalf("private value leaked: %q", formatted)
			}
		}
	}
}

func TestRetainedConfigRoundTripPreservesExactPublicationAuthority(t *testing.T) {
	want := configFixture(t)
	retained := resultFixture(t)
	want.Retained = &retained
	encoded, err := EncodeConfig(want)
	if err != nil {
		t.Fatal(err)
	}
	got, err := DecodeConfig(encoded)
	if err != nil || !reflect.DeepEqual(got, want) {
		t.Fatalf("retained config round trip changed authority: %v", err)
	}
	bad := want
	bad.Retained = &Result{}
	if _, err := EncodeConfig(bad); !errors.Is(err, ErrInvalidContract) {
		t.Fatalf("invalid retained authority encoded: %v", err)
	}
}

// An orchestrator's config names no Change: its names and retained result
// are empty, and a worker's may not be.
func TestOrchestratorConfigCarriesNoChange(t *testing.T) {
	config := configFixture(t)
	config.Role, config.FinalName, config.StagingName = kernel.RoleOrchestrator, "", ""
	encoded, err := EncodeConfig(config)
	if err != nil {
		t.Fatal(err)
	}
	got, err := DecodeConfig(encoded)
	if err != nil || !reflect.DeepEqual(got, config) {
		t.Fatalf("orchestrator config round trip = %v", err)
	}
	named := config
	named.FinalName = "published"
	retained := resultFixture(t)
	withChange := config
	withChange.Retained = &retained
	worker := configFixture(t)
	worker.FinalName = ""
	for name, bad := range map[string]Config{"orchestrator with a name": named, "orchestrator with a retained tree": withChange, "worker without a name": worker} {
		if _, err := EncodeConfig(bad); !errors.Is(err, ErrInvalidContract) {
			t.Fatalf("%s encoded: %v", name, err)
		}
	}
	noRole := bytes.Replace(encoded, []byte(`"role":"orchestrator"`), []byte(`"role":"overseer"`), 1)
	if _, err := DecodeConfig(noRole); !errors.Is(err, ErrInvalidContract) {
		t.Fatalf("unknown role decoded: %v", err)
	}
}

func TestCodexConfigCarriesNoTaskBytes(t *testing.T) {
	config := configFixture(t)
	config.Provider = kernel.ProviderCodex
	config.ProviderTask = nil
	encoded, err := EncodeConfig(config)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(encoded, []byte("printf exact")) {
		t.Fatal("Codex worker config contains private task text")
	}
	got, err := DecodeConfig(encoded)
	if err != nil || got.Provider != kernel.ProviderCodex || len(got.ProviderTask) != 0 {
		t.Fatalf("Codex config round trip = provider %s task %d bytes, err %v", got.Provider, len(got.ProviderTask), err)
	}
	config.ProviderTask = []byte("private task")
	if _, err := EncodeConfig(config); !errors.Is(err, ErrInvalidContract) {
		t.Fatalf("Codex worker accepted task bytes: %v", err)
	}
}

func TestConfigStrictJSONRejectsOversizeUnknownTrailingMissingAndInvalidProvider(t *testing.T) {
	want := configFixture(t)
	encoded, err := EncodeConfig(want)
	if err != nil {
		t.Fatal(err)
	}
	unknown := bytes.Replace(encoded, []byte{'{'}, []byte(`{"unknown":true,`), 1)
	invalidProvider := bytes.Replace(encoded, []byte(`"provider":"shell"`), []byte(`"provider":"unknown"`), 1)
	if bytes.Equal(invalidProvider, encoded) {
		t.Fatal("provider fixture was not replaced")
	}
	invalidPath := bytes.Replace(encoded, []byte(`"repository_root":"/private/repository"`), []byte(`"repository_root":"relative"`), 1)
	if bytes.Equal(invalidPath, encoded) {
		t.Fatal("path fixture was not replaced")
	}
	for name, value := range map[string][]byte{
		"oversize":         bytes.Repeat([]byte{' '}, ConfigLimit+1),
		"unknown field":    unknown,
		"trailing data":    append(bytes.Clone(encoded), []byte(`{}`)...),
		"missing required": []byte(`{}`),
		"invalid provider": invalidProvider,
		"invalid path":     invalidPath,
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := DecodeConfig(value); !errors.Is(err, ErrInvalidContract) {
				t.Fatalf("malformed config accepted: %v", err)
			}
		})
	}
}

func TestResultRoundTripIsStrictBoundedAndPrivate(t *testing.T) {
	want := resultFixture(t)
	encoded, err := EncodeResult(want)
	if err != nil {
		t.Fatal(err)
	}
	got, err := DecodeResult(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if got.Format != want.Format || got.Base.Hex() != want.Base.Hex() || !got.Commitment.Equal(want.Commitment) ||
		got.EntryCount != want.EntryCount || got.BlobBytes != want.BlobBytes || !got.Tree.Equal(want.Tree) {
		t.Fatal("round trip changed result")
	}
	formatted := fmt.Sprintf("%v %+v %#v", want, want, want)
	for _, private := range []string{want.Base.Hex(), want.Commitment.Hex()} {
		if strings.Contains(formatted, private) {
			t.Fatalf("private result leaked: %q", formatted)
		}
	}
}

func TestResultRejectsOversizeUnknownTrailingMissingInvalidAndPartialJSON(t *testing.T) {
	encoded, err := EncodeResult(resultFixture(t))
	if err != nil {
		t.Fatal(err)
	}
	unknown := bytes.Replace(encoded, []byte{'{'}, []byte(`{"unknown":true,`), 1)
	missingEntries := bytes.Replace(encoded, []byte(`,"entry_count":7`), nil, 1)
	if bytes.Equal(missingEntries, encoded) {
		t.Fatal("entry-count fixture was not removed")
	}
	invalidFormat := bytes.Replace(encoded, []byte(`"format":"sha1"`), []byte(`"format":"sha512"`), 1)
	if bytes.Equal(invalidFormat, encoded) {
		t.Fatal("format fixture was not replaced")
	}
	for name, value := range map[string][]byte{
		"oversize":         bytes.Repeat([]byte{' '}, ResultLimit+1),
		"unknown field":    unknown,
		"trailing data":    append(bytes.Clone(encoded), []byte(`{}`)...),
		"missing required": missingEntries,
		"invalid format":   invalidFormat,
		"partial":          bytes.Clone(encoded[:len(encoded)-1]),
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := DecodeResult(value); !errors.Is(err, ErrInvalidContract) {
				t.Fatalf("malformed result accepted: %v", err)
			}
		})
	}
}

func TestConfigRejectsMalformedRetainedChangeJSON(t *testing.T) {
	want := configFixture(t)
	result := resultFixture(t)
	want.Retained = &result
	encoded, err := EncodeConfig(want)
	if err != nil {
		t.Fatal(err)
	}
	malformed := bytes.Replace(encoded, []byte(`"tree":{"device":21,"inode":22}`), []byte(`"tree":{"device":21,"inode":0}`), 1)
	if bytes.Equal(malformed, encoded) {
		t.Fatal("retained tree fixture was not replaced")
	}
	if _, err := DecodeConfig(malformed); !errors.Is(err, ErrInvalidContract) {
		t.Fatalf("malformed retained Change accepted: %v", err)
	}
}

func TestConfigRejectsRawAuthorityAndInputCorruption(t *testing.T) {
	want := configFixture(t)
	mutations := []func(*Config){
		func(v *Config) { v.Provider = kernel.Provider(255) },
		func(v *Config) { v.Provider, v.Model = kernel.ProviderCodex, string([]byte{0xff}) },
		func(v *Config) { v.Provider, v.Model = kernel.ProviderCodex, "model\x00suffix" },
		func(v *Config) { v.ReasoningEffort = strings.Repeat("x", 33) },
		func(v *Config) { v.RuntimeIdentity = runner.FileIdentity{} },
		func(v *Config) { v.RepositoryRoot = "relative" },
		func(v *Config) { v.FactoryctlExecutable = "" },
		func(v *Config) { v.FactoryctlExecutable = "relative/factoryctl" },
		func(v *Config) { v.FactoryctlExecutable = "/private/../factoryctl" },
		func(v *Config) { v.FactoryctlExecutable = "/private/factoryctl\x00foreign" },
		func(v *Config) { v.FactoryctlExecutable = "/" + strings.Repeat("f", maximumLocatorBytes) },
		func(v *Config) { v.ToolPath = "relative:/usr/bin" },
		func(v *Config) { v.Provider, v.ReasoningEffort = kernel.ProviderCodex, "speculative" },
		func(v *Config) { v.FinalName = ".GiT" },
		func(v *Config) { v.StagingName = v.FinalName },
		func(v *Config) { v.AttemptSocket = "/" + strings.Repeat("s", install.MaxSocketPathBytes) },
		func(v *Config) { v.ProviderTask = []byte{0xff} },
		func(v *Config) { v.ProviderTask = []byte{'x', 0} },
		func(v *Config) { v.ProviderTask = nil },
		func(v *Config) { v.ProviderTask = make([]byte, runner.MaxProviderTaskBytes+1) },
	}
	for i, mutate := range mutations {
		bad := want
		bad.ProviderTask = bytes.Clone(want.ProviderTask)
		mutate(&bad)
		if _, err := EncodeConfig(bad); !errors.Is(err, ErrInvalidContract) {
			t.Fatalf("mutation %d accepted: %v", i, err)
		}
	}
	fields := reflect.VisibleFields(reflect.TypeOf(Config{}))
	for _, forbidden := range []string{"ProviderHome", "ProviderTemp", "AttemptTokenPath", "ProviderProgram"} {
		for _, field := range fields {
			if field.Name == forbidden {
				t.Fatalf("raw authority field retained: %s", forbidden)
			}
		}
	}
}

func configFixture(t testing.TB) Config {
	t.Helper()
	repository, err := change.NewRepositoryIdentity(11, 12)
	if err != nil {
		t.Fatal(err)
	}
	return Config{Provider: kernel.ProviderShell, Role: kernel.RoleWorker, RuntimePath: "/private/runtime", RuntimeIdentity: runner.FileIdentity{Device: 1, Inode: 2}, GitExecutable: "/Library/Developer/CommandLineTools/usr/bin/git", FactoryctlExecutable: "/private/release/factoryctl", ToolPath: "/opt/homebrew/bin:/usr/bin:/bin", AccountHome: "/private/account", RepositoryRoot: "/private/repository", RepositoryIdentity: repository, Revision: "main", ChangeParent: "/private/changes", FinalName: "change", StagingName: ".change.stage", AttemptSocket: "/private/api.sock", ProviderTask: []byte("printf exact")}
}

func resultFixture(t testing.TB) Result {
	t.Helper()
	format, err := change.NewObjectFormat("sha1")
	if err != nil {
		t.Fatal(err)
	}
	base, err := change.NewObjectID(format, bytes.Repeat([]byte{1}, format.OIDLength()))
	if err != nil {
		t.Fatal(err)
	}
	commitment, err := change.ParseCommitment(bytes.Repeat([]byte{7}, 32))
	if err != nil {
		t.Fatal(err)
	}
	return Result{
		Format: format, Base: base, Commitment: commitment,
		EntryCount: 7, BlobBytes: 99, Tree: mustStage(t, 21, 22),
	}
}

func mustStage(t testing.TB, device, inode uint64) change.StageIdentity {
	t.Helper()
	result, err := change.NewStageIdentity(device, inode)
	if err != nil {
		t.Fatal(err)
	}
	return result
}
