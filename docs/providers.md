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
     `providers::hooks::hook_command(&ctx.factoryctl_path, &ctx.hook_token_path, event)`,
     written into whatever configuration format your CLI expects. Use
     `providers::hooks::write_private_file` to write it (atomic, mode
     `0600`, parent directory `0700`).
   - If your CLI needs isolated credentials/config (like Codex's
     `CODEX_HOME`), seed it once under `ctx.agent_dir` (per *agent*, not
     per session, so a resumed session can find prior state — see
     `CodexProvider`'s doc comment for why) and add it to
     `InteractiveLaunch::env`.
   - If your CLI can resume a prior session, use `ctx.resume` (`Some` when
     continuing that agent's last known provider-session identity) and
     validate whatever identity format your CLI requires before trusting
     it into argv. If your CLI reports its own session/thread identity back
     through a hook payload rather than accepting one you assign up front
     (Codex's `SessionStart` `session_id` field is the shipped example),
     that is a `local_api.rs`/`store.rs` concern, not this trait's — see
     `Store::set_provider_session_id`.
   - **Provider A1** — a session's own `factoryctl` calls (an agent's own
     `task done`/`task blocked`, or an operator typing one directly) use a
     bare `factoryctl`, not `ctx.factoryctl_path`: that trusted absolute
     path is only for *generated hook commands*, which a provider's hook
     subprocess invokes directly and so never needs `PATH` resolution for.
     The session runner already prepends `factoryctl`'s own directory to
     `PATH` for every terminal-mode launch
     (`runner_process::apply_runner_environment`) — nothing a new provider
     needs to do itself, but worth knowing if you see an agent's own
     `factoryctl` calls fail to resolve: check that the launch actually
     went through the terminal-mode path, not a bare-name lookup against
     the operator's own shell configuration.
   - If your CLI shows an interactive one-time "trust this directory"
     prompt (both shipped providers do), pre-trust `ctx.worktree` before
     spawning — it is a git worktree Dark Factory itself created for this
     agent, never handed to it by an untrusted source, so the prompt can
     never be answered by anyone but the daemon. See
     `ClaudeProvider::pretrust_worktree`'s doc comment for the two hard
     parts found running this manually: canonicalize the worktree path
     first (symlinks resolved) — the CLI checks trust against its own
     resolved `cwd`, and a key written from the raw path silently never
     matches; and never create or overwrite the CLI's own trust-state file
     if it does not already exist and parse as valid JSON, since that file
     is real user state Dark Factory does not own.
   - If your CLI has its own default-deny permission/approval prompt for
     shell commands (Claude's native Bash approval, matched here), the
     product's posture for it is "the native prompt is the human-in-the-
     loop gate" — do not pass a blanket bypass flag. Instead pre-approve
     *only* the composed delivery's own `factoryctl` calls in whatever
     narrow, CLI-native allowlist mechanism exists (see
     `claude_settings_json`'s `permissions.allow`) — otherwise an agent's
     own progress report never gets past its first Bash prompt when nobody
     is attached to answer it.
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

## Codex: `CODEX_HOME` seeding is filtered, not a raw copy

`CodexProvider` seeds each agent's isolated `CODEX_HOME` once
(`seed_codex_home_once`, `crates/factoryd/src/providers/codex.rs`) from the
Codex home the daemon's own environment names — `$CODEX_HOME` if set, else
`~/.codex`, exactly as `codex` resolves its own — copying its `config.toml`
(filtered, see below) and symlinking its `auth.json`. That is how a factory
runs on a different Codex account than the operator's shell: log it in once
(`CODEX_HOME=~/.codex-dogfood codex login`) and give the daemon that
`CODEX_HOME` (`CODEX_HOME=~/.codex-dogfood factoryctl init` carries it into
the launchd job; `factoryctl doctor` reports which home is in effect and
whether it holds an `auth.json`). Switching the daemon's `CODEX_HOME`
later re-points every agent's `auth.json` link on its next spawn (a link
the daemon made; a real file is never touched); the seeded `config.toml`
stays. Copying `config.toml` whole was the original design and it is
filtered, not verbatim, because copying it whole stalled every session at
"Starting MCP servers" on a real machine: the operator's own
`config.toml` brings along `[mcp_servers.*]` (Codex launches every one of
them before a session becomes usable, several of which expect an
interactive terminal/browser/local dev server that plainly is not there
inside a headless factory worker), `[projects.*]` (the operator's own
per-repo trust decisions — irrelevant to a factory worker's own worktree,
which gets its own explicit trust entry from `rewrite_config_block` every
spawn regardless), and `[hooks.*]` (both the array-of-tables shape,
`[[hooks.<Event>]]`/`[[hooks.<Event>.hooks]]`, if the operator has their
own hooks configured, and the plain `[hooks.state]` table Codex persists
hook-trust decisions into — confirmed on a real machine's
`~/.codex/config.toml`, keyed `"<config-path>:<event>:0:<n>"`). None of
these belong in a daemon-owned, `--dangerously-bypass-hook-trust` session
that never asks for hook trust in the first place.

The allow-list (`filter_operator_config_for_seed`/`DROPPED_SEED_TABLES`)
is table-scoped, not key-scoped: **dropped** — any top-level table whose
name (the part before the first `.` inside `[...]`/`[[...]]`) is
`mcp_servers`, `projects`, or `hooks`. **Kept** — every root-level scalar
(before the first table header at all: `model`, `model_provider`,
`approval_policy`, the operator's own `sandbox_mode`, ...) and every other
table (e.g. `[model_providers.custom]`) verbatim. This only runs once, at
first seed — `seed_codex_home_once` never overwrites an existing seeded
`config.toml` on a later spawn, so a real edit an operator makes to a
seeded home (or Codex's own writes back into it) is never clobbered by a
later filter pass. See `operator_config_is_filtered_to_the_documented_allow_list_at_seed`
in `codex.rs`'s test module for the fixture-driven proof.

## Sandboxed providers: the outbox

A provider's *hooks* (`SessionStart`/`UserPromptSubmit`/.../`Stop`) always
reach the daemon: they are daemon-authored commands the provider itself
invokes directly, outside any sandbox the provider applies to the agent's
own tool calls. But an agent's *own* `factoryctl task done`/`task
blocked`/`agent message` call — the one a composed delivery's instructions
ask it to run once it finishes its work — is just another shell-command
tool call as far as the provider is concerned, and can be sandboxed right
along with everything else. Confirmed on one real Codex session
(`workspace-write`, ROADMAP.md's now-resolved unresolved decision): the
Unix-socket connect to the daemon's control socket failed with `Operation
not permitted (os error 1)` even though the socket's own directory is
inside `writable_roots` — the task stayed `running` forever, silently,
since the agent's own transcript was the only place the failure showed up.

Rather than chase a narrower sandbox exception (or a provider-specific
`danger-full-access`-style bypass — tried, and traded one hang for a worse
one: Codex's own built-in `codex_apps` MCP server hangs indefinitely at
startup under it, see ROADMAP.md), Dark Factory borrows Munder Difflin's
file-based agent outbox: an agent writes its intended mutation as a file in
its own directory, and the harness carries it the rest of the way.
Concretely:

- Every session's environment includes `DARK_FACTORY_AGENT_DIR` (this
  agent's guidance directory, `factory_core::paths::agent_dir` — already
  inside a Codex session's `writable_roots`, see the section above), a
  fixed addition to `runner_process::SESSION_ENVIRONMENT_NAMES` like every
  other `DARK_FACTORY_*` identity variable.
- `factoryctl task done`/`task blocked`/`agent message` — the three
  agent-facing mutations, not every command — fall back to writing the
  intended request as JSON to
  `$DARK_FACTORY_AGENT_DIR/outbox/<unix ms>-<8 hex>.json`
  (`crates/factoryctl/src/outbox.rs`) on any connect/send failure to the
  daemon socket, printing `queued: outbox/<name> (delivered on the next hook)`
  and exiting `0` rather than failing the agent's tool call outright.
- `factoryctl hook` — which always runs unsandboxed — drains that
  directory before every hook request it sends, not just `Stop`: it sends
  each queued request to the daemon in submission order, deleting the file
  on success or on a daemon-side rejection (poison-pill avoidance; logged
  to stderr), and stopping the drain (leaving the rest queued) on the
  first transport failure, so an unreachable daemon is not retried file by
  file for nothing. Bounded to 100 files and ~3 seconds of wall-clock time
  so a large or wedged outbox can never make a hook invocation itself
  stall the operator's live provider session — separate from and on top of
  `factoryctl hook`'s own 5-second fail-open budget.

Only these three commands fall back this way. Every other `factoryctl`
command — the operator's own `project add`, `task assign`, and so on —
fails exactly as it always has on an unreachable daemon: silently queuing
an operator-facing command that nothing but a session's own hooks will
ever drain would just hide a real failure behind a misleading "queued"
message.

The daemon's own dispatcher tick does **not** also drain outboxes — the
hook is the only carrier. A session's `Stop` hook always fires at the end
of a turn (and every other hook fires more often than that), so the next
opportunity to drain is never far away; giving the dispatcher a second,
redundant drain path would be YAGNI.

If you are adding a new provider that runs its own tool calls under a
sandbox, wire your hooks so `factoryctl hook` still gets invoked
unsandboxed (matching Claude and Codex), and nothing else is required —
the outbox fallback is generic across providers, keyed only on
`DARK_FACTORY_AGENT_DIR` being set in the session environment.

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
