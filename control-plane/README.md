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

Provision the dedicated empty Neon PostgreSQL 17-or-newer database before the
first deployment. Keep Neon's optional Data API disabled. For production
project `withered-mouse-49434395`, create a temporary project-scoped API key;
never use a general personal or organization-wide key. Temporarily connect the
Vercel Marketplace database so its owner `DATABASE_URL` and `NEON_PROJECT_ID`
are available only to the operator command.

Build the provisioner before putting the key in any process environment:

```sh
cargo build --locked --release --features provision-runtime --bin provision-runtime
```

On macOS, then copy the temporary key to the clipboard and run this one shell
invocation from the trusted linked `control-plane/` project. The command text
contains no credential: only the provisioner child receives the key, the
generated database URL moves through the pipe without being printed, and an
exit trap clears the clipboard on success or failure.

```sh
DARK_FACTORY_VERCEL_GLOBAL_CONFIG=/absolute/path/to/isolated-vercel-config \
DARK_FACTORY_VERCEL_PROJECT_DIR=/absolute/path/to/linked-control-plane \
  ./scripts/provision-production.sh
```

Both paths are mandatory. The script rejects any `.env*` file in the linked
project and invokes both Vercel processes from an empty environment containing
only `PATH` and `TMPDIR`. It passes the isolated paths to Vercel's
`--global-config` and `--cwd` flags and refuses to fall back to ambient or
Keychain authentication.

Immediately revoke the temporary Neon key after every bootstrap attempt,
whether it succeeds or fails. Do not add it to Vercel, an `.env` file, shell
history, Dark Factory state, an agent prompt, or the macOS Keychain. A later
rotation or explicit recovery from an indeterminate reset uses a newly created
temporary project-scoped key and revokes it again after that one attempt.

The optional provisioner is not linked into the default broker binary. It
first rejects any owner URL whose host is not a strict `.neon.tech` name, and
every SQLx connection is upgraded to certificate and hostname verification.
Marketplace `channel_binding=require` input is tolerated but is not copied to
the result because SQLx 0.8 does not enforce it. The emitted runtime URL uses
`sslmode=verify-full` only.

Under one database transaction and advisory lock, the provisioner binds the
connection's `neon.project_id` and `neon.branch_id`, requires the project to
equal `NEON_PROJECT_ID`, applies the checked migration, creates or normalizes
only `dark_factory_broker_runtime` as `NOLOGIN PASSWORD NULL`, resets its
session defaults, and normalizes the database/schema/table ACLs. Neon's
provider-managed database grant to `neon_superuser` is revoked only after the
bound Neon connection proves the project's exact provider-role attributes and
the database owner's direct, non-admin, `cloud_admin`-granted membership in it;
the managed role must have Neon's observed `NULL` validity rather than the
runtime role's explicit `infinity` validity. The runtime readiness contract is
not widened to accept the provider grant.
PostgreSQL's required creator management grant is accepted only when it names
the current database owner, has `ADMIN`, and has neither `SET` nor `INHERIT`.

Only after that fail-closed commit does the operator-only adapter use the fixed
Neon API host. It first confirms that the exact role is visible on the bound
project and branch, issues exactly one password-reset `POST`, and polls the
typed operations at a bounded rate. Redirects are disabled and all requests
have bounded timeouts. The `POST` is never automatically retried: a transport
failure, server error, malformed success, or any later operation poll failure
or timeout is indeterminate because Neon may have changed the password without
returning a usable result. Rerun only as an explicit operator recovery
decision. After successful operations, a fresh owner pool re-audits the
exact role, membership, schema, tables, constraints, ACLs, default ACLs, and
migration record in one transaction while the role is still `NOLOGIN`. The
preparation pool is closed before the reset and activation uses a fresh owner
pool, so a provider-side connection reset cannot reuse a stale socket. Because
Neon's reset operation can restore the managed `neon_superuser` database ACL,
activation repeats the same exact provider-role gate and revoke before that
full audit. Only then does it enable `LOGIN` without putting a secret in SQL
and prove runtime readiness through the generated URL. Fixed stderr never
contains API bodies, keys, passwords, or URLs; stdout contains only the proven
restricted URL.

The schema audit pins every ordered column's type, default, and `attnotnull`
value directly. Explicit check and primary-key constraints are compared
exactly without mixing in PostgreSQL 18's `pg_constraint` `NOT NULL` rows.
Those rows have a separate version-aware exact audit: PostgreSQL 17 must have
none, while PostgreSQL 18-or-newer must have the exact validated, local,
non-inherited, single-column attribute-number set. This catches invalid or
drifted `NOT NULL` constraints without depending on their version-specific
catalog representation.

Next disconnect the Marketplace integration before any deployment. Confirm
with `vercel env ls production` that `DATABASE_URL`,
`DATABASE_URL_UNPOOLED`, `NEON_PROJECT_ID`, `DARK_FACTORY_NEON_API_KEY`, and
every `PG*`/`POSTGRES_*` alias are gone. The custom
`DARK_FACTORY_BROKER_DATABASE_URL` added by the pipe must remain. Do not
display or pull its value.

Never deploy while the owner integration is connected. Future migrations use
the same connect, provision/rotate, disconnect, replace-runtime-URL, deploy
sequence; rotating the role before replacing the deployed URL creates a brief
intentional maintenance interval. These are operator deployment commands, not
authority for an agent task to fetch or store production configuration.

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
cargo build --locked --release --features provision-runtime --bin provision-runtime
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
