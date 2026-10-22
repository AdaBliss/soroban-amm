import { CONTRACT_METADATA, type ContractKind } from "./generated/contract-metadata.js";

export class ProtocolContractError extends Error {
  constructor(
    public readonly contract: ContractKind,
    public readonly code: number,
    public readonly variant: string
  ) {
    super(`${contract}.${variant} (${code})`);
    this.name = "ProtocolContractError";
  }
}

export function decodeProtocolContractError(contract: ContractKind, message: string): Error {
  const code = Number(/Error\(Contract,\s*#(\d+)\)/.exec(message)?.[1]);
  if (!Number.isInteger(code)) return new Error(message);
  const errors = CONTRACT_METADATA[contract].errors as Record<number, string>;
  return new ProtocolContractError(contract, code, errors[code] ?? "UnknownContractError");
}

export { CONTRACT_METADATA };
export type { ContractKind };
