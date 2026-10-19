/**
 * Management API security tests (issue #1123): authentication, secret
 * redaction, request signing and request validation.
 * Run with: node --test dist/management.test.js
 */

import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { createServer, request, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { after, before, describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import { CircuitBreaker } from "./circuit-breaker.js";
import { DeadLetterQueue } from "./dead-letter.js";
import { WebhookDispatcher } from "./dispatcher.js";
import {
  AuthConfigError,
  BodyTooLargeError,
  InvalidJsonError,
  MAX_SECRET_LENGTH,
  ValidationError,
  isAuthorized,
  parseWebhookRegistration,
  readBody,
  resolveAuthConfig,
  safeEqual,
} from "./management-security.js";
import { WebhookRegistry } from "./registry.js";
import { signPayload, verifySignature } from "./signing.js";
import { toPublic, toPublicDeadLetter, type PoolEvent } from "./types.js";

process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] = "true";

const SECRET = "s3cret-value-for-tests";
const EVENT: PoolEvent = {
  id: "evt-1",
  contractId: "CABC123",
  eventType: "swap",
  ledger: 1234,
  timestamp: "2026-06-01T00:00:00Z",
  payload: { amountIn: 1000, amountOut: 990 },
};

// ── Signing ──────────────────────────────────────────────────────────────

describe("signing", () => {
  const body = JSON.stringify(EVENT);
  const now = 1_800_000_000;

  it("produces t=<unix>,v1=<hex> and verifies", () => {
    const header = signPayload(SECRET, body, now);
    assert.match(header, /^t=1800000000,v1=[0-9a-f]{64}$/);
    assert.deepEqual(verifySignature({ secret: SECRET, body, header, now }), { valid: true });
  });

  it("rejects a tampered body", () => {
    const header = signPayload(SECRET, body, now);
    const result = verifySignature({ secret: SECRET, body: body + " ", header, now });
    assert.deepEqual(result, { valid: false, reason: "signature_mismatch" });
  });

  it("rejects the wrong secret", () => {
    const header = signPayload(SECRET, body, now);
    const result = verifySignature({ secret: "other", body, header, now });
    assert.deepEqual(result, { valid: false, reason: "signature_mismatch" });
  });

  it("rejects a timestamp outside the tolerance, old or in the future", () => {
    const header = signPayload(SECRET, body, now);
    for (const later of [now + 301, now - 301]) {
      const result = verifySignature({ secret: SECRET, body, header, now: later });
      assert.deepEqual(result, { valid: false, reason: "timestamp_out_of_tolerance" });
    }
    assert.equal(verifySignature({ secret: SECRET, body, header, now: now + 300 }).valid, true);
  });

  it("rejects a header whose timestamp was altered to dodge the window", () => {
    const header = signPayload(SECRET, body, now - 10_000);
    const forged = header.replace(`t=${now - 10_000}`, `t=${now}`);
    const result = verifySignature({ secret: SECRET, body, header: forged, now });
    assert.deepEqual(result, { valid: false, reason: "signature_mismatch" });
  });

  it("rejects missing and malformed headers", () => {
    for (const header of [undefined, "", "garbage", "t=1,v1=zz", `t=abc,v1=${"0".repeat(64)}`]) {
      const result = verifySignature({ secret: SECRET, body, header, now });
      assert.deepEqual(result, { valid: false, reason: "malformed_header" });
    }
  });
});

// ── Delivery headers ─────────────────────────────────────────────────────

interface Receiver {
  url: string;
  requests: { headers: IncomingMessage["headers"]; body: string }[];
  close(): Promise<void>;
}

async function startReceiver(status = 200): Promise<Receiver> {
  const requests: Receiver["requests"] = [];
  const server: Server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      requests.push({ headers: req.headers, body });
      res.writeHead(status);
      res.end("ok");
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}/hook`,
    requests,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

function dispatcherFor(registry: WebhookRegistry, extra: Partial<{ legacySecretHeader: boolean; deadLetterQueue: DeadLetterQueue }> = {}) {
  return new WebhookDispatcher(registry, {
    timeoutMs: 500,
    maxRetries: 0,
    sleep: () => Promise.resolve(),
    circuitBreaker: new CircuitBreaker({ failureThreshold: 100 }),
    deadLetterQueue: new DeadLetterQueue(10),
    ...extra,
  });
}

describe("delivery headers", () => {
  it("signs the delivered body so the reference verifier accepts it", async () => {
    const receiver = await startReceiver();
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url, { secret: SECRET });
      await dispatcherFor(registry).dispatch(EVENT);

      const [got] = receiver.requests;
      assert.ok(got);
      const header = got.headers["x-webhook-signature"] as string;
      assert.deepEqual(verifySignature({ secret: SECRET, body: got.body, header }), { valid: true });
      assert.equal(got.headers["x-webhook-id"], EVENT.id);
    } finally {
      await receiver.close();
    }
  });

  it("never puts the secret on the wire by default", async () => {
    const receiver = await startReceiver();
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url, { secret: SECRET });
      await dispatcherFor(registry).dispatch(EVENT);

      const [got] = receiver.requests;
      assert.ok(got);
      assert.equal(got.headers["x-webhook-secret"], undefined);
      assert.ok(!JSON.stringify(got.headers).includes(SECRET));
    } finally {
      await receiver.close();
    }
  });

  it("sends the legacy X-Webhook-Secret header only when enabled", async () => {
    const receiver = await startReceiver();
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url, { secret: SECRET });
      await dispatcherFor(registry, { legacySecretHeader: true }).dispatch(EVENT);

      const [got] = receiver.requests;
      assert.ok(got);
      assert.equal(got.headers["x-webhook-secret"], SECRET);
      assert.ok(got.headers["x-webhook-signature"]);
    } finally {
      await receiver.close();
    }
  });

  it("a dead-letter replay is signed with a fresh timestamp", async () => {
    const receiver = await startReceiver(500);
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url, { secret: SECRET });
      const dispatcher = dispatcherFor(registry);
      await dispatcher.dispatch(EVENT);
      const [entry] = dispatcher.deadLetters.list();
      assert.ok(entry);
      await dispatcher.replay(entry.id);

      assert.equal(receiver.requests.length, 2);
      for (const req of receiver.requests) {
        const header = req.headers["x-webhook-signature"] as string;
        assert.equal(verifySignature({ secret: SECRET, body: req.body, header }).valid, true);
      }
    } finally {
      await receiver.close();
    }
  });
});

// ── Secret redaction ─────────────────────────────────────────────────────

describe("secret redaction", () => {
  it("toPublic drops the secret and reports hasSecret", () => {
    const registry = new WebhookRegistry();
    const withSecret = registry.register("https://example.com/a", { secret: SECRET });
    const without = registry.register("https://example.com/b");
    assert.equal(JSON.stringify(toPublic(withSecret)).includes(SECRET), false);
    assert.equal(toPublic(withSecret).hasSecret, true);
    assert.equal(toPublic(without).hasSecret, false);
    assert.equal("secret" in toPublic(withSecret), false);
  });

  it("a dead-letter listing never contains the subscription secret", async () => {
    const receiver = await startReceiver(500);
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url, { secret: SECRET });
      const dispatcher = dispatcherFor(registry);
      const results = await dispatcher.dispatch(EVENT);

      const listing = JSON.stringify(dispatcher.deadLetters.list().map(toPublicDeadLetter));
      assert.ok(listing.includes("evt-1"), "the listing should still describe the dead letter");
      assert.equal(listing.includes(SECRET), false);
      assert.equal(JSON.stringify(results).includes(SECRET), false);
      assert.equal(JSON.stringify(dispatcher.metrics()).includes(SECRET), false);
    } finally {
      await receiver.close();
    }
  });
});

// ── Auth primitives ──────────────────────────────────────────────────────

describe("auth primitives", () => {
  const withAuth = (value?: string) =>
    ({ headers: value === undefined ? {} : { authorization: value } }) as IncomingMessage;

  it("startup config refuses to run without a token unless insecure is set", () => {
    assert.throws(() => resolveAuthConfig({}), AuthConfigError);
    assert.throws(() => resolveAuthConfig({ MANAGEMENT_API_TOKEN: "   " }), AuthConfigError);
    assert.deepEqual(resolveAuthConfig({ MANAGEMENT_API_TOKEN: "tok" }), { token: "tok", insecure: false });
    assert.equal(resolveAuthConfig({ MANAGEMENT_API_INSECURE: "true" }).insecure, true);
    assert.throws(() => resolveAuthConfig({ MANAGEMENT_API_INSECURE: "yes" }), AuthConfigError);
  });

  it("accepts only the exact bearer token", () => {
    const config = { token: "tok-123", insecure: false };
    assert.equal(isAuthorized(withAuth("Bearer tok-123"), config), true);
    assert.equal(isAuthorized(withAuth("bearer tok-123"), config), true);
    for (const bad of [undefined, "", "tok-123", "Bearer ", "Bearer tok-12", "Bearer tok-1234", "Basic tok-123", "Bearer tok-123 extra"]) {
      assert.equal(isAuthorized(withAuth(bad), config), false, String(bad));
    }
  });

  it("compares strings of different lengths without throwing", () => {
    assert.equal(safeEqual("a", "a"), true);
    assert.equal(safeEqual("a", "abc"), false);
    assert.equal(safeEqual("", "x"), false);
  });
});

// ── Request validation ───────────────────────────────────────────────────

describe("parseWebhookRegistration", () => {
  const field = (raw: string) => {
    try {
      parseWebhookRegistration(raw);
    } catch (err) {
      assert.ok(err instanceof ValidationError, `expected ValidationError, got ${String(err)}`);
      return err;
    }
    assert.fail("expected a ValidationError");
  };

  it("reports invalid JSON separately from invalid fields", () => {
    assert.throws(() => parseWebhookRegistration("{not json"), InvalidJsonError);
    assert.throws(() => parseWebhookRegistration('{"url": 5}'), ValidationError);
  });

  it("rejects a non-string contractId, naming the field", () => {
    const err = field('{"url":"https://example.com","contractId":42}');
    assert.equal(err.field, "contractId");
    assert.match(err.message, /contractId must be a string/);
  });

  it("rejects a missing, empty or non-string url", () => {
    assert.equal(field("{}").field, "url");
    assert.match(field('{"url":""}').message, /url must not be empty/);
    assert.match(field('{"url":"   "}').message, /url must not be empty/);
    assert.match(field('{"url":["x"]}').message, /url must be a string/);
  });

  it("rejects an oversized or empty secret", () => {
    const big = JSON.stringify({ url: "https://example.com", secret: "x".repeat(MAX_SECRET_LENGTH + 1) });
    assert.equal(field(big).field, "secret");
    assert.equal(field('{"url":"https://example.com","secret":""}').field, "secret");
  });

  it("rejects a body that is not a JSON object", () => {
    for (const raw of ["[]", "null", '"str"', "7"]) assert.equal(field(raw).field, "body");
  });

  it("accepts a valid registration", () => {
    assert.deepEqual(
      parseWebhookRegistration('{"url":"https://example.com/h","contractId":"C1","eventType":"swap","secret":"s"}'),
      { url: "https://example.com/h", contractId: "C1", eventType: "swap", secret: "s" },
    );
  });
});

// ── Bounded body reader ──────────────────────────────────────────────────

describe("readBody", () => {
  async function withServer(limit: number, send: (port: number) => Promise<void>) {
    let outcome: unknown = "";
    let received = 0;
    const server = createServer((req, res) => {
      req.on("data", (c: Buffer) => (received += c.length));
      readBody(req, limit).then(
        (body) => {
          outcome = body;
          res.end("ok");
        },
        (err: Error) => {
          outcome = err;
          res.writeHead(413);
          res.end();
        },
      );
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    try {
      await send((server.address() as AddressInfo).port);
    } finally {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
    return { outcome, received };
  }

  function post(port: number, payload: Buffer, headers: Record<string, string> = {}) {
    return new Promise<void>((resolve) => {
      const req = request({ host: "127.0.0.1", port, method: "POST", headers }, (res) => {
        res.resume();
        res.on("end", () => resolve());
      });
      req.on("error", () => resolve());
      req.end(payload);
    });
  }

  it("returns a body within the limit", async () => {
    const { outcome } = await withServer(1024, (port) => post(port, Buffer.from("hello")));
    assert.equal(outcome, "hello");
  });

  it("rejects on Content-Length alone, before buffering anything", async () => {
    const { outcome } = await withServer(10, (port) => post(port, Buffer.alloc(5000, 97)));
    assert.ok(outcome instanceof BodyTooLargeError);
  });

  it("stops buffering a chunked body that passes the limit", async () => {
    const limit = 1024;
    const { outcome } = await withServer(limit, (port) =>
      new Promise<void>((resolve) => {
        const req = request(
          { host: "127.0.0.1", port, method: "POST", headers: { "transfer-encoding": "chunked" } },
          (res) => {
            res.resume();
            res.on("end", () => resolve());
          },
        );
        req.on("error", () => resolve());
        for (let i = 0; i < 64; i++) req.write(Buffer.alloc(512, 98));
        req.end();
      }),
    );
    assert.ok(outcome instanceof BodyTooLargeError);
  });
});

// ── The running service ──────────────────────────────────────────────────

const INDEX_JS = fileURLToPath(new URL("./index.js", import.meta.url));

async function freePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  await new Promise<void>((resolve) => server.close(() => resolve()));
  return port;
}

interface Service {
  port: number;
  child: ChildProcess;
  output: () => string;
  stop(): Promise<void>;
}

function startService(env: Record<string, string>, waitFor = /management API listening/): Promise<Service> {
  return freePort().then(
    (port) =>
      new Promise<Service>((resolve, reject) => {
        const child = spawn(process.execPath, [INDEX_JS], {
          env: { PATH: process.env["PATH"] ?? "", WEBHOOK_ALLOW_PRIVATE_TARGETS: "true", ...env, PORT: String(port) },
          stdio: ["ignore", "pipe", "pipe"],
        });
        let output = "";
        const onData = (chunk: Buffer) => {
          output += chunk.toString();
          if (waitFor.test(output)) {
            resolve({
              port,
              child,
              output: () => output,
              stop: () =>
                new Promise<void>((done) => {
                  child.once("exit", () => done());
                  child.kill("SIGKILL");
                }),
            });
          }
        };
        child.stdout?.on("data", onData);
        child.stderr?.on("data", onData);
        child.once("error", reject);
        child.once("exit", () => reject(new Error(`service exited early:\n${output}`)));
      }),
  );
}

function exitOf(env: Record<string, string>): Promise<{ code: number | null; output: string }> {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [INDEX_JS], {
      env: { PATH: process.env["PATH"] ?? "", PORT: "0", ...env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let output = "";
    child.stdout?.on("data", (c: Buffer) => (output += c.toString()));
    child.stderr?.on("data", (c: Buffer) => (output += c.toString()));
    child.once("exit", (code) => resolve({ code, output }));
  });
}

async function call(
  service: Service,
  method: string,
  path: string,
  opts: { token?: string; body?: string; headers?: Record<string, string> } = {},
): Promise<{ status: number; text: string }> {
  const response = await fetch(`http://127.0.0.1:${service.port}${path}`, {
    method,
    headers: {
      ...(opts.token === undefined ? {} : { Authorization: `Bearer ${opts.token}` }),
      ...(opts.body === undefined ? {} : { "Content-Type": "application/json" }),
      ...opts.headers,
    },
    body: opts.body,
  });
  return { status: response.status, text: await response.text() };
}

