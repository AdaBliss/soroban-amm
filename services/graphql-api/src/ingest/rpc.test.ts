import test from "node:test";
import assert from "node:assert";
import { RpcIngester, type SorobanRpcEvent } from "./rpc.js";
import { xdr, nativeToScVal } from "@stellar/stellar-sdk";
import type { PoolEvent } from "../store/interface.js";
import { asSeconds } from "../time.js";

// Helper to encode sample XDR fixtures for tests
function encodeFixture(topicSymbol: string, schemaVersion: number, payload: unknown, traderOrProvider?: string): { topic: string[]; value: string } {
  const topics: string[] = [nativeToScVal(topicSymbol, { type: "symbol" }).toXDR("base64")];
  if (traderOrProvider) {
    // arbitrary address scVal for topic[1] or similar
    topics.push(nativeToScVal(traderOrProvider, { type: "address" }).toXDR("base64"));
  } else {
    // schema version in topic[1] or data tuple
    topics.push(nativeToScVal(schemaVersion, { type: "u32" }).toXDR("base64"));
  }

  // Versioned envelope data: (schemaVersion, payload)
  const envelope = xdr.ScVal.scvVec([
    nativeToScVal(schemaVersion, { type: "u32" }),
    nativeToScVal(payload),
  ]);
  return {
    topic: topics,
    value: envelope.toXDR("base64"),
  };
}

/** Shape of the JSON-RPC request body RpcIngester sends. */
interface RpcRequestBody {
  jsonrpc: string;
  id: string;
  method: string;
  params: Record<string, unknown>;
}

/** The private surface these tests exercise directly. */
interface RpcIngesterInternals {
  _poll(): Promise<void>;
  _decodeEvent(raw: SorobanRpcEvent, contractId: string): PoolEvent | null;
  contractCursors: Map<string, string>;
}

function internals(ingester: RpcIngester): RpcIngesterInternals {
  return ingester as unknown as RpcIngesterInternals;
}

/** A `fetch`-shaped stub for monkey-patching `global.fetch` in these tests. */
function stubFetch(
  handler: (body: RpcRequestBody) => { ok: boolean; json: () => Promise<unknown> },
): typeof fetch {
  return (async (_url, init) => {
    const body = JSON.parse(init?.body as string) as RpcRequestBody;
    return handler(body);
  }) as typeof fetch;
}

test("1. With START_LEDGER unset, the first getEvents request contains a positive startLedger", async () => {
  let capturedBody: RpcRequestBody | null = null;
  const originalFetch = global.fetch;

  global.fetch = stubFetch((body) => {
    capturedBody = body;
    if (body.method === "getLatestLedger") {
      return {
        ok: true,
        json: async () => ({ jsonrpc: "2.0", id: "1", result: { sequence: 12345, oldestLedger: 2345 } }),
      };
    }
    return {
      ok: true,
      json: async () => ({
        jsonrpc: "2.0",
        id: "1",
        result: { events: [], latestLedger: 12345 },
      }),
    };
  });

  try {
    const ingester = new RpcIngester(
      { rpcUrl: "https://rpc.test", contractIds: ["C_POOL"] },
      async () => {},
      async () => {},
    );

    // Trigger poll manually by starting and stopping quickly or calling private method
    await internals(ingester)._poll();

    assert.ok(capturedBody, "Fetch should have been called");
    const body = capturedBody as RpcRequestBody;
    assert.strictEqual(body.method, "getEvents");
    const startLedger = body.params["startLedger"];
    assert.ok(typeof startLedger === "number" && startLedger > 0, `startLedger should be positive, got ${String(startLedger)}`);
  } finally {
    global.fetch = originalFetch;
  }
});

test("2. A real swap event fixture (AMM) decodes to a PoolEvent of type swap with correct amounts and schema version", async () => {
  const fixture = encodeFixture("swap", 1, ["GC_IN", 1000, "GC_OUT", 950, null], "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF");
  const rawEvent: SorobanRpcEvent = {
    type: "contract",
    ledger: 100,
    ledgerClosedAt: new Date().toISOString(),
    contractId: "C_POOL",
    id: "evt-1",
    pagingToken: "100-0",
    topic: fixture.topic,
    value: fixture.value,
    inSuccessfulContractInvocation: true,
    txHash: "tx-abc",
  };

  const ingester = new RpcIngester(
    { rpcUrl: "https://rpc.test", contractIds: ["C_POOL"] },
    async () => {},
    async () => {},
  );

  const result = internals(ingester)._decodeEvent(rawEvent, "C_POOL");
  assert.notStrictEqual(result, null);
  assert.strictEqual(result!.type, "swap");
  assert.strictEqual(result!.payload["amountIn"], 1000);
  assert.strictEqual(result!.payload["amountOut"], 950);
  assert.strictEqual(result!.payload["feeBpsSource"], "default");
  assert.strictEqual(result!.txHash, "tx-abc");
});

