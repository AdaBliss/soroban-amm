//! Shared cross-contract call interfaces for `amm`, `factory`, and
//! `concentrated_liquidity`.
//!
//! `router`, `dex-aggregator`, `batch_router`, and `batch_auction` each need
//! to call into more than one of those pools/registries, but must not depend
//! on their crates directly: every `#[contractimpl]` fn in a dependency is
//! exported as a wasm symbol, so linking two contract crates' functions into
//! one wasm module collides on every shared entry-point name (e.g.
//! `accept_admin`, `is_paused`). This crate has no `#[contract]` /
//! `#[contractimpl]` block and produces no wasm exports of its own — it only
//! declares the subset of each pool's interface that callers actually invoke,
//! via `#[contractclient]`, plus the plain data types those calls exchange.
//!
//! Soroban resolves cross-contract errors and struct fields by their
//! `#[contracterror]`/`#[contracttype]` discriminant and field names encoded
//! on the wire, not by Rust type identity, so these declarations only need to
//! stay in sync with the real contracts' public signatures — they do not need
//! to be the same Rust type.
#![no_std]

use soroban_sdk::{contractclient, contracterror, contracttype, Address, Env};

// ── amm ──────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum AmmError {
    AlreadyInitialized = 1,
    InvalidFeeBps = 2,
    InsufficientShares = 3,
    DeadlineExceeded = 4,
    SlippageExceeded = 5,
    Paused = 6,
    Unauthorized = 7,
    ZeroAmount = 8,
    InvalidToken = 9,
    EmptyPool = 10,
    InsufficientLiquidity = 11,
    NoPendingAdmin = 12,
    WrongAdmin = 13,
    Reentrant = 14,
    CircuitBreaker = 15,
    FotSlippage = 16,
    OracleDeviationExceeded = 17,
    FlashLoanRepaymentFailed = 18,
    AlreadyExecuted = 19,
    ProposalExpired = 20,
    NotInitialized = 21,
}

#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct PoolInfo {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
    pub flash_loan_fee_bps: i128,
    pub admin: Address,
    pub fee_recipient: Address,
    pub protocol_fee_bps: i128,
    pub lp_rebate_bps: i128,
}

/// The subset of `amm::AmmPool`'s interface called by `router`,
/// `dex-aggregator`, `batch_router`, and `batch_auction`.
#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn add_liquidity(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, AmmError>;

    fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AmmError>;

    fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), AmmError>;

    fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> Result<i128, AmmError>;

    fn get_amount_in(env: Env, token_out: Address, amount_out: i128) -> Result<i128, AmmError>;

    fn get_info(env: Env) -> PoolInfo;
}

// ── factory ──────────────────────────────────────────────────────────────────

/// The subset of `factory::Factory`'s interface called by `router`,
/// `dex-aggregator`, and `batch_router`.
#[contractclient(name = "FactoryClient")]
pub trait FactoryInterface {
    fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address>;

    fn get_cl_pool(env: Env, token_a: Address, token_b: Address, fee_bps: i128) -> Option<Address>;

    fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)>;

    fn is_cl_pool(env: Env, pool: Address) -> bool;
}

// ── concentrated_liquidity ───────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum ClError {
    AlreadyInitialized = 1,
    TokensMustDiffer = 2,
    InvalidFeeBps = 3,
    TickOutOfRange = 4,
    ZeroAmounts = 5,
    SlippageExceeded = 6,
    ZeroLiquidity = 7,
    InsufficientLiquidity = 8,
    PositionNotFound = 9,
    DeadlineExpired = 10,
    Paused = 11,
    Unauthorized = 12,
    TickNotAligned = 13,
    InvalidTickSpacing = 14,
    TickNotInitialized = 15,
    InvalidToken = 16,
    RangeOrderInRange = 17,
    OracleDeviationExceeded = 18,
    NftNotConfigured = 19,
    NotNftOwner = 20,
    NftContractChangeBlocked = 21,
    RangeOrderExists = 22,
    ExactOutNotFullyFilled = 23,
    NotInitialized = 24,
    MathOverflow = 25,
}

#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct PriceImpactEstimate {
    pub amount_in: i128,
    pub amount_in_after_fee: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
    pub spot_price_before: i128,
    pub effective_price: i128,
    pub price_impact_bps: i128,
    pub sqrt_price_before: u128,
    pub sqrt_price_after: u128,
    pub tick_before: i32,
    pub tick_after: i32,
    pub active_liquidity_before: i128,
    pub active_liquidity_after: i128,
}

/// The subset of `concentrated_liquidity::ConcentratedLiquidity`'s interface
/// called by `batch_auction`.
#[contractclient(name = "ConcentratedLiquidityClient")]
pub trait ConcentratedLiquidityInterface {
    #[allow(clippy::too_many_arguments)]
    fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        min_amount_out: i128,
        deadline: u64,
    ) -> Result<i128, ClError>;

    fn get_tokens(env: Env) -> (Address, Address);

    fn fee_bps(env: Env) -> i128;

    fn estimate_price_impact(
        env: Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
    ) -> Result<PriceImpactEstimate, ClError>;
}

// ── factory / cl_pool ────────────────────────────────────────────────────────

/// The on-chain state of a concentrated-liquidity pool, returned by
/// `ClPoolInterface::get_pool_state`. Declared here so `factory` can use it
/// via `pool_interfaces` instead of depending on the `concentrated_liquidity`
/// contract crate directly (which would link the whole CL WASM into factory).
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolState {
    pub sqrt_price: u128,
    pub current_tick: i32,
    pub active_liquidity: i128,
    pub tick_spacing: i32,
}

/// The subset of a concentrated-liquidity pool's interface that `factory`
/// needs for deployment and post-deploy inspection.
#[contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    #[allow(clippy::too_many_arguments)]
    fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
        tick_spacing: i32,
    );

    fn get_pool_state(env: Env) -> PoolState;
}
