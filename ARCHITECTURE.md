# Architecture

Dark Factory separates model policy from durable work authority. This file
describes the current Go runtime's attempt kernel, daemon-owned Change model,
and fail-closed process boundary. It is a contract, not a component catalogue.

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
or exit observations are idempotent and cannot replace that request.

Only the finalizer writes `terminal`. It may do so only when every ephemeral
resource is released and every retained artifact is durably transferred to its
next owner. Cleanup failure leaves the run visibly `finalizing`; it never
pretends that a resource disappeared or rewrites the outcome. The current
daemon accepts only `VerificationNone`; non-None verification policies are
rejected as unsupported before a provider runs.

## Authority invariants

1. SQLite is the sole durable authority. State mutations and their bounded
   events commit together. Process-local locks serialize work but never prove
   ownership.
2. Every attempt mutation requires one bearer credential for one
   exact `running` run. Authentication derives project, agent, task, run, role,
   provider, and Change scope from the store.
3. Anonymous local requests may ask only for health. Operator requests require
   the private operator credential. Attempt credentials are valid only while
   the exact run is `running`; admission, `finalizing`, and `terminal` grant no
   effect authority. Operator authentication cannot impersonate an attempt.
4. Admission is one global `BEGIN IMMEDIATE` Store transaction. It validates
   the fresh schema image and one SQL integrity predicate before reconciliation,
   capacity, or selection. No caller supplies an agent, task, observation, or
   cursor. Corrupt durable control is corruption, never queue ineligibility.
   Capacity counts admitted, running, and finalizing runs together; fresh
   no-admission precedence is `dispatch_disabled`, `at_capacity`,
   `queue_empty`, then `no_eligible_work`.
5. The Store selects the canonical eligible task and agent globally by priority
   descending, creation time ascending, and exact 16-byte task-ID BLOB bytes
   ascending. It validates the selected Change and binds the task incarnation,
   revision, provider, and launch facts before external effects. Repository or
   provider availability becomes typed post-admission failure, never a stale
   scheduler filter.
6. `dispatch_enabled` controls only new admission. An admitted run retains its
   provider, optional model, and optional reasoning effort. V1 provider choice
   is unrestricted interactive authority; no permission-profile field is
   persisted or interpreted.
7. No admitted attempt means no provider process or outcome request, and no
   writable source lease. A retry creates a new run and bearer; it never revives
   an old process or credential.

## Browser authority

The paired browser is a client of the local API, not a second scheduler. It
may submit a bounded instruction to a configured idle agent. The daemon creates
the same durable task used by `factoryctl`, and the browser renders its
canonical queued, running, or terminal state. An idle configured agent remains
selectable so the operator can add work; no provider process or terminal is
created until admission starts a run.

## Browser state

The browser reads one bounded, transactionally pinned public snapshot and is
told only that the durable head moved. There is no cursor, page, per-entity
refresh, tombstone or restart taxonomy: a client either holds one whole
coherent snapshot or none, and a change notification carries nothing but a
head.

The snapshot read runs in a single pinned SQLite transaction and selects only
public columns, so a concurrent writer cannot produce a mixed head and private
durable data is never loaded. A watcher registers before rereading the durable
head, so a commit landing between a client's snapshot and its registration is
announced immediately rather than waiting for the next poll.

Bounds are exact and fail closed. Client-to-server control stays at 64 KiB;
only a server snapshot may reach 1 MiB, and the kernel refuses to project more
than 4,096 entities. Neither bound truncates: a Factory that does not fit is
one finite `too_large` answer, never a partial view.

The wire contract is unversioned and tolerates additive change. There is no
envelope generation, no versioned loopback path, no version in the pairing and
auth transcript domains, and no protocol identity in the published artifacts;
the loopback path is `/browser`, and `/pair` beside it is the one HTML page
the daemon serves: a script-free confirm page whose form mints a pairing
challenge and redirects to the hosted console. A control frame carrying a
member this build does not know is served, so the hosted console and the
daemon can be installed in either order without a mismatch window.

An ignored member is an ASCII name that is not a known name under any case. A
non-ASCII name is refused outright, and a member differing from a known one
only in case is refused, not ignored: Go matches struct fields with Unicode
case folding, so tolerating either would let it overwrite the exact member the
shape check validated while a TypeScript peer,
whose keys are exact, kept reading the exact one. Both decoders refuse it, so
both read the same frame the same way.

