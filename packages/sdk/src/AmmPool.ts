/**
 * AmmPool — typed wrapper for the Soroban AMM contract — Issue #104
 *
 * Provides human-readable error decoding, a `simulate` helper that composes
 * `simulate_swap` + `get_amount_out`, and typed wrappers for every public
 * AMM function.
 */

import {
  Contract,
  rpc as StellarRpc,
  nativeToScVal,
  scValToNative,
  xdr,
  Address,
} from "@stellar/stellar-sdk";
import type {
  NetworkConfig,
  PoolInfo,
  SwapSimulation,
} from "./types.js";
import { AmmErrors, AmmErrorNames } from "./types.js";
import type { AmmErrorCode, AmmErrorKey } from "./types.js";
import { simulateRead } from "./internal/simulate.js";
import { toBigInt, toText } from "./internal/decode.js";

// ── Helpers ────────────────────────────────────────────────────────────────────

function i128(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "i128" });
}

function addr(address: string): xdr.ScVal {
  return nativeToScVal(Address.fromString(address));
}

/**
 * A contract-returned `AmmError`, decoded from the numeric discriminant Soroban
 * reports. Carries the discriminant and its symbolic name so callers can branch
 * on the specific failure rather than string-matching the message.
 */
export class AmmContractError extends Error {
  /** Numeric discriminant, matching `AmmError` in contracts/amm/src/lib.rs. */
  readonly code: AmmErrorCode;
  /** Symbolic variant name, e.g. `"Paused"`. */
  readonly name: AmmErrorKey;
  /** Unmodified message reported by the RPC server. */
  readonly rawMessage: string;

  constructor(code: AmmErrorCode, rawMessage: string) {
    super(`AMM error: ${AmmErrors[code]}`);
    this.code = code;
    this.name = AmmErrorNames[code];
    this.rawMessage = rawMessage;
  }
}

/**
 * Matches the numeric-coded form Soroban RPC uses to report a contract-returned
 * error, e.g. `Error(Contract, #6)`. Whitespace is tolerated because the exact
 * spacing varies between RPC versions and SDK error wrappers.
 */
