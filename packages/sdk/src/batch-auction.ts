import type { xdr } from "@stellar/stellar-sdk";
import { ReadContractClient, address, i128, u64 } from "./internal/contract-client.js";

export class BatchAuctionClient extends ReadContractClient {
  protected readonly contractKind = "batch_auction" as const;
  getPendingOrders(): Promise<unknown[]> { return this.read("get_pending_orders"); }
  getBatchInfo(): Promise<[number, number, bigint, bigint]> { return this.read("get_batch_info"); }
  getClaimable(orderId: bigint): Promise<[string, string, bigint] | null> { return this.read("get_claimable", u64(orderId)); }
  submitOrderParams(trader: string, pool: string, tokenIn: string, tokenOut: string, amountIn: bigint, minOut: bigint, deadline: bigint): xdr.ScVal[] {
    return this.params(address(trader), address(pool), address(tokenIn), address(tokenOut), i128(amountIn), i128(minOut), u64(deadline));
  }
  settleBatchParams(): xdr.ScVal[] { return []; }
  claimRefundParams(trader: string, orderId: bigint): xdr.ScVal[] { return this.params(address(trader), u64(orderId)); }
}
