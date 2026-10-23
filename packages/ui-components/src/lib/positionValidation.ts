/**
 * Pure validation logic for PositionManager, extracted so the dispatch rules
 * ("invalid input is rejected and surfaced, not silently coerced") can be
 * unit-tested directly.
 *
 * A position is only valid when:
 *   - both deposit amounts are strictly positive (zero and negative amounts
 *     are rejected),
 *   - the price range is well-formed and ordered (lower < upper, both > 0).
 */

import type { PriceRange } from "../types.js";

export interface PositionValidationInput {
  amountA: number;
  amountB: number;
  priceRange: PriceRange;
}

/** Returns a human-readable error message for the first offending attribute. */
export function validatePosition({
  amountA,
  amountB,
  priceRange,
}: PositionValidationInput): string[] {
  const errors: string[] = [];

  if (!Number.isFinite(amountA) || amountA <= 0) {
    errors.push(`Amount A must be a positive number (received ${fmt(amountA)}).`);
  }
  if (!Number.isFinite(amountB) || amountB <= 0) {
    errors.push(`Amount B must be a positive number (received ${fmt(amountB)}).`);
  }

  const { lower, upper } = priceRange;
  if (!Number.isFinite(lower) || lower <= 0) {
    errors.push("Lower price must be a positive number.");
  }
  if (!Number.isFinite(upper) || upper <= 0) {
    errors.push("Upper price must be a positive number.");
  }
  if (lower > 0 && upper > 0 && lower >= upper) {
    errors.push("Lower price must be below the upper price.");
  }

  return errors;
}

function fmt(n: number): string {
  return String(n);
}
