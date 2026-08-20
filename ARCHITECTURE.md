# Architecture

Dark Factory separates model policy from durable work authority. This file
describes the Stage 1 kernel now implemented on the refactor branch and the
fail-closed boundaries that remain until Stages 2 and 3. It is a contract, not
a component catalogue.

## Current status

Stage 1 is an intermediate implementation pending pull-request review. It is
not a boot candidate. Worker admission deliberately fails because no
production Change allocator exists yet. Do not start the daemon against the
operator installation or submit provider work.

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
          |          |           +-- immutable outcome, no effect authority
          |          +-- exact attempt bearer authorizes bounded effects
          +-- child may be prepared but cannot exec

Resource: declared -> active -> releasing -> released
                                      \----> unresolved
```

The run outcome is distinct from its phase: succeeded, blocked with a reason,
failed with a typed reason, or cancelled with a reason. The first durable move
to `finalizing` wins. Later completion, block, cancel, or exit observations are
idempotent and cannot replace that outcome.

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
   identity for completion, blocking, hooks, or repository mutation.
5. Admission is the only transition from queued work to an attempt. It binds
   the immutable task incarnation and work revision before external effects.
6. No admitted attempt means no provider process, tool hook, outcome request,
   repository mutation, or writable source lease.
7. A retry creates a new run and new bearer. It never revives an old process
   or credential.

## Process and resource ownership

One admitted run launches one fresh non-interactive provider process.
Providers receive one `startup_input` on stdin; stdin then closes. There is no
resident process, PTY attach surface, terminal input, delivery replay, or
provider-process resume.

Launch is a two-step handshake:

1. `factoryd` records the admitted run and declares its runtime and process
   resources.
2. `factory-runner` creates a child blocked before provider `exec` and reports
   the stable PID and process group.
3. `factoryd` persists those exact identities and moves the run to `running`.
4. The runner releases the child to `exec`.

If preparation or activation fails, the run enters `finalizing`; a provider
must never execute first and become durable later. The runner is a
provider-blind effect host, not a second lifecycle owner.

The resource ledger records process, process group, runner, runtime root, and
other external effects before use. Each record contains enough identity to
refuse PID, path, or job-label reuse. Rust `Drop` and shell traps may accelerate
cleanup, but correctness comes from the durable daemon finalizer, including
after restart.

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

## Source ownership: Stage 1 refusal

Stage 1 has no production source allocator. Worker admission therefore returns
`SourceProvisioningUnavailable`. Module-private tests may insert an exact
Change backed by a temporary directory solely to prove the authority model.

Migration preserves each legacy agent worktree as an unlinked retained Change.
It does not inspect, delete, relocate, or assign that path. Stage 2 will make
`factoryd` the only product path that creates and leases Change worktrees,
remove Git administrative locators from provider source views, and refuse
non-Git source mutation.

## Build and storage boundary

Stage 1 does not solve build storage. Stage 3 must replace per-worktree Cargo
targets with a bounded project cache keyed by toolchain/profile configuration,
then prepare content-addressed immutable executable bundles with verified
digests. Mutable `target/debug` sibling launch is not an acceptable boot path.

Until that stage and its storage proofs land, Dark Factory remains frozen.

## Policy versus correctness

God/orchestrators schedule and prioritize through ordinary authenticated
requests. Factoryd independently checks project scope, task state, capacity,
budget, source availability, and admission. An orchestrator cannot create
worktrees, launch processes directly, mutate capacity or agents, perform
repository publication, choose an outcome for another attempt, or finalize a
run. Its death cannot prevent the daemon finalizer from converging.

## Clients and integrations

`factoryctl` and `factory-tui` are disposable clients of one local API. They do
not own runtime state. Both use the operator credential for operator requests.
Generated provider hooks and attempt commands read the private credential file
for their exact run through `DARK_FACTORY_ATTEMPT_TOKEN_FILE` (or an explicit
hook `--token-file`). A provider-invoked `factoryctl` process cannot cross into
operator authority by choosing an operator command.

The optional loopback webhook remains an input transport, not trusted work.
GitHub and external intake stay quarantined until the complete kernel boot
gate. No input source can bypass admission.

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
