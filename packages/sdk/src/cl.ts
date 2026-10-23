/**
 * ConcentratedLiquidityClient — typed client for the tick-based CL AMM contract.
 *
 * Covers the public interface of contracts/concentrated_liquidity/src/lib.rs.
 */

import {
  Contract,
  rpc as StellarRpc,
  nativeToScVal,
  scValToNative,
  xdr,
  Address,
} from "@stellar/stellar-sdk";
import type { NetworkConfig } from "./types.js";
import { simulateRead } from "./internal/simulate.js";
import { invoke } from "./internal/invoke.js";
import type { InvocationResult, InvokeOptions, Signer } from "./internal/invoke.js";
import { ContractDecodeError, requireBigInt, toBigInt } from "./internal/decode.js";

// ── Helpers ────────────────────────────────────────────────────────────────────

function addr(address: string): xdr.ScVal {
  return nativeToScVal(Address.fromString(address));
}

function i128(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "i128" });
}

function i32(value: number): xdr.ScVal {
  return nativeToScVal(value, { type: "i32" });
}

function u64(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "u64" });
}

function u128(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "u128" });
}

// ── Types ──────────────────────────────────────────────────────────────────────

/** A liquidity position returned by `get_position`. */
export interface Position {
  lowerTick: number;
  upperTick: number;
  liquidity: bigint;
  feeGrowthInsideA: bigint;
  feeGrowthInsideB: bigint;
  tokensOwedA: bigint;
  tokensOwedB: bigint;
}

/** Full pool state returned by `get_pool_state`. */
export interface ClPoolState {
  sqrtPrice: bigint;
  currentTick: number;
  activeLiquidity: bigint;
  tickSpacing: number;
}

/** Quote returned by `quote_position`. */
export interface PositionQuote {
  amountA: bigint;
  amountB: bigint;
  liquidity: bigint;
}

/** Detailed quote returned by `estimate_price_impact`. */
export interface PriceImpactEstimate {
  amountIn: bigint;
  amountInAfterFee: bigint;
  amountOut: bigint;
  feeAmount: bigint;
  spotPriceBefore: bigint;
  effectivePrice: bigint;
  priceImpactBps: bigint;
  sqrtPriceBefore: bigint;
  sqrtPriceAfter: bigint;
  tickBefore: number;
  tickAfter: number;
  activeLiquidityBefore: bigint;
  activeLiquidityAfter: bigint;
}

// ── ConcentratedLiquidityClient ───────────────────────────────────────────────

export class ConcentratedLiquidityClient {
  private readonly server: StellarRpc.Server;
  private readonly contract: Contract;
  private readonly networkPassphrase: string;

  constructor(config: NetworkConfig) {
    this.server = new StellarRpc.Server(config.rpcUrl);
    this.contract = new Contract(config.contractId);
    this.networkPassphrase = config.networkPassphrase;
  }

  get contractId(): string {
    return this.contract.contractId();
  }

  private async simulate(method: string, ...args: xdr.ScVal[]): Promise<xdr.ScVal> {
    return simulateRead(this.server, this.contract, this.networkPassphrase, method, args);
  }

  private async execute<T>(method: string, args: xdr.ScVal[], signer: Signer, options?: InvokeOptions): Promise<InvocationResult<T>> {
    return invoke<T>({ server: this.server, contract: this.contract, method, args,
      source: await signer.publicKey(), signer, networkPassphrase: this.networkPassphrase, ...options });
  }

  // ── Read-only methods ──────────────────────────────────────────────────────

  /** Returns the full pool state (sqrt price, current tick, active liquidity, tick spacing). */
  async getPoolState(): Promise<ClPoolState> {
    const raw = await this.simulate("get_pool_state");
    const native = scValToNative(raw) as Record<string, unknown>;
    return {
      sqrtPrice: requireBigInt(native, "sqrt_price", "PoolState"),
      currentTick: Number(requireBigInt(native, "current_tick", "PoolState")),
      activeLiquidity: requireBigInt(native, "active_liquidity", "PoolState"),
      tickSpacing: Number(requireBigInt(native, "tick_spacing", "PoolState")),
    };
  }

  /** Returns the current active tick. */
  async currentTick(): Promise<number> {
    const raw = await this.simulate("current_tick");
    return Number(scValToNative(raw));
  }

  /** Returns the current active liquidity across the current tick. */
  async activeLiquidity(): Promise<bigint> {
    const raw = await this.simulate("active_liquidity");
    return toBigInt(scValToNative(raw));
  }

