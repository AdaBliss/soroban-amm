#!/usr/bin/env node
// Fails when a non-test module under a package's `src/` is not reachable from
// that package's public entrypoint (issue #1168).
//
// The ui-components package had three "pure logic" modules under `src/lib/`
// with their own unit tests, but no component imported them: the components
// carried private copies, the tests exercised code that never shipped, and the
// coverage gate counted the dead modules. `tsc` and `eslint` cannot catch that
// — an unreferenced file is perfectly valid on its own — so it has to be
// checked rather than left to review.
//
//   node scripts/check_reachability.mjs
//
// The walk starts at `src/index.ts`, follows every static `import` / `export
// ... from` (including `import type` and dynamic `import()`), and reports any
// module that is never reached. Test files (`*.test.*`), declaration files and
// the entrypoint's own support files are ignored.

import { readFileSync, readdirSync, existsSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Packages whose `src/` is checked, with the entrypoint that must reach all of it. */
export const PACKAGES = [
  { name: "ui-components", entry: "packages/ui-components/src/index.ts" },
];

const MODULE_EXTENSIONS = [".ts", ".tsx", ".mts", ".js", ".jsx", ".mjs"];

/** True for files that are not part of the shipped graph. */
function isIgnored(relPath) {
  const segments = relPath.split("/");
  const base = segments[segments.length - 1] ?? "";
  return (
    base.includes(".test.") ||
    base.includes(".spec.") ||
    base.endsWith(".d.ts") ||
    base.endsWith(".stories.tsx") ||
    // Test-support directories (vitest setup, fixtures, harnesses).
    segments.includes("test") ||
    segments.includes("__tests__") ||
    segments.includes("__mocks__")
  );
}

/** Resolves a relative specifier ("./x.js") to the file that actually exists. */
function resolveSpecifier(fromFile, spec) {
  const base = resolve(dirname(fromFile), spec);
  const candidates = [];
  if (/\.[cm]?[jt]sx?$/.test(base)) {
    // A `.js` specifier in TypeScript source refers to the `.ts`/`.tsx` file.
    candidates.push(base.replace(/\.js$/, ".ts"), base.replace(/\.js$/, ".tsx"), base);
  } else {
    for (const ext of MODULE_EXTENSIONS) candidates.push(base + ext);
    for (const ext of MODULE_EXTENSIONS) candidates.push(join(base, "index" + ext));
  }
  for (const c of candidates) {
    if (existsSync(c) && statSync(c).isFile()) return c;
  }
  return null;
}

// Static imports/exports and dynamic import("..."), including type-only forms.
const IMPORT_RE =
  /(?:^|[^\w$])(?:import|export)\s*(?:type\s*)?(?:[\s\S]*?\sfrom\s*)?["']([^"']+)["']|(?:^|[^\w$])import\s*\(\s*["']([^"']+)["']\s*\)/g;

/** Collects the module specifiers a file depends on. */
export function importsOf(file) {
  const source = readFileSync(file, "utf8");
  const specs = new Set();
  for (const match of source.matchAll(IMPORT_RE)) {
    const spec = match[1] ?? match[2];
    if (spec) specs.add(spec);
  }
  return [...specs];
}

/** Walks the import graph from `entry` and returns every reachable file. */
export function reachableModules(entry) {
  const reachable = new Set();
  const queue = [resolve(entry)];
  while (queue.length > 0) {
    const file = queue.pop();
    if (reachable.has(file)) continue;
    reachable.add(file);
    for (const spec of importsOf(file)) {
      if (!spec.startsWith(".")) continue; // bare specifier: a dependency
      const resolved = resolveSpecifier(file, spec);
      if (resolved && !reachable.has(resolved)) queue.push(resolved);
    }
  }
  return reachable;
}

/** Every non-test source file under a directory. */
function sourceFiles(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "node_modules" || entry.name === "dist") continue;
      sourceFiles(full, out);
    } else if (/\.[cm]?[jt]sx?$/.test(entry.name)) {
      out.push(full);
    }
  }
  return out;
}

/** Returns a list of human-readable problems; empty means every module is reached. */
export function findOrphanedModules({ name, entry }, root = ROOT) {
  const entryPath = join(root, entry);
  if (!existsSync(entryPath)) {
    return [`${name}: entrypoint ${entry} does not exist`];
  }
  const reachable = reachableModules(entryPath);
  const srcDir = dirname(entryPath);
  const problems = [];
  for (const file of sourceFiles(srcDir)) {
    if (isIgnored(relative(root, file).replaceAll("\\", "/"))) continue;
    if (!reachable.has(file)) {
      problems.push(
        `${name}: ${relative(root, file).replaceAll("\\", "/")} is not reachable from ${entry}`,
      );
    }
  }
  return problems;
}

/** Scans every package in {@link PACKAGES}. */
export function findProblems(root = ROOT, packages = PACKAGES) {
  return packages.flatMap((pkg) => findOrphanedModules(pkg, root));
}

function main() {
  const problems = findProblems();
  if (problems.length > 0) {
    console.error(`Reachability check failed (${problems.length} orphaned module(s)):\n`);
    for (const p of problems) console.error(`  - ${p}`);
    console.error(
      "\nEvery non-test module under src/ must be imported (transitively) from the " +
        "package entrypoint. Wire it into a component or delete it.",
    );
    process.exit(1);
  }
  console.log(
    `Reachability check passed: every module under ${PACKAGES.map((p) => p.entry).join(", ")} is reachable.`,
  );
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) main();
