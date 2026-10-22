import type { xdr } from "@stellar/stellar-sdk";
import { ReadContractClient, address, i128, u32, u64 } from "./internal/contract-client.js";

export interface Campaign {
  id: bigint;
  pool: string;
  lp_token: string;
  reward_token: string;
  start_time: bigint;
  end_time: bigint;
  reward_rate: bigint;
  funding_amount: bigint;
  total_distributed: bigint;
  active: boolean;
}

export interface CreateCampaignInput {
  caller: string; pool: string; lpToken: string; rewardToken: string;
  startTime: bigint; endTime: bigint; rewardRate: bigint; fundingAmount: bigint;
}

export class IncentiveCampaignsClient extends ReadContractClient {
  protected readonly contractKind = "incentive_campaigns" as const;
  getCampaign(id: bigint): Promise<Campaign> { return this.read("get_campaign", u64(id)); }
  listCampaignsPaginated(offset = 0, limit = 50): Promise<bigint[]> { return this.read("list_campaigns_paginated", u32(offset), u32(limit)); }
  getActiveCampaignsPaginated(offset = 0, limit = 50): Promise<Campaign[]> { return this.read("get_active_campaigns_paginated", u32(offset), u32(limit)); }
  getClaimHistory(provider: string, offset = 0, limit = 50): Promise<unknown[]> { return this.read("get_claim_history", address(provider), u32(offset), u32(limit)); }
  listDistributionRecords(campaignId: bigint, offset = 0, limit = 50): Promise<unknown[]> { return this.read("list_distribution_records", u64(campaignId), u32(offset), u32(limit)); }
  getCampaignAccrual(id: bigint): Promise<[bigint, bigint]> { return this.read("get_campaign_accrual", u64(id)); }
  claimRewardsParams(provider: string, campaignId: bigint): xdr.ScVal[] { return this.params(address(provider), u64(campaignId)); }
  createCampaignParams(input: CreateCampaignInput): xdr.ScVal[] {
    return this.params(address(input.caller), address(input.pool), address(input.lpToken), address(input.rewardToken), u64(input.startTime), u64(input.endTime), i128(input.rewardRate), i128(input.fundingAmount));
  }
  setCampaignRateParams(caller: string, id: bigint, rate: bigint): xdr.ScVal[] { return this.params(address(caller), u64(id), i128(rate)); }
  recoverLeftoverFundsParams(caller: string, id: bigint, recipient: string): xdr.ScVal[] { return this.params(address(caller), u64(id), address(recipient)); }
}
