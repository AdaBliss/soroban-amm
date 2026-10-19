// ── Unit tests for the Soroban RPC event poller (issue #1049) ───────────────
//
// The poller replaced the Horizon-based streamer: subscriptions no longer use
// Horizon's streaming endpoint, they page `getEvents` on Stellar RPC. These
// tests pin the wire format of the request, the cursor bookkeeping that keeps
// re-polls from replaying events, and the XDR decoding of topics and payloads.

import assert from "node:assert/strict";
import test from "node:test";

import { nativeToScVal, xdr } from "@stellar/stellar-sdk";

import {
  RpcPoller,
  RawRpcError,
  decodeEvent,
  decodeTopicName,
  decodeValue,
  toPlainObject,
  unwrapVersionedEnvelope,
} from "./rpc-poller.js";
import type { Logger, RawGetEventsResult } from "./rpc-poller.js";
import type { PoolEvent } from "./types.js";

// ── helpers ────────────────────────────────────────────────────────────────

/** Base64 XDR of a Symbol ScVal, the encoding RPC uses for event topics. */
function symbolTopic(name: string): string {
  return nativeToScVal(name, { type: "symbol" }).toXDR("base64");
}

/** Base64 XDR of an ScVec([u32 version, map payload]) event envelope. */
function versionedEnvelope(
  version: number,
  entries: Array<[string, unknown]>
): string {
  const scMap = xdr.ScVal.scvMap(
    entries.map(
      ([key, val]) =>
        new xdr.ScMapEntry({
          key: xdr.ScVal.scvSymbol(key),
          val: nativeToScVal(val),
        })
    )
  );
  return xdr.ScVal.scvVec([
    nativeToScVal(version, { type: "u32" }),
    scMap,
  ]).toXDR("base64");
}

/** Base64 XDR of an ScVec([u32 version, vec payload]) event envelope — the
 * shape a tuple-valued event body takes, as opposed to `versionedEnvelope`'s
 * map-valued one. */
function versionedTuple(version: number, items: unknown[]): string {
  return xdr.ScVal.scvVec([
    nativeToScVal(version, { type: "u32" }),
    xdr.ScVal.scvVec(items.map((item) => nativeToScVal(item))),
  ]).toXDR("base64");
}

function silentLogger(): Logger {
  return { info: () => {}, warn: () => {}, error: () => {} };
}

/** A logger that records the messages it was given, for error assertions. */
function recordingLogger(sink: string[]): Logger {
  return {
    info: () => {},
    warn: () => {},
    error: (msg: string) => {
      sink.push(msg);
    },
  };
}

interface FetchCall {
  url: string;
  method: string;
  body: {
    jsonrpc: string;
    id: number;
    method: string;
    params: Record<string, unknown>;
  };
}

/** A `fetch` stub that answers each JSON-RPC method with a canned payload. */
function stubFetch(
  calls: FetchCall[],
  responder: (method: string, params: Record<string, unknown>) => unknown
): typeof fetch {
  return (async (url: string | URL, init?: RequestInit) => {
    const raw = typeof init?.body === "string" ? init.body : "{}";
    const body = JSON.parse(raw) as FetchCall["body"];
    calls.push({ url: String(url), method: init?.method ?? "", body });
    const result = responder(body.method, body.params);
    if (result instanceof Error) {
      return { ok: false, status: 500, json: async () => ({}) } as Response;
    }
    return {
      ok: true,
      status: 200,
      json: async () => ({ jsonrpc: "2.0", id: body.id, result }),
    } as Response;
  }) as unknown as typeof fetch;
}

function rpcEvent(overrides: Partial<Record<string, unknown>> = {}) {
  return {
    id: "0000000123-00001",
    type: "contract",
    ledger: 123,
    ledgerClosedAt: "2026-01-01T00:00:00Z",
    contractId: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
    txHash: "aa".repeat(32),
    pagingToken: "0000000123-00001",
    topic: [symbolTopic("swap")],
    value: versionedEnvelope(3, [["amount", "1000"]]),
    ...overrides,
  };
}

// ── decoding ───────────────────────────────────────────────────────────────

test("decodeTopicName decodes a base64 Symbol topic", () => {
  assert.equal(decodeTopicName(symbolTopic("swap")), "swap");
  assert.equal(decodeTopicName(symbolTopic("mint_pos")), "mint_pos");
});

