# Safe kernel contract and boot proof

Live use remains frozen until an independent exact-main boot review passes.

This document records the enduring kernel contract and the causal proof needed
for that review. It is not a merge diary. GitHub owns issue and pull-request
history; [the roadmap](../../ROADMAP.md) owns work after the boot decision.

## Kernel model

`RunId` is the attempt identity. Queued work has no run and no provider
process. Admission creates one run, one exact bearer, and, for a worker, one
lease on a retained Change.

```text
Task: queued --------> running ----------------------> terminal result
                         |
                         v
Run:  admitted -> running -> finalizing -> terminal
          |          |           |
          |          |           +-- immutable outcome request, no authority
          |          +-- exact bearer authorizes bounded effects
          +-- child may be prepared but cannot exec

Resource: declared -> active -> releasing -> released
                                      \----> unresolved

Change: one retained .git-free writable tree across serial attempts
```

The first durable transition to `finalizing` freezes the requested outcome and
revokes attempt authority. Success, block, failure, cancellation, provider
exit, and spawn failure all converge through that phase. Late observations are
idempotent and cannot replace the first request. A configured verification
failure may refine only proposed success to `failed(unverifiable)`.

Only the restartable daemon finalizer writes `terminal`. It does so after every
ephemeral resource is released and every retained artifact has transferred to
its next durable owner. Cleanup uncertainty leaves the run visibly
`finalizing`; it never becomes an invented success or absence.

## Admission and principals

Admission is one immediate Store transaction. It:

1. checks the durable dispatch switch and capacity;
2. loads the current agent and profile;
3. selects the canonical assigned queue head using the shared ordering;
4. derives role, provider, task incarnation, work revision, and typed execution
   mode;
5. reserves or reuses the exact Change where the role requires one;
6. writes the running task projection, admitted run, bearer digest, resources,
   messages, and events; and
7. returns the immutable launch target.

Execution cannot select an arbitrary queued task or provider. A reorder or new
higher-priority assignment before the transaction changes what is admitted;
the caller's stale observation does not win. Process-local locks may serialize
effects but never prove durable assignment or authority.

The bearer resolves to one exact project, agent, task, run, role, provider, and
Change. It authorizes effects only while that run is `running`. Attempt calls
cannot supply or widen any of those identities, and operator authentication
cannot impersonate an attempt for hooks or outcomes.

Relationship checks occur in the same Store transaction as the mutation:

- a worker may message itself, its immediate parent, or its nearest
  orchestrator ancestor;
- an orchestrator may message itself or a strict descendant;
- an orchestrator may create a child of its current task only when assigning
  it to a strict descendant, and may assign queued work only to a strict
  descendant; and
- attempts cannot edit tasks, unassign work, administer agents, or mutate
  factory policy.

Task and run ancestry never substitute for the durable agent hierarchy.

Dispatch and execution authority are separate. `dispatch_enabled` controls
only new admission. Every admitted run freezes one `PlanOnly`,
`WorkspaceWrite`, or `Unrestricted` mode derived from the agent profile;
changing the profile or dispatch later does not rewrite that run.

A retry creates a new run, bearer, runtime, and provider process. It may reuse
the retained Change only after the preceding run is terminal. It never revives
an old credential, conversation, or process.

## Process and resource ownership

One admitted run launches one fresh non-interactive provider process. Startup
input is written once to stdin and stdin closes. There is no resident provider,
PTY attach/input, prompt replay, delivery journal, or provider-process resume.

Launch uses register-before-exec gates:

1. factoryd records the run, runtime claim, and declared resources;
2. factoryd records an exact locked startup-file identity before an inert
   runner gate can inherit that lock and spawn;
3. factoryd records the gate's stable PID before activation;
4. the runner prepares a provider child blocked before `exec`;
5. factoryd records the provider PID, process group, and birth identity and
   moves the run to `running`; and
6. only then may the same child execute the provider.

