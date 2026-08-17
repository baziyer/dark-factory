# Adding a provider

A "provider" is one supported coding-agent CLI: today, Claude Code
(`claude`), Codex (`codex`), and `shell` — a minimal reference
implementation (`sh -lc <command>`, no resume, no generated config) that
exercises the exact same `Provider` boundary as the real two, and is what
`crates/factoryd/tests/sessions_e2e.rs` drives instead of a real provider
CLI (see `tests/fixtures/shell-agent.sh`, a POSIX-`sh` fixture speaking the
same hook/`task done`/`task blocked` protocol a real session would). Start
there if you want a working example to copy before reading the rest of this
file. Dark Factory runs each provider as one
resident interactive process per agent, under a PTY, visible and typeable
by the operator. Provider-specific code is deliberately small: everything
that is the same for every provider (owning the PTY, `send_input`,
`resize`, `stop`, reading process output) is implemented once, generically,
by the daemon's session runner. A provider only has to answer two
questions:

1. **How do I launch you?** — an executable, argv, and any environment
   additions.
2. **How do you tell me what's happening?** — for both shipped providers,
   the answer is "hooks": a small script (`factoryctl hook`) the provider
   itself invokes at defined lifecycle points (session start, before/after
   a tool call, when it needs input, when it stops), which the daemon
   normalizes into one shared state machine
   (`factory_core::ProviderHookEvent`).

That's the whole boundary. A provider never touches a PTY, never parses the
provider's own terminal output, and never owns process lifecycle.

## The `Provider` trait

Defined in `crates/factoryd/src/providers/mod.rs`:

```rust
pub trait Provider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError>;
    fn capabilities(&self) -> Capabilities;
}
```

`spawn_spec` is the entire contract. Given a [`SpawnContext`] — the
project/agent/session identity, the worktree to run in, an optional model
and permission mode, an optional prior provider-session identity to
resume, where this session's hook token file lives, and the trusted path
to `factoryctl` — it writes whatever provider-specific configuration it
needs (a settings file, a seeded home directory, ...) and returns an
[`InteractiveLaunch`]: an executable, its argument vector, environment
additions, and the paths of any files it just wrote (for logging and
tests).

`capabilities()` declares, without a caller needing a provider-specific
`match`, whether this provider drives session state via hooks at all,
whether resuming a prior session is meaningful, and which
permission/approval-mode strings it accepts.

The session runner takes the `InteractiveLaunch` and spawns it under a PTY.
It knows nothing about Claude or Codex specifically; it only knows how to
run one program with one argv and environment under a terminal, and how to
receive `factoryctl hook` calls on the daemon's control socket and turn
them into `factory_core::ProviderHookEvent` values (see
`crates/factoryctl/src/main.rs`'s `hook` subcommand and
`crates/factoryd/src/local_api.rs`'s `ProviderHook` handler).

## Adding a new provider: the contributor path

1. Create `crates/factoryd/src/providers/<name>.rs` and register it in
   `crates/factoryd/src/providers/mod.rs` (`pub mod <name>;`).
2. Define a small struct (it can be a unit struct, like
   `ClaudeProvider`, or carry construction-time configuration, like
   `CodexProvider`'s seed-source home) and `impl Provider for <Name>`.
3. In `spawn_spec`:
   - Resolve the executable: usually a bare name (`PathBuf::from("your-cli")`)
     resolved against the runner's sanitized `PATH`, matching
     `runner_process::resolve_executable`. Never a secret or task content
     on argv.
   - If your CLI supports hooks, wire every event in
     `providers::hooks::HOOK_EVENTS` to
     `providers::hooks::hook_command(&ctx.factoryctl_path,
     &ctx.hook_token_path, event)`, written into whatever configuration
     format your CLI expects. Use `providers::hooks::write_private_file`
     to write it (atomic, mode `0600`, parent directory `0700`).
   - If your CLI needs isolated credentials/config (like Codex's
     `CODEX_HOME`), seed it once under `ctx.agent_dir` (per *agent*, not
     per session, so a resumed session can find prior state — see
     `CodexProvider`'s doc comment for why) and add it to
     `InteractiveLaunch::env`.
   - If your CLI can resume a prior session, use `ctx.resume` (`Some` when
     continuing that agent's last known provider-session identity) and
     validate whatever identity format your CLI requires before trusting
     it into argv.
4. In `capabilities()`, return `hooks: true` if you wired hooks up,
   `resume: true` if step 3's resume path is real, and the permission-mode
   strings your CLI actually accepts (validated end to end, not just
   documented by the CLI's own `--help`).
5. Write exact-argv and exact-generated-config tests (see
   `claude.rs`'s and `codex.rs`'s `provider_tests` modules for the shape:
   a `context()` helper building a `SpawnContext` against a `tempfile`
   directory, then asserting on `InteractiveLaunch::args`/`env` and the
   generated file's exact content).
6. You do not need to touch the session runner, the dispatcher, or the
   local control protocol. If your provider needs a wire type this trait
   doesn't expose (e.g. a genuinely new kind of launch input), that is a
   sign the boundary needs to grow — raise it rather than routing around
   it with provider-specific glue elsewhere in the daemon.

## Example/mock provider

Two different test doubles exist at two different layers, easy to conflate:

- `providers::shell::ShellProvider` (`crates/factoryd/src/providers/shell.rs`)
  *is* a real `Provider` implementation — the minimal reference one, above.
  Use it to exercise dispatch/delivery/hooks end to end without a real
  provider CLI, the way `tests/sessions_e2e.rs` does.
- `crates/factory-runner/src/bin/fake-agent.rs` is a minimal scriptable
  stand-in for a real provider CLI's raw *process behavior* (configurable
  exit code, stdout/stderr text and timing, stdin echo, and crash simulation
  via flags), one layer below `Provider` entirely — used by
  `factory-runner`'s own deterministic PTY/lifecycle tests, which know
  nothing about hooks or the `Provider` trait. It is what a `Provider`'s
  `spawn_spec` could point `InteractiveLaunch::program` at for a test
  double, but it is not itself a `Provider` — it is the reference for "what
  does the runner need from any program it spawns" if you are debugging
  process-lifecycle behavior (PTY sizing, signal handling, exit codes)
  rather than a provider's own hook wiring.

## What providers do *not* do

- They do not own a PTY, call `send_input`/`resize`/`stop`, or read process
  output — that is generic, implemented once.
- They do not guess precise session state when the underlying CLI does not
  expose it. `Notification` (Claude) or its Codex equivalent is treated
  uniformly as "waiting for input," whether that's a permission prompt or
  genuine idle-wait — providers report what actually happened; the state
  machine, not the provider, decides what it means.
- They do not manage ambient environment (`HOME`, `PATH`, ...); that is the
  session runner's sanitized-environment concern.
  `InteractiveLaunch::env` is only for provider-specific additions, like
  Codex's `CODEX_HOME`.
- They do not put secrets or task content on argv. Anything sensitive is a
  file path (e.g. the hook token file); anything private a hook needs is
  read from the daemon over the authenticated `factoryctl hook` request,
  never passed as a CLI flag.
