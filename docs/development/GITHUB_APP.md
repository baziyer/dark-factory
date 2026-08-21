# GitHub App authority decision

Status: proposed and inactive. This document does not authorize App
registration, installation, token minting, delivery intake, repository
publication, merge, release, or live-factory changes.

## Decision

Dark Factory will support two deployments behind the same bounded operation
contract:

1. The official deployment uses one Dark Factory-operated control plane with
   two permanent GitHub App registrations. `Dark Factory` is the product App;
   `Dark Factory Maintainer` is the private internal coordinator App. Each has
   its own App ID, private key, installations, permission revisions, operation
   journal namespace, and audit identity. The broker, outside the operator
   machine and provider runtime, owns both keys, mints repository- and
   permission-restricted installation tokens, performs the exact GitHub
   operation, and discards the token from memory. It never returns a GitHub
   credential to `factoryd`, and neither key is distributed with Dark Factory
   or copied into `factoryd`.
2. A self-hosted deployment may use its own App and broker. The operator owns
   that key and broker, but the daemon and provider contract is unchanged. A
   BYO deployment is not a mode that loads a private key into `factoryd`.

The broker's hosting vendor, region, and operational topology are deployment
choices for the future control plane. They do not change this authority
boundary. The public site and release-manifest host are not implicitly the
credential broker.

## Registration identity

Both official registrations use `https://darkfactory.build` as their canonical
homepage. The product App's display name is `Dark Factory`, subject to GitHub
name availability, with target bot identity `dark-factory[bot]`. The internal
App's display name is `Dark Factory Maintainer`, with target bot identity
`dark-factory-maintainer[bot]`. Their badges are distinct raster derivatives of
the canonical `dark-factory-site/app/icon.svg` artwork so operators can tell the
product and maintainer authorities apart. Branding metadata conveys no
repository authority and does not relax any activation gate in this document.

## Phase 0 official maintainer broker

Development coordination needs a separate, non-product authority before the
runtime integration can activate. The permanent official private `Dark Factory
Maintainer` App and its operator-approved installation may act only on
explicitly selected development repositories. Its typed broker/tool surface is
outside `factoryd`, is never callable through an attempt capability, and is not
shipped, registered, or installed as part of Dark Factory product activation.
The product and maintainer registrations may share control-plane implementation
but never keys, installation mappings, permission revisions, operation journal
namespaces, or audit identities.

The broker implementation belongs in the sibling
`dark-factory-control-plane` service, not in the pure-Rust local-runtime
workspace or `factoryd`. The temporary `control-plane/` staging export proves
only a versioned, signed, inert maintainer `ping` boundary. Every non-ping event
is policy-rejected. Its default build uses a durable Postgres journal behind
Vercel's official Rust and Axum runtime adapter; SQLite exists only behind the
non-default `development-sqlite` feature and can never satisfy readiness. The
production adapter accepts exactly `DATABASE_URL`,
`DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET`,
`DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION`, and
`DARK_FACTORY_MAINTAINER_APP_ID`. Missing or partial configuration leaves the
fixed inactive router with no webhook route. A configured but unavailable or
unmigrated Postgres journal makes readiness and delivery acknowledgement fail
closed. The URL must bind its host, user, password, database, and TLS mode;
ambient `PG*` connection settings are rejected rather than becoming a hidden
credential or routing fallback.

The intended stable route is
`https://broker.darkfactory.build/v1/github/maintainer/webhook`, but committing
the adapter does not register that domain, deploy the service, configure an
App, or activate a webhook. Production credentials are never shared with
preview deployments; preview integration requires a distinct disposable App,
secret, and database. The migration is applied explicitly before deployment,
never by a function cold start. The fixed-output migration runner can execute
under `vercel env run -e production -- cargo run --locked --bin migrate`
without printing `DATABASE_URL`; this is an operator deployment step, not
provider or task authority to pull production configuration. Readiness verifies
the expected migration revision and digest as well as the required table shape
and runtime privileges. Product webhook intake and operator/PWA projections
keep separate routes, configuration, storage namespaces, and authentication
even if they later share hardened HTTP or signature primitives.

The local coordinator client has no direct GitHub mutation path. Every
maintainer effect executes inside the broker authenticated as the exact
operation-bound App installation. It never falls back to ambient `git` or
`gh` authentication, SSH agents or keys, personal access tokens, browser
sessions, credential helpers, or the macOS keychain. An unavailable or denied
broker, including a `403`, fails closed. An effect with an uncertain result is
indeterminate until the typed reconciliation operation proves its outcome; it
is never retried through personal authority.

The maintainer broker exposes only these repository-scoped operations:

- create one bounded issue;
- publish one exact independently reviewed tree and App-verified commit to a
  generated branch;
- create or update one PR for that exact branch and base;
- submit one bounded formal Pull Request Review through the Pull Request Review
  API;
- observe Check Runs for one exact PR head;
- enqueue one exact reviewed head for merge after its bound checks and
  approvals; and
- delete the exact generated branch after the bound merge result.

