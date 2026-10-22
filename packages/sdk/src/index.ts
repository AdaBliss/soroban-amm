/**
 * @soroban-amm/sdk
 *
 * Typed TypeScript SDK for the Soroban AMM protocol contracts.
 *
 * ```ts
 * import { AmmPool, TokenClient, FactoryClient, GovernanceClient, ConcentratedLiquidityClient } from "@soroban-amm/sdk";
 *
 * const pool = new AmmPool({ rpcUrl, networkPassphrase, contractId });
 * const info = await pool.getInfo();
 * const quote = await pool.simulateSwap(tokenIn, amountIn);
 *
 * const factory = new FactoryClient({ rpcUrl, networkPassphrase, contractId: factoryId });
 * const pools = await factory.allPools();
 * ```
 */

export { AmmPool, AmmContractError, decodeError } from "./AmmPool.js";
export { RouterClient } from "./router.js";
export type { SwapExactInInput, SwapExactOutInput } from "./router.js";
export { SIMULATION_SOURCE_ACCOUNT } from "./internal/simulate.js";
export {
  invoke,
  keypairSigner,
  waitForTransaction,
  SimulationError,
  TransactionFailedError,
  TransactionTimeoutError,
} from "./internal/invoke.js";
export type {
  Signer,
  InvokeOptions,
  InvokeRequest,
  InvocationResult,
} from "./internal/invoke.js";
export { TokenClient } from "./token.js";
export { FactoryClient } from "./factory.js";
export type { CreatePoolResult } from "./factory.js";
export { GovernanceClient, GovernanceContractError, decodeGovernanceError, encodeVote, encodeProposalKind, encodeProposalStatus, encodeVoteRecord, decodeProposalKind, PROPOSAL_KIND_VARIANTS, GovernanceErrorNames } from "./governance.js";
export type {
  GovernanceErrorCode,
  GovernanceErrorName,
  GovernanceParams,
  VetoAudit,
  ProposalData,
  Proposal,
  ProposalKind,
  ProposalStatus,
  ProposalStatusPage,
  VoteChoice,
  VoteRecord,
  UpdateProtocolFeeParams,
  UpdateFactoryTreasuryParams,
  UpdateFactoryGlobalFeeParams,
  UpdateClOracleParams,
  UpdateClMaxOracleDeviationParams,
  UpdateClProtocolFeeParams,
  TransferClPoolAdminParams,
  SetClPositionNftParams,
  CreatePolVestingParams,
} from "./governance.js";
export { ConcentratedLiquidityClient } from "./cl.js";
export type { Position, ClPoolState, PositionQuote, PriceImpactEstimate } from "./cl.js";
export { StakingClient } from "./staking.js";
export { IncentiveCampaignsClient } from "./incentives.js";
export type { Campaign, CreateCampaignInput } from "./incentives.js";
export { PolVestingClient } from "./pol-vesting.js";
export { OracleAggregatorClient } from "./oracle-aggregator.js";
export type { AggregatedPrice } from "./oracle-aggregator.js";
export { TwapConsumerClient, TwalConsumerClient } from "./twap.js";
export { DexAggregatorClient } from "./dex-aggregator.js";
export type { RouteQuote } from "./dex-aggregator.js";
export { BatchAuctionClient } from "./batch-auction.js";
export { ProtocolContractError, decodeProtocolContractError, CONTRACT_METADATA } from "./contract-errors.js";
export type { ContractKind } from "./contract-errors.js";
export * from "./types.js";
