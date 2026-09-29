/**
 * Tests for the shared `scValToNative` decoding helpers.
 *
 * The clients used to coerce contract fields with `String(...)`, which turned
 * any unexpected object into "[object Object]". The helpers must keep decoding
 * every shape a contract actually returns while rejecting everything else.
 */

import { describe, it, expect } from "vitest";
import { nativeToScVal, scValToNative, xdr } from "@stellar/stellar-sdk";

import { toBigInt, toText, toVariant } from "./internal/decode.js";

describe("toBigInt", () => {
  it("decodes integer-valued contract fields", () => {
    expect(toBigInt(scValToNative(nativeToScVal(123n, { type: "i128" })))).toBe(123n);
    expect(toBigInt(scValToNative(nativeToScVal(7, { type: "u32" })))).toBe(7n);
    expect(toBigInt("42")).toBe(42n);
  });

  it("treats a missing field as zero", () => {
    expect(toBigInt(undefined)).toBe(0n);
    expect(toBigInt(null)).toBe(0n);
  });

  it("rejects values that are not integers", () => {
    expect(() => toBigInt({ amount: 1 })).toThrow(TypeError);
    expect(() => toBigInt([1n])).toThrow(/array\(1\)/);
    expect(() => toBigInt(true)).toThrow(TypeError);
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
