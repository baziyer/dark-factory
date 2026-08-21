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
  with explicit host, user, password, database, and `sslmode=require` (or
  stronger);
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET`: the byte-exact GitHub webhook
  secret, at least 32 bytes;
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION`: a non-secret bounded
  revision such as `maintainer-v1`; and
- `DARK_FACTORY_MAINTAINER_APP_ID`: the positive numeric App ID expected in
  `X-GitHub-Hook-Installation-Target-ID`.

There are no alternative variable names or ambient credential fallbacks. The
Marketplace owner `DATABASE_URL`, `DATABASE_URL_UNPOOLED`, `NEON_PROJECT_ID`,
and every `PG*` or `POSTGRES_*` alias must be absent from a deployment. Their
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

Provision a dedicated empty PostgreSQL 17-or-newer database before the first
deployment. Temporarily connect the Vercel Marketplace database so its owner
`DATABASE_URL` is available only to the operator command. From a trusted shell
in the linked `control-plane/` project, pipe the generated restricted URL
straight into Vercel's sensitive custom runtime setting:

```sh
vercel env run -e production -- cargo run --locked --bin provision-runtime \
  | vercel env add DARK_FACTORY_BROKER_DATABASE_URL production --sensitive --force
```

The provisioner reads the temporary owner `DATABASE_URL`, applies the checked
migration under an advisory lock, creates or safely rotates only the fixed
`dark_factory_broker_runtime` SQL role, resets its session defaults and
normalizes database/schema/table ACLs. It derives the PostgreSQL SCRAM verifier
client-side, so the generated password is never embedded in a server-loggable
`CREATE ROLE` or `ALTER ROLE` statement. PostgreSQL's required creator
management grant is accepted only when it names the current database owner,
has `ADMIN`, and has neither `SET` nor `INHERIT`. It then
connects through that derived URL to run the same readiness proof, and writes
exactly the proven restricted pooled URL to stdout. Database failures go to
fixed stderr without printing either URL; the pipe prevents terminal or file
storage of the generated credential.

Next disconnect the Marketplace integration before any deployment. Confirm
with `vercel env ls production` that `DATABASE_URL`,
`DATABASE_URL_UNPOOLED`, `NEON_PROJECT_ID`, and every `PG*`/`POSTGRES_*` alias
are gone. The custom `DARK_FACTORY_BROKER_DATABASE_URL` added by the pipe must
remain. Do not display or pull its value.

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
cargo fmt --all -- --check
cargo build --locked --release --bin broker --bin provision-runtime
```

The destructive authority/schema lane is intentionally opt-in because it
creates and rotates the fixed SQL role and deliberately corrupts then restores
the target tables. Point it only at a disposable, empty, TLS-enabled PostgreSQL
17+ database whose non-superuser owner has `CREATEROLE` and whose TCP
authentication checks password rotation:

```sh
DATABASE_URL='postgresql://owner:password@127.0.0.1:55441/disposable?sslmode=require' \
  cargo test --locked --test postgres_journal -- --ignored --exact \
  migrated_postgres_proves_readiness_concurrent_replay_and_conflict
```

The lane provisions twice, proves SCRAM authentication and that the first
password is dead, exercises restricted replay/conflict handling and forbidden
mutations, and verifies that excess ACLs, grant options, memberships, role
settings, ownership, nonpersistent or inherited tables, RLS, triggers, rules,
column/default drift, changed constraints, and missing primary keys all make
readiness fail closed. When the disposable server enables `log_statement=all`
and `logging_collector=on`, it also proves that neither generated password nor
the complete runtime URL appears in the captured PostgreSQL statement log.
