/**
 * GovernanceClient — typed client for the LP-governed fee-voting contract.
 *
 * Covers the public interface of contracts/governance/src/lib.rs.
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
import { toBigInt, toText, toVariant } from "./internal/decode.js";

// ── Helpers ────────────────────────────────────────────────────────────────────

function addr(address: string): xdr.ScVal {
  return nativeToScVal(Address.fromString(address));
}

function u32(value: number): xdr.ScVal {
  return nativeToScVal(value, { type: "u32" });
}

// ── Types ──────────────────────────────────────────────────────────────────────

/** On-chain proposal status. */
export type ProposalStatus =
  | "Active"
  | "Pending"
  | "Queued"
  | "Executed"
  | "Defeated"
  | "Expired"
  | "Cancelled";

/** Vote choice passed to `vote`. */
export type VoteChoice = "For" | "Against" | "Abstain";

/** On-chain vote record for a voter. */
export type VoteRecord = "DidNotVote" | "VotedFor" | "VotedAgainst" | "VotedAbstain";

/** A voter's participation record on a proposal. */
export interface VoterRecord {
  voter: string;
  vote: VoteRecord;
  weight: bigint;
}

/** Page of proposal ids from a resumable status scan. */
export interface ProposalStatusPage {
  ids: number[];
  nextId: number;
}

/** Governance configuration returned by `get_params`. */
export interface GovernanceParams {
  votingPeriodSecs: bigint;
  timelockSecs: bigint;
  quorumBps: bigint;
  minProposerStakeBps: bigint;
}

/** On-chain proposal data returned by `get_proposal`. */
export interface Proposal {
  id: number;
  proposer: string;
  snapshotTotalSupply: bigint;
  voteStart: bigint;
  voteEnd: bigint;
  executeAfter: bigint;
  expiresAt: bigint;
  votesFor: bigint;
  votesAgainst: bigint;
  votesAbstain: bigint;
  executed: boolean;
  cancelled: boolean;
  status: ProposalStatus;
}

