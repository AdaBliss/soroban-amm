import {
  Address,
  Contract,
  nativeToScVal,
  rpc as StellarRpc,
  scValToNative,
  xdr,
} from "@stellar/stellar-sdk";
import type { NetworkConfig } from "../types.js";
import { simulateRead } from "./simulate.js";
import { decodeProtocolContractError } from "../contract-errors.js";
import type { ContractKind } from "../contract-errors.js";

export const address = (value: string): xdr.ScVal => nativeToScVal(Address.fromString(value));
export const i128 = (value: bigint): xdr.ScVal => nativeToScVal(value, { type: "i128" });
export const u64 = (value: bigint): xdr.ScVal => nativeToScVal(value, { type: "u64" });
export const u32 = (value: number): xdr.ScVal => nativeToScVal(value, { type: "u32" });
export const bool = (value: boolean): xdr.ScVal => nativeToScVal(value);
export const value = (input: unknown): xdr.ScVal => nativeToScVal(input);

export abstract class ReadContractClient {
  protected abstract readonly contractKind: ContractKind;
  protected readonly server: StellarRpc.Server;
  protected readonly contract: Contract;
  protected readonly networkPassphrase: string;

  constructor(config: NetworkConfig) {
    this.server = new StellarRpc.Server(config.rpcUrl);
    this.contract = new Contract(config.contractId);
    this.networkPassphrase = config.networkPassphrase;
  }

  get contractId(): string { return this.contract.contractId(); }

  protected async read<T>(method: string, ...args: xdr.ScVal[]): Promise<T> {
    const value = await simulateRead(
      this.server,
      this.contract,
      this.networkPassphrase,
      method,
      args,
      (message) => decodeProtocolContractError(this.contractKind, message)
    );
    return scValToNative(value) as T;
  }

  protected params(...args: xdr.ScVal[]): xdr.ScVal[] { return args; }
}
