/**
 * GovernanceClient — typed client for the LP-governed fee-voting contract.
 *
 * Every entrypoint this class names is a `pub fn` in
 * `contracts/governance/src/lib.rs`, and every argument is encoded in the shape
 * that contract's `#[contracttype]` definitions expect. `governance.test.ts`
 * enforces both mechanically: it reads the contract source, extracts the
 * `pub fn` names, and fails if this client sends anything else; it also asserts
 * the arity, order and XDR type of every builder below.
 *
 * Two kinds of method live here, and the distinction is load-bearing:
 *
 * - **Contract entrypoints.** One RPC call against one exported contract
 *   function. `proposalStatus` sends `proposal_status`.
 * - **Client-side compositions.** *Not* entrypoints. They page or fan out over
 *   real entrypoints, cost more than one RPC call, and say so in their own doc
 *   comment. `listProposalsByStatus` sends `get_proposals_paginated` and then
 *   `proposal_status` per page entry.
 *
 * Nothing in this file sends a method name the contract does not export.
 *
 * `docs/abi.json` is produced by `scripts/generate_abi.sh` from built WASM and
 * is currently behind the contract source (it still reports `vote`'s third
 * argument as a `bool` and lists 6 of the 20 `ProposalKind` variants), so it is
 * not used as the conformance oracle here. The test reads the Rust source
 * instead, which cannot go stale without the contract changing.
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
import {
  optionalBigInt,
  requireBigInt,
  requireBoolean,
  requireText,
  requireValue,
  toBigInt,
  toText,
  toVariant,
} from "./internal/decode.js";

// ── Contract error ─────────────────────────────────────────────────────────────

/**
 * A contract-returned `GovernanceError`, decoded from the numeric discriminant
 * Soroban reports. Carries both the discriminant and its symbolic name so
 * callers can branch on the specific failure instead of string-matching.
 */
export class GovernanceContractError extends Error {
  /** Numeric discriminant, matching `GovernanceError` in contracts/governance/src/lib.rs. */
  readonly code: GovernanceErrorCode;
  /** Symbolic variant name, e.g. `"ProposalNotFound"`. */
  readonly variant: GovernanceErrorName;
  /** Unmodified message reported by the RPC server. */
  readonly rawMessage: string;

  constructor(code: GovernanceErrorCode, rawMessage: string) {
    super(`Governance error: ${GovernanceErrorNames[code]}`);
    this.code = code;
    this.variant = GovernanceErrorNames[code];
    this.rawMessage = rawMessage;
  }
}