The resource ledger owns processes, process groups, runners, runtime roots,
temporary roots, and verification effects. Stored process numbers support
absence checks, not signalling. The authenticated live runner may signal its
provider group only while it owns the unreaped leader child; recovery otherwise
waits for exact absence. Reused or weak identities stay unresolved.

A process group may outlive its leader. If the recorded leader disappears
while descendants remain, its numeric group ID is not signal authority and is
not proof of absence. Finalization, temporary-root cleanup, and cache
remeasurement remain pending until exact group absence is independently
established.

The runner's live-child guard preserves its bounded group authority across
cancellation or unwind and disarms immediately after a successful wait. Its
`Drop`, shell traps, and provider exit handlers may accelerate cleanup; none is
terminal authority.

## Change and repository ownership

For a worker, factoryd reserves one daemon-derived Change path for the exact
task incarnation. A registered parent-bound materializer selects one full
local Git commit, validates its bounded tree, reads exact blob objects, and
atomically publishes a plain writable tree. Partial clones and unsafe paths,
links, gitlinks, modes, or attribute-transformed export behavior are refused.

The provider view contains no `.git` administrative locator. Ordinary
repository discovery and linked-worktree creation fail. The product exposes no
status, commit, push, pull-request, or publication operation. Retries lease the
same retained Change serially; automatic storage reclamation never targets
unique retained work.

Managed removal requires an explicit operator request, no live lease, and the
exact recorded identity and revision. Replacement or ambiguity remains a
visible recoverable failure and is never touched. Source paths retained from
pre-kernel databases are metadata-only quarantine: factoryd does not inspect,
adopt, measure, launch from, rename, or delete them.

## Verification and regenerable storage

A project selects `None` or the fixed `RustWorkspaceTest` completion policy.
There is no generic provider-visible build API. For a Rust completion,
factoryd first moves the run to `finalizing`, revokes its bearer, and reaps all
provider resources. It then publishes a stable scan/copy/scan snapshot of the
Change or fails unverifiable.

Compilation uses one mutable cache keyed by project incarnation, exact Cargo
and rustc identity, target, and fixed policy. Source revision is provenance,
not a cache-namespace dimension. Cargo's top-level test executables are copied
into attempt-owned staging with a manifest binding source, build
configuration, toolchain, identities, and digests. The daemon re-verifies each
copy before launch and rechecks the stable source snapshot before and after
each test. Mutable Cargo output is never a top-level launch target.

The verifier is one registered effect with bounded output and time. Shutdown
publishes a private finish marker; a healthy live verifier kills its own group.
Recovery consumes only an atomically published result after exact group absence
and never rebuilds against later Change contents. The same leader-loss rule
above prevents premature completion or cache handoff.

Cache count is checked before effects. A writer makes measured bytes
incomplete. After its exact effect is absent, factoryd remeasures allocated
bytes and reclaims only identity-matched, unleased regenerable caches. A known
over-limit cache cannot be claimed for another run. Status reports measured
bytes, protected entry count, and recoverable failures without claiming an
instantaneous filesystem byte ceiling.

These identity and digest checks protect against mistaken and cooperative
substitution. Provider and test code still run as the operator; hostile
same-user isolation requires a separate OS user, container, or sandbox.

## External input boundary

There is no HTTP webhook or generic connector listener. Work enters only
through the authenticated private local API. A future integration must first
store an authenticated, bounded input envelope as a non-executable candidate,
then cross an explicit quarantine and acceptance boundary before it can create
or message work. External payloads must never materialize executable tasks
directly.

## Ownership boundaries

Keep the five crates that reflect real dependency and process boundaries:

- `factory-core`: current domain and bounded wire types;
- `factory-runner`: provider-blind process host and blocked-exec handshake;
- `factoryd`: Store, admission, principals, resources, finalization, Changes,
  and verification;
- `factoryctl`: operator and attempt-scoped requests with no lifecycle logic;
  and
- `factory-tui`: operator projections through the same API as `factoryctl`.

