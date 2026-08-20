# Safe kernel refactor epic

Status: architecture merged; Stage 1 implemented on a branch pending PR,
independent review, and merge, 20 August 2026

Architecture merge: `f4f17d05315368139e408296ade6e95982cff137`

Audit baseline: `c19091ea2eb2b20c2de8717cb14799340268c8c7`

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
- One non-interactive provider process exists for that attempt only.
- The process ends before the attempt becomes terminal.
- A later attempt may receive bounded retained context, but it starts a fresh
  provider conversation and process under new authority.
- A retained change, not a session or attempt, owns the review worktree across
  retries and review.
- God proposes priority and assignment. Factoryd alone admits work, grants
  capabilities, owns resources, and terminalizes attempts.

This deliberately gives up resident provider processes across tasks. That
product behavior creates most of the delivery acknowledgement, taskless tool
authority, resume recovery, and session-versus-run ambiguity now dominating the
daemon.

`RunId` is the attempt identity. Do not add a parallel public `AttemptId`,
aggregate, or table. `Queued` belongs to a task, not a run. The durable run
phase and outcome are separate:

```text
Task:       queued / running / blocked / succeeded / failed / cancelled
                |
                v
Run:        admitted -> running -> finalizing -> terminal
                 \----------/          |             |
                  exact capability      |             +-- durable outcome
                                        +-- immutable pending outcome
                                      |
                                      v
Resources: declared -> active -> releasing -> released
                                      \-> unresolved

Change:     owns one retained review worktree across serial attempts
```

`running` on the task is a projection written by the admission transaction and
retained until the finalizer writes the terminal task result. Outcome, stop
intent, operator wait, and cleanup failure are fields or related records. They
are not competing top-level lifecycle states.

The transition and projection contract is exhaustive:

| Trigger | Run transition | Pending outcome | Final task status |
| --- | --- | --- | --- |
| Provider proposes completion | `running -> finalizing` | `succeeded` | `succeeded` |
| Provider proposes a block | `running -> finalizing` | `blocked(reason)` | `blocked` |
| Spawn fails before provider exec | `admitted -> finalizing` | `failed(reason)` | `failed` |
| Provider/runner exits before an outcome | `running -> finalizing` | `failed(reason)` | `failed` |
| Operator cancels before the first `finalizing` transition | `admitted|running -> finalizing` | `cancelled(reason)` | `cancelled` |
| Finalizer proves every resource released/transferred | `finalizing -> terminal` | unchanged | exact mapping above |

The first durable transition to `finalizing` wins. An outcome received before
exit is preserved; exit received first records failure; late outcomes, exits,
and cancellation requests are idempotent no-ops. Cleanup failure never changes
the outcome: it leaves the run visibly `finalizing` with an unresolved
resource. Retry is an explicit, post-terminal operation that increments the
task work revision, returns that exact task incarnation to `queued`, and later
admits a new `RunId`; there is no implicit retry or reuse of run authority.

## Why this reset is necessary

The audited baseline named `session_work` as the work authority, but it owned
prompt delivery rather than all mutation and finalization. Other paths granted
or closed work independently:

| Concern | Competing authority on the audit baseline |
| --- | --- |
| Queue and admission | task status, dispatcher session checks, explicit task start |
| Delivery | `session_work`, delivery attempts, PTY/hook acknowledgement |
| Mutation | live session token, hook policy, repository request token |
| Completion | complete/block/cancel endpoints, provider exit, deadlines, recovery |
| Cleanup | runner acknowledgement, Tokio tasks, Rust `Drop`, `TempDir`, shell traps |

Concrete failures followed from that split:

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

These are boot-candidate invariants. Each intermediate PR must satisfy its own
stage checkpoint below and fail closed for capabilities delivered by a later
stage; it must not claim the complete boot contract early.

1. Every provider-mediated mutation derives its project, agent, task, attempt,
   change, source path, and allowed operation from one unforgeable attempt
   capability. Provider requests never select those identities.
2. No admitted attempt means no provider process, writable source lease, tool
   authority, repository mutation, or outcome authority.
3. Admission atomically captures the task incarnation and revision, provider
   configuration, change/worktree lease, and capability before any provider
   effect is possible.
4. Only `running` grants provider effect authority or permits initial provider
   `exec`. The first `finalizing` transition atomically revokes tool, mutation,
   repository, and outcome authority and requests stop. The exact provider
   process may still exist while draining or awaiting reap as an owned resource;
   it may not initiate another authorized effect.
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
| `factory-runner` | minimal provider-blind process host and blocked-exec handshake |
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

An intermediate stage is deliberately not bootable, but it must be internally
safe: unsupported later-stage operations fail closed and its stage checkpoint
passes. Stage 1 has no production Change allocator for new work. Its
module-private causal fixtures insert an exact Change row backed by a disposable
temporary directory; production source admission remains disabled until Stage
2. This is a test fixture, not a public abstraction or second ownership path.

