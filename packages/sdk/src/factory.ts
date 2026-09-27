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
import { toText } from "./internal/decode.js";

// ── Helpers ────────────────────────────────────────────────────────────────────

function addr(address: string): xdr.ScVal {
  return nativeToScVal(Address.fromString(address));
}

function i128(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "i128" });
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

  /** Returns the pool count (monotonic counter used to derive deployment salts). */
  async poolCount(): Promise<bigint> {
    const raw = await this.simulate("pool_count");
    return BigInt(String(scValToNative(raw)));
  }

  // ── Write-method parameter types ───────────────────────────────────────────

  /**
   * Parameters for `create_pool(token_a, token_b, fee_bps, governance_wasm_hash)`.
   *
   * `governanceWasmHash` should be a 32-byte hex string, or omit the field to
   * deploy a pool without governance.
   */
  createPoolParams(
    tokenA: string,
    tokenB: string,
    feeBps: bigint,
    governanceWasmHash?: string
  ): xdr.ScVal[] {
    const govHash =
      governanceWasmHash !== undefined
        ? nativeToScVal(Buffer.from(governanceWasmHash, "hex"), { type: "bytes" })
        : xdr.ScVal.scvVoid();
    return [addr(tokenA), addr(tokenB), i128(feeBps), govHash];
  }
}