test("decodeTopicName falls back to the raw topic when it is not decodable", () => {
  assert.equal(decodeTopicName("not-base64-xdr"), "not-base64-xdr");
  assert.equal(decodeTopicName(""), "");
});

test("decodeValue unwraps a versioned envelope into its schema version and payload", () => {
  const decoded = decodeValue(
    versionedEnvelope(3, [
      ["amount", "1000"],
      ["actor", "GABC"],
    ])
  );
  assert.equal(decoded.schemaVersion, 3);
  assert.deepEqual(decoded.payload, { amount: "1000", actor: "GABC" });
});

test("decodeValue returns an empty payload for a missing value within the poller's expectations", () => {
  assert.deepEqual(decodeValue(undefined), { schemaVersion: 0, payload: {} });
  assert.deepEqual(decodeValue(""), { schemaVersion: 0, payload: {} });
});

test("decodeValue keeps undecodable values verbatim instead of dropping the event", () => {
  assert.deepEqual(decodeValue("not-xdr"), {
    schemaVersion: 0,
    payload: { raw: "not-xdr" },
  });
});

test("unwrapVersionedEnvelope treats a non-pair array as an unversioned payload", () => {
  // A bare ScVec payload is not an envelope: the version is unknown, so it is
  // reported as 0 rather than misreading the first element as a version.
  assert.deepEqual(unwrapVersionedEnvelope(["a", "b", "c"]), {
    schemaVersion: 0,
    payload: { value: ["a", "b", "c"] },
  });
  assert.deepEqual(unwrapVersionedEnvelope("scalar"), {
    schemaVersion: 0,
    payload: { value: "scalar" },
  });
});

test("unwrapVersionedEnvelope coerces a non-numeric version to 0", () => {
  assert.deepEqual(unwrapVersionedEnvelope(["v3", { a: 1 }]), {
    schemaVersion: 0,
    payload: { a: 1 },
  });
  assert.deepEqual(unwrapVersionedEnvelope(["not-a-number", { a: 1 }]), {
    schemaVersion: 0,
    payload: { a: 1 },
  });
});

test("toPlainObject and normalise convert bigints to strings for JSON transport", () => {
  assert.deepEqual(toPlainObject({ amount: 1n }), { amount: "1" });
  assert.deepEqual(toPlainObject([1n, 2n]), { value: ["1", "2"] });
  assert.deepEqual(toPlainObject(null), { value: null });
  assert.deepEqual(toPlainObject(7), { value: 7 });
});

test("decodeEvent maps the RPC event onto the webhook payload shape", () => {
  const decoded = decodeEvent(rpcEvent());
  assert.ok(decoded);
  const expected: PoolEvent = {
    id: "0000000123-00001",
    contractId: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
    eventType: "swap",
    schemaVersion: 3,
    payload: { amount: "1000" },
    ledger: 123,
    timestamp: "2026-01-01T00:00:00Z",
    txHash: "aa".repeat(32),
  };
  assert.deepEqual(decoded, expected);
});

test("decodeEvent skips an event with no topics rather than forwarding an empty type", () => {
  assert.equal(decodeEvent(rpcEvent({ topic: [] })), undefined);
  assert.equal(decodeEvent(rpcEvent({ topic: undefined })), undefined);
});

test("decodeEvent tolerates events missing contractId, timestamp and value", () => {
  const decoded = decodeEvent(
    rpcEvent({
      contractId: undefined,
      ledgerClosedAt: undefined,
      value: undefined,
      txHash: undefined,
    })
  );
  assert.ok(decoded);
  assert.equal(decoded.contractId, "");
  assert.equal(decoded.timestamp, "");
  assert.equal(decoded.schemaVersion, 0);
  assert.deepEqual(decoded.payload, {});
  assert.equal(decoded.txHash, undefined);
});

test("decodeTopicName translates an abbreviated on-chain topic to its documented eventType", () => {
  // contracts/amm emits the raw topic `rm_liq`/`rm_liq_1s`, but the service's
  // README documents the subscribable eventType as `remove_liquidity` /
  // `remove_liquidity_one_sided` — without this alias, a webhook filtered on
  // the documented name could never match.
  assert.equal(decodeTopicName(symbolTopic("rm_liq")), "remove_liquidity");
  assert.equal(
    decodeTopicName(symbolTopic("rm_liq_1s")),
    "remove_liquidity_one_sided"
  );
});

