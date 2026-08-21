# Architecture

Dark Factory separates model policy from durable work authority. This file
describes the attempt kernel, daemon-owned Change model, and fail-closed
completion-verification boundary implemented by the current kernel.
It is a contract, not a component catalogue.

## Current status

Live use remains frozen until an independent exact-main boot review passes.

The complete design and causal proof matrix live in
[`docs/development/SAFE_KERNEL_REFACTOR.md`](docs/development/SAFE_KERNEL_REFACTOR.md).

## Durable model

`RunId` is the attempt identity. A task can be queued without a run; a run
exists only after admission.

```text
Task: queued --------> running ----------------------> terminal result
                         |
                         v
Run:  admitted -> running -> finalizing -> terminal
          |          |           |
          |          |           +-- immutable outcome request, no authority
          |          +-- exact attempt bearer authorizes bounded effects
          +-- child may be prepared but cannot exec

Resource: declared -> active -> releasing -> released
                                      \----> unresolved
```

The run outcome is distinct from its phase: succeeded, blocked with a reason,
failed with a typed reason, or cancelled with a reason. The first durable move
to `finalizing` freezes its requested outcome. Later completion, block, cancel,
or exit observations are idempotent and cannot replace that request. The
`finalizing` projection exposes that proposal; the terminal projection exposes
the actual result, while append-only events preserve both. For a configured
Rust check, failed verification is the one documented refinement: it converts
proposed success into `failed(unverifiable)` rather than claiming success.

Only the finalizer writes `terminal`. It may do so only when every ephemeral
resource is released and every retained artifact is durably transferred to
its next owner. Cleanup failure leaves the run visibly `finalizing`; it never
pretends the resource disappeared or rewrites the outcome.

## Authority invariants

1. SQLite is the sole durable authority. State mutations and their bounded
   events commit together. Process-local locks serialize work but never prove
   ownership.
2. Every provider-mediated mutation requires one bearer credential for one
   exact `running` run. Authentication derives project, agent, task, run, role,
   provider, and any Change scope from the store.
3. Anonymous local requests may ask only for health. Operator requests require
   the private operator credential. Attempt credentials are valid only while
   the exact run is `running`; admission, `finalizing`, and `terminal` do not
   grant effect authority.
4. Request authorization is exhaustive and fail-closed. A worker may message
   itself, its immediate parent, or its nearest orchestrator ancestor. An
   orchestrator may message itself or
   a strict descendant, create a child of its current task only when assigning
   it to a strict descendant, and assign queued work only to a strict
   descendant; attempt authority cannot edit or unassign tasks. These
   relationship checks rederive the exact running run and `agents.parent_agent_id`
   inside the same immediate Store transaction as the mutation. Task and run
   ancestry never grant agent authority. Operator authority cannot be used as
   an attempt identity for completion, blocking, or hooks.
5. Admission is the only transition from queued work to an attempt. One Store
   transaction checks dispatch and capacity, selects the current canonical
   queue head, derives its agent role, provider, and typed execution mode, and
   binds the immutable task incarnation and work revision before external
   effects. The factory-wide dispatch switch controls only whether this
   transaction may admit new work; changing an agent profile or disabling
   dispatch cannot rewrite an admitted run's launch authority.
6. No admitted attempt means no provider process, tool hook, outcome request,
   or writable source lease.
7. A retry creates a new run and new bearer. It never revives an old process
   or credential.
8. External-input receipt is separate from work authority. An operator-only
   transaction may create an immutable `InputEnvelope`, one quarantined
   `WorkCandidate`, and bounded events. It cannot create a Task, Message, Run,
   Change, provider prompt, ProposedAction, or scheduling event. A changed
   source revision advances an exact expected-current pointer and stales an old
   quarantined candidate atomically. A rejected predecessor keeps its durable
   decision while losing current-source authority. Exact observation or
   rejection replay has no second effect, even after that pointer advances.

## Process and resource ownership

One admitted run launches one fresh non-interactive provider process.
Providers receive one `startup_input` on stdin; stdin then closes. There is no
resident process, PTY attach surface, terminal input, delivery replay, or
provider-process resume.

Launch is one nested register-before-exec handshake:

1. `factoryd` records the admitted run with a random runtime claim. The
   claim-derived path is durable before `mkdir`; its inode replaces the claim
   before any credential, configuration, or process is created inside it.
