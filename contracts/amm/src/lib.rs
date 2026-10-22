//! Constant-product AMM (x * y = k) on Soroban.
//!
//! Flow:
//!   1. Deploy this contract + two asset token contracts.
//!   2. Call `initialize` with both token addresses.
//!   3. First LP calls `add_liquidity` to seed the pool.
//!   4. Traders call `swap` to exchange tokens.
//!   5. LPs call `remove_liquidity` to redeem their share.

#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address,
    Bytes, BytesN, Env, Symbol,
};
// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/amm.wasm"
));

// Standard SEP-41 interface for pool tokens (token_a, token_b)
use soroban_sdk::token::Client as SepTokenClient;

/// Interface for the LP token contract.
///
/// We define this locally rather than importing the `token` crate to avoid
/// duplicate symbol errors during the WASM build.
/// Oracle aggregator price quote (#318).
#[contracttype]
#[derive(Clone, Debug)]
pub struct AggregatedPrice {
    pub price: i128,
    pub confidence: u32,
}

#[contractclient(name = "OracleAggregatorClient")]
pub trait OracleAggregatorInterface {
    /// Returns median price + confidence; confidence 0 when sources are stale.
    fn get_price_safe(env: Env, token_a: Address, token_b: Address) -> AggregatedPrice;
}

#[soroban_sdk::contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn initialize(
        env: Env,
        admin: Address,
        name: soroban_sdk::String,
        symbol: soroban_sdk::String,
        decimals: u32,
    );
    fn mint(env: Env, to: Address, amount: i128);
    fn burn(env: Env, from: Address, amount: i128);
    fn balance(env: Env, id: Address) -> i128;
    fn set_locker(env: Env, locker: Address);
}

/// Lifetime of a multisig emergency-withdraw proposal before it expires.
const MULTISIG_PROPOSAL_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

/// Default delay between announcing an emergency withdrawal (or reaching
/// multisig quorum for one) and being allowed to execute it.
const DEFAULT_EMERGENCY_DELAY_SECS: u64 = 24 * 60 * 60; // 24 hours

/// Upper bound for the configurable emergency delay.
const MAX_EMERGENCY_DELAY_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

/// Persistent-storage TTL bounds (in ledgers) for multisig proposals. A proposal
/// can stay live for `MULTISIG_PROPOSAL_TTL_SECS` plus the emergency delay, so
/// the bump comfortably exceeds both.
const PROPOSAL_TTL_THRESHOLD: u32 = 200_000;
const PROPOSAL_TTL_EXTEND_TO: u32 = 400_000;

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
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
    /// A reentrant call was detected while a flash loan or state-mutating
    /// operation was already in progress. The receiver contract must not
    /// call back into this pool during an `on_flash_loan` callback.
    Reentrant = 14,
    /// The circuit breaker tripped: spot price deviated more than the
    /// configured threshold in a single block.  The pool has been
    /// automatically paused.  Recovery requires the cooldown period to
    /// elapse and a call to `try_circuit_breaker_recovery`, or a direct
    /// admin/governance `unpause`.
    CircuitBreaker = 15,
    /// A fee-on-transfer token deducted more fees than the caller's
    /// `min_received` threshold permitted. The pool received fewer tokens
    /// than requested; the call is reverted to protect the caller.
    FotSlippage = 16,
    /// Spot price deviated beyond the configured oracle tolerance.
    OracleDeviationExceeded = 17,
    /// Flash-loan receiver did not return the borrowed amounts plus fees.
    FlashLoanRepaymentFailed = 18,
    /// Multisig emergency withdrawal was already executed.
    AlreadyExecuted = 19,
    /// Multisig emergency withdrawal proposal has expired.
    ProposalExpired = 20,
    /// A function that reads pool configuration (tokens, LP token, admin,
    /// fee settings) was called before `initialize`.
    NotInitialized = 21,
    /// The pool has undergone an emergency withdrawal and is permanently drained.
    PoolDrained = 22,
    /// Multisig emergency withdrawal proposal has insufficient approvals to meet quorum.
    QuorumNotMet = 23,
    /// No emergency withdrawal announcement or multisig proposal exists to act on.
    NoPendingProposal = 24,
    /// `exec_multisig_emergency_wd` was called while the collected approvals
    /// are below `MultisigQuorum`. Distinct from `QuorumNotMet`, which is the
    /// governance vote quorum error, so tooling can tell the two apart.
    QuorumNotReached = 25,
    /// `set_multisig` / `propose_set_multisig` was given a quorum greater than
    /// the signer count, or a signer set containing duplicates.
    InvalidQuorum = 26,
    /// The emergency delay has not elapsed since the announcement (single
    /// admin path) or since quorum was reached (multisig path).
    EmergencyDelayNotElapsed = 27,
    /// The emergency recipient is the pool itself or its LP token, or does not
    /// match the announced recipient.
    InvalidRecipient = 28,
    /// `set_multisig` was called while a multisig guard is active. An active
    /// guard can only be changed through `propose_set_multisig`.
    MultisigGuardActive = 29,
    /// The configured emergency delay exceeds `MAX_EMERGENCY_DELAY_SECS` (7 days).
    InvalidEmergencyDelay = 30,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    TokenA,
    TokenB,
    LpToken,
    ReserveA,
    ReserveB,
    TotalShares,
    PriceCumulativeA,
    PriceCumulativeB,
    LiquidityCumulative,
    LastTimestamp,
    Shares(Address),
    // Emergency withdrawal storage
    EmergencyWithdrawTimestamp,
    EmergencyWithdrawRecipient,
    /// Terminal drained state after emergency withdrawal.
    Drained,

    // Admin & fees
    Admin,
    PendingAdmin,
    FeeBps,
    FeeRecipient,
    ProtocolFeeBps,
    AccruedFeeA,
    AccruedFeeB,
    FlashLoanFeeBps,

    // Issue #292: LP fee rebate — fraction of protocol fee redistributed to LPs.
    /// Basis points of the protocol fee that are rebated back into LP reserves
    /// (e.g. 5_000 = 50 % of the protocol fee goes back to LPs).
    LpRebateBps,

    // Issue #293: k-of-n multisig guard for emergency operations.
    /// `Vec<Address>` of multisig signers.
    MultisigSigners,
    /// Required quorum (k in k-of-n).
    MultisigQuorum,
    /// Monotonic counter of multisig proposals; the last id handed out.
    MultisigProposalCount,
    /// Multisig proposal by id (persistent storage).
    MultisigProposalById(u64),
    /// Bumped whenever the signer set or quorum changes, so proposals created
    /// under an older configuration can no longer be approved or executed.
    MultisigEpoch,

    // Emergency withdrawal hardening (audit M-04).
    /// Seconds that must pass between an announcement (or multisig quorum) and
    /// the withdrawal itself.
    EmergencyDelaySecs,
    /// The pending single-admin announcement: `EmergencyAnnouncement`.
    EmergencyAnnouncement,

    // Issue #294: minimum liquidity lock — LP tokens permanently locked on first deposit.
    /// Whether the minimum liquidity has already been locked (set on first deposit).
    MinLiquidityLocked,

    // Pause / reentrancy
    Paused,
    /// Set to `true` while a flash loan is executing to block reentrant calls.
    /// Cleared to `false` after the callback returns and repayment is verified.
    Locked,

    // Circuit breaker
    /// Price deviation threshold in bps above which the circuit breaker trips
    /// (default 5 000 = 50 %). Configurable via `set_circuit_breaker_config`.
    CircuitBreakerThresholdBps,

    /// Minimum seconds that must elapse after the circuit breaker trips before
    /// automatic recovery is attempted (default 600 s = 10 min).
    CircuitBreakerCooldown,

    /// Ledger timestamp at which the circuit breaker was last triggered.
    /// `0` when not triggered.
    CircuitBreakerTriggeredAt,

    /// Spot price (reserve_b * 1_000_000 / reserve_a) captured at the
    /// beginning of the current ledger sequence. Used to measure intra-block
    /// price deviation.
    CircuitBreakerLastPrice,

    /// Ledger sequence number at which `CircuitBreakerLastPrice` was captured.
    CircuitBreakerLastSeqno,

    /// Optional oracle aggregator for pre-swap deviation checks (#318).
    OracleAggregator,
    /// Max allowed spot-vs-oracle deviation in basis points (e.g. 500 = 5 %).
    MaxOracleDeviationBps,
}

// ── Pool info returned by `get_info` ─────────────────────────────────────────

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
    /// Issue #292: fraction of protocol fee rebated back to LP reserves (bps).
    pub lp_rebate_bps: i128,
}

/// Issue #293: multisig configuration returned by `get_multisig_config`.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct MultisigConfig {
    pub signers: soroban_sdk::Vec<Address>,
    pub quorum: u32,
}

/// What a multisig proposal does once executed.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub enum MultisigAction {
    /// Drain the pool to the given recipient.
    EmergencyWithdraw(Address),
    /// Replace the signer set and quorum. A quorum of 0 disables the guard.
    SetMultisig(soroban_sdk::Vec<Address>, u32),
}

/// Issue #293: pending multisig proposal, addressed by id.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct MultisigProposal {
    pub action: MultisigAction,
    pub approvals: soroban_sdk::Vec<Address>,
    /// Timestamp after which the proposal can no longer be approved/executed.
    pub expires_at: u64,
    /// Timestamp at which approvals first reached the quorum; 0 until then.
    pub quorum_reached_at: u64,
    /// Signer-set epoch the proposal was created under.
    pub epoch: u32,
    pub executed: bool,
}

/// A pending single-admin emergency withdrawal announcement.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct EmergencyAnnouncement {
    pub to: Address,
    pub announced_at: u64,
}

#[contractclient(name = "FlashLoanReceiverClient")]
pub trait FlashLoanReceiver {
    fn on_flash_loan(
        env: Env,
        token_a_amount: i128,
        token_b_amount: i128,
        fee_a: i128,
        fee_b: i128,
        data: Bytes,
    ) -> bool;
}

// ── Swap simulation returned by `simulate_swap` ───────────────────────────────

#[contracttype]
#[derive(Debug, PartialEq, Clone)]
pub struct SwapSimulation {
    pub amount_out: i128,
    pub fee_amount: i128,
    pub price_impact_bps: i128, // price impact in basis points
    pub effective_price: i128,  // amount_out / amount_in scaled by 1_000_000
    pub spot_price: i128,       // reserve_out / reserve_in scaled by 1_000_000
}

/// Circuit breaker configuration and current state returned by
/// `get_circuit_breaker_config`.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct CircuitBreakerConfig {
    /// Price deviation threshold in bps (e.g. 5 000 = 50 %).
    pub threshold_bps: i128,
    /// Minimum cooldown seconds before automatic recovery.
    pub cooldown_secs: u64,
    /// Timestamp at which the circuit breaker last tripped (0 = never).
    pub triggered_at: u64,
    /// Whether the circuit breaker is currently active (pool paused by CB).
    pub tripped: bool,
}

// ── Reentrancy guard ─────────────────────────────────────────────────────────

/// RAII guard for the pool-wide reentrancy lock (`DataKey::Locked`).
///
/// `ReentrancyGuard::acquire` sets the lock and returns a guard; the lock is
/// released by the guard's `Drop` impl, which runs when the guard goes out
/// of scope — on the `Ok` return path, on every `Err` return path (including
/// every early return via `?`), and on a panic. This is deliberately chosen
/// over hand-placing a release call at each exit: a function with several
/// `?`-chained fallible steps only has to bind `let _guard = ...;` once at
/// the top, and there is no exit path left for a future edit to miss.
///
/// Every fund-moving entry point (`swap`, `swap_exact_out`, `swap_fot`,
/// `add_liquidity`, `add_liquidity_fot`, `remove_liquidity`,
/// `remove_liquidity_one_sided`, `flash_loan`, `withdraw_protocol_fees`,
/// `emergency_withdraw`) acquires one of these before doing anything else
/// that could be observed by a reentrant call, and none of these functions
/// call each other internally, so a guard is never acquired twice in the
/// same call stack during legitimate use.
struct ReentrancyGuard<'a> {
    env: &'a Env,
}

