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
3. `factory-ui` and `factoryctl` are clients. Stopping, rebuilding, or losing
   either one cannot change the lifetime of an agent.
4. Each run is launched through a small, stable `factory-runner` process.
   `factoryd` resolves a trusted absolute runner path before spawning it, clears
   the daemon's ambient environment to a fixed non-secret allowlist, and sends
   bounded task bytes only over runner stdin—not argv or environment variables.
   It creates a private socket and bounded event spool before directly spawning
   one process group. Events are appended before publication. Runners prove both
   run ID and a random runner-instance ID, retain terminal state until its exact
   sequence is acknowledged, and never adopt or signal from PID coincidence
   alone. A graceful runner signal stops the group but preserves an unacknowledged
   spool for diagnosis and recovery. After a daemon restart, adapter state is
   rebuilt by replaying that spool from sequence zero; SQLite's committed runner
   sequence is only the deduplication boundary for durable state and events. A
   terminal runner is marked reconciled only after its exact acknowledgement
   reply, or after its terminal outcome is durable and its runtime endpoint is
   proven absent; reconciliation itself is private cleanup, not a public state
   transition. The concrete Claude Code and Codex execution actor reserves work
   before launch, rebuilds the provider decoder from replay sequence zero,
   commits and publishes state before acknowledging a terminal runner, and
   drops observation on daemon shutdown without stopping the runner. A start
   request succeeds at durable
   reservation; runner readiness and completion are observed from run events,
   never hidden behind a long-lived request. Each run also persists observer
   health: `unknown` means no authenticated caught-up observation has been
   proven, `healthy` means the exact runner has replayed durably through its
   advertised head, and `degraded` means supervision was lost without proof
   that the runner stopped. Only healthy observation makes a missing endpoint
   authoritative after restart; degraded work remains assigned and recoverable.
   An adopted provider session is inseparable from its exact canonical
   worktree. Codex may additionally bind an explicit, canonical, same-owner
   `CODEX_HOME`; it is injected through a closed provider-environment type after
   the ambient environment is cleared. Resume never falls back to a fresh
   session or a different worktree/home. Claude uses its exact daemon-bound
   fresh or adopted session UUID on every launch and replay; both providers are
   executable from a greenfield database.
5. Provider adapters translate structured provider output into the shared
   protocol. Persisted observations contain bounded structural metadata and an
   explicitly user-visible, bounded final preview; raw reasoning, tool inputs,
   command output, and patches remain transient runner data. The native local
   control API may expose a bounded, sanitized tail of the private runner spool
   to the operator for inspection, but it never enters public events, webhook
   snapshots, or tracing. A PTY is an
   adapter-specific last resort, never the system's state model. The first
   Claude adapter uses `--safe-mode`, deliberately
   disabling ambient settings, hooks, plugins, MCP servers, and project
   instructions; enabling project Claude configuration requires a later explicit
   trusted-worktree policy.
6. The local control and event API uses a private Unix socket by default. A
   subscription captures a durable replay head and marks when it has caught up.
   Inbound HTTP webhooks are an explicit, authenticated listener; receiving a
   message is a durable write before it wakes the orchestrator.
7. The UI repaints on input or factory events. Elapsed-time labels may request a
   coarse repaint while visible; no background animation or state polling is a
   source of truth.
8. The orchestrator uses the same durable task and message interfaces as other
   clients. It may choose work, but it cannot bypass daemon-owned limits.

## First launch

`factoryd` starts from an empty database. A human creates projects, agents, and
tasks through `factoryctl` or its native UI and explicitly assigns each v1 run.
The daemon starts the worker through `factory-runner`, normalises structured
provider events, and streams persisted state to disposable observers. A launch
is proven only after a real provider command has run and an observer and daemon
have both restarted without stopping or misidentifying it.

## Deliberately unresolved

- Zero-downtime handoff between two daemon binaries is deferred. Ordinary
  restart recovery reconnects to exact stable runners; unverifiable identities
  fail visibly rather than risk attaching to the wrong process.
- Webhook exposure beyond loopback remains external. The daemon accepts
  exactly one owner-configured endpoint, on by default when its config file
  is present.
- Repository visibility is private during early operation; making it public is
  a separate product decision.
- Pause remains deferred, but stop intent is now durable: `StopRun` signals
  the exact live runner and, once the runner accepts it, persists
  `stop_requested_at_ms` on the run. When that run's terminal event lands,
  the daemon records `stopped` (not `failed`) and moves its task to
  `cancelled`; retry is an explicit requeue of a terminal task, including one
  stopped this way.
