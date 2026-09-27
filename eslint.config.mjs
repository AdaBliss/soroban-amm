// Shared ESLint configuration for every JavaScript/TypeScript workspace in
// this repository (issue #957).
//
// The Rust side runs `cargo clippy -- -D warnings`; this is its counterpart for
// the code that faces the network. Every workspace runs `eslint .` from its own
// directory, ESLint walks up to this file, and the tooling itself is installed
// once from the root package.json, so there is exactly one config and one set
// of lint dependencies to keep up to date.
//
// Type-aware rules resolve each file against the nearest tsconfig.json through
// the typescript-eslint project service, so every workspace is linted with its
// own compiler options. Linting uses the TypeScript pinned in the root
// package.json, which is independent of the compiler a workspace builds with.
//
// Every rule below is an error. There is no warn-only tier: a rule is either
// enforced or deliberately switched off here with a reason next to it.

import js from "@eslint/js";
import { defineConfig, globalIgnores } from "eslint/config";
import globals from "globals";
import tseslint from "typescript-eslint";

export default defineConfig(
  globalIgnores([
    "**/node_modules/",
    "**/dist/",
    "**/coverage/",
    // ts-advanced-client compiles to lib/ rather than dist/.
    "packages/ts-advanced-client/lib/",
  ]),

  js.configs.recommended,
  tseslint.configs.recommendedTypeChecked,

  {
    linterOptions: {
      reportUnusedDisableDirectives: "error",
    },
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "module",
      globals: { ...globals.node },
      parserOptions: {
        projectService: {
          // Config and test files that sit outside a workspace's tsconfig
          // "include" are still type-checked, against default options.
          allowDefaultProject: [
            "packages/ui-components/vitest.config.ts",
            "packages/ts-advanced-client/test/*.js",
          ],
        },
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      // An intentionally unused binding must say so with a leading underscore.
      "@typescript-eslint/no-unused-vars": [
        "error",
        {
          argsIgnorePattern: "^_",
          varsIgnorePattern: "^_",
          caughtErrorsIgnorePattern: "^_",
          destructuredArrayIgnorePattern: "^_",
        },
      ],
      // An `async` function without `await` is how a synchronous
      // implementation satisfies a promise-returning interface (MemoryStore
      // implementing AnalyticsStore, test doubles standing in for fetch or an
      // RPC server). The hazards this rule proxies for are enforced directly
      // by no-floating-promises, no-misused-promises and await-thenable.
      "@typescript-eslint/require-await": "off",
      // node:test's describe/it/test return promises that the runner itself
      // tracks and awaits; awaiting them by hand is neither required nor
      // idiomatic. Every other floating promise is still an error.
      "@typescript-eslint/no-floating-promises": [
        "error",
        {
          allowForKnownSafeCalls: [
            {
              from: "package",
              package: "node:test",
              name: ["describe", "it", "test", "suite"],
            },
          ],
        },
      ],
    },
  },

  // packages/sdk keeps its tests out of tsconfig.json (so they are not
  // emitted) and type-checks them through tsconfig.test.json instead. Lint
  // against the latter so tests and sources share one program.
  {
    files: ["packages/sdk/**/*.ts"],
    languageOptions: {
      parserOptions: {
        projectService: false,
        project: "./packages/sdk/tsconfig.test.json",
      },
    },
  },

  // Plain JavaScript has no type annotations, so every parameter and every
  // parsed JSON value is `any` by construction rather than by escape hatch.
  // The no-unsafe-* family would flag nearly every line; the rest of the
  // type-aware set (floating/misused promises, await-thenable, ...) still
  // applies.
  {
    files: ["**/*.js"],
    rules: {
      "@typescript-eslint/no-unsafe-argument": "off",
      "@typescript-eslint/no-unsafe-assignment": "off",
      "@typescript-eslint/no-unsafe-call": "off",
      "@typescript-eslint/no-unsafe-member-access": "off",
      "@typescript-eslint/no-unsafe-return": "off",
    },
  },

  // ts-advanced-client is a CommonJS package; its tests load the compiled
  // output with require().
  {
    files: ["packages/ts-advanced-client/test/**/*.js"],
    languageOptions: { sourceType: "commonjs" },
    rules: { "@typescript-eslint/no-require-imports": "off" },
  },

  // The dashboard runs in the browser; its tests and DOM stub run under Node.
  {
    files: ["services/health-dashboard/src/dashboard.js"],
    languageOptions: { globals: { ...globals.browser } },
  },

  // The configuration file itself is plain Node ESM outside every workspace.
  {
    files: ["eslint.config.mjs"],
    ...tseslint.configs.disableTypeChecked,
  },
);
