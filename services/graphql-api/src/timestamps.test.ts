/**
 * Timestamp unit tests (issue #1126).
 *
 * The ingester stamps events in seconds; the indexer used to prune and window
 * against `Date.now()` in milliseconds, so everything ingested was deleted on
 * arrival. These tests pin one unit (seconds since the Unix epoch) across the
 * ingester, the indexer and the GraphQL responses.
 * Run with: node --test dist/timestamps.test.js
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { ApolloServer } from "@apollo/server";
import { nativeToScVal, xdr } from "@stellar/stellar-sdk";
import { PoolIndexer, RETENTION_SECONDS, defaultIndexer, type PoolEvent } from "./indexer.js";
import { RpcIngester, type SorobanRpcEvent } from "./ingest/rpc.js";
import { resolvers } from "./resolvers.js";
import { typeDefs } from "./schema.js";
import { asSeconds, isoToSeconds, msToSeconds, systemClock, type Seconds } from "./time.js";

const NOW = asSeconds(1_800_000_000);

function at(offsetSeconds: number): Seconds {
  return asSeconds(NOW + offsetSeconds);
}

function swap(id: string, timestamp: Seconds, price = 1.5): PoolEvent {
  return {
    id,
    poolId: "pool-t",
    type: "swap",
    timestamp,
    payload: { amountIn: 100, fee: 1, price, tokenA: "XLM", tokenB: "USDC" },
  };
}

// ── time.ts ──────────────────────────────────────────────────────────────

describe("time helpers", () => {
  it("msToSeconds floors a millisecond epoch value", () => {
    assert.equal(msToSeconds(1_800_000_000_999), 1_800_000_000);
  });

  it("isoToSeconds converts a ledger close time", () => {
    assert.equal(isoToSeconds("2027-01-15T08:00:00Z"), 1_800_000_000);
  });

  it("systemClock returns seconds, not milliseconds", () => {
    const before = Math.floor(Date.now() / 1000);
    const value = systemClock();
    const after = Math.floor(Date.now() / 1000);
    assert.ok(value >= before && value <= after);
    assert.ok(value < 1e11, "a seconds value is far below 1e11 until the year 5138");
  });
});

// ── Indexer with second-resolution events (the issue's probe) ─────────────

describe("PoolIndexer with second-resolution timestamps", () => {
  function fiveSwaps(idx: PoolIndexer) {
    for (let i = 0; i < 5; i++) {
      idx.indexEvent(swap(`e${i}`, at(-(5 - i) * 60), 1 + i));
    }
  }

  it("retains every event", () => {
    const idx = new PoolIndexer(() => NOW);
    fiveSwaps(idx);
    assert.equal(idx.getEvents("pool-t").length, 5);
  });

  it("retains every price point", () => {
    const idx = new PoolIndexer(() => NOW);
    fiveSwaps(idx);
    assert.equal(idx.getPriceHistory("pool-t").length, 5);
  });

  it("getTwal returns the time-weighted price", () => {
    const idx = new PoolIndexer(() => NOW);
    // Price 1 for 60s, price 3 for the following 240s (until NOW).
    idx.recordPrice({ poolId: "pool-t", timestamp: at(-300), price: 1, feeBps: 30 });
    idx.recordPrice({ poolId: "pool-t", timestamp: at(-240), price: 3, feeBps: 30 });
    const twal = idx.getTwal("pool-t", 3600);
    assert.ok(twal !== null);
    assert.ok(Math.abs(twal - (1 * 60 + 3 * 240) / 300) < 1e-9, `got ${twal}`);
  });

  it("getTwal only looks at the trailing window", () => {
    const idx = new PoolIndexer(() => NOW);
    idx.recordPrice({ poolId: "pool-t", timestamp: at(-7200), price: 9, feeBps: 30 });
    idx.recordPrice({ poolId: "pool-t", timestamp: at(-30), price: 2, feeBps: 30 });
    assert.equal(idx.getTwal("pool-t", 60), 2);
  });

  it("getPriceHistory filters by an inclusive [from, to] range in seconds", () => {
    const idx = new PoolIndexer(() => NOW);
    fiveSwaps(idx); // at -300, -240, -180, -120, -60
    const slice = idx.getPriceHistory("pool-t", at(-240), at(-120));
    assert.deepEqual(
      slice.map((p) => p.timestamp),
      [at(-240), at(-180), at(-120)],
    );
  });

  it("price deviation compares against the last hour of seconds", () => {
    const idx = new PoolIndexer(() => NOW);
    // Stale point (2h old) must not drag the one-hour average.
    idx.recordPrice({ poolId: "pool-t", timestamp: at(-7200), price: 100, feeBps: 30 });
    idx.indexEvent(swap("a", at(-120), 1));
    idx.indexEvent(swap("b", at(-60), 1));
    idx.indexEvent(swap("c", at(-1), 1.1));
    const [stats] = idx.getPoolStats("pool-t");
    assert.ok(stats.priceDeviationBps > 0 && stats.priceDeviationBps < 1000);
  });
});

// ── Retention against an injected clock ───────────────────────────────────

describe("retention with an injected clock", () => {
  it("keeps an event exactly at the cutoff's newer side and drops the older one", () => {
    const idx = new PoolIndexer(() => NOW);
    idx.indexEvent(swap("edge-new", at(-RETENTION_SECONDS + 1)));
    idx.indexEvent(swap("edge-old", at(-RETENTION_SECONDS - 1)));
    assert.deepEqual(
      idx.getEvents("pool-t").map((e) => e.id),
      ["edge-new"],
    );
  });

  it("prunes events as the clock advances", () => {
    let now: Seconds = NOW;
    const idx = new PoolIndexer(() => now);
    idx.indexEvent(swap("early", at(0)));
    now = at(RETENTION_SECONDS + 10);
    idx.indexEvent(swap("late", now));
    assert.deepEqual(
      idx.getEvents("pool-t").map((e) => e.id),
      ["late"],
    );
  });

  it("prunes old price points and fired alerts, keeps new ones", () => {
    let now: Seconds = NOW;
    const idx = new PoolIndexer(() => now);
    idx.setAlertConfig({ poolId: "pool-t", metric: "volume24h", thresholdValue: 1 });
    idx.indexEvent(swap("a", NOW)); // fires an alert stamped NOW
    assert.equal(idx.getPoolHealth("pool-t")!.alertsFired[0].firedAt, NOW);

    now = at(RETENTION_SECONDS + 10);
    idx.indexEvent(swap("b", now));
    assert.deepEqual(
      idx.getPriceHistory("pool-t").map((p) => p.timestamp),
      [now],
    );
    const fired = idx.getPoolHealth("pool-t")!.alertsFired;
    assert.ok(fired.every((a) => a.firedAt === now), "only the new alert survives");
  });

  it("stamps firedAt with the injected clock, in seconds", () => {
    const idx = new PoolIndexer(() => NOW);
    idx.setAlertConfig({ poolId: "pool-t", metric: "volume24h", thresholdValue: 1 });
    idx.indexEvent(swap("a", NOW));
    assert.equal(idx.getPoolHealth("pool-t")!.alertsFired[0].firedAt, NOW);
  });
});

// ── Ingester -> indexer -> GraphQL ────────────────────────────────────────

function encodeSwap(): { topic: string[]; value: string } {
  return {
    topic: [
      nativeToScVal("swap", { type: "symbol" }).toXDR("base64"),
      nativeToScVal("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF", {
        type: "address",
      }).toXDR("base64"),
    ],
    value: xdr.ScVal.scvVec([
      nativeToScVal(1, { type: "u32" }),
      nativeToScVal(["GC_IN", 1000, "GC_OUT", 950, null]),
    ]).toXDR("base64"),
  };
}

describe("RpcIngester events through the GraphQL API", () => {
  const POOL = "C_TIMESTAMP_E2E";
  const ingester = new RpcIngester(
    { rpcUrl: "https://rpc.test", contractIds: [POOL] },
    async () => {},
    async () => {},
  );
  const decode = (ingester as unknown as {
    _decodeEvent(raw: SorobanRpcEvent, contractId: string): PoolEvent | null;
  })._decodeEvent.bind(ingester);

  const nowSec = systemClock();
  const closedAtSeconds = [600, 480, 360, 240, 120].map((age) => nowSec - age);
  const fixture = encodeSwap();

  for (const [i, closed] of closedAtSeconds.entries()) {
    const event = decode(
      {
        type: "contract",
        ledger: 100 + i,
        ledgerClosedAt: new Date(closed * 1000).toISOString(),
        contractId: POOL,
        id: `e2e-${i}`,
        pagingToken: `${100 + i}-0`,
        topic: fixture.topic,
        value: fixture.value,
        inSuccessfulContractInvocation: true,
        txHash: `tx-${i}`,
      },
      POOL,
    );
    assert.ok(event, "fixture should decode");
    defaultIndexer.indexEvent(event);
  }

  const server = new ApolloServer({ typeDefs, resolvers });

  async function query<T>(q: string): Promise<T> {
    const response = await server.executeOperation({ query: q });
    assert.equal(response.body.kind, "single");
    if (response.body.kind !== "single") throw new Error("unreachable");
    assert.equal(response.body.singleResult.errors, undefined);
    return response.body.singleResult.data as T;
  }

  it("poolEvents returns all five events with second timestamps", async () => {
    const data = await query<{ poolEvents: { timestamp: number }[] }>(
      `{ poolEvents(poolId: "${POOL}") { timestamp } }`,
    );
    assert.equal(data.poolEvents.length, 5);
    assert.ok(data.poolEvents.every((e) => e.timestamp < 1e11));
    assert.deepEqual(data.poolEvents.map((e) => e.timestamp).sort(), [...closedAtSeconds].sort());
  });

  it("priceHistory honours from/to in seconds", async () => {
    const [, second, third] = closedAtSeconds;
    const data = await query<{ priceHistory: { timestamp: number; price: number }[] }>(
      `{ priceHistory(poolId: "${POOL}", from: ${second}, to: ${third}) { timestamp price } }`,
    );
    assert.deepEqual(
      data.priceHistory.map((p) => p.timestamp),
      [second, third],
    );
    assert.ok(Math.abs(data.priceHistory[0].price - 0.95) < 1e-9);
  });

  it("twal returns the price", async () => {
    const data = await query<{ twal: number | null }>(
      `{ twal(poolId: "${POOL}", windowSeconds: 3600) }`,
    );
    assert.ok(data.twal !== null && Math.abs(data.twal - 0.95) < 1e-9);
  });

  it("poolStats counts every swap", async () => {
    const data = await query<{ poolStats: { swapCount: number; volume24h: number }[] }>(
      `{ poolStats(poolId: "${POOL}") { swapCount volume24h } }`,
    );
    assert.equal(data.poolStats[0].swapCount, 5);
    assert.equal(data.poolStats[0].volume24h, 5000);
  });

  it("alert firedAt is in seconds", async () => {
    defaultIndexer.setAlertConfig({ poolId: POOL, metric: "volume24h", thresholdValue: 1 });
    defaultIndexer.indexEvent({
      id: "e2e-alert",
      poolId: POOL,
      type: "swap",
      timestamp: systemClock(),
      payload: { amountIn: 10, price: 1, tokenA: "A", tokenB: "B" },
    });
    const data = await query<{ poolHealth: { alertsFired: { firedAt: number }[] } }>(
      `{ poolHealth(poolId: "${POOL}") { alertsFired { firedAt } } }`,
    );
    const fired = data.poolHealth.alertsFired;
    assert.ok(fired.length > 0);
    assert.ok(fired.every((a) => a.firedAt < 1e11));
    defaultIndexer.removeAlertConfig(POOL, "volume24h");
  });
});
