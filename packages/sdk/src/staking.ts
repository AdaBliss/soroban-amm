import type { xdr } from "@stellar/stellar-sdk";
import { ReadContractClient, address, i128, u32, u64 } from "./internal/contract-client.js";

export class StakingClient extends ReadContractClient {
  protected readonly contractKind = "staking" as const;
  getPoolInfo(): Promise<Record<string, unknown>> { return this.read("get_pool_info"); }
  getStakerInfo(staker: string): Promise<Record<string, unknown>> { return this.read("get_staker_info", address(staker)); }
  pendingRewards(staker: string): Promise<bigint> { return this.read("pending_rewards", address(staker)); }
  currentBoost(staker: string): Promise<bigint> { return this.read("current_boost", address(staker)); }
  getLockedPosition(staker: string): Promise<Record<string, unknown>> { return this.read("get_locked_position", address(staker)); }
  listExpiredBoosts(offset = 0, limit = 50): Promise<string[]> { return this.read("list_expired_boosts", u32(offset), u32(limit)); }
  isPaused(): Promise<boolean> { return this.read("is_paused"); }
  isEmergencyMode(): Promise<boolean> { return this.read("is_emergency_mode"); }
  getOwedRewards(): Promise<bigint> { return this.read("get_owed_rewards"); }
  boostMultiplierHuman(raw: bigint): number { return Number(raw) / 1_000_000; }
  stakeParams(staker: string, amount: bigint): xdr.ScVal[] { return this.params(address(staker), i128(amount)); }
  stakeLockedParams(staker: string, amount: bigint, seconds: bigint): xdr.ScVal[] { return this.params(address(staker), i128(amount), u64(seconds)); }
  unstakeParams(staker: string, amount: bigint): xdr.ScVal[] { return this.params(address(staker), i128(amount)); }
  claimParams(staker: string): xdr.ScVal[] { return this.params(address(staker)); }
  extendLockParams(staker: string, seconds: bigint): xdr.ScVal[] { return this.params(address(staker), u64(seconds)); }
  emergencyWithdrawParams(staker: string): xdr.ScVal[] { return this.params(address(staker)); }
  settleBoostParams(staker: string): xdr.ScVal[] { return this.params(address(staker)); }
}
