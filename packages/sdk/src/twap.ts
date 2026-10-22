import type { xdr } from "@stellar/stellar-sdk";
import { ReadContractClient, address, u32, u64 } from "./internal/contract-client.js";

export class TwapConsumerClient extends ReadContractClient {
  protected readonly contractKind = "twap_consumer" as const;
  getTwapPrice(pool: string, window: bigint): Promise<bigint> { return this.read("get_twap_price", address(pool), u64(window)); }
  getTwapPriceDetailed(pool: string, window: bigint): Promise<Record<string, unknown>> { return this.read("get_twap_price_detailed", address(pool), u64(window)); }
  getClTwap(pool: string, window: bigint): Promise<bigint> { return this.read("get_cl_twap", address(pool), u64(window)); }
  saveSnapshotParams(pool: string): xdr.ScVal[] { return this.params(address(pool)); }
}

export class TwalConsumerClient extends ReadContractClient {
  protected readonly contractKind = "twal_consumer" as const;
  getTwalLiquidity(pool: string, window: bigint): Promise<bigint> { return this.read("get_twal_liquidity", address(pool), u64(window)); }
  getTrackedPoolsPaginated(offset = 0, limit = 50): Promise<string[]> { return this.read("get_tracked_pools_paginated", u32(offset), u32(limit)); }
  saveSnapshotParams(pool: string): xdr.ScVal[] { return this.params(address(pool)); }
}