/** Matches the numeric-coded form Soroban RPC uses, e.g. `Error(Contract, #9)`. */
const CONTRACT_ERROR_PATTERN = /Error\s*\(\s*Contract\s*,\s*#(\d+)\s*\)/;

/**
 * `GovernanceError` discriminants, in the order the contract declares them.
 * Mirrors `enum GovernanceError` in contracts/governance/src/lib.rs.
 */
export const GovernanceErrorNames = {
  1: "AlreadyInitialized",
  2: "InvalidVotingPeriod",
  3: "InvalidTimelock",
  4: "InvalidQuorumBps",
  5: "InvalidProposerStake",
  6: "InvalidFeeBps",
  7: "ZeroTotalSupply",
  8: "InsufficientStake",
  9: "ProposalNotFound",
  10: "VotingNotStarted",
  11: "VotingPeriodEnded",
  12: "AlreadyExecuted",
  13: "ProposalCancelled",
  14: "AlreadyVoted",
  15: "NoVotingPower",
  16: "VotingPeriodActive",
  17: "ProposalExpired",
  18: "TimelockNotElapsed",
  19: "QuorumNotMet",
  20: "ProposalDefeated",
  21: "NotProposer",
  22: "NoLockedVote",
  23: "ProposalNotConcluded",
  24: "CannotDelegateToSelf",
  25: "Unauthorized",
  26: "HasDelegated",
  27: "DelegationCycle",
  28: "ProposalVetoed",
  29: "VetoWindowExpired",
  30: "NotVetoMultisig",
  31: "InsufficientSnapshotBal",
  32: "VetoMultisigNotSet",
  33: "NoPendingAdmin",
  34: "PartialFactoryUpdate",
  35: "NotInitialized",
} as const;

/** Numeric discriminant of a known `GovernanceError` variant. */
export type GovernanceErrorCode = keyof typeof GovernanceErrorNames;

/** Symbolic name of a known `GovernanceError` variant. */
export type GovernanceErrorName = (typeof GovernanceErrorNames)[GovernanceErrorCode];

/**
 * Decode a simulation or RPC failure into a typed `GovernanceContractError`
 * when Soroban reported a contract discriminant, and a plain `Error` otherwise
 * (host errors, network failures, unknown discriminants).
 */
export function decodeGovernanceError(err: unknown): Error {
  const msg = err instanceof Error ? err.message : String(err);
  const match = CONTRACT_ERROR_PATTERN.exec(msg);
  if (match) {
    const code = Number(match[1]) as GovernanceErrorCode;
    if (code in GovernanceErrorNames) {
      return new GovernanceContractError(code, msg);
    }
    return new Error(`Governance error: unknown contract error #${code}: ${msg}`);
  }
  return new Error(`Governance error: ${msg}`);
}

// ── XDR encoders ───────────────────────────────────────────────────────────────

function addr(address: string): xdr.ScVal {
  return nativeToScVal(Address.fromString(address));
}

function u32(value: number): xdr.ScVal {
  return nativeToScVal(value, { type: "u32" });
}

function i128(value: bigint): xdr.ScVal {
  return nativeToScVal(value, { type: "i128" });
}

function sym(name: string): xdr.ScVal {
  return nativeToScVal(name, { type: "symbol" });
}

function scvVec(items: xdr.ScVal[]): xdr.ScVal {
  return xdr.ScVal.scvVec(items);
}

/** Encode a `#[contracttype]` struct as the symbol-keyed `scvMap` the SDK expects. */
function scvStruct(entries: ReadonlyArray<readonly [string, xdr.ScVal]>): xdr.ScVal {
  return xdr.ScVal.scvMap(entries.map(([k, v]) => new xdr.ScMapEntry({ key: sym(k), val: v })));
}

/** Encode `Option<Address>` as `Some`/`None` — `scvVec([addr])` or `scvVoid`. */
function optAddr(value: string | null | undefined): xdr.ScVal {
  return value === null || value === undefined ? xdr.ScVal.scvVoid() : scvVec([addr(value)]);
}

// ── Types ──────────────────────────────────────────────────────────────────────

/**
 * On-chain proposal status. Mirrors `enum ProposalStatus` in the contract,
 * which gained `InDiscussion` and `Vetoed` for the multisig veto flow.
 */
export type ProposalStatus =
  | "Active"
  | "Pending"
  | "Queued"
  | "Executed"
  | "Defeated"
  | "Expired"
  | "Cancelled"
  | "InDiscussion"
  | "Vetoed";

/**
 * Vote choice passed to `vote`. Mirrors the contract's `enum Vote`, which is a
 * `#[contracttype]` enum and therefore encodes as `scvVec([scvSymbol(variant)])`
 * — not as a bare symbol. See {@link encodeVote}.
 */
export type VoteChoice = "For" | "Against" | "Abstain";

/** On-chain vote record for a voter. Mirrors `enum VoteRecord`. */
export type VoteRecord = "DidNotVote" | "VotedFor" | "VotedAgainst" | "VotedAbstain";

/** Payload of `ProposalKind::UpdateProtocolFee`. */
export interface UpdateProtocolFeeParams {
  newBps: bigint;
  newRecipient: string;
}

/** Payload of `ProposalKind::UpdateFactoryTreasury`. */
export interface UpdateFactoryTreasuryParams {
  factory: string;
  treasury: string;
  globalProtocolFeeBps: bigint;
}

/** Payload of `ProposalKind::UpdateFactoryGlobalFee`. */
export interface UpdateFactoryGlobalFeeParams {
  factory: string;
  offset: number;
  limit: number;
}

/** Payload of `ProposalKind::UpdateClOracle`. `oracle: null` detaches it. */
export interface UpdateClOracleParams {
  clPool: string;
  oracle: string | null;
}

/** Payload of `ProposalKind::UpdateClMaxOracleDeviation`. */
export interface UpdateClMaxOracleDeviationParams {
  clPool: string;
  maxDeviationBps: bigint;
}

/** Payload of `ProposalKind::UpdateClProtocolFee`. */
export interface UpdateClProtocolFeeParams {
  clPool: string;
  recipient: string;
  bps: bigint;
}

/** Payload of `ProposalKind::TransferClPoolAdmin`. */
export interface TransferClPoolAdminParams {
  clPool: string;
  newAdmin: string;
}

/** Payload of `ProposalKind::SetClPositionNft`. `nft: null` detaches it. */
export interface SetClPositionNftParams {
  clPool: string;
  nft: string | null;
}

/** Payload of `ProposalKind::CreatePolVesting`. */
export interface CreatePolVestingParams {
  polVesting: string;
  beneficiary: string;
  lpToken: string;
  pool: string;
  total: bigint;
  startLedger: number;
  cliffLedger: number;
  endLedger: number;
}

/**
 * What a proposal changes. Discriminated union over the 20 variants of the
 * contract's `enum ProposalKind`, in its declared order.
 *
 * Variants with a payload carry it inline as a value (`newFeeBps`, `newAdmin`,
 * `clPool`, `to`); variants whose payload is a `#[contracttype]` struct carry it
 * as `params`. The unit variants `PausePool` and `UnpausePool` carry nothing.
 */
export type ProposalKind =
  | { kind: "UpdateFee"; newFeeBps: bigint }
  | { kind: "UpdateFeeTier"; feeTier: number }
  | { kind: "UpdateProtocolFee"; params: UpdateProtocolFeeParams }
  | { kind: "UpdateFlashLoanFee"; newFeeBps: bigint }
  | { kind: "TransferAdmin"; newAdmin: string }
  | { kind: "PausePool" }
  | { kind: "UnpausePool" }
  | { kind: "EmergencyWithdraw"; to: string }
  | { kind: "UpdateFactoryTreasury"; params: UpdateFactoryTreasuryParams }
  | { kind: "UpdateFactoryGlobalFee"; params: UpdateFactoryGlobalFeeParams }
  | { kind: "CreatePolVesting"; params: CreatePolVestingParams }
  | { kind: "PauseClPool"; clPool: string }
  | { kind: "UnpauseClPool"; clPool: string }
  | { kind: "UpdateClOracle"; params: UpdateClOracleParams }
  | { kind: "UpdateClMaxOracleDeviation"; params: UpdateClMaxOracleDeviationParams }
  | { kind: "UpdateClProtocolFee"; params: UpdateClProtocolFeeParams }
  | { kind: "TransferClPoolAdmin"; params: TransferClPoolAdminParams }
  | { kind: "SetClPositionNft"; params: SetClPositionNftParams };

/** Variant names of the contract's `enum ProposalKind`, in declared order. */
export const PROPOSAL_KIND_VARIANTS = [
  "UpdateFee",
  "UpdateFeeTier",
  "UpdateProtocolFee",
  "UpdateFlashLoanFee",
  "TransferAdmin",
  "PausePool",
  "UnpausePool",
  "EmergencyWithdraw",
  "UpdateFactoryTreasury",
  "UpdateFactoryGlobalFee",
  "CreatePolVesting",
  "PauseClPool",
  "UnpauseClPool",
  "UpdateClOracle",
  "UpdateClMaxOracleDeviation",
  "UpdateClProtocolFee",
  "TransferClPoolAdmin",
  "SetClPositionNft",
] as const;

/** Governance configuration returned by `get_params`. */
export interface GovernanceParams {
  votingPeriodSecs: bigint;
  timelockSecs: bigint;
  quorumBps: bigint;
  minProposerStakeBps: bigint;
  /** Protocol multisig allowed to veto passed proposals; `null` when unset. */
  vetoMultisig: string | null;
  /** Extra quorum bps required per day open; 0 disables decay. */
  quorumDecayRateBpsPerDay: bigint;
}

/** On-chain veto audit trail returned by `get_veto_audit`. */
export interface VetoAudit {
  proposalId: number;
  vetoedBy: string;
  vetoedAt: bigint;
  discussionEnd: bigint;
}

/**
 * A proposal exactly as the contract's `#[contracttype] struct Proposal` stores
 * it — every field, and no `status`.
 *
 * The contract derives status from these fields plus the current ledger clock
 * and returns it from a separate `proposal_status` entrypoint, so it is not
 * part of this struct. See {@link Proposal} for the composed shape.
 */
export interface ProposalData {
  id: number;
  proposer: string;
  kind: ProposalKind;
  snapshotTotalSupply: bigint;
  /** Ledger sequence the voting-power snapshot was taken at. */
  snapshotLedger: number;
  voteStart: bigint;
  voteEnd: bigint;
  executeAfter: bigint;
  expiresAt: bigint;
  votesFor: bigint;
  votesAgainst: bigint;
  votesAbstain: bigint;
  executed: boolean;
  cancelled: boolean;
  vetoed: boolean;
  vetoedBy: string | null;
  vetoedAt: bigint | null;
  discussionEnd: bigint | null;
}

/** A proposal plus the status the contract derives for it. Client-side composition. */
export type Proposal = ProposalData & { status: ProposalStatus };

/** Page of proposal ids from a resumable status scan. */
export interface ProposalStatusPage {
  ids: number[];
  nextId: number;
}

// ── Enum encoders ──────────────────────────────────────────────────────────────

/**
 * Encode a `Vote` as the contract expects: a `#[contracttype]` enum, so a unit
 * variant is `scvVec([scvSymbol(variant)])`.
 *
 * The previous client passed `nativeToScVal(choice)`, which produced a bare
 * `scvString` and was rejected at argument conversion.
 */
export function encodeVote(choice: VoteChoice): xdr.ScVal {
  return scvVec([sym(choice)]);
}

/** Encode a `ProposalStatus` as `scvVec([scvSymbol(variant)])`. */
export function encodeProposalStatus(status: ProposalStatus): xdr.ScVal {
  return scvVec([sym(status)]);
}

/** Encode a `VoteRecord` as `scvVec([scvSymbol(variant)])`. */
export function encodeVoteRecord(record: VoteRecord): xdr.ScVal {
  return scvVec([sym(record)]);
}

/**
 * Encode a `ProposalKind` as the contract expects: `scvVec([scvSymbol(variant),
 * ...payload])`, with a `#[contracttype]` struct payload encoded as a
 * symbol-keyed `scvMap` inside the vec.
 *
 * The previous client encoded `UpdateFee` as the map `{ UpdateFee: n }`; the
 * contract rejected it at argument conversion.
 */
export function encodeProposalKind(kind: ProposalKind): xdr.ScVal {
  switch (kind.kind) {
    case "UpdateFee":
      return scvVec([sym(kind.kind), i128(kind.newFeeBps)]);
    case "UpdateFeeTier":
      return scvVec([sym(kind.kind), i128(BigInt(kind.feeTier))]);
    case "UpdateProtocolFee":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["new_bps", i128(kind.params.newBps)],
          ["new_recipient", addr(kind.params.newRecipient)],
        ]),
      ]);
    case "UpdateFlashLoanFee":
      return scvVec([sym(kind.kind), i128(kind.newFeeBps)]);
    case "TransferAdmin":
      return scvVec([sym(kind.kind), addr(kind.newAdmin)]);
    case "PausePool":
    case "UnpausePool":
      return scvVec([sym(kind.kind)]);
    case "EmergencyWithdraw":
      return scvVec([sym(kind.kind), addr(kind.to)]);
    case "UpdateFactoryTreasury":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["factory", addr(kind.params.factory)],
          ["treasury", addr(kind.params.treasury)],
          ["global_protocol_fee_bps", i128(kind.params.globalProtocolFeeBps)],
        ]),
      ]);
    case "UpdateFactoryGlobalFee":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["factory", addr(kind.params.factory)],
          ["offset", u32(kind.params.offset)],
          ["limit", u32(kind.params.limit)],
        ]),
      ]);
    case "CreatePolVesting":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["pol_vesting", addr(kind.params.polVesting)],
          ["beneficiary", addr(kind.params.beneficiary)],
          ["lp_token", addr(kind.params.lpToken)],
          ["pool", addr(kind.params.pool)],
          ["total", i128(kind.params.total)],
          ["start_ledger", u32(kind.params.startLedger)],
          ["cliff_ledger", u32(kind.params.cliffLedger)],
          ["end_ledger", u32(kind.params.endLedger)],
        ]),
      ]);
    case "PauseClPool":
    case "UnpauseClPool":
      return scvVec([sym(kind.kind), addr(kind.clPool)]);
    case "UpdateClOracle":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["cl_pool", addr(kind.params.clPool)],
          ["oracle", optAddr(kind.params.oracle)],
        ]),
      ]);
    case "UpdateClMaxOracleDeviation":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["cl_pool", addr(kind.params.clPool)],
          ["max_deviation_bps", i128(kind.params.maxDeviationBps)],
        ]),
      ]);
    case "UpdateClProtocolFee":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["cl_pool", addr(kind.params.clPool)],
          ["recipient", addr(kind.params.recipient)],
          ["bps", i128(kind.params.bps)],
        ]),
      ]);
    case "TransferClPoolAdmin":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["cl_pool", addr(kind.params.clPool)],
          ["new_admin", addr(kind.params.newAdmin)],
        ]),
      ]);
    case "SetClPositionNft":
      return scvVec([
        sym(kind.kind),
        scvStruct([
          ["cl_pool", addr(kind.params.clPool)],
          ["nft", optAddr(kind.params.nft)],
        ]),
      ]);
  }
}

