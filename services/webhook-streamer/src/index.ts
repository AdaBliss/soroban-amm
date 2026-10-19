/**
 * Webhook Streamer — entry point (issue #306)
 *
 * Starts an HTTP management API on PORT (default 3001) and a Soroban RPC poller
 * that fans contract events out to registered webhooks.
 *
 * Management API (every route except GET /health needs
 * `Authorization: Bearer $MANAGEMENT_API_TOKEN`):
 *   POST   /webhooks          – register a webhook
 *   DELETE /webhooks/:id      – unregister a webhook
 *   GET    /webhooks          – list all webhooks (secrets are never returned)
 *   GET    /health            – liveness probe (detail only when authenticated)
 *
 * Environment variables:
 *   SOROBAN_RPC_URL  – Soroban RPC base URL (required; must answer getHealth)
 *   HORIZON_URL      – deprecated alias for SOROBAN_RPC_URL (removed next release)
 *   CONTRACT_IDS     – comma-separated list of contract IDs to watch
 *   CONTRACT_KINDS   – comma-separated <contractId>:<kind> pairs (kind = a
 *                      directory under contracts/, e.g. amm); event fields are
 *                      named only for contracts listed here
 *   POLL_INTERVAL_MS – polling interval in ms (default: 5000)
 *   PORT             – HTTP port (default: 3001)
 *   MANAGEMENT_API_TOKEN          – bearer token for the management API (required)
 *   MANAGEMENT_API_INSECURE       – "true" to run without a token (local dev only)
 *   MANAGEMENT_MAX_BODY_BYTES     – request body limit (default: 65536)
 *   WEBHOOK_LEGACY_SECRET_HEADER  – "true" to also send the deprecated
 *                                   X-Webhook-Secret header for one release
 *   DATA_DIR         – directory for durable state (subscriptions, dead letters,
 *                      poller cursors). Unset = in-memory, lost on restart.
 *   SHUTDOWN_TIMEOUT_MS – how long SIGTERM/SIGINT waits for in-flight
 *                      deliveries before exiting (default: 10000)
 */

import { createServer, IncomingMessage, ServerResponse } from "node:http";
import { WebhookRegistry } from "./registry.js";
import { FileStateStore, MemoryStateStore, StateFileError, type StateStore } from "./storage.js";
import { InvalidWebhookUrlError } from "./url-validation.js";
import { WebhookDispatcher } from "./dispatcher.js";
import { DeadLetterQueue } from "./dead-letter.js";
import { CircuitBreaker } from "./circuit-breaker.js";
import { RpcPoller } from "./rpc-poller.js";
import { parseContractKinds } from "./event-schemas.js";
import {
  AuthConfigError,
  BodyTooLargeError,
  DEFAULT_MAX_BODY_BYTES,
  InvalidJsonError,
  ValidationError,
  isAuthorized,
  parseWebhookRegistration,
  readBody,
  resolveAuthConfig,
  type AuthConfig,
} from "./management-security.js";
import { toPublic, toPublicDeadLetter, type PoolEvent } from "./types.js";

const DEFAULT_SOROBAN_RPC_URL = "https://soroban-testnet.stellar.org";

const PORT = Number(process.env["PORT"] ?? 3001);
const SOROBAN_RPC_URL = process.env["SOROBAN_RPC_URL"];
const HORIZON_URL = process.env["HORIZON_URL"];

if (!SOROBAN_RPC_URL && HORIZON_URL) {
  console.warn(
    "[webhook-streamer] HORIZON_URL is deprecated; use SOROBAN_RPC_URL instead. " +
      "HORIZON_URL will be removed in a future release.",
  );
}

const RPC_URL = SOROBAN_RPC_URL ?? HORIZON_URL ?? DEFAULT_SOROBAN_RPC_URL;

