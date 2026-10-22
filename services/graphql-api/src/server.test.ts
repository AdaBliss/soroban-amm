import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { createClient } from "graphql-ws";
import WebSocket from "ws";
import { startServer } from "./index.js";
import { asSeconds } from "./time.js";

describe("GraphQL transport", () => {
  it("serves health and publishes ingested events over graphql-ws", async (t) => {
    const previousStore = process.env.STORE_TYPE;
    process.env.STORE_TYPE = "memory";
    const running = await startServer(0);
    t.after(async () => {
      if (previousStore === undefined) delete process.env.STORE_TYPE;
      else process.env.STORE_TYPE = previousStore;
      await running.stop();
    });
    const address = running.httpServer.address();
    assert.ok(address && typeof address === "object");

    const health = await fetch(`http://localhost:${address.port}/health`);
    assert.equal(health.status, 200);
    assert.equal((await health.json() as { status: string }).status, "ok");

    const client = createClient({
      url: `ws://localhost:${address.port}/graphql`,
      webSocketImpl: WebSocket,
    });
    const received = new Promise<Record<string, unknown>>((resolve, reject) => {
      const unsubscribe = client.subscribe(
        { query: "subscription { poolEvent(poolId: \"pool-live\") { id poolId type } }" },
        {
          next: (result) => {
            resolve(result.data as Record<string, unknown>);
            unsubscribe();
          },
          error: reject,
          complete: () => {},
        },
      );
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    await running.store.appendEvent({
      id: "live-1", poolId: "pool-live", type: "swap", timestamp: asSeconds(100),
      ledger: 1, txHash: "tx-live", eventIndex: 0,
      payload: { amountIn: 10, amountOut: 9, token_in: "A", token_out: "B" },
    });
    await running.persistUpdate({
      id: "live-1", poolId: "pool-live", type: "swap", timestamp: asSeconds(100),
      ledger: 1, txHash: "tx-live", eventIndex: 0,
      payload: { amountIn: 10, amountOut: 9, token_in: "A", token_out: "B" },
    });
    const data = await received;
    assert.deepEqual(data, { poolEvent: { id: "live-1", poolId: "pool-live", type: "swap" } });
    await client.dispose();
  });
});
