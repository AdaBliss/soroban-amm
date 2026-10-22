/**
 * Shared TypeScript types for the Soroban AMM SDK — Issue #104
 */

/** Network configuration for the SDK. */
export interface NetworkConfig {
  rpcUrl: string;
  networkPassphrase: string;
  contractId: string;
}

/** Full pool state returned by `get_info`. */
export interface PoolInfo {
  tokenA: string;
  tokenB: string;
  reserveA: bigint;
  reserveB: bigint;
  totalShares: bigint;
  feeBps: bigint;
  protocolFeeBps: bigint;
  feeRecipient: string | null;
  flashLoanFeeBps: bigint;
  admin: string | null;
  isPaused: boolean;
  name: string | null;
}

/** Input for a swap call. */
export interface SwapParams {
  /** Caller / trader address (public key). */
  trader: string;
  /** Address of the token being sent in. */
  tokenIn: string;
  /** Amount to send in (in token's smallest unit). */
  amountIn: bigint;
  /** Minimum amount of the output token to accept (slippage guard). */
  minAmountOut: bigint;
  /** Unix timestamp deadline — transaction must execute before this. */
  deadline: bigint;
}

/**
 * Result of a swap simulation, decoded from the contract's `SwapSimulation`
 * struct (`contracts/amm/src/lib.rs`). `priceImpactBps` is surfaced as a number
 * because it is a small basis-point figure; every other field stays a bigint.
 */
export interface SwapSimulation {
  amountIn: bigint;
  amountOut: bigint;
  priceImpactBps: number;
  feeAmount: bigint;
  /** `amount_out / amount_in`, scaled by 1_000_000. */
  effectivePrice: bigint;
  /** `reserve_out / reserve_in`, scaled by 1_000_000. */
  spotPrice: bigint;
}

/** Input for adding liquidity. */
export interface AddLiquidityParams {
  provider: string;
  amountA: bigint;
  amountB: bigint;
  minShares: bigint;
  deadline: bigint;
}

/** Input for removing liquidity. */
export interface RemoveLiquidityParams {
  provider: string;
  shares: bigint;
  minAmountA: bigint;
  minAmountB: bigint;
  deadline: bigint;
}

/** Result of liquidity operations. */
export interface LiquidityResult {
  amountA: bigint;
  amountB: bigint;
  shares: bigint;
}

/** Flash loan parameters. */
export interface FlashLoanParams {
  receiver: string;
  tokenA: bigint;
  tokenB: bigint;
}

/**
 * Every variant of `AmmError` in contracts/amm/src/lib.rs, keyed by the numeric
 * discriminant the contract assigns it — Issue #831.
 *
 * Soroban RPC reports contract-returned errors in the numeric-coded form
 * `Error(Contract, #6)`, never as descriptive English text, so the discriminant
 * is the only reliable join key between an RPC error and a friendly message.
 *
 * Keep in sync with `AmmError` — a variant added there must be added here.
 */
export const AmmErrors = {
  1: "already initialized",
  2: "invalid fee bps",
  3: "insufficient shares",
  4: "deadline exceeded",
  5: "slippage exceeded",
  6: "contract is paused",
  7: "unauthorized",
  8: "zero amount",
  9: "invalid token",
  10: "empty pool",
  11: "insufficient liquidity",
  12: "no pending admin",
  13: "wrong admin",
  14: "reentrant call detected",
  15: "circuit breaker tripped",
  16: "fee-on-transfer slippage",
  17: "oracle deviation exceeded",
  18: "flash loan repayment failed",
  19: "multisig proposal already executed",
  20: "multisig proposal expired",
  21: "pool not initialized",
  22: "pool drained by an emergency withdrawal",
  23: "governance quorum not met",
  24: "no pending emergency announcement or multisig proposal",
  25: "multisig quorum not reached",
  26: "invalid multisig quorum",
  27: "emergency delay not elapsed",
  28: "invalid emergency recipient",
  29: "multisig guard is active",
  30: "invalid emergency delay",
} as const;

/** Numeric discriminant of an `AmmError` variant. */
export type AmmErrorCode = keyof typeof AmmErrors;

/**
 * Symbolic names for each `AmmError` discriminant, mirroring the Rust variant
 * identifiers so callers can branch on a stable name rather than a magic number.
 */
export const AmmErrorNames = {
  1: "AlreadyInitialized",
  2: "InvalidFeeBps",
  3: "InsufficientShares",
  4: "DeadlineExceeded",
  5: "SlippageExceeded",
  6: "Paused",
  7: "Unauthorized",
  8: "ZeroAmount",
  9: "InvalidToken",
  10: "EmptyPool",
  11: "InsufficientLiquidity",
  12: "NoPendingAdmin",
  13: "WrongAdmin",
  14: "Reentrant",
  15: "CircuitBreaker",
  16: "FotSlippage",
  17: "OracleDeviationExceeded",
  18: "FlashLoanRepaymentFailed",
  19: "AlreadyExecuted",
  20: "ProposalExpired",
  21: "NotInitialized",
  22: "PoolDrained",
  23: "QuorumNotMet",
  24: "NoPendingProposal",
  25: "QuorumNotReached",
  26: "InvalidQuorum",
  27: "EmergencyDelayNotElapsed",
  28: "InvalidRecipient",
  29: "MultisigGuardActive",
  30: "InvalidEmergencyDelay",
} as const;

export type AmmErrorKey = (typeof AmmErrorNames)[AmmErrorCode];
