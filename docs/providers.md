# Adding a provider

A "provider" is one supported coding-agent CLI: today, Claude Code
(`claude`), Codex (`codex`), and `shell` — a minimal reference
implementation (`sh -lc <command>`, no resume, no generated config) that
exercises the exact same `Provider` boundary as the real two, and is what
`crates/factoryd/tests/sessions_e2e.rs` drives instead of a real provider
CLI (see `tests/fixtures/shell-agent.sh`, a small POSIX-`sh` launcher around
the byte-oriented `shell-agent.py` fixture, speaking the same
hook/`task done`/`task blocked` protocol a real session would). Its PTY
fixture buffers bytes through the complete prompt and emits one
`UserPromptSubmit` at the CR submission boundary, including for multiline
prompts; this keeps the test provider faithful to the exact durable attempt
acknowledgement contract. Start
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

## Runtime-resolved session settings

The agent profile is the configured override and remains separate from the
session's historical runtime metadata. For new Codex agents, the shared
project policy records the selected model, reasoning effort, and selection
reason; it defaults routine workers/reviewers to Luna and keeps Sol/xhigh for
God or an explicit escalation. At spawn, a provider may report the model,
reasoning effort, permission mode, and control mode only when the launch argv
or a configuration file it actually uses establishes them. Those values are
stored on the session row and remain on ended sessions, so changing an agent
profile cannot rewrite history. `None` is rendered as `unreported`; the daemon
never fills a missing provider default from a plausible model name.

Codex reports an explicit `--model`, root-level `model` and
`model_reasoning_effort` from its isolated seeded `config.toml`, and the exact
approval/bypass posture Dark Factory passes on argv. Claude reports explicit
launch values and its `bypassPermissions` auto-mode value; an omitted Claude
default remains unreported. The same session snapshot is returned by
`factoryctl agent status`/`session list` and consumed by `factory-tui`.

Profile model, reasoning-effort, selection-reason, and permission values are
validated against one provider capability declaration before the profile row is
written. Codex accepts the supported GPT-5.6 model IDs and reasoning efforts
`none`, `low`, `medium`, `high`, `xhigh`, and `max`; Claude and shell reject
reasoning efforts because those providers do not consume that field, while a
shell model remains its explicit command. For example, Codex accepts only
`on-request` and `never`; `bypass` is the factory-wide auto-mode launch posture,
not a valid Codex profile value, and is rejected instead of being deferred to
the next session launch. Upgrade migration 0022 also clears invalid
provider-scoped values already present in older profile rows, restoring the
provider default so they cannot reach a post-upgrade launch argv. Migration
0023 adds the nullable model-policy fields without backfilling existing
profiles; an existing profile remains unchanged until an operator edits it.

Repository authority is provider-independent too. Provider processes do not
inherit ambient Git/GitHub token variables or the SSH agent; the runner resets
Git credential helpers, disables Git SSH and prompting, and hides the
operator's `gh` configuration. Workers use the session-authenticated
`factoryctl change create` followed by `factoryctl git ...` and `factoryctl pr ...`
daemon capabilities instead. A
provider adapter must not add a separate remote credential path. This is an
authority and audit boundary, not OS isolation; #54 tracks the separate runner
user that supplies that outer wall.
Repository execution itself never consumes provider or repository config: the
daemon uses an empty-config temporary gitdir/index and an operator-pinned
remote/base, with compare-and-swap ref publication.

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
   - A clean stop may be acknowledged by the runner before the provider
     process has exited. The dispatcher holds queued work until that terminal
     event, then launches the successor with `ctx.resume` when supported. If
     the resumed provider does not acknowledge the first delivery, the shared
     path records the exact prompt as a durable delivery attempt, makes one
     bounded recovery submission using a bare CR, and leaves
     `delivery unacknowledged` durably visible if its hook still fails. It
     never retypes a body after that CR because the provider may already have
     accepted it. A daemon restart consumes an
     interrupted attempt rather than resetting its retry budget; only an
     explicit operator resume resets a terminal attempt. A provider must not
     silently consume a prompt without its exact hook acknowledgement. The
     submitted prompt also carries an invisible durable-attempt nonce; the
     hook must echo that exact body, including the nonce, so a delayed
     same-text hook from a deleted/recreated task cannot acknowledge the new
     attempt. A provider that normalizes the nonce is rejected as
     unacknowledged rather than guessed successful.
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
   - Implement the factory-wide `SpawnContext::auto_mode` posture. With no
     explicit per-agent permission mode, on means the provider's full
     approval/sandbox bypass and off means its native approval posture. An
     explicit per-agent mode always wins. This only removes provider prompts:
     every tool must still invoke the authenticated `PreToolUse` hook so the
     daemon can answer its policy gate.
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

