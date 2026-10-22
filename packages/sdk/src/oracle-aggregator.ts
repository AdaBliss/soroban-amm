import { ReadContractClient, address } from "./internal/contract-client.js";

export interface AggregatedPrice { price: bigint; confidence_bps: number; timestamp: bigint }
export class OracleAggregatorClient extends ReadContractClient {
  protected readonly contractKind = "oracle_aggregator" as const;
  getPrice(a: string, b: string): Promise<AggregatedPrice> { return this.read("get_price", address(a), address(b)); }
  getPriceSafe(a: string, b: string): Promise<AggregatedPrice> { return this.read("get_price_safe", address(a), address(b)); }
  getPriceDetailed(a: string, b: string): Promise<unknown> { return this.read("get_price_detailed", address(a), address(b)); }
  listSources(): Promise<unknown[]> { return this.read("list_sources"); }
}
