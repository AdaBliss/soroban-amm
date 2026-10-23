/**
 * RouterClient — typed client for the multi-hop swap router contract.
 *
 * Covers the public interface of contracts/router/src/lib.rs.
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
import { toBigInt, toText } from "./internal/decode.js";

// ── Helpers ────────────────────────────────────────────────────────────────────

function addr(address: string): xdr.ScVal {
  return nativeToScVal(Address.fromString(address));
}

function i128(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "i128" });
}

function u64(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "u64" });
}

/** Encode a swap path as the `Vec<Address>` the router expects. */
function addressVec(path: string[]): xdr.ScVal {
  return xdr.ScVal.scvVec(path.map(addr));
}

// ── Types ──────────────────────────────────────────────────────────────────────

/** Input for `swap_exact_in`. */
export interface SwapExactInInput {
  /** Address whose funds are swapped. The contract calls `require_auth` on it. */
  trader: string;
  /** Swap path, from input token to output token. Must contain >= 2 tokens. */
  path: string[];
  /** Exact amount of `path[0]` to send in. Must be positive. */
  amountIn: bigint;
  /** Minimum acceptable amount of the final token (slippage guard). */
  minAmountOut: bigint;
  /** Unix timestamp after which the contract rejects the swap. */
  deadline: bigint;
}

/** Input for `swap_exact_out`. */
export interface SwapExactOutInput {
  /** Address whose funds are swapped. The contract calls `require_auth` on it. */
  trader: string;
  /** Swap path, from input token to output token. Must contain >= 2 tokens. */
  path: string[];
  /** Exact amount of the final token to receive. Must be positive. */
  amountOut: bigint;
  /** Maximum acceptable amount of `path[0]` to spend (slippage guard). */
  maxIn: bigint;
  /** Unix timestamp after which the contract rejects the swap. */
  deadline: bigint;
}

// ── RouterClient ──────────────────────────────────────────────────────────────

export class RouterClient {
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

  /**
   * Quote the output of a multi-hop swap without executing it.
   *
   * Mirrors `Router::get_amount_out_path` — contracts/router/src/lib.rs:155
   * `(path: Vec<Address>, amount_in: i128)`
   *
   * Note the argument order: the path comes first, then the amount. Returns
   * `0` when any hop in the path has no registered pool.
   */
  async getAmountOutPath(path: string[], amountIn: bigint): Promise<bigint> {
    const raw = await this.simulate(
      "get_amount_out_path",
      addressVec(path),
      i128(amountIn)
    );
    return toBigInt(scValToNative(raw));
  }

  /** Returns the factory address this router resolves pools through. */
  async getFactory(): Promise<string> {
    const raw = await this.simulate("get_factory");
    return toText(scValToNative(raw));
  }

  // ── Write-method parameter types ───────────────────────────────────────────
  //
  // These methods require a signed transaction envelope. The parameter types
  // are provided here to support typed integration layers; submitting the
  // transaction is the caller's responsibility using the Stellar SDK.

  /**
   * Parameters for `swap_exact_in`.
   *
   * Mirrors `Router::swap_exact_in` — contracts/router/src/lib.rs:34
   * `(trader: Address, path: Vec<Address>, amount_in: i128,
   *   min_amount_out: i128, deadline: u64)`
   *
   * `trader` must be passed explicitly: the contract calls
   * `trader.require_auth()` and moves that address's tokens, so signing the
   * envelope alone is not enough. The contract has no recipient parameter —
   * output is always credited to `trader`.
   */
  swapExactInParams(input: SwapExactInInput): xdr.ScVal[] {
    return [
      addr(input.trader),
      addressVec(input.path),
      i128(input.amountIn),
      i128(input.minAmountOut),
      u64(input.deadline),
    ];
  }

  /**
   * Parameters for `swap_exact_out`.
   *
   * Mirrors `Router::swap_exact_out` — contracts/router/src/lib.rs:84
   * `(trader: Address, path: Vec<Address>, amount_out: i128,
   *   max_in: i128, deadline: u64)`
   *
   * As with `swap_exact_in`, `trader` is a real contract argument and output is
   * always credited to `trader`.
   */
  swapExactOutParams(input: SwapExactOutInput): xdr.ScVal[] {
    return [
      addr(input.trader),
      addressVec(input.path),
      i128(input.amountOut),
      i128(input.maxIn),
      u64(input.deadline),
    ];
  }

  async swapExactIn(input: SwapExactInInput, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("swap_exact_in", this.swapExactInParams(input), signer, options);
  }

  async swapExactOut(input: SwapExactOutInput, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<bigint>> {
    return this.execute("swap_exact_out", this.swapExactOutParams(input), signer, options);
  }
}
