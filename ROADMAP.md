# Dark Factory roadmap

Status: safe-kernel refactor, 20 August 2026

Dark Factory is frozen. The
[safe-kernel epic](docs/development/SAFE_KERNEL_REFACTOR.md) is the only active
architecture roadmap until its boot gate passes. GitHub issues and pull
requests are the execution record; this file records only product order and
boundaries.

## Current sequence

1. **Attempt and resource authority.** Replace resident sessions with one fresh
   process per admitted run; enforce exact attempt bearers; converge every
   outcome through `Finalizing`; let one durable daemon finalizer own cleanup
   and terminalization. Stage 1 is implemented on its branch and pending its
   PR, independent review, and merge.
2. **Change and source ownership.** Make `factoryd` exclusively create and
   lease retained Change worktrees. Give providers a source view without a Git
   administrative locator. Refuse shared-root/non-Git mutation and bound
   unique retained Changes without deleting them automatically.
3. **Build, bundle, and storage ownership.** Use one bounded mutable Cargo cache
   per project/build configuration, build from an immutable exact-source
   snapshot, publish digest-verified executable bundles, and reclaim only
   unleased regenerable resources.
4. **Boot review.** Run the causal crash/restart, taskless-refusal,
   external-finalization, source-view, immutable-launch, cache-reuse, and
   bounded-storage suites. An independent reviewer must explicitly approve
   re-enablement.

Stages are serial. GitHub intake stays quarantined, releases stay paused, and
the live installation stays untouched throughout.

## After the boot gate

Only then resume product work, in this order:

- validate the TUI and CLI around the same attempt/change projections;
- introduce external work through quarantined, typed, reviewable intake;
- add reusable personas and workflows without granting new authority;
- validate Linux and real-provider support separately from source-only tests;
- consider a hardened runner under a separate OS/container boundary.

## Boundaries to preserve

- Five crates reflecting real process/dependency boundaries; no micro-crate
  decomposition.
- One SQLite `Store`; no ORM, actor framework, generic saga, or repository and
  service traits with one implementation.
- One exhaustive principal policy and one durable attempt authority.
- One restartable finalizer as the only terminal writer.
- Provider adapters describe launch only.
- God proposes scheduling policy; `factoryd` enforces correctness.
- CLI and TUI use the same daemon operations.
- Public events stay bounded and omit credentials, prompts, raw output,
  messages, source, and private deliberation.
- Unique retained work is never an automatic storage-reclamation target.

Split a large module only when surviving code has distinct owners. Prefer
deleting an obsolete lifecycle over moving it into a smaller file.
