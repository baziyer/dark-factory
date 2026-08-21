# Dark Factory control plane (staging export)

This directory is a self-contained staging export for the sibling
`dark-factory-control-plane` service. It is not a `dark-factory` workspace
member and never links to or runs inside `factoryd`.

The current code proves only the inert maintainer webhook boundary:

- `GET /healthz` proves process liveness only. `GET /readyz` returns 503 and
  reports every product, maintainer, and operator surface inactive.
- `POST /v1/github/maintainer/webhook` verifies `X-Hub-Signature-256` over the
  exact body with HMAC-SHA-256, bounds the body to 64 KiB, and requires the
  GitHub delivery, event, hook, installation-target ID, and target-type
  headers. Each required header must occur exactly once, the target type must
  be `integration`, and the target ID must match the configured App ID.
- A replay binds every required header, the parsed action, body digest,
  disposition, and webhook-secret revision. An exact replay returns its stored
  result; reuse of a delivery ID with any different binding is a conflict.
- A valid `ping` is the only acknowledged event. Every installation,
  lifecycle, and other event is journalled as `policy_rejected` and returns
  422. No payload can create a task, message, prompt, provider run, or GitHub
  mutation.

The SQLite journal exists only behind the non-default `development-sqlite`
feature for local causal tests. The default/release build does not link SQLite
and installs no webhook route. SQLite is not a Vercel storage design. A
deployment must add the
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
cargo test --locked --features development-sqlite
cargo clippy --locked --all-targets --features development-sqlite -- -D warnings
cargo fmt --all -- --check
```
