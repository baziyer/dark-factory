# Known issues

Backlog-ready: every entry here is meant to move to a GitHub issue once
dogfooding starts. Each has a symptom, how it was observed, the smallest
fix anyone has found so far, and a rough size (S/M/L). Resolved problems
are not listed here — see `ARCHITECTURE.md` and `docs/providers.md` for
how the daemon actually behaves today.

## Sessions and hooks

### Codex's `SessionStart` hook sometimes doesn't fire on its own
**Symptom**: a real Codex session's TUI renders its prompt and sits fully
idle — 0% CPU, no hook-related log entries — for several minutes; the
daemon's session stays `starting` forever because PTY-typed delivery only
happens once `SessionStart` moves a session to `idle`.
**Evidence**: observed on one real Codex session (README's "Unattended
operation" section, "Codex's own sandbox can still block..." paragraph);
`crates/factoryd/src/execution.rs`'s delivery path gates on `idle`.
Manually POSTing the exact `SessionStart` hook command unblocked it
immediately; every other hook then fired reliably for the rest of the
session.
**Suggested fix**: investigate whether this is specific to a
heavily-customized `~/.codex/config.toml` (many MCP servers/plugins
syncing at startup) or a more general timing issue in Codex's own hook
subsystem init. Not yet root-caused.
**Size**: M.

### Composed delivery's submitting `\r` must stay a separate PTY write
**Symptom**: if the text and the submitting `\r` are written to the PTY in
one call, real Claude Code's paste-vs-keystroke heuristic absorbs the `\r`
as an inserted newline instead of Enter.
**Evidence**: fixed with an 80ms delay between the two writes
(`execution::SUBMIT_DELAY`, `crates/factoryd/src/execution.rs`).
**Suggested fix**: no action needed for Claude. Watch for the same
class of issue if Codex's own input handling grows a comparable
paste-detection heuristic.
**Size**: S (watch item, not currently broken).

### A killed test harness can orphan a runner process
**Symptom**: `factory-runner` deliberately keeps its control socket open
after a session ends until a client sends `AcknowledgeExit`. A harness that
stops watching before that handshake completes (e.g. killing the daemon
right after `StopSession` returns) leaks the runner process for the rest
of the machine's uptime.
**Evidence**: `crates/factoryd/tests/sessions_e2e.rs`'s `Daemon`'s `impl
Drop` (around line 100) exists specifically as a safety net for a
panicking test; `cleanup_session` (around line 585) is the normal-path
pattern to follow.
**Suggested fix**: none needed in production code (the daemon always
acknowledges immediately, both for a freshly spawned and a recovered
session). Any new E2E test must follow the `cleanup_session`/`Drop` guard
pattern rather than a bare `daemon.stop()`.
**Size**: S (documentation/test-authoring discipline).

## Startup and webhooks

### Startup validates webhook project/agent existence eagerly
**Symptom**: a fresh install with a pre-existing `webhooks.json` pointing
at a project/orchestrator that hasn't been created yet fails to start the
daemon at all, rather than starting with webhooks disabled and a warning.
**Evidence**: `crates/factoryd/src/webhook_http.rs`'s `webhook_router`
calls `store.webhook_snapshot(&project_id, &orchestrator_agent_id, ...)`
eagerly and maps any error to `WebhookHttpError::InvalidConfig`; `bind_webhooks`
propagates that with `?`; `crates/factoryd/src/main.rs`'s `main()` awaits
`bind_webhooks(...)?` before the daemon can serve at all.
**Suggested fix**: make this validation lazy — bind the listener
regardless, and log a warning (or serve a 503 from the endpoint) if the
configured project/agent doesn't exist yet, instead of refusing to start
the whole daemon.
**Size**: M.

### One E2E fixture never acks its delivery
**Symptom**: `bare_factoryctl_resolves_via_path_inside_a_terminal_mode_session`
spawns a shell fixture that loops forever after its first hook
(`while :; do sleep 3600; done`) rather than acking a task; under load
(full-suite, single-threaded) this test alone accounts for roughly a
third of observed timeouts.
**Evidence**: `crates/factoryd/tests/sessions_e2e.rs:1084`.
**Suggested fix**: either shorten the sleep loop, or restructure the test
to `cleanup_session` immediately after observing `Idle` instead of relying
on timeout-based teardown.
**Size**: S.

## TUI

### A task created after board startup shows "(no body)" permanently
**Symptom**: assign a task while `factory-tui` is already running, and its
WORKSHOP detail pane shows "(no body)" forever, even after the task
completes.
**Evidence**: `FactoryEvent::TaskChanged` (`crates/factory-core/src/lib.rs`)
carries only a `TaskSnapshot`, never a task's `body`/`result` (those live
in the separate `TaskDetail` returned by `GetTask`); `Board::apply_event`'s
`TaskChanged` arm (`crates/factory-tui/src/model/mod.rs`) deliberately
preserves whatever `body`/`result` it already has for a task it already
knew about, which is correct for a task loaded at the initial fleet
snapshot — but a task created afterward has no prior body to preserve.
**Suggested fix**: add `body`/`result` to `FactoryEvent::TaskChanged`
(mirrors how `SessionChanged` already carries its full snapshot inline)
rather than having the client round-trip a `GetTask` after the fact.
**Size**: S.

