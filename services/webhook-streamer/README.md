# webhook-streamer

Push-based event streaming microservice for the Soroban AMM (issue #306).

Subscribes to Soroban contract events via Stellar RPC's `getEvents` method
and fans them out to registered HTTP webhooks.  Eliminates the need for
integrators (trading bots, analytics dashboards, notification services) to
poll Stellar RPC directly.

## Quick start

```bash
# Install dependencies
npm install

# Start in dev mode
CONTRACT_IDS=CABC123,CDEF456 npm run dev

# Build and start
npm run build && npm start
```

## Environment variables

| Variable          | Default                                    | Description                              |
|-------------------|--------------------------------------------|------------------------------------------|
| `SOROBAN_RPC_URL` | `https://soroban-testnet.stellar.org`      | Stellar RPC base URL                     |
| `HORIZON_URL`     | _(deprecated)_                             | Legacy alias for `SOROBAN_RPC_URL`; accepted for one release with a startup deprecation warning. `SOROBAN_RPC_URL` takes precedence. |
| `CONTRACT_IDS`    | _(empty)_                                  | Comma-separated contract IDs to watch    |
| `START_LEDGER`    | `1`                                        | Ledger to start `getEvents` from on a contract's first poll (before it has a cursor). A live RPC endpoint only retains a recent window of ledgers, so the default of `1` will be rejected outside local/sandbox testing — set this to a recent ledger before pointing the service at testnet/mainnet. |
| `POLL_INTERVAL_MS`| `5000`                                     | Polling interval in milliseconds         |
| `PORT`            | `3001`                                     | Management API HTTP port                 |
| `DATA_DIR`        | _(unset: in memory)_                       | Directory for durable state: subscriptions, dead letters and poller cursors. See [Storage and restarts](#storage-and-restarts). Without it everything is lost on restart and the service warns at startup. |
| `SHUTDOWN_TIMEOUT_MS` | `10000`                                | How long SIGTERM/SIGINT waits for in-flight deliveries before exiting. |
| `MANAGEMENT_API_TOKEN` | _(required)_                           | Bearer token for the management API. The service refuses to start without it unless `MANAGEMENT_API_INSECURE=true`. Use a long random string. |
| `MANAGEMENT_API_INSECURE` | `false`                             | Set to `true` to run the management API with no authentication. Local development only; the service logs a warning at startup. |
| `MANAGEMENT_MAX_BODY_BYTES` | `65536`                           | Largest request body the management API accepts; bigger bodies get `413`. |
| `WEBHOOK_LEGACY_SECRET_HEADER` | `false`                        | Set to `true` to also send the deprecated `X-Webhook-Secret` header (the raw secret) for one release. See [Migrating from `X-Webhook-Secret`](#migrating-from-x-webhook-secret). |
| `WEBHOOK_ALLOW_PRIVATE_TARGETS` | `false`                       | Set to `true` to allow registering webhook URLs that point at loopback/link-local/private-range addresses. Only for local development against a same-host test receiver — leave unset in production, since it disables SSRF protection on `POST /webhooks`. |

## Management API

### Authentication

Every route except `GET /health` needs a bearer token:

```
Authorization: Bearer $MANAGEMENT_API_TOKEN
```

A missing or wrong token gets `401`. The token is compared in constant time.
`GET /health` is public so load balancers can probe it, but without a valid
token it returns only `{"status":"ok"}`; the webhook count and the poller's
details (including its last error) need the token.

The service will not start without `MANAGEMENT_API_TOKEN`. For local
development only, set `MANAGEMENT_API_INSECURE=true` to skip authentication.

Secrets are write-only. `POST /webhooks`, `GET /webhooks` and
`GET /dead-letters` return `"hasSecret": true` instead of the secret, and the
secret never appears in logs or error messages.

Request bodies are limited to `MANAGEMENT_MAX_BODY_BYTES` (64 KiB by default,
`413` above that). Bad input returns `400`: `{"error":"invalid JSON"}` when
the body cannot be parsed, or `{"error":"contractId must be a string","field":"contractId"}`
when a field is wrong.

### Register a webhook
```
POST /webhooks
Content-Type: application/json

{
  "url": "https://your-server.com/hook",
  "contractId": "CABC123",   // optional — omit to receive all contracts
  "eventType": "swap",       // optional — omit to receive all event types
  "secret": "my-secret"      // optional — used to sign deliveries, never returned
}
```

`url` must be an `http`/`https` URL and must not target loopback, link-local
(including the cloud metadata address `169.254.169.254`), or RFC1918
private-range addresses — requests that fail this check return `400`. See
`WEBHOOK_ALLOW_PRIVATE_TARGETS` above to relax this for local development.

### List webhooks
```
GET /webhooks
```

### Unregister a webhook
```
DELETE /webhooks/:id
```

### Health check
```
GET /health
```

## Event payload

Each webhook receives a `POST` with a JSON body:

```json
{
  "id": "0000000012345678-0000000001",
  "contractId": "CABC123",
  "eventType": "swap",
  "ledger": 1234567,
  "timestamp": "2026-06-01T12:00:00Z",
  "schemaVersion": 1,
  "payload": {
    "zeroForOne": true,
    "amountIn": 1000000,
    "amountOut": 998000
  }
}
```

`schemaVersion` is the version carried by the contract's versioned event
envelope (`(EVENT_SCHEMA_VERSION, payload)`); subscribers can use it to tell
schema versions apart.  Topics and values are decoded from base64 XDR
`ScVal`s via `@stellar/stellar-sdk`.  `bigint` values are encoded as decimal
strings in the JSON delivered to webhooks.

Supported event types: `swap`, `add_liquidity`, `remove_liquidity`,
`mint_pos`, `burn_pos`, `coll_fees`, `mint_1t`, `rng_ord`, `staked`,
`unstaked`, `claimed`.  Unknown topics still pass through with their decoded
name.

## Verifying deliveries

Every delivery carries an `X-Webhook-Id` header (the event id, stable across
retries; use it to deduplicate). If the subscription has a secret, it also
carries a signature:

```
X-Webhook-Signature: t=1767225600,v1=5257a869e7ecebeda32affa62cdca3fa51cad7e77a0e56ff536d0ce8e108d8bd
```

`t` is the unix time the request was signed and `v1` is the hex HMAC-SHA256,
keyed with your secret, of the string `"<t>.<raw request body>"`. To verify:

1. Parse `t` and `v1` from the header.
2. Recompute the HMAC over `t + "." + rawBody`, using the exact bytes you
   received (not re-serialised JSON).
3. Compare with `v1` in constant time.
4. Reject the request if `t` is more than 5 minutes from your clock. This is
   what stops someone replaying a captured request.

```ts
import { createHmac, timingSafeEqual } from "node:crypto";

export function verifyWebhook(secret: string, rawBody: string, header: string, toleranceSeconds = 300): boolean {
  const parts = Object.fromEntries(header.split(",").map((p) => p.split("=", 2)));
  const t = Number(parts.t);
  if (!Number.isInteger(t) || !/^[0-9a-f]{64}$/.test(parts.v1 ?? "")) return false;
  if (Math.abs(Date.now() / 1000 - t) > toleranceSeconds) return false;
  const expected = createHmac("sha256", secret).update(`${t}.${rawBody}`).digest();
  return timingSafeEqual(expected, Buffer.from(parts.v1, "hex"));
}
```

A retry or a dead-letter replay is signed again with a fresh timestamp, so
the tolerance window applies to each attempt, not to the original event.
`src/signing.ts` has the same logic (`verifySignature`) if you are working in
this repo.

### Migrating from `X-Webhook-Secret`

Earlier versions sent the secret itself in `X-Webhook-Secret`. That header is
gone by default: anyone who could observe one request learned the secret, and
a receiver could not tell a fresh event from a replay. To migrate:

1. Add signature verification to your receiver (above).
2. If you cannot ship that yet, set `WEBHOOK_LEGACY_SECRET_HEADER=true` on the
   streamer. It sends both headers for one release and logs a warning at
   startup.
3. Once your receiver verifies `X-Webhook-Signature`, unset the variable.
   Consider rotating secrets that were ever sent in the old header.

## Delivery guarantees

- **At least once.** An event is delivered one or more times, never zero. The
  poller saves each contract's cursor only after the events in that page have
  been handed to the dispatcher, so a crash or restart between delivery and
  the cursor write replays the page instead of skipping it. Receivers must
  deduplicate (see below).
- Every delivery carries `X-Webhook-Id`, the event id. It is the same on every
  retry, dead-letter replay and post-restart redelivery, so it is the key to
  deduplicate on.
- Up to 3 retries with exponential back-off (500 ms, 1 s, 2 s).
- Failed deliveries are logged but do not block other webhooks. Deliveries that
  exhaust their retries go to the dead-letter queue and can be replayed.
- A non-2xx response or an RPC `error` is logged at error level and surfaced
  on the health output; polling resumes on the next tick.

## Storage and restarts

Set `DATA_DIR` to a directory that survives restarts (a volume, in a
container). The service keeps one file there, `state.json`, holding:

- **subscriptions** (including their secrets, which are needed to sign
  deliveries and replays),
- **dead letters**, still replayable after a restart, and
- **cursors**, the position of each watched contract.

The file is replaced atomically on every change (write to a temp file, fsync,
rename), so a crash leaves the previous state or the new one, never a mix. It
is created readable by its owner only; keep the directory private. A file that
cannot be parsed stops the service at startup rather than silently resetting
to empty: fix it or move it aside to start fresh.

Without `DATA_DIR` all of this lives in memory and is lost on restart, and the
service logs a warning. It also means a restart resumes from `START_LEDGER`
and re-delivers events since then.

The service runs as a single instance. Two instances sharing one `DATA_DIR`
are not supported (no locking, no leader election).

### When RPC no longer has your ledgers

Stellar RPC only retains a recent window of ledgers. After downtime longer
than that window, the saved cursor (or `START_LEDGER`, which defaults to `1`)
is rejected with "must be within the ledger range". Instead of failing the
same request forever, the poller reads the retained range from the error (or
`getHealth`), resumes from the oldest retained ledger, and records what it
could not read:

- a log line: `[rpc-poller] gap contract=C... reason=cursor ledgers=151-249 resumedAt=250 ...`
- `GET /health` lists the latest gaps under `poller.gaps`, each with
  `contractId`, `reason` (`cursor` if a saved position expired, `start` if
  `START_LEDGER` was outside the window), `fromLedger`, `toLedger`,
  `resumedAt` and `detectedAt`.

Events in a gap are lost for good, since RPC no longer has them. Recover them
from another source if you need them. A `START_LEDGER` newer than the chain is
not treated as a gap; the poller keeps waiting.

### Shutdown

On SIGTERM or SIGINT the service stops polling, waits up to
`SHUTDOWN_TIMEOUT_MS` for in-flight deliveries to finish, saves state, and
exits 0. Anything that did not finish keeps its old cursor and is redelivered
on the next start. A second signal exits immediately.
