/**
 * Pure concentrated-liquidity capital-efficiency math, extracted from
 * CapitalEfficiencyCalc.tsx so the formula can be unit-tested directly.
 *
 * Capital efficiency compares the capital a concentrated-range position needs
 * to deliver the same depth as a full-range (v2-style) position. The larger
 * the multiplier, the more "efficient" the capital is but the narrower and
 * more rebalancing-prone the range.
 *
 * For liquidity L and current price P inside [Pa, Pb], the quote-token
 * capital is `L * (2sqrt(P) - sqrt(Pa) - P/sqrt(Pb))`, while equivalent
 * full-range capital is `2L*sqrt(P)`. This is the sqrt-price relationship
 * implemented by `contracts/concentrated_liquidity/src/math.rs`.
 */

import type { PriceRange } from "../types.js";

/**
 * Computes the capital-efficiency multiplier of a [lower, upper] range
 * relative to a full range, given the current spot price.
 *
 * Returns `1` (i.e. no capital advantage, equivalent to a full-range
 * position) for any degenerate or out-of-range input:
 *   - lower or upper <= 0
 *   - lower >= upper (equal or inverted ranges)
 *   - currentPrice is outside [lower, upper] (position inactive)
 */
export function computeEfficiency(
  currentPrice: number,
  lower: number,
  upper: number,
): number {
  if (!Number.isFinite(lower) || !Number.isFinite(upper)) return 1;
  if (lower <= 0 || upper <= 0 || lower >= upper) return 1;
  if (!Number.isFinite(currentPrice) || currentPrice < lower || currentPrice > upper) return 1;
  // Mirrors the sqrt-price liquidity relationship in
  // contracts/concentrated_liquidity/src/math.rs:
  // full = 2L*sqrt(P), finite = L*(2sqrt(P)-sqrt(Pa)-P/sqrt(Pb)).
  const sqrtPrice = Math.sqrt(currentPrice);
  const concentratedCapital =
    2 * sqrtPrice - Math.sqrt(lower) - currentPrice / Math.sqrt(upper);
  if (!Number.isFinite(concentratedCapital) || concentratedCapital <= 0) return 1;
  const eff = (2 * sqrtPrice) / concentratedCapital;
  return Math.max(1, eff);
}

/** Compute efficiency directly from Soroban CL ticks (`price = 1.0001^tick`). */
export function computeEfficiencyFromTicks(
  currentTick: number,
  lowerTick: number,
  upperTick: number,
): number {
  if (![currentTick, lowerTick, upperTick].every(Number.isFinite)) return 1;
  if (lowerTick >= upperTick || currentTick < lowerTick || currentTick > upperTick) return 1;
  const current = Math.pow(1.0001, currentTick);
  const lower = Math.pow(1.0001, lowerTick);
  const upper = Math.pow(1.0001, upperTick);
  if (![current, lower, upper].every(Number.isFinite)) return 1;
  return computeEfficiency(current, lower, upper);
}

/**
 * Percentage width of a price range relative to a reference price.
 * Handles a zero reference so it never returns Infinity/NaN.
 */
export function rangePct(lower: number, upper: number, currentPrice: number): number {
  if (currentPrice <= 0) return 0;
  return ((upper - lower) / currentPrice) * 100;
}

/** Whether a price sits inside [lower, upper] (inclusive bounds). */
export function isInRange(currentPrice: number, range: PriceRange): boolean {
  return currentPrice >= range.lower && currentPrice <= range.upper;
}

/** Format the efficiency multiplier for display (clamped at "__.x", ">1000"). */
export function formatEfficiency(efficiency: number): string {
  if (!Number.isFinite(efficiency) || efficiency <= 0) return "1.0";
  if (efficiency >= 1000) return ">1000";
  return efficiency.toFixed(1);
}
