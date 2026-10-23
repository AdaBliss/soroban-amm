import { describe, it, expect } from "vitest";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Guards the point of issue #1168: the lib modules are the single source of
 * truth, and no component may carry a private copy of the logic they own.
 *
 * `tsc` and `eslint` cannot see this — a second copy of a function in another
 * file is perfectly valid TypeScript — so the duplication is checked here.
 */

const SRC = dirname(fileURLToPath(import.meta.url));

/** Every non-test source file in the package's src/, relative to src/. */
function sourceFiles(dir = SRC, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      sourceFiles(full, out);
    } else if (/\.(ts|tsx)$/.test(entry.name) && !/\.test\.(ts|tsx)$/.test(entry.name)) {
      out.push(full);
    }
  }
  return out;
}

/** Files (relative to src/) whose contents match `pattern`. */
function filesMatching(pattern: RegExp): string[] {
  return sourceFiles()
    .filter((f) => pattern.test(readFileSync(f, "utf8")))
    .map((f) => f.slice(SRC.length + 1))
    .sort();
}

describe("single source of truth for shared UI logic", () => {
  it("defines assessPositionRisk in exactly one module", () => {
    expect(filesMatching(/(?:export\s+)?function\s+assessPositionRisk\b/)).toEqual([
      "lib/riskAssessment.ts",
    ]);
  });

  it("defines computeEfficiency in exactly one module", () => {
    expect(filesMatching(/(?:export\s+)?function\s+computeEfficiency\b/)).toEqual([
      "lib/capitalEfficiency.ts",
    ]);
  });

  it("defines validatePosition in exactly one module", () => {
    expect(filesMatching(/(?:export\s+)?function\s+validatePosition\b/)).toEqual([
      "lib/positionValidation.ts",
    ]);
  });

  it("keeps the validation messages only in the lib validator", () => {
    expect(filesMatching(/Amount A must be a positive number/)).toEqual([
      "lib/positionValidation.ts",
    ]);
    expect(filesMatching(/Lower price must be below the upper price/)).toEqual([
      "lib/positionValidation.ts",
    ]);
  });

  it("keeps risk level thresholds only in the lib", () => {
    // A private re-implementation of the score→level mapping was the bug; the
    // comparison operators against the shared thresholds must live in one file.
    expect(filesMatching(/RISK_THRESHOLDS\./)).toEqual(["lib/riskAssessment.ts"]);
  });

  it("has every component import from the lib rather than re-implement", () => {
    const components = ["RiskIndicator.tsx", "PositionManager.tsx", "CapitalEfficiencyCalc.tsx"];
    for (const component of components) {
      const source = readFileSync(join(SRC, component), "utf8");
      expect(source).toMatch(/from "\.\/lib\//);
    }
  });
});