### `factory-tui` can't create projects or agents
**Symptom**: FORTRESS/WORKSHOP have no key or prompt for `project add`/
`agent add`; a brand-new project with zero agents can only be created via
`factoryctl`.
**Evidence**: `crates/factory-tui/src/model/keymap.rs`'s `Action` enum has
no project/agent-creation variant (`NewTask`/`MessageAgent`/
`MessageOrchestrator`/... exist; project/agent creation does not).
**Suggested fix**: add a prompt flow analogous to `n` (new task) — a
project-creation prompt reachable from FORTRESS, an agent-creation prompt
reachable from an empty WORKSHOP.
**Size**: M.

### No inbox/message view in WORKSHOP
**Symptom**: `agent message`/`agent inbox` are full CLI features (durable,
delivered alongside the next task) with no way to read an agent's inbox
from the board — only the detail pane's task body/result.
**Evidence**: `crates/factory-tui/src/ui/workshop.rs` renders tasks and
the agent hierarchy only; no inbox pane.
**Suggested fix**: add an inbox list to WORKSHOP's detail pane when an
agent (not a task) is selected.
**Size**: M.

### FORTRESS doesn't scale a workshop to available space
**Symptom**: with one project, its workshop box is still sized as if many
projects were laid out left-to-right — a small box in a large empty map.
**Evidence**: `crates/factory-tui/src/fortress.rs`'s `compute_workshops`
positions workshops deterministically by creation order but does not
resize them to fill unused space.
**Suggested fix**: scale workshop box size to the number of projects
actually present, keeping the deterministic left-to-right order.
**Size**: S-M.

### `--dev-local-pty` dev path: keep or drop?
**Symptom**: `factory-tui --dev-local-pty` spawns a local `bash` shell per
agent instead of attaching to a real session, for offline TERMINALS/FOCUS
testing. It predates real session support landing on the daemon side and
is no longer the only way to exercise the pane mechanics.
**Evidence**: `crates/factory-tui/src/main.rs` (`dev_local_pty` field),
`crates/factory-tui/src/pane.rs`.
**Suggested fix**: decide whether it still earns its keep now that a real
daemon reliably provides sessions to attach to, or whether it should be
deleted in favor of always testing against a throwaway `factoryd` (see
`docs/development/WORKFLOW.md`).
**Size**: S (decision, not a bug).

### Terminal rendering gaps in the `tui-term`/`vt100` pane widget
**Symptom**: `factory-tui`'s embedded terminal panes (TERMINALS/FOCUS) are
faithful for the common case (colors, box drawing, cursor, alt-screen,
resize/reflow all verified against real `claude`/`codex`) but have known
gaps: no mouse forwarding (`claude` requests SGR mouse tracking; we
receive but discard `crossterm::event::Event::Mouse`), no Kitty keyboard
protocol passthrough, no scrollback view (only the live screen renders —
`vt100::Parser` keeps 10,000 lines of scrollback that nothing exposes), no
synchronized-output (mode 2026) awareness, and OSC 10/11 answers are a
fixed guess rather than the real outer terminal's palette.
**Evidence**: this was the verdict of the pre-build fidelity spike,
formerly `crates/factory-tui/SPIKE.md` (deleted as part of this docs
pass — this entry preserves its "GO recommendation" table). Idle CPU/RSS
was 0.0-0.1% / ~4MB in that testing.
**Suggested fix**: prioritize mouse forwarding first if dogfooding needs
mouse-driven interaction with `claude`'s composer (small effort, same
shape as `crates/factory-tui/src/keys.rs`); the rest are cosmetic/
completeness gaps, not blockers.
**Size**: S (mouse) to M (Kitty keyboard, scrollback) per gap.

## Security and state files

