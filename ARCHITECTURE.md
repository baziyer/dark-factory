# Architecture

Dark Factory separates deterministic process supervision from LLM
decisions. This file records constraints, not an aspirational component
catalogue.

## Invariants

1. `factoryd` is the sole owner of processes, scheduling, dependencies,
   concurrency, retries, budgets, durable state, and health.
2. SQLite is the durable source of truth. State changes and their append-only
   events commit in the same transaction. The store uses WAL with `FULL`
   synchronous writes so acknowledged commands survive more than a process
   restart. One exclusive database lock prevents split-brain daemon writers.
   Carve-out: project and agent guidance, memory, and standing instructions
   are operator- and agent-editable files under `$DARK_FACTORY_HOME/projects`
   (`factory_core::paths`, `factoryd::guidance`), not SQLite columns or
   events; SQLite still owns the identities those files are keyed by, and
   the daemon bounds the files but never treats their content as ledger.
3. `factory-tui` and `factoryctl` are clients. Stopping, rebuilding, or losing
   either one cannot change the lifetime of an agent.
4. A **session**, not a run, is the unit `factory-runner` supervises: one
   resident, interactive provider process per agent (Claude Code, Codex, or
   the minimal `shell` provider), spawned under a PTY and living across many
   task *episodes* — not a fresh process per task. `factoryd` resolves a
   trusted absolute runner path, clears the daemon's ambient environment to
   a fixed non-secret allowlist plus a small allowlisted
   `session_environment`
   (`DARK_FACTORY_AGENT/PROJECT/SOCKET/SESSION_TOKEN_FILE/FACTORYCTL` — a
   provider's only way to reach `factoryctl` and identify itself), and
   creates a private socket and
   bounded, retained `terminal.log` before spawning one process group under
   the PTY. A `starting` session row exists before the spawn is even
   attempted, so a failure is always durably visible (`session list`/the
   TUI, an announcement, the error as `wait_reason`); a persistently broken
   spawn path retries with exponential per-agent backoff (5s doubling to a
   5 minute cap), never busy-loops. Delivery into an idle session types
   composed text into its PTY and waits for the provider's own
   `UserPromptSubmit` hook to acknowledge receipt before committing the
   delivery durably, synchronously, as part of handling that exact hook
   request; a session already `working`/`waiting_for_input` is instead
   delivered via its `Stop`/`SubagentStop` hook's block-reply contract. A
   single pending-delivery slot per agent keeps the dispatcher's PTY-typed
   path and a hook-reply delivery from ever composing and delivering the
   same task or messages twice. `factoryd`'s own restart never stops a
   session: `factory-runner` is a detached process tree — spawned as its
   own process-group leader, and the launchd job abandons its group, so a
   group-wide signal to the daemon (launchd's `bootout`, a terminal's
   Ctrl-C) reaches no runner — and a fresh daemon recovers by reconnecting
   to its control socket and replaying its retained spool from sequence
   zero; a session with no live connection at
   all, or whose endpoint never becomes reachable again within a bounded
   number of reconnect attempts, is recorded `failed`/`unverifiable` rather
   than left dangling. `factory-runner` deliberately holds its control
   socket open, retaining `terminal.log`, until a client acknowledges the
   exact terminal sequence it durably logged (`AcknowledgeExit`) — the
   daemon does this itself immediately, whether the exit was an operator
   `StopSession` or the provider exiting on its own; skipping that
   acknowledgement orphans the runner process (#26 records the
   test-harness risk this creates). One git worktree per agent
   (`agent/<id>`, provisioned on `CreateAgent`, removed on `DeleteAgent`)
   keeps concurrent agents from colliding in the same working tree; an
   operator may override it with an explicit `--worktree`.
5. Provider adapters answer exactly two questions for the daemon's generic
   session runner: how to launch (`spawn_spec` — executable, argv,
   environment additions, generated configuration) and what they can do
   (`capabilities` — whether hooks drive state, whether resume is
   meaningful, which permission-mode strings are accepted). A provider
   never owns a PTY, never parses terminal output, and never owns process
   lifecycle — that is the session runner's job, once, generically, for
   every provider including the `shell` reference implementation (see
   `docs/providers.md`). Durable session state is driven entirely by the
   provider's own hook invocations (`factoryctl hook --token-file PATH <Event>`,
   normalized into `factory_core::ProviderHookEvent`), never by decoding raw
   terminal bytes; the local control API may still expose or attach live to
   a session's retained `terminal.log` for operator inspection — none of
   this enters public events, webhook snapshots, or tracing.
