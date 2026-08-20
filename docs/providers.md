# Adding a provider

A provider is a small adapter that describes how to launch one supported
coding-agent CLI for one admitted run. Claude Code, Codex, and the deterministic
shell adapter implement the same boundary.

> Stage 1 is frozen for live use and deliberately refuses worker admission
> until daemon-owned Changes arrive in Stage 2. Provider tests use temporary
> directories and fake/shell processes; do not send a real paid prompt while
> developing this refactor.

## Contract

The complete trait is in `crates/factoryd/src/providers/mod.rs`:

```rust
pub trait Provider {
    fn spawn_spec(&self, ctx: &SpawnContext)
        -> Result<ProviderLaunch, ProviderError>;
    fn capabilities(&self) -> Capabilities;
}
```

`SpawnContext` is created by `factoryd`. It contains:

- the exact `RunId` that owns this process;
- the daemon-selected source directory;
- one `startup_input` byte string;
- resolved model, reasoning, permission, and auto-mode settings;
- private paths for the attempt bearer and generated configuration; and
- the trusted absolute `factoryctl` path.

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

Model and permission flags are explicit when configured. Missing runtime
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
tool call. Completion and blocking use the same bearer and derive the current
task from it. No hook can directly terminalize a run; it can only cause the
first transition to `finalizing`.

Hook policy is a tripwire, not an OS sandbox. A provider runs as the operator's
user and can bypass string-level policy through other execution paths. See
[`SECURITY.md`](../SECURITY.md).

## Generated configuration

Keep generated files private and limited to what the provider needs for this
launch.

- Claude receives a per-run settings file containing daemon-authored hooks.
  Dark Factory does not edit the operator's `~/.claude.json`.
- Codex uses an isolated generated home. The first seed may retain bounded
  operator authentication/model configuration while excluding operator MCP,
  project-trust, and hook tables. Dark Factory then writes its own hook,
  approval, sandbox, and source settings.
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
3. Return a `Capabilities` declaration for accepted model, reasoning, and
   permission values. Validation happens before a future launch.
4. Add focused tests proving the exact executable/argv, one unchanged startup
   input, private generated files, sanitized environment additions, and no
   resume or caller-selected source path.
5. Exercise lifecycle behavior through the generic prepare/activate runner
   tests. Do not add provider-specific supervision.
6. Update this guide and run the authoritative local gate.

If a provider needs a second lifecycle, output decoder, interactive terminal,
or custom authority path, it does not fit this interface. Challenge the
requirement instead of widening the kernel.

## Source and repository boundary

Stage 1 passes a source path only in private causal fixtures; production worker
admission refuses because Stage 2 has not yet created daemon-owned Changes.
After Stage 2, the provider will receive a leased source view and no Git
administrative locator. Repository status, diff, commit, push, and PR
operations remain daemon-owned and target the Change derived from the attempt
credential. A provider adapter must not run `git worktree`, accept a worktree
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
