# Architecture

Dark Factory separates deterministic process supervision from LLM
decisions. This file records constraints, not an aspirational component
catalogue.

## Invariants

1. `factoryd` is the sole owner of processes, scheduling, dependencies,
   concurrency, retries, budgets, durable state, and health. Its live-session
   capacity is a finite operator-owned launchd setting (`--max-active-runs`),
   never model-selected or agent-mutable; changing it restarts only the daemon
   and preserves detached runner processes.
2. SQLite is the durable source of truth. State changes and their append-only
   events commit in the same transaction. The store uses WAL with `FULL`
   synchronous writes so acknowledged commands survive more than a process
   restart. One exclusive database lock prevents split-brain daemon writers.
   Carve-out: project and agent guidance, memory, and standing instructions
   are operator- and agent-editable files under `$DARK_FACTORY_HOME/projects`
   (`factory_core::paths`, `factoryd::guidance`), not SQLite columns or
   events; SQLite still owns the identities those files are keyed by, and
   the daemon bounds the files but never treats their content as ledger. The
   active `memory.md` is capped at 16 KiB; `agent get`/`agent status` report a
   bounded health state and omit unhealthy content rather than failing the
   mechanical lookup. At the no-live-session dispatch boundary, an oversized
   memory is archived losslessly in a private bounded rotation and replaced
   atomically with a valid UTF-8 recent-line projection. `PROJECT.md`, Rules,
   and standing instructions are never rewritten by memory compaction.
