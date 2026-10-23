/**
 * Pure risk-assessment logic for the UI. This module is the single source of
 * truth: `RiskIndicator` renders what `assessPositionRisk` returns and nothing
 * in the components re-derives a level or score.
 *
 * `assessPositionRisk` derives a {@link RiskAssessment} (level, score and a
 * list of contributing factors) from a set of common position/pool metrics.
 * `riskLevelForScore` maps a (already-clamped) 0–100 score to a categorical
 * level. Both are pure functions with no I/O.
 */

import type { RiskAssessment, RiskFactor, RiskLevel } from "../types.js";

/** Score threshold below which a risk level drops down a category. */
export const RISK_THRESHOLDS = {
  low: 80,
  medium: 55,
  high: 30,
} as const;

/**
 * Maps a clamped 0–100 risk score to a categorical level.
 * Boundaries are inclusive on their lower edge:
 *   score >= 80 → "low"
 *   score >= 55 → "medium"
 *   score >= 30 → "high"
 *   otherwise     → "critical"
 */
export function riskLevelForScore(score: number): RiskLevel {
  const clamped = Math.max(0, Math.min(100, score));
  if (clamped >= RISK_THRESHOLDS.low) return "low";
  if (clamped >= RISK_THRESHOLDS.medium) return "medium";
  if (clamped >= RISK_THRESHOLDS.high) return "high";
  return "critical";
}

export interface RiskParams {
  /** Deviation from TWAP in basis points (non-negative). */
  priceDeviationBps: number;
  /** Range width around the current price, in percent (non-negative). */
  rangePct: number;
  /** Pool TVL in USD (non-negative). */
  tvl: number;
  /** Whether the current price sits inside the selected range. */
  isInRange: boolean;
}

/** Name of the factor emitted when the inputs cannot be assessed. */
export const INVALID_DATA_FACTOR = "Missing or invalid data";

/** Derives a RiskAssessment from common position metrics. */
export function assessPositionRisk(params: RiskParams): RiskAssessment {
  const factors: RiskFactor[] = [];

  // Missing or nonsensical inputs (NaN/Infinity, negative magnitudes) must not
  // be scored as if the position were healthy: every comparison against a
  // threshold is false for NaN, which previously produced a perfect score and
  // a green indicator for absent data. Report an explicit outcome instead.
  const metrics = [params.priceDeviationBps, params.rangePct, params.tvl];
  if (metrics.some((m) => !Number.isFinite(m) || m < 0)) {
    return {
      level: "unknown",
      score: 0,
      factors: [
        {
          name: INVALID_DATA_FACTOR,
          description:
            "One or more position metrics are missing or invalid, so the risk of this position cannot be assessed.",
          severity: "critical",
        },
      ],
    };
  }

  let score = 100;

  if (!params.isInRange) {
    factors.push({
      name: "Out of range",
      description: "Current price is outside your selected range. The position is inactive and earning no fees.",
      severity: "critical",
    });
    score -= 40;
  }

  if (params.priceDeviationBps > 200) {
    const sev: RiskLevel = params.priceDeviationBps > 500 ? "high" : "medium";
    factors.push({
      name: "High price deviation",
      description: `Price has deviated ${(params.priceDeviationBps / 100).toFixed(1)}% from TWAP, indicating unusual volatility.`,
      severity: sev,
      value: params.priceDeviationBps,
      threshold: 500,
    });
    score -= params.priceDeviationBps > 500 ? 25 : 10;
  }

  if (params.rangePct < 5) {
    factors.push({
      name: "Very narrow range",
      description: `Range spans only ${params.rangePct.toFixed(1)}% around current price. High efficiency but high rebalancing risk.`,
      severity: "medium",
      value: params.rangePct,
      threshold: 5,
    });
    score -= 15;
  }

  if (params.tvl < 10_000) {
    factors.push({
      name: "Low TVL",
      description: "Pool TVL below $10,000. Thin liquidity may cause higher slippage.",
      severity: params.tvl < 1_000 ? "high" : "low",
      value: params.tvl,
      threshold: 10_000,
    });
    score -= params.tvl < 1_000 ? 20 : 5;
  }

  score = Math.max(0, Math.min(100, score));
  const level = riskLevelForScore(score);

  return { level, score, factors };
}
