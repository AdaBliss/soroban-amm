/**
 * Tests for the shared `scValToNative` decoding helpers.
 *
 * The clients used to coerce contract fields with `String(...)`, which turned
 * any unexpected object into "[object Object]". The helpers must keep decoding
 * every shape a contract actually returns while rejecting everything else.
 */

import { describe, it, expect } from "vitest";
import { nativeToScVal, scValToNative, xdr } from "@stellar/stellar-sdk";

import {
  ContractDecodeError,
  optionalBigInt,
  requireBigInt,
  toBigInt,
  toText,
  toVariant,
} from "./internal/decode.js";

describe("toBigInt", () => {
  it("decodes integer-valued contract fields", () => {
    expect(toBigInt(scValToNative(nativeToScVal(123n, { type: "i128" })))).toBe(123n);
    expect(toBigInt(scValToNative(nativeToScVal(7, { type: "u32" })))).toBe(7n);
    expect(toBigInt("42")).toBe(42n);
  });

  it("rejects missing values instead of fabricating zero", () => {
    expect(() => toBigInt(undefined)).toThrow(TypeError);
    expect(() => toBigInt(null)).toThrow(TypeError);
  });

  it("rejects values that are not integers", () => {
    expect(() => toBigInt({ amount: 1 })).toThrow(TypeError);
    expect(() => toBigInt([1n])).toThrow(/array\(1\)/);
    expect(() => toBigInt(true)).toThrow(TypeError);
  });
});

describe("optionalBigInt", () => {
  it("decodes absent Option values as null and present values as bigint", () => {
    expect(optionalBigInt(null)).toBeNull();
    expect(optionalBigInt(undefined)).toBeNull();
    expect(optionalBigInt([])).toBeNull();
    expect(optionalBigInt(["9"])).toBe(9n);
    expect(optionalBigInt(11n)).toBe(11n);
  });
});

describe("required decoded struct fields", () => {
  const fields = (struct: string, names: string[]): Array<[string, string]> =>
    names.map((field): [string, string] => [struct, field]);
  const cases: Array<[string, string]> = [
    ...fields("PoolInfo", ["reserve_a", "reserve_b", "total_shares", "fee_bps", "protocol_fee_bps", "lp_rebate_bps", "flash_loan_fee_bps"]),
    ...fields("SwapSimulation", ["amount_out", "fee_amount", "price_impact_bps", "effective_price", "spot_price"]),
    ...fields("PoolState", ["sqrt_price", "current_tick", "active_liquidity", "tick_spacing"]),
    ...fields("Position", ["lower_tick", "upper_tick", "liquidity", "fee_growth_inside_a", "fee_growth_inside_b"]),
    ...fields("PriceImpactEstimate", ["amount_in", "amount_in_after_fee", "amount_out", "fee_amount", "spot_price_before", "effective_price", "price_impact_bps", "sqrt_price_before", "sqrt_price_after", "tick_before", "tick_after", "active_liquidity_before", "active_liquidity_after"]),
    ...fields("Proposal", ["snapshot_total_supply", "snapshot_ledger", "vote_start", "vote_end", "execute_after", "expires_at", "votes_for", "votes_against", "votes_abstain"]),
    ...fields("GovernanceParams", ["voting_period_secs", "timelock_secs", "quorum_bps", "min_proposer_stake_bps", "quorum_decay_rate_bps_per_day"]),
  ];

  it.each(cases)("throws ContractDecodeError for missing %s.%s", (structName, field) => {
    let thrown: unknown;
    try {
      requireBigInt({}, field, structName);
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(ContractDecodeError);
    expect((thrown as Error).message).toContain(`${structName}.${field}`);
  });

  it("includes present keys in the missing-field error", () => {
    expect(() => requireBigInt({ reserve_b: 4n }, "reserve_a", "PoolInfo"))
      .toThrow(/PoolInfo\.reserve_a; present keys: reserve_b/);
  });
});

describe("toText", () => {
  it("decodes strings, symbols and addresses", () => {
    expect(toText(scValToNative(xdr.ScVal.scvSymbol("pool")))).toBe("pool");
    expect(toText("GABC")).toBe("GABC");
    expect(toText(5n)).toBe("5");
  });

  it("rejects objects instead of producing [object Object]", () => {
    expect(() => toText({ inner: "x" })).toThrow(/got object/);
    expect(() => toText(undefined)).toThrow(TypeError);
  });
});

describe("toVariant", () => {
  it("decodes a unit enum variant, which scValToNative returns as a one-element array", () => {
    const active = xdr.ScVal.scvVec([xdr.ScVal.scvSymbol("Active")]);
    expect(toVariant(scValToNative(active), "Unknown")).toBe("Active");
  });

  it("accepts a bare symbol and falls back when the field is missing", () => {
    expect(toVariant("VotedFor", "DidNotVote")).toBe("VotedFor");
    expect(toVariant(undefined, "DidNotVote")).toBe("DidNotVote");
  });

  it("rejects shapes that are not enum variants", () => {
    expect(() => toVariant({ tag: "Active" }, "Unknown")).toThrow(TypeError);
    expect(() => toVariant([1], "Unknown")).toThrow(TypeError);
  });
});
