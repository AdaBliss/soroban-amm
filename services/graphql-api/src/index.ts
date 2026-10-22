import { createServer } from "node:http";
import { pathToFileURL } from "node:url";
import { ApolloServer } from "@apollo/server";
import { ApolloServerPluginDrainHttpServer } from "@apollo/server/plugin/drainHttpServer";
import { expressMiddleware } from "@as-integrations/express5";
import { makeExecutableSchema } from "@graphql-tools/schema";
import cors from "cors";
import express from "express";
import { WebSocketServer } from "ws";
import { useServer } from "graphql-ws/use/ws";
import { typeDefs } from "./schema.js";
import { pubsub, resolvers, type GraphQLContext } from "./resolvers.js";
import { PoolIndexer, RETENTION_SECONDS, seedDemoData, type PoolConfig } from "./indexer.js";
import { MemoryStore } from "./store/memory.js";
import { SqliteStore } from "./store/sqlite.js";
import type { AnalyticsStore, PoolEvent } from "./store/interface.js";
import { RpcIngester } from "./ingest/rpc.js";
import { asSeconds, systemClock } from "./time.js";

function jsonEnv<T>(name: string, fallback: T): T {
  const raw = process.env[name];
  if (!raw) return fallback;
  try {
    return JSON.parse(raw) as T;
  } catch (error) {
    throw new Error(`${name} must contain valid JSON`, { cause: error });
  }
}

function readPoolConfigs(): Record<string, PoolConfig> {
  const configs = jsonEnv<Record<string, PoolConfig>>("POOL_CONFIG_JSON", {});
  const tokenPrices = jsonEnv<Record<string, number>>("TOKEN_PRICES", {});
  for (const config of Object.values(configs)) config.tokenPrices = tokenPrices;
  return configs;
}

function createStore(): AnalyticsStore {
  return process.env.STORE_TYPE === "memory"
    ? new MemoryStore()
    : new SqliteStore(process.env.DATABASE_PATH);
}

export async function startServer(port = 4000) {
  const store = createStore();
  const poolConfigs = readPoolConfigs();
  const indexer = new PoolIndexer(systemClock, poolConfigs);
  await store.prune(asSeconds(systemClock() - RETENTION_SECONDS));
  await indexer.rebuild(store);
  seedDemoData(indexer);

  const rpcUrl = process.env.SOROBAN_RPC_URL;
  const contractIds = (process.env.CONTRACT_IDS ?? "").split(",").map((id) => id.trim()).filter(Boolean);
  const pollIntervalMs = process.env.POLL_INTERVAL_MS ? Number.parseInt(process.env.POLL_INTERVAL_MS, 10) : undefined;
  const startLedger = process.env.START_LEDGER ? Number.parseInt(process.env.START_LEDGER, 10) : undefined;
  let ingester: RpcIngester | undefined;

  const persistUpdate = async (event: PoolEvent): Promise<void> => {
    const update = indexer.indexEvent(event);
    if (!update.inserted) return;
    if (update.stats) await store.upsertPoolStats(update.stats);
    if (update.price) await store.recordPrice(update.price);
    for (const position of indexer.getPositions().filter((item) => item.poolId === event.poolId)) {
      await store.upsertPosition(position);
    }
    for (const alert of update.alerts) await store.recordAlert(alert);
    await store.prune(asSeconds(systemClock() - RETENTION_SECONDS));
    const payload = { poolEvent: event };
    await Promise.all([
      pubsub.publish("EVENT:ALL", payload),
      pubsub.publish(`EVENT:${event.poolId}`, payload),
    ]);
  };

  if (rpcUrl && contractIds.length > 0) {
    const poolFeeBps = Object.fromEntries(
      Object.entries(poolConfigs).flatMap(([poolId, config]) =>
        config.feeBps === undefined ? [] : [[poolId, config.feeBps]]),
    );
    const poolTokens = Object.fromEntries(
      Object.entries(poolConfigs).map(([poolId, config]) => [poolId, { tokenA: config.tokenA, tokenB: config.tokenB }]),
    );
    ingester = new RpcIngester(
      { rpcUrl, contractIds, pollIntervalMs, startLedger, poolFeeBps, poolTokens },
      persistUpdate,
      async (error) => console.error("[RpcIngester error]:", error),
      store,
    );
    ingester.start();
  }

  const schema = makeExecutableSchema({ typeDefs, resolvers });
  const app = express();
  const httpServer = createServer(app);
  const webSocketServer = new WebSocketServer({ server: httpServer, path: "/graphql" });
  const socketCleanup = useServer<GraphQLContext>({
    schema,
    context: async () => ({ indexer, store }),
  }, webSocketServer);
  const server = new ApolloServer<GraphQLContext>({
    schema,
    plugins: [
      ApolloServerPluginDrainHttpServer({ httpServer }),
      { serverWillStart: async () => ({ drainServer: async () => socketCleanup.dispose() }) },
    ],
  });
  await server.start();
  app.get("/health", async (_request, response) => {
    const cursors = await Promise.all(contractIds.map((contractId) => store.getCursor(contractId)));
    response.json({
      status: "ok",
      ingestionLagLedgers: ingester?.getLagLedgers() ?? 0,
      ingestionLagSeconds: ingester?.getLagSeconds() ?? 0,
      cursors: cursors.filter(Boolean),
    });
  });
  app.use("/graphql", cors(), express.json(), expressMiddleware(server, {
    context: async () => ({ indexer, store }),
  }));
  await new Promise<void>((resolve) => httpServer.listen({ port }, resolve));

  const stop = async (): Promise<void> => {
    ingester?.stop();
    await server.stop();
    await store.close();
  };
  return { url: `http://localhost:${port}/graphql`, ingester, store, indexer, httpServer, stop, persistUpdate };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  startServer().then(
    ({ url, stop }) => {
      console.log(`GraphQL API ready at ${url}`);
      const shutdown = () => void stop().catch((error: unknown) => {
        console.error("GraphQL API shutdown failed:", error);
        process.exitCode = 1;
      });
      process.once("SIGINT", shutdown);
      process.once("SIGTERM", shutdown);
    },
    (error: unknown) => {
      console.error("GraphQL API failed to start:", error);
      process.exitCode = 1;
    },
  );
}
