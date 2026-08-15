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
   spool for diagnosis and recovery.
5. Provider adapters translate structured provider output into the shared
   protocol. A PTY is an adapter-specific last resort, never the system's state
   model.
6. The local control and event API uses a private Unix socket by default. A
   subscription captures a durable replay head and marks when it has caught up.
   Inbound HTTP webhooks are an explicit, authenticated listener; receiving a
   message is a durable write before it wakes the orchestrator.
7. The UI repaints on input or factory events. Elapsed-time labels may request a
   coarse repaint while visible; no background animation or state polling is a
   source of truth.
8. The orchestrator uses the same durable task and message interfaces as other
   clients. It may choose work, but it cannot bypass daemon-owned limits.

## First vertical slice

One project has one persistent orchestrator task and one worker task. `factoryd`
stores both, starts the worker through `factory-runner`, normalises structured
provider events, and streams persisted events to a sparse observer. The slice is
complete only after a real provider command has run and the observer has been
closed and reopened without affecting it.

## Deliberately unresolved

- Exact daemon-upgrade handoff is deferred until runner reconnection works in
  the vertical slice. Initial recovery may mark unverifiable runs failed rather
  than risk attaching to the wrong process.
- Webhook exposure beyond loopback and its authentication scheme will be chosen
  with the first inbound/outbound messaging slice.
- Repository visibility is private during early dogfooding; making it public is
  a separate product decision.