  /**
   * Returns the tick cumulative and its timestamp `(tick_cumulative, last_ts)`.
   *
   * Used by the TWAP consumer via `save_cl_snapshot`.
   */
  async getTickCumulative(): Promise<{ tickCumulative: bigint; lastTimestamp: bigint }> {
    const raw = await this.simulate("get_tick_cumulative");
    const native = scValToNative(raw) as [unknown, unknown];
    return {
      tickCumulative: toBigInt(native[0]),
      lastTimestamp: toBigInt(native[1]),
    };
  }

  /** Returns the position for `owner` between `lowerTick` and `upperTick`. */
  async getPosition(owner: string, lowerTick: number, upperTick: number): Promise<Position> {
    const raw = await this.simulate("get_position", addr(owner), i32(lowerTick), i32(upperTick));
    const native = scValToNative(raw) as Record<string, unknown>;
    const posLower = Number(requireBigInt(native, "lower_tick", "Position"));
    const posUpper = Number(requireBigInt(native, "upper_tick", "Position"));
    const liquidity = requireBigInt(native, "liquidity", "Position");
    const feeGrowthInsideA = requireBigInt(native, "fee_growth_inside_a", "Position");
    const feeGrowthInsideB = requireBigInt(native, "fee_growth_inside_b", "Position");
    const owed = native.tokens_owed;
    if (!Array.isArray(owed) || owed.length !== 2) {
      throw new ContractDecodeError("Position", "tokens_owed", Object.keys(native));
    }
    return {
      lowerTick: posLower,
      upperTick: posUpper,
      liquidity,
      feeGrowthInsideA,
      feeGrowthInsideB,
      tokensOwedA: toBigInt(owed?.[0]),
      tokensOwedB: toBigInt(owed?.[1]),
    };
  }

  /**
   * Quotes how much token A and token B are required — and how much liquidity
   * would be minted — for a position between `lowerTick` and `upperTick` given
   * desired deposit amounts.
   */
  async quotePosition(
    lowerTick: number,
    upperTick: number,
    amountA: bigint,
    amountB: bigint
  ): Promise<PositionQuote> {
    const raw = await this.simulate(
      "quote_position",
      i32(lowerTick),
      i32(upperTick),
      i128(amountA),
      i128(amountB)
    );
    const native = scValToNative(raw) as [unknown, unknown, unknown];
    return {
      amountA: toBigInt(native[0]),
      amountB: toBigInt(native[1]),
      liquidity: toBigInt(native[2]),
    };
  }

  /**
   * Estimates swap output and price impact for a concentrated-liquidity swap.
   *
   * The contract walks initialized ticks with the same math used by `swap`, so
   * this is suitable for frontends, slippage previews, and route comparison.
   */
  async estimatePriceImpact(
    zeroForOne: boolean,
    amountIn: bigint,
    sqrtPriceLimit: bigint
  ): Promise<PriceImpactEstimate> {
    const raw = await this.simulate(
      "estimate_price_impact",
      nativeToScVal(zeroForOne),
      i128(amountIn),
      nativeToScVal(sqrtPriceLimit, { type: "u128" })
    );
    const native = scValToNative(raw) as Record<string, unknown>;
    return {
      amountIn: requireBigInt(native, "amount_in", "PriceImpactEstimate"),
      amountInAfterFee: requireBigInt(native, "amount_in_after_fee", "PriceImpactEstimate"),
      amountOut: requireBigInt(native, "amount_out", "PriceImpactEstimate"),
      feeAmount: requireBigInt(native, "fee_amount", "PriceImpactEstimate"),
      spotPriceBefore: requireBigInt(native, "spot_price_before", "PriceImpactEstimate"),
      effectivePrice: requireBigInt(native, "effective_price", "PriceImpactEstimate"),
      priceImpactBps: requireBigInt(native, "price_impact_bps", "PriceImpactEstimate"),
      sqrtPriceBefore: requireBigInt(native, "sqrt_price_before", "PriceImpactEstimate"),
      sqrtPriceAfter: requireBigInt(native, "sqrt_price_after", "PriceImpactEstimate"),
      tickBefore: Number(requireBigInt(native, "tick_before", "PriceImpactEstimate")),
      tickAfter: Number(requireBigInt(native, "tick_after", "PriceImpactEstimate")),
      activeLiquidityBefore: requireBigInt(native, "active_liquidity_before", "PriceImpactEstimate"),
      activeLiquidityAfter: requireBigInt(native, "active_liquidity_after", "PriceImpactEstimate"),
    };
  }

