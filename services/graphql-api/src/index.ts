import { ApolloServer } from "@apollo/server";
import { startStandaloneServer } from "@apollo/server/standalone";
import { typeDefs } from "./schema.js";
import { resolvers } from "./resolvers.js";

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
