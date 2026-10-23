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
  AddLiquidityParams,
  NetworkConfig,
  PoolInfo,
  RemoveLiquidityParams,
  SwapParams,
  SwapSimulation,
} from "./types.js";
import { AmmErrors, AmmErrorNames } from "./types.js";
import type { AmmErrorCode, AmmErrorKey } from "./types.js";
import { simulateRead } from "./internal/simulate.js";
import { invoke } from "./internal/invoke.js";
import type { InvocationResult, InvokeOptions, Signer } from "./internal/invoke.js";
import { requireBigInt, requireText, toBigInt } from "./internal/decode.js";

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

  private async execute<T>(method: string, args: xdr.ScVal[], signer: Signer, options?: InvokeOptions): Promise<InvocationResult<T>> {
    return invoke<T>({
      server: this.server,
      contract: this.contract,
      method,
      args,
      source: await signer.publicKey(),
      signer,
      networkPassphrase: this.networkPassphrase,
      decodeError,
      ...options,
    });
  }

  // ── Pool info ──────────────────────────────────────────────────────────────

  private decodeInfo(native: Record<string, unknown>): Omit<PoolInfo, "isPaused"> {
    return {
      tokenA: requireText(native, "token_a", "PoolInfo"),
      tokenB: requireText(native, "token_b", "PoolInfo"),
      reserveA: requireBigInt(native, "reserve_a", "PoolInfo"),
      reserveB: requireBigInt(native, "reserve_b", "PoolInfo"),
      totalShares: requireBigInt(native, "total_shares", "PoolInfo"),
      feeBps: requireBigInt(native, "fee_bps", "PoolInfo"),
      protocolFeeBps: requireBigInt(native, "protocol_fee_bps", "PoolInfo"),
      lpRebateBps: requireBigInt(native, "lp_rebate_bps", "PoolInfo"),
      feeRecipient: requireText(native, "fee_recipient", "PoolInfo"),
      flashLoanFeeBps: requireBigInt(native, "flash_loan_fee_bps", "PoolInfo"),
      admin: requireText(native, "admin", "PoolInfo"),
    };
  }

  /** Fetch pool fields in one RPC call, without querying pause status. */
  async getInfoLight(): Promise<Omit<PoolInfo, "isPaused">> {
    const raw = await this.simulate("get_info");
    return this.decodeInfo(scValToNative(raw) as Record<string, unknown>);
  }

  /**
   * Fetch full pool state, including pause status. `get_info` and `is_paused`
   * are simulated concurrently, so this performs two RPC calls.
   */
  async getInfo(): Promise<PoolInfo> {
    const [infoRaw, pausedRaw] = await Promise.all([
      this.simulate("get_info"),
      this.simulate("is_paused"),
    ]);
    const paused: unknown = scValToNative(pausedRaw);
    if (typeof paused !== "boolean") {
      throw new TypeError(`expected a boolean contract value for is_paused, got ${typeof paused}`);
    }
    return {
      ...this.decodeInfo(scValToNative(infoRaw) as Record<string, unknown>),
      isPaused: paused,
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

  /**
   * Return the flash-loan fee in basis points.
   *
   * The contract has no exported `get_flash_loan_fee_bps` view — it is a private
   * helper — so this reads the field from `get_info` instead.
   */
  async getFlashLoanFeeBps(): Promise<bigint> {
    const info = await this.getInfoLight();
    return info.flashLoanFeeBps;
  }

  /** Return the LP share balance of `address`. */
  async sharesOf(address: string): Promise<bigint> {
    const raw = await this.simulate("shares_of", addr(address));
    return toBigInt(scValToNative(raw));
  }

  // ── Swap simulation ────────────────────────────────────────────────────────

  /**
   * Simulate a swap and return the output, fee and price impact.
   *
   * Calls the contract's `simulate_swap` and decodes its `SwapSimulation`
   * verbatim — fee, spot price, effective price and price impact are all
   * computed in-contract, so this can never disagree with the swap the pool
   * would execute. No transaction is submitted.
   */
  async simulateSwap(
    tokenIn: string,
    amountIn: bigint
  ): Promise<SwapSimulation> {
    const raw = await this.simulate("simulate_swap", addr(tokenIn), i128(amountIn));
    const native = scValToNative(raw) as Record<string, unknown>;
    return {
      amountIn,
      amountOut: requireBigInt(native, "amount_out", "SwapSimulation"),
      feeAmount: requireBigInt(native, "fee_amount", "SwapSimulation"),
      priceImpactBps: Number(requireBigInt(native, "price_impact_bps", "SwapSimulation")),
      effectivePrice: requireBigInt(native, "effective_price", "SwapSimulation"),
      spotPrice: requireBigInt(native, "spot_price", "SwapSimulation"),
    };
  }

  /** Return the amount out for `amountIn` of `tokenIn` (on-chain query). */
  async getAmountOut(tokenIn: string, amountIn: bigint): Promise<bigint> {
    const raw = await this.simulate("get_amount_out", addr(tokenIn), i128(amountIn));
    return toBigInt(scValToNative(raw));
  }

  /** Return the amount in required to receive `amountOut` of `tokenOut`. */
  async getAmountIn(tokenOut: string, amountOut: bigint): Promise<bigint> {
    const raw = await this.simulate("get_amount_in", addr(tokenOut), i128(amountOut));
    return toBigInt(scValToNative(raw));
  }

  async swap(input: SwapParams, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("swap", [addr(input.trader), addr(input.tokenIn), i128(input.amountIn), i128(input.minAmountOut), nativeToScVal(input.deadline, { type: "u64" })], signer, options);
  }

  async swapExactOut(trader: string, tokenOut: string, amountOut: bigint, maxIn: bigint, deadline: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("swap_exact_out", [addr(trader), addr(tokenOut), i128(amountOut), i128(maxIn), nativeToScVal(deadline, { type: "u64" })], signer, options);
  }

  async addLiquidity(input: AddLiquidityParams, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("add_liquidity", [addr(input.provider), i128(input.amountA), i128(input.amountB), i128(input.minShares), nativeToScVal(input.deadline, { type: "u64" })], signer, options);
  }

  async removeLiquidity(input: RemoveLiquidityParams, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<[bigint, bigint]>> {
    return this.execute("remove_liquidity", [addr(input.provider), i128(input.shares), i128(input.minAmountA), i128(input.minAmountB), nativeToScVal(input.deadline, { type: "u64" })], signer, options);
  }

  async removeLiquidityOneSided(provider: string, shares: bigint, tokenOut: string, minOut: bigint, deadline: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("remove_liquidity_one_sided", [addr(provider), i128(shares), addr(tokenOut), i128(minOut), nativeToScVal(deadline, { type: "u64" })], signer, options);
  }

  async flashLoan(receiver: string, amountA: bigint, amountB: bigint, data: Uint8Array, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<[bigint, bigint]>> {
    return this.execute("flash_loan", [addr(receiver), i128(amountA), i128(amountB), nativeToScVal(data, { type: "bytes" })], signer, options);
  }

  // ── Contract ID ────────────────────────────────────────────────────────────

  get contractId(): string {
    return this.contract.contractId();
  }
}
