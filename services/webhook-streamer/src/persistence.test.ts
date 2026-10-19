/**
 * Durability tests (issue #1125): storage, cursor commit ordering, recovery
 * from the RPC retention window, delivery ids and graceful shutdown.
 * Run with: node --test dist/persistence.test.js
 */

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, beforeEach, describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import { CircuitBreaker } from "./circuit-breaker.js";
import { DeadLetterQueue } from "./dead-letter.js";
import { WebhookDispatcher } from "./dispatcher.js";
import { WebhookRegistry } from "./registry.js";
import { RawRpcError, RpcPoller, ledgerFromPagingToken, type Logger } from "./rpc-poller.js";
import {
  FileStateStore,
  MemoryStateStore,
  STATE_FILE_NAME,
  StateFileError,
  type CursorState,
} from "./storage.js";
import { nativeToScVal } from "@stellar/stellar-sdk";
import type { PoolEvent, RpcEvent } from "./types.js";

process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] = "true";
delete process.env["START_LEDGER"];

const silent: Logger = { info: () => {}, warn: () => {}, error: () => {} };
const dirs: string[] = [];

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "streamer-state-"));
  dirs.push(dir);
  return dir;
}
after(() => {
  for (const dir of dirs) rmSync(dir, { recursive: true, force: true });
});

// ── Fake RPC ─────────────────────────────────────────────────────────────

/** A Soroban paging token: the TOID (ledger in the high 32 bits) and an event index. */
function token(ledger: number, index = 0): string {
  return `${(BigInt(ledger) << 32n).toString().padStart(19, "0")}-${String(index).padStart(10, "0")}`;
}

function rpcEvent(ledger: number, index = 0, contractId = "CPOOL"): RpcEvent {
  return {
    id: token(ledger, index),
    pagingToken: token(ledger, index),
    type: "contract",
    ledger,
    ledgerClosedAt: "2026-06-01T00:00:00Z",
    contractId,
    topic: [nativeToScVal("swap", { type: "symbol" }).toXDR("base64")],
  };
}

interface RpcCall {
  method: string;
  params: Record<string, unknown>;
}

/**
 * A `getEvents` endpoint over a fixed ledger history with a retention window
 * `[oldest, latest]`, answering out-of-window requests with the real error
 * shape (code -32602, "... must be within the ledger range: a - b").
 */
function fakeRpc(opts: { events: RpcEvent[]; oldest?: number; latest?: number; rangeInMessage?: boolean }) {
  const calls: RpcCall[] = [];
  const oldest = opts.oldest ?? 1;
  const latest = opts.latest ?? 1_000_000;
  const fetchFn = (async (_url: unknown, init?: { body?: string }) => {
    const body = JSON.parse(init?.body ?? "{}") as { id: number; method: string; params: Record<string, unknown> };
    calls.push({ method: body.method, params: body.params });
    const reply = (result: unknown) => ({ ok: true, status: 200, json: async () => ({ jsonrpc: "2.0", id: body.id, result }) });
    const fail = (code: number, message: string) => ({ ok: true, status: 200, json: async () => ({ jsonrpc: "2.0", id: body.id, error: { code, message } }) });

    if (body.method === "getHealth") {
      return reply({ status: "healthy", latestLedger: latest, oldestLedger: oldest, ledgerRetentionWindow: latest - oldest });
    }
    const { startLedger, cursor } = body.params as { startLedger?: number; cursor?: string };
    const from = cursor ? ledgerFromPagingToken(cursor) : startLedger;
    if (from !== undefined && (from < oldest || from > latest)) {
      const range = opts.rangeInMessage === false ? "" : ` ${oldest} - ${latest}`;
      return fail(-32602, `${cursor ? "cursor" : "startLedger"} must be within the ledger range:${range}`);
    }
    const page = opts.events.filter((e) => (cursor ? e.pagingToken! > cursor : e.ledger >= (startLedger ?? 0)));
    const lastToken = page.at(-1)?.pagingToken;
    return reply({ events: page, latestLedger: latest, ...(lastToken ? { cursor: lastToken } : {}) });
  }) as unknown as typeof fetch;
  return { fetchFn, calls };
}

