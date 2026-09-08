# GitHub App authority decision

Status: the maintainer broker has one finite typed MCP surface whose every
operation names the repository it acts on, bounded by the App's installation.
Its merge mutations are two explicit, mutually exclusive operations: queue
enqueue where the base has a merge queue, and exact-head squash merge where it
does not. This document does not itself authorize App registration,
installation, repository publication, merge, release, or live-factory changes.

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

The official broker is hosted as a Cloudflare Worker with SQLite Durable
Objects. A self-hosted broker may choose another implementation while preserving
the same typed authority contract. The public site and release-manifest host
are not implicitly the credential broker.

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

The broker implementation lives in the standalone `control-plane/` crate, not
in the local-runtime workspace or `factoryd`. Its webhook remains a
versioned, signed, inert maintainer `ping` boundary. With the App-authority
group configured, readiness and every ping also prove that the broker can sign
an App JWT and that the key belongs to the configured App. It names no
repository: the installation, its bounded permissions, and the repository
identity are established per operation, when a token is minted for the
repository the caller named. This proof creates no installation token and
exposes no repository operation through webhook intake. Every non-ping event
is policy-rejected.
The hosted adapter is a Rust Cloudflare Worker; repository operations are
available only through the separately authenticated finite MCP surface.
Its production journal uses strongly consistent SQLite Durable Objects;
native SQLite exists only behind the non-default `development-sqlite` feature
and can never satisfy readiness. The production adapter accepts exactly
`DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET`,
`DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION`, and
`DARK_FACTORY_MAINTAINER_APP_ID`, plus the all-or-nothing App-authority group
`DARK_FACTORY_MAINTAINER_PRIVATE_KEY_PKCS8`,
`DARK_FACTORY_MAINTAINER_PERMISSION_REVISION`,
`DARK_FACTORY_MAINTAINER_OPERATOR_EMAIL_SHA256`,
`DARK_FACTORY_CLOUDFLARE_ACCESS_TEAM_DOMAIN`, and
`DARK_FACTORY_CLOUDFLARE_ACCESS_AUD`. The private key is standard
base64 of unencrypted PKCS#8 DER, no repository is configured, and the
implemented permission revision is exactly
`maintainer-operations-v6`. Missing webhook authority or a partial or
syntactically invalid App-authority group leaves the fixed inactive router with
no webhook route. An unusable key or configured but unavailable or drifted
Durable Object journal or GitHub authority makes readiness and ping
acknowledgement fail closed.

The namespace is sharded deterministically by App ID and the first byte of the
SHA-256 delivery-ID digest. This keeps every replay identity on one serialized
object without creating a global singleton. Each shard owns one private SQLite
database, creates the reviewed schema on first use, records the exact migration
digest, and audits both stored table definitions and its migration row before
use. The delivery primary key and complete replay-identity comparison preserve
exact replay and conflict behavior across concurrency, eviction, and Worker
restart.

The intended stable route is
`https://maintainer.darkfactory.build/v1/github/maintainer/webhook`, but committing
the adapter does not register that domain, deploy the service, configure an
App, or activate a webhook. Production credentials are never shared with
preview or disposable deployments; those use a distinct App, secret, Durable
Object namespace, and activation contract. `workers.dev` and preview URLs are
disabled and the checked-in configuration has no route, so an upload cannot
silently claim a public ingress. All eight production authority settings are
required Cloudflare secrets. There is no database URL, owner integration,
runtime role, provider API key, or ambient authentication fallback.