// ── Decoders ───────────────────────────────────────────────────────────────────

/**
 * Decode an `Option<Address>` field.
 *
 * `scValToNative` reads `Some(x)` as a one-element array and `None` as `null`,
 * so a bare string is not the shape to expect here.
 */
function toOptionalText(value: unknown): string | null {
  if (value === null || value === undefined) return null;
  if (Array.isArray(value)) return value.length === 0 ? null : toText(value[0]);
  return toText(value);
}

/**
 * Decode an `Option<u64>` field.
 *
 * `scValToNative` reads `Some(x)` as a one-element array and `None` as `null`.
 */
/**
 * Decode a `#[contracttype]` struct payload into camelCase fields.
 *
 * The contract keys the map by the Rust field name, so each entry pairs the
 * on-chain (snake_case) key with the TypeScript (camelCase) key it lands on.
 */
function decodeParamsStruct(
  native: Record<string, unknown>,
  fields: ReadonlyArray<readonly [string, string, (v: unknown) => unknown]>
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [contractKey, tsKey, decode] of fields) out[tsKey] = decode(native[contractKey]);
  return out;
}

/**
 * Decode a `ProposalKind` from `scValToNative` output.
 *
 * The inverse of {@link encodeProposalKind}: a `scvVec` reads back as an array
 * whose head is the variant name and whose tail is the payload.
 */