test("decodeEvent names a tuple-shaped payload's fields instead of flattening it to an array", () => {
  // contracts/amm's `remove_liquidity` emits `(provider, shares, out_a, out_b)`
  // as a plain tuple (contracts/amm-sdk/src/events.rs). Consumers rely on
  // named fields like the README's own example payload, so this must not
  // collapse to `{ value: [...] }`.
  const decoded = decodeEvent(
    rpcEvent({
      topic: [symbolTopic("rm_liq")],
      value: versionedTuple(1, ["GPROVIDER", 100, 11, 12]),
    }),
    "amm"
  );
  assert.ok(decoded);
  assert.equal(decoded.eventType, "remove_liquidity");
  assert.deepEqual(decoded.payload, {
    provider: "GPROVIDER",
    shares_burned: "100",
    amount_a: "11",
    amount_b: "12",
  });
});

test("decodeEvent falls back to the generic array shape when a tuple's arity doesn't match its schema", () => {
  const decoded = decodeEvent(
    rpcEvent({
      topic: [symbolTopic("rm_liq")],
      value: versionedTuple(1, ["GPROVIDER", 100]),
    }),
    "amm"
  );
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { value: ["GPROVIDER", "100"] });
});

// ── request shape and cursors ──────────────────────────────────────────────

test("the first poll sends startLedger, because RPC rejects a request without one", async () => {
  const calls: FetchCall[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => ({ events: [] })),
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();

  assert.equal(calls.length, 1);
  assert.equal(calls[0]?.body.method, "getEvents");
  assert.equal(calls[0]?.body.jsonrpc, "2.0");
  assert.equal(calls[0]?.body.id, 1);
  assert.deepEqual(calls[0]?.body.params["filters"], [
    { type: "contract", contractIds: ["C1"] },
  ]);
  assert.equal(calls[0]?.body.params["limit"], 1000);
  assert.equal(typeof calls[0]?.body.params["startLedger"], "number");
  assert.ok(!("cursor" in (calls[0]?.body.params ?? {})));
});

test("a later poll sends the cursor and drops startLedger", async () => {
  const calls: FetchCall[] = [];
  let cursor = "cursor-1";
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => ({ events: [], cursor })),
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();
  cursor = "cursor-2";
  await poller.poll();

  assert.ok(!("cursor" in (calls[0]?.body.params ?? {})));
  assert.equal(calls[1]?.body.params["cursor"], "cursor-1");
  assert.ok(!("startLedger" in (calls[1]?.body.params ?? {})));
  assert.equal(poller.getCursor("C1"), "cursor-2");
  // The RPC request id advances so a response can be matched to its request.
  assert.equal(calls[1]?.body.id, 2);
});

test("a response without a cursor but with events advances to the last event's paging token", async () => {
  const calls: FetchCall[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => ({
        events: [
          rpcEvent({ id: "1-1", pagingToken: "0000000123-00001" }),
          rpcEvent({ id: "1-2", pagingToken: "0000000123-00002" }),
        ],
      })),
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();
  await poller.poll();

  // RPC paginates by event id: replaying the whole page every tick would
  // re-deliver events, so the last page entry becomes the next cursor.
  assert.equal(poller.getCursor("C1"), "0000000123-00002");
  assert.equal(calls[1]?.body.params["cursor"], "0000000123-00002");
});

test("a response without a cursor leaves the previous cursor in place", async () => {
  const calls: FetchCall[] = [];
  let cursor: string | undefined = "cursor-1";
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => ({ events: [], cursor })),
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();
  cursor = undefined;
  await poller.poll();

  assert.equal(poller.getCursor("C1"), "cursor-1");
  assert.equal(calls[1]?.body.params["cursor"], "cursor-1");
});

test("a response whose events carry no paging token leaves the previous cursor in place", async () => {
  const calls: FetchCall[] = [];
  let first = true;
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => {
        if (first) {
          first = false;
          return { events: [], cursor: "cursor-1" };
        }
        return { events: [rpcEvent({ pagingToken: undefined })] };
      }),
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();
  await poller.poll();

  assert.equal(poller.getCursor("C1"), "cursor-1");
});