const CONTRACT_ERROR_PATTERN = /Error\s*\(\s*Contract\s*,\s*#(\d+)\s*\)/;

/**
 * Decode a simulation or RPC failure into a friendly, typed AMM error.
 *
 * Soroban reports contract errors as `Error(Contract, #N)` where `N` is the
 * `AmmError` discriminant — never as descriptive English text, which is why the
 * previous substring match against phrases like "contract is paused" could never
 * fire. We parse the discriminant and look it up in {@link AmmErrors}, falling
 * back to the raw message when no discriminant is present (host errors, network
 * failures) or when the discriminant is one this SDK does not know.
 */
export function decodeError(err: unknown): Error {
  const msg = err instanceof Error ? err.message : String(err);
  const match = CONTRACT_ERROR_PATTERN.exec(msg);
  if (match) {
    const code = Number(match[1]);
    if (code in AmmErrors) {
      return new AmmContractError(code as AmmErrorCode, msg);
    }
    return new Error(`AMM error: unknown contract error #${code}: ${msg}`);
  }
  return new Error(`AMM error: ${msg}`);
}

// ── AmmPool class ─────────────────────────────────────────────────────────────

export class AmmPool {
  private readonly server: StellarRpc.Server;
  private readonly contract: Contract;
  private readonly networkPassphrase: string;

  constructor(config: NetworkConfig) {
    this.server = new StellarRpc.Server(config.rpcUrl);
    this.contract = new Contract(config.contractId);
    this.networkPassphrase = config.networkPassphrase;
  }

  // ── Read-only helpers ──────────────────────────────────────────────────────

  private async simulate(method: string, ...args: xdr.ScVal[]): Promise<xdr.ScVal> {
    return simulateRead(
      this.server,
      this.contract,
      this.networkPassphrase,
      method,
      args,
      decodeError
    );
  }

  // ── Pool info ──────────────────────────────────────────────────────────────

  /** Fetch full pool state. */
  async getInfo(): Promise<PoolInfo> {
    const raw = await this.simulate("get_info");
    const native = scValToNative(raw) as Record<string, unknown>;
    return {
      tokenA: toText(native.token_a),
      tokenB: toText(native.token_b),
      reserveA: toBigInt(native.reserve_a),
      reserveB: toBigInt(native.reserve_b),
      totalShares: toBigInt(native.total_shares),
      feeBps: toBigInt(native.fee_bps),
      protocolFeeBps: toBigInt(native.protocol_fee_bps),
      feeRecipient: native.fee_recipient ? toText(native.fee_recipient) : null,
      flashLoanFeeBps: toBigInt(native.flash_loan_fee_bps),
      admin: native.admin ? toText(native.admin) : null,
      isPaused: Boolean(native.is_paused),
      name: native.name ? toText(native.name) : null,
    };
  }

  /** Return protocol fees accrued but not yet withdrawn — read-only. */
  async getAccruedFees(): Promise<{ accruedA: bigint; accruedB: bigint }> {
    const raw = await this.simulate("get_accrued_fees");
    const native = scValToNative(raw) as [unknown, unknown];
    return {
      accruedA: toBigInt(native[0]),
      accruedB: toBigInt(native[1]),
    };
  }

  /** Return the human-readable pool name (or null). */
  async getName(): Promise<string | null> {
    const raw = await this.simulate("get_name");
    const native: unknown = scValToNative(raw);
    return native !== null && native !== undefined ? toText(native) : null;
  }

  /** Return the flash-loan fee in basis points. */
  async getFlashLoanFeeBps(): Promise<bigint> {
    const raw = await this.simulate("get_flash_loan_fee_bps");
    return BigInt(scValToNative(raw) as number);
  }

  /** Return the LP share balance of `address`. */
  async sharesOf(address: string): Promise<bigint> {
    const raw = await this.simulate("shares_of", addr(address));
    return BigInt(scValToNative(raw) as number);
  }

  // ── Swap simulation ────────────────────────────────────────────────────────

  /**
   * Simulate a swap and return amount out + price impact.
   *
   * Composes `get_amount_out` — no transaction is submitted.
   */
  async simulateSwap(
    tokenIn: string,
    amountIn: bigint
  ): Promise<SwapSimulation> {
    const info = await this.getInfo();
    const [reserveIn, reserveOut] =
      tokenIn === info.tokenA
        ? [info.reserveA, info.reserveB]
        : [info.reserveB, info.reserveA];

    // x*y = k constant-product formula with fee
    const feeMul = 10_000n - info.feeBps;
    const amountInWithFee = amountIn * feeMul;
    const numerator = amountInWithFee * reserveOut;
    const denominator = reserveIn * 10_000n + amountInWithFee;
    const amountOut = denominator > 0n ? numerator / denominator : 0n;
    const feeAmount = (amountIn * info.feeBps) / 10_000n;

    const spotPrice = reserveIn > 0n ? (reserveOut * 10_000n) / reserveIn : 0n;
    const executionPrice =
      amountOut > 0n ? (amountIn * 10_000n) / amountOut : 0n;
    const priceImpactBps =
      spotPrice > 0n
        ? Number(((executionPrice - spotPrice) * 10_000n) / spotPrice)
        : 0;

    return { amountIn, amountOut, priceImpactBps, feeAmount };
  }

  /** Return the amount out for `amountIn` of `tokenIn` (on-chain query). */
  async getAmountOut(tokenIn: string, amountIn: bigint): Promise<bigint> {
    const raw = await this.simulate("get_amount_out", addr(tokenIn), i128(amountIn));
    return BigInt(scValToNative(raw) as number);
  }

  /** Return the amount in required to receive `amountOut` of `tokenOut`. */
  async getAmountIn(tokenOut: string, amountOut: bigint): Promise<bigint> {
    const raw = await this.simulate("get_amount_in", addr(tokenOut), i128(amountOut));
    return BigInt(scValToNative(raw) as number);
  }

  // ── Contract ID ────────────────────────────────────────────────────────────

  get contractId(): string {
    return this.contract.contractId();
  }
}