if (!SOROBAN_RPC_URL && !HORIZON_URL) {
  console.warn(
    `[webhook-streamer] SOROBAN_RPC_URL is not set; defaulting to ${DEFAULT_SOROBAN_RPC_URL}.`,
  );
}

const CONTRACT_IDS = (process.env["CONTRACT_IDS"] ?? "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
let CONTRACT_KINDS: Record<string, string>;
try {
  CONTRACT_KINDS = parseContractKinds(process.env["CONTRACT_KINDS"]);
} catch (err) {
  console.error(`[webhook-streamer] ${err instanceof Error ? err.message : String(err)}`);
  process.exit(1);
}
const POLL_INTERVAL_MS = Number(process.env["POLL_INTERVAL_MS"] ?? 5_000);
const DELIVERY_TIMEOUT_MS = Number(process.env["DELIVERY_TIMEOUT_MS"] ?? 10_000);
const DELIVERY_CONCURRENCY = Number(process.env["DELIVERY_CONCURRENCY"] ?? 10);
const DEAD_LETTER_MAX = Number(process.env["DEAD_LETTER_MAX"] ?? 1_000);
const CIRCUIT_THRESHOLD = Number(process.env["CIRCUIT_FAILURE_THRESHOLD"] ?? 5);
const CIRCUIT_COOLDOWN_MS = Number(process.env["CIRCUIT_COOLDOWN_MS"] ?? 30_000);

const MAX_BODY_BYTES = Number(process.env["MANAGEMENT_MAX_BODY_BYTES"] ?? DEFAULT_MAX_BODY_BYTES);
const LEGACY_SECRET_HEADER = process.env["WEBHOOK_LEGACY_SECRET_HEADER"] === "true";
const DATA_DIR = process.env["DATA_DIR"];
const SHUTDOWN_TIMEOUT_MS = Number(process.env["SHUTDOWN_TIMEOUT_MS"] ?? 10_000);

// Refuse to expose the management routes without authentication unless the
// operator explicitly opted out.
let auth: AuthConfig;
try {
  auth = resolveAuthConfig(process.env);
} catch (err) {
  if (err instanceof AuthConfigError) {
    console.error(`[webhook-streamer] ${err.message}`);
    process.exit(1);
  }
  throw err;
}
if (!auth.token) {
  console.warn(
    "[webhook-streamer] MANAGEMENT_API_INSECURE=true: the management API has NO " +
      "authentication. Use this for local development only.",
  );
}
if (LEGACY_SECRET_HEADER) {
  console.warn(
    "[webhook-streamer] WEBHOOK_LEGACY_SECRET_HEADER=true: deliveries also carry the raw " +
      "secret in X-Webhook-Secret. This is deprecated and will be removed in the next release.",
  );
}

let store: StateStore;
try {
  store = DATA_DIR ? new FileStateStore(DATA_DIR) : new MemoryStateStore();
} catch (err) {
  if (err instanceof StateFileError) {
    console.error(`[webhook-streamer] ${err.message}`);
    process.exit(1);
  }
  throw err;
}
if (!DATA_DIR) {
  console.warn(
    "[webhook-streamer] DATA_DIR is not set: subscriptions, dead letters and poller cursors " +
      "are kept in memory and lost on restart. Set DATA_DIR to a persistent directory.",
  );
}

const defaultRegistry = new WebhookRegistry(store);

const dispatcher = new WebhookDispatcher(defaultRegistry, {
  legacySecretHeader: LEGACY_SECRET_HEADER,
  timeoutMs: DELIVERY_TIMEOUT_MS,
  concurrency: DELIVERY_CONCURRENCY,
  deadLetterQueue: new DeadLetterQueue(DEAD_LETTER_MAX, store),
  circuitBreaker: new CircuitBreaker({
    failureThreshold: CIRCUIT_THRESHOLD,
    cooldownMs: CIRCUIT_COOLDOWN_MS,
  }),
});

// ── Soroban RPC poller ──────────────────────────────────────────────────────

let poller: RpcPoller | undefined;

if (CONTRACT_IDS.length > 0) {
  const unknownKind = CONTRACT_IDS.filter((id) => !CONTRACT_KINDS[id]);
  if (unknownKind.length > 0) {
    console.warn(
      `[poller] no CONTRACT_KINDS entry for ${unknownKind.join(", ")}: their event payloads ` +
        "keep the generic { value: [...] } shape because the same event name means " +
        "different things in different contracts.",
    );
  }
  poller = new RpcPoller(
    {
      rpcUrl: RPC_URL,
      contractIds: CONTRACT_IDS,
      pollIntervalMs: POLL_INTERVAL_MS,
      contractKinds: CONTRACT_KINDS,
      store,
    },
    async (event: PoolEvent) => {
      const results = await dispatcher.dispatch(event);
      const failed = results.filter((r) => !r.success);
      if (failed.length > 0) {
        console.warn(`[dispatcher] ${failed.length} delivery failure(s) for event ${event.id}`);
      }
    },
  );

  // Fail fast at startup if the RPC endpoint does not answer getHealth.
  poller
    .checkHealth()
    .then(() => {
      poller!.start();
      console.log(
        `[poller] watching ${CONTRACT_IDS.length} contract(s) via ${RPC_URL}`,
      );
    })
    .catch((err: unknown) => {
      console.error(
        `[poller] RPC endpoint ${RPC_URL} failed getHealth; refusing to start:`,
        err,
      );
      process.exit(1);
    });
} else {
  console.warn("[poller] no CONTRACT_IDS set — poller not started");
}

// ── HTTP management API ─────────────────────────────────────────────────────

function json(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(payload);
}

async function handleRequest(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const url = req.url ?? "/";
  const method = req.method ?? "GET";

  const authorized = isAuthorized(req, auth);

  // GET /health — the liveness form is public so probes need no credential;
  // operational detail (including the poller's last error, which can carry
  // the RPC URL) is for authenticated callers only.
  if (method === "GET" && url === "/health") {
    if (!authorized && !auth.insecure) {
      return json(res, 200, { status: "ok" });
    }
    const pollerHealth = poller?.health() ?? { running: false };
    return json(res, 200, {
      status: poller?.lastError ? "degraded" : "ok",
      webhooks: defaultRegistry.size,
      poller:
        "lastError" in pollerHealth && pollerHealth.lastError
          ? { ...pollerHealth, lastError: pollerHealth.lastError.slice(0, 300) }
          : pollerHealth,
    });
  }

  if (!authorized) {
    res.setHeader("WWW-Authenticate", 'Bearer realm="webhook-streamer"');
    return json(res, 401, { error: "unauthorized" });
  }

  // GET /webhooks — includes each subscription's circuit state.
  if (method === "GET" && url === "/webhooks") {
    return json(
      res,
      200,
      defaultRegistry
        .list()
        .map((sub) => ({
          ...toPublic(sub),
          circuit: dispatcher.circuitBreaker.snapshot(sub.id),
        })),
    );
  }

  // GET /metrics
  if (method === "GET" && url === "/metrics") {
    return json(res, 200, dispatcher.metrics());
  }

  // GET /dead-letters
  if (method === "GET" && url === "/dead-letters") {
    return json(res, 200, dispatcher.deadLetters.list().map(toPublicDeadLetter));
  }

  // POST /dead-letters/:id/replay
  const replayMatch = url.match(/^\/dead-letters\/([^/]+)\/replay$/);
  if (method === "POST" && replayMatch) {
    const id = replayMatch[1];
    const result = await dispatcher.replay(id);
    if (!result) {
      return json(res, 404, { error: "dead letter not found" });
    }
    return json(res, result.success ? 200 : 502, { replayed: result.success, result });
  }

  // DELETE /dead-letters/:id
  const deadLetterMatch = url.match(/^\/dead-letters\/([^/]+)$/);
  if (method === "DELETE" && deadLetterMatch) {
    const removed = dispatcher.deadLetters.remove(deadLetterMatch[1]);
    return json(res, removed ? 200 : 404, { removed });
  }

  // POST /webhooks
  if (method === "POST" && url === "/webhooks") {
    let registration;
    try {
      registration = parseWebhookRegistration(await readBody(req, MAX_BODY_BYTES));
    } catch (err) {
      if (err instanceof BodyTooLargeError) {
        // Stop reading and drop the connection instead of draining the rest.
        res.setHeader("Connection", "close");
        res.once("finish", () => req.destroy());
        return json(res, 413, { error: err.message });
      }
      if (err instanceof InvalidJsonError) {
        return json(res, 400, { error: err.message });
      }
      if (err instanceof ValidationError) {
        return json(res, 400, { error: err.message, field: err.field });
      }
      throw err;
    }
    try {
      const sub = defaultRegistry.register(registration.url, {
        contractId: registration.contractId,
        eventType: registration.eventType,
        secret: registration.secret,
      });
      return json(res, 201, toPublic(sub));
    } catch (err) {
      if (err instanceof InvalidWebhookUrlError) {
        return json(res, 400, { error: err.message, field: "url" });
      }
      throw err;
    }
  }

  // DELETE /webhooks/:id
  const deleteMatch = url.match(/^\/webhooks\/([^/]+)$/);
  if (method === "DELETE" && deleteMatch) {
    const id = deleteMatch[1];
    const removed = defaultRegistry.unregister(id);
    if (removed) dispatcher.circuitBreaker.forget(id);
    return json(res, removed ? 200 : 404, { removed });
  }

  json(res, 404, { error: "not found" });
}

// Node ignores the promise a request listener returns, so a rejection escaping
// handleRequest would be unhandled and take the whole process down. Log it and
// fail only the request that caused it.
const server = createServer((req, res) => {
  handleRequest(req, res).catch((err: unknown) => {
    console.error(`[webhook-streamer] ${req.method ?? "GET"} ${req.url ?? "/"} failed:`, err);
    if (res.headersSent) {
      res.end();
    } else {
      json(res, 500, { error: "internal error" });
    }
  });
});

server.listen(PORT, () => {
  console.log(`[webhook-streamer] management API listening on port ${PORT}`);
});

// ── Graceful shutdown ───────────────────────────────────────────────────────

let shuttingDown = false;

/**
 * Stops polling, lets in-flight deliveries finish (up to SHUTDOWN_TIMEOUT_MS),
 * flushes state and exits 0. Work that doesn't finish keeps its old cursor, so
 * it is redelivered on the next start instead of lost.
 */
async function shutdown(signal: string): Promise<void> {
  if (shuttingDown) {
    console.warn(`[webhook-streamer] ${signal} again: exiting now`);
    process.exit(1);
  }
  shuttingDown = true;
  console.log(`[webhook-streamer] ${signal} received, draining (up to ${SHUTDOWN_TIMEOUT_MS} ms)`);

  const deadline = Date.now() + SHUTDOWN_TIMEOUT_MS;
  server.close();
  // Idle keep-alive connections would otherwise hold close() open.
  server.closeIdleConnections();

  const pollerDrained = poller ? await poller.stopAndDrain(SHUTDOWN_TIMEOUT_MS) : true;
  const deliveriesDrained = await dispatcher.drain(Math.max(0, deadline - Date.now()));
  store.flush();

  if (!pollerDrained || !deliveriesDrained) {
    console.warn(
      `[webhook-streamer] shutdown timed out with ${dispatcher.inFlight} delivery(ies) in flight; ` +
        "they will be redelivered after restart",
    );
  }
  console.log("[webhook-streamer] shutdown complete");
  process.exit(0);
}

for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.on(signal, () => {
    void shutdown(signal);
  });
}

export { server, dispatcher, defaultRegistry, poller };