test("poll fans out across every configured contract independently", async () => {
  const calls: FetchCall[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1", "C2"],
      fetchFn: stubFetch(calls, (_method, params) => ({
        events: [
          rpcEvent({
            contractId: (params["filters"] as Array<{ contractIds: string[] }>)
              .at(0)?.contractIds.at(0),
          }),
        ],
        cursor: "c",
      })),
      logger: silentLogger(),
    },
    undefined
  );

  const events = await poller.poll();

  assert.equal(calls.length, 2);
  assert.equal(events.length, 2);
  assert.deepEqual(
    events.map((e) => e.contractId),
    ["C1", "C2"]
  );
  assert.equal(poller.getCursor("C1"), "c");
  assert.equal(poller.getCursor("C2"), "c");
  assert.equal(poller.getCursor("C3"), undefined);
});

test("an event that cannot be decoded is dropped, not returned as a partial payload", async () => {
  const calls: FetchCall[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => ({
        events: [rpcEvent(), rpcEvent({ topic: [] }), rpcEvent({ topic: [] })],
      })),
      logger: silentLogger(),
    },
    undefined
  );

  const events = await poller.poll();

  assert.equal(events.length, 1);
  assert.equal(events[0]?.eventType, "swap");
});

// ── error surfacing ────────────────────────────────────────────────────────

test("an HTTP failure is surfaced once per contract and does not abort the sweep", async () => {
  const calls: FetchCall[] = [];
  const errors: Error[] = [];
  const logged: string[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1", "C2"],
      fetchFn: stubFetch(calls, () => new Error("boom")),
      logger: recordingLogger(logged),
      onError: (err) => errors.push(err),
    },
    undefined
  );

  const events = await poller.poll();

  assert.deepEqual(events, []);
  assert.equal(calls.length, 2);
  assert.equal(errors.length, 2);
  assert.ok(errors[0] instanceof RawRpcError);
  assert.match(errors[0]?.message ?? "", /HTTP 500/);
  assert.equal(logged.length, 2);
  assert.match(logged[0] ?? "", /C1/);
  assert.match(logged[1] ?? "", /C2/);
  assert.equal(poller.getLastError()?.message, errors[0]?.message);
});

test("a JSON-RPC error response becomes a RawRpcError carrying the RPC code", async () => {
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: async () =>
        ({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: "2.0",
            id: 1,
            error: { code: -32602, message: "invalid params" },
          }),
        }) as Response,
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();

  const err = poller.lastError;
  assert.ok(err instanceof RawRpcError);
  assert.equal(err.code, -32602);
  assert.match(err.message, /invalid params/);
});

test("a success response with no result is rejected rather than read as empty", async () => {
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: async () =>
        ({
          ok: true,
          status: 200,
          json: async () => ({ jsonrpc: "2.0", id: 1 }),
        }) as Response,
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();

  assert.match(poller.lastError?.message ?? "", /had no result/);
});

test("checkHealth resolves on a healthy RPC and rejects on an unhealthy one", async () => {
  const healthy = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: [],
      fetchFn: stubFetch([], () => ({ status: "healthy" })),
      logger: silentLogger(),
    },
    undefined
  );
  assert.deepEqual(await healthy.checkHealth(), { status: "healthy" });

  const unhealthy = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: [],
      fetchFn: stubFetch([], () => new Error("down")),
      logger: silentLogger(),
    },
    undefined
  );
  await assert.rejects(() => unhealthy.checkHealth(), /HTTP 500/);
});

// ── lifecycle ──────────────────────────────────────────────────────────────

test("start polls immediately, then on the configured interval, and stop ends it", async () => {
  const calls: FetchCall[] = [];
  const delivered: PoolEvent[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      pollIntervalMs: 10,
      fetchFn: stubFetch(calls, () => ({ events: [rpcEvent()] })),
      logger: silentLogger(),
    },
    (event) => {
      delivered.push(event);
    }
  );

  poller.start();
  await new Promise((resolve) => setTimeout(resolve, 60));
  poller.stop();
  const afterStop = calls.length;
  await new Promise((resolve) => setTimeout(resolve, 40));

  assert.equal(poller.health().running, false);
  // One immediate poll plus several interval ticks.
  assert.ok(afterStop >= 2, `expected repeat polls, saw ${afterStop}`);
  // stop() actually stops: no further requests are issued.
  assert.equal(calls.length, afterStop);
  assert.equal(delivered.length, calls.length);
  assert.ok(delivered.every((e) => e.eventType === "swap"));
});

