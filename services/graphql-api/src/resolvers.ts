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
import { asSeconds } from "./time.js";
import type { AnalyticsStore } from "./store/interface.js";
import type { PoolIndexer } from "./indexer.js";

export interface GraphQLContext {
  [key: string]: unknown;
  indexer: PoolIndexer;
  store?: AnalyticsStore;
}

function activeIndexer(context?: Partial<GraphQLContext>): PoolIndexer {
  return context?.indexer ?? defaultIndexer;
}

export const pubsub = new PubSub();

export const resolvers = {
  Query: {
    poolStats: (_: unknown, { poolId }: { poolId?: string }, context?: GraphQLContext) =>
      activeIndexer(context).getPoolStats(poolId),
    poolEvents: (
      _: unknown,
      { poolId, limit }: { poolId?: string; limit?: number },
      context?: GraphQLContext,
    ) => activeIndexer(context).getEvents(poolId, limit ?? 100),
    positions: (_: unknown, { owner }: { owner?: string }, context?: GraphQLContext) =>
      activeIndexer(context).getPositions(owner),
    priceHistory: (
      _: unknown,
      { poolId, from, to }: { poolId: string; from?: number; to?: number },
      context?: GraphQLContext,
    ) =>
      activeIndexer(context).getPriceHistory(
        poolId,
        from === undefined ? undefined : asSeconds(from),
        to === undefined ? undefined : asSeconds(to),
      ),
    twal: (
      _: unknown,
      { poolId, windowSeconds }: { poolId: string; windowSeconds: number },
      context?: GraphQLContext,
    ) => {
      if (windowSeconds <= 0) {
        throw new GraphQLError("windowSeconds must be greater than 0", {
          extensions: { code: "BAD_USER_INPUT" },
        });
      }
      return activeIndexer(context).getTwal(poolId, windowSeconds);
    },
    poolHealth: (_: unknown, { poolId }: { poolId: string }, context?: GraphQLContext) =>
      activeIndexer(context).getPoolHealth(poolId),
    alertConfigs: (_: unknown, { poolId }: { poolId?: string }, context?: GraphQLContext) =>
      activeIndexer(context).getAlertConfigs(poolId),
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
      context?: GraphQLContext,
    ): AlertConfig | Promise<AlertConfig> => {
      try {
        const config = activeIndexer(context).setAlertConfig({
          poolId,
          metric: metric as AlertMetric,
          thresholdBps,
          thresholdValue,
        });
        return context?.store
          ? context.store.setAlertConfig(config).then(() => config)
          : config;
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
      context?: GraphQLContext,
    ): boolean | Promise<boolean> => {
      const removed = activeIndexer(context).removeAlertConfig(poolId, metric);
      return context?.store
        ? context.store.removeAlertConfig(poolId, metric).then((stored) => removed || stored)
        : removed;
    },
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