test("3. Fixtures for add_liquidity and remove_liquidity decode correctly", async () => {
  const addFixture = encodeFixture("add_liquidity", 1, [5000, 5000, 1000], "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF");
  const rawAdd: SorobanRpcEvent = {
    type: "contract",
    ledger: 101,
    ledgerClosedAt: new Date().toISOString(),
    contractId: "C_POOL",
    id: "evt-add",
    pagingToken: "101-0",
    topic: addFixture.topic,
    value: addFixture.value,
    inSuccessfulContractInvocation: true,
  };

  const ingester = new RpcIngester(
    { rpcUrl: "https://rpc.test", contractIds: ["C_POOL"] },
    async () => {},
    async () => {},
  );

  const decodedAdd = internals(ingester)._decodeEvent(rawAdd, "C_POOL");
  assert.notStrictEqual(decodedAdd, null);
  assert.strictEqual(decodedAdd!.type, "add_liquidity");
  assert.strictEqual(decodedAdd!.payload["amountA"], 5000);
  assert.strictEqual(decodedAdd!.payload["amountB"], 5000);

  const rmFixture = encodeFixture("remove_liquidity", 1, ["G_PROV", 500, 2500, 2500]);
  const rawRm: SorobanRpcEvent = {
    type: "contract",
    ledger: 102,
    ledgerClosedAt: new Date().toISOString(),
    contractId: "C_POOL",
    id: "evt-rm",
    pagingToken: "102-0",
    topic: rmFixture.topic,
    value: rmFixture.value,
    inSuccessfulContractInvocation: true,
  };

  const decodedRm = internals(ingester)._decodeEvent(rawRm, "C_POOL");
  assert.notStrictEqual(decodedRm, null);
  assert.strictEqual(decodedRm!.type, "remove_liquidity");
  assert.strictEqual(decodedRm!.payload["amountA"], 2500);
});

test("4. An event whose data version is higher than supported is rejected. Version equal to supported is accepted", async () => {
  const newerFixture = encodeFixture("swap", 2, [100, 200]);
  const rawNewer: SorobanRpcEvent = {
    type: "contract",
    ledger: 103,
    ledgerClosedAt: new Date().toISOString(),
    contractId: "C_POOL",
    id: "evt-new",
    pagingToken: "103-0",
    topic: newerFixture.topic,
    value: newerFixture.value,
    inSuccessfulContractInvocation: true,
  };

  const ingester = new RpcIngester(
    { rpcUrl: "https://rpc.test", contractIds: ["C_POOL"] },
    async () => {},
    async () => {},
  );

  const resNewer = internals(ingester)._decodeEvent(rawNewer, "C_POOL");
  assert.strictEqual(resNewer, null, "Newer schema version should be rejected");

  const validFixture = encodeFixture("swap", 1, [100, 200]);
  const rawValid: SorobanRpcEvent = {
    type: "contract",
    ledger: 104,
    ledgerClosedAt: new Date().toISOString(),
    contractId: "C_POOL",
    id: "evt-valid",
    pagingToken: "104-0",
    topic: validFixture.topic,
    value: validFixture.value,
    inSuccessfulContractInvocation: true,
  };

  const resValid = internals(ingester)._decodeEvent(rawValid, "C_POOL");
  assert.notStrictEqual(resValid, null, "Supported schema version should be accepted");
});

