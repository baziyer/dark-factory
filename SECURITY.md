# Security

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/dark-factory-build/dark-factory/security/advisories/new).
Do not open a public issue for a capability, credential, process-ownership, or
network-boundary failure.

Dark Factory is pre-1.0. Only current `main` and the latest release receive
security fixes.

## Threat model

Dark Factory is a local, single-operator application. Every provider runs as
the operator. The kernel prevents confused or cooperative providers from
acting outside an exact attempt; it does not isolate a hostile same-user process from readable files,
credentials, other processes, or the local socket. That claim requires a
separate OS user, container, or sandbox.

The operator API boundary is a private Unix socket with owner-only
directory/socket modes. The browser boundary is a loopback-only
WebSocket with exact Host/Origin checks and proof-of-possession client keys; it
is not a webhook or generic connector listener. Exposing either local surface
beyond the machine is unsupported.

The same loopback listener serves one first-party page, `/pair`, whose form
post mints a browser pairing challenge on the strength of the request's Fetch
Metadata alone (a top-level document navigation, and a same-origin post). Those
headers say which browser context sent a request; they do not authenticate the
sender as a browser. Any local process that can connect to 127.0.0.1:43123 can
send them, obtain a challenge, and, by presenting the console origin, pair a
client with the full web grant. The loopback listener is therefore a
same-machine trust boundary, not a per-user one: a separate OS user isolates
files, credentials and the operator socket, but not pairing. This was accepted
on 5 September 2026 in exchange for pairing without a terminal. The control is
revocation, through `factoryctl web list-clients` and `web revoke` or the
console; a boot holds at most 32 live challenges, each expiring after five
minutes, so a hostile local process can delay pairing but cannot hold a grant
it did not pair. A bare 503 from the page means the mint failed — the
live-challenge cap, the transport, or the store — and `factoryctl web status`
reports the transport state and the live challenge count. A remote
invitation is minted only by a client holding the full loopback grant, whose
terminal_input bit a remote grant never carries, so a paired phone can never
invite another phone. Such a console does consume the same cap, so the daemon
bounds it to four invitations per five minutes: even a console looping the
request holds at most four of the 32 slots, and the local `/pair` page can
always still mint one. Revoking any client invalidates every live pairing
challenge, so a link minted before the revocation cannot pair after it.

### Hosted-origin compromise response

A compromised hosted origin or same-origin dependency can exercise every
capability granted to a paired browser client. Stop trusting the hosted page,
revoke every paired browser client through the owner-authenticated local API,
and prove that no browser connection remains. A compromised console holding the
full loopback grant can mint remote invitations, but no more than four per five
minutes, so it can neither exhaust the challenge cap nor deny the local `/pair`
page; a compromised remote client could not have minted one at all. Revocation
also invalidates every live pairing challenge, so an invitation minted before it
cannot pair afterwards. If revocation
cannot prove connection cleanup, quiesce the browser runtime or stop the
service and keep it stopped until cleanup is resolved. Only then remediate and
republish the exact reviewed hosted artifact. Re-pair clients only after that
artifact is live. Origin allowlisting, CSP, and non-exportable browser keys
reduce risk but do not replace this response.

## Principals and capabilities

Every request carries one auth-domain byte and is resolved once as one of:

- **Anonymous**: health only.
- **Operator**: authenticated by the private operator credential. Operator
  commands administer durable state but cannot impersonate an attempt for
  completion or blocking.
- **Attempt**: authenticated by a random bearer stored in a private per-run
  file. The store derives exact project, agent, task, run, role, provider, and
  Change scope. The bearer works only while that run is `running`.

Missing credentials never imply operator access. Bearers are redacted from
debug/display output and are not accepted in argv, environment variables,
events, logs, request payloads, or caller-selected identity fields. The first
transition to `finalizing` revokes attempt mutation authority. Old, forged,
cross-project, taskless, and terminal credentials fail closed.

The provider environment contains `DARK_FACTORY_ATTEMPT_TOKEN_FILE`, which is
only the path to the private bearer file, not the bearer itself. When present,
`factoryctl` uses that attempt credential for every local-API request. An
operator-shaped command invoked by a provider is therefore authorized as the
attempt and rejected if outside its allowlist; it never falls back to
`operator.token`.

The current daemon supports worker runs only. Operator and attempt roles confer
no cross-agent authority; task and run ancestry never grant agent authority.

Public browser state is one bounded snapshot built from a positive allowlist of
public columns. Project roots and verification policy, agent model, reasoning
effort and budgets, task bodies, results and blocked reasons, and HumanRequest
question, reply and run identity are not selected at all, so they cannot reach
a projection. The snapshot has an exact entity bound and an exact encoded-byte
bound, and exceeding either is a finite refusal rather than a trimmed answer.

Paired browser clients have one durable, revocable capability mask. Public
observation, private HumanRequest detail, HumanRequest effects, and terminal
input are separate bits and are reloaded for every operation while an exact
per-client gate orders them against revocation. A transport-minted connection
identity fences live effects but is never serialized or accepted from a
caller. Private detail does not imply reply/cancel authority, and a terminal
target returned by detail is only an observation coordinate: attach and input
independently revalidate the exact running run, active session, live owner, and
their own capabilities.

HumanRequest reply and cancellation never accept a run destination. Reply
reserves a unique delivery against the Store-derived origin before live-owner
lookup, writes at most once, and permanently marks ambiguity as
`delivery_unknown`. Cancellation derives the same origin in one SQLite
transaction, checks exact request and run revisions, and atomically finalizes,
revokes, resolves, and invalidates before synchronously fencing the exact live
attempt's current terminal binding. Rejected, partial, uncertain, or
controller-failed fencing is visible after the durable commit and is never
retried against another run. Browser results are correlated by envelope,
request, and exact post-transition revisions; malformed or forged results fail
closed.

