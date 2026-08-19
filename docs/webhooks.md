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
    "secretFile": "/absolute/path/to/monitor.secret",
    "projectId": "factory"
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
  "timestampMs": 1787054400000,
  "projectId": "factory",
  "type": "task",
  "data": {"title": "Check CI", "body": "Investigate the failed gate", "priority": 0}
}
```

`projectId` is required in the endpoint configuration and is the daemon-owned
scope for every signed event; the body cannot select another project. `type`
is `task`, `backlog`, or `message`. A message uses `agentId` and `body`.
`timestampMs` is Unix time in milliseconds and is part of the signed
request. Dark Factory accepts timestamps from five minutes in the past through
30 seconds in the future, inclusive. Retry with the exact same signed bytes
inside that window.

The response status is `accepted`, `duplicate`, or `rejected`. `eventId` is
idempotent per endpoint and survives daemon restart. It is bound to the SHA-256
digest of the complete authenticated request bytes. Reusing an ID with changed
bytes returns `409` and `idempotency_mismatch`; it is never reported as a
duplicate. Unknown agents within the configured project return `404`; a body
that names a different project is rejected as unauthorized without preventing
daemon startup.

HTTP bodies are limited to 1 MiB. Task and backlog titles use the same contract
as `factoryctl`: surrounding whitespace is removed, the result must contain
1–240 bytes, and the body is limited to 65,536 bytes. Message bodies use the
same 65,536-byte bound as `factoryctl agent message`.

`legacy_v1` remains available only for compatibility with existing endpoint
configurations. New integrations use `generic_v1`.
