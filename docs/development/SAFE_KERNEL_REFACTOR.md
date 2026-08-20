# Safe kernel refactor epic

Status: proposed architecture, 20 August 2026

Baseline: `c19091ea2eb2b20c2de8717cb14799340268c8c7`

Factory state: frozen; do not start provider work until the boot gate below is met

This document is the durable plan for replacing Dark Factory's overlapping
work, process, worktree, and build authorities with one small daemon-owned
kernel. GitHub issues and pull requests are the execution record; this document
owns the architecture, order, deletion targets, and definition of done.

The goal is not to preserve the current system behind smaller files. The goal
is to remove whole lifecycle paths, make the remaining authority explicit, and
only then split code that still has more than one reason to change.

## Executive decision

The smallest safe model is stricter than the current resident-session design:

- A queued task has no provider process.
- Admission creates one exact attempt, one scoped capability, and one lease on
  a factoryd-created change worktree.
- One provider process and PTY exist for that attempt only.
- The process ends before the attempt becomes terminal.
- A new attempt may resume provider conversation metadata, but it starts a new
  process under new authority.
- A retained change, not a session or attempt, owns the review worktree across
  retries and review.
- God proposes priority and assignment. Factoryd alone admits work, grants
  capabilities, owns resources, and terminalizes attempts.

This deliberately gives up resident provider processes across tasks. That
product behavior creates most of the delivery acknowledgement, taskless tool
authority, resume recovery, and session-versus-run ambiguity now dominating the
daemon.

`Queued` belongs to a task, not an attempt. The durable attempt state machine is:

```text
Task:       queued / blocked / complete
                |
                v
Attempt:    admitted -> running -> finalizing -> terminal
                                      |
                                      v
Resources: declared -> active -> releasing -> released
                                      \-> unresolved

Change:     owns one retained review worktree across serial attempts
```

Outcome, stop intent, operator wait, and cleanup failure are fields or related
records. They are not competing top-level lifecycle states.

## Why this reset is necessary

The current source names `session_work` as the work authority, but it owns prompt
delivery rather than all mutation and finalization. Other paths still grant or
close work independently:

| Concern | Current competing authority |
| --- | --- |
| Queue and admission | task status, dispatcher session checks, explicit task start |
| Delivery | `session_work`, delivery attempts, PTY/hook acknowledgement |
| Mutation | live session token, hook policy, repository request token |
| Completion | complete/block/cancel endpoints, provider exit, deadlines, recovery |
| Cleanup | runner acknowledgement, Tokio tasks, Rust `Drop`, `TempDir`, shell traps |

Concrete failures follow from this split:

- A taskless live session can invoke tool hooks and repository operations without
  proving an admitted attempt.
- Task completion and blocking name a task rather than deriving it from an
  exact attempt capability;
- completion releases durable work ownership before the provider process and
  external resources are known to be quiescent;
- failed runner stops can leave an old provider alive while a replacement is
  launched into the same worktree;
- factoryd does not durably persist enough process identity to reap after the
  runner or test process that owns `Drop` is killed;
- callers can select source paths, non-Git projects can fall back to the shared
  project root, and the development worktree script recursively creates nested
  worktrees when invoked inside a managed checkout;
- each worktree defaults to an independent Cargo target, while executable
  fixtures launch mutable `target/debug` siblings.

