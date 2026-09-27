import { ApolloServer } from "@apollo/server";
import { GraphQLError } from "graphql";
import { startStandaloneServer } from "@apollo/server/standalone";
import { PubSub } from "graphql-subscriptions";
import { typeDefs } from "./schema.js";
import {
  defaultIndexer,
  InvalidMetricError,
  InvalidThresholdError,
  type AlertConfig,
  type AlertMetric,
} from "./indexer.js";

const pubsub = new PubSub();

const resolvers = {
  Query: {
    poolStats: (_: unknown, { poolId }: { poolId?: string }) =>
      defaultIndexer.getPoolStats(poolId),
    poolEvents: (
      _: unknown,
      { poolId, limit }: { poolId?: string; limit?: number },
    ) => defaultIndexer.getEvents(poolId, limit ?? 100),
    positions: (_: unknown, { owner }: { owner?: string }) =>
      defaultIndexer.getPositions(owner),
    priceHistory: (
      _: unknown,
      { poolId, from, to }: { poolId: string; from?: number; to?: number },
    ) => defaultIndexer.getPriceHistory(poolId, from, to),
    twal: (
      _: unknown,
      { poolId, windowSeconds }: { poolId: string; windowSeconds: number },
    ) => {
      if (windowSeconds <= 0) {
        throw new GraphQLError("windowSeconds must be greater than 0", {
          extensions: { code: "BAD_USER_INPUT" },
        });
      }
      return defaultIndexer.getTwal(poolId, windowSeconds);
    },
    poolHealth: (_: unknown, { poolId }: { poolId: string }) =>
      defaultIndexer.getPoolHealth(poolId),
    alertConfigs: (_: unknown, { poolId }: { poolId?: string }) =>
      defaultIndexer.getAlertConfigs(poolId),
  },
  Mutation: {
    setAlertConfig: (
      _: unknown,
      {
        poolId,
        metric,
        thresholdBps,
        thresholdValue,
      }: {
        poolId: string;
        metric: string;
        thresholdBps?: number;
        thresholdValue?: number;
      },
    ): AlertConfig => {
      try {
        return defaultIndexer.setAlertConfig({
          poolId,
          metric: metric as AlertMetric,
          thresholdBps,
          thresholdValue,
        });
      } catch (error) {
        if (error instanceof InvalidMetricError || error instanceof InvalidThresholdError) {
          throw new GraphQLError(error.message, {
            extensions: { code: "BAD_USER_INPUT" },
          });
        }
        throw error;
      }
    },
    removeAlertConfig: (
      _: unknown,
      { poolId, metric }: { poolId: string; metric: string },
    ): boolean => defaultIndexer.removeAlertConfig(poolId, metric),
  },
  PoolEvent: {
    payload: (parent: { payload: Record<string, unknown> }) =>
      JSON.stringify(parent.payload),
  },
  Subscription: {
    poolEvent: {
      subscribe: (_: unknown, { poolId }: { poolId?: string }) =>
        pubsub.asyncIterator(poolId ? `EVENT:${poolId}` : "EVENT:ALL"),
    },
  },
};

export async function startServer(port = 4000) {
  const server = new ApolloServer({ typeDefs, resolvers });
  const { url } = await startStandaloneServer(server, {
    listen: { port },
  });
  return url;
}

startServer().then(
  (url) => console.log(`GraphQL API ready at ${url}`),
  (error: unknown) => {
    console.error("GraphQL API failed to start:", error);
    process.exitCode = 1;
  },
);
