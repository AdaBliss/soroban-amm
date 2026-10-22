/**
 * Tests for AmmPool's contract-error decoder — Issue #831.
 *
 * Soroban RPC reports contract errors as `Error(Contract, #N)`, so the decoder
 * must map the numeric discriminant to an `AmmErrors` entry rather than
 * substring-matching English text (which never matched a real error).
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Networks, nativeToScVal, xdr } from "@stellar/stellar-sdk";

import { AmmPool, AmmContractError, decodeError } from "./AmmPool.js";
import { AmmErrors, AmmErrorNames } from "./types.js";


/** The `invokeContract` payload of the first operation of a built transaction. */
function invokeOf(tx: { operations: unknown[] }): { functionName: unknown; args: xdr.ScVal[] } {
  const op = tx.operations[0] as {
    func: { invokeContract: { functionName: unknown; args: xdr.ScVal[] } };
  };
  return op.func.invokeContract;
}

/** The XDR type discriminant name of an ScVal, e.g. `"scvI128"`. */
function scvName(v: xdr.ScVal): string {
  return `scv${v.constructor.name.replace(/^ScVal/, "")}`;
}

describe("decodeError", () => {
  it("maps Error(Contract, #6) to the PAUSED entry", () => {
    const err = decodeError(
      new Error("host invocation failed: Error(Contract, #6)")
    );
    expect(err).toBeInstanceOf(AmmContractError);
    const contractErr = err as AmmContractError;
    expect(contractErr.code).toBe(6);
    expect(contractErr.name).toBe("Paused");
    expect(contractErr.message).toBe(`AMM error: ${AmmErrors[6]}`);
    expect(contractErr.message).toContain("contract is paused");
  });

  it.each([
    [4, "DeadlineExceeded", "deadline exceeded"],
    [5, "SlippageExceeded", "slippage exceeded"],
    [7, "Unauthorized", "unauthorized"],
    [11, "InsufficientLiquidity", "insufficient liquidity"],
    [18, "FlashLoanRepaymentFailed", "flash loan repayment failed"],
    [19, "AlreadyExecuted", "multisig proposal already executed"],
    [20, "ProposalExpired", "multisig proposal expired"],
    [21, "NotInitialized", "pool not initialized"],
    [27, "EmergencyDelayNotElapsed", "emergency delay not elapsed"],
    [29, "MultisigGuardActive", "multisig guard is active"],
  ])("maps Error(Contract, #%i) to %s", (code, name, text) => {
    const err = decodeError(new Error(`Error(Contract, #${code})`));
    expect(err).toBeInstanceOf(AmmContractError);
    const contractErr = err as AmmContractError;
    expect(contractErr.code).toBe(code);
    expect(contractErr.name).toBe(name);
    expect(contractErr.message).toBe(`AMM error: ${text}`);
  });

  it("decodes every discriminant declared in AmmErrors", () => {
    for (const key of Object.keys(AmmErrors)) {
      const code = Number(key);
      const err = decodeError(new Error(`Error(Contract, #${code})`));
      expect(err).toBeInstanceOf(AmmContractError);
      expect((err as AmmContractError).code).toBe(code);
    }
  });

  it("tolerates whitespace variations in the RPC error format", () => {
    for (const raw of [
      "Error(Contract, #6)",
      "Error(Contract,#6)",
      "Error( Contract , #6 )",
      "Error  (  Contract  ,  #6  )",
    ]) {
      const err = decodeError(new Error(raw));
      expect((err as AmmContractError).code).toBe(6);
    }
  });

  it("preserves the raw RPC message on the decoded error", () => {
    const raw = "simulation failed: Error(Contract, #9) at ledger 42";
    const err = decodeError(new Error(raw)) as AmmContractError;
    expect(err.rawMessage).toBe(raw);
  });

  it("accepts a bare string as well as an Error", () => {
    const err = decodeError("Error(Contract, #6)");
    expect((err as AmmContractError).code).toBe(6);
  });

  it("falls back to the raw message when no discriminant is present", () => {
    const err = decodeError(new Error("connection refused"));
    expect(err).not.toBeInstanceOf(AmmContractError);
    expect(err.message).toBe("AMM error: connection refused");
  });

  it("does not match on descriptive text alone", () => {
    // The old decoder matched this substring; a real RPC error never looks
    // like this, so it must fall through to the raw-message branch.
    const err = decodeError(new Error("the contract is paused"));
    expect(err).not.toBeInstanceOf(AmmContractError);
    expect(err.message).toBe("AMM error: the contract is paused");
  });

  it("reports unknown discriminants without inventing a mapping", () => {
    const err = decodeError(new Error("Error(Contract, #999)"));
    expect(err).not.toBeInstanceOf(AmmContractError);
    expect(err.message).toContain("unknown contract error #999");
  });
});

