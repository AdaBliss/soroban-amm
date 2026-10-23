/**
 * FactoryClient — typed client for the pool factory contract.
 *
 * Covers the public interface of contracts/factory/src/lib.rs.
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

/**
 * Encode `Option<BytesN<32>>` the way the contract expects: `None` is `scvVoid`
 * and `Some` is `scvVec([scvBytes(32)])`, matching `soroban_sdk`'s `Option`
 * conversion.
 */
function governanceHash(value?: string): xdr.ScVal {
  if (value === undefined) return xdr.ScVal.scvVoid();
  return xdr.ScVal.scvVec([
    nativeToScVal(Buffer.from(value, "hex"), { type: "bytes" }),
  ]);
}

// ── Types ──────────────────────────────────────────────────────────────────────

/** Result of `create_pool` — the pool address and optional governance address. */
export interface CreatePoolResult {
  poolAddress: string;
  governanceAddress: string | null;
}

// ── FactoryClient ─────────────────────────────────────────────────────────────

export class FactoryClient {
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
   * Look up the pool address for a token pair.
   *
   * Token order is normalised by the factory — pass them in either order.
   * Returns `null` if no pool exists for this pair.
   */
  async getPool(tokenA: string, tokenB: string): Promise<string | null> {
    const raw = await this.simulate("get_pool", addr(tokenA), addr(tokenB));
    const native: unknown = scValToNative(raw);
    return native !== null && native !== undefined ? toText(native) : null;
  }

  /** Returns the addresses of all deployed AMM pools. */
  async allPools(): Promise<string[]> {
    const raw = await this.simulate("all_pools");
    const native = scValToNative(raw) as unknown[];
    return (native ?? []).map(String);
  }

  /** Returns the LP token address for a given pool, or `null` if not found. */
  async getLpToken(pool: string): Promise<string | null> {
    const raw = await this.simulate("get_lp_token", addr(pool));
    const native: unknown = scValToNative(raw);
    return native !== null && native !== undefined ? toText(native) : null;
  }

  /**
   * Returns the governance contract address for a given pool,
   * or `null` if no governance was deployed for that pool.
   */
  async getGovernance(pool: string): Promise<string | null> {
    const raw = await this.simulate("get_governance", addr(pool));
    const native: unknown = scValToNative(raw);
    return native !== null && native !== undefined ? toText(native) : null;
  }

  /** Returns the number of AMM pools deployed by this factory. */
  async poolCount(): Promise<bigint> {
    // The exported entrypoint is `get_pool_count`; the contract's `pool_count`
    // is a private helper and is not callable over RPC.
    const raw = await this.simulate("get_pool_count");
    return toBigInt(scValToNative(raw));
  }

  // ── Write-method parameter types ───────────────────────────────────────────

  /**
   * Parameters for `create_pool`.
   *
   * Mirrors `Factory::create_pool` — contracts/factory/src/lib.rs:253
   * `(caller: Address, token_a: Address, token_b: Address, fee_tier: i128,
   *   governance_wasm_hash: Option<BytesN<32>>)`
   *
   * The contract calls `caller.require_auth()`, so `caller` must be passed
   * explicitly. `feeTier` is the 0–3 standard tier index, not basis points; for
   * a custom fee use {@link createPoolWithFeeBpsParams}. `governanceWasmHash` is
   * a 32-byte hex string, or omit it to deploy a pool without governance.
   */
  createPoolParams(
    caller: string,
    tokenA: string,
    tokenB: string,
    feeTier: bigint,
    governanceWasmHash?: string
  ): xdr.ScVal[] {
    return [
      addr(caller),
      addr(tokenA),
      addr(tokenB),
      i128(feeTier),
      governanceHash(governanceWasmHash),
    ];
  }

  /**
   * Parameters for `create_pool_with_fee_bps`.
   *
   * Mirrors `Factory::create_pool_with_fee_bps` — contracts/factory/src/lib.rs:277
   * `(caller: Address, token_a: Address, token_b: Address, fee_bps: i128,
   *   governance_wasm_hash: Option<BytesN<32>>)`
   *
   * Identical to {@link createPoolParams} except `feeBps` is a custom fee in
   * basis points (0–10_000) rather than a standard tier index.
   */
  createPoolWithFeeBpsParams(
    caller: string,
    tokenA: string,
    tokenB: string,
    feeBps: bigint,
    governanceWasmHash?: string
  ): xdr.ScVal[] {
    return [
      addr(caller),
      addr(tokenA),
      addr(tokenB),
      i128(feeBps),
      governanceHash(governanceWasmHash),
    ];
  }

  createClPoolParams(caller: string, tokenA: string, tokenB: string, feeBps: bigint, initialTick: number): xdr.ScVal[] {
    return [addr(caller), addr(tokenA), addr(tokenB), i128(feeBps), nativeToScVal(initialTick, { type: "i32" })];
  }

  async createPool(caller: string, tokenA: string, tokenB: string, feeTier: bigint, signer: Signer, governanceWasmHash?: string, options?: InvokeOptions): Promise<InvocationResult<CreatePoolResult>> {
    return this.execute("create_pool", this.createPoolParams(caller, tokenA, tokenB, feeTier, governanceWasmHash), signer, options);
  }

  async createClPool(caller: string, tokenA: string, tokenB: string, feeBps: bigint, initialTick: number, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<string>> {
    return this.execute("create_cl_pool", this.createClPoolParams(caller, tokenA, tokenB, feeBps, initialTick), signer, options);
  }
}