3. `factory-tui` and `factoryctl` are clients. Stopping, rebuilding, or losing
   either one cannot change the lifetime of an agent. A manual TUI update uses
   the same verified active-runtime transaction as `factoryctl`, then execs
   only the digest-verified immutable version-directory viewer after exact
   daemon health succeeds; the mutation lock spans that seam and a private
   phase record bound to the canonical home/socket/plist/UID/job identity
   makes a crashed handoff recoverable only by its original authority. Managed
   health proves the launchd PID and active sibling executables, not just a
   version string. Local attach panes are closed, but runner and provider
   process identities are unchanged.
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
   `session_work` is the one provider-independent authority for this work,
   separate from the provider lifecycle state on `sessions`. Its exhaustive
   states are `empty`, `delivering`, `uncertain`, and `running`, guarded by a
   monotonic compare-and-swap revision. Reservation captures the exact attempt,
   task incarnation, task work revision, and preallocated run
   id before any external effect. Immediately before the first PTY write or
   hook reply it moves to `uncertain`; only the exact echoed attempt may move
   it to `running`, and only exact run completion or session end returns it to
   `empty`. A provider becoming idle, an operator resume, a journal retry, or
   an in-memory lock cannot release it. `delivery_attempts` retains prompt and
   effect history but is an audit journal, never a second owner. The journal's
   exact acknowledged identity is the idempotency receipt; run counts and task
   status are not evidence that a prompt landed. A crash while still
   `delivering` may retry the stored attempt, while `uncertain` is never
   replayed because submission may already have been accepted.
   A delivery timeout may project `waiting_for_input` only while that exact
   attempt still owns `uncertain`; an acknowledgement that reaches `running`
   first wins the same durable fence and cannot be overwritten by the timeout.
   Each typed prompt carries an invisible attempt nonce that the acknowledgement must
   echo, binding the hook to the immutable attempt even when task ids are
   deleted and recreated. A resumed Codex composer
   that does not acknowledge its first delivery is retired: the daemon blocks
   that exact provider thread from future resume, stops its runner, and sends
   the still-queued task once in a fresh conversation after the runner's exit
   is durable. Hook acknowledgement and recovery are one durable fence: an
   acknowledgement atomically opens the run and wins over any delayed event
   waiter, while recovery cancellation makes a later exact prompt hook block
   before provider model/tool execution. The recovery stop is itself durable
   work; reconciliation and restart supervision reissue it idempotently until
   the old runner's actual exit is observed. Resumed-vs-fresh launch provenance
   is recorded at session creation; a fresh Codex `SessionStart` assigning a
   thread id cannot turn it into a resumed launch or cause fresh-conversation
   churn. No recovery writes a second body or submit keystroke while authority
   is `uncertain`: exact identity cannot prove that the first external effect
   did not already trigger provider work.
   The hook must match that
   stored prompt exactly before acknowledging it, so a stale hook cannot
   recompose a newer queue head. The per-agent pending-delivery slot only
   reduces duplicate preparation; correctness comes from the durable
   `session_work` CAS when the dispatcher and hook-reply paths race.
   Its stored attempt identity is protected by a composite foreign key, while
   every transition transaction also proves the session, project, agent, task
   incarnation/revision, and run relationship. The v28 migration adopts only
   that exact ownership: cross-owner or contradictory legacy rows are
   quarantined, and ownership retained by an already-ended session is
   terminalized together with its task, journal, and durable events instead
   of leaving non-deliverable `running` work behind.
   `factoryd`'s own restart never stops a
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
   that worktree and the same structured operator-attention reasons. Those
   reasons separate provider questions/permissions from worker blocks,
   delivery or observation failures, exhausted budgets, and lifecycle
   inference; every client receives the same bounded control-safe summary,
   source IDs, age, and safe action. NEEDS YOU is a decision inbox: only
   reasons with a valid typed operator decision enter it. A worker-blocked item
   enters only when it carries exact task identity for the existing typed retry
   operation; retry has no default. Opaque worker blocks and unproven lifecycle
   inference remain control-plane concerns alongside delivery, observer,
   capacity, and other deterministic recovery. Selecting a row keeps BUILDING
   visible and shows
   the same bounded cause, exact evidence, typed choices, optional safe
   recommendation, and consequences in both `factoryctl status` and the TUI.
   Permission and budget choices have no default and require explicit selection;
   budget reset also requires a second stale-safe confirmation.
   Attention is not permission to stop or replace a worker; recovery begins by
   preserving or explicitly resolving dirty work. Fleet attention snapshots carry the
   durable event high-water mark from the same store read, so a delayed
   snapshot cannot overwrite a resolution already observed on the event
   stream. The TUI also binds an in-flight decision to its exact operation,
   request, and source. At most one exact retry operation per task may merge a
   response payload; retiring the decision or admitting a durable task event
   removes that permission, so a delayed success cannot become valid through
   bounded-history eviction and fold older state over a newer projection.
   Completed task decisions are invalidated by durable `TaskChanged`
   transitions, so a queued task that blocks again within the same millisecond
   is a new decision without treating summary text or wall-clock precision as
   identity.
   The agent's `current_session_id` is the exact live-session relation used
   by BUILDING activity: it is derived from sessions whose `ended_at_ms` is
   null, and session create/end transactions publish the matching
   `AgentChanged` snapshot with their `SessionChanged` event. Bootstrap and
   recovery use the same derivation, so a hook's activity can never fall
   through to an ended session or disappear from event-driven clients.
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
   this enters public events, webhook snapshots, or tracing. Terminal
   attachment is a negotiated daemon/runner operation, never a client-side
   read of `terminal.log`. The default `factoryctl attach` and TUI pane
   request a bounded 256 KiB tail and receive retained generation plus byte
   bounds before raw PTY frames. `--full-history` (or explicit
   `--since-offset 0`) replays all bytes still retained; `--since-offset N
   --generation G` resumes a negotiated cursor. A cursor beyond the live end
   or outside retained generations is a structured gap containing the current
   generation and valid offset range, so recovery is actionable rather than a
   blank pane. Ready and gap frames include the generation owning the retained
   base, the generation and exact byte offset at which replay starts, and
   continuity metadata; every output chunk carries its owning generation.
   Frames remain opaque base64 bytes, bounded to the runner frame limit,
   preserving UTF-8 and ANSI bytes without interpreting provider output. A new
   daemon explicitly probes runner attach capabilities: an old runner may
   serve `Legacy` or explicit full history, but a bounded tail/cursor is
   refused rather than silently becoming an unbounded replay. The bounded tail
   is deliberately a reset-baseline view, not a reconstruction of an
   application's mouse, paste, cursor, or alternate-screen state; use full
   history when that state matters. Its suffix starts at safe UTF-8/ANSI
   boundaries. Replay snapshots pin independent file descriptions and use
   positional reads, so rotations and appends cannot move a replay cursor.
   Ctrl-] closes only the client stream; Ctrl-C remains ordinary PTY input, and
   output failure wakes blocked CLI input.
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
   repaint when their PTY emits bytes. A 1Hz tick may request a coarse repaint
   for elapsed-time labels and to age the bounded five-second activity
   sparklines while visible; durable hook/tool events are the only source of
   activity counts, with no background animation or state polling as a source
   of truth. Each repaint replaces the complete mouse hit map for footer
   tabs/help/detach controls, visible rows, and pane
   geometry; a click is revalidated against current model state before it can
   select anything.
   Board hit testing and terminal input are separate routes: only events inside
   the rendered terminal content, while the pane is in typing mode, are encoded
   for a child, and only when that child's parsed output has enabled an xterm
   mouse protocol.
   Terminal attach is a CLI-first handshake: a session/runner race returns a
   bounded typed refusal with the identities checked. The TUI removes the
   refused pane before rendering, refreshes the durable fleet snapshot, and
   stays on the board while preserving independent task, session, and runner
   state; keyboard and mouse selection use the same path.
   If local scrollback is nonzero, a child-bound coordinate event is consumed
   while resetting to the live tail; forwarding can resume only from a later
   event after that redraw, so historical coordinates never act on live state.