### Stage 1: atomic attempt and resource-authority cutover

Purpose: replace resident sessions, delivery authority, endpoint-specific
terminalization, and killable cleanup in one internally complete cutover. The
resource/finalizer seam cannot safely be postponed to a later stage.

Checked items are present in the working branch. They are not accepted until
the exact-head local/hosted gates and independent adversarial review pass.

- [x] Reuse `RunId` for attempts and rebuild runs around the pure phase and
  outcome contract above. Add a private retained Change identity; do not add a
  parallel public attempt aggregate.
- [x] Add daemon-derived attempt principals and exhaustive, fail-closed request
  classification. A missing credential never means operator authority.
- [x] Make one transaction the only route from a queued task to an admitted run
  and the task's running projection.
- [x] Add the resource ledger and restartable finalizer before switching the
  process path. Register every process, group, runner, runtime root, temporary
  root, and disposable job used by attempt execution.
- [x] Add the blocked-child launch handshake. Declare intent, fork without
  provider execution, persist PID/PGID/birth fingerprint and resource state,
  move the run to `running`, then release the child to `exec`.
- [x] Replace caller-selected complete/block/cancel operations with the exact
  outcome contract above. The first durable transition to `finalizing` wins.
- [x] Make one finalizer the sole writer of terminal run/task state and resume
  it after daemon restart.
- [x] Replace resident cross-task sessions with one noninteractive provider
  process per run. Bounded retained context may seed a later run, but no native
  provider conversation or process is resumed.
- [x] Make God scheduling-only and give it no source mutation, process,
  capacity, agent administration, or repository authority.
- [x] Preserve a legacy per-agent worktree only as a retained Change path until
  Stage 2; never create, delete, or infer ownership for it during migration.
- [x] Remove message-only provider turns, generic unaffiliated request replay,
  task-start source overrides, session lifecycle APIs, delivery
  acknowledgement/replay, direct episode opening, idle dispatch, direct
  terminalization, and session recovery.
- [x] Retain legacy session event decoding only for historical replay; stop
  producing live session events.
- [x] Make the cutover migration refuse any schema-29 database with a live
  session, non-empty/quarantined session-work row, active delivery, or
  nonterminal run. Never infer whether an uncertain external prompt executed.
- [x] Preserve every legacy agent worktree as an unlinked retained Change
  record. Do not inspect, move, clean, delete, or automatically adopt it into a
  new task. Require an explicit database backup/rollback decision before the
  first schema-30 boot.

Stage checkpoint:

- No provider effect authority exists outside one exact `running` run. A
  provider process may remain during `finalizing` only as an authority-revoked,
  stop-requested resource being drained or reaped; no provider process survives
  terminal.
- Every supported hook and outcome is refused unless the exact run is
  `running` and the request is in that principal's allowlist. Repository
  execution is absent until Stage 2 can bind it to an exact Change.
- Spawn failure, provider exit, success, block, and cancellation all converge
  through `finalizing`; restart resumes the same finalizer.
- No successor run is admitted until the earlier run is terminal.
- Reused PID, job, path, or runner identities are refused rather than killed or
  deleted.
- No run becomes terminal with an active or unresolved ephemeral resource.

Do not boot Dark Factory after this intermediate stage.

### Stage 2: daemon-owned changes and provider source views

Purpose: make factoryd the sole supported product path for source-worktree
administration and prevent accidental recursive worktree creation.

- [ ] Give each retained Change one factoryd-created worktree and lease it
  serially to runs. A retry reuses the Change only after the previous run is
  terminal.
- [ ] Place managed Changes under a daemon-owned root, not underneath whichever
  checkout invoked a helper.
- [ ] Give the provider a writable source view with no `.git` file or other Git
  administrative locator. Factoryd retains the worktree gitdir privately and
  performs status/diff/commit/push/PR operations for the exact running Change.
- [ ] Prove that ordinary `git worktree add` from the provider source view fails
  while factoryd repository operations continue to work.
- [ ] Require a Git repository for source mutation; remove fallback mutation in
  the shared project root.
- [ ] Remove per-agent worktrees, caller-selected paths, `--worktree` protocol
  fields, and provider guidance that invokes the development worktree helper.
  Keep any human helper explicitly outside product authority.
- [ ] Give unique retained Changes an operator-visible soft byte bound and a
  hard admission count/byte cap. At the cap, refuse new Change admission and
  request an operator retention/removal decision; never automatically delete
  unique or dirty work.

Stage checkpoint:

- Factoryd is the only supported product path that creates or administers a
  Change worktree.
