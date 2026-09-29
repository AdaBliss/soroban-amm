/**
 * Unit tests for the in-memory AnalyticsStore used by the RPC ingestion layer.
 * Run with: node --test dist/store/memory.test.js
 *
 * These cover the store-level guarantees the ingester relies on — idempotent
 * event appends keyed by (ledger, txHash, eventIndex) and cursor persistence —
 * independently of any indexer.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { MemoryStore } from "./memory.js";
import type { PoolEvent } from "./interface.js";

function event(overrides: Partial<PoolEvent> = {}): PoolEvent {
  return {
    id: "evt-1",
    poolId: "pool-1",
    type: "swap",
    timestamp: 1_723_000_000,
    ledger: 1000,
    txHash: "tx-1",
    eventIndex: 0,
    payload: { amountIn: 1000, fee: 3 },
    ...overrides,
  };
}

describe("MemoryStore event idempotency", () => {
  it("stores an event appended twice only once", async () => {
    const store = new MemoryStore();
    await store.appendEvent(event());
    await store.appendEvent(event());

    assert.equal((await store.getRecentEvents(10)).length, 1);
  });

  it("treats events differing only in eventIndex as distinct", async () => {
    const store = new MemoryStore();
    await store.appendEvent(event({ id: "evt-1", eventIndex: 0 }));
    await store.appendEvent(event({ id: "evt-2", eventIndex: 1 }));

    assert.equal((await store.getRecentEvents(10)).length, 2);
  });

  it("treats events differing only in txHash as distinct", async () => {
    const store = new MemoryStore();
    await store.appendEvent(event({ id: "evt-1", txHash: "tx-1" }));
    await store.appendEvent(event({ id: "evt-2", txHash: "tx-2" }));

    assert.equal((await store.getRecentEvents(10)).length, 2);
  });
});

describe("MemoryStore event queries", () => {
  it("filters queryEvents by pool and inclusive time range", async () => {
    const store = new MemoryStore();
    await store.appendEvent(event({ id: "a", eventIndex: 0, timestamp: 100 }));
    await store.appendEvent(event({ id: "b", eventIndex: 1, timestamp: 200 }));
    await store.appendEvent(event({ id: "c", eventIndex: 2, timestamp: 300 }));
    await store.appendEvent(
      event({ id: "d", eventIndex: 3, timestamp: 200, poolId: "pool-2" }),
    );

    const ids = (await store.queryEvents("pool-1", 200, 300)).map((e) => e.id);
    assert.deepEqual(ids, ["b", "c"]);
  });

  it("returns getRecentEvents newest first", async () => {
    const store = new MemoryStore();
    await store.appendEvent(event({ id: "first", eventIndex: 0 }));
    await store.appendEvent(event({ id: "second", eventIndex: 1 }));

    const ids = (await store.getRecentEvents(10)).map((e) => e.id);
    assert.deepEqual(ids, ["second", "first"]);
  });
});

describe("MemoryStore ingestion cursor", () => {
  it("starts with no cursor", async () => {
    const store = new MemoryStore();
    assert.equal(await store.getCursor(), null);
  });

  it("returns the cursor that was last persisted", async () => {
    const store = new MemoryStore();
    const cursor = {
      ledger: 1000,
      txHash: "tx-1",
      eventIndex: 0,
      updatedAt: 1_723_000_000_000,
    };
    await store.setCursor(cursor);
    await store.setCursor({ ...cursor, ledger: 1001 });

    assert.deepEqual(await store.getCursor(), { ...cursor, ledger: 1001 });
  });
});
