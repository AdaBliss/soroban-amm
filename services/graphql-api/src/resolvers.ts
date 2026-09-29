/**
 * GraphQL resolvers, kept apart from `index.ts` so tests can build a server
 * from them without starting one (`index.ts` listens on import).
 */
import { GraphQLError } from "graphql";
import { PubSub } from "graphql-subscriptions";
import {
  defaultIndexer,
  InvalidMetricError,
  InvalidThresholdError,
  type AlertConfig,
  type AlertMetric,
} from "./indexer.js";

export const pubsub = new PubSub();

export const resolvers = {
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
        pubsub.asyncIterableIterator(poolId ? `EVENT:${poolId}` : "EVENT:ALL"),
    },
  },
};