## Process and cleanup safety

Before provider execution, `factoryd` durably records the admitted run and its
resources. Before the outer runner gate is spawned, the daemon records the
exact identity of a locked private startup file inherited as the gate's stdin.
After a crash, that lock proves whether a gate not yet bound to a PID can still
exist; a missing or replaced bound file is unresolved, never absent.
`factory-runner` prepares a child blocked before `exec`, reports its PID and
process group, and waits. Only after the daemon records those exact identities
and transitions the run to `running` may the child execute.

Success, block, failure, cancellation, and exit converge through `finalizing`.
A restartable daemon finalizer is the only writer of `terminal`. It uses the
authenticated runner while that runner is live; only the runner may signal a
provider group, and only before it reaps the leader child it directly owns. Its
live-child guard retains bounded cleanup authority across cancellation or
unwind and disarms immediately after a successful wait; it is not terminal
authority. Stored numeric identities are observation evidence, never signal
authority. Reused PIDs, paths, runner identities, and job labels are reported
as unresolved rather than touched. A run remains visibly `finalizing` while any
ephemeral resource is active or unresolved.

## Provider and tool boundary

Go uses one concrete `internal/provider.Build(Request) (Launch, error)` boundary.
Admission freezes provider, optional model, and optional reasoning effort only.
`Build` returns one exact absolute executable, ordered argv, and complete ordered
environment. The runner owns the descriptor-bound Change cwd, task delivery,
PTY, input, process group, wait/reap, output, and cleanup.

Provider choice is unrestricted interactive authority in V1. Shell receives
its task through a sealed descriptor. Claude Code and Codex resolve one direct
executable commitment from the daemon's fixed tool path. Claude receives its
task through the PTY before the terminal is exposed. Codex receives a fixed
non-secret startup instruction in argv and reads the exact task only through
its running attempt credential; the task is absent from argv, environment, and
Change-worker configuration. They reuse the operator's existing account:
Claude uses the account `HOME`, while Codex uses its explicit configuration
root with a private runtime `HOME`. Both keep a private `TMPDIR`; no provider
API key is copied into the environment. The [provider
contract](docs/providers.md) owns the exact launch details. Non-None verification
policies are unsupported by the current daemon and cannot be treated as
completion proof.

The runner's provider boundary does not grant source or lifecycle authority.

Factory dispatch and provider authority are separate durable controls.
`dispatch_enabled` decides only whether another attempt may be admitted; it
cannot weaken or rewrite an already-admitted attempt. Missing or invalid
external repository state becomes a typed post-admission failure, without
selecting lower work.

## Source and repository boundary

`factoryd` is the only product creator and administrator of Changes. Worker
admission reserves one daemon-derived path for one task incarnation, and a
registered wrapper materializes one exact committed tree before the provider
can execute. The leased provider view is a plain writable directory with no
Git administrative locator. Factoryd exposes no repository status, commit,
push, pull-request, or publication operation.

Managed Change removal requires the exact typed ID, current revision, durable
inode identity, and absence of a live lease; a replacement or ambiguous path
remains visibly pending and is never touched.

## Build and storage boundary

The current daemon has no generic build API or completion verifier. Project
creation uses `VerificationNone`; non-None verification values are
schema-recognized, but every non-None policy is rejected by the supervisor as
unsupported before provider execution. No client or provider may treat
an unimplemented verifier as proof of success.

Resource reclamation may remove only exact, registered, unleased regenerable
runtime data; unique retained Changes are never automatic cleanup targets. A
writer makes status incomplete. Only after exact process-group absence may
remeasurement and cleanup proceed. A reused numeric group ID is not kill
authority and cannot prove the effect absent. Stronger execution isolation
requires a separate user, sandbox, or container.

## Bounded inputs and durable data

Local frames, messages, events, and logs have hard size limits. SQLite uses
durable transactions for authority.

The browser control frames and the local API bodies are unversioned JSON that
ignores a member neither build knows, so a hosted console and a daemon can be
installed in either order. An ignored member is an ASCII name that is not a
known name under any case; a non-ASCII name, or a member differing from a known
one only in case, is refused, because Go matches struct fields with Unicode
case folding and would otherwise let it overwrite the exact member that was
validated. The local API
reaches the same result by requiring every member name to be canonical
lowercase and rejecting duplicates.

Tolerance is additive only and grants no authority: an ignored member reaches
no field, so a caller cannot supply an entity id, a run destination, a failure
code, or any other value the decoded shape has no place for. Size, count, depth
and member bounds bind unchanged and are measured inside a tolerated member
too, required members and value types are still validated, and an unknown frame
type or a frame in the wrong direction is still refused.

Prompts, raw output, message bodies, and source content do not belong in public
events or diagnostic projections. HumanRequest questions and replies are
private operator data;
public state contains no run/session locator, question, reply, cancellation
descriptor, delivery identity, provider data, process identity, or token.

## Repository automation boundary

The Maintainer App refuses workflow and CODEOWNERS publication because a pull
request can modify its own workflow, including `runs-on`. The protected branch
uses the `required` aggregate and exact combined-tree review. The CODEOWNED
classifier selects relevant source gates and sends unknown paths through the
Darwin gate. Persistent CI runner isolation remains a separate hardening
concern.