8. The orchestrator is an agent like any other: it drives its own resident
   session and reaches the daemon only through the same durable task,
   message, and control interfaces every other client uses (`factoryctl`,
   directly or via its own session's shell access to it). It may coordinate
   and delegate work, but this local socket currently does not establish a
   human caller or enforce operator-only agent creation/profile changes; that
   principal boundary remains #133/#127. Prompt guidance must not be described
   as authorization, and the orchestrator cannot reach SQLite directly.
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

`factoryd` starts from an empty database. An operator normally creates a
project and agent, then creates work in the project backlog or directly in an
agent queue through `factoryctl` or the `factory-tui` board. The local socket
currently does not prove that the caller is human (see #133/#127). The daemon's
dispatcher spawns that agent's resident
session automatically if none is live, or delivers into it if one already is
idle — there is no separate explicit "start" step in the common case. The
daemon starts the session through `factory-runner`, drives its state entirely
from the provider's own hook invocations, and streams persisted state to
disposable observers. A launch is proven only after a real provider command
has run and an observer and daemon have both restarted without stopping or
misidentifying it.

Queue vocabulary is intentionally narrow: a queue is the stable ordered work
assigned to one agent, using `(created_at_ms, id)` delivery order. Unassigned
queued tasks form the project backlog; private messages form the inbox; and
approval/proposal attention is review work. Creating directly for an agent is
one SQLite transaction that inserts the assignment with the task, publishes
one task event, and only then wakes that agent. Moving a queued task uses the
same assignment field, including `NULL` for the backlog, so no other worker
can observe a transient delivery.

Each task row has an internal immutable incarnation identity and a monotonic
work revision, separate from its operator-facing task id. A reservation binds
both identities and a preallocated run id into
`session_work`; only that exact attempt's atomic acknowledgement makes a
concurrent commit idempotent. Retrying or editing advances the work revision;
deleting and recreating the same task id also gets a new incarnation.

### Runtime-resolved provider settings

An agent profile's configured overrides are not a claim about what a provider
actually ran. At session spawn, authoritative launch/config evidence is stored
on that session for model, reasoning effort, permission mode, and control mode.
The values remain historical after the session ends; missing evidence is
explicitly unreported rather than inferred from a provider default. The local
API, `factoryctl agent status`, and `factory-tui` all project the same fields.

### Auditable model tiers

The shared `factory_core::model_policy` normalizes the final desired profile
for creation and update, so the daemon, CLI, and TUI cannot disagree. New
Codex routine workers and focused reviewers without an explicit escalation
receive `gpt-5.6-luna` with `medium` reasoning; the orchestrator/God is fixed
to `gpt-5.6-sol` with `xhigh`; a worker receives Sol/xhigh only with an
explicit high-risk reason. Provider model/effort capabilities are declared by
this same policy table, and unsupported values fail before launch. The
selected model, reasoning effort, and reason are durable profile fields and
are projected through `factoryctl` and the TUI. Existing profiles remain
unchanged; this is a bounded tier policy, not a live pricing engine. Local
protocol version mismatches are rejected rather than silently dropping policy
fields. This PR does not implement the human authorization boundary for agent
creation; that remains #133/#127.

The resumed-Codex delivery lifecycle repair is tracked by
[#170](https://github.com/baziyer/dark-factory/issues/170): live evidence shows
that after a clean stop and provider-thread resume, a completed task followed
by a newly assigned task can reach `delivery unacknowledged` without creating
a run or typing the new task body. Its regression shape is completed task →
stop resident session → resume the same provider thread → assign the next task;
the acceptance condition is the new task body entering a running task.

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
- `StopSession` acknowledges the runner's stop request before its provider
  process necessarily exits. While that stop intent is durable, queued work
  is never typed into the still-live session; the dispatcher waits for the
  terminal event and then starts the successor, resuming the provider thread
  when its capabilities support that. A reserved delivery interrupted before
  the external-effect claim may retry its stored bytes. After the claim, a
  missing provider hook leaves the session durably `waiting_for_input` with
  `delivery unacknowledged` and its authority `uncertain`; neither a journal
  retry nor a second submit is allowed. Exact session termination releases the fence
  so the still-queued task can deliver once into a successor. Stop admission
  shares the per-agent delivery slot and persists stop
  intent before signalling the runner, so no admitted delivery can open a
  later run after stop wins.
