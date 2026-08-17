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

This file lists direction and unfinished product work only. Known problems
with the current baseline — including things that don't work yet and
things that are simply undecided — live in
[docs/KNOWN-ISSUES.md](docs/KNOWN-ISSUES.md), backlog-ready for GitHub
issues.

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
- Demonstrate one real orchestrator delegating to real workers end to end
  (see `docs/KNOWN-ISSUES.md`).

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
- Let `factory-tui` create projects and agents, and surface an agent's
  inbox (see `docs/KNOWN-ISSUES.md`) — CLI-only today.

## Later: work allocation

- Add task dependencies and scheduler/dependency-driven allocation after the
  explicit operator workflow is stable.
- Add design, execution, and operations as a separate function axis, with
  optional personas that never replace the authority roles.

## Later: external operation

- Add further webhook wire profiles and public-network deployment hardening.
- Minerva's `legacy_v1` endpoint is the only wire profile the config accepts
  today; broadening beyond it is roadmap, not baseline.

## Later: distribution

- Design in [docs/development/WORKFLOW.md](docs/development/WORKFLOW.md)
  (not implemented): a hosted release manifest, `factoryctl update`, and an
  in-terminal update signal, ahead of npm/Homebrew packaging.

## Product boundaries

- Usage is shown as normalized provider headroom, not historical costs.
- Explicit task starts are v1; scheduling remains roadmap.
- Public FactoryEvent snapshots stay bounded and free of guidance-file
  content, message bodies, raw runner output, and tracing payloads.