Resumed Codex input has one deliberate fail-safe at the generic delivery
boundary. The Codex TUI can consume a CR as recovery from a `Conversation
interrupted` repaint while discarding the text already pasted into its
composer. Terminal output cannot prove whether that text is still present, so
Dark Factory does not guess with another bare CR or risk duplicate work by
retyping it. If a resumed Codex session does not emit the exact
`UserPromptSubmit` acknowledgement for a delivery, the daemon stops
that runner, durably excludes that provider thread from resume selection, and
delivers the still-queued task in a fresh Codex conversation only after the old
runner has exited. Session creation durably records whether the launch used a
resume target; learning a new thread id from a fresh session's `SessionStart`
does not change that provenance. A transient miss in the fresh conversation
therefore takes the ordinary bounded same-composer retry instead of spawning a
loop of replacements. The task/run is committed only by the exact nonce-bearing
hook, preserving one durable run. That acknowledgement and timeout recovery are
serialized as a durable fence: if the hook wins, recovery cannot cancel its run;
if recovery wins, a late exact `UserPromptSubmit` receives a blocking response
before Codex begins the turn. Recovery-stop intent survives daemon and control
socket failures and is replayed after reconnect until the poisoned runner's
actual exit is observed.

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

## Codex: `SessionStart` never fires at TUI startup, so the daemon synthesizes it

Codex 0.147's TUI does not dispatch the `SessionStart` hook — not even with
`source: "startup"`, and not because of hook trust/review (see below) — at
process launch. It defers session/thread creation, and therefore hook
dispatch, to the first turn: confirmed live (2 of 2 real dogfood sessions
sat in `starting` indefinitely, 2 and 1 minutes observed, unblocked only by
an operator manually posting the session's own `SessionStart` hook by
hand — `printf '{}' | factoryctl hook --token-file
$DARK_FACTORY_HOME/runs/<session>/hook.token SessionStart`) and empirically
(a throwaway `CODEX_HOME` with only a `SessionStart` hook produced zero
invocations across a much longer unbounded idle window while sitting at
Codex's own ready-to-type prompt; the identical hook fired exactly once,
tagged `"source":"startup"`, only once a prompt was actually submitted —
its command ran and its payload was captured, proving the hook's own
plumbing works, just not at the time its own `source` value claims). A real
dogfood rollout's own `session_meta`/`task_started` pair (the earliest
evidence Codex has an actual session object at all) appear together several
minutes after process launch, exactly at the moment delivery finally
happened — never at launch.

Ruled out along the way, with evidence: **hook trust/review** —
`--dangerously-bypass-hook-trust` is confirmed to skip Codex's own startup
review modal entirely (its own banner reads `` `--dangerously-bypass-hook-
trust` is enabled. Enabled hooks may run without review for this
invocation.``; no `[hooks.state]` entries are ever written for a bypassed
session, and no review modal (`tui/src/startup_hooks_review.rs`'s own
"Hooks need review"/"Trust all and continue" flow) ever rendered in either
experiment) — hooks besides `SessionStart` fire completely normally once a
turn is underway, so trust is not the blocker. **Config format** —
`config.toml`'s `[[hooks.SessionStart]]` shape is confirmed to work at all
(the same hook fired, tagged `startup`, once a turn began), so a
`.codex/hooks.json`-style file would not change the timing, only the
source. **A `matcher`** — nothing in the working config uses one, and nothing
in Codex's own source strings ties `SessionStart`'s dispatch timing to it.

This collides with an invariant `crates/factoryd/src/execution.rs`'s
dispatcher already has: nothing is ever PTY-typed into a session that is
not already `idle` (`Handle::start_task`, the dispatcher tick), and a
session only reaches `idle` via a `SessionStart` hook
(`Store::record_hook_event`). Left to Codex alone, every fresh Codex
session deadlocks forever — Codex waits for a typed prompt to begin the
turn that would fire `SessionStart`; the daemon waits for `SessionStart`
before typing anything.