describe("AmmErrors", () => {
  it("covers all 30 AmmError discriminants from contracts/amm/src/lib.rs", () => {
    const codes = Object.keys(AmmErrors).map(Number).sort((a, b) => a - b);
    expect(codes).toEqual(Array.from({ length: 30 }, (_, i) => i + 1));
  });

  it("has a symbolic name for every discriminant", () => {
    expect(Object.keys(AmmErrorNames).sort()).toEqual(Object.keys(AmmErrors).sort());
  });

  it("mirrors the Rust variant names exactly", () => {
    // Transcribed from `pub enum AmmError` in contracts/amm/src/lib.rs:61.
    expect(AmmErrorNames).toEqual({
      1: "AlreadyInitialized",
      2: "InvalidFeeBps",
      3: "InsufficientShares",
      4: "DeadlineExceeded",
      5: "SlippageExceeded",
      6: "Paused",
      7: "Unauthorized",
      8: "ZeroAmount",
      9: "InvalidToken",
      10: "EmptyPool",
      11: "InsufficientLiquidity",
      12: "NoPendingAdmin",
      13: "WrongAdmin",
      14: "Reentrant",
      15: "CircuitBreaker",
      16: "FotSlippage",
      17: "OracleDeviationExceeded",
      18: "FlashLoanRepaymentFailed",
      19: "AlreadyExecuted",
      20: "ProposalExpired",
      21: "NotInitialized",
      22: "PoolDrained",
      23: "QuorumNotMet",
      24: "NoPendingProposal",
      25: "QuorumNotReached",
      26: "InvalidQuorum",
      27: "EmergencyDelayNotElapsed",
      28: "InvalidRecipient",
      29: "MultisigGuardActive",
      30: "InvalidEmergencyDelay",
    });
  });

  it("matches `pub enum AmmError` in the Rust source", () => {
    // Parse the enum straight from the contract so a variant added on the
    // Rust side fails this test instead of silently going undecoded here.
    const src = readFileSync(
      fileURLToPath(new URL("../../../contracts/amm/src/lib.rs", import.meta.url)),
      "utf8",
    );
    const body = src.match(/pub enum AmmError \{([\s\S]*?)\n\}/)?.[1];
    expect(body).toBeDefined();
    const fromRust: Record<number, string> = {};
    for (const m of body!.matchAll(/^\s*(\w+)\s*=\s*(\d+),/gm)) {
      fromRust[Number(m[2])] = m[1];
    }
    expect(Object.keys(fromRust).length).toBeGreaterThan(0);
    expect(AmmErrorNames).toEqual(fromRust);
  });
});

// ── simulateSwap decodes the contract's SwapSimulation ────────────────────────

const SWAP_TOKEN_A = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526";
const SWAP_TOKEN_B = "CABAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAFNSZ";

const poolConfig = {
  rpcUrl: "https://rpc.example.invalid",
  networkPassphrase: Networks.TESTNET,
  contractId: "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5SOAOPIFY6YQGAXE",
};

interface SimCall {
  method: string;
  args: xdr.ScVal[];
}

