# Agent instructions

This file is the canonical guidance for the `dark-factory-control-plane`
repository. The service is a provider-neutral authority boundary for Dark
Factory's GitHub Apps. It is deployed separately from `factoryd`; neither
Codex nor Claude owns its credentials or its durable journal.

## Critical rules

1. Work on a branch in an isolated worktree, never directly on `main`. Open a
   pull request and require an adversarial review by someone other than the
   author before merge.
2. Keep runtime credentials outside agent processes and Dark Factory state.
   Store them only in the deployment platform's secret manager; never put a
   GitHub App private key, installation token, webhook secret, or database URL
   in a prompt, a checked-in file, an ambient `gh` session, or the macOS
   Keychain.
3. Expose typed, policy-checked operations only. Do not add a generic GitHub
   REST or GraphQL proxy, a shell-command surface, or a fallback to personal
   GitHub credentials.
4. Treat webhook authentication and replay handling as load-bearing. Verify
   the signature over the exact bounded request body, require exactly one of
   every security header, bind the configured App ID, and journal the full
   replay identity atomically before acknowledging a delivery.
5. Keep the bootstrap inert. Only a signed GitHub `ping` may be acknowledged
   until a separately reviewed typed event contract is implemented. An
   authenticated but unsupported event is a policy rejection, not work.
6. Keep liveness and readiness distinct. `/healthz` reports only that the
   process can answer; `/readyz` succeeds only when every dependency required
   by the active surface is configured and live. Missing or invalid authority
   must fail closed.
7. Keep the three planes separate: maintainer operations, product webhook
   intake, and the operator/PWA API. A product delivery can at most become a
   quarantined input envelope; it must never directly become a task, prompt,
   provider run, or GitHub mutation. Browser clients never receive raw
   deliveries or GitHub credentials.
8. Prefer deletion and direct implementations over speculative abstraction.
   Update tests and documentation in the same change when behavior changes.
9. Rust 1.88 is the pinned toolchain. Run `./scripts/local-ci.sh` before
   finishing; it is the authoritative local and hosted source gate.
10. Deployments and credential changes require an explicit task. Never send a
    provider prompt, mutate the Dark Factory live install, use ambient
    Keychain authentication, or deploy as a side effect of tests or review.
11. Report exactly which checks, deployments, migrations, and live probes did
    or did not run. Local proof is not hosted CI or production proof.
