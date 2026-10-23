import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    // Ensure the suite and the package scripts never touch the network.
    pool: "forks",
    coverage: {
      provider: "v8",
      reporter: ["text", "json-summary", "html"],
      // Measure every shipped source file rather than a hand-maintained list:
      // the lib modules used to need their own entry because nothing imported
      // them. `scripts/check_reachability.mjs` now guarantees they are part of
      // the graph, so the include no longer names them.
      include: ["src/**/*.{ts,tsx}"],
      exclude: ["src/test/**", "src/**/*.d.ts"],
      thresholds: {
        // Enforced rather than merely reported: a drop below these fails the
        // `test:coverage` run.
        lines: 70,
        functions: 60,
        statements: 70,
        branches: 55,
      },
    },
  },
});
