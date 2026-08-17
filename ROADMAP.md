# Dark Factory roadmap

Status: 17 August 2026

Dark Factory's current baseline is the durable local supervisor and native
control plane: one resident, PTY-backed session per agent (Claude Code,
Codex, or the minimal `shell` provider) spanning many task episodes, driven
entirely by the provider's own hooks; automatic dispatch on task
assignment, with PTY-typed delivery into an idle session and Stop-hook
block-reply delivery into a busy one; a real git worktree per agent;
operator stop/attach/terminal-input on a live session; full recovery of a
live session across a daemon restart; transactional results, queue
assignment, retry/stop controls, on-demand Codex subscription usage via
`factoryctl usage`, provider-scoped model and permission-mode selection
consumed at launch, file-backed project and agent guidance and memory
composed at delivery, and a private durable message inbox delivered
alongside the next task.

This file lists unfinished product work only. Completed launch work is not
repeated as a backlog item.

## Next: God command center

The orchestrator already reaches every daemon-owned operation the same way
any operator does — `agent add`/`task add`/`task assign`/`agent message`/
`session stop`/... from inside its own resident session's shell, identity
scoped automatically via `$DARK_FACTORY_PROJECT`/`$DARK_FACTORY_AGENT` — so
"give the orchestrator daemon access" is done. What is not:

- Give the orchestrator a focused view of its plan, child agents, delegated
  tasks, questions, and results (today it has the same `factoryctl`/board
  views as any client, nothing purpose-built for the orchestrator role).
- Make the agent hierarchy (`parent_agent_id`) visible without duplicating
  the task board.

## Next: intervention and operations

- Add a provider-thread view that renders bounded Claude Code/Codex transcript
  and tool activity, distinct from the raw runner-output panel.
- Provide a clear local workspace-terminal entry point for direct operator
  intervention (`factoryctl attach` covers a session's own PTY; a *separate*
  plain shell in the agent's worktree is still unaddressed).
- Decide and document the security policy for any future interactive terminal
  embedded in the `factory-tui` board (`factoryctl attach` is CLI-only today).
- Add richer activity inspection and first-class blocked-question/document
  workflows.

## Later: work allocation

- Add task dependencies and scheduler/dependency-driven allocation after the
  explicit operator workflow is stable.
- Add design, execution, and operations as a separate function axis, with
  optional personas that never replace the authority roles.

## Later: external operation

- Add further webhook wire profiles and public-network deployment hardening.
- Minerva's `legacy_v1` endpoint is the only wire profile the config accepts
  today; broadening beyond it is roadmap, not baseline.

## Unresolved decisions carried forward from resident sessions

These were made pragmatically while landing sessions/dispatch/delivery and
should be revisited, not silently relied on:

- **A composed delivery's submitting `\r` must be a separate PTY write**,
  not concatenated onto the text: real Claude Code's paste-vs-keystroke
  heuristic otherwise absorbs it as just another inserted newline rather
  than Enter. Fixed with a short (80ms) delay between the two writes
  (`execution::SUBMIT_DELAY`); worth watching for a similar issue in Codex's
  own input handling if it grows a comparable heuristic.
- **A session's underlying process will not exit without an explicit
  acknowledgement**: `factory_runner::run` deliberately keeps its control
  socket open (retaining `terminal.log`) after an ordinary termination until
  a client sends `AcknowledgeExit` for the exact terminal sequence it
  logged. The daemon does this itself immediately now (a real defect this
  track found and fixed, not a design choice) — but anything that spawns a
  session and stops watching it before that handshake completes (a test
  harness killing the daemon right after `StopSession` returns, say) will
  still orphan the runner process. `crates/factoryd/tests/sessions_e2e.rs`'s
  `cleanup_session`/`Daemon::drop` document the pattern to follow.
- **A Codex agent's own `factoryctl` tool calls (not the daemon's hooks)
  can fail under the `workspace-write` sandbox**: verified on one real
  Codex session (temp home, this track's item 7 check). Hooks
  (`SessionStart`/`UserPromptSubmit`/.../`Stop`, all daemon-authored
  commands Codex itself invokes) reached the daemon reliably every time —
  the resident-session/hook architecture is solid. But when the *agent*
  decided to run `factoryctl task done ...` as its own shell-command tool
  call (exactly what a composed delivery's instructions ask it to do),
  Codex's own sandboxed tool execution returned `Operation not permitted
  (os error 1)` even though the socket's directory is inside
  `writable_roots`; the task stayed `running` forever, silently, since the
  agent had no way to tell it failed beyond its own transcript.
  `sandbox_mode = "danger-full-access"` (this track's documented fallback
  to try) does not cleanly fix it either in this environment: it removes
  the sandbox restriction but makes Codex's own built-in `codex_apps` MCP
  server hang indefinitely at startup instead of failing fast the way it
  does under `workspace-write` (reproduced twice, once resuming a prior
  session and once on a brand new agent, so it is not a `--resume`
  artifact). Shipped default stays `workspace-write` (fails fast and
  deterministically, and every other real-Codex behavior confirmed sound)
  rather than a change that trades one hang for another. Needs either a
  narrower sandbox exception (specifically for connecting to the daemon's
  own control socket, not a blanket `network_access`/`danger-full-access`
  toggle) or an upstream Codex fix for `codex_apps`' own timeout behavior;
  out of this track's scope to chase further.

Resolved since (Track 5E — daemon follow-ups):

- **An operator's own MCP server config stalling a Codex session
  indefinitely in `starting`** ("Starting MCP servers", found running a
  real Codex session manually against an operator's real, MCP-server-heavy
  `~/.codex/config.toml`) is fixed at the source: `CodexProvider`'s
  one-time seed of a fresh per-agent `CODEX_HOME` now copies the operator's
  `config.toml` filtered down to what a factory worker needs, explicitly
  dropping `[mcp_servers.*]` (along with `[projects.*]` and
  `[hooks.*]`/`[hooks.state]` — see `docs/providers.md`'s "Codex:
  `CODEX_HOME` seeding is filtered, not a raw copy") — there is nothing
  left to launch, hang, or time out. Verified against one real Codex
  session on a temp home (this track's report has the exact `task list`
  result and which `permission_mode`/sandbox setting worked).
- **A daemon restart recovering a session that never leaves `starting`**
  (a session whose underlying process had already exited, or whose runner
  had become permanently unreachable) now durably fails instead of
  dangling forever: `execution.rs`'s `supervise_recovered` bounds its
  reconnect retries (`MAX_RECOVERY_ATTEMPTS`) and durably ends the session
  `failed`/`unverifiable` once exhausted, covering both a cleanly-absent
  runtime directory (already handled) and a runtime directory/socket that
  looks structurally present but never actually answers (previously
  retried forever).

## Product boundaries

- Usage is shown as normalized provider headroom, not historical costs.
- Explicit task starts are v1; scheduling remains roadmap.
- Public FactoryEvent snapshots stay bounded and free of guidance-file
  content, message bodies, raw runner output, and tracing payloads.
