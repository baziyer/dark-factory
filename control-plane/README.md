# Dark Factory control plane (staging export)

This directory is the self-contained staging export for the future
`dark-factory-control-plane` service. It is not a `dark-factory` workspace
member and never links to or runs inside `factoryd`.

The current code proves one inert authority boundary. It is deployable as a
Rust Cloudflare Worker backed by SQLite Durable Objects, but this repository
does not claim that a Worker, route, domain, or GitHub App is live.

## Current surface

- `GET /healthz` proves only that the Worker can answer.
- `GET /readyz` returns 200 only when all three exact authority bindings are
  valid and the Durable Object binding, SQLite schema, and migration marker
  pass their audit.
- `POST /v1/github/maintainer/webhook` accepts only a bounded GitHub webhook.
  It verifies `X-Hub-Signature-256` over the exact body with HMAC-SHA-256,
  limits the body to 64 KiB, requires one value for every security header,
  requires an `integration` target, and binds the configured App ID.
- A valid `ping` is the only acknowledged event. Every other authenticated
  event is journalled as `policy_rejected` and returns 422. No payload can
  create a task, message, prompt, provider run, or GitHub mutation.
- The product webhook and operator/PWA namespaces have no routes.

Missing, empty, partial, or malformed authority produces the fixed inactive
router: liveness remains 200, readiness is 503, and the webhook route is not
installed. Storage failure returns 503 without acknowledging a delivery.
Responses never contain configuration or storage errors.

## Durable replay model

The Worker binds one `MaintainerDeliveryJournal` SQLite Durable Object
namespace. Object names include the configured App ID and one byte of the
SHA-256 digest of the GitHub delivery ID. This gives 256 deterministic shards
per App: every use of one delivery ID reaches the same strongly consistent
object without turning the whole service into a global singleton.

Each shard creates the reviewed SQLite schema on first use, records the exact
migration digest, and audits both stored table definitions and the migration
row before readiness or delivery work. The delivery ID is the primary key. An
insert uses `ON CONFLICT DO NOTHING`, then compares the complete stored replay
identity: hook ID, App target, target type, event, parsed action, body digest,
disposition, and webhook-secret revision. An exact replay returns its stored
result; any changed binding returns 409.

Durable Object serialization and the unique key make concurrent exact replay
collapse and conflicting-body behavior deterministic. SQLite state is not part
of a Worker code rollback; deployment and data rollback remain separate
operator decisions. Cloudflare documents SQLite Durable Objects as strongly
consistent and transactionally isolated, with encrypted storage and point-in-
time recovery. See the official [SQLite storage API], [Durable Object rules],
and [data security] documentation.

[SQLite storage API]: https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/
[Durable Object rules]: https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/
[data security]: https://developers.cloudflare.com/durable-objects/reference/data-security/

Native SQLite remains behind the non-default `development-sqlite` feature. It
is a fast causal model for the webhook contract and deliberately cannot make
readiness succeed. Production contains no Postgres, Neon, database URL,
runtime database role, Vercel adapter, or provider management API.

## Cloudflare configuration

`wrangler.toml` deliberately has:

- `workers_dev = false`;
- `preview_urls = false`;
- no route or custom domain;
- one capability-style Durable Object binding; and
- three required secret bindings.

The exact required bindings are:

- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET`: the byte-exact GitHub webhook
  secret, 32-1024 bytes;
- `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION`: a bounded lowercase
  revision such as `maintainer-v1`; and
- `DARK_FACTORY_MAINTAINER_APP_ID`: the positive numeric App ID expected in
  `X-GitHub-Hook-Installation-Target-ID`.

The revision and App ID are stored as secrets too. They are not confidential,
but treating all authority settings identically avoids a dashboard/source
split and makes a missing value block upload or deployment through Wrangler's
required-secret validation. There are no aliases or ambient fallbacks.

Never put values in a checked-in file, `.env*`, `.dev.vars*`, a provider
process, Dark Factory state, shell history, or the macOS Keychain. Cloudflare
secret values belong only in the platform secret binding. The Durable Object
binding embeds resource authority without exposing a resource credential to
the Worker. See Cloudflare's [binding] and [secret] documentation.

[binding]: https://developers.cloudflare.com/workers/runtime-apis/bindings/
[secret]: https://developers.cloudflare.com/workers/configuration/secrets/

`wrangler secret put` immediately creates and deploys a Worker version. It is
therefore not an acceptable staging command for this bootstrap. The reviewed
live runbook must use the versions API or another no-traffic staging mechanism,
prove the exact draft, and add a route only after the independent deployment
gate. Do not improvise the first live sequence from generic Wrangler examples.

## Local proof

Rust is pinned to 1.88 and Node 22 or newer is required. Wrangler, `worker`,
`worker-build`, and `wasm-bindgen` are pinned because their generated
interfaces must agree. Run:

```sh
./scripts/local-ci.sh
```

The authoritative gate performs:

1. Rust format and native all-feature Clippy;
2. the fixed inactive default tests;
3. the native SQLite replay, conflict, signature, header, body-limit, and
   policy tests;
4. production-target `wasm32-unknown-unknown` Clippy;
5. a release Worker build;
6. a non-uploading `wrangler deploy --dry-run`; and
7. a local `workerd` integration proof, including readiness, signed ping,
   exact concurrent replay, concurrent conflict, policy rejection, duplicate
   headers, the 64 KiB limit, persistence across runtime restart, absent future
   routes, and invalid-config inactivity.

The gate installs `worker-build` 0.8.5 under ignored `.tools/` and uses the
project-local locked Wrangler. It creates no `.env` file, calls no live API,
and deploys nothing.

## Activation gates

No live action is implied by merging this code. Activation requires a separate
operator-authorized run in this order:

1. local CI, release bundle, Wrangler dry-run, and independent adversarial
   `ALLOW` on one exact commit;
2. a disposable Cloudflare account or isolated Worker proof using distinct
   test bindings and no production GitHub App;
3. review of the exact no-traffic secret-staging and route/domain commands;
4. creation of the permanent `Dark Factory Maintainer` GitHub App and its
   exact webhook secret;
5. upload of one exact Worker version with required secrets while it has no
   public route;
6. verification of the version, Durable Object binding, and deployment
   configuration without reading secret values;
7. an explicit deployment/route decision, followed by `/healthz`, `/readyz`,
   signed ping, replay, conflict, and failure probes; and
8. only then, activation of the GitHub webhook.

Cloudflare account credentials, GitHub App credentials, route changes, and
deployments remain operator authority. Tests and review never exercise them.

## Future integrations

This hosting choice does not turn the control plane into a generic webhook
proxy. Each future source gets an explicit verifier, bounded schema, policy,
and storage namespace. A product delivery may at most become the existing
provider-neutral quarantine envelope; it never becomes executable work by
itself.

High-volume durable receipt can later hand off to Cloudflare Queues only after
the Durable Object commit. Queues are at-least-once, so the Durable Object
identity remains the deduplication authority. Long-running typed effects may
later use Workflows, but every external GitHub mutation still needs a durable
operation key and reconciliation state for ambiguous outcomes. Those are
separate reviewed increments, not abstractions added to this ping bootstrap.