  /**
   * Returns the fee growth inside the tick range `[lowerTick, upperTick]` for
   * both tokens as `(fee_growth_inside_a, fee_growth_inside_b)`.
   */
  async feeGrowthInside(
    lowerTick: number,
    upperTick: number
  ): Promise<{ feeGrowthA: bigint; feeGrowthB: bigint }> {
    const raw = await this.simulate("fee_growth_inside", i32(lowerTick), i32(upperTick));
    const native = scValToNative(raw) as [unknown, unknown];
    return {
      feeGrowthA: toBigInt(native[0]),
      feeGrowthB: toBigInt(native[1]),
    };
  }

  /**
   * Converts a tick index to the corresponding price ratio scaled by 1_000_000.
   * Price = 1.0001^tick, returned as (price * 1_000_000).
   */
  async tickToPrice(tick: number): Promise<bigint> {
    const raw = await this.simulate("tick_to_price", i32(tick));
    return toBigInt(scValToNative(raw));
  }

  /**
   * Returns the tick cumulative `secondsAgo` seconds in the past.
   *
   * Mirrors `ConcentratedLiquidity::observe` —
   * contracts/concentrated_liquidity/src/lib.rs:3294
   * `(seconds_ago: u64) -> i64`. The contract takes a single timestamp, not a
   * list, and returns a single cumulative value. Use {@link observeBatch} to
   * sample several points; each point is one RPC call.
   */
  async observe(secondsAgo: bigint): Promise<bigint> {
    const raw = await this.simulate("observe", u64(secondsAgo));
    return toBigInt(scValToNative(raw));
  }

  /**
   * Sample {@link observe} at several `secondsAgo` points, in order.
   *
   * Client-side convenience, not a contract entrypoint: the contract only
   * exposes the single-point `observe`, so this loops and costs one RPC call per
   * point.
   */
  async observeBatch(secondsAgos: bigint[]): Promise<bigint[]> {
    const out: bigint[] = [];
    for (const secondsAgo of secondsAgos) out.push(await this.observe(secondsAgo));
    return out;
  }

  /** Returns whether the pool is paused. */
  async isPaused(): Promise<boolean> {
    const raw = await this.simulate("is_paused");
    return Boolean(scValToNative(raw));
  }

  // ── Write-method parameter types ───────────────────────────────────────────

  /**
   * Parameters for `mint_position`.
   *
   * Mirrors `ConcentratedLiquidity::mint_position` —
   * contracts/concentrated_liquidity/src/lib.rs:706
   * `(provider: Address, lower_tick: i32, upper_tick: i32,
   *   amount_a_desired: i128, amount_b_desired: i128, min_a: i128,
   *   min_b: i128, deadline: u64)`
   *
   * The trailing `deadline` is a Unix timestamp (seconds); the contract rejects
   * a call whose ledger time has passed it with `DeadlineExpired`. `minA`/`minB`
   * are independent per-token slippage floors, not a single combined
   * minimum-liquidity bound.
   */
  mintPositionParams(
    provider: string,
    lowerTick: number,
    upperTick: number,
    amountADesired: bigint,
    amountBDesired: bigint,
    minA: bigint,
    minB: bigint,
    deadline: bigint
  ): xdr.ScVal[] {
    return [
      addr(provider),
      i32(lowerTick),
      i32(upperTick),
      i128(amountADesired),
      i128(amountBDesired),
      i128(minA),
      i128(minB),
      u64(deadline),
    ];
  }

  /**
   * Parameters for `modify_position`.
   *
   * Mirrors `ConcentratedLiquidity::modify_position` —
   * contracts/concentrated_liquidity/src/lib.rs:889
   * `(provider: Address, lower_tick: i32, upper_tick: i32,
   *   liquidity_delta: i128, min_a: i128, min_b: i128, deadline: u64)`
   *
   * Returns `(amount_a, amount_b)`.
   */
  modifyPositionParams(
    provider: string,
    lowerTick: number,
    upperTick: number,
    liquidityDelta: bigint,
    minA: bigint,
    minB: bigint,
    deadline: bigint
  ): xdr.ScVal[] {
    return [addr(provider), i32(lowerTick), i32(upperTick), i128(liquidityDelta), i128(minA), i128(minB), u64(deadline)];
  }

