# Dark Factory control plane (staging export)

This directory is a self-contained staging export for the sibling
`dark-factory-control-plane` service. It is not a `dark-factory` workspace
member and never links to or runs inside `factoryd`.

The current code proves only the inert maintainer webhook boundary:

- `GET /healthz` reports `development_only`; every product, maintainer, and
  operator surface remains inactive for deployment.
- `POST /v1/github/maintainer/webhook` verifies `X-Hub-Signature-256` over the
  exact body with HMAC-SHA-256, bounds the body to 64 KiB, and requires the
  GitHub delivery, event, hook, installation-target ID, and target-type
  headers.
- A replay binds every required header, the parsed action, body digest,
  disposition, and webhook-secret revision. An exact replay returns its stored
  result; reuse of a delivery ID with any different binding is a conflict.
- `ping` is acknowledged. Known `installation` and
  `installation_repositories` lifecycle actions are journalled as inert audit
  records. Other events and actions fail closed. No payload can create a task,
  message, prompt, provider run, or GitHub mutation.

The SQLite journal exists only for local causal tests and persistent-filesystem
development. It is not a Vercel storage design. A deployment must add the
reviewed Postgres journal adapter, configure an explicit secret revision, and
make readiness depend on both the secret and journal before the webhook is
reported active. Until then, do not configure this route as an active GitHub
App webhook.

The product webhook and operator/PWA namespaces are deliberately separate and
have no installed routes. Product deliveries may eventually create only the
existing provider-neutral quarantine envelope. The operator API may eventually
expose an authenticated, bounded `needs you`/status projection and commands; it
must never expose raw GitHub deliveries, App keys, or installation tokens.

Run the local proof with:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
```
