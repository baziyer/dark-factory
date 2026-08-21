# Adding a provider

A provider is a small adapter that describes how to launch one supported
coding-agent CLI for one admitted run. Claude Code, Codex, and the deterministic
shell adapter implement the same boundary.

> The safe-kernel refactor is frozen for live use until Stage 3 and the boot
> review. Provider tests use temporary directories and fake/shell processes;
> do not send a real paid prompt while developing it.

## Contract

The complete trait is in `crates/factoryd/src/providers/mod.rs`:

```rust
pub trait Provider {
    fn spawn_spec(&self, ctx: &SpawnContext)
        -> Result<ProviderLaunch, ProviderError>;
}
```

`SpawnContext` is created by `factoryd`. It contains:

- the exact `RunId` that owns this process;
- the daemon-selected source directory;
- one `startup_input` byte string;
- resolved model and reasoning settings plus one typed execution mode frozen by
  admission;
- private paths for the attempt bearer and generated configuration; and
- the trusted absolute `factoryctl` path and exact daemon socket.

`ProviderLaunch` returns one executable, argv, provider-specific environment
additions, the same startup input, generated configuration paths, and runtime
metadata actually established by that launch.

The runner writes `startup_input` once to the process stdin and closes stdin.
The provider process exits when that one run ends. There is no interactive PTY
contract, later `send_input`, terminal attach, delivery acknowledgement,
message-only turn, resident process, or provider-process resume.

Provider adapters never:

- create, choose, infer, or delete source paths;
- spawn or reap their own process;
- parse output to infer lifecycle or success;
- grant capabilities or make a run terminal;
- copy attempt bearer values into argv or environment; or
- add a provider-specific local API or repository path.

## Shipped launch shapes

The launch is always fresh and non-interactive:

- Claude Code: `claude -p --session-id <RunId>`, with the startup input on
  stdin. The run UUID is a fresh Claude conversation identity; `--resume` is
  never used.
- Codex: `codex exec ... -`, with the startup input on stdin. `CODEX_HOME`
  points at daemon-generated bounded configuration; no `codex resume` path
  exists.
- Shell: `sh -s`, or `sh -lc <configured-fixture-command>`, receiving the same
  startup input. This is the deterministic reference adapter.

Dispatch is not a provider input. It controls only whether the Store may admit
another attempt. Each admitted run instead freezes one `ExecutionMode`:

| Mode | Codex | Claude Code | Shell |
|---|---|---|---|
| `PlanOnly` | named profile extending `:read-only`, `approval_policy="never"`, and command network limited to the exact daemon socket | conservatively restricted to the supported macOS product runtime; `--permission-mode dontAsk`, read tools plus only the two `factoryctl task` outcome commands, with all write tools denied | unsupported |
| `WorkspaceWrite` | named profile extending `:workspace`, both system-temp roots denied, `approval_policy="never"`, and command network limited to the exact daemon socket | macOS-only because exact AF_UNIX sandbox policy is required; `--permission-mode dontAsk`, exact `Edit(./**)` rule anchored by the Change working directory, and native sandboxing that fails if unavailable or asked to retry unsandboxed | unsupported |
| `Unrestricted` | `--dangerously-bypass-approvals-and-sandbox` | `--permission-mode bypassPermissions` | the only honest shell mode |

Codex and Claude profiles default to `WorkspaceWrite`; shell defaults to its
only supported mode. Claude always uses `-p`, ignores user/project setting
sources, and uses strict MCP configuration. Codex always uses `exec
--strict-config`, disables interactive approvals for bounded modes, and reads
the one task from stdin.
No advertised mode can wait for an unanswered native approval prompt.

Bounded Codex modes require Codex CLI 0.138.0 or later. They pass
`--enable network_proxy`, so a client that does not recognize the required
enforcement feature exits before `exec` rather than silently ignoring the
socket-only policy. Adapter tests validate the complete generated profile with
an installed Codex metadata command and never send a prompt.

Claude `WorkspaceWrite` uses a native Bash sandbox that writes to the Change
and one provider-created per-session temp directory; the latter is ephemeral
runtime scratch, not durable product source. Its exact AF_UNIX allowlist is
enforced by Claude only on macOS, so `WorkspaceWrite` fails before launch on
other platforms. `PlanOnly` has no sandbox stanza and technically does not
depend on that allowlist, but is conservatively restricted to the supported
macOS product runtime to avoid a second platform claim. The explicit
`Unrestricted` mode remains available elsewhere. Linux remains a source/test
lane rather than an advertised daemon runtime.

At daemon startup, Dark Factory resolves one canonical Claude executable,
requires the reviewed exact `2.1.236 (Claude Code)` version, and passes every
generated settings shape through the metadata-only `doctor` command with a
fixed non-colour `C` locale. Claude reports invalid settings while still
exiting zero, so the daemon also rejects its `Invalid settings` diagnostic. No
prompt is submitted. Missing, unreviewed, or invalid Claude is marked
unavailable: the daemon can still serve Codex and Shell, but a Claude attempt
fails explicitly. Attempts reuse the validated executable identity and fail
before launch if an auto-update or replacement changed it; the next Claude
version must be reviewed and allow-listed deliberately.

