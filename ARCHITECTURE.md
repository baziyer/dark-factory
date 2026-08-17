# Architecture

Dark Factory separates deterministic process supervision from LLM decisions.
This file records constraints, not an aspirational component catalogue.

## Invariants

1. `factoryd` is the sole owner of processes, scheduling, dependencies,
   concurrency, retries, budgets, durable state, and health.
2. SQLite is the durable source of truth. State changes and their append-only
   events commit in the same transaction. The store uses WAL with `FULL`
   synchronous writes so acknowledged commands survive more than a process
   restart. One exclusive database lock prevents split-brain daemon writers.
   Carve-out: project and agent guidance, memory, and standing instructions
   are operator- and agent-editable files under `$DARK_FACTORY_HOME/projects`
   (see `factory_core::paths` and `factoryd::guidance`), not SQLite columns or
   events; SQLite still owns the identities (projects, agents) those files are
   keyed by, and the daemon creates and bounds the files but never treats
   their content as part of the durable ledger.
3. `factory-tui` and `factoryctl` are clients. Stopping, rebuilding, or losing
   either one cannot change the lifetime of an agent.
4. A **session**, not a run, is the unit `factory-runner` supervises: one
   resident, interactive provider process per agent (Claude Code, Codex, or
   the minimal `shell` provider), spawned under a PTY and living across many
   task *episodes* — not a fresh process per task. `factoryd` resolves a
   trusted absolute runner path before spawning it, clears the daemon's
   ambient environment to a fixed non-secret allowlist (plus a small,
   allowlisted `session_environment` — `DARK_FACTORY_AGENT/PROJECT/SOCKET/
   SESSION_TOKEN_FILE/FACTORYCTL` — the provider's only way to reach
   `factoryctl` and identify itself), and creates a private socket and
   bounded, retained `terminal.log` before spawning one process group under
   the PTY. A `starting` session row exists before that process spawn is
   even attempted, so a spawn failure is always durably visible (`session
   list`/the TUI, an announcement and the error as `wait_reason`) rather
   than only in the daemon's own log; a persistently broken spawn path
   retries with exponential per-agent backoff (5s doubling to a 5 minute
   cap), never busy-loops. Runners prove both a run ID and a random
   runner-instance ID and never adopt or signal from PID coincidence alone.
   Delivery into an idle session types composed text into its PTY and waits
   for the provider's own hook to acknowledge receipt (a
   `UserPromptSubmit`) before committing the delivery durably — committed
   synchronously as part of handling that exact hook request, before its
   reply reaches the provider process, so a fast-reacting provider can never
   observe the ack before the episode it names is actually open; a session
   that is already `working`/`waiting_for_input` is instead delivered into
   via its `Stop`/`SubagentStop` hook's block-reply contract, so a second
   task can land without interrupting a live turn. A single pending-delivery
   slot per agent keeps two independent delivery attempts (the dispatcher's
   own PTY-typed path racing a `Stop`/`SubagentStop` hook reply for the same
   agent) from ever composing and delivering the same task or messages
   twice. `factoryd`'s own restart never stops a session: `factory-runner`
   is a detached process tree, and a fresh daemon recovers by reconnecting
   to its control socket and replaying its retained spool from sequence
   zero; a session with no live connection at all (endpoint proven absent),
   or whose endpoint never becomes reachable again within a bounded number
   of reconnect attempts, is recorded `failed`/`unverifiable` rather than
   left dangling in whatever state it was recovered in. A supervised
   program's *own* process is not free to exit the instant it terminates,
   either: `factory-runner`
   deliberately holds the control socket open, retaining `terminal.log`,
   until a client acknowledges the exact terminal sequence it durably
   logged (`AcknowledgeExit`) — the daemon does this itself, immediately,
   for both a freshly spawned and a recovered session, whether the exit was
   an operator `StopSession` or the provider exiting on its own; skipping
   that acknowledgement (a defect this track fixed, not a design choice)
   orphans the runner process forever. One git worktree per agent
   (`agent/<id>`, provisioned on `CreateAgent`, removed on `DeleteAgent`,
   both best-effort against the underlying git repository) keeps concurrent
   agents from colliding in the same working tree; an operator may still
   override it with an explicit `--worktree`.
5. Provider adapters answer exactly two questions for the daemon's generic
   session runner, and nothing else: how to launch (`spawn_spec` — an
   executable, argv, environment additions, and any generated configuration
   file, e.g. Claude's per-session `--settings` file or Codex's per-agent
   seeded `CODEX_HOME`), and what they can do (`capabilities` — whether hooks
   drive state, whether resume is meaningful, which permission-mode strings
   are accepted). A provider never owns a PTY, never parses the provider's
   own terminal output, and never owns process lifecycle — that is the
   session runner's job, once, generically, for every provider including the
   `shell` reference implementation (see `docs/providers.md`). Durable
   session state is driven entirely by the provider's own hook invocations
   (`factoryctl hook --token-file PATH <Event>`, normalized into
   `factory_core::ProviderHookEvent`), never by decoding raw terminal bytes;
   the local control API may still expose a bounded, sanitized tail of a
   session's retained `terminal.log`, or attach live to it and accept
   operator input and resize requests, for inspection — none of this ever
   enters public events, webhook snapshots, or tracing, and the daemon proxy
   treats attached bytes as opaque (never logged, never decoded).
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

- Zero-downtime handoff between two daemon binaries is deferred. Ordinary
  restart recovery reconnects to exact stable runners; unverifiable identities
  fail visibly rather than risk attaching to the wrong process.
- Webhook exposure beyond loopback remains external. The daemon accepts
  exactly one owner-configured endpoint, on by default when its config file
  is present.
- Repository visibility is private during early operation; making it public is
  a separate product decision.
- Stop intent is durable at both the run and the session level. `StopRun`
  signals the exact live runner and, once accepted, persists
  `stop_requested_at_ms` on the run; `StopSession` does the same for a
  resident session's own process (and, since a run's process is now its
  session's, also requests the run stop). When the terminal event lands, the
  daemon records `stopped` (not `failed`) and moves the open task to
  `cancelled`; retry is an explicit requeue of a terminal task, including one
  stopped this way. Pause (`agent pause`/`resume`) durably holds an agent's
  queue — no new session spawns, no delivery into an idle one — without
  touching a session already live.
- Codex's `resume` is per-agent, not per-session: a fresh session always
  launches without `--resume` because nothing yet threads the Codex-reported
  thread ID (learned from its own `SessionStart` hook payload) back into
  `sessions.provider_session_id`. Claude's resume is unaffected, since the
  daemon assigns its own session UUID up front rather than learning it back.
