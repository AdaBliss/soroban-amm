/**
 * Unit tests for the graphql-api indexer (issue #854).
 * Run with: node --test dist/indexer.test.js
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  PoolIndexer,
  RETENTION_SECONDS,
  InvalidMetricError,
  InvalidThresholdError,
  seedDemoData,
  type PoolEvent,
} from "./indexer.js";
import { asSeconds, type Seconds } from "./time.js";

/** Fixed "now" (seconds) so tests don't depend on the wall clock. */
const NOW = asSeconds(1_800_000_000);

/** `NOW` shifted by `offsetSeconds`. */
function at(offsetSeconds: number): Seconds {
  return asSeconds(NOW + offsetSeconds);
}

function makeIndexer() {
  return new PoolIndexer(() => NOW, {
    p1: { tokenA: "XLM", tokenB: "USDC", tokenPrices: { XLM: 1, USDC: 1 } },
  });
}

function swapEvent(
  overrides: Partial<PoolEvent> & { poolId: string; id: string },
): PoolEvent {
  return {
    type: "swap",
    timestamp: NOW,
    payload: { amountIn: 500, amountOut: 600, fee: 5, token_in: "XLM", token_out: "USDC" },
    ...overrides,
  };
}

function addLiquidityEvent(
  overrides: Partial<PoolEvent> & { poolId: string; id: string },
): PoolEvent {
  return {
    type: "add_liquidity",
    timestamp: NOW,
    payload: { amountA: 10_000, amountB: 20_000, tokenA: "XLM", tokenB: "USDC" },
    ...overrides,
  };
}

function removeLiquidityEvent(
  overrides: Partial<PoolEvent> & { poolId: string; id: string },
): PoolEvent {
  return {
    type: "remove_liquidity",
    timestamp: NOW,
    payload: { amountA: 5_000, amountB: 10_000, tokenA: "XLM", tokenB: "USDC" },
    ...overrides,
  };
}

// ── Event indexing ────────────────────────────────────────────────────────

describe("indexEvent", () => {
  it("swap increments swapCount and accounts volume and fees in quote token", () => {
    const idx = makeIndexer();
    idx.indexEvent(swapEvent({ id: "s1", poolId: "p1" }));
    idx.indexEvent(
      swapEvent({ id: "s2", poolId: "p1", payload: { amountIn: 300, amountOut: 360, fee: 3, token_in: "XLM", token_out: "USDC" } }),
    );

    const [stats] = idx.getPoolStats("p1");
    assert.equal(stats.swapCount, 2);
    assert.equal(stats.volume24h, 960);
    assert.equal(stats.fees24h, 9.6);
  });

  it("swap records price data into history", () => {
    const idx = makeIndexer();
    idx.indexEvent(swapEvent({ id: "s1", poolId: "p1" }));

    const prices = idx.getPriceHistory("p1");
    assert.equal(prices.length, 1);
    assert.equal(prices[0].price, 1.2);
  });

  it("add_liquidity tracks reserves and prices TVL", () => {
    const idx = makeIndexer();
    idx.indexEvent(addLiquidityEvent({ id: "a1", poolId: "p1" }));

    const [stats] = idx.getPoolStats("p1");
    assert.equal(stats.reserveA, 10_000);
    assert.equal(stats.reserveB, 20_000);
    assert.equal(stats.tvl, 30_000);
  });

  it("remove_liquidity decreases tvl without going below zero", () => {
    const idx = makeIndexer();
    idx.indexEvent(addLiquidityEvent({ id: "a1", poolId: "p1" }));
    idx.indexEvent(removeLiquidityEvent({ id: "r1", poolId: "p1" }));

    let [stats] = idx.getPoolStats("p1");
    assert.equal(stats.tvl, 15_000);

    // Remove more than available; tvl should clamp to 0.
    idx.indexEvent(
      removeLiquidityEvent({
        id: "r2",
        poolId: "p1",
        payload: { amountA: 100_000, amountB: 100_000, tokenA: "XLM", tokenB: "USDC" },
      }),
    );
    [stats] = idx.getPoolStats("p1");
    assert.equal(stats.tvl, 0);
  });
});

