import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, it } from "node:test";
import { asSeconds } from "../time.js";
import { SqliteStore } from "./sqlite.js";
import type { IngestionCursor, PoolEvent } from "./interface.js";

function event(overrides: Partial<PoolEvent> = {}): PoolEvent {
  return {
    id: "event-1", poolId: "pool-1", type: "swap", timestamp: asSeconds(100),
    ledger: 12, txHash: "tx-1", eventIndex: 0,
    payload: { amountIn: 10, amountOut: 9 }, ...overrides,
  };
}

function cursor(overrides: Partial<IngestionCursor> = {}): IngestionCursor {
  return {
    contractId: "pool-1", ledger: 12, txHash: "tx-1", eventIndex: 0,
    updatedAt: asSeconds(101), ...overrides,
  };
}

async function databasePath(): Promise<{ directory: string; path: string }> {
  const directory = await mkdtemp(join(tmpdir(), "soroban-amm-graphql-"));
  return { directory, path: join(directory, "analytics.sqlite") };
}

describe("SqliteStore", () => {
  it("atomically persists an idempotent event and per-contract cursor", async (t) => {
    const location = await databasePath();
    t.after(() => rm(location.directory, { recursive: true, force: true }));
    const store = new SqliteStore(location.path);
    assert.equal(await store.appendEventAndSetCursor(event(), cursor()), true);
    assert.equal(await store.appendEventAndSetCursor(event(), cursor({ updatedAt: asSeconds(102) })), false);
    await store.appendEventAndSetCursor(
      event({ id: "event-2", poolId: "pool-2", ledger: 20, txHash: "tx-2" }),
      cursor({ contractId: "pool-2", ledger: 20, txHash: "tx-2" }),
    );
    assert.equal((await store.getRecentEvents(10)).length, 2);
    assert.equal((await store.getCursor("pool-1"))?.updatedAt, 102);
    assert.equal((await store.getCursor("pool-2"))?.ledger, 20);
    await store.close();
  });

  it("survives close and reopen with analytics and alert data", async (t) => {
    const location = await databasePath();
    t.after(() => rm(location.directory, { recursive: true, force: true }));
    let store = new SqliteStore(location.path);
    await store.appendEventAndSetCursor(event(), cursor());
    await store.upsertPoolStats({
      poolId: "pool-1", tokenA: "A", tokenB: "B", reserveA: 10, reserveB: 20,
      tvl: 50, volume24h: 9, fees24h: 0.03, feeBps: 30,
      feeBpsSource: "configured", swapCount: 1, priceDeviationBps: 0,
      lastUpdate: asSeconds(100),
    });
    await store.upsertPosition({ id: "position-1", poolId: "pool-1", owner: "owner", shares: 5, valueUsd: 25 });
    await store.recordPrice({ poolId: "pool-1", timestamp: asSeconds(100), price: 2, feeBps: 30, feeBpsSource: "configured" });
    await store.setAlertConfig({ poolId: "pool-1", metric: "tvl", thresholdValue: 10 });
    await store.recordAlert({ poolId: "pool-1", metric: "tvl", threshold: 10, currentValue: 50, firedAt: asSeconds(100) });
    await store.close();

    store = new SqliteStore(location.path);
    assert.equal((await store.getEventsSince(asSeconds(0))).length, 1);
    assert.equal((await store.getPoolStats("pool-1"))[0].reserveB, 20);
    assert.equal((await store.getPositions("owner"))[0].valueUsd, 25);
    assert.equal((await store.getPriceHistory("pool-1", asSeconds(0), asSeconds(200)))[0].price, 2);
    assert.equal((await store.getAlertConfigs("pool-1"))[0].thresholdValue, 10);
    assert.equal((await store.getFiredAlerts("pool-1"))[0].currentValue, 50);
    await store.close();
  });

  it("prunes retained history without deleting cursors", async (t) => {
    const location = await databasePath();
    t.after(() => rm(location.directory, { recursive: true, force: true }));
    const store = new SqliteStore(location.path);
    await store.appendEventAndSetCursor(event({ timestamp: asSeconds(10) }), cursor());
    await store.appendEvent(event({ id: "new", ledger: 13, timestamp: asSeconds(30) }));
    await store.recordPrice({ poolId: "pool-1", timestamp: asSeconds(10), price: 1, feeBps: 30 });
    await store.recordAlert({ poolId: "pool-1", metric: "tvl", threshold: 1, currentValue: 2, firedAt: asSeconds(10) });
    await store.prune(asSeconds(20));
    assert.deepEqual((await store.getRecentEvents(10)).map((item) => item.id), ["new"]);
    assert.equal((await store.getPriceHistory("pool-1", asSeconds(0), asSeconds(100))).length, 0);
    assert.equal((await store.getFiredAlerts()).length, 0);
    assert.equal((await store.getCursor("pool-1"))?.ledger, 12);
    await store.close();
  });
});
