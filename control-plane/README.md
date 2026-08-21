# Dark Factory control plane (staging export)

This directory is a self-contained staging export for the sibling
`dark-factory-control-plane` service. It is not a `dark-factory` workspace
member and never links to or runs inside `factoryd`.

The current code proves only the inert maintainer webhook boundary. It includes
a deployable Vercel adapter, but no deployment or GitHub App is active:

- `GET /healthz` proves process liveness only. `GET /readyz` returns 200 only
  when all four exact production settings are valid and the required Postgres
  schema is reachable with `SELECT` and `INSERT` authority. SQLite never makes
  readiness succeed.
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

The default/release build uses a SQLx Postgres journal and does not link
SQLite. The SQLite adapter exists only behind the non-default
`development-sqlite` feature for local causal tests. The checked-in migration
is applied out of band; a function cold start never creates or changes schema.
Concurrent insert uses `ON CONFLICT DO NOTHING`, then compares every stored
binding in the same transaction to distinguish an exact replay from a delivery
ID conflict.

## Vercel bootstrap

The Vercel project root is this `control-plane/` directory. `api/broker.rs`
uses Vercel's official Rust runtime and Axum adapter; `vercel.json` sends all
requests through that one bounded router. The intended stable webhook URL is
`https://broker.darkfactory.build/v1/github/maintainer/webhook`; this repository
does not claim that domain or service is deployed.

Exactly these environment variables configure production:

- `DATABASE_URL`: a provider-pooled Postgres URL with explicit host, user,
  password, database, and `sslmode=require` (or stronger);
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET`: the byte-exact GitHub webhook
  secret, at least 32 bytes;
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION`: a non-secret bounded
  revision such as `maintainer-v1`; and
- `DARK_FACTORY_MAINTAINER_APP_ID`: the positive numeric App ID expected in
  `X-GitHub-Hook-Installation-Target-ID`.

There are no alternative variable names or ambient credential fallbacks. Any
`PG*` connection setting that could modify SQLx connection authority makes the
production entry point inactive; all connection authority must be explicit in
`DATABASE_URL`. A
missing, empty, malformed, or partial configuration produces the fixed inactive
router: liveness remains 200, readiness is 503, and the webhook route is not
installed. A syntactically valid configuration with an unavailable or
unmigrated database installs the route but returns 503 without acknowledging a
delivery. Responses never include configuration values or connection errors.

Production values belong in Vercel's production-scoped server-side settings;
the URL and webhook secret are sensitive. Preview deployments intentionally
receive none of the production database, secret, or App binding and therefore
remain inactive. A future preview integration test must use a separate
disposable App, secret, and database rather than borrowing production values.
Do not put values in `.env.example`, commit an `.env` file, pull them into an
agent worktree, or store them in a provider process or macOS Keychain.

Apply the checked-in migration as an explicit pre-deployment operation from a
trusted operator shell in the linked `control-plane/` Vercel project:

```sh
vercel env run -e production -- cargo run --locked --bin migrate
```

The runner reads `DATABASE_URL` only from the child environment, emits no URL
or database error, takes a transaction-scoped advisory lock, records the exact
migration digest, and rejects a changed or conflicting revision. Do not add
shell expansion that prints the value. This is an operator deployment command,
not authority for an agent task to fetch production environment variables.
Then create a new deployment so it sees the four settings. `/readyz` checks the
required columns, migration revision and digest, and runtime `SELECT`/`INSERT`
authority; it must return 200 before the webhook is activated in GitHub.
Activation proof must include signed ping persistence, exact concurrent replay
collapse, delivery-ID conflict, bad-signature rejection, and a database cut
returning 503 without acknowledgement. The App private key and every outbound
maintainer operation remain absent from this bootstrap.

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
cargo build --locked --release --bin broker
```
