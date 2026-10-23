import { describe, it, expect } from "vitest";
import * as publicApi from "./index.js";
import { assessPositionRisk } from "./lib/riskAssessment.js";
import { validatePosition } from "./lib/positionValidation.js";
import { computeEfficiency } from "./lib/capitalEfficiency.js";

/**
 * The public surface must expose the lib functions directly, so consumers get
 * the tested implementations rather than a component-local copy (issue #1168).
 */
describe("public API re-exports the shared lib logic", () => {
  it("exports assessPositionRisk from the package root", () => {
    expect(publicApi.assessPositionRisk).toBe(assessPositionRisk);
  });

  it("exports validatePosition from the package root", () => {
    expect(publicApi.validatePosition).toBe(validatePosition);
  });

  it("exports computeEfficiency from the package root", () => {
    expect(publicApi.computeEfficiency).toBe(computeEfficiency);
  });

  it("exports the risk thresholds and level helper", () => {
    expect(publicApi.RISK_THRESHOLDS).toBeDefined();
    expect(publicApi.riskLevelForScore).toBeTypeOf("function");
    expect(publicApi.INVALID_DATA_FACTOR).toBe("Missing or invalid data");
  });

  it("exposes the efficiency helpers used by the components", () => {
    expect(publicApi.isInRange).toBeTypeOf("function");
    expect(publicApi.rangePct).toBeTypeOf("function");
  });

  it("routes an invalid assessment through the root export", () => {
    const assessment = publicApi.assessPositionRisk({
      priceDeviationBps: NaN,
      rangePct: 10,
      tvl: 1_000_000,
      isInRange: true,
    });
    expect(assessment.level).toBe("unknown");
  });
});