async function until(condition: () => boolean, timeoutMs = 3000): Promise<void> {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) throw new Error("timed out waiting for condition");
    await new Promise((r) => setTimeout(r, 5));
  }
}

// ── Receiver ─────────────────────────────────────────────────────────────

interface Receiver {
  url: string;
  ids: string[];
  close(): Promise<void>;
}

async function startReceiver(handler?: (req: IncomingMessage, n: number) => { status?: number; delayMs?: number }): Promise<Receiver> {
  const ids: string[] = [];
  const server: Server = createServer((req, res) => {
    const spec = handler?.(req, ids.length + 1) ?? {};
    ids.push(String(req.headers["x-webhook-id"]));
    req.resume();
    req.on("end", () => {
      setTimeout(() => {
        res.writeHead(spec.status ?? 200);
        res.end("ok");
      }, spec.delayMs ?? 0);
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    url: `http://127.0.0.1:${(server.address() as AddressInfo).port}/hook`,
    ids,
    close: () => new Promise<void>((resolve) => { server.closeAllConnections(); server.close(() => resolve()); }),
  };
}

function dispatcherFor(registry: WebhookRegistry, deadLetters = new DeadLetterQueue(50)) {
  return new WebhookDispatcher(registry, {
    timeoutMs: 1000,
    maxRetries: 0,
    sleep: () => Promise.resolve(),
    circuitBreaker: new CircuitBreaker({ failureThreshold: 100 }),
    deadLetterQueue: deadLetters,
  });
}

// ── Storage ──────────────────────────────────────────────────────────────

describe("FileStateStore", () => {
  it("round-trips subscriptions, dead letters and cursors across instances", () => {
    const dir = tempDir();
    const a = new FileStateStore(dir);
    a.saveSubscriptions({ subscriptions: [{ id: "7", url: "https://example.com/h", createdAt: 1, secret: "s" }], nextId: 8 });
    a.saveCursor("C1", { cursor: "abc", ledger: 42 });
    a.saveDeadLetters({ entries: [], nextId: 5 });

    const b = new FileStateStore(dir);
    assert.equal(b.loadSubscriptions().subscriptions[0].secret, "s");
    assert.equal(b.loadSubscriptions().nextId, 8);
    assert.deepEqual(b.loadCursors(), { C1: { cursor: "abc", ledger: 42 } });
    assert.equal(b.loadDeadLetters().nextId, 5);
  });

  it("replaces the file atomically and leaves no temp files behind", () => {
    const dir = tempDir();
    const store = new FileStateStore(dir);
    for (let i = 0; i < 5; i++) store.saveCursor("C1", { cursor: `c${i}` });
    assert.deepEqual(readdirSync(dir), [STATE_FILE_NAME]);
    const file = JSON.parse(readFileSync(join(dir, STATE_FILE_NAME), "utf8")) as { cursors: Record<string, CursorState> };
    assert.equal(file.cursors["C1"]?.cursor, "c4");
  });

  it("creates the state file readable by its owner only, since it holds secrets", () => {
    const dir = join(tempDir(), "nested", "data");
    new FileStateStore(dir).saveCursor("C1", { cursor: "x" });
    assert.equal(statSync(join(dir, STATE_FILE_NAME)).mode & 0o077, 0);
  });

  it("refuses to start from a corrupt or unsupported file instead of resetting", () => {
    const dir = tempDir();
    writeFileSync(join(dir, STATE_FILE_NAME), "{not json");
    assert.throws(() => new FileStateStore(dir), StateFileError);
    writeFileSync(join(dir, STATE_FILE_NAME), JSON.stringify({ version: 99 }));
    assert.throws(() => new FileStateStore(dir), /unsupported format/);
  });

  it("starts empty when the directory has no state yet", () => {
    const store = new FileStateStore(tempDir());
    assert.deepEqual(store.loadSubscriptions(), { subscriptions: [], nextId: 1 });
    assert.deepEqual(store.loadCursors(), {});
  });
});

describe("registry and dead letters over a store", () => {
  it("keeps subscriptions across a restart and never reuses an id", () => {
    const dir = tempDir();
    const first = new WebhookRegistry(new FileStateStore(dir));
    const a = first.register("https://example.com/a", { secret: "s3" });
    const b = first.register("https://example.com/b");
    first.unregister(b.id);

    const second = new WebhookRegistry(new FileStateStore(dir));
    assert.deepEqual(second.list().map((s) => s.url), ["https://example.com/a"]);
    assert.equal(second.list()[0].secret, "s3");
    const c = second.register("https://example.com/c");
    assert.notEqual(c.id, a.id);
    assert.notEqual(c.id, b.id);
    assert.ok(Number(c.id) > Number(b.id));
  });

  it("reports a failed write instead of pretending a registration is durable", () => {
    const store = new MemoryStateStore();
    const registry = new WebhookRegistry(store);
    store.saveSubscriptions = () => { throw new Error("disk full"); };
    assert.throws(() => registry.register("https://example.com/a"), /disk full/);
    assert.equal(registry.size, 0);
  });

  it("keeps dead letters across a restart and they stay replayable", async () => {
    const dir = tempDir();
    const receiver = await startReceiver((_req, n) => ({ status: n === 1 ? 500 : 200 }));
    try {
      const event: PoolEvent = { id: "evt-9", contractId: "C", eventType: "swap", ledger: 1, timestamp: "t", payload: {} };
      const store1 = new FileStateStore(dir);
      const registry1 = new WebhookRegistry(store1);
      registry1.register(receiver.url);
      const dispatcher1 = dispatcherFor(registry1, new DeadLetterQueue(50, store1));
      await dispatcher1.dispatch(event);
      assert.equal(dispatcher1.deadLetters.size, 1);

      // Restart: new objects over the same data dir.
      const store2 = new FileStateStore(dir);
      const dispatcher2 = dispatcherFor(new WebhookRegistry(store2), new DeadLetterQueue(50, store2));
      const [entry] = dispatcher2.deadLetters.list();
      assert.ok(entry);
      assert.equal(entry.event.id, "evt-9");

      const result = await dispatcher2.replay(entry.id);
      assert.equal(result?.success, true);
      assert.equal(dispatcher2.deadLetters.size, 0);
      assert.equal(new DeadLetterQueue(50, new FileStateStore(dir)).size, 0, "the removal is durable too");
    } finally {
      await receiver.close();
    }
  });

  it("does not reuse a dead-letter id after a restart", () => {
    const dir = tempDir();
    const sub = { id: "1", url: "https://example.com", createdAt: 1 };
    const event: PoolEvent = { id: "e", contractId: "C", eventType: "swap", ledger: 1, timestamp: "t", payload: {} };
    const q1 = new DeadLetterQueue(10, new FileStateStore(dir));
    const first = q1.add(event, sub, [], "boom");
    const q2 = new DeadLetterQueue(10, new FileStateStore(dir));
    assert.notEqual(q2.add(event, sub, [], "boom").id, first.id);
    assert.equal(q2.size, 2);
  });
});

// ── Cursors: restart, ordering, crash ────────────────────────────────────

function boot(dir: string, rpc: ReturnType<typeof fakeRpc>, extra: { store?: FileStateStore } = {}) {
  const store = extra.store ?? new FileStateStore(dir);
  const registry = new WebhookRegistry(store);
  const dispatcher = dispatcherFor(registry, new DeadLetterQueue(50, store));
  const poller = new RpcPoller(
    { rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn: rpc.fetchFn, logger: silent, pollIntervalMs: 10, store },
    async (event) => { await dispatcher.dispatch(event); },
  );
  return { store, registry, dispatcher, poller };
}

describe("cursors", () => {
  it("resumes after a restart with no duplicate and no missed event", async () => {
    const dir = tempDir();
    const receiver = await startReceiver();
    try {
      const history = [rpcEvent(100), rpcEvent(101), rpcEvent(102)];
      const rpc = fakeRpc({ events: history.slice(0, 2) });

      const one = boot(dir, rpc);
      one.registry.register(receiver.url);
      one.poller.start();
      await until(() => receiver.ids.length === 2);
      assert.ok(await one.poller.stopAndDrain(1000));

      // The chain moves on while the service is down.
      const rpc2 = fakeRpc({ events: history });
      const two = boot(dir, rpc2);
      assert.equal(two.registry.size, 1, "the webhook survived the restart");
      two.poller.start();
      await until(() => receiver.ids.length === 3);
      assert.ok(await two.poller.stopAndDrain(1000));

      assert.deepEqual(receiver.ids, history.map((e) => e.id), "each event exactly once, in order");
      const firstCall = rpc2.calls.find((c) => c.method === "getEvents")!;
      assert.equal(firstCall.params["cursor"], token(101), "resumed from the persisted cursor");
      assert.ok(!("startLedger" in firstCall.params));
    } finally {
      await receiver.close();
    }
  });

  it("persists a cursor only after its events were dispatched", async () => {
    const order: string[] = [];
    const store = new MemoryStateStore();
    const save = store.saveCursor.bind(store);
    store.saveCursor = (id: string, state: CursorState) => { order.push("save"); save(id, state); };
    const rpc = fakeRpc({ events: [rpcEvent(100), rpcEvent(101)] });
    const poller = new RpcPoller(
      { rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn: rpc.fetchFn, logger: silent, pollIntervalMs: 5, store },
      async () => { order.push("dispatch"); },
    );
    poller.start();
    await until(() => order.includes("save"));
    await poller.stopAndDrain(1000);
    assert.deepEqual(order.slice(0, 3), ["dispatch", "dispatch", "save"]);
  });

  it("redelivers, never skips, a page whose cursor could not be persisted", async () => {
    const dir = tempDir();
    const receiver = await startReceiver();
    try {
      const rpc = fakeRpc({ events: [rpcEvent(100), rpcEvent(101)] });

      // Process A "crashes" between dispatch and persisting the cursor.
      const crashing = new FileStateStore(dir);
      crashing.saveCursor = () => { throw new Error("crash before cursor write"); };
      const a = boot(dir, rpc, { store: crashing });
      a.registry.register(receiver.url);
      a.poller.start();
      await until(() => receiver.ids.length >= 2);
      await a.poller.stopAndDrain(1000);
      const firstRun = [...receiver.ids];
      assert.deepEqual(new FileStateStore(dir).loadCursors(), {}, "nothing was committed");

      // Process B starts clean over the same data dir and gets the page again.
      const b = boot(dir, rpc);
      b.poller.start();
      await until(() => new FileStateStore(dir).loadCursors()["CPOOL"] !== undefined);
      await b.poller.stopAndDrain(1000);

      assert.ok(firstRun.includes(token(100)) && firstRun.includes(token(101)));
      for (const id of [token(100), token(101)]) {
        assert.ok(receiver.ids.filter((x) => x === id).length >= 2, `${id} was redelivered (at-least-once)`);
      }
      assert.equal(new FileStateStore(dir).loadCursors()["CPOOL"].cursor, token(101));
    } finally {
      await receiver.close();
    }
  });

  it("keeps the old cursor in memory when persisting fails, so the next tick retries", async () => {
    const store = new MemoryStateStore();
    let failures = 1;
    const save = store.saveCursor.bind(store);
    store.saveCursor = (id: string, state: CursorState) => {
      if (failures-- > 0) throw new Error("disk full");
      save(id, state);
    };
    const delivered: string[] = [];
    const rpc = fakeRpc({ events: [rpcEvent(100)] });
    const poller = new RpcPoller(
      { rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn: rpc.fetchFn, logger: silent, pollIntervalMs: 5, store },
      async (event) => { delivered.push(event.id); },
    );
    poller.start();
    await until(() => store.loadCursors()["CPOOL"] !== undefined);
    await poller.stopAndDrain(1000);
    assert.equal(delivered.length, 2, "delivered again after the failed write");
    assert.equal(new Set(delivered).size, 1, "same event id, so receivers can deduplicate");
  });

  it("only warns about START_LEDGER when some contract has no saved cursor", () => {
    const warnings: string[] = [];
    const logger: Logger = { ...silent, warn: (m) => { warnings.push(m); } };
    const store = new MemoryStateStore();
    store.saveCursor("CPOOL", { cursor: "x" });
    new RpcPoller({ rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], logger, store });
    assert.equal(warnings.length, 0);
    new RpcPoller({ rpcUrl: "https://rpc.example", contractIds: ["CPOOL", "COTHER"], logger, store });
    assert.equal(warnings.length, 1);
  });
});

// ── Retention window ─────────────────────────────────────────────────────

describe("RPC retention window", () => {
  it("reads the paging token's ledger", () => {
    assert.equal(ledgerFromPagingToken(token(12345, 7)), 12345);
    assert.equal(ledgerFromPagingToken("not-a-token"), undefined);
    assert.equal(ledgerFromPagingToken("0000000012345678-0000000001"), undefined);
  });

  it("resumes from the oldest retained ledger when START_LEDGER is outside the window", async () => {
    const rpc = fakeRpc({ events: [rpcEvent(500), rpcEvent(501)], oldest: 400, latest: 600 });
    const warnings: string[] = [];
    const poller = new RpcPoller({
      rpcUrl: "https://rpc.example",
      contractIds: ["CPOOL"],
      fetchFn: rpc.fetchFn,
      logger: { ...silent, warn: (m) => { warnings.push(m); } },
    });

    const events = await poller.poll();
    assert.equal(events.length, 2);
    const requests = rpc.calls.filter((c) => c.method === "getEvents");
    assert.deepEqual(requests.map((c) => c.params["startLedger"]), [1, 400]);

    const { gaps } = poller.health();
    assert.equal(gaps.length, 1);
    assert.deepEqual(
      { contractId: gaps[0].contractId, reason: gaps[0].reason, fromLedger: gaps[0].fromLedger, toLedger: gaps[0].toLedger, resumedAt: gaps[0].resumedAt },
      { contractId: "CPOOL", reason: "start", fromLedger: 1, toLedger: 399, resumedAt: 400 },
    );
    assert.ok(warnings.some((w) => /gap contract=CPOOL .*ledgers=1-399/.test(w)));
  });

  it("recovers an expired cursor and reports only the ledgers it never read", async () => {
    const store = new MemoryStateStore();
    store.saveCursor("CPOOL", { cursor: token(150, 3), ledger: 150 });
    const rpc = fakeRpc({ events: [rpcEvent(300)], oldest: 250, latest: 400 });
    const poller = new RpcPoller({ rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn: rpc.fetchFn, logger: silent, store });

    const events = await poller.poll();
    assert.equal(events.length, 1);
    const [gap] = poller.health().gaps;
    assert.deepEqual([gap.reason, gap.fromLedger, gap.toLedger], ["cursor", 151, 249]);
    const requests = rpc.calls.filter((c) => c.method === "getEvents");
    assert.equal(requests[0].params["cursor"], token(150, 3));
    assert.equal(requests[1].params["startLedger"], 250);
    assert.ok(!("cursor" in requests[1].params));
    assert.equal(store.loadCursors()["CPOOL"].cursor, token(300), "the new position is saved");
  });

  it("falls back to getHealth when the error does not carry the range", async () => {
    const rpc = fakeRpc({ events: [rpcEvent(500)], oldest: 400, latest: 600, rangeInMessage: false });
    const poller = new RpcPoller({ rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn: rpc.fetchFn, logger: silent });
    assert.equal((await poller.poll()).length, 1);
    assert.ok(rpc.calls.some((c) => c.method === "getHealth"));
    assert.equal(poller.health().gaps[0].resumedAt, 400);
  });

  it("does not skip ahead when START_LEDGER is newer than the chain", async () => {
    process.env["START_LEDGER"] = "9000";
    try {
      const rpc = fakeRpc({ events: [], oldest: 400, latest: 600 });
      const errors: string[] = [];
      const poller = new RpcPoller({ rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn: rpc.fetchFn, logger: { ...silent, error: (m) => { errors.push(m); } } });
      await poller.poll();
      assert.equal(poller.health().gaps.length, 0);
      assert.equal(errors.length, 1);
      assert.equal(rpc.calls.filter((c) => c.method === "getEvents").length, 1, "no blind retry");
    } finally {
      delete process.env["START_LEDGER"];
    }
  });

  it("leaves other RPC errors alone", async () => {
    const fetchFn = (async () => ({
      ok: true,
      status: 200,
      json: async () => ({ jsonrpc: "2.0", id: 1, error: { code: -32603, message: "internal error" } }),
    })) as unknown as typeof fetch;
    const poller = new RpcPoller({ rpcUrl: "https://rpc.example", contractIds: ["CPOOL"], fetchFn, logger: silent });
    await poller.poll();
    assert.equal(poller.health().gaps.length, 0);
    assert.ok(poller.lastError instanceof RawRpcError);
  });
});

// ── Delivery ids and draining ────────────────────────────────────────────

describe("delivery ids", () => {
  it("sends X-Webhook-Id, identical on every retry", async () => {
    const receiver = await startReceiver((_req, n) => ({ status: n < 3 ? 500 : 200 }));
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url);
      const dispatcher = new WebhookDispatcher(registry, {
        timeoutMs: 1000, maxRetries: 3, baseDelayMs: 1, sleep: () => Promise.resolve(),
        circuitBreaker: new CircuitBreaker({ failureThreshold: 100 }), deadLetterQueue: new DeadLetterQueue(5),
      });
      const event: PoolEvent = { id: "stable-id", contractId: "C", eventType: "swap", ledger: 1, timestamp: "t", payload: {} };
      await dispatcher.dispatch(event);
      assert.deepEqual(receiver.ids, ["stable-id", "stable-id", "stable-id"]);
    } finally {
      await receiver.close();
    }
  });
});

