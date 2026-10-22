import type { xdr } from "@stellar/stellar-sdk";
import { ReadContractClient, address, i128, u32, u64, value } from "./internal/contract-client.js";

export interface RouteQuote { amount_out: bigint; hops: unknown[] }
export class DexAggregatorClient extends ReadContractClient {
  protected readonly contractKind = "dex_aggregator" as const;
  findBestRoute(tokenIn: string, tokenOut: string, amountIn: bigint, maxHops: number): Promise<RouteQuote> {
    return this.read("find_best_route", address(tokenIn), address(tokenOut), i128(amountIn), u32(maxHops));
  }
  getQuote(tokenIn: string, tokenOut: string, amountIn: bigint, maxHops: number): Promise<RouteQuote> {
    return this.read("get_quote", address(tokenIn), address(tokenOut), i128(amountIn), u32(maxHops));
  }
  swapBestParams(trader: string, tokenIn: string, tokenOut: string, amountIn: bigint, minOut: bigint, deadline: bigint): xdr.ScVal[] {
    return this.params(address(trader), address(tokenIn), address(tokenOut), i128(amountIn), i128(minOut), u64(deadline));
  }
  executeRouteParams(route: RouteQuote, trader: string, amountIn: bigint, minOut: bigint, deadline: bigint): xdr.ScVal[] {
    return this.params(value(route), address(trader), i128(amountIn), i128(minOut), u64(deadline));
  }
}
