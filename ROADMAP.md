# Dark Factory roadmap

Live use remains frozen until an independent exact-main boot review passes.
GitHub issues and pull requests are the execution record; this file records
only product order and architectural boundaries.

## Phase A — exact-main boot proof

The attempt kernel and its immediate architecture hygiene are on `main`:

- one fresh provider process belongs to one admitted run;
- admission transactionally checks dispatch and capacity, selects the current
  canonical queue head, and freezes the derived role, provider, task revision,
  Change lease, and typed execution mode;
- worker and orchestrator mutations are restricted by the durable agent
  hierarchy inside the same transaction as the mutation;
- dispatch enablement is independent from `PlanOnly`, `WorkspaceWrite`, and
  `Unrestricted` provider authority;
- executable HTTP webhook intake and repository publication are absent; and
- daemon finalization owns provider, verifier, runtime, and storage recovery,
  including process-group leader loss.

Before live use, run the complete causal matrix against one exact `main` head,
soak only isolated temporary homes, and obtain an independent adversarial boot
decision. Keep release, installation, external intake, and the operator's live
home outside that proof. Do not start broad move-only decomposition until the
reviewed boot head is fixed.

## Phase B — behavior-free decomposition

Split only surviving responsibilities, in reviewable serial changes:

1. Store conventional domains and migrations.
2. Store attempt and resource ownership.
3. Execution management, admission, launch, observation, and finalization.
4. Execution Change and Rust maintenance.
5. Local-API framing, authentication, subscriptions, and domain handlers.
6. Change materialization.
7. Rust verification.
8. Runner supervision.
9. Provider launch and configuration.
10. CLI and TUI command, input, and projection modules.

Each change uses an exact `base..head` diff and preserves schema, wire, text,
filesystem, and process semantics. File size alone is not a reason to split;
the result must expose a real ownership boundary without adding a framework.

## Phase C — semantic simplification

- Introduce one concrete control layer only where it deletes socket-handler
  choreography.
- Replace manual write/delete gate cleanup with narrow RAII leases.
- Separate agent settings, rules, and memory operations so database and
  filesystem failure semantics are honest.
- Make attempt sources, resource identities, paths, digests, revisions, and
  persisted enum conversion typed at their internal boundaries.
- Pass `FactoryLayout`, tool paths, and policies explicitly instead of reading
  ambient state in execution paths.
- Version durable events independently from the local protocol and keep legacy
  forms behind one decoder/upcaster.
- Delete compatibility and placeholder fields that have no authoritative
  producer or current consumer.

## Phase D — measured deletion

- Benchmark the current Rust verifier against a simpler Cargo-owned test path;
  retain snapshot, cache, bundle, and manual-execution machinery only where its
  measured protection justifies the code and recovery surface.
- Reassess public Rust cache/storage status if it is not an operator need.
- Reassess the conservative shell tripwire after a hardened execution boundary
  exists; do not grow it into a shell interpreter.
- Consolidate filesystem identity and private-file helpers only where their
  semantics and failure modes are demonstrably identical.

## Boundaries to preserve

- Five crates reflecting real process and dependency boundaries; no
  micro-crate decomposition.
- One SQLite `Store`; no ORM, actor framework, generic saga, or repository and
  service traits with one implementation.
- One exhaustive principal policy and one durable attempt authority.
- One restartable finalizer as the only terminal writer.
- Provider adapters describe launch only.
- Orchestrators propose scheduling policy; `factoryd` enforces correctness.
- CLI and TUI use the same daemon operations.
- Public events stay bounded and omit credentials, prompts, raw output,
  messages, source, and private deliberation.
- Unique retained work is never an automatic storage-reclamation target.

Prefer deleting an obsolete lifecycle over moving it into a smaller file.