// ── GovernanceClient ──────────────────────────────────────────────────────────

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
    return simulateRead(this.server, this.contract, this.networkPassphrase, method, args);
  }

  private proposalFromNative(native: Record<string, unknown>, fallbackId?: number): Proposal {
    const id = Number(native.id ?? fallbackId ?? 0);
    return {
      id,
      proposer: toText(native.proposer ?? ""),
      snapshotTotalSupply: toBigInt(native.snapshot_total_supply),
      voteStart: toBigInt(native.vote_start),
      voteEnd: toBigInt(native.vote_end),
      executeAfter: toBigInt(native.execute_after),
      expiresAt: toBigInt(native.expires_at),
      votesFor: toBigInt(native.votes_for),
      votesAgainst: toBigInt(native.votes_against),
      votesAbstain: toBigInt(native.votes_abstain),
      executed: Boolean(native.executed),
      cancelled: Boolean(native.cancelled),
      status: toVariant(native.status, "Active") as ProposalStatus,
    };
  }

  // ── Read-only methods ──────────────────────────────────────────────────────

  /** Returns the current governance configuration. */
  async getParams(): Promise<GovernanceParams> {
    const raw = await this.simulate("get_params");
    const native = scValToNative(raw) as Record<string, unknown>;
    return {
      votingPeriodSecs: toBigInt(native.voting_period_secs),
      timelockSecs: toBigInt(native.timelock_secs),
      quorumBps: toBigInt(native.quorum_bps),
      minProposerStakeBps: toBigInt(native.min_proposer_stake_bps),
    };
  }

  /** Returns the total number of proposals created so far. */
  async getProposalCount(): Promise<number> {
    const raw = await this.simulate("get_proposal_count");
    return Number(scValToNative(raw));
  }

  /** Alias for `getProposalCount`. */
  async proposalCount(): Promise<number> {
    return this.getProposalCount();
  }

  /** Returns the on-chain data for `proposalId`. */
  async getProposal(proposalId: number): Promise<Proposal> {
    const raw = await this.simulate("get_proposal", u32(proposalId));
    return this.proposalFromNative(scValToNative(raw) as Record<string, unknown>, proposalId);
  }

  /** Returns `None` (via `null`) if the proposal id is unknown. */
  async tryGetProposal(proposalId: number): Promise<Proposal | null> {
    const raw = await this.simulate("try_get_proposal", u32(proposalId));
    const native: unknown = scValToNative(raw);
    if (native === null || native === undefined) return null;
    return this.proposalFromNative(native as Record<string, unknown>, proposalId);
  }

  /** Lists proposals by ascending id, paginated. */
  async listProposals(offset: number, limit: number): Promise<Proposal[]> {
    const raw = await this.simulate("list_proposals", u32(offset), u32(limit));
    const native = scValToNative(raw) as Array<Record<string, unknown>>;
    return native.map((n) => this.proposalFromNative(n));
  }

  /** Lists proposals by descending id (newest first), paginated. */
  async listProposalsDesc(offset: number, limit: number): Promise<Proposal[]> {
    const raw = await this.simulate("list_proposals_desc", u32(offset), u32(limit));
    const native = scValToNative(raw) as Array<Record<string, unknown>>;
    return native.map((n) => this.proposalFromNative(n));
  }

  /** Returns ids of proposals matching `status`, paginated. */
  async listProposalsByStatus(status: ProposalStatus, offset: number, limit: number): Promise<number[]> {
    const raw = await this.simulate("list_proposals_by_status", nativeToScVal(status), u32(offset), u32(limit));
    return scValToNative(raw) as number[];
  }

  /** Returns all currently active proposal ids (bounded by the contract). */
  async getActiveProposalIds(): Promise<number[]> {
    const raw = await this.simulate("get_active_proposal_ids");
    return scValToNative(raw) as number[];
  }

  /** Counts proposals by status. */
  async countProposalsByStatus(status: ProposalStatus): Promise<number> {
    const raw = await this.simulate("count_proposals_by_status", nativeToScVal(status));
    return Number(scValToNative(raw));
  }

  /** Resumable status scan: returns a page of ids and the next id to resume from. */
  async listProposalsByStatusFrom(
    status: ProposalStatus,
    startId: number,
    scanLimit: number
  ): Promise<ProposalStatusPage> {
    const raw = await this.simulate(
      "list_proposals_by_status_from",
      nativeToScVal(status),
      u32(startId),
      u32(scanLimit)
    );
    const native = scValToNative(raw) as [number[], number];
    return { ids: native[0], nextId: native[1] };
  }

  /** Returns proposal ids proposed by a specific address, paginated. */
  async getProposalsByProposer(proposer: string, offset: number, limit: number): Promise<number[]> {
    const raw = await this.simulate("get_proposals_by_proposer", addr(proposer), u32(offset), u32(limit));
    return scValToNative(raw) as number[];
  }

  /** Returns the number of voters on a proposal. */
  async getVoterCount(proposalId: number): Promise<number> {
    const raw = await this.simulate("get_voter_count", u32(proposalId));
    return Number(scValToNative(raw));
  }

  /** Lists voters on a proposal with their recorded choice and weight. */
  async listVoters(proposalId: number, offset: number, limit: number): Promise<VoterRecord[]> {
    const raw = await this.simulate("list_voters", u32(proposalId), u32(offset), u32(limit));
    const native = scValToNative(raw) as Array<Record<string, unknown>>;
    return native.map((n) => ({
      voter: toText(n.voter ?? ""),
      vote: toVariant(n.vote, "DidNotVote") as VoteRecord,
      weight: toBigInt(n.weight),
    }));
  }

  /** Returns addresses that delegate to `delegate`, paginated. */
  async getDelegators(delegate: string, offset: number, limit: number): Promise<string[]> {
    const raw = await this.simulate("get_delegators", addr(delegate), u32(offset), u32(limit));
    return scValToNative(raw) as string[];
  }

  /** Returns whether `voter` has voted on `proposalId`. */
  async hasVoted(proposalId: number, voter: string): Promise<boolean> {
    const raw = await this.simulate("has_voted", u32(proposalId), addr(voter));
    return Boolean(scValToNative(raw));
  }

  /** Returns the vote record for `voter` on `proposalId`. */
  async getVoteRecord(proposalId: number, voter: string): Promise<VoteRecord> {
    const raw = await this.simulate("get_vote_record", u32(proposalId), addr(voter));
    const native: unknown = scValToNative(raw);
    return toVariant(native, "DidNotVote") as VoteRecord;
  }

  /** Returns the delegation target for `from`, or `null` if not delegated. */
  async getDelegate(from: string): Promise<string | null> {
    const raw = await this.simulate("get_delegate", addr(from));
    const native: unknown = scValToNative(raw);
    return native !== null && native !== undefined ? toText(native) : null;
  }

  // ── Write-method parameter types ───────────────────────────────────────────

  /** Parameters for `propose(proposer, kind)` — returns the new proposal id. */
  proposeUpdateFeeParams(proposer: string, newFeeBps: bigint): xdr.ScVal[] {
    return [
      addr(proposer),
      nativeToScVal({ UpdateFee: newFeeBps }, { type: "map" }),
    ];
  }

  /** Parameters for `vote(voter, proposal_id, vote_choice)`. */
  voteParams(voter: string, proposalId: number, choice: VoteChoice): xdr.ScVal[] {
    return [addr(voter), u32(proposalId), nativeToScVal(choice)];
  }

  /** Parameters for `execute(proposal_id)`. */
  executeParams(proposalId: number): xdr.ScVal[] {
    return [u32(proposalId)];
  }

  /** Parameters for `cancel(proposal_id, caller)`. */
  cancelParams(proposalId: number, caller: string): xdr.ScVal[] {
    return [u32(proposalId), addr(caller)];
  }

  /** Parameters for `unlock_vote(proposal_id, voter)`. */
  unlockVoteParams(proposalId: number, voter: string): xdr.ScVal[] {
    return [u32(proposalId), addr(voter)];
  }

  /** Parameters for `delegate(from, to)`. */
  delegateParams(from: string, to: string): xdr.ScVal[] {
    return [addr(from), addr(to)];
  }
}