2. `factoryd` creates and locks a private startup file, persists its exact
   filesystem identity, then maps that lock to the inert runner gate's stdin.
   A restart can prove that a gate spawned before PID registration is gone only
   by acquiring the same lock; missing or replaced identities stay unresolved.
3. `factoryd` persists the inert gate's stable PID, then activates that same
   PID into `factory-runner`. The runner creates a second parent-bound child
   gate before provider `exec`
   and reports the stable provider PID and process group.
4. `factoryd` persists those identities and moves the run to `running`.
5. The runner releases the child to provider `exec`.

If preparation or activation fails, the run enters `finalizing`; a provider
must never execute first and become durable later. The runner is a
provider-blind effect host, not a second lifecycle owner.

The resource ledger records process, process group, runner, runtime root, and
other external effects before use. Each record contains enough identity to
refuse PID, path, or job-label reuse, but stored numeric identities never grant
signal authority. The daemon requests shutdown through the authenticated live
runner; the runner may signal its provider group only while it still owns the
unreaped leader child. A live-child guard preserves that authority across
runner cancellation or unwind, then disarms immediately after a successful
wait; it never authorizes terminalization. After leader or runner loss, the
durable finalizer only observes exact absence. A live, reused, or weak identity
remains unresolved and cannot authorize signalling, runtime removal, or
terminalization.

## Provider boundary

The `Provider` trait answers only:

- which executable, arguments, environment additions, and generated private
  configuration launch this run.

It receives a daemon-derived `SpawnContext` with an exact `RunId`, source path,
single `startup_input`, hook-token path, trusted `factoryctl` path, and resolved
profile including one typed `PlanOnly`, `WorkspaceWrite`, or `Unrestricted`
mode frozen by admission. Provider adapters exhaustively translate that value
to non-interactive native flags; free-form permission strings are not part of
the domain. An adapter cannot choose a source path, keep a process alive for
later work, or extend authority. See [the provider guide](docs/providers.md).

`WorkspaceWrite` means durable provider writes belong to the admitted Change.
Codex denies its inherited system-temp write roots; Claude's native sandbox
retains only its per-launch ephemeral temp scratch in addition to the Change.
That scratch is provider-owned runtime state, not product source or publication
authority.

Factoryd validates one exact Claude executable and the finite generated
settings shapes before Store admission begins. Claude `WorkspaceWrite` is
macOS-only because its exact AF_UNIX sandbox policy is ignored elsewhere.
Claude `PlanOnly` has no sandbox stanza, but is conservatively restricted to
the supported macOS product runtime rather than asserting a second platform
claim. `Unrestricted` remains available elsewhere. A missing or rejected
install disables only that provider, while a provider version or executable
identity change fails its launch closed. Codex parses every actual launch
under `--strict-config` and inherits no ambient provider configuration.

The generic runner exports `DARK_FACTORY_ATTEMPT_TOKEN_FILE` as the path to the
private bearer file. It does not export the bearer value. When that variable is
present, `factoryctl` authenticates every request with the ambient attempt
credential, including commands whose shape is normally operator-only. The
daemon then rejects commands outside the attempt allowlist; the client never
falls back to the operator token.

Provider output is opaque. Hooks are authenticated observations and bounded
requests, not lifecycle authority. The daemon never infers success from text.

## Source ownership

For a worker, admission atomically reserves one Change ID and one daemon-derived
path for the exact task incarnation. A registered, parent-bound wrapper then
selects one full local Git commit, records the repository and staging inode,
reads its bounded manifest and exact blob OIDs through `git cat-file`, and
atomically publishes the resulting safe tree. It does not use `git archive`,
so repository-local export attributes cannot transform committed bytes.
Partial clones are refused before object reads, and lazy promisor fetch is
disabled: the selected commit must already be wholly local. The real provider
replaces that same registered process only after SQLite records the Change as
`available`.

The provider sees a plain writable source tree with no `.git` locator. Git
repository discovery and linked-worktree creation are refused by construction
and by the sanitized environment. Retries reuse the same retained Change;
deletion is an explicit identity- and revision-checked transition that is
refused while an attempt leases it. Factoryd supplies no status, commit, push,
pull-request, or publication operation.

Pre-kernel source paths live only in `legacy_sources`. They are quarantine
metadata, not Changes: factoryd never touches the recorded filesystem path and
can only forget the metadata row by typed ID.