Tolerance is additive only. Every size, count, depth and member bound still
binds, and an unknown member counts toward them: an array or a nesting depth
inside a tolerated member is measured exactly as one inside a known member. A
missing required member, a member of the wrong type and a frame arriving in the
wrong direction remain finite refusals that end the connection. A client
control type the daemon does not know is answered by its request id with
`ERROR` code `unsupported` and the daemon keeps its side open, so a console
that knows that code degrades one feature instead of losing its session. The
other direction is still closed: the console decoder ends the session on a
server frame type or an `ERROR` code it does not know (#556).

## Browser HumanRequest authority

The Go runtime keeps HumanRequest authority in SQLite and exposes only bounded,
correlated browser operations. Public HumanRequest state contains request,
project, agent and task relationships, chronology, revision, kind, status, the
fixed reply bound, and display-only `can_reply`. It contains no run locator,
terminal locator, question, reply, cancel descriptor, process identity, or
other private source data.

Closing the selected agent's current-run terminal detaches that observer while
leaving the authenticated BrowserSession connected; only an ambiguous detach
fails closed by stopping the session.

Private detail is one pinned SQLite read. A client with
`private_human_request_detail` may receive the exact hostile question and, only
for an open request whose originating run is running with its exact active
terminal session, an observation-only terminal target. Before minting that
target, the same snapshot validates the canonical task assignment, resource
topology and identities, run/resource/session chronology, and requires the
validated terminal session to equal the selected active session.
`human_actions` is checked independently: without it, detail may contain that
target but cannot advertise reply or cancellation. With it, the same snapshot
may mint the one concrete cancellation descriptor containing exact request and
run revisions. Delivering, delivery-unknown, finalizing, terminal, missing, and
non-active origins expose no reply or cancellation authority; corrupt active
relationships fail closed rather than resembling unavailability.
`administration` is the operator's own bit: discovering and linking this
machine's provider logins and choosing which one an agent runs as. A pairing
minted on loopback carries it; a relay pairing does not.

A reply contains only request ID, expected request revision, and bounded text.
The Store derives the originating run and commits a unique delivery receipt
before the daemon looks up that exact live owner or writes once to its PTY.
Failure or uncertainty after reservation becomes `delivery_unknown` and is
never replayed. Cancellation likewise derives the origin, checks exact
revisions and capability, enters finalizing, revokes authority, resolves the
request, and appends invalidations in one Store transaction. The exact live
attempt then fences its current terminal binding; rejected, partial, uncertain,
or controller-failed fencing is visible after commit and is never retried
against another run.

## Process and resource ownership

The Go runtime launches one fresh runner-owned interactive PTY per run with
explicit authenticated attach/input authority. No provider process is reused
across runs.

Launch is one nested register-before-exec handshake:

1. `factoryd` records the admitted run and a random runtime claim. The
   claim-derived path is durable before `mkdir`; its inode replaces the claim
   before a process is created inside it.
2. `factoryd` creates and locks a private startup file, persists its exact
   filesystem identity, then maps that lock to the inert runner gate's stdin.
3. `factoryd` persists the inert gate's stable PID before activating it into
   `factory-runner`.
4. The runner prepares a second child blocked before provider `exec` and reports
   the stable provider PID and process group.
5. `factoryd` persists those identities, moves the run to `running`, and only
   then releases the child to provider `exec`.

If preparation or activation fails, the run enters `finalizing`; a provider
must never execute first and become durable later. The runner is a
provider-blind effect host, not a second lifecycle owner.

A successful attempt outcome commits `finalizing` together with one in-memory
response fence. The daemon sends a fresh random receipt after the outcome
response; the attempt client validates the reply, echoes that receipt on the
same connection, and then half-closes. Only that acknowledgement (or a visible
transport failure) clears the fence and lets the live owner terminate the
provider group. Socket I/O never holds the global operation gate, and explicit
daemon cancellation preempts a missing receipt.

Owned failed attempts apply exact recovery before `RunNext` returns. The
scheduler reports any remaining `finalizing` residue as unsettled and leaves it
for the startup recovery sweep.

The resource ledger records process, process group, runner, runtime root, and
other external effects before use. Stored numeric identities never grant
signal authority. The daemon requests shutdown through the authenticated live
runner; the runner may signal its provider group only while it owns the
unreaped leader child. After leader or runner loss, the finalizer only observes
exact absence. Reused or weak identities remain unresolved and cannot authorize
signalling, removal, or terminalization.

## Provider boundary

`internal/provider.Build(Request) (Launch, error)` is the one closed provider
selection boundary. It returns only one exact absolute executable, ordered argv,
and complete ordered environment. The runner owns the descriptor-bound Change
cwd, task delivery, PTY, process group, wait/reap, output, and cleanup. A
provider cannot select a source path or lifecycle result.

Shell receives bounded task bytes through a sealed descriptor. Claude Code and
Codex resolve their named CLI through the daemon's fixed tool path to one exact
direct executable commitment. Claude receives its task once through the PTY
before the terminal is exposed. Codex receives only a fixed non-secret startup
instruction in argv, then reads its exact task through the running attempt's
authenticated local API; task text never enters its argv, environment, or
Change-worker configuration. Native tools use the operator's existing account:
Claude uses the account `HOME`, while Codex uses its explicit configuration root
with a private runtime `HOME`. Both keep a private `TMPDIR`. [The provider
contract](docs/providers.md) owns the exact argv, environment, and task-delivery
details. The schema and wire contract contain no permission-profile field.

Provider output is opaque and never lifecycle authority.

## Change and repository ownership

`factoryd` is the only product creator and administrator of Changes. Admission
reserves one daemon-derived path for one task incarnation. A registered wrapper
materializes one exact committed tree before the provider can execute. The
provider sees a plain writable directory with no Git administrative locator.
An orchestrator run binds no Change: it works in its private runtime home, and
publication of a worker's retained Change is the Maintainer App's, reached
through the one MCP server an orchestrator's Claude session is given.
Factoryd exposes no repository status, commit, push, pull-request, or
publication operation.

Managed Change removal requires the exact typed ID, current revision, durable
inode identity, and no live lease; replacement or ambiguity remains visibly
pending and is never touched. Retries reuse a retained Change only after the
preceding run is terminal.

Fresh selection pins the exact repository root, Git administration directory,
bounded local config, object-directory root, and trusted Git executable around
each metadata process. It does not enumerate unrelated historical objects.
Trusted Git resolves the revision once, the tree query names that exact commit,
and the manifest binds every path, mode, size, and blob object ID. Materializing
each selected blob requires the expected object ID, type, and size and
independently hashes its bytes before the `.git`-free tree is published.
Concurrent garbage collection, repacking, or unrelated object creation may
therefore preserve the exact selection or make its read fail; it cannot select
a different moving revision.

## Verification and storage

The current daemon has no generic build API or completion verifier. Project
creation uses `VerificationNone`; non-None verification values are
schema-recognized but rejected by the supervisor. No provider or client may
treat an unimplemented verifier as proof of success.

Regenerable runtime data may be reclaimed only through exact registered,
unleased identity. A writer makes status incomplete; after exact effect absence
the daemon remeasures before cleanup. A live or reused process/group identity
keeps finalization pending. Unique retained Changes are never automatic cleanup
targets, and the daemon does not claim an instantaneous filesystem byte ceiling.

## Clients and integrations

`factoryctl` and the hosted browser are disposable clients of one local API;
neither owns runtime state. `factoryctl` uses the operator credential, while a
paired browser uses its scoped durable browser authority. Browser task
submission, like CLI task submission, is applied by the daemon's durable queue
and does not bypass dispatch or provider admission.
Browser selection is pinned to canonical state heads and never polls
or sleeps across lifecycle boundaries. Stale discovery is treated as stale, not
as authority for a retry or an old absence. Attempt commands read the private
credential file for their exact run; an attempt cannot cross into operator
authority.

## State outside SQLite

The local socket and runtime roots are private daemon-owned files. The live
operator home and launchd job are never test fixtures.

## Deliberate non-goals

- No new package, ORM, actor framework, repository/service interface with one
  implementation, generic saga, or event-sourcing framework.
- No protection from a hostile process running as the operator. Bearer scoping
  prevents confused/cooperative cross-attempt behavior; real isolation needs a
  separate OS user, container, or sandbox.