### Pre-trusting a worktree reorders every key in `~/.claude.json`
**Symptom**: `ClaudeProvider::pretrust_worktree` parses the operator's
whole `~/.claude.json`, adds one nested key, and writes the whole document
back — with no ordering guarantee, this resorts every key alphabetically
even though every value is preserved.
**Evidence**: `crates/factoryd/src/providers/claude.rs`'s
`try_pretrust_worktree` (around line 203) round-trips through
`serde_json::Value`/`serde_json::Map`; the workspace `serde_json`
dependency (`Cargo.toml:13`) does not enable the `preserve_order` feature,
so `Map` is BTreeMap-backed and sorts keys on serialize.
**Suggested fix**: add `features = ["preserve_order"]` to the `serde_json`
workspace dependency.
**Size**: S.

### Claude approvals: only `Bash(factoryctl *)` is pre-allowed
**Symptom**: a session's generated `claude-settings.json` pre-approves
exactly one command prefix; every other tool call still hits Claude's
native permission prompt unless the agent's `permission_mode` is set to
something more permissive (`acceptEdits`/`plan` semantics apply, not a
blanket bypass).
**Evidence**: README's "Unattended operation" section;
`crates/factoryd/src/providers/claude.rs`'s `claude_settings_json`.
**Suggested fix**: this is largely a documented, deliberate boundary (the
native prompt is the human-in-the-loop gate) rather than a bug — revisit
only if dogfooding shows agents routinely stalling on tool prompts nobody
is attached to answer.
**Size**: design decision, not scheduled.

### Codex sandbox `EPERM` on the daemon socket, handled via file outbox
**Symptom**: under Codex's `workspace-write` sandbox, an agent's own
`factoryctl task done`/`task blocked`/`agent message` call can fail to
connect to the daemon socket even though the socket's directory is inside
`writable_roots`.
**Evidence**: `docs/providers.md`'s "Sandboxed providers: the outbox";
`crates/factoryctl/src/outbox.rs`.
**Suggested fix**: current fix (a file-based outbox drained by the next
hook) is shipped and covers the three agent-facing mutations. A narrower
sandbox exception (rather than a workaround) would need Codex-side
cooperation or an upstream fix for the `codex_apps` MCP server's hang
under `danger-full-access` — not available today.
**Size**: L (needs upstream Codex change; current workaround is durable).

## Product gaps

### God (orchestrator) assigning work end to end isn't demonstrated
**Symptom**: an orchestrator agent driving `agent add`/`task add`/`task
assign` from inside its own session, delegating to worker agents, has been
exercised manually but not proven with a real Claude/Codex orchestrator
plus real workers in one recorded run.
**Evidence**: ROADMAP.md's "Next: God command center".
**Suggested fix**: run the flow once end to end with real providers and
record the result (task list, events) as the proof.
**Size**: M (validation work, not new code).

### `MAX_TASK_PAGE_ITEMS` is 10; every other list caps at 100-1000
**Symptom**: `factoryctl task list`'s default and max page size is 10,
while `project`/`agent`/`run`/`session`/`event` lists cap at 100 or 1000.
**Evidence**: `crates/factory-core/src/local.rs` — `MAX_TASK_PAGE_ITEMS:
u32 = 10` is deliberately small so a worst-case page of full task bodies
still fits one bounded local-API frame (`MAX_LOCAL_FRAME_BYTES`); the
constant's doc comment already records the reason.
**Suggested fix**: the constraint is real (task bodies can be large), but
worth checking whether trimming bodies from the list response (matching
how `TaskChanged` events already omit them) would let the page size grow
without changing the frame-size guarantee.
**Size**: S (investigation).

## Toolchain

### Toolchain pinned to 1.85, ratatui 0.29, tui-term 0.2
**Symptom**: the workspace pins `rust-version = "1.85"`. `ratatui` 0.30.2
needs rustc 1.88; `tui-term` 0.3.4 needs rustc 1.86. Neither newest release
is usable at the current pin.
**Evidence**: `crates/factory-tui/SPIKE.md`'s "MSRV" section (this file is
deleted as part of this docs pass; the fact is recorded here instead).
`vt100` 0.16.2, `portable-pty` 0.9.0, and `crossterm` 0.28-0.29 all build
fine on 1.85 already.
**Suggested fix**: bump the toolchain pin to 1.97 (or whatever current
stable is at the time) and take `ratatui` 0.30/`tui-term` 0.3 in the same
change, since they were held back by the compiler pin, not by any
compatibility problem.
**Size**: M (toolchain bump + verifying nothing else regresses).

## Stale documentation found while writing this pass

- `crates/factory-core/src/paths.rs`'s module doc comment (lines 14-19)
  still marks `codex-home/`, `claude-settings.json`, and `worktrees/<agent_id>`
  as "reserved, not yet created" — all three are created today (see
  README's "Guidance files" section). Trivial doc-comment fix, left for a
  future change since this pass is docs-only outside `AGENTS.md`/`CLAUDE.md`/
  `scripts/new-worktree.sh`/`.gitignore`.
