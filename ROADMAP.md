# Dark Factory roadmap

Status: 16 August 2026

Dark Factory’s current baseline is the durable local supervisor and native
control plane: explicit task starts, Claude and Codex runners, transactional
results, queue assignment, retry/stop controls, on-demand Codex subscription
usage via `factoryctl usage`, provider-scoped model and permission-mode
selection, file-backed project and agent guidance and memory composed at
launch, and a private durable message inbox delivered into the next launch.

This file lists unfinished product work only. Completed launch work is not
repeated as a backlog item.

## Next: God command center

- Expose the durable message inbox to provider clients so agents can send and
  receive messages with an explicit sender identity.
- Give the orchestrator a focused view of its plan, child agents, delegated
  tasks, questions, and results.
- Expose bounded create-agent, create-task, assign, start, retry, and stop
  operations to the orchestrator through the same daemon-owned interfaces as
  other clients.
- Make the agent hierarchy visible without duplicating the task board.

## Next: intervention and operations

- Add durable pause/resume intent and bounded steering for active runs.
- Add a provider-thread view that renders bounded Claude Code/Codex transcript
  and tool activity, distinct from the raw runner-output panel.
- Provide a clear local workspace-terminal entry point for direct operator
  intervention.
- Decide whether active-run messages should remain queued or be delivered as a
  provider-acknowledged next turn; never inject unacknowledged raw PTY input.
- Decide and document the security policy for any future interactive terminal
  embedded in the native UI.
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

## Later: per-agent runtime isolation

- Provision the reserved `worktrees/<agent_id>` git worktree, `codex-home/`
  `CODEX_HOME`, and generated `claude-settings.json` hooks settings under each
  agent's guidance directory (`factory_core::paths` already computes these
  paths; nothing creates or consumes them yet).
- Consume `permission_mode` at launch once the provider adapters accept it;
  it is stored and shown today but not yet wired into `execution::prepare_launch`.

## Product boundaries

- Usage is shown as normalized provider headroom, not historical costs.
- Explicit task starts are v1; scheduling remains roadmap.
- Public FactoryEvent snapshots stay bounded and free of guidance-file
  content, message bodies, raw runner output, and tracing payloads.