// ── Retention pruning ────────────────────────────────────────────────────

describe("retention pruning", () => {
  it("drops events older than RETENTION_MS", () => {
    const idx = makeIndexer();

    idx.indexEvent(swapEvent({ id: "fresh", poolId: "p1", timestamp: NOW }));
    idx.indexEvent(
      swapEvent({ id: "old", poolId: "p1", timestamp: at(-RETENTION_SECONDS - 1) }),
    );

    const events = idx.getEvents("p1");
    assert.equal(events.length, 1);
    assert.equal(events[0].id, "fresh");
  });

  it("prunes stale price history on the next indexEvent", () => {
    const idx = makeIndexer();

    idx.recordPrice({ poolId: "p1", timestamp: at(-RETENTION_SECONDS - 1), price: 1, feeBps: 30 });
    // Indexing a new event triggers pruneOldData.
    idx.indexEvent(swapEvent({ id: "trigger", poolId: "p1", timestamp: NOW }));

    const prices = idx.getPriceHistory("p1");
    // The stale price point should have been removed; only the one from the swap remains.
    assert.ok(prices.length <= 1, "old price point should be pruned");
  });
});

// ── Health alert generation ──────────────────────────────────────────────

describe("health alerts", () => {
  it("fires a HealthAlert when a metric crosses its threshold", () => {
    const idx = makeIndexer();

    idx.setAlertConfig({
      poolId: "p1",
      metric: "volume24h",
      thresholdValue: 100,
    });

    idx.indexEvent(
      swapEvent({ id: "s1", poolId: "p1", payload: { amountIn: 500, amountOut: 600, fee: 5, token_in: "XLM", token_out: "USDC" } }),
    );

    const health = idx.getPoolHealth("p1");
    assert.ok(health, "pool health should exist");
    assert.equal(health.alertsFired.length, 1);
    assert.equal(health.alertsFired[0].metric, "volume24h");
    assert.equal(health.alertsFired[0].threshold, 100);
    assert.equal(health.alertsFired[0].currentValue, 600);
  });

  it("does not fire when the metric stays below the threshold", () => {
    const idx = makeIndexer();

    idx.setAlertConfig({
      poolId: "p1",
      metric: "volume24h",
      thresholdValue: 10_000,
    });

    idx.indexEvent(
      swapEvent({ id: "s1", poolId: "p1", payload: { amountIn: 500, fee: 5, price: 1.2, tokenA: "XLM", tokenB: "USDC" } }),
    );

    const health = idx.getPoolHealth("p1");
    assert.equal(health!.alertsFired.length, 0);
  });
});