## Build and storage boundary

Each project has one operator-selected verification policy: `None` or one fixed
`RustWorkspaceTest`. There is no provider-visible generic build operation or
Cargo shim. For a Rust-policy worker, `factoryctl task done` is the single
completion boundary: the daemon moves the run to `finalizing`, revokes its
authority, asks the live runner to reap the provider process group, and only
after exact resource absence snapshots source and starts verification. A lost
runner leaves finalization pending; stored PIDs are not a fallback. Orchestrator
runs are not verified this way.

The source snapshot is a canonical scan/copy/scan of the plain Change;
it is published only when the manifests agree. This deliberately replaces the
earlier private-Git-index and in-flight-writer design: Changes contain no Git
administration, and a hook has no trustworthy `PostToolUse` writer ledger.

Rust verification uses one mutable cache per random project incarnation and
fixed Cargo/rustc identity and configuration, not per Change or source
revision. It compiles only the private snapshot, copies the top-level Cargo
test executables into a content-addressed directory under the run's registered
temporary root, verifies its manifest/identity/digest, and launches those
copies. The stable snapshot is the test working directory and is rechecked
before and after every top-level test; a mutation fails verification before a
later test can launch. Fixtures are not copied into the executable directory,
doctests are not run, and test code may still launch other same-UID processes.
Mutable `target/debug` or
`target/release` top-level launch is forbidden. These checks prevent confused
or cooperative replacement; they are not a sandbox against hostile same-UID
code.

Verifier recovery retains the same ownership rule as provider recovery. If a
process-group leader disappears while descendants remain, the run stays
`finalizing`; a numeric process-group ID without the exact live leader identity
is neither signal authority nor proof that the effect is gone.

Regenerable cache storage has a hard entry count and a measured byte policy.
Starting a writer makes byte status incomplete; after its exact process group
is absent, factoryd remeasures allocated bytes and reclaims unprotected caches
until the policy converges. Healthy verifier shutdown is cooperative: factoryd
publishes a private finish marker and the live leader terminates its own group.
A measured over-limit cache cannot be claimed for
another verification. Status reports aggregate measured bytes, protected entry
count, and recoverable failure count, not an invented protected-byte subtotal.
An ordinary directory
cannot promise a portable instantaneous byte ceiling while Cargo is writing,
so the architecture does not claim one.

## Policy versus correctness

God/orchestrators schedule and prioritize through ordinary authenticated
requests. Factoryd independently checks project scope, task state, capacity,
budget, source availability, and admission. An orchestrator cannot create
Changes, launch processes directly, mutate capacity or agents, choose an
outcome for another attempt, or finalize a run. Its death cannot prevent the
daemon finalizer from converging.

## Clients and integrations

`factoryctl` and `factory-tui` are disposable clients of one local API. They do
not own runtime state. Both use the operator credential for operator requests.
Generated provider hooks and attempt commands read the private credential file
for their exact run through `DARK_FACTORY_ATTEMPT_TOKEN_FILE` (or an explicit
hook `--token-file`). A provider-invoked `factoryctl` process cannot cross into
operator authority by choosing an operator command.

There is no HTTP webhook, GitHub adapter, or generic connector intake. The
operator may place bounded provider-neutral observations into inert quarantine
through the authenticated private local API, then list, inspect, or reject
them. Raw content is private local detail and public events carry only project,
envelope, candidate, and status identities. There is no accept/materialize
operation, so receipt cannot become executable work or bypass admission.

## State outside SQLite

Bounded project guidance, rules, and memory remain files under the factory
home. SQLite owns their identities; their prose is not authority. Cross-file
and database operations must state their ordering and failure semantics rather
than pretending to be one transaction.

The local socket, credential files, runtime roots, and generated provider
configuration are private daemon-owned files. The live operator home and
launchd job are never test fixtures.

## Deliberate non-goals

- No new crate, ORM, actor framework, repository/service trait with one
  implementation, generic saga, or event-sourcing framework.
- No protection from a hostile process running as the operator. Bearer scoping
  prevents confused/cooperative cross-attempt behavior; real isolation needs a
  separate OS user, container, or sandbox.
- No session compatibility layer beyond decoding historical events needed for
  migration and replay.
- No live installation, release, or external intake before the independent
  exact-main boot review and a separate operator decision.
