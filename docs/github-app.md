# Dark Factory GitHub App preparation

This is the security-boundary preparation for [#188](https://github.com/baziyer/dark-factory/issues/188), composed with [#153](https://github.com/baziyer/dark-factory/issues/153) and [#154](https://github.com/baziyer/dark-factory/issues/154). It does not register or install an App, create a credential, change attribution, or contact GitHub as an App.

The machine-readable artifacts are deliberately split by trust boundary:

- [`github-app-manifest.json`](github-app-manifest.json): the reviewed permission/event **policy**. It is not a GitHub registration payload because it has no endpoint.
- [`github-app-repository-selection.json`](github-app-repository-selection.json): the installation boundary.
- [`github-app-event-actions.json`](github-app-event-actions.json): the receiver action allowlist used to derive the event revision.
- [`github-app-registration.json`](github-app-registration.json): an endpoint-bound registration gate, intentionally incomplete until the operator supplies exact values.
- [`github-app-semantic-fixtures.json`](github-app-semantic-fixtures.json): executable contract cases for delivery identity, authority feasibility, error classification, and registration readiness.

The operator must still confirm the owner, endpoint, repositories, and recovery contact before any registration. A trusted setup operation must fail closed unless the registration gate has an exact endpoint, `hook_attributes.active: true`, daemon secret-store readiness, selected repository ID, policy permission digest, receiver event digest, and explicit operator confirmation. The policy artifact and endpoint-bound registration payload must never be conflated.

## Requested registration

Create a private App, visible only on its owning account, with the Dark Factory logo. The first installation must use **Only select repositories** and select exactly:

```text
baziyer/dark-factory
```

Do not select `baziyer/dark-factory-site`, `baziyer/homebrew-tap`, or all repositories. A future source repository or release surface needs a separate operator decision and installation-scope change. Repository selection is independent of the permission manifest: an installation token must also be narrowed to the exact repository and permission subset needed for each request.

The logo reference is the shipped site icon at [`~/dark-factory-site/app/icon.svg`](https://github.com/baziyer/dark-factory-site/blob/main/app/icon.svg): 64×64, `#070A09` background, `#4AC7A5` outline, and `#1E6F60` center. The inspected source is site commit `566e83620775de01f28224cf4c73e57d89c08cab`, blob `00312914cf70a625a61ac1ccdfa8cf9245fd886f`; re-check that source and record the uploaded asset checksum at registration time. Upload that asset through the GitHub App registration UI only after the operator confirms the owning account and repository scope. Do not copy it into a provider worktree or generate a new logo in this preparation.

### Permission rationale

| Permission | Level | Needed for | Explicitly not granted |
| --- | --- | --- | --- |
| Metadata | read | repository identity and boundary checks; `repository` events | no administration or collaborator management |
| Issues | write | issue intake, issue comments, labels, and the post-merge issue closure in #153 | no project/discussion/secret mutation |
| Pull requests | read | linked PR state, head/base SHA, merge state, and review evidence | no PR review comments, labels, or merge authority |
| Checks | write | the App-authored bounded status check in #153/#154; read existing check runs | no workflow execution authority |
| Actions | read | release-readiness workflow-run observation for #154 | no dispatch, cancellation, artifact deletion, or workflow mutation |
| Contents | read | commit/release metadata and exact merged-SHA verification | no push, branch, tag, release, or asset write |
| Commit statuses | read | compatibility with legacy hosted status contexts during gate reconciliation | no status write |

`issues:write` is sufficient for ordinary timeline comments on both issues and PRs because GitHub exposes a PR as an issue for that operation. The App must not request `pull_requests:write`; that broader permission would also grant merge-related authority. Line-level PR review comments are out of scope for this identity.

No organization or account permissions are requested. No `administration`, `repository_hooks`, `secrets`, `workflows`, `packages`, `deployments`, `members`, or `organization_*` permission is requested. Release publication remains the existing trusted Actions path in `.github/workflows/release.yml`; adding `contents:write` later is a separate security-reviewed change, not part of this manifest.

### Event rationale and receiver allowlist

The manifest subscribes only to these event families:

- `issues`: `opened`, `edited`, `labeled`, `unlabeled`, `reopened`, `closed`, `transferred`, and `deleted` for #153 intake and reconciliation.
- `issue_comment`: `created`, `edited`, and `deleted` for issue/PR conversation changes. Bodies are untrusted input.
- `pull_request`: `opened`, `edited`, `reopened`, `synchronize`, `ready_for_review`, `converted_to_draft`, `closed`, and label changes for linkage and #154 readiness.
- `pull_request_review`: `submitted`, `edited`, and `dismissed` for independent-review evidence; the App never approves its own work.
- `check_run` and `check_suite`: `completed`, plus only the App's own bounded re-request path if that is separately implemented. Existing checks are observed, not trusted merely because the event arrived.
- `workflow_run`: `requested`, `in_progress`, and `completed` for the existing trusted release workflow and exact commit binding.
- `release`: `created`, `published`, `prereleased`, `edited`, and `deleted` for release verification only; this App does not publish or edit releases.
- `repository`: `archived`, `deleted`, `renamed`, `transferred`, `publicized`, and `privatized` to fail closed when the selected-repository boundary changes.
- `installation`, `installation_target`, and the automatic `installation_repositories` lifecycle delivery to audit install, suspend, rename, permission, and repository-selection changes.

The receiver rejects every unlisted action, validates the webhook signature over the exact bytes, binds every payload to its installation and repository, and records a bounded immutable delivery before any work candidate or projection. GitHub documents `installation_repositories` as an automatic App delivery rather than a manually selected event; it must still be handled for scope changes.

### Immutable identity pin

Every connector mutation must carry and durably audit one immutable identity tuple:

```text
app_id
app_slug
installation_id
installation_account_id
repository_id
repository_full_name
repository_selection = selected
permission_revision = sha256(canonical(github-app-manifest.json.default_permissions))
event_revision = sha256(canonical({"default_events": github-app-manifest.json.default_events, "receiver_actions": github-app-event-actions.json}))
key_fingerprint
```

The connector rejects a missing, unknown, or changed tuple before minting or using a token. It verifies the installation account, repository ID/full name, selected-repository membership, granted permission response, manifest revision, event revision, and key fingerprint against daemon-owned operator state. The tuple is copied into the mutation's request/result audit record and run-bundle metadata; no mutation is accepted merely because a token exists. An installation, repository selection, permission, event, or key change creates a new revision and invalidates outstanding mutation proposals.

## Credential and token boundary

The future daemon-owned connector, not a provider session, owns this material:

1. Store the App ID, installation ID, key fingerprint, selected repository IDs, permission revision, and last rotation/revocation times as non-secret daemon state.
2. Store the private key and webhook secret in the operator's OS secret store (macOS Keychain in the supported product), under a Dark Factory service/account namespace. A keychain-unavailable setup fails closed; it never falls back to a provider worktree, task body, database blob, isolated provider config, log, event, run bundle, argv, provider environment, or `gh` configuration.
3. Keep any encrypted export outside all project and agent worktrees, owner-readable only, with its encryption key held separately by the OS secret store. Do not add a secret file to the repository. Provider processes receive neither the private key nor an installation token; neither may appear in provider argv/env/config, worktrees, task bodies, logs, public events, crash/retry diagnostics, or run bundles.
4. Mint an installation JWT/token only in the connector. Pin every token request to the selected repository ID and the smallest permission subset; do not rely on GitHub's default “all repositories/all granted permissions” token behavior. Keep the token in memory, refresh before its one-hour expiry, and erase it after the request or on shutdown.
5. Audit every GitHub mutation with the full identity tuple, API operation kind, result, request/correlation ID, originating Dark Factory task/change, and exact target SHA where applicable. Never log Authorization headers, JWTs, private keys, webhook secrets, issue bodies, or full request payloads.

### Rotation

Rotation is an operator-authorized, two-key transition: create the replacement GitHub App key, store it under a new fingerprint, validate App-authenticated token minting and a read-only repository call, switch the connector atomically, allow only the bounded lifetime of in-flight tokens, then delete the old GitHub key and secret-store entry. A failed validation leaves the old key active. Permission or repository-scope changes are not hidden inside rotation and require a fresh operator approval plus an installation audit record.

Webhook-secret rotation keeps the previous verifier only for a short, recorded overlap after GitHub is changed, accepts both only during that bounded window, then deletes the old value. A valid first delivery with a new, well-formed `X-GitHub-Delivery` GUID is accepted only after signature, installation, repository, event, and action validation. The daemon atomically stores GUID + raw-payload digest + identity-tuple digest with the one durable result. GitHub redelivery with the exact same triple returns or reconciles that result without a second mutation. Missing or malformed GUIDs, or a reused GUID with a different payload digest or identity tuple, are rejected as an idempotency conflict. A signature failure, installation mismatch, repository mismatch, or action outside the allowlist is rejected visibly.

### Revocation and degraded behavior

The connector has explicit durable states: `healthy`; `degraded_rate_limited`; `degraded_unavailable`; `permission_mismatch`; `target_not_found`; `scope_revoked`; and `installation_revoked`. A `429` or provider rate-limit response records the server-provided retry time, stops new mutation attempts, and schedules one bounded reconciliation after that time; it never mints tokens in a tight loop. Network timeout, DNS/TLS failure, GitHub `5xx`, or connector crash records `degraded_unavailable` with bounded exponential retry and visible evidence. Retry exhaustion becomes one actionable `NEEDS YOU` item, not a silent fallback. A target-specific `404` is mutation-blocking but is classified as `target_not_found` only after an independent installation, repository-scope, and grant check succeeds; it creates a durable source-reconciliation item and does not claim App revocation. A confirmed removed repository scope is `scope_revoked`; a verified uninstall, suspension, or installation-level `401` is `installation_revoked`; a grant mismatch is `permission_mismatch`. These confirmed authority failures stop GitHub mutation immediately and expose recovery. Lost external responses are reconciled by exact identity/idempotency before retrying. It must not silently fall back to an operator token, `gh`, a provider credential, or a different repository. Reconciliation resumes only after the operator verifies the installation ID, repository allowlist, permission and event revisions, key fingerprint, rate-limit state, and webhook health.

## Attribution and identity verification seam

Autonomous GitHub API calls use an installation access token, so GitHub attributes them to the App bot rather than to an operator. User-to-server tokens are intentionally out of scope: they would attribute activity to a human and require additional authorization. Every UI/API projection must display the App bot identity and installation/repository audit context.

The future commit path must not guess a bot email from the App slug. The current policy grants only `contents:read` and `pull_requests:read` on `baziyer/dark-factory`; it cannot create a branch, commit, or PR, and it cannot reach an out-of-scope disposable repository. Therefore the read-only path makes no verification claim and emits a typed, expiring `github_app_identity_verification` temporary-authority proposal containing the exact disposable repository, required `contents:write`/`pull_requests:write` capabilities, operator approver, teardown plan, and expected policy revision. The proposal is durable audit state only: it is not registration, permission expansion, credential minting, or provider input. It must be rejected if it lacks an expiry, exact repository, teardown, or `provider_receives_credentials: false`.

Only after a separately approved authority exists may the trusted connector:

1. install the App only on the selected disposable/test repository;
2. use the installation token to create one test branch/PR or equivalent harmless commit path;
3. read the resulting commit's author/co-author identity and the App bot login/avatar from GitHub;
4. add a `Co-authored-by` trailer with the exact GitHub-linked noreply email only in a second disposable test commit;
5. verify in the GitHub UI/API that the trailer maps to the App bot and shows the App logo; and
6. record the verified bot login, numeric ID, exact email, test commit SHA, App ID, date, proposal ID, and teardown result as non-secret metadata.

The attribution policy is narrow: credit Dark Factory only when it materially authored the committed change. Monitoring, issue ingestion, reconciliation, rebasing, independent review, merge, release publication, and merely executing an operator-approved action do not earn a co-author trailer. Preserve human author and reviewer attribution. Replacing `factory@localhost` is a separate implementation and migration decision; this preparation intentionally does not change it.

## Compact future CLI surface

These are proposed daemon-owned operations, not commands implemented or safe to run by this preparation:

```text
factoryctl github app setup --manifest PATH --repository OWNER/REPO
factoryctl github app status [--json]
factoryctl github app rotate --confirm
factoryctl github app revoke --confirm
```

`setup` should validate the manifest, require the explicit selected repository and operator confirmation, then pause at the GitHub registration/install boundary. It must not silently register, install, widen permissions, or accept a provider-supplied path. `status` should show App/install IDs, selected repository IDs, granted-vs-requested permissions, event/webhook health, key fingerprint and age, last successful token mint, last delivery, last mutation, and degraded/revoked reason—never secrets. `rotate` and `revoke` require the operator principal and durable confirmation; provider sessions cannot call them.

## Operator checklist: the human boundary

No action below should be automated by this preparation.

- [ ] Choose the App owner and a recovery contact with authority to suspend, rotate, and delete it.
- [ ] Confirm the first repository list is exactly `baziyer/dark-factory`; leave `dark-factory-site`, `homebrew-tap`, and all other repositories unselected.
- [ ] Provide the deployed webhook endpoint and daemon secret-store host; do not use a provider worktree, task body, or provider environment.
- [ ] Review `github-app-manifest.json` as the permission/event policy and verify `github-app-event-actions.json`; reject any extra permission, event, OAuth request, or public visibility.
- [ ] Verify the endpoint-bound registration artifact has an exact endpoint, active hook, ready secret store, repository ID `1335380107` for `baziyer/dark-factory`, exact policy/event digests, and operator confirmation. Reject registration while any field is null, false, or mismatched.
- [ ] Upload the referenced Dark Factory logo and record the asset checksum/source.
- [ ] Register the private App only after the preceding choices are approved.
- [ ] Install it with selected-repository scope only; record installation ID and the GitHub audit event.
- [ ] Store the private key/webhook secret in the daemon-owned OS secret store and verify file/worktree/process scans contain no secret.
- [ ] If identity verification is needed, review the typed temporary-authority proposal, exact disposable repository, additional permissions, expiry, and teardown; do not treat the current read-only App scope as able to push.
- [ ] Exercise first delivery, exact redelivery, response loss, conflicting GUID/digest/identity, invalid signature, cross-repository, revoked-installation, permission/event-revision mismatch, edited-payload, `404` target absence, `404` scope removal, `429`/Retry-After, provider `5xx`, connector crash/restart, and App-unavailable paths from `github-app-semantic-fixtures.json` before any real mutation.
- [ ] Inspect provider argv, env, isolated config, worktrees, task bodies, logs, public events, crash/retry diagnostics, and run bundles; assert that private keys and installation tokens are absent from every surface.
- [ ] Verify every mutation audit record pins app ID/slug, installation ID/account, repository ID/name, selected scope, permission revision, event revision, key fingerprint, operation, result, task/change, and target SHA.
- [ ] Obtain a separate Sol/xhigh security review for credential/auth implementation and a separate independent security review of its PR.
- [ ] Request the serialized clean full local-ci slot, then require hosted green at the exact head before merge. This preparation has not requested or used that slot.

## References

- [Choosing permissions for a GitHub App](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app)
- [Registering from a GitHub App manifest](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest)
- [Webhook events and payloads](https://docs.github.com/en/webhooks/webhook-events-and-payloads)
- [Installing a GitHub App with selected repositories](https://docs.github.com/en/apps/using-github-apps/installing-your-own-github-app)
- [Authenticating as a GitHub App installation](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)
- [Creating and updating check runs](https://docs.github.com/en/rest/checks/runs)
- [Issue comments on issues and pull requests](https://docs.github.com/en/rest/issues/comments)
- [GitHub commit email attribution](https://docs.github.com/en/account-and-profile/concepts/email-addresses)
- [GitHub App logo/custom badge](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/creating-a-custom-badge-for-your-github-app)