6. The local control and event API uses a private Unix socket by default. A
   subscription captures a durable replay head and marks when it has caught up.
   Inbound HTTP webhooks are an explicit, authenticated listener; receiving a
   message is a durable write before it wakes the orchestrator.
7. The board repaints on input or factory events; embedded agent terminals
   repaint when their PTY emits bytes. A 1Hz tick may request a coarse repaint
   for elapsed-time labels and activity sparklines while visible; no
   background animation or state polling is a source of truth.
8. The orchestrator is an agent like any other: it drives its own resident
   session and reaches the daemon only through the same durable task,
   message, and control interfaces every other client uses (`factoryctl`,
   directly or via its own session's shell access to it) — it may choose and
   delegate work, but it cannot bypass daemon-owned limits or reach SQLite
   directly.
9. Once deletion of an agent or project begins, every known writer of files
   under its identity is blocked from starting a new write and drained if
   one is already running, before any of those files are removed. The
   mechanism is a per-identity `deleting` mark plus an in-flight `preparing`
   count (`execution::DeleteGate`, generic over `AgentId`/`ProjectId`),
   checked and incremented atomically under one lock so a delete beginning
   and a fresh write can never race past each other. `DeleteAgent`/
   `DeleteProject` set the mark first, then wait (bounded, 5s) for
   `preparing` to reach zero; every gated writer below participates by
   wrapping its write in the same check-in/check-out pair. Once a delete's
   wait returns, it removes the identity's owned files, *then* its database
   row (files first, so a removal failure — unrelated to this race, e.g. a
   permission problem — leaves the row intact and the request retryable,
   rather than a ledger entry with no request left able to target its
   leftover files); a removal that still fails is the request's own error,
   never a log line and a swallowed failure.
   - Agent-scoped (`AgentId` gate, embedded in `SpawnBackoff`): gates the
     dispatcher's spawn preparation (composing guidance, writing a
     provider's generated config), an idle session's delivery composition
     (`compose_text`'s `guidance::read_or_create`), and the local-API
     handlers that read-or-lazily-create or overwrite an agent's guidance
     files outside the dispatcher (`GetAgent`/`AgentStatus`,
     `UpdateAgentProfile`).
   - Project-scoped (`ProjectId` gate, `Handle::project_gate`): gates
     `CreateAgent`'s worktree/guidance-tree provisioning for a brand-new
     agent id — the one writer a `DeleteProject` already in progress can
     never have covered through the agent-scoped gate above, since that id
     didn't exist yet for it to mark.
   - Known gap, not gated: a `Stop`/`SubagentStop` hook reply
     (`stop_hook_reply`, reached directly from the local control API's
     `ProviderHook` handler, not through the dispatcher) composes a
     delivery the same way the dispatcher's idle-session path does, but
     without access to the execution manager's gate from that call chain.
     Reaching it requires a live session to end its episode exactly while
     a delete is draining — narrow, and left as a follow-up rather than
     threading the gate through the hook-reply path in this change.

## First launch

`factoryd` starts from an empty database. A human creates a project, an agent,
and a task through `factoryctl` or the `factory-tui` board and assigns the
task to the agent; the daemon's dispatcher spawns that agent's resident
session automatically if none is live, or delivers into it if one already is
idle — there is no separate explicit "start" step in the common case. The
daemon starts the session through `factory-runner`, drives its state entirely
from the provider's own hook invocations, and streams persisted state to
disposable observers. A launch is proven only after a real provider command
has run and an observer and daemon have both restarted without stopping or
misidentifying it.

## Deliberately unresolved

- Zero-downtime handoff between two daemon binaries is deferred (see
  `docs/development/WORKFLOW.md`'s release design). Ordinary restart
  recovery reconnects to exact stable runners; unverifiable identities
  fail visibly rather than risk attaching to the wrong process.
- Webhook exposure beyond loopback remains external. The daemon accepts
  exactly one owner-configured endpoint, on by default when its config file
  is present.
- Repository visibility is private during early operation, a separate
  product decision from the code itself already being MIT-licensed (`LICENSE`).
- Stop intent is durable at both levels: `StopRun` signals the live runner
  and persists `stop_requested_at_ms`; `StopSession` does the same for the
  resident session's own process (and also requests the run stop). The
  terminal event then records `stopped` (not `failed`) and moves the task
  to `cancelled`; retry is an explicit requeue. `agent pause`/`resume`
  durably holds a queue — no new spawn, no delivery into an idle session —
  without touching a session already live.