test("5. Two contracts polled in one cycle each advance their own cursor", async () => {
  const originalFetch = global.fetch;

  global.fetch = stubFetch((body) => {
    const params = body.params as { filters: Array<{ contractIds: string[] }>; pagination: { cursor?: string } };
    const contractId = params.filters[0].contractIds[0];
    const cursor = params.pagination.cursor;

    let events: SorobanRpcEvent[] = [];
    if (!cursor) {
      const fixture = encodeFixture("swap", 1, ["A", 10, "B", 10]);
      events = [
        {
          type: "contract",
          ledger: 200,
          ledgerClosedAt: new Date().toISOString(),
          contractId,
          id: `evt-${contractId}-1`,
          pagingToken: `${contractId}-token-1`,
          topic: fixture.topic,
          value: fixture.value,
          inSuccessfulContractInvocation: true,
        },
      ];
    }
    return {
      ok: true,
      json: async () => ({
        jsonrpc: "2.0",
        id: "1",
        result: { events, latestLedger: 250 },
      }),
    };
  });

  try {
    const ingester = new RpcIngester(
      { rpcUrl: "https://rpc.test", contractIds: ["POOL_1", "POOL_2"], startLedger: 100 },
      async () => {},
      async () => {},
    );

    await internals(ingester)._poll();
    await internals(ingester)._poll(); // second poll should send respective cursors

    const cursors = internals(ingester).contractCursors;
    assert.strictEqual(cursors.get("POOL_1"), "POOL_1-token-1");
    assert.strictEqual(cursors.get("POOL_2"), "POOL_2-token-1");
  } finally {
    global.fetch = originalFetch;
  }
});

test("6. Replaying the same page twice produces no duplicate store entries (idempotent MemoryStore)", async () => {
  const { MemoryStore } = await import("../store/memory.js");
  const store = new MemoryStore();

  const event: PoolEvent = {
    id: "evt-dup",
    poolId: "POOL_1",
    type: "swap",
    timestamp: asSeconds(123456),
    ledger: 100,
    txHash: "tx-1",
    eventIndex: 0,
    payload: { amountIn: 100 },
  };

  await store.appendEvent(event);
  await store.appendEvent(event); // replay

  const recent = await store.getRecentEvents(10);
  assert.strictEqual(recent.length, 1);
});

test("7. Retention check detects ledger outside window", async () => {
  let errorCaught: Error | null = null;
  const originalFetch = global.fetch;

  global.fetch = stubFetch(() => ({
    ok: true,
    json: async () => ({
      jsonrpc: "2.0",
      id: "1",
      result: {
        events: [],
        latestLedger: 50000,
        oldestLedger: 40000,
      },
    }),
  }));

  try {
    const ingester = new RpcIngester(
      { rpcUrl: "https://rpc.test", contractIds: ["POOL_1"], startLedger: 1000 },
      async () => {},
      async (err: Error) => { errorCaught = err; },
    );

    await internals(ingester)._poll();
    assert.notStrictEqual(errorCaught, null);
    assert.ok(errorCaught!.message.includes("retention window"));
  } finally {
    global.fetch = originalFetch;
  }
});

test("8. Malformed event topics or XDR gracefully handled", async () => {
  const ingester = new RpcIngester(
    { rpcUrl: "https://rpc.test", contractIds: ["POOL_1"] },
    async () => {},
    async () => {},
  );

  const rawBad: SorobanRpcEvent = {
    type: "contract",
    ledger: 100,
    ledgerClosedAt: new Date().toISOString(),
    contractId: "POOL_1",
    id: "bad-evt",
    pagingToken: "100-0",
    topic: ["invalid-base85-or-xdr"],
    value: "invalid-value",
    inSuccessfulContractInvocation: true,
  };

  const res = internals(ingester)._decodeEvent(rawBad, "POOL_1");
  assert.strictEqual(res, null);
});

test("9. Event persistence and cursor advancement are atomic before indexing", async () => {
  const { MemoryStore } = await import("../store/memory.js");
  const store = new MemoryStore();
  const originalFetch = global.fetch;
  const fixture = encodeFixture("swap", 1, ["A", 10, "B", 9, null]);
  const raw: SorobanRpcEvent = {
    type: "contract", ledger: 500, ledgerClosedAt: new Date().toISOString(),
    contractId: "POOL_1", id: "atomic", pagingToken: "500-0",
    topic: fixture.topic, value: fixture.value,
    inSuccessfulContractInvocation: true, txHash: "atomic-tx",
  };
  global.fetch = stubFetch(() => ({
    ok: true,
    json: async () => ({ jsonrpc: "2.0", id: "1", result: { events: [raw], latestLedger: 501 } }),
  }));
  let callbacks = 0;
  try {
    const ingester = new RpcIngester(
      { rpcUrl: "https://rpc.test", contractIds: ["POOL_1"], startLedger: 499 },
      async () => {
        callbacks += 1;
        assert.equal((await store.getRecentEvents(10)).length, 1);
        assert.equal((await store.getCursor("POOL_1"))?.ledger, 500);
      },
      async () => {},
      store,
    );
    await internals(ingester)._poll();
    await internals(ingester)._poll();
    assert.equal(callbacks, 1, "idempotent replay must not update computed state twice");
  } finally {
    global.fetch = originalFetch;
  }
});
