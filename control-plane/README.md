# Dark Factory control plane (staging export)

This directory is a self-contained staging export for the sibling
`dark-factory-control-plane` service. It is not a `dark-factory` workspace
member and never links to or runs inside `factoryd`.

The current code proves only the inert maintainer webhook boundary. It includes
a deployable Vercel adapter, but no deployment or GitHub App is active:

- `GET /healthz` proves process liveness only. `GET /readyz` returns 200 only
  when all four exact production settings are valid and the required Postgres
  schema is reachable through the fixed unprivileged runtime role. Readiness
  proves the exact schema, conflict key, and separately required `SELECT` and
  `INSERT` rights while rejecting ownership, DDL, membership, or excess object
  authority. SQLite never makes readiness succeed.
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

- `DARK_FACTORY_BROKER_DATABASE_URL`: the provisioned restricted runtime URL
  with explicit Neon host, user, password, database, and
  `sslmode=verify-full`;
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET`: the byte-exact GitHub webhook
  secret, at least 32 bytes;
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION`: a non-secret bounded
  revision such as `maintainer-v1`; and
- `DARK_FACTORY_MAINTAINER_APP_ID`: the positive numeric App ID expected in
  `X-GitHub-Hook-Installation-Target-ID`.

There are no alternative variable names or ambient credential fallbacks. The
Marketplace owner `DATABASE_URL`, `DATABASE_URL_UNPOOLED`, `NEON_PROJECT_ID`,
the operator-only `DARK_FACTORY_NEON_API_KEY`, and every `PG*` or `POSTGRES_*`
alias must be absent from a deployment. Their
presence proves that the owner integration is still attached and selects the
fixed inactive router; the function never reads them. A
missing, empty, malformed, or partial configuration produces the fixed inactive
router: liveness remains 200, readiness is 503, and the webhook route is not
installed. A syntactically valid configuration with an unavailable or
unmigrated database installs the route but returns 503 without acknowledging a
delivery. Responses never include configuration values or connection errors.

Production values belong in Vercel's production-scoped server-side settings;
the URL and webhook secret are sensitive. The function also requires Vercel's
system `VERCEL_ENV` metadata to equal `production`; missing, preview,
development, and unknown values select the fixed inactive router even if a
credential was accidentally scoped there. Preview deployments intentionally
receive none of the production database, secret, or App binding. A future
preview integration test must use a separate disposable App, secret, database,
and separately reviewed activation contract rather than borrowing production
values. Do not put values in `.env.example`, commit an `.env` file, pull them
into an agent worktree, or store them in a provider process or macOS Keychain.

The recovery bootstrap expects the checked migration, exact ACLs, and fixed
`dark_factory_broker_runtime` role to exist already, with the role `NOLOGIN`.
It deliberately cannot create or normalize a role, apply a migration, or put a
password in SQL. A fresh database bootstrap or future migration is a separate
reviewed operation. Keep Neon's optional Data API disabled.

Build the optional bootstrap binary before putting a Neon key in any process:

```sh
cargo build --locked --release --features provision-runtime --bin runtime-bootstrap
```

For production project `withered-mouse-49434395`, create one temporary
project-scoped API key only after the code and release binary have passed
independent review. Never use a personal or organization-wide key. Keep the
Vercel Marketplace integration connected so its owner `DATABASE_URL` and
`NEON_PROJECT_ID` keep every broker deployment inactive. Copy the temporary
key to the macOS clipboard, then stage the existing restricted credential:

```sh
DARK_FACTORY_VERCEL_GLOBAL_CONFIG=/absolute/path/to/isolated-vercel-config \
DARK_FACTORY_VERCEL_PROJECT_DIR=/absolute/path/to/linked-control-plane \
  ./scripts/bootstrap-production.sh stage
```

The stage transaction takes an advisory lock and proves the exact Neon project
and branch, provider role, runtime role with `NOLOGIN`, membership, schema,
tables, constraints, migration record, default privileges, and ACLs. It then
confirms the exact role through Neon and calls the typed
`GET .../roles/dark_factory_broker_runtime/reveal_password` endpoint. A `200`
password is used without a reset. HTTP `412` is the only unavailable response;
the command fails with a fixed message and does not reset. Every other response
also fails closed.

Only after that exact `412`, the explicit fallback permits one reset:

```sh
DARK_FACTORY_VERCEL_GLOBAL_CONFIG=/absolute/path/to/isolated-vercel-config \
DARK_FACTORY_VERCEL_PROJECT_DIR=/absolute/path/to/linked-control-plane \
  ./scripts/bootstrap-production.sh stage --reset-if-unavailable
```

The flag does not force a reset: reveal still runs first. If reveal remains
unavailable, exactly one non-idempotent reset `POST` is sent and never retried.
As soon as its exact role and password response is accepted, the restricted URL
moves directly through a pipe into the sensitive production-scoped
`DARK_FACTORY_BROKER_DATABASE_URL`; reset operation polling cannot discard the
only returned password. Stage never enables `LOGIN`, disconnects the owner
integration, or deploys. A reset transport failure, any non-success response,
or malformed success is indeterminate and requires an operator decision; it is
never hidden behind another automatic reset.