/** An `AmmPool` whose RPC server is replaced by a responder, plus its calls. */
function mockPool(responder: (call: SimCall) => xdr.ScVal): { client: AmmPool; calls: SimCall[] } {
  const client = new AmmPool(poolConfig);
  const server = (client as unknown as { server: Record<string, unknown> }).server;
  const calls: SimCall[] = [];
  server.simulateTransaction = async (tx: { operations: unknown[] }) => {
    const invoke = invokeOf(tx);
    const call: SimCall = { method: String(invoke.functionName), args: invoke.args };
    calls.push(call);
    return { result: { retval: responder(call) } };
  };
  return { client, calls };
}

interface ContractQuote {
  amountOut: bigint;
  feeAmount: bigint;
  priceImpactBps: bigint;
  effectivePrice: bigint;
  spotPrice: bigint;
}

/** The exact `SwapSimulation` math of `contracts/amm/src/lib.rs::simulate_swap`. */
function contractQuote(
  reserveIn: bigint,
  reserveOut: bigint,
  amountIn: bigint,
  feeBps: bigint
): ContractQuote {
  const amountInWithFee = amountIn * (10_000n - feeBps);
  const amountOut = (amountInWithFee * reserveOut) / (reserveIn * 10_000n + amountInWithFee);
  const feeAmount = (amountIn * feeBps) / 10_000n;
  const spotPrice = (reserveOut * 1_000_000n) / reserveIn;
  const effectivePrice = (amountOut * 1_000_000n) / amountIn;
  const priceImpactBps =
    amountOut === 0n ? 0n : ((spotPrice - effectivePrice) * 10_000n) / spotPrice;
  return { amountOut, feeAmount, priceImpactBps, effectivePrice, spotPrice };
}

/** Encode a `SwapSimulation` struct the way `scValToNative` expects to decode it. */
function swapSimulationScVal(quote: ContractQuote): xdr.ScVal {
  const fields: Array<[string, bigint]> = [
    ["amount_out", quote.amountOut],
    ["fee_amount", quote.feeAmount],
    ["price_impact_bps", quote.priceImpactBps],
    ["effective_price", quote.effectivePrice],
    ["spot_price", quote.spotPrice],
  ];
  return xdr.ScVal.scvMap(
    fields.map(
      ([key, value]) =>
        new xdr.ScMapEntry({
          key: xdr.ScVal.scvSymbol(key),
          val: nativeToScVal(value, { type: "i128" }),
        })
    )
  );
}

describe("AmmPool.simulateSwap", () => {
  it("calls simulate_swap with (token_in, amount_in) and decodes every field", async () => {
    const expected = contractQuote(1_000_000n, 4_000_000n, 1_000n, 30n);
    const { client, calls } = mockPool(() => swapSimulationScVal(expected));

    const quote = await client.simulateSwap(SWAP_TOKEN_A, 1_000n);

    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("simulate_swap");
    expect(calls[0].args.map((a) => scvName(a))).toEqual(["scvAddress", "scvI128"]);
    expect(quote).toEqual({
      amountIn: 1_000n,
      amountOut: expected.amountOut,
      feeAmount: expected.feeAmount,
      priceImpactBps: Number(expected.priceImpactBps),
      effectivePrice: expected.effectivePrice,
      spotPrice: expected.spotPrice,
    });
  });

  const directions: Array<[string, string, bigint, bigint]> = [
    ["A in, B out", SWAP_TOKEN_A, 1_000_000n, 4_000_000n],
    ["B in, A out", SWAP_TOKEN_B, 4_000_000n, 1_000_000n],
  ];

  it.each(directions)(
    "reports a small non-negative impact for %s on a 1M/4M pool",
    async (_label, tokenIn, reserveIn, reserveOut) => {
      const expected = contractQuote(reserveIn, reserveOut, 1_000n, 30n);
      const { client } = mockPool(() => swapSimulationScVal(expected));

      const quote = await client.simulateSwap(tokenIn, 1_000n);

      expect(quote.priceImpactBps).toBeGreaterThanOrEqual(0);
      // The old inverted-units math reported -9,372 and +150,640 bps here.
      expect(quote.priceImpactBps).toBeLessThan(100);
      expect(quote.amountOut).toBe(expected.amountOut);
    }
  );
});