describe("startup", () => {
  it("refuses to start without a token", async () => {
    const { code, output } = await exitOf({});
    assert.equal(code, 1);
    assert.match(output, /MANAGEMENT_API_TOKEN is not set/);
  });

  it("starts without a token only with MANAGEMENT_API_INSECURE=true, and warns", async () => {
    const service = await startService({ MANAGEMENT_API_INSECURE: "true" });
    try {
      assert.match(service.output(), /NO authentication/);
      assert.equal((await call(service, "GET", "/webhooks")).status, 200);
    } finally {
      await service.stop();
    }
  });
});

describe("management API over HTTP", () => {
  const TOKEN = "management-token-for-tests";
  let service: Service;

  before(async () => {
    service = await startService({ MANAGEMENT_API_TOKEN: TOKEN, MANAGEMENT_MAX_BODY_BYTES: "2048" });
  });
  after(async () => {
    await service.stop();
  });

  const ROUTES: [method: string, path: string, body?: string][] = [
    ["GET", "/webhooks"],
    ["POST", "/webhooks", '{"url":"http://127.0.0.1:9/hook"}'],
    ["DELETE", "/webhooks/999"],
    ["GET", "/metrics"],
    ["GET", "/dead-letters"],
    ["POST", "/dead-letters/999/replay"],
    ["DELETE", "/dead-letters/999"],
    ["GET", "/does-not-exist"],
  ];

  for (const [method, path, body] of ROUTES) {
    it(`${method} ${path} is 401 without or with a wrong token and not 401 with the right one`, async () => {
      assert.equal((await call(service, method, path, { body })).status, 401);
      assert.equal((await call(service, method, path, { body, token: "wrong" })).status, 401);
      assert.equal((await call(service, method, path, { body, headers: { Authorization: TOKEN } })).status, 401);
      assert.notEqual((await call(service, method, path, { body, token: TOKEN })).status, 401);
    });
  }

  it("answers 401 with a WWW-Authenticate challenge", async () => {
    const response = await fetch(`http://127.0.0.1:${service.port}/webhooks`);
    assert.equal(response.status, 401);
    assert.match(response.headers.get("www-authenticate") ?? "", /^Bearer/);
  });

  it("serves a minimal /health to anyone and full detail only with the token", async () => {
    const anonymous = await call(service, "GET", "/health");
    assert.equal(anonymous.status, 200);
    assert.deepEqual(JSON.parse(anonymous.text), { status: "ok" });

    const authed = JSON.parse((await call(service, "GET", "/health", { token: TOKEN })).text) as {
      status: string;
      webhooks: unknown;
      poller: unknown;
    };
    assert.equal(authed.status, "ok");
    assert.equal(typeof authed.webhooks, "number");
    assert.ok(authed.poller);
  });

  it("never returns, or logs, a configured secret", async () => {
    const created = await call(service, "POST", "/webhooks", {
      token: TOKEN,
      body: JSON.stringify({ url: "http://127.0.0.1:9/hook", secret: SECRET, contractId: "C1" }),
    });
    assert.equal(created.status, 201);
    assert.equal((JSON.parse(created.text) as { hasSecret: boolean }).hasSecret, true);

    const outputs = [created.text];
    for (const path of ["/webhooks", "/dead-letters", "/metrics", "/health"]) {
      outputs.push((await call(service, "GET", path, { token: TOKEN })).text);
    }
    // Error responses are scanned too.
    outputs.push((await call(service, "POST", "/webhooks", { token: TOKEN, body: `{"url":"x","secret":"${SECRET}"` })).text);
    outputs.push(service.output());
    for (const text of outputs) assert.equal(text.includes(SECRET), false);

    const listed = JSON.parse((await call(service, "GET", "/webhooks", { token: TOKEN })).text) as {
      contractId?: string;
      hasSecret: boolean;
    }[];
    const mine = listed.find((sub) => sub.contractId === "C1");
    assert.ok(mine);
    assert.equal(mine.hasSecret, true);
    assert.equal("secret" in mine, false);
  });

  it("answers distinct 400s for invalid JSON and invalid fields", async () => {
    const bad = (body: string) => call(service, "POST", "/webhooks", { token: TOKEN, body });

    const json = await bad("{nope");
    assert.equal(json.status, 400);
    assert.equal((JSON.parse(json.text) as { error: string }).error, "invalid JSON");

    const contract = await bad('{"url":"http://127.0.0.1:9/h","contractId":7}');
    assert.equal(contract.status, 400);
    assert.deepEqual(JSON.parse(contract.text), { error: "contractId must be a string", field: "contractId" });

    const empty = await bad('{"url":""}');
    assert.equal(empty.status, 400);
    assert.equal((JSON.parse(empty.text) as { field: string }).field, "url");

    const secret = await bad(JSON.stringify({ url: "http://127.0.0.1:9/h", secret: "x".repeat(300) }));
    assert.equal(secret.status, 400);
    assert.equal((JSON.parse(secret.text) as { field: string }).field, "secret");
  });

  it("rejects an oversized body with 413", async () => {
    const huge = JSON.stringify({ url: "http://127.0.0.1:9/h", secret: "x".repeat(10_000) });
    const response = await call(service, "POST", "/webhooks", { token: TOKEN, body: huge });
    assert.equal(response.status, 413);
    assert.match((JSON.parse(response.text) as { error: string }).error, /exceeds 2048 bytes/);
  });

  it("checks the token before reading an oversized body", async () => {
    const huge = JSON.stringify({ url: "x", secret: "x".repeat(10_000) });
    assert.equal((await call(service, "POST", "/webhooks", { body: huge })).status, 401);
  });
});