The storage incident measured 77 independent Cargo targets consuming about
327 GiB. Headroom refusal reduced the chance of an opaque compiler failure but
did not change the worktree-to-target multiplication that caused it; issue
[#223](https://github.com/baziyer/dark-factory/issues/223) remains the evidence
and implementation tracker for that part of Stage 3.

The concentration is substantial on the baseline above:

| Source | Lines |
| --- | ---: |
| `factoryd/src/store.rs` | 8,167 |
| `factoryd/src/execution.rs` | 6,145 |
| `factoryd/src/local_api.rs` | 4,479 |
| `factory-runner/src/lib.rs` | 3,154 |
| `factoryd/src/runner_process.rs` | 1,277 |
| CI lease shell implementation and focused tests | 1,533 |

File size is evidence of accumulated responsibility, not the refactor target.
The refactor target is one authority and one owner for each transition and
resource.

## Kernel invariants

These invariants are the acceptance contract for every implementation PR:

1. Every provider-mediated mutation derives its project, agent, task, attempt,
   change, source path, and allowed operation from one unforgeable attempt
   capability. Provider requests never select those identities.
2. No admitted attempt means no provider process, writable source lease, tool
   authority, repository mutation, or outcome authority.
3. Admission atomically captures the task incarnation and revision, provider
   configuration, change/worktree lease, and capability before any provider
   effect is possible.
4. Only `running` permits provider execution. A provider outcome moves the
   attempt to `finalizing`; it does not directly close the task or attempt.
5. One restartable finalizer is the only code allowed to write `terminal`.
   Terminalization requires all ephemeral resources to be released or a
   retained artifact to be durably transferred to its next owner.
6. Process, process-group, runtime-root, temporary-root, launchd-job, build,
   bundle, and execution identities are registered durably and reconciled by
   factoryd. `Drop` and shell traps remain optional fast cleanup, never
   correctness authority.
7. A retry creates a new attempt. It may lease the same retained change only
   after the preceding attempt is terminal.
8. Factoryd creates, records, and removes change worktrees. Providers receive a
   leased source directory but cannot choose or create source paths through the
   product API.
9. Rust builds use a bounded project cache with one writer lease per project
   configuration. Executions use daemon-owned prepared bundles, not mutable
   Cargo outputs.
10. God has scheduling capabilities only. Its absence, confusion, or death
    cannot bypass admission, authority, finalization, storage, or security
    invariants.
11. GitHub intake remains quarantined until the complete kernel boot gate is
    met.

Dark Factory's current same-UID model can enforce these invariants against
mistaken and cooperative providers. It cannot claim isolation from a hostile
provider running as the operator. That stronger claim requires a separate OS
user, container, or sandbox and remains a distinct security project.

## Ownership boundaries

Keep the existing five crates unless implementation evidence proves a process
or dependency boundary has changed:

| Owner | Responsibility after the refactor |
| --- | --- |
| `factory-core` | attempt/resource/change domain and bounded wire types |
| `factory-runner` | minimal provider-blind PTY/process host and blocked-exec handshake |
| `factoryd` | durable admission, capabilities, resources, finalization, worktrees, cache |
| `factoryctl` | operator and attempt-scoped client requests; no hidden lifecycle logic |
| `factory-tui` | operator projection through the same API as `factoryctl` |

Keep one SQLite `Store`, one migration chain, and ordinary inherent
implementations. Do not add repository/service traits with one implementation,
an ORM, actor framework, generic saga, event-sourcing framework, or new
micro-crates.

Inside factoryd the intended dependency direction is:

```text
domain -> store -> control -> API/webhooks
                   |
                   +-------> execution/resource effects
```

A small concrete `ControlPlane` is useful only if it replaces direct API
coordination. It owns principal resolution, lock ordering, durable
transactions, external-effect ordering, and execution wake-up. It must not be
added beside the existing handler orchestration while both remain live.

## Delivery plan: three serial implementation stages

The default is one reviewed PR per stage. A stage may be split once at a
demonstrably independent seam, but its PRs remain serial and the superseded
path is deleted in the same stage. Do not run parallel architecture branches.

### Stage 1: one attempt owns every mutation

Purpose: replace session, run, delivery, and endpoint-specific work authority
with one exact admitted attempt.

- [ ] Add the pure attempt transition model and a retained change identity.
- [ ] Add daemon-derived attempt principals and exhaustive, fail-closed request
  classification.
- [ ] Make admission the only route from a queued task to executable work.
- [ ] Replace caller-selected complete/block/cancel operations with an
  attempt-scoped proposed outcome.
- [ ] Replace resident cross-task sessions with one provider process per
  attempt; provider resume data may be input to a new launch.
- [ ] Remove message-only provider turns and generic unaffiliated request
  replay.
- [ ] Remove task-start and agent-create source-path overrides from the product
  protocol.
- [ ] Make God scheduling-only and give it no writable worktree.
- [ ] Project old session/run vocabulary only where a temporary protocol
  compatibility boundary requires it.
- [ ] Delete delivery acknowledgement/replay, direct episode opening, idle
  session dispatch, and competing task terminalization paths.

Stage checkpoint:

- No provider exists without one admitted attempt.
- Every provider hook, repository mutation, and outcome is rejected without
  the exact current attempt capability.
- Outcome submission reaches `finalizing`, never `terminal`.
- No successor attempt can be admitted while an earlier attempt for that
  change is non-terminal.

Do not boot Dark Factory after this intermediate stage.

### Stage 2: durable resources and one finalizer

Purpose: make cleanup survive the death of the provider, runner, daemon, or
test process that created the resource.

- [ ] Add a durable resource ledger with exact owner, kind, desired state,
  observed state, locator, birth fingerprint or digest, retries, timestamps,
  and bounded last failure.
- [ ] Declare process intent before spawn.
- [ ] Add a blocked-child launch handshake: the runner forks a child held on a
  private release pipe; factoryd persists PID, PGID, birth fingerprint, runner,
  and runtime-root identity, moves the attempt to `running`, and only then
  permits provider `exec`.
- [ ] Reconcile declared, active, releasing, and unresolved resources at
  startup and periodically.
- [ ] Route success, failure, cancel, timeout, provider exit, runner exit, and
  daemon restart through the same idempotent finalizer.
- [ ] Register and reconcile runtime roots, temporary roots, launchd jobs, and
  build/execution leases through adapters owned by the same ledger.
- [ ] Guard terminalization in the database on all ephemeral resources being
  released or explicitly transferred.
- [ ] Delete runner acknowledgements, Tokio task lifetime, `Drop`, test guards,
  and shell traps as lifecycle authority. Retain small best-effort cleanup only
  where it still reduces ordinary leakage.

Stage checkpoint:

- Killing each participating process at every launch/finalization boundary
  converges after daemon restart.
- Reused PID, job, path, or runner identities are refused rather than killed or
  deleted.
- No attempt becomes terminal with an active or unresolved ephemeral resource.

Do not boot Dark Factory after this intermediate stage.

### Stage 3: daemon-owned source, builds, bundles, and storage

Purpose: remove recursive worktree creation, per-worktree targets, mutable
binary launch, and unbounded retained runtime output.

- [ ] Give each retained change one factoryd-created worktree and lease it
  serially to attempts.
- [ ] Require a Git repository for source mutation; remove fallback mutation in
  the shared project root.
- [ ] Remove provider guidance and protocol paths that create or select
  worktrees. Keep any human development helper explicitly outside the product
  authority model.
- [ ] Add a bounded project build cache keyed by repository identity,
  toolchain, target triple, profile, feature/package/target selection,
  `Cargo.lock`, relevant compiler/linker configuration, allowlisted build
  environment, and exact source snapshot.
- [ ] Hold one writer lease through build, open, copy, hash, sync, and atomic
  bundle publication.
- [ ] Publish daemon-owned content-addressed executable and required-fixture
  bundles. Verify by digest and execute an already verified file descriptor or
  equivalent immutable handoff.
- [ ] Hold reader/execution leases so reclamation cannot remove a live bundle.
- [ ] Bound caches, bundles, runtime roots/logs, and retained worktrees by bytes
  and count; report active, regenerable, retained, unresolved, and reclaimable
  storage separately.
- [ ] Reclaim only registered, identity-matched, unleased resources. Dirty
  review worktrees require explicit retention or operator approval.
- [ ] Delete runtime Cargo builds, mutable sibling executable discovery, the
  shell CI lease implementation/tests, and obsolete headroom-only machinery
  after their Rust replacements are causally proven.

Stage and boot checkpoint:

- Factoryd is the only product path that creates a change worktree.
- Executable replacement after bundle preparation cannot change what runs.
- Quota pressure converges below the configured bound without touching active
  or retained work.
- All causal suites below pass on the exact reviewed head.
- An independent adversarial review explicitly approves re-enabling the
  factory.

## Causal proof matrix

Every test must fail for the intended reason against the pre-fix path and prove
the externally visible effect, not only an internal callback or row.

| Proof | Required injected boundaries and assertions |
| --- | --- |
| Crash/restart | Crash after admission, resource declaration, fork-before-release, exec release, provider exit, external deletion, and before ledger acknowledgement. Restart must produce at most one provider execution, no prompt replay, exact identity, and idempotent cleanup. |
| Taskless refusal | With no admitted attempt, no provider process exists. Old/forged capabilities, hooks, repository requests, outcome requests, and queued replays cannot mutate task, budget, repository, or provider state. |
| Completion ordering | After a provider proposes success, another mutation and successor admission are refused until process drain and resource release complete; only then may attempt/task become terminal. |
| External finalization | Own a process group, runtime root, temporary root, and throwaway launchd job. Kill provider, runner, daemon, and fixture separately. Restart reaps only exact fingerprint-matched resources and leaves reused identities untouched. |
| Immutable launch | Prepare A, replace mutable Cargo output with B, then launch: A runs. Tampered bundles fail closed. Restart between prepare and launch preserves the same manifest/digest. Reclamation cannot remove an execution-leased bundle. |
| Bounded storage | Exceed quota with active, retained, dirty, executing, and regenerable entries. Reclamation reaches the bound using only safe entries and is idempotent. |
| God policy only | God may propose priority and assignment. Worktree creation, process launch, repository publication, outcome submission, capacity/budget mutation, and operator control fail. Killing God does not change finalization. |

Dangerous process and launchd fixtures must use disposable identities and an
external cleanup verifier. They are not run during this design phase.

## GitHub issue and PR disposition

Issue [#242](https://github.com/baziyer/dark-factory/issues/242) should remain
the discoverable umbrella only after its body is replaced by, or reduced to a
link to, this epic. Its current "move first, semantics later" order is
superseded. Mechanical decomposition follows the safe kernel instead of
preceding it.

Consolidate the current issue set as follows:

| Epic area | Retain as evidence or execution issue |
| --- | --- |
| Attempt authority and finalization | #279, absorbing remaining #222/#227 and relevant #189 cases |
| Process/resource ledger | replacement for overloaded #267; absorb #165 and the causal #26 case; retain #280 as the launchd adapter suite |
| Worktree ownership | #276, absorbing #220 and #175 |
| Build cache and immutable launch | rewrite #223 around the project cache; extract immutable bundle proof from #201 |
| God policy | #191/#130 only after the kernel |
| Attempt principal | #133 principle, implemented narrowly in Stage 1 |
| GitHub intake | keep quarantined; no implementation work before the boot gate |

Open implementations are evidence, not an integration chain:

| PR | Disposition |
| --- | --- |
| #275 oversized-frame causal fixture | Retain independently. |
| #274 provider UUID deduplication | Retain independently after #275. |
| #271 issue-intake quarantine guidance | Retain independently. |
| #268 session-principal core | Extract credential redaction, daemon-derived identity, exhaustive classification, and fail-closed tests; supersede the broad implementation. |
| #272 store barrier fixture | Close/supersede; preserve the causal scenario. |
| #270 orchestrator wake | Supersede; preserve only coalescing/restart scenarios for post-kernel policy scheduling. |
| #265 blocked-task retry UI | Close/supersede; preserve stale-response tests after the attempt model exists. |
| #269, #264, #262 move-only extractions | Close/defer; re-evaluate only for code that survives semantic deletion. |

Closed PRs #277, #278, and #266 remain failure evidence. Do not reopen their
shell worktree transaction, ambient prepared-binary, or test-only supervisor
implementations.

No issue or PR should be edited or closed from this document change alone.
Those are deliberate operator actions after review of this plan.

## Reconciliation with the third-party architectural review

The review correctly identifies severe module concentration and several good
implementation constraints. It underweights the fact that the concentrated
modules contain authority paths the reset should delete.

| Recommendation | Decision |
| --- | --- |
| Keep the five crates and one rusqlite Store | Keep. These reflect real process and transaction boundaries. |
| Avoid repository/service traits, ORM, actors, and sagas | Keep. |
| Treat `session_work` as the landed durable authority | Reject the scope, keep the method. Its pure exact-identity transitions are a good implementation pattern, but today it authorizes delivery rather than every mutation and finalization. Replace it with the attempt model rather than adding another owner beside it. |
| Add an ordered migration registry and build historical fixtures from the real chain | Keep, as an early supporting slice inside Stage 1 if needed. Pass `FactoryLayout` explicitly; do not read ambient home paths in migrations. |
| Add one concrete principal-aware `ControlPlane` | Adapt. Introduce it only while deleting direct handler orchestration, and bind it to attempts rather than resident sessions. |
| Centralize `FactoryLayout`, narrow `pub` surface, separate event/protocol versions | Keep as follow-on deletion/simplification work where touched by the kernel. |
| Replace display-string routing and nullable conceptual variants with enums | Keep for surviving projections and cache/resource types. Do not type obsolete delivery states before deleting them. |
| Land #268 and #270 before central refactoring | Reject. Both encode lifecycle assumptions being replaced; extract narrow evidence and tests only. |
| Mechanically split Store, execution, and API first | Reject as the next step. Delete resident-session and competing terminalization machinery first, then split surviving owners mechanically. |
| Split all large client/provider modules as a broad sequence | Defer. Size alone is not an epic; split only at a stable owner boundary needed by real work. |

## Further opportunities to strip down the codebase

These are candidates, not automatic scope. The first group is coupled to the
kernel; the second should wait for measured post-kernel evidence.

### Delete with the kernel

- Remove `StartTask` as a second admission model. Queue assignment plus one
  admission transaction should be sufficient.
- Remove public session/run lifecycle commands in favor of attempt inspection,
  outcome, cancel intent, and resource status.
- Remove the generic serialized `LocalRequest` outbox. If offline durability is
  still needed, keep a narrow attempt-scoped outcome record.
- Remove message-only provider turns. Messages may influence the next admitted
  attempt or remain operator-visible; they do not justify a taskless model turn.
- Remove the second session-only socket proposal. One private socket with a
  mandatory principal envelope is simpler; same-UID isolation does not improve
  merely by adding another socket path.
- Remove non-Git source mutation and fallback-to-project-root behavior. A
  non-Git project may remain observable/read-only or be rejected explicitly.
- Remove per-agent source worktrees, caller-provided worktree paths, and
  provider-facing `--worktree` fields.
- Remove provider-state changes as proof of work ownership. Provider state is
  observation only.
- Remove provider-thread delivery recovery, invisible prompt nonces, stop-hook
  delivery, and delivery-attempt replay once one process starts with one
  admitted attempt.
- Remove durable God cycle authority. A policy proposal is an ordinary request
  that factoryd validates; scheduling correctness does not need a second
  lifecycle ledger.

### Simplify after the kernel

- Expose `factoryd::run(DaemonConfig)` as the narrow library boundary and make
  internal modules `pub(crate)`; keep only true process/protocol tests external.
- Split agent settings, rules, and memory updates into honest DB-only and
  filesystem-only commands rather than pretending one cross-resource request
  is atomic.
- Separate local protocol, durable event schema, and runner protocol versions.
- Generate one current-install schema baseline while retaining the ordered
  legacy migration chain only for supported upgrades; tests construct old
  databases by applying real migrations through version N.
- Collapse status and attention projection around attempts/resources, deleting
  session-specific compatibility fields on one planned protocol bump.
- Replace human display text used in control flow with typed causes.
- Move row decoding beside its owning Store domain. Do not create `rows.rs`,
  `utils.rs`, or another dumping ground.
- Split a surviving large module only when a normal change otherwise needs two
  independent owners. Use move-only PRs with exact `base..head` review after
  semantics stabilize.
- Re-evaluate the shell provider after it has served the deterministic kernel
  tests. Keep it if it remains a small reference adapter; remove product-facing
  configuration if it exists only as test scaffolding.

## Deletion budget

Each implementation PR records production additions and deletions separately
from tests and generated migrations. Tests are not deleted to manufacture a
favorable total; race-specific tests are replaced by the causal matrix above.

Target across the epic:

| Measure | Target |
| --- | ---: |
| New production kernel code | 4,000-7,000 lines |
| Deleted production lifecycle/build/worktree code | 15,000-22,000 lines |
| Net production reduction | at least 8,000 lines |
| New crates or one-implementation framework traits | 0 |
| Competing terminalization paths at completion | 0 |

Failure to reach the line target is not itself a correctness failure. Failure
to delete superseded authority paths is.

## Progress record

Update this table only after a PR is merged. Record exact evidence rather than
"done" or "green" without provenance.

| Stage | PR(s) | Merged SHA | Net production lines | Local gate | Hosted gate | Adversarial review | Status |
| --- | --- | --- | ---: | --- | --- | --- | --- |
| Architecture decision | — | — | — | docs checks only | — | pending | Proposed |
| 1. Attempt authority | — | — | — | — | — | — | Not started |
| 2. Resource finalizer | — | — | — | — | — | — | Not started |
| 3. Worktree/build/storage | — | — | — | — | — | — | Not started |
| Boot review | — | — | — | — | — | — | Frozen |

For every implementation PR:

- state its exact current-main parent and review `base..head`, not only the
  GitHub three-dot projection;
- list the old authority paths deleted in that PR;
- run focused causal tests first and the authoritative local gate on the exact
  reviewed head;
- distinguish local proof, hosted CI, merge, release, and live verification;
- obtain independent adversarial review and resolve every finding;
- do not start Dark Factory, publish a release, install, merge, or delete
  preserved worktrees as part of implementation proof.

## Boot gate

Dark Factory remains frozen until all three stages are merged and an
independent review confirms:

- the attempt state machine is the only work and terminalization authority;
- no provider can exist or mutate while taskless;
- factoryd can finalize exact external resources after crashes and restarts;
- factoryd exclusively owns change worktrees;
- builds and executable launches use the bounded cache and immutable bundles;
- storage reporting and reclamation are identity-safe and bounded;
- God is policy-only and GitHub intake is still quarantined;
- the full causal matrix and authoritative local/hosted gates pass on the exact
  candidate head;
- additions/deletions and any surviving compatibility machinery have been
  independently challenged.

Re-enabling auto, starting provider sessions, installing, releasing, or
changing `~/.dark-factory` is a separate explicit operator decision after this
gate. Passing the gate does not perform those actions.