describe("dispatcher.drain", () => {
  it("waits for an in-flight delivery and reports true", async () => {
    const receiver = await startReceiver(() => ({ delayMs: 150 }));
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url);
      const dispatcher = dispatcherFor(registry);
      const pending = dispatcher.dispatch({ id: "e", contractId: "C", eventType: "swap", ledger: 1, timestamp: "t", payload: {} });
      await until(() => dispatcher.inFlight === 1);
      assert.equal(await dispatcher.drain(2000), true);
      assert.equal(dispatcher.inFlight, 0);
      assert.equal((await pending)[0].success, true);
    } finally {
      await receiver.close();
    }
  });

  it("gives up after the timeout and reports false", async () => {
    const receiver = await startReceiver(() => ({ delayMs: 400 }));
    try {
      const registry = new WebhookRegistry();
      registry.register(receiver.url);
      const dispatcher = dispatcherFor(registry);
      const pending = dispatcher.dispatch({ id: "e", contractId: "C", eventType: "swap", ledger: 1, timestamp: "t", payload: {} });
      await until(() => dispatcher.inFlight === 1);
      assert.equal(await dispatcher.drain(30), false);
      await pending;
    } finally {
      await receiver.close();
    }
  });

  it("is immediately true when nothing is in flight", async () => {
    assert.equal(await dispatcherFor(new WebhookRegistry()).drain(10), true);
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

function startService(env: Record<string, string>) {
  return freePort().then(
    (port) =>
      new Promise<{ port: number; output: () => string; exited: Promise<number | null>; signal: (s: NodeJS.Signals) => void }>((resolve, reject) => {
        const child = spawn(process.execPath, [INDEX_JS], {
          env: {
            PATH: process.env["PATH"] ?? "",
            WEBHOOK_ALLOW_PRIVATE_TARGETS: "true",
            // These tests are about durability, not auth.
            MANAGEMENT_API_INSECURE: "true",
            ...env,
            PORT: String(port),
          },
          stdio: ["ignore", "pipe", "pipe"],
        });
        let output = "";
        const exited = new Promise<number | null>((done) => child.once("exit", (code) => done(code)));
        const onData = (chunk: Buffer) => {
          output += chunk.toString();
          if (/management API listening/.test(output)) {
            resolve({ port, output: () => output, exited, signal: (s) => child.kill(s) });
          }
        };
        child.stdout?.on("data", onData);
        child.stderr?.on("data", onData);
        child.once("error", reject);
        void exited.then((code) => reject(new Error(`exited early (${code}):\n${output}`)));
      }),
  );
}

describe("the running service", () => {
  let dataDir: string;
  beforeEach(() => { dataDir = tempDir(); });

  it("keeps webhooks across a SIGTERM restart", async () => {
    const first = await startService({ DATA_DIR: dataDir });
    const created = await fetch(`http://127.0.0.1:${first.port}/webhooks`, {
      method: "POST",
      body: JSON.stringify({ url: "http://127.0.0.1:9/hook", eventType: "swap" }),
    });
    assert.equal(created.status, 201);
    first.signal("SIGTERM");
    assert.equal(await first.exited, 0);

    const second = await startService({ DATA_DIR: dataDir });
    const listed = (await (await fetch(`http://127.0.0.1:${second.port}/webhooks`)).json()) as { url: string }[];
    assert.deepEqual(listed.map((w) => w.url), ["http://127.0.0.1:9/hook"]);
    second.signal("SIGTERM");
    assert.equal(await second.exited, 0);
  });

  it("warns when DATA_DIR is unset", async () => {
    const service = await startService({});
    try {
      assert.match(service.output(), /DATA_DIR is not set/);
    } finally {
      service.signal("SIGTERM");
      await service.exited;
    }
  });

  it("refuses to start from a corrupt state file", async () => {
    mkdirSync(dataDir, { recursive: true });
    writeFileSync(join(dataDir, STATE_FILE_NAME), "garbage");
    await assert.rejects(startService({ DATA_DIR: dataDir }), /Refusing to start with empty state/);
  });

  it("on SIGTERM finishes the in-flight delivery, saves the cursor and exits 0", async () => {
    let armed = false;
    const rpcServer = createServer((req, res) => {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        const { method, params } = JSON.parse(body) as { method: string; params: Record<string, unknown> };
        res.setHeader("content-type", "application/json");
        if (method === "getHealth") {
          res.end(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { status: "healthy", oldestLedger: 1, latestLedger: 1000 } }));
          return;
        }
        const events = armed && !params["cursor"] ? [rpcEvent(100)] : [];
        res.end(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { events, latestLedger: 1000, ...(events.length ? { cursor: token(100) } : {}) } }));
      });
    });
    await new Promise<void>((resolve) => rpcServer.listen(0, "127.0.0.1", resolve));

    const receiver = await startReceiver(() => ({ delayMs: 600 }));

    const service = await startService({
      DATA_DIR: dataDir,
      SOROBAN_RPC_URL: `http://127.0.0.1:${(rpcServer.address() as AddressInfo).port}`,
      CONTRACT_IDS: "CPOOL",
      POLL_INTERVAL_MS: "20",
      DELIVERY_TIMEOUT_MS: "5000",
      SHUTDOWN_TIMEOUT_MS: "5000",
    });
    try {
      const created = await fetch(`http://127.0.0.1:${service.port}/webhooks`, {
        method: "POST",
        body: JSON.stringify({ url: receiver.url }),
      });
      assert.equal(created.status, 201);

      armed = true;
      await until(() => receiver.ids.length === 1, 5000); // request received, response still pending (600 ms)
      const started = Date.now();
      service.signal("SIGTERM");
      const code = await service.exited;

      assert.equal(code, 0);
      assert.ok(Date.now() - started >= 200, "waited for the in-flight delivery instead of exiting at once");
      assert.ok(Date.now() - started < 5000, "exited within the shutdown timeout");
      assert.match(service.output(), /shutdown complete/);
      const saved = new FileStateStore(dataDir).loadCursors()["CPOOL"];
      assert.equal(saved?.cursor, token(100), "the cursor was committed after the delivery finished");
      assert.equal(receiver.ids.length, 1, "no duplicate delivery");
    } finally {
      service.signal("SIGKILL");
      await receiver.close();
      await new Promise<void>((resolve) => { rpcServer.closeAllConnections(); rpcServer.close(() => resolve()); });
    }
  });
});