export function decodeProposalKind(value: unknown): ProposalKind {
  if (!Array.isArray(value) || typeof value[0] !== "string") {
    throw new TypeError("expected a ProposalKind contract value");
  }
  // `Array.isArray` narrows to `any[]`; read the slots out individually with
  // explicit types rather than destructuring an `any`.
  const variant: string = value[0];
  const payload: unknown = value[1];
  switch (variant) {
    case "UpdateFee":
      return { kind: variant, newFeeBps: toBigInt(payload) };
    case "UpdateFeeTier":
      return { kind: variant, feeTier: Number(toBigInt(payload)) };
    case "UpdateProtocolFee":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["new_bps", "newBps", toBigInt],
          ["new_recipient", "newRecipient", toText],
        ]) as unknown as UpdateProtocolFeeParams,
      };
    case "UpdateFlashLoanFee":
      return { kind: variant, newFeeBps: toBigInt(payload) };
    case "TransferAdmin":
      return { kind: variant, newAdmin: toText(payload) };
    case "PausePool":
    case "UnpausePool":
      return { kind: variant };
    case "EmergencyWithdraw":
      return { kind: variant, to: toText(payload) };
    case "UpdateFactoryTreasury":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["factory", "factory", toText],
          ["treasury", "treasury", toText],
          ["global_protocol_fee_bps", "globalProtocolFeeBps", toBigInt],
        ]) as unknown as UpdateFactoryTreasuryParams,
      };
    case "UpdateFactoryGlobalFee":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["factory", "factory", toText],
          ["offset", "offset", (v) => Number(toBigInt(v))],
          ["limit", "limit", (v) => Number(toBigInt(v))],
        ]) as unknown as UpdateFactoryGlobalFeeParams,
      };
    case "CreatePolVesting":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["pol_vesting", "polVesting", toText],
          ["beneficiary", "beneficiary", toText],
          ["lp_token", "lpToken", toText],
          ["pool", "pool", toText],
          ["total", "total", toBigInt],
          ["start_ledger", "startLedger", (v) => Number(toBigInt(v))],
          ["cliff_ledger", "cliffLedger", (v) => Number(toBigInt(v))],
          ["end_ledger", "endLedger", (v) => Number(toBigInt(v))],
        ]) as unknown as CreatePolVestingParams,
      };
    case "PauseClPool":
    case "UnpauseClPool":
      return { kind: variant, clPool: toText(payload) };
    case "UpdateClOracle":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["cl_pool", "clPool", toText],
          ["oracle", "oracle", toOptionalText],
        ]) as unknown as UpdateClOracleParams,
      };
    case "UpdateClMaxOracleDeviation":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["cl_pool", "clPool", toText],
          ["max_deviation_bps", "maxDeviationBps", toBigInt],
        ]) as unknown as UpdateClMaxOracleDeviationParams,
      };
    case "UpdateClProtocolFee":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["cl_pool", "clPool", toText],
          ["recipient", "recipient", toText],
          ["bps", "bps", toBigInt],
        ]) as unknown as UpdateClProtocolFeeParams,
      };
    case "TransferClPoolAdmin":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["cl_pool", "clPool", toText],
          ["new_admin", "newAdmin", toText],
        ]) as unknown as TransferClPoolAdminParams,
      };
    case "SetClPositionNft":
      return {
        kind: variant,
        params: decodeParamsStruct(payload as Record<string, unknown>, [
          ["cl_pool", "clPool", toText],
          ["nft", "nft", toOptionalText],
        ]) as unknown as SetClPositionNftParams,
      };
    default:
      throw new TypeError(`unknown ProposalKind variant: ${String(variant)}`);
  }
}

