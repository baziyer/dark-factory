# External integrations

Dark Factory accepts provider-neutral events on a loopback HTTP listener. An
external monitor, backlog feeder, or recovery tool can create queued tasks or
send durable agent messages. These writes use the same store and event ledger
as `factoryctl`.

Configure one `generic_v1` endpoint in the owner-only
`$DARK_FACTORY_HOME/webhooks.json` file:

```json
{
  "version": 1,
  "bind": "127.0.0.1:8787",
  "endpoints": [{
    "id": "monitor",
    "wireProfile": "generic_v1",
    "secretFile": "/absolute/path/to/monitor.secret"
  }]
}
```

The config and secret files must have mode `0600`. Send `POST
/monitor/events` with JSON bytes and an
`X-Dark-Factory-Signature: sha256=<lowercase hex>` header. The signature is
HMAC-SHA256 of the exact request bytes.

```json
{
  "version": 1,
  "eventId": "source-stable-id",
  "projectId": "factory",
  "type": "task",
  "data": {"title": "Check CI", "body": "Investigate the failed gate", "priority": 0}
}
```

`type` is `task`, `backlog`, or `message`. A message uses `agentId` and
`body`. The response status is `accepted`, `duplicate`, or `rejected`.
`eventId` is idempotent per endpoint and survives daemon restart. Unknown
projects or agents return `404` without preventing daemon startup. Bodies are
limited to 1 MiB; field limits are validated before commit.

`legacy_v1` remains available only for compatibility with existing endpoint
configurations. New integrations use `generic_v1`.
