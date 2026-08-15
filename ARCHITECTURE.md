# Architecture

Dark Factory separates deterministic process supervision from LLM decisions.
This file records constraints, not an aspirational component catalogue.

## Invariants

1. `factoryd` is the sole owner of processes, scheduling, dependencies,
   concurrency, retries, budgets, durable state, and health.
2. SQLite is the durable source of truth. State changes and their append-only
   events commit in the same transaction. The store uses WAL with `FULL`
   synchronous writes so acknowledged commands survive more than a process
   restart.
3. `factory-ui` and `factoryctl` are clients. Stopping, rebuilding, or losing
   either one cannot change the lifetime of an agent.
4. Each run is launched through a small, stable `factory-runner` process.
   Runners identify themselves by run ID so a restarted daemon can verify and
   adopt them instead of assuming a recorded PID is still the right process.
5. Provider adapters translate structured provider output into the shared
   protocol. A PTY is an adapter-specific last resort, never the system's state
   model.
6. The local control and event API uses a Unix socket by default. Inbound HTTP
   webhooks are an explicit, authenticated listener; receiving a message is a
   durable write before it wakes the orchestrator.
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