- A confused provider starting in its source view cannot recursively create a
  Git worktree through ordinary repository discovery.
- Active and retained Changes are reported exactly; reaching the hard cap stops
  admission without deleting unique data.

The same-UID hostile-process caveat still applies: cryptographic or OS-level
filesystem isolation requires the separate hardened-runner project.

Do not boot Dark Factory after this intermediate stage.

### Stage 3: bounded builds, immutable bundles, and regenerable storage

Purpose: remove per-worktree targets, mutable binary launch, and unbounded
regenerable runtime output without multiplying the cache by source revision.

- [ ] Add a bounded mutable Cargo cache namespace keyed only by project
  identity plus toolchain, target triple, profile, feature/package/target
  selection, and compiler/linker configuration. Exact source revision is not a
  cache namespace dimension.
- [ ] At build admission, atomically revoke Change mutation authority, wait for
  registered/in-flight source writers to quiesce, create an exact Git tree with
  a private index, and materialize that tree into a daemon-owned immutable
  source snapshot. Restore mutation authority only after snapshot publication
  or fail-closed cleanup.
- [ ] Hold one writer lease through immutable source selection, build, open,
  copy, hash, sync, and atomic bundle publication. Compilation reads only the
  immutable snapshot, never the live writable Change.
- [ ] Put exact source snapshot, `Cargo.lock`, allowlisted build environment,
  cache configuration, executable digest, and required fixture digests in the
  immutable bundle provenance manifest.
- [ ] Publish daemon-owned content-addressed executable and required-fixture
  bundles. Verify by digest and execute an already verified file descriptor or
  equivalent immutable handoff.
- [ ] Hold reader/execution leases so reclamation cannot remove a live bundle.
- [ ] Hard-bound regenerable caches, bundles, runtime roots, and logs by bytes
  and count; report active, regenerable, retained, unresolved, and reclaimable
  storage separately.
- [ ] Reclaim only registered, identity-matched, unleased regenerable
  resources. If protected regenerable resources alone exceed a hard bound,
  refuse new build admission until leases release.
- [ ] Prove multiple source revisions reuse the same mutable cache namespace
  while producing distinct exact-source immutable bundles.
- [ ] Mutate the live Change after snapshot selection and during compilation;
  prove the bundle still corresponds exactly to the selected snapshot. If
  stable snapshot publication cannot be proven, fail closed before compiling.
- [ ] Delete runtime Cargo builds, mutable sibling executable discovery, and
  obsolete headroom-only logic after replacement proof.
- [ ] Keep local-CI serialization daemon-independent. Replace its shell lease
  only if a standalone Rust helper remains usable while factoryd is absent and
  produces measured net deletion with equal causal coverage.

Stage and boot checkpoint:

- Executable replacement after bundle preparation cannot change what runs.
- Regenerable quota pressure converges immediately when safe entries suffice.
  If protected leases alone exceed the bound, the daemon reports the exact
  overage, refuses new build admission, and converges only after leases release;
  it never touches unique retained Changes.
- Multiple revisions do not create multiple Cargo cache namespaces for the same
  project build configuration.
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
| Outcome/exit race | Exercise outcome-before-exit and exit-before-outcome. The first durable `finalizing` transition wins, late signals are harmless, and task projection matches the immutable outcome. |
| Completion ordering | After a provider proposes success, another mutation and successor admission are refused until process drain and resource release complete; only then may run/task become terminal. |
| Failure, cancel, retry | Spawn failure, provider crash, and operator cancellation each finalize to the documented task result. Retry is refused before terminal, then creates a new RunId and work revision while retaining the same Change. Stale credentials remain invalid. |
| External finalization | Own a process group, runtime root, temporary root, and throwaway launchd job. Kill provider, runner, daemon, and fixture separately. Restart reaps only exact fingerprint-matched resources and leaves reused identities untouched. |
| Immutable launch | Prepare A, replace mutable Cargo output with B, then launch: A runs. Tampered bundles fail closed. Restart between prepare and launch preserves the same manifest/digest. Reclamation cannot remove an execution-leased bundle. |
| Immutable source | Select source snapshot A, then mutate the live Change before and during compilation. The bundle is built entirely from A and records A's exact tree, or publication fails closed; it never records a mixed or later tree. |
| Cache reuse | Build two exact source revisions with the same project/build configuration. They use one mutable cache namespace and publish different source-bound immutable manifests. |
| Bounded storage | When safe regenerable entries suffice, reclamation reaches the hard bound and is idempotent. When protected leases alone exceed it, new build admission is refused and exact overage is reported until lease release permits convergence. Separately exceed the unique-Change admission cap and prove new work is refused without deleting retained data. |
| Source-view boundary | The provider source view has no discoverable Git administrative locator; ordinary worktree creation fails. Exact factoryd status/diff/commit operations still target the retained Change. |
| God policy only | God may propose priority and assignment. Worktree creation, process launch, repository publication, outcome submission, capacity/budget mutation, and operator control fail. Killing God does not change finalization. |