Both isolation paths are mandatory absolute directories. The helper rejects
any `.env*` file in the linked project and invokes every Vercel command from an
empty environment containing only `PATH` and `TMPDIR`, with explicit
`--global-config` and `--cwd`. Only the bootstrap child sees the clipboard key.
An exit trap clears the clipboard on success or failure and always reminds the
operator to revoke the key. Revoke it immediately after every stage attempt;
the helper cannot safely revoke a lost key without adding broader Neon
authority. Never put the key in Vercel, a file, shell history, Dark Factory
state, an agent prompt, an ambient provider CLI, or the macOS Keychain.

After the sensitive URL is confirmed present by name only, activate without a
Neon key:

```sh
DARK_FACTORY_VERCEL_GLOBAL_CONFIG=/absolute/path/to/isolated-vercel-config \
DARK_FACTORY_VERCEL_PROJECT_DIR=/absolute/path/to/linked-control-plane \
  ./scripts/bootstrap-production.sh activate
```

Activation obtains both URLs in memory through isolated `vercel env run`; it
never prints, pulls, or writes either value. It rejects a restricted URL whose
scheme, Neon host, port, database, fixed role, or exact `sslmode=verify-full`
does not match the owner URL. Each bounded attempt creates a fresh owner pool
whose connections use certificate and hostname verification, validates the
exact provider identity, removes any provider database ACL restored by Neon,
and repeats the full role/schema/ACL/migration audit. Only then does it enable
`LOGIN`. It accepts an already-`LOGIN` exact role so a crash after commit is
resumable, then verifies the restricted connection. Nine attempts span 47
seconds; a later activation-only rerun is safe and uses the Vercel-stored URL.
Any failure leaves the URL stored and the broker inactive because the owner
integration remains connected.

Marketplace `channel_binding=require` input is tolerated but is not copied to
the restricted URL because SQLx 0.8 does not enforce it. Every SQLx connection
is upgraded independently to `VerifyFull`. Fixed stderr never contains API
bodies, keys, passwords, or URLs.

The schema audit pins every ordered column's type, default, and `attnotnull`
value directly. Explicit check and primary-key constraints are compared
exactly without mixing in PostgreSQL 18's `pg_constraint` `NOT NULL` rows.
Those rows have a separate version-aware exact audit: PostgreSQL 17 must have
none, while PostgreSQL 18-or-newer must have the exact validated, local,
non-inherited, single-column attribute-number set. This catches invalid or
drifted `NOT NULL` constraints without depending on their version-specific
catalog representation.

Only after independent adversarial `ALLOW`, disconnect the Marketplace
integration before any deployment. Confirm
with `vercel env ls production` that `DATABASE_URL`,
`DATABASE_URL_UNPOOLED`, `NEON_PROJECT_ID`, `DARK_FACTORY_NEON_API_KEY`, and
every `PG*`/`POSTGRES_*` alias are gone. The custom
`DARK_FACTORY_BROKER_DATABASE_URL` added by the pipe must remain. Do not
display or pull its value.

Never deploy while the owner integration is connected. Future migrations need
a separately reviewed connect, migrate, recover/rotate, activate, disconnect,
and deploy sequence. These are operator deployment commands, not authority for
an unrelated agent task to fetch or store production configuration.

Then create a production deployment with the four runtime settings.
`/readyz` performs a read-only verification of the exact migration digest,
ordered columns, named constraints and valid nonpartial primary keys for both
tables, persistent non-inherited heap identity, restricted role and session
settings, and catalog-exact ACL allowlists. The disposable Postgres gate
causally proves the corresponding `ON CONFLICT` behavior. Readiness must return
200 before the webhook is activated in GitHub.
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
cargo test --locked --features provision-runtime
cargo fmt --all -- --check
cargo build --locked --release --bin broker
cargo build --locked --release --features provision-runtime --bin runtime-bootstrap
```

The destructive authority/schema lane is intentionally opt-in because it
deliberately corrupts then restores the target tables and role authority. It
does not provision a role or call Neon. Point it only at a disposable,
TLS-enabled PostgreSQL 17+ database whose non-superuser owner has `CREATEROLE`,
and supply a separately prepared restricted runtime URL for that same database:

```sh
DATABASE_URL='postgresql://owner:password@127.0.0.1:55441/disposable?sslmode=require' \
DARK_FACTORY_TEST_RUNTIME_DATABASE_URL='postgresql://dark_factory_broker_runtime:test-password@127.0.0.1:55441/disposable?sslmode=verify-full' \
  cargo test --locked --test postgres_journal -- --ignored --exact \
  migrated_postgres_proves_readiness_concurrent_replay_and_conflict
```

The lane exercises restricted replay/conflict handling and forbidden
mutations, and verifies that excess ACLs, grant options, memberships, role
settings, ownership, nonpersistent or inherited tables, RLS, triggers, rules,
column/default drift, changed constraints, and missing primary keys all make
readiness fail closed.