Model and reasoning flags are explicit when configured. Missing runtime
metadata remains `None`; never invent a plausible provider default.

## Hooks and attempt authority

Generated hook commands use the absolute `factoryctl` path and
`--token-file <private path>`. The bearer is read from its `0600` file when the
hook runs, never embedded in generated text, argv, or environment.

The generic runner also sets `DARK_FACTORY_ATTEMPT_TOKEN_FILE` to that private
file path. The path is not a secret substitute for the bearer; its file remains
owner-only. While this variable is present, `factoryctl` authenticates every
request with the ambient attempt credential. This includes operator-shaped
commands: the daemon evaluates them against the exact attempt allowlist and
refuses them instead of loading the operator credential.

The daemon resolves the bearer to one exact attempt principal and accepts
attempt operations only while that run is `running`. It derives project,
agent, task, run, role, provider, and source scope from durable state. A stale,
forged, taskless, finalizing, or terminal credential fails closed.

Provider hook names come from the upstream CLIs and may contain the word
“Session”; they are observations within one run, not Dark Factory session
lifecycle states. `PreToolUse` enforces the daemon policy and budget before a
tool call. It also refuses direct `cargo`, `rustc`, and `rustup` invocation and
direct execution from a recognized mutable Cargo
`target/.../{debug,release}` path: providers do not own Rust build paths or
mutable Cargo outputs. Completion and blocking use the same bearer and derive
the current task from it. On a project configured for `RustWorkspaceTest`,
`factoryctl task done` first causes the transition to `finalizing`; factoryd
reaps the provider and owns the fixed verification before it can terminalize.
There is no generic provider build API, Cargo shim, or provider-selected build
configuration. The fixed verifier excludes doctests and launches only copied
top-level Cargo test executables; it is not an OS sandbox for test code. No
hook can directly terminalize a run.

Hook policy is a tripwire, not an OS sandbox. A provider runs as the operator's
user and can bypass string-level policy through other execution paths. See
[`SECURITY.md`](../SECURITY.md).

Provider startup guidance directs workers to edit their Change and finish with
`factoryctl task done --result <summary>`. It must not tell them to run a Rust
toolchain first: the configured completion policy is the authoritative
verification and runs only after their process has been reaped. Non-Rust
projects configured with `None` keep the ordinary completion path;
orchestrators are not workspace-test subjects.

## Generated configuration

Keep generated files private and limited to what the provider needs for this
launch.

- Claude receives a per-run settings file containing daemon-authored hooks and
  the exact permissions/sandbox settings for the frozen execution mode.
  Dark Factory does not edit the operator's `~/.claude.json`.
- Codex uses an isolated generated home. The seed retains only the operator's
  `auth.json` link, not ambient configuration. Model and reasoning selection
  comes from the admitted agent profile; custom model-provider tables and
  their command-backed authentication cannot enter the attempt.
  It excludes every table, including custom model-provider tables whose
  authentication may execute a helper command, plus every ambient rule,
  profile, MCP, project-trust, hook, permission, network-feature, and legacy
  sandbox setting. Dark Factory then writes its own hook and source-trust
  settings and supplies the exact frozen permission profile at launch. Ambient
  authority is removed, not merged or overridden by precedence.
- Provider environment additions must not duplicate the runner's generic
  sanitized environment or expose repository credentials.

Generated configuration is a registered run resource. Rust `Drop` or a test
temporary-directory destructor is not its cleanup authority; the daemon
finalizer must release and acknowledge it durably.

## Adding an adapter

1. Add `crates/factoryd/src/providers/<name>.rs` and register the provider in
   `providers/mod.rs` and the shared provider enum.
2. Implement `spawn_spec` as a pure launch description plus the smallest
   necessary private configuration writes.
3. Add the provider's model/reasoning policy to the shared model-policy module
   and define which typed execution modes it can truthfully enforce. Validation
   happens before a future launch.
4. Add focused tests proving the exact executable/argv for every supported
   execution mode, one unchanged startup
   input, private generated files, sanitized environment additions, and no
   resume or caller-selected source path.
5. Exercise lifecycle behavior through the generic prepare/activate runner
   tests. Do not add provider-specific supervision.
6. Update this guide and run the authoritative local gate.

If a provider needs a second lifecycle, output decoder, interactive terminal,
or custom authority path, it does not fit this interface. Challenge the
requirement instead of widening the kernel.

## Source and repository boundary

Before worker execution, the daemon materializes one exact committed tree into
the attempt's leased Change. The provider receives that plain writable source
view with no Git administrative locator. Status, diff, commit, push,
pull-request, and publication operations do not exist in the product. A
provider adapter must not run `git worktree`, accept a caller-selected source
argument, or add another credential route.

## Testing

Use `ShellProvider` for end-to-end daemon tests and `fake-agent` for lower-level
process behavior. All fixtures use a temporary `DARK_FACTORY_HOME`, explicit
socket, disposable paths, and an independent post-test verifier. A crash test
must prove the resource ledger/finalizer converges after restart; a passing
destructor is not evidence.

Run focused tests through the repository CI lease, then `./scripts/local-ci.sh`.
Real Claude and Codex runs are reserved for an explicit provider-validation
task after the complete safe-kernel boot gate.