impl<'a> ReentrancyGuard<'a> {
    /// Acquires the lock, or returns `Err(AmmError::Reentrant)` if it is
    /// already held — including when held by an enclosing invocation of this
    /// very function, reached via a cross-contract callback.
    fn acquire(env: &'a Env) -> Result<Self, AmmError> {
        if AmmPool::lock_held(env) {
            return Err(AmmError::Reentrant);
        }
        env.storage().instance().set(&DataKey::Locked, &true);
        Ok(Self { env })
    }
}

impl<'a> Drop for ReentrancyGuard<'a> {
    fn drop(&mut self) {
        self.env.storage().instance().set(&DataKey::Locked, &false);
    }
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct AmmPool;

#[contractimpl]
impl AmmPool {
    // ── Admin / Setup ─────────────────────────────────────────────────────────

    /// Initialize the AMM pool with two tokens, an LP token, and a swap fee.
    ///
    /// Must be called exactly once after deployment. The LP token contract must
    /// already be deployed with this contract set as its admin so it can mint
    /// and burn shares on behalf of liquidity providers.
    ///
    /// # Parameters
    /// - `token_a` – Address of the first pool token (SEP-41 compliant).
    /// - `token_b` – Address of the second pool token (SEP-41 compliant).
    /// - `lp_token` – Address of the LP token contract used to represent pool shares.
    /// - `fee_bps` – Swap fee in basis points (e.g. `30` = 0.30 %). Must be in `[0, 10_000)`.
    ///
    /// `lp_token` must already be deployed and its admin set to this contract.
    /// `admin` is stored as the contract administrator and is the only address
    /// permitted to call `set_protocol_fee` after deployment.
    /// `fee_recipient` receives accrued protocol fees via `withdraw_protocol_fees`.
    /// `protocol_fee_bps` must be strictly below `fee_bps` (or both 0); set to 0 to disable protocol fees.
    /// # Panics
    /// - If the pool has already been initialized.
    /// - If `token_a == token_b`.
    /// - If `fee_bps` is outside the range `[0, 10_000)`.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128, // recommended: 30 (0.30 %)
        fee_recipient: Address,
        protocol_fee_bps: i128,
    ) -> Result<(), AmmError> {
        Self::initialize_with_flash_loan_fee(
            env,
            admin,
            token_a,
            token_b,
            lp_token,
            fee_bps,
            fee_recipient,
            protocol_fee_bps,
            fee_bps,
        )
    }

