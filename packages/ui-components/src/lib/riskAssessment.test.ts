import { describe, it, expect } from "vitest";
import {
  assessPositionRisk,
  riskLevelForScore,
  RISK_THRESHOLDS,
  INVALID_DATA_FACTOR,
} from "./riskAssessment.js";

const healthy = {
  priceDeviationBps: 0,
  rangePct: 50,
  tvl: 1_000_000,
  isInRange: true,
};

describe("riskLevelForScore — every threshold on both sides", () => {
  // Boundary 80 (low/medium)
  it("returns low at exactly the low threshold (80)", () => {
    expect(riskLevelForScore(RISK_THRESHOLDS.low)).toBe("low");
  });
  it("returns medium just below the low threshold (79)", () => {
    expect(riskLevelForScore(RISK_THRESHOLDS.low - 1)).toBe("medium");
  });
  it("clamps scores above 100 to low", () => {
    expect(riskLevelForScore(120)).toBe("low");
  });

  // Boundary 55 (medium/high)
  it("returns medium at exactly the medium threshold (55)", () => {
    expect(riskLevelForScore(RISK_THRESHOLDS.medium)).toBe("medium");
  });
  it("returns high just below the medium threshold (54)", () => {
    expect(riskLevelForScore(RISK_THRESHOLDS.medium - 1)).toBe("high");
  });

  // Boundary 30 (high/critical)
  it("returns high at exactly the high threshold (30)", () => {
    expect(riskLevelForScore(RISK_THRESHOLDS.high)).toBe("high");
  });
  it("returns critical just below the high threshold (29)", () => {
    expect(riskLevelForScore(RISK_THRESHOLDS.high - 1)).toBe("critical");
  });
  it("clamps negative scores to critical", () => {
    expect(riskLevelForScore(-5)).toBe("critical");
  });
});

describe("assessPositionRisk", () => {
  it("returns low risk with a full score for a healthy in-range position", () => {
    const r = assessPositionRisk(healthy);
    expect(r.score).toBe(100);
    expect(r.level).toBe("low");
    expect(r.factors).toHaveLength(0);
  });

  it("penalises an out-of-range position with a critical factor", () => {
    const r = assessPositionRisk({ ...healthy, isInRange: false });
    expect(r.factors.some((f) => f.name === "Out of range" && f.severity === "critical")).toBe(true);
    expect(r.score).toBe(60);
  });

  it("applies medium deviation exactly at the >200 boundary and high above 500", () => {
    const medium = assessPositionRisk({ ...healthy, priceDeviationBps: 250 });
    const high = assessPositionRisk({ ...healthy, priceDeviationBps: 501 });
    expect(medium.factors.find((f) => f.name === "High price deviation")?.severity).toBe("medium");
    expect(medium.score).toBe(90);
    expect(high.factors.find((f) => f.name === "High price deviation")?.severity).toBe("high");
    expect(high.score).toBe(75);
  });

  it("does not apply deviation at or below the 200 threshold", () => {
    const r = assessPositionRisk({ ...healthy, priceDeviationBps: 200 });
    expect(r.factors.find((f) => f.name === "High price deviation")).toBeUndefined();
    expect(r.score).toBe(100);
  });

  it("penalises a very narrow range (<5%)", () => {
    const r = assessPositionRisk({ ...healthy, rangePct: 4 });
    expect(r.factors.some((f) => f.name === "Very narrow range")).toBe(true);
    expect(r.score).toBe(85);
  });

  it("does not penalise a range at exactly 5%", () => {
    const r = assessPositionRisk({ ...healthy, rangePct: 5 });
    expect(r.factors.find((f) => f.name === "Very narrow range")).toBeUndefined();
    expect(r.score).toBe(100);
  });

  it("flags low TVL below 10k (low>1k) and high TVL risk below 1k", () => {
    const low = assessPositionRisk({ ...healthy, tvl: 9_000 });
    const high = assessPositionRisk({ ...healthy, tvl: 500 });
    expect(low.factors.find((f) => f.name === "Low TVL")?.severity).toBe("low");
    expect(low.score).toBe(95);
    expect(high.factors.find((f) => f.name === "Low TVL")?.severity).toBe("high");
    expect(high.score).toBe(80);
  });

  it("hand-computed combination lands exactly on the medium boundary (score 55)", () => {
    const r = assessPositionRisk({
      priceDeviationBps: 600,
      rangePct: 4,
      tvl: 5_000,
      isInRange: true,
    });
    // 100 - 25 (high deviation) - 15 (narrow) - 5 (low TVL) = 55 → medium
    expect(r.score).toBe(55);
    expect(r.level).toBe("medium");
  });

  it("aggregates multiple factors to critical and clamps at zero", () => {
    const r = assessPositionRisk({
      priceDeviationBps: 1000,
      rangePct: 1,
      tvl: 500,
      isInRange: false,
    });
    // 100 - 40 - 25 - 15 - 20 = 0 → critical
    expect(r.score).toBe(0);
    expect(r.level).toBe("critical");
    expect(r.factors.length).toBe(4);
  });

  it("does not overflow with very large numeric inputs", () => {
    const r = assessPositionRisk({
      ...healthy,
      priceDeviationBps: Number.MAX_SAFE_INTEGER,
      tvl: Number.MAX_SAFE_INTEGER,
    });
    expect(Number.isFinite(r.score)).toBe(true);
    expect(r.score).toBeGreaterThanOrEqual(0);
  });
});

describe("assessPositionRisk — missing or invalid data", () => {
  it("returns the unknown level and an invalid-data factor for NaN metrics", () => {
    const r = assessPositionRisk({
      priceDeviationBps: NaN,
      rangePct: NaN,
      tvl: NaN,
      isInRange: true,
    });
    expect(r.level).toBe("unknown");
    expect(r.score).toBe(0);
    expect(r.factors).toHaveLength(1);
    expect(r.factors[0].name).toBe(INVALID_DATA_FACTOR);
    expect(r.factors[0].severity).toBe("critical");
  });

  it("treats each NaN metric on its own as invalid", () => {
    expect(assessPositionRisk({ ...healthy, priceDeviationBps: NaN }).level).toBe("unknown");
    expect(assessPositionRisk({ ...healthy, rangePct: NaN }).level).toBe("unknown");
    expect(assessPositionRisk({ ...healthy, tvl: NaN }).level).toBe("unknown");
  });

  it("treats negative metrics as invalid rather than as a good score", () => {
    // -5 previously scored 80 ("low") because it is simply below the TVL
    // threshold; it must now be rejected outright.
    expect(assessPositionRisk({ ...healthy, tvl: -5 }).level).toBe("unknown");
    expect(assessPositionRisk({ ...healthy, priceDeviationBps: -1 }).level).toBe("unknown");
    expect(assessPositionRisk({ ...healthy, rangePct: -1 }).level).toBe("unknown");
  });

  it("treats non-finite Infinity metrics as invalid", () => {
    expect(assessPositionRisk({ ...healthy, tvl: Infinity }).level).toBe("unknown");
    expect(assessPositionRisk({ ...healthy, rangePct: Infinity }).level).toBe("unknown");
    expect(assessPositionRisk({ ...healthy, priceDeviationBps: -Infinity }).level).toBe("unknown");
  });

  it("does not add risk factors for an invalid assessment beyond the invalid-data factor", () => {
    const r = assessPositionRisk({
      priceDeviationBps: NaN,
      rangePct: 1,
      tvl: 500,
      isInRange: false,
    });
    expect(r.factors.map((f) => f.name)).toEqual([INVALID_DATA_FACTOR]);
  });
});
