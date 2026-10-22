import type { xdr } from "@stellar/stellar-sdk";
import { ReadContractClient, address, u32 } from "./internal/contract-client.js";

export class PolVestingClient extends ReadContractClient {
  protected readonly contractKind = "pol_vesting" as const;
  getVesting(beneficiary: string, scheduleId: number): Promise<Record<string, unknown>> { return this.read("get_vesting", address(beneficiary), u32(scheduleId)); }
  releasableAmount(beneficiary: string, scheduleId: number): Promise<bigint> { return this.read("releasable_amount", address(beneficiary), u32(scheduleId)); }
  listSchedules(beneficiary: string, offset = 0, limit = 50): Promise<unknown[]> { return this.read("list_schedules", address(beneficiary), u32(offset), u32(limit)); }
  committed(lpToken: string): Promise<bigint> { return this.read("committed", address(lpToken)); }
  releaseParams(beneficiary: string, scheduleId: number): xdr.ScVal[] { return this.params(address(beneficiary), u32(scheduleId)); }
}