`wrangler secret put` deploys immediately. The
[control-plane activation sequence](../../control-plane/README.md#activation-gates)
stages an exact version and its secrets without traffic, verifies names and
bindings without reading values, and adds the route after independent review.
This is deployment authority, not provider or task authority. Product webhook intake and
operator/PWA projections keep separate routes, configuration, storage
namespaces, and authentication even if they later share hardened HTTP or
signature primitives.

The local coordinator client has no direct GitHub mutation path. Every
maintainer effect executes inside the broker authenticated as the exact
operation-bound App installation. It never falls back to ambient `git` or
`gh` authentication, SSH agents or keys, personal access tokens, browser
sessions, credential helpers, or the macOS keychain. An unavailable or denied
broker, including a `403`, fails closed. An effect with an uncertain result is
indeterminate until the typed reconciliation operation proves its outcome; it
is never retried through personal authority.

Every deserialized GitHub response models only fields GitHub returns to *this*
App's permission set. That is a contract about live responses, not a style
preference: a required field GitHub omits makes an otherwise successful 200
fail to deserialize, and the caller sees an opaque "authority is unavailable"
naming no endpoint. `delete_branch_on_merge` was exactly this and disabled all
eleven repository-observing operations until it was removed -- three of them
through `verify_workflow_pr` rather than directly. A source-level assertion
that such a read exists proves nothing about whether it works, so the shape
contract is proven by deserializing a captured real body instead. The tree
carries that proof for `RepositoryMetadata` only; the other response types have
no such fixture, so treat them as unproven rather than as checked.

The live maintainer broker exposes only these repository-scoped operations:

- verify one named repository's installation, numeric repository ID, permission
  revision, and live default branch head;
- observe the exact commit any branch points at, or its absence;
- observe one file's exact bytes at one exact commit, or its absence;
- observe one exact commit's parents and recursive tree: every path with its
  git object kind, file mode and object id, refused whole when GitHub truncates
  it, and with no judgement about what a difference between two of them means;
- observe one durable operation UUID without mutating it;
- create, observe, and resolve one bounded issue with an evidence comment;
- publish one exact independently reviewed tree as an App-authored commit to a
  generated branch;
- create one PR for that exact branch and base;
- replace one open PR body only while it still names the caller's exact head;
- close one PR only while it still names the caller's exact head;
- submit one bounded exact-head review verdict through the Pull Request Review
  API;
- observe Check Runs, bounded workflow/job/step state, a bounded failed-job log
  tail, and eventual merge state for one exact PR head;
- rerun failed jobs from one exact completed failed workflow attempt;
- enqueue one exact reviewed head for merge after its bound checks and
  approvals;
- squash-merge one exact reviewed head when its queue-less base either has a
  strict active ruleset or satisfies the private-unprotected 403 contract;
- publish and observe one immutable semver release tag, and recover only that
  exact tag through the fixed release workflow; and
- dispatch and observe the fixed control-plane deployment workflow at one exact
  default-branch commit and reviewed tree.

Replacing the already-open canonical bodies for #126, #153, and #188 is a
one-time Phase 0 bootstrap action, not a maintainer-broker operation. It must
be performed by the operator in GitHub's UI or by a distinct explicitly
authorized tool that binds the numeric issue, expected current-body digest,
replacement digest, and exact reviewed body, and exposes no comment, label,
state, or close authority. Until that bootstrap authority exists, the reviewed
replacement bodies remain local and Phase 0 is incomplete.

The live tools mint only their operation-specific subsets of Actions write,
Administration write, Metadata read, Contents write, Issues write, Pull
requests write, Checks read, and Merge queues write. Administration write is
minted only by `merge_pull_request_at_head`, because GitHub omits ruleset bypass
actors from callers without ruleset-write access; that operation uses it only
for fixed `GET` requests for the active rulesets and exposes no administration
mutation. Issues write exists only for bounded issue creation and
evidence-backed terminal state. Pull requests
write authorizes PR creation, the bounded exact-head body replacement, formal review, the bounded close operation, and
the exact-head enqueue, which mutates the pull request's queue state; a PR
review is not an Issues API comment. Merge queues write authorizes only the
typed exact-head enqueue and
reconciliation operation; the enqueue token also mints Contents write, because
a queued entry ends with GitHub pushing the squash commit to the default branch
(#371 tracks the live proof of the scope set). Actions write is narrowed by code
to exact workflow/run identities; Contents write also authorizes only the
strict exact-head squash operation described below. No generic workflow,
administration mutation, Secrets, arbitrary status, caller-selected merge,
dequeue, queue-jump, or generic API authority is exposed. Exact-tree publication still
rejects any tree that changes
`.github/workflows/**` rather than silently publishing an incomplete or
unauthorized workflow update.

The installation must carry the complete revision grant even though each
operation token receives only its subset. That all-or-nothing check prevents
`maintainer_status` from reporting v6 for a repository where direct merge is
unusable; it does not copy Administration into any other operation token.

Workflow and CODEOWNERS publication is outside the maintainer broker's typed
surface. It can proceed through another repository authority without widening
the broker.

When the maintainer App also authored the PR — which is every PR it reviews —
GitHub refuses a self-review that takes a side, `APPROVE` and `REQUEST_CHANGES`
alike. So the formal review is always submitted as a `COMMENT` and carries its
bounded findings plus one App-written verdict line, which is what the required
`review` check reads; the GitHub review state never carries the verdict. The
review is not an independent GitHub approval and cannot satisfy a
distinct-reviewer requirement. The status check consumes a cold review from a
separate agent or person.

The two typed merge operations are mutually exclusive; neither silently falls
back to the other. Before queue enqueue, the broker re-reads the PR and requires
the bound base and head. The GraphQL mutation supplies `expectedHeadOid`, never
`jump`, and the broker reconciles the exact queue entry. The enqueue result
reports the state the entry was created in, which makes an immediately
`UNMERGEABLE` entry visible. An entry already present before the durable claim
is refused as external; it is never adopted as an App enqueue. The read-only
merge observer first binds to the completed durable App enqueue attempt, then
distinguishes its active exact entry, a merged PR after that attempt with its
merge commit, and an exact PR whose recorded entry is no longer in the queue.
The last state deliberately does not guess whether the entry was ejected or
manually removed. The durable entry ID is returned in every state; a generic
`merged: true` response alone is not reported as queue lineage. GitHub tests
the exact PR head against the queue's latest base before merging.

`merge_pull_request_at_head` is the deliberately narrower path for a private
repository whose plan does not offer merge queues. The normal protected path
requires an active strict squash ruleset and proves the App is absent from each
disclosed bypass list. When that rules read returns exactly 403 on a private
repository, the alternate path instead requires GitHub to report
`protected:false` and squash enabled, proves the queue field is explicitly
null before and after checks, then re-reads the branch as still unprotected at
the same SHA and the pull request against that unchanged base before merge.
Both paths require nonempty completed non-failing exact-head checks, a
durable Maintainer `ALLOW`, and no exact-head `BLOCK`; classic protection alone
is unsupported. The fixed squash request atomically binds the head SHA, while
the final base read narrows—but cannot eliminate—the remaining base race. A
network-ambiguous response is reconciled
only when the merged PR names the exact head and base and its squash commit
carries this operation's digest marker. It is never blindly repeated or adopted
from an external merge.

Every request binds the App installation, repository numeric ID, permission
revision, operation kind, exact expected base and head where applicable,
immutable tree or bounded payload digest, operation ID, canonical request
digest, and durable reconciliation state. A provider or coordinating agent
receives only the typed result; App keys, JWTs, installation tokens, headers,
and credential-bearing URLs remain in broker memory and never enter the agent,
repository, prompt, worktree, log, or SQLite state.

Existing refs are never force-updated or replaced. Publication may adopt only
the exact expected generated ref/commit and refuses the live default branch;
any different target is a conflict.
GitHub's `delete_branch_on_merge` setting removes the source ref as part of
merge processing. The broker does not read that setting: GitHub returns it only
to a caller holding Administration access, and repository metadata operations
deliberately do not mint that permission. The direct-merge operation's narrow
ruleset-read token does not turn this unrelated setting into a precondition.
Source-branch cleanup is the repository owner's setting, and the broker exposes
no read-then-delete ref mutation either way.
This maintainer surface is permanent official coordinator infrastructure, not
executable intake and not the future runtime broker.

## Repository selection

Which repositories the Maintainer may act on is the App installation's answer,
not this service's configuration. There is no configured repository: the caller
names an `owner/name` on every operation, and the grant is what binds it.

The mechanism is the installation token itself. The broker looks the
installation up by the named repository, checks it is this App, still
selected-repository, unsuspended, and carries the revision's permissions, then
mints a token naming that repository. GitHub answers with the exact repository
the token covers, and the broker accepts the token only if it covers exactly
one repository, exactly the one named, owned by the installation's own account,
with exactly the permissions requested. The numeric repository ID is read out
of that answer rather than asserted from configuration, so identity comes from
the grant that authorizes the access rather than from a value a caller or an
operator could restate wrongly. Token and repository are then held together for
the operation's lifetime, so no request can be spelled for a repository other
than the one its credential was minted for.

Three consequences are deliberate:

- Adding a repository is an installation change, not a deployment. Removing one
  likewise revokes reach the moment GitHub stops minting tokens for it.
- The repository is part of every operation's replay identity, so one operation
  UUID cannot address two repositories as though they were one request. Names
  are lower-cased before the digest is taken, because GitHub resolves them
  case-insensitively and two spellings of one repository must not become two
  operations.
- A single Access identity can act on every repository the App is installed on.
  Narrowing that — mapping a caller identity to the installations it may use —
  is the tenancy question, and it is a filter in front of repository
  resolution, not a change to this mechanism.

Operations that name a workflow take its path as an argument for the same
reason. A hard-coded `.github/workflows/ci.yml` silently meant "this tool works
on one repository".

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

The coordinator retains the exact canonical request until completion. After a
closed transport, `observe_operation` says whether the UUID never reached the
journal, is still planned or executing, is indeterminate, or completed; a
completed record includes its typed result. This observation does not retry a
write. Only the byte-identical request may be replayed under that UUID. If the
caller discovers that its local request was truncated or assembled from the
wrong bytes, it uses a fresh UUID for the corrected request rather than
turning an idempotency conflict into a guess.

The runtime operation set is deliberately finite: read the exact default
revision, create, observe, and resolve one bounded issue with an evidence
comment, publish one exact immutable Change tree to a generated branch, create
one PR, observe checks and merge state for its exact head, post one bounded
formal PR review, enqueue it, publish and observe one immutable release, and
dispatch one fixed control-plane deployment. GitHub owns merged source-branch
cleanup through delete-on-merge. There is no generic issue-comment or closure
authority and no arbitrary REST, GraphQL, Git, shell, merge, ref update, or
administration mutation escape hatch.

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

`maintainer-operations-v6` grants exactly what
`maintainer-operations-v5` granted. It adds one bounded
`update_pull_request_body` operation, so the revision changes as the surface's
fail-closed handshake even though no GitHub permission expands. The request
binds the pull-request number, exact head, and replacement body in the durable
digest, re-reads that head before the fixed body replacement, and accepts or
reconciles only the App-marked replacement at that head. Caller text cannot
carry an App operation marker or review-verdict marker. GitHub's body API does
not offer the merge API's atomic `expectedHeadOid`; a moved or uncertain
response is therefore indeterminate rather than reported as success. Rotate
the `DARK_FACTORY_MAINTAINER_PERMISSION_REVISION` secret before promoting a v6
build.

`maintainer-operations-v5` grants exactly what
`maintainer-operations-v4` granted. It adds one bounded
`close_pull_request` operation, so the revision changes as the surface's
fail-closed handshake even though no GitHub permission expands. The request
binds the pull-request number and exact head in the durable digest, re-reads
that head immediately before the fixed close request, and accepts or
reconciles only a closed, unmerged response at that head. GitHub's close API
does not offer the merge API's atomic `expectedHeadOid`; a moved or uncertain
response is therefore indeterminate rather than reported as success. Rotate
the `DARK_FACTORY_MAINTAINER_PERMISSION_REVISION` secret before promoting a v5
build.

`maintainer-operations-v4` adds Administration write to
`maintainer-operations-v3` solely because GitHub withholds ruleset bypass actors
from a caller without ruleset-write access. Only the strict direct-merge
operation mints it, and only for fixed detailed-ruleset reads; the broker has no
administration mutation operation. The revision also adds that separate strict
direct-merge operation.
Before promotion, update the App registration and accept the new permission for
every installation the broker must serve. `/readyz` names no repository and
therefore cannot prove installation acceptance; after promotion,
`maintainer_status` must report v4 for each named repository.
Rotate the `DARK_FACTORY_MAINTAINER_PERMISSION_REVISION` secret before
promoting a v4 build; a build promoted against the old value fails its own
authority check, readiness reports not-ready, and the deploy gate rolls it
back.

`maintainer-operations-v3` grants exactly what `maintainer-operations-v2`
granted; no GitHub permission changed. It is a new revision because the
revision is the surface's fail-closed handshake and the operation contract did
change: every tool gained a required `repository`, three gained a
`workflow_path`, and the replay digest now covers the repository, so an
operation UUID journalled under v2 cannot be replayed under v3. Rotate the
`DARK_FACTORY_MAINTAINER_PERMISSION_REVISION` secret before promoting a v3
build; a build promoted against the old value fails its own authority check,
readiness reports not-ready, and the deploy gate rolls it back.

The `maintainer-operations-v2` revision added only Actions
read/write, Contents read/write, Issues read/write, Pull requests read/write,
Checks read, Merge queues read/write, and Metadata read. Each credential minted
from it is downscoped again to one typed operation. Pull requests write covers
PR creation and the formal review API; Contents write covers exact generated-ref
and immutable tag creation, queue completion, and the fixed exact-head squash
mutation; Actions write covers exact failed-run recovery and
the two fixed workflow dispatches. None creates a generic mutation surface.
The direct mutation is not generic merge authority and cannot target a
queue-enabled base. GitHub's required delete-on-merge repository setting
performs source-branch cleanup atomically instead of a broker read-then-delete
race.

## Bound App publication

For a Dark Factory-authored publication, the broker authenticates as the exact
installation and builds immutable blobs, one tree, and one commit through the
Git Database API. Those objects do not publish a branch. The only persistent
publication is the final ref write: it creates an absent topic ref directly at
the commit or advances an existing exact-parent ref without force. A moved ref
therefore fails instead of being overwritten, and a failed first publication
cannot strand an empty branch at the parent. Before reporting success the
broker requires the returned ref and commit to agree; a lost response is
reconciled only from a branch tip with the byte-exact operation message, the
complete request digest in its trailer, the exact materialized request tree,
and the stated sole parent.

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

GitHub documents no general idempotency key for Git commit creation. Publication
therefore uses a durable at-most-once claim, an exact-parent non-forced ref
write, and branch-tip reconciliation; it never blindly resubmits an ambiguous
publication.

Runtime ref creation never uses force or replacement. A pre-existing generated
ref is success only when it already names the exact verified commit. Runtime
crash tests cover request-before-effect, effect-before-acknowledgement, and
acknowledgement-after-effect for publication and formal review; ambiguity stops
instead of turning SDK retry defaults into a second mutation. The maintainer
broker has separate equivalent tests for its exact merge observation and
GitHub-owned delete-on-merge precondition.

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
