/**
 * Resolver tests through a real ApolloServer (issue #984).
 *
 * `executeOperation` runs the same request pipeline and error formatting a
 * client gets over HTTP, so these assert what clients actually receive: the
 * GraphQL error `message` and `extensions.code`. They pin that contract ahead
 * of the graphql 17 upgrade, which changes how graphql-js builds errors.
 * Run with: node --test dist/resolvers.test.js
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { ApolloServer } from "@apollo/server";
import { typeDefs } from "./schema.js";
import { pubsub, resolvers } from "./resolvers.js";

const server = new ApolloServer({ typeDefs, resolvers });

interface ClientError {
  message: string;
  extensions?: { code?: unknown };
}

/** Executes `query` and returns the errors a client would see. */
async function clientErrors(
  query: string,
  variables: Record<string, unknown> = {},
): Promise<ClientError[]> {
  const response = await server.executeOperation({ query, variables });
  assert.equal(response.body.kind, "single");
  if (response.body.kind !== "single") return [];
  return (response.body.singleResult.errors ?? []) as ClientError[];
}

function assertBadUserInput(errors: ClientError[], message: RegExp) {
  assert.equal(errors.length, 1, `expected one error, got ${JSON.stringify(errors)}`);
  assert.equal(errors[0].extensions?.code, "BAD_USER_INPUT");
  assert.match(errors[0].message, message);
}

describe("BAD_USER_INPUT reaches clients", () => {
  it("twal rejects a non-positive windowSeconds", async () => {
    for (const windowSeconds of [0, -60]) {
      const errors = await clientErrors(
        "query ($w: Int!) { twal(poolId: \"pool-1\", windowSeconds: $w) }",
        { w: windowSeconds },
      );
      assertBadUserInput(errors, /windowSeconds must be greater than 0/);
    }
  });

  it("setAlertConfig rejects an unknown metric", async () => {
    const errors = await clientErrors(
      "mutation { setAlertConfig(poolId: \"pool-1\", metric: \"nope\", thresholdValue: 1) { poolId } }",
    );
    assertBadUserInput(errors, /Invalid alert metric "nope"/);
  });

  it("setAlertConfig rejects a missing or negative threshold", async () => {
    assertBadUserInput(
      await clientErrors(
        "mutation { setAlertConfig(poolId: \"pool-1\", metric: \"price_deviation\") { poolId } }",
      ),
      /thresholdBps is required/,
    );
    assertBadUserInput(
      await clientErrors(
        "mutation { setAlertConfig(poolId: \"pool-1\", metric: \"price_deviation\", thresholdBps: -1) { poolId } }",
      ),
      /Threshold must be >= 0/,
    );
  });

  it("valid input returns data and no errors", async () => {
    const response = await server.executeOperation({
      query:
        "mutation { setAlertConfig(poolId: \"pool-ok\", metric: \"price_deviation\", thresholdBps: 50) { poolId metric thresholdBps } }",
    });
    assert.equal(response.body.kind, "single");
    if (response.body.kind !== "single") return;
    assert.equal(response.body.singleResult.errors, undefined);
    // Compare the serialized form a client receives; graphql-js builds the
    // result from null-prototype objects.
    assert.deepEqual(JSON.parse(JSON.stringify(response.body.singleResult.data)), {
      setAlertConfig: { poolId: "pool-ok", metric: "price_deviation", thresholdBps: 50 },
    });
  });
});

describe("poolEvent subscription", () => {
  it("delivers events published on the pool's channel", async () => {
    const iterator = resolvers.Subscription.poolEvent.subscribe(undefined, {
      poolId: "pool-7",
    });
    // graphql-subscriptions 3 returns an AsyncIterableIterator, which is
    // what Apollo Server's subscription support iterates.
    assert.equal(typeof iterator[Symbol.asyncIterator], "function");
    const next = iterator.next();
    const event = { poolEvent: { id: "evt-1", poolId: "pool-7" } };
    await pubsub.publish("EVENT:pool-7", event);
    assert.deepEqual(await next, { value: event, done: false });
    await iterator.return?.();
  });

  it("listens on EVENT:ALL when no poolId is given", async () => {
    const iterator = resolvers.Subscription.poolEvent.subscribe(undefined, {});
    const next = iterator.next();
    const event = { poolEvent: { id: "evt-2", poolId: "pool-8" } };
    await pubsub.publish("EVENT:ALL", event);
    assert.deepEqual(await next, { value: event, done: false });
    await iterator.return?.();
  });
});