test("start is idempotent: a second call does not double the poll rate", async () => {
  const calls: FetchCall[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      pollIntervalMs: 10,
      fetchFn: stubFetch(calls, () => ({ events: [] })),
      logger: silentLogger(),
    },
    undefined
  );

  poller.start();
  poller.start();
  await new Promise((resolve) => setTimeout(resolve, 35));
  poller.stop();

  // With a single interval of 10ms over 35ms the ceiling is a handful of
  // ticks; doubling the interval would roughly double the request count.
  assert.ok(calls.length <= 6, `expected one interval, saw ${calls.length} calls`);
  // stop() when already stopped stays a no-op.
  poller.stop();
  assert.equal(poller.health().running, false);
});

test("a slow poll is not overlapped by the next tick", async () => {
  let inFlight = 0;
  let maxInFlight = 0;
  let resolveFetch: (() => void) | undefined;
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      pollIntervalMs: 5,
      logger: silentLogger(),
      fetchFn: async () => {
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        await new Promise<void>((resolve) => {
          resolveFetch = resolve;
        });
        inFlight -= 1;
        return {
          ok: true,
          status: 200,
          json: async () => ({ jsonrpc: "2.0", id: 1, result: { events: [] } }),
        } as Response;
      },
    },
    undefined
  );

  poller.start();
  await new Promise((resolve) => setTimeout(resolve, 40));
  const ticksWhileBlocked = poller.health().ticks;
  resolveFetch?.();
  poller.stop();

  assert.equal(maxInFlight, 1);
  // A blocked first poll swallows every intervening tick instead of stacking.
  assert.equal(ticksWhileBlocked, 1);
});

test("an event handler that throws is reported without killing the poller", async () => {
  const errors: Error[] = [];
  const delivered: string[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      pollIntervalMs: 5,
      fetchFn: stubFetch([], () => ({
        events: [rpcEvent({ id: "1-1" }), rpcEvent({ id: "1-2" })],
      })),
      logger: silentLogger(),
      onError: (err) => errors.push(err),
    },
    (event) => {
      if (event.id === "1-1") throw new Error("delivery exploded");
      delivered.push(event.id);
    }
  );

  poller.start();
  await new Promise((resolve) => setTimeout(resolve, 30));
  poller.stop();

  assert.equal(errors.length >= 1, true);
  assert.match(errors[0]?.message ?? "", /delivery exploded/);
  // The sibling event in the same batch was still delivered.
  assert.ok(delivered.includes("1-2"));
  assert.ok(poller.health().eventsEmitted >= 2);
});

test("health reports the configured shape before any poll has happened", () => {
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1", "C2"],
      fetchFn: stubFetch([], () => ({ events: [] })),
      logger: silentLogger(),
    },
    undefined
  );

  assert.deepEqual(poller.health(), {
    running: false,
    pollIntervalMs: 5000,
    contracts: 2,
    ticks: 0,
    eventsEmitted: 0,
    lastError: null,
    gaps: [],
  });
});

test("a non-Error rejection is normalised into an Error on the health snapshot", async () => {
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: async () => {
        // A bare rejection, not an Error: callers get a non-Error rejection
        // from a transport occasionally, and the poller must still report it.
        // eslint-disable-next-line @typescript-eslint/only-throw-error
        throw "socket closed";
      },
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();

  assert.equal(poller.health().lastError, "socket closed");
  assert.ok(poller.lastError instanceof Error);
});

test("a caller-supplied fetch receives the JSON-RPC request as a POST", async () => {
  const calls: FetchCall[] = [];
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example/rpc",
      contractIds: ["C1"],
      fetchFn: stubFetch(calls, () => ({ events: [] })),
      logger: silentLogger(),
    },
    undefined
  );

  await poller.poll();

  assert.equal(calls[0]?.url, "https://rpc.example/rpc");
  assert.equal(calls[0]?.method, "POST");
});

// ── a result with no events array at all ───────────────────────────────────

test("a getEvents result missing its events array is read as empty", async () => {
  const poller = new RpcPoller(
    {
      rpcUrl: "https://rpc.example",
      contractIds: ["C1"],
      fetchFn: async () =>
        ({
          ok: true,
          status: 200,
          json: async () =>
            ({ jsonrpc: "2.0", id: 1, result: {} }) as {
              jsonrpc: string;
              id: number;
              result: RawGetEventsResult;
            },
        }) as Response,
      logger: silentLogger(),
    },
    undefined
  );

  assert.deepEqual(await poller.poll(), []);
  assert.equal(poller.lastError, undefined);
});