Replacing the already-open canonical bodies for #126, #153, and #188 is a
one-time Phase 0 bootstrap action, not a maintainer-broker operation. It must
be performed by the operator in GitHub's UI or by a distinct explicitly
authorized tool that binds the numeric issue, expected current-body digest,
replacement digest, and exact reviewed body, and exposes no comment, label,
state, or close authority. Until that bootstrap authority exists, the reviewed
replacement bodies remain local and Phase 0 is incomplete.

This maintainer revision may use Metadata read, Issues read/write, Contents
read/write, Pull requests read/write, Checks read, and Merge queues write.
Issues write exists only for the typed issue-creation operation: the surface
exposes no issue-comment, issue-update, or issue-close operation. Pull requests
write authorizes PR creation, update, and formal review; a PR review is not an
Issues API comment. Merge queues write authorizes only the typed exact-head
enqueue and reconciliation operation. No Actions, Workflow, Release,
Administration, Secrets, arbitrary status, direct-merge, dequeue, queue-jump,
or generic API authority is exposed. Because the maintainer revision has no
Workflows permission, exact-tree publication rejects any tree that changes
`.github/workflows/**` rather than silently publishing an incomplete or
unauthorized workflow update.

A workflow change is therefore a separate human-authored and human-published
pull request. It is never smuggled through the maintainer broker, and broker
refusal never authorizes the coordinating agent to fall back to a personal
token or credential helper. The ordinary non-workflow stack may resume through
the broker only after that workflow pull request has passed its own review and
merged.

When the maintainer App also authored the PR, its formal review may record
bounded findings, `COMMENT`, or `REQUEST_CHANGES`; it is not an independent
GitHub approval and cannot satisfy a distinct-reviewer requirement. The cold
review must still be performed by a separate agent or person, and repository
policy may require a separate GitHub actor to approve before the typed merge.

The typed merge operation uses a GitHub-enforced merge queue as its sole
automated path. Before enqueueing it re-reads the PR and requires the bound base
and head; the GraphQL mutation supplies `expectedHeadOid`, never `jump`, and the
broker reconciles the exact queue entry and eventual merge result. GitHub then
tests the exact PR head against the queue's latest base before merging. A base
or head mismatch observed before enqueue invalidates the operation; an
ambiguous enqueue is reconciled and never blindly repeated. A repository with
no merge queue, or only ruleset/classic branch protection the broker cannot
prove applies without bypass, is unsupported and fails closed. The broker does
not request Administration permission or expose direct merge as a fallback.

Every request binds the App installation, repository numeric ID, permission
revision, operation kind, exact expected base and head where applicable,
immutable tree or bounded payload digest, operation ID, canonical request
digest, and durable reconciliation state. A provider or coordinating agent
receives only the typed result; App keys, JWTs, installation tokens, headers,
and credential-bearing URLs remain in broker memory and never enter the agent,
repository, prompt, worktree, log, or SQLite state.

Existing refs are never force-updated or replaced. Publication may adopt only
the exact expected generated ref/commit; any different target is a conflict.
Branch deletion re-reads the ref, requires the exact merged head, and deletes
only the generated ref. A moved, reused, missing-before-acknowledgement, or
ambiguous ref becomes a reconciled success or a visible indeterminate/conflict,
never a blind delete or retry. This maintainer surface is permanent official
coordinator infrastructure, not executable intake and not the future runtime
broker.

## Authority boundary

`factoryd` is the only local authority that can turn an exact attempt request
into a repository operation. A provider receives only its existing attempt
credential and calls a finite local operation. It receives no App JWT,
installation token, Git credential helper, remote URL containing a secret, or
generic GitHub API proxy.

The same no-fallback rule applies to product operations: `factoryd` can ask
the broker for a typed effect but cannot invoke a credential-bearing Git or
GitHub client itself. Broker absence, denial, or an indeterminate result is a
visible operation failure, never authority to use an operator's personal
credentials.

Each durable operation binds:

- project and repository numeric identity;
- App installation and explicit permission revision;
- exact attempt, Run, Change, and immutable source digest where applicable;
- exact source/base commit;
- a canonical request digest and idempotency key; and
- one durable result, including the resulting Git object or PR identity.

The broker revalidates installation, selected-repository scope, permission
subset, expiry, and request binding before the effect. Cross-project,
cross-repository, cross-installation, stale-base, ambiguous, revoked, or
permission-expanded requests fail closed. Retry returns the exact prior result
or an idempotency conflict. A mutation submitted to GitHub without a durable
acknowledgement becomes visibly indeterminate and is not submitted again until
operation-specific reconciliation proves the result; the system never guesses
whether a second mutation is safe.

The future runtime operation set is deliberately finite: read an exact
revision, publish one exact immutable Change tree to a generated branch,
create or update one PR, observe Check Runs for its exact head, and post one
bounded formal PR review.
A later Issues-write revision may add source-issue closure after an exact
approved terminal workflow. Creating Check Runs, writing legacy commit
statuses, issue comments, and issue closure are unavailable in the publication
revision. There is no arbitrary REST, GraphQL, Git, shell, merge, ref update,
release, or administration escape hatch.