**The first version of this fix synthesized `SessionStart` immediately on
"process spawned" — that is wrong, and not hypothetically.** A freshly
opened pty pair starts in canonical mode with echo on (`openpty`'s own
default; neither `factory-runner` nor `portable_pty::spawn_command` ever
calls `tcsetattr` on the slave), and stays that way until the provider
itself enables raw mode during its own startup, seconds later. A line
written into a canonical-mode tty longer than `MAX_CANON` (1024 bytes on
macOS) is silently discarded past the limit, submitting `\r` included — and
a composed delivery routinely contains one line longer than that (the task
body alone is budgeted 16 KiB). Worse, with the child not yet reading, the
body and its submitting `\r` sit contiguously in the input queue and the
first `read()` the provider ever does returns both at once — exactly the
condition that defeats `type_and_await_ack`'s own submit-timing heuristic,
so the unacknowledged delivery retries by re-typing the *entire* text,
reachably producing a truncated first copy still sitting in the composer,
a full second copy appended later, and one `\r` submitting both together
as one corrupted, duplicated prompt. None of this shows up nudging an
already-booted, already-raw-mode Codex by hand (which is all the evidence
above covers) — it is specific to typing at the instant a process is
merely confirmed spawned, before its tty leaves canonical mode.

**The fix instead waits for a kernel-level fact about the child's own
terminal setup — never terminal *output* — before synthesizing anything.**
`factory-runner` polls the pty master's line discipline (`MasterPty::
get_termios`, `crates/factory-runner/src/lib.rs`'s `supervise_terminal`,
via a dedicated thread sharing the master through a `Mutex` rather than
`unsafe` fd tricks — this workspace forbids `unsafe_code`) every 100 ms for
up to 120 s. When the tty enters raw mode, the runner reports it once as
the durable, replayable lifecycle event `RunnerEvent::TerminalRaw`. This
event is additive to `RUNNER_PROTOCOL_VERSION` 1. An older daemon reads
this data-free event as `RunnerEvent::Unknown` and continues. See
`docs/development/WORKFLOW.md` for the protocol compatibility requirement.
`wait_for_runner_exit`/`consume_until_exit`
(`execution.rs`) watch the same subscription they already hold open for
`Exited`, and the moment `RunnerEvent::TerminalRaw` arrives for a Codex
session (`should_synthesize_session_start`, gated on
`provider == Provider::Codex` at each caller's own spawn or recovery site —
never for Claude/`shell`), call `synthesize_codex_session_start`, which
makes the exact same `starting -> idle` transition a real hook would
(`Store::synthesize_session_start`) directly against the store, and wakes
the dispatcher — automating exactly what the operator's manual nudge did
live, with the identical proven-safe result (every later hook fires
normally afterward), now gated on the pty genuinely being ready to receive
input rather than a guess about timing. If Codex's own real (once-delayed)
`SessionStart(source=startup)` arrives later anyway — which it does, after
the first turn it caused — it is a harmless no-op: `record_hook_event`'s
`SessionStart` arm only transitions a session still `starting`, and
`set_provider_session_id` (`local_api.rs`) is already idempotent once a
provider session id is set.

**The synthesized transition stays durably distinguishable from a real
one.** `Store::synthesize_session_start` deliberately never touches
`last_hook_event`/`last_hook_at_ms`, unlike `record_hook_event` — leaving
them at whatever they already were (`None`, for a session no hook has ever
reached) means `state = idle` with `last_hook_event IS NULL` can only
happen via synthesis, never a genuine hook POST, so `session list`/`agent
status` never claim a hook fired that never did. No schema or wire change
was needed for this.

**Recovery** (a daemon restart while a Codex session is still `starting`)
needs no separate mechanism: `RunnerClient::subscribe` is always a
replay-plus-live stream from sequence zero, so a recovered session's fresh
subscription (`consume_until_exit`) sees `RunnerEvent::TerminalRaw` again
on replay if the runner already logged it before the restart — "the runner
re-reports on subscribe" falls out of the existing durable-spool design for
free, no new request type needed. Synthesis is idempotent
(`Store::synthesize_session_start`'s own doc comment), so seeing the event
again on every reconnect attempt is harmless.

If the tty stays in canonical mode for 120 s, the runner emits
`RunnerEvent::TerminalRawTimedOut`. The daemon logs a warning for a Codex
session, does not synthesize `SessionStart`, and leaves the session
`starting`. Issue #52 provides the durable session-start deadline and needs
the session to remain `starting` so it can fail and retry it. An old runner
under a new daemon does not know `TerminalRaw`, so it cannot trigger
synthesis and relies on the same deadline. The bounded poll never loops or
guesses that the terminal is ready.

No config on the Dark Factory side can fix Codex's own deferred
`SessionStart` dispatch — it is Codex's own session-lifecycle timing, not a
hooks-table or trust-state shape a generated `config.toml` controls.

## Codex: unattended approvals — historical pre-auto-mode design

This section records the v0.1.1 design evidence that led to auto mode; it is
not the current launch contract. Current default launches use
`--dangerously-bypass-approvals-and-sandbox`. `factoryctl auto off` uses
`approval_policy="on-request"`; an explicit per-agent `on-request` or `never`
still wins. The generated workspace-write configuration and copied rules
remain useful when bypass is off, but they are not a security boundary while
bypass is on. The daemon's `PreToolUse` policy and its documented limitations
are the current in-process tripwire (see `SECURITY.md`).

Left at Codex's own default, every agent stops on a native "Would you like
to run the following command?" prompt with nobody attached to answer it: a
real dogfood run's orchestrator (`god`) stopped on its very first
`factoryctl agent add`, and every later `factoryctl` call — even a bare
`sleep 30` — prompted again until the operator interactively chose "don't
ask again for commands that start with `factoryctl`". A worker would hit
the identical wall at its first `git push`/`gh pr create`. Two independent
gates were involved, and both had to change:

- **`approval_policy`** is now always passed explicitly —
  `-c approval_policy="never"` — instead of only when an agent's own
  profile sets a `permission_mode` (which left every agent silently on
  Codex's own un-set `on-request`). `on-request` and `never` were both
  considered for the default: `on-request` needs every command a role
  might run — `factoryctl`, `git`, `cargo`, `gh`, even `sleep` — allow-
  listed by hand as each new one is hit, an ever-growing, brittle
  treadmill that is the exact opposite of unattended operation. `never`
  was chosen for both the orchestrator and worker roles — a real per-role
  evaluation that reached the same answer for both, not an oversight, since
  both need to run unattended and neither's actual capability is gated by
  this flag (see the next point). An operator can still override it per
  agent with `agent profile set --permission-mode <on-request|never>`,
  which always wins over the default.
- **What actually gates capability is unchanged and still on**: Codex's
  `workspace-write` sandbox. `approval_policy` only controls whether Codex
  *asks* before running a sandboxed or escalation-requiring command, not
  what the sandbox itself permits — flipping it to `never` removes an
  interactive question nobody is there to answer, it does not widen what a
  session can do by itself. `SECURITY.md`'s own boundary already says as
  much: "Its security boundary is the operating-system user it runs as; it
  does not try to protect the operator from their own agents. ... An
  agent's own `permission_mode` widens or narrows that." One consequence is
  worth knowing precisely: under `never`, Codex cannot *ask* to escalate a
  command out of the sandbox at all (confirmed in the 0.147 binary: an
  internal check refuses "escalated permissions if the approval policy is
  [never]") — but Codex's own exec-policy checks (built-in denials, and any
  `forbid_rule` an operator's own `rules/*.rules` carries forward, see
  below) are unaffected by `approval_policy` either way; only the `allow`
  side is ever gated by it. Anything that needs to reach outside
  `workspace-write`'s own limits must already be granted directly in the
  sandbox itself, which is exactly what the next point does for the one
  thing every agent needs past the sandbox's previous default.
- **`network_access` is now `true`** in `[sandbox_workspace_write]`
  (previously `false`). Confirmed live that `false` denies even a *local*
  Unix-socket connect — seatbelt has no "just localhost" exception — which
  blocked not only a worker's `git push`/`gh pr create` but the
  orchestrator's own everyday daemon calls (only `task done`/`task blocked`/`agent
  message` have the outbox fallback below; nothing else does). This is a
  real widening — reads are already unrestricted under `workspace-write`,
  so `network_access = true` means a Codex session can read anything the
  operator's own user can read and send it anywhere on the internet, with
  no prompt at any point in the session — accepted under `SECURITY.md`'s
  documented threat model, not a gap in it. The alternative tried and
  rejected was a provider-wide `danger-full-access` bypass instead of a
  narrower `network_access` flip: it traded this hang for a worse one,
  since `codex_apps`'s own built-in MCP server hangs indefinitely at
  startup under it.
- **A `factoryctl` prefix rule is pre-seeded and kept present.**
  `CodexProvider` ensures `CODEX_HOME/rules/default.rules` contains
  ```
  prefix_rule(pattern=["factoryctl"], decision="allow")
  ```
  the exact shape (confirmed against a real dogfood agent's own
  operator-approved `rules/default.rules`) Codex itself writes once an
  operator chooses "don't ask again" by hand. Unlike `config.toml`'s
  one-time seed, this is checked and, if missing, appended on *every*
  spawn (`ensure_factoryctl_rule_present`) — not just when the file does
  not exist yet: a real dogfood agent already had a `rules/default.rules`
  from Codex approving some other command, with no `factoryctl` rule in
  it, and a seed-once-if-missing check would never have reached it. **This
  `allow` rule is inert under the shipped default.** `approval_policy =
  "never"` never consults `allow` rules — only the `allow` side is ever
  gated by `approval_policy`; Codex's own exec-policy checks and any
  `forbid_rule` an operator's own `rules/*.rules` carries forward (see
  below) apply regardless of `approval_policy` — so this line does
  nothing until an operator overrides an agent to `on-request` — at which
  point it pre-approves **unsandboxed** execution of any command whose
  parsed prefix is `factoryctl`, every subcommand (including `factoryctl
  update`/`factoryctl init`), not just the task/agent verbs a session's own
  delivery composes (`SECURITY.md` states this precisely; `prefix_rule` is
  documented in the 0.147 binary's own strings as valid "only with
  `sandbox_permissions: \"require_escalated\"`"). It is seeded anyway so an
  operator who does override one agent does not also have to rediscover
  and re-approve `factoryctl` by hand. If an operator's own copied-forward
  `default.rules` already mentions `factoryctl` at all — including a
  `forbid_rule` — `ensure_factoryctl_rule_present` leaves the file alone
  rather than appending its own `allow` line next to a possibly
  contradictory existing decision (which of the two Codex's own conflict
  resolution would apply is not established); an operator's own explicit
  `factoryctl` decision, once made, is never second-guessed.

  Separately, and unlike `config.toml`'s `mcp_servers`/`projects`/`hooks`
  tables (deliberately dropped, above): every `source_home/rules/*.rules`
  file an operator already has — including their own `forbid_rule`s — is
  now copied into a fresh agent's seeded home once, at first seed
  (`seed_rules_directory`, the same one-time-per-file contract as
  `config.toml`). Before this, `rules/` was never read from the source
  home at all, so an operator who had hardened their own `~/.codex` with
  approval or `forbid` rules lost every one of them, silently, in every
  factory agent.

## Sandboxed providers: the outbox

A provider's *hooks* (`SessionStart`/`UserPromptSubmit`/.../`Stop`) always
reach the daemon: they are daemon-authored commands the provider itself
invokes directly, outside any sandbox the provider applies to the agent's
own tool calls. But an agent's *own* `factoryctl task done`/`task
blocked`/`agent message` call — the one a composed delivery's instructions
ask it to run once it finishes its work — is just another shell-command
tool call as far as the provider is concerned, and can be sandboxed right
along with everything else. Confirmed on one real Codex session under the
`network_access = false` this provider used to ship (see the section
above for why it is now `true`): the Unix-socket connect to the daemon's
control socket failed with `Operation not permitted (os error 1)` even
though the socket's own directory is inside `writable_roots` — seatbelt
has no "just localhost" exception for a Unix-domain connect, so
`workspace-write`'s network restriction reached the daemon's own socket
too — and the task stayed `running` forever, silently, since the agent's
own transcript was the only place the failure showed up.

Rather than chase a narrower sandbox exception at the time (or a
provider-specific `danger-full-access`-style bypass — tried, and traded
one hang for a worse one: Codex's own built-in `codex_apps` MCP server
hangs indefinitely at startup under it), Dark Factory borrowed Munder
Difflin's file-based agent outbox: an agent writes its intended mutation as
a file in its own directory, and the harness carries it the rest of the
way. `network_access = true` (above) has since made the daemon's socket
directly reachable from a Codex session, so this fallback is no longer the
*only* way `task done`/`task blocked`/`agent message` reach the daemon —
but it stays as a belt-and-braces path for any other reason the socket
might be briefly unreachable (the daemon restarting mid-session, for
instance), and nothing about wiring a new provider changes: implement it
the same way regardless. Concretely:

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
  expose it. Claude's `Notification` hook and Codex's `PermissionRequest`
  hook (added in Codex 0.147.0; no Claude equivalent name, since Claude's
  permission prompts already surface through `Notification`) are both
  treated uniformly as "waiting for input," whether that's a permission
  prompt or genuine idle-wait — providers report what actually happened;
  the state machine, not the provider, decides what it means. A provider
  does not answer `PermissionRequest` approval prompts; auto mode prevents
  those prompts. The separate `PreToolUse` hook always receives the
  factory's allow/deny policy answer and is durably audited.
- They do not manage ambient environment (`HOME`, `PATH`, ...); that is the
  session runner's sanitized-environment concern.
  `InteractiveLaunch::env` is only for provider-specific additions, like
  Codex's `CODEX_HOME`.
- They do not put secrets or task content on argv. Anything sensitive is a
  file path (e.g. the hook token file); anything private a hook needs is
  read from the daemon over the authenticated `factoryctl hook` request,
  never passed as a CLI flag.
