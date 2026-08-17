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

- **Codex resume is per-agent only, not wired at all**: a fresh Codex session
  never passes `resume <thread-id>`, because nothing yet threads the thread
  ID Codex reports on its own `SessionStart` hook payload back into
  `sessions.provider_session_id` (`Store::record_hook_event` only accepts
  bounded/derived fields today, not the raw payload). Claude is unaffected —
  it resumes off a daemon-assigned UUID it never has to learn back. See
  CONTRIBUTING.md's "good first issues".
- **Codex's own sandbox can block task completion**: Codex's default
  shell-tool sandbox restricts execution to the workspace directory, which
  blocks reaching `factoryctl`'s control socket and an agent's `memory.md`
  (both under `$DARK_FACTORY_HOME`, outside the git worktree) — found running
  a real Codex session manually. A Codex agent may need an explicit
  sandbox/writable-roots policy written into its seeded `config.toml` before
  `task done`/`task blocked` and memory-file writes reliably succeed, not
  just get attempted by the model.
- **Claude's permission modes don't cover Bash**: `acceptEdits` auto-approves
  file edits but still interactively prompts for every distinct Bash
  invocation (including `factoryctl task done` itself) with no "don't ask
  again" persisted across turns — real friction for unattended dogfooding,
  found in the same manual check. The three modes this system exposes
  (`default`/`acceptEdits`/`plan`) do not include a full-bypass option;
  whether to add one (and how to gate it) is a security-relevant product
  decision, not just a wiring gap.
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

## Product boundaries

- Usage is shown as normalized provider headroom, not historical costs.
- Explicit task starts are v1; scheduling remains roadmap.
- Public FactoryEvent snapshots stay bounded and free of guidance-file
  content, message bodies, raw runner output, and tracing payloads.