  /**
   * Parameters for `burn_position`.
   *
   * Mirrors `ConcentratedLiquidity::burn_position` —
   * contracts/concentrated_liquidity/src/lib.rs:1567
   * `(provider: Address, lower_tick: i32, upper_tick: i32, liquidity: i128)`
   *
   * `liquidity` specifies how much of the position to burn and must be
   * positive; the contract rejects non-positive values with `ZeroLiquidity`.
   *
   * Returns `(amount_a, amount_b)` of tokens sent back to the provider.
   */
  burnPositionParams(
    provider: string,
    lowerTick: number,
    upperTick: number,
    liquidity: bigint
  ): xdr.ScVal[] {
    return [addr(provider), i32(lowerTick), i32(upperTick), i128(liquidity)];
  }

  /**
   * Parameters for `collect_fees`.
   *
   * Mirrors `ConcentratedLiquidity::collect_fees` —
   * contracts/concentrated_liquidity/src/lib.rs:1627
   * `(provider: Address, lower_tick: i32, upper_tick: i32)`
   *
   * Returns `(fee_a, fee_b)`.
   */
  collectFeesParams(provider: string, lowerTick: number, upperTick: number): xdr.ScVal[] {
    return [addr(provider), i32(lowerTick), i32(upperTick)];
  }

  /**
   * Parameters for `swap`.
   *
   * Mirrors `ConcentratedLiquidity::swap` —
   * contracts/concentrated_liquidity/src/lib.rs:2146
   * `(sender: Address, zero_for_one: bool, amount_in: i128,
   *   sqrt_price_limit_x96: u128, min_amount_out: i128, deadline: u64)`
   *
   * Direction is expressed as `zeroForOne` (true = swapping token A in for
   * token B out), not as an input-token address — the contract takes a `bool`
   * in that position. `sqrtPriceLimitX96` is an unsigned Q64.96 value and must
   * be encoded as `u128`, not `i128`.
   */
  swapParams(
    sender: string,
    zeroForOne: boolean,
    amountIn: bigint,
    sqrtPriceLimitX96: bigint,
    minAmountOut: bigint,
    deadline: bigint
  ): xdr.ScVal[] {
    return [
      addr(sender),
      nativeToScVal(zeroForOne, { type: "bool" }),
      i128(amountIn),
      u128(sqrtPriceLimitX96),
      i128(minAmountOut),
      u64(deadline),
    ];
  }

  swapExactOutParams(sender: string, zeroForOne: boolean, amountOut: bigint, sqrtPriceLimitX96: bigint, maxAmountIn: bigint, deadline: bigint): xdr.ScVal[] {
    return [addr(sender), nativeToScVal(zeroForOne, { type: "bool" }), i128(amountOut), u128(sqrtPriceLimitX96), i128(maxAmountIn), u64(deadline)];
  }

  async mintPosition(provider: string, lowerTick: number, upperTick: number, amountADesired: bigint, amountBDesired: bigint, minA: bigint, minB: bigint, deadline: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<PositionQuote>> {
    return this.execute("mint_position", this.mintPositionParams(provider, lowerTick, upperTick, amountADesired, amountBDesired, minA, minB, deadline), signer, options);
  }

  async modifyPosition(provider: string, lowerTick: number, upperTick: number, liquidityDelta: bigint, minA: bigint, minB: bigint, deadline: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<[bigint, bigint]>> {
    return this.execute("modify_position", this.modifyPositionParams(provider, lowerTick, upperTick, liquidityDelta, minA, minB, deadline), signer, options);
  }

  async burnPosition(provider: string, lowerTick: number, upperTick: number, liquidity: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<[bigint, bigint]>> {
    return this.execute("burn_position", this.burnPositionParams(provider, lowerTick, upperTick, liquidity), signer, options);
  }

  async collectFees(provider: string, lowerTick: number, upperTick: number, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<[bigint, bigint]>> {
    return this.execute("collect_fees", this.collectFeesParams(provider, lowerTick, upperTick), signer, options);
  }

  async swap(sender: string, zeroForOne: boolean, amountIn: bigint, sqrtPriceLimitX96: bigint, minAmountOut: bigint, deadline: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("swap", this.swapParams(sender, zeroForOne, amountIn, sqrtPriceLimitX96, minAmountOut, deadline), signer, options);
  }

  async swapExactOut(sender: string, zeroForOne: boolean, amountOut: bigint, sqrtPriceLimitX96: bigint, maxAmountIn: bigint, deadline: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("swap_exact_out", this.swapExactOutParams(sender, zeroForOne, amountOut, sqrtPriceLimitX96, maxAmountIn, deadline), signer, options);
  }
}