// ── GovernanceClient ──────────────────────────────────────────────────────────

/** Default page size for the client-side paginations below. */
const DEFAULT_PAGE_SIZE = 50;

export class GovernanceClient {
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
    return simulateRead(
      this.server,
      this.contract,
      this.networkPassphrase,
      method,
      args,
      decodeGovernanceError
    );
  }

  private async executeWrite<T>(method: string, args: xdr.ScVal[], signer: Signer, options?: InvokeOptions): Promise<InvocationResult<T>> {
    return invoke<T>({ server: this.server, contract: this.contract, method, args,
      source: await signer.publicKey(), signer, networkPassphrase: this.networkPassphrase,
      decodeError: decodeGovernanceError, ...options });
  }

  private proposalFromNative(native: Record<string, unknown>): ProposalData {
    return {
      id: Number(requireBigInt(native, "id", "Proposal")),
      proposer: requireText(native, "proposer", "Proposal"),
      kind: decodeProposalKind(requireValue(native, "kind", "Proposal")),
      snapshotTotalSupply: requireBigInt(native, "snapshot_total_supply", "Proposal"),
      snapshotLedger: Number(requireBigInt(native, "snapshot_ledger", "Proposal")),
      voteStart: requireBigInt(native, "vote_start", "Proposal"),
      voteEnd: requireBigInt(native, "vote_end", "Proposal"),
      executeAfter: requireBigInt(native, "execute_after", "Proposal"),
      expiresAt: requireBigInt(native, "expires_at", "Proposal"),
      votesFor: requireBigInt(native, "votes_for", "Proposal"),
      votesAgainst: requireBigInt(native, "votes_against", "Proposal"),
      votesAbstain: requireBigInt(native, "votes_abstain", "Proposal"),
      executed: requireBoolean(native, "executed", "Proposal"),
      cancelled: requireBoolean(native, "cancelled", "Proposal"),
      vetoed: requireBoolean(native, "vetoed", "Proposal"),
      vetoedBy: toOptionalText(native.vetoed_by),
      vetoedAt: optionalBigInt(native.vetoed_at),
      discussionEnd: optionalBigInt(native.discussion_end),
    };
  }

  private paramsFromNative(native: Record<string, unknown>): GovernanceParams {
    return {
      votingPeriodSecs: requireBigInt(native, "voting_period_secs", "GovernanceParams"),
      timelockSecs: requireBigInt(native, "timelock_secs", "GovernanceParams"),
      quorumBps: requireBigInt(native, "quorum_bps", "GovernanceParams"),
      minProposerStakeBps: requireBigInt(native, "min_proposer_stake_bps", "GovernanceParams"),
      vetoMultisig: toOptionalText(native.veto_multisig),
      quorumDecayRateBpsPerDay: requireBigInt(native, "quorum_decay_rate_bps_per_day", "GovernanceParams"),
    };
  }

  // ── Read entrypoints ────────────────────────────────────────────────────────

  /** Returns the current governance configuration. Sends `get_params`. */
  async getParams(): Promise<GovernanceParams> {
    const raw = await this.simulate("get_params");
    return this.paramsFromNative(scValToNative(raw) as Record<string, unknown>);
  }

  /** Returns the total number of proposals ever created. Sends `get_proposal_count`. */
  async getProposalCount(): Promise<number> {
    const raw = await this.simulate("get_proposal_count");
    return Number(scValToNative(raw));
  }

  /** Alias for {@link getProposalCount}. Sends `get_proposal_count`. */
  async proposalCount(): Promise<number> {
    return this.getProposalCount();
  }

  /**
   * Returns the stored proposal for `proposalId`. Sends `get_proposal`.
   *
   * The returned `ProposalData` has no `status`: the contract's `Proposal`
   * struct does not carry one, and status depends on the current ledger clock.
   * Use {@link proposalStatus} for it, or {@link getProposalWithStatus} for
   * both in one call.
   *
   * @throws {GovernanceContractError} `ProposalNotFound` for an unknown id.
   */
  async getProposal(proposalId: number): Promise<ProposalData> {
    const raw = await this.simulate("get_proposal", u32(proposalId));
    return this.proposalFromNative(scValToNative(raw) as Record<string, unknown>);
  }

  /**
   * Derives the current status of a proposal. Sends `proposal_status`.
   *
   * This is the only place status comes from — the stored `Proposal` struct has
   * no `status` field.
   */
  async proposalStatus(proposalId: number): Promise<ProposalStatus> {
    const raw = await this.simulate("proposal_status", u32(proposalId));
    return toVariant(scValToNative(raw), "Active") as ProposalStatus;
  }

  /**
   * Reads a proposal and its derived status.
   *
   * Client-side composition: sends `get_proposal` then `proposal_status`, so it
   * costs two RPC calls where {@link getProposal} costs one.
   */
  async getProposalWithStatus(proposalId: number): Promise<Proposal> {
    const [proposal, status] = await Promise.all([
      this.getProposal(proposalId),
      this.proposalStatus(proposalId),
    ]);
    return { ...proposal, status };
  }

  /**
   * Like {@link getProposalWithStatus}, but resolves to `null` for an unknown
   * id instead of throwing.
   *
   * Client-side composition: the contract has no `try_get_proposal`, so this
   * sends `get_proposal` and maps the `ProposalNotFound` discriminant to
   * `null`. Every other failure still throws.
   */
  async tryGetProposal(proposalId: number): Promise<Proposal | null> {
    try {
      return await this.getProposalWithStatus(proposalId);
    } catch (err) {
      if (err instanceof GovernanceContractError && err.code === 9) return null;
      throw err;
    }
  }

  /**
   * Lists proposals by ascending id, paginated. Sends `get_proposals_paginated`.
   *
   * A page can come back shorter than `min(limit, count - offset)` when a
   * proposal's persistent entry has had its TTL lapse; the contract skips such
   * ids rather than failing the whole page. See {@link forEachProposal}.
   */
  async listProposals(offset: number, limit: number): Promise<ProposalData[]> {
    const raw = await this.simulate("get_proposals_paginated", u32(offset), u32(limit));
    const native = scValToNative(raw) as Array<Record<string, unknown>>;
    return native.map((n) => this.proposalFromNative(n));
  }

  /**
   * Client-side pagination over every proposal, in ascending id order.
   *
   * The loop is driven by `get_proposal_count` and `offset`, never by page
   * length, because a page can legitimately come back short. This is a
   * composition, not a contract entrypoint.
   */
  private async forEachProposal(
    visit: (proposal: ProposalData) => boolean | Promise<boolean>,
    pageSize: number = DEFAULT_PAGE_SIZE
  ): Promise<void> {
    const count = await this.getProposalCount();
    for (let offset = 0; offset < count; offset += pageSize) {
      const page = await this.listProposals(offset, pageSize);
      for (const proposal of page) {
        if (!(await visit(proposal))) return;
      }
    }
  }

  /**
   * Lists proposals by ascending id, newest first.
   *
   * Client-side composition: `get_proposals_paginated` is ascending-only, so
   * this pages the whole set via {@link forEachProposal} and reverses the
   * result. `offset`/`limit` apply after the reversal.
   */
  async listProposalsDesc(offset: number, limit: number): Promise<ProposalData[]> {
    const all: ProposalData[] = [];
    await this.forEachProposal((p) => {
      all.push(p);
      return true;
    });
    return all.reverse().slice(offset, offset + limit);
  }

  /**
   * Returns ids of proposals in `status`, paginated.
   *
   * Client-side composition: pages `get_proposals_paginated` and calls
   * `proposal_status` for each entry. Costs roughly one extra RPC call per
   * proposal scanned — do not run it over a large proposal set on a hot path.
   */
  async listProposalsByStatus(
    status: ProposalStatus,
    offset: number,
    limit: number
  ): Promise<number[]> {
    const ids: number[] = [];
    await this.forEachProposal(async (p) => {
      if ((await this.proposalStatus(p.id)) === status) ids.push(p.id);
      return true;
    });
    return ids.slice(offset, offset + limit);
  }

  /**
   * Resumable status scan: a page of matching ids and the id to resume from.
   *
   * Client-side composition over `get_proposals_paginated` and
   * `proposal_status`, preserving the original contract-shaped contract:
   * `nextId` is the exclusive upper bound of the ids examined.
   */
  async listProposalsByStatusFrom(
    status: ProposalStatus,
    startId: number,
    scanLimit: number
  ): Promise<ProposalStatusPage> {
    const ids: number[] = [];
    let examined = 0;
    await this.forEachProposal(async (p) => {
      if (p.id < startId) return true;
      if (examined >= scanLimit) return false;
      examined += 1;
      if ((await this.proposalStatus(p.id)) === status) ids.push(p.id);
      return true;
    });
    return { ids: ids.sort((a, b) => a - b), nextId: startId + examined };
  }

  /**
   * Counts proposals in `status`.
   *
   * Client-side composition: scans `get_proposals_paginated` and calls
   * `proposal_status` per entry.
   */
  async countProposalsByStatus(status: ProposalStatus): Promise<number> {
    let total = 0;
    await this.forEachProposal(async (p) => {
      if ((await this.proposalStatus(p.id)) === status) total += 1;
      return true;
    });
    return total;
  }

  /**
   * Ids of proposals currently in the `Active` status.
   *
   * Client-side composition over `get_proposals_paginated` and
   * `proposal_status`.
   */
  async getActiveProposalIds(): Promise<number[]> {
    return this.listProposalsByStatus("Active", 0, Number.MAX_SAFE_INTEGER);
  }

  /**
   * Proposal ids proposed by `proposer`, paginated.
   *
   * Client-side composition: filters `get_proposals_paginated` by the stored
   * `proposer` field, which needs no extra RPC call per proposal.
   */
  async getProposalsByProposer(
    proposer: string,
    offset: number,
    limit: number
  ): Promise<number[]> {
    const ids: number[] = [];
    await this.forEachProposal((p) => {
      if (p.proposer === proposer) ids.push(p.id);
      return true;
    });
    return ids.slice(offset, offset + limit);
  }

  /**
   * How `voter` voted on `proposalId`. Sends `get_vote_info`.
   *
   * `DidNotVote` covers both "has not voted" and "no such voter".
   */
  async getVoteInfo(proposalId: number, voter: string): Promise<VoteRecord> {
    const raw = await this.simulate("get_vote_info", u32(proposalId), addr(voter));
    return toVariant(scValToNative(raw), "DidNotVote") as VoteRecord;
  }

  /** Alias for {@link getVoteInfo}. Sends `get_vote_info`. */
  async getVoteRecord(proposalId: number, voter: string): Promise<VoteRecord> {
    return this.getVoteInfo(proposalId, voter);
  }

  /**
   * Whether `voter` has voted on `proposalId`.
   *
   * Client-side composition over `get_vote_info` — the contract stores votes
   * under a `(proposal_id, voter)` key and exposes no `has_voted`.
   */
  async hasVoted(proposalId: number, voter: string): Promise<boolean> {
    return (await this.getVoteInfo(proposalId, voter)) !== "DidNotVote";
  }

  /**
   * The delegation target for `from`, or `null` if not delegated.
   * Sends `get_delegate`.
   */
  async getDelegate(from: string): Promise<string | null> {
    const raw = await this.simulate("get_delegate", addr(from));
    return toOptionalText(scValToNative(raw));
  }

  /**
   * The quorum in bps a proposal must reach, after decay.
   * Sends `get_effective_quorum`.
   */
  async getEffectiveQuorum(proposalId: number): Promise<bigint> {
    const raw = await this.simulate("get_effective_quorum", u32(proposalId));
    return toBigInt(scValToNative(raw));
  }

  /**
   * The veto audit record for a proposal, or `null` if it was never vetoed.
   * Sends `get_veto_audit`.
   */
  async getVetoAudit(proposalId: number): Promise<VetoAudit | null> {
    const raw = await this.simulate("get_veto_audit", u32(proposalId));
    const native = scValToNative(raw) as Record<string, unknown> | null;
    if (native === null || native === undefined) return null;
    return {
      proposalId: Number(requireBigInt(native, "proposal_id", "VetoAudit")),
      vetoedBy: toText(native.vetoed_by),
      vetoedAt: requireBigInt(native, "vetoed_at", "VetoAudit"),
      discussionEnd: requireBigInt(native, "discussion_end", "VetoAudit"),
    };
  }

  /**
   * `holder`'s LP balance as of the proposal's snapshot ledger — the balance
   * that determines their voting power, not their current balance.
   * Sends `get_snapshot_balance`.
   */
  async getSnapshotBalance(proposalId: number, holder: string): Promise<bigint> {
    const raw = await this.simulate("get_snapshot_balance", u32(proposalId), addr(holder));
    return toBigInt(scValToNative(raw));
  }

  // ── Write-method parameter builders ─────────────────────────────────────────

  /**
   * Parameters for `propose(proposer, kind)`.
   *
   * Covers every `ProposalKind` variant; the kind is encoded as
   * `scvVec([scvSymbol(variant), ...payload])`.
   */
  proposeParams(proposer: string, kind: ProposalKind): xdr.ScVal[] {
    return [addr(proposer), encodeProposalKind(kind)];
  }

  /** Parameters for `propose` with a fee update. Thin wrapper over {@link proposeParams}. */
  proposeUpdateFeeParams(proposer: string, newFeeBps: bigint): xdr.ScVal[] {
    return this.proposeParams(proposer, { kind: "UpdateFee", newFeeBps });
  }

  /** Parameters for `vote(voter, proposal_id, choice)`. */
  voteParams(voter: string, proposalId: number, choice: VoteChoice): xdr.ScVal[] {
    return [addr(voter), u32(proposalId), encodeVote(choice)];
  }

  /** Parameters for `execute(proposal_id)`. */
  executeParams(proposalId: number): xdr.ScVal[] {
    return [u32(proposalId)];
  }

  /** Parameters for `cancel_proposal(proposal_id, proposer)`. */
  cancelParams(proposalId: number, proposer: string): xdr.ScVal[] {
    return [u32(proposalId), addr(proposer)];
  }

  /**
   * Parameters for `unlock_vote(voter, proposal_id)`.
   *
   * The address comes first: the contract's parameter order is
   * `(voter: Address, proposal_id: u32)`, not the reverse.
   */
  unlockVoteParams(voter: string, proposalId: number): xdr.ScVal[] {
    return [addr(voter), u32(proposalId)];
  }

  /** Parameters for `veto(proposal_id)`. Requires the veto multisig to sign. */
  vetoParams(proposalId: number): xdr.ScVal[] {
    return [u32(proposalId)];
  }

  /** Parameters for `delegate(from, to)`. */
  delegateParams(from: string, to: string): xdr.ScVal[] {
    return [addr(from), addr(to)];
  }

  /** Parameters for `undelegate(from)`. */
  undelegateParams(from: string): xdr.ScVal[] {
    return [addr(from)];
  }

  async propose(proposer: string, kind: ProposalKind, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<number>> {
    return this.executeWrite("propose", this.proposeParams(proposer, kind), signer, options);
  }

  async vote(voter: string, proposalId: number, choice: VoteChoice, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.executeWrite("vote", this.voteParams(voter, proposalId, choice), signer, options);
  }

  async execute(proposalId: number, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.executeWrite("execute", this.executeParams(proposalId), signer, options);
  }

  async cancel(proposalId: number, proposer: string, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.executeWrite("cancel_proposal", this.cancelParams(proposalId, proposer), signer, options);
  }

  async delegate(from: string, to: string, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.executeWrite("delegate", this.delegateParams(from, to), signer, options);
  }

  async undelegate(from: string, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.executeWrite("undelegate", this.undelegateParams(from), signer, options);
  }

  async unlockVote(voter: string, proposalId: number, signer: Signer, options?: InvokeOptions): Promise<InvocationResult<void>> {
    return this.executeWrite("unlock_vote", this.unlockVoteParams(voter, proposalId), signer, options);
  }
}
