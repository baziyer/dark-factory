# Architecture

Dark Factory separates model policy from durable work authority. This file
describes the attempt kernel, daemon-owned Change model, and fail-closed
completion-verification boundary implemented across the safe-kernel refactor.
It is a contract, not a component catalogue.

## Current status

Stages 1 and 2 are merged. Stage 3 is implemented on an isolated branch but has
not passed its exact-head gates, independent review, or merge; the separate
boot review also remains. Do not start the daemon against the operator
installation or submit real provider work.

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
4. Request authorization is exhaustive and fail-closed. Workers can act only
   on their admitted work. Orchestrators can propose bounded scheduling policy
   within their project. Operator authority cannot be used as an attempt
   identity for completion, blocking, or hooks.
5. Admission is the only transition from queued work to an attempt. One Store
   transaction checks dispatch and capacity, selects the current canonical
   queue head, derives its agent role and provider, and binds the immutable
   task incarnation and work revision before external effects.
6. No admitted attempt means no provider process, tool hook, outcome request,
   or writable source lease.
7. A retry creates a new run and new bearer. It never revives an old process
   or credential.

## Process and resource ownership

One admitted run launches one fresh non-interactive provider process.
Providers receive one `startup_input` on stdin; stdin then closes. There is no
resident process, PTY attach surface, terminal input, delivery replay, or
provider-process resume.

Launch is one nested register-before-exec handshake:

1. `factoryd` records the admitted run with a random runtime claim. The
   claim-derived path is durable before `mkdir`; its inode replaces the claim
   before any credential, configuration, or process is created inside it.
2. `factoryd` spawns an inert runner exec gate tied to the exact daemon parent,
   persists its stable PID, then activates the same PID into `factory-runner`.
3. The runner creates a second parent-bound child gate before provider `exec`
   and reports the stable provider PID and process group.
4. `factoryd` persists those identities and moves the run to `running`.
5. The runner releases the child to provider `exec`.

If preparation or activation fails, the run enters `finalizing`; a provider
must never execute first and become durable later. The runner is a
provider-blind effect host, not a second lifecycle owner.

The resource ledger records process, process group, runner, runtime root, and
other external effects before use. Each record contains enough identity to
refuse PID, path, or job-label reuse. Rust `Drop` and shell traps may accelerate
cleanup, but correctness comes from the durable daemon finalizer, including
after restart. On platforms without a stable process birth identity, a live or
reused PID remains unresolved: weak presence can never authorize signalling or
terminalization.

## Provider boundary

The `Provider` trait answers only:

- which executable, arguments, environment additions, and generated private
  configuration launch this run; and
- which model, reasoning, and permission values the provider supports.

It receives a daemon-derived `SpawnContext` with an exact `RunId`, source path,
single `startup_input`, hook-token path, trusted `factoryctl` path, and resolved
profile. It cannot choose a source path, keep a process alive for later work,
or extend authority. See [the provider guide](docs/providers.md).

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

Stages 1 and 2 did not solve build storage. The Stage 3 branch gives each project an
operator-selected verification policy: `None` or one fixed
`RustWorkspaceTest`. There is no provider-visible generic build operation or
Cargo shim. For a Rust-policy worker, `factoryctl task done` is the single
completion boundary: the daemon moves the run to `finalizing`, revokes its
authority, reaps every provider process group, and only then snapshots source
and starts verification. Orchestrator runs are not verified this way.

The source snapshot is a canonical scan/copy/scan of the plain Stage 2 Change;
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

Regenerable cache storage has a hard entry count and a measured byte policy.
Starting a writer makes byte status incomplete; after its exact process group
is reaped, factoryd remeasures allocated bytes and reclaims unprotected caches
until the policy converges. A measured over-limit cache cannot be claimed for
another verification. Status reports aggregate measured bytes, protected entry
count, and recoverable failure count, not an invented protected-byte subtotal.
An ordinary directory
cannot promise a portable instantaneous byte ceiling while Cargo is writing,
so the architecture does not claim one.

Until that stage and its storage proofs land, Dark Factory remains frozen.

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

There is no HTTP webhook or generic connector intake. GitHub and other external
intake remain outside the product; work enters only through the authenticated
private local API and cannot bypass admission.

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
- No live installation, release, or GitHub intake until all three stages and
  the independent boot review pass.