## Input quarantine

GitHub delivery authentication proves transport only. The initial adapter may
create an immutable `InputEnvelope` and quarantined `WorkCandidate`; it cannot
create a Task, Message, ProposedAction, provider prompt, or scheduling event.
Deterministic policy and explicit review bind one exact source revision before
any later materialization path exists.

The initial App does not subscribe to `issue_comment`, and Dark Factory does
not fetch or ingest comments. Reconciliation uses the same source identity,
revision, digest, and idempotency rules as delivery receipt so missed delivery
recovery cannot duplicate work. GitHub's unavoidable App lifecycle events are
validated and audited or rejected without materializing work.

Runtime App installation, activation, and executable intake remain disabled
until the provider-neutral quarantine in #126 is merged and independently
reviewed.
Public-untrusted execution additionally remains gated by #125's hardened
execution boundary.

## Permission revisions

Permission authority is durable and revisioned. Expansion requires a new
recorded revision and explicit operator approval; a prompt cannot widen it.

The initial intake-only revision requests only Metadata read and Issues read.
Its effective allowlist includes the automatic `installation` lifecycle event
plus subscribed `installation_target`, `repository`, and `issues` events. The
manifest does not list automatic events. The unavoidable
`installation_repositories` event may add or remove selected repositories;
addition stays unauthorized until explicit operator approval, while removal
revokes the existing mapping. `installation` actions separately cover
creation, suspension, unsuspension, deletion, and acceptance of new
permissions. Creation is recorded inactive; the others disable, revoke, or
record state but never expand authority or create work. Renames from
`installation_target` or `repository` update display metadata only; repository
transfer or deletion revokes the mapping pending fresh approval. Every other
lifecycle action fails closed. The revision requests no Contents, Pull
requests, Checks, Actions, Workflows, Releases, Administration, or Secrets
authority.

A separately approved publication revision may add only Contents read/write,
Pull requests read/write, and Checks read. It does not inherit broader
permissions speculatively. Closing a plain source issue or posting an issue
comment remains unavailable: either operation would require a later,
separately approved Issues-write revision and its own causal tests.
Pull requests write covers the formal Pull Request Review API; Contents write
covers exact generated-ref creation. Neither permission creates a generic
mutation surface. Runtime merge and branch deletion are not added by this
foundation; those operations above belong only to the separate maintainer
broker.

## Verified App identity

For a Dark Factory-authored publication, the broker authenticates as the exact
installation and creates the Git tree and commit from the daemon's immutable
post-attempt publication snapshot through an API that has been causally proven
to sign as that App. Omitting a signature from the REST Git commit API does not
satisfy this contract. Before branch or PR publication is reported successful,
Dark Factory requires the selected API to return both
`verification.verified == true` and `verification.reason == "valid"`.

The same read must match the expected repository, App bot numeric identity,
tree SHA, sole parent/base SHA, and byte-exact bounded message and trailers.
No branch ref is created or advanced and no PR is opened or updated until every
check succeeds.

The durable audit records Run, Change, exact source digest, base commit,
resulting commit, installation, permission revision, request digest, and
verification result. Bounded `Dark-Factory-Run` and `Dark-Factory-Change`
trailers provide provenance. Dark Factory is credited only when it materially
authored the change.

The safe kernel has already deleted the old
`Co-authored-by: Dark Factory <factory@localhost>` publication behavior; it
must not be reintroduced. No DCO `Signed-off-by` trailer is added
automatically: legal signoff is a separate explicit product-policy decision,
not cryptographic commit verification.

GitHub documents no general idempotency key for Git commit creation. The REST
Git Database sequence therefore remains blocked until the broker can prove
at-most-once submission and honest indeterminate recovery, or an atomic API is
shown to preserve every Change byte and Git mode. No implementation may claim
exactly-once commit publication from blind REST retry.

Runtime ref creation never uses force or replacement. A pre-existing generated
ref is success only when it already names the exact verified commit. Runtime
crash tests cover request-before-effect, effect-before-acknowledgement, and
acknowledgement-after-effect for publication and formal review; ambiguity stops
instead of turning SDK retry defaults into a second mutation. The maintainer
broker has separate equivalent tests for its exact merge and branch deletion.

## Activation gates

Preparing contracts, an inactive broker protocol, or a private App manifest
does not activate the integration. Activation requires all of the following:

- an independent exact-main safe-kernel boot ALLOW;
- merged and independently reviewed provider-neutral quarantine;
- explicit operator approval for the exact App installation and permission
  revision;
- causal crash, replay, stale-source, wrong-scope, revocation, and credential
  absence proofs; and
- a disposable-repository dogfood run before any non-test repository is in
  scope.

Until #125 provides hostile-process isolation, the credential-absence proof is
limited to deliberate dataflow: no GitHub credential is passed to provider
argv/env, files, prompts, logs, or protocol data. The current same-UID kernel
does not claim to contain a hostile provider that reads other operator files or
credentials.
