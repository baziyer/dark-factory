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
   5 minute cap), never busy-loops. A session still `starting` past a fixed
   deadline (120s, generous enough for a cold Codex start with many MCP
   servers) is treated exactly like a failure by that same path -- the
   daemon stops it and records it `failed` -- rather than staying
   `starting` forever if the provider's own `SessionStart` hook never
   reaches the daemon (`execution.rs`'s `SESSION_START_DEADLINE`, [known
   issue #24](https://github.com/baziyer/dark-factory/issues/24)); the
   commit is itself guarded on the session still being exactly `starting`,
   so a hook that lands while this is in flight always wins the race and
   the deadline becomes a no-op, never overwriting an already-recovered
   session with a false reason. "Success" for this backoff is reaching
   `idle`, not merely a spawn call returning `Ok` -- a provider whose spawn
   always succeeds but whose hook never arrives has nowhere else to
   escalate to, so after 3 consecutive start-deadline failures the daemon
   pauses the agent instead of respawning again; `agent resume` is the way
   back in and resets that streak. This also catches a session recovered
   `starting` after a daemon restart, and never fires at all for a paused
   agent (dispatch for a paused agent returns before reaching it) or a
   session with an operator `StopSession` already in flight (that
   resolves through the ordinary stop-completion path instead, with its
   own real exit status). Delivery into an idle session types
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
   keeps concurrent agents from colliding in the same working tree. New
   agent branches start from `origin/HEAD`, or local `main` when no remote
   default exists, never the project root's current checkout; an
   operator may override it with an explicit `--worktree`.
   Fleet and per-agent status both expose the same live git summary for
   that worktree. A worker waiting for input is an operator-attention
   condition, not permission to stop or replace it; recovery begins by
   inspecting that summary and preserving or explicitly resolving dirty
   work.
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
   Carve-out: Codex 0.147 does not dispatch its own `SessionStart` hook at
   TUI startup — only once its first turn begins, which the daemon cannot
   wait for without deadlocking every fresh Codex session (`docs/
   providers.md`'s Codex `SessionStart` section has the full evidence). The
   daemon records a `SessionStart` itself the moment `factory-runner`
   reports the provider's own tty left canonical mode
   (`RunnerEvent::TerminalRaw`, from `tcgetattr` on the pty master —
   `crates/factory-runner/src/lib.rs`'s `supervise_terminal`): a
   kernel-level fact about the child's own terminal setup, not decoded
   terminal *output*, so no provider's own bytes are ever read or
   interpreted to make this decision. The synthesized transition
   (`Store::synthesize_session_start`) stays durably distinguishable from a
   real hook (it never sets `last_hook_event`), and Codex's own real,
   delayed `SessionStart` is still recorded normally whenever it arrives.
   No other provider gets this treatment; only Codex is known to defer the
   hook this way.
6. The local control and event API uses a private Unix socket by default. A
   subscription captures a durable replay head and marks when it has caught up.
   Inbound HTTP webhooks are an explicit, authenticated listener; receiving a
   message is a durable write before it wakes the orchestrator.
7. The board repaints on input or factory events; embedded agent terminals
   repaint when their PTY emits bytes. Each repaint replaces the complete
   mouse hit map for footer tabs/help/detach controls, visible rows, and pane
   geometry; a click is revalidated against current model state before it can
   select anything.
   Board hit testing and terminal input are separate routes: only events inside
   the rendered terminal content, while the pane is in typing mode, are encoded
   for a child, and only when that child's parsed output has enabled an xterm
   mouse protocol.
   If local scrollback is nonzero, a child-bound coordinate event is consumed
   while resetting to the live tail; forwarding can resume only from a later
   event after that redraw, so historical coordinates never act on live state.
   A 1Hz tick may request a coarse repaint for elapsed-time
   labels and activity sparklines while visible; no background animation or
   state polling is a source of truth.
8. The orchestrator is an agent like any other: it drives its own resident
   session and reaches the daemon only through the same durable task,
   message, and control interfaces every other client uses (`factoryctl`,
   directly or via its own session's shell access to it) — it may choose and
   delegate work, but it cannot bypass daemon-owned limits or reach SQLite
   directly.
9. Remote repository mutation is a daemon boundary. A session may edit and
   inspect its worktree, but its environment disables Git credential helpers,
   SSH transport, interactive credential prompts, and the operator's `gh`
   configuration. `factoryctl git commit`, `git push`, and `pr open|update`
   authenticate the live session token and carry no caller-selected project,
   agent, path, branch, or remote. `factoryd` re-resolves the exact managed
   worktree, linked-worktree gitdir/common-dir identity, and `agent/<id>` branch, uses an empty-config temporary
   gitdir/index so repository hooks, filters, helpers, fsmonitor, and external
   drivers cannot execute, and publishes commits with a compare-and-swap ref
   update. Remote and PR base come from write-once operator-pinned durable state configured before any session, never
   mutable `origin` metadata. It serializes repository operations through one
   committer, rejects protected/detached/mismatched refs, and writes
   credential- and content-free request/result events. Review and merge remain
   outside this API, preserving the independent-review invariant.
9. Once deletion of an agent or project begins, every known writer of files
   under its identity is blocked from starting a new write and drained if
   one is already running, before any of those files are removed. The
   mechanism is a per-identity `deleting` mark plus an in-flight `preparing`
   count (`execution::DeleteGate`, generic over `AgentId`/`ProjectId`),
   checked and incremented atomically under one lock so a delete beginning
   and a fresh write can never race past each other. `DeleteAgent`/
   `DeleteProject` set the mark first, then wait (bounded, 5s) for
   `preparing` to reach zero; every gated writer below participates by
   wrapping its write in the same check-in/check-out pair. Once the wait
   returns, the delete re-checks — read-only, no side effects yet — that
   its own database precondition would actually pass right now
   (`Store::check_agent_deletable`/`check_project_deletable`: no active
   run, no live session, no children, no dependent runs; the identity's
   deletion mark already rules out a *new* one appearing before the
   database delete's own transaction re-confirms it as the authoritative
   last word), so an agent or project that in fact cannot be deleted is
   refused before any file is touched, not after. Only then does it remove
   the identity's owned files, *then* its database row (files first, so a
   removal failure — unrelated to this race, e.g. a permission problem —
   leaves the row intact and the request retryable, rather than a ledger
   entry with no request left able to target its leftover files); a
   removal that still fails is the request's own error, never a log line
   and a swallowed failure.
   - Agent-scoped (`AgentId` gate, embedded in `SpawnBackoff`): gates the
     dispatcher's spawn preparation (composing guidance, writing a
     provider's generated config), an idle session's delivery composition
     (`deliver_pending`), a `Stop`/`SubagentStop` hook's delivery reply
     (`stop_hook_reply`) and a `UserPromptSubmit` hook's pending-delivery
     commit (`commit_pending_delivery_on_prompt`) — both reached directly
     from the local control API's `ProviderHook` handler, not through the
     dispatcher, and both declining silently (matching the hook contract:
     a live provider process's own hook call is not a request it can
     retry) rather than surfacing an error into the provider — and the
     handlers that read-or-lazily-create or overwrite an agent's guidance
     files outside the dispatcher (`GetAgent`/`AgentStatus`,
     `UpdateAgentProfile`).
   - Project-scoped (`ProjectId` gate, `Handle::project_gate`): gates
     `CreateAgent`'s worktree/guidance-tree provisioning for a brand-new
     agent id — the one writer a `DeleteProject` already in progress can
     never have covered through the agent-scoped gate above, since that id
     didn't exist yet for it to mark.

10. Autonomy posture is factory-wide durable state. Auto mode defaults on;
    an explicit per-agent provider mode overrides it. Provider bypass never
    bypasses Dark Factory's authenticated `PreToolUse` decision path, whose
    allow and deny answers are appended to the event ledger before reply.

11. Provider budgets are daemon-owned durable state, never provider- or
    client-inferred state. Every authenticated `PreToolUse` observation is
    counted and audited transactionally. Exhaustion pauses dispatch and
    makes subsequent tool hooks fail closed until an explicit reset. A
    provider metric the daemon cannot observe authoritatively is unavailable,
    not zero and not estimated.
    The ordinary agent hold and budget hold compose independently: resume
    cannot bypass exhaustion, reset cannot erase an ordinary hold, and spawn
    and delivery query the durable budget hold themselves.

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

Each task row has an internal immutable incarnation identity, separate from
its operator-facing task id. Delivery commits carry that identity plus the
resident session's prior episode count, so only a run created for the exact
composed task incarnation makes a concurrent commit idempotent. Retrying a
task preserves its incarnation; deleting and recreating the same task id does
not.

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