describe("analytics correctness", () => {
  it("keeps reciprocal swap directions at the same canonical price", () => {
    const idx = makeIndexer();
    idx.indexEvent(swapEvent({ id: "ab", poolId: "p1" }));
    idx.indexEvent(swapEvent({
      id: "ba",
      poolId: "p1",
      payload: { amountIn: 600, amountOut: 500, fee: 6, token_in: "USDC", token_out: "XLM" },
    }));
    assert.deepEqual(idx.getPriceHistory("p1").map((point) => point.price), [1.2, 1.2]);
    assert.equal(idx.getPoolStats("p1")[0].priceDeviationBps, 0);
  });

  it("returns null TVL without a configured price source", () => {
    const idx = new PoolIndexer(() => NOW);
    idx.indexEvent(addLiquidityEvent({ id: "a1", poolId: "unpriced" }));
    assert.equal(idx.getPoolStats("unpriced")[0].tvl, null);
  });

  it("does not apply the same event key twice", () => {
    const idx = makeIndexer();
    const event = swapEvent({ id: "same", poolId: "p1", ledger: 9, txHash: "tx", eventIndex: 1 });
    idx.indexEvent(event);
    idx.indexEvent({ ...event, id: "different-rpc-id" });
    assert.equal(idx.getPoolStats("p1")[0].swapCount, 1);
  });

  it("drops swap volume and fees after the trailing 24-hour window", () => {
    let now = NOW;
    const idx = new PoolIndexer(() => now, {
      p1: { tokenA: "XLM", tokenB: "USDC" },
    });
    idx.indexEvent(swapEvent({ id: "old", poolId: "p1", timestamp: at(-23 * 60 * 60) }));
    assert.equal(idx.getPoolStats("p1")[0].volume24h, 600);
    now = at(2 * 60 * 60);
    assert.equal(idx.getPoolStats("p1")[0].volume24h, 0);
    assert.equal(idx.getPoolStats("p1")[0].fees24h, 0);
  });

  it("derives AMM and concentrated-liquidity positions", () => {
    const idx = makeIndexer();
    idx.indexEvent(addLiquidityEvent({
      id: "lp", poolId: "p1", payload: { amountA: 10, amountB: 20, shares: 5, provider: "alice" },
    }));
    idx.indexEvent({
      id: "cl", poolId: "p1", type: "mint_pos", timestamp: NOW,
      payload: { amountA: 4, amountB: 8, liquidity: 9, owner: "bob", lowerTick: -10, upperTick: 10 },
    });
    const positions = idx.getPositions();
    assert.equal(positions.find((position) => position.owner === "alice")?.shares, 5);
    assert.equal(positions.find((position) => position.owner === "bob")?.lowerTick, -10);
  });

  it("surfaces when the pool fee came from the explicit default", () => {
    const idx = new PoolIndexer(() => NOW, { p1: { tokenA: "XLM", tokenB: "USDC" } });
    idx.indexEvent(swapEvent({ id: "fee", poolId: "p1" }));
    assert.equal(idx.getPoolStats("p1")[0].feeBpsSource, "default");
    assert.equal(idx.getPriceHistory("p1")[0].feeBpsSource, "default");
  });
});

// ── Alert config validation ──────────────────────────────────────────────

describe("alert config validation", () => {
  it("rejects an unrecognized metric", () => {
    const idx = makeIndexer();
    assert.throws(
      () =>
        idx.setAlertConfig({
          poolId: "pool-a",
          metric: "not_a_real_metric" as never,
          thresholdValue: 10,
        }),
      InvalidMetricError,
    );
    assert.deepEqual(idx.getAlertConfigs("pool-a"), []);
  });

  it("rejects a negative threshold", () => {
    const idx = makeIndexer();
    assert.throws(
      () =>
        idx.setAlertConfig({
          poolId: "pool-a",
          metric: "tvl",
          thresholdValue: -5,
        }),
      InvalidThresholdError,
    );
    assert.deepEqual(idx.getAlertConfigs("pool-a"), []);
  });

  it("accepts each valid metric and getAlertConfigs returns it", () => {
    const idx = makeIndexer();
    idx.setAlertConfig({ poolId: "pool-a", metric: "price_deviation", thresholdBps: 100 });
    idx.setAlertConfig({ poolId: "pool-a", metric: "tvl", thresholdValue: 500 });
    idx.setAlertConfig({ poolId: "pool-a", metric: "volume24h", thresholdValue: 1000 });

    const configs = idx.getAlertConfigs("pool-a");
    assert.equal(configs.length, 3);
    assert.ok(configs.some((c) => c.metric === "price_deviation" && c.thresholdBps === 100));
    assert.ok(configs.some((c) => c.metric === "tvl" && c.thresholdValue === 500));
    assert.ok(configs.some((c) => c.metric === "volume24h" && c.thresholdValue === 1000));
  });

  it("requires the correct threshold field for the given metric", () => {
    const idx = makeIndexer();
    assert.throws(
      () => idx.setAlertConfig({ poolId: "pool-a", metric: "price_deviation" }),
      InvalidThresholdError,
    );
    assert.throws(
      () => idx.setAlertConfig({ poolId: "pool-a", metric: "tvl" }),
      InvalidThresholdError,
    );
  });
});