The design PR runs no process or launchd fixtures. Implementation proof may and
must start isolated source-built processes once the external cleanup verifier
for that fixture exists. Every such test uses a temporary `DARK_FACTORY_HOME`,
an explicit temporary socket, unique disposable job labels, and an independent
post-test reaper/verifier. It must never address the operator installation,
socket, home, plist, or job label.

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

For #268, extract shapes and tests rather than cherry-picking its six-commit
implementation: credential/redaction from `9d5df79`, bearer uniqueness from
`e1de1c1`, and exact-head fail-closed classification/race tests from
`00fba4b7`. Do not retain its parallel principal lifecycle, filesystem lock, or
immediate completion/blocking terminalization.

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

The baseline deletion map is reviewable rather than aspirational:

| Superseded production area | Baseline location | Expected gross deletion |
| --- | --- | ---: |
| Resident dispatch, delivery, deadlines, session launch/recovery | `execution.rs` delivery/session ranges | 3,000-4,000 |
| Session, episode, delivery journal, direct terminalization | `store.rs`, `session_work.rs` | 3,000-4,000 |
| Session protocol/API, generic outbox, resume clients/projections | `local_api.rs`, `factory-core`, providers, CLI/TUI | 2,000-3,000 |
| Per-agent worktrees and caller path selection | worktrees, paths, API/CLI | 500-1,000 |
| Mutable sibling launch and obsolete build/headroom paths | runner/build scripts and fixtures | 300-800 |

The daemon-independent local-CI lease is excluded unless its replacement is
both smaller and equally causal. Stage 1 should delete or replace roughly
8,000-10,000 lines while adding 3,000-4,000 lines of kernel and proof, for
about 4,000-6,000 net deletion. Later stages must not erase that reduction.

Target across the epic:

| Measure | Target |
| --- | ---: |
| Gross production deletion | 9,000-13,000 lines |
| New production kernel/cache code | 4,000-7,000 lines |
| Net production reduction | at least 5,000 lines; stretch 8,000 |
| New crates or one-implementation framework traits | 0 |
| Competing terminalization paths at completion | 0 |

Failure to reach the line target is not itself a correctness failure. Failure
to delete superseded authority paths is.

## Progress record

Fill merge/evidence columns only after a PR is merged. A pending status may
record branch implementation, but must not imply review, gates, or acceptance.

| Stage | PR(s) | Merged SHA | Net production lines | Local gate | Hosted gate | Adversarial review | Status |
| --- | --- | --- | ---: | --- | --- | --- | --- |
| Architecture decision | #281 | `f4f17d05315368139e408296ade6e95982cff137` | docs only | docs checks | required passed | [ALLOW](https://github.com/baziyer/dark-factory/pull/281#pullrequestreview-4987687825) | Merged |
| 1. Attempt/resource cutover | pending | — | pending exact count | pending | pending | pending | Implemented on branch; not reviewed or merged |
| 2. Change/source ownership | — | — | — | — | — | — | Not started |
| 3. Build/bundle/storage | — | — | — | — | — | — | Not started |
| Boot review | — | — | — | — | — | — | Frozen |

For every implementation PR:

- state its exact current-main parent and review `base..head`, not only the
  GitHub three-dot projection;
- list the old authority paths deleted in that PR;
- run focused causal tests first and the authoritative local gate on the exact
  reviewed head;
- distinguish local proof, hosted CI, merge, release, and live verification;
- obtain independent adversarial review and resolve every finding;
- never start or modify the operator installation. Isolated source-built
  daemons and disposable fixtures are permitted only under the causal-test
  boundary above. Do not publish a release, install, or delete preserved
  worktrees as part of implementation proof.

## Boot gate

Dark Factory remains frozen until all three stages are merged and an
independent review confirms:

- the run phase/outcome contract is the only work and terminalization authority;
- no provider can exist or mutate while taskless;
- factoryd can finalize exact external resources after crashes and restarts;
- factoryd is the only supported product path that creates or administers
  Change worktrees, and provider source views expose no Git administrative
  locator;
- builds and executable launches use the bounded cache and immutable bundles;
- storage reporting and reclamation are identity-safe and bounded;
- God is policy-only and GitHub intake is still quarantined;
- the full causal matrix and authoritative local/hosted gates pass on the exact
  candidate head;
- additions/deletions and any surviving compatibility machinery have been
  independently challenged.

Re-enabling auto, starting provider work, installing, releasing, or
changing `~/.dark-factory` is a separate explicit operator decision after this
gate. Passing the gate does not perform those actions.