Keep one SQLite Store and one ordered migration chain. Do not add an ORM, actor
framework, generic saga, event-sourcing framework, micro-crates, or repository
and service traits with one implementation. Split a large module only when
surviving code has distinct owners; prefer deletion over relocating obsolete
authority.

## Causal proof matrix

Each proof must exercise an external effect or durable observation, not only a
callback or row:

| Proof | Required boundaries and assertions |
| --- | --- |
| Crash and restart | Inject failure after admission, resource declaration, each blocked-exec release, provider exit, external cleanup, and before acknowledgement. Restart yields at most one provider execution, no input replay, exact identity, and idempotent convergence. |
| Taskless refusal | With no admitted run, no provider exists. Old, forged, taskless, finalizing, and terminal credentials cannot mutate task, budget, source, or outcome state. |
| Queue race | Reorder or insert higher-priority assigned work between observation and admission. The Store admits only the canonical queue head selected inside its transaction. |
| Hierarchy scope | Construct alternate durable agent hierarchies and attempt cross-agent messaging, creation, and assignment. The same mutation transaction uses the stored hierarchy and refuses siblings, ancestors outside the worker allowance, and non-descendants. |
| Outcome and exit race | Exercise request-before-exit and exit-before-request. The first `finalizing` request wins; only failed configured verification may refine proposed success. |
| Completion ordering | After a success proposal, further attempt mutation and successor admission are refused until provider reap, exact snapshot and configured verification, release of every resource, and terminalization. |
| Failure, cancel, and retry | Spawn failure, provider crash, and cancellation converge through finalization. Retry is refused before terminal and creates a fresh run and bearer afterward. |
| External finalization | Kill provider, runner, verifier, and daemon at separate cuts. Recovery touches only exact resources. Leader loss with live descendants remains nonterminal past restart and converges only after exact group absence. The operator launchd service is outside attempt ownership, is not a `KernelResource`, and is proven separately by a release fixture. |
| Source boundary | Materialize one exact commit; repository discovery and ordinary worktree creation fail in the provider view. Removal refuses a replacement identity. |
| Immutable source and launch | Concurrent Change mutation yields one canonical snapshot or fails before compilation. Replacing or tampering with prepared test output cannot change what launches. |
| Cache reuse and bounds | Two revisions reuse one project/toolchain cache while producing distinct source-bound manifests. Cache count and measured-byte pressure refuse or reclaim only regenerable entries. Separately, exceeding the hard factory-wide retained-Change count cap refuses new admission without deleting retained work. |
| Orchestrator policy only | Orchestrators remain subject to hierarchy scope and cannot direct-launch, publish repositories, submit another attempt's outcome, mutate capacity or budgets, or invoke operator control. |
| Dispatch and execution mode | Disabled dispatch admits nothing. An admitted run retains its frozen typed mode across profile and dispatch changes, and every provider maps each supported mode to exact non-interactive native flags. |

Process tests use a temporary `DARK_FACTORY_HOME`, explicit private socket,
unique disposable paths and labels, deterministic providers, and an
independent post-test reaper. They never address the operator home, socket,
job, credentials, or paid provider subscription.

## Exact-main boot gate

The independent reviewer must inspect one exact `main` commit and confirm:

- the run phase/outcome contract is the only work and terminalization
  authority;
- no provider exists or mutates without one exact running attempt;
- queue selection and hierarchy authorization occur transactionally;
- typed execution mode is separate from dispatch and frozen at admission;
- crashes, identity reuse, and verifier leader loss cannot cause replay,
  unsafe cleanup, or premature terminalization;
- factoryd alone creates and removes Changes, and provider views expose no Git
  administrative or publication surface;
- completion verification launches only exact staged artifacts from a stable
  source snapshot and storage convergence touches only regenerable data;
- external executable intake remains absent;
- the causal matrix and authoritative local and hosted gates pass on that same
  commit; and
- surviving compatibility and complexity have been challenged for deletion.

The review records its exact base/head, commands, results, unverified lanes,
and explicit allow or block decision. Passing it does not install, release,
start, enable dispatch, or modify the live operator system; each remains a
separate operator action.
