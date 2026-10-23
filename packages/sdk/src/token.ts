/**
 * TokenClient — typed client for the SEP-41 LP token contract.
 *
 * Covers the public interface of contracts/token/src/lib.rs.
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

function u32(value: number): xdr.ScVal {
  return nativeToScVal(value, { type: "u32" });
}

// ── TokenClient ───────────────────────────────────────────────────────────────

export class TokenClient {
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
    return invoke<T>({
      server: this.server,
      contract: this.contract,
      networkPassphrase: this.networkPassphrase,
      source: await signer.publicKey(),
      signer,
      method,
      args,
      ...options,
    });
  }

  // ── Read-only methods ──────────────────────────────────────────────────────

  /** Returns the token name. */
  async name(): Promise<string> {
    const raw = await this.simulate("name");
    return toText(scValToNative(raw));
  }

  /** Returns the token symbol. */
  async symbol(): Promise<string> {
    const raw = await this.simulate("symbol");
    return toText(scValToNative(raw));
  }

  /** Returns the number of decimal places used to represent token amounts. */
  async decimals(): Promise<number> {
    const raw = await this.simulate("decimals");
    return Number(scValToNative(raw));
  }

  /** Returns the total number of tokens currently in circulation. */
  async totalSupply(): Promise<bigint> {
    const raw = await this.simulate("total_supply");
    return toBigInt(scValToNative(raw));
  }

  /** Returns the token balance of `address`. Returns `0n` if no balance. */
  async balance(address: string): Promise<bigint> {
    const raw = await this.simulate("balance", addr(address));
    return toBigInt(scValToNative(raw));
  }

  /**
   * Returns the amount `spender` is allowed to transfer on behalf of `from`.
   * Returns `0n` if no allowance has been set.
   */
  async allowance(from: string, spender: string): Promise<bigint> {
    const raw = await this.simulate("allowance", addr(from), addr(spender));
    return toBigInt(scValToNative(raw));
  }

  // ── Write-method parameter types ───────────────────────────────────────────
  //
  // These methods require a signed transaction envelope. The parameter types
  // are provided here to support typed integration layers; submitting the
  // transaction is the caller's responsibility using the Stellar SDK.

  /**
   * Parameters for `transfer`.
   *
   * Mirrors `LpToken::transfer` — contracts/token/src/lib.rs:289
   * `(from: Address, to: Address, amount: i128)`
   */
  transferParams(from: string, to: string, amount: bigint): xdr.ScVal[] {
    return [addr(from), addr(to), i128(amount)];
  }

  /**
   * Parameters for `transfer_from`.
   *
   * Mirrors `LpToken::transfer_from` — contracts/token/src/lib.rs:303
   * `(spender: Address, from: Address, to: Address, amount: i128)`
   */
  transferFromParams(spender: string, from: string, to: string, amount: bigint): xdr.ScVal[] {
    return [addr(spender), addr(from), addr(to), i128(amount)];
  }

  /**
   * Parameters for `approve`.
   *
   * Mirrors `LpToken::approve` — contracts/token/src/lib.rs:334
   * `(from: Address, spender: Address, amount: i128, live_until_ledger: u32)`
   *
   * `liveUntilLedger` is the ledger sequence through which the allowance is
   * valid. The contract requires it to be >= the current ledger sequence
   * whenever `amount > 0`, rejecting the call with `InvalidExpiry` otherwise.
   * This is the SEP-41 `live_until_ledger` argument a previous version of this
   * builder omitted. Setting `amount` to `0` revokes the allowance.
   */
  approveParams(
    from: string,
    spender: string,
    amount: bigint,
    liveUntilLedger: number
  ): xdr.ScVal[] {
    return [addr(from), addr(spender), i128(amount), u32(liveUntilLedger)];
  }

  /**
   * Parameters for `mint` — admin only.
   *
   * Mirrors `LpToken::mint` — contracts/token/src/lib.rs:360
   * `(to: Address, amount: i128)`
   */
  mintParams(to: string, amount: bigint): xdr.ScVal[] {
    return [addr(to), i128(amount)];
  }

  /**
   * Parameters for `burn` — admin only.
   *
   * Mirrors `LpToken::burn` — contracts/token/src/lib.rs:384
   * `(from: Address, amount: i128)`
   */
  burnParams(from: string, amount: bigint): xdr.ScVal[] {
    return [addr(from), i128(amount)];
  }

  async transfer(from: string, to: string, amount: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.execute("transfer", this.transferParams(from, to, amount), signer, options);
  }

  async transferFrom(spender: string, from: string, to: string, amount: bigint, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.execute("transfer_from", this.transferFromParams(spender, from, to, amount), signer, options);
  }

  async approve(from: string, spender: string, amount: bigint, liveUntilLedger: number, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.execute("approve", this.approveParams(from, spender, amount, liveUntilLedger), signer, options);
  }
}