    /// Initialize the pool with a custom emergency-withdrawal delay.
    ///
    /// Behaves like `initialize`, and additionally sets the number of seconds
    /// that must pass between `announce_emergency_withdraw` (or multisig
    /// quorum) and the withdrawal itself. `emergency_delay_secs` may not exceed
    /// 7 days; `initialize` uses the 24 hour default.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize_with_emergency_delay(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128,
        fee_recipient: Address,
        protocol_fee_bps: i128,
        emergency_delay_secs: u64,
    ) -> Result<(), AmmError> {
        if emergency_delay_secs > MAX_EMERGENCY_DELAY_SECS {
            return Err(AmmError::InvalidEmergencyDelay);
        }
        Self::initialize(
            env.clone(),
            admin,
            token_a,
            token_b,
            lp_token,
            fee_bps,
            fee_recipient,
            protocol_fee_bps,
        )?;
        env.storage()
            .instance()
            .set(&DataKey::EmergencyDelaySecs, &emergency_delay_secs);
        Ok(())
    }

    /// Initialize the pool with a distinct flash-loan fee.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize_with_flash_loan_fee(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128,
        fee_recipient: Address,
        protocol_fee_bps: i128,
        flash_loan_fee_bps: i128,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::TokenA) {
            return Err(AmmError::AlreadyInitialized);
        }
        if token_a == token_b {
            return Err(AmmError::InvalidToken);
        }
        // #1107: one shared check for the swap fee and the protocol cut, so
        // `initialize`, `update_fee` and `set_protocol_fee` cannot drift apart.
        Self::validate_fee_pair(fee_bps, protocol_fee_bps)?;
        Self::validate_flash_loan_fee_bps(flash_loan_fee_bps)?;

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &fee_recipient);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &protocol_fee_bps);
        env.storage().instance().set(&DataKey::AccruedFeeA, &0_i128);
        env.storage().instance().set(&DataKey::AccruedFeeB, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::FlashLoanFeeBps, &flash_loan_fee_bps);
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        env.storage().instance().set(&DataKey::TotalShares, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::PriceCumulativeA, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::PriceCumulativeB, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::LiquidityCumulative, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::LastTimestamp, &env.ledger().timestamp());
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::Locked, &false);
        // Issue #292: LP rebate disabled by default.
        env.storage().instance().set(&DataKey::LpRebateBps, &0_i128);
        // Issue #293: multisig disabled by default (empty signers, quorum 0).
        env.storage()
            .instance()
            .set(&DataKey::MultisigQuorum, &0_u32);
        env.storage().instance().set(
            &DataKey::MultisigSigners,
            &soroban_sdk::Vec::<Address>::new(&env),
        );
        env.storage()
            .instance()
            .set(&DataKey::MultisigEpoch, &0_u32);
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalCount, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::EmergencyDelaySecs, &DEFAULT_EMERGENCY_DELAY_SECS);
        // Issue #294: minimum liquidity not yet locked.
        env.storage()
            .instance()
            .set(&DataKey::MinLiquidityLocked, &false);
        // Circuit breaker: default threshold 50 % (5 000 bps), cooldown 600 s.
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerThresholdBps, &5_000_i128);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerCooldown, &600_u64);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerLastPrice, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerLastSeqno, &0_u32);
        // Oracle deviation guard (#318): disabled until admin configures an oracle.
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &Option::<Address>::None);
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &500_i128);
        Ok(())
    }

    /// Admin: attach or remove the oracle aggregator used for swap deviation checks.
    pub fn set_oracle(env: Env, admin: Address, oracle: Option<Address>) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = Self::read_admin(&env)?;
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &oracle);
        Ok(())
    }

    /// Admin: max spot-vs-oracle deviation in basis points before swaps revert.
    pub fn set_max_oracle_deviation_bps(
        env: Env,
        admin: Address,
        max_deviation_bps: i128,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = Self::read_admin(&env)?;
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        if !(0..=10_000).contains(&max_deviation_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &max_deviation_bps);
        Ok(())
    }

    pub fn pause(env: Env) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);
        Ok(())
    }

    /// Admin-only: make `locker` the LP token's locker (issue #986).
    ///
    /// The LP token's admin is this pool, so `LpToken::set_locker` can only be
    /// authorised by the pool itself; no external account can call it. This
    /// entrypoint is how the pool admin delegates that authority, most often
    /// to point the locker at the governance contract so `governance::vote`
    /// can lock voting LP tokens. When governance is itself the pool admin
    /// (pools created by the factory with governance), governance invokes
    /// this through `claim_lp_locker`.
    pub fn set_lp_locker(env: Env, locker: Address) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();
        let lp_token = Self::read_lp_token(&env)?;
        LpTokenClient::new(&env, &lp_token).set_locker(&locker);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "lp_locker_set"),),
            (lp_token, locker)
        );
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Return `true` if the pool has undergone an emergency withdrawal.
    pub fn is_drained(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Drained)
            .unwrap_or(false)
    }

    /// Announce an emergency withdrawal of all pool reserves to `to`.
    ///
    /// Admin-only. Starts the emergency delay clock; `emergency_withdraw` only
    /// succeeds for the same recipient once `EmergencyDelaySecs` have passed and
    /// while the pool is paused. Announcing again replaces the previous
    /// announcement and restarts the delay. `to` may not be the pool itself or
    /// its LP token.
    pub fn announce_emergency_withdraw(
        env: Env,
        admin: Address,
        to: Address,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        Self::validate_emergency_recipient(&env, &to)?;
        let announced_at = env.ledger().timestamp();
        env.storage().instance().set(
            &DataKey::EmergencyAnnouncement,
            &EmergencyAnnouncement {
                to: to.clone(),
                announced_at,
            },
        );
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "emergency_announce"),),
            (admin, to, announced_at, Self::emergency_delay(&env))
        );
        Ok(())
    }

    /// Cancel a pending emergency withdrawal announcement. Admin-only.
    pub fn cancel_emergency_withdraw(env: Env, admin: Address) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        let announcement: EmergencyAnnouncement = env
            .storage()
            .instance()
            .get(&DataKey::EmergencyAnnouncement)
            .ok_or(AmmError::NoPendingProposal)?;
        env.storage()
            .instance()
            .remove(&DataKey::EmergencyAnnouncement);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "emergency_cancel"),),
            (admin, announcement.to)
        );
        Ok(())
    }

    /// Return the pending single-admin emergency announcement, if any.
    pub fn get_emergency_announcement(env: Env) -> Option<EmergencyAnnouncement> {
        env.storage()
            .instance()
            .get(&DataKey::EmergencyAnnouncement)
    }

    /// Return the configured emergency delay in seconds.
    pub fn get_emergency_delay(env: Env) -> u64 {
        Self::emergency_delay(&env)
    }

    /// Emergency withdraw of all pool reserves and accrued protocol fees to a
    /// designated address.
    ///
    /// Admin-only, and only on the single-admin path: it is rejected while a
    /// multisig guard is active. The pool must be paused, `to` must match a
    /// prior `announce_emergency_withdraw`, and `EmergencyDelaySecs` must have
    /// elapsed since that announcement. A governance `EmergencyWithdraw`
    /// proposal executes through this same call, so it is subject to the same
    /// rules.
    pub fn emergency_withdraw(env: Env, admin: Address, to: Address) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        // Acquire the reentrancy lock before any external transfer below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        Self::require_admin(&env, &admin)?;

        // If a k-of-n multisig guard is configured, the single-admin path must
        // not be allowed to drain reserves. Callers must use the multisig flow
        // (propose_emergency_withdraw / exec_multisig_emergency_wd) instead.
        if Self::multisig_quorum(&env) > 0 {
            return Err(AmmError::Unauthorized);
        }

        let announcement: EmergencyAnnouncement = env
            .storage()
            .instance()
            .get(&DataKey::EmergencyAnnouncement)
            .ok_or(AmmError::NoPendingProposal)?;
        if announcement.to != to {
            return Err(AmmError::InvalidRecipient);
        }
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if !Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        Self::validate_emergency_recipient(&env, &to)?;
        let ready_at = announcement
            .announced_at
            .saturating_add(Self::emergency_delay(&env));
        if env.ledger().timestamp() < ready_at {
            return Err(AmmError::EmergencyDelayNotElapsed);
        }
        env.storage()
            .instance()
            .remove(&DataKey::EmergencyAnnouncement);

        let (drain_a, drain_b) = Self::drain_to(&env, &to)?;

        // Emit event for audit trail
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "emergency_wd"),),
            (admin, to, drain_a, drain_b)
        );

        Ok(())
    }

    /// Return `true` while the reentrancy lock is held.
    ///
    /// The lock is held for the duration of every fund-moving entry point —
    /// `swap`, `swap_exact_out`, `swap_fot`, `add_liquidity`,
    /// `add_liquidity_fot`, `remove_liquidity`, `remove_liquidity_one_sided`,
    /// `flash_loan`, `withdraw_protocol_fees`, and `emergency_withdraw` —
    /// not only `flash_loan`. Any of them will reject a reentrant call with
    /// `AmmError::Reentrant` while this is `true`. This is a read-only
    /// diagnostic; the guard itself is enforced internally by
    /// `ReentrancyGuard::acquire`.
    pub fn is_locked(env: Env) -> bool {
        Self::lock_held(&env)
    }

    /// Deprecated alias for `is_locked`, kept for ABI compatibility.
    /// The name described only the flash-loan holder of the lock; every
    /// fund-moving entry point now holds it while executing.
    pub fn flash_loan_locked(env: Env) -> bool {
        Self::is_locked(env)
    }

    /// Admin-only, multisig-gated last resort to clear a stranded lock.
    ///
    /// The lock is released automatically by `ReentrancyGuard`'s `Drop` on
    /// every return path of every guarded function, so this should never be
    /// needed in practice. It exists purely as recovery if some future
    /// change reintroduces a stranded lock. When a multisig is configured
    /// (`quorum > 0`), at least `quorum` of the configured signers passed in
    /// `callers` must each authorize this call; otherwise the pool admin
    /// alone must authorize it.
    pub fn force_unlock(env: Env, callers: soroban_sdk::Vec<Address>) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let quorum: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MultisigQuorum)
            .unwrap_or(0);
        if quorum == 0 {
            let admin: Address = Self::read_admin(&env)?;
            admin.require_auth();
        } else {
            let signers: soroban_sdk::Vec<Address> = env
                .storage()
                .instance()
                .get(&DataKey::MultisigSigners)
                .unwrap_or_else(|| soroban_sdk::Vec::new(&env));
            let mut approved: u32 = 0;
            for caller in callers.iter() {
                if signers.contains(&caller) {
                    caller.require_auth();
                    approved += 1;
                }
            }
            if approved < quorum {
                return Err(AmmError::Unauthorized);
            }
        }
        env.storage().instance().set(&DataKey::Locked, &false);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "force_unlock"),),
            (env.ledger().timestamp(),)
        );
        Ok(())
    }

    // ── Circuit breaker ───────────────────────────────────────────────────────

    /// Configure the circuit breaker.
    ///
    /// # Parameters
    /// - `threshold_bps` – Maximum allowed intra-block spot-price deviation in
    ///   basis points before the pool is auto-paused (e.g. `5_000` = 50 %).
    ///   Must be in `(0, 10_000]`.
    /// - `cooldown_secs` – Minimum seconds that must pass after tripping before
    ///   automatic recovery via `try_circuit_breaker_recovery` is allowed.
    ///
    /// Admin-only.
    pub fn set_circuit_breaker_config(
        env: Env,
        threshold_bps: i128,
        cooldown_secs: u64,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();

        if threshold_bps <= 0 || threshold_bps > 10_000 {
            return Err(AmmError::InvalidFeeBps);
        }

        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerThresholdBps, &threshold_bps);

        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerCooldown, &cooldown_secs);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "cb_config"),),
            (threshold_bps, cooldown_secs)
        );

        Ok(())
    }

    /// Return the current circuit breaker configuration and state.
    pub fn get_circuit_breaker_config(env: Env) -> CircuitBreakerConfig {
        let threshold_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerThresholdBps)
            .unwrap_or(5_000);

        let cooldown_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerCooldown)
            .unwrap_or(600);

        let triggered_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerTriggeredAt)
            .unwrap_or(0);

        let tripped = triggered_at > 0 && Self::is_paused(env.clone());

        CircuitBreakerConfig {
            threshold_bps,
            cooldown_secs,
            triggered_at,
            tripped,
        }
    }

    /// Attempt automatic recovery after the circuit breaker cooldown.
    pub fn try_circuit_breaker_recovery(env: Env) -> Result<bool, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        let triggered_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerTriggeredAt)
            .unwrap_or(0);

        if triggered_at == 0 {
            return Ok(false);
        }

        if !Self::is_paused(env.clone()) {
            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);
            return Ok(false);
        }

        let cooldown: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerCooldown)
            .unwrap_or(600);

        let now = env.ledger().timestamp();

        if now < triggered_at + cooldown {
            return Ok(false);
        }

        env.storage().instance().set(&DataKey::Paused, &false);

        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "cb_recovered"),), (now,));

        Ok(true)
    }

    /// Keeper entrypoint to trip the circuit breaker if the current spot price
    /// has deviated from the block baseline by at least threshold_bps.
    pub fn trip_circuit_breaker(env: Env) -> Result<bool, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Ok(false);
        }
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        if reserve_a <= 0 || reserve_b <= 0 {
            return Ok(false);
        }
        Self::check_circuit_breaker(&env, reserve_a, reserve_b)
    }

    /// Internal: check whether the trade moving reserves to (post_reserve_a, post_reserve_b)
    /// trips the circuit breaker against the intra-block baseline price.
    ///
    /// If tripped, persists the pause and triggered_at timestamp, emits the circuit_break event,
    /// and returns Ok(true) so callers can trip-and-succeed (return Ok(0)) without executing the trade.
    /// Returns Ok(false) if the price movement is within the threshold.
    fn check_circuit_breaker(
        env: &Env,
        post_reserve_a: i128,
        post_reserve_b: i128,
    ) -> Result<bool, AmmError> {
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());

        if reserve_a <= 0 || reserve_b <= 0 || post_reserve_a <= 0 || post_reserve_b <= 0 {
            return Ok(false);
        }

        let threshold_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerThresholdBps)
            .unwrap_or(5_000);

        let current_seqno = env.ledger().sequence();

        let last_seqno: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerLastSeqno)
            .unwrap_or(0);

        // High precision price: (reserve_b << 64) / reserve_a
        let current_price = ((reserve_b as u128) << 64) / (reserve_a as u128);

        let baseline_price: u128 = if last_seqno != current_seqno {
            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerLastPrice, &(current_price as i128));

            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerLastSeqno, &current_seqno);

            current_price
        } else {
            let stored: i128 = env
                .storage()
                .instance()
                .get(&DataKey::CircuitBreakerLastPrice)
                .unwrap_or(0);
            if stored > 0 {
                stored as u128
            } else {
                0
            }
        };

        if baseline_price == 0 {
            return Ok(false);
        }

        let post_price = ((post_reserve_b as u128) << 64) / (post_reserve_a as u128);

        let deviation_bps = if post_price >= baseline_price {
            ((post_price - baseline_price) * 10_000 / baseline_price) as i128
        } else {
            ((baseline_price - post_price) * 10_000 / baseline_price) as i128
        };

        if deviation_bps >= threshold_bps {
            let now = env.ledger().timestamp();

            env.storage().instance().set(&DataKey::Paused, &true);

            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerTriggeredAt, &now);

            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(env, "circuit_break"),),
                (
                    baseline_price as i128,
                    post_price as i128,
                    deviation_bps,
                    threshold_bps
                )
            );

            return Ok(true);
        }

        Ok(false)
    }

    /// Update the protocol fee configuration. Admin-only.
    ///
    /// Set `protocol_fee_bps` to 0 to disable protocol fee collection.
    /// `protocol_fee_bps` must be strictly below the pool's `fee_bps` (or both
    /// must be 0); see `validate_fee_pair`.
    pub fn set_protocol_fee(
        env: Env,
        admin: Address,
        recipient: Address,
        protocol_fee_bps: i128,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = Self::read_admin(&env)?;
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        let fee_bps: i128 = Self::read_fee_bps(&env)?;
        // Fix M-02: protocol_fee_bps must be *strictly* less than fee_bps so LPs
        // always retain a portion of swap income (see `validate_fee_pair`).
        Self::validate_fee_pair(fee_bps, protocol_fee_bps)?;
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &recipient);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &protocol_fee_bps);
        // Emit an event so LPs and indexers can monitor protocol-fee changes.
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "protocol_fee_set"),),
            (protocol_fee_bps, recipient)
        );
        Ok(())
    }

    /// Return the current protocol fee recipient and rate.
    ///
    /// Returns `(None, 0)` when protocol fees are disabled.
    pub fn get_protocol_fee(env: Env) -> (Option<Address>, i128) {
        let recipient: Option<Address> = env.storage().instance().get(&DataKey::FeeRecipient);
        let bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        (recipient, bps)
    }

    // ── Issue #292: LP fee rebate ─────────────────────────────────────────────

    /// Set the fraction of the protocol fee rebated back into LP reserves.
    ///
    /// `lp_rebate_bps` is a fraction of `protocol_fee_bps`
    /// (e.g. 5_000 = 50 % of the protocol cut goes back to LPs).
    /// Must be in `[0, 10_000]`. Admin-only.
    pub fn set_lp_rebate(env: Env, admin: Address, lp_rebate_bps: i128) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = Self::read_admin(&env)?;
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        if !(0..=10_000).contains(&lp_rebate_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::LpRebateBps, &lp_rebate_bps);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "lp_rebate_set"), admin),
            (lp_rebate_bps,)
        );
        Ok(())
    }

    /// Return the current LP rebate rate in basis points.
    pub fn get_lp_rebate(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0)
    }

    // ── Issue #293: k-of-n multisig emergency guard ───────────────────────────

    /// Install the initial k-of-n multisig guard for emergency operations.
    ///
    /// Admin-only, and only while no guard is active (`quorum == 0`). Once a
    /// guard exists, the admin it constrains can no longer change or disable it
    /// alone: use `propose_set_multisig` / `approve_multisig_proposal` /
    /// `exec_multisig_set_multisig` instead. `quorum` must be in
    /// `0..=signers.len()` and `signers` must be unique; a quorum of 0 stores an
    /// empty signer set (single-admin mode).
    pub fn set_multisig(
        env: Env,
        admin: Address,
        signers: soroban_sdk::Vec<Address>,
        quorum: u32,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        if Self::multisig_quorum(&env) > 0 {
            return Err(AmmError::MultisigGuardActive);
        }
        Self::apply_multisig_config(&env, signers, quorum)?;
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "multisig_set"), admin),
            (quorum,)
        );
        Ok(())
    }

    /// Return the current multisig configuration.
    pub fn get_multisig_config(env: Env) -> MultisigConfig {
        MultisigConfig {
            signers: Self::multisig_signers(&env),
            quorum: Self::multisig_quorum(&env),
        }
    }

    /// Return the current multisig signers and quorum.
    pub fn get_multisig(env: Env) -> (soroban_sdk::Vec<Address>, u32) {
        (Self::multisig_signers(&env), Self::multisig_quorum(&env))
    }

    /// Propose an emergency withdrawal to `recipient` (multisig mode).
    ///
    /// Any configured signer may call this; the proposer's approval is recorded
    /// automatically. Other signers add theirs with `approve_multisig_proposal`.
    /// Once the quorum is reached and `EmergencyDelaySecs` have passed the
    /// proposal can be executed via `exec_multisig_emergency_wd`. Returns the
    /// proposal id.
    pub fn propose_emergency_withdraw(
        env: Env,
        signer: Address,
        recipient: Address,
    ) -> Result<u64, AmmError> {
        Self::extend_ttl(&env);
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        Self::validate_emergency_recipient(&env, &recipient)?;
        let id = Self::create_multisig_proposal(
            &env,
            &signer,
            MultisigAction::EmergencyWithdraw(recipient.clone()),
        );
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "ms_proposed"), signer),
            (id, recipient)
        );
        Ok(id)
    }

    /// Propose replacing the multisig signer set and quorum. A quorum of 0
    /// disables the guard. This is the only way to change an active guard.
    /// Returns the proposal id.
    pub fn propose_set_multisig(
        env: Env,
        signer: Address,
        new_signers: soroban_sdk::Vec<Address>,
        new_quorum: u32,
    ) -> Result<u64, AmmError> {
        Self::extend_ttl(&env);
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        Self::validate_multisig_config(&new_signers, new_quorum)?;
        let id = Self::create_multisig_proposal(
            &env,
            &signer,
            MultisigAction::SetMultisig(new_signers, new_quorum),
        );
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "ms_set_proposed"), signer),
            (id, new_quorum)
        );
        Ok(id)
    }

    /// Add `signer`'s approval to multisig proposal `proposal_id`.
    ///
    /// Re-approving is a no-op. When the approvals first reach the quorum the
    /// emergency delay clock starts and the proposal's lifetime is extended once
    /// to cover it.
    pub fn approve_multisig_proposal(
        env: Env,
        signer: Address,
        proposal_id: u64,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        let mut proposal = Self::load_live_proposal(&env, proposal_id)?;
        if !proposal.approvals.contains(&signer) {
            proposal.approvals.push_back(signer.clone());
            Self::mark_quorum_if_reached(&env, &mut proposal);
            Self::store_proposal(&env, proposal_id, &proposal);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "ms_approved"), signer),
            (proposal_id, proposal.approvals.len())
        );
        Ok(())
    }

    /// Return a multisig proposal by id, if it exists.
    pub fn get_multisig_proposal(env: Env, proposal_id: u64) -> Option<MultisigProposal> {
        env.storage()
            .persistent()
            .get(&DataKey::MultisigProposalById(proposal_id))
    }

    /// Execute multisig emergency-withdraw proposal `proposal_id`.
    ///
    /// Any signer may call this once the quorum is reached, the emergency delay
    /// has passed since it was reached, and the pool is paused. Sweeps reserves
    /// and accrued protocol fees to the proposal's recipient.
    pub fn exec_multisig_emergency_wd(
        env: Env,
        signer: Address,
        proposal_id: u64,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let _guard = ReentrancyGuard::acquire(&env)?;
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        let mut proposal = Self::load_live_proposal(&env, proposal_id)?;
        let MultisigAction::EmergencyWithdraw(to) = proposal.action.clone() else {
            return Err(AmmError::NoPendingProposal);
        };
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        Self::require_quorum_and_delay(&env, &proposal)?;
        if !Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        Self::validate_emergency_recipient(&env, &to)?;

        proposal.executed = true;
        Self::store_proposal(&env, proposal_id, &proposal);

        let (drain_a, drain_b) = Self::drain_to(&env, &to)?;
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "ms_ew"), signer),
            (to, drain_a, drain_b)
        );
        Ok(())
    }

    /// Execute multisig proposal `proposal_id` that replaces the signer set and
    /// quorum. Requires the current quorum of approvals; no delay applies.
    pub fn exec_multisig_set_multisig(
        env: Env,
        signer: Address,
        proposal_id: u64,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        let mut proposal = Self::load_live_proposal(&env, proposal_id)?;
        let MultisigAction::SetMultisig(new_signers, new_quorum) = proposal.action.clone() else {
            return Err(AmmError::NoPendingProposal);
        };
        if (proposal.approvals.len()) < Self::multisig_quorum(&env) {
            return Err(AmmError::QuorumNotReached);
        }

        proposal.executed = true;
        Self::store_proposal(&env, proposal_id, &proposal);

        Self::apply_multisig_config(&env, new_signers, new_quorum)?;
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "multisig_set"), signer),
            (new_quorum,)
        );
        Ok(())
    }

    fn check_oracle_deviation(
        env: &Env,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
        amount_out: i128,
    ) -> Result<(), AmmError> {
        let oracle: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::OracleAggregator)
            .unwrap_or(None);
        let Some(oracle_addr) = oracle else {
            return Ok(());
        };
        let max_dev: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxOracleDeviationBps)
            .unwrap_or(500);

        let agg =
            OracleAggregatorClient::new(env, &oracle_addr).get_price_safe(token_in, token_out);
        if amount_in <= 0 {
            return Ok(());
        }
        if agg.confidence == 0 || agg.price <= 0 {
            return Err(AmmError::OracleDeviationExceeded);
        }

        let spot_price = amount_out * 1_000_000 / amount_in;
        let oracle_price = agg.price;
        let deviation_bps = if spot_price >= oracle_price {
            (spot_price - oracle_price) * 10_000 / oracle_price
        } else {
            (oracle_price - spot_price) * 10_000 / oracle_price
        };
        if deviation_bps > max_dev {
            return Err(AmmError::OracleDeviationExceeded);
        }
        Ok(())
    }

    /// Shared by initialize and update_fee.
    /// Swap fee must be strictly in [0, 10_000) (0 to 9_999 bps).
    fn validate_fee_bps(fee_bps: i128) -> Result<(), AmmError> {
        if !(0..10_000).contains(&fee_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        Ok(())
    }

    /// The pool's fee invariant (#1107), shared by `initialize*`, `update_fee`
    /// and `set_protocol_fee`:
    ///
    /// - the swap fee passes `validate_fee_bps`, and
    /// - `0 <= protocol_fee_bps < fee_bps`, or both are 0.
    ///
    /// `protocol_fee_bps == fee_bps` would send every swap fee to the protocol
    /// and leave LPs with nothing (AUDIT M-02 / M-03), and a non-zero protocol
    /// fee on a zero swap fee has nothing to take a cut of.
    fn validate_fee_pair(fee_bps: i128, protocol_fee_bps: i128) -> Result<(), AmmError> {
        Self::validate_fee_bps(fee_bps)?;
        let ok = if fee_bps == 0 {
            protocol_fee_bps == 0
        } else {
            (0..fee_bps).contains(&protocol_fee_bps)
        };
        if ok {
            Ok(())
        } else {
            Err(AmmError::InvalidFeeBps)
        }
    }

    /// Flash loan fee validation: [0, 10_000] bps.
    fn validate_flash_loan_fee_bps(fee_bps: i128) -> Result<(), AmmError> {
        if !(0..=10_000).contains(&fee_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        Ok(())
    }

    fn require_signer(env: &Env, signer: &Address) -> Result<(), AmmError> {
        if Self::multisig_quorum(env) == 0 || !Self::multisig_signers(env).contains(signer) {
            return Err(AmmError::Unauthorized);
        }
        Ok(())
    }

    fn validate_multisig_config(
        signers: &soroban_sdk::Vec<Address>,
        quorum: u32,
    ) -> Result<(), AmmError> {
        if quorum > signers.len() {
            return Err(AmmError::InvalidQuorum);
        }
        // A duplicate would make the signer count overstate how many distinct
        // approvals are possible and could leave the quorum unreachable.
        for i in 0..signers.len() {
            for j in (i + 1)..signers.len() {
                if signers.get(i) == signers.get(j) {
                    return Err(AmmError::InvalidQuorum);
                }
            }
        }
        Ok(())
    }

    /// Store a validated signer set and quorum, invalidating every proposal made
    /// under the previous configuration.
    fn apply_multisig_config(
        env: &Env,
        signers: soroban_sdk::Vec<Address>,
        quorum: u32,
    ) -> Result<(), AmmError> {
        Self::validate_multisig_config(&signers, quorum)?;
        let signers = if quorum == 0 {
            soroban_sdk::Vec::new(env)
        } else {
            signers
        };
        env.storage()
            .instance()
            .set(&DataKey::MultisigSigners, &signers);
        env.storage()
            .instance()
            .set(&DataKey::MultisigQuorum, &quorum);
        env.storage()
            .instance()
            .set(&DataKey::MultisigEpoch, &(Self::multisig_epoch(env) + 1));
        Ok(())
    }

    fn create_multisig_proposal(env: &Env, proposer: &Address, action: MultisigAction) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MultisigProposalCount)
            .unwrap_or(0)
            + 1;
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalCount, &id);
        let mut approvals = soroban_sdk::Vec::new(env);
        approvals.push_back(proposer.clone());
        let mut proposal = MultisigProposal {
            action,
            approvals,
            expires_at: env
                .ledger()
                .timestamp()
                .saturating_add(MULTISIG_PROPOSAL_TTL_SECS),
            quorum_reached_at: 0,
            epoch: Self::multisig_epoch(env),
            executed: false,
        };
        Self::mark_quorum_if_reached(env, &mut proposal);
        Self::store_proposal(env, id, &proposal);
        id
    }

    /// Start the emergency delay clock the first time approvals reach the
    /// quorum, extending the proposal's lifetime once so the delay cannot make
    /// it expire before it can be executed.
    fn mark_quorum_if_reached(env: &Env, proposal: &mut MultisigProposal) {
        if proposal.quorum_reached_at == 0 && proposal.approvals.len() >= Self::multisig_quorum(env)
        {
            let now = env.ledger().timestamp();
            proposal.quorum_reached_at = now;
            proposal.expires_at = proposal.expires_at.max(
                now.saturating_add(MULTISIG_PROPOSAL_TTL_SECS)
                    .saturating_add(Self::emergency_delay(env)),
            );
        }
    }

    fn store_proposal(env: &Env, id: u64, proposal: &MultisigProposal) {
        let key = DataKey::MultisigProposalById(id);
        env.storage().persistent().set(&key, proposal);
        env.storage()
            .persistent()
            .extend_ttl(&key, PROPOSAL_TTL_THRESHOLD, PROPOSAL_TTL_EXTEND_TO);
    }

    /// Load a proposal that can still be approved or executed.
    fn load_live_proposal(env: &Env, id: u64) -> Result<MultisigProposal, AmmError> {
        let proposal: MultisigProposal = env
            .storage()
            .persistent()
            .get(&DataKey::MultisigProposalById(id))
            .ok_or(AmmError::NoPendingProposal)?;
        if proposal.executed {
            return Err(AmmError::AlreadyExecuted);
        }
        if proposal.epoch != Self::multisig_epoch(env)
            || env.ledger().timestamp() > proposal.expires_at
        {
            return Err(AmmError::ProposalExpired);
        }
        Ok(proposal)
    }

    fn require_quorum_and_delay(env: &Env, proposal: &MultisigProposal) -> Result<(), AmmError> {
        if proposal.approvals.len() < Self::multisig_quorum(env) {
            return Err(AmmError::QuorumNotReached);
        }
        let ready_at = proposal
            .quorum_reached_at
            .saturating_add(Self::emergency_delay(env));
        if env.ledger().timestamp() < ready_at {
            return Err(AmmError::EmergencyDelayNotElapsed);
        }
        Ok(())
    }

    fn require_admin(env: &Env, admin: &Address) -> Result<(), AmmError> {
        let stored_admin: Address = Self::read_admin(env)?;
        if *admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        Ok(())
    }

    fn multisig_quorum(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MultisigQuorum)
            .unwrap_or(0)
    }

    fn multisig_signers(env: &Env) -> soroban_sdk::Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::MultisigSigners)
            .unwrap_or_else(|| soroban_sdk::Vec::new(env))
    }

    fn multisig_epoch(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MultisigEpoch)
            .unwrap_or(0)
    }

    fn emergency_delay(env: &Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::EmergencyDelaySecs)
            .unwrap_or(DEFAULT_EMERGENCY_DELAY_SECS)
    }

    /// The pool and its LP token can never be an emergency recipient: funds
    /// sent there would be unrecoverable or would corrupt share accounting.
    fn validate_emergency_recipient(env: &Env, to: &Address) -> Result<(), AmmError> {
        if *to == env.current_contract_address() || *to == Self::read_lp_token(env)? {
            return Err(AmmError::InvalidRecipient);
        }
        Ok(())
    }

    /// Transfer all reserves and accrued protocol fees to `to` and put the pool
    /// in its terminal drained state. Returns the amounts moved.
    fn drain_to(env: &Env, to: &Address) -> Result<(i128, i128), AmmError> {
        // Record audit information
        let ts: u64 = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::EmergencyWithdrawTimestamp, &ts);
        env.storage()
            .instance()
            .set(&DataKey::EmergencyWithdrawRecipient, to);

        let token_a: Address = Self::read_token_a(env)?;
        let token_b: Address = Self::read_token_b(env)?;
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        let (accrued_a, accrued_b) = Self::get_accrued_fees(env.clone());
        let drain_a = reserve_a.saturating_add(accrued_a);
        let drain_b = reserve_b.saturating_add(accrued_b);

        if drain_a > 0 {
            SepTokenClient::new(env, &token_a).transfer(
                &env.current_contract_address(),
                to,
                &drain_a,
            );
        }
        if drain_b > 0 {
            SepTokenClient::new(env, &token_b).transfer(
                &env.current_contract_address(),
                to,
                &drain_b,
            );
        }

        // Zero out reserves and accrued protocol fees. The pool enters a terminal
        // Drained state (#1095) so existing LP tokens cannot drain subsequent deposits.
        // TotalShares and MinLiquidityLocked are left intact to record historical LP shares.
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        env.storage().instance().set(&DataKey::AccruedFeeA, &0_i128);
        env.storage().instance().set(&DataKey::AccruedFeeB, &0_i128);
        env.storage().instance().set(&DataKey::Drained, &true);
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok((drain_a, drain_b))
    }

    fn extend_ttl(env: &Env) {
        env.storage().instance().extend_ttl(172_800, 518_400);
    }

    fn read_admin(env: &Env) -> Result<Address, AmmError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(AmmError::NotInitialized)
    }

    fn read_token_a(env: &Env) -> Result<Address, AmmError> {
        env.storage()
            .instance()
            .get(&DataKey::TokenA)
            .ok_or(AmmError::NotInitialized)
    }

    fn read_token_b(env: &Env) -> Result<Address, AmmError> {
        env.storage()
            .instance()
            .get(&DataKey::TokenB)
            .ok_or(AmmError::NotInitialized)
    }

    fn read_lp_token(env: &Env) -> Result<Address, AmmError> {
        env.storage()
            .instance()
            .get(&DataKey::LpToken)
            .ok_or(AmmError::NotInitialized)
    }

    fn read_fee_bps(env: &Env) -> Result<i128, AmmError> {
        env.storage()
            .instance()
            .get(&DataKey::FeeBps)
            .ok_or(AmmError::NotInitialized)
    }

    /// Update the swap fee post-deployment. Admin-only.
    ///
    /// The new fee takes effect on the very next swap.
    /// Emits a `fee_upd` event on every successful call.
    ///
    /// # Parameters
    /// - `admin` - must match the stored admin address.
    /// - `new_fee_bps` - new swap fee in basis points; must be in [0, 10_000).
    ///
    /// # Errors
    /// - `InvalidFeeBps` if `new_fee_bps` is outside [0, 10_000).
    /// - `InvalidFeeBps` if the new fee and the current `protocol_fee_bps` would
    ///   break the invariant (`protocol_fee_bps < fee_bps`, or both 0). The
    ///   call is rejected as a whole: the protocol fee is never silently
    ///   clamped, so lowering the fee below the protocol cut means lowering
    ///   `set_protocol_fee` first.
    ///
    /// # Panics
    /// - If `admin` auth fails.
    pub fn update_fee(env: Env, new_fee_bps: i128) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        Self::validate_fee_pair(new_fee_bps, protocol_fee_bps)?;
        env.storage().instance().set(&DataKey::FeeBps, &new_fee_bps);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("fee_upd"), admin.clone()),
            (new_fee_bps,)
        );
        Ok(())
    }

    /// Update the flash loan fee post-deployment. Admin-only.
    pub fn update_flash_loan_fee(env: Env, new_fee_bps: i128) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();
        Self::validate_flash_loan_fee_bps(new_fee_bps)?;
        env.storage()
            .instance()
            .set(&DataKey::FlashLoanFeeBps, &new_fee_bps);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "flash_fee_upd"), admin.clone()),
            (new_fee_bps,)
        );
        Ok(())
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete the transfer.
    ///
    /// # Panics
    /// - If `current_admin` is not the stored admin.
    /// - If `current_admin` auth fails.
    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let stored: Address = Self::read_admin(&env)?;
        if current_admin != stored {
            return Err(AmmError::Unauthorized);
        }
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
        Ok(())
    }

    /// Accept the pending admin nomination. Caller becomes the new admin.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(AmmError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(AmmError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
        Ok(())
    }

    /// Replace the contract WASM with a new version. Admin-only.
    ///
    /// The new WASM must already be uploaded to the network.
    /// State is preserved; only bytecode is replaced.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), AmmError> {
        Self::extend_ttl(&env);
        let admin: Address = Self::read_admin(&env)?;
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "upgraded"),),
            (new_wasm_hash,)
        );
        Ok(())
    }

    /// Return the pending admin nominee, or `None` if no transfer is in progress.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    // ── Reentrancy guard ──────────────────────────────────────────────────────

    /// Return `true` if the reentrancy lock is currently held.
    ///
    /// The lock is stored in instance storage so it is visible to every
    /// cross-contract call within the same transaction, including a
    /// callback made by an external token or flash-loan receiver contract
    /// back into this pool.
    fn lock_held(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false)
    }

    // ── TWAP ──────────────────────────────────────────────────────────────────

    /// Update the TWAP price accumulators based on the current reserves and elapsed time.
    /// This ensures that any reserve-mutating operation (add_liquidity, remove_liquidity,
    /// swap, flash_loan) correctly records the price at the time of the operation,
    /// preventing TWAP manipulation vectors.
    fn checkpoint_twap(env: &Env) -> (i128, i128) {
        let now = env.ledger().timestamp();
        let last: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(now);
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        if now > last {
            if reserve_a > 0 && reserve_b > 0 {
                let elapsed = (now - last) as i128;
                let mut cum_a: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::PriceCumulativeA)
                    .unwrap_or(0);
                let mut cum_b: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::PriceCumulativeB)
                    .unwrap_or(0);
                // Use wrapping_add so overflow is defined and consumers can handle it via
                // unsigned subtraction: (now - then) as u128 gives the correct delta.
                cum_a = cum_a.wrapping_add((reserve_b * 1_000_000 / reserve_a) * elapsed);
                cum_b = cum_b.wrapping_add((reserve_a * 1_000_000 / reserve_b) * elapsed);
                env.storage()
                    .instance()
                    .set(&DataKey::PriceCumulativeA, &cum_a);
                env.storage()
                    .instance()
                    .set(&DataKey::PriceCumulativeB, &cum_b);
            }
            env.storage().instance().set(&DataKey::LastTimestamp, &now);
        }
        (reserve_a, reserve_b)
    }

    /// Update the TWAL liquidity accumulator (sqrt(reserve_a * reserve_b) * elapsed).
    fn checkpoint_twal(env: &Env) {
        let now = env.ledger().timestamp();
        let last: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(now);
        if now > last {
            let reserve_a = Self::get_reserve_a(env.clone());
            let reserve_b = Self::get_reserve_b(env.clone());
            if reserve_a > 0 && reserve_b > 0 {
                let elapsed = (now - last) as i128;
                let liquidity = Self::sqrt(reserve_a * reserve_b);
                let mut cum: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::LiquidityCumulative)
                    .unwrap_or(0);
                cum = cum.wrapping_add(liquidity * elapsed);
                env.storage()
                    .instance()
                    .set(&DataKey::LiquidityCumulative, &cum);
            }
        }
    }

    /// Checkpoint both TWAP price and TWAL liquidity accumulators.
    /// TWAL must run first so it reads the old `LastTimestamp` before TWAP updates it.
    /// Returns the pre-checkpoint reserves (same as calling `checkpoint_twap` directly).
    fn checkpoint_oracles(env: &Env) -> (i128, i128) {
        Self::checkpoint_twal(env);
        Self::checkpoint_twap(env)
    }

    // ── Liquidity ─────────────────────────────────────────────────────────────

    /// Deposit tokens into the pool and receive LP shares in return.
    ///
    /// On the first deposit any ratio is accepted and the initial share supply is
    /// set to the geometric mean of the two amounts. Subsequent deposits must
    /// match the current pool ratio (within integer rounding); excess tokens are
    /// **not** refunded automatically — callers should compute amounts off-chain
    /// before calling.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider funding the deposit.
    /// - `amount_a` – Amount of `token_a` to deposit. Must be positive.
    /// - `amount_b` – Amount of `token_b` to deposit. Must be positive.
    /// - `min_shares` – Minimum number of LP shares the caller is willing to
    ///   receive; the transaction panics if fewer would be minted (slippage guard).
    ///
    /// # Returns
    /// The number of LP shares minted to `provider`.
    ///
    /// # Panics
    /// - If either `amount_a` or `amount_b` is not positive.
    /// - If the shares that would be minted are less than `min_shares`.
    pub fn add_liquidity(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        provider.require_auth();
        if amount_a <= 0 || amount_b <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let lp_token: Address = Self::read_lp_token(&env)?;
        let total_shares: i128 = Self::get_total_shares(env.clone());

        let (shares_to_provider, shares_locked) = Self::compute_liquidity_mint(
            &env,
            amount_a,
            amount_b,
            reserve_a,
            reserve_b,
            total_shares,
            min_shares,
        )?;

        // Pull tokens from provider into the pool contract.
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);
        client_a.transfer(&provider, &env.current_contract_address(), &amount_a);
        client_b.transfer(&provider, &env.current_contract_address(), &amount_b);

        // Update reserves.
        let total_minted = shares_to_provider + shares_locked;
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a + amount_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b + amount_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares + total_minted));

        // Mint LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        // Issue #294: lock minimum liquidity to the contract address itself (zero-address equivalent).
        if shares_locked > 0 {
            lp_client.mint(&env.current_contract_address(), &shares_locked);
            env.storage()
                .instance()
                .set(&DataKey::MinLiquidityLocked, &true);
        }
        lp_client.mint(&provider, &shares_to_provider);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "add_liquidity"), provider),
            (amount_a, amount_b, shares_to_provider)
        );

        Ok(shares_to_provider)
    }

    /// Withdraw liquidity from the pool by burning LP shares.
    ///
    /// Burns exactly `shares` LP tokens held by `provider` and transfers a
    /// proportional amount of both pool tokens back to the provider. The
    /// proportion is `shares / total_shares` at the time of the call.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider redeeming shares.
    /// - `shares` – Number of LP shares to burn. Must be positive and ≤ the
    ///   provider's current balance.
    /// - `min_a` – Minimum amount of `token_a` the caller is willing to receive
    ///   (slippage guard).
    /// - `min_b` – Minimum amount of `token_b` the caller is willing to receive
    ///   (slippage guard).
    ///
    /// # Returns
    /// A tuple `(amount_a, amount_b)` — the token amounts transferred back to
    /// the provider.
    ///
    /// # Panics
    /// - If `shares` is not positive.
    /// - If `provider` owns fewer shares than `shares`.
    /// - If the computed `token_a` output would be less than `min_a`.
    /// - If the computed `token_b` output would be less than `min_b`.
    pub fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        provider.require_auth();
        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

        let owned = Self::shares_of(env.clone(), provider.clone())?;
        if owned < shares {
            return Err(AmmError::InsufficientShares);
        }

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let lp_token: Address = Self::read_lp_token(&env)?;

        let total_shares = Self::get_total_shares(env.clone());

        let out_a = shares * reserve_a / total_shares;
        let out_b = shares * reserve_b / total_shares;

        if out_a < min_a || out_b < min_b {
            return Err(AmmError::SlippageExceeded);
        }

        // Burn LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.burn(&provider, &shares);

        // Update state.
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a - out_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b - out_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));

        // Return tokens.
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);
        client_a.transfer(&env.current_contract_address(), &provider, &out_a);
        client_b.transfer(&env.current_contract_address(), &provider, &out_b);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("rm_liq"),),
            (provider.clone(), shares, out_a, out_b)
        );

        Ok((out_a, out_b))
    }

    /// Burn LP shares and return a single token, swapping the other internally.
    ///
    /// Equivalent to calling `remove_liquidity` followed by `swap`, but in a single
    /// transaction. This saves users a swap fee and simplifies the UX when they want
    /// to exit a position receiving only one asset.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider redeeming shares.
    /// - `shares` – Number of LP shares to burn. Must be positive and ≤ the
    ///   provider's current balance.
    /// - `token_out` – Address of the token to receive; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `min_out` – Minimum total amount of `token_out` the caller is willing to
    ///   receive after the internal swap (slippage guard).
    ///
    /// # Returns
    /// The total amount of `token_out` received (withdrawal + internal swap proceeds).
    ///
    /// # Panics
    /// - If `shares` is not positive.
    /// - If `provider` owns fewer shares than `shares`.
    /// - If `token_out` is not one of the two pool tokens.
    /// - If the computed output would be less than `min_out`.
    /// - If the pool is paused.
    #[allow(clippy::too_many_arguments)]
    pub fn remove_liquidity_one_sided(
        env: Env,
        provider: Address,
        shares: i128,
        token_out: Address,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        provider.require_auth();
        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Checkpoint TWAP before updating reserves.
        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

        let owned = Self::shares_of(env.clone(), provider.clone())?;
        if owned < shares {
            return Err(AmmError::InsufficientShares);
        }

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let lp_token: Address = Self::read_lp_token(&env)?;

        if token_out != token_a && token_out != token_b {
            return Err(AmmError::InvalidToken);
        }

        let total_shares = Self::get_total_shares(env.clone());

        // Compute proportional withdrawal amounts.
        let withdraw_a = shares * reserve_a / total_shares;
        let withdraw_b = shares * reserve_b / total_shares;

        // Determine which token we keep and which we swap away.
        let (_token_keep, token_swap, amount_keep, amount_swap) = if token_out == token_a {
            (token_a.clone(), token_b.clone(), withdraw_a, withdraw_b)
        } else {
            (token_b.clone(), token_a.clone(), withdraw_b, withdraw_a)
        };

        // Update reserves: deduct the withdrawn amounts.
        let new_reserve_a = reserve_a - withdraw_a;
        let new_reserve_b = reserve_b - withdraw_b;

        // Internal swap: swap the "other" token for more of token_out.
        let fee_bps: i128 = Self::read_fee_bps(&env)?;

        // amount_swap after fee
        let amount_swap_with_fee = amount_swap * (10_000 - fee_bps);

        // We swap token_swap (from withdrawal) for token_keep using the updated reserves.
        // After withdrawal, reserves are (new_reserve_a, new_reserve_b).
        // We're swapping amount_swap of token_swap.
        let swap_output = if token_out == token_a {
            // We're swapping token_b for more token_a.
            // Reserves after withdrawal: (new_reserve_a, new_reserve_b) where new_reserve_b = reserve_b - withdraw_b
            // But we're adding amount_swap to the input token.
            amount_swap_with_fee * new_reserve_a / (new_reserve_b * 10_000 + amount_swap_with_fee)
        } else {
            // We're swapping token_a for more token_b.
            amount_swap_with_fee * new_reserve_b / (new_reserve_a * 10_000 + amount_swap_with_fee)
        };

        // Update reserves after internal swap.
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            amount_swap * protocol_fee_bps / 10_000
        } else {
            0
        };
        let lp_rebate_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let lp_rebate = if protocol_fee > 0 && lp_rebate_bps > 0 {
            protocol_fee * lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;

        let final_reserve_a = if token_out == token_a {
            // We paid out swap_output of token_a
            new_reserve_a - swap_output
        } else {
            // We received amount_swap of token_a (minus net protocol fee)
            new_reserve_a + amount_swap - net_protocol_fee
        };

        let final_reserve_b = if token_out == token_a {
            // We received amount_swap of token_b (minus net protocol fee)
            new_reserve_b + amount_swap - net_protocol_fee
        } else {
            // We paid out swap_output of token_b
            new_reserve_b - swap_output
        };

        Self::check_oracle_deviation(&env, &token_swap, &token_out, amount_swap, swap_output)?;

        // Total output is the amount we kept from withdrawal plus the swap output.
        let total_out = amount_keep + swap_output;

        if total_out < min_out {
            return Err(AmmError::SlippageExceeded);
        }

        if Self::check_circuit_breaker(&env, final_reserve_a, final_reserve_b)? {
            return Ok(0);
        }

        // Burn LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.burn(&provider, &shares);

        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &final_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &final_reserve_b);

        // Track protocol fees if applicable.
        if net_protocol_fee > 0 {
            let token_to_accrue = if token_out == token_a {
                token_b.clone()
            } else {
                token_a.clone()
            };

            if token_to_accrue == token_a {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + net_protocol_fee));
            } else {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + net_protocol_fee));
            }
        }

        // Transfer the total output to provider.
        let client_out = SepTokenClient::new(&env, &token_out);
        client_out.transfer(&env.current_contract_address(), &provider, &total_out);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("rm_liq_1s"),),
            (provider.clone(), shares, token_out.clone(), total_out)
        );

        Ok(total_out)
    }

    // ── Swap ──────────────────────────────────────────────────────────────────

    /// Swap an exact amount of one pool token for the other.
    ///
    /// Transfers `amount_in` of `token_in` from `trader` into the pool and
    /// sends back the calculated output amount of the opposite token, computed
    /// via the constant-product formula `x * y = k` with the pool fee deducted
    /// from `amount_in` before the calculation.
    ///
    /// Requires `trader` to have authorized this call.
    ///
    /// # Parameters
    /// - `trader` – Address of the account initiating the swap.
    /// - `token_in` – Address of the token being sold; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `amount_in` – Exact amount of `token_in` to sell. Must be positive.
    /// - `min_out` – Minimum amount of the output token the caller is willing to
    ///   accept (slippage guard).
    ///
    /// Uses the constant-product formula with fee deducted from `amount_in`.
    /// The `protocol_fee_bps` portion of `amount_in` is held for `withdraw_protocol_fees`.
    /// # Returns
    /// The amount of the output token transferred to `trader`.
    ///
    /// # Panics
    /// - If `amount_in` is not positive.
    /// - If `token_in` is not one of the two pool tokens.
    /// - If either pool reserve is zero (pool is empty).
    /// - If the computed output would be less than `min_out`.
    /// - If the computed output equals or exceeds the output reserve (insufficient liquidity).
    pub fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        trader.require_auth();
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let (reserve_in, reserve_out, token_out) = if token_in == token_a {
            (reserve_a, reserve_b, token_b.clone())
        } else if token_in == token_b {
            (reserve_b, reserve_a, token_a.clone())
        } else {
            return Err(AmmError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }

        let fee_bps: i128 = Self::read_fee_bps(&env)?;

        // amount_in after fee
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        // constant-product: out = (amount_in_with_fee * reserve_out) / (reserve_in * 10_000 + amount_in_with_fee)
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);

        // Separate protocol fee from LP reserves.
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            amount_in * protocol_fee_bps / 10_000
        } else {
            0
        };
        // Issue #292: LP rebate — fraction of protocol fee returned to LP reserves.
        let lp_rebate_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let lp_rebate = if protocol_fee > 0 && lp_rebate_bps > 0 {
            protocol_fee * lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;

        let (post_reserve_a, post_reserve_b) = if token_in == token_a {
            (
                reserve_in + amount_in - net_protocol_fee,
                reserve_out - amount_out,
            )
        } else {
            (
                reserve_out - amount_out,
                reserve_in + amount_in - net_protocol_fee,
            )
        };

        if amount_out < min_out {
            return Err(AmmError::SlippageExceeded);
        }
        if amount_out >= reserve_out {
            return Err(AmmError::InsufficientLiquidity);
        }

        Self::check_oracle_deviation(&env, &token_in, &token_out, amount_in, amount_out)?;

        if Self::check_circuit_breaker(&env, post_reserve_a, post_reserve_b)? {
            return Ok(0);
        }

        // Transfer in.
        let client_in = SepTokenClient::new(&env, &token_in);
        client_in.transfer(&trader, &env.current_contract_address(), &amount_in);

        // Transfer out.
        let client_out = SepTokenClient::new(&env, &token_out);
        client_out.transfer(&env.current_contract_address(), &trader, &amount_out);

        // Update reserves (net protocol fee held outside LP reserves; rebate stays in reserves).
        if token_in == token_a {
            env.storage().instance().set(
                &DataKey::ReserveA,
                &(reserve_in + amount_in - net_protocol_fee),
            );
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_out - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + net_protocol_fee));
            }
        } else {
            env.storage().instance().set(
                &DataKey::ReserveB,
                &(reserve_in + amount_in - net_protocol_fee),
            );
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_out - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + net_protocol_fee));
            }
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "swap"), trader),
            (token_in, amount_in, token_out, amount_out)
        );

        Ok(amount_out)
    }

    /// Swap a variable input amount to receive exactly `amount_out` of `token_out`.
    ///
    /// Computes the required input via `get_amount_in` and enforces `max_in` as a
    /// slippage guard. Updates reserves and the TWAP accumulator identically to `swap`.
    ///
    /// # Parameters
    /// - `trader`     – Address initiating the swap; must authorise this call.
    /// - `token_out`  – Address of the token to receive; must be `token_a` or `token_b`.
    /// - `amount_out` – Exact amount of `token_out` the caller wants to receive.
    /// - `max_in`     – Maximum `token_in` the caller is willing to spend (slippage guard).
    /// - `deadline`   – Latest ledger timestamp at which this call is valid.
    ///
    /// # Returns
    /// The amount of the input token actually spent.
    ///
    /// # Panics
    /// - If `amount_out` is not positive.
    /// - If `token_out` is not one of the two pool tokens.
    /// - If the required input exceeds `max_in`.
    /// - If the pool is paused.
    #[allow(clippy::too_many_arguments)]
    pub fn swap_exact_out(
        env: Env,
        trader: Address,
        token_out: Address,
        amount_out: i128,
        max_in: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        trader.require_auth();
        if amount_out <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;

        let token_in = if token_out == token_a {
            token_b.clone()
        } else if token_out == token_b {
            token_a.clone()
        } else {
            return Err(AmmError::InvalidToken);
        };

        let amount_in = Self::get_amount_in(env.clone(), token_out.clone(), amount_out)?;
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Separate protocol fee from LP reserves.
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            amount_in * protocol_fee_bps / 10_000
        } else {
            0
        };
        // Issue #292: LP rebate — fraction of protocol fee returned to LP reserves.
        let lp_rebate_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let lp_rebate = if protocol_fee > 0 && lp_rebate_bps > 0 {
            protocol_fee * lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;

        let (post_reserve_a, post_reserve_b) = if token_in == token_a {
            (
                reserve_a + amount_in - net_protocol_fee,
                reserve_b - amount_out,
            )
        } else {
            (
                reserve_a - amount_out,
                reserve_b + amount_in - net_protocol_fee,
            )
        };

        if amount_in > max_in {
            return Err(AmmError::SlippageExceeded);
        }

        Self::check_oracle_deviation(&env, &token_in, &token_out, amount_in, amount_out)?;

        if Self::check_circuit_breaker(&env, post_reserve_a, post_reserve_b)? {
            return Ok(0);
        }

        // Transfer tokens.
        SepTokenClient::new(&env, &token_in).transfer(
            &trader,
            &env.current_contract_address(),
            &amount_in,
        );
        SepTokenClient::new(&env, &token_out).transfer(
            &env.current_contract_address(),
            &trader,
            &amount_out,
        );

        // Update reserves (net protocol fee held outside LP reserves; rebate stays in reserves).
        if token_in == token_a {
            env.storage().instance().set(
                &DataKey::ReserveA,
                &(reserve_a + amount_in - net_protocol_fee),
            );
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_b - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + net_protocol_fee));
            }
        } else {
            env.storage().instance().set(
                &DataKey::ReserveB,
                &(reserve_b + amount_in - net_protocol_fee),
            );
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_a - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + net_protocol_fee));
            }
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "swap"), trader),
            (token_in, amount_in, token_out, amount_out)
        );

        Ok(amount_in)
    }

    // ── Protocol Fees ─────────────────────────────────────────────────────────

    /// Withdraw all accrued protocol fees to the configured fee recipient.
    ///
    /// Only callable by the fee recipient. Resets accrued balances to zero.
    /// Returns `(fee_a_withdrawn, fee_b_withdrawn)`.
    pub fn withdraw_protocol_fees(env: Env) -> Result<(i128, i128), AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        // Acquire the reentrancy lock before any external transfer below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        let fee_recipient: Address = env
            .storage()
            .instance()
            .get(&DataKey::FeeRecipient)
            .ok_or(AmmError::NotInitialized)?;
        fee_recipient.require_auth();

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;

        let fee_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeA)
            .unwrap_or(0);
        let fee_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeB)
            .unwrap_or(0);

        if fee_a > 0 {
            SepTokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &fee_recipient,
                &fee_a,
            );
            env.storage().instance().set(&DataKey::AccruedFeeA, &0_i128);
        }

        if fee_b > 0 {
            SepTokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &fee_recipient,
                &fee_b,
            );
            env.storage().instance().set(&DataKey::AccruedFeeB, &0_i128);
        }

        Ok((fee_a, fee_b))
    }

    /// Borrow pool liquidity and repay it plus a fee during the receiver callback.
    ///
    /// # Reentrancy safety
    /// This function acquires the pool-wide reentrancy lock before calling
    /// the external `on_flash_loan` callback and holds it for the duration
    /// of that call — the same lock every other fund-moving entry point
    /// (`swap`, `add_liquidity`, `remove_liquidity`, `flash_loan`, and the
    /// rest) now acquires. Any attempt by the receiver to call back into any
    /// of them on this same pool will fail with `AmmError::Reentrant`. The
    /// lock is released by `ReentrancyGuard`'s `Drop` on every return path —
    /// success, repayment failure, or any other error — not only on success.
    pub fn flash_loan(
        env: Env,
        receiver: Address,
        amount_a: i128,
        amount_b: i128,
        data: Bytes,
    ) -> Result<(i128, i128), AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        if amount_a < 0 || amount_b < 0 || (amount_a == 0 && amount_b == 0) {
            return Err(AmmError::ZeroAmount);
        }

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);
        // Acquire the reentrancy lock before any external call. This
        // prevents the receiver's on_flash_loan callback from calling back
        // into any fund-moving entry point on this pool, and is released
        // automatically (including on every error path below) when `_guard`
        // drops.
        let _guard = ReentrancyGuard::acquire(&env)?;

        // Circuit breaker check before borrowing funds.
        if Self::check_circuit_breaker(&env, reserve_a, reserve_b)? {
            return Ok((0, 0));
        }

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        if reserve_a < amount_a || reserve_b < amount_b {
            return Err(AmmError::InsufficientLiquidity);
        }

        let fee_bps = Self::get_flash_loan_fee_bps(env.clone());
        let fee_a = if fee_bps > 0 && amount_a > 0 {
            (amount_a * fee_bps / 10_000).max(1)
        } else {
            0
        };
        let fee_b = if fee_bps > 0 && amount_b > 0 {
            (amount_b * fee_bps / 10_000).max(1)
        } else {
            0
        };
        let pool = env.current_contract_address();
        let token_a_client = SepTokenClient::new(&env, &token_a);
        let token_b_client = SepTokenClient::new(&env, &token_b);
        let balance_a_before = token_a_client.balance(&pool);
        let balance_b_before = token_b_client.balance(&pool);

        if amount_a > 0 {
            token_a_client.transfer(&pool, &receiver, &amount_a);
        }
        if amount_b > 0 {
            token_b_client.transfer(&pool, &receiver, &amount_b);
        }

        // ── External callback (lock is held) ─────────────────────────────────
        // The receiver cannot reenter this pool because Locked == true.
        // Any reentrant call will return AmmError::Reentrant.
        let accepted = FlashLoanReceiverClient::new(&env, &receiver)
            .on_flash_loan(&amount_a, &amount_b, &fee_a, &fee_b, &data);
        if !accepted {
            return Err(AmmError::FlashLoanRepaymentFailed);
        }

        let balance_a_after = token_a_client.balance(&pool);
        let balance_b_after = token_b_client.balance(&pool);
        if balance_a_after < balance_a_before + fee_a || balance_b_after < balance_b_before + fee_b
        {
            return Err(AmmError::FlashLoanRepaymentFailed);
        }

        let accrued_fee_a = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeA)
            .unwrap_or(0);
        let accrued_fee_b = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeB)
            .unwrap_or(0);
        let reserve_a_after = balance_a_after - accrued_fee_a;
        let reserve_b_after = balance_b_after - accrued_fee_b;
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &reserve_a_after);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &reserve_b_after);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "flash_loan"), receiver),
            (amount_a, amount_b, fee_a, fee_b)
        );

        // `_guard` releases the lock when it drops here on the success path
        // (and would release it identically had any `?` above returned early).
        Ok((fee_a, fee_b))
    }

    // ── Quotes (read-only) ────────────────────────────────────────────────────

    /// Return the current spot price of each token in terms of the other,
    /// scaled by 1_000_000.
    ///
    /// Returns `(price_a, price_b)` where:
    /// - `price_a` = price of token_a in terms of token_b (reserve_b * 1_000_000 / reserve_a)
    /// - `price_b` = price of token_b in terms of token_a (reserve_a * 1_000_000 / reserve_b)
    ///
    /// Panics if either reserve is zero (pool is empty).
    pub fn price_ratio(env: Env) -> Result<(i128, i128), AmmError> {
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env);
        if reserve_a <= 0 || reserve_b <= 0 {
            return Err(AmmError::EmptyPool);
        }
        let price_a = reserve_b * 1_000_000 / reserve_a;
        let price_b = reserve_a * 1_000_000 / reserve_b;
        Ok((price_a, price_b))
    }

    /// Quote how much `token_out` you receive for `amount_in` of `token_in`.
    /// Calculate the output amount for a hypothetical swap without executing it.
    ///
    /// Applies the same constant-product formula and fee as `swap` but
    /// makes no state changes. Useful for quoting prices off-chain or in other
    /// contracts before committing to a swap.
    ///
    /// # Parameters
    /// - `token_in` – Address of the token being sold; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `amount_in` – Hypothetical amount of `token_in` to sell.
    ///
    /// # Returns
    /// The amount of the output token that would be received for `amount_in`,
    /// after the pool fee is applied.
    ///
    /// # Panics
    /// - If `token_in` is not one of the two pool tokens.
    pub fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> Result<i128, AmmError> {
        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let fee_bps: i128 = Self::read_fee_bps(&env)?;

        let (reserve_in, reserve_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }
        if fee_bps >= 10_000 {
            return Err(AmmError::InvalidFeeBps);
        }
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        Ok(amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee))
    }

    /// Simulate a swap and return a detailed breakdown without executing it.
    ///
    /// Returns the expected output, total fee taken, effective execution price,
    /// spot price, and price impact — all computed from current reserve state.
    /// `amount_out` is guaranteed to match `get_amount_out` for the same inputs.
    pub fn simulate_swap(
        env: Env,
        token_in: Address,
        amount_in: i128,
    ) -> Result<SwapSimulation, AmmError> {
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let fee_bps: i128 = Self::read_fee_bps(&env)?;
        let (reserve_in, reserve_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }
        if fee_bps >= 10_000 {
            return Err(AmmError::InvalidFeeBps);
        }
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);
        let fee_amount = amount_in * fee_bps / 10_000;
        let spot_price = reserve_out * 1_000_000 / reserve_in;
        let effective_price = amount_out * 1_000_000 / amount_in;
        let price_impact_bps = if amount_out == 0 {
            0
        } else {
            ((spot_price - effective_price) * 10_000 / spot_price).max(0)
        };
        Ok(SwapSimulation {
            amount_out,
            fee_amount,
            price_impact_bps,
            effective_price,
            spot_price,
        })
    }

    /// Quote how much `token_in` is required to receive exactly `amount_out` of `token_out`.
    ///
    /// # Returns
    /// The amount of input token needed, or an error if:
    /// - `token_out` does not match either pool token (InvalidToken)
    /// - Either reserve is zero (EmptyPool)
    /// - `amount_out` is zero or negative (ZeroAmount)
    /// - `amount_out` is >= the output reserve (InsufficientLiquidity)
    /// - `fee_bps` is >= 10_000 (InvalidFeeBps)
    pub fn get_amount_in(env: Env, token_out: Address, amount_out: i128) -> Result<i128, AmmError> {
        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let fee_bps: i128 = Self::read_fee_bps(&env)?;
        let (reserve_in, reserve_out) = if token_out == token_a {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else if token_out == token_b {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }
        if amount_out <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        if amount_out >= reserve_out {
            return Err(AmmError::InsufficientLiquidity);
        }
        // Defensive guard for any pool created before the tightened fee bound:
        // a 100% fee makes the `(10_000 - fee_bps)` divisor zero. A swap can
        // never return any output in that case, so the input is unquotable.
        if fee_bps >= 10_000 {
            return Err(AmmError::InvalidFeeBps);
        }
        let numerator = reserve_in
            .checked_mul(amount_out)
            .ok_or(AmmError::InsufficientLiquidity)?
            .checked_mul(10_000)
            .ok_or(AmmError::InsufficientLiquidity)?;
        let denominator = (reserve_out - amount_out)
            .checked_mul(10_000 - fee_bps)
            .ok_or(AmmError::InsufficientLiquidity)?;
        let amount_in = (numerator / denominator) + 1;
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        Ok(amount_in)
    }

    /// Return the current swap fee in basis points.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Returns
    /// The pool's `fee_bps` as an `i128`. For the full pool state, see [`AmmPool::get_info`].
    pub fn get_fee_info(env: Env) -> Result<i128, AmmError> {
        Self::read_fee_bps(&env)
    }

    /// Return full pool state.
    /// Return a snapshot of the full pool state.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Returns
    /// A [`PoolInfo`] struct containing:
    /// - `token_a` / `token_b` — addresses of the two pool tokens.
    /// - `reserve_a` / `reserve_b` — current token reserves held by the pool.
    /// - `total_shares` — total outstanding LP shares.
    /// - `fee_bps` — the swap fee in basis points.
    /// - `flash_loan_fee_bps` — the flash-loan fee in basis points.
    /// - `admin` — the pool administrator.
    /// - `fee_recipient` — recipient of accrued protocol fees.
    /// - `protocol_fee_bps` — protocol fee in basis points (subset of `fee_bps`).
    pub fn get_info(env: Env) -> Result<PoolInfo, AmmError> {
        Ok(PoolInfo {
            token_a: Self::read_token_a(&env)?,
            token_b: Self::read_token_b(&env)?,
            reserve_a: Self::get_reserve_a(env.clone()),
            reserve_b: Self::get_reserve_b(env.clone()),
            total_shares: Self::get_total_shares(env.clone()),
            fee_bps: Self::read_fee_bps(&env)?,
            flash_loan_fee_bps: Self::get_flash_loan_fee_bps(env.clone()),
            admin: Self::read_admin(&env)?,
            fee_recipient: env
                .storage()
                .instance()
                .get(&DataKey::FeeRecipient)
                .ok_or(AmmError::NotInitialized)?,
            protocol_fee_bps: env
                .storage()
                .instance()
                .get(&DataKey::ProtocolFeeBps)
                .unwrap_or(0),
            lp_rebate_bps: env
                .storage()
                .instance()
                .get(&DataKey::LpRebateBps)
                .unwrap_or(0),
        })
    }

    /// Return the protocol fees accrued but not yet withdrawn, without moving funds.
    ///
    /// Read-only counterpart to [`AmmPool::withdraw_protocol_fees`]; useful for fee recipients
    /// and dashboards that need a non-destructive view of pending fees.
    ///
    /// # Returns
    /// `(accrued_fee_a, accrued_fee_b)` — pending protocol fees in each token.
    pub fn get_accrued_fees(env: Env) -> (i128, i128) {
        let fee_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeA)
            .unwrap_or(0);
        let fee_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeB)
            .unwrap_or(0);
        (fee_a, fee_b)
    }

    /// Return the number of LP shares currently held by a given provider.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider to query.
    ///
    /// # Returns
    /// The LP share balance of `provider`, or `0` if the address has never
    /// provided liquidity to this pool.
    pub fn shares_of(env: Env, provider: Address) -> Result<i128, AmmError> {
        let lp_token: Address = Self::read_lp_token(&env)?;
        Ok(LpTokenClient::new(&env, &lp_token).balance(&provider))
    }

    // ── Fee-on-transfer support ───────────────────────────────────────────────

    /// Swap with fee-on-transfer (FOT) token support.
    ///
    /// Identical to `swap` but measures the actual amount of `token_in`
    /// received by the pool via a pre/post balance snapshot instead of trusting
    /// `amount_in`. This handles tokens that silently deduct a transfer fee, so
    /// the constant-product calculation always uses the true net amount.
    ///
    /// # Parameters
    /// - `min_received` – Minimum tokens the pool must actually receive after any
    ///   FOT deduction (slippage guard on input). Set to `0` to disable.
    ///
    /// # Returns
    /// `(amount_out, actual_received)` — the output amount transferred to `trader`
    /// and the net input the pool received.
    ///
    /// # Events
    /// Emits a `fot_detected` event when `actual_received < amount_in`, carrying
    /// `(nominal_amount_in, actual_received)` for off-chain monitoring.
    #[allow(clippy::too_many_arguments)]
    pub fn swap_fot(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
        min_received: i128,
        deadline: u64,
        referrer: Option<Address>,
    ) -> Result<(i128, i128), AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        trader.require_auth();
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        Self::checkpoint_oracles(&env);

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;

        let (reserve_in, reserve_out, token_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
                token_b.clone(),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
                token_a.clone(),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }

        let fee_bps_pre: i128 = Self::read_fee_bps(&env)?;
        let est_in_with_fee = amount_in * (10_000 - fee_bps_pre);
        let est_out = est_in_with_fee * reserve_out / (reserve_in * 10_000 + est_in_with_fee);
        let protocol_fee_bps_pre: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let est_protocol_fee = if protocol_fee_bps_pre > 0 {
            amount_in * protocol_fee_bps_pre / 10_000
        } else {
            0
        };
        let lp_rebate_bps_pre: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let est_lp_rebate = if est_protocol_fee > 0 && lp_rebate_bps_pre > 0 {
            est_protocol_fee * lp_rebate_bps_pre / 10_000
        } else {
            0
        };
        let est_net_protocol_fee = est_protocol_fee - est_lp_rebate;
        let (est_post_a, est_post_b) = if token_in == token_a {
            (
                reserve_in + amount_in - est_net_protocol_fee,
                reserve_out - est_out,
            )
        } else {
            (
                reserve_out - est_out,
                reserve_in + amount_in - est_net_protocol_fee,
            )
        };
        if Self::check_circuit_breaker(&env, est_post_a, est_post_b)? {
            return Ok((0, 0));
        }

        let pool = env.current_contract_address();
        let client_in = SepTokenClient::new(&env, &token_in);
        let balance_before = client_in.balance(&pool);

        client_in.transfer(&trader, &pool, &amount_in);

        let actual_received = client_in.balance(&pool) - balance_before;
        if actual_received <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        if actual_received < min_received {
            return Err(AmmError::FotSlippage);
        }

        let fee_bps: i128 = Self::read_fee_bps(&env)?;
        let amount_in_with_fee = actual_received * (10_000 - fee_bps);
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);

        if amount_out < min_out {
            return Err(AmmError::SlippageExceeded);
        }
        if amount_out >= reserve_out {
            return Err(AmmError::InsufficientLiquidity);
        }

        Self::check_oracle_deviation(&env, &token_in, &token_out, actual_received, amount_out)?;

        SepTokenClient::new(&env, &token_out).transfer(&pool, &trader, &amount_out);

        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            actual_received * protocol_fee_bps / 10_000
        } else {
            0
        };
        // Issue #502: LP rebate — fraction of protocol fee returned to LP reserves.
        let lp_rebate_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let lp_rebate = if protocol_fee > 0 && lp_rebate_bps > 0 {
            protocol_fee * lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;

        if token_in == token_a {
            env.storage().instance().set(
                &DataKey::ReserveA,
                &(reserve_in + actual_received - net_protocol_fee),
            );
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_out - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + net_protocol_fee));
            }
        } else {
            env.storage().instance().set(
                &DataKey::ReserveB,
                &(reserve_in + actual_received - net_protocol_fee),
            );
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_out - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + net_protocol_fee));
            }
        }

        if actual_received < amount_in {
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(&env, "fot_detected"), token_in.clone()),
                (amount_in, actual_received)
            );
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "swap"), trader),
            (token_in, actual_received, token_out, amount_out, referrer)
        );

        Ok((amount_out, actual_received))
    }

    /// Add liquidity with fee-on-transfer (FOT) token support.
    ///
    /// Like `add_liquidity` but measures the actual token amounts received
    /// by the pool via pre/post balance snapshots rather than trusting the
    /// nominal `amount_a`/`amount_b` values. LP shares are minted proportional
    /// to what the pool actually received.
    ///
    /// # Parameters
    /// - `min_received_a` – Minimum actual `token_a` the pool must receive.
    /// - `min_received_b` – Minimum actual `token_b` the pool must receive.
    #[allow(clippy::too_many_arguments)]
    pub fn add_liquidity_fot(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_received_a: i128,
        min_received_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        Self::extend_ttl(&env);
        if Self::is_drained(env.clone()) {
            return Err(AmmError::PoolDrained);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Acquire the reentrancy lock for the duration of this call; a
        // reentrant call arriving via a token/receiver callback is rejected
        // here with `Reentrant`, and the lock is released automatically
        // when `_guard` drops on any return path below.
        let _guard = ReentrancyGuard::acquire(&env)?;
        provider.require_auth();
        if amount_a <= 0 || amount_b <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        Self::checkpoint_oracles(&env);

        let token_a: Address = Self::read_token_a(&env)?;
        let token_b: Address = Self::read_token_b(&env)?;
        let lp_token: Address = Self::read_lp_token(&env)?;

        let pool = env.current_contract_address();
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);

        let bal_a_before = client_a.balance(&pool);
        let bal_b_before = client_b.balance(&pool);

        client_a.transfer(&provider, &pool, &amount_a);
        client_b.transfer(&provider, &pool, &amount_b);

        let actual_a = client_a.balance(&pool) - bal_a_before;
        let actual_b = client_b.balance(&pool) - bal_b_before;

        if actual_a <= 0 || actual_b <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        if actual_a < min_received_a || actual_b < min_received_b {
            return Err(AmmError::FotSlippage);
        }

        let reserve_a: i128 = Self::get_reserve_a(env.clone());
        let reserve_b: i128 = Self::get_reserve_b(env.clone());
        let total_shares: i128 = Self::get_total_shares(env.clone());

        let (shares, shares_locked) = Self::compute_liquidity_mint(
            &env,
            actual_a,
            actual_b,
            reserve_a,
            reserve_b,
            total_shares,
            min_shares,
        )?;

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a + actual_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b + actual_b));
        env.storage().instance().set(
            &DataKey::TotalShares,
            &(total_shares + shares + shares_locked),
        );

        let lp_client = LpTokenClient::new(&env, &lp_token);
        if shares_locked > 0 {
            lp_client.mint(&env.current_contract_address(), &shares_locked);
            env.storage()
                .instance()
                .set(&DataKey::MinLiquidityLocked, &true);
        }
        lp_client.mint(&provider, &shares);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "add_liquidity"), provider),
            (actual_a, actual_b, shares)
        );

        Ok(shares)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    /// Shared first-deposit / proportional share computation used by both
    /// dd_liquidity and dd_liquidity_fot.
    ///
    /// Returns (shares_to_provider, shares_locked). On a pool's first deposit
    /// MINIMUM_LIQUIDITY shares are carved out and locked (Issue #294); the
    /// provider's share (after the lock) is compared against min_shares.
    /// Returns EmptyPool rather than dividing by zero when shares exist but a
    /// reserve is zero.
    #[allow(clippy::too_many_arguments)]
    fn compute_liquidity_mint(
        env: &Env,
        amount_a: i128,
        amount_b: i128,
        reserve_a: i128,
        reserve_b: i128,
        total_shares: i128,
        min_shares: i128,
    ) -> Result<(i128, i128), AmmError> {
        const MINIMUM_LIQUIDITY: i128 = 1_000;
        let shares = if total_shares == 0 {
            Self::sqrt(amount_a * amount_b)
        } else {
            if reserve_a <= 0 || reserve_b <= 0 {
                return Err(AmmError::EmptyPool);
            }
            let shares_a = amount_a * total_shares / reserve_a;
            let shares_b = amount_b * total_shares / reserve_b;
            shares_a.min(shares_b)
        };

        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        let already_locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::MinLiquidityLocked)
            .unwrap_or(false);
        let (shares_to_provider, shares_locked) = if total_shares == 0 && !already_locked {
            if shares <= MINIMUM_LIQUIDITY {
                return Err(AmmError::InsufficientShares);
            }
            (shares - MINIMUM_LIQUIDITY, MINIMUM_LIQUIDITY)
        } else {
            (shares, 0)
        };

        if shares_to_provider < min_shares {
            return Err(AmmError::SlippageExceeded);
        }
        Ok((shares_to_provider, shares_locked))
    }

    /// Returns the cumulative liquidity accumulator and the timestamp of the last update.
    pub fn get_liquidity_cumulative(env: Env) -> (i128, u64) {
        let liquidity_cum = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityCumulative)
            .unwrap_or(0);
        let last_timestamp = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(0);
        (liquidity_cum, last_timestamp)
    }

    /// Returns the cumulative price accumulators and the timestamp of the last update.
    pub fn get_price_cumulative(env: Env) -> (i128, i128, u64) {
        let price_cum_a = env
            .storage()
            .instance()
            .get(&DataKey::PriceCumulativeA)
            .unwrap_or(0);
        let price_cum_b = env
            .storage()
            .instance()
            .get(&DataKey::PriceCumulativeB)
            .unwrap_or(0);
        let last_timestamp = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(0);
        (price_cum_a, price_cum_b, last_timestamp)
    }

    fn get_reserve_a(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReserveA)
            .unwrap_or(0)
    }

    fn get_reserve_b(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReserveB)
            .unwrap_or(0)
    }

    fn get_total_shares(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0)
    }

    fn get_flash_loan_fee_bps(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FlashLoanFeeBps)
            .unwrap_or_else(|| env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0))
    }

    /// Integer square root via Newton's method.
    fn sqrt(n: i128) -> i128 {
        if n < 0 {
            panic!("sqrt of negative: {n}");
        }
        if n == 0 {
            return 0;
        }
        let mut x = n;
        let mut y = (x + 1) / 2;
        while y < x {
            x = y;
            y = (x + n / x) / 2;
        }
        x
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests;

// ── Property-based tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod prop_tests;