// ── TWAL ─────────────────────────────────────────────────────────────────

describe("getTwal", () => {
  it("returns null when there is no price history in the window", () => {
    const idx = makeIndexer();
    assert.equal(idx.getTwal("pool-x", 3600), null);
  });

  it("returns a time-weighted value that differs from the naive mean for unevenly spaced points", () => {
    const idx = makeIndexer();
    // Price held at 1.0 for a long stretch, then briefly spikes to 2.0 just before "now".
    idx.recordPrice({ poolId: "pool-x", timestamp: at(-3000), price: 1.0, feeBps: 30 });
    idx.recordPrice({ poolId: "pool-x", timestamp: at(-100), price: 2.0, feeBps: 30 });

    const twal = idx.getTwal("pool-x", 3600);
    const naiveMean = (1.0 + 2.0) / 2;

    assert.notEqual(twal, null);
    assert.notEqual(twal, naiveMean);
    // The 1.0 price dominates the window (2900s vs 100s), so twal should be close to 1.0.
    assert.ok(twal! < naiveMean);
  });

  it("ignores price points outside the requested window", () => {
    const idx = makeIndexer();
    idx.recordPrice({ poolId: "pool-x", timestamp: at(-7200), price: 5.0, feeBps: 30 });
    idx.recordPrice({ poolId: "pool-x", timestamp: at(-10), price: 1.0, feeBps: 30 });

    const twal = idx.getTwal("pool-x", 60);
    assert.equal(twal, 1.0);
  });

  it("is scoped to the given poolId", () => {
    const idx = makeIndexer();
    idx.recordPrice({ poolId: "pool-a", timestamp: at(-10), price: 1.0, feeBps: 30 });
    assert.equal(idx.getTwal("pool-b", 3600), null);
  });
});

// ── getEvents / positions ────────────────────────────────────────────────

describe("getEvents", () => {
  it("returns events in reverse chronological order with a limit", () => {
    const idx = makeIndexer();
    idx.indexEvent(swapEvent({ id: "e1", poolId: "p1", timestamp: at(-3000) }));
    idx.indexEvent(swapEvent({ id: "e2", poolId: "p1", timestamp: at(-2000) }));
    idx.indexEvent(swapEvent({ id: "e3", poolId: "p1", timestamp: at(-1000) }));

    const last2 = idx.getEvents("p1", 2);
    assert.equal(last2.length, 2);
    assert.equal(last2[0].id, "e3");
    assert.equal(last2[1].id, "e2");
  });
});

describe("positions", () => {
  it("upserts and queries positions by owner", () => {
    const idx = makeIndexer();
    idx.upsertPosition({ id: "pos1", poolId: "p1", owner: "alice", shares: 100, valueUsd: 500 });
    idx.upsertPosition({ id: "pos2", poolId: "p1", owner: "bob", shares: 200, valueUsd: 1000 });

    assert.equal(idx.getPositions().length, 2);
    const alicePos = idx.getPositions("alice");
    assert.equal(alicePos.length, 1);
    assert.equal(alicePos[0].shares, 100);
  });
});

describe("demo data", () => {
  it("is opt-in and remains disabled in production", () => {
    const previousSeed = process.env.SEED_DEMO_DATA;
    const previousNodeEnv = process.env.NODE_ENV;
    try {
      delete process.env.SEED_DEMO_DATA;
      const disabled = makeIndexer();
      seedDemoData(disabled);
      assert.equal(disabled.getEvents().length, 0);

      process.env.SEED_DEMO_DATA = "true";
      process.env.NODE_ENV = "production";
      const production = makeIndexer();
      seedDemoData(production);
      assert.equal(production.getEvents().length, 0);
    } finally {
      if (previousSeed === undefined) delete process.env.SEED_DEMO_DATA;
      else process.env.SEED_DEMO_DATA = previousSeed;
      if (previousNodeEnv === undefined) delete process.env.NODE_ENV;
      else process.env.NODE_ENV = previousNodeEnv;
    }
  });
});
